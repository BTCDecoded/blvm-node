//! IPC (Inter-Process Communication) layer for modules
//!
//! Handles communication between module processes and the base node using
//! Unix domain sockets and length-delimited binary messages.
//!
//! NOTE: This module is Unix-only (uses Unix domain sockets)

#[cfg(unix)]
pub mod client;
pub mod protocol;
#[cfg(unix)]
pub mod server;

#[cfg(unix)]
pub use client::ModuleIpcClient;
pub use protocol::{EventMessage, MessageType, ModuleMessage, RequestMessage, ResponseMessage};
#[cfg(unix)]
pub use server::ModuleIpcServer;
