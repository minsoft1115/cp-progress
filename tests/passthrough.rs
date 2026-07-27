#![cfg(feature = "integration")]
//! Integration tests for the passthrough path of the `cprog` binary (docs/testing.md).
//!
//! These exercise the real binary against the real `cp`, covering the core contract:
//! preserve `cp`'s exit code, and stay byte-identical to `cp` when not managed.

use std::ffi::OsStr;
use std::process::{Command, Output};

mod common;
use common::TmpDir;

fn cprog<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_cprog"))
        .args(args)
        .output()
        .expect("run cprog")
}

fn cp<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("cp").args(args).output().expect("run cp")
}

#[test]
fn no_args_prints_usage_and_exits_1() {
    // docs/testing.md D5.
    let out = cprog(std::iter::empty::<&OsStr>());
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("usage"), "stderr was {err:?}");
}

#[test]
fn copies_a_file_and_preserves_exit_zero() {
    // docs/testing.md D6: cp exits 0 -> cprog returns 0, and the copy really happened.
    let tmp = TmpDir::new("copy_ok");
    let src = tmp.path("src.bin");
    let dst = tmp.path("dst.bin");
    let data = vec![0xABu8; 64 * 1024];
    std::fs::write(&src, &data).unwrap();

    let out = cprog([src.as_os_str(), dst.as_os_str()]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(std::fs::read(&dst).unwrap(), data);
}

#[test]
fn preserves_nonzero_exit_on_cp_failure() {
    // docs/testing.md D6/D1: a missing source makes cp fail; cprog returns cp's code.
    let tmp = TmpDir::new("copy_fail");
    let missing = tmp.path("does-not-exist");
    let dst = tmp.path("dst");

    let mine = cprog([missing.as_os_str(), dst.as_os_str()]);
    let theirs = cp([missing.as_os_str(), dst.as_os_str()]);
    assert_ne!(mine.status.code(), Some(0));
    assert_eq!(mine.status.code(), theirs.status.code(), "same exit code as cp");
    // E1: passthrough leaves the environment untouched, so cp's error text (locale included)
    // is byte-identical to running cp directly. A leaked LC_ALL=C would diverge here.
    assert_eq!(mine.stderr, theirs.stderr, "cp's error output is byte-identical");
}

#[test]
fn passthrough_output_is_byte_identical_to_cp() {
    // docs/testing.md E1/E3: not a TTY here -> passthrough. With a user-supplied -v, cprog's
    // (inherited) stdout/stderr must match cp's exactly — same env, same quoting, same code.
    let tmp = TmpDir::new("identical");
    let src = tmp.path("s.bin");
    std::fs::write(&src, b"hello cprog").unwrap();
    let dst_mine = tmp.path("d_mine.bin");
    let dst_theirs = tmp.path("d_theirs.bin");

    let mine = cprog(["-v".as_ref(), src.as_os_str(), dst_mine.as_os_str()]);
    let theirs = cp(["-v".as_ref(), src.as_os_str(), dst_theirs.as_os_str()]);

    assert_eq!(mine.status.code(), theirs.status.code());
    // The -v line differs only in the destination file name; strip the dst path to compare.
    let norm = |bytes: &[u8], dst: &std::path::Path| {
        String::from_utf8_lossy(bytes).replace(dst.to_str().unwrap(), "DST")
    };
    assert_eq!(
        norm(&mine.stdout, &dst_mine),
        norm(&theirs.stdout, &dst_theirs),
        "passthrough stdout matches cp"
    );
    assert_eq!(mine.stderr, theirs.stderr, "passthrough stderr matches cp");
}

#[test]
fn informational_output_stays_byte_identical_when_not_a_terminal() {
    // #15: cprog names itself after `--help`/`--version`, but only on a terminal. With output
    // captured — a pipe, a redirect, any script — both streams must still match `cp` exactly,
    // or `alias cp='cprog'` would change what every `cp --version | …` in the system sees.
    for flag in ["--version", "--help"] {
        let ours = cprog([flag]);
        let theirs = cp([flag]);
        assert_eq!(ours.status.code(), theirs.status.code(), "{flag}: exit code");
        assert_eq!(ours.stdout, theirs.stdout, "{flag}: stdout byte-identical");
        assert_eq!(ours.stderr, theirs.stderr, "{flag}: stderr byte-identical");
        assert!(
            !String::from_utf8_lossy(&ours.stderr).contains("cprog "),
            "{flag}: no version line when stderr is not a tty"
        );
    }
}
