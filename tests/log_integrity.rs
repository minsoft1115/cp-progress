#![cfg(feature = "integration")]
//! Log-integrity integration test (#4, docs/ui.md invariant 11).
//!
//! `cp` writes one error line as several `write(2)` calls (glibc's `error()` emits the `"cp: "`
//! prefix, the message, the reason and the newline separately). The relay forwards each fragment
//! the moment it arrives — it must not wait for a newline — so a footer drawn between fragments
//! would `\r` over the partial line and the next erase would take it off screen. Only the
//! fragment carrying the newline would survive, leaving the user with something like
//! `: File too large`: a reason with no file name and no operation.
//!
//! The window is specific: the error has to arrive **while a footer is up**, i.e. during a copy,
//! with no `-v` line in between (an instant failure never engages the footer, which is why
//! `tests/managed.rs` does not cover this). Here a FIFO source keeps one copy slow so the footer
//! is up, and `RLIMIT_FSIZE` makes the write fail mid-copy — the same shape as ENOSPC.
//!
//! Note on scope: whether the kernel actually hands those four writes to the relay as separate
//! reads is a scheduling matter, so this test cannot *force* the fragmented case. It asserts the
//! user-visible invariant end to end; the deterministic coverage of the fragmentation rule itself
//! lives in the `render` unit tests (`split_cp_error_survives_on_screen`).

mod common;

use common::{render_screen, Feeder, TmpDir};
use nix::pty::openpty;
use nix::sys::termios::Termios;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::process::{Command, Stdio};

/// Bytes the destination is allowed to reach before writes start failing.
const FILE_LIMIT: u64 = 1024 * 1024;

fn winsize(rows: u16, cols: u16) -> nix::pty::Winsize {
    nix::pty::Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

#[test]
fn cp_error_during_a_slow_copy_stays_readable_on_screen() {
    let tmp = TmpDir::new("logint");
    let fifo = tmp.path("slow.src");
    let dst = tmp.path("dst.bin");
    let cfifo = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a fresh path inside our own temp dir; failure is reported below.
    assert_eq!(unsafe { libc::mkfifo(cfifo.as_ptr(), 0o600) }, 0, "mkfifo");
    let _feeder = Feeder::start(fifo.clone());

    let pty = openpty(Some(&winsize(24, 80)), None::<&Termios>).unwrap();
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cprog"));
    cmd.arg(&fifo)
        .arg(&dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env("CPROG_SLOW_THRESHOLD_MS", "1")
        .env("CPROG_SAMPLE_INTERVAL_MS", "10")
        .env("CPROG_RENDER_TICK_MS", "10")
        .env_remove("CI")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd));
    // Cap the destination size so `cp` fails *during* the copy. SIGXFSZ must be ignored,
    // otherwise the limit kills cp outright instead of surfacing as an error message.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let lim = libc::rlimit { rlim_cur: FILE_LIMIT, rlim_max: FILE_LIMIT };
            libc::setrlimit(libc::RLIMIT_FSIZE, &lim);
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    // Every slave handle must go: `cmd` still owns the `Stdio`s built from the cloned slave fds,
    // and while any slave stays open in this process the master read below never sees EOF.
    drop(cmd);
    drop(pty.slave);

    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match common::read_retry(&mut master, &mut buf) {
            0 => break,
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    let status = child.wait().unwrap();

    assert_eq!(status.code(), Some(1), "cp's exit code is preserved");

    let raw = String::from_utf8_lossy(&out).into_owned();
    assert!(raw.contains("cp:"), "cp reported an error at all: {raw:?}");
    // A FIFO source has no knowable total, so the bar is the indeterminate one (empty cells).
    assert!(
        raw.contains('\u{2588}') || raw.contains('\u{2591}') || raw.contains('#'),
        "a footer bar must have been drawn, otherwise this test proves nothing: {raw:?}"
    );

    // What the user actually sees after all the overdrawing.
    let screen = render_screen(&out);
    assert!(
        screen.iter().any(|l| l.trim_start().starts_with("cp:")),
        "the error kept its `cp: …` prefix on screen: {screen:#?}"
    );
    assert!(
        !screen.iter().any(|l| l.trim_start().starts_with(':')),
        "no orphaned error fragment (a line starting with `: …` means the prefix was overdrawn): {screen:#?}"
    );
}
