//! TOPMOST-toggle wiring for the float layer.
//!
//! Translates the pure [`crate::float_topmost`] decision into Win32
//! `SetWindowPos` calls, evaluated in the focus sink
//! ([`FlowWM::on_focus_changed`](super::FlowWM::on_focus_changed)).
//! (`docs/src/dev-guide/floating-space.md`)

use crate::common::WindowId;
use crate::float_topmost::{self, FloatObserved, FloatTopmostSnapshot, TopmostAction};
use crate::registry::hooks::float_hwnds_snapshot;
use crate::registry::win32 as registry_win32;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
};

use super::types::FlowWM;

impl FlowWM {
    /// Re-evaluate the float layer's TOPMOST state for a new `foreground`.
    ///
    /// Re-evaluate the float layer's TOPMOST state for `foreground`.
    ///
    /// Runs the pure [`crate::float_topmost`] decision and applies the resulting
    /// [`TopmostAction`] to the active-workspace floats; TOPMOST-target outcomes
    /// re-assert via [`Self::reassert_floats_topmost`].
    pub(super) fn reconcile_float_topmost(&mut self, foreground: isize) {
        // No floats on the active workspace ⇒ nothing to toggle and no work
        // (the no-overhead no-op). The fast path is a pure field read that
        // avoids touching the shared float-HWND set.
        if self.active_floating().is_empty() {
            return;
        }
        let floats = float_hwnds_snapshot();
        if floats.is_empty() {
            return;
        }

        // "Flow owns the foreground": actively tiling OR an active float on the
        // active workspace. `is_tracked` would also count `Ignored` windows
        // (maximized / fullscreen), which must drop the floats — so test the
        // managed states directly.
        let is_flow_managed = self.registry.is_tiling(foreground)
            || self.active_floating().contains(WindowId(foreground));

        // LIVE fullscreen check on the foreground (geometry + style), never the
        // stored registry classification. Fail-open (not fullscreen) on a Win32
        // error so a destroyed/transient foreground does not spuriously drop.
        let is_fullscreen =
            registry_win32::is_fullscreen(HWND(foreground as *mut _)).unwrap_or(false);

        let action = float_topmost::decide_float_topmost(FloatTopmostSnapshot {
            foreground: float_topmost::classify_foreground(is_flow_managed, is_fullscreen),
            has_floats: true,
            currently_topmost: self.floats_topmost,
        });

        match action {
            // Desired state already holds per the cache. The non-topmost case
            // is a genuine no-op (floats already dropped). But the topmost
            // `NoOp` cannot see drift — another app may have cleared a float's
            // real flag while the cache still believes it is set — so re-assert
            // against observed reality there. Reality, not the cache.
            TopmostAction::NoOp if self.floats_topmost => {
                self.reassert_floats_topmost(&floats);
            }
            // Target is topmost but the cache said otherwise: pin via the
            // reality-based pass too, so already-topmost floats are skipped and
            // only the ones that actually read non-topmost are re-applied.
            TopmostAction::Pin => {
                self.reassert_floats_topmost(&floats);
                self.floats_topmost = true;
            }
            TopmostAction::NoOp => {}
            TopmostAction::Drop => {
                self.set_floats_topmost(&floats, false);
                self.floats_topmost = false;
            }
        }
    }

    /// Re-apply `WS_EX_TOPMOST` to floats whose live flag was lost.
    ///
    /// Uses [`registry_win32::is_topmost`] and the pure
    /// [`crate::float_topmost::reassert_float_topmost`] decision.
    fn reassert_floats_topmost(&self, floats: &[isize]) {
        // `is_topmost` fail-opens to false, making re-assertion re-apply (idempotent).
        let observed: Vec<FloatObserved> = floats
            .iter()
            .map(|&id| FloatObserved {
                id,
                observed_topmost: registry_win32::is_topmost(HWND(id as *mut _)),
            })
            .collect();
        let drifted = float_topmost::reassert_float_topmost(true, &observed);
        if drifted.is_empty() {
            return;
        }
        // Re-pin only the drifted floats, reusing the shared SetWindowPos loop.
        self.set_floats_topmost(&drifted, true);
    }

    /// Toggle every float HWND to topmost (`topmost == true`) or non-topmost.
    ///
    /// A single `SetWindowPos(HWND_TOPMOST | HWND_NOTOPMOST)` per float with
    /// `NOMOVE | NOSIZE | NOACTIVATE` touches only the `WS_EX_TOPMOST` bit,
    /// preserving each float's position and not stealing focus.
    fn set_floats_topmost(&self, floats: &[isize], topmost: bool) {
        let insert_after = if topmost {
            Some(HWND_TOPMOST)
        } else {
            Some(HWND_NOTOPMOST)
        };
        let flags = SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE;
        for &hwnd_val in floats {
            let hwnd = HWND(hwnd_val as *mut _);
            // SAFETY: `SetWindowPos` with NOMOVE|NOSIZE|NOACTIVATE toggles only
            // the WS_EX_TOPMOST bit. HWND_TOPMOST / HWND_NOTOPMOST are special
            // sentinel HWND values (not real windows). Accessing a foreign
            // higher-integrity window (elevated / uiAccess / UWP) fails with
            // ERROR_ACCESS_DENIED — logged and skipped, never fatal.
            if let Err(e) = unsafe { SetWindowPos(hwnd, insert_after, 0, 0, 0, 0, flags) } {
                log::warn!("float topmost toggle failed for hwnd {hwnd_val}: {e}");
            }
        }
    }
}
