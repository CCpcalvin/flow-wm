//! Named pipe transport for IPC between `stm` CLI and `stmd` daemon.
//!
//! Uses the Windows named pipe `\\.\pipe\stm` with synchronous (blocking) I/O.
//! Messages are newline-delimited JSON (see [`super::message`]).

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use super::message::{self, PIPE_NAME, SocketMessage, SocketResponse};

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

/// Named pipe server for the `stmd` daemon.
///
/// Listens on `\\.\pipe\stm`, accepts one client at a time (sequential),
/// reads newline-delimited JSON messages, and writes responses.
pub struct PipeServer {
    handle: PipeHandle,
}

impl PipeServer {
    /// Create a new named pipe server instance.
    ///
    /// This creates the pipe in a listening state. Call [`Self::wait_for_client`]
    /// to accept a connection.
    ///
    /// # Errors
    ///
    /// Returns an error if the pipe cannot be created (e.g., another daemon is
    /// already running).
    pub fn create() -> io::Result<Self> {
        let name = wide(PIPE_NAME);

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

        Ok(Self {
            handle: PipeHandle(handle),
        })
    }

    /// Block until a client connects to the pipe.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection fails unexpectedly.
    pub fn wait_for_client(&self) -> io::Result<()> {
        let result = unsafe { ConnectNamedPipe(self.handle.raw(), None) };
        if let Err(e) = result {
            // ERROR_PIPE_CONNECTED (536) means a client already connected
            // before we called ConnectNamedPipe — this is a success case.
            let err = unsafe { GetLastError() };
            if err.0 != 536 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("ConnectNamedPipe failed: {e}"),
                ));
            }
        }
        Ok(())
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
    let handle = connect_to_pipe()?;

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
/// The handle is RAII-wrapped and closed immediately when dropped — no kernel
/// handle leak.
///
/// Returns `true` if the pipe exists (daemon is running), `false` otherwise.
#[must_use]
pub fn is_daemon_running() -> bool {
    connect_to_pipe().is_ok()
}

/// Open the daemon's named pipe, returning an RAII-wrapped handle.
fn connect_to_pipe() -> io::Result<PipeHandle> {
    let name = wide(PIPE_NAME);
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(name.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            HANDLE::default(),
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
