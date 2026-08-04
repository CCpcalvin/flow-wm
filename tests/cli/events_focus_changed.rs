//! Integration test for the `FocusChanged` event (ticket #17).
//!
//! Spawns a real `flowd` on an isolated [`TestDesktop`], subscribes a
//! [`SubscriberPipe`], creates a `TestWindow` (which takes the OS foreground
//! on creation), and asserts the daemon emits a `focus_changed` event carrying
//! that window's descriptor.
//!
//! # Isolated-desktop caveat
//!
//! `FocusChanged` is driven by `EVENT_SYSTEM_FOREGROUND`, which fires when the
//! OS foreground changes. On the isolated `TestDesktop` that foreground change
//! is racy — the same limitation that `#[ignore]`s the focus/foreground
//! assertions in `window_creation_focus.rs` and several `dispatch_workspace`
//! tests. The test is therefore `#[ignore]`d by default; run it manually on an
//! interactive desktop. The `FocusChanged` wire shape is pinned reliably (and
//! un-ignored) by the unit test in `src/events.rs`.

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

/// Delay after spawning the daemon to let it enter its accept loop.
const DAEMON_SETTLE: Duration = Duration::from_millis(500);

/// Ceiling for waiting on a subscriber read / connect.
const SUB_TIMEOUT: Duration = Duration::from_secs(3);

/// Read one event line off `sub` and parse it as JSON.
fn read_event(sub: &SubscriberPipe) -> serde_json::Value {
    let line = sub.read_line(SUB_TIMEOUT).expect("read event line");
    serde_json::from_str(line.trim()).expect("event line is valid JSON")
}

/// Positive: creating a window that takes the foreground emits a `focus_changed`
/// event with the correct monitor, workspace, and window descriptor.
#[test]
#[ignore = "isolated desktop makes the OS foreground change driving on_focus_changed nondeterministic (focus race); run manually on an interactive desktop"]
fn focus_change_emits_focus_changed_event() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(DAEMON_SETTLE);

    // Subscribe and drain the bootstrap snapshot first.
    let sub_name = unique_subscriber_pipe_name();
    let (sub, connected) = SubscriberPipe::create(&sub_name).expect("create subscriber pipe");
    let resp = send_ipc_retry(
        &pipe,
        &SocketMessage::Subscribe {
            pipe_name: sub_name.clone(),
        },
    )
    .expect("IPC send: subscribe");
    assert_eq!(resp, SocketResponse::Ok, "subscribe must ack Ok");
    connected
        .recv_timeout(SUB_TIMEOUT)
        .expect("daemon connected")
        .expect("connect succeeded");
    let snapshot = read_event(&sub);
    assert_eq!(snapshot["type"], "state_snapshot");

    // Create a window. The daemon registers it, tiles it, and pushes the OS
    // foreground to it — firing EVENT_SYSTEM_FOREGROUND, whose single sink
    // `on_focus_changed` emits `FocusChanged`.
    let title = unique_title("FocusChanged");
    let w = TestWindow::create(&title).expect("create window");
    let hwnd_id = w.hwnd.0 as i64;

    // Wait until the daemon has tiled the window. `FocusChanged` is broadcast
    // as part of the same creation handler (after the insert), so it is already
    // in the pipe buffer by the time the window appears tiled.
    wait_until_windows_tiled(&pipe, 1).expect("window tiled");

    // Find the focus_changed line among the buffered events.
    let mut found: Option<serde_json::Value> = None;
    for _ in 0..10 {
        match sub.read_line(Duration::from_secs(1)) {
            Ok(line) => {
                let v: serde_json::Value =
                    serde_json::from_str(line.trim()).expect("valid JSON line");
                if v["type"] == "focus_changed" && v["window"]["hwnd"] == hwnd_id {
                    found = Some(v);
                    break;
                }
            }
            Err(_) => break, // no more data buffered
        }
    }
    let event = found.expect("expected a focus_changed event for the created window");

    assert_eq!(event["monitor"], 0, "default (only) monitor");
    assert_eq!(event["workspace"], 1, "default active workspace");
    assert_eq!(event["window"]["hwnd"], hwnd_id, "the focused window's hwnd");
    assert_eq!(event["window"]["title"], title.as_str());
    // The descriptor also carries exe/class — assert they are present (non-empty)
    // rather than asserting exact values, which depend on the test binary's
    // image path and the registered window class.
    assert!(
        event["window"]["exe"].as_str().is_some_and(|s| !s.is_empty()),
        "window.exe must be a non-empty string",
    );
    assert!(
        event["window"]["class"].as_str().is_some_and(|s| !s.is_empty()),
        "window.class must be a non-empty string",
    );

    // Sanity: the daemon still serves IPC after emitting the event.
    let probe = send_ipc_retry(&pipe, &SocketMessage::Ping).expect("probe ping");
    assert_eq!(probe, SocketResponse::Ok, "daemon must keep serving IPC");
}
