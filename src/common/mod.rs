//! Common types shared across all modules.
//!
//! Contains geometry types, directional enums, window identity,
//! and project-wide error types.

pub mod error;
pub mod types;

pub use error::{StmError, StmResult};
pub use types::{Direction, Point, Rect, Size, WindowId};
