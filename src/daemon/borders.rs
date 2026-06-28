//! Border overlay lifecycle helpers — bridges the registry's per-window
//! state with per-window [`Border`] overlays.
//!
//! The daemon's hook handlers (`daemon/hooks.rs`) call into these helpers
//! to keep each window's `border` field in sync with its registry state:
//! creating/recoloring on focus and state changes, dropping (→ `None`) on
//! minimize / hide / destroy. See `docs/src/dev-guide/borders.md`.

use windows::Win32::Foundation::HWND;

use crate::borders::{Border, BorderState, BorderStyle, style_for_state};
use crate::common::Rect;
use crate::registry::types::{FloatingState, TilingState, WindowState};

use super::types::ScrollTilingManager;

/// Seat a floating window's border overlay for `visible_rect`.
///
/// A floating window is **not** inset to reserve room for the ring (unlike a
/// tiled window, whose content the animator shrinks by `thickness − overlap`).
/// To land the ring in the same place relative to the content — 2 px gap +
/// `overlap` px overlap, matching a tiled border — the overlay itself must
/// outset by `thickness − overlap`. See the geometry diagram in
/// `docs/src/dev-guide/borders.md`.
///
/// Centralized so the drag path (`store_float_rect`) and the creation path
/// (`refresh_border_for`) agree: without it, a freshly created float border
/// renders with the ring sprawled over the content edge (full `thickness` px
/// overlap) until the next drag, looking thicker and inset versus a tiled
/// border.
pub(super) fn float_border_rect(visible_rect: Rect, thickness: u32, overlap: u32) -> Rect {
    let outset = thickness as i32 - overlap as i32;
    visible_rect.inset(-outset)
}

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
    /// Focus takes priority over tile/float mode: any focused window resolves
    /// to `Focused`. A non-focused tiled window resolves to `Unfocused`; a
    /// non-focused floating window resolves to `Floating`. This is the single
    /// source of truth for border color, so the focus-change hook is the only
    /// path that needs to refresh color — toggle and drag never special-case it.
    pub(super) fn border_style_for(&self, hwnd: isize) -> Option<BorderStyle> {
        let window = self.registry.get_window(HWND(hwnd as *mut _))?;
        let is_focused = self.registry.focused() == Some(crate::common::WindowId(hwnd));
        let state = match window.state {
            WindowState::Tiling(TilingState::Active { .. }) => {
                if is_focused {
                    BorderState::Focused
                } else {
                    BorderState::Unfocused
                }
            }
            WindowState::Floating(FloatingState::Active { .. }) => {
                if is_focused {
                    BorderState::Focused
                } else {
                    BorderState::Floating
                }
            }
            // Minimized / Hidden / Ignored: no overlay.
            WindowState::Tiling(TilingState::Minimized | TilingState::Hidden)
            | WindowState::Floating(FloatingState::Minimized | FloatingState::Hidden)
            | WindowState::Ignored(_) => return None,
        };
        let mut style = style_for_state(&self.config.borders, state);
        // Reuse the existing border's cached corner preference when one is
        // already attached; only pay the DWM round-trip on the creation path
        // (border == None). A window's corner rounding never changes at
        // runtime, so this turns a per-focus-change `DwmGetWindowAttribute`
        // into a cheap field read. See `Border::corner_preference`.
        // See `docs/src/dev-guide/borders.md`.
        style.corner_preference = window
            .border
            .as_ref()
            .map(|b| b.corner_preference())
            .unwrap_or_else(|| {
                crate::registry::win32::get_window_corner_preference(HWND(hwnd as *mut _))
                    .unwrap_or_default()
            });
        Some(style)
    }

    /// Re-sync a single window's border overlay against its current registry
    /// state. Creates or recolors when [`border_style_for`](Self::border_style_for)
    /// returns `Some`, drops (sets to `None`) when it returns `None`. Idempotent.
    pub(super) fn refresh_border_for(&mut self, hwnd: isize) {
        let desired = self.border_style_for(hwnd);
        // Read the border dims before the mutable registry borrow below: the
        // float-creation and float-reseat arms need them to outset the rect,
        // and re-borrowing `self.config` while `window` (from `get_window_mut`)
        // is live would conflict.
        let (thickness, overlap) = (self.config.borders.thickness, self.config.borders.overlap);
        let Some(window) = self.registry.get_window_mut(HWND(hwnd as *mut _)) else {
            return;
        };
        match desired {
            None => {
                window.border = None;
            }
            Some(style) => {
                if let Some(border) = window.border.as_ref() {
                    // Already has a border: re-assert z-order (focus and other
                    // events can raise the target above its own overlay, which
                    // would hide the border), recolor, and — for floats —
                    // re-seat the overlay geometry. Tiled geometry is owned by
                    // the animator (it moves tile overlays each frame), so it
                    // is intentionally not touched here; re-seating a tile
                    // overlay mid-animation would fight the animator.
                    let geo = match &window.state {
                        WindowState::Floating(FloatingState::Active { rect }) => {
                            Some(float_border_rect(*rect, thickness, overlap))
                        }
                        _ => None,
                    };
                    border.seat_above_target();
                    if let Some(rect) = geo {
                        border.set_geometry(rect);
                    }
                    border.set_style(style);
                } else {
                    // New border: create it and sync to the window's current
                    // position so it doesn't flash at (0,0) before the next
                    // animation frame positions it. `hwnd` is the target the
                    // overlay is seated just above in z-order.
                    let current_rect = match &window.state {
                        WindowState::Tiling(TilingState::Active { .. }) => window.tiled_rect,
                        WindowState::Floating(FloatingState::Active { rect }) => {
                            Some(float_border_rect(*rect, thickness, overlap))
                        }
                        _ => None,
                    };
                    match Border::create(style, hwnd) {
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
    /// attach borders for windows found during the initial scan. Per-event
    /// recoloring (focus changes, creates, etc.) goes through
    /// [`refresh_border_for`](Self::refresh_border_for) for the affected
    /// windows, not this full sweep.
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
