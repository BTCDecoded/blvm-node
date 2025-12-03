//! Module system for reference-node
//!
//! This module provides process-isolated module support, enabling optional features
//! (Lightning, merge mining, privacy enhancements) without affecting consensus or
//! base node stability.
//!
//! ## Architecture
//!
//! - **Process Isolation**: Each module runs in separate process with isolated memory
//! - **API Boundaries**: Modules communicate only through well-defined APIs
//! - **Crash Containment**: Module failures don't propagate to base node
//! - **Consensus Isolation**: Modules cannot modify consensus rules, UTXO set, or block validation
//! - **State Separation**: Module state is completely separate from consensus state

pub mod api;
pub mod encryption;
pub mod hooks;
pub mod inter_module;
pub mod ipc;
pub mod loader;
pub mod manager;
pub mod metrics;
pub mod process;
pub mod registry;
pub mod rpc;
pub mod sandbox;
pub mod security;
pub mod timers;
pub mod traits;
pub mod validation;

pub use security::{Permission, PermissionChecker, PermissionSet, RequestValidator};

pub use encryption::ModuleEncryption;
pub use manager::ModuleManager;
pub use process::{ModuleProcess, ModuleProcessMonitor, ModuleProcessSpawner};
pub use traits::{
    EventType, Module, ModuleContext, ModuleError, ModuleMetadata, ModuleState, NodeAPI,
};

// Re-export IPC protocol types
pub use ipc::protocol::{EventMessage, EventPayload, ModuleMessage};

// Re-export IPC types conditionally
#[cfg(unix)]
pub use ipc::{ModuleIpcClient, ModuleIpcServer};
