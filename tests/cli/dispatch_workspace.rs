//! Integration tests for `stm dispatch switch-workspace` and `stm dispatch
//! move-to-workspace`.
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
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

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

/// Read a window's physical screen rect via Win32 `GetWindowRect`.
///
/// Returns `(left, top, right, bottom)` in screen coordinates. The IPC layout
/// queries ([`query_layout_virtual`]) report *logical* positions for the
/// **active** workspace only — they cannot observe a parked workspace's actual
/// on-screen rect, which is exactly what the rapid-switch stranding bug
/// freezes mid-slide. This helper is the only way to inspect that physical
/// rect from the test process.
fn win32_window_rect(hwnd: HWND) -> (i32, i32, i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `GetWindowRect` only reads window geometry into the supplied
    // `&mut RECT`; the pointer is a valid local. Failure (e.g. invalid HWND)
    // returns `Err` — surfaced as a panic because every assertion in this
    // file hinges on a successful read of a live test window.
    unsafe { GetWindowRect(hwnd, &mut rect) }
        .expect("GetWindowRect must succeed for a live test window");
    (rect.left, rect.top, rect.right, rect.bottom)
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
        .args(["dispatch", "move-to-workspace", "99"])
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

// ── Tests: rapid switch-workspace stranding regression ───────────────
//
// The tests below guard the fix for the user's bug report:
//
// > When in workspace 1, trigger `switch-workspace 2` and during the
// > animation trigger `switch-workspace 3`: the window in workspace 1
// > stops the animation and remains partially visible in the work area.
//
// Root cause: `switch_workspace_layout` in `src/daemon/dispatch.rs` used to
// partition workspaces into animate / teleport / **skip** buckets. Same-side
// bystanders fell into the skip bucket under the (false) assumption "same
// parked side ⇒ already at rest." That assumption breaks when the bystander
// is still mid-flight from the immediately preceding switch — the second
// `animate_workspaces` call submits a batch that omits the bystander, and
// `animator.rs::start_batch` replaces the active batch, silently dropping
// the bystander's in-flight tween. The window is frozen at its last
// interpolated y.
//
// The fix drops the skip bucket: every non-empty workspace is submitted to
// `animate_workspaces`. `start_batch` reads each window's current position
// (interpolated when mid-flight, `GetWindowRect` otherwise) and `build_tweens`
// drops windows already at their target — so the same-side bystander is
// retargeted smoothly to its parked slot instead of being stranded.
//
// # Why this is a CLI integration test
//
// The bug requires (a) animation ENABLED (the test daemon runs with the
// compiled default — `AnimationConfig { enabled: true, duration_ms: 240 }`)
// so the second switch can land mid-flight, and (b) the dispatcher's
// partition loop running against real workspace state. The animator's
// `MockBackend` is `pub(crate)` so it cannot be driven from `tests/`; the
// CLI harness exercises the dispatcher + animator end-to-end against a real
// `stmd` process. The physical window rect is then read back via Win32
// `GetWindowRect` (the IPC layout queries report *logical* rects for the
// active workspace only and cannot see a parked workspace's frozen rect).

/// Window A's parked top-edge must match the empirically captured value
/// within this many pixels. The animator's last `apply_batch` may land a few
/// pixels short of the exact target due to interpolation rounding (the
/// completion gate fires at the first frame where `t >= 1.0`, which can be
/// the frame *after* `t == 1.0` was applied), so a small tolerance keeps the
/// test deterministic without weakening the regression signal.
const PARKED_TOLERANCE_PX: i32 = 20;

/// Delay between the two rapid switches — short enough that the second
/// `SwitchWorkspace` lands while the first 240 ms animation is still in
/// flight (the bug condition), long enough that the daemon has actually
/// received and dispatched the first command. 50 ms sits comfortably inside
/// the ~240 ms animation window with margin on both sides.
const RAPID_SWITCH_GAP: Duration = Duration::from_millis(50);

/// Delay after the second rapid switch, before reading the final window
/// rect. Must exceed the 240 ms animation duration plus IPC + worker-tick
/// latency so the retargeted animation has fully settled.
const POST_SWITCH_SETTLE: Duration = Duration::from_millis(700);

/// Regression test (positive): rapid `switch-workspace 1→2→3` with the
/// second switch landing mid-flight must NOT strand ws1's window.
///
/// Reproduces the exact user scenario:
///
/// 1. Window A on workspace 1 (active).
/// 2. Slow switch 1→2 captures A's **parked** rect — the ground-truth
///    position the rapid switch must converge to. Capturing it empirically
///    keeps the assertion robust to any local config (monitor size, padding,
///    window_gap).
/// 3. Set up window C on ws3 so the rapid `switch-workspace 2→3` actually
///    submits a real animation batch. Without a real batch the second
///    `animate_workspaces` returns early (`batches.is_empty()`) and the
///    first animation completes normally — no bug to reproduce.
/// 4. Switch back to ws1 so A is on screen.
/// 5. **Bug scenario**: rapid 1→2, wait `RAPID_SWITCH_GAP`, rapid 2→3.
/// 6. After settle, assert A's top edge matches the captured parked top
///    within `PARKED_TOLERANCE_PX`.
///
/// Pre-fix, A's tween from the 1→2 animation is dropped when 2→3 submits
/// its participant-only batch; A is frozen at its mid-flight interpolated y
/// (tens to hundreds of pixels off parked, depending on the exact
/// interruption timing and easing curve). Post-fix, A is re-submitted with
/// target `-y_unit`; `start_batch` reads its interpolated position and
/// retargets it smoothly to parked.
#[test]
fn rapid_switch_workspace_does_not_strand_bystander() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    // Arrange: window A on ws1 (the default active workspace).
    let title_a = unique_title("StrandA");
    let w_a = TestWindow::create(&title_a).expect("create window A");
    std::thread::sleep(HOOK_SETTLE);

    // Arrange: empirically capture A's PARKED top by slowly switching to ws2.
    // Switching 1→2 parks ws1 one `y_unit` above the active workspace, so A
    // slides from y=0 to y=-y_unit. After full settle this is the ground-
    // truth parked position the rapid switch must converge to. Using the
    // empirical value avoids hardcoding monitor_height / window_gap.
    let resp = send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 2 })
        .expect("IPC: slow switch 1→2");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "slow switch 1→2 must succeed so we can capture A's parked rect",
    );
    std::thread::sleep(WORKSPACE_SETTLE);
    let parked_rect = win32_window_rect(w_a.hwnd);
    // Pre-condition: A really did slide fully off-screen above. If this fails
    // the test machine has animation disabled locally (the bug is not
    // reproducible in that mode) — re-enable animation to run this test.
    assert!(
        parked_rect.3 <= 0,
        "pre-condition: A's bottom edge must be at or above the screen top after \
         slow parking, got {:?}; if bottom > 0 the test daemon is likely running \
         with animation disabled (the stranding bug requires animation enabled)",
        parked_rect,
    );

    // Arrange: switch back to ws1 so A is on screen again.
    let resp = send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 1 })
        .expect("IPC: slow switch 2→1");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "slow switch 2→1 must succeed so A returns on screen",
    );
    std::thread::sleep(WORKSPACE_SETTLE);

    // Arrange: populate ws3 with window C. This is required so the rapid
    // `switch-workspace 2→3` submits a real animation batch (ws3 as the
    // participant / destination). Without ws3 being non-empty, the second
    // switch would have no participant and `animate_workspaces` would return
    // early — never invoking `start_batch`, so the first animation would
    // simply complete and no stranding could ever occur.
    let resp = send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 3 })
        .expect("IPC: slow switch 1→3 (to populate ws3)");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "slow switch 1→3 must succeed so window C can be created on ws3",
    );
    std::thread::sleep(WORKSPACE_SETTLE);
    let title_c = unique_title("StrandC");
    let _w_c = TestWindow::create(&title_c).expect("create window C on ws3");
    std::thread::sleep(HOOK_SETTLE);
    let resp = send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 1 })
        .expect("IPC: slow switch 3→1");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "slow switch 3→1 must succeed so A is back on screen for the bug scenario",
    );
    std::thread::sleep(WORKSPACE_SETTLE);

    // Pre-condition: A is fully on screen again at ws1.
    let onscreen_rect = win32_window_rect(w_a.hwnd);
    assert!(
        onscreen_rect.3 > 0,
        "pre-condition: A must be fully on screen at ws1 before the rapid \
         switch, got {onscreen_rect:?}",
    );

    // === ACT: the bug scenario ===
    // Switch 1→2 starts A's animation (0 → -y_unit).
    let resp = send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 2 })
        .expect("IPC: rapid switch 1→2");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "rapid switch 1→2 must succeed (it kicks off A's parking animation)",
    );
    // Wait inside the 240 ms animation window so the next switch lands
    // mid-flight. See RAPID_SWITCH_GAP doc-comment for the timing rationale.
    std::thread::sleep(RAPID_SWITCH_GAP);
    // Switch 2→3 (mid-flight): pre-fix this drops A's in-flight tween.
    let resp = send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 3 })
        .expect("IPC: rapid switch 2→3");
    assert_eq!(
        resp,
        SocketResponse::Ok,
        "rapid switch 2→3 must succeed (it interrupts A's animation mid-flight)",
    );

    // Let the retargeted animation fully settle.
    std::thread::sleep(POST_SWITCH_SETTLE);

    // === ASSERT ===
    // A must have landed at its parked offset (matches the captured
    // `parked_rect` within tolerance). Pre-fix, A is frozen at its mid-flight
    // interpolated y — easily outside tolerance.
    let after_rect = win32_window_rect(w_a.hwnd);
    let top_diff = (after_rect.1 - parked_rect.1).abs();
    assert!(
        top_diff <= PARKED_TOLERANCE_PX,
        "regression: ws1's window is stranded after rapid 1→2→3 switch. \
         Expected parked top={parked_top} (±{tol}px); got top={after_top} \
         (off by {top_diff}px). Pre-fix this reproduces the user's \
         'window remains partially visible in the work area' bug. \
         after_rect={after_rect:?}, parked_rect={parked_rect:?}",
        parked_top = parked_rect.1,
        after_top = after_rect.1,
        tol = PARKED_TOLERANCE_PX,
    );
}

/// Negative control: the SAME 1→2→3 sequence with each switch allowed to
/// fully settle (no mid-flight interruption) must also leave A parked
/// correctly.
///
/// This test passes both pre-fix and post-fix — there is no rapid interrupt,
/// so the skip-bucket bug never triggers. Its job is to:
/// - confirm the test setup is sound (a passing baseline that isolates the
///   rapid-timing as the only difference from the regression test above),
/// - guard against future regressions that would strand A even in the
///   slow-path (which would be a different, more severe bug).
#[test]
fn slow_switch_workspace_settles_bystander() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    // Arrange: window A on ws1.
    let title_a = unique_title("SlowStrandA");
    let w_a = TestWindow::create(&title_a).expect("create window A");
    std::thread::sleep(HOOK_SETTLE);

    // Arrange: capture parked rect empirically (slow switch 1→2).
    send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 2 })
        .expect("IPC: slow switch 1→2 (capture parked rect)");
    std::thread::sleep(WORKSPACE_SETTLE);
    let parked_rect = win32_window_rect(w_a.hwnd);

    // Arrange: switch back to ws1.
    send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 1 })
        .expect("IPC: slow switch 2→1");
    std::thread::sleep(WORKSPACE_SETTLE);

    // Arrange: populate ws3 so the eventual 2→3 has a participant.
    send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 3 })
        .expect("IPC: slow switch 1→3");
    std::thread::sleep(WORKSPACE_SETTLE);
    let title_c = unique_title("SlowStrandC");
    let _w_c = TestWindow::create(&title_c).expect("create window C on ws3");
    std::thread::sleep(HOOK_SETTLE);
    send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 1 })
        .expect("IPC: slow switch 3→1");
    std::thread::sleep(WORKSPACE_SETTLE);

    // ACT: SLOW 1→2→3 — each switch fully settles before the next.
    send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 2 })
        .expect("IPC: slow switch 1→2");
    std::thread::sleep(WORKSPACE_SETTLE);
    send_ipc_retry(&pipe, &SocketMessage::SwitchWorkspace { workspace_id: 3 })
        .expect("IPC: slow switch 2→3");
    std::thread::sleep(WORKSPACE_SETTLE);

    // ASSERT: A is at its parked offset (within tolerance).
    let after_rect = win32_window_rect(w_a.hwnd);
    let top_diff = (after_rect.1 - parked_rect.1).abs();
    assert!(
        top_diff <= PARKED_TOLERANCE_PX,
        "slow 1→2→3 must park A correctly even without mid-flight interrupt. \
         Expected parked top={parked_top} (±{tol}px); got top={after_top} \
         (off by {top_diff}px). after_rect={after_rect:?}, \
         parked_rect={parked_rect:?}",
        parked_top = parked_rect.1,
        after_top = after_rect.1,
        tol = PARKED_TOLERANCE_PX,
    );
}
