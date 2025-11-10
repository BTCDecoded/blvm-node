//! Message bridge between consensus-proof and transport layer
//!
//! Provides conversion between consensus-proof NetworkMessage types
//! and the transport layer's message format.

use crate::network::protocol_adapter::ProtocolAdapter;
use crate::network::transport::TransportType;
use anyhow::Result;
use bllvm_protocol::network::{NetworkMessage as ConsensusNetworkMessage, NetworkResponse};
use tracing::debug;

/// Message bridge for connecting consensus-proof message processing
/// with the transport layer
pub struct MessageBridge;

impl MessageBridge {
    /// Convert consensus-proof NetworkMessage to transport wire format
    pub fn to_transport_message(
        msg: &ConsensusNetworkMessage,
        transport: TransportType,
    ) -> Result<Vec<u8>> {
        debug!(
            "Converting consensus message to transport format: {:?}",
            transport
        );
        ProtocolAdapter::serialize_message(msg, transport)
    }

    /// Convert transport wire format to consensus-proof NetworkMessage
    pub fn from_transport_message(
        data: &[u8],
        transport: TransportType,
    ) -> Result<ConsensusNetworkMessage> {
        debug!(
            "Converting transport message to consensus format: {:?}",
            transport
        );
        ProtocolAdapter::deserialize_message(data, transport)
    }

    /// Process a consensus-proof NetworkResponse and extract messages to send
    ///
    /// NetworkResponse can indicate sending one or multiple messages,
    /// or other actions (Ok, Reject).
    pub fn extract_send_messages(
        response: &NetworkResponse,
        transport: TransportType,
    ) -> Result<Vec<Vec<u8>>> {
        match response {
            NetworkResponse::Ok => {
                Ok(Vec::new()) // No messages to send
            }
            NetworkResponse::SendMessage(msg) => {
                let wire = Self::to_transport_message(msg, transport)?;
                Ok(vec![wire])
            }
            NetworkResponse::SendMessages(msgs) => {
                let mut wires = Vec::new();
                for msg in msgs {
                    let wire = Self::to_transport_message(msg, transport)?;
                    wires.push(wire);
                }
                Ok(wires)
            }
            NetworkResponse::Reject(reason) => {
                debug!("Message rejected: {}", reason);
                Ok(Vec::new()) // Rejection doesn't send a message
            }
        }
    }

    /// Process incoming transport message and generate response
    ///
    /// Takes transport bytes, converts to protocol message, processes it,
    /// and returns wire format messages to send back (if any).
    ///
    /// NOTE: This is a future integration point. To fully implement, this would:
    /// 1. Take `&BitcoinProtocolEngine`, `&mut PeerState`, and `Option<&dyn ChainStateAccess>`
    /// 2. Call `bllvm_protocol::network::process_network_message()` with these parameters
    /// 3. Convert the `NetworkResponse` to wire format messages using `extract_send_messages()`
    ///
    /// For now, this method only handles message conversion, not processing.
    pub fn process_incoming_message(data: &[u8], transport: TransportType) -> Result<Vec<Vec<u8>>> {
        // Convert to protocol message
        let protocol_msg = Self::from_transport_message(data, transport)?;

        // Note: Protocol layer processing is handled in NetworkManager::handle_incoming_wire_tcp
        // This function provides message extraction for transport layer
        // This requires:
        // - BitcoinProtocolEngine instance
        // - PeerState management
        // - ChainStateAccess implementation
        // - UTXO set and height for block/tx validation
        debug!("Converted incoming protocol message: {:?}", protocol_msg);

        // Return empty for now - actual processing would be done by network handlers
        // that have access to protocol engine and chain state
        Ok(Vec::new())
    }
}
