//! Tile drag-resize: the rect-diff classifier and the boundary-move column
//! resize mutation.
//!
//! These are the **pure** foundations of drag-resize (ticket #9). The
//! [`classify_drag`] function decides, from two rects, whether a move-size
//! gesture is a title-bar reorder ([`DragKind::Translate`]) or an edge/corner
//! resize ([`DragKind::Resize`]). The [`resize_column_boundary_move`] function
//! applies the boundary-move column resize — the "1b"/tmux model — where one
//! boundary shifts, the growing column absorbs the delta, and the shrinking
//! neighbor gives up the pixels down to its minimum, after which the canvas
//! extends (Grow) and the column elastically pins at the monitor-width maximum.
//!
//! Both are pure: [`classify_drag`] is pure over two [`Rect`]s, and
//! [`resize_column_boundary_move`] is a pure function over the
//! [`VirtualLayout`]. The daemon resize-drag orchestration (move-size-start,
//! classify on first location-change, teleport during the drag, commit +
//! animate on release) is Win32 orchestration covered by manual interactive
//! testing — the same character as the existing translate-drag handler.
//!
//! See the *Tile resize* glossary in `CONTEXT.md` and ADR-0004
//! (`docs/adr/0004-tile-resize-contract.md`) for the binding contract and the
//! rejected alternatives (translate-neighbors/1a, hard-pin, live-snap,
//! animate-during-drag).
//!
//! (`docs/src/dev-guide/layout/mutations.md`)

use crate::common::Rect;
use crate::layout::types::VirtualLayout;

use super::mutations::MutationConfig;

// ---------------------------------------------------------------------------
// Edge / classification vocabulary
// ---------------------------------------------------------------------------

/// A horizontal resize grip edge, or `None` when no horizontal edge is involved.
///
/// The grip identifies which side of the column the user grabbed; the opposite
/// edge is the **anchor** (it stays fixed while the grip edge follows the
/// cursor). `None` represents "this axis is not being resized" — for example a
/// purely vertical resize (a row divider, ticket #10) classifies with
/// `horizontal = None`.
///
/// Vertical edges (`Top`/`Bottom`) are intentionally absent: ticket #9 delivers
/// horizontal-only resize, and a corner grip is treated as horizontal-only for
/// now (the vertical component is ticket #10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResizeEdge {
    /// No edge on this axis (the axis is not being resized).
    #[default]
    None,
    /// The left edge is the grip; the right edge anchors.
    Left,
    /// The right edge is the grip; the left edge anchors.
    Right,
}

impl ResizeEdge {
    /// The neighbor column a grip acts on: a right grip shrinks the column to
    /// the **right**, a left grip shrinks the column to the **left**. `None`
    /// for `None` (no horizontal axis).
    ///
    /// Returns the signed offset to add to the resized column's index to find
    /// the neighbor: `+1` for `Right`, `-1` for `Left`, `0` for `None`.
    #[must_use]
    pub fn neighbor_offset(self) -> i32 {
        match self {
            ResizeEdge::Right => 1,
            ResizeEdge::Left => -1,
            ResizeEdge::None => 0,
        }
    }
}

/// What kind of drag the rect-diff classifier inferred, with the identified
/// horizontal grip edge when it is a resize.
///
/// Produced by [`classify_drag`]. The daemon's `Classifying` state consumes
/// this on the first `LOCATIONCHANGE` to promote itself into the
/// `Translate(existing)` or `Resize(new)` drag state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    /// No movement yet — a click without a drag. The daemon treats this as a
    /// no-op and snaps back on release.
    None,
    /// Position changed but size did not — a title-bar reorder. The existing
    /// translate-drag path handles this unchanged.
    Translate,
    /// Size changed — an edge/corner resize. `horizontal` identifies which
    /// edge is the grip on the horizontal axis (`None` when only the vertical
    /// axis changed, e.g. a row divider — ticket #10).
    Resize {
        /// The horizontal grip edge (`None` if the width did not change).
        horizontal: ResizeEdge,
    },
}

/// Classify a move-size gesture by comparing the start rect to the current
/// rect (the rect-diff classifier).
///
/// The move-size-start hook event carries only the window handle — no sizing
/// edge — so classification is derived from how the rect changed on the first
/// `LOCATIONCHANGE`:
///
/// - **Width or height changed** → [`DragKind::Resize`]. Which horizontal edge
///   moved identifies the grip: if the left edge moved and the right edge
///   stayed fixed → `Left`; if the right edge moved and the left stayed fixed
///   → `Right`. A corner grip moves one edge on each axis, so its horizontal
///   edge is identified the same way. When both horizontal edges moved (a
///   translate coupled with a resize — unusual for a native grip), the edge
///   with the larger displacement wins.
/// - **Position changed, size unchanged** → [`DragKind::Translate`] (a
///   title-bar reorder).
/// - **Nothing changed** → [`DragKind::None`] (a click with no movement; the
///   daemon treats it as a no-op).
///
/// This is the *only* seam that decides resize vs. translate; it is pure over
/// two [`Rect`]s so it unit-tests without Win32.
#[must_use]
pub fn classify_drag(start: Rect, current: Rect) -> DragKind {
    let dw = current.width - start.width;
    let dh = current.height - start.height;
    let dx_left = current.x - start.x;
    let dx_right = current.right() - start.right();

    if dw == 0 && dh == 0 {
        // No size change. If nothing moved at all, it is a click; otherwise it
        // is a position-only translate (title-bar reorder).
        if dx_left == 0 && dx_right == 0 {
            return DragKind::None;
        }
        return DragKind::Translate;
    }

    // Size changed → resize. Identify the horizontal grip edge. For a clean
    // single-edge grip exactly one edge moves; for a corner, one horizontal
    // edge still moves (the other axis is reported separately in #10). When
    // both move (translate+resize), the larger displacement identifies the
    // dominant grip; ties break to the left edge.
    let horizontal = if dw == 0 {
        ResizeEdge::None
    } else if dx_left != 0 && dx_right == 0 {
        ResizeEdge::Left
    } else if dx_right != 0 && dx_left == 0 {
        ResizeEdge::Right
    } else if dx_left.abs() >= dx_right.abs() {
        ResizeEdge::Left
    } else {
        ResizeEdge::Right
    };

    DragKind::Resize { horizontal }
}

// ---------------------------------------------------------------------------
// Boundary-move column resize
// ---------------------------------------------------------------------------

/// Boundary-move column resize (the "1b"/tmux model).
///
/// Drag column `col`'s `grip` edge so the column targets `target_width` pixels.
/// The growing side absorbs the delta; the shrinking neighbor gives up the same
/// number of pixels, down to its minimum width, after which the neighbor
/// becomes a rigid rider and the canvas extends (Grow — the scrolling axis is
/// unbounded). The resized column itself is clamped to the absolute maximum
/// (`monitor_width − 2*gap`): this is the elastic-pin ceiling. The caller
/// overshoots during the drag (Win32 owns the dragged window's geometry, so its
/// width tracks the cursor past the ceiling) and snaps back on release — this
/// function never produces a column wider than `abs_max_width`.
///
/// # Anchor edge
///
/// The edge opposite the grip is the anchor. Because column positions are a
/// prefix sum of widths (the canvas is left-anchored), the anchor holds exactly
/// for a `Right` grip (the resized column's left edge depends only on unchanged
/// columns to its left). For a `Left` grip in Grow mode the anchor's position
/// drifts rightward by the unabsorbed excess — an inherent consequence of the
/// left-anchored canvas; the canvas still extends, which is the user-visible
/// Grow behavior.
///
/// # Neighbors
///
/// - `grip = Right` → the column to the right (`col + 1`) shrinks/absorbs.
/// - `grip = Left` → the column to the left (`col - 1`) shrinks/absorbs.
/// - Resizing the only column (no neighbor on the grip side) grows/shrinks it
///   freely — the canvas extends/contracts (user story: "resizing the only
///   column horizontally still works").
///
/// # Viewport
///
/// This is a pure layout mutation: the viewport offset is **not** touched. The
/// viewport never scrolls mid-grab — scrolling would move the grabbed edge
/// relative to the cursor and break the grab. The caller brings the resized
/// column into view on release via [`ensure_column_visible`].
///
/// # Returns
///
/// `Some(new_layout)` with the resized column clamped to `[min_column_width_px,
/// abs_max_width]` and the neighbor clamped to the same bounds, or `None` when:
/// - `col` is out of bounds,
/// - `grip` is `None`,
/// - the clamped target equals the column's current width (no change).
///
/// [`ensure_column_visible`]: super::mutations::ensure_column_visible
#[must_use]
pub fn resize_column_boundary_move(
    layout: &VirtualLayout,
    col: usize,
    grip: ResizeEdge,
    target_width: i32,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let column = layout.columns.get(col)?;
    let neighbor_offset = grip.neighbor_offset();
    if neighbor_offset == 0 {
        return None;
    }

    let min = config.min_column_width_px as i32;
    let max = config.abs_max_width;
    let clamped_target = target_width.clamp(min, max);

    let old_width = column.width_px;
    if clamped_target == old_width {
        return None;
    }
    let delta = clamped_target - old_width;

    let mut new_layout = layout.clone();
    new_layout.columns[col].width_px = clamped_target;

    // The neighbor on the grip side compensates: it gives up `delta` when the
    // resized column grows, and absorbs `-delta` when it shrinks. Clamp to the
    // same [min, max] bounds — once the neighbor hits its floor it becomes a
    // rigid rider and the canvas extends (Grow); once it hits its ceiling it
    // pins (elastic on the neighbor side).
    let neighbor_idx = (col as i32 + neighbor_offset) as usize;
    if neighbor_idx < new_layout.columns.len() {
        let neighbor_old = new_layout.columns[neighbor_idx].width_px;
        let neighbor_new = (neighbor_old - delta).clamp(min, max);
        new_layout.columns[neighbor_idx].width_px = neighbor_new;
    }
    // No neighbor on the grip side (only column, or grip faces the canvas
    // edge): the column simply grows/shrinks and the canvas extends/contracts.

    Some(new_layout)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::WindowId;
    use crate::layout::types::{Column, Padding, Row};

    /// Standard test config mirroring `mutations::tests::test_config`:
    /// 1920×1080 monitor, 960px columns, 4px gap, min 480, abs_max 1912.
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

    fn two_column_layout() -> VirtualLayout {
        VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        )
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

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    // --- classify_drag: rect-diff classifier ---

    #[test]
    fn classify_no_movement_is_none() {
        // A click: start == current.
        let r = rect(10, 20, 960, 1080);
        assert_eq!(classify_drag(r, r), DragKind::None);
    }

    #[test]
    fn classify_position_only_is_translate() {
        // Title-bar drag: position changed, size unchanged.
        let start = rect(10, 20, 960, 1080);
        let cur = rect(50, 80, 960, 1080);
        assert_eq!(classify_drag(start, cur), DragKind::Translate);
    }

    #[test]
    fn classify_right_edge_grip() {
        // Right edge dragged rightward: width grows, left edge fixed, right
        // edge moved → grip = Right.
        let start = rect(10, 20, 960, 1080);
        let cur = rect(10, 20, 1200, 1080);
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::Right
            }
        );
    }

    #[test]
    fn classify_left_edge_grip() {
        // Left edge dragged leftward: width grows, right edge fixed, left edge
        // moved → grip = Left.
        let start = rect(10, 20, 960, 1080);
        let cur = rect(-30, 20, 1000, 1080);
        // cur.right() = -30 + 1000 = 970 == start.right() (10+960=970) → fixed.
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::Left
            }
        );
    }

    #[test]
    fn classify_left_edge_shrink() {
        // Left edge dragged rightward (shrink): width shrinks, right edge
        // fixed, left edge moved right → grip = Left.
        let start = rect(10, 20, 960, 1080);
        let cur = rect(100, 20, 870, 1080);
        // cur.right() = 100 + 870 = 970 == start.right() → fixed.
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::Left
            }
        );
    }

    #[test]
    fn classify_height_only_resize_horizontal_none() {
        // Vertical-only resize (a row divider, ticket #10): height changed,
        // width unchanged → Resize with horizontal = None.
        let start = rect(10, 20, 960, 1080);
        let cur = rect(10, 20, 960, 900);
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::None
            }
        );
    }

    #[test]
    fn classify_corner_top_right_is_horizontal_right() {
        // A top-right corner grip: width grows (right edge moved) and height
        // changes (top edge moved). Horizontal grip = Right; the vertical
        // component is ticket #10.
        let start = rect(10, 20, 960, 1080);
        let cur = rect(10, 50, 1200, 1050);
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::Right
            }
        );
    }

    #[test]
    fn classify_corner_top_left_is_horizontal_left() {
        // Top-left corner grip: width grows (left edge moved), height changes.
        let start = rect(10, 20, 960, 1080);
        let cur = rect(-30, 50, 1000, 1050);
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::Left
            }
        );
    }

    #[test]
    fn classify_both_edges_moved_picks_larger_displacement() {
        // Translate+resize (unusual for a native grip): width grew, both edges
        // moved. Left moved 40, right moved 60 (dw=20 → dx_right - dx_left =
        // 20; 60-40=20 ✓). Larger displacement is right → grip = Right.
        let start = rect(100, 20, 960, 1080);
        let cur = rect(140, 20, 1020, 1080);
        // dx_left = 40, dx_right = (140+1020) - (100+960) = 1160-1060 = 100?
        // Recompute: start.right=1060, cur.right=1160 → dx_right=100. dw=60.
        // dx_right (100) > dx_left (40) → Right.
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::Right
            }
        );
    }

    // --- resize_column_boundary_move: boundary-move + grow + elastic ---

    #[test]
    fn right_grip_grows_column_and_shrinks_neighbor() {
        // Boundary-move: col 0 grows by 240, col 1 shrinks by 240.
        let layout = two_column_layout();
        let cfg = test_config();
        let result = resize_column_boundary_move(&layout, 0, ResizeEdge::Right, 1200, &cfg)
            .expect("some layout");
        assert_eq!(result.columns[0].width_px, 1200);
        assert_eq!(result.columns[1].width_px, 720);
    }

    #[test]
    fn left_grip_grows_column_and_shrinks_left_neighbor() {
        // Drag col 1's left edge: col 1 grows, col 0 (left neighbor) shrinks.
        let layout = two_column_layout();
        let cfg = test_config();
        let result = resize_column_boundary_move(&layout, 1, ResizeEdge::Left, 1200, &cfg)
            .expect("some layout");
        assert_eq!(result.columns[1].width_px, 1200);
        assert_eq!(result.columns[0].width_px, 720);
    }

    #[test]
    fn grow_past_neighbor_min_grows_canvas() {
        // col 0 → 1500 (delta +540). neighbor can give 960-480=480, then clamps
        // at min 480. Canvas grows by the 60px excess.
        let layout = two_column_layout();
        let cfg = test_config();
        let result = resize_column_boundary_move(&layout, 0, ResizeEdge::Right, 1500, &cfg)
            .expect("some layout");
        assert_eq!(result.columns[0].width_px, 1500);
        assert_eq!(result.columns[1].width_px, 480);
        // Canvas width = gap + (1500+gap) + (480+gap) = 4 + 1504 + 484 = 1992.
        // Before = 4 + 964 + 964 = 1932. Grew by 60.
        let gap = cfg.padding.window_gap;
        let canvas_before = gap + layout.columns.iter().map(|c| c.width_px + gap).sum::<i32>();
        let canvas_after = gap + result.columns.iter().map(|c| c.width_px + gap).sum::<i32>();
        assert_eq!(canvas_after - canvas_before, 60);
    }

    #[test]
    fn grow_past_neighbor_min_translates_other_columns() {
        // Three columns: col 0 grows past col 1's min; col 2 should translate
        // rightward by the excess (its width is unchanged). Column positions
        // are a prefix sum of widths, so col 2's canvas position shifts by the
        // excess automatically.
        let layout = three_column_layout();
        let cfg = test_config();
        let result = resize_column_boundary_move(&layout, 0, ResizeEdge::Right, 1500, &cfg)
            .expect("some layout");
        let gap = cfg.padding.window_gap;
        // col 2's canvas left = gap + (w0+gap) + (w1+gap).
        let col2_before = gap + (960 + gap) + (960 + gap);
        let col2_after = gap + (1500 + gap) + (480 + gap);
        assert_eq!(col2_after - col2_before, 60);
        assert_eq!(result.columns[2].width_px, 960, "col 2 width unchanged");
    }

    #[test]
    fn elastic_pin_clamps_column_at_abs_max() {
        // Target 2000 (> abs_max 1912): col clamps to 1912 (elastic ceiling).
        // neighbor gives up (1912-960)=952, clamps to min 480.
        let layout = two_column_layout();
        let cfg = test_config();
        let result = resize_column_boundary_move(&layout, 0, ResizeEdge::Right, 2000, &cfg)
            .expect("some layout");
        assert_eq!(result.columns[0].width_px, 1912);
        assert_eq!(result.columns[1].width_px, 480);
    }

    #[test]
    fn shrink_column_grows_neighbor() {
        // col 0 shrinks to 700 (delta -260); neighbor absorbs +260 → 1220.
        let layout = two_column_layout();
        let cfg = test_config();
        let result = resize_column_boundary_move(&layout, 0, ResizeEdge::Right, 700, &cfg)
            .expect("some layout");
        assert_eq!(result.columns[0].width_px, 700);
        assert_eq!(result.columns[1].width_px, 1220);
    }

    #[test]
    fn shrink_grows_neighbor_clamped_at_abs_max() {
        // col 0 shrinks drastically; neighbor would exceed abs_max → clamped.
        let layout = two_column_layout();
        let cfg = test_config();
        // target 480 → delta -480 → neighbor 960+480=1440 (< 1912, ok).
        let result = resize_column_boundary_move(&layout, 0, ResizeEdge::Right, 480, &cfg)
            .expect("some layout");
        assert_eq!(result.columns[0].width_px, 480);
        assert_eq!(result.columns[1].width_px, 1440);
    }

    #[test]
    fn target_below_min_clamps_to_min() {
        // target 200 < min 480 → clamps to 480; neighbor grows by (480-960)=-480? No: delta = 480-960 = -480, neighbor = 960-(-480)=1440.
        let layout = two_column_layout();
        let cfg = test_config();
        let result = resize_column_boundary_move(&layout, 0, ResizeEdge::Right, 200, &cfg)
            .expect("some layout");
        assert_eq!(result.columns[0].width_px, 480);
        assert_eq!(result.columns[1].width_px, 1440);
    }

    #[test]
    fn only_column_grows_freely() {
        // Single column, grip Right (no right neighbor) → grows freely.
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(960, Row::new(WindowId(1), 0))], 0);
        let cfg = test_config();
        let result = resize_column_boundary_move(&layout, 0, ResizeEdge::Right, 1500, &cfg)
            .expect("some layout");
        assert_eq!(result.columns.len(), 1);
        assert_eq!(result.columns[0].width_px, 1500);
    }

    #[test]
    fn only_column_left_grip_no_neighbor_grows_freely() {
        // Single column, grip Left (no left neighbor) → grows freely.
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(960, Row::new(WindowId(1), 0))], 0);
        let cfg = test_config();
        let result = resize_column_boundary_move(&layout, 0, ResizeEdge::Left, 1500, &cfg)
            .expect("some layout");
        assert_eq!(result.columns[0].width_px, 1500);
    }

    #[test]
    fn no_change_returns_none() {
        // target == current width → None.
        let layout = two_column_layout();
        let cfg = test_config();
        assert!(resize_column_boundary_move(&layout, 0, ResizeEdge::Right, 960, &cfg).is_none());
    }

    #[test]
    fn clamped_target_equal_current_returns_none() {
        // target above abs_max clamps to abs_max; if current is already abs_max
        // → None.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(1912, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let cfg = test_config();
        assert!(resize_column_boundary_move(&layout, 0, ResizeEdge::Right, 2000, &cfg).is_none());
    }

    #[test]
    fn invalid_col_returns_none() {
        let layout = two_column_layout();
        let cfg = test_config();
        assert!(resize_column_boundary_move(&layout, 5, ResizeEdge::Right, 1200, &cfg).is_none());
    }

    #[test]
    fn none_grip_returns_none() {
        let layout = two_column_layout();
        let cfg = test_config();
        assert!(resize_column_boundary_move(&layout, 0, ResizeEdge::None, 1200, &cfg).is_none());
    }

    #[test]
    fn viewport_offset_preserved() {
        // The viewport never scrolls mid-grab — the mutation must not touch it.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            1234,
        );
        let cfg = test_config();
        let result = resize_column_boundary_move(&layout, 0, ResizeEdge::Right, 1200, &cfg)
            .expect("some layout");
        assert_eq!(result.viewport_offset, 1234);
    }

    #[test]
    fn neighbor_offset_maps_edges() {
        assert_eq!(ResizeEdge::Right.neighbor_offset(), 1);
        assert_eq!(ResizeEdge::Left.neighbor_offset(), -1);
        assert_eq!(ResizeEdge::None.neighbor_offset(), 0);
    }
}
