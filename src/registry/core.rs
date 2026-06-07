//! Core window registry — authoritative source of truth for all tracked windows.
//!
//! [`WindowRegistry`] maintains a `HashMap` of all windows the daemon is aware
//! of, keyed by their Win32 `HWND` value (stored as `isize` for `Send` safety).
//! It provides methods for initialization scanning, event-driven updates, and
//! JSON serialization for the query API.

use std::collections::HashMap;

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GW_OWNER, GetWindow};

use crate::config::types::{StmConfig, WindowAction, WindowRule};

use super::classification;
use super::hooks::HookEvent;
use super::types::{FloatingState, IgnoredReason, TilingState, VirtualSlot, Window, WindowState};
use super::win32;

/// The authoritative source of truth for every window the daemon is aware of.
///
/// The registry is updated via two paths:
/// 1. **Initialization** — [`scan_existing_windows`](Self::scan_existing_windows) enumerates
///    all current top-level windows on the desktop.
/// 2. **Events** — [`process_pending_events`](Self::process_pending_events) drains the
///    WinEvent hook channel and applies state transitions.
///
/// The registry is shared between the IPC thread (reads for queries) and the
/// WinEvent hook thread (writes on events) via `Arc<Mutex<WindowRegistry>>`.
pub struct WindowRegistry {
    /// All tracked windows, keyed by HWND value (`isize` for `Send` safety).
    windows: HashMap<isize, Window>,

    /// Currently focused window handle value.
    focused: Option<isize>,

    /// Default classification action for windows not matching any rule.
    default_action: WindowAction,

    /// Window classification rules (evaluated top-to-bottom, first match wins).
    window_rules: Vec<WindowRule>,
}

impl WindowRegistry {
    /// Creates a new empty registry with classification settings from config.
    #[must_use]
    pub fn new(config: &StmConfig) -> Self {
        Self {
            windows: HashMap::new(),
            focused: None,
            default_action: config.default_window_action,
            window_rules: config.window_rules.clone(),
        }
    }

    /// Scans all existing top-level windows and registers them.
    ///
    /// Called once at daemon startup to build the initial registry state.
    /// Only registers windows that are visible, have no owner (top-level),
    /// and have a non-empty title.
    ///
    /// If `EnumWindows` fails (e.g., on an isolated test desktop where the
    /// process lacks access), logs a warning and returns `Ok(())` — the
    /// hook thread will still catch windows via events.
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
    /// If the window is already registered, this is a no-op (the existing
    /// entry is preserved).
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

        let state = classification::classify_with_state(
            &candidate,
            info.is_maximized,
            info.is_fullscreen,
            &self.window_rules,
            self.default_action,
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
    /// Called when `EVENT_OBJECT_DESTROY` is received. If the window was
    /// focused, clears the focus.
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
    /// Called when `EVENT_SYSTEM_FOREGROUND` is received.
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
    /// Called when `EVENT_SYSTEM_MINIMIZESTART` is received. Preserves the
    /// window's virtual slot for future restore.
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
    /// Called when `EVENT_SYSTEM_MINIMIZEEND` is received. Restores the
    /// window to its previous sub-state (tiling or floating).
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
    /// This must be called periodically from the IPC thread (which owns the
    /// `MutexGuard`) to process events queued by the WinEvent hook thread.
    /// Uses `try_recv` for non-blocking operation.
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

    /// Handles a window creation event.
    ///
    /// Gathers window info, classifies, and registers if appropriate.
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
fn hwnd_key(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

/// Converts a `WindowState` to a JSON value for the query API.
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
/// Returns a `Vec<HWND>` of all top-level window handles.
///
/// # Errors
///
/// Returns an error string if `EnumWindows` fails.
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
/// # Safety
///
/// The `l_param` must be a valid pointer to a `Vec<HWND>` created by the caller.
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
    use crate::config::types::StmConfig;

    #[test]
    fn new_registry_is_empty() {
        let config = StmConfig::default();
        let reg = WindowRegistry::new(&config);
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.focused.is_none());
    }

    #[test]
    fn to_json_value_empty_registry() {
        let config = StmConfig::default();
        let reg = WindowRegistry::new(&config);
        let json = reg.to_json_value();

        assert_eq!(json["count"], 0);
        assert!(json["windows"].as_array().unwrap().is_empty());
        assert!(json["focused"].is_null());
    }

    #[test]
    fn to_json_value_has_correct_structure() {
        let config = StmConfig::default();
        let reg = WindowRegistry::new(&config);
        let json = reg.to_json_value();

        assert!(json.get("windows").is_some());
        assert!(json.get("focused").is_some());
        assert!(json.get("count").is_some());
    }

    #[test]
    fn remove_window_clears_focus() {
        let mut config = StmConfig::default();
        config.default_window_action = WindowAction::Tile;

        let mut reg = WindowRegistry::new(&config);
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
        let config = StmConfig::default();
        let mut reg = WindowRegistry::new(&config);
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
        let config = StmConfig::default();
        let mut reg = WindowRegistry::new(&config);
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
        let config = StmConfig::default();
        let mut reg = WindowRegistry::new(&config);

        // No windows in registry — focus should remain None.
        reg.set_focused(99999);
        assert!(reg.focused.is_none());
    }

    #[test]
    fn set_focused_changes_between_windows() {
        let config = StmConfig::default();
        let mut reg = WindowRegistry::new(&config);

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
        let config = StmConfig::default();
        let mut reg = WindowRegistry::new(&config);
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
        let config = StmConfig::default();
        let mut reg = WindowRegistry::new(&config);
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
        let mut config = StmConfig::default();
        config.default_window_action = WindowAction::Tile;
        let mut reg = WindowRegistry::new(&config);

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
        let config = StmConfig::default();
        let mut reg = WindowRegistry::new(&config);

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
        let config = StmConfig::default();
        let mut reg = WindowRegistry::new(&config);
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
        let config = StmConfig::default();
        let mut reg = WindowRegistry::new(&config);
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
