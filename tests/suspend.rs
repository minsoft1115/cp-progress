#![cfg(feature = "integration")]
//! bug2 (issue #2): `Ctrl-Z` (SIGTSTP) while the footer is up must restore the terminal (show
//! cursor, erase footer) *before* stopping, and redraw on resume (SIGCONT). Plus the bug1/bug2
//! seam: if the job is resumed in the **background** (`Ctrl-Z` then `bg`), cprog must not redraw
//! the footer (it is no longer the foreground process group).

use std::fs::File;
use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
use std::os::raw::c_char;
use std::time::{Duration, Instant};

use nix::sys::stat::Mode;

mod common;
use common::{contains, cprog_exec, drain, open_fifo_writer, openpty_cloexec, rfind, Feeder, TmpDir};

/// Fork a child that establishes a controlling terminal (setsid + TIOCSCTTY) and execs `cprog`
/// as the foreground process group. Returns `(child_pid, master File, master_fd)`.
fn spawn_foreground_cprog(fifo: &std::path::Path, dst: &std::path::Path) -> (i32, File, RawFd) {
    let (prog, argv_o, envp_o) = cprog_exec(fifo, dst);
    let mut argv: Vec<*const c_char> = argv_o.iter().map(|s| s.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut envp: Vec<*const c_char> = envp_o.iter().map(|s| s.as_ptr()).collect();
    envp.push(std::ptr::null());

    let pty = openpty_cloexec(None);
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

    let pty = openpty_cloexec(None);
    let master_fd = pty.master.into_raw_fd();
    let slave_fd = pty.slave.into_raw_fd();

    // Child A owns the terminal and drives the job-control dance (foreground -> Ctrl-Z -> bg ->
    // resume), so the parent only needs to drain and assert.

    // One rendezvous crosses between parent and child: the child cannot see the master, so it
    // used to *wait 800 ms and hope* the footer had appeared. On a loaded runner that is not
    // always true, and the run then proved nothing — which is how this failed in CI (#61 D). The
    // parent watches for the footer and pokes this pipe; the child blocks on it.
    let mut rendezvous = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(rendezvous.as_mut_ptr()) }, 0, "pipe()");
    let (ready_r, ready_w) = (rendezvous[0], rendezvous[1]);
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
            libc::close(ready_w);
            libc::tcsetpgrp(0, b); // cprog foreground -> managed
            let nap = |s: i64| {
                let ts = libc::timespec { tv_sec: 0, tv_nsec: s * 1_000_000 };
                libc::nanosleep(&ts, std::ptr::null_mut());
            };
            // Wait for the parent to say the footer is actually up, not for a clock.
            let mut ack = [0u8; 1];
            libc::read(ready_r, ack.as_mut_ptr() as *mut libc::c_void, 1);
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

    unsafe {
        libc::close(slave_fd);
        libc::close(ready_r);
    }
    let _feeder = Feeder::start(fifo.clone());
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let mut out = Vec::new();
    // Phase 1: wait for the footer to actually engage, then release the child. The cursor-hide is
    // cprog's first footer byte, so it is the earliest honest signal that the scenario can start.
    drain(&mut master, master_fd, &mut out, Instant::now() + Duration::from_millis(5000), Some((0, b"\x1b[?25l")));
    assert!(
        contains(&out, b"\x1b[?25l"),
        "footer never engaged within 5 s, so the job-control sequence was never started"
    );
    unsafe { libc::write(ready_w, b"1".as_ptr() as *const libc::c_void, 1) };
    // Phase 2: the child now runs its sequence; drain the rest.
    drain(&mut master, master_fd, &mut out, Instant::now() + Duration::from_millis(3000), None);
    unsafe {
        libc::close(ready_w);
        libc::kill(-a, libc::SIGKILL);
    }
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

#[test]
fn ctrl_z_bg_then_second_ctrl_z_fg_restores_footer() {
    // The suppression from A10 is one-way but not permanent: once footer-suppressed by a
    // background resume, another Ctrl-Z followed by `fg` re-checks the foreground and turns the
    // footer back on (docs/usage.md "바가 안 뜨는 경우", docs/process-model.md "정리",
    // exceptions A10). Same rig as the test above, with a second suspend/resume cycle appended.
    let tmp = TmpDir::new("bgfg");
    let fifo = tmp.0.join("src.fifo");
    let dst = tmp.0.join("dst.bin");
    nix::unistd::mkfifo(&fifo, Mode::from_bits_truncate(0o600)).unwrap();

    let (prog, argv_o, envp_o) = cprog_exec(&fifo, &dst);
    let mut argv: Vec<*const c_char> = argv_o.iter().map(|s| s.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut envp: Vec<*const c_char> = envp_o.iter().map(|s| s.as_ptr()).collect();
    envp.push(std::ptr::null());

    let pty = openpty_cloexec(None);
    let master_fd = pty.master.into_raw_fd();
    let slave_fd = pty.slave.into_raw_fd();

    // One rendezvous crosses between parent and child: the child cannot see the master, so it
    // used to *wait 800 ms and hope* the footer had appeared. On a loaded runner that is not
    // always true, and the run then proved nothing — which is how this failed in CI (#61 D). The
    // parent watches for the footer and pokes this pipe; the child blocks on it.
    let mut rendezvous = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(rendezvous.as_mut_ptr()) }, 0, "pipe()");
    let (ready_r, ready_w) = (rendezvous[0], rendezvous[1]);

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
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            let apg = libc::getpgrp();
            let b = libc::fork();
            if b == 0 {
                libc::setpgid(0, 0);
                libc::execvpe(prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
            libc::close(ready_w);
            libc::tcsetpgrp(0, b); // cprog foreground -> managed
            let nap = |ms: i64| {
                let ts = libc::timespec { tv_sec: 0, tv_nsec: ms * 1_000_000 };
                libc::nanosleep(&ts, std::ptr::null_mut());
            };
            let mut st = 0;
            // Wait for the parent to say the footer is actually up, not for a clock.
            let mut ack = [0u8; 1];
            libc::read(ready_r, ack.as_mut_ptr() as *mut libc::c_void, 1);
            libc::kill(b, libc::SIGTSTP);
            libc::waitpid(b, &mut st, libc::WUNTRACED);
            libc::tcsetpgrp(0, apg); // `bg`: cprog resumes suppressed
            libc::kill(b, libc::SIGCONT);
            nap(400); // long enough that a wrongly-unsuppressed footer would have drawn
            libc::kill(b, libc::SIGTSTP); // second Ctrl-Z, taken by the still-installed flag
            libc::waitpid(b, &mut st, libc::WUNTRACED);
            // A marker on the terminal so the parent can date the `fg`. Without it the only
            // anchor is the last cursor-show, which is the *first* suspend's (the second
            // `suspend_restore` writes nothing — the cursor is already visible), and then a
            // background redraw would satisfy the assertion just as well as the recovery (#61).
            let mark = b"<FG>";
            libc::write(1, mark.as_ptr() as *const libc::c_void, mark.len());
            libc::tcsetpgrp(0, b); // `fg`: cprog is the foreground group again
            libc::kill(b, libc::SIGCONT);
            nap(800); // the resumed render loop re-checks the foreground and redraws
            libc::kill(-b, libc::SIGKILL);
            libc::_exit(0);
        }
    }

    unsafe {
        libc::close(slave_fd);
        libc::close(ready_r);
    }
    let _feeder = Feeder::start(fifo.clone());
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let mut out = Vec::new();
    // Phase 1: wait for the footer to actually engage, then release the child. The cursor-hide is
    // cprog's first footer byte, so it is the earliest honest signal that the scenario can start.
    drain(&mut master, master_fd, &mut out, Instant::now() + Duration::from_millis(5000), Some((0, b"\x1b[?25l")));
    assert!(
        contains(&out, b"\x1b[?25l"),
        "footer never engaged within 5 s, so the job-control sequence was never started"
    );
    unsafe { libc::write(ready_w, b"1".as_ptr() as *const libc::c_void, 1) };
    // Phase 2: the child now runs its sequence; drain the rest.
    drain(&mut master, master_fd, &mut out, Instant::now() + Duration::from_millis(4500), None);
    unsafe {
        libc::close(ready_w);
        libc::kill(-a, libc::SIGKILL);
    }
    let mut s = 0;
    unsafe { libc::waitpid(a, &mut s, 0) };

    // Scenario exercised: the footer engaged, and the first suspend restored the cursor.
    assert!(contains(&out, b"\x1b[?25l"), "footer never engaged; scenario not exercised");
    assert!(rfind(&out, b"\x1b[?25h").is_some(), "suspend should have shown the cursor");
    // The inverse of the background test above: the footer must come back *after the `fg`*, so
    // the hide is required after the marker the child wrote at that moment. Anchoring on the last
    // cursor-show instead would also accept a footer wrongly redrawn while backgrounded.
    let fg = rfind(&out, b"<FG>").expect("the child should have marked the fg moment");
    assert!(
        contains(&out[fg..], b"\x1b[?25l"),
        "footer must come back after a second Ctrl-Z followed by fg (suppression is per-resume, not permanent)"
    );
}

#[test]
fn ctrl_z_during_teardown_still_stops_the_process() {
    // exceptions A11, and the half nothing held (#59). When the render loop ends, cprog puts
    // SIGTSTP back to `SIG_DFL`; `lib.rs::restore_default_suspend` is unit-tested, but that the
    // loop *calls* it was not, and deleting the call left all 261 tests green.
    //
    // What the call buys: during teardown the loop is gone, so nobody reads the suspend flag any
    // more. With signal-hook's flag handler still installed, a Ctrl-Z there would neither stop
    // the process nor let it continue — a wedge. Restored, the kernel's default action applies
    // and the process really stops, which is what this test observes.
    //
    // The teardown window is made wide on purpose: the sampler thread checks its stop flag
    // between ticks, so a three-second sample interval makes its join take about that long. A
    // FIFO with no data keeps cp deterministic (it blocks in `read`) exactly as in
    // `tests/signals.rs`.
    use nix::sys::signal::{kill, Signal};
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Command, Stdio};

    let tmp = TmpDir::new("teardown_tstp");
    let fifo = tmp.0.join("src.fifo");
    let dst = tmp.0.join("dst.bin");
    nix::unistd::mkfifo(&fifo, Mode::from_bits_truncate(0o600)).unwrap();

    let ws = nix::pty::Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty_cloexec(Some(&ws));
    let out_fd = pty.slave.try_clone().unwrap();
    let err_fd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg(&fifo)
        .arg(&dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI")
        .env("CPROG_SLOW_THRESHOLD_MS", "1")
        .env("CPROG_RENDER_TICK_MS", "5")
        .env("CPROG_SAMPLE_INTERVAL_MS", "3000") // widen the sampler join = the teardown window
        .process_group(0) // signal cprog alone
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);
    let pid = Pid::from_raw(child.id() as i32);

    let _writer = open_fifo_writer(&fifo).expect("cp never opened the FIFO to read");
    let mut master = File::from(pty.master);
    let fd = std::os::fd::AsRawFd::as_raw_fd(&master);

    // Wait for the footer to engage, so the render loop is definitely running.
    let mut out = Vec::new();
    drain(&mut master, fd, &mut out, Instant::now() + Duration::from_millis(3000), Some((0, b"\x1b[?25l")));
    assert!(contains(&out, b"\x1b[?25l"), "footer never engaged; scenario not exercised");

    // End the render loop, then arrive with Ctrl-Z while it is tearing down.
    kill(pid, Signal::SIGTERM).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    kill(pid, Signal::SIGTSTP).unwrap();

    let deadline = Instant::now() + Duration::from_millis(2000);
    let mut stopped = false;
    while Instant::now() < deadline {
        match waitpid(pid, Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Stopped(_, sig)) => {
                assert_eq!(sig, Signal::SIGTSTP, "stopped by the signal we sent");
                stopped = true;
                break;
            }
            Ok(WaitStatus::StillAlive) => std::thread::sleep(Duration::from_millis(20)),
            other => panic!("cprog left teardown without stopping: {other:?}"),
        }
    }
    assert!(stopped, "SIGTSTP during teardown was swallowed instead of stopping the process");

    kill(pid, Signal::SIGCONT).unwrap();
    let status = child.wait().unwrap();
    assert!(status.signal().is_some(), "cprog still exits signaled, got {status:?}");
}

#[test]
fn resuming_drops_the_rate_history_that_spans_the_stop() {
    // exceptions A13, the bridge nothing held (#59). `progress::reset_samples` and
    // `sampler::reset_rate_history` are both tested, but that the render loop *tells* the sampler
    // it was resumed — the `resumed` flag between them — was not, and removing it left the suite
    // green.
    //
    // What it costs when it breaks is visible in the footer: the window is wall-clock, so a stop
    // that spans it reads as "no throughput" and the rate collapses to `0 B/s` for a whole window
    // after resuming, instead of going honestly unknown (`-- MiB/s`) until real samples exist.
    // That difference is the assertion: `--` can only come from a model that was emptied.
    //
    // Only stop and continue are needed here, not the foreground dance of the tests above: the
    // rule is about the rate, not about who owns the terminal.
    use nix::sys::signal::{kill, Signal};
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let tmp = TmpDir::new("resumed_rate");
    let fifo = tmp.0.join("src.fifo");
    let dst = tmp.0.join("dst.bin");
    nix::unistd::mkfifo(&fifo, Mode::from_bits_truncate(0o600)).unwrap();

    let ws = nix::pty::Winsize { ws_row: 24, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty_cloexec(Some(&ws));
    let out_fd = pty.slave.try_clone().unwrap();
    let err_fd = pty.slave.try_clone().unwrap();

    // Scoped so the parent stops holding the dup'd slaves (see `tests/managed.rs`).
    let mut child = {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cprog"));
        cmd.arg(&fifo)
            .arg(&dst)
            .env("TERM", "xterm")
            .env("LC_ALL", "C.UTF-8")
            .env_remove("CI")
            .env("CPROG_SLOW_THRESHOLD_MS", "1")
            .env("CPROG_RENDER_TICK_MS", "8")
            // Wide enough that the one post-resume tick with a single sample — the `--` window —
            // is drawn many times before a second sample makes a rate again.
            .env("CPROG_SAMPLE_INTERVAL_MS", "300")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_fd))
            .stderr(Stdio::from(err_fd));
        cmd.spawn().unwrap()
    };
    drop(pty.slave);
    let pid = Pid::from_raw(child.id() as i32);

    let _feeder = Feeder::start(fifo.clone());
    let mut master = File::from(pty.master);
    let fd = std::os::fd::AsRawFd::as_raw_fd(&master);
    let mut out = Vec::new();

    // A real rate first: the bar must have been up long enough for two samples.
    //
    // Read the rate out of the *most recent* frame only. `strip_sgr` drops the control
    // characters, so a footer redrawn in place becomes one concatenation of every frame ever
    // drawn — and the first frames legitimately say `-- MiB/s`, before two samples existed.
    // Asking the whole history whether it contains `--` always answers yes, whatever the screen
    // says now.
    let last_rate = |data: &[u8]| -> Option<String> {
        let t = common::strip_sgr(&data[data.len().saturating_sub(4096)..]);
        let end = t.rfind("/s)")?;
        let start = t[..end].rfind('(')?;
        Some(t[start..end + 3].to_string())
    };
    let deadline = Instant::now() + Duration::from_millis(8000);
    while Instant::now() < deadline {
        drain(&mut master, fd, &mut out, Instant::now() + Duration::from_millis(100), None);
        if last_rate(&out).is_some_and(|r| !r.contains("--")) {
            break;
        }
    }
    assert!(
        last_rate(&out).is_some_and(|r| !r.contains("--")),
        "a real rate never appeared, so the scenario was not exercised: {:?}",
        last_rate(&out)
    );

    kill(pid, Signal::SIGTSTP).unwrap();
    let stop_deadline = Instant::now() + Duration::from_millis(2000);
    let mut stopped = false;
    while Instant::now() < stop_deadline {
        match waitpid(pid, Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Stopped(..)) => {
                stopped = true;
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    assert!(stopped, "cprog never stopped on SIGTSTP");

    // Long enough that a surviving window would be mostly dead time.
    std::thread::sleep(Duration::from_millis(1200));
    let mark = out.len();
    kill(pid, Signal::SIGCONT).unwrap();

    let resume_deadline = Instant::now() + Duration::from_millis(4000);
    let mut unknown_after_resume = false;
    while Instant::now() < resume_deadline {
        drain(&mut master, fd, &mut out, Instant::now() + Duration::from_millis(100), None);
        if common::strip_sgr(&out[mark..]).contains("-- MiB/s") {
            unknown_after_resume = true;
            break;
        }
    }
    let _ = kill(pid, Signal::SIGKILL);
    let _ = child.wait();

    assert!(
        unknown_after_resume,
        "after resuming, the rate must go unknown rather than carry a window full of dead time: {:?}",
        common::strip_sgr(&out[mark..])
    );
}
