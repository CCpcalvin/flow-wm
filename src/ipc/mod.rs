//! IPC message types shared between `stmd` daemon and `stm` CLI.
//!
//! All IPC uses newline-delimited JSON over a Windows named pipe (`\\.\pipe\stm`).
//! This module defines the message and response enums and the transport layer.
//!
//! Command dispatch is handled by
//! [`ScrollTilingManager::dispatch()`](crate::daemon::ScrollTilingManager::dispatch)
//! which has direct access to all subsystems.

pub mod message;
pub mod transport;

pub use message::{SocketMessage, SocketResponse};
