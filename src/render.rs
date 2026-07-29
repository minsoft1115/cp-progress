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

/// Move to column 0 (carriage return).
const CR: &[u8] = b"\r";
/// Erase from the cursor to the end of the line (`CSI K`), clearing any leftover from a
/// previously longer footer.
const ERASE_EOL: &[u8] = b"\x1b[K";
/// Hide / show the terminal cursor (DECTCEM). The cursor would otherwise sit and blink at the
/// end of the footer bar; we hide it while the footer is live and restore it on teardown.
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
/// Move the cursor up one line (`CSI A`), used to erase a multi-row footer from the bottom up.
const CURSOR_UP: &[u8] = b"\x1b[A";

/// Owns the terminal writer and manages the footer rows, erasing them on drop.
pub struct FooterGuard<W: Write> {
    w: W,
    /// How many rows the footer currently occupies on screen; 0 when none is shown. The erase
    /// path walks back exactly this far, so it has to match what `draw` last wrote.
    shown_rows: usize,
    /// Whether we have hidden the cursor (and thus must restore it on teardown).
    cursor_hidden: bool,
    /// Whether the last log bytes written left an unterminated line on screen (docs/ui.md
    /// invariant 11). While true the footer is withheld — see [`Self::line_pending`].
    line_pending: bool,
}

impl<W: Write> FooterGuard<W> {
    /// Wrap a writer with no footer shown yet.
    pub fn new(w: W) -> Self {
        Self { w, shown_rows: 0, cursor_hidden: false, line_pending: false }
    }

    /// Whether an unterminated log line is currently on screen.
    ///
    /// The footer draws from column 0 (`\r`), and the next erase clears the whole line, so
    /// drawing it over a partial line makes that log text vanish. `cp` hits this routinely:
    /// glibc's `error()` emits one error line as four separate writes, and the relay forwards
    /// each fragment as it arrives (it must not wait for a newline). This is tracked on the
    /// bytes actually written to the terminal, because stdout and stderr merge on one screen.
    ///
    /// Test-only as an accessor: production code goes through [`Self::write_log`] and
    /// [`Self::draw_unless_line_pending`], which consult the flag themselves.
    #[cfg(test)]
    pub fn line_pending(&self) -> bool {
        self.line_pending
    }

    /// Draw (or redraw in place) the footer's rows, top to bottom. Any footer already on screen
    /// is erased first, so rows that shrink leave no residue. On the first draw the cursor is
    /// hidden (restored on drop) so it does not blink at the end of the bar.
    ///
    /// Each row must already fit the terminal width. A row the terminal folds would occupy two
    /// physical lines while [`Self::erase`] walks back over one, and every write after that lands
    /// a line off (docs/ui.md invariant 7).
    pub fn draw(&mut self, rows: &[&str]) -> io::Result<()> {
        if rows.is_empty() {
            return self.erase();
        }
        // A footer of a different height cannot be overwritten row for row, so clear it first.
        // In practice the height is fixed for a whole run; this only guards the general case.
        if self.shown_rows != 0 && self.shown_rows != rows.len() {
            self.erase()?;
        }
        if !self.cursor_hidden {
            // Mark hidden before writing so a partial write is still restored on drop.
            self.cursor_hidden = true;
            self.w.write_all(HIDE_CURSOR)?;
        }
        // Redraw in place rather than erase-then-draw: the rows would otherwise blank for an
        // instant on every tick, and "no flicker" is the point of the erase-redraw discipline
        // (docs/ui.md). The cursor sits on the bottom row, so walk it back up to the first.
        for _ in 1..self.shown_rows {
            self.w.write_all(CURSOR_UP)?;
        }
        // Count the rows before writing them, so a partial write is still cleaned up on drop.
        self.shown_rows = rows.len();
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                self.w.write_all(b"\n")?;
            }
            self.w.write_all(CR)?;
            self.w.write_all(row.as_bytes())?;
            self.w.write_all(ERASE_EOL)?;
        }
        self.w.flush()
    }

    /// Erase the footer if one is shown; a no-op otherwise.
    ///
    /// The cursor sits on the bottom row, so this clears upwards and leaves it at column 0 of the
    /// row the footer started on — where the next log bytes belong.
    pub fn erase(&mut self) -> io::Result<()> {
        let rows = std::mem::take(&mut self.shown_rows);
        if rows == 0 {
            return Ok(());
        }
        for i in 0..rows {
            if i > 0 {
                self.w.write_all(CURSOR_UP)?;
            }
            self.w.write_all(CR)?;
            self.w.write_all(ERASE_EOL)?;
        }
        self.w.flush()
    }

    /// Restore the terminal for a job-control suspend (`SIGTSTP` / Ctrl-Z): erase the footer and
    /// show the cursor, and reset our state to "fresh" so that after the process is resumed the
    /// next [`draw`](Self::draw) re-hides the cursor and redraws the footer. Unlike `Drop`, the
    /// guard keeps living across the stop.
    pub fn suspend_restore(&mut self) -> io::Result<()> {
        self.erase()?; // footer gone, `shown_rows` back to 0
        if self.cursor_hidden {
            self.cursor_hidden = false;
            self.w.write_all(SHOW_CURSOR)?;
        }
        self.w.flush()
    }

    /// Draw the footer unless an unterminated log line is on screen (docs/ui.md invariant 11).
    /// This is what the render loop's idle tick should call; [`Self::draw`] stays the primitive.
    pub fn draw_unless_line_pending(&mut self, rows: &[&str]) -> io::Result<()> {
        if self.line_pending {
            return Ok(());
        }
        self.draw(rows)
    }

    /// Relay one run of log bytes; see [`Self::write_log_chunks`], which this defers to.
    /// Test-only: the render loop always has a drained batch and calls the chunked form.
    #[cfg(test)]
    pub fn write_log(&mut self, bytes: &[u8], footer: Option<&[&str]>) -> io::Result<()> {
        self.write_log_chunks(std::iter::once(bytes), footer)
    }

    /// Relay several drained chunks through the sole writer: erase the footer, write each chunk in
    /// turn, then redraw the footer if one should remain (docs/testing.md C8).
    ///
    /// The chunks are written separately rather than concatenated first. The writer buffers, so
    /// joining them would only add a copy and a reallocation on a path that runs once per file
    /// during a large recursive copy.
    ///
    /// If the bytes do not end on a line boundary the footer is withheld, because drawing it
    /// would overwrite the partial line and the next erase would take it off screen entirely
    /// (docs/ui.md invariant 11). It comes back with the write that completes the line.
    /// Taking an iterator rather than a slice keeps the caller from having to materialise one.
    pub fn write_log_chunks<'a>(
        &mut self,
        chunks: impl IntoIterator<Item = &'a [u8]>,
        footer: Option<&[&str]>,
    ) -> io::Result<()> {
        self.erase()?;
        for chunk in chunks {
            self.w.write_all(chunk)?;
            if let Some(&last) = chunk.last() {
                self.line_pending = last != b'\n';
            }
        }
        match footer {
            Some(rows) if !self.line_pending => self.draw(rows),
            _ => self.w.flush(),
        }
    }
}

impl<W: Write> Drop for FooterGuard<W> {
    fn drop(&mut self) {
        // Best-effort screen restore on every exit path; failures must not panic.
        let _ = self.erase();
        if self.cursor_hidden {
            let _ = self.w.write_all(SHOW_CURSOR);
            let _ = self.w.flush();
        }
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

    const ERASE: &[u8] = b"\r\x1b[K";
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
    }

    impl Screen {
        fn new(rows: usize) -> Self {
            Screen { rows, grid: vec![Vec::new(); rows], ..Default::default() }
        }

        fn line_feed(&mut self) {
            if self.row + 1 < self.rows {
                self.row += 1;
            } else {
                self.scrolled_off.push(text(&self.grid.remove(0)));
                self.grid.push(Vec::new());
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

        /// Everything the user could still read: scrollback then the visible rows.
        fn all_lines(&self) -> Vec<String> {
            self.scrolled_off.iter().cloned().chain(self.visible()).collect()
        }
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

    #[test]
    fn a_footer_at_the_bottom_row_survives_the_scroll_it_causes() {
        // #35 / docs/testing.md C9. Once the footer reaches the last line, the `\n` between
        // its two rows scrolls the screen — and the next erase still walks up exactly one row.
        // Get that wrong and every following write lands a line off, silently eating log output.
        //
        // Six rows is the smallest terminal that gets a footer at all (MIN_LOG_ROWS 2 +
        // FOOTER_ROWS 2) with room to actually reach the bottom.
        let screen = SharedScreen::new(6);
        let footer = ["NAME", "BAR"];
        {
            let mut g = FooterGuard::new(screen.clone());
            for i in 0..6 {
                g.write_log(format!("L{i}\n").as_bytes(), Some(&footer[..])).unwrap();
            }

            let s = screen.0.borrow();
            assert_eq!(
                s.visible(),
                vec!["L2", "L3", "L4", "L5", "NAME", "BAR"],
                "the footer holds the last two rows and the log keeps the ones above"
            );
            assert_eq!(s.scrolled_off, vec!["L0", "L1"], "only whole log lines scrolled off");
        }

        // Every log line is readable exactly once: none overwritten by a misplaced footer, none
        // duplicated by a redraw landing on the wrong row.
        let s = screen.0.borrow();
        let lines = s.all_lines();
        for i in 0..6 {
            let want = format!("L{i}");
            assert_eq!(
                lines.iter().filter(|l| **l == want).count(),
                1,
                "{want} must appear exactly once in {lines:?}"
            );
        }
    }

    #[test]
    fn a_tick_redraw_at_the_bottom_row_does_not_scroll() {
        // The commonest state at runtime: one big file, no new log bytes, the footer redrawn
        // every render tick in place. At the bottom row that redraw must walk the cursor back up
        // first — otherwise its own `\n` scrolls a log line away *per tick*, and the previous
        // name row is left stranded above the new one.
        let screen = SharedScreen::new(6);
        let mut g = FooterGuard::new(screen.clone());
        for i in 0..6 {
            g.write_log(format!("L{i}\n").as_bytes(), Some(&["NAME", "BAR"][..])).unwrap();
        }
        let scrolled_before = screen.0.borrow().scrolled_off.len();

        for tick in 0..3 {
            g.draw(&[&format!("NAME{tick}"), &format!("BAR{tick}")]).unwrap();
        }

        let s = screen.0.borrow();
        assert_eq!(
            s.visible(),
            vec!["L2", "L3", "L4", "L5", "NAME2", "BAR2"],
            "three redraws replace the same two rows"
        );
        assert_eq!(
            s.scrolled_off.len(),
            scrolled_before,
            "an in-place redraw must not scroll the log away"
        );
    }

    #[test]
    fn dropping_the_guard_at_the_bottom_row_clears_only_the_footer() {
        // The same boundary on the teardown path: Drop erases two rows from the last line, and
        // must not take the log line above the footer with it.
        let screen = SharedScreen::new(6);
        {
            let mut g = FooterGuard::new(screen.clone());
            for i in 0..6 {
                g.write_log(format!("L{i}\n").as_bytes(), Some(&["NAME", "BAR"][..])).unwrap();
            }
        } // Drop erases the footer

        let s = screen.0.borrow();
        assert_eq!(
            s.visible(),
            vec!["L2", "L3", "L4", "L5", "", ""],
            "footer gone, log intact"
        );
    }

    #[test]
    fn two_rows_are_drawn_top_down() {
        // Row one names the file, row two is the bar. The newline between them is what puts the
        // second row below the first; the cursor ends on the second row.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["name.iso", "BAR"]).unwrap();
        assert_eq!(buf.bytes(), b"\x1b[?25l\rname.iso\x1b[K\n\rBAR\x1b[K");
    }

    #[test]
    fn erasing_two_rows_walks_back_up_to_the_first() {
        // The cursor sits on the bottom row, so the erase clears it, moves up, clears the row
        // above, and leaves the cursor at column 0 of the first — where log bytes go next.
        // Getting the count wrong here shifts every subsequent write by a line.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["name.iso", "BAR"]).unwrap();
        let mark = buf.bytes().len();
        g.erase().unwrap();
        assert_eq!(&buf.bytes()[mark..], b"\r\x1b[K\x1b[A\r\x1b[K");
    }

    #[test]
    fn erase_walks_back_exactly_as_far_as_it_drew() {
        // A one-row footer must not move the cursor up at all, or it would eat a line of log.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["only"]).unwrap();
        let mark = buf.bytes().len();
        g.erase().unwrap();
        assert_eq!(&buf.bytes()[mark..], b"\r\x1b[K", "no cursor movement for one row");
    }

    #[test]
    fn a_redraw_replaces_both_rows_in_place() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["a", "AAAA"]).unwrap();
        let mark = buf.bytes().len();
        g.draw(&["b", "BB"]).unwrap();
        // Walk up to the first row and overwrite both in place — no blanking in between.
        assert_eq!(&buf.bytes()[mark..], b"\x1b[A\rb\x1b[K\n\rBB\x1b[K");
    }

    #[test]
    fn cursor_hidden_only_once() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["A"]).unwrap();
        let mark = buf.bytes().len();
        g.draw(&["B"]).unwrap();
        // A redraw does not re-emit the hide sequence.
        assert!(!buf.bytes()[mark..].windows(HIDE.len()).any(|w| w == HIDE));
    }

    #[test]
    fn redraw_overwrites_in_place() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["AAAA"]).unwrap();
        let mark = buf.bytes().len();
        g.draw(&["BB"]).unwrap();
        assert_eq!(&buf.bytes()[mark..], b"\rBB\x1b[K");
    }

    #[test]
    fn suspend_restore_shows_cursor_then_redraw_re_hides() {
        // bug2: on SIGTSTP the footer is erased and the cursor shown, and state resets so that
        // after resume the next draw re-hides the cursor and redraws the footer.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["F"]).unwrap(); // hide cursor + footer
        let mark = buf.bytes().len();
        g.suspend_restore().unwrap();
        assert_eq!(&buf.bytes()[mark..], b"\r\x1b[K\x1b[?25h", "erase footer, then show cursor");
        let mark2 = buf.bytes().len();
        g.draw(&["F"]).unwrap(); // resumed
        assert_eq!(&buf.bytes()[mark2..], b"\x1b[?25l\rF\x1b[K", "redraw re-hides + draws");
    }

    #[test]
    fn erase_is_noop_until_shown() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.erase().unwrap();
        assert!(buf.bytes().is_empty(), "nothing shown -> nothing erased");

        g.draw(&["F"]).unwrap();
        let mark = buf.bytes().len();
        g.erase().unwrap();
        assert_eq!(&buf.bytes()[mark..], ERASE);
        let after = buf.bytes().len();
        g.erase().unwrap(); // already erased -> noop
        assert_eq!(buf.bytes().len(), after);
    }

    #[test]
    fn partial_log_line_withholds_the_footer() {
        // #4 / docs/ui.md invariant 11: bytes not ending in a newline leave a partial line on
        // screen. Drawing the footer would `\r` over it and the next erase would wipe it, so the
        // footer is withheld until the line is finished.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["F"]).unwrap();
        let mark = buf.bytes().len();
        g.write_log(b"cp: ", Some(&["F"][..])).unwrap();
        assert_eq!(&buf.bytes()[mark..], b"\r\x1b[Kcp: ", "footer withheld, partial line intact");
        assert!(g.line_pending(), "an unterminated line is on screen");
    }

    #[test]
    fn footer_returns_once_the_line_is_finished() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.write_log(b"cp: ", Some(&["F"][..])).unwrap();
        assert!(g.line_pending());
        let mark = buf.bytes().len();
        g.write_log(b"cannot stat 'x': No such file\n", Some(&["F"][..])).unwrap();
        assert!(!g.line_pending(), "the newline completed the line");
        // No erase first: nothing was drawn over the partial line, so it must survive.
        assert_eq!(&buf.bytes()[mark..], b"cannot stat 'x': No such file\n\x1b[?25l\rF\x1b[K");
    }

    #[test]
    fn split_cp_error_survives_on_screen() {
        // The concrete failure from #4: glibc's error() emits one line as four writes. Every
        // fragment must still be readable afterwards.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["BAR"]).unwrap();
        for frag in [&b"cp: "[..], b"error writing 'dst.bin'", b": No space left on device", b"\n"] {
            g.write_log(frag, Some(&["BAR"][..])).unwrap();
        }
        let out = buf.bytes();
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("cp: error writing 'dst.bin': No space left on device"),
            "the whole error must reach the screen uninterrupted: {text:?}"
        );
    }

    #[test]
    fn tick_redraw_is_suppressed_while_a_line_is_pending() {
        // The render loop's idle tick must consult `line_pending` too, otherwise the periodic
        // redraw would clobber the partial line that write_log deliberately left alone.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.write_log(b"partial", None).unwrap();
        assert!(g.line_pending());
        let mark = buf.bytes().len();
        g.draw_unless_line_pending(&["F"]).unwrap();
        assert!(buf.bytes()[mark..].is_empty(), "no footer drawn over the partial line");
    }

    #[test]
    fn write_log_erases_then_writes_then_redraws() {
        // docs/testing.md C8: log bytes arrive with the footer erased, then it is redrawn.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["F"]).unwrap();
        let mark = buf.bytes().len();
        g.write_log(b"a -> b\n", Some(&["F"][..])).unwrap();
        assert_eq!(&buf.bytes()[mark..], b"\r\x1b[Ka -> b\n\rF\x1b[K");
    }

    #[test]
    fn drained_chunks_are_bracketed_by_one_erase_and_one_draw() {
        // The render loop's actual path: several chunks drained from the queue go out in one
        // pass. Relaying them one at a time would repeat erase -> write -> draw per chunk.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["F"]).unwrap();
        let mark = buf.bytes().len();
        let chunks: [&[u8]; 3] = [b"one\n", b"two\n", b"three\n"];
        g.write_log_chunks(chunks, Some(&["F"][..])).unwrap();
        assert_eq!(
            &buf.bytes()[mark..],
            b"\r\x1b[Kone\ntwo\nthree\n\rF\x1b[K",
            "one erase, the chunks in order, one redraw"
        );
    }

    #[test]
    fn line_pending_follows_the_last_chunk_of_a_batch() {
        // #4's rule applies to the batch as a whole: what matters is whether the *last* byte
        // written ended a line, not whether some earlier chunk did.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        let ends_open: [&[u8]; 2] = [b"done\n", b"cp: "];
        g.write_log_chunks(ends_open, Some(&["F"][..])).unwrap();
        assert!(g.line_pending(), "batch ends mid-line -> footer withheld");
        assert!(!buf.bytes().ends_with(b"F\x1b[K"), "no footer drawn over it");

        let ends_closed: [&[u8]; 2] = [b"cannot stat 'x'", b": No such file\n"];
        g.write_log_chunks(ends_closed, Some(&["F"][..])).unwrap();
        assert!(!g.line_pending(), "the newline in the last chunk completes it");
        assert!(buf.bytes().ends_with(b"\rF\x1b[K"), "footer returns");
    }

    #[test]
    fn an_empty_batch_still_clears_a_stale_footer() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["F"]).unwrap();
        let mark = buf.bytes().len();
        g.write_log_chunks(std::iter::empty(), None).unwrap();
        assert_eq!(&buf.bytes()[mark..], b"\r\x1b[K", "footer erased, nothing written");
    }

    #[test]
    fn write_log_without_footer_just_erases_and_writes() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw(&["F"]).unwrap();
        let mark = buf.bytes().len();
        g.write_log(b"x\n", None).unwrap();
        assert_eq!(&buf.bytes()[mark..], b"\r\x1b[Kx\n");
    }

    #[test]
    fn drop_erases_footer_and_restores_cursor() {
        let buf = SharedBuf::default();
        {
            let mut g = FooterGuard::new(buf.clone());
            g.draw(&["F"]).unwrap();
        }
        // Drop erases the footer, then restores the cursor (show is last).
        let out = buf.bytes();
        assert!(out.ends_with(SHOW), "cursor restored last: {out:?}");
        let before_show = &out[..out.len() - SHOW.len()];
        assert!(before_show.ends_with(ERASE), "footer erased before cursor restore");
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
            g.draw(&["F"]).unwrap();
            panic!("boom");
        }));
        panic::set_hook(prev);
        assert!(r.is_err());
        // Footer erased and cursor restored even while unwinding.
        assert!(buf.bytes().ends_with(SHOW), "cursor restored during unwind");
        assert!(buf.bytes().windows(ERASE.len()).any(|w| w == ERASE), "footer erased");
    }

    #[test]
    fn io_failure_is_returned_and_drop_never_panics() {
        // docs/testing.md C6: render IO failures are best-effort; they must not panic.
        let mut g = FooterGuard::new(FailWriter);
        assert!(g.draw(&["F"]).is_err());
        drop(g); // erase-on-drop over a failing writer must not panic
    }
}
