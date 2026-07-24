//! `stat().st_size` (with `st_blocks` fallback) polling -> `ProgressState`
//! (docs/progress-model.md).
//!
//! Each tick resolves the file `cp` is currently writing ([`crate::proc::select_current`]),
//! stats its destination for `done` and its source for `total`, and feeds a per-file
//! [`ProgressModel`]. Reads go through the [`crate::proc::ProcSource`] / [`StatSource`] seams
//! so the tick logic is unit-tested from fixtures. Any read failure skips the tick and keeps
//! the last value (docs/testing.md A9); a `cp` that has moved on to a new file gets a fresh
//! model with a fresh `total`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::proc::{select_current, ProcSource};
use crate::progress::{ProgressModel, ProgressState};

/// Size information for a path, read from `stat` (docs/progress-model.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    /// `st_size` — the inode's logical length in bytes.
    pub size: u64,
    /// `st_blocks` — allocated 512-byte blocks (real bytes on disk).
    pub blocks: u64,
}

impl FileStat {
    /// Bytes actually written: the smaller of the logical size and the on-disk block count.
    /// This makes preallocated destinations (size jumps to full) and sparse files track real
    /// progress instead of reporting a fake 100% (docs/testing.md A2/A4).
    pub fn effective_bytes(&self) -> u64 {
        self.size.min(self.blocks.saturating_mul(512))
    }
}

/// Stats a path for its size. Behind a trait so the sampler is testable without real I/O;
/// the Linux implementation is provided separately.
pub trait StatSource {
    /// Read size information for `path`.
    fn stat(&self, path: &Path) -> io::Result<FileStat>;
}

/// The real `stat`-backed [`StatSource`].
pub struct LinuxStatSource;

impl StatSource for LinuxStatSource {
    fn stat(&self, path: &Path) -> io::Result<FileStat> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path)?; // follows symlinks: we want the target file
        Ok(FileStat { size: m.size(), blocks: m.blocks() })
    }
}

/// The per-file model plus the destination it belongs to, so a file change can be detected.
struct CurrentModel {
    dest: PathBuf,
    name: String,
    model: ProgressModel,
}

/// Polls a `cp` process's current file and produces per-file [`ProgressState`] snapshots.
pub struct Sampler<'a, P: ProcSource, S: StatSource> {
    proc: &'a P,
    stat: &'a S,
    pid: u32,
    window: Duration,
    current: Option<CurrentModel>,
}

impl<'a, P: ProcSource, S: StatSource> Sampler<'a, P, S> {
    /// Create a sampler observing `pid`, smoothing rate over `window`.
    pub fn new(proc: &'a P, stat: &'a S, pid: u32, window: Duration) -> Self {
        Self { proc, stat, pid, window, current: None }
    }

    /// Take one sample at `now`. Returns the current file's progress, or `None` when there
    /// is nothing to sample (no growing destination) or a read failed (skip, keep last).
    pub fn tick(&mut self, now: Instant) -> Option<ProgressState> {
        let fds = self.proc.fds(self.pid).ok()?; // A9: proc read failure -> skip
        let cur = select_current(&fds)?; // A8: no destination -> nothing to sample

        let is_new = self.current.as_ref().is_none_or(|c| c.dest != cur.dest);
        if is_new {
            // Establish this file's total once, from the source size (unknown if unstattable).
            let total = cur
                .source
                .as_ref()
                .and_then(|src| self.stat.stat(src).ok())
                .map(|st| st.size);
            let name = cur
                .dest
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.current = Some(CurrentModel {
                dest: cur.dest.clone(),
                name,
                model: ProgressModel::new(total, self.window),
            });
        }

        let cm = self.current.as_mut().expect("current set above");
        let done = match self.stat.stat(&cm.dest) {
            Ok(st) => st.effective_bytes(),
            Err(_) => return None, // A9: skip this sample, keep the model's last value
        };
        cm.model.push(now, done);
        Some(cm.model.snapshot(cm.name.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::{FdEntry, FdKind, ProcSource};
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    const WINDOW: Duration = Duration::from_secs(1);

    // ---- fakes: pokeable between ticks ------------------------------------------------

    struct FakeProc {
        entries: RefCell<Vec<FdEntry>>,
        fail: Cell<bool>,
    }
    impl FakeProc {
        fn new(entries: Vec<FdEntry>) -> Self {
            Self { entries: RefCell::new(entries), fail: Cell::new(false) }
        }
        fn set(&self, entries: Vec<FdEntry>) {
            *self.entries.borrow_mut() = entries;
        }
    }
    impl ProcSource for FakeProc {
        fn fds(&self, _pid: u32) -> io::Result<Vec<FdEntry>> {
            if self.fail.get() {
                Err(io::Error::other("proc fail"))
            } else {
                Ok(self.entries.borrow().clone())
            }
        }
    }

    #[derive(Default)]
    struct FakeStat {
        map: RefCell<HashMap<PathBuf, Result<FileStat, ()>>>,
    }
    impl FakeStat {
        fn set(&self, path: &str, r: Result<FileStat, ()>) {
            self.map.borrow_mut().insert(PathBuf::from(path), r);
        }
    }
    impl StatSource for FakeStat {
        fn stat(&self, path: &Path) -> io::Result<FileStat> {
            match self.map.borrow().get(path).copied() {
                Some(Ok(fs)) => Ok(fs),
                _ => Err(io::Error::other("stat fail")),
            }
        }
    }

    fn write_fd(fd: i32, path: &str) -> FdEntry {
        FdEntry { fd, path: PathBuf::from(path), kind: FdKind::RegularWrite }
    }
    fn read_fd(fd: i32, path: &str) -> FdEntry {
        FdEntry { fd, path: PathBuf::from(path), kind: FdKind::RegularRead }
    }
    fn file(dst: &str, src: Option<&str>) -> Vec<FdEntry> {
        let mut v = vec![write_fd(4, dst)];
        if let Some(s) = src {
            v.push(read_fd(3, s));
        }
        v
    }

    // ---- A2: st_blocks fallback -------------------------------------------------------

    #[test]
    fn effective_bytes_is_min_of_size_and_blocks() {
        // Normal file: size <= blocks*512 -> exact size.
        assert_eq!(FileStat { size: 1000, blocks: 2 }.effective_bytes(), 1000);
        assert_eq!(FileStat { size: 4096, blocks: 8 }.effective_bytes(), 4096);
        // Preallocated: size jumps to full, blocks*512 tracks real writes.
        assert_eq!(FileStat { size: 1_000_000, blocks: 200 }.effective_bytes(), 102_400);
    }

    // ---- basic progress ---------------------------------------------------------------

    #[test]
    fn progress_rises_to_complete() {
        let proc = FakeProc::new(file("/dst/a.iso", Some("/src/a.iso")));
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000, blocks: 2 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();

        stat.set("/dst/a.iso", Ok(FileStat { size: 200, blocks: 1 }));
        let a = s.tick(t0).unwrap();
        assert_eq!(a.name, "a.iso");
        assert_eq!(a.total, Some(1000));
        assert_eq!(a.done, 200);

        stat.set("/dst/a.iso", Ok(FileStat { size: 1000, blocks: 2 }));
        let b = s.tick(t0 + Duration::from_secs(1)).unwrap();
        assert_eq!(b.done, 1000);
        assert_eq!(crate::progress::percent_of(b.done, b.total), Some(100.0));
    }

    #[test]
    fn preallocated_destination_does_not_report_fake_full() {
        // docs/testing.md A2: dest size is already full (preallocated) but only 512 bytes
        // are on disk -> report 512, not 100%.
        let proc = FakeProc::new(file("/dst/a.iso", Some("/src/a.iso")));
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000, blocks: 2 }));
        stat.set("/dst/a.iso", Ok(FileStat { size: 1000, blocks: 1 })); // full size, 512 on disk
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let st = s.tick(Instant::now()).unwrap();
        assert_eq!(st.done, 512);
        assert!(crate::progress::percent_of(st.done, st.total).unwrap() < 100.0);
    }

    // ---- A9: skip on error, keep last -------------------------------------------------

    #[test]
    fn dest_stat_error_skips_tick_and_keeps_model() {
        let proc = FakeProc::new(file("/dst/a.iso", Some("/src/a.iso")));
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000, blocks: 2 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();

        stat.set("/dst/a.iso", Ok(FileStat { size: 500, blocks: 1 }));
        assert_eq!(s.tick(t0).unwrap().done, 500);

        stat.set("/dst/a.iso", Err(())); // transient stat failure
        assert_eq!(s.tick(t0 + Duration::from_millis(500)), None, "skip, no crash");

        stat.set("/dst/a.iso", Ok(FileStat { size: 700, blocks: 2 }));
        let c = s.tick(t0 + Duration::from_secs(1)).unwrap();
        assert_eq!(c.done, 700, "continues from the same model");
        assert_eq!(c.total, Some(1000), "total not reset");
    }

    #[test]
    fn proc_error_skips_tick() {
        let proc = FakeProc::new(file("/dst/a.iso", Some("/src/a.iso")));
        let stat = FakeStat::default();
        stat.set("/dst/a.iso", Ok(FileStat { size: 1, blocks: 1 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        proc.fail.set(true);
        assert_eq!(s.tick(Instant::now()), None);
    }

    #[test]
    fn no_current_file_returns_none() {
        // docs/testing.md A8: no growing destination -> nothing to sample.
        let proc = FakeProc::new(vec![read_fd(3, "/src/a.iso")]); // read only, no write
        let stat = FakeStat::default();
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        assert_eq!(s.tick(Instant::now()), None);
    }

    #[test]
    fn absent_source_gives_indeterminate_total() {
        // docs/testing.md A10 downstream: source not a regular file -> total unknown.
        let proc = FakeProc::new(file("/dst/a.iso", None));
        let stat = FakeStat::default();
        stat.set("/dst/a.iso", Ok(FileStat { size: 300, blocks: 1 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let st = s.tick(Instant::now()).unwrap();
        assert_eq!(st.total, None);
        assert_eq!(crate::progress::percent_of(st.done, st.total), None);
    }

    #[test]
    fn linux_stat_source_reads_real_size_and_blocks() {
        let path = std::env::temp_dir().join(format!("cprog_stat_{}", std::process::id()));
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let st = LinuxStatSource.stat(&path).unwrap();
        // Logical size is exact; a non-empty file has some blocks allocated. We avoid asserting
        // blocks*512 >= size, which can fail on transparently-compressed/inline filesystems.
        assert_eq!(st.size, 4096);
        assert!(st.blocks > 0, "a non-empty file has allocated blocks");
        // effective_bytes is the min of the two, so it never exceeds the logical size.
        assert!(st.effective_bytes() <= 4096);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn new_file_resets_total() {
        let proc = FakeProc::new(file("/dst/a", Some("/src/a")));
        let stat = FakeStat::default();
        stat.set("/src/a", Ok(FileStat { size: 1000, blocks: 2 }));
        stat.set("/dst/a", Ok(FileStat { size: 300, blocks: 1 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let a = s.tick(Instant::now()).unwrap();
        assert_eq!(a.total, Some(1000));

        // cp moves on to the next file.
        proc.set(file("/dst/b", Some("/src/b")));
        stat.set("/src/b", Ok(FileStat { size: 2000, blocks: 4 }));
        stat.set("/dst/b", Ok(FileStat { size: 100, blocks: 1 }));
        let b = s.tick(Instant::now() + Duration::from_secs(2)).unwrap();
        assert_eq!(b.name, "b");
        assert_eq!(b.total, Some(2000));
        assert_eq!(b.done, 100);
    }
}
