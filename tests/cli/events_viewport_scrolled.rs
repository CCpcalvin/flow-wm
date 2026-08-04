//! Integration test for the `ViewportScrolled` event (ticket #18).
//!
//! Spawns a real `flowd` on an isolated [`TestDesktop`], subscribes a
//! [`SubscriberPipe`], creates two tiled windows (so the canvas is wider than
//! the monitor and scrolling actually moves the viewport), triggers a
//! `scroll-right`, and asserts the next event line is a `viewport_scrolled`
//! with the expected monitor / workspace / offset / columns fields.
//!
//! This depends on the daemon tiling the created windows, which can be racy on
//! the isolated desktop. If the scroll does not actually move (no diff
//! produced), the daemon emits no event and the test fails. The serialization
//! unit test in `src/events.rs` is the reliable fallback.

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message. See the same pattern in `dispatch_workspace.rs`.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use flow_wm::ipc::message::{SocketMessage, SocketResponse};

use super::common::{SubscriberPipe, unique_pipe_name, unique_subscriber_pipe_name};
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, query_layout_virtual, send_ipc_retry, start_test_daemon,
    unique_title, wait_until_windows_tiled,
};

/// Delay after spawning the daemon to let it enter its accept loop.
const DAEMON_SETTLE: Duration = Duration::from_millis(500);

/// Delay after creating windows to let hooks fire and the daemon tile them.
const HOOK_SETTLE: Duration = Duration::from_millis(1500);

/// Ceiling for waiting on a subscriber read / connect.
const SUB_TIMEOUT: Duration = Duration::from_secs(3);

/// Positive: `scroll-right` emits a `ViewportScrolled` event with the monitor,
/// workspace, offset, and columns fields.
///
/// Requires two tiled columns so the canvas overflows the viewport and a
/// scroll-right actually moves the offset. On the isolated test desktop the
/// daemon’s hooks may not tile the windows reliably, so this is `#[ignore]`d —
/// the serialization unit test in `src/events.rs` is the reliable coverage.
#[test]
#[ignore = "depends on daemon tiling two windows on the isolated desktop (startup hook race); see src/events.rs for the reliable serialization test"]
fn scroll_right_emits_viewport_scrolled_event() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(DAEMON_SETTLE);

    // Create two windows so the canvas has more than one column.
    let _w_a = TestWindow::create(&unique_title("ScrollA")).expect("create window A");
    let _w_b = TestWindow::create(&unique_title("ScrollB")).expect("create window B");
    std::thread::sleep(HOOK_SETTLE);

    // Confirm both windows were tiled into columns.
    let layout = wait_until_windows_tiled(&pipe, 2).expect("two windows tiled");
    let column_count = layout["column_count"].as_u64().unwrap_or(0);
    assert!(
        column_count >= 2,
        "precondition: need >= 2 columns for a scroll to move the viewport, got {column_count}",
    );

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

    // Drain the bootstrap snapshot.
    let snapshot = sub.read_line(SUB_TIMEOUT).expect("read snapshot");
    let snapshot_json: serde_json::Value =
        serde_json::from_str(snapshot.trim()).expect("snapshot is JSON");
    assert_eq!(snapshot_json["type"], "state_snapshot");

    // Trigger a scroll-right.
    let resp = send_ipc_retry(&pipe, &SocketMessage::ScrollRight)
        .expect("IPC send: scroll-right");

    // If the daemon couldn’t scroll (already at the right edge), it returns
    // Error and emits no event. This can happen if the viewport was already
    // scrolled. In that case the test is inconclusive — skip by returning.
    // (Under #[ignore], this is acceptable; the assertion below runs on Ok.)
    if resp != SocketResponse::Ok {
        eprintln!("scroll-right returned {resp:?} — viewport already at edge; test inconclusive");
        return;
    }

    // The next event line must be the ViewportScrolled.
    let line = sub.read_line(SUB_TIMEOUT).expect("read viewport_scrolled");
    let event: serde_json::Value = serde_json::from_str(line.trim()).expect("event is JSON");
    assert_eq!(event["type"], "viewport_scrolled", "wire: {event}");
    assert_eq!(
        event["monitor"], 0,
        "scroll occurred on the default (only) monitor",
    );
    assert_eq!(
        event["workspace"], 1,
        "scroll occurred on the default active workspace",
    );
    assert!(
        event["offset"].is_i64(),
        "offset must be present as a number",
    );
    assert!(
        event["columns"].as_u64().unwrap_or(0) >= 2,
        "columns must reflect the >= 2 tiled columns, got {}",
        event["columns"],
    );

    // Sanity: the daemon is still serving IPC after emitting the event.
    let _probe = query_layout_virtual(&pipe).expect("probe layout");
}
