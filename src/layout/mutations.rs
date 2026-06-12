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
    /// Minimum column width in eighths (computed from config `min_column_width_px`).
    pub min_column_eighths: u8,
    /// Maximum column width in eighths (computed from `monitor_width / column_width`).
    pub max_column_eighths: u8,
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
/// This is used by focus, swap, resize, and other operations that need
/// to ensure a specific column is on-screen.
pub(crate) fn ensure_column_visible(
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

/// Centers the viewport so the given column appears at the horizontal midpoint
/// of the monitor.
///
/// Unlike [`ensure_column_visible`] which only scrolls the minimum amount needed
/// to bring a column into view, this function always places the target column
/// at the *center* of the viewport. This produces a more visually intentional
/// result during initialization — the user's most-recently-active window
/// appears front-and-center rather than potentially hugging the left edge.
///
/// # Small-canvas handling
///
/// When the total canvas width is **less than or equal to** the monitor width,
/// *all* columns fit on screen at once. In this case the entire canvas is
/// centered on the monitor — `viewport_offset` becomes negative so the canvas
/// is shifted rightward into the middle of the viewport. The projection layer
/// ([`super::projection::project`]) already handles negative offsets correctly.
///
/// # Algorithm
///
/// 1. Compute total canvas width from all columns.
/// 2. **If canvas ≤ monitor**: center the *entire canvas* with
///    `viewport_offset = -(monitor_width - canvas_width) / 2`.
/// 3. **If canvas > monitor**: walk columns left-to-right, find the target
///    column's center, compute `viewport_offset = col_center - monitor_width/2`,
///    clamped to `≥ 0`.
///
/// # Panics
///
/// Panics if `col_idx` is out of bounds for the layout's columns.
///
/// # Arguments
///
/// * `layout` — The current virtual layout.
/// * `col_idx` — Index of the column to center.
/// * `config` — Mutation configuration providing monitor width and column width.
///
/// # Returns
///
/// A new [`VirtualLayout`] with `viewport_offset` adjusted to center the
/// requested column (or the entire canvas, if it fits within the monitor).
#[must_use]
pub(crate) fn center_on_column(
    layout: &VirtualLayout,
    col_idx: usize,
    config: &MutationConfig,
) -> VirtualLayout {
    assert!(
        col_idx < layout.columns.len(),
        "center_on_column: col_idx {} out of bounds ({} columns)",
        col_idx,
        layout.columns.len()
    );

    let total_canvas: i32 = layout
        .columns
        .iter()
        .map(|c| column_eighths_to_pixels(c.width_eighths, config.column_width))
        .sum();

    if total_canvas <= config.monitor_width {
        // All columns fit on screen — center the *entire canvas* horizontally.
        // A negative viewport_offset shifts the virtual canvas rightward
        // relative to the monitor's left edge.
        let viewport_offset = -(config.monitor_width - total_canvas) / 2;
        return VirtualLayout {
            viewport_offset,
            ..layout.clone()
        };
    }

    // Canvas exceeds monitor — center the requested column in the viewport.
    // viewport_offset is always ≥ 0 here because col_center ≥ monitor_width/2
    // when the canvas is wider than the monitor.
    let mut canvas_x: i32 = 0;
    for (i, col) in layout.columns.iter().enumerate() {
        let col_px = column_eighths_to_pixels(col.width_eighths, config.column_width);
        if i == col_idx {
            let col_center = canvas_x + col_px / 2;
            let viewport_offset = (col_center - config.monitor_width / 2).max(0);
            return VirtualLayout {
                viewport_offset,
                ..layout.clone()
            };
        }
        canvas_x += col_px;
    }

    // Unreachable because we asserted `col_idx < len`, but satisfy the compiler.
    layout.clone()
}

// ---------------------------------------------------------------------------
// Swap mutations
// ---------------------------------------------------------------------------

/// Swap the focused window's **column** with an adjacent column (Left/Right).
///
/// Columns only have horizontal neighbours, so vertical directions (`Up` / `Down`)
/// are not meaningful for a column-level swap and always return `None`.
/// Use [`swap_window`] for per-window swaps in all four directions.
///
/// For horizontal swaps, after reordering the columns in the [`VirtualLayout`],
/// [`ensure_column_visible`] is called to guarantee the focused window's column
/// is within the viewport. If the column is already visible, this is a no-op;
/// if not, the camera shifts automatically.
///
/// Focus requires no fixup — it is tracked by [`WindowId`], so it follows
/// the focused window regardless of column reordering.
///
/// Returns `None` if there is no adjacent column in that direction, or if the
/// direction is vertical.
#[must_use]
pub fn swap_column(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let (col, _row) = layout.find_window(focused)?;
    match direction {
        Direction::Up | Direction::Down => {
            log::warn!(
                "swap_column: vertical direction ({direction:?}) is invalid — \
                 columns only have horizontal neighbours; use swap_window instead"
            );
            None
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
// Resize mutations (independent — no neighbor compensation)
// ---------------------------------------------------------------------------

/// Convert a pixel width to the nearest eighths value.
///
/// Uses rounding: `(px * 4 + column_width / 2) / column_width`.
/// Result is clamped to `[1, 255]`.
fn pixels_to_eighths(px: i32, column_width: u32) -> u8 {
    let cw = column_width as i32;
    ((px * 4 + cw / 2) / cw).clamp(1, 255) as u8
}

/// Set the focused column width to an explicit pixel value.
///
/// This is the **core resize primitive** — all other resize functions
/// delegate here. The target pixel width is snapped to the nearest
/// eighth, validated against `[min_column_eighths, max_column_eighths]`,
/// applied to the column, and followed by [`ensure_column_visible`]
/// to guarantee the resized column is in view.
///
/// Returns `None` if the focused window is not found, the target snaps
/// to the same width as current, or the target is out of bounds.
#[must_use]
pub fn set_column_width(
    layout: &VirtualLayout,
    focused: WindowId,
    target_px: i32,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let (col, _) = layout.find_window(focused)?;

    let target_eighths = pixels_to_eighths(target_px, config.column_width);

    // Validate bounds
    if target_eighths < config.min_column_eighths || target_eighths > config.max_column_eighths {
        return None;
    }

    // No change — nothing to do
    if target_eighths == layout.columns[col].width_eighths {
        return None;
    }

    let mut new_layout = layout.clone();
    new_layout.columns[col].width_eighths = target_eighths;

    Some(ensure_column_visible(&new_layout, col, config))
}

/// Resize the focused column by a pixel delta.
///
/// Computes `target_px = current_px + delta_px` and delegates to
/// [`set_column_width`]. Positive delta grows the column, negative
/// shrinks it.
///
/// Returns `None` if the resulting width is out of bounds.
#[must_use]
pub fn resize_column(
    layout: &VirtualLayout,
    focused: WindowId,
    delta_px: i32,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let (col, _) = layout.find_window(focused)?;
    let current_px =
        column_eighths_to_pixels(layout.columns[col].width_eighths, config.column_width);
    let target_px = current_px + delta_px;
    set_column_width(layout, focused, target_px, config)
}

/// Expand the focused column to the next `column_width` boundary above.
///
/// Snap points are multiples of `column_width` (e.g., 0, 960, 1920 with
/// column_width=960). The column width is set to the next snap point
/// strictly above its current width, capped at `max_column_eighths`.
///
/// Returns `None` if already at max or no higher snap point exists.
#[must_use]
pub fn expand_column(
    layout: &VirtualLayout,
    focused: WindowId,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let (col, _) = layout.find_window(focused)?;
    let current_px =
        column_eighths_to_pixels(layout.columns[col].width_eighths, config.column_width);
    let cw = config.column_width as i32;

    // Next column_width boundary strictly above current
    let target_px = ((current_px / cw) + 1) * cw;

    // Already at or beyond monitor width
    if target_px > config.monitor_width {
        return None;
    }

    set_column_width(layout, focused, target_px, config)
}

/// Shrink the focused column to the previous `column_width` boundary below.
///
/// Snap points are multiples of `column_width` (e.g., 0, 960, 1920 with
/// column_width=960). The column width is set to the next snap point
/// strictly below its current width, floored at `min_column_eighths`.
///
/// Returns `None` if already at minimum or no lower snap point exists.
#[must_use]
pub fn shrink_column(
    layout: &VirtualLayout,
    focused: WindowId,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let (col, _) = layout.find_window(focused)?;
    let current_px =
        column_eighths_to_pixels(layout.columns[col].width_eighths, config.column_width);
    let cw = config.column_width as i32;

    // Previous column_width boundary strictly below current
    let target_px = ((current_px - 1) / cw) * cw;
    let min_px = column_eighths_to_pixels(config.min_column_eighths, config.column_width);

    if target_px < min_px {
        return None;
    }

    set_column_width(layout, focused, target_px, config)
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

/// Build a complete virtual layout from a list of window IDs.
///
/// Creates one column per window with the default width. Called on an
/// empty layout during daemon startup when the registry already has
/// tracked windows from the init scan.
///
/// This is more efficient than calling [`add_window`] N times because
/// it builds the layout in a single operation without intermediate
/// projection + diff steps.
///
/// # Camera centering
///
/// When `focus_col_idx` is `Some(idx)`, the viewport is centered on the
/// specified column so that column appears at the horizontal midpoint of the
/// monitor. This produces a natural initial view focused on the user's
/// most-recently-active window. When `None`, the viewport starts at offset
/// `0` (left-aligned with the first column).
///
/// # Arguments
///
/// * `ids` — Window IDs to place in the layout, one per column, in order.
/// * `config` — Mutation configuration (provides default column width).
/// * `focus_col_idx` — Optional index of the column to center in the viewport.
///   If `Some`, the viewport is positioned so this column appears at the
///   monitor's horizontal center. If `None`, `viewport_offset` is `0`.
///
/// # Returns
///
/// A [`VirtualLayout`] with one column per window ID, each at the default
/// width from `config.default_column_width_eighths`.
///
/// # Example
///
/// ```
/// # use scrolling_tiling_manager::layout::mutations::{initialize_windows, MutationConfig};
/// # use scrolling_tiling_manager::layout::types::Padding;
/// # use scrolling_tiling_manager::common::WindowId;
/// let config = MutationConfig {
///     monitor_width: 1920,
///     column_width: 960,
///     default_column_width_eighths: 4,
///     min_column_eighths: 2,
///     max_column_eighths: 8,
///     padding: Padding { window_gap: 4, up: 0, down: 0 },
/// };
/// let layout = initialize_windows(
///     &[WindowId(1), WindowId(2), WindowId(3)],
///     &config,
///     None,
/// );
/// assert_eq!(layout.columns.len(), 3);
/// ```
#[must_use]
pub fn initialize_windows(
    ids: &[WindowId],
    config: &MutationConfig,
    focus_col_idx: Option<usize>,
) -> VirtualLayout {
    let columns: Vec<Column> = ids
        .iter()
        .map(|&id| Column::new(config.default_column_width_eighths, id))
        .collect();

    let initial = VirtualLayout {
        columns,
        viewport_offset: 0,
    };

    match focus_col_idx {
        Some(idx) if idx < initial.columns.len() => center_on_column(&initial, idx, config),
        _ => initial,
    }
}

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
            // ceil((320 * 4) / 960) = 2 eighths
            min_column_eighths: 2,
            // (1920 * 4) / 960 = 8 eighths
            max_column_eighths: 8,
            padding: Padding {
                window_gap: 4,
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
    fn swap_column_down_returns_none() {
        // Vertical directions are invalid for column-level swap
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        assert!(swap_column(&layout, WindowId(1), Direction::Down, &test_config()).is_none());
    }

    #[test]
    fn swap_column_up_returns_none() {
        // Vertical directions are invalid for column-level swap
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        assert!(swap_column(&layout, WindowId(2), Direction::Up, &test_config()).is_none());
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

    // --- Resize: set_column_width ---

    #[test]
    fn set_column_width_sets_target_in_px() {
        // Positive: 1440px → pixels_to_eighths(1440, 960) = 6 → width set to 6 eighths
        let layout = three_column_layout();
        let result =
            set_column_width(&layout, WindowId(1), 1440, &test_config()).expect("set width");
        assert_eq!(result.columns[0].width_eighths, 6);
    }

    #[test]
    fn set_column_width_only_affects_target_column() {
        // Positive: independent resize — other columns unchanged
        let layout = three_column_layout();
        let result =
            set_column_width(&layout, WindowId(1), 1440, &test_config()).expect("set width");
        assert_eq!(result.columns[0].width_eighths, 6);
        // Other columns remain at 4 (no compensation)
        assert_eq!(result.columns[1].width_eighths, 4);
        assert_eq!(result.columns[2].width_eighths, 4);
    }

    #[test]
    fn set_column_width_returns_none_if_below_min() {
        // Negative: target px snaps to eighths below min_column_eighths
        // 320px → 1.33 → rounds to 1 eighth, but min is 2 → None
        let layout = three_column_layout();
        assert!(set_column_width(&layout, WindowId(1), 320, &test_config()).is_none());
    }

    #[test]
    fn set_column_width_returns_none_if_above_max() {
        // Negative: target px exceeds monitor width
        // 2400px → 10 eighths, max is 8 → None
        let layout = three_column_layout();
        assert!(set_column_width(&layout, WindowId(1), 2400, &test_config()).is_none());
    }

    #[test]
    fn set_column_width_returns_none_if_same_as_current() {
        // Negative: target matches current → no-op → None
        // 960px → 4 eighths, current is 4 → None
        let layout = three_column_layout();
        assert!(set_column_width(&layout, WindowId(1), 960, &test_config()).is_none());
    }

    #[test]
    fn set_column_width_ensures_column_visible() {
        // Positive: after resize, viewport adjusts to show the column
        let config = test_config();
        // 3 columns × 960px = 2880px total canvas. viewport at 3000 → all off-screen right.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            3000,
        );
        // Resize column 2 (W3) from 960px to 1920px → ensure_column_visible shifts viewport
        let result = set_column_width(&layout, WindowId(3), 1920, &config).expect("set width");
        assert_eq!(result.columns[2].width_eighths, 8);
        // ensure_column_visible should have shifted the viewport left
        assert!(
            result.viewport_offset < 3000,
            "viewport should shift left to reveal the resized column"
        );
    }

    // --- Resize: resize_column ---

    #[test]
    fn resize_column_positive_delta_grows() {
        // Positive: +480px delta on 960px column → 1440px → 6 eighths
        let layout = three_column_layout();
        let result = resize_column(&layout, WindowId(1), 480, &test_config()).expect("resize +480");
        assert_eq!(result.columns[0].width_eighths, 6);
    }

    #[test]
    fn resize_column_negative_delta_shrinks() {
        // Positive: -480px delta on 960px column → 480px → 2 eighths
        let layout = three_column_layout();
        let result =
            resize_column(&layout, WindowId(1), -480, &test_config()).expect("resize -480");
        assert_eq!(result.columns[0].width_eighths, 2);
    }

    #[test]
    fn resize_column_delta_too_small_returns_none() {
        // Negative: +100px on 960px → 1060px → rounds to 4 eighths (same) → None
        let layout = three_column_layout();
        assert!(resize_column(&layout, WindowId(1), 100, &test_config()).is_none());
    }

    // --- Resize: expand_column ---

    #[test]
    fn expand_column_snaps_to_next_column_width_boundary() {
        // Positive: 960px (4 eighths) → snap up to 1920px (8 eighths)
        let layout = three_column_layout(); // columns at 4 eighths = 960px
        let result = expand_column(&layout, WindowId(1), &test_config()).expect("expand");
        assert_eq!(result.columns[0].width_eighths, 8);
    }

    #[test]
    fn expand_column_from_sub_boundary() {
        // Positive: 480px (2 eighths) → snap up to 960px (4 eighths)
        let layout = VirtualLayout::with_columns(
            vec![Column::new(2, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        let result = expand_column(&layout, WindowId(1), &test_config()).expect("expand");
        assert_eq!(result.columns[0].width_eighths, 4);
    }

    #[test]
    fn expand_column_at_max_returns_none() {
        // Negative: 1920px (8 eighths) → next boundary would be 2880 > 1920 → None
        let layout = VirtualLayout::with_columns(
            vec![Column::new(8, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        assert!(expand_column(&layout, WindowId(1), &test_config()).is_none());
    }

    #[test]
    fn expand_column_only_affects_target() {
        // Positive: independent resize — neighbors unchanged
        let layout = three_column_layout();
        let result = expand_column(&layout, WindowId(1), &test_config()).expect("expand");
        assert_eq!(result.columns[0].width_eighths, 8);
        assert_eq!(result.columns[1].width_eighths, 4);
        assert_eq!(result.columns[2].width_eighths, 4);
    }

    // --- Resize: shrink_column ---

    #[test]
    fn shrink_column_snaps_to_prev_column_width_boundary() {
        // Positive: 1920px (8 eighths) → snap down to 960px (4 eighths)
        let layout = VirtualLayout::with_columns(
            vec![Column::new(8, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        let result = shrink_column(&layout, WindowId(1), &test_config()).expect("shrink");
        assert_eq!(result.columns[0].width_eighths, 4);
    }

    #[test]
    fn shrink_column_from_mid_boundary() {
        // Positive: 1200px (5 eighths) → snap down to 960px (4 eighths)
        let layout = VirtualLayout::with_columns(
            vec![Column::new(5, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        let result = shrink_column(&layout, WindowId(1), &test_config()).expect("shrink");
        assert_eq!(result.columns[0].width_eighths, 4);
    }

    #[test]
    fn shrink_column_at_boundary_returns_none() {
        // Negative: 960px (4 eighths) → prev boundary is 0 → below min (2 eighths = 480px) → None
        let layout = three_column_layout();
        assert!(shrink_column(&layout, WindowId(1), &test_config()).is_none());
    }

    #[test]
    fn shrink_column_at_min_eighths_returns_none() {
        // Negative: 480px (2 eighths = min) → prev boundary is 0 → None
        let layout = VirtualLayout::with_columns(
            vec![Column::new(2, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        assert!(shrink_column(&layout, WindowId(1), &test_config()).is_none());
    }

    #[test]
    fn shrink_column_only_affects_target() {
        // Positive: independent resize — neighbors unchanged
        let layout = VirtualLayout::with_columns(
            vec![Column::new(8, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        let result = shrink_column(&layout, WindowId(1), &test_config()).expect("shrink");
        assert_eq!(result.columns[0].width_eighths, 4);
        assert_eq!(result.columns[1].width_eighths, 4);
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
        // Negative: can't shrink column below min_column_eighths
        let layout = VirtualLayout::with_columns(
            vec![Column::new(2, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        // 2 eighths = 480px → prev boundary = 0 → below min → None
        assert!(shrink_column(&layout, WindowId(1), &test_config()).is_none());
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
    fn set_column_width_no_change_returns_none() {
        // Negative: setting width to current px value → same eighths → None
        let layout = three_column_layout();
        // 4 eighths * 960 / 4 = 960px; setting 960px → 4 eighths = current → None
        assert!(set_column_width(&layout, WindowId(1), 960, &test_config()).is_none());
    }

    #[test]
    fn swap_column_left_single_column_returns_none() {
        // Negative: single column, no left neighbour to swap with
        let layout = VirtualLayout::with_columns(
            vec![Column::with_equal_rows(8, vec![WindowId(1), WindowId(2)])],
            0,
        );
        assert!(swap_column(&layout, WindowId(1), Direction::Left, &test_config()).is_none());
    }

    #[test]
    fn swap_column_right_at_last_column_returns_none() {
        // Negative: can't swap right from last column
        let layout = three_column_layout();
        assert!(swap_column(&layout, WindowId(3), Direction::Right, &test_config()).is_none());
    }

    // --- initialize_windows tests ---

    #[test]
    fn initialize_windows_empty_list() {
        // Positive: empty list → empty layout
        let layout = initialize_windows(&[], &test_config(), None);
        assert!(layout.columns.is_empty());
        assert_eq!(layout.viewport_offset, 0);
    }

    #[test]
    fn initialize_windows_single_window() {
        // Positive: single window → single column
        let layout = initialize_windows(&[WindowId(1)], &test_config(), None);
        assert_eq!(layout.columns.len(), 1);
        assert_eq!(layout.columns[0].rows, vec![WindowId(1)]);
        assert_eq!(layout.columns[0].width_eighths, 4); // default
        assert_eq!(layout.viewport_offset, 0);
    }

    #[test]
    fn initialize_windows_multiple_windows() {
        // Positive: multiple windows → multiple columns in order
        let layout = initialize_windows(
            &[WindowId(10), WindowId(20), WindowId(30)],
            &test_config(),
            None,
        );
        assert_eq!(layout.columns.len(), 3);
        assert_eq!(layout.columns[0].rows, vec![WindowId(10)]);
        assert_eq!(layout.columns[1].rows, vec![WindowId(20)]);
        assert_eq!(layout.columns[2].rows, vec![WindowId(30)]);
        // All columns get default width
        assert_eq!(layout.columns[0].width_eighths, 4);
        assert_eq!(layout.columns[1].width_eighths, 4);
        assert_eq!(layout.columns[2].width_eighths, 4);
        assert_eq!(layout.viewport_offset, 0);
    }

    // --- center_on_column tests ---

    #[test]
    fn center_on_column_first_column() {
        // Positive: centering on column 0 clamps to offset 0 because the column
        // is already visible from the left edge. 3 columns × 4 eighths (960px)
        // on a 1920px monitor.
        // col_center = 0 + 960/2 = 480
        // offset = 480 - 1920/2 = 480 - 960 = -480 → clamped to 0
        let layout = three_column_layout();
        let config = test_config();
        let result = center_on_column(&layout, 0, &config);
        assert_eq!(result.viewport_offset, 0);
    }

    #[test]
    fn center_on_column_middle_column() {
        // Positive: centering on column 1 centers it in the viewport.
        // col_center = 960 + 960/2 = 1440
        // offset = 1440 - 960 = 480
        let layout = three_column_layout();
        let config = test_config();
        let result = center_on_column(&layout, 1, &config);
        assert_eq!(result.viewport_offset, 480);
    }

    #[test]
    fn center_on_column_last_column() {
        // Positive: centering on the last column (index 2) scrolls right.
        // col_center = 1920 + 960/2 = 2400
        // offset = 2400 - 960 = 1440
        let layout = three_column_layout();
        let config = test_config();
        let result = center_on_column(&layout, 2, &config);
        assert_eq!(result.viewport_offset, 1440);
    }

    #[test]
    fn center_on_column_single_column() {
        // Positive: single column (960px canvas) on 1920px monitor →
        // entire canvas centered: offset = -(1920 - 960) / 2 = -480.
        // This places the single column at screen_x = 0 + (0 - (-480)) = 480,
        // centering it horizontally on the monitor.
        let layout = VirtualLayout::with_columns(vec![Column::new(4, WindowId(1))], 0);
        let config = test_config();
        let result = center_on_column(&layout, 0, &config);
        assert_eq!(result.viewport_offset, -480);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn center_on_column_out_of_bounds_panics() {
        // Negative: centering on a column index that exceeds the layout
        // should panic with an out-of-bounds assertion.
        let layout = three_column_layout();
        let config = test_config();
        let _ = center_on_column(&layout, 99, &config);
    }

    // --- initialize_windows with focus_col_idx tests ---

    #[test]
    fn initialize_windows_with_focus_produces_centered_viewport() {
        // Positive: passing focus_col_idx=Some(2) (last column of 3) produces
        // a viewport_offset > 0 because the camera centers on the last column.
        let layout = initialize_windows(
            &[WindowId(1), WindowId(2), WindowId(3)],
            &test_config(),
            Some(2),
        );
        assert_eq!(layout.columns.len(), 3);
        assert!(
            layout.viewport_offset > 0,
            "viewport should be centered on last column, not left-aligned"
        );
        // Expected: 1440 (col_center = 2400, offset = 2400 - 960 = 1440)
        assert_eq!(layout.viewport_offset, 1440);
    }

    #[test]
    fn initialize_windows_without_focus_produces_zero_offset() {
        // Positive: passing None for focus_col_idx → viewport_offset stays 0.
        let layout = initialize_windows(
            &[WindowId(1), WindowId(2), WindowId(3)],
            &test_config(),
            None,
        );
        assert_eq!(layout.viewport_offset, 0);
    }

    #[test]
    fn initialize_windows_with_out_of_bounds_focus_uses_zero_offset() {
        // Negative: focus_col_idx=Some(99) exceeds the 3 columns → graceful
        // fallback to viewport_offset 0 (no centering applied).
        let layout = initialize_windows(
            &[WindowId(1), WindowId(2), WindowId(3)],
            &test_config(),
            Some(99),
        );
        assert_eq!(
            layout.viewport_offset, 0,
            "out-of-bounds focus index should produce zero offset"
        );
    }

    #[test]
    fn initialize_windows_with_focus_on_first_column() {
        // Positive: focus_col_idx=Some(0) → clamped to 0 (first column visible
        // from left edge).
        let layout = initialize_windows(
            &[WindowId(1), WindowId(2), WindowId(3)],
            &test_config(),
            Some(0),
        );
        assert_eq!(layout.viewport_offset, 0);
    }
}
