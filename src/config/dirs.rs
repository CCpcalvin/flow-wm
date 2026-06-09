//! Config directory resolution with a priority chain.
//!
//! The STM daemon and CLI need a consistent location for user configuration
//! files (`stm.toml` for app settings, `stm-rules.toml` for window rules, and
//! JSON Schema files). This module resolves that directory at runtime using a
//! three-level priority chain:
//!
//! 1. **CLI override** — `--config <dir>` flag passed by the user.
//! 2. **Environment variable** — `STM_CONFIG_DIR` set by the daemon launcher
//!    or the user's shell profile.
//! 3. **Default** — `%USERPROFILE%\.config\stm\` (Linux-style `~/.config/stm/`
//!    adapted for Windows).
//!
//! # Why `~/.config/stm/` instead of `%APPDATA%\stm\`?
//!
//! The original implementation used `%APPDATA%`, but this has two drawbacks:
//! - `%APPDATA%` points into a hidden `AppData\Roaming` directory that most
//!   users never browse. Discovering config files requires digging through
//!   hidden folders.
//! - `%APPDATA%` paths are long and verbose, making them cumbersome to type
//!   in documentation or terminal commands.
//!
//! `%USERPROFILE%\.config\stm\` follows the XDG Base Directory convention
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
/// bypassing the default `%USERPROFILE%\.config\stm\` path.
///
/// # Examples
///
/// ```text
/// set STM_CONFIG_DIR=C:\Users\alice\my-stm-config
/// stm start
/// ```
pub const CONFIG_DIR_ENV: &str = "STM_CONFIG_DIR";

/// Resolve the config directory using the priority chain.
///
/// This is the core resolution function. It determines the config directory
/// in this order:
///
/// 1. If `cli_override` is `Some(path)`, that path is used directly.
/// 2. If the [`CONFIG_DIR_ENV`] (`STM_CONFIG_DIR`) environment variable is set,
///    its value is used.
/// 3. Otherwise, `%USERPROFILE%\.config\stm\` is used as the default.
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
/// use scrolling_tiling_manager::config::dirs::resolve_config_dir;
/// use std::path::Path;
///
/// // With a CLI override:
/// let dir = resolve_config_dir(Some(Path::new("C:\\custom\\stm")));
/// assert!(dir.ends_with("stm"));
///
/// // Without override (uses env var or default):
/// let dir = resolve_config_dir(None);
/// assert!(dir.ends_with("stm"));
/// ```
#[must_use]
pub fn resolve_config_dir(cli_override: Option<&Path>) -> PathBuf {
    let (dir, source) = if let Some(override_path) = cli_override {
        (override_path.to_path_buf(), "CLI --config flag")
    } else if let Ok(env_val) = std::env::var(CONFIG_DIR_ENV) {
        (PathBuf::from(&env_val), "STM_CONFIG_DIR env var")
    } else {
        (default_config_dir(), "default (USERPROFILE/.config/stm)")
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
/// use scrolling_tiling_manager::config::dirs::config_dir;
/// let dir = config_dir();
/// assert!(dir.ends_with("stm"));
/// ```
#[must_use]
pub fn config_dir() -> PathBuf {
    resolve_config_dir(None)
}

/// Returns the path to the user's `stm-rules.toml` file using the default
/// config directory resolution.
///
/// The path is resolved via [`resolve_config_dir`]`(None)`, then `stm-rules.toml`
/// is appended. This is the file where users define window classification rules
/// and default actions.
///
/// # Returns
///
/// Full path to `stm-rules.toml` as a [`PathBuf`].
///
/// # Examples
///
/// ```ignore
/// use scrolling_tiling_manager::config::dirs::user_rules_path;
/// let path = user_rules_path();
/// assert!(path.ends_with("stm-rules.toml"));
/// assert!(path.to_string_lossy().contains("stm"));
/// ```
#[must_use]
pub fn user_rules_path() -> PathBuf {
    resolve_config_dir(None).join("stm-rules.toml")
}

/// Returns the path to the user's `stm-rules.toml` file in an explicitly
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
/// `dir.join("stm-rules.toml")` as a [`PathBuf`].
///
/// # Examples
///
/// ```ignore
/// use scrolling_tiling_manager::config::dirs::user_rules_path_in;
/// use std::path::Path;
///
/// let custom = Path::new("C:\\my-config");
/// let path = user_rules_path_in(custom);
/// assert_eq!(path, custom.join("stm-rules.toml"));
/// ```
#[must_use]
pub fn user_rules_path_in(dir: &Path) -> PathBuf {
    dir.join("stm-rules.toml")
}

/// Returns the path to the user's `stm.toml` app config file in an explicitly
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
/// `dir.join("stm.toml")` as a [`PathBuf`].
///
/// # Examples
///
/// ```ignore
/// use scrolling_tiling_manager::config::dirs::user_app_config_path_in;
/// use std::path::Path;
///
/// let custom = Path::new("C:\\my-config");
/// let path = user_app_config_path_in(custom);
/// assert_eq!(path, custom.join("stm.toml"));
/// ```
#[must_use]
pub fn user_app_config_path_in(dir: &Path) -> PathBuf {
    dir.join("stm.toml")
}

/// Returns the path to the user's `stm.toml` app config file using the default
/// config directory resolution.
///
/// The path is resolved via [`config_dir`]`, then `stm.toml` is appended.
/// This file contains application settings such as hotkeys, padding, and
/// animation preferences.
///
/// # Returns
///
/// Full path to `stm.toml` as a [`PathBuf`].
///
/// # Examples
///
/// ```ignore
/// use scrolling_tiling_manager::config::dirs::user_app_config_path;
/// let path = user_app_config_path();
/// assert!(path.ends_with("stm.toml"));
/// ```
#[must_use]
pub fn user_app_config_path() -> PathBuf {
    config_dir().join("stm.toml")
}

/// Compute the default config directory: `%USERPROFILE%\.config\stm\`.
///
/// Falls back to `%APPDATA%` with `\AppData\Roaming` stripped, and ultimately
/// to `"."` (current directory) if neither environment variable is set.
///
/// This is a private implementation detail — call [`resolve_config_dir`]
/// instead.
fn default_config_dir() -> PathBuf {
    // Primary: USERPROFILE (always set on normal Windows user accounts).
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        return PathBuf::from(userprofile).join(".config").join("stm");
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
        return user_home.join(".config").join("stm");
    }

    // Last resort: current directory.
    log::warn!("neither USERPROFILE nor APPDATA is set; falling back to '.' for config dir");
    PathBuf::from(".").join("stm")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module-level Mutex that serializes all tests that mutate `STM_CONFIG_DIR`.
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
    fn user_rules_path_ends_with_stm_rules_toml() {
        let path = user_rules_path();
        assert!(path.ends_with("stm-rules.toml"), "path was: {path:?}");
    }

    /// Positive: `user_rules_path` contains `stm` in its path components.
    #[test]
    fn user_rules_path_is_under_stm_subdir() {
        let path = user_rules_path();
        assert!(
            path.to_string_lossy().contains("stm"),
            "path should contain 'stm' subdir: {path:?}"
        );
    }

    /// Positive: the parent of the rules path should be named "stm".
    #[test]
    fn user_rules_path_parent_is_stm() {
        let path = user_rules_path();
        assert_eq!(
            path.parent().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("stm")),
            "parent dir should be 'stm': {path:?}"
        );
    }

    /// Positive: `user_rules_path` returns a [`PathBuf`].
    #[test]
    fn user_rules_path_returns_pathbuf() {
        let path: PathBuf = user_rules_path();
        assert!(!path.as_os_str().is_empty(), "path should not be empty");
    }

    /// Positive: the filename component is exactly "stm-rules.toml".
    #[test]
    fn user_rules_path_filename_is_stm_rules_toml() {
        let path = user_rules_path();
        assert_eq!(
            path.file_name(),
            Some(std::ffi::OsStr::new("stm-rules.toml")),
            "filename should be stm-rules.toml: {path:?}"
        );
    }

    // ── Priority chain tests ───────────────────────────────────────────

    /// Positive: CLI override is used when provided.
    #[test]
    fn resolve_config_dir_with_override_uses_override() {
        let custom = Path::new("C:\\my-custom-config");
        let dir = resolve_config_dir(Some(custom));
        assert_eq!(dir, custom, "CLI override should be used directly");
    }

    /// Positive: `STM_CONFIG_DIR` env var is used when no CLI override.
    ///
    /// We temporarily set the env var for this test and restore the original
    /// value afterward to avoid polluting the test environment.
    #[test]
    fn resolve_config_dir_without_override_uses_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let original = std::env::var(CONFIG_DIR_ENV).ok();
        unsafe { std::env::set_var(CONFIG_DIR_ENV, "C:\\env-config\\stm") };
        let dir = resolve_config_dir(None);
        // Restore
        match original {
            Some(val) => unsafe { std::env::set_var(CONFIG_DIR_ENV, val) },
            None => unsafe { std::env::remove_var(CONFIG_DIR_ENV) },
        }
        assert_eq!(
            dir,
            PathBuf::from("C:\\env-config\\stm"),
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
        match original {
            Some(val) => unsafe { std::env::set_var(CONFIG_DIR_ENV, val) },
            None => {}
        }
        // The path should end with stm and contain .config
        let path_str = dir.to_string_lossy();
        assert!(
            path_str.contains(".config") && path_str.ends_with("stm"),
            "default path should be …/.config/stm: {dir:?}"
        );
    }

    // ── Path helper tests ──────────────────────────────────────────────

    /// Negative: CLI override takes precedence even when `STM_CONFIG_DIR` is set.
    ///
    /// This tests the priority ordering: `--config` flag > env var. Both are set,
    /// and the CLI override path must win.
    #[test]
    fn resolve_config_dir_cli_override_beats_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Arrange: set STM_CONFIG_DIR to a different directory.
        let original = std::env::var(CONFIG_DIR_ENV).ok();
        unsafe { std::env::set_var(CONFIG_DIR_ENV, "C:\\env-config\\stm") };

        // Act: resolve with a CLI override that differs from the env var.
        let cli_path = Path::new("C:\\cli-override\\stm");
        let dir = resolve_config_dir(Some(cli_path));

        // Restore env.
        match original {
            Some(val) => unsafe { std::env::set_var(CONFIG_DIR_ENV, val) },
            None => unsafe { std::env::remove_var(CONFIG_DIR_ENV) },
        }

        // Assert: CLI override is returned, not the env var path.
        assert_eq!(
            dir, cli_path,
            "CLI override must win over STM_CONFIG_DIR env var"
        );
        assert_ne!(
            dir,
            PathBuf::from("C:\\env-config\\stm"),
            "env var path must not be returned when CLI override is given"
        );
    }

    /// Positive: `user_app_config_path` returns a path ending with `stm.toml`.
    #[test]
    fn user_app_config_path_returns_stm_toml() {
        let path = user_app_config_path();
        assert!(path.ends_with("stm.toml"), "path was: {path:?}");
    }

    /// Positive: `user_rules_path_in` appends correctly.
    #[test]
    fn user_rules_path_in_returns_correct_path() {
        let dir = Path::new("C:\\test\\config");
        let path = user_rules_path_in(dir);
        assert_eq!(path, dir.join("stm-rules.toml"));
    }

    /// Positive: `user_app_config_path_in` appends correctly.
    #[test]
    fn user_app_config_path_in_returns_correct_path() {
        let dir = Path::new("C:\\test\\config");
        let path = user_app_config_path_in(dir);
        assert_eq!(path, dir.join("stm.toml"));
    }
}
