//! Integration tests for `stm dispatch swapcolumn` and `stm dispatch movewindow`.
//!
//! These tests create an isolated desktop, start `stmd` on it, create dummy
//! windows, and exercise the swap/move CLI commands. The daemon's layout is
//! queried before and after each command to verify that columns are actually
//! swapped.
//!
//! Each test uses [`query_layout_virtual`] to inspect the virtual layout's
//! column→window-id mapping — the most direct way to detect a column swap.
//!
//! # Desktop isolation
//!
//! Every test creates a [`TestDesktop`] and spawns `stmd` directly via
//! [`start_test_daemon`] with the `--desktop` flag. The user's real desktop is
//! never touched. Each test gets a unique pipe name for parallel isolation.

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message.  See the same pattern in `daemon_lifecycle.rs` and
// `registry.rs`.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use predicates::prelude::*;

use super::common::{stm, unique_pipe_name};
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, query_layout_virtual, send_ipc_ignore, start_test_daemon,
    unique_title,
};

/// Delay after creating windows to let hooks fire and the daemon tile them.
const HOOK_SETTLE: Duration = Duration::from_millis(1500);

/// Delay after a dispatch command to let the layout update propagate.
const SWAP_SETTLE: Duration = Duration::from_millis(500);

// ── Helpers ──────────────────────────────────────────────────────────

/// Extract the column→window-id mapping from a `query_layout_virtual` JSON
/// response.
///
/// Returns a `Vec` where element *i* is a `Vec` of the window-id integers in
/// column *i* (row order).
fn column_window_ids(json: &serde_json::Value) -> Vec<Vec<i64>> {
    json["columns"]
        .as_array()
        .map(|cols| {
            cols.iter()
                .map(|col| {
                    col["rows"]
                        .as_array()
                        .map(|rows| rows.iter().filter_map(|r| r.as_i64()).collect::<Vec<_>>())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── swapcolumn tests ─────────────────────────────────────────────────

/// `stm dispatch swapcolumn right` swaps the focused column with its right
/// neighbour.
///
/// Setup: two windows → two columns. Focus is moved to the leftmost column
/// (via `focus left`, which is a no-op if already there) so that `swapcolumn
/// right` has a valid target. After the swap the column contents are exchanged.
#[test]
fn swapcolumn_right_swaps_two_columns() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title_a = unique_title("SwapColR-A");
    let title_b = unique_title("SwapColR-B");
    let _wa = TestWindow::create(&title_a).expect("create window A");
    let _wb = TestWindow::create(&title_b).expect("create window B");
    std::thread::sleep(HOOK_SETTLE);

    // Ensure the focused window is in the leftmost column.
    use scrolling_tiling_manager::ipc::message::SocketMessage;
    send_ipc_ignore(&pipe, &SocketMessage::FocusLeft);
    std::thread::sleep(SWAP_SETTLE);

    // Snapshot the column structure before the swap.
    let before = query_layout_virtual(&pipe).expect("query layout before");
    let cols_before = column_window_ids(&before);
    assert_eq!(
        cols_before.len(),
        2,
        "expected 2 columns, got {cols_before:?}"
    );

    // Act: swap columns to the right.
    stm(&pipe)
        .args(["dispatch", "swapcolumn", "right"])
        .assert()
        .stdout(predicate::str::contains("column swapped"))
        .success();
    std::thread::sleep(SWAP_SETTLE);

    // Assert: columns are swapped.
    let after = query_layout_virtual(&pipe).expect("query layout after");
    let cols_after = column_window_ids(&after);
    assert_eq!(
        cols_after.len(),
        2,
        "expected 2 columns after swap, got {cols_after:?}"
    );
    assert_eq!(
        cols_after[0], cols_before[1],
        "column 0 should now hold the old column 1 windows"
    );
    assert_eq!(
        cols_after[1], cols_before[0],
        "column 1 should now hold the old column 0 windows"
    );
}

/// `stm dispatch swapcolumn left` swaps the focused column with its left
/// neighbour.
///
/// Setup: two windows → two columns. Focus is moved to the rightmost column
/// (via `focus right`) so that `swapcolumn left` has a valid target.
#[test]
fn swapcolumn_left_swaps_two_columns() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title_a = unique_title("SwapColL-A");
    let title_b = unique_title("SwapColL-B");
    let _wa = TestWindow::create(&title_a).expect("create window A");
    let _wb = TestWindow::create(&title_b).expect("create window B");
    std::thread::sleep(HOOK_SETTLE);

    // Ensure focus is on the rightmost column.
    use scrolling_tiling_manager::ipc::message::SocketMessage;
    send_ipc_ignore(&pipe, &SocketMessage::FocusRight);
    std::thread::sleep(SWAP_SETTLE);

    let before = query_layout_virtual(&pipe).expect("query layout before");
    let cols_before = column_window_ids(&before);
    assert_eq!(
        cols_before.len(),
        2,
        "expected 2 columns, got {cols_before:?}"
    );

    // Act: swap columns to the left.
    stm(&pipe)
        .args(["dispatch", "swapcolumn", "left"])
        .assert()
        .stdout(predicate::str::contains("column swapped"))
        .success();
    std::thread::sleep(SWAP_SETTLE);

    let after = query_layout_virtual(&pipe).expect("query layout after");
    let cols_after = column_window_ids(&after);
    assert_eq!(cols_after.len(), 2);
    assert_eq!(
        cols_after[0], cols_before[1],
        "column 0 should now hold the old column 1 windows"
    );
    assert_eq!(
        cols_after[1], cols_before[0],
        "column 1 should now hold the old column 0 windows"
    );
}

// ── movewindow tests ─────────────────────────────────────────────────

/// `stm dispatch movewindow right` on tiled windows is equivalent to
/// `swapcolumn right` (the semantic "move" resolves to a column swap for
/// horizontal movement of tiled windows).
#[test]
fn movewindow_right_swaps_two_columns() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title_a = unique_title("MoveWinR-A");
    let title_b = unique_title("MoveWinR-B");
    let _wa = TestWindow::create(&title_a).expect("create window A");
    let _wb = TestWindow::create(&title_b).expect("create window B");
    std::thread::sleep(HOOK_SETTLE);

    // Ensure focus is on the leftmost column.
    use scrolling_tiling_manager::ipc::message::SocketMessage;
    send_ipc_ignore(&pipe, &SocketMessage::FocusLeft);
    std::thread::sleep(SWAP_SETTLE);

    let before = query_layout_virtual(&pipe).expect("query layout before");
    let cols_before = column_window_ids(&before);
    assert_eq!(
        cols_before.len(),
        2,
        "expected 2 columns, got {cols_before:?}"
    );

    // Act: move window right (semantic → column swap for tiled L/R).
    stm(&pipe)
        .args(["dispatch", "movewindow", "right"])
        .assert()
        .stdout(predicate::str::contains("window moved"))
        .success();
    std::thread::sleep(SWAP_SETTLE);

    let after = query_layout_virtual(&pipe).expect("query layout after");
    let cols_after = column_window_ids(&after);
    assert_eq!(cols_after.len(), 2);
    assert_eq!(
        cols_after[0], cols_before[1],
        "column 0 should now hold the old column 1 windows"
    );
    assert_eq!(
        cols_after[1], cols_before[0],
        "column 1 should now hold the old column 0 windows"
    );
}

// ── Edge-case tests ──────────────────────────────────────────────────

/// `stm dispatch swapcolumn right` at the right edge (single column) returns
/// an error — there is no column to swap with.
#[test]
fn swapcolumn_right_at_edge_returns_error() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title = unique_title("EdgeOnly");
    let _w = TestWindow::create(&title).expect("create single window");
    std::thread::sleep(HOOK_SETTLE);

    // Verify we have exactly one column.
    let before = query_layout_virtual(&pipe).expect("query layout before");
    let cols_before = column_window_ids(&before);
    assert_eq!(
        cols_before.len(),
        1,
        "expected exactly 1 column, got {cols_before:?}"
    );

    // Act: swap right should fail (no column to the right).
    stm(&pipe)
        .args(["dispatch", "swapcolumn", "right"])
        .assert()
        .failure();
}
