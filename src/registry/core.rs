//! Core window registry — authoritative source of truth for all tracked windows.
//!
//! [`WindowRegistry`] is the central data structure that tracks every window
//! the daemon is aware of. It is the **single source of truth** for window
//! metadata, classification state, and focus tracking.
//!
//! # Data Structure
//!
//! The registry maintains a `HashMap<isize, Window>` keyed by the Win32 `HWND`
//! value stored as `isize`. This design choice (rather than using `HWND` directly)
//! is deliberate:
//!
//! - `HWND` wraps `*mut c_void` and is `!Send` — it cannot be transferred
//!   across thread boundaries.
//! - `isize` is `Send + Sync + Hash + Eq` — it works as a HashMap key and can
//!   be sent through channels safely.
//! - The conversion is trivial: `hwnd.0 as isize` / `HWND(val as *mut _)`.
//!
//! # Threading Model
//!
//! The registry is wrapped in `Arc<Mutex<WindowRegistry>>` and shared between
//! two threads:
//!
//! ```text
//! IPC Thread (main):              Hook Thread (background):
//!   lock(registry)                  SetWinEventHook ×3
//!   ├─ process_pending_events()     GetMessageW loop
//!   │    └─ try_recv() from mpsc        ↓ callback
//!   ├─ dispatch IPC command         sender.send(HookEvent)
//!   ├─ process_pending_events()
//!   └─ ... (repeat)
//! ```
//!
//! The **hook thread never locks the mutex**. It sends typed [`HookEvent`]s
//! through a non-blocking `mpsc` channel. The IPC thread drains these events
//! under its `MutexGuard` and applies all state transitions. This design:
//!
//! - Eliminates deadlocks (only one thread ever holds the lock).
//! - Keeps HWND dereferencing on the IPC thread (which owns the `MutexGuard`).
//! - Makes the hook callback fast and non-blocking (`try_recv` on the other end).
//!
//! # Initialization Flow
//!
//! ```text
//! 1. WindowRegistry::new(user_rules, default_rules)
//!    └─ builds ClassificationPipeline from user and default rule configs
//!
//! 2. scan_existing_windows()
//!    └─ EnumWindows → for each visible, top-level, titled window:
//!       ├─ get_window_info(hwnd)
//!       └─ register_window_from_info(info)
//!          ├─ classify_with_state_pipeline(candidate, is_max, is_fs, pipeline)
//!          └─ insert into HashMap
//!
//! 3. start_hook_thread()
//!    └─ background thread registers WinEvent hooks
//!       └─ sends HookEvent::Created/Destroyed/Foreground/MinimizeStart/MinimizeEnd
//!
//! 4. IPC loop:
//!    └─ each iteration:
//!       ├─ process_pending_events(receiver)  ← drain hook events
//!       └─ dispatch IPC command              ← handle CLI request
//! ```
//!
//! # State Transitions
//!
//! When a hook event arrives, the registry applies a state transition:
//!
//! | Event | Method | Transition |
//! |-------|--------|------------|
//! | `Created` | `handle_created` | New window → classify → register |
//! | `Destroyed` | `remove_window` | Remove from HashMap, clear focus if needed |
//! | `Foreground` | `set_focused` | Update `focused` field (only if tracked) |
//! | `MinimizeStart` | `minimize_window` | `Active` → `Minimized`, save virtual slot |
//! | `MinimizeEnd` | `restore_window` | `Minimized` → `Active`, restore virtual slot |
//!
//! # Design Decision: No Win32 in State Transitions
//!
//! All state transition methods (`minimize_window`, `restore_window`, etc.)
//! are **pure data transformations** on the `Window` struct. They do not call
//! any Win32 APIs. This is intentional:
//!
//! - State transitions only mutate the registry's in-memory state.
//! - Win32 calls (`SetWindowPos`, `MoveWindow`) are the compositor's job,
//!   not the registry's.
//! - This makes the registry's state machine testable without Win32 mocking.
//!
//! # Design Decision: Idempotent Registration
//!
//! `register_window_from_info` is idempotent —
//! if the window is already tracked, it returns early without modifying the
//! existing entry. This is critical because both the init scan and hook events
//! can race to register the same window. The first registration wins, and the
//! window's classification and state are preserved.

use std::collections::HashMap;

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GW_OWNER, GetWindow};

use crate::config::types::WindowRulesConfig;

use super::classification;
use super::hooks::HookEvent;
use super::types::{FloatingState, IgnoredReason, TilingState, VirtualSlot, Window, WindowState};
use super::win32;

/// The authoritative source of truth for every window the daemon is aware of.
///
/// `WindowRegistry` owns a `HashMap<isize, Window>` containing every tracked
/// window, along with the currently focused window handle and the classification
/// pipeline loaded from config.
///
/// # How Windows Enter and Leave the Registry
///
/// Windows enter via two paths:
/// 1. **Initialization scan** — [`scan_existing_windows`](Self::scan_existing_windows)
///    enumerates all current top-level windows via `EnumWindows`.
/// 2. **Live events** — [`process_pending_events`](Self::process_pending_events)
///    receives `HookEvent::Created` from the WinEvent hook thread.
///
/// Windows leave when `HookEvent::Destroyed` is received (via
/// [`remove_window`](Self::remove_window)).
///
/// # Threading
///
/// Shared via `Arc<Mutex<WindowRegistry>>` between:
/// - **IPC thread** — locks to process events and answer queries.
/// - **Hook thread** — never locks; sends events through an `mpsc` channel.
///
/// See the [module-level documentation](super) for the full threading diagram.
///
/// # Fields
///
/// - `windows` — The primary data store. Keyed by `HWND` as `isize` for
///   `Send` safety (see [module-level docs](super) for rationale).
/// - `focused` — The HWND value of the currently focused window, or `None`.
///   Only updated when the focused window is already tracked in the registry.
/// - `pipeline` — The multi-layer classification pipeline that combines
///   user rules, default rules, and the default action.
pub struct WindowRegistry {
    /// All tracked windows, keyed by HWND value (`isize` for `Send` safety).
    windows: HashMap<isize, Window>,

    /// Currently focused window handle value.
    focused: Option<isize>,

    /// Multi-layer classification pipeline (user rules → learned → default → fallback).
    pipeline: classification::ClassificationPipeline,
}

impl WindowRegistry {
    /// Creates a new empty registry with classification settings from both configs.
    ///
    /// # Arguments
    ///
    /// * `user_rules` - User-defined window rules from `stm-rules.yml`.
    /// * `default_rules` - Bundled default rules from `default-stm-rules.yml`.
    #[must_use]
    pub fn new(user_rules: &WindowRulesConfig, default_rules: &WindowRulesConfig) -> Self {
        Self {
            windows: HashMap::new(),
            focused: None,
            pipeline: classification::ClassificationPipeline::new(
                user_rules.clone(),
                default_rules.clone(),
            ),
        }
    }

    /// Scans all existing top-level windows and registers them.
    ///
    /// Called once at daemon startup to build the initial registry state
    /// **before** the hook thread starts. This ensures the registry is
    /// populated before any live events arrive.
    ///
    /// # Filtering
    ///
    /// Only registers windows that pass all three filters:
    /// 1. **Visible** — `IsWindowVisible(hwnd)` returns `true`.
    /// 2. **No owner** — `GetWindow(hwnd, GW_OWNER)` returns null (top-level only).
    /// 3. **Non-empty title** — `GetWindowTextW` returns a non-empty string.
    ///
    /// These filters exclude dialogs, popups, tool windows, and invisible
    /// containers (like the Windows desktop window).
    ///
    /// # Graceful Degradation
    ///
    /// If `EnumWindows` fails (e.g., on an isolated test desktop where the
    /// process lacks access), logs a warning and returns `Ok(())` — the
    /// hook thread will still catch windows via live events. This means the
    /// daemon can function even without a successful init scan.
    pub fn scan_existing_windows(&mut self) -> Result<(), String> {
        match enum_toplevel_windows() {
            Ok(hwnds) => {
                for hwnd in hwnds {
                    if !win32::is_window_visible(hwnd) {
                        continue;
                    }

                    let title = win32::get_window_text(hwnd).unwrap_or_default();
                    if title.is_empty() {
                        continue;
                    }

                    match win32::get_window_info(hwnd) {
                        Ok(info) => {
                            self.register_window_from_info(&info);
                            log::debug!("init: registered {} ({:?})", info.exe, hwnd);
                        }
                        Err(e) => {
                            log::warn!("init: skipping {:?}: {e}", hwnd);
                        }
                    }
                }
                log::info!("registry initialized with {} window(s)", self.windows.len());
            }
            Err(e) => {
                log::warn!(
                    "scan_existing_windows: EnumWindows failed ({e}), relying on event hooks"
                );
            }
        }
        Ok(())
    }

    /// Classifies and registers a window based on its Win32 metadata.
    ///
    /// This is the shared registration path used by both the init scan and
    /// live event handling. It:
    /// 1. Builds a [`WindowCandidate`](super::classification::WindowCandidate) from the window info.
    /// 2. Classifies it via [`classify_with_state`](super::classification::classify_with_state)
    ///    (applying rules, maximized/fullscreen overrides).
    /// 3. Creates a [`Window`] entry and inserts it into the HashMap.
    ///
    /// # Idempotency
    ///
    /// If the window is already registered, this is a **no-op** — the existing
    /// entry is preserved unchanged. This is critical because both the init scan
    /// and `EVENT_OBJECT_CREATE` can race to register the same window. The first
    /// registration wins.
    ///
    /// # Design: Why Separate Classification from Registration?
    ///
    /// Classification is done by the [`super::classification`] module,
    /// which is pure Rust with no Win32 dependencies. Registration is done here,
    /// where we have access to the HashMap. This separation means classification
    /// can be unit-tested without any Win32 mocking.
    pub fn register_window_from_info(&mut self, info: &win32::WindowInfo) {
        let key = hwnd_key(info.hwnd);
        if self.windows.contains_key(&key) {
            return;
        }

        let candidate = classification::WindowCandidate {
            exe: info.exe.clone(),
            title: info.title.clone(),
            class: info.class.clone(),
            process_path: info.process_path.clone(),
        };

        let state = classification::classify_with_state_pipeline(
            &candidate,
            info.is_maximized,
            info.is_fullscreen,
            &self.pipeline,
        );

        let window = Window::new(
            info.hwnd,
            info.exe.clone(),
            info.title.clone(),
            info.class.clone(),
            std::path::PathBuf::from(&info.process_path),
            info.rect,
            state,
        );

        self.windows.insert(key, window);
        log::info!("registered window: {:?} ({})", info.hwnd, info.exe);
    }

    /// Removes a window from the registry.
    ///
    /// Called when `EVENT_OBJECT_DESTROY` is received. If the removed window
    /// was focused, clears the `focused` field to `None`. This prevents a
    /// dangling focus reference to a destroyed window.
    ///
    /// # Note
    ///
    /// This only removes the window from the registry. It does **not** notify
    /// the layout engine — that coordination happens at a higher level in the
    /// daemon's event loop.
    pub fn remove_window(&mut self, hwnd_val: isize) {
        if let Some(window) = self.windows.remove(&hwnd_val) {
            log::info!("removed window: (isize={hwnd_val}) ({})", window.exe);
            if self.focused == Some(hwnd_val) {
                self.focused = None;
            }
        } else {
            log::debug!("remove_window: isize={hwnd_val} not in registry");
        }
    }

    /// Updates the focused window handle.
    ///
    /// Called when `EVENT_SYSTEM_FOREGROUND` is received. Only updates the
    /// `focused` field if the window is already tracked in the registry —
    /// untracked windows (e.g., system dialogs) are silently ignored.
    ///
    /// # Design Decision: Ignore Untracked Windows
    ///
    /// The Windows OS fires `EVENT_SYSTEM_FOREGROUND` for any window that
    /// gains focus, including windows stm doesn't manage (like the taskbar
    /// or system dialogs). By checking `contains_key`, we avoid recording
    /// focus for windows that aren't in our registry.
    pub fn set_focused(&mut self, hwnd_val: isize) {
        if self.windows.contains_key(&hwnd_val) {
            self.focused = Some(hwnd_val);
            log::debug!("focus changed: isize={hwnd_val}");
        } else {
            log::debug!("set_focused: isize={hwnd_val} not in registry, ignoring");
        }
    }

    /// Transitions a window to minimized state.
    ///
    /// Called when `EVENT_SYSTEM_MINIMIZESTART` is received. The transition
    /// depends on the window's current state:
    ///
    /// - **Tiling::Active { col, row }** → saves the virtual slot to
    ///   `last_virtual_slot`, transitions to `Tiling::Minimized`.
    /// - **Floating::Active { .. }** → transitions to `Floating::Minimized`.
    /// - **Already minimized or ignored** → no-op (returns early).
    ///
    /// # Design: Why Save the Virtual Slot?
    ///
    /// When a tiled window is minimized, its position in the layout grid
    /// (`col`, `row`) is saved into [`last_virtual_slot`](super::types::Window::last_virtual_slot).
    /// When the window is restored, this slot is used to place it back at
    /// its original position — the user doesn't lose their window arrangement.
    ///
    /// # Borrow Checker Workaround
    ///
    /// The col/row values are copied before the mutable borrow to avoid a
    /// borrow conflict: we need to read `window.state` and write to
    /// `window.last_virtual_slot` simultaneously.
    pub fn minimize_window(&mut self, hwnd_val: isize) {
        if let Some(window) = self.windows.get_mut(&hwnd_val) {
            // Copy col/row before mutating to avoid borrow conflict.
            let new_state = match &window.state {
                WindowState::Tiling(TilingState::Active { col, row }) => {
                    // Save virtual slot before transitioning.
                    let slot = VirtualSlot {
                        col: *col,
                        row: *row,
                    };
                    window.last_virtual_slot = Some(slot);
                    WindowState::Tiling(TilingState::Minimized)
                }
                WindowState::Floating(FloatingState::Active { .. }) => {
                    WindowState::Floating(FloatingState::Minimized)
                }
                _ => return, // Already minimized or ignored — no-op.
            };
            window.state = new_state;
            log::info!("minimized window: isize={hwnd_val}");
        }
    }

    /// Transitions a minimized window back to active state.
    ///
    /// Called when `EVENT_SYSTEM_MINIMIZEEND` is received. The restore logic
    /// depends on the window's previous state:
    ///
    /// - **Tiling::Minimized** → restores to `Tiling::Active { col, row }` using
    ///   the saved `last_virtual_slot`. If no slot was saved (edge case),
    ///   defaults to `(0, 0)`.
    /// - **Floating::Minimized** → restores to `Floating::Active { rect }` using
    ///   the `pre_manage_rect` (the window's position before stm managed it).
    /// - **Not minimized** → no-op (returns early).
    ///
    /// # Design: Why Default to (0, 0)?
    ///
    /// If `last_virtual_slot` is `None` (which shouldn't happen in normal
    /// operation but could occur due to a race), we default to column 0,
    /// row 0. This ensures the window always gets a valid tiling position
    /// rather than being lost.
    pub fn restore_window(&mut self, hwnd_val: isize) {
        if let Some(window) = self.windows.get_mut(&hwnd_val) {
            let new_state = match &window.state {
                WindowState::Tiling(TilingState::Minimized) => {
                    // Restore to the saved virtual slot, or default position.
                    let (col, row) = window
                        .last_virtual_slot
                        .as_ref()
                        .map(|s| (s.col, s.row))
                        .unwrap_or((0, 0));
                    WindowState::Tiling(TilingState::Active { col, row })
                }
                WindowState::Floating(FloatingState::Minimized) => {
                    let rect = window.pre_manage_rect;
                    WindowState::Floating(FloatingState::Active { rect })
                }
                _ => return, // Not minimized — no-op.
            };
            window.state = new_state;
            log::info!("restored window: isize={hwnd_val}");
        }
    }

    /// Returns an immutable reference to a tracked window.
    #[must_use]
    pub fn get_window(&self, hwnd: HWND) -> Option<&Window> {
        self.windows.get(&hwnd_key(hwnd))
    }

    /// Returns an iterator over all tracked windows.
    pub fn windows(&self) -> impl Iterator<Item = &Window> {
        self.windows.values()
    }

    /// Returns the number of tracked windows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Returns `true` if the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Serializes the registry state to a JSON value for the query API.
    ///
    /// Returns a JSON object with:
    /// - `windows`: array of window state objects
    /// - `focused`: the focused HWND value (or null)
    /// - `count`: number of tracked windows
    #[must_use]
    pub fn to_json_value(&self) -> serde_json::Value {
        let windows_json: Vec<serde_json::Value> = self
            .windows
            .values()
            .map(|w| {
                serde_json::json!({
                    "hwnd": w.hwnd.0 as isize,
                    "exe": w.exe,
                    "title": w.title,
                    "class": w.class,
                    "process_path": w.process_path.to_string_lossy(),
                    "state": state_to_json(&w.state),
                    "rect": serde_json::json!({
                        "x": w.pre_manage_rect.x,
                        "y": w.pre_manage_rect.y,
                        "width": w.pre_manage_rect.width,
                        "height": w.pre_manage_rect.height,
                    }),
                })
            })
            .collect();

        serde_json::json!({
            "windows": windows_json,
            "focused": self.focused,
            "count": self.windows.len(),
        })
    }

    /// Drains all pending hook events and applies them to the registry.
    ///
    /// This is the **main event dispatch loop** for the registry. It must be
    /// called periodically from the IPC thread (which owns the `MutexGuard`)
    /// to process events queued by the WinEvent hook thread.
    ///
    /// Uses `try_recv` for non-blocking operation — if no events are pending,
    /// returns immediately. This ensures the IPC loop never blocks waiting
    /// for events; it processes whatever is available and moves on to the
    /// next IPC command.
    ///
    /// # Event Dispatch Table
    ///
    /// | Event | Handler | Effect |
    /// |-------|---------|--------|
    /// | `Created` | `handle_created` | Query info → classify → register |
    /// | `Destroyed` | [`remove_window`](Self::remove_window) | Remove from HashMap, clear focus |
    /// | `Foreground` | [`set_focused`](Self::set_focused) | Update focused HWND |
    /// | `MinimizeStart` | [`minimize_window`](Self::minimize_window) | Active → Minimized, save slot |
    /// | `MinimizeEnd` | [`restore_window`](Self::restore_window) | Minimized → Active, restore slot |
    pub fn process_pending_events(&mut self, receiver: &std::sync::mpsc::Receiver<HookEvent>) {
        while let Ok(event) = receiver.try_recv() {
            match event {
                HookEvent::Created { hwnd } => {
                    self.handle_created(hwnd);
                }
                HookEvent::Destroyed { hwnd } => {
                    self.remove_window(hwnd);
                }
                HookEvent::Foreground { hwnd } => {
                    self.set_focused(hwnd);
                }
                HookEvent::MinimizeStart { hwnd } => {
                    self.minimize_window(hwnd);
                }
                HookEvent::MinimizeEnd { hwnd } => {
                    self.restore_window(hwnd);
                }
            }
        }
    }

    /// Handles a window creation event from the WinEvent hook.
    ///
    /// This is the live-event counterpart to the init scan's
    /// [`scan_existing_windows`](Self::scan_existing_windows). It:
    /// 1. Skips if already tracked (idempotent, same as init scan).
    /// 2. Skips invisible windows (no `WS_VISIBLE` style).
    /// 3. Skips windows with empty titles (background containers).
    /// 4. Skips windows with an owner (dialogs, popups — not top-level).
    /// 5. Gathers full window info via [`win32::get_window_info`].
    /// 6. Delegates to [`register_window_from_info`](Self::register_window_from_info).
    ///
    /// # Why Re-check Visibility and Title?
    ///
    /// The init scan checks these same conditions, but we re-check here because
    /// a window's state can change between the init scan and the first
    /// `EVENT_OBJECT_CREATE`. A window might be created invisible and then shown,
    /// or shown and then hidden. The WinEvent hook catches the creation event
    /// regardless, so we filter at registration time.
    fn handle_created(&mut self, hwnd_val: isize) {
        if self.windows.contains_key(&hwnd_val) {
            return; // Already tracked.
        }

        let hwnd = HWND(hwnd_val as *mut _);

        // Only manage visible windows with titles.
        if !win32::is_window_visible(hwnd) {
            return;
        }

        let title = win32::get_window_text(hwnd).unwrap_or_default();
        if title.is_empty() {
            return;
        }

        // Skip windows with an owner (dialogs, popups).
        if has_owner(hwnd) {
            return;
        }

        match win32::get_window_info(hwnd) {
            Ok(info) => {
                self.register_window_from_info(&info);
            }
            Err(e) => {
                log::warn!("handle_created: failed to get info for {:?}: {e}", hwnd);
            }
        }
    }
}

/// Converts an `HWND` to the `isize` key used in the HashMap.
///
/// This is the bridge between the Win32 world (`HWND` = `*mut c_void`) and
/// the registry's HashMap (`isize` keys). The conversion is lossless because
/// HWND values fit within `isize` on both 32-bit and 64-bit Windows.
///
/// # Why `isize` and not `HWND`?
///
/// `HWND` wraps a raw pointer and is `!Send`. Storing `isize` allows the
/// registry to be shared across threads via `Arc<Mutex<>>`, and allows
/// window IDs to be sent through `mpsc` channels in [`HookEvent`](super::HookEvent).
fn hwnd_key(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

/// Converts a `WindowState` to a JSON value for the query API.
///
/// This produces a human-readable JSON representation of each state variant,
/// suitable for the `QueryWindowsAll` IPC command. The format uses nested
/// objects to match the Rust enum structure (e.g., `{"Tiling": {"Active": ...}}`).
fn state_to_json(state: &WindowState) -> serde_json::Value {
    match state {
        WindowState::Tiling(TilingState::Active { col, row }) => {
            serde_json::json!({"Tiling": {"Active": {"col": col, "row": row}}})
        }
        WindowState::Tiling(TilingState::Minimized) => {
            serde_json::json!("Tiling::Minimized")
        }
        WindowState::Floating(FloatingState::Active { rect }) => serde_json::json!({
            "Floating": {"Active": {"rect": {"x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height}}}
        }),
        WindowState::Floating(FloatingState::Minimized) => {
            serde_json::json!("Floating::Minimized")
        }
        WindowState::Ignored(IgnoredReason::Maximized) => {
            serde_json::json!("Ignored::Maximized")
        }
        WindowState::Ignored(IgnoredReason::Fullscreen) => {
            serde_json::json!("Ignored::Fullscreen")
        }
        WindowState::Ignored(IgnoredReason::ExplicitRule) => {
            serde_json::json!("Ignored::ExplicitRule")
        }
    }
}

/// Checks if a window has an owner (i.e., is not top-level).
///
/// Many Win32 windows are actually child windows or owned dialogs. We only
/// track top-level windows (those without an owner) because:
/// - Owned windows (dialogs, popups) have their position managed by their owner.
/// - Including them would double-count application windows.
///
/// `GetWindow(hwnd, GW_OWNER)` may return `Ok(HWND(null))` for ownerless
/// windows, so we must check the handle value, not just `is_ok()`.
fn has_owner(hwnd: HWND) -> bool {
    match unsafe { GetWindow(hwnd, GW_OWNER) } {
        Ok(owner) => !owner.is_invalid(),
        Err(_) => false,
    }
}

/// Enumerates all top-level windows using `EnumWindows`.
///
/// `EnumWindows` calls the provided callback once for each top-level window
/// in the system. We pass a raw pointer to a `Vec<HWND>` through `LPARAM`
/// to collect all handles.
///
/// Returns a `Vec<HWND>` of all top-level window handles, which is then
/// filtered by [`scan_existing_windows`](WindowRegistry::scan_existing_windows).
///
/// # Errors
///
/// Returns an error string if `EnumWindows` fails (extremely rare — typically
/// only happens in sandboxed environments or during system shutdown).
///
/// # Safety
///
/// The `LPARAM` carries a raw pointer to a `Vec<HWND>` allocated on the
/// caller's stack. The callback dereferences this pointer — the caller must
/// ensure the `Vec` outlives the enumeration.
fn enum_toplevel_windows() -> Result<Vec<HWND>, String> {
    let mut collected: Vec<HWND> = Vec::new();
    let ptr = &mut collected as *mut Vec<HWND>;

    // SAFETY: EnumWindows calls our callback once per top-level window.
    // We pass a raw pointer to our Vec<HWND> through LPARAM.
    let result = unsafe { EnumWindows(Some(enum_windows_callback), LPARAM(ptr as isize)) };

    result.map_err(|e| format!("EnumWindows failed: {e}"))?;

    Ok(collected)
}

/// Callback for `EnumWindows`. Appends each `HWND` to the `Vec` passed via `LPARAM`.
///
/// This is called by Windows once per top-level window. The return value
/// controls whether enumeration continues:
/// - `BOOL(1)` → continue enumerating.
/// - `BOOL(0)` → stop enumerating (we never do this).
///
/// # Safety
///
/// The `l_param` must be a valid pointer to a `Vec<HWND>` created by the
/// caller in [`enum_toplevel_windows`]. The pointer is valid for the duration
/// of the `EnumWindows` call because the `Vec` is on the caller's stack.
///
/// This function is `extern "system"` (stdcall calling convention) as required
/// by the Win32 `EnumWindows` API.
unsafe extern "system" fn enum_windows_callback(
    hwnd: HWND,
    l_param: LPARAM,
) -> windows::core::BOOL {
    // SAFETY: l_param is a valid pointer to Vec<HWND> created in enum_toplevel_windows.
    let vec = unsafe { &mut *(l_param.0 as *mut Vec<HWND>) };
    vec.push(hwnd);
    windows::core::BOOL(1) // Continue enumeration.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{WindowAction, WindowRulesConfig};

    /// Helper to create a default user rules config and empty default rules.
    fn default_rules() -> (WindowRulesConfig, WindowRulesConfig) {
        (WindowRulesConfig::default(), WindowRulesConfig::default())
    }

    #[test]
    fn new_registry_is_empty() {
        let (user, default) = default_rules();
        let reg = WindowRegistry::new(&user, &default);
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.focused.is_none());
    }

    #[test]
    fn to_json_value_empty_registry() {
        let (user, default) = default_rules();
        let reg = WindowRegistry::new(&user, &default);
        let json = reg.to_json_value();

        assert_eq!(json["count"], 0);
        assert!(json["windows"].as_array().unwrap().is_empty());
        assert!(json["focused"].is_null());
    }

    #[test]
    fn to_json_value_has_correct_structure() {
        let (user, default) = default_rules();
        let reg = WindowRegistry::new(&user, &default);
        let json = reg.to_json_value();

        assert!(json.get("windows").is_some());
        assert!(json.get("focused").is_some());
        assert!(json.get("count").is_some());
    }

    #[test]
    fn remove_window_clears_focus() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        let hwnd_val = 12345isize;
        let hwnd = HWND(hwnd_val as *mut _);

        // Manually insert a window for testing.
        let window = Window::new(
            hwnd,
            "test.exe".into(),
            "Test".into(),
            "TestClass".into(),
            std::path::PathBuf::from("C:\\test.exe"),
            crate::common::Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        reg.windows.insert(hwnd_val, window);
        reg.focused = Some(hwnd_val);

        assert_eq!(reg.len(), 1);
        assert_eq!(reg.focused, Some(hwnd_val));

        reg.remove_window(hwnd_val);
        assert!(reg.is_empty());
        assert!(reg.focused.is_none());
    }

    #[test]
    fn minimize_and_restore_tiling_window() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        let hwnd_val = 99999isize;
        let hwnd = HWND(hwnd_val as *mut _);

        let window = Window::new(
            hwnd,
            "app.exe".into(),
            "App".into(),
            "AppClass".into(),
            std::path::PathBuf::from("C:\\app.exe"),
            crate::common::Rect {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
            },
            WindowState::Tiling(TilingState::Active { col: 2, row: 1 }),
        );
        reg.windows.insert(hwnd_val, window);

        // Minimize.
        reg.minimize_window(hwnd_val);
        let w = reg.windows.get(&hwnd_val).unwrap();
        assert!(matches!(
            w.state,
            WindowState::Tiling(TilingState::Minimized)
        ));
        assert_eq!(
            w.last_virtual_slot.as_ref().map(|s| (s.col, s.row)),
            Some((2, 1))
        );

        // Restore.
        reg.restore_window(hwnd_val);
        let w = reg.windows.get(&hwnd_val).unwrap();
        assert!(matches!(
            w.state,
            WindowState::Tiling(TilingState::Active { col: 2, row: 1 })
        ));
    }

    // ── Helper to insert a test window ───────────────────────────────

    /// Inserts a minimal window into the registry for testing.
    fn insert_test_window(reg: &mut WindowRegistry, hwnd_val: isize, state: WindowState) {
        let hwnd = HWND(hwnd_val as *mut _);
        let window = Window::new(
            hwnd,
            "test.exe".into(),
            format!("Test-{hwnd_val}"),
            "TestClass".into(),
            std::path::PathBuf::from("C:\\test.exe"),
            crate::common::Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            state,
        );
        reg.windows.insert(hwnd_val, window);
    }

    // ── set_focused tests ────────────────────────────────────────────

    #[test]
    fn set_focused_on_tracked_window() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        let hwnd_val = 42isize;

        insert_test_window(
            &mut reg,
            hwnd_val,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );

        assert!(reg.focused.is_none());
        reg.set_focused(hwnd_val);
        assert_eq!(reg.focused, Some(hwnd_val));
    }

    #[test]
    fn set_focused_ignores_untracked_window() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        // No windows in registry — focus should remain None.
        reg.set_focused(99999);
        assert!(reg.focused.is_none());
    }

    #[test]
    fn set_focused_changes_between_windows() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        insert_test_window(
            &mut reg,
            10,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        insert_test_window(
            &mut reg,
            20,
            WindowState::Tiling(TilingState::Active { col: 1, row: 0 }),
        );

        reg.set_focused(10);
        assert_eq!(reg.focused, Some(10));

        reg.set_focused(20);
        assert_eq!(reg.focused, Some(20));
    }

    // ── register_window_from_info tests ──────────────────────────────

    #[test]
    fn register_window_from_info_inserts_new_window() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        let hwnd_val = 5555isize;

        let info = win32::WindowInfo {
            hwnd: HWND(hwnd_val as *mut _),
            title: "MyApp".to_owned(),
            class: "AppClass".to_owned(),
            rect: crate::common::Rect {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
            exe: "myapp.exe".to_owned(),
            process_path: "C:\\myapp.exe".to_owned(),
            is_visible: true,
            is_maximized: false,
            is_fullscreen: false,
        };

        assert!(reg.is_empty());
        reg.register_window_from_info(&info);
        assert_eq!(reg.len(), 1);

        let w = reg.get_window(HWND(hwnd_val as *mut _)).unwrap();
        assert_eq!(w.exe, "myapp.exe");
        assert_eq!(w.title, "MyApp");
    }

    #[test]
    fn register_window_from_info_is_noop_for_existing() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        let hwnd_val = 7777isize;

        let info = win32::WindowInfo {
            hwnd: HWND(hwnd_val as *mut _),
            title: "Original".to_owned(),
            class: "TestClass".to_owned(),
            rect: crate::common::Rect {
                x: 0,
                y: 0,
                width: 640,
                height: 480,
            },
            exe: "app.exe".to_owned(),
            process_path: "C:\\app.exe".to_owned(),
            is_visible: true,
            is_maximized: false,
            is_fullscreen: false,
        };

        reg.register_window_from_info(&info);
        assert_eq!(reg.len(), 1);

        // Register again with different title — should be a no-op.
        let info2 = win32::WindowInfo {
            title: "Changed".to_owned(),
            ..info.clone()
        };
        reg.register_window_from_info(&info2);
        assert_eq!(reg.len(), 1);

        // Original title should be preserved.
        let w = reg.get_window(HWND(hwnd_val as *mut _)).unwrap();
        assert_eq!(w.title, "Original");
    }

    #[test]
    fn register_window_from_info_maximized_becomes_ignored() {
        let user = WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![],
        };
        let default = WindowRulesConfig::default();
        let mut reg = WindowRegistry::new(&user, &default);

        let info = win32::WindowInfo {
            hwnd: HWND(8888 as *mut _),
            title: "MaxApp".to_owned(),
            class: "MaxClass".to_owned(),
            rect: crate::common::Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            exe: "maxapp.exe".to_owned(),
            process_path: "C:\\maxapp.exe".to_owned(),
            is_visible: true,
            is_maximized: true,
            is_fullscreen: false,
        };

        reg.register_window_from_info(&info);
        let w = reg.get_window(HWND(8888 as *mut _)).unwrap();
        assert!(matches!(
            w.state,
            WindowState::Ignored(IgnoredReason::Maximized)
        ));
    }

    // ── process_pending_events tests ─────────────────────────────────

    #[test]
    fn process_pending_events_handles_created() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        let (tx, rx) = std::sync::mpsc::channel();
        // Created event for an HWND that doesn't exist — will be ignored
        // because we can't query real window info in unit tests.
        tx.send(HookEvent::Created { hwnd: 0 }).unwrap();

        // Should not panic on unknown HWNDs.
        reg.process_pending_events(&rx);
        assert!(reg.is_empty());
    }

    #[test]
    fn process_pending_events_handles_destroyed() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        let hwnd_val = 33isize;

        insert_test_window(
            &mut reg,
            hwnd_val,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(HookEvent::Destroyed { hwnd: hwnd_val }).unwrap();

        reg.process_pending_events(&rx);
        assert!(reg.is_empty());
    }

    #[test]
    fn process_pending_events_handles_foreground() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        let hwnd_val = 44isize;

        insert_test_window(
            &mut reg,
            hwnd_val,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(HookEvent::Foreground { hwnd: hwnd_val }).unwrap();

        reg.process_pending_events(&rx);
        assert_eq!(reg.focused, Some(hwnd_val));
    }
}
