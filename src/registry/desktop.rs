//! Desktop management for debug/test builds.
//!
//! Provides functions to create, open, and switch Windows desktops. Used by
//! the daemon and test infrastructure so all window operations happen on an
//! isolated desktop instead of the user's real desktop.
//!
//! **Only compiled in debug builds** (`#[cfg(debug_assertions)]`).
//! Excluded from release builds entirely — no desktop code ships in the
//! production binary.
//!
//! # Why Isolate?
//!
//! Integration tests create and manipulate windows. Without isolation, these
//! test windows would appear on the user's actual desktop, interfere with
//! their work, and potentially break their layout. By switching to a dedicated
//! test desktop, all test window operations are invisible to the user.
//!
//! # Typical Flow
//!
//! ```text
//! Test code                    Daemon (stmd)              Hook Thread
//! ┌──────────────────┐        ┌──────────────────┐      ┌──────────────────┐
//! │ create_desktop() │        │ switch_to_       │      │ switch_to_       │
//! │ set_thread_      │        │  desktop(name)   │      │  desktop(name)   │
//! │  desktop(test)   │───────►│ scan_existing_   │─────►│ SetWinEventHook  │
//! │ spawn stmd       │        │  windows()       │      │ GetMessageW loop │
//! │ ...run tests...  │        │ IPC loop         │      │ callback → send  │
//! │ set_thread_      │        └──────────────────┘      └──────────────────┘
//! │desktop(original) │
//! │ close_desktop()  │
//! └──────────────────┘
//! ```
//!
//! # Handle Lifetime
//!
//! - Handles from [`create_desktop`] must be closed via [`close_desktop`].
//! - Handles from [`current_desktop`] must **not** be closed (managed by Windows).
//! - The handle opened by [`switch_to_desktop`] is intentionally **leaked**
//!   (see its docs for rationale).

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, DESKTOP_CONTROL_FLAGS, GetThreadDesktop, HDESK, OpenDesktopW,
    SetThreadDesktop,
};
use windows::core::PCWSTR;

/// Access rights used for all desktop operations:
/// - `DESKTOP_READOBJECTS` (0x01) — read window data
/// - `DESKTOP_WRITEOBJECTS` (0x02) — write window data
/// - `DESKTOP_ENUMERATE` (0x04) — enumerate windows
const DESKTOP_ACCESS: u32 = 0x0001 | 0x0002 | 0x0004;

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
    // devmode is needed for a simple hidden desktop.
    let desktop = unsafe {
        CreateDesktopW(
            PCWSTR(wide_name.as_ptr()),
            PCWSTR::null(),
            None,
            DESKTOP_CONTROL_FLAGS(0),
            DESKTOP_ACCESS,
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
