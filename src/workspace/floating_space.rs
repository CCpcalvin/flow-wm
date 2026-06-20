//! Floating window space — ordered list of windows in literal pixel coordinates.
//!
//! [`FloatingSpace`] holds non-tiled windows as an ordered list of on-screen
//! rectangles ([`ActualEntry`](crate::layout::types::ActualEntry)). It lives
//! directly in pixel/actual space — it does NOT run windows through the
//! virtual/actual projection pipeline.
//!
//! # Coordinate model
//!
//! Each floating window keeps the on-screen rectangle the user dragged it to (or
//! the centered default when first floated). The space just remembers those
//! rectangles so it can hide/show them on workspace switch and merge them into
//! an [`ActualLayout`](crate::layout::types::ActualLayout) for the animation
//! batch.
//!
//! See (`docs/src/dev-guide/workspace.md`) for how `FloatingSpace` fits within
//! the two-coordinate-space model alongside
//! [`ScrollingSpace`](super::ScrollingSpace).

use crate::common::{Rect, Size, WindowId};
use crate::layout::types::{ActualEntry, ActualLayout};

/// Space for floating (non-tiled) windows within a [`Workspace`](super::Workspace).
///
/// An ordered list of windows with their on-screen pixel rectangles. Later
/// entries render on top (z-order). Pure data + math — no Win32, no side
/// effects.
///
/// See (`docs/src/dev-guide/floating-space.md`) for the architecture overview.
pub struct FloatingSpace {
    /// Ordered list of floating windows; later entries render on top.
    windows: Vec<ActualEntry>,
}

impl FloatingSpace {
    /// Create a new empty floating space.
    #[must_use]
    pub fn new() -> Self {
        Self {
            windows: Vec::new(),
        }
    }

    /// Add or update a floating window.
    ///
    /// Appends to the end (new window on top of z-order). If `window_id`
    /// already exists, updates its rect in place without changing z-order.
    pub fn add(&mut self, window_id: WindowId, rect: Rect) {
        if let Some(entry) = self.windows.iter_mut().find(|e| e.window_id == window_id) {
            entry.rect = rect;
        } else {
            self.windows.push(ActualEntry { window_id, rect });
        }
    }

    /// Remove a floating window, returning its old rect if it existed.
    pub fn remove(&mut self, window_id: WindowId) -> Option<Rect> {
        let idx = self.windows.iter().position(|e| e.window_id == window_id)?;
        let entry = self.windows.remove(idx);
        Some(entry.rect)
    }

    /// Returns `true` if the given window is in this floating space.
    #[must_use]
    pub fn contains(&self, window_id: WindowId) -> bool {
        self.windows.iter().any(|e| e.window_id == window_id)
    }

    /// Returns `true` if no floating windows are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Returns the number of floating windows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Read-only view of all floating window entries.
    #[must_use]
    pub fn windows(&self) -> &[ActualEntry] {
        &self.windows
    }

    /// Build an [`ActualLayout`] from this space's entries.
    ///
    /// The daemon merges this into the animation batch alongside the scrolling
    /// space's `ActualLayout`.
    #[must_use]
    pub fn to_actual_layout(&self) -> ActualLayout {
        ActualLayout {
            entries: self.windows.clone(),
        }
    }

    /// Center a rect of `preferred` size within `work_area`.
    ///
    /// Clamps `preferred` to `work_area` dimensions if larger in either axis.
    /// Non-positive dimensions are clamped to zero.
    #[must_use]
    pub fn centered_rect(preferred: Size, work_area: Rect) -> Rect {
        let w = preferred.w.clamp(0, work_area.width);
        let h = preferred.h.clamp(0, work_area.height);
        let x = work_area.x + (work_area.width - w) / 2;
        let y = work_area.y + (work_area.height - h) / 2;
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }
}

impl Default for FloatingSpace {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- centered_rect tests ---

    #[test]
    fn centered_rect_exact_center() {
        let preferred = Size { w: 400, h: 300 };
        let work_area = Rect {
            x: 100,
            y: 200,
            width: 1000,
            height: 800,
        };
        let result = FloatingSpace::centered_rect(preferred, work_area);
        assert_eq!(
            result,
            Rect {
                x: 400,
                y: 450,
                width: 400,
                height: 300
            }
        );
    }

    #[test]
    fn centered_rect_clamp_too_large() {
        let preferred = Size { w: 2000, h: 1500 };
        let work_area = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 800,
        };
        let result = FloatingSpace::centered_rect(preferred, work_area);
        assert_eq!(
            result,
            Rect {
                x: 0,
                y: 0,
                width: 1000,
                height: 800
            }
        );
    }

    #[test]
    fn centered_rect_one_axis_oversized() {
        let preferred = Size { w: 1500, h: 300 };
        let work_area = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 800,
        };
        let result = FloatingSpace::centered_rect(preferred, work_area);
        // Width clamped to 1000 (x=0), height centered: y = (800-300)/2 = 250
        assert_eq!(
            result,
            Rect {
                x: 0,
                y: 250,
                width: 1000,
                height: 300
            }
        );
    }

    #[test]
    fn centered_rect_zero_preferred() {
        let preferred = Size { w: 0, h: 0 };
        let work_area = Rect {
            x: 50,
            y: 50,
            width: 1000,
            height: 800,
        };
        let result = FloatingSpace::centered_rect(preferred, work_area);
        // x = 50 + (1000 - 0) / 2 = 550; y = 50 + (800 - 0) / 2 = 450
        assert_eq!(
            result,
            Rect {
                x: 550,
                y: 450,
                width: 0,
                height: 0
            }
        );
    }

    #[test]
    fn centered_rect_negative_preferred() {
        let preferred = Size { w: -100, h: -200 };
        let work_area = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let result = FloatingSpace::centered_rect(preferred, work_area);
        // Negative → clamped to 0, positioned at center
        assert_eq!(
            result,
            Rect {
                x: 960,
                y: 540,
                width: 0,
                height: 0
            }
        );
    }

    // --- add / contains / len round-trip ---

    #[test]
    fn add_then_contains_and_len() {
        let mut fs = FloatingSpace::new();
        assert!(fs.is_empty());
        assert_eq!(fs.len(), 0);

        let id = WindowId(1);
        let rect = Rect {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        fs.add(id, rect);
        assert!(!fs.is_empty());
        assert_eq!(fs.len(), 1);
        assert!(fs.contains(id));
        assert!(!fs.contains(WindowId(99)));
    }

    #[test]
    fn add_duplicate_updates_rect_no_duplicate() {
        let mut fs = FloatingSpace::new();
        let id = WindowId(1);
        let rect1 = Rect {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let rect2 = Rect {
            x: 500,
            y: 500,
            width: 200,
            height: 200,
        };
        fs.add(id, rect1);
        assert_eq!(fs.len(), 1);
        fs.add(id, rect2);
        assert_eq!(fs.len(), 1);
        assert_eq!(fs.windows()[0].rect, rect2);
    }

    // --- remove tests ---

    #[test]
    fn remove_existing_returns_old_rect() {
        let mut fs = FloatingSpace::new();
        let id = WindowId(1);
        let rect = Rect {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        fs.add(id, rect);
        let old = fs.remove(id);
        assert_eq!(old, Some(rect));
        assert!(!fs.contains(id));
        assert!(fs.is_empty());
    }

    #[test]
    fn remove_absent_returns_none() {
        let mut fs = FloatingSpace::new();
        let result = fs.remove(WindowId(99));
        assert_eq!(result, None);
        assert!(fs.is_empty());
    }

    // --- to_actual_layout round-trip ---

    #[test]
    fn to_actual_layout_preserves_entries() {
        let mut fs = FloatingSpace::new();
        let id1 = WindowId(1);
        let id2 = WindowId(2);
        let r1 = Rect {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let r2 = Rect {
            x: 200,
            y: 200,
            width: 500,
            height: 400,
        };
        fs.add(id1, r1);
        fs.add(id2, r2);
        let layout = fs.to_actual_layout();
        assert_eq!(layout.entries.len(), 2);
        assert_eq!(
            layout.entries[0],
            ActualEntry {
                window_id: id1,
                rect: r1
            }
        );
        assert_eq!(
            layout.entries[1],
            ActualEntry {
                window_id: id2,
                rect: r2
            }
        );
    }

    // --- z-order ---

    #[test]
    fn z_order_newest_on_top() {
        let mut fs = FloatingSpace::new();
        fs.add(
            WindowId(1),
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
        );
        fs.add(
            WindowId(2),
            Rect {
                x: 0,
                y: 0,
                width: 200,
                height: 200,
            },
        );
        fs.add(
            WindowId(3),
            Rect {
                x: 0,
                y: 0,
                width: 300,
                height: 300,
            },
        );
        let entries = fs.windows();
        assert_eq!(entries[0].window_id, WindowId(1));
        assert_eq!(entries[1].window_id, WindowId(2));
        assert_eq!(entries[2].window_id, WindowId(3));
    }
}
