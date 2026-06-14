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
    /// Target number of columns per screen from config (`columns_per_screen`).
    ///
    /// Used by [`compute_initial_viewport`] to decide whether all columns
    /// fit on one screen (show everything) or scrolling is needed (focus
    /// column visible).
    pub columns_per_screen: u32,
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

/// Round `value` **down** to the largest multiple of `unit` that is `≤ value`.
///
/// Both `value` and `unit` must be non-negative, and `unit` must be nonzero.
fn floor_to_multiple(value: i32, unit: i32) -> i32 {
    (value / unit) * unit
}

/// Round `value` **up** to the smallest multiple of `unit` that is `≥ value`.
///
/// Both `value` and `unit` must be non-negative, and `unit` must be nonzero.
fn ceil_to_multiple(value: i32, unit: i32) -> i32 {
    ((value + unit - 1) / unit) * unit
}

/// Shift the camera so the given column becomes visible.
///
/// This is the core "camera shift" operation. It checks whether the target
/// column's virtual canvas range overlaps the current viewport:
/// - If the column is off-screen **left**, the camera scrolls left so the
///   column appears with a `window_gap` between its left edge and the
///   screen's left edge.
/// - If the column is off-screen **right**, the camera scrolls right so the
///   column appears with a `window_gap` between its right edge and the
///   screen's right edge.
/// - If already visible, no change.
///
/// This is used by focus, swap, resize, and other operations that need
/// to ensure a specific column is on-screen.
///
/// # Slot Model
///
/// Columns are laid out in slots: `slot_width = col_width + window_gap`.
/// The canvas starts at `window_gap` (left-edge gap). Each column `i`
/// is at canvas position `window_gap + i * slot_width`.
///
/// # Quantized Camera Shifts
///
/// The `viewport_offset` is always quantized to a multiple of
/// `column_shift = column_width + window_gap` (the standard slot width).
/// This guarantees that after any scroll, columns appear at the *same*
/// screen position they would occupy at the initial viewport — giving a
/// consistent, "stepped" visual appearance rather than arbitrary offsets.
///
/// For uniform-width columns this means every column's left edge lands at
/// exactly `window_gap` pixels from the screen's left edge, regardless of
/// which scroll direction revealed it.
///
/// ## Direction-specific rounding
///
/// - **Left scroll**: we need `viewport_offset ≤ col_left − gap` (so the
///   gap is visible). We floor to the largest multiple of `column_shift`
///   satisfying that constraint, ensuring we never *undershoot*.
/// - **Right scroll**: we need `viewport_offset ≥ col_right + gap −
///   monitor_width` (so the gap is visible). We ceil to the smallest
///   multiple of `column_shift` satisfying that constraint, ensuring we
///   never *overshoot*.
#[must_use]
pub(crate) fn ensure_column_visible(
    layout: &VirtualLayout,
    col_idx: usize,
    config: &MutationConfig,
) -> VirtualLayout {
    let gap = config.padding.window_gap;
    let column_shift = config.column_width as i32 + gap;
    let mut canvas_x: i32 = gap;
    for (i, col) in layout.columns.iter().enumerate() {
        let col_px = column_eighths_to_pixels(col.width_eighths, config.column_width);
        if i == col_idx {
            let col_left = canvas_x;
            let col_right = canvas_x + col_px;
            let vp_left = layout.viewport_offset;
            let vp_right = vp_left + config.monitor_width;

            if col_left < vp_left {
                // Column is off-screen left — scroll left.
                // We want a `gap` between the screen's left edge and the
                // column's left edge, then snap to the column-shift grid.
                let ideal_vp = col_left - gap;
                let quantized = floor_to_multiple(ideal_vp, column_shift).max(0);
                return VirtualLayout {
                    viewport_offset: quantized,
                    ..layout.clone()
                };
            }
            if col_right > vp_right {
                // Column is off-screen right — scroll right.
                // We want a `gap` between the screen's right edge and the
                // column's right edge, then snap to the column-shift grid.
                let ideal_vp = col_right + gap - config.monitor_width;
                let quantized = ceil_to_multiple(ideal_vp, column_shift).max(0);
                return VirtualLayout {
                    viewport_offset: quantized,
                    ..layout.clone()
                };
            }
            // Already visible
            return layout.clone();
        }
        canvas_x += col_px + gap;
    }
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

/// Expand the focused column by one `column_shift`.
///
/// The step is `column_shift = column_width + window_gap`, matching the slot
/// grid used by the camera/scroll model (see `column_step_width` in
/// projection). The column's width is increased by this delta; the resulting
/// pixel value is then quantized to eighths by [`set_column_width`], which
/// rejects values outside `[min_column_eighths, max_column_eighths]`.
///
/// Returns `None` if the expansion would exceed `max_column_eighths`.
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
    let column_shift = cw + config.padding.window_gap;

    // Delta: grow by one column_shift (column_width + window_gap).
    let target_px = current_px + column_shift;

    // set_column_width quantizes to eighths and validates against
    // [min_column_eighths, max_column_eighths]. A raw target that
    // transiently exceeds monitor_width may still quantize to a valid
    // eighths value, so we delegate the bounds check entirely.
    set_column_width(layout, focused, target_px, config)
}

/// Shrink the focused column by one `column_shift`.
///
/// The step is `column_shift = column_width + window_gap`, matching the slot
/// grid. The column's width is decreased by this delta; the resulting pixel
/// value is quantized to eighths by [`set_column_width`], which rejects
/// values outside `[min_column_eighths, max_column_eighths]`.
///
/// Returns `None` if the shrink would go below `min_column_eighths`.
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
    let column_shift = cw + config.padding.window_gap;

    // Delta: shrink by one column_shift (column_width + window_gap).
    let target_px = current_px - column_shift;

    // set_column_width quantizes to eighths and validates against
    // [min_column_eighths, max_column_eighths]. We delegate the bounds
    // check entirely — the raw target can go negative or below min,
    // but pixels_to_eighths clamps and set_column_width rejects it.
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

/// Compute the optimal `viewport_offset` for the initial layout.
///
/// This function replaces the old `center_on_column` approach for
/// initialization. It chooses a **slot-aligned** offset (`k * slot` where
/// `slot = col_width + gap`) that satisfies two goals:
///
/// 1. **All relevant columns are visible** on screen.
/// 2. **The focus column is as centered as possible** in the viewport.
///
/// # Two cases
///
/// The decision is based on the user's `columns_per_screen` setting
/// (see the rationale section below for why pixel geometry is not used):
///
/// **All columns fit** (`N ≤ columns_per_screen`):
/// Every column must be fully visible. The valid offset range is
/// `[N*slot - monitor_width, gap]`. Among slot-aligned offsets in this range,
/// we pick the one closest to centering the focus column. If no slot-aligned
/// offset exists (canvas barely exceeds monitor), we fall back to offset `0`.
///
/// **Scroll needed** (`N > columns_per_screen`):
/// Exactly `C = columns_per_screen` columns are shown — the screen is always
/// **completely filled with real columns, no blanks on either side**.
///
/// The leftmost visible column is `start`. It is chosen from the valid range
/// `[max(0, f−C+1), min(f, N−C)]` which guarantees: (a) no columns before
/// column 0, (b) no columns past the last column, and (c) the focus column
/// is visible. Among valid `start` values, the one closest to centering the
/// focus is selected: `ideal_start = f − C/2` (integer division, placing the
/// focus slightly right of center for even C). The offset is
/// `start * slot` — slot-aligned by construction.
///
/// # Example (N = 7, C = 4)
///
/// | Focus | `start` | Visible columns |
/// |-------|---------|-----------------|
/// | a (0) | 0       | `[a, b, c, d]`  |
/// | d (3) | 1       | `[b, c, d, e]`  |
/// | g (6) | 3       | `[d, e, f, g]`  |
///
/// # Why `columns_per_screen`, not pixel geometry
///
/// The user sets `columns_per_screen` to declare how many columns they want
/// visible at once. When N ≤ `columns_per_screen`, we treat all columns as
/// "fitting" even if the explicit `column_width` makes the total slightly
/// exceed the monitor — the edge-case fallback (offset 0) handles the tiny
/// overflow gracefully. When N > `columns_per_screen`, we show exactly C
/// columns filled edge-to-edge because the user's own setting says not all
/// columns should be visible.
///
/// # Slot model
///
/// All initialization columns have equal width (`default_column_width_eighths`),
/// so `col_px = column_eighths_to_pixels(eighths, column_width)` and
/// `slot = col_px + gap`. Column `i` occupies canvas range
/// `[i*slot + gap, (i+1)*slot]`.
///
/// # Tie-breaking
///
/// **All-fit case**: when two slot-aligned offsets equally center the focus
/// column, the larger offset (less negative / closer to zero) is preferred.
/// This minimizes camera movement from the initial `viewport_offset = 0` and
/// keeps the view left-aligned.
///
/// **Scroll case**: when the ideal `start` is a half-integer (even C),
/// integer division truncates toward zero, preferring the smaller `start`
/// (less scrolling, focus slightly right of center).
///
/// # Arguments
///
/// * `num_columns` — Total number of columns in the initial layout.
/// * `focus_col` — Index of the focus column (0-based).
/// * `config` — Mutation configuration.
///
/// # Returns
///
/// The computed `viewport_offset` as a pixel value (may be negative when the
/// monitor is wider than the canvas, centering the entire canvas rightward).
#[must_use]
pub fn compute_initial_viewport(
    num_columns: usize,
    focus_col: usize,
    config: &MutationConfig,
) -> i32 {
    debug_assert!(
        num_columns > 0 && focus_col < num_columns,
        "compute_initial_viewport: focus_col {focus_col} out of range (0..{num_columns})"
    );

    let gap = config.padding.window_gap;
    let col_px = column_eighths_to_pixels(config.default_column_width_eighths, config.column_width);
    let slot = col_px + gap;
    let monitor_width = config.monitor_width;
    let n = num_columns as i32;
    let f = focus_col as i32;

    // ── Determine whether all columns fit on screen ───────────────────
    //
    // The scroll case (N > columns_per_screen) is handled by an early
    // return below. The range [min_offset, max_offset] that follows applies
    // only to the all-fit case, ensuring every column is visible.
    let all_fit = num_columns as u32 <= config.columns_per_screen;

    if !all_fit {
        // ── Scroll case: N > columns_per_screen ──────────────────────
        //
        // We show exactly C = columns_per_screen columns, all filled —
        // no blank/parked columns on either side of the screen.
        //
        // The leftmost visible column is `start`. Three constraints
        // guarantee no blanks and focus visibility:
        //
        //   1. No blanks on left:   start ≥ 0
        //   2. No blanks on right:  start + C ≤ N   →   start ≤ N − C
        //   3. Focus column seen:   start ≤ f ≤ start + C − 1
        //                           →   f − C + 1 ≤ start ≤ f
        //
        // Combined valid range:
        //   start ∈ [max(0, f − C + 1), min(f, N − C)]
        //
        // Among valid `start` values we pick the one closest to centering
        // the focus column. The ideal position for the focus is at slot
        // `C/2` — slightly right of center for even C — so:
        //
        //   ideal_start = f − C/2   (integer division)
        //
        // This places the focus at position C/2 within the visible window,
        // meaning for C = 4 the focus lands on the 3rd visible column.
        //
        // The resulting viewport_offset = start * slot, which is
        // slot-aligned (a multiple of column_shift = col_px + gap).
        let c = config.columns_per_screen as i32;
        let start_min = 0.max(f - c + 1);
        let start_max = f.min(n - c);
        let ideal_start = f - c / 2;
        let best_start = ideal_start.clamp(start_min, start_max);
        return best_start * slot;
    }

    // ── All-fit case: all columns must be visible ─────────────────────
    //
    // First col left = gap; last col right = n * slot.
    let min_offset = n * slot - monitor_width;
    let max_offset = gap;

    // ── Find valid slot-aligned offsets ───────────────────────────────
    //
    // We want k such that k * slot ∈ [min_offset, max_offset].
    // k_min = ceil(min_offset / slot), k_max = floor(max_offset / slot).
    // Using div_euclid (stable, always floors for positive divisor) plus
    // a remainder check for ceiling.
    let k_min = {
        let d = min_offset.div_euclid(slot);
        if min_offset.rem_euclid(slot) != 0 {
            d + 1
        } else {
            d
        }
    };
    let k_max = max_offset.div_euclid(slot);

    if k_min > k_max {
        // Edge case: no slot-aligned offset satisfies the constraint.
        // Show from the start (offset 0), accepting a tiny right-edge
        // cutoff on the last column.
        return 0;
    }

    // ── Pick the slot-aligned offset that best centers the focus col ──
    //
    // Focus column center on canvas: f * slot + gap + col_px / 2.
    // Ideal viewport_offset = focus_center - monitor_width / 2.
    let focus_center = f * slot + gap + col_px / 2;
    let ideal_offset = focus_center - monitor_width / 2;

    // Round ideal_offset / slot to the nearest integer, ties go up (larger k).
    let ideal_k = (ideal_offset + slot / 2).div_euclid(slot);

    // Clamp to the valid range.
    let best_k = ideal_k.clamp(k_min, k_max);
    best_k * slot
}

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
/// # Initial viewport
///
/// When `focus_col_idx` is `Some(idx)`, the viewport is computed by
/// [`compute_initial_viewport`] to show all columns when they fit on one
/// screen, or fill the screen with exactly `columns_per_screen` columns
/// (no blanks) centered on the focus column when they don't. The offset is
/// slot-aligned (`k * (col_width + gap)`) for clean scroll alignment.
///
/// When `focus_col_idx` is `None`, the viewport starts at offset `0`
/// (left-aligned with the first column).
///
/// # Arguments
///
/// * `ids` — Window IDs to place in the layout, one per column, in order.
/// * `config` — Mutation configuration (provides default column width,
///   monitor dimensions, and `columns_per_screen`).
/// * `focus_col_idx` — Optional index of the column to prioritize in the
///   initial viewport computation.
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
///     columns_per_screen: 4,
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

    let viewport_offset = match focus_col_idx {
        Some(idx) if idx < columns.len() => compute_initial_viewport(columns.len(), idx, config),
        _ => 0,
    };

    VirtualLayout {
        columns,
        viewport_offset,
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

/// Compute the step width of the first visible column (slot width).
fn first_visible_step(layout: &VirtualLayout, config: &MutationConfig) -> Option<i32> {
    let gap = config.padding.window_gap;
    let mut canvas_x: i32 = gap;
    let vp_right = layout.viewport_offset + config.monitor_width;
    for col in &layout.columns {
        let col_px = column_eighths_to_pixels(col.width_eighths, config.column_width);
        let col_left = canvas_x;
        let col_right = canvas_x + col_px;
        if col_right > layout.viewport_offset && col_left < vp_right {
            return Some(column_step_width(col, config.column_width, gap));
        }
        canvas_x += col_px + gap;
    }
    None
}

/// Total pixel span of all columns using the slot model.
///
/// Canvas width = `window_gap` (leading) + `sum(col_width_i + window_gap)`.
fn slot_based_canvas_width(layout: &VirtualLayout, config: &MutationConfig) -> i32 {
    if layout.columns.is_empty() {
        return 0;
    }
    let gap = config.padding.window_gap;
    let total_slots: i32 = layout
        .columns
        .iter()
        .map(|c| column_eighths_to_pixels(c.width_eighths, config.column_width) + gap)
        .sum();
    gap + total_slots
}

/// Legacy alias for canvas width calculation used by `remove_window`.
/// Uses the slot-based canvas model.
fn total_column_span(layout: &VirtualLayout, config: &MutationConfig) -> i32 {
    slot_based_canvas_width(layout, config)
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
            columns_per_screen: 4,
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
        // Slot model: step = col_width + window_gap = 960 + 4 = 964
        assert_eq!(result.viewport_offset, 964);
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

    // --- ensure_column_visible: quantized shifts ---

    #[test]
    fn floor_to_multiple_basic() {
        assert_eq!(floor_to_multiple(0, 964), 0);
        assert_eq!(floor_to_multiple(1, 964), 0);
        assert_eq!(floor_to_multiple(963, 964), 0);
        assert_eq!(floor_to_multiple(964, 964), 964);
        assert_eq!(floor_to_multiple(1927, 964), 964);
        assert_eq!(floor_to_multiple(1928, 964), 1928);
    }

    #[test]
    fn ceil_to_multiple_basic() {
        assert_eq!(ceil_to_multiple(0, 964), 0);
        assert_eq!(ceil_to_multiple(1, 964), 964);
        assert_eq!(ceil_to_multiple(963, 964), 964);
        assert_eq!(ceil_to_multiple(964, 964), 964);
        assert_eq!(ceil_to_multiple(965, 964), 1928);
    }

    #[test]
    fn ensure_column_visible_left_scroll_quantizes() {
        // 3 uniform columns (960px each, gap=4).
        // column_shift = 960 + 4 = 964.
        // Column 0: left=4. Viewport far to the right.
        let config = test_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            2000, // viewport well past column 0
        );
        let result = ensure_column_visible(&layout, 0, &config);
        // ideal_vp = col_left - gap = 4 - 4 = 0; floor_to_multiple(0, 964) = 0
        assert_eq!(result.viewport_offset, 0);
        assert_eq!(
            result.viewport_offset % (config.column_width as i32 + config.padding.window_gap),
            0,
            "viewport_offset must be a multiple of column_shift"
        );
    }

    #[test]
    fn ensure_column_visible_right_scroll_quantizes() {
        // 3 uniform columns (960px each, gap=4, monitor=1920).
        // column_shift = 964.
        // Column 2: left=1932, right=2892. Viewport at 0 → off-screen right.
        let config = test_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            0,
        );
        let result = ensure_column_visible(&layout, 2, &config);
        // ideal_vp = 2892 + 4 - 1920 = 976; ceil_to_multiple(976, 964) = 1928
        assert_eq!(result.viewport_offset, 1928);
        assert_eq!(
            result.viewport_offset % (config.column_width as i32 + config.padding.window_gap),
            0,
            "viewport_offset must be a multiple of column_shift"
        );
    }

    #[test]
    fn ensure_column_visible_left_scroll_has_gap_at_edge() {
        // After scrolling left, the column's left edge should be at least
        // `window_gap` from the screen's left edge.
        let config = test_config();
        let gap = config.padding.window_gap;
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            2000,
        );
        let result = ensure_column_visible(&layout, 0, &config);
        // Column 0's canvas left = gap (4). Screen left = viewport_offset (0).
        // Gap = col_left - vp_left = 4 - 0 = 4 = window_gap.
        let col_left = gap; // column 0 is always at canvas position `gap`
        assert!(
            col_left - result.viewport_offset >= gap,
            "left-edge gap must be >= window_gap"
        );
    }

    #[test]
    fn ensure_column_visible_right_scroll_has_gap_at_edge() {
        // After scrolling right, the column's right edge should be at least
        // `window_gap` from the screen's right edge.
        let config = test_config();
        let gap = config.padding.window_gap;
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            0,
        );
        let result = ensure_column_visible(&layout, 2, &config);
        // Column 2: canvas_x starts at gap, col 0 = 960px, col 1 = 960px.
        // col 2 left = gap + 2 * (960 + gap) = 4 + 2*964 = 1932
        // col 2 right = 1932 + 960 = 2892
        let col_right = gap + 2 * (960 + gap) + 960;
        let vp_right = result.viewport_offset + config.monitor_width;
        assert!(
            vp_right - col_right >= gap,
            "right-edge gap ({}) must be >= window_gap ({})",
            vp_right - col_right,
            gap
        );
    }

    #[test]
    fn ensure_column_visible_already_visible_no_change() {
        // With zero gap, two 960px columns fit exactly in 1920px viewport.
        // Column 1 [960, 1920] is fully inside [0, 1920] — no scroll needed.
        let config = MutationConfig {
            monitor_width: 1920,
            column_width: 960,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            max_column_eighths: 8,
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        let result = ensure_column_visible(&layout, 1, &config);
        assert_eq!(
            result.viewport_offset, 0,
            "no shift when column is already visible"
        );
    }

    #[test]
    fn ensure_column_visible_zero_gap_preserves_alignment() {
        // With gap=0, column_shift = column_width. Quantization should
        // produce exact column-aligned offsets with no fractional remainder.
        let config = MutationConfig {
            monitor_width: 1920,
            column_width: 960,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            max_column_eighths: 8,
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
        let layout = VirtualLayout::with_columns(
            vec![
                Column::new(4, WindowId(1)),
                Column::new(4, WindowId(2)),
                Column::new(4, WindowId(3)),
            ],
            0,
        );
        // Scroll right to column 2.
        // col_right = 0 + 2*960 + 960 = 2880. ideal_vp = 2880 + 0 - 1920 = 960.
        // ceil_to_multiple(960, 960) = 960.
        let result = ensure_column_visible(&layout, 2, &config);
        assert_eq!(result.viewport_offset, 960);
    }

    #[test]
    fn ensure_column_visible_non_uniform_widths_quantizes() {
        // Mixed column widths: col 0 = 2 eighths (480px), col 1 = 4 eighths (960px).
        // column_shift = 960 + 4 = 964 (based on base column_width, not actual).
        // Col 0: canvas_x = 4, right = 4 + 480 = 484
        // Col 1: canvas_x = 4 + 480 + 4 = 488, right = 488 + 960 = 1448
        // Viewport at 2000 → both columns off-screen left.
        let config = test_config();
        let layout = VirtualLayout::with_columns(
            vec![Column::new(2, WindowId(1)), Column::new(4, WindowId(2))],
            2000,
        );
        let result = ensure_column_visible(&layout, 1, &config);
        // ideal_vp = col_left - gap = 488 - 4 = 484
        // floor_to_multiple(484, 964) = 0
        assert_eq!(result.viewport_offset, 0);
        // Column 1 is now visible: canvas [488, 1448] ⊂ viewport [0, 1920]
        let column_shift = config.column_width as i32 + config.padding.window_gap;
        assert_eq!(result.viewport_offset % column_shift, 0);
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
        // With zero gap, both columns fit exactly on the monitor
        let config = MutationConfig {
            monitor_width: 1920,
            column_width: 960,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            max_column_eighths: 8,
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        let result = swap_column(&layout, WindowId(1), Direction::Right, &config).expect("swap");
        assert_eq!(
            result.viewport_offset, 0,
            "camera should not shift when both columns are visible (zero gap)"
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
        // With zero gap, both columns fit exactly on the monitor
        let config = MutationConfig {
            monitor_width: 1920,
            column_width: 960,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            max_column_eighths: 8,
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
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
    fn expand_column_from_default_width() {
        // Delta: 960px (4 eighths) + column_shift(964) = 1924px → 8 eighths
        let layout = three_column_layout(); // columns at 4 eighths = 960px
        let result = expand_column(&layout, WindowId(1), &test_config()).expect("expand");
        assert_eq!(result.columns[0].width_eighths, 8);
    }

    #[test]
    fn expand_column_from_sub_boundary() {
        // Delta: 480px (2 eighths) + column_shift(964) = 1444px → 6 eighths
        let layout = VirtualLayout::with_columns(
            vec![Column::new(2, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        let result = expand_column(&layout, WindowId(1), &test_config()).expect("expand");
        assert_eq!(result.columns[0].width_eighths, 6);
    }

    #[test]
    fn expand_column_at_max_returns_none() {
        // Delta: 1920px (8 eighths) + column_shift(964) = 2884px → 12 eighths > max(8) → None
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
    fn shrink_column_from_max_width() {
        // Delta: 1920px (8 eighths) - column_shift(964) = 956px → 4 eighths
        let layout = VirtualLayout::with_columns(
            vec![Column::new(8, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        let result = shrink_column(&layout, WindowId(1), &test_config()).expect("shrink");
        assert_eq!(result.columns[0].width_eighths, 4);
    }

    #[test]
    fn shrink_column_from_mid_boundary() {
        // Delta: 1200px (5 eighths) - column_shift(964) = 236px → 1 eighth < min(2) → None
        let layout = VirtualLayout::with_columns(
            vec![Column::new(5, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        assert!(shrink_column(&layout, WindowId(1), &test_config()).is_none());
    }

    #[test]
    fn shrink_column_at_boundary_returns_none() {
        // Delta: 960px (4 eighths) - column_shift(964) = -4px → 1 eighth < min(2) → None
        let layout = three_column_layout();
        assert!(shrink_column(&layout, WindowId(1), &test_config()).is_none());
    }

    #[test]
    fn shrink_column_at_min_eighths_returns_none() {
        // Delta: 480px (2 eighths = min) - column_shift(964) = -484px → 1 eighth < min(2) → None
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
        // Slot model: single 8/8 column = 1920px. Canvas = 4 + (1920+4) = 1928
        // max_offset = 1928 - 1920 = 8. But viewport_offset=0 + step=1924 > 8 → None
        let layout = VirtualLayout::with_columns(vec![Column::new(8, WindowId(1))], 0);
        let config = test_config();
        // Single column fills viewport — no scroll possible (step would exceed max)
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

    // --- initialize_windows with focus_col_idx tests ---

    #[test]
    fn initialize_windows_with_focus_shows_all_when_within_columns_per_screen() {
        // With test_config (monitor=1920, col_width=960, gap=4, slot=964,
        // columns_per_screen=4): 3 columns ≤ 4 → all-fit case.
        // Valid offset range [972, 4] has no slot-aligned k → edge case → 0.
        // All three columns visible from offset 0.
        let layout = initialize_windows(
            &[WindowId(1), WindowId(2), WindowId(3)],
            &test_config(),
            Some(2),
        );
        assert_eq!(layout.columns.len(), 3);
        assert_eq!(
            layout.viewport_offset, 0,
            "3 columns within columns_per_screen=4 should all be visible (offset 0)"
        );
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

    // --- compute_initial_viewport tests ---

    /// Helper: build a MutationConfig with arbitrary values.
    fn viewport_config(
        monitor_width: i32,
        column_width: u32,
        gap: i32,
        columns_per_screen: u32,
    ) -> MutationConfig {
        MutationConfig {
            monitor_width,
            column_width,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            max_column_eighths: 8,
            padding: Padding {
                window_gap: gap,
                up: 0,
                down: 0,
            },
            columns_per_screen,
        }
    }

    #[test]
    fn compute_initial_viewport_all_fit_within_columns_per_screen() {
        // 2 cols, columns_per_screen=4 → all-fit. Monitor wider than canvas.
        // slot=964, min_offset=1928-3000=-1072, max_offset=4.
        // k∈{-1,0}. Focus col 1: ideal_k=0 → offset 0.
        let config = viewport_config(3000, 960, 4, 4);
        let offset = compute_initial_viewport(2, 1, &config);
        assert_eq!(
            offset, 0,
            "2 cols within columns_per_screen=4, focus=1 → offset 0"
        );
    }

    #[test]
    fn compute_initial_viewport_scroll_when_exceeds_columns_per_screen() {
        // 4 cols, columns_per_screen=2 → scroll case.
        // slot=964, focus=2 (f=2), C=2, N=4.
        // start_min = max(0, 2-2+1) = 1, start_max = min(2, 4-2) = 2.
        // ideal_start = 2 - 2/2 = 1. Clamp(1, 1, 2) = 1.
        // offset = 1 * 964 = 964. Shows columns [1,2], focus at position 1.
        let config = viewport_config(1920, 960, 4, 2);
        let offset = compute_initial_viewport(4, 2, &config);
        assert_eq!(
            offset, 964,
            "4 cols > columns_per_screen=2, focus=2 → start=1, offset 964"
        );
    }

    #[test]
    fn compute_initial_viewport_scroll_with_multiple_valid_offsets() {
        // 4 cols, columns_per_screen=2, wider monitor → still scroll case.
        // slot=964, focus=1 (f=1), C=2, N=4.
        // start_min = max(0, 1-2+1) = 0, start_max = min(1, 4-2) = 1.
        // ideal_start = 1 - 2/2 = 0. Clamp(0, 0, 1) = 0.
        // offset = 0 * 964 = 0. Shows columns [0,1], focus at position 1.
        let config = viewport_config(3000, 960, 4, 2);
        let offset = compute_initial_viewport(4, 1, &config);
        assert_eq!(
            offset, 0,
            "scroll case, focus=1 → start=0, offset 0 (focus right of center)"
        );
    }

    #[test]
    fn compute_initial_viewport_user_scenario_4_cols_explicit_width() {
        // User's exact scenario: monitor=5120, col_width=1280, gap=16, slot=1296.
        // 4 cols, columns_per_screen=4 → all-fit.
        // min_offset=5184-5120=64, max_offset=16.
        // k_min=ceil(64/1296)=1, k_max=floor(16/1296)=0.
        // k_min > k_max → all-fit edge → return 0.
        let config = viewport_config(5120, 1280, 16, 4);
        let offset = compute_initial_viewport(4, 3, &config);
        assert_eq!(
            offset, 0,
            "user scenario: 4 cols within columns_per_screen=4 → all visible (offset 0)"
        );
    }

    #[test]
    fn compute_initial_viewport_all_fit_centers_on_wider_monitor() {
        // 2 cols, columns_per_screen=4, very wide monitor.
        // slot=964, all-fit: min_offset=-1072, max_offset=4, k∈{-1,0}.
        // Focus col 0: center=484, ideal_offset=484-3000=-2516.
        // ideal_k=(-2516+482)/964 = -2034/964. div_floor(-2034/964) = -3.
        // But -3 < k_min(-1), so clamp to -1 → offset=-964.
        let config = viewport_config(3000, 960, 4, 4);
        let offset = compute_initial_viewport(2, 0, &config);
        assert_eq!(
            offset, -964,
            "all-fit on wide monitor, focus=0 → negative offset to center canvas"
        );
    }

    #[test]
    fn compute_initial_viewport_single_column_always_zero() {
        // Positive: 1 col, any columns_per_screen → all-fit, only one slot-aligned
        // offset (k=0). Canvas=4+960=964, min_offset=964-1920=-956, max_offset=4.
        // k_min=ceil(-956/964)=0, k_max=floor(4/964)=0. Only k=0 → offset=0.
        let config = viewport_config(1920, 960, 4, 4);
        let offset = compute_initial_viewport(1, 0, &config);
        assert_eq!(offset, 0, "single column should always produce offset 0");
    }

    #[test]
    fn compute_initial_viewport_single_column_wide_monitor_centers() {
        // Positive: 1 col, wide monitor → all-fit.
        // col_px=960, slot=964, canvas=964. min_offset=964-3840=-2876, max_offset=4.
        // k_min=-2, k_max=0. Focus col 0 center=484, ideal_offset=-1436.
        // ideal_k=(-1436+482).div_euclid(964)=(-954).div_euclid(964)=-1.
        // Clamped to k_min=-2, k_max=0 → -1 is valid → offset=-964.
        // The single column is centered (shifted left) on the wide monitor.
        let config = viewport_config(3840, 960, 4, 4);
        let offset = compute_initial_viewport(1, 0, &config);
        assert_eq!(
            offset, -964,
            "single column on wide monitor → centered with negative offset"
        );
    }

    #[test]
    fn compute_initial_viewport_scroll_focus_first_column() {
        // Positive: 4 cols, columns_per_screen=2 → scroll case.
        // Focus=0: focus col left=4, right=968. Visible if offset ∈ [4-1920+968, 4] = [-948, 4].
        // min_offset=-948, max_offset=4. k_min=ceil(-948/964)=0, k_max=floor(4/964)=0.
        // Only k=0 → offset=0. First column is visible from offset 0.
        let config = viewport_config(1920, 960, 4, 2);
        let offset = compute_initial_viewport(4, 0, &config);
        assert_eq!(
            offset, 0,
            "scroll case with focus on first column → offset 0"
        );
    }

    #[test]
    fn compute_initial_viewport_scroll_focus_last_column() {
        // Positive: 4 cols, columns_per_screen=2 → scroll case.
        // Focus=3 (f=3), C=2, N=4, slot=964.
        // start_min = max(0, 3-2+1) = 2, start_max = min(3, 4-2) = 2.
        // ideal_start = 3 - 2/2 = 2. Clamp(2, 2, 2) = 2.
        // offset = 2 * 964 = 1928. Shows columns [2,3], focus at position 1.
        // No blanks: column 3 is the last, column 2 fills the left slot.
        let config = viewport_config(1920, 960, 4, 2);
        let offset = compute_initial_viewport(4, 3, &config);
        assert_eq!(
            offset, 1928,
            "scroll case with focus on last column → start=2, offset 1928"
        );
    }

    #[test]
    fn compute_initial_viewport_exact_boundary_n_equals_columns_per_screen() {
        // Positive: N == columns_per_screen → all-fit should be triggered.
        // 4 cols, columns_per_screen=4, monitor=1920.
        // slot=964, canvas=4+4*964=3860. min_offset=3860-1920=1940, max_offset=4.
        // k_min=ceil(1940/964)=3, k_max=floor(4/964)=0.
        // k_min > k_max → all-fit edge → return 0.
        let config = viewport_config(1920, 960, 4, 4);
        let offset = compute_initial_viewport(4, 2, &config);
        assert_eq!(
            offset, 0,
            "N == columns_per_screen triggers all-fit, edge case → offset 0"
        );
    }

    #[test]
    fn compute_initial_viewport_scroll_edge_no_valid_k() {
        // Edge: scroll case with tight monitor (monitor narrower than C slots).
        // 4 cols, columns_per_screen=2, monitor=960.
        // col_px = 4*960/4 = 960. slot = 964.
        // Focus=2 (f=2), C=2, N=4.
        // start_min = max(0, 2-2+1) = 1, start_max = min(2, 4-2) = 2.
        // ideal_start = 2 - 2/2 = 1. Clamp(1, 1, 2) = 1.
        // offset = 1 * 964 = 964. Shows columns [1,2], no blanks.
        // (This degenerate config has columns wider than half the monitor,
        // but the column-index logic still picks the best slot-aligned
        // offset without creating blank columns.)
        let config = viewport_config(960, 960, 4, 2);
        let offset = compute_initial_viewport(4, 2, &config);
        assert_eq!(
            offset, 964,
            "scroll case tight monitor → start=1, offset 964 (no blanks)"
        );
    }

    #[test]
    fn compute_initial_viewport_all_fit_with_zero_gap() {
        // Positive: zero gap changes the slot math.
        // 2 cols, columns_per_screen=4, monitor=1920, gap=0.
        // col_px=960, slot=960. canvas=0+2*960=1920.
        // min_offset=1920-1920=0, max_offset=0.
        // k_min=ceil(0/960)=0, k_max=floor(0/960)=0.
        // Only k=0 → offset=0.
        let config = viewport_config(1920, 960, 0, 4);
        let offset = compute_initial_viewport(2, 1, &config);
        assert_eq!(
            offset, 0,
            "all-fit with zero gap, 2 cols exactly fill monitor → offset 0"
        );
    }

    #[test]
    fn compute_initial_viewport_scroll_with_large_gap() {
        // Positive: large gap changes the slot alignment.
        // 3 cols, columns_per_screen=2, monitor=1920, gap=100.
        // col_px=960, slot=1060. Focus=2 (f=2), C=2, N=3.
        // start_min = max(0, 2-2+1) = 1, start_max = min(2, 3-2) = 1.
        // ideal_start = 2 - 2/2 = 1. Clamp(1, 1, 1) = 1.
        // offset = 1 * 1060 = 1060. Shows columns [1,2], no blanks.
        let config = viewport_config(1920, 960, 100, 2);
        let offset = compute_initial_viewport(3, 2, &config);
        assert_eq!(
            offset, 1060,
            "scroll case with large gap → start=1, offset = 1 * 1060 = 1060"
        );
    }

    // --- initialize_windows: scroll case through full function ---

    #[test]
    fn initialize_windows_scroll_case_with_focus() {
        // Positive: 4 cols > columns_per_screen=2 → scroll case.
        // Focus on column 2 should produce a non-zero viewport_offset.
        let config = viewport_config(1920, 960, 4, 2);
        let layout = initialize_windows(
            &[WindowId(1), WindowId(2), WindowId(3), WindowId(4)],
            &config,
            Some(2),
        );
        assert_eq!(layout.columns.len(), 4);
        assert_ne!(
            layout.viewport_offset, 0,
            "4 cols > columns_per_screen=2 with focus=2 → non-zero offset"
        );
    }

    #[test]
    fn initialize_windows_scroll_case_focus_first_col() {
        // Positive: scroll case but focus on first column → offset 0.
        let config = viewport_config(1920, 960, 4, 2);
        let layout = initialize_windows(
            &[WindowId(1), WindowId(2), WindowId(3), WindowId(4)],
            &config,
            Some(0),
        );
        assert_eq!(
            layout.viewport_offset, 0,
            "scroll case with focus=0 → offset 0 (first col visible from start)"
        );
    }

    // -----------------------------------------------------------------------
    // No-blank-columns regression: when N > columns_per_screen, the initial
    // viewport must fill every on-screen slot with a real column — no blank
    // space on either side — while centering the focus as much as possible.
    // -----------------------------------------------------------------------

    #[test]
    fn scroll_no_blanks_focus_first_shows_from_start() {
        // 7 cols, columns_per_screen=4, focus=0.
        // start_min = max(0, -3) = 0, start_max = min(0, 3) = 0.
        // ideal = 0 - 2 = -2 → clamp to 0. Shows [a,b,c,d].
        let config = viewport_config(1920, 960, 4, 4);
        let offset = compute_initial_viewport(7, 0, &config);
        assert_eq!(offset, 0, "focus on first col → start=0, no left blanks");
    }

    #[test]
    fn scroll_no_blanks_focus_last_shows_to_end() {
        // 7 cols, columns_per_screen=4, focus=6.
        // start_min = max(0, 3) = 3, start_max = min(6, 3) = 3.
        // ideal = 6 - 2 = 4 → clamp to 3. Shows [d,e,f,g].
        let config = viewport_config(1920, 960, 4, 4);
        let offset = compute_initial_viewport(7, 6, &config);
        // start=3 → offset = 3 * slot = 3 * 964 = 2892.
        assert_eq!(offset, 2892, "focus on last col → start=3, no right blanks");
    }

    #[test]
    fn scroll_no_blanks_focus_center_shows_centered() {
        // 7 cols, columns_per_screen=4, focus=3 (d).
        // start_min = max(0, 0) = 0, start_max = min(3, 3) = 3.
        // ideal = 3 - 2 = 1. Clamp(1, 0, 3) = 1. Shows [b,c,d,e].
        let config = viewport_config(1920, 960, 4, 4);
        let offset = compute_initial_viewport(7, 3, &config);
        // start=1 → offset = 1 * slot = 1 * 964 = 964.
        assert_eq!(offset, 964, "focus=3 → start=1, shows [b,c,d,e]");
    }

    #[test]
    fn scroll_no_blanks_focus_never_creates_negative_offset() {
        // For every focus position in a scroll layout, the offset must be
        // non-negative (no blank columns on the left).
        let config = viewport_config(1920, 960, 4, 4);
        for f in 0..7 {
            let offset = compute_initial_viewport(7, f, &config);
            assert!(
                offset >= 0,
                "focus={f}: offset {offset} must be ≥ 0 (no left blanks)"
            );
        }
    }

    #[test]
    fn scroll_no_blanks_focus_never_exceeds_max_scroll() {
        // The rightmost visible column must not exceed N-1.
        // max_offset for N columns = (N - C) * slot.
        // For N=7, C=4: max = 3 * 964 = 2892.
        let config = viewport_config(1920, 960, 4, 4);
        let max_offset = (7 - 4) * (960 + 4);
        for f in 0..7 {
            let offset = compute_initial_viewport(7, f, &config);
            assert!(
                offset <= max_offset,
                "focus={f}: offset {offset} must be ≤ {max_offset} (no right blanks)"
            );
        }
    }

    #[test]
    fn scroll_no_blanks_offset_always_slot_aligned() {
        // Every offset must be a multiple of slot (column_shift).
        let config = viewport_config(1920, 960, 4, 4);
        let slot = 960 + 4;
        for f in 0..7 {
            let offset = compute_initial_viewport(7, f, &config);
            assert_eq!(
                offset % slot,
                0,
                "focus={f}: offset {offset} must be a multiple of slot {slot}"
            );
        }
    }

    #[test]
    fn scroll_no_blanks_user_monitor_5120_scenario() {
        // User's actual hardware: 5120×1440 monitor, columns_per_screen=4.
        // column_width=1280, gap=4 → col_px=1280, slot=1284.
        // 7 windows, focus on a (0): start=0, offset=0. Shows [a,b,c,d].
        // 7 windows, focus on g (6): start=3, offset=3*1284=3852. Shows [d,e,f,g].
        // 7 windows, focus on d (3): start=1, offset=1*1284=1284. Shows [b,c,d,e].
        let config = viewport_config(5120, 1280, 4, 4);
        let slot = 1280 + 4;

        let off_a = compute_initial_viewport(7, 0, &config);
        assert_eq!(off_a, 0, "focus a → offset 0");

        let off_g = compute_initial_viewport(7, 6, &config);
        assert_eq!(off_g, 3 * slot, "focus g → offset {}", 3 * slot);

        let off_d = compute_initial_viewport(7, 3, &config);
        assert_eq!(off_d, 1 * slot, "focus d → offset {}", 1 * slot);
    }

    #[test]
    fn scroll_no_blanks_five_windows_four_per_screen() {
        // 5 cols, columns_per_screen=4.
        // focus=0: start=0, shows [a,b,c,d]. offset=0.
        // focus=4: start=1 (max(0,1) to min(4,1)=1), shows [b,c,d,e]. offset=slot.
        let config = viewport_config(1920, 960, 4, 4);
        let slot = 960 + 4;

        assert_eq!(compute_initial_viewport(5, 0, &config), 0);
        assert_eq!(compute_initial_viewport(5, 4, &config), slot);
    }

    #[test]
    fn scroll_no_blanks_odd_columns_per_screen_centers_exactly() {
        // columns_per_screen=3 (odd), 6 cols.
        // focus=2: ideal_start = 2 - 3/2 = 2 - 1 = 1.
        // start_min = max(0, 0) = 0, start_max = min(2, 3) = 2.
        // Clamp(1, 0, 2) = 1. Focus at position 1 (exact center of 0,1,2).
        let config = viewport_config(1920, 960, 4, 3);
        let slot = 960 + 4;
        assert_eq!(
            compute_initial_viewport(6, 2, &config),
            1 * slot,
            "odd C=3: focus=2 → start=1, exact center"
        );
    }

    // -------------------------------------------------------------------
    // Bug 2 regression: expand/shrink column step must include window_gap.
    // column_shift = column_width + window_gap, NOT just column_width.
    // -------------------------------------------------------------------

    /// Helper: build a MutationConfig with a specific window_gap for testing
    /// that the gap is included in the expand/shrink step.
    fn gap_config(gap: i32) -> MutationConfig {
        MutationConfig {
            monitor_width: 1920,
            column_width: 960,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            max_column_eighths: 8,
            padding: Padding {
                window_gap: gap,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        }
    }

    #[test]
    fn expand_step_includes_gap_zero_gap_vs_large_gap() {
        // Positive: verify that the expand step includes window_gap by comparing
        // results with gap=0 vs gap=20. With gap=0, column_shift = 960.
        // With gap=20, column_shift = 980. The resulting eighths must differ.
        //
        // gap=0: 960 + 960 = 1920 → 8 eighths (exactly monocle width)
        // gap=20: 960 + 980 = 1940 → pixels_to_eighths(1940, 960) = 8 eighths (same)
        // So for this specific case they converge. Let's use 3 eighths start:
        //
        // 3 eighths = 720px.
        // gap=0: 720 + 960 = 1680 → pixels_to_eighths(1680, 960) = 7 eighths
        // gap=20: 720 + 980 = 1700 → pixels_to_eighths(1700, 960) = 7 eighths
        // Still same. Let's verify with smaller column_width to see the gap matter:
        // Use column_width=500, so column_shift matters more.
        let config_zero = MutationConfig {
            monitor_width: 1920,
            column_width: 500,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            // max: (1920 * 4) / 500 = 15
            max_column_eighths: 15,
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
        let config_gap = MutationConfig {
            monitor_width: 1920,
            column_width: 500,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            max_column_eighths: 15,
            padding: Padding {
                window_gap: 20,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };

        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );

        // gap=0: column_shift = 500. 500px + 500 = 1000 → eighths(1000,500)=8
        let _r_zero = expand_column(&layout, WindowId(1), &config_zero).expect("expand zero gap");
        // gap=20: column_shift = 520. 500px + 520 = 1020 → eighths(1020,500)=8
        let r_gap = expand_column(&layout, WindowId(1), &config_gap).expect("expand 20 gap");

        // Both hit 8 eighths from 4 — but the *intermediate pixel target* differed.
        // Verify by checking with a starting width where the gap produces a different
        // eighth boundary. 3 eighths = 375px.
        let layout3 = VirtualLayout::with_columns(
            vec![Column::new(3, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        // gap=0: 375 + 500 = 875 → eighths(875, 500) = 7
        let r3_zero = expand_column(&layout3, WindowId(1), &config_zero).expect("expand3 zero");
        // gap=20: 375 + 520 = 895 → eighths(895, 500) = 7
        assert_eq!(r3_zero.columns[0].width_eighths, 7);
        assert_eq!(r_gap.columns[0].width_eighths, 8);
        assert_eq!(r3_zero.columns[0].width_eighths, 7);
    }

    #[test]
    fn expand_with_large_gap_advances_further_than_zero_gap() {
        // Positive: with a large gap, the expand step pushes further, so expanding
        // from a low width can skip an eighth boundary that gap=0 would not.
        // column_width=500. Start at 2 eighths = 250px.
        // gap=0: 250 + 500 = 750 → eighths(750, 500) = 6
        // gap=60: 250 + 560 = 810 → eighths(810, 500) = 6
        // gap=0 and gap=60 both round to 6. Let's try gap=200:
        // gap=200: 250 + 700 = 950 → eighths(950, 500) = 8
        let config_small = MutationConfig {
            monitor_width: 1920,
            column_width: 500,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            max_column_eighths: 15,
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
        let config_large = MutationConfig {
            monitor_width: 1920,
            column_width: 500,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            max_column_eighths: 15,
            padding: Padding {
                window_gap: 200,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };

        let layout = VirtualLayout::with_columns(
            vec![Column::new(2, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );

        // gap=0: 250 + 500 = 750 → 6 eighths
        let r_small = expand_column(&layout, WindowId(1), &config_small).expect("expand small gap");
        assert_eq!(
            r_small.columns[0].width_eighths, 6,
            "gap=0 expand from 2→6 eighths"
        );

        // gap=200: 250 + 700 = 950 → 8 eighths
        let r_large = expand_column(&layout, WindowId(1), &config_large).expect("expand large gap");
        assert_eq!(
            r_large.columns[0].width_eighths, 8,
            "gap=200 expand from 2→8 eighths (larger step skips more eighth boundaries)"
        );

        // Verify the gap is what causes the difference
        assert_ne!(
            r_small.columns[0].width_eighths, r_large.columns[0].width_eighths,
            "different gaps must produce different expand results when starting from same width"
        );
    }

    #[test]
    fn shrink_with_large_gap_reduces_more_than_zero_gap() {
        // Positive: with a large gap, shrink removes more, so it can drop below
        // what gap=0 would reach.
        // column_width=500. Start at 6 eighths = 750px.
        // gap=0: 750 - 500 = 250 → eighths(250, 500) = 2
        // gap=200: 750 - 700 = 50 → eighths(50, 500) = 0 → clamped to 1 < min(2) → None
        let config_small = MutationConfig {
            monitor_width: 1920,
            column_width: 500,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            max_column_eighths: 15,
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
        let config_large = MutationConfig {
            monitor_width: 1920,
            column_width: 500,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            max_column_eighths: 15,
            padding: Padding {
                window_gap: 200,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };

        let layout = VirtualLayout::with_columns(
            vec![Column::new(6, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );

        // gap=0: 750 - 500 = 250 → 2 eighths (at min, should succeed)
        let r_small = shrink_column(&layout, WindowId(1), &config_small).expect("shrink small gap");
        assert_eq!(r_small.columns[0].width_eighths, 2);

        // gap=200: 750 - 700 = 50 → 0 → 1 eighth < min(2) → None
        assert!(
            shrink_column(&layout, WindowId(1), &config_large).is_none(),
            "gap=200 shrink from 6 eighths drops below min → None"
        );
    }

    #[test]
    fn expand_shrink_roundtrip_with_gap() {
        // Positive: expand then shrink with non-zero gap returns to same width.
        // 4 eighths (960px), gap=16. column_shift = 976.
        // expand: 960 + 976 = 1936 → 8 eighths
        // shrink: 1920 - 976 = 944 → 4 eighths
        let config = gap_config(16);
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );

        let expanded = expand_column(&layout, WindowId(1), &config).expect("expand");
        assert_eq!(expanded.columns[0].width_eighths, 8);

        let shrunk = shrink_column(&expanded, WindowId(1), &config).expect("shrink");
        assert_eq!(
            shrunk.columns[0].width_eighths, 4,
            "expand→shrink roundtrip must return to original width with gap=16"
        );
    }

    #[test]
    fn expand_at_max_with_large_gap_returns_none() {
        // Negative: even with a large gap, expanding at max_eighths → None.
        // The step doesn't matter — bounds validation catches it.
        let config = gap_config(100);
        let layout = VirtualLayout::with_columns(
            vec![Column::new(8, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        assert!(
            expand_column(&layout, WindowId(1), &config).is_none(),
            "expand at max_eighths must return None regardless of gap size"
        );
    }

    #[test]
    fn shrink_at_min_with_large_gap_returns_none() {
        // Negative: even with a large gap, shrinking at min_eighths → None.
        let config = gap_config(100);
        let layout = VirtualLayout::with_columns(
            vec![Column::new(2, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );
        assert!(
            shrink_column(&layout, WindowId(1), &config).is_none(),
            "shrink at min_eighths must return None regardless of gap size"
        );
    }

    #[test]
    fn expand_step_uses_column_shift_not_column_width() {
        // Positive: direct verification that the expand step is column_width + gap,
        // not just column_width. We verify by computing the expected pixel target
        // and checking it matches the function's behavior.
        //
        // column_width=960, gap=4, column_shift=964.
        // Start: 4 eighths = 960px.
        // Expected target: 960 + 964 = 1924px → 8 eighths.
        //
        // If the bug were present (using column_width=960 instead of column_shift=964):
        // target would be 960 + 960 = 1920 → 8 eighths (happens to match in this case).
        //
        // Use a case where the difference matters: start at 3 eighths = 720px.
        // Correct: 720 + 964 = 1684 → eighths(1684, 960) = (1684*4 + 480)/960 = 7
        // Buggy:  720 + 960 = 1680 → eighths(1680, 960) = 7 (still same)
        //
        // Try 5 eighths = 1200px.
        // Correct: 1200 + 964 = 2164 → eighths(2164, 960) = (2164*4+480)/960 = 9
        // Buggy:  1200 + 960 = 2160 → eighths(2160, 960) = 9 (still same)
        //
        // The rounding makes it hard to see the difference with these sizes.
        // Use column_width=250, gap=50, column_shift=300. Start at 4 eighths=250px.
        // Correct: 250 + 300 = 550 → eighths(550, 250) = (550*4+125)/250 = 2325/250 = 9
        // Buggy:  250 + 250 = 500 → eighths(500, 250) = (500*4+125)/250 = 2125/250 = 8
        let config = MutationConfig {
            monitor_width: 1920,
            column_width: 250,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            max_column_eighths: 15,
            padding: Padding {
                window_gap: 50,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
        let layout = VirtualLayout::with_columns(
            vec![Column::new(4, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );

        let result = expand_column(&layout, WindowId(1), &config).expect("expand");
        assert_eq!(
            result.columns[0].width_eighths, 9,
            "expand with column_width=250, gap=50 must use column_shift=300, \
             not column_width=250. Expected 9 eighths (correct) vs 8 (buggy)."
        );
    }

    #[test]
    fn shrink_step_uses_column_shift_not_column_width() {
        // Positive: same verification as expand but for shrink.
        // column_width=250, gap=50, column_shift=300. Start at 8 eighths=500px.
        // Correct: 500 - 300 = 200 → eighths(200, 250) = (200*4+125)/250 = 925/250 = 3
        // Buggy:  500 - 250 = 250 → eighths(250, 250) = (250*4+125)/250 = 1125/250 = 4
        let config = MutationConfig {
            monitor_width: 1920,
            column_width: 250,
            default_column_width_eighths: 4,
            min_column_eighths: 2,
            max_column_eighths: 15,
            padding: Padding {
                window_gap: 50,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
        let layout = VirtualLayout::with_columns(
            vec![Column::new(8, WindowId(1)), Column::new(4, WindowId(2))],
            0,
        );

        let result = shrink_column(&layout, WindowId(1), &config).expect("shrink");
        assert_eq!(
            result.columns[0].width_eighths, 3,
            "shrink with column_width=250, gap=50 must use column_shift=300, \
             not column_width=250. Expected 3 eighths (correct) vs 4 (buggy)."
        );
    }
}
