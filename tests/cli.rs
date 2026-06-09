//! CLI integration tests — entry point.
//!
//! Submodules are organised by feature area under `tests/cli/`.
//! `#[path]` attributes are needed because Rust integration-test crate roots
//! resolve `mod` relative to the `tests/` directory, not the file's directory.
//!
//! `test_desktop` and `registry` modules require the `desktop` module which is
//! gated by `#[cfg(debug_assertions)]`. Integration tests run in debug mode by
//! default (`cargo test`), so these are always available during development.

#[path = "cli/common.rs"]
mod common;
#[cfg(debug_assertions)]
#[path = "cli/daemon_init.rs"]
mod daemon_init;
#[cfg(debug_assertions)]
#[path = "cli/daemon_lifecycle.rs"]
mod daemon_lifecycle;
#[cfg(debug_assertions)]
#[path = "cli/registry.rs"]
mod registry;
#[cfg(debug_assertions)]
#[path = "cli/test_desktop.rs"]
mod test_desktop;
