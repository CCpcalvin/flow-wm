//! Frame timing metrics for animation runs.
//!
//! [`MetricsRecorder`] accumulates per-frame timestamps during an animation
//! and produces a [`FrameMetrics`] summary at the end of the run.

use std::time::Instant;

// ---------------------------------------------------------------------------
// FrameMetrics
// ---------------------------------------------------------------------------

/// Summary statistics computed from a completed animation run.
#[derive(Debug, Clone, Default)]
pub struct FrameMetrics {
    /// Total number of frames submitted during the run.
    pub frame_count: usize,
    /// Average inter-frame interval in milliseconds.
    pub avg_interval_ms: f64,
    /// 95th-percentile inter-frame interval in milliseconds.
    pub p95_interval_ms: f64,
    /// Worst (maximum) inter-frame interval in milliseconds.
    pub worst_interval_ms: f64,
    /// Number of inter-frame intervals that exceeded the jank threshold.
    pub jank_count: usize,
}

// ---------------------------------------------------------------------------
// MetricsRecorder
// ---------------------------------------------------------------------------

/// Collects frame timestamps during an animation run and computes statistics.
///
/// Create one recorder per animation batch, call [`Self::record_frame`] once
/// per submitted frame, then call [`Self::compute`] after the run finishes.
pub struct MetricsRecorder {
    /// Intervals longer than this value (ms) are counted as jank events.
    jank_threshold_ms: f64,
    /// Ordered list of wall-clock instants, one per recorded frame.
    timestamps: Vec<Instant>,
}

impl MetricsRecorder {
    /// Create a new recorder with the given jank threshold in milliseconds.
    pub fn new(jank_threshold_ms: f64) -> Self {
        Self {
            jank_threshold_ms,
            timestamps: Vec::new(),
        }
    }

    /// Record the current instant as a frame timestamp.
    pub fn record_frame(&mut self) {
        self.timestamps.push(Instant::now());
    }

    /// Compute and return summary [`FrameMetrics`] from all recorded frames.
    ///
    /// Returns [`FrameMetrics::default`] if fewer than 2 frames were recorded
    /// (no inter-frame intervals can be computed).
    ///
    /// # Algorithm
    /// 1. Compute all consecutive inter-frame intervals in milliseconds.
    /// 2. `avg`  = arithmetic mean of all intervals.
    /// 3. `p95`  = value at index `⌊0.95 × count⌋` of the sorted intervals.
    /// 4. `worst`= maximum interval.
    /// 5. `jank_count` = number of intervals that strictly exceed the threshold.
    pub fn compute(&self) -> FrameMetrics {
        let n = self.timestamps.len();
        if n < 2 {
            return FrameMetrics::default();
        }

        // Build inter-frame intervals in milliseconds.
        let mut intervals: Vec<f64> = self
            .timestamps
            .windows(2)
            .map(|pair| {
                let delta = pair[1].duration_since(pair[0]);
                delta.as_secs_f64() * 1_000.0
            })
            .collect();

        let count = intervals.len();
        let sum: f64 = intervals.iter().sum();
        let avg_interval_ms = sum / count as f64;

        // worst before sort so we avoid a second pass.
        let worst_interval_ms = intervals.iter().copied().fold(f64::NEG_INFINITY, f64::max);

        intervals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p95_index = (0.95 * count as f64).floor() as usize;
        let p95_interval_ms = intervals[p95_index.min(count - 1)];

        let jank_threshold_ms = self.jank_threshold_ms;
        let jank_count = intervals
            .iter()
            .filter(|&&iv| iv > jank_threshold_ms)
            .count();

        FrameMetrics {
            frame_count: n,
            avg_interval_ms,
            p95_interval_ms,
            worst_interval_ms,
            jank_count,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn zero_frames_returns_default() {
        let recorder = MetricsRecorder::new(25.0);
        let m = recorder.compute();
        assert_eq!(m.frame_count, 0);
        assert_eq!(m.avg_interval_ms, 0.0);
        assert_eq!(m.jank_count, 0);
    }

    #[test]
    fn one_frame_returns_default() {
        let mut recorder = MetricsRecorder::new(25.0);
        recorder.record_frame();
        let m = recorder.compute();
        assert_eq!(m.frame_count, 0); // default: no intervals computable
        assert_eq!(m.avg_interval_ms, 0.0);
    }

    #[test]
    fn multiple_frames_returns_valid_stats() {
        let mut recorder = MetricsRecorder::new(25.0);
        // Inject synthetic timestamps by recording and sleeping briefly.
        // We use a small sleep so the intervals are non-zero.
        for _ in 0..5 {
            recorder.record_frame();
            std::thread::sleep(Duration::from_millis(5));
        }
        let m = recorder.compute();
        assert_eq!(m.frame_count, 5);
        assert!(m.avg_interval_ms > 0.0, "avg should be > 0");
        assert!(m.worst_interval_ms >= m.avg_interval_ms - 1.0);
        assert!(m.p95_interval_ms > 0.0);
    }

    #[test]
    fn jank_count_correctly_counts_above_threshold() {
        // Build a recorder and directly check the compute algorithm with
        // manually constructed timestamps.
        //
        // We use a threshold of 8 ms and inject timestamps spaced at
        // 5 ms, 5 ms, 10 ms, 5 ms → 3 intervals below threshold, 1 above.
        let mut recorder = MetricsRecorder::new(8.0);

        let base = Instant::now();
        recorder.timestamps.push(base);
        recorder.timestamps.push(base + Duration::from_millis(5));
        recorder.timestamps.push(base + Duration::from_millis(10));
        recorder.timestamps.push(base + Duration::from_millis(20)); // 10 ms gap → jank
        recorder.timestamps.push(base + Duration::from_millis(25));

        let m = recorder.compute();
        assert_eq!(m.frame_count, 5);
        // Intervals: 5, 5, 10, 5 ms. Only the 10 ms interval > 8 ms threshold.
        assert_eq!(m.jank_count, 1, "expected exactly 1 jank interval");
        assert!(
            (m.worst_interval_ms - 10.0).abs() < 1.0,
            "worst={}",
            m.worst_interval_ms
        );
        // avg = (5+5+10+5)/4 = 6.25
        assert!(
            (m.avg_interval_ms - 6.25).abs() < 0.5,
            "avg={}",
            m.avg_interval_ms
        );
    }

    #[test]
    fn jank_count_zero_when_all_intervals_below_threshold() {
        let mut recorder = MetricsRecorder::new(100.0);
        let base = Instant::now();
        recorder.timestamps.push(base);
        recorder.timestamps.push(base + Duration::from_millis(10));
        recorder.timestamps.push(base + Duration::from_millis(20));

        let m = recorder.compute();
        assert_eq!(m.jank_count, 0);
    }
}
