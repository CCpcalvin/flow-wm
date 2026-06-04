//! Project-wide error types.

use std::fmt;

/// All errors produced by stm subsystems.
#[derive(Debug)]
pub enum StmError {
    /// Configuration parsing or validation failure.
    Config(String),
    /// Layout computation failure (e.g. invalid state).
    Layout(String),
    /// I/O error (file read/write, socket, etc.).
    Io(std::io::Error),
}

impl fmt::Display for StmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(s) => write!(f, "config error: {s}"),
            Self::Layout(s) => write!(f, "layout error: {s}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for StmError {}

impl From<std::io::Error> for StmError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Convenience alias used across the project.
pub type StmResult<T> = Result<T, StmError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_config_error() {
        let err = StmError::Config("bad yaml".into());
        assert_eq!(format!("{err}"), "config error: bad yaml");
    }

    #[test]
    fn display_layout_error() {
        let err = StmError::Layout("no focused window".into());
        assert_eq!(format!("{err}"), "layout error: no focused window");
    }
}
