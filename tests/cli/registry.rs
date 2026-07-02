//! Integrated tests for the WindowRegistry.
//!
//! These tests create an isolated desktop, start `flowd` on it, create dummy
//! windows on it, and verify the registry state via IPC queries. The user's
//! main desktop is never affected.

use std::time::Duration;

use super::common::unique_pipe_name;
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, query_windows, start_test_daemon, unique_title,
};

/// Helper: find a window by title in the JSON response.
fn find_window_by_title<'a>(
    json: &'a serde_json::Value,
    title: &str,
) -> Option<&'a serde_json::Value> {
    json["windows"]
        .as_array()
        .and_then(|arr| arr.iter().find(|w| w["title"].as_str() == Some(title)))
}

// ── Event Hooking Tests ──────────────────────────────────────────────

/// Event Hooking Test: create windows after daemon starts, verify they appear.
#[test]
fn event_hooking_detects_new_windows() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title1 = unique_title("Hook-A");
    let title2 = unique_title("Hook-B");

    let w1 = TestWindow::create(&title1).expect("create window 1");
    let w2 = TestWindow::create(&title2).expect("create window 2");

    // Wait for hook events to fire and be processed.
    std::thread::sleep(Duration::from_millis(1000));

    let json = query_windows(&pipe).expect("query after create");
    let windows = json["windows"].as_array().expect("windows array");

    // Our two windows should be present.
    assert!(
        find_window_by_title(&json, &title1).is_some(),
        "window '{title1}' should be in registry. Windows: {:?}",
        windows
            .iter()
            .filter_map(|w| w["title"].as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        find_window_by_title(&json, &title2).is_some(),
        "window '{title2}' should be in registry. Windows: {:?}",
        windows
            .iter()
            .filter_map(|w| w["title"].as_str())
            .collect::<Vec<_>>()
    );

    println!(
        "✓ event_hooking_detects_new_windows: {} windows in registry",
        windows.len()
    );

    drop(w1);
    drop(w2);
    drop(td);
}

/// Event Hooking Test: create and destroy windows, verify removal.
#[test]
fn event_hooking_create_and_destroy() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title1 = unique_title("CD-1");
    let title2 = unique_title("CD-2");

    let w1 = TestWindow::create(&title1).expect("create window 1");
    let w2 = TestWindow::create(&title2).expect("create window 2");

    std::thread::sleep(Duration::from_millis(1000));

    // Both should be present.
    let json = query_windows(&pipe).expect("query after create");
    assert!(
        find_window_by_title(&json, &title1).is_some(),
        "window 1 should exist"
    );
    assert!(
        find_window_by_title(&json, &title2).is_some(),
        "window 2 should exist"
    );
    let count_after_create = json["windows"].as_array().unwrap().len();

    // Destroy w2.
    drop(w2);
    std::thread::sleep(Duration::from_millis(1000));

    let json2 = query_windows(&pipe).expect("query after destroy");
    assert!(
        find_window_by_title(&json2, &title2).is_none(),
        "destroyed window '{title2}' should be removed from registry"
    );
    assert!(
        find_window_by_title(&json2, &title1).is_some(),
        "surviving window '{title1}' should still be in registry"
    );
    let count_after_destroy = json2["windows"].as_array().unwrap().len();
    assert!(
        count_after_destroy < count_after_create,
        "count should decrease after destroy ({count_after_destroy} >= {count_after_create})"
    );

    println!("✓ event_hooking_create_and_destroy: {count_after_create} -> {count_after_destroy}");

    drop(w1);
    drop(td);
}

/// Event Hooking Test: minimize and restore.
#[test]
fn event_hooking_minimize_and_restore() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    std::thread::sleep(Duration::from_millis(500));

    let title = unique_title("Min");
    let w = TestWindow::create(&title).expect("create window");

    std::thread::sleep(Duration::from_millis(1000));

    // Verify initial state — window should be in registry.
    let json = query_windows(&pipe).expect("query initial");
    let initial = find_window_by_title(&json, &title);
    assert!(initial.is_some(), "window '{title}' should be in registry");
    println!("Initial state: {}", initial.unwrap()["state"]);

    // Minimize.
    w.minimize();
    std::thread::sleep(Duration::from_millis(1000));

    let json_min = query_windows(&pipe).expect("query minimized");
    let m = find_window_by_title(&json_min, &title)
        .unwrap_or_else(|| panic!("window '{title}' should still be in registry after minimize"));
    let state = m["state"].to_string();
    println!("Minimized state: {state}");
    assert!(
        state.contains("Minimized"),
        "state should be Minimized, got: {state}"
    );

    // Restore.
    w.restore();
    std::thread::sleep(Duration::from_millis(1000));

    let json_rest = query_windows(&pipe).expect("query restored");
    let r = find_window_by_title(&json_rest, &title)
        .unwrap_or_else(|| panic!("window '{title}' should still be in registry after restore"));
    let state = r["state"].to_string();
    println!("Restored state: {state}");
    assert!(
        state.contains("Active"),
        "state should be Active after restore, got: {state}"
    );

    drop(w);
    drop(td);
}
