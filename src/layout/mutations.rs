//! Layout mutation operations — all pure functions.
//!
//! Every mutation takes a `&VirtualLayout` and returns a **new** `VirtualLayout`.
//! The layout is never mutated in place. This functional approach makes mutations
//! easy to test, compose, and reason about — there is no mutable state to get
//! out of sync.
//!
//! # Container Model
//!
//! ```text
//! VirtualLayout (horizontal container)
//! ├── Column 0 (vertical container) — rows stacked top-to-bottom
//! ├── Column 1
//! └── Column 2
//! ```
//!
//! - **Horizontal operations** (Left/Right): swap columns, resize `width_eighths`, scroll
//! - **Vertical operations** (Up/Down): swap rows within a column, focus between rows
//!
//! # Size Philosophy
//!
//! All size parameters come from [`MutationConfig`], which is derived from
//! [`StmConfig`](crate::config::StmConfig). The mutation layer never hardcodes
//! pixel values — it delegates to [`super::projection`] for all pixel math.
//! for all pixel math.

use crate::common::{Direction, WindowId};
use crate::layout::projection::{column_eighths_to_pixels, column_step_width};
use crate::layout::types::{Column, Padding, VirtualLayout};

/// Parameters that configure how mutations behave.
///
/// Extracted from [`StmConfig`](crate::config::StmConfig) by the daemon.
/// The layout engine receives this (not the full config) to stay decoupled
/// from config parsing details.
#[derive(Debug, Clone, Copy)]
pub struct MutationConfig {
    /// Monitor pixel width (used for visibility checks).
    pub monitor_width: i32,
    /// Default column width in pixels for new columns.
    pub column_width: u32,
    /// Default column width in eighths (1–8) for new columns.
    pub default_column_width_eighths: u8,
    /// Padding settings.
    pub padding: Padding,
}

// ---------------------------------------------------------------------------
// Scroll mutations
// ---------------------------------------------------------------------------

/// Scroll the viewport left by one column step.
///
/// Returns `None` if already at the leftmost position.
#[must_use]
pub fn scroll_left(layout: &VirtualLayout, config: &MutationConfig) -> Option<VirtualLayout> {
    if layout.viewport_offset <= 0 {
        return None;
    }
    // Find the first visible column and scroll by its step width
    let step = first_visible_step(layout, config)?;
    let new_offset = (layout.viewport_offset - step).max(0);
    Some(VirtualLayout {
        viewport_offset: new_offset,
        ..layout.clone()
    })
}

/// Scroll the viewport right by one column step.
///
/// Returns `None` if already at the rightmost position.
#[must_use]
pub fn scroll_right(layout: &VirtualLayout, config: &MutationConfig) -> Option<VirtualLayout> {
    let step = first_visible_step(layout, config)?;
    let total_canvas = total_column_span(layout, config);
    let max_offset = (total_canvas - config.monitor_width).max(0);
    let new_offset = layout.viewport_offset + step;
    if new_offset > max_offset {
        return None;
    }
    Some(VirtualLayout {
        viewport_offset: new_offset,
        ..layout.clone()
    })
}

// ---------------------------------------------------------------------------
// Focus mutations
// ---------------------------------------------------------------------------

/// Result of a focus change — the newly focused window, and optionally
/// a viewport scroll if the target column was off-screen.
///
/// The [`LayoutEngine`](crate::layout::LayoutEngine) reads `focused` to update
/// its internal focus tracker, and applies `new_layout` through the mutation pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct FocusResult {
    /// The newly focused [`WindowId`].
    pub focused: WindowId,
    /// Updated layout (viewport may have scrolled).
    pub new_layout: VirtualLayout,
}

/// Move focus in the given direction.
///
/// If focus would leave the visible viewport, the viewport scrolls
/// to keep the newly focused window visible.
///
/// Returns `None` if there is no window to focus in that direction.
#[must_use]
pub fn focus(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
    config: &MutationConfig,
) -> Option<FocusResult> {
    let (col, row) = layout.find_window(focused)?;

    match direction {
        Direction::Left => focus_horizontal(layout, col, row, col.saturating_sub(1), config),
        Direction::Right => {
            let target_col = (col + 1).min(layout.columns.len().saturating_sub(1));
            focus_horizontal(layout, col, row, target_col, config)
        }
        Direction::Up => {
            if row == 0 {
                return None;
            }
            let target = layout.columns[col].rows[row - 1];
            Some(FocusResult {
                focused: target,
                new_layout: layout.clone(),
            })
        }
        Direction::Down => {
            let col_ref = &layout.columns[col];
            if row + 1 >= col_ref.rows.len() {
                return None;
            }
            let target = col_ref.rows[row + 1];
            Some(FocusResult {
                focused: target,
                new_layout: layout.clone(),
            })
        }
    }
}

/// Focus a window in a target column, scrolling the viewport if needed.
fn focus_horizontal(
    layout: &VirtualLayout,
    current_col: usize,
    _current_row: usize,
    target_col: usize,
    config: &MutationConfig,
) -> Option<FocusResult> {
    if target_col == current_col {
        return None;
    }
    let col_ref = layout.columns.get(target_col)?;
    let target_window = col_ref.rows.first()?;

    // Check if target column is visible; scroll if needed
    let new_layout = ensure_column_visible(layout, target_col, config);
    Some(FocusResult {
        focused: *target_window,
        new_layout,
    })
}

/// Adjust viewport offset so the given column is visible.
fn ensure_column_visible(
    layout: &VirtualLayout,
    col_idx: usize,
    config: &MutationConfig,
) -> VirtualLayout {
    let mut canvas_x: i32 = 0;
    for (i, col) in layout.columns.iter().enumerate() {
        let col_px = column_eighths_to_pixels(col.width_eighths, config.column_width);
        if i == col_idx {
            let col_left = canvas_x;
            let col_right = canvas_x + col_px;
            let vp_left = layout.viewport_offset;
            let vp_right = vp_left + config.monitor_width;

            if col_left < vp_left {
                // Column is off-screen left — scroll left
                return VirtualLayout {
                    viewport_offset: col_left,
                    ..layout.clone()
                };
            }
            if col_right > vp_right {
                // Column is off-screen right — scroll right
                return VirtualLayout {
                    viewport_offset: col_right - config.monitor_width,
                    ..layout.clone()
                };
            }
            // Already visible
            return layout.clone();
        }
        canvas_x += col_px;
    }
    layout.clone()
}

// ---------------------------------------------------------------------------
// Swap mutations
// ---------------------------------------------------------------------------

/// Swap the focused window with an adjacent sibling within its column (vertical swap),
/// or swap the focused window's column with an adjacent column (horizontal swap).
///
/// Returns `None` if there is no adjacent element in that direction.
#[must_use]
pub fn swap(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
) -> Option<VirtualLayout> {
    let (col, row) = layout.find_window(focused)?;
    match direction {
        Direction::Up => swap_rows(layout, col, row, row.saturating_sub(1)),
        Direction::Down => {
            let max_row = layout.columns[col].rows.len().saturating_sub(1);
            if row >= max_row {
                return None;
            }
            swap_rows(layout, col, row, row + 1)
        }
        Direction::Left => {
            if col == 0 {
                return None;
            }
            swap_columns(layout, col, col - 1)
        }
        Direction::Right => {
            let max_col = layout.columns.len().saturating_sub(1);
            if col >= max_col {
                return None;
            }
            swap_columns(layout, col, col + 1)
        }
    }
}

/// Swap two rows within a column (vertical container reorder).
fn swap_rows(
    layout: &VirtualLayout,
    col_idx: usize,
    row_a: usize,
    row_b: usize,
) -> Option<VirtualLayout> {
    if row_a == row_b {
        return None;
    }
    let mut new_layout = layout.clone();
    let col = &mut new_layout.columns[col_idx];
    col.rows.swap(row_a, row_b);
    Some(new_layout)
}

/// Swap two columns (horizontal container reorder).
fn swap_columns(layout: &VirtualLayout, col_a: usize, col_b: usize) -> Option<VirtualLayout> {
    let mut new_layout = layout.clone();
    new_layout.columns.swap(col_a, col_b);
    Some(new_layout)
}

/// Swap the focused window's column with the first off-screen column
/// in the given direction, then scroll so the swapped-in column is visible.
#[must_use]
pub fn swap_with_offscreen(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let (focused_col, _) = layout.find_window(focused)?;
    let offscreen_col = find_first_offscreen_column(layout, direction, config)?;

    let mut new_layout = layout.clone();
    new_layout.columns.swap(focused_col, offscreen_col);

    // Scroll so the focused column (now containing swapped windows) is visible
    let new_layout = ensure_column_visible(&new_layout, focused_col, config);
    Some(new_layout)
}

/// Find the first column that is fully off-screen in the given direction.
fn find_first_offscreen_column(
    layout: &VirtualLayout,
    direction: Direction,
    config: &MutationConfig,
) -> Option<usize> {
    let vp_left = layout.viewport_offset;
    let vp_right = vp_left + config.monitor_width;

    let mut canvas_x: i32 = 0;
    for (i, col) in layout.columns.iter().enumerate() {
        let col_px = column_eighths_to_pixels(col.width_eighths, config.column_width);
        let col_left = canvas_x;
        let col_right = canvas_x + col_px;

        let offscreen = match direction {
            Direction::Left => col_right <= vp_left,
            Direction::Right => col_left >= vp_right,
            _ => false,
        };

        if offscreen {
            return Some(i);
        }
        canvas_x += col_px;
    }
    None
}

// ---------------------------------------------------------------------------
// Resize mutations (horizontal container = adjust width_eighths)
// ---------------------------------------------------------------------------

/// Expand the focused column by 1 eighth. The adjacent column in the
/// direction of growth shrinks by 1 to compensate.
///
/// Returns `None` if the column is already at max width or there is
/// no neighbor to shrink.
#[must_use]
pub fn expand_column(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
) -> Option<VirtualLayout> {
    resize_column(layout, focused, direction, 1)
}

/// Shrink the focused column by 1 eighth. The adjacent column in the
/// direction of shrink grows by 1 to compensate.
#[must_use]
pub fn shrink_column(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
) -> Option<VirtualLayout> {
    resize_column(layout, focused, direction, -1)
}

/// Set the focused column width explicitly.
///
/// The adjacent column compensates to keep total width constant.
#[must_use]
pub fn set_column_width(
    layout: &VirtualLayout,
    focused: WindowId,
    eighths: u8,
    _config: &MutationConfig,
) -> Option<VirtualLayout> {
    let (col, _) = layout.find_window(focused)?;
    let current = layout.columns[col].width_eighths;
    let delta = eighths as i8 - current as i8;
    if delta == 0 {
        return Some(layout.clone());
    }
    let direction = if delta > 0 {
        // Prefer shrinking the right neighbor
        Direction::Right
    } else {
        Direction::Left
    };
    // Apply delta iteratively (simpler than computing compound compensation)
    let mut result = layout.clone();
    for _ in 0..delta.abs() {
        result = resize_column(&result, focused, direction, delta.signum())?;
    }
    Some(result)
}

fn resize_column(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
    delta: i8,
) -> Option<VirtualLayout> {
    let (col, _) = layout.find_window(focused)?;
    let mut new_layout = layout.clone();

    let current_width = new_layout.columns[col].width_eighths;
    let new_width = (current_width as i8 + delta) as u8;

    // Validate bounds
    if !(1..=8).contains(&new_width) {
        return None;
    }

    // Find neighbor to compensate
    let neighbor = match direction {
        Direction::Left => col.checked_sub(1),
        Direction::Right => {
            if col + 1 < new_layout.columns.len() {
                Some(col + 1)
            } else {
                None
            }
        }
        _ => None,
    };

    let neighbor = neighbor?;

    let neighbor_width = new_layout.columns[neighbor].width_eighths;
    let compensated = (neighbor_width as i8 - delta) as u8;
    if !(1..=8).contains(&compensated) {
        return None;
    }

    new_layout.columns[col].width_eighths = new_width;
    new_layout.columns[neighbor].width_eighths = compensated;

    Some(new_layout)
}

// ---------------------------------------------------------------------------
// Merge mutations (horizontal container join)
// ---------------------------------------------------------------------------

/// Merge the focused column with its left neighbor.
///
/// All rows from the left neighbor are prepended to the focused column.
/// The left neighbor is then removed.
#[must_use]
pub fn merge_column_left(layout: &VirtualLayout, focused: WindowId) -> Option<VirtualLayout> {
    let (col, _) = layout.find_window(focused)?;
    if col == 0 {
        return None;
    }
    merge_columns(layout, col - 1, col)
}

/// Merge the focused column with its right neighbor.
///
/// All rows from the right neighbor are appended to the focused column.
/// The right neighbor is then removed.
#[must_use]
pub fn merge_column_right(layout: &VirtualLayout, focused: WindowId) -> Option<VirtualLayout> {
    let (col, _) = layout.find_window(focused)?;
    if col + 1 >= layout.columns.len() {
        return None;
    }
    merge_columns(layout, col, col + 1)
}

/// Merge `absorbed` column's rows into `absorber` column, then remove `absorbed`.
fn merge_columns(
    layout: &VirtualLayout,
    absorber: usize,
    absorbed: usize,
) -> Option<VirtualLayout> {
    let mut new_layout = layout.clone();
    let absorbed_rows = new_layout.columns[absorbed].rows.clone();
    let absorber_col = &mut new_layout.columns[absorber];
    absorber_col.rows.extend(absorbed_rows);

    new_layout.columns.remove(absorbed);
    Some(new_layout)
}

// ---------------------------------------------------------------------------
// Monocle toggle
// ---------------------------------------------------------------------------

/// Toggle monocle mode — expand the focused window's column to full width.
///
/// Returns the new layout and whether monocle is now active.
#[must_use]
pub fn toggle_monocle(
    layout: &VirtualLayout,
    focused: WindowId,
    saved_width: Option<u8>,
) -> Option<(VirtualLayout, Option<u8>)> {
    let (col, _) = layout.find_window(focused)?;
    let current_width = layout.columns[col].width_eighths;

    if current_width == 8 {
        // Already monocle — restore previous width
        let restored = saved_width.unwrap_or(4);
        let mut new_layout = layout.clone();
        new_layout.columns[col].width_eighths = restored;
        Some((new_layout, None))
    } else {
        // Enter monocle
        let mut new_layout = layout.clone();
        new_layout.columns[col].width_eighths = 8;
        Some((new_layout, Some(current_width)))
    }
}

// ---------------------------------------------------------------------------
// Window add / remove
// ---------------------------------------------------------------------------

/// Add a window to the layout as a new column appended to the right.
#[must_use]
pub fn add_window(
    layout: &VirtualLayout,
    window: WindowId,
    config: &MutationConfig,
) -> VirtualLayout {
    let mut new_layout = layout.clone();
    new_layout
        .columns
        .push(Column::new(config.default_column_width_eighths, window));
    new_layout
}

/// Add a window to an existing column as a new row (vertical container append).
#[must_use]
pub fn add_window_to_column(
    layout: &VirtualLayout,
    col_idx: usize,
    window: WindowId,
) -> VirtualLayout {
    let mut new_layout = layout.clone();
    if let Some(col) = new_layout.columns.get_mut(col_idx) {
        col.rows.push(window);
    }
    new_layout
}

/// Remove a window from the layout.
///
/// If the column becomes empty after removal, it is removed and
/// the viewport adjusts if needed.
#[must_use]
pub fn remove_window(
    layout: &VirtualLayout,
    window: WindowId,
    config: &MutationConfig,
) -> VirtualLayout {
    let Some((col, row)) = layout.find_window(window) else {
        return layout.clone();
    };

    let mut new_layout = layout.clone();
    let col_ref = &mut new_layout.columns[col];
    col_ref.rows.remove(row);

    if col_ref.rows.is_empty() {
        new_layout.columns.remove(col);
    }

    // Clamp viewport offset using actual config values
    let total = total_column_span(&new_layout, config);
    let max_offset = (total - config.monitor_width).max(0);
    new_layout.viewport_offset = new_layout.viewport_offset.min(max_offset);

    new_layout
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute the step width of the first visible column.
fn first_visible_step(layout: &VirtualLayout, config: &MutationConfig) -> Option<i32> {
    let mut canvas_x: i32 = 0;
    let vp_right = layout.viewport_offset + config.monitor_width;
    for col in &layout.columns {
        let col_px = column_eighths_to_pixels(col.width_eighths, config.column_width);
        let col_left = canvas_x;
        let col_right = canvas_x + col_px;
        if col_right > layout.viewport_offset && col_left < vp_right {
            return Some(column_step_width(col, config.column_width));
        }
        canvas_x += col_px;
    }
    None
}

/// Total pixel span of all columns (packed, no inter-column gap).
fn total_column_span(layout: &VirtualLayout, config: &MutationConfig) -> i32 {
    if layout.columns.is_empty() {
        return 0;
    }
    layout
        .columns
        .iter()
        .map(|c| column_eighths_to_pixels(c.width_eighths, config.column_width))
        .sum()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::WindowId;
    use crate::layout::types::Padding;

    fn test_config() -> MutationConfig {
        MutationConfig {
            monitor_width: 1920,
            column_width: 960,
            default_column_width_eighths: 4,
            padding: Padding {
                window: 4,
                up: 0,
                down: 0,
            },
        }
    }

    fn three_column_layout() -> VirtualLayout {
        VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            0,
        )
    }

    // --- Scroll ---

    #[test]
    fn scroll_right_advances_viewport() {
        let layout = three_column_layout();
        let config = test_config();
        let result = scroll_right(&layout, &config).expect("scroll right");
        assert!(result.viewport_offset > 0);
        // Step = col_width = 960 (packed, no inter-column gap)
        assert_eq!(result.viewport_offset, 960);
    }

    #[test]
    fn scroll_left_returns_none_at_zero() {
        let layout = three_column_layout();
        let config = test_config();
        assert!(scroll_left(&layout, &config).is_none());
    }

    #[test]
    fn scroll_right_then_left_roundtrips() {
        let layout = three_column_layout();
        let config = test_config();
        let scrolled = scroll_right(&layout, &config).expect("scroll right");
        let back = scroll_left(&scrolled, &config).expect("scroll left");
        assert_eq!(back.viewport_offset, 0);
    }

    // --- Focus ---

    #[test]
    fn focus_right_moves_to_next_column() {
        let layout = three_column_layout();
        let result =
            focus(&layout, WindowId(1), Direction::Right, &test_config()).expect("focus right");
        assert_eq!(result.focused, WindowId(2));
    }

    #[test]
    fn focus_left_at_edge_returns_none() {
        let layout = three_column_layout();
        assert!(focus(&layout, WindowId(1), Direction::Left, &test_config()).is_none());
    }

    #[test]
    fn focus_vertical_in_multirow_column() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(
                8,
                vec![WindowId(1), WindowId(2), WindowId(3)],
            )],
            0,
        );
        let r1 = focus(&layout, WindowId(1), Direction::Down, &test_config()).expect("down");
        assert_eq!(r1.focused, WindowId(2));
        let r2 = focus(&layout, WindowId(2), Direction::Down, &test_config()).expect("down");
        assert_eq!(r2.focused, WindowId(3));
        assert!(focus(&layout, WindowId(3), Direction::Down, &test_config()).is_none());
    }

    #[test]
    fn focus_right_scrolls_if_column_offscreen() {
        // 4 columns × 4 eighths each, viewport = 1920, only 2 visible at a time
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
                Column::new(4, WindowId(4)),
            ],
            0,
        );
        let config = test_config();
        // Focus right to column 2 (still visible)
        let r1 = focus(&layout, WindowId(1), Direction::Right, &config).expect("r1");
        assert_eq!(r1.focused, WindowId(2));
        // Focus right to column 3 — should trigger scroll
        let r2 = focus(&r1.new_layout, WindowId(2), Direction::Right, &config).expect("r2");
        assert_eq!(r2.focused, WindowId(3));
        // Viewport should have scrolled
        assert!(r2.new_layout.viewport_offset > r1.new_layout.viewport_offset);
    }

    // --- Swap ---

    #[test]
    fn swap_columns_reorders() {
        let layout = three_column_layout();
        let result = swap(&layout, WindowId(1), Direction::Right).expect("swap");
        assert_eq!(result.columns[0].rows[0], WindowId(2));
        assert_eq!(result.columns[1].rows[0], WindowId(1));
        assert_eq!(result.columns[2].rows[0], WindowId(3));
    }

    #[test]
    fn swap_rows_within_column() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        let result = swap(&layout, WindowId(1), Direction::Down).expect("swap down");
        assert_eq!(result.columns[0].rows[0], WindowId(2));
        assert_eq!(result.columns[0].rows[1], WindowId(1));
    }

    #[test]
    fn swap_left_at_edge_returns_none() {
        let layout = three_column_layout();
        assert!(swap(&layout, WindowId(1), Direction::Left).is_none());
    }

    // --- Expand / Shrink ---

    #[test]
    fn expand_column_grows_focused_shrinks_neighbor() {
        let layout = three_column_layout();
        let result = expand_column(&layout, WindowId(1), Direction::Right).expect("expand");
        assert_eq!(result.columns[0].width_eighths, 5);
        assert_eq!(result.columns[1].width_eighths, 3);
    }

    #[test]
    fn shrink_column_shrinks_focused_grows_neighbor() {
        let layout = three_column_layout();
        let result = shrink_column(&layout, WindowId(1), Direction::Right).expect("shrink");
        assert_eq!(result.columns[0].width_eighths, 3);
        assert_eq!(result.columns[1].width_eighths, 5);
    }

    #[test]
    fn expand_at_max_width_returns_none() {
        let layout = VirtualLayout::with_columns(
            vec![Column::new(8, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        assert!(expand_column(&layout, WindowId(1), Direction::Right).is_none());
    }

    #[test]
    fn set_column_width_explicit() {
        let layout = three_column_layout();
        let result = set_column_width(&layout, WindowId(1), 6, &test_config()).expect("set width");
        assert_eq!(result.columns[0].width_eighths, 6);
    }

    // --- Merge ---

    #[test]
    fn merge_column_right_combines_rows() {
        let layout = three_column_layout();
        let result = merge_column_right(&layout, WindowId(1)).expect("merge right");
        assert_eq!(result.columns.len(), 2);
        assert_eq!(result.columns[0].rows, vec![WindowId(1), WindowId(2)]);
    }

    #[test]
    fn merge_column_left_combines_rows() {
        let layout = three_column_layout();
        let result = merge_column_left(&layout, WindowId(2)).expect("merge left");
        assert_eq!(result.columns.len(), 2);
        assert_eq!(result.columns[0].rows, vec![WindowId(1), WindowId(2)]);
    }

    #[test]
    fn merge_at_edge_returns_none() {
        let layout = three_column_layout();
        assert!(merge_column_left(&layout, WindowId(1)).is_none());
        assert!(merge_column_right(&layout, WindowId(3)).is_none());
    }

    // --- Monocle ---

    #[test]
    fn toggle_monocle_expands_to_full() {
        let layout = three_column_layout();
        let (result, saved) = toggle_monocle(&layout, WindowId(1), None).expect("monocle on");
        assert_eq!(result.columns[0].width_eighths, 8);
        assert_eq!(saved, Some(4)); // saved original width

        // Toggle back
        let (restored, saved2) = toggle_monocle(&result, WindowId(1), saved).expect("monocle off");
        assert_eq!(restored.columns[0].width_eighths, 4);
        assert_eq!(saved2, None);
    }

    // --- Add / Remove ---

    #[test]
    fn add_window_appends_column() {
        let layout = three_column_layout();
        let result = add_window(&layout, WindowId(10), &test_config());
        assert_eq!(result.columns.len(), 4);
        assert_eq!(result.columns[3].rows[0], WindowId(10));
        assert_eq!(result.columns[3].width_eighths, 4);
    }

    #[test]
    fn remove_window_from_column() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        let result = remove_window(&layout, WindowId(1), &test_config());
        assert_eq!(result.columns.len(), 1);
        assert_eq!(result.columns[0].rows, vec![WindowId(2)]);
    }

    #[test]
    fn remove_last_window_in_column_removes_column() {
        let layout = three_column_layout();
        let result = remove_window(&layout, WindowId(2), &test_config());
        assert_eq!(result.columns.len(), 2);
        assert_eq!(result.columns[0].rows[0], WindowId(1));
        assert_eq!(result.columns[1].rows[0], WindowId(3));
    }

    #[test]
    fn remove_nonexistent_window_is_noop() {
        let layout = three_column_layout();
        let result = remove_window(&layout, WindowId(99), &test_config());
        assert_eq!(result, layout);
    }

    // --- Integration: Mutation edge cases ---

    #[test]
    fn swap_with_offscreen_right() {
        // Positive: swap focused column with first off-screen-right column
        let config = test_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
                Column::new(4, WindowId(4)),
            ],
            0,
        );
        // Viewport at 0, col 3 (index 2) and 4 (index 3) are off-screen right
        let result =
            swap_with_offscreen(&layout, WindowId(1), Direction::Right, &config).expect("swap");
        // Column 0 (was id=1) swapped with first off-screen right (index 2, was id=3)
        // After swap: cols = [id=3, id=2, id=1, id=4], and viewport scrolls to show col 0
        assert_eq!(result.columns[0].rows[0], WindowId(3));
        assert_eq!(result.columns[2].rows[0], WindowId(1));
    }

    #[test]
    fn swap_with_offscreen_left() {
        // Positive: swap focused with first off-screen-left column
        let config = test_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            960, // offset past first column (packed, no inter-column gap)
        );
        let result =
            swap_with_offscreen(&layout, WindowId(3), Direction::Left, &config).expect("swap");
        // Column 2 (was id=3) swapped with first off-screen left (index 0, was id=1)
        assert_eq!(result.columns[2].rows[0], WindowId(1));
        assert_eq!(result.columns[0].rows[0], WindowId(3));
    }

    #[test]
    fn swap_with_offscreen_none_when_all_visible() {
        // Negative: all columns visible → no off-screen to swap with
        let config = test_config();
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        assert!(swap_with_offscreen(&layout, WindowId(1), Direction::Right, &config).is_none());
    }

    #[test]
    fn add_window_to_column_appends_row() {
        // Positive: adding window to existing column creates multi-row
        let layout = VirtualLayout::with_columns(vec![Column::new(8, WindowId(1))], 0);
        let result = add_window_to_column(&layout, 0, WindowId(2));
        assert_eq!(result.columns[0].rows, vec![WindowId(1), WindowId(2)]);
    }

    #[test]
    fn add_window_to_invalid_column_is_noop() {
        // Negative: adding to non-existent column index
        let layout = VirtualLayout::with_columns(vec![Column::new(8, WindowId(1))], 0);
        let result = add_window_to_column(&layout, 5, WindowId(2));
        assert_eq!(result.columns.len(), 1);
        assert_eq!(result.columns[0].rows.len(), 1);
    }

    #[test]
    fn scroll_right_at_max_offset_returns_none() {
        // Negative: can't scroll beyond rightmost column
        let layout = VirtualLayout::with_columns(vec![Column::new(8, WindowId(1))], 0);
        let config = test_config();
        // Single column fills viewport exactly — no scroll possible
        assert!(scroll_right(&layout, &config).is_none());
    }

    #[test]
    fn focus_right_at_last_column_returns_none() {
        // Negative: focus right at rightmost column → None
        let layout = three_column_layout();
        assert!(focus(&layout, WindowId(3), Direction::Right, &test_config()).is_none());
    }

    #[test]
    fn focus_up_at_first_row_returns_none() {
        // Negative: focus up at top row → None
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        assert!(focus(&layout, WindowId(1), Direction::Up, &test_config()).is_none());
    }

    #[test]
    fn focus_on_nonexistent_window_returns_none() {
        // Negative: focus on window that doesn't exist → None
        let layout = three_column_layout();
        assert!(focus(&layout, WindowId(99), Direction::Right, &test_config()).is_none());
    }

    #[test]
    fn shrink_at_minimum_width_returns_none() {
        // Negative: can't shrink column below 1 eighth
        let layout = VirtualLayout::with_columns(
            vec![Column::new(1, WindowId(1)), Column::new(7, WindowId(2))],
            0,
        );
        assert!(shrink_column(&layout, WindowId(1), Direction::Right).is_none());
    }

    #[test]
    fn expand_column_neighbor_at_minimum_prevents() {
        // Negative: can't expand if neighbor would go below 1
        let layout = VirtualLayout::with_columns(
            vec![Column::new(7, WindowId(1)), Column::new(1, WindowId(2))],
            0,
        );
        assert!(expand_column(&layout, WindowId(1), Direction::Right).is_none());
    }

    #[test]
    fn expand_column_left_shrinks_left_neighbor() {
        // Positive: expand left direction shrinks left neighbor
        let layout = three_column_layout();
        let result = expand_column(&layout, WindowId(2), Direction::Left).expect("expand left");
        assert_eq!(result.columns[1].width_eighths, 5);
        assert_eq!(result.columns[0].width_eighths, 3);
    }

    #[test]
    fn merge_columns_combines_all_rows() {
        // Positive: after merge, all rows are present in the merged column
        let layout = three_column_layout();
        let result = merge_column_right(&layout, WindowId(1)).expect("merge");
        let col = &result.columns[0];
        assert_eq!(col.rows.len(), 2);
        assert_eq!(col.rows, vec![WindowId(1), WindowId(2)]);
    }

    #[test]
    fn toggle_monocle_without_saved_width_uses_default() {
        // Positive: monocle off without saved width → defaults to 4
        let layout = VirtualLayout::with_columns(vec![Column::new(8, WindowId(1))], 0);
        let (result, saved) = toggle_monocle(&layout, WindowId(1), None).expect("toggle");
        assert_eq!(result.columns[0].width_eighths, 4); // restored to default 4
        assert_eq!(saved, None);
    }

    #[test]
    fn add_window_to_empty_layout() {
        // Positive: adding first window to empty layout
        let layout = VirtualLayout::new();
        let result = add_window(&layout, WindowId(1), &test_config());
        assert_eq!(result.columns.len(), 1);
        assert_eq!(result.columns[0].rows[0], WindowId(1));
        assert_eq!(result.columns[0].width_eighths, 4); // default
    }

    #[test]
    fn remove_all_windows_yields_empty_layout() {
        // Positive: remove every window → empty layout
        let layout = three_column_layout();
        let cfg = test_config();
        let r1 = remove_window(&layout, WindowId(1), &cfg);
        let r2 = remove_window(&r1, WindowId(2), &cfg);
        let r3 = remove_window(&r2, WindowId(3), &cfg);
        assert!(r3.columns.is_empty());
    }

    #[test]
    fn set_column_width_no_change_returns_same_layout() {
        // Positive: setting width to current value is no-op
        let layout = three_column_layout();
        let result = set_column_width(&layout, WindowId(1), 4, &test_config()).expect("set");
        assert_eq!(result.columns[0].width_eighths, 4);
        // Width didn't change, so delta was 0 and layout is unchanged
        assert_eq!(result.columns.len(), 3);
    }

    #[test]
    fn swap_same_row_returns_none() {
        // Negative: can't swap with self (edge case)
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        assert!(swap(&layout, WindowId(1), Direction::Left).is_none());
    }

    #[test]
    fn swap_down_at_last_row_returns_none() {
        // Negative: can't swap down from last row
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        assert!(swap(&layout, WindowId(2), Direction::Down).is_none());
    }

    #[test]
    fn swap_right_at_last_column_returns_none() {
        // Negative: can't swap right from last column
        let layout = three_column_layout();
        assert!(swap(&layout, WindowId(3), Direction::Right).is_none());
    }

    #[test]
    fn merge_right_at_last_column_returns_none() {
        // Negative: can't merge right from last column (already tested but explicit)
        assert!(merge_column_right(&three_column_layout(), WindowId(3)).is_none());
    }

    #[test]
    fn expand_column_without_horizontal_direction_returns_none() {
        // Negative: expand/shrink only works for Left/Right
        let layout = three_column_layout();
        assert!(expand_column(&layout, WindowId(1), Direction::Up).is_none());
        assert!(shrink_column(&layout, WindowId(1), Direction::Down).is_none());
    }
}
