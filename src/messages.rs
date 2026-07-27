//! Summary strings and the `Fatal` type (docs/architecture.md).
//!
//! `Fatal` blocks execution and carries a non-zero exit code. cprog-side non-fatal failures
//! (relay/render/sample) are swallowed and never allowed to change `cp`'s exit code
//! (docs/architecture.md "에러 철학"). The exit summary is minimal — cprog counts neither files
//! nor total bytes — and goes to stderr so stdout stays `cp`'s (docs/runtime-model.md,
//! docs/ui.md examples 4/5).

use std::fmt;
use std::time::Duration;

use crate::exit::ExitDisposition;

/// A fatal problem that stops cprog before/around running `cp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fatal {
    /// No operands were given.
    Usage,
    /// `cp` could not be spawned.
    CpSpawn(String),
    /// Waiting on `cp` failed.
    CpWait {
        /// The child pid that could not be waited on.
        pid: u32,
        /// The underlying error.
        source: String,
    },
}

impl Fatal {
    /// The exit code cprog returns for this fatal.
    pub fn code(&self) -> i32 {
        match self {
            Fatal::Usage => 1,
            Fatal::CpSpawn(_) => 127, // cannot execute cp (shell convention)
            Fatal::CpWait { .. } => 1,
        }
    }
}

impl fmt::Display for Fatal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fatal::Usage => write!(f, "usage: cprog <cp args...>"),
            Fatal::CpSpawn(e) => write!(f, "cprog: failed to run cp: {e}"),
            Fatal::CpWait { pid, source } => {
                write!(f, "cprog: failed to wait for cp (pid {pid}): {source}")
            }
        }
    }
}

/// Build the one-line exit summary for stderr, or `None` when none should be shown.
///
/// Gated on `progress_shown`: if the footer never engaged (cp did nothing worth monitoring —
/// e.g. `--help`, an instant success/failure), there is no summary at all. Otherwise a signaled
/// `cp` still gets none (signal semantics win); success is `✓ done - T elapsed`; a non-zero exit
/// is stated neutrally as `✗ cp exited n - T elapsed` (cp's own stderr, relayed above, explains
/// why). With `color`, success is green and failure red (docs/ui.md "색/글리프 정책").
pub fn summary(
    disp: &ExitDisposition,
    elapsed: Duration,
    color: bool,
    progress_shown: bool,
) -> Option<String> {
    if !progress_shown {
        return None;
    }
    let t = format_duration(elapsed);
    match disp {
        ExitDisposition::Signal(_) => None,
        ExitDisposition::Code(0) => {
            Some(colorize(format!("✓ done - {t} elapsed"), "\x1b[32m", color)) // green
        }
        ExitDisposition::Code(n) => {
            Some(colorize(format!("✗ cp exited {n} - {t} elapsed"), "\x1b[31m", color)) // red
        }
    }
}

/// Wrap `line` in `sgr`…reset when `color` is enabled, else return it plain.
fn colorize(line: String, sgr: &str, color: bool) -> String {
    if color {
        format!("{sgr}{line}\x1b[0m")
    } else {
        line
    }
}

/// Format an elapsed duration as `MM:SS`, or `H:MM:SS` once past an hour.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 3600 {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}

/// One line naming cprog itself, appended after `cp`'s output for informational invocations
/// (`--help` / `--version`), or `None` when it must stay silent.
///
/// `cprog` has no options of its own — `--version` reaches `cp` like any other argument — so
/// without this a user has no way to tell which wrapper build is running, or that one is running
/// at all. It is gated on `stderr_tty` because everywhere else the passthrough contract applies:
/// redirected and piped output must stay byte-identical to `cp`, and a script doing
/// `cp --version | tail -1` must keep working. Like the exit summary, it goes to stderr so stdout
/// stays `cp`'s (docs/runtime-model.md "버전 표시").
pub fn version_line(informational: bool, stderr_tty: bool) -> Option<String> {
    if !informational || !stderr_tty {
        return None;
    }
    Some(format!(
        "cprog {} — {}",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::ExitDisposition;
    use std::time::Duration;

    // ---- fatals (block execution, non-zero exit) -------------------------------------

    #[test]
    fn usage_fatal(){
        // docs/testing.md D5: no args -> usage, exit 1.
        assert_eq!(Fatal::Usage.to_string(), "usage: cprog <cp args...>");
        assert_eq!(Fatal::Usage.code(), 1);
    }

    #[test]
    fn cp_spawn_fatal() {
        // docs/testing.md D4.
        let f = Fatal::CpSpawn("No such file or directory".into());
        assert_eq!(f.to_string(), "cprog: failed to run cp: No such file or directory");
        assert_eq!(f.code(), 127);
    }

    #[test]
    fn cp_wait_fatal() {
        let f = Fatal::CpWait { pid: 42, source: "ECHILD".into() };
        assert_eq!(f.to_string(), "cprog: failed to wait for cp (pid 42): ECHILD");
        assert_eq!(f.code(), 1);
    }

    // ---- version line (#15, docs/runtime-model.md "버전 표시") ------------------------

    #[test]
    fn version_line_names_cprog_and_its_repository() {
        let line = version_line(true, true).expect("shown for --help/--version on a tty");
        assert!(line.starts_with("cprog "), "names the wrapper, not cp: {line:?}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "carries the version: {line:?}");
        assert!(line.contains("github.com"), "points somewhere useful: {line:?}");
    }

    #[test]
    fn version_line_is_absent_for_an_ordinary_copy() {
        // Only informational invocations get it; a real copy already has the exit summary.
        assert_eq!(version_line(false, true), None);
    }

    #[test]
    fn version_line_is_absent_when_stderr_is_not_a_tty() {
        // The passthrough contract: redirected or piped output stays byte-identical to `cp`,
        // so `cp --version 2>/dev/null` and `cp --version | tail -1` are unaffected.
        assert_eq!(version_line(true, false), None);
        assert_eq!(version_line(false, false), None);
    }

    // ---- exit summary (docs/ui.md examples 4/5, runtime-model) -----------------------

    #[test]
    fn no_summary_without_progress() {
        // The general gate: if the footer never engaged (e.g. --help, an instant exit), cp did
        // nothing worth summarizing -> stay quiet, whatever the exit code.
        assert_eq!(summary(&ExitDisposition::Code(0), Duration::from_secs(1), false, false), None);
        assert_eq!(summary(&ExitDisposition::Code(1), Duration::from_secs(1), false, false), None);
    }

    #[test]
    fn no_summary_on_signal() {
        // docs/runtime-model.md: a signaled cp gets no summary (preserve signal semantics).
        assert_eq!(summary(&ExitDisposition::Signal(2), Duration::from_secs(14), false, true), None);
    }

    #[test]
    fn success_summary() {
        let s = summary(&ExitDisposition::Code(0), Duration::from_secs(14), false, true);
        assert_eq!(s.as_deref(), Some("✓ done - 00:14 elapsed"));
    }

    #[test]
    fn failure_summary_states_exit_code_and_elapsed() {
        // Neutral wording: cp exited with a code (not editorialized as "failed"), plus elapsed.
        let s = summary(&ExitDisposition::Code(1), Duration::from_secs(3), false, true);
        assert_eq!(s.as_deref(), Some("✗ cp exited 1 - 00:03 elapsed"));
    }

    #[test]
    fn duration_formats_hours() {
        let s = summary(&ExitDisposition::Code(0), Duration::from_secs(3665), false, true);
        assert_eq!(s.as_deref(), Some("✓ done - 1:01:05 elapsed"));
    }

    #[test]
    fn color_wraps_success_green_and_failure_red() {
        let ok = summary(&ExitDisposition::Code(0), Duration::from_secs(1), true, true).unwrap();
        assert!(ok.starts_with("\x1b[32m") && ok.ends_with("\x1b[0m"), "green: {ok:?}");
        let bad = summary(&ExitDisposition::Code(1), Duration::from_secs(1), true, true).unwrap();
        assert!(bad.starts_with("\x1b[31m") && bad.ends_with("\x1b[0m"), "red: {bad:?}");
    }
}
