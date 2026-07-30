//! `ExitDisposition` -> signal-preserving finalize (docs/process-model.md).
//!
//! `cp`'s wait status is the final authority. A normal exit returns its code verbatim
//! (docs/testing.md D6); a signal death is preserved so the parent shell sees a true signaled
//! exit — cprog restores the default disposition and re-raises the signal
//! (docs/process-model.md "시그널 보존 종료", docs/testing.md D2). `process-model.md` carries no
//! numbered rows, so the bare `D2` used to name nothing there — and `D2` in exceptions.md is an
//! unrelated rule about trailing bytes (#61 C).
//!
//! The `128 + signal` fallback is unreachable, and kept anyway. What makes it unreachable is
//! **`WTERMSIG`**: the only signals that reach [`finalize`] are ones that actually terminated
//! `cp`, and cprog cannot be handed any other kind. The re-raise mechanics then fall out in three
//! shapes. A standard signal whose default action is *termination* goes through
//! [`signal_hook::low_level::emulate_default_handler`], which ends in `abort()` if its own raise
//! ever returns — a failure there dies of SIGABRT rather than falling through. A standard signal
//! whose default action is *ignore* or *stop* does **not** abort: the emulation performs that
//! default, returns `Ok`, and the fallback really is reached (`finalize(Signal(SIGWINCH))` returns
//! 156, `SIGCHLD` 145 — measured). Those are exactly the signals `WTERMSIG` can never name, which
//! is why the sentence above is about wait statuses and not about `abort`. For the realtime range
//! cprog raises directly, and that raise *can* return when the signal is blocked — but a signal
//! blocked in cprog is blocked in `cp` too (the mask is inherited across `spawn`, measured), so
//! `cp` cannot have died of it. It costs one line and covers a wait status no kernel produces
//! (docs/exceptions.md A1/A1a, F15).

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
/// * **For a signal that terminates, it does not return on failure — it aborts.** That path ends
///   in `abort()` if its own raise comes back, so a failed re-raise dies of SIGABRT rather than
///   falling through to `128 + s`. Getting there needs `sigaction` to fail on a signal that came
///   from `WTERMSIG`, which does not happen on Linux; a *blocked* signal is not enough, because
///   the emulation unblocks first (measured: a blocked SIGTERM still terminates as SIGTERM).
/// * **For a signal whose default action is ignore or stop, it returns normally**, having done
///   that default — so `reraise` comes back and `finalize` does fall through to `128 + s`
///   (measured: `Signal(SIGWINCH)` -> 156, `Signal(SIGCHLD)` -> 145). This is not a hole, because
///   such a signal cannot arrive: `WTERMSIG` only names what terminated `cp`. Stating the abort as
///   if it covered every standard signal was wrong (#69 D).
/// * **It does not know the realtime range**, which comes back `EINVAL`. Raising those directly
///   is right: cprog installs handlers only for SIGINT/TERM/HUP/QUIT/WINCH/TSTP, so a signal
///   arriving here is guaranteed to have its default disposition. Without this branch a realtime
///   death would exit *normally* with `128 + s` and A1 would be broken — which is what
///   `tests/signals.rs::cp_killed_by_a_realtime_signal_still_exits_cprog_signaled` pins.
///
/// A blocked signal cannot arrive here at all. The raise would indeed return and leave it
/// pending, but reaching this function means `WTERMSIG` named that signal, and `cp` inherits
/// cprog's mask verbatim — a signal cprog blocks is blocked in `cp` and cannot kill it. The
/// kernel-forced synchronous signals are the one class that kills through a block, and they are
/// all standard, so they take the branch above (exceptions A1a).
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
    fn signal_maps_to_128_plus_n() {
        // The arithmetic `ExitDisposition::code()` performs, and nothing more. `finalize` never
        // returns it: A1a ② and F15 ⑤ record that both re-raise branches either abort or cannot
        // meet a blocked signal, so the fallback is unreachable.
        //
        // The old name and comment presented these two as the fallback in action, and picked the
        // two signals that provably cannot reach it — SIGINT ends in `abort()` inside
        // `emulate_default_handler`, and SIGKILL never returns to anyone (#61).
        assert_eq!(disposition(ExitStatus::from_raw(2)).code(), 130); // SIGINT
        assert_eq!(disposition(ExitStatus::from_raw(9)).code(), 137); // SIGKILL
        assert_eq!(disposition(ExitStatus::from_raw(libc::SIGRTMIN())).code(), 128 + libc::SIGRTMIN());
    }

    #[test]
    fn finalize_returns_code_verbatim() {
        // The Code path has no side effects; the Signal path (re-raise) is covered by the
        // signal integration test, since exercising it here would kill the test process.
        assert_eq!(finalize(ExitDisposition::Code(0)), 0);
        assert_eq!(finalize(ExitDisposition::Code(3)), 3);
    }
}
