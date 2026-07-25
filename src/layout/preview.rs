//! Drop-zone preview: pure computation of a layout after a drag-and-drop move.
//!
//! Provides [`preview_move`] (move a window already in the layout),
//! [`preview_insert`] (insert a window not yet in the layout — e.g. a float
//! being dragged into the grid), and [`preview_gap_close`] (preview the
//! remaining tiles filling the gap a dragged tile would leave behind). All
//! three clone the virtual layout, apply their operation, and project to
//! actual coordinates — without touching any live state or Win32 APIs.
//!
//! (`docs/src/dev-guide/tile-drag.md`)

use crate::common::WindowId;
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

/// Computes the layout that would result from inserting `dragged_id` (not
/// currently in the layout) into `zone`.
///
/// Unlike [`preview_move`], this is an **insert-only** operation: it assumes
/// `dragged_id` is NOT in `virtual_layout` (e.g. a float window being dragged
/// back into the tiling grid). The window is inserted at the target zone
/// without any prior removal step, and no source-column shift adjustment is
/// applied (nothing was removed).
///
/// Returns `None` if:
/// - the zone is `ScrollLeft` or `ScrollRight` (not a layout mutation),
/// - `dragged_id` is already present in the layout (caller error — use
///   [`preview_move`] instead, since this function refuses to create a
///   duplicate),
/// - the target column is out of bounds for a `Row` zone.
///
/// # Zone → mutation mapping
///
/// - `Column { col }` → new single-row column at index `col` (clamped to append)
/// - `Row { col, row }` → new row at position `row` in column `col`
/// - `ScrollLeft` / `ScrollRight` → `None` (not a layout mutation)
#[must_use]
pub fn preview_insert(
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

    // This function is for windows NOT yet in the layout. If the id is already
    // present, the caller should have used preview_move; bail out rather than
    // risk a duplicate insertion.
    if virtual_layout.find_window(dragged_id).is_some() {
        return None;
    }

    // Preserve the original viewport offset (no removal step to clamp it).
    let original_offset = virtual_layout.viewport_offset;

    let mut layout = virtual_layout.clone();

    match zone {
        DropZone::Column { col } => {
            // No source removal → no index shift. Clamp to append at the end.
            let pos = col.min(layout.columns.len());
            let new_col = make_single_row_column(dragged_id, config.column_width as i32, config);
            layout.columns.insert(pos, new_col);
        }
        DropZone::Row { col, row } => {
            if col >= layout.columns.len() {
                return None;
            }
            insert_row_at(&mut layout, col, row, dragged_id, config);
        }
        DropZone::ScrollLeft | DropZone::ScrollRight => return None,
    }

    layout.viewport_offset = original_offset;

    let actual = projection::project(&layout, monitor, &config.padding);

    Some(AppliedLayout {
        virtual_layout: layout,
        actual_layout: actual,
    })
}

/// Computes the layout that would result from removing `dragged_id` — i.e.
/// the remaining tiles closing the gap it leaves behind.
///
/// Used for the **center gap-closing preview**: while a tile is dragged over
/// the center (uncovered) region, this shows the user where the remaining
/// windows would land if the dragged window were promoted to float on release.
/// The preview is computed without committing, so the actual tiling state is
/// untouched until the user releases.
///
/// Unlike the removal inside [`preview_move`], this preserves the original
/// viewport offset so the preview does not pan the camera.
///
/// Returns `None` if `dragged_id` is not in the layout (e.g. a float source,
/// for which there is no gap to close).
#[must_use]
pub fn preview_gap_close(
    virtual_layout: &VirtualLayout,
    dragged_id: WindowId,
    config: &MutationConfig,
    monitor: &MonitorInfo,
) -> Option<AppliedLayout> {
    // The dragged window must exist in the layout for there to be a gap.
    virtual_layout.find_window(dragged_id)?;

    let original_offset = virtual_layout.viewport_offset;

    let mut layout = remove_window(virtual_layout, dragged_id, config);

    // Preserve the original viewport offset so the preview does not pan.
    layout.viewport_offset = original_offset;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Rect;
    use crate::layout::types::Padding;

    fn test_config() -> MutationConfig {
        MutationConfig {
            monitor_width: 1920,
            monitor_height: 1080,
            min_window_height_px: 100,
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

    // ── preview_insert (float-source: window NOT in layout) ────────────────

    #[test]
    fn insert_column_at_index_zero() {
        // Layout [A], [B, C]. Insert W99 (absent) as Column{col:0} →
        // [W99], [A], [B, C]. No removal, so no shift adjustment.
        let layout = two_col_layout();
        let applied = preview_insert(
            &layout,
            WindowId(99),
            DropZone::Column { col: 0 },
            &test_config(),
            &test_monitor(),
        )
        .expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 3);
        assert_eq!(
            applied.virtual_layout.columns[0].rows[0].window_id,
            WindowId(99)
        );
        assert_eq!(
            applied.virtual_layout.columns[1].rows[0].window_id,
            WindowId(1)
        ); // A unmoved
    }

    #[test]
    fn insert_column_clamps_to_append() {
        // Out-of-range col index clamps to append at the end.
        let layout = two_col_layout();
        let applied = preview_insert(
            &layout,
            WindowId(99),
            DropZone::Column { col: 99 },
            &test_config(),
            &test_monitor(),
        )
        .expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 3);
        assert_eq!(
            applied.virtual_layout.columns[2].rows[0].window_id,
            WindowId(99)
        );
    }

    #[test]
    fn insert_row_into_column() {
        // Layout [A], [B, C]. Insert W99 as Row{col:0,row:0} → [W99, A], [B, C].
        let layout = two_col_layout();
        let applied = preview_insert(
            &layout,
            WindowId(99),
            DropZone::Row { col: 0, row: 0 },
            &test_config(),
            &test_monitor(),
        )
        .expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 2);
        let col = &applied.virtual_layout.columns[0];
        assert_eq!(col.rows.len(), 2);
        assert_eq!(col.rows[0].window_id, WindowId(99));
        assert_eq!(col.rows[1].window_id, WindowId(1));
    }

    #[test]
    fn insert_row_out_of_bounds_returns_none() {
        let layout = two_col_layout();
        assert!(
            preview_insert(
                &layout,
                WindowId(99),
                DropZone::Row { col: 5, row: 0 },
                &test_config(),
                &test_monitor(),
            )
            .is_none(),
            "Row zone on a non-existent column should return None"
        );
    }

    #[test]
    fn insert_refuses_window_already_in_layout() {
        // W1 is in the layout — preview_insert must refuse to duplicate it.
        let layout = two_col_layout();
        assert!(
            preview_insert(
                &layout,
                WindowId(1),
                DropZone::Column { col: 0 },
                &test_config(),
                &test_monitor(),
            )
            .is_none(),
            "preview_insert must return None when the window is already in the layout"
        );
    }

    #[test]
    fn insert_scroll_zone_returns_none() {
        let layout = two_col_layout();
        assert!(
            preview_insert(
                &layout,
                WindowId(99),
                DropZone::ScrollRight,
                &test_config(),
                &test_monitor(),
            )
            .is_none()
        );
    }

    #[test]
    fn insert_preserves_viewport_offset() {
        let h1 = distribute_heights(1, 1080, 4)[0];
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), h1)),
                Column::with_row(960, Row::new(WindowId(2), h1)),
            ],
            960, // scrolled one column right
        );
        let applied = preview_insert(
            &layout,
            WindowId(99),
            DropZone::Column { col: 0 },
            &test_config(),
            &test_monitor(),
        )
        .expect("should produce layout");
        assert_eq!(
            applied.virtual_layout.viewport_offset, 960,
            "preview_insert must preserve the viewport offset"
        );
    }

    #[test]
    fn insert_grows_window_count_by_one() {
        let layout = three_col_layout(); // 4 windows
        let applied = preview_insert(
            &layout,
            WindowId(99),
            DropZone::Column { col: 1 },
            &test_config(),
            &test_monitor(),
        )
        .expect("should produce layout");
        assert_eq!(applied.virtual_layout.window_count(), 5);
        assert_eq!(applied.actual_layout.entries.len(), 5);
    }

    #[test]
    fn insert_column_into_empty_layout() {
        // Empty grid + float→tile drop → the float becomes the only column.
        // This is the zero-state edge case for preview_insert
        // (every pure fn needs an empty-layout case).
        let empty = VirtualLayout::new();
        let applied = preview_insert(
            &empty,
            WindowId(99),
            DropZone::Column { col: 0 },
            &test_config(),
            &test_monitor(),
        )
        .expect("empty layout should accept a Column-zone insert");
        assert_eq!(applied.virtual_layout.columns.len(), 1);
        assert_eq!(
            applied.virtual_layout.columns[0].rows[0].window_id,
            WindowId(99)
        );
        assert_eq!(applied.virtual_layout.window_count(), 1);
        assert_eq!(applied.actual_layout.entries.len(), 1);

        // Row zone on an empty layout has no target column (0 >= 0) → None.
        assert!(
            preview_insert(
                &empty,
                WindowId(99),
                DropZone::Row { col: 0, row: 0 },
                &test_config(),
                &test_monitor(),
            )
            .is_none(),
            "Row zone on an empty layout must return None (no column to insert into)"
        );
    }

    #[test]
    fn insert_row_clamps_row_index_to_append() {
        // Row index beyond the column's row count must clamp to append at the
        // bottom of the target column. Exercises insert_row_at's row_idx.min
        // clamp on the preview_insert path.
        // [A], [B, C] → insert W99 as Row{col:1, row:99} → [A], [B, C, W99].
        let layout = two_col_layout();
        let applied = preview_insert(
            &layout,
            WindowId(99),
            DropZone::Row { col: 1, row: 99 },
            &test_config(),
            &test_monitor(),
        )
        .expect("valid column with overflowing row index should clamp");
        let col = &applied.virtual_layout.columns[1];
        assert_eq!(col.rows.len(), 3);
        assert_eq!(col.rows.last().unwrap().window_id, WindowId(99));
        // Original rows keep their relative order.
        assert_eq!(col.rows[0].window_id, WindowId(2));
        assert_eq!(col.rows[1].window_id, WindowId(3));
    }

    // ── preview_gap_close (center gap-closing preview) ─────────────────────

    #[test]
    fn gap_close_removes_window_and_keeps_others() {
        // [A], [B, C] → remove A → [B, C] (col 0).
        let layout = two_col_layout();
        let applied = preview_gap_close(&layout, WindowId(1), &test_config(), &test_monitor())
            .expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 1);
        let col = &applied.virtual_layout.columns[0];
        assert_eq!(col.rows.len(), 2);
        assert_eq!(col.rows[0].window_id, WindowId(2));
        assert_eq!(col.rows[1].window_id, WindowId(3));
    }

    #[test]
    fn gap_close_removes_middle_window_of_multi_row_column() {
        // remove C (col 1, row 1) → [A], [B]. Col 1 survives with one row.
        let layout = two_col_layout();
        let applied = preview_gap_close(&layout, WindowId(3), &test_config(), &test_monitor())
            .expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 2);
        assert_eq!(
            applied.virtual_layout.columns[1].rows[0].window_id,
            WindowId(2)
        );
        assert_eq!(applied.virtual_layout.columns[1].rows.len(), 1);
    }

    #[test]
    fn gap_close_not_in_layout_returns_none() {
        // A float-source drag has no gap to close.
        let layout = two_col_layout();
        assert!(
            preview_gap_close(&layout, WindowId(99), &test_config(), &test_monitor()).is_none(),
            "preview_gap_close must return None when the window is not in the layout"
        );
    }

    #[test]
    fn gap_close_preserves_viewport_offset() {
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
        let applied = preview_gap_close(&layout, WindowId(2), &test_config(), &test_monitor())
            .expect("should produce layout");
        assert_eq!(
            applied.virtual_layout.viewport_offset, 960,
            "preview_gap_close must preserve the viewport offset (no camera pan during preview)"
        );
    }

    #[test]
    fn gap_close_single_window_yields_empty_layout() {
        // Removing the only window leaves an empty layout — a valid (Some)
        // result; the animation layer no-ops on empty actual layouts.
        let h1 = distribute_heights(1, 1080, 4)[0];
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(960, Row::new(WindowId(1), h1))], 0);
        let applied = preview_gap_close(&layout, WindowId(1), &test_config(), &test_monitor())
            .expect("should produce layout");
        assert!(applied.virtual_layout.columns.is_empty());
        assert!(applied.actual_layout.entries.is_empty());
    }
}
