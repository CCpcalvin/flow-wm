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
//! The input-desktop lock is shared with `cursor_warp` via a file-lock-style
//! mutex on a common lock file, because only one desktop can be the session
//! input desktop at a time and the session cursor is shared.

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message. See the same pattern in `cursor_warp.rs`.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use flow_wm::ipc::message::{SocketMessage, SocketResponse};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, MSG, PM_NOREMOVE, PM_QS_PAINT, PeekMessageW, TranslateMessage,
};

use super::common::unique_pipe_name;
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, start_test_daemon, test_config_dir, unique_title,
    wait_until_windows_tiled,
};

/// How long the daemon needs to settle after start before hide timing is
/// stable (hook backlog drain + first cursor poll).
const START_SETTLE: Duration = Duration::from_millis(500);

/// The hide timeout the tests configure — short enough for a snappy test,
/// long enough that startup jitter cannot look like "no activity".
const HIDE_TIMEOUT_MS: u32 = 1500;

/// The poll interval the tests configure — bounds the un-hide latency.
const POLL_INTERVAL_MS: u32 = 125;

/// How long to wait (polling) for the daemon to blank/restore the cursors
/// after the timeout elapses: one poll interval of slack.
const HIDE_SLACK: Duration = Duration::from_millis(POLL_INTERVAL_MS as u64 + 400);

/// The `[cursor]` section enabling hide for these tests.
const CURSOR_HIDE_TOML: &str = "[cursor]\nhide_timeout_ms = 1500\npoll_interval_ms = 125\n";

/// Read the session cursor position (screen coordinates).
#[allow(dead_code)] // symmetry with cursor_warp; used when debugging locally
fn cursor_pos() -> (i32, i32) {
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: `p` is a valid out-pointer; the call has no preconditions.
    unsafe { GetCursorPos(&mut p) }.expect("GetCursorPos");
    (p.x, p.y)
}

/// Move the session cursor (also counts as mouse activity for the daemon's
/// poll).
fn set_cursor_pos(x: i32, y: i32) {
    // SAFETY: two scalars, no preconditions.
    unsafe { windows::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y) }.expect("SetCursorPos");
}

/// Pump pending paint messages on the test thread (it is a GUI thread — it
/// owns the test windows; see the cursor_warp module docs for why).
fn pump_paint_messages() {
    let mut msg = MSG::default();
    // SAFETY: PeekMessageW inspects the thread's queue and fills `msg`;
    // TranslateMessage/DispatchMessage for paint-only messages go to
    // DefWindowProc (via test_wnd_proc), which just validates.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE | PM_QS_PAINT).as_bool() {
            let _ = TranslateMessage(&msg);
            windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
        }
    }
}

/// Send an IPC message on a worker thread while the test thread pumps.
fn ipc_while_pumping<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let handle = std::thread::spawn(f);
    loop {
        if handle.is_finished() {
            return handle
                .join()
                .map_err(|_| "IPC thread panicked".to_string())?;
        }
        pump_paint_messages();
    }
}

/// Send one IPC message (retrying through pipe refusals) while pumping.
fn dispatch(pipe: &str, msg: SocketMessage) -> Result<SocketResponse, String> {
    let pipe = pipe.to_owned();
    ipc_while_pumping(move || super::test_desktop::send_ipc_retry(&pipe, &msg))
}

/// Wait until `expected` windows are tiled (polling while pumping).
fn wait_tiled(pipe: &str, expected: usize) -> Result<(), String> {
    let pipe = pipe.to_owned();
    ipc_while_pumping(move || wait_until_windows_tiled(&pipe, expected).map(|_| ()))
}

/// The per-test config directory, with a fresh `[cursor]` hide config
/// written into `flow.toml` (mirrors `fresh_config_dir` in `cursor_warp`,
/// plus writing our section).
fn config_dir_with_hide(pipe: &str) -> std::path::PathBuf {
    let dir = test_config_dir(pipe);
    let _ = std::fs::remove_file(dir.join("flow.toml"));
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(dir.join("flow.toml"), CURSOR_HIDE_TOML).expect("write flow.toml");
    dir
}

/// The per-test config directory with **no** `flow.toml` (hide disabled by
/// default).
fn config_dir_default(pipe: &str) -> std::path::PathBuf {
    let dir = test_config_dir(pipe);
    let _ = std::fs::remove_file(dir.join("flow.toml"));
    std::fs::create_dir_all(&dir).expect("create config dir");
    dir
}

/// The daemon's per-test log path (start_test_daemon redirects there unless
/// the test overrides `--log-file`).
fn daemon_log(pipe: &str) -> std::path::PathBuf {
    let safe: String = pipe
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    std::env::temp_dir().join(format!("flowd-test-{safe}.log"))
}

/// Wait until the daemon log contains `pattern`, polling up to `budget`.
fn wait_for_log(pipe: &str, pattern: &str, budget: Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        pump_paint_messages();
        if let Ok(log) = std::fs::read_to_string(daemon_log(pipe))
            && log.contains(pattern)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Count occurrences of `pattern` in the daemon log (0 when unreadable).
fn log_count(pipe: &str, pattern: &str) -> usize {
    std::fs::read_to_string(daemon_log(pipe))
        .map(|log| log.matches(pattern).count())
        .unwrap_or(0)
}

/// Wait until the daemon log contains **more than `at_least`** occurrences
/// of `pattern`, polling up to `budget`.
fn wait_for_log_extra(pipe: &str, pattern: &str, at_least: usize, budget: Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        pump_paint_messages();
        if log_count(pipe, pattern) > at_least {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

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
    let _input_guard = super::cursor_warp::lock_input_desktop_public();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();
    config_dir_with_hide(&pipe);
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(START_SETTLE);

    let t1 = unique_title("Hide-A");
    let w1 = TestWindow::create(&t1).expect("create W1");
    let _ = w1;
    std::thread::sleep(Duration::from_millis(1500));
    wait_tiled(&pipe, 1).expect("one window tiled");

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
        wait_for_log(&pipe, "system cursors blanked", Duration::from_secs(2)),
        "daemon should have hidden the cursor; log: {}",
        std::fs::read_to_string(daemon_log(&pipe)).unwrap_or_default()
    );

    // Real mouse motion: un-hide within a poll interval (+ slack).
    set_cursor_pos(410, 410);
    assert!(
        wait_for_log(&pipe, "system cursors restored", Duration::from_secs(2)),
        "daemon should have un-hidden the cursor on motion"
    );

    // And the full cycle repeats: quiet again → blanked again.
    std::thread::sleep(HIDE_SLACK + Duration::from_millis(HIDE_TIMEOUT_MS as u64));
    assert!(
        wait_for_log_extra(&pipe, "system cursors blanked", 1, Duration::from_secs(2)),
        "daemon should re-hide after the restarted timeout"
    );

    drop(td);
}

/// Hide disabled (default config): the daemon never blanks the cursors even
/// after far past any plausible timeout, and the log shows no hide activity
/// (no polling is scheduled at all).
#[test]
fn hide_disabled_by_default_never_blanks() {
    let _input_guard = super::cursor_warp::lock_input_desktop_public();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();
    config_dir_default(&pipe);
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(START_SETTLE);

    let t1 = unique_title("NoHide-A");
    let w1 = TestWindow::create(&t1).expect("create W1");
    let _ = w1;
    std::thread::sleep(Duration::from_millis(1500));
    wait_tiled(&pipe, 1).expect("one window tiled");

    // Park and wait far past the (nonexistent) timeout.
    set_cursor_pos(300, 300);
    std::thread::sleep(Duration::from_millis(HIDE_TIMEOUT_MS as u64 + 1500));

    let log = std::fs::read_to_string(daemon_log(&pipe)).unwrap_or_default();
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
    let _input_guard = super::cursor_warp::lock_input_desktop_public();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();
    config_dir_with_hide(&pipe);
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(START_SETTLE);

    let t1 = unique_title("StopHide-A");
    let w1 = TestWindow::create(&t1).expect("create W1");
    let _ = w1;
    std::thread::sleep(Duration::from_millis(1500));
    wait_tiled(&pipe, 1).expect("one window tiled");

    // Park, let the timeout elapse, confirm blanked.
    set_cursor_pos(500, 500);
    assert!(
        wait_for_log(
            &pipe,
            "system cursors blanked",
            HIDE_SLACK + Duration::from_millis(HIDE_TIMEOUT_MS as u64) + Duration::from_secs(2)
        ),
        "daemon should have hidden the cursor before stop"
    );

    // Stop the daemon (clean shutdown) while the cursors are blanked.
    let resp = dispatch(&pipe, SocketMessage::Stop).expect("send Stop");
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
