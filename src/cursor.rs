//! Ask the terminal where the cursor is (DSR / CPR), so the footer can sit directly under the
//! log instead of at the bottom of the screen (docs/ui.md invariant 6a).
//!
//! **This was ruled out three times before it was tried.** The reasoning was that the reply to
//! `CSI 6 n` arrives on the terminal's *input*, and stdin belongs to `cp` — so cprog could never
//! read it. The premise is wrong in the one place that matters: managed mode already refuses to
//! run with `-i` (`plan.rs`), and without `-i` **`cp` never reads stdin at all**. During a managed
//! copy nobody is reading the terminal, so opening `/dev/tty` and asking is available.
//!
//! Everything here is best-effort. A terminal that does not answer, a `/dev/tty` that will not
//! open, a `tcsetattr` that fails — each returns `None`, and the caller falls back to pinning the
//! footer to the last two rows, which is what cprog did before this existed.
//!
//! The cost is a window in which the terminal is in raw mode and a crash would leave it there.
//! Measured at **0.1 ms** — the query is one write and one read of a dozen bytes — against a
//! hidden cursor that is left behind for the whole run when cprog is `SIGKILL`ed (exceptions F7).
//! [`Restore`] puts the settings back on every path out, including a panic.

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use rustix::termios::{self, LocalModes, OptionalActions, SpecialCodeIndex, Termios};

/// How long to wait for the reply before giving up and laying out against the bottom of the
/// screen. A terminal answers in microseconds when it answers at all; this bounds the case where
/// it never will.
const REPLY_TIMEOUT: Duration = Duration::from_millis(120);

/// Restores terminal settings when dropped, so no early return or panic can leave the terminal
/// raw. The failure this guards is worse than the one it enables: a raw terminal swallows the
/// user's next keystrokes and shows nothing.
struct Restore<'a> {
    tty: &'a std::fs::File,
    saved: Termios,
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(self.tty.as_fd(), OptionalActions::Now, &self.saved);
    }
}

/// The cursor's 1-based screen row, or `None` if the terminal will not say.
///
/// Deliberately not taking stdout: the *reply* comes back on the terminal's input side, and cprog's
/// stdin may be a redirect even in managed mode. `/dev/tty` is the process's controlling terminal
/// whichever way the standard streams were pointed.
pub fn row() -> Option<u16> {
    let tty = std::fs::File::options().read(true).write(true).open("/dev/tty").ok()?;
    let saved = termios::tcgetattr(tty.as_fd()).ok()?;

    let mut raw = saved.clone();
    // Echo off so the reply is not painted onto the screen, canonical mode off so it can be read
    // before a newline that is never coming.
    raw.local_modes -= LocalModes::ECHO | LocalModes::ICANON;
    raw.special_codes[SpecialCodeIndex::VMIN] = 0;
    raw.special_codes[SpecialCodeIndex::VTIME] = 1; // 100 ms, in case the poll below is skipped
    termios::tcsetattr(tty.as_fd(), OptionalActions::Now, &raw).ok()?;
    let _restore = Restore { tty: &tty, saved };

    (&tty).write_all(b"\x1b[6n").ok()?;
    (&tty).flush().ok()?;
    parse(&read_reply(&tty)?)
}

/// Read until the reply's terminating `R`, or until [`REPLY_TIMEOUT`].
fn read_reply(tty: &std::fs::File) -> Option<Vec<u8>> {
    let deadline = Instant::now() + REPLY_TIMEOUT;
    let mut buf = Vec::new();
    while Instant::now() < deadline {
        let mut chunk = [0u8; 32];
        match (&*tty).read(&mut chunk) {
            Ok(0) => continue, // VTIME expiry with VMIN 0 reads zero bytes, not EOF
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'R') {
                    return Some(buf);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Pull the row out of `ESC [ <row> ; <col> R`.
///
/// Scans for the sequence rather than requiring the buffer to start with it: anything the user
/// typed during the window arrives on the same descriptor and lands in front of the reply. Those
/// bytes are lost either way — that is the honest cost of reading a terminal cprog does not own —
/// but they must not stop the reply from being found.
fn parse(buf: &[u8]) -> Option<u16> {
    let start = buf.windows(2).rposition(|w| w == b"\x1b[")?;
    let rest = &buf[start + 2..];
    let end = rest.iter().position(|b| *b == b'R')?;
    let (row, _) = std::str::from_utf8(&rest[..end]).ok()?.split_once(';')?;
    row.parse().ok().filter(|r| *r > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_yields_its_row() {
        assert_eq!(parse(b"\x1b[4;1R"), Some(4));
        assert_eq!(parse(b"\x1b[24;80R"), Some(24));
        assert_eq!(parse(b"\x1b[1;1R"), Some(1));
    }

    #[test]
    fn typed_bytes_in_front_of_the_reply_do_not_hide_it() {
        // The window is a tenth of a millisecond, but the descriptor is the user's: whatever they
        // pressed arrives first. The reply still has to be found — otherwise a stray keystroke
        // silently costs the layout.
        assert_eq!(parse(b"hello\x1b[7;3R"), Some(7));
        assert_eq!(parse(b"\x03\x1b[9;1R"), Some(9), "even a control byte");
    }

    #[test]
    fn a_reply_that_is_not_one_yields_nothing() {
        // Every one of these has to end in the fallback rather than a wrong row, because a wrong
        // row puts the footer over the user's log.
        assert_eq!(parse(b""), None, "nothing arrived");
        assert_eq!(parse(b"\x1b[4;1"), None, "truncated before the R");
        assert_eq!(parse(b"4;1R"), None, "no CSI");
        assert_eq!(parse(b"\x1b[abc;1R"), None, "not a number");
        assert_eq!(parse(b"\x1b[4R"), None, "no column, so not a CPR");
        assert_eq!(parse(b"\x1b[0;1R"), None, "row 0 is not a screen row");
    }

    #[test]
    fn the_last_sequence_wins() {
        // Two replies can be in the buffer if an earlier query timed out and its answer arrived
        // late. The fresh one is the one at the end.
        assert_eq!(parse(b"\x1b[2;1R\x1b[9;1R"), Some(9));
    }
}
