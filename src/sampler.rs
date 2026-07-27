//! `stat().st_size` (with `st_blocks` fallback) polling -> `ProgressState`
//! (docs/progress-model.md).
//!
//! Each tick resolves the file `cp` is currently writing ([`crate::proc::select_current`]),
//! stats its destination for `done` and its source for `total`, and feeds a per-file
//! [`ProgressModel`]. Reads go through the [`crate::proc::ProcSource`] / [`StatSource`] seams
//! so the tick logic is unit-tested from fixtures. A read failure keeps the last value
//! ([`Tick::Skip`], docs/testing.md A9) while having nothing to measure takes the bar down
//! ([`Tick::Idle`]); a `cp` that has moved on to a new file gets a fresh model with a fresh
//! `total` and a freshly chosen [`Basis`].

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::proc::{select_current, source_for, ProcSource};
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
    /// Bytes on disk (`st_blocks * 512`).
    fn disk_bytes(&self) -> u64 {
        self.blocks.saturating_mul(512)
    }

    /// Bytes copied so far, read on the given measurement basis.
    pub fn bytes(&self, basis: Basis) -> u64 {
        match basis {
            Basis::Size => self.size,
            Basis::Blocks => self.disk_bytes(),
        }
    }
}

/// Which `stat` field measures "bytes copied" for the current file (docs/progress-model.md
/// "측정 기준(basis)은 파일마다 한 번만 정한다").
///
/// The two fields fail in opposite directions, so the choice is made once per file rather than
/// reconciled every sample:
///
/// * a **preallocated** destination (`fallocate`) has its full `st_size` from the start, so only
///   `st_blocks` tracks real writes;
/// * a **sparse** destination — which `cp` produces by default (`--sparse=auto`) whenever the
///   source has holes — legitimately has `st_blocks * 512` far below `st_size`, so measuring
///   blocks would under-report and never reach 100%. The same holds on compressing filesystems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Measure `st_size` — the default.
    Size,
    /// Measure `st_blocks * 512` — only for a positively identified preallocated destination.
    Blocks,
}

impl Basis {
    /// Choose the basis from the first sample of a new destination.
    ///
    /// `Size` is the default because `st_blocks` deviates from real progress in *both* directions
    /// (low on sparse/compressed files, high under ext4's speculative preallocation), while
    /// `st_size` states plainly how far the file has logically landed. `Blocks` is used only when
    /// preallocation is positively identified: a non-sparse source, yet a destination that is
    /// already at full length while almost nothing is on disk.
    ///
    /// Known limitation: under ext4's *delayed* allocation `st_blocks` trails `st_size` until
    /// writeback, so a first sample taken when the destination is already at full length can read
    /// as preallocation. The window is narrow — the bar only engages for slow files, whose first
    /// sample lands mid-copy with `st_size` still below `total` — and GNU `cp` does not
    /// preallocate at all, which makes `Blocks` a defensive path rather than a routine one
    /// (docs/exceptions.md E22).
    pub fn detect(first_dest: &FileStat, source: Option<&FileStat>, total: Option<u64>) -> Basis {
        let (Some(src), Some(total)) = (source, total) else {
            return Basis::Size;
        };
        // A source with holes explains a small-blocks destination on its own -> keep Size.
        let source_is_sparse = src.disk_bytes() < src.size;
        let dest_full_already = first_dest.size >= total;
        let dest_blocks_lag = first_dest.disk_bytes() < total;
        if total > 0 && !source_is_sparse && dest_full_already && dest_blocks_lag {
            Basis::Blocks
        } else {
            Basis::Size
        }
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
    /// Fixed for this file once its first sample is taken (docs/progress-model.md).
    basis: Basis,
    model: ProgressModel,
}

/// Polls a `cp` process's current file and produces per-file [`ProgressState`] snapshots.
pub struct Sampler<'a, P: ProcSource, S: StatSource> {
    proc: &'a P,
    stat: &'a S,
    pid: u32,
    window: Duration,
    current: Option<CurrentModel>,
    /// Last observed size per write candidate, kept only while more than one candidate exists
    /// so the growing one can be identified (#6, docs/progress-model.md).
    candidate_sizes: HashMap<PathBuf, u64>,
}

impl<'a, P: ProcSource, S: StatSource> Sampler<'a, P, S> {
    /// Create a sampler observing `pid`, smoothing rate over `window`.
    pub fn new(proc: &'a P, stat: &'a S, pid: u32, window: Duration) -> Self {
        Self { proc, stat, pid, window, current: None, candidate_sizes: HashMap::new() }
    }

    /// Pick which write candidate is the copy destination.
    ///
    /// One candidate is the overwhelmingly common case and is taken as-is, with no extra `stat`.
    /// With several, one of them is not cp's destination — typically a shell redirection
    /// (`cprog a b 3>log`) inherited into `cp`, which sorts *before* the real destination in the
    /// fd table. The copy destination is the file that is actually growing, so candidates are
    /// compared against their previous sizes and the biggest gainer wins. Until there is
    /// something to compare (or if nothing grew) we keep whatever was already being tracked
    /// rather than guess, and otherwise report no choice at all.
    fn choose_dest(&mut self, dests: &[(i32, PathBuf)]) -> Option<(i32, PathBuf)> {
        if let [only] = dests {
            self.candidate_sizes.clear();
            return Some(only.clone());
        }
        let mut best: Option<(u64, (i32, PathBuf))> = None;
        for cand in dests {
            let (_, path) = cand;
            let Ok(st) = self.stat.stat(path) else { continue };
            let previous = self.candidate_sizes.insert(path.clone(), st.size);
            let growth = st.size.saturating_sub(previous.unwrap_or(st.size));
            if growth > 0 && best.as_ref().is_none_or(|(g, _)| growth > *g) {
                best = Some((growth, cand.clone()));
            }
        }
        // Nothing measurably grew this tick: stay on the file we were already tracking, provided
        // it is still open, so a momentary pause does not make the bar jump to another file.
        best.map(|(_, c)| c).or_else(|| {
            let tracked = &self.current.as_ref()?.dest;
            dests.iter().find(|(_, p)| p == tracked).cloned()
        })
    }

    /// Discard the current file's timing history while keeping its identity, `total` and
    /// [`Basis`].
    ///
    /// Called after a job-control stop: `cp` was stopped alongside cprog, so the wall-clock span
    /// across the suspend carries no throughput information and would otherwise be smoothed in as
    /// a near-zero rate for a full window (#9).
    pub fn reset_rate_history(&mut self) {
        if let Some(cm) = self.current.as_mut() {
            cm.model.reset_samples();
        }
    }

    /// Take one sample at `now` (docs/progress-model.md "실패 처리").
    pub fn tick(&mut self, now: Instant) -> Tick {
        let Ok(fds) = self.proc.fds(self.pid) else {
            return Tick::Skip; // A9: proc read failure -> keep the last value
        };
        let Some(cur) = select_current(&fds) else {
            // A8: no growing destination — between files, a directory op, or the file just
            // finished and its fd is closed. Nothing is being copied, so the bar comes down.
            self.current = None;
            self.candidate_sizes.clear();
            return Tick::Idle;
        };
        let Some((dest_fd, dest)) = self.choose_dest(&cur.dests) else {
            return Tick::Skip; // ambiguous for now — keep the last value rather than guess
        };

        let is_new = self.current.as_ref().is_none_or(|c| c.dest != dest);
        if is_new {
            // Establish this file's total once, from the source size (unknown if unstattable).
            // Pair the source to the chosen destination by fd position, so a read fd the shell
            // handed down is not mistaken for it (#11, docs/progress-model.md).
            let source = source_for(dest_fd, &cur.sources)
                .and_then(|src| self.stat.stat(&src).ok());
            let total = source.map(|st| st.size);
            // The first destination sample also fixes the measurement basis for this file. If it
            // cannot be read we skip the tick entirely rather than guess a basis we would be
            // stuck with for the whole file (A9: skip, keep last).
            let Ok(first_dest) = self.stat.stat(&dest) else {
                return Tick::Skip;
            };
            let name = dest
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.current = Some(CurrentModel {
                dest: dest.clone(),
                name,
                basis: Basis::detect(&first_dest, source.as_ref(), total),
                model: ProgressModel::new(total, self.window),
            });
        }

        let cm = self.current.as_mut().expect("current set above");
        let done = match self.stat.stat(&cm.dest) {
            Ok(st) => st.bytes(cm.basis),
            Err(_) => return Tick::Skip, // A9: keep the model's last value
        };
        cm.model.push(now, done);
        Tick::Sample(cm.model.snapshot(cm.name.clone()))
    }
}

/// The outcome of one sampler tick (docs/progress-model.md "실패 처리").
///
/// [`Tick::Skip`] and [`Tick::Idle`] are deliberately distinct: a *failed read* should leave the
/// last value on screen rather than make the bar flicker, whereas *nothing to measure* means the
/// bar must come down. Collapsing both into "keep the last value" leaves a frozen bar on screen
/// after a slow file finishes, because the slow timer only resets on the next `-v` pulse.
#[derive(Debug, Clone, PartialEq)]
pub enum Tick {
    /// A fresh sample for the file currently being copied.
    Sample(ProgressState),
    /// A read failed; keep whatever was published last.
    Skip,
    /// Nothing is being copied right now; clear the published progress.
    Idle,
}

impl Tick {
    /// The sample, if this tick produced one. Test-only: the render loop matches on all three
    /// variants, because telling `Skip` from `Idle` is the whole point of the type.
    #[cfg(test)]
    pub fn sample(self) -> Option<ProgressState> {
        match self {
            Tick::Sample(s) => Some(s),
            _ => None,
        }
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

    // ---- measurement basis, decided once per file (docs/progress-model.md) ------------

    #[test]
    fn bytes_reads_the_chosen_basis() {
        let st = FileStat { size: 1_000_000, blocks: 200 }; // 200 blocks = 102_400 bytes
        assert_eq!(st.bytes(Basis::Size), 1_000_000);
        assert_eq!(st.bytes(Basis::Blocks), 102_400);
    }

    #[test]
    fn preallocated_destination_picks_the_blocks_basis() {
        // docs/testing.md A2: a normal source, but the destination's size is already full on the
        // first sample while almost nothing is on disk -> fallocate -> measure real blocks.
        let src = FileStat { size: 1000, blocks: 2 }; // not sparse
        let first_dst = FileStat { size: 1000, blocks: 1 }; // full size, 512 bytes on disk
        assert_eq!(Basis::detect(&first_dst, Some(&src), Some(1000)), Basis::Blocks);
    }

    #[test]
    fn sparse_source_keeps_the_size_basis() {
        // #3: `cp --sparse=auto` (the default) makes the destination sparse when the source has
        // holes. blocks*512 << size is then CORRECT, so measuring blocks would under-report.
        let src = FileStat { size: 209_715_200, blocks: 2056 }; // 200 MiB, mostly hole
        let first_dst = FileStat { size: 209_715_200, blocks: 8 }; // dest already seeked to full
        assert_eq!(
            Basis::detect(&first_dst, Some(&src), Some(209_715_200)),
            Basis::Size,
            "a sparse source must not be mistaken for a preallocated destination"
        );
    }

    #[test]
    fn ordinary_copy_keeps_the_size_basis() {
        let src = FileStat { size: 1000, blocks: 2 };
        let first_dst = FileStat { size: 300, blocks: 1 }; // still growing
        assert_eq!(Basis::detect(&first_dst, Some(&src), Some(1000)), Basis::Size);
    }

    #[test]
    fn unknown_total_or_source_keeps_the_size_basis() {
        let dst = FileStat { size: 300, blocks: 1 };
        assert_eq!(Basis::detect(&dst, None, None), Basis::Size);
        assert_eq!(Basis::detect(&dst, Some(&FileStat { size: 1000, blocks: 2 }), None), Basis::Size);
    }

    #[test]
    fn compressed_filesystem_keeps_the_size_basis() {
        // btrfs `compress` / ZFS: blocks*512 < size is normal for the destination too, but the
        // size grows gradually, so this is never mistaken for preallocation.
        let src = FileStat { size: 1_000_000, blocks: 1954 };
        let first_dst = FileStat { size: 50_000, blocks: 40 }; // compressed, still growing
        assert_eq!(Basis::detect(&first_dst, Some(&src), Some(1_000_000)), Basis::Size);
    }

    #[test]
    fn sparse_destination_progress_reaches_completion() {
        // #3 end to end: a 200 MiB mostly-hole file must read 100 % when the copy finishes,
        // not the ~0.5 % the old min(size, blocks*512) clamp produced.
        const TOTAL: u64 = 209_715_200;
        let proc = FakeProc::new(file("/dst/holey", Some("/src/holey")));
        let stat = FakeStat::default();
        stat.set("/src/holey", Ok(FileStat { size: TOTAL, blocks: 2056 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();

        stat.set("/dst/holey", Ok(FileStat { size: TOTAL / 2, blocks: 1028 }));
        let mid = s.tick(t0).sample().unwrap();
        assert_eq!(crate::progress::percent_of(mid.done, mid.total), Some(50.0));

        stat.set("/dst/holey", Ok(FileStat { size: TOTAL, blocks: 2056 }));
        let end = s.tick(t0 + Duration::from_secs(1)).sample().unwrap();
        assert_eq!(end.done, TOTAL);
        assert_eq!(crate::progress::percent_of(end.done, end.total), Some(100.0));
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
        let a = s.tick(t0).sample().unwrap();
        assert_eq!(a.name, "a.iso");
        assert_eq!(a.total, Some(1000));
        assert_eq!(a.done, 200);

        stat.set("/dst/a.iso", Ok(FileStat { size: 1000, blocks: 2 }));
        let b = s.tick(t0 + Duration::from_secs(1)).sample().unwrap();
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
        let st = s.tick(Instant::now()).sample().unwrap();
        assert_eq!(st.done, 512);
        assert!(crate::progress::percent_of(st.done, st.total).unwrap() < 100.0);
    }

    #[test]
    fn basis_is_fixed_for_the_file_and_re_detected_on_the_next_one() {
        // The basis is decided once per destination: a preallocated file keeps the blocks basis
        // even as its blocks fill in, and the next (sparse) file starts the decision over.
        let proc = FakeProc::new(file("/dst/pre", Some("/src/pre")));
        let stat = FakeStat::default();
        stat.set("/src/pre", Ok(FileStat { size: 1000, blocks: 2 }));
        stat.set("/dst/pre", Ok(FileStat { size: 1000, blocks: 1 })); // preallocated
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();
        assert_eq!(s.tick(t0).sample().unwrap().done, 512, "blocks basis chosen");

        stat.set("/dst/pre", Ok(FileStat { size: 1000, blocks: 2 }));
        assert_eq!(s.tick(t0 + Duration::from_millis(100)).sample().unwrap().done, 1024, "still blocks");

        // cp moves on to a sparse file -> the basis is decided again, and must be size.
        proc.set(file("/dst/sp", Some("/src/sp")));
        stat.set("/src/sp", Ok(FileStat { size: 100_000, blocks: 8 })); // sparse source
        stat.set("/dst/sp", Ok(FileStat { size: 60_000, blocks: 4 }));
        let st = s.tick(t0 + Duration::from_secs(2)).sample().unwrap();
        assert_eq!(st.done, 60_000, "size basis for the sparse file");
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
        assert_eq!(s.tick(t0).sample().unwrap().done, 500);

        stat.set("/dst/a.iso", Err(())); // transient stat failure
        assert_eq!(s.tick(t0 + Duration::from_millis(500)), Tick::Skip, "skip, no crash");

        stat.set("/dst/a.iso", Ok(FileStat { size: 700, blocks: 2 }));
        let c = s.tick(t0 + Duration::from_secs(1)).sample().unwrap();
        assert_eq!(c.done, 700, "continues from the same model");
        assert_eq!(c.total, Some(1000), "total not reset");
    }

    // ---- #6: several write candidates (an inherited shell fd alongside the real dest) --------

    /// fds as a shell redirection leaves them: the inherited write fd sorts *before* the real
    /// destination, so a naive "first write fd" pick takes the wrong file.
    fn decoy_and_dest() -> Vec<FdEntry> {
        vec![
            write_fd(3, "/tmp/decoy.log"), // `exec 3>decoy.log`, inherited into cp
            read_fd(4, "/src/a.iso"),
            write_fd(5, "/dst/a.iso"), // the real destination
        ]
    }

    #[test]
    fn growing_candidate_wins_over_an_inherited_write_fd() {
        let proc = FakeProc::new(decoy_and_dest());
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000, blocks: 2 }));
        stat.set("/tmp/decoy.log", Ok(FileStat { size: 40, blocks: 1 })); // never grows
        stat.set("/dst/a.iso", Ok(FileStat { size: 100, blocks: 1 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();

        // First tick has nothing to compare against, so it refuses to guess.
        assert_eq!(s.tick(t0), Tick::Skip, "no growth history yet -> no guess");

        // The destination grows; the decoy does not.
        stat.set("/dst/a.iso", Ok(FileStat { size: 400, blocks: 1 }));
        let st = s.tick(t0 + Duration::from_millis(100)).sample().unwrap();
        assert_eq!(st.name, "a.iso", "the growing file is the destination");
        assert_eq!(st.done, 400);
        assert_eq!(st.total, Some(1000));
    }

    #[test]
    fn single_candidate_is_used_without_growth_history() {
        // The common case must not pay the extra tick: one candidate is the destination at once.
        let proc = FakeProc::new(file("/dst/a.iso", Some("/src/a.iso")));
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000, blocks: 2 }));
        stat.set("/dst/a.iso", Ok(FileStat { size: 100, blocks: 1 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        assert_eq!(s.tick(Instant::now()).sample().unwrap().done, 100);
    }

    #[test]
    fn a_paused_destination_is_not_abandoned_for_another_candidate() {
        // Once tracking a file, a tick where nothing grew must keep it rather than jump to the
        // decoy — copies stall briefly all the time.
        let proc = FakeProc::new(decoy_and_dest());
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000, blocks: 2 }));
        stat.set("/tmp/decoy.log", Ok(FileStat { size: 40, blocks: 1 }));
        stat.set("/dst/a.iso", Ok(FileStat { size: 100, blocks: 1 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();
        assert_eq!(s.tick(t0), Tick::Skip);
        stat.set("/dst/a.iso", Ok(FileStat { size: 400, blocks: 1 }));
        assert_eq!(s.tick(t0 + Duration::from_millis(100)).sample().unwrap().name, "a.iso");

        // Nothing changes this tick: stay on a.iso.
        let st = s.tick(t0 + Duration::from_millis(200)).sample().unwrap();
        assert_eq!(st.name, "a.iso", "a stalled copy keeps its bar");
        assert_eq!(st.done, 400);
    }

    #[test]
    fn a_growing_decoy_does_not_hijack_a_tracked_destination() {
        // If the decoy grows more than the destination in one tick it does win — but only
        // because it is genuinely the file being written to. Verify the mechanism is symmetric
        // so the rule stays "the growing file", not "the file we happened to pick first".
        let proc = FakeProc::new(decoy_and_dest());
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000, blocks: 2 }));
        stat.set("/tmp/decoy.log", Ok(FileStat { size: 40, blocks: 1 }));
        stat.set("/dst/a.iso", Ok(FileStat { size: 100, blocks: 1 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();
        assert_eq!(s.tick(t0), Tick::Skip);

        stat.set("/dst/a.iso", Ok(FileStat { size: 110, blocks: 1 })); // +10
        stat.set("/tmp/decoy.log", Ok(FileStat { size: 9000, blocks: 18 })); // +8960
        let st = s.tick(t0 + Duration::from_millis(100)).sample().unwrap();
        assert_eq!(st.name, "decoy.log", "the biggest gainer is what is being written");
    }

    #[test]
    fn an_inherited_read_fd_does_not_become_the_total() {
        // #11 at the sampler level: `exec 3<other` leaves a low read fd in cp. `total` must come
        // from the real source (paired by fd position), not from the decoy — otherwise the ratio
        // is measured against the wrong file and a small decoy pins the bar at 100%.
        let proc = FakeProc::new(vec![
            read_fd(3, "/etc/passwd"),   // inherited from the shell, tiny
            read_fd(4, "/src/big.iso"),  // the real source
            write_fd(5, "/dst/big.iso"),
        ]);
        let stat = FakeStat::default();
        stat.set("/etc/passwd", Ok(FileStat { size: 2_000, blocks: 4 }));
        stat.set("/src/big.iso", Ok(FileStat { size: 1_000_000, blocks: 1954 }));
        stat.set("/dst/big.iso", Ok(FileStat { size: 250_000, blocks: 489 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);

        let st = s.tick(Instant::now()).sample().unwrap();
        assert_eq!(st.total, Some(1_000_000), "total comes from the real source");
        assert_eq!(crate::progress::percent_of(st.done, st.total), Some(25.0));
    }

    #[test]
    fn both_inherited_fds_together_still_track_the_real_copy() {
        // The full hostile case: an inherited write fd (#6) *and* an inherited read fd (#11).
        let proc = FakeProc::new(vec![
            write_fd(3, "/tmp/decoy.log"),
            read_fd(4, "/etc/passwd"),
            read_fd(5, "/src/a.iso"),
            write_fd(6, "/dst/a.iso"),
        ]);
        let stat = FakeStat::default();
        stat.set("/tmp/decoy.log", Ok(FileStat { size: 10, blocks: 1 })); // never grows
        stat.set("/etc/passwd", Ok(FileStat { size: 2_000, blocks: 4 }));
        stat.set("/src/a.iso", Ok(FileStat { size: 800, blocks: 2 }));
        stat.set("/dst/a.iso", Ok(FileStat { size: 100, blocks: 1 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();
        assert_eq!(s.tick(t0), Tick::Skip, "two write candidates, no growth history yet");

        stat.set("/dst/a.iso", Ok(FileStat { size: 400, blocks: 1 }));
        let st = s.tick(t0 + Duration::from_millis(100)).sample().unwrap();
        assert_eq!(st.name, "a.iso", "destination by growth (#6)");
        assert_eq!(st.total, Some(800), "source by fd pairing (#11)");
        assert_eq!(st.done, 400);
    }

    #[test]
    fn finished_file_reports_idle_not_skip() {
        // #7: once the destination fd closes, the sampler must say "nothing to measure" (Idle)
        // rather than "read failed" (Skip). Skip would keep the last snapshot published, leaving
        // a frozen bar on screen until the next `-v` pulse — or until cp exits, for the last file.
        let proc = FakeProc::new(file("/dst/a.iso", Some("/src/a.iso")));
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000, blocks: 2 }));
        stat.set("/dst/a.iso", Ok(FileStat { size: 1000, blocks: 2 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();
        assert!(matches!(s.tick(t0), Tick::Sample(_)), "sampling while the file is open");

        proc.set(vec![read_fd(3, "/src/a.iso")]); // cp closed the destination
        assert_eq!(
            s.tick(t0 + Duration::from_millis(100)),
            Tick::Idle,
            "a closed destination is Idle, so the render loop clears the bar"
        );
    }

    #[test]
    fn idle_then_a_new_file_starts_a_fresh_model() {
        // After Idle the sampler holds no per-file state, so the next file is established from
        // scratch (fresh total and basis) rather than continuing the finished one.
        let proc = FakeProc::new(file("/dst/a", Some("/src/a")));
        let stat = FakeStat::default();
        stat.set("/src/a", Ok(FileStat { size: 1000, blocks: 2 }));
        stat.set("/dst/a", Ok(FileStat { size: 400, blocks: 1 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();
        assert_eq!(s.tick(t0).sample().unwrap().total, Some(1000));

        proc.set(vec![]); // between files
        assert_eq!(s.tick(t0 + Duration::from_millis(50)), Tick::Idle);

        proc.set(file("/dst/b", Some("/src/b")));
        stat.set("/src/b", Ok(FileStat { size: 77, blocks: 1 }));
        stat.set("/dst/b", Ok(FileStat { size: 10, blocks: 1 }));
        let st = s.tick(t0 + Duration::from_millis(100)).sample().unwrap();
        assert_eq!(st.total, Some(77));
        assert_eq!(st.done, 10);
    }

    #[test]
    fn proc_error_skips_tick() {
        let proc = FakeProc::new(file("/dst/a.iso", Some("/src/a.iso")));
        let stat = FakeStat::default();
        stat.set("/dst/a.iso", Ok(FileStat { size: 1, blocks: 1 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        proc.fail.set(true);
        assert_eq!(s.tick(Instant::now()), Tick::Skip);
    }

    #[test]
    fn no_current_file_is_idle() {
        // docs/testing.md A8: no growing destination -> nothing to sample, and the bar comes
        // down (Idle) — as opposed to a failed read, which keeps the last value (Skip).
        let proc = FakeProc::new(vec![read_fd(3, "/src/a.iso")]); // read only, no write
        let stat = FakeStat::default();
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        assert_eq!(s.tick(Instant::now()), Tick::Idle);
    }

    #[test]
    fn absent_source_gives_indeterminate_total() {
        // docs/testing.md A10 downstream: source not a regular file -> total unknown.
        let proc = FakeProc::new(file("/dst/a.iso", None));
        let stat = FakeStat::default();
        stat.set("/dst/a.iso", Ok(FileStat { size: 300, blocks: 1 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let st = s.tick(Instant::now()).sample().unwrap();
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
        // The default (size) basis reports the logical length verbatim.
        assert_eq!(st.bytes(Basis::Size), 4096);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn real_sparse_file_is_measured_on_the_size_basis() {
        // #3 against the kernel rather than fixtures: a real file with a hole reports
        // blocks*512 far below st_size, and that must not be read as preallocation.
        use std::io::{Seek, SeekFrom, Write};
        let path = std::env::temp_dir().join(format!("cprog_sparse_{}", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        f.seek(SeekFrom::Start(64 * 1024 * 1024)).unwrap(); // leave a 64 MiB hole
        f.write_all(b"END").unwrap();
        f.sync_all().unwrap();
        drop(f);

        let st = LinuxStatSource.stat(&path).unwrap();
        std::fs::remove_file(&path).ok();
        // Filesystems that do not support holes (or compress inline) would allocate the whole
        // range; there is nothing to assert there.
        if st.blocks.saturating_mul(512) >= st.size {
            return;
        }
        assert_eq!(
            Basis::detect(&st, Some(&st), Some(st.size)),
            Basis::Size,
            "a sparse source/destination must keep the size basis, not be read as preallocated"
        );
        assert_eq!(st.bytes(Basis::Size), st.size, "size basis reports full logical progress");
    }

    #[test]
    fn new_file_resets_total() {
        let proc = FakeProc::new(file("/dst/a", Some("/src/a")));
        let stat = FakeStat::default();
        stat.set("/src/a", Ok(FileStat { size: 1000, blocks: 2 }));
        stat.set("/dst/a", Ok(FileStat { size: 300, blocks: 1 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let a = s.tick(Instant::now()).sample().unwrap();
        assert_eq!(a.total, Some(1000));

        // cp moves on to the next file.
        proc.set(file("/dst/b", Some("/src/b")));
        stat.set("/src/b", Ok(FileStat { size: 2000, blocks: 4 }));
        stat.set("/dst/b", Ok(FileStat { size: 100, blocks: 1 }));
        let b = s.tick(Instant::now() + Duration::from_secs(2)).sample().unwrap();
        assert_eq!(b.name, "b");
        assert_eq!(b.total, Some(2000));
        assert_eq!(b.done, 100);
    }
}
