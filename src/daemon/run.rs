//! ScrollTilingManager main event loop and hook event routing.
//!
//! This module contains:
//!
//! - [`ScrollTilingManager::run`] — the main IPC event loop that blocks
//!   until the daemon is stopped.
//! - [`ScrollTilingManager::process_hook_events`] — drains pending hook
//!   events from the channel and routes them to individual handlers.

use crate::registry::HookEvent;

use super::types::ScrollTilingManager;

impl ScrollTilingManager {
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

    /// Drain all pending hook events from the channel and route them to
    /// the appropriate subsystem handlers.
    ///
    /// Each event follows a pipeline:
    ///
    /// ```text
    /// HookEvent → registry mutation → layout engine update → animate_layout
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
}
