//! Geometry primitives, direction enum, and platform-independent window handle.
//!
//! These types are the vocabulary shared by every subsystem in stm. They contain
//! no platform-specific logic — the actual Win32 conversion (`WindowId` → `HWND`)
//! happens only in the (future) `registry` module.

/// Axis-parallel rectangle with integer pixel coordinates.
///
/// This is the **frozen cross-layer contract** between [`LayoutEngine`](crate::layout::LayoutEngine)
/// and the Win32 compositor. The field layout (`x`, `y`, `width`, `height`) must not change.
///
/// After projection, every [`ActualEntry`](crate::layout::ActualEntry) carries a `Rect`
/// representing the final `SetWindowPos` coordinates — padding already baked in.
///
/// # Overlap Semantics
///
/// Touching rectangles are **not** overlapping (consistent with Win32 `IntersectRect`):
///
/// ```
/// # use scrolling_tiling_manager::common::Rect;
/// let a = Rect { x: 0, y: 0, width: 100, height: 100 };
/// let b = Rect { x: 100, y: 0, width: 100, height: 100 };
/// assert!(!a.overlaps(b)); // touching at x=100 → no overlap
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    /// Left edge X coordinate.
    pub x: i32,
    /// Top edge Y coordinate.
    pub y: i32,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
}

/// 2D size in integer pixels.
///
/// Used for preferred window sizes and monitor dimensions where position
/// is not relevant (unlike [`Rect`] which includes position).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Size {
    /// Width in pixels.
    pub w: i32,
    /// Height in pixels.
    pub h: i32,
}

/// 2D point with integer pixel coordinates.
///
/// Used for absolute screen positions (e.g., viewport offset calculations).
/// For positioned+bounded regions, prefer [`Rect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Point {
    /// Horizontal coordinate.
    pub x: i32,
    /// Vertical coordinate.
    pub y: i32,
}

/// Cardinal direction for focus, swap, and resize operations.
///
/// - `Left`/`Right` operate on **columns** (horizontal container in [`VirtualLayout`](crate::layout::VirtualLayout))
/// - `Up`/`Down` operate on **rows** within a column (vertical container in [`Column`](crate::layout::Column))
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Direction {
    /// Move focus/swap to the left column.
    Left,
    /// Move focus/swap to the right column.
    Right,
    /// Move focus/swap to the row above (within column).
    Up,
    /// Move focus/swap to the row below (within column).
    Down,
}

/// Platform-independent opaque window handle.
///
/// Wraps the OS-native window handle as an `isize`. On Windows, this stores
/// the HWND value. The actual Win32 conversion (`HWND` ↔ `WindowId`) happens
/// only in the (future) `registry` module.
///
/// This is the **bridge type** between [`LayoutEngine`](crate::layout::LayoutEngine)
/// (which only knows `WindowId`) and `WindowRegistry` (which knows HWNDs).
///
/// # Usage as map key
///
/// `WindowId` implements `Hash` + `Eq`, so it works as a `HashMap`/`HashSet` key:
///
/// ```
/// # use scrolling_tiling_manager::common::WindowId;
/// # use std::collections::HashSet;
/// let mut set = HashSet::new();
/// set.insert(WindowId(1));
/// assert!(set.contains(&WindowId(1)));
/// ```
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

    // --- Direction serialization roundtrip tests ---

    #[test]
    fn direction_serialize_roundtrip_all_variants() {
        // Positive: all 4 Direction variants roundtrip through JSON
        for dir in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            let json = serde_json::to_string(&dir).unwrap();
            let parsed: Direction = serde_json::from_str(&json).unwrap();
            assert_eq!(dir, parsed, "Direction roundtrip failed for: {dir:?}");
        }
    }

    #[test]
    fn direction_serialized_values() {
        // Positive: Direction serializes to the expected string form
        assert_eq!(
            serde_json::to_string(&Direction::Left).unwrap(),
            r#""Left""#
        );
        assert_eq!(
            serde_json::to_string(&Direction::Right).unwrap(),
            r#""Right""#
        );
        assert_eq!(serde_json::to_string(&Direction::Up).unwrap(), r#""Up""#);
        assert_eq!(
            serde_json::to_string(&Direction::Down).unwrap(),
            r#""Down""#
        );
    }

    #[test]
    fn direction_deserialize_invalid_returns_none() {
        // Negative: invalid direction string fails deserialization
        let result: Result<Direction, _> = serde_json::from_str(r#""diagonal""#);
        assert!(
            result.is_err(),
            "invalid direction should fail to deserialize"
        );
    }

    #[test]
    fn direction_deserialize_wrong_type_fails() {
        // Negative: number instead of string fails deserialization
        let result: Result<Direction, _> = serde_json::from_str("42");
        assert!(
            result.is_err(),
            "number should not deserialize as Direction"
        );
    }
}
