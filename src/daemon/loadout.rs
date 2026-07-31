//! Loadout save/restore orchestration.
//!
//! This module implements the daemon-side logic for serializing the current
//! workspace arrangement to a versioned JSON file (save) and resolving a
//! saved file back onto live windows (load). Both paths are wired through
//! [`FlowWM::dispatch`] and share a single save code path so that
//! `flow loadout save` and the save-on-stop hook behave identically.
//!
//! # Window identity: HWND-exact
//!
//! The matcher keys each saved slot to its stored `HWND` (see [`WindowRef`]) —
//! an exact, unambiguous lookup with no scoring or tie-breaking. `exe`/`title`
//! ride along as diagnostic-only fields so a failed restore can name the
//! missing window. The rationale for HWND-exact matching (and the rejected
//! fuzzy alternative) is in the dev guide (`docs/src/dev-guide/loadout.md`).
//!
//! # Save algorithm
//!
//! Walk every monitor → workspace → join the virtual layout (columns, rows,
//! viewport offset, focus) with per-window registry metadata (HWND, exe,
//! title). Swap `WindowId` for [`WindowRef`]. Serialize as JSON.
//!
//! # Load algorithm (no-partial guarantee)
//!
//! 1. Parse the file; reject on a non-current `version`; reject on a stale
//!    timestamp (unless `force`).
//! 2. Collect the set of live managed windows' `HWND`s (skipping `Ignored`).
//! 3. **Phase B (resolve, no mutation):** for each loadout slot, pop its
//!    stored `HWND` from the live set. If any slot's `HWND` is not live →
//!    abort with a diagnostic error naming `exe`/`title`, zero state touched.
//! 4. **Phase D (apply):** per-workspace `set_layout` + `replace_all`, then
//!    sync registry slots/rects.
//! 5. Leftover tiling windows (still live but unreferenced by the loadout)
//!    are appended as new columns on the active workspace.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::common::{Rect, WindowId};
use crate::ipc::message::SocketResponse;
use crate::layout::types::{ActualLayout, AppliedLayout};
use crate::loadout::{
    ColumnSnapshot, FloatingEntry, LoadoutFile, RectJson, RowSnapshot, ScrollingSnapshot,
    WindowRef, WorkspaceSnapshot, is_stale,
};
use crate::registry::hooks::set_float_hwnds;
use crate::registry::types::{FloatingState, WindowState};
use crate::workspace::{Workspace, WorkspaceId, workspace_y_offset};
use windows::Win32::Foundation::HWND;

use super::types::FlowWM;

// ── Pure helpers (unit-testable without FlowWM) ────────────────────

/// Convert a [`RectJson`] (loadout file format, `w`/`h` fields) to a
/// [`Rect`] (project-wide format, `width`/`height` fields).
#[must_use]
fn rect_json_to_rect(rj: &RectJson) -> Rect {
    Rect {
        x: rj.x,
        y: rj.y,
        width: rj.w,
        height: rj.h,
    }
}

/// Resolve a loadout slot's stored [`WindowRef`] against the live set.
///
/// On a hit, removes the slot's `HWND` from `live` (so each live window is
/// claimed by at most one slot) and returns the matching [`WindowId`]. On a
/// miss, returns an error naming the slot's diagnostic `exe`/`title` so a
/// failed restore can identify which application did not come back — that
/// identity is only known at save time, which is why `exe`/`title` are
/// persisted in the file despite never driving the match.
///
/// This is the whole matcher: a single exact `HWND` lookup, no scoring.
fn resolve_hwnd(live: &mut HashSet<isize>, window: &WindowRef) -> Result<WindowId, String> {
    if live.remove(&window.hwnd) {
        Ok(WindowId(window.hwnd))
    } else {
        Err(format!(
            "window not currently open: \"{}\" ({}, hwnd {:#x}) — aborting load (no-partial)",
            window.title, window.exe, window.hwnd as u64
        ))
    }
}

/// Resolve the seating target from the loadout's saved active-workspace flag.
///
/// Returns the [`WorkspaceId`] of the first snapshot marked `active: true`,
/// falling back to workspace 1 when none is marked. The `active` flag is
/// persisted per workspace at save time (exactly one snapshot is marked) but
/// was historically ignored at load — this reads it for the first time so the
/// workspace the user had visible is the one seated on restore.
///
/// Pure: operates only on the parsed snapshot list — no daemon state, no Win32.
/// The foreground-first refinement (foreground window's workspace → this
/// fallback → workspace 1) layers on top of this elsewhere.
#[must_use]
fn saved_active_target(workspaces: &[WorkspaceSnapshot]) -> WorkspaceId {
    workspaces
        .iter()
        .find(|ws| ws.active)
        .map(|ws| WorkspaceId(ws.workspace_id))
        .unwrap_or(WorkspaceId(1))
}

/// Find which workspace contains a given window, searching both each
/// workspace's scrolling space and its floating space.
///
/// Returns the [`WorkspaceId`] of the first workspace whose tiling columns or
/// floating set holds `window_id`, or `None` when no workspace owns it. The
/// search is exhaustive across both spaces because a window is seated in
/// exactly one — tiles live in `scrolling`, floats in `floating` — so the first
/// hit is authoritative.
///
/// Pure: walks the workspace slice's read-only views only — no daemon state, no
/// Win32. Used by the seating-target resolution to seat the workspace that
/// currently holds the OS foreground window (read separately via
/// `get_foreground_window`).
#[must_use]
fn workspace_containing_window(
    workspaces: &[Workspace],
    window_id: WindowId,
) -> Option<WorkspaceId> {
    workspaces.iter().find_map(|ws| {
        let in_scrolling = ws
            .scrolling
            .virtual_layout()
            .columns
            .iter()
            .any(|col| col.rows.iter().any(|row| row.window_id == window_id));
        let in_floating = ws.floating.contains(window_id);
        if in_scrolling || in_floating {
            Some(ws.id)
        } else {
            None
        }
    })
}

/// Resolve the seating target, preferring the workspace holding the OS
/// foreground window.
///
/// Order: foreground window's workspace → the loadout's saved active workspace
/// (via [`saved_active_target`], itself falling back to workspace 1). When the
/// foreground window is absent (`foreground: None`, e.g. desktop has focus) or
/// not managed by any workspace, resolution degrades to the saved-active
/// fallback unchanged.
///
/// The foreground is read-only here — this never calls `SetForegroundWindow`.
/// Seating the foreground's workspace keeps that window on-screen by
/// construction (it is the workspace the focused window actually lives in),
/// sidestepping the Windows startup foreground lock. The caller resolves the
/// foreground HWND against the post-apply workspaces, so `workspace_containing`
/// reports where the foreground window NOW sits after the loadout is applied.
#[must_use]
fn resolve_seating_target(
    workspaces: &[Workspace],
    foreground: Option<WindowId>,
    saved: &[WorkspaceSnapshot],
) -> WorkspaceId {
    foreground
        .and_then(|wid| workspace_containing_window(workspaces, wid))
        .unwrap_or_else(|| saved_active_target(saved))
}

/// Build the seating batch list that seats every non-empty workspace relative
/// to a target workspace.
///
/// For each non-empty workspace on the monitor, merges its scrolling and
/// floating actual layouts into one batch entry and computes that workspace's
/// vertical parking offset relative to `target_id` via
/// [`workspace_y_offset`]: the target sits at offset `0` (visible), every other
/// workspace parks off-screen above (`-`) or below (`+`). Workspaces with no
/// windows in either space are skipped — there is nothing to seat.
///
/// Pure: no Win32, no animator, no daemon state. The caller submits the
/// returned `&[(ActualLayout, i32)]` through
/// [`FlowWM::animate_workspaces`](super::types::FlowWM::animate_workspaces),
/// which performs the visible-rect → window-rect translation, flattens border
/// overlays, and arms float-tracking suppression.
///
/// # Why this is separate from the workspace-switch path
///
/// This is structurally the same merge+offset loop that
/// `switch_workspace_layout` performs inline, lifted out as a pure function so
/// the load path can share the math **without** reusing the switch path's
/// bystander-teleport step. That teleport is correct for a real switch
/// (bystanders are already parked off-screen, so the snap is invisible) but
/// wrong at restore time, where every window begins on-screen — reusing it
/// would visibly snap on-screen windows off-screen. The full rationale is
/// restated briefly at the call site in `apply_loadout`.
#[must_use]
fn build_seating_batches(
    workspaces: &[Workspace],
    target_id: WorkspaceId,
    monitor_height: i32,
    window_gap: i32,
) -> Vec<(ActualLayout, i32)> {
    let mut batches = Vec::new();
    for ws in workspaces {
        let scroll_actual = ws.scrolling.actual_layout();
        let float_actual = ws.floating.to_actual_layout();
        // Skip workspaces with nothing to seat — an empty workspace has no
        // windows to park, and emitting an empty batch entry would only add
        // noise (the animator drops empty target lists anyway).
        if scroll_actual.entries.is_empty() && float_actual.entries.is_empty() {
            continue;
        }
        let y_offset = workspace_y_offset(ws.id, target_id, monitor_height, window_gap);
        // Merge scrolling + floating into one entry so a single workspace's
        // tiles and floats animate together at the same parking offset.
        let mut entries = scroll_actual.entries.clone();
        entries.extend(float_actual.entries.iter().cloned());
        batches.push((ActualLayout { entries }, y_offset));
    }
    batches
}

impl FlowWM {
    /// Save the current workspace arrangement to a loadout file.
    ///
    /// `path` overrides the config-default location; `None` resolves to
    /// `config_dir / config.loadout.default_path`. Returns [`SocketResponse::Ok`]
    /// on success, [`SocketResponse::Error`] on failure.
    pub(super) fn dispatch_loadout_save(&self, path: Option<PathBuf>) -> SocketResponse {
        let resolved =
            path.unwrap_or_else(|| self.config_dir.join(&self.config.loadout.default_path));

        match self.build_and_write_loadout(&resolved) {
            Ok(()) => SocketResponse::Ok,
            Err(e) => {
                log::warn!("loadout save to {resolved:?} failed: {e}");
                SocketResponse::Error {
                    message: format!("loadout save failed: {e}"),
                }
            }
        }
    }

    /// Best-effort save to the config-default path, used by save-on-stop.
    ///
    /// Never blocks shutdown — errors are logged as warnings and discarded.
    pub(super) fn try_save_loadout_default(&self) -> Result<(), String> {
        let default = self.config_dir.join(&self.config.loadout.default_path);
        self.build_and_write_loadout(&default)
    }

    /// Load a saved loadout and apply it to live windows (IPC dispatch).
    ///
    /// Thin wrapper over [`Self::apply_loadout`] that maps the `Result` to a
    /// [`SocketResponse`] for the IPC layer. `force: true` (manual
    /// `flow loadout load`) ignores staleness; `force: false` honors
    /// `config.loadout.max_age_secs`.
    pub(super) fn dispatch_loadout_load(
        &mut self,
        path: Option<PathBuf>,
        force: bool,
    ) -> SocketResponse {
        match self.apply_loadout(path, force) {
            Ok(()) => SocketResponse::Ok,
            Err(e) => {
                log::warn!("loadout load failed: {e}");
                SocketResponse::Error { message: e }
            }
        }
    }

    /// Best-effort auto-restore of the config-default loadout at startup.
    ///
    /// Called from the daemon's boot sequence (see `main::run`) right after
    /// [`FlowWM::new`](Self::new) finishes initializing the registry and
    /// layout, and before the IPC event loop starts. Runs the same code path
    /// as `dispatch_loadout_load` with `force: false`, so the
    /// `max_age_secs` staleness guard applies.
    ///
    /// Never blocks startup: a missing file (first run), a stale loadout
    /// (post-crash), a parse failure, or an unmatched-window abort are all
    /// logged and silently continue — the just-built init layout remains in
    /// place. Performing restore here (rather than via an IPC round-trip
    /// from the CLI) sidesteps the startup pipe race entirely.
    pub fn try_restore_loadout_default(&mut self) {
        let default = self.config_dir.join(&self.config.loadout.default_path);
        if !default.exists() {
            log::debug!("loadout restore: no loadout file at {default:?}, starting fresh");
            return;
        }
        if let Err(e) = self.apply_loadout(Some(default), false) {
            log::info!("loadout restore skipped: {e}");
        }
    }

    /// Core load algorithm: resolve a loadout file onto live windows.
    ///
    /// Shared by [`Self::dispatch_loadout_load`] (IPC) and
    /// [`Self::try_restore_loadout_default`] (startup) so both paths behave
    /// identically. See the [module docs](self) for the no-partial algorithm.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` if the file cannot be read, parsed, or if any
    /// loadout slot cannot be matched to a live window (whole load aborted,
    /// zero state touched). A stale loadout (when `force` is false) is a
    /// silent `Ok` skip, not an error.
    fn apply_loadout(&mut self, path: Option<PathBuf>, force: bool) -> Result<(), String> {
        let resolved =
            path.unwrap_or_else(|| self.config_dir.join(&self.config.loadout.default_path));

        // 1. Read + parse
        let raw = std::fs::read_to_string(&resolved)
            .map_err(|e| format!("loadout load: failed to read {resolved:?}: {e}"))?;
        let file: LoadoutFile = serde_json::from_str(&raw)
            .map_err(|e| format!("loadout load: failed to parse {resolved:?}: {e}"))?;

        // 2. Version guard: reject any non-current schema up front. A legacy
        //    pre-`HWND` file cannot be migrated (HWND can't be synthesized),
        //    so it is rejected with a reason rather than silently misread.
        if file.version != LoadoutFile::CURRENT_VERSION {
            return Err(format!(
                "unsupported file version {} (expected {}) — rejecting {resolved:?}",
                file.version,
                LoadoutFile::CURRENT_VERSION
            ));
        }

        // 3. Staleness guard (silent skip, NOT an error)
        if !force && is_stale(&file.saved_at, self.config.loadout.max_age_secs) {
            log::info!(
                "loadout load: skipping stale loadout from {}",
                file.saved_at
            );
            return Ok(());
        }

        // 4. Phase B: resolve every loadout slot's HWND against the live set
        //    (NO mutation of daemon state). Collect live managed windows by
        //    HWND first (skip Ignored — they belong to no loadout), then pop
        //    each slot's stored HWND from that set. `HWND` is the sole identity
        //    key — a singleton entry per live window — so matching is a trivial
        //    membership check; the first slot whose HWND is not live aborts the
        //    entire load (no partial application).
        let mut live: HashSet<isize> = HashSet::new();
        for win in self.registry.windows() {
            if matches!(win.state, WindowState::Ignored(_)) {
                continue;
            }
            live.insert(win.hwnd.0 as isize);
        }

        struct ResolvedWorkspace {
            workspace_id: u32,
            columns: Vec<(u32, Vec<WindowId>)>,
            focus: Option<WindowId>,
            viewport_offset: i32,
            floating: Vec<(WindowId, Rect)>,
        }

        let mut resolved_workspaces: Vec<ResolvedWorkspace> = Vec::new();
        for ws_snap in &file.workspaces {
            let mut columns = Vec::new();

            for col_snap in &ws_snap.scrolling.columns {
                let mut rows = Vec::new();
                for row_snap in &col_snap.rows {
                    rows.push(resolve_hwnd(&mut live, &row_snap.window)?);
                }
                columns.push((col_snap.width_px, rows));
            }

            // Resolve floating entries by HWND.
            let mut floating = Vec::new();
            for float_entry in &ws_snap.floating {
                let hwnd = resolve_hwnd(&mut live, &float_entry.window)?;
                let rect = rect_json_to_rect(&float_entry.rect);
                floating.push((hwnd, rect));
            }

            // Focus resolves directly by stored HWND: it must be one of the
            // tiled windows already assigned to a column above. (The focus
            // window was popped from `live` as a column row, so we look it up
            // among the resolved columns rather than re-querying `live`.)
            let focus = ws_snap.scrolling.focus.as_ref().and_then(|focus_ref| {
                let focus_hwnd = focus_ref.hwnd;
                columns
                    .iter()
                    .flat_map(|(_, rows)| rows.iter())
                    .find(|wid| wid.0 == focus_hwnd)
                    .copied()
            });

            resolved_workspaces.push(ResolvedWorkspace {
                workspace_id: ws_snap.workspace_id,
                columns,
                focus,
                viewport_offset: ws_snap.scrolling.viewport_offset,
                floating,
            });
        }

        // 5. Leftover tiling windows: live windows still in `live` (not claimed
        //    by any slot) that are in the Tiling state. These are appended as
        //    new columns on the active workspace below. Floating leftovers are
        //    handled per-workspace by `replace_all`; Ignored windows never
        //    entered `live`.
        let mut leftovers: Vec<WindowId> = Vec::new();
        for win in self.registry.windows() {
            if !matches!(win.state, WindowState::Tiling(_)) {
                continue;
            }
            let hwnd = win.hwnd.0 as isize;
            if live.contains(&hwnd) {
                leftovers.push(WindowId(hwnd));
            }
        }

        // 6. Phase D: apply per-workspace
        for rw in resolved_workspaces {
            let ws_id = WorkspaceId(rw.workspace_id);
            // Find which monitor owns this workspace, and get its index.
            let (monitor_idx, ws_idx) = {
                let mut found = None;
                for (mi, monitor) in self.monitors.iter().enumerate() {
                    if let Some(wsi) = monitor.find_workspace_index(ws_id) {
                        found = Some((mi, wsi));
                        break;
                    }
                }
                match found {
                    Some(pair) => pair,
                    None => {
                        log::warn!(
                            "loadout load: workspace {} not found on any monitor, skipping",
                            rw.workspace_id
                        );
                        continue;
                    }
                }
            };

            // We need &mut access to both the workspace (for set_layout +
            // replace_all) and the registry (for slot/rect sync). Use index-
            // based access with a temporary scope so the ws borrow is dropped
            // before we touch the registry.
            // Collect the canonical (wid, rect) pairs from the workspace's own
            // FloatingSpace AFTER replace_all so we sync exactly what was stored.
            let (app, floats_to_sync): (AppliedLayout, Vec<(WindowId, Rect)>) = {
                let ws = &mut self.monitors[monitor_idx].workspaces_mut()[ws_idx];
                let app = ws
                    .scrolling
                    .set_layout(rw.columns, rw.focus, rw.viewport_offset);
                ws.floating.replace_all(rw.floating);
                let floats_to_sync = ws
                    .floating
                    .windows()
                    .iter()
                    .map(|entry| (entry.window_id, entry.rect))
                    .collect();
                (app, floats_to_sync)
            };

            // ws borrow dropped — safe to touch the registry now.
            self.registry
                .update_tiling_slots_from_layout(&app.virtual_layout);
            self.registry.update_tiled_rects(&app.actual_layout);
            // Mirror each float placement into the registry so border color and
            // float-location tracking are correct (mirrors register_float).
            for (wid, rect) in &floats_to_sync {
                if let Some(window) = self.registry.get_window_mut(HWND(wid.0 as *mut _)) {
                    window.state = WindowState::Floating(FloatingState::Active { rect: *rect });
                }
            }
        }

        // 7. Leftover tiling windows: append as columns on the active workspace
        for wid in leftovers {
            let applied = self.active_scrolling_mut().add_window(wid);
            // Sync registry for the added window.
            self.registry
                .update_tiling_slots_from_layout(&applied.virtual_layout);
            self.registry.update_tiled_rects(&applied.actual_layout);
        }

        // 8. Resolve the seating target. Preferred: the workspace that holds
        //    the OS foreground window — read-only (`GetForegroundWindow`, never
        //    `SetForegroundWindow`), so seating it keeps the focused window
        //    on-screen by construction and avoids the startup foreground lock.
        //    The foreground is read here, AFTER the layout apply, so
        //    `workspace_containing_window` reports where the foreground window
        //    now sits after the loadout seated it. First fallback is the
        //    loadout's saved active workspace (`saved_active_target`, itself
        //    falling back to workspace 1) — used when the foreground is absent
        //    (desktop focus) or belongs to no managed workspace.
        let foreground = crate::registry::win32::get_foreground_window().map(WindowId);
        let target_id = resolve_seating_target(
            self.active_monitor().workspaces(),
            foreground,
            &file.workspaces,
        );

        // 9. Make the target workspace the visible one. The apply above set
        //    every workspace's logical layout; this selects which one is on
        //    screen — the first half of a workspace switch, without its
        //    teleport step (see step 11 for why the teleport is wrong here).
        //    The target is always present on the active monitor in the
        //    single-monitor daemon; the guard keeps a missing target from
        //    panicking and leaves the freshly-applied layout visible.
        if self
            .active_monitor_mut()
            .set_active_workspace(target_id)
            .is_none()
        {
            log::warn!(
                "loadout load: seating target workspace {} not found on the active monitor; \
                 leaving the freshly-tiled layout in place",
                target_id.0
            );
            log::info!("loadout load: applied layout from {resolved:?} (seating skipped)");
            return Ok(());
        }

        // 10. Rebuild the TARGET workspace's float-tracking set so
        //     LOCATIONCHANGE forwarding and animator float-suppression reflect
        //     the loaded layout (mirrors switch_workspace_layout). Parked
        //     workspaces' floats are excluded — they are off-screen and must
        //     not be draggable, and excluding them prevents a stray
        //     LOCATIONCHANGE during the seating animation from corrupting a
        //     parked workspace's rect.
        let target_floats: Vec<isize> = self
            .active_workspace()
            .floating
            .windows()
            .iter()
            .map(|entry| entry.window_id.0)
            .collect();
        set_float_hwnds(&target_floats);

        // 11. Seat the full workspace stack in one animation: the target
        //     workspace settles at offset 0 (visible) and every other
        //     non-empty workspace animates to its parking offset. This is the
        //     core fix — previously only the active workspace animated, so the
        //     physical workspace stacking was never established at load time
        //     and only appeared after a manual workspace switch. The pure
        //     `build_seating_batches` helper builds the batch; see its docs for
        //     why this path deliberately shares the switch path's merge+offset
        //     math but not its bystander-teleport step.
        let monitor_height = self.active_monitor().screen_rect().height;
        let window_gap = self.active_scrolling().padding().window_gap;
        let batches = build_seating_batches(
            self.active_monitor().workspaces(),
            target_id,
            monitor_height,
            window_gap,
        );
        self.animate_workspaces(&batches);

        log::info!("loadout load: restored layout from {resolved:?}");
        Ok(())
    }

    // ── Save implementation detail ────────────────────────────────────

    /// Build a [`LoadoutFile`] from the current live state and write it to
    /// `path` as pretty-printed JSON.
    fn build_and_write_loadout(&self, path: &PathBuf) -> Result<(), String> {
        let workspaces = self.snapshot_workspaces();
        let file = LoadoutFile {
            version: LoadoutFile::CURRENT_VERSION,
            saved_at: chrono::Utc::now().to_rfc3339(),
            workspaces,
        };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|e| format!("serde serialization failed: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("failed to write {path:?}: {e}"))?;
        Ok(())
    }

    /// Walk every monitor → workspace and produce a [`WorkspaceSnapshot`] per
    /// workspace.
    fn snapshot_workspaces(&self) -> Vec<WorkspaceSnapshot> {
        let mut snapshots = Vec::new();
        for monitor in &self.monitors {
            let active_ws_id = monitor.active_workspace_id();
            for ws in monitor.workspaces() {
                let scrolling = self.snapshot_scrolling(ws);
                let floating = self.snapshot_floating(ws);
                snapshots.push(WorkspaceSnapshot {
                    workspace_id: ws.id.0,
                    active: ws.id == active_ws_id,
                    scrolling,
                    floating,
                });
            }
        }
        snapshots
    }

    /// Snapshot the tiling area of a single workspace.
    fn snapshot_scrolling(&self, ws: &crate::workspace::Workspace) -> ScrollingSnapshot {
        let vl = ws.scrolling.virtual_layout();
        let viewport_offset = vl.viewport_offset;

        // Focus: translate last_focused_window WindowId → WindowRef
        let focus = ws
            .scrolling
            .last_focused_window()
            .and_then(|wid| self.window_ref_for(wid));

        let columns: Vec<ColumnSnapshot> = vl
            .columns
            .iter()
            .map(|col| {
                let rows: Vec<RowSnapshot> = col
                    .rows
                    .iter()
                    .filter_map(|row| {
                        self.window_ref_for(row.window_id).map(|wr| RowSnapshot {
                            window: wr,
                            height_px: row.height,
                        })
                    })
                    .collect();
                ColumnSnapshot {
                    width_px: col.width_px as u32,
                    rows,
                }
            })
            .collect();

        ScrollingSnapshot {
            viewport_offset,
            focus,
            columns,
        }
    }

    /// Snapshot the floating area of a single workspace.
    fn snapshot_floating(&self, ws: &crate::workspace::Workspace) -> Vec<FloatingEntry> {
        ws.floating
            .windows()
            .iter()
            .filter_map(|entry| {
                self.window_ref_for(entry.window_id)
                    .map(|wr| FloatingEntry {
                        window: wr,
                        rect: RectJson {
                            x: entry.rect.x,
                            y: entry.rect.y,
                            w: entry.rect.width,
                            h: entry.rect.height,
                        },
                    })
            })
            .collect()
    }

    /// Translate a [`WindowId`] to a [`WindowRef`] keyed by its `HWND`, with
    /// `exe`/`title` carried as diagnostics.
    ///
    /// The matcher reads only `hwnd`; `exe`/`title` are persisted so a failed
    /// restore can name the missing window. Returns `None` (with a warning) if
    /// the window is not found in the registry.
    fn window_ref_for(&self, wid: WindowId) -> Option<WindowRef> {
        let win = self.registry.get_window(HWND(wid.0 as *mut _))?;
        Some(WindowRef {
            hwnd: wid.0,
            exe: win.exe.clone(),
            title: win.title.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::types::{MonitorInfo, Padding};
    use crate::workspace::{ScrollingSpace, Workspace};

    // ── rect_json_to_rect: pure conversion ───────────────────────────

    /// Positive: RectJson → Rect converts field names correctly.
    #[test]
    fn rect_json_to_rect_correct() {
        let rj = RectJson {
            x: 10,
            y: 20,
            w: 300,
            h: 400,
        };
        let r = rect_json_to_rect(&rj);
        assert_eq!(
            r,
            Rect {
                x: 10,
                y: 20,
                width: 300,
                height: 400
            }
        );
    }

    /// Positive: zero rect survives round-trip.
    #[test]
    fn rect_json_to_rect_zero() {
        let rj = RectJson {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        let r = rect_json_to_rect(&rj);
        assert_eq!(
            r,
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0
            }
        );
    }

    // ── is_stale delegation ──────────────────────────────────────────

    /// Positive: is_stale is callable from loadout module.
    #[test]
    fn is_stale_callable() {
        let old = "2020-01-01T00:00:00Z";
        assert!(is_stale(old, 60));
    }

    // ── saved_active_target: saved active-workspace flag ───────────────
    //
    // The loadout persists an `active` flag per workspace at save time; the
    // seating target is the id of the snapshot marked active, falling back to
    // workspace 1 when none is marked (defensive — save always marks exactly
    // one, but a hand-edited or all-false file must still resolve).

    /// Positive: the snapshot marked active resolves to its workspace id.
    #[test]
    fn saved_active_target_picks_the_marked_workspace() {
        let snaps = vec![
            WorkspaceSnapshot {
                workspace_id: 1,
                active: false,
                scrolling: empty_scrolling_snap(),
                floating: vec![],
            },
            WorkspaceSnapshot {
                workspace_id: 2,
                active: true,
                scrolling: empty_scrolling_snap(),
                floating: vec![],
            },
        ];
        assert_eq!(saved_active_target(&snaps), WorkspaceId(2));
    }

    /// Negative: no snapshot marked active → workspace 1 fallback.
    #[test]
    fn saved_active_target_falls_back_to_workspace_one_when_none_marked() {
        let snaps = vec![WorkspaceSnapshot {
            workspace_id: 3,
            active: false,
            scrolling: empty_scrolling_snap(),
            floating: vec![],
        }];
        assert_eq!(saved_active_target(&snaps), WorkspaceId(1));
    }

    /// Negative: empty snapshot list → workspace 1 fallback.
    #[test]
    fn saved_active_target_falls_back_to_workspace_one_when_empty() {
        assert_eq!(saved_active_target(&[]), WorkspaceId(1));
    }

    // ── build_seating_batches: full-stack seating output ──────────────
    //
    // The core contract: for a given target, the builder returns EVERY
    // non-empty workspace (not just the active one), each merged across
    // scrolling + floating, at its correct parking offset relative to the
    // target. Empty workspaces are absent.

    /// Positive: every non-empty workspace appears at its correct offset for a
    /// target at the top of the stack. Windows on ws 1 and ws 3, target ws 1 →
    /// ws 1 at 0, ws 3 below at +Y_UNIT. Empty ws 2 is absent.
    #[test]
    fn seating_batches_cover_every_nonempty_workspace() {
        let workspaces = vec![
            make_workspace_with_tile(1, WindowId(100)),
            make_empty_workspace(2),
            make_workspace_with_tile(3, WindowId(200)),
        ];
        let batches = build_seating_batches(&workspaces, WorkspaceId(1), MONITOR_H, GAP);

        // Two non-empty workspaces; the empty middle one is skipped.
        assert_eq!(batches.len(), 2, "empty workspace must be skipped");
        // Target workspace sits at offset 0 (visible).
        assert_eq!(batches[0].1, 0, "target workspace parks at offset 0");
        assert!(
            batches[0]
                .0
                .entries
                .iter()
                .any(|e| e.window_id == WindowId(100)),
            "target workspace's tile must be in its merged batch"
        );
        // Workspace 3 is below the target → parks at +Y_UNIT.
        assert_eq!(batches[1].1, Y_UNIT, "ws 3 parks below target at +Y_UNIT");
        assert!(
            batches[1]
                .0
                .entries
                .iter()
                .any(|e| e.window_id == WindowId(200)),
            "ws 3's tile must be in its merged batch"
        );
    }

    /// Positive: a target in the middle produces both an above (negative) and a
    /// below (positive) offset around the offset-0 target.
    #[test]
    fn seating_batches_offset_signs_around_a_middle_target() {
        let workspaces = vec![
            make_workspace_with_tile(1, WindowId(100)),
            make_workspace_with_tile(2, WindowId(200)),
            make_workspace_with_tile(3, WindowId(300)),
        ];
        let batches = build_seating_batches(&workspaces, WorkspaceId(2), MONITOR_H, GAP);

        // Offsets in workspace-id order: ws1 above (-Y_UNIT), ws2 target (0),
        // ws3 below (+Y_UNIT).
        assert_eq!(
            batches.iter().map(|(_, off)| *off).collect::<Vec<_>>(),
            vec![-Y_UNIT, 0, Y_UNIT],
            "offsets must be [-Y_UNIT, 0, +Y_UNIT] around a middle target"
        );
    }

    /// Positive: a workspace with ONLY a floating window (no tiles) still gets
    /// a batch entry — floats are part of the merge and must be seated too.
    #[test]
    fn seating_batches_include_float_only_workspaces() {
        let mut ws = make_empty_workspace(2);
        ws.floating.add(
            WindowId(400),
            Rect {
                x: 10,
                y: 10,
                width: 100,
                height: 100,
            },
        );
        let batches = build_seating_batches(&[ws], WorkspaceId(2), MONITOR_H, GAP);

        assert_eq!(
            batches.len(),
            1,
            "float-only workspace must produce a batch"
        );
        assert_eq!(batches[0].1, 0, "target workspace at offset 0");
        assert!(
            batches[0]
                .0
                .entries
                .iter()
                .any(|e| e.window_id == WindowId(400)),
            "floating window must be merged into the batch"
        );
    }

    /// Positive: a workspace holding BOTH a tile and a float merges both into a
    /// single batch entry (one entry per workspace, not one per space).
    #[test]
    fn seating_batches_merge_scrolling_and_float_into_one_entry() {
        let mut ws = make_workspace_with_tile(1, WindowId(100));
        ws.floating.add(
            WindowId(101),
            Rect {
                x: 20,
                y: 20,
                width: 80,
                height: 80,
            },
        );
        let batches = build_seating_batches(&[ws], WorkspaceId(1), MONITOR_H, GAP);

        assert_eq!(batches.len(), 1);
        let ids: Vec<isize> = batches[0].0.entries.iter().map(|e| e.window_id.0).collect();
        assert!(ids.contains(&100), "tile merged");
        assert!(ids.contains(&101), "float merged");
    }

    /// Negative: when every workspace is empty, no batches are produced.
    #[test]
    fn seating_batches_empty_when_all_workspaces_empty() {
        let workspaces = vec![make_empty_workspace(1), make_empty_workspace(2)];
        let batches = build_seating_batches(&workspaces, WorkspaceId(1), MONITOR_H, GAP);
        assert!(batches.is_empty());
    }

    // ── workspace_containing_window: workspace lookup by window ──────────
    //
    // Seating-target resolution needs to map a foreground window back to the
    // workspace it lives in. The search must cover BOTH spaces: a tile in the
    // scrolling canvas and a float in the floating set.

    /// Positive: a tiled window resolves to its workspace's id.
    #[test]
    fn workspace_containing_finds_tiled_window() {
        let workspaces = vec![
            make_empty_workspace(1),
            make_workspace_with_tile(2, WindowId(111)),
        ];
        assert_eq!(
            workspace_containing_window(&workspaces, WindowId(111)),
            Some(WorkspaceId(2))
        );
    }

    /// Positive: a floating window resolves to its workspace's id — the search
    /// covers the floating space, not just tiles.
    #[test]
    fn workspace_containing_finds_floating_window() {
        let mut ws = make_empty_workspace(3);
        ws.floating.add(
            WindowId(222),
            Rect {
                x: 5,
                y: 5,
                width: 50,
                height: 50,
            },
        );
        let workspaces = vec![make_empty_workspace(1), ws];
        assert_eq!(
            workspace_containing_window(&workspaces, WindowId(222)),
            Some(WorkspaceId(3))
        );
    }

    /// Negative: an unmanaged / absent window resolves to none — no workspace
    /// claims it, so the foreground lookup falls back to saved-active.
    #[test]
    fn workspace_containing_returns_none_for_absent_window() {
        let workspaces = vec![make_workspace_with_tile(1, WindowId(100))];
        assert_eq!(
            workspace_containing_window(&workspaces, WindowId(999)),
            None
        );
    }

    /// Negative: empty workspace list → none.
    #[test]
    fn workspace_containing_returns_none_when_no_workspaces() {
        assert_eq!(workspace_containing_window(&[], WindowId(1)), None);
    }

    // ── resolve_seating_target: foreground-first ordering ───────────────
    //
    // Target order: foreground window's workspace → saved-active → workspace 1.

    /// Positive: when the foreground window sits in a managed workspace, that
    /// workspace wins over the saved-active flag.
    #[test]
    fn seating_target_prefers_foreground_workspace() {
        let workspaces = vec![
            make_workspace_with_tile(1, WindowId(100)),
            make_workspace_with_tile(2, WindowId(200)),
        ];
        // Saved-active is workspace 1, but the foreground window lives on ws 2.
        let saved = vec![WorkspaceSnapshot {
            workspace_id: 1,
            active: true,
            scrolling: empty_scrolling_snap(),
            floating: vec![],
        }];
        assert_eq!(
            resolve_seating_target(&workspaces, Some(WindowId(200)), &saved),
            WorkspaceId(2)
        );
    }

    /// Fallback: foreground present but unmanaged → saved-active workspace.
    #[test]
    fn seating_target_falls_back_to_saved_when_foreground_unmanaged() {
        let workspaces = vec![
            make_workspace_with_tile(1, WindowId(100)),
            make_workspace_with_tile(2, WindowId(200)),
        ];
        // Foreground window 999 is not on any workspace → saved-active ws 2.
        let saved = vec![WorkspaceSnapshot {
            workspace_id: 2,
            active: true,
            scrolling: empty_scrolling_snap(),
            floating: vec![],
        }];
        assert_eq!(
            resolve_seating_target(&workspaces, Some(WindowId(999)), &saved),
            WorkspaceId(2)
        );
    }

    /// Fallback: no foreground (desktop has focus) → saved-active workspace.
    #[test]
    fn seating_target_falls_back_to_saved_when_no_foreground() {
        let workspaces = vec![make_workspace_with_tile(1, WindowId(100))];
        let saved = vec![WorkspaceSnapshot {
            workspace_id: 1,
            active: true,
            scrolling: empty_scrolling_snap(),
            floating: vec![],
        }];
        assert_eq!(
            resolve_seating_target(&workspaces, None, &saved),
            WorkspaceId(1)
        );
    }

    /// Fallback: foreground unmanaged AND no saved-active → workspace 1.
    #[test]
    fn seating_target_falls_back_to_workspace_one() {
        let workspaces = vec![make_workspace_with_tile(3, WindowId(100))];
        assert_eq!(
            resolve_seating_target(&workspaces, Some(WindowId(999)), &[]),
            WorkspaceId(1)
        );
    }

    // ── Test helpers ────────────────────────────────────────────────────
    /// Test monitor height and gap — mirror `y_offset.rs` test constants so the
    /// parking unit matches: `Y_UNIT = MONITOR_H + GAP`.
    const MONITOR_H: i32 = 1080;
    const GAP: i32 = 4;
    const Y_UNIT: i32 = MONITOR_H + GAP; // 1084

    /// Build a [`ScrollingSpace`] sized like the canonical 1920×1080 test
    /// monitor, mirroring `workspace::monitor::tests::make_scrolling`.
    fn make_scrolling() -> ScrollingSpace {
        ScrollingSpace::new(
            MonitorInfo {
                work_area: Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            },
            960,
            320,
            100,
            Padding {
                window_gap: GAP,
                up: 0,
                down: 0,
            },
            4,
        )
    }

    /// Build a workspace that owns one tiled window.
    fn make_workspace_with_tile(id: u32, wid: WindowId) -> Workspace {
        let mut scrolling = make_scrolling();
        scrolling.add_window(wid);
        Workspace::new(WorkspaceId(id), scrolling)
    }

    /// Build an empty workspace (no tiles, no floats).
    fn make_empty_workspace(id: u32) -> Workspace {
        Workspace::new(WorkspaceId(id), make_scrolling())
    }

    /// An all-zero [`ScrollingSnapshot`] for [`saved_active_target`] tests that
    /// only care about the `active` flag.
    fn empty_scrolling_snap() -> ScrollingSnapshot {
        ScrollingSnapshot {
            viewport_offset: 0,
            focus: None,
            columns: vec![],
        }
    }
}
