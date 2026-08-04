//! Integration tests for `flow ping` / `Ping` (ticket #14): a side-effect-free
//! daemon reachability probe used by subscriber reconnect loops (ADR-0005).
//!
//! These cover both acceptance criteria from the ticket: `Ping` returns `Ok`
//! while the daemon runs, and `flow ping` fails (non-zero exit) when no daemon
//! is running. Neither test depends on window tiling, so they are reliable
//! (no `#[ignore]`).

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message. See the same pattern in `dispatch_workspace.rs`.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use flow_wm::ipc::message::{SocketMessage, SocketResponse};

use super::common::{SubscriberPipe, flow, unique_pipe_name, unique_subscriber_pipe_name};
use super::test_desktop::{DaemonGuard, TestDesktop, send_ipc_retry, start_test_daemon};

/// Delay after spawning the daemon to let it enter its accept loop.
const DAEMON_SETTLE: Duration = Duration::from_millis(500);

/// Ceiling for waiting on a subscriber read / connect.
const SUB_TIMEOUT: Duration = Duration::from_secs(3);

/// Positive: `Ping` returns `Ok` while the daemon is running, and a subscriber
/// registered after the ping still receives its `state_snapshot` — proving the
/// probe is side-effect-free and does not disturb event delivery.
#[test]
fn ping_returns_ok_while_daemon_runs() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(DAEMON_SETTLE);

    let resp = send_ipc_retry(&pipe, &SocketMessage::Ping).expect("IPC send: ping");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "Ping must ack Ok while the daemon runs",
    );

    // A subscriber registered after the ping must still receive its snapshot —
    // the ping touched no state and broke nothing about event delivery.
    let name = unique_subscriber_pipe_name();
    let (sub, connected) = SubscriberPipe::create(&name).expect("create subscriber pipe");
    let resp = send_ipc_retry(
        &pipe,
        &SocketMessage::Subscribe {
            pipe_name: name.clone(),
        },
    )
    .expect("IPC send: subscribe");
    assert_eq!(resp, SocketResponse::Ok, "subscribe must still ack Ok");
    connected
        .recv_timeout(SUB_TIMEOUT)
        .expect("daemon connected")
        .expect("connect succeeded");
    let line = sub.read_line(SUB_TIMEOUT).expect("read snapshot");
    let parsed: serde_json::Value = serde_json::from_str(line.trim()).expect("valid JSON");
    assert_eq!(parsed["type"], "state_snapshot");
}

/// Negative: `flow ping` fails (non-zero exit) when no daemon is running on
/// the configured pipe — the signal a subscriber's reconnect loop polls.
#[test]
fn flow_ping_fails_when_no_daemon() {
    // A unique pipe name with no daemon listening on it.
    let pipe = unique_pipe_name();
    flow(&pipe).arg("ping").assert().failure();
}
