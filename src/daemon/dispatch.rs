//! IPC command dispatch router and action handlers.
//!
//! This module contains:
//!
//! - [`ScrollTilingManager::dispatch`] — routes each [`SocketMessage`] variant
//!   to the appropriate subsystem.
//! - Individual `dispatch_*` helper methods for each command category.
//! - Helper functions for unimplemented commands.

use crate::common::Direction;
use crate::ipc::message::{SocketMessage, SocketResponse};
use crate::registry::win32 as registry_win32;

use super::types::ScrollTilingManager;

impl ScrollTilingManager {
    /// Dispatch a single IPC command and return the response.
    ///
    /// Routes each [`SocketMessage`] variant to the appropriate subsystem:
    /// - **Stop**: sets the shutdown flag.
    /// - **Layout commands**: call layout engine methods and animate the result.
    /// - **Query commands**: return registry data as JSON.
    /// - **Unimplemented commands**: return an error response.
    pub(super) fn dispatch(&mut self, msg: &SocketMessage) -> SocketResponse {
        match msg {
            // --- Shutdown ---
            SocketMessage::Stop => {
                self.shutting_down = true;
                SocketResponse::Ok
            }

            // --- Focus ---
            SocketMessage::FocusLeft => self.dispatch_focus(Direction::Left),
            SocketMessage::FocusRight => self.dispatch_focus(Direction::Right),
            SocketMessage::FocusUp => self.dispatch_focus(Direction::Up),
            SocketMessage::FocusDown => self.dispatch_focus(Direction::Down),

            // --- Swap (column-level) ---
            SocketMessage::SwapLeft => self.dispatch_swap(Direction::Left),
            SocketMessage::SwapRight => self.dispatch_swap(Direction::Right),
            SocketMessage::SwapUp => self.dispatch_swap(Direction::Up),
            SocketMessage::SwapDown => self.dispatch_swap(Direction::Down),

            // --- Swap with offscreen ---
            SocketMessage::SwapWithOffscreen { direction } => {
                self.dispatch_swap_with_offscreen(*direction)
            }

            // --- Scroll ---
            SocketMessage::ScrollLeft => self.dispatch_scroll_left(),
            SocketMessage::ScrollRight => self.dispatch_scroll_right(),

            // --- Column resize ---
            SocketMessage::ExpandColumn => self.dispatch_expand(),
            SocketMessage::ShrinkColumn => self.dispatch_shrink(),
            SocketMessage::SetColumnWidth { eighths } => self.dispatch_set_column_width(*eighths),

            // --- Window state ---
            SocketMessage::ToggleFloat => unimplemented_command("toggle_float"),
            SocketMessage::ToggleMonocle => self.dispatch_toggle_monocle(),
            SocketMessage::PlaceAbove => unimplemented_command("place_above"),
            SocketMessage::Promote => unimplemented_command("promote"),
            SocketMessage::CloseWindow => unimplemented_command("close_window"),

            // --- Queries ---
            SocketMessage::QueryWindowsAll => self.query_windows_all(),
            SocketMessage::QueryLayoutVirtual => self.query_layout_virtual(),
            SocketMessage::QueryLayoutActual => self.query_layout_actual(),
            SocketMessage::QueryState => unimplemented_command("query_state"),

            // --- Config mutation ---
            SocketMessage::ReloadConfig => unimplemented_command("reload_config"),
            SocketMessage::CheckConfig => unimplemented_command("check_config"),
            SocketMessage::SetConfigValue { .. } => unimplemented_command("set_config_value"),
            SocketMessage::ForgetApp { .. } => unimplemented_command("forget_app"),
            SocketMessage::ForgetAllApps => unimplemented_command("forget_all_apps"),
        }
    }

    /// Dispatch a focus movement in the given direction.
    ///
    /// This is the complete focus pipeline:
    ///
    /// 1. **Layout focus** — [`LayoutEngine::focus`] resolves the neighbor
    ///    [`WindowId`] and optionally shifts the viewport (producing a
    ///    [`LayoutDiff`] when the camera scrolls).
    /// 2. **OS foreground** — [`registry_win32::set_foreground_window`] moves
    ///    the actual Win32 focus to the target window using the
    ///    `AttachThreadInput` trick to bypass foreground-lock restrictions.
    /// 3. **Registry sync** — [`WindowRegistry::set_focused`] updates the
    ///    registry's focus tracking so queries report the correct focused
    ///    window (fixes the `"focused": null` bug).
    /// 4. **Animation** — if the viewport scrolled, [`animate_diff`](Self::animate_diff)
    ///    animates the camera shift so the focused window becomes visible.
    fn dispatch_focus(&mut self, dir: Direction) -> SocketResponse {
        match self.layout.focus(dir) {
            Some((focused_id, diff_opt)) => {
                // 2. Apply OS-level foreground focus to the target window.
                let target_hwnd = focused_id.0;
                if !registry_win32::set_foreground_window(target_hwnd) {
                    log::warn!("dispatch_focus: SetForegroundWindow failed for hwnd {target_hwnd}");
                }

                // 3. Sync the registry's focus tracking.
                self.registry.set_focused(target_hwnd);

                // 4. Animate the camera shift if the viewport moved.
                if let Some(diff) = diff_opt {
                    self.animate_diff(&diff);
                }

                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "no window to focus in that direction".into(),
            },
        }
    }

    /// Dispatch a column swap in the given direction.
    ///
    /// Calls [`LayoutEngine::swap_column`] and animates the resulting
    /// layout diff if the swap succeeded.
    fn dispatch_swap(&mut self, dir: Direction) -> SocketResponse {
        match self.layout.swap_column(dir) {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot swap in that direction".into(),
            },
        }
    }

    /// Dispatch a swap with an offscreen column.
    ///
    /// This command swaps the focused column with the nearest offscreen
    /// column in the given direction. Currently delegates to
    /// [`dispatch_swap`] since the layout engine handles offscreen
    /// swapping transparently via viewport scrolling.
    fn dispatch_swap_with_offscreen(&mut self, direction: Direction) -> SocketResponse {
        // The layout engine's swap_column already handles viewport scrolling
        // when the target is offscreen. This is a thin wrapper for the IPC.
        self.dispatch_swap(direction)
    }

    /// Dispatch a scroll-left command.
    fn dispatch_scroll_left(&mut self) -> SocketResponse {
        match self.layout.scroll_left() {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot scroll left".into(),
            },
        }
    }

    /// Dispatch a scroll-right command.
    fn dispatch_scroll_right(&mut self) -> SocketResponse {
        match self.layout.scroll_right() {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot scroll right".into(),
            },
        }
    }

    /// Dispatch an expand-column command on the focused column.
    fn dispatch_expand(&mut self) -> SocketResponse {
        match self.layout.expand_column() {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot expand column".into(),
            },
        }
    }

    /// Dispatch a shrink-column command on the focused column.
    fn dispatch_shrink(&mut self) -> SocketResponse {
        match self.layout.shrink_column() {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot shrink column".into(),
            },
        }
    }

    /// Dispatch an explicit column width setting.
    ///
    /// Converts the `eighths` value (1–8) to pixel width based on
    /// the resolved `column_width` and passes it to the layout engine.
    fn dispatch_set_column_width(&mut self, eighths: u8) -> SocketResponse {
        if !(1..=8).contains(&eighths) {
            return SocketResponse::Error {
                message: format!("eighths must be 1–8, got {eighths}"),
            };
        }
        let target_px = self.resolved_column_width as i32 * eighths as i32 / 4;
        match self.layout.set_column_width(target_px) {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "no focused window".into(),
            },
        }
    }

    /// Dispatch a monocle mode toggle on the focused column.
    fn dispatch_toggle_monocle(&mut self) -> SocketResponse {
        match self.layout.toggle_monocle() {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot toggle monocle".into(),
            },
        }
    }
}

/// Return a standard "not yet implemented" error response.
fn unimplemented_command(name: &str) -> SocketResponse {
    SocketResponse::Error {
        message: format!("command '{name}' is not yet implemented"),
    }
}
