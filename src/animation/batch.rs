//! In-flight animation batch and tween construction.
//!
//! An [`AnimationBatch`] holds all per-window tweens for one animation run and
//! provides progress tracking and per-frame interpolation.

use std::time::{Duration, Instant};

use crate::animation::{
    config::{AnimatorConfig, SizeAnimation},
    easing::{ease_position, ease_size},
    interpolation::lerp_rect,
    types::{Rect, WindowRef, WindowTarget},
};

// ---------------------------------------------------------------------------
// WindowTween
// ---------------------------------------------------------------------------

/// One per-window tween within an active animation batch.
pub(crate) struct WindowTween {
    /// The window being animated.
    pub window_ref: WindowRef,
    /// Geometry at the start of the animation.
    pub from_rect: Rect,
    /// Desired geometry at the end of the animation.
    pub to_rect: Rect,
    /// `true` when [`SizeAnimation::DisabledIfUnchanged`] is active **and**
    /// the window's width and height are already equal to the target.
    ///
    /// The size channels still participate in interpolation, but because
    /// `from == to` their output is constant regardless of the easing shape.
    pub size_skip: bool,
}

// ---------------------------------------------------------------------------
// AnimationBatch
// ---------------------------------------------------------------------------

/// The active animation batch shared by the worker thread.
///
/// Holds all tweens for one animation run and owns the start time used to
/// compute normalised progress.
pub(crate) struct AnimationBatch {
    /// Wall-clock instant at which the animation started.
    pub start_time: Instant,
    /// Total duration of the animation.
    pub duration: Duration,
    /// Per-window tweens to interpolate each frame.
    pub tweens: Vec<WindowTween>,
}

impl AnimationBatch {
    /// Create a new batch starting at the current instant.
    pub fn new(tweens: Vec<WindowTween>, duration: Duration) -> Self {
        Self {
            start_time: Instant::now(),
            duration,
            tweens,
        }
    }

    /// Normalised progress in `[0.0, 1.0]`.
    ///
    /// Returns `0.0` immediately after construction and clamps to `1.0` once
    /// elapsed time reaches or exceeds [`Self::duration`].
    pub fn progress(&self) -> f64 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let elapsed = self.start_time.elapsed();
        (elapsed.as_secs_f64() / self.duration.as_secs_f64()).min(1.0)
    }

    /// Compute the interpolated [`Rect`] for a single tween at normalised time `t`.
    ///
    /// Applies easing independently to position and size channels:
    /// - `tp = ease_position(&config.position_animation, t)`
    /// - `ts = ease_size(&config.size_animation, tween.size_skip, t)`
    ///
    /// Then delegates to [`lerp_rect`].
    pub fn interpolated_rect(tween: &WindowTween, config: &AnimatorConfig, t: f64) -> Rect {
        let tp = ease_position(&config.position_animation, t);
        let ts = ease_size(&config.size_animation, tween.size_skip, t);
        lerp_rect(tween.from_rect, tween.to_rect, tp, ts)
    }
}

// ---------------------------------------------------------------------------
// build_tweens
// ---------------------------------------------------------------------------

/// Build a [`Vec<WindowTween>`] from a list of [`WindowTarget`]s and their
/// current geometry.
///
/// **No-op filter**: windows whose `from_rect == to_rect` are silently
/// skipped — submitting them would waste a frame without visual effect.
///
/// **`size_skip`**: set to `true` when `config.size_animation` is
/// [`SizeAnimation::DisabledIfUnchanged`] *and* the window's width and height
/// are already equal to the target values.  The size channels are still
/// interpolated, but because `from == to` the output is constant.
///
/// `from_rects` is a slice of `(WindowRef, Rect)` pairs representing the
/// current (or mid-animation) position of each window.  Windows not present
/// in `from_rects` are skipped.
pub(crate) fn build_tweens(
    targets: &[WindowTarget],
    from_rects: &[(WindowRef, Rect)],
    config: &AnimatorConfig,
) -> Vec<WindowTween> {
    targets
        .iter()
        .filter_map(|target| {
            // Locate the current rect for this window.
            let from_rect = from_rects
                .iter()
                .find(|(wref, _)| *wref == target.window_ref)
                .map(|(_, rect)| *rect)?;

            let to_rect = target.as_rect();

            // No-op filter: skip windows that are already at their target.
            if from_rect == to_rect {
                return None;
            }

            let size_skip = config.size_animation == SizeAnimation::DisabledIfUnchanged
                && from_rect.w == to_rect.w
                && from_rect.h == to_rect.h;

            Some(WindowTween {
                window_ref: target.window_ref,
                from_rect,
                to_rect,
                size_skip,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::animation::{
        config::{AnimatorConfig, SizeAnimation},
        types::{IVec2, WindowRef, WindowTarget},
    };
    use std::time::Duration;

    fn make_config() -> AnimatorConfig {
        AnimatorConfig::default()
    }

    fn make_batch(dur_ms: u64) -> AnimationBatch {
        AnimationBatch::new(vec![], Duration::from_millis(dur_ms))
    }

    // --- progress ---

    #[test]
    fn progress_is_zero_at_start() {
        let batch = make_batch(500);
        // Immediately after construction progress should be essentially 0.
        // Allow a small epsilon for scheduling jitter (< 5 ms).
        assert!(batch.progress() < 0.05, "progress={}", batch.progress());
    }

    #[test]
    fn progress_clamps_to_one_after_duration() {
        // Use a zero-duration batch — guaranteed to be complete.
        let batch = make_batch(0);
        assert_eq!(batch.progress(), 1.0);
    }

    #[test]
    fn progress_clamps_to_exactly_one_after_real_duration() {
        // A non-zero duration exercises the `.min(1.0)` clamp rather than the
        // `duration.is_zero()` short-circuit. After enough wall-clock time the
        // animator must observe progress() == *exactly* 1.0, because its
        // completion check is `t >= 1.0` and the batch is only retired on the
        // frame applied at that exact value (see animator.rs `run_worker`).
        let batch = make_batch(5);
        std::thread::sleep(Duration::from_millis(20));
        let p = batch.progress();
        assert!(
            (p - 1.0).abs() < f64::EPSILON,
            "progress should clamp to exactly 1.0, got {p}"
        );
    }

    /// Locks down the load-bearing upper-bound invariant: `progress()` must
    /// **never** exceed `1.0`, even when wall-clock elapsed time is vastly
    /// larger than the batch duration.
    ///
    /// This invariant is what makes `t >= 1.0` a *precise* and *safe*
    /// completion signal in `run_worker` (animator.rs). If `progress()` could
    /// exceed `1.0`, then `t >= 1.0` would be true on a frame whose
    /// interpolation happened at `t > 1.0` — potentially past the target rect
    /// if easing curves overshoot. The `.min(1.0)` clamp prevents this.
    ///
    /// Uses a 1 ms duration with a 50 ms sleep (50× overshoot) to stress the
    /// ratio `elapsed / duration` and confirm the clamp holds regardless of
    /// how much time passes.
    #[test]
    fn progress_never_exceeds_one_even_well_past_duration() {
        let batch = make_batch(1); // 1 ms duration — tiny, high ratio after sleep
        std::thread::sleep(Duration::from_millis(50)); // 50× past duration
        let p = batch.progress();
        assert!(
            p <= 1.0,
            "progress must never exceed 1.0; got {p} \
             (this breaks the t >= 1.0 completion invariant in run_worker)"
        );
        assert!(
            (p - 1.0).abs() < f64::EPSILON,
            "progress should be exactly 1.0 when well past duration, got {p}"
        );
    }

    /// Verifies that the `duration.is_zero()` short-circuit path in `progress()`
    /// returns exactly `1.0` on **every** call, not just the first. Because
    /// the short-circuit ignores `start_time.elapsed()` entirely, repeated
    /// calls should all return `1.0` regardless of how much wall-clock time
    /// has elapsed since construction.
    #[test]
    fn progress_is_always_one_for_zero_duration() {
        let batch = make_batch(0);
        // First call immediately.
        assert_eq!(batch.progress(), 1.0);
        // Second call after some wall-clock time — still 1.0.
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(batch.progress(), 1.0);
    }

    // --- interpolated_rect ---

    #[test]
    fn interpolated_rect_at_zero_returns_from() {
        let from = Rect::new(0, 0, 100, 100);
        let to = Rect::new(200, 200, 200, 200);
        let tween = WindowTween {
            window_ref: WindowRef(1),
            from_rect: from,
            to_rect: to,
            size_skip: false,
        };
        let config = make_config();
        let result = AnimationBatch::interpolated_rect(&tween, &config, 0.0);
        assert_eq!(result, from);
    }

    #[test]
    fn interpolated_rect_at_one_returns_to() {
        let from = Rect::new(0, 0, 100, 100);
        let to = Rect::new(200, 200, 200, 200);
        let tween = WindowTween {
            window_ref: WindowRef(1),
            from_rect: from,
            to_rect: to,
            size_skip: false,
        };
        let config = make_config();
        let result = AnimationBatch::interpolated_rect(&tween, &config, 1.0);
        assert_eq!(result, to);
    }

    // --- build_tweens ---

    fn target(id: isize, x: i32, y: i32, w: i32, h: i32) -> WindowTarget {
        WindowTarget::new(WindowRef(id), IVec2::new(x, y), IVec2::new(w, h))
    }

    fn from_rect(id: isize, x: i32, y: i32, w: i32, h: i32) -> (WindowRef, Rect) {
        (WindowRef(id), Rect::new(x, y, w, h))
    }

    #[test]
    fn build_tweens_filters_noop_windows() {
        let targets = vec![
            target(1, 0, 0, 100, 100),   // from == to → skip
            target(2, 50, 50, 100, 100), // from != to → include
        ];
        let from_rects = vec![from_rect(1, 0, 0, 100, 100), from_rect(2, 0, 0, 100, 100)];
        let tweens = build_tweens(&targets, &from_rects, &make_config());
        assert_eq!(tweens.len(), 1);
        assert_eq!(tweens[0].window_ref, WindowRef(2));
    }

    #[test]
    fn build_tweens_size_skip_true_when_size_unchanged_and_disabled_if_unchanged() {
        let targets = vec![target(1, 100, 100, 200, 200)]; // position changes, size same
        let from_rects = vec![from_rect(1, 0, 0, 200, 200)];
        let config = AnimatorConfig {
            size_animation: SizeAnimation::DisabledIfUnchanged,
            ..AnimatorConfig::default()
        };
        let tweens = build_tweens(&targets, &from_rects, &config);
        assert_eq!(tweens.len(), 1);
        assert!(tweens[0].size_skip, "expected size_skip=true");
    }

    #[test]
    fn build_tweens_size_skip_false_when_size_changes_even_with_disabled_if_unchanged() {
        let targets = vec![target(1, 0, 0, 300, 300)]; // size changes
        let from_rects = vec![from_rect(1, 0, 0, 200, 200)];
        let config = AnimatorConfig {
            size_animation: SizeAnimation::DisabledIfUnchanged,
            ..AnimatorConfig::default()
        };
        let tweens = build_tweens(&targets, &from_rects, &config);
        assert_eq!(tweens.len(), 1);
        assert!(!tweens[0].size_skip, "expected size_skip=false");
    }

    #[test]
    fn build_tweens_skips_windows_not_in_from_rects() {
        let targets = vec![target(99, 10, 10, 100, 100)];
        let from_rects: Vec<(WindowRef, Rect)> = vec![]; // no match
        let tweens = build_tweens(&targets, &from_rects, &make_config());
        assert!(tweens.is_empty());
    }
}
