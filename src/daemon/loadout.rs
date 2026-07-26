//! Loadout save/restore orchestration.
//!
//! This module implements the daemon-side logic for serializing the current
//! workspace arrangement to a versioned JSON file (save) and resolving a
//! saved file back onto live windows (load). Both paths are wired through
//! [`FlowWM::dispatch`] and share a single save code path so that
//! `flow loadout save` and the save-on-stop hook behave identically.
//!
//! # Save algorithm
//!
//! Walk every monitor → workspace → join the virtual layout (columns, rows,
//! viewport offset, focus) with per-window registry metadata (exe, class,
//! title). Swap `WindowId` for [`WindowRef`] triples. Serialize as JSON.
//!
//! # Load algorithm (no-partial guarantee)
//!
//! 1. Parse the file; reject on stale timestamp (unless `force`).
//! 2. Build a `HashMap<(exe,class,title), Vec<WindowId>>` pool of live
//!    tiling/floating windows (skipping `Ignored`).
//! 3. **Phase B (resolve, no mutation):** greedily pop from each triple's
//!    pool. If any slot finds a dry pool → abort with error, zero state
//!    touched.
//! 4. **Phase D (apply):** per-workspace `set_layout` + `replace_all`, then
//!    sync registry slots/rects.
//! 5. Leftover tiling windows (still in the pool) are appended as new
//!    columns on the active workspace.

use std::collections::{HashMap, HashSet};
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

/// Build the strict-match triple key for a window identity.
///
/// The triple `(exe, class, title)` is the unique identifier used to
/// match loadout entries to live windows across daemon restarts.
/// Two windows with the same triple are treated as interchangeable.
#[must_use]
fn build_triple(exe: &str, class: &str, title: &str) -> (String, String, String) {
    (exe.to_string(), class.to_string(), title.to_string())
}

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
    /// as [`Self::dispatch_loadout_load`] with `force: false`, so the
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

        // 2. Staleness guard (silent skip, NOT an error)
        if !force && is_stale(&file.saved_at, self.config.loadout.max_age_secs) {
            log::info!(
                "loadout load: skipping stale loadout from {}",
                file.saved_at
            );
            return Ok(());
        }

        // 3. Build pool from live windows
        let mut pool: HashMap<(String, String, String), Vec<WindowId>> = HashMap::new();
        for win in self.registry.windows() {
            // Skip Ignored windows — they are not part of any loadout.
            if matches!(win.state, WindowState::Ignored(_)) {
                continue;
            }
            let hwnd_isize = win.hwnd.0 as isize;
            let wid = WindowId(hwnd_isize);
            let triple = build_triple(&win.exe, &win.class, &win.title);
            pool.entry(triple).or_default().push(wid);
        }

        // 4. Phase B: resolve all loadout slots (NO mutation)
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
                    let triple = build_triple(
                        &row_snap.window.exe,
                        &row_snap.window.class,
                        &row_snap.window.title,
                    );
                    let assigned =
                        pool.get_mut(&triple).and_then(|v| v.pop()).ok_or_else(|| {
                            "loadout references a window that is not currently open".to_string()
                        })?;
                    rows.push(assigned);
                }
                columns.push((col_snap.width_px, rows));
            }

            // Resolve floating entries
            let mut floating = Vec::new();
            for float_entry in &ws_snap.floating {
                let triple = build_triple(
                    &float_entry.window.exe,
                    &float_entry.window.class,
                    &float_entry.window.title,
                );
                let assigned = pool.get_mut(&triple).and_then(|v| v.pop()).ok_or_else(|| {
                    "loadout references a window that is not currently open".to_string()
                })?;
                let rect = rect_json_to_rect(&float_entry.rect);
                floating.push((assigned, rect));
            }

            // Resolve focus: find the already-assigned tiled window whose
            // original WindowRef matches the snapshot's focus ref. Do NOT
            // pop from the pool (focus is a tiled window already assigned
            // above). Walk columns by index to map the matching row ref to
            // its resolved WindowId.
            let focus = ws_snap.scrolling.focus.as_ref().and_then(|focus_ref| {
                let focus_triple = build_triple(&focus_ref.exe, &focus_ref.class, &focus_ref.title);
                for (col_idx, col_snap) in ws_snap.scrolling.columns.iter().enumerate() {
                    for (row_idx, row_snap) in col_snap.rows.iter().enumerate() {
                        let row_triple = build_triple(
                            &row_snap.window.exe,
                            &row_snap.window.class,
                            &row_snap.window.title,
                        );
                        if row_triple == focus_triple
                            && let Some(wid) = columns.get(col_idx).and_then(|c| c.1.get(row_idx))
                        {
                            return Some(*wid);
                        }
                    }
                }
                None
            });

            resolved_workspaces.push(ResolvedWorkspace {
                workspace_id: ws_snap.workspace_id,
                columns,
                focus,
                viewport_offset: ws_snap.scrolling.viewport_offset,
                floating,
            });
        }

        // 5. Phase C: identify assigned vs leftover windows
        let mut all_assigned: HashSet<WindowId> = HashSet::new();
        for rw in &resolved_workspaces {
            for (_, rows) in &rw.columns {
                for wid in rows {
                    all_assigned.insert(*wid);
                }
            }
            for (wid, _) in &rw.floating {
                all_assigned.insert(*wid);
            }
        }

        // Leftover tiling windows: registry windows in Tiling state (not
        // Ignored, not Floating) whose id is not in all_assigned.
        let mut leftovers: Vec<WindowId> = Vec::new();
        for win in self.registry.windows() {
            let is_floating = matches!(win.state, WindowState::Floating(_));
            let is_ignored = matches!(win.state, WindowState::Ignored(_));
            if is_ignored || is_floating || !matches!(win.state, WindowState::Tiling(_)) {
                continue;
            }
            let hwnd_isize = win.hwnd.0 as isize;
            let wid = WindowId(hwnd_isize);
            if !all_assigned.contains(&wid) {
                leftovers.push(wid);
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
            version: 1,
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

    /// Translate a [`WindowId`] to a [`WindowRef`] (exe, class, title) by
    /// looking up the window in the registry.
    ///
    /// Returns `None` if the window is not found in the registry and logs
    /// a warning.
    fn window_ref_for(&self, wid: WindowId) -> Option<WindowRef> {
        let hwnd = HWND(wid.0 as *mut _);
        let win = self.registry.get_window(hwnd)?;
        Some(WindowRef {
            exe: win.exe.clone(),
            class: win.class.clone(),
            title: win.title.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── build_triple: pure helper ────────────────────────────────────

    /// Positive: triple construction produces correct tuple.
    #[test]
    fn build_triple_correct() {
        let t = build_triple("code.exe", "Chrome_WidgetWin_1", "main.rs");
        assert_eq!(
            t,
            (
                "code.exe".into(),
                "Chrome_WidgetWin_1".into(),
                "main.rs".into()
            )
        );
    }

    /// Positive: distinct triples are distinct keys.
    #[test]
    fn distinct_triples_are_distinct() {
        let a = build_triple("a.exe", "cls", "title");
        let b = build_triple("b.exe", "cls", "title");
        assert_ne!(a, b);
    }

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

    // ── Greedy pool assignment logic ──────────────────────────────────

    /// Positive: every slot matches when the pool has exactly one per triple.
    #[test]
    fn greedy_assign_all_match() {
        let mut pool: HashMap<(String, String, String), Vec<WindowId>> = HashMap::new();
        let t1 = build_triple("a.exe", "cls", "win1");
        let t2 = build_triple("a.exe", "cls", "win2");
        pool.insert(t1.clone(), vec![WindowId(100)]);
        pool.insert(t2.clone(), vec![WindowId(200)]);

        // Simulate popping for t1 and t2
        let id1 = pool.get_mut(&t1).and_then(|v| v.pop());
        let id2 = pool.get_mut(&t2).and_then(|v| v.pop());
        assert_eq!(id1, Some(WindowId(100)));
        assert_eq!(id2, Some(WindowId(200)));
    }

    /// Negative: dry pool triggers abort (no partial).
    #[test]
    fn greedy_assign_dry_pool_aborts() {
        let mut pool: HashMap<(String, String, String), Vec<WindowId>> = HashMap::new();
        let t = build_triple("a.exe", "cls", "win1");
        pool.insert(t.clone(), vec![WindowId(100)]);

        // First pop succeeds
        let id1 = pool.get_mut(&t).and_then(|v| v.pop());
        assert_eq!(id1, Some(WindowId(100)));

        // Second pop (same triple, pool now empty) → abort
        let id2 = pool.get_mut(&t).and_then(|v| v.pop());
        assert_eq!(id2, None, "dry pool must return None (abort signal)");
    }

    /// Positive: two identical triples are handled correctly (one per pop).
    #[test]
    fn greedy_assign_identical_triples() {
        let mut pool: HashMap<(String, String, String), Vec<WindowId>> = HashMap::new();
        let t = build_triple("chrome.exe", "cls", "tab");
        pool.insert(t.clone(), vec![WindowId(10), WindowId(20)]);

        let id1 = pool.get_mut(&t).and_then(|v| v.pop());
        let id2 = pool.get_mut(&t).and_then(|v| v.pop());
        assert!(id1.is_some());
        assert!(id2.is_some());
        assert_ne!(id1, id2);

        // Third pop → dry
        let id3 = pool.get_mut(&t).and_then(|v| v.pop());
        assert_eq!(id3, None);
    }

    // ── is_stale delegation ──────────────────────────────────────────

    /// Positive: is_stale is callable from loadout module.
    #[test]
    fn is_stale_callable() {
        let old = "2020-01-01T00:00:00Z";
        assert!(is_stale(old, 60));
    }

    // ── Leftover detection (basis for append-as-columns) ──────────────

    /// After greedy assignment, any window still in the pool is a "leftover"
    /// — a live tiling window not referenced by any loadout slot. The daemon
    /// appends these as new columns on the active workspace.
    ///
    /// This test exercises the pure pool mechanics that
    /// `dispatch_loadout_load` relies on (FlowWM itself can't be constructed
    /// in unit tests): build a pool, pop one match per loadout slot, then
    /// assert the remaining pool entries are exactly the leftovers.
    // Positive: leftover tiling windows remain in pool after greedy assign.
    #[test]
    fn greedy_assign_leftovers_remain_in_pool() {
        let mut pool: HashMap<(String, String, String), Vec<WindowId>> = HashMap::new();
        // Loadout references one slot for this triple; the live desktop has
        // three windows sharing it (e.g. three chrome.exe tabs).
        let t = build_triple("chrome.exe", "Chrome_WidgetWin_1", "tab");
        let extra_a = build_triple("code.exe", "Chrome_WidgetWin_1", "main.rs");
        let extra_b = build_triple("slack.exe", "Slack_Window", "Slack");
        pool.insert(t.clone(), vec![WindowId(10), WindowId(20), WindowId(30)]);
        pool.insert(extra_a.clone(), vec![WindowId(40)]);
        pool.insert(extra_b.clone(), vec![WindowId(50)]);

        // Phase B: resolve the single loadout slot for `t`.
        let assigned = pool.get_mut(&t).and_then(|v| v.pop());
        assert_eq!(
            assigned,
            Some(WindowId(30)),
            "pop returns the last pushed id"
        );

        // Phase C: collect every window still in the pool — these are the
        // leftovers the daemon appends as columns. Sort by the inner id so the
        // assertion is order-independent (`WindowId` is a frozen contract that
        // does not derive `Ord`).
        let mut leftovers: Vec<WindowId> = pool
            .values()
            .flat_map(std::ops::Deref::deref)
            .copied()
            .collect();
        leftovers.sort_by_key(|w| w.0);
        assert_eq!(
            leftovers,
            vec![WindowId(10), WindowId(20), WindowId(40), WindowId(50)],
            "leftovers must be the two remaining chrome tabs plus code + slack"
        );
        // The assigned window must NOT appear in the leftovers.
        assert!(
            !leftovers.contains(&WindowId(30)),
            "assigned window must not be a leftover (no double-placement)"
        );
    }
}
