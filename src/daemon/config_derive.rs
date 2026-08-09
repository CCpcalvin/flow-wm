//! Configuration derivation helpers.
//!
//! This module contains methods that derive configuration values from
//! [`FlowConfig`] for use by the layout engine and animator.

use std::time::Duration;

use crate::animation::AnimatorConfig;
use crate::animation::config::PositionAnimation;
use crate::animation::easing::EasingStyle;
use crate::config::types::ConfigEasing;
use crate::config::types::FlowConfig;
use crate::layout::types::{MonitorInfo, Padding as LayoutPadding};

use super::types::LayoutConfig;

/// Derive scrolling-space parameters from [`FlowConfig`].
///
/// Converts the user-facing config types (from `flow.toml`) into the
/// layout-engine-specific types needed by [`ScrollingSpace::new`](crate::workspace::ScrollingSpace::new).
///
/// # Column Width Resolution
///
/// When `FlowConfig::column_width` is `Some(v)`, that value is used directly
/// (power-user override). When `None`, the width is computed from
/// `columns_per_screen`:
///
/// ```text
/// base_content_width = (monitor_width - (N+1) * window_gap) / N
/// ```
///
/// where `N = columns_per_screen`. This ensures the layout fills the entire
/// screen with uniform gaps.
pub(super) fn derive_layout_config(app_config: &FlowConfig, monitor: &MonitorInfo) -> LayoutConfig {
    let gap = app_config.padding.window_gap;
    let column_width = match app_config.column_width {
        Some(cw) => {
            log::debug!(
                "column_width: using explicit override of {}px (ignoring columns_per_screen={})",
                cw,
                app_config.columns_per_screen,
            );
            cw
        }
        None => {
            let monitor_width = monitor.work_area.width;
            let n = app_config.columns_per_screen as i32;
            // (N+1) gaps: one on each side plus N-1 between columns = N+1 total
            let total_gap = (n + 1) * gap;
            let computed = (monitor_width - total_gap) / n;
            log::debug!(
                "column_width: auto-computed from columns_per_screen={}, monitor_width={}px, window_gap={}px → {}px",
                app_config.columns_per_screen,
                monitor_width,
                gap,
                computed,
            );
            computed as u32
        }
    };

    LayoutConfig {
        column_width,
        min_column_width_px: app_config.min_column_width_px,
        min_window_height_px: app_config.min_window_height_px,
        min_row_height_px: app_config.min_row_height_px,
        padding: LayoutPadding {
            window_gap: app_config.padding.window_gap,
            up: app_config.padding.up,
            down: app_config.padding.down,
        },
    }
}

/// Map a user-configured [`ConfigEasing`] to the animation engine's [`EasingStyle`].
///
/// This is the bridge between the config layer (which cannot import from the
/// animation layer) and the animation engine. Every `ConfigEasing` variant has
/// a 1:1 mapping to an `EasingStyle` variant.
///
/// # Design
///
/// The mapping lives in the `daemon/` layer rather than `config/` or
/// `animation/` to respect the module dependency rule: `config/` must not
/// import from `animation/`, and `animation/` should not depend on config
/// types. The `daemon/` orchestrator sits above both and can safely import
/// from either.
fn config_easing_to_style(easing: &ConfigEasing) -> EasingStyle {
    match easing {
        ConfigEasing::Linear => EasingStyle::Linear,
        ConfigEasing::EaseInSine => EasingStyle::EaseInSine,
        ConfigEasing::EaseOutSine => EasingStyle::EaseOutSine,
        ConfigEasing::EaseInOutSine => EasingStyle::EaseInOutSine,
        ConfigEasing::EaseInQuad => EasingStyle::EaseInQuad,
        ConfigEasing::EaseOutQuad => EasingStyle::EaseOutQuad,
        ConfigEasing::EaseInOutQuad => EasingStyle::EaseInOutQuad,
        ConfigEasing::EaseInCubic => EasingStyle::EaseInCubic,
        ConfigEasing::EaseOutCubic => EasingStyle::EaseOutCubic,
        ConfigEasing::EaseInOutCubic => EasingStyle::EaseInOutCubic,
        ConfigEasing::EaseInQuart => EasingStyle::EaseInQuart,
        ConfigEasing::EaseOutQuart => EasingStyle::EaseOutQuart,
        ConfigEasing::EaseInOutQuart => EasingStyle::EaseInOutQuart,
        ConfigEasing::EaseInQuint => EasingStyle::EaseInQuint,
        ConfigEasing::EaseOutQuint => EasingStyle::EaseOutQuint,
        ConfigEasing::EaseInOutQuint => EasingStyle::EaseInOutQuint,
        ConfigEasing::EaseInExpo => EasingStyle::EaseInExpo,
        ConfigEasing::EaseOutExpo => EasingStyle::EaseOutExpo,
        ConfigEasing::EaseInOutExpo => EasingStyle::EaseInOutExpo,
        ConfigEasing::EaseInCirc => EasingStyle::EaseInCirc,
        ConfigEasing::EaseOutCirc => EasingStyle::EaseOutCirc,
        ConfigEasing::EaseInOutCirc => EasingStyle::EaseInOutCirc,
        ConfigEasing::EaseInBack => EasingStyle::EaseInBack,
        ConfigEasing::EaseOutBack => EasingStyle::EaseOutBack,
        ConfigEasing::EaseInOutBack => EasingStyle::EaseInOutBack,
        ConfigEasing::EaseInElastic => EasingStyle::EaseInElastic,
        ConfigEasing::EaseOutElastic => EasingStyle::EaseOutElastic,
        ConfigEasing::EaseInOutElastic => EasingStyle::EaseInOutElastic,
        ConfigEasing::EaseInBounce => EasingStyle::EaseInBounce,
        ConfigEasing::EaseOutBounce => EasingStyle::EaseOutBounce,
        ConfigEasing::EaseInOutBounce => EasingStyle::EaseInOutBounce,
    }
}

/// Derive animator configuration from [`FlowConfig`].
///
/// The `override_duration` parameter allows the caller to force a specific
/// animation duration. Pass `Duration::ZERO` to let the config decide
/// (enabled → user-configured ms, disabled → zero/instant).
pub(super) fn derive_animator_config(
    app_config: &FlowConfig,
    override_duration: Duration,
) -> AnimatorConfig {
    let duration = if override_duration != Duration::ZERO {
        override_duration
    } else if app_config.animation.enabled {
        Duration::from_millis(app_config.animation.duration_ms as u64)
    } else {
        Duration::ZERO
    };

    let position_animation =
        PositionAnimation::Custom(config_easing_to_style(&app_config.animation.easing));

    AnimatorConfig {
        duration,
        position_animation,
        ..AnimatorConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::animation::config::PositionAnimation;
    use crate::animation::easing::EasingStyle;
    use crate::config::types::AnimationConfig;
    use crate::config::types::ConfigEasing;

    /// Build a default [`FlowConfig`] with animation enabled and a given duration.
    fn make_enabled_config(duration_ms: u32) -> FlowConfig {
        FlowConfig {
            animation: AnimationConfig {
                enabled: true,
                duration_ms,
                ..AnimationConfig::default()
            },
            ..FlowConfig::default()
        }
    }

    /// Build a [`FlowConfig`] with animation disabled.
    fn make_disabled_config() -> FlowConfig {
        FlowConfig {
            animation: AnimationConfig {
                enabled: false,
                ..AnimationConfig::default()
            },
            ..FlowConfig::default()
        }
    }

    // W2-related: verify derive_animator_config with Duration::ZERO sentinel
    // (meaning "no override — use config defaults") respects the user's
    // enabled/disabled setting and returns the configured duration_ms when
    // animation is enabled.
    //
    // Note: the W2 fix does NOT call derive_animator_config for the snap —
    // it constructs AnimatorConfig { duration: Duration::ZERO, ..Default::default() }
    // directly. derive_animator_config is only used for the runtime config (W3).

    #[test]
    fn derive_animator_config_zero_sentinel_uses_user_settings() {
        // Positive: animation enabled + zero sentinel → user's configured duration.
        // This is the W3 case (runtime config after initial snap).
        let cfg = make_enabled_config(250);
        let result = derive_animator_config(&cfg, Duration::ZERO);
        assert_eq!(
            result.duration,
            Duration::from_millis(250),
            "Duration::ZERO sentinel with animation enabled should use user's 250ms"
        );

        // Positive: animation disabled + zero sentinel → zero duration.
        let cfg = make_disabled_config();
        let result = derive_animator_config(&cfg, Duration::ZERO);
        assert_eq!(
            result.duration,
            Duration::ZERO,
            "Duration::ZERO sentinel with animation disabled should produce zero duration"
        );
    }

    /// Negative: verify that a non-zero override takes precedence over user config.
    #[test]
    fn derive_animator_config_nonzero_override_overrides_user() {
        let cfg = make_enabled_config(250);
        let result = derive_animator_config(&cfg, Duration::from_millis(50));
        assert_eq!(
            result.duration,
            Duration::from_millis(50),
            "non-zero override should take precedence over user's 250ms"
        );

        // Also test with animation disabled — override still wins.
        let cfg = make_disabled_config();
        let result = derive_animator_config(&cfg, Duration::from_millis(100));
        assert_eq!(
            result.duration,
            Duration::from_millis(100),
            "non-zero override should take precedence even when animation is disabled"
        );
    }

    /// Verify that `derive_animator_config` maps the config easing to the
    /// correct `PositionAnimation::Custom` variant.
    #[test]
    fn derive_animator_config_maps_easing_to_custom() {
        // Default config uses EaseOutExpo
        let cfg = make_enabled_config(250);
        let result = derive_animator_config(&cfg, Duration::ZERO);
        match &result.position_animation {
            PositionAnimation::Custom(style) => {
                assert_eq!(*style, EasingStyle::EaseOutExpo);
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    /// Verify that a non-default easing value flows through correctly.
    #[test]
    fn derive_animator_config_maps_linear_easing() {
        let mut cfg = make_enabled_config(250);
        cfg.animation.easing = ConfigEasing::Linear;
        let result = derive_animator_config(&cfg, Duration::ZERO);
        match &result.position_animation {
            PositionAnimation::Custom(style) => {
                assert_eq!(*style, EasingStyle::Linear);
            }
            other => panic!("expected Custom(Linear), got {other:?}"),
        }
    }

    /// Verify all 31 ConfigEasing variants map correctly.
    #[test]
    fn config_easing_to_style_all_variants() {
        for (config_easing, expected_style) in [
            (ConfigEasing::Linear, EasingStyle::Linear),
            (ConfigEasing::EaseInSine, EasingStyle::EaseInSine),
            (ConfigEasing::EaseOutSine, EasingStyle::EaseOutSine),
            (ConfigEasing::EaseInOutSine, EasingStyle::EaseInOutSine),
            (ConfigEasing::EaseInQuad, EasingStyle::EaseInQuad),
            (ConfigEasing::EaseOutQuad, EasingStyle::EaseOutQuad),
            (ConfigEasing::EaseInOutQuad, EasingStyle::EaseInOutQuad),
            (ConfigEasing::EaseInCubic, EasingStyle::EaseInCubic),
            (ConfigEasing::EaseOutCubic, EasingStyle::EaseOutCubic),
            (ConfigEasing::EaseInOutCubic, EasingStyle::EaseInOutCubic),
            (ConfigEasing::EaseInQuart, EasingStyle::EaseInQuart),
            (ConfigEasing::EaseOutQuart, EasingStyle::EaseOutQuart),
            (ConfigEasing::EaseInOutQuart, EasingStyle::EaseInOutQuart),
            (ConfigEasing::EaseInQuint, EasingStyle::EaseInQuint),
            (ConfigEasing::EaseOutQuint, EasingStyle::EaseOutQuint),
            (ConfigEasing::EaseInOutQuint, EasingStyle::EaseInOutQuint),
            (ConfigEasing::EaseInExpo, EasingStyle::EaseInExpo),
            (ConfigEasing::EaseOutExpo, EasingStyle::EaseOutExpo),
            (ConfigEasing::EaseInOutExpo, EasingStyle::EaseInOutExpo),
            (ConfigEasing::EaseInCirc, EasingStyle::EaseInCirc),
            (ConfigEasing::EaseOutCirc, EasingStyle::EaseOutCirc),
            (ConfigEasing::EaseInOutCirc, EasingStyle::EaseInOutCirc),
            (ConfigEasing::EaseInBack, EasingStyle::EaseInBack),
            (ConfigEasing::EaseOutBack, EasingStyle::EaseOutBack),
            (ConfigEasing::EaseInOutBack, EasingStyle::EaseInOutBack),
            (ConfigEasing::EaseInElastic, EasingStyle::EaseInElastic),
            (ConfigEasing::EaseOutElastic, EasingStyle::EaseOutElastic),
            (
                ConfigEasing::EaseInOutElastic,
                EasingStyle::EaseInOutElastic,
            ),
            (ConfigEasing::EaseInBounce, EasingStyle::EaseInBounce),
            (ConfigEasing::EaseOutBounce, EasingStyle::EaseOutBounce),
            (ConfigEasing::EaseInOutBounce, EasingStyle::EaseInOutBounce),
        ] {
            assert_eq!(
                config_easing_to_style(&config_easing),
                expected_style,
                "mismatch for {config_easing:?}"
            );
        }
    }
}
