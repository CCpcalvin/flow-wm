//! IPC message types shared between `stmd` daemon and `stm` CLI.
//!
//! All IPC uses newline-delimited JSON over a Windows named pipe (`\\.\pipe\stm`).
//! This module defines the message and response enums, the transport layer,
//! and the fallback command dispatcher.
//!
//! The primary command dispatch is handled by
//! [`ScrollTilingManager::dispatch()`](crate::daemon::ScrollTilingManager::dispatch)
//! which has direct access to all subsystems. The base [`dispatch`] function
//! here is kept as a fallback and for standalone testing.

pub mod dispatch;
pub mod message;
pub mod transport;

pub use dispatch::dispatch;
pub use message::{SocketMessage, SocketResponse};
