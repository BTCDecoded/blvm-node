//! ZeroMQ notification publisher
//!
//! Provides ZMQ-based notifications for blockchain events compatible with
//! standard Bitcoin node notification interfaces.
//!
//! **BLLVM Differentiator**: ZMQ notifications are enabled by default in BLLVM node,
//! making real-time blockchain event notifications available out of the box without
//! requiring special build flags or configuration.
//!
//! Supports the following notification types:
//! - `hashblock`: Block hash notifications
//! - `hashtx`: Transaction hash notifications
//! - `rawblock`: Raw block data notifications
//! - `rawtx`: Raw transaction data notifications
//! - `sequence`: Sequence notifications for mempool events

#[cfg(feature = "zmq")]
pub mod publisher;

#[cfg(feature = "zmq")]
pub use publisher::{ZmqConfig, ZmqPublisher};

#[cfg(all(test, feature = "zmq"))]
mod tests;
