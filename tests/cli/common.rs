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
/// Suitable for commands that do **not** spawn a daemon (`stop`, `reload-config`, etc.).
/// For `stm start`, use [`daemon_start`] instead — it avoids capturing stdout/stderr via
/// pipes, which would be held open by the spawned daemon process.
pub fn stm(pipe: &str) -> Command {
    let mut cmd = Command::cargo_bin("stm").expect("stm binary should be built by cargo test");
    cmd.env(PIPE_ENV, pipe).timeout(COMMAND_TIMEOUT);
    cmd
}

/// Run `stm start` without capturing stdout/stderr.
///
/// `stm start` spawns `stmd.exe` as a child process. If stdout/stderr were captured via
/// pipes (as `assert_cmd` does), the daemon would inherit the write end of those pipes
/// and keep them open after `stm` exits, preventing the test from reading EOF. Using
/// `.status()` avoids pipe capture entirely.
///
/// # Panics
///
/// Panics if the `stm` binary cannot be found, the process fails to spawn, the command
/// exits with a non-zero status, or the timeout expires.
pub fn daemon_start(pipe: &str) {
    let exe = assert_cmd::cargo_bin!("stm");

    let mut child = std::process::Command::new(&exe)
        .arg("start")
        .env(PIPE_ENV, pipe)
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn stm: {e}"));

    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child
            .try_wait()
            .unwrap_or_else(|e| panic!("failed to wait on stm: {e}"))
        {
            Some(status) => {
                assert!(
                    status.success(),
                    "stm start exited with {status} (pipe={pipe})"
                );
                return;
            }
            None => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "stm start timed out after {COMMAND_TIMEOUT:?} (pipe={pipe})"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Ensure the daemon for the given pipe is stopped, ignoring errors if it was not running.
///
/// Call this at the start and end of each test to guarantee a clean slate
/// and avoid leaving a daemon process running after a test failure.
pub fn ensure_daemon_stopped(pipe: &str) {
    let _ = stm(pipe).arg("stop").assert();
}
