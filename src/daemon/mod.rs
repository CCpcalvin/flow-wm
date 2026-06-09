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
//! │  │ WindowRegistry │  │ LayoutEngine │  │ WindowAnimator           │  │
//! │  │ (window state) │  │ (layout math)│  │ (src/animation/)         │  │
//! │  └────────────────┘  └──────────────┘  └──────────────────────────┘  │
//! │  ┌────────────────┐  ┌──────────────┐  ┌──────────────────────────┐  │
//! │  │ PipeServer     │  │ AppConfig    │  │ FloatingManager (stub)   │  │
//! │  │ (IPC transport)│  │ (loaded once)│  │ (placeholder)            │  │
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
//!     → registry.handle_created() → layout.add_window() → animate_diff()
//! ```
//!
//! ## IPC Commands
//!
//! ```text
//! stm CLI → SocketMessage → PipeServer → dispatch() → layout.swap_column()
//!     → animate_diff() → SocketResponse
//! ```

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crate::animation::backend::win32::Win32Backend;
use crate::animation::{AnimatorConfig, IVec2, WindowAnimator, WindowRef, WindowTarget};
use crate::common::{Direction, WindowId};
use crate::config::types::{StmConfig, WindowRulesConfig};
use crate::floating::FloatingManager;
use crate::ipc::message::{SocketMessage, SocketResponse};
use crate::ipc::transport::PipeServer;
use crate::layout::engine::LayoutEngine;
use crate::layout::types::{LayoutDiff, MonitorInfo, Padding as LayoutPadding};
use crate::registry::hooks::HookThreadHandle;
use crate::registry::{HookEvent, WindowRegistry, hooks, win32 as registry_win32};

// ---------------------------------------------------------------------------
// LayoutConfig helper — derived from StmConfig for LayoutEngine::new()
// ---------------------------------------------------------------------------

/// Intermediate struct holding layout engine parameters derived from
/// [`StmConfig`]. Used during construction to keep the parameter list
/// readable and avoid recomputing values.
struct LayoutConfig {
    /// Default column width in pixels (from `StmConfig::column_width`).
    column_width: u32,
    /// Default column width in eighths of the monitor (computed as 4 for
    /// a 960px column on a 1920px monitor).
    default_column_width_eighths: u8,
    /// Minimum column width in pixels.
    min_column_width_px: u32,
    /// Padding converted from config types to layout types.
    padding: LayoutPadding,
}

// ---------------------------------------------------------------------------
// ScrollTilingManager — the single orchestrator
// ---------------------------------------------------------------------------

/// The single orchestrator for the ScrollingTilingManager daemon.
///
/// Owns all subsystems and routes events between them. Lives entirely on
/// the IPC thread — no interior mutability (`Arc<Mutex<>>`) is needed.
///
/// See the [module-level documentation](self) for the architecture overview
/// and event pipeline descriptions.
pub struct ScrollTilingManager {
    /// Window registry — authoritative source of truth for all tracked windows.
    registry: WindowRegistry,

    /// Layout engine — pure layout math, no Win32 knowledge.
    layout: LayoutEngine,

    /// Window animator — background-threaded rect animation for smooth moves.
    animator: WindowAnimator,

    /// Floating window manager — stub for future implementation.
    #[allow(dead_code)] // Stored for future floating window management.
    floating: FloatingManager,

    /// IPC named pipe server — accepts commands from the `stm` CLI.
    server: PipeServer,

    /// Application configuration loaded from `stm.yml`.
    config: StmConfig,

    /// Path to the configuration directory (for future reload support).
    #[allow(dead_code)] // Stored for future config reload functionality.
    config_dir: PathBuf,

    /// Receiver for hook events from the background WinEvent hook thread.
    hook_receiver: Receiver<HookEvent>,

    /// Handle to the background hook thread. Kept for its `Drop` impl
    /// which posts `WM_QUIT` to the hook thread's message loop.
    _hook_handle: HookThreadHandle,

    /// Set to `true` when the `Stop` IPC command is received, causing
    /// the main event loop to exit on the next iteration.
    shutting_down: bool,
}

impl ScrollTilingManager {
    /// Construct and initialize the daemon.
    ///
    /// Performs all startup work in sequence:
    ///
    /// 1. Create [`WindowRegistry`] from user and default rules.
    /// 2. Scan existing windows (populates registry before hooks start).
    /// 3. Query monitor work area via Win32.
    /// 4. Derive layout parameters from [`StmConfig`].
    /// 5. Create [`LayoutEngine`] with those parameters.
    /// 6. Batch-initialize layout from existing tiling windows.
    /// 7. Create [`WindowAnimator`] with Win32 backend and zero-duration
    ///    config (for instant snap of initial positions).
    /// 8. Animate the initial layout diff (instant, no visible animation).
    /// 9. Start the WinEvent hook thread.
    /// 10. Create the IPC named pipe server.
    /// 11. Return the fully initialized STM ready for [`run()`](Self::run).
    ///
    /// # Arguments
    ///
    /// * `app_config` - Application settings loaded from `stm.yml`.
    /// * `user_rules` - User-defined window rules from `stm-rules.yml`.
    /// * `default_rules` - Bundled default rules.
    /// * `config_dir` - Path to the configuration directory.
    /// * `desktop_name` - Optional test desktop name (debug builds only).
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` if any startup step fails:
    /// - Window scan failure (non-fatal — logged, returns `Ok`).
    /// - Monitor work area query failure.
    /// - Hook thread start failure.
    /// - Named pipe creation failure (likely another daemon running).
    pub fn new(
        app_config: StmConfig,
        user_rules: WindowRulesConfig,
        default_rules: WindowRulesConfig,
        config_dir: PathBuf,
        desktop_name: Option<String>,
    ) -> Result<Self, String> {
        // 1. Create registry from rules.
        let mut registry = WindowRegistry::new(&user_rules, &default_rules);

        // 2. Scan existing windows before hooks start.
        registry.scan_existing_windows()?;

        // 3. Get monitor work area via Win32.
        let monitor = MonitorInfo {
            work_area: registry_win32::get_primary_monitor_work_area()?,
        };

        // 4. Derive layout parameters from StmConfig.
        let layout_config = Self::derive_layout_config(&app_config, &monitor);

        // 5. Create layout engine.
        let mut layout = LayoutEngine::new(
            monitor,
            layout_config.column_width,
            layout_config.default_column_width_eighths,
            layout_config.min_column_width_px,
            layout_config.padding,
        );

        // 6. Batch-initialize layout from existing tiling windows.
        let tiling_ids = registry.tiling_window_ids();
        let initial_diff = if !tiling_ids.is_empty() {
            let diff = layout.initialize_windows(tiling_ids);
            Some(diff)
        } else {
            None
        };

        // 7. Create animator with zero-duration config for instant initial snap.
        //    Duration::ZERO means "complete immediately" — windows snap to their
        //    starting positions without visible animation.
        let backend = Win32Backend::new();
        let snap_config = AnimatorConfig {
            duration: Duration::ZERO,
            ..AnimatorConfig::default()
        };
        let mut animator = WindowAnimator::new(backend, snap_config);

        // 8. Animate initial layout (instant snap — zero duration).
        if let Some(diff) = initial_diff {
            // Use a standalone function to avoid borrow checker issues
            // — animate_diff takes &mut self, but we don't have Self yet.
            animate_diff_raw(&mut animator, &diff);
        }

        // 9. Switch to the user-configured animation duration for runtime.
        //    After the initial snap, all subsequent window moves should animate
        //    at the speed the user configured (or stay instant if disabled).
        let runtime_config = Self::derive_animator_config(&app_config, Duration::ZERO);
        animator.update_config(runtime_config);

        // 10. Start hook thread.
        let (hook_receiver, _hook_handle) = hooks::start_hook_thread(desktop_name)?;

        // 11. Create IPC server.
        let server = PipeServer::create()
            .map_err(|e| format!("failed to create pipe (is another daemon running?): {e}"))?;

        log::info!("stmd: daemon initialized successfully");

        Ok(Self {
            registry,
            layout,
            animator,
            floating: FloatingManager::new(),
            server,
            config: app_config,
            config_dir,
            hook_receiver,
            _hook_handle,
            shutting_down: false,
        })
    }

    // -----------------------------------------------------------------------
    // Main event loop
    // -----------------------------------------------------------------------

    /// Run the main event loop. Blocks until the `Stop` command is received
    /// or a fatal error occurs.
    ///
    /// The loop structure:
    ///
    /// ```text
    /// loop {
    ///     wait_for_client()              // Block until a CLI connects
    ///     loop {
    ///         process_hook_events()      // Drain all pending hook events
    ///         msg = read_message()       // Block for next IPC command
    ///         response = dispatch(msg)   // Route to subsystems
    ///         write_response(response)   // Send response back
    ///         if shutting_down: return   // Exit on Stop command
    ///     }
    ///     disconnect()                   // Clean up client connection
    /// }
    /// ```
    pub fn run(&mut self) {
        log::info!("stmd: daemon started, listening on named pipe");

        loop {
            // Wait for a client connection.
            if let Err(e) = self.server.wait_for_client() {
                log::error!("stmd: failed to accept client: {e}");
                break;
            }

            log::debug!("stmd: client connected");

            // Process messages from this client until they disconnect or send Stop.
            loop {
                // 1. Drain hook events BEFORE each IPC message.
                self.process_hook_events();

                // 2. Read next IPC message (blocking).
                match self.server.read_message() {
                    Ok(msg) => {
                        let response = self.dispatch(&msg);
                        let is_stop = self.shutting_down;

                        if let Err(e) = self.server.write_response(&response) {
                            log::warn!("stmd: failed to write response: {e}");
                            break;
                        }

                        if is_stop {
                            log::info!("stmd: shutting down");
                            return;
                        }
                    }
                    Err(e) => {
                        // Client disconnected or read error — not fatal for the daemon.
                        log::debug!("stmd: client read error: {e}");
                        break;
                    }
                }
            }

            // Disconnect the client so a new one can connect.
            if let Err(e) = self.server.disconnect() {
                log::warn!("stmd: failed to disconnect client: {e}");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Hook event processing
    // -----------------------------------------------------------------------

    /// Drain all pending hook events from the channel and route them to
    /// the appropriate subsystem handlers.
    ///
    /// Each event follows a pipeline:
    ///
    /// ```text
    /// HookEvent → registry mutation → layout engine update → animate_diff
    /// ```
    ///
    /// This method is called before each IPC message read in the main loop,
    /// ensuring hook events are processed promptly without blocking on IPC.
    fn process_hook_events(&mut self) {
        while let Ok(event) = self.hook_receiver.try_recv() {
            match event {
                HookEvent::Created { hwnd } => {
                    self.on_window_created(hwnd);
                }
                HookEvent::Destroyed { hwnd } => {
                    self.on_window_destroyed(hwnd);
                }
                HookEvent::Foreground { hwnd } => {
                    self.on_focus_changed(hwnd);
                }
                HookEvent::MinimizeStart { hwnd } => {
                    self.on_window_minimized(hwnd);
                }
                HookEvent::MinimizeEnd { hwnd } => {
                    self.on_window_restored(hwnd);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Individual hook event handlers
    // -----------------------------------------------------------------------

    /// Handle a window creation event.
    ///
    /// Pipeline:
    /// 1. `registry.handle_created(hwnd)` — classifies and registers the window.
    /// 2. If the window was classified as tiling (`Some(WindowId)`):
    ///    - `layout.add_window(id)` — adds it as a new column.
    ///    - `animate_diff(diff)` — animates the resulting layout change.
    /// 3. If the window was floating, ignored, or skipped: no action needed.
    fn on_window_created(&mut self, hwnd: isize) {
        if let Some(window_id) = self.registry.handle_created(hwnd) {
            let diff = self.layout.add_window(window_id);
            self.animate_diff(&diff);
        }
    }

    /// Handle a window destruction event.
    ///
    /// Pipeline:
    /// 1. Check if the window was in tiling state **before** removal.
    /// 2. If tiling: `layout.remove_window(id)` → `animate_diff(diff)`.
    /// 3. `registry.remove_window(hwnd)` — always, regardless of state.
    ///
    /// The tiling check happens before removal because `remove_window`
    /// deletes the entry from the registry.
    fn on_window_destroyed(&mut self, hwnd: isize) {
        let was_tiling = self.registry.is_tiling(hwnd);

        if was_tiling {
            let diff = self.layout.remove_window(WindowId(hwnd));
            self.animate_diff(&diff);
        }

        self.registry.remove_window(hwnd);
    }

    /// Handle a window minimize event.
    ///
    /// Pipeline:
    /// 1. `registry.minimize_window(hwnd)` — updates state to `Tiling::Minimized`.
    /// 2. If the window was tiling-active (before minimize):
    ///    - `layout.remove_window(id)` — removes from layout.
    ///    - `animate_diff(diff)` — animates remaining windows filling the gap.
    fn on_window_minimized(&mut self, hwnd: isize) {
        let was_tiling = self.registry.is_tiling(hwnd);
        self.registry.minimize_window(hwnd);

        if was_tiling {
            let diff = self.layout.remove_window(WindowId(hwnd));
            self.animate_diff(&diff);
        }
    }

    /// Handle a window restore (un-minimize) event.
    ///
    /// Pipeline:
    /// 1. `registry.restore_window(hwnd)` — updates state back to `Tiling::Active`.
    /// 2. If the window is now tiling-active (after restore):
    ///    - `layout.add_window(id)` — re-adds to layout.
    ///    - `animate_diff(diff)` — animates the new window appearing.
    fn on_window_restored(&mut self, hwnd: isize) {
        self.registry.restore_window(hwnd);

        // After restore, check if the window is now tiling-active.
        if self.registry.is_tiling(hwnd) {
            let diff = self.layout.add_window(WindowId(hwnd));
            self.animate_diff(&diff);
        }
    }

    /// Handle a focus change event.
    ///
    /// Pipeline:
    /// 1. `registry.set_focused(hwnd)` — updates focused window in registry.
    /// 2. If the focused window is tiling:
    ///    - `layout.set_focus(id)` — updates layout focus state.
    ///
    /// Note: `set_focus` does not produce a [`LayoutDiff`] — it only updates
    /// internal focus tracking. The next layout mutation will use the correct
    /// focus.
    fn on_focus_changed(&mut self, hwnd: isize) {
        self.registry.set_focused(hwnd);

        if self.registry.is_tiling(hwnd) {
            self.layout.set_focus(WindowId(hwnd));
        }
    }

    // -----------------------------------------------------------------------
    // IPC command dispatch
    // -----------------------------------------------------------------------

    /// Dispatch a single IPC command and return the response.
    ///
    /// Routes each [`SocketMessage`] variant to the appropriate subsystem:
    /// - **Stop**: sets the shutdown flag.
    /// - **Layout commands**: call layout engine methods and animate the result.
    /// - **Query commands**: return registry data as JSON.
    /// - **Unimplemented commands**: return an error response.
    fn dispatch(&mut self, msg: &SocketMessage) -> SocketResponse {
        match msg {
            // --- Shutdown ---
            SocketMessage::Stop => {
                self.shutting_down = true;
                SocketResponse::Ok
            }

            // --- Focus ---
            SocketMessage::FocusLeft => self.dispatch_focus(Direction::Left),
            SocketMessage::FocusRight => self.dispatch_focus(Direction::Right),
            SocketMessage::FocusUp => self.dispatch_focus(Direction::Up),
            SocketMessage::FocusDown => self.dispatch_focus(Direction::Down),

            // --- Swap (column-level) ---
            SocketMessage::SwapLeft => self.dispatch_swap(Direction::Left),
            SocketMessage::SwapRight => self.dispatch_swap(Direction::Right),
            SocketMessage::SwapUp => self.dispatch_swap(Direction::Up),
            SocketMessage::SwapDown => self.dispatch_swap(Direction::Down),

            // --- Swap with offscreen ---
            SocketMessage::SwapWithOffscreen { direction } => {
                self.dispatch_swap_with_offscreen(*direction)
            }

            // --- Scroll ---
            SocketMessage::ScrollLeft => self.dispatch_scroll_left(),
            SocketMessage::ScrollRight => self.dispatch_scroll_right(),

            // --- Column resize ---
            SocketMessage::ExpandColumn => self.dispatch_expand(),
            SocketMessage::ShrinkColumn => self.dispatch_shrink(),
            SocketMessage::SetColumnWidth { eighths } => self.dispatch_set_column_width(*eighths),

            // --- Window state ---
            SocketMessage::ToggleFloat => unimplemented_command("toggle_float"),
            SocketMessage::ToggleMonocle => self.dispatch_toggle_monocle(),
            SocketMessage::PlaceAbove => unimplemented_command("place_above"),
            SocketMessage::Promote => unimplemented_command("promote"),
            SocketMessage::CloseWindow => unimplemented_command("close_window"),

            // --- Queries ---
            SocketMessage::QueryWindowsAll => SocketResponse::Data {
                payload: self.registry.to_json_value(),
            },
            SocketMessage::QueryLayoutVirtual => unimplemented_command("query_layout_virtual"),
            SocketMessage::QueryLayoutActual => unimplemented_command("query_layout_actual"),
            SocketMessage::QueryState => unimplemented_command("query_state"),

            // --- Config mutation ---
            SocketMessage::ReloadConfig => unimplemented_command("reload_config"),
            SocketMessage::CheckConfig => unimplemented_command("check_config"),
            SocketMessage::SetConfigValue { .. } => unimplemented_command("set_config_value"),
            SocketMessage::ForgetApp { .. } => unimplemented_command("forget_app"),
            SocketMessage::ForgetAllApps => unimplemented_command("forget_all_apps"),
        }
    }

    // -----------------------------------------------------------------------
    // Dispatch helper methods (one per command category)
    // -----------------------------------------------------------------------

    /// Dispatch a focus movement in the given direction.
    ///
    /// Calls [`LayoutEngine::focus`] which returns the newly focused
    /// [`WindowId`] (or `None` if no movement was possible).
    fn dispatch_focus(&mut self, dir: Direction) -> SocketResponse {
        match self.layout.focus(dir) {
            Some(_focused) => SocketResponse::Ok,
            None => SocketResponse::Error {
                message: "no window to focus in that direction".into(),
            },
        }
    }

    /// Dispatch a column swap in the given direction.
    ///
    /// Calls [`LayoutEngine::swap_column`] and animates the resulting
    /// layout diff if the swap succeeded.
    fn dispatch_swap(&mut self, dir: Direction) -> SocketResponse {
        match self.layout.swap_column(dir) {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot swap in that direction".into(),
            },
        }
    }

    /// Dispatch a swap with an offscreen column.
    ///
    /// This command swaps the focused column with the nearest offscreen
    /// column in the given direction. Currently delegates to
    /// [`dispatch_swap`] since the layout engine handles offscreen
    /// swapping transparently via viewport scrolling.
    fn dispatch_swap_with_offscreen(&mut self, direction: Direction) -> SocketResponse {
        // The layout engine's swap_column already handles viewport scrolling
        // when the target is offscreen. This is a thin wrapper for the IPC.
        self.dispatch_swap(direction)
    }

    /// Dispatch a scroll-left command.
    fn dispatch_scroll_left(&mut self) -> SocketResponse {
        match self.layout.scroll_left() {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot scroll left".into(),
            },
        }
    }

    /// Dispatch a scroll-right command.
    fn dispatch_scroll_right(&mut self) -> SocketResponse {
        match self.layout.scroll_right() {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot scroll right".into(),
            },
        }
    }

    /// Dispatch an expand-column command on the focused column.
    fn dispatch_expand(&mut self) -> SocketResponse {
        match self.layout.expand_column() {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot expand column".into(),
            },
        }
    }

    /// Dispatch a shrink-column command on the focused column.
    fn dispatch_shrink(&mut self) -> SocketResponse {
        match self.layout.shrink_column() {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot shrink column".into(),
            },
        }
    }

    /// Dispatch an explicit column width setting.
    ///
    /// Converts the `eighths` value (1–8) to pixel width based on
    /// the configured `column_width` and passes it to the layout engine.
    fn dispatch_set_column_width(&mut self, eighths: u8) -> SocketResponse {
        if !(1..=8).contains(&eighths) {
            return SocketResponse::Error {
                message: format!("eighths must be 1–8, got {eighths}"),
            };
        }
        let target_px = self.config.column_width as i32 * eighths as i32 / 4;
        match self.layout.set_column_width(target_px) {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "no focused window".into(),
            },
        }
    }

    /// Dispatch a monocle mode toggle on the focused column.
    fn dispatch_toggle_monocle(&mut self) -> SocketResponse {
        match self.layout.toggle_monocle() {
            Some(diff) => {
                self.animate_diff(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot toggle monocle".into(),
            },
        }
    }

    // -----------------------------------------------------------------------
    // Animation bridge
    // -----------------------------------------------------------------------

    /// Convert a [`LayoutDiff`] into animation targets and submit to the animator.
    ///
    /// This is the critical conversion point between the layout engine's output
    /// (STM types) and the animation system's input (animation types):
    ///
    /// | STM Type | Animation Type |
    /// |----------|---------------|
    /// | `WindowId(isize)` | `WindowRef(isize)` |
    /// | `Rect { x, y, width, height }` position | `IVec2::new(x, y)` |
    /// | `Rect { x, y, width, height }` size | `IVec2::new(width, height)` |
    ///
    /// If the diff contains no moves, this is a no-op. Animation errors are
    /// logged as warnings but not propagated — a jarring animation is better
    /// than a crash.
    fn animate_diff(&mut self, diff: &LayoutDiff) {
        if diff.moves.is_empty() {
            return;
        }

        let targets: Vec<WindowTarget> = diff
            .moves
            .iter()
            .map(|wm| {
                WindowTarget::new(
                    WindowRef(wm.window_id.0),
                    IVec2::new(wm.to.x, wm.to.y),
                    IVec2::new(wm.to.width, wm.to.height),
                )
            })
            .collect();

        if let Err(e) = self.animator.animate(targets) {
            log::warn!("animation error: {e}");
        }
    }

    // -----------------------------------------------------------------------
    // Config derivation helpers
    // -----------------------------------------------------------------------

    /// Derive layout engine parameters from [`StmConfig`].
    ///
    /// Converts the user-facing config types (from `stm.yml`) into the
    /// layout-engine-specific types needed by [`LayoutEngine::new`].
    ///
    /// # Default Column Width Eighths
    ///
    /// Computed as 4 (meaning the column occupies 4/8 = half the monitor width).
    /// This matches the default `column_width` of 960px on a 1920px monitor.
    fn derive_layout_config(app_config: &StmConfig, _monitor: &MonitorInfo) -> LayoutConfig {
        LayoutConfig {
            column_width: app_config.column_width,
            default_column_width_eighths: 4,
            min_column_width_px: app_config.min_column_width_px,
            padding: LayoutPadding {
                window: app_config.padding.window,
                up: app_config.padding.up,
                down: app_config.padding.down,
            },
        }
    }

    /// Derive animator configuration from [`StmConfig`].
    ///
    /// The `override_duration` parameter allows the caller to force a specific
    /// animation duration. Pass `Duration::ZERO` to let the config decide
    /// (enabled → user-configured ms, disabled → zero/instant).
    fn derive_animator_config(
        app_config: &StmConfig,
        override_duration: Duration,
    ) -> AnimatorConfig {
        let duration = if override_duration != Duration::ZERO {
            override_duration
        } else if app_config.animation.enabled {
            Duration::from_millis(app_config.animation.duration_ms as u64)
        } else {
            Duration::ZERO
        };

        AnimatorConfig {
            duration,
            ..AnimatorConfig::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::AnimationConfig;

    /// Build a default [`StmConfig`] with animation enabled and a given duration.
    fn make_enabled_config(duration_ms: u32) -> StmConfig {
        let mut cfg = StmConfig::default();
        cfg.animation = AnimationConfig {
            enabled: true,
            duration_ms,
            ..AnimationConfig::default()
        };
        cfg
    }

    /// Build a [`StmConfig`] with animation disabled.
    fn make_disabled_config() -> StmConfig {
        let mut cfg = StmConfig::default();
        cfg.animation = AnimationConfig {
            enabled: false,
            ..AnimationConfig::default()
        };
        cfg
    }

    // W2-related: verify derive_animator_config with Duration::ZERO sentinel
    // (meaning "no override — use config defaults") respects the user's
    // enabled/disabled setting and returns the configured duration_ms when
    // animation is enabled.
    //
    // Note: the W2 fix does NOT call derive_animator_config for the snap —
    // it constructs AnimatorConfig { duration: Duration::ZERO, ..Default::default() }
    // directly. derive_animator_config is only used for the runtime config (W3).

    #[test]
    fn derive_animator_config_zero_sentinel_uses_user_settings() {
        // Positive: animation enabled + zero sentinel → user's configured duration.
        // This is the W3 case (runtime config after initial snap).
        let cfg = make_enabled_config(250);
        let result = ScrollTilingManager::derive_animator_config(&cfg, Duration::ZERO);
        assert_eq!(
            result.duration,
            Duration::from_millis(250),
            "Duration::ZERO sentinel with animation enabled should use user's 250ms"
        );

        // Positive: animation disabled + zero sentinel → zero duration.
        let cfg = make_disabled_config();
        let result = ScrollTilingManager::derive_animator_config(&cfg, Duration::ZERO);
        assert_eq!(
            result.duration,
            Duration::ZERO,
            "Duration::ZERO sentinel with animation disabled should produce zero duration"
        );
    }

    /// Negative: verify that a non-zero override takes precedence over user config.
    #[test]
    fn derive_animator_config_nonzero_override_overrides_user() {
        let cfg = make_enabled_config(250);
        let result = ScrollTilingManager::derive_animator_config(&cfg, Duration::from_millis(50));
        assert_eq!(
            result.duration,
            Duration::from_millis(50),
            "non-zero override should take precedence over user's 250ms"
        );

        // Also test with animation disabled — override still wins.
        let cfg = make_disabled_config();
        let result = ScrollTilingManager::derive_animator_config(&cfg, Duration::from_millis(100));
        assert_eq!(
            result.duration,
            Duration::from_millis(100),
            "non-zero override should take precedence even when animation is disabled"
        );
    }
}

// ---------------------------------------------------------------------------
// Standalone animation function (used during construction)
// ---------------------------------------------------------------------------

/// Convert a [`LayoutDiff`] into animation targets and submit to an animator.
///
/// This is a standalone version of [`ScrollTilingManager::animate_diff`] that
/// takes `&mut WindowAnimator` directly instead of `&mut ScrollTilingManager`.
/// Used during construction when `ScrollTilingManager` doesn't exist yet
/// but the animator needs to snap windows to their initial positions.
fn animate_diff_raw(animator: &mut WindowAnimator, diff: &LayoutDiff) {
    if diff.moves.is_empty() {
        return;
    }

    let targets: Vec<WindowTarget> = diff
        .moves
        .iter()
        .map(|wm| {
            WindowTarget::new(
                WindowRef(wm.window_id.0),
                IVec2::new(wm.to.x, wm.to.y),
                IVec2::new(wm.to.width, wm.to.height),
            )
        })
        .collect();

    if let Err(e) = animator.animate(targets) {
        log::warn!("animation error (initial snap): {e}");
    }
}

// ---------------------------------------------------------------------------
// Helper for unimplemented commands
// ---------------------------------------------------------------------------

/// Return a standard "not yet implemented" error response.
fn unimplemented_command(name: &str) -> SocketResponse {
    SocketResponse::Error {
        message: format!("command '{name}' is not yet implemented"),
    }
}
