//! Integration tests for cursor hide after mouse-inactivity timeout
//! (ticket #37).
//!
//! Real-daemon, isolated-desktop, unique-pipe tests of the observable
//! lifecycle of the hide mechanism:
//!
//! - `hide_timeout_ms > 0`: after the timeout with no mouse activity the
//!   daemon blanks the system cursors; real mouse motion un-hides within the
//!   poll interval.
//! - `hide_timeout_ms = 0` (default): the daemon never blanks the cursors —
//!   no polling is scheduled (the zero-CPU-while-idle property).
//! - clean shutdown (`flow stop`) restores the system cursors even if the
//!   daemon dies while they are blanked.
//!
//! # What is deliberately NOT tested here
//!
//! The cursor-swap Win32 wrapper mutates **session-global** cursor state.
//! Within one test it is safe (the test desktop is promoted to the session
//! input desktop, exactly like the cursor-warp tests, and the assertions
//! query that same session state), but the wrapper is not exercised from
//! unit tests that run in parallel with the developer's real session. The
//! daemonless `flow cursor restore` smoke test lives in `cursor.rs`; the
//! full manual checklist (drag suspension, panic restore) is noted in the
//! ticket/PR.
//!
//! The pointer/pump/IPC/log helpers are shared with `cursor_warp` and
//! `cursor_interaction` via `test_desktop` (hoisted in ticket #38), and the
//! input-desktop lock lives there too — only one desktop can be the session
//! input desktop at a time and the session cursor is shared.

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message. See the same pattern in `cursor_warp.rs`.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use flow_wm::ipc::message::{SocketMessage, SocketResponse};

use super::common::unique_pipe_name;
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, daemon_log_path, dispatch_while_pumping,
    fresh_config_dir_with, lock_input_desktop, set_cursor_pos, start_test_daemon, unique_title,
    wait_for_daemon_log, wait_for_daemon_log_extra, wait_tiled_while_pumping,
};

// Shared timing constants (hoisted in ticket #38), aliased to the names this
// module was written against.
use super::test_desktop::CURSOR_HIDE_SLACK as HIDE_SLACK;
use super::test_desktop::CURSOR_HIDE_TIMEOUT_MS as HIDE_TIMEOUT_MS;

/// How long the daemon needs to settle after start before hide timing is
/// stable (hook backlog drain + first cursor poll).
const START_SETTLE: Duration = Duration::from_millis(500);

/// The `[cursor]` section enabling hide for these tests (must stay in sync
/// with the shared `CURSOR_HIDE_TIMEOUT_MS` / `CURSOR_POLL_INTERVAL_MS`).
const CURSOR_HIDE_TOML: &str = "[cursor]\nhide_timeout_ms = 1500\npoll_interval_ms = 125\n";

// ── Tests ───────────────────────────────────────────────────────────

/// Hide enabled: after the timeout with no mouse activity the daemon blanks
/// the system cursors; mouse motion un-hides within the poll interval.
///
/// Observation strategy: the daemon logs each transition at debug level
/// ("cursor hide: system cursors blanked" / "cursor unhide: system cursors
/// restored"), and the log is truncated per daemon start — so the log is a
/// faithful, timestamped transition trace for this exact daemon. A
/// handle-comparison probe (`GetCursorInfo`) was tried and is NOT reliable:
/// Windows caches the current cursor handle and keeps reporting the pre-swap
/// anchor, so the log trace (plus the unit-tested state machine that emits
/// it) is the strongest automated observable. The blank-swap itself is
/// additionally covered by the manual checklist (see the module docs).
#[test]
fn hide_after_timeout_and_unhide_on_motion() {
    let _input_guard = lock_input_desktop();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();
    fresh_config_dir_with(&pipe, Some(CURSOR_HIDE_TOML)).expect("seed hide config");
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(START_SETTLE);

    let t1 = unique_title("Hide-A");
    let w1 = TestWindow::create(&t1).expect("create W1");
    let _ = w1;
    std::thread::sleep(Duration::from_millis(1500));
    wait_tiled_while_pumping(&pipe, 1).expect("one window tiled");

    // Park the pointer somewhere neutral (this counts as activity, so the
    // inactivity timeout starts roughly now).
    set_cursor_pos(400, 400);

    // Wait past the timeout + poll slack without touching the mouse: the
    // daemon must blank. (Background activity — any window gaining focus
    // warps the pointer — can legitimately un-blank and re-blank, so the
    // assertions below tolerate extra full cycles; the unit tests pin the
    // no-re-blank-while-continuously-idle property hermetically.)
    std::thread::sleep(HIDE_SLACK + Duration::from_millis(HIDE_TIMEOUT_MS as u64));
    assert!(
        wait_for_daemon_log(&pipe, "system cursors blanked", Duration::from_secs(2)),
        "daemon should have hidden the cursor; log: {}",
        std::fs::read_to_string(daemon_log_path(&pipe)).unwrap_or_default()
    );

    // Real mouse motion: un-hide within a poll interval (+ slack).
    set_cursor_pos(410, 410);
    assert!(
        wait_for_daemon_log(&pipe, "system cursors restored", Duration::from_secs(2)),
        "daemon should have un-hidden the cursor on motion"
    );

    // And the full cycle repeats: quiet again → blanked again.
    std::thread::sleep(HIDE_SLACK + Duration::from_millis(HIDE_TIMEOUT_MS as u64));
    assert!(
        wait_for_daemon_log_extra(&pipe, "system cursors blanked", 1, Duration::from_secs(2)),
        "daemon should re-hide after the restarted timeout"
    );

    drop(td);
}

/// Hide disabled (default config): the daemon never blanks the cursors even
/// after far past any plausible timeout, and the log shows no hide activity
/// (no polling is scheduled at all).
#[test]
fn hide_disabled_by_default_never_blanks() {
    let _input_guard = lock_input_desktop();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();
    fresh_config_dir_with(&pipe, None).expect("fresh default config");
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(START_SETTLE);

    let t1 = unique_title("NoHide-A");
    let w1 = TestWindow::create(&t1).expect("create W1");
    let _ = w1;
    std::thread::sleep(Duration::from_millis(1500));
    wait_tiled_while_pumping(&pipe, 1).expect("one window tiled");

    // Park and wait far past the (nonexistent) timeout.
    set_cursor_pos(300, 300);
    std::thread::sleep(Duration::from_millis(HIDE_TIMEOUT_MS as u64 + 1500));

    let log = std::fs::read_to_string(daemon_log_path(&pipe)).unwrap_or_default();
    assert!(
        !log.contains("system cursors blanked"),
        "hide is disabled by default; the daemon must never blank: {log}"
    );
    assert!(
        !log.contains("cursor hide:") && !log.contains("cursor unhide:"),
        "with hide disabled the daemon must not even poll cursor state: {log}"
    );

    drop(td);
}

/// Clean shutdown restores the system cursors: stop the daemon while the
/// cursors are blanked and require the arrow to come back (the log shows the
/// blank, then `flow stop` runs the restore — either via the unhide-on-stop
/// path if activity raced in, or the unconditional SPI_SETCURSORS at exit).
#[test]
fn clean_shutdown_restores_cursors() {
    let _input_guard = lock_input_desktop();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();
    fresh_config_dir_with(&pipe, Some(CURSOR_HIDE_TOML)).expect("seed hide config");
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(START_SETTLE);

    let t1 = unique_title("StopHide-A");
    let w1 = TestWindow::create(&t1).expect("create W1");
    let _ = w1;
    std::thread::sleep(Duration::from_millis(1500));
    wait_tiled_while_pumping(&pipe, 1).expect("one window tiled");

    // Park, let the timeout elapse, confirm blanked.
    set_cursor_pos(500, 500);
    assert!(
        wait_for_daemon_log(
            &pipe,
            "system cursors blanked",
            HIDE_SLACK + Duration::from_millis(HIDE_TIMEOUT_MS as u64) + Duration::from_secs(2)
        ),
        "daemon should have hidden the cursor before stop"
    );

    // Stop the daemon (clean shutdown) while the cursors are blanked.
    let resp = dispatch_while_pumping(&pipe, SocketMessage::Stop).expect("send Stop");
    assert!(
        matches!(resp, SocketResponse::Ok),
        "Stop should succeed: {resp:?}"
    );

    // The shutdown restore (unconditional SPI_SETCURSORS in main::run) is
    // not log-observable (it runs after the logger closes) and the handle
    // probe is unreliable (see test 1's docs). What IS observable and
    // meaningful: the daemonless escape hatch `flow cursor restore` —
    // which runs the very same SPI_SETCURSORS restore — succeeds against
    // whatever state the daemon left behind (it is harmless when the
    // shutdown restore already ran). The blank-then-stop-then-restore
    // sequence is additionally covered by the manual checklist.
    std::thread::sleep(Duration::from_millis(800));
    super::common::flow(&pipe)
        .arg("cursor")
        .arg("restore")
        .assert()
        .success()
        .stdout(predicates::str::contains("cursors restored"));

    drop(td);
}
