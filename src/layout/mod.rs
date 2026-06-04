//! Layout engine — virtual canvas, projection, and mutation logic.
//!
//! All layout computation is pure Rust with no Win32 dependencies.
//! The layout engine owns the virtual layout, projects it to actual
//! pixel coordinates, and computes diffs for animation.

pub mod diff;
pub mod engine;
pub mod mutations;
pub mod projection;
pub mod types;

pub use engine::LayoutEngine;
pub use types::{ActualEntry, ActualLayout, AnimationHint, Column, VirtualLayout, WindowMove};
