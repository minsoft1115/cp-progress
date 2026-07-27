#![cfg(feature = "integration")]
//! Signal-preservation integration test over a PTY (docs/testing.md D3).
//!
//! While cprog is drawing a footer in managed mode, deliver SIGINT to its process group (as
//! Ctrl-C would). cprog must clear the footer and re-raise the signal so it exits *signaled* —
//! not with a plain `128+n` code — mirroring cp's fate.

use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use nix::pty::{openpty, Winsize};
use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::Pid;

mod common;
use common::{read_retry, rfind, TmpDir};

#[test]
fn sigint_during_managed_copy_cleans_footer_and_preserves_signal() {
    let tmp = TmpDir::new("int");
    let src = tmp.0.join("src.bin");
    let dst = tmp.0.join("dst.bin");
    std::fs::write(&src, vec![0u8; 256 * 1024 * 1024]).unwrap();

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).unwrap();
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg(&src)
        .arg(&dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI")
        .env("CPROG_SLOW_THRESHOLD_MS", "1")
        .env("CPROG_SAMPLE_INTERVAL_MS", "5")
        .env("CPROG_RENDER_TICK_MS", "5")
        .process_group(0) // own group so a group signal hits both cprog and its cp child
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);
    let pgid = Pid::from_raw(child.id() as i32);

    let bar: [u8; 3] = [0xE2, 0x96, 0x88]; // █
    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut signaled = false;
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => {
                out.extend_from_slice(&buf[..n]);
                // Once the footer is visibly up, interrupt the whole group.
                if !signaled && out.windows(3).any(|w| w == bar) {
                    killpg(pgid, Signal::SIGINT).unwrap();
                    signaled = true;
                }
            }
        }
    }
    let status = child.wait().unwrap();

    assert!(signaled, "footer never appeared, so the signal path was not exercised");
    // True signaled exit (not a plain 128+n code).
    assert_eq!(status.signal(), Some(Signal::SIGINT as i32), "cprog should exit signaled");
    // Footer cleared on exit: after the last bar draw there must be a *bare* erase — `\r\x1b[K`
    // with nothing between the carriage return and the erase-to-EOL. That is what the teardown
    // `erase()`/Drop emits; an ordinary redraw ends `...footer-text\x1b[K`, so a plain
    // `last_erase > last_bar` would pass even if the teardown clear were removed. This does not.
    let last_bar = rfind(&out, &bar).expect("saw a bar");
    let tail = &out[last_bar + bar.len()..];
    assert!(
        tail.windows(4).any(|w| w == b"\r\x1b[K"),
        "footer must be cleared by a bare erase after the last bar on exit: tail={:?}",
        String::from_utf8_lossy(tail)
    );
}

#[test]
fn sigterm_to_cprog_alone_terminates_without_hanging() {
    // A signal delivered to cprog ALONE (not the whole group) leaves cp running. To make cp
    // *deterministically* still-running (no copy-speed race), copy from a FIFO with no data:
    // cp blocks forever on read, the reader threads block relaying its (empty) stdout, and
    // without the teardown fix `join()` would deadlock. cprog must terminate cp and exit.
    let tmp = TmpDir::new("term");
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
        .env("CPROG_SAMPLE_INTERVAL_MS", "5")
        .env("CPROG_RENDER_TICK_MS", "5")
        .process_group(0) // own group so kill(pid) hits cprog alone, not cp
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);
    let cprog_pid = Pid::from_raw(child.id() as i32);

    // Watchdog: if cprog deadlocks in join (the bug), force-kill after a grace period and flag it.
    let hung = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (hung, done) = (Arc::clone(&hung), Arc::clone(&done));
        std::thread::spawn(move || {
            for _ in 0..80 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if done.load(Ordering::Relaxed) {
                    return;
                }
            }
            hung.store(true, Ordering::Relaxed);
            let _ = kill(cprog_pid, Signal::SIGKILL);
        })
    };

    // Open the write end so cp's open(FIFO) unblocks; send no data so cp blocks on read.
    let _writer = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();

    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut sent = false;
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => {
                out.extend_from_slice(&buf[..n]);
                // The -v line means cp has started (and is now blocked reading the FIFO).
                if !sent && out.windows(2).any(|w| w == b"->") {
                    kill(cprog_pid, Signal::SIGTERM).unwrap(); // cprog only; cp keeps blocking
                    sent = true;
                }
            }
        }
    }
    let status = child.wait().unwrap();
    done.store(true, Ordering::Relaxed);
    let _ = watchdog.join();

    assert!(sent, "cp never started, so the teardown path was not exercised");
    assert!(!hung.load(Ordering::Relaxed), "cprog hung after SIGTERM (reader join deadlock)");
    // cprog forwards the termination to cp and re-raises, exiting signaled.
    assert!(status.signal().is_some(), "cprog should exit signaled, got {status:?}");
}

#[test]
fn signal_to_cprog_alone_is_forwarded_to_cp_and_re_raised() {
    // A signal delivered to cprog ALONE must be forwarded to cp *as itself* (not normalized to
    // SIGTERM): cp then dies of that signal and cprog re-raises the same one, so the parent shell
    // sees the exact signal the operator sent. We use SIGINT (distinct from the old hardcoded
    // SIGTERM) so a normalization regression is caught: it would report SIGTERM here.
    let tmp = TmpDir::new("fwd");
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
        .env("CPROG_SAMPLE_INTERVAL_MS", "5")
        .env("CPROG_RENDER_TICK_MS", "5")
        .process_group(0) // own group so kill(pid) hits cprog alone, not cp
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);
    let cprog_pid = Pid::from_raw(child.id() as i32);

    // Watchdog: never let a teardown deadlock hang the suite.
    let hung = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (hung, done) = (Arc::clone(&hung), Arc::clone(&done));
        std::thread::spawn(move || {
            for _ in 0..80 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if done.load(Ordering::Relaxed) {
                    return;
                }
            }
            hung.store(true, Ordering::Relaxed);
            let _ = kill(cprog_pid, Signal::SIGKILL);
        })
    };

    let _writer = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();

    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut sent = false;
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => {
                out.extend_from_slice(&buf[..n]);
                if !sent && out.windows(2).any(|w| w == b"->") {
                    kill(cprog_pid, Signal::SIGINT).unwrap(); // cprog only; cp keeps blocking
                    sent = true;
                }
            }
        }
    }
    let status = child.wait().unwrap();
    done.store(true, Ordering::Relaxed);
    let _ = watchdog.join();

    assert!(sent, "cp never started, so the teardown path was not exercised");
    assert!(!hung.load(Ordering::Relaxed), "cprog hung after SIGINT (reader join deadlock)");
    // The re-raised signal must be the *same* one cprog received (SIGINT), not a normalized SIGTERM.
    assert_eq!(
        status.signal(),
        Some(Signal::SIGINT as i32),
        "cprog-alone signal must be forwarded to cp and re-raised as itself, got {status:?}"
    );
}
