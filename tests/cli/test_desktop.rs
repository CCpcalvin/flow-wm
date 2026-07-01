//! Test infrastructure for WindowRegistry integrated tests.
//!
//! Provides a [`TestDesktop`] RAII guard that creates an isolated Windows
//! desktop for testing. All dummy windows and the daemon's hook thread
//! operate on this desktop, leaving the user's main desktop untouched.

use std::ffi::OsStr;
use std::ops::{Deref, DerefMut};
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

/// Owns an `stmd` child process and **force-kills it on drop**.
///
/// # Why this exists
///
/// A plain [`std::process::Child`] is *not* killed when dropped — Rust only
/// closes the handle, leaving the OS process running. In the integration
/// tests that is a hazard: the daemon runs its own Win32 message/hook loop
/// and, on an isolated test desktop, can stall mid-`SetWindowPos` during
/// layout recomputation. When that happens:
///
/// 1. The [`DaemonGuard`] tries `Stop` via the IPC transport. The transport's
///    overlapped read is deadline-bounded (see `send_message_to`), so this
///    returns within ~30 s rather than hanging forever — *but* the daemon may
///    still be alive (it acknowledged `Stop` yet blocked before exiting, or it
///    never read the `Stop` at all).
/// 2. Without a kill, the daemon process is orphaned: its parent test process
///    is gone, nothing will ever wake it from its blocking kernel wait, and it
///    lingers forever (confirmed in debugging — orphaned `stmd.exe` parked at
///    0 % CPU that no test can recover).
///
/// `KillingChild` closes that hole. On drop it checks whether the daemon has
/// already exited (via [`Child::try_wait`]); if not, it calls
/// [`Child::kill`] ([`TerminateProcess`]) — which is unconditional on Windows
/// and terminates even a process blocked in a kernel wait — then reaps it with
/// [`Child::wait`]. This is idempotent: an already-exited daemon is left
/// alone, so normal (graceful) shutdown is unaffected.
///
/// Because drop order on a panic runs the [`DaemonGuard`] *first* (graceful
/// `Stop`) and the daemon handle *second*, the daemon always gets a chance to
/// exit cleanly before being force-killed.
///
/// [`Child::try_wait`]: std::process::Child::try_wait
/// [`Child::kill`]: std::process::Child::kill
/// [`Child::wait`]: std::process::Child::wait
/// [`TerminateProcess`]: windows::Win32::System::Threading::TerminateProcess
pub struct KillingChild(std::process::Child);

impl KillingChild {
    /// Wrap a freshly-spawned daemon [`Child`] so it is force-killed on drop.
    pub fn new(child: std::process::Child) -> Self {
        Self(child)
    }
}

/// Transparent access to the underlying [`Child`] so existing call sites that
/// use `child.try_wait()` / `child.id()` etc. keep working unchanged.
impl Deref for KillingChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for KillingChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for KillingChild {
    fn drop(&mut self) {
        // If the daemon already exited (graceful `Stop` succeeded) there is
        // nothing to do — and we must not call `kill()` on a dead process.
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        // Otherwise force-terminate (unconditional on Windows) and reap to
        // avoid a zombie, ignoring errors: the process may have exited in the
        // race window between `try_wait` and `kill`.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start `stmd` in test mode on the given desktop.
///
/// The daemon is spawned with `--desktop <name>` so both its main thread
/// and hook thread join the isolated test desktop.
///
/// The returned [`KillingChild`] force-kills the daemon when dropped, so a
/// test that panics — or a daemon that stalls on the isolated desktop — can
/// never leak an orphaned `stmd.exe`. See [`KillingChild`] for details.
pub fn start_test_daemon(pipe: &str, desktop_name: &str) -> Result<KillingChild, String> {
    start_test_daemon_with_extra_args(pipe, desktop_name, &[])
}

/// Start `stmd` in test mode with additional CLI arguments (e.g. `--log-file`).
///
/// Same as [`start_test_daemon`] but appends caller-supplied arguments after
/// the standard `--desktop` flag. Used by tests that need to redirect the
/// daemon's log to an isolated file (the default daily log is shared across
/// parallel test daemons, so reading it back is racy).
pub fn start_test_daemon_with_extra_args(
    pipe: &str,
    desktop_name: &str,
    extra_args: &[&str],
) -> Result<KillingChild, String> {
    let exe = assert_cmd::cargo_bin!("stmd");

    // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
    const DETACHED: u32 = 0x00000200 | 0x08000000;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--desktop")
        .arg(desktop_name)
        .env("STM_PIPE_NAME", pipe)
        .creation_flags(DETACHED);

    // Per-test filesystem key derived from the (unique) pipe name. Used for
    // both the isolated config directory and the log file below so parallel
    // tests never collide on disk.
    let safe: String = pipe
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();

    // Point the daemon at an isolated, per-test config directory seeded with a
    // rules file that tiles unknown windows.
    //
    // The compiled default is `default_action = "float"` (see `src/config/
    // types.rs` and `default-stm-rules.toml`). Every window the tests create
    // (class `StmTestClass`, exe `cli-*.exe`, title `StmTest-*`) matches NO
    // rule at any layer, so without this override the classifier falls back to
    // `float` and no test window ever tiles — every integration assertion that
    // expects tiled columns would see `columns: []`. A caller that explicitly
    // passes its own `--config` in `extra_args` wins.
    const TEST_RULES_TOML: &str = include_str!("fixtures/stm-rules.toml");
    let config_overridden = extra_args.contains(&"--config");
    if !config_overridden {
        let config_dir = std::env::temp_dir().join(format!("stm-test-config-{safe}"));
        std::fs::create_dir_all(&config_dir)
            .map_err(|e| format!("failed to create test config dir: {e}"))?;
        let rules_path = config_dir.join("stm-rules.toml");
        std::fs::write(&rules_path, TEST_RULES_TOML)
            .map_err(|e| format!("failed to write test stm-rules.toml: {e}"))?;
        eprintln!("[test] stmd config dir -> {}", config_dir.display());
        cmd.arg("--config").arg(&config_dir);
    }

    // Redirect the daemon log away from the user's real `~/.config/stm/logs/`
    // daily log (shared with any live daemon, so reading it back per-test is
    // racy). The pipe name is unique per test, so it keys a unique temp log;
    // the daemon truncates the file on each start. A caller that explicitly
    // passes its own `--log-file` in `extra_args` wins.
    let already_redirected = extra_args.contains(&"--log-file");
    if !already_redirected {
        let log_path = std::env::temp_dir().join(format!("stmd-test-{safe}.log"));
        eprintln!("[test] stmd log -> {}", log_path.display());
        cmd.arg("--log-file").arg(log_path);
    }

    for arg in extra_args {
        cmd.arg(arg);
    }
    let mut child = cmd
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
                    return Ok(KillingChild::new(child));
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
    use windows::Win32::Foundation::CloseHandle;
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

/// Send `QueryLayoutVirtual` and return the JSON response.
///
/// Returns the virtual layout structure — `viewport_offset`, `column_count`,
/// `window_count`, and a `columns` array where each entry has `index`,
/// `width_px`, and `rows` (the window IDs in column order). This is the
/// most direct way to verify that a column swap changed the layout.
pub fn query_layout_virtual(pipe: &str) -> Result<serde_json::Value, String> {
    use scrolling_tiling_manager::ipc::message::{SocketMessage, SocketResponse};
    use scrolling_tiling_manager::ipc::transport;

    let response = transport::send_message_to(pipe, &SocketMessage::QueryLayoutVirtual)
        .map_err(|e| format!("query_layout_virtual failed: {e}"))?;

    match response {
        SocketResponse::Data { payload } => Ok(payload),
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        SocketResponse::Ok => Err("unexpected Ok response".into()),
    }
}

/// Send an IPC message and discard the response.
///
/// Useful for fire-and-forget commands during test setup where the command may
/// legitimately fail (e.g. focusing left when already at the leftmost column).
pub fn send_ipc_ignore(pipe: &str, msg: &scrolling_tiling_manager::ipc::message::SocketMessage) {
    use scrolling_tiling_manager::ipc::transport;
    let _ = transport::send_message_to(pipe, msg);
}

/// Send `stm query layout actual` and return the JSON response.
///
/// Returns the actual (projected) layout: pixel-level rects for each window
/// after projection and padding. Intended for integration tests to verify that
/// remaining windows physically shift left after a column is removed.
#[allow(dead_code)]
pub fn query_layout_actual(pipe: &str) -> Result<serde_json::Value, String> {
    use scrolling_tiling_manager::ipc::message::{SocketMessage, SocketResponse};
    use scrolling_tiling_manager::ipc::transport;

    let response = transport::send_message_to(pipe, &SocketMessage::QueryLayoutActual)
        .map_err(|e| format!("query layout actual failed: {e}"))?;

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
