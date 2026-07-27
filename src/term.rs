//! TTY checks, terminal size (`TIOCGWINSZ`), and `SIGWINCH` flag + low-frequency
//! fallback re-query (docs/runtime-model.md).
//!
//! [`detect`] fills the runtime half of [`crate::plan::Capabilities`]; the pure decision
//! rules (`TERM` acceptable, two fds on the same terminal) are split out and unit-tested,
//! while the ambient probes (`IsTerminal`, `fstat`, `PATH`, `/proc`, `TIOCGWINSZ`) compose
//! on top.

use std::io::{self, IsTerminal};
use std::path::Path;
use std::time::Duration;

use libc::c_int;

use crate::plan::Capabilities;
use crate::ui::Style;

/// A file's `(st_dev, st_ino)` identity — two fds share a terminal iff these match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevIno {
    /// `st_dev`.
    pub dev: u64,
    /// `st_ino`.
    pub ino: u64,
}

/// Whether two fds refer to the same terminal: identical device *and* inode.
pub fn same_terminal(a: &DevIno, b: &DevIno) -> bool {
    a == b
}

/// Whether `TERM` is acceptable for a managed TUI: set, non-empty, and not `dumb`.
pub fn term_ok(term: Option<&str>) -> bool {
    matches!(term, Some(t) if !t.is_empty() && t != "dumb")
}

/// Whether the cached terminal size should be re-queried: on a SIGWINCH event, or once the
/// low-frequency fallback has elapsed. The fallback covers signals that were missed or
/// coalesced, which pure SIGWINCH handling can drop (docs/runtime-model.md).
pub fn should_requery_size(resized: bool, since_last: Duration, fallback: Duration) -> bool {
    resized || since_last >= fallback
}

/// Detect the runtime capabilities relevant to the managed/passthrough choice
/// (docs/runtime-model.md). Interactive intent comes separately from arg inspection.
pub fn detect() -> Capabilities {
    Capabilities {
        stdout_tty: io::stdout().is_terminal(),
        stderr_tty: io::stderr().is_terminal(),
        same_terminal: same_terminal_fds(libc::STDOUT_FILENO, libc::STDERR_FILENO),
        term_ok: term_ok(std::env::var("TERM").ok().as_deref()),
        ci: std::env::var_os("CI").is_some(),
        linux_proc: proc_available(),
        stdbuf: stdbuf_available(),
        foreground: is_foreground(libc::STDOUT_FILENO),
        passthrough_forced: passthrough_forced(),
    }
}

/// Whether `CPROG_PASSTHROUGH` is set — any value counts, the same value-agnostic rule as
/// `CI` (B6) and `NO_COLOR` (F10). The user's explicit ask for cprog to get out of the way,
/// read both here (mode decision) and in dispatch (version-notice suppression).
pub fn passthrough_forced() -> bool {
    std::env::var_os("CPROG_PASSTHROUGH").is_some()
}

/// Whether we are the foreground process group of our controlling terminal — i.e. not a
/// background job (`cprog … &`). `tcgetpgrp` fails with `ENOTTY` when `fd` isn't our controlling
/// terminal; there we can't prove we're backgrounded, so we're lenient (return `true`). A real
/// backgrounded `cprog &` has the tty as its controlling terminal and `tcgetpgrp` returns a
/// *different* (foreground) pgrp, so it is correctly detected as background.
///
/// Checked at startup and re-checked when resuming from a `SIGTSTP` suspend (a `Ctrl-Z` then
/// `bg` can move us to the background), so the footer is never drawn from a background job.
pub fn is_foreground(fd: c_int) -> bool {
    let fg = unsafe { libc::tcgetpgrp(fd) };
    if fg < 0 {
        return true; // e.g. ENOTTY: not our controlling terminal -> can't tell -> allow
    }
    fg == unsafe { libc::getpgrp() }
}

/// Detect colour/glyph [`Style`] from the environment (docs/ui.md "색/글리프 정책").
pub fn detect_style() -> Style {
    Style {
        color: color_from(
            std::env::var_os("NO_COLOR").is_some(),
            std::env::var("TERM").ok().as_deref(),
        ),
        unicode: unicode_from(locale().as_deref()),
    }
}

/// Colour rule: enabled when `NO_COLOR` is unset and `TERM` is usable.
pub fn color_from(no_color: bool, term: Option<&str>) -> bool {
    !no_color && term_ok(term)
}

/// Unicode rule: the active locale advertises UTF-8 (or no locale is set — assume modern).
pub fn unicode_from(locale: Option<&str>) -> bool {
    match locale {
        Some(l) => l.to_ascii_lowercase().contains("utf"),
        None => true,
    }
}

/// The effective locale string from the usual precedence of env vars.
fn locale() -> Option<String> {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
}

/// Read a fd's `(st_dev, st_ino)` via `fstat`.
// The casts are no-ops on 64-bit glibc but keep this portable across Linux arches where
// dev_t/ino_t widths differ.
#[allow(clippy::unnecessary_cast)]
fn dev_ino(fd: c_int) -> io::Result<DevIno> {
    // SAFETY: fstat writes into a zeroed stat buffer; we check the return code.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(DevIno { dev: st.st_dev as u64, ino: st.st_ino as u64 })
}

/// Whether two fds resolve to the same terminal; false if either cannot be stat'd.
fn same_terminal_fds(a: c_int, b: c_int) -> bool {
    matches!((dev_ino(a), dev_ino(b)), (Ok(x), Ok(y)) if same_terminal(&x, &y))
}

/// Query a terminal's size via `TIOCGWINSZ`, or `None` if the fd is not a sized terminal.
pub fn terminal_size(fd: c_int) -> Option<TerminalSize> {
    // SAFETY: ioctl writes into a zeroed winsize; we check the return code.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } != 0 || ws.ws_col == 0 {
        return None;
    }
    Some(TerminalSize::new(ws.ws_col, ws.ws_row))
}

/// Whether `/proc` is readable (Linux with a mounted procfs).
fn proc_available() -> bool {
    cfg!(target_os = "linux") && Path::new("/proc/self/fd").exists()
}

/// Whether an executable `stdbuf` is on `PATH` (feature-detect, not a version parse).
fn stdbuf_available() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable_file(&dir.join("stdbuf")))
}

/// Whether `p` is a regular, executable file.
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Terminal dimensions in character cells. Plain data so layout logic (docs/ui.md) can be
/// unit-tested without querying a real terminal; [`terminal_size`] does the real
/// `TIOCGWINSZ` query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    /// Width in columns.
    pub cols: u16,
    /// Height in rows.
    pub rows: u16,
}

impl TerminalSize {
    /// Construct a size from columns and rows.
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_ok_requires_set_and_not_dumb() {
        assert!(!term_ok(None), "unset TERM");
        assert!(!term_ok(Some("")), "empty TERM");
        assert!(!term_ok(Some("dumb")), "dumb TERM");
        assert!(term_ok(Some("xterm-256color")));
        assert!(term_ok(Some("linux")));
    }

    #[test]
    fn same_terminal_requires_both_dev_and_ino() {
        let a = DevIno { dev: 5, ino: 9 };
        assert!(same_terminal(&a, &DevIno { dev: 5, ino: 9 }));
        assert!(!same_terminal(&a, &DevIno { dev: 6, ino: 9 }), "different device");
        assert!(!same_terminal(&a, &DevIno { dev: 5, ino: 8 }), "different inode");
    }

    #[test]
    fn terminal_size_of_bad_fd_is_none() {
        assert_eq!(terminal_size(-1), None);
    }

    #[test]
    fn same_terminal_fds_matches_itself_and_rejects_bad_fd() {
        // fd 1 fstat'd twice is identical; a bad fd cannot match anything.
        assert!(same_terminal_fds(1, 1));
        assert!(!same_terminal_fds(1, -1));
    }

    #[test]
    fn color_rule() {
        assert!(color_from(false, Some("xterm-256color")));
        assert!(!color_from(true, Some("xterm")), "NO_COLOR disables colour");
        assert!(!color_from(false, Some("dumb")), "dumb TERM disables colour");
        assert!(!color_from(false, None), "unset TERM disables colour");
    }

    #[test]
    fn resize_requery_rule() {
        let fallback = Duration::from_secs(1);
        assert!(should_requery_size(true, Duration::ZERO, fallback), "SIGWINCH forces requery");
        assert!(!should_requery_size(false, Duration::from_millis(500), fallback), "within fallback");
        assert!(should_requery_size(false, Duration::from_secs(1), fallback), "fallback elapsed");
        assert!(should_requery_size(false, Duration::from_secs(3), fallback));
    }

    #[test]
    fn unicode_rule() {
        assert!(unicode_from(Some("en_US.UTF-8")));
        assert!(unicode_from(Some("C.utf8")));
        assert!(!unicode_from(Some("C")), "C locale is not UTF-8");
        assert!(!unicode_from(Some("POSIX")));
        assert!(unicode_from(None), "no locale -> assume modern UTF-8");
    }
}
