//! Core window registry — authoritative source of truth for all tracked windows.
//!
//! [`WindowRegistry`] is the central data structure tracking every window the
//! daemon is aware of: a `HashMap<isize, Window>` keyed by the Win32 `HWND`
//! value stored as `isize`. Storing the handle as `isize` (rather than `HWND`
//! directly) is deliberate — `HWND` wraps `*mut c_void` and is `!Send`, while
//! `isize` is `Send + Sync + Hash + Eq` and works as a HashMap key that can be
//! sent through channels (`hwnd.0 as isize` / `HWND(val as *mut _)`).
//!
//! # Threading model
//!
//! The registry is owned directly by
//! [`FlowWM`](crate::daemon::FlowWM) — **no
//! `Arc<Mutex<>>`** wrapping. The IPC thread owns it; the background hook thread
//! never accesses any flow field — it only sends typed [`HookEvent`]s through a
//! non-blocking `mpsc` channel. The IPC thread drains that channel and dispatches
//! to handler methods. All HWND dereferencing happens on the IPC thread. Because
//! handlers take `&mut self`, the borrow checker enforces exclusive access at
//! compile time — no locks, no deadlocks.
//!
//! # Initialization
//!
//! Construction builds a [`ClassificationPipeline`] from user + default rules,
//! then `scan_existing_windows()` runs `EnumWindows` and registers every
//! visible/titled/Alt+Tab-visible top-level window (pre-filtering before
//! classification), then `start_hook_thread()` registers the WinEvent hooks.
//! Thereafter the IPC loop drains the channel and dispatches per event.
//!
//! # State transitions
//!
//! | Event | Method | Transition |
//! |-------|--------|------------|
//! | `Created` | `handle_created` | New window → classify → register |
//! | `Destroyed` | `remove_window` | Remove from HashMap, clear focus if needed |
//! | `Foreground` | `set_focused` | Update `focused` field (only if tracked) |
//! | `MinimizeStart` | `minimize_window` | `Active` → `Minimized`, save virtual slot |
//! | `MinimizeEnd` | `restore_window` | `Minimized` → `Active`, restore virtual slot |
//!
//! # Design decisions
//!
//! - **No Win32 in state transitions**: `minimize_window`, `restore_window`,
//!   etc. are pure data transformations on the `Window` struct — they call no
//!   Win32 APIs. `SetWindowPos`/`MoveWindow` are the animator's job, not the
//!   registry's, so the state machine is testable without Win32 mocking.
//! - **Idempotent registration**: `register_window_from_info` returns early if
//!   the window is already tracked — the init scan and hook events can race to
//!   register the same window, and the first registration wins.
//!
//! See the developer guide's *Window Registry* chapter
//! (`docs/src/dev-guide/window-registry.md`) for the classification pipeline and
//! lifecycle state machine with diagrams.

use std::collections::HashMap;

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_STYLE, GetWindow, GetWindowLongW, WINDOW_STYLE, WS_CHILD,
};

use crate::common::{InvisibleBounds, Rect};
use crate::config::types::{WindowRule, WindowRulesConfig};

use super::classification;
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
/// 2. **Live events** — [`handle_created`](Self::handle_created)
///    is called when `HookEvent::Created` arrives from the WinEvent hook thread.
///
/// Windows leave when `HookEvent::Destroyed` is received (via
/// [`remove_window`](Self::remove_window)).
///
/// # Threading
///
/// Owned directly by [`FlowWM`](crate::daemon::FlowWM)
/// — no `Arc<Mutex<>>` wrapping. The orchestrator drains hook events from the
/// mpsc channel on the IPC thread and calls registry methods via `&mut self`.
///
/// The hook thread never accesses registry fields; it only sends
/// [`HookEvent`]s through the channel.
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
    /// * `user_rules` - User-defined window rules from `flow-rules.toml`.
    /// * `default_rules` - Bundled default rules (embedded at compile time from
    ///   `default-flow-rules.toml`).
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
    /// Only registers windows that pass all filters:
    /// 1. **Visible** — `IsWindowVisible(hwnd)` returns `true`.
    /// 2. **Non-empty title** — `GetWindowTextW` returns a non-empty string.
    /// 3. **Alt+Tab visible** — Window would appear in the Alt+Tab switcher
    ///    (checks `WS_EX_TOOLWINDOW` / `WS_EX_APPWINDOW` extended styles).
    ///    This automatically excludes background helper windows, tray icons,
    ///    and tool windows that the user never interacts with directly.
    /// 4. **No owner and no parent** — `GetWindow(hwnd, GW_OWNER)` returns null
    ///    (not an owned dialog/popup) AND `has_parent(hwnd)` returns false
    ///    (not a `WS_CHILD` control). The owner check filters owned dialogs;
    ///    the parent check filters `WS_CHILD` controls (buttons, labels, Inno
    ///    Setup `TNew*`) embedded inside a dialog.
    ///
    /// These filters exclude dialogs, popups, tool windows, invisible
    /// containers (like the Windows desktop window), and background helper
    /// applications (like `GearLink_KBAgent.exe`, `SystemSettings.exe`).
    ///
    /// # Filter Order (Performance)
    ///
    /// The filters are ordered from cheapest to most expensive:
    /// 1. Visibility (`IsWindowVisible`) — single Win32 call, no string alloc.
    /// 2. Title length (`GetWindowTextLengthW`) — single Win32 call, no string alloc.
    /// 3. Alt+Tab visibility (`GetWindowLongW(GWL_EXSTYLE)`) — single Win32 call.
    /// 4. Owner check (`GetWindow(GW_OWNER)`) — single Win32 call.
    /// 5. Parent check (`WS_CHILD` style bit via `GetWindowLongW(GWL_STYLE)`) —
    ///    single Win32 call.
    /// 6. Full `get_window_info()` — multiple Win32 calls (expensive).
    ///
    /// Early termination on cheap filters avoids expensive process queries
    /// for windows that would be discarded anyway.
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

                    // Skip windows that wouldn't appear in Alt+Tab (tool windows,
                    // tray icons, background helpers like GearLink_KBAgent.exe).
                    if !win32::is_alt_tab_visible(hwnd) {
                        log::debug!("init: skipping {:?} — not Alt+Tab visible", hwnd);
                        continue;
                    }

                    // Skip iconic (minimized) windows — `IsWindowVisible` returns
                    // true for minimized windows, so this check is needed to
                    // exclude them from the initial scan. They will be caught
                    // by live MinimizeStart/MinimizeEnd events instead.
                    if win32::is_iconic(hwnd) {
                        log::debug!("init: skipping {:?} — iconic (minimized)", hwnd);
                        continue;
                    }

                    // Skip owned windows (dialogs, popups) and child windows (controls inside
                    // a dialog — e.g. buttons, labels). See has_owner and has_parent.
                    if has_owner(hwnd) || has_parent(hwnd) {
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
    /// 2. Classifies it via [`classify_with_state_pipeline`](super::classification::classify_with_state_pipeline)
    ///    (applying pipeline rules, maximized/fullscreen overrides).
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

        let invisible_bounds = win32::get_invisible_bounds(info.hwnd);

        let window = Window::new(
            info.hwnd,
            info.exe.clone(),
            info.title.clone(),
            info.class.clone(),
            std::path::PathBuf::from(&info.process_path),
            info.rect,
            state,
            invisible_bounds,
        );

        self.windows.insert(key, window);
        log::info!(
            "registered window: {:?} ({}) class={:?} title={:?}",
            info.hwnd,
            info.exe,
            info.class,
            info.title,
        );
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
    /// gains focus, including windows flow doesn't manage (like the taskbar
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

    /// Returns the OS-focused window, if any.
    ///
    /// This is the registry-level focus owned by the OS foreground event
    /// handler (`set_focused`), distinct from the per-space tiled history
    /// cursor in `ScrollingSpace::last_focused_window`. Returns `None`
    /// when no tracked window is focused.
    ///
    /// The daemon's border subsystem uses this to resolve per-window border
    /// colors (focused vs unfocused) without re-querying the OS — `set_focused`
    /// already filters out untracked windows, so this value is always a tracked
    /// HWND or `None`.
    ///
    /// See the Window Registry chapter (`docs/src/dev-guide/window-registry.md`).
    #[must_use]
    pub fn focused(&self) -> Option<crate::common::WindowId> {
        self.focused.map(crate::common::WindowId)
    }

    /// Replace the classification pipeline's learned-rules layer.
    ///
    /// See (`docs/src/dev-guide/classification.md`).
    pub fn set_learned_rules(&mut self, rules: Vec<WindowRule>) {
        self.pipeline.set_learned_rules(rules);
    }

    /// Replace the classification pipeline's user-rules layer.
    ///
    /// Used by hot-reload so edited `flow-rules.toml` rules take effect without
    /// a restart. See (`docs/src/dev-guide/config-and-persistence.md`).
    pub fn set_user_rules(&mut self, user_rules: WindowRulesConfig) {
        self.pipeline.set_user_rules(user_rules);
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
    ///   the `pre_manage_rect` (the window's position before flow managed it).
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

    /// Reconciles a tracked window's stored state against its actual Win32
    /// visibility, transitioning it to/from `Hidden` as needed.
    ///
    /// **Idempotent and state-based** (not event-based): computes the window's
    /// real visibility from Win32 (`is_window_visible && !is_cloaked &&
    /// !is_iconic`) and transitions only when the stored state disagrees. This
    /// means duplicate `EVENT_OBJECT_HIDE` events (e.g. an ordinary minimize
    /// also fires HIDE) collapse to [`Unchanged`](super::types::VisibilityChange::Unchanged)
    /// once the window is already non-active.
    ///
    /// # Transitions
    ///
    /// - `Tiling(Active)` + not user-visible → save slot to `last_virtual_slot`,
    ///   set `Tiling(Hidden)`, return `Hidden`.
    /// - `Floating(Active)` + not user-visible → set `Floating(Hidden)`,
    ///   return `Hidden`.
    /// - `Tiling(Hidden)` + user-visible → restore slot from `last_virtual_slot`
    ///   (default `(0, 0)` if absent, mirroring `restore_window`), set
    ///   `Tiling(Active{col,row})`, return `Shown`.
    /// - `Floating(Hidden)` + user-visible → restore `pre_manage_rect`, set
    ///   `Floating(Active{rect})`, return `Shown`.
    /// - Untracked, `Ignored`, `Minimized`, or already-matching → `Unchanged`.
    ///
    /// # Arguments
    ///
    /// * `hwnd_val` — Win32 window handle value (the HashMap key).
    ///
    /// # Borrow Checker Note
    ///
    /// When transitioning from `Tiling(Active)` to `Tiling(Hidden)`, we must
    /// copy `col`/`row` out of the current state before mutating
    /// `window.last_virtual_slot` and `window.state`. This mirrors the pattern
    /// used in [`minimize_window`](Self::minimize_window).
    #[must_use]
    pub fn reconcile_visibility(&mut self, hwnd_val: isize) -> super::types::VisibilityChange {
        use super::types::VisibilityChange;

        // Compute user-visible ONCE from Win32 state.
        let h = HWND(hwnd_val as *mut _);
        let user_visible =
            win32::is_window_visible(h) && !win32::is_cloaked(h) && !win32::is_iconic(h);

        let Some(window) = self.windows.get_mut(&hwnd_val) else {
            return VisibilityChange::Unchanged;
        };

        if !user_visible {
            // Window is not user-visible — transition Active → Hidden if applicable.
            let new_state = match &window.state {
                WindowState::Tiling(TilingState::Active { col, row }) => {
                    // Copy col/row before mutating (same pattern as minimize_window).
                    let slot = VirtualSlot {
                        col: *col,
                        row: *row,
                    };
                    window.last_virtual_slot = Some(slot);
                    WindowState::Tiling(TilingState::Hidden)
                }
                WindowState::Floating(FloatingState::Active { .. }) => {
                    WindowState::Floating(FloatingState::Hidden)
                }
                _ => return VisibilityChange::Unchanged,
                // Already Minimized, Hidden, or Ignored — nothing to do.
            };
            window.state = new_state;
            log::info!("reconcile_visibility: window isize={hwnd_val} → Hidden");
            VisibilityChange::Hidden
        } else {
            // Window is user-visible — transition Hidden → Active if applicable.
            let new_state = match &window.state {
                WindowState::Tiling(TilingState::Hidden) => {
                    // Restore from saved virtual slot, defaulting to (0, 0)
                    // (mirrors restore_window).
                    let (col, row) = window
                        .last_virtual_slot
                        .as_ref()
                        .map(|s| (s.col, s.row))
                        .unwrap_or((0, 0));
                    WindowState::Tiling(TilingState::Active { col, row })
                }
                WindowState::Floating(FloatingState::Hidden) => {
                    let rect = window.pre_manage_rect;
                    WindowState::Floating(FloatingState::Active { rect })
                }
                _ => return VisibilityChange::Unchanged,
                // Already Active, Minimized, or Ignored — nothing to do.
            };
            window.state = new_state;
            log::info!("reconcile_visibility: window isize={hwnd_val} → Shown");
            VisibilityChange::Shown
        }
    }

    /// Returns an immutable reference to a tracked window.
    #[must_use]
    pub fn get_window(&self, hwnd: HWND) -> Option<&Window> {
        self.windows.get(&hwnd_key(hwnd))
    }

    /// Returns a mutable reference to a tracked window.
    ///
    /// Used by the daemon to transition a window between Tiling and Floating
    /// states (`docs/src/dev-guide/floating-space.md`).
    #[must_use]
    pub fn get_window_mut(&mut self, hwnd: HWND) -> Option<&mut Window> {
        self.windows.get_mut(&hwnd_key(hwnd))
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
    /// - `windows`: array of window state objects, each containing:
    ///   - Basic metadata (hwnd, exe, title, class, process_path)
    ///   - `state`: lifecycle state with correct col/row indices
    ///   - `tiled_rect`: current layout-engine position (or null if not tiled)
    ///   - `window_rect`: current full window position from Windows OS (via `GetWindowRect`),
    ///     or null if the window cannot be queried (destroyed, minimized, etc.)
    ///   - `visible_rect`: the user-visible portion of the window, derived from
    ///     `window_rect` minus the stored invisible borders, or null if `window_rect`
    ///     is unavailable
    ///   - `invisible_bounds`: per-edge invisible border sizes (left, top, right, bottom)
    ///   - `pre_manage_rect`: position before flow touched the window
    /// - `viewport_offset`: camera position on the virtual canvas
    /// - `focused`: the focused HWND value (or null)
    /// - `count`: number of tracked windows
    ///
    /// # Arguments
    ///
    /// * `viewport_offset` — The current camera position from the layout engine,
    ///   included in the response for diagnostic purposes (e.g., debugging centering).
    #[must_use]
    pub fn to_json_value(&self, viewport_offset: i32) -> serde_json::Value {
        let windows_json: Vec<serde_json::Value> = self
            .windows
            .values()
            .map(|w| {
                let tiled_rect_json = w.tiled_rect.map(|r| {
                    serde_json::json!({
                        "x": r.x,
                        "y": r.y,
                        "width": r.width,
                        "height": r.height,
                    })
                });

                // Query the actual window position from Windows OS (full rect
                // including invisible borders).
                let window_rect_json = win32::get_window_rect(w.hwnd).ok().map(|r| {
                    serde_json::json!({
                        "x": r.x,
                        "y": r.y,
                        "width": r.width,
                        "height": r.height,
                    })
                });

                // Compute the visible rect from the window rect using stored
                // invisible bounds (or null if no window rect available).
                let visible_rect_json = window_rect_json.as_ref().and_then(|wr| {
                    let wr_obj = wr.as_object()?;
                    let x = wr_obj.get("x")?.as_i64()?;
                    let y = wr_obj.get("y")?.as_i64()?;
                    let width = wr_obj.get("width")?.as_i64()?;
                    let height = wr_obj.get("height")?.as_i64()?;
                    let visible = w.invisible_bounds.window_to_visible(Rect {
                        x: x as i32,
                        y: y as i32,
                        width: width as i32,
                        height: height as i32,
                    });
                    Some(serde_json::json!({
                        "x": visible.x,
                        "y": visible.y,
                        "width": visible.width,
                        "height": visible.height,
                    }))
                });

                serde_json::json!({
                    "hwnd": w.hwnd.0 as isize,
                    "exe": w.exe,
                    "title": w.title,
                    "class": w.class,
                    "process_path": w.process_path.to_string_lossy(),
                    "state": state_to_json(&w.state),
                    "tiled_rect": tiled_rect_json,
                    "window_rect": window_rect_json,
                    "visible_rect": visible_rect_json,
                    "invisible_bounds": serde_json::json!({
                        "left": w.invisible_bounds.left,
                        "top": w.invisible_bounds.top,
                        "right": w.invisible_bounds.right,
                        "bottom": w.invisible_bounds.bottom,
                    }),
                    "pre_manage_rect": serde_json::json!({
                        "x": w.pre_manage_rect.x,
                        "y": w.pre_manage_rect.y,
                        "width": w.pre_manage_rect.width,
                        "height": w.pre_manage_rect.height,
                    }),
                })
            })
            .collect();

        serde_json::json!({
            "viewport_offset": viewport_offset,
            "windows": windows_json,
            "focused": self.focused,
            "count": self.windows.len(),
        })
    }

    /// Returns the WindowIds of all currently tiling-active windows.
    ///
    /// Used at startup to build the initial layout in one batch operation.
    /// Only includes windows in `TilingState::Active` — minimized and
    /// non-tiling windows are excluded.
    ///
    /// # Returns
    ///
    /// A `Vec<WindowId>` in no guaranteed order. The caller (orchestrator)
    /// should pass these to [`ScrollingSpace::initialize_windows`](crate::workspace::ScrollingSpace::initialize_windows)
    /// for efficient batch layout construction.
    #[must_use]
    pub fn tiling_window_ids(&self) -> Vec<crate::common::WindowId> {
        self.windows
            .iter()
            .filter_map(|(key, w)| match &w.state {
                WindowState::Tiling(TilingState::Active { .. }) => {
                    Some(crate::common::WindowId(*key))
                }
                _ => None,
            })
            .collect()
    }

    /// Returns tiling window IDs sorted by their current x-coordinate (ascending).
    ///
    /// This is used during initialization to assign columns left-to-right,
    /// matching the spatial arrangement of windows on screen. Sorting minimizes
    /// the total travel distance when windows animate to their tiling positions.
    ///
    /// Without sorting, [`tiling_window_ids`](Self::tiling_window_ids) returns IDs
    /// in arbitrary `HashMap` iteration order, which can cause windows to animate
    /// diagonally across the screen — e.g., the rightmost window could be assigned
    /// to column 0 while the leftmost gets column 2.
    ///
    /// # Returns
    ///
    /// A `Vec<WindowId>` sorted by `pre_manage_rect.x` in ascending order.
    /// Only includes windows in `TilingState::Active`.
    #[must_use]
    pub fn tiling_window_ids_sorted_by_x(&self) -> Vec<crate::common::WindowId> {
        let mut ids: Vec<(crate::common::WindowId, i32)> = self
            .windows
            .iter()
            .filter_map(|(key, w)| match &w.state {
                WindowState::Tiling(TilingState::Active { .. }) => {
                    Some((crate::common::WindowId(*key), w.pre_manage_rect.x))
                }
                _ => None,
            })
            .collect();
        ids.sort_by_key(|(_, x)| *x);
        ids.into_iter().map(|(id, _)| id).collect()
    }

    /// Returns tiling window IDs with their `pre_manage_rect.width` sorted by x.
    ///
    /// Like [`tiling_window_ids_sorted_by_x`](Self::tiling_window_ids_sorted_by_x)
    /// but also returns each window's pre-flow width for init-time width
    /// preservation. Widths are clamped to `u32` (negative widths become 0,
    /// callers should substitute `column_width`).
    #[must_use]
    pub fn tiling_window_ids_with_widths_sorted_by_x(&self) -> Vec<(crate::common::WindowId, u32)> {
        let mut entries: Vec<(crate::common::WindowId, i32, u32)> = self
            .windows
            .iter()
            .filter_map(|(key, w)| match &w.state {
                WindowState::Tiling(TilingState::Active { .. }) => {
                    let width = w.pre_manage_rect.width.max(0) as u32;
                    Some((crate::common::WindowId(*key), w.pre_manage_rect.x, width))
                }
                _ => None,
            })
            .collect();
        entries.sort_by_key(|(_, x, _)| *x);
        entries.into_iter().map(|(id, _, w)| (id, w)).collect()
    }

    /// Returns `(hwnd_key, pre_manage_rect, invisible_bounds)` for every window
    /// flow is actively positioning — those in a `Tiling(Active)` or
    /// `Floating(Active)` state.
    ///
    /// `Minimized`/`Hidden` windows are excluded: flow does not actively place
    /// them, so their position is the OS's/user's concern and the rescue pass
    /// leaves them alone. `Ignored` windows are excluded for the same reason.
    /// The rescue pass uses `pre_manage_rect` as the on-screen anchor and
    /// `invisible_bounds` to convert the window rect reported by `GetWindowRect`
    /// back to the visible-content rect used for the visibility test.
    ///
    /// See `docs/src/dev-guide/ipc-and-watchdog.md` for the rescue contract.
    #[must_use]
    pub fn restorable_windows(&self) -> Vec<(isize, Rect, InvisibleBounds)> {
        self.windows
            .iter()
            .filter_map(|(key, w)| match &w.state {
                WindowState::Tiling(TilingState::Active { .. })
                | WindowState::Floating(FloatingState::Active { .. }) => {
                    Some((*key, w.pre_manage_rect, w.invisible_bounds))
                }
                _ => None,
            })
            .collect()
    }

    /// Synchronize tiling state from the layout engine's virtual layout.
    ///
    /// Walks all columns in the [`VirtualLayout`] and updates each window's
    /// [`TilingState::Active { col, row }`](TilingState::Active) to reflect
    /// its current position in the layout grid. This must be called after
    /// every layout mutation to keep registry state in sync with the engine.
    ///
    /// # Arguments
    ///
    /// * `virtual_layout` — The current virtual layout from the layout engine.
    ///
    /// # Design Decision
    ///
    /// Rather than giving the layout engine direct access to the registry,
    /// this method provides a *pull-based* sync: the orchestrator calls it
    /// after each mutation, passing the new layout. This keeps the layout
    /// engine pure (no Win32, no registry dependency) while ensuring the
    /// registry always has up-to-date position data for queries.
    pub fn update_tiling_slots_from_layout(
        &mut self,
        virtual_layout: &crate::layout::types::VirtualLayout,
    ) {
        for (col_idx, column) in virtual_layout.columns.iter().enumerate() {
            for (row_idx, row) in column.rows.iter().enumerate() {
                if let Some(window) = self.windows.get_mut(&row.window_id.0) {
                    window.state = WindowState::Tiling(TilingState::Active {
                        col: col_idx,
                        row: row_idx,
                    });
                }
            }
        }
    }

    /// Synchronize tiled rectangles from the layout engine's actual layout.
    ///
    /// Updates each tiling window's `tiled_rect` field with its current
    /// screen position from the projected [`ActualLayout`]. This must be
    /// called after every layout mutation so that queries return the live
    /// tiled position rather than the stale `pre_manage_rect`.
    ///
    /// # Arguments
    ///
    /// * `actual_layout` — The current actual layout (projected screen coords).
    pub fn update_tiled_rects(&mut self, actual_layout: &crate::layout::types::ActualLayout) {
        for entry in &actual_layout.entries {
            if let Some(window) = self.windows.get_mut(&entry.window_id.0) {
                window.tiled_rect = Some(entry.rect);
            }
        }
    }

    /// Check if a window is in tiling state (before removal).
    ///
    /// Used by the orchestrator to determine if removing a window
    /// requires layout engine updates. Returns `true` if the window
    /// is in any `TilingState` variant (Active, Minimized, or Hidden).
    ///
    /// Note: this is the broad "is managed as tiling at all" check. For the
    /// strict "is currently occupying a live layout slot" check (only
    /// `Tiling(Active)`), use [`is_tiling_active`](Self::is_tiling_active).
    ///
    /// This check should be performed **before** calling
    /// [`remove_window`](Self::remove_window) because the window
    /// will no longer exist in the registry afterward.
    ///
    /// # Arguments
    ///
    /// * `hwnd_val` - The window handle value (HWND as `isize`).
    #[must_use]
    pub fn is_tiling(&self, hwnd_val: isize) -> bool {
        self.windows
            .get(&hwnd_val)
            .map(|w| matches!(w.state, WindowState::Tiling(_)))
            .unwrap_or(false)
    }

    /// Returns `true` iff `hwnd` is tracked AND in the `Tiling(Active)` state.
    ///
    /// Stricter than [`is_tiling`](Self::is_tiling), which also returns `true`
    /// for `Tiling(Minimized)` and `Tiling(Hidden)`. The daemon uses this to
    /// decide whether a re-shown window should re-enter the live layout: a
    /// `Hidden` window is tracked (so `is_tiling` is true) but NOT active, so
    /// `on_window_shown` should re-add it.
    ///
    /// # Arguments
    ///
    /// * `hwnd_val` - The window handle value (HWND as `isize`).
    #[must_use]
    pub fn is_tiling_active(&self, hwnd_val: isize) -> bool {
        match self.windows.get(&hwnd_val) {
            Some(window) => {
                matches!(
                    window.state,
                    WindowState::Tiling(TilingState::Active { .. })
                )
            }
            None => false,
        }
    }

    /// Returns `true` iff `hwnd` is currently tracked by the registry,
    /// regardless of state (tiling, floating, or ignored).
    ///
    /// Used by the daemon's late-title recovery path
    /// ([`HookEvent::NameChange`](super::HookEvent::NameChange)) to decide
    /// whether a title change concerns a window flow already manages. Acting
    /// only on *untracked* windows avoids re-classifying (and potentially
    /// re-tiling) tracked windows on every title change, which would churn
    /// the layout.
    ///
    /// # Arguments
    ///
    /// * `hwnd_val` - The window handle value (HWND as `isize`).
    #[must_use]
    pub fn is_tracked(&self, hwnd_val: isize) -> bool {
        self.windows.contains_key(&hwnd_val)
    }

    /// Re-evaluates a tracked window's OS state (maximized/fullscreen) and
    /// updates its stored [`WindowState`] if that state has changed.
    ///
    /// Drives Option D recovery: a window that launched maximized or
    /// fullscreen was classified `Ignored(Maximized|Fullscreen)` and never
    /// entered the tiling layout. When the user later restores it,
    /// `EVENT_OBJECT_STATECHANGE` fires; the daemon calls this method to
    /// re-run the classifier against the window's *current* OS state. If the
    /// window is now tile-eligible, the returned
    /// [`Recovered`](super::types::ReclassifyResult::Recovered) variant tells
    /// the daemon to insert it into the live layout.
    ///
    /// # Filtering — why this is cheap on a noisy event
    ///
    /// `EVENT_OBJECT_STATECHANGE` fires for many state bits on many windows.
    /// This method short-circuits in every non-recovery case, so the common
    /// path is a single `HashMap` lookup plus a state match:
    ///
    /// - **Untracked** → [`Untracked`](super::types::ReclassifyResult::Untracked)
    ///   (the window is not managed by flow; let `CREATE`/`NAMECHANGE` handle it).
    /// - **Not OS-ignored** → [`NotApplicable`](super::types::ReclassifyResult::NotApplicable)
    ///   (the window is already tiling/floating, or ignored by an explicit
    ///   rule). OS-state recovery does not touch rule-classified windows.
    /// - **Still OS-ignored** → [`Unchanged`](super::types::ReclassifyResult::Unchanged)
    ///   (the user has not actually restored the window; `IsZoomed`/fullscreen
    ///   still true).
    ///
    /// Only when the stored state *was* OS-ignored but the live Win32 state no
    /// longer matches do we pay for a full re-classification (which clones the
    /// stored metadata strings — acceptable, since this runs only on a genuine
    /// recovery, not on every `STATECHANGE`).
    ///
    /// # Why not the reverse direction?
    ///
    /// This method recovers ignored → tiling only. It deliberately does NOT
    /// turn a tiling window into `Ignored(Maximized)` when the user maximizes
    /// it; that would evict an in-layout window on every maximize and is a
    /// separate, opt-in behavior outside this recovery feature's scope.
    ///
    /// # Arguments
    ///
    /// * `hwnd_val` - The window handle value (HWND as `isize`).
    #[must_use]
    pub fn reclassify_os_state(&mut self, hwnd_val: isize) -> super::types::ReclassifyResult {
        use super::types::ReclassifyResult;

        // Stage 1: cheap read-only check — is this window a recovery candidate?
        // Returns early for every common non-recovery case (untracked, or
        // tracked-but-not-OS-ignored), so the noisy STATECHANGE event costs
        // almost nothing on the happy path. `was_maximized`/`was_fullscreen`
        // are Copy bools, so no borrow is held past this block.
        let (was_maximized, was_fullscreen) = match self.windows.get(&hwnd_val) {
            None => return ReclassifyResult::Untracked,
            Some(window) => {
                let mx = matches!(window.state, WindowState::Ignored(IgnoredReason::Maximized));
                let fs = matches!(
                    window.state,
                    WindowState::Ignored(IgnoredReason::Fullscreen)
                );
                if !mx && !fs {
                    return ReclassifyResult::NotApplicable;
                }
                (mx, fs)
            }
        };

        // Stage 2: query the live OS state. No registry borrow is held here,
        // and these use the same predicates the classification pipeline applied
        // at creation time, so recovery stays consistent with initial classify.
        let hwnd = HWND(hwnd_val as *mut _);
        let is_maximized = win32::is_zoomed(hwnd);
        let is_fullscreen = win32::is_fullscreen(hwnd).unwrap_or(false);

        // If the OS condition that originally caused the ignore is still in
        // effect, the user hasn't restored the window — nothing to do.
        if (was_maximized && is_maximized) || (was_fullscreen && is_fullscreen) {
            return ReclassifyResult::Unchanged;
        }

        // Stage 3: the OS state genuinely changed — re-run the classifier. We
        // rebuild the candidate from the stored metadata: the app identity has
        // not changed, only its maximized/fullscreen state has. (Note: the
        // result may still be ignored, e.g. maximized → fullscreen; that's fine
        // — `now_tiling` will be false and the daemon simply won't tile it.)
        let new_state = match self.windows.get(&hwnd_val) {
            Some(window) => {
                let candidate = classification::WindowCandidate {
                    exe: window.exe.clone(),
                    title: window.title.clone(),
                    class: window.class.clone(),
                    process_path: window.process_path.to_string_lossy().into_owned(),
                };
                classification::classify_with_state_pipeline(
                    &candidate,
                    is_maximized,
                    is_fullscreen,
                    &self.pipeline,
                )
            }
            // The window vanished between stages (e.g. destroyed mid-recovery).
            None => return ReclassifyResult::Untracked,
        };
        let now_tiling = matches!(new_state, WindowState::Tiling(_));

        // Stage 4: commit the new state.
        if let Some(window) = self.windows.get_mut(&hwnd_val) {
            window.state = new_state;
        }
        log::info!(
            "reclassify_os_state: window isize={hwnd_val} recovered \
             (is_maximized={is_maximized}, is_fullscreen={is_fullscreen}, now_tiling={now_tiling})"
        );
        ReclassifyResult::Recovered { now_tiling }
    }

    /// Handles a window creation event from the WinEvent hook.
    ///
    /// Applies the visibility/title/Alt+Tab/owner gates below, then delegates
    /// to [`register_window_from_info`](Self::register_window_from_info) which
    /// runs the classification pipeline and commits the resulting state.
    ///
    /// # Why Re-check Visibility and Title?
    ///
    /// The init scan checks these same conditions, but we re-check here because
    /// a window's state can change between the init scan and the first
    /// `EVENT_OBJECT_CREATE`. A window might be created invisible and then shown,
    /// or shown and then hidden. The WinEvent hook catches the creation event
    /// regardless, so we filter at registration time.
    ///
    /// # Returns
    ///
    /// Returns `Some(WindowId(hwnd_val))` if the window was classified as
    /// tiling (i.e., it entered the registry in a `WindowState::Tiling` state).
    /// Returns `None` for floating, ignored, or skipped windows. This allows the
    /// caller (orchestrator) to immediately add the window to the layout engine
    /// without a second lookup.
    ///
    /// # Diagnostics
    ///
    /// Every early-return path emits a `log::debug!` line naming the gate that
    /// rejected the window (e.g. `handle_created: skipping HWND(..) — not
    /// Alt+Tab visible`), mirroring [`scan_existing_windows`](Self::scan_existing_windows).
    /// This makes the common "window was created but never tiled" failure
    /// (e.g. an app with an unusual extended-window style or a transient owner)
    /// directly diagnosable from the daemon log instead of failing silently.
    /// A window that passes every gate but is classified as non-tiling
    /// (Floating/Ignored) is logged separately.
    pub fn handle_created(&mut self, hwnd_val: isize) -> Option<crate::common::WindowId> {
        let hwnd = HWND(hwnd_val as *mut _);

        // Already tracked — benign no-op. This can happen on a duplicate
        // CREATE event or when the init scan already registered the window.
        // Logged so a "never evaluated" window is distinguishable from one we
        // correctly de-duplicated.
        if self.windows.contains_key(&hwnd_val) {
            log::debug!("handle_created: {hwnd:?} already tracked — skipping");
            return None;
        }

        // Only manage visible windows with titles.
        if !win32::is_window_visible(hwnd) {
            log::debug!("handle_created: skipping {hwnd:?} — not visible");
            return None;
        }

        let title = win32::get_window_text(hwnd).unwrap_or_default();
        if title.is_empty() {
            log::debug!("handle_created: skipping {hwnd:?} — empty title");
            return None;
        }

        // Skip windows that wouldn't appear in Alt+Tab (tool windows,
        // tray icons, background helpers).
        if !win32::is_alt_tab_visible(hwnd) {
            log::debug!("handle_created: skipping {hwnd:?} — not Alt+Tab visible");
            return None;
        }

        // Skip iconic (minimized) windows — same rationale as scan_existing_windows.
        if win32::is_iconic(hwnd) {
            log::debug!("handle_created: skipping {hwnd:?} — iconic (minimized)");
            return None;
        }

        // Skip windows with an owner (dialogs, popups) or a parent (child controls
        // inside a dialog — e.g. buttons, labels, Inno Setup TNew* controls).
        if has_owner(hwnd) || has_parent(hwnd) {
            log::debug!(
                "handle_created: skipping {hwnd:?} — has owner or parent (dialog/popup/child)"
            );
            return None;
        }

        match win32::get_window_info(hwnd) {
            Ok(info) => {
                self.register_window_from_info(&info);
            }
            Err(e) => {
                log::warn!("handle_created: failed to get info for {hwnd:?}: {e}");
                return None;
            }
        }

        // Check if the window was classified as tiling.
        if let Some(window) = self.windows.get(&hwnd_val) {
            match &window.state {
                WindowState::Tiling(_) => Some(crate::common::WindowId(hwnd_val)),
                // Passed every gate, but classified Floating/Ignored (e.g.
                // maximized, fullscreen, or an explicit rule) — so it does not
                // enter the layout. Logged because "passed the gates but still
                // not tiled" is otherwise indistinguishable from a rejection.
                other => {
                    log::debug!("handle_created: {hwnd:?} registered as non-tiling ({other:?})");
                    None
                }
            }
        } else {
            None
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
        WindowState::Tiling(TilingState::Hidden) => {
            serde_json::json!("Tiling::Hidden")
        }
        WindowState::Floating(FloatingState::Active { rect }) => serde_json::json!({
            "Floating": {"Active": {"rect": {"x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height}}}
        }),
        WindowState::Floating(FloatingState::Minimized) => {
            serde_json::json!("Floating::Minimized")
        }
        WindowState::Floating(FloatingState::Hidden) => {
            serde_json::json!("Floating::Hidden")
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

/// Checks if a window has an owner (i.e., is an owned dialog or popup).
///
/// Uses `GetWindow(hwnd, GW_OWNER)`. Owned windows are dialogs or popups whose
/// position is managed by their owner. This does NOT catch child windows
/// (those with a parent via [`has_parent`]) — see [`has_parent`].
///
/// See the Window Registry chapter (`docs/src/dev-guide/window-registry.md`).
fn has_owner(hwnd: HWND) -> bool {
    match unsafe { GetWindow(hwnd, GW_OWNER) } {
        Ok(owner) => !owner.is_invalid(),
        Err(_) => false,
    }
}

/// Checks if a window has a parent (i.e., is a `WS_CHILD` control, not
/// top-level), via `GetWindowLongW(hwnd, GWL_STYLE)`. Identifies embedded
/// controls (buttons, labels, Inno Setup `TNew*`) that cannot be tiled.
///
/// Deliberately narrow: catches `WS_CHILD` only, not reparented popups or
/// owned top-level windows. Distinct from [`has_owner`] (owner relation).
///
/// See the Window Registry chapter (`docs/src/dev-guide/window-registry.md`).
fn has_parent(hwnd: HWND) -> bool {
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) };
    let style = WINDOW_STYLE(style as u32);
    style & WS_CHILD != WINDOW_STYLE(0)
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
    use super::super::types::ReclassifyResult;
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
        let json = reg.to_json_value(0);

        assert_eq!(json["count"], 0);
        assert!(json["windows"].as_array().unwrap().is_empty());
        assert!(json["focused"].is_null());
        assert_eq!(json["viewport_offset"], 0);
    }

    #[test]
    fn to_json_value_has_correct_structure() {
        let (user, default) = default_rules();
        let reg = WindowRegistry::new(&user, &default);
        let json = reg.to_json_value(0);

        assert!(json.get("windows").is_some());
        assert!(json.get("focused").is_some());
        assert!(json.get("count").is_some());
        assert!(json.get("viewport_offset").is_some());
    }

    #[test]
    fn to_json_value_includes_window_rect_field_for_windows() {
        // Positive: each window entry in the JSON must contain a "window_rect" field.
        // The field is populated via GetWindowRect in the query output.
        // For windows with invalid HWNDs (unit test), window_rect will be null.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        // Insert a tiling-active window with an invalid HWND (not a real Win32 window).
        // GetWindowRect will fail, so window_rect should be null.
        let hwnd_val = 12345isize;
        insert_test_window(
            &mut reg,
            hwnd_val,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );

        let json = reg.to_json_value(0);
        let windows = json["windows"]
            .as_array()
            .expect("windows should be an array");
        assert_eq!(windows.len(), 1);

        let w = &windows[0];
        // The window_rect field must be present
        assert!(
            w.get("window_rect").is_some(),
            "window_rect field must be present in JSON output"
        );
        // For an invalid HWND, window_rect should be null (GetWindowRect fails)
        assert!(
            w["window_rect"].is_null(),
            "window_rect should be null for invalid HWND (GetWindowRect fails)"
        );
    }

    #[test]
    fn to_json_value_includes_tiled_rect_and_window_rect() {
        // Positive: both tiled_rect and window_rect appear together in JSON.
        // tiled_rect is set from layout engine data; window_rect from Win32.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        let hwnd_val = 54321isize;
        let hwnd = HWND(hwnd_val as *mut _);
        let window = Window::new(
            hwnd,
            "test.exe".into(),
            "Test".into(),
            "TestClass".into(),
            std::path::PathBuf::from("C:\\test.exe"),
            crate::common::Rect {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
            },
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
            crate::common::InvisibleBounds::zero(),
        );
        // Set tiled_rect to simulate what the layout engine would produce
        let mut window = window;
        window.tiled_rect = Some(crate::common::Rect {
            x: 4,
            y: 0,
            width: 952,
            height: 1080,
        });
        reg.windows.insert(hwnd_val, window);

        let json = reg.to_json_value(0);
        let windows = json["windows"]
            .as_array()
            .expect("windows should be an array");
        let w = &windows[0];

        // tiled_rect should be the engine-computed value
        let tiled = w["tiled_rect"]
            .as_object()
            .expect("tiled_rect should be an object");
        assert_eq!(tiled["x"], 4);
        assert_eq!(tiled["width"], 952);

        // window_rect should be present (null for invalid HWND, but field exists)
        assert!(
            w.get("window_rect").is_some(),
            "window_rect field must be present alongside tiled_rect"
        );

        // invisible_bounds should be present with zero values
        let ib = w["invisible_bounds"]
            .as_object()
            .expect("invisible_bounds should be an object");
        assert_eq!(ib["left"], 0);
        assert_eq!(ib["top"], 0);
        assert_eq!(ib["right"], 0);
        assert_eq!(ib["bottom"], 0);
    }

    #[test]
    fn to_json_value_window_rect_null_format() {
        // Negative: window_rect must be either null or a {x,y,width,height} object.
        // Verify the field exists and is null when GetWindowRect fails.
        let (user, default) = default_rules();
        let reg = WindowRegistry::new(&user, &default);
        let json = reg.to_json_value(0);

        // Empty registry: windows array is empty, but the field path is valid
        assert!(json["windows"].is_array());
    }

    #[test]
    fn to_json_value_includes_visible_rect_and_invisible_bounds_fields() {
        // Positive: each window entry must contain "visible_rect" and
        // "invisible_bounds" fields. For an invalid HWND, window_rect is null
        // (GetWindowRect fails), so visible_rect should also be null.
        // invisible_bounds should always be present with zero values for
        // test windows (which are constructed with InvisibleBounds::zero()).
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        let hwnd_val = 99999isize;
        let hwnd = HWND(hwnd_val as *mut _);
        let window = Window::new(
            hwnd,
            "visrect.exe".into(),
            "VisRect".into(),
            "VisClass".into(),
            std::path::PathBuf::from("C:\\visrect.exe"),
            crate::common::Rect {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
            },
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
            crate::common::InvisibleBounds::zero(),
        );
        reg.windows.insert(hwnd_val, window);

        let json = reg.to_json_value(0);
        let w = &json["windows"].as_array().unwrap()[0];

        // invisible_bounds must be present with all four fields
        assert!(
            w.get("invisible_bounds").is_some(),
            "invisible_bounds field must be present in JSON output"
        );
        let ib = w["invisible_bounds"].as_object().unwrap();
        assert_eq!(ib["left"], 0);
        assert_eq!(ib["top"], 0);
        assert_eq!(ib["right"], 0);
        assert_eq!(ib["bottom"], 0);

        // visible_rect must be present (null when window_rect is null)
        assert!(
            w.get("visible_rect").is_some(),
            "visible_rect field must be present in JSON output"
        );
        // For invalid HWND, window_rect is null, so visible_rect must also be null
        assert!(
            w["visible_rect"].is_null(),
            "visible_rect should be null when window_rect is null (invalid HWND)"
        );
    }

    #[test]
    fn to_json_value_visible_rect_null_when_window_rect_null() {
        // Negative: if window_rect is null (GetWindowRect fails for invalid HWND),
        // then visible_rect must also be null — no math on a missing rect.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        let hwnd_val = 11111isize;
        insert_test_window(
            &mut reg,
            hwnd_val,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );

        let json = reg.to_json_value(0);
        let w = &json["windows"].as_array().unwrap()[0];

        assert!(
            w["window_rect"].is_null(),
            "window_rect should be null for test window with invalid HWND"
        );
        assert!(
            w["visible_rect"].is_null(),
            "visible_rect should be null when window_rect is null"
        );
    }

    #[test]
    fn register_window_from_info_stores_invisible_bounds() {
        // Positive: register_window_from_info should call get_invisible_bounds
        // and store the result on the Window entry. For an invalid HWND (the
        // test hwnd is not a real window), get_invisible_bounds returns zero.
        let user = WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![],
        };
        let default = WindowRulesConfig::default();
        let mut reg = WindowRegistry::new(&user, &default);

        let hwnd_val = 44444isize;
        let info = win32::WindowInfo {
            hwnd: HWND(hwnd_val as *mut _),
            title: "BoundsTest".to_owned(),
            class: "BoundsClass".to_owned(),
            rect: crate::common::Rect {
                x: 0,
                y: 0,
                width: 640,
                height: 480,
            },
            exe: "boundstest.exe".to_owned(),
            process_path: "C:\\boundstest.exe".to_owned(),
            is_visible: true,
            is_maximized: false,
            is_fullscreen: false,
        };

        reg.register_window_from_info(&info);
        let w = reg.get_window(HWND(hwnd_val as *mut _)).unwrap();

        // For an invalid HWND, get_invisible_bounds returns zero (fail-open).
        // Verify it was actually stored.
        assert_eq!(
            w.invisible_bounds,
            crate::common::InvisibleBounds::zero(),
            "invisible_bounds should be stored as zero for invalid HWND"
        );
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
            crate::common::InvisibleBounds::zero(),
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
            crate::common::InvisibleBounds::zero(),
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
        insert_test_window_with_rect(reg, hwnd_val, state, 0);
    }

    /// Inserts a test window with a custom `pre_manage_rect.x` position.
    ///
    /// Used to test spatial sorting (e.g., `tiling_window_ids_sorted_by_x`).
    /// All other rect fields use defaults (width=100, height=100, y=0).
    fn insert_test_window_with_rect(
        reg: &mut WindowRegistry,
        hwnd_val: isize,
        state: WindowState,
        x: i32,
    ) {
        let hwnd = HWND(hwnd_val as *mut _);
        let window = Window::new(
            hwnd,
            "test.exe".into(),
            format!("Test-{hwnd_val}"),
            "TestClass".into(),
            std::path::PathBuf::from("C:\\test.exe"),
            crate::common::Rect {
                x,
                y: 0,
                width: 100,
                height: 100,
            },
            state,
            crate::common::InvisibleBounds::zero(),
        );
        reg.windows.insert(hwnd_val, window);
    }

    fn insert_test_window_with_rect_and_width(
        reg: &mut WindowRegistry,
        hwnd_val: isize,
        state: WindowState,
        x: i32,
        width: i32,
    ) {
        let hwnd = HWND(hwnd_val as *mut _);
        let window = Window::new(
            hwnd,
            "test.exe".into(),
            format!("Test-{hwnd_val}"),
            "TestClass".into(),
            std::path::PathBuf::from("C:\\test.exe"),
            crate::common::Rect {
                x,
                y: 0,
                width,
                height: 100,
            },
            state,
            crate::common::InvisibleBounds::zero(),
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

    // ── focused() getter tests ──────────────────────────────────────

    #[test]
    fn focused_returns_none_by_default() {
        let (user, default) = default_rules();
        let reg = WindowRegistry::new(&user, &default);
        assert!(reg.focused().is_none());
    }

    #[test]
    fn focused_returns_set_value() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            42,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        reg.set_focused(42);
        assert_eq!(reg.focused(), Some(crate::common::WindowId(42)));
    }

    #[test]
    fn focused_returns_none_after_clear() {
        // Removing the focused window clears focus — getter must reflect that.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            42,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        reg.set_focused(42);
        assert_eq!(reg.focused(), Some(crate::common::WindowId(42)));

        reg.remove_window(42);
        assert!(reg.focused().is_none());
    }

    // ── get_window_mut tests ─────────────────────────────────────────
    //
    // get_window_mut is the mutable accessor used by the daemon to transition
    // a window between Tiling and Floating states (dispatch_set_window). It is
    // a pure HashMap lookup — no Win32 — so it is fully unit-testable using
    // the same insert_test_window helper as the focused() tests above.

    #[test]
    fn get_window_mut_returns_some_and_allows_mutation_for_existing() {
        // Positive: an existing window is returned as Some, and mutating
        // through the reference persists in the registry. This mirrors how
        // dispatch_set_window writes a new WindowState (Tiling → Floating).
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            42,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );

        // Act: mutate the window's state through the mutable reference.
        let window = reg
            .get_window_mut(HWND(42 as *mut _))
            .expect("existing window should be Some");
        window.state = WindowState::Floating(FloatingState::Active {
            rect: crate::common::Rect {
                x: 10,
                y: 10,
                width: 800,
                height: 600,
            },
        });

        // Assert: the mutation persisted.
        let after = reg.get_window(HWND(42 as *mut _)).unwrap();
        assert!(
            matches!(
                after.state,
                WindowState::Floating(FloatingState::Active { .. })
            ),
            "state mutation via get_window_mut should persist"
        );
    }

    #[test]
    fn get_window_mut_returns_none_for_absent_hwnd() {
        // Negative: an HWND never inserted returns None — the daemon relies on
        // this (it guards the mutation with `if let Some(window) = ...` in
        // set_window_to_float, so a None must not panic).
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        // Empty registry.
        assert!(reg.get_window_mut(HWND(99999 as *mut _)).is_none());

        // Non-empty registry but absent hwnd.
        insert_test_window(
            &mut reg,
            10,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        assert!(reg.get_window_mut(HWND(77777 as *mut _)).is_none());

        // After removal, a previously-present hwnd returns None.
        reg.remove_window(10);
        assert!(reg.get_window_mut(HWND(10 as *mut _)).is_none());
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

    // ── tiling_window_ids tests ────────────────────────────────────────

    #[test]
    fn tiling_window_ids_returns_active_tiling_only() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        // Insert a mix of states.
        insert_test_window(
            &mut reg,
            100,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        insert_test_window(&mut reg, 200, WindowState::Tiling(TilingState::Minimized));
        insert_test_window(
            &mut reg,
            300,
            WindowState::Floating(FloatingState::Active {
                rect: crate::common::Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            }),
        );
        insert_test_window(
            &mut reg,
            400,
            WindowState::Ignored(IgnoredReason::Maximized),
        );

        let ids = reg.tiling_window_ids();
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(&crate::common::WindowId(100)));
    }

    #[test]
    fn tiling_window_ids_empty_registry() {
        let (user, default) = default_rules();
        let reg = WindowRegistry::new(&user, &default);
        assert!(reg.tiling_window_ids().is_empty());
    }

    #[test]
    fn tiling_window_ids_multiple_active() {
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
        insert_test_window(
            &mut reg,
            30,
            WindowState::Tiling(TilingState::Active { col: 2, row: 0 }),
        );

        let ids = reg.tiling_window_ids();
        assert_eq!(ids.len(), 3);
    }

    // ── is_tiling tests ──────────────────────────────────────────────

    #[test]
    fn is_tiling_true_for_active() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            42,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        assert!(reg.is_tiling(42));
    }

    #[test]
    fn is_tiling_true_for_minimized() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(&mut reg, 42, WindowState::Tiling(TilingState::Minimized));
        assert!(reg.is_tiling(42));
    }

    #[test]
    fn is_tiling_false_for_floating() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            42,
            WindowState::Floating(FloatingState::Active {
                rect: crate::common::Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            }),
        );
        assert!(!reg.is_tiling(42));
    }

    #[test]
    fn is_tiling_false_for_ignored() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(&mut reg, 42, WindowState::Ignored(IgnoredReason::Maximized));
        assert!(!reg.is_tiling(42));
    }

    #[test]
    fn is_tiling_false_for_unknown() {
        let (user, default) = default_rules();
        let reg = WindowRegistry::new(&user, &default);
        assert!(!reg.is_tiling(99999));
    }

    // ── handle_created returns tests ──────────────────────────────────

    #[test]
    fn handle_created_returns_none_for_nonexistent_hwnd() {
        // handle_created on an HWND that doesn't exist will fail to get
        // window info and return None — should not panic.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        // HWND 0 is typically invalid — handle_created should return None.
        let result = reg.handle_created(0);
        assert!(result.is_none());
    }

    // ── handle_created de-duplication gate tests ──────────────────────
    //
    // The first check in `handle_created` is `self.windows.contains_key(&hwnd_val)`.
    // This gate is the *final guard* that makes the daemon-layer recovery
    // handlers safe: when both `on_window_name_change` and `on_window_shown`
    // attempt to register the same HWND (e.g. Windows Terminal's
    // `Created → NameChange → Shown` lifecycle), the second attempt is a
    // silent no-op rather than a duplicate registration or layout churn.
    //
    // These tests verify that guarantee directly. They are pure (no Win32
    // calls) because `insert_test_window` inserts straight into the HashMap
    // and the `contains_key` check fires BEFORE any `win32::` call inside
    // `handle_created`.

    #[test]
    fn handle_created_skips_already_tracked_tiling_window() {
        // Positive: an HWND already registered as Tiling(Active) must be
        // silently ignored on a second handle_created call. This is the
        // exact path taken when SHOW recovery re-runs creation for a window
        // that NAMECHANGE recovery already registered.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            42,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        assert_eq!(reg.len(), 1);

        // Second attempt — must return None and leave the registry untouched.
        let result = reg.handle_created(42);
        assert!(
            result.is_none(),
            "handle_created must no-op on an already-tracked HWND"
        );
        assert_eq!(
            reg.len(),
            1,
            "registry size must not change on duplicate handle_created"
        );
        // State must be preserved exactly — no spurious reclassification.
        let w = reg.get_window(HWND(42 as *mut _)).unwrap();
        assert!(matches!(
            w.state,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 })
        ));
    }

    #[test]
    fn handle_created_skips_already_tracked_non_tiling_window() {
        // Edge: the de-dup gate is `contains_key`, which is state-agnostic.
        // A window registered as Floating or Ignored must also be skipped,
        // proving the gate's safety claim "an HWND is unique per window"
        // regardless of classification. (A floating/ignored window that
        // later receives a SHOW event must not be re-evaluated as tiling.)
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            10,
            WindowState::Floating(FloatingState::Active {
                rect: crate::common::Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            }),
        );
        insert_test_window(&mut reg, 20, WindowState::Ignored(IgnoredReason::Maximized));
        assert_eq!(reg.len(), 2);

        // Both must be skipped without reclassification or churn.
        assert!(reg.handle_created(10).is_none());
        assert!(reg.handle_created(20).is_none());
        assert_eq!(
            reg.len(),
            2,
            "non-tiling tracked windows must not be churned"
        );

        // State preserved.
        assert!(matches!(
            reg.get_window(HWND(10 as *mut _)).unwrap().state,
            WindowState::Floating(_)
        ));
        assert!(matches!(
            reg.get_window(HWND(20 as *mut _)).unwrap().state,
            WindowState::Ignored(IgnoredReason::Maximized)
        ));
    }

    // ── tiling_window_ids_sorted_by_x tests ──────────────────────────

    #[test]
    fn sorted_by_x_returns_ascending_order() {
        // Positive: 3 tiling windows at x=300, x=100, x=500 (unsorted).
        // The sorted version should return IDs in ascending x order:
        // [x=100, x=300, x=500].
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        insert_test_window_with_rect(
            &mut reg,
            300,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
            300,
        );
        insert_test_window_with_rect(
            &mut reg,
            100,
            WindowState::Tiling(TilingState::Active { col: 1, row: 0 }),
            100,
        );
        insert_test_window_with_rect(
            &mut reg,
            500,
            WindowState::Tiling(TilingState::Active { col: 2, row: 0 }),
            500,
        );

        let sorted = reg.tiling_window_ids_sorted_by_x();
        assert_eq!(sorted.len(), 3);
        // Ascending x: 100, 300, 500
        assert_eq!(sorted[0], crate::common::WindowId(100));
        assert_eq!(sorted[1], crate::common::WindowId(300));
        assert_eq!(sorted[2], crate::common::WindowId(500));
    }

    #[test]
    fn sorted_by_x_empty_registry() {
        // Positive: empty registry → returns empty vec (no panic).
        let (user, default) = default_rules();
        let reg = WindowRegistry::new(&user, &default);
        let sorted = reg.tiling_window_ids_sorted_by_x();
        assert!(sorted.is_empty());
    }

    #[test]
    fn sorted_by_x_single_window() {
        // Positive: single tiling window → returns single-element vec.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            42,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        let sorted = reg.tiling_window_ids_sorted_by_x();
        assert_eq!(sorted.len(), 1);
        assert_eq!(sorted[0], crate::common::WindowId(42));
    }

    #[test]
    fn sorted_by_x_excludes_non_tiling_windows() {
        // Negative: windows in floating/ignored states are excluded from
        // the sorted result, even if they have x-positions.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        insert_test_window_with_rect(
            &mut reg,
            10,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
            500,
        );
        insert_test_window_with_rect(
            &mut reg,
            20,
            WindowState::Floating(FloatingState::Active {
                rect: crate::common::Rect {
                    x: 100,
                    y: 0,
                    width: 200,
                    height: 200,
                },
            }),
            100,
        );
        insert_test_window_with_rect(
            &mut reg,
            30,
            WindowState::Ignored(IgnoredReason::Maximized),
            0,
        );

        let sorted = reg.tiling_window_ids_sorted_by_x();
        // Only the tiling-active window (hwnd=10) should appear.
        assert_eq!(sorted.len(), 1);
        assert_eq!(sorted[0], crate::common::WindowId(10));
    }

    #[test]
    fn sorted_by_x_handles_equal_x_positions() {
        // Edge case: two windows at the same x-position — sort is stable
        // but order among equal keys is not strictly guaranteed.
        // We only verify both are present.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        insert_test_window_with_rect(
            &mut reg,
            10,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
            200,
        );
        insert_test_window_with_rect(
            &mut reg,
            20,
            WindowState::Tiling(TilingState::Active { col: 1, row: 0 }),
            200,
        );

        let sorted = reg.tiling_window_ids_sorted_by_x();
        assert_eq!(sorted.len(), 2);
        assert!(sorted.contains(&crate::common::WindowId(10)));
        assert!(sorted.contains(&crate::common::WindowId(20)));
    }

    #[test]
    fn sorted_with_widths_returns_ids_and_widths() {
        // Positive: returns (id, width) pairs sorted by x. Widths come
        // from pre_manage_rect.width.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        // Insert with a helper that lets us set custom width.
        insert_test_window_with_rect_and_width(
            &mut reg,
            300,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
            300,
            800,
        );
        insert_test_window_with_rect_and_width(
            &mut reg,
            100,
            WindowState::Tiling(TilingState::Active { col: 1, row: 0 }),
            100,
            1200,
        );
        insert_test_window_with_rect_and_width(
            &mut reg,
            500,
            WindowState::Tiling(TilingState::Active { col: 2, row: 0 }),
            500,
            600,
        );

        let result = reg.tiling_window_ids_with_widths_sorted_by_x();
        assert_eq!(result.len(), 3);
        // Sorted by x: 100, 300, 500
        assert_eq!(result[0].0, crate::common::WindowId(100));
        assert_eq!(result[0].1, 1200);
        assert_eq!(result[1].0, crate::common::WindowId(300));
        assert_eq!(result[1].1, 800);
        assert_eq!(result[2].0, crate::common::WindowId(500));
        assert_eq!(result[2].1, 600);
    }

    #[test]
    fn sorted_with_widths_empty_registry() {
        // Positive: empty registry → empty vec.
        let (user, default) = default_rules();
        let reg = WindowRegistry::new(&user, &default);
        let result = reg.tiling_window_ids_with_widths_sorted_by_x();
        assert!(result.is_empty());
    }

    #[test]
    fn sorted_with_widths_single_window() {
        // Positive: single tiling window → single (id, width) pair.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window_with_rect_and_width(
            &mut reg,
            42,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
            100,
            800,
        );
        let result = reg.tiling_window_ids_with_widths_sorted_by_x();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, crate::common::WindowId(42));
        assert_eq!(result[0].1, 800);
    }

    #[test]
    fn sorted_with_widths_excludes_non_tiling_windows() {
        // Negative: floating and ignored windows are excluded even when they
        // have valid x and width. Mirrors `sorted_by_x_excludes_non_tiling_windows`
        // for the width-returning variant — important because dispatch's init
        // flow feeds these widths straight into `initialize_windows`, so a
        // non-tiling leak would desync the column count vs. the ids count.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window_with_rect_and_width(
            &mut reg,
            10,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
            500,
            800,
        );
        insert_test_window_with_rect_and_width(
            &mut reg,
            20,
            WindowState::Floating(FloatingState::Active {
                rect: crate::common::Rect {
                    x: 100,
                    y: 0,
                    width: 200,
                    height: 200,
                },
            }),
            100,
            1200,
        );
        insert_test_window_with_rect_and_width(
            &mut reg,
            30,
            WindowState::Ignored(IgnoredReason::Maximized),
            0,
            600,
        );
        let result = reg.tiling_window_ids_with_widths_sorted_by_x();
        assert_eq!(result.len(), 1, "only the tiling-active window must appear");
        assert_eq!(result[0].0, crate::common::WindowId(10));
        assert_eq!(result[0].1, 800);
    }

    #[test]
    fn sorted_with_widths_clamps_negative_width_to_zero() {
        // Edge: a negative `pre_manage_rect.width` (should not happen via
        // Win32, but the impl clamps with `max(0)` before `as u32`). Without
        // the clamp, `-200i32 as u32` wraps to `u32::MAX - 199 ≈ 4.29e9`,
        // which would then blow past `abs_max_width` during quantize.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window_with_rect_and_width(
            &mut reg,
            10,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
            100,
            -200,
        );
        let result = reg.tiling_window_ids_with_widths_sorted_by_x();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].1, 0,
            "negative width must clamp to 0, not wrap to a huge u32"
        );
    }

    // ── Direct handler tests (replaces process_pending_events tests) ──

    #[test]
    fn direct_destroy_handler_works() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        let hwnd_val = 33isize;

        insert_test_window(
            &mut reg,
            hwnd_val,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );

        reg.remove_window(hwnd_val);
        assert!(reg.is_empty());
    }

    #[test]
    fn direct_foreground_handler_works() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        let hwnd_val = 44isize;

        insert_test_window(
            &mut reg,
            hwnd_val,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );

        reg.set_focused(hwnd_val);
        assert_eq!(reg.focused, Some(hwnd_val));
    }

    // ── is_tiling_active tests ──────────────────────────────────────────

    #[test]
    fn is_tiling_active_true_for_tiling_active() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            42,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        assert!(reg.is_tiling_active(42));
    }

    #[test]
    fn is_tiling_active_false_for_tiling_minimized() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(&mut reg, 42, WindowState::Tiling(TilingState::Minimized));
        assert!(!reg.is_tiling_active(42));
    }

    #[test]
    fn is_tiling_active_false_for_tiling_hidden() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(&mut reg, 42, WindowState::Tiling(TilingState::Hidden));
        assert!(!reg.is_tiling_active(42));
    }

    #[test]
    fn is_tiling_active_false_for_untracked() {
        let (user, default) = default_rules();
        let reg = WindowRegistry::new(&user, &default);
        assert!(!reg.is_tiling_active(99999));
    }

    #[test]
    fn is_tiling_active_false_for_floating() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            42,
            WindowState::Floating(FloatingState::Active {
                rect: crate::common::Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            }),
        );
        assert!(!reg.is_tiling_active(42));
    }

    // ── VisibilityChange PartialEq tests ───────────────────────────────

    #[test]
    fn visibility_change_variants_are_comparable() {
        use super::super::types::VisibilityChange;
        assert_eq!(VisibilityChange::Hidden, VisibilityChange::Hidden);
        assert_eq!(VisibilityChange::Shown, VisibilityChange::Shown);
        assert_eq!(VisibilityChange::Unchanged, VisibilityChange::Unchanged);
        assert_ne!(VisibilityChange::Hidden, VisibilityChange::Shown);
        assert_ne!(VisibilityChange::Shown, VisibilityChange::Unchanged);
        assert_ne!(VisibilityChange::Hidden, VisibilityChange::Unchanged);
    }

    // ── reconcile_visibility tests ────────────────────────────────────
    //
    // reconcile_visibility calls Win32 APIs (is_window_visible, is_cloaked,
    // is_iconic), so we cannot unit-test its full behavior without a real
    // window. We verify the signature compiles and test the trivial cases
    // (untracked → Unchanged, already-Hidden → Unchanged when the Win32
    // check agrees).
    // TODO: integration test with a real hidden window.

    #[test]
    fn reconcile_visibility_untracked_returns_unchanged() {
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        // Untracked HWND — should return Unchanged without touching Win32
        // (early return before the HWND construction).
        let result = reg.reconcile_visibility(12345);
        assert_eq!(result, super::super::types::VisibilityChange::Unchanged);
    }

    #[test]
    fn reconcile_visibility_signature_compiles() {
        // Compile-check: ensure reconcile_visibility has the correct
        // signature and returns VisibilityChange.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        let _result: super::super::types::VisibilityChange = reg.reconcile_visibility(0);
    }

    // ── is_tracked tests ───────────────────────────────────────────────
    //
    // is_tracked is a cheap HashMap::contains_key wrapper. Fully
    // unit-testable — no Win32 calls.

    #[test]
    fn is_tracked_true_for_registered_window() {
        // Positive: a window in the registry should return true.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            42,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        assert!(reg.is_tracked(42));
    }

    #[test]
    fn is_tracked_false_for_unknown_hwnd() {
        // Negative: an HWND never inserted should return false.
        let (user, default) = default_rules();
        let reg = WindowRegistry::new(&user, &default);
        assert!(!reg.is_tracked(99999));
    }

    #[test]
    fn is_tracked_false_after_removal() {
        // Negative: after removing a window, is_tracked should return false.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            77,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        assert!(reg.is_tracked(77));
        reg.remove_window(77);
        assert!(!reg.is_tracked(77));
    }

    #[test]
    fn is_tracked_works_for_all_window_states() {
        // Positive: is_tracked should return true regardless of window state
        // (tiling, floating, ignored) — it checks existence, not state.
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
            WindowState::Floating(FloatingState::Active {
                rect: crate::common::Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            }),
        );
        insert_test_window(&mut reg, 30, WindowState::Ignored(IgnoredReason::Maximized));

        assert!(reg.is_tracked(10), "tiling window should be tracked");
        assert!(reg.is_tracked(20), "floating window should be tracked");
        assert!(reg.is_tracked(30), "ignored window should be tracked");
    }

    // ── reclassify_os_state tests ─────────────────────────────────────
    //
    // reclassify_os_state has 4 result branches:
    //   1. Untracked    → pure HashMap miss (unit-testable)
    //   2. NotApplicable → pure state match (unit-testable)
    //   3. Unchanged    → calls win32::is_zoomed/is_fullscreen (Win32)
    //   4. Recovered    → calls win32::is_zoomed/is_fullscreen + classifier (Win32)
    //
    // Branches 1 and 2 are short-circuits before any Win32 call, so they
    // are fully unit-testable. Branches 3 and 4 require a live HWND for
    // is_zoomed/is_fullscreen and there is NO test seam (trait abstraction /
    // mock) for these Win32 calls. They are documented as requiring Win32
    // integration testing.

    #[test]
    fn reclassify_os_state_untracked_for_unknown_hwnd() {
        // Positive: untracked window returns ReclassifyResult::Untracked.
        // This exercises the first short-circuit (HashMap miss, no Win32).
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        let result = reg.reclassify_os_state(99999);
        assert_eq!(result, ReclassifyResult::Untracked);
    }

    #[test]
    fn reclassify_os_state_untracked_after_removal() {
        // Negative: a window removed between the STATECHANGE event and the
        // reclassify call should return Untracked (window vanished mid-event).
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(&mut reg, 55, WindowState::Ignored(IgnoredReason::Maximized));
        reg.remove_window(55);
        let result = reg.reclassify_os_state(55);
        assert_eq!(result, ReclassifyResult::Untracked);
    }

    #[test]
    fn reclassify_os_state_not_applicable_for_tiling_window() {
        // Positive: a tiling window is not in an OS-ignored state, so
        // reclassify returns NotApplicable (no Win32 call).
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            42,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        let result = reg.reclassify_os_state(42);
        assert_eq!(result, ReclassifyResult::NotApplicable);
    }

    #[test]
    fn reclassify_os_state_not_applicable_for_floating_window() {
        // Positive: a floating window is not OS-ignored → NotApplicable.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            88,
            WindowState::Floating(FloatingState::Active {
                rect: crate::common::Rect {
                    x: 0,
                    y: 0,
                    width: 200,
                    height: 200,
                },
            }),
        );
        let result = reg.reclassify_os_state(88);
        assert_eq!(result, ReclassifyResult::NotApplicable);
    }

    #[test]
    fn reclassify_os_state_not_applicable_for_explicit_rule_ignored() {
        // Positive: a window ignored by an explicit rule (not OS state) is
        // NotApplicable — recovery only targets OS-ignored windows.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            33,
            WindowState::Ignored(IgnoredReason::ExplicitRule),
        );
        let result = reg.reclassify_os_state(33);
        assert_eq!(result, ReclassifyResult::NotApplicable);
    }

    #[test]
    fn reclassify_os_state_not_applicable_for_tiling_minimized() {
        // Positive: a minimized tiling window is not OS-ignored → NotApplicable.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(&mut reg, 44, WindowState::Tiling(TilingState::Minimized));
        let result = reg.reclassify_os_state(44);
        assert_eq!(result, ReclassifyResult::NotApplicable);
    }

    // ── ReclassifyResult PartialEq tests ────────────────────────────────

    #[test]
    fn reclassify_result_variants_are_comparable() {
        // Positive: all ReclassifyResult variants support equality comparison.
        assert_eq!(ReclassifyResult::Untracked, ReclassifyResult::Untracked);
        assert_eq!(
            ReclassifyResult::NotApplicable,
            ReclassifyResult::NotApplicable
        );
        assert_eq!(ReclassifyResult::Unchanged, ReclassifyResult::Unchanged);
        assert_eq!(
            ReclassifyResult::Recovered { now_tiling: true },
            ReclassifyResult::Recovered { now_tiling: true }
        );
        assert_eq!(
            ReclassifyResult::Recovered { now_tiling: false },
            ReclassifyResult::Recovered { now_tiling: false }
        );
        // Negative: variants differ from each other.
        assert_ne!(ReclassifyResult::Untracked, ReclassifyResult::NotApplicable);
        assert_ne!(ReclassifyResult::NotApplicable, ReclassifyResult::Unchanged);
        assert_ne!(
            ReclassifyResult::Unchanged,
            ReclassifyResult::Recovered { now_tiling: true }
        );
        // Negative: Recovered with different flags are not equal.
        assert_ne!(
            ReclassifyResult::Recovered { now_tiling: true },
            ReclassifyResult::Recovered { now_tiling: false }
        );
    }

    // ── restorable_windows tests ───────────────────────────────────────

    /// Collects `restorable_windows()` output into a `hwnd -> Rect` map so the
    /// tests can look up by hwnd without depending on HashMap iteration order.
    fn collect_restorable(
        reg: &WindowRegistry,
    ) -> std::collections::HashMap<isize, crate::common::Rect> {
        reg.restorable_windows()
            .into_iter()
            .map(|(k, r, _)| (k, r))
            .collect()
    }

    #[test]
    fn restorable_windows_empty_registry() {
        // Positive: an empty registry yields nothing to restore.
        let (user, default) = default_rules();
        let reg = WindowRegistry::new(&user, &default);
        assert!(reg.restorable_windows().is_empty());
    }

    #[test]
    fn restorable_windows_includes_only_active_tiling() {
        // Only Active tiling windows are positioned by flow, so only they are
        // rescue candidates. Minimized/Hidden windows are not actively placed
        // and must be left alone on shutdown.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window_with_rect(
            &mut reg,
            101,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
            10,
        );
        insert_test_window_with_rect(
            &mut reg,
            102,
            WindowState::Tiling(TilingState::Minimized),
            20,
        );
        insert_test_window_with_rect(&mut reg, 103, WindowState::Tiling(TilingState::Hidden), 30);

        let map = collect_restorable(&reg);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&101));
        assert!(!map.contains_key(&102), "Minimized must not be restorable");
        assert!(!map.contains_key(&103), "Hidden must not be restorable");
    }

    #[test]
    fn restorable_windows_includes_only_active_floating() {
        // Only Active floating windows are positioned by flow.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window_with_rect(
            &mut reg,
            201,
            WindowState::Floating(FloatingState::Active {
                rect: crate::common::Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            }),
            10,
        );
        insert_test_window_with_rect(
            &mut reg,
            202,
            WindowState::Floating(FloatingState::Minimized),
            20,
        );
        insert_test_window_with_rect(
            &mut reg,
            203,
            WindowState::Floating(FloatingState::Hidden),
            30,
        );

        let map = collect_restorable(&reg);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&201));
        assert!(!map.contains_key(&202), "Minimized must not be restorable");
        assert!(!map.contains_key(&203), "Hidden must not be restorable");
    }

    #[test]
    fn restorable_windows_excludes_all_ignored_variants() {
        // Negative: every Ignored* variant is excluded — flow never moved them.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            301,
            WindowState::Ignored(IgnoredReason::Maximized),
        );
        insert_test_window(
            &mut reg,
            302,
            WindowState::Ignored(IgnoredReason::Fullscreen),
        );
        insert_test_window(
            &mut reg,
            303,
            WindowState::Ignored(IgnoredReason::ExplicitRule),
        );
        // One controlled window to ensure the filter is the cause of exclusion,
        // not just an empty registry.
        insert_test_window(
            &mut reg,
            304,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );

        let map = collect_restorable(&reg);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&304));
        assert!(!map.contains_key(&301));
        assert!(!map.contains_key(&302));
        assert!(!map.contains_key(&303));
    }

    #[test]
    fn restorable_windows_returns_pre_manage_rect_as_anchor() {
        // Positive: the returned Rect is the window's pre_manage_rect (the
        // pre-flow anchor), not some other rect field. We insert at a known x
        // and check it round-trips.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        let anchor_x = 4242;
        insert_test_window_with_rect(
            &mut reg,
            401,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
            anchor_x,
        );

        let map = collect_restorable(&reg);
        let rect = map
            .get(&401)
            .expect("controlled window should be present in restorable set");
        assert_eq!(rect.x, anchor_x);
        assert_eq!(rect.y, 0);
        assert_eq!(rect.width, 100);
        assert_eq!(rect.height, 100);
    }

    #[test]
    fn restorable_windows_returns_pre_manage_rect_not_tiled_rect() {
        // Positive: when both `pre_manage_rect` and `tiled_rect` are set (the
        // realistic mid-session case), the rescue anchor MUST be
        // `pre_manage_rect` — the position the window held *before* flow moved
        // it. Returning `tiled_rect` instead would defeat the rescue: it would
        // put the window back at the off-screen tiled position we are trying
        // to rescue it from. The plain `insert_test_window_with_rect` helper
        // leaves `tiled_rect = None`, so without this test a regression to
        // `tiled_rect.unwrap_or(pre_manage_rect)` would pass every other test.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        let pre_manage = crate::common::Rect {
            x: 100,
            y: 100,
            width: 800,
            height: 600,
        };
        // A wildly different "current tiled" rect — parked off-screen, where
        // flow put it (e.g. a non-active workspace).
        let tiled = crate::common::Rect {
            x: 5000,
            y: 5000,
            width: 400,
            height: 300,
        };

        let hwnd_val = 4242isize;
        let mut window = Window::new(
            HWND(hwnd_val as *mut _),
            "anchor.exe".into(),
            "Anchor".into(),
            "AnchorClass".into(),
            std::path::PathBuf::from("C:\\anchor.exe"),
            pre_manage,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
            crate::common::InvisibleBounds::zero(),
        );
        window.tiled_rect = Some(tiled);
        reg.windows.insert(hwnd_val, window);

        let map = collect_restorable(&reg);
        let returned = map
            .get(&hwnd_val)
            .expect("controlled window should be present in restorable set");
        assert_eq!(
            *returned, pre_manage,
            "rescue anchor must be pre_manage_rect, not tiled_rect"
        );
        assert_ne!(
            *returned, tiled,
            "rescue anchor must NOT be the current (off-screen) tiled rect"
        );
    }

    #[test]
    fn restorable_windows_returns_invisible_bounds() {
        // Positive: the 3rd tuple element is the window's invisible_bounds, so
        // the rescue pass can convert the window rect reported by GetWindowRect
        // back to the visible-content rect for the visibility test. Without it,
        // a parked window's invisible-border bleed into the work_area would
        // fool the rescue pass into leaving it stranded.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);

        let bounds = crate::common::InvisibleBounds {
            left: 7,
            top: 0,
            right: 7,
            bottom: 7,
        };
        let hwnd_val = 5001isize;
        let window = Window::new(
            HWND(hwnd_val as *mut _),
            "bounded.exe".into(),
            "Bounded".into(),
            "BoundedClass".into(),
            std::path::PathBuf::from("C:\\bounded.exe"),
            crate::common::Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
            bounds,
        );
        reg.windows.insert(hwnd_val, window);

        let got = reg.restorable_windows();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, hwnd_val);
        assert_eq!(got[0].2, bounds, "invisible_bounds must round-trip");
    }

    #[test]
    fn restorable_windows_mixed_registry_filters_correctly() {
        // Realistic mix — only Active tile/float windows surface. Minimized,
        // Hidden, and Ignored windows are all left alone on shutdown.
        let (user, default) = default_rules();
        let mut reg = WindowRegistry::new(&user, &default);
        insert_test_window(
            &mut reg,
            1,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        insert_test_window(&mut reg, 2, WindowState::Tiling(TilingState::Minimized));
        insert_test_window(
            &mut reg,
            3,
            WindowState::Floating(FloatingState::Active {
                rect: crate::common::Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            }),
        );
        insert_test_window(&mut reg, 4, WindowState::Floating(FloatingState::Hidden));
        insert_test_window(&mut reg, 5, WindowState::Ignored(IgnoredReason::Maximized));
        insert_test_window(
            &mut reg,
            6,
            WindowState::Ignored(IgnoredReason::ExplicitRule),
        );

        let map = collect_restorable(&reg);
        // Only hwnds 1 and 3 are Active; the rest are minimized/hidden/ignored.
        assert_eq!(map.len(), 2);
        for key in [1, 3] {
            assert!(map.contains_key(&key), "active hwnd {key} missing");
        }
        for key in [2, 4, 5, 6] {
            assert!(!map.contains_key(&key), "non-active hwnd {key} leaked");
        }
    }

    // --- set_user_rules (hot-reload) tests ---

    /// Positive: `WindowRegistry::set_user_rules` MUST delegate to the
    /// classification pipeline so a hot-reloaded `default_action` and rule list
    /// change subsequent classifications. Verified by observing the pipeline
    /// directly (no Win32 / no windows inserted) — the daemon's
    /// `dispatch_reload_config` calls this method for the non-fatal rules reload.
    #[test]
    fn set_user_rules_delegates_to_the_classification_pipeline() {
        use crate::config::types::MatchRule;
        use classification::WindowCandidate;

        // Arrange: registry starts with default_action = Tile and a rule that
        // pins "pinned.exe" to Ignore.
        let user = WindowRulesConfig {
            default_action: WindowAction::Tile,
            rules: vec![WindowRule {
                match_: MatchRule {
                    exe: Some("pinned.exe".into()),
                    ..Default::default()
                },
                action: WindowAction::Ignore,
                initial_width_px: None,
                override_persist: false,
            }],
        };
        let default = WindowRulesConfig::default();
        let mut reg = WindowRegistry::new(&user, &default);

        let pinned = WindowCandidate {
            exe: "pinned.exe".into(),
            title: String::new(),
            class: String::new(),
            process_path: String::new(),
        };
        let other = WindowCandidate {
            exe: "other.exe".into(),
            title: String::new(),
            class: String::new(),
            process_path: String::new(),
        };
        // Guard: original rules are in effect.
        assert_eq!(reg.pipeline.classify(&pinned), WindowAction::Ignore);
        assert_eq!(reg.pipeline.classify(&other), WindowAction::Tile);

        // Act: hot-reload — drop "pinned.exe", switch default to Float.
        reg.set_user_rules(WindowRulesConfig {
            default_action: WindowAction::Float,
            rules: vec![],
        });

        // Assert: delegation took effect — old rule gone, default refreshed.
        assert_eq!(
            reg.pipeline.classify(&pinned),
            WindowAction::Float,
            "registry.set_user_rules must drop old rules via delegation"
        );
        assert_eq!(
            reg.pipeline.classify(&other),
            WindowAction::Float,
            "registry.set_user_rules must refresh default_action via delegation"
        );
    }

    // ── has_parent / has_owner pre-filter predicates ──────────────────
    //
    // `has_parent` and `has_owner` are private predicates that call real
    // Win32 APIs (`GetWindowLongW(GWL_STYLE)` / `GetWindow(GW_OWNER)`) with no
    // test seam — the same shape as the rest of the `win32::` helpers. The
    // pure-data `insert_test_window` pattern used above cannot reach them,
    // because it bypasses the pre-filter by inserting straight into the
    // HashMap.
    //
    // The integration tests in `tests/cli/` exercise the ACCEPT path
    // (top-level windows pass the filter and get tiled) but they are too
    // slow and racy to anchor a REJECT-path regression guard — 13 of them
    // carry `#[ignore = "...startup hook race"]`. The REJECT path (a
    // `WS_CHILD` control being filtered out) had NO automated coverage at
    // all before these tests.
    //
    // These tests close that gap deterministically: they spin up two real
    // Win32 windows (a top-level parent + a `WS_CHILD` child) in the test
    // process itself, call the predicate directly, and tear them down — no
    // daemon, no IPC, no hooks, no race. Window creation happens on
    // whatever desktop the test process owns; if the host has no interactive
    // desktop (headless CI without a session), `CreateWindowExW` fails and
    // the test skips itself rather than falsely failing.
    use has_parent_test_helpers::*;

    /// Private helper module holding the Win32 imports and the window-proc
    /// / RAII plumbing needed by the `has_parent` tests below.
    ///
    /// Sequestered into a sub-module so the (many) `windows::` imports
    /// don't leak into the rest of the test module's namespace.
    mod has_parent_test_helpers {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;

        use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::Graphics::Gdi::HBRUSH;
        use windows::Win32::UI::WindowsAndMessaging::{
            CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, HCURSOR, HICON,
            RegisterClassExW, WINDOW_EX_STYLE, WNDCLASSEXW, WS_CHILD, WS_OVERLAPPEDWINDOW,
        };
        use windows::core::PCWSTR;

        /// Minimal window procedure — just delegates to `DefWindowProcW`.
        /// Required so `CreateWindowExW` has a valid `lpfnWndProc`.
        pub unsafe extern "system" fn test_wnd_proc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        /// Convert a Rust string to a null-terminated UTF-16 `Vec<u16>`.
        pub fn wide(s: &str) -> Vec<u16> {
            OsStr::new(s)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        }

        /// RAII guard that registers a private window class (idempotent
        /// across tests) and creates two windows against it: an invisible
        /// top-level `WS_OVERLAPPEDWINDOW` parent and a `WS_CHILD` child
        /// whose parent is that top-level window. Both are destroyed on drop.
        ///
        /// Returns `None` if window creation fails — callers should skip
        /// the test (headless CI without an interactive desktop).
        pub struct RealTestWindows {
            pub parent: HWND,
            pub child: HWND,
        }

        impl RealTestWindows {
            pub fn create() -> Option<Self> {
                let class_name = wide("FlowHasParentTestClass");
                let wnd_class = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(test_wnd_proc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: HINSTANCE::default(),
                    hIcon: HICON::default(),
                    hCursor: HCURSOR::default(),
                    hbrBackground: HBRUSH::default(),
                    lpszMenuName: PCWSTR::null(),
                    lpszClassName: PCWSTR(class_name.as_ptr()),
                    hIconSm: HICON::default(),
                };
                // Idempotent: returns 0 if already registered (parallel
                // tests), which is harmless — the class is process-global.
                unsafe {
                    let _ = RegisterClassExW(&wnd_class);
                }

                // Top-level parent: no hWndParent → parent is the desktop
                // window. Created WITHOUT WS_VISIBLE so the user's desktop
                // is not disturbed.
                let parent = unsafe {
                    CreateWindowExW(
                        WINDOW_EX_STYLE::default(),
                        PCWSTR(class_name.as_ptr()),
                        PCWSTR(wide("FlowHasParentTopLevel").as_ptr()),
                        WS_OVERLAPPEDWINDOW,
                        0,
                        0,
                        100,
                        100,
                        None,
                        None,
                        None,
                        None,
                    )
                }
                .ok()?;

                // Child control: WS_CHILD with hWndParent = `parent`. This
                // is the exact shape of the Inno Setup TNew* / Win32
                // Button/Static controls that the pre-filter rejects.
                let child = unsafe {
                    CreateWindowExW(
                        WINDOW_EX_STYLE::default(),
                        PCWSTR(class_name.as_ptr()),
                        PCWSTR(wide("FlowHasParentChild").as_ptr()),
                        WS_CHILD,
                        0,
                        0,
                        50,
                        50,
                        Some(parent),
                        None,
                        None,
                        None,
                    )
                }
                .ok()?;

                Some(Self { parent, child })
            }
        }

        impl Drop for RealTestWindows {
            fn drop(&mut self) {
                // Destroy child first so the parent's child list is
                // consistent during teardown. Errors are ignored: by drop
                // time the test has already captured its result, and a
                // failed destroy would leak only transient invisible
                // windows in the test process.
                unsafe {
                    let _ = DestroyWindow(self.child);
                    let _ = DestroyWindow(self.parent);
                }
            }
        }
    }

    #[test]
    fn has_parent_returns_false_for_toplevel_window() {
        // Positive: a top-level window (WS_OVERLAPPEDWINDOW, no WS_CHILD style)
        // is not a child control. `has_parent` must return `false`, allowing
        // the window through the pre-filter so it can be tiled.
        let windows = match RealTestWindows::create() {
            Some(w) => w,
            None => {
                eprintln!("skipping: real window creation failed (headless test environment?)");
                return;
            }
        };
        assert!(
            !has_parent(windows.parent),
            "top-level window should not be flagged as WS_CHILD"
        );
    }

    #[test]
    fn has_parent_returns_true_for_child_window() {
        // Negative: a WS_CHILD window must be flagged by `has_parent`,
        // blocking it at the pre-filter. This is the regression guard for
        // the Inno Setup TNew* leak and the now-removed `Button` / `Static`
        // / `ComboBox` rules in default-flow-rules.toml: if `has_parent`
        // regresses, those controls would once again slip into the tiling
        // pipeline and draw border overlays over dialog buttons.
        let windows = match RealTestWindows::create() {
            Some(w) => w,
            None => {
                eprintln!("skipping: real window creation failed (headless test environment?)");
                return;
            }
        };
        assert!(
            has_parent(windows.child),
            "WS_CHILD window should be flagged as having a parent"
        );
        // Lock in the root cause: WS_CHILD controls have NO owner, so has_owner
        // alone cannot catch them — this is exactly the gap has_parent fills.
        assert!(
            !has_owner(windows.child),
            "WS_CHILD controls should have no GW_OWNER — that's why has_parent exists"
        );
    }
}
