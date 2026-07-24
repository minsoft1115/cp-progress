#![cfg(feature = "integration")]
//! Terminal-resize (SIGWINCH) integration test (docs/testing.md C1).
//!
//! While cprog is drawing a footer in managed mode, shrink the PTY and deliver SIGWINCH. cprog
//! must re-query the size and redraw the footer to the new (narrower) width. (A real terminal
//! delivers SIGWINCH via the controlling terminal; the test harness sends it directly, which
//! exercises the same handling — flag -> re-query -> relayout.)
//!
//! The copy source is a FIFO fed by a throttled writer thread, so the copy stays alive for as
//! long as we keep feeding: we deterministically observe a wide footer, resize, observe a narrow
//! redraw, then stop feeding so cp hits EOF and exits. This avoids any race with copy speed (a
//! plain large regular file can finish before the post-resize redraw on fast storage).

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nix::pty::{openpty, Winsize};
use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::Pid;

mod common;
use common::read_retry;

struct TmpDir(std::path::PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cprog_rsz_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Strip ANSI CSI sequences so a footer line's visible width can be measured.
fn strip_sgr(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// Visible widths of the footer redraws (each `\r`-delimited segment containing a `%`) in `data`.
fn footer_widths(data: &[u8]) -> Vec<usize> {
    use unicode_width::UnicodeWidthStr;
    data.split(|&b| b == b'\r')
        .map(strip_sgr)
        .filter(|t| t.contains('%'))
        .map(|t| UnicodeWidthStr::width(t.trim_end()))
        .collect()
}

#[test]
fn sigwinch_relayouts_the_footer_to_the_new_width() {
    let tmp = TmpDir::new("winch");
    let fifo = tmp.0.join("src.fifo");
    let dst = tmp.0.join("dst.bin");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).unwrap();
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg(&fifo)
        .arg(&dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI")
        .env("CPROG_SLOW_THRESHOLD_MS", "1")
        .env("CPROG_SAMPLE_INTERVAL_MS", "8")
        .env("CPROG_RENDER_TICK_MS", "8")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    let master_fd = pty.master.as_raw_fd();
    let pgid = Pid::from_raw(child.id() as i32);
    let cprog_pid = Pid::from_raw(child.id() as i32);
    drop(pty.slave);

    // Throttled feeder: keeps the copy alive (dest growing) until told to stop.
    let stop_feed = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (fifo, stop) = (fifo.clone(), Arc::clone(&stop_feed));
        std::thread::spawn(move || {
            // Blocks until cp opens the read end. Rust ignores SIGPIPE, so a closed reader
            // surfaces as a write error rather than killing the test.
            let Ok(mut w) = std::fs::OpenOptions::new().write(true).open(&fifo) else { return };
            let chunk = vec![0u8; 64 * 1024];
            while !stop.load(Ordering::Relaxed) {
                if w.write_all(&chunk).is_err() {
                    return; // cp closed the pipe (finished/killed)
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };

    // Watchdog: never let a stuck relayout hang the suite.
    let hung = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (hung, done) = (Arc::clone(&hung), Arc::clone(&done));
        std::thread::spawn(move || {
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(100));
                if done.load(Ordering::Relaxed) {
                    return;
                }
            }
            hung.store(true, Ordering::Relaxed);
            let _ = kill(cprog_pid, Signal::SIGKILL);
        })
    };

    let track: [u8; 3] = [0xE2, 0x96, 0x91]; // ░ (indeterminate bar, FIFO source has no total)
    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut resize_off: Option<usize> = None;
    let mut narrow_seen = false;
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => {
                out.extend_from_slice(&buf[..n]);
                match resize_off {
                    // Once the wide (80-col) footer is up, shrink to 30 cols and signal cprog.
                    None => {
                        if out.windows(3).any(|w| w == track) {
                            let narrow =
                                Winsize { ws_row: 24, ws_col: 30, ws_xpixel: 0, ws_ypixel: 0 };
                            unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &narrow) };
                            killpg(pgid, Signal::SIGWINCH).unwrap();
                            resize_off = Some(out.len());
                        }
                    }
                    // After the resize, wait to observe a narrow redraw, then stop feeding so
                    // cp reaches EOF and cprog exits — no dependence on copy speed.
                    Some(off) if !narrow_seen => {
                        if footer_widths(&out[off..]).iter().any(|&w| w <= 30) {
                            narrow_seen = true;
                            stop_feed.store(true, Ordering::Relaxed);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    child.wait().unwrap();
    done.store(true, Ordering::Relaxed);
    stop_feed.store(true, Ordering::Relaxed);
    let _ = feeder.join();
    let _ = watchdog.join();

    assert!(!hung.load(Ordering::Relaxed), "cprog hung; relayout never produced a narrow footer");
    assert!(resize_off.is_some(), "footer never appeared, so resize was not exercised");
    let widths = footer_widths(&out);
    assert!(widths.iter().any(|&w| w > 40), "some wide (80-col) footers before resize: {widths:?}");
    // The relayout produced a narrow (<=30) footer *after* the SIGWINCH, proving the re-query.
    assert!(narrow_seen, "footer must re-lay-out to the narrow width after SIGWINCH: {widths:?}");
}
