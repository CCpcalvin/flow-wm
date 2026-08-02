//! FlowWM — the single top-level orchestrator for the `flowd` daemon.
//!
//! [`FlowWM`] owns all subsystems and routes events between them.
//! See individual module files for detailed documentation.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use crate::animation::WindowAnimator;
use crate::config::history::HistoryStore;
use crate::config::types::FlowConfig;
use crate::ipc::transport::PipeServer;
use crate::registry::WindowRegistry;
use crate::registry::hooks::{HookSignal, HookThreadHandle};
use crate::workspace::{FloatingSpace, Monitor, ScrollingSpace, Workspace};

/// Intermediate struct holding layout engine parameters derived from
/// [`FlowConfig`]. Used during construction to keep the parameter list
/// readable and avoid recomputing values.
///
/// This type is private to the daemon module — only construction logic
/// needs these derived values.
pub(super) struct LayoutConfig {
    /// Default column width in pixels.
    ///
    /// When `FlowConfig::column_width` is `Some`, this is that value directly.
    /// When `None`, this is computed from `columns_per_screen`, the monitor width,
    /// and `window_gap`:
    /// `base_content_width = (monitor_width - (N+1) * window_gap) / N`
    pub(super) column_width: u32,

    /// Minimum column width in pixels.
    pub(super) min_column_width_px: u32,

    /// Minimum per-row window height in pixels. Caps how many windows a
    /// column can hold. Sourced directly from
    /// [`FlowConfig::min_window_height_px`].
    pub(super) min_window_height_px: u32,

    /// Padding converted from config types to layout types.
    pub(super) padding: crate::layout::types::Padding,
}

/// The single orchestrator for the FlowWM daemon.
///
/// Owns all subsystems and routes events between them. Lives entirely on
/// the IPC thread — no interior mutability (`Arc<Mutex<>>`) is needed.
///
/// # Architecture
///
/// ```text
/// ┌──────────────────────────────────────────────────────────────────────┐
/// │                      FlowWM                             │
/// │                                                                      │
/// │  Owns:                                                               │
/// │  ┌────────────────┐  ┌──────────────┐  ┌──────────────────────────┐  │
/// │  │ WindowRegistry │  │ Vec<Monitor> │  │ WindowAnimator           │  │
/// │  │ (window state) │  │ (workspaces: │  │ (src/animation/)         │  │
/// │  └────────────────┘  │  Scrolling + │  └──────────────────────────┘  │
/// │                      │  Floating)   │                                 │
/// │  ┌────────────────┐  └──────────────┘  ┌──────────────────────────┐  │
/// │  │ PipeServer     │  ┌──────────────┐  │ HistoryStore             │  │
/// │  │ (IPC transport)│  │ AppConfig    │  │ (learned rules)          │  │
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
///   SetWinEventHook ×3                owns FlowWM (all fields)
///   GetMessageW loop                  ├─ process_hook_events()
///       ↓ callback                    ├─ dispatch IPC command
///   sender.send(HookEvent)            ├─ process_hook_events()
///                                     └─ ... (repeat)
/// ```
///
/// The hook thread never touches any flow field. It only sends [`HookEvent`]
/// through the `mpsc` channel. The IPC thread reads the channel and calls
/// methods on `registry`, the active workspace's scrolling space, and the
/// `animator` directly — **no mutex, no locking, no deadlocks**.
///
/// Since all subsystem methods take `&mut self`, the borrow checker enforces
/// exclusive access at compile time. This is strictly safer than `Mutex`
/// (which only enforces at runtime and can deadlock).
pub struct FlowWM {
    /// Window registry — authoritative source of truth for all tracked windows.
    pub(super) registry: WindowRegistry,

    /// Per-app learned window modes, persisted to `history-flow-rules.toml`.
    ///
    /// Records the user's explicit `set-window float|tile` decisions so the
    /// next window of the same app is classified automatically. See
    /// (`docs/src/dev-guide/classification.md`) for the priority chain.
    pub(super) history: HistoryStore,

    /// The monitor stack — one [`Monitor`] per physical display, each owning
    /// its own vertical stack of [`Workspace`]s.
    ///
    /// The daemon currently initialises exactly one monitor (the primary
    /// display) with ten workspaces (see `new.rs`); multi-monitor support
    /// will grow this vector. Use [`active_monitor`](Self::active_monitor) to
    /// reach the monitor that currently owns keyboard focus.
    pub(super) monitors: Vec<Monitor>,

    /// Index into [`monitors`](Self::monitors) of the monitor that currently
    /// owns keyboard focus. Always a valid index — the daemon keeps at least
    /// one monitor for the lifetime of the process.
    pub(super) active_monitor: usize,

    /// Window animator — background-threaded rect animation for smooth moves.
    pub(super) animator: WindowAnimator,

    /// IPC named pipe server — accepts commands from the `flow` CLI.
    pub(super) server: PipeServer,

    /// Application configuration loaded from `flow.toml`.
    ///
    /// The live source for border colors/thickness (read by the border overlay
    /// and animation code) and the target of hot-reload. The layout engine owns
    /// its own derived width/bounds state internally (see [`crate::workspace`]).
    pub(super) config: FlowConfig,

    /// Path to the configuration directory — used to locate `flow.toml` and
    /// `flow-rules.toml` on hot-reload.
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

    /// Deadline at which float-location tracking resumes after flow suppresses
    /// it for a float animation.
    ///
    /// `None` when no suppression is active — `FLOAT_TRACKING_ACTIVE` stays on
    /// and `EVENT_OBJECT_LOCATIONCHANGE` from floats is captured live. Set by
    /// [`arm_float_tracking_suppression`](super::FlowWM::arm_float_tracking_suppression)
    /// and cleared by the resume poll in the main loop.
    pub(super) float_resume_deadline: Option<std::time::Instant>,

    /// State for the in-progress tile-window drag, or `None` when idle.
    ///
    /// Set by [`on_drag_start`](super::drag::FlowWM::on_drag_start) (MoveSizeStart
    /// on a tiled window) and cleared by
    /// [`on_drag_end`](super::drag::FlowWM::on_drag_end) (MoveSizeEnd). When
    /// `Some`, layout-mutating IPC is rejected with
    /// [`SocketResponse::Busy`](crate::ipc::message::SocketResponse::Busy).
    pub(super) drag_state: Option<super::drag::DragState>,

    /// The single edge-scroll auto-repeat scheduler.
    ///
    /// A single instance shared by every edge-scroll consumer; today only the
    /// tile-drag move handler feeds it. Promoted from per-drag ownership so a
    /// later hover feed can reuse it. See (`docs/src/dev-guide/tile-drag.md`)
    /// and the `edge_scroll` module.
    pub(super) edge_scroll: super::edge_scroll::EdgeScrollScheduler,

    /// Already-clamped effective edge-scroll timings for the current consumer
    /// (a tile drag today). Set fresh at drag start from
    /// `DragConfig` (`src/config/types.rs`); the scheduler consumes them with
    /// no per-event clamp math.
    pub(super) edge_scroll_timings: super::edge_scroll::EdgeScrollTimings,

    /// Deadline at which the armed auto-repeat timer fires, or `None` when no
    /// repeat is armed. The main loop folds this into its wait-timeout
    /// `min`-reduce (see `compute_wait_timeout_inner` in `run.rs`) and fires
    /// `maybe_fire_edge_scroll` when it arrives. Set/cleared by the `Arm` /
    /// `Cancel` actions the scheduler emits.
    pub(super) edge_scroll_deadline: Option<std::time::Instant>,

    /// The pure focus-follows-mouse / edge-hover decision controller
    /// (`crate::hover::HoverController`). The wiring (this daemon module)
    /// translates its [`HoverAction`](crate::hover::HoverAction)s into
    /// `GetCursorPos` polls and OS foreground pushes. (`docs/src/dev-guide/hover.md`)
    pub(super) hover: crate::hover::HoverController,

    /// Already-clamped effective hover dwell durations for the current poll.
    /// Focus dwell is read from `config.hover.focus_dwell_ms`; edge dwell from
    /// `config.hover.edge_dwell_ms`. Mirrors
    /// [`edge_scroll_timings`](Self::edge_scroll_timings).
    pub(super) hover_timings: crate::hover::HoverTimings,

    /// The armed focus-follows-mouse dwell deadline, or `None` when no dwell is
    /// armed. The main loop folds this into its wait-timeout `min`-reduce and
    /// fires `maybe_fire_focus_dwell` when it arrives. Set by `ArmDwell`, cleared
    /// by `CancelDwell` and after the dwell fires. (`docs/src/dev-guide/hover.md`)
    pub(super) focus_dwell_deadline: Option<std::time::Instant>,

    /// The armed edge-hover-scroll dwell deadline, or `None` when none is armed.
    /// The main loop folds this into its wait-timeout `min`-reduce and fires
    /// [`FlowWM::maybe_fire_edge_dwell`]
    /// when it arrives. Set by `ArmEdgeDwell`, cleared by `CancelEdgeDwell`,
    /// after it fires, and when a tile drag starts (the hover subsystem is
    /// suppressed during a drag). (`docs/src/dev-guide/hover.md`)
    pub(super) edge_dwell_deadline: Option<std::time::Instant>,

    /// Whether the active workspace's floats are currently pinned
    /// `WS_EX_TOPMOST` (the last state the toggle applied).
    ///
    /// Drives the no-churn short-circuit in
    /// [`reconcile_float_topmost`](super::FlowWM::reconcile_float_topmost):
    /// the toggle only calls `SetWindowPos` when the desired TOPMOST state
    /// differs from this value, preserving the floats' mutual z-order on
    /// unchanged foreground changes. Re-evaluated on every foreground change
    /// in the focus sink. (`docs/src/dev-guide/floating-space.md`)
    pub(super) floats_topmost: bool,

    /// Timestamp of the most recent hover cursor poll. `None` until the first
    /// poll. Throttles the poll to `config.hover.poll_interval_ms` (the loop can
    /// wake far more often on hook activity) and anchors the hover poll deadline
    /// so both agree on the cadence. (`docs/src/dev-guide/hover.md`)
    pub(super) last_hover_poll: Option<std::time::Instant>,

    /// Timestamp of the most recent foreground-reconciliation pass.
    ///
    /// The main loop folds `last_foreground_sync + foreground_sync_interval_ms`
    /// into its `MsgWaitForMultipleObjects` timeout so it never sleeps past the
    /// next reconciliation deadline. On each wake,
    /// [`reconcile_foreground`](super::FlowWM::reconcile_foreground) stamps this
    /// field to `now` and re-checks `GetForegroundWindow()` against the tracked
    /// focus — closing the gap when `EVENT_SYSTEM_FOREGROUND` is dropped under
    /// rapid window churn. (`docs/src/dev-guide/event-pipelines.md`)
    pub(super) last_foreground_sync: std::time::Instant,
}

impl FlowWM {
    /// The index of the monitor that currently owns keyboard focus.
    #[must_use]
    #[allow(dead_code)] // API surface for the upcoming multi-monitor / workspace commands.
    pub(super) fn active_monitor_index(&self) -> usize {
        self.active_monitor
    }

    /// Borrow the active monitor (the one that owns keyboard focus).
    ///
    /// # Panics
    ///
    /// Panics if `monitors` is empty. The daemon always keeps at least one
    /// monitor, so this never fires in practice.
    #[must_use]
    pub(super) fn active_monitor(&self) -> &Monitor {
        &self.monitors[self.active_monitor]
    }

    /// Mutably borrow the active monitor.
    ///
    /// # Panics
    ///
    /// Panics if `monitors` is empty — see [`active_monitor`](Self::active_monitor).
    #[must_use]
    pub(super) fn active_monitor_mut(&mut self) -> &mut Monitor {
        &mut self.monitors[self.active_monitor]
    }

    /// Borrow the active workspace (the one currently on screen) of the
    /// active monitor.
    ///
    /// This is the workspace every IPC command and hook event operates on
    /// by default. Workspace-switching commands (`switch-workspace`,
    /// `move-to-workspace`) reassign the active index before animating.
    #[must_use]
    pub(super) fn active_workspace(&self) -> &Workspace {
        self.active_monitor().active_workspace()
    }

    /// Mutably borrow the active workspace of the active monitor.
    #[must_use]
    pub(super) fn active_workspace_mut(&mut self) -> &mut Workspace {
        self.active_monitor_mut().active_workspace_mut()
    }

    /// Borrow the scrolling space of the active workspace.
    ///
    /// This is the primary accessor the daemon's hook and dispatch handlers
    /// use to reach the (formerly `LayoutEngine`) tiling state. Every
    /// `self.layout.X(...)` call site was migrated to
    /// `self.active_scrolling[_mut]().X(...)` during the workspace refactor.
    #[must_use]
    pub(super) fn active_scrolling(&self) -> &ScrollingSpace {
        self.active_monitor().active_scrolling()
    }

    /// Mutably borrow the scrolling space of the active workspace.
    #[must_use]
    pub(super) fn active_scrolling_mut(&mut self) -> &mut ScrollingSpace {
        self.active_monitor_mut().active_scrolling_mut()
    }

    /// Borrow the floating space of the active workspace.
    #[must_use]
    pub(super) fn active_floating(&self) -> &FloatingSpace {
        self.active_monitor().active_floating()
    }

    /// Mutably borrow the floating space of the active workspace.
    #[must_use]
    #[allow(dead_code)] // API surface for upcoming floating-window management.
    pub(super) fn active_floating_mut(&mut self) -> &mut FloatingSpace {
        self.active_monitor_mut().active_floating_mut()
    }
}
