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
use crate::hover::{
    FfmCandidate, HoverAction, HoverPoll, HoverTimings, edge_band_direction, ffm_target_eligible,
};
use crate::registry::win32 as registry_win32;

use super::drag::interaction_suppresses_hover;
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
            || interaction_suppresses_hover(self.drag_state.as_ref())
        {
            return;
        }
        let now = Instant::now();
        let interval =
            Duration::from_millis(u64::from(self.config.hover.effective_poll_interval_ms()));
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
    /// it through the existing OS foreground path. The entire subsystem is inert
    /// while a tile drag is in progress. When focus-follows-mouse is off it
    /// clears any stale deadline (so a hot-reload disable can't pin the loop to a
    /// 1 ms busy-loop); otherwise it is a no-op when no dwell is armed or before
    /// the deadline arrives.
    pub(super) fn maybe_fire_focus_dwell(&mut self) {
        // Defense in depth: the hover subsystem is suppressed while a tile drag
        // is in progress (`on_drag_start` already clears this deadline).
        if interaction_suppresses_hover(self.drag_state.as_ref()) {
            return;
        }
        if !self.config.hover.focus_follows_mouse {
            // Flag disabled (e.g. toggled off via hot-reload while a dwell was
            // armed): drop the stale deadline so a past value cannot pin the
            // wait-timeout to a 1 ms busy-loop.
            self.focus_dwell_deadline = None;
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
    /// so the immediate-first-scroll-then-repeat behavior is reused exactly. The
    /// entire subsystem is inert while a tile drag is in progress. When
    /// edge-hover-scroll is off it clears any stale deadline (so a hot-reload
    /// disable can't pin the loop to a 1 ms busy-loop); otherwise it is a no-op
    /// when no edge-dwell is armed or before the deadline arrives.
    pub(super) fn maybe_fire_edge_dwell(&mut self) {
        // Defense in depth: the hover subsystem is suppressed while a tile drag
        // is in progress (`on_drag_start` already clears this deadline).
        if interaction_suppresses_hover(self.drag_state.as_ref()) {
            return;
        }
        if !self.config.hover.edge_scroll {
            // Flag disabled (e.g. toggled off via hot-reload while an edge-dwell
            // was armed): drop the stale deadline so a past value cannot pin the
            // wait-timeout to a 1 ms busy-loop.
            self.edge_dwell_deadline = None;
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
    /// sit on does not immediately steal focus back while the cursor is still.
    /// Note this cancel only holds **while the cursor is still**: a cursor that
    /// keeps moving re-arms on the next poll and can steal focus back — the
    /// moving-cursor case (during an IPC-driven workspace switch or viewport
    /// scroll) is handled by suspending hover for the animation's duration, not
    /// here. See
    /// `docs/adr/0009-ffm-active-workspace-and-animation-suppression.md`.
    /// Called from [`FlowWM::on_focus_changed`].
    pub(super) fn on_hover_foreground_change(&mut self) {
        let action = self.hover.on_foreground_change();
        self.apply_hover_action(action);
    }

    /// Resolve the focus-follows-mouse target under the cursor, if eligible.
    ///
    /// This is the Win32-coupled half of FFM target resolution: it performs
    /// only the OS lookups — `WindowFromPoint` walked to its top-level ancestor
    /// (so child controls read as their owning window), the registry lookup,
    /// the foreground query, and workspace resolution — then hands the gathered
    /// snapshot to the pure [`ffm_target_eligible`] predicate, which owns every
    /// eligibility rule (managed, on the active workspace, not already the
    /// foreground). See `src/hover/ffm.rs` and
    /// `docs/adr/0009-ffm-active-workspace-and-animation-suppression.md`.
    fn hover_ffm_target(&self, cx: i32, cy: i32) -> Option<WindowId> {
        let hwnd = registry_win32::window_from_point(cx, cy)?;
        let hwnd_handle = HWND(hwnd as *mut _);
        let window = self.registry.get_window(hwnd_handle);
        // Workspace resolution: a tracked window's home workspace must be the
        // active workspace, or it is never an FFM target (the load-bearing fix
        // for the workspace-switch flicker). Scoped to the active monitor —
        // matching `on_focus_changed`'s lookup — so a window on a non-active
        // monitor resolves to `None` and is ineligible (FFM does not cross
        // monitors). An untracked window has no home workspace either, so the
        // flag is false there.
        let active = self.active_monitor();
        let on_active_workspace = window.is_some_and(|_| {
            let active_id = active.active_workspace_id();
            active
                .find_workspace_containing(WindowId(hwnd))
                .is_some_and(|home| home == active_id)
        });
        let candidate = FfmCandidate {
            state: window.map(|w| &w.state),
            on_active_workspace,
            is_foreground: registry_win32::get_foreground_window() == Some(hwnd),
        };
        ffm_target_eligible(candidate).then_some(WindowId(hwnd))
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
