//! ScrollTilingManager — the single top-level orchestrator for the `stmd` daemon.
//!
//! [`ScrollTilingManager`] owns all subsystems and routes events between them.
//! It is the entire application — there is no "daemon core" or higher-level
//! wrapper. Construction performs all startup work (config loading, window
//! scanning, layout initialization, animation setup, hook registration).
//! Calling [`run()`](ScrollTilingManager::run) enters the IPC event loop.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────┐
//! │                      ScrollTilingManager                             │
//! │                                                                      │
//! │  Owns:                                                               │
//! │  ┌────────────────┐  ┌──────────────┐  ┌──────────────────────────┐  │
//! │  │ WindowRegistry │  │ Vec<Monitor> │  │ WindowAnimator           │  │
//! │  │ (window state) │  │ (workspaces: │  │ (src/animation/)         │  │
//! │  └────────────────┘  │  Scrolling + │  └──────────────────────────┘  │
//! │                      │  Floating)   │                                 │
//! │  ┌────────────────┐  └──────────────┘  ┌──────────────────────────┐  │
//! │  │ PipeServer     │  ┌──────────────┐  │ (FloatingManager now     │  │
//! │  │ (IPC transport)│  │ AppConfig    │  │  lives inside Workspace) │  │
//! │  └────────────────┘  └──────────────┘  └──────────────────────────┘  │
//! │                                                                      │
//! │  Routes:                                                             │
//! │  • Hook events  → registry mutation → layout engine → animator       │
//! │  • IPC commands → layout engine / registry query → animator          │
//! └──────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Threading Model
//!
//! ```text
//! Hook Thread (background):          IPC Thread (main):
//!   SetWinEventHook ×3                owns ScrollTilingManager (all fields)
//!   GetMessageW loop                  ├─ process_hook_events()
//!       ↓ callback                    ├─ dispatch IPC command
//!   sender.send(HookEvent)            ├─ process_hook_events()
//!                                     └─ ... (repeat)
//! ```
//!
//! The hook thread never touches any STM field. It only sends [`HookEvent`]
//! through the `mpsc` channel. The IPC thread reads the channel and calls
//! methods on `registry`, `layout`, and `animator` directly — **no mutex,
//! no locking, no deadlocks**.
//!
//! Since all subsystem methods take `&mut self`, the borrow checker enforces
//! exclusive access at compile time. This is strictly safer than `Mutex`
//! (which only enforces at runtime and can deadlock).
//!
//! # Event Pipelines
//!
//! ## Hook Events
//!
//! ```text
//! Win32 hook → HookEvent → process_hook_events() → on_window_created/destroyed/...
//!     → registry.handle_created() → layout.add_window() → animate_layout()
//! ```
//!
//! ## IPC Commands
//!
//! ```text
//! stm CLI → SocketMessage → PipeServer → dispatch() → layout.swap_column()
//!     → animate_layout() → SocketResponse
//! ```
//!
//! # Module Structure
//!
//! The daemon module is split into focused submodules:
//!
//! - [`types`] — Struct definitions ([`ScrollTilingManager`] and [`LayoutConfig`])
//! - [`new`] — Constructor ([`ScrollTilingManager::new`])
//! - [`run`] — Main event loop ([`ScrollTilingManager::run`] and event routing)
//! - [`hooks`] — Win32 hook event handlers
//! - [`dispatch`] — IPC command dispatch router and action handlers
//! - [`query`] — Query handlers (extracted from dispatch)
//! - [`animation`] — Animation bridge ([`ScrollTilingManager::animate_layout`])
//! - [`config_derive`] — Configuration derivation helpers and tests

mod animation;
mod config_derive;
mod dispatch;
mod hooks;
mod new;
mod query;
mod run;
mod types;

pub use types::ScrollTilingManager;
