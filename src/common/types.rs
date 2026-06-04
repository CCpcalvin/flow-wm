//! Geometry types and direction enum.

/// Axis-parallel rectangle with integer pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// 2D size in integer pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

/// 2D point with integer pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// Cardinal direction for focus and swap operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Platform-independent window identifier.
///
/// Wraps the OS-native window handle as an opaque `isize`.
/// On Windows, this stores the HWND value. The actual Win32
/// conversion happens only in the `registry` and `win32` modules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(pub isize);

impl Rect {
    /// Returns `true` if this rectangle has zero area.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    /// Returns the right edge coordinate.
    #[must_use]
    pub fn right(self) -> i32 {
        self.x + self.width
    }

    /// Returns the bottom edge coordinate.
    #[must_use]
    pub fn bottom(self) -> i32 {
        self.y + self.height
    }

    /// Returns `true` if this rectangle overlaps with `other`.
    #[must_use]
    pub fn overlaps(self, other: Rect) -> bool {
        self.x < other.right()
            && self.right() > other.x
            && self.y < other.bottom()
            && self.bottom() > other.y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_overlaps_self() {
        let r = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert!(r.overlaps(r));
    }

    #[test]
    fn rect_no_overlap_disjoint() {
        let a = Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        };
        let b = Rect {
            x: 100,
            y: 100,
            width: 50,
            height: 50,
        };
        assert!(!a.overlaps(b));
    }

    #[test]
    fn rect_is_empty() {
        assert!(
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 10
            }
            .is_empty()
        );
        assert!(
            !Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            }
            .is_empty()
        );
    }

    // --- Additional common type tests ---

    #[test]
    fn rect_is_empty_negative_width() {
        // Negative: negative width → empty
        assert!(
            Rect {
                x: 0,
                y: 0,
                width: -5,
                height: 100
            }
            .is_empty()
        );
    }

    #[test]
    fn rect_is_empty_negative_height() {
        // Negative: negative height → empty
        assert!(
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: -1
            }
            .is_empty()
        );
    }

    #[test]
    fn rect_right_and_bottom() {
        assert_eq!(
            Rect {
                x: 10,
                y: 20,
                width: 100,
                height: 200
            }
            .right(),
            110
        );
        assert_eq!(
            Rect {
                x: 10,
                y: 20,
                width: 100,
                height: 200
            }
            .bottom(),
            220
        );
    }

    #[test]
    fn rect_overlaps_adjacent_no_overlap() {
        // Negative: adjacent (touching) rectangles don't overlap
        let a = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let b = Rect {
            x: 100,
            y: 0,
            width: 100,
            height: 100,
        };
        assert!(!a.overlaps(b));
    }

    #[test]
    fn rect_overlaps_partial() {
        // Positive: partially overlapping
        let a = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let b = Rect {
            x: 50,
            y: 50,
            width: 100,
            height: 100,
        };
        assert!(a.overlaps(b));
        assert!(b.overlaps(a));
    }

    #[test]
    fn rect_overlaps_contained() {
        // Positive: one rect fully inside another
        let outer = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let inner = Rect {
            x: 25,
            y: 25,
            width: 50,
            height: 50,
        };
        assert!(outer.overlaps(inner));
    }

    #[test]
    fn direction_equality() {
        // Positive: Direction variants compare correctly
        assert_eq!(Direction::Left, Direction::Left);
        assert_ne!(Direction::Left, Direction::Right);
    }

    #[test]
    fn window_id_hashable() {
        // Positive: WindowId can be used in HashSet
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(WindowId(1));
        set.insert(WindowId(2));
        assert!(set.contains(&WindowId(1)));
        assert!(!set.contains(&WindowId(3)));
    }

    #[test]
    fn window_id_clone_and_copy() {
        // Positive: WindowId is Copy + Clone
        let id = WindowId(42);
        let copy = id;
        assert_eq!(id, copy);
    }
}
