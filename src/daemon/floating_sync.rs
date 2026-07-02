//! Floating-window location synchronization.
//!
//! Handles `EVENT_OBJECT_LOCATIONCHANGE` for active-workspace float windows:
//! reads the window's current screen rect, translates it back into
//! [`FloatingSpace`]'s visible-rect space, and writes it into both the active
//! workspace's [`FloatingSpace`] and the registry's
//! [`FloatingState::Active`](crate::registry::types::FloatingState::Active)
//! mirror.
//!
//! See (`docs/src/dev-guide/floating-space.md`) for why both copies are kept in
//! sync and how the timer suppression avoids capturing flow's own animation
//! moves as user drags.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::common::WindowId;
use crate::registry::hooks::{
    float_hwnds_snapshot, float_tracking_active, is_float_hwnd, set_float_tracking_active,
};
use crate::registry::types::{FloatingState, WindowState};
use crate::registry::win32 as registry_win32;

use super::borders::float_border_rect;
use super::types::FlowWM;

impl FlowWM {
    /// Apply a location-change notification for a floating window.
    ///
    /// Re-reads the window's rect via `GetWindowRect`, converts it from
    /// window-rect space to visible-rect space, and upserts it into the active
    /// workspace's [`FloatingSpace`] and the registry's
    /// [`FloatingState::Active`](crate::registry::types::FloatingState::Active).
    ///
    /// The hook callback already filters by `FLOAT_HWNS` membership and
    /// `FLOAT_TRACKING_ACTIVE`; this handler re-checks both on the main thread
    /// to close the race between the callback forwarding an event and the
    /// daemon flipping tracking off (flow float animation) or removing the
    /// window from the float set (tile/destroy/switch).
    pub(super) fn on_float_location_changed(&mut self, hwnd: isize) {
        // Race-closing re-check #1: tracking may have been turned off after
        // the callback forwarded this event.
        if !float_tracking_active() {
            return;
        }
        // Race-closing re-check #2: the window may have left the float set
        // (tiled / destroyed / workspace switched away) in the interim.
        if !is_float_hwnd(hwnd) {
            return;
        }
        self.store_float_rect(hwnd);
    }

    /// Read `hwnd`'s current on-screen rect and store it as the window's
    /// float position, in both the active workspace's [`FloatingSpace`] and
    /// the registry's [`FloatingState::Active`] mirror.
    ///
    /// Unlike [`on_float_location_changed`](Self::on_float_location_changed),
    /// this does NOT check `FLOAT_TRACKING_ACTIVE` — the caller gates that.
    /// The resume poll uses this entry point so it can store rects while the
    /// flag is intentionally off (poll-then-enable ordering).
    ///
    /// Skips silently if `hwnd` is untracked, not an active float, or not in
    /// the active workspace's `FloatingSpace`.
    fn store_float_rect(&mut self, hwnd: isize) {
        let hwnd_handle = HWND(hwnd as *mut _);

        // Read the window-rect (full, including invisible borders).
        let window_rect = match registry_win32::get_window_rect(hwnd_handle) {
            Ok(r) => r,
            Err(e) => {
                log::debug!("float location: GetWindowRect failed for {hwnd}: {e}");
                return;
            }
        };

        // Bounds + state come from the registry. A non-Active float (minimized
        // or hidden) is skipped so its stored rect isn't clobbered by a stale
        // GetWindowRect reading.
        let Some(window) = self.registry.get_window(hwnd_handle) else {
            return;
        };
        if !matches!(
            window.state,
            WindowState::Floating(FloatingState::Active { .. })
        ) {
            return;
        }
        let invisible_bounds = window.invisible_bounds;

        // Convert window-rect → visible-rect (the space FloatingSpace stores).
        let visible_rect = invisible_bounds.window_to_visible(window_rect);
        let wid = WindowId(hwnd);

        // Seat the overlay at the visible rect outset by `(thickness − overlap)`
        // — the float counterpart of the tile side's content inset — so the
        // ring lands in the surrounding gap like a tiled border. The formula is
        // shared with the creation path via `float_border_rect`, keeping the two
        // paths visually identical.
        let border_rect = float_border_rect(
            visible_rect,
            self.config.borders.thickness,
            self.config.borders.overlap,
        );

        // FLOAT_HWNS holds only active-workspace floats, so the window must be
        // in the active workspace's FloatingSpace. Guard the upsert with
        // contains() so a stray event can never append a bogus entry.
        if !self.active_workspace().floating.contains(wid) {
            return;
        }
        self.active_workspace_mut().floating.add(wid, visible_rect);

        // Mirror into the registry so the two copies never drift.
        if let Some(win) = self.registry.get_window_mut(hwnd_handle) {
            win.state = WindowState::Floating(FloatingState::Active { rect: visible_rect });
            // Update the border overlay to match the new float position.
            // The border follows the window's real OS-reported rect (via
            // the LOCATIONCHANGE hook), not the layout engine's commanded
            // rect — this is the float-side counterpart to the animator's
            // tile-side border flattening. set_geometry takes &self
            // (interior mutability) so this is sound through &mut Window.
            if let Some(border) = win.border.as_ref() {
                border.set_geometry(border_rect);
            }
        }

        log::trace!(
            "float location: hwnd={hwnd} stored visible rect ({},{},{},{})",
            visible_rect.x,
            visible_rect.y,
            visible_rect.width,
            visible_rect.height
        );
    }

    /// Suppress float-location tracking for the duration of one flow-initiated
    /// float animation.
    ///
    /// Turns `FLOAT_TRACKING_ACTIVE` off and arms `float_resume_deadline` to
    /// `now + animation duration + epsilon`. Resetting the deadline on each
    /// call means overlapping animations extend the suppression rather than
    /// letting an earlier timer expire mid-flight.
    ///
    /// Callers must ensure this only runs when animation is enabled: a 0 ms
    /// instant snap fires LOCATIONCHANGE at the final rect, so no suppression
    /// is needed.
    pub(super) fn arm_float_tracking_suppression(&mut self) {
        set_float_tracking_active(false);
        // Epsilon: one DwmFlush frame (~16 ms) of slack so the resume fires
        // just after the animator's last frame, not before it.
        const RESUME_EPSILON_MS: u64 = 16;
        let duration_ms = u64::from(self.config.animation.duration_ms);
        self.float_resume_deadline =
            Some(Instant::now() + Duration::from_millis(duration_ms + RESUME_EPSILON_MS));
        log::debug!(
            "float tracking suppressed: resume in {} ms ({} ms duration + {} ms epsilon)",
            duration_ms + RESUME_EPSILON_MS,
            duration_ms,
            RESUME_EPSILON_MS,
        );
    }

    /// Resume float-location tracking if the suppression deadline has passed.
    ///
    /// Polls every tracked float's true on-screen rect and stores it, then
    /// re-enables `FLOAT_TRACKING_ACTIVE`. Polling runs while the flag is
    /// still off, so no LOCATIONCHANGE can race the poll — the poll-then-
    /// enable ordering the design requires.
    ///
    /// No-op when no suppression is armed or the deadline hasn't arrived.
    pub(super) fn maybe_resume_float_tracking(&mut self) {
        let Some(deadline) = self.float_resume_deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }

        let hwnds = float_hwnds_snapshot();
        log::debug!(
            "float tracking resume: polling {} float window(s) before re-enabling",
            hwnds.len(),
        );
        for hwnd in hwnds {
            self.store_float_rect(hwnd);
        }

        set_float_tracking_active(true);
        self.float_resume_deadline = None;
        log::debug!("float tracking resumed");
    }
}
