//! IPC message types shared between `stmd` daemon and `stm` CLI.
//!
//! All IPC uses newline-delimited JSON over a Windows named pipe (`\\.\pipe\stm`).
//! This module defines the message and response enums — platform-independent,
//! testable on any OS.

pub mod dispatch;
pub mod message;
#[cfg(target_os = "windows")]
pub mod transport;

pub use dispatch::dispatch;
pub use message::{SocketMessage, SocketResponse};
