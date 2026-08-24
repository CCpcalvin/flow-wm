//! Integration tests for the `flow cursor` command group.
//!
//! `flow cursor restore` is the escape hatch for the cursor-hide mechanism:
//! it restores the system cursor shapes **locally** in the CLI process — no
//! daemon contact, no IPC. These tests therefore need no test desktop and are
//! not gated by `#[cfg(debug_assertions)]`.
//!
//! The command is harmless by construction (it only restores, never blanks),
//! so running it against the developer's real session is safe.

use super::common::{flow, unique_pipe_name};

/// `flow cursor restore` must succeed with **no daemon running**.
///
/// The test points `FLOW_PIPE_NAME` at a pipe that will never exist, so any
/// accidental pipe connection would fail the command — proving the command
/// executes entirely locally.
#[test]
fn cursor_restore_succeeds_without_daemon() {
    let pipe = unique_pipe_name();
    flow(&pipe)
        .arg("cursor")
        .arg("restore")
        .assert()
        .success()
        .stdout(predicates::str::contains("cursors restored"));
}

/// `flow cursor restore` is idempotent: running it twice in a row both
/// succeeds (restoring already-normal cursors is a no-op).
#[test]
fn cursor_restore_is_idempotent() {
    let pipe = unique_pipe_name();
    flow(&pipe).args(["cursor", "restore"]).assert().success();
    flow(&pipe).args(["cursor", "restore"]).assert().success();
}
