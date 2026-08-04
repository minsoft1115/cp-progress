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

use nix::sys::stat::Mode;

mod common;
use common::{contains, cprog_exec, drain, openpty_cloexec, silent_read_errors, Feeder, TmpDir, Watchdog};

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

    let pty = openpty_cloexec(Some(&nix::pty::Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 }));
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

    // `copied > 0` proves the copy ran, not that the master was ever readable — and everything
    // below is an absence, which an unread channel satisfies too. A `drain` that folds a read
    // error into "nothing arrived" leaves this test green (measured with EBADF, #69).
    assert_eq!(silent_read_errors(), 0, "the drain hit a read error, so `out` proves nothing");

    // The bug: a backgrounded cprog draws the managed footer + hides the cursor. After the fix it
    // must not — no DECTCEM hide (ESC[?25l) and no footer erase-to-EOL (ESC[K).
    assert!(!contains(&out, b"\x1b[?25l"), "backgrounded cprog hid the cursor (managed TUI)");
    assert!(!contains(&out, b"\x1b[K"), "backgrounded cprog drew a footer (managed TUI)");
}

#[test]
fn a_pty_master_on_stdout_is_not_a_foreground_terminal() {
    // exceptions B10, the half that is a definite "no" rather than "cannot tell".
    //
    // A pty master is a terminal by every other test cprog makes — `isatty` says yes, stdout and
    // stderr share it, `TERM` is fine — so nothing else stops managed mode. What stops it is
    // that a master has no foreground process group: `tcgetpgrp` answers with pgid 0. Drawing a
    // footer into a master would not reach a screen at all; it would arrive as *input* on the
    // slave, i.e. as keystrokes for whatever is reading there.
    //
    // Regression guard for the rustix migration (#42). libc returned that 0 verbatim and the
    // pgrp comparison failed, so cprog passed through by accident. rustix reports it as
    // `OPNOTSUPP` instead, and mapping every error to "cannot tell" would have turned an
    // accidental correctness into a real bug.
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::process::{Command, Stdio};

    let tmp = TmpDir::new("ptymaster");
    let src = tmp.0.join("src.bin");
    let dst = tmp.0.join("dst.bin");
    // Big enough that a footer would certainly have been drawn at a 1 ms threshold.
    std::fs::write(&src, vec![0u8; 256 * 1024 * 1024]).unwrap();

    let pty = openpty_cloexec(Some(&nix::pty::Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 }));
    let master_out: OwnedFd = pty.master.try_clone().unwrap();
    let master_err: OwnedFd = pty.master.try_clone().unwrap();
    let slave_fd = pty.slave.as_raw_fd();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg(&src)
        .arg(&dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI")
        .env("CPROG_SLOW_THRESHOLD_MS", "1")
        .env("CPROG_SAMPLE_INTERVAL_MS", "5")
        .env("CPROG_RENDER_TICK_MS", "5")
        .stdin(Stdio::null())
        .stdout(Stdio::from(master_out))
        .stderr(Stdio::from(master_err))
        .spawn()
        .expect("spawn cprog");

    // Read the *slave* — anything cprog wrote to the master surfaces there. Draining keeps a
    // regression visible as a failed assertion rather than a hang on a full input queue.
    //
    // The 30 s bound on the poll below is not on its own enough to keep that promise: the loop
    // can end by *deadline* rather than by the child exiting, and the `child.wait()` after it is
    // unconditional — so a cprog that never exits was still an unbounded wait. Sleeping before
    // `cmd.exec()` in the passthrough path left this test running forever, which is exactly the
    // hang the comment above says it avoids. The watchdog is armed past the poll's deadline so it
    // only ever fires on that overrun (#69).
    let dog = Watchdog::arm(child.id() as i32, 40);
    let mut seen = Vec::new();
    let mut slave = unsafe { File::from_raw_fd(pty.slave.into_raw_fd()) };
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        drain(&mut slave, slave_fd, &mut seen, Instant::now() + Duration::from_millis(100), None);
        if child.try_wait().unwrap().is_some() {
            break;
        }
    }
    let status = child.wait().expect("wait cprog");
    dog.disarm();
    assert!(!dog.hung(), "cprog never exited within the poll deadline");

    assert!(status.success(), "the copy itself still succeeds: {status:?}");
    assert_eq!(std::fs::read(&dst).unwrap().len(), 256 * 1024 * 1024, "and really happened");
    assert!(
        !contains(&seen, &[0xE2, 0x96, 0x88]) && !contains(&seen, &[0xE2, 0x96, 0x91]),
        "no footer may be written into a pty master: {:?}",
        String::from_utf8_lossy(&seen[..seen.len().min(160)])
    );
    // The guards above prove the *copy* ran; they say nothing about the slave ever being
    // readable, and the assertion below is that nothing arrived — which a channel that never
    // produced a byte satisfies just as well. Making `drain` return nothing leaves this test
    // green while 27 other integration tests go red (measured, #69).
    //
    // `silent_read_errors` covers a read that failed; it cannot cover a slave that simply never
    // became readable, because then `drain` never calls `read` at all. So prove the path
    // end-to-end: a sentinel written into the master must come back on the slave, by the same
    // master-to-slave delivery this test says a footer would have taken. It is read into its own
    // buffer so `seen` stays untouched.
    assert_eq!(silent_read_errors(), 0, "the drain hit a read error, so `seen` proves nothing");
    {
        use std::io::Write;
        const SENTINEL: &[u8] = b"cprog-channel-probe\n";
        let mut master = File::from(pty.master);
        master.write_all(SENTINEL).expect("write the probe into the master");
        let mut echoed = Vec::new();
        drain(&mut slave, slave_fd, &mut echoed, Instant::now() + Duration::from_secs(5), Some((0, SENTINEL)));
        assert!(
            contains(&echoed, SENTINEL),
            "the slave never delivered a byte written into the master, so `seen` being empty \
             proves nothing about cprog: {:?}",
            String::from_utf8_lossy(&echoed[..echoed.len().min(160)])
        );
    }
    assert!(seen.is_empty(), "passthrough with no -v emits nothing at all: {} bytes", seen.len());
}
