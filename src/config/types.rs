//! Configuration type definitions for ScrollingTilingManager.
//!
//! # CODE is the single source of truth
//!
//! Every config struct carries a `#[serde(default)]` container attribute, so
//! any field missing from the user's `stm.toml` is filled from the struct's
//! [`Default`] implementation. These `Default` impls — defined inline below —
//! are the **canonical default values**. There is no shipped-defaults TOML
//! merged at runtime.
//!
//! As a result, a user's `stm.toml` may be **partial, empty, or even
//! nested-partial** (e.g. a `[padding]` block with only `window_gap` set).
//! Serde creates a `Default` instance first, then overrides only the fields
//! present in the TOML. This is simpler and more robust than the previous
//! two-layer TOML merge, which silently fell back to stale compiled-in values
//! when the shipped file was absent during development.
//!
//! # `default-config.toml` is an example
//!
//! [`default-config.toml`](../../../../default-config.toml) in the project root
//! is a hand-written, fully-commented **example** file. It is copied verbatim
//! into a user's config directory by `stm config init` (see
//! [`lifecycle::init_config_dir`](crate::config::lifecycle::init_config_dir)).
//! It is **not** read at runtime. It must stay in sync with the compiled
//! defaults; the `default_config_toml_matches_compiled_defaults` test enforces
//! this automatically.
//!
//! # Config File Split
//!
//! Configuration is split across two TOML files:
//!
//! - **`stm.toml`** ([`StmConfig`]) — Application settings (padding,
//!   animation, etc.). Loaded from `%USERPROFILE%\.config\stm\stm.toml`
//!   (see [`config::dirs`](crate::config::dirs) for the full resolution chain).
//!
//! - **`stm-rules.toml`** ([`WindowRulesConfig`]) — Window classification rules
//!   and default action. Loaded from `%USERPROFILE%\.config\stm\stm-rules.toml`.
//!
//! This separation allows users to edit rules frequently (adding ignore patterns
//! for new apps) without risk of corrupting their app settings, and vice-versa.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Top-level application configuration structure.
///
/// Loaded from `%USERPROFILE%\.config\stm\stm.toml` (see [`config::dirs`](crate::config::dirs)
/// for the full resolution chain including `--config` flag and `STM_CONFIG_DIR` env var
/// overrides). The struct carries `#[serde(default)]`, so the file may be partial or even empty —
/// serde fills missing fields from the [`Default`] impl.
///
/// The canonical default values live in the `Default` impl below. The `default-config.toml` file is
/// a hand-written **example** copied to users by `stm config init` — it is not read at runtime.
///
/// This struct contains **application settings only** — padding,
/// animation, etc. Window classification rules live in a separate file
/// ([`WindowRulesConfig`]) to allow independent editing.
///
/// # Column Sizing
///
/// The primary column sizing mode is [`columns_per_screen`](StmConfig::columns_per_screen):
/// the user specifies how many columns fit on one monitor, and the daemon computes
/// the actual pixel width at runtime from the monitor resolution and [`window_gap`](Padding::window_gap).
///
/// Power users can override this by setting [`column_width`](StmConfig::column_width)
/// to a fixed pixel value. When `column_width` is `Some`, it takes priority over
/// `columns_per_screen` — the auto-computation is skipped entirely.
///
/// # Example
///
/// ```toml
/// columns_per_screen = 4
/// min_column_width_px = 640
///
/// [padding]
/// window_gap = 16
/// up = 16
/// down = 16
///
/// [animation]
/// enabled = true
/// duration_ms = 240
/// easing = "ease-out-expo"
/// ```
///
/// See `docs/spec/04-config-and-persistence.md` for the full schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct StmConfig {
    /// Number of columns that fit side-by-side on one monitor screen.
    ///
    /// The daemon computes the actual pixel width at startup:
    /// `base_content_width = (monitor_width - (N+1) * window_gap) / N`
    /// where `N = columns_per_screen`.
    ///
    /// If [`column_width`](StmConfig::column_width) is also set, that value
    /// takes priority and this field is ignored.
    pub columns_per_screen: u32,

    /// Fixed column width in pixels — power-user override.
    ///
    /// When `Some`, this overrides the auto-computation from [`columns_per_screen`](StmConfig::columns_per_screen).
    /// When `None`, the daemon computes the width from the monitor resolution, `columns_per_screen`,
    /// and `window_gap`.
    ///
    /// This field is **optional** in TOML — most users should rely on `columns_per_screen`.
    /// A missing `Option` field deserializes to `None` automatically.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column_width: Option<u32>,

    /// Minimum column width in pixels. Columns cannot be resized below this.
    pub min_column_width_px: u32,

    /// Padding settings.
    pub padding: Padding,

    /// Animation settings.
    pub animation: AnimationConfig,

    /// Behavior when a minimized tiling window is restored.
    pub minimize_restore: MinimizeRestore,
}

fn default_window_action() -> WindowAction {
    WindowAction::Tile
}

impl Default for StmConfig {
    fn default() -> Self {
        Self {
            columns_per_screen: 4,
            column_width: None,
            min_column_width_px: 640,
            padding: Padding::default(),
            animation: AnimationConfig::default(),
            minimize_restore: MinimizeRestore::default(),
        }
    }
}

/// Gap and margin configuration in pixels.
///
/// Gap is applied during the projection step (see [`projection`](crate::layout::projection)),
/// not stored inside window structs. This means [`ActualEntry`](crate::layout::ActualEntry) rects
/// are the **final HWND rects** — they can be passed directly to `SetWindowPos`.
///
/// # Uniform Gap Model
///
/// `window_gap` creates **uniform spacing** everywhere:
///
/// ```text
/// Edge | window_gap | [Window 1] | window_gap | [Window 2] | window_gap | Edge
/// ```
///
/// - `window_gap`: uniform gap between all elements (windows, screen edges)
/// - `up`: top screen margin (reserved space above tiling area)
/// - `down`: bottom screen margin (reserved space below tiling area, e.g., taskbar)
///
/// The slot-based canvas model ensures that expanding a column consumes the
/// inter-column gap so the layout always fills the screen.
///
/// ```text
/// ┌─────────────────── monitor work area ────────────────────┐
/// │ ↑ padding.up                                             │
/// │ ↑ window_gap                                             │
/// │ ┌──── window (HWND) ────┐   ┌──── window (HWND) ────┐    │
/// │ │                       │   │                       │    │
/// │ └───────────────────────┘   └───────────────────────┘    │
/// │ ↓ window_gap                                             │
/// │ ↓ padding.down                                           │
/// └──────────────────────────────────────────────────────────┘
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Padding {
    /// Uniform gap between all elements (windows and screen edges), in pixels.
    ///
    /// This single value controls horizontal (inter-column), vertical (inter-row),
    /// and edge-to-window spacing. Setting `window_gap = 16` means every gap
    /// everywhere is 16 pixels.
    pub window_gap: i32,
    /// Top screen margin — reserved space above the tiling area.
    pub up: i32,
    /// Bottom screen margin — reserved space below the tiling area (e.g., taskbar).
    pub down: i32,
}

impl Default for Padding {
    fn default() -> Self {
        Self {
            window_gap: 16,
            up: 16,
            down: 16,
        }
    }
}

impl Padding {
    /// Validate that all padding values are non-negative.
    ///
    /// Returns `Err` with a descriptive message for the first invalid field.
    pub fn validate(&self) -> Result<(), String> {
        if self.window_gap < 0 {
            return Err(format!(
                "padding.window_gap must be non-negative, got {}",
                self.window_gap
            ));
        }
        if self.up < 0 {
            return Err(format!("padding.up must be non-negative, got {}", self.up));
        }
        if self.down < 0 {
            return Err(format!(
                "padding.down must be non-negative, got {}",
                self.down
            ));
        }
        Ok(())
    }
}

impl StmConfig {
    /// Validate config values that serde cannot enforce.
    ///
    /// Call this after deserializing a config file to catch
    /// semantically invalid values like negative padding or
    /// min_column_width_px exceeding column_width.
    ///
    /// When `column_width` is `None`, the comparison against
    /// `min_column_width_px` is deferred to runtime (after the
    /// daemon computes the actual width from `columns_per_screen`).
    pub fn validate(&self) -> Result<(), String> {
        self.padding.validate()?;
        if self.columns_per_screen == 0 {
            return Err("columns_per_screen must be at least 1, got 0".into());
        }
        if self.min_column_width_px == 0 {
            return Err("min_column_width_px must be positive, got 0".into());
        }
        if let Some(cw) = self.column_width
            && self.min_column_width_px > cw
        {
            return Err(format!(
                "min_column_width_px ({}) must not exceed column_width ({})",
                self.min_column_width_px, cw
            ));
        }
        Ok(())
    }
}

/// A single window classification rule.
///
/// Rules are evaluated **top-to-bottom, first match wins** against new windows.
/// If no rule matches, [`WindowRulesConfig::default_action`] is used.
///
/// The `match` field uses `#[serde(rename = "match")]` so the TOML key is
/// `match` while the Rust field is `match_` (avoiding the reserved word).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WindowRule {
    /// Criteria to match against a window.
    #[serde(rename = "match")]
    pub match_: MatchRule,
    /// Action to take when matched.
    pub action: WindowAction,
    /// Optional initial column width in pixels.
    ///
    /// When set, a window matching this rule is created at this width instead
    /// of the default `column_width`. The value must fall within the engine's
    /// current bounds (`[min_column_width_px, abs_max_width]`) at apply time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_width_px: Option<u32>,
    /// If true, this rule overrides per-app learned state.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub override_persist: bool,
}

/// Match criteria for a window rule.
///
/// All fields are optional — a rule matches if **all specified fields** match
/// (AND logic). Unspecified (`None`) fields are ignored entirely.
///
/// Each field supports a different matching mode:
///
/// - **Exact match** (`exe`, `title`, `class`, `process_path`) — the candidate
///   string must equal the rule value exactly.
///
/// - **Substring match** (`title_contains`) — the rule value must appear as a
///   contiguous substring of the candidate string.
///
/// - **Regex match** (`exe_regex`, `title_regex`, `class_regex`,
///   `process_path_regex`) — the rule value is compiled as a Rust regex and the
///   full candidate string must match (not just a substring). Uses the `regex`
///   crate's syntax.
///
/// # Case Sensitivity
///
/// | Field               | Semantics                                         |
/// |---------------------|---------------------------------------------------|
/// | `exe`               | Case-insensitive (Windows paths are case-insensitive) |
/// | `exe_regex`         | Case-insensitive by default (Windows paths)       |
/// | `title`             | Case-sensitive                                    |
/// | `title_contains`    | Case-sensitive                                    |
/// | `title_regex`       | Case-sensitive (use `(?i)` for case-insensitive)  |
/// | `class`             | Case-sensitive                                    |
/// | `class_regex`       | Case-sensitive (use `(?i)` for case-insensitive)  |
/// | `process_path`      | Case-insensitive (Windows paths)                  |
/// | `process_path_regex` | Case-insensitive by default (Windows paths)      |
///
/// **Why are `exe` and `process_path` case-insensitive?**
///
/// On Windows, the filesystem is case-insensitive — `chrome.exe` and `Chrome.exe`
/// refer to the same file. Window class names (`class`) and titles (`title`) are
/// case-sensitive strings that applications control, so they are matched exactly.
/// Regex fields use the `(?i)` / `(?-i)` inline flags if the user needs different
/// behavior.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct MatchRule {
    /// Exact executable name (e.g., `"code.exe"`). Case-insensitive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,

    /// Exact window title. Case-sensitive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Case-sensitive substring match on title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_contains: Option<String>,

    /// Full regex match on title. Case-sensitive.
    /// Use `(?i)` inline flag for case-insensitive matching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_regex: Option<String>,

    /// Exact Win32 window class name. Case-sensitive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,

    /// Exact full executable path. Case-insensitive (Windows filesystem).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_path: Option<String>,

    /// Regex on executable name. Case-insensitive by default.
    /// Use `(?-i)` inline flag to opt into case-sensitive matching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe_regex: Option<String>,

    /// Regex on Win32 window class name. Case-sensitive.
    /// Use `(?i)` inline flag for case-insensitive matching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class_regex: Option<String>,

    /// Regex on full executable path. Case-insensitive by default.
    /// Use `(?-i)` inline flag to opt into case-sensitive matching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_path_regex: Option<String>,
}

/// Action to apply when a window rule matches.
///
/// - `Tile` — managed by [`LayoutEngine`](crate::layout::LayoutEngine)
/// - `Float` — free-floating, user-positioned, not tiled
/// - `Ignore` — excluded from tiling entirely (e.g., fullscreen apps)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowAction {
    /// Managed by [`LayoutEngine`](crate::layout::LayoutEngine).
    Tile,
    /// Free-floating, user-positioned, not tiled.
    Float,
    /// Excluded from tiling entirely (e.g., fullscreen apps).
    Ignore,
}

/// Window classification configuration, loaded from `stm-rules.toml`.
///
/// This is the user-facing window rules file. Rules are evaluated top-to-bottom,
/// first match wins. If no rule matches, `default_action` is used.
///
/// Loaded from `%USERPROFILE%\.config\stm\stm-rules.toml` (see [`config::dirs`](crate::config::dirs)
/// for overrides). If the file doesn't exist,
/// defaults to an empty rule list with `default_action: tile`.
///
/// # Example
///
/// ```toml
/// default_action = "tile"
///
/// [[rules]]
/// match = { exe = "explorer.exe", title_contains = "Open" }
/// action = "ignore"
///
/// [[rules]]
/// match = { class = "Chrome_WidgetWin_1" }
/// action = "tile"
/// initial_width_px = 960
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WindowRulesConfig {
    /// Default action for windows not matching any rule.
    pub default_action: WindowAction,

    /// Window classification rules (first match wins).
    #[serde(default)]
    pub rules: Vec<WindowRule>,
}

impl Default for WindowRulesConfig {
    /// Returns a default config with `tile` as the default action and no rules.
    fn default() -> Self {
        Self {
            default_action: default_window_action(),
            rules: Vec::new(),
        }
    }
}

/// Named easing curves available in user configuration.
///
/// Each variant corresponds to an easing function implemented in
/// [`crate::animation::easing::EasingStyle`]. The variant names use
/// CSS-like kebab-case when serialized to TOML (e.g. `EaseOutExpo` →
/// `"ease-out-expo"`).
///
/// The full set mirrors the 31 named curves in the animation engine.
/// `CubicBezier` is excluded because it requires four `f64` parameters
/// that cannot be expressed as a simple serde enum variant.
///
/// # Design Decision
///
/// This enum lives in the `config/` layer (not `animation/`) to honour
/// the module dependency hierarchy: `config/` may only import from
/// `common/`, never from `animation/`. The conversion from
/// `ConfigEasing` → `EasingStyle` is performed in the `daemon/` layer
/// (see [`crate::daemon::config_derive`]).
///
/// # Example
///
/// ```toml
/// [animation]
/// easing = "ease-in-out-cubic"
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigEasing {
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
    #[default]
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
}

/// Animation configuration for layout transitions.
///
/// When disabled, all window position changes are applied instantly
/// without animation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AnimationConfig {
    /// Whether layout transitions are animated.
    pub enabled: bool,
    /// Animation duration in milliseconds.
    pub duration_ms: u32,
    /// Easing curve applied to window position channels (x, y).
    ///
    /// See [`ConfigEasing`] for the full list of supported curves.
    /// Defaults to [`ConfigEasing::EaseOutExpo`].
    pub easing: ConfigEasing,
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            duration_ms: 240,
            easing: ConfigEasing::EaseOutExpo,
        }
    }
}

/// Strategy for restoring minimized tiling windows.
///
/// - `OriginalSlot` — put the window back where it was before minimize
/// - `RightOfFocused` — insert as a new column to the right of focused
/// - `AppendRight` — append as the rightmost column on the canvas
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MinimizeRestore {
    /// Strategy for placing restored windows.
    pub strategy: MinimizeRestoreStrategy,
}

impl Default for MinimizeRestore {
    fn default() -> Self {
        Self {
            strategy: MinimizeRestoreStrategy::OriginalSlot,
        }
    }
}

/// Available minimize restore strategies.
///
/// See [`MinimizeRestore`] for the semantics of each variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MinimizeRestoreStrategy {
    /// Put the window back where it was before minimize.
    OriginalSlot,
    /// Insert as a new column to the right of focused.
    RightOfFocused,
    /// Append as the rightmost column on the canvas.
    AppendRight,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_roundtrips_toml() {
        let config = StmConfig::default();
        let toml_str = toml::to_string(&config).expect("serialize");
        let parsed: StmConfig = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(parsed.columns_per_screen, 4);
        assert_eq!(parsed.column_width, None);
        assert_eq!(parsed.min_column_width_px, 640);
        assert_eq!(parsed.padding.window_gap, 16);
        assert_eq!(parsed.padding.up, 16);
        assert_eq!(parsed.padding.down, 16);
        assert_eq!(parsed.animation.duration_ms, 240);
        assert_eq!(parsed.animation.easing, ConfigEasing::EaseOutExpo);
    }

    #[test]
    fn config_from_toml_with_settings() {
        // Full TOML exercises every field end-to-end. (Serde defaults now exist,
        // but a complete file is the clearest way to verify full-population.)
        let toml_str = r#"
columns_per_screen = 3
column_width = 1200
min_column_width_px = 400

[padding]
window_gap = 8
up = 10
down = 40

[animation]
enabled = false
duration_ms = 180
easing = "ease-out-expo"

[minimize_restore]
strategy = "original_slot"
"#;
        let config: StmConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.columns_per_screen, 3);
        assert_eq!(config.column_width, Some(1200));
        assert_eq!(config.padding.window_gap, 8);
        assert_eq!(config.padding.up, 10);
        assert_eq!(config.padding.down, 40);
        assert!(!config.animation.enabled);
    }

    /// Positive: empty TOML deserializes to compiled defaults.
    ///
    /// Because `StmConfig` carries `#[serde(default)]` at the container level,
    /// an empty file
    /// (or a file with no recognized keys) yields a fully-defaulted [`StmConfig`].
    /// This is the core of the "code is the source of truth" model: there is no
    /// separate shipped-defaults file to merge.
    #[test]
    fn config_from_empty_toml_uses_defaults() {
        let config: StmConfig = toml::from_str("").expect("empty TOML should use defaults");
        assert_eq!(config, StmConfig::default());
    }

    /// Positive: partial TOML (a single field) fills the rest from defaults.
    #[test]
    fn config_from_partial_toml_uses_defaults() {
        let config: StmConfig =
            toml::from_str("columns_per_screen = 3\n").expect("partial TOML should parse");
        assert_eq!(config.columns_per_screen, 3);
        // Everything else comes from defaults.
        assert_eq!(config.min_column_width_px, 640);
        assert_eq!(config.padding.window_gap, 16);
        assert_eq!(config.animation.duration_ms, 240);
    }

    /// Positive: a nested-partial `[padding]` block fills missing sub-fields.
    ///
    /// Only `window_gap` is set; `up` and `down` must come from their serde
    /// defaults. This verifies the per-field defaults reach inside nested structs.
    #[test]
    fn config_from_nested_partial_toml_uses_defaults() {
        let toml_str = "[padding]\nwindow_gap = 20\n";
        let config: StmConfig = toml::from_str(toml_str).expect("nested-partial should parse");
        assert_eq!(config.padding.window_gap, 20);
        assert_eq!(config.padding.up, 16);
        assert_eq!(config.padding.down, 16);
        // Top-level defaults still apply.
        assert_eq!(config.columns_per_screen, 4);
    }

    /// Positive: a nested-partial `[animation]` block fills missing fields.
    ///
    /// Only `duration_ms` is set; `enabled` and `easing` must come from serde
    /// defaults. This verifies per-field defaults inside `AnimationConfig`.
    #[test]
    fn config_from_nested_partial_animation_uses_defaults() {
        let toml_str = "[animation]\nduration_ms = 500\n";
        let config: StmConfig =
            toml::from_str(toml_str).expect("nested-partial animation should parse");
        assert_eq!(config.animation.duration_ms, 500);
        // Missing fields should be their compiled defaults.
        assert!(
            config.animation.enabled,
            "animation.enabled should default to true"
        );
        assert_eq!(
            config.animation.easing,
            ConfigEasing::EaseOutExpo,
            "animation.easing should default to ease-out-expo"
        );
    }

    /// Sync guard: the hand-written `default-config.toml` example must parse to
    /// exactly the compiled [`StmConfig::default()`].
    ///
    /// This enforces the AGENTS.md rule that `default-config.toml` stays in sync
    /// with the compiled `Default` impl. If you change a
    /// default in code, update the example file too (or this test fails).
    #[test]
    fn default_config_toml_matches_compiled_defaults() {
        let example: &str = include_str!("../../default-config.toml");
        let parsed: StmConfig =
            toml::from_str(example).expect("default-config.toml must parse as StmConfig");
        assert_eq!(
            parsed,
            StmConfig::default(),
            "default-config.toml drifted from compiled defaults; \
             update one to match the other"
        );
    }

    // --- Integration: Full field preservation through round-trip ---

    #[test]
    fn config_roundtrip_preserves_all_fields() {
        // Positive: every field survives TOML → StmConfig → TOML
        let config = StmConfig {
            columns_per_screen: 3,
            column_width: Some(1200),
            min_column_width_px: 400,
            padding: Padding {
                window_gap: 6,
                up: 10,
                down: 40,
            },
            animation: AnimationConfig {
                enabled: false,
                duration_ms: 250,
                easing: ConfigEasing::EaseInOutCubic,
            },
            minimize_restore: MinimizeRestore {
                strategy: MinimizeRestoreStrategy::AppendRight,
            },
        };

        let toml_str = toml::to_string(&config).expect("serialize all fields");
        let parsed: StmConfig = toml::from_str(&toml_str).expect("deserialize all fields");

        assert_eq!(parsed.columns_per_screen, 3);
        assert_eq!(parsed.column_width, Some(1200));
        assert_eq!(parsed.min_column_width_px, 400);
        assert_eq!(parsed.padding.window_gap, 6);
        assert_eq!(parsed.padding.up, 10);
        assert_eq!(parsed.padding.down, 40);
        assert!(!parsed.animation.enabled);
        assert_eq!(parsed.animation.duration_ms, 250);
        assert_eq!(parsed.animation.easing, ConfigEasing::EaseInOutCubic);
        assert_eq!(
            parsed.minimize_restore.strategy,
            MinimizeRestoreStrategy::AppendRight
        );
    }

    #[test]
    fn config_validate_rejects_negative_window_padding() {
        let config = StmConfig {
            padding: Padding {
                window_gap: -1,
                ..Padding::default()
            },
            ..StmConfig::default()
        };
        assert!(config.validate().is_err());
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("padding.window_gap")
        );
    }

    #[test]
    fn config_validate_rejects_negative_up_padding() {
        let config = StmConfig {
            padding: Padding {
                up: -5,
                ..Padding::default()
            },
            ..StmConfig::default()
        };
        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("padding.up"));
    }

    #[test]
    fn config_validate_rejects_negative_down_padding() {
        let config = StmConfig {
            padding: Padding {
                down: -10,
                ..Padding::default()
            },
            ..StmConfig::default()
        };
        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("padding.down"));
    }

    #[test]
    fn config_validate_accepts_zero_padding() {
        let config = StmConfig {
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            ..StmConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_validate_accepts_default_config() {
        assert!(StmConfig::default().validate().is_ok());
    }

    #[test]
    fn config_validate_rejects_zero_min_column_width() {
        let config = StmConfig {
            min_column_width_px: 0,
            ..StmConfig::default()
        };
        assert!(config.validate().is_err());
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("min_column_width_px")
        );
    }

    #[test]
    fn config_validate_rejects_min_exceeding_column_width() {
        let config = StmConfig {
            min_column_width_px: 1000,
            column_width: Some(960),
            ..StmConfig::default()
        };
        assert!(config.validate().is_err());
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("min_column_width_px")
        );
    }

    #[test]
    fn config_validate_accepts_min_equal_to_column_width() {
        // When column_width is explicitly set, min must be <= it (equality is ok).
        let config = StmConfig {
            min_column_width_px: 960,
            column_width: Some(960),
            ..StmConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_validate_rejects_zero_columns_per_screen() {
        let config = StmConfig {
            columns_per_screen: 0,
            ..StmConfig::default()
        };
        assert!(config.validate().is_err());
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("columns_per_screen")
        );
    }

    #[test]
    fn config_validate_accepts_column_width_none() {
        // When column_width is None (auto-compute mode), min check is deferred.
        let config = StmConfig {
            column_width: None,
            min_column_width_px: 9999,
            ..StmConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    // --- WindowRulesConfig tests ---

    #[test]
    fn window_rules_config_default_roundtrips() {
        let config = WindowRulesConfig::default();
        let toml_str = toml::to_string(&config).expect("serialize");
        let parsed: WindowRulesConfig = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(parsed.default_action, WindowAction::Tile);
        assert!(parsed.rules.is_empty());
    }

    #[test]
    fn window_rules_config_from_toml() {
        let toml_str = r#"
default_action = "float"

[[rules]]
match = { exe = "explorer.exe", title_contains = "Open" }
action = "ignore"

[[rules]]
match = { class = "Chrome_WidgetWin_1" }
action = "tile"
initial_width_px = 960
"#;
        let config: WindowRulesConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.default_action, WindowAction::Float);
        assert_eq!(config.rules.len(), 2);
        assert_eq!(config.rules[0].action, WindowAction::Ignore);
        assert_eq!(config.rules[1].initial_width_px, Some(960));
    }

    /// Positive: TOML with only `default_action` parses correctly (rules defaults to empty).
    #[test]
    fn window_rules_config_minimal_toml() {
        let toml_str = "default_action = \"tile\"\n";
        let config: WindowRulesConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.default_action, WindowAction::Tile);
        assert!(config.rules.is_empty());
    }

    /// Negative: empty TOML fails because `default_action` is required.
    #[test]
    fn window_rules_config_empty_toml_is_rejected() {
        let toml_str = "";
        let result = toml::from_str::<WindowRulesConfig>(toml_str);
        assert!(
            result.is_err(),
            "empty TOML should fail without serde defaults on default_action"
        );
    }

    #[test]
    fn window_rules_config_with_regex_fields_roundtrips() {
        let config = WindowRulesConfig {
            default_action: WindowAction::Ignore,
            rules: vec![WindowRule {
                match_: MatchRule {
                    exe: None,
                    title: None,
                    title_contains: None,
                    title_regex: Some("^Settings".into()),
                    class: Some("SettingsApp".into()),
                    class_regex: Some("Settings.*".into()),
                    process_path: None,
                    exe_regex: Some("chrome\\.exe".into()),
                    process_path_regex: Some(".*\\\\Google\\\\Chrome\\\\.*".into()),
                },
                action: WindowAction::Float,
                initial_width_px: None,
                override_persist: false,
            }],
        };

        let toml_str = toml::to_string(&config).expect("serialize");
        let parsed: WindowRulesConfig = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(parsed.default_action, WindowAction::Ignore);
        assert_eq!(parsed.rules.len(), 1);
        assert_eq!(
            parsed.rules[0].match_.exe_regex,
            Some("chrome\\.exe".into())
        );
        assert_eq!(
            parsed.rules[0].match_.class_regex,
            Some("Settings.*".into())
        );
        assert_eq!(
            parsed.rules[0].match_.process_path_regex,
            Some(".*\\\\Google\\\\Chrome\\\\.*".into())
        );
    }

    #[test]
    fn window_rules_config_all_window_actions_parse() {
        for action_str in ["tile", "float", "ignore"] {
            let toml_str = format!("default_action = \"{action_str}\"");
            let config: WindowRulesConfig = toml::from_str(&toml_str).expect(&toml_str);
            assert_eq!(
                config.default_action,
                match action_str {
                    "tile" => WindowAction::Tile,
                    "float" => WindowAction::Float,
                    "ignore" => WindowAction::Ignore,
                    _ => unreachable!(),
                },
                "action mismatch for {action_str}"
            );
        }
    }

    #[test]
    fn window_rules_config_invalid_enum_rejects() {
        let toml_str = r#"
default_action = "foobar"
"#;
        let result = toml::from_str::<WindowRulesConfig>(toml_str);
        assert!(result.is_err(), "invalid enum value should reject");
    }

    // --- ConfigEasing-specific tests ---

    /// Positive: all 31 ConfigEasing variants round-trip through TOML
    /// serialize → deserialize.
    ///
    /// This validates that `#[serde(rename_all = "kebab-case")]` produces
    /// the expected kebab-case strings and that serde can parse them back
    /// to the correct variant.
    ///
    /// Standalone `toml::to_string(&enum_variant)` is unsupported by the
    /// `toml` crate, so we embed each variant inside a `AnimationConfig`
    /// wrapper and verify the serialized TOML contains the expected
    /// kebab-case string.
    #[test]
    fn config_easing_roundtrips_all_variants() {
        for (variant, expected_kebab) in [
            (ConfigEasing::Linear, "linear"),
            (ConfigEasing::EaseInSine, "ease-in-sine"),
            (ConfigEasing::EaseOutSine, "ease-out-sine"),
            (ConfigEasing::EaseInOutSine, "ease-in-out-sine"),
            (ConfigEasing::EaseInQuad, "ease-in-quad"),
            (ConfigEasing::EaseOutQuad, "ease-out-quad"),
            (ConfigEasing::EaseInOutQuad, "ease-in-out-quad"),
            (ConfigEasing::EaseInCubic, "ease-in-cubic"),
            (ConfigEasing::EaseOutCubic, "ease-out-cubic"),
            (ConfigEasing::EaseInOutCubic, "ease-in-out-cubic"),
            (ConfigEasing::EaseInQuart, "ease-in-quart"),
            (ConfigEasing::EaseOutQuart, "ease-out-quart"),
            (ConfigEasing::EaseInOutQuart, "ease-in-out-quart"),
            (ConfigEasing::EaseInQuint, "ease-in-quint"),
            (ConfigEasing::EaseOutQuint, "ease-out-quint"),
            (ConfigEasing::EaseInOutQuint, "ease-in-out-quint"),
            (ConfigEasing::EaseInExpo, "ease-in-expo"),
            (ConfigEasing::EaseOutExpo, "ease-out-expo"),
            (ConfigEasing::EaseInOutExpo, "ease-in-out-expo"),
            (ConfigEasing::EaseInCirc, "ease-in-circ"),
            (ConfigEasing::EaseOutCirc, "ease-out-circ"),
            (ConfigEasing::EaseInOutCirc, "ease-in-out-circ"),
            (ConfigEasing::EaseInBack, "ease-in-back"),
            (ConfigEasing::EaseOutBack, "ease-out-back"),
            (ConfigEasing::EaseInOutBack, "ease-in-out-back"),
            (ConfigEasing::EaseInElastic, "ease-in-elastic"),
            (ConfigEasing::EaseOutElastic, "ease-out-elastic"),
            (ConfigEasing::EaseInOutElastic, "ease-in-out-elastic"),
            (ConfigEasing::EaseInBounce, "ease-in-bounce"),
            (ConfigEasing::EaseOutBounce, "ease-out-bounce"),
            (ConfigEasing::EaseInOutBounce, "ease-in-out-bounce"),
        ] {
            // Serialize inside an AnimationConfig wrapper
            let wrapper = AnimationConfig {
                easing: variant,
                ..AnimationConfig::default()
            };
            let toml_str = toml::to_string(&wrapper).expect(&format!("serialize {expected_kebab}"));
            assert!(
                toml_str.contains(&format!("easing = \"{expected_kebab}\"")),
                "serialization should contain 'easing = \"{expected_kebab}\"', got:\n{toml_str}"
            );

            // Verify deserialization produces the original variant
            let parsed: AnimationConfig =
                toml::from_str(&toml_str).expect(&format!("deserialize {expected_kebab}"));
            assert_eq!(
                parsed.easing, variant,
                "round-trip mismatch for {expected_kebab}"
            );
        }
    }

    /// Negative: an unknown easing string is rejected by serde.
    ///
    /// This prevents silent misconfiguration — a typo like `"ease-out-exp"` should
    /// fail fast at parse time, not silently fall back to the default.
    #[test]
    fn config_easing_invalid_string_is_rejected() {
        let invalid_values = [
            "ease-out-exp",  // typo: missing 'o'
            "linear ",       // trailing whitespace
            "Linear",        // PascalCase — kebab-case only
            "EASE-OUT-EXPO", // uppercase
            "ease_out_expo", // snake_case
            "foobar",        // completely unknown
        ];
        for bad in &invalid_values {
            let result = toml::from_str::<ConfigEasing>(bad);
            assert!(
                result.is_err(),
                "expected rejection for invalid easing value: {bad:?}"
            );
        }
    }

    #[test]
    fn match_rule_with_new_regex_fields_serializes_correctly() {
        let rule = MatchRule {
            exe_regex: Some("msedge\\.exe".into()),
            class_regex: Some("Edge.*".into()),
            process_path_regex: Some(".*\\\\Microsoft\\\\Edge.*".into()),
            ..Default::default()
        };
        let toml_str = toml::to_string(&rule).expect("serialize");
        // Verify round-trip: deserialize back and check values match
        let parsed: MatchRule = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(parsed.exe_regex, Some("msedge\\.exe".into()));
        assert_eq!(parsed.class_regex, Some("Edge.*".into()));
        assert_eq!(
            parsed.process_path_regex,
            Some(".*\\\\Microsoft\\\\Edge.*".into())
        );
    }
}
