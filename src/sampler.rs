//! `stat().st_size` polling -> `ProgressState`
//! (docs/progress-model.md).
//!
//! Each tick resolves the file `cp` is currently writing ([`crate::proc::select_current`]),
//! stats its destination for `done` and its source for `total`, and feeds a per-file
//! [`ProgressModel`]. Reads go through the [`crate::proc::ProcSource`] / [`StatSource`] seams
//! so the tick logic is unit-tested from fixtures. A read failure keeps the last value
//! ([`Tick::Skip`], docs/testing.md A9) while having nothing to measure takes the bar down
//! ([`Tick::Idle`]); a `cp` that has moved on to a new file gets a fresh model with a fresh
//! `total`.

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
}

impl FileStat {
    /// Bytes copied so far.
    ///
    /// Always the logical size. `st_blocks * 512` is the alternative, and it is wrong more often
    /// here: a sparse destination (which `cp` produces by default whenever the source has holes),
    /// a compressing filesystem, and ext4's delayed allocation all leave the block count below
    /// the length, which would pin the bar short of 100%. The one case where blocks would win —
    /// a preallocated destination — is unreachable, because GNU `cp` never preallocates
    /// (docs/progress-model.md "언제나 대상의 `st_size`로 잰다").
    pub fn copied_bytes(&self) -> u64 {
        self.size
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
        Ok(FileStat { size: m.size() })
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
    ///
    /// The comparison is exclusive, so on a tie the **first candidate in enumeration order** wins
    /// — including over the file already being tracked, since `self.current` is consulted only
    /// when nothing grew at all (exceptions E24). Equal growth carries no information about which
    /// file cp is writing, and fd order is at best a weak counter-signal: the inherited decoy is
    /// the one that sorts *first*. The tie-break is therefore arbitrary and accepted as such.
    fn choose_dest(&mut self, dests: &[(i32, PathBuf)]) -> Option<(i32, PathBuf)> {
        if let [only] = dests {
            self.candidate_sizes.clear();
            return Some(only.clone());
        }
        // Forget candidates that are no longer open. Without this the history is only ever
        // cleared wholesale — on a single candidate above, or on Idle — and neither is reached
        // while an inherited write fd keeps the count above one and files follow each other
        // closely enough to leave no gap. The map would then hold every destination the copy
        // ever touched (#33, exceptions E23). The live candidate list is a handful of entries,
        // so the scan is cheaper than the growth it prevents.
        self.candidate_sizes.retain(|seen, _| dests.iter().any(|(_, path)| path == seen));

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

    /// Discard the current file's timing history while keeping its identity and `total`.
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
            return Tick::Skip; // docs/testing.md A9: proc read failure -> keep the last value
        };
        let Some(cur) = select_current(&fds) else {
            // docs/testing.md A8: no growing destination — between files, a directory op, or
            // the file just finished and its fd is closed. Nothing is being copied, so the bar
            // comes down.
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
            // The whole path, not just the base name: without `-v` the footer's first row is
            // the only thing naming the file, and it is truncated from the front — which needs a
            // front to drop (docs/ui.md "footer 1행 — 파일 이름").
            let name = dest.to_string_lossy().into_owned();
            self.current =
                Some(CurrentModel { dest, name, model: ProgressModel::new(total, self.window) });
        }

        let cm = self.current.as_mut().expect("current set above");
        let done = match self.stat.stat(&cm.dest) {
            Ok(st) => st.copied_bytes(),
            Err(_) => return Tick::Skip, // docs/testing.md A9: keep the model's last value
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

    // ---- always the size basis (docs/progress-model.md) --------------------------------

    #[test]
    fn copied_bytes_are_the_logical_size() {
        // No basis decision exists any more: `done` is the destination's st_size, full stop.
        assert_eq!(FileStat { size: 1_000_000 }.copied_bytes(), 1_000_000);
        assert_eq!(FileStat { size: 0 }.copied_bytes(), 0);
    }

    #[test]
    fn sparse_destination_progress_reaches_completion() {
        // #3: a 200 MiB mostly-hole file must read 100 % when the copy finishes. `cp` produces a
        // sparse destination by default (--sparse=auto) whenever the source has holes, so this is
        // the ordinary case, not an exotic one.
        const TOTAL: u64 = 209_715_200;
        let proc = FakeProc::new(file("/dst/holey", Some("/src/holey")));
        let stat = FakeStat::default();
        stat.set("/src/holey", Ok(FileStat { size: TOTAL }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();

        stat.set("/dst/holey", Ok(FileStat { size: TOTAL / 2 }));
        let mid = s.tick(t0).sample().unwrap();
        assert_eq!(crate::progress::percent_of(mid.done, mid.total), Some(50.0));

        stat.set("/dst/holey", Ok(FileStat { size: TOTAL }));
        let end = s.tick(t0 + Duration::from_secs(1)).sample().unwrap();
        assert_eq!(end.done, TOTAL);
        assert_eq!(crate::progress::percent_of(end.done, end.total), Some(100.0));
    }

    #[test]
    fn a_destination_smaller_on_disk_than_its_length_still_completes() {
        // The shape shared by sparse files, compressing filesystems and ext4 delayed allocation:
        // what is on disk lags the logical length. Measuring blocks would pin the bar below 100 %
        // in all three; measuring size is right in all three (#12).
        let proc = FakeProc::new(file("/dst/a", Some("/src/a")));
        let stat = FakeStat::default();
        stat.set("/src/a", Ok(FileStat { size: 1_000 }));
        stat.set("/dst/a", Ok(FileStat { size: 1_000 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let st = s.tick(Instant::now()).sample().unwrap();
        assert_eq!(crate::progress::percent_of(st.done, st.total), Some(100.0));
    }

    // ---- basic progress ---------------------------------------------------------------

    #[test]
    fn progress_rises_to_complete() {
        let proc = FakeProc::new(file("/dst/a.iso", Some("/src/a.iso")));
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();

        stat.set("/dst/a.iso", Ok(FileStat { size: 200 }));
        let a = s.tick(t0).sample().unwrap();
        assert_eq!(a.name, "/dst/a.iso", "the whole destination path");
        assert_eq!(a.total, Some(1000));
        assert_eq!(a.done, 200);

        stat.set("/dst/a.iso", Ok(FileStat { size: 1000 }));
        let b = s.tick(t0 + Duration::from_secs(1)).sample().unwrap();
        assert_eq!(b.done, 1000);
        assert_eq!(crate::progress::percent_of(b.done, b.total), Some(100.0));
    }

    #[test]
    fn a_preallocated_destination_reads_complete_immediately() {
        // Documented limitation, asserted so it stays deliberate (#12, docs/progress-model.md).
        // A tool that preallocates its destination gives it the full st_size up front, so the bar
        // jumps straight to 100%. GNU `cp` has no preallocation path, which is why measuring
        // st_size unconditionally is the right trade: the cases that argue for counting blocks
        // (sparse, compressed, delayed allocation) are routine, this one is unreachable.
        let proc = FakeProc::new(file("/dst/a.iso", Some("/src/a.iso")));
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000 }));
        stat.set("/dst/a.iso", Ok(FileStat { size: 1000 })); // preallocated: full length, no data
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let st = s.tick(Instant::now()).sample().unwrap();
        assert_eq!(crate::progress::percent_of(st.done, st.total), Some(100.0));
    }

    #[test]
    fn dest_stat_error_skips_tick_and_keeps_model() {
        let proc = FakeProc::new(file("/dst/a.iso", Some("/src/a.iso")));
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();

        stat.set("/dst/a.iso", Ok(FileStat { size: 500 }));
        assert_eq!(s.tick(t0).sample().unwrap().done, 500);

        stat.set("/dst/a.iso", Err(())); // transient stat failure
        assert_eq!(s.tick(t0 + Duration::from_millis(500)), Tick::Skip, "skip, no crash");

        stat.set("/dst/a.iso", Ok(FileStat { size: 700 }));
        let c = s.tick(t0 + Duration::from_secs(1)).sample().unwrap();
        assert_eq!(c.done, 700, "continues from the same model");
        assert_eq!(c.total, Some(1000), "total not reset");
        // `done` and `total` alone cannot tell a kept model from a rebuilt one — both are re-read
        // from the same two paths on the next tick. The rate can: it needs the sample from
        // *before* the failure, so only a model that survived the skip can state one (#59).
        assert_eq!(c.rate, Some(200.0), "the pre-failure sample is still in the window");
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
        stat.set("/src/a.iso", Ok(FileStat { size: 1000 }));
        stat.set("/tmp/decoy.log", Ok(FileStat { size: 40 })); // never grows
        stat.set("/dst/a.iso", Ok(FileStat { size: 100 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();

        // First tick has nothing to compare against, so it refuses to guess.
        assert_eq!(s.tick(t0), Tick::Skip, "no growth history yet -> no guess");

        // The destination grows; the decoy does not.
        stat.set("/dst/a.iso", Ok(FileStat { size: 400 }));
        let st = s.tick(t0 + Duration::from_millis(100)).sample().unwrap();
        assert_eq!(st.name, "/dst/a.iso", "the growing file is the destination");
        assert_eq!(st.done, 400);
        assert_eq!(st.total, Some(1000));
    }

    #[test]
    fn single_candidate_is_used_without_growth_history() {
        // The common case must not pay the extra tick: one candidate is the destination at once.
        let proc = FakeProc::new(file("/dst/a.iso", Some("/src/a.iso")));
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000 }));
        stat.set("/dst/a.iso", Ok(FileStat { size: 100 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        assert_eq!(s.tick(Instant::now()).sample().unwrap().done, 100);
    }

    #[test]
    fn a_paused_destination_is_not_abandoned_for_another_candidate() {
        // Once tracking a file, a tick where nothing grew must keep it rather than jump to the
        // decoy — copies stall briefly all the time.
        let proc = FakeProc::new(decoy_and_dest());
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000 }));
        stat.set("/tmp/decoy.log", Ok(FileStat { size: 40 }));
        stat.set("/dst/a.iso", Ok(FileStat { size: 100 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();
        assert_eq!(s.tick(t0), Tick::Skip);
        stat.set("/dst/a.iso", Ok(FileStat { size: 400 }));
        assert_eq!(s.tick(t0 + Duration::from_millis(100)).sample().unwrap().name, "/dst/a.iso");

        // Nothing changes this tick: stay on a.iso.
        let st = s.tick(t0 + Duration::from_millis(200)).sample().unwrap();
        assert_eq!(st.name, "/dst/a.iso", "a stalled copy keeps its bar");
        assert_eq!(st.done, 400);
    }

    #[test]
    fn candidate_tracking_does_not_grow_with_the_number_of_files() {
        // #33 / exceptions E23. With a decoy write fd held open the candidate count never drops
        // to one, and back-to-back destinations leave no gap for the Idle path to clear on. The
        // size history must still be bounded by the live candidates, not by the files seen: at
        // ~40 bytes a PathBuf entry, a 20,000-file recursive copy would otherwise keep every
        // destination path it ever touched alive for the whole run.
        let proc = FakeProc::new(vec![]);
        let stat = FakeStat::default();
        stat.set("/tmp/decoy.log", Ok(FileStat { size: 40 })); // inherited, never grows
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();

        const FILES: usize = 200;
        for i in 0..FILES {
            let (src, dst) = (format!("/src/f{i}"), format!("/dst/f{i}"));
            stat.set(&src, Ok(FileStat { size: 1000 }));
            // Two ticks per file so the second one sees growth and can pick a winner.
            for (n, size) in [(0u32, 100), (1, 400)] {
                stat.set(&dst, Ok(FileStat { size }));
                proc.set(vec![
                    write_fd(3, "/tmp/decoy.log"),
                    read_fd(4, &src),
                    write_fd(5, &dst),
                ]);
                s.tick(t0 + Duration::from_millis((i as u64 * 2 + n as u64) * 50));
            }
        }

        assert!(
            s.candidate_sizes.len() <= 4,
            "size history must stay bounded by the live candidates (2 here), got {} after \
             {FILES} files",
            s.candidate_sizes.len()
        );
    }

    #[test]
    fn the_biggest_gainer_wins_even_when_it_is_the_inherited_fd() {
        // The mechanism is symmetric, and that is the point: the rule is "the growing file", not
        // "the file we picked first". If the inherited fd is the one actually being written to,
        // it wins — which is right, because then it is not a decoy at all.
        //
        // The old name promised the opposite of the assertion below, and spoke of a "tracked"
        // destination when nothing was tracked yet: tick one returns `Skip`, which returns before
        // `self.current` is ever set. The tracked case is
        // `a_tie_overrides_the_file_already_being_tracked` (#61).
        let proc = FakeProc::new(decoy_and_dest());
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 1000 }));
        stat.set("/tmp/decoy.log", Ok(FileStat { size: 40 }));
        stat.set("/dst/a.iso", Ok(FileStat { size: 100 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();
        assert_eq!(s.tick(t0), Tick::Skip);

        stat.set("/dst/a.iso", Ok(FileStat { size: 110 })); // +10
        stat.set("/tmp/decoy.log", Ok(FileStat { size: 9000 })); // +8960
        let st = s.tick(t0 + Duration::from_millis(100)).sample().unwrap();
        assert_eq!(st.name, "/tmp/decoy.log", "the biggest gainer is what is being written");

        // The decoy is also the *first* candidate, so the assertion above is equally satisfied by
        // "the first file that grew at all" — the rule and a much weaker one agree here. Reversing
        // the amounts separates them: now the bigger gainer is the second candidate, and only a
        // comparison on size picks it (#61).
        stat.set("/tmp/decoy.log", Ok(FileStat { size: 9010 })); // +10
        stat.set("/dst/a.iso", Ok(FileStat { size: 9000 })); // +8890
        let st = s.tick(t0 + Duration::from_millis(200)).sample().unwrap();
        assert_eq!(st.name, "/dst/a.iso", "size decides, not position");
    }

    #[test]
    fn equal_growth_keeps_the_candidate_seen_first() {
        // exceptions E24. Two candidates that grew by exactly the same amount say nothing about
        // which one `cp` is writing, so the exclusive comparison hands the tick to whichever came
        // first in enumeration order. Nothing makes that the right answer — it is an arbitrary
        // but stated tie-break, and the point of pinning it is that the rule is written down
        // rather than that it is optimal.
        let proc = FakeProc::new(vec![write_fd(4, "/dst/first"), write_fd(5, "/dst/second")]);
        let stat = FakeStat::default();
        stat.set("/dst/first", Ok(FileStat { size: 100 }));
        stat.set("/dst/second", Ok(FileStat { size: 100 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();
        assert_eq!(s.tick(t0), Tick::Skip, "no growth history yet -> no guess");

        for n in 1..=3u64 {
            let size = 100 + n * 500; // both grow by the same amount, every tick
            stat.set("/dst/first", Ok(FileStat { size }));
            stat.set("/dst/second", Ok(FileStat { size }));
            let st = s.tick(t0 + Duration::from_millis(n * 100)).sample().unwrap();
            assert_eq!(st.name, "/dst/first", "tick {n}: the earlier candidate takes the tie");
        }
    }

    #[test]
    fn a_tie_overrides_the_file_already_being_tracked() {
        // The uncomfortable half of E24, recorded because it is what the code does rather than
        // what one would assume from "keeps the candidate seen first": `choose_dest` looks at
        // `self.current` only when *nothing* grew, so a tie re-elects the first candidate even
        // when the bar was following the second. Documenting the tie-break as "sticky" would
        // have been wrong, and only a test that starts out tracking the other file can say so.
        let proc = FakeProc::new(vec![write_fd(4, "/dst/first"), write_fd(5, "/dst/second")]);
        let stat = FakeStat::default();
        stat.set("/dst/first", Ok(FileStat { size: 100 }));
        stat.set("/dst/second", Ok(FileStat { size: 100 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();
        assert_eq!(s.tick(t0), Tick::Skip);

        // second out-grows first, so the bar follows second
        stat.set("/dst/first", Ok(FileStat { size: 110 })); // +10
        stat.set("/dst/second", Ok(FileStat { size: 900 })); // +800
        let tracked = s.tick(t0 + Duration::from_millis(100)).sample().unwrap();
        assert_eq!(tracked.name, "/dst/second", "the bigger gainer takes the bar");

        // now both grow by 500: the tie hands it back to the first candidate
        stat.set("/dst/first", Ok(FileStat { size: 610 }));
        stat.set("/dst/second", Ok(FileStat { size: 1400 }));
        let after = s.tick(t0 + Duration::from_millis(200)).sample().unwrap();
        assert_eq!(after.name, "/dst/first", "a tie moves the bar, it does not hold it");
    }

    #[test]
    fn reset_rate_history_clears_the_current_files_history() {
        // A13 / #9 at the sampler level. `ProgressModel::reset_samples` is what drops the samples
        // and is tested there, but the Ctrl-Z path reaches it through this wrapper, and the
        // suspend tests assert on the footer rather than on the rate. Emptying this function's
        // body therefore changed nothing any test could see.
        let proc = FakeProc::new(file("/dst/a.iso", Some("/src/a.iso")));
        let stat = FakeStat::default();
        stat.set("/src/a.iso", Ok(FileStat { size: 10_000 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();

        stat.set("/dst/a.iso", Ok(FileStat { size: 100 }));
        s.tick(t0);
        stat.set("/dst/a.iso", Ok(FileStat { size: 200 }));
        let before = s.tick(t0 + Duration::from_secs(1)).sample().unwrap();
        assert!(before.rate.is_some(), "two samples inside the window give a rate");

        s.reset_rate_history();

        // Resumed a minute later, having copied nothing while stopped. Without the reset those
        // two facts average into a rate of ~0 for a full window; with it, one post-resume sample
        // is simply not enough to state a rate at all.
        stat.set("/dst/a.iso", Ok(FileStat { size: 200 }));
        let after = s.tick(t0 + Duration::from_secs(61)).sample().unwrap();
        assert_eq!(after.rate, None, "the stopped span must not be averaged in");
        assert_eq!(after.name, "/dst/a.iso", "identity survives: the same file is still copying");
        assert_eq!(after.total, Some(10_000), "and so does its total — that is not timing data");
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
        stat.set("/etc/passwd", Ok(FileStat { size: 2_000 }));
        stat.set("/src/big.iso", Ok(FileStat { size: 1_000_000 }));
        stat.set("/dst/big.iso", Ok(FileStat { size: 250_000 }));
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
        stat.set("/tmp/decoy.log", Ok(FileStat { size: 10 })); // never grows
        stat.set("/etc/passwd", Ok(FileStat { size: 2_000 }));
        stat.set("/src/a.iso", Ok(FileStat { size: 800 }));
        stat.set("/dst/a.iso", Ok(FileStat { size: 100 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();
        assert_eq!(s.tick(t0), Tick::Skip, "two write candidates, no growth history yet");

        stat.set("/dst/a.iso", Ok(FileStat { size: 400 }));
        let st = s.tick(t0 + Duration::from_millis(100)).sample().unwrap();
        assert_eq!(st.name, "/dst/a.iso", "destination by growth (#6)");
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
        stat.set("/src/a.iso", Ok(FileStat { size: 1000 }));
        stat.set("/dst/a.iso", Ok(FileStat { size: 1000 }));
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
        // scratch (a fresh total) rather than continuing the finished one.
        let proc = FakeProc::new(file("/dst/a", Some("/src/a")));
        let stat = FakeStat::default();
        stat.set("/src/a", Ok(FileStat { size: 1000 }));
        stat.set("/dst/a", Ok(FileStat { size: 400 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let t0 = Instant::now();
        assert_eq!(s.tick(t0).sample().unwrap().total, Some(1000));

        proc.set(vec![]); // between files
        assert_eq!(s.tick(t0 + Duration::from_millis(50)), Tick::Idle);

        proc.set(file("/dst/b", Some("/src/b")));
        stat.set("/src/b", Ok(FileStat { size: 77 }));
        stat.set("/dst/b", Ok(FileStat { size: 10 }));
        let st = s.tick(t0 + Duration::from_millis(100)).sample().unwrap();
        assert_eq!(st.total, Some(77));
        assert_eq!(st.done, 10);

        // A *different* path would be established from scratch anyway — `is_new` compares paths,
        // so the assertions above cannot see whether Idle cleared anything. The same destination
        // coming back is what needs the clear: a model that survived would still hold the
        // pre-Idle sample and could state a rate, and a fresh one cannot (#61).
        proc.set(vec![]);
        assert_eq!(s.tick(t0 + Duration::from_millis(150)), Tick::Idle);
        proc.set(file("/dst/b", Some("/src/b")));
        stat.set("/dst/b", Ok(FileStat { size: 20 }));
        let again = s.tick(t0 + Duration::from_millis(200)).sample().unwrap();
        assert_eq!(again.rate, None, "the same file after Idle starts a fresh model");
    }

    #[test]
    fn proc_error_skips_tick() {
        let proc = FakeProc::new(file("/dst/a.iso", Some("/src/a.iso")));
        let stat = FakeStat::default();
        stat.set("/dst/a.iso", Ok(FileStat { size: 1 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        proc.fail.set(true);
        assert_eq!(s.tick(Instant::now()), Tick::Skip);
    }

    #[test]
    fn no_current_file_is_idle() {
        // docs/testing.md A8: **no write candidate at all** -> nothing to sample, and the bar
        // comes down (Idle). "No *growing* destination" is the other verdict — a candidate that
        // merely did not grow is `Skip`, pinned by `growing_candidate_wins_over_an_inherited_write_fd`
        // — and this file exists to keep the two apart, so the wording matters (#61).
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
        stat.set("/dst/a.iso", Ok(FileStat { size: 300 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let st = s.tick(Instant::now()).sample().unwrap();
        assert_eq!(st.total, None);
        assert_eq!(crate::progress::percent_of(st.done, st.total), None);
    }

    #[test]
    fn linux_stat_source_reads_the_real_size() {
        let path = std::env::temp_dir().join(format!("cprog_stat_{}", std::process::id()));
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let st = LinuxStatSource.stat(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(st.size, 4096);
        assert_eq!(st.copied_bytes(), 4096);
    }

    #[test]
    fn a_real_sparse_file_reports_its_full_logical_size() {
        // #3/#12 against the kernel rather than fixtures. A file with a hole has far fewer blocks
        // than its length — the very shape that made the old block-based measurement stall short
        // of 100%. Reading st_size gives the logical progress the bar is meant to show.
        use std::io::{Seek, SeekFrom, Write};
        use std::os::unix::fs::MetadataExt;
        const LEN: u64 = 64 * 1024 * 1024;
        let path = std::env::temp_dir().join(format!("cprog_sparse_{}", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        f.seek(SeekFrom::Start(LEN)).unwrap(); // leave a 64 MiB hole
        f.write_all(b"END").unwrap();
        f.sync_all().unwrap();
        drop(f);

        let blocks = std::fs::metadata(&path).unwrap().blocks();
        let st = LinuxStatSource.stat(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(st.copied_bytes(), LEN + 3, "the full logical length, holes included");
        // The claim worth pinning is about the *kernel*, not about `copied_bytes`: a hole really
        // does leave the block count far short of the length, which is what would have stalled a
        // block-based bar. Comparing blocks against `copied_bytes()` restates the guard above it
        // — `copied_bytes` returns `size` — so it could never fail (#61).
        //
        // Filesystems without hole support allocate the whole range; there the premise is absent,
        // and saying so is better than a comparison that quietly passes either way.
        let on_disk = blocks.saturating_mul(512);
        if on_disk >= LEN {
            eprintln!("note: {path:?} has no hole support ({on_disk} bytes allocated); the \
                       block-vs-length half of this test did not apply");
        } else {
            assert!(
                on_disk < LEN / 2,
                "a 64 MiB hole must leave the block count far short of the length, got {on_disk}"
            );
        }
    }

    #[test]
    fn new_file_resets_total() {
        let proc = FakeProc::new(file("/dst/a", Some("/src/a")));
        let stat = FakeStat::default();
        stat.set("/src/a", Ok(FileStat { size: 1000 }));
        stat.set("/dst/a", Ok(FileStat { size: 300 }));
        let mut s = Sampler::new(&proc, &stat, 42, WINDOW);
        let a = s.tick(Instant::now()).sample().unwrap();
        assert_eq!(a.total, Some(1000));

        // cp moves on to the next file.
        proc.set(file("/dst/b", Some("/src/b")));
        stat.set("/src/b", Ok(FileStat { size: 2000 }));
        stat.set("/dst/b", Ok(FileStat { size: 100 }));
        let b = s.tick(Instant::now() + Duration::from_secs(2)).sample().unwrap();
        assert_eq!(b.name, "/dst/b");
        assert_eq!(b.total, Some(2000));
        assert_eq!(b.done, 100);
    }
}
