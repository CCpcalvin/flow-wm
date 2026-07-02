//! Layout mutation operations — all pure functions.
//!
//! Every mutation takes a `&VirtualLayout` and returns a **new** `VirtualLayout`.
//! The layout is never mutated in place. This functional approach makes mutations
//! easy to test, compose, and reason about — there is no mutable state to get
//! out of sync.
//!
//! # Design principles
//!
//! - **Camera over window-shifting**: many operations that intuitively feel like
//!   "move windows" are implemented by adjusting `viewport_offset` (the camera)
//!   instead. For example, `ensure_column_visible` shifts the camera so a target
//!   column comes into view — no individual window positions are touched in the
//!   [`VirtualLayout`].
//! - **Focus-by-WindowId**: focus is tracked as a stable [`WindowId`], not a
//!   column/row index, so operations like column swapping require no focus fixup —
//!   the focused window id stays valid regardless of where it moves.
//!
//! # Container model
//!
//! A [`VirtualLayout`] is a horizontal container of [`Column`]s; each column is a
//! vertical container of rows stacked top-to-bottom. Horizontal operations
//! (Left/Right) swap/resize/scroll columns; vertical operations (Up/Down) swap or
//! focus rows within a column.
//!
//! # Sizing
//!
//! All size parameters come from [`MutationConfig`], derived from
//! [`FlowConfig`](crate::config::FlowConfig). The engine receives this (not the full
//! config) to stay decoupled from config parsing. Column widths are stored in
//! pixels ([`width_px`](crate::layout::types::Column::width_px)); expand/shrink
//! move along a discrete **slot ladder** that preserves `window_gap`, while
//! free-form widths (drag-resize, viewport offsets) are bounded but not snapped.
//!
//! See the developer guide's *Mutations Catalog* chapter
//! (`docs/src/dev-guide/layout/mutations.md`) for the full mutation table and the
//! slot-ladder width math.

use crate::common::{Direction, WindowId};
use crate::layout::projection::{canvas_width, column_step_width};
use crate::layout::types::{Column, Padding, Row, VirtualLayout};

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
/// Extracted from [`FlowConfig`](crate::config::FlowConfig) by the daemon.
/// The layout engine receives this (not the full config) to stay decoupled
/// from config parsing details.
///
/// # Width ladder (derived values)
///
/// `max_n` and `abs_max_width` are computed once by the engine (see
/// [`ScrollingSpace::new`](crate::workspace::ScrollingSpace::new)) from
/// `column_width`, `monitor_width`, and `padding.window_gap`, then stored here
/// so every mutation reuses the same ladder without recompute. The helper
/// methods [`column_shift`](Self::column_shift) and
/// [`slot_max`](Self::slot_max) express the two core ladder quantities.
#[derive(Debug, Clone, Copy)]
pub struct MutationConfig {
    /// Monitor pixel width (used for visibility checks and `abs_max_width`).
    pub monitor_width: i32,
    /// Monitor pixel height — the work area's vertical extent. Used by
    /// [`distribute_heights`] to compute per-row heights when column membership
    /// changes (`add_window_to_column`, `remove_window`, `merge_column`,
    /// `promote_window`). The projection layer does NOT consult this — it
    /// consumes [`Row::height`] verbatim.
    ///
    /// [`Row::height`]: crate::layout::types::Row::height
    pub monitor_height: i32,
    /// Minimum allowed per-row window height in pixels. Caps how many windows
    /// a single column can hold: `max_rows = available_height /
    /// min_window_height_px` (computed lazily by callers that need the cap).
    /// Adding a row that would push any row below this floor is rejected.
    pub min_window_height_px: u32,
    /// Base column width in pixels. New columns are created at this width, and
    /// it is the `n = 0` rung of the expand/shrink slot ladder.
    pub column_width: u32,
    /// Minimum allowed column width in pixels — the lower bound for free-form
    /// drag-resize ([`set_column_width`]). Expand/shrink do not go below
    /// `column_width` (ladder floor); values in
    /// `[min_column_width_px, column_width)` are reachable only via drag-resize.
    pub min_column_width_px: u32,
    /// Number of slot steps in the expand/shrink ladder. The regular ladder
    /// rungs are `column_width + n * column_shift` for `n ∈ [0, max_n]`.
    pub max_n: u32,
    /// Absolute maximum column width in pixels
    /// (`= monitor_width − 2 * window_gap`). This is the monocle width and the
    /// two-step top of the expand ladder (a jump beyond [`slot_max`]).
    ///
    /// [`slot_max`]: Self::slot_max
    pub abs_max_width: i32,
    /// Padding settings.
    pub padding: Padding,
    /// Target number of columns per screen from config (`columns_per_screen`).
    ///
    /// Retained for config compatibility. The variable-width centering logic
    /// uses `projection::canvas_width` rather than this field; see
    /// (`docs/src/dev-guide/layout/mutations.md`).
    pub columns_per_screen: u32,
}

impl MutationConfig {
    /// One expand/shrink slot step: `column_width + window_gap`.
    ///
    /// This is the gap-aware step that the old eighths quantization discarded.
    /// Expand grows by exactly this; shrink shrinks by exactly this.
    #[must_use]
    pub fn column_shift(&self) -> i32 {
        self.column_width as i32 + self.padding.window_gap
    }

    /// Largest regular ladder rung below [`abs_max_width`](Self::abs_max_width):
    /// `column_width + max_n * column_shift`. Expanding a column already at
    /// `slot_max` jumps to `abs_max_width` (the two-step top).
    #[must_use]
    pub fn slot_max(&self) -> i32 {
        self.column_width as i32 + self.max_n as i32 * self.column_shift()
    }

    /// Vertical pixel budget available for stacking rows in a column:
    /// `monitor_height − padding.up − padding.down`.
    ///
    /// This is the input to [`distribute_heights`] whenever row membership
    /// changes. Row heights are stored per-`Row` and consumed verbatim by
    /// projection, so this is recomputed only at mutation time.
    #[must_use]
    pub fn available_height(&self) -> i32 {
        self.monitor_height - self.padding.up - self.padding.down
    }
}

// ---------------------------------------------------------------------------
// Height distribution
// ---------------------------------------------------------------------------

/// Distribute `available` pixels evenly across `n` rows with `gap` between
/// them, returning one height per row (top-to-bottom order).
///
/// Formula: each row gets `(available − (n + 1) * gap) / n`. The leading
/// `(n + 1) * gap` accounts for the top edge gap, the bottom edge gap, and the
/// `n − 1` inter-row gaps. The remainder of the integer division is distributed
/// one pixel at a time to the **top `remainder` rows** so the total always
/// sums exactly to `available − (n + 1) * gap`.
///
/// # Contract
///
/// - `n == 0` → empty `Vec` (caller must not pass zero; checked defensively).
/// - Negative numerator (`available < (n + 1) * gap`) → all-zero heights.
///   This signals the column is over-stuffed relative to its monitor; callers
///   that enforce `min_window_height_px` will reject the operation before it
///   reaches this state.
///
/// This is the **only** place row heights are computed. Everywhere else —
/// `add_window_to_column`, `remove_window`, `merge_column`, `promote_window`,
/// `initialize_windows` — row membership changes call this and write the result
/// back into [`Row::height`]. Between mutations the heights are stable, which
/// is what unlocks future drag-resize of individual rows.
///
/// [`Row::height`]: crate::layout::types::Row::height
#[must_use]
pub fn distribute_heights(n: usize, available: i32, gap: i32) -> Vec<i32> {
    if n == 0 {
        return Vec::new();
    }
    let n_i = n as i32;
    let numerator = available - (n_i + 1) * gap;
    if numerator <= 0 {
        return vec![0; n];
    }
    let base = numerator / n_i;
    let remainder = (numerator % n_i) as usize;
    (0..n)
        .map(|i| if i < remainder { base + 1 } else { base })
        .collect()
}

/// Build a fresh single-row [`Column`] at the given width, with the row's
/// height set to the equal-share of `config.available_height()` for `n = 1`.
///
/// Used by every mutation that creates a new column (`add_window`,
/// `insert_window_after_focused[_with_width]`, `initialize_windows`,
/// `promote_window`). The row's height is the source-of-truth consumed verbatim
/// by projection.
fn single_row_column(window: WindowId, width_px: i32, config: &MutationConfig) -> Column {
    let h = distribute_heights(1, config.available_height(), config.padding.window_gap)[0];
    Column::with_row(width_px, Row::new(window, h))
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

/// Find the next window to focus after a window is removed.
///
/// Given the `(col, row)` of a window that is **about to be removed** (on the
/// layout *before* removal), returns the best candidate to receive focus:
///
/// 1. **Left column** — if `col > 0`, the window in column `col - 1` whose row
///    is closest to the removed window's row.
/// 2. **Right column** — otherwise, if a column exists to the right
///    (`col + 1 < columns.len()`), the window in column `col + 1` whose row is
///    closest to the removed window's row.
/// 3. **None** — if there is no column on either side (the removed window was
///    the only one in the layout).
///
/// # Design decision: horizontal neighbours only
///
/// This function deliberately considers only *horizontal* neighbours (adjacent
/// columns), not vertical siblings within the same column. In the scrolling
/// horizontal-canvas model, focus after removal snaps to an adjacent column so
/// the user's attention stays at roughly the same horizontal position. Windows
/// stacked in the same column as the removed one are *not* chosen even when
/// they exist — this matches the left-then-right preference the tiling manager
/// uses throughout its focus model.
///
/// # Column non-emptiness invariant
///
/// Every column in a [`VirtualLayout`] always contains at least one row: the
/// mutation API deletes a column the moment its last row is removed, and no
/// mutation ever creates an empty column. Therefore indexing into the chosen
/// neighbour's `rows` after [`closest_row`] is always safe.
///
/// # Arguments
///
/// * `layout` — the virtual layout **before** the window is removed.
/// * `col` — the column index of the window being removed.
/// * `row` — the row index of the window being removed (used to pick the
///   closest row in the neighbour column).
///
/// # Returns
///
/// The [`WindowId`] of the best focus candidate, or `None` if the layout has
/// no horizontal neighbours for the removed window.
///
/// # Examples
///
/// ```text
/// Col 0        Col 1        Col 2
/// [W1]         [W3]         [W5]
///              [W4]
///
/// Removing W3 (col 1, row 0) → left col 0, closest row → W1
/// Removing W1 (col 0, row 0) → no left, right col 1, closest row → W3
/// Removing W5 (col 2, row 0) → left col 1, closest row → W3
/// ```
#[must_use]
pub fn next_available_window(layout: &VirtualLayout, col: usize, row: usize) -> Option<WindowId> {
    // Prefer the column to the left.
    if col > 0 {
        let left = &layout.columns[col - 1];
        let r = closest_row(left, row);
        return Some(left.rows[r].window_id);
    }
    // Otherwise, fall back to the column to the right.
    let right_idx = col + 1;
    if right_idx < layout.columns.len() {
        let right = &layout.columns[right_idx];
        let r = closest_row(right, row);
        return Some(right.rows[r].window_id);
    }
    // No neighbour on either side — the removed window was the only one.
    None
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
    let target_window = layout.columns[neighbor.col].rows[neighbor.row].window_id;

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
/// # Slot model & free-form offset (Ripple #1)
///
/// Column x-positions are a prefix sum of each column's
/// [`width_px`](crate::layout::types::Column::width_px) plus `window_gap`. The
/// resulting `viewport_offset` is **free-form** — it is *not* snapped to the
/// `column_shift` grid. Snapping was only valid when every column had the
/// uniform base `column_width`; with variable widths (drag-resize, expand/
/// shrink) the grid would jump over the exact fit, so we place the camera at
/// the minimal scroll that reveals the target column with a one-gap margin.
/// Each branch scrolls the **minimum** amount in its direction:
///
/// - **Left scroll**: `viewport_offset = col_left − gap` (column's left edge
///   lands `gap` pixels from the screen's left edge), clamped to `≥ 0`.
/// - **Right scroll**: `viewport_offset = col_right + gap − monitor_width`
///   (column's right edge lands `gap` pixels from the screen's right edge),
///   clamped to `≥ 0`.
#[must_use]
pub(crate) fn ensure_column_visible(
    layout: &VirtualLayout,
    col_idx: usize,
    config: &MutationConfig,
) -> VirtualLayout {
    let gap = config.padding.window_gap;
    let mut canvas_x: i32 = gap;
    for (i, col) in layout.columns.iter().enumerate() {
        let col_px = col.width_px;
        if i == col_idx {
            let col_left = canvas_x;
            let col_right = canvas_x + col_px;
            let vp_left = layout.viewport_offset;
            let vp_right = vp_left + config.monitor_width;

            if col_left < vp_left {
                // Column is off-screen left — scroll left by the minimum amount
                // that reveals it with a `gap` margin on the left. Free-form
                // offset (no column_shift grid snapping — see Ripple #1).
                let ideal_vp = (col_left - gap).max(0);
                return VirtualLayout {
                    viewport_offset: ideal_vp,
                    ..layout.clone()
                };
            }
            if col_right > vp_right {
                // Column is off-screen right — scroll right by the minimum amount
                // that reveals it with a `gap` margin on the right. Free-form
                // offset (no column_shift grid snapping — see Ripple #1).
                let ideal_vp = (col_right + gap - config.monitor_width).max(0);
                return VirtualLayout {
                    viewport_offset: ideal_vp,
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
        // Same column — swap rows directly. Row items (including their
        // heights) travel with the window; redistribution is not needed
        // because the row count is unchanged.
        new_layout.columns[src_col].rows.swap(src_row, dst_row);
    } else {
        // Different columns — exchange the entire `Row` items (window id
        // AND its height) between the two positions. Heights therefore
        // travel with the window across columns too, preserving any
        // future drag-resize state. Row counts in each column are
        // unchanged, so no redistribution is required.
        let dst_row_val = new_layout.columns[dst_col].rows[dst_row];
        let src_row_val = new_layout.columns[src_col].rows[src_row];
        new_layout.columns[dst_col].rows[dst_row] = src_row_val;
        new_layout.columns[src_col].rows[src_row] = dst_row_val;
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

/// Set the focused column width to an explicit pixel value (free-form).
///
/// This is the **drag-resize primitive** — the target is applied directly
/// (not snapped to the expand/shrink slot ladder), bounded only by
/// `[min_column_width_px, abs_max_width]`. After applying, the column is kept
/// visible via [`ensure_column_visible`].
///
/// Returns `None` if the focused window is not found, the target equals the
/// current width, or the target is out of bounds.
#[must_use]
pub fn set_column_width(
    layout: &VirtualLayout,
    focused: WindowId,
    target_px: i32,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let (col, _) = layout.find_window(focused)?;

    // Free-form bounds: drag-resize may land anywhere in this range.
    let min = config.min_column_width_px as i32;
    let max = config.abs_max_width;
    if target_px < min || target_px > max {
        return None;
    }

    // No change — nothing to do
    if target_px == layout.columns[col].width_px {
        return None;
    }

    let mut new_layout = layout.clone();
    new_layout.columns[col].width_px = target_px;

    Some(ensure_column_visible(&new_layout, col, config))
}

/// Resize the focused column by a pixel delta (free-form).
///
/// Computes `target_px = current_px + delta_px` and delegates to
/// [`set_column_width`]. Positive delta grows the column, negative
/// shrinks it. The result is bounded (not ladder-snapped).
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
    let current_px = layout.columns[col].width_px;
    let target_px = current_px + delta_px;
    set_column_width(layout, focused, target_px, config)
}

/// Expand the focused column by one slot on the width ladder (F4).
///
/// The ladder is
/// `{ column_width + n * column_shift : n ∈ [0, max_n] } ∪ { abs_max_width }`
/// where `column_shift = column_width + window_gap`. **Crucially, the gap is
/// part of the step** — this is the bug fix: under the old eighths model the
/// gap was smaller than one eighth and was rounded away, so an expand step
/// moved by exactly `column_width` instead of `column_width + window_gap`.
///
/// Behaviour from a column of width `W`:
/// - `W ≥ abs_max_width` → `None` (no-op; already at the absolute top).
/// - `W ≥ slot_max` → jump to `abs_max_width` (the two-step top; the leftover
///   pixels between `slot_max` and `abs_max_width` are too small for a full
///   `column_shift`).
/// - otherwise → advance one rung: `target = column_width + (n+1) * column_shift`
///   where `n = floor((W − column_width) / column_shift)`.
///
/// Returns `None` if the focused window is not found or the column is already
/// at `abs_max_width`.
#[must_use]
pub fn expand_column(
    layout: &VirtualLayout,
    focused: WindowId,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let (col, _) = layout.find_window(focused)?;
    let w = layout.columns[col].width_px;
    let base = config.column_width as i32;
    let shift = config.column_shift();
    let abs_max = config.abs_max_width;
    debug_assert!(shift > 0, "column_shift must be positive");

    // Already at (or beyond) the absolute top — no-op.
    if w >= abs_max {
        return None;
    }
    // Two-step top: from the top regular slot (or the free-form gap just below
    // abs_max), jump straight to abs_max_width.
    if w >= config.slot_max() {
        return set_column_width(layout, focused, abs_max, config);
    }

    // Advance one rung. floor handles columns sitting between rungs (e.g. a
    // free-form drag-resize width) by snapping up to the next boundary.
    let n = (w - base).div_euclid(shift);
    let target_n = (n + 1).min(config.max_n as i32);
    let target_px = base + target_n * shift;
    set_column_width(layout, focused, target_px, config)
}

/// Shrink the focused column by one slot on the width ladder (F4).
///
/// The mirror of [`expand_column`]. Behaviour from a column of width `W`:
/// - `W ≤ column_width` → `None` (no-op; already at the ladder floor).
/// - otherwise → descend one rung to `target = column_width + (n−1) * column_shift`
///   where `n = ceil((W − column_width) / column_shift)`. `ceil` snaps a
///   between-rung (or `abs_max_width`) width down to the next lower boundary.
///   From `abs_max_width` this lands on `slot_max` (the reverse of expand's
///   two-step top), or one rung below when `abs_max == slot_max`.
///
/// Note: shrink never goes below `column_width` (the ladder floor). Widths in
/// `[min_column_width_px, column_width)` are reachable only via drag-resize
/// ([`set_column_width`]).
///
/// Returns `None` if the focused window is not found or the column is already
/// at `column_width`.
#[must_use]
pub fn shrink_column(
    layout: &VirtualLayout,
    focused: WindowId,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let (col, _) = layout.find_window(focused)?;
    let w = layout.columns[col].width_px;
    let base = config.column_width as i32;
    let shift = config.column_shift();
    debug_assert!(shift > 0, "column_shift must be positive");

    // At or below the ladder floor — no-op.
    if w <= base {
        return None;
    }

    // Descend one rung to the next lower slot boundary. This also handles the
    // `abs_max_width` case: ceil((abs_max - base) / shift) is `max_n + 1` when
    // `abs_max > slot_max` (lands on `slot_max` — the reverse of expand's
    // two-step top), and `max_n` when `abs_max == slot_max` (lands one rung
    // down). The previous explicit `abs_max → slot_max` branch routed through
    // `set_column_width`, which no-oped when `slot_max == abs_max` because the
    // target equalled the current width (Bug 2).
    let n = ((w - base) + shift - 1).div_euclid(shift);
    let target_n = (n - 1).max(0);
    let target_px = base + target_n * shift;
    set_column_width(layout, focused, target_px, config)
}

// ---------------------------------------------------------------------------
// Monocle toggle
// ---------------------------------------------------------------------------

/// Toggle monocle mode — expand the focused window's column to `abs_max_width`.
///
/// Entering monocle sets the column to
/// [`abs_max_width`](MutationConfig::abs_max_width) (`monitor_width − 2*gap`)
/// and saves the previous width so it can be restored. Exiting monocle
/// restores the saved width (falling back to the base `column_width`).
///
/// Returns the new layout and the new saved-width state:
/// - On enter: `Some((layout, Some(previous_width)))`.
/// - On exit: `Some((layout, None))`.
#[must_use]
pub fn toggle_monocle(
    layout: &VirtualLayout,
    focused: WindowId,
    saved_width: Option<i32>,
    config: &MutationConfig,
) -> Option<(VirtualLayout, Option<i32>)> {
    let (col, _) = layout.find_window(focused)?;
    let current = layout.columns[col].width_px;

    if current == config.abs_max_width {
        // Already monocle — restore previous width (default: base column_width).
        let restored = saved_width.unwrap_or(config.column_width as i32);
        let mut new_layout = layout.clone();
        new_layout.columns[col].width_px = restored;
        Some((new_layout, None))
    } else {
        // Enter monocle.
        let mut new_layout = layout.clone();
        new_layout.columns[col].width_px = config.abs_max_width;
        Some((new_layout, Some(current)))
    }
}

// ---------------------------------------------------------------------------
// Viewport center primitives (prefix-sum, variable-width aware)
// ---------------------------------------------------------------------------

/// Compute the viewport offset that centers the focused column at the
/// monitor midpoint, using actual column widths from the layout.
///
/// Always computes `canvas_x(focused) − (monitor_width − focused_width) / 2`,
/// even when all columns fit on screen — this is the **explicit center**
/// command behavior. The `canvas_x` prefix-sum uses the slot model:
/// `Σ(width[i] for i in 0..f) + (f + 1) * window_gap`. See
/// (`docs/src/dev-guide/layout/mutations.md`).
#[must_use]
pub fn center_viewport_on_focused(
    layout: &VirtualLayout,
    focus_col: usize,
    config: &MutationConfig,
) -> i32 {
    debug_assert!(
        !layout.columns.is_empty() && focus_col < layout.columns.len(),
        "focus_col {focus_col} out of range (0..{})",
        layout.columns.len()
    );

    let gap = config.padding.window_gap;
    let focused_width = layout.columns[focus_col].width_px;

    // canvas_x(f) = Σ(width[i] for i in 0..f) + (f + 1) * window_gap
    let canvas_x: i32 = layout.columns[..focus_col]
        .iter()
        .map(|c| c.width_px)
        .sum::<i32>()
        + (focus_col as i32 + 1) * gap;

    canvas_x - (config.monitor_width - focused_width) / 2
}

/// Compute the viewport offset that centers the entire canvas within the
/// monitor.
///
/// `(canvas_width − monitor_width) / 2` — may be **negative** when the
/// canvas is narrower than the monitor (projection already supports
/// negative offsets). Used by the init and MoveWindowToWorkspace fit cases.
/// See (`docs/src/dev-guide/layout/mutations.md`).
#[must_use]
pub fn center_viewport_canvas(layout: &VirtualLayout, config: &MutationConfig) -> i32 {
    let cw = canvas_width(layout, config.padding.window_gap);
    (cw - config.monitor_width) / 2
}

/// Quantize a raw pixel width to the nearest slot-ladder rung, clamped to
/// `[column_width, abs_max_width]`.
///
/// The ladder rungs are `column_width + n * column_shift` for `n ∈ [0,
/// max_n]`, plus `abs_max_width` (the two-step top). Between-rung values
/// snap to the nearest rung using the same rounding policy as
/// [`expand_column`]/[`shrink_column`].
pub(crate) fn quantize_to_ladder(raw_width: i32, config: &MutationConfig) -> i32 {
    let base = config.column_width as i32;
    let shift = config.column_shift();
    let abs_max = config.abs_max_width;

    // Clamp to [column_width, abs_max_width] first.
    let clamped = raw_width.clamp(base, abs_max);

    // If at or below the base rung, it's the base.
    if clamped <= base {
        return base;
    }
    // If at or above abs_max, it's abs_max.
    if clamped >= abs_max {
        return abs_max;
    }
    // If in the two-step gap (slot_max..abs_max), snap to nearest.
    if clamped > config.slot_max() {
        // Midpoint between slot_max and abs_max decides direction.
        let mid = (config.slot_max() + abs_max) / 2;
        return if clamped >= mid {
            abs_max
        } else {
            config.slot_max()
        };
    }

    // Regular ladder: nearest rung n = round((clamped - base) / shift).
    let n = ((clamped - base) + shift / 2) / shift;
    let target_n = n.clamp(0, config.max_n as i32);
    base + target_n * shift
}

/// Build a complete virtual layout from a list of window IDs.
///
/// Creates one column per window. When `widths` is `Some`, each width is
/// quantized to the nearest slot-ladder rung and clamped to
/// `[column_width, abs_max_width]`. When `None`, every column gets the base
/// [`column_width`](MutationConfig::column_width).
///
/// The initial viewport uses the actual canvas width (not
/// `columns_per_screen`) to decide fit vs. overflow:
/// - **Fit** (canvas ≤ monitor): centers the entire canvas.
/// - **Overflow** with focus: ensures the focus column is visible.
/// - No focus: offset `0`.
///
/// See (`docs/src/dev-guide/layout/mutations.md`).
#[must_use]
pub fn initialize_windows(
    ids: &[WindowId],
    config: &MutationConfig,
    focus_col_idx: Option<usize>,
    widths: Option<&[u32]>,
) -> VirtualLayout {
    let columns: Vec<Column> = ids
        .iter()
        .enumerate()
        .map(|(i, &id)| {
            let width = match widths {
                Some(ws) => quantize_to_ladder(ws[i] as i32, config),
                None => config.column_width as i32,
            };
            single_row_column(id, width, config)
        })
        .collect();

    let viewport_offset = if columns.is_empty() {
        0
    } else {
        let gap = config.padding.window_gap;
        let temp_layout = VirtualLayout::with_columns(columns.clone(), 0);
        let cw = canvas_width(&temp_layout, gap);
        if cw <= config.monitor_width {
            // Fit: center the entire canvas.
            center_viewport_canvas(&temp_layout, config)
        } else if let Some(idx) = focus_col_idx {
            if idx < columns.len() {
                ensure_column_visible(&temp_layout, idx, config).viewport_offset
            } else {
                0
            }
        } else {
            0
        }
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
    new_layout.columns.push(single_row_column(
        window,
        config.column_width as i32,
        config,
    ));
    new_layout
}

/// Insert a new window as a column immediately **after** the focused window's
/// column.
///
/// This is the window-creation placement strategy: rather than appending the
/// new window to the far right end of the layout (which [`add_window`] does),
/// this inserts it right next to the focused window so the user sees their
/// newly opened window adjacent to where they were working.
///
/// # How "make space" works
///
/// In the virtual layout, columns are stored in a `Vec` and their canvas
/// positions are derived from that order. Inserting a new column at index
/// `focused_col + 1` pushes every column to the right of the insertion point
/// rightward by exactly one `column_shift` (= `column_width + window_gap`) on
/// the canvas — no manual coordinate math is needed. This is the "make space"
/// step the placement algorithm describes.
///
/// # Edge cases
///
/// - **No focused window** (empty layout, first window): the new column is
///   appended at the end, which for an empty layout means it becomes the sole
///   column.
/// - **Focused window is the last column**: insertion at `col + 1` is
///   equivalent to appending at the end.
/// - **Focused window not found** (stale focus): falls back to appending at
///   the end.
///
/// # Viewport
///
/// After insertion, [`ensure_column_visible`] is called on the new column's
/// index to bring it into the viewport. If the new column is already visible
/// (e.g. there is room on screen), the viewport is unchanged. If it is
/// off-screen right, the camera scrolls right by a quantized `column_shift`.
///
/// The caller is responsible for setting focus to the new window — this
/// function only computes the layout.
#[must_use]
pub fn insert_window_after_focused(
    layout: &VirtualLayout,
    focused: Option<WindowId>,
    window: WindowId,
    config: &MutationConfig,
) -> VirtualLayout {
    let mut new_layout = layout.clone();
    let new_column = single_row_column(window, config.column_width as i32, config);

    // Determine the insertion index: immediately after the focused column.
    // Falls back to the end when there is no focus or the focused window is
    // no longer present (stale focus / empty layout).
    let insert_idx = focused
        .and_then(|f| layout.find_window(f))
        .map(|(col, _)| col + 1)
        .unwrap_or_else(|| new_layout.columns.len());

    new_layout.columns.insert(insert_idx, new_column);

    ensure_column_visible(&new_layout, insert_idx, config)
}

/// Insert a new window as a column after the focused window, at an explicit
/// (already quantized) width.
///
/// Identical to [`insert_window_after_focused`] except the new column's
/// `width_px` is set to `width` directly instead of the base
/// [`column_width`](MutationConfig::column_width). The caller is responsible
/// for quantizing/clamping `width` beforehand (e.g. via
/// [`quantize_to_ladder`]). See (`docs/src/dev-guide/layout/mutations.md`).
#[must_use]
pub fn insert_window_after_focused_with_width(
    layout: &VirtualLayout,
    focused: Option<WindowId>,
    window: WindowId,
    width: i32,
    config: &MutationConfig,
) -> VirtualLayout {
    let mut new_layout = layout.clone();
    let new_column = single_row_column(window, width, config);

    let insert_idx = focused
        .and_then(|f| layout.find_window(f))
        .map(|(col, _)| col + 1)
        .unwrap_or_else(|| new_layout.columns.len());

    new_layout.columns.insert(insert_idx, new_column);

    ensure_column_visible(&new_layout, insert_idx, config)
}

/// Add a window to an existing column as a new row (vertical container append).
///
/// The column's rows are redistributed to equal heights via
/// [`distribute_heights`] so the new window fits alongside its siblings.
/// Returns the layout unchanged (no panic) if `col_idx` is out of bounds.
#[must_use]
pub fn add_window_to_column(
    layout: &VirtualLayout,
    col_idx: usize,
    window: WindowId,
    config: &MutationConfig,
) -> VirtualLayout {
    let mut new_layout = layout.clone();
    let Some(col) = new_layout.columns.get_mut(col_idx) else {
        return new_layout;
    };
    let n = col.rows.len() + 1;
    let heights = distribute_heights(n, config.available_height(), config.padding.window_gap);
    for (i, row) in col.rows.iter_mut().enumerate() {
        row.height = heights[i];
    }
    col.rows.push(Row::new(window, heights[n - 1]));
    new_layout
}

/// Remove a window from the layout.
///
/// If the column still has rows remaining after removal, those rows are
/// redistributed to equal heights via [`distribute_heights`]. If the column
/// becomes empty, the column itself is removed and the viewport adjusts if
/// needed.
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
    } else {
        // Column still has rows — redistribute equally across the new count.
        let n = col_ref.rows.len();
        let heights = distribute_heights(n, config.available_height(), config.padding.window_gap);
        for (i, r) in col_ref.rows.iter_mut().enumerate() {
            r.height = heights[i];
        }
    }

    // Clamp viewport offset using actual config values
    let total = total_column_span(&new_layout, config);
    let max_offset = (total - config.monitor_width).max(0);
    new_layout.viewport_offset = new_layout.viewport_offset.min(max_offset);

    new_layout
}

// ---------------------------------------------------------------------------
// Merge / promote
// ---------------------------------------------------------------------------

/// Merge the focused window into an adjacent column (Left/Right).
///
/// The focused window is **removed** from its current column and appended as
/// a new row at the bottom of the neighbor column indicated by `direction`.
/// Both columns then have their rows redistributed to equal heights via
/// [`distribute_heights`]:
/// - the destination column grows by one row, so its heights are recomputed
///   for `n + 1` rows;
/// - the source column (if it still has rows) shrinks by one row, so its
///   heights are recomputed for `n − 1` rows.
///
/// If the source column becomes empty (its only row was the focused window),
/// the column itself is removed and the destination column's index may shift
/// down by one — this is handled internally.
///
/// # Min-height guard
///
/// Before applying, the redistribution for `n + 1` rows in the destination
/// column is computed speculatively; if any resulting row height would fall
/// below [`min_window_height_px`](MutationConfig::min_window_height_px), the
/// merge is rejected with a warning log and `None` is returned. This keeps
/// the invariant that no row ever ends up shorter than the configured floor,
/// which is what naturally caps how many windows a column can hold.
///
/// # Vertical directions
///
/// Vertical directions (`Up`/`Down`) are not meaningful at the column level
/// and always return `None` (logged at warning level), matching
/// [`swap_column`]'s behavior. Use [`swap_window`] for vertical motion
/// within a column.
///
/// Focus is tracked by [`WindowId`], so after the merge the focused window
/// is still the same window id — it now lives at the bottom of the
/// destination column.
///
/// Returns `None` if:
/// - the focused window is not found,
/// - the direction is vertical,
/// - there is no adjacent column in the requested direction,
/// - the resulting redistribution would violate `min_window_height_px`.
#[must_use]
pub fn merge_column(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let (src_col, src_row) = layout.find_window(focused)?;

    let dst_col = match direction {
        Direction::Left => {
            if src_col == 0 {
                return None;
            }
            src_col - 1
        }
        Direction::Right => {
            if src_col + 1 >= layout.columns.len() {
                return None;
            }
            src_col + 1
        }
        Direction::Up | Direction::Down => {
            log::warn!(
                "merge_column: vertical direction ({direction:?}) is invalid — \
                 merge only crosses column boundaries; use swap_window for vertical motion"
            );
            return None;
        }
    };

    // Speculative redistribution: would the destination column still respect
    // `min_window_height_px` once the focused window joins? This is the only
    // check that can reject an otherwise-valid merge.
    let gap = config.padding.window_gap;
    let new_count = layout.columns[dst_col].rows.len() + 1;
    let heights = distribute_heights(new_count, config.available_height(), gap);
    let floor = config.min_window_height_px as i32;
    if heights.iter().any(|&h| h < floor) {
        log::warn!(
            "merge_column: destination would have {new_count} rows; at least one \
             redistributed height is below min_window_height_px ({floor}px) — rejected"
        );
        return None;
    }

    // Detach the focused row from the source column.
    let mut new_layout = layout.clone();
    let moved_row = new_layout.columns[src_col].rows.remove(src_row);

    let src_emptied = new_layout.columns[src_col].rows.is_empty();
    if src_emptied {
        // Source column had only the focused window — drop the column
        // entirely. The destination column's index may shift down by one if
        // the source was to its left.
        new_layout.columns.remove(src_col);
    } else {
        // Source column still has rows — redistribute them for the new
        // (smaller) row count so the freed vertical space is shared equally.
        let n = new_layout.columns[src_col].rows.len();
        let redistributed = distribute_heights(n, config.available_height(), gap);
        for (i, r) in new_layout.columns[src_col].rows.iter_mut().enumerate() {
            r.height = redistributed[i];
        }
    }

    // If the source column was to the left of the destination (i.e. direction
    // was Right) and has just been removed, the destination column shifts
    // down to src_col's old index.
    let dst_after_removal = if direction == Direction::Right && src_emptied {
        src_col
    } else {
        dst_col
    };

    // Apply the pre-computed redistribution to the destination column and
    // append the moved window as the new bottom row.
    let dst = &mut new_layout.columns[dst_after_removal];
    for (i, r) in dst.rows.iter_mut().enumerate() {
        r.height = heights[i];
    }
    dst.rows
        .push(Row::new(moved_row.window_id, heights[new_count - 1]));

    Some(ensure_column_visible(
        &new_layout,
        dst_after_removal,
        config,
    ))
}

/// Promote the focused window into its own column (Left/Right).
///
/// The focused window is **extracted** from its current column and inserted
/// as a new single-row column immediately to the left (`Left`) or right
/// (`Right`) of the source column. The source column's remaining rows are
/// redistributed to equal heights via [`distribute_heights`] so the freed
/// vertical space is shared equally among them.
///
/// # No-op for single-row columns
///
/// If the focused window is already the only row in its column, the
/// operation is a no-op and returns `None`: the window is already in its
/// own column, so there is nothing to promote. This matches the user-facing
/// spec ("It should be a no-op when the column we are in has only ONE
/// window").
///
/// # Vertical directions
///
/// Vertical directions (`Up`/`Down`) are not meaningful at the column level
/// and always return `None` (logged at warning level), matching
/// [`merge_column`] and [`swap_column`]. Promotion only creates columns to
/// the left or right.
///
/// Focus is tracked by [`WindowId`], so after promotion the focused window
/// is still the same window id — it now lives alone in the new column.
///
/// Returns `None` if:
/// - the focused window is not found,
/// - the source column has only one row (already promoted),
/// - the direction is vertical.
#[must_use]
pub fn promote_window(
    layout: &VirtualLayout,
    focused: WindowId,
    direction: Direction,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let (src_col, src_row) = layout.find_window(focused)?;

    // Already in its own column — nothing to promote.
    if layout.columns[src_col].rows.len() <= 1 {
        return None;
    }

    let insert_at = match direction {
        Direction::Left => src_col,
        Direction::Right => src_col + 1,
        Direction::Up | Direction::Down => {
            log::warn!(
                "promote_window: vertical direction ({direction:?}) is invalid — \
                 promotion only creates columns to the left or right"
            );
            return None;
        }
    };

    let gap = config.padding.window_gap;
    let mut new_layout = layout.clone();

    // Detach the focused row.
    let moved_row = new_layout.columns[src_col].rows.remove(src_row);

    // Redistribute the source column's remaining rows so the freed vertical
    // space is shared equally.
    let n = new_layout.columns[src_col].rows.len();
    let redistributed = distribute_heights(n, config.available_height(), gap);
    for (i, r) in new_layout.columns[src_col].rows.iter_mut().enumerate() {
        r.height = redistributed[i];
    }

    // Build a fresh single-row column for the promoted window at the base
    // `column_width`. Its row height is the equal share for n=1 (full
    // available height) via `single_row_column`.
    let new_column = single_row_column(moved_row.window_id, config.column_width as i32, config);

    new_layout.columns.insert(insert_at, new_column);

    Some(ensure_column_visible(&new_layout, insert_at, config))
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
        let col_px = col.width_px;
        let col_left = canvas_x;
        let col_right = canvas_x + col_px;
        if col_right > layout.viewport_offset && col_left < vp_right {
            return Some(column_step_width(col, gap));
        }
        canvas_x += col_px + gap;
    }
    None
}

/// Total pixel span of all columns (the virtual canvas width).
///
/// Delegates to [`projection::canvas_width`] so the canvas model lives in
/// exactly one place: `window_gap` (leading) + `sum(width_px + window_gap)`.
fn total_column_span(layout: &VirtualLayout, config: &MutationConfig) -> i32 {
    canvas_width(layout, config.padding.window_gap)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::WindowId;
    use crate::layout::types::Padding;

    /// Standard test config: 1920×1080 monitor, 960px columns, 4px gap.
    ///
    /// Derived: abs_max=1912, shift=964, max_n=0, slot_max=960.
    /// With max_n=0 the ladder has only the base rung (960) and the
    /// abs_max top (1912) — expanding from base jumps straight to abs_max.
    fn test_config() -> MutationConfig {
        MutationConfig {
            monitor_width: 1920,
            monitor_height: 1080,
            min_window_height_px: 100,
            column_width: 960,
            min_column_width_px: 480,
            max_n: 0,
            abs_max_width: 1912,
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
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            0,
        )
    }

    // --- distribute_heights (direct unit tests) ---
    //
    // distribute_heights is the ONLY place row heights are computed. Every
    // mutation that changes row membership (add/remove/merge/promote) routes
    // through it. Although those mutations exercise the formula indirectly,
    // the branching logic (n=0, negative numerator, remainder distribution
    // to the TOP rows) deserves direct pins — a regression in the remainder
    // order would silently redistribute pixels to the wrong rows.

    #[test]
    fn distribute_heights_zero_count_returns_empty_vec() {
        // Contract: n == 0 → empty Vec (caller must not pass zero).
        let result = distribute_heights(0, 1080, 4);
        assert!(result.is_empty(), "n=0 must return empty Vec");
    }

    #[test]
    fn distribute_heights_one_row_gets_full_available_minus_gaps() {
        // n=1: numerator = available - 2*gap. No remainder possible.
        // available=1080, gap=4 → numerator = 1080 - 8 = 1072 → [1072].
        let result = distribute_heights(1, 1080, 4);
        assert_eq!(result, vec![1072]);
    }

    #[test]
    fn distribute_heights_two_rows_even_split_no_remainder() {
        // n=2: numerator = 1080 - 3*4 = 1068. 1068 / 2 = 534, rem 0 → [534, 534].
        let result = distribute_heights(2, 1080, 4);
        assert_eq!(result, vec![534, 534]);
    }

    #[test]
    fn distribute_heights_three_rows_distributes_remainder_to_top() {
        // n=3: numerator = 1080 - 4*4 = 1064. 1064 / 3 = 354, rem 2.
        // Top `rem` rows get +1 → [355, 355, 354].
        // This pins the "top gets +1" ordering documented in the contract.
        let result = distribute_heights(3, 1080, 4);
        assert_eq!(result, vec![355, 355, 354]);
    }

    #[test]
    fn distribute_heights_four_rows_even_split_no_remainder() {
        // n=4: numerator = 1080 - 5*4 = 1060. 1060 / 4 = 265, rem 0.
        let result = distribute_heights(4, 1080, 4);
        assert_eq!(result, vec![265, 265, 265, 265]);
    }

    #[test]
    fn distribute_heights_negative_numerator_returns_all_zeros() {
        // Contract: when available < (n+1)*gap the numerator goes negative.
        // available=10, gap=4, n=3 → numerator = 10 - 16 = -6 ≤ 0 → [0, 0, 0].
        let result = distribute_heights(3, 10, 4);
        assert_eq!(result, vec![0, 0, 0]);
    }

    #[test]
    fn distribute_heights_insufficient_budget_returns_all_zeros() {
        // Large n where (n+1)*gap alone exceeds available.
        // available=20, gap=4, n=10 → numerator = 20 - 44 = -24 ≤ 0 → zeros.
        let result = distribute_heights(10, 20, 4);
        assert_eq!(result, vec![0; 10]);
    }

    #[test]
    fn distribute_heights_zero_gap_distributes_evenly() {
        // Edge: gap=0 → numerator = available. n=3, available=1080 → 360 each.
        let result = distribute_heights(3, 1080, 0);
        assert_eq!(result, vec![360, 360, 360]);
    }

    #[test]
    fn distribute_heights_sums_to_available_minus_gap_overhead() {
        // Property: sum(result) == available - (n+1)*gap (when numerator > 0).
        // This is the invariant callers rely on: no pixels are lost or invented.
        let n = 5;
        let available = 1080;
        let gap = 7;
        let result = distribute_heights(n, available, gap);
        let expected_sum = available - (n as i32 + 1) * gap;
        let actual_sum: i32 = result.iter().sum();
        assert_eq!(
            actual_sum, expected_sum,
            "sum of heights must equal available - (n+1)*gap"
        );
        assert_eq!(result.len(), n);
    }

    // --- next_available_window ---

    #[test]
    fn next_available_prefers_left_column() {
        // Layout: [W1][W2][W3]; removing W2 (col 1, row 0) → left col 0 → W1.
        let layout = three_column_layout();
        assert_eq!(next_available_window(&layout, 1, 0), Some(WindowId(1)));
    }

    #[test]
    fn next_available_leftmost_falls_to_right() {
        // Removing W1 (col 0) has no left → right col 1 → W2.
        let layout = three_column_layout();
        assert_eq!(next_available_window(&layout, 0, 0), Some(WindowId(2)));
    }

    #[test]
    fn next_available_rightmost_falls_to_left() {
        // Removing W3 (col 2) → left col 1 → W2.
        let layout = three_column_layout();
        assert_eq!(next_available_window(&layout, 2, 0), Some(WindowId(2)));
    }

    #[test]
    fn next_available_only_window_returns_none() {
        // Single column [W1]; removing W1 (col 0) → no neighbours → None.
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(960, Row::new(WindowId(1), 0))], 0);
        assert_eq!(next_available_window(&layout, 0, 0), None);
    }

    #[test]
    fn next_available_clamps_row_to_neighbour() {
        // Layout: col 0 has 1 row [W1], col 1 has 2 rows [W2, W3].
        // Removing a window at (col 1, row 1): left col 0 has only 1 row, so
        // the closest row clamps to 0 → W1.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(2), 0), Row::new(WindowId(3), 0)],
                ),
            ],
            0,
        );
        assert_eq!(next_available_window(&layout, 1, 1), Some(WindowId(1)));
    }

    #[test]
    fn next_available_picks_closest_row_in_left_column() {
        // Layout: col 0 has 2 rows [W1, W4], col 1 has 3 rows [W2, W3, W5].
        // Removing (col 1, row 2): left col 0, closest row clamps 2 → 1 → W4.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(1), 0), Row::new(WindowId(4), 0)],
                ),
                Column::with_rows(
                    960,
                    vec![
                        Row::new(WindowId(2), 0),
                        Row::new(WindowId(3), 0),
                        Row::new(WindowId(5), 0),
                    ],
                ),
            ],
            0,
        );
        assert_eq!(next_available_window(&layout, 1, 2), Some(WindowId(4)));
    }

    #[test]
    fn next_available_single_column_multirow_returns_none() {
        // Negative: single column with multiple rows — no horizontal neighbours
        // exist, so the result is None even though vertical siblings (W1, W3)
        // remain in the same column. This validates the "horizontal neighbours
        // only" design decision documented in next_available_window's docstring.
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                960,
                vec![
                    Row::new(WindowId(1), 0),
                    Row::new(WindowId(2), 0),
                    Row::new(WindowId(3), 0),
                ],
            )],
            0,
        );
        // Removing W2 at (col 0, row 1): no left, no right → None.
        assert_eq!(next_available_window(&layout, 0, 1), None);
        // Removing W1 at (col 0, row 0): same result.
        assert_eq!(next_available_window(&layout, 0, 0), None);
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

    // --- ensure_column_visible: free-form offsets (Ripple #1) ---

    #[test]
    fn ensure_column_visible_left_scroll() {
        // 3 uniform columns (960px each, gap=4).
        // column_shift = 960 + 4 = 964.
        // Column 0: left=4. Viewport far to the right.
        let config = test_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            2000, // viewport well past column 0
        );
        let result = ensure_column_visible(&layout, 0, &config);
        // Free-form (Ripple #1): ideal_vp = col_left - gap = 4 - 4 = 0.
        assert_eq!(result.viewport_offset, 0);
    }

    #[test]
    fn ensure_column_visible_right_scroll() {
        // 3 uniform columns (960px each, gap=4, monitor=1920).
        // Column 2: left=1932, right=2892. Viewport at 0 → off-screen right.
        let config = test_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            0,
        );
        let result = ensure_column_visible(&layout, 2, &config);
        // Free-form (Ripple #1): ideal_vp = 2892 + 4 - 1920 = 976.
        assert_eq!(result.viewport_offset, 976);
    }

    #[test]
    fn ensure_column_visible_left_scroll_has_gap_at_edge() {
        // After scrolling left, the column's left edge should be at least
        // `window_gap` from the screen's left edge.
        let config = test_config();
        let gap = config.padding.window_gap;
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
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
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
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
            monitor_height: 1080,
            min_window_height_px: 100,
            column_width: 960,
            min_column_width_px: 480,
            max_n: 1,
            abs_max_width: 1920,
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let result = ensure_column_visible(&layout, 1, &config);
        assert_eq!(
            result.viewport_offset, 0,
            "no shift when column is already visible"
        );
    }

    #[test]
    fn ensure_column_visible_zero_gap_exact_alignment() {
        // With gap=0, column_shift = column_width. Two 960px columns fit exactly
        // in 1920px viewport. Scrolling to show column 2 of 3.
        let config = MutationConfig {
            monitor_width: 1920,
            monitor_height: 1080,
            min_window_height_px: 100,
            column_width: 960,
            min_column_width_px: 480,
            max_n: 1,
            abs_max_width: 1920,
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            0,
        );
        // col_right = 0 + 2*960 + 960 = 2880. ideal_vp = 2880 + 0 - 1920 = 960.
        let result = ensure_column_visible(&layout, 2, &config);
        assert_eq!(result.viewport_offset, 960);
    }

    #[test]
    fn ensure_column_visible_non_uniform_widths() {
        // Mixed column widths: col 0 = 480px, col 1 = 960px.
        // Col 0: canvas_x = 4, right = 4 + 480 = 484
        // Col 1: canvas_x = 4 + 480 + 4 = 488, right = 488 + 960 = 1448
        // Viewport at 2000 → both columns off-screen left.
        let config = test_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(480, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            2000,
        );
        let result = ensure_column_visible(&layout, 1, &config);
        // Free-form (Ripple #1): ideal_vp = col_left - gap = 488 - 4 = 484.
        assert_eq!(result.viewport_offset, 484);
        // Column 1 is now visible: canvas [488, 1448] ⊂ viewport [484, 2404]
        let col_left = config.padding.window_gap + 480 + config.padding.window_gap;
        let col_right = col_left + 960;
        assert!(col_right <= result.viewport_offset + config.monitor_width);
        assert!(col_left >= result.viewport_offset);
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
            vec![Column::with_rows(
                1920,
                vec![
                    Row::new(WindowId(1), 0),
                    Row::new(WindowId(2), 0),
                    Row::new(WindowId(3), 0),
                ],
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
        // 4 columns × 960px each, viewport = 1920, only 2 visible at a time
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
                Column::with_row(960, Row::new(WindowId(4), 0)),
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
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_rows(
                    960,
                    vec![
                        Row::new(WindowId(2), 0),
                        Row::new(WindowId(3), 0),
                        Row::new(WindowId(4), 0),
                    ],
                ),
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
        assert_eq!(result.columns[0].rows[0].window_id, WindowId(2));
        assert_eq!(result.columns[1].rows[0].window_id, WindowId(1));
        assert_eq!(result.columns[2].rows[0].window_id, WindowId(3));
    }

    #[test]
    fn swap_column_down_returns_none() {
        // Vertical directions are invalid for column-level swap
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
            0,
        );
        assert!(swap_column(&layout, WindowId(1), Direction::Down, &test_config()).is_none());
    }

    #[test]
    fn swap_column_up_returns_none() {
        // Vertical directions are invalid for column-level swap
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
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
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
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
            monitor_height: 1080,
            min_window_height_px: 100,
            column_width: 960,
            min_column_width_px: 480,
            max_n: 1,            // (1920-960)/960 = 1
            abs_max_width: 1920, // 1920 - 2*0
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
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
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(2), 0), Row::new(WindowId(3), 0)],
                ),
            ],
            0,
        );
        let result =
            swap_window(&layout, WindowId(1), Direction::Right, &test_config()).expect("swap");
        assert_eq!(result.columns[0].rows, vec![Row::new(WindowId(2), 0)]);
        assert_eq!(
            result.columns[1].rows,
            vec![Row::new(WindowId(1), 0), Row::new(WindowId(3), 0)]
        );
    }

    #[test]
    fn swap_window_left_swaps_with_neighbor_in_prev_column() {
        // [W1, W2] [W3] → swap_window left on W3 → [W1, W3] [W2]
        // W3 is at row 0 in col 1. Closest row in col 0 is row 0 = W1.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
                ),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            0,
        );
        let result =
            swap_window(&layout, WindowId(3), Direction::Left, &test_config()).expect("swap");
        assert_eq!(
            result.columns[0].rows,
            vec![Row::new(WindowId(3), 0), Row::new(WindowId(2), 0)]
        );
        assert_eq!(result.columns[1].rows, vec![Row::new(WindowId(1), 0)]);
    }

    #[test]
    fn swap_window_down_same_column() {
        // [W1, W2] in same column → swap_window down on W1 → [W2, W1]
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
            0,
        );
        let result =
            swap_window(&layout, WindowId(1), Direction::Down, &test_config()).expect("swap");
        assert_eq!(
            result.columns[0].rows,
            vec![Row::new(WindowId(2), 0), Row::new(WindowId(1), 0)]
        );
    }

    #[test]
    fn swap_window_picks_closest_row_in_target_column() {
        // Col 0: [W1, W2] (2 rows)
        // Col 1: [W3] (1 row)
        // W2 is at row 1. Closest row in col 1 (clamped) = row 0 = W3.
        // swap_window right on W2 → W2 goes to col 1 row 0, W3 goes to col 0 row 1.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
                ),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            0,
        );
        let result =
            swap_window(&layout, WindowId(2), Direction::Right, &test_config()).expect("swap");
        assert_eq!(
            result.columns[0].rows,
            vec![Row::new(WindowId(1), 0), Row::new(WindowId(3), 0)]
        );
        assert_eq!(result.columns[1].rows, vec![Row::new(WindowId(2), 0)]);
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
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
            0,
        );
        assert!(swap_window(&layout, WindowId(2), Direction::Down, &test_config()).is_none());
    }

    #[test]
    fn swap_window_up_same_column() {
        // [W1, W2] → swap_window up on W2 → [W2, W1]
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
            0,
        );
        let result =
            swap_window(&layout, WindowId(2), Direction::Up, &test_config()).expect("swap");
        assert_eq!(
            result.columns[0].rows,
            vec![Row::new(WindowId(2), 0), Row::new(WindowId(1), 0)]
        );
    }

    #[test]
    fn swap_window_up_at_first_row_returns_none() {
        // Negative: W1 at row 0, swap_window up → None
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
            0,
        );
        assert!(swap_window(&layout, WindowId(1), Direction::Up, &test_config()).is_none());
    }

    #[test]
    fn swap_window_shifts_viewport_when_target_offscreen() {
        let config = test_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
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
            monitor_height: 1080,
            min_window_height_px: 100,
            column_width: 960,
            min_column_width_px: 480,
            max_n: 1,            // (1920-960)/960 = 1
            abs_max_width: 1920, // 1920 - 2*0
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
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
        assert_eq!(
            layout.columns[neighbor.col].rows[neighbor.row].window_id,
            WindowId(2)
        );
    }

    #[test]
    fn find_neighbor_left_returns_first_row_in_prev_column() {
        let layout = three_column_layout();
        let neighbor = find_neighbor_window(&layout, WindowId(2), Direction::Left).expect("left");
        assert_eq!(neighbor, NeighborLocation { col: 0, row: 0 });
        assert_eq!(
            layout.columns[neighbor.col].rows[neighbor.row].window_id,
            WindowId(1)
        );
    }

    #[test]
    fn find_neighbor_up_returns_prev_row_same_column() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
            0,
        );
        let neighbor = find_neighbor_window(&layout, WindowId(2), Direction::Up).expect("up");
        assert_eq!(neighbor, NeighborLocation { col: 0, row: 0 });
    }

    #[test]
    fn find_neighbor_down_returns_next_row_same_column() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
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
                Column::with_rows(
                    960,
                    vec![
                        Row::new(WindowId(1), 0),
                        Row::new(WindowId(2), 0),
                        Row::new(WindowId(3), 0),
                    ],
                ),
                Column::with_row(960, Row::new(WindowId(4), 0)),
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
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
            0,
        );
        assert!(find_neighbor_window(&layout, WindowId(1), Direction::Up).is_none());
    }

    #[test]
    fn find_neighbor_down_at_last_row_returns_none() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
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
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_rows(
                    960,
                    vec![
                        Row::new(WindowId(2), 0),
                        Row::new(WindowId(3), 0),
                        Row::new(WindowId(4), 0),
                    ],
                ),
            ],
            0,
        );
        let neighbor = find_neighbor_window(&layout, WindowId(1), Direction::Right).expect("right");
        assert_eq!(neighbor, NeighborLocation { col: 1, row: 0 });
        assert_eq!(
            layout.columns[neighbor.col].rows[neighbor.row].window_id,
            WindowId(2)
        );
    }

    #[test]
    fn find_neighbor_multirow_matching_row_in_both_columns() {
        // Both columns have 3 rows. W5 at row 1 in col 0 → right neighbor = row 1 in col 1 = W8.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_rows(
                    960,
                    vec![
                        Row::new(WindowId(1), 0),
                        Row::new(WindowId(5), 0),
                        Row::new(WindowId(9), 0),
                    ],
                ),
                Column::with_rows(
                    960,
                    vec![
                        Row::new(WindowId(2), 0),
                        Row::new(WindowId(8), 0),
                        Row::new(WindowId(6), 0),
                    ],
                ),
            ],
            0,
        );
        let neighbor = find_neighbor_window(&layout, WindowId(5), Direction::Right).expect("right");
        assert_eq!(neighbor, NeighborLocation { col: 1, row: 1 });
        assert_eq!(
            layout.columns[neighbor.col].rows[neighbor.row].window_id,
            WindowId(8)
        );
    }

    // --- Resize: set_column_width ---

    #[test]
    fn set_column_width_sets_target_in_px() {
        // Positive: 1440px → free-form width set directly (no eighths quantization)
        let layout = three_column_layout();
        let result =
            set_column_width(&layout, WindowId(1), 1440, &test_config()).expect("set width");
        assert_eq!(result.columns[0].width_px, 1440);
    }

    #[test]
    fn set_column_width_only_affects_target_column() {
        // Positive: independent resize — other columns unchanged
        let layout = three_column_layout();
        let result =
            set_column_width(&layout, WindowId(1), 1440, &test_config()).expect("set width");
        assert_eq!(result.columns[0].width_px, 1440);
        // Other columns remain at 4 (no compensation)
        assert_eq!(result.columns[1].width_px, 960);
        assert_eq!(result.columns[2].width_px, 960);
    }

    #[test]
    fn set_column_width_returns_none_if_below_min() {
        // Negative: target px below min_column_width_px (480)
        // 320px < 480 → None
        let layout = three_column_layout();
        assert!(set_column_width(&layout, WindowId(1), 320, &test_config()).is_none());
    }

    #[test]
    fn set_column_width_returns_none_if_above_max() {
        // Negative: target px exceeds abs_max_width (1912)
        // 2400px > 1912 → None
        let layout = three_column_layout();
        assert!(set_column_width(&layout, WindowId(1), 2400, &test_config()).is_none());
    }

    #[test]
    fn set_column_width_returns_none_if_same_as_current() {
        // Negative: target matches current px → no-op → None
        // 960px = current → None
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
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            3000,
        );
        // Resize column 2 (W3) from 960px to 1500px → within bounds [480, 1912]
        let result = set_column_width(&layout, WindowId(3), 1500, &config).expect("set width");
        assert_eq!(result.columns[2].width_px, 1500);
        // ensure_column_visible should have shifted the viewport left
        assert!(
            result.viewport_offset < 3000,
            "viewport should shift left to reveal the resized column"
        );
    }

    // --- Resize: resize_column ---

    #[test]
    fn resize_column_positive_delta_grows() {
        // Positive: +480px delta on 960px column → 1440px (free-form)
        let layout = three_column_layout();
        let result = resize_column(&layout, WindowId(1), 480, &test_config()).expect("resize +480");
        assert_eq!(result.columns[0].width_px, 1440);
    }

    #[test]
    fn resize_column_negative_delta_shrinks() {
        // Positive: -480px delta on 960px column → 480px (free-form, clamped to min)
        let layout = three_column_layout();
        let result =
            resize_column(&layout, WindowId(1), -480, &test_config()).expect("resize -480");
        assert_eq!(result.columns[0].width_px, 480);
    }

    #[test]
    fn resize_column_delta_out_of_bounds_returns_none() {
        // Negative: +2000px delta on 960px column → 2960px > abs_max(1912) → None
        let layout = three_column_layout();
        assert!(resize_column(&layout, WindowId(1), 2000, &test_config()).is_none());
    }

    // --- Resize: expand_column ---

    #[test]
    fn expand_column_from_default_width() {
        // expand: 960px (base) + column_shift(964) = 1924 → clamped to abs_max 1912px
        let layout = three_column_layout(); // columns at base width 960px
        let result = expand_column(&layout, WindowId(1), &test_config()).expect("expand");
        assert_eq!(result.columns[0].width_px, 1912);
    }

    #[test]
    fn expand_column_from_sub_boundary() {
        // Start at half base width (480px). With max_n=0, the only rung above
        // base is abs_max. Expanding snaps up to base (960px).
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(480, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let result = expand_column(&layout, WindowId(1), &test_config()).expect("expand");
        assert_eq!(result.columns[0].width_px, 960);
    }

    #[test]
    fn expand_column_at_max_returns_none() {
        // Column at abs_max (1912px) → expanding returns None.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(1912, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        assert!(expand_column(&layout, WindowId(1), &test_config()).is_none());
    }

    #[test]
    fn expand_column_only_affects_target() {
        // Positive: independent resize — neighbors unchanged
        let layout = three_column_layout();
        let result = expand_column(&layout, WindowId(1), &test_config()).expect("expand");
        assert_eq!(result.columns[0].width_px, 1912);
        assert_eq!(result.columns[1].width_px, 960);
        assert_eq!(result.columns[2].width_px, 960);
    }

    // --- Resize: shrink_column ---

    #[test]
    fn shrink_column_from_max_width() {
        // Column at abs_max (1912px) → shrink returns to slot_max (base 960px).
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(1912, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let result = shrink_column(&layout, WindowId(1), &test_config()).expect("shrink");
        assert_eq!(result.columns[0].width_px, 960);
    }

    #[test]
    fn shrink_column_from_mid_boundary() {
        // Column at 1200px (between rungs). With max_n=0, the only rung below
        // base is not applicable; ceil snaps down to base (960px).
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(1200, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let result = shrink_column(&layout, WindowId(1), &test_config()).expect("shrink");
        assert_eq!(result.columns[0].width_px, 960);
    }

    #[test]
    fn shrink_column_at_boundary_returns_none() {
        // 960px (base) - column_shift(964) = -4px → below base → None
        let layout = three_column_layout();
        assert!(shrink_column(&layout, WindowId(1), &test_config()).is_none());
    }

    #[test]
    fn shrink_column_at_base_width_returns_none() {
        // Column at 480px (below base 960). Shrink: w <= base → None.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(480, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        assert!(shrink_column(&layout, WindowId(1), &test_config()).is_none());
    }

    #[test]
    fn shrink_column_only_affects_target() {
        // Positive: independent resize — neighbors unchanged
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(1912, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let result = shrink_column(&layout, WindowId(1), &test_config()).expect("shrink");
        assert_eq!(result.columns[0].width_px, 960);
        assert_eq!(result.columns[1].width_px, 960);
    }

    // --- Monocle ---

    #[test]
    fn toggle_monocle_expands_to_full() {
        let layout = three_column_layout();
        let (result, saved) =
            toggle_monocle(&layout, WindowId(1), None, &test_config()).expect("monocle on");
        assert_eq!(result.columns[0].width_px, 1912);
        assert_eq!(saved, Some(960)); // saved original width

        // Toggle back
        let (restored, saved2) =
            toggle_monocle(&result, WindowId(1), saved, &test_config()).expect("monocle off");
        assert_eq!(restored.columns[0].width_px, 960);
        assert_eq!(saved2, None);
    }

    // --- Add / Remove ---

    #[test]
    fn add_window_appends_column() {
        let layout = three_column_layout();
        let result = add_window(&layout, WindowId(10), &test_config());
        assert_eq!(result.columns.len(), 4);
        assert_eq!(result.columns[3].rows[0].window_id, WindowId(10));
        assert_eq!(result.columns[3].width_px, 960);
    }

    // --- insert_window_after_focused ---

    #[test]
    fn insert_after_focused_inserts_between_columns() {
        // Positive: focused on col 0 → new column inserted at index 1,
        // shifting the old col 1 and col 2 rightward.
        let layout = three_column_layout();
        let config = test_config();
        let result = insert_window_after_focused(&layout, Some(WindowId(1)), WindowId(10), &config);
        assert_eq!(result.columns.len(), 4);
        assert_eq!(result.columns[0].rows[0].window_id, WindowId(1)); // unchanged
        assert_eq!(result.columns[1].rows[0].window_id, WindowId(10)); // new window
        assert_eq!(result.columns[2].rows[0].window_id, WindowId(2)); // shifted right
        assert_eq!(result.columns[3].rows[0].window_id, WindowId(3)); // shifted right
    }

    #[test]
    fn insert_after_focused_middle_column() {
        // Positive: focused on col 1 (middle) → insert at index 2.
        let layout = three_column_layout();
        let config = test_config();
        let result = insert_window_after_focused(&layout, Some(WindowId(2)), WindowId(10), &config);
        assert_eq!(result.columns.len(), 4);
        assert_eq!(result.columns[0].rows[0].window_id, WindowId(1)); // unchanged
        assert_eq!(result.columns[1].rows[0].window_id, WindowId(2)); // unchanged
        assert_eq!(result.columns[2].rows[0].window_id, WindowId(10)); // new
        assert_eq!(result.columns[3].rows[0].window_id, WindowId(3)); // shifted right
    }

    #[test]
    fn insert_after_focused_last_column_appends() {
        // Positive: focused on last column → insert at end (same as append).
        let layout = three_column_layout();
        let config = test_config();
        let result = insert_window_after_focused(&layout, Some(WindowId(3)), WindowId(10), &config);
        assert_eq!(result.columns.len(), 4);
        assert_eq!(result.columns[3].rows[0].window_id, WindowId(10));
    }

    #[test]
    fn insert_after_focused_no_focus_appends_to_empty() {
        // Positive: no focus + empty layout → single column.
        let layout = VirtualLayout::new();
        let config = test_config();
        let result = insert_window_after_focused(&layout, None, WindowId(1), &config);
        assert_eq!(result.columns.len(), 1);
        assert_eq!(result.columns[0].rows[0].window_id, WindowId(1));
    }

    #[test]
    fn insert_after_focused_stale_focus_appends() {
        // Negative: focused window not in layout (stale) → append at end.
        let layout = three_column_layout();
        let config = test_config();
        let result =
            insert_window_after_focused(&layout, Some(WindowId(99)), WindowId(10), &config);
        assert_eq!(result.columns.len(), 4);
        assert_eq!(result.columns[3].rows[0].window_id, WindowId(10));
    }

    #[test]
    fn insert_after_focused_brings_new_column_into_view() {
        // Positive: when the new column is off-screen right, the viewport
        // scrolls to make it visible.
        // 4 columns at 960px each, monitor 1920 → only 2 visible. Focus col 1.
        // Insert after col 1 → new col at index 2, which is off-screen right.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
                Column::with_row(960, Row::new(WindowId(4), 0)),
            ],
            0,
        );
        let config = test_config();
        let result = insert_window_after_focused(&layout, Some(WindowId(2)), WindowId(10), &config);
        assert_eq!(result.columns.len(), 5);
        assert_eq!(result.columns[2].rows[0].window_id, WindowId(10));
        // Viewport should have scrolled right to reveal the new column.
        assert!(
            result.viewport_offset > 0,
            "viewport should scroll to reveal newly inserted column, got offset {}",
            result.viewport_offset
        );
    }

    #[test]
    fn remove_window_from_column() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
            0,
        );
        let result = remove_window(&layout, WindowId(1), &test_config());
        assert_eq!(result.columns.len(), 1);
        // After removal, the remaining row is redistributed to fill available height.
        // (1080 - 2*4) / 1 = 1072.
        assert_eq!(result.columns[0].rows, vec![Row::new(WindowId(2), 1072)]);
    }

    #[test]
    fn remove_last_window_in_column_removes_column() {
        let layout = three_column_layout();
        let result = remove_window(&layout, WindowId(2), &test_config());
        assert_eq!(result.columns.len(), 2);
        assert_eq!(result.columns[0].rows[0].window_id, WindowId(1));
        assert_eq!(result.columns[1].rows[0].window_id, WindowId(3));
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
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(1920, Row::new(WindowId(1), 0))], 0);
        let result = add_window_to_column(&layout, 0, WindowId(2), &test_config());
        // Adding a row redistributes the column to equal heights.
        // (1080 - 3*4) / 2 = 534 each.
        assert_eq!(
            result.columns[0].rows,
            vec![Row::new(WindowId(1), 534), Row::new(WindowId(2), 534)]
        );
    }

    #[test]
    fn add_window_to_invalid_column_is_noop() {
        // Negative: adding to non-existent column index
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(1920, Row::new(WindowId(1), 0))], 0);
        let result = add_window_to_column(&layout, 5, WindowId(2), &test_config());
        assert_eq!(result.columns.len(), 1);
        assert_eq!(result.columns[0].rows.len(), 1);
    }

    #[test]
    fn scroll_right_at_max_offset_returns_none() {
        // Negative: can't scroll beyond rightmost column
        // Single column at abs_max (1912px). Canvas = 4 + (1912+4) = 1920.
        // max_offset = 1920 - 1920 = 0. step = 1916. 0 + 1916 > 0 → None.
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(1912, Row::new(WindowId(1), 0))], 0);
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
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
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
        // Negative: can't shrink column below base width (ladder floor)
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(480, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        // 480px <= base(960) → None
        assert!(shrink_column(&layout, WindowId(1), &test_config()).is_none());
    }

    #[test]
    fn toggle_monocle_without_saved_width_uses_default() {
        // Positive: monocle off without saved width → defaults to column_width (960)
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(1912, Row::new(WindowId(1), 0))], 0);
        let (result, saved) =
            toggle_monocle(&layout, WindowId(1), None, &test_config()).expect("toggle");
        assert_eq!(result.columns[0].width_px, 960); // restored to column_width
        assert_eq!(saved, None);
    }

    #[test]
    fn add_window_to_empty_layout() {
        // Positive: adding first window to empty layout
        let layout = VirtualLayout::new();
        let result = add_window(&layout, WindowId(1), &test_config());
        assert_eq!(result.columns.len(), 1);
        assert_eq!(result.columns[0].rows[0].window_id, WindowId(1));
        assert_eq!(result.columns[0].width_px, 960); // default
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
        // Negative: setting width to current px value → same → None
        let layout = three_column_layout();
        // 960px = current → None
        assert!(set_column_width(&layout, WindowId(1), 960, &test_config()).is_none());
    }

    #[test]
    fn swap_column_left_single_column_returns_none() {
        // Negative: single column, no left neighbour to swap with
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
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
        let layout = initialize_windows(&[], &test_config(), None, None);
        assert!(layout.columns.is_empty());
        assert_eq!(layout.viewport_offset, 0);
    }

    #[test]
    fn initialize_windows_single_window() {
        // Positive: single window → single column. Canvas (968) < monitor (1920) →
        // fit case → center canvas: (968 - 1920) / 2 = -476.
        let layout = initialize_windows(&[WindowId(1)], &test_config(), None, None);
        assert_eq!(layout.columns.len(), 1);
        // Fresh single-row column gets full available height.
        // (1080 - 2*4) / 1 = 1072.
        assert_eq!(layout.columns[0].rows, vec![Row::new(WindowId(1), 1072)]);
        assert_eq!(layout.columns[0].width_px, 960); // default
        assert_eq!(layout.viewport_offset, -476, "fit case → canvas centered");
    }

    #[test]
    fn initialize_windows_multiple_windows() {
        // Positive: multiple windows → multiple columns in order
        let layout = initialize_windows(
            &[WindowId(10), WindowId(20), WindowId(30)],
            &test_config(),
            None,
            None,
        );
        assert_eq!(layout.columns.len(), 3);
        // Each fresh single-row column gets (1080 - 2*4) / 1 = 1072.
        assert_eq!(layout.columns[0].rows, vec![Row::new(WindowId(10), 1072)]);
        assert_eq!(layout.columns[1].rows, vec![Row::new(WindowId(20), 1072)]);
        assert_eq!(layout.columns[2].rows, vec![Row::new(WindowId(30), 1072)]);
        // All columns get default width
        assert_eq!(layout.columns[0].width_px, 960);
        assert_eq!(layout.columns[1].width_px, 960);
        assert_eq!(layout.columns[2].width_px, 960);
        assert_eq!(layout.viewport_offset, 0);
    }

    // --- initialize_windows with focus_col_idx tests ---

    #[test]
    fn initialize_windows_with_focus_shows_all_when_within_columns_per_screen() {
        // With test_config (monitor=1920, col_width=960, gap=4, slot=964):
        // canvas = 4 + 3*964 = 2896 < 1920 → NO, 2896 > 1920 → overflow.
        // Actually: canvas = 4 + 3*(960+4) = 4 + 3*964 = 2896. monitor=1920.
        // 2896 > 1920 → overflow. focus=2 → ensure_column_visible.
        let layout = initialize_windows(
            &[WindowId(1), WindowId(2), WindowId(3)],
            &test_config(),
            Some(2),
            None,
        );
        assert_eq!(layout.columns.len(), 3);
        // Overflow with focus=2: ensure_column_visible should center col 2.
        // Col 2: canvas_x = 4 + 2*964 = 1932, right = 1932+960 = 2892.
        // ideal_vp = 2892 + 4 - 1920 = 976.
        assert_eq!(
            layout.viewport_offset, 976,
            "3 cols overflow with focus=2 → ensure_column_visible offset"
        );
    }

    #[test]
    fn initialize_windows_without_focus_produces_zero_offset() {
        // Positive: passing None for focus_col_idx → viewport_offset stays 0.
        let layout = initialize_windows(
            &[WindowId(1), WindowId(2), WindowId(3)],
            &test_config(),
            None,
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
            None,
        );
        assert_eq!(
            layout.viewport_offset, 0,
            "out-of-bounds focus index should produce zero offset"
        );
    }

    #[test]
    fn initialize_windows_with_focus_on_first_column() {
        // Positive: focus_col_idx=Some(0) → col 0 already visible from offset 0.
        let layout = initialize_windows(
            &[WindowId(1), WindowId(2), WindowId(3)],
            &test_config(),
            Some(0),
            None,
        );
        // Overflow case, col 0 visible at offset 0.
        assert_eq!(layout.viewport_offset, 0);
    }

    /// Helper: build a MutationConfig with arbitrary values.
    fn viewport_config(
        monitor_width: i32,
        column_width: u32,
        gap: i32,
        columns_per_screen: u32,
    ) -> MutationConfig {
        let shift = column_width as i32 + gap;
        let abs_max = monitor_width - 2 * gap;
        let max_n = if shift > 0 && abs_max > column_width as i32 {
            ((abs_max - column_width as i32) / shift) as u32
        } else {
            0
        };
        MutationConfig {
            monitor_width,
            monitor_height: 1080,
            min_window_height_px: 100,
            column_width,
            min_column_width_px: column_width / 2,
            max_n,
            abs_max_width: abs_max,
            padding: Padding {
                window_gap: gap,
                up: 0,
                down: 0,
            },
            columns_per_screen,
        }
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
            None,
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
            None,
        );
        assert_eq!(
            layout.viewport_offset, 0,
            "scroll case with focus=0 → offset 0 (first col visible from start)"
        );
    }

    // -------------------------------------------------------------------
    // Bug 2 regression: expand/shrink column step must include window_gap.
    // column_shift = column_width + window_gap, NOT just column_width.
    //
    // The old eighths-based code lost the gap during re-quantization:
    // pixels_to_eighths snapped to the column_width grid, discarding
    // the gap term. The new px-based code preserves it exactly.
    //
    // Uses a realistic config (960/16 ratio) with max_n=1 so the
    // two-step ladder (base → slot_max → abs_max) is exercised.
    // -------------------------------------------------------------------

    /// Helper: build a realistic MutationConfig with a meaningful window_gap.
    ///
    /// - monitor=2560, column_width=960, gap=16
    /// - abs_max = 2560 - 2*16 = 2528
    /// - column_shift = 960 + 16 = 976
    /// - max_n = floor((2528 - 960) / 976) = 1
    /// - slot_max = 960 + 1*976 = 1936
    ///
    /// The gap term (16) is small relative to column_width (960), which is
    /// exactly why the old eighths code hid it — re-quantization rounded it
    /// away. The new px code preserves it, so expand from base yields
    /// 1936 (with gap) not 1920 (without gap).
    fn realistic_gap_config() -> MutationConfig {
        MutationConfig {
            monitor_width: 2560,
            monitor_height: 1080,
            min_window_height_px: 100,
            column_width: 960,
            min_column_width_px: 480,
            max_n: 1,            // (2528-960)/976 = 1
            abs_max_width: 2528, // 2560 - 2*16
            padding: Padding {
                window_gap: 16,
                up: 0,
                down: 0,
            },
            columns_per_screen: 2,
        }
    }

    #[test]
    fn expand_preserves_window_gap_in_ladder() {
        // Bug 2: expand must move by column_width + window_gap (=976), not just column_width (960).
        // Old eighths code gave 1920 (gap lost). New px code gives 1936 (gap preserved).
        let config = realistic_gap_config(); // base=960, shift=976, slot_max=1936, abs_max=2528
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let result = expand_column(&layout, WindowId(1), &config).expect("expand from base");
        assert_eq!(
            result.columns[0].width_px, 1936,
            "expand must include window_gap (960+976, not 960+960)"
        );
    }

    #[test]
    fn expand_two_step_abs_max_then_noop() {
        // F4 abs_max two-step ladder: slot_max → abs_max jump; abs_max → no-op.
        let config = realistic_gap_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(1936, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let result =
            expand_column(&layout, WindowId(1), &config).expect("expand slot_max -> abs_max");
        assert_eq!(result.columns[0].width_px, 2528);
        // at abs_max -> None
        assert!(expand_column(&result, WindowId(1), &config).is_none());
    }

    #[test]
    fn shrink_reverse_ladder() {
        let config = realistic_gap_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(2528, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let r1 = shrink_column(&layout, WindowId(1), &config).expect("shrink abs_max -> slot_max");
        assert_eq!(r1.columns[0].width_px, 1936);
        let r2 = shrink_column(&r1, WindowId(1), &config).expect("shrink slot_max -> base");
        assert_eq!(r2.columns[0].width_px, 960);
        assert!(
            shrink_column(&r2, WindowId(1), &config).is_none(),
            "shrink at base -> None"
        );
    }

    #[test]
    fn expand_shrink_roundtrip_with_gap() {
        let config = realistic_gap_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let expanded = expand_column(&layout, WindowId(1), &config).expect("expand");
        assert_eq!(expanded.columns[0].width_px, 1936);
        let shrunk = shrink_column(&expanded, WindowId(1), &config).expect("shrink");
        assert_eq!(shrunk.columns[0].width_px, 960);
    }

    // -------------------------------------------------------------------
    // Bug 2 regression: shrink from abs_max when slot_max == abs_max.
    //
    // When `window_gap` divides the monitor evenly, `slot_max` collides with
    // `abs_max_width`. The old `shrink_column` routed `w >= abs_max` through
    // `set_column_width(slot_max)`, which returned `None` (target == current)
    // and froze the column at the top — shrink was impossible. The general
    // ceil-descent now handles it by landing one rung below.
    // -------------------------------------------------------------------

    /// Helper: a config where `slot_max == abs_max` (gap = 0).
    ///
    /// - monitor=1920, column_width=960, gap=0
    /// - abs_max = 1920 − 0 = 1920
    /// - column_shift = 960 + 0 = 960
    /// - max_n = ⌊(1920 − 960) / 960⌋ = 1
    /// - slot_max = 960 + 1×960 = 1920 == abs_max  ← the degenerate collision
    fn slot_max_equals_abs_max_config() -> MutationConfig {
        MutationConfig {
            monitor_width: 1920,
            monitor_height: 1080,
            min_window_height_px: 100,
            column_width: 960,
            min_column_width_px: 480,
            max_n: 1,
            abs_max_width: 1920,
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            columns_per_screen: 2,
        }
    }

    #[test]
    fn shrink_from_abs_max_when_slot_max_equals_abs_max() {
        // Bug 2: shrinking from abs_max must NOT no-op when slot_max == abs_max.
        let config = slot_max_equals_abs_max_config(); // slot_max == abs_max == 1920
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(1920, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let result = shrink_column(&layout, WindowId(1), &config)
            .expect("shrink from abs_max must succeed even when slot_max == abs_max");
        // ceil((1920−960)/960) = 1 → target_n = 0 → base + 0 = 960.
        assert_eq!(
            result.columns[0].width_px, 960,
            "shrink from abs_max lands one rung below when slot_max == abs_max"
        );
    }

    // -------------------------------------------------------------------
    // Multi-step ladder: max_n >= 2 exercises intermediate rungs (Area 3).
    // -------------------------------------------------------------------

    /// Helper: build a config with max_n=2 for multi-step ladder tests.
    ///
    /// - monitor=3840, column_width=960, gap=16
    /// - abs_max = 3840 − 2×16 = 3808
    /// - column_shift = 960 + 16 = 976
    /// - max_n = ⌊(3808 − 960) / 976⌋ = ⌊2848 / 976⌋ = 2
    /// - slot_max = 960 + 2×976 = 2912
    fn multi_step_config() -> MutationConfig {
        MutationConfig {
            monitor_width: 3840,
            monitor_height: 1080,
            min_window_height_px: 100,
            column_width: 960,
            min_column_width_px: 480,
            max_n: 2,
            abs_max_width: 3808,
            padding: Padding {
                window_gap: 16,
                up: 0,
                down: 0,
            },
            columns_per_screen: 3,
        }
    }

    #[test]
    fn expand_multi_step_ladder_advances_one_rung_at_a_time() {
        // Area 3: max_n=2, expand advances through each rung:
        // base(960) → n=1(1936) → n=2/slot_max(2912) → abs_max(3808) → None.
        let config = multi_step_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        // Step 1: base (960) → n=1 (960 + 976 = 1936)
        let r1 = expand_column(&layout, WindowId(1), &config).expect("expand base → n=1");
        assert_eq!(
            r1.columns[0].width_px, 1936,
            "first rung: base + column_shift"
        );
        // Step 2: n=1 (1936) → n=2 (960 + 2×976 = 2912 = slot_max)
        let r2 = expand_column(&r1, WindowId(1), &config).expect("expand n=1 → n=2");
        assert_eq!(r2.columns[0].width_px, 2912, "second rung: slot_max");
        // Step 3: slot_max (2912) → abs_max (3808) — two-step top
        let r3 = expand_column(&r2, WindowId(1), &config).expect("expand slot_max → abs_max");
        assert_eq!(
            r3.columns[0].width_px, 3808,
            "two-step top: slot_max → abs_max"
        );
        // Step 4: abs_max → no-op
        assert!(
            expand_column(&r3, WindowId(1), &config).is_none(),
            "abs_max → None"
        );
    }

    #[test]
    fn shrink_multi_step_ladder_descends_one_rung_at_a_time() {
        // Area 3: max_n=2, shrink descends:
        // abs_max(3808) → slot_max(2912) → n=1(1936) → base(960) → None.
        let config = multi_step_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(3808, Row::new(WindowId(1), 0)), // abs_max
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let r1 = shrink_column(&layout, WindowId(1), &config).expect("shrink abs_max → slot_max");
        assert_eq!(
            r1.columns[0].width_px, 2912,
            "reverse two-step: abs_max → slot_max"
        );
        let r2 = shrink_column(&r1, WindowId(1), &config).expect("shrink n=2 → n=1");
        assert_eq!(r2.columns[0].width_px, 1936, "slot_max → n=1");
        let r3 = shrink_column(&r2, WindowId(1), &config).expect("shrink n=1 → base");
        assert_eq!(r3.columns[0].width_px, 960, "n=1 → base");
        assert!(
            shrink_column(&r3, WindowId(1), &config).is_none(),
            "base → None"
        );
    }

    // --- Area 5: Small free-form deltas ---

    #[test]
    fn resize_column_small_delta_succeeds() {
        // Area 5: small non-eighth-aligned deltas now succeed (free-form).
        // Old eighths code: pixels_to_eighths(1060, 960) = (1060×4 + 480) / 960
        //   = (4240 + 480) / 960 = 4720 / 960 = 4 (unchanged — delta lost).
        // New px code: 960 + 100 = 1060, within [480, 1912] → succeeds.
        let layout = three_column_layout();
        let result = resize_column(&layout, WindowId(1), 100, &test_config()).expect("resize +100");
        assert_eq!(
            result.columns[0].width_px, 1060,
            "small +100 delta must succeed as free-form"
        );
    }

    #[test]
    fn resize_column_negative_small_delta_succeeds() {
        // Area 5: negative small delta shrinks freely (free-form, not eighths-snapped).
        let layout = three_column_layout();
        let result =
            resize_column(&layout, WindowId(1), -100, &test_config()).expect("resize -100");
        assert_eq!(
            result.columns[0].width_px, 860,
            "small -100 delta must succeed as free-form"
        );
    }

    #[test]
    fn resize_column_negative_delta_below_min_returns_none() {
        // Area 5: negative delta that would push below min_column_width_px → None.
        // 960 − 600 = 360 < 480 (min) → None.
        let layout = three_column_layout();
        assert!(
            resize_column(&layout, WindowId(1), -600, &test_config()).is_none(),
            "resize below min must return None"
        );
    }

    // --- Area 6: toggle_monocle with non-base saved width ---

    #[test]
    fn toggle_monocle_saves_and_restores_expanded_width() {
        // Area 6: column at slot_max (not base) when entering monocle → saved
        // width is slot_max, not base. Toggling off restores to slot_max.
        let config = realistic_gap_config(); // base=960, slot_max=1936, abs_max=2528
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(1936, Row::new(WindowId(1), 0)), // at slot_max
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        // Enter monocle: save 1936, set to abs_max (2528)
        let (r1, saved) = toggle_monocle(&layout, WindowId(1), None, &config).expect("monocle on");
        assert_eq!(r1.columns[0].width_px, 2528);
        assert_eq!(
            saved,
            Some(1936),
            "saved width should be the pre-monocle expanded width"
        );
        // Exit monocle: restore to 1936 (not base 960)
        let (r2, saved2) = toggle_monocle(&r1, WindowId(1), saved, &config).expect("monocle off");
        assert_eq!(
            r2.columns[0].width_px, 1936,
            "restored to slot_max, not base"
        );
        assert_eq!(saved2, None);
    }

    // --- Area 8: column_shift() / slot_max() direct unit tests ---

    #[test]
    fn mutation_config_column_shift_includes_gap() {
        // Area 8: column_shift = column_width + window_gap.
        let config = realistic_gap_config(); // column_width=960, gap=16
        assert_eq!(config.column_shift(), 976, "column_shift = 960 + 16");
    }

    #[test]
    fn mutation_config_column_shift_and_slot_max_zero_gap() {
        // Area 8: with gap=0, column_shift = column_width, slot_max = base + max_n*base.
        let config = MutationConfig {
            monitor_width: 1920,
            monitor_height: 1080,
            min_window_height_px: 100,
            column_width: 960,
            min_column_width_px: 480,
            max_n: 1,
            abs_max_width: 1920,
            padding: Padding {
                window_gap: 0,
                up: 0,
                down: 0,
            },
            columns_per_screen: 4,
        };
        assert_eq!(
            config.column_shift(),
            960,
            "column_shift = column_width when gap=0"
        );
        assert_eq!(config.slot_max(), 1920, "slot_max = 960 + 1×960 = 1920");
    }

    #[test]
    fn mutation_config_slot_max_equals_base_when_max_n_zero() {
        // Area 8: when max_n=0 (column_width close to abs_max), slot_max = base.
        // This is the degenerate case where expand from base jumps straight to
        // abs_max (no intermediate rungs).
        let config = test_config(); // max_n=0, column_width=960
        assert_eq!(
            config.slot_max(),
            960,
            "slot_max = column_width when max_n=0"
        );
    }

    // -------------------------------------------------------------------
    // center_viewport_on_focused: prefix-sum center on focused column
    // -------------------------------------------------------------------

    #[test]
    fn center_viewport_on_focused_lands_at_monitor_midpoint() {
        // 3 uniform 960px columns, gap=4, monitor=1920. Focus col 1 (middle).
        // canvas_x(1) = 0 + 2*4 = 8 (Σ widths[0] + 2*gap)
        //   = 960 + 2*4 = 968
        // offset = 968 - (1920 - 960)/2 = 968 - 480 = 488
        // Verify: focus col center on screen = canvas_x(1) + 960/2 - 488 = 968 + 480 - 488 = 960
        // = monitor_width / 2. ✓
        let config = test_config();
        let layout = three_column_layout();
        let offset = center_viewport_on_focused(&layout, 1, &config);
        let focus_center = 960 + 2 * 4 + 960 / 2;
        assert_eq!(
            focus_center - offset,
            config.monitor_width / 2,
            "focused column center must land at monitor midpoint"
        );
    }

    #[test]
    fn center_viewport_on_focused_all_fit_case() {
        // 2 small columns that fit in monitor, focused col 1.
        // canvas_x(1) = 480 + 2*4 = 488
        // offset = 488 - (1920 - 480)/2 = 488 - 720 = -232
        // Property: focus col center on screen = 488 + 240 - (-232) = 960 = monitor/2
        let config = viewport_config(1920, 480, 4, 4);
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(480, Row::new(WindowId(1), 0)),
                Column::with_row(480, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let offset = center_viewport_on_focused(&layout, 1, &config);
        let focus_center = 480 + 2 * 4 + 480 / 2;
        assert_eq!(
            focus_center - offset,
            config.monitor_width / 2,
            "all-fit: focused col still lands at monitor midpoint"
        );
    }

    #[test]
    fn center_viewport_on_focused_variable_widths() {
        // Non-uniform widths: col 0=480, col 1=720, col 2=960, gap=4, monitor=1920.
        // Focus col 2: canvas_x(2) = 480+720 + 3*4 = 1212
        // offset = 1212 - (1920-960)/2 = 1212 - 480 = 732
        // Verify: 1212 + 480 - 732 = 960 = monitor/2
        let config = test_config();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(480, Row::new(WindowId(1), 0)),
                Column::with_row(720, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            0,
        );
        let offset = center_viewport_on_focused(&layout, 2, &config);
        let focus_center = 480 + 720 + 3 * 4 + 960 / 2;
        assert_eq!(
            focus_center - offset,
            config.monitor_width / 2,
            "variable widths: prefix-sum math correctly centers focused col"
        );
    }

    // -------------------------------------------------------------------
    // center_viewport_canvas: canvas centering (fit case)
    // -------------------------------------------------------------------

    #[test]
    fn center_viewport_canvas_negative_when_canvas_smaller() {
        // Single 480px column, gap=4, monitor=1920.
        // canvas = 4 + (480+4) = 488
        // offset = (488 - 1920) / 2 = -716
        let config = viewport_config(1920, 480, 4, 4);
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(480, Row::new(WindowId(1), 0))], 0);
        let offset = center_viewport_canvas(&layout, &config);
        assert_eq!(offset, -716, "negative when canvas < monitor");
    }

    #[test]
    fn center_viewport_canvas_positive_when_canvas_larger() {
        // 3 columns × 960px, gap=4, monitor=1920.
        // canvas = 4 + 3*(960+4) = 2896
        // offset = (2896 - 1920) / 2 = 488
        let config = test_config();
        let layout = three_column_layout();
        let offset = center_viewport_canvas(&layout, &config);
        assert_eq!(offset, 488, "positive when canvas > monitor");
    }

    #[test]
    fn center_viewport_canvas_zero_when_equal() {
        // 2 columns × 960px, gap=0, monitor=1920.
        // canvas = 0 + 2*(960+0) = 1920
        // offset = (1920 - 1920) / 2 = 0
        let config = viewport_config(1920, 960, 0, 4);
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let offset = center_viewport_canvas(&layout, &config);
        assert_eq!(offset, 0, "zero when canvas == monitor");
    }

    // -------------------------------------------------------------------
    // quantize_to_ladder: nearest rung snapping
    // -------------------------------------------------------------------

    #[test]
    fn quantize_to_ladder_at_base_returns_base() {
        let config = realistic_gap_config(); // base=960, shift=976, max_n=1, slot_max=1936, abs_max=2528
        assert_eq!(quantize_to_ladder(960, &config), 960);
    }

    #[test]
    fn quantize_to_ladder_near_first_rung_snaps_up() {
        // 960 + 976/2 = 1448 → rounds to 1936
        let config = realistic_gap_config();
        assert_eq!(quantize_to_ladder(1448, &config), 1936);
    }

    #[test]
    fn quantize_to_ladder_near_first_rung_snaps_down() {
        // 960 + 976/2 - 1 = 1447 → rounds to 960
        let config = realistic_gap_config();
        assert_eq!(quantize_to_ladder(1447, &config), 960);
    }

    #[test]
    fn quantize_to_ladder_at_slot_max_returns_slot_max() {
        let config = realistic_gap_config();
        assert_eq!(quantize_to_ladder(1936, &config), 1936);
    }

    #[test]
    fn quantize_to_ladder_in_two_step_gap_snaps_to_abs_max() {
        // Between slot_max (1936) and abs_max (2528), midpoint = 2232.
        // 2300 >= 2232 → snaps to abs_max.
        let config = realistic_gap_config();
        assert_eq!(quantize_to_ladder(2300, &config), 2528);
    }

    #[test]
    fn quantize_to_ladder_in_two_step_gap_snaps_to_slot_max() {
        // 2200 < 2232 → snaps to slot_max.
        let config = realistic_gap_config();
        assert_eq!(quantize_to_ladder(2200, &config), 1936);
    }

    #[test]
    fn quantize_to_ladder_clamps_below_base() {
        let config = realistic_gap_config();
        assert_eq!(quantize_to_ladder(100, &config), 960);
    }

    #[test]
    fn quantize_to_ladder_clamps_above_abs_max() {
        let config = realistic_gap_config();
        assert_eq!(quantize_to_ladder(5000, &config), 2528);
    }

    // -------------------------------------------------------------------
    // initialize_windows: new paths (Some-widths, fit→center_canvas,
    // overflow→ensure_column_visible)
    // -------------------------------------------------------------------

    #[test]
    fn init_with_widths_quantizes_and_clamps() {
        // widths = [500, 1450, 5000]
        // 500 < base(960) → clamped to 960
        // 1450 → nearest rung of {960, 1936, 2528}: midpoint 1448, 1450 >= 1448 → 1936
        // 5000 > abs_max(2528) → clamped to 2528
        let config = realistic_gap_config();
        let layout = initialize_windows(
            &[WindowId(1), WindowId(2), WindowId(3)],
            &config,
            None,
            Some(&[500, 1450, 5000]),
        );
        assert_eq!(layout.columns[0].width_px, 960, "below base → clamped");
        assert_eq!(layout.columns[1].width_px, 1936, "near rung → snapped");
        assert_eq!(layout.columns[2].width_px, 2528, "above abs_max → clamped");
    }

    #[test]
    fn init_fit_case_centers_canvas() {
        // 2 small cols (300px each) fit in 1920 monitor → center canvas.
        // canvas = 4 + 2*(300+4) = 612
        // center = (612 - 1920) / 2 = -654
        let config = viewport_config(1920, 300, 4, 4);
        let layout = initialize_windows(&[WindowId(1), WindowId(2)], &config, None, None);
        assert_eq!(layout.viewport_offset, -654, "fit case → canvas center");
    }

    #[test]
    fn init_overflow_with_focus_uses_ensure_column_visible() {
        // 3 × 960px cols, gap=4, monitor=1920 → overflow (canvas=2896 > 1920).
        // Focus col 2: ensure_column_visible should produce offset 976.
        let layout = initialize_windows(
            &[WindowId(1), WindowId(2), WindowId(3)],
            &test_config(),
            Some(2),
            None,
        );
        assert_eq!(
            layout.viewport_offset, 976,
            "overflow with focus → ensure_column_visible"
        );
    }

    #[test]
    fn init_with_widths_none_preserves_old_behavior() {
        // widths=None should give same result as old initialize_windows.
        let layout = initialize_windows(
            &[WindowId(1), WindowId(2), WindowId(3)],
            &test_config(),
            Some(0),
            None,
        );
        assert_eq!(layout.columns.len(), 3);
        for col in &layout.columns {
            assert_eq!(col.width_px, 960, "all cols at base width");
        }
    }

    // -------------------------------------------------------------------
    // center_viewport_on_focused: focus_col = 0 / last / single-column
    //
    // The leading `window_gap` term in `canvas_x(f) = Σ widths[0..f] +
    // (f+1) * gap` is the easy off-by-one trap (the canvas left-edge gap).
    // focus_col = 0 is the cleanest place to catch it: canvas_x(0) = gap,
    // NOT 0. Existing tests all used focus_col ≥ 1, which would miss a
    // `f * gap` vs `(f + 1) * gap` confusion only at the boundary.
    // -------------------------------------------------------------------

    #[test]
    fn center_viewport_on_focused_first_column_includes_leading_gap() {
        // 3 uniform 960px columns, gap=4, monitor=1920. Focus col 0.
        // canvas_x(0) = 0 widths + (0+1)*4 = 4 (the leading gap).
        // offset = 4 - (1920 - 960) / 2 = 4 - 480 = -476.
        // A `f * gap` implementation would produce canvas_x(0) = 0 → offset
        // -480, failing the assertion by 4 pixels.
        let config = test_config();
        let layout = three_column_layout();
        let offset = center_viewport_on_focused(&layout, 0, &config);
        let focus_center = 4 + 960 / 2; // canvas_x(0) + width/2
        assert_eq!(
            focus_center - offset,
            config.monitor_width / 2,
            "focus col 0 center must land at monitor midpoint"
        );
        // Pin the explicit value to catch any leading-gap regression.
        assert_eq!(
            offset, -476,
            "canvas_x(0)=4 (leading gap), offset = 4 - 480"
        );
    }

    #[test]
    fn center_viewport_on_focused_last_column() {
        // 3 uniform 960px columns, gap=4, monitor=1920. Focus col 2 (last).
        // canvas_x(2) = 960 + 960 + 3*4 = 1932.
        // offset = 1932 - (1920 - 960) / 2 = 1932 - 480 = 1452.
        let config = test_config();
        let layout = three_column_layout();
        let offset = center_viewport_on_focused(&layout, 2, &config);
        let canvas_x_2 = 960 + 960 + 3 * 4;
        let focus_center = canvas_x_2 + 960 / 2;
        assert_eq!(
            focus_center - offset,
            config.monitor_width / 2,
            "last focus col must land at monitor midpoint"
        );
        assert_eq!(offset, 1452);
    }

    #[test]
    fn center_viewport_on_focused_single_column() {
        // Edge: single column. canvas_x(0) = (0+1)*4 = 4. width = 960.
        // offset = 4 - (1920-960)/2 = -476.
        let config = test_config();
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(960, Row::new(WindowId(1), 0))], 0);
        let offset = center_viewport_on_focused(&layout, 0, &config);
        assert_eq!(offset, -476, "single column centered at midpoint");
    }

    // -------------------------------------------------------------------
    // insert_window_after_focused_with_width: direct layout-level coverage.
    //
    // The workspace wrapper (`insert_window_with_width`) is tested at the
    // engine level but only for the col-0-focused and None-focus cases.
    // These pin the contract of the underlying layout primitive directly:
    // the explicit width passes through verbatim (caller quantizes), the
    // insertion index follows the same rules as `insert_window_after_focused`,
    // and the same stale-focus / no-focus fallbacks apply.
    // -------------------------------------------------------------------

    #[test]
    fn insert_after_focused_with_width_sets_explicit_width_verbatim() {
        // Positive: focused on col 0 → new column at index 1 with the exact
        // width passed in. The caller is responsible for quantizing first;
        // this function must NOT re-quantize or snap.
        let layout = three_column_layout();
        let config = test_config();
        let result = insert_window_after_focused_with_width(
            &layout,
            Some(WindowId(1)),
            WindowId(10),
            1500,
            &config,
        );
        assert_eq!(result.columns.len(), 4);
        assert_eq!(
            result.columns[1].rows[0].window_id,
            WindowId(10),
            "new column at index 1"
        );
        assert_eq!(
            result.columns[1].width_px, 1500,
            "width must pass through verbatim (caller quantizes)"
        );
        // Other columns unchanged.
        assert_eq!(result.columns[0].width_px, 960);
        assert_eq!(result.columns[2].width_px, 960);
        assert_eq!(result.columns[3].width_px, 960);
    }

    #[test]
    fn insert_after_focused_with_width_middle_column_inserts_at_next_index() {
        // Positive: focused on col 1 (middle) → new column at index 2 with
        // the explicit width. Columns at indices ≥ 2 shift right.
        let layout = three_column_layout();
        let config = test_config();
        let result = insert_window_after_focused_with_width(
            &layout,
            Some(WindowId(2)),
            WindowId(10),
            1200,
            &config,
        );
        assert_eq!(result.columns.len(), 4);
        assert_eq!(
            result.columns[0].rows[0].window_id,
            WindowId(1),
            "unchanged"
        );
        assert_eq!(
            result.columns[1].rows[0].window_id,
            WindowId(2),
            "unchanged"
        );
        assert_eq!(
            result.columns[2].rows[0].window_id,
            WindowId(10),
            "new column"
        );
        assert_eq!(result.columns[2].width_px, 1200);
        assert_eq!(
            result.columns[3].rows[0].window_id,
            WindowId(3),
            "shifted right"
        );
    }

    #[test]
    fn insert_after_focused_with_width_last_column_appends() {
        // Positive: focused on last column → insert at end with explicit width.
        let layout = three_column_layout();
        let config = test_config();
        let result = insert_window_after_focused_with_width(
            &layout,
            Some(WindowId(3)),
            WindowId(10),
            1500,
            &config,
        );
        assert_eq!(result.columns.len(), 4);
        assert_eq!(result.columns[3].rows[0].window_id, WindowId(10));
        assert_eq!(result.columns[3].width_px, 1500);
    }

    #[test]
    fn insert_after_focused_with_width_stale_focus_appends() {
        // Negative: focused window id not in layout (stale) → fall back to
        // appending at the end. Width still passes through.
        let layout = three_column_layout();
        let config = test_config();
        let result = insert_window_after_focused_with_width(
            &layout,
            Some(WindowId(99)),
            WindowId(10),
            1500,
            &config,
        );
        assert_eq!(result.columns.len(), 4);
        assert_eq!(result.columns[3].rows[0].window_id, WindowId(10));
        assert_eq!(result.columns[3].width_px, 1500);
    }

    #[test]
    fn insert_after_focused_with_width_no_focus_appends_to_empty_layout() {
        // Edge: None focus + empty layout → single column with the explicit
        // width (not the base column_width).
        let layout = VirtualLayout::new();
        let config = test_config();
        let result =
            insert_window_after_focused_with_width(&layout, None, WindowId(1), 1500, &config);
        assert_eq!(result.columns.len(), 1);
        assert_eq!(result.columns[0].rows[0].window_id, WindowId(1));
        assert_eq!(
            result.columns[0].width_px, 1500,
            "None-focus path must still honor the explicit width"
        );
    }

    #[test]
    fn insert_after_focused_with_width_clamps_via_caller_quantize() {
        // Integration: the typical caller path quantizes first via
        // `quantize_to_ladder` and passes the result here. Verify the two
        // functions compose correctly — the explicit width survives the
        // insert unchanged.
        let layout = three_column_layout();
        let config = test_config();
        let quantized = quantize_to_ladder(1500, &config); // test_config has max_n=0
        let result = insert_window_after_focused_with_width(
            &layout,
            Some(WindowId(1)),
            WindowId(10),
            quantized,
            &config,
        );
        assert_eq!(
            result.columns[1].width_px, quantized,
            "quantized width passes through insert unchanged"
        );
    }

    // --- merge_column ---

    #[test]
    fn merge_column_right_into_single_row_column_removes_source() {
        // Layout: [W1][W2]; merge W1 right → src (col 0) emptied and removed,
        // dst (col 1) now has [W2, W1] with 2 equal rows of 534 each.
        // (available=1080, n=2 → (1080 - 3*4)/2 = 534 each.)
        let layout = three_column_layout(); // [W1][W2][W3]
        let config = test_config();
        let result = merge_column(&layout, WindowId(1), Direction::Right, &config).unwrap();

        assert_eq!(result.columns.len(), 2, "source column removed");
        assert_eq!(
            result.columns[0]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(2), WindowId(1)],
            "W1 appended to bottom of dst"
        );
        assert_eq!(result.columns[0].rows[0].height, 534);
        assert_eq!(result.columns[0].rows[1].height, 534);
    }

    #[test]
    fn merge_column_left_into_single_row_column_removes_source() {
        // Layout: [W1][W2]; merge W2 left → src (col 1) emptied and removed,
        // dst (col 0) now has [W1, W2].
        let layout = three_column_layout(); // [W1][W2][W3]
        let config = test_config();
        let result = merge_column(&layout, WindowId(2), Direction::Left, &config).unwrap();

        assert_eq!(result.columns.len(), 2, "source column removed");
        assert_eq!(
            result.columns[0]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(1), WindowId(2)],
            "W2 appended to bottom of dst"
        );
        assert_eq!(result.columns[0].rows[0].height, 534);
        assert_eq!(result.columns[0].rows[1].height, 534);
    }

    #[test]
    fn merge_column_into_multi_row_column_appends_at_bottom() {
        // Layout: [W1][W2,W3]; merge W1 right → src (col 0) emptied and
        // removed, dst (col 1) now has [W2, W3, W1] with 3 rows.
        // n=3: (1080 - 4*4)/3 = 1064/3 = 354, remainder 2 → [355, 355, 354].
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(2), 0), Row::new(WindowId(3), 0)],
                ),
            ],
            0,
        );
        let config = test_config();
        let result = merge_column(&layout, WindowId(1), Direction::Right, &config).unwrap();

        assert_eq!(result.columns.len(), 1, "source column removed");
        assert_eq!(
            result.columns[0]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(2), WindowId(3), WindowId(1)],
            "W1 appended to bottom of multi-row dst"
        );
        assert_eq!(result.columns[0].rows[0].height, 355);
        assert_eq!(result.columns[0].rows[1].height, 355);
        assert_eq!(result.columns[0].rows[2].height, 354);
    }

    #[test]
    fn merge_column_source_keeps_remaining_rows_redistributed() {
        // Layout: [W1,W2][W3]; merge W1 right → src still has [W2],
        // redistributed for n=1 → 1072. dst has [W3, W1] with 534 each.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
                ),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            0,
        );
        let config = test_config();
        let result = merge_column(&layout, WindowId(1), Direction::Right, &config).unwrap();

        assert_eq!(result.columns.len(), 2, "source column kept");
        assert_eq!(
            result.columns[0]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(2)],
            "src still has W2"
        );
        assert_eq!(result.columns[0].rows[0].height, 1072);
        assert_eq!(
            result.columns[1]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(3), WindowId(1)],
            "dst has W3 then W1"
        );
        assert_eq!(result.columns[1].rows[0].height, 534);
        assert_eq!(result.columns[1].rows[1].height, 534);
    }

    #[test]
    fn merge_column_no_neighbor_left_returns_none() {
        // Layout: [W1][W2][W3]; W1 is leftmost — merge left has no target.
        let layout = three_column_layout();
        let config = test_config();
        assert!(merge_column(&layout, WindowId(1), Direction::Left, &config).is_none());
    }

    #[test]
    fn merge_column_no_neighbor_right_returns_none() {
        // Layout: [W1][W2][W3]; W3 is rightmost — merge right has no target.
        let layout = three_column_layout();
        let config = test_config();
        assert!(merge_column(&layout, WindowId(3), Direction::Right, &config).is_none());
    }

    #[test]
    fn merge_column_vertical_direction_rejected() {
        let layout = three_column_layout();
        let config = test_config();
        assert!(merge_column(&layout, WindowId(2), Direction::Up, &config).is_none());
        assert!(merge_column(&layout, WindowId(2), Direction::Down, &config).is_none());
    }

    #[test]
    fn merge_column_unknown_focused_returns_none() {
        let layout = three_column_layout();
        let config = test_config();
        assert!(merge_column(&layout, WindowId(99), Direction::Right, &config).is_none());
    }

    #[test]
    fn merge_column_rejects_when_min_height_violated() {
        // Construct a config where 3 rows in the destination would push each
        // row below the floor. dst has 2 rows; merging 1 more → 3 rows →
        // distribute_heights(3, 1080, 4) = [355, 355, 354]. With floor=400,
        // every row violates, so the merge is rejected.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(2), 0), Row::new(WindowId(3), 0)],
                ),
            ],
            0,
        );
        let config = MutationConfig {
            min_window_height_px: 400,
            ..test_config()
        };
        assert!(merge_column(&layout, WindowId(1), Direction::Right, &config).is_none());
    }

    #[test]
    fn merge_column_accepts_at_min_height_boundary() {
        // Borderline positive: distribute_heights(3, 1080, 4) = [355, 355, 354].
        // The minimum produced is 354. With floor=354, the guard condition
        // `h < floor` is `354 < 354` = false → merge ACCEPTED.
        // This pins the inclusive boundary: a height equal to the floor is OK.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(2), 0), Row::new(WindowId(3), 0)],
                ),
            ],
            0,
        );
        let config = MutationConfig {
            min_window_height_px: 354,
            ..test_config()
        };
        let result = merge_column(&layout, WindowId(1), Direction::Right, &config);
        assert!(
            result.is_some(),
            "merge must succeed when min height exactly equals floor (354 == 354)"
        );
        // Verify the resulting heights match the formula.
        let result = result.unwrap();
        assert_eq!(result.columns[0].rows[0].height, 355);
        assert_eq!(result.columns[0].rows[1].height, 355);
        assert_eq!(result.columns[0].rows[2].height, 354);
    }

    #[test]
    fn merge_column_rejects_borderline_min_height_violation() {
        // Borderline negative: distribute_heights(3, 1080, 4) = [355, 355, 354].
        // The minimum produced is 354. With floor=355, the guard condition
        // `h < floor` is `354 < 355` = true → merge REJECTED.
        // This pins the off-by-one: a height 1px below the floor is rejected.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(2), 0), Row::new(WindowId(3), 0)],
                ),
            ],
            0,
        );
        let config = MutationConfig {
            min_window_height_px: 355,
            ..test_config()
        };
        assert!(
            merge_column(&layout, WindowId(1), Direction::Right, &config).is_none(),
            "merge must fail when shortest row (354) is 1px below floor (355)"
        );
    }

    #[test]
    fn merge_column_left_with_multi_row_source_and_single_row_dst() {
        // Layout: [W1][W2,W3]; merge W3 left → src still has [W2],
        // redistributed for n=1 → 1072. dst (col 0) has [W1, W3] with 534
        // each. Verifies direction=Left path with non-emptied source.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(2), 0), Row::new(WindowId(3), 0)],
                ),
            ],
            0,
        );
        let config = test_config();
        let result = merge_column(&layout, WindowId(3), Direction::Left, &config).unwrap();

        assert_eq!(result.columns.len(), 2, "source column kept");
        assert_eq!(
            result.columns[0]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(1), WindowId(3)],
            "W3 appended to bottom of dst"
        );
        assert_eq!(result.columns[0].rows[0].height, 534);
        assert_eq!(result.columns[0].rows[1].height, 534);
        assert_eq!(
            result.columns[1]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(2)],
            "src still has W2 after W3 was removed"
        );
        assert_eq!(result.columns[1].rows[0].height, 1072);
    }

    // --- promote_window ---

    #[test]
    fn promote_right_from_two_row_column_splits_into_two_columns() {
        // Layout: [W1,W2]; promote W1 right → [W2][W1].
        // src (col 0) has [W2] redistributed for n=1 → 1072.
        // new column (col 1) has [W1] at single-row height 1072.
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                960,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
            0,
        );
        let config = test_config();
        let result = promote_window(&layout, WindowId(1), Direction::Right, &config).unwrap();

        assert_eq!(result.columns.len(), 2, "split into two columns");
        assert_eq!(
            result.columns[0]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(2)],
            "src keeps the non-promoted window"
        );
        assert_eq!(result.columns[0].rows[0].height, 1072);
        assert_eq!(
            result.columns[1]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(1)],
            "new column has the promoted window"
        );
        assert_eq!(result.columns[1].rows[0].height, 1072);
    }

    #[test]
    fn promote_left_from_two_row_column_splits_into_two_columns() {
        // Layout: [W1,W2]; promote W2 left → [W2][W1].
        // src (originally col 0) becomes col 1 with [W1]; new column
        // (col 0) has [W2].
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                960,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
            0,
        );
        let config = test_config();
        let result = promote_window(&layout, WindowId(2), Direction::Left, &config).unwrap();

        assert_eq!(result.columns.len(), 2, "split into two columns");
        assert_eq!(
            result.columns[0]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(2)],
            "promoted window is in the new leftmost column"
        );
        assert_eq!(
            result.columns[1]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(1)],
            "src column shifted right by one"
        );
    }

    #[test]
    fn promote_middle_row_redistributes_remaining() {
        // Layout: [W1,W2,W3]; promote W2 right → [W1,W3][W2].
        // src has [W1, W3] redistributed for n=2 → 534 each.
        // new column has [W2] at 1072.
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                960,
                vec![
                    Row::new(WindowId(1), 0),
                    Row::new(WindowId(2), 0),
                    Row::new(WindowId(3), 0),
                ],
            )],
            0,
        );
        let config = test_config();
        let result = promote_window(&layout, WindowId(2), Direction::Right, &config).unwrap();

        assert_eq!(result.columns.len(), 2, "split into two columns");
        assert_eq!(
            result.columns[0]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(1), WindowId(3)],
            "W2 extracted; W1 and W3 remain in src in original order"
        );
        assert_eq!(result.columns[0].rows[0].height, 534);
        assert_eq!(result.columns[0].rows[1].height, 534);
        assert_eq!(
            result.columns[1]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(2)],
            "new column has the promoted window"
        );
        assert_eq!(result.columns[1].rows[0].height, 1072);
    }

    #[test]
    fn promote_from_four_row_column_distributes_remainder_to_top() {
        // Layout: [W1,W2,W3,W4]; promote W2 right → src has [W1, W3, W4] (n=3).
        // distribute_heights(3, 1080, 4) = [355, 355, 354] — top 2 rows get +1.
        // This pins the "top gets +1" remainder ordering via the promote path
        // (complementing the n=2 no-remainder case above and the merge tests).
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                960,
                vec![
                    Row::new(WindowId(1), 0),
                    Row::new(WindowId(2), 0),
                    Row::new(WindowId(3), 0),
                    Row::new(WindowId(4), 0),
                ],
            )],
            0,
        );
        let config = test_config();
        let result = promote_window(&layout, WindowId(2), Direction::Right, &config).unwrap();

        assert_eq!(result.columns.len(), 2, "split into two columns");
        assert_eq!(
            result.columns[0]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(1), WindowId(3), WindowId(4)],
            "W2 extracted; remaining rows keep their relative order"
        );
        // src redistributed for n=3: [355, 355, 354] (top gets +1).
        assert_eq!(result.columns[0].rows[0].height, 355);
        assert_eq!(result.columns[0].rows[1].height, 355);
        assert_eq!(result.columns[0].rows[2].height, 354);
        // new column is single-row: full available height.
        assert_eq!(result.columns[1].rows[0].window_id, WindowId(2));
        assert_eq!(result.columns[1].rows[0].height, 1072);
    }

    #[test]
    fn promote_single_row_column_is_no_op() {
        // Layout: [W1]; W1 is already alone — promote is a no-op.
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(960, Row::new(WindowId(1), 0))], 0);
        let config = test_config();
        assert!(promote_window(&layout, WindowId(1), Direction::Right, &config).is_none());
        assert!(promote_window(&layout, WindowId(1), Direction::Left, &config).is_none());
    }

    #[test]
    fn promote_vertical_direction_rejected() {
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                960,
                vec![Row::new(WindowId(1), 0), Row::new(WindowId(2), 0)],
            )],
            0,
        );
        let config = test_config();
        assert!(promote_window(&layout, WindowId(1), Direction::Up, &config).is_none());
        assert!(promote_window(&layout, WindowId(1), Direction::Down, &config).is_none());
    }

    #[test]
    fn promote_unknown_focused_returns_none() {
        let layout = three_column_layout();
        let config = test_config();
        assert!(promote_window(&layout, WindowId(99), Direction::Right, &config).is_none());
    }

    #[test]
    fn promote_right_from_three_columns_inserts_after_src() {
        // Layout: [W1][W2,W3][W4]; promote W2 right → [W1][W3][W2][W4].
        // The new column lands between the original src and the right
        // neighbor.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(2), 0), Row::new(WindowId(3), 0)],
                ),
                Column::with_row(960, Row::new(WindowId(4), 0)),
            ],
            0,
        );
        let config = test_config();
        let result = promote_window(&layout, WindowId(2), Direction::Right, &config).unwrap();

        assert_eq!(result.columns.len(), 4);
        assert_eq!(result.columns[0].rows[0].window_id, WindowId(1));
        assert_eq!(
            result.columns[1]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(3)],
            "src column now has only W3"
        );
        assert_eq!(
            result.columns[2].rows[0].window_id,
            WindowId(2),
            "promoted window inserted as its own column right after src"
        );
        assert_eq!(result.columns[3].rows[0].window_id, WindowId(4));
    }
}
