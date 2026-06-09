//! Win32 backend: real DeferWindowPos + DwmFlush implementation.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetWindowRect, SWP_NOACTIVATE,
    SWP_NOZORDER,
};

use crate::animation::backend::WindowBackend;
use crate::animation::types::{AnimationError, Rect, Result, WindowRef};

/// Win32 implementation of [`WindowBackend`] using the deferred-window-position
/// API for atomic batch moves and `DwmFlush` for frame-paced animation.
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

    /// Atomically reposition all windows in `updates` using the Win32
    /// deferred-window-position API.
    ///
    /// The sequence is:
    /// 1. [`BeginDeferWindowPos`] — allocate a handle for `n` moves.
    /// 2. [`DeferWindowPos`] × n — queue each `(WindowRef, Rect)` pair.
    /// 3. [`EndDeferWindowPos`] — flush all moves atomically.
    fn apply_batch(&self, updates: &[(WindowRef, Rect)]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        // Allocate the deferred-window-position handle for this batch.
        let mut current_hdwp = unsafe { BeginDeferWindowPos(updates.len() as i32) }
            .map_err(|e| AnimationError::Backend(e.message().to_string()))?;

        for (window, rect) in updates {
            // DeferWindowPos may return a new (reallocated) handle — always use the
            // latest returned value for subsequent calls.
            let result = unsafe {
                DeferWindowPos(
                    current_hdwp,
                    HWND(window.0 as _),
                    None, // hwnd_insert_after: preserve Z-order
                    rect.x,
                    rect.y,
                    rect.w,
                    rect.h,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };

            match result {
                Ok(h) => current_hdwp = h,
                Err(e) => {
                    // Flush whatever handle we have to avoid leaking the HDWP resource.
                    let _ = unsafe { EndDeferWindowPos(current_hdwp) };
                    return Err(AnimationError::Backend(e.message().to_string()));
                }
            }
        }

        // Flush all queued moves atomically in a single WM_WINDOWPOSCHANGED round.
        unsafe { EndDeferWindowPos(current_hdwp) }
            .map_err(|e| AnimationError::Backend(e.message().to_string()))
    }

    /// Block until the Desktop Window Manager completes its current composition
    /// cycle, synchronising the animation loop with the display refresh rate.
    fn dwm_flush(&self) -> Result<()> {
        unsafe { DwmFlush() }.map_err(|e| AnimationError::Backend(e.message().to_string()))
    }
}
