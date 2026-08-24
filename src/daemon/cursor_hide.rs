//! Pure, clock-injectable state machine for cursor hide after mouse
//! inactivity (ticket #37, parent spec #35).
//!
//! Design rationale — why polling instead of a hook, why the swap/restore
//! split, and the layered-restore safety net — lives in
//! `docs/adr/0007-cursor-hide-and-warp.md`; the domain terms (*cursor hide*,
//! *activity*, *move-size gesture*) are pinned in `CONTEXT.md`.
//!
//! After `hide_timeout_ms` with no mouse activity the system cursor becomes
//! invisible; any real mouse activity — pointer motion or a button press —
//! makes it visible again and restarts the timer. Activity detection is
//! main-loop polling (position via `GetCursorPos`, buttons via
//! `GetAsyncKeyState`): position change OR button-down = activity. There is
//! no low-level mouse hook and no new thread — the hide deadline joins the
//! main loop's existing deadline-driven handler set the same way the
//! float-resume, foreground-sync, and edge-scroll deadlines do.
//!
//! This module is the single source of truth for that lifecycle. It is a
//! small state machine expressed directly as code, because a state machine
//! states the rules more precisely than prose:
//!
//! ```text
//! States: Idle | Hidden
//!
//! Idle   --timeout elapses-->            Hide        (swap system cursors)
//! Hidden --activity (motion/button)-->   Unhide      (restore cursors)
//!                                          `-- restart timer --> Idle
//! Idle   --activity-->                   restart timer (stay visible)
//!
//! either --move-size gesture starts-->   Unhide (if hidden) + suspend
//! either --gesture ends-->               restart timer
//! either --warp-->                       Unhide (if hidden) + restart timer
//! ```
//!
//! # Purity & the caller protocol
//!
//! The scheduler is **pure**: it owns its state and decides the next action
//! from an injected `now` ([`Instant`]), the injected activity sample
//! (pointer position + button state, read by the caller via the thin Win32
//! wrappers), and the config knobs. It never touches Win32, the registry,
//! or the layout — so every rule above is a hermetic unit test with no
//! daemon construction. This mirrors the codebase precedent
//! ([`super::edge_scroll::EdgeScrollScheduler`], `compute_wait_timeout_inner`):
//! pure clock-injectable machines extracted for the same reason.
//!
//! The cursor swap itself is impure (session-global state), so the scheduler
//! does not perform it — it emits a [`CursorHideAction`] and the daemon
//! performs it.

use std::time::{Duration, Instant};

/// An action the scheduler asks the daemon to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CursorHideAction {
    /// Swap every system cursor shape to the transparent cursor.
    Hide,
    /// Restore the system cursor set (`SPI_SETCURSORS`).
    Unhide,
    /// Nothing to do — the visibility state is already correct.
    None,
}

/// Activity sample from one poll of the real input devices.
///
/// Read by the caller (the daemon's main loop) via
/// [`get_cursor_pos`](crate::registry::win32::get_cursor_pos) and
/// [`any_mouse_button_down`](crate::registry::win32::any_mouse_button_down),
/// so the machine itself stays Win32-free and testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ActivitySample {
    /// Pointer position in screen coordinates.
    pub position: (i32, i32),
    /// Whether either physical mouse button is currently held down.
    pub button_down: bool,
}

/// The cursor-hide scheduler: a pure state machine held on the daemon.
///
/// The caller drives it through [`Self::on_poll`] (every poll tick, with the
/// fresh activity sample and injected clock), [`Self::on_gesture_begin`] /
/// [`Self::on_gesture_end`] (move-size gesture suspension), and
/// [`Self::on_warp`] (a warp counts as activity — the interaction invariant
/// with `warp_on_focus`). The next deadline is always available via
/// [`Self::next_deadline`] for folding into the main loop's wait timeout;
/// `None` means the machine is inactive and no polling should be scheduled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CursorHideScheduler {
    /// Hide is enabled (`hide_timeout_ms > 0` in the live config).
    enabled: bool,
    /// The configured inactivity timeout.
    timeout: Duration,
    /// The configured poll interval.
    poll_interval: Duration,
    /// Whether the system cursors are currently swapped to blank.
    hidden: bool,
    /// A move-size gesture is in progress — hiding is suspended.
    gesture_active: bool,
    /// The instant until which the cursor stays visible (Idle) or hidden
    /// (after unhide-restart). `None` when disabled.
    deadline: Option<Instant>,
    /// The pointer position observed at the previous poll. `None` before
    /// the first poll (the first sample establishes the baseline and cannot
    /// itself be "motion").
    last_position: Option<(i32, i32)>,
}

/// Resolve the poll cadence actually used by the machine.
///
/// The config places no lower bound on `hide_timeout_ms`, so a legal
/// configuration can name an interval larger than the timeout (e.g.
/// `hide_timeout_ms = 100` with the default `poll_interval_ms = 125`).
/// Rather than reject it, the interval is clamped down to the timeout:
/// the activity poll then runs exactly once per timeout window, which is
/// the coarsest cadence that still observes every elapse. Validation only
/// rejects a zero interval (a busy loop); see `CursorConfig::validate`.
fn effective_poll_interval(hide_timeout_ms: u32, poll_interval_ms: u32) -> Duration {
    let interval = Duration::from_millis(poll_interval_ms as u64);
    let timeout = Duration::from_millis(hide_timeout_ms as u64);
    if interval > timeout {
        timeout.max(Duration::from_millis(1))
    } else {
        interval
    }
}

impl CursorHideScheduler {
    /// Build a scheduler from the live `[cursor]` config knobs.
    ///
    /// `hide_timeout_ms = 0` produces a **disabled** scheduler
    /// ([`Self::is_active`] is false, [`Self::next_deadline`] is `None`) —
    /// the caller must not schedule any polling for it, preserving the
    /// daemon's zero-CPU-while-idle property for default configs.
    pub(super) fn new(hide_timeout_ms: u32, poll_interval_ms: u32) -> Self {
        Self {
            enabled: hide_timeout_ms > 0,
            timeout: Duration::from_millis(hide_timeout_ms as u64),
            poll_interval: effective_poll_interval(hide_timeout_ms, poll_interval_ms),
            hidden: false,
            gesture_active: false,
            deadline: None,
            last_position: None,
        }
    }

    /// Whether the machine schedules work at all.
    ///
    /// `false` for `hide_timeout_ms = 0`: the daemon must not poll the
    /// cursor, fold any deadline into its wait timeout, or touch cursor
    /// state — opting out returns the daemon to zero-CPU-while-idle.
    pub(super) fn is_active(&self) -> bool {
        self.enabled
    }

    /// Whether the system cursors are currently swapped to blank.
    #[cfg(test)]
    pub(super) fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// The poll-interval cadence to fold into the main loop's wait timeout
    /// while the machine is active (and not suspended by a gesture).
    #[cfg(test)]
    pub(super) fn poll_interval(&self) -> Option<Duration> {
        self.enabled.then_some(self.poll_interval)
    }

    /// Apply a fresh config (hot-reload) to the running machine.
    ///
    /// Returns the action the caller must perform:
    /// - disabling hide (`hide_timeout_ms = 0`) un-hides if currently
    ///   hidden and stops all scheduling;
    /// - changing the timeout re-arms the timer from `now`;
    /// - while hidden, a resize keeps the cursor hidden but re-arms the
    ///   machine so the next poll resumes normally;
    /// - the poll interval change takes effect on the next deadline read.
    pub(super) fn reconfigure(
        &mut self,
        hide_timeout_ms: u32,
        poll_interval_ms: u32,
        now: Instant,
    ) -> CursorHideAction {
        let was_enabled = self.enabled;
        self.enabled = hide_timeout_ms > 0;
        self.timeout = Duration::from_millis(hide_timeout_ms as u64);
        self.poll_interval = effective_poll_interval(hide_timeout_ms, poll_interval_ms);

        if !self.enabled {
            // Disabled mid-flight: restore the cursors if blank and stop
            // scheduling. Reset the runtime state so a later re-enable
            // starts fresh (baseline re-established on first poll).
            let action = if self.hidden {
                self.hidden = false;
                CursorHideAction::Unhide
            } else {
                CursorHideAction::None
            };
            self.deadline = None;
            self.last_position = None;
            return action;
        }

        if !was_enabled {
            // Freshly enabled: arm the timer from now.
            self.deadline = Some(now + self.timeout);
            return CursorHideAction::None;
        }
        if self.hidden {
            // Staying enabled while hidden: keep hidden, re-arm so the
            // next poll (and any activity) behaves against the new knobs.
            self.deadline = Some(now + self.poll_interval);
            return CursorHideAction::None;
        }
        // Visible + still enabled: timeout changed (or not) — re-arm.
        self.deadline = Some(now + self.timeout);
        CursorHideAction::None
    }

    /// Feed one poll tick: the fresh activity sample and the injected clock.
    ///
    /// Activity is position change **or** button-down. Returns the action to
    /// perform (`Unhide` + timer restart on activity while hidden; `Hide`
    /// when the deadline elapses with no activity). A disabled machine
    /// returns [`CursorHideAction::None`] and must not be polled by the
    /// caller (defensive: also a no-op here).
    pub(super) fn on_poll(&mut self, sample: ActivitySample, now: Instant) -> CursorHideAction {
        if !self.enabled {
            return CursorHideAction::None;
        }

        // Motion = position differs from the previous sample. The very
        // first sample only establishes the baseline — enabling hide must
        // not immediately count as activity (nor hide early).
        let motion = self.last_position != Some(sample.position);
        self.last_position = Some(sample.position);
        let activity = motion || sample.button_down;

        // Hiding is suspended for the whole gesture: keep the cursor
        // visible, keep polling only to maintain the position baseline, and
        // leave the deadline unarmed until the gesture ends. Clearing the
        // deadline here also disarms any deadline a mid-gesture
        // `reconfigure` armed — a stale past deadline would otherwise pin
        // `next_deadline` in the past and busy-wake the main loop at the
        // 1 ms floor for the rest of the gesture.
        if self.gesture_active {
            self.deadline = None;
            if self.hidden {
                self.hidden = false;
                return CursorHideAction::Unhide;
            }
            return CursorHideAction::None;
        }

        if activity {
            self.deadline = Some(now + self.timeout);
            if self.hidden {
                self.hidden = false;
                return CursorHideAction::Unhide;
            }
            return CursorHideAction::None;
        }

        match self.deadline {
            Some(deadline) if now >= deadline => {
                // Re-arm at the poll cadence: the machine stays hidden until
                // activity, and the poll deadline is what wakes the loop to
                // look for it. Emit `Hide` only on the visible→hidden
                // transition — an already-hidden machine re-arming past its
                // poll deadline must not re-swap the cursors.
                self.deadline = Some(now + self.poll_interval);
                if self.hidden {
                    CursorHideAction::None
                } else {
                    self.hidden = true;
                    CursorHideAction::Hide
                }
            }
            // Deadline not yet reached (or mid-gesture unarmed): nothing.
            _ => CursorHideAction::None,
        }
    }

    /// A move-size gesture (tile translate/resize) began: suspend hiding.
    ///
    /// The cursor must stay visible for the whole gesture — losing the grab
    /// point mid-drag is the failure mode this prevents. Un-hides if
    /// currently hidden; the timer restarts cleanly on
    /// [`Self::on_gesture_end`].
    pub(super) fn on_gesture_begin(&mut self) -> CursorHideAction {
        self.gesture_active = true;
        self.deadline = None;
        if self.hidden {
            self.hidden = false;
            CursorHideAction::Unhide
        } else {
            CursorHideAction::None
        }
    }

    /// The move-size gesture ended: resume normal hiding.
    ///
    /// Re-arms the inactivity timer from scratch — the gesture's mouse
    /// motion was real activity, so the full timeout elapses again before
    /// the cursor may hide.
    pub(super) fn on_gesture_end(&mut self, now: Instant) -> CursorHideAction {
        self.gesture_active = false;
        if self.enabled {
            self.deadline = Some(now + self.timeout);
        }
        CursorHideAction::None
    }

    /// A warp happened (the `warp_on_focus` invariant): it counts as mouse
    /// activity.
    ///
    /// Un-hides if hidden and restarts the inactivity timer — keyboard
    /// focus navigation always produces a visible, centered pointer that
    /// hides again after the full timeout (closing Hyprland bug #2570's
    /// lingering-visible failure mode). Note the spec's complement: when no
    /// warp occurs (pointer already inside the new window), a hidden cursor
    /// **stays** hidden — deterministic, no unhide without a warp.
    pub(super) fn on_warp(&mut self, now: Instant) -> CursorHideAction {
        if !self.enabled {
            return CursorHideAction::None;
        }
        self.deadline = Some(now + self.timeout);
        if self.hidden {
            self.hidden = false;
            CursorHideAction::Unhide
        } else {
            CursorHideAction::None
        }
    }

    /// The next instant the main loop must wake to run a poll, if any.
    ///
    /// While hidden (or suspended by a gesture) the poll cadence applies;
    /// while visible the hide deadline and the poll cadence coincide in
    /// practice (both restart together), so the poll deadline is what keeps
    /// the loop observing activity. `None` when disabled or unarmed — the
    /// caller must not schedule any polling then.
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        if !self.enabled {
            return None;
        }
        self.deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: u32 = 2000;
    const POLL: u32 = 125;

    fn scheduler() -> CursorHideScheduler {
        CursorHideScheduler::new(TIMEOUT, POLL)
    }

    fn now() -> Instant {
        Instant::now()
    }

    fn still(pos: (i32, i32)) -> ActivitySample {
        ActivitySample {
            position: pos,
            button_down: false,
        }
    }

    /// Drive the machine to the hidden state: baseline poll, then quiet
    /// polls past the timeout.
    fn drive_to_hidden(s: &mut CursorHideScheduler, t0: Instant) {
        assert_eq!(s.on_poll(still((100, 100)), t0), CursorHideAction::None);
        let quiet = t0 + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(still((100, 100)), quiet), CursorHideAction::Hide);
        assert!(s.is_hidden());
    }

    // ── effective poll interval clamping ──────────────────────────────

    /// An interval larger than the timeout is clamped to the timeout, not
    /// rejected: the spec puts no lower bound on `hide_timeout_ms`, and the
    /// spec-legal `hide_timeout_ms = 100` + default `poll_interval_ms = 125`
    /// must yield a working (once-per-timeout) cadence.
    #[test]
    fn poll_interval_above_timeout_is_clamped() {
        assert_eq!(
            effective_poll_interval(100, 125),
            Duration::from_millis(100),
            "interval clamps down to the timeout"
        );
        assert_eq!(
            effective_poll_interval(2000, 125),
            Duration::from_millis(125),
            "in-range interval passes through unchanged"
        );
        assert_eq!(
            effective_poll_interval(0, 125),
            Duration::from_millis(1),
            "disabled machine: interval clamps to the 1 ms floor (never scheduled anyway)"
        );
    }

    /// A clamped scheduler still hides on schedule: with
    /// `hide_timeout_ms = 100` and `poll_interval_ms = 125`, the effective
    /// poll runs at the 100 ms cadence and observes the elapse.
    #[test]
    fn clamped_interval_still_observes_timeout() {
        let mut s = CursorHideScheduler::new(100, 125);
        assert_eq!(s.poll_interval(), Some(Duration::from_millis(100)));
        let t0 = now();
        assert_eq!(s.on_poll(still((0, 0)), t0), CursorHideAction::None);
        assert_eq!(
            s.on_poll(still((0, 0)), t0 + Duration::from_millis(101)),
            CursorHideAction::Hide
        );
    }

    // ── timeout elapse → hidden ─────────────────────────────────────────

    #[test]
    fn timeout_elapse_hides() {
        let mut s = scheduler();
        let t0 = now();
        // First poll establishes the baseline; no activity, deadline armed.
        assert_eq!(s.on_poll(still((100, 100)), t0), CursorHideAction::None);
        assert!(!s.is_hidden());
        // Just before the deadline: still visible.
        let before = t0 + Duration::from_millis(TIMEOUT as u64 - 1);
        assert_eq!(s.on_poll(still((100, 100)), before), CursorHideAction::None);
        // Past the deadline with no activity: hide.
        let after = t0 + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(still((100, 100)), after), CursorHideAction::Hide);
        assert!(s.is_hidden());
    }

    #[test]
    fn first_poll_establishes_baseline_without_counting_as_motion() {
        let mut s = scheduler();
        let t0 = now();
        // Even polled well past the timeout, the FIRST sample must not hide
        // (no baseline to differ from, and enabling must not instantly hide).
        let late = t0 + Duration::from_secs(10);
        assert_eq!(s.on_poll(still((5, 5)), late), CursorHideAction::None);
        assert!(!s.is_hidden());
        // The timer starts from that first poll: hides at late + timeout.
        let after = late + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(still((5, 5)), after), CursorHideAction::Hide);
    }

    // ── motion → visible + timer restart ────────────────────────────────

    #[test]
    fn motion_unhides_and_restarts_timer() {
        let mut s = scheduler();
        let t0 = now();
        drive_to_hidden(&mut s, t0);
        // Motion while hidden: unhide + restart.
        let t1 = t0 + Duration::from_millis(TIMEOUT as u64 + 10);
        assert_eq!(s.on_poll(still((240, 100)), t1), CursorHideAction::Unhide);
        assert!(!s.is_hidden());
        // Timer restarted from t1: quiet poll just before t1 + timeout stays
        // visible, past it hides again.
        let before = t1 + Duration::from_millis(TIMEOUT as u64 - 1);
        assert_eq!(s.on_poll(still((240, 100)), before), CursorHideAction::None);
        assert!(!s.is_hidden());
        let after = t1 + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(still((240, 100)), after), CursorHideAction::Hide);
        assert!(s.is_hidden());
    }

    #[test]
    fn motion_while_visible_restarts_timer() {
        let mut s = scheduler();
        let t0 = now();
        s.on_poll(still((10, 10)), t0);
        // Move well inside the timeout: the timer restarts, so a quiet
        // stretch measured from the ORIGINAL t0 must not hide.
        let tm = t0 + Duration::from_millis(500);
        s.on_poll(still((20, 10)), tm);
        let t_orig_deadline = t0 + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(
            s.on_poll(still((20, 10)), t_orig_deadline),
            CursorHideAction::None,
            "timer must have restarted from the motion instant"
        );
        // ...and hides after the restarted deadline passes.
        let after = tm + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(still((20, 10)), after), CursorHideAction::Hide);
    }

    // ── button-down (no motion) → visible + timer restart ───────────────

    #[test]
    fn button_down_without_motion_unhides_and_restarts() {
        let mut s = scheduler();
        let t0 = now();
        drive_to_hidden(&mut s, t0);
        // Same position, button held: activity.
        let t1 = t0 + Duration::from_millis(TIMEOUT as u64 + 10);
        let click = ActivitySample {
            position: (100, 100),
            button_down: true,
        };
        assert_eq!(s.on_poll(click, t1), CursorHideAction::Unhide);
        assert!(!s.is_hidden());
        // Timer restarted from t1 even though the button is still held:
        // a HELD button is not new activity, only a press edge would be —
        // but per the spec any observed button-down state counts as the
        // activity sample, so the quiet stretch below (button released,
        // no motion) hides only after t1 + timeout.
        let release = ActivitySample {
            position: (100, 100),
            button_down: false,
        };
        let before = t1 + Duration::from_millis(TIMEOUT as u64 - 1);
        assert_eq!(s.on_poll(release, before), CursorHideAction::None);
        let after = t1 + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(release, after), CursorHideAction::Hide);
    }

    // ── no activity → stays hidden ──────────────────────────────────────

    #[test]
    fn no_activity_stays_hidden() {
        let mut s = scheduler();
        let t0 = now();
        drive_to_hidden(&mut s, t0);
        // Many quiet polls far past the deadline: stays hidden, no repeated
        // Hide actions (the swap is idempotent-by-construction but the
        // machine must not re-request it).
        let mut t = t0 + Duration::from_millis(TIMEOUT as u64 + POLL as u64);
        for _ in 0..10 {
            assert_eq!(s.on_poll(still((100, 100)), t), CursorHideAction::None);
            assert!(s.is_hidden());
            t += Duration::from_millis(POLL as u64);
        }
    }

    // ── timeout = 0 disabled path ───────────────────────────────────────

    #[test]
    fn zero_timeout_disables_entirely() {
        let mut s = CursorHideScheduler::new(0, POLL);
        assert!(!s.is_active());
        assert_eq!(s.next_deadline(), None);
        assert_eq!(s.poll_interval(), None);
        // Defensive: even if polled with elapsed time, nothing happens.
        let t = now();
        for i in 0..100 {
            assert_eq!(
                s.on_poll(still((i, i)), t + Duration::from_secs(i as u64)),
                CursorHideAction::None
            );
        }
        assert!(!s.is_hidden());
    }

    // ── move-size gesture suspension ────────────────────────────────────

    #[test]
    fn gesture_suspends_hiding_for_whole_gesture() {
        let mut s = scheduler();
        let t0 = now();
        drive_to_hidden(&mut s, t0);
        // Gesture begins while hidden: unhide, no deadline armed.
        assert_eq!(s.on_gesture_begin(), CursorHideAction::Unhide);
        assert!(!s.is_hidden());
        // Far-past-timeout quiet polls during the gesture: never hides.
        let mut t = t0 + Duration::from_secs(10);
        for _ in 0..5 {
            assert_eq!(s.on_poll(still((100, 100)), t), CursorHideAction::None);
            assert!(!s.is_hidden());
            t += Duration::from_secs(1);
        }
        // Gesture ends: timer resumes cleanly — hides after the full
        // timeout from the gesture end, not before.
        let tend = t;
        assert_eq!(s.on_gesture_end(tend), CursorHideAction::None);
        let before = tend + Duration::from_millis(TIMEOUT as u64 - 1);
        assert_eq!(s.on_poll(still((100, 100)), before), CursorHideAction::None);
        let after = tend + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(still((100, 100)), after), CursorHideAction::Hide);
    }

    #[test]
    fn gesture_begin_while_visible_is_noop_but_restarts_on_end() {
        let mut s = scheduler();
        let t0 = now();
        s.on_poll(still((50, 50)), t0);
        assert_eq!(s.on_gesture_begin(), CursorHideAction::None);
        // Quiet polls during the gesture never hide even past t0 + timeout.
        let t = t0 + Duration::from_secs(30);
        assert_eq!(s.on_poll(still((50, 50)), t), CursorHideAction::None);
        assert!(!s.is_hidden());
        // Gesture end re-arms: full timeout from tend.
        assert_eq!(s.on_gesture_end(t), CursorHideAction::None);
        let after = t + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(still((50, 50)), after), CursorHideAction::Hide);
    }

    // ── warp = activity ─────────────────────────────────────────────────

    #[test]
    fn warp_unhides_and_restarts_timer() {
        let mut s = scheduler();
        let t0 = now();
        drive_to_hidden(&mut s, t0);
        let t1 = t0 + Duration::from_millis(TIMEOUT as u64 + 10);
        assert_eq!(s.on_warp(t1), CursorHideAction::Unhide);
        assert!(!s.is_hidden());
        // Full timeout from the warp, not from the original arm.
        let before = t1 + Duration::from_millis(TIMEOUT as u64 - 1);
        assert_eq!(s.on_poll(still((100, 100)), before), CursorHideAction::None);
        let after = t1 + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(still((100, 100)), after), CursorHideAction::Hide);
    }

    #[test]
    fn warp_while_visible_restarts_timer() {
        let mut s = scheduler();
        let t0 = now();
        s.on_poll(still((10, 10)), t0);
        let tw = t0 + Duration::from_millis(100);
        assert_eq!(s.on_warp(tw), CursorHideAction::None);
        // The original t0 + timeout deadline must no longer fire.
        let t_orig = t0 + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(still((10, 10)), t_orig), CursorHideAction::None);
        let after = tw + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(still((10, 10)), after), CursorHideAction::Hide);
    }

    #[test]
    fn warp_during_gesture_does_not_fight_suppression() {
        // A focus change can land mid-gesture (the gesture is mouse-driven,
        // the focus change is not). The warp it triggers must not fight the
        // gesture suppression: the cursor stays visible, the warp's re-arm
        // does not survive into the gesture, and the gesture end re-arms
        // from its own instant (full timeout from tend).
        let mut s = scheduler();
        let t0 = now();
        drive_to_hidden(&mut s, t0);
        assert_eq!(s.on_gesture_begin(), CursorHideAction::Unhide);
        // Warp mid-gesture: the cursor is already visible, so no action —
        // but the warp arms a deadline that the gesture must disarm.
        let tw = t0 + Duration::from_millis(TIMEOUT as u64 + 20);
        assert_eq!(s.on_warp(tw), CursorHideAction::None);
        let mut t = tw;
        for _ in 0..5 {
            assert_eq!(s.on_poll(still((100, 100)), t), CursorHideAction::None);
            assert!(!s.is_hidden());
            t += Duration::from_secs(1);
        }
        assert_eq!(
            s.next_deadline(),
            None,
            "no deadline may stay armed mid-gesture, not even a warp's"
        );
        // Gesture end re-arms cleanly from its own instant.
        let tend = t;
        assert_eq!(s.on_gesture_end(tend), CursorHideAction::None);
        let before = tend + Duration::from_millis(TIMEOUT as u64 - 1);
        assert_eq!(s.on_poll(still((100, 100)), before), CursorHideAction::None);
        let after = tend + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(still((100, 100)), after), CursorHideAction::Hide);
    }

    #[test]
    fn warp_on_disabled_machine_is_noop() {
        let mut s = CursorHideScheduler::new(0, POLL);
        assert_eq!(s.on_warp(now()), CursorHideAction::None);
        assert!(!s.is_hidden());
    }

    // ── hot-reload transitions ──────────────────────────────────────────

    #[test]
    fn reload_disabling_unhides_and_stops_scheduling() {
        let mut s = scheduler();
        let t0 = now();
        drive_to_hidden(&mut s, t0);
        let t1 = t0 + Duration::from_millis(TIMEOUT as u64 + 10);
        assert_eq!(s.reconfigure(0, POLL, t1), CursorHideAction::Unhide);
        assert!(!s.is_active());
        assert_eq!(s.next_deadline(), None);
        assert_eq!(s.poll_interval(), None);
        // Even elapsed-time polls do nothing.
        assert_eq!(
            s.on_poll(still((100, 100)), t1 + Duration::from_secs(60)),
            CursorHideAction::None
        );
        assert!(!s.is_hidden());
    }

    #[test]
    fn reload_disabling_while_visible_is_quiet_stop() {
        let mut s = scheduler();
        let t0 = now();
        s.on_poll(still((10, 10)), t0);
        assert_eq!(
            s.reconfigure(0, POLL, t0 + Duration::from_millis(100)),
            CursorHideAction::None
        );
        assert!(!s.is_active());
        assert_eq!(s.next_deadline(), None);
    }

    #[test]
    fn reload_enabling_on_disabled_machine_arms_timer() {
        let mut s = CursorHideScheduler::new(0, POLL);
        let t0 = now();
        assert_eq!(s.reconfigure(TIMEOUT, POLL, t0), CursorHideAction::None);
        assert!(s.is_active());
        // Baseline from the next poll; hides at t0 + timeout after it.
        s.on_poll(still((1, 1)), t0 + Duration::from_millis(1));
        let after = t0 + Duration::from_millis(TIMEOUT as u64 + 2);
        assert_eq!(s.on_poll(still((1, 1)), after), CursorHideAction::Hide);
    }

    #[test]
    fn reload_resizing_timeout_while_visible_rearms_from_now() {
        let mut s = scheduler();
        let t0 = now();
        s.on_poll(still((10, 10)), t0);
        // GROW the timeout to 3000 ms at t0 + 100: the re-arm lands at
        // tr + 3000, so the ORIGINAL t0 + 2000 deadline must not fire...
        let tr = t0 + Duration::from_millis(100);
        assert_eq!(s.reconfigure(3000, POLL, tr), CursorHideAction::None);
        let t_orig = t0 + Duration::from_millis(2001);
        assert_eq!(s.on_poll(still((10, 10)), t_orig), CursorHideAction::None);
        // ...but tr + 3000 does.
        let after = tr + Duration::from_millis(3001);
        assert_eq!(s.on_poll(still((10, 10)), after), CursorHideAction::Hide);
    }

    #[test]
    fn reload_resizing_timeout_while_hidden_stays_hidden() {
        let mut s = scheduler();
        let t0 = now();
        drive_to_hidden(&mut s, t0);
        // Resize while hidden: no unhide (the user is not active), the
        // machine re-arms at the poll cadence and keeps working.
        let tr = t0 + Duration::from_millis(TIMEOUT as u64 + 10);
        assert_eq!(s.reconfigure(5000, POLL, tr), CursorHideAction::None);
        assert!(s.is_hidden());
        // Motion still unhides.
        assert_eq!(
            s.on_poll(still((77, 77)), tr + Duration::from_millis(50)),
            CursorHideAction::Unhide
        );
        assert!(!s.is_hidden());
    }

    #[test]
    fn reload_same_values_while_visible_rearms_timer() {
        let mut s = scheduler();
        let t0 = now();
        s.on_poll(still((10, 10)), t0);
        // Reload with identical knobs at t0 + 100: timer re-arms from there,
        // so the original deadline must not fire.
        let tr = t0 + Duration::from_millis(100);
        assert_eq!(s.reconfigure(TIMEOUT, POLL, tr), CursorHideAction::None);
        let t_orig = t0 + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(still((10, 10)), t_orig), CursorHideAction::None);
        let after = tr + Duration::from_millis(TIMEOUT as u64 + 1);
        assert_eq!(s.on_poll(still((10, 10)), after), CursorHideAction::Hide);
    }

    #[test]
    fn reload_during_gesture_does_not_pin_a_past_deadline() {
        // A mid-gesture reconfigure arms the visible-path deadline; the
        // gesture branch of on_poll must disarm it, or `next_deadline`
        // would forever report a past instant and the main loop would
        // busy-wake at the 1 ms floor until the gesture ends.
        let mut s = scheduler();
        let t0 = now();
        s.on_poll(still((10, 10)), t0);
        assert_eq!(s.on_gesture_begin(), CursorHideAction::None);
        // Reload mid-gesture arms a deadline (visible path)...
        let tr = t0 + Duration::from_millis(100);
        assert_eq!(s.reconfigure(TIMEOUT, POLL, tr), CursorHideAction::None);
        // ...but a poll during the gesture (even past that deadline) clears
        // it and never hides.
        let past = tr + Duration::from_secs(5);
        assert_eq!(s.on_poll(still((10, 10)), past), CursorHideAction::None);
        assert!(!s.is_hidden());
        assert_eq!(
            s.next_deadline(),
            None,
            "no deadline may stay armed mid-gesture"
        );
        // Gesture end re-arms cleanly from its own instant.
        let tend = past + Duration::from_millis(10);
        assert_eq!(s.on_gesture_end(tend), CursorHideAction::None);
        assert_eq!(
            s.next_deadline(),
            Some(tend + Duration::from_millis(TIMEOUT as u64))
        );
    }

    // ── deadline surface ────────────────────────────────────────────────

    #[test]
    fn next_deadline_tracks_timer() {
        let mut s = scheduler();
        assert_eq!(s.next_deadline(), None, "no deadline before first poll");
        let t0 = now();
        s.on_poll(still((0, 0)), t0);
        assert_eq!(
            s.next_deadline(),
            Some(t0 + Duration::from_millis(TIMEOUT as u64))
        );
        // After hiding, the deadline rolls to the poll cadence.
        let th = t0 + Duration::from_millis(TIMEOUT as u64 + 1);
        s.on_poll(still((0, 0)), th);
        assert_eq!(
            s.next_deadline(),
            Some(th + Duration::from_millis(POLL as u64))
        );
    }
}
