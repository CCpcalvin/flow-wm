//! WinEvent hook setup for window lifecycle tracking.
//!
//! This module registers Win32 event hooks via [`SetWinEventHook`] on a
//! dedicated background thread. Events are forwarded to the main thread
//! through an [`mpsc`] channel and processed by
//! [`WindowRegistry::process_pending_events`](super::WindowRegistry::process_pending_events).
//!
//! # Why a Background Thread?
//!
//! `SetWinEventHook` with `WINEVENT_OUTOFCONTEXT` requires the calling thread
//! to run a Windows message loop (`GetMessageW`). This is incompatible with
//! the IPC thread's named-pipe message loop. By running the hook on its own
//! thread, we isolate the two message loops and avoid conflicts.
//!
//! # Event Flow
//!
//! ```text
//! Windows OS                    Hook Thread                   IPC Thread
//! ┌──────────┐    callback    ┌──────────────┐   send()    ┌────────────────┐
//! │ SetWin-  │──────────────►│ hook_callback │────────────►│ process_pending │
//! │ EventHook│               │              │             │ _events()       │
//! └──────────┘               └──────────────┘             │ try_recv()      │
//!                                                          └────────────────┘
//! ```
//!
//! # Threading Model
//!
//! ```text
//! Main Thread (IPC):                Hook Thread:
//!   owns Arc<Mutex<Registry>>         SetWinEventHook ×3
//!   process_pending_events()          GetMessageW loop
//!       ↑ receiver.try_recv()             ↓ callback
//!       │                           sender.send(HookEvent)
//! ```
//!
//! # Hook Registration
//!
//! Three hooks are registered as event ranges:
//!
//! | Hook | Event Range | Purpose |
//! |------|-------------|---------|
//! | CREATE/DESTROY | `EVENT_OBJECT_CREATE` → `EVENT_OBJECT_DESTROY` | Window lifecycle |
//! | FOREGROUND | `EVENT_SYSTEM_FOREGROUND` | Focus changes |
//! | MINIMIZE | `EVENT_SYSTEM_MINIMIZESTART` → `EVENT_SYSTEM_MINIMIZEEND` | Minimize/restore |
//!
//! # Cleanup
//!
//! The hook thread runs until [`HookThreadHandle::stop()`] is called (or the
//! handle is dropped), which posts `WM_QUIT` to the hook thread's message loop.
//! On exit, all hooks are unregistered via `UnhookWinEvent`.
//!
//! # Test Isolation
//!
//! The optional `desktop_name` parameter allows the hook thread to switch to
//! a test desktop before registering hooks. This ensures hooks only fire for
//! windows on the test desktop, preventing interference with the user's real
//! windows during integration tests.

use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, PostThreadMessageW, WM_QUIT};

// ── Event constants ──────────────────────────────────────────────────
// Defined locally because windows-rs 0.58 may not expose all of them.

/// `EVENT_SYSTEM_FOREGROUND` — focus changed to a different window.
const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;

/// `EVENT_SYSTEM_MINIMIZESTART` — window was minimized.
const EVENT_SYSTEM_MINIMIZESTART: u32 = 0x0016;

/// `EVENT_SYSTEM_MINIMIZEEND` — window was restored from minimize.
const EVENT_SYSTEM_MINIMIZEEND: u32 = 0x0017;

/// `EVENT_OBJECT_CREATE` — a new window appeared.
const EVENT_OBJECT_CREATE: u32 = 0x8000;

/// `EVENT_OBJECT_DESTROY` — a window was destroyed.
const EVENT_OBJECT_DESTROY: u32 = 0x8001;

/// `OBJID_WINDOW` — identifies the window object itself (not a child).
const OBJID_WINDOW: i32 = 0x0000;

/// `WINEVENT_OUTOFCONTEXT` — callback runs in the caller's context.
const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;

// ── HookEvent ────────────────────────────────────────────────────────

/// Typed event produced by the WinEvent hook callback.
///
/// Each variant carries the window handle as an `isize` (the raw HWND value)
/// for `Send` safety — `HWND` itself is `!Send` because it wraps `*mut c_void`.
/// The IPC thread converts these back to `HWND` when processing events.
///
/// # Event Sources
///
/// These events are produced by the hook callback on the background thread
/// and consumed by [`WindowRegistry::process_pending_events`](super::WindowRegistry::process_pending_events)
/// on the IPC thread via an `mpsc` channel.
///
/// # Filtering
///
/// The callback filters by `OBJID_WINDOW` to only process events for actual
/// top-level windows (not child controls like buttons or text fields).
/// Unrecognized event IDs within the registered ranges are silently ignored.
#[derive(Debug)]
pub enum HookEvent {
    /// A new window appeared (`EVENT_OBJECT_CREATE`).
    Created {
        /// The created window handle value.
        hwnd: isize,
    },
    /// A window was destroyed (`EVENT_OBJECT_DESTROY`).
    Destroyed {
        /// The destroyed window handle value.
        hwnd: isize,
    },
    /// Focus changed to a different window (`EVENT_SYSTEM_FOREGROUND`).
    Foreground {
        /// The newly focused window handle value.
        hwnd: isize,
    },
    /// A window was minimized (`EVENT_SYSTEM_MINIMIZESTART`).
    MinimizeStart {
        /// The minimized window handle value.
        hwnd: isize,
    },
    /// A window was restored from minimize (`EVENT_SYSTEM_MINIMIZEEND`).
    MinimizeEnd {
        /// The restored window handle value.
        hwnd: isize,
    },
}

// ── Global sender ────────────────────────────────────────────────────

/// Module-level sender used by the hook callback.
///
/// `SetWinEventHook` does not support passing user-data in its callback, so
/// we cannot pass the `Sender<HookEvent>` directly. Instead, we store it in
/// a `OnceLock` that the callback reads. This is set exactly once when the
/// hook thread starts (in [`start_hook_thread`]).
///
/// # Safety
///
/// `OnceLock` provides safe one-time initialization. The sender is set before
/// any hooks are registered, so the callback will always find it populated.
/// The sender is `Send + Sync`, so reading it from the hook thread is safe.
static HOOK_SENDER: OnceLock<Sender<HookEvent>> = OnceLock::new();

// ── HookThreadHandle ─────────────────────────────────────────────────

/// Handle to the background hook thread.
///
/// Dropping this handle signals the hook thread to stop by posting `WM_QUIT`
/// to its message loop. The hook thread will then unregister all hooks and
/// exit cleanly.
///
/// # Design: RAII Cleanup
///
/// The `Drop` impl ensures the hook thread is always stopped, even if the
/// daemon crashes or the handle goes out of scope unexpectedly. This prevents
/// orphaned hook threads from lingering after the daemon exits.
pub struct HookThreadHandle {
    os_thread_id: u32,
}

impl HookThreadHandle {
    /// Signals the hook thread to stop by posting `WM_QUIT`.
    ///
    /// The hook thread will clean up all hooks and exit its message loop.
    pub fn stop(&self) {
        unsafe {
            let _ = PostThreadMessageW(self.os_thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}

impl Drop for HookThreadHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

// ── Public API ───────────────────────────────────────────────────────

/// Spawns the background hook thread and returns a channel receiver.
///
/// The hook thread:
/// 1. Optionally switches to the named desktop (for test isolation)
/// 2. Reports its OS thread ID back to the caller
/// 3. Registers WinEvent hooks via `SetWinEventHook`
/// 4. Runs a `GetMessageW` message loop (required for `WINEVENT_OUTOFCONTEXT`)
/// 5. The callback sends typed [`HookEvent`]s through the channel
///
/// Returns a tuple of `(event_receiver, thread_handle)`. The receiver should
/// be polled periodically via
/// [`WindowRegistry::process_pending_events`](super::WindowRegistry::process_pending_events).
///
/// # Arguments
///
/// * `desktop_name` — If `Some`, the hook thread opens and switches to this
///   desktop before registering hooks. Used for test isolation so hooks only
///   fire for windows on the test desktop.
///
/// # Errors
///
/// Returns an error if the hook thread has already been started (double-init),
/// the hook thread fails to start, or the desktop switch fails.
pub fn start_hook_thread(
    desktop_name: Option<String>,
) -> Result<(Receiver<HookEvent>, HookThreadHandle), String> {
    // In release builds, desktop_name must always be None.
    // This is a defense-in-depth check — the CLI arg is gated by debug_assertions
    // so this should never happen, but if it does, fail loudly.
    #[cfg(not(debug_assertions))]
    if desktop_name.is_some() {
        return Err(
            "--desktop is not supported in release builds (desktop isolation is debug-only)"
                .to_owned(),
        );
    }

    let (sender, receiver) = mpsc::channel();
    let (tid_tx, tid_rx) = mpsc::channel::<u32>();

    // Store the sender in the global OnceLock so the callback can access it.
    HOOK_SENDER
        .set(sender)
        .map_err(|_| "start_hook_thread called more than once — this is a bug".to_owned())?;

    std::thread::spawn(move || {
        // Switch to test desktop before any User32/GDI calls (debug builds only).
        #[cfg(debug_assertions)]
        if let Some(ref name) = desktop_name
            && let Err(e) = super::desktop::switch_to_desktop(name)
        {
            log::error!("hook thread: {e}");
            return;
        }

        // Suppress unused-variable warning in release builds.
        #[cfg(not(debug_assertions))]
        let _ = desktop_name;

        // Send our OS thread ID back to the caller before registering hooks.
        let os_tid = unsafe { GetCurrentThreadId() };
        let _ = tid_tx.send(os_tid);

        run_hook_loop();
    });

    // Wait for the thread to report its OS thread ID.
    let os_thread_id = tid_rx.recv().map_err(|_| {
        "hook thread failed to start (exited before reporting thread ID)".to_owned()
    })?;

    log::info!("hook thread started (OS tid={os_thread_id})");

    Ok((receiver, HookThreadHandle { os_thread_id }))
}

// ── Hook thread internals ────────────────────────────────────────────

/// Runs the hook message loop on the current thread.
///
/// This function:
/// 1. Registers WinEvent hooks
/// 2. Enters a `GetMessageW` loop
/// 3. Cleans up hooks on `WM_QUIT`
fn run_hook_loop() {
    let hooks = register_hooks();

    if hooks.is_empty() {
        log::error!("hook thread: failed to register any hooks");
        return;
    }

    log::info!("hook thread: {} hook(s) registered", hooks.len());

    // Standard Windows message loop — required for WINEVENT_OUTOFCONTEXT.
    let mut msg = MSG::default();
    loop {
        // SAFETY: GetMessageW retrieves the next message from the thread's
        // queue. Return value: -1 = error, 0 = WM_QUIT, positive = continue.
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        match ret.0 {
            0 => {
                log::info!("hook thread: WM_QUIT received, exiting");
                break;
            }
            -1 => {
                log::error!("hook thread: GetMessageW returned -1");
                break;
            }
            _ => {
                // Messages are dispatched internally by Windows for
                // WinEvent hook callbacks. No TranslateMessage/DispatchMessageW
                // needed for WINEVENT_OUTOFCONTEXT hooks.
            }
        }
    }

    // Unregister all hooks.
    for hook in &hooks {
        // SAFETY: UnhookWinEvent releases a previously registered hook.
        unsafe {
            let _ = windows::Win32::UI::Accessibility::UnhookWinEvent(*hook);
        }
    }

    log::info!("hook thread: stopped");
}

/// Registers WinEvent hooks for all tracked event types.
///
/// Returns a vector of hook handles. Each handle must be unregistered via
/// `UnhookWinEvent` when the hook thread exits.
///
/// Hooks are registered as ranges:
/// - `EVENT_OBJECT_CREATE` → `EVENT_OBJECT_DESTROY` (create + destroy)
/// - `EVENT_SYSTEM_FOREGROUND` (focus change)
/// - `EVENT_SYSTEM_MINIMIZESTART` → `EVENT_SYSTEM_MINIMIZEEND` (minimize/restore)
fn register_hooks() -> Vec<HWINEVENTHOOK> {
    let mut hooks = Vec::new();

    // Hook: EVENT_OBJECT_CREATE + EVENT_OBJECT_DESTROY (range)
    // SAFETY: SetWinEventHook registers a callback for events in the given range.
    let h = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_CREATE,
            EVENT_OBJECT_DESTROY,
            None,
            Some(hook_callback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if is_valid_hook(h) {
        hooks.push(h);
        log::debug!("hook registered: CREATE/DESTROY");
    } else {
        log::error!("failed to hook CREATE/DESTROY");
    }

    // Hook: EVENT_SYSTEM_FOREGROUND
    let h = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(hook_callback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if is_valid_hook(h) {
        hooks.push(h);
        log::debug!("hook registered: FOREGROUND");
    } else {
        log::error!("failed to hook FOREGROUND");
    }

    // Hook: EVENT_SYSTEM_MINIMIZESTART + EVENT_SYSTEM_MINIMIZEEND
    let h = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_MINIMIZESTART,
            EVENT_SYSTEM_MINIMIZEEND,
            None,
            Some(hook_callback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if is_valid_hook(h) {
        hooks.push(h);
        log::debug!("hook registered: MINIMIZE events");
    } else {
        log::error!("failed to hook MINIMIZE events");
    }

    hooks
}

/// Checks if a hook handle is valid (non-null).
fn is_valid_hook(h: HWINEVENTHOOK) -> bool {
    !h.is_invalid()
}

/// WinEvent hook callback.
///
/// Called by Windows on the hook thread for every event in the registered
/// range. We filter by `OBJID_WINDOW` to only process events for actual
/// windows (not child controls), then send typed [`HookEvent`]s through
/// the global sender.
///
/// # Safety
///
/// This is called by Windows. The `HOOK_SENDER` global must be initialized
/// before any hook is registered.
unsafe extern "system" fn hook_callback(
    _h_win_event_hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _dw_event_thread: u32,
    _dwms_event_time: u32,
) {
    // Only process events for actual windows (OBJID_WINDOW = 0).
    if id_object != OBJID_WINDOW {
        return;
    }

    // Get the sender from the global. If it's not set, we can't send events.
    let sender = match HOOK_SENDER.get() {
        Some(s) => s,
        None => return,
    };

    // Store the HWND value as isize for Send safety.
    let hwnd_val = hwnd.0 as isize;

    let hook_event = match event {
        EVENT_OBJECT_CREATE => HookEvent::Created { hwnd: hwnd_val },
        EVENT_OBJECT_DESTROY => HookEvent::Destroyed { hwnd: hwnd_val },
        EVENT_SYSTEM_FOREGROUND => HookEvent::Foreground { hwnd: hwnd_val },
        EVENT_SYSTEM_MINIMIZESTART => HookEvent::MinimizeStart { hwnd: hwnd_val },
        EVENT_SYSTEM_MINIMIZEEND => HookEvent::MinimizeEnd { hwnd: hwnd_val },
        _ => return, // Ignore other events in the range.
    };

    // Send the event. If the receiver is dropped (daemon shutting down),
    // that's fine — we just stop sending.
    let _ = sender.send(hook_event);
}
