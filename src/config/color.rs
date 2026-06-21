//! 24-bit RGB color used by border styling.
//!
//! Serialized as a hex string with a `#` prefix in TOML, e.g. `"#FF8800"`.
//! Parsing is strict: lowercase/uppercase both accepted on input, but
//! serialization always emits uppercase hex.

use schemars::schema::Schema;
use schemars::{JsonSchema, SchemaGenerator};
use serde::{Deserialize, Serialize};
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
    /// hex notation. (`docs/src/dev-guide/borders.md` will cover this.)
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
    fn schema_name() -> String {
        "Color".to_string()
    }

    fn json_schema(_gen: &mut SchemaGenerator) -> Schema {
        serde_json::from_value(serde_json::json!({
            "type": "string",
            "pattern": "^#[0-9A-Fa-f]{6}$",
            "format": "hex-color",
            "description": "RGB hex color, e.g. \"#FF8800\"."
        }))
        .expect("color schema is a valid JSON Schema object")
    }

    fn is_referenceable() -> bool {
        false
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

    /// Tiny wrapper used only so `toml::to_string` has a top-level table to
    /// serialize — `Color` itself serializes to a bare string.
    #[derive(Debug, Serialize, Deserialize)]
    struct BorrowedColor {
        color: Color,
    }
}
