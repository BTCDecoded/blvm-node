//! Node API for modules
//!
//! Provides the API implementation that modules use to query node state.

pub mod blockchain;
pub mod events;
pub mod hub;
pub mod node_api;
pub mod node_api_ipc;

pub use events::EventManager;
pub use node_api::NodeApiImpl;
pub use node_api_ipc::NodeApiIpc;
