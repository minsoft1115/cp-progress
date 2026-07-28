//! TTY checks, terminal size (`TIOCGWINSZ`), and `SIGWINCH` flag + low-frequency
//! fallback re-query (docs/runtime-model.md).
//!
//! [`detect`] fills the runtime half of [`crate::plan::Capabilities`]; the pure decision
//! rules (`TERM` acceptable, two fds on the same terminal) are split out and unit-tested,
//! while the ambient probes (`IsTerminal`, `fstat`, `PATH`, `/proc`, `TIOCGWINSZ`) compose
//! on top.

use std::io::{self, IsTerminal};
use std::os::fd::AsFd;
use std::path::Path;
use std::time::Duration;

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
        same_terminal: same_terminal_fds(io::stdout(), io::stderr()),
        term_ok: term_ok(std::env::var("TERM").ok().as_deref()),
        ci: std::env::var_os("CI").is_some(),
        linux_proc: proc_available(),
        stdbuf: stdbuf_available(),
        foreground: is_foreground(io::stdout()),
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
pub fn is_foreground<F: AsFd>(fd: F) -> bool {
    match rustix::termios::tcgetpgrp(fd) {
        Ok(fg) => fg == rustix::process::getpgrp(),
        // A terminal with *no* foreground process group answers with pgid 0, which rustix
        // reports as `OPNOTSUPP` rather than handing back a `Pid` that cannot exist. A pty
        // master is the case that reaches this: nobody is in front of one, and drawing a footer
        // into a master would deliver it as *input* on the slave. That is a definite "not the
        // foreground", not an unanswerable question, so it is the one error that means no
        // (docs/exceptions.md B10).
        Err(rustix::io::Errno::OPNOTSUPP) => false,
        // Anything else — ENOTTY above all — is "cannot tell", and cprog is lenient there.
        Err(_) => true,
    }
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

/// Read a fd's `(st_dev, st_ino)`. Takes the fd rather than a path: the question is about the
/// *streams* cprog was handed, which have no name to stat.
// The casts are no-ops on 64-bit Linux but keep this portable across arches where the widths
// differ.
#[allow(clippy::unnecessary_cast)]
fn dev_ino<F: AsFd>(fd: F) -> io::Result<DevIno> {
    let st = rustix::fs::fstat(fd)?;
    Ok(DevIno { dev: st.st_dev as u64, ino: st.st_ino as u64 })
}

/// Whether two fds resolve to the same terminal; false if either cannot be stat'd.
fn same_terminal_fds<A: AsFd, B: AsFd>(a: A, b: B) -> bool {
    matches!((dev_ino(a), dev_ino(b)), (Ok(x), Ok(y)) if same_terminal(&x, &y))
}

/// Query a terminal's size via `TIOCGWINSZ`, or `None` if the fd is not a sized terminal.
pub fn terminal_size<F: AsFd>(fd: F) -> Option<TerminalSize> {
    // A zero column count means the terminal never had its size set (a PTY nobody sized), which
    // is as unusable for layout as an outright error.
    let ws = rustix::termios::tcgetwinsize(fd).ok()?;
    if ws.ws_col == 0 {
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
///
/// Both halves are load-bearing: a directory named `stdbuf` on `PATH`, or a copy that lost its
/// execute bit, must read as "not here" and let the search continue. Answering yes stops the
/// search there, and if that entry was the only `stdbuf` on `PATH` the spawn then fails with
/// `EACCES` — `Fatal::CpSpawn`, exit 127, the copy never running at all where plain `cp` would
/// have succeeded. With a working `stdbuf` further along `PATH`, `execvp` skips the bad entry
/// and the run is unaffected, which is why the probe's job is to keep looking rather than to
/// answer early (exceptions B8).
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

    /// A regular file inside the temp dir, removed when the guard drops.
    struct TmpFile(std::path::PathBuf, std::fs::File);
    impl TmpFile {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir()
                .join(format!("cprog_term_{}_{}", std::process::id(), tag));
            let f = std::fs::File::create(&p).unwrap();
            TmpFile(p, f)
        }
    }
    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn terminal_size_of_a_non_terminal_is_none() {
        // A regular file is not a terminal, so the size query fails with ENOTTY and there is no
        // layout to compute. This replaces an fd of -1, which `BorrowedFd` cannot hold — and it
        // is the better test anyway: "stdout is not a terminal" is a condition that actually
        // occurs (any redirect), whereas a fabricated bad descriptor never does.
        let f = TmpFile::new("ws");
        assert!(
            rustix::termios::tcgetwinsize(&f.1).is_err(),
            "precondition: a regular file must not answer a size query"
        );
        assert_eq!(terminal_size(&f.1), None);
    }

    #[test]
    fn same_terminal_fds_matches_itself_and_separates_distinct_files() {
        // stdout stat'd twice is the same (dev, ino); two different files are not. The second
        // half is what B4 actually guards — stdout and stderr pointing somewhere different —
        // and a fabricated bad fd never exercised it.
        //
        // The `Err` arm of the composition is not reachable here: with a valid `BorrowedFd`
        // there is no way to make `fstat` fail. The rule it guards is covered directly by
        // `same_terminal_requires_both_dev_and_ino`.
        assert!(same_terminal_fds(io::stdout(), io::stdout()));
        let (a, b) = (TmpFile::new("dev_a"), TmpFile::new("dev_b"));
        assert!(!same_terminal_fds(&a.1, &b.1), "different files are different (dev, ino)");
    }

    #[test]
    fn a_non_terminal_fd_is_treated_as_foreground() {
        // exceptions B10. `tcgetpgrp` fails with ENOTTY on anything that is not a terminal, and
        // that answer is "I cannot tell", not "you are backgrounded". cprog is lenient there: a
        // genuinely backgrounded job *does* have a controlling terminal and is detected by the
        // pgrp comparison, so leniency costs nothing and refusing would disable the footer for
        // anyone whose stdout is not the controlling terminal.
        let f = TmpFile::new("fg");
        assert!(
            rustix::termios::tcgetpgrp(&f.1).is_err(),
            "precondition: a regular file must not answer tcgetpgrp"
        );
        assert!(is_foreground(&f.1), "an unanswerable tcgetpgrp must not read as backgrounded");
    }

    #[test]
    fn the_stdbuf_probe_requires_a_regular_executable_file() {
        // exceptions B8. Both halves of the probe carry weight, and dropping either one answers
        // "installed" for something cprog cannot run: a directory named `stdbuf` on PATH, or a
        // copy without its execute bit (an interrupted install, a file unpacked from an archive
        // that lost its mode). Answering yes stops the PATH search at that entry; if it was the
        // only `stdbuf` there, the spawn fails with EACCES and cprog exits 127 without copying
        // anything — a working `cp` turned into a failure by the wrapper. (With a real `stdbuf`
        // later on PATH, execvp skips the bad entry and nothing is lost, which is exactly why
        // the probe must keep searching rather than answer early.) C7 is the opposite shape,
        // where cp's own tooling reports the problem.
        use std::os::unix::fs::PermissionsExt;

        let f = TmpFile::new("exec");
        std::fs::set_permissions(&f.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable_file(&f.0), "a regular file with an execute bit is usable");

        std::fs::set_permissions(&f.0, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable_file(&f.0), "no execute bit -> not usable");

        // A directory, executable bits and all: searchable, but not something to exec.
        let dir = std::env::temp_dir().join(format!("cprog_term_{}_dir", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        struct RmDir(std::path::PathBuf);
        impl Drop for RmDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = RmDir(dir);
        assert!(
            std::fs::metadata(&dir.0).unwrap().permissions().mode() & 0o111 != 0,
            "precondition: a directory carries execute (search) bits"
        );
        assert!(!is_executable_file(&dir.0), "a directory is not an executable file");
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
