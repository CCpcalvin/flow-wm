//! Graceful-shutdown window rescue.
//!
//! When the daemon stops, windows stranded off-screen (parked in non-active
//! workspaces at ±1 monitor height, or scrolled into the packing region)
//! would become unreachable. This module brings them back into the active
//! monitor's [`work_area`](crate::workspace::Monitor::work_area) without
//! disturbing windows the user can currently see.
//!
//! See `docs/src/dev-guide/ipc-and-watchdog.md` for the design rationale
//! (why visible windows are left untouched, and the "pre-this-run" contract
//! on the rescue anchor).

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_NOTOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
};

use super::types::FlowWM;
use crate::common::{InvisibleBounds, Rect};
use crate::registry::types::WindowState;
use crate::registry::win32;

impl FlowWM {
    /// Rescue windows stranded off-screen back into the active monitor's
    /// `work_area`. Called once during graceful shutdown, after the event
    /// loop exits and before the daemon process tears down.
    ///
    /// For each window flow controls ([`restorable_windows`][crate::registry::WindowRegistry::restorable_windows]):
    ///
    /// 1. Ask Win32 for the window's current rect (`GetWindowRect`) — ground
    ///    truth from the OS, not workspace bookkeeping.
    /// 2. If its visible content (window rect minus invisible borders) overlaps
    ///    the active monitor's `work_area`, leave it alone: the user can see it,
    ///    and touching it would disturb their in-progress arrangement.
    /// 3. Otherwise, move it to its `pre_manage_rect` anchor, clamped into the
    ///    `work_area` if the anchor is itself off-screen.
    ///
    /// `Ignored`, `Minimized`, and `Hidden` windows never reach this loop — the
    /// registry accessor filters them out, since flow does not actively place
    /// them.
    ///
    /// Failures (window vanished mid-loop, `SetWindowPos` rejected) are logged
    /// at `warn` and skipped: one bad window must not abort the rescue pass.
    pub fn rescue_stranded_windows(&self) {
        let work_area = self.active_monitor().work_area();
        let mut rescued = 0_usize;

        for (hwnd_val, anchor, invisible_bounds) in self.registry.restorable_windows() {
            let hwnd = HWND(hwnd_val as *mut _);
            // Ground truth: where does Win32 say the window is right now?
            let Ok(current) = win32::get_window_rect(hwnd) else {
                log::warn!("shutdown rescue: GetWindowRect failed for hwnd {hwnd_val}, skipping");
                continue;
            };
            // Visible on the active monitor → leave it where the user put it.
            if content_overlaps_work_area(current, invisible_bounds, work_area) {
                continue;
            }
            // Stranded — bring it back to its pre-flow anchor, clamped on-screen.
            let target = anchor.clamped_into(work_area);
            if win32::set_window_rect(hwnd_val, target.x, target.y, target.width, target.height) {
                rescued += 1;
            }
        }

        log::info!("shutdown rescue: brought {rescued} stranded window(s) back on-screen");
    }

    /// Drop every flow-managed float window from the `WS_EX_TOPMOST` band.
    ///
    /// flow pins the float layer `WS_EX_TOPMOST` while a flow-managed window
    /// holds the foreground. The float HWNDs belong to other processes
    /// (browsers, editors), so their `WS_EX_TOPMOST` flag outlives the daemon
    /// — without this pass those windows would stay pinned above everything
    /// after flow stops. Iterates the whole registry (every workspace, not
    /// just the active one) so parked-workspace floats are covered too.
    /// (`docs/src/dev-guide/floating-space.md`)
    ///
    /// Border overlays are owned by the daemon process and are destroyed when
    /// it tears down, so they need no explicit cleanup here.
    ///
    /// Failures (window vanished mid-loop, `SetWindowPos` rejected) are
    /// counted out but never abort the pass.
    pub fn release_float_topmost(&self) {
        let mut released = 0_usize;
        for window in self.registry.windows() {
            if !matches!(window.state, WindowState::Floating(_)) {
                continue;
            }
            // SAFETY: `SetWindowPos` with NOMOVE|NOSIZE|NOACTIVATE toggles only
            // the WS_EX_TOPMOST bit. HWND_NOTOPMOST is a sentinel HWND value
            // (not a real window). The float HWND is a live foreign window;
            // an access-denied failure is logged-and-counted, never fatal.
            let result = unsafe {
                SetWindowPos(window.hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE)
            };
            if result.is_ok() {
                released += 1;
            }
        }
        log::info!("shutdown: released {released} float window(s) from topmost");
    }
}

/// True if the window's **visible content** overlaps `work_area`.
///
/// Returns `false` (never panics) even for degenerate inputs. See
/// `docs/src/dev-guide/ipc-and-watchdog.md` for why the visible-content rect —
/// not the raw `GetWindowRect` rect — is the visibility test.
fn content_overlaps_work_area(window_rect: Rect, bounds: InvisibleBounds, work_area: Rect) -> bool {
    // GetWindowRect includes invisible borders, so a window parked flush at the
    // work-area edge bleeds a few pixels into it. Stripping the borders first
    // recovers the visible rect; parked windows then only touch (not overlap)
    // the edge and are correctly treated as stranded.
    bounds.window_to_visible(window_rect).overlaps(work_area)
}

#[cfg(test)]
mod tests {
    use crate::common::{InvisibleBounds, Rect};

    use super::content_overlaps_work_area;

    // Typical Win11 invisible borders.
    const BOUNDS: InvisibleBounds = InvisibleBounds {
        left: 7,
        top: 0,
        right: 7,
        bottom: 7,
    };
    // 1920x1040 work area (taskbar excluded).
    const WORK_AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1040,
    };

    #[test]
    fn parked_right_window_is_not_visible_despite_border_bleed() {
        // Visible left edge at x=1920; GetWindowRect reports x=1913 due to the
        // 7px left invisible border, so the raw rect overlaps the work area.
        // The visible-content rect only touches the edge → stranded → rescue.
        let window_rect = Rect {
            x: 1913,
            y: 0,
            width: 974, // visible 960 + 7 + 7
            height: 607,
        };
        assert!(!content_overlaps_work_area(window_rect, BOUNDS, WORK_AREA));
    }

    #[test]
    fn parked_left_window_is_not_visible_despite_border_bleed() {
        // Mirror of the right case: visible right edge at x=0, window right edge
        // bleeds to x=7. Visible-content rect only touches → stranded → rescue.
        let window_rect = Rect {
            x: -967,
            y: 0,
            width: 974,
            height: 607,
        };
        assert!(!content_overlaps_work_area(window_rect, BOUNDS, WORK_AREA));
    }

    #[test]
    fn genuinely_on_screen_window_is_visible() {
        // A window squarely inside the work area stays visible → no rescue.
        let visible = Rect {
            x: 100,
            y: 100,
            width: 800,
            height: 600,
        };
        let window_rect = BOUNDS.visible_to_window(visible);
        assert!(content_overlaps_work_area(window_rect, BOUNDS, WORK_AREA));
    }

    #[test]
    fn parked_flush_with_zero_bounds_is_not_visible() {
        // Borderless windows have no bleed; the raw rect already only touches
        // the edge. Either way they must be rescued.
        let window_rect = Rect {
            x: 1920,
            y: 0,
            width: 960,
            height: 600,
        };
        assert!(!content_overlaps_work_area(
            window_rect,
            InvisibleBounds::zero(),
            WORK_AREA,
        ));
    }

    #[test]
    fn inter_workspace_vertically_parked_window_is_not_visible() {
        // Inter-workspace windows are parked a full monitor-height above/below
        // the active work_area. These were already rescued correctly before
        // the border-bleed fix (the bleed is irrelevant at ±1040px); this pins
        // that the fix did not regress that path. It is also the only test
        // here that forces the y-axis conjunct of `overlaps` false — every
        // other test keeps the visible-y range inside the work_area, so the
        // decision always came down to the x-axis.
        let visible_above = Rect {
            x: 0,
            y: -1040,
            width: 960,
            height: 600,
        };
        let window_rect = BOUNDS.visible_to_window(visible_above);

        assert!(!content_overlaps_work_area(window_rect, BOUNDS, WORK_AREA));
    }
}
