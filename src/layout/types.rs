//! Layout engine type definitions.

use crate::common::{Rect, WindowId};

/// Proportional column width in units of 1/8 monitor width.
///
/// Valid range: 1–8 (1 = 1/8 screen, 4 = half, 8 = full).
pub type WidthEighths = u8;

/// A column on the virtual canvas containing one or more stacked windows.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// Proportional width (1–8 eighths of monitor width).
    pub width_eighths: WidthEighths,
    /// Window IDs ordered top-to-bottom within this column.
    pub rows: Vec<WindowId>,
    /// Height ratios for each row, summing to 1.0.
    /// Defaults to equal division if empty or mismatched.
    pub row_ratios: Vec<f32>,
}

impl Column {
    /// Create a new column with a single window and the given width.
    #[must_use]
    pub fn new(width_eighths: WidthEighths, window: WindowId) -> Self {
        Self {
            width_eighths,
            rows: vec![window],
            row_ratios: vec![1.0],
        }
    }

    /// Create a column with equal row ratios for the given windows.
    #[must_use]
    pub fn with_equal_rows(width_eighths: WidthEighths, rows: Vec<WindowId>) -> Self {
        let count = rows.len();
        let ratio = if count > 0 { 1.0 / count as f32 } else { 1.0 };
        Self {
            width_eighths,
            rows,
            row_ratios: vec![ratio; count],
        }
    }

    /// Validate that width is within bounds (1–8).
    #[must_use]
    pub fn is_valid_width(&self) -> bool {
        (1..=8).contains(&self.width_eighths)
    }
}

/// The complete virtual layout — all tiling columns on the infinite canvas.
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
#[derive(Debug, Clone, PartialEq)]
pub struct ActualEntry {
    pub window_id: WindowId,
    pub rect: Rect,
}

/// The on-screen projection of the virtual layout.
#[derive(Debug, Clone, PartialEq)]
pub struct ActualLayout {
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
#[derive(Debug, Clone, PartialEq)]
pub struct WindowMove {
    pub window_id: WindowId,
    pub from: Rect,
    pub to: Rect,
    pub hint: AnimationHint,
}

/// Animation hint for a window move, controlling easing behavior.
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorInfo {
    /// Monitor work area in screen coordinates.
    pub work_area: Rect,
}

/// Gap configuration for the layout engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gaps {
    pub inner: i32,
    pub outer: i32,
}

/// Result of a layout mutation.
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
