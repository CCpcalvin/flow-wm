//! Layout mutation operations — all pure functions.
//!
//! Every mutation takes a `&VirtualLayout` and returns a **new** `VirtualLayout`.
//! The layout is never mutated in place. This functional approach makes mutations
//! easy to test, compose, and reason about — there is no mutable state to get
//! out of sync.
//!
//! # Design principles
//!
//! **Camera over window-shifting**: Many operations that intuitively feel like
//! "move windows" are actually implemented by adjusting `viewport_offset` (the
//! camera position) instead. For example, `ensure_column_visible` shifts the
//! camera so a target column comes into view — no individual window positions
//! are touched in the [`VirtualLayout`].
//!
//! **Focus-by-WindowId**: Focus is tracked as a stable [`WindowId`],
//! not as a column/row index. This means operations like column swapping require
//! no focus fixup — the focused window ID remains valid regardless of where it
//! moves in the layout.
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

use crate::common::{Direction, WindowId};
use crate::layout::projection::{column_eighths_to_pixels, column_step_width};
use crate::layout::types::{Column, Padding, VirtualLayout};

/// Location of a neighboring window returned by [`find_neighbor_window`].
///
/// Contains the column and row indices identifying a single window's position
/// within the [`VirtualLayout`]. This is the vocabulary shared by `focus` and
/// `swap_window` for directional neighbor lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NeighborLocation {
    /// Column index in [`VirtualLayout::columns`].
    pub col: usize,
    /// Row index within the column's `rows` vector.
    pub row: usize,
}

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
// Neighbor lookup
// ---------------------------------------------------------------------------

/// Find the nearest neighboring window in the given direction.
///
/// This is the shared directional-lookup primitive used by both [`focus`] and
/// [`swap_window`]. For vertical directions (Up/Down), the neighbor is the
/// adjacent row within the same column. For horizontal directions (Left/Right),
/// the neighbor is the window in the adjacent column whose row index is
/// **closest** to the focused window's row.
///
/// # Horizontal neighbor selection
///
/// When crossing column boundaries, the target column may have a different
/// number of rows than the source column. This function picks the row whose
/// index is closest to the focused window's row:
///
/// ```text
/// Col 0        Col 1
/// [W1] (row 0) [W3] (row 0)  ← W1's right neighbor = W3
/// [W2] (row 1) [W4] (row 1)  ← W2's right neighbor = W4
///              [W5] (row 2)  ← (extra row, not matched)
/// ```
///
/// If the target column has fewer rows, the last row is used:
/// ```text
/// Col 0        Col 1
/// [W1] (row 0) [W3] (row 0)  ← W2's right neighbor = W3 (clamped from row 1)
/// [W2] (row 1)
/// ```
///
/// Returns `None` if there is no window in that direction.
#[must_use]
pub fn find_neighbor_window(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
) -> Option<NeighborLocation> {
    let (col, row) = layout.find_window(focused)?;

    match direction {
        Direction::Up => {
            if row == 0 {
                return None;
            }
            Some(NeighborLocation { col, row: row - 1 })
        }
        Direction::Down => {
            let max_row = layout.columns[col].rows.len().saturating_sub(1);
            if row >= max_row {
                return None;
            }
            Some(NeighborLocation { col, row: row + 1 })
        }
        Direction::Left => {
            if col == 0 {
                return None;
            }
            let target_col = col - 1;
            let target_row = closest_row(&layout.columns[target_col], row);
            Some(NeighborLocation {
                col: target_col,
                row: target_row,
            })
        }
        Direction::Right => {
            if col + 1 >= layout.columns.len() {
                return None;
            }
            let target_col = col + 1;
            let target_row = closest_row(&layout.columns[target_col], row);
            Some(NeighborLocation {
                col: target_col,
                row: target_row,
            })
        }
    }
}

/// Pick the row index in `column` closest to `preferred_row`.
///
/// Clamps to the valid range `[0, column.rows.len() - 1]`.
fn closest_row(column: &Column, preferred_row: usize) -> usize {
    preferred_row.min(column.rows.len().saturating_sub(1))
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
/// a new virtual layout if the camera shifted.
#[derive(Debug, Clone, PartialEq)]
pub struct FocusResult {
    /// The newly focused window.
    pub focused: WindowId,
    /// The new virtual layout (viewport may have scrolled to reveal the target).
    pub new_layout: VirtualLayout,
}

/// Move focus in the given direction.
///
/// Uses [`find_neighbor_window`] to locate the nearest window in the specified
/// direction. For horizontal focus changes (Left/Right), if the target column
/// is outside the viewport, the **camera shifts** (via `ensure_column_visible`)
/// to bring it into view — no individual window positions are modified.
///
/// Focus is tracked by [`WindowId`], not by position, so this function simply
/// resolves the target window ID and optionally adjusts the camera.
///
/// Returns `None` if there is no window to focus in that direction.
#[must_use]
pub fn focus(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
    config: &MutationConfig,
) -> Option<FocusResult> {
    let neighbor = find_neighbor_window(layout, focused, direction)?;
    let target_window = layout.columns[neighbor.col].rows[neighbor.row];

    match direction {
        Direction::Left | Direction::Right => {
            let new_layout = ensure_column_visible(layout, neighbor.col, config);
            Some(FocusResult {
                focused: target_window,
                new_layout,
            })
        }
        Direction::Up | Direction::Down => Some(FocusResult {
            focused: target_window,
            new_layout: layout.clone(),
        }),
    }
}

/// Shift the camera so the given column becomes visible.
///
/// This is the core "camera shift" operation. It checks whether the target
/// column's virtual canvas range overlaps the current viewport:
/// - If the column is off-screen **left**, the camera scrolls left to align
///   the viewport with the column's left edge.
/// - If the column is off-screen **right**, the camera scrolls right so the
///   column's right edge aligns with the viewport's right edge.
/// - If already visible, no change.
///
/// This is used by focus, swap, and other operations that need
/// to ensure a specific column is on-screen.
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

/// Swap the focused window's **column** with an adjacent column (Left/Right),
/// or swap two rows within the same column (Up/Down).
///
/// This is the **column-level** swap — horizontal directions reorder entire
/// columns, keeping all windows within each column together.
///
/// For horizontal swaps, after reordering the columns in the [`VirtualLayout`],
/// [`ensure_column_visible`] is called to guarantee the focused window's column
/// is within the viewport. If the column is already visible, this is a no-op;
/// if not, the camera shifts automatically.
///
/// Focus requires no fixup — it is tracked by [`WindowId`], so it follows
/// the focused window regardless of column reordering.
///
/// Returns `None` if there is no adjacent element in that direction.
#[must_use]
pub fn swap_column(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
    config: &MutationConfig,
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
            let swapped = swap_columns(layout, col, col - 1)?;
            Some(ensure_column_visible(&swapped, col - 1, config))
        }
        Direction::Right => {
            let max_col = layout.columns.len().saturating_sub(1);
            if col >= max_col {
                return None;
            }
            let swapped = swap_columns(layout, col, col + 1)?;
            Some(ensure_column_visible(&swapped, col + 1, config))
        }
    }
}

/// Swap the focused window with an adjacent **individual window**.
///
/// Unlike [`swap_column`] which moves entire columns, this swaps two specific
/// window IDs regardless of which columns they belong to. For Left/Right,
/// the focused window is swapped with the nearest window in the adjacent column
/// (found via [`find_neighbor_window`]). For Up/Down, it behaves like row swap
/// within the same column.
///
/// # Example
///
/// ```text
/// Before: [W1] [W2, W3]   ← W1 focused, swap_window Right
/// After:  [W2] [W1, W3]   ← W1 and W2 exchanged positions
/// ```
///
/// After the swap, [`ensure_column_visible`] is called to guarantee the
/// focused window's new position is visible in the viewport.
///
/// Returns `None` if there is no neighbor in that direction.
#[must_use]
pub fn swap_window(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let (src_col, src_row) = layout.find_window(focused)?;
    let neighbor = find_neighbor_window(layout, focused, direction)?;
    let (dst_col, dst_row) = (neighbor.col, neighbor.row);

    // Same position — nothing to swap
    if src_col == dst_col && src_row == dst_row {
        return None;
    }

    let mut new_layout = layout.clone();

    if src_col == dst_col {
        // Same column — swap rows directly
        new_layout.columns[src_col].rows.swap(src_row, dst_row);
    } else {
        // Different columns — exchange window IDs between the two positions
        let dst_window = new_layout.columns[dst_col].rows[dst_row];
        new_layout.columns[src_col].rows[src_row] = dst_window;
        new_layout.columns[dst_col].rows[dst_row] = focused;
    }

    // Ensure the focused window (now at dst position) is visible
    Some(ensure_column_visible(&new_layout, dst_col, config))
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
///
/// Only the column order in the [`VirtualLayout`] changes — no pixel coordinates
/// are touched. Focus is unaffected because it is tracked by [`WindowId`], not
/// by column index. The projection layer will compute new pixel positions from
/// the reordered layout.
fn swap_columns(layout: &VirtualLayout, col_a: usize, col_b: usize) -> Option<VirtualLayout> {
    let mut new_layout = layout.clone();
    new_layout.columns.swap(col_a, col_b);
    Some(new_layout)
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

    #[test]
    fn focus_right_crosses_to_column_with_different_row_count() {
        // Col 0: [W1] (1 row), Col 1: [W2, W3, W4] (3 rows)
        // Focus right from W1 → picks closest row in col 1 (row 0) = W2
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::with_equal_rows(4, vec![WindowId(2), WindowId(3), WindowId(4)]),
            ],
            0,
        );
        let result =
            focus(&layout, WindowId(1), Direction::Right, &test_config()).expect("focus right");
        assert_eq!(result.focused, WindowId(2));
    }

    // --- Swap ---

    #[test]
    fn swap_column_reorders() {
        let layout = three_column_layout();
        let result =
            swap_column(&layout, WindowId(1), Direction::Right, &test_config()).expect("swap");
        assert_eq!(result.columns[0].rows[0], WindowId(2));
        assert_eq!(result.columns[1].rows[0], WindowId(1));
        assert_eq!(result.columns[2].rows[0], WindowId(3));
    }

    #[test]
    fn swap_column_rows_within_column() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        let result =
            swap_column(&layout, WindowId(1), Direction::Down, &test_config()).expect("swap down");
        assert_eq!(result.columns[0].rows[0], WindowId(2));
        assert_eq!(result.columns[0].rows[1], WindowId(1));
    }

    #[test]
    fn swap_column_up_swaps_rows() {
        // Positive: W2 at row 1, swap_column Up → swaps with W1 at row 0
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        let result =
            swap_column(&layout, WindowId(2), Direction::Up, &test_config()).expect("swap up");
        assert_eq!(result.columns[0].rows[0], WindowId(2));
        assert_eq!(result.columns[0].rows[1], WindowId(1));
    }

    #[test]
    fn swap_column_up_at_first_row_returns_none() {
        // Negative: W1 at row 0, swap_column Up → None (swap_rows with self returns None)
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        assert!(swap_column(&layout, WindowId(1), Direction::Up, &test_config()).is_none());
    }

    #[test]
    fn swap_column_left_at_edge_returns_none() {
        let layout = three_column_layout();
        assert!(swap_column(&layout, WindowId(1), Direction::Left, &test_config()).is_none());
    }

    #[test]
    fn swap_column_shifts_viewport_when_target_offscreen() {
        let config = test_config(); // monitor_width=1920, column_width=960 → each col = 960px
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            960, // viewport past column 0
        );
        // Window 2 is in column 1 (visible). Swap left → column 0 (off-screen).
        let result = swap_column(&layout, WindowId(2), Direction::Left, &config).expect("swap");
        assert!(
            result.viewport_offset < 960,
            "camera should shift left to reveal col 0"
        );
    }

    #[test]
    fn swap_column_no_viewport_change_when_both_visible() {
        let config = test_config();
        let layout = three_column_layout(); // viewport=0, cols 0+1 fully visible
        let result = swap_column(&layout, WindowId(1), Direction::Right, &config).expect("swap");
        assert_eq!(
            result.viewport_offset, 0,
            "camera should not shift when both columns are visible"
        );
    }

    // --- Swap Window (individual window swap) ---

    #[test]
    fn swap_window_right_swaps_with_neighbor_in_next_column() {
        // [W1] [W2, W3] → swap_window right on W1 → [W2] [W1, W3]
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::with_equal_rows(4, vec![WindowId(2), WindowId(3)]),
            ],
            0,
        );
        let result =
            swap_window(&layout, WindowId(1), Direction::Right, &test_config()).expect("swap");
        assert_eq!(result.columns[0].rows, vec![WindowId(2)]);
        assert_eq!(result.columns[1].rows, vec![WindowId(1), WindowId(3)]);
    }

    #[test]
    fn swap_window_left_swaps_with_neighbor_in_prev_column() {
        // [W1, W2] [W3] → swap_window left on W3 → [W1, W3] [W2]
        // W3 is at row 0 in col 1. Closest row in col 0 is row 0 = W1.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_equal_rows(4, vec![WindowId(1), WindowId(2)]),
                Column::new(4, WindowId(3)),
            ],
            0,
        );
        let result =
            swap_window(&layout, WindowId(3), Direction::Left, &test_config()).expect("swap");
        assert_eq!(result.columns[0].rows, vec![WindowId(3), WindowId(2)]);
        assert_eq!(result.columns[1].rows, vec![WindowId(1)]);
    }

    #[test]
    fn swap_window_down_same_column() {
        // [W1, W2] in same column → swap_window down on W1 → [W2, W1]
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        let result =
            swap_window(&layout, WindowId(1), Direction::Down, &test_config()).expect("swap");
        assert_eq!(result.columns[0].rows, vec![WindowId(2), WindowId(1)]);
    }

    #[test]
    fn swap_window_picks_closest_row_in_target_column() {
        // Col 0: [W1, W2] (2 rows)
        // Col 1: [W3] (1 row)
        // W2 is at row 1. Closest row in col 1 (clamped) = row 0 = W3.
        // swap_window right on W2 → W2 goes to col 1 row 0, W3 goes to col 0 row 1.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_equal_rows(4, vec![WindowId(1), WindowId(2)]),
                Column::new(4, WindowId(3)),
            ],
            0,
        );
        let result =
            swap_window(&layout, WindowId(2), Direction::Right, &test_config()).expect("swap");
        assert_eq!(result.columns[0].rows, vec![WindowId(1), WindowId(3)]);
        assert_eq!(result.columns[1].rows, vec![WindowId(2)]);
    }

    #[test]
    fn swap_window_right_at_edge_returns_none() {
        let layout = three_column_layout();
        assert!(swap_window(&layout, WindowId(3), Direction::Right, &test_config()).is_none());
    }

    #[test]
    fn swap_window_left_at_edge_returns_none() {
        let layout = three_column_layout();
        assert!(swap_window(&layout, WindowId(1), Direction::Left, &test_config()).is_none());
    }

    #[test]
    fn swap_window_down_at_last_row_returns_none() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        assert!(swap_window(&layout, WindowId(2), Direction::Down, &test_config()).is_none());
    }

    #[test]
    fn swap_window_up_same_column() {
        // [W1, W2] → swap_window up on W2 → [W2, W1]
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        let result =
            swap_window(&layout, WindowId(2), Direction::Up, &test_config()).expect("swap");
        assert_eq!(result.columns[0].rows, vec![WindowId(2), WindowId(1)]);
    }

    #[test]
    fn swap_window_up_at_first_row_returns_none() {
        // Negative: W1 at row 0, swap_window up → None
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        assert!(swap_window(&layout, WindowId(1), Direction::Up, &test_config()).is_none());
    }

    #[test]
    fn swap_window_shifts_viewport_when_target_offscreen() {
        let config = test_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            960, // viewport past column 0
        );
        let result = swap_window(&layout, WindowId(2), Direction::Left, &config).expect("swap");
        assert!(
            result.viewport_offset < 960,
            "camera should shift left to reveal col 0"
        );
    }

    #[test]
    fn swap_window_no_viewport_change_when_both_visible() {
        let config = test_config();
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        let result = swap_window(&layout, WindowId(1), Direction::Right, &config).expect("swap");
        assert_eq!(result.viewport_offset, 0);
    }

    #[test]
    fn swap_window_nonexistent_returns_none() {
        let layout = three_column_layout();
        assert!(swap_window(&layout, WindowId(99), Direction::Right, &test_config()).is_none());
    }

    // --- find_neighbor_window ---

    #[test]
    fn find_neighbor_right_returns_first_row_in_next_column() {
        let layout = three_column_layout();
        let neighbor = find_neighbor_window(&layout, WindowId(1), Direction::Right).expect("right");
        assert_eq!(neighbor, NeighborLocation { col: 1, row: 0 });
        assert_eq!(layout.columns[neighbor.col].rows[neighbor.row], WindowId(2));
    }

    #[test]
    fn find_neighbor_left_returns_first_row_in_prev_column() {
        let layout = three_column_layout();
        let neighbor = find_neighbor_window(&layout, WindowId(2), Direction::Left).expect("left");
        assert_eq!(neighbor, NeighborLocation { col: 0, row: 0 });
        assert_eq!(layout.columns[neighbor.col].rows[neighbor.row], WindowId(1));
    }

    #[test]
    fn find_neighbor_up_returns_prev_row_same_column() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        let neighbor = find_neighbor_window(&layout, WindowId(2), Direction::Up).expect("up");
        assert_eq!(neighbor, NeighborLocation { col: 0, row: 0 });
    }

    #[test]
    fn find_neighbor_down_returns_next_row_same_column() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        let neighbor = find_neighbor_window(&layout, WindowId(1), Direction::Down).expect("down");
        assert_eq!(neighbor, NeighborLocation { col: 0, row: 1 });
    }

    #[test]
    fn find_neighbor_clamps_row_to_target_column_size() {
        // Col 0 has 3 rows, Col 1 has 1 row.
        // W3 is at row 2 in col 0. Right neighbor in col 1 → clamped to row 0.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_equal_rows(4, vec![WindowId(1), WindowId(2), WindowId(3)]),
                Column::new(4, WindowId(4)),
            ],
            0,
        );
        let neighbor = find_neighbor_window(&layout, WindowId(3), Direction::Right).expect("right");
        assert_eq!(neighbor, NeighborLocation { col: 1, row: 0 });
    }

    #[test]
    fn find_neighbor_right_at_edge_returns_none() {
        let layout = three_column_layout();
        assert!(find_neighbor_window(&layout, WindowId(3), Direction::Right).is_none());
    }

    #[test]
    fn find_neighbor_left_at_edge_returns_none() {
        let layout = three_column_layout();
        assert!(find_neighbor_window(&layout, WindowId(1), Direction::Left).is_none());
    }

    #[test]
    fn find_neighbor_up_at_first_row_returns_none() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        assert!(find_neighbor_window(&layout, WindowId(1), Direction::Up).is_none());
    }

    #[test]
    fn find_neighbor_down_at_last_row_returns_none() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        assert!(find_neighbor_window(&layout, WindowId(2), Direction::Down).is_none());
    }

    #[test]
    fn find_neighbor_nonexistent_window_returns_none() {
        let layout = three_column_layout();
        assert!(find_neighbor_window(&layout, WindowId(99), Direction::Right).is_none());
    }

    #[test]
    fn find_neighbor_source_fewer_rows_picks_closest() {
        // Col 0 has 1 row, Col 1 has 3 rows.
        // W1 at row 0 in col 0. Right neighbor in col 1 → closest_row(3, 0) = 0 → W2.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::with_equal_rows(4, vec![WindowId(2), WindowId(3), WindowId(4)]),
            ],
            0,
        );
        let neighbor = find_neighbor_window(&layout, WindowId(1), Direction::Right).expect("right");
        assert_eq!(neighbor, NeighborLocation { col: 1, row: 0 });
        assert_eq!(layout.columns[neighbor.col].rows[neighbor.row], WindowId(2));
    }

    #[test]
    fn find_neighbor_multirow_matching_row_in_both_columns() {
        // Both columns have 3 rows. W5 at row 1 in col 0 → right neighbor = row 1 in col 1 = W8.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_equal_rows(4, vec![WindowId(1), WindowId(5), WindowId(9)]),
                Column::with_equal_rows(4, vec![WindowId(2), WindowId(8), WindowId(6)]),
            ],
            0,
        );
        let neighbor = find_neighbor_window(&layout, WindowId(5), Direction::Right).expect("right");
        assert_eq!(neighbor, NeighborLocation { col: 1, row: 1 });
        assert_eq!(layout.columns[neighbor.col].rows[neighbor.row], WindowId(8));
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
    fn swap_column_same_row_returns_none() {
        // Negative: can't swap with self (edge case)
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        assert!(swap_column(&layout, WindowId(1), Direction::Left, &test_config()).is_none());
    }

    #[test]
    fn swap_column_down_at_last_row_returns_none() {
        // Negative: can't swap down from last row
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        assert!(swap_column(&layout, WindowId(2), Direction::Down, &test_config()).is_none());
    }

    #[test]
    fn swap_column_right_at_last_column_returns_none() {
        // Negative: can't swap right from last column
        let layout = three_column_layout();
        assert!(swap_column(&layout, WindowId(3), Direction::Right, &test_config()).is_none());
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
