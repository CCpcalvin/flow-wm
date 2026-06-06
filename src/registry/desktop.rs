//! Desktop switching for test mode.
//!
//! Provides [`switch_to_desktop`] which opens an existing Windows desktop
//! by name and switches the calling thread to it. Used by the daemon in
//! test mode so it operates on an isolated desktop.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::System::StationsAndDesktops::{
    DESKTOP_CONTROL_FLAGS, OpenDesktopW, SetThreadDesktop,
};
use windows::core::PCWSTR;

/// Opens an existing desktop by name and switches the calling thread to it.
///
/// Used in test mode so the daemon (and its hook thread) operate on an
/// isolated desktop instead of the user's real desktop.
///
/// The requested access rights are:
/// - `DESKTOP_READOBJECTS` (0x01) — read window data
/// - `DESKTOP_WRITEOBJECTS` (0x02) — write window data
/// - `DESKTOP_ENUMERATE` (0x04) — enumerate windows
///
/// # Arguments
///
/// * `name` — The desktop name (e.g., `"stm-test-0"`).
///
/// # Errors
///
/// Returns an error if `OpenDesktopW` or `SetThreadDesktop` fails.
pub fn switch_to_desktop(name: &str) -> Result<(), String> {
    let wide_name: Vec<u16> = OsStr::new(name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS | DESKTOP_ENUMERATE
    let access: u32 = 0x0001 | 0x0002 | 0x0004;

    // SAFETY: OpenDesktopW opens an existing desktop by name. The returned
    // handle is a desktop object handle managed by Windows.
    let desktop = unsafe {
        OpenDesktopW(
            PCWSTR(wide_name.as_ptr()),
            DESKTOP_CONTROL_FLAGS(0),
            false,
            access,
        )
    };

    let desktop = desktop.map_err(|e| format!("failed to open desktop '{name}': {e}"))?;

    // SAFETY: SetThreadDesktop switches the calling thread to the given desktop.
    unsafe { SetThreadDesktop(desktop) }
        .map_err(|e| format!("failed to switch to desktop '{name}': {e}"))?;

    log::info!("stmd: switched to desktop '{name}'");
    Ok(())
}
