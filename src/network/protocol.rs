//! Bitcoin protocol message handling
//!
//! Implements Bitcoin P2P protocol message serialization and deserialization.

use bllvm_protocol::bip157::NODE_COMPACT_FILTERS;
use crate::network::transport::TransportType;
use anyhow::Result;
use bllvm_protocol::{Block, BlockHeader, Hash, Transaction};
use serde::{Deserialize, Serialize};

/// Bitcoin protocol constants
pub const BITCOIN_MAGIC_MAINNET: [u8; 4] = [0xf9, 0xbe, 0xb4, 0xd9];
pub const BITCOIN_MAGIC_TESTNET: [u8; 4] = [0x0b, 0x11, 0x09, 0x07];
pub const BITCOIN_MAGIC_REGTEST: [u8; 4] = [0xfa, 0xbf, 0xb5, 0xda];

/// Maximum protocol message size (32MB)
pub const MAX_PROTOCOL_MESSAGE_LENGTH: usize = 32 * 1024 * 1024;

/// Service flags (bitfield in Version.services)
#[cfg(feature = "dandelion")]
pub const NODE_DANDELION: u64 = 1 << 24;
pub const NODE_PACKAGE_RELAY: u64 = 1 << 25;
pub const NODE_FIBRE: u64 = 1 << 26;

/// Allowed Bitcoin protocol commands
pub const ALLOWED_COMMANDS: &[&str] = &[
    "version",
    "verack",
    "ping",
    "pong",
    "getheaders",
    "headers",
    "getblocks",
    "block",
    "getdata",
    "inv",
    "tx",
    "notfound",
    "getaddr",
    "addr",
    "mempool",
    "reject",
    "feefilter",
    "sendcmpct",
    "cmpctblock",
    "getblocktxn",
    "blocktxn",
    "getblocktxn",
    // UTXO commitment protocol extensions
    "getutxoset",
    "utxoset",
    "getfilteredblock",
    "filteredblock",
    // Block Filtering (BIP157)
    "getcfilters",
    "cfilter",
    "getcfheaders",
    "cfheaders",
    "getcfcheckpt",
    "cfcheckpt",
    // Payment Protocol (BIP70) - P2P variant
    "getpaymentrequest",
    "paymentrequest",
    "payment",
    "paymentack",
    // Package Relay (BIP 331)
    "sendpkgtxn",
    "pkgtxn",
    "pkgtxnreject",
    // Ban List Sharing
    "getbanlist",
    "banlist",
];

/// Bitcoin protocol message types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProtocolMessage {
    Version(VersionMessage),
    Verack,
    Ping(PingMessage),
    Pong(PongMessage),
    GetHeaders(GetHeadersMessage),
    Headers(HeadersMessage),
    GetBlocks(GetBlocksMessage),
    Block(BlockMessage),
    GetData(GetDataMessage),
    Inv(InvMessage),
    Tx(TxMessage),
    // Compact Block Relay (BIP152)
    SendCmpct(SendCmpctMessage),
    CmpctBlock(CompactBlockMessage),
    GetBlockTxn(GetBlockTxnMessage),
    BlockTxn(BlockTxnMessage),
    // UTXO commitment protocol extensions
    GetUTXOSet(GetUTXOSetMessage),
    UTXOSet(UTXOSetMessage),
    GetFilteredBlock(GetFilteredBlockMessage),
    FilteredBlock(FilteredBlockMessage),
    // Block Filtering (BIP157)
    GetCfilters(GetCfiltersMessage),
    Cfilter(CfilterMessage),
    GetCfheaders(GetCfheadersMessage),
    Cfheaders(CfheadersMessage),
    GetCfcheckpt(GetCfcheckptMessage),
    Cfcheckpt(CfcheckptMessage),
    // Payment Protocol (BIP70) - P2P variant
    GetPaymentRequest(GetPaymentRequestMessage),
    PaymentRequest(PaymentRequestMessage),
    Payment(PaymentMessage),
    PaymentACK(PaymentACKMessage),
    // Package Relay (BIP 331)
    SendPkgTxn(SendPkgTxnMessage),
    PkgTxn(PkgTxnMessage),
    PkgTxnReject(PkgTxnRejectMessage),
    // Ban List Sharing
    GetBanList(GetBanListMessage),
    BanList(BanListMessage),
}

/// Version message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionMessage {
    pub version: i32,
    pub services: u64,
    pub timestamp: i64,
    pub addr_recv: NetworkAddress,
    pub addr_from: NetworkAddress,
    pub nonce: u64,
    pub user_agent: String,
    pub start_height: i32,
    pub relay: bool,
}

/// Network address
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NetworkAddress {
    pub services: u64,
    pub ip: [u8; 16],
    pub port: u16,
}

/// Ping message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingMessage {
    pub nonce: u64,
}

/// Pong message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PongMessage {
    pub nonce: u64,
}

/// Get headers message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetHeadersMessage {
    pub version: i32,
    pub block_locator_hashes: Vec<Hash>,
    pub hash_stop: Hash,
}

/// Headers message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeadersMessage {
    pub headers: Vec<BlockHeader>,
}

/// Get blocks message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetBlocksMessage {
    pub version: i32,
    pub block_locator_hashes: Vec<Hash>,
    pub hash_stop: Hash,
}

/// Block message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockMessage {
    pub block: Block,
    /// Witness data for each transaction in the block (one Witness per transaction)
    /// This is populated when parsing from Bitcoin wire format
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub witnesses: Vec<Vec<Vec<u8>>>,
}

/// Get data message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetDataMessage {
    pub inventory: Vec<InventoryItem>,
}

/// Inventory item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryItem {
    pub inv_type: u32,
    pub hash: Hash,
}

/// Inventory message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvMessage {
    pub inventory: Vec<InventoryItem>,
}

/// Transaction message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxMessage {
    pub transaction: Transaction,
}

// Compact Block Relay (BIP152) messages
use crate::network::compact_blocks::CompactBlock;

/// SendCmpct message - Negotiate compact block relay support
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendCmpctMessage {
    /// Compact block version (1 or 2)
    pub version: u64,
    /// Whether to prefer compact blocks (1) or regular blocks (0)
    pub prefer_cmpct: u8,
}

impl SendCmpctMessage {
    /// Create SendCmpct message with recommended version for transport
    pub fn for_transport(transport: TransportType, prefer_cmpct: bool) -> Self {
        use crate::network::compact_blocks::recommended_compact_block_version;

        Self {
            version: recommended_compact_block_version(transport),
            prefer_cmpct: if prefer_cmpct { 1 } else { 0 },
        }
    }

    /// Check if peer also supports BIP157 filters (based on version message services)
    pub fn supports_filters(&self, peer_services: u64) -> bool {
        use bllvm_protocol::bip157::NODE_COMPACT_FILTERS;
        (peer_services & NODE_COMPACT_FILTERS) != 0
    }
}

/// CompactBlock message - Compact block data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactBlockMessage {
    pub compact_block: CompactBlock,
}

/// GetBlockTxn message - Request missing transactions from compact block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetBlockTxnMessage {
    /// Block hash for the compact block
    pub block_hash: Hash,
    /// Indices of transactions to request (0-indexed)
    pub indices: Vec<u16>,
}

/// BlockTxn message - Response with requested transactions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockTxnMessage {
    /// Block hash for the compact block
    pub block_hash: Hash,
    /// Requested transactions in order
    pub transactions: Vec<Transaction>,
}

/// GetUTXOSet message - Request UTXO set at specific height
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetUTXOSetMessage {
    /// Block height for which to request UTXO set
    pub height: u64,
    /// Block hash at requested height (for verification)
    pub block_hash: Hash,
}

/// UTXOSet message - Response with UTXO set commitment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UTXOSetMessage {
    /// Request ID (echo from GetUTXOSet for matching)
    pub request_id: u64,
    /// UTXO commitment (Merkle root, supply, count, etc.)
    pub commitment: UTXOCommitment,
    /// UTXO set size hint (for chunking)
    pub utxo_count: u64,
    /// Indicates if this is a complete set or partial chunk
    pub is_complete: bool,
    /// Chunk identifier if partial
    pub chunk_id: Option<u32>,
}

/// UTXO commitment structure (matches consensus-proof definition)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UTXOCommitment {
    pub merkle_root: Hash,
    pub total_supply: u64,
    pub utxo_count: u64,
    pub block_height: u64,
    pub block_hash: Hash,
}

/// GetFilteredBlock message - Request filtered block (spam-filtered)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetFilteredBlockMessage {
    /// Request ID for async request-response matching
    pub request_id: u64,
    /// Block hash to request
    pub block_hash: Hash,
    /// Filter preferences (what spam types to filter)
    pub filter_preferences: FilterPreferences,
    /// Request BIP158 compact block filter in response (optional)
    ///
    /// When true, the response FilteredBlockMessage will include
    /// bip158_filter field with the compact block filter.
    /// This allows clients to get both spam filtering and light client
    /// discovery filters in a single request.
    pub include_bip158_filter: bool,
}

/// FilterPreferences - Configure spam filtering
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterPreferences {
    /// Filter Ordinals/Inscriptions
    pub filter_ordinals: bool,
    /// Filter dust outputs (default: < 546 satoshis)
    pub filter_dust: bool,
    /// Filter BRC-20 patterns
    pub filter_brc20: bool,
    /// Minimum output value to include (satoshis)
    pub min_output_value: u64,
}

/// FilteredBlock message - Response with filtered transactions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilteredBlockMessage {
    /// Request ID (echo from GetFilteredBlock for matching)
    pub request_id: u64,
    /// Block header
    pub header: BlockHeader,
    /// UTXO commitment for this block
    pub commitment: UTXOCommitment,
    /// Filtered transactions (only non-spam)
    pub transactions: Vec<Transaction>,
    /// Transaction indices in original block (for verification)
    pub transaction_indices: Vec<u32>,
    /// Summary of filtered spam
    pub spam_summary: SpamSummary,
    /// Optional BIP158 compact block filter (if requested and available)
    ///
    /// This allows clients to get both spam-filtered transactions (UTXO commitments)
    /// and BIP158 filters (light client discovery) in a single response.
    /// When present, clients can use the filter for efficient transaction matching
    /// while still receiving the commitment data for verification.
    pub bip158_filter: Option<Bip158FilterData>,
}

/// BIP158 filter data (embedded in FilteredBlock message)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bip158FilterData {
    /// Filter type (0 = Basic)
    pub filter_type: u8,
    /// Compact block filter data
    pub filter_data: Vec<u8>,
    /// Number of elements in filter
    pub num_elements: u32,
}

// Block Filtering (BIP157) messages

/// getcfilters message - Request filters for block range
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetCfiltersMessage {
    /// Filter type (0 = Basic)
    pub filter_type: u8,
    /// Start block height
    pub start_height: u32,
    /// Stop block hash
    pub stop_hash: Hash,
}

/// cfilter message - Compact block filter response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfilterMessage {
    /// Filter type (0 = Basic)
    pub filter_type: u8,
    /// Block hash
    pub block_hash: Hash,
    /// Compact block filter data
    pub filter_data: Vec<u8>,
    /// Number of elements in filter
    pub num_elements: u32,
}

/// getcfheaders message - Request filter headers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetCfheadersMessage {
    /// Filter type (0 = Basic)
    pub filter_type: u8,
    /// Start block height
    pub start_height: u32,
    /// Stop block hash
    pub stop_hash: Hash,
}

/// cfheaders message - Filter headers response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfheadersMessage {
    /// Filter type (0 = Basic)
    pub filter_type: u8,
    /// Stop block hash
    pub stop_hash: Hash,
    /// Previous filter header
    pub prev_header: FilterHeaderData,
    /// Filter headers (one per block in range)
    pub filter_headers: Vec<Hash>,
}

/// Filter header data (for serialization)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterHeaderData {
    /// Filter hash
    pub filter_hash: Hash,
    /// Previous filter header hash
    pub prev_header_hash: Hash,
}

/// getcfcheckpt message - Request filter checkpoints
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetCfcheckptMessage {
    /// Filter type (0 = Basic)
    pub filter_type: u8,
    /// Stop block hash
    pub stop_hash: Hash,
}

/// cfcheckpt message - Filter checkpoint response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfcheckptMessage {
    /// Filter type (0 = Basic)
    pub filter_type: u8,
    /// Stop block hash
    pub stop_hash: Hash,
    /// Filter header hashes at checkpoint intervals
    pub filter_header_hashes: Vec<Hash>,
}

// Payment Protocol (BIP70) - P2P variant messages

/// getpaymentrequest message - Request payment details from merchant
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPaymentRequestMessage {
    /// Merchant's Bitcoin public key (compressed, 33 bytes)
    #[serde(with = "serde_bytes")]
    pub merchant_pubkey: Vec<u8>,
    /// Unique payment identifier (32-byte hash)
    #[serde(with = "serde_bytes")]
    pub payment_id: Vec<u8>,
    /// Network identifier ("main", "test", "regtest")
    pub network: String,
}

/// paymentrequest message - Merchant payment request response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentRequestMessage {
    /// Payment request details (from bip70 module)
    pub payment_request: bllvm_protocol::payment::PaymentRequest,
    /// Signature over payment_request by merchant's Bitcoin key
    #[serde(with = "serde_bytes")]
    pub merchant_signature: Vec<u8>,
    /// Merchant's public key (compressed, 33 bytes)
    #[serde(with = "serde_bytes")]
    pub merchant_pubkey: Vec<u8>,
    /// Payment ID (echo from GetPaymentRequest)
    #[serde(with = "serde_bytes")]
    pub payment_id: Vec<u8>,
}

/// payment message - Customer payment transaction(s)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentMessage {
    /// Payment details (from payment protocol module)
    pub payment: bllvm_protocol::payment::Payment,
    /// Payment ID (echo from PaymentRequest)
    #[serde(with = "serde_bytes")]
    pub payment_id: Vec<u8>,
    /// Optional customer signature (for authenticated payments)
    #[serde(with = "serde_bytes")]
    pub customer_signature: Option<Vec<u8>>,
}

/// paymentack message - Merchant payment confirmation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentACKMessage {
    /// Payment acknowledgment (from payment protocol module)
    pub payment_ack: bllvm_protocol::payment::PaymentACK,
    /// Payment ID (echo from Payment)
    #[serde(with = "serde_bytes")]
    pub payment_id: Vec<u8>,
    /// Merchant signature confirming receipt
    #[serde(with = "serde_bytes")]
    pub merchant_signature: Vec<u8>,
}

// Package Relay (BIP 331) messages

/// sendpkgtxn message - Request to send package of transactions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendPkgTxnMessage {
    /// Package ID (combined hash of all transactions)
    #[serde(with = "serde_bytes")]
    pub package_id: Vec<u8>,
    /// Transaction hashes in package (ordered: parents first)
    pub tx_hashes: Vec<Hash>,
}

/// pkgtxn message - Package of transactions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgTxnMessage {
    /// Package ID (echo from SendPkgTxn)
    #[serde(with = "serde_bytes")]
    pub package_id: Vec<u8>,
    /// Transactions in package (ordered: parents first)
    /// Using Vec<u8> for serialized transactions (matches BIP 331 spec)
    pub transactions: Vec<Vec<u8>>,
}

/// pkgtxnreject message - Package rejection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgTxnRejectMessage {
    /// Package ID that was rejected
    #[serde(with = "serde_bytes")]
    pub package_id: Vec<u8>,
    /// Rejection reason code
    pub reason: u8,
    /// Optional rejection reason text
    pub reason_text: Option<String>,
}

/// SpamSummary - Summary of filtered spam transactions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpamSummary {
    /// Number of transactions filtered
    pub filtered_count: u32,
    /// Total size of filtered transactions (bytes)
    pub filtered_size: u64,
    /// Breakdown by spam type
    pub by_type: SpamBreakdown,
}

/// SpamBreakdown - Breakdown of spam by category
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpamBreakdown {
    pub ordinals: u32,
    pub inscriptions: u32,
    pub dust: u32,
    pub brc20: u32,
}

/// Bitcoin protocol message parser
pub struct ProtocolParser;

impl ProtocolParser {
    /// Parse a raw message into a protocol message
    pub fn parse_message(data: &[u8]) -> Result<ProtocolMessage> {
        // Validate message size
        if data.len() < 24 {
            return Err(anyhow::anyhow!("Message too short"));
        }

        if data.len() > MAX_PROTOCOL_MESSAGE_LENGTH {
            return Err(anyhow::anyhow!("Message too large"));
        }

        // Parse message header
        let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if magic != 0xd9b4bef9 {
            return Err(anyhow::anyhow!("Invalid magic number"));
        }

        let command = String::from_utf8_lossy(&data[4..12])
            .trim_end_matches('\0')
            .to_string();

        // Validate command string
        if !ALLOWED_COMMANDS.contains(&command.as_str()) {
            return Err(anyhow::anyhow!("Unknown command: {}", command));
        }

        let payload_length = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
        let checksum = &data[20..24];

        // Validate payload length
        if payload_length as usize > MAX_PROTOCOL_MESSAGE_LENGTH - 24 {
            return Err(anyhow::anyhow!("Payload too large"));
        }

        if data.len() < 24 + payload_length as usize {
            return Err(anyhow::anyhow!("Incomplete message"));
        }

        let payload = &data[24..24 + payload_length as usize];

        // Verify checksum using Bitcoin double SHA256
        let calculated_checksum = Self::calculate_checksum(payload);
        if calculated_checksum != checksum {
            return Err(anyhow::anyhow!("Invalid checksum"));
        }

        // Parse payload based on command
        match command.as_str() {
            "version" => Ok(ProtocolMessage::Version(bincode::deserialize(payload)?)),
            "verack" => Ok(ProtocolMessage::Verack),
            "ping" => Ok(ProtocolMessage::Ping(bincode::deserialize(payload)?)),
            "pong" => Ok(ProtocolMessage::Pong(bincode::deserialize(payload)?)),
            "getheaders" => Ok(ProtocolMessage::GetHeaders(bincode::deserialize(payload)?)),
            "headers" => Ok(ProtocolMessage::Headers(bincode::deserialize(payload)?)),
            "getblocks" => Ok(ProtocolMessage::GetBlocks(bincode::deserialize(payload)?)),
            "block" => Ok(ProtocolMessage::Block(bincode::deserialize(payload)?)),
            "getdata" => Ok(ProtocolMessage::GetData(bincode::deserialize(payload)?)),
            "inv" => Ok(ProtocolMessage::Inv(bincode::deserialize(payload)?)),
            "tx" => Ok(ProtocolMessage::Tx(bincode::deserialize(payload)?)),
            // Compact Block Relay (BIP152)
            "sendcmpct" => Ok(ProtocolMessage::SendCmpct(bincode::deserialize(payload)?)),
            "cmpctblock" => Ok(ProtocolMessage::CmpctBlock(bincode::deserialize(payload)?)),
            "getblocktxn" => Ok(ProtocolMessage::GetBlockTxn(bincode::deserialize(payload)?)),
            "blocktxn" => Ok(ProtocolMessage::BlockTxn(bincode::deserialize(payload)?)),
            // UTXO commitment protocol extensions
            "getutxoset" => Ok(ProtocolMessage::GetUTXOSet(bincode::deserialize(payload)?)),
            "utxoset" => Ok(ProtocolMessage::UTXOSet(bincode::deserialize(payload)?)),
            "getfilteredblock" => Ok(ProtocolMessage::GetFilteredBlock(bincode::deserialize(
                payload,
            )?)),
            "filteredblock" => Ok(ProtocolMessage::FilteredBlock(bincode::deserialize(
                payload,
            )?)),
            // Block Filtering (BIP157)
            "getcfilters" => Ok(ProtocolMessage::GetCfilters(bincode::deserialize(payload)?)),
            "cfilter" => Ok(ProtocolMessage::Cfilter(bincode::deserialize(payload)?)),
            "getcfheaders" => Ok(ProtocolMessage::GetCfheaders(bincode::deserialize(
                payload,
            )?)),
            "cfheaders" => Ok(ProtocolMessage::Cfheaders(bincode::deserialize(payload)?)),
            "getcfcheckpt" => Ok(ProtocolMessage::GetCfcheckpt(bincode::deserialize(
                payload,
            )?)),
            "cfcheckpt" => Ok(ProtocolMessage::Cfcheckpt(bincode::deserialize(payload)?)),
            // Payment Protocol (BIP70) - P2P variant
            "getpaymentrequest" => Ok(ProtocolMessage::GetPaymentRequest(bincode::deserialize(
                payload,
            )?)),
            "paymentrequest" => Ok(ProtocolMessage::PaymentRequest(bincode::deserialize(
                payload,
            )?)),
            "payment" => Ok(ProtocolMessage::Payment(bincode::deserialize(payload)?)),
            "paymentack" => Ok(ProtocolMessage::PaymentACK(bincode::deserialize(payload)?)),
            // Package Relay (BIP 331)
            "sendpkgtxn" => Ok(ProtocolMessage::SendPkgTxn(bincode::deserialize(payload)?)),
            "pkgtxn" => Ok(ProtocolMessage::PkgTxn(bincode::deserialize(payload)?)),
            "pkgtxnreject" => Ok(ProtocolMessage::PkgTxnReject(bincode::deserialize(
                payload,
            )?)),
            // Ban List Sharing
            "getbanlist" => Ok(ProtocolMessage::GetBanList(bincode::deserialize(payload)?)),
            "banlist" => Ok(ProtocolMessage::BanList(bincode::deserialize(payload)?)),
            _ => Err(anyhow::anyhow!("Unknown command: {}", command)),
        }
    }

    /// Serialize a protocol message to bytes
    pub fn serialize_message(message: &ProtocolMessage) -> Result<Vec<u8>> {
        let (command, payload) = match message {
            ProtocolMessage::Version(msg) => ("version", bincode::serialize(msg)?),
            ProtocolMessage::Verack => ("verack", vec![]),
            ProtocolMessage::Ping(msg) => ("ping", bincode::serialize(msg)?),
            ProtocolMessage::Pong(msg) => ("pong", bincode::serialize(msg)?),
            ProtocolMessage::GetHeaders(msg) => ("getheaders", bincode::serialize(msg)?),
            ProtocolMessage::Headers(msg) => ("headers", bincode::serialize(msg)?),
            ProtocolMessage::GetBlocks(msg) => ("getblocks", bincode::serialize(msg)?),
            ProtocolMessage::Block(msg) => ("block", bincode::serialize(msg)?),
            ProtocolMessage::GetData(msg) => ("getdata", bincode::serialize(msg)?),
            ProtocolMessage::Inv(msg) => ("inv", bincode::serialize(msg)?),
            ProtocolMessage::Tx(msg) => ("tx", bincode::serialize(msg)?),
            // Compact Block Relay (BIP152)
            ProtocolMessage::SendCmpct(msg) => ("sendcmpct", bincode::serialize(msg)?),
            ProtocolMessage::CmpctBlock(msg) => ("cmpctblock", bincode::serialize(msg)?),
            ProtocolMessage::GetBlockTxn(msg) => ("getblocktxn", bincode::serialize(msg)?),
            ProtocolMessage::BlockTxn(msg) => ("blocktxn", bincode::serialize(msg)?),
            // UTXO commitment protocol extensions
            ProtocolMessage::GetUTXOSet(msg) => ("getutxoset", bincode::serialize(msg)?),
            ProtocolMessage::UTXOSet(msg) => ("utxoset", bincode::serialize(msg)?),
            ProtocolMessage::GetFilteredBlock(msg) => {
                ("getfilteredblock", bincode::serialize(msg)?)
            }
            ProtocolMessage::FilteredBlock(msg) => ("filteredblock", bincode::serialize(msg)?),
            // Block Filtering (BIP157)
            ProtocolMessage::GetCfilters(msg) => ("getcfilters", bincode::serialize(msg)?),
            ProtocolMessage::Cfilter(msg) => ("cfilter", bincode::serialize(msg)?),
            ProtocolMessage::GetCfheaders(msg) => ("getcfheaders", bincode::serialize(msg)?),
            ProtocolMessage::Cfheaders(msg) => ("cfheaders", bincode::serialize(msg)?),
            ProtocolMessage::GetCfcheckpt(msg) => ("getcfcheckpt", bincode::serialize(msg)?),
            ProtocolMessage::Cfcheckpt(msg) => ("cfcheckpt", bincode::serialize(msg)?),
            // Payment Protocol (BIP70) - P2P variant
            ProtocolMessage::GetPaymentRequest(msg) => {
                ("getpaymentrequest", bincode::serialize(msg)?)
            }
            ProtocolMessage::PaymentRequest(msg) => ("paymentrequest", bincode::serialize(msg)?),
            ProtocolMessage::Payment(msg) => ("payment", bincode::serialize(msg)?),
            ProtocolMessage::PaymentACK(msg) => ("paymentack", bincode::serialize(msg)?),
            // Package Relay (BIP 331)
            ProtocolMessage::SendPkgTxn(msg) => ("sendpkgtxn", bincode::serialize(msg)?),
            ProtocolMessage::PkgTxn(msg) => ("pkgtxn", bincode::serialize(msg)?),
            ProtocolMessage::PkgTxnReject(msg) => ("pkgtxnreject", bincode::serialize(msg)?),
            // Ban List Sharing
            ProtocolMessage::GetBanList(msg) => ("getbanlist", bincode::serialize(msg)?),
            ProtocolMessage::BanList(msg) => ("banlist", bincode::serialize(msg)?),
        };

        let mut message = Vec::new();

        // Magic number
        message.extend_from_slice(&0xd9b4bef9u32.to_le_bytes());

        // Command (12 bytes, null-padded)
        let mut command_bytes = [0u8; 12];
        command_bytes[..command.len()].copy_from_slice(command.as_bytes());
        message.extend_from_slice(&command_bytes);

        // Payload length
        message.extend_from_slice(&(payload.len() as u32).to_le_bytes());

        // Checksum
        let checksum = Self::calculate_checksum(&payload);
        message.extend_from_slice(&checksum);

        // Payload
        message.extend_from_slice(&payload);

        Ok(message)
    }

    /// Calculate message checksum
    fn calculate_checksum(payload: &[u8]) -> [u8; 4] {
        use sha2::{Digest, Sha256};

        let hash1 = Sha256::digest(payload);
        let hash2 = Sha256::digest(hash1);

        let mut checksum = [0u8; 4];
        checksum.copy_from_slice(&hash2[..4]);
        checksum
    }
}

// Ban List Sharing messages

/// GetBanList message - Request peer's ban list (or hashed version)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetBanListMessage {
    /// Request full ban list (true) or just hash (false)
    pub request_full: bool,
    /// Minimum ban duration to include (seconds, 0 = all)
    pub min_ban_duration: u64,
}

/// BanList message - Response with ban list or hash
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BanListMessage {
    /// If false, only ban_list_hash is valid
    pub is_full: bool,
    /// Hash of full ban list (SHA256 of sorted entries)
    pub ban_list_hash: Hash,
    /// Full ban list entries (only if is_full = true)
    pub ban_entries: Vec<BanEntry>,
    /// Timestamp when ban list was generated
    pub timestamp: u64,
}

/// Single ban entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BanEntry {
    /// Banned peer address
    pub addr: NetworkAddress,
    /// Unix timestamp when ban expires (u64::MAX = permanent)
    pub unban_timestamp: u64,
    /// Reason for ban (optional)
    pub reason: Option<String>,
}
