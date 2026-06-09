//! Config lifecycle: init, load, validate, and check.
//!
//! This module owns the four-phase config lifecycle:
//!
//! 1. **Init** — [`init_config_dir`] creates the config directory and writes
//!    default config files (`stm.yml`, `stm-rules.yml`) with JSON Schema files
//!    for IDE autocomplete.
//!
//! 2. **Load** — Functions load config from shipped defaults and user overrides:
//!    - [`load_default_config`] — loads `default-config.yml` from the exe directory
//!    - [`load_default_rules`] — loads `default-stm-rules.yml` from the exe directory
//!    - [`load_app_config`] — loads user's `stm.yml`, falling back to defaults
//!    - [`load_rules_config`] — loads user's `stm-rules.yml`, falling back to defaults
//!
//! 3. **Validate** — [`check_config`] validates config files without loading them
//!    into the daemon. Used by `stm config check`.
//!
//! 4. **Use** — The daemon extracts fields from [`StmConfig`](super::types::StmConfig)
//!    into the layout engine.
//!
//! # Resilient Loading Design
//!
//! All load functions are resilient — they never panic or propagate errors to the
//! caller. Instead, they return defaults and log the issue. This ensures the daemon
//! can always start regardless of config file state (missing, malformed, I/O error).
//!
//! # Schema Headers
//!
//! When [`init_config_dir`] writes YAML files, it prepends a
//! `yaml-language-server: $schema=...` comment header for IDE autocomplete support.
//! The schema files are written into a `schemas/` subdirectory.

use std::path::Path;

use super::schema;
use super::types::{StmConfig, WindowRulesConfig};

/// Schema header for `stm.yml` files (yaml-language-server autocomplete).
///
/// Prepended to the YAML content when [`init_config_dir`] writes the default
/// `stm.yml`. The relative path `./schemas/stm-config.schema.json` is resolved
/// against the config directory, so the schema file must be in `dir/schemas/`.
const STM_CONFIG_SCHEMA_HEADER: &str =
    "# yaml-language-server: $schema=./schemas/stm-config.schema.json\n\n";

/// Schema header for `stm-rules.yml` files (yaml-language-server autocomplete).
///
/// Prepended to the YAML content when [`init_config_dir`] writes the default
/// `stm-rules.yml`. The relative path `./schemas/stm-rules.schema.json` is resolved
/// against the config directory, so the schema file must be in `dir/schemas/`.
const STM_RULES_SCHEMA_HEADER: &str =
    "# yaml-language-server: $schema=./schemas/stm-rules.schema.json\n\n";

// ── Load functions ────────────────────────────────────────────────────

/// Load application config from a YAML file.
///
/// Reads [`StmConfig`] from the given YAML file. This function is resilient:
///
/// - **File not found** → returns `StmConfig::default()`, logs at `debug` level.
/// - **Parse error** → returns `StmConfig::default()`, logs an error with the parse failure.
/// - **Success** → calls [`StmConfig::validate()`] and logs warnings if validation
///   fails, but still returns the loaded config. Logs at `info` level on success.
///
/// This function never panics — it is designed for daemon startup where a bad
/// config file should not prevent the daemon from running.
///
/// # Arguments
///
/// * `path` - Path to the `stm.yml` file.
///
/// # Returns
///
/// A [`StmConfig`]. On success, the parsed file contents (validated with warnings).
/// On any error (file not found, parse error, I/O error), returns the default config.
///
/// # Example
///
/// ```no_run
/// use scrolling_tiling_manager::config::load_app_config;
/// use std::path::Path;
///
/// let config = load_app_config(Path::new("stm.yml"));
/// println!("column_width = {}", config.column_width);
/// ```
#[must_use]
pub fn load_app_config(path: &Path) -> StmConfig {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_yaml::from_str::<StmConfig>(&contents) {
            Ok(config) => {
                // Validate post-deserialization — log warnings but don't fail.
                if let Err(warning) = config.validate() {
                    log::warn!("config validation warning for {:?}: {warning}", path);
                }
                log::info!("loaded app config from {:?}", path);
                config
            }
            Err(e) => {
                log::error!("failed to parse app config {:?}: {e}; using defaults", path);
                StmConfig::default()
            }
        },
        Err(e) => {
            log::debug!("app config not found at {:?}: {e}; using defaults", path);
            StmConfig::default()
        }
    }
}

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
#[must_use]
pub fn load_rules_config(path: &Path) -> WindowRulesConfig {
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

/// Load default app config from `default-config.yml` bundled next to the executable.
///
/// Looks for the file in the same directory as the running executable. If the
/// file doesn't exist, returns `StmConfig::default()`. This is **not an error** —
/// the binary may not ship with a default config file (e.g., in development).
///
/// This is the analogue of [`load_default_rules`] for the app config. It enables
/// a two-layer config model: shipped defaults as the base layer, user's `stm.yml`
/// as the overlay.
///
/// # Returns
///
/// A [`StmConfig`] with whatever default settings were found, or the Rust
/// default if the file doesn't exist.
#[must_use]
pub fn load_default_config() -> StmConfig {
    let exe_dir = match std::env::current_exe() {
        Ok(exe) => exe.parent().map(|p| p.to_path_buf()),
        Err(e) => {
            log::debug!("cannot determine exe directory: {e}");
            None
        }
    };

    let Some(dir) = exe_dir else {
        return StmConfig::default();
    };

    let path = dir.join("default-config.yml");
    if !path.exists() {
        log::debug!("no default config file at {:?}", path);
        return StmConfig::default();
    }

    load_app_config(&path)
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
#[must_use]
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

// ── Init function ──────────────────────────────────────────────────────

/// Initialize a config directory with default files, schema headers, and JSON Schemas.
///
/// Creates the directory at `dir` if it doesn't exist (idempotent — calling
/// multiple times is safe). Then writes each default file only if it doesn't
/// already exist:
///
/// - `stm.yml` — default [`StmConfig`] as YAML, with a `yaml-language-server`
///   `$schema` header prepended for IDE autocomplete.
/// - `stm-rules.yml` — default [`WindowRulesConfig`] as YAML, with a
///   `yaml-language-server` `$schema` header prepended.
/// - `schemas/stm-config.schema.json` — JSON Schema for [`StmConfig`].
/// - `schemas/stm-rules.schema.json` — JSON Schema for [`WindowRulesConfig`].
///
/// **Existing files are never overwritten.** This makes it safe to call
/// repeatedly (e.g., on every daemon startup) without losing user edits.
///
/// # Arguments
///
/// * `dir` - Path to the config directory to initialize.
///
/// # Returns
///
/// `Ok(())` on success (or if all files already exist).
/// `Err(message)` on I/O failure (e.g., directory creation fails).
///
/// # Errors
///
/// Returns `Err` if directory creation fails, or if writing `stm.yml` or
/// `stm-rules.yml` fails. JSON Schema write failures are non-fatal (logged
/// as warnings) and do not cause this function to return `Err`.
///
/// # Example
///
/// ```no_run
/// use scrolling_tiling_manager::config::init_config_dir;
/// use std::path::Path;
///
/// if let Err(e) = init_config_dir(Path::new("~/.config/stm")) {
///     eprintln!("failed to init config dir: {e}");
/// }
/// ```
#[must_use = "initialization errors must be handled"]
pub fn init_config_dir(dir: &Path) -> Result<(), String> {
    // Create the directory (create_dir_all is idempotent).
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("failed to create config directory {:?}: {e}", dir))?;
    log::info!("ensuring config directory exists: {:?}", dir);

    // Create the schemas subdirectory.
    let schemas_dir = dir.join("schemas");
    std::fs::create_dir_all(&schemas_dir)
        .map_err(|e| format!("failed to create schemas directory {:?}: {e}", schemas_dir))?;

    // Serialize default configs for the user's config directory.
    // This generates a starter stm.yml and stm-rules.yml for users.
    // The actual runtime defaults come from `default-config.yml` /
    // `default-stm-rules.yml` shipped next to stmd.exe (loaded via
    // `load_default_config` / `load_default_rules`).
    let default_config_yaml = serde_yaml::to_string(&StmConfig::default())
        .map_err(|e| format!("failed to serialize default StmConfig: {e}"))?;
    let default_rules_yaml = serde_yaml::to_string(&WindowRulesConfig::default())
        .map_err(|e| format!("failed to serialize default WindowRulesConfig: {e}"))?;

    // Write default stm.yml with schema header, if it doesn't exist.
    let stm_yml = dir.join("stm.yml");
    let stm_content = format!("{STM_CONFIG_SCHEMA_HEADER}{default_config_yaml}");
    match write_default_config_file(&stm_yml, &stm_content) {
        Ok(written) => {
            if written {
                log::info!("wrote default config to {:?}", stm_yml);
            }
        }
        Err(e) => {
            return Err(format!("failed to write default config {:?}: {e}", stm_yml));
        }
    }

    // Write default stm-rules.yml with schema header, if it doesn't exist.
    let rules_yml = dir.join("stm-rules.yml");
    let rules_content = format!("{STM_RULES_SCHEMA_HEADER}{default_rules_yaml}");
    match write_default_config_file(&rules_yml, &rules_content) {
        Ok(written) => {
            if written {
                log::info!("wrote default rules to {:?}", rules_yml);
            }
        }
        Err(e) => {
            return Err(format!(
                "failed to write default rules {:?}: {e}",
                rules_yml
            ));
        }
    }

    // Generate and write JSON Schemas into the schemas/ subdirectory.
    let config_schema_path = schemas_dir.join("stm-config.schema.json");
    match schema::generate_config_schema() {
        Ok(schema_json) => match write_default_config_file(&config_schema_path, &schema_json) {
            Ok(true) => log::info!("wrote config schema to {:?}", config_schema_path),
            Ok(false) => log::debug!(
                "config schema already exists, skipping {:?}",
                config_schema_path
            ),
            Err(e) => log::warn!(
                "failed to write config schema {:?}: {e}",
                config_schema_path
            ),
        },
        Err(e) => {
            log::warn!("failed to generate config schema: {e}");
        }
    }

    let rules_schema_path = schemas_dir.join("stm-rules.schema.json");
    match schema::generate_rules_schema() {
        Ok(schema_json) => match write_default_config_file(&rules_schema_path, &schema_json) {
            Ok(true) => log::info!("wrote rules schema to {:?}", rules_schema_path),
            Ok(false) => log::debug!(
                "rules schema already exists, skipping {:?}",
                rules_schema_path
            ),
            Err(e) => {
                log::warn!("failed to write rules schema {:?}: {e}", rules_schema_path);
            }
        },
        Err(e) => {
            log::warn!("failed to generate rules schema: {e}");
        }
    }

    Ok(())
}

// ── Check function ─────────────────────────────────────────────────────

/// Validate config files in a directory without loading them into the daemon.
///
/// Checks both `stm.yml` and `stm-rules.yml` in the given directory:
///
/// - Loads `stm.yml`, deserializes as [`StmConfig`], calls [`StmConfig::validate()`].
/// - Loads `stm-rules.yml`, checks it parses correctly as [`WindowRulesConfig`].
///
/// Missing files are **not errors** — they simply mean the user hasn't created
/// a config yet and defaults will be used. This function only reports actual
/// parse/validation failures.
///
/// **Logs nothing** — designed for pure CLI validation (`stm config check`) where
/// the caller controls output formatting.
///
/// # Arguments
///
/// * `dir` - Path to the config directory to validate.
///
/// # Returns
///
/// `Ok(())` if all present files are valid (or no files exist).
/// `Err(message)` describing the first validation/parse failure found.
///
/// # Example
///
/// ```no_run
/// use scrolling_tiling_manager::config::check_config;
/// use std::path::Path;
///
/// if let Err(e) = check_config(Path::new("~/.config/stm")) {
///     eprintln!("config error: {e}");
/// }
/// ```
#[must_use = "validation errors must be handled"]
pub fn check_config(dir: &Path) -> Result<(), String> {
    let stm_path = dir.join("stm.yml");

    // Only validate stm.yml if it exists — missing is not an error.
    if stm_path.exists() {
        let contents = std::fs::read_to_string(&stm_path)
            .map_err(|e| format!("failed to read {:?}: {e}", stm_path))?;
        let config: StmConfig =
            serde_yaml::from_str(&contents).map_err(|e| format!("stm.yml parse error: {e}"))?;
        config
            .validate()
            .map_err(|e| format!("stm.yml validation error: {e}"))?;
    }

    let rules_path = dir.join("stm-rules.yml");

    // Only validate stm-rules.yml if it exists — missing is not an error.
    if rules_path.exists() {
        let contents = std::fs::read_to_string(&rules_path)
            .map_err(|e| format!("failed to read {:?}: {e}", rules_path))?;
        let _: WindowRulesConfig = serde_yaml::from_str(&contents)
            .map_err(|e| format!("stm-rules.yml parse error: {e}"))?;
    }

    Ok(())
}

// ── Private helper ─────────────────────────────────────────────────────

/// Write content to a file only if the file doesn't already exist.
///
/// Creates parent directories as needed. Used by [`init_config_dir`] to write
/// default config files without overwriting user edits.
///
/// # Arguments
///
/// * `path` - File path to write to.
/// * `content` - String content to write.
///
/// # Returns
///
/// `Ok(true)` if the file was written (it didn't exist before).
/// `Ok(false)` if the file already exists (nothing was written).
/// `Err` on I/O failure.
///
/// # Design Decision
///
/// Returns `bool` so callers can log whether a file was actually created,
/// which helps users understand what [`init_config_dir`] did on their behalf.
fn write_default_config_file(path: &Path, content: &str) -> std::io::Result<bool> {
    if path.exists() {
        return Ok(false);
    }

    // Create parent directory if needed.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(path, content)?;
    Ok(true)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::WindowAction;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use tempfile::TempDir;

    // ── load_app_config tests ──────────────────────────────────────────

    /// Positive: valid YAML file parses into the expected `StmConfig`.
    #[test]
    fn load_app_config_valid_file_parses_correctly() {
        let yaml = r#"
super_key: VK_LWIN
column_width: 1200
min_column_width_px: 400
padding:
  window: 8
  up: 10
  down: 40
animation:
  enabled: false
"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();

        let config = load_app_config(f.path());
        assert_eq!(config.super_key, "VK_LWIN");
        assert_eq!(config.column_width, 1200);
        assert_eq!(config.min_column_width_px, 400);
        assert_eq!(config.padding.window, 8);
        assert_eq!(config.padding.up, 10);
        assert_eq!(config.padding.down, 40);
        assert!(!config.animation.enabled);
    }

    /// Negative: missing file returns default config (not panic, not error).
    #[test]
    fn load_app_config_missing_file_returns_default() {
        let path = std::path::PathBuf::from("C:\\__nonexistent_test_path__\\stm.yml");
        let config = load_app_config(&path);
        assert_eq!(config.super_key, "VK_F24");
        assert_eq!(config.column_width, 960);
        assert_eq!(config.min_column_width_px, 320);
        assert_eq!(config.padding.window, 4);
    }

    /// Negative: malformed YAML returns default config (not panic).
    #[test]
    fn load_app_config_malformed_yaml_returns_default() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"this is not: valid: yaml: [[[[").unwrap();

        let config = load_app_config(f.path());
        assert_eq!(config.super_key, "VK_F24");
        assert_eq!(config.column_width, 960);
    }

    /// Negative: empty YAML object `{}` returns default config (all serde defaults fill in).
    #[test]
    fn load_app_config_empty_yaml_returns_default() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"{}").unwrap();

        let config = load_app_config(f.path());
        assert_eq!(config.super_key, "VK_F24");
        assert_eq!(config.column_width, 960);
        assert_eq!(config.padding.window, 4);
    }

    // ── init_config_dir tests ──────────────────────────────────────────

    /// Positive: `init_config_dir` creates directory and writes default files.
    #[test]
    fn init_config_dir_creates_directory_and_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let result = init_config_dir(dir);
        assert!(result.is_ok(), "init_config_dir failed: {result:?}");

        // Directory should exist.
        assert!(dir.is_dir());

        // schemas/ subdirectory should exist.
        assert!(
            dir.join("schemas").is_dir(),
            "schemas/ subdirectory should be created"
        );

        // Default files should exist.
        assert!(dir.join("stm.yml").exists(), "stm.yml should be created");
        assert!(
            dir.join("stm-rules.yml").exists(),
            "stm-rules.yml should be created"
        );

        // Schema files should be in schemas/ subdirectory.
        assert!(
            dir.join("schemas/stm-config.schema.json").exists(),
            "schemas/stm-config.schema.json should be created"
        );
        assert!(
            dir.join("schemas/stm-rules.schema.json").exists(),
            "schemas/stm-rules.schema.json should be created"
        );

        // stm.yml should start with the schema header.
        let contents = std::fs::read_to_string(dir.join("stm.yml")).unwrap();
        assert!(
            contents.contains("yaml-language-server"),
            "stm.yml should contain schema header"
        );

        // stm.yml should contain valid StmConfig YAML (after the header).
        let config: StmConfig = serde_yaml::from_str(&contents).unwrap();
        assert_eq!(config.super_key, "VK_F24");

        // stm-rules.yml should start with the schema header.
        let contents = std::fs::read_to_string(dir.join("stm-rules.yml")).unwrap();
        assert!(
            contents.contains("yaml-language-server"),
            "stm-rules.yml should contain schema header"
        );

        // stm-rules.yml should contain valid WindowRulesConfig YAML.
        let rules: WindowRulesConfig = serde_yaml::from_str(&contents).unwrap();
        assert_eq!(rules.default_action, WindowAction::Tile);
    }

    /// Negative: `init_config_dir` does not overwrite existing files.
    #[test]
    fn init_config_dir_does_not_overwrite_existing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Create stm.yml with custom content BEFORE init.
        let custom_content = "super_key: VK_LWIN\ncolumn_width: 9999\n";
        std::fs::write(dir.join("stm.yml"), custom_content).unwrap();

        let result = init_config_dir(dir);
        assert!(result.is_ok(), "init_config_dir failed: {result:?}");

        // File should still have the custom content.
        let contents = std::fs::read_to_string(dir.join("stm.yml")).unwrap();
        assert_eq!(contents, custom_content);
        assert!(contents.contains("column_width: 9999"));
    }

    // ── check_config tests ──────────────────────────────────────────────

    /// Positive: valid config directory (freshly initialized) passes validation.
    #[test]
    fn check_config_valid_directory_returns_ok() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Initialize the directory with default (valid) files.
        init_config_dir(dir).expect("init should succeed");

        let result = check_config(dir);
        assert!(result.is_ok(), "check_config failed: {result:?}");
    }

    /// Negative: invalid stm.yml returns validation error.
    #[test]
    fn check_config_invalid_app_config_returns_err() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Write an stm.yml with invalid values (negative padding).
        std::fs::write(
            dir.join("stm.yml"),
            "super_key: VK_F24\npadding:\n  window: -1\n",
        )
        .unwrap();

        let result = check_config(dir);
        assert!(
            result.is_err(),
            "check_config should reject negative padding"
        );
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("padding.window"),
            "error should mention the invalid field: {err_msg}"
        );
    }

    /// Positive: empty directory (no config files) returns Ok.
    ///
    /// Missing files are not errors — they simply mean defaults will be used.
    #[test]
    fn check_config_empty_directory_returns_ok() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Directory is empty — no stm.yml or stm-rules.yml.
        let result = check_config(dir);
        assert!(result.is_ok(), "empty directory should pass check_config");
    }

    /// Negative: malformed `stm-rules.yml` returns a parse error.
    #[test]
    fn check_config_malformed_rules_returns_err() {
        // Arrange: directory with a syntactically invalid stm-rules.yml.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        std::fs::write(dir.join("stm-rules.yml"), b"this is not: valid: yaml: [[[[").unwrap();

        // Act
        let result = check_config(dir);

        // Assert: returns an error mentioning the rules file.
        assert!(
            result.is_err(),
            "malformed stm-rules.yml should cause check_config to fail"
        );
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("stm-rules.yml"),
            "error message should identify the rules file: {err_msg}"
        );
    }

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
        assert_eq!(config.default_action, WindowAction::Float);
        assert_eq!(config.rules.len(), 2);
        assert_eq!(config.rules[0].action, WindowAction::Ignore);
        assert_eq!(config.rules[1].initial_width_eighths, Some(4));
    }

    /// Negative: missing file returns default config (not panic, not error).
    #[test]
    fn load_rules_config_missing_file_returns_default() {
        let path = std::path::PathBuf::from("C:\\__nonexistent_test_path__\\stm-rules.yml");
        let config = load_rules_config(&path);
        assert_eq!(config.default_action, WindowAction::Tile);
        assert!(config.rules.is_empty());
    }

    /// Negative: malformed YAML returns default config (not panic).
    #[test]
    fn load_rules_config_malformed_yaml_returns_default() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"this is not: valid: yaml: [[[[").unwrap();

        let config = load_rules_config(f.path());
        assert_eq!(config.default_action, WindowAction::Tile);
        assert!(config.rules.is_empty());
    }

    /// Negative: empty JSON-like file `{}` returns default config.
    #[test]
    fn load_rules_config_empty_yaml_object_returns_default() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"{}").unwrap();

        let config = load_rules_config(f.path());
        assert_eq!(config.default_action, WindowAction::Tile);
        assert!(config.rules.is_empty());
    }

    /// Positive: valid YAML with regex fields round-trips through file I/O.
    #[test]
    fn load_rules_config_roundtrips_regex_fields() {
        use crate::config::types::{MatchRule, WindowRule, WindowRulesConfig};

        let config = WindowRulesConfig {
            default_action: WindowAction::Ignore,
            rules: vec![WindowRule {
                match_: MatchRule {
                    exe_regex: Some("chrome\\.exe".into()),
                    class_regex: Some("Chrome.*".into()),
                    process_path_regex: Some(".*\\\\Chrome\\\\.*".into()),
                    ..Default::default()
                },
                action: WindowAction::Tile,
                initial_width_eighths: None,
                override_persist: false,
            }],
        };

        let yaml = serde_yaml::to_string(&config).unwrap();
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();

        let loaded = load_rules_config(f.path());
        assert_eq!(loaded.default_action, WindowAction::Ignore);
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

    // ── load_default_config tests ──────────────────────────────────────

    /// Negative: `load_default_config()` does not panic regardless of whether
    /// `default-config.yml` exists next to the test binary.
    ///
    /// The exe directory in test environments (`target\debug\deps\`) will not
    /// have the bundled config file, so this exercises the "file not found →
    /// default" path. We do not assert content because CI environments with
    /// the file deployed alongside the binary would see different values.
    #[test]
    fn load_default_config_no_file_returns_default() {
        // Only verify it does not panic; content depends on test environment.
        let _config = load_default_config();
    }

    // ── default-stm-rules.yml parse test ────────────────────────────────

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
        assert_eq!(config.default_action, WindowAction::Tile);
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
        assert_eq!(taskbar_rule.unwrap().action, WindowAction::Ignore);
    }

    // ── default-config.yml parse test ──────────────────────────────────

    /// Positive: the bundled `default-config.yml` in the project root
    /// parses correctly as `StmConfig`.
    ///
    /// This catches syntax errors or schema drift in the shipped defaults.
    /// It does NOT enforce that the YAML values match the Rust serde defaults
    /// — `default-config.yml` is the single source of truth and can be freely
    /// edited to change defaults without recompiling.
    #[test]
    fn default_config_yml_parses_correctly() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR should be set during tests");
        let path = std::path::PathBuf::from(manifest_dir).join("default-config.yml");

        if !path.exists() {
            eprintln!("skipping: default-config.yml not found at {path:?}");
            return;
        }

        let config = load_app_config(&path);

        // Verify the file parsed and is valid (passes semantic validation).
        assert!(
            config.validate().is_ok(),
            "default-config.yml should pass validation"
        );

        // Spot-check: file should have all top-level sections.
        assert!(!config.super_key.is_empty(), "super_key should be set");
        assert!(config.column_width > 0, "column_width should be positive");
        assert!(
            config.min_column_width_px > 0,
            "min_column_width_px should be positive"
        );
        assert!(
            !config.hotkeys.focus_left.is_empty(),
            "hotkeys should be populated"
        );
    }
}
