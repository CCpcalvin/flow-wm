//! Geometry primitives, direction enum, and platform-independent window handle.
//!
//! These types are the vocabulary shared by every subsystem in flow. They contain
//! no platform-specific logic — the actual Win32 conversion (`WindowId` → `HWND`)
//! happens only in the (future) `registry` module.

/// Axis-parallel rectangle with integer pixel coordinates.
///
/// This is the **frozen cross-layer contract** between [`ScrollingSpace`](crate::workspace::ScrollingSpace)
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
/// # use flow_wm::common::Rect;
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
/// This is the **bridge type** between [`ScrollingSpace`](crate::workspace::ScrollingSpace)
/// (which only knows `WindowId`) and `WindowRegistry` (which knows HWNDs).
///
/// # Usage as map key
///
/// `WindowId` implements `Hash` + `Eq`, so it works as a `HashMap`/`HashSet` key:
///
/// ```
/// # use flow_wm::common::WindowId;
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

    /// Shrinks this rectangle by `amount` on every edge.
    ///
    /// Used by the border overlay subsystem to make room for the colored ring
    /// around each managed window: the daemon issues `SetWindowPos` with the
    /// inset rect so the overlay (drawn slightly outside the content rect)
    /// remains visible without covering the window.
    ///
    /// Clamps each dimension to a minimum of 1×1 when `2 * amount` would
    /// empty or invert the rect — this is a defensive floor so a misconfigured
    /// `thickness` (e.g. 50 px on a 30 px window) never produces a negative-
    /// size rect that would confuse `SetWindowPos`.
    ///
    /// # Negative values
    ///
    /// A negative `amount` would *outset* the rect. Callers within flow always
    /// pass `u32 as i32` (capped at 50 by `BorderConfig::validate`), so this
    /// is unreachable in practice. The multiplication uses `saturating_mul`
    /// defensively so an accidental huge value cannot debug-panic.
    #[must_use]
    pub fn inset(self, amount: i32) -> Rect {
        let shrink = amount.saturating_mul(2);
        let new_width = (self.width - shrink).max(1);
        let new_height = (self.height - shrink).max(1);
        Rect {
            x: self.x + amount,
            y: self.y + amount,
            width: new_width,
            height: new_height,
        }
    }
    /// Returns a rect positioned to fit inside `bounds`, preserving size where
    /// possible.
    ///
    /// If this rect is larger than `bounds` in either dimension it is shrunk
    /// to `bounds`' size (a window wider than the work area cannot fit). The
    /// position is then clamped so the result lies wholly inside `bounds`:
    /// shifted right if its left edge is left of `bounds`, left if its right
    /// edge is past `bounds.right()`, and similarly on the vertical axis.
    ///
    /// Used by the shutdown rescue pass
    /// (`docs/src/dev-guide/ipc-and-watchdog.md`) to bring an off-screen
    /// `pre_manage_rect` anchor into the active monitor's `work_area`.
    #[must_use]
    pub fn clamped_into(self, bounds: Rect) -> Rect {
        // Shrink-to-fit first so the clamp range is non-empty (max >= min);
        // otherwise i32::clamp would panic in debug builds for a window
        // larger than bounds.
        let width = self.width.min(bounds.width).max(0);
        let height = self.height.min(bounds.height).max(0);
        let x = self
            .x
            .clamp(bounds.x, (bounds.right() - width).max(bounds.x));
        let y = self
            .y
            .clamp(bounds.y, (bounds.bottom() - height).max(bounds.y));
        Rect {
            x,
            y,
            width,
            height,
        }
    }
}

/// Pixel offsets between the full window rect and the visible rect.
///
/// On Windows 10/11, most top-level windows have invisible borders used for
/// shadows and resize hit-testing. `GetWindowRect` returns the larger rect
/// including these borders, while `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)`
/// returns the smaller visible rect that the user actually sees.
///
/// This struct stores the per-edge difference and provides conversion methods
/// between the two coordinate spaces.
///
/// # Coordinate Relationship
///
/// ```text
/// Window rect (GetWindowRect):
/// ┌──────────────────────────────────────┐
/// │ invisible │                  │ invisi │
/// │  border   │  Visible content │  ble   │
/// │ (left)    │                  │ border │
/// │           │                  │(right) │
/// │           │                  │        │
/// │           ├──────────────────┤        │  ← top of visible
/// │           │                  │        │
/// └──────────────────────────────────────┘
///              ↑                   ↑
///           visible left      visible right
///           window left is further left, window right is further right
/// ```
///
/// All fields are ≥ 0 in normal operation. A window with no invisible borders
/// (e.g., a borderless fullscreen window) has all zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct InvisibleBounds {
    /// Invisible border width on the left edge (visible_left - window_left).
    pub left: i32,
    /// Invisible border height on the top edge (visible_top - window_top).
    pub top: i32,
    /// Invisible border width on the right edge (window_right - visible_right).
    pub right: i32,
    /// Invisible border height on the bottom edge (window_bottom - visible_bottom).
    pub bottom: i32,
}

impl InvisibleBounds {
    /// Creates an `InvisibleBounds` with all edges set to zero.
    ///
    /// Represents a window with no invisible borders (e.g., borderless
    /// fullscreen). Used as a safe fallback when DWM queries fail.
    #[must_use]
    pub fn zero() -> Self {
        Self {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        }
    }

    /// Converts a **visible rect** to the corresponding **window rect**.
    ///
    /// Expands the rect outward by the invisible border amounts. This is
    /// used when translating layout-engine output (visible rects) into
    /// `SetWindowPos` coordinates (window rects).
    ///
    /// # Example
    ///
    /// ```
    /// # use flow_wm::common::{InvisibleBounds, Rect};
    /// let bounds = InvisibleBounds { left: 7, top: 0, right: 7, bottom: 7 };
    /// let visible = Rect { x: 100, y: 0, width: 800, height: 600 };
    /// let window = bounds.visible_to_window(visible);
    /// // Window rect is larger and shifted left
    /// assert_eq!(window.x, 93);       // 100 - 7
    /// assert_eq!(window.y, 0);        // 0 - 0
    /// assert_eq!(window.width, 814);  // 800 + 7 + 7
    /// assert_eq!(window.height, 607); // 600 + 0 + 7
    /// ```
    #[must_use]
    pub fn visible_to_window(self, visible: Rect) -> Rect {
        Rect {
            x: visible.x - self.left,
            y: visible.y - self.top,
            width: visible.width + self.left + self.right,
            height: visible.height + self.top + self.bottom,
        }
    }

    /// Converts a **window rect** to the corresponding **visible rect**.
    ///
    /// Shrinks the rect inward by the invisible border amounts. This is
    /// the inverse of [`visible_to_window`](Self::visible_to_window).
    ///
    /// # Example
    ///
    /// ```
    /// # use flow_wm::common::{InvisibleBounds, Rect};
    /// let bounds = InvisibleBounds { left: 7, top: 0, right: 7, bottom: 7 };
    /// let window = Rect { x: 93, y: 0, width: 814, height: 607 };
    /// let visible = bounds.window_to_visible(window);
    /// assert_eq!(visible, Rect { x: 100, y: 0, width: 800, height: 600 });
    /// ```
    #[must_use]
    pub fn window_to_visible(self, window: Rect) -> Rect {
        Rect {
            x: window.x + self.left,
            y: window.y + self.top,
            width: window.width - self.left - self.right,
            height: window.height - self.top - self.bottom,
        }
    }
}

impl Default for InvisibleBounds {
    /// Returns all-zeros bounds (no invisible borders).
    fn default() -> Self {
        Self::zero()
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
    fn rect_inset_basic() {
        let r = Rect {
            x: 100,
            y: 200,
            width: 300,
            height: 400,
        };
        assert_eq!(
            r.inset(5),
            Rect {
                x: 105,
                y: 205,
                width: 290,
                height: 390,
            }
        );
    }

    #[test]
    fn rect_inset_zero_is_identity() {
        let r = Rect {
            x: 7,
            y: 11,
            width: 42,
            height: 53,
        };
        assert_eq!(r.inset(0), r);
    }

    #[test]
    fn rect_inset_clamps_to_min_1x1() {
        // 2*amount (30) exceeds width (20) and height (15) — clamp applies.
        let r = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 15,
        };
        assert_eq!(
            r.inset(15),
            Rect {
                x: 15,
                y: 15,
                width: 1,
                height: 1,
            }
        );
    }

    #[test]
    fn rect_inset_exact_half_produces_1x1() {
        // 2*amount == width and height → result is exactly 1×1 (not 0×0).
        let r = Rect {
            x: 10,
            y: 10,
            width: 10,
            height: 10,
        };
        assert_eq!(
            r.inset(5),
            Rect {
                x: 15,
                y: 15,
                width: 1,
                height: 1,
            }
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

    // --- InvisibleBounds tests ---

    #[test]
    fn invisible_bounds_zero_all_zeros() {
        let b = InvisibleBounds::zero();
        assert_eq!(b.left, 0);
        assert_eq!(b.top, 0);
        assert_eq!(b.right, 0);
        assert_eq!(b.bottom, 0);
    }

    #[test]
    fn invisible_bounds_default_is_zero() {
        assert_eq!(InvisibleBounds::default(), InvisibleBounds::zero());
    }

    #[test]
    fn invisible_bounds_visible_to_window_expands() {
        let bounds = InvisibleBounds {
            left: 7,
            top: 0,
            right: 7,
            bottom: 7,
        };
        let visible = Rect {
            x: 100,
            y: 0,
            width: 800,
            height: 600,
        };
        let window = bounds.visible_to_window(visible);
        assert_eq!(window.x, 93);
        assert_eq!(window.y, 0);
        assert_eq!(window.width, 814);
        assert_eq!(window.height, 607);
    }

    #[test]
    fn invisible_bounds_window_to_visible_shrinks() {
        let bounds = InvisibleBounds {
            left: 7,
            top: 0,
            right: 7,
            bottom: 7,
        };
        let window = Rect {
            x: 93,
            y: 0,
            width: 814,
            height: 607,
        };
        let visible = bounds.window_to_visible(window);
        assert_eq!(visible.x, 100);
        assert_eq!(visible.y, 0);
        assert_eq!(visible.width, 800);
        assert_eq!(visible.height, 600);
    }

    #[test]
    fn invisible_bounds_roundtrip_visible_to_window_to_visible() {
        // Positive: converting visible→window→visible should be identity
        let bounds = InvisibleBounds {
            left: 7,
            top: 3,
            right: 7,
            bottom: 7,
        };
        let original = Rect {
            x: 500,
            y: 200,
            width: 1000,
            height: 800,
        };
        let roundtrip = bounds.window_to_visible(bounds.visible_to_window(original));
        assert_eq!(original, roundtrip);
    }

    #[test]
    fn invisible_bounds_zero_bounds_is_identity() {
        // Positive: zero bounds means visible_to_window and window_to_visible are identity
        let bounds = InvisibleBounds::zero();
        let rect = Rect {
            x: 100,
            y: 200,
            width: 300,
            height: 400,
        };
        assert_eq!(bounds.visible_to_window(rect), rect);
        assert_eq!(bounds.window_to_visible(rect), rect);
    }

    #[test]
    fn invisible_bounds_asymmetric() {
        // Positive: asymmetric bounds (common on Windows — top=0, others=7)
        let bounds = InvisibleBounds {
            left: 7,
            top: 0,
            right: 7,
            bottom: 7,
        };
        let visible = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let window = bounds.visible_to_window(visible);
        // Top has no invisible border, so y stays at 0
        assert_eq!(window.y, 0);
        // But x goes negative (window extends further left)
        assert_eq!(window.x, -7);
        assert_eq!(window.width, 114); // 100 + 7 + 7
        assert_eq!(window.height, 107); // 100 + 0 + 7
    }

    #[test]
    fn invisible_bounds_serialize_roundtrip() {
        let bounds = InvisibleBounds {
            left: 7,
            top: 0,
            right: 7,
            bottom: 7,
        };
        let json = serde_json::to_string(&bounds).unwrap();
        let parsed: InvisibleBounds = serde_json::from_str(&json).unwrap();
        assert_eq!(bounds, parsed);
    }

    // --- Rect::clamped_into tests ---

    #[test]
    fn clamped_into_unchanged_when_already_inside() {
        // Positive: a rect fully inside bounds stays exactly where it is.
        let bounds = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let r = Rect {
            x: 100,
            y: 100,
            width: 800,
            height: 600,
        };
        assert_eq!(r.clamped_into(bounds), r);
    }

    #[test]
    fn clamped_into_shifts_left_edge_inward() {
        // Positive: left edge before bounds.x is shifted right to bounds.x.
        let bounds = Rect {
            x: 100,
            y: 0,
            width: 1000,
            height: 1000,
        };
        let r = Rect {
            x: -200,
            y: 50,
            width: 400,
            height: 400,
        };
        let clamped = r.clamped_into(bounds);
        assert_eq!(clamped.x, 100);
        assert_eq!(clamped.y, 50);
        assert_eq!(clamped.width, 400);
        assert_eq!(clamped.height, 400);
    }

    #[test]
    fn clamped_into_shifts_right_edge_inward() {
        // Positive: right edge past bounds.right() is shifted left to fit.
        let bounds = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        };
        let r = Rect {
            x: 900,
            y: 0,
            width: 400,
            height: 400,
        };
        let clamped = r.clamped_into(bounds);
        assert_eq!(clamped.x, 600); // 1000 - 400
        assert_eq!(clamped.width, 400);
    }

    #[test]
    fn clamped_into_shifts_top_edge_inward() {
        // Positive: top edge before bounds.y is shifted down to bounds.y.
        let bounds = Rect {
            x: 0,
            y: 100,
            width: 1000,
            height: 1000,
        };
        let r = Rect {
            x: 50,
            y: -300,
            width: 200,
            height: 200,
        };
        let clamped = r.clamped_into(bounds);
        assert_eq!(clamped.y, 100);
    }

    #[test]
    fn clamped_into_shifts_bottom_edge_inward() {
        // Positive: bottom edge past bounds.bottom() is shifted up to fit.
        let bounds = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        };
        let r = Rect {
            x: 0,
            y: 900,
            width: 200,
            height: 200,
        };
        let clamped = r.clamped_into(bounds);
        assert_eq!(clamped.y, 800); // 1000 - 200
    }

    #[test]
    fn clamped_into_shrinks_when_larger_than_bounds() {
        // Positive: a window bigger than bounds collapses to bounds itself.
        let bounds = Rect {
            x: 0,
            y: 0,
            width: 500,
            height: 400,
        };
        let r = Rect {
            x: -100,
            y: -100,
            width: 2000,
            height: 2000,
        };
        assert_eq!(r.clamped_into(bounds), bounds);
    }

    #[test]
    fn clamped_into_exactly_filling_bounds_is_unchanged() {
        // Positive: a rect that fills bounds exactly (touching all edges) is
        // not shifted — touching is allowed, consistent with `overlaps`.
        let bounds = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        };
        let r = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        };
        assert_eq!(r.clamped_into(bounds), r);
    }

    #[test]
    fn clamped_into_offset_bounds() {
        // Positive: bounds not anchored at origin clamp correctly on both axes.
        let bounds = Rect {
            x: 1920,
            y: 0,
            width: 1280,
            height: 1024,
        };
        let r = Rect {
            x: 0,
            y: 0,
            width: 600,
            height: 400,
        };
        let clamped = r.clamped_into(bounds);
        assert_eq!(clamped.x, 1920);
        assert_eq!(clamped.y, 0);
        assert_eq!(clamped.width, 600);
        assert_eq!(clamped.height, 400);
    }
}
