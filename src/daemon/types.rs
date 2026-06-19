//! ScrollTilingManager — the single top-level orchestrator for the `stmd` daemon.
//!
//! [`ScrollTilingManager`] owns all subsystems and routes events between them.
//! See individual module files for detailed documentation.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use crate::animation::WindowAnimator;
use crate::config::types::StmConfig;
use crate::floating::FloatingManager;
use crate::ipc::transport::PipeServer;
use crate::layout::engine::LayoutEngine;
use crate::registry::WindowRegistry;
use crate::registry::hooks::{HookSignal, HookThreadHandle};

/// Intermediate struct holding layout engine parameters derived from
/// [`StmConfig`]. Used during construction to keep the parameter list
/// readable and avoid recomputing values.
///
/// This type is private to the daemon module — only construction logic
/// needs these derived values.
pub(super) struct LayoutConfig {
    /// Default column width in pixels.
    ///
    /// When `StmConfig::column_width` is `Some`, this is that value directly.
    /// When `None`, this is computed from `columns_per_screen`, the monitor width,
    /// and `window_gap`:
    /// `base_content_width = (monitor_width - (N+1) * window_gap) / N`
    pub(super) column_width: u32,

    /// Minimum column width in pixels.
    pub(super) min_column_width_px: u32,

    /// Padding converted from config types to layout types.
    pub(super) padding: crate::layout::types::Padding,
}

/// The single orchestrator for the ScrollingTilingManager daemon.
///
/// Owns all subsystems and routes events between them. Lives entirely on
/// the IPC thread — no interior mutability (`Arc<Mutex<>>`) is needed.
///
/// # Architecture
///
/// ```text
/// ┌──────────────────────────────────────────────────────────────────────┐
/// │                      ScrollTilingManager                             │
/// │                                                                      │
/// │  Owns:                                                               │
/// │  ┌────────────────┐  ┌──────────────┐  ┌──────────────────────────┐  │
/// │  │ WindowRegistry │  │ LayoutEngine │  │ WindowAnimator           │  │
/// │  │ (window state) │  │ (layout math)│  │ (src/animation/)         │  │
/// │  └────────────────┘  └──────────────┘  └──────────────────────────┘  │
/// │  ┌────────────────┐  ┌──────────────┐  ┌──────────────────────────┐  │
/// │  │ PipeServer     │  │ AppConfig    │  │ FloatingManager (stub)   │  │
/// │  │ (IPC transport)│  │ (loaded once)│  │ (placeholder)            │  │
/// │  └────────────────┘  └──────────────┘  └──────────────────────────┘  │
/// │                                                                      │
/// │  Routes:                                                             │
/// │  • Hook events  → registry mutation → layout engine → animator       │
/// │  • IPC commands → layout engine / registry query → animator          │
/// └──────────────────────────────────────────────────────────────────────┘
/// ```
///
/// # Threading Model
///
/// ```text
/// Hook Thread (background):          IPC Thread (main):
///   SetWinEventHook ×3                owns ScrollTilingManager (all fields)
///   GetMessageW loop                  ├─ process_hook_events()
///       ↓ callback                    ├─ dispatch IPC command
///   sender.send(HookEvent)            ├─ process_hook_events()
///                                     └─ ... (repeat)
/// ```
///
/// The hook thread never touches any STM field. It only sends [`HookEvent`]
/// through the `mpsc` channel. The IPC thread reads the channel and calls
/// methods on `registry`, `layout`, and `animator` directly — **no mutex,
/// no locking, no deadlocks**.
///
/// Since all subsystem methods take `&mut self`, the borrow checker enforces
/// exclusive access at compile time. This is strictly safer than `Mutex`
/// (which only enforces at runtime and can deadlock).
pub struct ScrollTilingManager {
    /// Window registry — authoritative source of truth for all tracked windows.
    pub(super) registry: WindowRegistry,

    /// Layout engine — pure layout math, no Win32 knowledge.
    pub(super) layout: LayoutEngine,

    /// Window animator — background-threaded rect animation for smooth moves.
    pub(super) animator: WindowAnimator,

    /// Floating window manager — stub for future implementation.
    #[allow(dead_code)] // Stored for future floating window management.
    pub(super) floating: FloatingManager,

    /// IPC named pipe server — accepts commands from the `stm` CLI.
    pub(super) server: PipeServer,

    /// Application configuration loaded from `stm.toml`.
    ///
    /// Stored for future config hot-reload support. Not read at runtime today
    /// — the layout engine owns all derived width/bounds state internally.
    #[allow(dead_code)] // Stored for future config reload functionality.
    pub(super) config: StmConfig,

    /// Path to the configuration directory (for future reload support).
    #[allow(dead_code)] // Stored for future config reload functionality.
    pub(super) config_dir: PathBuf,

    /// Receiver for hook events from the background WinEvent hook thread.
    pub(super) hook_receiver: Receiver<crate::registry::HookEvent>,

    /// Handle to the background hook thread. Kept for its `Drop` impl
    /// which posts `WM_QUIT` to the hook thread's message loop.
    pub(super) _hook_handle: HookThreadHandle,

    /// Win32 Event handle signaled by the hook callback thread.
    ///
    /// The main event loop waits on this via `WaitForMultipleObjects` so hook
    /// events (window create/destroy/focus) are processed immediately, even
    /// when no IPC client is connected.
    ///
    /// RAII: closed automatically on drop via `HookSignal`'s `Drop` impl.
    pub(super) hook_signal: HookSignal,

    /// Set to `true` when the `Stop` IPC command is received, causing
    /// the main event loop to exit on the next iteration.
    pub(super) shutting_down: bool,

    /// Windows whose `Created` hook event fired before they were fully
    /// initialized (not yet visible, no title, styles not finalized).
    ///
    /// Each entry is `(hwnd, retry_count)`. On every `process_hook_events`
    /// call, pending windows are retried via `handle_created`. A window is
    /// removed from the list when classification succeeds or when
    /// `retry_count` exceeds `MAX_PENDING_RETRIES`.
    ///
    /// # Why this is needed
    ///
    /// `EVENT_OBJECT_CREATE` fires early in the Win32 window lifecycle —
    /// before `ShowWindow`, `SetWindowText`, or style finalization. With
    /// the event-driven loop (ResetEvent + immediate drain), hook events
    /// are processed within microseconds of arrival, so the window's
    /// classification checks (`is_window_visible`, title non-empty, etc.)
    /// fail. A short retry gives the window time to finish initializing.
    ///
    /// # Timeout interaction
    ///
    /// When this list is non-empty, `run()` uses a finite timeout (100 ms)
    /// on `WaitForMultipleObjects` instead of `INFINITE`. This ensures
    /// pending windows are retried even if no new hook events arrive.
    pub(super) pending_creations: Vec<(isize, u8)>,
}
