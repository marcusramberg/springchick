//! Pure frame-timing statistics for perf validation (M4).
//!
//! Records per-frame wall-clock durations in a ring buffer and computes
//! fps / p50 / p99 / dropped-frame counts. Backend-agnostic so winit and DRM
//! numbers are directly comparable.

use std::collections::VecDeque;
use std::time::Duration;

/// A computed snapshot of recent frame timings.
#[derive(Clone, Copy, Debug)]
pub struct StatsSnapshot {
    pub fps: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub dropped: usize,
    pub samples: usize,
}

/// Ring buffer of recent frame durations.
pub struct FrameStats {
    budget: Duration,
    cap: usize,
    samples: VecDeque<Duration>,
}

impl FrameStats {
    /// Default capacity ~ a couple seconds at 90 Hz.
    pub fn new(budget: Duration) -> Self {
        Self::with_capacity(budget, 256)
    }

    pub fn with_capacity(budget: Duration, cap: usize) -> Self {
        Self {
            budget,
            cap,
            samples: VecDeque::with_capacity(cap),
        }
    }

    /// Record one frame's wall-clock duration.
    pub fn record_frame(&mut self, dt: Duration) {
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(dt);
    }

    /// Number of recorded samples.
    #[allow(dead_code)] // diagnostic helper; wired by later backends
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Compute a snapshot. Returns zeros when empty.
    pub fn snapshot(&self) -> StatsSnapshot {
        if self.samples.is_empty() {
            return StatsSnapshot {
                fps: 0.0,
                p50_ms: 0.0,
                p99_ms: 0.0,
                dropped: 0,
                samples: 0,
            };
        }
        let mut sorted: Vec<f64> = self
            .samples
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| -> f64 {
            let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
            sorted[idx.min(sorted.len() - 1)]
        };
        let mean: f64 = sorted.iter().sum::<f64>() / sorted.len() as f64;
        let budget_ms = self.budget.as_secs_f64() * 1000.0;
        let dropped = sorted.iter().filter(|&&v| v > budget_ms).count();
        StatsSnapshot {
            fps: if mean > 0.0 { 1000.0 / mean } else { 0.0 },
            p50_ms: pct(50.0),
            p99_ms: pct(99.0),
            dropped,
            samples: sorted.len(),
        }
    }

    /// One-line summary for logging.
    pub fn format_line(&self) -> String {
        let s = self.snapshot();
        format!(
            "fps={:.0} p50={:.1}ms p99={:.1}ms dropped={} n={}",
            s.fps, s.p50_ms, s.p99_ms, s.dropped, s.samples
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ms(n: f64) -> Duration {
        Duration::from_secs_f64(n / 1000.0)
    }

    #[test]
    fn percentiles_from_known_set() {
        let mut s = FrameStats::new(Duration::from_micros(11_111)); // 90 Hz budget
        for v in [10.0, 10.0, 10.0, 10.0, 30.0] {
            s.record_frame(ms(v));
        }
        let snap = s.snapshot();
        assert!((snap.p50_ms - 10.0).abs() < 0.5);
        // p99 of this tiny set is the max sample.
        assert!((snap.p99_ms - 30.0).abs() < 0.5);
    }

    #[test]
    fn dropped_counts_over_budget_frames() {
        let mut s = FrameStats::new(Duration::from_micros(11_111));
        for v in [5.0, 5.0, 20.0, 20.0] {
            s.record_frame(ms(v));
        }
        assert_eq!(s.snapshot().dropped, 2);
    }

    #[test]
    fn fps_is_inverse_of_mean() {
        let mut s = FrameStats::new(Duration::from_micros(11_111));
        for _ in 0..10 {
            s.record_frame(ms(10.0)); // 10 ms => 100 fps
        }
        let snap = s.snapshot();
        assert!((snap.fps - 100.0).abs() < 1.0);
    }

    #[test]
    fn ring_buffer_evicts_old_samples() {
        let mut s = FrameStats::with_capacity(Duration::from_micros(11_111), 3);
        for v in [100.0, 100.0, 100.0, 10.0, 10.0, 10.0] {
            s.record_frame(ms(v));
        }
        // Only the last 3 (all 10 ms) should remain.
        assert!((s.snapshot().p50_ms - 10.0).abs() < 0.5);
    }
}
