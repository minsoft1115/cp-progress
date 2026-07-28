//! `ExitDisposition` -> signal-preserving finalize (docs/process-model.md).
//!
//! `cp`'s wait status is the final authority. A normal exit returns its code verbatim
//! (docs/testing.md D6); a signal death is preserved so the parent shell sees a true signaled
//! exit — cprog restores the default disposition and re-raises the signal (docs/process-model.md
//! D2).
//!
//! The `128 + signal` fallback is narrower than it reads. For the standard signals the re-raise
//! goes through [`signal_hook::low_level::emulate_default_handler`], which ends in `abort()` if
//! its own raise ever returns — so a failure there dies of SIGABRT instead of reaching the
//! fallback. What does reach it is the realtime range: the emulation does not know those signals,
//! cprog raises them directly, and a raise can return if the signal is blocked in an inherited
//! mask (docs/exceptions.md A1/A1a).

use std::process::ExitStatus;

/// What cprog should do about `cp`'s termination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitDisposition {
    /// `cp` exited normally with this code — return it verbatim.
    Code(i32),
    /// `cp` was killed by this signal — re-raise it. [`reraise`] says when the `128 + signal`
    /// fallback is actually reachable.
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
/// true signaled exit; for a code, return it.
///
/// [`reraise`] normally does not return; `128 + signal` is what a shell reports for a signal
/// death anyway, so it is the right value when it does. See [`reraise`] for the one path that
/// actually gets here.
pub fn finalize(disp: ExitDisposition) -> i32 {
    if let ExitDisposition::Signal(sig) = disp {
        reraise(sig); // normally does not return (terminates the process)
    }
    disp.code()
}

/// Restore the default disposition for `signal`, unblock it, and re-raise it on ourselves so
/// cprog exits with the same signaled status `cp` did (docs/process-model.md, exceptions A1/A1a).
///
/// `emulate_default_handler` is signal-hook's safe wrapper for exactly that sequence, and unlike
/// a hand-rolled version it reports whether it worked. Two consequences are worth stating
/// because neither is visible at the call site:
///
/// * **It does not return on failure — it aborts.** Its standard-signal path ends in `abort()`
///   if its own raise comes back, so a failed re-raise dies of SIGABRT rather than falling
///   through to `128 + s`. Getting there needs `sigaction` to fail on a signal that came from
///   `WTERMSIG`, which does not happen on Linux; a *blocked* signal is not enough, because the
///   emulation unblocks first (measured: a blocked SIGTERM still terminates as SIGTERM).
/// * **It does not know the realtime range**, which comes back `EINVAL`. Raising those directly
///   is right: cprog installs handlers only for SIGINT/TERM/HUP/QUIT/WINCH/TSTP, so a signal
///   arriving here is guaranteed to have its default disposition. The signal *mask*, though, is
///   whatever was inherited — cprog never unblocks anything itself. If the parent blocked that
///   signal the raise leaves it pending and returns, and `finalize`'s `128 + s` is what the
///   caller sees. That is the only path that reaches it.
fn reraise(signal: i32) {
    if signal_hook::low_level::emulate_default_handler(signal).is_err() {
        let _ = signal_hook::low_level::raise(signal);
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
