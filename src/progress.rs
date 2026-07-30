//! Per-file progress model (docs/progress-model.md).
//!
//! Consumes `(t, done)` samples of the destination size and derives percent / rate / eta
//! for the **current file only** (never a whole-job figure). rate/eta are smoothed over a
//! short rolling window (~1s) so bursty I/O does not make the numbers jump.
//!
//! All values are `Option`: unknown-until-enough-samples and unknown-size stay explicit
//! rather than showing a fake 100% or a wild rate (docs/ui.md invariant 5).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Default rolling-window length for rate/eta smoothing (docs/progress-model.md "~1s").
pub const DEFAULT_RATE_WINDOW: Duration = Duration::from_secs(1);

/// Current-file percentage in `0..=100` from `done`/`total`, or `None` when the total is
/// unknown (indeterminate). Empty source (total 0) reads as complete; overshoot clamps to
/// 100. The footer renderer calls this, and so does `ProgressModel::percent` (test-only), so
/// the A5/A6 rules (docs/testing.md) live in exactly one place.
pub fn percent_of(done: u64, total: Option<u64>) -> Option<f64> {
    match total {
        None => None,
        Some(0) => Some(100.0),                    // docs/testing.md A5: complete, and no divide-by-zero
        Some(total) => Some((done as f64 / total as f64 * 100.0).clamp(0.0, 100.0)), // docs/testing.md A6 clamp
    }
}

/// Snapshot of the current file's progress, consumed by the footer renderer
/// (docs/architecture.md "핵심 타입"). Carries raw `done`/`total`; percent is derived with
/// [`percent_of`] so overshoot/empty-source handling stays in one place.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressState {
    /// The destination path of the file being copied, shown on the footer's first row when the
    /// user did not pass `-v` (docs/ui.md). The whole path, because that row is then the only
    /// place naming the file and it is fitted by dropping its front.
    pub name: String,
    /// Source size in bytes; `None` when unknown -> indeterminate bar.
    pub total: Option<u64>,
    /// Bytes landed so far (destination size), raw and unclamped.
    pub done: u64,
    /// Smoothed throughput in bytes/sec; `None` until enough samples.
    pub rate: Option<f64>,
    /// Estimated time remaining for this file; `None` when unknown.
    pub eta: Option<Duration>,
}

/// Rolling-window model over `(t, done)` samples of the destination size.
#[derive(Debug)]
pub struct ProgressModel {
    total: Option<u64>,
    window: Duration,
    /// `(time, done)` samples, oldest first. Bounded to roughly one window (plus one anchor).
    samples: VecDeque<(Instant, u64)>,
}

impl ProgressModel {
    /// Create a model for a file of size `total` (None if unknown), smoothing over `window`.
    pub fn new(total: Option<u64>, window: Duration) -> Self {
        Self { total, window, samples: VecDeque::new() }
    }

    /// Record a destination-size sample. Evicts samples that have aged out of the window,
    /// keeping at most one anchor just outside it so the window stays fully covered.
    pub fn push(&mut self, now: Instant, done: u64) {
        self.samples.push_back((now, done));
        // Drop a front sample only while the next one is already at/older than the window,
        // i.e. the front is redundant for covering `[now - window, now]`.
        while self.samples.len() >= 2 {
            let second = self.samples[1].0;
            if now.saturating_duration_since(second) >= self.window {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Drop every recorded sample, keeping `total` and the window.
    ///
    /// Used when wall-clock time passed without the copy being able to run — a job-control stop
    /// (`Ctrl-Z`) stops `cp` too, so the elapsed span carries no throughput information. Leaving
    /// those samples in place would make the window read as "no progress for N seconds" and
    /// report a near-zero rate (or an absurd eta) for a full window after resuming (#9).
    pub fn reset_samples(&mut self) {
        self.samples.clear();
    }

    /// Bytes landed so far (last sample), or 0 if no sample yet.
    pub fn done(&self) -> u64 {
        self.samples.back().map(|&(_, d)| d).unwrap_or(0)
    }

    /// Current-file percentage in `0..=100`, or `None` when the total is unknown
    /// (indeterminate). Empty source (total 0) reads as complete; overshoot clamps to 100.
    ///
    /// Test-only: the renderer calls [`percent_of`] on a snapshot rather than reaching into the
    /// model, so the rule lives in exactly one place.
    #[cfg(test)]
    pub fn percent(&self) -> Option<f64> {
        percent_of(self.done(), self.total)
    }

    /// Smoothed throughput (bytes/sec) across the window, or `None` with fewer than two
    /// samples or a zero time span. A flat or negative delta yields exactly `0.0` (docs/testing.md A7).
    pub fn rate(&self) -> Option<f64> {
        if self.samples.len() < 2 {
            return None;
        }
        let (t0, d0) = *self.samples.front().unwrap();
        let (t1, d1) = *self.samples.back().unwrap();
        let span = t1.saturating_duration_since(t0).as_secs_f64();
        if span <= 0.0 {
            return None;
        }
        let delta = d1.saturating_sub(d0) as f64; // negative deltas floor at 0 -> rate 0
        Some(delta / span)
    }

    /// Estimated time remaining for the current file, or `None` when unknown. Returns
    /// `Duration::ZERO` once done reaches total; needs a known total and a positive rate.
    pub fn eta(&self) -> Option<Duration> {
        let total = self.total?;
        let remaining = total.saturating_sub(self.done());
        if remaining == 0 {
            return Some(Duration::ZERO);
        }
        let rate = self.rate()?;
        if rate <= 0.0 {
            return None; // docs/testing.md A7: no forward progress -> eta unknown
        }
        // try_from_secs_f64 rejects non-finite/overflowing durations instead of panicking.
        Duration::try_from_secs_f64(remaining as f64 / rate).ok()
    }

    /// Build a [`ProgressState`] snapshot for the current file named `name`.
    pub fn snapshot(&self, name: impl Into<String>) -> ProgressState {
        ProgressState {
            name: name.into(),
            total: self.total,
            done: self.done(),
            rate: self.rate(),
            eta: self.eta(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const WINDOW: Duration = Duration::from_secs(1);

    fn model(total: Option<u64>) -> ProgressModel {
        ProgressModel::new(total, WINDOW)
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    // ---- percent (docs/testing.md A5 / A6, indeterminate) --------------------------------------------

    #[test]
    fn percent_basic_ratio() {
        let mut m = model(Some(200));
        m.push(Instant::now(), 50);
        assert!(approx(m.percent().unwrap(), 25.0));
    }

    #[test]
    fn percent_empty_source_is_complete_not_divide_by_zero() {
        // docs/testing.md A5: total 0 (empty source) must not divide by zero.
        let mut m = model(Some(0));
        m.push(Instant::now(), 0);
        assert!(approx(m.percent().unwrap(), 100.0));
    }

    #[test]
    fn percent_overshoot_clamps_to_100() {
        // docs/testing.md A6: done > total clamps to 100.
        let mut m = model(Some(100));
        m.push(Instant::now(), 150);
        assert!(approx(m.percent().unwrap(), 100.0));
    }

    #[test]
    fn percent_unknown_total_is_indeterminate() {
        let mut m = model(None);
        m.push(Instant::now(), 50);
        assert_eq!(m.percent(), None);
    }

    // ---- rate ------------------------------------------------------------------------

    #[test]
    fn rate_unknown_before_two_samples() {
        let mut m = model(Some(1000));
        assert_eq!(m.rate(), None);
        m.push(Instant::now(), 100);
        assert_eq!(m.rate(), None, "one sample is not enough");
    }

    #[test]
    fn rate_over_window() {
        let mut m = model(Some(10_000));
        let t0 = Instant::now();
        m.push(t0, 0);
        m.push(t0 + Duration::from_secs(1), 100);
        assert!(approx(m.rate().unwrap(), 100.0), "100 bytes in 1s = 100 B/s");
    }

    #[test]
    fn rate_is_per_second_not_per_window() {
        // The rate is a *quotient*: bytes divided by the span they took. A one-second span cannot
        // show that — dividing and multiplying by it give the same number — and when this test was
        // written every other non-zero rate assertion used exactly that span, so nothing said which
        // operation it is. That is no longer the only witness:
        // `without_reset_a_long_stop_drags_the_rate_to_zero` gained a resume (#61), which put a
        // 300-byte delta across a 60.1 s span, so turning `delta / span` into `delta * span` now
        // fails both tests rather than this one alone (measured, #69 D). Two spans is not redundancy here — this one is the arithmetic
        // stated directly, that one is the same rule reached through a job-control stop.
        let mut m = ProgressModel::new(Some(10_000), Duration::from_secs(4));
        let t0 = Instant::now();
        m.push(t0, 0);
        m.push(t0 + Duration::from_secs(2), 300);
        assert!(approx(m.rate().unwrap(), 150.0), "300 bytes over 2 s = 150 B/s");

        // And on the other side of 1 s, where the two also disagree.
        let mut half = ProgressModel::new(Some(10_000), Duration::from_secs(4));
        half.push(t0, 0);
        half.push(t0 + Duration::from_millis(500), 100);
        assert!(approx(half.rate().unwrap(), 200.0), "100 bytes over 0.5 s = 200 B/s");
    }

    #[test]
    fn rate_needs_a_positive_span_not_only_two_samples() {
        // `rate`'s contract has two ways to be unknown — too few samples, and a zero span — and
        // only the first was pinned. Without the span guard this divides by zero and hands `inf`
        // to the footer's rate formatter (#59).
        //
        // The sampler cannot produce this: it pushes at tick instants, which differ. So the test
        // states the model's own contract rather than a reachable production state — the model is
        // a pure unit with a documented rule, and that rule was unchecked.
        let mut m = model(Some(1000));
        let t0 = Instant::now();
        m.push(t0, 100);
        m.push(t0, 200); // a real delta, no elapsed time
        assert_eq!(m.rate(), None, "a zero span is not a rate");
        assert_eq!(m.eta(), None, "and no eta can be derived from one");
    }

    #[test]
    fn rate_zero_when_no_increase() {
        // docs/testing.md A7: no increase between samples -> rate ~= 0.
        let mut m = model(Some(1000));
        let t0 = Instant::now();
        m.push(t0, 200);
        m.push(t0 + Duration::from_secs(1), 200);
        assert!(approx(m.rate().unwrap(), 0.0));
        assert_eq!(m.eta(), None, "no progress -> eta unknown");
    }

    #[test]
    fn rate_zero_when_negative_increase() {
        // docs/testing.md A7: a negative delta is floored at 0, never a negative rate.
        let mut m = model(Some(1000));
        let t0 = Instant::now();
        m.push(t0, 200);
        m.push(t0 + Duration::from_secs(1), 150);
        assert!(approx(m.rate().unwrap(), 0.0));
    }

    #[test]
    fn rate_reflects_recent_window_after_eviction() {
        let mut m = model(Some(10_000));
        let t0 = Instant::now();
        m.push(t0, 0);
        m.push(t0 + Duration::from_millis(500), 50);
        m.push(t0 + Duration::from_secs(1), 100);
        m.push(t0 + Duration::from_secs(2), 300); // old slow samples fall out of the window
        assert!(approx(m.rate().unwrap(), 200.0), "recent 1s: 100->300 = 200 B/s");
    }

    // ---- eta -------------------------------------------------------------------------

    #[test]
    fn eta_from_remaining_over_rate() {
        let mut m = model(Some(1000));
        let t0 = Instant::now();
        m.push(t0, 100);
        m.push(t0 + Duration::from_secs(1), 200); // rate 100 B/s, done 200, remaining 800
        assert_eq!(m.eta(), Some(Duration::from_secs(8)));
    }

    #[test]
    fn eta_unknown_when_rate_unknown() {
        let mut m = model(Some(1000));
        m.push(Instant::now(), 100); // only one sample -> rate None
        assert_eq!(m.eta(), None);
    }

    #[test]
    fn eta_unknown_when_total_unknown() {
        let mut m = model(None);
        let t0 = Instant::now();
        m.push(t0, 100);
        m.push(t0 + Duration::from_secs(1), 200);
        assert_eq!(m.eta(), None);
    }

    #[test]
    fn eta_zero_when_complete() {
        let mut m = model(Some(200));
        let t0 = Instant::now();
        m.push(t0, 100);
        m.push(t0 + Duration::from_secs(1), 200); // done == total
        assert_eq!(m.eta(), Some(Duration::ZERO));

        // With a rate in hand, `remaining / rate` is 0 anyway — so that pair says nothing about
        // the branch above it. A finished file with *no* usable rate is what needs it: one
        // sample, or a stalled one, must still read `0:00` rather than `--:--` at 100 % (#59).
        let mut one = model(Some(200));
        one.push(t0, 200);
        assert_eq!(one.rate(), None, "one sample gives no rate");
        assert_eq!(one.eta(), Some(Duration::ZERO), "complete is complete without a rate");

        let mut stalled = model(Some(200));
        stalled.push(t0, 200);
        stalled.push(t0 + Duration::from_secs(1), 200); // complete, and not moving
        assert!(approx(stalled.rate().unwrap(), 0.0));
        assert_eq!(stalled.eta(), Some(Duration::ZERO), "a zero rate at 100 % is still done");
    }

    // ---- snapshot DTO ----------------------------------------------------------------

    #[test]
    fn reset_samples_clears_the_rate_history_but_keeps_total() {
        // #9: after a Ctrl-Z the elapsed span carries no throughput information, so the samples
        // spanning the stop are dropped. total (the file's size) is not timing data and stays.
        let mut m = model(Some(1000));
        let t0 = Instant::now();
        m.push(t0, 100);
        m.push(t0 + Duration::from_secs(1), 200);
        assert!(m.rate().is_some());

        m.reset_samples();
        assert_eq!(m.rate(), None, "no rate until fresh samples arrive");
        assert_eq!(m.eta(), None);
        assert_eq!(m.percent(), Some(0.0), "total kept; done is unknown until the next sample");

        // Two post-resume samples rebuild the rate from real throughput only.
        let t1 = t0 + Duration::from_secs(60); // stopped for a minute
        m.push(t1, 200);
        m.push(t1 + Duration::from_secs(1), 500);
        assert!(approx(m.rate().unwrap(), 300.0), "the stopped minute is not averaged in");
    }

    #[test]
    fn without_reset_a_long_stop_drags_the_rate_to_zero() {
        // The behaviour #9 fixes: the window is wall-clock based, so a suspend looks like "no
        // progress for a minute" and the rate collapses for a full window **after resuming**.
        //
        // The resume is the part that matters and the old fixture did not have one — it stopped
        // at two flat samples, which is the plain zero-delta path (A7) that `rate_zero_when_no_increase`
        // already covers, and where the sixty-second span cannot change a zero quotient anyway (#61).
        let mut m = model(Some(1000));
        let t0 = Instant::now();
        m.push(t0, 200);
        m.push(t0 + Duration::from_secs(60), 200); // stopped the whole time
        assert!(approx(m.rate().unwrap(), 0.0), "no progress across the stop");

        // Now copying again, briskly: 300 bytes in the 100 ms since resuming = 3000 B/s. With the
        // stop still in the window the average is 300 bytes over the whole 60.1 s span = 4.99 B/s,
        // a *six-hundredth* of it — the assertion below caps the figure at 10 B/s, which a
        // hundredth (30 B/s) would not even satisfy (#69 D). The user sees a rate
        // and an eta that describe a minute of standing still rather than the copy in front of
        // them. `reset_samples` is what removes it.
        let resumed = t0 + Duration::from_millis(60_100);
        m.push(resumed, 500);
        let dragged = m.rate().unwrap();
        assert!(dragged < 10.0, "the stop is still averaged in: {dragged} B/s");

        let mut fresh = model(Some(1000));
        fresh.push(t0 + Duration::from_secs(60), 200);
        fresh.push(resumed, 500);
        assert!(approx(fresh.rate().unwrap(), 3000.0), "what the copy is actually doing");
    }

    #[test]
    fn snapshot_builds_progress_state() {
        let mut m = model(Some(1000));
        let t0 = Instant::now();
        m.push(t0, 100);
        m.push(t0 + Duration::from_secs(1), 200);
        let s = m.snapshot("a.iso");
        assert_eq!(s.name, "a.iso");
        assert_eq!(s.total, Some(1000));
        assert_eq!(s.done, 200);
        assert!(approx(s.rate.unwrap(), 100.0));
        assert_eq!(s.eta, Some(Duration::from_secs(8)));
    }

    #[test]
    fn default_rate_window_is_one_second() {
        assert_eq!(DEFAULT_RATE_WINDOW, Duration::from_secs(1));
    }
}
