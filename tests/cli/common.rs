//! Shared helpers for CLI integration tests.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use assert_cmd::Command;

/// Per-test unique pipe name counter.
static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Maximum time to wait for any single `stm` command to complete.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// Environment variable name for the pipe path (must match `ipc::message::PIPE_ENV`).
const PIPE_ENV: &str = "STM_PIPE_NAME";

/// Generate a unique pipe name for this test run.
///
/// Each call returns a different name like `\\.\pipe\stm-test-0`, `\\.\pipe\stm-test-1`, etc.
/// Tests that use separate pipe names can run in parallel without interference.
pub fn unique_pipe_name() -> String {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(r"\\.\pipe\stm-test-{id}")
}

/// Build an [`assert_cmd::Command`] for the `stm` CLI binary pre-configured with the given
/// pipe name and a default command timeout.
///
/// Suitable for CLI commands that do **not** spawn a daemon (`stop`,
/// `reload-config`, etc.). For spawning the daemon in tests, use
/// [`crate::test_desktop::start_test_daemon`] which runs it on an isolated
/// desktop.
pub fn stm(pipe: &str) -> Command {
    let mut cmd = Command::cargo_bin("stm").expect("stm binary should be built by cargo test");
    cmd.env(PIPE_ENV, pipe).timeout(COMMAND_TIMEOUT);
    cmd
}

/// Ensure the daemon for the given pipe is stopped, ignoring errors if it was not running.
///
/// Call this at the start and end of each test to guarantee a clean slate
/// and avoid leaving a daemon process running after a test failure.
pub fn ensure_daemon_stopped(pipe: &str) {
    let _ = stm(pipe).arg("stop").assert();
}
