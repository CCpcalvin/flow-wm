//! Config lifecycle: init, load, validate, and check.
//!
//! This module owns the four-phase config lifecycle:
//!
//! 1. **Init** — [`init_config_dir`] creates the config directory and writes
//!    default config files (`flow.toml`, `flow-rules.toml`) plus JSON Schema files
//!    for IDE autocomplete. The `flow.toml` written is a fully-commented example
//!    ([`DEFAULT_CONFIG_EXAMPLE`]).
//!
//! 2. **Load** — [`load_app_config`] reads the user's `flow.toml`. Because every
//!    field has a serde default (see each struct`s `Default` impl), the
//!    file may be partial or empty. [`load_rules_config`] does the same for
//!    window rules; [`load_default_rules`] loads the bundled default rules.
//!
//! 3. **Validate** — [`check_config`] validates config files without loading
//!    them into the daemon. Used by `flow config check`.
//!
//! 4. **Use** — The daemon extracts fields from [`FlowConfig`](super::types::FlowConfig)
//!    into the layout engine.
//!
//! # CODE is the single source of truth
//!
//! Default values live in each struct's `Default` impl, referenced by
//! serde `#[serde(default)]` container attributes. There is **no shipped-defaults
//! TOML merged at runtime** — a previous design did that, but it silently fell
//! back to stale compiled-in `Default` values when the shipped file was absent
//! during development (the build never copied it next to `flowd.exe`). Making
//! code the source of truth removes that failure mode.
//!
//! [`DEFAULT_CONFIG_EXAMPLE`] is a hand-written starter file, not a runtime
//! default source. It is copied to users by [`init_config_dir`] and kept in
//! sync with the compiled defaults by a test.
//!
//! Default *rules* — the classification list — are a different case: they come
//! from [`DEFAULT_RULES_TOML`], which embeds `default-flow-rules.toml` into the
//! binary via `include_str!`. [`WindowRulesConfig::default`] (empty rules)
//! remains only as the parse-failure fallback.
//!
//! # Error-Propagating Load Design
//!
//! Load functions distinguish three failure modes (see [`ConfigLoadError`]):
//! a missing file is benign (fresh install) and yields `T::default()`; a parse
//! or I/O error is propagated as `Err` so callers decide policy. The daemon
//! (`flowd`) treats an `flow.toml` parse error as fatal, while the `flow` client
//! runs a pre-flight check to surface the error on the user's terminal before
//! spawning the daemon. Rules-file failures are non-fatal everywhere (warn +
//! default rules).
//!
//! # Schema Headers
//!
//! When [`init_config_dir`] writes TOML files, it ensures a `#:schema ...` comment
//! header for IDE autocomplete support (taplo LSP). The schema files are written
//! into a `schemas/` subdirectory.

use std::fmt;
use std::io;
use std::path::Path;

use serde::de::DeserializeOwned;

use super::schema;
use super::types::{FlowConfig, WindowRulesConfig};
use crate::common::{FlowError, FlowResult};

/// The hand-written, fully-commented example `flow.toml` copied into a user's
/// config directory by [`init_config_dir`].
///
/// Embedded at compile time via [`include_str!`]. This is **not** read at
/// runtime for defaults — defaults live in each struct`s `Default` impl
/// and are applied by serde. This file is purely the starter template written by
/// `flow config init`. A test in [`types`](super::types) guarantees it stays in
/// sync with the compiled [`FlowConfig::default`](super::types::FlowConfig::default).
pub const DEFAULT_CONFIG_EXAMPLE: &str = include_str!("../../default-config.toml");

/// The bundled default window classification rules, embedded at compile time.
///
/// This is the content of `default-flow-rules.toml` from the project root,
/// baked into the binary via [`include_str!`]. It is parsed in-memory by
/// [`load_default_rules`] — there is **no runtime file lookup**, so the
/// defaults are always present and cannot be accidentally deleted or corrupted
/// by an end user. Users override these defaults via their own
/// `flow-rules.toml`, which the classification pipeline checks first.
///
/// A test (`default_flow_rules_toml_parses_correctly`) guarantees the embedded
/// content parses cleanly as [`WindowRulesConfig`].
pub const DEFAULT_RULES_TOML: &str = include_str!("../../default-flow-rules.toml");

/// The bundled AutoHotkey v2 keybinding template, embedded at compile time.
///
/// This is the content of `flow.ahk` from the project root, baked into the
/// binary via [`include_str!`]. It is copied verbatim into a user's config
/// directory by [`write_ahk_template`] when they run `flow config init --ahk`.
/// FlowWM does not parse or execute this file — it is a convenience starter
/// for users who want to drive FlowWM via AutoHotkey without authoring a
/// script from scratch. Existing copies are never overwritten (see
/// [`write_ahk_template`]).
pub const DEFAULT_AHK_TEMPLATE: &str = include_str!("../../flow.ahk");

/// Schema header for `flow-rules.toml` files (taplo LSP autocomplete).
///
/// Prepended to the TOML content when [`init_config_dir`] writes the default
/// `flow-rules.toml`. The relative path `./schemas/flow-rules.schema.json` is resolved
/// against the config directory, so the schema file must be in `dir/schemas/`.
///
/// (The `flow.toml` example does not need a prepended header — it already carries
/// its own `#:schema` line as part of [`DEFAULT_CONFIG_EXAMPLE`].)
const FLOW_RULES_SCHEMA_HEADER: &str = "#:schema ./schemas/flow-rules.schema.json\n\n";

// ── Load functions ────────────────────────────────────────────────────

/// Internal cause of a config load failure, distinguishing the benign
/// missing-file case from genuine errors.
///
/// The public boundary collapses `Io` + `Parse` into [`FlowError::Config`];
/// `Missing` is handled inside each loader as `Ok(T::default())` and never
/// reaches public callers.
enum ConfigLoadError {
    /// File not found — treated as a fresh install, not a failure.
    Missing,
    /// Filesystem error other than `NotFound` (permissions, locked, etc.).
    Io(io::Error),
    /// TOML parse or schema-mismatch error.
    Parse(toml::de::Error),
}

impl fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => write!(f, "file not found"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Parse(e) => write!(f, "parse error: {e}"),
        }
    }
}

/// Read and parse a TOML file into `T`, classifying the failure mode.
///
/// The shared core behind [`load_app_config`] and [`load_rules_config`].
/// `NotFound` maps to [`ConfigLoadError::Missing`] (benign); every other read
/// failure is [`ConfigLoadError::Io`]; a TOML deserialization failure is
/// [`ConfigLoadError::Parse`]. Callers decide policy (default vs. propagate).
fn load_toml<T: DeserializeOwned>(path: &Path) -> Result<T, ConfigLoadError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(ConfigLoadError::Missing);
        }
        Err(e) => return Err(ConfigLoadError::Io(e)),
    };
    toml::from_str(&contents).map_err(ConfigLoadError::Parse)
}

/// Load application config from a TOML file.
///
/// Reads [`FlowConfig`] from the given TOML file.
///
/// - **File not found** → returns `Ok(FlowConfig::default())` (benign: fresh install).
/// - **Parse error or I/O error** → returns `Err(FlowError::Config)` carrying the
///   path and underlying cause, so callers can surface the failure to the user.
/// - **Success** → calls [`FlowConfig::validate()`] and logs warnings if validation
///   fails, but still returns the loaded config. Logs at `info` level on success.
///
/// This function never panics and imposes no error policy -- callers decide how
/// to handle `Err` (`docs/src/dev-guide/config-and-persistence.md`).
///
/// # Arguments
///
/// * `path` - Path to the `flow.toml` file.
///
/// # Errors
///
/// [`FlowError::Config`] if the file exists but cannot be read or parsed, with a
/// message of the form `failed to load config <path>: <reason>`. A missing file
/// is not an error — it yields the default config.
///
/// # Example
///
/// ```no_run
/// use flow_wm::config::load_app_config;
/// use std::path::Path;
///
/// let config = load_app_config(Path::new("flow.toml")).unwrap_or_default();
/// println!("columns_per_screen = {}", config.columns_per_screen);
/// ```
pub fn load_app_config(path: &Path) -> FlowResult<FlowConfig> {
    match load_toml::<FlowConfig>(path) {
        Ok(config) => {
            // Validate post-deserialization — log warnings but don't fail.
            if let Err(warning) = config.validate() {
                log::warn!("config validation warning for {:?}: {warning}", path);
            }
            log::info!("loaded app config from {:?}", path);
            Ok(config)
        }
        Err(ConfigLoadError::Missing) => {
            log::debug!("app config not found at {:?}; using defaults", path);
            Ok(FlowConfig::default())
        }
        Err(e) => Err(FlowError::Config(format!(
            "failed to load config {}: {e}",
            path.display()
        ))),
    }
}

/// Load window rules config from a TOML file.
///
/// Reads [`WindowRulesConfig`] from the given TOML file.
///
/// - **File not found** → returns `Ok(WindowRulesConfig::default())` (benign).
/// - **Parse error or I/O error** → returns `Err(FlowError::Config)` carrying the
///   path and underlying cause.
/// - **Success** → returns the parsed config. Logs at `info` level on success.
///
/// This function never panics and imposes no error policy -- callers decide how
/// to handle `Err` (`docs/src/dev-guide/config-and-persistence.md`).
///
/// # Arguments
///
/// * `path` - Path to the `flow-rules.toml` file.
///
/// # Errors
///
/// [`FlowError::Config`] if the file exists but cannot be read or parsed, with a
/// message of the form `failed to load config <path>: <reason>`. A missing file
/// is not an error — it yields the default config.
pub fn load_rules_config(path: &Path) -> FlowResult<WindowRulesConfig> {
    match load_toml::<WindowRulesConfig>(path) {
        Ok(config) => {
            log::info!("loaded window rules from {:?}", path);
            Ok(config)
        }
        Err(ConfigLoadError::Missing) => {
            log::debug!("rules config not found at {:?}; using defaults", path);
            Ok(WindowRulesConfig::default())
        }
        Err(e) => Err(FlowError::Config(format!(
            "failed to load config {}: {e}",
            path.display()
        ))),
    }
}

/// Load the bundled default window rules.
///
/// The rules are embedded into the binary at compile time via
/// [`DEFAULT_RULES_TOML`] (sourced from `default-flow-rules.toml` in the project
/// root) and parsed in-memory. There is **no runtime file lookup** — the
/// defaults are always present and cannot be accidentally deleted or corrupted
/// by the user. (Users override defaults via their own `flow-rules.toml`, which
/// the classification pipeline checks at an earlier layer.)
///
/// Because the content is fixed at compile time, a parse failure here indicates
/// a bug in the shipped `default-flow-rules.toml` — caught in CI by the
/// [`default_flow_rules_toml_parses_correctly`] test. As a last line of defense,
/// an empty [`WindowRulesConfig`] is returned and the error is logged.
///
/// # Design rationale
///
/// A previous design read `default-flow-rules.toml` from the executable's
/// directory at runtime. This silently fell back to empty rules during
/// development (the build never copied the file next to `flowd.exe`) and would
/// have shipped a separate plaintext file that users could accidentally break.
/// Embedding eliminates both failure modes.
///
/// # Returns
///
/// A [`WindowRulesConfig`] parsed from the embedded defaults. On the
/// (should-be-impossible) parse failure, returns [`WindowRulesConfig::default`].
#[must_use]
pub fn load_default_rules() -> WindowRulesConfig {
    match toml::from_str::<WindowRulesConfig>(DEFAULT_RULES_TOML) {
        Ok(config) => {
            log::info!(
                "loaded bundled default window rules ({} rules)",
                config.rules.len()
            );
            config
        }
        Err(e) => {
            log::error!("failed to parse bundled default rules: {e}; using empty defaults");
            WindowRulesConfig::default()
        }
    }
}

// ── Init function ──────────────────────────────────────────────────────

/// Initialize a config directory with default files, schema headers, and JSON Schemas.
///
/// Creates the directory at `dir` if it doesn't exist (idempotent — calling
/// multiple times is safe). Then writes each default file only if it doesn't
/// already exist:
///
/// - `flow.toml` — the fully-commented example template ([`DEFAULT_CONFIG_EXAMPLE`]),
///   which already carries its own `#:schema` header. Defaults at runtime come
///   from serde (see each struct`s `Default` impl), so this file is a
///   human-readable starting point that users can trim or empty freely.
/// - `flow-rules.toml` — default [`WindowRulesConfig`] as TOML, with a
///   `#:schema` header prepended.
/// - `schemas/flow-config.schema.json` — JSON Schema for [`FlowConfig`].
/// - `schemas/flow-rules.schema.json` — JSON Schema for [`WindowRulesConfig`].
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
/// Returns `Err` if directory creation fails, or if writing `flow.toml` or
/// `flow-rules.toml` fails. JSON Schema write failures are non-fatal (logged
/// as warnings) and do not cause this function to return `Err`.
///
/// # Example
///
/// ```no_run
/// use flow_wm::config::init_config_dir;
/// use std::path::Path;
///
/// if let Err(e) = init_config_dir(Path::new("~/.config/flow")) {
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
    //
    // flow.toml: write the fully-commented example template
    // ([`DEFAULT_CONFIG_EXAMPLE`]). It already carries its own `#:schema`
    // header. Defaults at runtime come from serde (see each struct`s `Default` impl),
    // so this file is purely a human-readable starting point — users can trim it
    // to just the fields they want to override, or even empty it entirely.
    let flow_toml_content = DEFAULT_CONFIG_EXAMPLE;

    // flow-rules.toml: write the Rust defaults. Rules don't have a partial-override
    // model, so a complete starter file is appropriate.
    let default_rules_toml = toml::to_string(&WindowRulesConfig::default())
        .map_err(|e| format!("failed to serialize default WindowRulesConfig: {e}"))?;

    // Write example flow.toml, if it doesn't exist.
    let flow_toml = dir.join("flow.toml");
    match write_default_config_file(&flow_toml, flow_toml_content) {
        Ok(written) => {
            if written {
                log::info!("wrote example config to {:?}", flow_toml);
            }
        }
        Err(e) => {
            return Err(format!(
                "failed to write default config {:?}: {e}",
                flow_toml
            ));
        }
    }

    // Write default flow-rules.toml with schema header, if it doesn't exist.
    let rules_toml = dir.join("flow-rules.toml");
    let rules_content = format!("{FLOW_RULES_SCHEMA_HEADER}{default_rules_toml}");
    match write_default_config_file(&rules_toml, &rules_content) {
        Ok(written) => {
            if written {
                log::info!("wrote default rules to {:?}", rules_toml);
            }
        }
        Err(e) => {
            return Err(format!(
                "failed to write default rules {:?}: {e}",
                rules_toml
            ));
        }
    }

    // Generate and write JSON Schemas into the schemas/ subdirectory.
    let config_schema_path = schemas_dir.join("flow-config.schema.json");
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

    let rules_schema_path = schemas_dir.join("flow-rules.schema.json");
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

/// Write the bundled AutoHotkey keybinding template into a config directory.
///
/// Copies [`DEFAULT_AHK_TEMPLATE`] to `dir/flow.ahk`, but only if that file
/// does not already exist — so re-running `flow config init --ahk` never
/// clobbers user edits.
///
/// CLI-only: invoked by `flow config init --ahk`; the daemon never calls this.
///
/// # Arguments
///
/// * `dir` - Path to the config directory. Missing parent directories are
///   created as needed.
///
/// # Returns
///
/// `Ok(true)` if `flow.ahk` was newly written. `Ok(false)` if it already
/// existed (untouched). Callers use this to report accurate feedback.
///
/// # Errors
///
/// Returns `Err` if writing `flow.ahk` fails.
///
/// # Example
///
/// ```no_run
/// use flow_wm::config::write_ahk_template;
/// use std::path::Path;
///
/// if let Err(e) = write_ahk_template(Path::new("~/.config/flow")) {
///     eprintln!("failed to write flow.ahk: {e}");
/// }
/// ```
#[must_use = "initialization errors must be handled"]
pub fn write_ahk_template(dir: &Path) -> Result<bool, String> {
    let ahk_path = dir.join("flow.ahk");
    match write_default_config_file(&ahk_path, DEFAULT_AHK_TEMPLATE) {
        Ok(true) => {
            log::info!("wrote AutoHotkey template to {:?}", ahk_path);
            Ok(true)
        }
        Ok(false) => {
            log::debug!("flow.ahk already exists, skipping {:?}", ahk_path);
            Ok(false)
        }
        Err(e) => Err(format!(
            "failed to write AutoHotkey template {:?}: {e}",
            ahk_path
        )),
    }
}

// ── Check function ─────────────────────────────────────────────────────

/// Validate config files in a directory without loading them into the daemon.
///
/// Checks both `flow.toml` and `flow-rules.toml` in the given directory:
///
/// - Loads `flow.toml` and deserializes it as [`FlowConfig`], then runs semantic
///   validation. Because every field carries a serde default (see
///   each struct`s `Default` impl), partial user files are valid —
///   missing keys are filled in automatically, exactly as the daemon does.
/// - Loads `flow-rules.toml` and checks it parses correctly as [`WindowRulesConfig`].
///
/// Missing files are **not errors** — they simply mean the user hasn't created
/// a config yet and defaults will be used. This function only reports actual
/// parse/validation failures.
///
/// **Logs nothing** — designed for pure CLI validation (`flow config check`) where
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
/// use flow_wm::config::check_config;
/// use std::path::Path;
///
/// if let Err(e) = check_config(Path::new("~/.config/flow")) {
///     eprintln!("config error: {e}");
/// }
/// ```
#[must_use = "validation errors must be handled"]
pub fn check_config(dir: &Path) -> Result<(), String> {
    let flow_path = dir.join("flow.toml");

    // Only validate flow.toml if it exists — missing is not an error.
    // Serde defaults fill in any absent fields (see the struct's `Default` impl), so a
    // partial user file validates successfully, matching daemon behavior.
    if flow_path.exists() {
        let contents = std::fs::read_to_string(&flow_path)
            .map_err(|e| format!("failed to read {flow_path:?}: {e}"))?;
        let config: FlowConfig =
            toml::from_str(&contents).map_err(|e| format!("flow.toml parse error: {e}"))?;
        config
            .validate()
            .map_err(|e| format!("flow.toml validation error: {e}"))?;
    }

    let rules_path = dir.join("flow-rules.toml");

    // Only validate flow-rules.toml if it exists — missing is not an error.
    if rules_path.exists() {
        let contents = std::fs::read_to_string(&rules_path)
            .map_err(|e| format!("failed to read {:?}: {e}", rules_path))?;
        let _: WindowRulesConfig =
            toml::from_str(&contents).map_err(|e| format!("flow-rules.toml parse error: {e}"))?;
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

    /// Positive: valid TOML file parses into the expected `FlowConfig`.
    #[test]
    fn load_app_config_valid_file_parses_correctly() {
        let toml_content = r#"
columns_per_screen = 3
column_width = 1200
min_column_width_px = 400

[padding]
window_gap = 8
up = 10
down = 40

[animation]
enabled = false
duration_ms = 200
easing = "linear"

[minimize_restore]
strategy = "original_slot"
"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml_content.as_bytes()).unwrap();

        let config = load_app_config(f.path()).unwrap();
        assert_eq!(config.columns_per_screen, 3);
        assert_eq!(config.column_width, Some(1200));
        assert_eq!(config.min_column_width_px, 400);
        assert_eq!(config.padding.window_gap, 8);
        assert_eq!(config.padding.up, 10);
        assert_eq!(config.padding.down, 40);
        assert!(!config.animation.enabled);
    }

    /// Negative: missing file returns default config (not panic, not error).
    #[test]
    fn load_app_config_missing_file_returns_default() {
        let path = std::path::PathBuf::from("C:\\__nonexistent_test_path__\\flow.toml");
        let config = load_app_config(&path).unwrap();
        assert_eq!(config, FlowConfig::default());
        assert_eq!(config.min_column_width_px, 640);
        assert_eq!(config.padding.window_gap, 16);
        assert_eq!(config.padding.up, 0);
        assert_eq!(config.padding.down, 0);
    }

    /// Negative: malformed TOML propagates an error naming the failure.
    #[test]
    fn load_app_config_malformed_toml_returns_error() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"this is = not = valid = toml = [[[[").unwrap();

        let result = load_app_config(f.path());
        assert!(
            result.is_err(),
            "malformed flow.toml should propagate an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failed to load config"),
            "error should describe the failure: {err}"
        );
    }

    /// Negative: empty TOML file returns default config (all serde defaults fill in).
    #[test]
    fn load_app_config_empty_toml_returns_default() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"").unwrap();

        let config = load_app_config(f.path()).unwrap();
        assert_eq!(config, FlowConfig::default());
        assert_eq!(config.padding.window_gap, 16);
    }

    /// Positive: partial TOML file returns merged config (user field + serde defaults).
    ///
    /// This verifies that the `load_app_config` wrapper correctly returns a config
    /// where the user-specified field is preserved and all other fields are filled
    /// by serde defaults — matching what the daemon would see at runtime.
    #[test]
    fn load_app_config_partial_toml_merges_with_defaults() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"columns_per_screen = 2\n").unwrap();

        let config = load_app_config(f.path()).unwrap();
        assert_eq!(
            config.columns_per_screen, 2,
            "user-specified field must be preserved"
        );
        // Everything else should be defaults.
        assert_eq!(config.min_column_width_px, 640);
        assert_eq!(config.padding.window_gap, 16);
        assert_eq!(config.animation.duration_ms, 240);
    }

    /// Negative: a path that exists but is a directory (not a file) triggers a
    /// non-`NotFound` I/O error, exercising the `ConfigLoadError::Io` branch.
    ///
    /// `read_to_string` on a directory returns an I/O error whose kind is NOT
    /// `NotFound` (Windows: `PermissionDenied`), so a directory path is the
    /// most portable way to hit the Io arm without permission-denied tricks.
    /// This error must **propagate** (not silently fall back to default, which
    /// is reserved for `Missing`) and must name the offending path.
    #[test]
    fn load_app_config_directory_path_propagates_io_error() {
        // Arrange: a real directory kept alive by TempDir for the test's duration.
        let tmp = TempDir::new().unwrap();
        let dir_path = tmp.path();

        // Act
        let result = load_app_config(dir_path);

        // Assert: must be an error — NOT the benign Missing -> default path.
        assert!(
            result.is_err(),
            "reading a directory should propagate an I/O error, not return default"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failed to load config"),
            "error should describe the failure: {err}"
        );
        // The offending path must appear so the user knows WHERE the failure is.
        assert!(
            err.contains(&dir_path.display().to_string()),
            "error should name the offending path: {err}"
        );
        // It must be the I/O branch, not the parse branch.
        assert!(
            !err.contains("parse error"),
            "directory read should surface as I/O, not parse: {err}"
        );
    }

    /// Negative: the error message names the offending file path (Parse branch).
    ///
    /// `load_app_config_malformed_toml_returns_error` proves malformed TOML is an
    /// error; this test pins the message-format contract that the failing file's
    /// path appears verbatim in the error — the user needs to know WHICH file is
    /// broken when multiple config files exist.
    #[test]
    fn load_app_config_parse_error_message_includes_path() {
        // Arrange: a malformed TOML file with a known path.
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"this is = not = valid = toml = [[[[").unwrap();
        let path_str = f.path().display().to_string();

        // Act
        let result = load_app_config(f.path());

        // Assert: error contains both the failure tag, the file path, and the
        // parse-cause marker (proves it routed through the Parse branch).
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failed to load config"),
            "error should describe the failure: {err}"
        );
        assert!(
            err.contains(&path_str),
            "error should name the offending file path ({path_str}): {err}"
        );
        assert!(
            err.contains("parse error"),
            "malformed TOML should surface as a parse error: {err}"
        );
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
        assert!(
            dir.join("flow.toml").exists(),
            "flow.toml should be created"
        );
        assert!(
            dir.join("flow-rules.toml").exists(),
            "flow-rules.toml should be created"
        );

        // Schema files should be in schemas/ subdirectory.
        assert!(
            dir.join("schemas/flow-config.schema.json").exists(),
            "schemas/flow-config.schema.json should be created"
        );
        assert!(
            dir.join("schemas/flow-rules.schema.json").exists(),
            "schemas/flow-rules.schema.json should be created"
        );

        // flow.toml should start with the schema header.
        let contents = std::fs::read_to_string(dir.join("flow.toml")).unwrap();
        assert!(
            contents.contains("#:schema"),
            "flow.toml should contain taplo schema header"
        );

        // flow.toml is the full, commented example template. It should parse as
        // a valid FlowConfig that equals the compiled defaults (serde fills gaps,
        // but the example carries every field explicitly anyway).
        let parsed: FlowConfig = toml::from_str(&contents).expect("flow.toml should parse");
        assert_eq!(
            parsed,
            FlowConfig::default(),
            "init should write an example that matches compiled defaults"
        );
        assert!(
            contents.contains("min_column_width_px = 640"),
            "flow.toml should be the full example, not an empty stub"
        );
        assert!(
            contents.contains("min_row_height_px = 100"),
            "flow.toml should carry the min_row_height_px default (ticket #10)"
        );

        // flow-rules.toml should start with the schema header.
        let contents = std::fs::read_to_string(dir.join("flow-rules.toml")).unwrap();
        assert!(
            contents.contains("#:schema"),
            "flow-rules.toml should contain taplo schema header"
        );

        // flow-rules.toml should contain valid WindowRulesConfig TOML.
        let rules: WindowRulesConfig = toml::from_str(&contents).unwrap();
        assert_eq!(rules.default_action, WindowAction::Float);
    }

    /// Negative: `init_config_dir` does not overwrite existing files.
    #[test]
    fn init_config_dir_does_not_overwrite_existing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Create flow.toml with custom content BEFORE init.
        let custom_content = "column_width = 9999\n";
        std::fs::write(dir.join("flow.toml"), custom_content).unwrap();

        let result = init_config_dir(dir);
        assert!(result.is_ok(), "init_config_dir failed: {result:?}");

        // File should still have the custom content.
        let contents = std::fs::read_to_string(dir.join("flow.toml")).unwrap();
        assert_eq!(contents, custom_content);
        assert!(contents.contains("column_width = 9999"));
    }

    // ── write_ahk_template tests ─────────────────────────────────────────

    /// Positive: `write_ahk_template` creates `flow.ahk` with the bundled content.
    #[test]
    fn write_ahk_template_creates_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let result = write_ahk_template(dir);
        assert_eq!(result, Ok(true), "should report newly written");

        let ahk = dir.join("flow.ahk");
        assert!(ahk.exists(), "flow.ahk should be created");

        // Content must match the embedded template verbatim.
        let contents = std::fs::read_to_string(&ahk).unwrap();
        assert_eq!(contents, DEFAULT_AHK_TEMPLATE);
        // Sanity: the template is a real AHK v2 script, not an empty stub.
        assert!(
            contents.contains("#Requires AutoHotkey v2.0.2"),
            "flow.ahk should be the AutoHotkey v2 template"
        );
    }

    /// Negative: `write_ahk_template` does not overwrite an existing file.
    #[test]
    fn write_ahk_template_does_not_overwrite_existing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Pre-create flow.ahk with custom content.
        let custom = "# custom user script\n";
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("flow.ahk"), custom).unwrap();

        let result = write_ahk_template(dir);
        assert_eq!(result, Ok(false), "should report already-existed");

        // The user's content must survive.
        let contents = std::fs::read_to_string(dir.join("flow.ahk")).unwrap();
        assert_eq!(contents, custom);
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

    /// Negative: invalid flow.toml returns validation error.
    #[test]
    fn check_config_invalid_app_config_returns_err() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Minimal partial flow.toml: serde fills the rest, but negative padding
        // still fails semantic validation.
        std::fs::write(dir.join("flow.toml"), "[padding]\nwindow_gap = -1\n").unwrap();

        let result = check_config(dir);
        assert!(
            result.is_err(),
            "check_config should reject negative padding"
        );
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("padding.window_gap"),
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

        // Directory is empty — no flow.toml or flow-rules.toml.
        let result = check_config(dir);
        assert!(result.is_ok(), "empty directory should pass check_config");
    }

    /// Positive: partial flow.toml passes validation (merge fills missing fields).
    ///
    /// This is the key behavior fix: partial user files should pass `check_config`
    /// because serde fills gaps from the struct's `Default` impl.
    #[test]
    fn check_config_partial_toml_returns_ok() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Write a partial flow.toml with only one field.
        std::fs::write(dir.join("flow.toml"), "column_width = 800\n").unwrap();

        let result = check_config(dir);
        assert!(
            result.is_ok(),
            "partial flow.toml should pass check_config (merged with defaults): {result:?}"
        );
    }

    /// Negative: syntactically malformed `flow.toml` returns a parse error.
    ///
    /// Unlike `check_config_invalid_app_config_returns_err` (which uses valid TOML
    /// with invalid *values*), this tests completely garbled syntax.
    #[test]
    fn check_config_malformed_app_toml_returns_err() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        std::fs::write(
            dir.join("flow.toml"),
            b"this is = not = valid = toml = [[[[",
        )
        .unwrap();

        let result = check_config(dir);
        assert!(
            result.is_err(),
            "malformed flow.toml should cause check_config to fail"
        );
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("flow.toml parse error"),
            "error message should identify parse failure: {err_msg}"
        );
    }

    /// Negative: malformed `flow-rules.toml` returns a parse error.
    #[test]
    fn check_config_malformed_rules_returns_err() {
        // Arrange: directory with a syntactically invalid flow-rules.toml.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        std::fs::write(
            dir.join("flow-rules.toml"),
            b"this is = not = valid = toml = [[[[",
        )
        .unwrap();

        // Act
        let result = check_config(dir);

        // Assert: returns an error mentioning the rules file.
        assert!(
            result.is_err(),
            "malformed flow-rules.toml should cause check_config to fail"
        );
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("flow-rules.toml"),
            "error message should identify the rules file: {err_msg}"
        );
    }

    // ── load_rules_config tests ────────────────────────────────────────

    /// Positive: valid TOML file parses into the expected `WindowRulesConfig`.
    #[test]
    fn load_rules_config_valid_file_parses_correctly() {
        let toml_content = r#"
default_action = "float"

[[rules]]
match = { exe = "explorer.exe", title_contains = "Open" }
action = "ignore"

[[rules]]
match = { class = "Chrome_WidgetWin_1" }
action = "tile"
initial_width_px = 960
"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml_content.as_bytes()).unwrap();

        let config = load_rules_config(f.path()).unwrap();
        assert_eq!(config.default_action, WindowAction::Float);
        assert_eq!(config.rules.len(), 2);
        assert_eq!(config.rules[0].action, WindowAction::Ignore);
        assert_eq!(config.rules[1].initial_width_px, Some(960));
    }

    /// Negative: missing file returns default config (not panic, not error).
    #[test]
    fn load_rules_config_missing_file_returns_default() {
        let path = std::path::PathBuf::from("C:\\__nonexistent_test_path__\\flow-rules.toml");
        let config = load_rules_config(&path).unwrap();
        assert_eq!(config.default_action, WindowAction::Float);
        assert!(config.rules.is_empty());
    }

    /// Negative: malformed TOML propagates an error naming the failure.
    #[test]
    fn load_rules_config_malformed_toml_returns_error() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"this is = not = valid = toml = [[[[").unwrap();

        let result = load_rules_config(f.path());
        assert!(
            result.is_err(),
            "malformed flow-rules.toml should propagate an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failed to load config"),
            "error should describe the failure: {err}"
        );
    }

    /// Negative: empty TOML propagates an error. Unlike `FlowConfig`, the rules
    /// type carries no container `#[serde(default)]`, so an empty file is a
    /// genuine parse error (`default_action` is required). Init handles this
    /// non-fatally: warn + fall back to default rules.
    #[test]
    fn load_rules_config_empty_toml_returns_error() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"").unwrap();

        let result = load_rules_config(f.path());
        assert!(
            result.is_err(),
            "empty flow-rules.toml should propagate an error (default_action is required)"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failed to load config"),
            "error should describe the failure: {err}"
        );
    }

    /// Positive: valid TOML with regex fields round-trips through file I/O.
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
                initial_width_px: None,
                override_persist: false,
            }],
        };

        let toml_str = toml::to_string(&config).unwrap();
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml_str.as_bytes()).unwrap();

        let loaded = load_rules_config(f.path()).unwrap();
        assert_eq!(loaded.default_action, WindowAction::Ignore);
        assert_eq!(loaded.rules.len(), 1);
        assert_eq!(
            loaded.rules[0].match_.exe_regex,
            Some("chrome\\.exe".into())
        );
    }

    /// Negative: a path that exists but is a directory triggers a non-`NotFound`
    /// I/O error, exercising the `ConfigLoadError::Io` branch for the rules loader.
    ///
    /// Mirrors `load_app_config_directory_path_propagates_io_error` to keep the
    /// two loaders' error coverage symmetric. The error must propagate (not the
    /// benign Missing -> default path) and must name the offending path.
    #[test]
    fn load_rules_config_directory_path_propagates_io_error() {
        // Arrange: a real directory kept alive by TempDir for the test's duration.
        let tmp = TempDir::new().unwrap();
        let dir_path = tmp.path();

        // Act
        let result = load_rules_config(dir_path);

        // Assert: must be an error — NOT the benign Missing -> default path.
        assert!(
            result.is_err(),
            "reading a directory should propagate an I/O error, not return default"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failed to load config"),
            "error should describe the failure: {err}"
        );
        assert!(
            err.contains(&dir_path.display().to_string()),
            "error should name the offending path: {err}"
        );
        assert!(
            !err.contains("parse error"),
            "directory read should surface as I/O, not parse: {err}"
        );
    }

    /// Negative: the error message names the offending file path (Parse branch)
    /// for the rules loader.
    ///
    /// Mirrors `load_app_config_parse_error_message_includes_path`: proves the
    /// path-in-message contract holds symmetrically for the rules loader too,
    /// so a broken `flow-rules.toml` is unambiguously identified in the error.
    #[test]
    fn load_rules_config_parse_error_message_includes_path() {
        // Arrange: a malformed TOML file with a known path.
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"this is = not = valid = toml = [[[[").unwrap();
        let path_str = f.path().display().to_string();

        // Act
        let result = load_rules_config(f.path());

        // Assert: error contains the failure tag, the file path, and the
        // parse-cause marker (proves Parse-branch routing).
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failed to load config"),
            "error should describe the failure: {err}"
        );
        assert!(
            err.contains(&path_str),
            "error should name the offending file path ({path_str}): {err}"
        );
        assert!(
            err.contains("parse error"),
            "malformed TOML should surface as a parse error: {err}"
        );
    }

    // ── load_default_rules tests ───────────────────────────────────────

    /// `load_default_rules()` returns the embedded (compile-time) defaults.
    ///
    /// Because defaults are embedded via [`include_str!`](std::include_str), this
    /// always returns the same non-empty ruleset regardless of the test
    /// environment (no file next to the binary is required). We verify the
    /// default action and that the ruleset is non-empty.
    #[test]
    fn load_default_rules_returns_embedded_rules() {
        let config = load_default_rules();
        assert_eq!(config.default_action, WindowAction::Float);
        assert!(
            !config.rules.is_empty(),
            "embedded default rules should not be empty"
        );
    }

    /// The `DEFAULT_RULES_TOML` constant (embedded via `include_str!`) must
    /// contain the same content as the on-disk `default-flow-rules.toml` file.
    /// (Both sides include the `#:schema` header line, which is harmless to
    /// `toml::from_str` since it parses it as a TOML comment.)
    ///
    /// This guards against a scenario where someone edits the on-disk file
    /// but the `include_str!` path is stale (or vice-versa). Both
    /// `load_default_rules_returns_embedded_rules` and
    /// `default_flow_rules_toml_parses_correctly` parse their respective
    /// sources, but only this test confirms they are **identical**.
    #[test]
    fn embedded_rules_toml_matches_disk_file() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR should be set during tests");
        let disk_path = std::path::PathBuf::from(manifest_dir).join("default-flow-rules.toml");

        if !disk_path.exists() {
            eprintln!("skipping: default-flow-rules.toml not found at {disk_path:?}");
            return;
        }

        let disk_content = std::fs::read_to_string(&disk_path)
            .unwrap_or_else(|e| panic!("failed to read {disk_path:?}: {e}"));

        assert_eq!(
            DEFAULT_RULES_TOML, disk_content,
            "DEFAULT_RULES_TOML (include_str!) has drifted from the on-disk file; \
             update one to match the other"
        );
    }

    /// The `DEFAULT_AHK_TEMPLATE` constant (embedded via `include_str!`) must
    /// contain the same content as the on-disk `flow.ahk` file.
    ///
    /// Guards against the `include_str!` path going stale after edits to the
    /// on-disk template. Mirrors [`embedded_rules_toml_matches_disk_file`] for
    /// the sibling `flow.ahk` embed.
    #[test]
    fn embedded_ahk_template_matches_disk_file() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR should be set during tests");
        let disk_path = std::path::PathBuf::from(manifest_dir).join("flow.ahk");

        if !disk_path.exists() {
            eprintln!("skipping: flow.ahk not found at {disk_path:?}");
            return;
        }

        let disk_content = std::fs::read_to_string(&disk_path)
            .unwrap_or_else(|e| panic!("failed to read {disk_path:?}: {e}"));

        assert_eq!(
            DEFAULT_AHK_TEMPLATE, disk_content,
            "DEFAULT_AHK_TEMPLATE (include_str!) has drifted from the on-disk file; \
             update one to match the other"
        );
    }

    /// Verifies the resilient fallback behavior of the parse logic used by
    /// `load_default_rules()`.
    ///
    /// The actual `load_default_rules()` function parses the compile-time
    /// `DEFAULT_RULES_TOML` constant, which is always valid TOML (the
    /// `default_flow_rules_toml_parses_correctly` test guarantees this).
    /// The `Err` branch — returning `WindowRulesConfig::default()` — is
    /// therefore **not directly exercisable** through the public API.
    ///
    /// This test documents the fallback path by feeding deliberately
    /// malformed TOML to `toml::from_str::<WindowRulesConfig>()` and
    /// verifying that (a) the error variant is reachable and (b)
    /// `WindowRulesConfig::default()` is a valid fallback (non-panicking,
    /// empty rules, `default_action: float`).
    ///
    /// Note: `load_default_rules` keeps its own defensive fallback (embedded
    /// content can't realistically fail, so an empty default + log is the right
    /// behavior). The file-backed loaders (`load_rules_config`,
    /// `load_app_config`) instead *propagate* parse errors — see
    /// `load_rules_config_malformed_toml_returns_error` and
    /// `load_app_config_malformed_toml_returns_error`.
    #[test]
    fn load_default_rules_resilient_fallback_on_bad_input() {
        // Arrange: malformed TOML that would trigger the Err branch.
        let bad_toml = "this is = not = valid = toml = [[[[[";

        // Act: attempt to parse as WindowRulesConfig.
        let result = toml::from_str::<WindowRulesConfig>(bad_toml);

        // Assert: parse must fail (the Err branch is reachable).
        assert!(result.is_err(), "malformed TOML should fail to parse");

        // Assert: the fallback default is a valid, usable config.
        let fallback = WindowRulesConfig::default();
        assert_eq!(fallback.default_action, WindowAction::Float);
        assert!(fallback.rules.is_empty(), "fallback should have no rules");
    }

    // ── default-flow-rules.toml parse test ────────────────────────────────

    /// Positive: the bundled `default-flow-rules.toml` in the project root
    /// parses correctly as `WindowRulesConfig`.
    ///
    /// This catches syntax errors or schema drift in the shipped defaults.
    #[test]
    fn default_flow_rules_toml_parses_correctly() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR should be set during tests");
        let path = std::path::PathBuf::from(manifest_dir).join("default-flow-rules.toml");

        // Only run if the file exists (it should in the project tree).
        if !path.exists() {
            eprintln!("skipping: default-flow-rules.toml not found at {path:?}");
            return;
        }

        let config = load_rules_config(&path).unwrap();
        assert_eq!(config.default_action, WindowAction::Float);
        assert!(
            !config.rules.is_empty(),
            "bundled rules should not be empty"
        );
    }

    // ── default-config.toml parse test ──────────────────────────────────

    /// Positive: the hand-written `default-config.toml` example parses correctly
    /// as `FlowConfig`.
    ///
    /// This catches syntax errors in the example file. The authoritative
    /// cross-check (that the example's *values* match the compiled defaults)
    /// lives in `types::tests::default_config_toml_matches_compiled_defaults`.
    #[test]
    fn default_config_toml_parses_correctly() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR should be set during tests");
        let path = std::path::PathBuf::from(manifest_dir).join("default-config.toml");

        if !path.exists() {
            eprintln!("skipping: default-config.toml not found at {path:?}");
            return;
        }

        let config = load_app_config(&path).unwrap();

        // Verify the file parsed and is valid (passes semantic validation).
        assert!(
            config.validate().is_ok(),
            "default-config.toml should pass validation"
        );

        // Spot-check: file should have all top-level sections.
        assert!(
            config.columns_per_screen > 0,
            "columns_per_screen should be positive"
        );
        assert!(
            config.min_column_width_px > 0,
            "min_column_width_px should be positive"
        );
    }

    // The previous TOML-level merge tests have been removed. The two-layer
    // merge model no longer exists — defaults now come from serde
    // `Default` impls, and partial/empty TOML deserialization is covered
    // by tests in `types::tests` (config_from_*_uses_defaults,
    // config_from_nested_partial_toml_uses_defaults).
}
