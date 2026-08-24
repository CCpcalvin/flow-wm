//! Cursor warp on focus change (ticket #34, parent spec #35).
//!
//! One invariant, applied at the daemon's single focus convergence point
//! ([`FlowWM::on_focus_changed`](super::FlowWM::on_focus_changed)): when the
//! focused window changes and the pointer lies **outside** the newly focused
//! window's rect, the pointer teleports instantly (`SetCursorPos`) to that
//! window's center, clamped to the window rect intersected with the monitor
//! work area. When the pointer is already inside, it is left byte-identically
//! untouched — no read-modify-write, no yank.
//!
//! The decision math lives here as a pure, Win32-free function
//! ([`warp_target`]) so the inside/outside/boundary/empty-rect/clamping
//! behavior is unit-testable without a daemon. The Win32 surface is a single
//! thin wrapper, [`set_cursor_pos`](crate::registry::win32::set_cursor_pos),
//! beside the existing `get_cursor_pos`.
//!
//! Floating windows use their own float rect (per-window, not per-column);
//! tiling windows use their projected actual-layout rect. There is no
//! animation and no per-path flag — every focus path (keyboard dispatch,
//! workspace switch, alt-tab/taskbar foreground change) converges on the
//! same `warp_cursor_to` call. The single-invariant design (one warp rule
//! instead of per-path flags) deliberately avoids the Hyprland multi-knob
//! interaction-bug failure mode; the full rationale lives in
//! `docs/adr/0007-cursor-hide-and-warp.md` and the domain terms (*cursor
//! warp*, *cursor hide*, *activity*) are pinned in `CONTEXT.md`.
//!
//! The cursor-**hide** daemon glue also lives here
//! ([`FlowWM::apply_cursor_hide_action`], [`FlowWM::poll_cursor_hide`]): the
//! pure decision machine is [`super::cursor_hide`]; this module performs the
//! impure half — the system-cursor swap/restore Win32 calls — and feeds the
//! machine one activity sample per main-loop poll tick.

use std::time::Instant;

use super::cursor_hide::{ActivitySample, CursorHideAction};
use super::types::FlowWM;
use crate::common::{Rect, WindowId};
use crate::cursor::restore_system_cursors;
use crate::registry::win32;

/// Read the pointer position, defaulting to an off-screen far corner.
///
/// `GetCursorPos` fails only when the thread lacks input-desktop access
/// (extremely rare for the daemon's main thread). An unreadable position is
/// treated as "outside any window" by every consumer — the warp path still
/// lands the pointer on the focused window, and the hide poll counts it as
/// motion (a transient failure un-hides rather than leaving stale hidden
/// state) — a deliberate fail-visible choice over fail-silent.
pub(super) fn pointer_position() -> (i32, i32) {
    win32::get_cursor_pos().unwrap_or((i32::MIN / 2, i32::MIN / 2))
}

impl FlowWM {
    /// Perform the impure half of a [`CursorHideAction`].
    ///
    /// The swap/restore calls mutate session-global cursor state, so the pure
    /// scheduler only *decides* and this method *acts*. Failures are warned
    /// and swallowed — a failed hide just means the cursor stays visible
    /// (cosmetic), and a failed unhide is recovered by the next activity's
    /// retry or by the daemonless `flow cursor restore` escape hatch.
    pub(super) fn apply_cursor_hide_action(&mut self, action: CursorHideAction) {
        match action {
            CursorHideAction::Hide => {
                if let Err(e) = win32::set_system_cursor_transparent() {
                    log::warn!("cursor hide: {e}");
                } else {
                    log::debug!("cursor hide: system cursors blanked");
                }
            }
            CursorHideAction::Unhide => {
                if let Err(e) = restore_system_cursors() {
                    log::warn!("cursor unhide: {e}");
                } else {
                    log::debug!("cursor unhide: system cursors restored");
                }
            }
            CursorHideAction::None => {}
        }
    }

    /// One activity-poll tick for the cursor-hide machine.
    ///
    /// Reads the pointer position and button state (the same thin wrappers
    /// the warp path uses — no new threads, no hooks, no shared state) and
    /// feeds the pure scheduler with an injected clock, then applies whatever
    /// action it emits. A no-op microsecond-scale guard clause when hide is
    /// disabled; the main loop must not even schedule the poll then (see
    /// [`FlowWM::cursor_hide_poll_deadline`]).
    pub(super) fn poll_cursor_hide(&mut self, now: Instant) {
        if !self.cursor_hide.is_active() {
            return;
        }
        let sample = ActivitySample {
            // A failed read coerces to the far corner via `pointer_position`'s
            // fail-visible rule — an unreadable position counts as motion, so
            // a transient desktop-access failure un-hides the cursor rather
            // than leaving a possibly-stale hidden state. The next successful
            // poll re-establishes the baseline.
            position: pointer_position(),
            button_down: win32::any_mouse_button_down(),
        };
        let action = self.cursor_hide.on_poll(sample, now);
        self.apply_cursor_hide_action(action);
    }

    /// The deadline the main loop must fold into its wait timeout for cursor
    /// hide, if any.
    ///
    /// `None` when the machine is inactive — the loop then schedules no
    /// polling at all, keeping the zero-CPU-while-idle property for
    /// `hide_timeout_ms = 0` configs.
    pub(super) fn cursor_hide_poll_deadline(&self) -> Option<Instant> {
        self.cursor_hide.next_deadline()
    }
}

/// Pure warp decision: where (if anywhere) the pointer should teleport.
///
/// Given the pointer's current screen position, the focused window's rect,
/// and the monitor work area, returns:
///
/// - `None` when the pointer already lies inside `window_rect` (inclusive of
///   the boundary edges) — the caller must not touch the pointer at all, so
///   an in-window position survives byte-identically.
/// - `None` when `window_rect` is empty or degenerate (zero/negative extent
///   after clamping) — there is no sane warp target, so the pointer is left
///   alone rather than being flung to a nonsense coordinate.
/// - `Some((x, y))` with the window-center clamped into the intersection of
///   the window rect and the monitor work area.
///
/// Pure math — no Win32, no daemon state. Unit tests cover inside/outside/
/// boundary, the empty-rect bail, and clamping on each axis.
#[must_use]
pub(crate) fn warp_target(
    pointer: (i32, i32),
    window_rect: Rect,
    work_area: Rect,
) -> Option<(i32, i32)> {
    // Pointer already inside (boundary-inclusive) → leave it byte-identical.
    if contains_inclusive(window_rect, pointer) {
        return None;
    }

    // Clamp the window rect into the work area: the warp target must land on
    // a visible pixel of the window, never under the taskbar or off-monitor.
    let clamped_left = window_rect.x.max(work_area.x);
    let clamped_top = window_rect.y.max(work_area.y);
    let clamped_right = window_rect.right().min(work_area.right());
    let clamped_bottom = window_rect.bottom().min(work_area.bottom());

    // Degenerate intersection (empty rect, or a rect that vanished under
    // clamping) → no safe target; do not move the pointer.
    if clamped_right <= clamped_left || clamped_bottom <= clamped_top {
        return None;
    }

    let target_x = (clamped_left + clamped_right) / 2;
    let target_y = (clamped_top + clamped_bottom) / 2;
    Some((target_x, target_y))
}

/// Point-in-rect test, inclusive on all four boundary edges.
///
/// Inclusive bounds are the least-surprise choice: a pointer parked exactly
/// on the window's edge pixel is already "on" the window, and warping it to
/// the center would be a visible yank for zero benefit.
fn contains_inclusive(r: Rect, p: (i32, i32)) -> bool {
    p.0 >= r.x && p.0 <= r.right() && p.1 >= r.y && p.1 <= r.bottom()
}

/// Resolve the on-screen rect of the given window on the active workspace.
///
/// Floating windows report their own float rect (`FloatingSpace` keeps
/// literal pixel coordinates); tiling windows report their entry in the
/// scrolling space's committed [`ActualLayout`](crate::layout::types::ActualLayout).
/// Returns `None` for windows present in neither space (e.g. a window whose
/// classification is mid-transition).
#[must_use]
pub(crate) fn focused_window_rect(
    scrolling: &crate::workspace::ScrollingSpace,
    floating: &crate::workspace::FloatingSpace,
    window: WindowId,
) -> Option<Rect> {
    floating
        .windows()
        .iter()
        .find(|e| e.window_id == window)
        .map(|e| e.rect)
        .or_else(|| scrolling.actual_layout().find(window).map(|e| e.rect))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Rect = Rect {
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    };
    const WORK_AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1040,
    };

    // ── skip-if-inside ─────────────────────────────────────────────────

    #[test]
    fn pointer_inside_returns_none() {
        assert_eq!(warp_target((300, 250), WINDOW, WORK_AREA), None);
    }

    #[test]
    fn pointer_on_boundary_counts_as_inside() {
        // All four edges, inclusive.
        assert_eq!(warp_target((100, 100), WINDOW, WORK_AREA), None);
        assert_eq!(warp_target((500, 400), WINDOW, WORK_AREA), None);
        assert_eq!(warp_target((100, 250), WINDOW, WORK_AREA), None);
        assert_eq!(warp_target((300, 400), WINDOW, WORK_AREA), None);
    }

    #[test]
    fn pointer_one_px_outside_returns_some() {
        assert_eq!(warp_target((99, 250), WINDOW, WORK_AREA), Some((300, 250)));
        assert_eq!(warp_target((501, 250), WINDOW, WORK_AREA), Some((300, 250)));
    }

    // ── center computation ─────────────────────────────────────────────

    #[test]
    fn outside_pointer_warps_to_window_center() {
        assert_eq!(warp_target((0, 0), WINDOW, WORK_AREA), Some((300, 250)));
    }

    #[test]
    fn odd_extent_centers_round_toward_topleft() {
        // 401×301 window centered at (100 + 401/2, 100 + 301/2) = (300, 250)
        // via integer division (401/2 = 200).
        let odd = Rect {
            x: 100,
            y: 100,
            width: 401,
            height: 301,
        };
        assert_eq!(warp_target((0, 0), odd, WORK_AREA), Some((300, 250)));
    }

    // ── work-area clamping ─────────────────────────────────────────────

    #[test]
    fn window_overhanging_work_area_bottom_clamps() {
        // Window extends past the taskbar (bottom 1160 > 1040): the warp
        // target must stay inside the visible work area.
        let overhang = Rect {
            x: 100,
            y: 900,
            width: 400,
            height: 260, // bottom = 1160
        };
        // Clamped band: y ∈ [900, 1040] → center y = 970.
        assert_eq!(warp_target((0, 0), overhang, WORK_AREA), Some((300, 970)));
    }

    #[test]
    fn window_overhanging_work_area_sides_clamps() {
        // Window partially off the right edge (right = 2020 > 1920).
        let overhang = Rect {
            x: 1700,
            y: 100,
            width: 320,
            height: 300,
        };
        // Clamped band: x ∈ [1700, 1920] → center x = 1810.
        assert_eq!(warp_target((0, 0), overhang, WORK_AREA), Some((1810, 250)));
    }

    #[test]
    fn window_fully_outside_work_area_returns_none() {
        let offscreen = Rect {
            x: 2000,
            y: 100,
            width: 400,
            height: 300,
        };
        assert_eq!(warp_target((0, 0), offscreen, WORK_AREA), None);
    }

    // ── degenerate rects ───────────────────────────────────────────────

    #[test]
    fn zero_extent_window_returns_none() {
        let zero = Rect {
            x: 100,
            y: 100,
            width: 0,
            height: 0,
        };
        // A pointer can never be "inside" a zero rect, but the clamped rect
        // is degenerate → bail without moving the pointer.
        assert_eq!(warp_target((0, 0), zero, WORK_AREA), None);
    }

    #[test]
    fn zero_width_window_returns_none() {
        let zero_w = Rect {
            x: 100,
            y: 100,
            width: 0,
            height: 300,
        };
        assert_eq!(warp_target((0, 0), zero_w, WORK_AREA), None);
    }

    #[test]
    fn negative_extent_window_returns_none() {
        let negative = Rect {
            x: 100,
            y: 100,
            width: -50,
            height: 300,
        };
        assert_eq!(warp_target((0, 0), negative, WORK_AREA), None);
    }

    // ── focused_window_rect precedence ─────────────────────────────────

    #[test]
    fn focused_window_rect_prefers_float_rect() {
        use crate::layout::types::{MonitorInfo, Padding};
        use crate::workspace::{FloatingSpace, ScrollingSpace};

        let id = WindowId(7);
        let mut scrolling = ScrollingSpace::new(
            MonitorInfo {
                work_area: WORK_AREA,
            },
            400,
            640,
            100,
            100,
            Padding {
                window_gap: 16,
                up: 0,
                down: 0,
            },
            4,
        );
        scrolling.add_window(id);
        let mut floating = FloatingSpace::new();
        let float_rect = Rect {
            x: 10,
            y: 10,
            width: 200,
            height: 200,
        };
        floating.add(id, float_rect);

        // The float rect wins even though the window also exists in the
        // scrolling actual layout.
        assert_eq!(
            focused_window_rect(&scrolling, &floating, id),
            Some(float_rect)
        );
    }

    #[test]
    fn focused_window_rect_falls_back_to_scrolling() {
        use crate::layout::types::{MonitorInfo, Padding};
        use crate::workspace::{FloatingSpace, ScrollingSpace};

        let id = WindowId(7);
        let mut scrolling = ScrollingSpace::new(
            MonitorInfo {
                work_area: WORK_AREA,
            },
            400,
            640,
            100,
            100,
            Padding {
                window_gap: 16,
                up: 0,
                down: 0,
            },
            4,
        );
        scrolling.add_window(id);
        let floating = FloatingSpace::new();

        let rect = focused_window_rect(&scrolling, &floating, id);
        assert!(
            rect.is_some(),
            "tiling window must resolve to its actual rect"
        );
    }

    #[test]
    fn focused_window_rect_none_for_unknown_window() {
        use crate::layout::types::{MonitorInfo, Padding};
        use crate::workspace::{FloatingSpace, ScrollingSpace};

        let scrolling = ScrollingSpace::new(
            MonitorInfo {
                work_area: WORK_AREA,
            },
            400,
            640,
            100,
            100,
            Padding {
                window_gap: 16,
                up: 0,
                down: 0,
            },
            4,
        );
        let floating = FloatingSpace::new();
        assert_eq!(
            focused_window_rect(&scrolling, &floating, WindowId(999)),
            None
        );
    }
}
