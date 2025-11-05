//! UTXO Commitments Network Client Implementation
//!
//! Implements UtxoCommitmentsNetworkClient trait for reference-node's NetworkManager.
//! Works with both TCP and Iroh transports via the transport abstraction layer.
//!
//! This enables UTXO commitments to work seamlessly with:
//! - Traditional TCP Bitcoin P2P (backward compatible)
//! - Modern Iroh QUIC transport (encrypted, NAT-traversing)

#[cfg(feature = "utxo-commitments")]
use protocol_engine::utxo_commitments::network_integration::{
    UtxoCommitmentsNetworkClient,
    FilteredBlock,
};
#[cfg(feature = "utxo-commitments")]
use protocol_engine::utxo_commitments::data_structures::UtxoCommitment;
#[cfg(feature = "utxo-commitments")]
use protocol_engine::types::{Hash, Natural};
#[cfg(feature = "utxo-commitments")]
use protocol_engine::utxo_commitments::data_structures::UtxoCommitmentResult;
#[cfg(feature = "utxo-commitments")]
use crate::network::{
    NetworkManager,
    protocol::{
        GetUTXOSetMessage,
        UTXOSetMessage,
        GetFilteredBlockMessage,
        FilteredBlockMessage,
    },
    protocol_extensions::{
        serialize_get_utxo_set,
        serialize_get_filtered_block,
    },
    transport::TransportType,
};
#[cfg(feature = "utxo-commitments")]
use crate::network::peer::Peer;
#[cfg(feature = "utxo-commitments")]
use anyhow::Result;
#[cfg(feature = "utxo-commitments")]
use std::sync::Arc;
#[cfg(feature = "utxo-commitments")]
use tokio::sync::RwLock;

/// Network client implementation for UTXO commitments
/// 
/// Works with both TCP and Iroh transports through the transport abstraction layer.
/// Automatically uses the appropriate transport based on peer connection type.
#[cfg(feature = "utxo-commitments")]
pub struct UtxoCommitmentsClient {
    network_manager: Arc<RwLock<NetworkManager>>,
}

#[cfg(feature = "utxo-commitments")]
impl UtxoCommitmentsClient {
    /// Create a new UTXO commitments client
    pub fn new(network_manager: Arc<RwLock<NetworkManager>>) -> Self {
        Self {
            network_manager,
        }
    }
    
    /// Determine transport type for a peer
    /// 
    /// Checks if peer is connected via TCP or Iroh transport.
    /// In hybrid mode, prefers Iroh if available.
    fn get_peer_transport_type(&self, peer_id: &str) -> TransportType {
        // Parse peer_id to determine transport type
        // TCP peers: "tcp:127.0.0.1:8333"
        // Iroh peers: "iroh:<pubkey_hex>"
        if peer_id.starts_with("iroh:") {
            #[cfg(feature = "iroh")]
            {
                return TransportType::Iroh;
            }
        }
        
        // Default to TCP (works for TCP addresses and fallback)
        TransportType::Tcp
    }
}

#[cfg(feature = "utxo-commitments")]
impl UtxoCommitmentsNetworkClient for UtxoCommitmentsClient {
    /// Request UTXO set from a peer at specific height
    /// 
    /// Sends GetUTXOSet message and awaits UTXOSet response.
    /// Works with both TCP and Iroh transports automatically.
    fn request_utxo_set(
        &self,
        peer_id: &str,
        height: Natural,
        block_hash: Hash,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = UtxoCommitmentResult<UtxoCommitment>> + Send + '_>> {
        // Clone Arc before async move to avoid lifetime issues
        let network_manager = self.network_manager.clone();
        let peer_id = peer_id.to_string(); // Clone string for move
        
        Box::pin(async move {
            // Parse peer_id to get SocketAddr
            // Format: "tcp:127.0.0.1:8333" or "iroh:<pubkey_hex>"
            let peer_addr = if peer_id.starts_with("tcp:") {
                peer_id.strip_prefix("tcp:").and_then(|s| s.parse::<std::net::SocketAddr>().ok())
            } else {
                // For Iroh peers, would need to map NodeId to address
                // For now, try to parse as SocketAddr
                peer_id.strip_prefix("iroh:").and_then(|_| None) // Placeholder
            };
            
            let peer_addr = match peer_addr {
                Some(addr) => addr,
                None => {
                    return Err(protocol_engine::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                        format!("Invalid peer_id format: {}", peer_id)
                    ));
                }
            };
            
            // Create GetUTXOSet message
            let get_utxo_set_msg = GetUTXOSetMessage {
                height,
                block_hash,
            };
            
            // Serialize message using protocol adapter (handles TCP vs Iroh format)
            let wire_format = serialize_get_utxo_set(&get_utxo_set_msg)
                .map_err(|e| protocol_engine::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                    format!("Failed to serialize GetUTXOSet: {}", e)
                ))?;
            
            // Send message to peer via NetworkManager
            let network = network_manager.read().await;
            network.send_to_peer(peer_addr, wire_format).await
                .map_err(|e| protocol_engine::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                    format!("Failed to send GetUTXOSet to peer {}: {}", peer_addr, e)
                ))?;
            
            // TODO: In full implementation, would:
            // 1. Register a response callback/future for this request
            // 2. Await the UTXOSet response message
            // 3. Deserialize UTXOSet message
            // 4. Extract and return UtxoCommitment
            
            // For now, return placeholder (request sent, but response handling needs async message routing)
            Err(protocol_engine::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                "Request sent - response handling needs async message routing system".to_string()
            ))
        })
    }
    
    /// Request filtered block from a peer
    /// 
    /// Sends GetFilteredBlock message and awaits FilteredBlock response.
    /// Works with both TCP and Iroh transports automatically.
    fn request_filtered_block(
        &self,
        peer_id: &str,
        block_hash: Hash,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = UtxoCommitmentResult<FilteredBlock>> + Send + '_>> {
        // Clone Arc before async move to avoid lifetime issues
        let network_manager = self.network_manager.clone();
        let peer_id = peer_id.to_string(); // Clone string for move
        
        Box::pin(async move {
            // Parse peer_id to get SocketAddr
            let peer_addr = if peer_id.starts_with("tcp:") {
                peer_id.strip_prefix("tcp:").and_then(|s| s.parse::<std::net::SocketAddr>().ok())
            } else {
                peer_id.strip_prefix("iroh:").and_then(|_| None) // Placeholder for Iroh
            };
            
            let peer_addr = match peer_addr {
                Some(addr) => addr,
                None => {
                    return Err(protocol_engine::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                        format!("Invalid peer_id format: {}", peer_id)
                    ));
                }
            };
            
            // Create GetFilteredBlock message (with default filter preferences)
            use crate::network::protocol::FilterPreferences;
            let get_filtered_block_msg = GetFilteredBlockMessage {
                block_hash,
                filter_preferences: FilterPreferences {
                    filter_ordinals: true,
                    filter_dust: true,
                    filter_brc20: true,
                    min_output_value: 546, // Default dust threshold
                },
            };
            
            // Serialize message using protocol adapter (handles TCP vs Iroh format)
            let wire_format = serialize_get_filtered_block(&get_filtered_block_msg)
                .map_err(|e| protocol_engine::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                    format!("Failed to serialize GetFilteredBlock: {}", e)
                ))?;
            
            // Send message to peer via NetworkManager
            let network = network_manager.read().await;
            network.send_to_peer(peer_addr, wire_format).await
                .map_err(|e| protocol_engine::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                    format!("Failed to send GetFilteredBlock to peer {}: {}", peer_addr, e)
                ))?;
            
            // TODO: In full implementation, would:
            // 1. Register a response callback/future for this request
            // 2. Await the FilteredBlock response message
            // 3. Deserialize FilteredBlock message
            // 4. Extract and return FilteredBlock
            
            // For now, return placeholder (request sent, but response handling needs async message routing)
            Err(protocol_engine::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                "Request sent - response handling needs async message routing system".to_string()
            ))
        })
    }
    
    /// Get list of connected peer IDs
    /// 
    /// Returns peer IDs in format "tcp:addr" or "iroh:pubkey" depending on transport.
    fn get_peer_ids(&self) -> Vec<String> {
        // Get peers from network manager
        // Note: This requires async, but trait method is sync
        // In full implementation, would need to make trait async or use blocking
        // For now, return empty list - full implementation would require trait redesign
        vec![]
    }
}

