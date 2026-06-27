//! IPC command dispatch router and action handlers.
//!
//! This module contains:
//!
//! - [`ScrollTilingManager::dispatch`] — routes each [`SocketMessage`] variant
//!   to the appropriate subsystem.
//! - Individual `dispatch_*` helper methods for each command category.
//! - Helper functions for unimplemented commands.

use crate::common::{Direction, Size, WindowId};
use crate::config::dirs::history_rules_path_in;
use crate::config::types::WindowAction;
use crate::ipc::message::{SocketMessage, SocketResponse, WindowMode};
use crate::layout::projection;
use crate::layout::types::ActualLayout;
use crate::registry::hooks::{add_float_hwnd, remove_float_hwnd, set_float_hwnds};
use crate::registry::types::{FloatingState, TilingState, WindowState};
use crate::registry::win32 as registry_win32;
use crate::workspace::{FloatingSpace, WorkspaceId, workspace_y_offset};
use windows::Win32::Foundation::HWND;

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
            SocketMessage::Center => self.dispatch_center(),

            // --- Window state ---
            SocketMessage::ToggleFloat => self.dispatch_set_window(WindowMode::Cycle),
            SocketMessage::SetWindow { mode } => self.dispatch_set_window(*mode),
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
        let Some(focused) = self.active_scrolling().last_focused_window() else {
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
    /// For now only the tiled left/right path is wired, so `move-window`
    /// behaves identically to `swap-column`. The branching structure is kept
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

    /// Center the viewport so the focused column lands at the monitor midpoint.
    ///
    /// Delegates to [`ScrollingSpace::center_focused_column`] which uses the
    /// actual prefix-sum canvas position (variable-width aware). Always centers
    /// even when all columns fit — this is the explicit center command. See
    /// (`docs/src/dev-guide/layout/mutations.md`).
    fn dispatch_center(&mut self) -> SocketResponse {
        match self.active_scrolling_mut().center_focused_column() {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot center viewport (empty workspace)".into(),
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
    /// Validates `target_id`, then delegates the active-index update and the
    /// partitioned switch animation to [`switch_active_workspace`]. See that
    /// helper for the vertical-packing animation model.
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

        if !self.switch_active_workspace(target_id, prev_active_id) {
            return SocketResponse::Error {
                message: format!("workspace {target_id:?} disappeared mid-dispatch"),
            };
        }

        SocketResponse::Ok
    }

    /// Perform a workspace switch from `prev_active_id` to `target_id`.
    ///
    /// Implements the **vertical packing** workspace model: each workspace
    /// is parked one monitor-height (plus one `window_gap`) above or below
    /// the active workspace, and only the source (previously active) and
    /// destination (newly active) workspaces animate during a switch.
    ///
    /// Called by [`switch_active_workspace`] (the IPC-path wrapper that also
    /// re-establishes tiling focus) and by `on_focus_changed` (the foreground
    /// hook, which must not re-push foreground — the OS already chose the
    /// window).
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
    /// # Caller invariants
    ///
    /// The caller MUST (a) validate that `target_id` exists on the active
    /// monitor before calling, and (b) capture `prev_active_id` before the
    /// active index changes. This method does **not** re-validate — it trusts
    /// the caller. It does **not** touch the OS foreground or the registry's
    /// `focused` field — that is the caller's responsibility (see
    /// [`switch_active_workspace`] for the IPC-path wrapper that re-establishes
    /// tiling focus). Tiled rects are also untouched: a switch changes only
    /// y-offsets, not workspace-local positions. (A window *move* caller syncs
    /// the registry's tiled rects for both workspaces before calling this.)
    ///
    /// # Returns
    ///
    /// `false` only if `target_id` could not be set active (impossible after
    /// caller validation — guarded to keep the codebase `.unwrap()`-free per
    /// AGENTS.md).
    pub(super) fn switch_workspace_layout(
        &mut self,
        target_id: WorkspaceId,
        prev_active_id: WorkspaceId,
    ) -> bool {
        // Geometry capture for the parking offset: parked workspaces must
        // travel the FULL physical monitor height to stay completely
        // off-screen — the taskbar-excluded work area would leave a slice
        // peeking past the taskbar strip. The window_gap is the same value
        // used intra-workspace, so stacking looks consistent with tiling.
        let monitor_height = self.active_monitor().screen_rect().height;
        let window_gap = self.active_scrolling().padding().window_gap;

        // Update the active index synchronously. The caller validated the id,
        // so failure is impossible in practice — but the explicit guard keeps
        // the codebase `.unwrap()`-free per AGENTS.md.
        if self
            .active_monitor_mut()
            .set_active_workspace(target_id)
            .is_none()
        {
            return false;
        }

        // The float-tracking set must hold only the NEW active workspace's
        // floats. Parked workspaces' floats can never be dragged (they're
        // off-screen), and excluding them guarantees a stray LOCATIONCHANGE
        // during the switch animation cannot corrupt a parked workspace's rect.
        let new_active_float_hwnds: Vec<isize> = self
            .active_monitor()
            .active_workspace()
            .floating
            .windows()
            .iter()
            .map(|entry| entry.window_id.0)
            .collect();
        set_float_hwnds(&new_active_float_hwnds);

        // Partition every non-empty workspace into the animate / teleport /
        // skip buckets described in the method-level docs.
        let mut animate_batches: Vec<(ActualLayout, i32)> = Vec::new();
        let mut teleport_batches: Vec<(ActualLayout, i32)> = Vec::new();
        for ws in self.active_monitor().workspaces() {
            let scroll_actual = ws.scrolling.actual_layout();
            let float_actual = ws.floating.to_actual_layout();

            // Floating windows share the same workspace and must ride along
            // with tiles on workspace switch. Build a merged layout so
            // animate_workspaces / teleport_workspaces moves both together.
            if scroll_actual.entries.is_empty() && float_actual.entries.is_empty() {
                continue;
            }

            let prev_offset = workspace_y_offset(ws.id, prev_active_id, monitor_height, window_gap);
            let new_offset = workspace_y_offset(ws.id, target_id, monitor_height, window_gap);

            let is_participant = ws.id == target_id || ws.id == prev_active_id;
            let side_changed = prev_offset != new_offset;

            // Merge scrolling + floating entries into one batch layout.
            let mut merged_entries = scroll_actual.entries.clone();
            merged_entries.extend(float_actual.entries.iter().cloned());
            let merged = ActualLayout {
                entries: merged_entries,
            };

            if is_participant {
                animate_batches.push((merged, new_offset));
            } else if side_changed {
                teleport_batches.push((merged, new_offset));
            }
            // else: bystander at the same parked side — leave untouched.
        }

        // Teleport first (instant backdrop), then animate the participants
        // (single coordinated batch — source and dest transition in lockstep).
        self.teleport_workspaces(&teleport_batches);
        self.animate_workspaces(&animate_batches);

        true
    }

    /// IPC-path workspace switch: layout/animation plus tiling-focus push.
    ///
    /// Wraps [`switch_workspace_layout`] and then re-establishes OS foreground
    /// on the destination's `last_focused_window`, mirroring [`dispatch_focus`].
    /// Used by [`dispatch_switch_workspace`] and
    /// [`dispatch_move_window_to_workspace`]. The foreground hook
    /// (`on_focus_changed`) calls [`switch_workspace_layout`] directly — it
    /// must not re-push foreground because the OS already chose the window.
    fn switch_active_workspace(
        &mut self,
        target_id: WorkspaceId,
        prev_active_id: WorkspaceId,
    ) -> bool {
        if !self.switch_workspace_layout(target_id, prev_active_id) {
            return false;
        }

        // Re-establish tiling focus on the destination. Without this the
        // registry's `focused` could keep pointing at a window in the (now
        // parked) previous workspace. Mirrors `dispatch_focus`.
        if let Some(target) = self.active_scrolling().last_focused_window() {
            let target_hwnd = target.0;
            if !registry_win32::set_foreground_window(target_hwnd) {
                log::warn!(
                    "switch_active_workspace: SetForegroundWindow failed for hwnd {target_hwnd}"
                );
            }
            self.registry.set_focused(target_hwnd);
        }

        true
    }

    /// Move the focused window from the active workspace to a target workspace.
    ///
    /// Implements the cross-workspace window move: the focused window in the
    /// active workspace is detached (with **local** focus succession — no OS
    /// foreground push) and re-inserted into the target workspace's
    /// [`ScrollingSpace`] after its currently focused column. The camera then
    /// **follows the moved window**: the active workspace switches to the
    /// destination so the moved window is brought into view.
    ///
    /// # Animation
    ///
    /// Both the source and destination workspaces are mutated (source loses a
    /// window, destination gains one), and then [`switch_active_workspace`]
    /// animates the transition from source to destination as a single
    /// coordinated switch: the source slides to its parked y-offset and the
    /// destination (now holding the moved window) slides into the active
    /// position at offset 0. The animator's default `RetargetFromCurrent`
    /// policy keeps the move non-blocking — any command issued mid-flight
    /// retargets from each window's current interpolated position.
    ///
    /// # Registry sync (per-workspace)
    ///
    /// A window *move* changes both [`VirtualLayout`]s: the source loses a
    /// window, the destination gains one. The registry's tiling slots and
    /// tiled rects must therefore be refreshed for each. Both syncs run
    /// **before** the switch animation (the switch itself changes only
    /// y-offsets, not workspace-local positions). The destination sync runs
    /// last so the moved window's slot/rect end up pointing at its
    /// destination position.
    ///
    /// # Mutation order
    ///
    /// The move-to-self case and the destination id are checked **before** any
    /// state mutation, so a no-op or bad-id request fails fast with no layout
    /// damage. The borrow checker forbids holding `&mut` to two workspaces in
    /// the same `Vec<Workspace>` simultaneously, so source and destination
    /// are mutated in two separate single-workspace steps.
    ///
    /// # Errors
    ///
    /// Returns [`SocketResponse::Error`] when:
    /// - `dest_id` does not match any workspace on the active monitor.
    /// - No window is focused on the active workspace (nothing to move).
    /// - The destination lookup fails post-validation (impossible in practice
    ///   but guarded to keep the codebase `.unwrap()`-free per AGENTS.md).
    ///
    /// Moving to the currently active workspace is a successful no-op.
    fn dispatch_move_window_to_workspace(&mut self, dest_id_raw: u32) -> SocketResponse {
        let dest_id = WorkspaceId(dest_id_raw);
        let active_id = self.active_monitor().active_workspace_id();

        // Move-to-self is a no-op success — moving the focused window to its
        // own workspace leaves everything exactly where it is. Mirrors the
        // switch-to-self guard in `dispatch_switch_workspace`.
        if dest_id == active_id {
            return SocketResponse::Ok;
        }

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
        let focused: WindowId = match self.active_scrolling().last_focused_window() {
            Some(f) => f,
            None => {
                return SocketResponse::Error {
                    message: "no focused window to move".into(),
                };
            }
        };

        // Capture the focused window's current column width before removal,
        // so it can be preserved in the destination workspace.
        let moved_width_px: u32 = self
            .active_scrolling()
            .virtual_layout()
            .find_window(focused)
            .map(|(col_idx, _)| {
                self.active_scrolling().virtual_layout().columns[col_idx].width_px as u32
            })
            .unwrap_or_else(|| {
                // Fallback to base column_width if lookup fails (e.g. stale
                // focus). This is safe — the window will just get the default.
                self.active_scrolling().config().column_width
            });

        // --- Mutation 1: remove from source (the active workspace). ---
        // `remove_window` runs the full focus-fallback + ensure-visible
        // pipeline internally and returns the post-removal AppliedLayout.
        let source_applied = self
            .active_monitor_mut()
            .active_scrolling_mut()
            .remove_window(focused);

        // Refresh registry state for the source workspace.
        self.registry
            .update_tiling_slots_from_layout(&source_applied.virtual_layout);
        self.registry
            .update_tiled_rects(&source_applied.actual_layout);

        // --- Mutation 2: insert into destination. ---
        // The moved window preserves its pre-move width via
        // `insert_window_with_width`. After insertion, decide how to
        // position the viewport: fit (all columns fit in monitor → center
        // the entire canvas) vs. overflow (ensure the moved window's new
        // column is visible).
        let dest_applied = match self.active_monitor_mut().workspace_mut(dest_id) {
            Some(ws) => {
                let post_insert = ws
                    .scrolling
                    .insert_window_with_width(focused, moved_width_px);
                let gap = ws.scrolling.config().padding.window_gap;
                let canvas_w = projection::canvas_width(&post_insert.virtual_layout, gap);
                let monitor_w = ws.scrolling.config().monitor_width;
                if canvas_w <= monitor_w {
                    // Fit: center the entire canvas (offset may be negative
                    // when canvas < monitor — projection handles this).
                    ws.scrolling.center_canvas().unwrap_or(post_insert)
                } else {
                    // Overflow: ensure the moved window's new column is
                    // visible. `insert_window_with_width` focuses the moved
                    // window, so `ensure_focused_visible` targets it.
                    ws.scrolling.ensure_focused_visible().unwrap_or(post_insert)
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

        // Refresh registry state for the destination workspace. This call
        // wins for the moved window — its slot/rect now reflect the
        // destination layout.
        self.registry
            .update_tiling_slots_from_layout(&dest_applied.virtual_layout);
        self.registry
            .update_tiled_rects(&dest_applied.actual_layout);

        // The camera follows the moved window: switch the active workspace to
        // the destination so the moved window is brought into view. The
        // switch reads the just-mutated layouts and animates source (to its
        // parked offset) and destination (into the active position at offset
        // 0) as a single coordinated batch.
        if !self.switch_active_workspace(dest_id, active_id) {
            return SocketResponse::Error {
                message: format!("workspace {dest_id:?} disappeared mid-dispatch"),
            };
        }

        SocketResponse::Ok
    }

    /// Set the focused window's mode (float, tile, or cycle).
    ///
    /// Transitions the OS-focused window between tiled and floating state:
    ///
    /// - **Float**: removes from [`ScrollingSpace`], adds centered to
    ///   [`FloatingSpace`], sets registry state to `Floating(Active { rect })`.
    /// - **Tile**: removes from [`FloatingSpace`], inserts after the
    ///   scrolling space's `last_focused_window`, sets registry state to
    ///   `Tiling(Active { col, row })`.
    /// - **Cycle**: toggles based on current state (tile→float, float→tile).
    /// - Already in the desired state → no-op `Ok`.
    ///
    /// On a successful non-no-op transition, also records the decision to
    /// `history-stm-rules.toml` so the next window of the same app is
    /// classified automatically. See (`docs/src/dev-guide/classification.md`).
    ///
    /// # Animation
    ///
    /// Both directions submit a single coordinated batch to
    /// [`animate_workspaces`](Self::animate_workspaces) at `y_offset = 0` (same
    /// workspace). The post-removal scrolling layout and the (possibly updated)
    /// floating layout are merged so the animator moves both sets of windows
    /// atomically.
    ///
    /// # Errors
    ///
    /// Returns [`SocketResponse::Error`] when:
    /// - No window is OS-focused (registry's `focused` is `None`).
    /// - The focused window is `Ignored` or not found in the registry.
    ///
    /// See (`docs/src/dev-guide/floating-space.md`) for the tile↔float
    /// transition design.
    fn dispatch_set_window(&mut self, mode: WindowMode) -> SocketResponse {
        // 1. Get the OS-focused window from the registry.
        let focused = match self.registry.focused() {
            Some(f) => f,
            None => {
                return SocketResponse::Error {
                    message: "no focused window".into(),
                };
            }
        };

        // 2. Inspect the focused window's current state.
        let hwnd = HWND(focused.0 as *mut _);
        let win = match self.registry.get_window(hwnd) {
            Some(w) => w,
            None => {
                return SocketResponse::Error {
                    message: "focused window not found in registry".into(),
                };
            }
        };

        let currently_tiling = matches!(win.state, WindowState::Tiling(TilingState::Active { .. }));
        let currently_floating = matches!(
            win.state,
            WindowState::Floating(FloatingState::Active { .. })
        );

        // Capture the window's identity for history recording before the
        // immutable borrow of `self.registry` (via `win`) ends — the
        // transition and recording calls below need `&mut self`. Cloning two
        // short strings is negligible. See (`docs/src/dev-guide/classification.md`).
        let exe = win.exe.clone();
        let class = win.class.clone();

        // 3. Resolve the effective action via the pure decision helper. This is
        //    the only non-trivial branching in the handler — extracting it
        //    makes the full mode × state table unit-testable without
        //    constructing the daemon (see `resolve_set_window_action`).
        let action = match resolve_set_window_action(mode, currently_tiling, currently_floating) {
            Ok(a) => a,
            Err(()) => {
                return SocketResponse::Error {
                    message: format!(
                        "window is in state {:?} (only active tiling/floating can transition)",
                        win.state
                    ),
                };
            }
        };

        // 4. Execute the transition (NoOp short-circuits to Ok without
        //    touching the layout engine or the animator).
        let response = match action {
            SetWindowAction::NoOp => SocketResponse::Ok,
            SetWindowAction::MakeFloating => self.set_window_to_float(focused),
            SetWindowAction::MakeTiling => self.set_window_to_tile(focused),
        };

        // 5. Record the user's explicit decision so the next window of the
        //    same app is auto-classified. See `record_learned_transition` for
        //    the idempotent save + pipeline-refresh logic.
        if matches!(response, SocketResponse::Ok) {
            self.record_learned_transition(action, &exe, &class);
        }

        response
    }

    /// Persist a `set-window` transition to `history-stm-rules.toml`.
    ///
    /// `NoOp` transitions are ignored (no user intent to record). The store is
    /// saved only when `record` reports a change, so repeated identical
    /// commands are a no-op on disk. After a save the classification pipeline
    /// is refreshed so the next window of the same app auto-classifies.
    fn record_learned_transition(&mut self, action: SetWindowAction, exe: &str, class: &str) {
        let Some(learned) = action_to_learned(action) else {
            return;
        };
        if !self.history.record(learned, exe, Some(class)) {
            return;
        }
        if let Err(e) = self.history.save(&history_rules_path_in(&self.config_dir)) {
            log::warn!("failed to persist history-stm-rules.toml: {e}");
        }
        self.registry
            .set_learned_rules(self.history.rules().to_vec());
    }

    /// Transition a window from tiling to floating.
    ///
    /// Removes the window from the active [`ScrollingSpace`], computes a
    /// centered float rect (preferring the window's `last_natural_size`,
    /// falling back to config fractions of the work area), adds it to the
    /// active [`FloatingSpace`], and animates both the post-removal scrolling
    /// layout and the updated floating layout in a single batch.
    ///
    /// Registry state is set to `Floating(Active { rect })` via direct field
    /// write (the `state` field on [`Window`](crate::registry::types::Window)
    /// is `pub`).
    fn set_window_to_float(&mut self, focused: WindowId) -> SocketResponse {
        // a) Remove from scrolling. remove_window handles focus fallback
        //    (next_available_window) and ensure_column_visible internally.
        let source_applied = self
            .active_monitor_mut()
            .active_scrolling_mut()
            .remove_window(focused);
        let source_virtual = source_applied.virtual_layout.clone();
        let source_actual = source_applied.actual_layout.clone();

        // b) Sync registry tiling state for the scrolling side.
        self.registry
            .update_tiling_slots_from_layout(&source_virtual);
        self.registry.update_tiled_rects(&source_actual);

        // c) Compute the floating rect.
        let work_area = self.active_scrolling().monitor().work_area;
        let config_default = Size {
            w: (work_area.width as f32 * self.config.floating.default_width) as i32,
            h: (work_area.height as f32 * self.config.floating.default_height) as i32,
        };
        let preferred = match self
            .registry
            .get_window(HWND(focused.0 as *mut _))
            .map(|w| w.last_natural_size)
            .filter(|s| s.w > 0 && s.h > 0)
        {
            Some(size) => size,
            None => config_default,
        };
        let float_rect = FloatingSpace::centered_rect(preferred, work_area);

        // d) Add to floating space.
        self.active_workspace_mut()
            .floating
            .add(focused, float_rect);
        let float_actual = self.active_workspace_mut().floating.to_actual_layout();

        // e) Update registry state: Tiling → Floating(Active { rect }).
        if let Some(window) = self.registry.get_window_mut(HWND(focused.0 as *mut _)) {
            window.state = WindowState::Floating(FloatingState::Active { rect: float_rect });
        }

        // Track this window as an active-workspace float so the
        // LOCATIONCHANGE callback forwards its future user drags. Done before
        // the animate so the (Batch 2) float-suppression can detect it.
        add_float_hwnd(focused.0);

        // f) Animate: single batch, both at y_offset 0 (same workspace).
        let batches = [(source_actual, 0), (float_actual, 0)];
        self.animate_workspaces(&batches);

        SocketResponse::Ok
    }

    /// Transition a window from floating to tiling.
    ///
    /// Removes the window from the active [`FloatingSpace`], inserts it into
    /// the [`ScrollingSpace`] after the `last_focused_window`, and animates
    /// both the updated floating layout and the post-insertion scrolling layout
    /// in a single batch.
    ///
    /// Registry state is set to `Tiling(Active { col, row })` via direct field
    /// write, with `col`/`row` sourced from the destination virtual layout.
    fn set_window_to_tile(&mut self, focused: WindowId) -> SocketResponse {
        // a) Remove from floating space.
        self.active_workspace_mut().floating.remove(focused);

        // Drop from the float-tracking set so the LOCATIONCHANGE callback no
        // longer forwards this window (it is becoming a tiled window).
        remove_float_hwnd(focused.0);

        // b) Insert into scrolling. insert_window places after
        //    last_focused_window, shifts right, sets last_focused_window,
        //    and calls ensure_column_visible internally.
        let dest_applied = self
            .active_monitor_mut()
            .active_scrolling_mut()
            .insert_window(focused);
        let dest_virtual = dest_applied.virtual_layout.clone();
        let dest_actual = dest_applied.actual_layout.clone();

        // c) Sync registry tiling state.
        self.registry.update_tiling_slots_from_layout(&dest_virtual);
        self.registry.update_tiled_rects(&dest_actual);

        // d) Get the floating layout (now without the removed window).
        let float_actual = self.active_workspace_mut().floating.to_actual_layout();

        // Registry state is already set to Tiling(Active { col, row }) by
        // update_tiling_slots_from_layout above — no manual write needed.

        // f) Animate: single batch, both at y_offset 0.
        let batches = [(dest_actual, 0), (float_actual, 0)];
        self.animate_workspaces(&batches);

        SocketResponse::Ok
    }
}
fn unimplemented_command(name: &str) -> SocketResponse {
    SocketResponse::Error {
        message: format!("command '{name}' is not yet implemented"),
    }
}

/// The resolved transition for a `SetWindow` request, computed purely from the
/// requested mode and the focused window's current state.
///
/// Extracted from `dispatch_set_window` so the full mode × state decision table
/// is unit-testable without constructing a `ScrollTilingManager` (which owns
/// Win32 handles and cannot be built in a unit test). See
/// (`docs/src/dev-guide/floating-space.md`) for the transition design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetWindowAction {
    /// Transition the window into floating mode.
    MakeFloating,
    /// Transition the window into tiling mode.
    MakeTiling,
    /// The window already satisfies the requested mode — no transition needed.
    NoOp,
}

/// Resolve a `SetWindow` mode against the focused window's current state.
///
/// Pure decision logic — no daemon state, no Win32. Returns:
/// - `Ok(action)` when the window is in an active tiling or floating state.
/// - `Err(())` when the window is ignored / minimized / hidden (no transition
///   is possible). The caller maps this to a descriptive error message.
///
/// # Decision table
///
/// | mode  | currently tiling | currently floating | result        |
/// |-------|------------------|--------------------|---------------|
/// | Float | true             | false              | MakeFloating  |
/// | Float | false            | true               | NoOp          |
/// | Tile  | true             | false              | NoOp          |
/// | Tile  | false            | true               | MakeTiling    |
/// | Cycle | true             | false              | MakeFloating  |
/// | Cycle | false            | true               | MakeTiling    |
/// | any   | false            | false              | Err (ignored) |
const fn resolve_set_window_action(
    mode: WindowMode,
    currently_tiling: bool,
    currently_floating: bool,
) -> Result<SetWindowAction, ()> {
    // Ignored / minimized / hidden windows cannot transition.
    if !currently_tiling && !currently_floating {
        return Err(());
    }
    // Cycle resolves against the current state: tiling → float, otherwise tile.
    let make_float = match mode {
        WindowMode::Float => true,
        WindowMode::Tile => false,
        WindowMode::Cycle => currently_tiling,
    };
    // No-op when the window already satisfies the requested mode.
    let already_satisfied = (make_float && currently_floating) || (!make_float && currently_tiling);
    if already_satisfied {
        Ok(SetWindowAction::NoOp)
    } else if make_float {
        Ok(SetWindowAction::MakeFloating)
    } else {
        Ok(SetWindowAction::MakeTiling)
    }
}

/// Map a resolved transition to the learned-rule action it should produce.
///
/// Returns `None` for [`SetWindowAction::NoOp`] — a no-op transition carries
/// no user intent (the window was already in the requested mode) and must not
/// be recorded. Extracted as a pure `const fn` so the mapping is unit-testable
/// without constructing the daemon (same rationale as `resolve_set_window_action`).
///
/// See (`docs/src/dev-guide/classification.md`) for the history model.
const fn action_to_learned(action: SetWindowAction) -> Option<WindowAction> {
    match action {
        SetWindowAction::MakeFloating => Some(WindowAction::Float),
        SetWindowAction::MakeTiling => Some(WindowAction::Tile),
        SetWindowAction::NoOp => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_set_window_action: the full mode × state table ─────────
    //
    // These tests pin the decision logic extracted from `dispatch_set_window`.
    // Each test is a single cell of the table documented on
    // `resolve_set_window_action`. They run with NO daemon and NO Win32 —
    // that is the whole point of the extraction.

    #[test]
    fn float_mode_on_tiling_makes_floating() {
        // Positive: Float requested, currently tiling → must transition.
        let action = resolve_set_window_action(WindowMode::Float, true, false);
        assert_eq!(action, Ok(SetWindowAction::MakeFloating));
    }

    #[test]
    fn float_mode_on_floating_is_noop() {
        // Positive no-op: already floating, asking to float → nothing to do.
        let action = resolve_set_window_action(WindowMode::Float, false, true);
        assert_eq!(action, Ok(SetWindowAction::NoOp));
    }

    #[test]
    fn tile_mode_on_tiling_is_noop() {
        // Positive no-op: already tiling, asking to tile → nothing to do.
        let action = resolve_set_window_action(WindowMode::Tile, true, false);
        assert_eq!(action, Ok(SetWindowAction::NoOp));
    }

    #[test]
    fn tile_mode_on_floating_makes_tiling() {
        // Positive: Tile requested, currently floating → must transition.
        let action = resolve_set_window_action(WindowMode::Tile, false, true);
        assert_eq!(action, Ok(SetWindowAction::MakeTiling));
    }

    #[test]
    fn cycle_mode_on_tiling_makes_floating() {
        // Positive: Cycle toggles tiling → floating.
        let action = resolve_set_window_action(WindowMode::Cycle, true, false);
        assert_eq!(action, Ok(SetWindowAction::MakeFloating));
    }

    #[test]
    fn cycle_mode_on_floating_makes_tiling() {
        // Positive: Cycle toggles floating → tiling.
        let action = resolve_set_window_action(WindowMode::Cycle, false, true);
        assert_eq!(action, Ok(SetWindowAction::MakeTiling));
    }

    #[test]
    fn float_mode_on_ignored_is_err() {
        // Negative: an ignored / minimized / hidden window cannot transition.
        let action = resolve_set_window_action(WindowMode::Float, false, false);
        assert_eq!(action, Err(()));
    }

    #[test]
    fn tile_mode_on_ignored_is_err() {
        // Negative: Tile on a non-active window is also rejected.
        let action = resolve_set_window_action(WindowMode::Tile, false, false);
        assert_eq!(action, Err(()));
    }

    #[test]
    fn cycle_mode_on_ignored_is_err() {
        // Negative: Cycle has nothing to toggle for a non-active window.
        let action = resolve_set_window_action(WindowMode::Cycle, false, false);
        assert_eq!(action, Err(()));
    }

    #[test]
    fn resolve_action_covers_all_distinct_cells() {
        // Exhaustive guard: every combination of mode and the two booleans is
        // asserted elsewhere in this module; this test re-confirms the full
        // 3 × 3 grid (plus the impossible tiling∧floating row) so a future
        // edit cannot silently regress a cell without tripping a test.
        // The (true, true) cell is treated as already-floating for no-op
        // purposes because the floating check wins the `already_satisfied`
        // short-circuit for Float/Cycle, and the tiling check wins for Tile.
        for mode in [WindowMode::Float, WindowMode::Tile, WindowMode::Cycle] {
            // tiling=true, floating=false
            let _ = resolve_set_window_action(mode, true, false);
            // tiling=false, floating=true
            let _ = resolve_set_window_action(mode, false, true);
            // tiling=false, floating=false (ignored)
            assert_eq!(
                resolve_set_window_action(mode, false, false),
                Err(()),
                "ignored window must always be Err for {mode:?}"
            );
        }
    }

    // ── action_to_learned: transition → recorded action ────────────────
    //
    // These tests pin the mapping used by `dispatch_set_window` to decide
    // whether a transition should be persisted to `history-stm-rules.toml`.
    // Like the resolve tests above, they run with NO daemon and NO Win32.

    #[test]
    fn action_to_learned_make_floating_maps_to_float() {
        assert_eq!(
            action_to_learned(SetWindowAction::MakeFloating),
            Some(WindowAction::Float)
        );
    }

    #[test]
    fn action_to_learned_make_tiling_maps_to_tile() {
        assert_eq!(
            action_to_learned(SetWindowAction::MakeTiling),
            Some(WindowAction::Tile)
        );
    }

    #[test]
    fn action_to_learned_noop_maps_to_none() {
        // NoOp carries no user intent (window was already in the requested
        // mode) and must not be recorded — otherwise every idempotent
        // `set-window` call would write to disk.
        assert_eq!(action_to_learned(SetWindowAction::NoOp), None);
    }
}
