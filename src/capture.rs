//! `cp` stdout/stderr capture and sole-writer relay into the log region
//! (docs/capture-and-verbose.md).
//!
//! Reader threads own the captured pipes and forward every byte to the main (sole-writer)
//! thread over a bounded channel; the main thread relays them to the terminal. The bound is what
//! keeps the pipe's backpressure intact — with an unbounded queue a terminal slower than `cp`'s
//! output would just accumulate un-rendered log bytes in memory (docs/architecture.md "동시성"). The stdout reader also
//! feeds line boundaries to the slow-file timer ([`crate::verbose`]) — the `-v` content itself is
//! never parsed. Both relays are immediate: bytes are forwarded as read, never held waiting
//! for a newline, so the live scroll stays live (docs/testing.md B9).

use std::io::{self, Read};
use std::sync::mpsc::SyncSender;
use std::sync::Mutex;
use std::time::Instant;

use crate::slowfile::SlowTimer;
use crate::verbose::LinePulse;

/// Read `cp`'s stdout: detect `-v` line boundaries to pulse the slow timer, and — only when the
/// user actually asked for `-v` — relay the bytes to the main writer. Stops on EOF or a closed
/// relay channel.
///
/// `-v` is injected either way, because the slow-file timer has nothing else to go on. But the
/// bytes only reach the terminal when they were requested: cprog otherwise floods the scrollback
/// with output the user never asked for, which is the one place it visibly differs from plain
/// `cp` (docs/capture-and-verbose.md). With `relay` false the loop counts newlines and drops the
/// rest, which also removes the per-file channel round-trip, wake-up and allocation (#18).
pub(crate) fn relay_stdout(
    mut reader: impl Read,
    slow: &Mutex<SlowTimer>,
    tx: &SyncSender<Vec<u8>>,
    relay: bool,
) {
    let mut pulse = LinePulse::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            // A signal-interrupted read is not the end of the stream: retry it. Folding EINTR
            // into the EOF arm would end the relay early and silently drop whatever `cp` had
            // left to say (exceptions D8). Any other error still ends the loop, or a
            // permanently failing fd would spin this thread forever.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if pulse.feed(&buf[..n]) > 0 {
                    crate::lock_shared(slow).on_pulse(Instant::now());
                }
                if relay && tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

/// Relay a captured stream verbatim to the main writer (used for `cp`'s stderr).
pub(crate) fn relay_bytes(mut reader: impl Read, tx: &SyncSender<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            // Same rule as relay_stdout: EINTR is a retry, not an EOF (exceptions D8). This is
            // the stream carrying cp's diagnostics, so truncating it is the worse of the two.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn relays_partial_line_without_waiting_for_newline() {
        // docs/testing.md B9: bytes with no terminating newline are forwarded at once, not
        // held back waiting for a line boundary.
        let (tx, rx) = mpsc::sync_channel(16);
        let slow = Mutex::new(SlowTimer::new(Duration::from_millis(100)));
        relay_stdout(Cursor::new(b"'a' -> ".to_vec()), &slow, &tx, true);
        drop(tx);
        let relayed: Vec<u8> = rx.into_iter().flatten().collect();
        assert_eq!(relayed, b"'a' -> ", "partial line relayed immediately");
    }

    #[test]
    fn completed_line_pulses_the_slow_timer() {
        let (tx, rx) = mpsc::sync_channel(16);
        let slow = Mutex::new(SlowTimer::new(Duration::from_millis(100)));
        let t0 = Instant::now();
        relay_stdout(Cursor::new(b"'a' -> 'b'\n".to_vec()), &slow, &tx, true);
        drop(tx);
        let _: Vec<u8> = rx.into_iter().flatten().collect();
        // The `-v` line registered a pulse, so the item is slow once the threshold elapses.
        assert!(slow.lock().unwrap().is_slow(t0 + Duration::from_millis(150)));
    }

    #[test]
    fn a_chunk_without_a_line_boundary_does_not_pulse() {
        // The other half of D3, and the one the pulse count depends on: a read that completed no
        // line must leave the timer alone. Pulsing on every read instead would restart the
        // slow-file clock mid-line (B3, a `-v` line split across chunks) and again at a file
        // boundary (B7) — the bar would keep resetting on the very files it exists for.
        let (tx, rx) = mpsc::sync_channel(16);
        let slow = Mutex::new(SlowTimer::new(Duration::from_millis(100)));
        let t0 = Instant::now();
        relay_stdout(Cursor::new(b"'a' -> ".to_vec()), &slow, &tx, true);
        drop(tx);
        let _: Vec<u8> = rx.into_iter().flatten().collect();
        assert!(
            !slow.lock().unwrap().is_slow(t0 + Duration::from_secs(10)),
            "no line completed -> no pulse -> nothing has started being timed"
        );
    }

    #[test]
    fn without_verbose_the_bytes_are_dropped_but_the_timer_still_pulses() {
        // #20: `-v` is injected for timing whether or not the user asked for it, so the pulse
        // must survive; the bytes must not, or cprog floods a scrollback nobody asked to fill.
        let (tx, rx) = mpsc::sync_channel(16);
        let slow = Mutex::new(SlowTimer::new(Duration::from_millis(100)));
        let t0 = Instant::now();
        relay_stdout(Cursor::new(b"'a' -> 'b'\n".to_vec()), &slow, &tx, false);
        drop(tx);
        assert!(rx.into_iter().next().is_none(), "nothing relayed");
        assert!(slow.lock().unwrap().is_slow(t0 + Duration::from_millis(150)), "still timed");
    }

    #[test]
    fn a_full_queue_makes_the_relay_wait_rather_than_buffer() {
        // #8: the channel is bounded so the pipe's backpressure survives. With an unbounded queue
        // this relay would race to the end regardless of whether anyone is rendering the bytes,
        // turning a slow terminal into unbounded memory growth.
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(2);
        let payload = vec![b'x'; 8192 * 6]; // six reads, far more than the queue holds
        let relay = std::thread::spawn(move || relay_bytes(Cursor::new(payload), &tx));

        std::thread::sleep(Duration::from_millis(30));
        assert!(!relay.is_finished(), "the relay must be waiting on a full queue, not buffering");

        let mut received = 0usize;
        while let Ok(chunk) = rx.recv_timeout(Duration::from_secs(2)) {
            received += chunk.len();
        }
        relay.join().unwrap();
        assert_eq!(received, 8192 * 6, "every byte still arrives once the queue drains");
    }

    #[test]
    fn dropping_the_receiver_releases_a_blocked_relay() {
        // Teardown relies on this: after the render loop the receiver is dropped, and a reader
        // parked in `send` on the bounded queue must fail and return so its join is bounded.
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let payload = vec![b'x'; 8192 * 8];
        let relay = std::thread::spawn(move || relay_bytes(Cursor::new(payload), &tx));
        std::thread::sleep(Duration::from_millis(30));
        assert!(!relay.is_finished(), "blocked on the full queue");

        drop(rx);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !relay.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(relay.is_finished(), "dropping the receiver must end the relay");
        relay.join().unwrap();
    }

    /// A reader that hands out `Interrupted` between chunks, the way a signal-interrupted
    /// `read(2)` surfaces when the handler was installed without `SA_RESTART`.
    struct InterruptingReader {
        chunks: Vec<&'static [u8]>,
        next: usize,
        interrupt_before_next: bool,
    }
    impl InterruptingReader {
        fn new(chunks: Vec<&'static [u8]>) -> Self {
            Self { chunks, next: 0, interrupt_before_next: true }
        }
    }
    impl std::io::Read for InterruptingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.interrupt_before_next {
                self.interrupt_before_next = false;
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            let Some(chunk) = self.chunks.get(self.next) else { return Ok(0) };
            self.next += 1;
            self.interrupt_before_next = true; // interrupt again before the following chunk
            buf[..chunk.len()].copy_from_slice(chunk);
            Ok(chunk.len())
        }
    }

    #[test]
    fn an_interrupted_read_does_not_end_the_relay() {
        // #32 / exceptions D8. EINTR is not the end of the stream. Folding it into the EOF arm
        // silently drops whatever cp had left to say — the same class of loss #4 fixed on the
        // render side, but with no trace at all.
        let (tx, rx) = mpsc::sync_channel(16);
        let slow = Mutex::new(SlowTimer::new(Duration::from_millis(100)));
        let reader = InterruptingReader::new(vec![b"'a' -> 'b'\n", b"cp: error\n"]);
        relay_stdout(reader, &slow, &tx, true);
        drop(tx);
        let relayed: Vec<u8> = rx.into_iter().flatten().collect();
        assert_eq!(relayed, b"'a' -> 'b'\ncp: error\n", "both chunks survive the interruptions");
    }

    #[test]
    fn an_interrupted_read_does_not_end_the_stderr_relay() {
        // stderr carries cp's diagnostics, so truncating it there is the worse of the two.
        let (tx, rx) = mpsc::sync_channel(16);
        let reader = InterruptingReader::new(vec![b"cp: cannot stat 'x'", b": No such file\n"]);
        relay_bytes(reader, &tx);
        drop(tx);
        let relayed: Vec<u8> = rx.into_iter().flatten().collect();
        assert_eq!(relayed, b"cp: cannot stat 'x': No such file\n");
    }

    #[test]
    fn a_real_read_error_still_ends_the_relay() {
        // Only Interrupted is retried; anything else must still terminate, or a permanently
        // failing fd would spin the reader thread forever.
        struct AlwaysBroken;
        impl std::io::Read for AlwaysBroken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("fd is gone"))
            }
        }
        let (tx, rx) = mpsc::sync_channel(16);
        relay_bytes(AlwaysBroken, &tx);
        drop(tx);
        assert!(rx.into_iter().next().is_none(), "returns instead of looping");
    }

    #[test]
    fn relay_bytes_forwards_verbatim() {
        let (tx, rx) = mpsc::sync_channel(16);
        relay_bytes(Cursor::new(b"cp: error\n".to_vec()), &tx);
        drop(tx);
        let relayed: Vec<u8> = rx.into_iter().flatten().collect();
        assert_eq!(relayed, b"cp: error\n");
    }
}
