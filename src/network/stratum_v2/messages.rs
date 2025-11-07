//! Stratum V2 Protocol Message Types
//!
//! Implements all standard Stratum V2 message types according to the specification:
//! https://stratumprotocol.org/

use crate::network::stratum_v2::error::{StratumV2Error, StratumV2Result};
use bllvm_protocol::types::{Block, Hash, Natural};
use serde::{Deserialize, Serialize};

/// Stratum V2 message type tags
pub mod message_types {
    // Setup Connection messages
    pub const SETUP_CONNECTION: u16 = 0x0001;
    pub const SETUP_CONNECTION_SUCCESS: u16 = 0x0002;
    pub const SETUP_CONNECTION_ERROR: u16 = 0x0003;

    // Mining channel messages
    pub const OPEN_MINING_CHANNEL: u16 = 0x0010;
    pub const OPEN_MINING_CHANNEL_SUCCESS: u16 = 0x0011;
    pub const OPEN_MINING_CHANNEL_ERROR: u16 = 0x0012;

    // Mining job messages
    pub const NEW_MINING_JOB: u16 = 0x0020;
    pub const SET_NEW_PREV_HASH: u16 = 0x0021;

    // Share submission messages
    pub const SUBMIT_SHARES: u16 = 0x0030;
    pub const SUBMIT_SHARES_SUCCESS: u16 = 0x0031;
    pub const SUBMIT_SHARES_ERROR: u16 = 0x0032;
}

/// Setup Connection message (client → server)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupConnectionMessage {
    /// Protocol version
    pub protocol_version: u16,
    /// Miner endpoint (identifies the miner)
    pub endpoint: String,
    /// Capabilities flags
    pub capabilities: Vec<String>,
}

/// Setup Connection Success message (server → client)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupConnectionSuccessMessage {
    /// Supported protocol versions
    pub supported_versions: Vec<u16>,
    /// Server capabilities
    pub capabilities: Vec<String>,
}

/// Setup Connection Error message (server → client)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupConnectionErrorMessage {
    /// Error code
    pub error_code: u16,
    /// Error message
    pub error_message: String,
}

/// Open Mining Channel message (client → server)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenMiningChannelMessage {
    /// Channel identifier
    pub channel_id: u32,
    /// Request ID
    pub request_id: u32,
    /// Minimum difficulty
    pub min_difficulty: u32,
}

/// Open Mining Channel Success message (server → client)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenMiningChannelSuccessMessage {
    /// Channel identifier
    pub channel_id: u32,
    /// Request ID
    pub request_id: u32,
    /// Target difficulty
    pub target: Hash,
    /// Maximum number of jobs
    pub max_jobs: u32,
}

/// Open Mining Channel Error message (server → client)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenMiningChannelErrorMessage {
    /// Request ID
    pub request_id: u32,
    /// Error code
    pub error_code: u16,
    /// Error message
    pub error_message: String,
}

/// New Mining Job message (server → client)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMiningJobMessage {
    /// Channel identifier
    pub channel_id: u32,
    /// Job identifier
    pub job_id: u32,
    /// Previous block hash
    pub prev_hash: Hash,
    /// Coinbase transaction prefix
    pub coinbase_prefix: Vec<u8>,
    /// Coinbase transaction suffix
    pub coinbase_suffix: Vec<u8>,
    /// Merkle path (for transaction inclusion)
    pub merkle_path: Vec<Hash>,
}

/// Set New Previous Hash message (server → client)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetNewPrevHashMessage {
    /// Channel identifier
    pub channel_id: u32,
    /// Job identifier
    pub job_id: u32,
    /// Previous block hash
    pub prev_hash: Hash,
    /// Minimum number of transactions
    pub min_txn_count: u32,
}

/// Share data for submission
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareData {
    /// Channel identifier
    pub channel_id: u32,
    /// Job identifier
    pub job_id: u32,
    /// Nonce
    pub nonce: u32,
    /// Version
    pub version: i64,
    /// Merkle root
    pub merkle_root: Hash,
}

/// Submit Shares message (client → server)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitSharesMessage {
    /// Channel identifier
    pub channel_id: u32,
    /// Share data
    pub shares: Vec<ShareData>,
}

/// Submit Shares Success message (server → client)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitSharesSuccessMessage {
    /// Channel identifier
    pub channel_id: u32,
    /// Last submitted job ID
    pub last_job_id: u32,
}

/// Submit Shares Error message (server → client)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitSharesErrorMessage {
    /// Channel identifier
    pub channel_id: u32,
    /// Job identifier
    pub job_id: u32,
    /// Error code
    pub error_code: u16,
    /// Error message
    pub error_message: String,
}

/// Trait for Stratum V2 message serialization
pub trait StratumV2Message: Serialize + for<'de> Deserialize<'de> {
    /// Get message type tag
    fn message_type(&self) -> u16;

    /// Serialize message to bytes (JSON format)
    fn to_bytes(&self) -> StratumV2Result<Vec<u8>> {
        let json = serde_json::to_vec(self)
            .map_err(|e| StratumV2Error::Serialization(format!("Failed to serialize: {}", e)))?;
        Ok(json)
    }

    /// Deserialize message from bytes
    fn from_bytes(data: &[u8]) -> StratumV2Result<Self>
    where
        Self: Sized,
    {
        let message: Self = serde_json::from_slice(data).map_err(|e| {
            StratumV2Error::Deserialization(format!("Failed to deserialize: {}", e))
        })?;
        Ok(message)
    }
}

// Implement StratumV2Message for all message types
impl StratumV2Message for SetupConnectionMessage {
    fn message_type(&self) -> u16 {
        message_types::SETUP_CONNECTION
    }
}

impl StratumV2Message for SetupConnectionSuccessMessage {
    fn message_type(&self) -> u16 {
        message_types::SETUP_CONNECTION_SUCCESS
    }
}

impl StratumV2Message for SetupConnectionErrorMessage {
    fn message_type(&self) -> u16 {
        message_types::SETUP_CONNECTION_ERROR
    }
}

impl StratumV2Message for OpenMiningChannelMessage {
    fn message_type(&self) -> u16 {
        message_types::OPEN_MINING_CHANNEL
    }
}

impl StratumV2Message for OpenMiningChannelSuccessMessage {
    fn message_type(&self) -> u16 {
        message_types::OPEN_MINING_CHANNEL_SUCCESS
    }
}

impl StratumV2Message for OpenMiningChannelErrorMessage {
    fn message_type(&self) -> u16 {
        message_types::OPEN_MINING_CHANNEL_ERROR
    }
}

impl StratumV2Message for NewMiningJobMessage {
    fn message_type(&self) -> u16 {
        message_types::NEW_MINING_JOB
    }
}

impl StratumV2Message for SetNewPrevHashMessage {
    fn message_type(&self) -> u16 {
        message_types::SET_NEW_PREV_HASH
    }
}

impl StratumV2Message for SubmitSharesMessage {
    fn message_type(&self) -> u16 {
        message_types::SUBMIT_SHARES
    }
}

impl StratumV2Message for SubmitSharesSuccessMessage {
    fn message_type(&self) -> u16 {
        message_types::SUBMIT_SHARES_SUCCESS
    }
}

impl StratumV2Message for SubmitSharesErrorMessage {
    fn message_type(&self) -> u16 {
        message_types::SUBMIT_SHARES_ERROR
    }
}
