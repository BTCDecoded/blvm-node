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

pub mod traits;
pub mod manager;
pub mod ipc;
pub mod process;
pub mod registry;
pub mod loader;
pub mod api;
pub mod security;
pub mod validation;
pub mod sandbox;

pub use security::{Permission, PermissionSet, PermissionChecker, RequestValidator};

pub use traits::{Module, NodeAPI, ModuleContext, ModuleError, ModuleState, ModuleMetadata};
pub use manager::ModuleManager;
pub use process::{ModuleProcessSpawner, ModuleProcess, ModuleProcessMonitor};

