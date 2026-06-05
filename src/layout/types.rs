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

/// The complete virtual layout — all tiling columns on the infinite canvas.
///
/// This is Layer 1 of the pipeline: a **logical description** with no pixel
/// coordinates. Columns store proportional widths ([`WidthEighths`]), and
/// position is implicit from their order in the `columns` vec.
///
/// The `viewport_offset` determines which portion of the infinite canvas is
/// visible on screen. It is adjusted by scroll operations and auto-scroll
/// during focus changes.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualLayout {
    /// Columns ordered left-to-right on the virtual canvas.
    pub columns: Vec<Column>,
    /// Pixel offset from the left edge of the canvas to the left edge of the viewport.
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

/// A single window's computed on-screen rectangle.
///
/// This is Layer 2 of the pipeline. The `rect` is the **final HWND rect**
/// with padding already baked in — it can be passed directly to `SetWindowPos`.
///
/// Produced by [`super::projection::project`], consumed by [`super::diff::diff`].
#[derive(Debug, Clone, PartialEq)]
pub struct ActualEntry {
    /// The window this entry describes.
    pub window_id: WindowId,
    /// Final on-screen rectangle (padding baked in, pass directly to `SetWindowPos`).
    pub rect: Rect,
}

/// The on-screen projection of the virtual layout.
///
/// Layer 2 output — a flat list of [`ActualEntry`]s, one per window (including
/// off-screen windows parked at hidden positions). Produced by
/// [`super::projection::project`].
#[derive(Debug, Clone, PartialEq)]
pub struct ActualLayout {
    /// One entry per window (including off-screen/parked windows).
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
/// The compositor uses this to choose the animation curve:
///
/// - `Snap` — fast, springy (in-viewport adjustment)
/// - `Displaced` — smooth, slightly slower (neighbor pushed out of the way)
/// - `ScrollEnter` — window entering viewport from off-screen
/// - `ScrollExit` — window leaving viewport
/// - `Restore` — no animation, instant (crash/minimize restore)
///
/// Classification is based on horizontal move distance. See
/// [`super::diff`] for the classification function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnimationHint {
    /// Snapped window itself — fast, springy.
    Snap,
    /// Neighbor pushed out of the way — smooth, slightly slower.
    Displaced,
    /// Window entering viewport from off-screen.
    ScrollEnter,
    /// Window leaving viewport.
    ScrollExit,
    /// Crash/minimize restore — no animation, instant.
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

/// Result of a layout mutation.
///
/// Layer 3 output — contains the new virtual layout, the new actual layout,
/// and the [`WindowMove`]s that describe how windows changed position.
///
/// The compositor reads `moves` to drive `SetWindowPos` calls with animation.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutDiff {
    /// The new virtual layout after the mutation.
    pub virtual_layout: VirtualLayout,
    /// The new actual layout after projection.
    pub actual_layout: ActualLayout,
    /// Window moves to animate.
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
