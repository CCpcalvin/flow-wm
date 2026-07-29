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
use crate::layout::types::AppliedLayout;
use crate::loadout::{
    ColumnSnapshot, FloatingEntry, LoadoutFile, RectJson, RowSnapshot, ScrollingSnapshot,
    WindowRef, WorkspaceSnapshot, is_stale,
};
use crate::registry::hooks::set_float_hwnds;
use crate::registry::types::{FloatingState, WindowState};
use crate::workspace::WorkspaceId;
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

        // 8. Rebuild the active workspace's float-tracking set so LOCATIONCHANGE
        //    forwarding and animator float-suppression reflect the loaded layout
        //    (mirrors switch_workspace_layout).
        let active_floats: Vec<isize> = self
            .active_workspace()
            .floating
            .windows()
            .iter()
            .map(|entry| entry.window_id.0)
            .collect();
        set_float_hwnds(&active_floats);

        // 9. Animate the active workspace's final layout, merging scrolling +
        //    floating so float HWNDs are also moved to their restored rects
        //    (mirrors set_window_to_float's batched animate_workspaces call).
        let scroll_actual = self.active_scrolling().actual_layout().clone();
        let float_actual = self.active_workspace().floating.to_actual_layout();
        let batches = [(scroll_actual, 0), (float_actual, 0)];
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
}
