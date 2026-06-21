//! Per-window border overlay (layered topmost window).
//!
//! A [`BorderOverlay`] is a click-through, layered overlay window positioned
//! just above a single target window. It draws a solid colored ring around
//! the target's visible content.
//!
//! Phase 2 (this file) defines the type and lifecycle methods. Phase 3 will
//! replace the logging stubs with real Win32 calls (`CreateWindowExW`,
//! `UpdateLayeredWindow`, `SetWindowPos`, `DestroyWindow`).

use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, GetWindowRect, IsWindow,
    RegisterClassExW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOSENDCHANGING, SWP_NOZORDER,
    SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WINDOW_EX_STYLE, WINDOW_STYLE,
    WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;
use windows::core::w;

use super::style::BorderStyle;
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

/// A single border overlay attached to one target window.
///
/// Lifecycle: created by [`BorderOverlay::create`], updated via
/// [`set_style`](Self::set_style) /
/// [`set_visible`](Self::set_visible) /
/// [`sync_geometry`](Self::sync_geometry), and torn down by
/// [`destroy`](Self::destroy) (also called on drop).
///
/// # Win32 handle storage
///
/// Both HWNDs are stored as `isize` (not `HWND`) so the struct is `Send`.
/// `HWND` itself is `!Send` because it wraps a raw pointer. The IPC thread
/// and the hook thread both construct `HWND(n as *mut _)` at call sites
/// inside `unsafe` blocks.
pub struct BorderOverlay {
    /// Target window being decorated.
    target: isize,
    /// Overlay window HWND. `0` after [`destroy`](Self::destroy).
    overlay: Mutex<isize>,
    /// Current style (color + thickness). Behind a `Mutex` so the hook
    /// thread can read it during sync.
    style: Mutex<BorderStyle>,
}

impl BorderOverlay {
    /// Construct the target handle for Win32 calls.
    ///
    /// # Safety
    ///
    /// The returned `HWND` is valid only as long as the target window still
    /// exists. Callers must validate via `IsWindow` if there is any doubt.
    unsafe fn target_hwnd(&self) -> HWND {
        HWND(self.target as *mut _)
    }

    /// Create a new overlay window attached to `target` with the given style.
    ///
    /// The overlay is created hidden (zero-size) and immediately positioned
    /// and shown by [`sync_geometry`](Self::sync_geometry). If the target
    /// HWND is not a real window (e.g. a unit-test fake), `CreateWindowExW`
    /// still succeeds — the overlay HWND is independent of the target. The
    /// target only matters for `sync_geometry`.
    ///
    /// # Errors
    ///
    /// Returns a human-readable `String` if the window class could not be
    /// registered or `CreateWindowExW` fails.
    pub(crate) fn create(target: HWND, style: BorderStyle) -> Result<Self, String> {
        ensure_window_class_registered()?;
        let target_raw = target.0 as isize;
        let ex_style = WINDOW_EX_STYLE(
            WS_EX_LAYERED.0
                | WS_EX_TRANSPARENT.0
                | WS_EX_TOPMOST.0
                | WS_EX_NOACTIVATE.0
                | WS_EX_TOOLWINDOW.0,
        );
        let style_flags = WINDOW_STYLE(WS_POPUP.0);
        // SAFETY: CreateWindowExW creates a top-level layered window. Class
        // is registered above; OVERLAY_CLASS_NAME is a static PCWSTR. Zero
        // size is harmless until `sync_geometry` runs.
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

        let overlay = Self {
            target: target_raw,
            overlay: Mutex::new(hwnd.0 as isize),
            style: Mutex::new(style),
        };
        // Position/size + paint the bitmap. If the target is not a real
        // window this is a no-op (handled gracefully inside sync_geometry).
        overlay.sync_geometry();
        // Reveal the overlay. CreateWindowExW returns a hidden window by
        // default; UpdateLayeredWindow paints the layered surface but does
        // NOT make the window itself visible. Without this call the border
        // ring never appears on screen.
        overlay.set_visible(true);
        Ok(overlay)
    }

    /// Update the border color/thickness without recreating the overlay HWND.
    pub(crate) fn set_style(&self, style: BorderStyle) {
        *self.style.lock().expect("style mutex poisoned") = style;
        self.redraw();
    }

    /// Show or hide the overlay (used for minimize/restore).
    pub(crate) fn set_visible(&self, visible: bool) {
        let raw = *self.overlay.lock().expect("overlay mutex poisoned");
        if raw == 0 {
            return;
        }
        let hwnd = HWND(raw as *mut _);
        let cmd = if visible { SW_SHOWNOACTIVATE } else { SW_HIDE };
        // SAFETY: ShowWindow is sound for any HWND value; we own this one.
        let _ = unsafe { ShowWindow(hwnd, cmd) };
    }

    /// Re-query the target's `GetWindowRect` and reposition the overlay
    /// around it. Called by the hook thread on `EVENT_OBJECT_LOCATIONCHANGE`.
    ///
    /// Safe to call with a stale/fake target HWND: every Win32 call is
    /// guarded by `IsWindow` and reports failures at trace level (these are
    /// expected during shutdown races and unit tests with fake HWNDs).
    pub(crate) fn sync_geometry(&self) {
        let raw = *self.overlay.lock().expect("overlay mutex poisoned");
        if raw == 0 {
            return;
        }
        let overlay_hwnd = HWND(raw as *mut _);
        // SAFETY: target_hwnd reconstructs the target HWND from the stored
        // isize; we re-validate it via IsWindow before use.
        let target = unsafe { self.target_hwnd() };
        // SAFETY: IsWindow is sound for any HWND value.
        let is_real = unsafe { IsWindow(Some(target)) }.as_bool();
        if !is_real {
            return;
        }
        let mut rect = Default::default();
        // SAFETY: GetWindowRect writes into a local RECT.
        let ok = unsafe { GetWindowRect(target, &mut rect) }.is_ok();
        if !ok {
            log::trace!(
                "borders: GetWindowRect failed for target={:#x}",
                self.target
            );
            return;
        }
        let w = (rect.right - rect.left).max(0);
        let h = (rect.bottom - rect.top).max(0);
        if w == 0 || h == 0 {
            return;
        }
        // SAFETY: SetWindowPos on our own overlay window with NOACTIVATE |
        // NOZORDER | NOSENDCHANGING. NOSENDCHANGING avoids WM_WINDOWPOSCHANGING
        // callbacks that could re-enter sync_geometry.
        //
        // We deliberately do NOT use SWP_ASYNCWINDOWPOS here. Although ASYNC
        // would prevent the calling thread from blocking on a cross-thread
        // SendMessage, it instead POSTS the request to the owning (main/IPC)
        // thread's queue — and during animations LOCATIONCHANGE fires ~60×/sec
        // per window, which floods the main thread's queue and starves IPC.
        // Sync dispatch is the correct choice: it provides natural backpressure
        // (the hook thread blocks until the main thread processes the request,
        // preventing flooding) and the Arc-clone pattern in `sync_overlay`
        // ensures the main thread is never blocked on the overlays Mutex when
        // the SendMessage arrives, so there's no deadlock.
        let _ = unsafe {
            SetWindowPos(
                overlay_hwnd,
                None,
                rect.left,
                rect.top,
                w,
                h,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOSENDCHANGING,
            )
        };
        // Redraw the layered bitmap to match the new size.
        self.redraw_with_rect(&rect);
    }

    /// Rebuild the layered bitmap from the current style and the target's
    /// current rect, then upload it via `UpdateLayeredWindow`.
    ///
    /// Called on initial create, on `set_style`, and from `sync_geometry`
    /// after a size change. Safe to call with a stale target HWND — every
    /// Win32 call is guarded and failures are reported at trace level.
    fn redraw(&self) {
        let target = unsafe { self.target_hwnd() };
        // SAFETY: IsWindow is sound for any HWND value.
        if !unsafe { IsWindow(Some(target)) }.as_bool() {
            return;
        }
        let mut rect = Default::default();
        // SAFETY: GetWindowRect writes into a local RECT.
        if unsafe { GetWindowRect(target, &mut rect) }.is_err() {
            log::trace!(
                "borders: GetWindowRect failed in redraw for target={:#x}",
                self.target
            );
            return;
        }
        self.redraw_with_rect(&rect);
    }

    /// Inner redraw using a caller-provided target rect (avoids a second
    /// `GetWindowRect` call when `sync_geometry` already queried it).
    fn redraw_with_rect(&self, target_rect: &RECT) {
        let raw = *self.overlay.lock().expect("overlay mutex poisoned");
        if raw == 0 {
            return;
        }
        let overlay_hwnd = HWND(raw as *mut _);
        let w = (target_rect.right - target_rect.left).max(0);
        let h = (target_rect.bottom - target_rect.top).max(0);
        if w == 0 || h == 0 {
            return;
        }
        let style = *self.style.lock().expect("style mutex poisoned");
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
                match CreateDIBSection(Some(hdc_mem), &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0)
                {
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
                log::trace!(
                    "borders: UpdateLayeredWindow failed for target={:#x}",
                    self.target
                );
            }
        }
    }

    /// Destroy the overlay HWND. Idempotent.
    pub(crate) fn destroy(&self) {
        let mut guard = self.overlay.lock().expect("overlay mutex poisoned");
        let raw = *guard;
        if raw == 0 {
            return;
        }
        // SAFETY: `raw` came from a valid HWND we created in `create`. After
        // DestroyWindow the handle is invalid; we clear it under the lock so
        // re-entrant calls are no-ops.
        let hwnd = HWND(raw as *mut _);
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd) };
        *guard = 0;
    }
}

impl Drop for BorderOverlay {
    fn drop(&mut self) {
        self.destroy();
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
}
