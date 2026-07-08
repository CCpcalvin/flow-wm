//! Integration tests for the new-window focus contract.
//!
//! Regression guard for `daemon::hooks::FlowWM::reconcile_new_window_focus` —
//! the function that actively pushes OS foreground to a freshly-tracked window
//! and then syncs registry focus.
//!
//! # The bug being guarded against
//!
//! Before the fix, `reconcile_new_window_focus` was **passive**: it only called
//! `registry.set_focused(hwnd)` when the OS had *already* foregrounded the
//! window (`get_foreground_window() == Some(hwnd)`). Apps launched hidden
//! (AutoHotkey `Run(..., "Hide")`) start without an OS foreground grant and
//! their own later `SetForegroundWindow` is denied by the foreground lock, so
//! they ended up tracked + bordered but never recorded as focused — no keyboard
//! input. The fix actively pushes foreground via `set_foreground_window(hwnd)`
//! and only then calls `set_focused`.
//!
//! # What these tests pin
//!
//! The observable in-registry effect: after a new window is created and tiled,
//! the registry's `focused` field equals the new window's HWND. This catches a
//! regression where `set_focused` stops being called (e.g. someone reverts the
//! `became_foreground` gate to the old passive form, or breaks its conditional).
//!
//! # Branch coverage caveat
//!
//! On the isolated test desktop, `CreateWindowExW(WS_VISIBLE)` naturally grants
//! the new window OS foreground, so these tests exercise the
//! `get_foreground_window() == Some(hwnd)` short-circuit branch — NOT the
//! active-push branch (which only fires for windows launched without an OS
//! foreground grant, a scenario that cannot be reproduced reliably under the
//! foreground lock). The active-push branch is covered by the inline
//! characterization tests in `src/daemon/hooks.rs` and by manual verification
//! with AHK-launched hidden windows.
//!
//! # Timing strategy
//!
//! Mirrors the proven pattern in `tests/cli/window_creation.rs`: create each
//! window with a short inter-create gap, then a long settle before a SINGLE
//! query. Polling queries (`wait_until_*`) are deliberately avoided because the
//! isolated test desktop floods the daemon with hundreds of spurious
//! `EVENT_OBJECT_CREATE` hook events for non-test windows; the daemon's
//! event-driven loop drains that backlog between IPC accepts, so a query fired
//! during the backlog hits a momentarily-unavailable pipe.

use std::time::Duration;

use super::common::unique_pipe_name;
use super::test_desktop::{
    DaemonGuard, TestDesktop, TestWindow, query_windows, start_test_daemon, unique_title,
};

/// Wait helper — keeps test code concise.
fn wait(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// Extract the HWND integer for the window with the given title.
///
/// Each window entry in the `QueryWindowsAll` JSON carries its HWND as
/// `"hwnd": <isize>`. Returns `None` if the title is not found.
fn hwnd_of(json: &serde_json::Value, title: &str) -> Option<i64> {
    json["windows"]
        .as_array()?
        .iter()
        .find(|w| w["title"].as_str() == Some(title))
        .and_then(|w| w["hwnd"].as_i64())
}

/// Pretty-print every tracked window's title + hwnd for assertion diagnostics.
fn debug_windows(json: &serde_json::Value) -> String {
    json["windows"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|w| {
                    let title = w["title"].as_str().unwrap_or("?");
                    let hwnd = w["hwnd"].as_i64().unwrap_or(-1);
                    let focused = if json["focused"].as_i64() == Some(hwnd) {
                        " *FOCUSED"
                    } else {
                        ""
                    };
                    format!("{title}@{hwnd:#x}{focused}")
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "<no windows array>".into())
}

// ── Tests ───────────────────────────────────────────────────────────

/// After creating a second window, the registry's `focused` field equals the
/// new window's HWND (and NOT the first window's).
///
/// Steps:
/// 1. Create W1, wait for it to tile.
/// 2. Create W2, wait for the long settle.
/// 3. Assert `focused == W2.hwnd` and `focused != W1.hwnd`.
///
/// This is the direct regression guard for the `if became_foreground {
/// self.registry.set_focused(hwnd); }` line in `reconcile_new_window_focus`.
#[test]
fn window_creation_focus_lands_on_newly_created_window() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    wait(500);

    let t1 = unique_title("Focus-A");
    let t2 = unique_title("Focus-B");

    // Create W1 then W2 — each gets OS foreground on creation (WS_VISIBLE).
    let w1 = TestWindow::create(&t1).expect("create W1");
    wait(400);
    let w2 = TestWindow::create(&t2).expect("create W2");
    wait(1500); // long settle so the FOREGROUND hook + reconcile run before the query

    let json = query_windows(&pipe).expect("query after W1+W2");
    let w1_hwnd = hwnd_of(&json, &t1)
        .unwrap_or_else(|| panic!("W1 '{t1}' missing. Windows: {}", debug_windows(&json)));
    let w2_hwnd = hwnd_of(&json, &t2)
        .unwrap_or_else(|| panic!("W2 '{t2}' missing. Windows: {}", debug_windows(&json)));

    // Assert: focus must land on W2 (positive), not W1 (negative — focus moved).
    assert_eq!(
        json["focused"].as_i64(),
        Some(w2_hwnd),
        "focus should be on W2 (last created). Windows: {}",
        debug_windows(&json)
    );
    assert_ne!(
        json["focused"].as_i64(),
        Some(w1_hwnd),
        "focus should have moved off W1 onto W2. Windows: {}",
        debug_windows(&json)
    );

    println!("✓ window_creation_focus_lands_on_newly_created_window: focused=W2 ({w2_hwnd:#x})");

    drop(w1);
    drop(w2);
    drop(td);
}

/// The focus-sync effect fires on every creation, not just the first.
///
/// Steps:
/// 1. Create W1, W2, W3 in sequence (each gets OS foreground on creation).
/// 2. Assert `focused == W3.hwnd` after the final settle.
/// 3. Assert `focused != W1.hwnd` (the first window must have lost focus).
///
/// Guards the idempotency of the reconcile path — a regression that only syncs
/// focus for the first-ever window (e.g. a stale guard) would leave `focused`
/// stuck on W1.
#[test]
fn window_creation_focus_follows_each_new_window() {
    let td = TestDesktop::create().expect("test desktop");
    let pipe = unique_pipe_name();
    let mut _child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    wait(500);

    let t1 = unique_title("Trail-1");
    let t2 = unique_title("Trail-2");
    let t3 = unique_title("Trail-3");

    let w1 = TestWindow::create(&t1).expect("create W1");
    wait(400);
    let w2 = TestWindow::create(&t2).expect("create W2");
    wait(400);
    let w3 = TestWindow::create(&t3).expect("create W3");
    wait(1500); // long settle so all FOREGROUND hooks + reconcile run

    let json = query_windows(&pipe).expect("query after W1+W2+W3");
    let w1_hwnd = hwnd_of(&json, &t1)
        .unwrap_or_else(|| panic!("W1 '{t1}' missing. Windows: {}", debug_windows(&json)));
    let w3_hwnd = hwnd_of(&json, &t3)
        .unwrap_or_else(|| panic!("W3 '{t3}' missing. Windows: {}", debug_windows(&json)));

    // Assert: focus on W3 (positive), not W1 (negative — first lost focus).
    assert_eq!(
        json["focused"].as_i64(),
        Some(w3_hwnd),
        "focus should be on W3 (last created). Windows: {}",
        debug_windows(&json)
    );
    assert_ne!(
        json["focused"].as_i64(),
        Some(w1_hwnd),
        "focus should not be stuck on W1. Windows: {}",
        debug_windows(&json)
    );

    println!(
        "✓ window_creation_focus_follows_each_new_window: focused=W3 ({w3_hwnd:#x}), not W1 ({w1_hwnd:#x})"
    );

    drop(w1);
    drop(w2);
    drop(w3);
    drop(td);
}
