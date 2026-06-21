//! Border overlay lifecycle helpers — bridges the registry's per-window
//! state with the [`BorderManager`] subsystem.
//!
//! The daemon's hook handlers (`daemon/hooks.rs`) call into these helpers
//! to keep the border overlay set in sync with the registry: attaching on
//! creation, detaching on destroy / minimize / hide, recoloring on focus
//! change. The design rationale ("follow HWND, not intent") and the threading
//! model are planned for `docs/src/dev-guide/borders.md`.

use windows::Win32::Foundation::HWND;

use crate::borders::{BorderState, BorderStyle, style_for_state};
use crate::registry::types::{FloatingState, TilingState, WindowState};

use super::types::ScrollTilingManager;

impl ScrollTilingManager {
    /// Resolve the appropriate border style for `hwnd` based on its current
    /// registry state and OS focus tracking.
    ///
    /// Returns `None` when the window should NOT currently have a visible
    /// overlay:
    /// - **Ignored** windows never get an overlay.
    /// - **Minimized / Hidden** windows are detached — the overlay is
    ///   destroyed and recreated on the next restore/show. This keeps the
    ///   border hook thread's HWND map focused on actually-visible windows
    ///   so it does no work for tray-hidden apps.
    ///
    /// Active tiled windows resolve to `Focused` or `Unfocused` based on the
    /// registry's tracked OS focus. Floating windows always resolve to
    /// `Floating` regardless of focus (komorebi convention).
    pub(super) fn border_style_for(&self, hwnd: isize) -> Option<BorderStyle> {
        let window = self.registry.get_window(HWND(hwnd as *mut _))?;
        let state = match window.state {
            WindowState::Tiling(TilingState::Active { .. }) => {
                if self.registry.focused() == Some(crate::common::WindowId(hwnd)) {
                    BorderState::Focused
                } else {
                    BorderState::Unfocused
                }
            }
            WindowState::Floating(FloatingState::Active { .. }) => BorderState::Floating,
            // Minimized / Hidden / Ignored: no overlay.
            WindowState::Tiling(TilingState::Minimized | TilingState::Hidden)
            | WindowState::Floating(FloatingState::Minimized | FloatingState::Hidden)
            | WindowState::Ignored(_) => return None,
        };
        Some(style_for_state(&self.config.borders, state))
    }

    /// Re-sync a single window's border overlay against its current registry
    /// state. Attaches (or recolors) when [`border_style_for`](Self::border_style_for)
    /// returns `Some`, detaches when it returns `None`. Idempotent.
    pub(super) fn refresh_border_for(&self, hwnd: isize) {
        let target = HWND(hwnd as *mut _);
        match self.border_style_for(hwnd) {
            Some(style) => self.borders.attach(target, style),
            None => self.borders.detach(target),
        }
    }

    /// Re-sync every tracked window's border overlay.
    ///
    /// Called once at the end of [`ScrollTilingManager::new`](Self::new) to
    /// attach borders for windows found during the initial scan, and on every
    /// focus change to recolor overlays (the focused-vs-unfocused distinction
    /// is the only per-focus state).
    ///
    /// Snapshots the hwnd list before touching the border manager so we don't
    /// hold a registry borrow across the (separately locked) overlay map.
    pub(super) fn refresh_all_border_styles(&self) {
        let snapshot: Vec<isize> = self.registry.windows().map(|w| w.hwnd.0 as isize).collect();
        for raw in snapshot {
            self.refresh_border_for(raw);
        }
    }
}
