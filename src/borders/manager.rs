//! [`BorderManager`] — top-level handle the daemon holds for the whole
//! border subsystem.
//!
//! Owns the target→overlay map and a background hook thread subscribed to
//! `EVENT_OBJECT_LOCATIONCHANGE`. All public methods are idempotent and safe
//! to call from the IPC thread.
//!
//! See `docs/src/dev-guide/borders.md` for the "follow HWND, not intent"
//! design rationale and the threading model.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, PostThreadMessageW, WM_QUIT};

use crate::config::BorderConfig;

use super::overlay::BorderOverlay;
use super::style::BorderStyle;

// ── Event constants ──────────────────────────────────────────────────
// Defined locally (same convention as `registry/hooks.rs`) for clarity and
// to keep this crate's Win32 surface self-contained.

/// `EVENT_OBJECT_LOCATIONCHANGE` — a window's on-screen rect changed.
///
/// Fires on every pixel of move/resize — including every frame of stm's own
/// animations. The daemon's hook thread in `registry/hooks.rs` deliberately
/// excludes this event because it would flood the IPC channel. The border
/// crate installs its own independent hook on its own thread so it can
/// sample the latest on-screen rect without going through the channel.
const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;

/// `OBJID_WINDOW` — identifies the window object itself (not a child control).
const OBJID_WINDOW: i32 = 0x0000;

/// `WINEVENT_OUTOFCONTEXT` — callback runs in the caller's context (this
/// thread's `GetMessageW` loop), not synchronously inside the event source.
const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;

/// Process-global pointer back to the manager for the WinEvent callback.
///
/// `SetWinEventHook` callbacks cannot take userdata, so the callback reaches
/// the manager through this static. Mirrors the `HOOK_SENDER` pattern in
/// `registry/hooks.rs`. Set once by [`BorderManager::start_hooks`] and never
/// cleared — `OnceLock` has no stable `clear`. This limits the crate to one
/// active `BorderManager` per process, which matches the single-daemon model.
static BORDER_INNER: OnceLock<Arc<Inner>> = OnceLock::new();

/// Top-level border subsystem handle.
///
/// One instance lives on `ScrollTilingManager` for the entire daemon
/// lifetime (created in `daemon/new.rs`, dropped on shutdown). It owns the
/// HWND→overlay map and a background hook thread subscribed to
/// `EVENT_OBJECT_LOCATIONCHANGE`.
///
/// # Thread safety
///
/// The hook thread (sync-on-LOCATIONCHANGE) and the IPC thread
/// (`attach`/`detach`/`set_style`/`set_visible`) both touch the overlay map,
/// hence the `Mutex<HashMap>`. Internally, the manager is wrapped in
/// `Arc<Inner>` so the hook callback (which has no userdata) can find it via
/// a process-global `OnceLock`.
pub struct BorderManager {
    inner: Arc<Inner>,
}

struct Inner {
    /// Resolved config (thickness, colors). Read-only after `new`.
    config: BorderConfig,
    /// Target HWND (raw `isize`) → overlay.
    ///
    /// Each [`BorderOverlay`] is wrapped in [`Arc`] so the hook callback can
    /// clone-out a reference under the lock and then drop the guard **before**
    /// issuing Win32 calls. Holding the lock during `SetWindowPos` /
    /// `UpdateLayeredWindow` / `ShowWindow` deadlocks: those calls may
    /// synchronously dispatch `WM_*` messages to the overlay window, which
    /// is owned by the IPC thread; if the IPC thread is itself blocked on
    /// this Mutex, neither thread can make progress.
    overlays: Mutex<HashMap<isize, Arc<BorderOverlay>>>,
    /// OS thread ID of the hook thread (set by `start_hooks`, read by `Drop`
    /// to post `WM_QUIT`). `None` until `start_hooks` succeeds.
    hook_thread: Mutex<Option<HookThreadState>>,
}

#[derive(Debug)]
struct HookThreadState {
    /// OS-level thread ID, used by `PostThreadMessageW(WM_QUIT)` to stop
    /// the hook thread.
    os_thread_id: u32,
}

impl BorderManager {
    /// Construct a new manager. Stores config but does **not** spawn the
    /// hook thread — call [`Self::start_hooks`] separately after construction.
    ///
    /// The split lets unit tests construct a manager (and exercise the
    /// overlay map directly) without competing for the process-global
    /// [`BORDER_INNER`] slot.
    #[must_use]
    pub fn new(config: BorderConfig) -> Self {
        let inner = Arc::new(Inner {
            config,
            overlays: Mutex::new(HashMap::new()),
            hook_thread: Mutex::new(None),
        });
        Self { inner }
    }

    /// Spawn the background hook thread that re-syncs overlays on every
    /// `EVENT_OBJECT_LOCATIONCHANGE`.
    ///
    /// Must be called at most once per process (the [`BORDER_INNER`]
    /// `OnceLock` is single-use). The daemon calls this once during startup
    /// after constructing the manager.
    ///
    /// # Errors
    ///
    /// Returns an error if `start_hooks` was already called, the thread
    /// fails to spawn, or the thread exits before reporting its OS thread ID.
    pub fn start_hooks(&self) -> Result<(), String> {
        BORDER_INNER
            .set(Arc::clone(&self.inner))
            .map_err(|_| "start_hooks called more than once — this is a bug".to_owned())?;

        let (tid_tx, tid_rx) = mpsc::channel::<u32>();
        let inner_clone = Arc::clone(&self.inner);

        std::thread::Builder::new()
            .name("stm-borders-hook".to_owned())
            .spawn(move || {
                // SAFETY: GetCurrentThreadId returns the OS thread ID of the
                // caller. Pure metadata read, no handle.
                let os_tid = unsafe { GetCurrentThreadId() };
                let _ = tid_tx.send(os_tid);
                run_border_hook_loop(inner_clone);
            })
            .map_err(|e| format!("failed to spawn border hook thread: {e}"))?;

        let os_thread_id = tid_rx.recv().map_err(|_| {
            "border hook thread failed to start (exited before reporting thread ID)".to_owned()
        })?;

        let mut hook_thread = self
            .inner
            .hook_thread
            .lock()
            .expect("hook_thread mutex poisoned");
        *hook_thread = Some(HookThreadState { os_thread_id });

        log::info!("borders: hook thread started (OS tid={os_thread_id})");
        Ok(())
    }

    /// Read-only access to the resolved config (used by `style_for_state`).
    pub fn config(&self) -> &BorderConfig {
        &self.inner.config
    }

    /// Attach a border overlay to `target`. Idempotent: re-attaching to an
    /// already-attached target updates its style without recreating the
    /// overlay HWND.
    ///
    /// Does nothing (returns early) when the manager's config has
    /// `enabled = false`.
    pub fn attach(&self, target: HWND, style: BorderStyle) {
        if !self.inner.config.enabled {
            return;
        }
        let target_raw = target.0 as isize;
        // Fast path: already attached → update style. Clone the Arc under the
        // lock, drop the guard, then call `set_style` (which makes Win32
        // calls) outside the lock — see the doc on `overlays` for why.
        let existing_arc = {
            let overlays = self.inner.overlays.lock().expect("overlays mutex poisoned");
            overlays.get(&target_raw).cloned()
        };
        if let Some(arc) = existing_arc {
            arc.set_style(style);
            return;
        }
        // Slow path: create a new overlay. `BorderOverlay::create` makes
        // Win32 calls (`CreateWindowExW`, `SetWindowPos`, `UpdateLayeredWindow`,
        // `ShowWindow`) — must happen outside the overlays lock.
        let new_overlay = match BorderOverlay::create(target, style) {
            Ok(o) => Arc::new(o),
            Err(e) => {
                log::error!("borders: failed to attach overlay: {e}");
                return;
            }
        };
        // Insert under the lock. `attach` is only called from the IPC thread
        // (the hook thread only reads via `sync_overlay`), so no concurrent
        // insert race to handle.
        {
            let mut overlays = self.inner.overlays.lock().expect("overlays mutex poisoned");
            overlays.insert(target_raw, new_overlay);
        }
        log::debug!("borders: attached overlay to target={target_raw:#x}");
    }

    /// Detach and destroy the overlay for `target`, if any. Idempotent.
    pub fn detach(&self, target: HWND) {
        let target_raw = target.0 as isize;
        // Remove under the lock; drop the removed overlay outside the lock
        // so `BorderOverlay::drop` (which calls `DestroyWindow`) cannot
        // re-enter the overlays Mutex.
        let removed = {
            let mut overlays = self.inner.overlays.lock().expect("overlays mutex poisoned");
            overlays.remove(&target_raw)
        };
        if removed.is_some() {
            drop(removed);
            log::debug!("borders: detached overlay from target={target_raw:#x}");
        }
    }

    /// Update the border color for `target` without recreating the overlay.
    /// No-op if the target is not attached.
    pub fn set_style(&self, target: HWND, style: BorderStyle) {
        let target_raw = target.0 as isize;
        let arc = {
            let overlays = self.inner.overlays.lock().expect("overlays mutex poisoned");
            overlays.get(&target_raw).cloned()
        };
        if let Some(overlay) = arc {
            overlay.set_style(style);
        }
    }

    /// Show or hide the overlay for `target` (used on minimize/restore).
    /// No-op if the target is not attached.
    pub fn set_visible(&self, target: HWND, visible: bool) {
        let target_raw = target.0 as isize;
        let arc = {
            let overlays = self.inner.overlays.lock().expect("overlays mutex poisoned");
            overlays.get(&target_raw).cloned()
        };
        if let Some(overlay) = arc {
            overlay.set_visible(visible);
        }
    }
}

impl Inner {
    /// Look up the overlay for `target_raw` and re-sync its geometry against
    /// the target's current `GetWindowRect`.
    ///
    /// **Critical**: clones the [`Arc`] under the lock, drops the guard, and
    /// only then calls `sync_geometry` (which makes Win32 calls). Holding the
    /// lock during the Win32 calls deadlocks — see the doc on `overlays`.
    fn sync_overlay(&self, target_raw: isize) {
        let arc = {
            let overlays = self.overlays.lock().expect("overlays mutex poisoned");
            overlays.get(&target_raw).cloned()
        };
        if let Some(overlay) = arc {
            overlay.sync_geometry();
        }
    }
}

impl Drop for BorderManager {
    fn drop(&mut self) {
        // Drain the map outside the lock scope so individual
        // `BorderOverlay::drop` calls (which invoke `DestroyWindow`) cannot
        // re-enter the overlays Mutex.
        let drained: Vec<Arc<BorderOverlay>> = {
            let mut overlays = self.inner.overlays.lock().expect("overlays mutex poisoned");
            overlays.drain().map(|(_, v)| v).collect()
        };
        drop(drained);

        // Signal the hook thread to exit. By now the map is empty, so any
        // in-flight `sync_overlay` callback will find nothing and no-op.
        // Fire-and-forget — matches `registry/hooks.rs::HookThreadHandle::stop`.
        let hook_thread = self
            .inner
            .hook_thread
            .lock()
            .expect("hook_thread mutex poisoned");
        if let Some(state) = hook_thread.as_ref() {
            // SAFETY: PostThreadMessageW delivers a message to the thread
            // identified by `os_thread_id`. WM_QUIT causes GetMessageW to
            // return 0, breaking the loop.
            unsafe {
                let _ = PostThreadMessageW(state.os_thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }
}

// ── Hook thread internals ────────────────────────────────────────────

/// Runs the border hook message loop on the current thread.
///
/// Registers a single `EVENT_OBJECT_LOCATIONCHANGE` hook, enters a
/// `GetMessageW` loop, and unregisters the hook on `WM_QUIT`. The
/// `_inner_keepalive` argument bumps the [`Arc<Inner>`] refcount for the
/// duration of the loop so `Inner` cannot be dropped while the callback
/// might still fire; the callback itself reaches the manager through
/// [`BORDER_INNER`] rather than this reference (because `SetWinEventHook`
/// provides no userdata).
fn run_border_hook_loop(_inner_keepalive: Arc<Inner>) {
    // SAFETY: SetWinEventHook registers our callback for the single event
    // EVENT_OBJECT_LOCATIONCHANGE. WINEVENT_OUTOFCONTEXT requires the
    // calling thread to pump messages, which the loop below does.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(border_hook_callback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if hook.is_invalid() {
        log::error!("borders: failed to register LOCATIONCHANGE hook");
        return;
    }
    log::debug!("borders: LOCATIONCHANGE hook registered");

    let mut msg = MSG::default();
    loop {
        // SAFETY: GetMessageW blocks waiting for messages on this thread.
        // Return value: -1 = error, 0 = WM_QUIT, positive = continue.
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        match ret.0 {
            0 => {
                log::info!("borders: WM_QUIT received, exiting hook loop");
                break;
            }
            -1 => {
                log::error!("borders: GetMessageW returned -1");
                break;
            }
            _ => {}
        }
    }

    // SAFETY: UnhookWinEvent releases our hook registration before the
    // thread exits, so the callback can no longer fire.
    unsafe {
        let _ = UnhookWinEvent(hook);
    }
    log::debug!("borders: LOCATIONCHANGE hook unregistered, thread exiting");
}

/// WinEvent hook callback for `EVENT_OBJECT_LOCATIONCHANGE`.
///
/// Called by Windows on the hook thread whenever any window's on-screen rect
/// changes. Filters out child-control events (`OBJID_WINDOW` only), looks up
/// the corresponding overlay in the manager, and re-syncs its geometry
/// against the target's current `GetWindowRect`. Defensive `IsWindow` check
/// inside [`BorderOverlay::sync_geometry`] handles the case where the target
/// HWND has died between the event firing and the callback running.
///
/// # Safety
///
/// This is a Win32 callback — Windows invokes it with raw handles. We treat
/// the HWND as opaque (only read its numeric value), so there is no UB risk
/// from a stale/invalid HWND at this layer.
unsafe extern "system" fn border_hook_callback(
    _h_win_event_hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _dw_event_thread: u32,
    _dwms_event_time: u32,
) {
    // Filter out events for child controls (buttons, list items, etc.) —
    // only top-level windows get a border.
    if id_object != OBJID_WINDOW {
        return;
    }
    let Some(inner) = BORDER_INNER.get() else {
        return;
    };
    let target_raw = hwnd.0 as isize;
    inner.sync_overlay(target_raw);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::borders::CornerPreference;
    use crate::config::BorderConfig;
    use crate::config::Color;

    fn hwnd(n: isize) -> HWND {
        HWND(n as *mut _)
    }

    #[test]
    fn manager_construction_with_default_config() {
        let _mgr = BorderManager::new(BorderConfig::default());
    }

    #[test]
    fn disabled_manager_skips_attach() {
        let cfg = BorderConfig {
            enabled: false,
            ..BorderConfig::default()
        };
        let mgr = BorderManager::new(cfg);
        // Should be a no-op — no panic, no overlay.
        mgr.attach(
            hwnd(0x1234),
            BorderStyle::new(Color::rgb(0, 0, 0), 3, CornerPreference::Default),
        );
        let overlays = mgr.inner.overlays.lock().unwrap();
        assert!(overlays.is_empty());
    }

    #[test]
    fn enabled_manager_attaches_creates_overlay_entry() {
        // `BorderOverlay::create` does real Win32 work; for these fake HWND
        // values the overlay's `sync_geometry` early-returns via its
        // `IsWindow` guard, so the entry exists but never paints.
        let mgr = BorderManager::new(BorderConfig::default());
        mgr.attach(
            hwnd(0x4321),
            BorderStyle::new(Color::rgb(0, 0, 0), 3, CornerPreference::Default),
        );
        let overlays = mgr.inner.overlays.lock().unwrap();
        assert!(overlays.contains_key(&0x4321));
    }

    #[test]
    fn detach_is_idempotent() {
        let mgr = BorderManager::new(BorderConfig::default());
        mgr.detach(hwnd(0x9999)); // No-op — no panic.
        mgr.attach(
            hwnd(0x9999),
            BorderStyle::new(Color::rgb(0, 0, 0), 1, CornerPreference::Default),
        );
        mgr.detach(hwnd(0x9999));
        mgr.detach(hwnd(0x9999)); // Double-detach — no panic.
        let overlays = mgr.inner.overlays.lock().unwrap();
        assert!(!overlays.contains_key(&0x9999));
    }

    #[test]
    fn event_object_locationchange_constant_correct() {
        // Microsoft Win32 event-constant value (verify invariant).
        assert_eq!(EVENT_OBJECT_LOCATIONCHANGE, 0x800B);
    }

    #[test]
    fn objid_window_constant_correct() {
        assert_eq!(OBJID_WINDOW, 0);
    }

    #[test]
    fn winevent_outofcontext_constant_correct() {
        // Microsoft Win32 flag value — `WINEVENT_OUTOFCONTEXT == 0x0000`
        // (callback runs on the caller's thread, not inside the event source).
        assert_eq!(WINEVENT_OUTOFCONTEXT, 0x0000);
    }

    /// The hook callback fires for every `EVENT_OBJECT_LOCATIONCHANGE` on
    /// every window on the desktop — the overwhelming majority are NOT
    /// managed by stm. `sync_overlay` must look up the target, find it
    /// missing, and silently return without touching Win32.
    ///
    /// This is the only branch of `sync_overlay` reachable without a real
    /// `BorderOverlay` in the map (the "found" branch calls
    /// `overlay.sync_geometry()`, which makes live Win32 calls).
    #[test]
    fn sync_overlay_with_unknown_target_is_silent_noop() {
        let mgr = BorderManager::new(BorderConfig::default());
        // Arrange: no overlays attached. Several arbitrary raw HWND values
        // the hook might plausibly pass in.
        // Act + Assert: must not panic, must not call any Win32 geometry API.
        for &raw in &[0isize, 1, 0xDEAD_BEEF, -1] {
            mgr.inner.sync_overlay(raw);
        }
        // Map is still empty — the call must not have inserted anything.
        let overlays = mgr.inner.overlays.lock().unwrap();
        assert!(overlays.is_empty(), "sync_overlay must not mutate the map");
    }
}
