//! Configuration type definitions matching the YAML schema.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Top-level configuration structure.
///
/// See `docs/04-config-and-persistence.md` for the full schema.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StmConfig {
    /// The virtual key code treated as the Super/modifier key.
    #[serde(default = "default_super_key")]
    pub super_key: String,

    /// Default action for windows not matching any rule.
    #[serde(default = "default_window_action")]
    pub default_window_action: WindowAction,

    /// Default column width in pixels. Default: 960.
    #[serde(default = "default_column_width")]
    pub column_width: u32,

    /// Padding settings.
    #[serde(default)]
    pub padding: Padding,

    /// Hotkey bindings.
    #[serde(default)]
    pub hotkeys: Hotkeys,

    /// Window classification rules (first match wins).
    #[serde(default)]
    pub window_rules: Vec<WindowRule>,

    /// Animation settings.
    #[serde(default)]
    pub animation: AnimationConfig,

    /// Behavior when a minimized tiling window is restored.
    #[serde(default)]
    pub minimize_restore: MinimizeRestore,
}

fn default_super_key() -> String {
    "VK_F24".into()
}

fn default_window_action() -> WindowAction {
    WindowAction::Tile
}

/// Default column width in pixels.
const fn default_column_width() -> u32 {
    960
}

impl Default for StmConfig {
    fn default() -> Self {
        Self {
            super_key: default_super_key(),
            default_window_action: default_window_action(),
            column_width: default_column_width(),
            padding: Padding::default(),
            hotkeys: Hotkeys::default(),
            window_rules: Vec::new(),
            animation: AnimationConfig::default(),
            minimize_restore: MinimizeRestore::default(),
        }
    }
}

/// Padding configuration in pixels.
///
/// `window` is the inset around each window within its container cell.
/// `up` and `down` are screen-level top/bottom margins so windows
/// don't touch the screen edges.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Padding {
    #[serde(default = "default_window_padding")]
    pub window: i32,
    #[serde(default)]
    pub up: i32,
    #[serde(default)]
    pub down: i32,
}

fn default_window_padding() -> i32 {
    4
}

impl Default for Padding {
    fn default() -> Self {
        Self {
            window: default_window_padding(),
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
        if self.window < 0 {
            return Err(format!(
                "padding.window must be non-negative, got {}",
                self.window
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
    /// semantically invalid values like negative padding.
    pub fn validate(&self) -> Result<(), String> {
        self.padding.validate()?;
        Ok(())
    }
}

/// All hotkey bindings.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct Hotkeys {
    #[serde(default = "default_focus_left")]
    pub focus_left: String,
    #[serde(default = "default_focus_right")]
    pub focus_right: String,
    #[serde(default = "default_focus_up")]
    pub focus_up: String,
    #[serde(default = "default_focus_down")]
    pub focus_down: String,
    #[serde(default = "default_swap_left")]
    pub swap_left: String,
    #[serde(default = "default_swap_right")]
    pub swap_right: String,
    #[serde(default = "default_scroll_left")]
    pub scroll_left: String,
    #[serde(default = "default_scroll_right")]
    pub scroll_right: String,
    #[serde(default = "default_toggle_float")]
    pub toggle_float: String,
    #[serde(default = "default_toggle_monocle")]
    pub toggle_monocle: String,
    #[serde(default = "default_close_window")]
    pub close_window: String,
    #[serde(default = "default_reload_config")]
    pub reload_config: String,
    #[serde(default = "default_place_above")]
    pub place_above: String,
}

macro_rules! default_hotkey {
    ($name:ident, $value:literal) => {
        fn $name() -> String {
            $value.into()
        }
    };
}

default_hotkey!(default_focus_left, "Super+H");
default_hotkey!(default_focus_right, "Super+L");
default_hotkey!(default_focus_up, "Super+K");
default_hotkey!(default_focus_down, "Super+J");
default_hotkey!(default_swap_left, "Super+Shift+H");
default_hotkey!(default_swap_right, "Super+Shift+L");
default_hotkey!(default_scroll_left, "Super+Left");
default_hotkey!(default_scroll_right, "Super+Right");
default_hotkey!(default_toggle_float, "Super+Space");
default_hotkey!(default_toggle_monocle, "Super+M");
default_hotkey!(default_close_window, "Super+Q");
default_hotkey!(default_reload_config, "Super+Shift+R");
default_hotkey!(default_place_above, "Super+A");

/// A single window classification rule.
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct MatchRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_contains: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_regex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_path: Option<String>,
}

/// Action to apply to a matched window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowAction {
    Tile,
    Float,
    Ignore,
}

/// Animation configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AnimationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_duration_ms")]
    pub duration_ms: u32,
    #[serde(default = "default_easing")]
    pub easing: String,
}

fn default_true() -> bool {
    true
}

fn default_duration_ms() -> u32 {
    180
}

fn default_easing() -> String {
    "ease-out-expo".into()
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            duration_ms: default_duration_ms(),
            easing: default_easing(),
        }
    }
}

/// Strategy for restoring minimized tiling windows.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MinimizeRestore {
    #[serde(default = "default_minimize_restore_strategy")]
    pub strategy: MinimizeRestoreStrategy,
}

fn default_minimize_restore_strategy() -> MinimizeRestoreStrategy {
    MinimizeRestoreStrategy::OriginalSlot
}

impl Default for MinimizeRestore {
    fn default() -> Self {
        Self {
            strategy: default_minimize_restore_strategy(),
        }
    }
}

/// Available minimize restore strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MinimizeRestoreStrategy {
    OriginalSlot,
    RightOfFocused,
    AppendRight,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_roundtrips_yaml() {
        let config = StmConfig::default();
        let yaml = serde_yaml::to_string(&config).expect("serialize");
        let parsed: StmConfig = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(parsed.super_key, "VK_F24");
        assert_eq!(parsed.default_window_action, WindowAction::Tile);
        assert_eq!(parsed.column_width, 960);
        assert_eq!(parsed.padding.window, 4);
        assert_eq!(parsed.padding.up, 0);
        assert_eq!(parsed.padding.down, 0);
        assert_eq!(parsed.animation.duration_ms, 180);
        assert_eq!(parsed.animation.easing, "ease-out-expo");
    }

    #[test]
    fn config_from_yaml_with_rules() {
        let yaml = r#"
super_key: VK_F24
default_window_action: tile
padding:
  window: 8
  up: 10
  down: 40
window_rules:
  - match:
      exe: "explorer.exe"
      title_contains: "Open"
    action: ignore
  - match:
      class: "Chrome_WidgetWin_1"
    action: tile
    initial_width_eighths: 4
animation:
  enabled: false
"#;
        let config: StmConfig = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(config.padding.window, 8);
        assert_eq!(config.padding.up, 10);
        assert_eq!(config.padding.down, 40);
        assert_eq!(config.window_rules.len(), 2);
        assert_eq!(config.window_rules[0].action, WindowAction::Ignore);
        assert!(!config.animation.enabled);
    }

    #[test]
    fn config_from_minimal_yaml() {
        // Empty YAML should produce defaults
        let yaml = "{}";
        let config: StmConfig = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(config.super_key, "VK_F24");
        assert_eq!(config.padding.window, 4);
        assert!(config.window_rules.is_empty());
    }

    // --- Integration: Full field preservation through round-trip ---

    #[test]
    fn config_roundtrip_preserves_all_fields() {
        // Positive: every field survives YAML → StmConfig → YAML
        let config = StmConfig {
            super_key: "VK_LWIN".into(),
            default_window_action: WindowAction::Float,
            column_width: 1200,
            padding: Padding {
                window: 6,
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
            window_rules: vec![
                WindowRule {
                    match_: MatchRule {
                        exe: Some("firefox.exe".into()),
                        title: None,
                        title_contains: Some("YouTube".into()),
                        title_regex: None,
                        class: Some("MozillaWindowClass".into()),
                        process_path: None,
                    },
                    action: WindowAction::Tile,
                    initial_width_eighths: Some(6),
                    override_persist: true,
                },
                WindowRule {
                    match_: MatchRule {
                        exe: None,
                        title: Some("Calculator".into()),
                        title_contains: None,
                        title_regex: Some("^Settings".into()),
                        class: None,
                        process_path: Some("C:\\Windows\\System32\\calc.exe".into()),
                    },
                    action: WindowAction::Ignore,
                    initial_width_eighths: None,
                    override_persist: false,
                },
            ],
            animation: AnimationConfig {
                enabled: false,
                duration_ms: 250,
                easing: "ease-in-out-cubic".into(),
            },
            minimize_restore: MinimizeRestore {
                strategy: MinimizeRestoreStrategy::AppendRight,
            },
        };

        let yaml = serde_yaml::to_string(&config).expect("serialize all fields");
        let parsed: StmConfig = serde_yaml::from_str(&yaml).expect("deserialize all fields");

        assert_eq!(parsed.super_key, "VK_LWIN");
        assert_eq!(parsed.default_window_action, WindowAction::Float);
        assert_eq!(parsed.column_width, 1200);
        assert_eq!(parsed.padding.window, 6);
        assert_eq!(parsed.padding.up, 10);
        assert_eq!(parsed.padding.down, 40);
        assert_eq!(parsed.hotkeys.focus_left, "Alt+H");
        assert_eq!(parsed.hotkeys.place_above, "Alt+A");
        assert_eq!(parsed.window_rules.len(), 2);
        assert_eq!(parsed.window_rules[0].action, WindowAction::Tile);
        assert_eq!(parsed.window_rules[0].initial_width_eighths, Some(6));
        assert!(parsed.window_rules[0].override_persist);
        assert_eq!(
            parsed.window_rules[1].match_.process_path,
            Some("C:\\Windows\\System32\\calc.exe".into())
        );
        assert!(!parsed.animation.enabled);
        assert_eq!(parsed.animation.duration_ms, 250);
        assert_eq!(parsed.animation.easing, "ease-in-out-cubic");
        assert_eq!(
            parsed.minimize_restore.strategy,
            MinimizeRestoreStrategy::AppendRight
        );
    }

    #[test]
    fn config_with_skip_serializing_if_false_defaults() {
        // Positive: fields with skip_serializing_if serialize when set
        let yaml = r#"
super_key: VK_RWIN
window_rules:
  - match:
      exe: "notepad.exe"
    action: tile
    initial_width_eighths: 5
    override_persist: true
"#;
        let config: StmConfig = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(config.window_rules.len(), 1);
        assert_eq!(config.window_rules[0].initial_width_eighths, Some(5));
        assert!(config.window_rules[0].override_persist);
    }

    #[test]
    fn config_invalid_enum_rejects() {
        // Negative: invalid window_action value returns parse error
        let yaml = r#"
default_window_action: foobar
"#;
        let result = serde_yaml::from_str::<StmConfig>(yaml);
        assert!(result.is_err(), "invalid enum value should reject");
    }

    #[test]
    fn config_all_window_actions_parse() {
        // Positive: all three window actions parse correctly
        for action_str in ["tile", "float", "ignore"] {
            let yaml = format!("default_window_action: {action_str}");
            let config: StmConfig = serde_yaml::from_str(&yaml).expect(&yaml);
            assert_eq!(
                config.default_window_action,
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
    fn config_validate_rejects_negative_window_padding() {
        // Negative: padding.window < 0 should fail validation
        let mut config = StmConfig::default();
        config.padding.window = -1;
        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("padding.window"));
    }

    #[test]
    fn config_validate_rejects_negative_up_padding() {
        // Negative: padding.up < 0 should fail validation
        let mut config = StmConfig::default();
        config.padding.up = -5;
        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("padding.up"));
    }

    #[test]
    fn config_validate_rejects_negative_down_padding() {
        // Negative: padding.down < 0 should fail validation
        let mut config = StmConfig::default();
        config.padding.down = -10;
        assert!(config.validate().is_err());
        assert!(config.validate().unwrap_err().contains("padding.down"));
    }

    #[test]
    fn config_validate_accepts_zero_padding() {
        // Positive: all zeros is valid
        let mut config = StmConfig::default();
        config.padding.window = 0;
        config.padding.up = 0;
        config.padding.down = 0;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_validate_accepts_default_config() {
        // Positive: default config should always validate
        assert!(StmConfig::default().validate().is_ok());
    }
}
