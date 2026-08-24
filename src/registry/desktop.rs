//! Desktop management for debug/test builds.
//!
//! Functions to create, open, and switch Windows desktops, used by the daemon
//! and test infrastructure so all window operations happen on an isolated
//! desktop instead of the user's real desktop.
//!
//! **Only compiled in debug builds** (`#[cfg(debug_assertions)]`). Excluded from
//! release builds entirely — no desktop code ships in the production binary.
//!
//! # Why isolate?
//!
//! Integration tests create and manipulate windows. Without isolation these test
//! windows would appear on the user's actual desktop, interfere with their work,
//! and potentially break their layout. Switching to a dedicated test desktop
//! keeps all test window operations invisible to the user. The typical flow:
//! test code creates a desktop and sets the test thread onto it, spawns `flowd`
//! (which calls [`switch_to_desktop`], scans existing windows, and starts the
//! hook thread on that desktop), runs the tests, then restores the original
//! desktop and closes the test one.
//!
//! # Handle lifetime
//!
//! - Handles from [`create_desktop`] must be closed via [`close_desktop`].
//! - Handles from [`current_desktop`] must **not** be closed (managed by Windows).
//! - The handle opened by [`switch_to_desktop`] is intentionally **leaked**
//!   (see its docs for rationale).

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_CREATEMENU,
    DESKTOP_CREATEWINDOW, DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP, GetThreadDesktop, HDESK,
    OpenDesktopW, OpenInputDesktop, SetThreadDesktop, SwitchDesktop,
};
use windows::core::PCWSTR;

/// Access rights used for all desktop operations (winuser.h
/// desktop-specific rights):
/// - `DESKTOP_READOBJECTS` (0x01) — read window data
/// - `DESKTOP_CREATEWINDOW` (0x02) — create top-level windows (needed by
///   `TestWindow::create` and the daemon's border overlays)
/// - `DESKTOP_CREATEMENU` (0x04) — create menus
const DESKTOP_ACCESS: u32 = DESKTOP_READOBJECTS.0 | DESKTOP_CREATEWINDOW.0 | DESKTOP_CREATEMENU.0;

/// [`DESKTOP_ACCESS`] plus `DESKTOP_SWITCHDESKTOP` (0x0100) — required on a
/// desktop handle to make it the session's input desktop via
/// [`SwitchDesktop`]. Only requested where needed (input-desktop tests);
/// the plain window-isolation tests never switch.
fn access_with_switch() -> DESKTOP_ACCESS_FLAGS {
    DESKTOP_ACCESS_FLAGS(DESKTOP_ACCESS | DESKTOP_SWITCHDESKTOP.0)
}

/// Creates a new Windows desktop with the given name.
///
/// Returns a handle to the new desktop. The caller is responsible for closing
/// it via [`close_desktop`] when no longer needed.
///
/// # Errors
///
/// Returns an error if `CreateDesktopW` fails (e.g. name already exists).
pub fn create_desktop(name: &str) -> Result<HDESK, String> {
    let wide_name = wide_null(name);

    // SAFETY: CreateDesktopW creates a new desktop object. No device or
    // devmode is needed for a simple hidden desktop. DESKTOP_SWITCHDESKTOP
    // is included so the creator may later promote this desktop to the
    // session's input desktop (see make_input_desktop).
    let desktop = unsafe {
        CreateDesktopW(
            PCWSTR(wide_name.as_ptr()),
            PCWSTR::null(),
            None,
            DESKTOP_CONTROL_FLAGS(0),
            access_with_switch().0,
            None,
        )
    };

    desktop.map_err(|e| format!("failed to create desktop '{name}': {e}"))
}

/// Opens an existing desktop by name and switches the calling thread to it.
///
/// Used by the daemon and hook thread to join the test desktop.
///
/// # Handle lifetime
///
/// The `OpenDesktopW` handle opened here is **intentionally not closed**.
/// The handle must remain valid for the lifetime of the thread's desktop
/// assignment. If the handle were closed, the thread could be orphaned on
/// a destroyed desktop once all other handles are released. Windows frees
/// the handle when the process exits.
///
/// # Errors
///
/// Returns an error if `OpenDesktopW` or `SetThreadDesktop` fails.
pub fn switch_to_desktop(name: &str) -> Result<(), String> {
    let desktop = open_desktop(name)?;
    set_thread_desktop(desktop)?;
    log::info!("desktop: switched to '{name}'");
    Ok(())
}

/// Switches the calling thread to the given desktop handle.
///
/// Use this to restore a previously saved desktop via [`current_desktop`].
///
/// # Errors
///
/// Returns an error if `SetThreadDesktop` fails.
pub fn set_thread_desktop(desktop: HDESK) -> Result<(), String> {
    // SAFETY: SetThreadDesktop switches the calling thread to the given desktop.
    unsafe { SetThreadDesktop(desktop) }.map_err(|e| format!("failed to set thread desktop: {e}"))
}

/// Returns a handle to the calling thread's current desktop.
///
/// The returned handle should **not** be closed with [`close_desktop`] — it
/// is managed by Windows.
///
/// # Errors
///
/// Returns an error if `GetThreadDesktop` fails (extremely unlikely).
pub fn current_desktop() -> Result<HDESK, String> {
    // SAFETY: GetThreadDesktop returns a handle to the thread's desktop.
    // Per MSDN, the returned handle does not need to be closed.
    unsafe { GetThreadDesktop(windows::Win32::System::Threading::GetCurrentThreadId()) }
        .map_err(|e| format!("failed to get current desktop: {e}"))
}

/// Closes a desktop handle obtained from [`create_desktop`].
///
/// The desktop is destroyed when all handles are closed and no threads are
/// assigned to it.
pub fn close_desktop(desktop: HDESK) {
    // SAFETY: CloseDesktop releases a desktop handle.
    unsafe {
        let _ = CloseDesktop(desktop);
    }
}

/// Opens an existing desktop by name, returning a handle without switching.
fn open_desktop(name: &str) -> Result<HDESK, String> {
    let wide_name = wide_null(name);

    // SAFETY: OpenDesktopW opens an existing desktop by name.
    let desktop = unsafe {
        OpenDesktopW(
            PCWSTR(wide_name.as_ptr()),
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS,
        )
    };

    desktop.map_err(|e| format!("failed to open desktop '{name}': {e}"))
}

/// Converts a Rust string to a null-terminated UTF-16 vector.
fn wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Makes the given desktop the session's **input** desktop.
///
/// Used by cursor integration tests (ticket #34): cursor APIs
/// (`GetCursorPos` / `SetCursorPos`) are gated on the input desktop, so the
/// isolated test desktop must be promoted for the daemon's warp
/// `SetCursorPos` (and the test's `GetCursorPos` read-back) to be permitted.
/// The caller captures the previous input desktop via [`input_desktop`]
/// first and restores it with [`restore_input_desktop`] — the RAII flow is
/// managed by the test harness.
///
/// # Errors
///
/// Returns an error if `SwitchDesktop` fails (e.g. the handle lacks
/// `DESKTOP_SWITCHDESKTOP`, or the process is not allowed to switch).
pub fn make_input_desktop(desktop: HDESK) -> Result<(), String> {
    // SAFETY: SwitchDesktop requires a handle with DESKTOP_SWITCHDESKTOP
    // access. The handle comes from create_desktop, which grants it via
    // DESKTOP_ACCESS | DESKTOP_SWITCHDESKTOP.
    unsafe { SwitchDesktop(desktop) }.map_err(|e| format!("failed to switch input desktop: {e}"))
}

/// Returns the session's current input desktop handle, for a later
/// [`restore_input_desktop`] call.
///
/// The caller must close the handle via [`close_desktop`] once done with it
/// (unlike [`current_desktop`], whose handle Windows manages).
///
/// # Errors
///
/// Returns an error if `OpenInputDesktop` fails.
pub fn input_desktop() -> Result<HDESK, String> {
    // SAFETY: OpenInputDesktop opens a handle to the current input desktop.
    unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, access_with_switch()) }
        .map_err(|e| format!("failed to open input desktop: {e}"))
}

/// Restores a previously captured input desktop (inverse of
/// [`make_input_desktop`]).
///
/// # Errors
///
/// Returns an error if `SwitchDesktop` fails.
pub fn restore_input_desktop(desktop: HDESK) -> Result<(), String> {
    make_input_desktop(desktop)
}
