//! `cp` stdout/stderr capture and sole-writer relay into the log region
//! (docs/capture-and-verbose.md).
//!
//! Reader threads own the captured pipes and forward every byte to the main (sole-writer)
//! thread over a channel; the main thread relays them to the terminal. The stdout reader also
//! feeds line boundaries to the slow-file timer (docs/verbose) — the `-v` content itself is
//! never parsed. Both relays are immediate: bytes are forwarded as read, never held waiting
//! for a newline, so the live scroll stays live (docs/testing.md B9).

use std::io::Read;
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::Instant;

use crate::slowfile::SlowTimer;
use crate::verbose::LinePulse;

/// Read `cp`'s stdout: detect `-v` line boundaries to pulse the slow timer, and relay every
/// byte to the main writer. Stops on EOF or a closed relay channel.
pub(crate) fn relay_stdout(mut reader: impl Read, slow: &Mutex<SlowTimer>, tx: &Sender<Vec<u8>>) {
    let mut pulse = LinePulse::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if pulse.feed(&buf[..n]) > 0 {
                    crate::lock_shared(slow).on_pulse(Instant::now());
                }
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

/// Relay a captured stream verbatim to the main writer (used for `cp`'s stderr).
pub(crate) fn relay_bytes(mut reader: impl Read, tx: &Sender<Vec<u8>>) {
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
        let (tx, rx) = mpsc::channel();
        let slow = Mutex::new(SlowTimer::new(Duration::from_millis(100)));
        relay_stdout(Cursor::new(b"'a' -> ".to_vec()), &slow, &tx);
        drop(tx);
        let relayed: Vec<u8> = rx.into_iter().flatten().collect();
        assert_eq!(relayed, b"'a' -> ", "partial line relayed immediately");
    }

    #[test]
    fn completed_line_pulses_the_slow_timer() {
        let (tx, rx) = mpsc::channel();
        let slow = Mutex::new(SlowTimer::new(Duration::from_millis(100)));
        let t0 = Instant::now();
        relay_stdout(Cursor::new(b"'a' -> 'b'\n".to_vec()), &slow, &tx);
        drop(tx);
        let _: Vec<u8> = rx.into_iter().flatten().collect();
        // The `-v` line registered a pulse, so the item is slow once the threshold elapses.
        assert!(slow.lock().unwrap().is_slow(t0 + Duration::from_millis(150)));
    }

    #[test]
    fn relay_bytes_forwards_verbatim() {
        let (tx, rx) = mpsc::channel();
        relay_bytes(Cursor::new(b"cp: error\n".to_vec()), &tx);
        drop(tx);
        let relayed: Vec<u8> = rx.into_iter().flatten().collect();
        assert_eq!(relayed, b"cp: error\n");
    }
}
