//! Configuration parser and JSON Schema generation.
//!
//! Defines the YAML config file structure ([`StmConfig`]), serde serialization,
//! and JSON Schema generation for editor autocomplete.
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
