//! Per-window state types tracked by the registry.
//!
//! These types define the vocabulary for the window registry.
//! All types in this module are Windows-only since they depend on Win32's `HWND`.

use std::path::PathBuf;

use crate::common::{Rect, Size};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::HWND;

// ── HWND serde helper ───────────────────────────────────────────────

/// Serde helper for serializing/deserializing `HWND` as `isize`.
///
/// Win32 window handles are stored as opaque pointers, but their underlying
/// value is an `isize`. This module provides `#[serde(with)]` support for
/// fields of type `HWND`.
mod hwnd_serde {
    use super::HWND;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serializes an `HWND` as its underlying `isize` value.
    pub fn serialize<S: Serializer>(hwnd: &HWND, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(hwnd.0 as isize as i64)
    }

    /// Deserializes an `isize` into an `HWND`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<HWND, D::Error> {
        let val: i64 = Deserialize::deserialize(d)?;
        Ok(HWND(val as isize as *mut _))
    }
}

// ── Window ──────────────────────────────────────────────────────────

/// Per-window state tracked by the registry.
///
/// This is the authoritative record for every window the daemon manages.
/// Each [`Window`] is identified by its Win32 `HWND` and carries all metadata
/// needed for classification, layout assignment, and recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    /// Win32 window handle.
    #[serde(with = "hwnd_serde")]
    pub hwnd: HWND,

    /// Executable name (e.g. `"code.exe"`).
    pub exe: String,

    /// Window title bar text.
    pub title: String,

    /// Win32 window class name.
    pub class: String,

    /// Full path to the executable.
    pub process_path: PathBuf,

    /// Current lifecycle state (tiling, floating, or ignored).
    pub state: WindowState,

    /// Position and size of the window before stm ever touched it.
    ///
    /// Used by `stm restore` to return windows to their pre-managed positions
    /// if the daemon dies.
    pub pre_manage_rect: Rect,

    /// Preferred unmanaged size, updated on explicit user resize.
    pub last_natural_size: Size,

    /// Remembered virtual-slot position for minimize/restore cycles.
    pub last_virtual_slot: Option<VirtualSlot>,
}

// SAFETY: `Window` contains `HWND` (a raw pointer), but we treat it as an
// opaque handle value. All Win32 API calls using this handle happen on the
// thread that owns the `MutexGuard<WindowRegistry>` — we never dereference
// HWND on a different thread. Safe to send across thread boundaries.
unsafe impl Send for Window {}

impl Window {
    /// Creates a new [`Window`] entry for initial registration.
    ///
    /// `last_virtual_slot` starts as `None`; it is populated when the window
    /// is assigned a virtual layout position. `last_natural_size` defaults to
    /// the size component of `pre_manage_rect`.
    #[must_use]
    pub fn new(
        hwnd: HWND,
        exe: String,
        title: String,
        class: String,
        process_path: PathBuf,
        pre_manage_rect: Rect,
        initial_state: WindowState,
    ) -> Self {
        let last_natural_size = Size {
            w: pre_manage_rect.width,
            h: pre_manage_rect.height,
        };
        Self {
            hwnd,
            exe,
            title,
            class,
            process_path,
            state: initial_state,
            pre_manage_rect,
            last_natural_size,
            last_virtual_slot: None,
        }
    }
}

// ── WindowState ────────────────────────────────────────────────────

/// Lifecycle state of a managed window.
///
/// Every window tracked by the registry is in exactly one of these states.
/// The state determines how the layout engine and compositor interact with
/// the window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WindowState {
    /// Window is participating in the tiling layout.
    Tiling(TilingState),
    /// Window is floating (user-dragged or rule-assigned).
    Floating(FloatingState),
    /// Window is ignored by stm (maximized, fullscreen, or explicit rule).
    Ignored(IgnoredReason),
}

// ── TilingState ────────────────────────────────────────────────────

/// Sub-state for windows participating in the tiling layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TilingState {
    /// Window is actively tiled at the given virtual position.
    Active {
        /// Column index in the virtual layout.
        col: usize,
        /// Row index within the column.
        row: usize,
    },
    /// Window is minimized (preserving its virtual-slot assignment).
    Minimized,
}

// ── FloatingState ──────────────────────────────────────────────────

/// Sub-state for floating (non-tiled) windows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FloatingState {
    /// Window is visible and floating at the given rect.
    Active {
        /// Current screen rectangle of the floating window.
        rect: Rect,
    },
    /// Window is minimized to the taskbar.
    Minimized,
}

// ── IgnoredReason ──────────────────────────────────────────────────

/// Reason why a window is ignored by stm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IgnoredReason {
    /// Window is maximized (`WS_MAXIMIZE` style).
    Maximized,
    /// Window is in exclusive or borderless fullscreen mode.
    Fullscreen,
    /// Window matched an explicit `ignore` rule in the config.
    ExplicitRule,
}

// ── VirtualSlot ─────────────────────────────────────────────────────

/// Virtual layout position remembered for minimize/restore cycles.
///
/// When a tiled window is minimized, its column/row position is saved here
/// so it can be restored to the same slot when un-minimized.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualSlot {
    /// Column index in the virtual layout.
    pub col: usize,
    /// Row index within the column.
    pub row: usize,
}
