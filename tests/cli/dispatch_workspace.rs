//! Integration tests for `stm dispatch switchworkspace` and `stm dispatch
//! movetoworkspace`.
//!
//! These tests cover two related bug fixes in cross-workspace window moves:
//!
//! - **Move-to-self is a no-op** (`dispatch_move_window_to_workspace` short-
//!   circuits when `dest_id == active_id`): the daemon returns `Ok` with no
//!   layout mutation and no active-workspace change.
//! - **Camera follows the moved window** (`dispatch_move_window_to_workspace`
//!   calls `switch_active_workspace` after the move): after moving a window
//!   to workspace *m*, the active workspace switches to *m* so the moved
//!   window is visible.
//!
//! # How the active workspace is observed
//!
//! None of the IPC queries (`QueryLayoutVirtual`, `QueryLayoutActual`,
//! `QueryWindowsAll`) report the active workspace id directly — each operates
//! on `self.active_scrolling()` only. We therefore observe the active
//! workspace **indirectly**: since [`query_layout_virtual`] always reflects
//! the active workspace's layout, after a camera-following move it must show
//! the destination workspace's layout (containing the moved window). To
//! inspect a *non-active* workspace, we explicitly `SwitchWorkspace` to it
//! and then query again.
//!
//! # IPC delivery
//!
//! The daemon's named-pipe server services one client at a time on a
//! background accept thread. Between a client disconnect and the next
//! `ConnectNamedPipe` there is a brief window where new connections are
//! refused. A single [`send_message_to`](scrolling_tiling_manager::ipc::transport::send_message_to)
//! that lands in that window returns `ConnectionRefused`, and the test
//! harness's [`send_ipc_ignore`] helper silently drops that error — which
//! loses the message. That is fatal for commands issued back-to-back with a
//! prior IPC round trip (e.g. `SwitchWorkspace` sent right after a query).
//! We therefore send every command under test through [`send_ipc_retry`],
//! which retries through the refusal window and also surfaces the
//! [`SocketResponse`] so success/no-op cases can be asserted directly.
//!
//! # Desktop isolation
//!
//! Each test creates a [`TestDesktop`] and spawns `stmd` on it via
//! [`start_test_daemon`], so the user's real desktop is never touched. Unique
//! pipe names provide parallel isolation between tests.

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message. See the same pattern in `dispatch_swap.rs`.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use scrolling_tiling_manager::ipc::message::{SocketMessage, SocketResponse};
use scrolling_tiling_manager::ipc::transport;

use super::common::unique_pipe_name;
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, query_layout_virtual, start_test_daemon, unique_title,
};

/// Delay after creating windows to let hooks fire and the daemon tile them.
///
/// Matches the value used by `dispatch_swap.rs`.
const HOOK_SETTLE: Duration = Duration::from_millis(1500);

/// Delay after a workspace dispatch to let the layout state propagate.
///
/// The virtual-layout state mutation happens synchronously inside the
/// dispatcher, so the next query reflects the new state once the IPC round
/// trip completes; the padding mirrors `SWAP_SETTLE` in `dispatch_swap.rs`.
const WORKSPACE_SETTLE: Duration = Duration::from_millis(500);

// ── IPC helper ──────────────────────────────────────────────────────

/// Send an IPC command, retrying through transient connection refusals.
///
/// The daemon's named-pipe server accepts one client at a time. Between a
/// client disconnection and the next background `ConnectNamedPipe` there is a
/// brief window where new connections are refused with `ConnectionRefused`.
/// A single [`transport::send_message_to`] call that lands in that window
/// fails — and the test harness's `send_ipc_ignore` helper silently drops
/// that error, losing the message. Retrying through the window (≈500 ms
/// budget) reliably delivers back-to-back commands.
///
/// Returns the final [`SocketResponse`] so callers can assert `Ok` directly
/// for no-op / success cases.
fn send_ipc_retry(pipe: &str, msg: &SocketMessage) -> Result<SocketResponse, String> {
    const ATTEMPTS: u32 = 20;
    const SLEEP: Duration = Duration::from_millis(25);

    let mut last_err = String::new();
    for _ in 0..ATTEMPTS {
        match transport::send_message_to(pipe, msg) {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                last_err = format!("{e}");
                std::thread::sleep(SLEEP);
            }
        }
    }
    Err(format!(
        "IPC send failed after {ATTEMPTS} attempts ({} ms total): {last_err}",
        ATTEMPTS * 25
    ))
}

// ── Layout inspection helpers ───────────────────────────────────────

/// Collect every window-id integer currently in the active workspace's
/// virtual layout, in (column, row) order.
///
/// Used to check whether a specific hwnd is present on whatever workspace is
/// currently active.
fn active_window_ids(json: &serde_json::Value) -> Vec<i64> {
    json["columns"]
        .as_array()
        .map(|cols| {
            cols.iter()
                .flat_map(|col| {
                    col["rows"]
                        .as_array()
                        .map(|rows| rows.iter().filter_map(|r| r.as_i64()).collect::<Vec<_>>())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Return the `window_count` field of a `query_layout_virtual` payload.
fn active_window_count(json: &serde_json::Value) -> i64 {
    json["window_count"].as_i64().unwrap_or(0)
}

/// Cast a window handle to the same integer form used by the layout JSON
/// (where `WindowId(isize)` is serialized as a JSON number and read back via
/// [`serde_json::Value::as_i64`]).
fn hwnd_id(hwnd: windows::Win32::Foundation::HWND) -> i64 {
    hwnd.0 as i64
}

// ── Tests: MoveWindowToWorkspace ────────────────────────────────────

/// Bug 1 (positive): moving the focused window to the *currently active*
/// workspace is a no-op — the daemon returns `Ok`, the layout (column
/// structure, window positions, window count) is byte-for-byte unchanged
/// afterward, and the active workspace stays put.
///
/// Without the fix, the daemon would remove the window from the workspace
/// and re-insert it (a pointless mutation that fired an animation and could
/// reshuffle column focus).
///
/// The layout-unchanged assertion is meaningful only because [`send_ipc_retry`]
/// guarantees the message was delivered — otherwise an empty `before == after`
/// could just mean the IPC was lost. The explicit `Ok` response assertion
/// adds a second, independent confirmation of the no-op.
#[test]
fn move_to_active_workspace_is_noop() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title = unique_title("MoveToSelf");
    let _w = TestWindow::create(&title).expect("create window");
    std::thread::sleep(HOOK_SETTLE);

    // Snapshot the active workspace's layout before the command.
    let before = query_layout_virtual(&pipe).expect("query layout before");
    assert_eq!(
        active_window_count(&before),
        1,
        "expected exactly 1 window on workspace 1 before move, got {before:?}",
    );
    let ids_before = active_window_ids(&before);
    let moved_id = ids_before[0];

    // Act: move focused window to workspace 1 (which is already active).
    let resp = send_ipc_retry(
        &pipe,
        &SocketMessage::MoveWindowToWorkspace { workspace_id: 1 },
    )
    .expect("IPC send: move-to-self");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "move-to-self must succeed without mutating any state",
    );

    std::thread::sleep(WORKSPACE_SETTLE);

    // Assert: layout is unchanged. The query reflects the *active* workspace,
    // so an unchanged layout (still containing the window) also implies the
    // active workspace is still 1 — if the camera had moved to a different
    // (empty) workspace, the columns array would be empty instead.
    let after = query_layout_virtual(&pipe).expect("query layout after");
    assert_eq!(
        before["columns"], after["columns"],
        "move-to-self must not mutate the layout",
    );
    assert_eq!(
        active_window_count(&after),
        1,
        "move-to-self must not change the window count, got {after:?}",
    );
    assert_eq!(
        active_window_ids(&after),
        vec![moved_id],
        "move-to-self must keep the same window id in place",
    );
}

/// Bug 2 (positive): moving the focused window to a *different* workspace
/// switches the active workspace to the destination so the moved window is
/// visible (the camera follows).
///
/// Asserts three things:
/// 1. The dispatcher returns `Ok`.
/// 2. The destination workspace is now active (its layout is returned by
///    `query_layout_virtual`, and contains the moved window).
/// 3. The source workspace no longer contains the moved window (verified by
///    switching back to it and querying — it must be empty).
#[test]
fn move_to_other_workspace_switches_active() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title = unique_title("MoveToOther");
    let w = TestWindow::create(&title).expect("create window");
    let moved_id = hwnd_id(w.hwnd);
    std::thread::sleep(HOOK_SETTLE);

    // Pre-condition: the window landed on workspace 1 (the default active).
    let initial = query_layout_virtual(&pipe).expect("query layout initial");
    assert_eq!(
        active_window_count(&initial),
        1,
        "expected exactly 1 window on workspace 1 initially, got {initial:?}",
    );
    assert!(
        active_window_ids(&initial).contains(&moved_id),
        "expected the created window ({moved_id}) on the initial active workspace, got {:?}",
        active_window_ids(&initial),
    );

    // Act: move the focused window to workspace 2.
    let resp = send_ipc_retry(
        &pipe,
        &SocketMessage::MoveWindowToWorkspace { workspace_id: 2 },
    )
    .expect("IPC send: move to ws 2");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "move to a valid workspace must succeed",
    );
    std::thread::sleep(WORKSPACE_SETTLE);

    // Assert (camera followed): query_layout_virtual now reflects workspace 2
    // and the moved window is on it. Since workspaces 2..=10 start empty,
    // the only way the query shows this window is if the active workspace
    // switched to 2.
    let after = query_layout_virtual(&pipe).expect("query layout after move");
    assert_eq!(
        active_window_count(&after),
        1,
        "expected exactly 1 window on the destination workspace, got {after:?}",
    );
    assert!(
        active_window_ids(&after).contains(&moved_id),
        "expected the moved window ({moved_id}) on the now-active workspace 2, got {:?}",
        active_window_ids(&after),
    );

    // Assert (source loses the window): the source workspace (1) no longer
    // contains the moved window. Switch back and query — workspace 1 must be
    // empty.
    let resp = send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 1 })
        .expect("IPC send: switch back to ws 1");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "switch to a valid workspace must succeed",
    );
    std::thread::sleep(WORKSPACE_SETTLE);
    let ws1 = query_layout_virtual(&pipe).expect("query workspace 1 after move");
    assert_eq!(
        active_window_count(&ws1),
        0,
        "source workspace 1 must be empty after the move, got {ws1:?}",
    );
    assert!(
        !active_window_ids(&ws1).contains(&moved_id),
        "source workspace 1 must not contain the moved window",
    );
}

/// Bug 2 (broader coverage): moving to a higher-numbered workspace (5) also
/// switches the active workspace there. Workspaces 2..=10 start empty, so
/// workspace 5 should hold exactly the moved window afterward and workspace
/// 1 should be empty.
#[test]
fn move_to_higher_workspace_switches_active() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title = unique_title("MoveToFive");
    let w = TestWindow::create(&title).expect("create window");
    let moved_id = hwnd_id(w.hwnd);
    std::thread::sleep(HOOK_SETTLE);

    // Act: move focused window to workspace 5.
    let resp = send_ipc_retry(
        &pipe,
        &SocketMessage::MoveWindowToWorkspace { workspace_id: 5 },
    )
    .expect("IPC send: move to ws 5");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "move to a valid workspace must succeed",
    );
    std::thread::sleep(WORKSPACE_SETTLE);

    // Assert: query reflects workspace 5 (camera followed) and contains the
    // moved window.
    let after = query_layout_virtual(&pipe).expect("query layout after move to 5");
    assert_eq!(
        active_window_count(&after),
        1,
        "expected exactly 1 window on workspace 5 after the move, got {after:?}",
    );
    assert!(
        active_window_ids(&after).contains(&moved_id),
        "expected the moved window ({moved_id}) on workspace 5, got {:?}",
        active_window_ids(&after),
    );

    // Sanity: switching back to 1 shows the source is empty.
    send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 1 })
        .expect("IPC send: switch back to ws 1");
    std::thread::sleep(WORKSPACE_SETTLE);
    let ws1 = query_layout_virtual(&pipe).expect("query workspace 1 after move to 5");
    assert_eq!(
        active_window_count(&ws1),
        0,
        "source workspace 1 must be empty after moving to workspace 5, got {ws1:?}",
    );
}

/// Bug 1 (negative edge): moving to a workspace id that does not exist on
/// the active monitor must surface an error response and leave the layout
/// unchanged.
///
/// This goes through the CLI surface so the error is observable as a
/// non-zero exit code (matching the `dispatch_swap.rs` edge-case pattern).
#[test]
fn move_to_unknown_workspace_returns_error() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title = unique_title("MoveUnknown");
    let _w = TestWindow::create(&title).expect("create window");
    std::thread::sleep(HOOK_SETTLE);

    let before = query_layout_virtual(&pipe).expect("query layout before");
    assert_eq!(
        active_window_count(&before),
        1,
        "expected exactly 1 window on workspace 1 before move, got {before:?}",
    );

    // Act: workspace 99 does not exist (the daemon creates 1..=10). The CLI
    // must report a failure.
    super::common::stm(&pipe)
        .args(["dispatch", "movetoworkspace", "99"])
        .assert()
        .failure();

    // Assert: the bogus destination did not mutate any state.
    std::thread::sleep(WORKSPACE_SETTLE);
    let after = query_layout_virtual(&pipe).expect("query layout after");
    assert_eq!(
        before["columns"], after["columns"],
        "move to an unknown workspace must not mutate the layout",
    );
    assert_eq!(
        active_window_count(&after),
        1,
        "move to an unknown workspace must not change the window count, got {after:?}",
    );
}

// ── Tests: SwitchWorkspace (regression guard for the extracted helper) ──

/// `dispatch_switch_workspace` was refactored to share the new
/// `switch_active_workspace` helper with `dispatch_move_window_to_workspace`.
/// This test guards that the plain switch path still no-ops on self: a
/// self-switch must return `Ok` and leave the layout completely unchanged.
#[test]
fn switch_to_self_is_noop() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title = unique_title("SwitchSelf");
    let _w = TestWindow::create(&title).expect("create window");
    std::thread::sleep(HOOK_SETTLE);

    let before = query_layout_virtual(&pipe).expect("query layout before");
    assert_eq!(
        active_window_count(&before),
        1,
        "expected exactly 1 window on workspace 1 before switch, got {before:?}",
    );

    // Act: switch to workspace 1 (already active) — must be a no-op.
    let resp = send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 1 })
        .expect("IPC send: switch-to-self");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "switch-to-self must succeed without mutating any state",
    );
    std::thread::sleep(WORKSPACE_SETTLE);

    let after = query_layout_virtual(&pipe).expect("query layout after");
    assert_eq!(
        before["columns"], after["columns"],
        "switch-to-self must not mutate the layout",
    );
    assert_eq!(
        active_window_count(&after),
        1,
        "switch-to-self must not change the window count, got {after:?}",
    );
}
