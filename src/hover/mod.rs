//! Hover subsystem decision logic — focus-follows-mouse and edge-hover-scroll.
//!
//! This module is the **pure, testable core** of the hover feature: a
//! clock-injectable [`HoverController`] that decides focus and edge-scroll
//! actions from injected time and cursor/foreground/timer inputs, plus a pure
//! [`edge_band_direction`] screen-edge classifier. Neither touches Win32 or the
//! daemon — both are hermetic and deterministic, so every hover rule is a unit
//! test with no daemon construction.
//!
//! The controller is **not yet wired** into the live daemon: the wiring tickets
//! translate its [`HoverAction`]s into `GetCursorPos` polls, OS foreground
//! pushes, and feeds to the shared edge-scroll scheduler. See
//! (`docs/src/dev-guide/hover.md`) for the architecture narrative and
//! `docs/adr/0001-hover-subsystem.md` for the pinned design rationale.
//!
//! # Vocabulary
//!
//! The controller reuses [`crate::common`] types only — [`WindowId`], [`Rect`],
//! [`Point`], [`Direction`] — so it shares the hermetic, zero-Win32 property of
//! the layout engine and the drag's `EdgeScrollScheduler`.
//!
//! [`WindowId`]: crate::common::WindowId
//! [`Rect`]: crate::common::Rect
//! [`Point`]: crate::common::Point
//! [`Direction`]: crate::common::Direction

pub mod controller;
pub mod edge_band;

pub use controller::{HoverAction, HoverController, HoverPoll, HoverTimings};
pub use edge_band::edge_band_direction;
