//! Integration tests for the window-creation placement pipeline.
//!
//! These tests verify that newly created tiling windows are **inserted
//! immediately after the focused column** rather than appended at the far
//! right of the layout.
//!
//! # Test strategy
//!
//! 1. Create an isolated test desktop and start `stmd` on it.
//! 2. Create dummy windows via [`TestWindow`] (fires `EVENT_OBJECT_CREATE`).
//! 3. Control focus via [`set_foreground_window`] (fires `EVENT_OBJECT_FOCUS`).
//! 4. Query the daemon via IPC and inspect each window's `col` index in its
//!    `state` field to verify column ordering.
//!
//! The key assertion: after focusing window W1 and creating W3, W3 should
//! land at column 1 (between W1 and W2), and W2 should shift to column 2.
//! With the old append-at-end behaviour, W3 would be at column 2 instead.
//!
//! # Desktop isolation
//!
//! Every test uses [`TestDesktop`] + [`start_test_daemon`] so the user's real
//! desktop is never touched. Each test gets a unique pipe name for parallel
//! safety.

use std::time::Duration;

use super::common::unique_pipe_name;
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, query_windows, start_test_daemon, unique_title,
};
use scrolling_tiling_manager::ipc::message::{SocketMessage, SocketResponse};
use scrolling_tiling_manager::ipc::transport;

/// Wait helper — keeps test code concise.
fn wait(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// Send an IPC command to the daemon and return the response.
///
/// Retries up to 5 times with 100 ms sleep between attempts. This handles the
/// brief window where the daemon's IPC loop is between `DisconnectNamedPipe`
/// and the next `ConnectNamedPipe` — the pipe appears unavailable during that
/// gap but becomes available again almost immediately.
///
/// Used to change layout focus deterministically (e.g. `FocusLeft`) without
/// relying on `SetForegroundWindow`, which is unreliable on test desktops due
/// to Windows foreground-lock restrictions.
fn send_ipc(pipe: &str, msg: &SocketMessage) -> Result<SocketResponse, String> {
    for attempt in 0..5 {
        match transport::send_message_to(pipe, msg) {
            Ok(resp) => return Ok(resp),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused && attempt < 4 => {
                wait(100);
                continue;
            }
            Err(e) => return Err(format!("IPC failed after {} attempts: {e}", attempt + 1)),
        }
    }
    unreachable!("retry loop exhausted without return")
}

/// Extract the column index for a tiling window from the IPC JSON response.
///
/// The `state` field for an active tiling window looks like:
/// `{"Tiling": {"Active": {"col": N, "row": 0}}}`.
///
/// Returns `None` if the window is not found or not in a tiling-active state.
fn column_of(json: &serde_json::Value, title: &str) -> Option<i64> {
    json["windows"]
        .as_array()?
        .iter()
        .find(|w| w["title"].as_str() == Some(title))
        .and_then(|w| w["state"]["Tiling"]["Active"]["col"].as_i64())
}

/// Print all tracked windows for diagnostic output on assertion failure.
fn debug_titles(json: &serde_json::Value) -> String {
    json["windows"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|w| {
                    let title = w["title"].as_str().unwrap_or("?");
                    let col = w["state"]["Tiling"]["Active"]["col"].as_i64();
                    format!("{title}@col{col:?}")
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "<no windows array>".into())
}

// ── Tests ───────────────────────────────────────────────────────────

/// New windows are inserted immediately after the focused column.
///
/// Steps:
/// 1. Create W1, W2 → layout `[W1][W2]`, focus on W2 (last created).
/// 2. Move focus to W1 via IPC `FocusLeft` (deterministic — avoids Win32
///    foreground-lock issues on test desktops).
/// 3. Create W3 → **must** be inserted at column 1 (between W1 and W2),
///    shifting W2 to column 2: `[W1][W3][W2]`.
///
/// With the old append-at-end code, W3 would be at column 2 and W2 would
/// remain at column 1. This test distinguishes the two behaviours.
#[test]
fn window_creation_inserts_after_focused() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    wait(500);

    let t1 = unique_title("Ins-A");
    let t2 = unique_title("Ins-B");
    let t3 = unique_title("Ins-C");

    // Create W1 and W2 — both become tiling columns.
    // After creation, W2 is focused (last created gets OS focus).
    let w1 = TestWindow::create(&t1).expect("create W1");
    wait(300);
    let w2 = TestWindow::create(&t2).expect("create W2");
    wait(1000);

    // Verify initial layout: W1 at col 0, W2 at col 1.
    let json = query_windows(&pipe).expect("query after W1+W2");
    let col1 = column_of(&json, &t1).unwrap_or_else(|| {
        panic!(
            "W1 '{t1}' should be tiling. Windows: {}",
            debug_titles(&json)
        )
    });
    let col2 = column_of(&json, &t2).unwrap_or_else(|| {
        panic!(
            "W2 '{t2}' should be tiling. Windows: {}",
            debug_titles(&json)
        )
    });
    assert_eq!(
        col1,
        0,
        "W1 should start at col 0. Windows: {}",
        debug_titles(&json)
    );
    assert_eq!(
        col2,
        1,
        "W2 should start at col 1. Windows: {}",
        debug_titles(&json)
    );

    // --- Move focus to W1 via IPC so the next window inserts after it ---
    // Focus is currently on W2 (col 1). FocusLeft moves it to W1 (col 0).
    let resp = send_ipc(&pipe, &SocketMessage::FocusLeft).expect("FocusLeft IPC");
    assert!(
        matches!(resp, SocketResponse::Ok),
        "FocusLeft should succeed, got: {resp:?}"
    );
    wait(500); // allow focus change to settle in the layout engine

    // --- Create W3: should be inserted between W1 and W2 ---
    let w3 = TestWindow::create(&t3).expect("create W3");
    wait(1500); // allow create event + animation to settle

    let json2 = query_windows(&pipe).expect("query after insert");
    let col1b = column_of(&json2, &t1)
        .unwrap_or_else(|| panic!("W1 missing after insert. Windows: {}", debug_titles(&json2)));
    let col2b = column_of(&json2, &t2)
        .unwrap_or_else(|| panic!("W2 missing after insert. Windows: {}", debug_titles(&json2)));
    let col3 = column_of(&json2, &t3)
        .unwrap_or_else(|| panic!("W3 missing after insert. Windows: {}", debug_titles(&json2)));

    // With insert-after-focused: W1=col0, W3=col1 (new), W2=col2 (shifted right)
    assert_eq!(
        col1b,
        0,
        "W1 should stay at col 0 after insert. Windows: {}",
        debug_titles(&json2)
    );
    assert_eq!(
        col3,
        1,
        "W3 (new) should be at col 1 — inserted after focused W1. Windows: {}",
        debug_titles(&json2)
    );
    assert_eq!(
        col2b,
        2,
        "W2 should shift to col 2 (pushed right by insertion). Windows: {}",
        debug_titles(&json2)
    );

    println!("✓ window_creation_inserts_after_focused: [W1={col1b}][W3={col3}][W2={col2b}]");

    drop(w1);
    drop(w2);
    drop(w3);
    drop(td);
}

/// The first window in an empty layout becomes the sole column.
///
/// This verifies the "no focused window" edge case: when the layout is empty
/// (no focus), `insert_window` falls back to appending, which for an empty
/// layout means the new window is the only column.
#[ignore = "non-deterministic on isolated test desktop: daemon hooks may not register/tile windows before the assertion (startup hook race)"]
#[test]
fn window_creation_first_window_is_sole_column() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    wait(500);

    let t1 = unique_title("First");
    let w1 = TestWindow::create(&t1).expect("create window");
    wait(1000);

    let json = query_windows(&pipe).expect("query after first window");
    let col = column_of(&json, &t1).unwrap_or_else(|| {
        panic!(
            "window '{t1}' should be tiling. Windows: {}",
            debug_titles(&json)
        )
    });
    assert_eq!(
        col,
        0,
        "first window should be at col 0. Windows: {}",
        debug_titles(&json)
    );

    println!("✓ window_creation_first_window_is_sole_column: W1 at col {col}");

    drop(w1);
    drop(td);
}

/// Creating successive windows with focus on the last one appends at the end.
///
/// This is the common workflow: each new window gets OS focus, so focus is
/// always on the last column. New windows are inserted after the last column,
/// which is equivalent to appending.
#[test]
fn window_creation_successive_appends_when_last_focused() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    wait(500);

    let t1 = unique_title("Succ-1");
    let t2 = unique_title("Succ-2");
    let t3 = unique_title("Succ-3");

    let w1 = TestWindow::create(&t1).expect("create W1");
    wait(400);
    let w2 = TestWindow::create(&t2).expect("create W2");
    wait(400);
    let w3 = TestWindow::create(&t3).expect("create W3");
    wait(1500);

    let json = query_windows(&pipe).expect("query after three windows");
    let col1 = column_of(&json, &t1)
        .unwrap_or_else(|| panic!("W1 missing. Windows: {}", debug_titles(&json)));
    let col2 = column_of(&json, &t2)
        .unwrap_or_else(|| panic!("W2 missing. Windows: {}", debug_titles(&json)));
    let col3 = column_of(&json, &t3)
        .unwrap_or_else(|| panic!("W3 missing. Windows: {}", debug_titles(&json)));

    // Each new window is focused (last created), so insert-after-focused =
    // append-at-end. Columns should be in creation order.
    assert_eq!(
        col1,
        0,
        "W1 should be col 0. Windows: {}",
        debug_titles(&json)
    );
    assert_eq!(
        col2,
        1,
        "W2 should be col 1. Windows: {}",
        debug_titles(&json)
    );
    assert_eq!(
        col3,
        2,
        "W3 should be col 2. Windows: {}",
        debug_titles(&json)
    );

    println!("✓ window_creation_successive_appends: [W1={col1}][W2={col2}][W3={col3}]");

    drop(w1);
    drop(w2);
    drop(w3);
    drop(td);
}
