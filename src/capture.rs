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

use std::io::Read;
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

    #[test]
    fn relay_bytes_forwards_verbatim() {
        let (tx, rx) = mpsc::sync_channel(16);
        relay_bytes(Cursor::new(b"cp: error\n".to_vec()), &tx);
        drop(tx);
        let relayed: Vec<u8> = rx.into_iter().flatten().collect();
        assert_eq!(relayed, b"cp: error\n");
    }
}
