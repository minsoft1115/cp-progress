#![cfg(feature = "integration")]
//! Managed-mode integration test over a real PTY with a real `cp` (docs/testing.md B2).
//!
//! This is the set of tests that proves the live pipeline end to end: attach cprog's stdout/stderr
//! to a pseudo-terminal so it enters managed mode, copy under a tiny slow threshold, and confirm
//! from the master side that the injected `-v` lines streamed *during* the copy and the footer
//! bar was drawn — then cleared, with a `✓` summary. The multi-file test additionally proves the
//! *during* property by ordering: a bar must be drawn *between* consecutive `-v` lines, which a
//! block-buffered flush-at-end could not satisfy.

use std::fs::File;
use std::os::fd::OwnedFd;
use std::process::{Command, Stdio};

use nix::pty::{openpty, Winsize};

mod common;
use common::{read_retry, TmpDir};

#[test]
fn managed_streams_verbose_and_draws_footer_over_pty() {
    let tmp = TmpDir::new("stream");
    let src = tmp.0.join("src.bin");
    let dst = tmp.0.join("dst.bin");
    // Large enough that the copy spans several render ticks even on fast storage.
    std::fs::write(&src, vec![0u8; 256 * 1024 * 1024]).unwrap();

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).expect("openpty");
    let slave_out: OwnedFd = pty.slave.try_clone().unwrap();
    let slave_err: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg(&src)
        .arg(&dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI") // managed requires CI unset
        // Force every file to count as "slow" and sample/redraw briskly so the footer shows.
        .env("CPROG_SLOW_THRESHOLD_MS", "1")
        .env("CPROG_SAMPLE_INTERVAL_MS", "5")
        .env("CPROG_RENDER_TICK_MS", "5")
        .stdin(Stdio::null())
        .stdout(Stdio::from(slave_out))
        .stderr(Stdio::from(slave_err))
        .spawn()
        .expect("spawn cprog");

    // Parent must hold no slave fd, or the master never sees EOF.
    drop(pty.slave);

    // Drain the master as cp writes (also unblocks cp), until the slave side closes.
    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    let status = child.wait().expect("wait cprog");

    assert!(status.success(), "cprog exited non-zero");
    assert_eq!(std::fs::read(&dst).unwrap().len(), 256 * 1024 * 1024, "copy completed");

    // The injected -v line streamed live (block-buffering would have hidden it until the end).
    assert!(out.windows(2).any(|w| w == b"->"), "expected a streamed -v line");
    // The footer bar was drawn during the copy (full-block glyph, U+2588 = E2 96 88).
    assert!(
        out.windows(3).any(|w| w == [0xE2, 0x96, 0x88]),
        "expected footer bar glyphs during the copy"
    );
    // The footer was cleared and a success summary printed (✓ = E2 9C 93).
    assert!(
        out.windows(3).any(|w| w == [0xE2, 0x9C, 0x93]),
        "expected a ✓ summary on completion"
    );
}

#[test]
fn managed_verbose_lines_interleave_with_footer_during_copy() {
    // docs/testing.md B2, strengthened. The single-file test only proves a `->` exists
    // *somewhere*, which a block-buffered flush-at-end would also satisfy. Copying several files
    // proves the `-v` lines arrive *during* the copy: cp prints each `'src' -> 'dst'` line before
    // copying that file, so with live `stdbuf -oL` streaming the footer bar is redrawn *between*
    // consecutive `->` lines. If line-buffering broke, all `-v` lines would flush together at cp
    // exit (contiguous arrows, no bar between them) and this assertion fails.
    let tmp = TmpDir::new("interleave");
    let dst = tmp.0.join("out");
    std::fs::create_dir_all(&dst).unwrap();
    const N: usize = 4;
    const EACH: usize = 64 * 1024 * 1024;

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).expect("openpty");
    let slave_out: OwnedFd = pty.slave.try_clone().unwrap();
    let slave_err: OwnedFd = pty.slave.try_clone().unwrap();

    // Scope the Command so it is dropped before draining: a named Command retains its Stdio fds
    // (dup'd PTY slave) for a possible re-spawn, which would keep the slave open in the parent
    // and the master would never see EOF. Dropping it here leaves the child as the sole holder.
    let mut child = {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cprog"));
        for i in 0..N {
            let f = tmp.0.join(format!("src{i}.bin"));
            std::fs::write(&f, vec![0u8; EACH]).unwrap();
            cmd.arg(&f);
        }
        cmd.arg(&dst); // multi-source -> dest must be an existing directory
        cmd.env("TERM", "xterm")
            .env("LC_ALL", "C.UTF-8")
            .env_remove("CI")
            .env("CPROG_SLOW_THRESHOLD_MS", "1")
            .env("CPROG_SAMPLE_INTERVAL_MS", "5")
            .env("CPROG_RENDER_TICK_MS", "5")
            .stdin(Stdio::null())
            .stdout(Stdio::from(slave_out))
            .stderr(Stdio::from(slave_err))
            .spawn()
            .expect("spawn cprog")
    };
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
    let status = child.wait().expect("wait cprog");

    assert!(status.success(), "cprog exited non-zero");
    for i in 0..N {
        assert_eq!(
            std::fs::read(dst.join(format!("src{i}.bin"))).unwrap().len(),
            EACH,
            "file {i} copied fully"
        );
    }

    let first = out.windows(2).position(|w| w == b"->").expect("at least one -v line");
    let last = out.windows(2).rposition(|w| w == b"->").expect("at least one -v line");
    assert!(last > first, "multi-file copy should emit multiple -v lines");
    // A footer bar glyph (U+2588 = E2 96 88) must appear *between* the first and last `-v` line:
    // proof cprog kept rendering live between file boundaries, rather than the `-v` stream landing
    // as one block at the end (which stdbuf -oL exists to prevent).
    assert!(
        out[first..last].windows(3).any(|w| w == [0xE2, 0x96, 0x88]),
        "expected a footer redraw between streamed -v lines (live stdbuf -oL streaming)"
    );
}

#[test]
fn managed_relays_cp_error_and_preserves_exit_code() {
    // docs/testing.md D1: cp fails under managed mode -> cprog relays the error, and returns
    // cp's exit code with a ✗ summary (no footer residue).
    let tmp = TmpDir::new("cperr");
    let src = tmp.0.join("src.bin");
    std::fs::write(&src, b"hi").unwrap();
    let bad_dst = tmp.0.join("no-such-dir").join("dst.bin"); // parent dir does not exist

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).unwrap();
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg(&src)
        .arg(&bad_dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI")
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

    assert_eq!(status.code(), Some(1), "cprog returns cp's exit code");
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("No such file") || text.contains("cannot create"),
        "cp's error was relayed: {text:?}"
    );
    // cp fails instantly, so no footer ever engaged -> the general gate suppresses the summary
    // (cp's own error above already explains it).
    assert!(!text.contains('✗'), "no summary when nothing was monitored: {text:?}");
}

#[test]
fn help_over_pty_passes_through_but_names_cprog() {
    // `--help` prints and exits without copying; even in a terminal cprog must pass through —
    // no footer, no `✓ done` summary (docs/bugs bug2). Over a PTY managed would otherwise engage.
    // It does append one line naming itself, because `--version`/`--help` reach `cp` untouched
    // and would otherwise leave no trace of the wrapper at all (#15).
    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).unwrap();
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg("--help")
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI")
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
    let text = String::from_utf8_lossy(&out);

    assert_eq!(status.code(), Some(0));
    assert!(text.contains("Usage: cp"), "cp's help was shown: {:.80?}", text);
    assert!(!text.contains('✓'), "no summary for --help: {text:?}");
    // The version line is the last thing on screen, after cp has had its say.
    let line = format!("cprog {}", env!("CARGO_PKG_VERSION"));
    assert!(text.contains(&line), "cprog names itself on a terminal: {text:?}");
    assert!(
        text.rfind(&line) > text.rfind("Usage: cp"),
        "the line comes after cp's output, not before it: {text:?}"
    );
}
