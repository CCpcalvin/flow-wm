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
    /// - **Resize / shape change that fits the existing buffer** (the common
    ///   resize-animation frame): re-composite into the reused grow-only DIB
    ///   (clear + [`composite_ring`] from a cached corner tile) and re-upload.
    ///   No `CreateDIBSection`, no per-pixel area integration — the hot-path win.
    /// - **Shape change that outgrows the buffer** (crossed a power-of-two
    ///   capacity boundary) or first paint: allocate a fresh capacity-sized DIB
    ///   and composite into it.
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
        // Shape changed (size / thickness / corner) or first paint. Prefer the
        // grow-only reuse path: if the existing DIB is large enough, re-composite
        // into it (cheap clear + composite_ring) without re-allocating.
        let reused = match surface_guard.as_mut() {
            Some(surface) if surface.buffer_fits(w, h) => {
                surface.render_ring(w, h, &style);
                surface.upload(overlay_hwnd, &rect);
                true
            }
            _ => false,
        };
        if !reused {
            // Buffer too small (or no surface yet): allocate a fresh
            // capacity-sized DIB, composite, upload, and replace the cache
            // (dropping the old one frees its GDI handles).
            if let Some(surface) = CachedSurface::build(w, h, &style) {
                surface.upload(overlay_hwnd, &rect);
                *surface_guard = Some(surface);
            }
            // GDI failure: leave the previous cache intact so a transient
            // failure doesn't lose the last good bitmap.
        }
    }

    /// Repaint the border at the overlay's current geometry.
    ///
    /// Called by `overlay_wnd_proc` when the overlay receives `WM_SIZE`
    /// (its dimensions changed via `SetWindowPos`). Because the size changed,
    /// [`paint`](Self::paint) re-composites the bitmap — usually into the
    /// reused grow-only DIB via [`CachedSurface::render_ring`], only allocating
    /// a fresh DIB at power-of-two capacity crossings — and re-uploads it,
    /// fixing the stale-bitmap artifact that previously occurred during resize
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
    /// Logical pixel width / height the ring was last composited at. Always
    /// `<= buf_w / buf_h`. `UpdateLayeredWindow` uploads exactly this size.
    w: i32,
    h: i32,
    /// DIB allocation size (power-of-two capacity, grow-only). The buffer at
    /// `bits` is `buf_w × buf_h`; the ring is composited into the top-left
    /// `w × h` region at stride `buf_w`. Reused across resizes that fit, so a
    /// smooth resize animation only re-runs `CreateDIBSection` at power-of-two
    /// capacity crossings — not every frame.
    buf_w: i32,
    buf_h: i32,
    /// Ring thickness (`style.width_px`) baked into the cached pixels.
    thickness: u32,
    /// Corner preference baked into the cached ring shape.
    corner: CornerPreference,
    /// The color currently baked into the cached pixels.
    color: Color,
    /// Cached alpha-coverage tile for the corner (top-left quadrant) at the
    /// effective `(tile_r, tile_t)` geometry. Re-rendered only when the
    /// effective `(r, t)` changes (i.e. thickness / corner-pref change, or the
    /// window shrinks enough to clamp the radius); stable across ordinary
    /// resizes, which is what makes the composite path cheap.
    coverage: Vec<u8>,
    /// Effective `(radius, thickness)` the `coverage` tile was rendered for.
    /// `usize::MAX` sentinel forces a first render in `build`.
    tile_r: usize,
    tile_t: usize,
    /// Memory DC the bitmap is selected into; reused for every upload.
    hdc_mem: HDC,
    /// The DIB section handle. Owns the pixel buffer at `bits`.
    bitmap: HBITMAP,
    /// Object displaced when `bitmap` was selected into `hdc_mem`; restored on
    /// drop so `DeleteObject(bitmap)` succeeds.
    old_obj: HGDIOBJ,
    /// Pointer into the DIB section's pixel buffer (`len` u32s, laid out at
    /// stride `buf_w`). Valid for the lifetime of `bitmap`; written through to
    /// recolor / recomposite without re-allocating.
    bits: *mut u32,
    /// `buf_w * buf_h` — total u32s in the DIB buffer.
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

    /// Whether the grow-only DIB can hold a `w × h` ring without reallocating.
    fn buffer_fits(&self, w: i32, h: i32) -> bool {
        w <= self.buf_w && h <= self.buf_h
    }

    /// Re-composite the ring into the existing DIB for `style` at `(w, h)`.
    ///
    /// Assumes [`buffer_fits`] holds (caller checks; debug-asserted); does NOT touch any GDI
    /// handle. Clears the `w × h` region to transparent, re-renders the corner
    /// coverage tile only when the effective `(radius, thickness)` changed, then
    /// composites edges + corners via [`composite_ring`]. This is the cheap
    /// resize-frame path that avoids `CreateDIBSection` + per-pixel area integration.
    ///
    /// [`buffer_fits`]: CachedSurface::buffer_fits
    fn render_ring(&mut self, w: i32, h: i32, style: &BorderStyle) {
        debug_assert!(self.buffer_fits(w, h), "caller must ensure buffer_fits");
        let thickness = style.width_px as usize;
        let corner_radius = corner_radius_px(style.corner_preference, thickness);
        let half = (w as usize).min(h as usize) / 2;
        let t = thickness.min(half);
        let r = corner_radius.min(half);

        // Re-render the corner tile only when the effective geometry changed
        // (thickness / corner-pref config change, or a shrink that clamps the
        // radius). Stable across ordinary resizes.
        if r != self.tile_r || t != self.tile_t {
            self.coverage = render_corner_coverage(r, t);
            self.tile_r = r;
            self.tile_t = t;
        }

        let stride = self.buf_w as usize;
        let wu = w as usize;
        let hu = h as usize;
        // SAFETY: `bits` owns `len = buf_w*buf_h` u32s; we touch `[0, wu)` of
        // each of the first `hu` rows, all in-bounds since `wu <= buf_w` and
        // `hu <= buf_h` (buffer_fits). Confined to this call, not aliased.
        unsafe {
            for y in 0..hu {
                std::ptr::write_bytes(self.bits.add(y * stride), 0, wu);
            }
            let pixels = std::slice::from_raw_parts_mut(self.bits, self.len);
            let colored = pack_bgra(style.color, 0xFF);
            composite_ring(
                pixels,
                BufferLayout {
                    stride,
                    width: wu,
                    height: hu,
                },
                thickness,
                corner_radius,
                &self.coverage,
                colored,
            );
        }

        self.w = w;
        self.h = h;
        self.thickness = style.width_px;
        self.corner = style.corner_preference;
        self.color = style.color;
    }

    /// Allocate a DIB sized to the next power-of-two capacity ≥ `(w, h)` plus a
    /// memory DC, then composite the ring into the top-left `w × h` region.
    ///
    /// The power-of-two capacity is what makes the DIB grow-only: a smooth
    /// resize animation only crosses a power-of-two boundary `O(log n)` times,
    /// so `CreateDIBSection` runs a handful of times across a whole animation
    /// instead of every frame. Returns `None` if any GDI call fails (logged at
    /// `trace`); the caller leaves the previous cache intact so a transient
    /// failure doesn't lose the last good bitmap.
    fn build(w: i32, h: i32, style: &BorderStyle) -> Option<Self> {
        let buf_w = next_capacity(w);
        let buf_h = next_capacity(h);
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

            // Top-down 32-bit ARGB DIB section (negative biHeight = top-down),
            // allocated at the power-of-two capacity so it can be reused across
            // resizes that fit.
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: buf_w,
                    biHeight: -buf_h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    biSizeImage: (buf_w as u32) * (buf_h as u32) * 4,
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

            let len = (buf_w as usize) * (buf_h as usize);
            let bits = bits_ptr as *mut u32;
            // Zero the whole capacity once; `render_ring` clears the logical
            // `w × h` region each subsequent composite.
            std::ptr::write_bytes(bits, 0, len);

            // Select the bitmap into the memory DC for UpdateLayeredWindow.
            let old_obj = SelectObject(hdc_mem, bitmap.into());

            let mut surface = CachedSurface {
                w,
                h,
                buf_w,
                buf_h,
                thickness: style.width_px,
                corner: style.corner_preference,
                color: style.color,
                coverage: Vec::new(),
                // Force `render_ring` to render the tile on first composite.
                tile_r: usize::MAX,
                tile_t: usize::MAX,
                hdc_mem,
                bitmap,
                old_obj,
                bits,
                len,
            };
            surface.render_ring(w, h, style);
            Some(surface)
        }
    }

    /// Recolor the cached ring pixels to `new_color` in place.
    ///
    /// Each pixel's alpha is its anti-aliasing coverage (from
    /// [`composite_ring`]); recoloring keeps that coverage and swaps only the
    /// RGB channels, so AA stays crisp across focus changes. Fully transparent
    /// pixels (alpha `0`) are left untouched. Only the logical `w × h` region
    /// is walked (at stride `buf_w`), skipping the grow-only capacity padding.
    fn recolor(&mut self, new_color: Color) {
        let stride = self.buf_w as usize;
        let w = self.w as usize;
        let h = self.h as usize;
        debug_assert!(
            w <= self.buf_w as usize && h <= self.buf_h as usize,
            "recolor bounds derived from render_ring/build invariant"
        );
        // SAFETY: `bits` owns `len = buf_w*buf_h` u32s; for each of the first
        // `h` rows we borrow a `w`-px slice starting at `y*stride`, in-bounds
        // since `w <= buf_w` and `h <= buf_h`. Confined to this call.
        unsafe {
            for y in 0..h {
                let row = self.bits.add(y * stride);
                let pixels = std::slice::from_raw_parts_mut(row, w);
                for px in pixels.iter_mut() {
                    *px = recolor_pixel(*px, new_color);
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
///
/// This is STRAIGHT (non-premultiplied) alpha — appropriate for opaque band
/// fills (`alpha = 0xFF`, where straight and premultiplied coincide). For
/// partial-coverage pixels written to the layered window's
/// `AC_SRC_ALPHA`-blended DIB, use [`pack_premultiplied`] instead: the
/// `BLENDFUNCTION` contract requires source RGB to be scaled by alpha.
#[must_use]
pub(crate) const fn pack_bgra(color: Color, alpha: u8) -> u32 {
    let r = color.r as u32;
    let g = color.g as u32;
    let b = color.b as u32;
    let a = alpha as u32;
    (a << 24) | (r << 16) | (g << 8) | b
}

/// Pack `rgb` (low 24 bits, `0xRRGGBB`) and `alpha` into a PREMULTIPLIED BGRA
/// `u32` for the layered window's `AC_SRC_ALPHA` blend.
///
/// Each RGB channel is scaled by `alpha / 255` with round-to-nearest division
/// (`(channel * alpha + 127) / 255`), so Windows's premultiplied `AC_SRC_OVER`
/// operator reproduces the correct perceptual colour at partial coverage
/// instead of adding the full-intensity RGB that a straight-alpha pack would
/// feed it (the cause of the "white edge at corner" — see
/// `docs/src/dev-guide/borders.md`). For `alpha = 255` this is the identity,
/// matching [`pack_bgra`]'s opaque output, so callers may use either at full
/// coverage.
#[must_use]
pub(crate) fn pack_premultiplied(rgb: u32, alpha: u8) -> u32 {
    let a = u32::from(alpha);
    let r = ((rgb >> 16) & 0xFF) * a;
    let g = ((rgb >> 8) & 0xFF) * a;
    let b = (rgb & 0xFF) * a;
    let r = (r + 127) / 255;
    let g = (g + 127) / 255;
    let b = (b + 127) / 255;
    (a << 24) | (r << 16) | (g << 8) | b
}

/// Repack `px` with `new_color`'s RGB channels, keeping `px`'s existing alpha
/// byte (its anti-aliasing coverage from [`fill_border_ring`]).
///
/// Returns `px` unchanged when its alpha is `0` (fully transparent). Used by
/// [`CachedSurface::recolor`] to swap a ring's colour without recomputing its
/// geometry or disturbing the AA coverage — so a focus change never re-aliases
/// the rounded corners.
///
/// The output is premultiplied (see [`pack_premultiplied`]): the new RGB is
/// scaled by the preserved coverage alpha so the layered window's
/// `AC_SRC_ALPHA` blend reproduces the correct perceptual colour.
#[must_use]
pub(crate) fn recolor_pixel(px: u32, new_color: Color) -> u32 {
    let a = (px >> 24) & 0xFF;
    if a == 0 {
        return px;
    }
    let rgb =
        (u32::from(new_color.r) << 16) | (u32::from(new_color.g) << 8) | u32::from(new_color.b);
    pack_premultiplied(rgb, a as u8)
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
/// Float-valued for geometric exactness. Kept as a test oracle — the production
/// renderer uses closed-form area integration (see [`corner_pixel_alpha`]).
/// `cr` is assumed ≤ `min(w, h) / 2` (callers clamp). With `cr == 0.0` this
/// reduces to a plain axis-aligned rectangle test. Pure and allocation-free.
///
/// Test-only oracle: the production render path uses [`composite_ring`] /
/// [`render_corner_coverage`] instead; this remains as the geometric reference
/// exercised by [`fill_border_ring`] under `#[cfg(test)]`.
#[cfg_attr(not(test), allow(dead_code))]
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

/// Indefinite integral of the chord half-length `sqrt(r^2 - y^2)` for a circle
/// of radius `r`, used by [`pixel_circle_area_q1`] to evaluate the exact
/// pixel-circle intersection area in closed form.
///
/// `G(y, r) = (y * sqrt(r^2 - y^2) + r^2 * asin(y / r)) / 2`. The input `y` is
/// clamped to `[-r, r]` so callers needn't pre-clip; for `|y| > r` the
/// integrand is zero and the antiderivative stays constant (matching the
/// definite-integral behaviour). `r == 0` returns `0.0`.
fn circle_segment_antiderivative(y: f64, r: f64) -> f64 {
    if r == 0.0 {
        return 0.0;
    }
    let r2 = r * r;
    let yc = y.clamp(-r, r);
    let chord = (r2 - yc * yc).max(0.0).sqrt();
    (yc * chord + r2 * (yc / r).asin()) * 0.5
}

/// Exact area of intersection of the axis-aligned rectangle `[u0, u1] × [v0,
/// v1]` with a circle of radius `r` centred at the origin.
///
/// Requires `u0 >= 0` and `v0 >= 0` (upper-right quadrant relative to the
/// centre); other quadrants are reached by reflecting the pixel before
/// calling. Quick-rejects when the closest corner `(u0, v0)` is outside the
/// circle and quick-accepts when the farthest corner `(u1, v1)` is inside, so
/// the expensive `asin` / `sqrt` path only runs for the thin band of pixels
/// the arc actually crosses. Geometry reference: the corner coverage math is
/// described in `docs/src/dev-guide/borders.md`.
fn pixel_circle_area_q1(u0: f64, u1: f64, v0: f64, v1: f64, r: f64) -> f64 {
    debug_assert!(
        u0 >= 0.0 && v0 >= 0.0 && u1 > u0 && v1 > v0 && r >= 0.0,
        "pixel_circle_area_q1 requires the upper-right quadrant"
    );
    // Quick reject: closest corner (u0, v0) outside the circle.
    if u0 * u0 + v0 * v0 >= r * r {
        return 0.0;
    }
    // Quick accept: farthest corner (u1, v1) inside the circle.
    if u1 * u1 + v1 * v1 <= r * r {
        return (u1 - u0) * (v1 - v0);
    }
    // Partial: the arc crosses the pixel. Find the heights at which the
    // circle's right boundary x = sqrt(r^2 - y^2) equals u0 and u1; the
    // integrand changes form at these crossings.
    let y_u1 = (r * r - u1 * u1).max(0.0).sqrt();
    let y_u0 = (r * r - u0 * u0).max(0.0).sqrt();
    // Segment 1: heights where chord >= u1 → the full pixel width is covered.
    let seg1_hi = v1.min(y_u1);
    let seg1 = (seg1_hi - v0).max(0.0) * (u1 - u0);
    // Segment 2: heights where u0 < chord < u1 → integrate chord(y) - u0.
    let seg2_lo = v0.max(y_u1);
    let seg2_hi = v1.min(y_u0);
    let seg2 = if seg2_lo < seg2_hi {
        circle_segment_antiderivative(seg2_hi, r)
            - circle_segment_antiderivative(seg2_lo, r)
            - u0 * (seg2_hi - seg2_lo)
    } else {
        0.0
    };
    seg1 + seg2
}

/// Compute the exact-area anti-aliased coverage (`0..=255`) for ONE pixel of a
/// rounded ring corner.
///
/// `px`, `py`: the pixel's integer coords in the same frame as the arc centre
/// `(cx, cy)`. `outer_r` / `inner_r`: the concentric outer and inner arc
/// radii. `in_inner_corner`: whether the pixel lies in the inner box's corner
/// region for THIS corner (where the inner arc subtracts coverage); pass
/// `false` for pixels that only the outer arc reaches.
///
/// The pixel is reflected about `(cx, cy)` into the upper-right quadrant
/// before the exact intersection area is computed by
/// [`pixel_circle_area_q1`]. Shared by [`render_corner_coverage`] (the cached
/// tile renderer) and [`fill_border_ring`] (the per-pixel reference path), so
/// the two stay byte-identical by construction.
fn corner_pixel_alpha(
    px: usize,
    py: usize,
    cx: usize,
    cy: usize,
    outer_r: f64,
    inner_r: f64,
    in_inner_corner: bool,
) -> u8 {
    let (u0, u1) = if px < cx {
        ((cx - px - 1) as f64, (cx - px) as f64)
    } else {
        ((px - cx) as f64, (px + 1 - cx) as f64)
    };
    let (v0, v1) = if py < cy {
        ((cy - py - 1) as f64, (cy - py) as f64)
    } else {
        ((py - cy) as f64, (py + 1 - cy) as f64)
    };
    let area_outer = pixel_circle_area_q1(u0, u1, v0, v1, outer_r);
    let area_inner = if in_inner_corner {
        pixel_circle_area_q1(u0, u1, v0, v1, inner_r)
    } else {
        0.0
    };
    let cov = (area_outer - area_inner).clamp(0.0, 1.0);
    (cov * 255.0).round() as u8
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
/// Rounded rings (nonzero `corner_radius`) are rendered with exact-area
/// anti-aliasing: each corner pixel's alpha is the exact fraction of its area
/// falling inside the ring annulus, computed analytically by
/// [`corner_pixel_alpha`] (closed-form circular-segment integration). The
/// straight, pixel-aligned bands are solid-filled. The square fast path
/// (`corner_radius == 0`) keeps its exact slice fill — pixel-aligned edges gain
/// nothing from AA.
///
/// The caller must size `pixels` to exactly `width * height` and zero it
/// first; this function writes `color` (with a per-pixel alpha byte on rounded
/// arcs) and never clears. Thickness is clamped against half the smaller
/// dimension so an oversized value just fills the whole buffer (no panic).
///
/// # Panics
///
/// Panics if `pixels.len() != width * height`.
///
/// Test-only oracle: [`composite_ring`] replaced this on the production render
/// path — this slow per-pixel reference is kept to verify byte-exact parity
/// (see `composite_ring_matches_fill_border_ring_across_geometry_sweep`).
/// Compiled in all configurations so doc-links resolve; the attribute only
/// silences the non-test dead-code lint.
#[cfg_attr(not(test), allow(dead_code))]
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
    // We fill the straight, pixel-aligned bands solidly (their edges have no
    // diagonal, so AA adds nothing) and compute the exact pixel-∩-annulus area
    // for each corner pixel via [`corner_pixel_alpha`]. Both this path and
    // [`render_corner_coverage`] share [`corner_pixel_alpha`], so they stay
    // byte-identical and [`composite_ring`] matches this reference exactly.
    let inner_radius = r.saturating_sub(t);
    let rgb = color & 0x00FF_FFFF;
    let outer_r = r as f64;
    let inner_r = inner_radius as f64;

    // Straight bands: same four-slice layout as [`composite_ring`]. Top and
    // bottom rows fill the full width minus the corner columns; the left and
    // right columns fill the `t`-px gutters between the corner rows. Overlap
    // is harmless (the same `color` lands on shared pixels).
    for y in 0..t {
        let row = y * width;
        pixels[row + r..row + (width - r)].fill(color);
    }
    for y in (height - t)..height {
        let row = y * width;
        pixels[row + r..row + (width - r)].fill(color);
    }
    for y in r..(height - r) {
        let row = y * width;
        pixels[row..row + t].fill(color);
        pixels[row + (width - t)..row + width].fill(color);
    }

    // Four corners. The inner arc only subtracts coverage where the pixel lies
    // in the inner box's corner region for THIS corner — the `[t, r) × [t, r)`
    // window at the matching corner of the buffer. For integer pixel
    // boundaries and integer `t`, each pixel is entirely inside or outside
    // that region, so the test is a simple coordinate compare.
    //
    // Top-left: arc centre (r, r), inner corner region [t, r) × [t, r).
    for py in 0..r {
        for px in 0..r {
            let in_inner = px >= t && py >= t;
            let alpha = corner_pixel_alpha(px, py, r, r, outer_r, inner_r, in_inner);
            if alpha > 0 {
                pixels[py * width + px] = pack_premultiplied(rgb, alpha);
            }
        }
    }
    // Top-right: arc centre (width - r, r), inner region [width-r, width-t) × [t, r).
    for py in 0..r {
        for px in (width - r)..width {
            let in_inner = px < width - t && py >= t;
            let alpha = corner_pixel_alpha(px, py, width - r, r, outer_r, inner_r, in_inner);
            if alpha > 0 {
                pixels[py * width + px] = pack_premultiplied(rgb, alpha);
            }
        }
    }
    // Bottom-left: arc centre (r, height - r), inner region [t, r) × [height-r, height-t).
    for py in (height - r)..height {
        for px in 0..r {
            let in_inner = px >= t && py < height - t;
            let alpha = corner_pixel_alpha(px, py, r, height - r, outer_r, inner_r, in_inner);
            if alpha > 0 {
                pixels[py * width + px] = pack_premultiplied(rgb, alpha);
            }
        }
    }
    // Bottom-right: arc centre (width-r, height-r), inner region [width-r, width-t) × [height-r, height-t).
    for py in (height - r)..height {
        for px in (width - r)..width {
            let in_inner = px < width - t && py < height - t;
            let alpha =
                corner_pixel_alpha(px, py, width - r, height - r, outer_r, inner_r, in_inner);
            if alpha > 0 {
                pixels[py * width + px] = pack_premultiplied(rgb, alpha);
            }
        }
    }
}

/// Grow-only DIB capacity for a given logical dimension.
///
/// Rounds up to the next power of two so a smooth resize animation only
/// reallocates `O(log n)` times across its full range instead of every frame.
/// `n` is clamped to at least `1` (a zero-sized buffer is never useful).
#[must_use]
const fn next_capacity(n: i32) -> i32 {
    let n = if n < 1 { 1 } else { n as usize };
    n.next_power_of_two() as i32
}

/// Pre-render the alpha-coverage tile for ONE rounded corner (the top-left
/// quadrant) as a `radius × radius` byte buffer of per-pixel coverage
/// (`0..=255`).
///
/// This is the window-size-independent piece of the 9-slice border composite.
/// By the arc geometry of [`fill_border_ring`], a pixel `(px, py)` with
/// `px, py < radius` lies in the ring iff it is inside the OUTER corner arc
/// (centre `(radius, radius)`, radius `radius`) and outside the INNER corner
/// arc (same centre, radius `radius - thickness`). Neither test consults the
/// window's `width` or `height`, so this tile is reusable across every window
/// size that shares `(radius, thickness)`.
///
/// Coverage is the EXACT pixel-∩-annulus area computed analytically by
/// [`corner_pixel_alpha`] (closed-form circular-segment integration — see
/// `docs/src/dev-guide/borders.md`), which yields the full `0..=255` coverage
/// range instead of the 17 discrete levels a 4×4 supersampler would produce.
/// [`composite_ring`] is pixel-identical to [`fill_border_ring`] for the
/// corner region because both paths share [`corner_pixel_alpha`] (verified by
/// the parity tests in `mod tests`). Returns an empty `Vec` for
/// `radius == 0` (square ring: no corners to pre-render).
///
/// `radius` and `thickness` are the ALREADY-CLAMPED effective values — i.e.
/// `radius = corner_radius.min(half)` and `thickness = thickness.min(half)`
/// where `half = width.min(height) / 2`, exactly as [`fill_border_ring`]
/// clamps. Callers key the tile cache by these effective values so a tiny
/// window (whose radius clamps down) fetches the right tile.
#[must_use]
pub(crate) fn render_corner_coverage(radius: usize, thickness: usize) -> Vec<u8> {
    if radius == 0 {
        return Vec::new();
    }
    let r = radius;
    let t = thickness.min(r);
    let inner_radius = r.saturating_sub(t);
    let outer_r = r as f64;
    let inner_r = inner_radius as f64;
    let mut coverage = vec![0u8; r * r];
    for py in 0..r {
        for px in 0..r {
            // Inner arc only reaches pixels past the inner box's top-left
            // corner `(t, t)`; for integer pixel boundaries and integer `t`
            // each pixel lies entirely inside or outside that region, never
            // straddling.
            let in_inner_corner = px >= t && py >= t;
            coverage[py * r + px] =
                corner_pixel_alpha(px, py, r, r, outer_r, inner_r, in_inner_corner);
        }
    }
    coverage
}

/// Placement of one corner tile: the screen-space origin of the tile's `(0,0)`
/// plus mirror flags.
///
/// The other three corners reuse the single top-left tile rendered by
/// [`render_corner_coverage`] via reflection. Reflection is exact because the
/// ring annulus is symmetric about both axes through the arc centre: a pixel
/// `(px, py)` reflected to `(r-1-px, py)` (or `(px, r-1-py)`) presents the same
/// `(u, v)` distances to the arc centre, so [`corner_pixel_alpha`] yields the
/// same coverage for the mirrored layout.
#[derive(Clone, Copy)]
struct CornerPos {
    /// Screen column of the tile's left edge.
    ox: usize,
    /// Screen row of the tile's top edge.
    oy: usize,
    /// Mirror horizontally (top-right / bottom-right corners).
    flip_x: bool,
    /// Mirror vertically (bottom-left / bottom-right corners).
    flip_y: bool,
}

/// Layout of the destination pixel buffer for [`composite_ring`].
///
/// `stride` is the number of `u32`s per row and may exceed `width` when
/// compositing into a grow-only DIB whose allocation is wider than the logical
/// image (the DIB's own row stride, which `UpdateLayeredWindow` reads with
/// `size = (width, height)`). For a tight buffer, pass `stride == width`.
#[derive(Clone, Copy)]
pub(crate) struct BufferLayout {
    stride: usize,
    width: usize,
    height: usize,
}

/// Blit one cached corner coverage tile into `pixels` at [`CornerPos`].
///
/// `stride` is the number of `u32`s per row of `pixels` — it may exceed the
/// logical image `width` when compositing into a grow-only DIB whose
/// allocation is wider than the current ring. Zero-coverage pixels are skipped
/// (leaving the pre-zeroed buffer at `0`), matching [`fill_border_ring`]'s
/// "write only when `hits > 0`" rule.
///
/// Internal helper — the caller ([`composite_ring`]) upholds `ox + r ≤ width`,
/// `oy + r ≤ height`, and `stride ≥ width`, so all writes stay in-bounds.
fn blit_corner(
    pixels: &mut [u32],
    stride: usize,
    coverage: &[u8],
    r: usize,
    rgb: u32,
    pos: CornerPos,
) {
    let CornerPos {
        ox,
        oy,
        flip_x,
        flip_y,
    } = pos;
    for ty in 0..r {
        let y = if flip_y { oy + r - 1 - ty } else { oy + ty };
        for tx in 0..r {
            let cov = coverage[ty * r + tx];
            if cov > 0 {
                let x = if flip_x { ox + r - 1 - tx } else { ox + tx };
                pixels[y * stride + x] = pack_premultiplied(rgb, cov);
            }
        }
    }
}

/// Composite the border ring into `pixels` from a cached corner coverage tile
/// plus four solid edge fills — the resize-frame hot-path replacement for
/// [`fill_border_ring`].
///
/// Instead of re-running per-pixel area integration on every `WM_SIZE`, this blits the
/// pre-rendered `corner_coverage` (produced once per `(radius, thickness)` by
/// [`render_corner_coverage`]) into the four corners and fills the straight
/// edge bands with a flat `color`. The output is byte-identical to
/// `fill_border_ring(pixels, width, height, thickness, corner_radius, color)`
/// for the same arguments — the parity tests in `mod tests` pin this — but the
/// per-frame cost drops from `O(perimeter × 16)` float tests plus a fresh
/// `CreateDIBSection` to `O(perimeter)` solid fills plus four small blits.
///
/// `layout` ([`BufferLayout`]) gives the destination buffer's row `stride`
/// alongside the logical image `width` / `height`; `stride` may exceed `width`
/// when compositing into a grow-only DIB whose allocation is wider than the
/// image (the DIB's own row stride, which `UpdateLayeredWindow` reads with
/// `size = (width, height)`). Pass `stride = width` for a tight buffer.
///
/// Contract — geometry identical to [`fill_border_ring`]:
/// - `pixels.len()` must be at least `stride * height` (panics otherwise).
/// - The `width × height` region at `pixels[y*stride + x]` must be pre-zeroed;
///   ring pixels are written and the interior + outside-arc corner cutouts are
///   left at `0`.
/// - `corner_coverage` must be the tile for the EFFECTIVE clamped geometry,
///   i.e. `render_corner_coverage(r, t)` with `r`/`t` clamped as below. It is
///   ignored when the effective radius is `0` (square ring), so an empty slice
///   is acceptable there. Must contain at least `r * r` bytes otherwise.
/// - `color` should carry `0xFF` alpha (the rounded path derives edge alpha
///   from coverage, which is `255` on straight bands).
///
/// # Panics
///
/// Panics if `pixels.len() < stride * height`, or if `corner_coverage` is
/// shorter than `r * r` for a nonzero effective radius.
pub(crate) fn composite_ring(
    pixels: &mut [u32],
    layout: BufferLayout,
    thickness: usize,
    corner_radius: usize,
    corner_coverage: &[u8],
    color: u32,
) {
    let BufferLayout {
        stride,
        width,
        height,
    } = layout;
    assert!(
        pixels.len() >= stride * height,
        "pixels buffer must hold at least stride * height"
    );
    if width == 0 || height == 0 {
        return;
    }
    // Mirror `fill_border_ring`'s clamping exactly so the cached tile is keyed
    // by the same effective (r, t) the rasteriser would use.
    let half = width.min(height) / 2;
    let t = thickness.min(half);
    if t == 0 {
        return;
    }
    let r = corner_radius.min(half);
    let rgb = color & 0x00FF_FFFF;

    // Four straight edge fills between the corner tiles. For `r == 0` these
    // alone reproduce `fill_border_ring`'s square fast path (the ranges grow
    // to cover the full row/column; overlaps land on identical `color`).
    // Each row write stays within the logical `width` (the stride padding to
    // the right is never touched).
    // Top + bottom bands: the full row minus the corner columns.
    for y in 0..t {
        let row = y * stride;
        pixels[row + r..row + (width - r)].fill(color);
    }
    for y in (height - t)..height {
        let row = y * stride;
        pixels[row + r..row + (width - r)].fill(color);
    }
    // Left + right bands: the `t`-px columns between the corner rows.
    for y in r..(height - r) {
        let row = y * stride;
        pixels[row..row + t].fill(color);
        pixels[row + (width - t)..row + width].fill(color);
    }

    if r == 0 {
        return;
    }
    debug_assert!(
        corner_coverage.len() >= r * r,
        "corner_coverage must hold at least r*r bytes for a nonzero radius"
    );
    // Four corners: top-left is the canonical tile; the others are reflections.
    blit_corner(
        pixels,
        stride,
        corner_coverage,
        r,
        rgb,
        CornerPos {
            ox: 0,
            oy: 0,
            flip_x: false,
            flip_y: false,
        },
    );
    blit_corner(
        pixels,
        stride,
        corner_coverage,
        r,
        rgb,
        CornerPos {
            ox: width - r,
            oy: 0,
            flip_x: true,
            flip_y: false,
        },
    );
    blit_corner(
        pixels,
        stride,
        corner_coverage,
        r,
        rgb,
        CornerPos {
            ox: 0,
            oy: height - r,
            flip_x: false,
            flip_y: true,
        },
    );
    blit_corner(
        pixels,
        stride,
        corner_coverage,
        r,
        rgb,
        CornerPos {
            ox: width - r,
            oy: height - r,
            flip_x: true,
            flip_y: true,
        },
    );
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
    /// Pinning this boundary behaviour keeps the coverage curve stable — a
    /// regression to `<` would thin the rendered arc by one sub-pixel and
    /// subtly dim the AA boundary pixels.
    #[test]
    fn in_rounded_rect_arc_boundary_point_is_inside() {
        // 10×10 rect, radius 5. Top-left arc centre is at (5, 5).
        // Point (5, 0): dx = 0, dy = -5 → 0 + 25 == 25 == cr² → inside (<=).
        assert!(in_rounded_rect(5.0, 0.0, 0.0, 0.0, 10.0, 10.0, 5.0));
        // Point just outside the arc (4, 0): dx = -1, dy = -5 → 1 + 25 = 26 > 25.
        assert!(!in_rounded_rect(4.0, 0.0, 0.0, 0.0, 10.0, 10.0, 5.0));
    }

    /// `pixel_circle_area_q1` for the full `[0, R] × [0, R]` quadrant bounding
    /// box equals the exact quarter-circle area `πR²/4`.
    #[test]
    fn pixel_circle_area_q1_matches_quarter_circle() {
        let r = 5.0_f64;
        let area = pixel_circle_area_q1(0.0, r, 0.0, r, r);
        let expected = std::f64::consts::PI * r * r / 4.0;
        assert!(
            (area - expected).abs() < 1e-9,
            "quarter-circle area: got {area}, expected {expected}"
        );
    }

    /// A pixel entirely outside the circle has zero area.
    #[test]
    fn pixel_circle_area_q1_outside_is_zero() {
        // Pixel [6,7]×[6,7], circle radius 5 → closest corner (6,6) at distance √72 > 5.
        assert_eq!(pixel_circle_area_q1(6.0, 7.0, 6.0, 7.0, 5.0), 0.0);
    }

    /// A pixel entirely inside the circle has full pixel area.
    #[test]
    fn pixel_circle_area_q1_inside_is_pixel_area() {
        // Pixel [0,1]×[0,1], circle radius 5 → farthest corner (1,1) at distance √2 < 5.
        assert_eq!(pixel_circle_area_q1(0.0, 1.0, 0.0, 1.0, 5.0), 1.0);
        // Non-unit pixel [0.5,1.5]×[0.5,1.5] entirely inside → area = 1.0.
        assert_eq!(pixel_circle_area_q1(0.5, 1.5, 0.5, 1.5, 5.0), 1.0);
    }

    /// `pack_premultiplied` scales RGB channels by `alpha/255` (round-to-nearest)
    /// and preserves the alpha byte in the high 8 bits.
    #[test]
    fn pack_premultiplied_scales_channels() {
        // White at α=128: each channel = (255*128+127)/255 = 128.
        assert_eq!(pack_premultiplied(0x00FF_FFFF, 128), 0x8080_8080);
        // #AABBCC at α=128: R=(170*128+127)/255=85, G=(187*128+127)/255=94, B=(204*128+127)/255=102.
        assert_eq!(pack_premultiplied(0x00AA_BBCC, 0x80), 0x8055_5E66);
    }

    /// `pack_premultiplied` is identity at α=255 (premultiplying by 1.0).
    #[test]
    fn pack_premultiplied_identity_at_full_alpha() {
        assert_eq!(pack_premultiplied(0x00FF_FFFF, 0xFF), 0xFFFF_FFFF);
        assert_eq!(pack_premultiplied(0x00AA_BBCC, 0xFF), 0xFFAA_BBCC);
    }

    /// `pack_premultiplied` at α=0 produces all-zero (fully transparent).
    #[test]
    fn pack_premultiplied_zero_alpha_is_zero() {
        assert_eq!(pack_premultiplied(0x00FF_FFFF, 0), 0);
        assert_eq!(pack_premultiplied(0x00AA_BBCC, 0), 0);
    }

    /// `circle_segment_antiderivative` is zero at the centre, negative at
    /// `y = -r`, positive at `y = r`, and monotonic in between.
    #[test]
    fn circle_segment_antiderivative_endpoints_and_monotonic() {
        let r = 4.0_f64;
        assert_eq!(circle_segment_antiderivative(0.0, r), 0.0);
        let g_neg = circle_segment_antiderivative(-r, r);
        let g_pos = circle_segment_antiderivative(r, r);
        assert!(g_neg < 0.0, "G(-r) must be negative, got {g_neg}");
        assert!(g_pos > 0.0, "G(r) must be positive, got {g_pos}");
        // Symmetric: G(-r) == -G(r) (antisymmetric about origin).
        assert!(
            (g_neg + g_pos).abs() < 1e-9,
            "G(-r) + G(r) must be 0, got {}",
            g_neg + g_pos
        );
        // G(r) = r²π/4 (quarter-circle area).
        let quarter = r * r * std::f64::consts::PI / 4.0;
        assert!(
            (g_pos - quarter).abs() < 1e-9,
            "G(r) must be πr²/4, got {g_pos}, expected {quarter}"
        );
        // Monotonic: midpoint between 0 and r is between G(0) and G(r).
        let g_mid = circle_segment_antiderivative(r * 0.5, r);
        assert!(g_mid > 0.0 && g_mid < g_pos, "G must be monotonic");
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
        // edge with a stray half-pixel. Exact-area integration produces many
        // distinct values as the arc sweeps across pixels.
        assert!(
            partial_alphas.len() >= 2,
            "AA arc must show a graded coverage gradient, found only {:?}",
            partial_alphas
        );
        // The ring shape is symmetric about the buffer center, so an unbiased
        // area calculation must yield a symmetric alpha map. A bug that shifts
        // the arc centre or reflects incorrectly breaks this symmetry.
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
        // Premultiplied output: RGB scaled by alpha/255, alpha byte preserved.
        assert_eq!(recolored, pack_premultiplied(0x00AA_BBCC, 0x80));
        assert_eq!(recolored >> 24, 0x80, "alpha byte must be preserved");
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

    // ── 9-slice composite parity ──────────────────────────────────────
    //
    // `composite_ring` is the resize-frame hot-path replacement for
    // `fill_border_ring`. Its correctness contract is byte-exact parity: given
    // the same buffer + args (and the matching cached corner tile), it MUST
    // produce identical output. The sweep below covers square rings (r == 0),
    // rounded rings, exact `2R` widths (corners meet edge-to-edge), `2R+1`
    // (one-pixel straight band), degenerate radii that clamp down, and a
    // realistic full-size window.

    /// Mirror `fill_border_ring`'s internal clamping so the test renders the
    /// coverage tile keyed by the SAME effective `(t, r)` the compositor uses.
    fn effective_geometry(
        width: usize,
        height: usize,
        thickness: usize,
        corner_radius: usize,
    ) -> (usize, usize) {
        let half = width.min(height) / 2;
        (thickness.min(half), corner_radius.min(half))
    }

    /// Render the matching corner tile and diff `composite_ring` against
    /// `fill_border_ring` pixel by pixel, failing with the offending
    /// coordinate + effective geometry for fast triage.
    fn assert_parity(width: usize, height: usize, thickness: usize, corner_radius: usize) {
        let (t, r) = effective_geometry(width, height, thickness, corner_radius);
        let coverage = render_corner_coverage(r, t);
        let colored = pack_bgra(color(0x42, 0x7A, 0xFF), 0xFF);
        let mut expected = vec![0u32; width * height];
        let mut actual = vec![0u32; width * height];
        fill_border_ring(
            &mut expected,
            width,
            height,
            thickness,
            corner_radius,
            colored,
        );
        composite_ring(
            &mut actual,
            BufferLayout {
                stride: width,
                width,
                height,
            },
            thickness,
            corner_radius,
            &coverage,
            colored,
        );
        for y in 0..height {
            for x in 0..width {
                let idx = y * width + x;
                assert_eq!(
                    actual[idx], expected[idx],
                    "composite != fill_border_ring at ({x},{y}) for \
                     W={width} H={height} T={thickness} R={corner_radius} \
                     (effective t={t} r={r})",
                );
            }
        }
    }

    /// Byte-exact parity across the geometry sweep. This is THE correctness
    /// gate for the optimisation: if `composite_ring` ever drifts from
    /// `fill_border_ring`, one of these cases fails with the exact coordinate.
    #[test]
    fn composite_ring_matches_fill_border_ring_across_geometry_sweep() {
        // Square rings (r == 0): edges-only composite must equal the slice fill.
        assert_parity(3, 3, 1, 0);
        assert_parity(5, 5, 2, 0);
        assert_parity(6, 3, 1, 0);
        assert_parity(4, 4, 100, 0); // thickness clamps to half.

        // Rounded rings of assorted sizes (the AA-producing cases).
        assert_parity(11, 11, 1, 4);
        assert_parity(11, 11, 3, 4);
        assert_parity(40, 40, 3, 15);
        assert_parity(33, 21, 2, 9);

        // Exact 2R width/height: corner tiles meet edge-to-edge (no straight
        // band between them).
        assert_parity(10, 20, 2, 5); // width == 2*r
        assert_parity(20, 10, 2, 5); // height == 2*r
        // 2R+1: exactly one pixel of straight band between the corners.
        assert_parity(11, 22, 2, 5);

        // Degenerate: requested radius ≥ half the dimension, so it clamps down.
        // The compositor must fetch the clamped-r tile, not the requested one.
        assert_parity(5, 5, 1, 8); // half=2 → r clamps 8→2.
        assert_parity(7, 9, 3, 20); // half=3 → r clamps 20→3, t=3 → inner 0.

        // Realistic full-size window (the actual jank scenario).
        assert_parity(1600, 900, 3, 11); // Rounded + thickness 3.
        assert_parity(1600, 900, 3, 7); // RoundedSmall + thickness 3.

        // Zero thickness: both paths are no-ops (fully transparent).
        assert_parity(20, 20, 0, 5);
    }

    /// The grow-only DIB path composites with `stride > width` (the buffer is
    /// allocated at power-of-two capacity, wider than the logical image). This
    /// proves that path still matches `fill_border_ring` pixel-for-pixel in the
    /// logical `w × h` region, and that the stride-padding columns are left
    /// untouched at `0`.
    #[test]
    fn composite_ring_with_wider_stride_matches_fill_border_ring() {
        let (width, height, thickness, corner_radius) = (40, 30, 3, 9);
        let (t, r) = effective_geometry(width, height, thickness, corner_radius);
        let coverage = render_corner_coverage(r, t);
        let colored = pack_bgra(color(0x42, 0x7A, 0xFF), 0xFF);

        // Tight reference via fill_border_ring.
        let mut expected = vec![0u32; width * height];
        fill_border_ring(
            &mut expected,
            width,
            height,
            thickness,
            corner_radius,
            colored,
        );

        // Wider-stride buffer (stride = width + 11, like a pow2 capacity slack).
        // Extra rows beyond `height` also pad the buffer vertically.
        let stride = width + 11;
        let buf_h = height + 5;
        let mut actual = vec![0u32; stride * buf_h];
        composite_ring(
            &mut actual,
            BufferLayout {
                stride,
                width,
                height,
            },
            thickness,
            corner_radius,
            &coverage,
            colored,
        );

        // The logical w×h region (read at `stride`) must match the tight output.
        for y in 0..height {
            for x in 0..width {
                assert_eq!(
                    actual[y * stride + x],
                    expected[y * width + x],
                    "stride composite != fill_border_ring at ({x},{y})"
                );
            }
        }
        // Stride padding to the right of the image must stay zero (untouched).
        for y in 0..height {
            for x in width..stride {
                assert_eq!(
                    actual[y * stride + x],
                    0,
                    "stride padding polluted at ({x},{y})"
                );
            }
        }
        // Rows below the image must stay zero too.
        for y in height..buf_h {
            for x in 0..stride {
                assert_eq!(
                    actual[y * stride + x],
                    0,
                    "vertical padding polluted at ({x},{y})"
                );
            }
        }
    }

    /// `render_corner_coverage` is the source of the graded AA: for a realistic
    /// corner it emits the full coverage range — `0` (outside the outer arc /
    /// interior), `255` (the solid straight-band part of the tile), and several
    /// intermediate levels along the arc. A binary edge (only 0 / 255) would
    /// mean the area integration regressed to a step function.
    #[test]
    fn render_corner_coverage_emits_graded_arc() {
        // r=15, t=3 (matches the 40×40 rounded-ring case).
        let cov = render_corner_coverage(15, 3);
        assert_eq!(cov.len(), 15 * 15);
        let distinct: std::collections::HashSet<u8> = cov.iter().copied().collect();
        assert!(distinct.contains(&0), "extreme corner + interior must be 0");
        assert!(
            distinct.contains(&255),
            "solid straight-band part of the tile must be fully covered"
        );
        assert!(
            distinct.len() >= 3,
            "arc must show a graded coverage gradient, got {distinct:?}"
        );
    }

    /// `render_corner_coverage(0, _)` is the square-ring case: no corners, so
    /// the tile is empty and `composite_ring` must fall back to edge fills
    /// alone — still byte-identical to `fill_border_ring`.
    #[test]
    fn render_corner_coverage_zero_radius_is_empty() {
        assert!(render_corner_coverage(0, 3).is_empty());
        let mut expected = vec![0u32; 6 * 4];
        let mut actual = vec![0u32; 6 * 4];
        let colored = pack_bgra(color(0, 0, 0), 0xFF);
        fill_border_ring(&mut expected, 6, 4, 1, 0, colored);
        composite_ring(
            &mut actual,
            BufferLayout {
                stride: 6,
                width: 6,
                height: 4,
            },
            1,
            0,
            &[],
            colored,
        );
        assert_eq!(actual, expected);
    }

    // ── next_capacity (pure grow-only allocator) ──────────────────────

    /// `next_capacity` clamps non-positive dimensions to `1` so a zero-sized
    /// DIB is never requested (a `CreateDIBSection` with `biWidth <= 0` would
    /// fail). This pins the `n < 1 → 1` branch — a regression that dropped the
    /// clamp would surface as `next_power_of_two` panicking on a negative
    /// `usize` cast.
    #[test]
    fn next_capacity_non_positive_clamps_to_one() {
        assert_eq!(next_capacity(0), 1);
        assert_eq!(next_capacity(-1), 1);
        assert_eq!(next_capacity(-1024), 1);
    }

    /// `next_capacity` rounds UP to the next power of two so the DIB is
    /// grow-only: a smooth resize animation only crosses a pow2 boundary
    /// `O(log n)` times. Exact powers of two are returned unchanged.
    #[test]
    fn next_capacity_rounds_up_to_next_power_of_two() {
        assert_eq!(next_capacity(1), 1);
        assert_eq!(next_capacity(2), 2);
        assert_eq!(next_capacity(3), 4);
        assert_eq!(next_capacity(5), 8);
        assert_eq!(next_capacity(1024), 1024, "exact pow2 stays put");
        assert_eq!(next_capacity(1025), 2048, "one past pow2 rounds up");
        assert_eq!(next_capacity(1600), 2048, "realistic window width");
    }

    // ── composite_ring early-return + panic contracts ─────────────────

    /// `composite_ring` with `width == 0` or `height == 0` is a documented
    /// no-op (the buffer is left untouched). Pin this independently of the
    /// parity sweep, which never passes a zero dimension. Uses a non-zero
    /// sentinel fill so any accidental write is observable.
    #[test]
    fn composite_ring_zero_dimension_leaves_buffer_untouched() {
        // width == 0 branch.
        let mut buf_w = vec![0xDEAD_BEEF_u32; 64];
        composite_ring(
            &mut buf_w,
            BufferLayout {
                stride: 8,
                width: 0,
                height: 8,
            },
            2,
            0,
            &[],
            0xFFFF00FF,
        );
        assert!(
            buf_w.iter().all(|&p| p == 0xDEAD_BEEF),
            "zero-width composite must not write any pixel"
        );

        // height == 0 branch.
        let mut buf_h = vec![0xDEAD_BEEF_u32; 64];
        composite_ring(
            &mut buf_h,
            BufferLayout {
                stride: 8,
                width: 8,
                height: 0,
            },
            2,
            0,
            &[],
            0xFFFF00FF,
        );
        assert!(
            buf_h.iter().all(|&p| p == 0xDEAD_BEEF),
            "zero-height composite must not write any pixel"
        );
    }

    /// `composite_ring` with `thickness` clamped to `0` (either explicitly 0
    /// or `≥ half`) hits the `if t == 0 { return; }` branch BEFORE any edge
    /// fill or corner blit. Buffer must stay untouched. This is the contract
    /// `CachedSurface::render_ring` relies on for degenerate tiny-window
    /// paints.
    #[test]
    fn composite_ring_zero_thickness_leaves_buffer_untouched() {
        let mut buf = vec![0xDEAD_BEEF_u32; 100];
        composite_ring(
            &mut buf,
            BufferLayout {
                stride: 10,
                width: 10,
                height: 10,
            },
            0,
            4, // would normally trigger corner blits, but t==0 returns first
            &[],
            0xFFFF00FF,
        );
        assert!(
            buf.iter().all(|&p| p == 0xDEAD_BEEF),
            "zero-thickness composite must not write any pixel"
        );
    }

    /// Documented `# Panics`: caller must size `pixels` to at least
    /// `stride * height`. Pin the contract guard so a future refactor that
    /// drops the `assert!` is caught.
    #[should_panic(expected = "pixels buffer must hold at least stride * height")]
    #[test]
    fn composite_ring_panics_when_buffer_smaller_than_stride_times_height() {
        // stride*height = 8*2 = 16, but buf.len() = 10.
        let mut buf = vec![0u32; 10];
        composite_ring(
            &mut buf,
            BufferLayout {
                stride: 8,
                width: 8,
                height: 2,
            },
            1,
            0,
            &[],
            0xFFFF00FF,
        );
    }

    /// Documented contract: `corner_coverage` must hold at least `r * r` bytes
    /// for a nonzero effective radius. In debug the `debug_assert!` fires with
    /// a descriptive message; in release the subsequent `blit_corner` index
    /// goes out of bounds and panics from stdlib. Either way this test sees a
    /// panic — bare `#[should_panic]` (no `expected`) is intentional so the
    /// test passes in both profiles.
    #[should_panic]
    #[test]
    fn composite_ring_panics_when_corner_coverage_shorter_than_r_squared() {
        // half = 5, r = 4.min(5) = 4 → coverage needs ≥ 16 bytes; we pass 3.
        let mut buf = vec![0u32; 100];
        composite_ring(
            &mut buf,
            BufferLayout {
                stride: 10,
                width: 10,
                height: 10,
            },
            1,
            4,
            &[0u8, 0, 0],
            0xFFFF00FF,
        );
    }

    // ── render_corner_coverage: thickness clamp + invariants ──────────

    /// `render_corner_coverage` clamps `thickness` to `radius` internally
    /// (`t = thickness.min(r)`), so an oversized thickness matches the
    /// thickness-equals-radius case exactly. Guards against removing the
    /// `.min(r)` clamp (which would let `inner_radius = r.saturating_sub(t)`
    /// still be correct but would mis-key the cache in `render_ring`).
    #[test]
    fn render_corner_coverage_oversized_thickness_clamps_to_radius() {
        let cov_clamped = render_corner_coverage(5, 100);
        let cov_at_radius = render_corner_coverage(5, 5);
        assert_eq!(
            cov_clamped, cov_at_radius,
            "thickness > radius must clamp to t == radius"
        );
    }

    /// Tile size is exactly `radius * radius` bytes across a range of radii.
    /// The compositor indexes `coverage[ty * r + tx]` with `tx, ty < r`, so a
    /// short tile would panic at blit time. The existing `emits_graded_arc`
    /// test only checks r=15; this pins the size invariant parametrically.
    #[test]
    fn render_corner_coverage_size_is_radius_squared() {
        for r in [1usize, 2, 3, 5, 8, 12] {
            assert_eq!(
                render_corner_coverage(r, r / 2).len(),
                r * r,
                "tile for r={r} must be r*r bytes"
            );
        }
    }

    /// The arc test `(sx - rf)² + (sy - rf)² ≤ rf²` is symmetric under
    /// `x ↔ y`, so the coverage tile must equal its own transpose
    /// (`cov[y*r + x] == cov[x*r + y]`). This is the geometric property that
    /// lets `blit_corner`'s `flip_x`/`flip_y` reproduce the other three
    /// corners from a single tile. A regression that broke the sampler's
    /// symmetry would also break bottom-left/top-right parity, but this pins
    /// the invariant directly with a precise coordinate.
    #[test]
    fn render_corner_coverage_tile_is_transpose_symmetric() {
        let r = 12;
        let cov = render_corner_coverage(r, 4);
        for y in 0..r {
            for x in 0..r {
                assert_eq!(
                    cov[y * r + x],
                    cov[x * r + y],
                    "tile must be symmetric under transpose at ({x},{y})"
                );
            }
        }
    }

    // ── blit_corner: flip + zero-coverage + offset/stride ─────────────
    //
    // `blit_corner` is the leaf that places the cached corner tile into one of
    // the four screen corners via `CornerPos` reflection flags. The parity
    // sweep covers it integration-style; these pin each flip combination
    // directly so a regression in the reflection index math is caught with a
    // precise coordinate, not a diff against the oracle.

    /// Build a small identifiable coverage tile where each byte encodes its
    /// own `(tx, ty)` coordinate as `tx*16 + ty + 1`. The `+ 1` keeps every
    /// byte non-zero so `blit_corner`'s zero-coverage skip branch never fires
    /// — the mirror direction is then observable at every destination pixel.
    fn identifiable_coverage(r: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(r * r);
        for ty in 0..r {
            for tx in 0..r {
                v.push((tx * 16 + ty + 1) as u8);
            }
        }
        v
    }

    /// No-flip blit writes the tile verbatim at the screen origin (the
    /// top-left-corner case in `composite_ring`).
    #[test]
    fn blit_corner_no_flip_writes_tile_verbatim_at_origin() {
        let r = 4;
        let cov = identifiable_coverage(r);
        let mut buf = vec![0u32; r * r];
        blit_corner(
            &mut buf,
            r,
            &cov,
            r,
            0x00FF_FFFF,
            CornerPos {
                ox: 0,
                oy: 0,
                flip_x: false,
                flip_y: false,
            },
        );
        for ty in 0..r {
            for tx in 0..r {
                let expected_alpha = cov[ty * r + tx];
                assert_eq!(
                    buf[ty * r + tx],
                    pack_premultiplied(0x00FF_FFFF, expected_alpha),
                    "no-flip blit wrong at ({tx},{ty})"
                );
            }
        }
    }

    /// `flip_x = true` mirrors the tile horizontally: the rightmost source
    /// column lands at the leftmost destination column (the top-right corner).
    #[test]
    fn blit_corner_flip_x_mirrors_horizontally() {
        let r = 4;
        let cov = identifiable_coverage(r);
        let mut buf = vec![0u32; r * r];
        blit_corner(
            &mut buf,
            r,
            &cov,
            r,
            0x00FF_FFFF,
            CornerPos {
                ox: 0,
                oy: 0,
                flip_x: true,
                flip_y: false,
            },
        );
        for ty in 0..r {
            for tx in 0..r {
                let expected_alpha = cov[ty * r + (r - 1 - tx)];
                assert_eq!(
                    buf[ty * r + tx],
                    pack_premultiplied(0x00FF_FFFF, expected_alpha),
                    "flip_x blit wrong at ({tx},{ty})"
                );
            }
        }
    }

    /// `flip_y = true` mirrors the tile vertically (the bottom-left corner).
    #[test]
    fn blit_corner_flip_y_mirrors_vertically() {
        let r = 4;
        let cov = identifiable_coverage(r);
        let mut buf = vec![0u32; r * r];
        blit_corner(
            &mut buf,
            r,
            &cov,
            r,
            0x00FF_FFFF,
            CornerPos {
                ox: 0,
                oy: 0,
                flip_x: false,
                flip_y: true,
            },
        );
        for ty in 0..r {
            for tx in 0..r {
                let expected_alpha = cov[(r - 1 - ty) * r + tx];
                assert_eq!(
                    buf[ty * r + tx],
                    pack_premultiplied(0x00FF_FFFF, expected_alpha),
                    "flip_y blit wrong at ({tx},{ty})"
                );
            }
        }
    }

    /// `flip_x && flip_y` rotates the tile 180° (the bottom-right corner).
    #[test]
    fn blit_corner_flip_xy_rotates_180() {
        let r = 4;
        let cov = identifiable_coverage(r);
        let mut buf = vec![0u32; r * r];
        blit_corner(
            &mut buf,
            r,
            &cov,
            r,
            0x00FF_FFFF,
            CornerPos {
                ox: 0,
                oy: 0,
                flip_x: true,
                flip_y: true,
            },
        );
        for ty in 0..r {
            for tx in 0..r {
                let expected_alpha = cov[(r - 1 - ty) * r + (r - 1 - tx)];
                assert_eq!(
                    buf[ty * r + tx],
                    pack_premultiplied(0x00FF_FFFF, expected_alpha),
                    "flip_xy blit wrong at ({tx},{ty})"
                );
            }
        }
    }

    /// Zero-coverage bytes are skipped (the destination is left at its
    /// pre-zeroed value), matching `fill_border_ring`'s "write only when
    /// hits > 0" rule. A regression that wrote `(0 << 24) | rgb` would paint
    /// a transparent-RGB value over the cleared buffer and break parity.
    #[test]
    fn blit_corner_skips_zero_coverage_pixels() {
        // r=3 tile with a zero at the centre (tx=1, ty=1); rest non-zero.
        let cov = vec![1u8, 2, 3, 4, 0, 6, 7, 8, 9];
        let mut buf = vec![0u32; 9];
        blit_corner(
            &mut buf,
            3,
            &cov,
            3,
            0x00FF_FFFF,
            CornerPos {
                ox: 0,
                oy: 0,
                flip_x: false,
                flip_y: false,
            },
        );
        // The zero-coverage centre is at tile coords (tx=1, ty=1) → index 4
        // in a row-major r=3 tile (ty*r + tx = 1*3 + 1 = 4).
        assert_eq!(buf[4], 0, "zero-coverage pixel must be skipped");
        assert_eq!(
            buf[0],
            pack_premultiplied(0x00FF_FFFF, cov[0]),
            "non-zero pixel at (0,0) must be written"
        );
    }

    /// `blit_corner` honours `ox`/`oy` to place the tile at a non-origin
    /// screen position within a larger, strided buffer — the production
    /// layout for non-top-left corners (which pass `ox = width - r`, stride
    /// = `buf_w`). Verifies the stride-offset destination indexing that the
    /// grow-only DIB path depends on.
    #[test]
    fn blit_corner_places_tile_at_offset_with_stride() {
        let r = 2;
        let cov = identifiable_coverage(r);
        let stride = 8;
        let width = 6;
        let height = 5;
        let mut buf = vec![0u32; stride * height];
        // Bottom-right corner placement with 180° rotation (the production
        // bottom-right case).
        let ox = width - r;
        let oy = height - r;
        blit_corner(
            &mut buf,
            stride,
            &cov,
            r,
            0x00FF_FFFF,
            CornerPos {
                ox,
                oy,
                flip_x: true,
                flip_y: true,
            },
        );
        for ty in 0..r {
            for tx in 0..r {
                let dst_x = ox + (r - 1 - tx);
                let dst_y = oy + (r - 1 - ty);
                let src = cov[ty * r + tx];
                assert_eq!(
                    buf[dst_y * stride + dst_x],
                    pack_premultiplied(0x00FF_FFFF, src),
                    "offset+stride blit wrong at src ({tx},{ty}) -> dst ({dst_x},{dst_y})"
                );
            }
        }
        // Pixel outside the placed tile stays untouched.
        assert_eq!(buf[0], 0, "buffer outside the tile placement must stay 0");
    }
}
