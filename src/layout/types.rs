//! Layout engine type definitions.
//!
//! Core data types for the 2-layer layout pipeline. The key types are:
//!
//! - [`Column`] — a vertical container of windows with a pixel width
//! - [`VirtualLayout`] — the infinite horizontal canvas (widths in pixels,
//!   no absolute x-positions stored)
//! - [`ActualLayout`] — projected screen coordinates (pixel rects)
//! - [`AppliedLayout`] — the result of a mutation (new virtual + actual layouts)
//!
//! # Width representation
//!
//! Column widths are stored directly in **pixels** (`width_px`). Earlier
//! revisions stored widths as eighths of the base `column_width` to stay
//! resolution-independent, but that quantization discarded the inter-column
//! `window_gap` during expand/shrink (the gap was smaller than one eighth and
//! rounded away), so a single expand step moved by exactly `column_width`
//! instead of `column_width + window_gap`. Pixel widths make the gap
//! observable and let expand/shrink advance by the true
//! `column_shift = column_width + window_gap`. The cost — dependence on the
//! configured `column_width` and monitor resolution — is accepted; the
//! `window_gap` is already pixel-based everywhere else.

use crate::common::{Rect, WindowId};

/// A column on the virtual canvas containing one or more stacked windows.
///
/// Columns are the **vertical containers** in the layout. Windows within a
/// column are always equally sized vertically (equal-height rows).
///
/// The column does **not** store pixel position — that is *implicit* from its
/// index in [`VirtualLayout::columns`] plus the cumulative widths of preceding
/// columns. Pixel coordinates are computed by [`super::projection::project`].
///
/// # Container Model
///
/// ```text
/// VirtualLayout (horizontal)
/// ├── Column 0 (vertical)
/// │   ├── Row 0: WindowId(1)
/// │   └── Row 1: WindowId(2)
/// ├── Column 1
/// │   └── Row 0: WindowId(3)
/// └── ...
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// Pixel width of this column on the virtual canvas (`> 0`).
    ///
    /// Real bounds (`[min_column_width_px, abs_max_width]`) are enforced at
    /// the mutation sites via [`MutationConfig`](super::mutations::MutationConfig),
    /// not here — the column type is config-agnostic by design (it lives in
    /// the pure layout layer). This field only carries the structural
    /// invariant `width_px > 0` (see [`Column::is_valid_width`]).
    ///
    /// Expand/shrink snap this onto a `column_shift`-aligned slot ladder
    /// (`column_shift = column_width + window_gap`); free-form widths arise
    /// from drag-resize and from `viewport_offset` adjustments.
    pub width_px: i32,
    /// Window IDs ordered top-to-bottom within this column.
    pub rows: Vec<WindowId>,
}

impl Column {
    /// Create a new column with a single window and the given pixel width.
    ///
    /// `width_px` is taken as-is; mutation-layer bounds (min/abs_max) are not
    /// checked here — see [`Column::is_valid_width`] for the structural check.
    #[must_use]
    pub fn new(width_px: i32, window: WindowId) -> Self {
        Self {
            width_px,
            rows: vec![window],
        }
    }

    /// Create a column with multiple equally-sized rows.
    #[must_use]
    pub fn with_equal_rows(width_px: i32, rows: Vec<WindowId>) -> Self {
        Self { width_px, rows }
    }

    /// Structural validity check: a column must have a strictly positive width.
    ///
    /// Config-dependent bounds (`min_column_width_px` / `abs_max_width`) are
    /// enforced at the mutation sites, not here, because this type is config-
    /// agnostic. This method only guards against the degenerate `width_px <= 0`
    /// invariant.
    #[must_use]
    pub fn is_valid_width(&self) -> bool {
        self.width_px > 0
    }
}

/// The complete virtual layout — all tiling columns on the infinite horizontal canvas.
///
/// This is the "source of truth" for the layout engine. It describes **what exists**
/// (columns, windows, their pixel widths) and **where the camera is**
/// (`viewport_offset`), but contains no absolute x-positions for individual windows.
///
/// # Camera model
///
/// `viewport_offset` acts as a camera position sliding along the infinite canvas.
/// A value of `0` means the camera is aligned with the left edge of the first column.
/// Increasing it scrolls the viewport rightward across the canvas.
///
/// Many operations (scrolling, focus-to-offscreen, swap) are
/// implemented by adjusting `viewport_offset` rather than moving individual windows.
/// The projection layer then computes actual pixel positions from this combined state.
///
/// # Immutability
///
/// Mutations never modify a `VirtualLayout` in place — they return a new one.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualLayout {
    /// Columns ordered left-to-right on the virtual canvas.
    pub columns: Vec<Column>,
    /// Camera position: pixel offset from canvas left edge to viewport left edge.
    pub viewport_offset: i32,
}

impl VirtualLayout {
    /// Create an empty virtual layout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
            viewport_offset: 0,
        }
    }

    /// Create a virtual layout with the given columns and viewport offset.
    #[must_use]
    pub fn with_columns(columns: Vec<Column>, viewport_offset: i32) -> Self {
        Self {
            columns,
            viewport_offset,
        }
    }

    /// Find the column and row index of a window.
    #[must_use]
    pub fn find_window(&self, id: WindowId) -> Option<(usize, usize)> {
        for (col_idx, col) in self.columns.iter().enumerate() {
            for (row_idx, wid) in col.rows.iter().enumerate() {
                if *wid == id {
                    return Some((col_idx, row_idx));
                }
            }
        }
        None
    }

    /// Return the total number of tiling windows.
    #[must_use]
    pub fn window_count(&self) -> usize {
        self.columns.iter().map(|c| c.rows.len()).sum()
    }
}

impl Default for VirtualLayout {
    fn default() -> Self {
        Self::new()
    }
}

/// A single window's computed on-screen rectangle — what Windows OS actually sees.
///
/// Every window in the [`VirtualLayout`] has a corresponding `ActualEntry`, whether
/// it is visible on-screen or parked off-screen. The `rect` field contains the real
/// pixel coordinates that will be passed to `SetWindowPos`.
#[derive(Debug, Clone, PartialEq)]
pub struct ActualEntry {
    /// The window identifier.
    pub window_id: WindowId,
    /// The computed on-screen rectangle (real pixel coordinates for Windows OS).
    pub rect: Rect,
}

/// The on-screen projection of the virtual layout — what Windows OS must render.
///
/// This is produced by [`super::projection::project()`] and contains pixel-accurate
/// rectangles for every window. Windows visible in the viewport receive on-screen
/// coordinates; windows outside the viewport are **parked** at deterministic
/// off-screen positions (one column-width beyond the nearest viewport edge).
///
/// # Why parking matters
///
/// Windows OS does not gracefully ignore windows placed at extreme off-screen
/// coordinates. Rather than leaving off-screen windows at their unreachable virtual
/// positions, we park them just beyond the viewport edge. This ensures:
/// - Animation transitions (scroll in/out) are smooth and short-distance.
/// - The OS window manager is never confused by far-off-screen windows.
/// - There are two parking zones: **left** (beyond the left edge) and **right**
///   (beyond the right edge), chosen based on which side the column exited.
#[derive(Debug, Clone, PartialEq)]
pub struct ActualLayout {
    /// One entry per window (visible on-screen or parked off-screen).
    pub entries: Vec<ActualEntry>,
}

impl ActualLayout {
    /// Create an empty actual layout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Find an entry by window ID.
    #[must_use]
    pub fn find(&self, id: WindowId) -> Option<&ActualEntry> {
        self.entries.iter().find(|e| e.window_id == id)
    }
}

impl Default for ActualLayout {
    fn default() -> Self {
        Self::new()
    }
}

/// Monitor geometry used for layout projection.
///
/// Contains the work area [`Rect`] (excluding taskbar). The layout engine uses
/// this to determine how many columns fit on screen and where to place windows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorInfo {
    /// Monitor work area in screen coordinates.
    pub work_area: Rect,
}

/// Gap and margin configuration for the layout engine.
///
/// This mirrors [`config::types::Padding`](crate::config::types::Padding) but lives
/// in the layout module to avoid a circular dependency. The daemon converts
/// between the two when constructing [`MutationConfig`](crate::layout::mutations::MutationConfig).
///
/// # Gap Model (Uniform Spacing)
///
/// `window_gap` creates **uniform spacing** everywhere — the same gap appears
/// between the screen edge and a window, and between two adjacent windows.
///
/// ```text
/// Edge | window_gap | [Window 1] | window_gap | [Window 2] | window_gap | Edge
/// ```
///
/// This is achieved via the slot-based canvas model:
/// - Each column occupies a "slot" = content + right gap
/// - `slot_width = base_content_width + window_gap`
/// - Canvas starts at `window_gap` (initial left-edge gap)
/// - The last column's embedded right gap serves as the right-edge gap
///
/// Vertically, each window is inset by `window_gap` on top and bottom within
/// its row cell, producing `2 * window_gap` gap between adjacent rows and
/// `window_gap` gap at the top/bottom edges of the tiling area.
///
/// See the [crate-level documentation](crate#padding-strategy) for a visual diagram.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Padding {
    /// Uniform gap between all elements (windows and screen edges), in pixels.
    ///
    /// This single value controls both horizontal (inter-column) and vertical
    /// (inter-row) spacing, as well as the gap from windows to screen edges.
    pub window_gap: i32,
    /// Top screen margin — reserved space above the tiling area (e.g., for
    /// custom title bars or visual breathing room above windows).
    pub up: i32,
    /// Bottom screen margin — reserved space below the tiling area (e.g.,
    /// for taskbar clearance).
    pub down: i32,
}

/// Result of a layout mutation — the new layout state after applying a change.
///
/// Produced by [`ScrollingSpace`](crate::workspace::ScrollingSpace) for each
/// mutation. Contains the new [`VirtualLayout`] (infinite canvas state) and
/// the new [`ActualLayout`] (pixel-accurate on-screen positions).
///
/// The animation layer compares each window's target rect (from
/// `actual_layout`) against its real on-screen position and animates only
/// the windows that differ. This means every window is represented — even
/// ones whose target didn't change — so a rapid second mutation during an
/// in-flight animation will correctly retarget windows that are still
/// mid-flight (the animator samples their real current position, sees it
/// differs from the target, and builds a tween).
///
/// ## Design decision: why no `moves` field
///
/// Previously this struct carried a list of window-move instructions
/// computed by diffing the previous and new actual layouts. That diff
/// only included windows whose *target* changed, which meant windows
/// that were still physically animating toward their old target were
/// absent from the diff. On a rapid second mutation, those windows
/// were dropped from the animator's batch and stranded mid-screen.
///
/// By returning the full `ActualLayout` instead, the animation layer
/// always sees every window and can make its own no-op filtering decision
/// based on real current positions (see `build_tweens` in the animation
/// module).
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedLayout {
    /// The new virtual layout after the mutation.
    pub virtual_layout: VirtualLayout,
    /// The new actual layout after projection — the authoritative target
    /// for every window's final position.
    pub actual_layout: ActualLayout,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_layout_find_window() {
        let id_a = WindowId(1);
        let id_b = WindowId(2);
        let layout =
            VirtualLayout::with_columns(vec![Column::new(960, id_a), Column::new(960, id_b)], 0);
        assert_eq!(layout.find_window(id_a), Some((0, 0)));
        assert_eq!(layout.find_window(id_b), Some((1, 0)));
        assert_eq!(layout.find_window(WindowId(99)), None);
    }

    #[test]
    fn virtual_layout_window_count() {
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_equal_rows(960, vec![WindowId(1), WindowId(2)]),
                Column::new(960, WindowId(3)),
            ],
            0,
        );
        assert_eq!(layout.window_count(), 3);
    }

    #[test]
    fn column_valid_width() {
        // Structural check only: strictly positive. Config-dependent min/max
        // bounds are enforced at the mutation sites.
        assert!(Column::new(1, WindowId(1)).is_valid_width());
        assert!(Column::new(960, WindowId(1)).is_valid_width());
        assert!(!Column::new(0, WindowId(1)).is_valid_width());
        assert!(!Column::new(-1, WindowId(1)).is_valid_width());
    }
}
