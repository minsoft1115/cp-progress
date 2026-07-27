#![cfg(feature = "integration")]
//! bug2 (issue #2): `Ctrl-Z` (SIGTSTP) while the footer is up must restore the terminal (show
//! cursor, erase footer) *before* stopping, and redraw on resume (SIGCONT). Plus the bug1/bug2
//! seam: if the job is resumed in the **background** (`Ctrl-Z` then `bg`), cprog must not redraw
//! the footer (it is no longer the foreground process group).

use std::fs::File;
use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
use std::os::raw::c_char;
use std::time::{Duration, Instant};

use nix::pty::openpty;
use nix::sys::stat::Mode;

mod common;
use common::{contains, cprog_exec, drain, rfind, Feeder, TmpDir};

/// Fork a child that establishes a controlling terminal (setsid + TIOCSCTTY) and execs `cprog`
/// as the foreground process group. Returns `(child_pid, master File, master_fd)`.
fn spawn_foreground_cprog(fifo: &std::path::Path, dst: &std::path::Path) -> (i32, File, RawFd) {
    let (prog, argv_o, envp_o) = cprog_exec(fifo, dst);
    let mut argv: Vec<*const c_char> = argv_o.iter().map(|s| s.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut envp: Vec<*const c_char> = envp_o.iter().map(|s| s.as_ptr()).collect();
    envp.push(std::ptr::null());

    let pty = openpty(None, None).expect("openpty");
    let master_fd = pty.master.into_raw_fd();
    let slave_fd = pty.slave.into_raw_fd();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        unsafe {
            libc::close(master_fd);
            libc::setsid();
            libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);
            libc::dup2(slave_fd, 0);
            libc::dup2(slave_fd, 1);
            libc::dup2(slave_fd, 2);
            libc::execvpe(prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
    }
    unsafe { libc::close(slave_fd) };
    let master = unsafe { File::from_raw_fd(master_fd) };
    (pid, master, master_fd)
}

#[test]
fn ctrl_z_restores_terminal_before_stop_then_redraws_on_resume() {
    let tmp = TmpDir::new("fg");
    let fifo = tmp.0.join("src.fifo");
    let dst = tmp.0.join("dst.bin");
    nix::unistd::mkfifo(&fifo, Mode::from_bits_truncate(0o600)).unwrap();

    let (pid, mut master, fd) = spawn_foreground_cprog(&fifo, &dst);
    let _feeder = Feeder::start(fifo.clone());
    let mut out = Vec::new();

    // 1) Wait for the footer (cursor hidden).
    drain(&mut master, fd, &mut out, Instant::now() + Duration::from_millis(2000), Some((0, b"\x1b[?25l")));
    assert!(contains(&out, b"\x1b[?25l"), "footer never engaged; scenario not exercised");

    // 2) Ctrl-Z -> the cursor must be restored, then the process actually stops.
    let pre = out.len();
    unsafe { libc::kill(pid, libc::SIGTSTP) };
    let mut stopped = false;
    let deadline = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < deadline {
        drain(&mut master, fd, &mut out, Instant::now() + Duration::from_millis(50), None);
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
        if r == pid && libc::WIFSTOPPED(status) {
            stopped = true;
            break;
        }
    }
    assert!(stopped, "cprog did not stop on SIGTSTP");
    assert!(contains(&out[pre..], b"\x1b[?25h"), "cursor not restored (ESC[?25h) before stopping");

    // 3) Resume in the foreground -> the footer is redrawn (cursor re-hidden).
    let mark = out.len();
    unsafe { libc::kill(pid, libc::SIGCONT) };
    drain(&mut master, fd, &mut out, Instant::now() + Duration::from_millis(1000), Some((mark, b"\x1b[?25l")));
    assert!(contains(&out[mark..], b"\x1b[?25l"), "footer not redrawn after resume");

    unsafe { libc::kill(pid, libc::SIGKILL) };
    let mut s = 0;
    unsafe { libc::waitpid(pid, &mut s, 0) };
}

#[test]
fn ctrl_z_then_bg_does_not_redraw_footer_in_background() {
    // seam (#1 x #2): a job resumed in the background must not take over the terminal again.
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

    // Child A owns the terminal and drives the job-control dance (foreground -> Ctrl-Z -> bg ->
    // resume), so the parent only needs to drain and assert.
    let a = unsafe { libc::fork() };
    assert!(a >= 0, "fork failed");
    if a == 0 {
        unsafe {
            libc::close(master_fd);
            libc::setsid();
            libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);
            libc::dup2(slave_fd, 0);
            libc::dup2(slave_fd, 1);
            libc::dup2(slave_fd, 2);
            // Like a real shell doing job control: ignore SIGTTOU so a background tcsetpgrp
            // (reclaiming the foreground on `bg`) succeeds instead of stopping us.
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            let apg = libc::getpgrp();
            let b = libc::fork();
            if b == 0 {
                libc::setpgid(0, 0);
                libc::execvpe(prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
            libc::tcsetpgrp(0, b); // cprog foreground -> managed
            let nap = |s: i64| {
                let ts = libc::timespec { tv_sec: 0, tv_nsec: s * 1_000_000 };
                libc::nanosleep(&ts, std::ptr::null_mut());
            };
            nap(800); // footer up
            libc::kill(b, libc::SIGTSTP);
            let mut st = 0;
            libc::waitpid(b, &mut st, libc::WUNTRACED);
            libc::tcsetpgrp(0, apg); // `bg`: A reclaims the foreground; cprog is now background
            libc::kill(b, libc::SIGCONT); // resume cprog in the background
            nap(800);
            libc::kill(-b, libc::SIGKILL);
            libc::_exit(0);
        }
    }

    unsafe { libc::close(slave_fd) };
    let _feeder = Feeder::start(fifo.clone());
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let mut out = Vec::new();
    drain(&mut master, master_fd, &mut out, Instant::now() + Duration::from_millis(3000), None);
    unsafe { libc::kill(-a, libc::SIGKILL) };
    let mut s = 0;
    unsafe { libc::waitpid(a, &mut s, 0) };

    // Scenario exercised: the footer engaged (hide) and the suspend restored the cursor (show).
    assert!(contains(&out, b"\x1b[?25l"), "footer never engaged; scenario not exercised");
    let last_show = rfind(&out, b"\x1b[?25h").expect("suspend should have shown the cursor");
    // After the suspend restore, cprog resumes in the *background*: it must not hide the cursor /
    // redraw the footer again. So there must be no more ESC[?25l after the last cursor-show.
    assert!(
        !contains(&out[last_show..], b"\x1b[?25l"),
        "cprog redrew the footer after being resumed in the background (bug1 symptom via Ctrl-Z then bg)"
    );
}
