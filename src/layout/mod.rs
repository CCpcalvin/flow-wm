//! Layout engine — infinite horizontal canvas with camera-based viewport.
//!
//! The pure-math core of flow: all layout computation here is **platform-independent
//! Rust with zero Win32 dependencies**, testable on any platform. Position on the
//! canvas is *implicit* (prefix-sum of column widths); a `viewport_offset` acts as
//! a camera that determines which slice of the canvas is visible.
//!
//! Every layout change flows through three pure stages — **mutate → project →
//! animate** — producing an [`AppliedLayout`] that the orchestrator
//! ([`workspace::ScrollingSpace`]) consumes. There is no mutable "update column →
//! propagate to windows" loop; windows outside the viewport are *parked* at
//! deterministic off-screen positions by projection (see [`projection`]).
//!
//! # Submodules
//!
//! | Module | Responsibility |
//! |--------|---------------|
//! | [`types`] | Core data types — [`Column`], [`VirtualLayout`], [`ActualLayout`], [`AppliedLayout`] |
//! | [`projection`] | Virtual → actual projection: camera shift, parking, padding |
//! | [`mutations`] | All pure mutation functions (scroll, focus, swap, resize, add/remove) |
//!
//! The orchestrator that wires mutations → projection → [`AppliedLayout`] used to
//! live here as `LayoutEngine`; it moved to [`workspace::ScrollingSpace`] in the
//! workspace-hierarchy refactor. This module now holds only the pure layout math.
//!
//! # Further reading
//!
//! The camera model, parking mechanism, mutation catalog, and projection
//! algorithm are walked through with mermaid diagrams in the *Layout Engine*
//! chapters of the developer guide (`docs/src/dev-guide/layout/`).

pub mod mutations;
pub mod projection;
pub mod types;

pub use mutations::NeighborLocation;
pub use types::{ActualEntry, ActualLayout, AppliedLayout, Column, VirtualLayout};
