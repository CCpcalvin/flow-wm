//! Tile drag-resize: the rect-diff classifier and the boundary-move column and
//! row resize mutations.
//!
//! These are the **pure** foundations of drag-resize (tickets #9 + #10). The
//! [`classify_drag`] function decides, from two rects, whether a move-size
//! gesture is a title-bar reorder ([`DragKind::Translate`]) or an edge/corner
//! resize ([`DragKind::Resize`]), and — for a resize — identifies the grip edge
//! on *both* axes (a corner grip carries a non-`None` edge on each).
//! [`resize_column_boundary_move`] applies the boundary-move column resize —
//! the "1b"/tmux model — where one boundary shifts, the growing column absorbs
//! the delta, and the shrinking neighbor gives up the pixels down to its
//! minimum, after which the canvas extends (Grow) and the column elastically
//! pins at the monitor-width maximum. [`resize_row_boundary_move`] is the
//! vertical analog (ticket #10): the boundary moves between two rows, the
//! growing row absorbs the delta, the shrinking neighbor gives up pixels down
//! to `min_row_height_px`, and — because the vertical axis is bounded to the
//! work area — the edge elastically pins at [`derived_max_row_height`] (no
//! canvas to grow into). A corner grip composes the two by applying each
//! boundary-move independently (they touch disjoint fields — column widths vs
//! row heights — so they commute).
//!
//! All three are pure: [`classify_drag`] is pure over two [`Rect`]s, and the
//! `resize_*_boundary_move` functions are pure over the [`VirtualLayout`]. The
//! daemon resize-drag orchestration (move-size-start, classify on first
//! location-change, teleport during the drag, commit + animate on release) is
//! Win32 orchestration covered by manual interactive testing — the same
//! character as the existing translate-drag handler.
//!
//! See the *Tile resize* glossary in `CONTEXT.md` and ADR-0004
//! (`docs/adr/0004-tile-resize-contract.md`) for the binding contract and the
//! rejected alternatives (translate-neighbors/1a, hard-pin, live-snap,
//! animate-during-drag).
//!
//! (`docs/src/dev-guide/layout/mutations.md`)

use crate::common::Rect;
use crate::layout::types::{Column, VirtualLayout};

use super::mutations::MutationConfig;

// ---------------------------------------------------------------------------
// Edge / classification vocabulary
// ---------------------------------------------------------------------------

/// A horizontal resize grip edge, or `None` when no horizontal edge is involved.
///
/// The grip identifies which side of the column the user grabbed; the opposite
/// edge is the **anchor** (it stays fixed while the grip edge follows the
/// cursor). `None` represents "this axis is not being resized" — for example a
/// purely vertical resize (a row divider) classifies with `horizontal = None`.
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

/// A vertical resize grip edge, or `None` when no vertical edge is involved
/// (ticket #10).
///
/// The vertical analog of [`ResizeEdge`]: `Top`/`Bottom` identify which
/// horizontal edge of the row the user grabbed; the opposite edge is the
/// anchor. A row's `Top` grip resizes against the row above (`neighbor_offset
/// = -1`); a `Bottom` grip resizes against the row below (`+1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerticalEdge {
    /// No edge on this axis (the axis is not being resized).
    #[default]
    None,
    /// The top edge is the grip; the bottom edge anchors. Neighbor = row above.
    Top,
    /// The bottom edge is the grip; the top edge anchors. Neighbor = row below.
    Bottom,
}

impl VerticalEdge {
    /// The neighbor row a grip acts on: a bottom grip shrinks the row below
    /// (`+1`), a top grip shrinks the row above (`-1`). `None` for `None`.
    #[must_use]
    pub fn neighbor_offset(self) -> i32 {
        match self {
            VerticalEdge::Bottom => 1,
            VerticalEdge::Top => -1,
            VerticalEdge::None => 0,
        }
    }
}

/// What kind of drag the rect-diff classifier inferred, with the identified
/// grip edges when it is a resize.
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
    /// Size changed — an edge/corner resize. `horizontal`/`vertical` identify
    /// which edge is the grip on each axis (`None` when that axis did not
    /// change). A corner grip carries a non-`None` edge on both axes.
    Resize {
        /// The horizontal grip edge (`None` if the width did not change).
        horizontal: ResizeEdge,
        /// The vertical grip edge (`None` if the height did not change).
        vertical: VerticalEdge,
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
    // edge still moves and one vertical edge moves. When both move
    // (translate+resize), the larger displacement identifies the dominant grip;
    // ties break to the left edge.
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

    // Identify the vertical grip edge symmetrically: which horizontal edge of
    // the window moved. A top grip moves `y` while the bottom edge stays fixed;
    // a bottom grip moves the bottom edge while `y` stays fixed.
    let dy_top = current.y - start.y;
    let dy_bottom = current.bottom() - start.bottom();
    let vertical = if dh == 0 {
        VerticalEdge::None
    } else if dy_top != 0 && dy_bottom == 0 {
        VerticalEdge::Top
    } else if dy_bottom != 0 && dy_top == 0 {
        VerticalEdge::Bottom
    } else if dy_top.abs() >= dy_bottom.abs() {
        VerticalEdge::Top
    } else {
        VerticalEdge::Bottom
    };

    DragKind::Resize {
        horizontal,
        vertical,
    }
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
// Boundary-move row resize (ticket #10)
// ---------------------------------------------------------------------------

/// The derived maximum height a row can grow to inside its column without
/// overflowing the work area (ticket #10's "derived maximum").
///
/// The vertical axis is bounded to the work area (no vertical canvas), so a row
/// can never grow taller than the column's content budget minus what the other
/// rows minimally occupy and the inter-row + edge gaps:
///
/// ```text
/// derived_max = available_height − (n + 1) * gap − (n − 1) * min_row_height
/// ```
///
/// where `n` is the row count, `available_height = monitor_height − up − down`,
/// `(n + 1) * gap` is the top/bottom edge gaps plus the `n − 1` inter-row gaps,
/// and every *other* row is assumed pinned at its floor `min_row_height`. The
/// result is floored at `min_row_height` so degenerate (over-stuffed) columns
/// still produce a valid clamp range.
///
/// Pure over `(&Column, &MutationConfig)`; unit-tested without Win32.
#[must_use]
pub fn derived_max_row_height(column: &Column, config: &MutationConfig) -> i32 {
    let n = column.rows.len() as i32;
    let gap = config.padding.window_gap;
    let min = config.min_row_height_px as i32;
    let total_content = config.available_height() - (n + 1) * gap;
    (total_content - (n - 1) * min).max(min)
}

/// Boundary-move row resize (the vertical analog of
/// [`resize_column_boundary_move`], ticket #10).
///
/// Drag row `row` of column `col`'s `grip` edge so the row targets
/// `target_height` pixels. The growing row absorbs the delta; the shrinking
/// vertical neighbor (the row on the grip side) gives up the same number of
/// pixels, down to [`MutationConfig::min_row_height_px`]. Because the vertical
/// axis is bounded to the work area (there is no vertical canvas to grow into),
/// once the neighbor hits its floor the edge **elastically pins**: the dragged
/// row is clamped to the [`derived_max_row_height`] ceiling, the caller
/// overshoots during the drag (Win32 owns the dragged window's geometry, so its
/// height tracks the cursor past the ceiling), and snaps back on release.
///
/// # Anchor edge
///
/// Unlike the horizontal axis (where Grow extends the left-anchored canvas and
/// the `Left` grip's anchor drifts), the vertical axis never grows the column's
/// total content — boundary-move preserves it (one row's gain is the other's
/// loss), so the anchor holds exactly for **both** grips. Rows are top-anchored
/// (positions are a prefix sum of heights from the top): a `Bottom` grip keeps
/// the row's top fixed; a `Top` grip keeps the row's bottom fixed (its top
/// follows the boundary, its height shrinks by the same delta). No anchor
/// asymmetry on the vertical axis.
///
/// # Neighbors
///
/// - `grip = Bottom` → the row below (`row + 1`) shrinks/absorbs.
/// - `grip = Top` → the row above (`row − 1`) shrinks/absorbs.
/// - Resizing the only row in a column (no neighbor) is **pinned**: the row is
///   clamped to the derived ceiling, which for a single row equals its own full
///   content height, so the resize is a no-op (nothing to steal from, nothing
///   to grow into).
///
/// # Returns
///
/// `Some(new_layout)` with the resized row clamped to `[min_row_height_px,
/// derived_max]` and the neighbor clamped to the same bounds, or `None` when:
/// - `col`/`row` is out of bounds,
/// - `grip` is `None`,
/// - the clamped target equals the row's current height (no change).
#[must_use]
pub fn resize_row_boundary_move(
    layout: &VirtualLayout,
    col: usize,
    row: usize,
    grip: VerticalEdge,
    target_height: i32,
    config: &MutationConfig,
) -> Option<VirtualLayout> {
    let column = layout.columns.get(col)?;
    let neighbor_offset = grip.neighbor_offset();
    if neighbor_offset == 0 {
        return None;
    }
    if row >= column.rows.len() {
        return None;
    }

    // Boundary-move resizes a boundary BETWEEN two rows. The grip must face a
    // neighbor (the row above for a Top grip, below for a Bottom grip); the
    // outer column edges (top of the first row, bottom of the last) and the
    // only row in a column have nothing to steal from, and the vertical axis
    // is bounded (no canvas to grow into), so they are safely pinned — a no-op.
    let neighbor_idx = row as i32 + neighbor_offset;
    if neighbor_idx < 0 || (neighbor_idx as usize) >= column.rows.len() {
        return None;
    }
    let neighbor_idx = neighbor_idx as usize;

    let min = config.min_row_height_px as i32;
    let max = derived_max_row_height(column, config);
    let clamped_target = target_height.clamp(min, max);

    let old_height = column.rows[row].height;
    if clamped_target == old_height {
        return None;
    }
    let delta = clamped_target - old_height;

    let mut new_layout = layout.clone();
    new_layout.columns[col].rows[row].height = clamped_target;

    // The vertical neighbor compensates: it gives up `delta` when the resized
    // row grows, absorbs `-delta` when it shrinks. Clamp to the same bounds —
    // once the neighbor hits its floor the dragged row is already at its
    // derived ceiling (elastic pin), and once it hits its own ceiling it pins.
    let neighbor_old = new_layout.columns[col].rows[neighbor_idx].height;
    let neighbor_new = (neighbor_old - delta).clamp(min, max);
    new_layout.columns[col].rows[neighbor_idx].height = neighbor_new;

    Some(new_layout)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::WindowId;
    use crate::layout::mutations::distribute_heights;
    use crate::layout::types::{Column, Padding, Row};

    /// Standard test config mirroring `mutations::tests::test_config`:
    /// 1920×1080 monitor, 960px columns, 4px gap, min 480, abs_max 1912.
    fn test_config() -> MutationConfig {
        MutationConfig {
            monitor_width: 1920,
            monitor_height: 1080,
            min_window_height_px: 100,
            min_row_height_px: 100,
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
        // edge moved → grip = Right. Height unchanged → vertical = None.
        let start = rect(10, 20, 960, 1080);
        let cur = rect(10, 20, 1200, 1080);
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::Right,
                vertical: VerticalEdge::None,
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
                horizontal: ResizeEdge::Left,
                vertical: VerticalEdge::None,
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
                horizontal: ResizeEdge::Left,
                vertical: VerticalEdge::None,
            }
        );
    }

    #[test]
    fn classify_height_only_resize_is_vertical_bottom() {
        // Vertical-only resize via the bottom edge (a row's bottom divider):
        // height shrinks, top edge fixed (y unchanged), bottom edge moved →
        // grip = Bottom. Width unchanged → horizontal = None.
        let start = rect(10, 20, 960, 1080);
        let cur = rect(10, 20, 960, 900);
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::None,
                vertical: VerticalEdge::Bottom,
            }
        );
    }

    #[test]
    fn classify_top_edge_grip_is_vertical_top() {
        // Vertical-only resize via the top edge: height shrinks, top edge moved
        // down (y grew), bottom edge fixed → grip = Top.
        let start = rect(10, 20, 960, 1080);
        let cur = rect(10, 200, 960, 900);
        // cur.bottom() = 200 + 900 = 1100 == start.bottom() (20+1080) → fixed.
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::None,
                vertical: VerticalEdge::Top,
            }
        );
    }

    #[test]
    fn classify_corner_top_right() {
        // A top-right corner grip: width grows (right edge moved) and height
        // changes (top edge moved, bottom fixed). Full corner classification —
        // horizontal = Right, vertical = Top.
        let start = rect(10, 20, 960, 1080);
        let cur = rect(10, 50, 1200, 1050);
        // cur.bottom() = 50 + 1050 = 1100 == start.bottom() (1100) → bottom fixed.
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::Right,
                vertical: VerticalEdge::Top,
            }
        );
    }

    #[test]
    fn classify_corner_top_left() {
        // Top-left corner grip: width grows (left edge moved), height changes
        // (top edge moved). horizontal = Left, vertical = Top.
        let start = rect(10, 20, 960, 1080);
        let cur = rect(-30, 50, 1000, 1050);
        // cur.right() = -30 + 1000 = 970 == start.right() → right fixed.
        // cur.bottom() = 50 + 1050 = 1100 == start.bottom() → bottom fixed.
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::Left,
                vertical: VerticalEdge::Top,
            }
        );
    }

    #[test]
    fn classify_corner_bottom_right() {
        // Bottom-right corner grip: width grows (right moved), height grows
        // (bottom moved, top fixed). horizontal = Right, vertical = Bottom.
        let start = rect(10, 20, 960, 1080);
        let cur = rect(10, 20, 1200, 1200);
        // top (y) fixed → vertical = Bottom; left fixed → horizontal = Right.
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::Right,
                vertical: VerticalEdge::Bottom,
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
        // dx_right (100) > dx_left (40) → Right. Height unchanged → vertical None.
        assert_eq!(
            classify_drag(start, cur),
            DragKind::Resize {
                horizontal: ResizeEdge::Right,
                vertical: VerticalEdge::None,
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

    #[test]
    fn vertical_neighbor_offset_maps_edges() {
        assert_eq!(VerticalEdge::Bottom.neighbor_offset(), 1);
        assert_eq!(VerticalEdge::Top.neighbor_offset(), -1);
        assert_eq!(VerticalEdge::None.neighbor_offset(), 0);
    }

    // --- derived_max_row_height ---

    /// Two-row column: each row is the equal share. With available=1080, gap=4,
    /// n=2: content = 1080 − 3*4 = 1068, each row 534.
    fn two_row_column() -> Column {
        let h = distribute_heights(2, 1080, 4);
        Column::with_rows(
            960,
            vec![Row::new(WindowId(1), h[0]), Row::new(WindowId(2), h[1])],
        )
    }

    /// Three-row column: content = 1080 − 4*4 = 1064, each row 354 (354+354+356).
    fn three_row_column() -> Column {
        let h = distribute_heights(3, 1080, 4);
        Column::with_rows(
            960,
            vec![
                Row::new(WindowId(1), h[0]),
                Row::new(WindowId(2), h[1]),
                Row::new(WindowId(3), h[2]),
            ],
        )
    }

    #[test]
    fn derived_max_two_rows() {
        // n=2: content = 1068, minus (2-1)*min(100) = 968.
        let cfg = test_config();
        assert_eq!(derived_max_row_height(&two_row_column(), &cfg), 968);
    }

    #[test]
    fn derived_max_three_rows() {
        // n=3: content = 1064, minus (3-1)*min(100) = 864.
        let cfg = test_config();
        assert_eq!(derived_max_row_height(&three_row_column(), &cfg), 864);
    }

    #[test]
    fn derived_max_single_row_is_full_content() {
        // n=1: content = 1080 − 2*4 = 1072, minus 0 = 1072 (fills the column).
        let col = Column::with_row(960, Row::new(WindowId(1), 1072));
        let cfg = test_config();
        assert_eq!(derived_max_row_height(&col, &cfg), 1072);
    }

    #[test]
    fn derived_max_floors_at_min_for_overstuffed_column() {
        // Over-stuffed: many rows, content budget goes negative. The result
        // must floor at min_row_height so the clamp range stays valid.
        let cfg = test_config();
        let rows: Vec<Row> = (0..40).map(|i| Row::new(WindowId(i), 100)).collect();
        let col = Column::with_rows(960, rows);
        assert_eq!(derived_max_row_height(&col, &cfg), 100);
    }

    // --- resize_row_boundary_move: boundary-move + elastic pin ---

    fn two_row_layout() -> VirtualLayout {
        VirtualLayout::with_columns(vec![two_row_column()], 0)
    }

    fn three_row_layout() -> VirtualLayout {
        VirtualLayout::with_columns(vec![three_row_column()], 0)
    }

    #[test]
    fn bottom_grip_grows_row_and_shrinks_below_neighbor() {
        // Row 0 grows via its bottom edge; row 1 (below) shrinks by the delta.
        // 534 → 700 (delta +166); row 1: 534 − 166 = 368.
        let layout = two_row_layout();
        let cfg = test_config();
        let result = resize_row_boundary_move(&layout, 0, 0, VerticalEdge::Bottom, 700, &cfg)
            .expect("some layout");
        assert_eq!(result.columns[0].rows[0].height, 700);
        assert_eq!(result.columns[0].rows[1].height, 368);
        // Column content total is preserved (boundary-move, no canvas growth).
        let before: i32 = layout.columns[0].rows.iter().map(|r| r.height).sum();
        let after: i32 = result.columns[0].rows.iter().map(|r| r.height).sum();
        assert_eq!(after, before);
    }

    #[test]
    fn top_grip_grows_row_and_shrinks_above_neighbor() {
        // Row 1 grows via its top edge; row 0 (above) shrinks by the delta.
        let layout = two_row_layout();
        let cfg = test_config();
        let result = resize_row_boundary_move(&layout, 0, 1, VerticalEdge::Top, 700, &cfg)
            .expect("some layout");
        assert_eq!(result.columns[0].rows[1].height, 700);
        assert_eq!(result.columns[0].rows[0].height, 368);
    }

    #[test]
    fn shrink_row_grows_neighbor() {
        // Row 0 shrinks to 300 (delta -234); row 1 absorbs +234 → 768.
        let layout = two_row_layout();
        let cfg = test_config();
        let result = resize_row_boundary_move(&layout, 0, 0, VerticalEdge::Bottom, 300, &cfg)
            .expect("some layout");
        assert_eq!(result.columns[0].rows[0].height, 300);
        assert_eq!(result.columns[0].rows[1].height, 768);
    }

    #[test]
    fn elastic_pin_clamps_row_at_derived_max() {
        // Target 1500 (> derived_max 968): row clamps to 968 (elastic ceiling),
        // neighbor hits the floor 100.
        let layout = two_row_layout();
        let cfg = test_config();
        let result = resize_row_boundary_move(&layout, 0, 0, VerticalEdge::Bottom, 1500, &cfg)
            .expect("some layout");
        assert_eq!(result.columns[0].rows[0].height, 968);
        assert_eq!(result.columns[0].rows[1].height, 100);
    }

    #[test]
    fn elastic_pin_snap_back_is_no_op_at_ceiling() {
        // When the row is already at derived_max, an oversize target clamps to
        // it → no change → None (the snap-back target on release).
        let col = Column::with_rows(
            960,
            vec![Row::new(WindowId(1), 968), Row::new(WindowId(2), 100)],
        );
        let layout = VirtualLayout::with_columns(vec![col], 0);
        let cfg = test_config();
        assert!(
            resize_row_boundary_move(&layout, 0, 0, VerticalEdge::Bottom, 1500, &cfg).is_none()
        );
    }

    #[test]
    fn row_target_below_min_clamps_to_min() {
        // Target 50 (< min 100) → clamps to 100; neighbor absorbs the rest.
        // delta = 100 − 534 = -434; neighbor = 534 − (-434) = 968 (clamped).
        let layout = two_row_layout();
        let cfg = test_config();
        let result = resize_row_boundary_move(&layout, 0, 0, VerticalEdge::Bottom, 50, &cfg)
            .expect("some layout");
        assert_eq!(result.columns[0].rows[0].height, 100);
        assert_eq!(result.columns[0].rows[1].height, 968);
    }

    #[test]
    fn only_row_in_column_is_pinned() {
        // A single-row column has no neighbor to steal from → pinned (None).
        let layout = VirtualLayout::with_columns(
            vec![Column::with_row(960, Row::new(WindowId(1), 1072))],
            0,
        );
        let cfg = test_config();
        assert!(resize_row_boundary_move(&layout, 0, 0, VerticalEdge::Bottom, 800, &cfg).is_none());
        assert!(resize_row_boundary_move(&layout, 0, 0, VerticalEdge::Top, 800, &cfg).is_none());
    }

    #[test]
    fn grip_facing_column_edge_is_pinned() {
        // Top grip on the first row faces the column's top edge (no neighbor
        // above); Bottom grip on the last row faces the bottom edge. Both pin.
        let layout = two_row_layout();
        let cfg = test_config();
        assert!(resize_row_boundary_move(&layout, 0, 0, VerticalEdge::Top, 700, &cfg).is_none());
        assert!(resize_row_boundary_move(&layout, 0, 1, VerticalEdge::Bottom, 700, &cfg).is_none());
    }

    #[test]
    fn three_row_interior_boundary_only_moves_one_neighbor() {
        // Three rows: growing row 0 via Bottom steals only from row 1; row 2
        // is untouched.
        let layout = three_row_layout();
        let cfg = test_config();
        let h_before = layout.columns[0].rows[2].height;
        let result = resize_row_boundary_move(&layout, 0, 0, VerticalEdge::Bottom, 600, &cfg)
            .expect("some layout");
        assert!(result.columns[0].rows[0].height > 354);
        assert!(result.columns[0].rows[1].height < 354);
        assert_eq!(
            result.columns[0].rows[2].height, h_before,
            "row 2 untouched"
        );
    }

    #[test]
    fn row_resize_preserves_viewport_offset() {
        let layout = VirtualLayout::with_columns(vec![two_row_column()], 999);
        let cfg = test_config();
        let result = resize_row_boundary_move(&layout, 0, 0, VerticalEdge::Bottom, 700, &cfg)
            .expect("some layout");
        assert_eq!(result.viewport_offset, 999);
    }

    #[test]
    fn row_resize_invalid_indices_return_none() {
        let layout = two_row_layout();
        let cfg = test_config();
        assert!(resize_row_boundary_move(&layout, 5, 0, VerticalEdge::Bottom, 700, &cfg).is_none());
        assert!(resize_row_boundary_move(&layout, 0, 9, VerticalEdge::Bottom, 700, &cfg).is_none());
        assert!(resize_row_boundary_move(&layout, 0, 0, VerticalEdge::None, 700, &cfg).is_none());
    }

    #[test]
    fn row_resize_no_change_returns_none() {
        let layout = two_row_layout();
        let cfg = test_config();
        let cur = layout.columns[0].rows[0].height;
        assert!(resize_row_boundary_move(&layout, 0, 0, VerticalEdge::Bottom, cur, &cfg).is_none());
    }

    // --- corner composition: horizontal + vertical in one gesture ---

    #[test]
    fn corner_compose_applies_both_axes_independently() {
        // A corner grip composes the horizontal column boundary-move and the
        // vertical row boundary-move — one mechanism on two axes. Apply column
        // resize (col 0 grows, col 1 shrinks) then row resize (row 0 of col 0
        // grows, row 1 shrinks) and assert BOTH took effect. The two commute
        // (column resize touches widths + neighbor columns; row resize touches
        // row heights within col 0 — disjoint fields).
        let layout = VirtualLayout::with_columns(
            vec![
                two_row_column(),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            0,
        );
        let cfg = test_config();

        // Horizontal: grow col 0 to 1200; col 1 shrinks to 720.
        let after_h = resize_column_boundary_move(&layout, 0, ResizeEdge::Right, 1200, &cfg)
            .expect("horizontal resize");
        assert_eq!(after_h.columns[0].width_px, 1200);
        assert_eq!(after_h.columns[1].width_px, 720);
        // Row heights unchanged by the horizontal pass.
        assert_eq!(after_h.columns[0].rows[0].height, 534);

        // Vertical: now grow row 0 of col 0 to 700; row 1 shrinks to 368.
        let after_v = resize_row_boundary_move(&after_h, 0, 0, VerticalEdge::Bottom, 700, &cfg)
            .expect("vertical resize");
        // Both axes' effects are present simultaneously.
        assert_eq!(after_v.columns[0].width_px, 1200, "horizontal width kept");
        assert_eq!(after_v.columns[1].width_px, 720, "neighbor width kept");
        assert_eq!(
            after_v.columns[0].rows[0].height, 700,
            "vertical grew row 0"
        );
        assert_eq!(
            after_v.columns[0].rows[1].height, 368,
            "vertical shrank row 1"
        );
    }

    #[test]
    fn corner_compose_order_does_not_matter() {
        // The two boundary-moves commute (disjoint fields), so applying
        // vertical-then-horizontal yields the same layout as horizontal-then-
        // vertical.
        let layout = VirtualLayout::with_columns(
            vec![
                two_row_column(),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            0,
        );
        let cfg = test_config();

        let hv = resize_column_boundary_move(&layout, 0, ResizeEdge::Right, 1200, &cfg)
            .and_then(|l| resize_row_boundary_move(&l, 0, 0, VerticalEdge::Bottom, 700, &cfg))
            .expect("hv");
        let vh = resize_row_boundary_move(&layout, 0, 0, VerticalEdge::Bottom, 700, &cfg)
            .and_then(|l| resize_column_boundary_move(&l, 0, ResizeEdge::Right, 1200, &cfg))
            .expect("vh");
        assert_eq!(hv, vh);
    }
}
