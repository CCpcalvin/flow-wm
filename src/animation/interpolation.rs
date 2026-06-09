//! Linear interpolation utilities for window geometry.
//!
//! All functions operate on already-eased time values — the caller is
//! responsible for applying an easing curve (via [`crate::animation::easing`]) before
//! passing `t` here.

use crate::animation::types::Rect;

// ---------------------------------------------------------------------------
// Scalar interpolation
// ---------------------------------------------------------------------------

/// Linearly interpolate between two `i32` pixel values and round to the
/// nearest integer.
///
/// `t` should be a pre-eased normalised time in `[0, 1]`.  Values outside
/// that range are accepted and produce extrapolated results.
///
/// # Examples
///
/// ```rust,ignore
/// // Use via the public re-exports or internally within the animation module.
/// // This function is `pub(crate)` — accessible from within the crate.
/// ```
#[inline]
#[allow(clippy::cast_possible_truncation)] // safe: inputs are i32, so the interpolated
// f64 fits in i32; .round() removes the fraction.
pub fn lerp_i32(from: i32, to: i32, t: f64) -> i32 {
    // Use mul_add for a single FMA instruction where the hardware supports it.
    f64::from(to - from).mul_add(t, f64::from(from)).round() as i32
}

/// Linearly interpolate between two `f64` values.
///
/// `t` should be a pre-eased normalised time in `[0, 1]`.
#[inline]
#[allow(dead_code)] // Available as a utility; not yet consumed by other crate modules.
pub fn lerp_f64(from: f64, to: f64, t: f64) -> f64 {
    (to - from).mul_add(t, from)
}

// ---------------------------------------------------------------------------
// Rectangle interpolation
// ---------------------------------------------------------------------------

/// Interpolate between two [`Rect`]s using separate eased time values for the
/// position and size channels.
///
/// - `tp` — pre-eased time for position (x, y).
/// - `ts` — pre-eased time for size (w, h).
///
/// Separating the two times allows callers to apply different easing curves to
/// movement and resizing independently, which is the core design of this crate.
#[inline]
pub fn lerp_rect(from: Rect, to: Rect, tp: f64, ts: f64) -> Rect {
    Rect {
        x: lerp_i32(from.x, to.x, tp),
        y: lerp_i32(from.y, to.y, tp),
        w: lerp_i32(from.w, to.w, ts),
        h: lerp_i32(from.h, to.h, ts),
    }
}

// ---------------------------------------------------------------------------
// Predicate helpers
// ---------------------------------------------------------------------------

/// Returns `true` when `from` and `to` are identical — the animation is a
/// no-op and no frames need to be submitted.
#[inline]
#[allow(dead_code)] // Available as a utility; not yet consumed by other crate modules.
pub fn is_noop(from: &Rect, to: &Rect) -> bool {
    from == to
}

/// Returns `true` when `from` and `to` have the same width and height — the
/// animation is a pure translation with no resize component.
///
/// Callers can use this to select a more expressive position easing (e.g.
/// elastic) that would look poor when combined with simultaneous resizing.
#[inline]
#[allow(dead_code)] // Available as a utility; not yet consumed by other crate modules.
pub fn is_translation_only(from: &Rect, to: &Rect) -> bool {
    from.w == to.w && from.h == to.h
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- lerp_i32 ---

    #[test]
    fn lerp_i32_midpoint() {
        assert_eq!(lerp_i32(0, 100, 0.5), 50);
    }

    #[test]
    fn lerp_i32_boundaries() {
        assert_eq!(lerp_i32(0, 100, 0.0), 0);
        assert_eq!(lerp_i32(0, 100, 1.0), 100);
    }

    #[test]
    fn lerp_i32_rounding() {
        // 0.4 of 1 = 0.4 → rounds to 0
        assert_eq!(lerp_i32(0, 1, 0.4), 0);
        // 0.6 of 1 = 0.6 → rounds to 1
        assert_eq!(lerp_i32(0, 1, 0.6), 1);
    }

    #[test]
    fn lerp_i32_negative_range() {
        assert_eq!(lerp_i32(-100, 100, 0.5), 0);
    }

    // --- lerp_f64 ---

    #[test]
    fn lerp_f64_midpoint() {
        assert!((lerp_f64(0.0, 1.0, 0.5) - 0.5).abs() < 1e-15);
    }

    // --- lerp_rect ---

    #[test]
    fn lerp_rect_separate_position_and_size() {
        let from = Rect::new(0, 0, 100, 100);
        let to = Rect::new(200, 400, 300, 500);

        // tp=0.5, ts=1.0: position at midpoint, size fully at target
        let result = lerp_rect(from, to, 0.5, 1.0);
        assert_eq!(result.x, 100);
        assert_eq!(result.y, 200);
        assert_eq!(result.w, 300);
        assert_eq!(result.h, 500);
    }

    #[test]
    fn lerp_rect_at_zero_returns_from() {
        let from = Rect::new(10, 20, 300, 200);
        let to = Rect::new(50, 80, 600, 400);
        let result = lerp_rect(from, to, 0.0, 0.0);
        assert_eq!(result, from);
    }

    #[test]
    fn lerp_rect_at_one_returns_to() {
        let from = Rect::new(10, 20, 300, 200);
        let to = Rect::new(50, 80, 600, 400);
        let result = lerp_rect(from, to, 1.0, 1.0);
        assert_eq!(result, to);
    }

    // --- is_noop ---

    #[test]
    fn is_noop_identical_rects() {
        let r = Rect::new(0, 0, 100, 100);
        assert!(is_noop(&r, &r));
    }

    #[test]
    fn is_noop_different_position() {
        let from = Rect::new(0, 0, 100, 100);
        let to = Rect::new(10, 0, 100, 100);
        assert!(!is_noop(&from, &to));
    }

    // --- is_translation_only ---

    #[test]
    fn is_translation_only_same_size() {
        let from = Rect::new(0, 0, 100, 200);
        let to = Rect::new(50, 50, 100, 200);
        assert!(is_translation_only(&from, &to));
    }

    #[test]
    fn is_translation_only_different_size() {
        let from = Rect::new(0, 0, 100, 200);
        let to = Rect::new(0, 0, 150, 200);
        assert!(!is_translation_only(&from, &to));
    }

    #[test]
    fn is_translation_only_noop_is_also_translation_only() {
        let r = Rect::new(10, 10, 80, 80);
        // A no-op rect trivially has identical w and h.
        assert!(is_translation_only(&r, &r));
    }
}
