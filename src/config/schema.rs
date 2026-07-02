//! JSON Schema generation for config editor autocomplete.
//!
//! Generates JSON Schemas from both [`super::types::FlowConfig`] (app settings)
//! and [`super::types::WindowRulesConfig`] (window rules) via `schemars`.
//!
//! These schemas are used by the **taplo** TOML language server for IDE
//! autocomplete and validation. They are written to a `schemas/` subdirectory
//! inside the config directory when [`super::lifecycle::init_config_dir`] runs.

use crate::common::{FlowError, FlowResult};
use schemars::schema_for;

use super::types::{FlowConfig, WindowRulesConfig};

/// Generate the JSON Schema for [`FlowConfig`] (app settings).
///
/// The schema can be written to `%APPDATA%\flow\schemas\flow-config.schema.json`
/// for editor autocomplete support (VS Code, Neovim with taplo LSP).
pub fn generate_config_schema() -> FlowResult<String> {
    let schema = schema_for!(FlowConfig);
    serde_json::to_string_pretty(&schema)
        .map_err(|e| FlowError::Config(format!("schema generation failed: {e}")))
}

/// Generate the JSON Schema for [`WindowRulesConfig`] (window rules).
///
/// The schema can be written to `%APPDATA%\flow\schemas\flow-rules.schema.json`
/// for editor autocomplete support on the rules file.
pub fn generate_rules_schema() -> FlowResult<String> {
    let schema = schema_for!(WindowRulesConfig);
    serde_json::to_string_pretty(&schema)
        .map_err(|e| FlowError::Config(format!("rules schema generation failed: {e}")))
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
    fn schema_references_flow_config() {
        let json = generate_config_schema().expect("schema gen");
        assert!(json.contains("column_width"));
        assert!(json.contains("padding"));
        assert!(json.contains("animation"));
    }

    #[test]
    fn schema_references_all_top_level_properties() {
        // Positive: schema must enumerate every field of FlowConfig
        let json = generate_config_schema().expect("schema gen");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("schema is valid JSON");
        let props = parsed
            .pointer("/properties")
            .expect("has /properties")
            .as_object()
            .expect("properties is object");

        let expected_keys = [
            "column_width",
            "min_column_width_px",
            "padding",
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
        let json = generate_config_schema().expect("schema gen");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("schema is valid JSON");

        let padding_schema = parsed
            .pointer("/properties/padding")
            .expect("has /properties/padding");

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
        assert!(
            obj.contains_key("window_gap"),
            "padding missing 'window_gap'"
        );
        assert!(obj.contains_key("up"), "padding missing 'up'");
        assert!(obj.contains_key("down"), "padding missing 'down'");
    }

    #[test]
    fn schema_animation_has_enabled_duration_easing() {
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
            .unwrap_or_else(|| panic!("resolved ref {ref_path} has properties"));

        let obj = anim_props
            .as_object()
            .expect("animation properties is object");

        assert!(obj.contains_key("enabled"));
        assert!(obj.contains_key("duration_ms"));
        assert!(obj.contains_key("easing"));

        // Verify the easing field is represented as an enum (not a bare
        // "string" type).  schemars derives unit enums as `oneOf` where
        // each variant is `type: "string"` with a single-element `enum`
        // constraint.  The animation schema uses $ref to
        // #/definitions/ConfigEasing, so we follow the reference.
        let easing_schema = obj
            .get("easing")
            .expect("easing property exists")
            .as_object()
            .expect("easing property is an object");

        // Follow the allOf/$ref if present (schemars puts enum types behind
        // a reference to the definitions section).
        let easing_def = if let Some(all_of) = easing_schema.get("allOf").and_then(|v| v.as_array())
        {
            let ref_str = all_of
                .iter()
                .find_map(|item| item.get("$ref").and_then(|v| v.as_str()))
                .expect("easing allOf has $ref");
            let ref_path = ref_str.trim_start_matches("#/");
            parsed
                .pointer(&format!("/{ref_path}"))
                .unwrap_or_else(|| panic!("definition {ref_path} exists"))
                .as_object()
                .unwrap_or_else(|| panic!("definition {ref_path} is object"))
        } else {
            easing_schema
        };

        // schemars uses `oneOf` with each variant as a separate object
        assert!(
            easing_def.contains_key("oneOf"),
            "easing schema should have 'oneOf' for enum variants; \
             got keys: {:?}",
            easing_def.keys().collect::<Vec<_>>()
        );

        let one_of = easing_def
            .get("oneOf")
            .expect("oneOf")
            .as_array()
            .expect("oneOf is array");

        // Each oneOf entry should be type "string" with a single-value enum
        let mut enum_strings: Vec<&str> = Vec::new();
        for entry in one_of {
            let entry_obj = entry.as_object().expect("oneOf entry is object");
            assert_eq!(
                entry_obj.get("type").and_then(|v| v.as_str()),
                Some("string"),
                "each oneOf entry should be type 'string'"
            );
            if let Some(enum_vals) = entry_obj.get("enum").and_then(|v| v.as_array()) {
                for val in enum_vals {
                    if let Some(s) = val.as_str() {
                        enum_strings.push(s);
                    }
                }
            }
        }

        // Spot-check known values
        assert!(
            enum_strings.contains(&"linear"),
            "enum should contain 'linear'"
        );
        assert!(
            enum_strings.contains(&"ease-out-expo"),
            "enum should contain 'ease-out-expo'"
        );
        assert!(
            enum_strings.contains(&"ease-in-out-bounce"),
            "enum should contain 'ease-in-out-bounce'"
        );
        // All 31 non-CubicBezier ConfigEasing variants
        assert_eq!(
            enum_strings.len(),
            31,
            "enum should have exactly 31 variants, got: {enum_strings:?}"
        );
    }

    // --- Rules schema tests ---

    #[test]
    fn rules_schema_is_valid_json() {
        let json = generate_rules_schema().expect("rules schema gen");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("schema should be valid JSON");
        assert!(parsed.is_object());
        let obj = parsed.as_object().expect("schema is an object");
        assert!(obj.contains_key("properties"));
    }

    #[test]
    fn rules_schema_has_default_action_and_rules() {
        let json = generate_rules_schema().expect("rules schema gen");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("schema is valid JSON");
        let props = parsed
            .pointer("/properties")
            .expect("has /properties")
            .as_object()
            .expect("properties is object");

        assert!(props.contains_key("default_action"));
        assert!(props.contains_key("rules"));
    }

    #[test]
    fn rules_schema_rules_is_array() {
        let json = generate_rules_schema().expect("rules schema gen");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("schema is valid JSON");
        let rules_type = parsed
            .pointer("/properties/rules")
            .and_then(|v| v.get("type"))
            .expect("rules has a type");
        assert_eq!(rules_type.as_str(), Some("array"));
    }

    #[test]
    fn rules_schema_match_rule_has_regex_fields() {
        let json = generate_rules_schema().expect("rules schema gen");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("schema is valid JSON");

        // Navigate to the MatchRule properties via definitions
        let match_ref = parsed
            .pointer("/definitions/MatchRule/properties")
            .expect("has MatchRule definition with properties");

        let obj = match_ref
            .as_object()
            .expect("MatchRule properties is object");
        assert!(obj.contains_key("exe"));
        assert!(obj.contains_key("exe_regex"));
        assert!(obj.contains_key("title"));
        assert!(obj.contains_key("title_contains"));
        assert!(obj.contains_key("title_regex"));
        assert!(obj.contains_key("class"));
        assert!(obj.contains_key("class_regex"));
        assert!(obj.contains_key("process_path"));
        assert!(obj.contains_key("process_path_regex"));
    }
}
