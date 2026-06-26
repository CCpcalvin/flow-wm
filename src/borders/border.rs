//! Per-window border overlay (layered, click-through, seated just above its
//! target).
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
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP,
    HDC, HGDIOBJ, ReleaseDC, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, GetWindowRect, RegisterClassExW,
    SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSENDCHANGING, SWP_NOSIZE,
    SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WINDOW_EX_STYLE, WINDOW_STYLE,
    WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;
use windows::core::w;

use super::style::{BorderStyle, CornerPreference};
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
/// The overlay is a click-through, layered window seated **just above its
/// target** in z-order (not `WS_EX_TOPMOST`) so that floating / ignored
/// windows the user drags over the target correctly cover the border. The
/// daemon *commands* its geometry via [`set_geometry`](Self::set_geometry) —
/// the border never queries the target window's position itself. This is the
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
    /// The managed window this border wraps. The overlay is seated just ABOVE
    /// this HWND in z-order (via `SetWindowPos(overlay, target, …)`) rather
    /// than using `WS_EX_TOPMOST`, so windows the user raises above the target
    /// (floats, ignored windows) correctly cover the border. Set once at
    /// construction; immutable for the border's lifetime — an HWND is stable
    /// for the window's life, and the border is destroyed when its target
    /// leaves the registry.
    target: isize,
    /// Current style (color + thickness).
    style: Mutex<BorderStyle>,
    /// Cached DWM corner preference resolved once from the target window at
    /// creation. A window's corner rounding essentially never changes at
    /// runtime, so [`Border::corner_preference`] lets the daemon avoid a
    /// `DwmGetWindowAttribute` round-trip on every focus change (the recolor
    /// path) — see `daemon::borders::border_style_for`.
    corner_preference: CornerPreference,
    /// Cached rendered ring surface (DIB + memory DC + shape signature).
    ///
    /// Lets [`Border::paint`] skip the expensive `CreateCompatibleDC` +
    /// `CreateDIBSection` + per-pixel `fill_border_ring` work when the new
    /// paint matches the cached shape (move-only → skip entirely; color-only
    /// change → recolor in place). This is the hot path on focus changes and
    /// workspace switches, where the ring geometry is stable and only the
    /// color (Focused ↔ Unfocused) or position differs.
    surface: Mutex<Option<CachedSurface>>,
}

impl Border {
    /// Create a new border overlay window with the given style, wrapping
    /// `target_hwnd`.
    ///
    /// The overlay is created at `(0,0)` with a 1×1 logical size, seated just
    /// above `target_hwnd` in z-order, and then shown. Until
    /// [`set_geometry`](Self::set_geometry) uploads a bitmap via
    /// `UpdateLayeredWindow`, a `WS_EX_LAYERED` window renders nothing, so
    /// nothing appears on screen. The daemon is expected to call
    /// `set_geometry` shortly after creation to position and paint the ring.
    ///
    /// `target_hwnd` is the window the border wraps; it is stored and used to
    /// re-assert the overlay's z-order (see [`seat_above_target`](Self::seat_above_target)).
    ///
    /// # Errors
    ///
    /// Returns a human-readable `String` if the window class could not be
    /// registered or `CreateWindowExW` fails.
    pub(crate) fn create(style: BorderStyle, target_hwnd: isize) -> Result<Self, String> {
        ensure_window_class_registered()?;
        // NOTE: deliberately NOT `WS_EX_TOPMOST`. A topmost overlay would
        // render above floating / ignored windows the user dragged over the
        // target, so those windows could not cover the border. Instead the
        // overlay is seated just above its target HWND in z-order (see
        // `seat_above_target` and the `hwndInsertAfter` argument in
        // `set_geometry`). The animator's `SetWindowPos`-based moves use
        // `SWP_NOZORDER`, so this relative z-order is preserved across
        // animation. See `docs/src/dev-guide/borders.md`.
        let ex_style = WINDOW_EX_STYLE(
            WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0 | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0,
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
                target: target_hwnd,
                style: Mutex::new(style),
                // Cache the corner preference baked into the creation style so
                // the daemon can reuse it on subsequent recolors without
                // re-querying DWM.
                corner_preference: style.corner_preference,
                // No bitmap rendered yet — the first `paint` builds it.
                surface: Mutex::new(None),
            }),
        };
        // Reveal the overlay. Until set_geometry paints a bitmap the layered
        // surface is fully transparent, so this never flashes on screen.
        border.set_visible(true);
        // Seat the overlay just above its target so the initial z-order is
        // correct before the first set_geometry.
        border.seat_above_target();
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

    /// Returns the target window's corner-rounding preference, cached at
    /// border creation.
    ///
    /// The daemon uses this in `border_style_for` to build the recolor style
    /// without re-issuing a `DwmGetWindowAttribute` call on every focus
    /// change — the preference is frozen once per window because a window's
    /// corner rounding never changes at runtime.
    #[must_use]
    pub(crate) fn corner_preference(&self) -> CornerPreference {
        self.inner.corner_preference
    }

    /// Re-assert the overlay's z-order so it sits just above its target
    /// window, without moving or resizing it.
    ///
    /// Necessary because the OS can raise the target (e.g. on focus) without
    /// moving the overlay, which would otherwise leave the border hidden
    /// beneath its own target. Called on creation, on every geometry update
    /// (via `set_geometry`'s `hwndInsertAfter`), and on every border refresh.
    /// Cheap: a single `SetWindowPos` with `NOMOVE | NOSIZE` touches z-order
    /// only. Safe to call with a destroyed overlay or target — the call fails
    /// and is ignored at `trace` level by the caller's expectation.
    pub(crate) fn seat_above_target(&self) {
        let raw = *self.inner.overlay.lock().expect("overlay mutex poisoned");
        if raw == 0 {
            return;
        }
        let overlay_hwnd = HWND(raw as *mut _);
        let target_hwnd = HWND(self.inner.target as *mut _);
        // SAFETY: SetWindowPos on our own overlay, asking the OS to place it
        // just above the target sibling in z-order. NOMOVE|NOSIZE|NOACTIVATE|
        // NOSENDCHANGING restrict the effect to z-order only.
        let _ = unsafe {
            SetWindowPos(
                overlay_hwnd,
                Some(target_hwnd),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSENDCHANGING | SWP_NOMOVE | SWP_NOSIZE,
            )
        };
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
    /// Performs `SetWindowPos` (move + resize + re-seat z-order) and then
    /// refreshes pixels via [`paint`](Self::paint). When the size and style are
    /// unchanged (the common teleport / animator case) [`paint`](Self::paint)
    /// is a no-op, so this reduces to a single `SetWindowPos` move. Safe to
    /// call with a destroyed overlay (no-op) or a zero-area rect (early return).
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
        let target_hwnd = HWND(self.inner.target as *mut _);
        // SAFETY: SetWindowPos on our own overlay window. Passing the target
        // as hwndInsertAfter seats the overlay just above the target in
        // z-order (replacing the old WS_EX_TOPMOST approach), so floating /
        // ignored windows the user raises above the target correctly cover
        // the border. NOACTIVATE | NOSENDCHANGING: don't steal focus and
        // avoid re-entrant WM_WINDOWPOSCHANGING callbacks. We deliberately
        // do NOT use SWP_NOZORDER (we want to set z-order) and NOT
        // SWP_ASYNCWINDOWPOS (this runs on the IPC thread, synchronous
        // dispatch is correct). See `docs/src/dev-guide/borders.md`.
        let _ = unsafe {
            SetWindowPos(
                overlay_hwnd,
                Some(target_hwnd),
                visible_rect.x,
                visible_rect.y,
                visible_rect.width,
                visible_rect.height,
                SWP_NOACTIVATE | SWP_NOSENDCHANGING,
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
    /// Does not move the overlay — only updates the ring pixels with the new
    /// style. The repaint position is queried from the overlay HWND itself
    /// (via [`overlay_rect`](Self::overlay_rect)), so it stays correct even
    /// after the animator has moved the overlay via `SetWindowPos`. If the
    /// overlay is destroyed or its rect cannot be queried, this is a no-op.
    ///
    /// [`paint`](Self::paint) recolors the cached bitmap in place when only the
    /// color changed (the focus-switch case), avoiding a full DIB rebuild.
    pub(crate) fn set_style(&self, style: BorderStyle) {
        // Short-circuit when the style is unchanged. Focus changes route every
        // potentially-affected border through here, and skipping the repaint
        // (UpdateLayeredWindow + full DIB rebuild) when the color is identical
        // turns a no-op recolor into a cheap equality check. This is what lets
        // `on_focus_changed` refresh only the prev/new focus windows without
        // forcing redundant repaints.
        {
            let mut guard = self.inner.style.lock().expect("style mutex poisoned");
            if *guard == style {
                return;
            }
            *guard = style;
        }
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

    /// Render the ring for `rect` and upload it via `UpdateLayeredWindow`.
    ///
    /// Uses the cached [`CachedSurface`] to avoid the expensive GDI churn
    /// (`CreateCompatibleDC` + `CreateDIBSection` + per-pixel
    /// [`fill_border_ring`]) on the common cases:
    ///
    /// - **Move-only** (same size + style, e.g. `teleport_workspaces` or the
    ///   animator moving the overlay via `SetWindowPos`): the cached bitmap is
    ///   already composited, so this is a no-op — `SetWindowPos` already moved
    ///   the layered window and the compositor follows.
    /// - **Color-only change** (focus switch Focused ↔ Unfocused, same size):
    ///   recolor the cached DIB pixels in place and re-upload. No DIB realloc.
    /// - **Shape change** (size / thickness / corner): rebuild the cache.
    ///
    /// The overlay HWND itself is positioned separately by
    /// [`set_geometry`](Self::set_geometry); this only updates pixels.
    fn paint(&self, rect: Rect) {
        let w = rect.width;
        let h = rect.height;
        if w <= 0 || h <= 0 {
            return;
        }
        let raw = *self.inner.overlay.lock().expect("overlay mutex poisoned");
        if raw == 0 {
            return;
        }
        let overlay_hwnd = HWND(raw as *mut _);
        let style = *self.inner.style.lock().expect("style mutex poisoned");

        let mut surface_guard = self.inner.surface.lock().expect("surface mutex poisoned");
        let shape_matches = surface_guard
            .as_ref()
            .is_some_and(|s| s.shape_matches(w, h, &style));
        if shape_matches {
            let surface = surface_guard.as_mut().expect("shape_matches implies Some");
            if surface.color == style.color {
                // Identical bitmap already composited at this size + style. The
                // overlay was moved or re-asserted; nothing to upload.
                return;
            }
            // Same ring geometry, new color: recolor the cached pixels in
            // place and re-upload. Avoids CreateDIBSection + fill_border_ring.
            surface.recolor(style.color);
            surface.upload(overlay_hwnd, &rect);
            return;
        }
        // Shape changed (size / thickness / corner) or first paint: build a
        // fresh surface, upload it, and replace the cache (dropping the old
        // one frees its GDI handles).
        if let Some(surface) = CachedSurface::build(w, h, &style) {
            surface.upload(overlay_hwnd, &rect);
            *surface_guard = Some(surface);
        }
        // GDI failure: leave the previous cache intact so a transient failure
        // doesn't lose the last good bitmap.
    }
}

// ── Cached layered surface ──────────────────────────────────────────

/// Cached DIB section + memory DC holding the last-rendered border ring.
///
/// Kept on [`BorderInner`] so consecutive paints of the same ring geometry
/// skip the GDI allocation churn (see [`Border::paint`]). The signature
/// `(w, h, thickness, corner_preference)` identifies the ring geometry;
/// `color` tracks what is currently baked into the pixels so a color-only
/// change can recolor in place via [`CachedSurface::recolor`].
///
/// Every `Create*` GDI handle (`hdc_mem`, `bitmap`, the displaced `old_obj`)
/// is paired with its `Delete*` in [`Drop`], so replacing a `CachedSurface`
/// (e.g. on a shape change) frees the prior surface's resources.
struct CachedSurface {
    /// Pixel width / height the DIB was created for.
    w: i32,
    h: i32,
    /// Ring thickness (`style.width_px`) baked into the cached pixels.
    thickness: u32,
    /// Corner preference baked into the cached ring shape.
    corner: CornerPreference,
    /// The color currently baked into the cached pixels.
    color: Color,
    /// Memory DC the bitmap is selected into; reused for every upload.
    hdc_mem: HDC,
    /// The DIB section handle. Owns the pixel buffer at `bits`.
    bitmap: HBITMAP,
    /// Object displaced when `bitmap` was selected into `hdc_mem`; restored on
    /// drop so `DeleteObject(bitmap)` succeeds.
    old_obj: HGDIOBJ,
    /// Pointer into the DIB section's pixel buffer (`len` u32s). Valid for the
    /// lifetime of `bitmap`; written through to recolor without re-allocating.
    bits: *mut u32,
    len: usize,
}

impl std::fmt::Debug for CachedSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Omit the GDI handles / raw pointer — only the shape signature matters
        // for debugging, and the windows-rs handle types needn't implement Debug.
        f.debug_struct("CachedSurface")
            .field("w", &self.w)
            .field("h", &self.h)
            .field("thickness", &self.thickness)
            .field("corner", &self.corner)
            .field("color", &self.color)
            .finish_non_exhaustive()
    }
}

impl CachedSurface {
    /// Whether the cached ring geometry matches the given dimensions + style.
    fn shape_matches(&self, w: i32, h: i32, style: &BorderStyle) -> bool {
        self.w == w
            && self.h == h
            && self.thickness == style.width_px
            && self.corner == style.corner_preference
    }

    /// Allocate a `w × h` DIB + memory DC and fill the ring for `style`.
    ///
    /// Returns `None` if any GDI call fails (logged at `trace`); the caller
    /// leaves the previous cache intact so a transient failure doesn't lose
    /// the last good bitmap.
    fn build(w: i32, h: i32, style: &BorderStyle) -> Option<Self> {
        // SAFETY: GetDC / CreateCompatibleDC / CreateDIBSection / SelectObject
        // are well-formed for these arguments. Each Create* is paired with
        // cleanup either on this error path or in `Drop`.
        unsafe {
            // Screen DC for CreateCompatibleDC; released immediately.
            let hdc_screen = GetDC(None);
            if hdc_screen.is_invalid() {
                log::trace!("borders: GetDC failed for screen");
                return None;
            }
            let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
            let _ = ReleaseDC(None, hdc_screen);
            if hdc_mem.is_invalid() {
                log::trace!("borders: CreateCompatibleDC failed");
                return None;
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
                        return None;
                    }
                };
            if bits_ptr.is_null() {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(hdc_mem);
                return None;
            }

            // Populate pixels: outer thickness-px ring colored, interior 0.
            let len = (w as usize) * (h as usize);
            let bits = bits_ptr as *mut u32;
            std::ptr::write_bytes(bits, 0, len);
            // SAFETY: CreateDIBSection sized the buffer to `len` u32s; the
            // slice borrow is confined to the fill call and is not aliased.
            let pixels = std::slice::from_raw_parts_mut(bits, len);
            let thickness = style.width_px as usize;
            let corner_radius = corner_radius_px(style.corner_preference, thickness);
            let colored = pack_bgra(style.color, 0xFF);
            fill_border_ring(
                pixels,
                w as usize,
                h as usize,
                thickness,
                corner_radius,
                colored,
            );

            // Select the bitmap into the memory DC for UpdateLayeredWindow.
            let old_obj = SelectObject(hdc_mem, bitmap.into());

            Some(CachedSurface {
                w,
                h,
                thickness: style.width_px,
                corner: style.corner_preference,
                color: style.color,
                hdc_mem,
                bitmap,
                old_obj,
                bits,
                len,
            })
        }
    }

    /// Recolor the cached ring pixels to `new_color` in place.
    ///
    /// Ring pixels are `pack_bgra(color, 0xFF)` (always nonzero — the alpha
    /// byte is `0xFF`); interior and outside-corner pixels are exactly `0`.
    /// Overwriting every nonzero pixel with the new packed color recolors the
    /// ring without recomputing its geometry.
    fn recolor(&mut self, new_color: Color) {
        let packed = pack_bgra(new_color, 0xFF);
        // SAFETY: `bits` points into the DIB section owned by `self.bitmap`,
        // sized `len` u32s; the slice borrow is confined to this call and is
        // not aliased by anything else.
        unsafe {
            let pixels = std::slice::from_raw_parts_mut(self.bits, self.len);
            for px in pixels.iter_mut() {
                if *px != 0 {
                    *px = packed;
                }
            }
        }
        self.color = new_color;
    }

    /// Upload the cached bitmap to `overlay_hwnd` at `rect` via
    /// `UpdateLayeredWindow` (`ULW_ALPHA`). Also re-asserts the composited
    /// position via `pt_dst`.
    fn upload(&self, overlay_hwnd: HWND, rect: &Rect) {
        let pt_dst = POINT {
            x: rect.x,
            y: rect.y,
        };
        let pt_src = POINT { x: 0, y: 0 };
        let size = SIZE {
            cx: self.w,
            cy: self.h,
        };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: UpdateLayeredWindow reads our cached memory DC (with the
        // bitmap selected in) and pushes the composited surface to the overlay.
        // Failures are cosmetic (logged at trace) and expected during shutdown
        // races.
        unsafe {
            let result = UpdateLayeredWindow(
                overlay_hwnd,
                None,
                Some(&pt_dst),
                Some(&size),
                Some(self.hdc_mem),
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            if result.is_err() {
                log::trace!("borders: UpdateLayeredWindow failed");
            }
        }
    }
}

impl Drop for CachedSurface {
    fn drop(&mut self) {
        // SAFETY: these handles came from CreateCompatibleDC / CreateDIBSection
        // / SelectObject. Restore the DC's original object, free the bitmap,
        // then free the DC. DeleteDC deselects non-stock objects but does NOT
        // delete them, so we DeleteObject the bitmap explicitly.
        unsafe {
            let _ = SelectObject(self.hdc_mem, self.old_obj);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.hdc_mem);
        }
    }
}

// SAFETY: CachedSurface owns GDI handles (HDC / HBITMAP / HGDIOBJ) and a raw
// pixel pointer, none of which are `Send` by default. All access happens on
// the single main/IPC thread that owns the overlay HWND (see BorderInner's
// threading note); these values are never dereferenced from another thread.
// This matches the existing `unsafe impl Send for Window` argument.
unsafe impl Send for CachedSurface {}

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

/// Map a [`CornerPreference`] to the OUTER corner radius (in pixels) for the
/// rounded ring, given the ring `thickness`.
///
/// Returns the radius of the overlay's outer rounded corner. Because the
/// target window is inset by `thickness` (see `docs/src/dev-guide/borders.md`),
/// to hug a window whose own corner radius is `R` the ring's outer radius must
/// be `R + thickness`: the inner edge of the ring then has radius `R`,
/// concentric with the window's corner. Square windows (`R = 0`) get a fully
/// square ring (outer radius 0).
///
/// The window radii `8` / `4` are the unofficial but stable Windows 11
/// defaults; Microsoft does not document the exact pixel values.
fn corner_radius_px(pref: CornerPreference, thickness: usize) -> usize {
    // Win11 rounds top-level windows by default, so `Default` is treated as
    // the standard rounded radius.
    let window_radius: usize = match pref {
        CornerPreference::Square => 0,
        CornerPreference::Rounded => 8,
        CornerPreference::RoundedSmall => 4,
        CornerPreference::Default => 8,
    };
    if window_radius == 0 {
        0
    } else {
        window_radius + thickness
    }
}

/// Test whether pixel `(x, y)` lies inside a rounded rectangle with top-left
/// `(ox, oy)`, pixel size `w × h`, and corner radius `cr`.
///
/// `cr` is assumed to be ≤ `min(w, h) / 2` (callers clamp). With `cr == 0`
/// this reduces to a plain axis-aligned rectangle test. Pure and allocation-
/// free so it can be used in the per-pixel ring loop without test setup.
fn in_rounded_rect(
    x: usize,
    y: usize,
    ox: usize,
    oy: usize,
    w: usize,
    h: usize,
    cr: usize,
) -> bool {
    if w == 0 || h == 0 {
        return false;
    }
    // Translate into the rectangle's local space.
    if x < ox || y < oy {
        return false;
    }
    let lx = x - ox;
    let ly = y - oy;
    if lx >= w || ly >= h {
        return false;
    }
    if cr == 0 {
        return true;
    }
    // Straight-edge bands: anywhere not in a corner square is inside.
    if lx >= cr && lx < w - cr {
        return true;
    }
    if ly >= cr && ly < h - cr {
        return true;
    }
    // Corner square: inside iff within radius `cr` of the arc center.
    let cx: i64 = (if lx < cr { cr } else { w - cr }) as i64;
    let cy: i64 = (if ly < cr { cr } else { h - cr }) as i64;
    let dx = lx as i64 - cx;
    let dy = ly as i64 - cy;
    let r = cr as i64;
    dx * dx + dy * dy <= r * r
}

/// Fill the outer `thickness`-pixel ring of a `width * height` pixel buffer
/// with `color`, leaving the interior transparent (`0`).
///
/// `corner_radius` is the OUTER corner radius in pixels (`0` = square ring).
/// For a rounded ring the inner edge is concentric with the outer, with radius
/// `corner_radius - thickness` (clamped to 0), so it hugs a target window
/// whose own corners have radius `corner_radius - thickness`. See
/// [`corner_radius_px`] for how the caller derives `corner_radius` from a
/// [`CornerPreference`].
///
/// The caller must size `pixels` to exactly `width * height` and zero it
/// first; this function only writes `color`, never clears. Thickness is
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
    corner_radius: usize,
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
    let half = width.min(height) / 2;
    let t = thickness.min(half);
    if t == 0 {
        return;
    }
    // Clamp the outer radius against the buffer; the inner radius is concentric
    // and therefore always ≤ the inner box's half-dimension (see below).
    let r = corner_radius.min(half);

    // Square fast path: identical to the original slice-fill ring. Keeps the
    // hot float-drag repaint path (which rebuilds the bitmap on every move)
    // cheap — no per-pixel arc math.
    if r == 0 {
        let last_row = height - t;
        let last_col = width - t;
        for y in 0..height {
            let row_start = y * width;
            if y < t || y >= last_row {
                pixels[row_start..row_start + width].fill(color);
            } else {
                pixels[row_start..row_start + t].fill(color);
                pixels[row_start + last_col..row_start + width].fill(color);
            }
        }
        return;
    }

    // Rounded path. Outer rect = full buffer with radius `r`; inner rect =
    // buffer inset by `t` with the concentric radius `r - t`. A pixel is part
    // of the ring iff it is inside the outer rounded rect AND outside the
    // inner rounded rect.
    let inner_radius = r.saturating_sub(t);
    let inner_w = width.saturating_sub(2 * t);
    let inner_h = height.saturating_sub(2 * t);

    for y in 0..height {
        let row_start = y * width;
        for x in 0..width {
            if in_rounded_rect(x, y, 0, 0, width, height, r)
                && !in_rounded_rect(x, y, t, t, inner_w, inner_h, inner_radius)
            {
                pixels[row_start + x] = color;
            }
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
        fill_border_ring(&mut buf, 3, 3, 0, 0, 0xFFFF00FF);
        assert!(buf.iter().all(|&p| p == 0));
    }

    #[test]
    fn fill_border_ring_one_pixel_ring_on_3x3_leaves_only_center_transparent() {
        // 3×3 with t=1: outer 8 pixels colored, center (idx 4) transparent.
        // Index layout (row*3 + col): 0 1 2 / 3 4 5 / 6 7 8.
        let mut buf = vec![0u32; 9];
        let colored = pack_bgra(color(0, 0, 0), 0xFF);
        fill_border_ring(&mut buf, 3, 3, 1, 0, colored);
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
        fill_border_ring(&mut buf, 5, 5, 2, 0, colored);
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
        fill_border_ring(&mut buf, 4, 4, 100, 0, colored);
        assert!(buf.iter().all(|&p| p == colored));
    }

    #[test]
    fn fill_border_ring_empty_dimensions_is_noop() {
        let mut buf: Vec<u32> = vec![];
        fill_border_ring(&mut buf, 0, 0, 5, 0, 0xFFFF00FF);
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
        fill_border_ring(&mut buf, 6, 3, 1, 0, colored);

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
        fill_border_ring(&mut buf, 3, 3, 1, 0, 0xFFFF00FF);
    }

    // ── rounded-corner rendering ──────────────────────────────────────

    /// `corner_radius_px`: square windows (and any preference mapping to
    /// radius 0) yield a fully square ring (outer radius 0).
    #[test]
    fn corner_radius_px_square_is_zero() {
        assert_eq!(corner_radius_px(CornerPreference::Square, 3), 0);
    }

    /// `corner_radius_px`: a rounded window with radius 8 and thickness 3
    /// yields outer radius 11 (8 + thickness), so the ring's inner edge has
    /// radius 8 — concentric with the window's own corner.
    #[test]
    fn corner_radius_px_rounded_adds_thickness() {
        assert_eq!(corner_radius_px(CornerPreference::Rounded, 3), 11);
        assert_eq!(corner_radius_px(CornerPreference::RoundedSmall, 3), 7);
        assert_eq!(corner_radius_px(CornerPreference::Default, 3), 11);
    }

    /// `in_rounded_rect`: a square rect (cr=0) is a plain bbox test.
    #[test]
    fn in_rounded_rect_zero_radius_is_bbox() {
        assert!(in_rounded_rect(0, 0, 0, 0, 5, 5, 0));
        assert!(in_rounded_rect(4, 4, 0, 0, 5, 5, 0));
        assert!(!in_rounded_rect(5, 0, 0, 0, 5, 5, 0));
        assert!(!in_rounded_rect(0, 5, 0, 0, 5, 5, 0));
    }

    /// `in_rounded_rect`: the extreme corner of a rounded rect is OUTSIDE the
    /// arc and therefore excluded — the defining behavior that keeps the border
    /// from sticking out past a rounded window.
    #[test]
    fn in_rounded_rect_arc_excludes_extreme_corner() {
        // 9×9 rect, radius 4. Corner center at (4,4). Pixel (0,0) is at
        // distance sqrt(32) ≈ 5.66 > 4, so it is outside.
        assert!(!in_rounded_rect(0, 0, 0, 0, 9, 9, 4));
        // Pixel (4,0) is on the top straight edge: inside.
        assert!(in_rounded_rect(4, 0, 0, 0, 9, 9, 4));
        // Center is inside.
        assert!(in_rounded_rect(4, 4, 0, 0, 9, 9, 4));
    }

    /// Rounded ring on a square-ish buffer: the four extreme corner pixels
    /// (outside the corner arcs) must stay transparent, while a mid-edge pixel
    /// stays colored. This is the core rounded-border invariant.
    #[test]
    fn fill_border_ring_rounded_keeps_extreme_corners_transparent() {
        // 11×11, thickness 1, outer corner_radius 4 (so window radius ≈ 3).
        let mut buf = vec![0u32; 11 * 11];
        let colored = pack_bgra(color(0, 0, 0), 0xFF);
        fill_border_ring(&mut buf, 11, 11, 1, 4, colored);
        // Extreme corners are outside the radius-4 arc → transparent.
        assert_eq!(buf[0], 0, "top-left corner transparent");
        assert_eq!(buf[10], 0, "top-right corner transparent");
        assert_eq!(buf[11 * 10], 0, "bottom-left corner transparent");
        assert_eq!(buf[11 * 10 + 10], 0, "bottom-right corner transparent");
        // Top-edge middle (col 5, row 0) is on the straight band → colored.
        assert_eq!(buf[5], colored, "top edge middle colored");
        // Left-edge middle (col 0, row 5) is on the straight band → colored.
        assert_eq!(buf[5 * 11], colored, "left edge middle colored");
    }

    /// Rounded ring still colors a normal-thickness band on the straight
    /// edges and leaves the interior transparent.
    #[test]
    fn fill_border_ring_rounded_leaves_center_transparent() {
        let mut buf = vec![0u32; 11 * 11];
        let colored = pack_bgra(color(0, 0, 0), 0xFF);
        fill_border_ring(&mut buf, 11, 11, 1, 4, colored);
        // Center pixel well inside the ring → transparent.
        assert_eq!(buf[5 * 11 + 5], 0, "center transparent");
    }
}
