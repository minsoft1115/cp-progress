//! `cp` spawn (managed wraps with `stdbuf -oL` + `-v`), wait, and PID capture
//! (docs/process-model.md).
//!
//! The command to run is assembled purely as a [`CommandSpec`] so argument/env/stdio
//! decisions are unit-tested; [`spawn`] turns a spec into a live child. In managed mode
//! `stdbuf` execs `cp`, so the child's pid *is* `cp`'s pid and `/proc/<pid>/fd` stays valid
//! (docs/testing.md D7). Passthrough leaves the environment untouched so output stays
//! byte-identical to `cp` (docs/testing.md E1).

use std::ffi::OsString;
use std::io;
use std::process::{Child, Command, Stdio};

/// A fully-resolved plan for launching the child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// The program to exec (`stdbuf` in managed mode, `cp` in passthrough).
    pub program: OsString,
    /// The full argument vector following the program.
    pub args: Vec<OsString>,
    /// Environment variables to set (managed only; empty for passthrough).
    pub env: Vec<(&'static str, &'static str)>,
    /// Whether to capture stdout/stderr via pipes (managed) or inherit them (passthrough).
    pub capture: bool,
}

impl CommandSpec {
    /// Managed launch: `stdbuf -oL cp [-v] <cp args…>` with `QUOTING_STYLE` pinned (the
    /// locale deliberately untouched) and captured stdout/stderr. `-v` is injected only when
    /// the user did not pass it.
    pub fn managed(cp_args: &[OsString], verbose_present: bool) -> Self {
        let mut args = vec![OsString::from("-oL"), OsString::from("cp")];
        if !verbose_present {
            args.push(OsString::from("-v"));
        }
        args.extend(cp_args.iter().cloned());
        Self {
            program: OsString::from("stdbuf"),
            args,
            // shell-escape keeps a name containing a newline on a single line, independent of
            // coreutils version. We deliberately do NOT set LC_ALL=C: cprog never parses -v, so
            // locale stability buys nothing, and forcing C would render non-ASCII file names
            // (and cp's messages) as octal escapes in the relayed output. Managed only —
            // passthrough must not alter env (docs/capture-and-verbose.md).
            env: vec![("QUOTING_STYLE", "shell-escape")],
            capture: true,
        }
    }

    /// Passthrough launch: `cp <args…>` verbatim, streams inherited, environment untouched.
    pub fn passthrough(cp_args: &[OsString]) -> Self {
        Self {
            program: OsString::from("cp"),
            args: cp_args.to_vec(),
            env: Vec::new(),
            capture: false,
        }
    }
}

/// Replace the current process with the one described by `spec` (passthrough only): on
/// success this never returns — the pid cprog held *is* `cp` from here on, so exit codes,
/// signals, job control and `$!` reach the shell with no relaying (docs/process-model.md
/// "Passthrough 생명주기"). Only the exec failure (e.g. no `cp` on PATH) comes back.
///
/// Two deliberate differences from [`spawn`]:
/// * **No `PR_SET_PDEATHSIG`** — there is no cprog left whose death could orphan `cp`, and a
///   setting inherited across the exec would tie `cp`'s life to the *shell* instead, which a
///   plain `cp` does not have (docs/exceptions.md C4).
/// * **SIGPIPE is restored to its default first.** The Rust runtime ignores SIGPIPE, an
///   ignored disposition survives exec, and an exec'd `cp` that cannot die of SIGPIPE reports
///   write errors instead — visibly different from plain `cp` in a pipeline (exceptions A7).
pub fn exec_replace(spec: &CommandSpec) -> io::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    // SAFETY: restoring a signal disposition to its default is a valid, async-signal-safe call.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let err = cmd.exec();
    // The exec failed and cprog lives on to report it. Put the Rust runtime's SIGPIPE-ignore
    // back, or the best-effort Fatal write to a broken-pipe stderr would now kill the process
    // with SIGPIPE instead of surfacing EPIPE — clobbering the exit-127 contract
    // (tests/exit_contract.rs::failed_exec_with_broken_stderr_still_exits_127).
    // SAFETY: as above.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    err
}

/// Spawn the child described by `spec`. On success, `child.id()` is `cp`'s pid.
pub fn spawn(spec: &CommandSpec) -> io::Result<Child> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    if spec.capture {
        // stdin stays inherited; only stdout/stderr are piped for sole-writer relay.
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    }

    // If cprog dies, don't leave cp orphaned (docs/process-model.md). Best-effort: a failure to
    // set the death signal must not abort the copy, so the pre_exec error is swallowed.
    #[cfg(target_os = "linux")]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong, 0, 0, 0);
            Ok(())
        });
    }

    cmd.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn args(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn managed_wraps_stdbuf_and_injects_verbose() {
        let spec = CommandSpec::managed(&args(&["a", "b"]), false);
        assert_eq!(spec.program, OsString::from("stdbuf"));
        assert_eq!(spec.args, args(&["-oL", "cp", "-v", "a", "b"]));
        assert!(spec.capture, "managed captures stdout/stderr");
    }

    #[test]
    fn managed_does_not_double_inject_verbose() {
        // docs/testing.md B8: the user already passed -v; do not add a second one.
        let spec = CommandSpec::managed(&args(&["-v", "a", "b"]), true);
        assert_eq!(spec.args, args(&["-oL", "cp", "-v", "a", "b"]));
    }

    #[test]
    fn managed_sets_only_quoting_style_not_locale() {
        // docs/capture-and-verbose.md: shell-escape keeps a newline-in-name on one line. We do
        // NOT force LC_ALL=C — cprog never parses -v, and forcing the C locale would garble
        // non-ASCII file names (and cp's messages) in the relayed output.
        let spec = CommandSpec::managed(&args(&["a", "b"]), false);
        assert_eq!(spec.env, vec![("QUOTING_STYLE", "shell-escape")]);
    }

    #[test]
    fn passthrough_runs_cp_verbatim() {
        let spec = CommandSpec::passthrough(&args(&["-a", "-i", "x", "y"]));
        assert_eq!(spec.program, OsString::from("cp"));
        assert_eq!(spec.args, args(&["-a", "-i", "x", "y"]));
        assert!(!spec.capture, "passthrough inherits streams");
    }

    #[test]
    fn passthrough_never_touches_env() {
        // docs/testing.md E1: passthrough leaves env untouched so cp's output — including
        // localized error messages — stays byte-identical.
        let spec = CommandSpec::passthrough(&args(&["x", "y"]));
        assert!(spec.env.is_empty());
    }
}
