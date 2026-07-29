//! Integration tests for the loadout save/restore feature.
//!
//! These tests exercise the **HWND-exact** window-identity matcher end to end
//! against the real `flowd` on an isolated [`TestDesktop`]. They spawn the
//! daemon, drive it over named-pipe IPC, and assert on layout state queried
//! back through IPC.
//!
//! # Why these tests exist
//!
//! The matcher keys purely on a window's Win32 `HWND`, which is stable and
//! unique across a daemon restart (the target applications keep running
//! independently of the daemon). The cases below verify the external behaviour
//! the matcher promises — not its internals:
//!
//! - **Round-trip** (save → stop → start → restored), using a *floating*
//!   window as the restore signal: the daemon's init re-tiles every window, so
//!   only a successful restore brings the float back.
//! - **Identical-window disambiguation** (the Windows Terminal fix): several
//!   windows sharing one exe/class/title triple each land in their exact saved
//!   slot — proving `HWND`, not the triple, is doing the matching.
//! - **No-partial abort**: a loadout referencing a destroyed window is rejected
//!   wholesale; the survivors keep their fresh init layout.
//! - **Leftover append**: a window open now but absent from the loadout is
//!   appended as a new column rather than dropped.
//!
//! # Desktop isolation & parallelism
//!
//! Each test gets its own [`TestDesktop`] and unique pipe name. Test windows
//! are real OS windows that survive a daemon stop/start — exactly the
//! `HWND`-stable scenario the matcher needs. These tests follow the same
//! window-creation pattern (create after the daemon is up, then poll until the
//! hooks register and tile them) as the existing `window_creation` suite; like
//! that suite, the isolated-desktop hook race can be timing-sensitive under
//! heavy parallel load, so failures here usually indicate a harness hiccup
//! rather than a matcher regression — re-run serially to confirm.

use std::time::{Duration, Instant};

use super::common::unique_pipe_name;
use super::test_desktop::{
    DaemonGuard, KillingChild, TestDesktop, TestWindow, query_windows, send_ipc_retry,
    start_test_daemon, stop_test_daemon, unique_title,
};
use flow_wm::ipc::message::{SocketMessage, SocketResponse, WindowMode};

/// Small sleep helper to keep test steps readable.
fn wait(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// Poll a spawned daemon child until it exits, force-killing it on timeout.
///
/// Used between a `stop_test_daemon` IPC and the next `start_test_daemon` so
/// the old process has fully released the named pipe before the replacement
/// binds it.
fn wait_for_exit(child: &mut KillingChild, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => wait(100),
            // Timed out still running, or wait error — force-terminate.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

// ── Query helpers (operate on `query_windows` JSON) ─────────────────

/// Column index of a tiled window with the given `HWND`, or `None` if it is
/// not present / not actively tiled.
fn col_of_hwnd(json: &serde_json::Value, hwnd: isize) -> Option<i64> {
    json["windows"]
        .as_array()?
        .iter()
        .find(|w| w["hwnd"].as_i64() == Some(hwnd as i64))
        .and_then(|w| w["state"]["Tiling"]["Active"]["col"].as_i64())
}

/// Whether the window with the given `HWND` is in the `Floating` state.
fn is_floating(json: &serde_json::Value, hwnd: isize) -> bool {
    json["windows"]
        .as_array()
        .map(|arr| {
            arr.iter().any(|w| {
                w["hwnd"].as_i64() == Some(hwnd as i64) && w["state"].get("Floating").is_some()
            })
        })
        .unwrap_or(false)
}

/// Whether the daemon is tracking a window with the given `HWND`.
fn window_present(json: &serde_json::Value, hwnd: isize) -> bool {
    json["windows"]
        .as_array()
        .map(|arr| arr.iter().any(|w| w["hwnd"].as_i64() == Some(hwnd as i64)))
        .unwrap_or(false)
}

/// Query all windows, retrying across the named-pipe refusal window.
///
/// `query_windows` is a single-shot send; right after another IPC command the
/// daemon is briefly between `DisconnectNamedPipe` and the next
/// `ConnectNamedPipe`, so a bare query can be refused. Retrying (like
/// [`send_ipc_retry`]) makes back-to-back query-after-command sequences
/// reliable.
fn query_windows_retry(pipe: &str) -> serde_json::Value {
    for _ in 0..40 {
        match query_windows(pipe) {
            Ok(v) => return v,
            Err(_) => wait(25),
        }
    }
    panic!("query_windows failed after retries (daemon unreachable)");
}

/// Block until the daemon is tracking every `HWND` in `hwnds`, or panic.
///
/// Used after a restart, when the surviving windows are picked up by the fresh
/// daemon's init scan and/or hooks — either path lands them in the registry,
/// so polling on presence (not just tiling) is the reliable signal.
fn wait_until_present(pipe: &str, hwnds: &[isize]) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let json = query_windows_retry(pipe);
        if hwnds.iter().all(|h| window_present(&json, *h)) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("daemon did not register windows {hwnds:?} within 15s");
        }
        wait(100);
    }
}

/// Poll the active workspace's virtual layout until exactly `expected` windows
/// are tiled in columns, with a generous budget.
///
/// The shared `wait_until_windows_tiled` polls for only 2 s. On an isolated
/// test desktop the daemon's WinEvent hooks are process-global, so while a
/// real desktop is attached they also field a stream of unrelated CREATE
/// events (filtered as "not visible"/"empty title"); under that noise a freshly
/// created window can take a few seconds to register *and* tile. A 10 s budget
/// absorbs that without false timeouts.
fn wait_until_n_tiled(pipe: &str, expected: usize) {
    use super::test_desktop::{active_window_ids, query_layout_virtual};
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = String::new();
    loop {
        if let Ok(json) = query_layout_virtual(pipe) {
            if active_window_ids(&json).len() == expected {
                return;
            }
            last = format!("{json:?}");
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {expected} windows to tile (last layout: {last})");
        }
        wait(100);
    }
}

/// Float the window identified by `target_hwnd`, retrying until it is actually
/// in the `Floating` state.
///
/// `SetWindow { Float }` operates on the OS-foreground window and is a silent
/// no-op when there is no foreground tile. The last-created test window is
/// normally foreground, but the foreground event can lag — so we retry until
/// the target is observed floating (or give up after a budget).
fn float_until_floating(pipe: &str, target_hwnd: isize) {
    for _ in 0..20 {
        let resp = send_ipc_retry(
            pipe,
            &SocketMessage::SetWindow {
                mode: WindowMode::Float,
            },
        )
        .expect("SetWindow Float IPC");
        assert!(
            matches!(resp, SocketResponse::Ok),
            "SetWindow Float should succeed, got: {resp:?}"
        );
        wait(150);
        if is_floating(&query_windows_retry(pipe), target_hwnd) {
            return;
        }
    }
    panic!("window hwnd {target_hwnd:#x} did not float after repeated SetWindow Float");
}

// ── Tests ───────────────────────────────────────────────────────────

/// Save → stop → start restores a floating window.
///
/// The daemon's init pass re-tiles **every** surviving window, so a restored
/// *float* is unambiguous proof that the loadout was applied (and not merely
/// that init happened to tile the same windows). This covers the core
/// resilience story: an arrangement survives a daemon restart.
#[test]
fn loadout_roundtrip_restores_floating_window() {
    let td = TestDesktop::create().expect("create test desktop");
    let pipe = unique_pipe_name();
    let mut child = start_test_daemon(&pipe, &td.name).expect("start first daemon");
    wait(700);

    // Two tiled windows; W2 (last created) holds OS foreground focus.
    let t1 = unique_title("RT-1");
    let t2 = unique_title("RT-2");
    let w1 = TestWindow::create(&t1).expect("create W1");
    wait(300);
    let w2 = TestWindow::create(&t2).expect("create W2");
    let h1 = w1.hwnd.0 as isize;
    let h2 = w2.hwnd.0 as isize;
    wait_until_n_tiled(&pipe, 2);
    float_until_floating(&pipe, h2);
    wait(200);
    let pre = query_windows_retry(&pipe);
    assert!(
        is_floating(&pre, h2),
        "W2 should be floating before save (setup precondition)"
    );
    assert!(col_of_hwnd(&pre, h1).is_some(), "W1 should remain tiled");

    // Save the arrangement.
    let resp =
        send_ipc_retry(&pipe, &SocketMessage::LoadoutSave { path: None }).expect("LoadoutSave IPC");
    assert!(matches!(resp, SocketResponse::Ok), "save failed: {resp:?}");

    // Stop the daemon and wait for the process to exit.
    stop_test_daemon(&pipe);
    wait_for_exit(&mut child, Duration::from_secs(10));
    drop(child);

    // Start a fresh daemon on the same pipe/desktop — it auto-restores during
    // init (the windows persist across the restart, so their HWNDs are stable).
    let child2 = start_test_daemon(&pipe, &td.name).expect("restart daemon");
    let _guard = DaemonGuard::new(&pipe);
    wait_until_present(&pipe, &[h1, h2]);
    wait(700); // allow init tiling + auto-restore to settle

    let post = query_windows_retry(&pipe);
    assert!(
        col_of_hwnd(&post, h1).is_some(),
        "W1 must be tiled after restore"
    );
    assert!(
        is_floating(&post, h2),
        "W2 must be restored to floating — init would have tiled it, so a float \
         here proves the loadout was applied"
    );

    drop(w1);
    drop(w2);
    drop(td);
    let _ = child2; // DaemonGuard stops it on drop
}

/// Identical-exe/class/title windows are disambiguated by `HWND`.
///
/// Three windows share one title (the Windows Terminal situation: same exe,
/// same class, volatile/duplicate title). Under the old `(exe, class, title)`
/// matcher they would collide and restore to arbitrary slots; under HWND-exact
/// matching each lands in its exact saved column. We float the third (the
/// restore signal) and assert the other two keep their precise columns.
#[test]
fn loadout_disambiguates_identical_windows_by_hwnd() {
    let td = TestDesktop::create().expect("create test desktop");
    let pipe = unique_pipe_name();
    let mut child = start_test_daemon(&pipe, &td.name).expect("start first daemon");
    wait(700);

    // Three windows with an IDENTICAL title → indistinguishable by triple.
    let same = unique_title("Same-Term");
    let w1 = TestWindow::create(&same).expect("create W1");
    wait(350);
    let w2 = TestWindow::create(&same).expect("create W2");
    wait(350);
    let w3 = TestWindow::create(&same).expect("create W3");
    let h1 = w1.hwnd.0 as isize;
    let h2 = w2.hwnd.0 as isize;
    let h3 = w3.hwnd.0 as isize;
    wait_until_n_tiled(&pipe, 3);

    // Float W3 (foreground) — restore signal + reduces the tiled pair to [W1, W2].
    float_until_floating(&pipe, h3);
    wait(200);
    let pre = query_windows_retry(&pipe);
    assert!(
        is_floating(&pre, h3),
        "W3 should float before save (precondition)"
    );

    // Save: layout is [W1@col0, W2@col1] tiled + W3 floating.
    let resp =
        send_ipc_retry(&pipe, &SocketMessage::LoadoutSave { path: None }).expect("LoadoutSave IPC");
    assert!(matches!(resp, SocketResponse::Ok), "save failed: {resp:?}");

    // Restart — init re-tiles all three (identical by triple); restore must put
    // each back by HWND.
    stop_test_daemon(&pipe);
    wait_for_exit(&mut child, Duration::from_secs(10));
    drop(child);
    let child2 = start_test_daemon(&pipe, &td.name).expect("restart daemon");
    let _guard = DaemonGuard::new(&pipe);
    wait_until_present(&pipe, &[h1, h2, h3]);
    wait(800); // allow init tiling + auto-restore to settle

    let post = query_windows_retry(&pipe);
    // W3 restored to floating — the signal that restore ran (init tiles it).
    assert!(is_floating(&post, h3), "W3 must be restored to floating");
    // W1 and W2 are indistinguishable by triple but must keep their EXACT saved
    // columns — this is the disambiguation the HWND matcher provides.
    assert_eq!(
        col_of_hwnd(&post, h1),
        Some(0),
        "W1 must be restored to column 0 (by HWND)"
    );
    assert_eq!(
        col_of_hwnd(&post, h2),
        Some(1),
        "W2 must be restored to column 1 (by HWND)"
    );

    drop(w1);
    drop(w2);
    drop(w3);
    drop(td);
    let _ = child2;
}

/// A loadout referencing a now-destroyed window aborts the whole load.
///
/// No partial application: the survivor keeps its fresh init layout, and the
/// error names the missing window so the failure is diagnosable.
#[test]
fn loadout_aborts_when_a_saved_window_is_missing() {
    let td = TestDesktop::create().expect("create test desktop");
    let pipe = unique_pipe_name();
    let child = start_test_daemon(&pipe, &td.name).expect("start daemon");
    let _guard = DaemonGuard::new(&pipe);
    wait(700);

    let t1 = unique_title("Ab-A");
    let t2 = unique_title("Ab-B");
    let w1 = TestWindow::create(&t1).expect("create W1");
    wait(300);
    let w2 = TestWindow::create(&t2).expect("create W2");
    let h1 = w1.hwnd.0 as isize;
    let h2 = w2.hwnd.0 as isize;
    wait_until_n_tiled(&pipe, 2);

    // Save references both W1 and W2.
    let resp =
        send_ipc_retry(&pipe, &SocketMessage::LoadoutSave { path: None }).expect("LoadoutSave IPC");
    assert!(matches!(resp, SocketResponse::Ok), "save failed: {resp:?}");

    // Destroy W2 — its HWND is no longer live.
    drop(w2);
    wait(700);
    let pre = query_windows_retry(&pipe);
    assert!(
        !window_present(&pre, h2),
        "W2 must be destroyed (absent from registry) before load"
    );

    // Force-load must abort (W2's HWND missing) — no partial application.
    let resp = send_ipc_retry(
        &pipe,
        &SocketMessage::LoadoutLoad {
            path: None,
            force: true,
        },
    )
    .expect("LoadoutLoad IPC");
    match resp {
        SocketResponse::Error { message } => {
            assert!(
                message.contains("not currently open"),
                "abort should diagnose the missing window, got: {message}"
            );
        }
        other => panic!("expected Error (no-partial abort), got {other:?}"),
    }

    // The survivor W1 must still be tiled — the abort touched no state.
    let post = query_windows_retry(&pipe);
    assert!(
        col_of_hwnd(&post, h1).is_some(),
        "W1 must remain tiled after the aborted load"
    );

    drop(w1);
    drop(td);
    let _ = child;
}

/// A window open now but absent from the loadout is appended as a new column.
///
/// W3 is created while the daemon is *down* (after the save), so the next
/// daemon's init scan registers it — no hook race. The auto-restore then treats
/// W3 as a leftover and appends it as a column.
#[test]
fn loadout_appends_unreferenced_window_as_column() {
    let td = TestDesktop::create().expect("create test desktop");
    let pipe = unique_pipe_name();
    let mut child = start_test_daemon(&pipe, &td.name).expect("start first daemon");
    wait(700);

    let t1 = unique_title("Lf-A");
    let t2 = unique_title("Lf-B");
    let w1 = TestWindow::create(&t1).expect("create W1");
    wait(300);
    let w2 = TestWindow::create(&t2).expect("create W2");
    let h1 = w1.hwnd.0 as isize;
    let h2 = w2.hwnd.0 as isize;
    wait_until_n_tiled(&pipe, 2);

    // Save references only W1 and W2.
    let resp =
        send_ipc_retry(&pipe, &SocketMessage::LoadoutSave { path: None }).expect("LoadoutSave IPC");
    assert!(matches!(resp, SocketResponse::Ok), "save failed: {resp:?}");

    // Stop the daemon, THEN create W3 — it is not referenced by the loadout and
    // the next daemon's init scan registers it (no hook race).
    stop_test_daemon(&pipe);
    wait_for_exit(&mut child, Duration::from_secs(10));
    drop(child);

    let t3 = unique_title("Lf-C");
    let w3 = TestWindow::create(&t3).expect("create W3");
    let h3 = w3.hwnd.0 as isize;

    // Restart — init scans [W1, W2, W3], tiles them, then auto-restore applies
    // the saved [W1, W2] and appends W3 as a leftover column.
    let child2 = start_test_daemon(&pipe, &td.name).expect("restart daemon");
    let _guard = DaemonGuard::new(&pipe);
    wait_until_present(&pipe, &[h1, h2, h3]);
    wait(800);

    let post = query_windows_retry(&pipe);
    assert!(col_of_hwnd(&post, h1).is_some(), "W1 must be tiled");
    assert!(col_of_hwnd(&post, h2).is_some(), "W2 must be tiled");
    assert!(
        col_of_hwnd(&post, h3).is_some(),
        "W3 (not in the loadout) must be appended as a column, not dropped"
    );

    drop(w1);
    drop(w2);
    drop(w3);
    drop(td);
    let _ = child2;
}
