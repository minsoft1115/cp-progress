//! `/proc/<pid>/fd` readlink to resolve the current dst/src paths
//! (docs/progress-model.md). Never touches `cp`.
//!
//! `cp` copies one file at a time, holding the source open read-only and the destination
//! open for writing. [`select_current`] turns a snapshot of a process's open fds into the
//! write and read candidates, and [`source_for`] pairs a source to a chosen destination by fd
//! number. Both are pure rules over `(fd, path, kind)`, so they are unit-tested from fixtures;
//! reading real `/proc` sits behind the [`ProcSource`] seam and is wired with the sampler.
//!
//! Neither list is trustworthy on its own: descriptors the shell opened are inherited into `cp`
//! and carry low fd numbers, so they sort ahead of the real ones. The destination is resolved by
//! growth in the sampler, the source by its fd position relative to it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The role of an open fd, combining file type and access mode (as read from `/proc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdKind {
    /// A regular file opened read-only — a copy source candidate.
    RegularRead,
    /// A regular file opened for writing — a copy destination candidate.
    RegularWrite,
    /// Anything else (pipe, socket, tty, directory, device) — ignored for progress.
    Other,
}

/// One open fd of the observed process: its number, resolved path, and role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdEntry {
    /// The fd number.
    pub fd: i32,
    /// The path the fd resolves to (via readlink); may be a non-path like `pipe:[…]`.
    pub path: PathBuf,
    /// The fd's role.
    pub kind: FdKind,
}

/// What `cp` currently has open: the plausible destinations and sources, each with its fd number.
///
/// Both lists can contain files that have nothing to do with the copy, because a file descriptor
/// the shell opened (`cprog a b 3>log`, `3<other`) is not close-on-exec and is inherited into
/// `cp` — with a *low* number, so it sorts ahead of what `cp` opens itself. Picking between them
/// needs information this snapshot does not have, so it is left to the caller:
/// [`crate::sampler::Sampler`] picks the destination by growth and then pairs the source with
/// [`source_for`] (docs/progress-model.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentFile {
    /// `(fd, path)` of every regular file above the stdio range open for writing, in fd order.
    pub dests: Vec<(i32, PathBuf)>,
    /// `(fd, path)` of every regular file above the stdio range open read-only, in fd order.
    pub sources: Vec<(i32, PathBuf)>,
}

/// Reads the open fds of a process. Behind a trait so the selection rule can be
/// tested without a real process; the Linux `/proc` reader is provided separately.
pub trait ProcSource {
    /// Snapshot the open fds of `pid`, resolved to `(fd, path, kind)`.
    fn fds(&self, pid: u32) -> io::Result<Vec<FdEntry>>;
}

/// Collect the write and read candidates from a snapshot of open fds.
///
/// Only fds above the stdio range (`fd > 2`) are considered, so a redirected stdin pointing at a
/// regular file is never mistaken for the copy's source (exceptions E11). The filter is on the
/// number, not the kind: under managed mode cp's stdout and stderr are pipes and would be
/// dropped by kind anyway, but stdin stays inherited and can be any regular file. Returns
/// `None` when nothing is open for writing (between files, or during a directory op).
pub fn select_current(entries: &[FdEntry]) -> Option<CurrentFile> {
    let copy_fds = entries.iter().filter(|e| e.fd > 2);
    let of_kind = |want: FdKind| -> Vec<(i32, PathBuf)> {
        copy_fds.clone().filter(|e| e.kind == want).map(|e| (e.fd, e.path.clone())).collect()
    };
    let dests = of_kind(FdKind::RegularWrite);
    if dests.is_empty() {
        return None;
    }
    Some(CurrentFile { sources: of_kind(FdKind::RegularRead), dests })
}

/// The source belonging to the destination at `dest_fd`.
///
/// `cp` copies one file at a time and opens its source immediately before its destination, so the
/// source of the copy in flight is the **highest-numbered read fd below the destination**. That
/// rule excludes both a read fd the shell handed down (low number, opened long before) and one
/// opened after the destination, neither of which can be this copy's source.
///
/// Growth cannot be used here the way it is for destinations (#6): a source is only read, so its
/// size never changes. When no read fd sits below the destination the answer is `None` rather than
/// a guess — `total` stays unknown and the bar renders indeterminate, which beats a ratio measured
/// against the wrong file.
pub fn source_for(dest_fd: i32, sources: &[(i32, PathBuf)]) -> Option<PathBuf> {
    sources
        .iter()
        .filter(|(fd, _)| *fd < dest_fd)
        .max_by_key(|(fd, _)| *fd)
        .map(|(_, path)| path.clone())
}

/// The real `/proc`-backed [`ProcSource`].
pub struct LinuxProcSource;

impl ProcSource for LinuxProcSource {
    fn fds(&self, pid: u32) -> io::Result<Vec<FdEntry>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(format!("/proc/{pid}/fd"))? {
            let entry = entry?;
            // A non-numeric name or an fd that vanished mid-scan is skipped, not fatal.
            let Some(fd) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            let Ok(target) = fs::read_link(entry.path()) else {
                continue;
            };
            out.push(FdEntry { fd, path: target, kind: classify_fd(&entry.path(), pid, fd) });
        }
        Ok(out)
    }
}

/// Classify an fd by resolving its target's file type (via the magic `/proc` symlink) and, for
/// regular files, its access mode from `fdinfo`.
fn classify_fd(fd_link: &Path, pid: u32, fd: i32) -> FdKind {
    match fs::metadata(fd_link) {
        // metadata follows the magic link to the target inode; non-regular -> ignored.
        Ok(m) if m.is_file() => match access_mode(pid, fd) {
            Some(0) => FdKind::RegularRead,  // O_RDONLY -> source
            Some(_) => FdKind::RegularWrite, // O_WRONLY / O_RDWR -> destination
            None => FdKind::Other,           // unknown access -> don't guess
        },
        _ => FdKind::Other,
    }
}

/// Read the `O_ACCMODE` bits from `/proc/<pid>/fdinfo/<fd>` (`flags:` is octal).
fn access_mode(pid: u32, fd: i32) -> Option<u32> {
    let content = fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")).ok()?;
    content.lines().find_map(|line| {
        let rest = line.strip_prefix("flags:")?;
        u32::from_str_radix(rest.trim(), 8).ok().map(|flags| flags & 0o3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(fd: i32, path: &str, kind: FdKind) -> FdEntry {
        FdEntry { fd, path: PathBuf::from(path), kind }
    }

    /// Standard stdio fds a cp process holds (captured pipes / inherited tty) — all `Other`.
    fn stdio() -> [FdEntry; 3] {
        [
            entry(0, "/dev/pts/1", FdKind::Other),
            entry(1, "pipe:[1]", FdKind::Other),
            entry(2, "pipe:[2]", FdKind::Other),
        ]
    }

    #[test]
    fn picks_write_dest_and_read_source() {
        let mut e = stdio().to_vec();
        e.push(entry(3, "/src/a.iso", FdKind::RegularRead));
        e.push(entry(4, "/dst/a.iso", FdKind::RegularWrite));
        let cur = select_current(&e).unwrap();
        assert_eq!(cur.dests, vec![(4, PathBuf::from("/dst/a.iso"))]);
        assert_eq!(source_for(4, &cur.sources), Some(PathBuf::from("/src/a.iso")));
    }

    // ---- #11: an inherited *read* fd must not be taken for the source --------------------

    #[test]
    fn inherited_read_fd_is_not_taken_as_the_source() {
        // `exec 3</etc/passwd` before cprog: fd 3 is inherited into cp and sorts first, but the
        // source of the copy in flight is the read fd cp opened just before the destination.
        let mut e = stdio().to_vec();
        e.push(entry(3, "/etc/passwd", FdKind::RegularRead)); // inherited from the shell
        e.push(entry(4, "/src/big.iso", FdKind::RegularRead)); // the real source
        e.push(entry(5, "/dst/big.iso", FdKind::RegularWrite));
        let cur = select_current(&e).unwrap();
        assert_eq!(
            source_for(5, &cur.sources),
            Some(PathBuf::from("/src/big.iso")),
            "the highest read fd below the destination is the source"
        );
    }

    #[test]
    fn a_read_fd_above_the_destination_is_ignored() {
        // An unrelated file opened after the destination cannot be this copy's source.
        let mut e = stdio().to_vec();
        e.push(entry(3, "/src/a.iso", FdKind::RegularRead));
        e.push(entry(4, "/dst/a.iso", FdKind::RegularWrite));
        e.push(entry(9, "/some/later-read", FdKind::RegularRead));
        let cur = select_current(&e).unwrap();
        assert_eq!(source_for(4, &cur.sources), Some(PathBuf::from("/src/a.iso")));
    }

    #[test]
    fn no_read_fd_below_the_destination_means_no_source() {
        // Rather than guess, report no source: total stays unknown and the bar goes
        // indeterminate instead of showing a ratio against the wrong file.
        let mut e = stdio().to_vec();
        e.push(entry(3, "/dst/a.iso", FdKind::RegularWrite));
        e.push(entry(4, "/unrelated", FdKind::RegularRead));
        let cur = select_current(&e).unwrap();
        assert_eq!(source_for(3, &cur.sources), None);
    }

    #[test]
    fn with_both_decoys_present_only_the_read_one_is_excluded_here() {
        // The two decoys together: fd 3 write (#6) and fd 4 read (#11). Only the read decoy is
        // dropped at this layer — `select_current` reports *every* write candidate and leaves the
        // choice to growth in `sampler::choose_dest` (E17). The old name promised both, while the
        // assertion below says the write decoy is included, which is the honest half (#61).
        let mut e = stdio().to_vec();
        e.push(entry(3, "/tmp/decoy.log", FdKind::RegularWrite));
        e.push(entry(4, "/etc/passwd", FdKind::RegularRead));
        e.push(entry(5, "/src/a.iso", FdKind::RegularRead));
        e.push(entry(6, "/dst/a.iso", FdKind::RegularWrite));
        let cur = select_current(&e).unwrap();
        assert_eq!(
            cur.dests,
            vec![(3, PathBuf::from("/tmp/decoy.log")), (6, PathBuf::from("/dst/a.iso"))]
        );
        // Given the destination the sampler picks by growth (fd 6), the source pairs to fd 5.
        assert_eq!(source_for(6, &cur.sources), Some(PathBuf::from("/src/a.iso")));
    }

    #[test]
    fn no_write_fd_means_no_current_file() {
        // docs/testing.md A8: between files / during a directory op there is no growing
        // destination -> no bar.
        let mut e = stdio().to_vec();
        e.push(entry(3, "/src/a.iso", FdKind::RegularRead));
        assert_eq!(select_current(&e), None);
    }

    #[test]
    fn special_source_gives_indeterminate_total() {
        // docs/testing.md A10: source is a fifo/device -> not a RegularRead -> total unknown,
        // but the destination bar still shows (indeterminate).
        //
        // "A destination with no source at all" is the same branch: the kind filter leaves
        // `sources` empty either way and `source_for` answers `None`. It had its own test, which
        // is duplication rather than coverage (E18, #61 B).
        let mut e = stdio().to_vec();
        e.push(entry(3, "pipe:[99]", FdKind::Other)); // source fifo
        e.push(entry(4, "/dst/a.iso", FdKind::RegularWrite));
        let cur = select_current(&e).unwrap();
        assert_eq!(cur.dests, vec![(4, PathBuf::from("/dst/a.iso"))]);
        assert_eq!(source_for(4, &cur.sources), None);
    }

    #[test]
    fn redirected_low_fds_are_not_selected() {
        // `cprog a b < in` in a terminal: cp inherits a regular-file stdin, which is stdio and
        // not one of its copy fds. Only fd > 2 counts.
        //
        // exceptions E11: the exclusion is by fd *number*, not by kind, and fd 0 is the half of
        // that rule which is actually reachable. Managed always hands cp pipes for stdout and
        // stderr, so those two can never be regular files; stdin is the one stdio fd that stays
        // inherited. Without the number rule it would join the read candidates and — being the
        // only read fd below the destination — become the source, making `total` the size of
        // whatever stdin happens to point at.
        let mut e = vec![
            entry(0, "/in", FdKind::RegularRead),
            entry(1, "pipe:[1]", FdKind::Other),
            entry(2, "pipe:[2]", FdKind::Other),
        ];
        assert_eq!(select_current(&e), None, "stdio alone is not a copy in flight");

        e.push(entry(3, "/dst/a.iso", FdKind::RegularWrite));
        let cur = select_current(&e).unwrap();
        assert_eq!(cur.dests, vec![(3, PathBuf::from("/dst/a.iso"))]);
        assert_eq!(source_for(3, &cur.sources), None, "a redirected stdin is not the source");

        // The boundary itself, at fd 2. Managed mode cannot produce a regular-file fd 2 — it is
        // a pipe by construction — so this half is defensive. It is still written as a number
        // rather than left to the kind filter, because that is what keeps the rule true if the
        // capture ever changes shape.
        let stdio_as_files = vec![
            entry(0, "/in", FdKind::RegularRead),
            entry(1, "/out", FdKind::RegularWrite),
            entry(2, "/err", FdKind::RegularWrite),
        ];
        assert_eq!(select_current(&stdio_as_files), None);
    }

    #[test]
    fn a_regular_file_on_a_stdio_number_is_still_not_a_candidate() {
        // The fd-number rule at its boundary, with a real destination present so the filter has
        // something to choose *between*.
        //
        // The old name and comment called fd 1 a "stdout redirect", which cannot occur: a
        // redirected stdout is not a terminal, so the run is passthrough (B1/B2), and managed
        // always hands cp a pipe there (E11). The rule is worth stating as what it is — a number
        // filter — rather than as a scenario the mode selection rules out (#61).
        let e = vec![
            entry(1, "/some/regular/file", FdKind::RegularWrite),
            entry(3, "/src/a.iso", FdKind::RegularRead),
            entry(4, "/dst/a.iso", FdKind::RegularWrite),
        ];
        let cur = select_current(&e).unwrap();
        assert_eq!(cur.dests, vec![(4, PathBuf::from("/dst/a.iso"))]);
        assert_eq!(source_for(4, &cur.sources), Some(PathBuf::from("/src/a.iso")));
    }

    #[test]
    fn inherited_write_fd_is_reported_as_an_extra_candidate() {
        // #6: `exec 3>decoy.log` before running cprog leaves fd 3 open for writing in cp, and it
        // sorts *before* the real destination. Selection must not silently take the first one —
        // both are surfaced so the sampler can disambiguate by growth.
        let e = vec![
            entry(3, "/tmp/decoy.log", FdKind::RegularWrite), // inherited from the shell
            entry(4, "/src/a.iso", FdKind::RegularRead),
            entry(5, "/dst/a.iso", FdKind::RegularWrite), // the real destination
        ];
        let cur = select_current(&e).unwrap();
        assert_eq!(
            cur.dests,
            vec![(3, PathBuf::from("/tmp/decoy.log")), (5, PathBuf::from("/dst/a.iso"))],
            "both write candidates are reported, in fd order"
        );
        assert_eq!(source_for(5, &cur.sources), Some(PathBuf::from("/src/a.iso")));
    }

    // ---- LinuxProcSource against our own /proc (no external tools) --------------------

    #[test]
    fn linux_proc_source_classifies_our_open_files() {
        use std::fs::File;
        use std::os::fd::AsRawFd;
        let pid = std::process::id();
        // Skip where fdinfo access is restricted (e.g. hidepid mounts): access mode is then
        // unavailable and every fd degrades to `Other` by design.
        if std::fs::read_to_string(format!("/proc/{pid}/fdinfo/1")).is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("cprog_proc_{pid}"));
        std::fs::create_dir_all(&dir).unwrap();
        let wpath = dir.join("wfile");
        let rpath = dir.join("rfile");
        std::fs::write(&rpath, b"data").unwrap();
        let w = File::create(&wpath).unwrap(); // O_WRONLY
        let _r = File::open(&rpath).unwrap(); // O_RDONLY

        // Skip where the sandbox forbids stat-through-the-magic-`/proc`-symlink — the exact call
        // `classify_fd` makes. Probing our own known regular fd separates an environment
        // restriction (skip) from a real classification regression (which fails the asserts).
        if std::fs::metadata(format!("/proc/{pid}/fd/{}", w.as_raw_fd())).is_err() {
            return;
        }

        let fds = LinuxProcSource.fds(pid).unwrap();
        assert!(
            fds.iter().any(|e| e.kind == FdKind::RegularWrite && e.path.ends_with("wfile")),
            "expected a RegularWrite for wfile in {fds:?}"
        );
        assert!(
            fds.iter().any(|e| e.kind == FdKind::RegularRead && e.path.ends_with("rfile")),
            "expected a RegularRead for rfile in {fds:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
