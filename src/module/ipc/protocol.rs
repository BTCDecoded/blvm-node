//! IPC message protocol
//!
//! Defines the message types and serialization for IPC communication
//! between modules and the base node.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::module::traits::EventType;
use crate::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};

/// Correlation ID for matching requests with responses
pub type CorrelationId = u64;

/// Main IPC message wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModuleMessage {
    /// Request from module to node
    Request(RequestMessage),
    /// Response from node to module
    Response(ResponseMessage),
    /// Event notification from node to module
    Event(EventMessage),
}

impl ModuleMessage {
    /// Get the correlation ID if this is a request/response
    pub fn correlation_id(&self) -> Option<CorrelationId> {
        match self {
            ModuleMessage::Request(req) => Some(req.correlation_id),
            ModuleMessage::Response(resp) => Some(resp.correlation_id),
            ModuleMessage::Event(_) => None,
        }
    }

    /// Get message type
    pub fn message_type(&self) -> MessageType {
        match self {
            ModuleMessage::Request(req) => req.request_type.clone(),
            ModuleMessage::Response(_resp) => MessageType::Response,
            ModuleMessage::Event(_) => MessageType::Event,
        }
    }
}

/// Message type classification
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageType {
    /// Request messages
    GetBlock,
    GetBlockHeader,
    GetTransaction,
    HasTransaction,
    GetChainTip,
    GetBlockHeight,
    GetUtxo,
    SubscribeEvents,
    Handshake,
    /// Response messages
    Response,
    /// Event messages
    Event,
    /// Error response
    Error,
}

/// Request message from module to node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestMessage {
    pub correlation_id: CorrelationId,
    pub request_type: MessageType,
    pub payload: RequestPayload,
}

/// Request payload types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RequestPayload {
    /// Handshake: Module identifies itself (first message)
    Handshake {
        module_id: String,
        module_name: String,
        version: String,
    },
    GetBlock {
        hash: Hash,
    },
    GetBlockHeader {
        hash: Hash,
    },
    GetTransaction {
        hash: Hash,
    },
    HasTransaction {
        hash: Hash,
    },
    GetChainTip,
    GetBlockHeight,
    GetUtxo {
        outpoint: OutPoint,
    },
    SubscribeEvents {
        event_types: Vec<EventType>,
    },
}

/// Response message from node to module
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMessage {
    pub correlation_id: CorrelationId,
    pub success: bool,
    pub payload: Option<ResponsePayload>,
    pub error: Option<String>,
}

/// Response payload types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResponsePayload {
    /// Handshake acknowledgment with node version
    HandshakeAck {
        node_version: String,
    },
    Block(Option<Block>),
    BlockHeader(Option<BlockHeader>),
    Transaction(Option<Transaction>),
    Bool(bool),
    Hash(Hash),
    U64(u64),
    Utxo(Option<UTXO>),
    SubscribeAck,
}

/// Event message from node to subscribed modules
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMessage {
    pub event_type: EventType,
    pub payload: EventPayload,
}

/// Event payload types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventPayload {
    NewBlock { block_hash: Hash, height: u64 },
    NewTransaction { tx_hash: Hash },
    BlockDisconnected { hash: Hash, height: u64 },
    ChainReorg { old_tip: Hash, new_tip: Hash },
}

/// Helper to create request messages
impl RequestMessage {
    pub fn get_block(correlation_id: CorrelationId, hash: Hash) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::GetBlock,
            payload: RequestPayload::GetBlock { hash },
        }
    }

    pub fn get_block_header(correlation_id: CorrelationId, hash: Hash) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::GetBlockHeader,
            payload: RequestPayload::GetBlockHeader { hash },
        }
    }

    pub fn get_transaction(correlation_id: CorrelationId, hash: Hash) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::GetTransaction,
            payload: RequestPayload::GetTransaction { hash },
        }
    }

    pub fn has_transaction(correlation_id: CorrelationId, hash: Hash) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::HasTransaction,
            payload: RequestPayload::HasTransaction { hash },
        }
    }

    pub fn get_chain_tip(correlation_id: CorrelationId) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::GetChainTip,
            payload: RequestPayload::GetChainTip,
        }
    }

    pub fn get_block_height(correlation_id: CorrelationId) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::GetBlockHeight,
            payload: RequestPayload::GetBlockHeight,
        }
    }

    pub fn get_utxo(correlation_id: CorrelationId, outpoint: OutPoint) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::GetUtxo,
            payload: RequestPayload::GetUtxo { outpoint },
        }
    }

    pub fn subscribe_events(correlation_id: CorrelationId, event_types: Vec<EventType>) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::SubscribeEvents,
            payload: RequestPayload::SubscribeEvents { event_types },
        }
    }
}

/// Helper to create response messages
impl ResponseMessage {
    pub fn success(correlation_id: CorrelationId, payload: ResponsePayload) -> Self {
        Self {
            correlation_id,
            success: true,
            payload: Some(payload),
            error: None,
        }
    }

    pub fn error(correlation_id: CorrelationId, error: String) -> Self {
        Self {
            correlation_id,
            success: false,
            payload: None,
            error: Some(error),
        }
    }
}
