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
/// The pulse fires only for reads that *completed* a line: one completed line is one new item.
/// A read landing mid-line is not an item, and counting it as one would start that item's clock
/// at a fragment rather than at the line naming the file (exceptions D3).
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
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    #[test]
    fn relays_partial_line_without_waiting_for_newline() {
        // docs/testing.md B9: bytes with no terminating newline are forwarded at once, not held
        // back waiting for a line boundary.
        //
        // "At once" has to be observed *before* the reader reaches EOF. Draining the channel
        // afterwards cannot tell an immediate relay from one that buffered the line and flushed
        // it on the way out — a hold-until-newline implementation passes that check (#61). So the
        // reader blocks after the partial line and the main thread reads the channel while it is
        // still parked there.
        let (tx, rx) = mpsc::sync_channel(16);
        let released = Arc::new(Barrier::new(2));

        struct PartialThenPark(Option<&'static [u8]>, Arc<Barrier>);
        impl Read for PartialThenPark {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                match self.0.take() {
                    Some(chunk) => {
                        buf[..chunk.len()].copy_from_slice(chunk);
                        Ok(chunk.len())
                    }
                    // Parked mid-stream: cp has said something and has not finished saying it.
                    None => {
                        self.1.wait();
                        Ok(0)
                    }
                }
            }
        }

        let reader = PartialThenPark(Some(b"'a' -> "), Arc::clone(&released));
        let relay = std::thread::spawn(move || {
            let slow = Mutex::new(SlowTimer::new(Duration::from_millis(100)));
            relay_stdout(reader, &slow, &tx, true);
        });

        let first = rx.recv_timeout(Duration::from_secs(2)).expect("partial line relayed at once");
        assert_eq!(first, b"'a' -> ", "and relayed verbatim");
        released.wait(); // let the reader finish
        relay.join().unwrap();
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
        // The other half of exceptions D3: a pulse means "a line completed", so a read that
        // completed none must leave the timer alone. Both docs/testing.md B3 (a `-v` line split
        // across chunks) and B7 (the bar switching at a file boundary) rest on that meaning —
        // pulsing on every read instead would start an item's clock at whichever fragment
        // happened to arrive rather than at the line that names it.
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
        // The reader is endless on purpose. With a finite payload the loop also ends at EOF, so
        // the test would pass with the send-failure `break` deleted — it would only be showing
        // that `SyncSender` unblocks when the receiver goes away, which is std's behaviour, not
        // cprog's rule (#59).
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let relay = std::thread::spawn(move || relay_bytes(std::io::repeat(b'x'), &tx));
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

    #[test]
    fn dropping_the_receiver_releases_a_blocked_stdout_relay() {
        // The same rule for the other reader. `relay_stdout` is the one that runs with `relay`
        // true whenever the user passed `-v`, and its send-failure `break` was pinned by nothing:
        // deleting it left the whole suite green, because every other test in this file feeds it
        // a finite reader that ends at EOF regardless (#59).
        //
        // Under that deletion this test does not fail, it *hangs* — the loop reads an endless
        // reader forever with nowhere to put the bytes. That is how a non-terminating rule shows
        // up, and how `cargo-mutants` scores it (a timeout is a detection).
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let slow = Mutex::new(SlowTimer::new(Duration::from_millis(100)));
        let relay = std::thread::spawn(move || {
            relay_stdout(std::io::repeat(b'x'), &slow, &tx, true);
        });
        std::thread::sleep(Duration::from_millis(30));
        assert!(!relay.is_finished(), "blocked on the full queue");

        drop(rx);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !relay.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(relay.is_finished(), "dropping the receiver must end the stdout relay too");
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
    fn a_real_read_error_ends_the_stdout_relay_too() {
        // The twin of `a_real_read_error_still_ends_the_relay`, for the *other* reader. D8 is one
        // rule over two functions, and only `relay_bytes` had both halves pinned: turning
        // `relay_stdout`'s guard into "retry every error" left the whole suite green, so nothing
        // said that a permanently failing stdout ends the loop rather than spinning the thread
        // forever — with `cp` never reaped and the footer never cleaned up.
        //
        // Verified by mutation the way a non-terminating rule has to be: with the guard widened,
        // this test does not fail, it *hangs* (which is how cargo-mutants scores it — a timeout
        // is a detection). Unmutated it returns immediately.
        struct AlwaysBroken;
        impl std::io::Read for AlwaysBroken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("fd is gone"))
            }
        }
        let (tx, rx) = mpsc::sync_channel(16);
        let slow = Mutex::new(SlowTimer::new(Duration::from_millis(100)));
        relay_stdout(AlwaysBroken, &slow, &tx, true);
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
