//! Network layer for blvm-node
//!
//! This module provides P2P networking, peer management, and Bitcoin protocol
//! message handling for communication with other Bitcoin nodes.

pub mod address_db;
pub mod address_table;
pub mod ban_list_merging;
pub mod ban_list_signing;
pub mod bandwidth_protection;
pub mod chain_access;
pub mod connection_manager;
pub mod dns_seeds;
pub mod dos_protection;
pub mod erlay;
pub mod ibd_pin;
pub mod ibd_protection;
pub mod inventory;
pub mod lan_discovery;
pub mod lan_security;
pub mod message_bridge;
pub mod module_registry_extensions;
pub mod network_manager;
pub mod peer;
pub mod peer_manager;
pub mod peer_scoring;
pub mod protocol;
pub mod protocol_adapter;
pub mod protocol_extensions;
pub mod relay;
pub mod replay_protection;
pub mod tcp_transport;
pub mod transport;

#[cfg(feature = "quinn")]
pub mod quinn_transport;

#[cfg(feature = "iroh")]
pub mod iroh_transport;

#[cfg(feature = "utxo-commitments")]
pub mod utxo_commitments_client;

// Compact Block Relay (BIP152)
pub mod compact_blocks;

// Block Filter Service (BIP157/158)
mod background_tasks;
pub mod bip157_handler;
mod block_announced_sync;
pub mod filter_service;
mod getdata_serve;
mod handlers;
pub(crate) mod header_work;
mod network_message_dispatch;
mod package_messages;
pub(crate) mod parse_offload;
mod peer_connections;
mod startup;
mod tx_inflight;
mod wire_dispatch;
// Payment Protocol (BIP70) - P2P handlers
// Privacy and Performance Enhancements
#[cfg(feature = "dandelion")]
pub mod dandelion; // Dandelion++ privacy-preserving transaction relay
pub mod package_relay; // BIP 331 Package Relay
pub mod package_relay_handler; // BIP 331 handlers
// Stratum V2: P2P TLV demux → `StratumV2MessageReceived`; dedicated miner TCP is owned by
// `blvm-stratum-v2`. `NodeAPI::send_peer_transport_payload` sends opaque bytes on P2P.
pub mod txhash; // Non-consensus hashing helpers for relay
#[cfg(feature = "bip324")]
pub mod v2_negotiation;

use std::net::SocketAddr;
use tokio::sync::mpsc;

pub use connection_manager::{ConnectionManager, NetworkIO};
pub use ibd_pin::{ibd_peers_pinned, ibd_pin_contains, ibd_pin_peers, parse_ibd_pin_peers};
pub use network_manager::NetworkManager;
pub use peer_manager::{PeerByteRateLimiter, PeerManager, PeerRateLimiter};
pub use transport::{TransportAddr, TransportPreference};

/// Network message types (central enum for network layer)
#[derive(Debug, Clone)]
pub enum NetworkMessage {
    PeerConnected(TransportAddr),
    PeerDisconnected(TransportAddr),
    BlockReceived(Vec<u8>),
    TransactionReceived(Vec<u8>),
    InventoryReceived(Vec<u8>, SocketAddr),
    #[cfg(feature = "utxo-commitments")]
    UTXOSetReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    #[cfg(feature = "utxo-commitments")]
    FilteredBlockReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    #[cfg(feature = "utxo-commitments")]
    GetUTXOSetReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    #[cfg(feature = "utxo-commitments")]
    GetFilteredBlockReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    #[cfg(feature = "stratum-v2")]
    StratumV2MessageReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // Raw message received from peer (needs processing)
    RawMessageReceived(Vec<u8>, crate::network::transport::PeerIdentity), // (data, peer)
    // Headers response (for IBD)
    HeadersReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // BIP157 Block Filter messages
    GetCfiltersReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    GetCfheadersReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    GetCfcheckptReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // BIP331 Package Relay messages
    PkgTxnReceived(Vec<u8>, SocketAddr),     // (data, peer_addr)
    SendPkgTxnReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // Module Registry messages
    GetModuleReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    ModuleReceived(Vec<u8>, SocketAddr),    // (data, peer_addr)
    GetModuleByHashReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    ModuleByHashReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    GetModuleListReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    ModuleListReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // BIP70 Payment Protocol messages
    GetPaymentRequestReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    PaymentRequestReceived(Vec<u8>, SocketAddr),    // (data, peer_addr)
    PaymentReceived(Vec<u8>, SocketAddr),           // (data, peer_addr)
    PaymentACKReceived(Vec<u8>, SocketAddr),        // (data, peer_addr)
    // CTV Payment Proof messages
    #[cfg(feature = "ctv")]
    PaymentProofReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    /// Outbound payment proof queued by PaymentStateMachine for NetworkManager delivery.
    #[cfg(feature = "ctv")]
    SendPaymentProof(Vec<u8>, SocketAddr), // (wire payload, peer_addr)
    SettlementNotificationReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // Mesh networking packets
    MeshPacketReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
}

#[cfg(test)]
mod tests;
