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
    HWND_BOTTOM, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SetWindowPos,
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
        // Capture the full geometry so the decision can be logged — the
        // `rect == screen` equality is fragile to ±px invisible-frame offsets
        // (`docs/adr/0003-float-tile-stacking-invariant.md`), and the rects are
        // the diagnostic for any "why didn't floats drop?" case.
        let geometry = registry_win32::fullscreen_geometry(HWND(foreground as *mut _));
        let is_fullscreen = geometry.as_ref().map(|g| g.fullscreen).unwrap_or(false);
        let foreground_kind = float_topmost::classify_foreground(is_flow_managed, is_fullscreen);

        let action = float_topmost::decide_float_topmost(FloatTopmostSnapshot {
            foreground: foreground_kind,
            has_floats: true,
            currently_topmost: self.floats_topmost,
        });

        // Diagnostic for the fullscreen trigger (HTML5 button vs F11,
        // multi-monitor, DPI): the rects, the verdict, and the resulting action,
        // captured on every reconcile so a replayed `debug.log` answers the
        // "why didn't floats drop on fullscreen?" question without re-probing.
        let was_topmost = self.floats_topmost;
        match &geometry {
            Ok(g) => {
                let (w, s) = (g.window_rect, g.screen_rect);
                log::debug!(
                    "float-topmost reconcile: fg={foreground:#x} flow_managed={is_flow_managed} \
                     maximized={} fullscreen={} window=({},{},{}x{}) screen=({},{},{}x{}) \
                     action={action:?} was_topmost={was_topmost}",
                    g.maximized,
                    g.fullscreen,
                    w.x,
                    w.y,
                    w.width,
                    w.height,
                    s.x,
                    s.y,
                    s.width,
                    s.height,
                );
            }
            Err(e) => log::debug!(
                "float-topmost reconcile: fg={foreground:#x} flow_managed={is_flow_managed} \
                 fullscreen=false (geo error: {e}) action={action:?} was_topmost={was_topmost}",
            ),
        }

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
                // How to lower depends on the foreground kind (ADR-0007). A
                // fullscreen foreground bottoms every float (and its border)
                // so the fullscreen app covers them; a non-flow foreground is
                // re-raised above the demoted floats, yielding
                // `foreground > float > tiles` without sinking the floats.
                match float_topmost::decide_drop_mechanism(foreground_kind) {
                    float_topmost::DropMechanism::ToBottom => {
                        self.drop_floats_to_bottom(&floats);
                    }
                    float_topmost::DropMechanism::ReRaiseForeground => {
                        self.drop_floats_below_foreground(&floats, foreground);
                    }
                }
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

    /// Drop floats **below a non-flow foreground** that may not cover the
    /// screen — ADR-0007's non-flow-case mechanism. Demotes the floats (and
    /// borders in lockstep via [`Self::set_floats_topmost`]), re-seats each
    /// border above its demoted float, then re-raises the foreign foreground
    /// to the top of its Z-order band, leaving `foreground > float > tiles`.
    ///
    /// (`docs/adr/0007-drop-lowers-floats-below-foreground.md`)
    fn drop_floats_below_foreground(&self, floats: &[isize], foreground: isize) {
        // Demote every float (and border overlay) to non-topmost. The re-raise
        // below is what leaves the floats beneath the foreground — clearing
        // TOPMOST alone is insufficient (ADR-0007).
        self.set_floats_topmost(floats, false);
        // Re-seat each border overlay just above its demoted float, mirroring
        // `drop_floats_to_bottom` so the resize ring yields with its float.
        for &hwnd_val in floats {
            let hwnd = HWND(hwnd_val as *mut _);
            if let Some(window) = self.registry.get_window(hwnd)
                && let Some(border) = window.border.as_ref()
            {
                border.seat_above_target();
            }
        }
        // Re-raise the foreign foreground to the top of its Z-order band — the
        // only way to leave the demoted float beneath it (there is no
        // seat-below-window-X primitive; `SetWindowPos(A, B)` seats A above B).
        let fg_hwnd = HWND(foreground as *mut _);
        let flags = SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE;
        // SAFETY: `SetWindowPos` on a foreground flow may not own.
        // NOACTIVATE|NOMOVE|NOSIZE restricts the effect to Z-order. `HWND_TOP`
        // is a sentinel HWND (value 0) seating the window at the top of its
        // band. An elevated foreground (Task Manager, `uiAccess`, UWP) denies
        // with `ERROR_ACCESS_DENIED` — logged and skipped, never fatal; the
        // float stays above such an app (ADR-0007's accepted UIPI wall).
        if let Err(e) = unsafe { SetWindowPos(fg_hwnd, Some(HWND_TOP), 0, 0, 0, 0, flags) } {
            log::warn!("foreground re-raise failed for hwnd {foreground}: {e}");
        }
    }

    /// Drop floats to the **bottom** of the Z-order so a non-topmost
    /// fullscreen foreground (HTML5 video button, F11, borderless game) covers
    /// them entirely — ADR-0007's fullscreen-case mechanism.
    ///
    /// Demotes via [`Self::set_floats_topmost`] (which also demotes each
    /// border overlay in lockstep), then sends each float to `HWND_BOTTOM` and
    /// re-seats its border just above it via
    /// [`Border::seat_above_target`](crate::borders::Border::seat_above_target).
    /// Only windows flow owns are repositioned — the foreign fullscreen app is
    /// never touched.
    ///
    /// (`docs/adr/0007-drop-lowers-floats-below-foreground.md`)
    fn drop_floats_to_bottom(&self, floats: &[isize]) {
        // Demote every float (and each float's border overlay) to non-topmost
        // via the shared loop. This clears `WS_EX_TOPMOST` but leaves floats at
        // the top of the non-topmost band — insufficient on its own.
        self.set_floats_topmost(floats, false);
        // Then send each float to HWND_BOTTOM so a non-topmost fullscreen
        // foreground covers it. The border was demoted above; re-seat it just
        // above the now-bottomed float so the resize ring hides with its float
        // rather than dangling over the fullscreen app.
        let flags = SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE;
        for &hwnd_val in floats {
            let hwnd = HWND(hwnd_val as *mut _);
            // SAFETY: `SetWindowPos` on a flow-owned float. NOMOVE|NOSIZE|
            // NOACTIVATE touches only z-order. HWND_BOTTOM is a sentinel HWND
            // (value 1, not a real window) that places the window at the
            // bottom of the Z-order. An elevated float (foreign uiAccess)
            // denies with ERROR_ACCESS_DENIED — logged and skipped, never
            // fatal. The foreign fullscreen app is never named here.
            if let Err(e) = unsafe { SetWindowPos(hwnd, Some(HWND_BOTTOM), 0, 0, 0, 0, flags) } {
                log::warn!("float bottom drop failed for hwnd {hwnd_val}: {e}");
            }
            // Re-seat the border overlay just above its (now-bottomed) float.
            // `set_topmost(false)` already cleared the overlay's WS_EX_TOPMOST;
            // `seat_above_target` now places it just above the float in z-order,
            // both below the fullscreen foreground (ADR-0007).
            if let Some(window) = self.registry.get_window(hwnd)
                && let Some(border) = window.border.as_ref()
            {
                border.seat_above_target();
            }
        }
    }

    /// Toggle every HWND in `floats` to topmost (`topmost == true`) or
    /// non-topmost.
    ///
    /// A single `SetWindowPos(HWND_TOPMOST | HWND_NOTOPMOST)` per float with
    /// `NOMOVE | NOSIZE | NOACTIVATE` touches only the `WS_EX_TOPMOST` bit,
    /// preserving each float's position and not stealing focus.
    pub(super) fn set_floats_topmost(&self, floats: &[isize], topmost: bool) {
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
            // Toggle the float's border overlay in lockstep. `seat_above_target`
            // promotes the overlay into the topmost band while the float is
            // pinned, so dropping the float must demote the overlay too —
            // otherwise it would stay topmost and render above a fullscreen /
            // non-flow window. Tiled borders are otherwise never topmost; the
            // one non-float caller is the float→tile toggle, which drops a
            // departing float's pin here before it becomes a tile.
            // (`docs/src/dev-guide/floating-space.md`)
            if let Some(window) = self.registry.get_window(hwnd)
                && let Some(border) = window.border.as_ref()
            {
                border.set_topmost(topmost);
            }
        }
    }
}
