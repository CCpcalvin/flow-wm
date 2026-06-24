//! Border overlay lifecycle helpers — bridges the registry's per-window
//! state with per-window [`Border`] overlays.
//!
//! The daemon's hook handlers (`daemon/hooks.rs`) call into these helpers
//! to keep each window's `border` field in sync with its registry state:
//! creating/recoloring on focus and state changes, dropping (→ `None`) on
//! minimize / hide / destroy. See `docs/src/dev-guide/borders.md`.

use windows::Win32::Foundation::HWND;

use crate::borders::{Border, BorderState, BorderStyle, style_for_state};
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
    ///   destroyed and recreated on the next restore/show. This avoids
    ///   creating overlays for windows the user can't see.
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
    /// state. Creates or recolors when [`border_style_for`](Self::border_style_for)
    /// returns `Some`, drops (sets to `None`) when it returns `None`. Idempotent.
    pub(super) fn refresh_border_for(&mut self, hwnd: isize) {
        let desired = self.border_style_for(hwnd);
        let Some(window) = self.registry.get_window_mut(HWND(hwnd as *mut _)) else {
            return;
        };
        match desired {
            None => {
                window.border = None;
            }
            Some(style) => {
                if let Some(border) = window.border.as_ref() {
                    // Already has a border: just recolor.
                    border.set_style(style);
                } else {
                    // New border: create it and sync to the window's current
                    // position so it doesn't flash at (0,0) before the next
                    // animation frame positions it.
                    let current_rect = match &window.state {
                        WindowState::Tiling(TilingState::Active { .. }) => window.tiled_rect,
                        WindowState::Floating(FloatingState::Active { rect }) => Some(*rect),
                        _ => None,
                    };
                    match Border::create(style) {
                        Ok(b) => {
                            if let Some(rect) = current_rect {
                                b.set_geometry(rect);
                            }
                            window.border = Some(b);
                        }
                        Err(e) => {
                            log::error!("failed to create border overlay for hwnd {hwnd}: {e}");
                        }
                    }
                }
            }
        }
    }

    /// Re-sync every tracked window's border overlay.
    ///
    /// Called once at the end of [`ScrollTilingManager::new`](Self::new) to
    /// attach borders for windows found during the initial scan, and on every
    /// focus change to recolor overlays (the focused-vs-unfocused distinction
    /// is the only per-focus state).
    ///
    /// Snapshots the hwnd list before mutating so we don't hold a registry
    /// borrow across the per-window `get_window_mut` calls.
    pub(super) fn refresh_all_border_styles(&mut self) {
        let snapshot: Vec<isize> = self.registry.windows().map(|w| w.hwnd.0 as isize).collect();
        for raw in snapshot {
            self.refresh_border_for(raw);
        }
    }
}
