//! `/proc/<pid>/fd` readlink to resolve the current dst/src paths
//! (docs/progress-model.md). Never touches `cp`.
//!
//! `cp` copies one file at a time, holding the source open read-only and the destination
//! open for writing. The pure [`select_current`] rule turns a snapshot of a process's open
//! fds into the current destination (whose size gives `done`) and source (whose size gives
//! `total`). Reading real `/proc` sits behind the [`ProcSource`] seam so this rule is
//! unit-tested from fixtures; the concrete Linux reader is wired with the sampler.

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

/// The file `cp` is currently copying: the growing destination and, when identifiable, its
/// source. A missing `source` leaves `total` unknown (indeterminate bar).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentFile {
    /// Destination path — polled for `done` (docs/progress-model.md).
    pub dest: PathBuf,
    /// Source path — stat'd once for `total`; `None` when the source is not a regular file.
    pub source: Option<PathBuf>,
}

/// Reads the open fds of a process. Behind a trait so the selection rule can be
/// tested without a real process; the Linux `/proc` reader is provided separately.
pub trait ProcSource {
    /// Snapshot the open fds of `pid`, resolved to `(fd, path, kind)`.
    fn fds(&self, pid: u32) -> io::Result<Vec<FdEntry>>;
}

/// Pick the current destination/source from a snapshot of open fds.
///
/// Only fds above the stdio range (`fd > 2`) are considered, so a redirected stdin/stdout
/// pointing at a regular file is never mistaken for the copy's source/destination. Returns
/// `None` when no growing destination is open (between files, or during a directory op).
pub fn select_current(entries: &[FdEntry]) -> Option<CurrentFile> {
    let mut copy_fds = entries.iter().filter(|e| e.fd > 2);
    let dest = copy_fds
        .clone()
        .find(|e| e.kind == FdKind::RegularWrite)?
        .path
        .clone();
    let source = copy_fds
        .find(|e| e.kind == FdKind::RegularRead)
        .map(|e| e.path.clone());
    Some(CurrentFile { dest, source })
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
        assert_eq!(cur.dest, PathBuf::from("/dst/a.iso"));
        assert_eq!(cur.source, Some(PathBuf::from("/src/a.iso")));
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
        let mut e = stdio().to_vec();
        e.push(entry(3, "pipe:[99]", FdKind::Other)); // source fifo
        e.push(entry(4, "/dst/a.iso", FdKind::RegularWrite));
        let cur = select_current(&e).unwrap();
        assert_eq!(cur.dest, PathBuf::from("/dst/a.iso"));
        assert_eq!(cur.source, None);
    }

    #[test]
    fn multiple_regular_fds_pick_the_write_destination() {
        // docs/testing.md A11: among several regular fds, the write-mode one is the growing
        // destination.
        let mut e = stdio().to_vec();
        e.push(entry(3, "/src/a.iso", FdKind::RegularRead));
        e.push(entry(4, "/dst/a.iso", FdKind::RegularWrite));
        e.push(entry(5, "/some/other-read", FdKind::RegularRead));
        let cur = select_current(&e).unwrap();
        assert_eq!(cur.dest, PathBuf::from("/dst/a.iso"));
        assert_eq!(cur.source, Some(PathBuf::from("/src/a.iso")));
    }

    #[test]
    fn redirected_low_fds_are_not_selected() {
        // `cprog a b < in > out` in a terminal: fd0/fd1 are regular files but are stdio, not
        // cp's copy fds. Only fd > 2 counts.
        let e = vec![
            entry(0, "/in", FdKind::RegularRead),
            entry(1, "/out", FdKind::RegularWrite),
            entry(2, "/dev/pts/1", FdKind::Other),
        ];
        assert_eq!(select_current(&e), None);
    }

    #[test]
    fn real_dest_preferred_over_redirected_stdout() {
        let e = vec![
            entry(1, "/redirect/out", FdKind::RegularWrite), // stdout redirect, ignored
            entry(3, "/src/a.iso", FdKind::RegularRead),
            entry(4, "/dst/a.iso", FdKind::RegularWrite),
        ];
        let cur = select_current(&e).unwrap();
        assert_eq!(cur.dest, PathBuf::from("/dst/a.iso"));
    }

    #[test]
    fn dest_without_source_is_selected() {
        let e = vec![entry(4, "/dst/a.iso", FdKind::RegularWrite)];
        let cur = select_current(&e).unwrap();
        assert_eq!(cur.dest, PathBuf::from("/dst/a.iso"));
        assert_eq!(cur.source, None);
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
