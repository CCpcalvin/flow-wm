//! Hover wiring — translates the pure
//! [`HoverController`](crate::hover::HoverController) into the live daemon.
//!
//! This module is the impure glue: it polls `GetCursorPos`, classifies the
//! cursor against the screen edge band (via
//! [`edge_band_direction`]) and resolves the
//! top-level window under it, feeds the controller a [`HoverPoll`], and applies
//! the returned [`HoverAction`]s — the OS foreground push for `Focus`,
//! arming/clearing the focus-dwell deadline for `ArmDwell` / `CancelDwell`, and
//! feeding the shared edge-scroll scheduler plus the edge-dwell deadline for the
//! edge actions. The pure decision logic (movement-gate, cancel-on-foreground,
//! eligibility precedence, edge-band precedence) lives entirely in the
//! controller; this module only translates.
//!
//! # Coverage
//!
//! Like the tile-drag lifecycle, this wiring is Win32-coupled and cannot be
//! unit-tested without a cross-cutting injection seam that is out of scope. It
//! is covered by the controller's hermetic unit tests plus manual interactive
//! testing. (`docs/src/dev-guide/hover.md`)

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::common::{Point, WindowId};
use crate::config::FlowConfig;
use crate::hover::{HoverAction, HoverPoll, HoverTimings, edge_band_direction};
use crate::registry::types::WindowState;
use crate::registry::win32 as registry_win32;

use super::types::FlowWM;

/// Compute the already-clamped effective hover dwell durations from the config.
///
/// Built once at construction (and on config reload) from
/// `HoverConfig::focus_dwell_ms` and `HoverConfig::edge_dwell_ms`; the
/// controller consumes the result with no per-event clamp math, mirroring the
/// drag's `edge_scroll_timings_for`.
pub(super) fn hover_timings_for(config: &FlowConfig) -> HoverTimings {
    HoverTimings {
        focus_dwell: Duration::from_millis(u64::from(config.hover.focus_dwell_ms)),
        edge_dwell: Duration::from_millis(u64::from(config.hover.edge_dwell_ms)),
    }
}

impl FlowWM {
    /// Poll the cursor and drive the hover behaviors (FFM and edge-hover-scroll).
    ///
    /// No-op when both behavior flags are off or a tile drag is in progress
    /// (the whole hover subsystem is suspended during a drag). Throttled to
    /// `config.hover.poll_interval_ms` via [`last_hover_poll`](Self::last_hover_poll):
    /// the loop can wake far more often on hook activity, but the poll only
    /// fires once per interval. On each poll it classifies the cursor against
    /// the screen edge band (using the active workspace's monitor work area and
    /// the shared `[edge_scroll]` band width) and resolves the FFM-eligible
    /// target, then feeds the controller and applies the emitted
    /// [`HoverAction`]s. Edge-band classification takes precedence over FFM
    /// (the controller cancels any pending FFM dwell on band entry).
    pub(super) fn poll_hover(&mut self) {
        if (!self.config.hover.focus_follows_mouse && !self.config.hover.edge_scroll)
            || self.drag_state.is_some()
        {
            return;
        }
        let now = Instant::now();
        let interval = Duration::from_millis(u64::from(
            self.config.hover.effective_poll_interval_ms(),
        ));
        // Throttle: only poll if the interval has elapsed since the last poll.
        // This bounds the poll rate to the configured interval regardless of how
        // often hook activity wakes the loop.
        if let Some(last) = self.last_hover_poll
            && now < last + interval
        {
            return;
        }
        self.last_hover_poll = Some(now);

        let (cx, cy) = match registry_win32::get_cursor_pos() {
            Ok(pos) => pos,
            Err(e) => {
                log::debug!("hover poll: GetCursorPos failed: {e}");
                return;
            }
        };

        let cursor_point = Point { x: cx, y: cy };

        // Classify the cursor against the screen edge band of the active
        // workspace's monitor work area, using the shared `[edge_scroll]` band
        // width (the same value drag edge-scroll uses). Disabled when
        // `edge_scroll` is off — the controller then sees `edge_band: None` and
        // only the FFM path can run.
        let edge_band = if self.config.hover.edge_scroll {
            let work_area = self.active_scrolling().monitor().work_area;
            edge_band_direction(cursor_point, work_area, self.config.edge_scroll.band_width)
        } else {
            None
        };

        // Resolve the FFM-eligible target only when FFM is on (skip the Win32
        // lookup otherwise). The controller ignores `target` while in a band
        // (edge precedence), so the two never conflict.
        let target = if self.config.hover.focus_follows_mouse {
            self.hover_ffm_target(cx, cy)
        } else {
            None
        };

        let poll = HoverPoll {
            cursor: cursor_point,
            edge_band,
            target,
        };
        let actions = self.hover.on_poll(poll, now, &self.hover_timings);
        for action in actions {
            self.apply_hover_action(action);
        }
    }

    /// Fire the focus-follows-mouse dwell if its armed deadline is due.
    ///
    /// A twin of [`FlowWM::maybe_fire_edge_scroll`]:
    /// called at the top of the main loop, it lands the dwell promptly when its
    /// deadline arrives — including when the cursor is held perfectly still,
    /// which is exactly the case that should focus. On fire it asks the
    /// controller for the [`HoverAction::Focus`] of the armed target and pushes
    /// it through the existing OS foreground path. No-op when focus-follows-mouse
    /// is off, no dwell is armed, or the deadline has not yet arrived.
    pub(super) fn maybe_fire_focus_dwell(&mut self) {
        if !self.config.hover.focus_follows_mouse {
            return;
        }
        let Some(deadline) = self.focus_dwell_deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        // The dwell is consumed whether or not the controller had one armed — a
        // spurious fire (nothing armed) is a harmless no-op — so clear the
        // deadline unconditionally before asking the controller to fire.
        self.focus_dwell_deadline = None;
        let action = self.hover.on_dwell_timer_fired();
        self.apply_hover_action(action);
    }

    /// Fire the edge-hover-scroll dwell if its armed deadline is due.
    ///
    /// A twin of [`maybe_fire_focus_dwell`](Self::maybe_fire_focus_dwell):
    /// called at the top of the main loop, it lands the edge-dwell promptly when
    /// its deadline arrives. On fire it asks the controller for the
    /// [`HoverAction::EdgeEnter`] (which feeds the shared edge-scroll scheduler),
    /// so the immediate-first-scroll-then-repeat behavior is reused exactly.
    /// No-op when edge-hover-scroll is off, no edge-dwell is armed, or the
    /// deadline has not yet arrived.
    pub(super) fn maybe_fire_edge_dwell(&mut self) {
        if !self.config.hover.edge_scroll {
            return;
        }
        let Some(deadline) = self.edge_dwell_deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        // The deadline is consumed whether or not the controller had one armed —
        // a spurious fire (nothing armed) is a harmless no-op in the controller —
        // so clear it unconditionally before asking the controller to fire.
        self.edge_dwell_deadline = None;
        let action = self.hover.on_edge_dwell_timer_fired();
        self.apply_hover_action(action);
    }

    /// Feed any `EVENT_SYSTEM_FOREGROUND` to the hover controller.
    ///
    /// An external focus change (alt-tab, click, or a self-induced push) cancels
    /// any pending focus-follows-mouse dwell, so the window the mouse happens to
    /// sit on does not immediately steal focus back. After the cancel the cursor
    /// has not moved, so the dwell cannot re-arm until the mouse actually moves
    /// — the movement-gate defeats the classic alt-tab steal-back with no
    /// keyboard detection or cooldown. Called from
    /// [`FlowWM::on_focus_changed`].
    pub(super) fn on_hover_foreground_change(&mut self) {
        let action = self.hover.on_foreground_change();
        self.apply_hover_action(action);
    }

    /// Resolve the focus-follows-mouse target under the cursor, if eligible.
    ///
    /// Walks `WindowFromPoint` to its top-level ancestor (so child controls read
    /// as their owning window), then checks eligibility: a tracked, managed
    /// window (tiling or floating — ignored windows are excluded) that is not
    /// already the foreground. Returns `None` for an untracked window, an
    /// ignored window, the taskbar/desktop, or the current foreground.
    fn hover_ffm_target(&self, cx: i32, cy: i32) -> Option<WindowId> {
        let hwnd = registry_win32::window_from_point(cx, cy)?;
        let hwnd_handle = HWND(hwnd as *mut _);
        let window = self.registry.get_window(hwnd_handle)?;
        // Eligibility: a managed window (tiling or floating). Ignored windows
        // (maximized/fullscreen) are tracked but excluded. Minimized/hidden
        // windows cannot be under the cursor, so a broad state match is safe.
        if !matches!(
            window.state,
            WindowState::Tiling(_) | WindowState::Floating(_)
        ) {
            return None;
        }
        // Not already the foreground (OS truth): focusing the current foreground
        // is a no-op and would let the cursor's window re-arm a dwell that fires
        // pointlessly. The controller's movement-gate handles restart-on-move.
        if registry_win32::get_foreground_window() == Some(hwnd) {
            return None;
        }
        Some(WindowId(hwnd))
    }

    /// Apply a controller-emitted [`HoverAction`] to the live orchestrator.
    ///
    /// `Focus` pushes the OS foreground (the existing focus path; the resulting
    /// `EVENT_SYSTEM_FOREGROUND` runs [`FlowWM::on_focus_changed`],
    /// which does scroll-to-reveal, border recolor, and workspace switching).
    /// `ArmDwell` / `CancelDwell` set / clear the focus-dwell deadline the main
    /// loop waits on. `ArmEdgeDwell` / `CancelEdgeDwell` set / clear the
    /// edge-dwell deadline. `EdgeEnter` feeds the shared edge-scroll scheduler
    /// the band entry (immediate scroll + arm the first-gap timer, reusing the
    /// drag's `scroll_once_and_rearm`); `EdgeLeave` tells the scheduler to stop
    /// and clears the edge-dwell deadline.
    fn apply_hover_action(&mut self, action: HoverAction) {
        match action {
            HoverAction::Focus(wid) => {
                // The OS foreground push triggers EVENT_SYSTEM_FOREGROUND →
                // on_focus_changed, which inherits scroll-to-reveal, border
                // refresh, and workspace switching. Do not reimplement it here.
                if !registry_win32::set_foreground_window(wid.0) {
                    log::debug!("hover focus: set_foreground_window failed for {}", wid.0);
                }
            }
            HoverAction::ArmDwell(deadline) => {
                self.focus_dwell_deadline = Some(deadline);
            }
            HoverAction::CancelDwell => {
                self.focus_dwell_deadline = None;
            }
            HoverAction::ArmEdgeDwell(deadline) => {
                self.edge_dwell_deadline = Some(deadline);
            }
            HoverAction::CancelEdgeDwell => {
                self.edge_dwell_deadline = None;
            }
            HoverAction::EdgeEnter(direction) => {
                // Feed the shared scheduler the band entry: `on_enter` records
                // the direction and returns the immediate-scroll request, then
                // `scroll_once_and_rearm` performs the scroll and arms the
                // first-gap timer (or stays idle at the content edge). This is
                // exactly the drag's band-entry path — one shared scheduler, one
                // immediate-then-first-gap-then-repeat state machine.
                let _ = self.edge_scroll.on_enter(direction);
                self.scroll_once_and_rearm(direction);
            }
            HoverAction::EdgeLeave => {
                // Tell the shared scheduler to stop and clear its timer.
                let action = self.edge_scroll.on_leave();
                self.apply_edge_scroll_action(action);
                self.edge_dwell_deadline = None;
            }
            HoverAction::NoOp => {}
        }
    }
}
