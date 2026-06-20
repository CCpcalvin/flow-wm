//! IPC command dispatch router and action handlers.
//!
//! This module contains:
//!
//! - [`ScrollTilingManager::dispatch`] — routes each [`SocketMessage`] variant
//!   to the appropriate subsystem.
//! - Individual `dispatch_*` helper methods for each command category.
//! - Helper functions for unimplemented commands.

use crate::common::{Direction, WindowId};
use crate::ipc::message::{SocketMessage, SocketResponse};
use crate::layout::types::ActualLayout;
use crate::registry::win32 as registry_win32;
use crate::workspace::{WorkspaceId, workspace_y_offset};

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
        log::debug!("ipc: dispatching {msg:?}");
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

            // --- Swap (per-window) ---
            // Swap* operates on the focused window, exchanging it with its
            // neighbour. Up/Down move within the same column; Left/Right
            // exchange it with a window in the adjacent column.
            SocketMessage::SwapLeft => self.dispatch_swap_window(Direction::Left),
            SocketMessage::SwapRight => self.dispatch_swap_window(Direction::Right),
            SocketMessage::SwapUp => self.dispatch_swap_window(Direction::Up),
            SocketMessage::SwapDown => self.dispatch_swap_window(Direction::Down),

            // --- Column swap ---
            SocketMessage::SwapColumn { direction } => self.dispatch_swap_column(*direction),

            // --- Semantic move ---
            SocketMessage::MoveWindow { direction } => self.dispatch_move_window(*direction),

            // --- Scroll ---
            SocketMessage::ScrollLeft => self.dispatch_scroll_left(),
            SocketMessage::ScrollRight => self.dispatch_scroll_right(),

            // --- Column resize ---
            SocketMessage::ExpandColumn => self.dispatch_expand(),
            SocketMessage::ShrinkColumn => self.dispatch_shrink(),
            SocketMessage::SetColumnWidth { width_px } => self.dispatch_set_column_width(*width_px),

            // --- Viewport center ---
            SocketMessage::Center => self.dispatch_center_absolute(),
            SocketMessage::CenterGrid => self.dispatch_center_grid(),

            // --- Window state ---
            SocketMessage::ToggleFloat => unimplemented_command("toggle_float"),
            SocketMessage::ToggleMonocle => self.dispatch_toggle_monocle(),
            SocketMessage::PlaceAbove => unimplemented_command("place_above"),
            SocketMessage::Promote => unimplemented_command("promote"),
            SocketMessage::CloseWindow => self.dispatch_close_window(),

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

            // --- Workspace ---
            //
            // SwitchWorkspace and MoveWindowToWorkspace animate the
            // participating workspaces (see [`dispatch_switch_workspace`]
            // and [`dispatch_move_window_to_workspace`]). SwapWorkspace is
            // still pending — its animation model is undecided.
            SocketMessage::SwitchWorkspace { workspace_id } => {
                self.dispatch_switch_workspace(*workspace_id)
            }
            SocketMessage::SwapWorkspace { .. } => unimplemented_command("swap_workspace"),
            SocketMessage::MoveWindowToWorkspace { workspace_id } => {
                self.dispatch_move_window_to_workspace(*workspace_id)
            }
        }
    }

    /// Dispatch a focus movement in the given direction.
    ///
    /// This is the complete focus pipeline:
    ///
    /// 1. **Layout focus** — [`ScrollingSpace::focus`](crate::workspace::ScrollingSpace::focus) resolves the neighbor
    ///    [`WindowId`] and optionally shifts the viewport (producing a
    ///    [`AppliedLayout`] when the camera scrolls).
    /// 2. **OS foreground** — [`registry_win32::set_foreground_window`] moves
    ///    the actual Win32 focus to the target window using the
    ///    `AttachThreadInput` trick to bypass foreground-lock restrictions.
    /// 3. **Registry sync** — [`WindowRegistry::set_focused`] updates the
    ///    registry's focus tracking so queries report the correct focused
    ///    window (fixes the `"focused": null` bug).
    /// 4. **Animation** — if the viewport scrolled, [`animate_layout`](Self::animate_layout)
    ///    animates the camera shift so the focused window becomes visible.
    fn dispatch_focus(&mut self, dir: Direction) -> SocketResponse {
        match self.active_scrolling_mut().focus(dir) {
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
                    self.animate_layout(&diff);
                }

                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "no window to focus in that direction".into(),
            },
        }
    }

    /// Close the currently focused window.
    ///
    /// Resolves the focused [`WindowId`](crate::common::WindowId) from the
    /// layout engine and asks Win32 to close that window gently via
    /// `WM_CLOSE` (see [`registry_win32::close_window`] for why this is
    /// preferred over `DestroyWindow`).
    ///
    /// # Why No Layout Mutation Here?
    ///
    /// Closing is **asynchronous**: [`registry_win32::close_window`] only
    /// *queues* a `WM_CLOSE` message and returns immediately. The window
    /// disappears later, once the owning application processes the message
    /// and destroys the window. At that point Win32 fires
    /// `EVENT_OBJECT_DESTROY`, which the registry hook turns into a
    /// `Destroyed` event — the daemon's normal event loop then removes the
    /// window from both the registry and the layout engine, and animates the
    /// gap closing. Mutating the layout here would race that pipeline and
    /// risk a double-remove.
    ///
    /// # Error Cases
    ///
    /// - No focused window (empty workspace) → error, surfaced so the CLI
    ///   can report it.
    /// - `WM_CLOSE` could not be queued (the window vanished mid-call,
    ///   etc.) → error.
    fn dispatch_close_window(&mut self) -> SocketResponse {
        let Some(focused) = self.active_scrolling().focused() else {
            return SocketResponse::Error {
                message: "no focused window to close".into(),
            };
        };
        let target_hwnd = focused.0;
        if registry_win32::close_window(target_hwnd) {
            SocketResponse::Ok
        } else {
            log::warn!("dispatch_close_window: close_window failed for hwnd {target_hwnd}");
            SocketResponse::Error {
                message: "failed to request window close".into(),
            }
        }
    }

    /// Dispatch a **column** swap in the given direction.
    ///
    /// Calls [`ScrollingSpace::swap_column`](crate::workspace::ScrollingSpace::swap_column) and animates the resulting layout
    /// diff if the swap succeeded. The layout engine handles off-screen
    /// columns transparently — `swap_column` internally calls
    /// `ensure_column_visible`, so the viewport scrolls as part of the same
    /// diff and no separate "offscreen" message is needed.
    fn dispatch_swap_column(&mut self, dir: Direction) -> SocketResponse {
        match self.active_scrolling_mut().swap_column(dir) {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot swap column in that direction".into(),
            },
        }
    }

    /// Dispatch a **per-window** swap in the given direction.
    ///
    /// Calls [`ScrollingSpace::swap_window`](crate::workspace::ScrollingSpace::swap_window), which exchanges the focused
    /// window with its neighbour: up/down moves it within the same column,
    /// left/right exchanges it with a window in the adjacent column.
    fn dispatch_swap_window(&mut self, dir: Direction) -> SocketResponse {
        match self.active_scrolling_mut().swap_window(dir) {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot swap window in that direction".into(),
            },
        }
    }

    /// Dispatch a semantic "move window" command.
    ///
    /// This is the daemon-side translation of the high-level
    /// [`SocketMessage::MoveWindow`] intent. The concrete action depends on
    /// the focused window's state and the direction:
    ///
    /// - **Tiled, left/right** → a column swap (delegates to
    ///   [`dispatch_swap_column`](Self::dispatch_swap_column)), since moving
    ///   a tiled window horizontally *is* swapping its column.
    /// - **Tiled, up/down** → a within-column window swap *(deferred)*.
    /// - **Floating, any direction** → a pixel nudge by a configurable shift
    ///   *(deferred)*.
    ///
    /// For now only the tiled left/right path is wired, so `movewindow`
    /// behaves identically to `swapcolumn`. The branching structure is kept
    /// as a single delegation point so that floating and up/down support can
    /// be added later without changing the IPC protocol or keybindings.
    fn dispatch_move_window(&mut self, dir: Direction) -> SocketResponse {
        // TODO(floating): inspect the focused window's state and branch:
        //   - floating → nudge by config move_shift
        //   - tiled up/down → dispatch_swap_window(dir)
        self.dispatch_swap_column(dir)
    }

    /// Dispatch a scroll-left command.
    fn dispatch_scroll_left(&mut self) -> SocketResponse {
        match self.active_scrolling_mut().scroll_left() {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot scroll left".into(),
            },
        }
    }

    /// Dispatch a scroll-right command.
    fn dispatch_scroll_right(&mut self) -> SocketResponse {
        match self.active_scrolling_mut().scroll_right() {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot scroll right".into(),
            },
        }
    }

    /// Dispatch an expand-column command on the focused column.
    fn dispatch_expand(&mut self) -> SocketResponse {
        match self.active_scrolling_mut().expand_column() {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot expand column".into(),
            },
        }
    }

    /// Dispatch a shrink-column command on the focused column.
    fn dispatch_shrink(&mut self) -> SocketResponse {
        match self.active_scrolling_mut().shrink_column() {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot shrink column".into(),
            },
        }
    }

    /// Dispatch an explicit column width setting.
    ///
    /// The width is in pixels and is validated against the layout engine's
    /// current bounds (`[min_column_width_px, abs_max_width]`) before being
    /// delegated. This is the **free-form / drag-resize** path — the value is
    /// applied directly and is not snapped to the expand/shrink slot ladder.
    fn dispatch_set_column_width(&mut self, width_px: u32) -> SocketResponse {
        let (min, max) = self.active_scrolling().column_width_bounds();
        // `u32 → i32` can fail for absurd inputs (`> i32::MAX`). Reject with a
        // precise message instead of letting the cast wrap negative and report
        // a misleading value.
        let target = match i32::try_from(width_px) {
            Ok(t) => t,
            Err(_) => {
                return SocketResponse::Error {
                    message: format!(
                        "width_px {width_px} exceeds the maximum representable column width"
                    ),
                };
            }
        };
        if target < min {
            return SocketResponse::Error {
                message: format!("width_px must be >= {min}, got {target}"),
            };
        }
        if target > max {
            return SocketResponse::Error {
                message: format!("width_px must be <= {max}, got {target}"),
            };
        }
        match self.active_scrolling_mut().set_column_width(target) {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot set column width (no focused window or no-op)".into(),
            },
        }
    }

    /// Dispatch a free-form viewport center on the focused window.
    ///
    /// Delegates to [`ScrollingSpace::center_absolute`](crate::workspace::ScrollingSpace::center_absolute)
    /// (see `docs/src/dev-guide/layout/mutations.md` for the grid-vs-absolute
    /// distinction).
    fn dispatch_center_absolute(&mut self) -> SocketResponse {
        match self.active_scrolling_mut().center_absolute() {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot center viewport (empty workspace)".into(),
            },
        }
    }

    /// Dispatch a slot-aligned viewport center on the grid.
    ///
    /// Delegates to [`ScrollingSpace::center_grid`](crate::workspace::ScrollingSpace::center_grid).
    fn dispatch_center_grid(&mut self) -> SocketResponse {
        match self.active_scrolling_mut().center_grid() {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot center viewport grid (empty workspace)".into(),
            },
        }
    }

    /// Dispatch a monocle mode toggle on the focused column.
    fn dispatch_toggle_monocle(&mut self) -> SocketResponse {
        match self.active_scrolling_mut().toggle_monocle() {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot toggle monocle".into(),
            },
        }
    }

    /// Switch the active monitor's focus to a different workspace.
    ///
    /// Implements the **vertical packing** workspace model: each workspace
    /// is parked one monitor-height (plus one `window_gap`) above or below
    /// the active workspace, and only the source (previously active) and
    /// destination (newly active) workspaces animate during a switch.
    ///
    /// # Animation partitioning
    ///
    /// Every non-empty workspace on the active monitor is classified into
    /// exactly one of three buckets:
    ///
    /// | Bucket | Workspaces | Action |
    /// |--------|------------|--------|
    /// | **Animate** | source (`prev_active_id`) + destination (`target_id`) | submitted to [`animate_workspaces`](Self::animate_workspaces) as a single coordinated batch |
    /// | **Teleport** | bystanders whose parked side changed (e.g. ws 3-7 when switching 2 → 8) | submitted to [`teleport_workspaces`](Self::teleport_workspaces) — instant `SetWindowPos`, no animator |
    /// | **Untouched** | bystanders whose parked side stayed the same | skipped entirely |
    ///
    /// The animate/teleport split is what keeps the user's attention on the
    /// two workspaces that are actually transitioning: a 10-workspace switch
    /// could otherwise animate up to eight bystander workspaces all sliding
    /// across the screen at once.
    ///
    /// # Why teleport before animate?
    ///
    /// Teleport is called first so the bystander "backdrop" snaps into its
    /// post-switch configuration before the participant animation begins.
    /// If a bystander is currently mid-flight from a prior animation, the
    /// teleport retargets it instantly to its new parked slot — exactly the
    /// behaviour we want for windows the user isn't looking at.
    ///
    /// # Why no registry sync?
    ///
    /// Unlike a layout mutation, a workspace switch does **not** change any
    /// workspace's [`VirtualLayout`] or [`ActualLayout`] — windows keep their
    /// workspace-local positions; only their *monitor-stack* y offset
    /// changes. The registry's `tiled_rects` (workspace-local visible rects)
    /// stay valid as-is, so neither [`update_tiling_slots_from_layout`] nor
    /// [`update_tiled_rects`] is called here.
    ///
    /// [`update_tiling_slots_from_layout`]:
    ///     crate::registry::WindowRegistry::update_tiling_slots_from_layout
    /// [`update_tiled_rects`]:
    ///     crate::registry::WindowRegistry::update_tiled_rects
    ///
    /// # Errors
    ///
    /// Returns [`SocketResponse::Error`] when `target_id` does not match any
    /// workspace on the active monitor. Switching to the already-active
    /// workspace is a successful no-op.
    fn dispatch_switch_workspace(&mut self, target_id_raw: u32) -> SocketResponse {
        let target_id = WorkspaceId(target_id_raw);
        let prev_active_id = self.active_monitor().active_workspace_id();

        // Switch-to-self is a no-op success — preserves window positions and
        // avoids submitting an empty animation batch.
        if prev_active_id == target_id {
            return SocketResponse::Ok;
        }

        // Reject unknown workspace ids up front with a precise message.
        if self
            .active_monitor()
            .find_workspace_index(target_id)
            .is_none()
        {
            return SocketResponse::Error {
                message: format!("workspace {target_id:?} not found on the active monitor"),
            };
        }

        // Geometry capture for the parking offset: parked workspaces must
        // travel the FULL physical monitor height to stay completely
        // off-screen — the taskbar-excluded work area would leave a slice
        // peeking past the taskbar strip. The window_gap is the same value
        // used intra-workspace, so stacking looks consistent with tiling.
        let monitor_height = self.active_monitor().screen_rect().height;
        let window_gap = self.active_scrolling().padding().window_gap;

        // Update the active index synchronously. The id was just validated
        // above so failure is impossible in practice — but the explicit guard
        // keeps the codebase `.unwrap()`-free per AGENTS.md.
        if self
            .active_monitor_mut()
            .set_active_workspace(target_id)
            .is_none()
        {
            return SocketResponse::Error {
                message: format!("workspace {target_id:?} disappeared mid-dispatch"),
            };
        }

        // Partition every non-empty workspace into the animate / teleport /
        // skip buckets described in the method-level docs.
        let mut animate_batches: Vec<(ActualLayout, i32)> = Vec::new();
        let mut teleport_batches: Vec<(ActualLayout, i32)> = Vec::new();
        for ws in self.active_monitor().workspaces() {
            if ws.scrolling.actual_layout().entries.is_empty() {
                continue;
            }

            let prev_offset = workspace_y_offset(ws.id, prev_active_id, monitor_height, window_gap);
            let new_offset = workspace_y_offset(ws.id, target_id, monitor_height, window_gap);

            let is_participant = ws.id == target_id || ws.id == prev_active_id;
            let side_changed = prev_offset != new_offset;

            if is_participant {
                animate_batches.push((ws.scrolling.actual_layout().clone(), new_offset));
            } else if side_changed {
                teleport_batches.push((ws.scrolling.actual_layout().clone(), new_offset));
            }
            // else: bystander at the same parked side — leave untouched.
        }

        // Teleport first (instant backdrop), then animate the participants
        // (single coordinated batch — source and dest transition in lockstep).
        self.teleport_workspaces(&teleport_batches);
        self.animate_workspaces(&animate_batches);

        SocketResponse::Ok
    }

    /// Move the focused window from the active workspace to a target workspace.
    ///
    /// Implements the cross-workspace window move: the focused window in the
    /// active workspace is detached (with **local** focus succession — no OS
    /// foreground push) and re-inserted into the target workspace's
    /// [`ScrollingSpace`] after its currently focused column. The active
    /// workspace itself does NOT change — focus stays with the source
    /// workspace, and the moved window becomes the destination workspace's
    /// focus.
    ///
    /// # Animation
    ///
    /// Both the source and destination workspaces are mutated, so both must
    /// be repainted. The two [`ActualLayout`]s are submitted to
    /// [`animate_workspaces`](Self::animate_workspaces) as a single
    /// coordinated batch with each entry's `final_position.y` shifted by its
    /// workspace's y-offset (active = 0, others = ±`(monitor_height +
    /// window_gap)`). The animator's default `RetargetFromCurrent` policy
    /// keeps the move non-blocking — any command issued mid-flight retargets
    /// from each window's current interpolated position.
    ///
    /// # Registry sync (per-workspace)
    ///
    /// Unlike a workspace *switch*, a window *move* changes both
    /// [`VirtualLayout`]s: the source loses a window, the destination gains
    /// one. The registry's tiling slots and tiled rects must therefore be
    /// refreshed for each. [`update_tiling_slots_from_layout`] and
    /// [`update_tiled_rects`] only update windows present in the supplied
    /// layout, so calling them with the destination layout last ensures the
    /// moved window's slot/rect end up pointing at its destination position.
    ///
    /// [`update_tiling_slots_from_layout`]:
    ///     crate::registry::WindowRegistry::update_tiling_slots_from_layout
    /// [`update_tiled_rects`]:
    ///     crate::registry::WindowRegistry::update_tiled_rects
    ///
    /// # Mutation order
    ///
    /// The destination id is validated **before** any state mutation, so a
    /// bad id fails fast with no layout damage. The borrow checker forbids
    /// holding `&mut` to two workspaces in the same `Vec<Workspace>`
    /// simultaneously, so source and destination are mutated in two separate
    /// single-workspace steps.
    ///
    /// # Errors
    ///
    /// Returns [`SocketResponse::Error`] when:
    /// - `dest_id` does not match any workspace on the active monitor.
    /// - No window is focused on the active workspace (nothing to move).
    /// - The destination lookup fails post-validation (impossible in practice
    ///   but guarded to keep the codebase `.unwrap()`-free per AGENTS.md).
    fn dispatch_move_window_to_workspace(&mut self, dest_id_raw: u32) -> SocketResponse {
        let dest_id = WorkspaceId(dest_id_raw);

        // Validate destination up front — fail fast with no state change.
        if self
            .active_monitor()
            .find_workspace_index(dest_id)
            .is_none()
        {
            return SocketResponse::Error {
                message: format!("workspace {dest_id:?} not found on the active monitor"),
            };
        }

        // Capture the focused window before mutating anything. The active
        // workspace remains active after the move — focus succession inside
        // the source workspace is handled by `remove_window`.
        let focused: WindowId = match self.active_scrolling().focused() {
            Some(f) => f,
            None => {
                return SocketResponse::Error {
                    message: "no focused window to move".into(),
                };
            }
        };

        // The active workspace id does NOT change here — the moved window
        // departs, but the user's viewport stays put.
        let active_id = self.active_monitor().active_workspace_id();

        // Geometry capture: shared across all workspaces on the monitor. Uses
        // the full physical height (not the work area) so parked workspaces
        // stay fully off-screen — see `dispatch_switch_workspace` for the
        // full rationale.
        let monitor_height = self.active_monitor().screen_rect().height;
        let window_gap = self.active_scrolling().padding().window_gap;

        // --- Mutation 1: remove from source (the active workspace). ---
        // `remove_window` runs the full focus-fallback + ensure-visible
        // pipeline internally and returns the post-removal AppliedLayout.
        let source_applied = self
            .active_monitor_mut()
            .active_scrolling_mut()
            .remove_window(focused);
        let source_virtual = source_applied.virtual_layout.clone();
        let source_actual = source_applied.actual_layout.clone();

        // Refresh registry state for the source workspace.
        self.registry
            .update_tiling_slots_from_layout(&source_virtual);
        self.registry.update_tiled_rects(&source_actual);

        // --- Mutation 2: insert into destination. ---
        // `insert_window` places the window after the dest's focused column
        // (or at the start if dest is empty) and re-focuses the new window.
        //
        // Auto-center: when the destination ends up sparser than
        // `columns_per_screen`, slot-center its grid so the moved window
        // doesn't sit alone at a left-aligned position. Grid variant matches
        // `initialize_windows` for consistency
        // (`docs/src/dev-guide/layout/mutations.md`). Strict `<` so an
        // exactly-full screen is left untouched.
        let dest_applied = match self.active_monitor_mut().workspace_mut(dest_id) {
            Some(ws) => {
                let applied = ws.scrolling.insert_window(focused);
                if applied.virtual_layout.columns.len() < ws.scrolling.columns_per_screen() as usize
                {
                    ws.scrolling.center_grid().unwrap_or(applied)
                } else {
                    applied
                }
            }
            None => {
                // `dest_id` was validated above, so this branch is
                // unreachable in practice. Guard anyway to stay
                // `.unwrap()`-free per AGENTS.md.
                return SocketResponse::Error {
                    message: format!("workspace {dest_id:?} disappeared mid-dispatch"),
                };
            }
        };
        let dest_virtual = dest_applied.virtual_layout.clone();
        let dest_actual = dest_applied.actual_layout.clone();

        // Refresh registry state for the destination workspace. This call
        // wins for the moved window — its slot/rect now reflect the
        // destination layout.
        self.registry.update_tiling_slots_from_layout(&dest_virtual);
        self.registry.update_tiled_rects(&dest_actual);

        // Skip the animation batch entirely if both layouts ended up empty
        // (the trivial case: moving the only window into an empty workspace
        // when the source ends up empty too).
        if source_actual.entries.is_empty() && dest_actual.entries.is_empty() {
            return SocketResponse::Ok;
        }

        // Build a single coordinated batch: source paints at offset 0 (it's
        // still the active workspace), destination paints at its parked
        // offset (±y_unit depending on whether dest is above or below the
        // active workspace in id order).
        let source_y_offset = workspace_y_offset(active_id, active_id, monitor_height, window_gap);
        let dest_y_offset = workspace_y_offset(dest_id, active_id, monitor_height, window_gap);
        let batches = [
            (source_actual, source_y_offset),
            (dest_actual, dest_y_offset),
        ];
        self.animate_workspaces(&batches);

        SocketResponse::Ok
    }
}

/// Return a standard "not yet implemented" error response.
fn unimplemented_command(name: &str) -> SocketResponse {
    SocketResponse::Error {
        message: format!("command '{name}' is not yet implemented"),
    }
}
