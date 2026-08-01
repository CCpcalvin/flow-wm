//! Pure, clock-injectable scheduler for edge-scroll auto-repeat, owned once on the orchestrator and shared by every edge-scroll consumer (the tile drag today).
//!
//! Replaces the old per-`LOCATIONCHANGE` scroll firing (which raced the viewport
//! to the far end one column per pixel) with the OS-keyboard-repeat model: one
//! **immediate** scroll on entering the edge band, a longer **initial delay**
//! (the first-gap), then continuous **repeat** at a shorter interval while the
//! cursor stays in the band. Because repetition runs off a timer and not the
//! window-move event stream, holding the cursor perfectly still at the edge
//! keeps scrolling — the move-driven design could not do this.
//!
//! This module is the single source of truth for that lifecycle. It is a small
//! state machine expressed directly as code, because a state machine states the
//! rules more precisely than prose:
//!
//! ```text
//! States: Idle | ArmedInitial | ArmedRepeat
//!
//! Idle --enter band-->  fire IMMEDIATE scroll
//!                       |-- scroll = Some --> ArmedInitial   (arm first-gap timer)
//!                       `-- scroll = None --> Idle            (already at content edge)
//!
//! ArmedInitial --timer fires-->  scroll
//!                       |-- scroll = Some --> ArmedRepeat     (arm repeat timer)
//!                       `-- scroll = None --> Idle
//!
//! ArmedRepeat  --timer fires-->  scroll
//!                       |-- scroll = Some --> ArmedRepeat     (re-arm repeat timer)
//!                       `-- scroll = None --> Idle
//!
//! ArmedInitial | ArmedRepeat  --leave band / drag end-->  cancel timer --> Idle
//! ```
//!
//! # Purity & the caller protocol
//!
//! The scheduler is **pure**: it owns its state and decides the next action from
//! events, an injected `now` ([`Instant`]), and the already-clamped effective
//! timings ([`EdgeScrollTimings`]). It never touches Win32, the layout engine,
//! or the animator — so every rule above is a hermetic unit test with no daemon
//! construction. This mirrors the codebase precedent: the main loop's
//! `compute_wait_timeout_inner` and the drag's `should_submit_preview` gate are
//! both pure, clock-injectable free functions extracted for the same reason.
//!
//! The scroll itself is impure (a real viewport mutation), so the scheduler does
//! not perform it — it emits a [`EdgeScrollAction::Scroll`] and the live drag
//! performs it, then reports whether the viewport actually moved back via
//! [`EdgeScrollScheduler::on_scroll_outcome`]. That boolean drives the
//! `Some`/`None` branching in the state machine (the content edge).
//!
//! Held on the orchestrator as a single instance shared by every edge-scroll
//! consumer — the tile drag's move handler today; a hover poll feed arrives in
//! a later ticket.

use std::time::{Duration, Instant};

use crate::common::Direction;

/// Auto-repeat state-machine state.
///
/// See the module docs for the full transition table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgeScrollState {
    /// No edge scroll armed. Entry into a band from here fires the immediate
    /// scroll.
    Idle,
    /// The first-gap timer is armed: the immediate scroll fired and succeeded,
    /// and we are waiting out the (longer) initial delay before repeating.
    ArmedInitial,
    /// The repeat timer is (re)armed: we are gliding at the steady repeat
    /// cadence.
    ArmedRepeat,
}

/// The already-clamped effective timings fed to the scheduler.
///
/// Built by the live drag from [`DragConfig::effective_repeat_interval_ms`] /
/// [`DragConfig::effective_initial_delay_ms`] so the scheduler itself contains
/// no clamp math — it just consumes the result.
///
/// [`DragConfig::effective_repeat_interval_ms`]: crate::config::DragConfig::effective_repeat_interval_ms
/// [`DragConfig::effective_initial_delay_ms`]: crate::config::DragConfig::effective_initial_delay_ms
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EdgeScrollTimings {
    /// Gap from the immediate entry scroll to the first repeat (the first-gap).
    pub initial_delay: Duration,
    /// Gap between successive repeat scrolls (the glide cadence).
    pub repeat_interval: Duration,
}

/// An action the scheduler asks the live drag to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgeScrollAction {
    /// Perform one column scroll in the given direction, then report whether the
    /// viewport moved via [`EdgeScrollScheduler::on_scroll_outcome`].
    Scroll(Direction),
    /// (Re)arm the auto-repeat timer to fire at `deadline`.
    Arm(Instant),
    /// Cancel any armed timer (drop the deadline). Idempotent — safe to emit when
    /// nothing is armed.
    Cancel,
}

/// The auto-repeat scheduler: a pure state machine held once on the
/// orchestrator and shared by every edge-scroll consumer.
///
/// The caller drives it through the event methods ([`Self::on_enter`],
/// [`Self::on_timer_fired`], [`Self::on_leave`], [`Self::on_drag_end`]) and the
/// outcome feedback ([`Self::on_scroll_outcome`]); each returns the action the
/// caller must apply. The stored direction is the last-known edge band; the
/// timer fires in that direction without re-reading the cursor (the band is
/// screen-edge-based, so scrolling the viewport does not move it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeScrollScheduler {
    state: EdgeScrollState,
    /// The direction of the band we are scrolling in. `None` only while `Idle`
    /// with no band entered.
    direction: Option<Direction>,
}

impl Default for EdgeScrollScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl EdgeScrollScheduler {
    /// A fresh scheduler in [`EdgeScrollState::Idle`].
    pub(crate) fn new() -> Self {
        Self {
            state: EdgeScrollState::Idle,
            direction: None,
        }
    }

    /// Current state-machine state (for introspection / assertions).
    #[cfg(test)]
    pub(crate) fn state(&self) -> EdgeScrollState {
        self.state
    }

    /// Cursor entered an edge band in `direction`.
    ///
    /// Fires the **immediate** defining scroll and records the direction. The
    /// caller performs the scroll and feeds whether it moved back through
    /// [`Self::on_scroll_outcome`], which arms the first-gap timer on success or
    /// stays idle at the content edge.
    ///
    /// Reset defensively to [`EdgeScrollState::Idle`] so re-entry always starts
    /// fresh at the immediate scroll — the well-behaved caller reaches this from
    /// `Idle` (leave-then-enter), but a stray re-entry cannot inherit a stale
    /// armed timer.
    pub(crate) fn on_enter(&mut self, direction: Direction) -> EdgeScrollAction {
        self.state = EdgeScrollState::Idle;
        self.direction = Some(direction);
        EdgeScrollAction::Scroll(direction)
    }

    /// Report whether the most recently requested scroll moved the viewport.
    ///
    /// Drives the `Some`/`None` branch of the state machine. Dispatches on the
    /// current state, which encodes which kind of scroll is pending:
    /// - [`Idle`](EdgeScrollState::Idle) → the immediate entry scroll just ran.
    /// - [`ArmedInitial`](EdgeScrollState::ArmedInitial) /
    ///   [`ArmedRepeat`](EdgeScrollState::ArmedRepeat) → a timer-fired repeat
    ///   just ran.
    pub(crate) fn on_scroll_outcome(
        &mut self,
        scrolled: bool,
        now: Instant,
        timings: &EdgeScrollTimings,
    ) -> EdgeScrollAction {
        match self.state {
            EdgeScrollState::Idle => {
                // Immediate-entry scroll outcome.
                if scrolled {
                    self.state = EdgeScrollState::ArmedInitial;
                    EdgeScrollAction::Arm(now + timings.initial_delay)
                } else {
                    // Already at the content edge: do not arm. Clear the
                    // direction for symmetry with `reset` so an Idle scheduler
                    // never carries a stale band.
                    self.direction = None;
                    EdgeScrollAction::Cancel
                }
            }
            EdgeScrollState::ArmedInitial | EdgeScrollState::ArmedRepeat => {
                if scrolled {
                    // Glide continues: (re)arm the repeat timer.
                    self.state = EdgeScrollState::ArmedRepeat;
                    EdgeScrollAction::Arm(now + timings.repeat_interval)
                } else {
                    // Content ran out mid-repeat: cancel and reset.
                    self.state = EdgeScrollState::Idle;
                    self.direction = None;
                    EdgeScrollAction::Cancel
                }
            }
        }
    }

    /// The armed timer fired.
    ///
    /// Returns [`EdgeScrollAction::Scroll`] in the stored direction for the
    /// caller to perform, after which it reports the outcome via
    /// [`Self::on_scroll_outcome`]. Returns `None` when no timer is armed
    /// (spurious fire from `Idle`) so the caller can no-op.
    pub(crate) fn on_timer_fired(&mut self) -> Option<EdgeScrollAction> {
        match self.state {
            EdgeScrollState::ArmedInitial | EdgeScrollState::ArmedRepeat => {
                self.direction.map(EdgeScrollAction::Scroll)
            }
            EdgeScrollState::Idle => None,
        }
    }

    /// Cursor left the edge band.
    ///
    /// Cancels and resets to [`EdgeScrollState::Idle`], so the next entry starts
    /// fresh at the immediate scroll.
    pub(crate) fn on_leave(&mut self) -> EdgeScrollAction {
        self.reset();
        EdgeScrollAction::Cancel
    }

    /// The drag ended.
    ///
    /// Same as [`Self::on_leave`] — the per-drag state (and its timer) is torn
    /// down — split out so the call site reads its intent.
    pub(crate) fn on_drag_end(&mut self) -> EdgeScrollAction {
        self.reset();
        EdgeScrollAction::Cancel
    }

    fn reset(&mut self) {
        self.state = EdgeScrollState::Idle;
        self.direction = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMINGS: EdgeScrollTimings = EdgeScrollTimings {
        initial_delay: Duration::from_millis(400),
        repeat_interval: Duration::from_millis(240),
    };

    /// A fixed reference instant the whole lifecycle is reasoned against, so the
    /// returned deadlines are deterministic. `Instant` has no public arbitrary
    /// constructor, so we anchor at `Instant::now()` once and add durations.
    fn now() -> Instant {
        Instant::now()
    }

    // --- Entry: the immediate scroll + arming on success ---

    #[test]
    fn enter_band_yields_immediate_scroll() {
        let mut sched = EdgeScrollScheduler::new();
        // Entering the band fires the defining immediate scroll, before any delay.
        assert_eq!(
            sched.on_enter(Direction::Left),
            EdgeScrollAction::Scroll(Direction::Left)
        );
        assert_eq!(sched.state(), EdgeScrollState::Idle); // pending outcome
    }

    #[test]
    fn successful_entry_scroll_arms_first_gap_timer() {
        let mut sched = EdgeScrollScheduler::new();
        let t = now();
        sched.on_enter(Direction::Left);
        // A successful immediate scroll arms the initial-delay timer.
        assert_eq!(
            sched.on_scroll_outcome(true, t, &TIMINGS),
            EdgeScrollAction::Arm(t + TIMINGS.initial_delay)
        );
        assert_eq!(sched.state(), EdgeScrollState::ArmedInitial);
    }

    #[test]
    fn entry_scroll_at_content_edge_does_not_arm() {
        let mut sched = EdgeScrollScheduler::new();
        let t = now();
        sched.on_enter(Direction::Right);
        // The scroll returned no result (content edge) → do not arm; stay Idle.
        assert_eq!(
            sched.on_scroll_outcome(false, t, &TIMINGS),
            EdgeScrollAction::Cancel
        );
        assert_eq!(sched.state(), EdgeScrollState::Idle);
        // An Idle scheduler never carries a stale band.
        assert_eq!(sched.direction, None);
    }

    // --- Initial-delay timer fire → scroll + arm repeat ---

    #[test]
    fn initial_delay_timer_fire_yields_scroll_and_arms_repeat() {
        let mut sched = EdgeScrollScheduler::new();
        let t = now();
        sched.on_enter(Direction::Left);
        sched.on_scroll_outcome(true, t, &TIMINGS); // → ArmedInitial

        // Timer fires at t + initial_delay.
        let fire_at = t + TIMINGS.initial_delay;
        assert_eq!(
            sched.on_timer_fired(),
            Some(EdgeScrollAction::Scroll(Direction::Left))
        );
        // Successful repeat → arm the (shorter) repeat timer, transition to Repeat.
        assert_eq!(
            sched.on_scroll_outcome(true, fire_at, &TIMINGS),
            EdgeScrollAction::Arm(fire_at + TIMINGS.repeat_interval)
        );
        assert_eq!(sched.state(), EdgeScrollState::ArmedRepeat);
    }

    // --- Repeat timer fire → scroll + re-arm repeat ---

    #[test]
    fn repeat_timer_fire_yields_scroll_and_re_arms() {
        let mut sched = EdgeScrollScheduler::new();
        let t = now();
        sched.on_enter(Direction::Left);
        sched.on_scroll_outcome(true, t, &TIMINGS); // ArmedInitial
        let first_fire = t + TIMINGS.initial_delay;
        sched.on_timer_fired();
        sched.on_scroll_outcome(true, first_fire, &TIMINGS); // ArmedRepeat

        // Second fire is a repeat fire: scroll + re-arm at the repeat cadence.
        let second_fire = first_fire + TIMINGS.repeat_interval;
        assert_eq!(
            sched.on_timer_fired(),
            Some(EdgeScrollAction::Scroll(Direction::Left))
        );
        assert_eq!(
            sched.on_scroll_outcome(true, second_fire, &TIMINGS),
            EdgeScrollAction::Arm(second_fire + TIMINGS.repeat_interval)
        );
        assert_eq!(sched.state(), EdgeScrollState::ArmedRepeat);
    }

    #[test]
    fn repeat_continues_indefinitely_at_steady_cadence() {
        // A long hold glides one column per repeat interval, indefinitely: the
        // first gap is the (longer) initial delay, every subsequent gap is the
        // (shorter) repeat interval.
        let mut sched = EdgeScrollScheduler::new();
        let t = now();
        sched.on_enter(Direction::Right);
        sched.on_scroll_outcome(true, t, &TIMINGS); // ArmedInitial, armed at t+400

        // First fire after the initial delay; each later fire one repeat later.
        let mut fire = t + TIMINGS.initial_delay;
        for _ in 0..10 {
            assert_eq!(
                sched.on_timer_fired(),
                Some(EdgeScrollAction::Scroll(Direction::Right))
            );
            assert_eq!(
                sched.on_scroll_outcome(true, fire, &TIMINGS),
                EdgeScrollAction::Arm(fire + TIMINGS.repeat_interval)
            );
            assert_eq!(sched.state(), EdgeScrollState::ArmedRepeat);
            fire += TIMINGS.repeat_interval;
        }
    }

    // --- Content edge cancels on repeat ---

    #[test]
    fn repeat_scroll_at_content_edge_cancels_and_resets() {
        let mut sched = EdgeScrollScheduler::new();
        let t = now();
        sched.on_enter(Direction::Left);
        sched.on_scroll_outcome(true, t, &TIMINGS);
        let fire = t + TIMINGS.initial_delay;
        sched.on_timer_fired();
        // Glide ArmedRepeat.
        sched.on_scroll_outcome(true, fire, &TIMINGS);
        assert_eq!(sched.state(), EdgeScrollState::ArmedRepeat);

        // Next repeat scroll hits the content edge → cancel, back to Idle.
        let next_fire = fire + TIMINGS.repeat_interval;
        assert_eq!(
            sched.on_timer_fired(),
            Some(EdgeScrollAction::Scroll(Direction::Left))
        );
        assert_eq!(
            sched.on_scroll_outcome(false, next_fire, &TIMINGS),
            EdgeScrollAction::Cancel
        );
        assert_eq!(sched.state(), EdgeScrollState::Idle);
    }

    // --- Leave / drag-end cancel and reset, enabling fresh re-entry ---

    #[test]
    fn leave_band_cancels_and_resets_to_idle() {
        let mut sched = EdgeScrollScheduler::new();
        let t = now();
        sched.on_enter(Direction::Left);
        sched.on_scroll_outcome(true, t, &TIMINGS);
        assert_eq!(sched.state(), EdgeScrollState::ArmedInitial);

        assert_eq!(sched.on_leave(), EdgeScrollAction::Cancel);
        assert_eq!(sched.state(), EdgeScrollState::Idle);
    }

    #[test]
    fn drag_end_cancels_and_resets_to_idle() {
        let mut sched = EdgeScrollScheduler::new();
        let t = now();
        sched.on_enter(Direction::Left);
        sched.on_scroll_outcome(true, t, &TIMINGS);
        sched.on_timer_fired();
        sched.on_scroll_outcome(true, t, &TIMINGS);
        assert_eq!(sched.state(), EdgeScrollState::ArmedRepeat);

        assert_eq!(sched.on_drag_end(), EdgeScrollAction::Cancel);
        assert_eq!(sched.state(), EdgeScrollState::Idle);
    }

    #[test]
    fn re_entry_after_leave_starts_fresh_at_immediate_scroll() {
        // Re-entering the band always yields the immediate scroll again, never
        // resumes a stale repeat.
        let mut sched = EdgeScrollScheduler::new();
        let t = now();
        sched.on_enter(Direction::Left);
        sched.on_scroll_outcome(true, t, &TIMINGS);
        sched.on_timer_fired();
        sched.on_scroll_outcome(true, t, &TIMINGS); // ArmedRepeat
        sched.on_leave(); // back to Idle

        // Re-enter: immediate scroll + first-gap arm, exactly like the first time.
        assert_eq!(
            sched.on_enter(Direction::Left),
            EdgeScrollAction::Scroll(Direction::Left)
        );
        assert_eq!(
            sched.on_scroll_outcome(true, t, &TIMINGS),
            EdgeScrollAction::Arm(t + TIMINGS.initial_delay)
        );
        assert_eq!(sched.state(), EdgeScrollState::ArmedInitial);
    }

    // --- Direction: timer fires in the last-known zone, flips naturally ---

    #[test]
    fn timer_fires_in_last_known_direction() {
        // The timer does not re-read the cursor: it scrolls the stored direction.
        let mut sched = EdgeScrollScheduler::new();
        let t = now();
        sched.on_enter(Direction::Right);
        sched.on_scroll_outcome(true, t, &TIMINGS);
        assert_eq!(
            sched.on_timer_fired(),
            Some(EdgeScrollAction::Scroll(Direction::Right))
        );
    }

    #[test]
    fn leaving_left_and_entering_right_flips_direction_naturally() {
        // Direction change needs no special case: it is a leave-then-enter, and
        // the new entry arms the opposite direction.
        let mut sched = EdgeScrollScheduler::new();
        let t = now();
        sched.on_enter(Direction::Left);
        sched.on_scroll_outcome(true, t, &TIMINGS);
        sched.on_leave();

        assert_eq!(
            sched.on_enter(Direction::Right),
            EdgeScrollAction::Scroll(Direction::Right)
        );
        sched.on_scroll_outcome(true, t, &TIMINGS);
        assert_eq!(
            sched.on_timer_fired(),
            Some(EdgeScrollAction::Scroll(Direction::Right))
        );
    }

    // --- Spurious / degenerate inputs ---

    #[test]
    fn timer_fire_from_idle_is_no_op() {
        let mut sched = EdgeScrollScheduler::new();
        assert_eq!(sched.on_timer_fired(), None);
        assert_eq!(sched.state(), EdgeScrollState::Idle);
    }

    #[test]
    fn leave_from_idle_is_idempotent_cancel() {
        let mut sched = EdgeScrollScheduler::new();
        assert_eq!(sched.on_leave(), EdgeScrollAction::Cancel);
        assert_eq!(sched.state(), EdgeScrollState::Idle);
    }

    #[test]
    fn zero_initial_delay_clamps_to_repeat_cadence() {
        // A 0 initial-delay config (clamped to the repeat interval upstream)
        // means the first-gap equals the repeat cadence: no special pause, no
        // double-scroll.
        let timings = EdgeScrollTimings {
            initial_delay: Duration::from_millis(240), // clamped == repeat
            repeat_interval: Duration::from_millis(240),
        };
        let mut sched = EdgeScrollScheduler::new();
        let t = now();
        sched.on_enter(Direction::Left);
        // First-gap arm uses the (clamped) initial delay.
        assert_eq!(
            sched.on_scroll_outcome(true, t, &timings),
            EdgeScrollAction::Arm(t + timings.initial_delay)
        );
        // And the subsequent repeat arm uses the same cadence.
        let fire = t + timings.initial_delay;
        sched.on_timer_fired();
        assert_eq!(
            sched.on_scroll_outcome(true, fire, &timings),
            EdgeScrollAction::Arm(fire + timings.repeat_interval)
        );
    }
}
