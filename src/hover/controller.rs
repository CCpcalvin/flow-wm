//! Pure, clock-injectable hover decision controller.
//!
//! This is the single new test seam for the whole hover feature. It is a pure
//! state machine: it owns the focus-follows-mouse dwell and the edge-hover
//! dwell, decides the next action from injected time ([`Instant`]) and event
//! inputs (cursor polls, foreground changes, timer fires), and emits the
//! [`HoverAction`] contract that the wiring tickets translate into Win32 calls.
//!
//! It touches **no** Win32, the daemon, or the layout engine — only
//! [`crate::common`] vocabulary types. So every rule below is a hermetic unit
//! test with no daemon construction, mirroring the codebase precedent of the
//! drag's `EdgeScrollScheduler` and the main loop's `compute_wait_timeout_inner`.
//!
//! # Decisions encoded (see the feature spec "Implementation Decisions" and
//! (`docs/src/dev-guide/hover.md`))
//!
//! - **Movement-gate:** a focus-follows-mouse dwell arms only when a poll
//!   observes the cursor at a position *different from the previous poll* and
//!   over an *eligible* window (tracked and not already foreground). Any motion
//!   restarts the dwell, so a jittering mouse never focuses. The first poll has
//!   no previous position, so it never arms — the cursor must actually move.
//! - **Alt-tab respect:** any foreground-change event cancels the dwell. After
//!   the cancel the cursor has not moved, so the dwell cannot re-arm until the
//!   mouse moves — no steal-back, with no keyboard detection or cooldown.
//! - **Edge-band precedence:** when the cursor is in an edge band the edge path
//!   owns the poll and any pending focus-follows-mouse dwell is cancelled.
//! - **Edge-dwell:** entry arms an edge-dwell timer; its expiry emits
//!   [`HoverAction::EdgeEnter`]; leaving the band emits
//!   [`HoverAction::EdgeLeave`] and [`HoverAction::CancelEdgeDwell`].
//! - **No defocus:** hovering an ineligible or absent target drops a pending
//!   dwell ([`HoverAction::CancelDwell`]) but never defocuses.

use std::time::{Duration, Instant};

use crate::common::{Direction, Point, WindowId};

/// A single action the controller asks the caller (the wiring) to perform.
///
/// This is the contract between the pure decision logic and the impure Win32
/// calls. Each variant is the precise effect of one input event; the caller
/// drains them and applies each. Cancel variants are idempotent at the wiring
/// level — safe to emit when nothing is armed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoverAction {
    /// Focus-follows-mouse dwell elapsed over an eligible window — focus it.
    Focus(WindowId),
    /// (Re)arm the focus-follows-mouse dwell timer to fire at `deadline`.
    ArmDwell(Instant),
    /// Cancel the focus-follows-mouse dwell (foreground changed or eligibility
    /// lost). Never defocuses.
    CancelDwell,
    /// Arm the edge-dwell timer to fire at `deadline` (cursor entered a band).
    ArmEdgeDwell(Instant),
    /// Cancel the edge-dwell timer (cursor left the band).
    CancelEdgeDwell,
    /// Edge-dwell elapsed — hand the band to the shared edge-scroll scheduler
    /// in `direction`.
    EdgeEnter(Direction),
    /// Cursor left the band — tell the shared edge-scroll scheduler to stop.
    EdgeLeave,
    /// Nothing to do.
    NoOp,
}

/// One cursor poll fed to the controller.
///
/// The wiring resolves the cursor (`GetCursorPos`), classifies it against the
/// edge band via [`edge_band_direction`](super::edge_band_direction), and
/// resolves the eligible focus target (`WindowFromPoint` walked to its top-level
/// ancestor, tracked and not already foreground). The controller consumes all
/// three as pure inputs and never touches Win32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoverPoll {
    /// Absolute screen position of the cursor this poll.
    pub cursor: Point,
    /// The edge band the cursor is in this poll (`None` = off-band). Drives
    /// edge-band precedence.
    pub edge_band: Option<Direction>,
    /// The eligible focus-follows-mouse target under the cursor. `Some(hwnd)`
    /// means a tracked, not-already-foreground managed window is there; `None`
    /// means no eligible target (untracked window, or no window). Consulted
    /// only when [`edge_band`](Self::edge_band) is `None`.
    pub target: Option<WindowId>,
}

/// The already-clamped effective dwell durations fed to the controller.
///
/// The config layer clamps the raw values; the controller consumes the result
/// and holds no clamp math, exactly like the drag's `EdgeScrollTimings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoverTimings {
    /// Focus-follows-mouse dwell before focus fires.
    pub focus_dwell: Duration,
    /// Edge-band dwell before the first edge-scroll fires (shorter than the
    /// focus dwell — reaching the edge is already a deliberate gesture).
    pub edge_dwell: Duration,
}

/// Edge-path state machine state.
///
/// - [`Self::Off`] — cursor not in a band.
/// - [`Self::Arming`] — entered a band; the edge-dwell timer is pending.
/// - [`Self::Active`] — edge-dwell elapsed and [`HoverAction::EdgeEnter`] sent;
///   the shared edge-scroll scheduler now owns the repeat until the band is left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeState {
    /// Not in an edge band.
    Off,
    /// In a band, edge-dwell timer pending.
    Arming(Direction),
    /// In a band, edge-dwell elapsed — the shared scheduler owns the repeat.
    Active(Direction),
}

/// The pure hover decision controller.
///
/// Hold one of these per daemon (it is per-workspace in spirit, but the daemon
/// targets the single active workspace today). Drive it through the event
/// methods and apply the [`HoverAction`]s each returns.
///
/// - [`Self::on_poll`] — every cursor poll; returns the (possibly compound)
///   effects of precedence, the movement-gate, and band transitions.
/// - [`Self::on_foreground_change`] — any `EVENT_SYSTEM_FOREGROUND`; cancels the
///   focus-follows-mouse dwell.
/// - [`Self::on_dwell_timer_fired`] — the focus-follows-mouse dwell elapsed.
/// - [`Self::on_edge_dwell_timer_fired`] — the edge-dwell elapsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverController {
    /// The cursor position from the previous poll (`None` until the first
    /// poll). Drives the movement-gate.
    prev_cursor: Option<Point>,
    /// The armed focus-follows-mouse dwell: the target window and its deadline.
    /// `None` when no dwell is armed.
    ffm: Option<(WindowId, Instant)>,
    /// Edge-path state.
    edge_state: EdgeState,
}

impl Default for HoverController {
    fn default() -> Self {
        Self::new()
    }
}

impl HoverController {
    /// A fresh controller with no dwell armed and no previous cursor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            prev_cursor: None,
            ffm: None,
            edge_state: EdgeState::Off,
        }
    }

    /// Drop all armed state — the previous cursor, any focus-follows-mouse
    /// dwell, and the edge-path state — so the next poll starts fresh.
    ///
    /// Called when a tile drag begins: the hover subsystem is suppressed for the
    /// drag's duration, and without this reset an armed edge state would persist
    /// past the drag, so edge-hover-scroll would not re-arm until the cursor left
    /// and re-entered the band. Mirrors the shared scheduler's fresh re-arm at
    /// drag start.
    pub fn reset(&mut self) {
        self.prev_cursor = None;
        self.ffm = None;
        self.edge_state = EdgeState::Off;
    }

    /// Process one cursor poll.
    ///
    /// Applies edge-band precedence first (any pending focus-follows-mouse dwell
    /// is cancelled on band entry), then — only when off-band — the
    /// movement-gated focus-follows-mouse classification. Returns the ordered
    /// list of effects for the caller to apply.
    pub fn on_poll(
        &mut self,
        poll: HoverPoll,
        now: Instant,
        timings: &HoverTimings,
    ) -> Vec<HoverAction> {
        // The movement-gate: armed only on an observed position change. The
        // first poll has no previous position, so it never counts as movement.
        let moved = self.prev_cursor.is_some_and(|prev| prev != poll.cursor);
        let mut actions = Vec::new();

        let prev_band = self.edge_band_dir();
        let band_changed = prev_band != poll.edge_band;

        if band_changed {
            if prev_band.is_some() {
                // Leaving a band (to off-band or to a different band's edge).
                // Both signals are idempotent at the wiring level: EdgeLeave
                // stops the shared scheduler, CancelEdgeDwell drops the timer.
                actions.push(HoverAction::EdgeLeave);
                actions.push(HoverAction::CancelEdgeDwell);
                self.edge_state = EdgeState::Off;
            }
            if let Some(dir) = poll.edge_band {
                // Entering a band: edge precedence cancels any pending FFM dwell.
                self.cancel_ffm(&mut actions);
                self.edge_state = EdgeState::Arming(dir);
                actions.push(HoverAction::ArmEdgeDwell(now + timings.edge_dwell));
            }
        }

        // The focus-follows-mouse path runs only when off-band (edge precedence).
        if poll.edge_band.is_none() && moved {
            match poll.target {
                Some(hwnd) => {
                    // Moved onto an eligible window: (re)arm the dwell. A restart
                    // replaces any previously armed dwell — one timer at the
                    // wiring level — so a jittering mouse never focuses.
                    let deadline = now + timings.focus_dwell;
                    self.ffm = Some((hwnd, deadline));
                    actions.push(HoverAction::ArmDwell(deadline));
                }
                None => {
                    // Moved onto an ineligible/absent target: drop a pending
                    // dwell, but never defocus.
                    if self.ffm.take().is_some() {
                        actions.push(HoverAction::CancelDwell);
                    }
                }
            }
        }

        self.prev_cursor = Some(poll.cursor);
        actions
    }

    /// Any `EVENT_SYSTEM_FOREGROUND` (alt-tab, external click, or self-induced).
    ///
    /// Cancels the focus-follows-mouse dwell. Does not touch the edge path
    /// (edge-scroll is independent of focus). Returns [`HoverAction::NoOp`] when
    /// no dwell was armed.
    #[must_use]
    pub fn on_foreground_change(&mut self) -> HoverAction {
        if self.ffm.take().is_some() {
            HoverAction::CancelDwell
        } else {
            HoverAction::NoOp
        }
    }

    /// The focus-follows-mouse dwell timer elapsed.
    ///
    /// Emits [`HoverAction::Focus`] for the armed target and clears the dwell.
    /// Returns [`HoverAction::NoOp`] on a spurious fire with nothing armed.
    #[must_use]
    pub fn on_dwell_timer_fired(&mut self) -> HoverAction {
        if let Some((hwnd, _)) = self.ffm.take() {
            HoverAction::Focus(hwnd)
        } else {
            HoverAction::NoOp
        }
    }

    /// The edge-dwell timer elapsed.
    ///
    /// Emits [`HoverAction::EdgeEnter`] in the band's direction and promotes the
    /// edge state so subsequent polls no-op (the shared scheduler owns the
    /// repeat). Returns [`HoverAction::NoOp`] on a spurious fire.
    #[must_use]
    pub fn on_edge_dwell_timer_fired(&mut self) -> HoverAction {
        if let EdgeState::Arming(dir) = self.edge_state {
            self.edge_state = EdgeState::Active(dir);
            HoverAction::EdgeEnter(dir)
        } else {
            HoverAction::NoOp
        }
    }

    /// The direction of the band the controller is currently in (`None` off-band).
    fn edge_band_dir(&self) -> Option<Direction> {
        match self.edge_state {
            EdgeState::Off => None,
            EdgeState::Arming(d) | EdgeState::Active(d) => Some(d),
        }
    }

    /// Cancel a pending focus-follows-mouse dwell, pushing [`HoverAction::CancelDwell`]
    /// only if one was armed.
    fn cancel_ffm(&mut self, actions: &mut Vec<HoverAction>) {
        if self.ffm.take().is_some() {
            actions.push(HoverAction::CancelDwell);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed dwell durations; tests reason about deadlines relative to `t0`.
    const TIMINGS: HoverTimings = HoverTimings {
        focus_dwell: Duration::from_millis(300),
        edge_dwell: Duration::from_millis(150),
    };

    /// A reference instant; `Instant` has no arbitrary constructor, so anchor
    /// once and add durations for deterministic deadlines.
    fn now() -> Instant {
        Instant::now()
    }

    fn pt(x: i32, y: i32) -> Point {
        Point { x, y }
    }

    /// A poll off-band over an eligible window.
    fn poll_over(hwnd: WindowId, at: Point) -> HoverPoll {
        HoverPoll {
            cursor: at,
            edge_band: None,
            target: Some(hwnd),
        }
    }

    /// A poll off-band over no eligible target.
    fn poll_none(at: Point) -> HoverPoll {
        HoverPoll {
            cursor: at,
            edge_band: None,
            target: None,
        }
    }

    /// A poll inside the given edge band (target is irrelevant under precedence).
    fn poll_band(dir: Direction, at: Point) -> HoverPoll {
        HoverPoll {
            cursor: at,
            edge_band: Some(dir),
            target: Some(WindowId(999)), // ignored under precedence
        }
    }

    const W1: WindowId = WindowId(1);
    const W2: WindowId = WindowId(2);

    // =====================================================================
    // Movement-gate
    // =====================================================================

    #[test]
    fn first_poll_sets_baseline_and_does_not_arm() {
        // No previous position → not "moved" → no arm, even over an eligible window.
        let mut c = HoverController::new();
        let t = now();
        let actions = c.on_poll(poll_over(W1, pt(100, 100)), t, &TIMINGS);
        assert!(actions.is_empty(), "first poll must not arm: {actions:?}");
        assert_eq!(c.ffm, None);
    }

    #[test]
    fn second_poll_with_movement_arms_dwell() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_over(W1, pt(100, 100)), t, &TIMINGS); // baseline
        let actions = c.on_poll(poll_over(W1, pt(120, 100)), t, &TIMINGS); // moved
        assert_eq!(actions, vec![HoverAction::ArmDwell(t + TIMINGS.focus_dwell)]);
        assert_eq!(c.ffm, Some((W1, t + TIMINGS.focus_dwell)));
    }

    #[test]
    fn stationary_cursor_does_not_arm() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_over(W1, pt(100, 100)), t, &TIMINGS); // baseline
        // Same position → no movement → no arm.
        let actions = c.on_poll(poll_over(W1, pt(100, 100)), t, &TIMINGS);
        assert!(actions.is_empty(), "stationary cursor must not arm: {actions:?}");
        assert_eq!(c.ffm, None);
    }

    #[test]
    fn stationary_cursor_does_not_restart_armed_dwell() {
        // Armed, then a stationary poll: the dwell persists untouched (not restarted).
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_over(W1, pt(100, 100)), t, &TIMINGS); // baseline
        c.on_poll(poll_over(W1, pt(120, 100)), t, &TIMINGS); // arm
        let first_deadline = c.ffm.unwrap().1;
        // Stationary poll: deadline unchanged.
        let actions = c.on_poll(poll_over(W1, pt(120, 100)), t, &TIMINGS);
        assert!(actions.is_empty());
        assert_eq!(c.ffm, Some((W1, first_deadline)));
    }

    #[test]
    fn jittering_cursor_restarts_dwell_each_poll_and_never_focuses() {
        // Every poll moves; each restarts the dwell. As long as it keeps moving
        // within the dwell window, no Focus is emitted by the poll itself.
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_over(W1, pt(0, 0)), t, &TIMINGS); // baseline
        for i in 1..5 {
            let actions = c.on_poll(poll_over(W1, pt(i * 10, 0)), t, &TIMINGS);
            // Every moving poll re-arms at the same `now + dwell`.
            assert_eq!(actions, vec![HoverAction::ArmDwell(t + TIMINGS.focus_dwell)]);
            // The poll never emits Focus — only the timer-fire event does.
        }
    }

    // =====================================================================
    // Dwell arm / fire / cancel
    // =====================================================================

    #[test]
    fn dwell_timer_fires_focuses_target() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_over(W1, pt(100, 100)), t, &TIMINGS); // baseline
        c.on_poll(poll_over(W1, pt(120, 100)), t, &TIMINGS); // arm W1
        assert_eq!(c.on_dwell_timer_fired(), HoverAction::Focus(W1));
        // Fire clears the dwell.
        assert_eq!(c.ffm, None);
    }

    #[test]
    fn dwell_timer_fire_with_nothing_armed_is_noop() {
        let mut c = HoverController::new();
        assert_eq!(c.on_dwell_timer_fired(), HoverAction::NoOp);
    }

    #[test]
    fn moving_to_ineligible_cancels_armed_dwell() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_over(W1, pt(100, 100)), t, &TIMINGS); // baseline
        c.on_poll(poll_over(W1, pt(120, 100)), t, &TIMINGS); // arm
        // Move onto an untracked/none target → cancel.
        let actions = c.on_poll(poll_none(pt(150, 100)), t, &TIMINGS);
        assert_eq!(actions, vec![HoverAction::CancelDwell]);
        assert_eq!(c.ffm, None);
    }

    #[test]
    fn moving_to_ineligible_with_no_dwell_is_noop() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_none(pt(100, 100)), t, &TIMINGS); // baseline (none)
        // Move to another ineligible spot: nothing armed → NoOp (no CancelDwell).
        let actions = c.on_poll(poll_none(pt(150, 100)), t, &TIMINGS);
        assert!(actions.is_empty(), "no dwell to drop: {actions:?}");
    }

    #[test]
    fn no_defocus_moving_to_ineligible_emits_no_focus() {
        // Moving to ineligible must emit only CancelDwell, never Focus/defocus.
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_over(W1, pt(100, 100)), t, &TIMINGS);
        c.on_poll(poll_over(W1, pt(120, 100)), t, &TIMINGS); // arm
        let actions = c.on_poll(poll_none(pt(150, 100)), t, &TIMINGS);
        assert!(actions.iter().all(|a| !matches!(a, HoverAction::Focus(_))));
        assert!(actions.contains(&HoverAction::CancelDwell));
    }

    // =====================================================================
    // Alt-tab respect (cancel-on-foreground)
    // =====================================================================

    #[test]
    fn foreground_change_cancels_armed_dwell() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_over(W1, pt(100, 100)), t, &TIMINGS);
        c.on_poll(poll_over(W1, pt(120, 100)), t, &TIMINGS); // arm
        assert_eq!(c.on_foreground_change(), HoverAction::CancelDwell);
        assert_eq!(c.ffm, None);
    }

    #[test]
    fn foreground_change_with_no_dwell_is_noop() {
        let mut c = HoverController::new();
        assert_eq!(c.on_foreground_change(), HoverAction::NoOp);
    }

    #[test]
    fn after_foreground_cancel_stationary_cursor_does_not_re_arm() {
        // The alt-tab steal-back defeat: after a focus change, the cursor has
        // not moved, so the dwell cannot re-arm until the mouse actually moves.
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_over(W1, pt(120, 100)), t, &TIMINGS); // baseline at 120
        c.on_poll(poll_over(W1, pt(130, 100)), t, &TIMINGS); // move + arm
        let _ = c.on_foreground_change(); // alt-tab cancels

        // Stationary poll at the same spot → no re-arm.
        let actions = c.on_poll(poll_over(W1, pt(130, 100)), t, &TIMINGS);
        assert!(actions.is_empty(), "no re-arm without movement: {actions:?}");
        assert_eq!(c.ffm, None);
    }

    #[test]
    fn after_foreground_cancel_moving_cursor_re_arms() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_over(W1, pt(120, 100)), t, &TIMINGS);
        c.on_poll(poll_over(W1, pt(130, 100)), t, &TIMINGS); // arm
        let _ = c.on_foreground_change(); // cancel

        // Actually move the mouse → re-arm.
        let actions = c.on_poll(poll_over(W1, pt(140, 100)), t, &TIMINGS);
        assert_eq!(actions, vec![HoverAction::ArmDwell(t + TIMINGS.focus_dwell)]);
    }

    #[test]
    fn foreground_change_does_not_touch_edge_path() {
        // A foreground event cancels only the FFM dwell; the edge dwell persists.
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_band(Direction::Left, pt(0, 500)), t, &TIMINGS); // enter band
        assert_eq!(c.on_foreground_change(), HoverAction::NoOp); // no FFM armed
        assert_eq!(c.edge_state, EdgeState::Arming(Direction::Left));
    }

    // =====================================================================
    // Edge-band precedence
    // =====================================================================

    #[test]
    fn entering_band_arms_edge_dwell() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_none(pt(100, 500)), t, &TIMINGS); // baseline off-band
        let actions = c.on_poll(poll_band(Direction::Left, pt(0, 500)), t, &TIMINGS);
        assert_eq!(
            actions,
            vec![HoverAction::ArmEdgeDwell(t + TIMINGS.edge_dwell)]
        );
        assert_eq!(c.edge_state, EdgeState::Arming(Direction::Left));
    }

    #[test]
    fn entering_band_cancels_pending_ffm_dwell() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_over(W1, pt(100, 500)), t, &TIMINGS); // baseline
        c.on_poll(poll_over(W1, pt(50, 500)), t, &TIMINGS); // arm FFM
        // Cursor moves into the band → precedence cancels FFM + arms edge.
        let actions = c.on_poll(poll_band(Direction::Left, pt(0, 500)), t, &TIMINGS);
        assert_eq!(
            actions,
            vec![
                HoverAction::CancelDwell,
                HoverAction::ArmEdgeDwell(t + TIMINGS.edge_dwell),
            ]
        );
        assert_eq!(c.ffm, None);
        assert_eq!(c.edge_state, EdgeState::Arming(Direction::Left));
    }

    #[test]
    fn in_band_ignores_ffm_target_even_when_eligible_and_moved() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_band(Direction::Left, pt(0, 500)), t, &TIMINGS); // enter
        // Poll inside the band at a new position with an eligible target: the
        // edge path owns this poll — no ArmDwell, no edge transition (same band).
        let actions = c.on_poll(poll_band(Direction::Left, pt(2, 500)), t, &TIMINGS);
        assert!(actions.is_empty(), "edge precedence suppresses FFM: {actions:?}");
        assert_eq!(c.ffm, None);
    }

    #[test]
    fn staying_in_same_band_is_noop() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_band(Direction::Left, pt(0, 500)), t, &TIMINGS); // enter → Arming
        let actions = c.on_poll(poll_band(Direction::Left, pt(3, 500)), t, &TIMINGS);
        assert!(actions.is_empty());
        assert_eq!(c.edge_state, EdgeState::Arming(Direction::Left));
    }

    // =====================================================================
    // Edge-dwell arm / fire / leave
    // =====================================================================

    #[test]
    fn edge_dwell_fires_emits_edge_enter_and_goes_active() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_band(Direction::Right, pt(1919, 500)), t, &TIMINGS); // enter
        assert_eq!(
            c.on_edge_dwell_timer_fired(),
            HoverAction::EdgeEnter(Direction::Right)
        );
        assert_eq!(c.edge_state, EdgeState::Active(Direction::Right));
    }

    #[test]
    fn active_band_poll_is_noop_scheduler_owns_repeat() {
        // After EdgeEnter, the shared scheduler repeats on its own timer; the
        // controller no-ops while the cursor stays in the band.
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_band(Direction::Right, pt(1919, 500)), t, &TIMINGS);
        let _ = c.on_edge_dwell_timer_fired(); // → Active
        let actions = c.on_poll(poll_band(Direction::Right, pt(1918, 500)), t, &TIMINGS);
        assert!(actions.is_empty());
        assert_eq!(c.edge_state, EdgeState::Active(Direction::Right));
    }

    #[test]
    fn edge_dwell_fire_with_nothing_armed_is_noop() {
        let mut c = HoverController::new();
        assert_eq!(c.on_edge_dwell_timer_fired(), HoverAction::NoOp);
    }

    #[test]
    fn edge_dwell_fire_after_already_active_is_noop() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_band(Direction::Left, pt(0, 500)), t, &TIMINGS);
        let _ = c.on_edge_dwell_timer_fired(); // Active
        assert_eq!(c.on_edge_dwell_timer_fired(), HoverAction::NoOp);
    }

    #[test]
    fn leaving_band_before_fire_cancels_edge_dwell() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_band(Direction::Left, pt(0, 500)), t, &TIMINGS); // enter → Arming
        // Leave the band (back to off-band) before the edge-dwell fires.
        let actions = c.on_poll(poll_none(pt(100, 500)), t, &TIMINGS);
        assert_eq!(
            actions,
            vec![HoverAction::EdgeLeave, HoverAction::CancelEdgeDwell]
        );
        assert_eq!(c.edge_state, EdgeState::Off);
    }

    #[test]
    fn leaving_band_after_fire_stops_scheduler() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_band(Direction::Left, pt(0, 500)), t, &TIMINGS);
        let _ = c.on_edge_dwell_timer_fired(); // Active
        // Leave the band: EdgeLeave (stop scheduler) + idempotent CancelEdgeDwell.
        let actions = c.on_poll(poll_none(pt(100, 500)), t, &TIMINGS);
        assert_eq!(
            actions,
            vec![HoverAction::EdgeLeave, HoverAction::CancelEdgeDwell]
        );
        assert_eq!(c.edge_state, EdgeState::Off);
    }

    #[test]
    fn switching_band_direction_leaves_then_enters() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_band(Direction::Left, pt(0, 500)), t, &TIMINGS); // enter Left
        let _ = c.on_edge_dwell_timer_fired(); // Active on Left
        // Cursor jumps from the left band to the right band in one poll.
        let actions = c.on_poll(poll_band(Direction::Right, pt(1919, 500)), t, &TIMINGS);
        assert_eq!(
            actions,
            vec![
                HoverAction::EdgeLeave,
                HoverAction::CancelEdgeDwell,
                HoverAction::ArmEdgeDwell(t + TIMINGS.edge_dwell),
            ]
        );
        assert_eq!(c.edge_state, EdgeState::Arming(Direction::Right));
    }

    #[test]
    fn switching_band_before_fire_cancels_then_re_arms() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_band(Direction::Left, pt(0, 500)), t, &TIMINGS); // Arming Left
        // Switch to Right before the Left edge-dwell fires.
        let actions = c.on_poll(poll_band(Direction::Right, pt(1919, 500)), t, &TIMINGS);
        assert_eq!(
            actions,
            vec![
                HoverAction::EdgeLeave,
                HoverAction::CancelEdgeDwell,
                HoverAction::ArmEdgeDwell(t + TIMINGS.edge_dwell),
            ]
        );
        assert_eq!(c.edge_state, EdgeState::Arming(Direction::Right));
    }

    #[test]
    fn leaving_band_onto_eligible_window_arms_ffm_after_leave_signals() {
        // A poll that leaves the band AND lands on an eligible window emits the
        // leave signals followed by an FFM arm.
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_band(Direction::Left, pt(0, 500)), t, &TIMINGS);
        let _ = c.on_edge_dwell_timer_fired(); // Active
        let actions = c.on_poll(poll_over(W2, pt(200, 500)), t, &TIMINGS);
        assert_eq!(
            actions,
            vec![
                HoverAction::EdgeLeave,
                HoverAction::CancelEdgeDwell,
                HoverAction::ArmDwell(t + TIMINGS.focus_dwell),
            ]
        );
        assert_eq!(c.ffm, Some((W2, t + TIMINGS.focus_dwell)));
        assert_eq!(c.edge_state, EdgeState::Off);
    }

    // =====================================================================
    // Restart semantics & re-arming over a different window
    // =====================================================================

    #[test]
    fn moving_between_eligible_windows_restarts_dwell_for_new_target() {
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_over(W1, pt(100, 100)), t, &TIMINGS); // baseline
        c.on_poll(poll_over(W1, pt(110, 100)), t, &TIMINGS); // arm W1
        // Move onto W2 (also eligible) → restart for W2 (single ArmDwell).
        let actions = c.on_poll(poll_over(W2, pt(300, 100)), t, &TIMINGS);
        assert_eq!(actions, vec![HoverAction::ArmDwell(t + TIMINGS.focus_dwell)]);
        assert_eq!(c.ffm, Some((W2, t + TIMINGS.focus_dwell)));
        // Firing now targets W2, not W1.
        assert_eq!(c.on_dwell_timer_fired(), HoverAction::Focus(W2));
    }

    #[test]
    fn entering_band_with_no_ffm_emits_only_edge_arm() {
        // Entering a band when nothing FFM is armed must not emit a stray CancelDwell.
        let mut c = HoverController::new();
        let t = now();
        c.on_poll(poll_none(pt(100, 500)), t, &TIMINGS); // baseline off-band, none
        let actions = c.on_poll(poll_band(Direction::Left, pt(0, 500)), t, &TIMINGS);
        assert_eq!(
            actions,
            vec![HoverAction::ArmEdgeDwell(t + TIMINGS.edge_dwell)]
        );
    }

    // =====================================================================
    // Default + construction
    // =====================================================================

    #[test]
    fn default_is_new() {
        assert_eq!(HoverController::default(), HoverController::new());
    }

    #[test]
    fn fresh_controller_has_no_armed_state() {
        let c = HoverController::new();
        assert_eq!(c.ffm, None);
        assert_eq!(c.edge_state, EdgeState::Off);
        assert_eq!(c.prev_cursor, None);
    }

    // =====================================================================
    // Full lifecycle: move → arm → dwell → focus, and edge enter → fire → scroll
    // =====================================================================

    #[test]
    fn ffm_lifecycle_move_arm_fire_focus() {
        let mut c = HoverController::new();
        let t0 = now();
        c.on_poll(poll_over(W1, pt(100, 100)), t0, &TIMINGS); // baseline
        // Move and arm at t0.
        assert_eq!(
            c.on_poll(poll_over(W1, pt(120, 100)), t0, &TIMINGS),
            vec![HoverAction::ArmDwell(t0 + TIMINGS.focus_dwell)]
        );
        // Stationary polls while the dwell runs do nothing.
        assert!(c
            .on_poll(poll_over(W1, pt(120, 100)), t0 + Duration::from_millis(100), &TIMINGS)
            .is_empty());
        // Timer fires at the deadline → focus.
        assert_eq!(
            c.on_dwell_timer_fired(),
            HoverAction::Focus(W1)
        );
        // Dwell consumed; a spurious second fire is NoOp.
        assert_eq!(c.on_dwell_timer_fired(), HoverAction::NoOp);
    }

    #[test]
    fn edge_lifecycle_enter_fire_leave() {
        let mut c = HoverController::new();
        let t0 = now();
        // Enter band → arm edge-dwell.
        assert_eq!(
            c.on_poll(poll_band(Direction::Left, pt(0, 500)), t0, &TIMINGS),
            vec![HoverAction::ArmEdgeDwell(t0 + TIMINGS.edge_dwell)]
        );
        // Edge-dwell fires → EdgeEnter, now Active.
        assert_eq!(
            c.on_edge_dwell_timer_fired(),
            HoverAction::EdgeEnter(Direction::Left)
        );
        // Held in the band → no-op (scheduler repeats on its own).
        assert!(c
            .on_poll(poll_band(Direction::Left, pt(2, 500)), t0 + Duration::from_millis(50), &TIMINGS)
            .is_empty());
        // Leave → stop scheduler + cancel edge-dwell (idempotent).
        assert_eq!(
            c.on_poll(poll_none(pt(100, 500)), t0 + Duration::from_millis(80), &TIMINGS),
            vec![HoverAction::EdgeLeave, HoverAction::CancelEdgeDwell]
        );
        assert_eq!(c.edge_state, EdgeState::Off);
    }

    #[test]
    fn reset_clears_armed_state_and_previous_cursor() {
        let mut c = HoverController::new();
        let t0 = now();
        // Establish a previous cursor, then drive the edge path to Active.
        let _ = c.on_poll(poll_none(pt(10, 10)), t0, &TIMINGS);
        let _ = c.on_poll(poll_band(Direction::Left, pt(0, 500)), t0 + Duration::from_millis(5), &TIMINGS);
        assert_eq!(
            c.on_edge_dwell_timer_fired(),
            HoverAction::EdgeEnter(Direction::Left)
        );
        assert_eq!(c.edge_state, EdgeState::Active(Direction::Left));
        assert!(c.prev_cursor.is_some());

        // reset() drops everything; the next poll behaves like a fresh controller.
        c.reset();
        assert_eq!(c.edge_state, EdgeState::Off);
        assert!(c.prev_cursor.is_none());
        assert!(c.ffm.is_none());

        // No previous cursor after reset, so the movement-gate treats the first
        // poll as not-moved: resting on an eligible target does NOT arm a dwell
        // (symmetric with a brand-new controller). This is what lets
        // edge-hover-scroll re-arm cleanly when polling resumes after a drag
        // even if the cursor never leaves the band.
        let actions = c.on_poll(poll_over(W1, pt(50, 50)), t0 + Duration::from_millis(10), &TIMINGS);
        assert!(actions.is_empty());
    }
}
