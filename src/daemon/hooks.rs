//! Win32 hook event handlers.
//!
//! This module contains individual handlers for each type of hook event:
//!
//! - [`on_window_created`] — handles new window creation
//! - [`on_window_destroyed`] — handles window destruction
//! - [`on_window_minimized`] — handles window minimize events
//! - [`on_window_restored`] — handles window restore (un-minimize) events
//! - [`on_window_hidden`] — handles tray-hide / DWM-cloak events
//! - [`on_window_shown`] — handles un-hide (tray-restore) events
//! - [`on_focus_changed`] — handles focus changes
//!
//! # Window-removal pipeline
//!
//! Both [`on_window_destroyed`] and [`on_window_minimized`] and
//! [`on_window_hidden`] share the
//! [`remove_from_layout_and_refocus`] helper, which implements the full
//! removal pipeline: remove from the virtual layout → push OS-level focus to
//! the successor window if focus changed → animate the resulting layout diff.
//! The successor window is chosen by [`ScrollingSpace::remove_window`](crate::workspace::ScrollingSpace::remove_window) via
//! [`mutations::next_available_window`] (left column, then right).

use crate::common::WindowId;
use crate::registry::hooks::remove_float_hwnd;
use crate::registry::types::{ReclassifyResult, VisibilityChange};
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
    /// [`ScrollingSpace::insert_window`](crate::workspace::ScrollingSpace::insert_window)
    /// for the full algorithm.
    pub(super) fn on_window_created(&mut self, hwnd: isize) -> bool {
        if let Some(window_id) = self.registry.handle_created(hwnd) {
            let applied = self.active_scrolling_mut().insert_window(window_id);
            self.animate_layout(&applied);
            true
        } else {
            // Classification failed — either the window isn't ready yet
            // (not visible, no title) or it was classified as floating/
            // ignored (already registered). The caller adds the hwnd to
            // the pending-creations retry list. For already-registered
            // windows, handle_created returns None immediately on retry
            // via its `contains_key` de-duplication gate, so the retry is
            // cheap and the window is dropped after the retry limit — harmless.
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

        // A destroyed float must leave the tracking set so the LOCATIONCHANGE
        // callback stops forwarding it. Harmless no-op for tiled/ignored windows.
        remove_float_hwnd(hwnd);

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

    /// Handle `EVENT_OBJECT_HIDE` for a window that may have been tray-hidden
    /// or DWM-cloaked (Discord/Steam "close to tray"), as opposed to an
    /// explicit minimize (handled by [`on_window_minimized`](Self::on_window_minimized)).
    ///
    /// Pipeline:
    /// 1. Capture active-tiling status **before** reconcile mutates state.
    /// 2. `registry.reconcile_visibility(hwnd)` — state-based idempotent
    ///    transition (`Active → Hidden`, preserving the layout slot in
    ///    `last_virtual_slot`). Returns [`VisibilityChange::Unchanged`] if the
    ///    window was already hidden/minimized (this event also fires on ordinary
    ///    minimize — the state check prevents double-removal).
    /// 3. If the window was an **active** tiling window immediately before the
    ///    transition and the state actually changed to `Hidden`:
    ///    [`remove_from_layout_and_refocus`] — removes from layout, pushes focus
    ///    to the successor, and animates.
    ///
    /// # Why `is_tiling_active` instead of `is_tiling`
    ///
    /// [`is_tiling`](crate::registry::WindowRegistry::is_tiling) returns `true`
    /// for both `Tiling(Active)` and `Tiling(Minimized)`. Using it here would
    /// cause a second removal from the layout when a minimized window also
    /// receives `EVENT_OBJECT_HIDE` (Win32 fires both). `is_tiling_active` is
    /// `true` **only** for `Tiling(Active)`, so the first transition (minimize
    /// or hide) removes the window, and the second event sees `is_tiling_active`
    /// as `false` and is a no-op.
    pub(super) fn on_window_hidden(&mut self, hwnd: isize) {
        let was_active_tiling = self.registry.is_tiling_active(hwnd);
        let change = self.registry.reconcile_visibility(hwnd);
        if change == VisibilityChange::Hidden && was_active_tiling {
            self.remove_from_layout_and_refocus(WindowId(hwnd));
        }
    }

    /// Shared window-removal pipeline for destroy and minimize events.
    ///
    /// This implements the focus-aware removal flow used by both
    /// [`on_window_destroyed`] and [`on_window_minimized`]:
    ///
    /// 1. Capture the current focus (before removal).
    /// 2. [`ScrollingSpace::remove_window`](crate::workspace::ScrollingSpace::remove_window) — removes the window from the virtual
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
        let prev_focus = self.active_scrolling().last_focused_window();
        let applied = self.active_scrolling_mut().remove_window(window);
        let new_focus = self.active_scrolling().last_focused_window();

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
            let applied = self.active_scrolling_mut().add_window(WindowId(hwnd));
            self.animate_layout(&applied);
        }
    }

    /// Handle `EVENT_OBJECT_SHOW`.
    ///
    /// This event covers two distinct situations, handled in order below.
    ///
    /// # 1. Recovery — a window created *invisible* (never registered)
    ///
    /// Some applications create their top-level window hidden and only show it
    /// **after** their title has arrived via `EVENT_OBJECT_NAMECHANGE`. For
    /// such windows both `CREATE` and the
    /// [`on_window_name_change`](Self::on_window_name_change) recovery fail
    /// [`handle_created`](crate::registry::WindowRegistry::handle_created)'s
    /// visibility gate (the window is not yet visible), so they stay untracked.
    /// Windows Terminal launched as `wt -p PowerShell` is the canonical
    /// example, with event order `Created → NameChange → Shown`.
    ///
    /// `SHOW` is the first event at which the window is actually visible, so it
    /// is the natural point to retry registration. If the window is **not
    /// already tracked**, we re-run the full creation pipeline via
    /// [`on_window_created`](Self::on_window_created). By the time `SHOW`
    /// fires the title is already set (the earlier `NAMECHANGE` proved it —
    /// it failed for *visibility*, not *empty title*), so the only thing that
    /// changed is visibility, and the window now passes every gate.
    ///
    /// This path is **complementary** to `on_window_name_change`: that handler
    /// catches the opposite ordering (window visible *first*, title arriving
    /// *later*), where `SHOW` would see an empty title. Both are needed for
    /// full coverage of asynchronous-window lifecycles.
    ///
    /// # 2. Re-show — a tracked window returning from hidden
    ///
    /// A window we already manage (e.g. Discord reopened from the tray, as
    /// opposed to an explicit restore-from-minimize handled by
    /// [`on_window_restored`](Self::on_window_restored)) transitions
    /// `Hidden → Active`:
    /// 1. `registry.reconcile_visibility(hwnd)` — state-based idempotent
    ///    transition (`Hidden → Active`, restoring the saved slot from
    ///    `last_virtual_slot`). Returns [`VisibilityChange::Unchanged`] if the
    ///    window was already active.
    /// 2. If the state actually changed to `Shown` and the window is now an
    ///    **active** tiling window:
    ///    - `layout.add_window(id)` — re-adds to layout.
    ///    - `animate_layout(applied)` — animates the window reappearing.
    ///
    /// # Why this is safe to fire on every `SHOW`
    ///
    /// Win32 fires `EVENT_OBJECT_SHOW` for many windows we do not manage. The
    /// untracked branch is bounded by `handle_created`'s full gate pipeline
    /// (visibility, title, Alt+Tab, owner) — non-app windows are filtered, not
    /// registered. The tracked branch is bounded by `reconcile_visibility`,
    /// which returns `Unchanged` for already-active and floating windows.
    /// `Floating` windows receive state updates but are never in the layout.
    ///
    /// Duplicate registration across recovery events (`NAMECHANGE` + `SHOW`) is
    /// impossible: [`is_tracked`](crate::registry::WindowRegistry::is_tracked)
    /// short-circuits the second handler, and `handle_created`'s `contains_key`
    /// gate is the final guard — an `HWND` is unique per window, so a window
    /// already in the registry is ignored.
    pub(super) fn on_window_shown(&mut self, hwnd: isize) {
        // Recovery: a window created invisible (e.g. `wt -p PowerShell`, whose
        // title arrives via NAMECHANGE while still hidden) was missed by both
        // CREATE and NAMECHANGE (both gate on visibility). SHOW is the first
        // event where the window is actually visible — attempt registration
        // now. Mirrors `on_window_name_change`; safe because `handle_created`
        // de-duplicates by HWND.
        if !self.registry.is_tracked(hwnd) {
            log::debug!(
                "on_window_shown: {hwnd:#x} not tracked — attempting creation (created invisible?)"
            );
            self.on_window_created(hwnd);
            return;
        }

        // Re-show path: a tracked window returning from hidden (Hidden → Active).
        let change = self.registry.reconcile_visibility(hwnd);
        if change == VisibilityChange::Shown && self.registry.is_tiling_active(hwnd) {
            let applied = self.active_scrolling_mut().add_window(WindowId(hwnd));
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
            self.active_scrolling_mut().set_focus(WindowId(hwnd));
        }
    }

    /// Handle `EVENT_OBJECT_STATECHANGE` — Option D recovery for windows that
    /// launched maximized or fullscreen.
    ///
    /// Pipeline:
    /// 1. `registry.reclassify_os_state(hwnd)` — re-queries the live OS state.
    ///    If a tracked window was `Ignored(Maximized|Fullscreen)` but is no
    ///    longer, the classifier is re-run and its stored state is updated.
    /// 2. If the window transitioned to tiling
    ///    ([`ReclassifyResult::Recovered`] with `now_tiling == true`):
    ///    - `layout.add_window(id)` — appends the recovered window to the
    ///      layout (mirrors [`on_window_restored`](Self::on_window_restored)).
    ///    - `animate_layout(applied)` — animates the window entering.
    ///
    /// # Why `add_window` (append) and not `insert_window`
    ///
    /// A recovered window was never part of the layout (it was ignored from
    /// creation), so there is no saved slot to restore. Appending at the far
    /// right is the predictable choice and matches the restore/show recovery
    /// paths. The cheap non-recovery cases (untracked / not OS-ignored /
    /// still ignored) are filtered inside the registry, so this handler costs
    /// almost nothing when nothing is recoverable.
    pub(super) fn on_window_state_change(&mut self, hwnd: isize) {
        let outcome = self.registry.reclassify_os_state(hwnd);
        if let ReclassifyResult::Recovered { now_tiling: true } = outcome {
            let applied = self.active_scrolling_mut().add_window(WindowId(hwnd));
            self.animate_layout(&applied);
        }
    }

    /// Handle `EVENT_OBJECT_NAMECHANGE` — Option A recovery for windows whose
    /// title arrives *after* `EVENT_OBJECT_CREATE`.
    ///
    /// Some applications — most notably Windows Terminal — set their window
    /// title asynchronously, long after `CREATE` fired and after stm's
    /// pending-creations retry budget has been exhausted. Such windows are
    /// never registered and silently fall outside the tiling layout.
    ///
    /// `NAMECHANGE` fires the moment the title lands. This handler gives stm a
    /// second chance: if the window is **not already tracked**, it re-attempts
    /// the full creation pipeline via
    /// [`on_window_created`](Self::on_window_created).
    ///
    /// # Why only untracked windows?
    ///
    /// Re-classifying an already-tracked window on every title change would
    /// risk layout churn (a rule could match the new title differently). The
    /// [`is_tracked`](crate::registry::WindowRegistry::is_tracked) guard
    /// ensures we only ever register windows that were missed the first time.
    ///
    /// If `on_window_created` still returns `false` (e.g. the window is not
    /// yet visible), we do **not** re-add it to `pending_creations` here:
    /// apps often fire several `NAMECHANGE`s during initialization, so the
    /// next one will give us another opportunity.
    pub(super) fn on_window_name_change(&mut self, hwnd: isize) {
        if self.registry.is_tracked(hwnd) {
            // Tracked windows fire NAMECHANGE frequently (e.g. a terminal's
            // title updating on every prompt). Re-classifying them would risk
            // layout churn, so we silently ignore these — no log line, to avoid
            // flooding the daemon log with per-keystroke title updates.
            return;
        }
        // Untracked window just got a name — likely a late-titled app (such as
        // Windows Terminal, which sets its title only after its child shell
        // starts). Give it a second chance at the full creation pipeline.
        log::debug!(
            "on_window_name_change: {hwnd:#x} not tracked — re-attempting creation (late title?)"
        );
        // Re-attempt registration. The window now has a title (this is a
        // NAMECHANGE event), so handle_created's title-empty gate should pass.
        // If it still returns false, we simply wait for the next NAMECHANGE.
        self.on_window_created(hwnd);
    }
}
