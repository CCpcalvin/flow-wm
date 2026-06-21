//! Window border overlay engine — komorebi/Hyprland-style colored borders.
//!
//! This module draws a thin colored ring around each managed window as a
//! separate layered overlay window. The overlay **follows the target HWND's
//! actual on-screen geometry** — driven by `EVENT_OBJECT_LOCATIONCHANGE` —
//! rather than the daemon's intended rect. This makes borders robust to
//! daemon lag, freeze, or crash: the border keeps tracking the real window
//! until the application detaches it or the HWND dies.
//!
//! # Architecture
//!
//! [`BorderManager`] owns:
//!
//! - A background hook thread subscribed to `EVENT_OBJECT_LOCATIONCHANGE`
//!   only. (The daemon's hook thread in `registry/hooks.rs` deliberately
//!   excludes this event because it would flood the IPC channel; the border
//!   crate installs its own independent hook on its own thread.)
//! - A `Mutex<HashMap<HWND, BorderOverlay>>` mapping target HWNDs to their
//!   overlay windows. Both the hook thread (sync-on-LOCATIONCHANGE) and the
//!   IPC thread (`attach`/`detach`/`set_style`) touch this map.
//!
//! # Threading
//!
//! Because `SetWinEventHook` callbacks cannot take userdata, the hook
//! callback reaches the manager through a process-global
//! `OnceLock<Arc<BorderManagerInner>>`. This limits the crate to one
//! `BorderManager` per process, which is fine — the daemon is the only
//! intended user.
//!
//! See `docs/src/dev-guide/borders.md` for design rationale and the
//! "follow HWND, not intent" principle.

pub(crate) mod manager;
pub(crate) mod overlay;
pub(crate) mod style;

pub use manager::BorderManager;
pub use style::{BorderStyle, CornerPreference};

use crate::config::BorderConfig;

/// Convert a config-layer [`BorderConfig`] into a per-window [`BorderStyle`]
/// for the given semantic state.
///
/// The daemon knows the window's state (focused / unfocused / floating) and
/// calls this helper to resolve the user-configured color before passing the
/// resulting [`BorderStyle`] to [`BorderManager::set_style`].
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
