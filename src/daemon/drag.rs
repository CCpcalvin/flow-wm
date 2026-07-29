//! Tile-window drag-and-drop lifecycle.
//!
//! When a tiled window is dragged by its title bar, this module manages the
//! continuous non-committing preview and the commit-on-release that places the
//! window. A tile stays a tile for the whole drag — there is no tile↔float
//! conversion, and floating windows never enter [`DragState`] (they stay on
//! the real-time float-sync path in `run.rs`, which routes their
//! `LOCATIONCHANGE` events to `on_float_location_changed` because `drag_state`
//! is never set for them).
//!
//! During the drag the committed `ScrollingSpace` layout is **frozen** for
//! window placement: [`FlowWM::on_drag_move`] only submits non-committing
//! preview animations of the *other* windows (the dragged window is excluded
//! by `submit_animation`'s filter and follows the mouse directly via
//! `border.set_geometry`). [`FlowWM::on_drag_end`] is the sole window-placement
//! commit. Viewport scroll is the one exception — it commits live so
//! edge-scrolling persists and the user sees the canvas move.
//!
//! The three entry points ([`FlowWM::on_drag_start`],
//! [`FlowWM::on_drag_move`], [`FlowWM::on_drag_end`]) are called from the
//! daemon's event loop; the hook callback remains stateless — it only
//! signals via [`set_dragged_hwnd`](crate::registry::hooks::set_dragged_hwnd)
//! / [`clear_dragged_hwnd`](crate::registry::hooks::clear_dragged_hwnd)
//! from the main thread.
//!
//! (`docs/src/dev-guide/tile-drag.md`)

use windows::Win32::Foundation::HWND;

use crate::borders::{BorderState, style_for_state};
use crate::common::WindowId;
use crate::layout::mutations::ensure_column_visible;
use crate::layout::preview::{DropZone, preview_move, resolve_drop_zone};
use crate::layout::types::AppliedLayout;
use crate::registry::hooks::{clear_dragged_hwnd, set_dragged_hwnd};
use crate::registry::types::{TilingState, WindowState};
use crate::registry::win32 as registry_win32;

use super::borders::float_border_rect;
use super::types::FlowWM;

/// State held while the user is dragging a tiled window.
///
/// Entered on `MoveSizeStart` for a `Tiling::Active` window and dropped on
/// `MoveSizeEnd`. Floats never enter this state machine (see the module docs).
///
/// Only placement-essential fields are kept: the dragged window's identity, its
/// HWND (for `GetWindowRect` and the `DRAGGED_HWND` global), and the drop zone
/// currently under the cursor (`None` until the first move, and for the
/// empty-workspace degenerate where [`resolve_drop_zone`] returns `None`).
/// There is no dwell timer. The preview is gated on zone change (see
/// [`should_submit_preview`]); the commit happens once, on release.
pub(super) struct DragState {
    /// The layout-engine ID of the dragged window.
    pub(super) dragged_id: WindowId,
    /// The raw HWND value (for `GetWindowRect`, the `DRAGGED_HWND` global).
    pub(super) dragged_hwnd: isize,
    /// Drop zone currently under the cursor (`None` until the first move, and
    /// for the empty-workspace degenerate where `resolve_drop_zone` returns
    /// `None`).
    pub(super) current_zone: Option<DropZone>,
}

// ---------------------------------------------------------------------------
// Handler methods on FlowWM
//
// Called from `process_hook_events` in `run.rs` on MoveSizeStart/MoveSizeEnd
// and LocationChange events during a tile drag.

impl FlowWM {
    /// Begin a tile-window drag (tiles only).
    ///
    /// Called on `MoveSizeStart` for any tracked window. Enters the drag state
    /// machine only when the window is `Tiling(Active)`; otherwise it returns
    /// early. Crucially, **floating windows never set `drag_state`**, so
    /// `run.rs` keeps routing their `LOCATIONCHANGE` events to the real-time
    /// float-sync path (`on_float_location_changed`) — that is the entire
    /// float-drag behavior, with no wiring change needed here.
    ///
    /// No-op if the window is not found or is not an active tile.
    pub(super) fn on_drag_start(&mut self, hwnd: isize) {
        let hwnd_handle = HWND(hwnd as *mut _);
        let Some(window) = self.registry.get_window(hwnd_handle) else {
            return;
        };
        // Only tiles enter the drag state machine. Floats stay on the normal
        // float-sync path (run.rs routes their LOCATIONCHANGE to
        // on_float_location_changed because drag_state is never set for them).
        if !matches!(
            window.state,
            WindowState::Tiling(TilingState::Active { .. })
        ) {
            return;
        }

        self.drag_state = Some(DragState {
            dragged_id: WindowId(hwnd),
            dragged_hwnd: hwnd,
            current_zone: None,
        });

        set_dragged_hwnd(hwnd);

        // Recolor the border to focused to give visual feedback.
        let focused_style = style_for_state(&self.config.borders, BorderState::Focused);
        if let Some(win) = self.registry.get_window_mut(hwnd_handle)
            && let Some(border) = win.border.as_ref()
        {
            border.set_style(focused_style);
        }

        log::debug!("drag start: hwnd={hwnd}");
    }

    /// Update the drag: border follows the window, the drop zone is resolved,
    /// and the other windows are previewed (non-committing) or the viewport
    /// scrolls (committed live).
    ///
    /// Called on each `LOCATIONCHANGE` for the dragged window while
    /// `self.drag_state` is `Some`. Flow:
    ///
    /// 1. Border follows the window (direct `set_geometry`, not the animator).
    /// 2. Read the cursor position.
    /// 3. Snapshot the committed layout and resolve the pure drop zone via
    ///    [`resolve_drop_zone`].
    /// 4. Act on the resolved zone, gating the preview on zone change:
    ///    - `ScrollLeft` / `ScrollRight` → commit the viewport scroll live and
    ///      animate the (non-dragged) windows to their scrolled slots. Each move
    ///      in the band scrolls one more column, naturally bounded because
    ///      scroll_left/right return None at the content edge. Fires every move
    ///      (not gated) because scroll is cumulative.
    ///    - `Row` / `Column` → submit a **non-committing** preview
    ///      ([`preview_move`] + [`Self::animate_preview`]) so the other windows
    ///      reflow toward the prospective layout. The committed
    ///      `ScrollingSpace` layout is untouched until [`on_drag_end`].
    ///      Gated on zone change via [`should_submit_preview`]: the animator's
    ///      `RetargetFromCurrent` policy samples the interpolated position as
    ///      the new `from` on every submit, so resubmitting an identical target
    ///      while windows are mid-flight resets the batch and the reflow never
    ///      completes under continuous cursor movement. Passing through a scroll
    ///      band (a distinct zone value) naturally invalidates the gate, so the
    ///      next preview after a scroll always fires.
    ///    - `None` → empty workspace; nothing to preview.
    ///
    /// NOTE: continuous *stationary* edge-scroll (cursor held still at the edge)
    /// is not implemented — it would need a repeating timer, since `LOCATIONCHANGE`
    /// only fires while the dragged window moves. Out of scope here.
    pub(super) fn on_drag_move(&mut self, hwnd: isize) {
        let Some(drag) = self.drag_state.as_ref() else {
            return;
        };
        let dragged_id = drag.dragged_id;
        let hwnd_handle = HWND(hwnd as *mut _);

        // 1. Border follows the window (direct set_geometry, not the animator).
        let window_rect = match registry_win32::get_window_rect(hwnd_handle) {
            Ok(r) => r,
            Err(e) => {
                log::debug!("drag move: GetWindowRect failed for {hwnd}: {e}");
                return;
            }
        };
        {
            let Some(window) = self.registry.get_window(hwnd_handle) else {
                return;
            };
            let visible_rect = window.invisible_bounds.window_to_visible(window_rect);
            let border_rect = float_border_rect(
                visible_rect,
                self.config.borders.thickness,
                self.config.borders.overlap,
            );
            if let Some(border) = window.border.as_ref() {
                border.set_geometry(border_rect);
            }
        }

        // 2. Cursor position.
        let (cx, cy) = match registry_win32::get_cursor_pos() {
            Ok(pos) => pos,
            Err(e) => {
                log::debug!("drag move: GetCursorPos failed: {e}");
                return;
            }
        };

        // 3. Snapshot committed layout for zone resolution.
        let (applied, config, monitor) = {
            let space = self.active_scrolling();
            (
                AppliedLayout {
                    virtual_layout: space.virtual_layout().clone(),
                    actual_layout: space.actual_layout().clone(),
                },
                *space.config(),
                *space.monitor(),
            )
        };
        let drag_cfg = &self.config.drag;

        let new_zone = resolve_drop_zone(
            &applied,
            &monitor,
            dragged_id,
            cx,
            cy,
            drag_cfg.edge_scroll_width,
            drag_cfg.col_edge_ratio,
            drag_cfg.col_edge_max_px,
        );

        // 4. Update the tracked zone (used by on_drag_end), then act — gating
        //    the preview on zone change. We capture the previous move's zone
        //    BEFORE overwriting it. The animator's `RetargetFromCurrent` policy
        //    samples each window's current interpolated position as the new
        //    `from` on every submit, so resubmitting an identical (stable)
        //    target while windows are mid-flight resets the batch's progress to
        //    zero and the reflow never completes under continuous cursor
        //    movement. Scroll is exempt: it is cumulative and MUST fire every
        //    move (advancing one column per move, bounded at the content edge).
        //    Because a scroll band is itself a distinct zone value, passing
        //    through it naturally invalidates this gate, so the next preview
        //    after a scroll always fires.
        let prev_zone = self.drag_state.as_ref().and_then(|ds| ds.current_zone);
        if let Some(drag) = self.drag_state.as_mut() {
            drag.current_zone = new_zone;
        }

        match new_zone {
            None => {
                // Degenerate (empty workspace). Nothing to preview.
            }
            Some(DropZone::ScrollLeft) => {
                // Viewport commits LIVE during drag (a real view change the
                // user expects to persist). The dragged window is excluded by
                // submit_animation's filter; the others animate to their
                // scrolled slots. Not gated: scroll is cumulative.
                if let Some(scrolled) = self.active_scrolling_mut().scroll_left() {
                    self.animate_layout(&scrolled);
                }
            }
            Some(DropZone::ScrollRight) => {
                if let Some(scrolled) = self.active_scrolling_mut().scroll_right() {
                    self.animate_layout(&scrolled);
                }
            }
            // NON-COMMITTING preview, gated on zone change (the guard below).
            // Resubmitting an identical target mid-flight resets the animator
            // (see `should_submit_preview`), so this arm only fires when the
            // zone changed. On a no-op move (`preview_move` returns None —
            // window already at its target) target the committed layout so a
            // stale reflow from a prior zone resets.
            Some(zone @ (DropZone::Row { .. } | DropZone::Column { .. }))
                if should_submit_preview(prev_zone, new_zone) =>
            {
                match preview_move(&applied.virtual_layout, dragged_id, zone, &config, &monitor) {
                    Some(preview) => self.animate_preview(&preview),
                    None => self.animate_preview(&applied),
                }
            }
            // Zone unchanged from the previous move — skip the preview so a
            // stable target doesn't reset mid-flight animations. The trailing
            // debug log still runs.
            Some(DropZone::Row { .. } | DropZone::Column { .. }) => {}
        }

        if let Some(zone) = new_zone {
            log::debug!("drag move: hwnd={hwnd} zone={zone:?}");
        }
    }

    /// End the tile drag — the sole window-placement commit point.
    ///
    /// Called on `MoveSizeEnd`. Resolves the final drop from the stored
    /// `current_zone` and commits exactly one layout mutation: a `Row` /
    /// `Column` zone runs [`preview_move`] and commits the result; a scroll
    /// zone or `None` (empty-workspace drop) commits the unchanged layout, so
    /// the dragged window snaps back to its existing tile. Because `drag_state`
    /// is `take()`n before [`animate_layout`], the dragged window is INCLUDED
    /// in the animation and visibly snaps from its mouse-following position
    /// into its tile.
    pub(super) fn on_drag_end(&mut self, _hwnd: isize) {
        let Some(drag) = self.drag_state.take() else {
            return;
        };
        clear_dragged_hwnd();

        // Destroyed-mid-drag guard: the window is already gone from the layout
        // and registry (on_window_destroyed handled it). Nothing to commit.
        if self
            .registry
            .get_window(HWND(drag.dragged_hwnd as *mut _))
            .is_none()
        {
            log::debug!(
                "drag end: window {} destroyed, skipping commit",
                drag.dragged_hwnd
            );
            return;
        }

        // Commit the final zone — the ONLY window-placement commit in the drag.
        let (vl, config, monitor) = {
            let space = self.active_scrolling();
            (
                space.virtual_layout().clone(),
                *space.config(),
                *space.monitor(),
            )
        };
        let target_vl = match drag.current_zone {
            Some(zone @ (DropZone::Row { .. } | DropZone::Column { .. })) => {
                preview_move(&vl, drag.dragged_id, zone, &config, &monitor)
                    .map(|p| p.virtual_layout)
                    .unwrap_or(vl)
            }
            // Scroll zone or None → no placement change; snap back to current tile.
            _ => vl,
        };

        // Ensure the dragged window's column is on-screen, then commit + animate.
        // drag_state is already take()n, so animate_layout INCLUDES the dragged
        // window — it visibly snaps from its mouse position into its tile.
        let target_vl = match target_vl.find_window(drag.dragged_id) {
            Some((col, _)) => ensure_column_visible(&target_vl, col, &config),
            None => target_vl,
        };
        let applied = self.active_scrolling_mut().commit_layout(target_vl);
        self.animate_layout(&applied);

        // Re-resolve border style + position for the (unchanged) window state.
        self.refresh_border_for(drag.dragged_hwnd);

        log::debug!("drag end: hwnd={}", drag.dragged_hwnd);
    }
}

// NOTE: drag lifecycle is Win32 orchestration; coverage via the pure
// resolve_drop_zone tests in layout::preview, the should_submit_preview unit
// tests below, and manual interactive testing.

/// Decide whether a new drop zone warrants a fresh preview animation during a
/// tile drag.
///
/// The preview must be gated on zone change. The animator's default
/// `RetargetFromCurrent` policy does NOT no-op on identical targets while a
/// window is mid-flight: it samples the interpolated position as the new `from`
/// and starts a fresh batch, resetting progress to zero. Resubmitting the same
/// stable target on every `LOCATIONCHANGE` (which fires per pixel of mouse
/// movement) therefore prevents the reflow from ever completing, producing the
/// slow/janky preview symptom.
///
/// Only `Row` / `Column` consult this — scroll commits live every move and
/// `None` previews nothing. Because a scroll band is a distinct zone value,
/// transitioning through it changes the zone, so the next preview after a
/// scroll always fires without a special reset.
#[must_use]
fn should_submit_preview(prev_zone: Option<DropZone>, new_zone: Option<DropZone>) -> bool {
    prev_zone != new_zone
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_zone_is_skipped() {
        // The bug case: cursor holds still inside one zone; resubmitting would
        // reset mid-flight windows and stall the reflow.
        let z = Some(DropZone::Column { col: 2 });
        assert!(!should_submit_preview(z, z));
    }

    #[test]
    fn different_column_zone_submits() {
        assert!(should_submit_preview(
            Some(DropZone::Column { col: 2 }),
            Some(DropZone::Column { col: 3 }),
        ));
    }

    #[test]
    fn row_vs_column_zone_submits() {
        assert!(should_submit_preview(
            Some(DropZone::Column { col: 2 }),
            Some(DropZone::Row { col: 2, row: 0 }),
        ));
    }

    #[test]
    fn first_preview_after_idle_submits() {
        assert!(should_submit_preview(
            None,
            Some(DropZone::Column { col: 1 })
        ));
    }

    #[test]
    fn scroll_band_naturally_invalidates_gate() {
        // Cursor was in a Column zone, passed through ScrollLeft, returned to
        // the same Column zone — the gate must fire (the committed layout
        // scrolled, so the preview target moved).
        let col = Some(DropZone::Column { col: 2 });
        assert!(should_submit_preview(col, Some(DropZone::ScrollLeft)));
        assert!(should_submit_preview(Some(DropZone::ScrollLeft), col));
    }

    #[test]
    fn leaving_to_none_and_back_submits() {
        let col = Some(DropZone::Column { col: 2 });
        assert!(should_submit_preview(col, None));
        assert!(should_submit_preview(None, col));
    }
}
