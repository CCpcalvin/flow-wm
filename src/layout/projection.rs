//! Virtual layout → actual layout projection (camera to screen).
//!
//! This module implements the core **camera-to-screen** projection: it takes the
//! infinite virtual canvas ([`VirtualLayout`]) and computes real pixel coordinates
//! for every window ([`ActualLayout`]) that Windows OS must render.
//!
//! # How it works
//!
//! 1. **Camera shift**: Each column's virtual x-position is offset by subtracting
//!    `viewport_offset` (the camera position). Columns whose shifted range overlaps
//!    the monitor rectangle are **visible** and receive on-screen coordinates.
//!
//! 2. **Parking**: Columns fully outside the viewport are **parked** — placed at a
//!    deterministic position exactly one column-width beyond the nearest viewport edge.
//!    There are two parking zones:
//!    - **Left parking** (`monitor_left - col_width`): for columns that scrolled off
//!      the left side of the viewport.
//!    - **Right parking** (`monitor_right + col_width`): for columns that scrolled off
//!      the right side.
//!
//!    Parking is necessary because Windows OS does not gracefully handle windows at
//!    extreme off-screen coordinates. By parking just beyond the edge, scroll-in/out
//!    animations are short-distance and smooth.
//!
//! # Container Model
//!
//! Columns are **packed** with no inter-column gap. Visual spacing
//! between adjacent windows comes entirely from `padding.window_gap`
//! (each window is inset by `padding.window_gap` on all sides within its cell).
//!
//! ```text
//! [Col 1] [Col 2] [Col 3] | viewport | [Col n] [Col n+1] ...
//! ```
//!
//! Screen-level top margin = `padding.up`, bottom margin = `padding.down`.
//!
//! # Padding: Outside the Window Concept
//!
//! Padding is applied **here**, during projection. The [`ActualEntry`] rects
//! produced are the **final HWND rects** — they can be passed directly to
//! `SetWindowPos` without any further adjustment. This keeps the padding logic
//! in one place and prevents every consumer of window rects from needing to
//! know about padding.

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
#[must_use]
pub fn project(
    virtual_layout: &VirtualLayout,
    monitor: &MonitorInfo,
    column_width: u32,
    padding: &Padding,
) -> ActualLayout {
    let monitor_rect = monitor.work_area;
    let viewport_left = virtual_layout.viewport_offset;
    let viewport_right = viewport_left + monitor_rect.width;

    let mut entries = Vec::new();
    let mut canvas_x: i32 = 0;

    for column in &virtual_layout.columns {
        let col_width = column_eighths_to_pixels(column.width_eighths, column_width);
        let canvas_col_left = canvas_x;
        let canvas_col_right = canvas_x + col_width;

        let visible = canvas_col_right > viewport_left && canvas_col_left < viewport_right;

        if visible {
            let screen_x = monitor_rect.x + (canvas_col_left - viewport_left);
            project_column_rows(
                column,
                screen_x,
                monitor_rect,
                col_width,
                padding,
                &mut entries,
            );
        } else if canvas_col_right <= viewport_left {
            // Off-screen left: park one column-width beyond the left edge
            let park_x = monitor_rect.x - col_width;
            park_column_rows(
                column,
                park_x,
                monitor_rect,
                col_width,
                padding,
                &mut entries,
            );
        } else {
            // Off-screen right: park one column-width beyond the right edge
            let park_x = monitor_rect.x + monitor_rect.width;
            park_column_rows(
                column,
                park_x,
                monitor_rect,
                col_width,
                padding,
                &mut entries,
            );
        }

        canvas_x += col_width;
    }

    ActualLayout { entries }
}

/// Project a visible column's rows into actual entries.
///
/// Rows are packed (no inter-row gap). Each window is inset by
/// `padding.window_gap` within its allocated row cell. The resulting
/// [`ActualEntry::rect`](crate::layout::ActualEntry::rect) is the final HWND rect.
fn project_column_rows(
    column: &Column,
    col_x: i32,
    monitor_rect: Rect,
    col_width: i32,
    padding: &Padding,
    entries: &mut Vec<ActualEntry>,
) {
    let available_height = monitor_rect.height - padding.up - padding.down;
    let row_count = column.rows.len();
    if row_count == 0 {
        return;
    }

    let usable_height = available_height.max(0);
    let mut y = monitor_rect.y + padding.up;

    for (i, window_id) in column.rows.iter().enumerate() {
        let row_height = compute_row_height(i, row_count, usable_height);

        entries.push(ActualEntry {
            window_id: *window_id,
            rect: Rect {
                x: col_x + padding.window_gap,
                y: y + padding.window_gap,
                width: (col_width - 2 * padding.window_gap).max(0),
                height: (row_height - 2 * padding.window_gap).max(0),
            },
        });

        y += row_height;
    }
}

/// Park an off-screen column's rows at a hidden but deterministic position.
///
/// Parked windows use the same padding as visible windows, so when a window
/// transitions from parked → visible (or vice versa), the animation is smooth
/// and the window dimensions remain consistent.
///
/// **Left parking**: `monitor_left - col_width` — for columns scrolled past the left edge.
/// **Right parking**: `monitor_right` — for columns scrolled past the right edge. Parked positions
/// are deterministic: one column-width beyond the nearest viewport edge.
fn park_column_rows(
    column: &Column,
    park_x: i32,
    monitor_rect: Rect,
    col_width: i32,
    padding: &Padding,
    entries: &mut Vec<ActualEntry>,
) {
    let available_height = monitor_rect.height - padding.up - padding.down;
    let row_count = column.rows.len();
    if row_count == 0 {
        return;
    }

    let usable_height = available_height.max(0);
    let mut y = monitor_rect.y + padding.up;

    for (i, window_id) in column.rows.iter().enumerate() {
        let height = compute_row_height(i, row_count, usable_height);

        entries.push(ActualEntry {
            window_id: *window_id,
            rect: Rect {
                x: park_x + padding.window_gap,
                y: y + padding.window_gap,
                width: (col_width - 2 * padding.window_gap).max(0),
                height: (height - 2 * padding.window_gap).max(0),
            },
        });

        y += height;
    }
}

/// Compute the pixel height for a row (always equal division).
///
/// All rows within a column have equal height. If custom row ratios are
/// needed in the future, this is the function to modify.
fn compute_row_height(_index: usize, row_count: usize, usable_height: i32) -> i32 {
    if row_count == 0 {
        return 0;
    }
    usable_height / row_count as i32
}

/// Convert width eighths to pixel width based on `column_width`.
///
/// A column with `width_eighths = 4` equals `column_width` pixels.
/// The formula is: `(eighths * column_width) / 4`.
pub(crate) fn column_eighths_to_pixels(eighths: u8, column_width: u32) -> i32 {
    ((eighths as i32) * (column_width as i32)) / 4
}

/// Compute the canvas width consumed by all columns (packed, no inter-column gap).
#[must_use]
pub fn canvas_width(layout: &VirtualLayout, column_width: u32) -> i32 {
    if layout.columns.is_empty() {
        return 0;
    }
    layout
        .columns
        .iter()
        .map(|c| column_eighths_to_pixels(c.width_eighths, column_width))
        .sum()
}

/// Compute the pixel width of a single step (one column width).
/// Used by scroll operations to determine viewport offset changes.
#[must_use]
pub fn column_step_width(column: &Column, column_width: u32) -> i32 {
    column_eighths_to_pixels(column.width_eighths, column_width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::WindowId;

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

    const TEST_COLUMN_WIDTH: u32 = 960;

    fn test_padding() -> Padding {
        Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        }
    }

    #[test]
    fn project_single_column_fills_monitor() {
        let layout = VirtualLayout::with_columns(vec![Column::new(8, WindowId(1))], 0);
        let actual = project(&layout, &test_monitor(), 960, &test_padding());

        assert_eq!(actual.entries.len(), 1);
        let entry = &actual.entries[0];
        assert_eq!(entry.window_id, WindowId(1));
        // width_eighths=8 with column_width=960 → 1920px column
        // window x = 0 + 4 = 4
        // window width = 1920 - 2*4 = 1912
        assert_eq!(entry.rect.x, 4);
        assert_eq!(entry.rect.y, 4);
        assert_eq!(entry.rect.width, 1912);
        assert_eq!(entry.rect.height, 1072);
    }

    #[test]
    fn project_two_equal_columns() {
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        let actual = project(&layout, &test_monitor(), TEST_COLUMN_WIDTH, &test_padding());

        assert_eq!(actual.entries.len(), 2);

        let col_width = 960; // column_width for 4/8
        // First column at x=0
        assert_eq!(actual.entries[0].rect.x, 4); // padding.window_gap
        assert_eq!(actual.entries[0].rect.width, col_width - 8); // 2 * padding.window_gap
        // Second column packed at x=960
        assert_eq!(actual.entries[1].rect.x, col_width + 4);
        assert_eq!(actual.entries[1].rect.width, col_width - 8);
    }

    #[test]
    fn project_column_with_two_rows() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        let actual = project(&layout, &test_monitor(), TEST_COLUMN_WIDTH, &test_padding());

        assert_eq!(actual.entries.len(), 2);
        assert_eq!(actual.entries[0].rect.x, actual.entries[1].rect.x);
        assert!(actual.entries[1].rect.y > actual.entries[0].rect.y);
    }

    #[test]
    fn off_screen_left_parked_one_column_beyond() {
        let monitor = test_monitor();
        let padding = test_padding();

        // Two columns, viewport scrolled past the first
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            960, // offset = one column width
        );
        let actual = project(&layout, &monitor, TEST_COLUMN_WIDTH, &padding);

        // First column parked at: monitor_left - col_width + padding.window_gap = -960 + 4 = -956
        assert_eq!(actual.entries[0].rect.x, -960 + 4);
        // Second column visible on screen
        assert!(actual.entries[1].rect.x >= 0);
    }

    #[test]
    fn off_screen_right_parked_one_column_beyond() {
        let monitor = test_monitor();
        let padding = test_padding();

        // Three columns, viewport at 0, only first two visible on 1920px
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)), // off-screen right
            ],
            0,
        );
        let actual = project(&layout, &monitor, TEST_COLUMN_WIDTH, &padding);

        // Third column parked at: monitor_right + padding.window_gap = 1920 + 4
        assert_eq!(actual.entries[2].rect.x, 1920 + 4);
    }

    #[test]
    fn canvas_width_empty() {
        let layout = VirtualLayout::new();
        assert_eq!(canvas_width(&layout, TEST_COLUMN_WIDTH), 0);
    }

    #[test]
    fn canvas_width_two_columns() {
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        // 2 * 960 = 1920 (packed, no inter-column gap)
        assert_eq!(canvas_width(&layout, TEST_COLUMN_WIDTH), 1920);
    }

    // --- Integration: Projection correctness ---

    #[test]
    fn project_three_varying_width_columns_exact_pixels() {
        // Positive: 1/8 + 3/8 + 4/8 columns
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(1, WindowId(1)),
                Column::new(3, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            0,
        );
        let actual = project(&layout, &test_monitor(), TEST_COLUMN_WIDTH, &test_padding());
        assert_eq!(actual.entries.len(), 3);

        // Column 1: 1/8 * 960/4 = 1 * 240 = 240px
        assert_eq!(actual.entries[0].rect.x, 4); // padding.window_gap
        assert_eq!(actual.entries[0].rect.width, 240 - 8); // 2*padding.window_gap

        // Column 2: 3/8 * 960/4 = 3 * 240 = 720px, packed at x=240
        assert_eq!(actual.entries[1].rect.x, 240 + 4);
        assert_eq!(actual.entries[1].rect.width, 720 - 8);

        // Column 3: 4/8 * 960/4 = 4 * 240 = 960px, packed at x=240+720=960
        assert_eq!(actual.entries[2].rect.x, 960 + 4);
        assert_eq!(actual.entries[2].rect.width, 960 - 8);
    }

    #[test]
    fn project_visible_tiles_cover_full_monitor_width() {
        // Positive: with 0 padding, visible tiles sum to exactly monitor width
        let zero_padding = Padding {
            window_gap: 0,
            up: 0,
            down: 0,
        };
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        let actual = project(&layout, &test_monitor(), TEST_COLUMN_WIDTH, &zero_padding);
        let total_width: i32 = actual.entries.iter().map(|e| e.rect.width).sum();
        assert_eq!(
            total_width, 1920,
            "visible tiles must cover full monitor width"
        );
    }

    #[test]
    fn project_parked_tiles_left_and_right_simultaneously() {
        // Positive: with 5 columns at 4/8 each, viewport showing cols 2-3,
        // cols 0-1 parked left, col 4 parked right
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
                Column::new(4, WindowId(4)),
                Column::new(4, WindowId(5)),
            ],
            1920, // offset: start of col 2 (2*960), viewport = [1920, 3840]
        );
        let actual = project(&layout, &test_monitor(), TEST_COLUMN_WIDTH, &test_padding());

        // Col 0 (canvas 0–960): parked left at -960 + 4 (padding.window_gap)
        assert_eq!(actual.entries[0].rect.x, -960 + 4);
        // Col 1 (canvas 960–1920): 1920 <= 1920 → off-screen left
        assert_eq!(actual.entries[1].rect.x, -960 + 4);

        // Col 2 (canvas 1920–2880): visible
        assert!(actual.entries[2].rect.x >= 0, "col 2 should be visible");

        // Col 3 (canvas 2880–3840): visible (2880 < 3840)
        assert!(actual.entries[3].rect.x >= 0, "col 3 should be visible");

        // Col 4 (canvas 3840–4800): 3840 >= 3840 → parked right at 1920 + padding.window_gap
        assert_eq!(actual.entries[4].rect.x, 1920 + 4);
    }

    #[test]
    fn project_parked_tiles_no_overlap_with_visible() {
        // Positive: parked tiles must not overlap the monitor area
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            1920, // offset past first 2 columns
        );
        let actual = project(&layout, &test_monitor(), TEST_COLUMN_WIDTH, &test_padding());

        // All parked-left entries must end before monitor left (0)
        for (i, entry) in actual.entries.iter().enumerate().take(2) {
            let parked_left_right = entry.rect.x + entry.rect.width;
            assert!(
                parked_left_right <= 0,
                "parked left tile {i} overlaps visible: right edge = {parked_left_right}"
            );
        }
    }

    #[test]
    fn project_nonzero_viewport_offset() {
        // Positive: viewport offset shifts visible columns right
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            500, // non-zero offset
        );
        let actual = project(&layout, &test_monitor(), TEST_COLUMN_WIDTH, &test_padding());

        // First column: canvas_x=0, visible since 960 > 500
        // screen_x = 0 + (0 - 500) = -500
        assert_eq!(actual.entries[0].rect.x, -500 + 4); // + padding.window_gap

        // Second column: canvas_x=960, screen_x = 0 + (960 - 500) = 460
        assert_eq!(actual.entries[1].rect.x, 460 + 4);

        // Third column: canvas_x=1920, screen_x = 0 + (1920 - 500) = 1420
        assert_eq!(actual.entries[2].rect.x, 1420 + 4);
    }

    #[test]
    fn project_two_rows_equal_height() {
        // Positive: two windows in one column are always equal height
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        let actual = project(&layout, &test_monitor(), TEST_COLUMN_WIDTH, &test_padding());

        assert_eq!(actual.entries.len(), 2);
        // Each row = 1080 / 2 = 540, window height = 540 - 2*4 = 532
        assert_eq!(actual.entries[0].rect.height, 532);
        assert_eq!(actual.entries[1].rect.height, 532);
    }

    #[test]
    fn project_single_column_narrow_width() {
        // Positive: 1 eighth column with column_width=960 → 240px wide
        let layout = VirtualLayout::with_columns(vec![Column::new(1, WindowId(1))], 0);
        let actual = project(&layout, &test_monitor(), TEST_COLUMN_WIDTH, &test_padding());

        assert_eq!(actual.entries.len(), 1);
        assert_eq!(actual.entries[0].rect.width, 240 - 8); // 2*padding.window_gap = 232
    }

    #[test]
    fn project_empty_layout_yields_empty_actual() {
        // Positive: empty virtual layout → no entries
        let layout = VirtualLayout::new();
        let actual = project(&layout, &test_monitor(), TEST_COLUMN_WIDTH, &test_padding());
        assert!(actual.entries.is_empty());
    }

    #[test]
    fn column_eighths_to_pixels_boundary_cases() {
        // Positive: 1 eighth → 240px (with column_width=960)
        assert_eq!(column_eighths_to_pixels(1, 960), 240);
        // Positive: 4 eighths = column_width
        assert_eq!(column_eighths_to_pixels(4, 960), 960);
        // Positive: 8 eighths = 2 * column_width
        assert_eq!(column_eighths_to_pixels(8, 960), 1920);
        // Edge: zero column_width
        assert_eq!(column_eighths_to_pixels(4, 0), 0);
    }

    #[test]
    fn canvas_width_single_column() {
        // Positive: single column → no inter-column gaps
        let layout = VirtualLayout::with_columns(vec![Column::new(8, WindowId(1))], 0);
        assert_eq!(canvas_width(&layout, TEST_COLUMN_WIDTH), 1920);
    }

    #[test]
    fn canvas_width_five_columns() {
        // Positive: 5 × 4/8 columns = 5*960 = 4800 (packed)
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
                Column::new(4, WindowId(4)),
                Column::new(4, WindowId(5)),
            ],
            0,
        );
        assert_eq!(canvas_width(&layout, TEST_COLUMN_WIDTH), 5 * 960);
    }

    #[test]
    fn project_with_up_and_down_padding() {
        // Positive: up/down padding creates screen-level margins
        let padding = Padding {
            window_gap: 4,
            up: 10,
            down: 40,
        };
        let layout = VirtualLayout::with_columns(vec![Column::new(8, WindowId(1))], 0);
        let actual = project(&layout, &test_monitor(), TEST_COLUMN_WIDTH, &padding);

        assert_eq!(actual.entries.len(), 1);
        let entry = &actual.entries[0];
        // Column height = 1080, available = 1080 - 10 - 40 = 1030
        // Row height = 1030, window y = 0 + 10 + 4 = 14
        // Window height = 1030 - 2*4 = 1022
        assert_eq!(entry.rect.y, 14);
        assert_eq!(entry.rect.height, 1022);
    }
}
