//! Named pipe transport for IPC between `stm` CLI and `stmd` daemon.
//!
//! Uses the Windows named pipe `\\.\pipe\stm` with synchronous (blocking) I/O.
//! Messages are newline-delimited JSON (see [`super::message`]).

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent, WaitForSingleObject};
use windows::core::HRESULT;

use super::message::{self, SocketMessage, SocketResponse};

/// Read buffer size for pipe I/O (8 KiB).
const BUF_SIZE: u32 = 8192;

/// Maximum wall-clock time a single client-side IPC round trip may take.
///
/// The CLI's transport is a request/response pair: write a message, then read
/// the daemon's reply. With a *synchronous* (blocking) pipe read there is no
/// escape hatch if the daemon accepts the connection but never replies — the
/// caller blocks forever. This is exactly what produced the integration-test
/// hang: a test panicking mid-cleanup destroys its windows, the daemon's main
/// thread stalls recomputing layout (including `SetWindowPos`, which is
/// unreliable on isolated test desktops), and so it never reads the `Stop`
/// command the dropping `DaemonGuard` sends.
///
/// The client pipe is therefore opened for **overlapped** I/O and every
/// read/write is bounded by this deadline (see [`read_line_overlapped`] and
/// [`write_all_overlapped`]). Thirty seconds is deliberately generous — a
/// healthy daemon answers in milliseconds, so this ceiling only ever trips when
/// the daemon is genuinely stuck, at which point failing fast beats hanging.
const IPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Win32 `ERROR_IO_PENDING` (997 / `0x3E5`) expressed as the `HRESULT`
/// windows-rs returns from an overlapped `ReadFile`/`WriteFile` that has not
/// yet completed. It is the normal "operation in flight" result, not an error.
const ERROR_IO_PENDING_HRESULT: HRESULT = HRESULT(0x8007_03E5_u32 as i32);

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

            // ERROR_PIPE_CONNECTED (Win32 535 / 0x217): a client connected
            // before ConnectNamedPipe was called — a success case.
            const ERROR_PIPE_CONNECTED_HRESULT: windows::core::HRESULT =
                windows::core::HRESULT(0x8007_0217_u32 as i32);

            // Retry loop. ConnectNamedPipe can fail transiently with
            // ERROR_NO_DATA ("The pipe is being closed") when the CLI's
            // `is_daemon_running` health-check connects + disconnects in the
            // window between pipe creation and this first ConnectNamedPipe,
            // leaving the instance in a "closing" state. The old code bailed
            // out here without signaling, so the connect event was never set
            // and IPC was permanently dead (debug.log showed exactly this).
            // Reset the instance with DisconnectNamedPipe and retry until a
            // real client connects. The loop also exits when the process
            // terminates (the handle close makes ConnectNamedPipe return an
            // error and the thread is killed on exit), so it cannot spin
            // forever in practice.
            loop {
                let result = unsafe { ConnectNamedPipe(pipe, None) };
                match result {
                    Ok(()) => break,
                    Err(e) if e.code() == ERROR_PIPE_CONNECTED_HRESULT => break,
                    Err(e) => {
                        log::warn!(
                            "PipeServer accept thread: ConnectNamedPipe failed ({e:#?}); \
                             resetting pipe instance and retrying"
                        );
                        // SAFETY: best-effort reset of the server-side instance
                        // so the next ConnectNamedPipe can wait for a fresh
                        // client. Errors (e.g. on a closing handle) are ignored.
                        let _ = unsafe { DisconnectNamedPipe(pipe) };
                        std::thread::sleep(std::time::Duration::from_millis(20));
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

// ── Overlapped (deadline-bounded) client I/O ────────────────────────
//
// The synchronous `read_line`/`write_all` above block forever: a `ReadFile`
// on a byte-mode pipe returns only when data arrives or the peer disconnects,
// with no timeout. That is acceptable for the *server* (whose clients are
// one-shot `stm` invocations that always send immediately), but dangerous for
// the *client*: if the daemon accepts the connection yet never replies, the
// CLI — or a test's cleanup path — hangs indefinitely.
//
// The functions below service the client side of `send_message_to`. Because
// the client handle is opened with `FILE_FLAG_OVERLAPPED` (see
// [`connect_to_named_pipe`]), each I/O can be initiated asynchronously and
// then awaited with a deadline. On timeout the outstanding operation is
// cancelled via `CancelIo`, so an unresponsive daemon yields a `TimedOut`
// error instead of a permanent hang.

/// Convert a deadline (`Instant`) into the millisecond argument expected by
/// [`WaitForSingleObject`], clamped at zero. Returns `None` if the deadline
/// has already passed.
fn ms_until(deadline: Instant) -> Option<u32> {
    let dur = deadline.checked_duration_since(Instant::now())?;
    // `WaitForSingleObject` takes a `u32`; cap at u32::MAX (~49 days).
    let ms = dur.as_millis().min(u128::from(u32::MAX)) as u32;
    Some(ms.max(1)) // never pass 0 (would mean "return immediately without waiting")
}

/// Block until the overlapped operation associated with `overlapped` completes
/// or `deadline` expires.
///
/// # Returns
///
/// On completion this calls [`GetOverlappedResult`] to retrieve the number of
/// bytes transferred and returns it. On timeout it cancels the operation with
/// [`CancelIo`] and returns a `TimedOut` error. Any other wait/I/O failure is
/// mapped to a `BrokenPipe` error.
///
/// # Safety
///
/// `overlapped` must reference an operation that was just started on `handle`
/// with the given event, and `event` must be the event recorded in
/// `overlapped.hEvent`.
unsafe fn await_overlapped(
    handle: HANDLE,
    event: HANDLE,
    overlapped: &OVERLAPPED,
    deadline: Instant,
) -> io::Result<u32> {
    let ms = ms_until(deadline).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "IPC operation timed out (deadline passed)",
        )
    })?;

    match unsafe { WaitForSingleObject(event, ms) } {
        // WAIT_OBJECT_0 (0): the overlapped operation's event was signalled —
        // it completed. Retrieve the authoritative byte count.
        WAIT_OBJECT_0 => {
            let mut transferred = 0u32;
            // SAFETY: `overlapped` was just used to start an I/O on `handle`
            // and its event has signalled completion; we only read the result.
            unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, false) }.map_err(
                |e| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        format!("GetOverlappedResult: {e}"),
                    )
                },
            )?;
            Ok(transferred)
        }
        // WAIT_TIMEOUT (258): the deadline expired before any I/O completed.
        // Cancel the in-flight operation so the kernel releases the buffers,
        // then surface a timeout.
        WAIT_TIMEOUT => {
            // SAFETY: cancelling I/O on our own client handle.
            let _ = unsafe { CancelIo(handle) };
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "IPC operation timed out waiting for the daemon",
            ))
        }
        other => {
            // SAFETY: as above.
            let _ = unsafe { CancelIo(handle) };
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("WaitForSingleObject returned {other:?}"),
            ))
        }
    }
}

/// Write all of `data` to an overlapped pipe handle, bounded by `deadline`.
///
/// Loops until every byte is written, each chunk awaited via
/// [`await_overlapped`]. Returns `TimedOut` if the deadline passes before the
/// write completes.
fn write_all_overlapped(handle: HANDLE, data: &[u8], deadline: Instant) -> io::Result<()> {
    let event = EventHandle::new()?;
    let mut total = 0usize;

    while total < data.len() {
        // Manual-reset event: reset before each operation so the signal from a
        // previous chunk doesn't make the next wait return spuriously.
        let _ = unsafe { ResetEvent(event.raw()) };
        let mut overlapped = OVERLAPPED {
            hEvent: event.raw(),
            ..Default::default()
        };
        let mut written = 0u32;

        let result = unsafe {
            WriteFile(
                handle,
                Some(&data[total..]),
                Some(&mut written),
                Some(&mut overlapped),
            )
        };

        match result {
            // Completed synchronously — `written` is already authoritative.
            Ok(()) => {}
            Err(e) if e.code() == ERROR_IO_PENDING_HRESULT => {
                written = unsafe { await_overlapped(handle, event.raw(), &overlapped, deadline)? };
            }
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("WriteFile: {e}"),
                ));
            }
        }

        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "WriteFile wrote zero bytes",
            ));
        }
        total += written as usize;
    }

    Ok(())
}

/// Read a newline-terminated line from an overlapped pipe handle, bounded by
/// `deadline`.
///
/// Each chunk is read asynchronously and awaited via [`await_overlapped`]; the
/// function returns as soon as a `\n` is seen. If `deadline` passes with no
/// data, the outstanding read is cancelled and a `TimedOut` error is returned.
fn read_line_overlapped(handle: HANDLE, deadline: Instant) -> io::Result<String> {
    let event = EventHandle::new()?;
    let mut buf = vec![0u8; BUF_SIZE as usize];
    let mut raw: Vec<u8> = Vec::new();

    loop {
        let _ = unsafe { ResetEvent(event.raw()) };
        let mut overlapped = OVERLAPPED {
            hEvent: event.raw(),
            ..Default::default()
        };
        let mut bytes_read = 0u32;

        let result = unsafe {
            ReadFile(
                handle,
                Some(buf.as_mut_slice()),
                Some(&mut bytes_read),
                Some(&mut overlapped),
            )
        };

        match result {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_IO_PENDING_HRESULT => {
                bytes_read =
                    unsafe { await_overlapped(handle, event.raw(), &overlapped, deadline)? };
            }
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("ReadFile: {e}"),
                ));
            }
        }

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
/// # Timeout behaviour
///
/// The whole request/response is bounded by [`IPC_TIMEOUT`]. The client pipe
/// is opened for overlapped I/O and each read/write is awaited with that
/// deadline; if the daemon never replies (or replies too slowly) the call
/// returns [`io::ErrorKind::TimedOut`] instead of blocking forever. This is
/// what stops an unresponsive daemon from hanging the CLI — or, during panic
/// unwinding, a test's `DaemonGuard` drop.
///
/// # Errors
///
/// - `ConnectionRefused` if the daemon is not running (pipe does not exist).
/// - `TimedOut` if the daemon does not complete the round trip within
///   [`IPC_TIMEOUT`].
/// - `InvalidData` if serialisation fails or the daemon sends a malformed response.
/// - Other I/O errors for transport failures.
pub fn send_message_to(pipe_name: &str, msg: &SocketMessage) -> io::Result<SocketResponse> {
    let handle = connect_to_named_pipe(pipe_name)?;
    let deadline = Instant::now() + IPC_TIMEOUT;

    // Write the message (overlapped, deadline-bounded).
    let wire = message::encode_message(msg)?;
    write_all_overlapped(handle.raw(), wire.as_bytes(), deadline)?;

    // Read the response (overlapped, same overall deadline).
    let line = read_line_overlapped(handle.raw(), deadline)?;

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
///
/// The handle is opened for **overlapped I/O** (`FILE_FLAG_OVERLAPPED`) so that
/// every read and write on it can be bounded by a deadline (see
/// [`send_message_to`]). This is what prevents an unresponsive daemon from
/// hanging the CLI/test forever: overlapped operations are awaited with
/// [`WaitForSingleObject`] and cancelled with [`CancelIo`] on timeout.
fn connect_to_named_pipe(pipe_name: &str) -> io::Result<PipeHandle> {
    let name = wide(pipe_name);
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(name.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // ── ms_until: deadline → milliseconds conversion for WaitForSingleObject ──
    //
    // Pure arithmetic extracted from the overlapped-I/O client transport.
    // No Win32, no pipe — just Instant arithmetic. Tests the clamping,
    // flooring, and None-on-expired behaviour that protects against
    // passing 0 (which would mean "return immediately") to WaitForSingleObject.

    #[test]
    fn ms_until_future_deadline_returns_positive_milliseconds() {
        // Positive: a deadline 500 ms in the future yields Some(≥500).
        let deadline = Instant::now() + Duration::from_millis(500);
        let result = ms_until(deadline);
        assert!(result.is_some());
        // Allow ±1 ms tolerance for clock drift during the call.
        let ms = result.unwrap();
        assert!(ms >= 499 && ms <= 501, "expected ~500 ms, got {ms}");
    }

    #[test]
    fn ms_until_past_deadline_returns_none() {
        // Negative: a deadline already passed yields None (caller turns this
        // into a TimedOut error).
        let deadline = Instant::now() - Duration::from_millis(100);
        assert_eq!(ms_until(deadline), None);
    }

    #[test]
    fn ms_until_nearly_expired_floor_at_one() {
        // Positive edge: the function floors at 1 ms so WaitForSingleObject
        // doesn't get 0 (which means "don't wait at all"). When the deadline
        // is a few microseconds away, the result is Some(1), not Some(0).
        let deadline = Instant::now() + Duration::from_micros(500);
        let result = ms_until(deadline);
        assert_eq!(result, Some(1));
    }

    #[test]
    fn ms_until_large_deadline_capped_at_u32_max() {
        // Positive: a 60-day deadline (far beyond u32::MAX ms ≈ 49 days)
        // is capped, not truncated.
        let deadline = Instant::now() + Duration::from_secs(60 * 24 * 3600);
        let result = ms_until(deadline);
        assert_eq!(result, Some(u32::MAX));
    }

    #[test]
    fn ms_until_zero_remaining_floors_to_one() {
        // Positive edge: when the deadline is effectively now (within a few
        // microseconds), checked_duration_since returns Some(~0), which is
        // floored to 1 ms by the `max(1)` guard so WaitForSingleObject
        // doesn't receive 0 (which would mean "don't wait at all").
        let deadline = Instant::now();
        let result = ms_until(deadline);
        // By the time ms_until runs, some sub-microsecond time has passed,
        // so we get Some(0ms) floored to Some(1). An exact-zero instant
        // would give None (checked_duration_since returns None when equal),
        // but that timing is not reproducible in a test.
        assert!(result.is_some(), "deadline at now should yield Some(1), got {result:?}");
        assert_eq!(result.unwrap(), 1);
    }

    // ── wide: Rust string → UTF-16 null-terminated ────────────────────────

    #[test]
    fn wide_empty_string_produces_single_null() {
        // Positive: empty input → only the null terminator.
        let result = wide("");
        assert_eq!(result, vec![0u16]);
    }

    #[test]
    fn wide_ascii_string_includes_null_terminator() {
        // Positive: ASCII stays single-code-unit + null.
        let result = wide("abc");
        assert_eq!(result, vec![b'a' as u16, b'b' as u16, b'c' as u16, 0]);
    }

    #[test]
    fn wide_unicode_produces_correct_code_units() {
        // Positive: a multi-byte UTF-8 character (é = U+00E9) is one UTF-16
        // code unit; a surrogate-pair character (𐍈 = U+10348) is two.
        let result = wide("é");
        assert_eq!(result, vec![0x00E9, 0]); // single code unit + null

        let result_surrogate = wide("𐍈");
        // U+10348 → UTF-16 surrogate pair: 0xD800, 0xDF48
        assert_eq!(result_surrogate, vec![0xD800, 0xDF48, 0]);
    }

    #[test]
    fn wide_never_produces_empty_vec() {
        // Negative edge: even for empty input, the null terminator is always
        // present. This is a Win32 invariant — null-terminated strings must
        // have at least one element.
        let result = wide("");
        assert!(!result.is_empty());
        assert_eq!(*result.last().unwrap(), 0);
    }
}
