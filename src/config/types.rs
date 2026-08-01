//! Configuration type definitions for FlowWM.
//!
//! # CODE is the single source of truth
//!
//! Every config struct carries a `#[serde(default)]` container attribute, so
//! any field missing from the user's `flow.toml` is filled from the struct's
//! [`Default`] implementation. These `Default` impls — defined inline below —
//! are the **canonical default values**. There is no shipped-defaults TOML
//! merged at runtime.
//!
//! As a result, a user's `flow.toml` may be **partial, empty, or even
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
//! into a user's config directory by `flow config init` (see
//! [`lifecycle::init_config_dir`](crate::config::lifecycle::init_config_dir)).
//! It is **not** read at runtime. It must stay in sync with the compiled
//! defaults; the `default_config_toml_matches_compiled_defaults` test enforces
//! this automatically.
//!
//! # Config File Split
//!
//! Configuration is split across two TOML files:
//!
//! - **`flow.toml`** ([`FlowConfig`]) — Application settings (padding,
//!   animation, etc.). Loaded from `%USERPROFILE%\.config\flow\flow.toml`
//!   (see [`config::dirs`](crate::config::dirs) for the full resolution chain).
//!
//! - **`flow-rules.toml`** ([`WindowRulesConfig`]) — Window classification rules
//!   and default action. Loaded from `%USERPROFILE%\.config\flow\flow-rules.toml`.
//!
//! This separation allows users to edit rules frequently (adding ignore patterns
//! for new apps) without risk of corrupting their app settings, and vice-versa.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::color::Color;

/// Top-level application configuration structure.
///
/// Loaded from `%USERPROFILE%\.config\flow\flow.toml` (see [`config::dirs`](crate::config::dirs)
/// for the full resolution chain including `--config` flag and `FLOW_CONFIG_DIR` env var
/// overrides). The struct carries `#[serde(default)]`, so the file may be partial or even empty —
/// serde fills missing fields from the [`Default`] impl.
///
/// The canonical default values live in the `Default` impl below. The `default-config.toml` file is
/// a hand-written **example** copied to users by `flow config init` — it is not read at runtime.
///
/// This struct contains **application settings only** — padding,
/// animation, etc. Window classification rules live in a separate file
/// ([`WindowRulesConfig`]) to allow independent editing.
///
/// # Column Sizing
///
/// The primary column sizing mode is [`columns_per_screen`](FlowConfig::columns_per_screen):
/// the user specifies how many columns fit on one monitor, and the daemon computes
/// the actual pixel width at runtime from the monitor resolution and [`window_gap`](Padding::window_gap).
///
/// Power users can override this by setting [`column_width`](FlowConfig::column_width)
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
pub struct FlowConfig {
    /// Number of columns that fit side-by-side on one monitor screen.
    ///
    /// The daemon computes the actual pixel width at startup:
    /// `base_content_width = (monitor_width - (N+1) * window_gap) / N`
    /// where `N = columns_per_screen`.
    ///
    /// If [`column_width`](FlowConfig::column_width) is also set, that value
    /// takes priority and this field is ignored.
    pub columns_per_screen: u32,

    /// Fixed column width in pixels — power-user override.
    ///
    /// When `Some`, this overrides the auto-computation from [`columns_per_screen`](FlowConfig::columns_per_screen).
    /// When `None`, the daemon computes the width from the monitor resolution, `columns_per_screen`,
    /// and `window_gap`.
    ///
    /// This field is **optional** in TOML — most users should rely on `columns_per_screen`.
    /// A missing `Option` field deserializes to `None` automatically.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column_width: Option<u32>,

    /// Minimum column width in pixels. Columns cannot be resized below this.
    pub min_column_width_px: u32,

    /// Minimum row height in pixels — the floor for any single window's
    /// allocated height inside a column.
    ///
    /// This bounds the maximum number of rows (windows) that can stack inside
    /// one column: a column cannot grow rows beyond `available_height /
    /// min_window_height_px`. It is also the lower clamp for future
    /// drag-resize / IPC continuous-height adjustment of individual rows.
    ///
    /// See (`docs/src/dev-guide/layout/mutations.md`) for the height-
    /// distribution formula and the `merge-column` / `promote` operations.
    pub min_window_height_px: u32,

    /// Padding settings.
    pub padding: Padding,

    /// Animation settings.
    pub animation: AnimationConfig,

    /// Behavior when a minimized tiling window is restored.
    pub minimize_restore: MinimizeRestore,

    /// Window border overlay configuration.
    pub borders: BorderConfig,

    /// Floating window default-size configuration.
    pub floating: FloatingConfig,

    /// Focus reconciliation configuration.
    pub focus: FocusConfig,

    /// Tile-drag configuration.
    pub drag: DragConfig,

    /// Edge-scroll configuration — shared band width and auto-repeat timings
    /// consumed by tile-drag edge-scroll and (future) edge-hover-scroll.
    pub edge_scroll: EdgeScrollConfig,

    /// Hover configuration — focus-follows-mouse and (future) edge-hover-scroll.
    pub hover: HoverConfig,

    /// Loadout save/restore configuration.
    pub loadout: LoadoutConfig,
    /// Whether `flow start` should query GitHub for a newer release and print a
    /// one-line notification prompting `flow update` when one exists.
    ///
    /// On by default. The check runs *after* the daemon is ready, is bounded by
    /// a short network timeout, and silences all errors — it never blocks or
    /// aborts startup. The explicit `flow update --check` command is unaffected
    /// by this flag. See (`docs/src/dev-guide/updater.md`).
    pub check_for_updates: bool,
}

/// Loadout save/restore configuration.
///
/// Controls the file path and staleness threshold used when saving or
/// restoring workspace loadouts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct LoadoutConfig {
    /// File name (not a full path) used by `flow loadout save` and
    /// `flow loadout restore`.
    ///
    /// The daemon resolves this relative to the user's config directory
    /// (`%USERPROFILE%\.config\flow\`). Defaults to `"loadout.json"`.
    pub default_path: String,
    /// Maximum age in seconds before a saved loadout is considered stale and
    /// rejected by `flow loadout restore`.
    ///
    /// Defaults to `60` seconds. Set to `0` to disable staleness checks.
    pub max_age_secs: u64,
}

impl Default for LoadoutConfig {
    fn default() -> Self {
        Self {
            default_path: "loadout.json".into(),
            max_age_secs: 60,
        }
    }
}

fn default_window_action() -> WindowAction {
    WindowAction::Float
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            columns_per_screen: 4,
            column_width: None,
            min_column_width_px: 640,
            min_window_height_px: 100,
            padding: Padding::default(),
            animation: AnimationConfig::default(),
            minimize_restore: MinimizeRestore::default(),
            borders: BorderConfig::default(),
            floating: FloatingConfig::default(),
            focus: FocusConfig::default(),
            drag: DragConfig::default(),
            edge_scroll: EdgeScrollConfig::default(),
            hover: HoverConfig::default(),
            loadout: LoadoutConfig::default(),
            check_for_updates: true,
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
            up: 0,
            down: 0,
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

impl FlowConfig {
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
        self.borders.validate()?;
        if self.columns_per_screen == 0 {
            return Err("columns_per_screen must be at least 1, got 0".into());
        }
        if self.min_column_width_px == 0 {
            return Err("min_column_width_px must be positive, got 0".into());
        }
        if self.min_window_height_px == 0 {
            return Err("min_window_height_px must be positive, got 0".into());
        }
        if let Some(cw) = self.column_width
            && self.min_column_width_px > cw
        {
            return Err(format!(
                "min_column_width_px ({}) must not exceed column_width ({})",
                self.min_column_width_px, cw
            ));
        }
        // Edge-scroll repeat interval: warn (not fail) when the configured value
        // sits below its effective floor. The runtime clamp via
        // `effective_repeat_interval_ms` makes the value safe regardless; the
        // warning tells the user why their value was not used as-is. The initial
        // delay needs no warning — its floor makes unsafe values silently safe.
        let effective_repeat = self.edge_scroll.effective_repeat_interval_ms(&self.animation);
        if self.edge_scroll.repeat_interval_ms < effective_repeat {
            return Err(format!(
                "edge_scroll.repeat_interval_ms ({}) is below its effective floor ({} ms, \
                 raised by the spam-guard floor or the enabled animation duration); \
                 it will be clamped at runtime — set it to at least {} to silence this warning",
                self.edge_scroll.repeat_interval_ms, effective_repeat, effective_repeat
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
/// - `Tile` — managed by [`ScrollingSpace`](crate::workspace::ScrollingSpace)
/// - `Float` — free-floating, user-positioned, not tiled
/// - `Ignore` — excluded from tiling entirely (e.g., fullscreen apps)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowAction {
    /// Managed by [`ScrollingSpace`](crate::workspace::ScrollingSpace).
    Tile,
    /// Free-floating, user-positioned, not tiled.
    Float,
    /// Excluded from tiling entirely (e.g., fullscreen apps).
    Ignore,
}

/// Window classification configuration, loaded from `flow-rules.toml`.
///
/// This is the user-facing window rules file. Rules are evaluated top-to-bottom,
/// first match wins. If no rule matches, `default_action` is used.
///
/// Loaded from `%USERPROFILE%\.config\flow\flow-rules.toml` (see [`config::dirs`](crate::config::dirs)
/// for overrides). If the file doesn't exist,
/// defaults to an empty rule list with `default_action: float`.
///
/// # Example
///
/// ```toml
/// default_action = "float"
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
    /// Returns a default config with `float` as the default action and no rules.
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

/// Window border overlay configuration.
///
/// The border crate draws a thin colored ring around each managed window as a
/// separate layered overlay window that follows the target HWND's geometry
/// (driven by `EVENT_OBJECT_LOCATIONCHANGE`). The daemon shrinks the actual
/// window rect by `(thickness - overlap)` pixels on each side so the colored
/// ring sits in the surrounding gap and overlaps the visible content by
/// `overlap` px (closing the DWM hairline gap). See
/// `docs/src/dev-guide/borders.md` for the full coordinate-space model.
///
/// # Example
///
/// ```toml
/// [borders]
/// enabled = true
/// thickness = 3
/// overlap = 1
/// focused_color = "#00AAFF"
/// unfocused_color = "#555555"
/// floating_color = "#AA00FF"
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct BorderConfig {
    /// Master switch. When `false`, no overlay windows are created and any
    /// existing overlays are detached.
    pub enabled: bool,
    /// Border ring thickness in pixels, applied uniformly on all four sides.
    ///
    /// The ring is drawn in the outer `thickness` px of each window's layout
    /// slot. The visible window content shrinks by `(thickness - overlap)` px
    /// per edge (see `overlap`), so the ring overlaps the content by `overlap`
    /// px rather than leaving a gap. (`docs/src/dev-guide/borders.md`)
    pub thickness: u32,
    /// How many pixels the border ring overlaps the window's visible content.
    ///
    /// Komorebi-style overlap. `0` keeps the ring entirely in the reserved
    /// gap (window shrinks by the full `thickness`); higher values pull the
    /// content edge outward until, at `overlap == thickness`, the content
    /// fills the whole layout slot and the ring sits fully on top of it. The
    /// default of `1` closes the 1px DWM-client-edge hairline that otherwise
    /// shows between an unfocused ring and the window content. Capped at
    /// `thickness` by [`BorderConfig::validate`]. (`docs/src/dev-guide/borders.md`)
    pub overlap: u32,
    /// Border color for the focused/active window.
    pub focused_color: Color,
    /// Border color for tiled-but-not-focused windows.
    pub unfocused_color: Color,
    /// Border color for floating windows.
    pub floating_color: Color,
}

impl Default for BorderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            thickness: 3,
            overlap: 1,
            focused_color: Color::rgb(0x00, 0xAA, 0xFF),
            unfocused_color: Color::rgb(0x55, 0x55, 0x55),
            floating_color: Color::rgb(0xAA, 0x00, 0xFF),
        }
    }
}

impl BorderConfig {
    /// Validate that field values are within sane bounds.
    ///
    /// `thickness` is capped at 50 px — anything wider is almost certainly a
    /// misconfiguration that would shrink windows into nothing. `overlap` must
    /// not exceed `thickness`, otherwise the effective inset `(thickness -
    /// overlap)` goes negative and the window would expand beyond its layout
    /// slot. Color values are parsed at TOML load time, so they are already
    /// well-formed here.
    pub fn validate(&self) -> Result<(), String> {
        const MAX_THICKNESS: u32 = 50;
        if self.thickness > MAX_THICKNESS {
            return Err(format!(
                "borders.thickness must be at most {MAX_THICKNESS} px, got {}",
                self.thickness
            ));
        }
        if self.overlap > self.thickness {
            return Err(format!(
                "borders.overlap must be at most borders.thickness ({}), got {}",
                self.thickness, self.overlap
            ));
        }
        Ok(())
    }
}

/// Focus reconciliation configuration.
///
/// `EVENT_SYSTEM_FOREGROUND` is a best-effort stream: under rapid window churn
/// (e.g. a browser tearing down tabs on close) the OS may settle the foreground
/// without emitting the final foreground event, leaving flow's internal focus
/// stranded on a stale window. To close that gap, the daemon periodically
/// reconciles its tracked focus against the authoritative `GetForegroundWindow()`
/// query on the main loop. (`docs/src/dev-guide/event-pipelines.md`)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct FocusConfig {
    /// Interval between foreground-reconciliation passes, in milliseconds.
    ///
    /// Lower values catch drift sooner at the cost of more wakeups; the
    /// per-pass work is a single `GetForegroundWindow()` read (~microseconds)
    /// that no-ops when the tracked focus already matches reality. Tunable at
    /// runtime via config hot-reload.
    pub foreground_sync_interval_ms: u64,
}

impl Default for FocusConfig {
    fn default() -> Self {
        Self {
            foreground_sync_interval_ms: 250,
        }
    }
}

/// Floating window size configuration.
///
/// Both fields are optional explicit pixel sizes. When a field is `None`, the
/// daemon falls back to a built-in policy: 60% × 80% of the monitor work area,
/// capped so ultrawide / 4K monitors don't produce absurdly large popups. The
/// fallback constants live in `src/daemon/dispatch.rs` (the sole consumer).
///
/// An explicit pixel value is always respected as-is — the cap applies only to
/// the fallback path.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct FloatingConfig {
    /// Explicit default float width in pixels. `None` → built-in fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_width: Option<i32>,
    /// Explicit default float height in pixels. `None` → built-in fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_height: Option<i32>,
}

/// Tile-drag configuration.
///
/// Controls the behavior of drag-and-drop repositioning of tiled windows.
/// When a user drags a tiled window by its title bar, a live non-committing
/// preview reflows the other windows to the prospective layout on each zone
/// change; on release the move commits and the dragged window snaps into its
/// slot. Floating windows never enter the drag state machine. See
/// (`docs/src/dev-guide/tile-drag.md`).
///
/// Only the drag-specific column-insert hit-band knobs live here; the shared
/// edge-scroll band width and auto-repeat timings live in [`EdgeScrollConfig`].
///
/// # Example
///
/// ```toml
/// [drag]
/// col_edge_ratio = 0.18
/// col_edge_max_px = 120
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DragConfig {
    /// Fraction of column width used as the column-insert hit-band floor;
    /// combined with `col_edge_max_px` to size the left/right edge bands
    /// that trigger a column-insert drop.
    pub col_edge_ratio: f32,
    /// Pixel cap on the column-insert hit band. The effective band is
    /// `min(col_edge_ratio * column_width, col_edge_max_px)`.
    pub col_edge_max_px: i32,
}

impl Default for DragConfig {
    fn default() -> Self {
        Self {
            col_edge_ratio: 0.18,
            col_edge_max_px: 120,
        }
    }
}

/// Shared edge-scroll configuration — the band width and auto-repeat timings
/// consumed by both tile-drag edge-scroll and (future) edge-hover-scroll.
///
/// Promoted out of [`DragConfig`] into its own `[edge_scroll]` block so the
/// two edge-scroll triggers (drag and hover) read one set of parameters. See
/// (`docs/src/dev-guide/tile-drag.md`).
///
/// # Example
///
/// ```toml
/// [edge_scroll]
/// band_width = 30
/// initial_delay_ms = 500
/// repeat_interval_ms = 240
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct EdgeScrollConfig {
    /// Width in pixels of the left/right edge-scroll bands. When the cursor
    /// enters this band at the screen edge, the viewport scrolls by one column
    /// (during a tile drag, committed live — not deferred to release).
    pub band_width: i32,
    /// Edge-scroll auto-repeat **initial delay** in milliseconds — the gap
    /// between the immediate scroll fired on entering the band and the first
    /// repeat. Clamped up to the effective repeat interval at runtime, so `0`
    /// cleanly means "glide at the normal cadence with no special pause."
    pub initial_delay_ms: u32,
    /// Edge-scroll auto-repeat **repeat interval** in milliseconds — the gap
    /// between successive column scrolls while the cursor is held in the band.
    /// Clamped up to the enabled animation duration and the spam-guard floor at
    /// runtime; a sub-floor value additionally logs a startup warning.
    pub repeat_interval_ms: u32,
}

/// Hardcoded floor on the edge-scroll repeat interval, applied **always** —
/// even with animation disabled — so the timer can never spin fast enough to
/// reintroduce the timer-driven "races to the edge" bug.
///
/// This is a private engine constant co-located with [`EdgeScrollConfig`],
/// deliberately not a user-facing field: shipping the guard and the knob to
/// disable it together would defeat its purpose. At 80 ms it caps auto-repeat
/// at ~12 columns/second, well above the ~24+/second "dozens" rate that
/// produced the original symptom. Both the validation warning and the runtime
/// clamp route through [`EdgeScrollConfig::effective_repeat_interval_ms`], so
/// they cannot drift.
const EDGE_SCROLL_SPAM_GUARD_FLOOR_MS: u32 = 80;

impl Default for EdgeScrollConfig {
    fn default() -> Self {
        Self {
            band_width: 30,
            // The default repeat interval equals the default animation duration
            // (240 ms), so at the default each column's animation lands as the
            // next begins: a continuous glide with no gaps and no jank.
            initial_delay_ms: 500,
            repeat_interval_ms: 240,
        }
    }
}

impl EdgeScrollConfig {
    /// Effective repeat interval the auto-repeat timer uses at runtime:
    /// `max(configured, animation duration when enabled, spam-guard floor)`.
    ///
    /// This is the single source of truth for the repeat-interval floor — both
    /// the runtime clamp and the validation warning call it, so they cannot
    /// disagree. The animation-duration bound only applies while animation is
    /// enabled (a sub-duration repeat would interrupt each animation mid-flight,
    /// stuttering); the spam-guard floor applies always.
    #[must_use]
    pub fn effective_repeat_interval_ms(&self, animation: &AnimationConfig) -> u32 {
        let mut floor = EDGE_SCROLL_SPAM_GUARD_FLOOR_MS;
        if animation.enabled {
            floor = floor.max(animation.duration_ms);
        }
        self.repeat_interval_ms.max(floor)
    }

    /// Effective initial delay: `max(configured, effective repeat interval)`.
    ///
    /// Passing the already-computed effective repeat interval (from
    /// [`Self::effective_repeat_interval_ms`]) keeps the two clamps in one
    /// pipeline. A configured `0` therefore means "glide at the normal cadence
    /// with no special pause" rather than a near-instant double-scroll on entry.
    /// No warning is emitted for a sub-floor initial delay: its floor makes
    /// unsafe values silently safe.
    #[must_use]
    pub fn effective_initial_delay_ms(&self, effective_repeat_interval_ms: u32) -> u32 {
        self.initial_delay_ms.max(effective_repeat_interval_ms)
    }
}

/// Floor on the hover poll interval, applied at runtime so a configuration typo
/// (e.g. `poll_interval_ms = 0`) cannot busy-loop the daemon. Private to this
/// module, mirroring the edge-scroll spam-guard floor: shipping the guard and a
/// knob to disable it together would defeat its purpose. At 8 ms it caps the
/// poll at ~125 Hz, far below a hardware-mouse reporting rate yet responsive.
const HOVER_POLL_FLOOR_MS: u32 = 8;

/// Hover configuration — focus-follows-mouse and edge-hover-scroll.
///
/// Both behaviors share one low-rate cursor poll folded into the main loop's
/// wait-timeout reduce, so when every behavior flag is off there is no poll
/// deadline and the daemon sleeps indefinitely. (`docs/src/dev-guide/hover.md`)
///
/// # Example
///
/// ```toml
/// [hover]
/// focus_follows_mouse = true
/// focus_dwell_ms = 300
/// edge_scroll = true
/// edge_dwell_ms = 150
/// poll_interval_ms = 50
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct HoverConfig {
    /// Master switch for focus-follows-mouse: when `true`, resting the cursor
    /// on an eligible tracked window for the dwell focuses it through the
    /// existing focus path. Ships **on** — see (`docs/src/dev-guide/hover.md`).
    pub focus_follows_mouse: bool,
    /// Focus-follows-mouse dwell in milliseconds — how long the cursor must
    /// rest on an eligible window before focus fires. Any movement restarts
    /// the dwell, so a jittering mouse never focuses.
    pub focus_dwell_ms: u32,
    /// Master switch for edge-hover-scroll: when `true`, resting the cursor in
    /// a screen edge band for the edge-dwell scrolls the tile viewport one
    /// column immediately, then glides at the shared edge-scroll cadence. The
    /// band width and repeat cadence come from [`EdgeScrollConfig`] (shared
    /// with drag edge-scroll). Ships **on** — see
    /// (`docs/src/dev-guide/hover.md`).
    pub edge_scroll: bool,
    /// Edge-band dwell in milliseconds — how long the cursor must rest in a
    /// screen edge band before the first edge-scroll fires. Shorter than the
    /// focus dwell because reaching the screen edge is already a deliberate
    /// gesture.
    pub edge_dwell_ms: u32,
    /// Cursor poll interval in milliseconds, shared by both hover behaviors.
    /// Clamped up to [`HOVER_POLL_FLOOR_MS`] by [`Self::effective_poll_interval_ms`]
    /// so a typo cannot busy-loop the daemon.
    pub poll_interval_ms: u32,
}

impl Default for HoverConfig {
    fn default() -> Self {
        Self {
            // Ships on: the mouse-driven experience works out of the box. This
            // breaks the daemon's zero-CPU-while-idle property (the poll then
            // wakes ~20 times per second); see (`docs/src/dev-guide/hover.md`).
            focus_follows_mouse: true,
            focus_dwell_ms: 300,
            edge_scroll: true,
            // Shorter than the focus dwell: reaching the screen edge is already
            // a deliberate gesture, so a shorter guard against accidental
            // brushes suffices.
            edge_dwell_ms: 150,
            poll_interval_ms: 50,
        }
    }
}

impl HoverConfig {
    /// Effective poll interval: `max(configured, poll floor)`.
    ///
    /// The runtime clamp the daemon's poll cadence reads, so a sub-floor value
    /// is silently made safe — no startup warning is emitted (the floor makes
    /// unsafe values silently safe, mirroring the edge-scroll initial delay).
    #[must_use]
    pub fn effective_poll_interval_ms(&self) -> u32 {
        self.poll_interval_ms.max(HOVER_POLL_FLOOR_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_roundtrips_toml() {
        let config = FlowConfig::default();
        let toml_str = toml::to_string(&config).expect("serialize");
        let parsed: FlowConfig = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(parsed.columns_per_screen, 4);
        assert_eq!(parsed.column_width, None);
        assert_eq!(parsed.min_column_width_px, 640);
        assert_eq!(parsed.min_window_height_px, 100);
        assert_eq!(parsed.padding.window_gap, 16);
        assert_eq!(parsed.padding.up, 0);
        assert_eq!(parsed.padding.down, 0);
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
        let config: FlowConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.columns_per_screen, 3);
        assert_eq!(config.column_width, Some(1200));
        assert_eq!(config.padding.window_gap, 8);
        assert_eq!(config.padding.up, 10);
        assert_eq!(config.padding.down, 40);
        assert!(!config.animation.enabled);
    }

    /// Positive: empty TOML deserializes to compiled defaults.
    ///
    /// Because `FlowConfig` carries `#[serde(default)]` at the container level,
    /// an empty file
    /// (or a file with no recognized keys) yields a fully-defaulted [`FlowConfig`].
    /// This is the core of the "code is the source of truth" model: there is no
    /// separate shipped-defaults file to merge.
    #[test]
    fn config_from_empty_toml_uses_defaults() {
        let config: FlowConfig = toml::from_str("").expect("empty TOML should use defaults");
        assert_eq!(config, FlowConfig::default());
    }

    /// Positive: partial TOML (a single field) fills the rest from defaults.
    #[test]
    fn config_from_partial_toml_uses_defaults() {
        let config: FlowConfig =
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
        let config: FlowConfig = toml::from_str(toml_str).expect("nested-partial should parse");
        assert_eq!(config.padding.window_gap, 20);
        assert_eq!(config.padding.up, 0);
        assert_eq!(config.padding.down, 0);
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
        let config: FlowConfig =
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
    /// exactly the compiled [`FlowConfig::default()`].
    ///
    /// This enforces the AGENTS.md rule that `default-config.toml` stays in sync
    /// with the compiled `Default` impl. If you change a
    /// default in code, update the example file too (or this test fails).
    #[test]
    fn default_config_toml_matches_compiled_defaults() {
        let example: &str = include_str!("../../default-config.toml");
        let parsed: FlowConfig =
            toml::from_str(example).expect("default-config.toml must parse as FlowConfig");
        assert_eq!(
            parsed,
            FlowConfig::default(),
            "default-config.toml drifted from compiled defaults; \
             update one to match the other"
        );
    }

    /// Positive: `FocusConfig::default()` ships `foreground_sync_interval_ms = 250`
    /// — the tuned ~4 Hz foreground-reconciliation cadence that balances
    /// focus-drift detection latency against wakeup cost.
    ///
    /// A regression to a much larger value would let the post-close-cascade
    /// settle window slip past the next poll (visible focus desync), while a
    /// much smaller value burns wakeups on a microsecond-scale no-op. The
    /// `default-config.toml` sync test catches this only if the example file is
    /// also updated; this focused check guards the compiled `Default` impl
    /// independently, mirroring `border_config_default_overlap_is_one`.
    #[test]
    fn focus_config_default_interval_is_250ms() {
        assert_eq!(FocusConfig::default().foreground_sync_interval_ms, 250);
    }

    /// Positive: `LoadoutConfig::default()` ships `default_path = "loadout.json"`
    /// and `max_age_secs = 60` — the canonical values the daemon resolves
    /// against when no `[loadout]` block is present in the user's `flow.toml`.
    ///
    /// The `default-config.toml` sync test catches drift only when the example
    /// file is also updated; this focused check guards the compiled `Default`
    /// impl independently, mirroring `focus_config_default_interval_is_250ms`
    /// and `border_config_default_overlap_is_one`. A regression to a different
    /// `default_path` would silently break save/restore (file written to one
    /// name, read from another); a regression to `max_age_secs = 0` would
    /// disable the staleness safety net for crashes/hard-kills.
    #[test]
    fn loadout_config_default_values() {
        let default = LoadoutConfig::default();
        assert_eq!(default.default_path, "loadout.json");
        assert_eq!(default.max_age_secs, 60);
    }

    // --- Integration: Full field preservation through round-trip ---

    #[test]
    fn config_roundtrip_preserves_all_fields() {
        // Positive: every field survives TOML → FlowConfig → TOML
        let config = FlowConfig {
            columns_per_screen: 3,
            column_width: Some(1200),
            min_column_width_px: 400,
            min_window_height_px: 120,
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
            borders: BorderConfig {
                enabled: false,
                thickness: 7,
                overlap: 2,
                focused_color: Color::rgb(0x10, 0x20, 0x30),
                unfocused_color: Color::rgb(0x40, 0x50, 0x60),
                floating_color: Color::rgb(0x70, 0x80, 0x90),
            },
            floating: FloatingConfig {
                default_width: Some(1200),
                default_height: Some(800),
            },
            focus: FocusConfig {
                foreground_sync_interval_ms: 400,
            },
            drag: DragConfig {
                col_edge_ratio: 0.3,
                col_edge_max_px: 50,
            },
            edge_scroll: EdgeScrollConfig {
                band_width: 25,
                initial_delay_ms: 350,
                repeat_interval_ms: 200,
            },
            hover: HoverConfig {
                focus_follows_mouse: false,
                focus_dwell_ms: 450,
                edge_scroll: false,
                edge_dwell_ms: 120,
                poll_interval_ms: 30,
            },
            loadout: LoadoutConfig {
                default_path: "my-loadout.json".into(),
                max_age_secs: 120,
            },
            check_for_updates: false,
        };

        let toml_str = toml::to_string(&config).expect("serialize all fields");
        let parsed: FlowConfig = toml::from_str(&toml_str).expect("deserialize all fields");

        assert_eq!(parsed.columns_per_screen, 3);
        assert_eq!(parsed.column_width, Some(1200));
        assert_eq!(parsed.min_column_width_px, 400);
        assert_eq!(parsed.min_window_height_px, 120);
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
        assert!(!parsed.borders.enabled);
        assert_eq!(parsed.borders.thickness, 7);
        assert_eq!(parsed.borders.overlap, 2);
        assert_eq!(parsed.borders.focused_color, Color::rgb(0x10, 0x20, 0x30));
        assert_eq!(parsed.borders.unfocused_color, Color::rgb(0x40, 0x50, 0x60));
        assert_eq!(parsed.borders.floating_color, Color::rgb(0x70, 0x80, 0x90));
        assert_eq!(parsed.floating.default_width, Some(1200));
        assert_eq!(parsed.floating.default_height, Some(800));
        assert_eq!(parsed.focus.foreground_sync_interval_ms, 400);
        assert_eq!(parsed.drag.col_edge_ratio, 0.3);
        assert_eq!(parsed.drag.col_edge_max_px, 50);
        assert_eq!(parsed.edge_scroll.band_width, 25);
        assert_eq!(parsed.edge_scroll.initial_delay_ms, 350);
        assert_eq!(parsed.edge_scroll.repeat_interval_ms, 200);
        assert!(!parsed.hover.focus_follows_mouse);
        assert_eq!(parsed.hover.focus_dwell_ms, 450);
        assert!(!parsed.hover.edge_scroll);
        assert_eq!(parsed.hover.edge_dwell_ms, 120);
        assert_eq!(parsed.hover.poll_interval_ms, 30);
        assert_eq!(parsed.loadout.default_path, "my-loadout.json");
        assert_eq!(parsed.loadout.max_age_secs, 120);
        assert!(!parsed.check_for_updates);
    }

    /// Positive: `check_for_updates` ships enabled by default so the start-time
    /// notification is opt-out, not opt-in. Mirrors the focused
    /// default-value guards (`focus_config_default_interval_is_250ms`,
    /// `border_config_default_overlap_is_one`); the `default-config.toml` sync
    /// test covers the example-file side.
    #[test]
    fn check_for_updates_defaults_to_true() {
        assert!(FlowConfig::default().check_for_updates);
    }

    // --- Edge-scroll auto-repeat timing defaults & clamps ---

    /// Positive: the two auto-repeat knobs ship at the design-session
    /// defaults — a 500 ms first-gap and a 240 ms glide cadence (the latter
    /// matching the default animation duration, so each column lands as the
    /// next begins). Mirrors the other focused default-value guards.
    #[test]
    fn edge_scroll_config_default_auto_repeat_timings() {
        let edge_scroll = EdgeScrollConfig::default();
        assert_eq!(edge_scroll.initial_delay_ms, 500);
        assert_eq!(edge_scroll.repeat_interval_ms, 240);
    }

    /// Positive: at the default config the repeat interval equals the default
    /// animation duration, so the effective value is unchanged (no clamp, no
    /// warning). This is the "continuous glide" invariant.
    #[test]
    fn effective_repeat_interval_unchanged_at_default() {
        let edge_scroll = EdgeScrollConfig::default();
        let anim = AnimationConfig::default();
        assert_eq!(edge_scroll.effective_repeat_interval_ms(&anim), 240);
    }

    /// Positive: the initial delay floor is the effective repeat interval, so a
    /// configured `0` cleanly means "no special pause" — it clamps up to the
    /// repeat cadence rather than producing a near-instant double-scroll.
    #[test]
    fn effective_initial_delay_clamps_to_repeat_interval() {
        let edge_scroll = EdgeScrollConfig {
            initial_delay_ms: 0,
            ..EdgeScrollConfig::default()
        };
        // Effective repeat at default is 240; initial delay clamps up to it.
        assert_eq!(edge_scroll.effective_initial_delay_ms(240), 240);
        // A larger configured delay is respected as-is.
        let edge_scroll = EdgeScrollConfig {
            initial_delay_ms: 600,
            ..EdgeScrollConfig::default()
        };
        assert_eq!(edge_scroll.effective_initial_delay_ms(240), 600);
    }

    /// Positive: with animation enabled, a sub-duration repeat interval is
    /// clamped up to the animation duration (each scroll must let the previous
    /// animation land, or it retargets mid-flight and stutters).
    #[test]
    fn effective_repeat_interval_clamps_to_animation_duration_when_enabled() {
        let edge_scroll = EdgeScrollConfig {
            repeat_interval_ms: 100,
            ..EdgeScrollConfig::default()
        };
        let anim = AnimationConfig {
            enabled: true,
            duration_ms: 300,
            ..AnimationConfig::default()
        };
        assert_eq!(edge_scroll.effective_repeat_interval_ms(&anim), 300);
    }

    /// Positive: with animation disabled, the animation-duration bound drops
    /// away and only the spam-guard floor applies — so the no-animation path
    /// still cannot fly to the far end.
    #[test]
    fn effective_repeat_interval_uses_spam_guard_floor_when_animation_disabled() {
        let edge_scroll = EdgeScrollConfig {
            repeat_interval_ms: 10,
            ..EdgeScrollConfig::default()
        };
        let anim = AnimationConfig {
            enabled: false,
            duration_ms: 300,
            ..AnimationConfig::default()
        };
        // 10 ms would scroll 100 columns/second — clamped to the 80 ms floor
        // (~12/second), well above the "dozens" rate that produced the bug.
        assert_eq!(edge_scroll.effective_repeat_interval_ms(&anim), 80);
    }

    /// Positive: a configured repeat interval above both the animation duration
    /// and the spam-guard floor is respected verbatim (the clamp only raises,
    /// never lowers).
    #[test]
    fn effective_repeat_interval_respects_value_above_all_floors() {
        let edge_scroll = EdgeScrollConfig {
            repeat_interval_ms: 500,
            ..EdgeScrollConfig::default()
        };
        let anim = AnimationConfig::default();
        assert_eq!(edge_scroll.effective_repeat_interval_ms(&anim), 500);
    }

    /// Positive: the default config validates cleanly — the default repeat
    /// interval equals the default animation duration, so no warning fires.
    #[test]
    fn config_validate_accepts_default_edge_scroll_timings() {
        assert!(FlowConfig::default().validate().is_ok());
    }

    /// Negative: a repeat interval below its effective floor warns. The warning
    /// is returned as an `Err` (the loader logs it non-fatally at startup).
    #[test]
    fn config_validate_warns_on_sub_floor_repeat_interval() {
        let config = FlowConfig {
            edge_scroll: EdgeScrollConfig {
                repeat_interval_ms: 50,
                ..EdgeScrollConfig::default()
            },
            ..FlowConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(
            err.contains("edge_scroll.repeat_interval_ms"),
            "warning should name the field: {err}"
        );
        assert!(
            err.contains("clamped at runtime"),
            "warning should explain the clamp: {err}"
        );
    }

    /// Positive: a sub-floor *initial delay* does NOT warn — its floor makes
    /// unsafe values silently safe, so there is nothing to tell the user.
    #[test]
    fn config_validate_does_not_warn_for_initial_delay() {
        let config = FlowConfig {
            edge_scroll: EdgeScrollConfig {
                initial_delay_ms: 0,
                ..EdgeScrollConfig::default()
            },
            ..FlowConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    // --- Hover config defaults, clamp, and round-trip ---

    /// Positive: `HoverConfig::default()` ships focus-follows-mouse on, a 300 ms
    /// focus dwell, edge-hover-scroll on, a 150 ms edge dwell, and a 50 ms poll
    /// interval — the design defaults. Mirrors the focused default-value guards
    /// (`focus_config_default_interval_is_250ms`).
    #[test]
    fn hover_config_default_values() {
        let hover = HoverConfig::default();
        assert!(hover.focus_follows_mouse);
        assert_eq!(hover.focus_dwell_ms, 300);
        assert!(hover.edge_scroll);
        assert_eq!(hover.edge_dwell_ms, 150);
        assert_eq!(hover.poll_interval_ms, 50);
    }

    /// Positive: the poll interval clamps up to its 8 ms floor so a typo cannot
    /// busy-loop the daemon. A value at or above the floor is unchanged.
    #[test]
    fn hover_config_poll_interval_clamps_to_floor() {
        let floor = 8u32;
        // Sub-floor values clamp up.
        assert_eq!(
            HoverConfig {
                poll_interval_ms: 0,
                ..HoverConfig::default()
            }
            .effective_poll_interval_ms(),
            floor
        );
        assert_eq!(
            HoverConfig {
                poll_interval_ms: 7,
                ..HoverConfig::default()
            }
            .effective_poll_interval_ms(),
            floor
        );
        // At the floor — unchanged.
        assert_eq!(
            HoverConfig {
                poll_interval_ms: 8,
                ..HoverConfig::default()
            }
            .effective_poll_interval_ms(),
            floor
        );
        // Above the floor — respected as-is.
        assert_eq!(
            HoverConfig {
                poll_interval_ms: 50,
                ..HoverConfig::default()
            }
            .effective_poll_interval_ms(),
            50
        );
    }

    /// Positive: a partial `[hover]` block fills the rest from compiled defaults
    /// (per-field serde defaults via `#[serde(default)]`).
    #[test]
    fn hover_config_partial_toml_uses_defaults() {
        let toml_str = "[hover]\nfocus_dwell_ms = 450\n";
        let parsed: FlowConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(parsed.hover.focus_dwell_ms, 450);
        assert!(parsed.hover.focus_follows_mouse);
        assert!(parsed.hover.edge_scroll);
        assert_eq!(parsed.hover.edge_dwell_ms, 150);
        assert_eq!(parsed.hover.poll_interval_ms, 50);
    }

    /// Positive: an empty `[hover]` block parses to the compiled defaults.
    #[test]
    fn hover_config_empty_block_uses_defaults() {
        let parsed: FlowConfig = toml::from_str("[hover]\n").expect("parse");
        assert_eq!(parsed.hover, HoverConfig::default());
    }

    /// Positive: with animation enabled and a repeat interval below the
    /// animation duration, the warning fires (the floor is the animation
    /// duration in that case, not the spam-guard constant).
    #[test]
    fn config_validate_warns_when_repeat_below_animation_duration() {
        let config = FlowConfig {
            animation: AnimationConfig {
                enabled: true,
                duration_ms: 350,
                ..AnimationConfig::default()
            },
            edge_scroll: EdgeScrollConfig {
                repeat_interval_ms: 200,
                ..EdgeScrollConfig::default()
            },
            ..FlowConfig::default()
        };
        let err = config.validate().unwrap_err();
        // Effective floor is max(80, 350) = 350.
        assert!(
            err.contains("350"),
            "warning should report floor 350: {err}"
        );
    }

    #[test]
    fn config_validate_rejects_negative_window_padding() {
        let config = FlowConfig {
            padding: Padding {
                window_gap: -1,
                ..Padding::default()
            },
            ..FlowConfig::default()
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
        let config = FlowConfig {
            padding: Padding {
                up: -5,
                ..Padding::default()
            },
            ..FlowConfig::default()
        };
        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("padding.up"));
    }

    #[test]
    fn config_validate_rejects_negative_down_padding() {
        let config = FlowConfig {
            padding: Padding {
                down: -10,
                ..Padding::default()
            },
            ..FlowConfig::default()
        };
        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("padding.down"));
    }

    #[test]
    fn config_validate_accepts_zero_padding() {
        let config = FlowConfig {
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            ..FlowConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_validate_accepts_default_config() {
        assert!(FlowConfig::default().validate().is_ok());
    }

    #[test]
    fn config_validate_rejects_overlap_exceeding_thickness() {
        let config = FlowConfig {
            borders: BorderConfig {
                thickness: 3,
                overlap: 4,
                ..BorderConfig::default()
            },
            ..FlowConfig::default()
        };
        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("borders.overlap"));
    }

    #[test]
    fn config_validate_accepts_overlap_equal_to_thickness() {
        // overlap == thickness is the boundary: content fills the whole slot.
        let config = FlowConfig {
            borders: BorderConfig {
                thickness: 3,
                overlap: 3,
                ..BorderConfig::default()
            },
            ..FlowConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    /// Positive: `BorderConfig::default()` ships `overlap = 1` — the komorebi
    /// default that closes the 1 px DWM-client-edge hairline gap between an
    /// unfocused ring and the window content.
    ///
    /// A regression to `0` would silently reintroduce the gap for every user
    /// relying on the shipped defaults. The `default-config.toml` sync test
    /// catches this only if the example file is also updated; this focused
    /// check guards the compiled `Default` impl independently.
    #[test]
    fn border_config_default_overlap_is_one() {
        assert_eq!(BorderConfig::default().overlap, 1);
    }

    /// Positive: `overlap = 0` (komorebi-style overlap disabled) is accepted
    /// by validation. This is the backward-compat value — the window shrinks
    /// by the full `thickness` and the ring sits wholly in the reserved gap,
    /// never overlapping the content.
    #[test]
    fn config_validate_accepts_overlap_zero() {
        let config = FlowConfig {
            borders: BorderConfig {
                thickness: 3,
                overlap: 0,
                ..BorderConfig::default()
            },
            ..FlowConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    /// Positive: an explicit `overlap = 0` survives a TOML serialize →
    /// deserialize round-trip on `BorderConfig`.
    ///
    /// Guards against a regression where a `skip_serializing_if` attribute
    /// would silently drop the field at its "default-ish" value and lose the
    /// user's explicit "komorebi off" choice on the next config write.
    #[test]
    fn config_overlap_zero_roundtrips_toml() {
        let config = BorderConfig {
            thickness: 5,
            overlap: 0,
            ..BorderConfig::default()
        };
        let toml_str = toml::to_string(&config).expect("serialize");
        let parsed: BorderConfig = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(parsed.overlap, 0);
        assert_eq!(parsed.thickness, 5);
    }

    #[test]
    fn config_validate_rejects_zero_min_column_width() {
        let config = FlowConfig {
            min_column_width_px: 0,
            ..FlowConfig::default()
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
    fn config_validate_rejects_zero_min_window_height() {
        // Negative: min_window_height_px == 0 is invalid (would allow zero-
        // height windows). The validation must reject it with a descriptive
        // error message naming the field.
        let config = FlowConfig {
            min_window_height_px: 0,
            ..FlowConfig::default()
        };
        assert!(config.validate().is_err());
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("min_window_height_px"),
            "error message must name the offending field"
        );
    }

    #[test]
    fn config_validate_rejects_min_exceeding_column_width() {
        let config = FlowConfig {
            min_column_width_px: 1000,
            column_width: Some(960),
            ..FlowConfig::default()
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
        let config = FlowConfig {
            min_column_width_px: 960,
            column_width: Some(960),
            ..FlowConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_validate_rejects_zero_columns_per_screen() {
        let config = FlowConfig {
            columns_per_screen: 0,
            ..FlowConfig::default()
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
        let config = FlowConfig {
            column_width: None,
            min_column_width_px: 9999,
            ..FlowConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    // --- WindowRulesConfig tests ---

    #[test]
    fn window_rules_config_default_roundtrips() {
        let config = WindowRulesConfig::default();
        let toml_str = toml::to_string(&config).expect("serialize");
        let parsed: WindowRulesConfig = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(parsed.default_action, WindowAction::Float);
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
        let toml_str = "default_action = \"float\"\n";
        let config: WindowRulesConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.default_action, WindowAction::Float);
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
            let toml_str =
                toml::to_string(&wrapper).unwrap_or_else(|_| panic!("serialize {expected_kebab}"));
            assert!(
                toml_str.contains(&format!("easing = \"{expected_kebab}\"")),
                "serialization should contain 'easing = \"{expected_kebab}\"', got:\n{toml_str}"
            );

            // Verify deserialization produces the original variant
            let parsed: AnimationConfig = toml::from_str(&toml_str)
                .unwrap_or_else(|_| panic!("deserialize {expected_kebab}"));
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

    // --- FloatingConfig tests ---
    //
    // `FloatingConfig` carries two optional explicit pixel sizes. When a field
    // is `None`, the daemon falls back to a built-in fraction-of-work-area
    // policy (tested in `src/daemon/dispatch.rs::fallback_float_size`). These
    // tests pin the serde + Default contract: explicit values round-trip, and
    // omitted keys parse to `None`.

    /// Positive: `FloatingConfig::default()` is `{None, None}` — both fields
    /// are optional, so the daemon's built-in fallback applies.
    #[test]
    fn floating_config_default_is_none_none() {
        let cfg = FloatingConfig::default();
        assert_eq!(cfg.default_width, None);
        assert_eq!(cfg.default_height, None);
    }

    /// Positive: explicit `Some(pixel)` values survive TOML serialize →
    /// deserialize without being altered or dropped.
    #[test]
    fn floating_config_explicit_values_roundtrip() {
        let toml_str = r#"
[floating]
default_width = 1200
default_height = 800
"#;
        let parsed: FlowConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(parsed.floating.default_width, Some(1200));
        assert_eq!(parsed.floating.default_height, Some(800));

        // Re-serialize and parse once more to confirm a full round-trip.
        let reserialized = toml::to_string(&parsed).expect("serialize");
        let reparsed: FlowConfig = toml::from_str(&reserialized).expect("deserialize");
        assert_eq!(reparsed.floating.default_width, Some(1200));
        assert_eq!(reparsed.floating.default_height, Some(800));
    }

    /// Positive: an empty `[floating]` block (both keys omitted) parses to
    /// `{None, None}`. The daemon then applies the built-in fallback. This
    /// matches what `default-config.toml` ships (commented-out keys).
    #[test]
    fn floating_config_omitted_keys_parse_to_none() {
        let toml_str = "[floating]\n";
        let parsed: FlowConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(parsed.floating.default_width, None);
        assert_eq!(parsed.floating.default_height, None);
        // The rest of the config still comes from compiled defaults.
        assert_eq!(parsed, FlowConfig::default());
    }

    /// Positive: a partial `[floating]` block (only one key) fills the other
    /// from the field's serde default (`None`). Guards against a regression
    /// where setting one dimension would force the other to a non-None value.
    #[test]
    fn floating_config_partial_width_only_preserves_height_none() {
        let toml_str = "[floating]\ndefault_width = 1000\n";
        let parsed: FlowConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(parsed.floating.default_width, Some(1000));
        assert_eq!(parsed.floating.default_height, None);
    }
}
