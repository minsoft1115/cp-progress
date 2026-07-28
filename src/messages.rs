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
use crate::ui::Style;

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
/// why).
///
/// [`Style`] drives two independent axes (docs/ui.md "색/글리프 정책"). Colour — from `TERM` and
/// `NO_COLOR` — paints success green and failure red. Glyphs — from the locale — decide the
/// marker: `✓`/`✗` on a UTF-8 terminal, `[ok]`/`[!]` otherwise, matching the bar's own
/// `[###---]` fallback. The marker has to follow the same rule as the bar; a terminal cprog has
/// already judged unable to render UTF-8 must not be handed a `✓` on the way out (#31, F11).
pub fn summary(
    disp: &ExitDisposition,
    elapsed: Duration,
    style: Style,
    progress_shown: bool,
) -> Option<String> {
    if !progress_shown {
        return None;
    }
    let t = format_duration(elapsed);
    let (ok, bad) = if style.unicode { ("✓", "✗") } else { ("[ok]", "[!]") };
    match disp {
        ExitDisposition::Signal(_) => None,
        ExitDisposition::Code(0) => {
            Some(colorize(format!("{ok} done - {t} elapsed"), "\x1b[32m", style.color)) // green
        }
        ExitDisposition::Code(n) => {
            Some(colorize(format!("{bad} cp exited {n} - {t} elapsed"), "\x1b[31m", style.color)) // red
        }
    }
}

/// SGR: dim — used for the version notice so it reads as an aside, not as cp output.
const DIM: &str = "\x1b[2m";

/// Wrap `line` in `sgr`…reset when `color` is enabled, else return it plain.
fn colorize(line: String, sgr: &str, color: bool) -> String {
    if color {
        format!("{sgr}{line}\x1b[0m")
    } else {
        line
    }
}

/// Format an elapsed duration as `MM:SS`, or `H:MM:SS` from one hour on (3600 s reads `1:00:00`,
/// not `60:00` — the same boundary as the footer's eta, docs/ui.md).
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 3600 {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}

/// A short notice naming cprog itself, appended after `cp`'s output for informational
/// invocations (`--help` / `--version`), or `None` when it must stay silent.
///
/// `cprog` has no options of its own — `--version` reaches `cp` like any other argument — so
/// without this a user has no way to tell which wrapper build is running, or that one is running
/// at all. It is gated on `stderr_tty` because everywhere else the passthrough contract applies:
/// redirected and piped output must stay byte-identical to `cp`, and a script doing
/// `cp --version | tail -1` must keep working. Like the exit summary, it goes to stderr so stdout
/// stays `cp`'s (docs/runtime-model.md "버전 표시").
///
/// Two touches keep it from reading as one more paragraph of cp's own output: a blank line ahead
/// of it, and dim text when colour is allowed. A horizontal rule would be the alternative and is
/// deliberately not used — it would need the terminal width, which this path never queries, and
/// cprog draws no decoration anywhere else.
///
/// Takes the whole [`Style`] for the same reason [`summary`] does: the separator is an em dash on
/// a UTF-8 terminal and a plain hyphen otherwise. This line reaches a terminal cprog has *not*
/// otherwise inspected — `--help`/`--version` is a passthrough, so no footer was ever laid out —
/// which makes it the easiest glyph rule to forget (docs/ui.md "색/글리프 정책", F11).
pub fn version_notice(informational: bool, stderr_tty: bool, style: Style) -> Option<String> {
    if !informational || !stderr_tty {
        return None;
    }
    let dash = if style.unicode { "—" } else { "-" };
    let line = format!(
        "cprog {} {dash} {}",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY")
    );
    // The blank line stays outside the SGR run so the escapes wrap only the text.
    Some(format!("\n{}", colorize(line, DIM, style.color)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::ExitDisposition;
    use std::time::Duration;

    /// UTF-8 glyphs, no colour — the baseline the wording assertions use.
    fn plain() -> Style {
        Style { color: false, unicode: true }
    }
    /// UTF-8 glyphs with colour.
    fn colored() -> Style {
        Style { color: true, unicode: true }
    }
    /// A terminal whose locale is not UTF-8 (LC_ALL=C): no glyphs, no colour.
    fn ascii() -> Style {
        Style { color: false, unicode: false }
    }

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

    // ---- version notice (#15, docs/runtime-model.md "버전 표시") ----------------------

    #[test]
    fn version_notice_names_cprog_and_its_repository() {
        let n = version_notice(true, true, plain()).expect("shown for --help/--version on a tty");
        assert!(n.contains("cprog "), "names the wrapper, not cp: {n:?}");
        assert!(n.contains(env!("CARGO_PKG_VERSION")), "carries the version: {n:?}");
        assert!(n.contains("github.com"), "points somewhere useful: {n:?}");
    }

    #[test]
    fn version_notice_is_separated_by_a_blank_line() {
        // Without it the notice reads as one more paragraph of cp's own output, which is exactly
        // what it is not.
        let n = version_notice(true, true, plain()).unwrap();
        assert!(n.starts_with('\n'), "a blank line comes first: {n:?}");
        assert_eq!(n.lines().filter(|l| !l.is_empty()).count(), 1, "still one line: {n:?}");
    }

    #[test]
    fn version_notice_is_dim_when_colour_is_allowed() {
        let dim = version_notice(true, true, colored()).unwrap();
        assert!(dim.contains("\x1b[2m") && dim.ends_with("\x1b[0m"), "dimmed: {dim:?}");
        // The separator must sit outside the escape run, or the blank line joins the SGR span.
        assert!(dim.starts_with("\n\x1b[2m"), "escapes wrap the text only: {dim:?}");
    }

    #[test]
    fn version_notice_is_plain_when_colour_is_off() {
        // NO_COLOR / TERM=dumb reach here through the same Style the summary uses.
        let plain = version_notice(true, true, plain()).unwrap();
        assert!(!plain.contains('\x1b'), "no escapes at all: {plain:?}");
    }

    #[test]
    fn version_notice_is_absent_for_an_ordinary_copy() {
        // Only informational invocations get it; a real copy already has the exit summary.
        assert_eq!(version_notice(false, true, colored()), None);
    }

    #[test]
    fn version_notice_is_absent_when_stderr_is_not_a_tty() {
        // The passthrough contract: redirected or piped output stays byte-identical to `cp`,
        // so `cp --version 2>/dev/null` and `cp --version | tail -1` are unaffected.
        assert_eq!(version_notice(true, false, colored()), None);
        assert_eq!(version_notice(false, false, plain()), None);
    }

    // ---- exit summary (docs/ui.md examples 4/5, runtime-model) -----------------------

    #[test]
    fn no_summary_without_progress() {
        // The general gate: if the footer never engaged (e.g. --help, an instant exit), cp did
        // nothing worth summarizing -> stay quiet, whatever the exit code.
        assert_eq!(summary(&ExitDisposition::Code(0), Duration::from_secs(1), plain(), false), None);
        assert_eq!(summary(&ExitDisposition::Code(1), Duration::from_secs(1), plain(), false), None);
    }

    #[test]
    fn no_summary_on_signal() {
        // docs/runtime-model.md: a signaled cp gets no summary (preserve signal semantics).
        assert_eq!(summary(&ExitDisposition::Signal(2), Duration::from_secs(14), plain(), true), None);
    }

    #[test]
    fn success_summary() {
        let s = summary(&ExitDisposition::Code(0), Duration::from_secs(14), plain(), true);
        assert_eq!(s.as_deref(), Some("✓ done - 00:14 elapsed"));
    }

    #[test]
    fn failure_summary_states_exit_code_and_elapsed() {
        // Neutral wording: cp exited with a code (not editorialized as "failed"), plus elapsed.
        let s = summary(&ExitDisposition::Code(1), Duration::from_secs(3), plain(), true);
        assert_eq!(s.as_deref(), Some("✗ cp exited 1 - 00:03 elapsed"));
    }

    #[test]
    fn duration_formats_hours() {
        let s = summary(&ExitDisposition::Code(0), Duration::from_secs(3665), plain(), true);
        assert_eq!(s.as_deref(), Some("✓ done - 1:01:05 elapsed"));
        // The boundary: 3600 s is the first elapsed time written as H:MM:SS, so the summary of an
        // hour-long copy reads `1:00:00`, not `60:00` (docs/ui.md, same rule as the footer's eta).
        let one_hour = summary(&ExitDisposition::Code(0), Duration::from_secs(3600), plain(), true);
        assert_eq!(one_hour.as_deref(), Some("✓ done - 1:00:00 elapsed"));
        let just_under = summary(&ExitDisposition::Code(0), Duration::from_secs(3599), plain(), true);
        assert_eq!(just_under.as_deref(), Some("✓ done - 59:59 elapsed"));
    }

    #[test]
    fn the_version_notice_separator_falls_back_to_ascii_too() {
        // #31, found reviewing the summary fix: this line is emitted on a passthrough, where no
        // footer was ever laid out, so it is the one place the glyph rule is easy to forget —
        // and the em dash was reaching a C-locale terminal as three replacement characters.
        let n = version_notice(true, true, ascii()).unwrap();
        assert!(n.is_ascii(), "no glyph may reach a non-UTF-8 terminal: {n:?}");
        assert!(n.contains(" - "), "a plain hyphen stands in for the em dash: {n:?}");

        let utf8 = version_notice(true, true, plain()).unwrap();
        assert!(utf8.contains(" — "), "UTF-8 terminals keep the em dash: {utf8:?}");
    }

    #[test]
    fn summary_glyphs_fall_back_to_ascii_without_unicode() {
        // #31 / exceptions F11. On LC_ALL=C the bar already renders as [###---], the ⏳ is gone
        // and the ellipsis is "...". Emitting ✓/✗ here contradicts that judgement on the one line
        // that is guaranteed to be printed.
        let ok = summary(&ExitDisposition::Code(0), Duration::from_secs(14), ascii(), true).unwrap();
        assert_eq!(ok, "[ok] done - 00:14 elapsed");
        let bad = summary(&ExitDisposition::Code(1), Duration::from_secs(3), ascii(), true).unwrap();
        assert_eq!(bad, "[!] cp exited 1 - 00:03 elapsed");
        for line in [&ok, &bad] {
            assert!(line.is_ascii(), "nothing outside ASCII may reach this terminal: {line:?}");
        }
    }

    #[test]
    fn glyph_fallback_is_independent_of_colour() {
        // Two separate axes: colour comes from TERM/NO_COLOR, glyphs from the locale. A colour
        // terminal on a C locale must still get ASCII markers, wrapped in SGR.
        let s = Style { color: true, unicode: false };
        let line = summary(&ExitDisposition::Code(0), Duration::from_secs(1), s, true).unwrap();
        assert_eq!(line, "\x1b[32m[ok] done - 00:01 elapsed\x1b[0m");
    }

    #[test]
    fn color_wraps_success_green_and_failure_red() {
        let ok = summary(&ExitDisposition::Code(0), Duration::from_secs(1), colored(), true).unwrap();
        assert!(ok.starts_with("\x1b[32m") && ok.ends_with("\x1b[0m"), "green: {ok:?}");
        let bad = summary(&ExitDisposition::Code(1), Duration::from_secs(1), colored(), true).unwrap();
        assert!(bad.starts_with("\x1b[31m") && bad.ends_with("\x1b[0m"), "red: {bad:?}");
    }
}
