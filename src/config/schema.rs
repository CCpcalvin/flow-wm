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

        // schemars v1 (Draft 2020-12) emits a direct `$ref` here; v0.8 wrapped
        // it in `allOf` because Draft 7 forbade sibling keys alongside $ref.
        let ref_path = padding_schema
            .get("$ref")
            .and_then(|v| v.as_str())
            .expect("padding has $ref");
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

        // See note in schema_padding_has_window_up_down: v1 emits direct $ref.
        let ref_path = anim_schema
            .get("$ref")
            .and_then(|v| v.as_str())
            .expect("animation has $ref");
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

        // Verify the easing field is represented as an enum, not a bare
        // string. Follow the $ref to #/$defs/ConfigEasing to inspect its
        // oneOf variants.
        let easing_schema = obj
            .get("easing")
            .expect("easing property exists")
            .as_object()
            .expect("easing property is an object");

        // The `easing` field is a direct `$ref` to `#/$defs/ConfigEasing`
        // (v1 direct ref; v0.8 wrapped it in allOf). Follow it to the def.
        let easing_ref = easing_schema
            .get("$ref")
            .and_then(|v| v.as_str())
            .expect("easing has $ref");
        let easing_ref_path = easing_ref.trim_start_matches("#/");
        let easing_def = parsed
            .pointer(&format!("/{easing_ref_path}"))
            .unwrap_or_else(|| panic!("definition {easing_ref_path} exists"))
            .as_object()
            .unwrap_or_else(|| panic!("definition {easing_ref_path} is object"));

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

        // v1 represents each unit-enum variant as `{"const": "variant"}`
        // (Draft 2020-12 `const` keyword); v0.8 used `{"type":"string","enum":["x"]}`.
        let mut enum_strings: Vec<&str> = Vec::new();
        for entry in one_of {
            let entry_obj = entry.as_object().expect("oneOf entry is object");
            if let Some(s) = entry_obj.get("const").and_then(|v| v.as_str()) {
                enum_strings.push(s);
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

    #[test]
    fn schema_color_fields_are_inlined_not_refd() {
        // schemars v1: `Color::inline_schema()` returns `true`, so color
        // fields appear inline (type + pattern + format) rather than as a
        // `$ref` to `#/$defs/Color`. v0.8 expressed this as
        // `is_referenceable() = false`. Regression test for the migration.
        // Arrange
        let json = generate_config_schema().expect("schema gen");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("schema is valid JSON");

        // Assert (negative): with inline_schema = true, no Color entry lands
        // in $defs.
        if let Some(defs) = parsed.pointer("/$defs").and_then(|v| v.as_object()) {
            assert!(
                !defs.contains_key("Color"),
                "Color should be inlined (inline_schema=true), but #/$defs/Color exists"
            );
        }

        // Act: `borders` is a normal referenceable struct -> direct $ref (v1).
        let borders_ref = parsed
            .pointer("/properties/borders")
            .and_then(|v| v.get("$ref"))
            .and_then(|v| v.as_str())
            .expect("borders has $ref");
        let borders_path = borders_ref.trim_start_matches("#/");

        // Assert (positive): focused_color is a Color field -> inlined, not
        // $ref'd, with the full hex-color string shape present directly.
        let focused = parsed
            .pointer(&format!("/{borders_path}/properties/focused_color"))
            .expect("focused_color schema exists");
        let focused_obj = focused.as_object().expect("focused_color is an object");

        assert!(
            !focused_obj.contains_key("$ref"),
            "inline_schema=true must not emit a $ref for Color fields"
        );
        assert_eq!(
            focused_obj.get("type").and_then(|v| v.as_str()),
            Some("string"),
            "inlined Color field should be type:string, got: {focused:?}"
        );
        assert_eq!(
            focused_obj.get("pattern").and_then(|v| v.as_str()),
            Some("^#[0-9A-Fa-f]{6}$")
        );
        assert_eq!(
            focused_obj.get("format").and_then(|v| v.as_str()),
            Some("hex-color")
        );
    }

    #[test]
    fn all_borders_color_fields_are_inlined() {
        // Complements `schema_color_fields_are_inlined_not_refd` (which deep-
        // dives `focused_color`) by sweeping ALL three Color fields declared
        // in `BorderConfig` (`focused_color`, `unfocused_color`,
        // `floating_color`). A regression that re-introduces a `$ref` for any
        // single field must fail this test.
        // Arrange
        let json = generate_config_schema().expect("schema gen");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("schema is valid JSON");
        // Act: borders is a referenceable struct -> direct `$ref` in v1.
        let borders_ref = parsed
            .pointer("/properties/borders")
            .and_then(|v| v.get("$ref"))
            .and_then(|v| v.as_str())
            .expect("borders has $ref");
        let borders_path = borders_ref.trim_start_matches("#/");
        let borders_props = parsed
            .pointer(&format!("/{borders_path}/properties"))
            .and_then(|v| v.as_object())
            .expect("borders struct has properties");
        // Assert: every Color field in BorderConfig must be inlined with the
        // full hex-color string shape (no $ref) because `Color::inline_schema`
        // returns true.
        for field in ["focused_color", "unfocused_color", "floating_color"] {
            let field_schema = borders_props
                .get(field)
                .unwrap_or_else(|| panic!("borders has {field}"));
            let obj = field_schema
                .as_object()
                .unwrap_or_else(|| panic!("{field} schema is an object"));
            assert!(
                !obj.contains_key("$ref"),
                "{field} should be inlined (inline_schema=true), got: {field_schema}"
            );
            assert_eq!(
                obj.get("type").and_then(|v| v.as_str()),
                Some("string"),
                "{field} should be type:string, got: {field_schema}"
            );
            assert_eq!(
                obj.get("pattern").and_then(|v| v.as_str()),
                Some("^#[0-9A-Fa-f]{6}$"),
                "{field} should carry the hex pattern, got: {field_schema}"
            );
            assert_eq!(
                obj.get("format").and_then(|v| v.as_str()),
                Some("hex-color"),
                "{field} should carry the hex-color format, got: {field_schema}"
            );
        }
    }

    #[test]
    fn schema_meta_schema_is_draft_2020_12() {
        // schemars v1 emits the JSON Schema Draft 2020-12 meta-schema URI;
        // v0.8 emitted the Draft 7 URI. Trivial, stable migration guard.
        // Arrange + Act
        let json = generate_config_schema().expect("schema gen");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("schema is valid JSON");
        let meta = parsed
            .get("$schema")
            .and_then(|v| v.as_str())
            .expect("schema has $schema");
        // Assert
        assert!(
            meta.contains("2020-12"),
            "expected Draft 2020-12 $schema URI, got: {meta}"
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

        // Navigate to the MatchRule properties via $defs (Draft 2020-12).
        // v0.8 schemars used the Draft 7 key "definitions"; v1 uses "$defs".
        let match_ref = parsed
            .pointer("/$defs/MatchRule/properties")
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
