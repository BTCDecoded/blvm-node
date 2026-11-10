//! Inter-module communication infrastructure
//!
//! Provides the infrastructure for modules to communicate with each other
//! through the node. The node acts as a mediator, routing requests and
//! validating permissions.

pub mod api;
pub mod ipc_proxy;
pub mod registry;
pub mod router;

pub use api::ModuleAPI;
pub use ipc_proxy::IpcForwardingModuleAPI;
pub use registry::ModuleApiRegistry;
pub use router::ModuleRouter;
