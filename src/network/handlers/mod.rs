//! Message handlers for NetworkManager.
//!
//! Extracted from network_manager.rs to reduce file size and improve maintainability.

pub mod addr;
pub mod ban_list;
pub mod bip157;
pub mod module;
pub mod package_relay;
#[cfg(feature = "utxo-commitments")]
pub mod utxo_commitments;
