//! Backend abstraction layer for window management operations.
//!
//! The [`WindowBackend`] trait decouples the animation engine from Win32 so
//! that tests can inject a [`mock::MockBackend`] without touching real windows.
//!
//! # Backends
//!
//! | Backend | When compiled |
//! |---------|---------------|
//! | [`mock`] | always (part of the public API; no feature gate required) |
//! | `win32` | `cfg(target_os = "windows")` |

/// Mock backend — always compiled so that both inline unit tests (`cfg(test)`)
/// **and** integration tests in `tests/` (which are separate compilation units
/// where `cfg(test)` is NOT set for the library) can import it freely.
///
/// The `testing` feature flag in `Cargo.toml` is retained for backward
/// compatibility but is now a no-op.
pub mod mock;

#[cfg(target_os = "windows")]
pub mod win32;

use crate::animation::types::{Rect, Result, WindowRef};

/// Abstraction over all Win32 calls required by the animation engine.
///
/// Implementors must be `Send + 'static` so the backend can be moved into
/// the background worker thread.
pub trait WindowBackend: Send + 'static {
    /// Query the current screen rectangle of a window.
    ///
    /// Wraps [`GetWindowRect`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getwindowrect),
    /// which fills a `RECT` with `{left, top, right, bottom}` in screen
    /// coordinates. The Win32 backend converts to the crate's `(x, y, w, h)`
    /// form internally. Called during retargeting to determine the current
    /// interpolated position when an animation is interrupted.
    fn get_window_rect(&self, window: WindowRef) -> Result<Rect>;

    /// Reposition all windows in `updates` via individual [`SetWindowPos`]
    /// calls.
    ///
    /// Each window is moved independently so that a failure on one window
    /// (e.g. UIPI access denied for an elevated process) does not prevent
    /// the remaining windows from being repositioned. Per-window errors are
    /// logged at `warn` level; the first error encountered is returned, but
    /// every window in the batch is attempted regardless.
    ///
    /// Wraps
    /// [`SetWindowPos`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowpos)
    /// for the Win32 backend. The `SWP_NOZORDER` flag preserves each
    /// window's existing Z-order.
    fn apply_batch(&self, updates: &[(WindowRef, Rect)]) -> Result<()>;

    /// Block until the Desktop Window Manager (DWM) completes its current
    /// composition cycle.
    ///
    /// Wraps [`DwmFlush`](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/nf-dwmapi-dwmflush).
    /// Calling this after [`apply_batch`](WindowBackend::apply_batch) ensures
    /// the animation loop runs in sync with the display refresh rate, producing
    /// frame-paced output without a busy-spin or a fixed `sleep` interval.
    fn dwm_flush(&self) -> Result<()>;
}
