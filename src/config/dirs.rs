//! Config directory resolution with a priority chain.
//!
//! The flow daemon and CLI need a consistent location for user configuration
//! files (`flow.toml` for app settings, `flow-rules.toml` for window rules, and
//! JSON Schema files). This module resolves that directory at runtime using a
//! three-level priority chain:
//!
//! 1. **CLI override** — `--config <dir>` flag passed by the user.
//! 2. **Environment variable** — `FLOW_CONFIG_DIR` set by the daemon launcher
//!    or the user's shell profile.
//! 3. **Default** — `%USERPROFILE%\.config\flow\` (Linux-style `~/.config/flow/`
//!    adapted for Windows).
//!
//! # Why `~/.config/flow/` instead of `%APPDATA%\flow\`?
//!
//! The original implementation used `%APPDATA%`, but this has two drawbacks:
//! - `%APPDATA%` points into a hidden `AppData\Roaming` directory that most
//!   users never browse. Discovering config files requires digging through
//!   hidden folders.
//! - `%APPDATA%` paths are long and verbose, making them cumbersome to type
//!   in documentation or terminal commands.
//!
//! `%USERPROFILE%\.config\flow\` follows the XDG Base Directory convention
//! (`$XDG_CONFIG_HOME/appname/`), which is well-known to developers and
//! increasingly expected on all platforms. On Windows, `%USERPROFILE%` is
//! always set (e.g., `C:\Users\<username>`).
//!
//! # Graceful Degradation
//!
//! If `%USERPROFILE%` is somehow unset (e.g., a broken service account), the
//! module falls back to `%APPDATA%` (stripping the `\AppData\Roaming` suffix),
//! and ultimately to the current working directory `"."`. All fallbacks are
//! logged so operators can diagnose configuration issues.

use std::path::{Path, PathBuf};

/// Environment variable name for overriding the config directory.
///
/// When set, [`resolve_config_dir`] uses this value as the config directory,
/// bypassing the default `%USERPROFILE%\.config\flow\` path.
///
/// # Examples
///
/// ```text
/// set FLOW_CONFIG_DIR=C:\Users\alice\my-flow-config
/// flow start
/// ```
pub const CONFIG_DIR_ENV: &str = "FLOW_CONFIG_DIR";

/// Resolve the config directory using the priority chain.
///
/// This is the core resolution function. It determines the config directory
/// in this order:
///
/// 1. If `cli_override` is `Some(path)`, that path is used directly.
/// 2. If the [`CONFIG_DIR_ENV`] (`FLOW_CONFIG_DIR`) environment variable is set,
///    its value is used.
/// 3. Otherwise, `%USERPROFILE%\.config\flow\` is used as the default.
///
/// After resolution, the directory (and all parent directories) is created
/// if it does not already exist. A warning is logged if directory creation
/// fails (e.g., permission denied), but the function still returns the path.
///
/// # Arguments
///
/// * `cli_override` — An optional path from the `--config` CLI flag.
///   When provided, this takes absolute precedence over all other sources.
///
/// # Returns
///
/// The resolved config directory as a [`PathBuf`].
///
/// # Examples
///
/// ```ignore
/// use flow_wm::config::dirs::resolve_config_dir;
/// use std::path::Path;
///
/// // With a CLI override:
/// let dir = resolve_config_dir(Some(Path::new("C:\\custom\\flow")));
/// assert!(dir.ends_with("flow"));
///
/// // Without override (uses env var or default):
/// let dir = resolve_config_dir(None);
/// assert!(dir.ends_with("flow"));
/// ```
#[must_use]
pub fn resolve_config_dir(cli_override: Option<&Path>) -> PathBuf {
    let (dir, source) = if let Some(override_path) = cli_override {
        (override_path.to_path_buf(), "CLI --config flag")
    } else if let Ok(env_val) = std::env::var(CONFIG_DIR_ENV) {
        (PathBuf::from(&env_val), "FLOW_CONFIG_DIR env var")
    } else {
        (default_config_dir(), "default (USERPROFILE/.config/flow)")
    };

    // Ensure the directory exists so users can create config files without
    // manually creating the directory first.
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!(
            "could not create config dir {:?} (source: {source}): {e}",
            dir
        );
    }

    log::info!("config dir resolved from {source}: {:?}", dir);
    dir
}

/// Resolve the config directory with no override.
///
/// Convenience wrapper around [`resolve_config_dir`] that passes `None`,
/// meaning the CLI override step is skipped. This is the function used by the
/// daemon when no `--config` flag was provided.
///
/// # Returns
///
/// The resolved config directory as a [`PathBuf`].
///
/// # Examples
///
/// ```ignore
/// use flow_wm::config::dirs::config_dir;
/// let dir = config_dir();
/// assert!(dir.ends_with("flow"));
/// ```
#[must_use]
pub fn config_dir() -> PathBuf {
    resolve_config_dir(None)
}

/// Returns the path to the user's `flow-rules.toml` file using the default
/// config directory resolution.
///
/// The path is resolved via [`resolve_config_dir`]`(None)`, then `flow-rules.toml`
/// is appended. This is the file where users define window classification rules
/// and default actions.
///
/// # Returns
///
/// Full path to `flow-rules.toml` as a [`PathBuf`].
///
/// # Examples
///
/// ```ignore
/// use flow_wm::config::dirs::user_rules_path;
/// let path = user_rules_path();
/// assert!(path.ends_with("flow-rules.toml"));
/// assert!(path.to_string_lossy().contains("flow"));
/// ```
#[must_use]
pub fn user_rules_path() -> PathBuf {
    resolve_config_dir(None).join("flow-rules.toml")
}

/// Returns the path to the user's `flow-rules.toml` file in an explicitly
/// provided config directory.
///
/// This is used when the config directory has already been resolved
/// elsewhere (e.g., via a CLI flag) and the caller wants the rules path
/// without re-resolving.
///
/// # Arguments
///
/// * `dir` — The config directory to use. Typically from [`resolve_config_dir`].
///
/// # Returns
///
/// `dir.join("flow-rules.toml")` as a [`PathBuf`].
///
/// # Examples
///
/// ```ignore
/// use flow_wm::config::dirs::user_rules_path_in;
/// use std::path::Path;
///
/// let custom = Path::new("C:\\my-config");
/// let path = user_rules_path_in(custom);
/// assert_eq!(path, custom.join("flow-rules.toml"));
/// ```
#[must_use]
pub fn user_rules_path_in(dir: &Path) -> PathBuf {
    dir.join("flow-rules.toml")
}

/// Returns the path to the user's `history-flow-rules.toml` file using the default
/// config directory resolution.
///
/// The path is resolved via [`resolve_config_dir`]`(None)`, then
/// `history-flow-rules.toml` is appended. This file stores machine-learned
/// window classification rules (see [`crate::config::history::HistoryStore`]).
///
/// # Returns
///
/// Full path to `history-flow-rules.toml` as a [`PathBuf`].
#[must_use]
pub fn history_rules_path() -> PathBuf {
    resolve_config_dir(None).join("history-flow-rules.toml")
}

/// Returns the path to the user's `history-flow-rules.toml` file in an explicitly
/// provided config directory.
///
/// This is used when the config directory has already been resolved
/// elsewhere (e.g., via a CLI flag) and the caller wants the history rules path
/// without re-resolving.
///
/// # Arguments
///
/// * `dir` — The config directory to use. Typically from [`resolve_config_dir`].
///
/// # Returns
///
/// `dir.join("history-flow-rules.toml")` as a [`PathBuf`].
#[must_use]
pub fn history_rules_path_in(dir: &Path) -> PathBuf {
    dir.join("history-flow-rules.toml")
}

/// Returns the path to the user's `flow.toml` app config file in an explicitly
/// provided config directory.
///
/// This is used when the config directory has already been resolved
/// elsewhere (e.g., via a CLI flag) and the caller wants the app config path
/// without re-resolving.
///
/// # Arguments
///
/// * `dir` — The config directory to use. Typically from [`resolve_config_dir`].
///
/// # Returns
///
/// `dir.join("flow.toml")` as a [`PathBuf`].
///
/// # Examples
///
/// ```ignore
/// use flow_wm::config::dirs::user_app_config_path_in;
/// use std::path::Path;
///
/// let custom = Path::new("C:\\my-config");
/// let path = user_app_config_path_in(custom);
/// assert_eq!(path, custom.join("flow.toml"));
/// ```
#[must_use]
pub fn user_app_config_path_in(dir: &Path) -> PathBuf {
    dir.join("flow.toml")
}

/// Returns the path to the user's `flow.toml` app config file using the default
/// config directory resolution.
///
/// The path is resolved via [`config_dir`]`, then `flow.toml` is appended.
/// This file contains application settings such as padding and
/// animation preferences.
///
/// # Returns
///
/// Full path to `flow.toml` as a [`PathBuf`].
///
/// # Examples
///
/// ```ignore
/// use flow_wm::config::dirs::user_app_config_path;
/// let path = user_app_config_path();
/// assert!(path.ends_with("flow.toml"));
/// ```
#[must_use]
pub fn user_app_config_path() -> PathBuf {
    config_dir().join("flow.toml")
}

// ── Logs directory and date-stamped log file paths ─────────────────
//
// Logs are co-located with the config directory (under `<config_dir>/logs/`)
// rather than under `%LOCALAPPDATA%\flow\logs\`. This matches the
// discoverability decision made by [`resolve_config_dir`]: users can find
// and `tail` their logs without digging through hidden `AppData` folders.
// The same rationale that rejected `%APPDATA%` for config applies doubly to
// logs, which users want to inspect frequently while debugging.
//
// The daemon writes one log file per day, named `flowd-YYYY-MM-DD.log`,
// opened in **append** mode so multiple daemon starts on the same day
// accumulate into a single file. No automatic rotation or deletion is
// performed — all historical logs are preserved. The `date` string is
// computed by the logging module (via Win32 `GetLocalTime`) and passed in
// here, keeping this module pure and free of Win32 dependencies.

/// Returns the logs subdirectory path inside an explicitly provided config
/// directory.
///
/// # Arguments
///
/// * `dir` — The config directory to use. Typically from [`resolve_config_dir`].
///
/// # Returns
///
/// `dir.join("logs")` as a [`PathBuf`].
///
/// # Examples
///
/// ```ignore
/// use flow_wm::config::dirs::logs_dir_in;
/// use std::path::Path;
///
/// let logs = logs_dir_in(Path::new("C:\\flow"));
/// assert!(logs.ends_with("logs"));
/// ```
#[must_use]
pub fn logs_dir_in(dir: &Path) -> PathBuf {
    dir.join("logs")
}

/// Returns the logs subdirectory path using the default config directory
/// resolution (CLI flag → `FLOW_CONFIG_DIR` → `%USERPROFILE%\.config\flow\`).
///
/// Convenience wrapper around [`logs_dir_in`] that resolves the config
/// directory first via [`resolve_config_dir`]`(None)`.
///
/// # Returns
///
/// Full path to the logs directory as a [`PathBuf`].
#[must_use]
pub fn logs_dir() -> PathBuf {
    logs_dir_in(&resolve_config_dir(None))
}

/// Returns the path to a date-stamped daemon log file inside an explicitly
/// provided config directory.
///
/// The `date` parameter must be a `YYYY-MM-DD` string (e.g. `"2026-06-17"`).
/// The caller is responsible for computing it in the local timezone — this
/// function is pure and does no date arithmetic, which keeps it trivially
/// testable and free of Win32 dependencies.
///
/// # Arguments
///
/// * `dir` — The config directory to use. Typically from [`resolve_config_dir`].
/// * `date` — A `YYYY-MM-DD` date string for the desired log file.
///
/// # Returns
///
/// `logs_dir_in(dir).join(format!("flowd-{date}.log"))` as a [`PathBuf`].
///
/// # Examples
///
/// ```ignore
/// use flow_wm::config::dirs::log_file_path_in;
/// use std::path::Path;
///
/// let path = log_file_path_in(Path::new("C:\\flow"), "2026-06-17");
/// assert!(path.ends_with("flowd-2026-06-17.log"));
/// ```
#[must_use]
pub fn log_file_path_in(dir: &Path, date: &str) -> PathBuf {
    logs_dir_in(dir).join(format!("flowd-{date}.log"))
}

/// Compute the default config directory: `%USERPROFILE%\.config\flow\`.
///
/// Falls back to `%APPDATA%` with `\AppData\Roaming` stripped, and ultimately
/// to `"."` (current directory) if neither environment variable is set.
///
/// This is a private implementation detail — call [`resolve_config_dir`]
/// instead.
fn default_config_dir() -> PathBuf {
    // Primary: USERPROFILE (always set on normal Windows user accounts).
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        return PathBuf::from(userprofile).join(".config").join("flow");
    }

    // Fallback: APPDATA (strip the trailing \AppData\Roaming).
    if let Ok(appdata) = std::env::var("APPDATA") {
        let base = PathBuf::from(&appdata);
        // APPDATA typically ends with "AppData\Roaming". We want the user
        // profile root, so we strip the last two components. If the path is
        // shorter than expected, we use it as-is.
        let stripped = base
            .parent() // -> ...\AppData
            .and_then(|p| p.parent()); // -> C:\Users\<name>
        let user_home = match stripped {
            Some(p) => p.to_path_buf(),
            None => base,
        };
        log::warn!(
            "USERPROFILE not set; falling back to APPDATA-derived path {:?}",
            user_home
        );
        return user_home.join(".config").join("flow");
    }

    // Last resort: current directory.
    log::warn!("neither USERPROFILE nor APPDATA is set; falling back to '.' for config dir");
    PathBuf::from(".").join("flow")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module-level Mutex that serializes all tests that mutate `FLOW_CONFIG_DIR`.
    ///
    /// `cargo test` runs tests in parallel within a module. Tests that call
    /// `std::env::set_var` / `remove_var` on the same variable race with each
    /// other non-deterministically — one test may read the variable mid-mutation
    /// by another. This lock ensures only one env-mutating test runs at a time.
    ///
    /// Usage: `let _guard = ENV_LOCK.lock().unwrap();` at the start of any test
    /// that temporarily alters `CONFIG_DIR_ENV`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // ── Existing tests (updated for new paths) ─────────────────────────

    /// Positive: `user_rules_path` ends with the expected filename.
    #[test]
    fn user_rules_path_ends_with_flow_rules_toml() {
        let path = user_rules_path();
        assert!(path.ends_with("flow-rules.toml"), "path was: {path:?}");
    }

    /// Positive: `user_rules_path` contains `flow` in its path components.
    #[test]
    fn user_rules_path_is_under_flow_subdir() {
        let path = user_rules_path();
        assert!(
            path.to_string_lossy().contains("flow"),
            "path should contain 'flow' subdir: {path:?}"
        );
    }

    /// Positive: the parent of the rules path should be named "flow".
    #[test]
    fn user_rules_path_parent_is_flow() {
        let path = user_rules_path();
        assert_eq!(
            path.parent().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("flow")),
            "parent dir should be 'flow': {path:?}"
        );
    }

    /// Positive: `user_rules_path` returns a [`PathBuf`].
    #[test]
    fn user_rules_path_returns_pathbuf() {
        let path: PathBuf = user_rules_path();
        assert!(!path.as_os_str().is_empty(), "path should not be empty");
    }

    /// Positive: the filename component is exactly "flow-rules.toml".
    #[test]
    fn user_rules_path_filename_is_flow_rules_toml() {
        let path = user_rules_path();
        assert_eq!(
            path.file_name(),
            Some(std::ffi::OsStr::new("flow-rules.toml")),
            "filename should be flow-rules.toml: {path:?}"
        );
    }

    // ── Priority chain tests ───────────────────────────────────────────

    /// Positive: CLI override is used when provided.
    #[test]
    fn resolve_config_dir_with_override_uses_override() {
        let tmp = tempfile::tempdir().unwrap();
        let custom = tmp.path().join("my-custom-config");
        let dir = resolve_config_dir(Some(custom.as_path()));
        assert_eq!(dir, custom, "CLI override should be used directly");
    }

    /// Positive: `FLOW_CONFIG_DIR` env var is used when no CLI override.
    ///
    /// We temporarily set the env var for this test and restore the original
    /// value afterward to avoid polluting the test environment.
    #[test]
    fn resolve_config_dir_without_override_uses_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let original = std::env::var(CONFIG_DIR_ENV).ok();
        let tmp = tempfile::tempdir().unwrap();
        let expected = tmp.path().join("flow");
        unsafe { std::env::set_var(CONFIG_DIR_ENV, expected.as_os_str()) };
        let dir = resolve_config_dir(None);
        // Restore
        match original {
            Some(val) => unsafe { std::env::set_var(CONFIG_DIR_ENV, val) },
            None => unsafe { std::env::remove_var(CONFIG_DIR_ENV) },
        }
        assert_eq!(
            dir, expected,
            "env var should override default"
        );
    }

    /// Positive: default path is used when neither override nor env var is set.
    #[test]
    fn resolve_config_dir_without_anything_uses_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Ensure env var is not set.
        let original = std::env::var(CONFIG_DIR_ENV).ok();
        unsafe { std::env::remove_var(CONFIG_DIR_ENV) };
        let dir = resolve_config_dir(None);
        // Restore
        if let Some(val) = original {
            unsafe { std::env::set_var(CONFIG_DIR_ENV, val) }
        }
        // The path should end with flow and contain .config
        let path_str = dir.to_string_lossy();
        assert!(
            path_str.contains(".config") && path_str.ends_with("flow"),
            "default path should be …/.config/flow: {dir:?}"
        );
    }

    // ── Path helper tests ──────────────────────────────────────────────

    /// Negative: CLI override takes precedence even when `FLOW_CONFIG_DIR` is set.
    ///
    /// This tests the priority ordering: `--config` flag > env var. Both are set,
    /// and the CLI override path must win.
    #[test]
    fn resolve_config_dir_cli_override_beats_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Arrange: set FLOW_CONFIG_DIR to a different directory.
        let original = std::env::var(CONFIG_DIR_ENV).ok();
        let env_tmp = tempfile::tempdir().unwrap();
        let env_val = env_tmp.path().join("flow");
        unsafe { std::env::set_var(CONFIG_DIR_ENV, env_val.as_os_str()) };

        // Act: resolve with a CLI override that differs from the env var.
        let cli_tmp = tempfile::tempdir().unwrap();
        let cli_path = cli_tmp.path().join("flow");
        let dir = resolve_config_dir(Some(cli_path.as_path()));

        // Restore env.
        match original {
            Some(val) => unsafe { std::env::set_var(CONFIG_DIR_ENV, val) },
            None => unsafe { std::env::remove_var(CONFIG_DIR_ENV) },
        }

        // Assert: CLI override is returned, not the env var path.
        assert_eq!(
            dir, cli_path,
            "CLI override must win over FLOW_CONFIG_DIR env var"
        );
        assert_ne!(
            dir, env_val,
            "env var path must not be returned when CLI override is given"
        );
    }

    /// Positive: `user_app_config_path` returns a path ending with `flow.toml`.
    #[test]
    fn user_app_config_path_returns_flow_toml() {
        let path = user_app_config_path();
        assert!(path.ends_with("flow.toml"), "path was: {path:?}");
    }

    /// Positive: `user_rules_path_in` appends correctly.
    #[test]
    fn user_rules_path_in_returns_correct_path() {
        let dir = Path::new("C:\\test\\config");
        let path = user_rules_path_in(dir);
        assert_eq!(path, dir.join("flow-rules.toml"));
    }

    /// Positive: `user_app_config_path_in` appends correctly.
    #[test]
    fn user_app_config_path_in_returns_correct_path() {
        let dir = Path::new("C:\\test\\config");
        let path = user_app_config_path_in(dir);
        assert_eq!(path, dir.join("flow.toml"));
    }

    // ── Logs path tests ────────────────────────────────────────────────

    /// Positive: `logs_dir_in` appends "logs" to the given directory.
    #[test]
    fn logs_dir_in_appends_logs_subdir() {
        let dir = Path::new("C:\\test\\config");
        let logs = logs_dir_in(dir);
        assert_eq!(logs, dir.join("logs"));
    }

    /// Positive: `logs_dir` (default resolution) ends with "logs".
    #[test]
    fn logs_dir_ends_with_logs() {
        let logs = logs_dir();
        assert!(logs.ends_with("logs"), "logs dir was: {logs:?}");
    }

    /// Positive: `log_file_path_in` produces the expected dated filename.
    #[test]
    fn log_file_path_in_produces_dated_filename() {
        let dir = Path::new("C:\\test\\config");
        let path = log_file_path_in(dir, "2026-06-17");
        assert!(path.ends_with("flowd-2026-06-17.log"), "path was: {path:?}");
    }

    /// Positive: `log_file_path_in` nests the file directly under the logs dir.
    #[test]
    fn log_file_path_in_nests_under_logs() {
        let dir = Path::new("C:\\test\\config");
        let path = log_file_path_in(dir, "2026-06-17");
        assert_eq!(
            path.parent(),
            Some(logs_dir_in(dir).as_path()),
            "log file should be directly under the logs dir"
        );
    }

    /// Positive: different dates produce different filenames in the same dir.
    #[test]
    fn log_file_path_in_distinguishes_dates() {
        let dir = Path::new("C:\\test\\config");
        let a = log_file_path_in(dir, "2026-06-17");
        let b = log_file_path_in(dir, "2026-06-18");
        assert_ne!(a, b, "different dates must produce different paths");
    }

    // ── History rules path tests ─────────────────────────────────────────

    /// Positive: `history_rules_path` ends with the expected filename.
    #[test]
    fn history_rules_path_ends_with_history_flow_rules_toml() {
        let path = history_rules_path();
        assert!(
            path.ends_with("history-flow-rules.toml"),
            "path was: {path:?}"
        );
    }

    /// Positive: `history_rules_path_in` appends correctly.
    #[test]
    fn history_rules_path_in_returns_correct_path() {
        let dir = Path::new("C:\\test\\config");
        let path = history_rules_path_in(dir);
        assert_eq!(path, dir.join("history-flow-rules.toml"));
    }
}
