//! Win32 backend: individual `SetWindowPos` calls + `DwmFlush` implementation.
//!
//! Each window in an animation batch is repositioned via its own
//! [`SetWindowPos`] call rather than the batch `DeferWindowPos` API.
//! This avoids the UIPI problem where a single higher-integrity window
//! (e.g. an admin-elevation UAC prompt) causes the *entire* deferred batch
//! to fail with `ERROR_ACCESS_DENIED`.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos,
};

use crate::animation::backend::WindowBackend;
use crate::animation::types::{AnimationError, Rect, Result, WindowRef};

/// Win32 implementation of [`WindowBackend`] using individual
/// [`SetWindowPos`] calls for per-window moves and [`DwmFlush`] for
/// frame-paced animation.
///
/// # Why not `DeferWindowPos`?
///
/// The Win32 `BeginDeferWindowPos` → `DeferWindowPos` × N → `EndDeferWindowPos`
/// batch API is atomic: if **any** window in the batch is owned by a
/// higher-integrity process, the UIPI (User Interface Privilege Isolation)
/// mechanism rejects the *entire* batch with `ERROR_ACCESS_DENIED`. This
/// means a single elevated admin window prevents every other window in the
/// batch from being moved.
///
/// By calling [`SetWindowPos`] individually for each window we ensure that a
/// single UIPI denial only logs a warning and does not block the remaining
/// windows in the same animation frame.
#[allow(dead_code)] // Not yet consumed by other crate modules; will be used by compositor.
pub struct Win32Backend;

#[allow(dead_code)] // Not yet consumed by other crate modules; will be used by compositor.
impl Win32Backend {
    /// Create a new [`Win32Backend`].
    pub fn new() -> Self {
        Self
    }
}

impl Default for Win32Backend {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowBackend for Win32Backend {
    /// Query the current screen rectangle of the given window.
    ///
    /// Calls [`GetWindowRect`] and converts the Win32 `{left, top, right, bottom}`
    /// representation to the crate's `{x, y, w, h}` form.
    fn get_window_rect(&self, window: WindowRef) -> Result<Rect> {
        let mut rect = RECT::default();
        unsafe { GetWindowRect(HWND(window.0 as _), &mut rect) }
            .map_err(|e| AnimationError::Backend(e.message().to_string()))?;
        Ok(Rect {
            x: rect.left,
            y: rect.top,
            w: rect.right - rect.left,
            h: rect.bottom - rect.top,
        })
    }

    /// Reposition every window in `updates` using individual [`SetWindowPos`]
    /// calls.
    ///
    /// # Resilience vs. the batch API
    ///
    /// The previous implementation used `BeginDeferWindowPos` → `DeferWindowPos`
    /// × N → `EndDeferWindowPos`. That API is atomic: if *any* window is
    /// protected by UIPI (e.g. an elevated admin process), the whole batch
    /// fails with `ERROR_ACCESS_DENIED` and no windows move at all.
    ///
    /// With per-window [`SetWindowPos`]:
    /// - Each failure is logged at `warn` level but does **not** prevent the
    ///   remaining windows from being moved.
    /// - The first error encountered is returned so callers can still react
    ///   to partial failures (e.g. retry on the next frame).
    /// - Every window in `updates` is attempted, regardless of earlier errors.
    ///
    /// # Z-order preservation
    ///
    /// The `hwnd_insert_after` parameter is `None` combined with the
    /// [`SWP_NOZORDER`] flag, which means the existing Z-order of each
    /// window is left unchanged.
    fn apply_batch(&self, updates: &[(WindowRef, Rect)]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        let mut first_error = None;
        for (window, rect) in updates {
            let result = unsafe {
                SetWindowPos(
                    HWND(window.0 as _),
                    None, // hwnd_insert_after: preserve Z-order
                    rect.x,
                    rect.y,
                    rect.w,
                    rect.h,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };

            if let Err(e) = result {
                log::warn!("SetWindowPos failed for window {:?}: {e}", window);
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }

        // Return the first error encountered, if any.
        // Individual failures are logged but don't prevent other windows from being moved.
        match first_error {
            Some(e) => Err(AnimationError::Backend(e.message().to_string())),
            None => Ok(()),
        }
    }

    /// Block until the Desktop Window Manager completes its current composition
    /// cycle, synchronising the animation loop with the display refresh rate.
    fn dwm_flush(&self) -> Result<()> {
        unsafe { DwmFlush() }.map_err(|e| AnimationError::Backend(e.message().to_string()))
    }
}
