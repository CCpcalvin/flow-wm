//! CLI integration tests — entry point.
//!
//! Submodules are organised by feature area under `tests/cli/`.
//! `#[path]` attributes are needed because Rust integration-test crate roots
//! resolve `mod` relative to the `tests/` directory, not the file's directory.

#[path = "cli/common.rs"]
mod common;
#[path = "cli/daemon_lifecycle.rs"]
mod daemon_lifecycle;
