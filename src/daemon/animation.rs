//! Animation bridge — converts layout snapshots to animation targets.
//!
//! This module contains:
//!
//! - [`ScrollTilingManager::animate_layout`] — converts an [`AppliedLayout`]
//!   into animation targets and submits to the animator.
//! - [`animate_layout_raw`] — standalone version used during construction when
//!   `ScrollTilingManager` doesn't exist yet.

use windows::Win32::Foundation::HWND;

use crate::animation::{IVec2, WindowAnimator, WindowRef, WindowTarget};
use crate::layout::types::AppliedLayout;
use crate::registry::WindowRegistry;

use super::types::ScrollTilingManager;

impl ScrollTilingManager {
    /// Convert an [`AppliedLayout`] into animation targets and submit to the animator.
    ///
    /// This is the critical conversion point between the layout engine's output
    /// (STM types) and the animation system's input (animation types):
    ///
    /// | STM Type | Animation Type |
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
    /// this mutation — fixing the "rapid swapcolumn stranding" bug.
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

        let targets: Vec<WindowTarget> = layout
            .actual_layout
            .entries
            .iter()
            .map(|entry| {
                // Look up this window's invisible bounds from the registry.
                // Falls back to zero bounds if the window is not tracked
                // (defensive — shouldn't happen in normal operation).
                let invisible_bounds = self
                    .registry
                    .get_window(HWND(entry.window_id.0 as *mut _))
                    .map(|w| w.invisible_bounds)
                    .unwrap_or_default();

                // Translate the layout engine's visible rect into a Win32
                // window rect. This compensates for invisible borders so
                // that SetWindowPos places the window's visible content
                // exactly where the layout engine intended.
                let window_rect = invisible_bounds.visible_to_window(entry.rect);

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

                WindowTarget::new(
                    WindowRef(entry.window_id.0),
                    IVec2::new(window_rect.x, window_rect.y),
                    IVec2::new(window_rect.width, window_rect.height),
                )
            })
            .collect();

        log::debug!(
            "animate_layout: submitting {} targets to animator",
            targets.len()
        );

        if let Err(e) = self.animator.animate(targets) {
            log::warn!("animation error: {e}");
        }
    }
}

/// Convert an [`AppliedLayout`] into animation targets and submit to an animator.
///
/// This is a standalone version of [`ScrollTilingManager::animate_layout`] that
/// takes `&mut WindowAnimator` directly instead of `&mut ScrollTilingManager`.
/// Used during construction when `ScrollTilingManager` doesn't exist yet
/// but the animator needs to snap windows to their initial positions.
///
/// # Visible-Rect to Window-Rect Translation
///
/// Like [`ScrollTilingManager::animate_layout`], this function translates each
/// entry's rect from the layout engine's visible-rect space to Win32 window-rect
/// space using the window's stored
/// [`InvisibleBounds`](crate::common::InvisibleBounds). This is necessary even
/// during the initial snap so that windows land at the correct position from
/// the very first frame.
pub(super) fn animate_layout_raw(
    animator: &mut WindowAnimator,
    layout: &AppliedLayout,
    registry: &WindowRegistry,
) {
    if layout.actual_layout.entries.is_empty() {
        return;
    }

    let targets: Vec<WindowTarget> = layout
        .actual_layout
        .entries
        .iter()
        .map(|entry| {
            let invisible_bounds = registry
                .get_window(HWND(entry.window_id.0 as *mut _))
                .map(|w| w.invisible_bounds)
                .unwrap_or_default();

            let window_rect = invisible_bounds.visible_to_window(entry.rect);

            WindowTarget::new(
                WindowRef(entry.window_id.0),
                IVec2::new(window_rect.x, window_rect.y),
                IVec2::new(window_rect.width, window_rect.height),
            )
        })
        .collect();

    if let Err(e) = animator.animate(targets) {
        log::warn!("animation error (initial snap): {e}");
    }
}
