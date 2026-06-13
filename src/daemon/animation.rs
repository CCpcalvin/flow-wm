//! Animation bridge — converts layout diffs to animation targets.
//!
//! This module contains:
//!
//! - [`ScrollTilingManager::animate_diff`] — converts a [`LayoutDiff`] into
//!   animation targets and submits to the animator.
//! - [`animate_diff_raw`] — standalone version used during construction when
//!   `ScrollTilingManager` doesn't exist yet.

use windows::Win32::Foundation::HWND;

use crate::animation::{IVec2, WindowAnimator, WindowRef, WindowTarget};
use crate::layout::types::LayoutDiff;
use crate::registry::WindowRegistry;

use super::types::ScrollTilingManager;

impl ScrollTilingManager {
    /// Convert a [`LayoutDiff`] into animation targets and submit to the animator.
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
    /// If the diff contains no moves, the animation step is skipped but the
    /// registry sync still runs. Animation errors are logged as warnings but
    /// not propagated — a jarring animation is better than a crash.
    ///
    /// # Visible-Rect to Window-Rect Translation
    ///
    /// The layout engine computes **visible rects** — the coordinates of the
    /// content area the user actually sees. However, `SetWindowPos` (called by
    /// the Win32 animation backend) positions windows using **window rects** —
    /// the full rect from `GetWindowRect` which includes invisible borders
    /// (shadows, resize hit-test areas).
    ///
    /// This method translates each target's `to` position from visible-rect
    /// space to window-rect space using the window's stored
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
    pub(super) fn animate_diff(&mut self, diff: &LayoutDiff) {
        // Always sync registry state from the new layout, even if no windows
        // physically moved (col/row indices may have changed).
        self.registry
            .update_tiling_slots_from_layout(&diff.virtual_layout);
        self.registry.update_tiled_rects(&diff.actual_layout);

        if diff.moves.is_empty() {
            return;
        }

        let targets: Vec<WindowTarget> = diff
            .moves
            .iter()
            .map(|wm| {
                // Look up this window's invisible bounds from the registry.
                // Falls back to zero bounds if the window is not tracked
                // (defensive — shouldn't happen in normal operation).
                let invisible_bounds = self
                    .registry
                    .get_window(HWND(wm.window_id.0 as *mut _))
                    .map(|w| w.invisible_bounds)
                    .unwrap_or_default();

                // Translate the layout engine's visible rect into a Win32
                // window rect. This compensates for invisible borders so
                // that SetWindowPos places the window's visible content
                // exactly where the layout engine intended.
                let window_rect = invisible_bounds.visible_to_window(wm.to);

                log::debug!(
                    "animate: hwnd={} from ({},{},{},{}) to ({},{},{},{}) [visible] \
                     → ({},{},{},{}) [window] hint={:?}",
                    wm.window_id.0,
                    wm.from.x,
                    wm.from.y,
                    wm.from.width,
                    wm.from.height,
                    wm.to.x,
                    wm.to.y,
                    wm.to.width,
                    wm.to.height,
                    window_rect.x,
                    window_rect.y,
                    window_rect.width,
                    window_rect.height,
                    wm.hint,
                );

                WindowTarget::new(
                    WindowRef(wm.window_id.0),
                    IVec2::new(window_rect.x, window_rect.y),
                    IVec2::new(window_rect.width, window_rect.height),
                )
            })
            .collect();

        log::debug!(
            "animate_diff: submitting {} targets to animator",
            targets.len()
        );

        if let Err(e) = self.animator.animate(targets) {
            log::warn!("animation error: {e}");
        }
    }
}

/// Convert a [`LayoutDiff`] into animation targets and submit to an animator.
///
/// This is a standalone version of [`ScrollTilingManager::animate_diff`] that
/// takes `&mut WindowAnimator` directly instead of `&mut ScrollTilingManager`.
/// Used during construction when `ScrollTilingManager` doesn't exist yet
/// but the animator needs to snap windows to their initial positions.
///
/// # Visible-Rect to Window-Rect Translation
///
/// Like [`ScrollTilingManager::animate_diff`], this function translates each
/// move's `to` position from the layout engine's visible-rect space to
/// Win32 window-rect space using the window's stored
/// [`InvisibleBounds`](crate::common::InvisibleBounds). This is necessary
/// even during the initial snap so that windows land at the correct position
/// from the very first frame.
pub(super) fn animate_diff_raw(
    animator: &mut WindowAnimator,
    diff: &LayoutDiff,
    registry: &WindowRegistry,
) {
    if diff.moves.is_empty() {
        return;
    }

    let targets: Vec<WindowTarget> = diff
        .moves
        .iter()
        .map(|wm| {
            let invisible_bounds = registry
                .get_window(HWND(wm.window_id.0 as *mut _))
                .map(|w| w.invisible_bounds)
                .unwrap_or_default();

            let window_rect = invisible_bounds.visible_to_window(wm.to);

            WindowTarget::new(
                WindowRef(wm.window_id.0),
                IVec2::new(window_rect.x, window_rect.y),
                IVec2::new(window_rect.width, window_rect.height),
            )
        })
        .collect();

    if let Err(e) = animator.animate(targets) {
        log::warn!("animation error (initial snap): {e}");
    }
}
