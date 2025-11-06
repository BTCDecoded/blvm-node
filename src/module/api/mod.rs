//! Node API for modules
//!
//! Provides the API implementation that modules use to query node state.

pub mod blockchain;
pub mod events;
pub mod hub;
pub mod node_api;

pub use events::EventManager;
pub use node_api::NodeApiImpl;
