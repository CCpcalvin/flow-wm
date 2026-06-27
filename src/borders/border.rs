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
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, GWLP_USERDATA, GetWindowLongPtrW,
    GetWindowRect, RegisterClassExW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSENDCHANGING, SWP_NOSIZE, SetWindowLongPtrW, SetWindowPos, ShowWindow, ULW_ALPHA,
    UpdateLayeredWindow, WINDOW_EX_STYLE, WINDOW_STYLE, WM_SIZE, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
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

/// Window procedure for the overlay class.
///
/// Handles `WM_SIZE` by rebuilding the cached ring bitmap so it matches the
/// overlay's new dimensions, then re-uploading it via `UpdateLayeredWindow`.
/// This makes the overlay self-sufficient: any caller that resizes the
/// overlay via `SetWindowPos` (the animator, `set_geometry`, teleport)
/// automatically gets a correctly-sized bitmap — no orchestrator-level
/// special-casing needed. All other messages are passed through to
/// `DefWindowProcW`.
///
/// # Threading
///
/// Runs on the IPC thread (the thread that owns the overlay HWND and pumps
/// messages via `PeekMessageW` + `DispatchMessageW` — see
/// `daemon::run::pump_messages`). When the animator worker thread calls
/// `SetWindowPos` cross-thread, `SendMessage(WM_SIZE)` blocks the animator
/// until the IPC thread dispatches it here, so all `BorderInner` access
/// remains single-threaded and the mutexes are uncontended.
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
    if msg == WM_SIZE {
        // SAFETY: GWLP_USERDATA was set in `Border::create`. The stored
        // pointer is valid for the HWND's lifetime because `BorderInner::drop`
        // calls `DestroyWindow` under the overlay mutex before the
        // `Arc<BorderInner>` can release its memory — after `DestroyWindow`
        // returns Win32 dispatches no further messages here, so the deref
        // cannot dangle.
        let ptr_val = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
        if ptr_val != 0 {
            // SAFETY: `ptr_val` is a valid `*const BorderInner` stored in
            // `Border::create`; see the lifetime note above.
            let border_inner = unsafe { &*(ptr_val as *const BorderInner) };
            border_inner.on_wm_size();
        }
        return LRESULT(0);
    }
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
        // Store a back-pointer to BorderInner so `overlay_wnd_proc` can find
        // it when `WM_SIZE` arrives. `Arc::as_ptr` yields a stable `*const
        // BorderInner` whose address is unaffected by `Border::clone` or Arc
        // refcount changes.
        //
        // SAFETY: the pointer is valid for the overlay HWND's entire lifetime.
        // `BorderInner::drop` calls `DestroyWindow` under the overlay mutex
        // before the `Arc<BorderInner>` releases its allocation, so after the
        // HWND is destroyed no further `WM_SIZE` messages can arrive and the
        // pointer can never be dereferenced after free. See
        // `overlay_wnd_proc`'s SAFETY note for the matching dereference.
        let inner_ptr = Arc::as_ptr(&border.inner) as isize;
        let _ = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, inner_ptr) };
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

// ── BorderInner: painting & geometry query ──────────────────────────

impl BorderInner {
    /// Query the overlay's current screen rect via `GetWindowRect`.
    ///
    /// Used by [`set_style`](Border::set_style) and
    /// [`on_wm_size`](Self::on_wm_size) to repaint at the overlay's *actual*
    /// position — which may differ from the last rect commanded via
    /// [`set_geometry`](Border::set_geometry) because the animator moves
    /// overlays via `SetWindowPos` without going through `set_geometry`.
    /// Querying the overlay itself (not the target window) is always correct:
    /// the animator is what put it where it is.
    ///
    /// Returns `None` if the overlay is destroyed or `GetWindowRect` fails
    /// (e.g. during shutdown races); the caller treats that as a no-op paint.
    fn overlay_rect(&self) -> Option<Rect> {
        let raw = *self.overlay.lock().expect("overlay mutex poisoned");
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
    /// [`set_geometry`](Border::set_geometry); this only updates pixels.
    fn paint(&self, rect: Rect) {
        let w = rect.width;
        let h = rect.height;
        if w <= 0 || h <= 0 {
            return;
        }
        let raw = *self.overlay.lock().expect("overlay mutex poisoned");
        if raw == 0 {
            return;
        }
        let overlay_hwnd = HWND(raw as *mut _);
        let style = *self.style.lock().expect("style mutex poisoned");

        let mut surface_guard = self.surface.lock().expect("surface mutex poisoned");
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

    /// Repaint the border at the overlay's current geometry.
    ///
    /// Called by `overlay_wnd_proc` when the overlay receives `WM_SIZE`
    /// (its dimensions changed via `SetWindowPos`). Because the size changed,
    /// [`paint`](Self::paint) rebuilds the bitmap via `CachedSurface::build`
    /// to match the new dimensions and re-uploads it — fixing the
    /// stale-bitmap artifact that previously occurred during resize
    /// animations (`expand-column`, `shrink-column`, etc.).
    ///
    /// No-op if the overlay is destroyed or its rect cannot be queried.
    fn on_wm_size(&self) {
        let Some(rect) = self.overlay_rect() else {
            return;
        };
        self.paint(rect);
    }
}

// ── Geometry, style, visibility ─────────────────────────────────────

impl Border {
    /// Command the overlay to cover `visible_rect`.
    ///
    /// This is the daemon-driven replacement for the old hook-driven
    /// `sync_geometry`: instead of querying `GetWindowRect(target)`, the
    /// caller passes the visible-content rect directly. `visible_rect` is
    /// in the same coordinate space as the layout engine's output (visible
    /// pixels), so the ring sits exactly at the visible-content edge —
    /// fixing the previous misalignment where it sat over the invisible
    /// resize border.
    ///
    /// Performs `SetWindowPos` (move + resize + re-seat z-order). When the
    /// size changes, `SetWindowPos` sends `WM_SIZE`, which
    /// `overlay_wnd_proc` handles by rebuilding the ring bitmap and
    /// re-uploading it via `UpdateLayeredWindow` — see
    /// [`BorderInner::on_wm_size`]. When the size is unchanged (move-only),
    /// no `WM_SIZE` is sent and the compositor simply translates the existing
    /// cached bitmap. Safe to call with a destroyed overlay (no-op) or a
    /// zero-area rect (early return).
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
        // dispatch is correct). If the size changed, Win32 sends WM_SIZE,
        // whose handler (overlay_wnd_proc) rebuilds the bitmap. See
        // `docs/src/dev-guide/borders.md`.
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
        // No explicit paint() call: if the size changed, WM_SIZE was already
        // dispatched synchronously (same thread) and on_wm_size rebuilt the
        // bitmap. If move-only, the compositor translates the cached bitmap.
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
        let Some(rect) = self.inner.overlay_rect() else {
            return;
        };
        self.inner.paint(rect);
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
    /// Each pixel's alpha is its anti-aliasing coverage (from
    /// [`fill_border_ring`]); recoloring keeps that coverage and swaps only the
    /// RGB channels, so AA stays crisp across focus changes. Fully transparent
    /// pixels (alpha `0`) are left untouched.
    fn recolor(&mut self, new_color: Color) {
        // SAFETY: `bits` points into the DIB section owned by `self.bitmap`,
        // sized `len` u32s; the slice borrow is confined to this call and is
        // not aliased by anything else.
        unsafe {
            let pixels = std::slice::from_raw_parts_mut(self.bits, self.len);
            for px in pixels.iter_mut() {
                *px = recolor_pixel(*px, new_color);
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

/// Repack `px` with `new_color`'s RGB channels, keeping `px`'s existing alpha
/// byte (its anti-aliasing coverage from [`fill_border_ring`]).
///
/// Returns `px` unchanged when its alpha is `0` (fully transparent). Used by
/// [`CachedSurface::recolor`] to swap a ring's colour without recomputing its
/// geometry or disturbing the AA coverage — so a focus change never re-aliases
/// the rounded corners.
#[must_use]
pub(crate) fn recolor_pixel(px: u32, new_color: Color) -> u32 {
    let a = (px >> 24) & 0xFF;
    if a == 0 {
        return px;
    }
    let r = new_color.r as u32;
    let g = new_color.g as u32;
    let b = new_color.b as u32;
    (a << 24) | (r << 16) | (g << 8) | b
}

/// Map a [`CornerPreference`] to the OUTER corner radius (in pixels) for the
/// rounded ring, given the ring `thickness`.
///
/// Returns the radius of the overlay's outer rounded corner. The ring is the
/// outer `thickness` px of the overlay (which sits at the visible content rect
/// — see `docs/src/dev-guide/borders.md`), so to wrap a window whose own corner
/// radius is `R` the ring's outer radius must be `R + thickness`; its inner
/// edge then has radius `R`. This ring geometry is independent of `overlap`:
/// `overlap` only moves the *window content* edge (inset by `thickness −
/// overlap`), so at `overlap = 0` the window's corner arc is exactly
/// concentric with the ring's inner arc (a perfect hug), while at `overlap > 0`
/// the window corner sits `overlap` px further out, under the opaque ring (no
/// gap). Square windows (`R = 0`) get a fully square ring (outer radius 0).
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

/// Test whether the point `(x, y)` lies inside a rounded rectangle with
/// top-left `(ox, oy)`, pixel size `w × h`, and corner radius `cr`.
///
/// Float-valued so the anti-aliased ring renderer can supersample at
/// sub-pixel offsets (see [`fill_border_ring`]). `cr` is assumed ≤
/// `min(w, h) / 2` (callers clamp). With `cr == 0.0` this reduces to a plain
/// axis-aligned rectangle test. Pure and allocation-free.
fn in_rounded_rect(x: f64, y: f64, ox: f64, oy: f64, w: f64, h: f64, cr: f64) -> bool {
    if w <= 0.0 || h <= 0.0 {
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
    if cr <= 0.0 {
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
    let cx = if lx < cr { cr } else { w - cr };
    let cy = if ly < cr { cr } else { h - cr };
    let dx = lx - cx;
    let dy = ly - cy;
    dx * dx + dy * dy <= cr * cr
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
/// Rounded rings (nonzero `corner_radius`) are rendered with 4×4 supersampled
/// anti-aliasing: each pixel's alpha is set from the fraction of 16 sub-samples
/// that land inside the ring, smoothing the diagonal arc edges. The square
/// fast path (`corner_radius == 0`) keeps its exact slice fill — pixel-aligned
/// edges gain nothing from AA.
///
/// The caller must size `pixels` to exactly `width * height` and zero it
/// first; this function writes `color` (with a per-pixel alpha byte on rounded
/// arcs) and never clears. Thickness is clamped against half the smaller
/// dimension so an oversized value just fills the whole buffer (no panic).
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
    //
    // We render with 4×4 supersampled anti-aliasing: each pixel is sampled at
    // 16 sub-points and its alpha set from the fraction landing inside the
    // ring. The straight, pixel-aligned edges are unaffected (all sub-samples
    // agree → full or zero coverage); only the diagonal arcs gain partial
    // alpha. `color` carries `0xFF` alpha; we keep its RGB and substitute the
    // per-pixel coverage alpha.
    let inner_radius = r.saturating_sub(t);
    let inner_w = width.saturating_sub(2 * t);
    let inner_h = height.saturating_sub(2 * t);
    let rgb = color & 0x00FF_FFFF;
    // Sub-pixel sample offsets for 4×4 supersampling: the centre of each
    // ¼-pixel cell.
    const OFFSETS: [f64; 4] = [0.125, 0.375, 0.625, 0.875];
    let (wf, hf) = (width as f64, height as f64);
    let (tf, rf) = (t as f64, r as f64);
    let (inner_wf, inner_hf, inner_rf) = (inner_w as f64, inner_h as f64, inner_radius as f64);

    // The ring only ever lives within `r` pixels of some edge (the outer `t`
    // straight band, or up to radius `r` at the corners). Pixels deeper than
    // `r + 1` from every edge are solidly inside the inner rect and can never
    // be part of the ring, so we skip them — making the per-rebuild cost
    // proportional to the border perimeter, not the window area.
    let frame = (r + 1).min(width).min(height);
    let bottom_start = height.saturating_sub(frame);
    let right_start = width.saturating_sub(frame);

    for y in 0..height {
        let row_start = y * width;
        let in_v_band = y < frame || y >= bottom_start;
        for x in 0..width {
            // Skip the deep interior — never part of the ring, leave it 0.
            if !in_v_band && x >= frame && x < right_start {
                continue;
            }
            let mut hits = 0u32;
            let xf = x as f64;
            let yf = y as f64;
            for &ox in &OFFSETS {
                let sx = xf + ox;
                for &oy in &OFFSETS {
                    let sy = yf + oy;
                    if in_rounded_rect(sx, sy, 0.0, 0.0, wf, hf, rf)
                        && !in_rounded_rect(sx, sy, tf, tf, inner_wf, inner_hf, inner_rf)
                    {
                        hits += 1;
                    }
                }
            }
            if hits > 0 {
                let alpha = (hits * 255) / 16;
                pixels[row_start + x] = (alpha << 24) | rgb;
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
        assert!(in_rounded_rect(0.0, 0.0, 0.0, 0.0, 5.0, 5.0, 0.0));
        assert!(in_rounded_rect(4.0, 4.0, 0.0, 0.0, 5.0, 5.0, 0.0));
        assert!(!in_rounded_rect(5.0, 0.0, 0.0, 0.0, 5.0, 5.0, 0.0));
        assert!(!in_rounded_rect(0.0, 5.0, 0.0, 0.0, 5.0, 5.0, 0.0));
    }

    /// `in_rounded_rect`: the extreme corner of a rounded rect is OUTSIDE the
    /// arc and therefore excluded — the defining behavior that keeps the border
    /// from sticking out past a rounded window.
    #[test]
    fn in_rounded_rect_arc_excludes_extreme_corner() {
        // 9×9 rect, radius 4. Corner center at (4,4). Pixel (0,0) is at
        // distance sqrt(32) ≈ 5.66 > 4, so it is outside.
        assert!(!in_rounded_rect(0.0, 0.0, 0.0, 0.0, 9.0, 9.0, 4.0));
        // Pixel (4,0) is on the top straight edge: inside.
        assert!(in_rounded_rect(4.0, 0.0, 0.0, 0.0, 9.0, 9.0, 4.0));
        // Center is inside.
        assert!(in_rounded_rect(4.0, 4.0, 0.0, 0.0, 9.0, 9.0, 4.0));
    }

    /// `in_rounded_rect`: a point lying exactly ON the corner arc
    /// (`dx² + dy² == cr²`) is treated as inside (the comparison is `<=`).
    ///
    /// Pinning this boundary behaviour keeps the 4×4 supersampling coverage
    /// curve stable — a regression to `<` would thin the rendered arc by one
    /// sub-pixel and subtly dim the AA boundary pixels.
    #[test]
    fn in_rounded_rect_arc_boundary_point_is_inside() {
        // 10×10 rect, radius 5. Top-left arc centre is at (5, 5).
        // Point (5, 0): dx = 0, dy = -5 → 0 + 25 == 25 == cr² → inside (<=).
        assert!(in_rounded_rect(5.0, 0.0, 0.0, 0.0, 10.0, 10.0, 5.0));
        // Point just outside the arc (4, 0): dx = -1, dy = -5 → 1 + 25 = 26 > 25.
        assert!(!in_rounded_rect(4.0, 0.0, 0.0, 0.0, 10.0, 10.0, 5.0));
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

    // ── anti-aliased rounded rendering ──────────────────────────────────

    /// Anti-aliasing: a rounded ring must produce pixels with PARTIAL alpha
    /// (0 < alpha < 255) along the diagonal arcs. Without AA every pixel is
    /// either fully opaque or fully transparent — the regression this guards.
    #[test]
    fn fill_border_ring_rounded_emits_partial_alpha_on_arcs() {
        // 40×40, thickness 3, outer radius 15 → sizeable corner arcs with
        // sub-pixel coverage transitions.
        const W: usize = 40;
        const H: usize = 40;
        let mut buf = vec![0u32; W * H];
        let colored = pack_bgra(color(0, 0, 0), 0xFF);
        fill_border_ring(&mut buf, W, H, 3, 15, colored);
        let rgb = colored & 0x00FF_FFFF;
        let mut partial = 0usize;
        let mut full = 0usize;
        let mut empty = 0usize;
        let mut partial_alphas = std::collections::HashSet::new();
        for &px in &buf {
            let a = (px >> 24) & 0xFF;
            if a == 0 {
                empty += 1;
            } else if a == 0xFF && (px & 0x00FF_FFFF) == rgb {
                full += 1;
            } else {
                partial += 1;
                partial_alphas.insert(a);
            }
        }
        assert!(
            partial > 0,
            "rounded ring must have AA (partial-alpha) pixels"
        );
        assert!(full > 0, "straight-band ring pixels must be fully opaque");
        assert!(
            empty > 0,
            "extreme corners + interior must stay transparent"
        );
        // The arc must be graded — a range of coverage levels — not a binary
        // edge with a stray half-pixel. A correct 4×4 sampler produces several
        // distinct hit counts (1..15) as the arc sweeps across pixels.
        assert!(
            partial_alphas.len() >= 2,
            "AA arc must show a graded coverage gradient, found only {:?}",
            partial_alphas
        );
        // The ring shape is symmetric about the buffer center, so an unbiased
        // sampler must yield a symmetric alpha map. A sampler whose sub-pixel
        // offsets cluster in one quadrant (e.g. only [0, 0.5)) shifts the arc
        // edges toward that quadrant and breaks this symmetry — this is the
        // guard that catches a biased supersample grid.
        for y in 0..H {
            for x in 0..W {
                let px = buf[y * W + x];
                assert_eq!(
                    px,
                    buf[y * W + (W - 1 - x)],
                    "alpha map must mirror horizontally at ({x},{y})"
                );
                assert_eq!(
                    px,
                    buf[(H - 1 - y) * W + x],
                    "alpha map must mirror vertically at ({x},{y})"
                );
            }
        }
    }

    /// Rounded-path boundary: when `thickness == corner_radius` the inner
    /// radius becomes `0` (via `saturating_sub`), reducing the inner rect to a
    /// plain bbox. The renderer must still produce an opaque straight band and
    /// leave both the extreme corners and the deep interior transparent.
    ///
    /// Guards the `inner_radius = r.saturating_sub(t)` edge against a regression
    /// where the inner-rect test would mishandle radius 0 and either leave the
    /// straight band transparent or color the interior.
    #[test]
    fn fill_border_ring_rounded_zero_inner_radius_keeps_interior_transparent() {
        // 16×16, thickness 4, outer radius 4 → inner radius = 4 - 4 = 0.
        let mut buf = vec![0u32; 16 * 16];
        let colored = pack_bgra(color(0, 0, 0), 0xFF);
        fill_border_ring(&mut buf, 16, 16, 4, 4, colored);
        // Extreme corner: outside the outer arc → transparent.
        assert_eq!(buf[0], 0, "top-left corner outside the outer arc");
        // Top straight band (col 8, row 0): inside outer rect, above the inner
        // bbox (whose top edge is at y = thickness = 4) → opaque ring pixel.
        assert_eq!(
            buf[8], colored,
            "top straight band must be fully opaque even with inner_radius 0"
        );
        // Deep interior (col 8, row 8): inside the inner bbox → transparent.
        assert_eq!(
            buf[8 * 16 + 8],
            0,
            "interior inside inner bbox must stay transparent"
        );
    }

    /// `recolor_pixel` keeps the coverage alpha and swaps only RGB — the
    /// property that keeps rounded corners crisp across a focus change.
    #[test]
    fn recolor_pixel_preserves_alpha_and_swaps_rgb() {
        let original = pack_bgra(color(0x11, 0x22, 0x33), 0x80);
        let recolored = recolor_pixel(original, color(0xAA, 0xBB, 0xCC));
        assert_eq!(recolored, pack_bgra(color(0xAA, 0xBB, 0xCC), 0x80));
    }

    /// `recolor_pixel` leaves fully-transparent pixels untouched (alpha 0), so
    /// transparent gaps never gain colour.
    #[test]
    fn recolor_pixel_leaves_transparent_untouched() {
        assert_eq!(recolor_pixel(0, color(0xFF, 0xFF, 0xFF)), 0);
        // Even an alpha-0 pixel with nonzero RGB stays as-is.
        let px = 0x00_11_22_33u32;
        assert_eq!(recolor_pixel(px, color(0xFF, 0xFF, 0xFF)), px);
    }

    /// `recolor_pixel` on a fully-opaque input (alpha `0xFF`) keeps full
    /// opacity and swaps only RGB — the straight-band counterpart to the
    /// partial-alpha test. Exercises the non-short-circuit branch at maximum
    /// coverage and guards against an off-by-one that would dim opaque pixels
    /// during a focus switch.
    #[test]
    fn recolor_pixel_full_alpha_preserves_opacity_and_swaps_rgb() {
        let original = pack_bgra(color(0x11, 0x22, 0x33), 0xFF);
        let recolored = recolor_pixel(original, color(0xAA, 0xBB, 0xCC));
        assert_eq!(recolored, pack_bgra(color(0xAA, 0xBB, 0xCC), 0xFF));
        // Top byte is the alpha — it must still be exactly 0xFF.
        assert_eq!(recolored >> 24, 0xFF);
    }

    // ── paint / on_wm_size destruction guards ─────────────────────────
    //
    // The actual WM_SIZE → repaint → bitmap-rebuild path (the core of the
    // resize fix) is integration-only: it needs a real layered window,
    // `SetWindowPos`, and the Win32 message pump. What IS unit-testable
    // without Win32 is the guard logic that makes `on_wm_size` and `paint`
    // safe to call from `overlay_wnd_proc` during shutdown races — when the
    // overlay HWND may already be in the destroyed state (`raw == 0`).
    //
    // `overlay_wnd_proc` unconditionally dereferences the GWLP_USERDATA
    // back-pointer and calls `on_wm_size`; these tests pin the no-op contract
    // that keeps that unconditional call sound.

    /// Build a `BorderInner` whose overlay HWND is `0` (destroyed) and whose
    /// surface cache is empty. No GDI handles are allocated, so constructing
    /// this fixture touches no Win32 state.
    fn destroyed_border_inner() -> BorderInner {
        BorderInner {
            // `0` is the post-`DestroyWindow` sentinel (see `BorderInner::drop`).
            overlay: Mutex::new(0),
            target: 0,
            style: Mutex::new(BorderStyle::new(
                color(0, 0, 0),
                1,
                CornerPreference::Square,
            )),
            corner_preference: CornerPreference::Square,
            surface: Mutex::new(None),
        }
    }

    /// `on_wm_size` (new method) must be a panic-free, GDI-free no-op when the
    /// overlay is in the destroyed state. This is the defense-in-depth that
    /// lets `overlay_wnd_proc` call it unconditionally: `on_wm_size` →
    /// `overlay_rect()` sees `raw == 0` → returns `None` → early return, with
    /// no `GetWindowRect` or `UpdateLayeredWindow` call.
    ///
    /// Without this guard, a `WM_SIZE` arriving during teardown (e.g. sent by
    /// `DestroyWindow` itself, or a cross-thread `SetWindowPos` racing the
    /// drop) would dereference a dead HWND into GDI.
    #[test]
    fn on_wm_size_is_noop_when_overlay_destroyed() {
        // Arrange — a BorderInner in the destroyed state (overlay == 0).
        let inner = destroyed_border_inner();

        // Act — WM_SIZE-equivalent entry point. Must not panic and must not
        // touch Win32/GDI (verified by the destroyed-overlay guard firing
        // before any GDI call site).
        inner.on_wm_size();

        // Assert — no panic occurred, surface cache untouched (still None).
        assert!(
            inner.surface.lock().unwrap().is_none(),
            "destroyed overlay must not allocate a cached surface"
        );
    }

    /// `paint` (moved from `impl Border` to `impl BorderInner`) must retain its
    /// destroyed-overlay early-return: a non-empty rect with `raw == 0` returns
    /// at the `raw == 0` guard, before any GDI call. Regression guard for the
    /// move — verifies the refactor did not drop the guard.
    #[test]
    fn paint_is_noop_when_overlay_destroyed() {
        // Arrange — destroyed overlay, but a non-empty rect so the zero-area
        // guard does NOT fire first; the `raw == 0` guard must be the one that
        // returns.
        let inner = destroyed_border_inner();
        let non_empty_rect = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };

        // Act
        inner.paint(non_empty_rect);

        // Assert — no GDI allocation leaked into the cache.
        assert!(
            inner.surface.lock().unwrap().is_none(),
            "destroyed overlay must not allocate a cached surface"
        );
    }

    /// `paint` (moved) must retain its zero-area-rect early-return: a rect with
    /// `width <= 0` or `height <= 0` returns before locking the overlay or
    /// touching GDI. Regression guard for the move, and the second guard that
    /// `on_wm_size`'s callee relies on for malformed-rect safety.
    #[test]
    fn paint_is_noop_for_zero_area_rect() {
        // Arrange — a rect with zero height (width is positive, so only the
        // height branch trips). Overlay is the destroyed sentinel but the
        // zero-area guard fires earlier, so this also confirms guard ordering.
        let inner = destroyed_border_inner();
        let zero_area_rect = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 0,
        };

        // Act
        inner.paint(zero_area_rect);

        // Assert — no allocation, no panic.
        assert!(
            inner.surface.lock().unwrap().is_none(),
            "zero-area rect must not allocate a cached surface"
        );
    }
}
