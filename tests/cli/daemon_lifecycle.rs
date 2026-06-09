//! Integration tests for the `stm` CLI daemon lifecycle (`start` / `stop`).
//!
//! These tests verify that:
//! - `stm start` fails with "already running" when the daemon is up.
//! - `stm stop` shuts the daemon down cleanly.
//! - `stm stop` fails with "not running" when no daemon is present.
//!
//! # Desktop isolation
//!
//! Every test creates a [`TestDesktop`] and spawns `stmd` directly via
//! [`start_test_daemon`] with the `--desktop` flag. This ensures the daemon
//! runs on an isolated desktop and never touches the user's real desktop.
//!
//! Each test gets a unique pipe name so they can run in parallel without
//! interference. All tests use named-pipe IPC.

use predicates::prelude::*;

use super::common::{ensure_daemon_stopped, stm, unique_pipe_name};
use super::test_desktop::{DaemonGuard, TestDesktop, start_test_daemon};

/// Full lifecycle: daemon running → verify `stm start` rejects → `stm stop` → verify `stm stop` rejects.
///
/// The daemon is spawned directly on an isolated test desktop (bypassing `stm
/// start`) so that the daemon never touches the real desktop. We then exercise
/// the `stm start` and `stm stop` CLI commands to verify their behaviour
/// against a running (or stopped) daemon.
#[test]
fn start_stop_lifecycle() {
    // Arrange: isolated desktop + daemon already running.
    let td = TestDesktop::create().expect("create test desktop");
    let pipe = unique_pipe_name();
    let _daemon = start_test_daemon(&pipe, &td.name).expect("start test daemon");
    let _guard = DaemonGuard::new(&pipe);

    // --- `stm start` should fail with "already running" ---
    stm(&pipe)
        .arg("start")
        .assert()
        .stderr(predicate::str::contains("daemon is already running"))
        .failure();

    // --- `stm stop` should succeed ---
    stm(&pipe)
        .arg("stop")
        .assert()
        .stdout(predicate::str::contains("daemon stopped"))
        .success();

    // --- `stm stop` again should fail with "not running" ---
    stm(&pipe)
        .arg("stop")
        .assert()
        .stderr(predicate::str::contains("daemon not running"))
        .failure();
}

/// `stm stop` when no daemon is running reports an error.
#[test]
fn stop_when_not_running() {
    let pipe = unique_pipe_name();
    ensure_daemon_stopped(&pipe);

    stm(&pipe)
        .arg("stop")
        .assert()
        .stderr(predicate::str::contains("daemon not running"))
        .failure();
}

/// `stm start` when the daemon is already running reports an error.
///
/// The daemon is spawned directly on an isolated test desktop. We then verify
/// that `stm start` (the CLI command) detects the already-running daemon.
#[test]
fn start_when_already_running() {
    // Arrange: isolated desktop + daemon already running.
    let td = TestDesktop::create().expect("create test desktop");
    let pipe = unique_pipe_name();
    let _daemon = start_test_daemon(&pipe, &td.name).expect("start test daemon");
    let _guard = DaemonGuard::new(&pipe);

    // Act + Assert: `stm start` should fail because the daemon is up.
    stm(&pipe)
        .arg("start")
        .assert()
        .stderr(predicate::str::contains("daemon is already running"))
        .failure();
}
