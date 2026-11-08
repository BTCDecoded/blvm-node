//! Module system for blvm-node
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

/// Module ids are `{name}_{32 hex}`. Names may contain underscores.
/// A suffix that is not 32 hex characters is part of the name.
pub fn module_name_from_id(module_id: &str) -> &str {
    if let Some((name, suffix)) = module_id.rsplit_once('_') {
        if suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return name;
        }
    }
    module_id
}
pub mod encryption;
pub mod github_release_install;
pub mod http;
pub mod integration;
pub mod inter_module;
pub mod ipc;
pub mod loader;
pub mod manager;
pub mod metrics;
pub mod pipeline;
pub mod process;
pub mod registry;
pub mod rpc;
pub mod sandbox;
pub mod security;
pub mod services;
pub mod timers;
pub mod traits;
pub mod validation;
pub mod version_constraint;
#[cfg(feature = "wasm-modules")]
pub mod wasm;
#[cfg(feature = "module-watcher")]
pub mod watcher;

pub use security::{Permission, PermissionChecker, PermissionSet, RequestValidator};

pub use api::NodeApiIpc;
pub use encryption::ModuleEncryption;
pub use integration::ModuleIntegration;
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
