//! Window registry — authoritative source of truth for all tracked windows.
//!
//! The bridge between the Windows OS window system and flow's internal layout
//! model. It hooks into Win32's WinEvent system to detect window creation,
//! destruction, focus changes, minimize, restore, maximize, and fullscreen
//! transitions. Each window is classified as
//! [`Tiling`](types::TilingState), [`Floating`](types::FloatingState), or
//! [`Ignored`](types::IgnoredReason) based on config rules, and per-window state
//! is maintained throughout the window's entire lifecycle.
//!
//! # Threading model
//!
//! The registry is **not** wrapped in `Arc<Mutex>`. All subsystems in flow take
//! `&mut self` and the borrow checker enforces exclusive access at compile time.
//! The background hook thread never touches the registry directly: it only sends
//! typed [`HookEvent`]s through an `mpsc` channel (non-blocking). The main IPC
//! thread drains the channel and applies all state transitions, keeping all Win32
//! HWND dereferencing on a single thread. See the developer guide's *Threading
//! Model* chapter (`docs/src/dev-guide/threading-model.md`).
//!
//! # Submodules
//!
//! | Module | Responsibility |
//! |--------|---------------|
//! | [`types`] | Vocabulary types — [`Window`], [`WindowState`], [`VirtualSlot`] |
//! | [`win32`] | Safe wrappers around Win32 window query APIs |
//! | [`classification`] | Window rule classification (pure logic, no Win32) |
//! | [`core`] | Core [`WindowRegistry`] struct with init scan and state transitions |
//! | [`hooks`] | WinEvent hook setup on a background thread |
//! | [`desktop`] | Desktop switching for debug/test builds (excluded from release) |
//!
//! The full classification algorithm, the window-lifecycle state machine, and the
//! 7px `GetWindowRect` border quirk are documented with diagrams in the
//! developer guide's *Window Registry* chapter
//! (`docs/src/dev-guide/window-registry.md`).

pub mod classification;
pub mod core;
pub mod hooks;
pub mod types;
pub mod win32;

/// Desktop switching for debug/test builds only.
///
/// Provides `switch_to_desktop` so the daemon can join an isolated test desktop.
/// Excluded from release builds entirely (`#[cfg(debug_assertions)]`).
/// Desktop creation/cleanup lives in `tests/cli/test_desktop.rs`.
#[cfg(debug_assertions)]
pub mod desktop;

pub use classification::{ClassificationPipeline, WindowCandidate, matches_rule};
pub use core::WindowRegistry;
pub use hooks::HookEvent;
pub use types::{FloatingState, IgnoredReason, TilingState, VirtualSlot, Window, WindowState};
pub use win32::WindowInfo;

// `desktop` is not re-exported — it's used internally by `main.rs` and the hook
// thread for debug/test-mode isolation, not by external consumers.
// Excluded from release builds via `#[cfg(debug_assertions)]`.
