//! Cross-feature integration tests: warp = activity (ticket #38, spec #35).
//!
//! The one rule governing how cursor warp (#34) and cursor hide (#37)
//! compose, exercised against a real daemon on an isolated desktop:
//!
//! - With hide enabled and the cursor hidden, a keyboard focus dispatch
//!   (pointer outside the new window) warps the pointer to the new center,
//!   **un-hides** it, and it hides again after the **full** timeout — never
//!   lingering visible forever.
//! - A hidden pointer already inside the newly focused window stays hidden:
//!   no warp occurred, so no activity event, no unhide.
//! - With hide disabled, warp behaves exactly as the warp ticket's tests —
//!   no regression from the interaction wiring.
//! - Hot-reload of the hide knob mid-flight keeps the invariant: disabling
//!   leaves pure warp; enabling with the pointer inside the focused window
//!   hides after the timeout.
//!
//! Observation strategy matches `cursor_hide`: hide/unhide transitions are
//! read from the daemon's per-test log trace ("system cursors blanked" /
//! "system cursors restored"), and pointer positions via `GetCursorPos` on
//! the promoted input desktop. All tests share the input-desktop lock with
//! `cursor_warp` / `cursor_hide` (session cursor state is global).

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message. See the same pattern in `cursor_warp.rs`.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use flow_wm::ipc::message::{SocketMessage, SocketResponse};

use super::common::unique_pipe_name;
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, actual_rect_of, cursor_hide_wait, cursor_pos,
    daemon_log_count, daemon_log_path, dispatch_while_pumping, fresh_config_dir_with,
    lock_input_desktop, query_actual_while_pumping, set_cursor_pos, start_test_daemon,
    unique_title, wait_for_daemon_log, wait_for_daemon_log_extra, wait_tiled_while_pumping,
};

// Shared timing constants (hoisted in ticket #38), aliased to the names this
// module was written against.
use super::test_desktop::CURSOR_HIDE_SLACK as HIDE_SLACK;
use super::test_desktop::CURSOR_HIDE_TIMEOUT_MS as HIDE_TIMEOUT_MS;
use super::test_desktop::CURSOR_HOOK_SETTLE as HOOK_SETTLE;
use super::test_desktop::CURSOR_WARP_SETTLE as WARP_SETTLE;

/// Wait budget layered on top of [`cursor_hide_wait`] when polling the log.
const LOG_BUDGET: Duration = Duration::from_secs(2);

/// The `[cursor]` section with both features on (must stay in sync with the
/// shared `CURSOR_HIDE_TIMEOUT_MS` / `CURSOR_POLL_INTERVAL_MS`).
const WARP_AND_HIDE_TOML: &str =
    "[cursor]\nwarp_on_focus = true\nhide_timeout_ms = 1500\npoll_interval_ms = 125\n";

/// Park the pointer in the left screen-edge gap (outside every column, same
/// spot the warp tests use), vertically centered on `rect`.
fn park_outside_left(rect: (i32, i32, i32, i32)) {
    set_cursor_pos(4, rect.1 + rect.3 / 2);
}

// ── Tests ───────────────────────────────────────────────────────────

/// Warp while hidden → pointer visible at the new center AND hides again
/// after the **full** timeout restarted from the warp.
#[test]
fn warp_while_hidden_unhides_and_rehides_after_full_timeout() {
    let _input_guard = lock_input_desktop();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();
    fresh_config_dir_with(&pipe, Some(WARP_AND_HIDE_TOML)).expect("seed warp+hide config");
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let t1 = unique_title("Inv-A");
    let t2 = unique_title("Inv-B");
    let w1 = TestWindow::create(&t1).expect("create W1");
    std::thread::sleep(Duration::from_millis(400));
    let w2 = TestWindow::create(&t2).expect("create W2");
    std::thread::sleep(HOOK_SETTLE);

    wait_tiled_while_pumping(&pipe, 2).expect("two windows tiled");
    let layout = query_actual_while_pumping(&pipe).expect("query layout actual");
    let r1 = actual_rect_of(&layout, w1.hwnd).expect("W1 in actual layout");

    // Park outside W1 (this is activity, so the quiet stretch starts now),
    // then idle until the daemon blanks the cursors.
    park_outside_left(r1);
    std::thread::sleep(cursor_hide_wait());
    assert!(
        wait_for_daemon_log(&pipe, "system cursors blanked", LOG_BUDGET),
        "cursor should hide while idle; log: {}",
        std::fs::read_to_string(daemon_log_path(&pipe)).unwrap_or_default()
    );
    let hidden_at = cursor_pos();
    assert_eq!(
        hidden_at,
        (4, r1.1 + r1.3 / 2),
        "sanity: a hidden cursor must still report its position"
    );

    // Keyboard focus navigation: dispatch focus to W1. The pointer is
    // outside W1, so a warp fires — which must un-hide the cursor.
    let resp = dispatch_while_pumping(&pipe, SocketMessage::FocusLeft).expect("send FocusLeft");
    assert!(
        matches!(resp, SocketResponse::Ok),
        "FocusLeft should succeed (two columns): {resp:?}"
    );
    std::thread::sleep(WARP_SETTLE);

    // Position asserted via the same seam the warp tests use: the pointer
    // now sits at W1's projected center.
    let after = cursor_pos();
    let expected = (r1.0 + r1.2 / 2, r1.1 + r1.3 / 2);
    assert_eq!(
        after, expected,
        "warp while hidden must land the (now visible) pointer at W1 center {expected:?}"
    );
    assert!(
        wait_for_daemon_log(&pipe, "system cursors restored", LOG_BUDGET),
        "the warp must count as activity and un-hide the cursor; log: {}",
        std::fs::read_to_string(daemon_log_path(&pipe)).unwrap_or_default()
    );

    // ...and the inactivity timer restarted from the warp: the cursor hides
    // again after the FULL timeout, never lingering visible forever.
    std::thread::sleep(cursor_hide_wait());
    assert!(
        wait_for_daemon_log_extra(&pipe, "system cursors blanked", 1, LOG_BUDGET),
        "cursor must re-hide one full timeout after the warp; log: {}",
        std::fs::read_to_string(daemon_log_path(&pipe)).unwrap_or_default()
    );

    drop(w1);
    drop(w2);
    drop(td);
}

/// Hidden pointer already inside the newly focused window → stays hidden
/// (no warp occurred → no activity event → no unhide, timer untouched).
#[test]
fn hidden_pointer_inside_new_window_stays_hidden() {
    let _input_guard = lock_input_desktop();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();
    fresh_config_dir_with(&pipe, Some(WARP_AND_HIDE_TOML)).expect("seed warp+hide config");
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let t1 = unique_title("StayHidden-A");
    let t2 = unique_title("StayHidden-B");
    let w1 = TestWindow::create(&t1).expect("create W1");
    std::thread::sleep(Duration::from_millis(400));
    let w2 = TestWindow::create(&t2).expect("create W2");
    std::thread::sleep(HOOK_SETTLE);

    wait_tiled_while_pumping(&pipe, 2).expect("two windows tiled");
    let layout = query_actual_while_pumping(&pipe).expect("query layout actual");
    let r1 = actual_rect_of(&layout, w1.hwnd).expect("W1 in actual layout");

    // Park at an off-center position INSIDE W1 (the warp target), so a buggy
    // warp-to-center would move it — parking is activity, so the quiet
    // stretch starts now. Then idle until blanked.
    let parked = (r1.0 + r1.2 / 4, r1.1 + r1.3 / 4);
    set_cursor_pos(parked.0, parked.1);
    std::thread::sleep(cursor_hide_wait());
    assert!(
        wait_for_daemon_log(&pipe, "system cursors blanked", LOG_BUDGET),
        "cursor should hide while idle inside W1; log: {}",
        std::fs::read_to_string(daemon_log_path(&pipe)).unwrap_or_default()
    );
    let restores_before = daemon_log_count(&pipe, "system cursors restored");

    // Focus W1 (focus is currently on W2): the pointer is inside W1, so the
    // skip-if-inside rule fires — no warp, no activity, stays hidden.
    let resp = dispatch_while_pumping(&pipe, SocketMessage::FocusLeft).expect("send FocusLeft");
    assert!(
        matches!(resp, SocketResponse::Ok),
        "FocusLeft should succeed: {resp:?}"
    );
    std::thread::sleep(WARP_SETTLE + HIDE_SLACK);

    // Skip-if-inside is evaluated against the pointer POSITION, not
    // visibility: the (invisible) pointer is byte-identically untouched...
    assert_eq!(
        cursor_pos(),
        parked,
        "pointer inside the newly focused window must stay byte-identical (hidden or not)"
    );
    // ...and no un-hide fired for the non-warp focus change.
    assert_eq!(
        daemon_log_count(&pipe, "system cursors restored"),
        restores_before,
        "a hidden pointer inside the newly focused window must stay hidden (no warp → no activity); log: {}",
        std::fs::read_to_string(daemon_log_path(&pipe)).unwrap_or_default()
    );

    drop(w1);
    drop(w2);
    drop(td);
}

/// A **visible** pointer inside the newly focused window is untouched AND the
/// skip does not restart the inactivity timer — skip-if-inside is evaluated
/// against the pointer position, not visibility, and a skipped warp is no
/// activity event.
#[test]
fn visible_pointer_inside_new_window_no_timer_restart() {
    let _input_guard = lock_input_desktop();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();
    fresh_config_dir_with(&pipe, Some(WARP_AND_HIDE_TOML)).expect("seed warp+hide config");
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let t1 = unique_title("VisSkip-A");
    let t2 = unique_title("VisSkip-B");
    let w1 = TestWindow::create(&t1).expect("create W1");
    std::thread::sleep(Duration::from_millis(400));
    let w2 = TestWindow::create(&t2).expect("create W2");
    std::thread::sleep(HOOK_SETTLE);

    wait_tiled_while_pumping(&pipe, 2).expect("two windows tiled");
    let layout = query_actual_while_pumping(&pipe).expect("query layout actual");
    let r1 = actual_rect_of(&layout, w1.hwnd).expect("W1 in actual layout");

    // Park inside W1 off-center (activity — the quiet stretch starts now) and
    // WAIT OUT roughly (timeout - warp-settle) of it: then focus W1 and let
    // the skip settle, so that a buggy timer-restart would push the hide
    // deadline past the observation window below.
    let parked = (r1.0 + r1.2 / 4, r1.1 + r1.3 / 4);
    set_cursor_pos(parked.0, parked.1);
    let partial = Duration::from_millis(HIDE_TIMEOUT_MS as u64).saturating_sub(WARP_SETTLE);
    std::thread::sleep(partial);

    let resp = dispatch_while_pumping(&pipe, SocketMessage::FocusLeft).expect("send FocusLeft");
    assert!(
        matches!(resp, SocketResponse::Ok),
        "FocusLeft should succeed: {resp:?}"
    );
    std::thread::sleep(WARP_SETTLE);
    assert_eq!(
        cursor_pos(),
        parked,
        "a visible pointer inside the newly focused window must stay byte-identical"
    );

    // The skip must NOT have restarted the timer: the original deadline (one
    // timeout from the park, minus the pre-focus wait) is already due, so the
    // daemon blanks within the remaining time + slack. A restart would push
    // the blank a full timeout later than this window.
    std::thread::sleep(HIDE_SLACK + WARP_SETTLE);
    assert!(
        wait_for_daemon_log(&pipe, "system cursors blanked", LOG_BUDGET),
        "a skipped warp must not restart the hide timer (hide was due now); log: {}",
        std::fs::read_to_string(daemon_log_path(&pipe)).unwrap_or_default()
    );

    drop(w1);
    drop(w2);
    drop(td);
}

/// With hide disabled, warp behavior is identical to the warp ticket's
/// tests — the interaction wiring must not regress pure warp.
#[test]
fn hide_disabled_warp_behavior_unchanged() {
    let _input_guard = lock_input_desktop();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();
    // Explicitly warp-on, hide off — the compiled default, but written out
    // so the test does not depend on the default for its meaning.
    fresh_config_dir_with(&pipe, Some("[cursor]\nwarp_on_focus = true\n"))
        .expect("seed warp-only config");
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let t1 = unique_title("PureWarp-A");
    let t2 = unique_title("PureWarp-B");
    let w1 = TestWindow::create(&t1).expect("create W1");
    std::thread::sleep(Duration::from_millis(400));
    let w2 = TestWindow::create(&t2).expect("create W2");
    std::thread::sleep(HOOK_SETTLE);

    wait_tiled_while_pumping(&pipe, 2).expect("two windows tiled");
    let layout = query_actual_while_pumping(&pipe).expect("query layout actual");
    let r1 = actual_rect_of(&layout, w1.hwnd).expect("W1 in actual layout");

    park_outside_left(r1);
    let resp = dispatch_while_pumping(&pipe, SocketMessage::FocusLeft).expect("send FocusLeft");
    assert!(
        matches!(resp, SocketResponse::Ok),
        "FocusLeft should succeed: {resp:?}"
    );
    std::thread::sleep(WARP_SETTLE);

    assert_eq!(
        cursor_pos(),
        (r1.0 + r1.2 / 2, r1.1 + r1.3 / 2),
        "with hide disabled the warp must behave exactly as the warp ticket's tests"
    );
    // And the daemon never touched cursor-visibility state.
    let log = std::fs::read_to_string(daemon_log_path(&pipe)).unwrap_or_default();
    assert!(
        !log.contains("cursor hide:") && !log.contains("cursor unhide:"),
        "with hide disabled the daemon must not touch cursor visibility: {log}"
    );

    drop(w1);
    drop(w2);
    drop(td);
}

/// Hot-reload of the hide knob mid-flight keeps the invariant: enabling hide
/// with the pointer inside the focused window hides it after the timeout;
/// disabling again restores it immediately and leaves pure warp.
#[test]
fn hot_reload_of_hide_knob_keeps_invariant() {
    let _input_guard = lock_input_desktop();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();
    // Phase A: pure warp (hide off).
    let config_dir = fresh_config_dir_with(&pipe, Some("[cursor]\nwarp_on_focus = true\n"))
        .expect("seed warp-only config");
    let flow_toml = config_dir.join("flow.toml");
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let t1 = unique_title("ReloadInv-A");
    let t2 = unique_title("ReloadInv-B");
    let w1 = TestWindow::create(&t1).expect("create W1");
    std::thread::sleep(Duration::from_millis(400));
    let w2 = TestWindow::create(&t2).expect("create W2");
    std::thread::sleep(HOOK_SETTLE);

    wait_tiled_while_pumping(&pipe, 2).expect("two windows tiled");
    let layout = query_actual_while_pumping(&pipe).expect("query layout actual");
    let r1 = actual_rect_of(&layout, w1.hwnd).expect("W1 in actual layout");
    let r2 = actual_rect_of(&layout, w2.hwnd).expect("W2 in actual layout");

    // Phase A: hide off — focus W1, pointer warps to its center (pure warp).
    park_outside_left(r1);
    let resp = dispatch_while_pumping(&pipe, SocketMessage::FocusLeft).expect("send FocusLeft");
    assert!(matches!(resp, SocketResponse::Ok));
    std::thread::sleep(WARP_SETTLE);
    assert_eq!(
        cursor_pos(),
        (r1.0 + r1.2 / 2, r1.1 + r1.3 / 2),
        "phase A (hide off): pure warp to W1 center"
    );

    // Phase B: enable hide mid-session. The pointer currently sits inside
    // the focused window (W1, from the warp) — after the timeout with no
    // further activity it must hide.
    std::fs::write(&flow_toml, WARP_AND_HIDE_TOML).expect("rewrite flow.toml (hide on)");
    let resp = dispatch_while_pumping(&pipe, SocketMessage::ReloadConfig).expect("ReloadConfig");
    assert!(
        matches!(resp, SocketResponse::Ok),
        "ReloadConfig should succeed: {resp:?}"
    );
    std::thread::sleep(cursor_hide_wait());
    assert!(
        wait_for_daemon_log(&pipe, "system cursors blanked", LOG_BUDGET),
        "phase B (hide enabled mid-session, pointer inside focused window): must hide after the timeout; log: {}",
        std::fs::read_to_string(daemon_log_path(&pipe)).unwrap_or_default()
    );

    // Phase C: disable hide again — the cursor is restored immediately and
    // warp keeps working (pure warp, no timer machinery).
    std::fs::write(&flow_toml, "[cursor]\nwarp_on_focus = true\n")
        .expect("rewrite flow.toml (hide off)");
    let resp = dispatch_while_pumping(&pipe, SocketMessage::ReloadConfig).expect("ReloadConfig");
    assert!(matches!(resp, SocketResponse::Ok));
    assert!(
        wait_for_daemon_log_extra(&pipe, "system cursors restored", 0, LOG_BUDGET),
        "phase C (hide disabled while hidden): cursor must be restored; log: {}",
        std::fs::read_to_string(daemon_log_path(&pipe)).unwrap_or_default()
    );
    park_outside_left(r1);
    let resp = dispatch_while_pumping(&pipe, SocketMessage::FocusRight).expect("FocusRight");
    assert!(matches!(resp, SocketResponse::Ok));
    std::thread::sleep(WARP_SETTLE);
    assert_eq!(
        cursor_pos(),
        (r2.0 + r2.2 / 2, r2.1 + r2.3 / 2),
        "phase C: pure warp to W2 center after disabling hide"
    );

    drop(w1);
    drop(w2);
    drop(td);
}
