//! Integration tests for `flow dispatch set-window {float,tile,cycle}`.
//!
//! These exercise the full `dispatch_set_window` → `set_window_to_float` →
//! `set_window_to_tile` pipeline end-to-end on an isolated desktop: a real
//! `flowd` daemon, real Win32 windows, real IPC. The decision logic itself
//! (mode × state → action) is unit-tested in `src/daemon/dispatch.rs`; these
//! tests prove the *wiring* — that floating a window removes it from the
//! tiling layout, and tiling it re-inserts it.
//!
//! Each test creates an isolated [`TestDesktop`], spawns `flowd` on it, creates
//! dummy windows, and queries the virtual layout before/after each command.
//! See `dispatch_swap.rs` for the harness pattern this file mirrors.

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use predicates::prelude::*;

use super::common::{flow, unique_pipe_name};
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, query_layout_virtual, send_ipc_ignore, start_test_daemon,
    unique_title, wait_until_windows_tiled,
};

/// Delay after creating windows to let hooks fire and the daemon tile them.
const HOOK_SETTLE: Duration = Duration::from_millis(1500);

/// Delay after a dispatch command to let the layout update propagate.
const DISPATCH_SETTLE: Duration = Duration::from_millis(500);

// ── Helpers ──────────────────────────────────────────────────────────

/// Count total tiled windows across all columns of a `query_layout_virtual`
/// response. A floated window leaves the tiling layout, so this count drops
/// by one on a successful `set-window float`.
fn tiled_window_count(json: &serde_json::Value) -> usize {
    json["columns"]
        .as_array()
        .map(|cols| {
            cols.iter()
                .filter_map(|col| col["rows"].as_array().map(|rows| rows.len()))
                .sum()
        })
        .unwrap_or(0)
}

/// Number of columns in the virtual layout.
fn column_count(json: &serde_json::Value) -> usize {
    json["columns"]
        .as_array()
        .map(|cols| cols.len())
        .unwrap_or(0)
}

// ── tile → float → tile round-trip ───────────────────────────────────

/// `flow dispatch set-window float` removes the focused window from the tiling
/// layout, and `flow dispatch set-window tile` re-inserts it.
///
/// Setup: two windows → two columns. Focus is moved to the leftmost window
/// (deterministic target). Floating it must collapse the layout to one
/// tiled window; tiling it again must restore two tiled windows.
#[ignore = "non-deterministic on isolated test desktop: daemon hooks may not register/tile windows before the assertion (startup hook race)"]
#[test]
fn set_window_float_then_tile_roundtrips_layout() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title_a = unique_title("SetWin-A");
    let title_b = unique_title("SetWin-B");
    let _wa = TestWindow::create(&title_a).expect("create window A");
    let _wb = TestWindow::create(&title_b).expect("create window B");
    // Wait for both windows to be tiled before reading the baseline layout.
    // Under parallel load a fixed sleep can query before the create → register
    // → classify → tile pipeline finishes, seeing `columns: []`.
    wait_until_windows_tiled(&pipe, 2).expect("2 windows tiled at baseline");

    // Drive focus to the leftmost window so the float target is deterministic.
    use flow_wm::ipc::message::SocketMessage;
    send_ipc_ignore(&pipe, &SocketMessage::FocusLeft);
    std::thread::sleep(DISPATCH_SETTLE);

    // Baseline: both windows tiled → 2 columns, 2 tiled windows.
    let before = query_layout_virtual(&pipe).expect("query layout before");
    let cols_before = column_count(&before);
    let tiled_before = tiled_window_count(&before);
    assert_eq!(
        cols_before, 2,
        "expected 2 tiled columns at baseline, got {cols_before}"
    );
    assert_eq!(
        tiled_before, 2,
        "expected 2 tiled windows at baseline, got {tiled_before}"
    );

    // Act 1: float the focused window.
    flow(&pipe)
        .args(["dispatch", "set-window", "float"])
        .assert()
        .stdout(predicate::str::contains("window set to float"))
        .success();
    std::thread::sleep(DISPATCH_SETTLE);

    // Assert 1: the floated window left the tiling layout → 1 column / 1 tiled.
    let after_float = query_layout_virtual(&pipe).expect("query layout after float");
    let cols_after_float = column_count(&after_float);
    let tiled_after_float = tiled_window_count(&after_float);
    assert_eq!(
        cols_after_float, 1,
        "floating one of two windows must collapse to 1 column, got {cols_after_float}"
    );
    assert_eq!(
        tiled_after_float, 1,
        "floating one of two windows must leave 1 tiled window, got {tiled_after_float}"
    );

    // Act 2: tile it again (OS focus stays on the floated window per spec).
    flow(&pipe)
        .args(["dispatch", "set-window", "tile"])
        .assert()
        .stdout(predicate::str::contains("window set to tile"))
        .success();
    std::thread::sleep(DISPATCH_SETTLE);

    // Assert 2: the window re-entered the tiling layout → 2 columns again.
    let after_tile = query_layout_virtual(&pipe).expect("query layout after tile");
    let cols_after_tile = column_count(&after_tile);
    let tiled_after_tile = tiled_window_count(&after_tile);
    assert_eq!(
        cols_after_tile, 2,
        "re-tiling the floated window must restore 2 columns, got {cols_after_tile}"
    );
    assert_eq!(
        tiled_after_tile, 2,
        "re-tiling the floated window must restore 2 tiled windows, got {tiled_after_tile}"
    );
}

// ── cycle toggle ─────────────────────────────────────────────────────

/// `flow dispatch set-window cycle` toggles tiled → floating on the first call
/// (mirroring `set-window float` on a tiled window).
///
/// This covers the `WindowMode::Cycle` branch end-to-end; the `Cycle` decision
/// itself is unit-tested in `src/daemon/dispatch.rs`.
#[ignore = "non-deterministic on isolated test desktop: daemon hooks may not register/tile windows before the assertion (startup hook race)"]
#[test]
fn set_window_cycle_toggles_tiled_to_floating() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title_a = unique_title("SetWinCyc-A");
    let title_b = unique_title("SetWinCyc-B");
    let _wa = TestWindow::create(&title_a).expect("create window A");
    let _wb = TestWindow::create(&title_b).expect("create window B");
    std::thread::sleep(HOOK_SETTLE);

    use flow_wm::ipc::message::SocketMessage;
    send_ipc_ignore(&pipe, &SocketMessage::FocusLeft);
    std::thread::sleep(DISPATCH_SETTLE);

    let before = query_layout_virtual(&pipe).expect("query layout before");
    assert_eq!(column_count(&before), 2);

    // Act: cycle a tiled window → must float (leave the tiling layout).
    flow(&pipe)
        .args(["dispatch", "set-window", "cycle"])
        .assert()
        .stdout(predicate::str::contains("window set to cycle"))
        .success();
    std::thread::sleep(DISPATCH_SETTLE);

    let after = query_layout_virtual(&pipe).expect("query layout after cycle");
    assert_eq!(
        column_count(&after),
        1,
        "cycle on a tiled window must float it → 1 column"
    );
}
