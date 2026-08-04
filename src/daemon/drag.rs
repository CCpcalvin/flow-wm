//! Tile-window drag lifecycle: a 4-state machine
//! `DragMode ::= Idle | Classifying | Translate | Resize`.
//!
//! When a tiled window is grabbed (`MoveSizeStart`), the drag enters the
//! provisional [`DragMode::Classifying`] state. Classification happens on the
//! first `LOCATIONCHANGE` by rect-diff ([`classify_drag`]): width/height
//! changed → [`DragMode::Resize`] (and which horizontal edge moved identifies
//! the grip); position-only → [`DragMode::Translate`] (the existing title-bar
//! reorder); a click with no movement stays `Classifying` and is a no-op on
//! release. `Classifying` also suppresses geometry snap-back from the moment
//! it is entered, because the dragged tile's `LOCATIONCHANGE` is routed here
//! (not to the float-sync path) for the whole drag.
//!
//! `Translate` keeps the existing reorder behavior unchanged: a non-committing
//! preview during the drag ([`FlowWM::on_translate_move`]) and the sole
//! window-placement commit on release ([`FlowWM::on_translate_end`]).
//!
//! `Resize` (tickets #9 + #10: horizontal column, vertical row, and corner-
//! compose edge/corner resize) is a sibling state. During the drag it
//! teleports the *other* windows to their boundary-move targets (direct
//! `SetWindowPos`, bypassing the animator) so the moving boundary stays fused
//! to the cursor; the committed `ScrollingSpace` layout stays **frozen**
//! (non-committing), and the sole commit is on release, which runs the
//! animator once. The viewport never scrolls mid-grab, and the resized column
//! is brought into view on release via [`ensure_column_visible`]. A corner
//! grip composes the horizontal and vertical boundary-moves in one gesture
//! (they touch disjoint fields, so they commute).
//!
//! Floating windows never enter [`DragMode`] (they stay on the real-time
//! float-sync path in `run.rs`, which routes their `LOCATIONCHANGE` events to
//! `on_float_location_changed` because `drag_state` is never set for them).
//!
//! Because resize lives in the same `drag_state` field, the existing IPC
//! `Busy` guard (layout-mutating commands rejected while `drag_state` is
//! `Some`) covers resize automatically.
//!
//! The three entry points ([`FlowWM::on_drag_start`],
//! [`FlowWM::on_drag_move`], [`FlowWM::on_drag_end`]) are called from the
//! daemon's event loop; the hook callback remains stateless — it only
//! signals via [`set_dragged_hwnd`](crate::registry::hooks::set_dragged_hwnd)
//! / [`clear_dragged_hwnd`](crate::registry::hooks::clear_dragged_hwnd)
//! from the main thread.
//!
//! (`docs/src/dev-guide/tile-drag.md`, `docs/adr/0004-tile-resize-contract.md`)

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::borders::{BorderState, style_for_state};
use crate::common::{Direction, Rect, WindowId};
use crate::layout::mutations::ensure_column_visible;
use crate::layout::preview::{DropZone, preview_move, resolve_drop_zone};
use crate::layout::projection;
use crate::layout::resize::{
    DragKind, ResizeEdge, VerticalEdge, classify_drag, resize_column_boundary_move,
    resize_row_boundary_move,
};
use crate::layout::types::AppliedLayout;
use crate::registry::hooks::{clear_dragged_hwnd, set_dragged_hwnd};
use crate::registry::types::{TilingState, WindowState};
use crate::registry::win32 as registry_win32;

use super::borders::float_border_rect;
use super::edge_scroll::{EdgeScrollAction, EdgeScrollScheduler, EdgeScrollTimings};
use super::types::FlowWM;

/// The 4-state drag state machine, or `None` when idle.
///
/// `Idle` is represented as `None` on [`FlowWM::drag_state`], so the existing
/// `is_some()` / `take()` API (and the IPC `Busy` guard that keys off it) is
/// unchanged — resize lives in this same field and is covered automatically.
pub(super) enum DragMode {
    /// Provisional state entered on `MoveSizeStart` for a tile. Suppresses
    /// geometry snap-back and classifies on the first `LOCATIONCHANGE` by
    /// rect-diff into [`Translate`](DragMode::Translate) or
    /// [`Resize`](DragMode::Resize).
    Classifying(ClassifyingDrag),
    /// Title-bar reorder drag — the existing translate behavior. Unchanged by
    /// ticket #9 (it was previously the only drag state).
    Translate(DragState),
    /// Horizontal edge/corner column resize — ticket #9.
    Resize(ResizeDrag),
}

impl DragMode {
    /// The layout-engine ID of the dragged window (valid for every variant).
    #[must_use]
    pub(super) fn dragged_id(&self) -> WindowId {
        match self {
            DragMode::Classifying(c) => c.dragged_id,
            DragMode::Translate(t) => t.dragged_id,
            DragMode::Resize(r) => r.dragged_id,
        }
    }

    /// The raw HWND value of the dragged window (valid for every variant).
    #[must_use]
    pub(super) fn dragged_hwnd(&self) -> isize {
        match self {
            DragMode::Classifying(c) => c.dragged_hwnd,
            DragMode::Translate(t) => t.dragged_hwnd,
            DragMode::Resize(r) => r.dragged_hwnd,
        }
    }

    /// The armed edge-scroll repeat deadline, or `None`. Only
    /// [`Translate`](DragMode::Translate) arms edge-scroll; the other variants
    /// never scroll mid-grab.
    #[must_use]
    pub(super) fn edge_scroll_deadline(&self) -> Option<Instant> {
        match self {
            DragMode::Translate(t) => t.edge_scroll_deadline,
            DragMode::Classifying(_) | DragMode::Resize(_) => None,
        }
    }
}

/// Provisional `Classifying` payload: identity + the start rect.
pub(super) struct ClassifyingDrag {
    /// The layout-engine ID of the dragged window.
    pub(super) dragged_id: WindowId,
    /// The raw HWND value (for `GetWindowRect`, the `DRAGGED_HWND` global).
    pub(super) dragged_hwnd: isize,
    /// The dragged window's screen rect captured at `MoveSizeStart`, the
    /// reference for the rect-diff classifier on the first `LOCATIONCHANGE`.
    pub(super) start_rect: Rect,
}

/// State held while the user is reordering a tiled window by its title bar
/// (the `Translate` variant). Unchanged by ticket #9.
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
    /// for the empty-workspace degenerate where [`resolve_drop_zone`] returns
    /// `None`).
    pub(super) current_zone: Option<DropZone>,
    /// The edge-scroll auto-repeat state machine. Drives the
    /// immediate-on-entry scroll plus the timer-based repeat cadence; see
    /// [`edge_scroll`] and [`EdgeScrollScheduler`].
    pub(super) edge_scroll: EdgeScrollScheduler,
    /// The already-clamped effective timings, computed once at drag start from
    /// [`DragConfig`](crate::config::DragConfig) so the scheduler consumes them
    /// with no per-event clamp math.
    pub(super) edge_scroll_timings: EdgeScrollTimings,
    /// The deadline at which the armed auto-repeat timer fires, or `None` when
    /// no repeat is armed. The main loop folds this into its wait-timeout
    /// `min`-reduce and runs [`FlowWM::maybe_fire_edge_scroll`] when it arrives.
    /// Set/cleared by the [`EdgeScrollAction::Arm`] / [`EdgeScrollAction::Cancel`]
    /// actions the scheduler emits.
    pub(super) edge_scroll_deadline: Option<Instant>,
}

/// State held during an edge/corner resize (the `Resize` variant). Tickets
/// #9 (horizontal) and #10 (vertical + corner compose).
///
/// The grip edges and the resized column/row indices are captured at
/// classification time and frozen for the drag's duration. The committed
/// layout is also frozen (the drag is non-committing); each move teleports the
/// bystander windows to their boundary-move targets and the sole commit is on
/// release. A corner grip carries a non-`None` edge on both axes; the daemon
/// composes them by applying the horizontal and vertical boundary-moves
/// independently (they touch disjoint fields — column widths vs row heights —
/// so they commute).
pub(super) struct ResizeDrag {
    /// The layout-engine ID of the dragged (resized) window.
    pub(super) dragged_id: WindowId,
    /// The raw HWND value (for `GetWindowRect`, the `DRAGGED_HWND` global).
    pub(super) dragged_hwnd: isize,
    /// Which horizontal edge the user grabbed (`None` for a vertical-only
    /// resize). The opposite edge anchors.
    pub(super) grip_h: ResizeEdge,
    /// Which vertical edge the user grabbed (`None` for a horizontal-only
    /// resize). The opposite edge anchors.
    pub(super) grip_v: VerticalEdge,
    /// The index of the resized column, captured at classification time. The
    /// column is found via [`WindowId`] so this index stays valid even though
    /// the committed layout is frozen.
    pub(super) col: usize,
    /// The index of the dragged window's row within `col`, captured at
    /// classification time. Frozen for the drag's duration.
    pub(super) row: usize,
}

// ---------------------------------------------------------------------------
// Handler methods on FlowWM
//
// Called from `process_hook_events` in `run.rs` on MoveSizeStart/MoveSizeEnd
// and LocationChange events during a tile drag.

impl FlowWM {
    /// Begin a tile-window drag (tiles only).
    ///
    /// Called on `MoveSizeStart` for any tracked window. Enters the
    /// [`DragMode::Classifying`] provisional state only when the window is
    /// `Tiling(Active)`; otherwise it returns early. Crucially, **floating
    /// windows never set `drag_state`**, so `run.rs` keeps routing their
    /// `LOCATIONCHANGE` events to the real-time float-sync path
    /// (`on_float_location_changed`) — that is the entire float-drag behavior,
    /// with no wiring change needed here.
    ///
    /// Entering `Classifying` immediately routes the dragged tile's subsequent
    /// `LOCATIONCHANGE` events into the drag path (not the float-sync path),
    /// which is what suppresses geometry snap-back from the moment the grab
    /// starts. The first `LOCATIONCHANGE` promotes the state to `Translate` or
    /// `Resize` via [`classify_drag`] (see [`Self::classify_and_promote`]). A
    /// click with no movement stays `Classifying` and is a no-op on release.
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

        // Capture the start rect for the rect-diff classifier. Falls back to a
        // zero rect on failure (classify_drag will then see the first move as a
        // pure translate/resize from origin — a degenerate but safe behavior;
        // GetWindowRect on a live window does not fail in practice).
        let start_rect = registry_win32::get_window_rect(hwnd_handle).unwrap_or(Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        });

        self.drag_state = Some(DragMode::Classifying(ClassifyingDrag {
            dragged_id: WindowId(hwnd),
            dragged_hwnd: hwnd,
            start_rect,
        }));

        set_dragged_hwnd(hwnd);

        // Recolor the border to focused to give visual feedback.
        let focused_style = style_for_state(&self.config.borders, BorderState::Focused);
        if let Some(win) = self.registry.get_window_mut(hwnd_handle)
            && let Some(border) = win.border.as_ref()
        {
            border.set_style(focused_style);
        }

        log::debug!("drag start: hwnd={hwnd} (classifying)");
    }

    /// Dispatch a `LOCATIONCHANGE` for the dragged window to the active drag
    /// variant.
    ///
    /// Called on each `LOCATIONCHANGE` for the dragged window while
    /// `self.drag_state` is `Some`. The variant decides the routing:
    /// - [`DragMode::Classifying`] → [`Self::classify_and_promote`] (then fall
    ///   through to the just-promoted variant's move handler).
    /// - [`DragMode::Translate`] → [`Self::on_translate_move`] (the existing
    ///   title-bar reorder preview).
    /// - [`DragMode::Resize`] → [`Self::on_resize_move`] (the boundary-move
    ///   teleport preview).
    pub(super) fn on_drag_move(&mut self, hwnd: isize) {
        // First, classify if we are still in the provisional state. Promotion
        // may install a Translate or Resize state; we then dispatch the move to
        // whichever variant is active afterwards.
        if matches!(self.drag_state.as_ref(), Some(DragMode::Classifying(_))) {
            self.classify_and_promote(hwnd);
        }
        match self.drag_state.as_ref() {
            Some(DragMode::Translate(_)) => self.on_translate_move(hwnd),
            Some(DragMode::Resize(_)) => self.on_resize_move(hwnd),
            // Still Classifying (no movement yet), or None — nothing to do.
            _ => {}
        }
    }

    /// Classify the gesture on the first `LOCATIONCHANGE` and promote the
    /// [`DragMode::Classifying`] state into `Translate` or `Resize`.
    ///
    /// Reads the current window rect, compares it to the captured start rect
    /// via [`classify_drag`], and installs the matching variant's state:
    /// - [`DragKind::None`] → no movement yet; stay `Classifying` (a click that
    ///   has not moved). The next `LOCATIONCHANGE` retries.
    /// - [`DragKind::Translate`] → install `Translate(DragState)` with the
    ///   edge-scroll timings computed on demand, so the existing reorder path
    ///   works unchanged.
    /// - [`DragKind::Resize`] with at least one real edge (a `Left`/`Right`
    ///   horizontal edge, a `Top`/`Bottom` vertical edge, or both — a corner)
    ///   → install `Resize(ResizeDrag)` capturing the grip(s) and the resized
    ///   column + row indices. A `Resize` whose edges are both `None` (a
    ///   degenerate size-only change with no identified grip) stays
    ///   `Classifying`: the native geometry is shown mid-drag and the layout
    ///   snaps back on release (a no-op commit).
    fn classify_and_promote(&mut self, hwnd: isize) {
        let classifying = match self.drag_state.take() {
            Some(DragMode::Classifying(c)) => c,
            // Not classifying — restore and return (caller should not have
            // invoked this, but be defensive).
            other => {
                self.drag_state = other;
                return;
            }
        };
        let hwnd_handle = HWND(hwnd as *mut _);
        let current_rect = match registry_win32::get_window_rect(hwnd_handle) {
            Ok(r) => r,
            Err(e) => {
                log::debug!("classify: GetWindowRect failed for {hwnd}: {e}");
                // Put the Classifying state back so the next move retries.
                self.drag_state = Some(DragMode::Classifying(classifying));
                return;
            }
        };
        match classify_drag(classifying.start_rect, current_rect) {
            DragKind::None => {
                // No movement yet — keep waiting for the first real move.
                self.drag_state = Some(DragMode::Classifying(classifying));
            }
            DragKind::Translate => {
                let edge_scroll_timings = self.compute_edge_scroll_timings();
                self.drag_state = Some(DragMode::Translate(DragState {
                    dragged_id: classifying.dragged_id,
                    dragged_hwnd: classifying.dragged_hwnd,
                    current_zone: None,
                    edge_scroll: EdgeScrollScheduler::new(),
                    edge_scroll_timings,
                    edge_scroll_deadline: None,
                }));
                log::debug!("drag classify: hwnd={hwnd} → translate");
            }
            DragKind::Resize {
                horizontal,
                vertical,
            } => {
                // Promote when the user grabbed a real edge on at least one
                // axis (a Left/Right horizontal edge, a Top/Bottom vertical
                // edge, or both — a corner). When neither edge moved (both
                // `None`) the size-change was degenerate; stay Classifying so
                // the release snaps back.
                let has_h = matches!(horizontal, ResizeEdge::Left | ResizeEdge::Right);
                let has_v = matches!(vertical, VerticalEdge::Top | VerticalEdge::Bottom);
                if !has_h && !has_v {
                    self.drag_state = Some(DragMode::Classifying(classifying));
                    return;
                }
                // Resolve the resized column AND row index from the committed
                // layout once; they are frozen for the drag's duration.
                let pos = self
                    .active_scrolling()
                    .virtual_layout()
                    .find_window(classifying.dragged_id);
                match pos {
                    Some((col, row)) => {
                        self.drag_state = Some(DragMode::Resize(ResizeDrag {
                            dragged_id: classifying.dragged_id,
                            dragged_hwnd: classifying.dragged_hwnd,
                            grip_h: horizontal,
                            grip_v: vertical,
                            col,
                            row,
                        }));
                        log::debug!(
                            "drag classify: hwnd={hwnd} → resize grip_h={horizontal:?} grip_v={vertical:?} col={col} row={row}"
                        );
                    }
                    None => {
                        // Window not in the committed layout (stale focus /
                        // destroyed mid-classify). Restore Classifying so the
                        // drag still completes on MoveSizeEnd and snaps back,
                        // rather than leaking the DRAGGED_HWND global.
                        log::debug!(
                            "drag classify: hwnd={hwnd} not in layout; staying Classifying"
                        );
                        self.drag_state = Some(DragMode::Classifying(classifying));
                    }
                }
            }
        }
    }

    /// Compute the clamped effective edge-scroll timings for a translate drag.
    fn compute_edge_scroll_timings(&self) -> EdgeScrollTimings {
        let drag_cfg = &self.config.drag;
        let repeat_ms = drag_cfg.effective_repeat_interval_ms(&self.config.animation);
        let initial_ms = drag_cfg.effective_initial_delay_ms(repeat_ms);
        EdgeScrollTimings {
            initial_delay: Duration::from_millis(u64::from(initial_ms)),
            repeat_interval: Duration::from_millis(u64::from(repeat_ms)),
        }
    }

    /// The `Resize` variant's move handler: teleport bystander windows to their
    /// boundary-move targets.
    ///
    /// Win32 owns the dragged window's geometry during a native move-size, so
    /// its width already tracks the cursor 1:1 (overshoot and all). This reads
    /// the resulting width and applies [`resize_column_boundary_move`] to the
    /// **frozen committed** virtual layout (the drag is non-committing), then
    /// teleports the *other* windows directly (`SetWindowPos`, bypassing the
    /// animator) so the moving boundary stays fused to the cursor. The dragged
    /// window is excluded from the teleport — Win32 is already placing it. The
    /// viewport never scrolls mid-grab.
    fn on_resize_move(&mut self, hwnd: isize) {
        let (dragged_hwnd, grip_h, grip_v, col, row) = match self.drag_state.as_ref() {
            Some(DragMode::Resize(r)) => (r.dragged_hwnd, r.grip_h, r.grip_v, r.col, r.row),
            _ => return,
        };
        let hwnd_handle = HWND(dragged_hwnd as *mut _);

        // The dragged window's native geometry tracks the cursor (overshoot and
        // all). Translate the window rect to a *visible* rect before feeding
        // the layout: column widths / row heights are visible quantities, and
        // feeding the window-rect dimensions would grow them by the per-edge
        // invisible borders on every drag.
        let (visible_rect, border) = match registry_win32::get_window_rect(hwnd_handle) {
            Ok(r) => {
                let window = self.registry.get_window(hwnd_handle);
                let invisible_bounds = window.map(|w| w.invisible_bounds).unwrap_or_default();
                let visible_rect = invisible_bounds.window_to_visible(r);
                // Clone the Border handle out of the borrow before any later
                // `&mut self` (teleport / layout snapshot) so the NLL borrow of
                // the registry closes here. `None` if this Tile has no overlay.
                let border = window.and_then(|w| w.border.clone());
                (visible_rect, border)
            }
            Err(e) => {
                log::debug!("resize move: GetWindowRect failed for {hwnd}: {e}");
                return;
            }
        };

        // Border follows the window (direct set_geometry, not the animator).
        // The resize Tile's Border is repositioned to the Tile's actual on-screen
        // visible rect on every move so the ring stays glued to the window edge
        // — including when the Tile Overshoots past a Clamp. This is the sibling
        // of the translate-drag Border-follow; it deliberately does *not* outset
        // (the resize window already *is* the cursor-tracked edge, so outsetting
        // would float the ring off into a gap). Placed before the "no bystander
        // change" early-return below so the Border still tracks when the resize
        // is fully Clamped (bystanders stationary, only this Tile Overshoots).
        // The active-drag exclusion in the animator/bystager path is unchanged:
        // this set_geometry is the sole mover of the resizing Tile's Border.
        if let Some(border) = border.as_ref() {
            border.set_geometry(visible_rect);
        }

        // Snapshot the frozen committed layout + config + monitor.
        let (vl, config, monitor) = {
            let space = self.active_scrolling();
            (
                space.virtual_layout().clone(),
                *space.config(),
                *space.monitor(),
            )
        };

        // Compose the two axes. A corner grip applies both boundary-moves;
        // they touch disjoint fields (column widths vs row heights) so they
        // commute. Either may independently return None (clamped to no change),
        // so the composition falls back to the current layout at each step.
        let preview_vl = vl.clone();
        let preview_vl = match grip_h {
            ResizeEdge::Left | ResizeEdge::Right => {
                match resize_column_boundary_move(
                    &preview_vl,
                    col,
                    grip_h,
                    visible_rect.width,
                    &config,
                ) {
                    Some(v) => v,
                    None => preview_vl,
                }
            }
            ResizeEdge::None => preview_vl,
        };
        let preview_vl = match grip_v {
            VerticalEdge::Top | VerticalEdge::Bottom => {
                match resize_row_boundary_move(
                    &preview_vl,
                    col,
                    row,
                    grip_v,
                    visible_rect.height,
                    &config,
                ) {
                    Some(v) => v,
                    None => preview_vl,
                }
            }
            VerticalEdge::None => preview_vl,
        };

        // If neither axis produced a change, bystanders are already at their
        // targets — nothing to teleport.
        if preview_vl == vl {
            return;
        }

        // Project with the FROZEN viewport (boundary-move preserves
        // viewport_offset) and teleport bystanders. The dragged window is
        // excluded by the active-drag filter inside teleport_preview.
        let actual = projection::project(&preview_vl, &monitor, &config.padding);
        let applied = AppliedLayout {
            virtual_layout: preview_vl,
            actual_layout: actual,
        };
        self.teleport_preview(&applied);
    }

    /// The `Translate` variant's move handler — the existing title-bar reorder
    /// preview logic, unchanged by ticket #9.
    ///
    /// Border follows the window, the drop zone is resolved, and the other
    /// windows are previewed (non-committing) or the viewport scrolls
    /// (committed live). See the historical `on_drag_move` doc for the full
    /// flow; only the dispatch shell changed — this body is identical to the
    /// pre-resize translate-drag handler.
    fn on_translate_move(&mut self, hwnd: isize) {
        let Some(drag) = self.drag_state.as_ref() else {
            return;
        };
        let dragged_id = drag.dragged_id();
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

        // 4. Update the tracked zone (used by on_drag_end), then act. We capture
        //    the previous move's zone BEFORE overwriting it. Two independent
        //    consumers:
        //    - Edge-scroll transition detection (below) keys off the band change.
        //    - The preview gate keys off the zone change. The animator's
        //      `RetargetFromCurrent` policy samples each window's current
        //      interpolated position as the new `from` on every submit, so
        //      resubmitting an identical (stable) target while windows are
        //      mid-flight resets the batch's progress to zero and the reflow
        //      never completes under continuous cursor movement. Because a scroll
        //      band is a distinct zone value, passing through it changes the zone
        //      and naturally invalidates this gate, so the next preview after a
        //      scroll always fires.
        let prev_zone = match self.drag_state.as_ref() {
            Some(DragMode::Translate(t)) => t.current_zone,
            _ => None,
        };
        if let Some(DragMode::Translate(t)) = self.drag_state.as_mut() {
            t.current_zone = new_zone;
        }

        // Edge-scroll transition detection — replaces the old per-move scroll.
        // Only band *changes* act here: entering fires the one immediate scroll
        // and arms the scheduler; leaving cancels. The repeat itself is
        // timer-driven (maybe_fire_edge_scroll), so a cursor held still in the
        // band keeps scrolling — impossible when scroll rode the move stream.
        // A band flip (Left↔Right) is a leave-then-enter; the new entry arms the
        // opposite direction.
        let prev_dir = scroll_direction(prev_zone);
        let new_dir = scroll_direction(new_zone);
        if new_dir != prev_dir {
            // Leaving any armed band cancels the timer.
            if prev_dir.is_some() {
                let action = match self.drag_state.as_mut() {
                    Some(DragMode::Translate(t)) => t.edge_scroll.on_leave(),
                    _ => EdgeScrollAction::Cancel,
                };
                self.apply_edge_scroll_action(action);
            }
            // Entering a new band fires the immediate defining scroll, then arms
            // the first-gap timer on success (or stays idle at the content edge).
            if let Some(dir) = new_dir {
                // on_enter returns Scroll(dir) — a *request* the scheduler makes,
                // not a scroll it performs. The caller (this method) performs the
                // real scroll and feeds whether it moved back through
                // on_scroll_outcome, per the scheduler's caller protocol. So the
                // returned action is intentionally discarded here. The borrow is
                // scoped so `scroll_once_and_rearm` (which needs `&mut self`) can
                // run afterwards.
                {
                    if let Some(DragMode::Translate(t)) = self.drag_state.as_mut() {
                        let _ = t.edge_scroll.on_enter(dir);
                    }
                }
                self.scroll_once_and_rearm(dir);
            }
        }

        // NON-COMMITTING preview, gated on zone change (the guard below).
        // Resubmitting an identical target mid-flight resets the animator
        // (see `should_submit_preview`), so this arm only fires when the zone
        // changed. On a no-op move (`preview_move` returns None — window already
        // at its target) target the committed layout so a stale reflow from a
        // prior zone resets.
        match new_zone {
            None => {
                // Degenerate (empty workspace). Nothing to preview.
            }
            // Scroll bands are handled above by the transition detector; the
            // repeat cadence runs off the timer, not here.
            Some(DropZone::ScrollLeft | DropZone::ScrollRight) => {}
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

    /// End the tile drag — dispatches to the active variant's sole commit
    /// point.
    ///
    /// Called on `MoveSizeEnd`. Takes `drag_state` (so [`animate_layout`] /
    /// [`teleport_preview`] include the dragged window again) and routes the
    /// payload to the variant's end handler:
    /// - [`DragMode::Translate`] → [`Self::on_translate_end`] (the existing
    ///   reorder commit — the only window-placement commit in that drag).
    /// - [`DragMode::Resize`] → [`Self::on_resize_end`] (commits the final
    ///   boundary-move layout and runs the animator once; the dragged window
    ///   snaps from its native overshoot to its clamped tile).
    /// - [`DragMode::Classifying`] → [`Self::on_classifying_end`] (a click with
    ///   no movement — the gesture never produced a classifiable rect-diff, so
    ///   it is still `Classifying` at release. Re-commits the unchanged layout
    ///   so any stray native motion snaps back; the re-commit is defensive, not
    ///   a real change). A genuine vertical, horizontal, or corner grip is
    ///   promoted out of `Classifying` on the first location-change (see
    ///   [`Self::classify_and_promote`]), so it never reaches this arm.
    ///
    /// Taking `drag_state` drops the translate edge-scroll scheduler and its
    /// armed deadline (translate variant), tearing down the auto-repeat timer —
    /// no leftover scroll continues after release.
    pub(super) fn on_drag_end(&mut self, _hwnd: isize) {
        let Some(mut drag) = self.drag_state.take() else {
            return;
        };
        let dragged_hwnd = drag.dragged_hwnd();
        // Always clear the dragged-HWND global so LOCATIONCHANGE routing returns
        // to the float-sync path, and (for Translate) tear down the scheduler.
        if let DragMode::Translate(t) = &mut drag {
            let _ = t.edge_scroll.on_drag_end();
        }
        clear_dragged_hwnd();

        // Destroyed-mid-drag guard: the window is already gone from the layout
        // and registry (on_window_destroyed handled it). Nothing to commit.
        if self
            .registry
            .get_window(HWND(dragged_hwnd as *mut _))
            .is_none()
        {
            log::debug!("drag end: window {dragged_hwnd} destroyed, skipping commit");
            return;
        }

        match drag {
            DragMode::Translate(t) => self.on_translate_end(t),
            DragMode::Resize(r) => self.on_resize_end(r),
            DragMode::Classifying(c) => self.on_classifying_end(c),
        }
    }

    /// The `Translate` variant's release commit — the existing title-bar
    /// reorder commit logic, unchanged by ticket #9.
    fn on_translate_end(&mut self, drag: DragState) {
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

        log::debug!("drag end (translate): hwnd={}", drag.dragged_hwnd);
    }

    /// The `Resize` variant's release commit — the sole commit of an edge/corner
    /// drag-resize.
    ///
    /// Reads the dragged window's final native geometry (the cursor position at
    /// release), applies the horizontal boundary-move ([`resize_column_boundary_move`])
    /// and the vertical boundary-move ([`resize_row_boundary_move`]) to the
    /// frozen committed layout, brings the resized column into view via
    /// [`ensure_column_visible`], then commits + animates once. `drag_state` is
    /// already `take()`n, so [`animate_layout`] includes the dragged window —
    /// it visibly snaps from its native overshoot rect into its clamped tile
    /// (the elastic-pin snap-back).
    fn on_resize_end(&mut self, drag: ResizeDrag) {
        let (vl, config) = {
            let space = self.active_scrolling();
            (space.virtual_layout().clone(), *space.config())
        };

        // Read the final native geometry (the cursor position at release) and
        // translate it to a *visible* rect — column widths and row heights are
        // visible quantities, so feeding the window-rect dimensions would grow
        // them by the per-edge invisible borders on every drag.
        let hwnd_handle = HWND(drag.dragged_hwnd as *mut _);
        let (fallback_width, fallback_height) = vl
            .columns
            .get(drag.col)
            .and_then(|c| c.rows.get(drag.row))
            .map(|r| (vl.columns[drag.col].width_px, r.height))
            .unwrap_or((config.column_width as i32, config.available_height()));
        let visible_rect = match registry_win32::get_window_rect(hwnd_handle) {
            Ok(r) => {
                let invisible_bounds = self
                    .registry
                    .get_window(hwnd_handle)
                    .map(|w| w.invisible_bounds)
                    .unwrap_or_default();
                invisible_bounds.window_to_visible(r)
            }
            Err(e) => {
                log::debug!(
                    "resize end: GetWindowRect failed for {}: {e}; committing layout as-is",
                    drag.dragged_hwnd
                );
                // Fall back to the column/row's current geometry so the commit
                // is a no-op snap-back rather than a silent drop.
                Rect {
                    x: 0,
                    y: 0,
                    width: fallback_width,
                    height: fallback_height,
                }
            }
        };

        // Compose the two axes (corner grip = both; edge grip = one). Each
        // boundary-move clamps to its axis's [min, max] (the elastic ceiling);
        // if an axis reports no change it falls back to the layout as-is.
        let target_vl = vl.clone();
        let target_vl = match drag.grip_h {
            ResizeEdge::Left | ResizeEdge::Right => resize_column_boundary_move(
                &target_vl,
                drag.col,
                drag.grip_h,
                visible_rect.width,
                &config,
            )
            .unwrap_or(target_vl),
            ResizeEdge::None => target_vl,
        };
        let target_vl = match drag.grip_v {
            VerticalEdge::Top | VerticalEdge::Bottom => resize_row_boundary_move(
                &target_vl,
                drag.col,
                drag.row,
                drag.grip_v,
                visible_rect.height,
                &config,
            )
            .unwrap_or(target_vl),
            VerticalEdge::None => target_vl,
        };

        // Bring the resized column into view on release (the viewport never
        // scrolled mid-grab). drag_state is take()n, so animate_layout includes
        // the dragged window — it snaps from its overshoot to its clamped tile.
        let target_vl = ensure_column_visible(&target_vl, drag.col, &config);
        let applied = self.active_scrolling_mut().commit_layout(target_vl);
        self.animate_layout(&applied);

        self.refresh_border_for(drag.dragged_hwnd);

        log::debug!(
            "drag end (resize): hwnd={} grip_h={:?} grip_v={:?} col={} row={}",
            drag.dragged_hwnd,
            drag.grip_h,
            drag.grip_v,
            drag.col,
            drag.row,
        );
    }

    /// The `Classifying` variant's release — a no-op commit that snaps any
    /// stray native motion back to the unchanged committed layout.
    ///
    /// Reached when the gesture never produced a classifiable movement (a pure
    /// click). The committed layout was never mutated mid-drag, so re-committing
    /// it re-projects and animates the dragged window back to its tile. The
    /// re-commit + animate is a defensive snap-back of an unchanged layout
    /// (story #4 — "nothing changes"); it produces no visible move.
    fn on_classifying_end(&mut self, drag: ClassifyingDrag) {
        let vl = self.active_scrolling().virtual_layout().clone();
        let applied = self.active_scrolling_mut().commit_layout(vl);
        self.animate_layout(&applied);
        self.refresh_border_for(drag.dragged_hwnd);
        log::debug!("drag end (classifying/no-op): hwnd={}", drag.dragged_hwnd);
    }

    /// Perform one column scroll in `direction` and report whether the viewport
    /// moved.
    ///
    /// The viewport commits LIVE during a drag (a real view change the user
    /// expects to persist). `scroll_left` / `scroll_right` return `None` at the
    /// content edge, which the scheduler reads as "stop repeating". The dragged
    /// window is excluded by `animate_layout`'s filter; the others animate to
    /// their scrolled slots. Returns whether a scroll happened (the boolean the
    /// scheduler's outcome feedback consumes).
    fn perform_edge_scroll(&mut self, direction: Direction) -> bool {
        let scrolled = match direction {
            Direction::Left => self.active_scrolling_mut().scroll_left(),
            Direction::Right => self.active_scrolling_mut().scroll_right(),
            // Edge scroll is horizontal only; Up/Down never reach here.
            Direction::Up | Direction::Down => return false,
        };
        match scrolled {
            Some(applied) => {
                self.animate_layout(&applied);
                true
            }
            None => false,
        }
    }

    /// Apply a scheduler-emitted [`EdgeScrollAction`] to the drag's armed
    /// deadline.
    ///
    /// `Arm(deadline)` stores the next-fire deadline (the main loop waits on
    /// it); `Cancel` clears it. `Scroll` is never returned by `on_scroll_outcome`
    /// — only by `on_enter` / `on_timer_fired`, whose callers perform the scroll
    /// directly and feed the outcome back, so a `Scroll` arriving here is a
    /// defensive no-op (leave the deadline untouched).
    fn apply_edge_scroll_action(&mut self, action: EdgeScrollAction) {
        let Some(drag) = self.drag_state.as_mut() else {
            return;
        };
        let DragMode::Translate(t) = drag else {
            // Edge-scroll only arms during a Translate drag.
            return;
        };
        match action {
            EdgeScrollAction::Arm(deadline) => t.edge_scroll_deadline = Some(deadline),
            EdgeScrollAction::Cancel => t.edge_scroll_deadline = None,
            EdgeScrollAction::Scroll(_) => {}
        }
    }

    /// Perform one scroll in `direction`, then feed its outcome to the scheduler
    /// and (re)arm or cancel the repeat timer per the result.
    ///
    /// Shared tail of the entry scroll ([`Self::on_drag_move`]) and each
    /// timer-fired repeat ([`Self::maybe_fire_edge_scroll`]): once the scheduler
    /// has requested a scroll via `on_enter` / `on_timer_fired`, the caller
    /// performs the real column scroll, reports whether the viewport moved back
    /// through `on_scroll_outcome`, and applies the emitted `Arm` / `Cancel` to
    /// the deadline.
    fn scroll_once_and_rearm(&mut self, direction: Direction) {
        let scrolled = self.perform_edge_scroll(direction);
        let (timings, action) = match self.drag_state.as_mut() {
            Some(DragMode::Translate(t)) => {
                let timings = t.edge_scroll_timings;
                let action = t
                    .edge_scroll
                    .on_scroll_outcome(scrolled, Instant::now(), &timings);
                (timings, action)
            }
            // Edge-scroll only arms during a Translate drag; a spurious timer
            // fire in another variant is a no-op.
            _ => return,
        };
        let _ = timings;
        self.apply_edge_scroll_action(action);
    }

    /// Fire one auto-repeat edge scroll if the armed timer's deadline arrived.
    ///
    /// A twin of [`maybe_resume_float_tracking`](super::FlowWM::maybe_resume_float_tracking):
    /// called at the top of the main loop (and inside the IPC inner loop), it
    /// drives the steady repeat cadence while the cursor stays in a band —
    /// including when the cursor is held perfectly still, which the move-driven
    /// design could not do. The timer fires in the scheduler's last-known
    /// direction (no fresh cursor read: the band is screen-edge-based, so
    /// scrolling the viewport does not move it). On each fire it performs one
    /// column scroll and re-arms or cancels per the outcome (the content edge
    /// stops it). No-op when no drag is active, no timer is armed, or the
    /// deadline has not yet arrived.
    pub(super) fn maybe_fire_edge_scroll(&mut self) {
        let deadline = match self.drag_state.as_ref() {
            Some(DragMode::Translate(t)) => t.edge_scroll_deadline,
            // Edge-scroll only arms during a Translate drag.
            _ => return,
        };
        let Some(deadline) = deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        // Deadline due: the scheduler emits a Scroll in its stored direction.
        // A spurious fire from Idle (no action armed) is a no-op.
        let dir = match self.drag_state.as_mut() {
            Some(DragMode::Translate(t)) => t.edge_scroll.on_timer_fired(),
            _ => return,
        };
        let dir = match dir {
            Some(EdgeScrollAction::Scroll(dir)) => dir,
            _ => return,
        };
        self.scroll_once_and_rearm(dir);
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

/// The edge-scroll band direction a drop zone sits in, or `None` off-band.
///
/// Drives the transition detector in [`FlowWM::on_drag_move`]: a change in this
/// value is the only thing that acts on the scroll — entering a band fires the
/// immediate scroll, leaving cancels. `Row` / `Column` / `None` are all "off"
/// (no band), so the detector only distinguishes Left / Right / off.
#[must_use]
fn scroll_direction(zone: Option<DropZone>) -> Option<Direction> {
    match zone {
        Some(DropZone::ScrollLeft) => Some(Direction::Left),
        Some(DropZone::ScrollRight) => Some(Direction::Right),
        _ => None,
    }
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

    #[test]
    fn scroll_direction_maps_bands() {
        assert_eq!(
            scroll_direction(Some(DropZone::ScrollLeft)),
            Some(Direction::Left)
        );
        assert_eq!(
            scroll_direction(Some(DropZone::ScrollRight)),
            Some(Direction::Right)
        );
    }

    #[test]
    fn scroll_direction_off_band_is_none() {
        // Row / Column / None are all off-band (no scroll).
        assert_eq!(scroll_direction(Some(DropZone::Column { col: 2 })), None);
        assert_eq!(
            scroll_direction(Some(DropZone::Row { col: 2, row: 0 })),
            None
        );
        assert_eq!(scroll_direction(None), None);
    }
}
