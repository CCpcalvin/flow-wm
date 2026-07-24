//! Animation bridge — converts layout snapshots to animation targets.
//!
//! This module contains:
//!
//! - [`FlowWM::animate_layout`] — converts an [`AppliedLayout`]
//!   into animation targets and submits to the animator.
//! - [`FlowWM::animate_workspaces`] — multi-workspace variant
//!   that submits a single combined batch with per-workspace vertical
//!   `y_offset` applied (used by `SwitchWorkspace` / `MoveWindowToWorkspace`).
//! - [`FlowWM::teleport_workspaces`] — bypass-animator variant
//!   that directly `SetWindowPos`-es bystander workspaces into place with no
//!   animation, used to maintain the workspace stacking invariant during
//!   switches without visual noise.
//! - [`animate_layout_raw`] — standalone version used during construction when
//!   `FlowWM` doesn't exist yet.

use windows::Win32::Foundation::HWND;

use crate::animation::{IVec2, WindowAnimator, WindowRef, WindowTarget};
use crate::common::Rect;
use crate::layout::types::{ActualLayout, AppliedLayout};
use crate::registry::WindowRegistry;

use super::types::FlowWM;

impl FlowWM {
    /// Convert an [`AppliedLayout`] into animation targets and submit to the animator.
    ///
    /// This is the critical conversion point between the layout engine's output
    /// (flow types) and the animation system's input (animation types):
    ///
    /// | flow Type | Animation Type |
    /// |----------|---------------|
    /// | `WindowId(isize)` | `WindowRef(isize)` |
    /// | `Rect { x, y, width, height }` position | `IVec2::new(x, y)` |
    /// | `Rect { x, y, width, height }` size | `IVec2::new(width, height)` |
    ///
    /// Also synchronizes the registry's tiling state (col/row indices and tiled
    /// rects) from the new layout. This happens even when there are no animation
    /// moves — a swap can change a window's logical position without triggering
    /// a pixel-level move if the swapped columns have the same width.
    ///
    /// # Why pass ALL windows (not just "changed" ones)
    ///
    /// Targets are built from **every** entry in `actual_layout`, not just the
    /// windows whose logical position changed. The animator's `build_tweens`
    /// compares each target rect against the window's real on-screen position
    /// and drops no-ops (windows already at their target). This ensures that
    /// windows which are still mid-flight from a previous (interrupted) animation
    /// are correctly retargeted even when their target rect didn't change in
    /// this mutation — fixing the "rapid swap-column stranding" bug.
    ///
    /// Animation errors are logged as warnings but not propagated — a jarring
    /// animation is better than a crash.
    ///
    /// # Visible-Rect to Window-Rect Translation
    ///
    /// The layout engine computes **visible rects** — the coordinates of the
    /// content area the user actually sees. However, `SetWindowPos` (called by
    /// the Win32 animation backend) positions windows using **window rects** —
    /// the full rect from `GetWindowRect` which includes invisible borders
    /// (shadows, resize hit-test areas).
    ///
    /// This method translates each target's rect from visible-rect space to
    /// window-rect space using the window's stored
    /// [`InvisibleBounds`](crate::common::InvisibleBounds). Without this
    /// translation, windows would appear with gaps larger than configured
    /// because `SetWindowPos` would place the *outer* edge at the intended
    /// *inner* edge position.
    ///
    /// This also fixes animation correctness: the animator's retarget path
    /// queries `GetWindowRect` for the current `from` position (window-rect
    /// space). If `to` were in visible-rect space, interpolation between
    /// mismatched coordinate spaces would cause visual distortion during
    /// the animation.
    pub(super) fn animate_layout(&mut self, layout: &AppliedLayout) {
        // Always sync registry state from the new layout, even if no windows
        // physically moved (col/row indices may have changed).
        self.registry
            .update_tiling_slots_from_layout(&layout.virtual_layout);
        self.registry.update_tiled_rects(&layout.actual_layout);

        if layout.actual_layout.entries.is_empty() {
            return;
        }

        // Safety net: during a tile drag, exclude the dragged window and its
        // border overlay from animation targets so the animator doesn't fight
        // the user's mouse. This covers code paths (e.g. window Created/
        // Destroyed hooks) that call animate_layout while a drag is active.
        // When on_drag_end calls animate_layout, drag_state has already been
        // take()n so the filter does not trigger — the dragged window animates
        // to its final position as intended.
        let excluded_id = self.drag_state.as_ref().map(|ds| ds.dragged_id);
        let excluded_border_hwnd = self.drag_state.as_ref().and_then(|ds| {
            self.registry
                .get_window(HWND(ds.dragged_hwnd as *mut _))
                .and_then(|w| w.border.as_ref())
                .map(|b| b.hwnd())
        });

        // Effective border inset. Each window's rect shrinks by
        // (thickness - overlap) px per edge so the overlay ring (the outer
        // `thickness` px of the layout slot) overlaps the visible content by
        // `overlap` px — closing the DWM hairline. overlap=0 reproduces the
        // old full-gap behavior. See `docs/src/dev-guide/borders.md`.
        let border_thickness = self.config.borders.thickness as i32;
        let border_overlap = self.config.borders.overlap as i32;

        let mut targets: Vec<WindowTarget> = Vec::new();
        for entry in &layout.actual_layout.entries {
            // Skip the dragged window during an active drag.
            if Some(entry.window_id) == excluded_id {
                continue;
            }

            // Look up the window once; extract invisible bounds and the
            // border overlay HWND from the shared reference. Option<&Window>
            // is Copy (all &T are Copy), so both .map calls below take
            // independent copies without invalidating `window`.
            let window = self.registry.get_window(HWND(entry.window_id.0 as *mut _));
            let invisible_bounds = window.map(|w| w.invisible_bounds).unwrap_or_default();
            let border_hwnd = window.and_then(|w| w.border.as_ref()).map(|b| b.hwnd());

            // Skip the dragged window's border overlay.
            if let Some(eb) = excluded_border_hwnd
                && border_hwnd == Some(eb)
            {
                continue;
            }

            // Translate the layout engine's visible rect into a Win32
            // window rect. This compensates for invisible borders so
            // that SetWindowPos places the window's visible content
            // exactly where the layout engine intended. Then shrink by
            // the configured border thickness to leave room for the
            // colored overlay ring.
            let window_rect = invisible_bounds
                .visible_to_window(entry.rect)
                .inset(border_thickness - border_overlap);

            log::debug!(
                "animate: hwnd={} target ({},{},{},{}) [visible] \
                 → ({},{},{},{}) [window]",
                entry.window_id.0,
                entry.rect.x,
                entry.rect.y,
                entry.rect.width,
                entry.rect.height,
                window_rect.x,
                window_rect.y,
                window_rect.width,
                window_rect.height,
            );

            targets.push(WindowTarget::new(
                WindowRef(entry.window_id.0),
                IVec2::new(window_rect.x, window_rect.y),
                IVec2::new(window_rect.width, window_rect.height),
            ));

            // Flatten the border overlay into the same target list so
            // the animator moves it in lockstep with the window. Border
            // overlays sit at the layout engine's visible rect — no
            // invisible-bounds translation, no thickness inset — because
            // the ring occupies the gap between entry.rect and the
            // inset window_rect. The animator's SetWindowPos-based
            // backend treats the overlay HWND like any other window.
            if let Some(b_hwnd) = border_hwnd {
                targets.push(WindowTarget::new(
                    WindowRef(b_hwnd),
                    IVec2::new(entry.rect.x, entry.rect.y),
                    IVec2::new(entry.rect.width, entry.rect.height),
                ));
            }
        }

        log::debug!(
            "animate_layout: submitting {} targets to animator",
            targets.len()
        );

        if let Err(e) = self.animator.animate(targets) {
            log::warn!("animation error: {e}");
        }
    }

    /// Submit a single combined animation batch spanning multiple workspaces.
    ///
    /// Each `(ActualLayout, y_offset)` pair describes one workspace's current
    /// on-screen layout plus the vertical parking offset to apply to every
    /// window in that workspace. All windows from all pairs are merged into a
    /// single [`Vec<WindowTarget>`] and submitted to the animator in one call.
    /// This is what makes a workspace switch animate as a single coordinated
    /// transition rather than N independent moves — the animator's worker
    /// thread builds tweens for every target simultaneously and drives them at
    /// a shared frame rate.
    ///
    /// # Where the `y_offset` is applied
    ///
    /// The offset is applied **here** in the daemon bridge, not in
    /// [`layout::projection`](crate::layout::projection). The projection layer
    /// stays pure and horizontal-only — it always produces workspace-local
    /// coordinates, and this method decides where each workspace sits
    /// vertically in the monitor stack. See
    /// [`workspace_y_offset`](crate::workspace::workspace_y_offset) for the
    /// offset computation.
    ///
    /// # When to use this vs [`animate_layout`](Self::animate_layout)
    ///
    /// - `animate_layout` — single-workspace mutation (scroll, focus, swap,
    ///   resize). One workspace, `y_offset = 0`.
    /// - `animate_workspaces` — workspace ops (switch, move-to-workspace)
    ///   where two workspaces animate in lockstep with different `y_offset`s.
    ///
    /// # Registry sync is the caller's responsibility
    ///
    /// Unlike [`animate_layout`](Self::animate_layout), this method does
    /// **not** call [`registry.update_tiling_slots_from_layout`] or
    /// [`registry.update_tiled_rects`] — those operate on a single virtual
    /// layout at a time, and the caller must invoke them per-workspace
    /// before constructing the `(ActualLayout, y_offset)` pairs. This method
    /// only submits animation targets.
    ///
    /// [`registry.update_tiling_slots_from_layout`]:
    ///     crate::registry::WindowRegistry::update_tiling_slots_from_layout
    /// [`registry.update_tiled_rects`]:
    ///     crate::registry::WindowRegistry::update_tiled_rects
    pub(super) fn animate_workspaces(&mut self, batches: &[(ActualLayout, i32)]) {
        if batches.is_empty() {
            return;
        }

        let border_thickness = self.config.borders.thickness as i32;
        let border_overlap = self.config.borders.overlap as i32;

        // Build the combined target list. Iterating with explicit loops
        // (rather than `flat_map`) lets us hold one `&self.registry` borrow
        // at a time without entangling the borrow checker across closures.
        let mut targets: Vec<WindowTarget> = Vec::new();
        // Whether any target in this batch is a tracked float window. Used
        // after the loop to decide whether to suppress LOCATIONCHANGE capture
        // for the animation's duration.
        let mut has_float_target = false;
        for (layout, y_offset) in batches {
            for entry in &layout.entries {
                if !has_float_target && crate::registry::hooks::is_float_hwnd(entry.window_id.0) {
                    has_float_target = true;
                }
                let window = self.registry.get_window(HWND(entry.window_id.0 as *mut _));
                let invisible_bounds = window.map(|w| w.invisible_bounds).unwrap_or_default();
                let border_hwnd = window.and_then(|w| w.border.as_ref()).map(|b| b.hwnd());

                // Translate visible-rect → window-rect, shrink by the border
                // inset (thickness - overlap) to make room for the ring, then apply the
                // vertical offset. Order doesn't matter mathematically (offset
                // is a pure y translation, invisible_bounds adjusts y by a
                // fixed `-top`, inset is uniform), but the chosen order keeps
                // the "workspace-local rect first, workspace stack second"
                // mental model clear.
                let window_rect = invisible_bounds
                    .visible_to_window(entry.rect)
                    .inset(border_thickness - border_overlap);
                targets.push(WindowTarget::new(
                    WindowRef(entry.window_id.0),
                    IVec2::new(window_rect.x, window_rect.y + y_offset),
                    IVec2::new(window_rect.width, window_rect.height),
                ));

                // Flatten the border overlay at the visible rect (same
                // y_offset) so it moves in lockstep with the window across
                // the workspace switch. See animate_layout for the rationale
                // on why border targets use entry.rect directly.
                if let Some(b_hwnd) = border_hwnd {
                    targets.push(WindowTarget::new(
                        WindowRef(b_hwnd),
                        IVec2::new(entry.rect.x, entry.rect.y + y_offset),
                        IVec2::new(entry.rect.width, entry.rect.height),
                    ));
                }
            }
        }

        if targets.is_empty() {
            return;
        }

        // Suppress LOCATIONCHANGE capture for the animation's duration so the
        // animator's intermediate rects don't reach FloatingSpace. Only armed
        // when animation is enabled: an instant (0 ms) snap lands at the final
        // rect, so its LOCATIONCHANGE is already correct.
        if has_float_target && self.config.animation.enabled {
            self.arm_float_tracking_suppression();
        }

        log::debug!(
            "animate_workspaces: submitting {} targets across {} workspace batch(es)",
            targets.len(),
            batches.len()
        );

        if let Err(e) = self.animator.animate(targets) {
            log::warn!("animate_workspaces error: {e}");
        }
    }

    /// Silently teleport windows across multiple workspaces, bypassing the animator.
    ///
    /// Each `(ActualLayout, y_offset)` pair is rendered by calling
    /// [`set_window_rect`](crate::registry::win32::set_window_rect) directly
    /// for every window — an instant `SetWindowPos` with no tweening. This is
    /// the **bystander path** used during workspace switches to relocate
    /// non-participating workspaces whose parking side changed (e.g. ws 3-7
    /// when switching 2 → 8: they were below active 2 but must end up above
    /// active 8). They snap into place instantly so the user's attention
    /// stays on the two workspaces that are actually animating.
    ///
    /// # Why bypass the animator?
    ///
    /// The animator's `RetargetFromCurrent` interrupt policy would animate
    /// every retargeted window from its current position to the new target.
    /// For a 10-workspace switch that could mean eight bystander workspaces
    /// all visibly sliding across the screen at once. Teleporting them keeps
    /// the animation focused on the participant workspaces and keeps the
    /// workspace stacking invariant intact.
    ///
    /// # Registry sync is the caller's responsibility
    ///
    /// Like [`animate_workspaces`](Self::animate_workspaces), this method
    /// only submits positions to Win32. It does **not** update tiling slots
    /// or tiled rects in the registry — the caller handles that
    /// per-workspace as needed.
    ///
    /// # Error handling
    ///
    /// Per-window failures are logged at `warn` level but never propagated.
    /// A failed teleport leaves a window at its old position — a minor
    /// visual inconsistency on a non-visible workspace, not a crash.
    pub(super) fn teleport_workspaces(&self, batches: &[(ActualLayout, i32)]) {
        let border_thickness = self.config.borders.thickness as i32;
        let border_overlap = self.config.borders.overlap as i32;
        for (layout, y_offset) in batches {
            for entry in &layout.entries {
                let window = self.registry.get_window(HWND(entry.window_id.0 as *mut _));
                let invisible_bounds = window.map(|w| w.invisible_bounds).unwrap_or_default();

                let window_rect = invisible_bounds
                    .visible_to_window(entry.rect)
                    .inset(border_thickness - border_overlap);
                let target_y = window_rect.y + y_offset;

                // Direct SetWindowPos — bypass the animator entirely.
                // Per-window result ignored: teleport is best-effort, and a
                // single failure must not abort the rest of the batch.
                let _ = crate::registry::win32::set_window_rect(
                    entry.window_id.0,
                    window_rect.x,
                    target_y,
                    window_rect.width,
                    window_rect.height,
                );

                // Teleport the border overlay to match. `set_geometry` calls
                // `SetWindowPos`, which triggers `WM_SIZE` → repaint if the size
                // changed, keeping the bitmap in sync on instant repositioning.
                // Option<&Window> is Copy so re-using `window` here is sound.
                if let Some(border) = window.and_then(|w| w.border.as_ref()) {
                    border.set_geometry(Rect {
                        x: entry.rect.x,
                        y: entry.rect.y + y_offset,
                        width: entry.rect.width,
                        height: entry.rect.height,
                    });
                }
            }
        }
    }
}

/// Convert an [`AppliedLayout`] into animation targets and submit to an animator.
///
/// This is a standalone version of [`FlowWM::animate_layout`] that
/// takes `&mut WindowAnimator` directly instead of `&mut FlowWM`.
/// Used during construction when `FlowWM` doesn't exist yet
/// but the animator needs to snap windows to their initial positions.
///
/// # Visible-Rect to Window-Rect Translation
///
/// Like [`FlowWM::animate_layout`], this function translates each
/// entry's rect from the layout engine's visible-rect space to Win32 window-rect
/// space using the window's stored
/// [`InvisibleBounds`](crate::common::InvisibleBounds). This is necessary even
/// during the initial snap so that windows land at the correct position from
/// the very first frame.
///
/// # Border Thickness
///
/// `border_thickness` and `border_overlap` are passed explicitly because this
/// function runs before `FlowWM` exists — the caller reads them
/// from the config and forwards them. The effective per-edge shrink is
/// `(thickness - overlap)`, same as
/// [`FlowWM::animate_layout`].
pub(super) fn animate_layout_raw(
    animator: &mut WindowAnimator,
    layout: &AppliedLayout,
    registry: &WindowRegistry,
    border_thickness: i32,
    border_overlap: i32,
) {
    if layout.actual_layout.entries.is_empty() {
        return;
    }

    let targets: Vec<WindowTarget> = layout
        .actual_layout
        .entries
        .iter()
        .flat_map(|entry| {
            let window = registry.get_window(HWND(entry.window_id.0 as *mut _));
            let invisible_bounds = window.map(|w| w.invisible_bounds).unwrap_or_default();
            let border_hwnd = window.and_then(|w| w.border.as_ref()).map(|b| b.hwnd());

            let window_rect = invisible_bounds
                .visible_to_window(entry.rect)
                .inset(border_thickness - border_overlap);

            let window_target = WindowTarget::new(
                WindowRef(entry.window_id.0),
                IVec2::new(window_rect.x, window_rect.y),
                IVec2::new(window_rect.width, window_rect.height),
            );

            // During initial construction borders are still None (they're
            // created afterwards in refresh_all_border_styles), so this
            // yields zero border targets on the first snap. Subsequent
            // calls (if any) would include them. See animate_layout.
            let border_target = border_hwnd.map(|b_hwnd| {
                WindowTarget::new(
                    WindowRef(b_hwnd),
                    IVec2::new(entry.rect.x, entry.rect.y),
                    IVec2::new(entry.rect.width, entry.rect.height),
                )
            });

            std::iter::once(window_target).chain(border_target)
        })
        .collect();

    if let Err(e) = animator.animate(targets) {
        log::warn!("animation error (initial snap): {e}");
    }
}
