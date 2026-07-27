#![cfg(feature = "integration")]
//! bug1 (issue #1): a backgrounded `cprog &` must NOT enter the managed TUI. It is not the
//! terminal's foreground process group, so it must fall back to passthrough — no footer, no
//! cursor hide — behaving exactly like `cp`.
//!
//! Setup mirrors a real interactive shell with job control: a child `A` establishes a controlling
//! terminal (setsid + TIOCSCTTY) and stays the foreground process group; it then forks `cprog`
//! into its *own* process group, making `cprog` a background job of `A`'s session. A throttled
//! FIFO keeps the copy slow so a managed footer *would* have rendered if the bug were present.

use std::fs::File;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::raw::c_char;
use std::time::{Duration, Instant};

use nix::pty::openpty;
use nix::sys::stat::Mode;

mod common;
use common::{contains, cprog_exec, drain, Feeder, TmpDir};

#[test]
fn backgrounded_cprog_falls_back_to_passthrough_no_tui() {
    let tmp = TmpDir::new("bg");
    let fifo = tmp.0.join("src.fifo");
    let dst = tmp.0.join("dst.bin");
    nix::unistd::mkfifo(&fifo, Mode::from_bits_truncate(0o600)).unwrap();

    let (prog, argv_o, envp_o) = cprog_exec(&fifo, &dst);
    let mut argv: Vec<*const c_char> = argv_o.iter().map(|s| s.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut envp: Vec<*const c_char> = envp_o.iter().map(|s| s.as_ptr()).collect();
    envp.push(std::ptr::null());

    let pty = openpty(None, None).expect("openpty");
    let master_fd = pty.master.into_raw_fd();
    let slave_fd = pty.slave.into_raw_fd();

    // SAFETY: after the first fork the child is single-threaded and calls only async-signal-safe
    // functions before exec.
    let a = unsafe { libc::fork() };
    assert!(a >= 0, "fork failed");
    if a == 0 {
        unsafe {
            libc::close(master_fd);
            libc::setsid(); // A = new session leader / foreground pgrp of the tty
            libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);
            libc::dup2(slave_fd, 0);
            libc::dup2(slave_fd, 1);
            libc::dup2(slave_fd, 2);
            let b = libc::fork();
            if b == 0 {
                libc::setpgid(0, 0); // cprog = its OWN pgrp => a background job of A's session
                libc::execvpe(prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
            let ts = libc::timespec { tv_sec: 3, tv_nsec: 0 };
            libc::nanosleep(&ts, std::ptr::null_mut());
            libc::kill(-b, libc::SIGKILL);
            libc::_exit(0);
        }
    }

    unsafe { libc::close(slave_fd) };
    let _feeder = Feeder::start(fifo.clone());
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let mut out = Vec::new();
    drain(&mut master, master_fd, &mut out, Instant::now() + Duration::from_millis(2000), None);

    unsafe { libc::kill(-a, libc::SIGKILL) };
    let mut status = 0;
    unsafe { libc::waitpid(a, &mut status, 0) };

    // Guard against a vacuous pass: the copy must actually have run in passthrough (dst grew).
    let copied = std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
    assert!(copied > 0, "cp did not copy anything; scenario not exercised");

    // The bug: a backgrounded cprog draws the managed footer + hides the cursor. After the fix it
    // must not — no DECTCEM hide (ESC[?25l) and no footer erase-to-EOL (ESC[K).
    assert!(!contains(&out, b"\x1b[?25l"), "backgrounded cprog hid the cursor (managed TUI)");
    assert!(!contains(&out, b"\x1b[K"), "backgrounded cprog drew a footer (managed TUI)");
}
