//! IPC (Inter-Process Communication) layer for modules
//!
//! Handles communication between module processes and the base node using
//! Unix domain sockets and length-delimited binary messages.

pub mod client;
pub mod protocol;
pub mod server;

pub use client::ModuleIpcClient;
pub use protocol::{EventMessage, MessageType, ModuleMessage, RequestMessage, ResponseMessage};
pub use server::ModuleIpcServer;
