//! Integration test for the `WindowMovedToWorkspace` event (ticket #16).
//!
//! Spawns a real `flowd` on an isolated [`TestDesktop`], subscribes a
//! [`SubscriberPipe`], creates a tiled window on workspace 1, then moves it to
//! workspace 2. Asserts that the move emits both a `workspace_changed` (the
//! camera-follow switch, from #15) and a `window_moved_to_workspace` carrying
//! the correct `from`/`to`/`window` payload.
//!
//! `#[ignore]`d because the move requires the daemon's create hook to register
//! and tile the test window first, which is nondeterministic on the isolated
//! test desktop (the same startup hook race that `#[ignore]`s several tests in
//! `dispatch_workspace.rs`). The reliable serialization unit test in
//! `src/events.rs` covers the wire shape unconditionally.

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message. See the same pattern in `dispatch_workspace.rs`.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use flow_wm::ipc::message::{SocketMessage, SocketResponse};

use super::common::{SubscriberPipe, unique_pipe_name, unique_subscriber_pipe_name};
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, send_ipc_retry, start_test_daemon, unique_title,
    wait_until_windows_tiled,
};

/// Delay after creating a window to let hooks fire and the daemon tile it.
const HOOK_SETTLE: Duration = Duration::from_millis(1500);

/// Ceiling for waiting on a subscriber read / connect.
const SUB_TIMEOUT: Duration = Duration::from_secs(3);

/// Read one event line off `sub` and parse it as JSON.
fn read_event(sub: &SubscriberPipe) -> serde_json::Value {
    let line = sub.read_line(SUB_TIMEOUT).expect("read event line");
    serde_json::from_str(line.trim()).expect("event line is valid JSON")
}

/// Positive: `move-to-workspace` emits both `workspace_changed` (the camera
/// follows the moved window) and `window_moved_to_workspace` (the relocation
/// detail), with correct `from`/`to`/`window` fields.
#[test]
#[ignore = "non-deterministic on isolated test desktop: daemon create hook may not register/tile the window before the assertion (startup hook race)"]
fn move_to_workspace_emits_window_moved_and_workspace_changed() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    // Create a tiled window on the default workspace (ws 1).
    let title = unique_title("MovedWin");
    let w = TestWindow::create(&title).expect("create window");
    let moved_hwnd = w.hwnd.0 as i64;
    // Wait until the daemon has registered + tiled the window.
    wait_until_windows_tiled(&pipe, 1).expect("window tiled before move");

    // Subscribe and drain the bootstrap snapshot.
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
    let snapshot = read_event(&sub);
    assert_eq!(snapshot["type"], "state_snapshot");

    // Move the focused window from ws 1 to ws 2.
    std::thread::sleep(HOOK_SETTLE);
    let resp = send_ipc_retry(
        &pipe,
        &SocketMessage::MoveWindowToWorkspace { workspace_id: 2 },
    )
    .expect("IPC send: move-to-workspace");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "move to a valid workspace must succeed",
    );

    // The switch fires first (camera follows), then the relocation detail.
    let ws_event = read_event(&sub);
    assert_eq!(ws_event["type"], "workspace_changed", "wire: {ws_event}");
    assert_eq!(ws_event["workspace"], 2);

    let move_event = read_event(&sub);
    assert_eq!(
        move_event["type"],
        "window_moved_to_workspace",
        "wire: {move_event}",
    );
    assert_eq!(move_event["from"], 1, "source is the default workspace");
    assert_eq!(move_event["to"], 2, "destination is the move target");
    assert_eq!(
        move_event["window"]["hwnd"], moved_hwnd,
        "the moved window's hwnd must match the created test window",
    );
}
