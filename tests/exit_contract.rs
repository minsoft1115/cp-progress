//! Exit-code contract under a hostile stderr (docs/architecture.md "에러 철학").
//!
//! cprog's own stderr writes — the `Fatal` message and the summary line — are best-effort: if the
//! write fails they must not panic, because a panic would replace cp's exit disposition with 101
//! and break the "cp's result is final" contract. `eprintln!` panics on a failed write; the fix
//! uses `let _ = writeln!(...)`. Here stderr is a broken pipe (no reader), so any write fails with
//! EPIPE (Rust ignores SIGPIPE, so it surfaces as an error rather than a signal).
//!
//! This runs in the default (non-integration) suite: it spawns only cprog — no external `cp` — and
//! exercises the empty-args `Fatal::Usage` path, which prints then returns code 1.

use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Command, Stdio};

#[test]
fn usage_exit_code_survives_a_broken_stderr_pipe() {
    // A pipe whose read end is closed: every write to the write end gets EPIPE.
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe()");
    let (read_fd, write_fd) = (fds[0], fds[1]);
    // Close the read end now (the child never needs it) so no reader exists once the child writes.
    assert_eq!(unsafe { libc::close(read_fd) }, 0, "close read end");
    // SAFETY: write_fd is a fresh, owned pipe fd handed to the child as its stderr.
    let stderr = unsafe { OwnedFd::from_raw_fd(write_fd) };

    // No args -> Fatal::Usage -> prints to (broken) stderr, then returns exit code 1.
    let status = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .status()
        .expect("spawn cprog");

    assert_eq!(
        status.code(),
        Some(1),
        "usage exit code must be preserved even when stderr is a broken pipe (a panic would give 101)"
    );
}
