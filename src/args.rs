//! Argument validation plus minimal interactive/`-v` inspection (docs/architecture.md).
//! `cp` arguments are never re-parsed; only the checks needed for the run policy.
//!
//! We scan the `cp` argument vector with `lexopt` to answer two questions — is an
//! interactive flag present (forces passthrough), and did the user already pass `-v`
//! (avoid double injection). Value-consuming options (`-S`/`--suffix`, `-t`/
//! `--target-directory`) have their value consumed so a value like `-i` is not mistaken
//! for a flag. Everything else is ignored and handed to `cp` untouched.

use std::ffi::OsString;

use lexopt::prelude::*;
use lexopt::Parser;

/// A problem found while inspecting the argument vector.
#[derive(Debug, PartialEq, Eq)]
pub enum ArgError {
    /// No operands at all -> usage error (docs/testing.md D5), maps to `Fatal::Usage`.
    Empty,
    /// The scan itself failed (e.g. a required option value is missing). The caller should
    /// fall back to passthrough conservatively (docs/runtime-model.md).
    Scan(String),
}

/// What cprog needs to know about the `cp` argument vector to choose a run mode.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ArgInspection {
    /// An interactive flag (`-i`, `--interactive[=…]`) is present -> force passthrough.
    pub interactive: bool,
    /// The user already passed `-v`/`--verbose` -> do not inject a second one.
    pub verbose: bool,
    /// `--help`/`--version` is present: cp just prints and exits without copying, so there is
    /// nothing to monitor -> force passthrough (byte-identical, no summary).
    pub informational: bool,
}

/// Inspect the `cp` arguments (no binary name) for interactive/verbose intent.
pub fn inspect(args: &[OsString]) -> Result<ArgInspection, ArgError> {
    if args.is_empty() {
        return Err(ArgError::Empty);
    }
    let mut found = ArgInspection::default();
    let mut parser = Parser::from_args(args.iter().cloned());
    loop {
        match parser.next().map_err(|e| ArgError::Scan(e.to_string()))? {
            None => break,
            Some(arg) => match arg {
                Short('i') | Long("interactive") => {
                    found.interactive = true;
                    // Swallow any attached `=value` (e.g. `--interactive=always`).
                    let _ = parser.optional_value();
                }
                Short('v') | Long("verbose") => found.verbose = true,
                Long("help") | Long("version") => found.informational = true,
                Short('S') | Long("suffix") | Short('t') | Long("target-directory") => {
                    // Consume the option's value so it cannot be misread as a flag.
                    parser.value().map_err(|e| ArgError::Scan(e.to_string()))?;
                }
                // Any other option or operand: ignore; it passes through to cp unchanged.
                _ => {}
            },
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn args(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    // ---- empty (D5) ------------------------------------------------------------------

    #[test]
    fn no_args_is_usage_error() {
        assert_eq!(inspect(&args(&[])), Err(ArgError::Empty));
    }

    #[test]
    fn plain_files_have_no_flags() {
        let i = inspect(&args(&["a.iso", "/mnt/a.iso"])).unwrap();
        assert!(!i.interactive);
        assert!(!i.verbose);
        assert!(!i.informational);
    }

    // ---- informational flags (--help / --version) ------------------------------------

    #[test]
    fn help_and_version_are_informational() {
        // These make cp print and exit without copying, so cprog should just pass through.
        assert!(inspect(&args(&["--help"])).unwrap().informational);
        assert!(inspect(&args(&["--version"])).unwrap().informational);
        // Present alongside other args (cp processes them first regardless).
        assert!(inspect(&args(&["-a", "--help", "src", "dst"])).unwrap().informational);
    }

    #[test]
    fn normal_copy_is_not_informational() {
        assert!(!inspect(&args(&["a", "b"])).unwrap().informational);
    }

    // ---- interactive detection (E2, runtime-model) -----------------------------------

    #[test]
    fn short_interactive() {
        assert!(inspect(&args(&["-i", "a", "b"])).unwrap().interactive);
    }

    #[test]
    fn long_interactive() {
        assert!(inspect(&args(&["--interactive", "a", "b"])).unwrap().interactive);
    }

    #[test]
    fn long_interactive_with_attached_value() {
        // runtime-model lists `--interactive=…` as an interactive form to detect.
        assert!(inspect(&args(&["--interactive=always", "a", "b"])).unwrap().interactive);
    }

    #[test]
    fn bundled_short_interactive() {
        let i = inspect(&args(&["-ai", "a", "b"])).unwrap(); // -a -i
        assert!(i.interactive);
        assert!(!i.verbose);
    }

    #[test]
    fn permuted_interactive_after_operand() {
        // cp permutes options after operands; so does our scan.
        assert!(inspect(&args(&["src", "-i", "dst"])).unwrap().interactive);
    }

    // ---- verbose detection / double-injection guard (B8) -----------------------------

    #[test]
    fn short_verbose() {
        assert!(inspect(&args(&["-v", "a", "b"])).unwrap().verbose);
    }

    #[test]
    fn long_verbose() {
        assert!(inspect(&args(&["--verbose", "a", "b"])).unwrap().verbose);
    }

    #[test]
    fn bundled_short_verbose() {
        assert!(inspect(&args(&["-av", "a", "b"])).unwrap().verbose);
    }

    #[test]
    fn both_verbose_and_interactive() {
        let i = inspect(&args(&["-vi", "a", "b"])).unwrap();
        assert!(i.verbose && i.interactive);
    }

    // ---- value-consuming options must not misread their value as a flag --------------

    #[test]
    fn suffix_value_looking_like_interactive_is_not_a_flag() {
        // `cp -S -i a b` -> suffix is "-i", not the interactive flag.
        let i = inspect(&args(&["-S", "-i", "a", "b"])).unwrap();
        assert!(!i.interactive);
    }

    #[test]
    fn long_suffix_attached_value_looking_like_flag() {
        let i = inspect(&args(&["--suffix=-i", "a", "b"])).unwrap();
        assert!(!i.interactive);
    }

    #[test]
    fn target_directory_value_looking_like_verbose_is_not_a_flag() {
        // `cp -t -v src` -> target dir is "-v", not the verbose flag.
        let i = inspect(&args(&["-t", "-v", "src"])).unwrap();
        assert!(!i.verbose);
    }

    #[test]
    fn long_target_directory_attached_value_looking_like_flag() {
        let i = inspect(&args(&["--target-directory=-v", "src"])).unwrap();
        assert!(!i.verbose);
    }

    #[test]
    fn attached_short_suffix_value() {
        // `-Sbak` -> suffix "bak"; no flags.
        let i = inspect(&args(&["-Sbak", "a", "b"])).unwrap();
        assert!(!i.interactive && !i.verbose);
    }

    // ---- `--` end of options ---------------------------------------------------------

    #[test]
    fn double_dash_makes_following_dash_i_an_operand() {
        let i = inspect(&args(&["--", "-i", "a"])).unwrap();
        assert!(!i.interactive);
    }

    #[test]
    fn double_dash_makes_following_dash_v_an_operand() {
        let i = inspect(&args(&["--", "-v", "a"])).unwrap();
        assert!(!i.verbose);
    }

    // ---- scan failure -> conservative passthrough ------------------------------------

    #[test]
    fn missing_required_value_is_scan_error() {
        // `--suffix` with no value: cp would error too; we signal Scan so the caller
        // falls back to passthrough (runtime-model "인자 검사 실패 -> passthrough").
        assert!(matches!(inspect(&args(&["--suffix"])), Err(ArgError::Scan(_))));
    }
}
