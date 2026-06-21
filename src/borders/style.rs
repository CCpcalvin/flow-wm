//! Style types passed from the daemon to the border crate.

use crate::config::Color;

/// Per-window border styling.
///
/// Constructed by the daemon (typically via [`crate::style_for_state`]) and
/// passed to [`crate::BorderManager::attach`] /
/// [`crate::BorderManager::set_style`]. The crate reads these values each
/// time it redraws the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderStyle {
    /// Border ring color.
    pub color: Color,
    /// Border ring thickness in pixels, applied uniformly on all four sides.
    pub width_px: u32,
    /// Corner rounding preference (v1: always default; reserved for future
    /// DWM corner APIs).
    pub corner_preference: CornerPreference,
}

impl BorderStyle {
    /// Construct a new style from the three independent values.
    #[must_use]
    pub const fn new(color: Color, width_px: u32, corner_preference: CornerPreference) -> Self {
        Self {
            color,
            width_px,
            corner_preference,
        }
    }
}

/// Corner shape preference for an overlay window.
///
/// v1 always uses the system default — the overlay draws a rectangular ring
/// via GDI regardless of this value. The enum is in place so future versions
/// can pass it through to `DwmSetWindowAttribute(DWMWA_WINDOW_CORNER_PREFERENCE)`
/// without changing the public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CornerPreference {
    /// Let Windows decide (typically rounded on Win11).
    #[default]
    Default,
    /// Square corners (don't round).
    Square,
    /// Rounded corners.
    Rounded,
    /// Small rounded corners.
    RoundedSmall,
}
