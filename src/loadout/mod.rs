//! Loadout save/restore data model.
//!
//! Pure serde types for serializing workspace snapshots to JSON. The daemon
//! reads/writes the loadout file — this module has no I/O or Win32 dependencies.

mod types;

pub use types::{
    ColumnSnapshot, FloatingEntry, LoadoutFile, RectJson, RowSnapshot, ScrollingSnapshot,
    WindowRef, WorkspaceSnapshot,
};
