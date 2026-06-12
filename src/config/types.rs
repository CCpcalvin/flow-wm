//! Configuration type definitions matching the TOML schema.
//!
//! Every field in [`StmConfig`] and its nested types is **required** in TOML —
//! there are no `#[serde(default)]` annotations. The single source of truth for
//! default values is `default-config.toml` shipped next to `stmd.exe`.
//!
//! # Design: No Serde Defaults
//!
//! Intentionally omitting `#[serde(default)]` creates a built-in safety net:
//!
//! - `default-config.toml` **must** contain every field. If a developer adds a
//!   new field to a Rust struct but forgets to add it to `default-config.toml`,
//!   deserialization fails with a clear `"missing field 'xyz'"` error.
//! - Users' `stm.toml` files can still be partial — the TOML-level merge in
//!   [`lifecycle::load_merged_app_config`] fills in missing fields from shipped
//!   defaults before deserializing.
//! - The compiled-in Rust `Default` impl serves as an **emergency fallback only**
//!   (e.g., dev environments without the shipped file). It is NOT the canonical
//!   source of default values.
//!
//! # Exceptions
//!
//! `Vec` fields (like `WindowRulesConfig::rules`) retain `#[serde(default)]`
//! because an empty collection is unambiguous. Per-entry boolean flags
//! (like `WindowRule::override_persist`) also keep defaults for convenience.
//!
//! # Config File Split
//!
//! Configuration is split across two TOML files:
//!
//! - **`stm.toml`** ([`StmConfig`]) — Application settings (hotkeys, padding,
//!   animation, etc.). Loaded from `%USERPROFILE%\.config\stm\stm.toml`
//!   (see [`config::dirs`](crate::config::dirs) for the full resolution chain).
//!
//! - **`stm-rules.toml`** ([`WindowRulesConfig`]) — Window classification rules
//!   and default action. Loaded from `%USERPROFILE%\.config\stm\stm-rules.toml`.
//!
//! This separation allows users to edit rules frequently (adding ignore patterns
//! for new apps) without risk of corrupting their app settings, and vice versa.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Top-level application configuration structure.
///
/// Loaded from `%USERPROFILE%\.config\stm\stm.toml` (see [`config::dirs`](crate::config::dirs)
/// for the full resolution chain including `--config` flag and `STM_CONFIG_DIR` env var
/// overrides). Every field is **required** in TOML — there are no serde defaults.
///
/// The canonical default values live in `default-config.toml` shipped next to `stmd.exe`.
/// The TOML-level merge (see [`lifecycle::load_merged_app_config`]) fills in missing
/// fields from shipped defaults before deserializing, so users' `stm.toml` files can
/// be partial.
///
/// This struct contains **application settings only** — hotkeys, padding,
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
/// super_key = "VK_F24"
/// columns_per_screen = 4
/// min_column_width_px = 320
///
/// [padding]
/// window_gap = 4
/// up = 0
/// down = 0
///
/// [hotkeys]
/// focus_left = "Super+H"
/// focus_right = "Super+L"
///
/// [animation]
/// enabled = true
/// duration_ms = 180
/// easing = "ease-out-expo"
/// ```
///
/// See `docs/spec/04-config-and-persistence.md` for the full schema.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StmConfig {
    /// The virtual key code treated as the Super/modifier key.
    pub super_key: String,

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column_width: Option<u32>,

    /// Minimum column width in pixels. Columns cannot be resized below this.
    pub min_column_width_px: u32,

    /// Padding settings.
    pub padding: Padding,

    /// Hotkey bindings.
    pub hotkeys: Hotkeys,

    /// Animation settings.
    pub animation: AnimationConfig,

    /// Behavior when a minimized tiling window is restored.
    pub minimize_restore: MinimizeRestore,
}

fn default_super_key() -> String {
    "VK_F24".into()
}

fn default_window_action() -> WindowAction {
    WindowAction::Tile
}

/// Default number of columns per screen.
const fn default_columns_per_screen() -> u32 {
    4
}

/// Default minimum column width in pixels.
const fn default_min_column_width_px() -> u32 {
    320
}

impl Default for StmConfig {
    fn default() -> Self {
        Self {
            super_key: default_super_key(),
            columns_per_screen: default_columns_per_screen(),
            column_width: None,
            min_column_width_px: default_min_column_width_px(),
            padding: Padding::default(),
            hotkeys: Hotkeys::default(),
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
/// │ ┌──── window (HWND) ────┐   ┌──── window (HWND) ────┐   │
/// │ │                       │   │                       │   │
/// │ └───────────────────────┘   └───────────────────────┘   │
/// │ ↓ window_gap                                             │
/// │ ↓ padding.down                                           │
/// └──────────────────────────────────────────────────────────┘
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Padding {
    /// Uniform gap between all elements (windows and screen edges), in pixels.
    ///
    /// This single value controls horizontal (inter-column), vertical (inter-row),
    /// and edge-to-window spacing. Setting `window_gap = 4` means every gap
    /// everywhere is 4 pixels.
    pub window_gap: i32,
    /// Top screen margin — reserved space above the tiling area.
    pub up: i32,
    /// Bottom screen margin — reserved space below the tiling area (e.g., taskbar).
    pub down: i32,
}

fn default_window_gap() -> i32 {
    4
}

impl Default for Padding {
    fn default() -> Self {
        Self {
            window_gap: default_window_gap(),
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

/// All hotkey bindings (13 total).
///
/// Default keybinds use Vim-style `Super+H/J/K/L` for focus. All defaults are
/// defined in a manual `Default` impl (emergency fallback for dev environments).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Hotkeys {
    /// Focus left: default `Super+H`.
    pub focus_left: String,
    /// Focus right: default `Super+L`.
    pub focus_right: String,
    /// Focus up: default `Super+K`.
    pub focus_up: String,
    /// Focus down: default `Super+J`.
    pub focus_down: String,
    /// Swap left: default `Super+Shift+H`.
    pub swap_left: String,
    /// Swap right: default `Super+Shift+L`.
    pub swap_right: String,
    /// Scroll left: default `Super+Left`.
    pub scroll_left: String,
    /// Scroll right: default `Super+Right`.
    pub scroll_right: String,
    /// Toggle float/tiling: default `Super+Space`.
    pub toggle_float: String,
    /// Toggle monocle mode: default `Super+M`.
    pub toggle_monocle: String,
    /// Close focused window: default `Super+Q`.
    pub close_window: String,
    /// Reload config from disk: default `Super+Shift+R`.
    pub reload_config: String,
    /// Place window above others in column: default `Super+A`.
    pub place_above: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            focus_left: "Super+H".into(),
            focus_right: "Super+L".into(),
            focus_up: "Super+K".into(),
            focus_down: "Super+J".into(),
            swap_left: "Super+Shift+H".into(),
            swap_right: "Super+Shift+L".into(),
            scroll_left: "Super+Left".into(),
            scroll_right: "Super+Right".into(),
            toggle_float: "Super+Space".into(),
            toggle_monocle: "Super+M".into(),
            close_window: "Super+Q".into(),
            reload_config: "Super+Shift+R".into(),
            place_above: "Super+A".into(),
        }
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
    /// Optional initial width in eighths (1–8).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_width_eighths: Option<u8>,
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
/// initial_width_eighths = 4
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

/// Animation configuration for layout transitions.
///
/// When disabled, all [`WindowMove`](crate::layout::WindowMove)s are applied
/// instantly (hint set to [`Restore`](crate::layout::AnimationHint::Restore)).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AnimationConfig {
    /// Whether layout transitions are animated.
    pub enabled: bool,
    /// Animation duration in milliseconds.
    pub duration_ms: u32,
    /// Easing function name (e.g., `"ease-out-expo"`).
    pub easing: String,
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            duration_ms: 180,
            easing: "ease-out-expo".into(),
        }
    }
}

/// Strategy for restoring minimized tiling windows.
///
/// - `OriginalSlot` — put the window back where it was before minimize
/// - `RightOfFocused` — insert as a new column to the right of focused
/// - `AppendRight` — append as the rightmost column on the canvas
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
        assert_eq!(parsed.super_key, "VK_F24");
        assert_eq!(parsed.columns_per_screen, 4);
        assert_eq!(parsed.column_width, None);
        assert_eq!(parsed.min_column_width_px, 320);
        assert_eq!(parsed.padding.window_gap, 4);
        assert_eq!(parsed.padding.up, 0);
        assert_eq!(parsed.padding.down, 0);
        assert_eq!(parsed.animation.duration_ms, 180);
        assert_eq!(parsed.animation.easing, "ease-out-expo");
    }

    #[test]
    fn config_from_toml_with_settings() {
        // Full TOML required since there are no serde defaults.
        let toml_str = r#"
super_key = "VK_LWIN"
columns_per_screen = 3
column_width = 1200
min_column_width_px = 400

[padding]
window_gap = 8
up = 10
down = 40

[hotkeys]
focus_left = "Super+H"
focus_right = "Super+L"
focus_up = "Super+K"
focus_down = "Super+J"
swap_left = "Super+Shift+H"
swap_right = "Super+Shift+L"
scroll_left = "Super+Left"
scroll_right = "Super+Right"
toggle_float = "Super+Space"
toggle_monocle = "Super+M"
close_window = "Super+Q"
reload_config = "Super+Shift+R"
place_above = "Super+A"

[animation]
enabled = false
duration_ms = 180
easing = "ease-out-expo"

[minimize_restore]
strategy = "original_slot"
"#;
        let config: StmConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.super_key, "VK_LWIN");
        assert_eq!(config.columns_per_screen, 3);
        assert_eq!(config.column_width, Some(1200));
        assert_eq!(config.padding.window_gap, 8);
        assert_eq!(config.padding.up, 10);
        assert_eq!(config.padding.down, 40);
        assert!(!config.animation.enabled);
    }

    /// Negative: empty TOML should fail to parse since all fields are required.
    ///
    /// This is the safety net — if a field is missing from `default-config.toml`,
    /// the error is caught at deserialization time.
    #[test]
    fn config_from_empty_toml_is_rejected() {
        let toml_str = "";
        let result = toml::from_str::<StmConfig>(toml_str);
        assert!(
            result.is_err(),
            "empty TOML should fail without serde defaults"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("missing field"),
            "error should mention missing field: {err}"
        );
    }

    /// Negative: partial TOML (missing fields) should fail to parse.
    #[test]
    fn config_from_partial_toml_is_rejected() {
        let toml_str = "columns_per_screen = 4\n";
        let result = toml::from_str::<StmConfig>(toml_str);
        assert!(
            result.is_err(),
            "partial TOML should fail without serde defaults"
        );
    }

    // --- Integration: Full field preservation through round-trip ---

    #[test]
    fn config_roundtrip_preserves_all_fields() {
        // Positive: every field survives TOML → StmConfig → TOML
        let config = StmConfig {
            super_key: "VK_LWIN".into(),
            columns_per_screen: 3,
            column_width: Some(1200),
            min_column_width_px: 400,
            padding: Padding {
                window_gap: 6,
                up: 10,
                down: 40,
            },
            hotkeys: Hotkeys {
                focus_left: "Alt+H".into(),
                focus_right: "Alt+L".into(),
                focus_up: "Alt+K".into(),
                focus_down: "Alt+J".into(),
                swap_left: "Alt+Shift+H".into(),
                swap_right: "Alt+Shift+L".into(),
                scroll_left: "Alt+Left".into(),
                scroll_right: "Alt+Right".into(),
                toggle_float: "Alt+Space".into(),
                toggle_monocle: "Alt+M".into(),
                close_window: "Alt+Q".into(),
                reload_config: "Alt+Shift+R".into(),
                place_above: "Alt+A".into(),
            },
            animation: AnimationConfig {
                enabled: false,
                duration_ms: 250,
                easing: "ease-in-out-cubic".into(),
            },
            minimize_restore: MinimizeRestore {
                strategy: MinimizeRestoreStrategy::AppendRight,
            },
        };

        let toml_str = toml::to_string(&config).expect("serialize all fields");
        let parsed: StmConfig = toml::from_str(&toml_str).expect("deserialize all fields");

        assert_eq!(parsed.super_key, "VK_LWIN");
        assert_eq!(parsed.columns_per_screen, 3);
        assert_eq!(parsed.column_width, Some(1200));
        assert_eq!(parsed.min_column_width_px, 400);
        assert_eq!(parsed.padding.window_gap, 6);
        assert_eq!(parsed.padding.up, 10);
        assert_eq!(parsed.padding.down, 40);
        assert_eq!(parsed.hotkeys.focus_left, "Alt+H");
        assert_eq!(parsed.hotkeys.place_above, "Alt+A");
        assert!(!parsed.animation.enabled);
        assert_eq!(parsed.animation.duration_ms, 250);
        assert_eq!(parsed.animation.easing, "ease-in-out-cubic");
        assert_eq!(
            parsed.minimize_restore.strategy,
            MinimizeRestoreStrategy::AppendRight
        );
    }

    #[test]
    fn config_validate_rejects_negative_window_padding() {
        let mut config = StmConfig::default();
        config.padding.window_gap = -1;
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
        let mut config = StmConfig::default();
        config.padding.up = -5;
        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("padding.up"));
    }

    #[test]
    fn config_validate_rejects_negative_down_padding() {
        let mut config = StmConfig::default();
        config.padding.down = -10;
        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("padding.down"));
    }

    #[test]
    fn config_validate_accepts_zero_padding() {
        let mut config = StmConfig::default();
        config.padding.window_gap = 0;
        config.padding.up = 0;
        config.padding.down = 0;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_validate_accepts_default_config() {
        assert!(StmConfig::default().validate().is_ok());
    }

    #[test]
    fn config_validate_rejects_zero_min_column_width() {
        let mut config = StmConfig::default();
        config.min_column_width_px = 0;
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
        let mut config = StmConfig::default();
        config.min_column_width_px = 1000;
        config.column_width = Some(960);
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
        let mut config = StmConfig::default();
        config.min_column_width_px = 960;
        config.column_width = Some(960);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_validate_rejects_zero_columns_per_screen() {
        let mut config = StmConfig::default();
        config.columns_per_screen = 0;
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
        let mut config = StmConfig::default();
        config.column_width = None;
        config.min_column_width_px = 9999;
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
initial_width_eighths = 4
"#;
        let config: WindowRulesConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.default_action, WindowAction::Float);
        assert_eq!(config.rules.len(), 2);
        assert_eq!(config.rules[0].action, WindowAction::Ignore);
        assert_eq!(config.rules[1].initial_width_eighths, Some(4));
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
                initial_width_eighths: None,
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
