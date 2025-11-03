//! IPC (Inter-Process Communication) layer for modules
//! 
//! Handles communication between module processes and the base node using
//! Unix domain sockets and length-delimited binary messages.

pub mod protocol;
pub mod client;
pub mod server;

pub use protocol::{ModuleMessage, MessageType, RequestMessage, ResponseMessage, EventMessage};
pub use client::ModuleIpcClient;
pub use server::ModuleIpcServer;

