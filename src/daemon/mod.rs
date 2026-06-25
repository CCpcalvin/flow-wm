//! ScrollTilingManager — the single top-level orchestrator for the `stmd` daemon.
//!
//! [`ScrollTilingManager`] owns all subsystems and routes events between them.
//! It is the entire application — there is no "daemon core" or higher-level
//! wrapper. Construction performs all startup work (config loading, window
//! scanning, layout initialisation, animation setup, hook registration).
//! Calling [`run()`](ScrollTilingManager::run) enters the IPC event loop.
//!
//! # Threading & event routing
//!
//! Two threads, no `Arc<Mutex>`. The background hook thread runs the WinEvent
//! callbacks and only sends [`HookEvent`]s through an `mpsc` channel — it never
//! touches any stm field. The main IPC thread owns the whole
//! [`ScrollTilingManager`] and, between IPC messages, drains the channel and
//! calls `&mut self` methods on the registry, layout, and animator. Because
//! every subsystem method takes `&mut self`, the borrow checker enforces
//! exclusive access at compile time — strictly safer than a runtime `Mutex`.
//!
//! - **Hook events**: Win32 hook → `HookEvent` → `on_window_created/destroyed/…`
//!   → registry mutation → layout change → `animate_layout`.
//! - **IPC commands**: `stm` CLI → `SocketMessage` → `dispatch()` → layout /
//!   registry query → `animate_layout` → `SocketResponse`.
//!
//! # Module structure
//!
//! The daemon module is split into focused submodules:
//!
//! - [`types`] — struct definitions ([`ScrollTilingManager`] and [`LayoutConfig`])
//! - [`new`] — constructor ([`ScrollTilingManager::new`])
//! - [`run`] — main event loop ([`ScrollTilingManager::run`] and event routing)
//! - [`hooks`] — Win32 hook event handlers
//! - [`borders`] — border overlay lifecycle (attach/detach/recolor helpers)
//! - [`dispatch`] — IPC command dispatch router and action handlers
//! - [`query`] — query handlers (extracted from dispatch)
//! - [`animation`] — animation bridge ([`ScrollTilingManager::animate_layout`])
//! - [`config_derive`] — configuration derivation helpers and tests
//! - [`shutdown`] — graceful-shutdown window rescue
//!   ([`ScrollTilingManager::rescue_stranded_windows`])
//!
//! The threading model, the hook/IPC event pipelines, and per-event behavior
//! tables are documented with sequence diagrams in the developer guide
//! (`docs/src/dev-guide/threading-model.md` and `event-pipelines.md`).

mod animation;
mod borders;
mod config_derive;
mod dispatch;
mod floating_sync;
mod hooks;
mod new;
mod query;
mod run;
mod shutdown;
mod types;

pub use types::ScrollTilingManager;
