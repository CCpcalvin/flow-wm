//! Pure, horizontal-only screen-edge band classifier for edge-hover-scroll.
//!
//! Given a cursor position, a monitor work-area [`Rect`], and a band width,
//! this returns `Some(Direction::Left)` / `Some(Direction::Right)` when the
//! cursor sits inside the left/right edge band of the work area, or `None`
//! otherwise (interior, above/below the work area, or off the work area).
//!
//! This is a **deliberately separate** test from the drag drop-zone resolver
//! ([`resolve_drop_zone`](crate::layout::preview::resolve_drop_zone)): that
//! resolver is drag-coupled (it takes the dragged window and returns column/row
//! drop targets). Hover does not drag and does not care about drop targets — it
//! only needs a pure geometric screen-edge test, so this leaf function owns it.
//!
//! See (`docs/src/dev-guide/hover.md`) for the edge-band precedence rationale.

use crate::common::{Direction, Point, Rect};

/// Classify the cursor against the horizontal edge bands of `work_area`.
///
/// The left band is the `band_width`-pixel strip at the work area's left edge;
/// the right band is the matching strip at the right edge. The cursor must be
/// within the work area's vertical extent to count: a cursor pinned to the left
/// edge but above/below the work area (e.g. in a top taskbar region) is **not**
/// in a band. Only [`Direction::Left`] / [`Direction::Right`] are ever
/// returned — edge-hover-scroll is horizontal-only.
///
/// `band_width <= 0` disables the bands entirely (always `None`). When the
/// bands overlap (a `band_width` larger than half the work area), the **left**
/// band wins, matching the left-first check order — a deterministic rule for a
/// degenerate configuration.
///
/// # Boundaries
///
/// A band spans `[edge, edge + band_width)` (half-open, width `band_width`).
/// The pixel exactly at `edge + band_width` is therefore **not** in the band —
/// it is the first interior pixel. (`edge` itself is in the band.)
///
/// # Examples
///
/// ```
/// # use flow_wm::common::{Direction, Point, Rect};
/// # use flow_wm::hover::edge_band_direction;
/// let area = Rect { x: 0, y: 0, width: 1920, height: 1080 };
/// // Far-left edge → Left band.
/// assert_eq!(edge_band_direction(Point { x: 0, y: 500 }, area, 8), Some(Direction::Left));
/// // Far-right edge → Right band.
/// assert_eq!(edge_band_direction(Point { x: 1919, y: 500 }, area, 8), Some(Direction::Right));
/// // Interior → None.
/// assert_eq!(edge_band_direction(Point { x: 960, y: 500 }, area, 8), None);
/// ```
#[must_use]
pub fn edge_band_direction(cursor: Point, work_area: Rect, band_width: i32) -> Option<Direction> {
    // A non-positive band width means "no edge bands".
    if band_width <= 0 {
        return None;
    }
    // The edge band spans the work area's full vertical extent. A cursor above
    // or below the work area is not in any band.
    if !(cursor.y >= work_area.y && cursor.y < work_area.bottom()) {
        return None;
    }
    // Left band: [work_area.x, work_area.x + band_width).
    if cursor.x >= work_area.x && cursor.x < work_area.x + band_width {
        return Some(Direction::Left);
    }
    // Right band: [work_area.right() - band_width, work_area.right()).
    if cursor.x >= work_area.right() - band_width && cursor.x < work_area.right() {
        return Some(Direction::Right);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A typical 1920×1080 work area anchored at the origin, used across tests.
    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    const BW: i32 = 8;

    // --- Interior / off-band ---

    #[test]
    fn interior_point_is_none() {
        assert_eq!(
            edge_band_direction(Point { x: 960, y: 540 }, AREA, BW),
            None
        );
    }

    #[test]
    fn center_is_none() {
        assert_eq!(
            edge_band_direction(Point { x: 960, y: 540 }, AREA, BW),
            None
        );
    }

    // --- Left edge band ---

    #[test]
    fn left_edge_pixel_is_left_band() {
        assert_eq!(
            edge_band_direction(Point { x: 0, y: 500 }, AREA, BW),
            Some(Direction::Left)
        );
    }

    #[test]
    fn inside_left_band_is_left() {
        assert_eq!(
            edge_band_direction(Point { x: 7, y: 500 }, AREA, BW),
            Some(Direction::Left)
        );
    }

    #[test]
    fn last_pixel_of_left_band_is_left() {
        // band is [0, 8); x=7 is the last band pixel.
        assert_eq!(
            edge_band_direction(Point { x: 7, y: 500 }, AREA, BW),
            Some(Direction::Left)
        );
    }

    #[test]
    fn first_pixel_past_left_band_is_none() {
        // x = 8 is the band-width boundary: half-open, so it is interior.
        assert_eq!(edge_band_direction(Point { x: 8, y: 500 }, AREA, BW), None);
    }

    // --- Right edge band ---

    #[test]
    fn right_edge_pixel_is_right_band() {
        // right = 1920; x = 1919 is the last in-area pixel → Right band.
        assert_eq!(
            edge_band_direction(Point { x: 1919, y: 500 }, AREA, BW),
            Some(Direction::Right)
        );
    }

    #[test]
    fn first_pixel_of_right_band_is_right() {
        // right band is [1920-8, 1920) = [1912, 1920); x = 1912 is the first.
        assert_eq!(
            edge_band_direction(Point { x: 1912, y: 500 }, AREA, BW),
            Some(Direction::Right)
        );
    }

    #[test]
    fn last_pixel_before_right_band_is_none() {
        // x = 1911 is just before the right band → interior.
        assert_eq!(
            edge_band_direction(Point { x: 1911, y: 500 }, AREA, BW),
            None
        );
    }

    #[test]
    fn exactly_at_right_edge_is_none() {
        // x = 1920 is past the work area (half-open right edge) → None.
        assert_eq!(
            edge_band_direction(Point { x: 1920, y: 500 }, AREA, BW),
            None
        );
    }

    // --- Corners (band + band-edge of vertical extent) ---

    #[test]
    fn top_left_corner_is_left_band() {
        assert_eq!(
            edge_band_direction(Point { x: 0, y: 0 }, AREA, BW),
            Some(Direction::Left)
        );
    }

    #[test]
    fn top_right_corner_is_right_band() {
        assert_eq!(
            edge_band_direction(Point { x: 1919, y: 0 }, AREA, BW),
            Some(Direction::Right)
        );
    }

    #[test]
    fn bottom_left_corner_is_left_band() {
        // bottom = 1080; last in-area vertical pixel is y = 1079.
        assert_eq!(
            edge_band_direction(Point { x: 0, y: 1079 }, AREA, BW),
            Some(Direction::Left)
        );
    }

    #[test]
    fn bottom_right_corner_is_right_band() {
        assert_eq!(
            edge_band_direction(Point { x: 1919, y: 1079 }, AREA, BW),
            Some(Direction::Right)
        );
    }

    // --- Vertical extent: above/below the work area is not a band ---

    #[test]
    fn above_work_area_at_left_edge_is_none() {
        // y = -1 is above the work area even though x is in the left band.
        assert_eq!(edge_band_direction(Point { x: 0, y: -1 }, AREA, BW), None);
    }

    #[test]
    fn at_bottom_edge_is_none() {
        // y = 1080 is the half-open bottom boundary → outside.
        assert_eq!(edge_band_direction(Point { x: 0, y: 1080 }, AREA, BW), None);
    }

    #[test]
    fn below_work_area_at_right_edge_is_none() {
        assert_eq!(
            edge_band_direction(Point { x: 1919, y: 1080 }, AREA, BW),
            None
        );
    }

    // --- Band width boundary ---

    #[test]
    fn larger_band_width_widens_the_band() {
        // With a 20px band, x = 15 is now in the left band (was interior at BW=8).
        assert_eq!(
            edge_band_direction(Point { x: 15, y: 500 }, AREA, 20),
            Some(Direction::Left)
        );
        // ...and x = 20 is the new boundary (interior).
        assert_eq!(edge_band_direction(Point { x: 20, y: 500 }, AREA, 20), None);
    }

    #[test]
    fn band_width_boundary_is_half_open_on_both_sides() {
        // BW=8: left band [0,8), right band [1912,1920). x=8 interior, x=1912 right.
        assert_eq!(edge_band_direction(Point { x: 8, y: 500 }, AREA, BW), None);
        assert_eq!(
            edge_band_direction(Point { x: 1912, y: 500 }, AREA, BW),
            Some(Direction::Right)
        );
    }

    // --- Degenerate configurations ---

    #[test]
    fn zero_band_width_is_none_everywhere() {
        for &x in &[0, 1, 959, 960, 1919] {
            assert_eq!(edge_band_direction(Point { x, y: 540 }, AREA, 0), None);
        }
    }

    #[test]
    fn negative_band_width_is_none_everywhere() {
        assert_eq!(edge_band_direction(Point { x: 0, y: 540 }, AREA, -5), None);
    }

    #[test]
    fn overlapping_bands_left_wins() {
        // band_width larger than half the work area → bands overlap. Left is
        // checked first, so a point in the overlap resolves to Left.
        let big_bw = AREA.width; // the whole width is "left band" then.
        assert_eq!(
            edge_band_direction(Point { x: 1900, y: 540 }, AREA, big_bw),
            Some(Direction::Left)
        );
    }

    // --- Offset (non-origin) work area ---

    #[test]
    fn offset_work_area_left_band() {
        // A second-monitor-style work area offset to x=1920.
        let area = Rect {
            x: 1920,
            y: 0,
            width: 1280,
            height: 1024,
        };
        assert_eq!(
            edge_band_direction(Point { x: 1920, y: 500 }, area, BW),
            Some(Direction::Left)
        );
        // right edge = 1920 + 1280 = 3200.
        assert_eq!(
            edge_band_direction(Point { x: 3199, y: 500 }, area, BW),
            Some(Direction::Right)
        );
        // Boundary just past left band.
        assert_eq!(
            edge_band_direction(Point { x: 1928, y: 500 }, area, BW),
            None
        );
    }

    #[test]
    fn offset_work_area_with_top_offset() {
        let area = Rect {
            x: 0,
            y: 40,
            width: 1920,
            height: 1040,
        };
        // y must be >= 40 now.
        assert_eq!(edge_band_direction(Point { x: 0, y: 39 }, area, BW), None);
        assert_eq!(
            edge_band_direction(Point { x: 0, y: 40 }, area, BW),
            Some(Direction::Left)
        );
    }

    // --- Only Left/Right are ever returned ---

    #[test]
    fn never_returns_up_or_down() {
        // Sample a dense grid; none should ever be Up/Down.
        for x in (0..=1920).step_by(60) {
            for y in (0..=1080).step_by(60) {
                let r = edge_band_direction(Point { x, y }, AREA, BW);
                assert!(
                    matches!(r, None | Some(Direction::Left) | Some(Direction::Right)),
                    "got {:?} at ({x},{y})",
                    r
                );
            }
        }
    }
}
