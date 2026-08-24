//! System cursor control — the restore escape hatch.
//!
//! FlowWM's cursor-hide mechanism blanks the mouse cursor by replacing the
//! system cursor set. If `flowd` dies or is hard-killed while cursors are
//! blanked, the user is left with an invisible mouse — so the recovery path
//! must not depend on the daemon being alive.
//!
//! That recovery path is this module: [`restore_system_cursors`] calls the
//! OS's restore-system-cursors action directly in the calling process
//! (`flow`), requiring no config, no IPC, and no daemon state.

use windows::Win32::UI::WindowsAndMessaging::{SPI_SETCURSORS, SystemParametersInfoW};

/// Restore the full system cursor set from the registry defaults.
///
/// Calls `SystemParametersInfoW(SPI_SETCURSORS, ...)`, which reloads every
/// system cursor (arrow, I-beam, wait, resize, …) from the current registry
/// scheme. This is the documented recovery action for applications that swap
/// system cursors: any replacement cursors installed via `SetSystemCursor`
/// are discarded and the originals come back. Idempotent — the call simply
/// reloads the registry scheme, so restoring already-normal cursors is a
/// no-op.
///
/// Runs entirely in the calling process — see the [module docs](self) for why
/// the escape hatch must be daemonless.
///
/// # Errors
///
/// Returns an error string when the Win32 call fails (e.g. the caller lacks
/// desktop access), with the OS error message appended for diagnostics.
pub fn restore_system_cursors() -> Result<(), String> {
    // SAFETY: SPI_SETCURSORS ignores `uiparam` and `pvparam` (both must be
    // zero/NULL per the docs); the call has no preconditions on caller state.
    // A failure returns a windows-core error, never undefined behaviour.
    unsafe {
        SystemParametersInfoW(SPI_SETCURSORS, 0, None, Default::default())
            .map_err(|e| format!("SystemParametersInfo(SPI_SETCURSORS) failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::restore_system_cursors;

    /// Restoring already-normal cursors must succeed — the command only ever
    /// restores, never blanks, so it is harmless by construction.
    #[test]
    fn restore_succeeds_and_is_idempotent() {
        restore_system_cursors().expect("SPI_SETCURSORS should succeed");
        restore_system_cursors().expect("SPI_SETCURSORS should be idempotent");
    }
}
