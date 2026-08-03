//! Terminal writer, cursor/erase sequences, and `FooterGuard` (RAII screen restore)
//! (docs/ui.md, docs/architecture.md).
//!
//! cprog is the terminal's sole writer, so it keeps the footer rows and the scrolling
//! log region from corrupting each other with an erase-redraw discipline: before any log
//! bytes pass through, and before exit, the footer is erased; afterwards it is redrawn
//! (docs/ui.md). [`FooterGuard`] owns the writer and, via `Drop`, guarantees the footer is
//! cleared on *every* exit path — normal, error, or panic (docs/testing.md C7). Every render
//! is best-effort: IO failures surface as `io::Result` and never affect `cp`'s exit code
//! (docs/testing.md C6).

use std::io::{self, Write};

use crate::term::TerminalSize;
use crate::ui::{FOOTER_ROWS, MIN_LOG_ROWS};

/// Erase from the cursor to the end of the line (`CSI K`), clearing any leftover from a
/// previously longer footer.
const ERASE_EOL: &[u8] = b"\x1b[K";
/// Hide / show the terminal cursor (DECTCEM). The cursor would otherwise sit and blink at the
/// end of the footer bar; we hide it while the footer is live and restore it on teardown.
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
/// Save / restore the cursor (DECSC / DECRC). The footer is repainted at absolute rows while the
/// log keeps its own position, so every repaint is bracketed by these.
///
/// Deliberately **not** `CSI s` / `CSI u`: that spelling comes from ANSI.SYS and is absent from
/// terminfo. apt moved off it in 2014 after a census of the terminfo database found 446 entries
/// spelling save-cursor `\E7` against a handful of everything else (Debian #772521).
const SAVE_CURSOR: &[u8] = b"\x1b7";
const RESTORE_CURSOR: &[u8] = b"\x1b8";
/// Release the scrolling region, handing the whole screen back (DECSTBM with no parameters).
const RELEASE_REGION: &[u8] = b"\x1b[r";
/// Erase from the cursor to the end of the screen (`CSI J`), used to take the footer down.
const ERASE_TO_END: &[u8] = b"\x1b[J";
/// Erase the whole visible screen (`CSI 2 J`), used when the terminal is resized. Deliberately
/// **not** `CSI 3 J`, which would take the scrollback with it.
const ERASE_SCREEN: &[u8] = b"\x1b[2J";

/// Owns the terminal writer and manages the footer rows, erasing them on drop.
pub struct FooterGuard<W: Write> {
    w: W,
    /// The terminal size the scrolling region is currently set for, or `None` when the region has
    /// been released. Both a marker that teardown owes a `RELEASE_REGION` and the value that says
    /// whether a resize needs the region re-armed.
    ///
    /// **The width is part of it even though the region only names rows.** A narrower terminal
    /// folds the footer that is already on screen, and the fragments land inside the scrolling
    /// region where they scroll up with the log — the residue this whole change exists to remove.
    /// Re-arming on *any* size change is what takes them away; apt re-arms on every SIGWINCH for
    /// the same reason (`install-progress.cc::CheckSIGWINCH`), unconditionally. How much the
    /// re-arm then clears depends on the direction — see [`Self::arm`].
    region_for: Option<TerminalSize>,
    /// The size the region was last armed for, kept even after [`Self::release`] clears
    /// `region_for`. It answers one question: did the terminal get *wider* since we last drew?
    /// That is the only case that blanks the screen, and the only case that may — a first arm
    /// happens when the bar appears mid-command, and a shrink costs the user a screen of `cp -v`
    /// log to fix residue an erase from the cursor already reaches ([`Self::arm`]).
    ///
    /// Surviving a release is what makes a resize *during* a Ctrl-Z stop count as one: the region
    /// is gone by then, so `region_for` cannot say.
    last_armed_for: Option<TerminalSize>,
    /// Whether we have hidden the cursor (and thus must restore it on teardown).
    cursor_hidden: bool,
}

impl<W: Write> FooterGuard<W> {
    /// Wrap a writer with no footer shown yet.
    pub fn new(w: W) -> Self {
        Self { w, region_for: None, last_armed_for: None, cursor_hidden: false }
    }

    /// Draw the footer on the bottom [`FOOTER_ROWS`] rows of a `term_rows`-high terminal.
    ///
    /// The rows are addressed absolutely and the log's cursor is saved and restored around them,
    /// so nothing here depends on where the previous write ended. That is the whole point: a
    /// cursor-relative footer is invalidated by any resize, because the terminal reflows what is
    /// already on screen and the walk-back arithmetic no longer describes it (#76).
    ///
    /// The scrolling region is armed on the first draw and re-armed when `term_rows` changes.
    /// It is **not** released between draws — the bottom two rows stay reserved for the run, the
    /// way apt reserves them for a dpkg run, so the layout does not jump every time a file
    /// finishes.
    pub fn draw(&mut self, rows: &[&str], size: TerminalSize) -> io::Result<()> {
        let term_rows = size.rows;
        if rows.is_empty() || term_rows < MIN_LOG_ROWS + FOOTER_ROWS {
            return Ok(());
        }
        self.arm(size)?;
        if !self.cursor_hidden {
            // Mark hidden before writing so a partial write is still restored on drop.
            self.cursor_hidden = true;
            self.w.write_all(HIDE_CURSOR)?;
        }
        self.w.write_all(SAVE_CURSOR)?;
        let first = term_rows as usize - rows.len().min(FOOTER_ROWS as usize);
        for (i, row) in rows.iter().take(FOOTER_ROWS as usize).enumerate() {
            self.goto(first + i + 1)?;
            self.w.write_all(row.as_bytes())?;
            self.w.write_all(ERASE_EOL)?;
        }
        self.w.write_all(RESTORE_CURSOR)?;
        self.w.flush()
    }

    /// Blank the footer rows, leaving the region armed.
    ///
    /// Releasing the region here would hand two rows back and take them again on the next file,
    /// making the log jump; [`Self::release`] is what teardown and job control use.
    pub fn erase(&mut self) -> io::Result<()> {
        let Some(term_rows) = self.region_for.map(|s| s.rows) else {
            return Ok(());
        };
        self.w.write_all(SAVE_CURSOR)?;
        self.goto((term_rows - FOOTER_ROWS) as usize + 1)?;
        self.w.write_all(ERASE_TO_END)?;
        self.w.write_all(RESTORE_CURSOR)?;
        self.w.flush()
    }

    /// Relay log bytes. They land wherever the log left off, inside the scrolling region, and the
    /// terminal scrolls that region for us.
    ///
    /// There is no erase-then-redraw and no partial-line bookkeeping: the footer is outside the
    /// region, so log output can neither overwrite it nor be overwritten by it. Both were
    /// necessary while the two shared one flowing area (docs/ui.md invariant 11, testing.md C8).
    pub fn write_log_chunks<'a>(
        &mut self,
        chunks: impl IntoIterator<Item = &'a [u8]>,
    ) -> io::Result<()> {
        for chunk in chunks {
            self.w.write_all(chunk)?;
        }
        self.w.flush()
    }

    /// Restore the terminal for a job-control suspend (`SIGTSTP` / Ctrl-Z): take the footer down,
    /// release the region and show the cursor, and reset our state so the next [`draw`](Self::draw)
    /// arms everything again. Unlike `Drop`, the guard keeps living across the stop.
    ///
    /// The region has to go: while cprog is stopped the shell owns the terminal, and a region left
    /// set would confine its prompt to the top of the screen.
    pub fn suspend_restore(&mut self) -> io::Result<()> {
        self.release()?;
        if self.cursor_hidden {
            self.cursor_hidden = false;
            self.w.write_all(SHOW_CURSOR)?;
        }
        self.w.flush()
    }

    /// Arm the scrolling region for a `term_rows`-high terminal, if it is not already armed for
    /// exactly that height.
    ///
    /// **The region starts at row 1 and that is load-bearing.** alacritty only grows its
    /// scrollback when the region starts at the top (`grid/mod.rs`: `if region.start == 0`), so a
    /// margin anywhere else throws the user's `cp -v` log away with nothing on screen to say so.
    /// foot and tmux preserve it either way, which is why this is a comment and a test rather
    /// than something a terminal would have shown us (#76).
    fn arm(&mut self, size: TerminalSize) -> io::Result<()> {
        if self.region_for == Some(size) {
            return Ok(());
        }
        let bottom = size.rows - FOOTER_ROWS;
        // Three cases clear three different amounts; only the last one is expensive — see below.
        let widened = self.last_armed_for.is_some_and(|prev| size.cols > prev.cols);
        self.region_for = Some(size);
        self.last_armed_for = Some(size);
        // **Save before setting the region.** DECSTBM homes the cursor — apt says so where it does
        // the same dance (`// set scroll region (this will place the cursor in the top left)`), so
        // saving afterwards would capture the home position and hand the log back a cursor at row
        // 1, where the next relayed byte would start overwriting it.
        self.w.write_all(SAVE_CURSOR)?;
        self.w.write_all(format!("\x1b[1;{bottom}r").as_bytes())?;
        self.w.write_all(RESTORE_CURSOR)?;
        if !widened {
            // **The cheap case, and it covers everything except a widening.**
            //
            // *First arm of the run*: the cursor is back where the log left off, and everything
            // below it is ours by construction — the footer is always written after the log — so
            // this reserves the footer's area and touches nothing the user has on screen. It must
            // stay this narrow: the bar appears mid-command, and a screen cleared here would take
            // the shell prompt and the command line with it.
            //
            // *A shrink*: the fold pushes the old footer's fragments **down**, into exactly the
            // rows this erase covers. `67e20be` fixed that direction and the terminal confirmed it.
            //
            // *A height-only change*: no reflow happens at all, since line wrapping follows the
            // columns. Nothing to chase.
            return self.w.write_all(ERASE_TO_END);
        }
        // **A widening, and only a widening, needs the whole visible screen.** The terminal pulls
        // lines back out of the scrollback to fill the wider rows, and a fragment of an older
        // footer comes back *above* where the log left off, out of `CSI J`'s reach. There is no
        // arithmetic that finds it either — after a reflow, nothing on this side knows how many
        // rows anything moved (#76).
        //
        // The cost is real and is why this is gated as tightly as it is: the visible log goes
        // blank, and under `-v` that is the log the user asked for. A single large file prints one
        // `cp -v` line and then nothing for minutes, so nothing arrives to fill the screen back in
        // — it simply stays empty until the next file. What survives is whatever had already
        // scrolled off, since `CSI 2 J` does not touch the scrollback the way `CSI 3 J` would;
        // whether the rows that were *visible* at that moment get pushed into it is the terminal's
        // choice, not ours. ratatui pays the same price for the same reason
        // (`terminal/resize.rs`: "clear screen on horizontal shrink to avoid line wrapping issues").
        self.w.write_all(ERASE_SCREEN)
    }

    /// Take the footer down and hand the whole screen back.
    ///
    /// Order matters: the region is released *before* the rows are cleared, so `CSI J` erases
    /// them as ordinary screen rather than as a fixed area, and the caller's flush covers both.
    /// apt learned the flushing half from a post-invoke hook printing into the residue of an
    /// unflushed clear (Debian #793672).
    fn release(&mut self) -> io::Result<()> {
        let Some(term_rows) = self.region_for.take().map(|s| s.rows) else {
            return Ok(());
        };
        self.w.write_all(SAVE_CURSOR)?;
        self.w.write_all(RELEASE_REGION)?;
        self.goto((term_rows - FOOTER_ROWS) as usize + 1)?;
        self.w.write_all(ERASE_TO_END)?;
        self.w.write_all(RESTORE_CURSOR)
    }

    /// Move to column 1 of a 1-based screen row (`CUP`).
    fn goto(&mut self, row: usize) -> io::Result<()> {
        self.w.write_all(format!("\x1b[{row};1H").as_bytes())
    }
}

impl<W: Write> Drop for FooterGuard<W> {
    fn drop(&mut self) {
        // Best-effort screen restore on every exit path; failures must not panic. The region goes
        // before the cursor: a terminal left with a scrolling region set is worse than one left
        // with a hidden cursor, because the user's shell then draws into a boxed-in screen.
        let _ = self.release();
        if self.cursor_hidden {
            let _ = self.w.write_all(SHOW_CURSOR);
        }
        let _ = self.w.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::{self, Write};
    use std::panic::{self, AssertUnwindSafe};
    use std::rc::Rc;

    /// A clonable in-memory writer so a test can hold one handle while the guard writes
    /// through another — and inspect the bytes after the guard is dropped.
    #[derive(Clone, Default)]
    struct SharedBuf(Rc<RefCell<Vec<u8>>>);
    impl SharedBuf {
        fn bytes(&self) -> Vec<u8> {
            self.0.borrow().clone()
        }
    }
    impl Write for SharedBuf {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A writer whose every operation fails, to prove IO errors surface as `Result` and never
    /// panic (docs/testing.md C6).
    struct FailWriter;
    impl Write for FailWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("io down"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("io down"))
        }
    }

    /// A writer that accepts `n` writes and then fails every one after that.
    ///
    /// [`FailWriter`] cannot reach the erase-on-drop path at all: it fails on the very first
    /// write, which is `HIDE_CURSOR`, so `draw` returns before anything reaches the screen and the
    /// `erase()` in `Drop` takes its `rows == 0` early return — a no-op that panics over nothing.
    /// Letting the draw succeed first is what puts a footer on screen for the drop to erase (#69).
    struct FailAfter(usize);
    impl Write for FailAfter {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            match self.0 {
                0 => Err(io::Error::other("io down")),
                n => {
                    self.0 = n - 1;
                    Ok(b.len())
                }
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.0 == 0 {
                return Err(io::Error::other("io down"));
            }
            Ok(())
        }
    }

    const HIDE: &[u8] = b"\x1b[?25l";
    const SHOW: &[u8] = b"\x1b[?25h";

    /// A terminal of fixed height that scrolls, so the cursor arithmetic can be checked against
    /// something with a bottom edge (docs/testing.md C9, #35).
    ///
    /// Not ui.md invariant 7 — that one is about a row being wider than the terminal, and neither
    /// `Screen` nor `FooterGuard` has a width. The rule here is the bottom edge: the erase must
    /// rewind exactly one row and a tick redraw must not scroll (#61 C).
    ///
    /// [`SharedBuf`] records bytes; it has no rows, so a footer drawn on the last line — where
    /// its own `\n` scrolls the screen — behaves exactly like one drawn in the middle. That is
    /// the case where a miscounted `CSI A` eats a line of log, and it was untested.
    ///
    /// Only what `FooterGuard` emits is interpreted: `\r`, `\n`, `CSI K`, `CSI A`, and the cursor
    /// show/hide pair (ignored — visibility does not move the cursor). `\n` moves down *without*
    /// resetting the column, the stricter reading: a PTY with `ONLCR` would also return to column
    /// 0, and `draw` writes its own `\r` so it is correct either way.
    #[derive(Default)]
    struct Screen {
        rows: usize,
        grid: Vec<Vec<u8>>,
        /// Lines pushed off the top, oldest first — the scrollback a user could still page to.
        scrolled_off: Vec<String>,
        row: usize,
        col: usize,
        /// Scrolling region as 0-based inclusive rows, or the whole screen when unset (DECSTBM).
        region: Option<(usize, usize)>,
        /// Cursor stashed by `ESC 7`, returned by `ESC 8` (DECSC/DECRC).
        saved: Option<(usize, usize)>,
    }

    impl Screen {
        fn new(rows: usize) -> Self {
            Screen { rows, grid: vec![Vec::new(); rows], ..Default::default() }
        }

        /// The region's 0-based inclusive bounds, defaulting to the whole screen.
        fn bounds(&self) -> (usize, usize) {
            self.region.unwrap_or((0, self.rows.saturating_sub(1)))
        }

        fn line_feed(&mut self) {
            let (top, bottom) = self.bounds();
            if self.row != bottom {
                // Below the region as well as inside it: a plain move, clamped to the screen.
                if self.row + 1 < self.rows {
                    self.row += 1;
                }
                return;
            }
            let gone = self.grid.remove(top);
            self.grid.insert(bottom, Vec::new());
            // **Only a region starting at the top feeds scrollback.** This is not the model
            // taking a side: alacritty grows its history under exactly that condition
            // (`grid/mod.rs`, `if region.start == 0`), so a footer that pins the margin anywhere
            // else loses the user's log with no way to notice. Reproducing the condition here is
            // what lets a test fail when someone moves the margin (docs/ui.md, issue #76).
            if top == 0 {
                self.scrolled_off.push(text(&gone));
            }
        }

        fn put(&mut self, b: u8) {
            let line = &mut self.grid[self.row];
            if self.col < line.len() {
                line[self.col] = b;
            } else {
                line.resize(self.col, b' ');
                line.push(b);
            }
            self.col += 1;
        }

        /// The visible rows, top to bottom.
        fn visible(&self) -> Vec<String> {
            self.grid.iter().map(|l| text(l)).collect()
        }

    }

    /// Split a `CSI params final` sequence: the numeric parameters, the final byte, and how many
    /// bytes it occupied. `None` if `bytes` does not start with a complete one.
    fn csi(bytes: &[u8]) -> Option<(Vec<u16>, u8, usize)> {
        let body = bytes.strip_prefix(b"\x1b[")?;
        let end = body.iter().position(|b| b.is_ascii_alphabetic())?;
        let params = body[..end]
            .split(|b| *b == b';')
            .filter(|p| !p.is_empty())
            .map(|p| std::str::from_utf8(p).ok()?.parse().ok())
            .collect::<Option<Vec<u16>>>()?;
        Some((params, body[end], end + 3))
    }

    fn text(line: &[u8]) -> String {
        String::from_utf8_lossy(line).trim_end().to_string()
    }

    /// A clonable handle so a test can inspect the screen after the guard has been dropped.
    #[derive(Clone)]
    struct SharedScreen(Rc<RefCell<Screen>>);

    impl SharedScreen {
        fn new(rows: usize) -> Self {
            SharedScreen(Rc::new(RefCell::new(Screen::new(rows))))
        }
    }

    impl Write for SharedScreen {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut s = self.0.borrow_mut();
            let mut i = 0;
            while i < bytes.len() {
                match bytes[i] {
                    b'\r' => {
                        s.col = 0;
                        i += 1;
                    }
                    b'\n' => {
                        s.line_feed();
                        i += 1;
                    }
                    0x1b => {
                        let rest = &bytes[i..];
                        if rest.starts_with(b"\x1b[K") {
                            let (row, col) = (s.row, s.col);
                            s.grid[row].truncate(col);
                            i += 3;
                        } else if rest.starts_with(b"\x1b[A") {
                            s.row = s.row.saturating_sub(1);
                            i += 3;
                        } else if rest.starts_with(b"\x1b7") {
                            // DECSC. Deliberately not `CSI s`: that spelling is ANSI.SYS and does
                            // not appear in terminfo, which is why apt moved off it (Debian
                            // #772521).
                            s.saved = Some((s.row, s.col));
                            i += 2;
                        } else if rest.starts_with(b"\x1b8") {
                            if let Some((r, c)) = s.saved {
                                s.row = r.min(s.rows - 1);
                                s.col = c;
                            }
                            i += 2;
                        } else if let Some((params, fin, len)) = csi(rest) {
                            match fin {
                                // DECSTBM. No parameters resets to the whole screen.
                                b'r' => {
                                    s.region = match params[..] {
                                        [t, b] if t >= 1 && b > t && (b as usize) <= s.rows => {
                                            Some((t as usize - 1, b as usize - 1))
                                        }
                                        _ => None,
                                    };
                                    // Setting the region homes the cursor, as a real terminal does.
                                    s.row = s.region.map_or(0, |(t, _)| t);
                                    s.col = 0;
                                }
                                // ED. `CSI J` (equivalently `CSI 0 J`) erases from the cursor to
                                // the end of the screen; `CSI 2 J` erases every visible row
                                // wherever the cursor is. Neither moves the cursor and neither
                                // touches scrollback — which is exactly the trade the second
                                // spelling makes when the terminal is resized (docs/ui.md). Mode
                                // 1 is not modelled because `FooterGuard` never emits it.
                                b'J' => match params.first().copied().unwrap_or(0) {
                                    0 => {
                                        let (row, col) = (s.row, s.col);
                                        s.grid[row].truncate(col);
                                        for line in s.grid[row + 1..].iter_mut() {
                                            line.clear();
                                        }
                                    }
                                    2 => {
                                        for line in s.grid.iter_mut() {
                                            line.clear();
                                        }
                                    }
                                    _ => {}
                                },
                                // CUP, 1-based and defaulting to the top-left corner.
                                b'H' => {
                                    let row = *params.first().unwrap_or(&1);
                                    let col = *params.get(1).unwrap_or(&1);
                                    s.row = (row.max(1) as usize - 1).min(s.rows - 1);
                                    s.col = col.max(1) as usize - 1;
                                }
                                _ => {}
                            }
                            i += len;
                        } else {
                            // Cursor show/hide and anything else: skip to the final byte.
                            i += 1;
                            while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
                                i += 1;
                            }
                            i += 1;
                        }
                    }
                    b => {
                        s.put(b);
                        i += 1;
                    }
                }
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // ---- the pinned-footer contract (#76) --------------------------------------------

    /// Draw a two-row footer on a `rows`-high screen and hand back the bytes written.
    fn pinned(rows: u16, footer: &[&str]) -> Vec<u8> {
        let mut g = FooterGuard::new(SharedBuf::default());
        g.draw(footer, TerminalSize::new(80, rows)).unwrap();
        let out = g.w.bytes();
        std::mem::forget(g); // no Drop bytes in the sample
        out
    }

    #[test]
    fn the_scroll_region_starts_at_row_one() {
        // **The single most load-bearing byte in this file.** alacritty only grows its scrollback
        // when the region starts at the top (`grid/mod.rs`: `if region.start == 0`), so a margin
        // anywhere else silently throws away the user's `cp -v` log. foot and tmux preserve it
        // either way, which is exactly why this needs a test rather than a terminal (#76).
        let out = pinned(24, &["name", "bar"]);
        assert!(
            contains(&out, b"\x1b[1;22r"),
            "region must be rows 1..=22 on a 24-row terminal: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn the_first_arm_clears_from_the_log_cursor_and_never_the_screen() {
        // **DECSTBM homes the cursor.** apt's own source says so where it does the same dance:
        // `// set scroll region (this will place the cursor in the top left)`. Setting the region
        // before saving captures the *home* position, so the restore hands the log back a cursor
        // at row 1 and the next relayed byte overwrites the log from the top. The save comes first.
        //
        // And once the restore has put the cursor back where the log left off, **everything below
        // it is footer by construction** — the footer is always written after the log. So erasing
        // to the end of the screen from there reserves the footer's area and touches nothing else.
        //
        // **`CSI 2 J` is not allowed here, and that is the point of this test.** The re-arm after
        // a resize does clear the whole screen (`re_arming_clears_the_rows_above_the_log_cursor_too`),
        // and the obvious way to write that — an unconditional `CSI 2 J` in `arm` — also fires on
        // the *first* arm, which happens mid-command the moment a file turns out to be slow. It
        // would wipe the shell prompt, the command the user typed, and whatever else was on
        // screen, for every single copy that shows a bar. Nothing above notices: the region, the
        // cursor bracket and the footer rows are all still correct.
        let out = pinned(24, &["name", "bar"]);
        let save = find(&out, SAVE_CURSOR).expect("saves");
        let region = find(&out, b"\x1b[1;22r").expect("sets the region");
        let restore = find(&out, RESTORE_CURSOR).expect("restores");
        let clear = find(&out, ERASE_TO_END).expect("clears");
        assert!(save < region, "save before the region set: {:?}", String::from_utf8_lossy(&out));
        assert!(region < restore, "restore after it: {:?}", String::from_utf8_lossy(&out));
        assert!(restore < clear, "clear after the restore: {:?}", String::from_utf8_lossy(&out));
        assert!(
            !contains(&out[restore + RESTORE_CURSOR.len()..clear], b"H"),
            "nothing repositions between the restore and the clear, or it is not the log's cursor \
             the clear starts from: {:?}",
            String::from_utf8_lossy(&out)
        );
        assert!(
            !contains(&out, ERASE_SCREEN),
            "the first arm must not blank the screen the user is looking at: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn the_footer_is_written_to_the_last_two_rows_by_absolute_position() {
        let out = pinned(24, &["name", "bar"]);
        assert!(contains(&out, b"\x1b[23;1Hname"), "row 23: {:?}", String::from_utf8_lossy(&out));
        assert!(contains(&out, b"\x1b[24;1Hbar"), "row 24: {:?}", String::from_utf8_lossy(&out));
        assert!(!contains(&out, b"\x1b[A"), "nothing walks back up any more");
    }

    #[test]
    fn drawing_the_footer_returns_the_cursor_to_the_log() {
        // The log keeps writing where it left off, so the repaint has to be bracketed. DECSC/DECRC
        // rather than `CSI s`/`CSI u`, which is ANSI.SYS and absent from terminfo (Debian #772521).
        // The *draw's* bracket, not the one `arm` uses around setting the region — hence the
        // search from the end. Both exist and both matter; this test is about the repaint.
        let out = pinned(24, &["name", "bar"]);
        let save = rfind(&out, SAVE_CURSOR).expect("saves the cursor");
        let restore = rfind(&out, RESTORE_CURSOR).expect("restores it");
        assert!(save < restore, "save comes first: {:?}", String::from_utf8_lossy(&out));
        let first_move = find(&out, b"\x1b[23;1H").unwrap();
        assert!(save < first_move && first_move < restore, "the jump is inside the bracket");
    }

    #[test]
    fn the_region_is_re_armed_whenever_the_terminal_size_changes() {
        // **Including a width-only change, and that is the bug this test exists for.** The first
        // version of this renderer re-armed only when the *height* moved, on the reasoning that
        // the region names rows. It shipped, and the original report reproduced against it: the
        // footer that is already on screen is as wide as the old terminal, a narrower one folds
        // it, and the fragments land inside the scrolling region where they scroll up with the
        // log — exactly the stack of repeated name rows in #76. Measured on a PTY: three
        // shrink/grow rounds emitted `arm` **zero** times, because `ws_row` never changed.
        //
        // apt re-arms on every SIGWINCH without comparing anything
        // (`install-progress.cc::CheckSIGWINCH`). One escape sequence is not worth being clever
        // about.
        let mut g = FooterGuard::new(SharedBuf::default());
        g.draw(&["a", "b"], TerminalSize::new(80, 24)).unwrap();
        g.draw(&["c", "d"], TerminalSize::new(80, 24)).unwrap();
        assert_eq!(count(&g.w.bytes(), b"\x1b[1;22r"), 1, "same size, one arm");

        g.w.0.borrow_mut().clear();
        g.draw(&["e", "f"], TerminalSize::new(50, 24)).unwrap();
        let narrower = g.w.bytes();
        assert_eq!(count(&narrower, b"\x1b[1;22r"), 1, "width-only change re-arms: {:?}", String::from_utf8_lossy(&narrower));
        assert!(
            contains(&narrower, ERASE_TO_END) && !contains(&narrower, ERASE_SCREEN),
            "and clears from the log cursor down, where a folded footer's fragments go — a shrink \
             must not cost the user the screen: {:?}",
            String::from_utf8_lossy(&narrower)
        );

        g.w.0.borrow_mut().clear();
        g.draw(&["g", "h"], TerminalSize::new(50, 30)).unwrap();
        assert_eq!(count(&g.w.bytes(), b"\x1b[1;28r"), 1, "a height change re-arms too");
        assert!(
            !contains(&g.w.bytes(), ERASE_SCREEN),
            "and a height change reflows nothing, so it does not clear the screen either"
        );

        g.w.0.borrow_mut().clear();
        g.draw(&["i", "j"], TerminalSize::new(80, 30)).unwrap();
        let wider = g.w.bytes();
        assert_eq!(count(&wider, b"\x1b[1;28r"), 1, "widening re-arms as well");
        assert!(
            contains(&wider, ERASE_SCREEN),
            "and it is the one case that clears the screen: {:?}",
            String::from_utf8_lossy(&wider)
        );
        std::mem::forget(g);
    }

    #[test]
    fn log_bytes_are_written_straight_through() {
        // The erase-then-redraw dance is gone: the footer lives outside the scrolling region, so
        // log output can never land on it and never has to be bracketed (ui.md invariant 11, C8).
        let mut g = FooterGuard::new(SharedBuf::default());
        g.draw(&["name", "bar"], TerminalSize::new(80, 24)).unwrap();
        g.w.0.borrow_mut().clear();
        g.write_log_chunks(std::iter::once(&b"'a' -> 'b'\n"[..])).unwrap();
        let out = g.w.bytes();
        assert_eq!(out, b"'a' -> 'b'\n", "just the bytes: {:?}", String::from_utf8_lossy(&out));
        std::mem::forget(g);
    }

    #[test]
    fn erase_clears_the_footer_rows_and_keeps_the_region() {
        // `erase` runs whenever the bar should go away without the run ending — between files,
        // or while the footer is suppressed. Nothing checked that it wrote anything at all:
        // replacing its whole body with `Ok(())` left the suite green (#76).
        let mut g = FooterGuard::new(SharedBuf::default());
        g.draw(&["name", "bar"], TerminalSize::new(80, 24)).unwrap();
        g.w.0.borrow_mut().clear();
        g.erase().unwrap();
        let out = g.w.bytes();
        assert_eq!(
            out,
            b"\x1b7\x1b[23;1H\x1b[J\x1b8",
            "clear from the footer's first row to the end of the screen, cursor bracketed: {:?}",
            String::from_utf8_lossy(&out)
        );
        assert!(!contains(&out, RELEASE_REGION), "and the region stays armed");
        std::mem::forget(g);
    }

    #[test]
    fn erase_is_a_no_op_before_anything_is_armed() {
        let mut g = FooterGuard::new(SharedBuf::default());
        g.erase().unwrap();
        assert!(g.w.bytes().is_empty(), "nothing on screen, nothing to clear");
        std::mem::forget(g);
    }

    #[test]
    fn a_terminal_too_short_for_a_footer_gets_nothing_at_all() {
        // `MIN_LOG_ROWS + FOOTER_ROWS` is 4: two rows of log plus the two the footer takes. Below
        // that there is no room to reserve, so nothing is armed and nothing is drawn — the copy
        // just runs. `ui::render_footer` refuses at the same boundary; this is the renderer's half
        // of it, and the boundary is exclusive on both sides.
        for rows in [0u16, 1, 2, 3] {
            let mut g = FooterGuard::new(SharedBuf::default());
            g.draw(&["name", "bar"], TerminalSize::new(80, rows)).unwrap();
            assert!(g.w.bytes().is_empty(), "{rows} rows is too short: nothing may be written");
            assert!(g.region_for.is_none(), "{rows} rows: no region armed");
            std::mem::forget(g);
        }
        let mut g = FooterGuard::new(SharedBuf::default());
        g.draw(&["name", "bar"], TerminalSize::new(80, 4)).unwrap();
        assert!(contains(&g.w.bytes(), b"\x1b[1;2r"), "four rows is exactly enough");
        std::mem::forget(g);
    }

    #[test]
    fn an_empty_row_list_draws_nothing() {
        // The other half of the same guard. `footer_for` never returns an empty row list, but the
        // two conditions are independent and a mutant that merges them has to fail somewhere.
        let mut g = FooterGuard::new(SharedBuf::default());
        g.draw(&[], TerminalSize::new(80, 24)).unwrap();
        assert!(g.w.bytes().is_empty(), "no rows, no output");
        assert!(g.region_for.is_none(), "and no region armed for a footer that is not coming");
        std::mem::forget(g);
    }

    #[test]
    fn drop_releases_the_region_and_clears_the_footer() {
        // Leave a region behind and the user's shell keeps drawing inside it. The release has to
        // be flushed before anything else writes — apt learned that one from a post-invoke hook
        // printing into the residue (Debian #793672).
        let rec = SharedBuf::default();
        {
            let mut g = FooterGuard::new(rec.clone());
            g.draw(&["name", "bar"], TerminalSize::new(80, 24)).unwrap();
            g.w.0.borrow_mut().clear();
        }
        let out = rec.bytes();
        let release = find(&out, b"\x1b[r").expect("releases the region");
        let show = find(&out, SHOW_CURSOR).expect("restores the cursor");
        assert!(release < show, "region first, then the cursor: {:?}", String::from_utf8_lossy(&out));
        assert!(contains(&out, b"\x1b[23;1H"), "and clears from the footer's first row");
    }

    #[test]
    fn suspend_restore_releases_the_region_and_the_next_draw_re_arms_it() {
        // Ctrl-Z hands the terminal back to the shell. A region left set would confine the
        // shell's own prompt to the top of the screen until cprog is resumed.
        let mut g = FooterGuard::new(SharedBuf::default());
        g.draw(&["name", "bar"], TerminalSize::new(80, 24)).unwrap();
        g.w.0.borrow_mut().clear();
        g.suspend_restore().unwrap();
        assert!(contains(&g.w.bytes(), b"\x1b[r"), "released on suspend");
        g.w.0.borrow_mut().clear();
        g.draw(&["name", "bar"], TerminalSize::new(80, 24)).unwrap();
        assert!(contains(&g.w.bytes(), b"\x1b[1;22r"), "re-armed on resume");
        std::mem::forget(g);
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }
    fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).rposition(|w| w == needle)
    }
    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        find(hay, needle).is_some()
    }
    fn count(hay: &[u8], needle: &[u8]) -> usize {
        hay.windows(needle.len()).filter(|w| *w == needle).count()
    }

    // ---- the Screen model's own contract -------------------------------------------
    //
    // These test the *model*, not `FooterGuard`. #62 is why: `render_screen` did not understand
    // `CSI A`, so it rebuilt a screen the terminal had never shown and every assertion made on it
    // was worth less than it looked. A model that is about to grow scroll regions and absolute
    // addressing has to be pinned sequence by sequence before anything is measured on it.

    /// Feed `bytes` to a fresh screen of `rows` rows and hand back the screen.
    fn screen_after(rows: usize, bytes: &[u8]) -> SharedScreen {
        let mut s = SharedScreen::new(rows);
        s.write_all(bytes).unwrap();
        s
    }

    #[test]
    fn the_model_scrolls_only_inside_the_region() {
        // `ESC[1;3r` keeps rows 4-5 fixed: a line feed on row 3 rotates rows 1-3 and leaves the
        // rest alone. Without region support the whole screen would scroll and the fixed rows
        // would drift up.
        // `\r\n`, not a bare `\n`: a line feed moves down without returning to column 0, on a
        // real terminal and in this model alike.
        let s = screen_after(5, b"\x1b[1;3rA\r\nB\r\nC\r\nD");
        let v = s.0.borrow().visible();
        assert_eq!(v[0], "B", "row 1 rotated up");
        assert_eq!(v[1], "C");
        assert_eq!(v[2], "D", "row 3 is the region's last row");
        assert_eq!(v[3], "", "row 4 is outside the region and untouched");
        assert_eq!(v[4], "");
    }

    #[test]
    fn the_model_feeds_scrollback_only_when_the_top_margin_is_row_one() {
        // The rule cprog's whole design rests on. alacritty only grows its history when the
        // scrolling region starts at the top (`grid/mod.rs`: `if region.start == 0`), so a footer
        // that pins the margin anywhere else loses the user's log silently. The model reproduces
        // that, which is what lets a test catch a change that moves the margin (docs/ui.md).
        let top = screen_after(5, b"\x1b[1;3rA\r\nB\r\nC\r\nD\r\nE");
        assert_eq!(top.0.borrow().scrolled_off, vec!["A", "B"], "margin at row 1 -> scrollback");

        let inset = screen_after(5, b"\x1b[2;4r\x1b[2;1HA\r\nB\r\nC\r\nD\r\nE");
        assert!(
            inset.0.borrow().scrolled_off.is_empty(),
            "margin below row 1 -> the lines are gone, not scrolled back: {:?}",
            inset.0.borrow().scrolled_off
        );
    }

    #[test]
    fn the_model_places_the_cursor_absolutely() {
        // `ESC[{row};{col}H` is 1-based. This is how the footer will be drawn once it stops
        // counting rows relative to itself.
        let s = screen_after(4, b"\x1b[3;1HX\x1b[1;2HY");
        let v = s.0.borrow().visible();
        assert_eq!(v[2], "X", "row 3, column 1");
        assert_eq!(v[0], " Y", "row 1, column 2");
    }

    #[test]
    fn the_model_saves_and_restores_the_cursor() {
        // `ESC 7` / `ESC 8` (DECSC/DECRC) — not `CSI s`/`CSI u`, which are ANSI.SYS and absent
        // from terminfo (Debian #772521). The footer draw jumps away and must come back.
        let s = screen_after(4, b"AB\x1b7\x1b[4;1HZ\x1b8C");
        let v = s.0.borrow().visible();
        assert_eq!(v[0], "ABC", "the write after the restore continued where it left off");
        assert_eq!(v[3], "Z", "and the detour really wrote to row 4");
    }

    #[test]
    fn resetting_the_region_restores_whole_screen_scrolling() {
        // `ESC[r` with no parameters. Teardown depends on this: leave a region behind and the
        // user's shell keeps drawing inside it.
        let s = screen_after(3, b"\x1b[1;2r\x1b[rA\r\nB\r\nC\r\nD");
        let v = s.0.borrow().visible();
        assert_eq!(v, vec!["B", "C", "D"], "the whole screen scrolls again");
        assert_eq!(s.0.borrow().scrolled_off, vec!["A"]);
    }

    #[test]
    fn the_model_erases_from_the_cursor_and_erases_the_whole_screen() {
        // The two spellings of ED, and the difference between them is the second half of #76:
        // `CSI J` can only reach forwards from the cursor, `CSI 2 J` reaches the rows above it too.
        // Neither moves the cursor — the write after each erase has to land where it already was.
        let s = screen_after(4, b"\x1b[1;1HAAA\x1b[2;1HBBB\x1b[3;1HCCC\x1b[4;1HDDD\x1b[2;2H\x1b[J");
        let v = s.0.borrow().visible();
        assert_eq!(v[0], "AAA", "the row above the cursor is out of `CSI J`'s reach");
        assert_eq!(v[1], "B", "the cursor's own row is cut at the cursor");
        assert_eq!(v, vec!["AAA", "B", "", ""], "and everything below is gone: {v:?}");
        s.clone().write_all(b"X").unwrap();
        assert_eq!(s.0.borrow().visible()[1], "BX", "the erase did not move the cursor");

        let all = screen_after(4, b"\x1b[1;1HAAA\x1b[2;1HBBB\x1b[3;1HCCC\x1b[2;2H\x1b[2J");
        assert_eq!(all.0.borrow().visible(), vec!["", "", "", ""], "`CSI 2 J` takes every row");
        all.clone().write_all(b"X").unwrap();
        assert_eq!(all.0.borrow().visible()[1], " X", "and leaves the cursor alone as well");
    }

    #[test]
    fn erasing_the_screen_does_not_touch_the_scrollback() {
        // The cost `CSI 2 J` is chosen for, stated where it can be checked: the rows the log has
        // already pushed off the top stay in the scrollback, so `cp -v` history survives a resize
        // even though the visible copy of it does not (docs/ui.md).
        let s = screen_after(3, b"A\r\nB\r\nC\r\nD\x1b[2J");
        assert_eq!(s.0.borrow().visible(), vec!["", "", ""], "the visible screen is blank");
        assert_eq!(s.0.borrow().scrolled_off, vec!["A"], "and the scrollback is not");
    }

    #[test]
    fn a_resize_during_a_stop_is_still_a_resize_on_resume() {
        // Ctrl-Z releases the region, so `region_for` is `None` by the time the user resizes the
        // window and types `fg`. If the memory of the last size went with it, the resume would
        // look like a first arm and the reflow residue from the resize would stay on screen until
        // the *next* resize came along. `last_armed_for` outliving `release` is what makes the
        // difference, and nothing else needs it.
        let mut g = FooterGuard::new(SharedBuf::default());
        g.draw(&["name", "bar"], TerminalSize::new(60, 24)).unwrap();
        g.suspend_restore().unwrap();
        g.w.0.borrow_mut().clear();
        g.draw(&["name", "bar"], TerminalSize::new(80, 24)).unwrap();
        assert!(
            contains(&g.w.bytes(), ERASE_SCREEN),
            "resumed wider, so the screen is cleared: {:?}",
            String::from_utf8_lossy(&g.w.bytes())
        );
        std::mem::forget(g);
    }

    /// Drive a guard through a size change on a `rows`-high screen, with enough log above the
    /// footer to have scrolled, and a fragment planted on row 2 as a reflow would leave one.
    /// Hands back the screen so the caller can say what should have survived.
    fn resized_from_to(
        rows: usize,
        before: TerminalSize,
        after: TerminalSize,
    ) -> (SharedScreen, Vec<String>) {
        let screen = SharedScreen::new(rows);
        let mut g = FooterGuard::new(screen.clone());
        for i in 1..=12 {
            g.write_log_chunks(std::iter::once(format!("L{i}\r\n").as_bytes())).unwrap();
        }
        g.draw(&["name", "bar"], before).unwrap();
        let scrollback = screen.0.borrow().scrolled_off.clone();
        screen.0.borrow_mut().grid[2] = "…/archives/dataset.tar.zst".as_bytes().to_vec();
        g.draw(&["name", "bar"], after).unwrap();
        std::mem::forget(g);
        (screen, scrollback)
    }

    #[test]
    fn shrinking_leaves_the_visible_log_where_it_is() {
        // The other half of the trade, and the reason the clear is not simply "on any resize".
        //
        // `CSI 2 J` costs the user everything on screen, and with `-v` that is the `cp` log they
        // asked for — a single large file prints one line and then nothing for minutes, so a blank
        // screen *stays* blank. Shrinking does not need paying for: the fold pushes the old
        // footer's fragments **down**, where an erase from the log's cursor already reaches them,
        // which is what `67e20be` fixed and what the terminal confirmed. Only a widening pulls a
        // fragment back up out of the scrollback.
        //
        // So the whole-screen clear is gated on the columns *growing*. A row-only change does not
        // reflow line wrapping either, and is treated the same as a shrink.
        let (narrower, _) = resized_from_to(10, TerminalSize::new(80, 10), TerminalSize::new(60, 10));
        let v = narrower.0.borrow().visible();
        assert!(
            v[..8].iter().filter(|r| !r.is_empty()).count() > 1,
            "a shrink must not blank the log the user is reading: {v:?}"
        );
        assert_eq!(v[0], "L4", "the rows above the cursor are untouched: {v:?}");

        let (shorter, _) = resized_from_to(10, TerminalSize::new(80, 10), TerminalSize::new(80, 8));
        assert!(
            shorter.0.borrow().visible()[..6].iter().any(|r| !r.is_empty()),
            "and neither does a height-only change, which reflows nothing"
        );
    }

    #[test]
    fn re_arming_clears_the_rows_above_the_log_cursor_too() {
        // The second half of #76, and the one an erase from the cursor cannot do.
        //
        // Shrinking a terminal folds the footer that is on screen and pushes the fragments *down*,
        // which is why `arm` erasing from the log cursor to the end of the screen fixed that
        // direction. **Widening does the opposite**: the terminal pulls lines back out of the
        // scrollback to fill the wider rows, and a fragment of the footer that was pushed off
        // earlier comes back *above* where the log left off. `CSI J` cannot reach it. `CSI 2 J`
        // can, so that is what a re-arm writes.
        //
        // The model has no reflow — it is a grid, not a terminal — so the returning fragment is
        // placed by hand here. What this pins is only "the erase reaches above the cursor", which
        // is the property the fix turns on; whether a real terminal puts the fragment there is the
        // observation from #76 that a byte-level model cannot make either way.
        //
        // 60 -> 80 columns, and the direction is the point: the same move the other way must
        // *not* clear (`shrinking_leaves_the_visible_log_where_it_is`).
        let (screen, scrollback) =
            resized_from_to(10, TerminalSize::new(60, 10), TerminalSize::new(80, 10));
        assert!(!scrollback.is_empty(), "the log scrolled, so there is scrollback to protect");
        let v = screen.0.borrow().visible();
        assert_eq!(v[2], "", "the fragment above the cursor is gone: {v:?}");
        assert_eq!(
            v[..8].iter().filter(|r| !r.is_empty()).count(),
            0,
            "and so is every other visible log row — the cost this buys the fix with: {v:?}"
        );
        assert_eq!((&v[8], &v[9]), (&"name".to_string(), &"bar".to_string()), "footer redrawn");
        assert_eq!(
            screen.0.borrow().scrolled_off,
            scrollback,
            "while the scrollback, where the log's history actually lives, is untouched"
        );
    }

    #[test]
    fn cursor_hidden_only_once() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["A"], TerminalSize::new(80, 24)).unwrap();
        let mark = buf.bytes().len();
        g.draw(&["B"], TerminalSize::new(80, 24)).unwrap();
        // A redraw does not re-emit the hide sequence.
        assert!(!buf.bytes()[mark..].windows(HIDE.len()).any(|w| w == HIDE));
    }

    #[test]
    fn suspend_restore_shows_cursor_then_redraw_re_hides() {
        // bug2: on SIGTSTP the footer is erased and the cursor shown, and state resets so that
        // after resume the next draw re-hides the cursor and redraws the footer.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["F"], TerminalSize::new(80, 24)).unwrap(); // hide cursor + footer
        let mark = buf.bytes().len();
        g.suspend_restore().unwrap();
        assert_eq!(
            &buf.bytes()[mark..],
            b"\x1b7\x1b[r\x1b[23;1H\x1b[J\x1b8\x1b[?25h",
            "release the region and clear from the footer's first row, then show the cursor"
        );
        let mark2 = buf.bytes().len();
        g.draw(&["F"], TerminalSize::new(80, 24)).unwrap(); // resumed
        assert_eq!(
            &buf.bytes()[mark2..],
            b"\x1b7\x1b[1;22r\x1b8\x1b[J\x1b[?25l\x1b7\x1b[24;1HF\x1b[K\x1b8",
            "re-arm inside a save/restore, clear from the log cursor down — the terminal is the \
             same size as before the stop, so there is no reflow residue to chase and no reason \
             to blank the screen the shell has just drawn into — then re-hide and repaint"
        );
    }

    #[test]
    fn drop_without_footer_writes_nothing() {
        let buf = SharedBuf::default();
        {
            let _g = FooterGuard::new(buf.clone());
        }
        assert!(buf.bytes().is_empty());
    }

    #[test]
    fn drop_erases_even_on_panic() {
        // docs/testing.md C7: a panic mid-render still restores the screen via Drop.
        let buf = SharedBuf::default();
        let inner = buf.clone();
        let prev = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        let r = panic::catch_unwind(AssertUnwindSafe(|| {
            let mut g = FooterGuard::new(inner);
            g.draw(&["F"], TerminalSize::new(80, 24)).unwrap();
            panic!("boom");
        }));
        panic::set_hook(prev);
        assert!(r.is_err());
        // Region released, footer cleared and cursor restored, all while unwinding.
        let out = buf.bytes();
        assert!(out.ends_with(SHOW), "cursor restored during unwind");
        assert!(contains(&out, RELEASE_REGION), "and the region handed back");
        assert!(contains(&out, ERASE_TO_END), "and the footer rows cleared");
    }

    #[test]
    fn io_failure_is_returned_and_drop_never_panics() {
        // docs/testing.md C6: render IO failures are best-effort; they must not panic.
        //
        // This half covers the failure that arrives *before* anything is on screen: the first
        // write is `HIDE_CURSOR`, so `draw` gives up before anything is on screen. What `Drop` then
        // has to survive is only the `SHOW_CURSOR` write, because `release()` returns early with
        // no region recorded and never touches the writer at all. The sibling below is the other way.
        let mut g = FooterGuard::new(FailWriter);
        assert!(g.draw(&["F"], TerminalSize::new(80, 24)).is_err());
        drop(g); // the cursor restore over a failing writer must not panic
    }

    #[test]
    fn drop_never_panics_when_the_teardown_itself_fails() {
        // The other half, and the one that was uncovered (#69): a footer that *is* on screen when
        // the writer dies. `Drop` calls `release`, which releases the region, walks to the
        // footer's first row and erases to the end of the screen — five writes over a writer that
        // has already failed once would panic if any of them were unwrapped.
        //
        // Thirteen is measured, not counted: probing 0..20 puts the smallest budget that lets
        // `draw(&["F"], TerminalSize::new(80, 24))` succeed at **11** — arming is four writes
        // (save, set region, restore, clear to end of screen). Thirteen therefore lets `release`
        // land two writes before failing on the third, so `Drop` has to survive a teardown that
        // already put bytes on the wire, and the cursor restore after it fails too.
        let mut g = FooterGuard::new(FailAfter(13));
        g.draw(&["F"], TerminalSize::new(80, 24)).expect("the draw must succeed, or this test is the sibling above again");
        assert!(g.region_for.is_some(), "a region is armed, so Drop has work to do");
        drop(g); // teardown over a writer that has since failed must not panic
    }
}
