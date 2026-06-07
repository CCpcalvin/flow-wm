//! Configuration parser and JSON Schema generation.
//!
//! Defines two YAML config file structures:
//!
//! - [`StmConfig`] — Application settings (hotkeys, padding, animation).
//!   Loaded from `%APPDATA%\stm\stm.yml`.
//!
//! - [`WindowRulesConfig`] — Window classification rules and default action.
//!   Loaded from `%APPDATA%\stm\stm-rules.yml`.
//!
//! # How Config Reaches the Layout Engine
//!
//! The config is not passed directly to [`LayoutEngine`](crate::layout::LayoutEngine).
//! Instead, the daemon extracts the relevant fields into a
//! [`MutationConfig`](crate::layout::mutations::MutationConfig) which the layout
//! engine uses for all size calculations. This keeps the layout engine decoupled
//! from config parsing details.
//!
//! # Padding Model
//!
//! Config defines [`types::Padding`] with three fields:
//!
//! - `window` — inset around each window within its cell (the visual gap you see)
//! - `up` — top screen margin so windows don't touch the top edge
//! - `down` — bottom screen margin (e.g., for taskbar clearance)
//!
//! Padding is applied during projection, not stored in window structs. See the
//! [`crate::layout::projection`] module for details.

pub mod schema;
pub mod types;

pub use types::StmConfig;
pub use types::WindowRulesConfig;

/// Platform-specific directory paths for config files.
pub mod dirs;

/// Load window rules config from a YAML file.
///
/// If the file doesn't exist, returns the default (empty rules, `default_action: tile`).
/// If the file exists but is malformed, logs an error and returns the default.
/// This function never panics — it is designed for daemon startup where a bad
/// rules file should not prevent the daemon from running.
///
/// # Arguments
///
/// * `path` - Path to the `stm-rules.yml` file.
///
/// # Returns
///
/// A [`WindowRulesConfig`]. On success, the parsed file contents. On any error
/// (file not found, parse error, I/O error), returns the default config.
pub fn load_rules_config(path: &std::path::Path) -> WindowRulesConfig {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_yaml::from_str::<WindowRulesConfig>(&contents) {
            Ok(config) => {
                log::info!("loaded window rules from {:?}", path);
                config
            }
            Err(e) => {
                log::error!(
                    "failed to parse rules config {:?}: {e}; using defaults",
                    path
                );
                WindowRulesConfig::default()
            }
        },
        Err(e) => {
            log::debug!("rules config not found at {:?}: {e}; using defaults", path);
            WindowRulesConfig::default()
        }
    }
}

/// Load default rules from `default-stm-rules.yml` bundled next to the executable.
///
/// Looks for the file in the same directory as the running executable. If the
/// file doesn't exist, returns empty rules. This is **not an error** — the
/// binary may not ship with a default rules file.
///
/// # Returns
///
/// A [`WindowRulesConfig`] with whatever default rules were found, or an empty
/// config if the file doesn't exist.
pub fn load_default_rules() -> WindowRulesConfig {
    let exe_dir = match std::env::current_exe() {
        Ok(exe) => exe.parent().map(|p| p.to_path_buf()),
        Err(e) => {
            log::debug!("cannot determine exe directory: {e}");
            None
        }
    };

    let Some(dir) = exe_dir else {
        return WindowRulesConfig::default();
    };

    let path = dir.join("default-stm-rules.yml");
    if !path.exists() {
        log::debug!("no default rules file at {:?}", path);
        return WindowRulesConfig::default();
    }

    load_rules_config(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // ── load_rules_config tests ────────────────────────────────────────

    /// Positive: valid YAML file parses into the expected `WindowRulesConfig`.
    #[test]
    fn load_rules_config_valid_file_parses_correctly() {
        let yaml = r#"
default_action: float
rules:
  - match:
      exe: "explorer.exe"
      title_contains: "Open"
    action: ignore
  - match:
      class: "Chrome_WidgetWin_1"
    action: tile
    initial_width_eighths: 4
"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();

        let config = load_rules_config(f.path());
        assert_eq!(config.default_action, types::WindowAction::Float);
        assert_eq!(config.rules.len(), 2);
        assert_eq!(config.rules[0].action, types::WindowAction::Ignore);
        assert_eq!(config.rules[1].initial_width_eighths, Some(4));
    }

    /// Negative: missing file returns default config (not panic, not error).
    #[test]
    fn load_rules_config_missing_file_returns_default() {
        let path = std::path::PathBuf::from("C:\\__nonexistent_test_path__\\stm-rules.yml");
        let config = load_rules_config(&path);
        assert_eq!(config.default_action, types::WindowAction::Tile);
        assert!(config.rules.is_empty());
    }

    /// Negative: malformed YAML returns default config (not panic).
    #[test]
    fn load_rules_config_malformed_yaml_returns_default() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"this is not: valid: yaml: [[[[").unwrap();

        let config = load_rules_config(f.path());
        assert_eq!(config.default_action, types::WindowAction::Tile);
        assert!(config.rules.is_empty());
    }

    /// Negative: empty JSON-like file `{}` returns default config.
    #[test]
    fn load_rules_config_empty_yaml_object_returns_default() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"{}").unwrap();

        let config = load_rules_config(f.path());
        assert_eq!(config.default_action, types::WindowAction::Tile);
        assert!(config.rules.is_empty());
    }

    /// Positive: valid YAML with regex fields round-trips through file I/O.
    #[test]
    fn load_rules_config_roundtrips_regex_fields() {
        let config = types::WindowRulesConfig {
            default_action: types::WindowAction::Ignore,
            rules: vec![types::WindowRule {
                match_: types::MatchRule {
                    exe_regex: Some("chrome\\.exe".into()),
                    class_regex: Some("Chrome.*".into()),
                    process_path_regex: Some(".*\\\\Chrome\\\\.*".into()),
                    ..Default::default()
                },
                action: types::WindowAction::Tile,
                initial_width_eighths: None,
                override_persist: false,
            }],
        };

        let yaml = serde_yaml::to_string(&config).unwrap();
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();

        let loaded = load_rules_config(f.path());
        assert_eq!(loaded.default_action, types::WindowAction::Ignore);
        assert_eq!(loaded.rules.len(), 1);
        assert_eq!(
            loaded.rules[0].match_.exe_regex,
            Some("chrome\\.exe".into())
        );
    }

    // ── load_default_rules tests ───────────────────────────────────────

    /// Negative: `load_default_rules()` does not panic regardless of whether
    /// `default-stm-rules.yml` exists next to the test binary.
    ///
    /// The exe directory in test environments (`target\debug\deps\`) will not
    /// have the bundled rules file, so this exercises the "file not found →
    /// default" path. We do not assert content because CI environments with
    /// the file deployed alongside the binary would see different values.
    #[test]
    fn load_default_rules_no_file_returns_default() {
        // Only verify it does not panic; content depends on test environment.
        let _config = load_default_rules();
    }

    // ── default-stm-rules.yml parse test ───────────────────────────────

    /// Positive: the bundled `default-stm-rules.yml` in the project root
    /// parses correctly as `WindowRulesConfig`.
    ///
    /// This catches syntax errors or schema drift in the shipped defaults.
    #[test]
    fn default_stm_rules_yml_parses_correctly() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR should be set during tests");
        let path = std::path::PathBuf::from(manifest_dir).join("default-stm-rules.yml");

        // Only run if the file exists (it should in the project tree).
        if !path.exists() {
            eprintln!("skipping: default-stm-rules.yml not found at {path:?}");
            return;
        }

        let config = load_rules_config(&path);
        assert_eq!(config.default_action, types::WindowAction::Tile);
        assert!(
            !config.rules.is_empty(),
            "bundled rules should not be empty"
        );

        // Spot-check a well-known rule: taskbar should be ignored.
        let taskbar_rule = config
            .rules
            .iter()
            .find(|r| r.match_.class.as_deref() == Some("Shell_TrayWnd"));
        assert!(
            taskbar_rule.is_some(),
            "bundled rules should include a Shell_TrayWnd rule"
        );
        assert_eq!(taskbar_rule.unwrap().action, types::WindowAction::Ignore);
    }
}
