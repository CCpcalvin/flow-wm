//! Per-window border overlay (layered, click-through, topmost).
//!
//! A [`Border`] is a thin colored ring drawn just inside a managed window's
//! visible content. Unlike the previous design (which gave each border its own
//! `EVENT_OBJECT_LOCATIONCHANGE` hook and queried the target's `GetWindowRect`),
//! a [`Border`] never queries the *target* window's position — the daemon
//! *commands* the geometry via [`Border::set_geometry`]. This removes the
//! duplicate hook and fixes the misalignment bug: the ring sits at the
//! visible-content edge because the daemon passes the visible rect directly.
//!
//! When recoloring via [`Border::set_style`], the border queries its *own*
//! overlay position (`GetWindowRect` on the overlay HWND, not the target). This
//! is necessary because the animator moves overlays via `SetWindowPos` without
//! going through `set_geometry`, so the overlay's actual position is the only
//! source of truth at repaint time.
//!
//! See `docs/src/dev-guide/borders.md` for the threading model and the
//! "daemon commands, border obeys" principle.

use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, GetWindowRect, RegisterClassExW,
    SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOSENDCHANGING, SWP_NOZORDER, SetWindowPos,
    ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;
use windows::core::w;

use super::style::BorderStyle;
use crate::common::Rect;
use crate::config::Color;

// ── Window class registration ─────────────────────────────────────────

/// Window class name used for all border overlays. Registered once per
/// process via [`ensure_window_class_registered`].
const OVERLAY_CLASS_NAME: PCWSTR = w!("STMBorderOverlay");

/// Stores the class atom once `RegisterClassExW` succeeds. Subsequent calls
/// return the cached atom without touching Win32.
static OVERLAY_CLASS_ATOM: OnceLock<u16> = OnceLock::new();

/// Minimal window procedure for the overlay class.
///
/// Overlays are click-through (`WS_EX_TRANSPARENT`) and draw via
/// `UpdateLayeredWindow` (no `WM_PAINT`), so this delegates everything to
/// `DefWindowProcW`. A custom proc is still required so the class can be
/// registered; passing `None` is rejected by `RegisterClassExW`.
///
/// # Safety
///
/// Called by Windows. Must match the `WindowProc` signature exactly.
unsafe extern "system" fn overlay_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: DefWindowProcW is sound for arbitrary hwnd/msg/wparam/lparam;
    // we pass through unchanged. The unsafe block is required by edition 2024.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Lock held during the actual `RegisterClassExW` call so two racing threads
/// do not both see "class not registered" and both attempt registration
/// (the second would fail with "class already exists").
static OVERLAY_CLASS_LOCK: Mutex<()> = Mutex::new(());

/// Register the overlay window class. Idempotent.
///
/// # Errors
///
/// Returns a human-readable `String` if `GetModuleHandleW` or
/// `RegisterClassExW` fails. Subsequent calls after a successful registration
/// are infallible.
fn ensure_window_class_registered() -> Result<(), String> {
    if OVERLAY_CLASS_ATOM.get().is_some() {
        return Ok(());
    }
    // Double-checked locking: hold the lock only for the actual registration
    // so the common (already-registered) path is lock-free.
    let _guard = OVERLAY_CLASS_LOCK.lock().expect("class lock poisoned");
    if OVERLAY_CLASS_ATOM.get().is_some() {
        return Ok(());
    }
    // SAFETY: GetModuleHandleW(NULL) returns the EXE module handle of the
    // calling process. Always safe to call.
    let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()) }
        .map_err(|e| format!("GetModuleHandleW failed: {e}"))?;
    let wcex = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(overlay_wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: OVERLAY_CLASS_NAME,
        ..Default::default()
    };
    // SAFETY: RegisterClassExW reads from a local WNDCLASSEXW. The class
    // name and proc are static; the struct is fully populated.
    let atom = unsafe { RegisterClassExW(&wcex) };
    if atom == 0 {
        return Err("RegisterClassExW returned 0".to_owned());
    }
    let _ = OVERLAY_CLASS_ATOM.set(atom);
    Ok(())
}

// ── Border ──────────────────────────────────────────────────────────

/// A single border overlay drawn around one managed window's visible content.
///
/// The overlay is a click-through, topmost, layered window. The daemon
/// *commands* its geometry via [`set_geometry`](Self::set_geometry) — the
/// border never queries the target window's position itself. This is the
/// key difference from the old `BorderOverlay`, which ran a private
/// `EVENT_OBJECT_LOCATIONCHANGE` hook and called `GetWindowRect(target)`.
///
/// Lifecycle: created by [`Border::create`], repositioned by
/// [`set_geometry`](Self::set_geometry), recolored by
/// [`set_style`](Self::set_style), shown/hidden by
/// [`set_visible`](Self::set_visible), and torn down when the last clone
/// drops (also via [`destroy`](Self::destroy)).
///
/// # Cloning & `Drop`
///
/// [`Border`] is [`Clone`] (cheap — bumps an `Arc` refcount). All clones
/// share the same overlay HWND, so `DestroyWindow` runs exactly once, when
/// the last clone is dropped. This keeps `Window: Clone` sound while
/// guaranteeing the overlay is destroyed when the window leaves the registry.
///
/// # Win32 handle storage
///
/// The overlay HWND is stored as `isize` (not `HWND`) so the struct is
/// `Send`. `HWND` itself is `!Send` because it wraps a raw pointer.
#[derive(Debug, Clone)]
pub struct Border {
    inner: Arc<BorderInner>,
}

/// Shared mutable state behind [`Border`]'s `Arc`.
///
/// All fields are behind `Mutex` so [`Border`]'s methods can mutate state
/// through `&self` (the struct is reached via `Arc` clones held by the
/// registry's `Window`). In practice all access happens on the single IPC
/// thread, so the mutexes are uncontended — they exist purely for
/// interior mutability, not cross-thread synchronization.
#[derive(Debug)]
struct BorderInner {
    /// Overlay window HWND. `0` after [`destroy`](Border::destroy) / drop.
    overlay: Mutex<isize>,
    /// Current style (color + thickness).
    style: Mutex<BorderStyle>,
}

impl Border {
    /// Create a new border overlay window with the given style.
    ///
    /// The overlay is created at `(0,0)` with a 1×1 logical size and then
    /// shown. Until [`set_geometry`](Self::set_geometry) uploads a bitmap
    /// via `UpdateLayeredWindow`, a `WS_EX_LAYERED` window renders nothing,
    /// so nothing appears on screen. The daemon is expected to call
    /// `set_geometry` shortly after creation to position and paint the ring.
    ///
    /// # Errors
    ///
    /// Returns a human-readable `String` if the window class could not be
    /// registered or `CreateWindowExW` fails.
    pub(crate) fn create(style: BorderStyle) -> Result<Self, String> {
        ensure_window_class_registered()?;
        let ex_style = WINDOW_EX_STYLE(
            WS_EX_LAYERED.0
                | WS_EX_TRANSPARENT.0
                | WS_EX_TOPMOST.0
                | WS_EX_NOACTIVATE.0
                | WS_EX_TOOLWINDOW.0,
        );
        let style_flags = WINDOW_STYLE(WS_POPUP.0);
        // SAFETY: CreateWindowExW creates a top-level layered window. The
        // class is registered above; OVERLAY_CLASS_NAME is a static PCWSTR.
        // A 1×1 hidden layered window is harmless until set_geometry runs.
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                OVERLAY_CLASS_NAME,
                w!(""),
                style_flags,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .map_err(|e| format!("CreateWindowExW failed: {e}"))?;

        let border = Self {
            inner: Arc::new(BorderInner {
                overlay: Mutex::new(hwnd.0 as isize),
                style: Mutex::new(style),
            }),
        };
        // Reveal the overlay. Until set_geometry paints a bitmap the layered
        // surface is fully transparent, so this never flashes on screen.
        border.set_visible(true);
        Ok(border)
    }

    /// Returns the overlay window's HWND value, or `0` if destroyed.
    ///
    /// Used by the daemon to flatten the overlay into the animator's target
    /// list as a `WindowRef` (border HWNDs are real windows, so the
    /// animator's `SetWindowPos`-based backend moves them like any other).
    #[must_use]
    pub(crate) fn hwnd(&self) -> isize {
        *self.inner.overlay.lock().expect("overlay mutex poisoned")
    }
}

impl Drop for BorderInner {
    fn drop(&mut self) {
        let mut guard = self.overlay.lock().expect("overlay mutex poisoned");
        let raw = *guard;
        if raw == 0 {
            return;
        }
        // SAFETY: `raw` came from a valid HWND created in `Border::create`.
        // After DestroyWindow the handle is invalid; we clear it under the
        // lock so re-entrant calls (or a stray clone dropping later) no-op.
        let hwnd = HWND(raw as *mut _);
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd) };
        *guard = 0;
    }
}

// ── Geometry, style, visibility ─────────────────────────────────────

impl Border {
    /// Command the overlay to cover `visible_rect` and repaint the ring.
    ///
    /// This is the daemon-driven replacement for the old hook-driven
    /// `sync_geometry`: instead of querying `GetWindowRect(target)`, the
    /// caller passes the visible-content rect directly. `visible_rect` is
    /// in the same coordinate space as the layout engine's output (visible
    /// pixels), so the ring sits exactly at the visible-content edge —
    /// fixing the previous misalignment where it sat over the invisible
    /// resize border.
    ///
    /// Performs both `SetWindowPos` (move + resize the overlay HWND) and
    /// `UpdateLayeredWindow` (rebuild the ring bitmap). Safe to call with
    /// a destroyed overlay (no-op) or a zero-area rect (early return).
    ///
    /// Note: the commanded rect is *not* cached. [`set_style`](Self::set_style)
    /// queries the overlay's actual position at repaint time, which stays
    /// correct even after the animator moves the overlay via `SetWindowPos`.
    pub(crate) fn set_geometry(&self, visible_rect: Rect) {
        if visible_rect.is_empty() {
            return;
        }
        let raw = *self.inner.overlay.lock().expect("overlay mutex poisoned");
        if raw == 0 {
            return;
        }
        let overlay_hwnd = HWND(raw as *mut _);
        // SAFETY: SetWindowPos on our own overlay window with NOACTIVATE |
        // NOZORDER | NOSENDCHANGING. NOSENDCHANGING avoids re-entrant
        // WM_WINDOWPOSCHANGING callbacks. We deliberately do NOT use
        // SWP_ASYNCWINDOWPOS: this runs on the IPC thread (not a hook
        // thread), so synchronous dispatch is correct and avoids queueing.
        let _ = unsafe {
            SetWindowPos(
                overlay_hwnd,
                None,
                visible_rect.x,
                visible_rect.y,
                visible_rect.width,
                visible_rect.height,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOSENDCHANGING,
            )
        };
        self.paint(visible_rect);
    }

    /// Query the overlay's current screen rect via `GetWindowRect`.
    ///
    /// Used by [`set_style`](Self::set_style) to repaint at the overlay's
    /// *actual* position — which may differ from the last rect commanded via
    /// [`set_geometry`](Self::set_geometry) because the animator moves overlays
    /// via `SetWindowPos` without going through `set_geometry`. Querying the
    /// overlay itself (not the target window) is always correct: the animator
    /// is what put it where it is.
    ///
    /// Returns `None` if the overlay is destroyed or `GetWindowRect` fails
    /// (e.g. during shutdown races); the caller treats that as a no-op paint.
    fn overlay_rect(&self) -> Option<Rect> {
        let raw = *self.inner.overlay.lock().expect("overlay mutex poisoned");
        if raw == 0 {
            return None;
        }
        let hwnd = HWND(raw as *mut _);
        let mut rect = RECT::default();
        // SAFETY: GetWindowRect on our own overlay window, filling a local
        // RECT by reference. Harmless if the window has been destroyed
        // (returns an error, which we map to None).
        if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
            return None;
        }
        Some(Rect {
            x: rect.left,
            y: rect.top,
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
        })
    }

    /// Update the border color/thickness and repaint at the overlay's current
    /// position.
    ///
    /// Does not move the overlay — only rebuilds the ring bitmap with the new
    /// style. The repaint position is queried from the overlay HWND itself
    /// (via [`overlay_rect`](Self::overlay_rect)), so it stays correct even
    /// after the animator has moved the overlay via `SetWindowPos`. If the
    /// overlay is destroyed or its rect cannot be queried, this is a no-op.
    pub(crate) fn set_style(&self, style: BorderStyle) {
        *self.inner.style.lock().expect("style mutex poisoned") = style;
        let Some(rect) = self.overlay_rect() else {
            return;
        };
        self.paint(rect);
    }

    /// Show or hide the overlay (used for minimize/restore).
    pub(crate) fn set_visible(&self, visible: bool) {
        let raw = *self.inner.overlay.lock().expect("overlay mutex poisoned");
        if raw == 0 {
            return;
        }
        let hwnd = HWND(raw as *mut _);
        let cmd = if visible { SW_SHOWNOACTIVATE } else { SW_HIDE };
        // SAFETY: ShowWindow is sound for any HWND value; we own this one.
        let _ = unsafe { ShowWindow(hwnd, cmd) };
    }

    /// Rebuild the ring bitmap for `rect` and upload it via
    /// `UpdateLayeredWindow`. The overlay HWND is positioned separately by
    /// [`set_geometry`](Self::set_geometry); this only updates pixels.
    fn paint(&self, rect: Rect) {
        if rect.is_empty() {
            return;
        }
        let raw = *self.inner.overlay.lock().expect("overlay mutex poisoned");
        if raw == 0 {
            return;
        }
        let overlay_hwnd = HWND(raw as *mut _);
        let target_rect = RECT {
            left: rect.x,
            top: rect.y,
            right: rect.right(),
            bottom: rect.bottom(),
        };
        paint_ring(
            overlay_hwnd,
            &target_rect,
            *self.inner.style.lock().expect("style mutex poisoned"),
        );
    }
}

// ── Layered bitmap rendering ────────────────────────────────────────

/// Rebuild the colored-ring bitmap for `overlay_hwnd` at `target_rect` and
/// upload it via `UpdateLayeredWindow` (`ULW_ALPHA`).
///
/// The bitmap is `width × height` 32-bit ARGB with the outer `thickness`-px
/// ring set to `style.color` and the interior fully transparent. Every
/// `Create*` GDI call is paired with its `Delete*`; all resource lifetimes
/// are confined to this function so nothing leaks across paints.
///
/// Failures are reported at `trace` level (expected during shutdown races)
/// rather than propagated: a missed paint frame is cosmetic, not fatal.
fn paint_ring(overlay_hwnd: HWND, target_rect: &RECT, style: BorderStyle) {
    let w = (target_rect.right - target_rect.left).max(0);
    let h = (target_rect.bottom - target_rect.top).max(0);
    if w == 0 || h == 0 {
        return;
    }
    let thickness = style.width_px as usize;

    // SAFETY: All GDI calls below are well-formed. We pair every Create*
    // with its matching Delete*; the DC/bitmap lifetimes are confined to
    // this function so they cannot leak across redraws.
    unsafe {
        // Screen DC for CreateCompatibleDC; released immediately.
        let hdc_screen = GetDC(None);
        if hdc_screen.is_invalid() {
            log::trace!("borders: GetDC failed for screen");
            return;
        }
        let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
        let _ = ReleaseDC(None, hdc_screen);
        if hdc_mem.is_invalid() {
            log::trace!("borders: CreateCompatibleDC failed");
            return;
        }

        // Top-down 32-bit ARGB DIB section (negative biHeight = top-down).
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: (w as u32) * (h as u32) * 4,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [Default::default()],
        };

        let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let bitmap =
            match CreateDIBSection(Some(hdc_mem), &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0) {
                Ok(b) => b,
                Err(e) => {
                    log::trace!("borders: CreateDIBSection failed: {e}");
                    let _ = DeleteDC(hdc_mem);
                    return;
                }
            };
        if bits_ptr.is_null() {
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(hdc_mem);
            return;
        }

        // Populate pixels: outer thickness-px ring colored, interior 0.
        let len = (w as usize) * (h as usize);
        let pixels: *mut u32 = bits_ptr as *mut u32;
        std::ptr::write_bytes(pixels, 0, len);
        // SAFETY: CreateDIBSection sized the buffer to len u32s; the
        // slice borrow is confined to the fill call and not aliased.
        let pixels_slice: &mut [u32] = std::slice::from_raw_parts_mut(pixels, len);
        let colored = pack_bgra(style.color, 0xFF);
        fill_border_ring(pixels_slice, w as usize, h as usize, thickness, colored);

        // Select the bitmap into the memory DC for UpdateLayeredWindow.
        let old_obj = SelectObject(hdc_mem, bitmap.into());

        let pt_dst = POINT {
            x: target_rect.left,
            y: target_rect.top,
        };
        let pt_src = POINT { x: 0, y: 0 };
        let size = SIZE { cx: w, cy: h };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // ULW_ALPHA: use per-pixel alpha from the source DC.
        let result = UpdateLayeredWindow(
            overlay_hwnd,
            None,
            Some(&pt_dst),
            Some(&size),
            Some(hdc_mem),
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        // Restore + release GDI resources regardless of outcome.
        SelectObject(hdc_mem, old_obj);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(hdc_mem);

        if result.is_err() {
            log::trace!("borders: UpdateLayeredWindow failed");
        }
    }
}

// ── Pure helpers (testable without Win32) ─────────────────────────────

/// Pack an RGB [`Color`] + alpha into a single `u32` in the BGRA byte order
/// expected by a 32-bit `BI_RGB` DIB section in memory.
///
/// DIB sections with `biBitCount = 32` and `biCompression = BI_RGB` lay each
/// pixel out as `(blue, green, red, alpha)` from low to high address. Read as
/// a little-endian `u32`, that is `(alpha << 24) | (red << 16) | (green << 8) | blue`.
#[must_use]
pub(crate) const fn pack_bgra(color: Color, alpha: u8) -> u32 {
    let r = color.r as u32;
    let g = color.g as u32;
    let b = color.b as u32;
    let a = alpha as u32;
    (a << 24) | (r << 16) | (g << 8) | b
}

/// Fill the outer `thickness`-pixel ring of a `width * height` pixel buffer
/// with `color`, leaving the interior transparent (`0`).
///
/// Used to render the border ring into a freshly zeroed DIB section. The
/// caller must size `pixels` to exactly `width * height`. Thickness is
/// clamped against half the smaller dimension so an oversized value just
/// fills the whole buffer (no panic).
///
/// # Panics
///
/// Panics if `pixels.len() != width * height`.
pub(crate) fn fill_border_ring(
    pixels: &mut [u32],
    width: usize,
    height: usize,
    thickness: usize,
    color: u32,
) {
    assert_eq!(
        pixels.len(),
        width * height,
        "pixels buffer must be exactly width * height"
    );
    if width == 0 || height == 0 {
        return;
    }
    // Clamp thickness so it never exceeds half the smaller dimension. If it
    // does, the "ring" covers the whole image, which is well-defined.
    let half = (width.min(height)) / 2;
    let t = thickness.min(half);
    if t == 0 {
        return;
    }

    let last_row = height - t;
    let last_col = width - t;
    for y in 0..height {
        let row_start = y * width;
        let is_border_row = y < t || y >= last_row;
        if is_border_row {
            // Whole row colored.
            pixels[row_start..row_start + width].fill(color);
        } else {
            // Left edge then right edge; middle stays 0 (already transparent).
            pixels[row_start..row_start + t].fill(color);
            pixels[row_start + last_col..row_start + width].fill(color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color(r: u8, g: u8, b: u8) -> Color {
        Color::rgb(r, g, b)
    }

    #[test]
    fn pack_bgra_orders_channels_as_blue_green_red_alpha() {
        // Memory layout is BGRA (low→high byte). Read as LE u32 that's
        // (a << 24) | (r << 16) | (g << 8) | b.
        let packed = pack_bgra(color(0xFF, 0x88, 0x00), 0xAA);
        assert_eq!(packed, 0xAA_FF_88_00);
    }

    #[test]
    fn pack_bgra_full_alpha_for_opaque() {
        let packed = pack_bgra(color(0x12, 0x34, 0x56), 0xFF);
        assert_eq!(packed & 0xFF00_0000, 0xFF00_0000);
    }

    #[test]
    fn fill_border_ring_zero_thickness_leaves_buffer_transparent() {
        let mut buf = vec![0u32; 9];
        fill_border_ring(&mut buf, 3, 3, 0, 0xFFFF00FF);
        assert!(buf.iter().all(|&p| p == 0));
    }

    #[test]
    fn fill_border_ring_one_pixel_ring_on_3x3_leaves_only_center_transparent() {
        // 3×3 with t=1: outer 8 pixels colored, center (idx 4) transparent.
        // Index layout (row*3 + col): 0 1 2 / 3 4 5 / 6 7 8.
        let mut buf = vec![0u32; 9];
        let colored = pack_bgra(color(0, 0, 0), 0xFF);
        fill_border_ring(&mut buf, 3, 3, 1, colored);
        assert_eq!(buf[0], colored);
        assert_eq!(buf[1], colored);
        assert_eq!(buf[2], colored);
        assert_eq!(buf[3], colored);
        assert_eq!(buf[4], 0, "center must stay transparent");
        assert_eq!(buf[5], colored);
        assert_eq!(buf[6], colored);
        assert_eq!(buf[7], colored);
        assert_eq!(buf[8], colored);
    }

    #[test]
    fn fill_border_ring_two_pixel_ring_on_5x5_colors_outer_two_layers() {
        // 5×5 with t=2: outer 2 rings colored, center (idx 12) transparent.
        // Row start indices: 0, 5, 10, 15, 20.
        let mut buf = vec![0u32; 25];
        let colored = pack_bgra(color(0, 0, 0), 0xFF);
        fill_border_ring(&mut buf, 5, 5, 2, colored);
        // Rows 0,1,3,4 fully colored; row 2 has cols 0,1,3,4 colored, col 2 transparent.
        for x in 0..5 {
            assert_eq!(buf[x], colored, "row 0 col {x}");
            assert_eq!(buf[5 + x], colored, "row 1 col {x}");
            assert_eq!(buf[15 + x], colored, "row 3 col {x}");
            assert_eq!(buf[20 + x], colored, "row 4 col {x}");
        }
        assert_eq!(buf[10], colored);
        assert_eq!(buf[11], colored);
        assert_eq!(buf[12], 0, "exact center must stay transparent");
        assert_eq!(buf[13], colored);
        assert_eq!(buf[14], colored);
    }

    #[test]
    fn fill_border_ring_oversized_thickness_clamps_to_half_dimension() {
        // 4×4 with thickness=100: clamps to t=2 (half of min(4,4)).
        // Whole image is "ring" — every pixel colored.
        let mut buf = vec![0u32; 16];
        let colored = pack_bgra(color(0, 0, 0), 0xFF);
        fill_border_ring(&mut buf, 4, 4, 100, colored);
        assert!(buf.iter().all(|&p| p == colored));
    }

    #[test]
    fn fill_border_ring_empty_dimensions_is_noop() {
        let mut buf: Vec<u32> = vec![];
        fill_border_ring(&mut buf, 0, 0, 5, 0xFFFF00FF);
    }

    // ── pack_bgra edge cases ───────────────────────────────────────────

    /// Negative edge case: a fully-transparent alpha (0) leaves the top byte
    /// clear. The BGRA layout reads alpha as the high byte, so a 0 alpha must
    /// produce a value whose top byte is 0x00 regardless of the RGB channels.
    #[test]
    fn pack_bgra_zero_alpha_is_fully_transparent() {
        let packed = pack_bgra(color(0xFF, 0xFF, 0xFF), 0x00);
        assert_eq!(packed & 0xFF00_0000, 0, "alpha byte must be zero");
        // RGB channels are still packed in the low bytes.
        assert_eq!(packed, 0x00_FF_FF_FF);
    }

    /// Positive edge case: opaque black packs to `0xFF_00_00_00` — alpha full,
    /// all color channels zero. Guards against an accidental channel swap that
    /// would make black render as a different color.
    #[test]
    fn pack_bgra_black_opaque_is_alpha_high_only() {
        let packed = pack_bgra(color(0, 0, 0), 0xFF);
        assert_eq!(packed, 0xFF_00_00_00);
    }

    /// Positive edge case: opaque white packs to `0xFF_FF_FF_FF` — every byte
    /// saturated. The symmetric counterpart to the black test.
    #[test]
    fn pack_bgra_white_opaque_is_all_ones() {
        let packed = pack_bgra(color(0xFF, 0xFF, 0xFF), 0xFF);
        assert_eq!(packed, 0xFFFF_FFFF);
    }

    // ── fill_border_ring edge cases ────────────────────────────────────

    /// Positive: a non-square (wide) buffer colors the top + bottom rows and
    /// the left + right edge columns of each middle row. 6×3 with t=1:
    /// rows 0 and 2 fully colored, row 1 has only cols 0 and 5 colored.
    #[test]
    fn fill_border_ring_non_square_wide_buffer() {
        // half = min(6,3)/2 = 1, so t clamps to 1. last_row = 2, last_col = 5.
        let mut buf = vec![0u32; 18]; // 6 × 3
        let colored = pack_bgra(color(0, 0, 0), 0xFF);
        fill_border_ring(&mut buf, 6, 3, 1, colored);

        // Row 0 (indices 0..6): all colored.
        assert!(
            buf[0..6].iter().all(|&p| p == colored),
            "row 0 fully colored"
        );
        // Row 1 (indices 6..12): only edges (col 0 → idx 6, col 5 → idx 11).
        assert_eq!(buf[6], colored, "row 1 left edge");
        assert_eq!(buf[7], 0, "row 1 col 1 transparent");
        assert_eq!(buf[8], 0, "row 1 col 2 transparent");
        assert_eq!(buf[9], 0, "row 1 col 3 transparent");
        assert_eq!(buf[10], 0, "row 1 col 4 transparent");
        assert_eq!(buf[11], colored, "row 1 right edge");
        // Row 2 (indices 12..18): all colored.
        assert!(
            buf[12..18].iter().all(|&p| p == colored),
            "row 2 fully colored"
        );
    }

    /// Negative: a buffer whose length does not match `width * height` trips
    /// the documented `# Panics` precondition. This is the contract guard —
    /// callers must size the DIB section to exactly `width * height` pixels.
    #[should_panic(expected = "pixels buffer must be exactly width * height")]
    #[test]
    fn fill_border_ring_wrong_buffer_size_panics() {
        let mut buf = vec![0u32; 10]; // should be 9 for a 3×3 grid
        fill_border_ring(&mut buf, 3, 3, 1, 0xFFFF00FF);
    }
}
