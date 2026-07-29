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

use nix::pty::{openpty, Winsize};
use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::Pid;

mod common;
use common::{read_retry, rfind, TmpDir, Watchdog};

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
    // Without this the only exit from the read loop below is EOF on the master, so a
    // teardown wedge hangs `cargo test` instead of failing it (#61 D).
    let dog = Watchdog::arm(child.id() as i32, 20);

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
    dog.disarm();
    assert!(!dog.hung(), "cprog hung: the watchdog had to kill it");

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

    // A hang here is a teardown deadlock, which is the bug this test exists for.
    let dog = Watchdog::arm(cprog_pid.as_raw(), 8);

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
                // Without `-v` nothing from cp reaches the terminal (#20), so readiness is the
                // footer engaging instead: the cursor-hide it emits on its first draw means the
                // slow timer fired, which means cp is running and pulsing.
                if !sent && common::contains(&out, b"\x1b[?25l") {
                    kill(cprog_pid, Signal::SIGTERM).unwrap(); // cprog only; cp keeps blocking
                    sent = true;
                }
            }
        }
    }
    let status = child.wait().unwrap();
    dog.disarm();

    assert!(sent, "cp never started, so the teardown path was not exercised");
    assert!(!dog.hung(), "cprog hung after SIGTERM (reader join deadlock)");
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

    // A hang here is a teardown deadlock, which is the bug this test exists for.
    let dog = Watchdog::arm(cprog_pid.as_raw(), 8);

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
                // Readiness is the footer's cursor-hide, not a `-v` line: without `-v`
                // cp's output is never relayed (#20).
                if !sent && common::contains(&out, b"\x1b[?25l") {
                    kill(cprog_pid, Signal::SIGINT).unwrap(); // cprog only; cp keeps blocking
                    sent = true;
                }
            }
        }
    }
    let status = child.wait().unwrap();
    dog.disarm();

    assert!(sent, "cp never started, so the teardown path was not exercised");
    assert!(!dog.hung(), "cprog hung after SIGINT (reader join deadlock)");
    // The re-raised signal must be the *same* one cprog received (SIGINT), not a normalized SIGTERM.
    assert_eq!(
        status.signal(),
        Some(Signal::SIGINT as i32),
        "cprog-alone signal must be forwarded to cp and re-raised as itself, got {status:?}"
    );
}

/// The pid of the `cp` that `cprog` spawned, once it exists. `stdbuf` execs `cp`, so it is
/// cprog's direct child and keeps the same pid (docs/testing.md D7).
fn cp_child_of(cprog: i32) -> Option<i32> {
    let children = std::fs::read_to_string(format!("/proc/{cprog}/task/{cprog}/children")).ok()?;
    children
        .split_whitespace()
        .filter_map(|p| p.parse::<i32>().ok())
        .find(|p| {
            std::fs::read_to_string(format!("/proc/{p}/comm"))
                .is_ok_and(|c| c.trim() == "cp")
        })
}

#[test]
fn cp_killed_by_a_realtime_signal_still_exits_cprog_signaled() {
    // exceptions A1 is "reproduce cp's termination exactly", and it must hold for the whole
    // signal range, not just the named ones. This is a characterization test: it passes on the
    // hand-rolled sigaction/raise, and it is what stops a switch to
    // signal_hook::low_level::emulate_default_handler from silently weakening the contract —
    // that function's table covers the standard signals only and reports EINVAL for
    // SIGRTMIN..SIGRTMAX, which would turn a signaled death into a plain `128+s` exit (#43).
    //
    // cp is signalled alone rather than the group: a realtime signal delivered to cprog itself
    // has no handler, so cprog would die of it directly and never reach the re-raise path.
    let tmp = TmpDir::new("rtsig");
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
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);
    let cprog_pid = child.id() as i32;
    let rtmin = libc::SIGRTMIN();

    let bar: [u8; 3] = [0xE2, 0x96, 0x88]; // █
    // Without this the only exit from the read loop below is EOF on the master, so a
    // teardown wedge hangs `cargo test` instead of failing it (#61 D).
    let dog = Watchdog::arm(child.id() as i32, 20);

    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut killed = false;
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => {
                out.extend_from_slice(&buf[..n]);
                // Wait for the footer so the managed path is genuinely running, then take cp
                // out with a signal signal-hook's table does not know.
                if !killed && out.windows(3).any(|w| w == bar) {
                    if let Some(cp) = cp_child_of(cprog_pid) {
                        // SAFETY: a live pid and a valid signal number; ESRCH if it just exited.
                        unsafe { libc::kill(cp, rtmin) };
                        killed = true;
                    }
                }
            }
        }
    }
    let status = child.wait().unwrap();
    dog.disarm();
    assert!(!dog.hung(), "cprog hung: the watchdog had to kill it");

    assert!(killed, "never found cp to signal, so the path was not exercised");
    assert_eq!(
        status.signal(),
        Some(rtmin),
        "cprog must die of the same realtime signal cp did, not exit {} normally",
        128 + rtmin
    );
}

#[test]
fn killing_cprog_outright_takes_cp_with_it() {
    // exceptions C4. PR_SET_PDEATHSIG(SIGTERM) is the only thing standing between a dead cprog
    // and an orphaned `cp` still writing to the destination. It is set inside a `pre_exec`
    // whose error is deliberately swallowed — a copy is not worth aborting over it — so if it
    // ever stopped being applied, nothing would say so.
    //
    // SIGKILL on purpose: it is the one signal cprog cannot handle, so no teardown of ours runs
    // and the kernel's death signal is genuinely all that is left.
    let tmp = TmpDir::new("pdeath");
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
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);
    let cprog_pid = child.id() as i32;

    // Open the write end so cp gets past open() and blocks on read with nothing coming.
    let writer = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut cp_pid = None;
    while cp_pid.is_none() && std::time::Instant::now() < deadline {
        cp_pid = cp_child_of(cprog_pid);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let cp_pid = cp_pid.expect("cp never appeared as cprog's child");

    kill(Pid::from_raw(cprog_pid), Signal::SIGKILL).unwrap();
    let _ = child.wait();

    // cp is not our child, so poll /proc rather than wait for it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut gone = false;
    while std::time::Instant::now() < deadline {
        if !std::path::Path::new(&format!("/proc/{cp_pid}")).exists() {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    drop(writer);
    if !gone {
        // Do not leave a stray cp blocked on the FIFO behind a failing test.
        let _ = kill(Pid::from_raw(cp_pid), Signal::SIGKILL);
    }
    assert!(gone, "cp {cp_pid} outlived a SIGKILLed cprog — PR_SET_PDEATHSIG is not taking");
}

#[test]
fn a_signal_cprog_does_not_register_keeps_its_default_action() {
    // exceptions A4: only SIGINT/TERM/HUP/QUIT are caught; everything else keeps the kernel
    // default. SIGUSR1 terminates, so cprog dies of it *without* running any teardown — the
    // footer and the hidden cursor are left on screen, exactly as in F7. That is the accepted
    // consequence of not catching every signal, and it is asserted here so "cprog cleans up on
    // any signal" cannot be assumed by mistake.
    let tmp = TmpDir::new("usr1");
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
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);
    let cprog_pid = Pid::from_raw(child.id() as i32);

    let bar: [u8; 3] = [0xE2, 0x96, 0x88]; // █
    // Without this the only exit from the read loop below is EOF on the master, so a
    // teardown wedge hangs `cargo test` instead of failing it (#61 D).
    let dog = Watchdog::arm(child.id() as i32, 20);

    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut signaled = false;
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => {
                out.extend_from_slice(&buf[..n]);
                // Only once the footer is up, so the teardown that does *not* happen is the
                // teardown that would otherwise have had something to clear.
                if !signaled && out.windows(3).any(|w| w == bar) {
                    kill(cprog_pid, Signal::SIGUSR1).unwrap();
                    signaled = true;
                }
            }
        }
    }
    let status = child.wait().unwrap();
    dog.disarm();
    assert!(!dog.hung(), "cprog hung: the watchdog had to kill it");

    assert!(signaled, "footer never appeared, so nothing was exercised");
    assert_eq!(
        status.signal(),
        Some(Signal::SIGUSR1 as i32),
        "an unregistered signal keeps its default action — cprog neither catches nor relays it"
    );
    let last_bar = rfind(&out, &bar).expect("saw a bar");
    assert!(
        !out[last_bar + bar.len()..].windows(4).any(|w| w == b"\r\x1b[K"),
        "no teardown erase is expected here: dying of an uncaught signal leaves the footer up"
    );
}

/// The single-letter state field of `/proc/<pid>/stat` (`T` = stopped), or `None` if the process
/// is gone. Read past the comm field, which may itself contain spaces or parentheses.
fn proc_state(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rfind(") ")? + 2;
    stat[after_comm..].chars().next()
}

#[test]
fn a_stopped_cp_is_continued_so_the_forwarded_signal_can_land() {
    // exceptions A8, and the half nothing held (#59): the signal cprog forwards to `cp` is sent
    // **with a `SIGCONT` companion**. A stopped process does not act on SIGTERM — only SIGCONT or
    // SIGKILL wakes it — so without the companion cp stays stopped, its pipes stay open, and the
    // reader joins never finish. Every existing test signals a *running* cp, where the companion
    // is a no-op, which is why deleting it left all 261 tests green.
    //
    // Same rig as `sigterm_to_cprog_alone_terminates_without_hanging`: a FIFO with no data pins cp
    // in `read`, so its state is deterministic rather than a copy-speed race.
    let tmp = TmpDir::new("stopped");
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
        .process_group(0) // cprog alone, so the signal does not reach cp by the group
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);
    let cprog_pid = Pid::from_raw(child.id() as i32);

    // A hang here means the stopped cp was never continued — the bug this test exists for.
    let dog = Watchdog::arm(cprog_pid.as_raw(), 8);

    let _writer = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();

    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut stopped_cp = None;
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => {
                out.extend_from_slice(&buf[..n]);
                // The footer's cursor-hide means cp is running and the slow timer has fired.
                if stopped_cp.is_none() && common::contains(&out, b"\x1b[?25l") {
                    let cp = Pid::from_raw(cp_child_of(cprog_pid.as_raw()).expect("cp child"));
                    kill(cp, Signal::SIGSTOP).unwrap(); // the state A8 exists for
                    // `kill` returns once the signal is queued, so wait for the stop to actually
                    // take effect. Forwarding to a cp that has not stopped yet would prove
                    // nothing — it would be the ordinary path every other test already covers.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                    while std::time::Instant::now() < deadline && proc_state(cp.as_raw()) != Some('T') {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    assert_eq!(proc_state(cp.as_raw()), Some('T'), "cp did not stop");
                    stopped_cp = Some(cp);
                    kill(cprog_pid, Signal::SIGTERM).unwrap();
                }
            }
        }
    }
    let status = child.wait().unwrap();
    dog.disarm();

    let cp = stopped_cp.expect("cp never started, so the scenario was not exercised");
    // Only if that pid is still cp: on the failure path cp is left stopped and PDEATHSIG cannot
    // reach it, so something must clean it up — but a reaped pid can be reused, and killing a
    // stranger is worse than leaking. The comm check makes the cleanup safe either way.
    if std::fs::read_to_string(format!("/proc/{}/comm", cp.as_raw())).is_ok_and(|c| c.trim() == "cp") {
        let _ = kill(cp, Signal::SIGKILL);
    }
    assert!(
        !dog.hung(),
        "cprog hung: a stopped cp was never continued, so its pipes never closed"
    );
    assert!(status.signal().is_some(), "cprog should still exit signaled, got {status:?}");
}
