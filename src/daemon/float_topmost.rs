//! TOPMOST-toggle wiring for the float layer.
//!
//! Translates the pure [`crate::float_topmost`] decision into Win32
//! `SetWindowPos` calls. The daemon evaluates [`FlowWM::reconcile_float_topmost`]
//! in the single focus sink ([`FlowWM::on_focus_changed`](super::FlowWM::on_focus_changed))
//! on every foreground change: floats stay `WS_EX_TOPMOST` while the foreground
//! is a flow-managed non-fullscreen window, and drop to non-topmost the moment
//! the foreground moves to a fullscreen app or any non-flow window.
//!
//! `WS_EX_TOPMOST` is not set-and-forget — it drifts when other apps grab
//! topmost or events reset the flag — so the TOPMOST-target path does not trust
//! the [`floats_topmost`](super::FlowWM::floats_topmost) cache. It re-asserts
//! against each float's **observed** real flag (the [`FlowWM::reassert_floats_topmost`]
//! pass) and re-applies only what was actually lost. See
//! (`docs/src/dev-guide/floating-space.md`).

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
    /// Reads the foreground's two live facts — is it a window flow actively
    /// manages, and is it currently fullscreen — runs the pure
    /// [`crate::float_topmost`] decision, and applies the resulting
    /// [`TopmostAction`] to the active-workspace floats. The TOPMOST-target
    /// outcomes (`Pin`, and the topmost `NoOp`) re-assert against each float's
    /// **observed** real flag via [`Self::reassert_floats_topmost`] rather than
    /// trusting the cache, so Z-order drift is corrected; the non-topmost
    /// `NoOp` is a true no-op and the `Drop` drops the floats. Called from the
    /// focus sink on every foreground change.
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

    /// Re-apply `WS_EX_TOPMOST` to floats that lost it.
    ///
    /// The drift guard for the TOPMOST-target path. Reads each float's live
    /// `WS_EX_TOPMOST` flag via [`registry_win32::is_topmost`], runs the pure
    /// [`crate::float_topmost::reassert_float_topmost`] decision, and re-applies
    /// `SetWindowPos(HWND_TOPMOST)` **only** to the floats whose real flag was
    /// actually lost — so the common (no-drift) case does no `SetWindowPos`, and
    /// an aligned sibling is never disturbed.
    ///
    /// Converges in a single pass with no busy-loop: `SetWindowPos` is issued
    /// with `SWP_NOACTIVATE`, so it does not change the foreground and cannot
    /// re-enter [`Self::reconcile_float_topmost`] recursively; and after a
    /// re-apply the drifted floats read topmost again, so a re-evaluation is a
    /// no-op.
    fn reassert_floats_topmost(&self, floats: &[isize]) {
        // Read each float's REAL flag — reality, not the cache. `is_topmost`
        // fail-opens to `false`, which only makes the re-assertion (re-)apply
        // the flag: idempotent and harmless.
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
