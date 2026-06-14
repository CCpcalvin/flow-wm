//! Win32 hook event handlers.
//!
//! This module contains individual handlers for each type of hook event:
//!
//! - [`on_window_created`] — handles new window creation
//! - [`on_window_destroyed`] — handles window destruction
//! - [`on_window_minimized`] — handles window minimize events
//! - [`on_window_restored`] — handles window restore (un-minimize) events
//! - [`on_focus_changed`] — handles focus changes

use crate::common::WindowId;

use super::types::ScrollTilingManager;

impl ScrollTilingManager {
    /// Handle a window creation event.
    ///
    /// Pipeline:
    /// 1. `registry.handle_created(hwnd)` — classifies and registers the window.
    /// 2. If the window was classified as tiling (`Some(WindowId)`):
    ///    - `layout.add_window(id)` — adds it as a new column.
    ///    - `animate_layout(applied)` — animates the resulting layout change.
    /// 3. If the window was floating, ignored, or skipped: no action needed.
    pub(super) fn on_window_created(&mut self, hwnd: isize) {
        if let Some(window_id) = self.registry.handle_created(hwnd) {
            let applied = self.layout.add_window(window_id);
            self.animate_layout(&applied);
        }
    }

    /// Handle a window destruction event.
    ///
    /// Pipeline:
    /// 1. Check if the window was in tiling state **before** removal.
    /// 2. If tiling: `layout.remove_window(id)` → `animate_layout(applied)`.
    /// 3. `registry.remove_window(hwnd)` — always, regardless of state.
    ///
    /// The tiling check happens before removal because `remove_window`
    /// deletes the entry from the registry.
    pub(super) fn on_window_destroyed(&mut self, hwnd: isize) {
        let was_tiling = self.registry.is_tiling(hwnd);

        if was_tiling {
            let applied = self.layout.remove_window(WindowId(hwnd));
            self.animate_layout(&applied);
        }

        self.registry.remove_window(hwnd);
    }

    /// Handle a window minimize event.
    ///
    /// Pipeline:
    /// 1. `registry.minimize_window(hwnd)` — updates state to `Tiling::Minimized`.
    /// 2. If the window was tiling-active (before minimize):
    ///    - `layout.remove_window(id)` — removes from layout.
    ///    - `animate_layout(applied)` — animates remaining windows filling the gap.
    pub(super) fn on_window_minimized(&mut self, hwnd: isize) {
        let was_tiling = self.registry.is_tiling(hwnd);
        self.registry.minimize_window(hwnd);

        if was_tiling {
            let applied = self.layout.remove_window(WindowId(hwnd));
            self.animate_layout(&applied);
        }
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
