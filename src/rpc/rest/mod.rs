//! Modern REST API Module
//!
//! Provides a developer-friendly REST API alongside the backwards-compatible JSON-RPC.
//! This API is designed for modern applications with better type safety, clearer
//! error messages, and improved developer experience.
//!
//! Base path: `/api/v1/`

pub mod addresses;
pub mod blocks;
pub mod chain;
pub mod fees;
pub mod mempool;
pub mod mining;
pub mod network;
pub mod node;
pub mod rbac;
pub mod server;
pub mod transactions;
pub mod types;
pub mod validation;

#[cfg(test)]
mod tests;

pub use server::RestApiServer;
