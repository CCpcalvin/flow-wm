//! Integration tests for the window-removal pipeline.
//!
//! These tests exercise the full daemon pipeline: when a window is destroyed
//! or minimised, the daemon's hook fires `on_window_destroyed` /
//! `on_window_minimized`, which calls `ScrollingSpace::remove_window`. That
//! method:
//!
//! 1. Finds the to-be-removed window's position in the virtual layout.
//! 2. If it was the focused window, picks the successor via
//!    `next_available_window` (left neighbour preferred, else right).
//! 3. Removes the window from the virtual layout (natural compression shifts
//!    right-side columns leftward by one slot).
//! 4. Ensures the successor column is visible (`ensure_column_visible`).
//! 5. Projects → diffs → animates.
//!
//! We verify the observable results via IPC queries:
//!
//! - [`query_windows`](super::test_desktop::query_windows) — the `focused`
//!   field, `count`, and each window's `tiled_rect` (projected pixel rect).
//! - [`query_layout_virtual`](super::test_desktop::query_layout_virtual) —
//!   column count and window-ids per column.

use std::time::Duration;

use scrolling_tiling_manager::ipc::message::SocketMessage;

use super::common::unique_pipe_name;
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, query_layout_virtual, query_windows, send_ipc_retry,
    start_test_daemon, unique_title,
};

// ── Helpers ──────────────────────────────────────────────────────────

/// The raw `isize` value of a [`TestWindow`]'s HWND, for JSON matching.
fn hwnd_id(w: &TestWindow) -> i64 {
    w.hwnd.0 as i64
}

/// Send a `FocusLeft` IPC command to move focus one column left.
///
/// Retries through the named-pipe refusal window (see `send_ipc_retry`) so the
/// command is not silently dropped if it lands between a prior client
/// disconnect and the daemon's next `ConnectNamedPipe` — losing it would leave
/// focus on the wrong window and fail the test for a transport reason, not a
/// product reason. The response is ignored: `FocusLeft` at the leftmost column
/// legitimately no-ops, and the caller asserts the resulting focus via a
/// follow-up query.
fn focus_left(pipe: &str) {
    let _ = send_ipc_retry(pipe, &SocketMessage::FocusLeft);
}

/// Collect all window-ids currently in the virtual layout, in column order.
///
/// Each row in the `columns[].rows` array is serialized as an *object*
/// `{"window_id": <id>, "height_px": <px>}` (see `VirtualLayout` /
/// `AppliedLayout` serialization), so the id lives under the `window_id` key —
/// not as a bare integer.
fn layout_window_ids(json: &serde_json::Value) -> Vec<i64> {
    json["columns"]
        .as_array()
        .expect("columns array")
        .iter()
        .flat_map(|col| {
            col["rows"]
                .as_array()
                .expect("rows array")
                .iter()
                .filter_map(|r| r["window_id"].as_i64())
        })
        .collect()
}

/// Find a window's tiled-rect x from the `QueryWindowsAll` JSON.
fn tiled_rect_x(json: &serde_json::Value, id: i64) -> Option<i64> {
    json["windows"]
        .as_array()?
        .iter()
        .find(|w| w["hwnd"].as_i64() == Some(id))?["tiled_rect"]["x"]
        .as_i64()
}

/// Wait 500 ms to let the daemon settle between IPC-heavy phases.
fn settle() {
    std::thread::sleep(Duration::from_millis(500));
}

// ── Tests ────────────────────────────────────────────────────────────

/// Destroying a tiled window removes its column and compacts the layout.
///
/// We create three windows (each becomes its own column), destroy the
/// middle one, and verify:
///
/// - Column count drops from 3 to 2.
/// - The surviving windows remain; the destroyed window is gone.
/// - The rightmost window physically shifts left (its tiled-rect x decreases).
#[test]
fn destroy_removes_column_and_compacts_layout() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    settle();

    let t1 = unique_title("RM-A");
    let t2 = unique_title("RM-B");
    let t3 = unique_title("RM-C");

    let w1 = TestWindow::create(&t1).expect("create w1");
    let w2 = TestWindow::create(&t2).expect("create w2");
    let w3 = TestWindow::create(&t3).expect("create w3");

    // Wait for Created hooks to fire and the daemon to tile them.
    std::thread::sleep(Duration::from_millis(1500));

    // Capture hwnd-ids and w3's x position BEFORE removal.
    let (w1_id, w2_id, w3_id) = (hwnd_id(&w1), hwnd_id(&w2), hwnd_id(&w3));

    // Sanity: three columns with one window each.
    let vl_before = query_layout_virtual(&pipe).expect("query virtual before");
    let col_count_before = vl_before["column_count"].as_u64();
    assert!(
        col_count_before == Some(3),
        "expected 3 columns after creating 3 windows, got {col_count_before:?}\n\
         layout: {vl_before:#?}"
    );

    settle();

    // Record w3's tiled-rect x BEFORE removal (it's in the third column slot).
    let json_before = query_windows(&pipe).expect("query windows before");
    let w3_x_before = tiled_rect_x(&json_before, w3_id)
        .unwrap_or_else(|| panic!("w3 (hwnd={w3_id}) should have a tiled_rect: {json_before:#?}"));

    // Destroy the middle window — triggers EVENT_OBJECT_DESTROY → on_window_destroyed.
    drop(w2);
    std::thread::sleep(Duration::from_millis(1500));

    // Column count should drop to 2.
    let vl_after = query_layout_virtual(&pipe).expect("query virtual after");
    let col_count_after = vl_after["column_count"].as_u64();
    assert_eq!(
        col_count_after,
        Some(2),
        "expected 2 columns after destroying middle window, got {col_count_after:?}\n\
         layout: {vl_after:#?}"
    );

    // Surviving windows remain; destroyed window is gone.
    let ids = layout_window_ids(&vl_after);
    assert!(
        ids.contains(&w1_id),
        "w1 (hwnd={w1_id}) should remain in layout: ids={ids:?}"
    );
    assert!(
        ids.contains(&w3_id),
        "w3 (hwnd={w3_id}) should remain in layout: ids={ids:?}"
    );
    assert!(
        !ids.contains(&w2_id),
        "w2 (hwnd={w2_id}) should be gone from layout: ids={ids:?}"
    );

    // w3 should have physically shifted left (its tiled-rect x decreased
    // because the middle column's slot is no longer consumed).
    settle();
    let json_after = query_windows(&pipe).expect("query windows after");
    let w3_x_after = tiled_rect_x(&json_after, w3_id).unwrap_or_else(|| {
        panic!("w3 should still have a tiled_rect after removal: {json_after:#?}")
    });

    assert!(
        w3_x_after < w3_x_before,
        "w3 should shift left after removing middle column \
         (before x={w3_x_before}, after x={w3_x_after})"
    );

    println!(
        "✓ destroy_removes_column_and_compacts_layout: \
         columns {col_count_before:?}→{col_count_after:?}, \
         w3 x {w3_x_before}→{w3_x_after}"
    );

    drop(w1);
    drop(w3);
    drop(td);
}

/// Destroying the focused window moves focus to the left neighbour.
///
/// We create three windows, focus the middle one via `FocusLeft`, destroy
/// it, and verify the daemon's `focused` field points to the left window.
#[test]
fn destroy_focused_window_moves_focus_left() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    settle();

    let t1 = unique_title("FOC-A");
    let t2 = unique_title("FOC-B");
    let t3 = unique_title("FOC-C");

    let w1 = TestWindow::create(&t1).expect("create w1");
    let w2 = TestWindow::create(&t2).expect("create w2");
    let w3 = TestWindow::create(&t3).expect("create w3");

    std::thread::sleep(Duration::from_millis(1500));

    // Capture hwnd-ids before any drops.
    let (w1_id, w2_id) = (hwnd_id(&w1), hwnd_id(&w2));

    // After creating three windows, focus should be on w3 (last created).
    // Move focus left once so w2 is focused.
    focus_left(&pipe);
    settle();

    let json_before = query_windows(&pipe).expect("query before destroy");
    let focused_before = json_before["focused"].as_i64();
    assert_eq!(
        focused_before,
        Some(w2_id),
        "focus should be on w2 (hwnd={w2_id}) after FocusLeft, \
         got {focused_before:?}\njson: {json_before:#?}"
    );

    // Destroy the focused window.
    drop(w2);
    std::thread::sleep(Duration::from_millis(1500));

    // Focus should have moved to w1 (left neighbour of w2).
    let json_after = query_windows(&pipe).expect("query after destroy");
    let focused_after = json_after["focused"].as_i64();
    assert_eq!(
        focused_after,
        Some(w1_id),
        "focus should move to w1 (hwnd={w1_id}) after destroying focused w2, \
         got {focused_after:?}\njson: {json_after:#?}"
    );

    println!(
        "✓ destroy_focused_window_moves_focus_left: \
         focus {w2_id} → {w1_id}"
    );

    drop(w1);
    drop(w3);
    drop(td);
}

/// Minimising a tiled window removes it from the layout (same as destroy).
///
/// We create three windows, minimise one, and verify the column count
/// drops.
#[test]
fn minimize_removes_window_from_layout() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    settle();

    let t1 = unique_title("MIN-A");
    let t2 = unique_title("MIN-B");
    let t3 = unique_title("MIN-C");

    let w1 = TestWindow::create(&t1).expect("create w1");
    let w2 = TestWindow::create(&t2).expect("create w2");
    let w3 = TestWindow::create(&t3).expect("create w3");

    std::thread::sleep(Duration::from_millis(1500));

    // Capture hwnd-id before minimising.
    let w2_id = hwnd_id(&w2);

    // Sanity: three columns.
    let vl_before = query_layout_virtual(&pipe).expect("query virtual before");
    assert_eq!(
        vl_before["column_count"].as_u64(),
        Some(3),
        "3 columns expected"
    );

    // Minimise w2 — triggers MinimizeStart hook → on_window_minimized.
    w2.minimize();
    std::thread::sleep(Duration::from_millis(1500));

    // Layout should drop to 2 columns.
    let vl_after = query_layout_virtual(&pipe).expect("query virtual after");
    let col_count_after = vl_after["column_count"].as_u64();
    assert_eq!(
        col_count_after,
        Some(2),
        "expected 2 columns after minimising w2, got {col_count_after:?}\n\
         layout: {vl_after:#?}"
    );

    // w2 should be gone from the layout.
    let ids = layout_window_ids(&vl_after);
    assert!(
        !ids.contains(&w2_id),
        "w2 (hwnd={w2_id}) should be removed from layout after minimise: ids={ids:?}"
    );

    println!(
        "✓ minimize_removes_window_from_layout: \
         columns 3 → {col_count_after:?}"
    );

    drop(w1);
    drop(w2);
    drop(w3);
    drop(td);
}

/// Destroying the only tiled window clears focus.
///
/// With a single window, there is no left or right neighbour, so focus
/// should become `None`.
#[test]
fn destroy_only_window_clears_focus() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    settle();

    let t1 = unique_title("SOLO");
    let w1 = TestWindow::create(&t1).expect("create w1");

    std::thread::sleep(Duration::from_millis(1500));

    let w1_id = hwnd_id(&w1);

    // Focus should be on the single window.
    let json_before = query_windows(&pipe).expect("query before");
    assert_eq!(
        json_before["focused"].as_i64(),
        Some(w1_id),
        "focus should be on the only window"
    );

    // Destroy it — no neighbours, focus should become null.
    drop(w1);
    std::thread::sleep(Duration::from_millis(1500));

    let json_after = query_windows(&pipe).expect("query after");
    let focused_after = &json_after["focused"];
    assert!(
        focused_after.is_null(),
        "focus should be null after destroying the only window, got {focused_after:?}"
    );

    println!("✓ destroy_only_window_clears_focus: focus → null");

    drop(td);
}
