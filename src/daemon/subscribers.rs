//! Subscriber-owned-pipe event delivery (ADR-0005, Option B).
//!
//! A [`SubscriberManager`] owns the set of subscriber pipes the daemon writes
//! to. Each subscriber creates its own named pipe, registers it via the
//! `Subscribe` IPC message, and the daemon opens that pipe as a [`HANDLE`] on
//! the main thread and pushes newline-delimited JSON [`Event`](crate::events::Event)s
//! to it.
//!
//! # Threading invariant
//!
//! The manager is a plain field on [`FlowWM`](super::types::FlowWM) — no
//! `Arc<Mutex>`, no background thread. All writes happen on the main thread,
//! alongside the existing hook-event drain, consistent with the daemon's
//! "main thread owns all state" model.
//!
//! # Write-error eviction (load-bearing)
//!
//! Every write to a subscriber pipe is deadline-bounded via
//! [`transport::write_all_overlapped`](crate::ipc::transport::write_all_overlapped).
//! Any error — a broken pipe (subscriber died), a full buffer (subscriber
//! stopped reading), or a write timeout — silently **evicts** that subscriber.
//! A dead or blocked subscriber therefore can never stall the window manager:
//! it is dropped on the first failed write and never written to again.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_GENERIC_WRITE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::core::PCWSTR;

use crate::events::Event;
use crate::ipc::transport;

/// Maximum wall-clock time a single subscriber write may take before the
/// subscriber is evicted.
///
/// A healthy subscriber drains its pipe within microseconds, so this ceiling
/// only ever trips when the subscriber has died, blocked, or stopped reading
/// (full buffer) — exactly the cases where evicting is the correct response.
/// One second is generous enough to absorb a momentary reader lag yet short
/// enough that a single misbehaving subscriber can never stall the window
/// manager for long. Eviction prevents recurrence.
const SUBSCRIBER_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// A single registered subscriber: a named pipe it owns, opened by the daemon
/// for overlapped writing.
struct Subscriber {
    /// The pipe path, kept for eviction logging only.
    name: String,
    /// Win32 file handle to the subscriber's pipe (opened for overlapped write).
    handle: HANDLE,
}

impl Subscriber {
    /// Returns the raw handle for use with the overlapped writer.
    fn raw(&self) -> HANDLE {
        self.handle
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        // SAFETY: the handle is owned exclusively by this Subscriber and is
        // not used after drop.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

// SAFETY: HANDLE is a process-wide kernel object identifier. The daemon owns
// every Subscriber on its main thread; sending the value across threads is
// safe because the kernel synchronises access to the underlying object.
unsafe impl Send for Subscriber {}

/// The set of registered subscriber pipes the daemon writes [`Event`]s to.
///
/// Owned as a single field on [`FlowWM`](super::types::FlowWM); constructed
/// empty and mutated only on the main thread.
pub(super) struct SubscriberManager {
    subscribers: Vec<Subscriber>,
}

impl SubscriberManager {
    /// Create an empty subscriber manager.
    pub(super) fn new() -> Self {
        Self {
            subscribers: Vec::new(),
        }
    }

    /// The number of currently registered subscribers.
    ///
    /// Exposed for diagnostics / tests; not used by the dispatch path.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.subscribers.len()
    }

    /// Open a subscriber-owned named pipe and register it.
    ///
    /// `pipe_name` is the full pipe path (e.g. `\\.\pipe\flow-bar`). The
    /// daemon opens it as a client for overlapped writing. On success the
    /// subscriber will receive every subsequent [`Event`] broadcast.
    ///
    /// # Errors
    ///
    /// Returns an error if the pipe cannot be opened — most commonly because
    /// no subscriber is listening at that path yet (the subscriber must create
    /// its pipe *before* sending `Subscribe`).
    pub(super) fn add(&mut self, pipe_name: &str) -> io::Result<()> {
        let handle = open_subscriber_pipe(pipe_name)?;
        self.subscribers.push(Subscriber {
            name: pipe_name.to_owned(),
            handle,
        });
        Ok(())
    }

    /// Serialize an [`Event`] and broadcast it to every subscriber, evicting
    /// any whose pipe fails the write.
    ///
    /// Serialization failure is impossible for the daemon's own [`Event`]
    /// variants (they serialize plain JSON); if it ever did fail the event is
    /// silently dropped rather than panicking — event publishing must never
    /// take the window manager down.
    pub(super) fn broadcast(&mut self, event: &Event) {
        let Ok(line) = serialize_event(event) else {
            log::error!("events: failed to serialize event {event:?} — dropping");
            return;
        };
        self.broadcast_line(&line);
    }

    /// Write a pre-serialized event line (`...json...\n`) to every subscriber,
    /// evicting any whose pipe fails the write.
    ///
    /// Each subscriber is written independently: one subscriber's failure
    /// never affects the others, and a failed subscriber is dropped (closing
    /// its handle) so it is never written to again.
    pub(super) fn broadcast_line(&mut self, line: &str) {
        if self.subscribers.is_empty() {
            return;
        }
        let bytes = line.as_bytes();
        let deadline = Instant::now() + SUBSCRIBER_WRITE_TIMEOUT;

        // Drain into a local so we can partition into survivors / evicted
        // without borrowing self across the write calls.
        let mut survivors: Vec<Subscriber> = Vec::with_capacity(self.subscribers.len());
        for sub in self.subscribers.drain(..) {
            match transport::write_all_overlapped(sub.raw(), bytes, deadline) {
                Ok(()) => survivors.push(sub),
                Err(e) => {
                    // Evict silently — never block, never panic. The handle is
                    // closed when `sub` drops at the end of this arm.
                    log::debug!(
                        "events: evicted subscriber '{}' (write failed: {e})",
                        sub.name
                    );
                }
            }
        }
        self.subscribers = survivors;
    }
}

impl Default for SubscriberManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Serialize an [`Event`] as a single newline-terminated JSON line.
///
/// # Errors
///
/// Returns an error only if `serde_json` fails to serialize `event`.
fn serialize_event(event: &Event) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(event)?;
    line.push('\n');
    Ok(line)
}

/// Open a subscriber-owned named pipe for overlapped writing.
///
/// The subscriber creates the pipe (as a server) and waits in
/// `ConnectNamedPipe`; the daemon connects here as a client via `CreateFileW`.
/// `GENERIC_WRITE` is requested because the daemon only ever writes outbound
/// to subscribers — it never reads from them. The handle is opened for
/// overlapped I/O so writes can be deadline-bounded (see [`SUBSCRIBER_WRITE_TIMEOUT`]).
///
/// # Errors
///
/// Returns an error if the pipe does not exist or cannot be opened.
fn open_subscriber_pipe(pipe_name: &str) -> io::Result<HANDLE> {
    let wide = wide(pipe_name);
    // SAFETY: `wide` produces a null-terminated UTF-16 string; CreateFileW
    // reads it and returns a handle or an error.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
            None,
        )
    }
    .map_err(|e| {
        io::Error::other(format!("failed to open subscriber pipe '{pipe_name}': {e}"))
    })?;

    Ok(handle)
}

/// Convert a Rust string to a null-terminated UTF-16 vector.
///
/// Mirrors the helper in `src/ipc/transport.rs`; duplicated here to keep the
/// subscriber module self-contained.
fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Positive: a freshly-constructed manager reports zero subscribers.
    #[test]
    fn new_manager_is_empty() {
        let mgr = SubscriberManager::new();
        assert_eq!(mgr.len(), 0);
    }

    /// Positive: `broadcast` to an empty manager is a no-op (no subscribers,
    /// no panic, no allocation of survivors beyond the early return).
    #[test]
    fn broadcast_to_empty_manager_is_noop() {
        let mut mgr = SubscriberManager::new();
        mgr.broadcast(&Event::StateSnapshot { state: json!({}) });
        assert_eq!(mgr.len(), 0);
    }

    /// Positive: the default-constructed manager is empty, matching `new`.
    #[test]
    fn default_matches_new() {
        assert_eq!(SubscriberManager::default().len(), SubscriberManager::new().len());
    }

    /// Positive: `serialize_event` terminates the line with a newline and
    /// carries the flat `type` tag.
    #[test]
    fn serialize_event_terminates_with_newline_and_carries_type() {
        let event = Event::StateSnapshot { state: json!({}) };
        let line = serialize_event(&event).expect("serialize");
        assert!(line.ends_with('\n'));
        assert!(line.contains(r#""type":"state_snapshot""#));
    }

    /// Negative: `open_subscriber_pipe` fails for a pipe nobody is listening on.
    ///
    /// The daemon must surface this as an error rather than blocking, so the
    /// caller can reply to the `Subscribe` message with a precise failure.
    #[test]
    fn open_subscriber_pipe_fails_when_no_listener() {
        // A pipe path that no test creates a server for. CreateFileW with
        // OPEN_EXISTING returns an error when the pipe does not exist.
        let missing = format!(
            r"\\.\pipe\flow-no-such-subscriber-{}",
            std::process::id()
        );
        let result = open_subscriber_pipe(&missing);
        assert!(
            result.is_err(),
            "opening a pipe with no listener must fail, got {result:?}",
        );
    }
}
