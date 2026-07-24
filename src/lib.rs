#![warn(missing_docs)]
//! FlowWM (`flow`) — a tiling window manager for Windows built
//! around a **scrolling, infinite-horizontal-canvas** layout model.
//!
//! # Binaries
//!
//! The project ships as two binaries inside a single Cargo package sharing
//! this library crate:
//!
//! | Binary | Role |
//! |--------|------|
//! | `flowd` | Daemon — owns all state, manages windows |
//! | `flow` | CLI client — sends commands to the daemon via named-pipe IPC |
//!
//! # Modules
//!
//! - [`workspace`] — layout orchestration (`ScrollingSpace`, `Monitor`, `Workspace`)
//! - [`layout`] — pure layout math: mutation + projection (zero Win32)
//! - [`registry`] — window metadata, classification, and the Win32 bridge
//! - [`animation`] — embedded animation crate (`WindowAnimator`, `DwmFlush` pacing)
//! - [`daemon`] — `FlowWM` coordinator, hook handlers, dispatch
//! - [`ipc`] — named-pipe server and message protocol
//! - [`config`] — TOML config (code is the single source of truth)
//! - [`common`] — shared bridge types (`WindowId`, `Rect`, `Direction`)
//!
//! Every layout change flows through a pure three-stage pipeline —
//! **mutate → project → animate** — and all subsystems take `&mut self` (no
//! `Arc<Mutex>`; the borrow checker enforces exclusive access).
//!
//! # Developer guide
//!
//! Architecture diagrams, design rationale, the threading model, and deep
//! algorithm walkthroughs (classification, projection, animation) live in the
//! mdBook developer guide under `docs/`. Start with *"How to Read This Book"*
//! (`docs/src/dev-guide/README.md`), which explains the three-layer
//! documentation strategy used throughout the crate.

pub mod animation;
pub mod autostart;
pub mod borders;
pub mod common;
pub mod config;
pub mod daemon;
pub mod ipc;
pub mod layout;
pub mod logging;
pub mod registry;
pub mod updater;
pub mod workspace;
