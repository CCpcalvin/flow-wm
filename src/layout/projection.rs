//! Virtual layout → actual layout projection (camera to screen).
//!
//! The core **camera-to-screen** projection: takes the infinite virtual canvas
//! ([`VirtualLayout`]) and computes real pixel coordinates for every window
//! ([`ActualLayout`]) that Windows OS must render.
//!
//! # How it works
//!
//! 1. **Camera shift**: each column's virtual x-position is offset by subtracting
//!    `viewport_offset` (the camera position). Columns whose shifted range
//!    overlaps the monitor rectangle are **visible** and receive on-screen
//!    coordinates.
//!
//! 2. **Parking**: columns fully outside the viewport are **parked** at a
//!    deterministic position exactly one column-width beyond the nearest viewport
//!    edge — left zone (`monitor_left - col_width`) or right zone
//!    (`monitor_right + col_width`). Parking is necessary because Windows OS does
//!    not gracefully handle windows at extreme off-screen coordinates; parking
//!    just beyond the edge keeps scroll-in/out animations short and smooth.
//!
//! Columns are laid out in **slots** of width `col_width + window_gap`, so the
//! visual gap between adjacent columns comes from the slot structure (the canvas
//! starts at `window_gap`). Vertically, rows are stacked with one `window_gap`
//! between consecutive rows plus a leading `window_gap` after `padding.up`;
//! screen-level top/bottom margins come from `padding.up` / `padding.down`.
//!
//! # Row heights are source-of-truth here
//!
//! Each [`Row`](crate::layout::types::Row)'s `height` field is consumed
//! **verbatim** as the window's pixel height. Projection never recomputes,
//! rescales, or insets row heights — it only stacks them. Equal distribution
//! of available vertical space happens once, at mutation time
//! ([`distribute_heights`](crate::layout::mutations::distribute_heights)),
//! whenever row membership changes (add/remove/merge/promote). Between
//! mutations, heights are stable so the user can drag-resize later.
//!
//! # Padding lives here
//!
//! Padding is applied **here**, during projection — outside the window concept.
//! The [`ActualEntry`] rects produced are the **final HWND rects**, passable
//! directly to `SetWindowPos` with no further adjustment. This keeps padding
//! logic in one place so no other consumer of window rects needs to know about it.
//!
//! See the developer guide's *Projection* chapter
//! (`docs/src/dev-guide/layout/projection.md`) for the slot model and parking
//! zones with diagrams.

use crate::common::Rect;
use crate::layout::types::{
    ActualEntry, ActualLayout, Column, MonitorInfo, Padding, VirtualLayout,
};

/// Project a virtual layout onto a monitor viewport.
///
/// This is the **camera-to-screen** conversion. For each column on the infinite
/// canvas, it:
///
/// 1. **Visible columns**: Computes real screen coordinates by subtracting the
///    camera offset (`viewport_offset`) from the column's virtual x-position.
///
/// 2. **Off-screen-left columns**: Parks at `monitor_left - col_width` (one
///    column-width beyond the left viewport edge).
///
/// 3. **Off-screen-right columns**: Parks at `monitor_right + col_width` (one
///    column-width beyond the right viewport edge).
///
/// Every window gets an [`ActualEntry`] — visible or parked. This ensures the
/// diff engine can track smooth transitions when windows scroll in/out.
///
/// # Slot Model
///
/// Each column occupies a **slot** on the virtual canvas whose width is that
/// column's own [`width_px`](crate::layout::types::Column::width_px) plus
/// `window_gap`. The canvas starts at `window_gap` (left-edge gap). Column
/// x-positions are therefore a **prefix sum**, not a uniform stride — the
/// running `canvas_x` accumulator in [`project`] advances by
/// `col.width_px + window_gap` per column.
///
/// The window rect fills the full `col.width_px` (no horizontal inset); the
/// gap between columns comes from the slot structure itself.
#[must_use]
pub fn project(
    virtual_layout: &VirtualLayout,
    monitor: &MonitorInfo,
    padding: &Padding,
) -> ActualLayout {
    let monitor_rect = monitor.work_area;
    let viewport_left = virtual_layout.viewport_offset;
    let viewport_right = viewport_left + monitor_rect.width;
    let slot_gap = padding.window_gap;

    let mut entries = Vec::new();
    // Canvas starts at window_gap (left-edge gap in the slot model)
    let mut canvas_x: i32 = slot_gap;

    for column in &virtual_layout.columns {
        let col_width = column.width_px;
        let canvas_col_left = canvas_x;
        let canvas_col_right = canvas_x + col_width;

        let visible = canvas_col_right > viewport_left && canvas_col_left < viewport_right;

        // Determine the screen x for this column's windows. Visible columns get
        // their real screen position; off-screen columns get a deterministic
        // parking position exactly one column-width beyond the nearest edge.
        let x = if visible {
            monitor_rect.x + (canvas_col_left - viewport_left)
        } else if canvas_col_right <= viewport_left {
            // Off-screen left: park one column-width beyond the left edge
            monitor_rect.x - col_width
        } else {
            // Off-screen right: park one column-width beyond the right edge
            monitor_rect.x + monitor_rect.width
        };

        stack_column_rows(column, x, monitor_rect, col_width, padding, &mut entries);

        // Advance by slot width (col_width + window_gap)
        canvas_x += col_width + slot_gap;
    }

    ActualLayout { entries }
}

/// Stack a column's rows into actual entries at the given screen x.
///
/// Each row's [`height`](crate::layout::types::Row::height) is consumed
/// **verbatim** as the window's pixel height. Rows are stacked downward with
/// one `padding.window_gap` between consecutive rows, plus a leading
/// `padding.window_gap` after `padding.up`. The window fills the full
/// `col_width` horizontally — inter-column gaps come from the slot model in
/// [`project`].
///
/// The same stacking is used for **visible** and **parked** columns. Only the
/// `x` argument differs (real screen position vs. parked offset). This keeps
/// window dimensions identical across parked ↔ visible transitions so the
/// animation engine can interpolate smoothly.
///
/// The resulting [`ActualEntry::rect`](crate::layout::ActualEntry::rect) is the
/// final HWND rect.
fn stack_column_rows(
    column: &Column,
    x: i32,
    monitor_rect: Rect,
    col_width: i32,
    padding: &Padding,
    entries: &mut Vec<ActualEntry>,
) {
    if column.rows.is_empty() {
        return;
    }

    // First row starts one gap below the top padding edge; each subsequent row
    // starts one gap below the previous row's bottom.
    let mut y = monitor_rect.y + padding.up + padding.window_gap;

    for row in &column.rows {
        entries.push(ActualEntry {
            window_id: row.window_id,
            rect: Rect {
                x,
                y,
                width: col_width,
                height: row.height,
            },
        });
        y += row.height + padding.window_gap;
    }
}

/// Compute the canvas width consumed by all columns using the slot model.
///
/// Each column occupies a slot of `col.width_px + window_gap`, starting from
/// `window_gap` (the initial left-edge gap). The total canvas width is:
/// `window_gap + sum(width_px_i + window_gap)`.
///
/// When `window_gap = 0`, this degenerates to the packed model (sum of col widths).
#[must_use]
pub fn canvas_width(layout: &VirtualLayout, window_gap: i32) -> i32 {
    if layout.columns.is_empty() {
        return 0;
    }
    let total_slots: i32 = layout.columns.iter().map(|c| c.width_px + window_gap).sum();
    // Leading edge gap is the window_gap at the very start of the canvas.
    // But each slot already includes its trailing gap, and the first slot starts
    // at window_gap. So the total span from canvas_x=0 to the end of the last
    // slot's gap is: window_gap (initial) + sum(col_width_i + window_gap).
    // The trailing gap of the last column is the right-edge gap.
    window_gap + total_slots
}

/// Compute the pixel width of a single step (one slot = column's own width + window gap).
/// Used by scroll operations to determine viewport offset changes.
#[must_use]
pub fn column_step_width(column: &Column, window_gap: i32) -> i32 {
    column.width_px + window_gap
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::WindowId;
    use crate::layout::types::Row;

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

    fn test_padding() -> Padding {
        Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        }
    }

    #[test]
    fn project_single_column_fills_monitor() {
        // Row height pre-distributed by mutation layer: (1080 - 2*4) / 1 = 1072.
        let layout = VirtualLayout::with_columns(
            vec![Column::with_row(1920, Row::new(WindowId(1), 1072))],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        assert_eq!(actual.entries.len(), 1);
        let entry = &actual.entries[0];
        assert_eq!(entry.window_id, WindowId(1));
        // Slot model: canvas starts at window_gap=4, window fills full width
        // window x = 0 + (4 - 0) = 4
        // window width = 1920 (full column width, no horizontal inset)
        // y = monitor_y + padding.up + window_gap = 0 + 0 + 4 = 4
        // height = row.height (consumed verbatim) = 1072
        assert_eq!(entry.rect.x, 4);
        assert_eq!(entry.rect.y, 4);
        assert_eq!(entry.rect.width, 1920);
        assert_eq!(entry.rect.height, 1072);
    }

    #[test]
    fn project_two_equal_columns() {
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        assert_eq!(actual.entries.len(), 2);

        let col_width = 960; // base column width (px)
        // Slot model: canvas starts at window_gap=4
        // Col 0: canvas_x = 4, screen_x = 0 + (4-0) = 4
        assert_eq!(actual.entries[0].rect.x, 4);
        assert_eq!(actual.entries[0].rect.width, col_width);
        // Col 1: canvas_x = 4 + 960 + 4 = 968, screen_x = 0 + (968-0) = 968
        assert_eq!(actual.entries[1].rect.x, col_width + 2 * 4);
        assert_eq!(actual.entries[1].rect.width, col_width);
    }

    #[test]
    fn project_column_with_two_rows() {
        // Two rows stacked: each height 534, gap 4.
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 534), Row::new(WindowId(2), 534)],
            )],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        assert_eq!(actual.entries.len(), 2);
        assert_eq!(actual.entries[0].rect.x, actual.entries[1].rect.x);
        // row 0 y = 4, row 1 y = 4 + 534 + 4 = 542
        assert!(actual.entries[1].rect.y > actual.entries[0].rect.y);
    }

    #[test]
    fn off_screen_left_parked_one_column_beyond() {
        let monitor = test_monitor();
        let padding = test_padding();

        // Two columns, viewport scrolled past the first
        // Slot model: col 0 at canvas_x=4, col 1 at canvas_x=4+960+4=968
        // viewport_offset = 968 means viewport starts at col 1's canvas position
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            968, // offset = start of col 1's slot
        );
        let actual = project(&layout, &monitor, &padding);

        // First column parked at: monitor_left - col_width = -960
        assert_eq!(actual.entries[0].rect.x, -960);
        // Second column visible on screen
        assert!(actual.entries[1].rect.x >= 0);
    }

    #[test]
    fn off_screen_right_parked_one_column_beyond() {
        let monitor = test_monitor();
        let padding = test_padding();

        // Three columns, viewport at 0, only first two visible on 1920px
        // Slot model: col 0 at canvas_x=4, col 1 at canvas_x=968, col 2 at canvas_x=1932
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)), // off-screen right
            ],
            0,
        );
        let actual = project(&layout, &monitor, &padding);

        // Third column parked at: monitor_right = 1920
        assert_eq!(actual.entries[2].rect.x, 1920);
    }

    #[test]
    fn canvas_width_empty() {
        let layout = VirtualLayout::new();
        assert_eq!(canvas_width(&layout, 4), 0);
    }

    #[test]
    fn canvas_width_two_columns() {
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        // Slot model: window_gap(4) + (960+4) + (960+4) = 4 + 964 + 964 = 1932
        assert_eq!(canvas_width(&layout, 4), 1932);
    }

    // --- Integration: Projection correctness ---

    #[test]
    fn project_three_varying_width_columns_exact_pixels() {
        // Positive: 240px + 720px + 960px columns
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(240, Row::new(WindowId(1), 0)),
                Column::with_row(720, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_padding());
        assert_eq!(actual.entries.len(), 3);

        // Slot model: canvas starts at window_gap=4
        // Column 1: 240px, canvas_x=4, screen_x = 0 + (4-0) = 4
        assert_eq!(actual.entries[0].rect.x, 4);
        assert_eq!(actual.entries[0].rect.width, 240);

        // Column 2: 720px, canvas_x = 4 + 240 + 4 = 248, screen_x = 248
        assert_eq!(actual.entries[1].rect.x, 248);
        assert_eq!(actual.entries[1].rect.width, 720);

        // Column 3: 960px, canvas_x = 248 + 720 + 4 = 972, screen_x = 972
        assert_eq!(actual.entries[2].rect.x, 972);
        assert_eq!(actual.entries[2].rect.width, 960);
    }

    #[test]
    fn project_visible_tiles_cover_full_monitor_width() {
        // Positive: with 0 padding, visible tiles cover exactly the col widths
        let zero_padding = Padding {
            window_gap: 0,
            up: 0,
            down: 0,
        };
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let actual = project(&layout, &test_monitor(), &zero_padding);
        let total_width: i32 = actual.entries.iter().map(|e| e.rect.width).sum();
        assert_eq!(
            total_width, 1920,
            "visible tiles must cover full monitor width with zero gap"
        );
    }

    #[test]
    fn project_parked_tiles_left_and_right_simultaneously() {
        // Positive: with 5 columns at 4/8 each, viewport showing cols 2-3,
        // cols 0-1 parked left, col 4 parked right
        // Slot model: col i at canvas_x = 4 + i * (960+4) = 4 + i*964
        // col 0: 4, col 1: 968, col 2: 1932, col 3: 2896, col 4: 3860
        // viewport at 1932: visible [1932, 3852)
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
                Column::with_row(960, Row::new(WindowId(4), 0)),
                Column::with_row(960, Row::new(WindowId(5), 0)),
            ],
            1932, // offset: start of col 2's slot
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        // Col 0 (canvas 4–964): 964 <= 1932 → parked left at -960
        assert_eq!(actual.entries[0].rect.x, -960);
        // Col 1 (canvas 968–1928): 1928 <= 1932 → parked left at -960
        assert_eq!(actual.entries[1].rect.x, -960);

        // Col 2 (canvas 1932–2892): visible
        assert!(actual.entries[2].rect.x >= 0, "col 2 should be visible");

        // Col 3 (canvas 2896–3856): visible (2896 < 3852)
        assert!(actual.entries[3].rect.x >= 0, "col 3 should be visible");

        // Col 4 (canvas 3860–4820): 3860 >= 3852 → parked right at 1920
        assert_eq!(actual.entries[4].rect.x, 1920);
    }

    #[test]
    fn project_parked_tiles_no_overlap_with_visible() {
        // Positive: parked tiles must not overlap the monitor area
        // Slot model: col 0 at canvas 4, col 1 at canvas 968, col 2 at canvas 1932
        // viewport at 968 shows col 1 and col 2
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            968, // offset past col 0
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        // Col 0 is parked left — must not overlap monitor area
        let parked_entry = &actual.entries[0];
        let parked_right = parked_entry.rect.x + parked_entry.rect.width;
        assert!(
            parked_right <= 0,
            "parked left tile overlaps visible: right edge = {parked_right}"
        );
    }

    #[test]
    fn project_nonzero_viewport_offset() {
        // Positive: viewport offset shifts visible columns right
        // Slot model: col 0 at canvas_x=4, col 1 at canvas_x=968, col 2 at canvas_x=1932
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            500, // non-zero offset
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        // Col 0: canvas_x=4, screen_x = 0 + (4 - 500) = -496
        assert_eq!(actual.entries[0].rect.x, -496);

        // Col 1: canvas_x=968, screen_x = 0 + (968 - 500) = 468
        assert_eq!(actual.entries[1].rect.x, 468);

        // Col 2: canvas_x=1932, screen_x = 0 + (1932 - 500) = 1432
        assert_eq!(actual.entries[2].rect.x, 1432);
    }

    #[test]
    fn project_two_rows_equal_height() {
        // Two rows pre-distributed equally by mutation layer.
        // User formula: (1080 - (2+1)*4) / 2 = 1068 / 2 = 534 each.
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 534), Row::new(WindowId(2), 534)],
            )],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        assert_eq!(actual.entries.len(), 2);
        // row.height consumed verbatim — no inset applied by projection.
        assert_eq!(actual.entries[0].rect.height, 534);
        assert_eq!(actual.entries[1].rect.height, 534);
        // row 0 y = 0 + 0 + 4 = 4
        assert_eq!(actual.entries[0].rect.y, 4);
        // row 1 y = 4 + 534 + 4 = 542
        assert_eq!(actual.entries[1].rect.y, 542);
    }

    #[test]
    fn project_single_column_narrow_width() {
        // Positive: narrow 240px column
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(240, Row::new(WindowId(1), 0))], 0);
        let actual = project(&layout, &test_monitor(), &test_padding());

        assert_eq!(actual.entries.len(), 1);
        // Slot model: full col_width, no horizontal inset
        assert_eq!(actual.entries[0].rect.width, 240);
    }

    #[test]
    fn project_empty_layout_yields_empty_actual() {
        // Positive: empty virtual layout → no entries
        let layout = VirtualLayout::new();
        let actual = project(&layout, &test_monitor(), &test_padding());
        assert!(actual.entries.is_empty());
    }

    #[test]
    fn canvas_width_single_column() {
        // Positive: single column → window_gap + col_width + window_gap
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(1920, Row::new(WindowId(1), 0))], 0);
        // 4 + (1920 + 4) = 1928
        assert_eq!(canvas_width(&layout, 4), 1928);
    }

    #[test]
    fn canvas_width_five_columns() {
        // Positive: 5 × 960px columns → 4 + 5*(960+4) = 4 + 4820 = 4824
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
                Column::with_row(960, Row::new(WindowId(4), 0)),
                Column::with_row(960, Row::new(WindowId(5), 0)),
            ],
            0,
        );
        assert_eq!(canvas_width(&layout, 4), 4824);
    }

    #[test]
    fn project_with_up_and_down_padding() {
        // Positive: up/down padding creates screen-level margins
        let padding = Padding {
            window_gap: 4,
            up: 10,
            down: 40,
        };
        // Row height pre-distributed: available = 1080 - 10 - 40 = 1030,
        // (1030 - 2*4) / 1 = 1022.
        let layout = VirtualLayout::with_columns(
            vec![Column::with_row(1920, Row::new(WindowId(1), 1022))],
            0,
        );
        let actual = project(&layout, &test_monitor(), &padding);

        assert_eq!(actual.entries.len(), 1);
        let entry = &actual.entries[0];
        // y = monitor_y + padding.up + window_gap = 0 + 10 + 4 = 14
        // height = row.height consumed verbatim = 1022
        // Slot model: window x = window_gap = 4, width = 1920 (no horizontal inset)
        assert_eq!(entry.rect.y, 14);
        assert_eq!(entry.rect.height, 1022);
    }

    #[test]
    fn project_consumes_row_height_verbatim_uneven() {
        // Rows need NOT be equal — projection is a pure consumer of row.height.
        // This test pins the source-of-truth contract: whatever heights the
        // mutation layer hands us, we stack verbatim.
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                960,
                vec![Row::new(WindowId(1), 200), Row::new(WindowId(2), 800)],
            )],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        assert_eq!(actual.entries.len(), 2);
        // row 0: y = 0 + 0 + 4 = 4, height = 200 verbatim
        assert_eq!(actual.entries[0].rect.y, 4);
        assert_eq!(actual.entries[0].rect.height, 200);
        // row 1: y = 4 + 200 + 4 = 208, height = 800 verbatim
        assert_eq!(actual.entries[1].rect.y, 208);
        assert_eq!(actual.entries[1].rect.height, 800);
    }
}
