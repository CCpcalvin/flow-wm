//! Per-window state types tracked by the registry.
//!
//! This module defines the vocabulary types used throughout the window registry.
//! These types represent the **complete lifecycle** of a managed window, from
//! initial classification through state transitions to removal.
//!
//! # State Machine
//!
//! Every tracked window exists in exactly one [`WindowState`] at any time.
//! The state determines how stm interacts with the window:
//!
//! ```text
//!                         classify_with_state()
//!                                │
//!                    ┌───────────┼───────────┐
//!                    ▼           ▼           ▼
//!               Tiling       Floating     Ignored
//!               (Active)     (Active)    (Maximized/
//!                    │           │        Fullscreen/
//!            MinimizeStart  MinimizeStart  ExplicitRule)
//!                    │           │
//!                    ▼           ▼
//!            Tiling::Minimized  Floating::Minimized
//!                    │           │
//!            MinimizeEnd    MinimizeEnd
//!                    │           │
//!                    └─────┬─────┘
//!                          ▼
//!                  Restored to Active
//!                  (with saved position)
//! ```
//!
//! # Send Safety
//!
//! All types in this module are `Send` (and most are `Send + Sync`). This is
//! critical because the registry is shared across threads via `Arc<Mutex<>>`.
//!
//! **The tricky case is `HWND`**: Win32's `HWND` wraps a raw pointer (`*mut c_void`)
//! and is `!Send` by default. We work around this by:
//! - Storing `HWND` inside [`Window`] with a manual `unsafe impl Send`.
//! - Converting `HWND` to `isize` when passing window IDs across thread
//!   boundaries (in [`HookEvent`](super::HookEvent)).
//!
//! The manual `Send` impl is sound because:
//! - We never dereference `HWND` on a different thread than the one holding
//!   the `MutexGuard<WindowRegistry>`.
//! - All Win32 API calls using the handle happen on the IPC thread.
//! - `HWND` is an opaque handle value, not a real pointer — we never read
//!   through it in Rust code.
//!
//! # Relationship to Layout Types
//!
//! The registry uses [`WindowState`] to track *what* a window is doing
//! (tiling, floating, ignored). The layout engine uses
//! [`WindowId`](crate::common::WindowId) and column/row positions to track
//! *where* a window is placed. The two systems are connected through
//! [`VirtualSlot`], which stores the layout engine's column/row assignment
//! inside the registry's [`Window`] struct.
//!
//! # Serde Integration
//!
//! All types implement `Serialize` and `Deserialize` for the query API.
//! `HWND` fields use a custom `#[serde(with)]` module that serializes to
//! `isize` (since HWND pointers aren't meaningful in JSON).

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
///
/// # Field Design Rationale
///
/// - **`hwnd`** — The Win32 window handle. Serialized as `isize` for JSON.
///   Used as the HashMap key in [`WindowRegistry`](super::core::WindowRegistry).
///
/// - **`exe`** / **`title`** / **`class`** / **`process_path`** — Window
///   metadata used by the [`classification`](super::classification) module
///   to match against config rules. These are snapshotted at registration
///   time and not updated (window titles can change, but re-classification
///   is not currently supported).
///
/// - **`state`** — The current lifecycle state. Updated by
///   [`WindowRegistry`](super::core::WindowRegistry) methods on each hook event.
///   See [module-level docs](super) for the state machine diagram.
///
/// - **`pre_manage_rect`** — The window's position and size when stm first
///   saw it. Used by `stm restore` to return windows to their pre-managed
///   positions if the daemon exits or is stopped.
///
/// - **`last_natural_size`** — The window's preferred size, updated on
///   explicit user resize. Used when the layout engine needs to determine
///   a window's natural proportions.
///
/// - **`last_virtual_slot`** — The window's last known column/row position
///   in the layout grid. Saved when a tiled window is minimized and
///   restored when it's un-minimized. This prevents the window from losing
///   its place in the layout.
///
/// # Send Safety
///
/// `Window` contains `HWND` (a raw pointer), but we treat it as an opaque
/// handle value. See the [module-level Send Safety section](super#send-safety)
/// for the full safety argument.
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
///
/// # State Transitions
///
/// Transitions are triggered by WinEvent hooks and applied by
/// [`WindowRegistry`](super::core::WindowRegistry):
///
/// | From | Event | To |
/// |------|-------|----|
/// | (new) | classification | `Tiling(Active)` / `Floating(Active)` / `Ignored(...)` |
/// | `Tiling(Active)` | `MinimizeStart` | `Tiling(Minimized)` |
/// | `Tiling(Minimized)` | `MinimizeEnd` | `Tiling(Active)` with restored slot |
/// | `Floating(Active)` | `MinimizeStart` | `Floating(Minimized)` |
/// | `Floating(Minimized)` | `MinimizeEnd` | `Floating(Active)` with original rect |
///
/// Note: transitions between `Tiling`, `Floating`, and `Ignored` (e.g., toggle
/// float, maximize while tiled) are not yet implemented — they will be added
/// when the layout engine integration is complete.
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
///
/// Tiled windows can be either actively positioned at a grid slot or minimized.
/// When minimized, the window's grid position is preserved in
/// [`Window::last_virtual_slot`](Window::last_virtual_slot) so it can be
/// restored to its original position.
///
/// # Relationship to Layout Engine
///
/// The `col` and `row` values in [`Active`](TilingState::Active) correspond to
/// the layout engine's column/row indices. These are updated when the layout
/// engine assigns a new position (via mutations like swap, focus, add).
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
///
/// Floating windows are positioned freely by the user. stm does not manage
/// their position — it only tracks whether they are active (visible) or
/// minimized. When restored from minimize, the window returns to its
/// `pre_manage_rect` position.
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
///
/// Ignored windows are excluded from all layout operations. The reason is
/// tracked for diagnostic purposes (e.g., `cargo doc` queries can show *why*
/// a window is not being tiled).
///
/// # Override Priority
///
/// Maximized and fullscreen checks happen **before** config rule evaluation
/// in [`classify_with_state`](super::classification::classify_with_state).
/// This means a window that is maximized will always be `Ignored(Maximized)`,
/// even if a config rule says to tile it. This is intentional — maximized and
/// fullscreen windows have their own window management behavior that conflicts
/// with tiling.
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
/// so it can be restored to the same slot when un-minimized. Without this,
/// a restored window would lose its place in the layout grid.
///
/// # Bridge Between Registry and Layout Engine
///
/// `VirtualSlot` is the data bridge between the registry (which tracks window
/// state) and the layout engine (which manages grid positions). The registry
/// saves the slot on minimize and reads it on restore. The layout engine
/// assigns new slots when windows are added or moved.
///
/// # Default on Restore
///
/// If `last_virtual_slot` is `None` when a tiled window is restored (which
/// shouldn't happen in normal operation), the registry defaults to `(0, 0)`.
/// This ensures the window always gets a valid position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualSlot {
    /// Column index in the virtual layout.
    pub col: usize,
    /// Row index within the column.
    pub row: usize,
}
