//! Project-wide error types.
//!
//! All flow subsystems report errors through [`FlowError`] and the [`FlowResult`]
//! convenience alias. This keeps error handling consistent across modules
//! without introducing per-module error types.

use std::fmt;

/// All errors produced by flow subsystems.
///
/// Each variant corresponds to a subsystem:
///
/// - [`Config`](FlowError::Config) — produced by [`config`](crate::config) during YAML parsing or validation
/// - [`Layout`](FlowError::Layout) — produced by [`layout`](crate::layout) on invalid state transitions
/// - [`Io`](FlowError::Io) — produced during file I/O or socket operations
/// - [`Registry`](FlowError::Registry) — produced by [`registry`](crate::registry) during Win32 bridge or window tracking
#[derive(Debug)]
pub enum FlowError {
    /// Configuration parsing or validation failure.
    Config(String),
    /// Layout computation failure (e.g. invalid state).
    Layout(String),
    /// I/O error (file read/write, socket, etc.).
    Io(std::io::Error),
    /// Registry error (Win32 bridge, window tracking).
    Registry(String),
}

impl fmt::Display for FlowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(s) => write!(f, "config error: {s}"),
            Self::Layout(s) => write!(f, "layout error: {s}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Registry(s) => write!(f, "registry error: {s}"),
        }
    }
}

impl std::error::Error for FlowError {}

impl From<std::io::Error> for FlowError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Convenience alias used across the project for fallible operations.
///
/// # Example
///
/// ```no_run
/// use flow_wm::common::{FlowResult, FlowError};
///
/// fn load_config() -> FlowResult<String> {
///     Err(FlowError::Config("file not found".into()))
/// }
/// ```
pub type FlowResult<T> = Result<T, FlowError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_config_error() {
        let err = FlowError::Config("bad yaml".into());
        assert_eq!(format!("{err}"), "config error: bad yaml");
    }

    #[test]
    fn display_layout_error() {
        let err = FlowError::Layout("no focused window".into());
        assert_eq!(format!("{err}"), "layout error: no focused window");
    }

    #[test]
    fn display_registry_error() {
        let err = FlowError::Registry("window not found".into());
        assert_eq!(format!("{err}"), "registry error: window not found");
    }
}
