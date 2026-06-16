//! Named pipe transport for IPC between `stm` CLI and `stmd` daemon.
//!
//! Uses the Windows named pipe `\\.\pipe\stm` with synchronous (blocking) I/O.
//! Messages are newline-delimited JSON (see [`super::message`]).

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};

use super::message::{self, SocketMessage, SocketResponse};

/// Read buffer size for pipe I/O (8 KiB).
const BUF_SIZE: u32 = 8192;

/// Wrapper around a Windows `HANDLE` that is closed on drop.
///
/// All pipe handles (both server and client) must be wrapped in this type
/// to prevent kernel handle leaks on early returns.
#[derive(Debug)]
struct PipeHandle(HANDLE);

impl PipeHandle {
    /// Returns the raw handle value.
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// Safety: HANDLE is a raw pointer-like value. We manage its lifetime via Drop.
unsafe impl Send for PipeHandle {}

/// RAII wrapper around a Win32 Event handle used for I/O signaling.
///
/// Created via [`CreateEventW`] (manual-reset mode, initially unset). The
/// handle is closed automatically when this wrapper is dropped, preventing
/// kernel handle leaks.
///
/// # Design
///
/// In the daemon's event-driven architecture, an [`EventHandle`] bridges
/// between a background thread (blocked in `ConnectNamedPipe`) and the main
/// thread (blocked in `WaitForMultipleObjects`). When the accept thread
/// detects a client connection it calls [`SetEvent`], waking the main thread
/// so it can proceed with IPC processing.
#[derive(Debug)]
struct EventHandle(HANDLE);

impl EventHandle {
    /// Creates a new manual-reset event that is initially unset.
    ///
    /// A manual-reset event stays signaled until explicitly reset, which is
    /// the correct semantic here: once a client connects, the event remains
    /// signaled until the main thread processes the connection and calls
    /// `start_accept` for the next one.
    ///
    /// # Errors
    ///
    /// Returns an error if [`CreateEventW`] fails or returns an invalid handle.
    fn new() -> io::Result<Self> {
        let handle = unsafe { CreateEventW(None, true, false, windows::core::PCWSTR::null()) }
            .map_err(|e| io::Error::other(format!("CreateEventW failed: {e}")))?;

        if handle.is_invalid() {
            return Err(io::Error::other("CreateEventW returned invalid handle"));
        }

        Ok(Self(handle))
    }

    /// Returns the raw [`HANDLE`] for use with `WaitForMultipleObjects`.
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// SAFETY: HANDLE is a process-wide kernel object identifier. Sending it
// across threads is safe — the kernel synchronizes access to the event object.
unsafe impl Send for EventHandle {}

/// Named pipe server for the `stmd` daemon.
///
/// Listens on `\\.\pipe\stm`, accepts one client at a time (sequential),
/// reads newline-delimited JSON messages, and writes responses.
///
/// # Event-Driven Architecture
///
/// Instead of blocking the main thread in `ConnectNamedPipe`, the server
/// uses a background accept thread. The flow is:
///
/// 1. Main thread calls [`start_accept()`] to spawn a one-shot thread that
///    blocks in `ConnectNamedPipe`.
/// 2. When a client connects, the accept thread calls `SetEvent` on the
///    `connected_event`, waking the main thread's `WaitForMultipleObjects`.
/// 3. The main thread processes IPC messages via [`read_message()`] /
///    [`write_response()`], then calls [`disconnect()`] and [`start_accept()`]
///    to accept the next client.
///
/// This decouples pipe-accept blocking from the main loop, allowing hook
/// events (window creation/removal) to be processed immediately even when no
/// CLI client is connected.
pub struct PipeServer {
    handle: PipeHandle,
    /// Manual-reset event signaled when a client connects to the pipe.
    /// The main thread waits on this via `WaitForMultipleObjects`.
    connected_event: EventHandle,
    /// Background thread blocked in `ConnectNamedPipe`.
    /// `None` when no accept is pending.
    accept_thread: Option<std::thread::JoinHandle<()>>,
}

impl PipeServer {
    /// Create a new named pipe server instance.
    ///
    /// This creates the pipe and the `connected_event` Win32 event object.
    /// The pipe is ready to accept connections — call [`Self::start_accept`]
    /// to begin listening on a background thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the pipe cannot be created (e.g., another daemon is
    /// already running) or if the event object cannot be created.
    pub fn create() -> io::Result<Self> {
        let name = wide(&message::pipe_name());

        // TODO(security): Create a SECURITY_ATTRIBUTES that restricts pipe
        // access to the current user session. Currently any local process can
        // connect and send commands. Acceptable for Phase 1 MVP (single-user
        // desktop tool), but must be hardened before wider distribution.
        let handle = unsafe {
            CreateNamedPipeW(
                windows::core::PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                BUF_SIZE,
                BUF_SIZE,
                0,
                None,
            )
        };

        if handle.is_invalid() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "pipe already in use (is another daemon running?)",
            ));
        }

        let connected_event = EventHandle::new()?;

        Ok(Self {
            handle: PipeHandle(handle),
            connected_event,
            accept_thread: None,
        })
    }

    /// Start accepting the next client connection on a background thread.
    ///
    /// Spawns a short-lived thread that calls `ConnectNamedPipe` (blocking).
    /// When a client connects, the thread signals `connected_event` via
    /// `SetEvent` and exits. The main thread should use
    /// [`connected_event_handle`] to obtain the event handle for
    /// `WaitForMultipleObjects`.
    ///
    /// Call this method:
    /// - Once at startup (before entering the main loop)
    /// - After each client disconnects (to accept the next client)
    pub fn start_accept(&mut self) {
        // Join the previous accept thread if it exists.
        // The accept thread is one-shot: it exits after ConnectNamedPipe
        // returns and SetEvent is called. By the time we get here, the
        // thread has already finished, so join() returns immediately.
        if let Some(handle) = self.accept_thread.take()
            && let Err(e) = handle.join()
        {
            log::warn!("PipeServer: previous accept thread panicked: {e:?}");
        }

        // Reset the connected event before spawning a new accept thread.
        let _ = unsafe { ResetEvent(self.connected_event.raw()) };

        let handle = self.handle.raw().0 as isize;
        let event = self.connected_event.raw().0 as isize;

        self.accept_thread = Some(std::thread::spawn(move || {
            // Block until a client connects. This runs on a background thread
            // so the main thread is free to process hook events.
            //
            // SAFETY: HANDLE values are process-wide kernel object identifiers.
            // We pass them as isize to satisfy Send requirements (HANDLE itself
            // is !Send in windows-rs), then reconstruct inside the closure.
            let pipe = HANDLE(handle as *mut core::ffi::c_void);
            let event = HANDLE(event as *mut core::ffi::c_void);
            let result = unsafe { ConnectNamedPipe(pipe, None) };
            match result {
                Ok(()) => {}
                Err(e) => {
                    // ERROR_PIPE_CONNECTED (Win32 error 535 / 0x217, HRESULT
                    // 0x80070217) means a client already connected before we
                    // called ConnectNamedPipe — this is a success case.
                    //
                    // We compare the HRESULT from the error object rather than
                    // GetLastError() because windows-rs may have called other
                    // Win32 functions internally that changed the last error.
                    let hresult = e.code();
                    let is_pipe_connected =
                        hresult == windows::core::HRESULT(0x8007_0217_u32 as i32);
                    if !is_pipe_connected {
                        log::error!("PipeServer accept thread: ConnectNamedPipe failed: {e}");
                        return;
                    }
                }
            }
            // Signal the main thread that a client is connected.
            let _ = unsafe { SetEvent(event) };
        }));
    }

    /// Returns the event handle that is signaled when a client connects.
    ///
    /// Used by the main thread with `WaitForMultipleObjects` to wait for
    /// either hook events or pipe connections simultaneously.
    pub fn connected_event_handle(&self) -> HANDLE {
        self.connected_event.raw()
    }

    /// Read a single newline-terminated JSON message from the client.
    ///
    /// Blocks until a full line (ending in `\n`) is received.
    ///
    /// # Errors
    ///
    /// Returns an error on I/O failure or if the connection is closed
    /// without sending a message.
    pub fn read_message(&self) -> io::Result<SocketMessage> {
        let line = read_line(self.handle.raw())?;
        message::decode_message(&line).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to parse message: {line:?}"),
            )
        })
    }

    /// Write a [`SocketResponse`] to the client as newline-delimited JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if serialisation or the write fails.
    pub fn write_response(&self, response: &SocketResponse) -> io::Result<()> {
        let wire = message::encode_message(response)?;
        write_all(self.handle.raw(), wire.as_bytes())
    }

    /// Disconnect the current client so a new one can connect.
    ///
    /// # Errors
    ///
    /// Returns an error if the disconnect fails.
    pub fn disconnect(&self) -> io::Result<()> {
        unsafe { DisconnectNamedPipe(self.handle.raw()) }
            .map_err(|e| io::Error::other(format!("DisconnectNamedPipe: {e}")))
    }
}

/// Read a single newline-terminated line from a pipe handle.
///
/// Blocks until a `\n` is received or the peer disconnects.
/// Accumulates raw bytes and performs UTF-8 conversion only once at the end,
/// avoiding corruption when a multi-byte character spans buffer boundaries.
fn read_line(handle: HANDLE) -> io::Result<String> {
    let mut buf = vec![0u8; BUF_SIZE as usize];
    let mut raw: Vec<u8> = Vec::new();

    loop {
        let mut bytes_read = 0u32;
        unsafe {
            ReadFile(
                handle,
                Some(buf.as_mut_slice()),
                Some(&mut bytes_read),
                None,
            )
        }
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, format!("ReadFile: {e}")))?;

        if bytes_read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "peer disconnected",
            ));
        }

        raw.extend_from_slice(&buf[..bytes_read as usize]);

        if raw.contains(&b'\n') {
            break;
        }
    }

    String::from_utf8(raw).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("non-UTF-8 data from pipe: {e}"),
        )
    })
}

/// Write all bytes to a pipe handle.
fn write_all(handle: HANDLE, data: &[u8]) -> io::Result<()> {
    let mut total_written = 0;
    while total_written < data.len() {
        let mut bytes_written = 0u32;
        unsafe {
            WriteFile(
                handle,
                Some(&data[total_written..]),
                Some(&mut bytes_written),
                None,
            )
        }
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, format!("WriteFile: {e}")))?;

        if bytes_written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "WriteFile wrote zero bytes",
            ));
        }
        total_written += bytes_written as usize;
    }
    Ok(())
}

/// Connect to the daemon's named pipe, send a message, and read the response.
///
/// This is the primary function used by the `stm` CLI to communicate with
/// the daemon. The pipe handle is RAII-wrapped — it is closed even if an
/// error occurs mid-transaction.
///
/// # Errors
///
/// - `ConnectionRefused` if the daemon is not running (pipe does not exist).
/// - `InvalidData` if serialisation fails or the daemon sends a malformed response.
/// - Other I/O errors for transport failures.
pub fn send_message(msg: &SocketMessage) -> io::Result<SocketResponse> {
    send_message_to(&message::pipe_name(), msg)
}

/// Connect to a specific named pipe, send a message, and read the response.
///
/// Like [`send_message`] but takes the pipe path directly instead of reading
/// from the `STM_PIPE_NAME` environment variable. Thread-safe for concurrent
/// use with different pipe names (no global env-var mutation).
///
/// # Errors
///
/// Same as [`send_message`].
pub fn send_message_to(pipe_name: &str, msg: &SocketMessage) -> io::Result<SocketResponse> {
    let handle = connect_to_named_pipe(pipe_name)?;

    // Write the message
    let wire = message::encode_message(msg)?;
    write_all(handle.raw(), wire.as_bytes())?;

    // Read the response
    let line = read_line(handle.raw())?;

    message::decode_message(&line).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse response: {line:?}"),
        )
    })
}

/// Check if the daemon is running by attempting to open the named pipe.
///
/// Retries up to three times with a 50 ms sleep between attempts. This handles
/// the brief window after a previous `connect_to_pipe` + drop cycle where the
/// daemon may be between `DisconnectNamedPipe` and the next `ConnectNamedPipe`.
///
/// The handle is RAII-wrapped and closed immediately when dropped — no kernel
/// handle leak.
///
/// Returns `true` if the pipe exists (daemon is running), `false` otherwise.
#[must_use]
pub fn is_daemon_running() -> bool {
    for _ in 0..3 {
        if connect_to_pipe().is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// Open the daemon's named pipe, returning an RAII-wrapped handle.
///
/// Uses the pipe name from the `STM_PIPE_NAME` environment variable
/// (or the default `\\.\pipe\stm`).
fn connect_to_pipe() -> io::Result<PipeHandle> {
    connect_to_named_pipe(&message::pipe_name())
}

/// Open a named pipe by path, returning an RAII-wrapped handle.
fn connect_to_named_pipe(pipe_name: &str) -> io::Result<PipeHandle> {
    let name = wide(pipe_name);
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(name.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|_| io::Error::new(io::ErrorKind::ConnectionRefused, "daemon not running"))?;

    Ok(PipeHandle(handle))
}

/// Convert a Rust string to a wide (UTF-16) string with null terminator.
fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
