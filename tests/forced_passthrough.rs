#![cfg(feature = "integration")]
//! `CPROG_PASSTHROUGH` integration tests (docs/usage.md "강제 passthrough", exceptions B16).
//!
//! Setting the variable (any value — the empty string included) must defeat an otherwise fully
//! managed-eligible environment: a PTY, a slow threshold, brisk timings. Nothing of cprog may
//! reach the screen — no cursor-hide, no footer, no summary — and for `--help`/`--version` the
//! one line cprog otherwise appends (#15) stays silent too: forced passthrough means cprog adds
//! nothing at all.

use std::fs::File;
use std::os::fd::OwnedFd;
use std::process::{Command, Stdio};

use nix::pty::Winsize;

mod common;
use common::{contains, openpty_cloexec, read_retry, Watchdog};
use common::TmpDir;

/// Read a PTY master to EOF.
///
/// EOF is the only exit, and it arrives only because cprog exited — so every caller arms a
/// [`Watchdog`] first. Without one, a cprog that never exits blocks the test forever instead of
/// failing it, and a hang is the one failure a suite cannot report (CI shows a timeout with no
/// test named). Sleeping before `cmd.exec()` in the passthrough path left both tests in this file
/// running forever; with the watchdog they go red (#69).
fn drain_all(master: &mut File) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match read_retry(master, &mut buf) {
            0 => break,
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    out
}

#[test]
fn forced_passthrough_disables_the_tui_even_on_a_pty() {
    let tmp = TmpDir::new("forced");
    let src = tmp.0.join("src.bin");
    let dst = tmp.0.join("dst.bin");
    std::fs::write(&src, vec![0u8; 64 * 1024 * 1024]).unwrap();

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty_cloexec(Some(&ws));
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg(&src)
        .arg(&dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI")
        .env("CPROG_PASSTHROUGH", "1")
        // Timings that reliably draw a footer in managed mode (see tests/managed.rs) — their
        // absence here is what proves the forced fallback.
        .env("CPROG_SLOW_THRESHOLD_MS", "1")
        .env("CPROG_SAMPLE_INTERVAL_MS", "5")
        .env("CPROG_RENDER_TICK_MS", "5")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);
    let dog = Watchdog::arm(child.id() as i32, 30); // see `drain_all` (#69)

    let mut master = File::from(pty.master);
    let out = drain_all(&mut master);
    let status = child.wait().unwrap();
    dog.disarm();
    assert!(!dog.hung(), "cprog never exited, so the master never saw EOF");

    assert!(status.success(), "the copy still succeeds");
    assert_eq!(std::fs::read(&dst).unwrap().len(), 64 * 1024 * 1024);
    assert!(!out.windows(3).any(|w| w == [0xE2, 0x96, 0x88]), "no footer bar");
    assert!(!out.windows(3).any(|w| w == [0xE2, 0x9C, 0x93]), "no summary");
    assert!(!contains(&out, b"\x1b[?25l"), "no cursor hide — no TUI at all");
}

#[test]
fn forced_passthrough_silences_the_version_notice() {
    // Without the variable, `--version` on a terminal appends one line naming cprog (#15,
    // tests/managed.rs::help_over_pty_passes_through_but_names_cprog). With it — even set to
    // the empty string, the value-agnostic rule — the screen shows only what cp printed.
    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty_cloexec(Some(&ws));
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg("--version")
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI")
        .env("CPROG_PASSTHROUGH", "") // empty still counts, like CI= (exceptions B6/B16)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);
    let dog = Watchdog::arm(child.id() as i32, 30); // see `drain_all` (#69)

    let mut master = File::from(pty.master);
    let out = drain_all(&mut master);
    let status = child.wait().unwrap();
    dog.disarm();
    assert!(!dog.hung(), "cprog never exited, so the master never saw EOF");
    let text = String::from_utf8_lossy(&out);

    assert_eq!(status.code(), Some(0));
    assert!(text.contains("cp ("), "cp's own version output was shown: {:.80?}", text);
    assert!(!text.contains("cprog "), "no cprog line under forced passthrough: {text:?}");
}
