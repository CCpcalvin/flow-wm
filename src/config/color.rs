//! 24-bit RGB color used by border styling.
//!
//! Serialized as a hex string with a `#` prefix in TOML, e.g. `"#FF8800"`.
//! Parsing is strict: lowercase/uppercase both accepted on input, but
//! serialization always emits uppercase hex.

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

/// 24-bit RGB color (no alpha) used by border styling.
///
/// Serialized as a hex string of the form `"#RRGGBB"` in TOML. The string
/// form is what the user writes; the parsed struct is what the daemon passes
/// to the border crate.
///
/// # Examples
///
/// ```toml
/// focused_color = "#00AAFF"
/// unfocused_color = "#555555"
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Color {
    /// Red channel (0–255).
    pub r: u8,
    /// Green channel (0–255).
    pub g: u8,
    /// Blue channel (0–255).
    pub b: u8,
}

impl Color {
    /// Construct a color from RGB components.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Convert to a `COLORREF` (`0x00BBGGRR`) for direct use with GDI.
    ///
    /// Win32 `RGB()` macro and most GDI APIs pack channels as
    /// low-order-byte red, mid green, high blue — the reverse of the usual
    /// hex notation. See `docs/src/dev-guide/borders.md`.
    #[must_use]
    pub const fn to_colorref(self) -> u32 {
        (self.r as u32) | ((self.g as u32) << 8) | ((self.b as u32) << 16)
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

impl FromStr for Color {
    type Err = String;

    /// Parse a `#RRGGBB` hex string. Case-insensitive on input.
    ///
    /// # Errors
    ///
    /// Returns a human-readable `String` if the input is missing the `#`
    /// prefix, is not exactly six hex digits, or contains a non-hex digit.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s
            .strip_prefix('#')
            .ok_or_else(|| format!("color must start with '#': got {s:?}"))?;
        if hex.len() != 6 {
            return Err(format!(
                "color must be 6 hex digits (#RRGGBB): got {s:?} ({} chars after '#')",
                hex.len()
            ));
        }
        let r = u8::from_str_radix(&hex[0..2], 16)
            .map_err(|e| format!("invalid red channel in {s:?}: {e}"))?;
        let g = u8::from_str_radix(&hex[2..4], 16)
            .map_err(|e| format!("invalid green channel in {s:?}: {e}"))?;
        let b = u8::from_str_radix(&hex[4..6], 16)
            .map_err(|e| format!("invalid blue channel in {s:?}: {e}"))?;
        Ok(Self::rgb(r, g, b))
    }
}

impl Serialize for Color {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Color {
    fn schema_name() -> Cow<'static, str> {
        "Color".into()
    }

    /// Include the module path so `Color` does not collide with same-named
    /// types in other crates when both contribute JSON Schemas to the same
    /// `$defs` map.
    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::Color").into()
    }

    fn json_schema(_gen: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^#[0-9A-Fa-f]{6}$",
            "format": "hex-color",
            "description": "RGB hex color, e.g. \"#FF8800\"."
        })
    }

    /// Inline the schema rather than emit a `$ref` — `Color` is a leaf scalar with no reuse.
    fn inline_schema() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uppercase_hex() {
        assert_eq!(
            "#FF8800".parse::<Color>().unwrap(),
            Color::rgb(0xFF, 0x88, 0x00)
        );
    }

    #[test]
    fn parse_lowercase_hex() {
        assert_eq!(
            "#ff8800".parse::<Color>().unwrap(),
            Color::rgb(0xFF, 0x88, 0x00)
        );
    }

    #[test]
    fn parse_rejects_missing_hash() {
        assert!("FF8800".parse::<Color>().is_err());
    }

    #[test]
    fn parse_rejects_short_string() {
        assert!("#FF88".parse::<Color>().is_err());
    }

    #[test]
    fn parse_rejects_non_hex_digit() {
        assert!("#GG8800".parse::<Color>().is_err());
    }

    #[test]
    fn display_emits_uppercase_with_hash() {
        assert_eq!(Color::rgb(0xFF, 0x88, 0x00).to_string(), "#FF8800");
    }

    #[test]
    fn to_colorref_packs_bgr() {
        // GDI COLORREF is 0x00BBGGRR — note the channel order swap.
        assert_eq!(Color::rgb(0xFF, 0x88, 0x00).to_colorref(), 0x00_00_88_FF);
    }

    #[test]
    fn roundtrips_through_toml() {
        let original = Color::rgb(0x12, 0x34, 0x56);
        let s = toml::to_string(&BorrowedColor { color: original }).unwrap();
        let parsed: BorrowedColor = toml::from_str(&s).unwrap();
        assert_eq!(parsed.color, original);
    }

    #[test]
    fn serde_rejects_invalid_string() {
        let bad = toml::from_str::<BorrowedColor>("color = \"magenta\"\n");
        assert!(bad.is_err());
    }

    #[test]
    fn json_schema_emits_v1_inline_string_shape() {
        // Regression test for the schemars v0.8 -> v1 port of the custom
        // `JsonSchema for Color` impl (the only hand-written impl touched by
        // the migration). The `json_schema!` macro must produce a Draft
        // 2020-12 string schema carrying the hex-color pattern and format.
        // Arrange
        let mut generator = SchemaGenerator::default();
        // Act
        let schema = Color::json_schema(&mut generator);
        let value = serde_json::to_value(&schema).expect("schema serializes");
        let obj = value.as_object().expect("schema is an object");
        // Assert
        assert_eq!(
            obj.get("type").and_then(|v| v.as_str()),
            Some("string"),
            "Color schema type should be 'string'"
        );
        assert_eq!(
            obj.get("pattern").and_then(|v| v.as_str()),
            Some("^#[0-9A-Fa-f]{6}$"),
            "Color schema should carry the hex pattern"
        );
        assert_eq!(
            obj.get("format").and_then(|v| v.as_str()),
            Some("hex-color"),
            "Color schema should carry the 'hex-color' format"
        );
    }

    #[test]
    fn json_schema_trait_methods_return_v1_contracts() {
        // schemars v1 renamed and reshaped the `JsonSchema` trait:
        // `schema_name`/`schema_id` now return `Cow<'static, str>`, and
        // `is_referenceable() -> bool` was INVERTED to `inline_schema() -> bool`.
        // Lock each contract so a partial revert to v0.8 surfaces here rather
        // than silently producing wrong schemas downstream.
        // Arrange + Act
        let name = Color::schema_name();
        let id = Color::schema_id();
        let inline = Color::inline_schema();
        // Assert
        assert_eq!(name.as_ref(), "Color", "schema_name must be 'Color'");
        assert_ne!(
            name.as_ref(),
            id.as_ref(),
            "schema_id should be module-qualified to disambiguate same-named types across crates, got id = {id}"
        );
        assert!(
            id.as_ref().ends_with("::Color"),
            "schema_id should end with '::Color', got: {id}"
        );
        assert!(
            inline,
            "inline_schema must be true (v1); corresponds to is_referenceable=false in v0.8"
        );
    }

    /// Tiny wrapper used only so `toml::to_string` has a top-level table to
    /// serialize — `Color` itself serializes to a bare string.
    #[derive(Debug, Serialize, Deserialize)]
    struct BorrowedColor {
        color: Color,
    }
}
