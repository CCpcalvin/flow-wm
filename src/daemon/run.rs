//! ScrollTilingManager main event loop and hook event routing.
//!
//! This module contains:
//!
//! - [`ScrollTilingManager::run`] — the event-driven main loop that uses
//!   `WaitForMultipleObjects` to wait on both hook events and IPC client
//!   connections simultaneously.
//! - [`ScrollTilingManager::process_hook_events`] — drains pending hook
//!   events from the channel and routes them to individual handlers.
//!
//! # Event-Driven Architecture
//!
//! The main loop blocks on `WaitForMultipleObjects` with two wait handles:
//!
//! 1. **Hook signal event** (index 0, highest priority) — signaled by the
//!    hook callback thread after each `sender.send(HookEvent)`.
//! 2. **Pipe connected event** (index 1) — signaled by the background accept
//!    thread when a CLI client connects via named pipe.
//!
//! When either event fires, the loop:
//! 1. Resets the hook signal (`ResetEvent`) — **before** draining, to close
//!    the race window (see below).
//! 2. Drains ALL pending hook events via `try_recv()` loop.
//! 3. If the pipe event was signaled, processes the IPC session (read,
//!    dispatch, write, disconnect, re-accept).
//! 4. Returns to `WaitForMultipleObjects`.
//!
//! # Race-Free Hook Drain
//!
//! The critical ordering is **ResetEvent before drain**:
//!
//! 1. `WaitForMultipleObjects` wakes (hook signal is set).
//! 2. `ResetEvent(hook_signal)` — clear the signal.
//! 3. `try_recv()` loop — drain ALL events from the channel.
//!
//! Any event pushed between ResetEvent and try_recv is caught by try_recv.
//! Any event pushed after the try_recv loop calls `SetEvent`, so the next
//! `WaitForMultipleObjects` wakes immediately. **No events are lost.**
//!
//! # Pending-Creations Retry
//!
//! `EVENT_OBJECT_CREATE` fires early in the Win32 window lifecycle — before
//! `ShowWindow`, `SetWindowText`, or style finalization. With the event-driven
//! loop processing hook events within microseconds, the classification checks
//! in [`handle_created`](crate::registry::WindowRegistry::handle_created)
//! (`is_window_visible`, title non-empty, `is_alt_tab_visible`) fail because
//! the window isn't fully initialized yet.
//!
//! To handle this, windows that fail classification are added to
//! [`pending_creations`](super::types::ScrollTilingManager::pending_creations).
//! On every `process_hook_events` call, pending windows are retried. When the
//! list is non-empty, `WaitForMultipleObjects` uses a finite timeout (100 ms)
//! instead of `INFINITE`, ensuring retries happen even without new hook events.
//!
//! This approach is event-driven by default (zero CPU while idle) with a
//! bounded timer fallback only when windows are pending classification.

use windows::Win32::System::Threading::{ResetEvent, WaitForMultipleObjects};

use crate::registry::HookEvent;

use super::types::ScrollTilingManager;

/// Maximum retry attempts for a pending window creation.
///
/// Each retry occurs on the next `process_hook_events` call — either triggered
/// by a new hook event (near-instant) or by the 100 ms timeout fallback.
/// After this many failures, the window is dropped (likely a tooltip, splash
/// screen, or other non-tiling window).
const MAX_PENDING_RETRIES: u8 = 5;

/// Timeout (milliseconds) for `WaitForMultipleObjects` when there are pending
/// window creations to retry.
///
/// When `pending_creations` is empty, `u32::MAX` (`INFINITE`) is used instead.
/// 100 ms is fast enough for interactive use but slow enough that the window
/// has time to finish initializing (set title, become visible, finalize styles).
const PENDING_RETRY_TIMEOUT_MS: u32 = 100;

impl ScrollTilingManager {
    /// Run the event-driven main loop. Blocks until the `Stop` command is
    /// received or a fatal error occurs.
    ///
    /// # Loop Structure
    ///
    /// ```text
    /// start_accept()                          // Spawn background accept thread
    /// loop {
    ///     timeout = pending_creations.is_empty() ? INFINITE : 100ms
    ///     WaitForMultipleObjects(             // Sleep until something happens
    ///         [hook_signal, connected_event],
    ///         timeout
    ///     )
    ///     ResetEvent(hook_signal)             // Race-free: reset BEFORE drain
    ///     process_hook_events()               // Drain hooks + retry pending
    ///
    ///     if connected_event was signaled:
    ///         loop {                           // IPC session with this client
    ///             process_hook_events()
    ///             read_message()              // Blocking (client is active)
    ///             dispatch(msg)
    ///             write_response(response)
    ///             if Stop: return
    ///         }
    ///         disconnect()
    ///         start_accept()                  // Spawn next accept thread
    /// }
    /// ```
    ///
    /// # Why This Works
    ///
    /// The previous implementation blocked the main thread in
    /// `ConnectNamedPipe`, preventing hook events from being processed until
    /// a CLI client connected. By moving `ConnectNamedPipe` to a background
    /// thread and using `WaitForMultipleObjects`, hook events are now
    /// processed immediately — window creation/removal/focus changes reflect
    /// in the layout without requiring an IPC dispatch.
    ///
    /// The inner loop (blocking `read_message`) is acceptable because the CLI
    /// is one-shot: connect → send one command → read response → disconnect.
    /// The entire IPC transaction completes in under 1 ms.
    pub fn run(&mut self) {
        log::info!("stmd: daemon started, event-driven loop on named pipe");

        // Start the first background accept — spawns a thread that blocks in
        // ConnectNamedPipe until a client connects.
        self.server.start_accept();

        // Event handles for WaitForMultipleObjects.
        // Index 0 = hook_signal (highest priority — hooks drained first)
        // Index 1 = connected_event (IPC client connected)
        let hook_handle = self.hook_signal.raw();
        let connect_handle = self.server.connected_event_handle();
        let wait_handles = [hook_handle, connect_handle];

        loop {
            // Block until either a hook event or an IPC client connection
            // arrives. When there are pending window creations, use a finite
            // timeout so they're retried even without new hook events.
            //
            // WaitForMultipleObjects returns WAIT_EVENT whose inner u32 is:
            //   0     = WAIT_OBJECT_0 (first handle = hook_signal)
            //   1     = WAIT_OBJECT_0 + 1 (second handle = connected_event)
            //   258   = WAIT_TIMEOUT (retry pending creations)
            //   u32::MAX = WAIT_FAILED (fatal — break)
            let timeout = if self.pending_creations.is_empty() {
                u32::MAX // INFINITE
            } else {
                PENDING_RETRY_TIMEOUT_MS
            };

            let wait_result = unsafe { WaitForMultipleObjects(&wait_handles, false, timeout) };

            // Check for WAIT_FAILED (0xFFFFFFFF).
            if wait_result.0 == u32::MAX {
                log::error!("stmd: WaitForMultipleObjects failed");
                break;
            }

            // The raw wait code — index of the first signaled handle, or
            // WAIT_TIMEOUT (258) if the timeout expired.
            let signaled = wait_result.0;

            // Always reset hook signal FIRST (race-free pattern), then drain.
            // Any event pushed between ResetEvent and try_recv is caught by
            // try_recv. Any event pushed after the drain calls SetEvent, so
            // the next WaitForMultipleObjects wakes immediately.
            // On WAIT_TIMEOUT this is a harmless no-op (event is unsignaled).
            unsafe {
                let _ = ResetEvent(hook_handle);
            }

            // Drain ALL pending hook events and retry pending creations.
            // This runs on every wake — hook signal, IPC connect, or timeout.
            self.process_hook_events();

            // Check if an IPC client connected (index 1 = WAIT_OBJECT_0 + 1).
            // On WAIT_TIMEOUT (258) or hook-only wake (0), this is false and
            // we skip the IPC inner loop.
            if signaled == 1 {
                // Inner loop: handle this client's messages.
                // The CLI is one-shot (connect → send → read response →
                // disconnect), so this loop typically runs once before the
                // client disconnects. Hook events are drained between reads.
                loop {
                    // Drain hook events before each IPC message.
                    self.process_hook_events();

                    // Read next IPC message (blocking — but client is active).
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
                        Err(_) => {
                            // Client disconnected or read error — not fatal.
                            break;
                        }
                    }
                }

                // Disconnect the client so a new one can connect.
                if let Err(e) = self.server.disconnect() {
                    log::warn!("stmd: failed to disconnect client: {e}");
                }

                // Start accepting the next client on a background thread.
                self.server.start_accept();
            }
        }
    }

    /// Drain all pending hook events from the channel, retry pending window
    /// creations, and route events to the appropriate subsystem handlers.
    ///
    /// # Processing Order
    ///
    /// 1. **Retry pending creations** — windows whose `Created` event fired
    ///    before they were fully initialized. Each gets another chance to
    ///    pass classification. Successful windows are inserted into the
    ///    layout. Failed windows remain pending (up to `MAX_PENDING_RETRIES`).
    /// 2. **Drain new hook events** — process each event from the mpsc channel:
    ///    - `Created`: classify the window. If classification fails, add to
    ///      `pending_creations` for retry.
    ///    - `Destroyed`, `Foreground`, `MinimizeStart`, `MinimizeEnd`: route
    ///      to the corresponding handler.
    ///
    /// This method is called:
    /// - After `WaitForMultipleObjects` wakes (before checking IPC)
    /// - Inside the IPC inner loop (before each `read_message`)
    ///
    /// This ensures hook events are processed promptly without blocking on IPC.
    fn process_hook_events(&mut self) {
        // 1. Retry pending window creations.
        //    handle_created re-checks visibility, title, styles — all of
        //    which may have changed since the initial Created event.
        if !self.pending_creations.is_empty() {
            // Move the Vec out to avoid borrowing self while iterating.
            let pending = std::mem::take(&mut self.pending_creations);
            let mut still_pending = Vec::new();
            for (hwnd, retries) in pending {
                if self.on_window_created(hwnd) {
                    // Classification succeeded — window is now in the
                    // registry (tiling/floating/ignored) and, if tiling,
                    // inserted into the layout.
                } else if retries < MAX_PENDING_RETRIES {
                    still_pending.push((hwnd, retries + 1));
                }
                // retries >= MAX_PENDING_RETRIES: give up silently.
                // The window is likely a non-tiling helper window (tooltip,
                // splash screen, etc.) that never passes classification.
            }
            self.pending_creations = still_pending;
        }

        // 2. Drain new hook events from the channel.
        while let Ok(event) = self.hook_receiver.try_recv() {
            match event {
                HookEvent::Created { hwnd } => {
                    // Try to classify immediately. EVENT_OBJECT_CREATE fires
                    // early in the lifecycle — the window may not be visible
                    // or have its title set yet. If classification fails,
                    // defer to pending_creations for retry.
                    if !self.on_window_created(hwnd) {
                        self.pending_creations.push((hwnd, 0));
                    }
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
                HookEvent::Shown { hwnd } => {
                    self.on_window_shown(hwnd);
                }
                HookEvent::Hidden { hwnd } => {
                    self.on_window_hidden(hwnd);
                }
            }
        }
    }
}
