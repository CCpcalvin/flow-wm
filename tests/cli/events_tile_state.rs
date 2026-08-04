//! Integration test for the `TileStateChanged` event (ticket #19).
//!
//! Spawns a real `flowd` on an isolated [`TestDesktop`], tiles a window,
//! subscribes a [`SubscriberPipe`], toggles the window to floating, and asserts
//! the next event line is a `tile_state_changed` with `state == "float"` and
//! the right window descriptor.
//!
//! # Reliability
//!
//! Unlike the `WorkspaceChanged` test, this one depends on the daemon’s window
//! hooks registering *and* tiling a created window before the assertion. On the
//! isolated, non-interactive `TestDesktop` that hook path can lose the race
//! (see the `#[ignore]`d tests in `dispatch_workspace.rs` for the same root
//! cause). The wire shape is additionally covered by the reliable unit tests in
//! `src/events.rs`, so this test guards only the live emit path.

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message. See the same pattern in `dispatch_workspace.rs`.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use flow_wm::ipc::message::{SocketMessage, SocketResponse, WindowMode};

use super::common::{SubscriberPipe, unique_pipe_name, unique_subscriber_pipe_name};
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, send_ipc_retry, start_test_daemon, unique_title,
    wait_until_windows_tiled,
};

/// Delay after spawning the daemon to let it enter its accept loop.
const DAEMON_SETTLE: Duration = Duration::from_millis(500);

/// Ceiling for waiting on a subscriber read / connect.
const SUB_TIMEOUT: Duration = Duration::from_secs(3);

/// Read one event line off `sub` and parse it as JSON.
fn read_event(sub: &SubscriberPipe) -> serde_json::Value {
    let line = sub.read_line(SUB_TIMEOUT).expect("read event line");
    serde_json::from_str(line.trim()).expect("event line is valid JSON")
}

/// Positive: `set-window float` on a tiled window emits a `tile_state_changed`
/// event whose `state` is `"float"` and whose `window` names the toggled
/// window. Read as the second line after the bootstrap `state_snapshot`.
#[test]
#[ignore = "non-deterministic on isolated test desktop: daemon hooks may not register/tile the created window before the assertion (startup hook race); wire shape is covered by the reliable unit tests in src/events.rs"]
fn set_window_float_emits_tile_state_changed_event() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(DAEMON_SETTLE);

    // Create and tile a window (the fixture rules tile unknown windows).
    let title = unique_title("TileState");
    let w = TestWindow::create(&title).expect("create window");
    let toggled_hwnd = w.hwnd.0 as i64;
    wait_until_windows_tiled(&pipe, 1).expect("created window tiled before toggle");

    // Subscribe — the daemon pushes a `state_snapshot` as the first line.
    let sub_name = unique_subscriber_pipe_name();
    let (sub, connected) = SubscriberPipe::create(&sub_name).expect("create subscriber pipe");
    assert_eq!(
        send_ipc_retry(
            &pipe,
            &SocketMessage::Subscribe {
                pipe_name: sub_name.clone(),
            },
        )
        .expect("IPC send: subscribe"),
        SocketResponse::Ok,
    );
    connected
        .recv_timeout(SUB_TIMEOUT)
        .expect("daemon connected")
        .expect("connect succeeded");
    // Drain the bootstrap snapshot so the next read is the live event.
    assert_eq!(read_event(&sub)["type"], "state_snapshot");

    // Toggle the focused window to floating.
    let resp = send_ipc_retry(&pipe, &SocketMessage::SetWindow { mode: WindowMode::Float })
        .expect("IPC send: set-window float");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "set-window float on a tiled window must succeed",
    );

    // The next event line must be the TileStateChanged with state "float".
    let event = read_event(&sub);
    assert_eq!(event["type"], "tile_state_changed", "wire: {event}");
    assert_eq!(
        event["state"], "float",
        "the window just became floating",
    );
    assert_eq!(
        event["window"]["hwnd"], toggled_hwnd,
        "the event must name the toggled window",
    );

    // Sanity: the daemon is still serving IPC after emitting the event.
    let probe = send_ipc_retry(&pipe, &SocketMessage::QueryState).expect("probe QueryState");
    assert!(
        matches!(probe, SocketResponse::Data { .. }),
        "daemon must keep serving IPC after emitting TileStateChanged, got {probe:?}",
    );
}
