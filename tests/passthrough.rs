#![cfg(feature = "integration")]
//! Integration tests for the passthrough path of the `cprog` binary (docs/testing.md).
//!
//! These exercise the real binary against the real `cp`, covering the core contract:
//! preserve `cp`'s exit code, and stay byte-identical to `cp` when not managed.

use std::ffi::OsStr;
use std::process::{Command, Output, Stdio};

mod common;
use common::{open_fifo_writer, TmpDir};

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
    // exceptions C2: a failing cp keeps its exit code, and its stderr reaches the user unchanged.
    //
    // Not what this shows: environment purity. `cp: cannot stat …: No such file or directory` is
    // byte-identical under `C` and `en_US.UTF-8`, so injecting `LC_ALL=C` into the exec path
    // leaves this test green (measured). The detector for a leak is
    // `informational_output_stays_byte_identical_when_not_a_terminal`, whose `--version` output
    // carries a non-ASCII author name — the old comment credited this test with it (#61).
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
fn scan_error_falls_back_to_passthrough_byte_identical() {
    // docs/runtime-model.md "인자 검사 자체가 실패해도 보수적으로 passthrough" / exceptions B13.
    // `--suffix` without a value fails cprog's own scan; the args must still reach cp untouched,
    // which prints its own diagnosis — same exit code, byte-identical output. This is the only
    // test exercising the Scan -> passthrough wiring in dispatch (the args unit tests stop at
    // inspect returning the error).
    let ours = cprog(["--suffix"]);
    let theirs = cp(["--suffix"]);
    assert_ne!(ours.status.code(), Some(0), "cp itself rejects the lone --suffix");
    assert_eq!(ours.status.code(), theirs.status.code(), "same exit code as cp");
    assert_eq!(ours.stdout, theirs.stdout, "stdout byte-identical");
    assert_eq!(ours.stderr, theirs.stderr, "cp's own diagnosis, not a cprog error");
}

#[test]
fn passthrough_execs_cp_replacing_the_process() {
    // docs/process-model.md "Passthrough 생명주기" / testing.md E5: no version notice is due
    // here (not informational), so cprog must exec cp rather than spawn it — same pid, no
    // wrapper process left, `$!` and signals reach cp itself. A FIFO source keeps cp blocked
    // in open() long enough to observe the replacement from /proc.
    let tmp = TmpDir::new("execpid");
    let fifo = tmp.path("src.fifo");
    let dst = tmp.path("dst.bin");
    let cfifo = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a fresh path inside our own temp dir; failure is reported below.
    assert_eq!(unsafe { libc::mkfifo(cfifo.as_ptr(), 0o600) }, 0, "mkfifo");

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg(&fifo)
        .arg(&dst)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // The exec is near-instant; poll briefly until the pid's comm reads "cp".
    let mut comm = String::new();
    for _ in 0..200 {
        comm = std::fs::read_to_string(format!("/proc/{}/comm", child.id())).unwrap_or_default();
        if comm.trim_end() == "cp" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Unblock cp: open the write end and drop it at once -> EOF -> empty copy, exit 0.
    //
    // `open_fifo_writer` rather than a plain open, because a blocking write-open on a FIFO waits
    // for a reader **forever**. If the exec path regresses and no cp ever opens the read end, this
    // line used to wedge the whole `cargo test` run instead of failing it — a hang is the one
    // failure mode a suite cannot report (#61 D). `ENXIO` is exactly "no reader", so it becomes
    // the assertion below. This test had that dance inline first; #69 A needed the same rule at
    // eight more sites and moved it to `common`, so this one now shares it (#69 C).
    let writer = open_fifo_writer(&fifo);
    assert!(
        writer.is_some(),
        "no reader ever opened the FIFO, so cp was never exec'd (comm was {comm:?})"
    );
    drop(writer);
    let status = child.wait().unwrap();

    assert_eq!(comm.trim_end(), "cp", "the spawned pid must have been replaced by cp");
    assert!(status.success(), "the empty copy still succeeds: {status:?}");
}

#[test]
fn verbose_to_a_closed_pipe_dies_of_sigpipe_like_cp() {
    // exceptions A7 / testing.md E6. Rust ignores SIGPIPE in cprog itself and an ignored
    // disposition survives exec — left alone, the exec'd cp would report a write error and
    // exit 1 instead of dying silently of SIGPIPE the way plain cp does in a pipeline.
    // Both must end the same way, stderr included.
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::ExitStatusExt;
    let tmp = TmpDir::new("sigpipe");
    let src = tmp.path("src.bin");
    std::fs::write(&src, b"data").unwrap();

    let run = |prog: &str, dst: &std::path::Path| {
        // A pipe whose read end is closed: the first stdout write (cp's -v line) breaks.
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe()");
        assert_eq!(unsafe { libc::close(fds[0]) }, 0, "close read end");
        // SAFETY: fds[1] is a fresh, owned pipe fd handed to the child as its stdout.
        let stdout = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        Command::new(prog)
            .arg("-v")
            .arg(&src)
            .arg(dst)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .output()
            .expect("run")
    };
    let ours = run(env!("CARGO_BIN_EXE_cprog"), &tmp.path("d_mine"));
    let theirs = run("cp", &tmp.path("d_theirs"));

    assert_eq!(
        ours.status.signal(),
        Some(libc::SIGPIPE),
        "cprog (exec'd into cp) must die of SIGPIPE, not report a write error: {:?}",
        String::from_utf8_lossy(&ours.stderr)
    );
    assert_eq!(ours.status.signal(), theirs.status.signal(), "same signal death as plain cp");
    assert_eq!(ours.stderr, theirs.stderr, "no extra error output");
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

#[test]
fn a_non_utf8_argument_reaches_cp_unchanged() {
    // exceptions G8. cprog scans the argument vector with lexopt but never rebuilds it — the
    // original `OsString`s go to `cp`. A path that is not valid UTF-8 is legal on Linux, so
    // this is not a curiosity: lossy handling anywhere in the scan would corrupt a real file
    // name, and the passthrough contract says the outcome must be `cp`'s, byte for byte.
    use std::os::unix::ffi::OsStrExt;

    let tmp = TmpDir::new("nonutf8");
    // 0xFF is not valid UTF-8 in any position.
    let name = OsStr::from_bytes(b"src-\xFF-name.bin");
    let src = tmp.0.join(name);
    let dst = tmp.0.join(OsStr::from_bytes(b"dst-\xFF-name.bin"));
    std::fs::write(&src, b"payload").unwrap();
    assert!(src.to_str().is_none(), "precondition: the path is not representable as UTF-8");

    let ours = cprog([src.as_os_str(), dst.as_os_str()]);
    assert_eq!(ours.status.code(), Some(0), "stderr={:?}", String::from_utf8_lossy(&ours.stderr));
    assert_eq!(std::fs::read(&dst).unwrap(), b"payload", "the copy landed at the exact name");
    assert!(ours.stdout.is_empty() && ours.stderr.is_empty(), "a plain copy says nothing");

    // And the failing shape: cp's own diagnosis must come through unaltered, which is where a
    // lossy round-trip of the name would show up.
    let missing = tmp.0.join(OsStr::from_bytes(b"missing-\xFF.bin"));
    let ours = cprog([missing.as_os_str(), dst.as_os_str()]);
    let theirs = cp([missing.as_os_str(), dst.as_os_str()]);
    assert_eq!(ours.status.code(), theirs.status.code());
    assert_eq!(ours.stdout, theirs.stdout);
    // `cp` escapes the byte in its own message (`$'\377'`), so the assertion is byte-identity
    // with cp — not the presence of a raw 0xFF, which cp never emits. Non-emptiness keeps the
    // comparison from passing vacuously.
    assert!(!theirs.stderr.is_empty(), "cp did diagnose the missing file");
    assert_eq!(ours.stderr, theirs.stderr, "cp's message, byte for byte");
}

#[test]
fn a_non_utf8_name_in_a_verbose_line_matches_cp_exactly() {
    // exceptions G8, the `-v` shape: the name has to survive as bytes all the way into cp's own
    // output line.
    //
    // Deliberately *not* named after the flag scan. Stdout is not a terminal here, so managed
    // and passthrough are indistinguishable from the outside and this would pass whether or not
    // the scan saw `-v` at all — verified by mutation. That claim is asserted where it is
    // actually observable: `args.rs::a_non_utf8_operand_does_not_stop_the_scan`.
    use std::os::unix::ffi::OsStrExt;

    let tmp = TmpDir::new("nonutf8flags");
    let src = tmp.0.join(OsStr::from_bytes(b"s-\xFF.bin"));
    let dst = tmp.0.join(OsStr::from_bytes(b"d-\xFF.bin"));
    std::fs::write(&src, b"x").unwrap();

    let ours = cprog([OsStr::new("-v"), src.as_os_str(), dst.as_os_str()]);
    let theirs = cp([OsStr::new("-v"), src.as_os_str(), dst.as_os_str()]);
    assert_eq!(ours.status.code(), Some(0));
    // Not a terminal here, so this is passthrough: cp's own -v line, byte for byte. cp quotes
    // the unrepresentable byte itself, so identity with cp is the contract, not a raw 0xFF.
    assert!(!theirs.stdout.is_empty(), "cp did print a -v line");
    assert_eq!(ours.stdout, theirs.stdout, "the -v line matches cp exactly");
    // The name really did survive as bytes: the copy exists at the exact path.
    assert_eq!(std::fs::read(&dst).unwrap(), b"x");
}
