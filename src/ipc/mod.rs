//! IPC message types shared between `flowd` daemon and `flow` CLI.
//!
//! All IPC uses newline-delimited JSON over a Windows named pipe (`\\.\pipe\flow`).
//! This module defines the message and response enums and the transport layer.
//!
//! Command dispatch is handled by
//! [`FlowWM::dispatch()`](crate::daemon::FlowWM::dispatch)
//! which has direct access to all subsystems.

pub mod message;
pub mod transport;

pub use message::{SocketMessage, SocketResponse};
