//! Integration test for the `ApplicationExiting` event (ticket #20).
//!
//! On a graceful `flow stop`, every registered subscriber must receive
//! `application_exiting` as its last line, followed by EOF (the daemon closes
//! the subscriber pipes during teardown). A crash skips the event — the
//! subscriber detects it via EOF and follows the same reconnect path — so only
//! the graceful case is asserted here.
//!
//! This test does not depend on window tiling or `SetWindowPos`, so it is not
//! subject to the isolated-desktop limitations that `#[ignore]` other tests.

// The daemon child process exits on its own after `Stop`; the guard is a
// safety net only. See the same allow in `events_subscribe.rs`.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use flow_wm::ipc::message::{SocketMessage, SocketResponse};

use super::common::{SubscriberPipe, unique_pipe_name, unique_subscriber_pipe_name};
use super::test_desktop::{DaemonGuard, TestDesktop, send_ipc_retry, start_test_daemon};

/// Delay after spawning the daemon to let it enter its accept loop.
const DAEMON_SETTLE: Duration = Duration::from_millis(500);

/// Ceiling for waiting on a subscriber read. Generous: after `Stop` the daemon
/// runs `rescue_stranded_windows` and tears down before closing the subscriber
/// pipes, so EOF may lag the `application_exiting` line by a moment.
const SUB_TIMEOUT: Duration = Duration::from_secs(5);

/// Positive: a graceful `Stop` pushes `application_exiting` to every
/// subscriber as the last line, then the subscriber's read returns EOF.
#[test]
fn graceful_stop_emits_application_exiting_then_eof() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(DAEMON_SETTLE);

    // Subscribe and drain the initial `state_snapshot` so the only remaining
    // line is the shutdown event.
    let sub_name = unique_subscriber_pipe_name();
    let (sub, connected) = SubscriberPipe::create(&sub_name).expect("create subscriber pipe");
    let resp = send_ipc_retry(
        &pipe,
        &SocketMessage::Subscribe {
            pipe_name: sub_name.clone(),
        },
    )
    .expect("subscribe");
    assert_eq!(resp, SocketResponse::Ok, "Subscribe must ack Ok");
    connected
        .recv_timeout(SUB_TIMEOUT)
        .expect("daemon connected")
        .expect("connect succeeded");
    let snapshot = sub.read_line(SUB_TIMEOUT).expect("read initial snapshot");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(snapshot.trim()).unwrap()["type"],
        "state_snapshot",
        "the first line after subscribe is the snapshot",
    );

    // Gracefully stop the daemon. `Ok` is written before the shutdown broadcast,
    // so the round trip completes before the daemon exits.
    let stop_resp = send_ipc_retry(&pipe, &SocketMessage::Stop).expect("send stop");
    assert_eq!(stop_resp, SocketResponse::Ok, "Stop must ack Ok");

    // Drain remaining lines until EOF. The last event received must be
    // `application_exiting`; the following read returns EOF (the daemon closed
    // the subscriber pipe during teardown).
    let mut last: Option<serde_json::Value> = None;
    while let Ok(line) = sub.read_line(SUB_TIMEOUT) {
        last =
            Some(serde_json::from_str(line.trim()).expect("event line is valid JSON"));
    }
    let last = last.expect("at least the application_exiting line was received");
    assert_eq!(
        last["type"],
        "application_exiting",
        "the last event before EOF must be application_exiting, got {last}",
    );
}
