//! IPC message types shared between `stmd` daemon and `stm` CLI.
//!
//! All IPC uses newline-delimited JSON over a Windows named pipe (`\\.\pipe\stm`).
//! This module defines the message and response enums, the transport layer,
//! and the command dispatcher.

pub mod dispatch;
pub mod message;
pub mod transport;

pub use dispatch::dispatch;
pub use dispatch::dispatch_with_registry;
pub use message::{SocketMessage, SocketResponse};
