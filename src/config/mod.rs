//! Configuration parser and schema generation.
//!
//! Defines the config file structure, loads YAML config, and
//! generates JSON Schema for editor autocomplete.

pub mod schema;
pub mod types;

pub use types::StmConfig;
