//! Test infrastructure for WindowRegistry integrated tests.
//!
//! Provides a [`TestDesktop`] RAII guard that creates an isolated Windows
//! desktop for testing. All dummy windows and the daemon's hook thread
//! operate on this desktop, leaving the user's main desktop untouched.
//!
//! All functions are Windows-only and gated with `#[cfg(target_os = "windows")]`.

#![cfg(target_os = "windows")]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::StationsAndDesktops::HDESK;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, HCURSOR,
    HICON, RegisterClassExW, SW_MINIMIZE, SW_RESTORE, ShowWindow, WINDOW_EX_STYLE, WNDCLASSEXW,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows::core::PCWSTR;

use scrolling_tiling_manager::registry::desktop;

/// Per-test unique counter for desktop names.
static DESKTOP_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Per-test unique title counter.
static TITLE_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Generates a unique desktop name.
fn unique_desktop_name() -> String {
    let id = DESKTOP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("stm-test-{id}")
}

/// Generates a unique window title with a test prefix.
pub fn unique_title(base: &str) -> String {
    let id = TITLE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("StmTest-{base}-{id}")
}

// ── TestDesktop ─────────────────────────────────────────────────────

/// RAII guard for an isolated test desktop.
///
/// On creation:
/// 1. Creates a new Windows desktop via `CreateDesktopW`.
/// 2. Saves the current thread's desktop handle.
/// 3. Switches the calling thread to the new desktop.
///
/// On drop:
/// 1. Switches the thread back to the original desktop.
/// 2. Closes the test desktop handle (desktop is destroyed when all handles
///    are closed and no threads remain on it).
///
/// # Usage
///
/// ```ignore
/// let td = TestDesktop::create().expect("desktop");
/// // Thread is now on the isolated desktop.
/// let w = TestWindow::create("my-window", &td).expect("window");
/// // w lives on the isolated desktop.
/// drop(w);
/// drop(td); // switches back, closes desktop.
/// ```
pub struct TestDesktop {
    /// The desktop name (passed to `stmd --desktop`).
    pub name: String,
    /// Handle to the created test desktop.
    desktop: HDESK,
    /// Handle to the original desktop (to restore on drop).
    original: HDESK,
}

impl TestDesktop {
    /// Creates a new isolated desktop and switches the calling thread to it.
    pub fn create() -> Result<Self, String> {
        let name = unique_desktop_name();

        // Save the current desktop so we can restore it on drop.
        let original = desktop::current_desktop()?;

        // Create the new desktop.
        let desk_handle = desktop::create_desktop(&name)?;

        // Switch the calling thread to the new desktop.
        desktop::set_thread_desktop(desk_handle)?;

        log::info!("test: created and switched to desktop '{name}'");
        Ok(Self {
            name,
            desktop: desk_handle,
            original,
        })
    }
}

impl Drop for TestDesktop {
    fn drop(&mut self) {
        // Switch back to the original desktop.
        if let Err(e) = desktop::set_thread_desktop(self.original) {
            log::error!("test: failed to restore original desktop: {e}");
        }

        // Close the test desktop handle.
        desktop::close_desktop(self.desktop);

        log::info!("test: cleaned up desktop '{}'", self.name);
    }
}

// ── TestWindow ──────────────────────────────────────────────────────

/// RAII guard for a test window. Destroys the window on drop.
pub struct TestWindow {
    /// The window handle.
    pub hwnd: HWND,
    /// The window title.
    pub title: String,
}

impl TestWindow {
    /// Creates a visible top-level window on the current thread's desktop.
    ///
    /// The calling thread should already be on the test desktop via
    /// [`TestDesktop::create`].
    pub fn create(title: &str) -> Result<Self, String> {
        let class_name = wide("StmTestClass");
        register_test_class(&class_name)?;

        let wide_title = wide(title);

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(wide_title.as_ptr()),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                640,
                480,
                None,
                None,
                None,
                None,
            )
        }
        .map_err(|e| format!("CreateWindowExW failed for '{title}': {e}"))?;

        // Give the window a moment to be fully created and visible.
        std::thread::sleep(std::time::Duration::from_millis(200));

        log::info!("test: created window '{title}' (hwnd={:?})", hwnd);
        Ok(Self {
            hwnd,
            title: title.to_owned(),
        })
    }

    /// Minimizes the window.
    pub fn minimize(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        log::debug!("test: minimized window '{}'", self.title);
    }

    /// Restores the window from minimize.
    pub fn restore(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_RESTORE);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        log::debug!("test: restored window '{}'", self.title);
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        log::info!("test: destroyed window '{}'", self.title);
    }
}

// ── Daemon helpers ──────────────────────────────────────────────────

/// Start `stmd` in test mode on the given desktop.
///
/// The daemon is spawned with `--desktop <name>` so both its main thread
/// and hook thread join the isolated test desktop.
pub fn start_test_daemon(pipe: &str, desktop_name: &str) -> Result<std::process::Child, String> {
    let exe = assert_cmd::cargo_bin!("stmd");

    // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
    const DETACHED: u32 = 0x00000200 | 0x08000000;

    let mut child = std::process::Command::new(&exe)
        .arg("--desktop")
        .arg(desktop_name)
        .env("STM_PIPE_NAME", pipe)
        .creation_flags(DETACHED)
        .spawn()
        .map_err(|e| format!("failed to spawn stmd: {e}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait().map_err(|e| format!("wait error: {e}"))? {
            Some(status) => {
                if !status.success() {
                    return Err(format!("stmd exited with {status}"));
                }
                return Err("stmd exited unexpectedly with success".into());
            }
            None => {
                if is_pipe_available(pipe) {
                    return Ok(child);
                }
                if std::time::Instant::now() >= deadline {
                    return Err("timed out waiting for test daemon to start".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}

/// Check if the named pipe is available.
fn is_pipe_available(pipe: &str) -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide = wide(pipe);
    let result = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };

    if let Ok(handle) = result {
        unsafe {
            let _ = CloseHandle(handle);
        }
        true
    } else {
        false
    }
}

// ── DaemonGuard ─────────────────────────────────────────────────────

/// RAII guard that stops the test daemon on drop.
///
/// If a test panics after starting the daemon, `stop_test_daemon` is called
/// during unwinding, preventing orphaned `stmd.exe` processes.
///
/// # Usage
///
/// ```ignore
/// let _guard = DaemonGuard::new(&pipe);
/// // ... test assertions that may panic ...
/// // daemon is stopped automatically when `_guard` is dropped
/// ```
pub struct DaemonGuard {
    pipe: String,
}

impl DaemonGuard {
    /// Create a new guard that will stop the daemon for `pipe` on drop.
    pub fn new(pipe: &str) -> Self {
        Self {
            pipe: pipe.to_owned(),
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        stop_test_daemon(&self.pipe);
        // Give the daemon process time to exit cleanly.
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

// ── IPC helpers ─────────────────────────────────────────────────────

/// Send `stm query windows` and return the JSON response.
///
/// Uses [`transport::send_message_to`] with the pipe name directly — no
/// environment variable mutation, safe for concurrent test threads.
pub fn query_windows(pipe: &str) -> Result<serde_json::Value, String> {
    use scrolling_tiling_manager::ipc::message::{SocketMessage, SocketResponse};
    use scrolling_tiling_manager::ipc::transport;

    let response = transport::send_message_to(pipe, &SocketMessage::QueryWindowsAll)
        .map_err(|e| format!("query failed: {e}"))?;

    match response {
        SocketResponse::Data { payload } => Ok(payload),
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        SocketResponse::Ok => Err("unexpected Ok response".into()),
    }
}

/// Stop the test daemon by sending the Stop IPC command.
///
/// Uses [`transport::send_message_to`] with the pipe name directly — no
/// environment variable mutation, safe for concurrent test threads.
pub fn stop_test_daemon(pipe: &str) {
    use scrolling_tiling_manager::ipc::message::SocketMessage;
    use scrolling_tiling_manager::ipc::transport;

    let _ = transport::send_message_to(pipe, &SocketMessage::Stop);
}

// ── Window class helpers ────────────────────────────────────────────

/// Registers the test window class (idempotent).
fn register_test_class(class_name: &[u16]) -> Result<(), String> {
    let wnd_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(test_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: windows::Win32::Foundation::HINSTANCE::default(),
        hIcon: HICON::default(),
        hCursor: HCURSOR::default(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hIconSm: HICON::default(),
    };

    let atom = unsafe { RegisterClassExW(&wnd_class) };
    if atom == 0 {
        log::debug!("test: RegisterClassExW returned 0 (class may already exist)");
    }

    Ok(())
}

/// Minimal window procedure.
unsafe extern "system" fn test_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// ── String conversion ───────────────────────────────────────────────

/// Convert a Rust string to a null-terminated UTF-16 `Vec<u16>`.
pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
