//! Easing curves for window animation.
//!
//! Provides [`EasingStyle`], a comprehensive set of easing variants, and
//! [`apply_ease`], a pure function that evaluates any curve at time `t ∈ [0, 1]`.
//!
//! Adapter functions [`ease_position`] and [`ease_size`] map the higher-level
//! config enums ([`crate::animation::config::PositionAnimation`], [`crate::animation::config::SizeAnimation`])
//! to an eased time value ready for interpolation.

use std::f64::consts::PI;

// ---------------------------------------------------------------------------
// Public enum
// ---------------------------------------------------------------------------

/// All supported easing curve variants.
///
/// The `CubicBezier` variant accepts four control-point coordinates
/// `(x1, y1, x2, y2)` in the CSS `cubic-bezier()` convention.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)] // Full variant set available for future use; only a subset is currently mapped.
pub enum EasingStyle {
    /// Constant-velocity linear interpolation.
    Linear,
    /// Accelerating sine curve.
    EaseInSine,
    /// Decelerating sine curve.
    EaseOutSine,
    /// Symmetric sine ease-in-out.
    EaseInOutSine,
    /// Accelerating quadratic curve.
    EaseInQuad,
    /// Decelerating quadratic curve.
    EaseOutQuad,
    /// Symmetric quadratic ease-in-out.
    EaseInOutQuad,
    /// Accelerating cubic curve.
    EaseInCubic,
    /// Decelerating cubic curve.
    EaseOutCubic,
    /// Symmetric cubic ease-in-out.
    EaseInOutCubic,
    /// Accelerating quartic curve.
    EaseInQuart,
    /// Decelerating quartic curve.
    EaseOutQuart,
    /// Symmetric quartic ease-in-out.
    EaseInOutQuart,
    /// Accelerating quintic curve.
    EaseInQuint,
    /// Decelerating quintic curve.
    EaseOutQuint,
    /// Symmetric quintic ease-in-out.
    EaseInOutQuint,
    /// Accelerating exponential curve.
    EaseInExpo,
    /// Decelerating exponential curve.
    EaseOutExpo,
    /// Symmetric exponential ease-in-out.
    EaseInOutExpo,
    /// Accelerating circular curve.
    EaseInCirc,
    /// Decelerating circular curve.
    EaseOutCirc,
    /// Symmetric circular ease-in-out.
    EaseInOutCirc,
    /// Slight pull-back before departure.
    EaseInBack,
    /// Slight overshoot past the target.
    EaseOutBack,
    /// Symmetric back ease-in-out.
    EaseInOutBack,
    /// Elastic oscillation on departure.
    EaseInElastic,
    /// Elastic oscillation on arrival.
    EaseOutElastic,
    /// Symmetric elastic ease-in-out.
    EaseInOutElastic,
    /// Bounce on departure.
    EaseInBounce,
    /// Bounce on arrival.
    EaseOutBounce,
    /// Symmetric bounce ease-in-out.
    EaseInOutBounce,
    /// Arbitrary CSS cubic-bezier curve defined by two control points.
    ///
    /// Arguments are `(x1, y1, x2, y2)`.
    CubicBezier(f64, f64, f64, f64),
}

// ---------------------------------------------------------------------------
// Public dispatch
// ---------------------------------------------------------------------------

/// Evaluate the easing curve `style` at normalised time `t ∈ [0, 1]`.
///
/// The return value is also normalised to `[0, 1]` for all non-oscillating
/// styles. Elastic and bounce variants may transiently exceed this range.
#[inline]
pub fn apply_ease(t: f64, style: EasingStyle) -> f64 {
    match style {
        EasingStyle::Linear => Linear::evaluate(t),
        EasingStyle::EaseInSine => EaseInSine::evaluate(t),
        EasingStyle::EaseOutSine => EaseOutSine::evaluate(t),
        EasingStyle::EaseInOutSine => EaseInOutSine::evaluate(t),
        EasingStyle::EaseInQuad => EaseInQuad::evaluate(t),
        EasingStyle::EaseOutQuad => EaseOutQuad::evaluate(t),
        EasingStyle::EaseInOutQuad => EaseInOutQuad::evaluate(t),
        EasingStyle::EaseInCubic => EaseInCubic::evaluate(t),
        EasingStyle::EaseOutCubic => EaseOutCubic::evaluate(t),
        EasingStyle::EaseInOutCubic => EaseInOutCubic::evaluate(t),
        EasingStyle::EaseInQuart => EaseInQuart::evaluate(t),
        EasingStyle::EaseOutQuart => EaseOutQuart::evaluate(t),
        EasingStyle::EaseInOutQuart => EaseInOutQuart::evaluate(t),
        EasingStyle::EaseInQuint => EaseInQuint::evaluate(t),
        EasingStyle::EaseOutQuint => EaseOutQuint::evaluate(t),
        EasingStyle::EaseInOutQuint => EaseInOutQuint::evaluate(t),
        EasingStyle::EaseInExpo => EaseInExpo::evaluate(t),
        EasingStyle::EaseOutExpo => EaseOutExpo::evaluate(t),
        EasingStyle::EaseInOutExpo => EaseInOutExpo::evaluate(t),
        EasingStyle::EaseInCirc => EaseInCirc::evaluate(t),
        EasingStyle::EaseOutCirc => EaseOutCirc::evaluate(t),
        EasingStyle::EaseInOutCirc => EaseInOutCirc::evaluate(t),
        EasingStyle::EaseInBack => EaseInBack::evaluate(t),
        EasingStyle::EaseOutBack => EaseOutBack::evaluate(t),
        EasingStyle::EaseInOutBack => EaseInOutBack::evaluate(t),
        EasingStyle::EaseInElastic => EaseInElastic::evaluate(t),
        EasingStyle::EaseOutElastic => EaseOutElastic::evaluate(t),
        EasingStyle::EaseInOutElastic => EaseInOutElastic::evaluate(t),
        EasingStyle::EaseInBounce => EaseInBounce::evaluate(t),
        EasingStyle::EaseOutBounce => EaseOutBounce::evaluate(t),
        EasingStyle::EaseInOutBounce => EaseInOutBounce::evaluate(t),
        EasingStyle::CubicBezier(x1, y1, x2, y2) => {
            CubicBezierSolver { x1, y1, x2, y2 }.evaluate(t)
        }
    }
}

// ---------------------------------------------------------------------------
// Config adapters
// ---------------------------------------------------------------------------

/// Map a [`crate::animation::config::PositionAnimation`] variant to a eased time value.
///
/// Pass the raw normalised time `t ∈ [0, 1]`; returns an eased value in the
/// same range suitable for interpolating position channels (x, y).
#[inline]
pub fn ease_position(style: &crate::animation::config::PositionAnimation, t: f64) -> f64 {
    let curve = match style {
        crate::animation::config::PositionAnimation::Linear => EasingStyle::Linear,
        crate::animation::config::PositionAnimation::EaseInOut => EasingStyle::EaseInOutCubic,
        crate::animation::config::PositionAnimation::EaseOutCubic => EasingStyle::EaseOutCubic,
        crate::animation::config::PositionAnimation::EaseOutExpo => EasingStyle::EaseOutExpo,
        crate::animation::config::PositionAnimation::EaseElastic => EasingStyle::EaseOutElastic,
    };
    apply_ease(t, curve)
}

/// Map a [`crate::animation::config::SizeAnimation`] variant to an eased time value.
///
/// `from_size_matches` should be `true` when the window's current size already
/// equals the target size (i.e. the resize channel is a no-op).  In that case
/// [`SizeAnimation::DisabledIfUnchanged`] returns `t` unchanged so the caller
/// can still use the same from/to value without producing visual artifacts.
#[inline]
pub fn ease_size(
    style: &crate::animation::config::SizeAnimation,
    from_size_matches: bool,
    t: f64,
) -> f64 {
    let curve = match style {
        crate::animation::config::SizeAnimation::Linear => EasingStyle::Linear,
        crate::animation::config::SizeAnimation::EaseInOut => EasingStyle::EaseInOutCubic,
        crate::animation::config::SizeAnimation::DisabledIfUnchanged => {
            if from_size_matches {
                // Size is unchanged; linear passthrough — the from/to values
                // are identical so the easing shape is irrelevant.
                return t;
            }
            EasingStyle::Linear
        }
    };
    apply_ease(t, curve)
}

// ---------------------------------------------------------------------------
// Private zero-sized curve structs
// ---------------------------------------------------------------------------

struct Linear;
impl Linear {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        t
    }
}

struct EaseInSine;
impl EaseInSine {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        1.0 - f64::cos((t * PI) / 2.0)
    }
}

struct EaseOutSine;
impl EaseOutSine {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        f64::sin((t * PI) / 2.0)
    }
}

struct EaseInOutSine;
impl EaseInOutSine {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        -(f64::cos(PI * t) - 1.0) / 2.0
    }
}

struct EaseInQuad;
impl EaseInQuad {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        t * t
    }
}

struct EaseOutQuad;
impl EaseOutQuad {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        (1.0 - t).mul_add(-(1.0 - t), 1.0)
    }
}

struct EaseInOutQuad;
impl EaseInOutQuad {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        if t < 0.5 {
            2.0 * t * t
        } else {
            1.0 - (-2.0f64).mul_add(t, 2.0).powi(2) / 2.0
        }
    }
}

struct EaseInCubic;
impl EaseInCubic {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        t * t * t
    }
}

struct EaseOutCubic;
impl EaseOutCubic {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        1.0 - (1.0 - t).powi(3)
    }
}

struct EaseInOutCubic;
impl EaseInOutCubic {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        if t < 0.5 {
            4.0 * t * t * t
        } else {
            1.0 - (-2.0f64).mul_add(t, 2.0).powi(3) / 2.0
        }
    }
}

struct EaseInQuart;
impl EaseInQuart {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        t * t * t * t
    }
}

struct EaseOutQuart;
impl EaseOutQuart {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        1.0 - (1.0 - t).powi(4)
    }
}

struct EaseInOutQuart;
impl EaseInOutQuart {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        if t < 0.5 {
            8.0 * t * t * t * t
        } else {
            1.0 - (-2.0f64).mul_add(t, 2.0).powi(4) / 2.0
        }
    }
}

struct EaseInQuint;
impl EaseInQuint {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        t * t * t * t * t
    }
}

struct EaseOutQuint;
impl EaseOutQuint {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        1.0 - (1.0 - t).powi(5)
    }
}

struct EaseInOutQuint;
impl EaseInOutQuint {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        if t < 0.5 {
            16.0 * t * t * t * t * t
        } else {
            1.0 - (-2.0f64).mul_add(t, 2.0).powi(5) / 2.0
        }
    }
}

struct EaseInExpo;
impl EaseInExpo {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        if t.abs() < f64::EPSILON {
            return 0.0;
        }
        10.0f64.mul_add(t, -10.0).exp2()
    }
}

struct EaseOutExpo;
impl EaseOutExpo {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        if (t - 1.0).abs() < f64::EPSILON {
            return 1.0;
        }
        1.0 - (-10.0 * t).exp2()
    }
}

struct EaseInOutExpo;
impl EaseInOutExpo {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        if t.abs() < f64::EPSILON || (t - 1.0).abs() < f64::EPSILON {
            return t;
        }
        if t < 0.5 {
            20.0f64.mul_add(t, -10.0).exp2() / 2.0
        } else {
            (2.0 - (-20.0f64).mul_add(t, 10.0).exp2()) / 2.0
        }
    }
}

struct EaseInCirc;
impl EaseInCirc {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        1.0 - f64::sqrt(t.mul_add(-t, 1.0))
    }
}

struct EaseOutCirc;
impl EaseOutCirc {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        f64::sqrt((t - 1.0).mul_add(-(t - 1.0), 1.0))
    }
}

struct EaseInOutCirc;
impl EaseInOutCirc {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        if t < 0.5 {
            (1.0 - f64::sqrt((2.0 * t).mul_add(-(2.0 * t), 1.0))) / 2.0
        } else {
            (f64::sqrt(
                (-2.0f64)
                    .mul_add(t, 2.0)
                    .mul_add(-(-2.0f64).mul_add(t, 2.0), 1.0),
            ) + 1.0)
                / 2.0
        }
    }
}

struct EaseInBack;
impl EaseInBack {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        let c1 = 1.70158_f64;
        let c3 = c1 + 1.0;
        (c3 * t * t).mul_add(t, -c1 * t * t)
    }
}

struct EaseOutBack;
impl EaseOutBack {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        let c1: f64 = 1.70158;
        let c3: f64 = c1 + 1.0;
        c1.mul_add((t - 1.0).powi(2), c3.mul_add((t - 1.0).powi(3), 1.0))
    }
}

struct EaseInOutBack;
impl EaseInOutBack {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        let c1: f64 = 1.70158;
        let c2: f64 = c1 * 1.525;
        if t < 0.5 {
            ((2.0 * t).powi(2) * ((c2 + 1.0) * 2.0).mul_add(t, -c2)) / 2.0
        } else {
            ((2.0f64.mul_add(t, -2.0))
                .powi(2)
                .mul_add((c2 + 1.0).mul_add(t.mul_add(2.0, -2.0), c2), 2.0))
                / 2.0
        }
    }
}

struct EaseInElastic;
impl EaseInElastic {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        if t.abs() < f64::EPSILON || (t - 1.0).abs() < f64::EPSILON {
            return t;
        }
        let c4 = (2.0 * PI) / 3.0;
        -(10.0f64.mul_add(t, -10.0).exp2()) * f64::sin(t.mul_add(10.0, -10.75) * c4)
    }
}

struct EaseOutElastic;
impl EaseOutElastic {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        if t.abs() < f64::EPSILON || (t - 1.0).abs() < f64::EPSILON {
            return t;
        }
        let c4 = (2.0 * PI) / 3.0;
        (-10.0 * t)
            .exp2()
            .mul_add(f64::sin(t.mul_add(10.0, -0.75) * c4), 1.0)
    }
}

struct EaseInOutElastic;
impl EaseInOutElastic {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        if t.abs() < f64::EPSILON || (t - 1.0).abs() < f64::EPSILON {
            return t;
        }
        let c5 = (2.0 * PI) / 4.5;
        if t < 0.5 {
            -(20.0f64.mul_add(t, -10.0).exp2() * f64::sin(20.0f64.mul_add(t, -11.125) * c5)) / 2.0
        } else {
            ((-20.0f64).mul_add(t, 10.0).exp2() * f64::sin(20.0f64.mul_add(t, -11.125) * c5)) / 2.0
                + 1.0
        }
    }
}

struct EaseOutBounce;
impl EaseOutBounce {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        let mut time = t;
        let n1 = 7.5625_f64;
        let d1 = 2.75_f64;

        if t < 1.0 / d1 {
            n1 * time * time
        } else if time < 2.0 / d1 {
            time -= 1.5 / d1;
            (n1 * time).mul_add(time, 0.75)
        } else if time < 2.5 / d1 {
            time -= 2.25 / d1;
            (n1 * time).mul_add(time, 0.9375)
        } else {
            time -= 2.625 / d1;
            (n1 * time).mul_add(time, 0.984_375)
        }
    }
}

struct EaseInBounce;
impl EaseInBounce {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        1.0 - EaseOutBounce::evaluate(1.0 - t)
    }
}

struct EaseInOutBounce;
impl EaseInOutBounce {
    #[inline]
    fn evaluate(t: f64) -> f64 {
        if t < 0.5 {
            (1.0 - EaseOutBounce::evaluate(2.0f64.mul_add(-t, 1.0))) / 2.0
        } else {
            (1.0 + EaseOutBounce::evaluate(2.0f64.mul_add(t, -1.0))) / 2.0
        }
    }
}

// ---------------------------------------------------------------------------
// Cubic Bézier solver (Newton-Raphson)
// ---------------------------------------------------------------------------

/// Cubic Bézier easing solver using Newton-Raphson iteration.
///
/// Models the CSS `cubic-bezier(x1, y1, x2, y2)` function.
/// The first and last control points are implicitly (0, 0) and (1, 1).
struct CubicBezierSolver {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
}

impl CubicBezierSolver {
    /// Compute the X component of the Bézier curve at parameter `s`.
    fn x(&self, s: f64) -> f64 {
        3.0 * self.x1 * s * (1.0 - s).powi(2) + 3.0 * self.x2 * s.powi(2) * (1.0 - s) + s.powi(3)
    }

    /// Compute the Y component of the Bézier curve at parameter `s`.
    fn y(&self, s: f64) -> f64 {
        3.0 * self.y1 * s * (1.0 - s).powi(2) + 3.0 * self.y2 * s.powi(2) * (1.0 - s) + s.powi(3)
    }

    /// Derivative dX/ds — used by Newton-Raphson to converge quickly.
    fn dx_ds(&self, s: f64) -> f64 {
        3.0 * self.x1 * (1.0 - s) * (1.0 - 3.0 * s)
            + 3.0 * self.x2 * (2.0 * s - 3.0 * s.powi(2))
            + 3.0 * s.powi(2)
    }

    /// Invert X → find the Bézier parameter `s` such that `x(s) == t`.
    ///
    /// Uses up to 8 Newton-Raphson iterations; clamps result to `[0, 1]`.
    fn find_s(&self, t: f64) -> f64 {
        if t <= 0.0 {
            return 0.0;
        }
        if t >= 1.0 {
            return 1.0;
        }

        let mut s = t; // good initial guess
        for _ in 0..8 {
            let x_val = self.x(s);
            let dx_val = self.dx_ds(s);
            if dx_val.abs() < 1e-6 {
                break;
            }
            let delta = (x_val - t) / dx_val;
            s = (s - delta).clamp(0.0, 1.0);
            if delta.abs() < 1e-6 {
                break;
            }
        }
        s
    }

    /// Evaluate the Bézier easing at normalised time `t`.
    fn evaluate(&self, t: f64) -> f64 {
        let s = self.find_s(t.clamp(0.0, 1.0));
        self.y(s)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const EPSILON: f64 = 1e-9;

    // Helper: check boundary values for a non-elastic style
    fn assert_boundaries(style: EasingStyle) {
        // Clone needed because we consume the enum twice
        let s0 = style.clone();
        assert!(
            (apply_ease(0.0, s0)).abs() < EPSILON,
            "expected 0.0 at t=0 for {style:?}"
        );
        assert!(
            (apply_ease(1.0, style) - 1.0).abs() < EPSILON,
            "expected 1.0 at t=1"
        );
    }

    #[test]
    fn linear_midpoint() {
        assert!((Linear::evaluate(0.5) - 0.5).abs() < EPSILON);
    }

    #[test]
    fn ease_in_out_cubic_symmetric_at_midpoint() {
        assert!((EaseInOutCubic::evaluate(0.5) - 0.5).abs() < EPSILON);
    }

    #[test]
    fn ease_out_cubic_at_one() {
        assert!((EaseOutCubic::evaluate(1.0) - 1.0).abs() < EPSILON);
    }

    #[test]
    fn boundaries_non_elastic() {
        for style in [
            EasingStyle::Linear,
            EasingStyle::EaseInSine,
            EasingStyle::EaseOutSine,
            EasingStyle::EaseInOutSine,
            EasingStyle::EaseInQuad,
            EasingStyle::EaseOutQuad,
            EasingStyle::EaseInOutQuad,
            EasingStyle::EaseInCubic,
            EasingStyle::EaseOutCubic,
            EasingStyle::EaseInOutCubic,
            EasingStyle::EaseInQuart,
            EasingStyle::EaseOutQuart,
            EasingStyle::EaseInOutQuart,
            EasingStyle::EaseInQuint,
            EasingStyle::EaseOutQuint,
            EasingStyle::EaseInOutQuint,
            EasingStyle::EaseInExpo,
            EasingStyle::EaseOutExpo,
            EasingStyle::EaseInOutExpo,
            EasingStyle::EaseInCirc,
            EasingStyle::EaseOutCirc,
            EasingStyle::EaseInOutCirc,
            EasingStyle::EaseInBounce,
            EasingStyle::EaseOutBounce,
            EasingStyle::EaseInOutBounce,
        ] {
            assert_boundaries(style);
        }
    }

    #[test]
    fn cubic_bezier_boundaries() {
        let style = EasingStyle::CubicBezier(0.25, 0.1, 0.25, 1.0);
        assert!((apply_ease(0.0, style.clone())).abs() < EPSILON);
        assert!((apply_ease(1.0, style) - 1.0).abs() < EPSILON);
    }

    #[test]
    fn cubic_bezier_linear_control_points_approximate_linear() {
        // When control points are on the diagonal the curve approximates linear.
        let style = EasingStyle::CubicBezier(0.33, 0.33, 0.66, 0.66);
        let result = apply_ease(0.5, style);
        assert!((result - 0.5).abs() < 0.01, "got {result}");
    }

    #[test]
    fn ease_position_adapter_linear() {
        let r = ease_position(&crate::animation::config::PositionAnimation::Linear, 0.5);
        assert!((r - 0.5).abs() < EPSILON);
    }

    #[test]
    fn ease_size_disabled_if_unchanged_passthrough() {
        let r = ease_size(
            &crate::animation::config::SizeAnimation::DisabledIfUnchanged,
            true,
            0.7,
        );
        assert!((r - 0.7).abs() < EPSILON);
    }

    #[test]
    fn ease_size_disabled_if_unchanged_linear_when_changed() {
        let r = ease_size(
            &crate::animation::config::SizeAnimation::DisabledIfUnchanged,
            false,
            0.6,
        );
        assert!((r - 0.6).abs() < EPSILON);
    }
}
