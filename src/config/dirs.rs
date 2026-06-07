//! Platform-specific directory paths for stm config files.
//!
//! All user-facing config files live under `%APPDATA%\stm\` on Windows.
//! This module provides helper functions to resolve these paths at runtime,
//! falling back to sensible defaults when environment variables are unset.

/// Returns the path to the user's `stm-rules.yml` file.
///
/// Resolves to `%APPDATA%\stm\stm-rules.yml`. If the `APPDATA` environment
/// variable is not set (extremely rare on normal Windows systems), falls
/// back to `stm-rules.yml` in the current working directory and logs a
/// warning.
///
/// # Why `%APPDATA%`?
///
/// `%APPDATA%` (typically `C:\Users\<name>\AppData\Roaming`) is the standard
/// Windows location for per-user application configuration. Using it ensures:
/// - Config survives OS reinstalls when the user profile is preserved.
/// - Config roams with the user account on domain-joined machines.
/// - Consistency with other Windows applications.
#[must_use]
pub fn user_rules_path() -> std::path::PathBuf {
    app_data_dir().join("stm-rules.yml")
}

/// Returns the stm application data directory (`%APPDATA%\stm\`).
///
/// Creates the directory if it doesn't exist so that users can simply
/// create a new config file without needing to create the directory first.
fn app_data_dir() -> std::path::PathBuf {
    let base = match std::env::var("APPDATA") {
        Ok(path) => std::path::PathBuf::from(path),
        Err(_) => {
            log::warn!("APPDATA env var not set; falling back to current directory for config");
            std::path::PathBuf::from(".")
        }
    };

    let dir = base.join("stm");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!(
            "could not create config dir {:?}: {e}; user rules will not be loaded from this path",
            dir
        );
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_rules_path_ends_with_stm_rules_yml() {
        let path = user_rules_path();
        assert!(path.ends_with("stm-rules.yml"), "path was: {path:?}");
    }

    #[test]
    fn user_rules_path_is_under_stm_subdir() {
        let path = user_rules_path();
        assert!(
            path.to_string_lossy().contains("stm"),
            "path should contain 'stm' subdir: {path:?}"
        );
    }

    /// Positive: `app_data_dir` returns a path ending with `stm` when APPDATA is set.
    ///
    /// On Windows, APPDATA is always set to a valid directory. This test verifies
    /// that `app_data_dir` correctly appends the `stm` subdirectory.
    #[test]
    fn app_data_dir_ends_with_stm() {
        let dir = app_data_dir();
        assert!(
            dir.ends_with("stm"),
            "app_data_dir should end with 'stm': {dir:?}"
        );
    }

    /// Positive: `user_rules_path` is `app_data_dir()` joined with `stm-rules.yml`.
    ///
    /// Verifies that `user_rules_path` composes correctly from the base dir.
    #[test]
    fn user_rules_path_is_app_data_dir_plus_filename() {
        let base = app_data_dir();
        let full = user_rules_path();
        let expected = base.join("stm-rules.yml");
        assert_eq!(full, expected);
    }

    /// Positive: the `stm` config directory is created if it doesn't exist.
    ///
    /// `app_data_dir` calls `create_dir_all` — if the directory already exists
    /// (from a previous test or run), this is a no-op. This test just ensures
    /// no panic occurs.
    #[test]
    fn app_data_dir_does_not_panic() {
        let _dir = app_data_dir();
        // If we got here without panic, the function is safe.
    }

    /// Positive: `user_rules_path` returns a [`std::path::PathBuf`] (not a string).
    ///
    /// Trivial type check to ensure the API returns the expected type.
    #[test]
    fn user_rules_path_returns_pathbuf() {
        let path: std::path::PathBuf = user_rules_path();
        assert!(!path.as_os_str().is_empty(), "path should not be empty");
    }

    /// Negative: verify the path does NOT contain unexpected separators or components.
    #[test]
    fn user_rules_path_has_exactly_two_components_after_base() {
        let path = user_rules_path();
        // Path should be: <base>/stm/stm-rules.yml
        // The filename should be exactly "stm-rules.yml"
        assert_eq!(
            path.file_name(),
            Some(std::ffi::OsStr::new("stm-rules.yml")),
            "filename should be stm-rules.yml: {path:?}"
        );
        // The parent directory should be named "stm"
        assert_eq!(
            path.parent().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("stm")),
            "parent dir should be 'stm': {path:?}"
        );
    }
}
