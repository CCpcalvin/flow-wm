//! Window registry — authoritative source of truth for all tracked windows.
//!
//! The registry is the bridge between the Windows OS window system and stm's
//! internal layout model. It hooks into Win32's WinEvent system to detect
//! window creation, destruction, focus changes, minimize, restore, maximize,
//! and fullscreen transitions. Each window is classified as
//! [`Tiling`](types::TilingState), [`Floating`](types::FloatingState), or
//! [`Ignored`](types::IgnoredReason) based on config rules, and per-window
//! state is maintained throughout the window's entire lifecycle.
//!
//! # Architecture: Where the Registry Fits
//!
//! The registry sits between the raw Win32 event system and the layout engine:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │ Windows OS (Win32)                                              │
//! │  ┌──────────────────────────────────────┐                       │
//! │  │ WinEvent hooks (SetWinEventHook)     │                       │
//! │  └──────────────┬───────────────────────┘                       │
//! │                 │ HookEvent (via mpsc channel)                  │
//! │                 ▼                                               │
//! │  ┌──────────────────────────────────────┐   ┌──────────────┐    │
//! │  │ WindowRegistry (Arc<Mutex<>>)        │◄──│ IPC Server   │    │
//! │  │ • classify windows                   │   │ (query API)  │    │
//! │  │ • track state transitions            │   └──────────────┘    │
//! │  │ • serialize to JSON                  │                       │
//! │  └──────────────┬───────────────────────┘                       │
//! │                 │ WindowId + state                              │
//! │                 ▼                                               │
//! │  ┌──────────────────────────────────────┐                       │
//! │  │ LayoutEngine (pure layout math)      │                       │
//! │  │ • no Win32 knowledge                 │                       │
//! │  │ • operates on WindowId, not HWND     │                       │
//! │  └──────────────────────────────────────┘                        │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Window Lifecycle
//!
//! Every tracked window follows a well-defined lifecycle:
//!
//! ```text
//!                     ┌──────────────┐
//!                     │  Not tracked │
//!                     └──────┬───────┘
//!              EVENT_OBJECT_CREATE │ (or init scan)
//!                            ▼
//!                     ┌──────────────┐   maximized/fullscreen
//!                     │  Classify    │──────────────────────► Ignored
//!                     │  (rules)     │
//!                     └──────┬───────┘
//!              ┌─────────────┼─────────────┐
//!              ▼                             ▼
//!         Tiling::Active              Floating::Active
//!              │                             │
//!     MinimizeStart │                 MinimizeStart │
//!              ▼                             ▼
//!       Tiling::Minimized            Floating::Minimized
//!              │                             │
//!     MinimizeEnd   │                 MinimizeEnd   │
//!              └──────────┘                 └──────────┘
//!                     │
//!          EVENT_OBJECT_DESTROY
//!                     ▼
//!               Removed from registry
//! ```
//!
//! # Threading Model
//!
//! The registry is shared between two threads via `Arc<Mutex<WindowRegistry>>`:
//!
//! - **IPC thread** (main) — holds the `MutexGuard` to process hook events
//!   and answer query commands between IPC messages.
//! - **Hook thread** (background) — runs the WinEvent hook callback,
//!   sends typed [`HookEvent`]s through an `mpsc` channel (non-blocking).
//!
//! The hook thread **never** touches the registry directly. It only sends
//! events through the channel. The IPC thread drains these events and applies
//! all state transitions under its `MutexGuard`. This eliminates data races
//! and keeps all Win32 HWND dereferencing on a single thread.
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
//! | [`desktop`] | Desktop management for test isolation |

pub mod classification;
pub mod core;
pub mod desktop;
pub mod hooks;
pub mod types;
pub mod win32;

pub use classification::{WindowCandidate, classify_window, classify_with_state, matches_rule};
pub use core::WindowRegistry;
pub use hooks::HookEvent;
pub use types::{FloatingState, IgnoredReason, TilingState, VirtualSlot, Window, WindowState};
pub use win32::WindowInfo;

// `desktop` is not re-exported — it's used internally by `main.rs` and the hook
// thread for test-mode isolation, not by external consumers.
