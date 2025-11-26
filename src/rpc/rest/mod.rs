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
pub mod network;
pub mod server;
pub mod transactions;
pub mod types;

pub use server::RestApiServer;
