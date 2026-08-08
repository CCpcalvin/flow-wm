//! Drop-zone preview: pure computation of a layout after a drag-and-drop move.
//!
//! Provides [`preview_move`] (move a window already in the layout to a new
//! drop zone) and [`resolve_drop_zone`] (map the cursor position to a
//! [`DropZone`]). Both are pure — they clone the virtual layout, apply their
//! operation, and project to actual coordinates without touching any live
//! state or Win32 APIs.
//!
//! (`docs/src/dev-guide/tile-drag.md`)

use crate::common::{Rect, WindowId};
use crate::layout::mutations::{MutationConfig, distribute_heights, remove_window};
use crate::layout::projection;
use crate::layout::types::{AppliedLayout, Column, MonitorInfo, Row, VirtualLayout};

/// Where a dragged window would land when dropped.
///
/// Produced by zone detection during a tile drag and consumed by
/// [`preview_move`] to compute the resulting layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropZone {
    /// Insert the dragged window as a new row in column `col` at row index `row`.
    Row {
        /// Target column index in the original layout.
        col: usize,
        /// Target row position within that column.
        row: usize,
    },
    /// Insert the dragged window as a new single-row column at index `col`.
    Column {
        /// Target column index in the original layout.
        col: usize,
    },
    /// Scroll the viewport left by one column. NOT a layout mutation.
    ScrollLeft,
    /// Scroll the viewport right by one column. NOT a layout mutation.
    ScrollRight,
}

/// Computes the layout that would result from moving `dragged_id` to `zone`.
///
/// Clones the virtual layout, removes the dragged window, re-inserts it at
/// the target zone, projects to actual coordinates, and returns the result.
///
/// Returns `None` if:
/// - the zone is `ScrollLeft` or `ScrollRight` (handled by daemon scroll logic),
/// - the dragged window is not found in the layout,
/// - the target column/row is out of bounds after source removal,
/// - the move is a no-op (the layout would be identical after the move).
///
/// # Zone → mutation mapping
///
/// - `Column { col }` → new single-row column at index `col`
/// - `Row { col, row }` → new row at position `row` in column `col`
/// - `ScrollLeft` / `ScrollRight` → `None` (not a layout mutation)
#[must_use]
pub fn preview_move(
    virtual_layout: &VirtualLayout,
    dragged_id: WindowId,
    zone: DropZone,
    config: &MutationConfig,
    monitor: &MonitorInfo,
) -> Option<AppliedLayout> {
    // Scroll zones are not layout mutations.
    if matches!(zone, DropZone::ScrollLeft | DropZone::ScrollRight) {
        return None;
    }

    // The dragged window must exist in the layout.
    let (src_col, _src_row) = virtual_layout.find_window(dragged_id)?;
    let src_had_single_row = virtual_layout.columns[src_col].rows.len() == 1;

    // Preserve the original viewport offset.
    let original_offset = virtual_layout.viewport_offset;

    // Step 1: Remove the dragged window from its current position.
    let mut layout = remove_window(virtual_layout, dragged_id, config);

    // Step 2: Re-insert at the target zone.
    match zone {
        DropZone::Column { col } => {
            let pos = adjust_index(col, src_col, src_had_single_row);
            let pos = pos.min(layout.columns.len());
            let new_col = make_single_row_column(dragged_id, config.column_width as i32, config);
            layout.columns.insert(pos, new_col);
        }
        DropZone::Row { col, row } => {
            let col_idx = adjust_index(col, src_col, src_had_single_row);
            if col_idx >= layout.columns.len() {
                return None;
            }
            insert_row_at(&mut layout, col_idx, row, dragged_id, config);
        }
        DropZone::ScrollLeft | DropZone::ScrollRight => return None,
    }

    // Restore the viewport offset.
    layout.viewport_offset = original_offset;

    // Step 3: No-op check.
    if layout == *virtual_layout {
        return None;
    }

    // Step 4: Project to actual screen coordinates.
    let actual = projection::project(&layout, monitor, &config.padding);

    Some(AppliedLayout {
        virtual_layout: layout,
        actual_layout: actual,
    })
}

/// Build a single-row column with height distributed for one row.
///
/// Duplicates the private `single_row_column` logic from `mutations.rs`.
fn make_single_row_column(window: WindowId, width_px: i32, config: &MutationConfig) -> Column {
    let h = distribute_heights(1, config.available_height(), config.padding.window_gap)[0];
    Column::with_row(width_px, Row::new(window, h))
}

/// Insert `window` as a new row at `row_idx` in the column at `col_idx`.
///
/// Redistributes all row heights equally for `n+1` rows. Clamps `row_idx`
/// to the current column length (append at bottom if beyond last row).
fn insert_row_at(
    layout: &mut VirtualLayout,
    col_idx: usize,
    row_idx: usize,
    window: WindowId,
    config: &MutationConfig,
) {
    let col = &mut layout.columns[col_idx];
    let n = col.rows.len() + 1;
    let heights = distribute_heights(n, config.available_height(), config.padding.window_gap);
    for (i, row) in col.rows.iter_mut().enumerate() {
        row.height = heights[i];
    }
    let pos = row_idx.min(col.rows.len());
    col.rows.insert(pos, Row::new(window, heights[n - 1]));
}

/// Map an index from the original layout to the post-removal layout.
///
/// When the source column is deleted (had one row), every column to its
/// right shifts left by one. This function subtracts one from `idx` when
/// the removed column was strictly before it.
fn adjust_index(idx: usize, src_col: usize, src_col_removed: bool) -> usize {
    if src_col_removed && src_col < idx {
        idx - 1
    } else {
        idx
    }
}

// ---------------------------------------------------------------------------
// resolve_drop_zone: cursor position to DropZone (pure function)
// ---------------------------------------------------------------------------

/// Compute the column-edge band width, capped by both the ratio and the
/// absolute maximum.
fn compute_col_edge_band(col_width: i32, ratio: f32, max_px: i32) -> i32 {
    ((col_width as f32 * ratio) as i32).min(max_px).max(1)
}

/// Check whether content exists offscreen to the left of the viewport.
fn can_scroll_left(vl: &VirtualLayout) -> bool {
    vl.viewport_offset > 0
}

/// Check whether content exists offscreen to the right of the viewport.
///
/// Derives the inter-column gap from the first two visible columns when
/// available; otherwise uses a structural check (single visible column that
/// is not the last virtual column).
fn can_scroll_right(vl: &VirtualLayout, wa_width: i32, vis_cols: &[(usize, Rect)]) -> bool {
    if vis_cols.len() >= 2 {
        // Visible columns form a contiguous virtual-index range, so the
        // first two are always adjacent on the canvas. Derive the gap from
        // their screen positions: gap = screen_x_1 - (screen_x_0 + col0_width).
        let col0_w = vl.columns[vis_cols[0].0].width_px;
        let gap = (vis_cols[1].1.x - (vis_cols[0].1.x + col0_w)).max(0);
        let total_span = projection::canvas_width(vl, gap);
        // canvas_width includes the trailing right-edge gap, which is not
        // scrollable content; compare the last column's content right edge.
        let content_right = total_span - gap;
        content_right > vl.viewport_offset + wa_width
    } else if let Some(&(idx, _)) = vis_cols.first() {
        // Single visible column: right scroll is possible iff more columns
        // exist to the right on the canvas.
        idx < vl.columns.len() - 1
    } else {
        false
    }
}

/// Build bounding rectangles for visible columns from the applied layout.
///
/// Groups actual-layout entries by virtual column index and computes each
/// column's bounding box. Columns entirely outside `wa` are excluded. Does
/// NOT exclude the dragged window's own column.
fn build_visible_column_rects(applied: &AppliedLayout, wa: &Rect) -> Vec<(usize, Rect)> {
    let vl = &applied.virtual_layout;
    if vl.columns.is_empty() {
        return Vec::new();
    }

    // Per-column bounding box: (x, y_min, y_max, width).
    // All entries in a column share the same x and width (projection
    // invariant), so only y bounds need updating.
    let mut boxes: Vec<Option<(i32, i32, i32, i32)>> = vec![None; vl.columns.len()];

    for entry in &applied.actual_layout.entries {
        if let Some((ci, _)) = vl.find_window(entry.window_id) {
            let r = entry.rect;
            match &mut boxes[ci] {
                Some(b) => {
                    b.1 = b.1.min(r.y);
                    b.2 = b.2.max(r.y + r.height);
                }
                None => {
                    boxes[ci] = Some((r.x, r.y, r.y + r.height, r.width));
                }
            }
        }
    }

    let mon_left = wa.x;
    let mon_right = wa.x + wa.width;

    let mut result = Vec::new();
    for (ci, b) in boxes.into_iter().enumerate() {
        if let Some((x, y_min, y_max, w)) = b {
            if x + w <= mon_left || x >= mon_right {
                continue;
            }
            result.push((
                ci,
                Rect {
                    x,
                    y: y_min,
                    width: w,
                    height: y_max - y_min,
                },
            ));
        }
    }
    result
}

/// Resolve the drop target for a tile drag from the cursor position.
///
/// Pure: reads only `applied`, `monitor`, and the three config knobs; touches
/// no live state or Win32 APIs. Total over the work area, every cursor
/// position maps to a zone. Returns `None` only for the empty-workspace
/// degenerate (no columns), which cannot arise for a tile drag.
///
/// Map: edge scroll, column-edge band, column body
/// ((n+1) equal row regions). See (`docs/src/dev-guide/tile-drag.md`).
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn resolve_drop_zone(
    applied: &AppliedLayout,
    monitor: &MonitorInfo,
    dragged_id: WindowId,
    mx: i32,
    my: i32,
    edge_band: i32,
    col_edge_ratio: f32,
    col_edge_max_px: i32,
) -> Option<DropZone> {
    let _ = dragged_id; // Own column is allowed, no exclusion logic needed.
    let wa = &monitor.work_area;
    let vl = &applied.virtual_layout;

    let vis_cols = build_visible_column_rects(applied, wa);
    if vis_cols.is_empty() {
        return None;
    }

    // Step 1: Edge scroll (checked first).
    let wa_right = wa.x + wa.width;
    if mx <= wa.x + edge_band && can_scroll_left(vl) {
        return Some(DropZone::ScrollLeft);
    }
    if mx >= wa_right - edge_band && can_scroll_right(vl, wa.width, &vis_cols) {
        return Some(DropZone::ScrollRight);
    }

    // Step 4: Locate cursor relative to columns.
    let first = &vis_cols[0];
    let last = &vis_cols[vis_cols.len() - 1];

    // Left of first column -> prepend.
    if mx < first.1.x {
        return Some(DropZone::Column { col: first.0 });
    }

    // Right of last column -> append.
    let last_right = last.1.x + last.1.width;
    if mx >= last_right {
        return Some(DropZone::Column { col: last.0 + 1 });
    }

    // Check seams between adjacent visible columns.
    for i in 0..vis_cols.len().saturating_sub(1) {
        let seam_left = vis_cols[i].1.x + vis_cols[i].1.width;
        let seam_right = vis_cols[i + 1].1.x;
        if mx >= seam_left && mx < seam_right {
            return Some(DropZone::Column {
                col: vis_cols[i + 1].0,
            });
        }
    }

    // Find the column body containing mx (Steps 5 to 6).
    for (col_idx, col_rect) in &vis_cols {
        let col_right = col_rect.x + col_rect.width;
        if mx >= col_rect.x && mx < col_right {
            // Step 5: Column-edge band -> column insert.
            let band = compute_col_edge_band(col_rect.width, col_edge_ratio, col_edge_max_px);
            if mx < col_rect.x + band {
                return Some(DropZone::Column { col: *col_idx });
            }
            if mx > col_right - band {
                return Some(DropZone::Column { col: col_idx + 1 });
            }

            // Step 6: Column body -> (n+1) row split.
            let n = vl.columns[*col_idx].rows.len();
            // n >= 1 (column invariant), so n+1 >= 2. col_rect.height >= 1
            // in practice, but .max(1) guards the theoretical degenerate.
            let rh = (col_rect.height / (n + 1) as i32).max(1);
            let rel_y = (my - col_rect.y).max(0);
            let j = (rel_y / rh) as usize;
            let j = j.min(n);
            return Some(DropZone::Row {
                col: *col_idx,
                row: j,
            });
        }
    }

    // Defensive fallback, unreachable for valid inputs where vis_cols is
    // non-empty and mx falls within the column range.
    Some(DropZone::Column { col: last.0 + 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Rect;
    use crate::layout::ActualLayout;
    use crate::layout::types::Padding;

    fn test_config() -> MutationConfig {
        MutationConfig {
            monitor_width: 1920,
            monitor_height: 1080,
            min_window_height_px: 100,
            min_row_height_px: 100,
            column_width: 960,
            min_column_width_px: 200,
            max_n: 1,
            abs_max_width: 1912,
            padding: Padding {
                window_gap: 4,
                up: 0,
                down: 0,
            },
            columns_per_screen: 2,
        }
    }

    fn test_monitor() -> MonitorInfo {
        MonitorInfo {
            work_area: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        }
    }

    /// Two-column layout: col 0 = [A], col 1 = [B, C].
    fn two_col_layout() -> VirtualLayout {
        let h1 = distribute_heights(1, 1080, 4)[0];
        let h2 = distribute_heights(2, 1080, 4);
        VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), h1)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(2), h2[0]), Row::new(WindowId(3), h2[1])],
                ),
            ],
            0,
        )
    }

    /// Three-column layout: col 0 = [A], col 1 = [B], col 2 = [C, D].
    fn three_col_layout() -> VirtualLayout {
        let h1 = distribute_heights(1, 1080, 4)[0];
        let h2 = distribute_heights(2, 1080, 4);
        VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), h1)),
                Column::with_row(960, Row::new(WindowId(2), h1)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(3), h2[0]), Row::new(WindowId(4), h2[1])],
                ),
            ],
            0,
        )
    }

    // ── Column zone tests ──────────────────────────────────────────────────

    #[test]
    fn column_zone_inserts_new_column_at_index() {
        // [A], [B, C] → drag B to Column{col:0} → [B], [A], [C]
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(2),
            DropZone::Column { col: 0 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 3);
        assert_eq!(
            applied.virtual_layout.columns[0].rows[0].window_id,
            WindowId(2)
        ); // B at col 0
        assert_eq!(
            applied.virtual_layout.columns[1].rows[0].window_id,
            WindowId(1)
        ); // A shifted to col 1
    }

    #[test]
    fn column_zone_at_index_zero() {
        // [A], [B, C] → drag B to Column{col:0} → [B], [A], [C]
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(2),
            DropZone::Column { col: 0 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 3);
        assert_eq!(
            applied.virtual_layout.columns[0].rows[0].window_id,
            WindowId(2)
        );
    }

    #[test]
    fn column_zone_appends_at_end() {
        // [A], [B], [C, D] → drag B (col 1, single row) to Column{col:99}
        // Remove B → [A], [C, D] (2 cols). adjust(99, 1, true) = 98. min(98, 2) = 2.
        // Insert at 2 → [A], [C, D], [B]
        let layout = three_col_layout();
        let result = preview_move(
            &layout,
            WindowId(2),
            DropZone::Column { col: 99 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 3);
        let last = applied.virtual_layout.columns.last().unwrap();
        assert_eq!(last.rows[0].window_id, WindowId(2)); // B appended at end
    }

    #[test]
    fn column_zone_clamps_beyond_end() {
        // [A], [B, C] → drag A to Column{col:99} → clamps to append
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(1),
            DropZone::Column { col: 99 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 2);
        assert_eq!(
            applied.virtual_layout.columns[1].rows[0].window_id,
            WindowId(1)
        );
    }

    #[test]
    fn column_zone_source_removal_shift() {
        // [A], [B], [C, D] → drag A (col 0, single row) to Column{col:2}
        // Remove A → [B], [C, D]. adjust_index(2, 0, true) = 1. Insert at 1.
        // Result: [B], [A], [C, D]
        let layout = three_col_layout();
        let result = preview_move(
            &layout,
            WindowId(1),
            DropZone::Column { col: 2 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 3);
        assert_eq!(
            applied.virtual_layout.columns[1].rows[0].window_id,
            WindowId(1)
        );
        assert_eq!(
            applied.virtual_layout.columns[0].rows[0].window_id,
            WindowId(2)
        );
    }

    // ── Row zone tests ─────────────────────────────────────────────────────

    #[test]
    fn row_zone_inserts_at_top() {
        // [A], [B, C] → drag A to Row{col:1, row:0} → [B,C] → [A, B, C]
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(1),
            DropZone::Row { col: 1, row: 0 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 1);
        let col = &applied.virtual_layout.columns[0];
        assert_eq!(col.rows.len(), 3);
        assert_eq!(col.rows[0].window_id, WindowId(1)); // A at top
        assert_eq!(col.rows[1].window_id, WindowId(2));
        assert_eq!(col.rows[2].window_id, WindowId(3));
    }

    #[test]
    fn row_zone_inserts_at_bottom() {
        // [A], [B, C] → drag A to Row{col:1, row:99} → clamped, appended
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(1),
            DropZone::Row { col: 1, row: 99 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        let col = &applied.virtual_layout.columns[0];
        assert_eq!(col.rows.last().unwrap().window_id, WindowId(1));
    }

    #[test]
    fn row_zone_inserts_in_middle() {
        // [A], [B, C] → drag A to Row{col:1, row:1} → [B, A, C]
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(1),
            DropZone::Row { col: 1, row: 1 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        let col = &applied.virtual_layout.columns[0];
        assert_eq!(col.rows.len(), 3);
        assert_eq!(col.rows[0].window_id, WindowId(2)); // B
        assert_eq!(col.rows[1].window_id, WindowId(1)); // A in middle
        assert_eq!(col.rows[2].window_id, WindowId(3)); // C
    }

    #[test]
    fn row_zone_cross_column() {
        // [A], [B, C] → drag C to Row{col:0, row:0} → [C, A], [B]
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(3),
            DropZone::Row { col: 0, row: 0 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 2);
        assert_eq!(
            applied.virtual_layout.columns[0].rows[0].window_id,
            WindowId(3)
        ); // C at top of col 0
        assert_eq!(
            applied.virtual_layout.columns[1].rows[0].window_id,
            WindowId(2)
        ); // B alone
    }

    #[test]
    fn row_zone_within_same_column() {
        // [A], [B, C] → drag C (row 1) to Row{col:1, row:0} → [A], [C, B]
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(3),
            DropZone::Row { col: 1, row: 0 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        let col = &applied.virtual_layout.columns[1];
        assert_eq!(col.rows[0].window_id, WindowId(3)); // C moved up
        assert_eq!(col.rows[1].window_id, WindowId(2)); // B
    }

    // ── Scroll zone tests ──────────────────────────────────────────────────

    #[test]
    fn scroll_left_returns_none() {
        let layout = two_col_layout();
        assert!(
            preview_move(
                &layout,
                WindowId(1),
                DropZone::ScrollLeft,
                &test_config(),
                &test_monitor()
            )
            .is_none()
        );
    }

    #[test]
    fn scroll_right_returns_none() {
        let layout = two_col_layout();
        assert!(
            preview_move(
                &layout,
                WindowId(1),
                DropZone::ScrollRight,
                &test_config(),
                &test_monitor()
            )
            .is_none()
        );
    }

    // ── Edge cases ─────────────────────────────────────────────────────────

    #[test]
    fn empty_layout_returns_none() {
        let empty = VirtualLayout::new();
        for zone in [
            DropZone::Column { col: 0 },
            DropZone::Row { col: 0, row: 0 },
        ] {
            assert!(
                preview_move(&empty, WindowId(1), zone, &test_config(), &test_monitor()).is_none(),
                "empty layout should return None for zone {zone:?}"
            );
        }
    }

    #[test]
    fn window_not_found_returns_none() {
        let layout = two_col_layout();
        assert!(
            preview_move(
                &layout,
                WindowId(99),
                DropZone::Column { col: 0 },
                &test_config(),
                &test_monitor()
            )
            .is_none()
        );
    }

    #[test]
    fn no_op_column_same_spot() {
        // [A], [B] single-row columns. Drag A to Column{col:0}.
        // Remove A → [B]. Insert at 0 → [A], [B] — identical → no-op.
        let h1 = distribute_heights(1, 1080, 4)[0];
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), h1)),
                Column::with_row(960, Row::new(WindowId(2), h1)),
            ],
            0,
        );
        assert!(
            preview_move(
                &layout,
                WindowId(1),
                DropZone::Column { col: 0 },
                &test_config(),
                &test_monitor()
            )
            .is_none(),
            "dropping back in same spot should be no-op"
        );
    }

    #[test]
    fn row_zone_on_removed_source_column_returns_none() {
        // [A] alone in col 0. Drag A to Row{col:0, row:0}.
        // Remove A → col 0 gone. adjust_index(0, 0, true) = 0.
        // 0 >= 0 (layout has 0 columns) → None.
        let h1 = distribute_heights(1, 1080, 4)[0];
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(960, Row::new(WindowId(1), h1))], 0);
        assert!(
            preview_move(
                &layout,
                WindowId(1),
                DropZone::Row { col: 0, row: 0 },
                &test_config(),
                &test_monitor()
            )
            .is_none()
        );
    }

    #[test]
    fn row_zone_out_of_bounds_column_returns_none() {
        // [A], [B, C] → drag A to Row{col:5, row:0}
        // Remove A → [B, C] (1 col). adjust_index(5, 0, true) = 4.
        // 4 >= 1 → None.
        let layout = two_col_layout();
        assert!(
            preview_move(
                &layout,
                WindowId(1),
                DropZone::Row { col: 5, row: 0 },
                &test_config(),
                &test_monitor()
            )
            .is_none()
        );
    }

    // ── Projection correctness ─────────────────────────────────────────────

    #[test]
    fn preview_produces_actual_layout() {
        // Drag B to Column{col:0} — a real move, not a no-op.
        let layout = two_col_layout();
        let applied = preview_move(
            &layout,
            WindowId(2),
            DropZone::Column { col: 0 },
            &test_config(),
            &test_monitor(),
        )
        .expect("should produce layout");

        // All windows must have entries in the actual layout.
        assert_eq!(applied.actual_layout.entries.len(), 3);
        let expected_actual = projection::project(
            &applied.virtual_layout,
            &test_monitor(),
            &test_config().padding,
        );
        assert_eq!(applied.actual_layout, expected_actual);
    }

    // ── Invariants ─────────────────────────────────────────────────────────

    #[test]
    fn preview_preserves_viewport_offset() {
        // remove_window clamps viewport_offset to the new span; preview_move
        // must restore the original offset so a drag preview never moves the
        // camera. (`docs/src/dev-guide/tile-drag.md`)
        let h1 = distribute_heights(1, 1080, 4)[0];
        let h2 = distribute_heights(2, 1080, 4);
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), h1)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(2), h2[0]), Row::new(WindowId(3), h2[1])],
                ),
            ],
            960, // scrolled one column right
        );
        let applied = preview_move(
            &layout,
            WindowId(2),
            DropZone::Column { col: 0 },
            &test_config(),
            &test_monitor(),
        )
        .expect("should produce layout");
        assert_eq!(
            applied.virtual_layout.viewport_offset, 960,
            "viewport offset must be preserved across preview_move"
        );
    }

    #[test]
    fn preview_preserves_window_count() {
        // A move must never drop or duplicate a window.
        let layout = three_col_layout();
        let applied = preview_move(
            &layout,
            WindowId(1),
            DropZone::Column { col: 2 },
            &test_config(),
            &test_monitor(),
        )
        .expect("should produce layout");
        assert_eq!(applied.virtual_layout.window_count(), 4);
        let ids: std::collections::HashSet<WindowId> = applied
            .virtual_layout
            .columns
            .iter()
            .flat_map(|c| c.rows.iter().map(|r| r.window_id))
            .collect();
        assert_eq!(ids.len(), 4, "window ids must remain unique after move");
    }

    #[test]
    fn row_insert_redistributes_heights_equally() {
        // insert_row_at must redistribute all rows of the target column to
        // equal heights (±1px integer remainder) for the new row count.
        let layout = two_col_layout();
        let applied = preview_move(
            &layout,
            WindowId(1),
            DropZone::Row { col: 1, row: 0 },
            &test_config(),
            &test_monitor(),
        )
        .expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 1);
        let col = &applied.virtual_layout.columns[0];
        assert_eq!(col.rows.len(), 3);
        let heights: Vec<i32> = col.rows.iter().map(|r| r.height).collect();
        let spread = heights.iter().max().unwrap() - heights.iter().min().unwrap();
        assert!(
            spread <= 1,
            "row heights must be equal up to 1px remainder: {heights:?}"
        );
    }

    #[test]
    fn no_op_row_same_spot() {
        // [A], [B, C] → drag C (col 1, row 1) back to Row{col:1, row:1}.
        // Remove + re-insert at the same slot yields an identical layout → None.
        let layout = two_col_layout();
        assert!(
            preview_move(
                &layout,
                WindowId(3),
                DropZone::Row { col: 1, row: 1 },
                &test_config(),
                &test_monitor(),
            )
            .is_none(),
            "dropping a window back into its own row slot should be a no-op"
        );
    }

    // =================================================================
    // resolve_drop_zone tests
    // =================================================================

    fn rdz_monitor() -> MonitorInfo {
        MonitorInfo {
            work_area: Rect {
                x: 0,
                y: 0,
                width: 1000,
                height: 1000,
            },
        }
    }

    fn rdz_padding(gap: i32) -> Padding {
        Padding {
            window_gap: gap,
            up: 0,
            down: 0,
        }
    }

    /// Build an AppliedLayout projected with the given gap and viewport offset.
    fn make_rdz_applied(columns: Vec<Column>, viewport_offset: i32, gap: i32) -> AppliedLayout {
        let vl = VirtualLayout::with_columns(columns, viewport_offset);
        let actual = projection::project(&vl, &rdz_monitor(), &rdz_padding(gap));
        AppliedLayout {
            virtual_layout: vl,
            actual_layout: actual,
        }
    }

    // -- n=1 column: row split --

    #[test]
    fn rdz_single_col_top_region() {
        // 1 col (W1, h=1000, w=500), gap=0. Col at (0,0,500,1000).
        // band=min(90,72)=72. Body: [72,428). n=1, rh=500.
        // my=100 in top half -> row 0.
        let layout = make_rdz_applied(
            vec![Column::with_row(500, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                100,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 0 }),
        );
    }

    #[test]
    fn rdz_single_col_bottom_region() {
        let layout = make_rdz_applied(
            vec![Column::with_row(500, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        // my=800 in bottom half -> row 1.
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                800,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 1 }),
        );
    }

    #[test]
    fn rdz_single_col_boundary_at_half() {
        let layout = make_rdz_applied(
            vec![Column::with_row(500, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        // n=1, rh=500. Boundary at col.y + 500 = 500.
        // my=499: rel_y=499, 499/500=0 -> row 0.
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                499,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 0 }),
        );
        // my=500: rel_y=500, 500/500=1 -> row 1.
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 1 }),
        );
    }

    // -- n=2 column: three regions --

    #[test]
    fn rdz_two_row_col_three_regions() {
        // 1 col, 2 rows (each 500), w=500, gap=0.
        // Col at (0,0,500,1000). n=2, rh=1000/3=333.
        let layout = make_rdz_applied(
            vec![Column::with_rows(
                500,
                vec![Row::new(WindowId(1), 500), Row::new(WindowId(2), 500)],
            )],
            0,
            0,
        );
        // Top region (0..333): my=100 -> row 0.
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                100,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 0 }),
        );
        // Middle region (333..666): my=400 -> row 1.
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                400,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 1 }),
        );
        // Bottom region (666..1000): my=800 -> row 2.
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                800,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 2 }),
        );
    }

    #[test]
    fn rdz_two_row_col_boundaries() {
        let layout = make_rdz_applied(
            vec![Column::with_rows(
                500,
                vec![Row::new(WindowId(1), 500), Row::new(WindowId(2), 500)],
            )],
            0,
            0,
        );
        // n=2, rh=333. Boundaries at 333 and 666.
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                332,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 0 }),
        );
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                333,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 1 }),
        );
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                666,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 2 }),
        );
    }

    // -- Column-edge band --

    #[test]
    fn rdz_col_edge_band_left() {
        let layout = make_rdz_applied(
            vec![Column::with_row(500, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        // band=min(90,72)=72. mx=50 (< 0+72=72) -> Column{col:0}.
        assert_eq!(
            resolve_drop_zone(&layout, &rdz_monitor(), WindowId(99), 50, 500, 30, 0.18, 72),
            Some(DropZone::Column { col: 0 }),
        );
    }

    #[test]
    fn rdz_col_edge_band_right() {
        let layout = make_rdz_applied(
            vec![Column::with_row(500, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        // band=72. col_right=500. mx=480 (> 500-72=428) -> Column{col:1}.
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                480,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Column { col: 1 }),
        );
    }

    #[test]
    fn rdz_band_caps_at_max_px() {
        // Wide column: 0.18*1000=180 > 72 -> band=72.
        let layout = make_rdz_applied(
            vec![Column::with_row(1000, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        // mx=71 -> inside left band (0..72).
        assert_eq!(
            resolve_drop_zone(&layout, &rdz_monitor(), WindowId(99), 71, 500, 30, 0.18, 72),
            Some(DropZone::Column { col: 0 }),
        );
        // mx=73 -> outside band, in body. n=1, rh=500, my=500 -> j=1.
        assert_eq!(
            resolve_drop_zone(&layout, &rdz_monitor(), WindowId(99), 73, 500, 30, 0.18, 72),
            Some(DropZone::Row { col: 0, row: 1 }),
        );
    }

    #[test]
    fn rdz_band_uses_ratio_for_narrow_col() {
        // Narrow column: 0.18*200=36 < 72 -> band=36.
        let layout = make_rdz_applied(
            vec![Column::with_row(200, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        // mx=35 -> inside left band (0..36).
        assert_eq!(
            resolve_drop_zone(&layout, &rdz_monitor(), WindowId(99), 35, 500, 30, 0.18, 72),
            Some(DropZone::Column { col: 0 }),
        );
        // mx=37 -> body. col_right=200. 37 < 200-36=164 -> body.
        // n=1, rh=500, my=500 -> j=1.
        assert_eq!(
            resolve_drop_zone(&layout, &rdz_monitor(), WindowId(99), 37, 500, 30, 0.18, 72),
            Some(DropZone::Row { col: 0, row: 1 }),
        );
    }

    // -- Seam between columns --

    #[test]
    fn rdz_seam_between_columns() {
        // 2 cols, each 490 wide, gap=10.
        // Canvas: col0 at 10, col1 at 510. Screen: col0 (10,0,490,1000),
        // col1 (510,0,490,1000). Seam: [500, 510).
        let layout = make_rdz_applied(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
            ],
            0,
            10,
        );
        // mx=505 -> in seam -> Column{col:1} (right neighbor).
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                505,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Column { col: 1 }),
        );
    }

    // -- Cursor outside columns --

    #[test]
    fn rdz_left_of_first_column() {
        // 2 cols, gap=10. Col0 at (10,0,490,1000). mx=5 -> left of col0.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
            ],
            0,
            10,
        );
        assert_eq!(
            resolve_drop_zone(&layout, &rdz_monitor(), WindowId(99), 5, 500, 30, 0.18, 72),
            Some(DropZone::Column { col: 0 }),
        );
    }

    #[test]
    fn rdz_right_of_last_column() {
        // 2 cols, gap=10. Col1 right edge = 510+490=1000. mx=1001 -> append.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
            ],
            0,
            10,
        );
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                1001,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Column { col: 2 }),
        );
    }

    // -- Own column allowed --

    #[test]
    fn rdz_own_column_allowed() {
        // W1 in col 0. Drag W1, cursor over col 0 -> resolves, not excluded.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
            ],
            0,
            0,
        );
        let zone = resolve_drop_zone(&layout, &rdz_monitor(), WindowId(1), 250, 500, 30, 0.18, 72);
        assert!(zone.is_some(), "own column should not be excluded");
        match zone {
            Some(DropZone::Row { col: 0, .. }) | Some(DropZone::Column { col: 0 }) => {}
            other => panic!("expected zone in col 0, got {other:?}"),
        }
    }

    // -- Edge scroll --

    #[test]
    fn rdz_edge_scroll_left_with_offscreen() {
        // 3 cols, each 500, gap=0, viewport_offset=500.
        // Screen: col0 parked left, col1 at (0,0,500,1000), col2 at (500,0,500,1000).
        // viewport_offset=500 > 0 -> can_scroll_left=true.
        // mx=10 (<= 0+30=30) -> ScrollLeft.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
                Column::with_row(500, Row::new(WindowId(3), 1000)),
            ],
            500,
            0,
        );
        assert_eq!(
            resolve_drop_zone(&layout, &rdz_monitor(), WindowId(99), 10, 500, 30, 0.18, 72),
            Some(DropZone::ScrollLeft),
        );
    }

    #[test]
    fn rdz_edge_scroll_left_no_offscreen() {
        // 2 cols, viewport_offset=0. No left offscreen.
        // mx=10 -> in left band but no scroll -> falls through to col insert.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
            ],
            0,
            0,
        );
        let zone = resolve_drop_zone(&layout, &rdz_monitor(), WindowId(99), 10, 500, 30, 0.18, 72);
        assert_ne!(zone, Some(DropZone::ScrollLeft));
    }

    #[test]
    fn rdz_edge_scroll_right_with_offscreen() {
        // 3 cols, each 500, gap=0, viewport_offset=0.
        // Total span=1500 > 0+1000. Right scroll possible.
        // mx=990 (>= 1000-30=970) -> ScrollRight.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
                Column::with_row(500, Row::new(WindowId(3), 1000)),
            ],
            0,
            0,
        );
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                990,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::ScrollRight),
        );
    }

    #[test]
    fn rdz_edge_scroll_right_no_offscreen() {
        // 2 cols, each 500, gap=0, viewport_offset=0.
        // Total span=1000 = wa_width. No right offscreen.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
            ],
            0,
            0,
        );
        let zone = resolve_drop_zone(
            &layout,
            &rdz_monitor(),
            WindowId(99),
            990,
            500,
            30,
            0.18,
            72,
        );
        assert_ne!(zone, Some(DropZone::ScrollRight));
    }

    // -- Totality --

    #[test]
    fn rdz_totality_all_points_some() {
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
            ],
            0,
            0,
        );
        for &mx in &[0, 200, 499, 500, 750, 999] {
            for &my in &[0, 333, 666, 999] {
                let zone =
                    resolve_drop_zone(&layout, &rdz_monitor(), WindowId(99), mx, my, 30, 0.18, 72);
                assert!(zone.is_some(), "no zone at ({mx}, {my})");
            }
        }
    }

    // -- Empty layout --

    #[test]
    fn rdz_empty_layout_returns_none() {
        let empty = AppliedLayout {
            virtual_layout: VirtualLayout::new(),
            actual_layout: ActualLayout::new(),
        };
        assert_eq!(
            resolve_drop_zone(&empty, &rdz_monitor(), WindowId(99), 500, 500, 30, 0.18, 72),
            None,
        );
    }

    // -- Scrolled viewport --

    #[test]
    fn rdz_scrolled_viewport_uses_projected_coords() {
        // 3 cols, each 500, gap=0, viewport_offset=500.
        // Screen: col1 at (0,0,500,1000), col2 at (500,0,500,1000).
        // mx=250 -> inside col1 (virtual idx 1), body -> Row{col:1, row:0}.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
                Column::with_row(500, Row::new(WindowId(3), 1000)),
            ],
            500,
            0,
        );
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                250,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 1, row: 0 }),
        );
    }

    // =================================================================
    // resolve_drop_zone — additional branch/boundary coverage
    // (these fill gaps left by the suite above: the can_scroll_right
    // single-visible-column branch, the trailing-gap content-right fix,
    // the row-index clamps, the band floor, and seam boundaries)
    // =================================================================

    // -- can_scroll_right: single-visible-column branch (vis_cols.len() < 2) --

    #[test]
    fn rdz_scroll_right_single_visible_column() {
        // 3 cols each exactly monitor-width (1000), gap=0, viewport=0.
        // Project: col0 screen [0,1000) visible; col1/col2 parked right at
        // x=1000 and excluded by build_visible_column_rects (x >= mon_right).
        // vis_cols.len() == 1, idx=0 < columns.len()-1 = 2 -> can scroll right.
        // mx=990 (>= wa_right-edge_band = 970) -> ScrollRight.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(1000, Row::new(WindowId(1), 1000)),
                Column::with_row(1000, Row::new(WindowId(2), 1000)),
                Column::with_row(1000, Row::new(WindowId(3), 1000)),
            ],
            0,
            0,
        );
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                990,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::ScrollRight),
        );
    }

    #[test]
    fn rdz_scroll_right_single_column_is_last_no_scroll() {
        // Single column total: vis_cols.len()==1, idx=0, but
        // 0 < columns.len()-1 = 0 is false -> can_scroll_right false.
        // Falls through to col body -> right edge band -> Column{col:1}.
        let layout = make_rdz_applied(
            vec![Column::with_row(1000, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        let zone = resolve_drop_zone(
            &layout,
            &rdz_monitor(),
            WindowId(99),
            990,
            500,
            30,
            0.18,
            72,
        );
        assert_ne!(zone, Some(DropZone::ScrollRight));
        assert_eq!(zone, Some(DropZone::Column { col: 1 }));
    }

    // -- can_scroll_right: trailing-gap content-right fix (line ~186) --
    // content_right = canvas_width - gap, NOT canvas_width. These two tests
    // pin the fix: gap must not inflate the scrollable span.

    #[test]
    fn rdz_scroll_right_trailing_gap_content_fits() {
        // 2 cols width 490, gap 10, viewport 0.
        // canvas_width = 10 + (490+10) + (490+10) = 1010.
        // content_right = 1010 - 10 = 1000 == wa_width. Strict `>` is false.
        // -> NOT scrollable, even though raw canvas_width (1010) > wa (1000).
        // Both cols visible: col0 screen [10,500), col1 screen [510,1000).
        // mx=985 lands in col1's right edge band -> Column{col:2}.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
            ],
            0,
            10,
        );
        let zone = resolve_drop_zone(
            &layout,
            &rdz_monitor(),
            WindowId(99),
            985,
            500,
            30,
            0.18,
            72,
        );
        assert_ne!(
            zone,
            Some(DropZone::ScrollRight),
            "trailing gap must not count as scrollable content"
        );
        assert_eq!(zone, Some(DropZone::Column { col: 2 }));
    }

    #[test]
    fn rdz_scroll_right_with_gap_still_scrolls_offscreen() {
        // 3 cols width 490, gap 10, viewport 0. col2 is parked offscreen right.
        // canvas_width = 1510; content_right = 1500 > 1000 -> scrollable.
        // The `- gap` adjustment must NOT suppress a true positive.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
                Column::with_row(490, Row::new(WindowId(3), 1000)),
            ],
            0,
            10,
        );
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                990,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::ScrollRight),
        );
    }

    // -- (n+1) row split: last-region clamp (.min(n) at line ~336) --

    #[test]
    fn rdz_two_row_col_clamps_at_very_bottom() {
        // 1 col, 2 rows (n=2), gap 0. col_rect height 1000, rh = 1000/3 = 333.
        // my=999: rel_y=999, 999/333 = 3, then .min(2) clamps to 2.
        // Without the clamp, j=3 would be out of range (only 0,1,2 valid).
        let layout = make_rdz_applied(
            vec![Column::with_rows(
                500,
                vec![Row::new(WindowId(1), 500), Row::new(WindowId(2), 500)],
            )],
            0,
            0,
        );
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                999,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 2 }),
        );
    }

    // -- negative relative y: .max(0) floor at line ~334 --

    #[test]
    fn rdz_cursor_above_column_clamps_to_row_zero() {
        // gap=10 shifts col down: first row y = up(0) + gap(10) = 10.
        // col_rect.y = 10. Cursor my=5 < 10 -> rel_y = (5-10).max(0) = 0 -> row 0.
        // Exercises the defensive .max(0) clamp on the row-split input.
        let layout = make_rdz_applied(
            vec![Column::with_row(500, Row::new(WindowId(1), 1000))],
            0,
            10,
        );
        assert_eq!(
            resolve_drop_zone(&layout, &rdz_monitor(), WindowId(99), 100, 5, 30, 0.18, 72),
            Some(DropZone::Row { col: 0, row: 0 }),
        );
    }

    // -- column-edge band: .max(1) floor in compute_col_edge_band --

    #[test]
    fn rdz_col_edge_band_floors_at_one_pixel() {
        // Very narrow col (width 3): ratio*width = 0.18*3 = 0.54 -> `as i32`
        // = 0. .min(max_px)=0. .max(1) = 1. So the left band is 1px wide;
        // mx=0 (col.x) falls inside it -> Column{col:0}. Without the floor
        // the band would be 0px and the left-edge insert zone unreachable.
        let layout = make_rdz_applied(vec![Column::with_row(3, Row::new(WindowId(1), 1000))], 0, 0);
        assert_eq!(
            resolve_drop_zone(&layout, &rdz_monitor(), WindowId(99), 0, 500, 30, 0.18, 72),
            Some(DropZone::Column { col: 0 }),
        );
        // mx=1 is past the 1px band, into the body -> Row split.
        assert_eq!(
            resolve_drop_zone(&layout, &rdz_monitor(), WindowId(99), 1, 500, 30, 0.18, 72),
            Some(DropZone::Row { col: 0, row: 1 }),
        );
    }

    // -- seam: exact boundaries map to the right neighbor --

    #[test]
    fn rdz_seam_boundaries_map_to_right_neighbor() {
        // 2 cols width 490, gap 10. col0 screen [10,500), col1 screen [510,1000).
        // Seam = [seam_left=500, seam_right=510). Inclusive on the left,
        // exclusive on the right: mx in [500,510) -> Column{col:1}.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
            ],
            0,
            10,
        );
        // Seam left boundary is inclusive: mx=500 -> Column{1}.
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                500,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Column { col: 1 }),
        );
        // Last seam pixel (seam_right=510 is exclusive): mx=509 -> Column{1}.
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                509,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Column { col: 1 }),
        );
        // mx past col1's left edge band (band=72, so body starts at 510+72=582)
        // -> col1 body row split (NOT a Column insert). col_rect.y=10 (gap
        // shifts the column down), so my=500 -> rel_y=490 -> row 0.
        assert_eq!(
            resolve_drop_zone(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                750,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 1, row: 0 }),
        );
    }
}
