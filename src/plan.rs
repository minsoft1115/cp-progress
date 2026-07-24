//! Capabilities detection and run-mode selection (docs/runtime-model.md).
//! Pure function of inspected args + detected runtime capabilities.
//!
//! The run mode is decided once, before `cp` starts, and never changes. Managed-TUI is
//! chosen only when *every* condition holds; any failure — or a failed arg scan handled by
//! the caller — falls back to passthrough, which is byte-identical to `cp`.

/// Detected runtime capabilities relevant to the managed/passthrough choice. Injected so the
/// decision stays a pure function; real detection (`IsTerminal`, `fstat`, env, `stdbuf`
/// probe) is wired separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// stdout is a TTY.
    pub stdout_tty: bool,
    /// stderr is a TTY.
    pub stderr_tty: bool,
    /// stdout and stderr refer to the same terminal.
    pub same_terminal: bool,
    /// `TERM` is set and not `dumb`.
    pub term_ok: bool,
    /// A CI environment is detected (`CI` set).
    pub ci: bool,
    /// Running on Linux with a readable `/proc`.
    pub linux_proc: bool,
    /// `stdbuf` is executable (required to stream `-v` live).
    pub stdbuf: bool,
    /// We are the terminal's foreground process group (i.e. not a background job like
    /// `cprog … &`). A background job must not draw the footer / take over the terminal.
    pub foreground: bool,
}

/// The chosen run mode (docs/architecture.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Live footer + relayed `-v` log region.
    ManagedTui,
    /// Streams inherited, byte-identical to `cp`.
    Passthrough,
}

/// Decide the run mode from detected `caps` and whether an interactive flag was requested.
/// Managed-TUI requires every condition to hold; otherwise passthrough.
pub fn decide(caps: &Capabilities, interactive: bool) -> RunMode {
    let managed = !interactive
        && caps.stdout_tty
        && caps.stderr_tty
        && caps.same_terminal
        && caps.term_ok
        && !caps.ci
        && caps.linux_proc
        && caps.stdbuf
        && caps.foreground;

    if managed {
        RunMode::ManagedTui
    } else {
        RunMode::Passthrough
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Capabilities that, with a non-interactive arg set, satisfy every managed condition.
    fn managed_caps() -> Capabilities {
        Capabilities {
            stdout_tty: true,
            stderr_tty: true,
            same_terminal: true,
            term_ok: true,
            ci: false,
            linux_proc: true,
            stdbuf: true,
            foreground: true,
        }
    }

    #[test]
    fn background_job_is_passthrough() {
        // bug1: a backgrounded `cprog &` is not the terminal's foreground process group, so it
        // must not enter the managed TUI (which would take over the terminal). -> passthrough.
        let caps = Capabilities { foreground: false, ..managed_caps() };
        assert_eq!(decide(&caps, false), RunMode::Passthrough);
    }

    #[test]
    fn all_conditions_met_is_managed() {
        assert_eq!(decide(&managed_caps(), false), RunMode::ManagedTui);
    }

    #[test]
    fn interactive_forces_passthrough() {
        // docs/testing.md E2 / runtime-model: an interactive flag forces passthrough.
        assert_eq!(decide(&managed_caps(), true), RunMode::Passthrough);
    }

    // docs/testing.md E3: any single missing capability drops to passthrough.

    #[test]
    fn stdout_not_tty_is_passthrough() {
        let caps = Capabilities { stdout_tty: false, ..managed_caps() };
        assert_eq!(decide(&caps, false), RunMode::Passthrough);
    }

    #[test]
    fn stderr_not_tty_is_passthrough() {
        let caps = Capabilities { stderr_tty: false, ..managed_caps() };
        assert_eq!(decide(&caps, false), RunMode::Passthrough);
    }

    #[test]
    fn different_terminals_is_passthrough() {
        let caps = Capabilities { same_terminal: false, ..managed_caps() };
        assert_eq!(decide(&caps, false), RunMode::Passthrough);
    }

    #[test]
    fn term_dumb_or_unset_is_passthrough() {
        let caps = Capabilities { term_ok: false, ..managed_caps() };
        assert_eq!(decide(&caps, false), RunMode::Passthrough);
    }

    #[test]
    fn ci_is_passthrough() {
        let caps = Capabilities { ci: true, ..managed_caps() };
        assert_eq!(decide(&caps, false), RunMode::Passthrough);
    }

    #[test]
    fn non_linux_is_passthrough() {
        let caps = Capabilities { linux_proc: false, ..managed_caps() };
        assert_eq!(decide(&caps, false), RunMode::Passthrough);
    }

    #[test]
    fn missing_stdbuf_is_passthrough() {
        // docs/capture-and-verbose.md: without stdbuf, -v cannot stream live -> passthrough.
        let caps = Capabilities { stdbuf: false, ..managed_caps() };
        assert_eq!(decide(&caps, false), RunMode::Passthrough);
    }
}
