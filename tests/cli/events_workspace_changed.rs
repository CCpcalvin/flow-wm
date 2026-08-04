//! Integration test for the `WorkspaceChanged` event (ticket #15).
//!
//! Spawns a real `flowd` on an isolated [`TestDesktop`], subscribes a
//! [`SubscriberPipe`], triggers a `switch-workspace`, and asserts the next
//! event line is a `workspace_changed` with the right monitor / workspace ids.
//!
//! This does not depend on window tiling or `SetWindowPos` — a workspace
//! switch works on an empty workspace — so it is not subject to the
//! isolated-desktop limitations that `#[ignore]` several other tests.

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message. See the same pattern in `dispatch_workspace.rs`.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use flow_wm::ipc::message::{SocketMessage, SocketResponse};

use super::common::{SubscriberPipe, unique_pipe_name, unique_subscriber_pipe_name};
use super::test_desktop::{DaemonGuard, TestDesktop, send_ipc_retry, start_test_daemon};

/// Delay after spawning the daemon to let it enter its accept loop.
const DAEMON_SETTLE: Duration = Duration::from_millis(500);

/// Ceiling for waiting on a subscriber read / connect.
const SUB_TIMEOUT: Duration = Duration::from_secs(3);

/// Read one event line off `sub` and parse it as JSON.
fn read_event(sub: &SubscriberPipe) -> serde_json::Value {
    let line = sub.read_line(SUB_TIMEOUT).expect("read event line");
    serde_json::from_str(line.trim()).expect("event line is valid JSON")
}

/// Positive: `switch-workspace` emits a `WorkspaceChanged` event with the
/// correct monitor and workspace ids, as the second line after the bootstrap
/// `state_snapshot`.
#[test]
fn switch_workspace_emits_workspace_changed_event() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(DAEMON_SETTLE);

    // Subscribe — the daemon pushes a `state_snapshot` as the first line.
    let sub_name = unique_subscriber_pipe_name();
    let (sub, connected) = SubscriberPipe::create(&sub_name).expect("create subscriber pipe");
    let resp = send_ipc_retry(
        &pipe,
        &SocketMessage::Subscribe {
            pipe_name: sub_name.clone(),
        },
    )
    .expect("IPC send: subscribe");
    assert_eq!(resp, SocketResponse::Ok);
    connected
        .recv_timeout(SUB_TIMEOUT)
        .expect("daemon connected")
        .expect("connect succeeded");

    // Drain the bootstrap snapshot so the next read is the live event.
    let snapshot = read_event(&sub);
    assert_eq!(snapshot["type"], "state_snapshot");

    // Trigger a workspace switch (default active monitor 0, ws 1 -> ws 2).
    let resp = send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 2 })
        .expect("IPC send: switch-workspace");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "switch to a valid workspace must succeed",
    );

    // The next event line must be the WorkspaceChanged for the new workspace.
    let event = read_event(&sub);
    assert_eq!(event["type"], "workspace_changed", "wire: {event}");
    assert_eq!(
        event["monitor"], 0,
        "switch occurred on the default (only) monitor",
    );
    assert_eq!(
        event["workspace"], 2,
        "the now-active workspace id must be the switch target",
    );

    // Sanity: the daemon is still serving IPC after emitting the event.
    let probe = send_ipc_retry(&pipe, &SocketMessage::QueryState).expect("probe QueryState");
    assert!(
        matches!(probe, SocketResponse::Data { .. }),
        "daemon must keep serving IPC after emitting WorkspaceChanged, got {probe:?}",
    );
}
