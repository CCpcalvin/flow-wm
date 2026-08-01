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

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::borders::{BorderState, style_for_state};
use crate::common::{Direction, WindowId};
use crate::config::FlowConfig;
use crate::layout::mutations::ensure_column_visible;
use crate::layout::preview::{DropZone, preview_move, resolve_drop_zone};
use crate::layout::types::AppliedLayout;
use crate::registry::hooks::{clear_dragged_hwnd, set_dragged_hwnd};
use crate::registry::types::{TilingState, WindowState};
use crate::registry::win32 as registry_win32;

use super::borders::float_border_rect;
use super::edge_scroll::{EdgeScrollAction, EdgeScrollScheduler, EdgeScrollTimings};
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
    /// for the empty-workspace degenerate where [`resolve_drop_zone`] returns
    /// `None`).
    pub(super) current_zone: Option<DropZone>,
}

/// Compute the already-clamped effective edge-scroll timings from the shared
/// edge-scroll config.
///
/// Built once per drag (at [`FlowWM::on_drag_start`], and for the
/// orchestrator's initial value in `new.rs`) from
/// `EdgeScrollConfig::effective_repeat_interval_ms` /
/// `EdgeScrollConfig::effective_initial_delay_ms` so the scheduler consumes
/// them with no per-event clamp math. The scheduler itself stays config-agnostic.
pub(super) fn edge_scroll_timings_for(config: &FlowConfig) -> EdgeScrollTimings {
    let edge_scroll_cfg = &config.edge_scroll;
    let repeat_ms = edge_scroll_cfg.effective_repeat_interval_ms(&config.animation);
    let initial_ms = edge_scroll_cfg.effective_initial_delay_ms(repeat_ms);
    EdgeScrollTimings {
        initial_delay: Duration::from_millis(u64::from(initial_ms)),
        repeat_interval: Duration::from_millis(u64::from(repeat_ms)),
    }
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

        // Arm the shared edge-scroll scheduler fresh for this drag (mirrors the
        // old per-drag `EdgeScrollScheduler::new()`), set this drag's clamped
        // timings (read from the shared `[edge_scroll]` config block via
        // [`edge_scroll_timings_for`]), and clear any stale deadline. The
        // scheduler is a single instance on the orchestrator now; see
        // `edge_scroll`.
        self.edge_scroll = EdgeScrollScheduler::new();
        self.edge_scroll_timings = edge_scroll_timings_for(&self.config);
        self.edge_scroll_deadline = None;
        // The entire hover subsystem is suppressed during a drag: cancel any
        // armed focus/edge dwell so neither can fire mid-drag (the drag owns the
        // shared scheduler while `drag_state` is set), and reset the controller
        // so edge-hover-scroll re-arms cleanly when polling resumes after the
        // drag — even if the cursor never leaves the band.
        self.focus_dwell_deadline = None;
        self.edge_dwell_deadline = None;
        self.hover.reset();

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
    /// 4. Act on the resolved zone:
    ///    - `ScrollLeft` / `ScrollRight` → **transition detection only**, NOT a
    ///      per-move scroll. Entering a band fires one immediate scroll and arms
    ///      the auto-repeat scheduler; leaving the band cancels it. The repeat
    ///      itself runs off the main-loop timer ([`Self::maybe_fire_edge_scroll`]),
    ///      so holding the cursor still in the band keeps scrolling. This replaces
    ///      the old per-`LOCATIONCHANGE` scroll that raced the viewport to the far
    ///      end one column per pixel.
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
    /// runs off the auto-repeat scheduler + main-loop timer, not this handler —
    /// `LOCATIONCHANGE` only fires while the dragged window moves, so the repeat
    /// is driven by [`Self::maybe_fire_edge_scroll`] between moves.
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
            self.config.edge_scroll.band_width,
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
        let prev_zone = self.drag_state.as_ref().and_then(|ds| ds.current_zone);
        if let Some(drag) = self.drag_state.as_mut() {
            drag.current_zone = new_zone;
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
                let action = self.edge_scroll.on_leave();
                self.apply_edge_scroll_action(action);
            }
            // Entering a new band fires the immediate defining scroll, then arms
            // the first-gap timer on success (or stays idle at the content edge).
            if let Some(dir) = new_dir {
                // on_enter returns Scroll(dir) — a *request* the scheduler makes,
                // not a scroll it performs. The caller (this method) performs the
                // real scroll and feeds whether it moved back through
                // on_scroll_outcome, per the scheduler's caller protocol. So the
                // returned action is intentionally discarded here.
                let _ = self.edge_scroll.on_enter(dir);
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
    ///
    /// Taking `drag_state` tears down the shared edge-scroll scheduler and clears
    /// its armed deadline, so no leftover scroll continues after release.
    pub(super) fn on_drag_end(&mut self, _hwnd: isize) {
        let Some(drag) = self.drag_state.take() else {
            return;
        };
        // Tear down the shared edge-scroll scheduler (reset to Idle) and clear
        // its armed deadline — no leftover scroll continues after release. The
        // scheduler is a single instance on the orchestrator now, so it is reset
        // here (the old per-drag struct dropped it with the DragState).
        let _ = self.edge_scroll.on_drag_end();
        self.edge_scroll_deadline = None;
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

    /// Apply a scheduler-emitted [`EdgeScrollAction`] to the orchestrator's
    /// armed deadline.
    ///
    /// `Arm(deadline)` stores the next-fire deadline (the main loop waits on
    /// it); `Cancel` clears it. `Scroll` is never returned by `on_scroll_outcome`
    /// — only by `on_enter` / `on_timer_fired`, whose callers perform the scroll
    /// directly and feed the outcome back, so a `Scroll` arriving here is a
    /// defensive no-op (leave the deadline untouched). Shared by the drag feed
    /// and the hover feed (`edge_dwell` EdgeLeave).
    pub(super) fn apply_edge_scroll_action(&mut self, action: EdgeScrollAction) {
        match action {
            EdgeScrollAction::Arm(deadline) => self.edge_scroll_deadline = Some(deadline),
            EdgeScrollAction::Cancel => self.edge_scroll_deadline = None,
            EdgeScrollAction::Scroll(_) => {}
        }
    }

    /// Perform one scroll in `direction`, then feed its outcome to the scheduler
    /// and (re)arm or cancel the repeat timer per the result.
    ///
    /// Shared tail of the entry scroll ([`Self::on_drag_move`]), each
    /// timer-fired repeat ([`Self::maybe_fire_edge_scroll`]), and the hover
    /// edge-dwell expiry (`EdgeEnter` → the hover feed reuses the same
    /// immediate-then-first-gap-then-repeat behavior). Once the scheduler has
    /// requested a scroll via `on_enter` / `on_timer_fired`, the caller performs
    /// the real column scroll, reports whether the viewport moved back through
    /// `on_scroll_outcome`, and applies the emitted `Arm` / `Cancel` to the
    /// deadline.
    pub(super) fn scroll_once_and_rearm(&mut self, direction: Direction) {
        let scrolled = self.perform_edge_scroll(direction);
        let timings = self.edge_scroll_timings;
        let action = self
            .edge_scroll
            .on_scroll_outcome(scrolled, Instant::now(), &timings);
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
    /// stops it). No-op when no timer is armed or the deadline has not yet
    /// arrived. The single scheduler is fed by exactly one site at a time (the
    /// drag's reactive move handler during a drag, the hover edge-dwell
    /// otherwise); they never overlap because `poll_hover` bails while a drag is
    /// active and `on_drag_start` clears any armed hover edge-dwell.
    pub(super) fn maybe_fire_edge_scroll(&mut self) {
        let Some(deadline) = self.edge_scroll_deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        // Deadline due: the scheduler emits a Scroll in its stored direction.
        // A spurious fire from Idle (no action armed) is a no-op.
        let dir = match self.edge_scroll.on_timer_fired() {
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
