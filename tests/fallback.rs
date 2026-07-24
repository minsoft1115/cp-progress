#![cfg(feature = "integration")]
//! stdbuf-missing fallback integration test (docs/testing.md B1/E3).
//!
//! Even over a PTY (where managed mode would otherwise engage), a `PATH` without `stdbuf`
//! must drop cprog to passthrough: no footer bar, no summary — byte-identical to `cp`. The same
//! PTY + large file + tiny threshold reliably draws a bar in managed mode (see tests/managed),
//! so their absence here proves the fallback.

use std::fs::File;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use nix::pty::{openpty, Winsize};

mod common;
use common::read_retry;

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cprog_fb_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Locate the real `cp` on the current PATH.
fn find_cp() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|d| d.join("cp"))
        .find(|p| p.exists())
        .expect("cp on PATH")
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
