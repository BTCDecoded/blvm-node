//! Message bridge between blvm-consensus and transport layer
//!
//! Converts [`NetworkMessage`](blvm_protocol::network::NetworkMessage) values to/from transport bytes
//! via [`ProtocolAdapter`]. Outgoing [`NetworkResponse`] values are turned into wire payloads here.
//!
//! **Incoming message processing** (handshake, inv/getdata, block validation) is implemented in
//! [`NetworkManager`](crate::network::network_manager::NetworkManager) (`handle_incoming_wire_tcp`,
//! `wire_dispatch`). This module intentionally does not duplicate that path.

use crate::network::protocol_adapter::ProtocolAdapter;
use crate::network::transport::TransportType;
use anyhow::Result;
use blvm_protocol::network::{NetworkMessage as ConsensusNetworkMessage, NetworkResponse};
use tracing::debug;

/// Message bridge for connecting blvm-consensus message processing
/// with the transport layer
pub struct MessageBridge;

impl MessageBridge {
    /// Convert blvm-consensus NetworkMessage to transport wire format
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

    /// Convert transport wire format to blvm-consensus NetworkMessage
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

    /// Process a blvm-consensus NetworkResponse and extract messages to send
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
}
