//! FlowWM main event loop and hook event routing.
//!
//! This module contains:
//!
//! - [`FlowWM::run`] — the event-driven main loop that uses
//!   `MsgWaitForMultipleObjects` to wait on hook events, IPC client
//!   connections, and the Win32 message queue simultaneously.
//! - [`FlowWM::process_hook_events`] — drains pending hook
//!   events from the channel and routes them to individual handlers.
//! - [`FlowWM::pump_messages`] — drains the Win32 message
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
//! 4. If a client is connected, processes the IPC session (read, dispatch,
//!    write, disconnect, re-accept), pumping messages between each read so
//!    border sync-dispatch never blocks on an IPC-blocked main thread.
//!    Connection is detected with an independent zero-timeout
//!    `WaitForSingleObject` poll rather than the wait's return index — see
//!    [`FlowWM::run`] for why the return index alone starves IPC.
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
//! [`pending_creations`](super::types::FlowWM::pending_creations).
//! On every `process_hook_events` call, pending windows are retried. When the
//! list is non-empty, `MsgWaitForMultipleObjects` uses a finite timeout (100 ms)
//! instead of `INFINITE`, ensuring retries happen even without new hook events.
//!
//! This approach is event-driven by default (zero CPU while idle) with a
//! bounded timer fallback only when windows are pending classification.

use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::System::Threading::{ResetEvent, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
    TranslateMessage,
};

use crate::registry::HookEvent;

use super::types::FlowWM;

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

impl FlowWM {
    /// Run the event-driven main loop. Blocks until the `Stop` command is
    /// received or a fatal error occurs.
    ///
    /// # Loop Structure
    ///
    /// ```text
    /// start_accept()                          // Spawn background accept thread
    /// loop {
    ///     resume float tracking if its deadline arrived
    ///     reconcile foreground against GetForegroundWindow()
    ///     timeout = soonest(pending_creations?100ms, float_resume_deadline,
    ///                       foreground_sync_interval, INFINITE)
    ///     MsgWaitForMultipleObjects(          // Sleep until something happens
    ///         [hook_signal, connected_event],
    ///         timeout,
    ///         QS_ALLINPUT                     // Also wake for window messages
    ///     )
    ///     pump_messages()                     // Service border overlay messages
    ///     ResetEvent(hook_signal)             // Race-free: reset BEFORE drain
    ///     process_hook_events()               // Drain hooks + retry pending
    ///
    ///     if WaitForSingleObject(connected_event, 0) is signaled:
    ///         // NOT the wait's return index — see "Why the return index
    ///         // starves IPC" below. The hook signal (index 0) is set on
    ///         // every forwarded WinEvent and would otherwise permanently
    ///         // shadow the connect event (index 1).
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
    /// # Why the Return Index Alone Starves IPC
    ///
    /// A natural implementation checks `if wait_result == WAIT_OBJECT_0 + 1`
    /// (the connect event's index) to decide whether to service IPC. That
    /// **does not work** with this hook design. `MsgWaitForMultipleObjects`
    /// with `bWaitAll = false` returns the **lowest** signaled handle index;
    /// the hook signal is index 0 and the connect event is index 1. The hook
    /// callback calls `SetEvent` after forwarding every non-`LOCATIONCHANGE`
    /// WinEvent system-wide, which arrive continuously (shell, tray, tooltips,
    /// and the border overlays' own creation). That signal is a manual-reset
    /// event re-set before each wait, so it is signaled on nearly every
    /// iteration — permanently shadowing the connect event and making the wait
    /// return `0` even while a client is connected. Draining the channel is no
    /// cure, because the *next* forwarded event re-sets the signal before the
    /// next wait. The independent zero-timeout `WaitForSingleObject` poll on
    /// the connect event breaks that dependency so IPC is serviced whenever a
    /// client is actually waiting, regardless of hook-signal state.
    ///
    /// # Why a Message Pump Is Required
    ///
    /// The border subsystem creates overlay windows on this thread (via
    /// `CreateWindowExW` in [`crate::borders::Border::create`]). Per
    /// Win32 rules, any thread that creates windows becomes a GUI thread and
    /// must pump messages — otherwise cross-thread `SendMessage` blocks
    /// indefinitely. The border hook thread's `SetWindowPos` /
    /// `UpdateLayeredWindow` calls sync-dispatch `WM_*` messages to these
    /// overlay windows. Without a pump, those calls would deadlock and so
    /// would every border operation (and, eventually, every IPC dispatch).
    /// See `docs/src/dev-guide/borders.md`.
    pub fn run(&mut self) {
        log::info!("flowd: daemon started, event-driven loop on named pipe");

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
            // Resume float tracking if its suppression deadline has arrived.
            // Runs before computing the timeout so a due deadline is handled
            // now instead of scheduling a 1 ms re-wake.
            self.maybe_resume_float_tracking();

            // Reconcile internal focus against GetForegroundWindow(). This runs
            // on every wake, but no-ops in ~1 µs when tracked focus already
            // matches reality. The `foreground_sync_interval_ms` timeout folded
            // into `compute_wait_timeout` is what guarantees this runs at least
            // that often even with no hook/IPC activity; running it on every
            // wake adds only a microsecond-scale guard clause.
            self.reconcile_foreground();

            // Block until a hook event, an IPC client connection, OR a Win32
            // window message arrives. When there are pending window creations
            // or a float-resume deadline, use the sooner finite timeout so
            // they're retried even without new wakes.
            //
            // MsgWaitForMultipleObjects returns WAIT_EVENT whose inner u32 is:
            //   0     = WAIT_OBJECT_0 (first handle = hook_signal)
            //   1     = WAIT_OBJECT_0 + 1 (second handle = connected_event)
            //   2     = WAIT_OBJECT_0 + 2 (new input in queue — needs pumping)
            //   258   = WAIT_TIMEOUT (retry pending creations / resume float)
            //   u32::MAX = WAIT_FAILED (fatal — break)
            let timeout = self.compute_wait_timeout();

            let wait_result = unsafe {
                MsgWaitForMultipleObjects(Some(&wait_handles), false, timeout, QS_ALLINPUT)
            };

            // Check for WAIT_FAILED (0xFFFFFFFF).
            if wait_result.0 == u32::MAX {
                log::error!("flowd: MsgWaitForMultipleObjects failed");
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

            // Check if an IPC client connected.
            //
            // We CANNOT rely solely on `signaled == 1` here.
            // `MsgWaitForMultipleObjects(bWaitAll = false)` returns the
            // LOWEST-index signaled handle. The hook signal is index 0 and the
            // connect event is index 1, so `signaled == 1` is returned only
            // when the connect event is signaled AND the hook signal is not.
            //
            // The hook callback calls `SetEvent` after forwarding EVERY
            // non-LOCATIONCHANGE WinEvent system-wide (CREATE/SHOW/HIDE/...),
            // which arrive continuously (shell, tray, tooltips, and the border
            // overlays' own creation at startup). Because the signal is a
            // manual-reset event re-set before each wait, it is signaled on
            // nearly every iteration — permanently shadowing the connect event
            // and starving IPC no matter how long the client waits.
            //
            // The fix: test the connect event independently with a zero-timeout
            // `WaitForSingleObject`. This services IPC whenever a client is
            // actually connected, regardless of the hook signal's state. The
            // manual-reset connect event stays signaled until `start_accept`
            // resets it (after the session), so the 0-timeout poll is
            // non-consuming and cheap.
            let ipc_connected =
                signaled == 1 || unsafe { WaitForSingleObject(connect_handle, 0) } == WAIT_OBJECT_0;

            if ipc_connected {
                // Inner loop: handle this client's messages.
                // The CLI is one-shot (connect → send → read response →
                // disconnect), so this loop typically runs once before the
                // client disconnects. Hook events AND window messages are
                // drained between reads so neither subsystem starves.
                loop {
                    // Service border overlay messages before each IPC read.
                    self.pump_messages();

                    // Drain hook events before each IPC message, and resume
                    // float tracking if its deadline arrived mid-session.
                    self.process_hook_events();
                    self.maybe_resume_float_tracking();

                    // Read next IPC message (blocking — but client is active).
                    match self.server.read_message() {
                        Ok(msg) => {
                            let response = self.dispatch(&msg);
                            let is_stop = self.shutting_down;

                            if let Err(e) = self.server.write_response(&response) {
                                log::warn!("flowd: failed to write response: {e}");
                                break;
                            }

                            if is_stop {
                                log::info!("flowd: shutting down");
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
                    log::warn!("flowd: failed to disconnect client: {e}");
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

    /// Compute the `WaitForMultipleObjects` timeout for this iteration.
    ///
    /// Delegates to [`compute_wait_timeout_inner`] (the testable pure form)
    /// using the manager's current pending-creations state and resume
    /// deadline. See that function for the deadline-selection rules.
    fn compute_wait_timeout(&self) -> u32 {
        // Fold the next foreground-reconciliation deadline into the wait so the
        // loop never sleeps past the configured sync interval. `None` only when
        // the interval is disabled (0); otherwise always Some.
        let next_foreground_sync =
            (self.config.focus.foreground_sync_interval_ms != 0).then(|| {
                self.last_foreground_sync
                    + std::time::Duration::from_millis(
                        self.config.focus.foreground_sync_interval_ms,
                    )
            });
        compute_wait_timeout_inner(
            !self.pending_creations.is_empty(),
            self.float_resume_deadline,
            next_foreground_sync,
            std::time::Instant::now(),
        )
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
                HookEvent::LocationChange { hwnd } => {
                    // During a tile drag, LOCATIONCHANGE for the dragged window
                    // goes to the drag-move handler; all other LOCATIONCHANGE
                    // events (floats) go to the float sync path as before.
                    let is_dragged = self
                        .drag_state
                        .as_ref()
                        .map(|ds| ds.dragged_hwnd == hwnd)
                        .unwrap_or(false);
                    if is_dragged {
                        self.on_drag_move(hwnd);
                    } else {
                        self.on_float_location_changed(hwnd);
                    }
                }
                HookEvent::MoveSizeStart { hwnd } => {
                    self.on_drag_start(hwnd);
                }
                HookEvent::MoveSizeEnd { hwnd } => {
                    self.on_drag_end(hwnd);
                }
            }
        }
    }
}

/// Pure, clock-injectable form of [`FlowWM::compute_wait_timeout`].
///
/// Returns the smaller of the two finite deadlines:
/// - `has_pending_creations` → [`PENDING_RETRY_TIMEOUT_MS`].
/// - `float_resume_deadline` → milliseconds remaining until it's due (floored
///   to 1 so a due deadline re-wakes the loop instead of busy-looping on 0).
///
/// Returns `u32::MAX` (`INFINITE`) when no source is active. Extracted
/// to a free function so the branching is unit-testable without constructing
/// a full [`FlowWM`] (which needs Win32 + a hook thread).
///
/// Three finite-deadline sources fold into the result via `min`:
/// 1. `has_pending_creations` → fixed `PENDING_RETRY_TIMEOUT_MS` cadence.
/// 2. `float_resume_deadline` → remaining ms until float tracking resumes.
/// 3. `next_foreground_sync` → remaining ms until the foreground is reconciled
///    against `GetForegroundWindow()` (closes the gap when
///    `EVENT_SYSTEM_FOREGROUND` is dropped under rapid window churn).
fn compute_wait_timeout_inner(
    has_pending_creations: bool,
    float_resume_deadline: Option<std::time::Instant>,
    next_foreground_sync: Option<std::time::Instant>,
    now: std::time::Instant,
) -> u32 {
    let mut best: Option<u32> = None;
    if has_pending_creations {
        best = Some(PENDING_RETRY_TIMEOUT_MS);
    }
    // Fold two optional deadlines through the same min-reduce. A due (past)
    // deadline floors at 1 ms so the loop re-wakes and runs the handler instead
    // of busy-spinning; the top-of-loop handlers clear due deadlines first.
    for deadline in float_resume_deadline
        .into_iter()
        .chain(next_foreground_sync)
    {
        let remaining = deadline.saturating_duration_since(now);
        let ms = remaining.as_millis().min(u32::MAX as u128) as u32;
        let ms = ms.max(1);
        best = Some(best.map_or(ms, |b| b.min(ms)));
    }
    best.unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn wait_timeout_infinite_when_idle() {
        let now = Instant::now();
        assert_eq!(compute_wait_timeout_inner(false, None, None, now), u32::MAX);
    }

    #[test]
    fn wait_timeout_pending_creations_uses_retry_cadence() {
        let now = Instant::now();
        assert_eq!(
            compute_wait_timeout_inner(true, None, None, now),
            PENDING_RETRY_TIMEOUT_MS,
        );
    }

    #[test]
    fn wait_timeout_float_deadline_uses_remaining_ms() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(500);
        assert_eq!(
            compute_wait_timeout_inner(false, Some(deadline), None, now),
            500
        );
    }

    #[test]
    fn wait_timeout_picks_sooner_when_deadline_under_pending_cadence() {
        let now = Instant::now();
        // Pending = 100 ms cadence; deadline in 50 ms → 50 wins.
        let deadline = now + Duration::from_millis(50);
        assert_eq!(
            compute_wait_timeout_inner(true, Some(deadline), None, now),
            50
        );
    }

    #[test]
    fn wait_timeout_pending_cadence_wins_when_deadline_is_farther() {
        let now = Instant::now();
        // Pending = 100 ms; deadline in 5 s → 100 wins.
        let deadline = now + Duration::from_secs(5);
        assert_eq!(
            compute_wait_timeout_inner(true, Some(deadline), None, now),
            PENDING_RETRY_TIMEOUT_MS,
        );
    }

    #[test]
    fn wait_timeout_due_deadline_floors_to_one_ms() {
        let now = Instant::now();
        // Already past: saturating_duration_since is 0 → floor to 1 so the
        // loop re-wakes and runs the resume handler instead of busy-spinning.
        let deadline = now - Duration::from_millis(10);
        assert_eq!(
            compute_wait_timeout_inner(false, Some(deadline), None, now),
            1
        );
    }

    #[test]
    fn wait_timeout_foreground_sync_uses_remaining_ms() {
        let now = Instant::now();
        // Sync deadline in 250 ms (the default interval) → 250.
        let deadline = now + Duration::from_millis(250);
        assert_eq!(
            compute_wait_timeout_inner(false, None, Some(deadline), now),
            250
        );
    }

    #[test]
    fn wait_timeout_foreground_sync_wins_when_soonest() {
        let now = Instant::now();
        // Float deadline in 5 s; foreground sync in 80 ms → 80 wins. Confirms
        // the third source participates in the min-reduce, not just appends.
        let float_deadline = now + Duration::from_secs(5);
        let sync_deadline = now + Duration::from_millis(80);
        assert_eq!(
            compute_wait_timeout_inner(false, Some(float_deadline), Some(sync_deadline), now),
            80
        );
    }
}
