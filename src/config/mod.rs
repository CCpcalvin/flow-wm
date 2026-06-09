//! Configuration parser, auto-creation, and JSON Schema generation.
//!
//! Defines two TOML config file structures:
//!
//! - [`StmConfig`] — Application settings (hotkeys, padding, animation).
//!   Loaded from `stm.toml` in the config directory.
//!
//! - [`WindowRulesConfig`] — Window classification rules and default action.
//!   Loaded from `stm-rules.toml` in the config directory.
//!
//! # Config File Lifecycle
//!
//! The configuration system follows a four-phase lifecycle:
//!
//! 1. **Init** — [`lifecycle::init_config_dir`] creates the config directory and writes
//!    default config files and JSON Schemas.
//!
//! 2. **Load** — [`lifecycle::load_app_config`] and [`lifecycle::load_rules_config`] read TOML
//!    files from disk, with shipped defaults as the base layer.
//!
//! 3. **Validate** — [`lifecycle::check_config`] validates config files without loading
//!    them into the running daemon.
//!
//! 4. **Use** — The daemon extracts fields from [`StmConfig`] into the layout engine.
//!
//! See [`lifecycle`] module docs for detailed documentation of each function.

pub mod dirs;
pub mod lifecycle;
pub mod schema;
pub mod types;

pub use lifecycle::{
    check_config, init_config_dir, load_app_config, load_default_config, load_default_rules,
    load_rules_config,
};
pub use types::StmConfig;
pub use types::WindowRulesConfig;
