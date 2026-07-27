//! Terminal writer, cursor/erase sequences, and `FooterGuard` (RAII screen restore)
//! (docs/ui.md, docs/architecture.md).
//!
//! cprog is the terminal's sole writer, so it keeps the single-line footer and the scrolling
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

/// Owns the terminal writer and manages the single-line footer, erasing it on drop.
pub struct FooterGuard<W: Write> {
    w: W,
    /// Whether a footer is currently on screen (and thus must be erased before log/teardown).
    shown: bool,
    /// Whether we have hidden the cursor (and thus must restore it on teardown).
    cursor_hidden: bool,
    /// Whether the last log bytes written left an unterminated line on screen (docs/ui.md
    /// invariant 10). While true the footer is withheld — see [`Self::line_pending`].
    line_pending: bool,
}

impl<W: Write> FooterGuard<W> {
    /// Wrap a writer with no footer shown yet.
    pub fn new(w: W) -> Self {
        Self { w, shown: false, cursor_hidden: false, line_pending: false }
    }

    /// Whether an unterminated log line is currently on screen.
    ///
    /// The footer draws from column 0 (`\r`), and the next erase clears the whole line, so
    /// drawing it over a partial line makes that log text vanish. `cp` hits this routinely:
    /// glibc's `error()` emits one error line as four separate writes, and the relay forwards
    /// each fragment as it arrives (it must not wait for a newline). This is tracked on the
    /// bytes actually written to the terminal, because stdout and stderr merge on one screen.
    pub fn line_pending(&self) -> bool {
        self.line_pending
    }

    /// Draw (or redraw in place) the footer line. Overwrites any current footer from column 0
    /// and clears trailing columns, so shrinking footers leave no residue. On the first draw
    /// the cursor is hidden (restored on drop) so it does not blink at the end of the bar.
    pub fn draw(&mut self, text: &str) -> io::Result<()> {
        if !self.cursor_hidden {
            // Mark hidden before writing so a partial write is still restored on drop.
            self.cursor_hidden = true;
            self.w.write_all(HIDE_CURSOR)?;
        }
        // Mark shown before writing so a partial write is still cleaned up on drop.
        self.shown = true;
        self.w.write_all(CR)?;
        self.w.write_all(text.as_bytes())?;
        self.w.write_all(ERASE_EOL)?;
        self.w.flush()
    }

    /// Erase the footer if one is shown; a no-op otherwise.
    pub fn erase(&mut self) -> io::Result<()> {
        if !self.shown {
            return Ok(());
        }
        self.shown = false;
        self.w.write_all(CR)?;
        self.w.write_all(ERASE_EOL)?;
        self.w.flush()
    }

    /// Restore the terminal for a job-control suspend (`SIGTSTP` / Ctrl-Z): erase the footer and
    /// show the cursor, and reset our state to "fresh" so that after the process is resumed the
    /// next [`draw`](Self::draw) re-hides the cursor and redraws the footer. Unlike `Drop`, the
    /// guard keeps living across the stop.
    pub fn suspend_restore(&mut self) -> io::Result<()> {
        self.erase()?; // footer gone, `shown` back to false
        if self.cursor_hidden {
            self.cursor_hidden = false;
            self.w.write_all(SHOW_CURSOR)?;
        }
        self.w.flush()
    }

    /// Draw the footer unless an unterminated log line is on screen (docs/ui.md invariant 10).
    /// This is what the render loop's idle tick should call; [`Self::draw`] stays the primitive.
    pub fn draw_unless_line_pending(&mut self, text: &str) -> io::Result<()> {
        if self.line_pending {
            return Ok(());
        }
        self.draw(text)
    }

    /// Relay log bytes through the sole writer: erase the footer, write the bytes, then redraw
    /// the footer if one should remain (docs/testing.md C8).
    ///
    /// If the bytes do not end on a line boundary the footer is withheld, because drawing it
    /// would overwrite the partial line and the next erase would take it off screen entirely
    /// (docs/ui.md invariant 10). It comes back with the write that completes the line.
    pub fn write_log(&mut self, bytes: &[u8], footer: Option<&str>) -> io::Result<()> {
        self.erase()?;
        self.w.write_all(bytes)?;
        if let Some(&last) = bytes.last() {
            self.line_pending = last != b'\n';
        }
        match footer {
            Some(text) if !self.line_pending => self.draw(text),
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

    #[test]
    fn draw_emits_cr_text_erase_to_eol() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw("hi").unwrap();
        // First draw hides the cursor, then draws the footer.
        assert_eq!(buf.bytes(), b"\x1b[?25l\rhi\x1b[K");
    }

    #[test]
    fn cursor_hidden_only_once() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw("A").unwrap();
        let mark = buf.bytes().len();
        g.draw("B").unwrap();
        // A redraw does not re-emit the hide sequence.
        assert!(!buf.bytes()[mark..].windows(HIDE.len()).any(|w| w == HIDE));
    }

    #[test]
    fn redraw_overwrites_in_place() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw("AAAA").unwrap();
        let mark = buf.bytes().len();
        g.draw("BB").unwrap();
        assert_eq!(&buf.bytes()[mark..], b"\rBB\x1b[K");
    }

    #[test]
    fn suspend_restore_shows_cursor_then_redraw_re_hides() {
        // bug2: on SIGTSTP the footer is erased and the cursor shown, and state resets so that
        // after resume the next draw re-hides the cursor and redraws the footer.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw("F").unwrap(); // hide cursor + footer
        let mark = buf.bytes().len();
        g.suspend_restore().unwrap();
        assert_eq!(&buf.bytes()[mark..], b"\r\x1b[K\x1b[?25h", "erase footer, then show cursor");
        let mark2 = buf.bytes().len();
        g.draw("F").unwrap(); // resumed
        assert_eq!(&buf.bytes()[mark2..], b"\x1b[?25l\rF\x1b[K", "redraw re-hides + draws");
    }

    #[test]
    fn erase_is_noop_until_shown() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.erase().unwrap();
        assert!(buf.bytes().is_empty(), "nothing shown -> nothing erased");

        g.draw("F").unwrap();
        let mark = buf.bytes().len();
        g.erase().unwrap();
        assert_eq!(&buf.bytes()[mark..], ERASE);
        let after = buf.bytes().len();
        g.erase().unwrap(); // already erased -> noop
        assert_eq!(buf.bytes().len(), after);
    }

    #[test]
    fn partial_log_line_withholds_the_footer() {
        // #4 / docs/ui.md invariant 10: bytes not ending in a newline leave a partial line on
        // screen. Drawing the footer would `\r` over it and the next erase would wipe it, so the
        // footer is withheld until the line is finished.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw("F").unwrap();
        let mark = buf.bytes().len();
        g.write_log(b"cp: ", Some("F")).unwrap();
        assert_eq!(&buf.bytes()[mark..], b"\r\x1b[Kcp: ", "footer withheld, partial line intact");
        assert!(g.line_pending(), "an unterminated line is on screen");
    }

    #[test]
    fn footer_returns_once_the_line_is_finished() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.write_log(b"cp: ", Some("F")).unwrap();
        assert!(g.line_pending());
        let mark = buf.bytes().len();
        g.write_log(b"cannot stat 'x': No such file\n", Some("F")).unwrap();
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
        g.draw("BAR").unwrap();
        for frag in [&b"cp: "[..], b"error writing 'dst.bin'", b": No space left on device", b"\n"] {
            g.write_log(frag, Some("BAR")).unwrap();
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
        g.draw_unless_line_pending("F").unwrap();
        assert!(buf.bytes()[mark..].is_empty(), "no footer drawn over the partial line");
    }

    #[test]
    fn write_log_erases_then_writes_then_redraws() {
        // docs/testing.md C8: log bytes arrive with the footer erased, then it is redrawn.
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw("F").unwrap();
        let mark = buf.bytes().len();
        g.write_log(b"a -> b\n", Some("F")).unwrap();
        assert_eq!(&buf.bytes()[mark..], b"\r\x1b[Ka -> b\n\rF\x1b[K");
    }

    #[test]
    fn write_log_without_footer_just_erases_and_writes() {
        let buf = SharedBuf::default();
        let mut g = FooterGuard::new(buf.clone());
        g.draw("F").unwrap();
        let mark = buf.bytes().len();
        g.write_log(b"x\n", None).unwrap();
        assert_eq!(&buf.bytes()[mark..], b"\r\x1b[Kx\n");
    }

    #[test]
    fn drop_erases_footer_and_restores_cursor() {
        let buf = SharedBuf::default();
        {
            let mut g = FooterGuard::new(buf.clone());
            g.draw("F").unwrap();
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
            g.draw("F").unwrap();
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
        assert!(g.draw("F").is_err());
        drop(g); // erase-on-drop over a failing writer must not panic
    }
}
