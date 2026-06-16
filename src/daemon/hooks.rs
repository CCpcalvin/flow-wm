//! Win32 hook event handlers.
//!
//! This module contains individual handlers for each type of hook event:
//!
//! - [`on_window_created`] — handles new window creation
//! - [`on_window_destroyed`] — handles window destruction
//! - [`on_window_minimized`] — handles window minimize events
//! - [`on_window_restored`] — handles window restore (un-minimize) events
//! - [`on_focus_changed`] — handles focus changes
//!
//! # Window-removal pipeline
//!
//! Both [`on_window_destroyed`] and [`on_window_minimized`] share the
//! [`remove_from_layout_and_refocus`] helper, which implements the full
//! removal pipeline: remove from the virtual layout → push OS-level focus to
//! the successor window if focus changed → animate the resulting layout diff.
//! The successor window is chosen by [`LayoutEngine::remove_window`] via
//! [`mutations::next_available_window`] (left column, then right).

use crate::common::WindowId;
use crate::registry::win32 as registry_win32;

use super::types::ScrollTilingManager;

impl ScrollTilingManager {
    /// Handle a window creation event.
    ///
    /// Pipeline:
    /// 1. `registry.handle_created(hwnd)` — classifies and registers the window.
    /// 2. If the window was classified as tiling (`Some(WindowId)`):
    ///    - `layout.insert_window(id)` — places the new column immediately
    ///      after the focused window, shifts right-side columns rightward by
    ///      one `column_shift`, moves focus to the new window, and ensures it
    ///      is visible.
    ///    - `animate_layout(applied)` — animates the resulting layout change.
    /// 3. If the window was floating, ignored, or skipped: no action needed.
    ///
    /// # Return value
    ///
    /// Returns `true` if [`handle_created`](crate::registry::WindowRegistry::handle_created)
    /// processed the window (classified it as tiling, floating, or ignored).
    /// Returns `false` if classification **failed** — the window is not yet
    /// ready (not visible, no title, styles not finalized). The caller should
    /// add the hwnd to the pending-creations retry list when this returns
    /// `false`.
    ///
    /// # Why classification can fail
    ///
    /// `EVENT_OBJECT_CREATE` fires early in the Win32 window lifecycle —
    /// before `ShowWindow`, `SetWindowText`, or style finalization. The
    /// classification checks (`is_window_visible`, title non-empty,
    /// `is_alt_tab_visible`) all fail on a not-yet-shown window. A subsequent
    /// retry (after `EVENT_SYSTEM_FOREGROUND` or other events arrive) will
    /// typically succeed.
    ///
    /// # Placement strategy
    ///
    /// Unlike [`on_window_restored`](Self::on_window_restored) which re-adds a
    /// previously-minimized window at the far right via `add_window`, new
    /// windows are inserted next to the focused window so they appear where
    /// the user is actively working. See
    /// [`LayoutEngine::insert_window`](crate::layout::LayoutEngine::insert_window)
    /// for the full algorithm.
    pub(super) fn on_window_created(&mut self, hwnd: isize) -> bool {
        if let Some(window_id) = self.registry.handle_created(hwnd) {
            let applied = self.layout.insert_window(window_id);
            self.animate_layout(&applied);
            true
        } else {
            // Classification failed — either the window isn't ready yet
            // (not visible, no title) or it was classified as floating/
            // ignored (already registered). The caller adds the hwnd to
            // the pending-creations retry list. For already-registered
            // windows, handle_created returns None immediately on retry
            // (line 730 check), so the retry is cheap and the window is
            // dropped after the retry limit — harmless.
            false
        }
    }

    /// Handle a window destruction event.
    ///
    /// Pipeline:
    /// 1. Check if the window was in tiling state **before** removal.
    /// 2. If tiling: [`remove_from_layout_and_refocus`] — removes from layout,
    ///    pushes focus to the successor, and animates.
    /// 3. `registry.remove_window(hwnd)` — always, regardless of state.
    ///
    /// The tiling check happens before removal because `remove_window`
    /// deletes the entry from the registry.
    pub(super) fn on_window_destroyed(&mut self, hwnd: isize) {
        let was_tiling = self.registry.is_tiling(hwnd);

        if was_tiling {
            self.remove_from_layout_and_refocus(WindowId(hwnd));
        }

        self.registry.remove_window(hwnd);
    }

    /// Handle a window minimize event.
    ///
    /// Pipeline:
    /// 1. `registry.minimize_window(hwnd)` — updates state to `Tiling::Minimized`.
    /// 2. If the window was tiling-active (before minimize):
    ///    [`remove_from_layout_and_refocus`] — removes from layout, pushes
    ///    focus to the successor, and animates remaining windows filling the gap.
    pub(super) fn on_window_minimized(&mut self, hwnd: isize) {
        let was_tiling = self.registry.is_tiling(hwnd);
        self.registry.minimize_window(hwnd);

        if was_tiling {
            self.remove_from_layout_and_refocus(WindowId(hwnd));
        }
    }

    /// Shared window-removal pipeline for destroy and minimize events.
    ///
    /// This implements the focus-aware removal flow used by both
    /// [`on_window_destroyed`] and [`on_window_minimized`]:
    ///
    /// 1. Capture the current focus (before removal).
    /// 2. [`LayoutEngine::remove_window`] — removes the window from the virtual
    ///    layout, resolving a focus successor via
    ///    [`mutations::next_available_window`] when the removed window was
    ///    focused (left column preferred, then right).
    /// 3. **Push OS focus** — if the layout focus changed as a result of the
    ///    removal, call [`registry_win32::set_foreground_window`] on the
    ///    successor so the OS actually foregrounds it, then sync the registry's
    ///    focus tracking via [`WindowRegistry::set_focused`].
    /// 4. [`animate_layout`](Self::animate_layout) — animate the remaining windows
    ///    into their new positions.
    ///
    /// # Why capture focus before *and* after
    ///
    /// Comparing focus before and after removal tells us whether the removed
    /// window was the focused one. Only then do we need to push a new
    /// foreground window to the OS — if focus is unchanged, the OS focus is
    /// already correct and we avoid a redundant (and potentially disruptive)
    /// `SetForegroundWindow` call.
    fn remove_from_layout_and_refocus(&mut self, window: WindowId) {
        let prev_focus = self.layout.focused();
        let applied = self.layout.remove_window(window);
        let new_focus = self.layout.focused();

        if new_focus != prev_focus
            && let Some(id) = new_focus
        {
            let target = id.0;
            if !registry_win32::set_foreground_window(target) {
                log::warn!(
                    "remove_from_layout_and_refocus: SetForegroundWindow failed for hwnd {target}"
                );
            }
            self.registry.set_focused(target);
        }

        self.animate_layout(&applied);
    }

    /// Handle a window restore (un-minimize) event.
    ///
    /// Pipeline:
    /// 1. `registry.restore_window(hwnd)` — updates state back to `Tiling::Active`.
    /// 2. If the window is now tiling-active (after restore):
    ///    - `layout.add_window(id)` — re-adds to layout.
    ///    - `animate_layout(applied)` — animates the new window appearing.
    pub(super) fn on_window_restored(&mut self, hwnd: isize) {
        self.registry.restore_window(hwnd);

        // After restore, check if the window is now tiling-active.
        if self.registry.is_tiling(hwnd) {
            let applied = self.layout.add_window(WindowId(hwnd));
            self.animate_layout(&applied);
        }
    }

    /// Handle a focus change event.
    ///
    /// Pipeline:
    /// 1. `registry.set_focused(hwnd)` — updates focused window in registry.
    /// 2. If the focused window is tiling:
    ///    - `layout.set_focus(id)` — updates layout focus state.
    ///
    /// Note: `set_focus` does not produce an [`AppliedLayout`] — it only updates
    /// internal focus tracking. The next layout mutation will use the correct
    /// focus.
    pub(super) fn on_focus_changed(&mut self, hwnd: isize) {
        self.registry.set_focused(hwnd);

        if self.registry.is_tiling(hwnd) {
            self.layout.set_focus(WindowId(hwnd));
        }
    }
}
