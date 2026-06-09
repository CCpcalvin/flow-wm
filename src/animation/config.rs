//! Configuration types for [`crate::animation::WindowAnimator`].

use std::time::Duration;

/// Full configuration for a [`crate::animation::WindowAnimator`].
///
/// All fields provide sensible defaults via [`Default`].
///
/// # Example
///
/// ```rust
/// use scrolling_tiling_manager::animation::AnimatorConfig;
/// use std::time::Duration;
///
/// let config = AnimatorConfig {
///     duration: Duration::from_millis(300),
///     ..AnimatorConfig::default()
/// };
/// ```
#[derive(Clone, Debug)]
pub struct AnimatorConfig {
    /// Total duration of one animation batch.
    pub duration: Duration,

    /// Easing curve applied to position channels (x, y).
    pub position_animation: PositionAnimation,

    /// Easing curve applied to size channels (w, h).
    pub size_animation: SizeAnimation,

    /// How to handle a new [`animate`](crate::animation::WindowAnimator::animate) call
    /// while an animation is already playing.
    pub interrupt_policy: InterruptPolicy,

    /// Strategy used to pace the frame loop.
    pub frame_pacing: FramePacing,

    /// Frame interval (ms) above which a frame is counted as a jank event
    /// in [`crate::animation::metrics::FrameMetrics`].
    pub jank_threshold_ms: f64,
}

impl Default for AnimatorConfig {
    fn default() -> Self {
        Self {
            duration: Duration::from_millis(250),
            position_animation: PositionAnimation::EaseInOut,
            size_animation: SizeAnimation::Linear,
            interrupt_policy: InterruptPolicy::RetargetFromCurrent,
            frame_pacing: FramePacing::DwmFlush,
            jank_threshold_ms: 25.0,
        }
    }
}

/// Easing curve for window position channels (x, y).
///
/// Expressive position curves work better than expressive resize curves
/// because resize-heavy transitions look worse on Windows (the window
/// content reflows). Reserve elastic/overshoot styles for translation-only
/// moves.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum PositionAnimation {
    /// Constant-velocity linear interpolation.
    Linear,

    /// Smooth ease-in and ease-out (`EaseInOutCubic`).
    ///
    /// Default — produces natural-looking window movement.
    #[default]
    EaseInOut,

    /// Fast departure, decelerating arrival (`EaseOutCubic`).
    EaseOutCubic,

    /// Immediate departure, very long deceleration tail (`EaseOutExpo`).
    EaseOutExpo,

    /// Slight overshoot then snap to target (`EaseOutElastic`).
    ///
    /// Best for pure translations; avoid when windows are also resizing.
    EaseElastic,
}

/// Easing curve for window size channels (w, h).
///
/// Resize curves are intentionally more conservative: expressive resize
/// looks poor on Windows because window content must reflow each frame.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum SizeAnimation {
    /// Constant-velocity linear interpolation.
    ///
    /// Default — stable and predictable for resizes.
    #[default]
    Linear,

    /// Smooth ease-in and ease-out for size (`EaseInOutCubic`).
    EaseInOut,

    /// Skip interpolation for any size axis that is unchanged, treating
    /// those moves as pure translations for that axis.
    ///
    /// When both width and height are unchanged this is effectively a
    /// no-resize check; size submission is still made through the batch
    /// but the values are constant.
    DisabledIfUnchanged,
}

/// Behaviour when a new [`animate`](crate::animation::WindowAnimator::animate) request
/// arrives while an animation is already in flight.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum InterruptPolicy {
    /// Sample the current interpolated position of every window in the
    /// active batch, cancel that batch, and immediately start a new one
    /// from those interpolated positions toward the new targets.
    ///
    /// Default — preserves visual continuity across interruptions.
    #[default]
    RetargetFromCurrent,

    /// Finish the current animation, then start the queued one.
    QueueAfterCurrent,

    /// Silently discard the new request if an animation is playing.
    DropNew,
}

/// Strategy used to pace the animation frame loop.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum FramePacing {
    /// After each frame, call `DwmFlush()` to block until the Desktop
    /// Window Manager has composited the queued work.
    ///
    /// This is the primary pacing primitive and produces better frame
    /// timing than a fixed `sleep` interval.
    #[default]
    DwmFlush,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sensible_values() {
        let c = AnimatorConfig::default();
        assert_eq!(c.duration, Duration::from_millis(250));
        assert_eq!(c.position_animation, PositionAnimation::EaseInOut);
        assert_eq!(c.size_animation, SizeAnimation::Linear);
        assert_eq!(c.interrupt_policy, InterruptPolicy::RetargetFromCurrent);
        assert_eq!(c.frame_pacing, FramePacing::DwmFlush);
        assert!(c.jank_threshold_ms > 0.0);
    }

    #[test]
    fn config_is_clone() {
        let c = AnimatorConfig::default();
        let _c2 = c.clone();
    }
}
