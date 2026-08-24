//! Integration tests for cursor warp on focus change (ticket #34).
//!
//! Real-daemon, isolated-desktop, unique-pipe tests of the warp invariant:
//! `flow dispatch focus-left` teleports the pointer to the newly focused
//! window's center (expected center derived from the existing
//! `query layout actual` IPC query), a pointer already inside the newly
//! focused window is left byte-identically untouched, and
//! `warp_on_focus = false` disables the warp on every path.
//!
//! # Observing the pointer across the desktop boundary
//!
//! The test thread joins the daemon's isolated desktop
//! ([`TestDesktop::make_input`] also promotes it to the session's *input*
//! desktop — cursor APIs are gated on the input desktop), so plain
//! `GetCursorPos` / `SetCursorPos` in the test process read/write the same
//! cursor the daemon warps. Setting the pointer up ("park it far outside any
//! window") uses `SetCursorPos` directly, the same Win32 call the daemon
//! makes.
//!
//! # Why the test thread must pump messages
//!
//! The test windows are created on the test thread, which makes it a GUI
//! thread under Win32 rules. The daemon's `set_foreground_window` sends
//! activation messages (`WM_NCACTIVATE` / `WM_ACTIVATE`) **synchronously**
//! to the current foreground window — the test's W2 — so while the test
//! thread blocks inside a pipe read, those sends stall until the
//! send-message timeout (~30 s) fires. Every IPC helper therefore spawns a
//! short-lived I/O thread and pumps Win32 messages on the test thread until
//! the response arrives, mirroring how a real foreground app stays
//! responsive.
//!
//! The pointer/pump/IPC helpers are shared with `cursor_hide` and
//! `cursor_interaction` via `test_desktop` (hoisted in ticket #38), and the
//! input-desktop lock lives there too — only one desktop can be the session
//! input desktop at a time and the session cursor is shared.

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message. See the same pattern in `dispatch_workspace.rs`.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use flow_wm::ipc::message::{SocketMessage, SocketResponse};

use super::common::unique_pipe_name;
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, actual_rect_of, cursor_pos, dispatch_while_pumping,
    fresh_config_dir_with, lock_input_desktop, query_actual_while_pumping, set_cursor_pos,
    start_test_daemon, unique_title, wait_tiled_while_pumping,
};

// Shared timing constants (hoisted in ticket #38), aliased to the names this
// module was written against.
use super::test_desktop::CURSOR_HOOK_SETTLE as HOOK_SETTLE;
use super::test_desktop::CURSOR_WARP_SETTLE as WARP_SETTLE;

/// Assert a position lies inside a rect (boundary-inclusive).
fn assert_inside(pos: (i32, i32), rect: (i32, i32, i32, i32), ctx: &str) {
    let (x, y, w, h) = rect;
    assert!(
        pos.0 >= x && pos.0 <= x + w && pos.1 >= y && pos.1 <= y + h,
        "pointer {pos:?} should be inside rect {rect:?} ({ctx})"
    );
}

// ── Tests ───────────────────────────────────────────────────────────

/// Dispatch focus → the pointer lands centered on the newly focused window.
///
/// With two tiled windows and the pointer parked outside both (in the left
/// screen-edge gap the projection reserves), `focus-left` moves focus to W1
/// and the pointer must land at W1's center (derived from
/// `query layout actual`, not hardcoded geometry).
#[test]
fn focus_dispatch_warps_pointer_to_new_window_center() {
    let _input_guard = lock_input_desktop();
    let mut td = TestDesktop::create().expect("test desktop");
    // Cursor APIs are gated on the input desktop — promote the isolated
    // desktop so both this test's GetCursorPos/SetCursorPos and the daemon's
    // warp SetCursorPos are permitted. Drop restores the real input desktop.
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();
    // Clear stale flow.toml from prior invocations (see test 1).
    fresh_config_dir_with(&pipe, None).expect("fresh config dir");
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let t1 = unique_title("Warp-A");
    let t2 = unique_title("Warp-B");

    // W1 first (focus lands on it), then W2 — final focus is W2 (rightmost).
    let w1 = TestWindow::create(&t1).expect("create W1");
    std::thread::sleep(Duration::from_millis(400));
    let w2 = TestWindow::create(&t2).expect("create W2");
    std::thread::sleep(HOOK_SETTLE);

    wait_tiled_while_pumping(&pipe, 2).expect("two windows tiled");
    let layout = query_actual_while_pumping(&pipe).expect("query layout actual");
    let r1 = actual_rect_of(&layout, w1.hwnd).expect("W1 in actual layout");
    let r2 = actual_rect_of(&layout, w2.hwnd).expect("W2 in actual layout");

    // Park the pointer in the left screen-edge gap (the tiling projection
    // reserves a `window_gap` margin, default 16px, before the first
    // column's left edge at x=16) — outside W1, outside W2. FocusLeft then
    // moves focus to W1, and the warp must land the pointer at W1's center.
    set_cursor_pos(4, r1.1 + r1.3 / 2);
    let before = cursor_pos();
    assert_ne!(before, (0, 0), "sanity: pointer parked");

    let resp = dispatch_while_pumping(&pipe, SocketMessage::FocusLeft).expect("send FocusLeft");
    assert!(
        matches!(resp, SocketResponse::Ok),
        "FocusLeft should succeed (two columns): {resp:?}"
    );
    std::thread::sleep(WARP_SETTLE);

    let after = cursor_pos();
    let expected = (r1.0 + r1.2 / 2, r1.1 + r1.3 / 2);
    assert_eq!(
        after, expected,
        "pointer should warp to W1 center {expected:?} (W1 rect {r1:?}, W2 rect {r2:?})"
    );

    drop(w1);
    drop(w2);
    drop(td);
}

/// Pointer already inside the newly focused window → byte-identical, no yank.
#[test]
fn pointer_inside_new_window_is_left_untouched() {
    let _input_guard = lock_input_desktop();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();
    // Clear stale flow.toml from prior invocations (see test 1).
    fresh_config_dir_with(&pipe, None).expect("fresh config dir");
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let t1 = unique_title("Inside-A");
    let t2 = unique_title("Inside-B");

    let w1 = TestWindow::create(&t1).expect("create W1");
    std::thread::sleep(Duration::from_millis(400));
    let w2 = TestWindow::create(&t2).expect("create W2");
    std::thread::sleep(HOOK_SETTLE);

    wait_tiled_while_pumping(&pipe, 2).expect("two windows tiled");
    let layout = query_actual_while_pumping(&pipe).expect("query layout actual");
    let r1 = actual_rect_of(&layout, w1.hwnd).expect("W1 in actual layout");

    // Park the pointer at an off-center position INSIDE W1 (the warp target),
    // so a buggy warp-to-center would visibly move it.
    let parked = (r1.0 + r1.2 / 4, r1.1 + r1.3 / 4);
    set_cursor_pos(parked.0, parked.1);
    let before = cursor_pos();
    assert_eq!(
        before, parked,
        "sanity: pointer parked off-center inside W1"
    );

    let resp = dispatch_while_pumping(&pipe, SocketMessage::FocusLeft).expect("send FocusLeft");
    assert!(
        matches!(resp, SocketResponse::Ok),
        "FocusLeft should succeed: {resp:?}"
    );
    std::thread::sleep(WARP_SETTLE);

    let after = cursor_pos();
    assert_eq!(
        after, before,
        "pointer inside the newly focused window must stay byte-identical"
    );
    assert_inside(after, r1, "still inside W1");

    drop(w1);
    drop(w2);
    drop(td);
}

/// `warp_on_focus = false` → no warp on any focus change.
#[test]
fn warp_disabled_leaves_pointer_untouched() {
    let _input_guard = lock_input_desktop();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();

    // Seed the per-test config dir's flow.toml with warp disabled BEFORE the
    // daemon starts, so the whole session runs with the knob off.
    fresh_config_dir_with(&pipe, Some("[cursor]\nwarp_on_focus = false\n"))
        .expect("seed warp-off config");

    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let t1 = unique_title("NoWarp-A");
    let t2 = unique_title("NoWarp-B");

    let w1 = TestWindow::create(&t1).expect("create W1");
    std::thread::sleep(Duration::from_millis(400));
    let w2 = TestWindow::create(&t2).expect("create W2");
    std::thread::sleep(HOOK_SETTLE);

    wait_tiled_while_pumping(&pipe, 2).expect("two windows tiled");
    let layout = query_actual_while_pumping(&pipe).expect("query layout actual");
    let r1 = actual_rect_of(&layout, w1.hwnd).expect("W1 in actual layout");

    // Park outside W1; a focus dispatch must NOT move the pointer.
    set_cursor_pos(4, r1.1 + r1.3 / 4);
    let before = cursor_pos();

    let resp = dispatch_while_pumping(&pipe, SocketMessage::FocusLeft).expect("send FocusLeft");
    assert!(
        matches!(resp, SocketResponse::Ok),
        "FocusLeft should succeed: {resp:?}"
    );
    std::thread::sleep(WARP_SETTLE);

    let after = cursor_pos();
    assert_eq!(
        after, before,
        "warp_on_focus = false must leave the pointer byte-identical"
    );

    drop(w1);
    drop(w2);
    drop(td);
}

/// reload-config picks up a `warp_on_focus` flip without a daemon restart.
#[test]
fn reload_config_flips_warp_without_restart() {
    let _input_guard = lock_input_desktop();
    let mut td = TestDesktop::create().expect("test desktop");
    td.make_input().expect("promote test desktop to input");
    let pipe = unique_pipe_name();

    let config_dir = fresh_config_dir_with(&pipe, Some("[cursor]\nwarp_on_focus = false\n"))
        .expect("seed warp-off config");
    let flow_toml = config_dir.join("flow.toml");

    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let t1 = unique_title("Reload-A");
    let t2 = unique_title("Reload-B");

    let w1 = TestWindow::create(&t1).expect("create W1");
    std::thread::sleep(Duration::from_millis(400));
    let w2 = TestWindow::create(&t2).expect("create W2");
    std::thread::sleep(HOOK_SETTLE);

    wait_tiled_while_pumping(&pipe, 2).expect("two windows tiled");
    let layout = query_actual_while_pumping(&pipe).expect("query layout actual");
    let r1 = actual_rect_of(&layout, w1.hwnd).expect("W1 in actual layout");
    let r2 = actual_rect_of(&layout, w2.hwnd).expect("W2 in actual layout");

    // Phase 1: warp off — dispatch must not move the pointer.
    set_cursor_pos(4, r1.1 + r1.3 / 4);
    let before = cursor_pos();
    let resp = dispatch_while_pumping(&pipe, SocketMessage::FocusLeft).expect("FocusLeft");
    assert!(matches!(resp, SocketResponse::Ok));
    std::thread::sleep(WARP_SETTLE);
    assert_eq!(
        cursor_pos(),
        before,
        "phase 1 (warp off via flow.toml): pointer must not move"
    );

    // Flip the knob on disk and hot-reload.
    std::fs::write(&flow_toml, "[cursor]\nwarp_on_focus = true\n").expect("rewrite flow.toml");
    let resp = dispatch_while_pumping(&pipe, SocketMessage::ReloadConfig).expect("ReloadConfig");
    assert!(
        matches!(resp, SocketResponse::Ok),
        "ReloadConfig should succeed: {resp:?}"
    );
    std::thread::sleep(Duration::from_millis(300));

    // Phase 2: warp on — dispatching focus back to W2 (right) must warp.
    set_cursor_pos(4, r1.1 + r1.3 / 4);
    let resp = dispatch_while_pumping(&pipe, SocketMessage::FocusRight).expect("FocusRight");
    assert!(matches!(resp, SocketResponse::Ok));
    std::thread::sleep(WARP_SETTLE);

    let after = cursor_pos();
    let expected = (r2.0 + r2.2 / 2, r2.1 + r2.3 / 2);
    assert_eq!(
        after, expected,
        "phase 2 (warp on after reload): pointer should warp to W2 center {expected:?}"
    );

    drop(w1);
    drop(w2);
    drop(td);
}
