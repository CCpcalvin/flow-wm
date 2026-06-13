//! Common types shared across all modules.
//!
//! This module defines the foundational types used throughout stm — geometry
//! primitives, direction enums, and the platform-independent [`WindowId`] handle.
//! These types have **zero platform dependencies** and are testable on any OS.
//!
//! # Cross-Layer Contract
//!
//! [`Rect`] is the frozen contract between the layout engine and the Win32
//! compositor. Its field layout (`x`, `y`, `width`, `height`) must not change —
//! every [`ActualEntry`](crate::layout::ActualEntry) carries a [`Rect`] that is
//! passed directly to `SetWindowPos`.

pub mod error;
pub mod types;

pub use error::{StmError, StmResult};
pub use types::{Direction, InvisibleBounds, Point, Rect, Size, WindowId};
