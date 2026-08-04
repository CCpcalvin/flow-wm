//! Shared helpers for CLI integration tests.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use assert_cmd::Command;

/// Per-test unique pipe name counter.
static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Maximum time to wait for any single `flow` command to complete.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// Environment variable name for the pipe path (must match `ipc::message::PIPE_ENV`).
const PIPE_ENV: &str = "FLOW_PIPE_NAME";

/// Generate a unique pipe name for this test run.
///
/// Each call returns a different name like `\\.\pipe\flow-test-0`, `\\.\pipe\flow-test-1`, etc.
/// Tests that use separate pipe names can run in parallel without interference.
pub fn unique_pipe_name() -> String {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(r"\\.\pipe\flow-test-{id}")
}

/// Build an [`assert_cmd::Command`] for the `flow` CLI binary pre-configured with the given
/// pipe name and a default command timeout.
///
/// Suitable for CLI commands that do **not** spawn a daemon (`stop`,
/// `reload-config`, etc.). For spawning the daemon in tests, use
/// [`crate::test_desktop::start_test_daemon`] which runs it on an isolated
/// desktop.
pub fn flow(pipe: &str) -> Command {
    let mut cmd = Command::cargo_bin("flow").expect("flow binary should be built by cargo test");
    cmd.env(PIPE_ENV, pipe).timeout(COMMAND_TIMEOUT);
    cmd
}

/// Ensure the daemon for the given pipe is stopped, ignoring errors if it was not running.
///
/// Call this at the start and end of each test to guarantee a clean slate
/// and avoid leaving a daemon process running after a test failure.
pub fn ensure_daemon_stopped(pipe: &str) {
    let _ = flow(pipe).arg("stop").assert();
}

// ── Subscriber-pipe helper (test-as-subscriber) ─────────────────────
//
// The event tests play the subscriber role: the test process creates its own
// named pipe (server side), lets the daemon connect as a client after sending
// `Subscribe`, and reads newline-delimited JSON `Event`s off it. This is the
// only new test helper the event-broadcast subsystem needs.

use std::sync::mpsc::{Receiver, channel};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{PIPE_ACCESS_DUPLEX, ReadFile};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::core::PCWSTR;

/// HRESULT encoding of Win32 `ERROR_PIPE_CONNECTED` (535) — `ConnectNamedPipe`
/// returns it when a client connected before the call was made (a success).
const ERROR_PIPE_CONNECTED_HRESULT: windows::core::HRESULT =
    windows::core::HRESULT(0x8007_0217_u32 as i32);

/// Read buffer size for the subscriber pipe (mirrors the daemon's `BUF_SIZE`).
const SUB_BUF_SIZE: u32 = 8192;

/// RAII guard for a subscriber-side named pipe used by event tests.
///
/// Create with [`SubscriberPipe::create`], then read newline-delimited JSON
/// [`Event`](flow_wm::events::Event) lines with [`SubscriberPipe::read_line`].
/// The underlying handle is closed on drop.
pub struct SubscriberPipe {
    /// Raw handle value, stored as `isize` so the guard is `Send`-ish across
    /// the spawned connect/read threads.
    raw: isize,
}

impl SubscriberPipe {
    /// Create the subscriber pipe and post a background `ConnectNamedPipe`.
    ///
    /// Returns the pipe plus a receiver that resolves once the daemon has
    /// connected (i.e. after the test sends `Subscribe`). `ConnectNamedPipe`
    /// blocks until a client connects, so it runs on a background thread —
    /// the test thread must be free to send the `Subscribe` message that
    /// triggers the daemon’s connect.
    pub fn create(name: &str) -> Result<(Self, Receiver<Result<(), String>>), String> {
        let wide = wide(name);
        // SAFETY: `wide` produces a null-terminated UTF-16 string; the created
        // pipe is a fresh kernel object the caller owns via this guard.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                SUB_BUF_SIZE,
                SUB_BUF_SIZE,
                0,
                None,
            )
        };
        if handle.is_invalid() {
            return Err(format!("CreateNamedPipeW failed for '{name}'"));
        }
        let raw = handle.0 as isize;
        let raw_for_thread = raw;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            // SAFETY: the handle is owned by the SubscriberPipe guard on the
            // main thread; the connect is a blocking wait for one client.
            let h = HANDLE(raw_for_thread as *mut _);
            let result = match unsafe { ConnectNamedPipe(h, None) } {
                Ok(()) => Ok(()),
                Err(e) if e.code() == ERROR_PIPE_CONNECTED_HRESULT => Ok(()),
                Err(e) => Err(format!("ConnectNamedPipe: {e}")),
            };
            let _ = tx.send(result);
        });
        Ok((Self { raw }, rx))
    }

    /// Read one newline-terminated line, bounded by `timeout`.
    ///
    /// The blocking `ReadFile` runs on a background thread so a missing line
    /// surfaces as a timeout instead of hanging the test. The snapshot is
    /// pushed synchronously during `Subscribe` dispatch (before the `Ok` ack),
    /// so the bytes are normally already in the pipe buffer when this is
    /// called and the read returns near-instantly.
    pub fn read_line(&self, timeout: Duration) -> Result<String, String> {
        let (tx, rx) = channel();
        let raw = self.raw;
        std::thread::spawn(move || {
            // SAFETY: the handle is owned by the guard; reads are non-mutating.
            let h = HANDLE(raw as *mut _);
            let mut buf = vec![0u8; SUB_BUF_SIZE as usize];
            let mut acc: Vec<u8> = Vec::new();
            loop {
                let mut read = 0u32;
                match unsafe { ReadFile(h, Some(&mut buf), Some(&mut read), None) } {
                    Ok(()) => {
                        if read == 0 {
                            let _ = tx.send(Err("subscriber pipe EOF".to_string()));
                            return;
                        }
                        acc.extend_from_slice(&buf[..read as usize]);
                        if acc.contains(&b'\n') {
                            match String::from_utf8(acc) {
                                Ok(s) => {
                                    let _ = tx.send(Ok(s));
                                }
                                Err(e) => {
                                    let _ = tx.send(Err(format!("non-utf8 line: {e}")));
                                }
                            }
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(format!("ReadFile: {e}")));
                        return;
                    }
                }
            }
        });
        rx.recv_timeout(timeout).map_err(|e| format!("read timed out: {e}"))?
    }
}

impl Drop for SubscriberPipe {
    fn drop(&mut self) {
        // SAFETY: the handle is owned exclusively by this guard.
        unsafe {
            let _ = CloseHandle(HANDLE(self.raw as *mut _));
        }
    }
}

/// A unique subscriber pipe path for this test run.
///
/// Distinct from the daemon command pipe ([`unique_pipe_name`]) so a test can
/// run a daemon and a subscriber side-by-side without colliding.
pub fn unique_subscriber_pipe_name() -> String {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(r"\\.\\pipe\\flow-test-events-{id}")
}

/// Convert a Rust string to a null-terminated UTF-16 vector.
///
/// Mirrors the helper in `test_desktop.rs`; duplicated here so `common` does
/// not depend on `test_desktop`.
fn wide(s: &str) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
