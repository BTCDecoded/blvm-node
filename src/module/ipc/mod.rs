//! IPC (Inter-Process Communication) layer for modules
//!
//! Handles communication between module processes and the base node.
//! Unix: domain sockets. Windows: named pipes.

#[cfg(unix)]
pub mod client;
#[cfg(windows)]
pub mod client_windows;
pub mod protocol;
pub mod server;

#[cfg(unix)]
pub use client::ModuleIpcClient;
#[cfg(windows)]
pub use client_windows::ModuleIpcClient;
pub use protocol::{
    EventMessage, EventPayload, MessageType, ModuleMessage, RequestMessage, ResponseMessage,
};
pub use server::ModuleIpcServer;
