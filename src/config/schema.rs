//! JSON Schema generation for config editor autocomplete.
//!
//! Generates a JSON Schema from [`super::types::StmConfig`] via
//! `schemars`. Write the output to `%APPDATA%\stm\stm-config-schema.json`
//! for VS Code / Neovim YAML autocomplete.

use crate::common::{StmError, StmResult};
use schemars::schema_for;

use super::types::StmConfig;

/// Generate the JSON Schema for `StmConfig`.
///
/// The schema can be written to `%APPDATA%\stm\stm-config-schema.json`
/// for editor autocomplete support (VS Code, Neovim with yaml-language-server).
pub fn generate_config_schema() -> StmResult<String> {
    let schema = schema_for!(StmConfig);
    serde_json::to_string_pretty(&schema)
        .map_err(|e| StmError::Config(format!("schema generation failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_valid_json() {
        let json = generate_config_schema().expect("schema gen");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("schema should be valid JSON");
        // Check it has the basic JSON Schema properties
        assert!(parsed.is_object());
        let obj = parsed.as_object().expect("schema is an object");
        assert!(obj.contains_key("properties"));
    }

    #[test]
    fn schema_references_stm_config() {
        let json = generate_config_schema().expect("schema gen");
        assert!(json.contains("super_key"));
        assert!(json.contains("column_width"));
        assert!(json.contains("padding"));
        assert!(json.contains("hotkeys"));
        assert!(json.contains("window_rules"));
        assert!(json.contains("animation"));
    }

    #[test]
    fn schema_references_all_top_level_properties() {
        // Positive: schema must enumerate every field of StmConfig
        let json = generate_config_schema().expect("schema gen");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("schema is valid JSON");
        let props = parsed
            .pointer("/properties")
            .expect("has /properties")
            .as_object()
            .expect("properties is object");

        let expected_keys = [
            "super_key",
            "default_window_action",
            "column_width",
            "padding",
            "hotkeys",
            "window_rules",
            "animation",
            "minimize_restore",
        ];
        for key in &expected_keys {
            assert!(
                props.contains_key(*key),
                "schema missing top-level property: {key}"
            );
        }
    }

    #[test]
    fn schema_padding_has_window_up_down() {
        // Positive: nested Padding schema must have window/up/down
        let json = generate_config_schema().expect("schema gen");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("schema is valid JSON");

        let padding_schema = parsed
            .pointer("/properties/padding")
            .expect("has /properties/padding");

        // schemars wraps $ref in allOf; resolve to definitions
        let ref_path = padding_schema
            .get("allOf")
            .and_then(|v| v.get(0))
            .and_then(|v| v.get("$ref"))
            .and_then(|v| v.as_str())
            .expect("padding has allOf > $ref");
        let ref_path = ref_path.trim_start_matches("#/");
        let padding_props = parsed
            .pointer(&format!("/{ref_path}/properties"))
            .unwrap_or_else(|| panic!("resolved ref {ref_path} has properties"));

        let obj = padding_props
            .as_object()
            .expect("padding properties is object");
        assert!(obj.contains_key("window"), "padding missing 'window'");
        assert!(obj.contains_key("up"), "padding missing 'up'");
        assert!(obj.contains_key("down"), "padding missing 'down'");
    }

    #[test]
    fn schema_hotkeys_has_all_bindings() {
        // Positive: Hotkeys schema must list every binding
        let json = generate_config_schema().expect("schema gen");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("schema is valid JSON");

        let hk_schema = parsed
            .pointer("/properties/hotkeys")
            .expect("has /properties/hotkeys");

        let ref_path = hk_schema
            .get("allOf")
            .and_then(|v| v.get(0))
            .and_then(|v| v.get("$ref"))
            .and_then(|v| v.as_str())
            .expect("hotkeys has allOf > $ref");
        let ref_path = ref_path.trim_start_matches("#/");
        let hk_props = parsed
            .pointer(&format!("/{ref_path}/properties"))
            .expect(&format!("resolved ref {ref_path} has properties"));

        let obj = hk_props.as_object().expect("hotkeys properties is object");

        let expected_hotkeys = [
            "focus_left",
            "focus_right",
            "focus_up",
            "focus_down",
            "swap_left",
            "swap_right",
            "scroll_left",
            "scroll_right",
            "toggle_float",
            "toggle_monocle",
            "close_window",
            "reload_config",
            "place_above",
        ];
        for key in &expected_hotkeys {
            assert!(obj.contains_key(*key), "hotkeys missing binding: {key}");
        }
    }

    #[test]
    fn schema_window_rules_is_array() {
        // Positive: window_rules must be typed as an array
        let json = generate_config_schema().expect("schema gen");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("schema is valid JSON");
        let wr_type = parsed
            .pointer("/properties/window_rules")
            .and_then(|v| v.get("type"))
            .expect("window_rules has a type");
        assert_eq!(wr_type.as_str(), Some("array"));
    }

    #[test]
    fn schema_animation_has_enabled_duration_easing() {
        // Positive: AnimationConfig schema must list all three fields
        let json = generate_config_schema().expect("schema gen");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("schema is valid JSON");

        let anim_schema = parsed
            .pointer("/properties/animation")
            .expect("has /properties/animation");

        let ref_path = anim_schema
            .get("allOf")
            .and_then(|v| v.get(0))
            .and_then(|v| v.get("$ref"))
            .and_then(|v| v.as_str())
            .expect("animation has allOf > $ref");
        let ref_path = ref_path.trim_start_matches("#/");
        let anim_props = parsed
            .pointer(&format!("/{ref_path}/properties"))
            .expect(&format!("resolved ref {ref_path} has properties"));

        let obj = anim_props
            .as_object()
            .expect("animation properties is object");

        assert!(obj.contains_key("enabled"));
        assert!(obj.contains_key("duration_ms"));
        assert!(obj.contains_key("easing"));
    }
}
