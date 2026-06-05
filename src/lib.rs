#![warn(missing_docs)]
//! ScrollingTilingManager (`stm`) — a tiling window manager for Windows built
//! around a **scrolling, infinite-horizontal-canvas** layout model.
//!
//! # Binaries
//!
//! The project ships as three binaries inside a single Cargo package sharing
//! this library crate:
//!
//! | Binary | Role |
//! |--------|------|
//! | `stmd` | Daemon process — owns all state, manages windows |
//! | `stm` | CLI client — sends commands to the daemon via IPC |
//! | `stm-watchdog` | Crash-recovery helper — restores windows if the daemon dies |
//!
//! # Architecture Overview
//!
//! The system is split into layers with clear ownership boundaries:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────┐
//! │                    stm daemon (main.rs)                  │
//! │                                                          │
//! │  ┌──────────────┐   ┌───────────────┐                    │
//! │  │ IPC Server   │   │ InputInter-   │                    │
//! │  │ (src/ipc)    │   │ ceptor        │                    │
//! │  └──────┬───────┘   └──────┬────────┘                    │
//! │         └──────────┬───────┘                             │
//! │                    ▼                                     │
//! │           ┌──────────────┐    ┌──────────────────────┐   │
//! │           │ LayoutEngine │◄──►│ WindowRegistry       │   │
//! │           │ (src/layout) │    │ (src/registry)       │   │
//! │           └──────┬───────┘    └──────────────────────┘   │
//! │                  ▼                                       │
//! │          ┌──────────────┐                                │
//! │          │ Compositor   │  →  SetWindowPos (Win32)       │
//! │          └──────────────┘                                │
//! │                                                          │
//! │  ┌──────────────┐   ┌──────────────┐                     │
//! │  │ src/config   │   │ src/persist  │                     │
//! │  └──────────────┘   └──────────────┘                     │
//! └──────────────────────────────────────────────────────────┘
//! ```
//!
//! # The 3-Layer Layout Pipeline
//!
//! Layout computation follows a functional, declarative pipeline:
//!
//! 1. **Virtual Layer** ([`layout::types::VirtualLayout`]) — logical structure on an infinite
//!    horizontal canvas. Columns store proportional widths (`width_eighths`),
//!    not pixel positions.
//!
//! 2. **Projection** ([`layout::projection::project`]) — pure function that converts the
//!    virtual layout into actual screen coordinates, applying all padding.
//!    Position is *implicit* from column index and cumulative widths — there is
//!    no mutable "update Column → propagate to Windows" loop.
//!
//! 3. **Diff** ([`layout::diff::diff`]) — compares previous and new [`layout::types::ActualLayout`] to
//!    produce [`layout::types::WindowMove`] instructions with [`layout::types::AnimationHint`]s.
//!
//! Every mutation flows through: **mutate → project → diff**.
//!
//! # Ownership Model
//!
//! - **[`layout::engine::LayoutEngine`]** owns the *layout logic*: virtual layout, focus state,
//!   column widths, viewport offset. It never touches Win32.
//!
//! - **`WindowRegistry`** (not yet implemented) will own the *window metadata*:
//!   HWND ↔ [`common::types::WindowId`] mapping, window titles/classes, tile/float/ignore
//!   state. It bridges the layout engine to Win32.
//!
//! - **[`common::types::WindowId`]** is the platform-independent bridge type between these two
//!   components. The layout engine only ever sees `WindowId`; it never knows
//!   about HWNDs.
//!
//! # Padding Strategy
//!
//! Padding is handled during projection — outside the Window concept entirely.
//! The [`layout::types::ActualEntry::rect`] produced by projection is the **final HWND rect**
//! that can be passed directly to `SetWindowPos`:
//!
//! ```text
//! Column cell (no padding):  [0, 0, 960, 1080]
//!      │
//!      │  projection::project_column_rows() applies padding.window on all sides
//!      ▼
//! Window rect (HWND):       [4, 4, 952, 1072]
//! ```
//!
//! See [`layout::projection`] module docs for the full container model.

pub mod common;
pub mod config;
pub mod layout;
