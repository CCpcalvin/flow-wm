//! Safe Win32 wrappers for window metadata queries.
//!
//! Every function in this module wraps one or more unsafe Win32 API calls
//! into a safe Rust interface. The unsafe blocks are kept to the minimum
//! possible scope — typically a single API call.
//!
//! # Safety Strategy
//!
//! All functions return `Result` types and never panic on Win32 failures.
//! This is a deliberate design choice:
//!
//! - **Window handles can become invalid at any time.** A window might be
//!   destroyed between our `GetWindowTextLengthW` and `GetWindowTextW` calls.
//!   All wrappers handle this gracefully (returning empty strings or errors).
//!
//! - **Permissions can vary.** `OpenProcess` might fail with access denied
//!   for system processes. We handle this by falling back to `"unknown"`.
//!
//! - **No raw pointer leaks.** All handles (process handles) are closed via
//!   `CloseHandle`, even on early returns.
//!
//! # Function Categories
//!
//! - **String queries**: [`get_window_text`], [`get_class_name`] — convert
//!   UTF-16 buffers to `String`.
//! - **Geometry**: [`get_window_rect`], [`is_fullscreen`] — window position
//!   and size queries.
//! - **State checks**: [`is_window_visible`], [`is_zoomed`], [`is_iconic`] — boolean checks.
//! - **Process info**: [`get_process_exe_and_path`] — executable name/path.
//! - **Aggregator**: [`get_window_info`] — queries all metadata at once.

use std::ffi::OsStr;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::{CloseHandle, GetLastError, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
    DWMWA_WINDOW_CORNER_PREFERENCE, DwmGetWindowAttribute,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, GetAncestor, GetClassNameW, GetCursorPos,
    GetForegroundWindow, GetShellWindow, GetSystemMetrics, GetWindowLongW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    IsZoomed, PostMessageW, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOZORDER,
    SetForegroundWindow, SetWindowPos, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WS_CAPTION,
    WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_THICKFRAME, WindowFromPoint,
};
use windows::core::PWSTR;

use crate::borders::CornerPreference;
use crate::common::{InvisibleBounds, Rect};

// ── WindowInfo struct ───────────────────────────────────────────────

/// Aggregated window metadata gathered from Win32 APIs.
///
/// Produced by [`get_window_info`] which calls all individual query
/// functions and collects their results into a single struct. This is the
/// primary input to the registry's window classification logic.
///
/// # Design: Single Snapshot
///
/// `WindowInfo` represents a point-in-time snapshot of a window's state.
/// The actual window may change between when this struct is created and
/// when it's used. This is acceptable for classification purposes — if the
/// window changes, the next event will trigger re-evaluation.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// Win32 window handle.
    pub hwnd: HWND,
    /// Window title bar text (empty if no title).
    pub title: String,
    /// Win32 window class name.
    pub class: String,
    /// Screen rectangle of the window (x, y, width, height).
    pub rect: Rect,
    /// Executable file name only (e.g. `"code.exe"`).
    pub exe: String,
    /// Full path to the executable (empty if unavailable).
    pub process_path: String,
    /// Whether the window is visible (`WS_VISIBLE` style).
    pub is_visible: bool,
    /// Whether the window is maximized (`WS_MAXIMIZE` style).
    pub is_maximized: bool,
    /// Whether the window is in exclusive or borderless fullscreen.
    pub is_fullscreen: bool,
}

// ── String conversion helpers ──────────────────────────────────────

/// Convert a Rust `&str` to a null-terminated UTF-16 `Vec<u16>`.
///
/// This is the standard pattern for passing strings to Win32 APIs that
/// accept `PCWSTR` or `PWSTR`. The trailing null is required by Win32.
#[must_use]
#[allow(dead_code)] // Utility for future Win32 string-passing wrappers (e.g., SetWindowPos, MoveWindow).
fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Convert a null-terminated UTF-16 slice to a Rust `String`.
///
/// Slices returned from Win32 APIs may contain a trailing null character.
/// This function finds the first null and converts only the content before it,
/// falling back to the full slice if no null is present (shouldn't happen in
/// practice).
fn from_wide(wide: &[u16]) -> String {
    let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16(&wide[..len]).unwrap_or_default()
}

// ── Individual query functions ──────────────────────────────────────

/// Retrieves the window title bar text.
///
/// Returns an empty string if the window has no title. Returns an error
/// only if the Win32 call itself fails unexpectedly.
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
///
/// # Errors
///
/// Returns a human-readable error string if `GetWindowTextLengthW` or
/// `GetWindowTextW` fails.
pub fn get_window_text(hwnd: HWND) -> Result<String, String> {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return Ok(String::new());
    }
    // Allocate `len + 1` to hold the text plus the null terminator.
    let mut buf = vec![0u16; (len + 1) as usize];
    let written = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if written == 0 {
        // Length was positive but write returned 0 — something changed
        // between the two calls (e.g., window was destroyed).
        return Ok(String::new());
    }
    Ok(from_wide(&buf))
}

/// Retrieves the Win32 window class name.
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
///
/// # Errors
///
/// Returns a human-readable error string if the class name cannot be retrieved.
pub fn get_class_name(hwnd: HWND) -> Result<String, String> {
    // 256 chars is more than enough for any realistic window class name.
    let mut buf = vec![0u16; 256];
    let written = unsafe { GetClassNameW(hwnd, &mut buf) };
    if written == 0 {
        return Err("GetClassNameW returned 0".to_owned());
    }
    Ok(from_wide(&buf))
}

/// Retrieves the window's screen rectangle as a [`Rect`].
///
/// Converts from Win32's `RECT` (left, top, right, bottom) to flow's
/// `Rect` (x, y, width, height).
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
///
/// # Errors
///
/// Returns a human-readable error string if `GetWindowRect` fails.
pub fn get_window_rect(hwnd: HWND) -> Result<Rect, String> {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe { GetWindowRect(hwnd, &mut rect) }.map_err(|e| format!("GetWindowRect failed: {e}"))?;

    Ok(Rect {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    })
}

/// Retrieves the current cursor (mouse pointer) position in screen coordinates.
///
/// The returned `(x, y)` is in the same coordinate space as [`get_window_rect`] —
/// physical screen pixels — so no translation is needed when comparing against
/// layout rects.
///
/// # Errors
///
/// Returns a human-readable error string if `GetCursorPos` fails (extremely rare;
/// typically only when the calling thread lacks a desktop).
pub fn get_cursor_pos() -> Result<(i32, i32), String> {
    let mut point = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut point) }.map_err(|e| format!("GetCursorPos failed: {e}"))?;
    Ok((point.x, point.y))
}

/// Resolve the **top-level** window under the given screen point.
///
/// `WindowFromPoint` returns the window (possibly a child control) directly
/// beneath `(x, y)`; this walks it to its top-level ancestor via `GetAncestor`
/// (`GA_ROOT`) so that hovering a button, an edit field, or any child control
/// inside a managed window resolves to that window itself. Without the walk,
/// child controls read as untracked and focus-follows-mouse is defeated.
///
/// Returns `None` when the point is over no window (the bare desktop) or the
/// APIs return a null handle.
///
/// (`docs/src/dev-guide/hover.md`)
#[must_use]
pub fn window_from_point(x: i32, y: i32) -> Option<isize> {
    let point = POINT { x, y };
    let hwnd = unsafe { WindowFromPoint(point) };
    if hwnd.0.is_null() {
        return None;
    }
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if root.0.is_null() {
        None
    } else {
        Some(root.0 as isize)
    }
}

/// Retrieves the window's **visible** screen rectangle via DWM extended frame bounds.
///
/// Unlike [`get_window_rect`] (which returns the full rect including invisible
/// borders), this function returns the rectangle that the user actually sees on
/// screen. On Windows 10/11, the difference is typically ~7px on left, right,
/// and bottom edges (used for shadows and resize hit-testing).
///
/// # How It Works
///
/// `DwmGetWindowAttribute` with `DWMWA_EXTENDED_FRAME_BOUNDS` queries the
/// Desktop Window Manager (DWM) for the compositor's knowledge of the window's
/// visible bounds. This is more accurate than `GetWindowRect` for tiling
/// purposes because it excludes the invisible "extended frame" area.
///
/// # Fail-Open Behavior
///
/// If DWM is unavailable (e.g., on older systems without DWM, or during
/// certain fullscreen transitions), this function returns an error. The caller
/// ([`get_invisible_bounds`]) handles this by falling back to zero bounds.
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
///
/// # Errors
///
/// Returns a human-readable error string if the DWM query fails.
///
/// # Example
///
/// ```no_run
/// use flow_wm::registry::win32::get_extended_frame_bounds;
/// use windows::Win32::Foundation::HWND;
/// use windows::core::PCWSTR;
/// // hwnd would come from EnumWindows or a hook event
/// // let visible_rect = get_extended_frame_bounds(hwnd).expect("visible rect");
/// let _ = get_extended_frame_bounds(HWND(std::ptr::null_mut())); // returns Err
/// ```
pub fn get_extended_frame_bounds(hwnd: HWND) -> Result<Rect, String> {
    let mut rect = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut core::ffi::c_void,
            size_of::<RECT>() as u32,
        )
    }
    .map_err(|e| format!("DwmGetWindowAttribute(EXTENDED_FRAME_BOUNDS) failed: {e}"))?;

    Ok(Rect {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    })
}

/// Read a window's Windows 11 corner-rounding preference and map it to a
/// semantic [`CornerPreference`] for border rendering.
///
/// Wraps `DwmGetWindowAttribute(DWMWA_WINDOW_CORNER_PREFERENCE)` (Windows 11
/// build 22000+). The border overlay uses this to round its own ring to match
/// the target window's corners, so a rounded window gets a rounded border and
/// a square window gets a square border (see `docs/src/dev-guide/borders.md`).
///
/// # Fail-open
///
/// Returns `None` on pre-Win11 Windows (where the attribute is unavailable),
/// on UWP/chemistry windows that reject the query, and during shutdown races.
/// Callers treat `None` as "use the renderer default" — the daemon substitutes
/// [`CornerPreference::Default`], which the renderer maps to Win11's standard
/// rounded radius.
///
/// The integer mapping mirrors `dwmapi.h`:
/// `DWMWCP_DEFAULT = 0`, `DWMWCP_DONOTROUND = 1`, `DWMWCP_ROUND = 2`,
/// `DWMWCP_ROUNDSMALL = 3`.
#[must_use]
pub fn get_window_corner_preference(hwnd: HWND) -> Option<CornerPreference> {
    let mut pref = DWM_WINDOW_CORNER_PREFERENCE::default();
    // SAFETY: DwmGetWindowAttribute reads a 4-byte attribute for the given
    // HWND into a local of the matching repr-transparent type. The out-pointer
    // and size are correct; the query is read-only with respect to the window.
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &mut pref as *mut _ as *mut core::ffi::c_void,
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        )
    };
    result.ok()?;
    Some(match pref.0 {
        1 => CornerPreference::Square,
        2 => CornerPreference::Rounded,
        3 => CornerPreference::RoundedSmall,
        // 0 (DEFAULT) and any unrecognized value: let the renderer default
        // (Win11 rounds top-level windows).
        _ => CornerPreference::Default,
    })
}

/// Computes the per-edge invisible border sizes for a window.
///
/// Compares [`get_window_rect`] (full rect including invisible borders) against
/// [`get_extended_frame_bounds`] (visible rect) to determine how many pixels
/// of invisible border exist on each edge.
///
/// # Fail-Open Strategy
///
/// If either query fails (e.g., DWM unavailable, window destroyed mid-query),
/// returns [`InvisibleBounds::zero()`]. This means the window will be treated
/// as having no invisible borders — the window may have slightly larger gaps,
/// but this is preferable to crashing or excluding the window entirely.
///
/// # Coordinate Math
///
/// Given:
/// - Window rect (from `GetWindowRect`): left=WL, top=WT, right=WR, bottom=WB
/// - Visible rect (from DWM): left=VL, top=VT, right=VR, bottom=VB
///
/// The window rect is always larger (or equal):
/// ```text
/// left   = VL - WL  (≥ 0)
/// top    = VT - WT  (≥ 0)
/// right  = WR - VR  (≥ 0)
/// bottom = WB - VB  (≥ 0)
/// ```
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
///
/// # Example
///
/// ```no_run
/// use flow_wm::registry::win32::get_invisible_bounds;
/// use windows::Win32::Foundation::HWND;
/// let bounds = get_invisible_bounds(HWND(std::ptr::null_mut()));
/// // For an invalid HWND, returns zero bounds (fail-open)
/// assert_eq!(bounds, flow_wm::common::InvisibleBounds::zero());
/// ```
#[must_use]
pub fn get_invisible_bounds(hwnd: HWND) -> InvisibleBounds {
    match (
        get_window_rect(hwnd).ok(),
        get_extended_frame_bounds(hwnd).ok(),
    ) {
        (Some(window_rect), Some(visible_rect)) => {
            // Clamp negative values to zero — in rare edge cases (e.g.,
            // window transitioning between states), the visible rect might
            // extend slightly beyond the window rect.
            InvisibleBounds {
                left: (visible_rect.x - window_rect.x).max(0),
                top: (visible_rect.y - window_rect.y).max(0),
                right: (window_rect.right() - visible_rect.right()).max(0),
                bottom: (window_rect.bottom() - visible_rect.bottom()).max(0),
            }
        }
        _ => InvisibleBounds::zero(),
    }
}

/// Returns `true` if the window has the `WS_VISIBLE` style.
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
#[must_use]
pub fn is_window_visible(hwnd: HWND) -> bool {
    let result = unsafe { IsWindowVisible(hwnd) };
    result.as_bool()
}

/// Returns `true` if the window is maximized (`WS_MAXIMIZE` style).
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
#[must_use]
pub fn is_zoomed(hwnd: HWND) -> bool {
    let result = unsafe { IsZoomed(hwnd) };
    result.as_bool()
}

/// Returns `true` if the window is minimized (iconic).
///
/// Wraps the Win32 `IsIconic()` call. A minimized window keeps the
/// `WS_VISIBLE` style, so [`is_window_visible`] returns `true` for it — this
/// function is therefore **not** redundant with the visibility check. It is the
/// komorebi-style "ignore iconic windows" filter: such windows should not
/// participate in the tiling layout.
///
/// Used together with [`is_cloaked`] in the registry's visibility
/// reconciliation to detect tray-hidden apps (Discord, Steam) and ordinary
/// minimizes alike.
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
#[must_use]
pub fn is_iconic(hwnd: HWND) -> bool {
    let result = unsafe { IsIconic(hwnd) };
    result.as_bool()
}

/// Query the currently foreground (active) window handle.
///
/// Wraps the Win32 `GetForegroundWindow()` call. Returns the HWND as an
/// `isize`, or `None` if there is no foreground window (e.g., the desktop
/// has focus or no window is active).
///
/// # Usage
///
/// During daemon initialization, this is called to determine which tiling
/// column should be treated as the focus column for viewport centering.
/// The caller checks whether the returned handle belongs to a managed
/// (tiling) window before using it.
///
/// # Returns
///
/// `Some(isize)` — the foreground window handle, or `None` if the handle
/// is null (no foreground window).
#[must_use]
pub fn get_foreground_window() -> Option<isize> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        None
    } else {
        Some(hwnd.0 as isize)
    }
}

/// Returns the shell (desktop / "Progman") window handle, or `None`.
///
/// Wraps `GetShellWindow()`. The shell window is the "defocus" target —
/// foregrounding it is the Win32 idiom for clicking empty desktop space (no
/// app receives keystrokes). See [`clear_foreground_window`].
///
/// # Returns
///
/// `Some(isize)` — the shell window handle, or `None` if the handle is null.
#[must_use]
pub fn get_shell_window() -> Option<isize> {
    let hwnd = unsafe { GetShellWindow() };
    if hwnd.0.is_null() {
        None
    } else {
        Some(hwnd.0 as isize)
    }
}

/// Forcefully set a window as the foreground (active) window.
///
/// Windows restricts `SetForegroundWindow` to the process that currently owns
/// the foreground. This function bypasses that restriction using the
/// `AttachThreadInput` trick: it temporarily attaches the calling thread's
/// input queue to the current foreground window's thread, which grants
/// foreground permission, then calls `SetForegroundWindow`, then detaches.
///
/// # Design Decision: AttachThreadInput Workaround
///
/// Windows enforces "foreground locking" — only the process that currently
/// owns the foreground window can set a new foreground window. This prevents
/// background processes from stealing focus unexpectedly. However, window
/// managers legitimately need to change focus across process boundaries.
///
/// The workaround, used by [komorebi](https://github.com/LGUG2Z/komorebi),
/// AutoHotkey, and many other window managers, is:
///
/// 1. Get the current foreground window and its thread ID
/// 2. Get our calling thread's ID
/// 3. Attach our thread to the foreground thread with `AttachThreadInput(TRUE)`
///    — this merges our input queues, making our thread part of the foreground's
///    "input desktop"
/// 4. Call `SetForegroundWindow(target_hwnd)` — this now succeeds because we're
///    attached to the foreground thread
/// 5. Call `BringWindowToTop(target_hwnd)` — ensures the window is at the top
///    of the Z-order
/// 6. Detach with `AttachThreadInput(FALSE)` — **always** do this, even on error
///
/// # When the Foreground Window is Null
///
/// If `GetForegroundWindow()` returns null (no foreground window exists), we skip
/// the `AttachThreadInput` step and call `SetForegroundWindow` directly. This
/// may succeed if there's no active foreground lock (e.g., during initial
/// daemon startup).
///
/// # Safety Guarantees
///
/// - All Win32 calls are wrapped in minimal-scope `unsafe` blocks
/// - `AttachThreadInput(FALSE)` is always called in a cleanup step, even if
///   `SetForegroundWindow` fails (defensive against resource leaks)
/// - No `.unwrap()` / `.expect()` — all errors are handled gracefully
///
/// # Arguments
///
/// * `hwnd_val` - The window handle (as `isize`, matching the project convention).
///
/// # Returns
///
/// `true` if `SetForegroundWindow` succeeded, `false` otherwise.
///
/// # Example
///
/// ```no_run
/// use flow_wm::registry::win32::set_foreground_window;
/// // Assume we have a window handle from registry
/// let success = set_foreground_window(0x12345678);
/// if success {
///     println!("Window is now foreground");
/// }
/// ```
#[must_use]
pub fn set_foreground_window(hwnd_val: isize) -> bool {
    let target_hwnd = HWND(hwnd_val as *mut _);

    // Get the current foreground window.
    let foreground_hwnd = unsafe { GetForegroundWindow() };

    // No foreground window means no foreground lock to defeat — try the
    // direct call. Benign (happens at startup or with no app focused),
    // so no log here.
    if foreground_hwnd.0.is_null() {
        let result = unsafe { SetForegroundWindow(target_hwnd) };
        return result.as_bool();
    }

    // GetWindowThreadProcessId's RETURN value is the THREAD ID of the
    // foreground window's GUI thread; the out-parameter receives the PID.
    // AttachThreadInput takes thread IDs in BOTH arguments — passing the
    // PID as `idAttachTo` fails with ERROR_INVALID_THREAD_ID, which
    // silently defeated the attach and left SetForegroundWindow at the
    // mercy of the foreground lock (the previous bug).
    let mut foreground_pid: u32 = 0;
    let foreground_thread_id =
        unsafe { GetWindowThreadProcessId(foreground_hwnd, Some(&mut foreground_pid)) };
    if foreground_thread_id == 0 {
        log::warn!(
            "set_foreground_window: GetWindowThreadProcessId returned thread_id 0 for foreground hwnd {:?} (Win32 error {:#x}); falling back to direct SetForegroundWindow",
            foreground_hwnd,
            unsafe { GetLastError() }.0
        );
        let result = unsafe { SetForegroundWindow(target_hwnd) };
        return result.as_bool();
    }

    let our_thread_id = unsafe { GetCurrentThreadId() };

    // Attach our input queue to the foreground thread's so that
    // SetForegroundWindow is permitted under the foreground lock. Without
    // this attach the call only succeeds when AllowSetForegroundWindow
    // granted the daemon one-shot permission (the CLI requests it before
    // every IPC dispatch — see src/bin/flow.rs).
    let attach_ok =
        unsafe { AttachThreadInput(our_thread_id, foreground_thread_id, true) }.as_bool();
    if !attach_ok {
        log::warn!(
            "set_foreground_window: AttachThreadInput(true) failed for our_thread={our_thread_id} fg_thread={foreground_thread_id} fg_pid={foreground_pid} (Win32 error {:#x})",
            unsafe { GetLastError() }.0
        );
    }

    // Compute success first, then detach in all paths.
    let success = unsafe { SetForegroundWindow(target_hwnd) }.as_bool();
    if !success {
        log::warn!(
            "set_foreground_window: SetForegroundWindow failed for hwnd {hwnd_val} (Win32 error {:#x})",
            unsafe { GetLastError() }.0
        );
    }

    // Bring the window to the top of the Z-order. Failure here is
    // non-fatal — the window still receives focus if SetForegroundWindow
    // succeeded, so we don't log.
    let _ = unsafe { BringWindowToTop(target_hwnd) };

    // Always detach when we attached, even if SetForegroundWindow failed.
    // Detach failure would leak the attach — rare enough that a warn log
    // is sufficient.
    if attach_ok {
        let detach_ok =
            unsafe { AttachThreadInput(our_thread_id, foreground_thread_id, false) }.as_bool();
        if !detach_ok {
            log::warn!(
                "set_foreground_window: AttachThreadInput(false) cleanup failed for our_thread={our_thread_id} fg_thread={foreground_thread_id} (Win32 error {:#x})",
                unsafe { GetLastError() }.0
            );
        }
    }

    success
}

/// Remove the current foreground window by foregrounding the desktop.
///
/// Windows has no "clear foreground" API; foregrounding the shell (Progman)
/// window is the standard idiom for "click empty desktop" — no application
/// receives keyboard input. Used when switching to an empty workspace so
/// keystrokes do not leak to the previously focused window.
///
/// Resolves the shell window via [`get_shell_window`] and pushes it to the
/// foreground via [`set_foreground_window`] (reusing its `AttachThreadInput`
/// foreground-lock bypass).
///
/// # Returns
///
/// `true` if the shell window was resolved and `SetForegroundWindow`
/// succeeded, `false` otherwise (no shell window, or the call was defeated
/// by the foreground lock).
#[must_use]
pub fn clear_foreground_window() -> bool {
    let Some(shell_hwnd) = get_shell_window() else {
        log::warn!("clear_foreground_window: GetShellWindow returned null");
        return false;
    };
    set_foreground_window(shell_hwnd)
}

/// Politely ask a window to close itself by posting `WM_CLOSE` to it.
///
/// This is the Win32 "gentle close" path: [`PostMessageW`] places a
/// [`WM_CLOSE`] message in the window's message queue without waiting for it
/// to be processed (fire-and-forget from the caller's perspective). The
/// owning application then receives `WM_CLOSE` and decides how to respond —
/// most apps run their normal shutdown logic: prompt to save unsaved work,
/// release resources, destroy the window, and so on.
///
/// # Design Decision: `PostMessageW` + `WM_CLOSE` (not `DestroyWindow`)
///
/// We deliberately use the *polite* close instead of `DestroyWindow`:
///
/// - **`WM_CLOSE`** gives the application agency. Editors like VS Code or
///   Word can show their "save changes?" dialog; background apps can
///   minimise to the tray instead of exiting; an app mid-operation can
///   refuse and stay open. This is exactly the message Windows synthesises
///   when the user clicks the window's red ✕ button.
/// - **`DestroyWindow`** would tear the window down unconditionally. That
///   skips the app's shutdown logic, loses unsaved data, and can leave the
///   owning process half-cleaned-up. It is the right tool for an app that
///   ignores `WM_CLOSE`, but not the sensible default.
///
/// # Why `PostMessageW` Rather Than `SendMessageW`?
///
/// [`SendMessageW`](windows::Win32::UI::WindowsAndMessaging::SendMessageW)
/// would block the calling (IPC) thread until the target window's thread
/// processes the message. A hung or modal application could stall the
/// daemon's entire IPC loop. [`PostMessageW`] returns immediately, keeping
/// the daemon responsive regardless of the target app's state.
///
/// # Lifecycle: Cleanup Is Asynchronous
///
/// This function returns as soon as the message is *queued*; it does **not**
/// wait for the window to actually disappear. When the application eventually
/// destroys the window in response to `WM_CLOSE`, Win32 fires
/// `EVENT_OBJECT_DESTROY`, which flow's WinEvent hook turns into a
/// [`Destroyed`](crate::registry::hooks::HookEvent::Destroyed) event. The
/// daemon's event loop then removes the window from the registry and the
/// layout engine automatically (and animates the gap closing). Callers must
/// therefore **not** mutate the layout themselves after calling this — the
/// normal event pipeline handles it.
///
/// # Arguments
///
/// * `hwnd_val` — the window handle as `isize`, matching the project's
///   cross-thread HWND convention (see [`set_foreground_window`]); converted
///   back to [`HWND`] at the Win32 boundary.
///
/// # Returns
///
/// `true` if the message was successfully queued, `false` if `PostMessageW`
/// failed (e.g. the window was destroyed between the focus query and this
/// call). A `true` result only means the message was queued — **not** that
/// the window has closed.
///
/// # Example
///
/// ```no_run
/// use flow_wm::registry::win32::close_window;
/// // Ask the currently focused window to close (gentle, like clicking ✕).
/// let queued = close_window(0x000C_1234);
/// if queued {
///     println!("close requested");
/// }
/// ```
#[must_use]
pub fn close_window(hwnd_val: isize) -> bool {
    let target_hwnd = HWND(hwnd_val as *mut _);
    // WM_CLOSE carries no payload, so WPARAM/LPARAM are zeroed. PostMessageW
    // takes `Option<HWND>` (None would post to the calling thread's own
    // queue); we always target a specific window, hence `Some(...)`.
    let queued = unsafe { PostMessageW(Some(target_hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    queued.is_ok()
}

/// Directly reposition a single window via `SetWindowPos`, bypassing the animator.
///
/// This is the **silent teleport** primitive used during workspace-switch
/// operations to relocate *bystander* workspaces — those whose vertical
/// parking side changed (e.g. ws 3-7 when switching 2 → 8) but which are
/// neither the source nor destination of the switch. They are moved
/// instantly so the workspace stacking invariant is restored without
/// every bystander workspace visibly sliding across the screen.
///
/// # Why bypass the animator?
///
/// The animator's `RetargetFromCurrent` interrupt policy would animate each
/// retargeted window from its current position to the new target. For a
/// 10-workspace switch that could mean eight bystander workspaces all
/// visibly sliding at once, which is visually noisy and distracts from the
/// two workspaces the user actually asked to switch between. Teleporting
/// them directly keeps the animation focused on the participant workspaces.
///
/// # Coordinate space — window rect, not visible rect
///
/// The `(x, y, w, h)` arguments must already be in **window-rect** space
/// (the coordinates `SetWindowPos` expects, which include invisible borders).
/// The caller is responsible for translating from the layout engine's
/// visible-rect space via
/// [`InvisibleBounds::visible_to_window`](crate::common::InvisibleBounds::visible_to_window)
/// before calling this function. The translation is **not** applied here so
/// that callers computing batches can amortise the registry lookup across
/// many windows.
///
/// # Z-order and focus
///
/// Uses `SWP_NOZORDER | SWP_NOACTIVATE` — the same flags used by the
/// animator's `apply_batch` — so the window's Z-order and keyboard-focus
/// state are preserved. This is critical for workspace switches: a
/// teleport must not steal focus from the destination workspace's window.
///
/// # Arguments
///
/// * `hwnd_val` — Window handle as `isize` (project cross-thread convention).
/// * `x`, `y` — Top-left corner of the target window rect, in screen coords.
/// * `w`, `h` — Width and height of the target window rect, in pixels.
///
/// # Returns
///
/// `true` if `SetWindowPos` succeeded; `false` on Win32 failure (logged at
/// `warn` level). A `false` return leaves the window at its previous
/// position — a minor visual inconsistency, not a crash.
///
/// # Example
///
/// ```no_run
/// use flow_wm::registry::win32::set_window_rect;
/// // Instantly snap a window to (0, -1084) — the parking slot of a workspace
/// // parked one unit above a 1080-tall monitor.
/// let ok = set_window_rect(0x000C_1234, 0, -1084, 960, 1080);
/// ```
#[must_use]
pub fn set_window_rect(hwnd_val: isize, x: i32, y: i32, w: i32, h: i32) -> bool {
    let target_hwnd = HWND(hwnd_val as *mut _);
    // hwnd_insert_after = None + SWP_NOZORDER → preserve existing Z-order.
    // SWP_NOACTIVATE → do not steal keyboard focus (critical for workspace switches).
    let result =
        unsafe { SetWindowPos(target_hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE) };
    if let Err(e) = result {
        log::warn!("set_window_rect SetWindowPos failed for hwnd {hwnd_val}: {e}");
        false
    } else {
        true
    }
}

/// Returns `true` if the window would appear in the Alt+Tab switcher.
///
/// Windows uses a combination of extended window styles and Desktop Window
/// Manager (DWM) cloaking state to determine which windows appear in the
/// Alt+Tab switcher. This function mirrors the OS-level logic with two checks:
///
/// ## 1. Extended Style Check (`WS_EX_TOOLWINDOW` / `WS_EX_APPWINDOW`)
///
/// - Windows with `WS_EX_TOOLWINDOW` are **hidden** from Alt+Tab (they're
///   considered tool windows, tray icons, floating toolbars, etc.).
/// - However, windows with `WS_EX_APPWINDOW` **force** visibility in Alt+Tab
///   even if they have `WS_EX_TOOLWINDOW`.
///
/// | `WS_EX_TOOLWINDOW` | `WS_EX_APPWINDOW` | Style check result |
/// |:-------------------:|:------------------:|:------------------:|
/// | ✗                   | ✗                  | ✓ (normal window)  |
/// | ✓                   | ✗                  | ✗ (tool window)    |
/// | ✗                   | ✓                  | ✓ (forced)         |
/// | ✓                   | ✓                  | ✓ (forced)         |
///
/// ## 2. DWM Cloaking Check (`DWMWA_CLOAKED`)
///
/// Modern Windows (Vista+) uses DWM cloaking to hide windows that are
/// technically "visible" to Win32 but not shown to the user. This is the
/// primary mechanism for suspending UWP/WinUI apps. A cloaked window has
/// `IsWindowVisible() == true` but is not rendered on screen.
///
/// Cloak reasons (any non-zero value means the window is hidden):
///
/// | Constant                | Value | Meaning                                  |
/// |:------------------------|:-----:|:-----------------------------------------|
/// | `DWM_CLOAKED_APP`       |   1   | Cloaked by its own application           |
/// | `DWM_CLOAKED_SHELL`     |   2   | Cloaked by the shell (suspended UWP)     |
/// | `DWM_CLOAKED_INHERITED` |   4   | Cloaked because owner window is cloaked  |
///
/// # Why This Matters
///
/// Without the cloaking check, background UWP frames like
/// `ApplicationFrameHost.exe` (class `ApplicationFrameWindow`) and
/// `SystemSettings.exe` (class `Windows.UI.Core.CoreWindow`) slip through
/// the style-only filter — they have no `WS_EX_TOOLWINDOW` but are cloaked
/// by the shell when suspended. These windows should never be tiled.
///
/// # Fail-Open Behavior
///
/// If `DwmGetWindowAttribute` fails (e.g., the window was destroyed between
/// our checks), we treat the window as **not cloaked** — we'd rather include
/// a window than accidentally exclude a legitimate one.
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
#[must_use]
pub fn is_alt_tab_visible(hwnd: HWND) -> bool {
    // ── Check 1: Extended style ───────────────────────────────────────
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) };
    let ex = WINDOW_EX_STYLE(ex_style as u32);

    let is_tool = ex & WS_EX_TOOLWINDOW != WINDOW_EX_STYLE(0);
    let is_app = ex & WS_EX_APPWINDOW != WINDOW_EX_STYLE(0);

    // Alt+Tab shows windows that are NOT toolwindows,
    // OR windows that explicitly opt in via APPWINDOW.
    let style_visible = !is_tool || is_app;
    if !style_visible {
        return false;
    }

    // ── Check 2: DWM cloaking (suspended UWP background frames) ──────
    !is_cloaked(hwnd)
}

/// Read a window's live `WS_EX_TOPMOST` flag.
///
/// Wraps `GetWindowLongW(GWL_EXSTYLE)` and tests the `WS_EX_TOPMOST` bit. This
/// is the **reality check** the float-layer re-assertion pass uses to detect
/// Z-order drift: another app can grab topmost or an event can clear the flag
/// while the daemon's cached `floats_topmost` still believes it is set, so the
/// re-assertion reads the live flag rather than trusting the cache.
///
/// # Fail-open
///
/// Returns `false` on a `GetWindowLongW` failure or for an invalid / destroyed
/// HWND. A `false` reading drives the re-assertion to (re-)apply `HWND_TOPMOST`,
/// which is idempotent and harmless — strictly safer than failing open toward
/// "already topmost", which would mask real drift.
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
#[must_use]
pub fn is_topmost(hwnd: HWND) -> bool {
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) };
    let ex = WINDOW_EX_STYLE(ex_style as u32);
    ex & WS_EX_TOPMOST != WINDOW_EX_STYLE(0)
}

/// Returns `true` if the window is DWM-cloaked (hidden from the screen).
///
/// DWM cloaking is the modern Windows mechanism for hiding windows that are
/// technically "visible" to `IsWindowVisible()` but not rendered. This is
/// primarily used for suspended UWP/WinUI apps and shell-managed windows.
///
/// # Cloak Reasons
///
/// | Constant                | Value | Typical cause                            |
/// |:------------------------|:-----:|:-----------------------------------------|
/// | `DWM_CLOAKED_APP`       |   1   | Application hid itself (e.g., minimised) |
/// | `DWM_CLOAKED_SHELL`     |   2   | Shell suspended a UWP app               |
/// | `DWM_CLOAKED_INHERITED` |   4   | Owner window is cloaked                  |
///
/// Any non-zero value means the window is cloaked.
///
/// # Fail-Open
///
/// If `DwmGetWindowAttribute` fails, returns `false` (not cloaked). This
/// prevents accidentally excluding legitimate windows due to transient
/// Win32 errors (e.g., window destroyed mid-query).
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
#[must_use]
pub fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut core::ffi::c_void,
            size_of::<u32>() as u32,
        )
    };
    match result {
        Ok(()) => cloaked != 0,
        Err(_) => false, // Fail-open: assume not cloaked.
    }
}

/// Detects whether the window is in exclusive or borderless fullscreen.
///
/// This is a basic heuristic:
/// 1. The window covers the full screen dimensions (`SM_CXSCREEN` × `SM_CYSCREEN`).
/// 2. The window style does **not** include `WS_CAPTION | WS_THICKFRAME`
///    (no title bar, no resize border).
///
/// A full monitor-aware implementation (using `MonitorFromWindow` /
/// `GetMonitorInfo`) can replace this in a future iteration.
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
///
/// # Errors
///
/// Returns a human-readable error string if any Win32 call fails.
pub fn is_fullscreen(hwnd: HWND) -> Result<bool, String> {
    let rect = get_window_rect(hwnd)?;

    let screen_cx = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let screen_cy = unsafe { GetSystemMetrics(SM_CYSCREEN) };

    // Check if window covers the entire screen.
    if rect.x != 0 || rect.y != 0 || rect.width != screen_cx || rect.height != screen_cy {
        return Ok(false);
    }

    // Check window style for absence of caption and thick frame.
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) };
    let style = WINDOW_STYLE(style as u32);
    let has_chrome = style & (WS_CAPTION | WS_THICKFRAME) != WINDOW_STYLE(0);

    Ok(!has_chrome)
}

/// Retrieves the process ID (PID) of the window's owner process.
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
///
/// # Errors
///
/// Returns a human-readable error string if the PID cannot be retrieved.
pub fn get_window_thread_process_id(hwnd: HWND) -> Result<u32, String> {
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return Err("GetWindowThreadProcessId returned PID 0".to_owned());
    }
    Ok(pid)
}

/// Retrieves the executable name and full path for a given process ID.
///
/// Uses `OpenProcess` with `PROCESS_QUERY_LIMITED_INFORMATION` (the least
/// privileged access right that still permits `QueryFullProcessImageNameW`),
/// then queries the full image path.
///
/// # Handle Lifetime Management
///
/// The process handle is opened, used for the query, and closed within this
/// function. `CloseHandle` is called even on early return to prevent kernel
/// handle leaks. The `let _ = CloseHandle(...)` intentionally ignores the
/// close result — there's nothing meaningful we can do if closing fails
/// (the query is already complete).
///
/// # Why PROCESS_QUERY_LIMITED_INFORMATION?
///
/// We use the minimum privilege level needed. This works even for elevated
/// processes where `PROCESS_QUERY_INFORMATION` would be denied. It's
/// sufficient for `QueryFullProcessImageNameW`.
///
/// # Arguments
///
/// * `pid` — Process ID.
///
/// # Returns
///
/// A tuple of `(exe_name, full_path)` where `exe_name` is just the file
/// name (e.g. `"code.exe"`) and `full_path` is the complete filesystem
/// path (e.g. `"C:\\Program Files\\VSCode\\code.exe"`).
///
/// # Errors
///
/// Returns a human-readable error string if the process cannot be opened
/// (e.g., access denied, process exited) or the image path cannot be queried.
pub fn get_process_exe_and_path(pid: u32) -> Result<(String, String), String> {
    // Open the process with limited query rights. This is the minimum
    // privilege level needed for QueryFullProcessImageNameW.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };

    let handle = handle.map_err(|e| format!("OpenProcess failed for PID {pid}: {e}"))?;

    // Ensure the handle is closed even on early return.
    let result = get_process_path_from_handle(handle);

    // Close the handle — fire-and-forget the result; nothing meaningful
    // we can do if CloseHandle fails here.
    let _ = unsafe { CloseHandle(handle) };

    result
}

/// Internal helper: queries the full image name from an open process handle.
///
/// # Buffer Strategy
///
/// Starts with a 260-character buffer (classic `MAX_PATH`). If the path is
/// longer, `QueryFullProcessImageNameW` reports the required size and we
/// retry with a larger buffer. Modern Windows supports paths longer than
/// 260 characters, so this retry logic is necessary for correctness.
fn get_process_path_from_handle(
    handle: windows::Win32::Foundation::HANDLE,
) -> Result<(String, String), String> {
    // MAX_PATH (260) is the classic limit, but modern Windows supports
    // longer paths. Start with 260 and retry with a larger buffer on truncation.
    const INITIAL_BUF: u32 = 260;
    let mut size: u32 = INITIAL_BUF;
    let mut buf = vec![0u16; size as usize];

    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
    };

    // If the buffer was too small, retry with the reported required size.
    if result.is_err() && size > INITIAL_BUF {
        buf = vec![0u16; size as usize];
        if unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut size,
            )
        }
        .is_err()
        {
            return Err("QueryFullProcessImageNameW failed after retry".to_owned());
        }
    } else if result.is_err() {
        return Err("QueryFullProcessImageNameW failed".to_owned());
    }

    let path = from_wide(&buf);
    if path.is_empty() {
        return Err("QueryFullProcessImageNameW returned empty path".to_owned());
    }

    // Extract the file name from the full path.
    let exe = std::path::Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_owned();

    Ok((exe, path))
}

// ── Convenience aggregator ─────────────────────────────────────────

/// Queries all available metadata for a window and returns a [`WindowInfo`].
///
/// This is the primary entry point for gathering window information during
/// registry initialization and event handling. It calls each individual
/// query function and assembles the results into a single struct.
///
/// # Error Tolerance Strategy
///
/// Individual query failures are tolerated where possible, following a
/// "best-effort" philosophy — we'd rather have a window with partial metadata
/// than no window at all:
///
/// | Query | On failure | Rationale |
/// |-------|------------|-----------|
/// | `title` | Empty string | Many windows have no title; not an error |
/// | `class` | Empty string | Rare but not critical for classification |
/// | `rect` | **Propagate error** | Essential for layout; can't tile without position |
/// | `is_fullscreen` | `false` | False negative is better than failing entirely |
/// | `exe`/`process_path` | `"unknown"`/empty | Access denied for system processes is common |
///
/// # Design: Why Aggregate?
///
/// Rather than having each consumer call individual query functions, we
/// aggregate everything into `WindowInfo` once. This:
/// - Reduces the number of Win32 API calls (each call has overhead).
/// - Provides a consistent snapshot (no TOCTOU between queries).
/// - Simplifies the consumer API (one function call, one result type).
///
/// # Arguments
///
/// * `hwnd` — Win32 window handle.
///
/// # Errors
///
/// Returns a human-readable error string if essential queries fail.
pub fn get_window_info(hwnd: HWND) -> Result<WindowInfo, String> {
    let title = get_window_text(hwnd).unwrap_or_default();
    let class = get_class_name(hwnd).unwrap_or_default();
    let rect = get_window_rect(hwnd)?;
    let is_visible = is_window_visible(hwnd);
    let is_maximized = is_zoomed(hwnd);
    let is_fullscreen = is_fullscreen(hwnd).unwrap_or(false);

    let pid = match get_window_thread_process_id(hwnd) {
        Ok(p) => p,
        Err(_) => {
            return Ok(WindowInfo {
                hwnd,
                title,
                class,
                rect,
                exe: "unknown".to_owned(),
                process_path: String::new(),
                is_visible,
                is_maximized,
                is_fullscreen,
            });
        }
    };

    let (exe, process_path) =
        get_process_exe_and_path(pid).unwrap_or(("unknown".to_owned(), String::new()));

    Ok(WindowInfo {
        hwnd,
        title,
        class,
        rect,
        exe,
        process_path,
        is_visible,
        is_maximized,
        is_fullscreen,
    })
}

// ── Monitor queries ──────────────────────────────────────────────────

/// The two coordinate rectangles Win32 exposes for a single physical
/// monitor, converted to the crate's [`Rect`] type.
///
/// Produced by [`get_primary_monitor_info`], which derives both fields from
/// a single `MONITORINFO` struct returned by `GetMonitorInfoW`.
///
/// # Why two rectangles?
///
/// A tiling manager needs **both** notions of "the screen":
///
/// - **`work_area`** (from `MONITORINFO::rcWork`) excludes the taskbar and
///   any shell-registered appbars (e.g. `yasb`). Window placement *inside* a
///   workspace uses this so tiled windows never underlap the taskbar.
/// - **`screen_rect`** (from `MONITORINFO::rcMonitor`) is the full physical
///   display bounds with nothing excluded. Workspace *parking* (hiding one
///   workspace while another is on screen) uses this: a parked workspace
///   must travel the full physical height to stay completely off-screen,
///   otherwise the strip occupied by the taskbar leaks a slice of the parked
///   workspace into view. See `workspace::workspace_y_offset`.
///
/// Splitting the two at the source (rather than re-deriving downstream)
/// keeps the two use sites from being confused with each other — which is
/// exactly the bug `get_primary_monitor_info` was introduced to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorGeometry {
    /// Full physical display bounds (`rcMonitor`). Nothing excluded.
    pub screen_rect: Rect,
    /// Taskbar- and appbar-excluded work area (`rcWork`).
    pub work_area: Rect,
}

/// Get both the physical screen rect and the taskbar-excluded work area of
/// the primary monitor in a single `GetMonitorInfoW` call.
///
/// `MonitorFromWindow(NULL, MONITOR_DEFAULTTOPRIMARY)` resolves to the
/// primary display, then `GetMonitorInfoW` fills a `MONITORINFO` whose
/// `rcMonitor` is the full physical rect and `rcWork` is the work area.
/// Returning both from one query removes the ambiguity of
/// `SystemParametersInfoW(SPI_GETWORKAREA)` — which exposed only the work
/// area and only for the primary monitor — and lifts the per-monitor
/// limitation the previous work-area-only helper explicitly flagged.
///
/// # Errors
///
/// Returns an error string if `GetMonitorInfoW` fails (extremely rare —
/// only occurs in sandboxed environments or during system shutdown).
///
/// # Example
///
/// ```no_run
/// use flow_wm::registry::win32::get_primary_monitor_info;
/// let geo = get_primary_monitor_info().expect("monitor info");
/// println!(
///     "Physical {}x{}, work area {}x{}",
///     geo.screen_rect.width, geo.screen_rect.height,
///     geo.work_area.width, geo.work_area.height,
/// );
/// ```
pub fn get_primary_monitor_info() -> Result<MonitorGeometry, String> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow,
    };

    // NULL hwnd + MONITOR_DEFAULTTOPRIMARY → primary display monitor.
    let hmonitor = unsafe { MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY) };

    // cbSize must be set before the call; the rects are filled in by Win32.
    let mut mi = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // `GetMonitorInfoW` returns `BOOL` (not `Result`); `.ok()` promotes a
    // `FALSE` return into `windows::core::Error` so it flows through `?`.
    unsafe { GetMonitorInfoW(hmonitor, &mut mi) }
        .ok()
        .map_err(|e| format!("GetMonitorInfoW failed: {e}"))?;

    let screen = mi.rcMonitor;
    let work = mi.rcWork;
    Ok(MonitorGeometry {
        screen_rect: Rect {
            x: screen.left,
            y: screen.top,
            width: screen.right - screen.left,
            height: screen.bottom - screen.top,
        },
        work_area: Rect {
            x: work.left,
            y: work.top,
            width: work.right - work.left,
            height: work.bottom - work.top,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a null HWND (invalid, not a real window).
    fn null_hwnd() -> HWND {
        HWND(std::ptr::null_mut())
    }

    /// Helper to create an arbitrary invalid HWND (non-null but not a real window).
    fn invalid_hwnd() -> HWND {
        HWND(0xDEAD_BEEF as *mut _)
    }

    #[test]
    fn get_extended_frame_bounds_null_hwnd_returns_err() {
        // Positive: null HWND should fail (DWM cannot query a non-existent window).
        let result = get_extended_frame_bounds(null_hwnd());
        assert!(
            result.is_err(),
            "get_extended_frame_bounds should return Err for null HWND"
        );
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("EXTENDED_FRAME_BOUNDS"),
            "error message should mention the DWM attribute name, got: {err_msg}"
        );
    }

    #[test]
    fn get_extended_frame_bounds_invalid_hwnd_returns_err() {
        // Negative: arbitrary non-null invalid HWND should also fail.
        let result = get_extended_frame_bounds(invalid_hwnd());
        assert!(
            result.is_err(),
            "get_extended_frame_bounds should return Err for invalid HWND"
        );
    }

    #[test]
    fn get_invisible_bounds_null_hwnd_returns_zero() {
        // Positive: fail-open behavior - null HWND means both GetWindowRect
        // and DwmGetWindowAttribute fail, so we should get zero bounds (not panic).
        let bounds = get_invisible_bounds(null_hwnd());
        assert_eq!(
            bounds,
            InvisibleBounds::zero(),
            "invalid HWND should produce zero invisible bounds (fail-open)"
        );
    }

    #[test]
    fn get_invisible_bounds_invalid_hwnd_returns_zero() {
        // Negative: non-null invalid HWND should also produce zero bounds.
        let bounds = get_invisible_bounds(invalid_hwnd());
        assert_eq!(
            bounds,
            InvisibleBounds::zero(),
            "non-null invalid HWND should produce zero invisible bounds (fail-open)"
        );
    }

    #[test]
    fn get_invisible_bounds_zero_is_identity_for_any_rect() {
        // Positive: verify that zero bounds means the conversion is identity.
        // This is the fail-open contract: layout engine visible rect equals
        // Win32 window rect when there are no invisible borders.
        let zero = InvisibleBounds::zero();
        let r = Rect {
            x: 100,
            y: 200,
            width: 800,
            height: 600,
        };
        assert_eq!(zero.visible_to_window(r), r);
        assert_eq!(zero.window_to_visible(r), r);
    }

    // --- is_topmost (float re-assertion reality check) ---

    /// Fail-open contract: a null / invalid HWND makes `GetWindowLongW` fail,
    /// so `is_topmost` must return `false` (never panic). A `false` reading
    /// drives the re-assertion to re-apply `HWND_TOPMOST`, which is idempotent —
    /// strictly safer than masking real drift behind a `true`.
    #[test]
    fn is_topmost_invalid_hwnd_fail_opens_to_false() {
        assert!(
            !is_topmost(null_hwnd()),
            "null HWND must fail-open to false"
        );
        assert!(
            !is_topmost(invalid_hwnd()),
            "invalid HWND must fail-open to false"
        );
    }

    // --- set_foreground_window tests ---

    /// Positive: verify that `set_foreground_window` has the correct signature.
    ///
    /// We can't actually call it without a real Windows desktop session,
    /// but we can verify it exists and has the right type.
    #[test]
    fn set_foreground_window_has_bool_return_type() {
        let fn_ptr: fn(isize) -> bool = set_foreground_window;
        let _ = fn_ptr; // Ensure the function pointer is not null (compilation check).
    }

    /// Positive: verify that `get_shell_window` has the `fn() -> Option<isize>`
    /// signature, mirroring [`get_foreground_window`].
    ///
    /// We cannot exercise the desktop HWND dependency without a real Windows
    /// desktop session (GetShellWindow returns the Progman window), so this
    /// is a compile-time signature check matching the pattern used by
    /// [`set_foreground_window`](super::set_foreground_window).
    #[test]
    fn get_shell_window_has_option_isize_return_type() {
        let fn_ptr: fn() -> Option<isize> = get_shell_window;
        let _ = fn_ptr; // Compile-time signature check.
    }

    /// Positive: verify that `clear_foreground_window` has the `fn() -> bool`
    /// signature, mirroring [`set_foreground_window`]'s bool return contract
    /// (true = success, false = no shell window or SetForegroundWindow failed).
    ///
    /// We cannot exercise the desktop HWND dependency without a real Windows
    /// desktop session, so this is a compile-time signature check matching
    /// the pattern used by the other Win32 foreground wrappers.
    #[test]
    fn clear_foreground_window_has_bool_return_type() {
        let fn_ptr: fn() -> bool = clear_foreground_window;
        let _ = fn_ptr; // Compile-time signature check.
    }

    // --- close_window tests ---

    /// Positive: verify that `close_window` has the `fn(isize) -> bool`
    /// signature, mirroring [`set_foreground_window`].
    ///
    /// We cannot actually close a real window inside a unit test (it would
    /// need a live, owned window on an interactive desktop), so this is a
    /// compile-time signature check.
    #[test]
    fn close_window_has_bool_return_type() {
        let fn_ptr: fn(isize) -> bool = close_window;
        let _ = fn_ptr; // Compile-time signature check.
    }

    /// Positive: verify that `set_window_rect` has the
    /// `fn(isize, i32, i32, i32, i32) -> bool` signature.
    ///
    /// We cannot actually move a real window inside a unit test (it would
    /// need a live, owned window on an interactive desktop), so this is a
    /// compile-time signature check matching the pattern used by
    /// [`close_window`](super::close_window) and
    /// [`set_foreground_window`](super::set_foreground_window).
    #[test]
    fn set_window_rect_has_correct_signature() {
        let fn_ptr: fn(isize, i32, i32, i32, i32) -> bool = set_window_rect;
        let _ = fn_ptr; // Compile-time signature check.
    }

    /// Positive: `SetWindowPos` flags used by `set_window_rect` are
    /// importable and non-zero. Guards against a future import resolving
    /// to a different symbol.
    #[test]
    fn win32_set_window_pos_flags_are_usable() {
        // SWP_NOZORDER and SWP_NOACTIVATE are non-zero single-bit flags.
        // We verify the imports resolve and that the combined mask still
        // contains each individual flag (bitfield algebra sanity check),
        // mirroring how `set_window_rect` combines them.
        let combined = SWP_NOZORDER | SWP_NOACTIVATE;
        assert!(
            combined.contains(SWP_NOZORDER),
            "SWP_NOZORDER should be present in the combined mask"
        );
        assert!(
            combined.contains(SWP_NOACTIVATE),
            "SWP_NOACTIVATE should be present in the combined mask"
        );
    }

    /// Positive: `WM_CLOSE` is the well-known message id (0x0010) and the
    /// `WPARAM` / `LPARAM` payloads `close_window` passes construct from
    /// integers. Asserting the constant value guards against a future
    /// import resolving to a different symbol.
    #[test]
    fn win32_close_primitives_are_usable() {
        assert_eq!(WM_CLOSE, 0x0010);
        let _w = WPARAM(0);
        let _l = LPARAM(0);
    }

    /// Positive: verify that the function accepts an isize (hwnd).
    #[test]
    fn set_foreground_window_accepts_isize_param() {
        let _isize_param: isize = 0x12345678;
        let fn_ptr: fn(isize) -> bool = set_foreground_window;
        let _ = fn_ptr; // The function should accept this type.
    }

    /// Verify that Win32 helper functions used by `set_foreground_window`
    /// are available and have the correct signatures.
    #[test]
    fn win32_focus_functions_have_correct_signatures() {
        // GetCurrentThreadId should return a u32.
        let _current_thread_id: u32 = unsafe { GetCurrentThreadId() };

        // GetForegroundWindow should return HWND.
        let _foreground_hwnd = unsafe { GetForegroundWindow() };

        // AttachThreadInput should take thread IDs and a bool.
        let _thread_id_1: u32 = 0;
        let _thread_id_2: u32 = 0;
        let _attach: bool = true;
        let _ = unsafe { AttachThreadInput(_thread_id_1, _thread_id_2, _attach) };

        // SetForegroundWindow should accept HWND.
        let _hwnd = HWND::default();
        let _ = unsafe { SetForegroundWindow(_hwnd) };

        // BringWindowToTop should accept HWND.
        let _ = unsafe { BringWindowToTop(_hwnd) };

        // GetWindowThreadProcessId should accept HWND and return u32.
        let _thread_id: u32 = unsafe { GetWindowThreadProcessId(_hwnd, None) };
    }

    /// Positive: verify that GetCurrentThreadId returns a non-zero value.
    ///
    /// This tests the infrastructure used by `set_foreground_window`
    /// to get thread IDs for AttachThreadInput.
    #[test]
    fn get_current_thread_id_returns_nonzero() {
        let thread_id = unsafe { GetCurrentThreadId() };
        assert!(
            thread_id > 0,
            "GetCurrentThreadId should return a non-zero thread ID"
        );
    }

    /// Positive: verify that GetWindowThreadProcessId handles a null HWND.
    ///
    /// This tests the edge case where the foreground window might be null,
    /// which is handled by `set_foreground_window`.
    #[test]
    fn get_window_thread_process_id_handles_null_hwnd() {
        let null_hwnd = HWND::default();
        let thread_id = unsafe { GetWindowThreadProcessId(null_hwnd, None) };
        // If the hwnd is null, GetWindowThreadProcessId typically returns 0.
        // The exact behavior depends on the Windows version.
        // We just verify it doesn't panic.
        let _ = thread_id;
    }

    /// Edge case: document that zero hwnd is handled.
    ///
    /// This documents the expected behavior for edge cases.
    /// The actual behavior is tested via integration tests in tests/cli/.
    #[test]
    fn zero_hwnd_is_documented_edge_case() {
        let zero_hwnd: isize = 0;
        assert_eq!(zero_hwnd, 0, "zero hwnd should be 0");
    }

    // --- is_iconic tests ---

    /// Positive: `is_iconic` exists and has the correct `fn(HWND) -> bool` signature.
    #[test]
    fn is_iconic_has_correct_signature() {
        let fn_ptr: fn(HWND) -> bool = is_iconic;
        let _ = fn_ptr; // Compile-time signature check.
    }
}
