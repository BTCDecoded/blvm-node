//! Message handlers for NetworkManager.
//!
//! Extracted from network_manager.rs to reduce file size and improve maintainability.

pub mod addr;
pub mod ban_list;
pub mod bip157;
#[cfg(feature = "governance")]
pub mod governance;
pub mod module;
pub mod package_relay;
pub mod payment;
#[cfg(feature = "utxo-commitments")]
pub mod utxo_commitments;
