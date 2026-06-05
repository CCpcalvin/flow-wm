//! Layout engine type definitions.
//!
//! Core data types for the 3-layer layout pipeline. The key types are:
//!
//! - [`Column`] — a vertical container of windows with proportional width
//! - [`VirtualLayout`] — the infinite horizontal canvas (logical, no pixels)
//! - [`ActualLayout`] — projected screen coordinates (pixel rects)
//! - [`LayoutDiff`] — the result of a mutation (new layouts + animation moves)

use crate::common::{Rect, WindowId};

/// Proportional column width in eighths of the default column width.
///
/// Valid range: 1–8. A value of 4 equals `column_width` pixels;
/// 8 equals `2 * column_width`. See [`super::projection`] for the conversion function.
///
/// This is intentionally **not** pixel-based — it keeps the virtual layout
/// resolution-independent. Pixel conversion happens only during projection.
#[doc(alias = "width")]
pub type WidthEighths = u8;

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
    /// Proportional width (1–8 eighths of column width base).
    pub width_eighths: WidthEighths,
    /// Window IDs ordered top-to-bottom within this column.
    pub rows: Vec<WindowId>,
}

impl Column {
    /// Create a new column with a single window and the given width.
    #[must_use]
    pub fn new(width_eighths: WidthEighths, window: WindowId) -> Self {
        Self {
            width_eighths,
            rows: vec![window],
        }
    }

    /// Create a column with multiple equally-sized rows.
    #[must_use]
    pub fn with_equal_rows(width_eighths: WidthEighths, rows: Vec<WindowId>) -> Self {
        Self {
            width_eighths,
            rows,
        }
    }

    /// Validate that width is within bounds (1–8).
    #[must_use]
    pub fn is_valid_width(&self) -> bool {
        (1..=8).contains(&self.width_eighths)
    }
}

/// The complete virtual layout — all tiling columns on the infinite horizontal canvas.
///
/// This is the "source of truth" for the layout engine. It describes **what exists**
/// (columns, windows, their proportional widths) and **where the camera is**
/// (`viewport_offset`), but contains no pixel coordinates for individual windows.
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

/// A single window move instruction produced by a layout diff.
///
/// Each move carries the window's previous and next [`Rect`], plus an
/// [`AnimationHint`] that controls the easing/interpolation behavior
/// when the compositor applies the move.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowMove {
    /// The window being moved.
    pub window_id: WindowId,
    /// Previous position rectangle.
    pub from: Rect,
    /// Target position rectangle.
    pub to: Rect,
    /// Animation hint controlling easing behavior.
    pub hint: AnimationHint,
}

/// Animation hint for a window move, controlling easing behavior.
///
/// Hints are classified by the [`diff`](crate::layout::diff) module based on
/// horizontal move distance. This allows the compositor to apply different animation
/// curves depending on *why* a window is moving:
///
/// | Hint | When | Curve |
/// |------|------|-------|
/// | `Snap` | Small in-viewport move (≤500px) | Fast, springy |
/// | `Displaced` | Neighbor pushed aside (unused currently) | Smooth, slower |
/// | `ScrollEnter` | Entering viewport from parked position | Scroll ease-in |
/// | `ScrollExit` | Leaving viewport to parked position | Scroll ease-out |
/// | `Restore` | Crash/minimize recovery | Instant (no animation) |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnimationHint {
    /// Small in-viewport move — fast, springy easing.
    Snap,
    /// Neighbor pushed out of the way — smooth, slightly slower easing.
    Displaced,
    /// Window entering viewport from off-screen (moving from parked → visible).
    ScrollEnter,
    /// Window leaving viewport (moving from visible → parked).
    ScrollExit,
    /// Crash/minimize restore — no animation, instant placement.
    Restore,
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

/// Padding configuration for the layout engine.
///
/// This mirrors [`config::types::Padding`](crate::config::types::Padding) but lives
/// in the layout module to avoid a circular dependency. The daemon converts
/// between the two when constructing [`MutationConfig`](crate::layout::mutations::MutationConfig).
///
/// See the [crate-level documentation](crate#padding-strategy) for a visual diagram.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Padding {
    /// Inset around each window within its container cell.
    pub window: i32,
    /// Top screen margin.
    pub up: i32,
    /// Bottom screen margin.
    pub down: i32,
}

/// Result of a layout mutation — everything needed to animate the transition.
///
/// Produced by [`LayoutEngine`](crate::layout::engine::LayoutEngine) for each
/// mutation. Contains:
/// - The new [`VirtualLayout`] (infinite canvas state after mutation).
/// - The new [`ActualLayout`] (pixel-accurate on-screen positions).
/// - A list of [`WindowMove`]s describing what changed (for animation).
///
/// Windows that did not move between the old and new actual layout are omitted
/// from `moves` — only windows that need animation are included.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutDiff {
    /// The new virtual layout after the mutation.
    pub virtual_layout: VirtualLayout,
    /// The new actual layout after projection.
    pub actual_layout: ActualLayout,
    /// Window moves to animate — only windows whose position changed.
    pub moves: Vec<WindowMove>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_layout_find_window() {
        let id_a = WindowId(1);
        let id_b = WindowId(2);
        let layout =
            VirtualLayout::with_columns(vec![Column::new(4, id_a), Column::new(4, id_b)], 0);
        assert_eq!(layout.find_window(id_a), Some((0, 0)));
        assert_eq!(layout.find_window(id_b), Some((1, 0)));
        assert_eq!(layout.find_window(WindowId(99)), None);
    }

    #[test]
    fn virtual_layout_window_count() {
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_equal_rows(4, vec![WindowId(1), WindowId(2)]),
                Column::new(4, WindowId(3)),
            ],
            0,
        );
        assert_eq!(layout.window_count(), 3);
    }

    #[test]
    fn column_valid_width() {
        assert!(Column::new(1, WindowId(1)).is_valid_width());
        assert!(Column::new(8, WindowId(1)).is_valid_width());
        assert!(!Column::new(0, WindowId(1)).is_valid_width());
        assert!(!Column::new(9, WindowId(1)).is_valid_width());
    }
}
