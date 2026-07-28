#![cfg(feature = "integration")]
//! stdbuf-missing fallback integration test (docs/testing.md B1/E3).
//!
//! Even over a PTY (where managed mode would otherwise engage), a `PATH` without `stdbuf`
//! must drop cprog to passthrough: no footer bar, no summary — byte-identical to `cp`. The same
//! PTY + large file + tiny threshold reliably draws a bar in managed mode (see tests/managed),
//! so their absence here proves the fallback.

use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use nix::pty::{openpty, Winsize};

mod common;
use common::{read_retry, TmpDir};

/// Locate a tool on the current PATH.
fn find_on_path(tool: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|d| d.join(tool))
        .find(|p| p.exists())
        .unwrap_or_else(|| panic!("{tool} on PATH"))
}

/// Locate the real `cp` on the current PATH.
fn find_cp() -> PathBuf {
    find_on_path("cp")
}

#[test]
fn missing_stdbuf_falls_back_to_passthrough() {
    let tmp = TmpDir::new("nostdbuf");
    let src = tmp.0.join("src.bin");
    let dst = tmp.0.join("dst.bin");
    std::fs::write(&src, vec![0u8; 256 * 1024 * 1024]).unwrap();

    // A PATH that has `cp` but not `stdbuf`, so the managed feature-detect fails.
    let bindir = tmp.0.join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    std::os::unix::fs::symlink(find_cp(), bindir.join("cp")).unwrap();

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).unwrap();
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg(&src)
        .arg(&dst)
        .env("TERM", "xterm")
        .env_remove("CI")
        .env("PATH", &bindir) // stdbuf is not here -> managed cannot engage
        .env("CPROG_SLOW_THRESHOLD_MS", "1")
        .env("CPROG_SAMPLE_INTERVAL_MS", "5")
        .env("CPROG_RENDER_TICK_MS", "5")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);

    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    let status = child.wait().unwrap();

    assert!(status.success(), "copy should still succeed");
    assert_eq!(std::fs::read(&dst).unwrap().len(), 256 * 1024 * 1024);
    // Passthrough: no footer bar (█) and no summary (✓) — cprog emits nothing of its own.
    assert!(!out.windows(3).any(|w| w == [0xE2, 0x96, 0x88]), "no bar in passthrough");
    assert!(!out.windows(3).any(|w| w == [0xE2, 0x9C, 0x93]), "no summary in passthrough");
}

#[test]
fn stdbuf_present_but_cp_missing_surfaces_as_cp_exiting_127() {
    // exceptions C7, and the asymmetry is the point. With no `cp` at all the *spawn* fails and
    // cprog reports Fatal::CpSpawn (C1, tests/exit_contract.rs). Here `stdbuf` is on PATH, so
    // managed mode engages and the spawn **succeeds** — it is `stdbuf` that then cannot exec
    // `cp`, prints its own diagnosis and exits 127. From cprog's side that is indistinguishable
    // from "cp exited 127", and it must be reported as such: cp's result is the final authority,
    // and cprog has no business inventing a Fatal for a child that started fine.
    let tmp = TmpDir::new("nocp");
    let src = tmp.0.join("src.bin");
    let dst = tmp.0.join("dst.bin");
    std::fs::write(&src, b"payload").unwrap();

    // A PATH with stdbuf but deliberately without cp.
    let bindir = tmp.0.join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    std::os::unix::fs::symlink(find_on_path("stdbuf"), bindir.join("stdbuf")).unwrap();
    assert!(!bindir.join("cp").exists(), "precondition: cp is not reachable");

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
        .env("PATH", &bindir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);

    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    let status = child.wait().unwrap();

    assert_eq!(status.code(), Some(127), "cprog returns the child's 127 verbatim");
    let text = String::from_utf8_lossy(&out);
    // stdbuf's own message is relayed, so the user is told what is actually wrong:
    //   stdbuf: failed to run command 'cp': No such file or directory
    // Matching on "stdbuf" specifically, not a loose "cp" — "cprog" contains "cp", so the loose
    // form would also accept the very failure the next assertion rules out.
    assert!(text.contains("stdbuf"), "the child's own diagnosis must reach the screen: {text:?}");
    // And the failure this distinguishes it from: had managed mode not engaged, cprog would have
    // exec'd `cp`, failed, and reported Fatal::CpSpawn — also exit 127, but naming the wrapper
    // for a failure that is not the wrapper's (C1). The exit code alone cannot tell them apart.
    assert!(!text.contains("cprog:"), "cprog must not invent a Fatal of its own: {text:?}");
    assert!(!dst.exists(), "nothing was copied");
}

#[test]
fn unreadable_path_entry_reads_as_stdbuf_missing() {
    // exceptions B8, the "cannot tell" half. A PATH directory without search permission makes
    // `stdbuf` unstattable, so the probe answers false and cprog concludes it is not installed.
    // Optimism would be worse — it would enter managed mode and then fail to keep the live-UI
    // promise `stdbuf` exists to guarantee — but the choice is a choice, and it was not written
    // down until now.
    if rustix::process::geteuid().is_root() {
        return; // root bypasses the permission bits, so there is nothing to observe
    }

    let tmp = TmpDir::new("unreadable");
    let src = tmp.0.join("src.bin");
    let dst = tmp.0.join("dst.bin");
    std::fs::write(&src, vec![0u8; 200 * 1024 * 1024]).unwrap();

    // Two directories on PATH. The *only* copy of stdbuf lives in the one that gets closed off;
    // cp lives in the readable one, so the copy can still run once cprog decides on passthrough.
    // Appending the real PATH would defeat the test — the probe would simply find stdbuf
    // elsewhere, which is correct behaviour and not the rule under test.
    let hidden = tmp.0.join("hidden");
    let plain = tmp.0.join("plain");
    std::fs::create_dir_all(&hidden).unwrap();
    std::fs::create_dir_all(&plain).unwrap();
    std::os::unix::fs::symlink(find_on_path("stdbuf"), hidden.join("stdbuf")).unwrap();
    std::os::unix::fs::symlink(find_cp(), plain.join("cp")).unwrap();
    assert!(hidden.join("stdbuf").exists(), "precondition: stdbuf is reachable while readable");
    let bindir = hidden;
    // Restore on *every* path, including an unwind: a directory left at mode 000 would make
    // TmpDir's own cleanup fail and leave it behind after a red run.
    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    std::fs::set_permissions(&bindir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let _restore = Restore(bindir.clone());
    if bindir.join("stdbuf").metadata().is_ok() {
        return; // some filesystems ignore the mode; the precondition does not hold here
    }

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).unwrap();
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let path = format!("{}:{}", bindir.display(), plain.display());
    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg(&src)
        .arg(&dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI")
        .env("PATH", &path)
        .env("CPROG_SLOW_THRESHOLD_MS", "1")
        .env("CPROG_SAMPLE_INTERVAL_MS", "5")
        .env("CPROG_RENDER_TICK_MS", "5")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);

    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    let status = child.wait().unwrap();

    assert!(status.success(), "the copy still succeeds via passthrough: {status:?}");
    assert_eq!(std::fs::read(&dst).unwrap().len(), 200 * 1024 * 1024, "and really happened");
    assert!(!out.windows(3).any(|w| w == [0xE2, 0x96, 0x88]), "no bar: the probe said no stdbuf");
    assert!(!out.windows(3).any(|w| w == [0xE2, 0x9C, 0x93]), "and no summary");
}
