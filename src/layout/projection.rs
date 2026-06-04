//! Virtual layout → actual layout projection.
//!
//! Projects the infinite virtual canvas onto the monitor viewport,
//! computing real pixel coordinates for each visible window.
//!
//! Off-screen windows are parked exactly one column-width + padding
//! beyond the visible viewport edge, so their position is geometrically
//! deterministic rather than using a magic offset.
//!
//! ## Parking Model
//!
//! ```text
//! [Parked L] [outer] [Col 1] [inner] [Col 2] [outer] | viewport | [outer] [Col n] [inner] ... [Parked R]
//! ```
//!
//! Columns to the left of the viewport are placed at:
//! `monitor_left - column_width - outer_gap`
//!
//! Columns to the right of the viewport are placed at:
//! `monitor_right + outer_gap`

use crate::common::Rect;
use crate::layout::types::{ActualEntry, ActualLayout, Column, Gaps, MonitorInfo, VirtualLayout};

/// Project a virtual layout onto a monitor viewport.
///
/// Visible windows receive real screen coordinates. Off-screen windows
/// are parked one column-width beyond the nearest viewport edge.
#[must_use]
pub fn project(virtual_layout: &VirtualLayout, monitor: &MonitorInfo, gaps: &Gaps) -> ActualLayout {
    let monitor_rect = monitor.work_area;
    let viewport_left = virtual_layout.viewport_offset;
    let viewport_right = viewport_left + monitor_rect.width;

    let mut entries = Vec::new();
    let mut canvas_x: i32 = 0;

    for column in &virtual_layout.columns {
        let col_width = column_eighths_to_pixels(column.width_eighths, monitor_rect.width);
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
                gaps,
                &mut entries,
            );
        } else if canvas_col_right <= viewport_left {
            // Off-screen left: park just beyond the left edge
            let park_x = monitor_rect.x - col_width - gaps.outer;
            park_column_rows(column, park_x, monitor_rect, col_width, gaps, &mut entries);
        } else {
            // Off-screen right: park just beyond the right edge
            let park_x = monitor_rect.x + monitor_rect.width + gaps.outer;
            park_column_rows(column, park_x, monitor_rect, col_width, gaps, &mut entries);
        }

        canvas_x += col_width + gaps.inner;
    }

    ActualLayout { entries }
}

/// Project a visible column's rows into actual entries.
fn project_column_rows(
    column: &Column,
    col_x: i32,
    monitor_rect: Rect,
    col_width: i32,
    gaps: &Gaps,
    entries: &mut Vec<ActualEntry>,
) {
    let available_height = monitor_rect.height - 2 * gaps.outer;
    let row_count = column.rows.len();
    if row_count == 0 {
        return;
    }

    let total_row_gap = gaps.inner * (row_count as i32 - 1).max(0);
    let usable_height = (available_height - total_row_gap).max(0);

    let mut y = monitor_rect.y + gaps.outer;

    for (i, window_id) in column.rows.iter().enumerate() {
        let row_height = compute_row_height(column, i, row_count, usable_height);

        entries.push(ActualEntry {
            window_id: *window_id,
            rect: Rect {
                x: col_x + gaps.outer,
                y,
                width: (col_width - 2 * gaps.outer).max(0),
                height: row_height,
            },
        });

        y += row_height + gaps.inner;
    }
}

/// Park an off-screen column's rows at a hidden position.
fn park_column_rows(
    column: &Column,
    park_x: i32,
    monitor_rect: Rect,
    col_width: i32,
    gaps: &Gaps,
    entries: &mut Vec<ActualEntry>,
) {
    let available_height = monitor_rect.height - 2 * gaps.outer;
    let row_count = column.rows.len();
    if row_count == 0 {
        return;
    }

    let total_row_gap = gaps.inner * (row_count as i32 - 1).max(0);
    let usable_height = (available_height - total_row_gap).max(0);

    let mut y = monitor_rect.y + gaps.outer;

    for (i, window_id) in column.rows.iter().enumerate() {
        let height = compute_row_height(column, i, row_count, usable_height);

        entries.push(ActualEntry {
            window_id: *window_id,
            rect: Rect {
                x: park_x,
                y,
                width: (col_width - 2 * gaps.outer).max(0),
                height: height.max(0),
            },
        });

        y += height + gaps.inner;
    }
}

/// Compute the pixel height for a row, using ratios if available.
fn compute_row_height(column: &Column, index: usize, row_count: usize, usable_height: i32) -> i32 {
    if row_count == 0 {
        return 0;
    }
    if index < column.row_ratios.len() && column.row_ratios.len() == row_count {
        (usable_height as f32 * column.row_ratios[index]).round() as i32
    } else {
        usable_height / row_count as i32
    }
}

/// Convert width eighths to pixel width.
pub(crate) fn column_eighths_to_pixels(eighths: u8, monitor_width: i32) -> i32 {
    ((eighths as i32) * monitor_width) / 8
}

/// Compute the canvas width consumed by all columns (including inner gaps).
#[must_use]
pub fn canvas_width(layout: &VirtualLayout, monitor_width: i32, gaps: &Gaps) -> i32 {
    if layout.columns.is_empty() {
        return 0;
    }
    let total_col_width: i32 = layout
        .columns
        .iter()
        .map(|c| column_eighths_to_pixels(c.width_eighths, monitor_width))
        .sum();
    let total_gaps = gaps.inner * (layout.columns.len() as i32 - 1).max(0);
    total_col_width + total_gaps
}

/// Compute the pixel width of a single step (one column + inner gap).
/// Used by scroll operations to determine viewport offset changes.
#[must_use]
pub fn column_step_width(column: &Column, monitor_width: i32, inner_gap: i32) -> i32 {
    column_eighths_to_pixels(column.width_eighths, monitor_width) + inner_gap
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

    fn test_gaps() -> Gaps {
        Gaps {
            inner: 8,
            outer: 16,
        }
    }

    #[test]
    fn project_single_column_fills_monitor() {
        let layout = VirtualLayout::with_columns(vec![Column::new(8, WindowId(1))], 0);
        let actual = project(&layout, &test_monitor(), &test_gaps());

        assert_eq!(actual.entries.len(), 1);
        let entry = &actual.entries[0];
        assert_eq!(entry.window_id, WindowId(1));
        assert_eq!(entry.rect.x, 16); // outer gap
        assert_eq!(entry.rect.y, 16); // outer gap
        assert_eq!(entry.rect.width, 1920 - 32); // 2 * outer
        assert_eq!(entry.rect.height, 1080 - 32); // 2 * outer
    }

    #[test]
    fn project_two_equal_columns() {
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_gaps());

        assert_eq!(actual.entries.len(), 2);

        let col_width = 960; // half of 1920
        // First column
        assert_eq!(actual.entries[0].rect.x, 16);
        assert_eq!(actual.entries[0].rect.width, col_width - 32); // 2 * outer
        // Second column: 960 + 8 (inner gap) + 16 (outer gap)
        assert_eq!(actual.entries[1].rect.x, 960 + 8 + 16);
        assert_eq!(actual.entries[1].rect.width, col_width - 32);
    }

    #[test]
    fn project_column_with_two_rows() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_gaps());

        assert_eq!(actual.entries.len(), 2);
        assert_eq!(actual.entries[0].rect.x, actual.entries[1].rect.x);
        assert!(actual.entries[1].rect.y > actual.entries[0].rect.y);
    }

    #[test]
    fn off_screen_left_parked_one_column_beyond() {
        let monitor = test_monitor();
        let gaps = test_gaps();
        let col_width = 960; // half monitor

        // Two columns, viewport scrolled past the first
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            col_width + gaps.inner,
        );
        let actual = project(&layout, &monitor, &gaps);

        // First column parked at: monitor_left - col_width - outer_gap
        let expected_park_x = 0 - 960 - 16;
        assert_eq!(actual.entries[0].rect.x, expected_park_x);
        // Second column visible on screen
        assert!(actual.entries[1].rect.x >= 0);
    }

    #[test]
    fn off_screen_right_parked_one_column_beyond() {
        let monitor = test_monitor();
        let gaps = test_gaps();

        // Three columns, viewport at 0, only first two visible on 1920px
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)), // off-screen right
            ],
            0,
        );
        let actual = project(&layout, &monitor, &gaps);

        // Third column parked at: monitor_right + outer_gap
        // monitor_right = 1920, so park_x = 1920 + 16
        assert_eq!(actual.entries[2].rect.x, 1920 + 16);
    }

    #[test]
    fn canvas_width_empty() {
        let layout = VirtualLayout::new();
        assert_eq!(canvas_width(&layout, 1920, &test_gaps()), 0);
    }

    #[test]
    fn canvas_width_two_columns() {
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        // 2 * 960 + 1 * 8 = 1928
        assert_eq!(canvas_width(&layout, 1920, &test_gaps()), 1928);
    }

    // --- Integration: Projection correctness ---

    #[test]
    fn project_three_varying_width_columns_exact_pixels() {
        // Positive: 1/8 + 3/8 + 4/8 = 8/8 (full width)
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(1, WindowId(1)),
                Column::new(3, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_gaps());
        assert_eq!(actual.entries.len(), 3);

        // Column 1: 1/8 * 1920 = 240px
        assert_eq!(actual.entries[0].rect.x, 16); // outer gap
        assert_eq!(actual.entries[0].rect.width, 240 - 32); // 2*outer

        // Column 2: 3/8 * 1920 = 720px, starts at 240 + 8 (inner) + 16 (outer)
        assert_eq!(actual.entries[1].rect.x, 240 + 8 + 16);
        assert_eq!(actual.entries[1].rect.width, 720 - 32);

        // Column 3: 4/8 * 1920 = 960px, starts at 240+8 + 720 + 8 + 16
        assert_eq!(actual.entries[2].rect.x, 240 + 8 + 720 + 8 + 16);
        assert_eq!(actual.entries[2].rect.width, 960 - 32);
    }

    #[test]
    fn project_visible_tiles_cover_full_monitor_width() {
        // Positive: with 0 gaps, visible tiles sum to exactly monitor width
        let zero_gaps = Gaps { inner: 0, outer: 0 };
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        let actual = project(&layout, &test_monitor(), &zero_gaps);
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
            1936, // offset: start of col 2, viewport = [1936, 3856]
        );
        let actual = project(&layout, &test_monitor(), &test_gaps());

        // Col 0 (canvas 0–960): parked left at monitor_left - 960 - 16 = -976
        assert_eq!(actual.entries[0].rect.x, -976);
        // Col 1 (canvas 968–1928): 1928 <= 1936 → parked left
        assert_eq!(actual.entries[1].rect.x, -976); // same park spot (all left-parked share)

        // Col 2 (canvas 1936–2896): visible
        assert!(actual.entries[2].rect.x >= 0, "col 3 should be visible");

        // Col 3 (canvas 2904–3864): visible (2904 < 3856)
        assert!(actual.entries[3].rect.x >= 0, "col 4 should be visible");

        // Col 4 (canvas 3872–4832): 3872 >= 3856 → parked right at 1920 + 16 = 1936
        assert_eq!(actual.entries[4].rect.x, 1936);
    }

    #[test]
    fn project_parked_tiles_no_overlap_with_visible() {
        // Positive: parked tiles must not overlap the monitor area
        // Use 3 cols at 4/8, offset past first 2 → only col 2 visible
        // Col 0: 0–960, Col 1: 968–1928, Col 2: 1936–2896
        // Offset = 1936, viewport = [1936, 3856]
        // Col 0 and 1 off-screen left, Col 2 visible
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            1936,
        );
        let actual = project(&layout, &test_monitor(), &test_gaps());

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
        let actual = project(&layout, &test_monitor(), &test_gaps());

        // First column: canvas_x=0, visible since 0 < 1500 (500+1920) and 960 > 500
        // screen_x = 0 + (0 - 500) = -500
        assert_eq!(actual.entries[0].rect.x, -500 + 16); // -484

        // Second column: canvas_x=968, screen_x = 0 + (968 - 500) = 468
        assert_eq!(actual.entries[1].rect.x, 468 + 16); // 484

        // Third column: canvas_x=1936, screen_x = 0 + (1936 - 500) = 1436
        assert_eq!(actual.entries[2].rect.x, 1436 + 16); // 1452
    }

    #[test]
    fn project_custom_row_ratios() {
        // Positive: non-equal row ratios produce proportional heights
        let mut col = Column::new(8, WindowId(1));
        col.rows.push(WindowId(2));
        col.row_ratios = vec![0.25, 0.75];
        let layout = VirtualLayout::with_columns(vec![col], 0);
        let actual = project(&layout, &test_monitor(), &test_gaps());

        assert_eq!(actual.entries.len(), 2);
        // available = 1080 - 32 = 1048, no inner gap since 2 rows → 1 gap = 8
        // usable = 1048 - 8 = 1040
        // row 1: 1040 * 0.25 = 260
        // row 2: 1040 * 0.75 = 780
        assert_eq!(actual.entries[0].rect.height, 260);
        assert_eq!(actual.entries[1].rect.height, 780);
    }

    #[test]
    fn project_single_column_narrow_width() {
        // Positive: 1 eighth column on 1920 monitor = 240px wide
        let layout = VirtualLayout::with_columns(vec![Column::new(1, WindowId(1))], 0);
        let actual = project(&layout, &test_monitor(), &test_gaps());

        assert_eq!(actual.entries.len(), 1);
        assert_eq!(actual.entries[0].rect.width, 240 - 32); // 2*outer = 208
    }

    #[test]
    fn project_empty_layout_yields_empty_actual() {
        // Positive: empty virtual layout → no entries
        let layout = VirtualLayout::new();
        let actual = project(&layout, &test_monitor(), &test_gaps());
        assert!(actual.entries.is_empty());
    }

    #[test]
    fn column_eighths_to_pixels_boundary_cases() {
        // Positive: 1 eighth on a 1920 monitor
        assert_eq!(column_eighths_to_pixels(1, 1920), 240);
        // Positive: 8 eighths = full width
        assert_eq!(column_eighths_to_pixels(8, 1920), 1920);
        // Positive: 4 eighths = half
        assert_eq!(column_eighths_to_pixels(4, 1920), 960);
        // Edge: zero width monitor
        assert_eq!(column_eighths_to_pixels(4, 0), 0);
    }

    #[test]
    fn canvas_width_single_column_no_inner_gap() {
        // Positive: single column → no inner gaps
        let layout = VirtualLayout::with_columns(vec![Column::new(8, WindowId(1))], 0);
        assert_eq!(canvas_width(&layout, 1920, &test_gaps()), 1920);
    }

    #[test]
    fn canvas_width_five_columns() {
        // Positive: 5 × 4/8 columns = 5*960 + 4*8 = 4832
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
        assert_eq!(canvas_width(&layout, 1920, &test_gaps()), 5 * 960 + 4 * 8);
    }
}
