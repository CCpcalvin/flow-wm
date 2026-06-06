//! Window registry — authoritative source of truth for all tracked windows.
//!
//! The registry hooks into the Windows OS event system to detect window
//! creation, destruction, focus changes, minimize, restore, maximize, and
//! fullscreen transitions. It classifies each window as [`Tiling`](types::TilingState),
//! [`Floating`](types::FloatingState), or [`Ignored`](types::IgnoredReason) based on
//! config rules, maintains per-window state, and emits typed events consumed
//! by the layout engine and input interceptor.
//!
//! # Submodules
//!
//! | Module | Responsibility |
//! |--------|---------------|
//! | [`types`] | Vocabulary types — [`Window`], [`WindowState`], [`VirtualSlot`] |
//! | [`win32`] | Safe wrappers around Win32 window query APIs |
//! | [`classification`] | Window rule classification (pure logic, no Win32) |
//! | [`registry`] | Core [`WindowRegistry`] struct with init scan and state transitions |
//! | [`hooks`] | WinEvent hook setup on a background thread |

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
