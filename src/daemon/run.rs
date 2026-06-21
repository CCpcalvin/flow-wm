//! ScrollTilingManager main event loop and hook event routing.
//!
//! This module contains:
//!
//! - [`ScrollTilingManager::run`] — the event-driven main loop that uses
//!   `MsgWaitForMultipleObjects` to wait on hook events, IPC client
//!   connections, and the Win32 message queue simultaneously.
//! - [`ScrollTilingManager::process_hook_events`] — drains pending hook
//!   events from the channel and routes them to individual handlers.
//! - [`ScrollTilingManager::pump_messages`] — drains the Win32 message
//!   queue so cross-thread `SendMessage` calls from the border hook thread
//!   can complete. Required because the main thread creates overlay windows
//!   and is therefore a GUI thread that must pump messages.
//!
//! # Event-Driven Architecture
//!
//! The main loop blocks on `MsgWaitForMultipleObjects` with two wait handles
//! plus the message queue (`QS_ALLINPUT`):
//!
//! 1. **Hook signal event** (index 0, highest priority) — signaled by the
//!    hook callback thread after each `sender.send(HookEvent)`.
//! 2. **Pipe connected event** (index 1) — signaled by the background accept
//!    thread when a CLI client connects via named pipe.
//! 3. **Win32 message queue** (index 2) — new window messages for any
//!    window owned by this thread (the border overlays). The border hook
//!    thread's `SetWindowPos` / `UpdateLayeredWindow` calls on overlays
//!    sync-dispatch `WM_*` messages here; pumping them lets those calls
//!    complete and prevents the border subsystem from deadlocking.
//!
//! When the wait returns, the loop:
//! 1. Pumps ALL pending window messages (`PeekMessage` loop) — every wake,
//!    regardless of which handle fired. This is critical: even on hook or
//!    IPC wakes, deferred border `SendMessage` calls need servicing.
//! 2. Resets the hook signal (`ResetEvent`) — **before** draining, to close
//!    the race window (see below).
//! 3. Drains ALL pending hook events via `try_recv()` loop.
//! 4. If the pipe event was signaled, processes the IPC session (read,
//!    dispatch, write, disconnect, re-accept), pumping messages between
//!    each read so border sync-dispatch never blocks on an IPC-blocked main
//!    thread.
//! 5. Returns to `MsgWaitForMultipleObjects`.
//!
//! # Race-Free Hook Drain
//!
//! The critical ordering is **ResetEvent before drain**:
//!
//! 1. `MsgWaitForMultipleObjects` wakes (hook signal is set).
//! 2. `pump_messages()` — service border overlay messages first.
//! 3. `ResetEvent(hook_signal)` — clear the signal.
//! 4. `try_recv()` loop — drain ALL events from the channel.
//!
//! Any event pushed between ResetEvent and try_recv is caught by try_recv.
//! Any event pushed after the try_recv loop calls `SetEvent`, so the next
//! `MsgWaitForMultipleObjects` wakes immediately. **No events are lost.**
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
//! list is non-empty, `MsgWaitForMultipleObjects` uses a finite timeout (100 ms)
//! instead of `INFINITE`, ensuring retries happen even without new hook events.
//!
//! This approach is event-driven by default (zero CPU while idle) with a
//! bounded timer fallback only when windows are pending classification.

use windows::Win32::System::Threading::ResetEvent;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
    TranslateMessage,
};

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
    ///     MsgWaitForMultipleObjects(          // Sleep until something happens
    ///         [hook_signal, connected_event],
    ///         timeout,
    ///         QS_ALLINPUT                     // Also wake for window messages
    ///     )
    ///     pump_messages()                     // Service border overlay messages
    ///     ResetEvent(hook_signal)             // Race-free: reset BEFORE drain
    ///     process_hook_events()               // Drain hooks + retry pending
    ///
    ///     if connected_event was signaled:
    ///         loop {                           // IPC session with this client
    ///             pump_messages()              // Don't starve border thread
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
    /// thread and using `MsgWaitForMultipleObjects`, hook events are now
    /// processed immediately — window creation/removal/focus changes reflect
    /// in the layout without requiring an IPC dispatch.
    ///
    /// The inner loop (blocking `read_message`) is acceptable because the CLI
    /// is one-shot: connect → send one command → read response → disconnect.
    /// The entire IPC transaction completes in under 1 ms.
    ///
    /// # Why a Message Pump Is Required
    ///
    /// The border subsystem creates overlay windows on this thread (via
    /// `CreateWindowExW` in [`crate::borders::BorderOverlay::create`]). Per
    /// Win32 rules, any thread that creates windows becomes a GUI thread and
    /// must pump messages — otherwise cross-thread `SendMessage` blocks
    /// indefinitely. The border hook thread's `SetWindowPos` /
    /// `UpdateLayeredWindow` calls sync-dispatch `WM_*` messages to these
    /// overlay windows. Without a pump, those calls would deadlock and so
    /// would every border operation (and, eventually, every IPC dispatch).
    /// See `docs/src/dev-guide/borders.md` (planned).
    pub fn run(&mut self) {
        log::info!("stmd: daemon started, event-driven loop on named pipe");

        // Start the first background accept — spawns a thread that blocks in
        // ConnectNamedPipe until a client connects.
        self.server.start_accept();

        // Event handles for MsgWaitForMultipleObjects.
        // Index 0 = hook_signal (highest priority — hooks drained first)
        // Index 1 = connected_event (IPC client connected)
        // Index 2 = wake-by-message-queue (implicit, from QS_ALLINPUT)
        let hook_handle = self.hook_signal.raw();
        let connect_handle = self.server.connected_event_handle();
        let wait_handles = [hook_handle, connect_handle];

        loop {
            // Block until a hook event, an IPC client connection, OR a Win32
            // window message arrives. When there are pending window creations,
            // use a finite timeout so they're retried even without new wakes.
            //
            // MsgWaitForMultipleObjects returns WAIT_EVENT whose inner u32 is:
            //   0     = WAIT_OBJECT_0 (first handle = hook_signal)
            //   1     = WAIT_OBJECT_0 + 1 (second handle = connected_event)
            //   2     = WAIT_OBJECT_0 + 2 (new input in queue — needs pumping)
            //   258   = WAIT_TIMEOUT (retry pending creations)
            //   u32::MAX = WAIT_FAILED (fatal — break)
            let timeout = if self.pending_creations.is_empty() {
                u32::MAX // INFINITE
            } else {
                PENDING_RETRY_TIMEOUT_MS
            };

            let wait_result = unsafe {
                MsgWaitForMultipleObjects(Some(&wait_handles), false, timeout, QS_ALLINPUT)
            };

            // Check for WAIT_FAILED (0xFFFFFFFF).
            if wait_result.0 == u32::MAX {
                log::error!("stmd: MsgWaitForMultipleObjects failed");
                break;
            }

            // The raw wait code — index of the first signaled handle, or
            // WAIT_TIMEOUT (258) if the timeout expired.
            let signaled = wait_result.0;

            // Pump ALL pending window messages BEFORE doing anything else.
            // This must run on every wake — hook signal, IPC connect, message
            // queue, or timeout — so the border hook thread's sync-dispatch
            // `SendMessage` calls to overlay windows complete promptly.
            // Without this, border operations (and eventually IPC) deadlock.
            self.pump_messages();

            // Always reset hook signal FIRST (race-free pattern), then drain.
            // Any event pushed between ResetEvent and try_recv is caught by
            // try_recv. Any event pushed after the drain calls SetEvent, so
            // the next MsgWaitForMultipleObjects wakes immediately.
            // On WAIT_TIMEOUT this is a harmless no-op (event is unsignaled).
            unsafe {
                let _ = ResetEvent(hook_handle);
            }

            // Drain ALL pending hook events and retry pending creations.
            // This runs on every wake — hook signal, IPC connect, or timeout.
            self.process_hook_events();

            // Check if an IPC client connected (index 1 = WAIT_OBJECT_0 + 1).
            // On WAIT_TIMEOUT (258), hook-only wake (0), or message wake (2),
            // this is false and we skip the IPC inner loop.
            if signaled == 1 {
                // Inner loop: handle this client's messages.
                // The CLI is one-shot (connect → send → read response →
                // disconnect), so this loop typically runs once before the
                // client disconnects. Hook events AND window messages are
                // drained between reads so neither subsystem starves.
                loop {
                    // Service border overlay messages before each IPC read.
                    self.pump_messages();

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

    /// Drain ALL pending Win32 window messages from this thread's queue.
    ///
    /// Required because the main thread creates border overlay windows
    /// (via `CreateWindowExW`), which makes it a GUI thread under Win32
    /// rules. GUI threads MUST pump messages or cross-thread `SendMessage`
    /// blocks indefinitely. The border hook thread's `SetWindowPos` /
    /// `UpdateLayeredWindow` calls on overlays sync-dispatch `WM_*`
    /// messages here; pumping them lets those calls complete.
    ///
    /// Uses `PeekMessageW(PM_REMOVE)` until empty rather than the blocking
    /// `GetMessageW` because the main loop still needs to wait on hook
    /// events and IPC connections between message batches.
    ///
    /// Window messages are dispatched to the overlay window procedure
    /// (`overlay_wnd_proc`), which currently just calls `DefWindowProcW`.
    /// The border crate does not handle any messages itself — `WM_PAINT`,
    /// `WM_NCHITTEST`, etc. all flow to `DefWindowProcW`. The pump's only
    /// job is to unblock cross-thread senders.
    fn pump_messages(&mut self) {
        let mut msg = MSG::default();
        // PeekMessageW returns BOOL — true if a message was available.
        // PM_REMOVE removes it from the queue; the loop ends once empty.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() } {
            // TranslateMessage (virtual-key → character) and DispatchMessageW
            // (invoke the target window's wndproc) are both infallible in
            // practice for these message types — ignore their return values.
            unsafe {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
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
            log::debug!("hook: received {event:?}");
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
                HookEvent::StateChange { hwnd } => {
                    // Option D recovery: a window ignored at creation because
                    // it launched maximized/fullscreen may now be restored.
                    // The handler re-classifies only tracked, OS-ignored
                    // windows, so this is cheap when nothing is recoverable.
                    self.on_window_state_change(hwnd);
                }
                HookEvent::NameChange { hwnd } => {
                    // Option A recovery: a window whose title arrived late
                    // (e.g. Windows Terminal) gets a second chance at
                    // registration. The handler only acts on untracked
                    // windows to avoid re-classifying tracked ones.
                    self.on_window_name_change(hwnd);
                }
            }
        }
    }
}
