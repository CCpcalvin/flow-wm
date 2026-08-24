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

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message. See the same pattern in `dispatch_workspace.rs`.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use flow_wm::ipc::message::{SocketMessage, SocketResponse};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, MSG, PM_NOREMOVE, PM_QS_PAINT, PeekMessageW, SetCursorPos, TranslateMessage,
};

use super::common::unique_pipe_name;
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, send_ipc_retry, start_test_daemon, test_config_dir,
    unique_title, wait_until_windows_tiled,
};

/// Delay for hook/foreground settle after window creation.
///
/// Mirrors `HOOK_SETTLE` in `dispatch_workspace.rs` — long enough for the
/// isolated desktop's spurious-event backlog to drain and the FOREGROUND
/// hook (plus any resulting warp) to run.
const HOOK_SETTLE: Duration = Duration::from_millis(1500);

/// Delay after a focus dispatch before reading the pointer.
///
/// The dispatch handler runs synchronously in the daemon, but the *warp*
/// fires from `on_focus_changed` consuming the `EVENT_SYSTEM_FOREGROUND`
/// the dispatch induced — an async hop through the hook channel. 500 ms is
/// comfortably above that while keeping the test fast.
const WARP_SETTLE: Duration = Duration::from_millis(500);

/// Read the pointer position (screen coordinates).
fn cursor_pos() -> (i32, i32) {
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: GetCursorPos writes into a valid local POINT. It cannot fail
    // for a thread on the input desktop; a panic here means the test
    // harness itself is broken.
    unsafe { GetCursorPos(&mut point) }.expect("GetCursorPos must succeed on the test desktop");
    (point.x, point.y)
}

/// Park the pointer at an explicit screen position.
fn set_cursor_pos(x: i32, y: i32) {
    // SAFETY: two scalar coordinates; failure would only mean the position
    // was rejected, which the subsequent read-back assertions catch.
    let _ = unsafe { SetCursorPos(x, y) };
}

/// Drain pending paint messages on the calling (GUI) thread.
///
/// The test windows live on this thread, so it must pump paint messages or
/// the daemon's synchronous activation sends to them stall. Paint-only
/// (`PM_QS_PAINT` + `PM_NOREMOVE`-checked retrieval loop) — input and
/// posted messages are left for the system, matching the minimum a
/// foreground app must do to stay responsive.
fn pump_paint_messages() {
    let mut msg = MSG::default();
    // SAFETY: PeekMessageW inspects the thread's queue and fills `msg`;
    // TranslateMessage/DispatchMessage for paint-only messages go to
    // DefWindowProc (via test_wnd_proc), which just validates.
    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE | PM_QS_PAINT).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
        }
    }
}

/// Run `f` (an IPC send) on a helper thread while pumping paint messages on
/// the calling thread, returning `f`'s result.
///
/// See the module docs: the test thread owns the test windows, so it must
/// stay responsive to the daemon's synchronous activation sends while the
/// pipe round trip is in flight.
fn ipc_while_pumping<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let handle = std::thread::spawn(f);
    loop {
        match handle.is_finished() {
            true => {
                return handle
                    .join()
                    .map_err(|_| "IPC thread panicked".to_string())?;
            }
            false => pump_paint_messages(),
        }
    }
}

/// Send an IPC message (retrying through pipe refusals) while pumping.
fn dispatch(pipe: &str, msg: SocketMessage) -> Result<SocketResponse, String> {
    let pipe = pipe.to_owned();
    ipc_while_pumping(move || send_ipc_retry(&pipe, &msg))
}

/// Query the actual layout (retrying) while pumping.
fn query_actual(pipe: &str) -> Result<serde_json::Value, String> {
    let pipe = pipe.to_owned();
    ipc_while_pumping(
        move || match send_ipc_retry(&pipe, &SocketMessage::QueryLayoutActual)? {
            SocketResponse::Data { payload } => Ok(payload),
            SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
            other => Err(format!("unexpected response: {other:?}")),
        },
    )
}

/// Wait until `expected` windows are tiled (polling while pumping).
fn wait_tiled(pipe: &str, expected: usize) -> Result<serde_json::Value, String> {
    let pipe = pipe.to_owned();
    ipc_while_pumping(move || wait_until_windows_tiled(&pipe, expected))
}

/// Serializes the cursor-warp tests.
///
/// [`TestDesktop::make_input`] promotes a *session-global* input desktop —
/// only one test desktop can be the input desktop at a time, and the
/// session cursor it exposes is shared. Running these tests in parallel
/// would have each test stealing input-desktop status (and warping the
/// pointer) out from under the others. The guard serializes exactly this
/// module's tests while leaving the rest of the suite parallel.
static INPUT_DESKTOP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard acquiring [`INPUT_DESKTOP_LOCK`].
fn lock_input_desktop() -> std::sync::MutexGuard<'static, ()> {
    INPUT_DESKTOP_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Look up the projected (actual-layout) rect of a window by HWND.
fn actual_rect_of(json: &serde_json::Value, hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let hwnd = hwnd.0 as i64;
    json["entries"].as_array()?.iter().find_map(|e| {
        if e["window_id"].as_i64() != Some(hwnd) {
            return None;
        }
        let r = &e["rect"];
        Some((
            r["x"].as_i64()? as i32,
            r["y"].as_i64()? as i32,
            r["width"].as_i64()? as i32,
            r["height"].as_i64()? as i32,
        ))
    })
}

/// Resolve the per-test config directory the daemon will use (shared
/// `test_config_dir` helper — same scheme as `start_test_daemon`) and clear
/// any stale `flow.toml` a previous run left behind, so a cursor test never
/// inherits another test's app config (the temp dir is keyed by pipe id,
/// which repeats across `cargo test` invocations).
fn fresh_config_dir(pipe: &str) -> std::path::PathBuf {
    let dir = test_config_dir(pipe);
    let _ = std::fs::remove_file(dir.join("flow.toml"));
    dir
}

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
    fresh_config_dir(&pipe);
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

    wait_tiled(&pipe, 2).expect("two windows tiled");
    let layout = query_actual(&pipe).expect("query layout actual");
    let r1 = actual_rect_of(&layout, w1.hwnd).expect("W1 in actual layout");
    let r2 = actual_rect_of(&layout, w2.hwnd).expect("W2 in actual layout");

    // Park the pointer in the left screen-edge gap (the tiling projection
    // reserves a `window_gap` margin, default 16px, before the first
    // column's left edge at x=16) — outside W1, outside W2. FocusLeft then
    // moves focus to W1, and the warp must land the pointer at W1's center.
    set_cursor_pos(4, r1.1 + r1.3 / 2);
    let before = cursor_pos();
    assert_ne!(before, (0, 0), "sanity: pointer parked");

    let resp = dispatch(&pipe, SocketMessage::FocusLeft).expect("send FocusLeft");
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
    fresh_config_dir(&pipe);
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let t1 = unique_title("Inside-A");
    let t2 = unique_title("Inside-B");

    let w1 = TestWindow::create(&t1).expect("create W1");
    std::thread::sleep(Duration::from_millis(400));
    let w2 = TestWindow::create(&t2).expect("create W2");
    std::thread::sleep(HOOK_SETTLE);

    wait_tiled(&pipe, 2).expect("two windows tiled");
    let layout = query_actual(&pipe).expect("query layout actual");
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

    let resp = dispatch(&pipe, SocketMessage::FocusLeft).expect("send FocusLeft");
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
    let config_dir = fresh_config_dir(&pipe);
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::write(
        config_dir.join("flow.toml"),
        "[cursor]\nwarp_on_focus = false\n",
    )
    .expect("write flow.toml");

    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let t1 = unique_title("NoWarp-A");
    let t2 = unique_title("NoWarp-B");

    let w1 = TestWindow::create(&t1).expect("create W1");
    std::thread::sleep(Duration::from_millis(400));
    let w2 = TestWindow::create(&t2).expect("create W2");
    std::thread::sleep(HOOK_SETTLE);

    wait_tiled(&pipe, 2).expect("two windows tiled");
    let layout = query_actual(&pipe).expect("query layout actual");
    let r1 = actual_rect_of(&layout, w1.hwnd).expect("W1 in actual layout");

    // Park outside W1; a focus dispatch must NOT move the pointer.
    set_cursor_pos(4, r1.1 + r1.3 / 4);
    let before = cursor_pos();

    let resp = dispatch(&pipe, SocketMessage::FocusLeft).expect("send FocusLeft");
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

    let config_dir = fresh_config_dir(&pipe);
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    let flow_toml = config_dir.join("flow.toml");
    std::fs::write(&flow_toml, "[cursor]\nwarp_on_focus = false\n").expect("write flow.toml");

    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let t1 = unique_title("Reload-A");
    let t2 = unique_title("Reload-B");

    let w1 = TestWindow::create(&t1).expect("create W1");
    std::thread::sleep(Duration::from_millis(400));
    let w2 = TestWindow::create(&t2).expect("create W2");
    std::thread::sleep(HOOK_SETTLE);

    wait_tiled(&pipe, 2).expect("two windows tiled");
    let layout = query_actual(&pipe).expect("query layout actual");
    let r1 = actual_rect_of(&layout, w1.hwnd).expect("W1 in actual layout");
    let r2 = actual_rect_of(&layout, w2.hwnd).expect("W2 in actual layout");

    // Phase 1: warp off — dispatch must not move the pointer.
    set_cursor_pos(4, r1.1 + r1.3 / 4);
    let before = cursor_pos();
    let resp = dispatch(&pipe, SocketMessage::FocusLeft).expect("FocusLeft");
    assert!(matches!(resp, SocketResponse::Ok));
    std::thread::sleep(WARP_SETTLE);
    assert_eq!(
        cursor_pos(),
        before,
        "phase 1 (warp off via flow.toml): pointer must not move"
    );

    // Flip the knob on disk and hot-reload.
    std::fs::write(&flow_toml, "[cursor]\nwarp_on_focus = true\n").expect("rewrite flow.toml");
    let resp = dispatch(&pipe, SocketMessage::ReloadConfig).expect("ReloadConfig");
    assert!(
        matches!(resp, SocketResponse::Ok),
        "ReloadConfig should succeed: {resp:?}"
    );
    std::thread::sleep(Duration::from_millis(300));

    // Phase 2: warp on — dispatching focus back to W2 (right) must warp.
    set_cursor_pos(4, r1.1 + r1.3 / 4);
    let resp = dispatch(&pipe, SocketMessage::FocusRight).expect("FocusRight");
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
