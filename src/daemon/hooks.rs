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
use crate::registry::types::{FloatingState, ReclassifyResult, VisibilityChange, WindowState};
use crate::registry::win32 as registry_win32;
use windows::Win32::Foundation::HWND;

use super::types::FlowWM;

impl FlowWM {
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
    /// 3. If the window was classified as floating: complete the float setup
    ///    that an explicit `set-window float` performs — place it into the
    ///    active [`FloatingSpace`](crate::workspace::FloatingSpace) at a
    ///    centered rect, arm float-location tracking, animate, and attach a
    ///    border. Without this, a rule-classified float would be tracked in the
    ///    registry but invisible to workspace switching and borderless until
    ///    the user toggled it. See (`docs/src/dev-guide/floating-space.md`).
    /// 4. Ignored or already-tracked windows: no action needed.
    ///
    /// # Return value
    ///
    /// Returns `true` if [`handle_created`](crate::registry::WindowRegistry::handle_created)
    /// processed the window (classified it as tiling, floating, or ignored, or
    /// it was already tracked). Returns `false` if classification **failed** —
    /// the window is not yet ready (not visible, no title, styles not
    /// finalized). The caller should add the hwnd to the pending-creations
    /// retry list when this returns `false`.
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
        // `was_tracked` distinguishes a FRESH registration (this call just
        // classified the window) from a de-duplicated duplicate CREATE for an
        // already-managed window. handle_created early-returns None on its
        // `contains_key` gate for the latter; without this flag a freshly
        // classified float (which still needs FloatingSpace init) would be
        // indistinguishable from a duplicate of an already-initialised float.
        let was_tracked = self.registry.is_tracked(hwnd);

        // Tiling: handle_created returns Some only for a freshly-registered
        // tiling window.
        if let Some(window_id) = self.registry.handle_created(hwnd) {
            let applied = self.active_scrolling_mut().insert_window(window_id);
            self.animate_layout(&applied);
            self.reconcile_new_window_focus(hwnd);
            return true;
        }

        // handle_created returned None. Sub-cases:
        //  (1) freshly classified as Floating → needs FloatingSpace/border init;
        //  (2) freshly classified as Ignored → tracked, no action, no retry;
        //  (3) duplicate CREATE for an already-managed window → no action;
        //  (4) classification failed (not visible / no title / styles pending)
        //      → caller retries via pending_creations.
        //
        // Only a freshly-registered active float (case 1) requires work. It is
        // identifiable as: not previously tracked, now in the registry as
        // Floating(Active). register_float + animate complete the float setup
        // that set_window_to_float performs for an explicit toggle, so an
        // init-classified float ends up identical to a toggled one.
        let fresh_float = !was_tracked
            && self
                .registry
                .get_window(HWND(hwnd as *mut _))
                .map(|w| matches!(w.state, WindowState::Floating(FloatingState::Active { .. })))
                .unwrap_or(false);

        if fresh_float {
            let float_rect = self.centered_float_rect(WindowId(hwnd));
            let float_actual = self.register_float(WindowId(hwnd), float_rect);
            self.animate_workspaces(&[(float_actual, 0)]);
            self.reconcile_new_window_focus(hwnd);
        }

        // Report whether the window ended up tracked (classified) so the caller
        // retries only genuinely-not-ready windows (case 4).
        self.registry.is_tracked(hwnd)
    }

    /// Reconcile OS focus and border overlays for a freshly-tracked window
    /// (tiling or floating), shared by both branches of [`on_window_created`].
    ///
    /// If the window is the live OS foreground, sync the registry's focus
    /// tracker to it before refreshing its border so it paints `Focused`
    /// immediately — late-titled apps (Windows Terminal, recovered via
    /// NAMECHANGE/SHOW) have their `EVENT_SYSTEM_FOREGROUND` fire before they
    /// are tracked, leaving `focused()` stale and the new foreground border
    /// resolving to `Unfocused`. The live `GetForegroundWindow` query is
    /// authoritative and cheap. Then recolor the previous foreground
    /// (`Focused → Unfocused`), mirroring [`on_focus_changed`]'s two-window
    /// refresh so the handoff is visually consistent.
    fn reconcile_new_window_focus(&mut self, hwnd: isize) {
        let prev_focus = self.registry.focused().map(|id| id.0);
        let now_foreground = registry_win32::get_foreground_window() == Some(hwnd);
        if now_foreground {
            self.registry.set_focused(hwnd);
        }

        // Border overlay: attach (or skip) based on the freshly-classified
        // state. Tiling/Floating → attach; Ignored → no-op.
        self.refresh_border_for(hwnd);

        // If focus moved to the new window, recolor the previous foreground.
        if now_foreground
            && let Some(prev) = prev_focus
            && prev != hwnd
        {
            self.refresh_border_for(prev);
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
        // Border overlay: cleaned up automatically — `remove_window` drops the
        // Window, whose `border: Option<Border>` field drops the Border, whose
        // Drop impl calls DestroyWindow on the overlay HWND.
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
        // Border overlay: registry now records Minimized ⇒ refresh detaches.
        self.refresh_border_for(hwnd);
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
        // Border overlay: registry now records Hidden ⇒ refresh detaches.
        self.refresh_border_for(hwnd);
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
        // Border overlay: registry now records Active ⇒ refresh re-attaches.
        self.refresh_border_for(hwnd);
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
        // Border overlay: registry now records Active ⇒ refresh re-attaches.
        // (The untracked-recovery branch above early-returns after
        // on_window_created, which performs its own refresh_border_for; this
        // line only runs for the tracked-reveal path.)
        self.refresh_border_for(hwnd);
    }

    /// Handle `EVENT_SYSTEM_FOREGROUND` — the single sink for all foreground
    /// changes, both external (alt-tab, PowerToys, clicks) and self-induced
    /// (`set_foreground_window` from `dispatch_focus` /
    /// `switch_active_workspace`). For self-induced changes the method is a
    /// natural no-op: the focused column is already visible, so
    /// `ensure_focused_visible` returns `None`.
    pub(super) fn on_focus_changed(&mut self, hwnd: isize) {
        // Capture the previous focus BEFORE set_focused mutates it: only the
        // previously-focused and newly-focused windows can change border
        // color on a focus switch, so we refresh just those two instead of
        // every overlay. Combined with `Border::set_style`'s unchanged-style
        // short-circuit, this makes the focus path O(1) rather than O(N).
        let prev_focus = self.registry.focused().map(|id| id.0);

        self.registry.set_focused(hwnd);

        let target = WindowId(hwnd);

        // Bail for untracked/ignored windows (taskbar, tray, off-monitor).
        let Some(home_ws) = self.active_monitor().find_workspace_containing(target) else {
            return;
        };

        // If the window lives on a different workspace, switch there. Use the
        // pure layout variant — the OS already foregrounded hwnd, so re-pushing
        // would steal focus from a floating window the user alt-tabbed to.
        let active_id = self.active_monitor().active_workspace_id();
        if home_ws != active_id {
            self.switch_workspace_layout(home_ws, active_id);
        }

        // For tiling windows: update the layout cursor and scroll the camera to
        // reveal the column. Floating windows only need the workspace switch
        // above — they float above all columns.
        if self.registry.is_tiling(hwnd) {
            self.active_scrolling_mut().set_focus(target);
            if let Some(diff) = self.active_scrolling_mut().ensure_focused_visible() {
                self.animate_layout(&diff);
            }
        }

        // Refresh the new foreground first, then the previous one. `set_focused`
        // ignores untracked HWNDs, so if `hwnd` is the taskbar / a dialog,
        // `focused()` is unchanged and neither refresh recolors anything
        // (short-circuit). If `hwnd` is newly-tracked, only it and the prior
        // focus actually change color.
        self.refresh_border_for(hwnd);
        if let Some(prev) = prev_focus
            && prev != hwnd
        {
            self.refresh_border_for(prev);
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
        // Border overlay: covers all transitions — Ignored→Tiling (attach),
        // Tiling→Ignored (detach), and no-op cases.
        self.refresh_border_for(hwnd);
    }

    /// Handle `EVENT_OBJECT_NAMECHANGE` — Option A recovery for windows whose
    /// title arrives *after* `EVENT_OBJECT_CREATE`.
    ///
    /// Some applications — most notably Windows Terminal — set their window
    /// title asynchronously, long after `CREATE` fired and after flow's
    /// pending-creations retry budget has been exhausted. Such windows are
    /// never registered and silently fall outside the tiling layout.
    ///
    /// `NAMECHANGE` fires the moment the title lands. This handler gives flow a
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
