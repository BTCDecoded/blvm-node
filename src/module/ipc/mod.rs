//! IPC (Inter-Process Communication) layer for modules
//!
//! Handles communication between module processes and the base node.
//! Unix: domain sockets. Windows: named pipes.

use tokio_util::codec::LengthDelimitedCodec;

/// Maximum length-delimited frame size for module IPC (above max P2P message / large RPC payloads).
pub(crate) const MODULE_IPC_MAX_FRAME_LENGTH: usize = 40 * 1024 * 1024;

pub(crate) fn module_ipc_length_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MODULE_IPC_MAX_FRAME_LENGTH)
        .new_codec()
}

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
