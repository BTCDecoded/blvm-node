//! UTXO commitments handlers (GetUTXOSet, GetFilteredBlock).
//!
//! Requires utxo-commitments feature.

use crate::network::bandwidth_protection::ServiceType;
use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{ProtocolMessage, ProtocolParser};
use crate::network::protocol_extensions::{handle_get_filtered_block, handle_get_utxo_set};
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::warn;

#[cfg(feature = "utxo-commitments")]
impl NetworkManager {
    /// Handle GetUTXOSet request from a peer
    pub(crate) async fn handle_get_utxo_set_request(
        &self,
        data: Vec<u8>,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        match self
            .bandwidth_protection()
            .check_service_request(ServiceType::UtxoSet, peer_addr)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Err(anyhow::anyhow!(
                    "UTXO set bandwidth limit exceeded for peer {}",
                    peer_addr
                ));
            }
            Err(e) => {
                warn!("Bandwidth check error for UTXO set: {}", e);
                return Err(anyhow::anyhow!("Bandwidth check failed: {}", e));
            }
        }

        self.bandwidth_protection()
            .record_service_request(ServiceType::UtxoSet, peer_addr)
            .await;

        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let get_utxo_set_msg = match protocol_msg {
            ProtocolMessage::GetUTXOSet(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetUTXOSet message")),
        };

        let storage = self.storage().as_ref().map(Arc::clone);
        let response = handle_get_utxo_set(get_utxo_set_msg, storage).await?;

        let response_wire = ProtocolParser::serialize_message(&ProtocolMessage::UTXOSet(response))?;
        let response_bytes = response_wire.len() as u64;
        self.send_to_peer(peer_addr, response_wire).await?;

        self.bandwidth_protection()
            .record_service_bandwidth(ServiceType::UtxoSet, peer_addr, response_bytes)
            .await;

        Ok(())
    }

    /// Handle GetFilteredBlock request from a peer
    pub(crate) async fn handle_get_filtered_block_request(
        &self,
        data: Vec<u8>,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        match self
            .bandwidth_protection()
            .check_service_request(ServiceType::FilteredBlocks, peer_addr)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Err(anyhow::anyhow!(
                    "Filtered block bandwidth limit exceeded for peer {}",
                    peer_addr
                ));
            }
            Err(e) => {
                warn!("Bandwidth check error for filtered blocks: {}", e);
                return Err(anyhow::anyhow!("Bandwidth check failed: {}", e));
            }
        }

        self.bandwidth_protection()
            .record_service_request(ServiceType::FilteredBlocks, peer_addr)
            .await;

        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let get_filtered_block_msg = match protocol_msg {
            ProtocolMessage::GetFilteredBlock(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetFilteredBlock message")),
        };

        if let Err(e) = self
            .replay_protection()
            .check_request_id(get_filtered_block_msg.request_id)
            .await
        {
            warn!(
                "Replay protection: Rejected duplicate GetFilteredBlock request from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }

        let storage = self.storage().as_ref().map(Arc::clone);
        let response =
            handle_get_filtered_block(get_filtered_block_msg, storage, Some(self.filter_service()))
                .await?;

        let response_wire =
            ProtocolParser::serialize_message(&ProtocolMessage::FilteredBlock(response))?;
        let response_bytes = response_wire.len() as u64;
        self.send_to_peer(peer_addr, response_wire).await?;

        self.bandwidth_protection()
            .record_service_bandwidth(ServiceType::FilteredBlocks, peer_addr, response_bytes)
            .await;

        Ok(())
    }
}
