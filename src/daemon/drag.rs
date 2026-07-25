//! Tile-window drag-and-drop lifecycle.
//!
//! When a tiled window is dragged by its title bar, this module manages:
//! - Border following (the overlay tracks the window's real Win32 position).
//! - Drop-zone hit-testing (which [`DropZone`] the cursor is over).
//! - Preview reflow (other windows animate to show the prospective layout).
//! - Commit or snap-back on release.
//!
//! The three entry points ([`FlowWM::on_drag_start`],
//! [`FlowWM::on_drag_move`], [`FlowWM::on_drag_end`]) are called from the
//! daemon's event loop; the hook callback remains stateless — it only
//! signals via [`set_dragged_hwnd`](crate::registry::hooks::set_dragged_hwnd)
//! / [`clear_dragged_hwnd`](crate::registry::hooks::clear_dragged_hwnd)
//! from the main thread.
//!
//! (`docs/src/dev-guide/tile-drag.md`)

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::borders::{BorderState, style_for_state};
use crate::common::{Rect, WindowId};
use crate::layout::mutations::{MutationConfig, ensure_column_visible};
use crate::layout::preview::{DropZone, preview_gap_close, preview_insert, preview_move};
use crate::layout::types::{AppliedLayout, MonitorInfo};
use crate::registry::hooks::{clear_dragged_hwnd, remove_float_hwnd, set_dragged_hwnd};
use crate::registry::types::{FloatingState, TilingState, WindowState};
use crate::registry::win32 as registry_win32;

use super::borders::float_border_rect;
use super::types::FlowWM;

/// Which kind of window started the drag, recorded at `MoveSizeStart`.
///
/// Drives the small behavioral differences between the two sources: a tile
/// sources lives in the virtual layout (so moves use [`preview_move`] and a
/// center dwell fires the gap-closing preview), while a float source is NOT in
/// the layout (so inserts use [`preview_insert`] and the center region is a
/// no-op — there is no gap to close). (`docs/src/dev-guide/tile-drag.md`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DragSource {
    /// Drag began on a `Tiling(Active)` window.
    Tile,
    /// Drag began on a `Floating(Active)` window.
    Float,
}

/// State held while the user is dragging a window.
///
/// Entered on `MoveSizeStart` for a `Tiling::Active` or `Floating::Active`
/// window and dropped on `MoveSizeEnd`. The drag handler reads this to know
/// which window is being dragged, where it came from ([`DragSource`]), and
/// what drop zone it is currently over.
///
/// # Dwell timer
///
/// Zone activation is **dwell-based**: the cursor must rest inside a zone for
/// `config.drag.dwell_time_ms` before the zone "fires" (commits a preview or
/// scrolls). This prevents accidental activations while sweeping across zones.
/// The same dwell applies to the center (uncovered) region for tile sources,
/// firing the gap-closing preview.
///
/// # Animation lock
///
/// After a zone fires, detection is locked for the animation duration so the
/// reflow can play out without interruption.
pub(super) struct DragState {
    /// The layout-engine ID of the dragged window.
    pub(super) dragged_id: WindowId,
    /// The raw HWND value (for `GetWindowRect`, `DRAGGED_HWND` global).
    pub(super) dragged_hwnd: isize,
    /// Whether the drag started on a tile or a float.
    pub(super) source: DragSource,
    /// The drop zone currently under the cursor, or `None` if in center/uncovered.
    pub(super) current_zone: Option<DropZone>,
    /// When the cursor entered `current_zone` (for dwell timing).
    /// Reset to `None` after activation, re-set when the animation lock expires.
    pub(super) zone_entered_at: Option<Instant>,
    /// When the animation lock expires (`None` = not locked).
    pub(super) unlock_at: Option<Instant>,
    /// Whether the non-committing center gap-closing preview is currently
    /// showing. Set when a tile source dwells in the center; cleared (and the
    /// intact layout re-animated) as soon as the cursor leaves the center or a
    /// directional zone fires. Always `false` for float sources.
    pub(super) center_preview_active: bool,
}

// ---------------------------------------------------------------------------
// Zone computation + hit-testing
// ---------------------------------------------------------------------------

/// Computes drop zones for the current visible layout.
///
/// Each visible column (except the dragged window's own column) generates:
/// - A **Left** strip (`lr_ratio` of column width) → `DropZone::Column { col }`.
/// - A **Right** strip → `DropZone::Column { col + 1 }`.
///
/// Each window inside those columns additionally generates:
/// - An **Upper** strip (`ul_ratio` of window height, inside Left/Right margins)
///   → `DropZone::Row { col, row }`.
/// - A **Lower** strip → `DropZone::Row { col, row + 1 }`.
///
/// Two scroll zones at the monitor edges (`edge_scroll_width` px wide)
/// → `DropZone::ScrollLeft` / `ScrollRight`.
///
/// Overlapping rects are expected — [`find_zone_at_point`] resolves priority.
#[must_use]
fn compute_window_zones(
    layout: &AppliedLayout,
    monitor: &MonitorInfo,
    dragged_id: WindowId,
    lr_ratio: f32,
    ul_ratio: f32,
    edge_scroll_width: i32,
) -> Vec<(DropZone, Rect)> {
    let mut zones = Vec::new();

    let col_rects = visible_column_rects(layout, monitor);
    let dragged_col = layout
        .virtual_layout
        .find_window(dragged_id)
        .map(|(c, _)| c);

    for (col_idx, col_rect) in &col_rects {
        // No zones on the dragged window's own column.
        if Some(*col_idx) == dragged_col {
            continue;
        }

        let lr_width = ((col_rect.width as f32 * lr_ratio) as i32).max(1);

        // Left zone → insert new column at this index.
        zones.push((
            DropZone::Column { col: *col_idx },
            Rect {
                x: col_rect.x,
                y: col_rect.y,
                width: lr_width,
                height: col_rect.height,
            },
        ));

        // Right zone → insert new column after this one.
        zones.push((
            DropZone::Column { col: col_idx + 1 },
            Rect {
                x: col_rect.x + col_rect.width - lr_width,
                y: col_rect.y,
                width: lr_width,
                height: col_rect.height,
            },
        ));

        // Upper/Lower zones per window inside this column.
        for entry in &layout.actual_layout.entries {
            let Some((entry_col, entry_row)) = layout.virtual_layout.find_window(entry.window_id)
            else {
                continue;
            };
            if entry_col != *col_idx || entry.window_id == dragged_id {
                continue;
            }

            let ul_height = ((entry.rect.height as f32 * ul_ratio) as i32).max(1);
            let inner_x = col_rect.x + lr_width;
            let inner_width = (col_rect.width - 2 * lr_width).max(1);

            // Upper zone → insert new row at this window's position.
            zones.push((
                DropZone::Row {
                    col: *col_idx,
                    row: entry_row,
                },
                Rect {
                    x: inner_x,
                    y: entry.rect.y,
                    width: inner_width,
                    height: ul_height,
                },
            ));

            // Lower zone → insert new row below this window.
            zones.push((
                DropZone::Row {
                    col: *col_idx,
                    row: entry_row + 1,
                },
                Rect {
                    x: inner_x,
                    y: entry.rect.y + entry.rect.height - ul_height,
                    width: inner_width,
                    height: ul_height,
                },
            ));
        }
    }

    // Edge scroll zones at monitor work-area boundaries.
    let mon = &monitor.work_area;
    zones.push((
        DropZone::ScrollLeft,
        Rect {
            x: mon.x,
            y: mon.y,
            width: edge_scroll_width,
            height: mon.height,
        },
    ));
    zones.push((
        DropZone::ScrollRight,
        Rect {
            x: mon.x + mon.width - edge_scroll_width,
            y: mon.y,
            width: edge_scroll_width,
            height: mon.height,
        },
    ));

    zones
}

/// Finds the highest-priority drop zone containing `(x, y)`.
///
/// Priority: Column zones (Left/Right) → Row zones (Upper/Lower) → Scroll
/// zones. Returns `None` if the point is outside all zone rects (center /
/// uncovered region).
#[must_use]
fn find_zone_at_point(zones: &[(DropZone, Rect)], x: i32, y: i32) -> Option<DropZone> {
    // Pass 1: Column zones.
    for (zone, rect) in zones {
        if matches!(zone, DropZone::Column { .. }) && rect_contains(rect, x, y) {
            return Some(*zone);
        }
    }
    // Pass 2: Row zones.
    for (zone, rect) in zones {
        if matches!(zone, DropZone::Row { .. }) && rect_contains(rect, x, y) {
            return Some(*zone);
        }
    }
    // Pass 3: Scroll zones.
    for (zone, rect) in zones {
        if matches!(zone, DropZone::ScrollLeft | DropZone::ScrollRight) && rect_contains(rect, x, y)
        {
            return Some(*zone);
        }
    }
    None
}

/// Point-in-rect test (half-open: inclusive origin, exclusive far edge).
#[must_use]
fn rect_contains(rect: &Rect, x: i32, y: i32) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

/// Build column bounding rects for hit-testing, keyed by **virtual-layout
/// column index** and filtered to **visible columns only**.
///
/// Each actual-layout entry is mapped back to its virtual column via
/// [`VirtualLayout::find_window`]. Entries are grouped by virtual column
/// index (not x-coordinate — parked columns can share the same x). Columns
/// whose screen rect lies entirely outside `monitor.work_area` are excluded:
/// the user can only drop on columns they can see.
///
/// Returns `(virtual_col_index, Rect)` pairs in left-to-right order.
fn visible_column_rects(layout: &AppliedLayout, monitor: &MonitorInfo) -> Vec<(usize, Rect)> {
    let mon_left = monitor.work_area.x;
    let mon_right = monitor.work_area.x + monitor.work_area.width;

    // Group entries by virtual column index.
    use std::collections::BTreeMap;
    let mut col_map: BTreeMap<usize, Vec<&crate::layout::types::ActualEntry>> = BTreeMap::new();
    for entry in &layout.actual_layout.entries {
        if let Some((col_idx, _)) = layout.virtual_layout.find_window(entry.window_id) {
            col_map.entry(col_idx).or_default().push(entry);
        }
    }

    let mut result = Vec::new();
    for (col_idx, entries) in col_map.into_iter() {
        if entries.is_empty() {
            continue;
        }

        let x = entries[0].rect.x;
        let width = entries[0].rect.width;

        // Skip parked columns — user can only drop on visible columns.
        if x + width <= mon_left || x >= mon_right {
            continue;
        }

        let min_y = entries.iter().map(|e| e.rect.y).min().unwrap_or(0);
        let max_y = entries
            .iter()
            .map(|e| e.rect.y + e.rect.height)
            .max()
            .unwrap_or(0);
        result.push((
            col_idx,
            Rect {
                x,
                y: min_y,
                width,
                height: max_y - min_y,
            },
        ));
    }
    result
}

// ---------------------------------------------------------------------------
// Handler methods on FlowWM
//
// Called from `process_hook_events` in `run.rs` on MoveSizeStart/MoveSizeEnd
// and LocationChange events during a tile drag.

impl FlowWM {
    /// Begin a window drag (tile or float source).
    ///
    /// Called on `MoveSizeStart` for any tracked window. Enters the drag state
    /// machine when the window is `Tiling(Active)` or `Floating(Active)`, and
    /// signals the hook thread to forward the window's `LOCATIONCHANGE` events
    /// to [`on_drag_move`](Self::on_drag_move) for the duration of the drag
    /// (the float sync path is bypassed while `drag_state` is `Some`).
    ///
    /// No-op if the window is not found or is in neither active state.
    pub(super) fn on_drag_start(&mut self, hwnd: isize) {
        let hwnd_handle = HWND(hwnd as *mut _);
        let Some(window) = self.registry.get_window(hwnd_handle) else {
            return;
        };
        let source = match window.state {
            WindowState::Tiling(TilingState::Active { .. }) => DragSource::Tile,
            WindowState::Floating(FloatingState::Active { .. }) => DragSource::Float,
            _ => return,
        };

        self.drag_state = Some(DragState {
            dragged_id: WindowId(hwnd),
            dragged_hwnd: hwnd,
            source,
            current_zone: None,
            zone_entered_at: None,
            unlock_at: None,
            center_preview_active: false,
        });

        set_dragged_hwnd(hwnd);

        // Recolor the border to focused to give visual feedback.
        let focused_style = style_for_state(&self.config.borders, BorderState::Focused);
        if let Some(win) = self.registry.get_window_mut(hwnd_handle)
            && let Some(border) = win.border.as_ref()
        {
            border.set_style(focused_style);
        }

        log::debug!("drag start: hwnd={hwnd} source={source:?}");
    }

    /// Update the drag position — border follows, zones re-evaluated with dwell.
    ///
    /// Called on each `LOCATIONCHANGE` for the dragged window while
    /// `self.drag_state` is `Some`.
    ///
    /// # Flow
    ///
    /// 1. Border follows the window (direct `set_geometry`, not animator).
    /// 2. If animation-locked (previous directional activation still
    ///    animating), skip.
    /// 3. Compute zones for current layout, find zone at cursor.
    /// 4. If zone changed → cancel any live center preview, store the new
    ///    zone, start its dwell timer, return.
    /// 5. If same zone + dwell expired → activate:
    ///    - **Center** (tile source only) → fire the non-committing
    ///      gap-closing preview (float sources do nothing in the center).
    ///    - **Scroll** → scroll the viewport, animate.
    ///    - **Row / Column** → move (if the window is in the layout) or insert
    ///      (float source not yet promoted), commit, animate. A float source
    ///      is finalized to a tile on its first directional activation.
    pub(super) fn on_drag_move(&mut self, hwnd: isize) {
        let now = Instant::now();

        let Some(drag) = self.drag_state.as_ref() else {
            return;
        };
        let dragged_id = drag.dragged_id;
        let source = drag.source;

        let hwnd_handle = HWND(hwnd as *mut _);

        // 1. Border follows the window.
        let window_rect = match registry_win32::get_window_rect(hwnd_handle) {
            Ok(r) => r,
            Err(e) => {
                log::debug!("drag move: GetWindowRect failed for {hwnd}: {e}");
                return;
            }
        };
        {
            let Some(window) = self.registry.get_window(hwnd_handle) else {
                return;
            };
            let visible_rect = window.invisible_bounds.window_to_visible(window_rect);
            let border_rect = float_border_rect(
                visible_rect,
                self.config.borders.thickness,
                self.config.borders.overlap,
            );
            if let Some(border) = window.border.as_ref() {
                border.set_geometry(border_rect);
            }
        }

        // 2. Check animation lock.
        {
            let Some(drag) = self.drag_state.as_mut() else {
                return;
            };
            match drag.unlock_at {
                Some(t) if now < t => return, // Still locked.
                Some(_) => {
                    // Just unlocked — restart dwell timer for current zone.
                    drag.unlock_at = None;
                    drag.zone_entered_at = Some(now);
                }
                None => {} // Not locked.
            }
        }

        // 3. Cursor position + zone detection.
        let (cx, cy) = match registry_win32::get_cursor_pos() {
            Ok(pos) => pos,
            Err(e) => {
                log::debug!("drag move: GetCursorPos failed: {e}");
                return;
            }
        };

        let drag_cfg = &self.config.drag;
        let dwell = Duration::from_millis(drag_cfg.dwell_time_ms);
        let anim_dur = Duration::from_millis(self.config.animation.duration_ms as u64);

        // Snapshot layout data for zone computation.
        let (applied, config, monitor) = {
            let space = self.active_scrolling();
            (
                AppliedLayout {
                    virtual_layout: space.virtual_layout().clone(),
                    actual_layout: space.actual_layout().clone(),
                },
                *space.config(),
                *space.monitor(),
            )
        };

        let zones = compute_window_zones(
            &applied,
            &monitor,
            dragged_id,
            drag_cfg.left_right_zone_ratio,
            drag_cfg.upper_lower_zone_ratio,
            drag_cfg.edge_scroll_width,
        );
        let new_zone = find_zone_at_point(&zones, cx, cy);

        // 4. Zone change → cancel any center preview, start dwell timer.
        let current_zone = self.drag_state.as_ref().and_then(|d| d.current_zone);
        if new_zone != current_zone {
            // Leaving the center while the gap-closing preview is showing →
            // snap the remaining tiles back to the intact (committed) layout.
            // The preview is non-committing, so the real layout is unchanged.
            if current_zone.is_none() {
                self.cancel_center_preview();
            }
            if let Some(drag) = self.drag_state.as_mut() {
                drag.current_zone = new_zone;
                drag.zone_entered_at = Some(now);
            }
            return;
        }

        // 5. Same zone — check dwell timer.
        let dwell_expired = self
            .drag_state
            .as_ref()
            .and_then(|d| d.zone_entered_at)
            .map(|t| now.duration_since(t) >= dwell)
            .unwrap_or(false);

        let Some(zone) = new_zone else {
            // Center (uncovered) region. Tile sources fire the non-committing
            // gap-closing preview on dwell so the user sees where a release
            // would promote the window to float. Float sources have no gap to
            // close, so the center is inert for them — the border just follows
            // the mouse.
            if dwell_expired && source == DragSource::Tile {
                self.fire_center_preview(dragged_id, &applied, &config, &monitor);
                // Consume the dwell so the preview fires once per center entry.
                // Do NOT arm the animation lock — the preview must remain
                // interruptible the instant the cursor re-enters a zone.
                if let Some(drag) = self.drag_state.as_mut() {
                    drag.zone_entered_at = None;
                }
            }
            return;
        };

        if !dwell_expired {
            return;
        }

        // 6. Activate the zone.
        let mut activated = false;
        match zone {
            DropZone::ScrollLeft => {
                if let Some(scrolled) = self.active_scrolling_mut().scroll_left() {
                    self.animate_layout(&scrolled);
                    activated = true;
                }
            }
            DropZone::ScrollRight => {
                if let Some(scrolled) = self.active_scrolling_mut().scroll_right() {
                    self.animate_layout(&scrolled);
                    activated = true;
                }
            }
            DropZone::Row { .. } | DropZone::Column { .. } => {
                let vl = self.active_scrolling().virtual_layout().clone();
                let in_layout = vl.find_window(dragged_id).is_some();
                // Tile source (or a float already promoted by an earlier
                // dwell-fire) → move within the layout. Float source not yet
                // promoted → insert-only.
                let preview = if in_layout {
                    preview_move(&vl, dragged_id, zone, &config, &monitor)
                } else {
                    preview_insert(&vl, dragged_id, zone, &config, &monitor)
                };
                if let Some(preview) = preview {
                    // Commit internally — subsequent zone detection sees the
                    // new layout and the dragged window's new column exclusion.
                    let applied = self
                        .active_scrolling_mut()
                        .commit_layout(preview.virtual_layout);
                    // A float source just inserted → finalize float→tile: drop
                    // it from the floating space + float-tracking set. The
                    // registry tiling state is assigned by animate_layout below.
                    if !in_layout {
                        self.finalize_float_to_tile(dragged_id);
                    }
                    self.animate_layout(&applied);
                    activated = true;
                }
            }
        }

        // 7. Consume dwell timer. Arm animation lock only when something
        //    actually committed, so no-op activations don't stall the drag.
        if let Some(drag) = self.drag_state.as_mut() {
            drag.zone_entered_at = None;
            if activated {
                drag.unlock_at = Some(now + anim_dur);
                // Defensive: a directional commit fully overrides any lingering
                // center preview state.
                drag.center_preview_active = false;
            }
        }

        if activated {
            log::debug!("drag activate: hwnd={hwnd} zone={zone:?}");
        }
    }

    /// Fire the center gap-closing preview for a tile-source drag if it is not
    /// already showing.
    ///
    /// Computes the layout with the dragged window removed and animates the
    /// remaining tiles to their gap-closed positions **without committing** —
    /// the preview is fully reversed by [`cancel_center_preview`](Self::cancel_center_preview)
    /// when the cursor leaves the center. The dragged window's border keeps
    /// following the mouse via the animator exclusion filter.
    fn fire_center_preview(
        &mut self,
        dragged_id: WindowId,
        applied: &AppliedLayout,
        config: &MutationConfig,
        monitor: &MonitorInfo,
    ) {
        let Some(drag) = self.drag_state.as_ref() else {
            return;
        };
        if drag.center_preview_active {
            return;
        }
        if let Some(gap_closed) =
            preview_gap_close(&applied.virtual_layout, dragged_id, config, monitor)
        {
            if let Some(drag) = self.drag_state.as_mut() {
                drag.center_preview_active = true;
            }
            self.animate_gap_close_preview(&gap_closed);
        }
    }

    /// Reverse the center gap-closing preview (if active) by re-animating the
    /// intact committed layout.
    ///
    /// No-op when no preview is showing. Safe to call unconditionally on every
    /// zone change away from the center: [`animate_layout`](Self::animate_layout)
    /// re-syncs the registry (a no-op here, since the preview never desynced
    /// it) and animates the remaining tiles back to their intact positions.
    fn cancel_center_preview(&mut self) {
        let Some(drag) = self.drag_state.as_ref() else {
            return;
        };
        if !drag.center_preview_active {
            return;
        }
        let intact = {
            let space = self.active_scrolling();
            AppliedLayout {
                virtual_layout: space.virtual_layout().clone(),
                actual_layout: space.actual_layout().clone(),
            }
        };
        if let Some(drag) = self.drag_state.as_mut() {
            drag.center_preview_active = false;
        }
        self.animate_layout(&intact);
    }

    /// Finalize a float→tile promotion triggered by a drop-zone dwell-fire.
    ///
    /// Removes the window from the active workspace's `FloatingSpace` and the
    /// float-tracking set. The registry state is flipped to `Tiling(Active)`
    /// by the surrounding [`animate_layout`](Self::animate_layout) call's
    /// `update_tiling_slots_from_layout`, which assigns col/row from the
    /// just-committed virtual layout.
    fn finalize_float_to_tile(&mut self, dragged_id: WindowId) {
        self.active_workspace_mut().floating.remove(dragged_id);
        remove_float_hwnd(dragged_id.0);
    }

    /// End the window drag (tile or float source).
    ///
    /// Called on `MoveSizeEnd`. The outcome depends on the drag source and the
    /// cursor position at release:
    ///
    /// **Tile source**
    ///
    /// - **On a directional zone** (`current_zone` is `Some`) → snap to its
    ///   committed tile. The layout was already committed during dwell
    ///   activations (or unchanged if no dwell fired). Animates ALL windows so
    ///   the dragged window visibly snaps from its physical position to its
    ///   tiled slot.
    /// - **Center / uncovered** (`current_zone` is `None`) → promote to float.
    ///   Removes the window from the tiling layout, registers it in
    ///   [`FloatingSpace`](crate::workspace::FloatingSpace) at the drop
    ///   position, and animates remaining tiled windows to fill the gap.
    ///
    /// **Float source**
    ///
    /// - **Promoted by a directional dwell-fire** (already in the layout) →
    ///   snap to its committed tile (same as a tile-source zone drop).
    /// - **Center / uncovered** (never promoted) → persist the float at its
    ///   dropped rect. The OS moved the window during the drag and the float
    ///   sync path was bypassed, so [`store_float_rect`](Self::store_float_rect)
    ///   records the final rect here.
    pub(super) fn on_drag_end(&mut self, _hwnd: isize) {
        let Some(drag) = self.drag_state.take() else {
            return;
        };

        clear_dragged_hwnd();

        // If the window was destroyed mid-drag, it's already been removed from
        // the layout and registry by `on_window_destroyed`. The current layout
        // already reflects its absence — nothing to snap or promote.
        if self
            .registry
            .get_window(HWND(drag.dragged_hwnd as *mut _))
            .is_none()
        {
            log::debug!(
                "drag end: window {} already destroyed, skipping snap/float",
                drag.dragged_hwnd
            );
            return;
        }

        match drag.source {
            DragSource::Tile => {
                if drag.current_zone.is_some() {
                    self.snap_dragged_to_tile(&drag);
                } else {
                    self.promote_dragged_to_float(&drag);
                }
            }
            DragSource::Float => {
                // A directional dwell-fire already committed the float into the
                // tiling layout (and finalize_float_to_tile removed it from the
                // floating space). Otherwise the window stayed a float — its
                // LOCATIONCHANGE sync was bypassed during the drag, so persist
                // the dropped rect here.
                let in_layout = self
                    .active_scrolling()
                    .virtual_layout()
                    .find_window(drag.dragged_id)
                    .is_some();
                if in_layout {
                    self.snap_dragged_to_tile(&drag);
                } else {
                    self.store_float_rect(drag.dragged_hwnd);
                }
            }
        }

        // Re-resolve border style + position for the new window state.
        self.refresh_border_for(drag.dragged_hwnd);

        log::debug!("drag end: hwnd={}", drag.dragged_hwnd);
    }

    /// Snap the dragged window back to its committed tiled slot.
    ///
    /// The layout may have been modified by dwell activations during the drag.
    /// Ensures the dragged window's column is on-screen, then animates ALL
    /// windows so the dragged window visibly snaps from its physical (mouse)
    /// position to its tile.
    fn snap_dragged_to_tile(&mut self, drag: &DragState) {
        let (vl, config) = {
            let space = self.active_scrolling();
            (space.virtual_layout().clone(), *space.config())
        };
        let vl = match vl.find_window(drag.dragged_id) {
            Some((col, _)) => ensure_column_visible(&vl, col, &config),
            None => vl,
        };
        let applied = self.active_scrolling_mut().commit_layout(vl);
        self.animate_layout(&applied);
    }

    /// Promote the dragged window to floating at its drop position.
    ///
    /// Reads the window's current screen rect (where the user dropped it),
    /// removes it from the tiling layout, and registers it as a float. The
    /// remaining tiled windows animate to fill the gap. The float window
    /// itself is already at its position (the OS moved it during the drag) —
    /// no animation needed for it.
    fn promote_dragged_to_float(&mut self, drag: &DragState) {
        let hwnd_handle = HWND(drag.dragged_hwnd as *mut _);

        // Read the drop position as a visible rect.
        let float_rect = match registry_win32::get_window_rect(hwnd_handle) {
            Ok(r) => {
                let ib = self
                    .registry
                    .get_window(hwnd_handle)
                    .map(|w| w.invisible_bounds)
                    .unwrap_or_default();
                ib.window_to_visible(r)
            }
            Err(e) => {
                log::warn!("drag-to-float: GetWindowRect failed: {e}, using centered fallback");
                self.centered_float_rect(drag.dragged_id)
            }
        };

        // Remove from tiling layout (handles focus fallback + ensure_column_visible).
        let source_applied = self.active_scrolling_mut().remove_window(drag.dragged_id);

        // Sync registry tiling state for remaining windows.
        self.registry
            .update_tiling_slots_from_layout(&source_applied.virtual_layout);
        self.registry
            .update_tiled_rects(&source_applied.actual_layout);

        // Register as float at the drop position. register_float adds to
        // FloatingSpace, sets registry state to Floating(Active), and calls
        // add_float_hwnd so future LOCATIONCHANGE events route through the
        // float-tracking path.
        let _float_actual = self.register_float(drag.dragged_id, float_rect);

        // Animate remaining tiled windows to fill the gap. The dragged window
        // was removed from the layout so animate_layout won't touch it — it
        // stays at its physical position (now a float).
        self.animate_layout(&source_applied);

        log::debug!(
            "drag-to-float: hwnd={} rect={float_rect:?}",
            drag.dragged_hwnd
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::types::{ActualEntry, ActualLayout, Column, Row, VirtualLayout};

    /// Build an `AppliedLayout` with two columns and known rects.
    ///
    /// Virtual: col 0 = [W1], col 1 = [W2, W3]
    /// Actual:  W1=(0,0,500,1000), W2=(500,0,500,500), W3=(500,500,500,500)
    fn test_applied_layout() -> AppliedLayout {
        let vl = VirtualLayout::with_columns(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_rows(
                    500,
                    vec![Row::new(WindowId(2), 500), Row::new(WindowId(3), 500)],
                ),
            ],
            0,
        );
        AppliedLayout {
            virtual_layout: vl,
            actual_layout: ActualLayout {
                entries: vec![
                    ActualEntry {
                        window_id: WindowId(1),
                        rect: Rect {
                            x: 0,
                            y: 0,
                            width: 500,
                            height: 1000,
                        },
                    },
                    ActualEntry {
                        window_id: WindowId(2),
                        rect: Rect {
                            x: 500,
                            y: 0,
                            width: 500,
                            height: 500,
                        },
                    },
                    ActualEntry {
                        window_id: WindowId(3),
                        rect: Rect {
                            x: 500,
                            y: 500,
                            width: 500,
                            height: 500,
                        },
                    },
                ],
            },
        }
    }

    fn test_mon() -> MonitorInfo {
        MonitorInfo {
            work_area: Rect {
                x: 0,
                y: 0,
                width: 1000,
                height: 1000,
            },
        }
    }

    // ── compute_window_zones ───────────────────────────────────────────────

    #[test]
    fn zones_exclude_dragged_column() {
        let layout = test_applied_layout();
        let zones = compute_window_zones(&layout, &test_mon(), WindowId(1), 0.25, 0.25, 30);
        // Dragging W1 (col 0) → no zone should reference col 0.
        for (zone, _) in &zones {
            match zone {
                DropZone::Column { col: c } | DropZone::Row { col: c, .. } => {
                    assert_ne!(*c, 0, "col 0 (dragged) should not appear in zones");
                }
                _ => {}
            }
        }
    }

    #[test]
    fn column_left_zone_rect() {
        let layout = test_applied_layout();
        let zones = compute_window_zones(&layout, &test_mon(), WindowId(1), 0.25, 0.25, 30);
        // Left zone of col 1: x=500, width=125 (25% of 500), full height.
        let (_, r) = zones
            .iter()
            .find(|(z, _)| *z == DropZone::Column { col: 1 })
            .expect("Column{col:1} zone should exist");
        assert_eq!(r.x, 500);
        assert_eq!(r.width, 125);
        assert_eq!(r.y, 0);
        assert_eq!(r.height, 1000);
    }

    #[test]
    fn column_right_zone_rect() {
        let layout = test_applied_layout();
        let zones = compute_window_zones(&layout, &test_mon(), WindowId(1), 0.25, 0.25, 30);
        let (_, r) = zones
            .iter()
            .find(|(z, _)| *z == DropZone::Column { col: 2 })
            .expect("Column{col:2} zone should exist");
        assert_eq!(r.x, 875); // 500 + 500 - 125
        assert_eq!(r.width, 125);
    }

    #[test]
    fn row_upper_zone_rect() {
        let layout = test_applied_layout();
        let zones = compute_window_zones(&layout, &test_mon(), WindowId(1), 0.25, 0.25, 30);
        // Upper zone of W2 (col 1, row 0): inside lr margins, top 25%.
        let (_, r) = zones
            .iter()
            .find(|(z, _)| *z == DropZone::Row { col: 1, row: 0 })
            .expect("Row{col:1,row:0} zone should exist");
        assert_eq!(r.x, 625); // 500 + 125
        assert_eq!(r.width, 250); // 500 - 2*125
        assert_eq!(r.y, 0);
        assert_eq!(r.height, 125); // 25% of 500
    }

    #[test]
    fn row_lower_zone_rect() {
        let layout = test_applied_layout();
        let zones = compute_window_zones(&layout, &test_mon(), WindowId(1), 0.25, 0.25, 30);
        // Lower zone of W2: bottom 25%.
        let (_, r) = zones
            .iter()
            .find(|(z, _)| *z == DropZone::Row { col: 1, row: 1 })
            .expect("Row{col:1,row:1} zone should exist");
        assert_eq!(r.x, 625);
        assert_eq!(r.y, 375); // 500 - 125
        assert_eq!(r.height, 125);
    }

    #[test]
    fn scroll_zones_at_monitor_edges() {
        let layout = test_applied_layout();
        let zones = compute_window_zones(&layout, &test_mon(), WindowId(1), 0.25, 0.25, 30);
        let (_, sl) = zones
            .iter()
            .find(|(z, _)| matches!(z, DropZone::ScrollLeft))
            .expect("ScrollLeft zone should exist");
        assert_eq!(sl.x, 0);
        assert_eq!(sl.width, 30);
        let (_, sr) = zones
            .iter()
            .find(|(z, _)| matches!(z, DropZone::ScrollRight))
            .expect("ScrollRight zone should exist");
        assert_eq!(sr.x, 970); // 1000 - 30
        assert_eq!(sr.width, 30);
    }

    #[test]
    fn empty_layout_only_scroll_zones() {
        let layout = AppliedLayout {
            virtual_layout: VirtualLayout::new(),
            actual_layout: ActualLayout::new(),
        };
        let zones = compute_window_zones(&layout, &test_mon(), WindowId(1), 0.25, 0.25, 30);
        assert_eq!(zones.len(), 2);
        assert!(
            zones
                .iter()
                .all(|(z, _)| matches!(z, DropZone::ScrollLeft | DropZone::ScrollRight))
        );
    }

    #[test]
    fn dragged_in_col1_still_has_col0_zones() {
        let layout = test_applied_layout();
        // Drag W2 (col 1) → col 1 excluded, col 0 gets zones.
        let zones = compute_window_zones(&layout, &test_mon(), WindowId(2), 0.25, 0.25, 30);
        assert!(zones.iter().any(|(z, _)| *z == DropZone::Column { col: 0 }));
        assert!(zones.iter().any(|(z, _)| *z == DropZone::Column { col: 1 }));
        assert!(
            zones
                .iter()
                .any(|(z, _)| matches!(z, DropZone::Row { col: 0, .. }))
        );
        assert!(
            !zones
                .iter()
                .any(|(z, _)| matches!(z, DropZone::Row { col: 1, .. }))
        );
    }

    // ── visible_column_rects ───────────────────────────────────────────────

    /// Build an AppliedLayout with 2 visible + 1 parked-right column.
    fn layout_with_parked() -> AppliedLayout {
        let vl = VirtualLayout::with_columns(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
                Column::with_row(500, Row::new(WindowId(3), 1000)),
            ],
            0,
        );
        AppliedLayout {
            virtual_layout: vl,
            actual_layout: ActualLayout {
                entries: vec![
                    ActualEntry {
                        window_id: WindowId(1),
                        rect: Rect {
                            x: 0,
                            y: 0,
                            width: 500,
                            height: 1000,
                        },
                    },
                    ActualEntry {
                        window_id: WindowId(2),
                        rect: Rect {
                            x: 500,
                            y: 0,
                            width: 500,
                            height: 1000,
                        },
                    },
                    // Parked right: beyond monitor (width=1000).
                    ActualEntry {
                        window_id: WindowId(3),
                        rect: Rect {
                            x: 1000,
                            y: 0,
                            width: 500,
                            height: 1000,
                        },
                    },
                ],
            },
        }
    }

    #[test]
    fn visible_column_rects_filters_parked_right() {
        let layout = layout_with_parked();
        let cols = visible_column_rects(&layout, &test_mon());
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].0, 0); // virtual col 0
        assert_eq!(cols[1].0, 1); // virtual col 1
        // Col 2 (x=1000) is parked — filtered out.
    }

    #[test]
    fn visible_column_rects_filters_parked_left() {
        // Window at x=-500 (fully off left edge).
        let vl = VirtualLayout::with_columns(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
            ],
            1,
        );
        let applied = AppliedLayout {
            virtual_layout: vl,
            actual_layout: ActualLayout {
                entries: vec![
                    ActualEntry {
                        window_id: WindowId(1),
                        rect: Rect {
                            x: -500,
                            y: 0,
                            width: 500,
                            height: 1000,
                        },
                    },
                    ActualEntry {
                        window_id: WindowId(2),
                        rect: Rect {
                            x: 0,
                            y: 0,
                            width: 500,
                            height: 1000,
                        },
                    },
                ],
            },
        };
        let cols = visible_column_rects(&applied, &test_mon());
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].0, 1); // Only virtual col 1 (visible).
    }

    #[test]
    fn visible_column_rects_multi_row_y_span() {
        // Col 1 has [W2 (y=0..500), W3 (y=500..1000)].
        let layout = test_applied_layout();
        let cols = visible_column_rects(&layout, &test_mon());
        let col1 = cols.iter().find(|(c, _)| *c == 1).expect("col 1 exists");
        assert_eq!(col1.1.y, 0);
        assert_eq!(col1.1.height, 1000); // Full y-span of both rows.
    }

    #[test]
    fn visible_column_rects_preserves_virtual_index_order() {
        let layout = test_applied_layout();
        let cols = visible_column_rects(&layout, &test_mon());
        // BTreeMap guarantees ascending order regardless of entry order.
        assert!(cols.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn visible_column_rects_includes_partial_overlap() {
        // Column straddling right edge: x=800, width=500 (extends to 1300,
        // monitor right=1000). Should be included — partially visible.
        let vl = VirtualLayout::with_columns(
            vec![Column::with_row(500, Row::new(WindowId(1), 1000))],
            0,
        );
        let applied = AppliedLayout {
            virtual_layout: vl,
            actual_layout: ActualLayout {
                entries: vec![ActualEntry {
                    window_id: WindowId(1),
                    rect: Rect {
                        x: 800,
                        y: 0,
                        width: 500,
                        height: 1000,
                    },
                }],
            },
        };
        let cols = visible_column_rects(&applied, &test_mon());
        assert_eq!(cols.len(), 1); // Partially visible — included.
    }

    // ── compute_window_zones: multi-row ────────────────────────────────────

    #[test]
    fn zones_for_bottom_window_in_multi_row_column() {
        let layout = test_applied_layout();
        // Dragging W1 (col 0). Col 1 has [W2 (row 0), W3 (row 1)].
        let zones = compute_window_zones(&layout, &test_mon(), WindowId(1), 0.25, 0.25, 30);
        // W3's Upper zone → Row{col:1, row:1} (insert above W3).
        assert!(
            zones
                .iter()
                .any(|(z, _)| *z == DropZone::Row { col: 1, row: 1 }),
            "W3 upper zone should produce Row{{col:1, row:1}}"
        );
        // W3's Lower zone → Row{col:1, row:2} (insert below W3).
        assert!(
            zones
                .iter()
                .any(|(z, _)| *z == DropZone::Row { col: 1, row: 2 }),
            "W3 lower zone should produce Row{{col:1, row:2}}"
        );
    }

    // ── find_zone_at_point ─────────────────────────────────────────────────

    #[test]
    fn find_column_zone() {
        let zones = compute_window_zones(
            &test_applied_layout(),
            &test_mon(),
            WindowId(1),
            0.25,
            0.25,
            30,
        );
        // Inside col 1's Left strip.
        assert_eq!(
            find_zone_at_point(&zones, 510, 10),
            Some(DropZone::Column { col: 1 })
        );
    }

    #[test]
    fn find_row_upper() {
        let zones = compute_window_zones(
            &test_applied_layout(),
            &test_mon(),
            WindowId(1),
            0.25,
            0.25,
            30,
        );
        // Inside W2's Upper zone.
        assert_eq!(
            find_zone_at_point(&zones, 700, 10),
            Some(DropZone::Row { col: 1, row: 0 })
        );
    }

    #[test]
    fn find_row_lower() {
        let zones = compute_window_zones(
            &test_applied_layout(),
            &test_mon(),
            WindowId(1),
            0.25,
            0.25,
            30,
        );
        // Inside W2's Lower zone.
        assert_eq!(
            find_zone_at_point(&zones, 700, 480),
            Some(DropZone::Row { col: 1, row: 1 })
        );
    }

    #[test]
    fn find_scroll_left() {
        // Col 0 excluded (dragged W1), so no column zone at the left edge.
        let zones = compute_window_zones(
            &test_applied_layout(),
            &test_mon(),
            WindowId(1),
            0.25,
            0.25,
            30,
        );
        assert_eq!(
            find_zone_at_point(&zones, 10, 500),
            Some(DropZone::ScrollLeft)
        );
    }

    #[test]
    fn find_scroll_right_on_empty_layout() {
        // No visible columns → only scroll zones exist.
        let layout = AppliedLayout {
            virtual_layout: VirtualLayout::new(),
            actual_layout: ActualLayout::new(),
        };
        let zones = compute_window_zones(&layout, &test_mon(), WindowId(1), 0.25, 0.25, 30);
        assert_eq!(
            find_zone_at_point(&zones, 985, 500),
            Some(DropZone::ScrollRight)
        );
    }

    #[test]
    fn column_priority_over_scroll_at_edge() {
        // Col 1's Right zone (x:875, w:125) overlaps ScrollRight (x:970, w:30).
        // Column priority wins at (985, 500).
        let zones = compute_window_zones(
            &test_applied_layout(),
            &test_mon(),
            WindowId(1),
            0.25,
            0.25,
            30,
        );
        assert_eq!(
            find_zone_at_point(&zones, 985, 500),
            Some(DropZone::Column { col: 2 })
        );
    }

    #[test]
    fn find_center_returns_none() {
        let zones = compute_window_zones(
            &test_applied_layout(),
            &test_mon(),
            WindowId(1),
            0.25,
            0.25,
            30,
        );
        // Center of W2 — between all zone strips.
        assert_eq!(find_zone_at_point(&zones, 750, 250), None);
    }

    #[test]
    fn column_priority_over_scroll() {
        // Drag W2 (col 1) so col 0 is not excluded.
        // Col 0 Left zone (x:0, w:125) overlaps ScrollLeft (x:0, w:30).
        // Point (10, 500) is in both → Column wins.
        let zones = compute_window_zones(
            &test_applied_layout(),
            &test_mon(),
            WindowId(2),
            0.25,
            0.25,
            30,
        );
        assert_eq!(
            find_zone_at_point(&zones, 10, 500),
            Some(DropZone::Column { col: 0 })
        );
    }

    #[test]
    fn find_zone_empty_list() {
        assert_eq!(find_zone_at_point(&[], 500, 500), None);
    }

    #[test]
    fn rect_contains_half_open() {
        let r = Rect {
            x: 100,
            y: 200,
            width: 50,
            height: 60,
        };
        // Origin inclusive.
        assert!(rect_contains(&r, 100, 200));
        // Far edge exclusive.
        assert!(!rect_contains(&r, 150, 200)); // x + width
        assert!(!rect_contains(&r, 100, 260)); // y + height
        // Just inside.
        assert!(rect_contains(&r, 149, 259));
        // Outside.
        assert!(!rect_contains(&r, 99, 200));
        assert!(!rect_contains(&r, 100, 199));
    }

    // ── Additional edge-case coverage ──────────────────────────────────────

    #[test]
    fn compute_zones_count_for_standard_layout() {
        // col0=[W1] dragged (excluded), col1=[W2,W3].
        // Zones: Col{1} (left), Col{2} (right),
        //        Row{1,0}+Row{1,1} (W2), Row{1,1}+Row{1,2} (W3),
        //        ScrollLeft, ScrollRight → 8 total.
        let layout = test_applied_layout();
        let zones = compute_window_zones(&layout, &test_mon(), WindowId(1), 0.25, 0.25, 30);
        assert_eq!(zones.len(), 8);
    }

    #[test]
    fn zones_when_dragged_not_in_layout_exclude_nothing() {
        // dragged_id absent → find_window returns None → no column excluded,
        // so col 0 also receives zones.
        let layout = test_applied_layout();
        let zones = compute_window_zones(&layout, &test_mon(), WindowId(999), 0.25, 0.25, 30);
        assert!(zones.iter().any(|(z, _)| *z == DropZone::Column { col: 0 }));
        assert!(zones.iter().any(|(z, _)| *z == DropZone::Column { col: 1 }));
    }

    #[test]
    fn compute_zones_inner_width_clamps_to_one() {
        // lr_ratio=0.6 on a 500px column → lr_width=300, inner=500-600 < 0.
        // Row zone widths must clamp to >= 1 (no panic, no negatives).
        let layout = test_applied_layout();
        let zones = compute_window_zones(&layout, &test_mon(), WindowId(1), 0.6, 0.25, 30);
        for (zone, rect) in &zones {
            if matches!(zone, DropZone::Row { .. }) {
                assert!(
                    rect.width >= 1,
                    "row zone width must clamp to >= 1: {rect:?}"
                );
            }
        }
    }

    #[test]
    fn rect_contains_zero_area_contains_nothing() {
        // Half-open semantics: x < x+width is false when width == 0.
        let r = Rect {
            x: 10,
            y: 10,
            width: 0,
            height: 0,
        };
        assert!(!rect_contains(&r, 10, 10));
        let rw = Rect {
            x: 10,
            y: 10,
            width: 0,
            height: 5,
        };
        assert!(!rect_contains(&rw, 10, 12));
    }

    #[test]
    fn visible_column_rects_empty_layout_returns_empty() {
        let layout = AppliedLayout {
            virtual_layout: VirtualLayout::new(),
            actual_layout: ActualLayout::new(),
        };
        assert!(visible_column_rects(&layout, &test_mon()).is_empty());
    }
}
