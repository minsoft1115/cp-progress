#![cfg(feature = "integration")]
//! bug1 (issue #1): a backgrounded `cprog &` must NOT enter the managed TUI. It is not the
//! terminal's foreground process group, so it must fall back to passthrough — no footer, no
//! cursor hide — behaving exactly like `cp`.
//!
//! Setup mirrors a real interactive shell with job control: a child `A` establishes a controlling
//! terminal (setsid + TIOCSCTTY) and stays the foreground process group; it then forks `cprog`
//! into its *own* process group, making `cprog` a background job of `A`'s session. A throttled
//! FIFO keeps the copy slow so a managed footer *would* have rendered if the bug were present.

use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::pty::openpty;
use nix::sys::stat::Mode;

mod common;
use common::read_retry;

struct TmpDir(std::path::PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cprog_bg_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn backgrounded_cprog_falls_back_to_passthrough_no_tui() {
    let tmp = TmpDir::new("nofooter");
    let fifo = tmp.0.join("src.fifo");
    let dst = tmp.0.join("dst.bin");
    nix::unistd::mkfifo(&fifo, Mode::from_bits_truncate(0o600)).unwrap();

    // Build argv/envp as C strings *before* fork (no allocation in the child before exec).
    let prog = CString::new(env!("CARGO_BIN_EXE_cprog")).unwrap();
    let argv_owned = [
        CString::new("cprog").unwrap(),
        CString::new(fifo.to_str().unwrap()).unwrap(),
        CString::new(dst.to_str().unwrap()).unwrap(),
    ];
    let mut argv: Vec<*const c_char> = argv_owned.iter().map(|s| s.as_ptr()).collect();
    argv.push(std::ptr::null());

    // Minimal controlled env: PATH (to find cp/stdbuf) + TERM, brisk timings, CI unset.
    let mut env_pairs = vec![
        "TERM=xterm".to_string(),
        "CPROG_SLOW_THRESHOLD_MS=1".to_string(),
        "CPROG_SAMPLE_INTERVAL_MS=10".to_string(),
        "CPROG_RENDER_TICK_MS=10".to_string(),
    ];
    if let Ok(p) = std::env::var("PATH") {
        env_pairs.push(format!("PATH={p}"));
    }
    let envp_owned: Vec<CString> = env_pairs.into_iter().map(|s| CString::new(s).unwrap()).collect();
    let mut envp: Vec<*const c_char> = envp_owned.iter().map(|s| s.as_ptr()).collect();
    envp.push(std::ptr::null());

    let pty = openpty(None, None).expect("openpty");
    let master_fd = pty.master.into_raw_fd();
    let slave_fd = pty.slave.into_raw_fd();

    // SAFETY: after the first fork the child is single-threaded and calls only async-signal-safe
    // functions before exec (setsid/ioctl/dup2/fork/setpgid/execvpe/nanosleep/_exit).
    let a = unsafe { libc::fork() };
    assert!(a >= 0, "fork failed");
    if a == 0 {
        unsafe {
            libc::close(master_fd);
            libc::setsid(); // A = new session leader (foreground pgrp of the tty below)
            libc::ioctl(slave_fd, libc::TIOCSCTTY, 0); // slave becomes controlling terminal
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

    // Parent: drop the slave, feed the FIFO slowly, and capture what cprog wrote to the terminal.
    unsafe { libc::close(slave_fd) };
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (fifo, stop) = (fifo.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let Ok(mut w) = std::fs::OpenOptions::new().write(true).open(&fifo) else { return };
            let chunk = vec![0u8; 64 * 1024];
            while !stop.load(Ordering::Relaxed) {
                if w.write_all(&chunk).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };

    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let deadline = Instant::now() + Duration::from_millis(3000);
    while Instant::now() < deadline {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    stop.store(true, Ordering::Relaxed);
    unsafe { libc::kill(-a, libc::SIGKILL) };
    let mut status = 0;
    unsafe { libc::waitpid(a, &mut status, 0) };
    let _ = feeder.join();

    // Guard against a vacuous pass: the copy must actually have run in passthrough (dst grew).
    let copied = std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
    assert!(copied > 0, "cp did not copy anything; scenario not exercised");

    // The bug: a backgrounded cprog draws the managed footer + hides the cursor. After the fix it
    // must not — no DECTCEM hide (ESC[?25l) and no footer erase-to-EOL (ESC[K).
    assert!(
        !out.windows(6).any(|w| w == b"\x1b[?25l"),
        "backgrounded cprog hid the cursor (managed TUI); expected passthrough. bytes={}",
        out.len()
    );
    assert!(
        !out.windows(3).any(|w| w == b"\x1b[K"),
        "backgrounded cprog drew a footer (managed TUI); expected passthrough. bytes={}",
        out.len()
    );
}
