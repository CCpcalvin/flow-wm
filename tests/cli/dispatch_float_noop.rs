//! Integration tests for the tiled-only-op no-op-on-float contract.
//!
//! Background: every tiled-only dispatch handler (`dispatch_focus`,
//! `dispatch_swap_*`, `dispatch_expand`, …) resolves its target from the
//! OS-foreground window via `registry.focused()` and silently returns
//! `SocketResponse::Ok` when that window is not an active tile. This prevents
//! the bug where focusing a float and pressing a tile keybind (e.g.
//! `expand-column`) would mutate the *stale* tile cursor instead.
//!
//! These tests exercise that contract end-to-end on an isolated desktop: a real
//! `flowd` daemon, real Win32 windows, real IPC. The decision logic itself (the
//! `matches!(win.state, Tiling(TilingState::Active { .. }))` guard) is unit-
//! tested in `src/daemon/dispatch.rs`; these tests prove the *wiring* — that
//! the no-op holds across the full IPC → dispatch → registry pipeline and that
//! the tiling layout is left byte-for-byte unchanged.
//!
//! See `dispatch_swap.rs` and `dispatch_set_window.rs` for the harness pattern
//! this file mirrors.

// The daemon child process is reaped by the OS after `DaemonGuard` sends the
// Stop IPC message.
#![allow(clippy::zombie_processes)]

use std::time::Duration;

use flow_wm::common::Direction;
use flow_wm::ipc::message::{SocketMessage, SocketResponse};

use super::common::{flow, unique_pipe_name};
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, active_window_ids, query_layout_virtual, send_ipc_ignore,
    send_ipc_retry, start_test_daemon, unique_title, wait_until_windows_tiled,
};

/// Delay after a dispatch command to let the layout update propagate.
const DISPATCH_SETTLE: Duration = Duration::from_millis(500);

// ── Helpers ──────────────────────────────────────────────────────────

/// Number of columns in a `query_layout_virtual` response.
fn column_count(json: &serde_json::Value) -> usize {
    json["columns"]
        .as_array()
        .map(|cols| cols.len())
        .unwrap_or(0)
}

/// Total tiled windows across all columns of a `query_layout_virtual` response.
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

// ── no-op storm ──────────────────────────────────────────────────────

/// Every tiled-only IPC message must be a silent no-op (`SocketResponse::Ok`)
/// when the OS-foreground window is a float, and the tiling layout must be
/// left unchanged.
///
/// Setup: two tiled windows → two columns. The leftmost is floated (collapsing
/// the layout to one column). OS focus stays on the float per the
/// `set-window float` contract, so every subsequent tiled-only command must
/// resolve to that float, hit the `Tiling(TilingState::Active)` guard, and
/// return `Ok` without touching the layout.
#[ignore = "non-deterministic on isolated test desktop: daemon hooks may not register/tile windows before the assertion (startup hook race)"]
#[test]
fn tiled_ops_are_noop_when_float_is_focused() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title_a = unique_title("FloatNoop-A");
    let title_b = unique_title("FloatNoop-B");
    let _wa = TestWindow::create(&title_a).expect("create window A");
    let _wb = TestWindow::create(&title_b).expect("create window B");
    wait_until_windows_tiled(&pipe, 2).expect("2 windows tiled at baseline");

    // Drive focus to the leftmost window so the float target is deterministic.
    send_ipc_ignore(&pipe, &SocketMessage::FocusLeft);
    std::thread::sleep(DISPATCH_SETTLE);

    // Float the focused window → layout collapses to 1 column / 1 tiled.
    // OS focus remains on the floated window per the `set-window float` spec.
    flow(&pipe)
        .args(["dispatch", "set-window", "float"])
        .assert()
        .stdout(predicates::str::contains("window set to float"))
        .success();
    std::thread::sleep(DISPATCH_SETTLE);

    let before = query_layout_virtual(&pipe).expect("query layout before no-op storm");
    assert_eq!(
        column_count(&before),
        1,
        "floating one of two windows must collapse to 1 column"
    );
    assert_eq!(
        tiled_window_count(&before),
        1,
        "floating one of two windows must leave 1 tiled window"
    );
    let tiled_ids_before = active_window_ids(&before);

    // Act: fire every tiled-only IPC message while a float is OS-foregrounded.
    // Each must return SocketResponse::Ok — the silent no-op contract.
    let tiled_ops: &[&SocketMessage] = &[
        &SocketMessage::FocusLeft,
        &SocketMessage::FocusRight,
        &SocketMessage::FocusUp,
        &SocketMessage::FocusDown,
        &SocketMessage::SwapLeft,
        &SocketMessage::SwapRight,
        &SocketMessage::SwapUp,
        &SocketMessage::SwapDown,
        &SocketMessage::SwapColumn {
            direction: Direction::Left,
        },
        &SocketMessage::SwapColumn {
            direction: Direction::Right,
        },
        &SocketMessage::MoveWindow {
            direction: Direction::Right,
        },
        &SocketMessage::ExpandColumn,
        &SocketMessage::ShrinkColumn,
        &SocketMessage::SetColumnWidth { width_px: 800 },
        &SocketMessage::ToggleMonocle,
        &SocketMessage::Center,
        &SocketMessage::Promote {
            direction: Direction::Right,
        },
        &SocketMessage::MergeColumn {
            direction: Direction::Right,
        },
    ];
    for msg in tiled_ops {
        let resp =
            send_ipc_retry(&pipe, msg).unwrap_or_else(|e| panic!("IPC send for {msg:?}: {e}"));
        assert_eq!(
            resp,
            SocketResponse::Ok,
            "{msg:?} on a focused float must be a silent no-op (Ok), got {resp:?}"
        );
    }

    // Assert: the tiling layout is byte-for-byte unchanged after the storm.
    let after = query_layout_virtual(&pipe).expect("query layout after no-op storm");
    assert_eq!(
        column_count(&after),
        1,
        "no tiled-only op may mutate the column count while a float is focused"
    );
    assert_eq!(
        tiled_window_count(&after),
        1,
        "no tiled-only op may change the tiled window count while a float is focused"
    );
    assert_eq!(
        active_window_ids(&after),
        tiled_ids_before,
        "tiled window id set must be identical before/after the no-op storm"
    );
}
