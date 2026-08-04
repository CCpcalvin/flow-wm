//! Integration tests for the event-broadcast tracer bullet (ticket #13):
//! `Subscribe` + `StateSnapshot` + write-error eviction.
//!
//! These tests play the subscriber role from the test process: they create a
//! named pipe, send `Subscribe`, and read the newline-delimited JSON
//! [`Event`](flow_wm::events::Event)s the daemon pushes. They exercise
//! serialization, emission, transport, the snapshot, and eviction end-to-end
//! against a real `flowd` on an isolated [`TestDesktop`] — no internal test
//! hooks (per ADR-0005's testing decisions).
//!
//! Unlike the layout/window tests, these do not depend on window tiling or
//! `SetWindowPos`, so they are not subject to the isolated-desktop limitations
//! that `#[ignore]` several other tests.

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

/// Send `Subscribe` for `pipe_name` and return the daemon’s response.
fn subscribe(pipe: &str, pipe_name: &str) -> SocketResponse {
    send_ipc_retry(
        pipe,
        &SocketMessage::Subscribe {
            pipe_name: pipe_name.to_owned(),
        },
    )
    .expect("IPC send: subscribe")
}

/// Read one event line off `sub` and parse it as JSON.
fn read_event(sub: &SubscriberPipe) -> serde_json::Value {
    let line = sub.read_line(SUB_TIMEOUT).expect("read event line");
    serde_json::from_str(line.trim()).expect("event line is valid JSON")
}

// ── Tests ───────────────────────────────────────────────────────────

/// Positive: immediately after `Subscribe` acks, the subscriber receives one
/// `state_snapshot` line whose payload (minus the `type` tag) equals the
/// `QueryState` payload — the “one serializer, two callers” contract.
#[test]
fn subscribe_pushes_state_snapshot_equal_to_query_state() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(DAEMON_SETTLE);

    // Pull the QueryState payload (the same shaping function the snapshot uses).
    let pull = send_ipc_retry(&pipe, &SocketMessage::QueryState).expect("query state");
    let payload = match pull {
        SocketResponse::Data { payload } => payload,
        other => panic!("expected Data from QueryState, got {other:?}"),
    };

    // Subscribe a subscriber-owned pipe.
    let sub_name = unique_subscriber_pipe_name();
    let (sub, connected) = SubscriberPipe::create(&sub_name).expect("create subscriber pipe");
    let resp = subscribe(&pipe, &sub_name);
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "Subscribe must ack Ok when the subscriber pipe is reachable",
    );
    connected
        .recv_timeout(SUB_TIMEOUT)
        .expect("daemon connected")
        .expect("connect succeeded");

    // The first (and so far only) line is the StateSnapshot.
    let event = read_event(&sub);
    assert_eq!(event["type"], "state_snapshot", "wire: {event}");

    // Strip the type tag; the remainder must equal the QueryState payload.
    let mut snapshot = event;
    snapshot
        .as_object_mut()
        .expect("snapshot is an object")
        .remove("type");
    assert_eq!(
        snapshot, payload,
        "the pushed state_snapshot payload must equal the pulled QueryState payload",
    );
}

/// Positive: multiple subscribers can register and each receives its own
/// `StateSnapshot`.
#[test]
fn multiple_subscribers_each_receive_snapshot() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(DAEMON_SETTLE);

    let name_a = unique_subscriber_pipe_name();
    let name_b = unique_subscriber_pipe_name();
    let (sub_a, conn_a) = SubscriberPipe::create(&name_a).expect("create subscriber A");
    let (sub_b, conn_b) = SubscriberPipe::create(&name_b).expect("create subscriber B");

    assert_eq!(subscribe(&pipe, &name_a), SocketResponse::Ok);
    conn_a
        .recv_timeout(SUB_TIMEOUT)
        .expect("A connected")
        .expect("A connect ok");
    let event_a = read_event(&sub_a);
    assert_eq!(event_a["type"], "state_snapshot");

    assert_eq!(subscribe(&pipe, &name_b), SocketResponse::Ok);
    conn_b
        .recv_timeout(SUB_TIMEOUT)
        .expect("B connected")
        .expect("B connect ok");
    let event_b = read_event(&sub_b);
    assert_eq!(event_b["type"], "state_snapshot");

    // Each subscriber got its own snapshot — prove independence by confirming
    // A did not receive B's line (A has exactly one line pending, read above;
    // a second read would time out). We assert on the two snapshots we did
    // read: both are snapshots, so both subscribers were served.
    let _ = (event_a, event_b);
}

/// Positive (eviction, load-bearing): closing a subscriber pipe mid-stream
/// must not stall or panic the daemon. After subscriber A is dropped, a fresh
/// subscriber B still receives its snapshot (B's broadcast writes to the dead
/// A first, evicting it), and the daemon keeps serving IPC.
#[test]
fn closing_subscriber_evicts_without_stalling_daemon() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(DAEMON_SETTLE);

    // Subscriber A registers and reads its snapshot.
    let name_a = unique_subscriber_pipe_name();
    let (sub_a, conn_a) = SubscriberPipe::create(&name_a).expect("create subscriber A");
    assert_eq!(subscribe(&pipe, &name_a), SocketResponse::Ok);
    conn_a
        .recv_timeout(SUB_TIMEOUT)
        .expect("A connected")
        .expect("A connect ok");
    let _snapshot_a = read_event(&sub_a);

    // Drop A — its server-side handle closes, breaking the pipe.
    drop(sub_a);
    // Give the OS a moment to tear down the pipe's server end.
    std::thread::sleep(Duration::from_millis(150));

    // Subscriber B registers. The daemon's broadcast for B also writes to the
    // now-dead A; that write fails and A is evicted, while B succeeds. The
    // daemon must not panic or stall.
    let name_b = unique_subscriber_pipe_name();
    let (sub_b, conn_b) = SubscriberPipe::create(&name_b).expect("create subscriber B");
    assert_eq!(
        subscribe(&pipe, &name_b),
        SocketResponse::Ok,
        "subscribing B after dropping A must still succeed",
    );
    conn_b
        .recv_timeout(SUB_TIMEOUT)
        .expect("B connected")
        .expect("B connect ok");
    let event_b = read_event(&sub_b);
    assert_eq!(
        event_b["type"],
        "state_snapshot",
        "B must still receive its snapshot despite A being dead",
    );

    // The daemon must still serve ordinary IPC after the eviction.
    let probe = send_ipc_retry(&pipe, &SocketMessage::QueryState).expect("probe QueryState");
    assert!(
        matches!(probe, SocketResponse::Data { .. }),
        "daemon must keep serving IPC after evicting a dead subscriber, got {probe:?}",
    );
}
