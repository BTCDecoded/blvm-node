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
#[cfg(feature = "ctv")]
#[cfg(feature = "bip70-http")]
pub mod congestion;
pub mod fees;
pub mod mempool;
pub mod network;
#[cfg(feature = "bip70-http")]
pub mod payment;
#[cfg(feature = "ctv")]
#[cfg(feature = "bip70-http")]
pub mod pool;
pub mod server;
pub mod transactions;
pub mod types;
pub mod validation;
#[cfg(feature = "ctv")]
#[cfg(feature = "bip70-http")]
pub mod vault;

#[cfg(test)]
mod tests;

pub use server::RestApiServer;
