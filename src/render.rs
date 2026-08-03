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

/// Owns the terminal writer and manages the footer rows, erasing them on drop.
pub struct FooterGuard<W: Write> {
    w: W,
    /// The terminal height the scrolling region is currently set for, or `None` when the region
    /// has been released. Both a marker that teardown owes a `RELEASE_REGION` and the value that
    /// says whether a resize needs the region re-armed.
    region_rows: Option<u16>,
    /// Whether we have hidden the cursor (and thus must restore it on teardown).
    cursor_hidden: bool,
}

impl<W: Write> FooterGuard<W> {
    /// Wrap a writer with no footer shown yet.
    pub fn new(w: W) -> Self {
        Self { w, region_rows: None, cursor_hidden: false }
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
    pub fn draw(&mut self, rows: &[&str], term_rows: u16) -> io::Result<()> {
        if rows.is_empty() || term_rows < MIN_LOG_ROWS + FOOTER_ROWS {
            return Ok(());
        }
        self.arm(term_rows)?;
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
        let Some(term_rows) = self.region_rows else {
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
    fn arm(&mut self, term_rows: u16) -> io::Result<()> {
        if self.region_rows == Some(term_rows) {
            return Ok(());
        }
        self.region_rows = Some(term_rows);
        let bottom = term_rows - FOOTER_ROWS;
        self.w.write_all(format!("\x1b[1;{bottom}r").as_bytes())
    }

    /// Take the footer down and hand the whole screen back.
    ///
    /// Order matters: the region is released *before* the rows are cleared, so `CSI J` erases
    /// them as ordinary screen rather than as a fixed area, and the caller's flush covers both.
    /// apt learned the flushing half from a post-invoke hook printing into the residue of an
    /// unflushed clear (Debian #793672).
    fn release(&mut self) -> io::Result<()> {
        let Some(term_rows) = self.region_rows.take() else {
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
        g.draw(footer, rows).unwrap();
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
        let out = pinned(24, &["name", "bar"]);
        let save = find(&out, SAVE_CURSOR).expect("saves the cursor");
        let restore = find(&out, RESTORE_CURSOR).expect("restores it");
        assert!(save < restore, "save comes first: {:?}", String::from_utf8_lossy(&out));
        let first_move = find(&out, b"\x1b[23;1H").unwrap();
        assert!(save < first_move && first_move < restore, "the jump is inside the bracket");
    }

    #[test]
    fn the_region_is_armed_once_and_re_armed_only_when_the_height_changes() {
        let mut g = FooterGuard::new(SharedBuf::default());
        g.draw(&["a", "b"], 24).unwrap();
        g.draw(&["c", "d"], 24).unwrap();
        let twice = g.w.bytes();
        assert_eq!(count(&twice, b"\x1b[1;22r"), 1, "one arm for two draws at one height");
        g.draw(&["e", "f"], 30).unwrap();
        let after = g.w.bytes();
        assert_eq!(count(&after, b"\x1b[1;28r"), 1, "a new height re-arms: {:?}", String::from_utf8_lossy(&after));
        std::mem::forget(g);
    }

    #[test]
    fn log_bytes_are_written_straight_through() {
        // The erase-then-redraw dance is gone: the footer lives outside the scrolling region, so
        // log output can never land on it and never has to be bracketed (ui.md invariant 11, C8).
        let mut g = FooterGuard::new(SharedBuf::default());
        g.draw(&["name", "bar"], 24).unwrap();
        g.w.0.borrow_mut().clear();
        g.write_log_chunks(std::iter::once(&b"'a' -> 'b'\n"[..])).unwrap();
        let out = g.w.bytes();
        assert_eq!(out, b"'a' -> 'b'\n", "just the bytes: {:?}", String::from_utf8_lossy(&out));
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
            g.draw(&["name", "bar"], 24).unwrap();
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
        g.draw(&["name", "bar"], 24).unwrap();
        g.w.0.borrow_mut().clear();
        g.suspend_restore().unwrap();
        assert!(contains(&g.w.bytes(), b"\x1b[r"), "released on suspend");
        g.w.0.borrow_mut().clear();
        g.draw(&["name", "bar"], 24).unwrap();
        assert!(contains(&g.w.bytes(), b"\x1b[1;22r"), "re-armed on resume");
        std::mem::forget(g);
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
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
    fn cursor_hidden_only_once() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["A"], 24).unwrap();
        let mark = buf.bytes().len();
        g.draw(&["B"], 24).unwrap();
        // A redraw does not re-emit the hide sequence.
        assert!(!buf.bytes()[mark..].windows(HIDE.len()).any(|w| w == HIDE));
    }

    #[test]
    fn suspend_restore_shows_cursor_then_redraw_re_hides() {
        // bug2: on SIGTSTP the footer is erased and the cursor shown, and state resets so that
        // after resume the next draw re-hides the cursor and redraws the footer.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["F"], 24).unwrap(); // hide cursor + footer
        let mark = buf.bytes().len();
        g.suspend_restore().unwrap();
        assert_eq!(
            &buf.bytes()[mark..],
            b"\x1b7\x1b[r\x1b[23;1H\x1b[J\x1b8\x1b[?25h",
            "release the region and clear from the footer's first row, then show the cursor"
        );
        let mark2 = buf.bytes().len();
        g.draw(&["F"], 24).unwrap(); // resumed
        assert_eq!(
            &buf.bytes()[mark2..],
            b"\x1b[1;22r\x1b[?25l\x1b7\x1b[24;1HF\x1b[K\x1b8",
            "the redraw re-arms the region, re-hides the cursor and repaints"
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
            g.draw(&["F"], 24).unwrap();
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
        assert!(g.draw(&["F"], 24).is_err());
        drop(g); // the cursor restore over a failing writer must not panic
    }

    #[test]
    fn drop_never_panics_when_the_teardown_itself_fails() {
        // The other half, and the one that was uncovered (#69): a footer that *is* on screen when
        // the writer dies. `Drop` calls `release`, which releases the region, walks to the
        // footer's first row and erases to the end of the screen — five writes over a writer that
        // has already failed once would panic if any of them were unwrapped.
        //
        // Ten is measured, not counted: probing 0..14 puts the smallest budget that lets
        // `draw(&["F"], 24)` succeed at **8** (arm, hide, save, goto, row, erase-to-eol, restore,
        // and a flush that needs the budget unspent). Ten therefore lets `release` land two writes
        // before failing on the third, so `Drop` has to survive a teardown that already put bytes
        // on the wire — and the cursor restore after it fails too.
        let mut g = FooterGuard::new(FailAfter(10));
        g.draw(&["F"], 24).expect("the draw must succeed, or this test is the sibling above again");
        assert!(g.region_rows.is_some(), "a region is armed, so Drop has work to do");
        drop(g); // teardown over a writer that has since failed must not panic
    }
}
