//! `ExitDisposition` -> signal-preserving finalize (docs/process-model.md).
//!
//! `cp`'s wait status is the final authority. A normal exit returns its code verbatim
//! (docs/testing.md D6); a signal death is preserved so the parent shell sees a true
//! signaled exit — cprog restores the default handler and re-raises the signal, and only if
//! that is impossible falls back to the shell's `128 + signal` convention (docs/process-
//! model.md D2). This module maps the status; the actual re-raise syscalls are wired with
//! the process/orchestration layer.

use std::process::ExitStatus;

/// What cprog should do about `cp`'s termination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitDisposition {
    /// `cp` exited normally with this code — return it verbatim.
    Code(i32),
    /// `cp` was killed by this signal — re-raise it (else fall back to `128 + signal`).
    Signal(i32),
}

impl ExitDisposition {
    /// The numeric code cprog reports as a fallback: the exit code itself, or `128 + signal`.
    pub fn code(&self) -> i32 {
        match self {
            ExitDisposition::Code(n) => *n,
            ExitDisposition::Signal(s) => 128 + s,
        }
    }
}

/// Classify `cp`'s wait status into an [`ExitDisposition`].
pub fn disposition(status: ExitStatus) -> ExitDisposition {
    use std::os::unix::process::ExitStatusExt;
    match status.signal() {
        Some(sig) => ExitDisposition::Signal(sig),
        None => ExitDisposition::Code(status.code().unwrap_or(1)),
    }
}

/// Turn a disposition into cprog's exit: for a signal, re-raise it so the parent shell sees a
/// true signaled exit; for a code, return it. Falls back to `128 + signal` only if the
/// re-raise somehow fails to terminate the process.
pub fn finalize(disp: ExitDisposition) -> i32 {
    if let ExitDisposition::Signal(sig) = disp {
        reraise(sig); // normally does not return (terminates the process)
    }
    disp.code()
}

/// Restore the default handler for `signal`, unblock it, and re-raise it on ourselves so cprog
/// exits with the same signaled status `cp` did (docs/process-model.md).
///
/// Best-effort: this runs on the normal main-thread call stack (after `child.wait()`), not in a
/// signal handler, so it is ordinary code — not an async-signal-safe context. Syscall returns are
/// intentionally unchecked; if the re-raise fails to terminate, `finalize` falls back to `128 + s`.
fn reraise(signal: i32) {
    // SAFETY: POD structs zero-initialised then filled; pointers valid; Linux-only.
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        // sa_flags == 0 (no SA_SIGINFO), so sa_sigaction aliases sa_handler and SIG_DFL applies.
        act.sa_flags = 0;
        act.sa_sigaction = libc::SIG_DFL;
        libc::sigaction(signal, &act, std::ptr::null_mut());

        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, signal);
        libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());

        libc::raise(signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    #[test]
    fn normal_exit_code_is_preserved() {
        // docs/testing.md D6: cp's exit code n is returned unchanged.
        let d = disposition(ExitStatus::from_raw(0));
        assert_eq!(d, ExitDisposition::Code(0));
        assert_eq!(d.code(), 0);
    }

    #[test]
    fn nonzero_exit_code_is_preserved() {
        let d = disposition(ExitStatus::from_raw(3 << 8)); // WEXITSTATUS = 3
        assert_eq!(d, ExitDisposition::Code(3));
        assert_eq!(d.code(), 3);
    }

    #[test]
    fn signal_termination_is_detected() {
        // docs/testing.md D2: killed by SIGINT (2) -> Signal, no exit code.
        let d = disposition(ExitStatus::from_raw(2)); // WTERMSIG = 2
        assert_eq!(d, ExitDisposition::Signal(2));
    }

    #[test]
    fn signal_falls_back_to_128_plus_n() {
        // "불가하면 128 + n" (docs/process-model.md): the numeric code a shell reports.
        assert_eq!(disposition(ExitStatus::from_raw(2)).code(), 130); // SIGINT
        assert_eq!(disposition(ExitStatus::from_raw(9)).code(), 137); // SIGKILL
    }

    #[test]
    fn finalize_returns_code_verbatim() {
        // The Code path has no side effects; the Signal path (re-raise) is covered by the
        // signal integration test, since exercising it here would kill the test process.
        assert_eq!(finalize(ExitDisposition::Code(0)), 0);
        assert_eq!(finalize(ExitDisposition::Code(3)), 3);
    }
}
