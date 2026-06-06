//! Integration tests for the `stm start` / `stm stop` daemon lifecycle.
//!
//! These tests spawn real `stm` and `stmd` processes and verify that:
//! - `stm start` launches the daemon and waits until it is ready.
//! - `stm start` fails with "already running" when the daemon is up.
//! - `stm stop` shuts the daemon down cleanly.
//! - `stm stop` fails with "not running" when no daemon is present.
//!
//! Each test gets a unique pipe name so they can run in parallel without
//! interference. All tests are Windows-only (named-pipe IPC).

#![cfg(target_os = "windows")]

use predicates::prelude::*;

use super::common::{daemon_start, ensure_daemon_stopped, stm, unique_pipe_name};

/// Full lifecycle: start → verify already running → stop → verify not running.
#[test]
fn start_stop_lifecycle() {
    let pipe = unique_pipe_name();
    ensure_daemon_stopped(&pipe);

    // --- `stm start` should succeed ---
    daemon_start(&pipe);

    // --- `stm start` again should fail with "already running" ---
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
#[test]
fn start_when_already_running() {
    let pipe = unique_pipe_name();
    ensure_daemon_stopped(&pipe);

    // Start once — should succeed.
    daemon_start(&pipe);

    // Start again — should fail.
    stm(&pipe)
        .arg("start")
        .assert()
        .stderr(predicate::str::contains("daemon is already running"))
        .failure();

    // Clean up.
    ensure_daemon_stopped(&pipe);
}
