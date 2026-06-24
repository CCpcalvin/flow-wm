//! Window border overlay engine — komorebi/Hyprland-style colored borders.
//!
//! Each managed window can own a [`Border`] — a click-through, layered
//! overlay window that draws a thin colored ring around the window's visible
//! content edge. The border lives directly on the registry's [`Window`]
//! struct as `Option<Border>`, so its Win32 lifecycle is coupled to the
//! window's own (the overlay `HWND` is destroyed when the `Window` is
//! dropped).
//!
//! # Positioning model
//!
//! Unlike the previous design (which ran a private `EVENT_OBJECT_LOCATIONCHANGE`
//! hook to follow the target HWND), the daemon now **commands** the border's
//! position:
//!
//! - **Tiled windows** — the border is flattened into the animator's target
//!   list alongside the window itself, so it moves in lockstep during
//!   animations. No extra hook traffic.
//! - **Floating windows** — the existing `EVENT_OBJECT_LOCATIONCHANGE` path
//!   (shared with float-rect tracking) calls `Border::set_geometry` after
//!   updating the registry.
//!
//! This eliminates the duplicate location-change hook and the process-global
//! `OnceLock` indirection. See `docs/src/dev-guide/borders.md`.
//!
//! [`Window`]: crate::registry::types::Window

pub(crate) mod border;
pub(crate) mod style;

pub use border::Border;
pub use style::{BorderStyle, CornerPreference};

use crate::config::BorderConfig;

/// Convert a config-layer [`BorderConfig`] into a per-window [`BorderStyle`]
/// for the given semantic state.
///
/// The daemon knows the window's state (focused / unfocused / floating) and
/// calls this helper to resolve the user-configured color before passing the
/// resulting [`BorderStyle`] to [`Border::set_style`].
///
/// (Phase 4 will wire this into the daemon; for now this is the contract.)
#[must_use]
pub fn style_for_state(cfg: &BorderConfig, state: BorderState) -> BorderStyle {
    let color = match state {
        BorderState::Focused => cfg.focused_color,
        BorderState::Unfocused => cfg.unfocused_color,
        BorderState::Floating => cfg.floating_color,
    };
    BorderStyle {
        color,
        width_px: cfg.thickness,
        corner_preference: CornerPreference::default(),
    }
}

/// Semantic per-window state used to resolve a [`BorderStyle`] from
/// [`BorderConfig`].
///
/// The daemon maps its internal `WindowState` enum onto this much smaller
/// enum — the border crate only cares about which color bucket applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderState {
    /// The focused/active window — receives `focused_color`.
    Focused,
    /// A tiled-but-not-focused window — receives `unfocused_color`.
    Unfocused,
    /// A floating window — receives `floating_color`.
    Floating,
}
