//! Slow-file decision (docs/capture-and-verbose.md "느린 파일 타이밍").
//!
//! Timing rule, driven purely by line-boundary pulses ([`crate::verbose`]):
//! ```text
//! new -v line (pulse)      -> record "current item start" time (ends the previous item)
//! next pulse / cp exit      -> previous item ends
//! elapsed > SLOW_THRESHOLD  -> current item is a "slow file" -> turn the footer bar on
//! elapsed <= SLOW_THRESHOLD -> a fast file/dir (next line comes soon) -> no bar
//! ```
//! The `>` boundary is exclusive: an item taking exactly the threshold is *not yet* slow
//! (docs/capture-and-verbose.md states `경과 > SLOW_THRESHOLD`).
//!
//! `SlowTimer` is pure over explicit `Instant`s: callers pass `Instant::now()` in production
//! and virtual instants in tests (docs/testing.md `Clock` seam — realized as explicit time
//! parameters rather than a trait), so timing logic stays deterministic without any I/O.

use std::time::{Duration, Instant};

/// Default slow-file threshold (docs/usage.md `CPROG_SLOW_THRESHOLD_MS`, default 100).
pub const DEFAULT_SLOW_THRESHOLD: Duration = Duration::from_millis(100);

/// Decides whether the file `cp` is currently copying has been running long enough to be
/// "slow" and warrant a footer bar. Fed line-boundary pulses; queried at render ticks.
#[derive(Debug)]
pub struct SlowTimer {
    threshold: Duration,
    /// When the current item's pulse arrived; `None` before the first pulse.
    current_start: Option<Instant>,
}

impl SlowTimer {
    /// Create a timer with the given slow threshold.
    pub fn new(threshold: Duration) -> Self {
        Self { threshold, current_start: None }
    }

    /// Record a "new item" pulse at `now`. This ends the previous item and restarts the
    /// slow timer for the newly started item (docs/testing.md B7 boundary switch).
    pub fn on_pulse(&mut self, now: Instant) {
        self.current_start = Some(now);
    }

    /// Whether the current item has been running longer than the threshold as of `now`.
    /// Returns false before any pulse. The `>` comparison keeps the boundary exclusive.
    pub fn is_slow(&self, now: Instant) -> bool {
        match self.current_start {
            Some(start) => now.saturating_duration_since(start) > self.threshold,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: Duration = Duration::from_millis(100);

    fn timer() -> SlowTimer {
        SlowTimer::new(THRESHOLD)
    }

    #[test]
    fn before_any_pulse_is_not_slow() {
        let t = timer();
        let now = Instant::now();
        assert!(!t.is_slow(now));
        assert!(!t.is_slow(now + Duration::from_secs(10)));
    }

    #[test]
    fn fast_file_below_threshold_is_not_slow() {
        // docs/testing.md B5: a very fast file (< threshold) never shows a bar.
        let mut t = timer();
        let t0 = Instant::now();
        t.on_pulse(t0);
        assert!(!t.is_slow(t0 + Duration::from_millis(50)));
    }

    #[test]
    fn slow_file_above_threshold_is_slow() {
        let mut t = timer();
        let t0 = Instant::now();
        t.on_pulse(t0);
        assert!(t.is_slow(t0 + Duration::from_millis(150)));
    }

    #[test]
    fn threshold_boundary_is_exclusive() {
        // docs/testing.md B6: just-before / exact / just-after the threshold.
        let mut t = timer();
        let t0 = Instant::now();
        t.on_pulse(t0);
        assert!(!t.is_slow(t0 + Duration::from_millis(99)), "just before -> no bar");
        assert!(!t.is_slow(t0 + THRESHOLD), "exactly threshold -> no bar (`>` is exclusive)");
        assert!(t.is_slow(t0 + THRESHOLD + Duration::from_nanos(1)), "just after -> bar");
    }

    #[test]
    fn new_pulse_resets_timer_at_file_boundary() {
        // docs/testing.md B7: two consecutive slow files -> bar switches at the boundary.
        let mut t = timer();
        let t0 = Instant::now();
        t.on_pulse(t0); // file A starts
        assert!(t.is_slow(t0 + Duration::from_millis(150)), "A became slow");

        let t1 = t0 + Duration::from_millis(200);
        t.on_pulse(t1); // file B starts -> previous item ends, timer resets
        assert!(!t.is_slow(t1 + Duration::from_millis(50)), "B not slow yet (bar switched off)");
        assert!(t.is_slow(t1 + Duration::from_millis(150)), "B became slow (bar switched on)");
    }

    #[test]
    fn default_threshold_is_100ms() {
        assert_eq!(DEFAULT_SLOW_THRESHOLD, Duration::from_millis(100));
    }
}
