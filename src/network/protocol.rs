//! Bitcoin protocol message handling
//!
//! Implements Bitcoin P2P protocol message serialization and deserialization.

#[cfg(feature = "protocol-verification")]
use blvm_spec_lock::spec_locked;

use crate::network::transport::TransportType;
use anyhow::Result;
use blvm_protocol::segwit::Witness;
use blvm_protocol::wire::{
    deserialize_getdata, deserialize_headers, deserialize_inv, deserialize_notfound,
    serialize_getdata, serialize_getheaders, serialize_inv, serialize_notfound,
};
use blvm_protocol::{Block, BlockHeader, Hash, Transaction};
use serde::{Deserialize, Serialize};

pub use blvm_protocol::network::{AddrV2Message, RejectMessage};

/// Bitcoin protocol constants
pub const BITCOIN_MAGIC_MAINNET: [u8; 4] = [0xf9, 0xbe, 0xb4, 0xd9];
pub const BITCOIN_MAGIC_TESTNET: [u8; 4] = [0x0b, 0x11, 0x09, 0x07];
pub const BITCOIN_MAGIC_REGTEST: [u8; 4] = [0xfa, 0xbf, 0xb5, 0xda];

/// Maximum protocol message size (32MB)
pub const MAX_PROTOCOL_MESSAGE_LENGTH: usize = 32 * 1024 * 1024;

/// Maximum addresses in an addr message (MAX_ADDR_TO_SEND)
pub const MAX_ADDR_TO_SEND: usize = 1000;

/// Maximum inventory items in inv/getdata messages (MAX_INV_SZ)
pub const MAX_INV_SZ: usize = 50000;

/// Maximum headers in a headers message (MAX_HEADERS_RESULTS)
pub const MAX_HEADERS_RESULTS: usize = 2000;

/// Service flags (bitfield in Version.services)
#[cfg(feature = "dandelion")]
pub const NODE_DANDELION: u64 = 1 << 24;
pub const NODE_PACKAGE_RELAY: u64 = 1 << 25;
pub const NODE_FIBRE: u64 = 1 << 26;
/// UTXO Commitments support (GetUTXOSet, UTXOSet, GetFilteredBlock, FilteredBlock)
#[cfg(feature = "utxo-commitments")]
pub const NODE_UTXO_COMMITMENTS: u64 = 1 << 27;
/// Ban List Sharing support (GetBanList, BanList)
pub const NODE_BAN_LIST_SHARING: u64 = 1 << 28;
/// Governance-related P2P capability (proposal/webhook integration; advertised via Version.services)
pub const NODE_GOVERNANCE: u64 = 1 << 29;
/// Erlay (BIP330) transaction relay support
#[cfg(feature = "erlay")]
pub const NODE_ERLAY: u64 = 1 << 30;

/// Wire command strings (single source for parse + serialize). Use these in both
/// [ProtocolParser::parse_message] and [ProtocolParser::serialize_message].
pub mod cmd {
    pub const ADDR: &str = "addr";
    pub const BANLIST: &str = "banlist";
    pub const BLOCK: &str = "block";
    pub const BLOCKTXN: &str = "blocktxn";
    pub const CFCHECKPT: &str = "cfcheckpt";
    pub const CFHEADERS: &str = "cfheaders";
    pub const CFILTER: &str = "cfilter";
    pub const CMPCTBLOCK: &str = "cmpctblock";
    pub const FEEFILTER: &str = "feefilter";
    pub const FILTEREDBLOCK: &str = "filteredblock";
    pub const GETADDR: &str = "getaddr";
    pub const GETBANLIST: &str = "getbanlist";
    pub const GETBLOCKTXN: &str = "getblocktxn";
    pub const GETCFCHECKPT: &str = "getcfcheckpt";
    pub const GETCFHEADERS: &str = "getcfheaders";
    pub const GETCFILTERS: &str = "getcfilters";
    pub const GETDATA: &str = "getdata";
    pub const GETFILTEREDBLOCK: &str = "getfilteredblock";
    pub const GETHEADERS: &str = "getheaders";
    pub const GETMODULE: &str = "getmodule";
    pub const GETMODULEBYHASH: &str = "getmodulebyhash";
    pub const GETMODULELIST: &str = "getmodulelist";
    pub const GETPAYMENTREQUEST: &str = "getpaymentrequest";
    pub const GETUTXOPROOF: &str = "getutxoproof";
    pub const GETUTXOSET: &str = "getutxoset";
    pub const GETBLOCKS: &str = "getblocks";
    pub const HEADERS: &str = "headers";
    pub const INV: &str = "inv";
    pub const MESH: &str = "mesh";
    pub const MODULE: &str = "module";
    pub const MODULEBYHASH: &str = "modulebyhash";
    pub const MODULEINV: &str = "moduleinv";
    pub const MODULELIST: &str = "modulelist";
    pub const NOTFOUND: &str = "notfound";
    pub const PAYMENT: &str = "payment";
    pub const PAYMENTACK: &str = "paymentack";
    pub const PAYMENTPROOF: &str = "paymentproof";
    pub const PAYMENTREQUEST: &str = "paymentrequest";
    pub const PING: &str = "ping";
    pub const PKGTXN: &str = "pkgtxn";
    pub const PKGTXNREJECT: &str = "pkgtxnreject";
    pub const PONG: &str = "pong";
    pub const REQRECON: &str = "reqrecon";
    pub const REQSKT: &str = "reqskt";
    pub const SENDCMPCT: &str = "sendcmpct";
    pub const SENDPKGTXN: &str = "sendpkgtxn";
    pub const SENDTXRCNCL: &str = "sendtxrcncl";
    pub const SETTLEMENTNOTIFICATION: &str = "settlementnotification";
    pub const SKETCH: &str = "sketch";
    pub const TX: &str = "tx";
    pub const UTXOPROOF: &str = "utxoproof";
    pub const UTXOSET: &str = "utxoset";
    pub const VERACK: &str = "verack";
    pub const VERSION: &str = "version";
    pub const SENDHEADERS: &str = "sendheaders";
    pub const REJECT: &str = "reject";
    pub const MEMPOOL: &str = "mempool";
    pub const ADDRV2: &str = "addrv2";
}

/// Allowed Bitcoin protocol commands
pub const ALLOWED_COMMANDS: &[&str] = &[
    cmd::VERSION,
    cmd::VERACK,
    cmd::PING,
    cmd::PONG,
    cmd::GETHEADERS,
    cmd::HEADERS,
    cmd::GETBLOCKS,
    cmd::BLOCK,
    cmd::GETDATA,
    cmd::INV,
    cmd::TX,
    cmd::NOTFOUND,
    cmd::GETADDR,
    cmd::ADDR,
    cmd::ADDRV2,
    cmd::SENDHEADERS,
    cmd::MEMPOOL,
    cmd::REJECT,
    cmd::FEEFILTER,
    cmd::SENDCMPCT,
    cmd::CMPCTBLOCK,
    cmd::GETBLOCKTXN,
    cmd::BLOCKTXN,
    // UTXO commitment protocol extensions
    cmd::GETUTXOSET,
    cmd::UTXOSET,
    cmd::GETFILTEREDBLOCK,
    cmd::FILTEREDBLOCK,
    // Block Filtering (BIP157)
    cmd::GETCFILTERS,
    cmd::CFILTER,
    cmd::GETCFHEADERS,
    cmd::CFHEADERS,
    cmd::GETCFCHECKPT,
    cmd::CFCHECKPT,
    // Payment Protocol (BIP70) - P2P variant
    cmd::GETPAYMENTREQUEST,
    cmd::PAYMENTREQUEST,
    cmd::PAYMENT,
    cmd::PAYMENTACK,
    // Package Relay (BIP 331)
    cmd::SENDPKGTXN,
    cmd::PKGTXN,
    cmd::PKGTXNREJECT,
    // Ban List Sharing
    cmd::GETBANLIST,
    cmd::BANLIST,
    // Module Registry
    cmd::GETMODULE,
    cmd::MODULE,
    cmd::GETMODULEBYHASH,
    cmd::MODULEBYHASH,
    cmd::MODULEINV,
    cmd::GETMODULELIST,
    cmd::MODULELIST,
    // Mesh networking
    cmd::MESH,
    // Erlay (BIP330) transaction relay
    #[cfg(feature = "erlay")]
    cmd::SENDTXRCNCL,
    #[cfg(feature = "erlay")]
    cmd::REQRECON,
    #[cfg(feature = "erlay")]
    cmd::REQSKT,
    #[cfg(feature = "erlay")]
    cmd::SKETCH,
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
    NotFound(NotFoundMessage),
    Tx(TxMessage),
    /// BIP133 FeeFilter - peer's minimum fee rate for tx relay (we accept, no response)
    FeeFilter(FeeFilterMessage),
    // Compact Block Relay (BIP152)
    SendCmpct(SendCmpctMessage),
    CmpctBlock(CompactBlockMessage),
    GetBlockTxn(GetBlockTxnMessage),
    BlockTxn(BlockTxnMessage),
    // UTXO commitment protocol extensions
    GetUTXOSet(GetUTXOSetMessage),
    UTXOSet(UTXOSetMessage),
    GetUTXOProof(GetUTXOProofMessage),
    UTXOProof(UTXOProofMessage),
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
    // CTV Payment Proof messages (for instant proof)
    #[cfg(feature = "ctv")]
    PaymentProof(PaymentProofMessage),
    SettlementNotification(SettlementNotificationMessage),
    // Package Relay (BIP 331)
    SendPkgTxn(SendPkgTxnMessage),
    PkgTxn(PkgTxnMessage),
    PkgTxnReject(PkgTxnRejectMessage),
    // Ban List Sharing
    GetBanList(GetBanListMessage),
    BanList(BanListMessage),
    // Mesh networking packets (payment-gated routing)
    MeshPacket(Vec<u8>), // Serialized mesh packet (handled by mesh module)
    // Address relay
    GetAddr,
    Addr(AddrMessage),
    AddrV2(AddrV2Message),
    SendHeaders,
    Reject(RejectMessage),
    MemPool,
    // Module Registry
    GetModule(GetModuleMessage),
    Module(ModuleMessage),
    GetModuleByHash(GetModuleByHashMessage),
    ModuleByHash(ModuleByHashMessage),
    ModuleInv(ModuleInvMessage),
    GetModuleList(GetModuleListMessage),
    ModuleList(ModuleListMessage),
}

/// Version message
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

impl VersionMessage {
    /// Check if peer supports UTXO commitments
    #[cfg(feature = "utxo-commitments")]
    pub fn supports_utxo_commitments(&self) -> bool {
        (self.services & NODE_UTXO_COMMITMENTS) != 0
    }

    /// Check if peer supports ban list sharing
    pub fn supports_ban_list_sharing(&self) -> bool {
        (self.services & NODE_BAN_LIST_SHARING) != 0
    }

    /// Check if peer supports BIP157 compact block filters
    pub fn supports_compact_filters(&self) -> bool {
        use blvm_protocol::bip157::NODE_COMPACT_FILTERS;
        (self.services & NODE_COMPACT_FILTERS) != 0
    }

    /// Check if peer supports package relay (BIP331)
    pub fn supports_package_relay(&self) -> bool {
        (self.services & NODE_PACKAGE_RELAY) != 0
    }

    /// Check if peer supports FIBRE
    pub fn supports_fibre(&self) -> bool {
        (self.services & NODE_FIBRE) != 0
    }

    #[cfg(feature = "dandelion")]
    /// Check if peer supports Dandelion
    pub fn supports_dandelion(&self) -> bool {
        (self.services & NODE_DANDELION) != 0
    }
}

/// Network address
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NetworkAddress {
    pub services: u64,
    pub ip: [u8; 16],
    pub port: u16,
}

/// Ping message
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingMessage {
    pub nonce: u64,
}

/// Pong message
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PongMessage {
    pub nonce: u64,
}

/// Get headers message
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetHeadersMessage {
    pub version: i32,
    pub block_locator_hashes: Vec<Hash>,
    pub hash_stop: Hash,
}

/// Headers message
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockMessage {
    pub block: Block,
    /// Witness data: Vec<Vec<Witness>> - one Vec<Witness> per transaction, one Witness per input
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub witnesses: Vec<Vec<Witness>>,
}

// Re-export inventory types from blvm-protocol (single source of truth)
pub use blvm_protocol::network::{GetDataMessage, InvMessage, InventoryVector, NotFoundMessage};

/// Transaction message
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxMessage {
    pub transaction: Transaction,
}

/// FeeFilter message (BIP133) - peer advertises minimum feerate for tx relay
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeFilterMessage {
    pub feerate: u64,
}

// Compact Block Relay (BIP152) messages
use crate::network::compact_blocks::CompactBlock;

/// SendCmpct message - Negotiate compact block relay support
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        use blvm_protocol::bip157::NODE_COMPACT_FILTERS;
        (peer_services & NODE_COMPACT_FILTERS) != 0
    }
}

/// CompactBlock message - Compact block data
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactBlockMessage {
    pub compact_block: CompactBlock,
}

/// GetBlockTxn message - Request missing transactions from compact block
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetBlockTxnMessage {
    /// Block hash for the compact block
    pub block_hash: Hash,
    /// Indices of transactions to request (0-indexed)
    pub indices: Vec<u16>,
}

/// BlockTxn message - Response with requested transactions
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// UTXO commitment structure (matches blvm-consensus definition)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UTXOCommitment {
    pub merkle_root: Hash,
    pub total_supply: u64,
    pub utxo_count: u64,
    pub block_height: u64,
    pub block_hash: Hash,
}

/// GetUTXOProof message - Request Merkle proof for a specific UTXO
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetUTXOProofMessage {
    /// Request ID for async request-response matching
    pub request_id: u64,
    /// OutPoint to request proof for (transaction hash + output index)
    pub tx_hash: Hash,
    pub output_index: u32,
    /// Block height/hash for which to request proof (must match commitment)
    pub block_height: u64,
    pub block_hash: Hash,
}

/// UTXOProof message - Response with Merkle proof for a UTXO
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UTXOProofMessage {
    /// Request ID (echo from GetUTXOProof for matching)
    pub request_id: u64,
    /// The transaction hash this proof is for
    pub tx_hash: Hash,
    /// The output index this proof is for
    pub output_index: u32,
    /// The UTXO data (for verification)
    pub value: i64,
    pub script_pubkey: Vec<u8>,
    pub height: u64,
    pub is_coinbase: bool,
    /// The Merkle proof (serialized as bytes)
    pub proof: Vec<u8>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetCfiltersMessage {
    /// Filter type (0 = Basic)
    pub filter_type: u8,
    /// Start block height
    pub start_height: u32,
    /// Stop block hash
    pub stop_hash: Hash,
}

/// cfilter message - Compact block filter response
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub payment_request: blvm_protocol::payment::PaymentRequest,
    /// Signature over payment_request by merchant's Bitcoin key
    #[serde(with = "serde_bytes")]
    pub merchant_signature: Vec<u8>,
    /// Merchant's public key (compressed, 33 bytes)
    #[serde(with = "serde_bytes")]
    pub merchant_pubkey: Vec<u8>,
    /// Payment ID (echo from GetPaymentRequest)
    #[serde(with = "serde_bytes")]
    pub payment_id: Vec<u8>,
    /// Optional CTV covenant proof (for instant proof)
    #[cfg(feature = "ctv")]
    #[serde(default)]
    pub covenant_proof: Option<crate::payment::covenant::CovenantProof>,
}

/// payment message - Customer payment transaction(s)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentMessage {
    /// Payment details (from payment protocol module)
    pub payment: blvm_protocol::payment::Payment,
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
    pub payment_ack: blvm_protocol::payment::PaymentACK,
    /// Payment ID (echo from Payment)
    #[serde(with = "serde_bytes")]
    pub payment_id: Vec<u8>,
    /// Merchant signature confirming receipt
    #[serde(with = "serde_bytes")]
    pub merchant_signature: Vec<u8>,
}

// CTV Payment Proof messages (for instant proof, not instant settlement)

/// paymentproof message - CTV covenant proof for payment commitment
#[cfg(feature = "ctv")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentProofMessage {
    /// Request ID for async request-response matching
    pub request_id: u64,
    /// Payment request ID this proof commits to
    pub payment_request_id: String,
    /// CTV covenant proof
    pub covenant_proof: crate::payment::covenant::CovenantProof,
    /// Optional full transaction template (for verification)
    pub transaction_template: Option<crate::payment::covenant::TransactionTemplate>,
}

/// settlementnotification message - Settlement status update
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementNotificationMessage {
    /// Payment request ID
    pub payment_request_id: String,
    /// Transaction hash (if in mempool or confirmed)
    pub transaction_hash: Option<Hash>,
    /// Confirmation count (0 = in mempool, >0 = confirmed)
    pub confirmation_count: u32,
    /// Block hash (if confirmed)
    pub block_hash: Option<Hash>,
    /// Settlement status
    pub status: String, // "mempool", "confirmed", "failed"
}

// Package Relay (BIP 331) messages

/// sendpkgtxn message - Request to send package of transactions
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendPkgTxnMessage {
    /// Package ID (combined hash of all transactions)
    #[serde(with = "serde_bytes")]
    pub package_id: Vec<u8>,
    /// Transaction hashes in package (ordered: parents first)
    pub tx_hashes: Vec<Hash>,
}

/// pkgtxn message - Package of transactions
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

// Module Registry messages

/// getmodule message - Request module by name
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetModuleMessage {
    /// Request ID for async request-response matching
    pub request_id: u64,
    /// Module name
    pub name: String,
    /// Optional version (if not specified, get latest)
    pub version: Option<String>,
    /// Optional payment ID (required if module requires payment)
    /// This is the payment_id from a completed PaymentACK
    pub payment_id: Option<String>,
}

/// module message - Module response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleMessage {
    /// Request ID (echo from GetModule for matching)
    pub request_id: u64,
    /// Module name
    pub name: String,
    /// Module version
    pub version: String,
    /// Module hash (content-addressable identifier)
    pub hash: Hash,
    /// Manifest hash
    pub manifest_hash: Hash,
    /// Binary hash
    pub binary_hash: Hash,
    /// Manifest content (TOML)
    pub manifest: Vec<u8>,
    /// Binary content (optional - may be fetched separately via getmodulebyhash)
    pub binary: Option<Vec<u8>>,
}

/// getmodulebyhash message - Request module by hash (content-addressable)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetModuleByHashMessage {
    /// Request ID for async request-response matching
    pub request_id: u64,
    /// Module hash
    pub hash: Hash,
    /// Request binary (if false, only manifest is returned)
    pub include_binary: bool,
}

/// modulebyhash message - Module response by hash
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleByHashMessage {
    /// Request ID (echo from GetModuleByHash for matching)
    pub request_id: u64,
    /// Module hash (echo from request)
    pub hash: Hash,
    /// Manifest content
    pub manifest: Vec<u8>,
    /// Binary content (if requested)
    pub binary: Option<Vec<u8>>,
}

/// moduleinv message - Module inventory (announce available modules)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleInvMessage {
    /// List of available modules
    pub modules: Vec<ModuleInventoryItem>,
}

/// Module inventory item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleInventoryItem {
    /// Module name
    pub name: String,
    /// Module version
    pub version: String,
    /// Module hash
    pub hash: Hash,
}

/// getmodulelist message - Request list of available modules
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetModuleListMessage {
    /// Optional filter by name prefix
    pub name_prefix: Option<String>,
    /// Maximum number of modules to return
    pub max_count: Option<u32>,
}

/// modulelist message - List of available modules
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleListMessage {
    /// List of available modules
    pub modules: Vec<ModuleInventoryItem>,
}

// Erlay (BIP330) transaction relay messages

/// sendtxrcncl message - Announce Erlay support and negotiate parameters
#[cfg(feature = "erlay")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendTxRcnclMessage {
    /// Erlay version (currently 1)
    pub version: u16,
    /// Initial reconciliation salt (for privacy)
    #[serde(with = "serde_bytes")]
    pub salt: [u8; 16],
    /// Minimum field size in bits (32 or 64)
    pub min_field_size: u8,
    /// Maximum field size in bits (32 or 64)
    pub max_field_size: u8,
}

/// reqrecon message - Request reconciliation
#[cfg(feature = "erlay")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReqReconMessage {
    /// Reconciliation salt (for privacy)
    #[serde(with = "serde_bytes")]
    pub salt: [u8; 16],
    /// Local transaction set size
    pub local_set_size: u32,
    /// Field size in bits (32 or 64)
    pub field_size: u8,
}

/// reqskt message - Request sketch
#[cfg(feature = "erlay")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReqSktMessage {
    /// Reconciliation salt (echo from ReqRecon)
    #[serde(with = "serde_bytes")]
    pub salt: [u8; 16],
    /// Remote transaction set size
    pub remote_set_size: u32,
    /// Field size in bits (32 or 64)
    pub field_size: u8,
}

/// sketch message - Send reconciliation sketch
#[cfg(feature = "erlay")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SketchMessage {
    /// Reconciliation salt (echo from ReqRecon)
    #[serde(with = "serde_bytes")]
    pub salt: [u8; 16],
    /// Reconciliation sketch (minisketch serialized data)
    #[serde(with = "serde_bytes")]
    pub sketch: Vec<u8>,
    /// Field size in bits (32 or 64)
    pub field_size: u8,
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
    /// Orange Paper 10.1.1: ParseMessage, size bounds, checksum rejection
    #[cfg_attr(feature = "protocol-verification", spec_locked("10.1.1"))]
    pub fn parse_message(data: &[u8]) -> Result<ProtocolMessage> {
        use tracing::{debug, warn};

        // Validate message size
        if data.len() < 24 {
            return Err(anyhow::anyhow!("Message too short: {} bytes", data.len()));
        }

        if data.len() > MAX_PROTOCOL_MESSAGE_LENGTH {
            return Err(anyhow::anyhow!("Message too large"));
        }

        // Parse message header
        let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        debug!(
            "Parsing message: magic=0x{:08x}, total_len={}",
            magic,
            data.len()
        );

        if magic != 0xd9b4bef9 {
            // Log first 24 bytes as hex for debugging
            let header_hex: String = data.iter().take(24).map(|b| format!("{b:02x}")).collect();
            warn!(
                "Invalid magic number 0x{:08x}, expected 0xd9b4bef9. Header hex: {}",
                magic, header_hex
            );
            return Err(anyhow::anyhow!("Invalid magic number 0x{:08x}", magic));
        }

        let command = String::from_utf8_lossy(&data[4..16])
            .trim_end_matches('\0')
            .to_string();

        debug!("Message command: '{}', data_len={}", command, data.len());

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
            cmd::VERSION => {
                // Use proper Bitcoin wire format deserialization for version messages
                use blvm_protocol::wire::deserialize_version;

                let version_msg = deserialize_version(payload)?;

                // Convert to node's ProtocolMessage format
                Ok(ProtocolMessage::Version(VersionMessage {
                    version: version_msg.version as i32, // blvm-node uses i32, blvm-protocol uses u32
                    services: version_msg.services,
                    timestamp: version_msg.timestamp,
                    addr_recv: NetworkAddress {
                        services: version_msg.addr_recv.services,
                        ip: version_msg.addr_recv.ip,
                        port: version_msg.addr_recv.port,
                    },
                    addr_from: NetworkAddress {
                        services: version_msg.addr_from.services,
                        ip: version_msg.addr_from.ip,
                        port: version_msg.addr_from.port,
                    },
                    nonce: version_msg.nonce,
                    user_agent: version_msg.user_agent,
                    start_height: version_msg.start_height,
                    relay: version_msg.relay,
                }))
            }
            cmd::VERACK => Ok(ProtocolMessage::Verack),
            cmd::PING => {
                // Use proper Bitcoin wire format (8-byte nonce)
                let wire_msg = blvm_protocol::wire::deserialize_ping(payload)
                    .map_err(|e| anyhow::anyhow!("Failed to deserialize ping: {}", e))?;
                Ok(ProtocolMessage::Ping(PingMessage {
                    nonce: wire_msg.nonce,
                }))
            }
            cmd::PONG => {
                // Use proper Bitcoin wire format (8-byte nonce)
                let wire_msg = blvm_protocol::wire::deserialize_pong(payload)
                    .map_err(|e| anyhow::anyhow!("Failed to deserialize pong: {}", e))?;
                Ok(ProtocolMessage::Pong(PongMessage {
                    nonce: wire_msg.nonce,
                }))
            }
            cmd::GETHEADERS => {
                // Use proper Bitcoin wire format deserialization
                let wire_msg = blvm_protocol::wire::deserialize_getheaders(payload)
                    .map_err(|e| anyhow::anyhow!("Failed to deserialize getheaders: {}", e))?;
                Ok(ProtocolMessage::GetHeaders(GetHeadersMessage {
                    version: wire_msg.version as i32,
                    block_locator_hashes: wire_msg.block_locator_hashes,
                    hash_stop: wire_msg.hash_stop,
                }))
            }
            cmd::HEADERS => {
                // Use proper Bitcoin wire format deserialization
                let wire_msg = deserialize_headers(payload)
                    .map_err(|e| anyhow::anyhow!("Failed to deserialize headers: {}", e))?;
                Ok(ProtocolMessage::Headers(HeadersMessage {
                    headers: wire_msg.headers,
                }))
            }
            cmd::GETBLOCKS => Ok(ProtocolMessage::GetBlocks(bincode::deserialize(payload)?)),
            cmd::BLOCK => {
                // Use consensus wire format (Bitcoin block + witness structure)
                let (block, witnesses) =
                    blvm_protocol::serialization::deserialize_block_with_witnesses(payload)
                        .map_err(|e| anyhow::anyhow!("Failed to deserialize block: {}", e))?;
                Ok(ProtocolMessage::Block(BlockMessage { block, witnesses }))
            }
            cmd::GETDATA => {
                let msg = deserialize_getdata(payload)
                    .map_err(|e| anyhow::anyhow!("Failed to deserialize getdata: {}", e))?;
                Ok(ProtocolMessage::GetData(msg))
            }
            cmd::INV => {
                let msg = deserialize_inv(payload)
                    .map_err(|e| anyhow::anyhow!("Failed to deserialize inv: {}", e))?;
                Ok(ProtocolMessage::Inv(msg))
            }
            cmd::NOTFOUND => {
                let msg = deserialize_notfound(payload)
                    .map_err(|e| anyhow::anyhow!("Failed to deserialize notfound: {}", e))?;
                Ok(ProtocolMessage::NotFound(msg))
            }
            cmd::TX => Ok(ProtocolMessage::Tx(bincode::deserialize(payload)?)),
            cmd::FEEFILTER => {
                // BIP133: 8-byte feerate (satoshis per KB) in little-endian
                if payload.len() < 8 {
                    return Err(anyhow::anyhow!("FeeFilter message too short"));
                }
                let feerate = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                Ok(ProtocolMessage::FeeFilter(FeeFilterMessage { feerate }))
            }
            // Compact Block Relay (BIP152)
            cmd::SENDCMPCT => Ok(ProtocolMessage::SendCmpct(bincode::deserialize(payload)?)),
            cmd::CMPCTBLOCK => Ok(ProtocolMessage::CmpctBlock(bincode::deserialize(payload)?)),
            cmd::GETBLOCKTXN => Ok(ProtocolMessage::GetBlockTxn(bincode::deserialize(payload)?)),
            cmd::BLOCKTXN => Ok(ProtocolMessage::BlockTxn(bincode::deserialize(payload)?)),
            // UTXO commitment protocol extensions
            cmd::GETUTXOSET => Ok(ProtocolMessage::GetUTXOSet(bincode::deserialize(payload)?)),
            cmd::UTXOSET => Ok(ProtocolMessage::UTXOSet(bincode::deserialize(payload)?)),
            cmd::GETUTXOPROOF => Ok(ProtocolMessage::GetUTXOProof(bincode::deserialize(
                payload,
            )?)),
            cmd::UTXOPROOF => Ok(ProtocolMessage::UTXOProof(bincode::deserialize(payload)?)),
            cmd::GETFILTEREDBLOCK => Ok(ProtocolMessage::GetFilteredBlock(bincode::deserialize(
                payload,
            )?)),
            cmd::FILTEREDBLOCK => Ok(ProtocolMessage::FilteredBlock(bincode::deserialize(
                payload,
            )?)),
            // Block Filtering (BIP157)
            cmd::GETCFILTERS => Ok(ProtocolMessage::GetCfilters(bincode::deserialize(payload)?)),
            cmd::CFILTER => Ok(ProtocolMessage::Cfilter(bincode::deserialize(payload)?)),
            cmd::GETCFHEADERS => Ok(ProtocolMessage::GetCfheaders(bincode::deserialize(
                payload,
            )?)),
            cmd::CFHEADERS => Ok(ProtocolMessage::Cfheaders(bincode::deserialize(payload)?)),
            cmd::GETCFCHECKPT => Ok(ProtocolMessage::GetCfcheckpt(bincode::deserialize(
                payload,
            )?)),
            cmd::CFCHECKPT => Ok(ProtocolMessage::Cfcheckpt(bincode::deserialize(payload)?)),
            // Payment Protocol (BIP70) - P2P variant
            cmd::GETPAYMENTREQUEST => Ok(ProtocolMessage::GetPaymentRequest(bincode::deserialize(
                payload,
            )?)),
            cmd::PAYMENTREQUEST => Ok(ProtocolMessage::PaymentRequest(bincode::deserialize(
                payload,
            )?)),
            cmd::PAYMENT => Ok(ProtocolMessage::Payment(bincode::deserialize(payload)?)),
            cmd::PAYMENTACK => Ok(ProtocolMessage::PaymentACK(bincode::deserialize(payload)?)),
            // Package Relay (BIP 331)
            cmd::SENDPKGTXN => Ok(ProtocolMessage::SendPkgTxn(bincode::deserialize(payload)?)),
            cmd::PKGTXN => Ok(ProtocolMessage::PkgTxn(bincode::deserialize(payload)?)),
            cmd::PKGTXNREJECT => Ok(ProtocolMessage::PkgTxnReject(bincode::deserialize(
                payload,
            )?)),
            // Ban List Sharing
            cmd::GETBANLIST => Ok(ProtocolMessage::GetBanList(bincode::deserialize(payload)?)),
            cmd::BANLIST => Ok(ProtocolMessage::BanList(bincode::deserialize(payload)?)),
            cmd::GETADDR => Ok(ProtocolMessage::GetAddr),
            cmd::ADDR => Ok(ProtocolMessage::Addr(bincode::deserialize(payload)?)),
            cmd::ADDRV2 => {
                let msg = blvm_protocol::wire::deserialize_addrv2(payload)
                    .map_err(|e| anyhow::anyhow!("Failed to deserialize addrv2: {e}"))?;
                Ok(ProtocolMessage::AddrV2(msg))
            },
            cmd::SENDHEADERS => Ok(ProtocolMessage::SendHeaders),
            cmd::MEMPOOL => Ok(ProtocolMessage::MemPool),
            cmd::REJECT => {
                let msg = blvm_protocol::wire::deserialize_reject(payload)
                    .map_err(|e| anyhow::anyhow!("Failed to deserialize reject: {e}"))?;
                Ok(ProtocolMessage::Reject(msg))
            },
            // Module Registry
            cmd::GETMODULE => Ok(ProtocolMessage::GetModule(bincode::deserialize(payload)?)),
            cmd::MODULE => Ok(ProtocolMessage::Module(bincode::deserialize(payload)?)),
            cmd::GETMODULEBYHASH => Ok(ProtocolMessage::GetModuleByHash(bincode::deserialize(
                payload,
            )?)),
            cmd::MODULEBYHASH => Ok(ProtocolMessage::ModuleByHash(bincode::deserialize(
                payload,
            )?)),
            cmd::MODULEINV => Ok(ProtocolMessage::ModuleInv(bincode::deserialize(payload)?)),
            cmd::GETMODULELIST => Ok(ProtocolMessage::GetModuleList(bincode::deserialize(
                payload,
            )?)),
            cmd::MODULELIST => Ok(ProtocolMessage::ModuleList(bincode::deserialize(payload)?)),
            // Mesh networking packets
            cmd::MESH => Ok(ProtocolMessage::MeshPacket(payload.to_vec())),
            #[cfg(feature = "erlay")]
            cmd::SENDTXRCNCL => Ok(ProtocolMessage::SendTxRcncl(bincode::deserialize(payload)?)),
            #[cfg(feature = "erlay")]
            cmd::REQRECON => Ok(ProtocolMessage::ReqRecon(bincode::deserialize(payload)?)),
            #[cfg(feature = "erlay")]
            cmd::REQSKT => Ok(ProtocolMessage::ReqSkt(bincode::deserialize(payload)?)),
            #[cfg(feature = "erlay")]
            cmd::SKETCH => Ok(ProtocolMessage::Sketch(bincode::deserialize(payload)?)),
            _ => Err(anyhow::anyhow!("Unknown command: {}", command)),
        }
    }

    /// Serialize a protocol message to bytes
    pub fn serialize_message(message: &ProtocolMessage) -> Result<Vec<u8>> {
        let (command, payload) = match message {
            ProtocolMessage::Version(msg) => {
                // Use proper Bitcoin wire format for version messages
                // Convert to blvm-protocol format and use its wire serialization
                use blvm_protocol::network::{NetworkAddress, VersionMessage};
                use blvm_protocol::wire::serialize_version;

                let version_msg = VersionMessage {
                    version: msg.version as u32,
                    services: msg.services,
                    timestamp: msg.timestamp,
                    addr_recv: NetworkAddress {
                        services: msg.addr_recv.services,
                        ip: msg.addr_recv.ip,
                        port: msg.addr_recv.port,
                    },
                    addr_from: NetworkAddress {
                        services: msg.addr_from.services,
                        ip: msg.addr_from.ip,
                        port: msg.addr_from.port,
                    },
                    nonce: msg.nonce,
                    user_agent: msg.user_agent.clone(),
                    start_height: msg.start_height,
                    relay: msg.relay,
                };

                // Serialize payload using proper Bitcoin wire format
                // This uses the serialize_version function from blvm-protocol wire.rs
                // which implements the exact Bitcoin protocol format
                let payload = serialize_version(&version_msg)?;
                (cmd::VERSION, payload)
            }
            ProtocolMessage::Verack => (cmd::VERACK, vec![]),
            ProtocolMessage::SendHeaders => (cmd::SENDHEADERS, vec![]),
            ProtocolMessage::MemPool => (cmd::MEMPOOL, vec![]),
            ProtocolMessage::Reject(msg) => (
                cmd::REJECT,
                blvm_protocol::wire::serialize_reject(msg).map_err(|e| anyhow::anyhow!("{e}"))?,
            ),
            ProtocolMessage::Ping(msg) => (cmd::PING, bincode::serialize(msg)?),
            ProtocolMessage::Pong(msg) => (cmd::PONG, bincode::serialize(msg)?),
            ProtocolMessage::GetHeaders(msg) => {
                // Use proper Bitcoin wire format for getheaders
                let wire_msg = blvm_protocol::network::GetHeadersMessage {
                    version: msg.version as u32,
                    block_locator_hashes: msg.block_locator_hashes.clone(),
                    hash_stop: msg.hash_stop,
                };
                (
                    cmd::GETHEADERS,
                    serialize_getheaders(&wire_msg).map_err(|e| anyhow::anyhow!("{}", e))?,
                )
            }
            ProtocolMessage::Headers(msg) => {
                // Use proper Bitcoin wire format for headers
                let wire_msg = blvm_protocol::network::HeadersMessage {
                    headers: msg.headers.clone(),
                };
                (
                    cmd::HEADERS,
                    blvm_protocol::wire::serialize_headers(&wire_msg)
                        .map_err(|e| anyhow::anyhow!("{}", e))?,
                )
            }
            ProtocolMessage::GetBlocks(msg) => (cmd::GETBLOCKS, bincode::serialize(msg)?),
            ProtocolMessage::Block(msg) => {
                // Must match `parse_message` cmd::BLOCK: consensus block+witness wire bytes, not bincode.
                let payload = blvm_protocol::serialization::serialize_block_with_witnesses(
                    &msg.block,
                    &msg.witnesses,
                    true,
                );
                (cmd::BLOCK, payload)
            }
            ProtocolMessage::GetData(msg) => (
                cmd::GETDATA,
                serialize_getdata(msg).map_err(|e| anyhow::anyhow!("{}", e))?,
            ),
            ProtocolMessage::Inv(msg) => (
                cmd::INV,
                serialize_inv(msg).map_err(|e| anyhow::anyhow!("{}", e))?,
            ),
            ProtocolMessage::NotFound(msg) => (
                cmd::NOTFOUND,
                serialize_notfound(msg).map_err(|e| anyhow::anyhow!("{}", e))?,
            ),
            ProtocolMessage::Tx(msg) => (cmd::TX, bincode::serialize(msg)?),
            ProtocolMessage::FeeFilter(msg) => (cmd::FEEFILTER, msg.feerate.to_le_bytes().to_vec()),
            // Compact Block Relay (BIP152)
            ProtocolMessage::SendCmpct(msg) => (cmd::SENDCMPCT, bincode::serialize(msg)?),
            ProtocolMessage::CmpctBlock(msg) => (cmd::CMPCTBLOCK, bincode::serialize(msg)?),
            ProtocolMessage::GetBlockTxn(msg) => (cmd::GETBLOCKTXN, bincode::serialize(msg)?),
            ProtocolMessage::BlockTxn(msg) => (cmd::BLOCKTXN, bincode::serialize(msg)?),
            // UTXO commitment protocol extensions
            ProtocolMessage::GetUTXOSet(msg) => (cmd::GETUTXOSET, bincode::serialize(msg)?),
            ProtocolMessage::UTXOSet(msg) => (cmd::UTXOSET, bincode::serialize(msg)?),
            ProtocolMessage::GetUTXOProof(msg) => (cmd::GETUTXOPROOF, bincode::serialize(msg)?),
            ProtocolMessage::UTXOProof(msg) => (cmd::UTXOPROOF, bincode::serialize(msg)?),
            ProtocolMessage::GetFilteredBlock(msg) => {
                (cmd::GETFILTEREDBLOCK, bincode::serialize(msg)?)
            }
            ProtocolMessage::FilteredBlock(msg) => (cmd::FILTEREDBLOCK, bincode::serialize(msg)?),
            // Block Filtering (BIP157)
            ProtocolMessage::GetCfilters(msg) => (cmd::GETCFILTERS, bincode::serialize(msg)?),
            ProtocolMessage::Cfilter(msg) => (cmd::CFILTER, bincode::serialize(msg)?),
            ProtocolMessage::GetCfheaders(msg) => (cmd::GETCFHEADERS, bincode::serialize(msg)?),
            ProtocolMessage::Cfheaders(msg) => (cmd::CFHEADERS, bincode::serialize(msg)?),
            ProtocolMessage::GetCfcheckpt(msg) => (cmd::GETCFCHECKPT, bincode::serialize(msg)?),
            ProtocolMessage::Cfcheckpt(msg) => (cmd::CFCHECKPT, bincode::serialize(msg)?),
            // Payment Protocol (BIP70) - P2P variant
            ProtocolMessage::GetPaymentRequest(msg) => {
                (cmd::GETPAYMENTREQUEST, bincode::serialize(msg)?)
            }
            ProtocolMessage::PaymentRequest(msg) => (cmd::PAYMENTREQUEST, bincode::serialize(msg)?),
            ProtocolMessage::Payment(msg) => (cmd::PAYMENT, bincode::serialize(msg)?),
            ProtocolMessage::PaymentACK(msg) => (cmd::PAYMENTACK, bincode::serialize(msg)?),
            // CTV Payment Proof messages
            #[cfg(feature = "ctv")]
            ProtocolMessage::PaymentProof(msg) => (cmd::PAYMENTPROOF, bincode::serialize(msg)?),
            ProtocolMessage::SettlementNotification(msg) => {
                (cmd::SETTLEMENTNOTIFICATION, bincode::serialize(msg)?)
            }
            // Package Relay (BIP 331)
            ProtocolMessage::SendPkgTxn(msg) => (cmd::SENDPKGTXN, bincode::serialize(msg)?),
            ProtocolMessage::PkgTxn(msg) => (cmd::PKGTXN, bincode::serialize(msg)?),
            ProtocolMessage::PkgTxnReject(msg) => (cmd::PKGTXNREJECT, bincode::serialize(msg)?),
            // Ban List Sharing
            ProtocolMessage::GetBanList(msg) => (cmd::GETBANLIST, bincode::serialize(msg)?),
            ProtocolMessage::BanList(msg) => (cmd::BANLIST, bincode::serialize(msg)?),
            // Address relay
            ProtocolMessage::GetAddr => (cmd::GETADDR, vec![]),
            ProtocolMessage::Addr(msg) => (cmd::ADDR, bincode::serialize(msg)?),
            ProtocolMessage::AddrV2(msg) => (
                cmd::ADDRV2,
                blvm_protocol::wire::serialize_addrv2(msg).map_err(|e| anyhow::anyhow!("{e}"))?,
            ),
            // Module Registry
            ProtocolMessage::GetModule(msg) => (cmd::GETMODULE, bincode::serialize(msg)?),
            ProtocolMessage::Module(msg) => (cmd::MODULE, bincode::serialize(msg)?),
            ProtocolMessage::GetModuleByHash(msg) => {
                (cmd::GETMODULEBYHASH, bincode::serialize(msg)?)
            }
            ProtocolMessage::ModuleByHash(msg) => (cmd::MODULEBYHASH, bincode::serialize(msg)?),
            ProtocolMessage::ModuleInv(msg) => (cmd::MODULEINV, bincode::serialize(msg)?),
            ProtocolMessage::GetModuleList(msg) => (cmd::GETMODULELIST, bincode::serialize(msg)?),
            ProtocolMessage::ModuleList(msg) => (cmd::MODULELIST, bincode::serialize(msg)?),
            ProtocolMessage::MeshPacket(_) => {
                return Err(anyhow::anyhow!("MeshPacket handled separately"))
            }
            #[cfg(feature = "erlay")]
            ProtocolMessage::SendTxRcncl(msg) => (cmd::SENDTXRCNCL, bincode::serialize(msg)?),
            #[cfg(feature = "erlay")]
            ProtocolMessage::ReqRecon(msg) => (cmd::REQRECON, bincode::serialize(msg)?),
            #[cfg(feature = "erlay")]
            ProtocolMessage::ReqSkt(msg) => (cmd::REQSKT, bincode::serialize(msg)?),
            #[cfg(feature = "erlay")]
            ProtocolMessage::Sketch(msg) => (cmd::SKETCH, bincode::serialize(msg)?),
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
    ///
    /// Computes double SHA256 of payload and returns first 4 bytes.
    /// Orange Paper 10.1.1: CalculateChecksum, |result| = 4
    #[cfg_attr(feature = "protocol-verification", spec_locked("10.1.1"))]
    pub fn calculate_checksum(payload: &[u8]) -> [u8; 4] {
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

// Address relay messages

/// Addr message - Contains peer addresses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddrMessage {
    /// List of network addresses
    pub addresses: Vec<NetworkAddress>,
}
