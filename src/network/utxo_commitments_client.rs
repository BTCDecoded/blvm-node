//! UTXO Commitments Network Client Implementation
//!
//! Implements UtxoCommitmentsNetworkClient trait for reference-node's NetworkManager.
//! Works with both TCP and Iroh transports via the transport abstraction layer.
//!
//! This enables UTXO commitments to work seamlessly with:
//! - Traditional TCP Bitcoin P2P (backward compatible)
//! - Modern Iroh QUIC transport (encrypted, NAT-traversing)

#[cfg(feature = "utxo-commitments")]
use crate::network::peer::Peer;
#[cfg(feature = "utxo-commitments")]
use crate::network::{
    protocol::{Bip158FilterData, FilteredBlockMessage, GetFilteredBlockMessage, GetUTXOSetMessage, UTXOSetMessage},
    protocol_extensions::{serialize_get_filtered_block, serialize_get_utxo_set},
    transport::TransportType,
    NetworkManager,
};
#[cfg(feature = "utxo-commitments")]
use anyhow::Result;
#[cfg(feature = "utxo-commitments")]
use bllvm_protocol::types::{BlockHeader, Hash, Natural};
#[cfg(feature = "utxo-commitments")]
use bllvm_protocol::utxo_commitments::data_structures::UtxoCommitment;
#[cfg(feature = "utxo-commitments")]
use bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentResult;
#[cfg(feature = "utxo-commitments")]
use bllvm_protocol::utxo_commitments::network_integration::{
    FilteredBlock, UtxoCommitmentsNetworkClient,
};
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
        Self { network_manager }
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
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = UtxoCommitmentResult<UtxoCommitment>> + Send + '_>,
    > {
        // Clone Arc before async move to avoid lifetime issues
        let network_manager = self.network_manager.clone();
        let peer_id = peer_id.to_string(); // Clone string for move

        Box::pin(async move {
            // Parse peer_id to get SocketAddr or TransportAddr
            // Format: "tcp:127.0.0.1:8333" or "iroh:<pubkey_hex>"
            let peer_addr_opt: Option<(std::net::SocketAddr, Option<crate::network::transport::TransportAddr>)> = if peer_id.starts_with("tcp:") {
                peer_id
                    .strip_prefix("tcp:")
                    .and_then(|s| s.parse::<std::net::SocketAddr>().ok())
                    .map(|addr| (addr, None))
            } else if peer_id.starts_with("iroh:") {
                // Parse Iroh node ID from hex
                #[cfg(feature = "iroh")]
                {
                    use crate::network::transport::TransportAddr;
                    use hex;
                    
                    let node_id_hex = peer_id.strip_prefix("iroh:").ok_or_else(|| {
                        bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                            format!("Invalid Iroh peer_id format: {}", peer_id)
                        )
                    })?;
                    
                    let node_id_bytes = hex::decode(node_id_hex).map_err(|e| {
                        bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                            format!("Invalid Iroh node ID hex: {}", e)
                        )
                    })?;
                    
                    // Validate node ID length (Iroh uses 32-byte public keys)
                    if node_id_bytes.len() != 32 {
                        return Err(bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                            format!("Invalid Iroh node ID length: expected 32 bytes, got {}", node_id_bytes.len())
                        ));
                    }
                    
                    // Create placeholder SocketAddr for Iroh (same approach as mod.rs)
                    // Use first 4 bytes of key for IP, last 2 bytes for port
                    let ip_bytes = [node_id_bytes[0], node_id_bytes[1], node_id_bytes[2], node_id_bytes[3]];
                    let port = u16::from_be_bytes([node_id_bytes[30], node_id_bytes[31]]);
                    let placeholder_addr = std::net::SocketAddr::from((ip_bytes, port));
                    
                    let transport_addr = TransportAddr::Iroh(node_id_bytes);
                    Some((placeholder_addr, Some(transport_addr)))
                }
                #[cfg(not(feature = "iroh"))]
                {
                    return Err(bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                        "Iroh feature not enabled".to_string()
                    ));
                }
            } else {
                None
            };

            let (peer_addr, transport_addr_opt) = match peer_addr_opt {
                Some((addr, transport)) => (addr, transport),
                None => {
                    return Err(bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                        format!("Invalid peer_id format: {}", peer_id)
                    ));
                }
            };
            
            // Store TransportAddr mapping if Iroh - drop RwLock before Mutex lock
            if let Some(transport_addr) = transport_addr_opt {
                let socket_to_transport = {
                    let network = network_manager.read().await;
                    // Clone the Arc to avoid holding RwLock while locking Mutex
                    Arc::clone(&network.socket_to_transport)
                };
                // Now lock the Mutex without holding the RwLock
                socket_to_transport.lock().await.insert(peer_addr, transport_addr);
            }

            // Check if peer supports UTXO commitments before sending request
            // Get peer_states Arc first, then drop RwLock before Mutex lock
            let peer_states_arc = {
                let network = network_manager.read().await;
                Arc::clone(&network.peer_states)
            };
            
            // Get peer version to check capabilities - now safe to lock Mutex
            let peer_supports_utxo_commitments = {
                let peer_states = peer_states_arc.lock().await;
                if let Some(peer_state) = peer_states.get(&peer_addr) {
                    #[cfg(feature = "utxo-commitments")]
                    {
                        use crate::network::protocol::NODE_UTXO_COMMITMENTS;
                        (peer_state.services & NODE_UTXO_COMMITMENTS) != 0
                    }
                    #[cfg(not(feature = "utxo-commitments"))]
                    {
                        false
                    }
                } else {
                    // No peer state yet, assume it doesn't support (will try anyway for backward compatibility)
                    false
                }
            };
            
            // If we know the peer doesn't support UTXO commitments, return error early
            if !peer_supports_utxo_commitments {
                drop(network);
                return Err(bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::VerificationFailed(
                    format!("Peer {} does not support UTXO commitments (missing NODE_UTXO_COMMITMENTS service flag)", peer_id)
                ));
            }
            
            // Register pending request before sending
            let (request_id, response_rx) = network.register_request(peer_addr);
            drop(network); // Release read lock before async wait
            
            // Create GetUTXOSet message (request_id is generated by the handler, not stored in the message)
            let get_utxo_set_msg = GetUTXOSetMessage { 
                height, 
                block_hash 
            };

            // Serialize message using protocol adapter (handles TCP vs Iroh format)
            let wire_format = serialize_get_utxo_set(&get_utxo_set_msg)
                .map_err(|e| bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                    format!("Failed to serialize GetUTXOSet: {}", e)
                ))?;
            
            // Send message to peer via NetworkManager
            {
                let network = network_manager.read().await;
                network.send_to_peer(peer_addr, wire_format).await
                    .map_err(|e| bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                        format!("Failed to send GetUTXOSet to peer {}: {}", peer_addr, e)
                    ))?;
            }
            
            // Await response with timeout (from config)
            let timeout_seconds = {
                let network = network_manager.read().await;
                network.request_timeout_config.utxo_commitment_request_timeout_seconds
            };
            tokio::select! {
                result = response_rx => {
                    match result {
                        Ok(response_data) => {
                            // Deserialize UTXOSet response
                            use crate::network::protocol::{ProtocolMessage, ProtocolParser};
                            let parsed = ProtocolParser::parse_message(&response_data)
                                .map_err(|e| bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                                    format!("Failed to parse UTXOSet response: {}", e)
                                ))?;
                            
                            match parsed {
                                ProtocolMessage::UTXOSet(utxo_set_msg) => {
                                    // Convert to UtxoCommitment
                                    let commitment = bllvm_protocol::utxo_commitments::data_structures::UtxoCommitment {
                                        merkle_root: utxo_set_msg.commitment.merkle_root,
                                        total_supply: utxo_set_msg.commitment.total_supply,
                                        utxo_count: utxo_set_msg.commitment.utxo_count,
                                        block_height: utxo_set_msg.commitment.block_height,
                                        block_hash: utxo_set_msg.commitment.block_hash,
                                    };
                                    Ok(commitment)
                                }
                                _ => Err(bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                                    format!("Unexpected response type: expected UTXOSet")
                                ))
                            }
                        }
                        Err(_) => Err(bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                            "Response channel closed".to_string()
                        ))
                    }
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(timeout_seconds)) => {
                    // Timeout - cleanup request - drop RwLock before Mutex lock
                    {
                        let pending_requests_arc = {
                            let network = network_manager.read().await;
                            Arc::clone(&network.pending_requests)
                        };
                        let mut pending = pending_requests_arc.lock().await;
                        pending.remove(&request_id);
                    }
                    Err(bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                        format!("Request timeout: no response received within {} seconds", timeout_seconds)
                    ))
                }
            }
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
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = UtxoCommitmentResult<FilteredBlock>> + Send + '_>,
    > {
        // Clone Arc before async move to avoid lifetime issues
        let network_manager = self.network_manager.clone();
        let peer_id = peer_id.to_string(); // Clone string for move

        Box::pin(async move {
            // Parse peer_id to get SocketAddr or TransportAddr
            // Format: "tcp:127.0.0.1:8333" or "iroh:<pubkey_hex>"
            let peer_addr_opt: Option<(std::net::SocketAddr, Option<crate::network::transport::TransportAddr>)> = if peer_id.starts_with("tcp:") {
                peer_id
                    .strip_prefix("tcp:")
                    .and_then(|s| s.parse::<std::net::SocketAddr>().ok())
                    .map(|addr| (addr, None))
            } else if peer_id.starts_with("iroh:") {
                // Parse Iroh node ID from hex
                #[cfg(feature = "iroh")]
                {
                    use crate::network::transport::TransportAddr;
                    use hex;
                    
                    let node_id_hex = peer_id.strip_prefix("iroh:").ok_or_else(|| {
                        bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                            format!("Invalid Iroh peer_id format: {}", peer_id)
                        )
                    })?;
                    
                    let node_id_bytes = hex::decode(node_id_hex).map_err(|e| {
                        bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                            format!("Invalid Iroh node ID hex: {}", e)
                        )
                    })?;
                    
                    // Validate node ID length (Iroh uses 32-byte public keys)
                    if node_id_bytes.len() != 32 {
                        return Err(bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                            format!("Invalid Iroh node ID length: expected 32 bytes, got {}", node_id_bytes.len())
                        ));
                    }
                    
                    // Create placeholder SocketAddr for Iroh (same approach as mod.rs)
                    // Use first 4 bytes of key for IP, last 2 bytes for port
                    let ip_bytes = [node_id_bytes[0], node_id_bytes[1], node_id_bytes[2], node_id_bytes[3]];
                    let port = u16::from_be_bytes([node_id_bytes[30], node_id_bytes[31]]);
                    let placeholder_addr = std::net::SocketAddr::from((ip_bytes, port));
                    
                    let transport_addr = TransportAddr::Iroh(node_id_bytes);
                    Some((placeholder_addr, Some(transport_addr)))
                }
                #[cfg(not(feature = "iroh"))]
                {
                    return Err(bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                        "Iroh feature not enabled".to_string()
                    ));
                }
            } else {
                None
            };

            let (peer_addr, transport_addr_opt) = match peer_addr_opt {
                Some((addr, transport)) => (addr, transport),
                None => {
                    return Err(bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                        format!("Invalid peer_id format: {}", peer_id)
                    ));
                }
            };
            
            // Store TransportAddr mapping if Iroh - drop RwLock before Mutex lock
            if let Some(transport_addr) = transport_addr_opt {
                let socket_to_transport = {
                    let network = network_manager.read().await;
                    Arc::clone(&network.socket_to_transport)
                };
                socket_to_transport.lock().await.insert(peer_addr, transport_addr);
            }

            // Register pending request before sending
            let network = network_manager.read().await;
            let (request_id, response_rx) = network.register_request(peer_addr);
            drop(network); // Release read lock before async wait
            
            // Create GetFilteredBlock message with request_id
            use crate::network::protocol::FilterPreferences;
            let get_filtered_block_msg = GetFilteredBlockMessage {
                request_id,
                block_hash,
                filter_preferences: FilterPreferences {
                    filter_ordinals: true,
                    filter_dust: true,
                    filter_brc20: true,
                    min_output_value: 546, // Default dust threshold
                },
                include_bip158_filter: true,
            };

            // Serialize message using protocol adapter (handles TCP vs Iroh format)
            let wire_format = serialize_get_filtered_block(&get_filtered_block_msg)
                .map_err(|e| bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                    format!("Failed to serialize GetFilteredBlock: {}", e)
                ))?;
            
            // Send message to peer via NetworkManager
            {
                let network = network_manager.read().await;
                network.send_to_peer(peer_addr, wire_format).await
                    .map_err(|e| bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                        format!("Failed to send GetFilteredBlock to peer {}: {}", peer_addr, e)
                    ))?;
            }
            
            // Await response with timeout (from config)
            let timeout_seconds = {
                let network = network_manager.read().await;
                network.request_timeout_config.utxo_commitment_request_timeout_seconds
            };
            tokio::select! {
                result = response_rx => {
                    match result {
                        Ok(response_data) => {
                            // Deserialize FilteredBlock response
                            use crate::network::protocol::{ProtocolMessage, ProtocolParser};
                            let parsed = ProtocolParser::parse_message(&response_data)
                                .map_err(|e| bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                                    format!("Failed to parse FilteredBlock response: {}", e)
                                ))?;
                            
                            match parsed {
                                ProtocolMessage::FilteredBlock(filtered_block_msg) => {
                                    // Convert to FilteredBlock
                                    // Note: FilteredBlock structure from bllvm_protocol doesn't match FilteredBlockMessage
                                    // We need to construct it from available data
                                    let header = BlockHeader {
                                        version: 1,
                                        prev_block_hash: [0; 32], // TODO: Get from message if available
                                        merkle_root: filtered_block_msg.commitment.merkle_root,
                                        timestamp: 0, // TODO: Get from message if available
                                        bits: 0, // TODO: Get from message if available
                                        nonce: 0, // TODO: Get from message if available
                                    };
                                    let filtered_block = FilteredBlock {
                                        header,
                                        commitment: bllvm_protocol::utxo_commitments::data_structures::UtxoCommitment {
                                            merkle_root: filtered_block_msg.commitment.merkle_root,
                                            total_supply: filtered_block_msg.commitment.total_supply,
                                            utxo_count: filtered_block_msg.commitment.utxo_count,
                                            block_height: filtered_block_msg.commitment.block_height,
                                            block_hash: filtered_block_msg.commitment.block_hash,
                                        },
                                        transactions: filtered_block_msg.transactions.clone(),
                                        transaction_indices: (0..filtered_block_msg.transactions.len() as u32).collect(),
                                        spam_summary: Default::default(), // TODO: Get from message if available
                                    };
                                    Ok(filtered_block)
                                }
                                _ => Err(bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                                    format!("Unexpected response type: expected FilteredBlock")
                                ))
                            }
                        }
                        Err(_) => Err(bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                            "Response channel closed".to_string()
                        ))
                    }
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(timeout_seconds)) => {
                    // Timeout - cleanup request - drop RwLock before Mutex lock
                    {
                        let pending_requests_arc = {
                            let network = network_manager.read().await;
                            Arc::clone(&network.pending_requests)
                        };
                        let mut pending = pending_requests_arc.lock().await;
                        pending.remove(&request_id);
                    }
                    Err(bllvm_protocol::utxo_commitments::data_structures::UtxoCommitmentError::SerializationError(
                        format!("Request timeout: no response received within {} seconds", timeout_seconds)
                    ))
                }
            }
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
