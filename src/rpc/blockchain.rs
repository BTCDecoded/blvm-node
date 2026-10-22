//! Blockchain RPC methods
//!
//! Implements blockchain-related JSON-RPC methods for querying blockchain state.

use crate::config::RequestTimeoutConfig;
use crate::node::block_processor::{prepare_block_validation_context, validate_block_with_context};
use crate::node::event_publisher::EventPublisher;
use crate::rpc::errors::{
    BLOCK_HASH_PARAM_REQUIRED_MSG, BLOCK_NOT_FOUND_MSG, BlockNotFoundError,
    HEIGHT_PARAM_REQUIRED_MSG, RpcError, STORAGE_NOT_AVAILABLE_MSG, TIP_BLOCK_NOT_FOUND_MSG,
};
use crate::rpc::params::{param_str_required, param_u64, param_u64_default};
use crate::storage::Storage;
use crate::storage::assumeutxo::AssumeUtxoManager;
use crate::utils::{
    CACHE_REFRESH_TIP, POLL_INTERVAL_WAIT_FOR_BLOCK, storage_timeout_from_config,
    with_custom_timeout,
};
use anyhow::Result;
use serde_json::{Number, Value, json};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, warn};

const ZERO_HASH_STR: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn non_coinbase_fee_sats(
    block: &blvm_protocol::Block,
    undo: &blvm_consensus::reorganization::BlockUndoLog,
) -> Vec<Option<u64>> {
    let mut spent: std::collections::HashMap<blvm_protocol::OutPoint, u64> =
        std::collections::HashMap::new();
    for entry in &undo.entries {
        if let Some(prev) = &entry.previous_utxo {
            if let Ok(value) = u64::try_from(prev.value) {
                spent.insert(entry.outpoint, value);
            }
        }
    }
    let mut fees = Vec::new();
    for tx in block.transactions.iter().skip(1) {
        let mut inputs = 0u64;
        let mut known = true;
        for input in &tx.inputs {
            match spent.get(&input.prevout) {
                Some(value) => inputs = inputs.saturating_add(*value),
                None => {
                    known = false;
                    break;
                }
            }
        }
        if !known {
            fees.push(None);
            continue;
        }
        let outputs: u64 = tx
            .outputs
            .iter()
            .filter_map(|output| u64::try_from(output.value).ok())
            .sum();
        fees.push(Some(inputs.saturating_sub(outputs)));
    }
    fees
}

thread_local! {
    static CACHED_TIP_HEIGHT: crate::rpc::cache::ThreadLocalTimedCache<u64> =
        crate::rpc::cache::ThreadLocalTimedCache::new();
    static CACHED_TIP_HASH_HEX: crate::rpc::cache::ThreadLocalKeyedCache<String, ([u8; 32], u64)> =
        crate::rpc::cache::ThreadLocalKeyedCache::new();
    static CACHED_DIFFICULTY: crate::rpc::cache::ThreadLocalKeyedCache<f64, (u64, u64)> =
        crate::rpc::cache::ThreadLocalKeyedCache::new();
}

/// Decode a Bitcoin RPC / explorer block hash hex string to internal digest order.
fn decode_hash32(hex: &str) -> Result<[u8; 32], RpcError> {
    crate::storage::hashing::hash_from_rpc_hex(hex)
        .map_err(|e| RpcError::invalid_hash_format(hex, Some(32), Some(&e)))
}

fn encode_hash32_rpc(hash: &[u8; 32]) -> String {
    crate::storage::hashing::hash_to_rpc_hex(hash)
}

fn spent_scripts_from_undo(storage: &Storage, hash: &blvm_protocol::Hash) -> Vec<Vec<u8>> {
    let mut scripts = Vec::new();
    #[cfg(feature = "production")]
    if let Ok(Some(undo)) = storage.blocks().get_undo_log(hash) {
        for entry in &undo.entries {
            if let Some(prev) = &entry.previous_utxo {
                scripts.push(prev.script_pubkey.as_ref().to_vec());
            }
        }
    }
    #[cfg(not(feature = "production"))]
    let _ = (storage, hash);
    scripts
}

fn previous_filter_header(
    storage: &Storage,
    header: &blvm_protocol::BlockHeader,
) -> Result<[u8; 32]> {
    if header.prev_block_hash == [0u8; 32] {
        return Ok([0u8; 32]);
    }
    match storage
        .blocks()
        .get_filter_header(&header.prev_block_hash)?
    {
        Some(prev) => Ok(prev),
        None => Err(anyhow::anyhow!("parent filter header is absent")),
    }
}

fn chain_filter_header(filter: &[u8], prev: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(filter.len() + 32);
    buf.extend_from_slice(filter);
    buf.extend_from_slice(prev);
    crate::storage::hashing::double_sha256(&buf)
}

fn require_address_index(storage: &Storage, method: &str) -> Result<()> {
    let index_on = storage
        .transactions()
        .get_index_stats()
        .map(|stats| stats.address_index_enabled)
        .unwrap_or(false);
    if index_on {
        Ok(())
    } else {
        Err(anyhow::anyhow!("{method}: address index is not enabled"))
    }
}

fn txindex_synced(storage: &Storage, tip_height: u64) -> bool {
    for height in 0..=tip_height {
        let Ok(Some(hash)) = storage.blocks().get_hash_by_height(height) else {
            return false;
        };
        let Ok(Some(block)) = storage.blocks().get_block(&hash) else {
            return false;
        };
        for tx in block.transactions.iter() {
            let txid = blvm_protocol::block::calculate_tx_id(tx);
            if storage
                .transactions()
                .get_transaction(&txid)
                .ok()
                .flatten()
                .is_none()
            {
                return false;
            }
        }
    }
    true
}

fn filter_index_synced(storage: &Storage, tip_height: u64) -> bool {
    let Ok(Some(tip_hash)) = storage.chain().get_tip_hash() else {
        return false;
    };
    if tip_hash == blvm_protocol::Hash::default() {
        return false;
    }
    // Missing headers are at the tip until the index catches up, so stop there.
    for height in (0..=tip_height).rev() {
        let Ok(Some(hash)) = storage.blocks().get_hash_by_height(height) else {
            return false;
        };
        if storage
            .blocks()
            .get_filter_header(&hash)
            .ok()
            .flatten()
            .is_none()
        {
            return false;
        }
    }
    true
}

fn in_money_range(value: i64) -> bool {
    const MAX_MONEY: i64 = 21_000_000 * 100_000_000;
    (0..=MAX_MONEY).contains(&value)
}

/// Blockchain RPC methods
#[derive(Clone)]
pub struct BlockchainRpc {
    storage: Option<Arc<Storage>>,
    /// Protocol engine for network/chain information
    protocol: Option<Arc<blvm_protocol::BitcoinProtocolEngine>>,
    /// Event publisher for BlockDisconnected/ChainReorg (reorg resolution)
    event_publisher: Option<Arc<EventPublisher>>,
    /// Request timeout config (storage/network/rpc timeouts)
    request_timeouts: Option<RequestTimeoutConfig>,
}

impl Default for BlockchainRpc {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockchainRpc {
    /// Create a new blockchain RPC handler
    pub fn new() -> Self {
        Self {
            storage: None,
            protocol: None,
            event_publisher: None,
            request_timeouts: None,
        }
    }

    /// Create with dependencies
    pub fn with_dependencies(storage: Arc<Storage>) -> Self {
        Self {
            storage: Some(storage),
            protocol: None,
            event_publisher: None,
            request_timeouts: None,
        }
    }

    /// Return a reference to storage or a consistent RPC error if not set.
    /// Use in RPC methods that require storage instead of repeating ok_or_else.
    pub fn require_storage(&self) -> Result<&Arc<Storage>, RpcError> {
        self.storage
            .as_ref()
            .ok_or_else(RpcError::storage_not_available)
    }

    /// Set event publisher for BlockDisconnected/ChainReorg notifications
    pub fn with_event_publisher(mut self, event_publisher: Option<Arc<EventPublisher>>) -> Self {
        self.event_publisher = event_publisher;
        self
    }

    /// Set request timeout config (storage/network/rpc timeouts from config)
    pub fn with_request_timeouts(mut self, config: Option<RequestTimeoutConfig>) -> Self {
        self.request_timeouts = config;
        self
    }

    /// Create with dependencies including protocol engine
    pub fn with_dependencies_and_protocol(
        storage: Arc<Storage>,
        protocol: Arc<blvm_protocol::BitcoinProtocolEngine>,
    ) -> Self {
        Self {
            storage: Some(storage),
            protocol: Some(protocol),
            event_publisher: None,
            request_timeouts: None,
        }
    }

    fn storage_timeout(&self) -> std::time::Duration {
        storage_timeout_from_config(self.request_timeouts.as_ref())
    }

    /// Chain name from the protocol, else the network stored with the chain.
    fn get_chain_name(&self) -> &str {
        self.known_chain_name().unwrap_or("regtest") // No protocol and no stored chain
    }

    /// The node's chain, if a protocol is configured or a network is stored with the chain.
    fn known_chain_name(&self) -> Option<&'static str> {
        if let Some(ref p) = self.protocol {
            return Some(match p.get_protocol_version() {
                blvm_protocol::ProtocolVersion::BitcoinV1 => "mainnet",
                blvm_protocol::ProtocolVersion::Testnet3 => "testnet",
                blvm_protocol::ProtocolVersion::Regtest => "regtest",
                blvm_protocol::ProtocolVersion::Signet => "signet",
                blvm_protocol::ProtocolVersion::Testnet4 => "testnet4",
            });
        }
        // Production RPC servers build BlockchainRpc without a protocol (rpc/mod.rs), so fall
        // back to the network stored with the chain, as core_chain_field does.
        if let Some(ref storage) = self.storage {
            if let Ok(Some(info)) = storage.chain().load_chain_info() {
                return Some(match info.chain_params.network.as_str() {
                    "testnet" => "testnet",
                    "testnet4" => "testnet4",
                    "regtest" => "regtest",
                    "signet" => "signet",
                    _ => "mainnet",
                });
            }
        }
        None
    }

    fn witness_address_on_this_chain(
        address_network: blvm_protocol::address::Network,
        chain: &str,
    ) -> bool {
        use blvm_protocol::address::Network;
        match address_network {
            Network::Mainnet => chain == "mainnet",
            Network::Testnet | Network::Testnet4 | Network::Signet => {
                matches!(chain, "testnet" | "testnet4" | "signet")
            }
            Network::Regtest => chain == "regtest",
        }
    }

    /// Base58Check version byte: 0x00/0x05 are mainnet; 0x6f/0xc4 are shared by
    /// testnet, testnet4, signet and regtest.
    fn base58_address_on_this_chain(version: u8, chain: &str) -> bool {
        match version {
            0x00 | 0x05 => chain == "mainnet",
            0x6f | 0xc4 => matches!(chain, "testnet" | "testnet4" | "signet" | "regtest"),
            _ => false,
        }
    }

    /// Core-compatible `chain` field for getblockchaininfo (`main`, `test`, `regtest`).
    fn core_chain_field(&self) -> &str {
        if let Some(ref storage) = self.storage {
            if let Ok(Some(info)) = storage.chain().load_chain_info() {
                return match info.chain_params.network.as_str() {
                    "mainnet" => "main",
                    "testnet" => "test",
                    "testnet4" => "testnet4",
                    "regtest" => "regtest",
                    "signet" => "signet",
                    _ => "main",
                };
            }
        }
        match self.get_chain_name() {
            "mainnet" => "main",
            "testnet" => "test",
            "testnet4" => "testnet4",
            "regtest" => "regtest",
            "signet" => "signet",
            _ => "main",
        }
    }

    /// Calculate difficulty from bits (compact target format).
    /// Uses blvm-consensus difficulty_from_bits (MAX_TARGET / target).
    fn calculate_difficulty(bits: u64) -> f64 {
        blvm_protocol::pow::difficulty_from_bits(bits).unwrap_or(1.0)
    }

    /// Median time of the header at `height` and up to 10 ancestors.
    ///
    /// This is the block's own median, not the tip window in `recent_headers`.
    fn median_time_through_height(&self, storage: &Storage, height: u64, fallback: u64) -> u64 {
        match storage.blocks().headers_back_from(height, 11) {
            Ok(headers) if !headers.is_empty() => {
                blvm_protocol::bip113::get_median_time_past(&headers)
            }
            _ => fallback,
        }
    }

    fn block_subsidy(&self, height: u64) -> u64 {
        let network = crate::storage::resolve_consensus_network(
            self.protocol.as_deref(),
            self.storage.as_deref(),
        );
        blvm_protocol::ConsensusProof::new().get_block_subsidy_for_network(height, network) as u64
    }

    /// Wire-format block sizes for RPC (stripped, total with witness when stored, BIP141 weight).
    fn compute_block_wire_sizes(
        block: &blvm_protocol::Block,
        witnesses: Option<&[Vec<blvm_protocol::segwit::Witness>]>,
    ) -> (usize, usize, u64) {
        use blvm_protocol::serialization::serialize_block_with_witnesses;
        use blvm_protocol::serialization::transaction::serialize_transaction;

        let stripped_size: usize = block
            .transactions
            .iter()
            .map(|tx| serialize_transaction(tx).len())
            .sum::<usize>()
            + 80;

        let block_size = if let Some(ws) = witnesses {
            let wire = serialize_block_with_witnesses(block, ws, true);
            wire.len().max(stripped_size)
        } else {
            stripped_size
        };

        let block_weight = (3 * stripped_size + block_size) as u64;
        (stripped_size, block_size, block_weight)
    }

    /// Resolve RPC address param: hex scriptPubKey or Bech32/Base58 address string.
    fn resolve_script_pubkey_param(address: &str) -> Result<Vec<u8>, RpcError> {
        if let Ok(script) = hex::decode(address) {
            if !script.is_empty() {
                return Ok(script);
            }
        }
        crate::rpc::rawtx::address_string_to_script_pubkey(address)
    }

    /// Calculate MuHash3072 for UTXO set (Core gettxoutsetinfo muhash).
    fn calculate_utxo_muhash(utxo_set: &blvm_protocol::UtxoSet) -> [u8; 32] {
        crate::storage::assumeutxo::AssumeUtxoManager::calculate_utxo_hash(utxo_set)
            .unwrap_or([0u8; 32])
    }

    /// Calculate confirmations for a block
    fn calculate_confirmations(block_height: u64, tip_height: u64) -> i64 {
        if block_height > tip_height {
            return 0;
        }
        (tip_height - block_height + 1) as i64
    }

    /// Format chainwork as hex string (32 bytes, Core `GetHex` / RPC order)
    fn format_chainwork(work: blvm_consensus::pow::U256) -> String {
        work.gbt_target_hex()
    }

    fn softforks_json(&self, height: u64) -> Value {
        use blvm_consensus::activation::ForkActivationTable;
        use blvm_consensus::types::Network;
        let network = match self.core_chain_field() {
            "test" => Network::Testnet,
            "testnet4" => Network::Testnet4,
            "regtest" => Network::Regtest,
            "signet" => Network::Signet,
            _ => Network::Mainnet,
        };
        let table = ForkActivationTable::from_network(network);
        json!({
            "segwit": {
                "type": "buried",
                "active": height >= table.segwit,
                "height": table.segwit
            },
            "taproot": {
                "type": "buried",
                "active": height >= table.taproot,
                "height": table.taproot
            }
        })
    }

    /// Get blockchain information
    ///
    /// Includes softfork information based on feature flags from blvm-protocol
    pub async fn get_blockchain_info(&self) -> Result<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getblockchaininfo");

        if self.protocol.is_none()
            && self
                .storage
                .as_ref()
                .and_then(|storage| storage.chain().load_chain_info().ok().flatten())
                .is_none()
        {
            return Err(anyhow::anyhow!("chain is not initialized"));
        }

        let softforks = self.softforks_json(0);

        if let Some(ref storage) = self.storage {
            let (best_hash, height) = storage.chain().get_tip_hash_and_height()?;
            let header_height = storage
                .blocks()
                .highest_stored_height()
                .ok()
                .flatten()
                .unwrap_or(height);
            let softforks = self.softforks_json(height);

            let best_hash_hex = CACHED_TIP_HASH_HEX.with(|c| {
                c.get_or_refresh(CACHE_REFRESH_TIP, &(best_hash, height), || {
                    encode_hash32_rpc(&best_hash)
                })
            });

            // Calculate difficulty from tip header (single lookup)
            let difficulty = if let Ok(Some(tip_header)) = storage.chain().get_tip_header() {
                Self::calculate_difficulty(tip_header.bits)
            } else {
                1.0
            };

            let mediantime = self.median_time_through_height(storage, height, 0);

            let chainwork = storage
                .chain()
                .get_chainwork(&best_hash)?
                .unwrap_or_else(blvm_consensus::pow::U256::zero);
            // CRITICAL FIX: Removed calculate_total_work() fallback - it iterates over ALL blocks
            // (357k+ iterations) causing 3+ minute RPC delays. If chainwork isn't cached,
            // return 0 instead of doing expensive calculation.
            let chainwork_hex = Self::format_chainwork(chainwork);

            Ok(json!({
                "chain": self.core_chain_field(),
                "blocks": height,
                "headers": header_height,
                "bestblockhash": best_hash_hex,
                "difficulty": difficulty,
                "mediantime": mediantime,
                "verificationprogress": 0.0,
                "initialblockdownload": true,
                "chainwork": chainwork_hex,
                "size_on_disk": if let Some(ref storage) = self.storage {
                    storage.disk_size().unwrap_or(0)
                } else {
                    0
                },
                "pruned": if let Some(ref storage) = self.storage {
                    storage.is_pruning_enabled()
                } else {
                    false
                },
                "pruneheight": if let Some(ref storage) = self.storage {
                    if let Some(pruning_manager) = storage.pruning() {
                        pruning_manager.get_stats().last_prune_height.unwrap_or(0)
                    } else {
                        0
                    }
                } else {
                    0
                },
                "automatic_pruning": if let Some(ref storage) = self.storage {
                    if let Some(pruning_manager) = storage.pruning() {
                        pruning_manager.config.auto_prune
                    } else {
                        false
                    }
                } else {
                    false
                },
                "softforks": softforks,
                "warnings": ""
            }))
        } else {
            // Graceful degradation: return default values when storage unavailable
            tracing::debug!(
                "getblockchaininfo called but storage not available, returning default values"
            );
            Ok(json!({
                "chain": self.core_chain_field(),
                "blocks": 0,
                "headers": 0,
                "bestblockhash": "0000000000000000000000000000000000000000000000000000000000000000",
                "difficulty": 1.0,
                "mediantime": 1231006505,
                "verificationprogress": 0.0,
                "initialblockdownload": true,
                "chainwork": "0000000000000000000000000000000000000000000000000000000000000000",
                "size_on_disk": 0,
                "pruned": false,
                "pruneheight": 0,
                "automatic_pruning": false,
                "softforks": softforks,
                "warnings": format!("{} - returning default values", STORAGE_NOT_AVAILABLE_MSG.split('.').next().unwrap_or(STORAGE_NOT_AVAILABLE_MSG))
            }))
        }
    }

    /// Get block by hash
    pub async fn get_block(&self, hash: &str) -> Result<Value> {
        self.get_block_with_verbosity(hash, 1).await
    }

    /// `verbosity` 0 is the block hex. Any other value is the JSON object.
    pub async fn get_block_with_verbosity(&self, hash: &str, verbosity: u64) -> Result<Value> {
        debug!("RPC: getblock {}", hash);

        let hash_array = decode_hash32(hash).map_err(|e| anyhow::anyhow!("{e}"))?;

        // Try to get block from storage with graceful degradation
        if let Some(ref storage) = self.storage {
            // Use timeout to prevent hanging on slow storage (wrap sync operation)
            let timeout_dur = self.storage_timeout();
            match with_custom_timeout(
                async {
                    tokio::task::spawn_blocking({
                        let storage = storage.clone();
                        move || storage.blocks().get_block(&hash_array)
                    })
                    .await
                },
                timeout_dur,
            )
            .await
            {
                Ok(Ok(Ok(Some(block)))) => {
                    if verbosity == 0 {
                        use blvm_protocol::serialization::serialize_block_with_witnesses;
                        let witnesses = storage
                            .blocks()
                            .get_witness(&hash_array)
                            .ok()
                            .flatten()
                            .unwrap_or_default();
                        let bytes = serialize_block_with_witnesses(&block, &witnesses, true);
                        return Ok(Value::String(hex::encode(bytes)));
                    }
                    // Block found - calculate all required fields (similar to get_block_header)
                    let block_height = storage.blocks().get_height_by_hash(&hash_array)?;

                    let tip_height = CACHED_TIP_HEIGHT.with(|c| {
                        c.get_or_refresh(CACHE_REFRESH_TIP, || {
                            storage
                                .chain()
                                .get_height()
                                .map(|h| h.unwrap_or(0))
                                .unwrap_or(0)
                        })
                    });

                    let confirmations = block_height
                        .map(|h| Self::calculate_confirmations(h, tip_height))
                        .unwrap_or(0);

                    let witnesses = storage.blocks().get_witness(&hash_array).ok().flatten();
                    let (stripped_size, block_size, block_weight) =
                        Self::compute_block_wire_sizes(&block, witnesses.as_deref());

                    let mediantime = match block_height {
                        Some(height) => {
                            self.median_time_through_height(storage, height, block.header.timestamp)
                        }
                        None => block.header.timestamp,
                    };

                    // Calculate difficulty
                    let difficulty = Self::calculate_difficulty(block.header.bits);

                    // Get transaction IDs
                    let tx_ids: Vec<String> = block
                        .transactions
                        .iter()
                        .map(|tx| {
                            let tx_hash = blvm_protocol::block::calculate_tx_id(tx);
                            encode_hash32_rpc(&tx_hash)
                        })
                        .collect();

                    // Get next block hash
                    let next_blockhash = block_height.and_then(|h| {
                        storage
                            .blocks()
                            .get_hash_by_height(h + 1)
                            .ok()
                            .flatten()
                            .map(|h| encode_hash32_rpc(&h))
                    });

                    // Get chainwork
                    let chainwork = if let Some(_height) = block_height {
                        storage
                            .chain()
                            .get_chainwork(&hash_array)?
                            .map(Self::format_chainwork)
                            .unwrap_or_else(|| ZERO_HASH_STR.to_string())
                    } else {
                        ZERO_HASH_STR.to_string()
                    };

                    return Ok(json!({
                        "hash": hash,
                        "confirmations": confirmations,
                        "size": block_size,
                        "strippedsize": stripped_size,
                        "weight": block_weight,
                        "height": block_height.unwrap_or(0),
                        "version": block.header.version,
                        "versionHex": format!("{:08x}", block.header.version),
                        "merkleroot": encode_hash32_rpc(&block.header.merkle_root),
                        "tx": tx_ids,
                        "time": block.header.timestamp,
                        "mediantime": mediantime,
                        "nonce": block.header.nonce as u32,
                        "bits": format!("{:08x}", block.header.bits),
                        "difficulty": difficulty,
                        "chainwork": chainwork,
                        "nTx": block.transactions.len(),
                        "previousblockhash": encode_hash32_rpc(&block.header.prev_block_hash),
                        "nextblockhash": next_blockhash,
                    }));
                }
                Ok(Ok(Ok(None))) => {
                    // Block not found - return error (server will map to RpcError::block_not_found)
                    return Err(BlockNotFoundError::new(hash).into());
                }
                Ok(Ok(Err(e))) => {
                    // Storage error - log and fall through to graceful degradation
                    warn!("Storage error getting block {}: {}", hash, e);
                }
                Ok(Err(_)) => {
                    // Task join error - log and fall through
                    warn!("Task error getting block {}", hash);
                }
                Err(_) => {
                    // Timeout - log and fall through to graceful degradation
                    warn!("Timeout getting block {} from storage", hash);
                }
            }
        }

        // Graceful degradation: return error if storage unavailable or block not found
        // (Don't return fake data - that's misleading)
        Err(anyhow::anyhow!(
            "{} or storage unavailable",
            BLOCK_NOT_FOUND_MSG
        ))
    }

    /// Get block hash by height
    pub async fn get_block_hash(&self, height: u64) -> Result<Value> {
        debug!("RPC: getblockhash {}", height);

        if let Some(ref storage) = self.storage {
            match storage.blocks().get_hash_by_height(height) {
                Ok(Some(hash)) => Ok(Value::String(encode_hash32_rpc(&hash))),
                Ok(None) => Err(BlockNotFoundError::new(format!("at height {height}")).into()),
                Err(err) => Err(anyhow::anyhow!("getblockhash: storage read failed: {err}")),
            }
        } else {
            Err(anyhow::anyhow!("Storage not available"))
        }
    }

    /// Get block header
    ///
    /// Params: ["blockhash", verbose (optional, default: true)]
    pub async fn get_block_header(&self, hash: &str, verbose: bool) -> Result<Value> {
        debug!("RPC: getblockheader {} verbose={}", hash, verbose);

        if let Some(ref storage) = self.storage {
            let hash_array = decode_hash32(hash)?;

            if let Ok(Some(header)) = storage.blocks().get_header(&hash_array) {
                if verbose {
                    let Some(block_height) = storage.blocks().get_height_by_hash(&hash_array)?
                    else {
                        return Err(anyhow::anyhow!(
                            "getblockheader: header has no height index"
                        ));
                    };

                    let tip_height = CACHED_TIP_HEIGHT.with(|c| {
                        c.get_or_refresh(CACHE_REFRESH_TIP, || {
                            storage
                                .chain()
                                .get_height()
                                .map(|h| h.unwrap_or(0))
                                .unwrap_or(0)
                        })
                    });

                    let confirmations = Self::calculate_confirmations(block_height, tip_height);
                    let mediantime =
                        self.median_time_through_height(storage, block_height, header.timestamp);

                    // Calculate difficulty
                    let difficulty = Self::calculate_difficulty(header.bits);

                    let n_tx = storage
                        .blocks()
                        .get_block_metadata(&hash_array)?
                        .map(|m| m.n_tx as usize)
                        .unwrap_or(0);

                    // Find next block hash
                    let next_blockhash = storage
                        .blocks()
                        .get_hash_by_height(block_height + 1)
                        .ok()
                        .flatten()
                        .map(|h| encode_hash32_rpc(&h));

                    let chainwork = storage
                        .chain()
                        .get_chainwork(&hash_array)?
                        .map(Self::format_chainwork)
                        .unwrap_or_else(|| ZERO_HASH_STR.to_string());

                    Ok(json!({
                        "hash": hash,
                        "confirmations": confirmations,
                        "height": block_height,
                        "version": header.version,
                        "versionHex": format!("{:08x}", header.version),
                        "merkleroot": encode_hash32_rpc(&header.merkle_root),
                        "time": header.timestamp,
                        "mediantime": mediantime,
                        "nonce": header.nonce as u32,
                        "bits": format!("{:08x}", header.bits),
                        "difficulty": difficulty,
                        "chainwork": chainwork,
                        "nTx": n_tx,
                        "previousblockhash": encode_hash32_rpc(&header.prev_block_hash),
                        "nextblockhash": next_blockhash
                    }))
                } else {
                    use blvm_protocol::serialization::serialize_block_header;
                    let header_bytes = serialize_block_header(&header);
                    Ok(Value::String(hex::encode(header_bytes)))
                }
            } else if let Err(err) = storage.blocks().get_header(&hash_array) {
                Err(anyhow::anyhow!(
                    "getblockheader: storage read failed: {err}"
                ))
            } else {
                Err(BlockNotFoundError::new("").into())
            }
        } else {
            Err(anyhow::anyhow!(STORAGE_NOT_AVAILABLE_MSG))
        }
    }

    /// Get best block hash
    ///
    /// Params: []
    pub async fn get_best_block_hash(&self) -> Result<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getbestblockhash");

        if let Some(ref storage) = self.storage {
            let current_height = storage.chain().get_height()?.unwrap_or(0);

            if let Ok(Some(hash)) = storage.chain().get_tip_hash() {
                let hex_str = CACHED_TIP_HASH_HEX.with(|c| {
                    c.get_or_refresh(CACHE_REFRESH_TIP, &(hash, current_height), || {
                        encode_hash32_rpc(&hash)
                    })
                });
                Ok(Value::String(hex_str))
            } else {
                Err(anyhow::anyhow!(TIP_BLOCK_NOT_FOUND_MSG))
            }
        } else {
            Err(anyhow::anyhow!(STORAGE_NOT_AVAILABLE_MSG))
        }
    }

    /// Get block count
    ///
    /// Params: []
    pub async fn get_block_count(&self) -> Result<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getblockcount");

        if let Some(ref storage) = self.storage {
            let height = storage.chain().get_height()?.unwrap_or(0);

            Ok(Value::Number(Number::from(height)))
        } else {
            Err(anyhow::anyhow!(STORAGE_NOT_AVAILABLE_MSG))
        }
    }

    /// Get current difficulty
    ///
    /// Params: []
    pub async fn get_difficulty(&self) -> Result<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getdifficulty");

        if let Some(ref storage) = self.storage {
            let current_height = storage.chain().get_height()?.unwrap_or(0);
            let bits = storage
                .chain()
                .get_tip_header()
                .ok()
                .flatten()
                .map(|h| h.bits);
            let difficulty = CACHED_DIFFICULTY.with(|c| {
                c.get_or_refresh(
                    CACHE_REFRESH_TIP,
                    &(current_height, bits.unwrap_or(0)),
                    || bits.map(Self::calculate_difficulty).unwrap_or(1.0),
                )
            });
            Ok(Value::Number(
                Number::from_f64(difficulty).unwrap_or_else(|| Number::from(1)),
            ))
        } else {
            Ok(Value::Number(Number::from_f64(1.0).unwrap()))
        }
    }

    /// Get UTXO set information
    ///
    /// Params: []
    pub async fn get_txoutset_info(&self) -> Result<Value> {
        debug!("RPC: gettxoutsetinfo");

        if let Some(ref storage) = self.storage {
            let (height, best_hash) = {
                let (hash, h) = storage.chain().get_tip_hash_and_height()?;
                (h, hash)
            };

            if let Ok(Some(stats)) = storage.chain().get_latest_utxo_stats() {
                Ok(json!({
                    "height": stats.height,
                    "bestblock": encode_hash32_rpc(&best_hash),
                    "transactions": stats.transactions,
                    "txouts": stats.txouts,
                    "bogosize": stats.txouts * 180,
                    "muhash": hex::encode(stats.muhash),
                    "disk_size": storage.disk_size().unwrap_or(0),
                    "total_amount": stats.total_amount as f64 / 100_000_000.0
                }))
            } else {
                let utxos = storage.utxos().get_all_utxos()?;
                let txouts = utxos.len();
                let total_amount = crate::storage::chainstate::utxo_money_total(
                    utxos.values().map(|utxo| utxo.value),
                );
                let muhash = Self::calculate_utxo_muhash(&utxos);

                Ok(json!({
                    "height": height,
                    "bestblock": encode_hash32_rpc(&best_hash),
                    "transactions": storage.transaction_count().unwrap_or(0),
                    "txouts": txouts,
                    "bogosize": txouts * 180,
                    "muhash": hex::encode(muhash),
                    "disk_size": storage.disk_size().unwrap_or(0),
                    "total_amount": total_amount as f64 / 100_000_000.0
                }))
            }
        } else {
            Ok(json!({
                "height": 0,
                "bestblock": ZERO_HASH_STR,
                "transactions": 0,
                "txouts": 0,
                "bogosize": 0,
                "muhash": ZERO_HASH_STR,
                "disk_size": 0,
                "total_amount": 0.0
            }))
        }
    }

    /// Load UTXO set from snapshot file (loadtxoutset)
    ///
    /// Params: ["path"] or ["path", "base_blockhash"]
    /// Writes the snapshot into chain storage and moves the tip to the snapshot block.
    pub async fn load_txout_set(&self, params: &Value) -> Result<Value> {
        debug!("RPC: loadtxoutset");

        let storage = self.require_storage()?;
        let path_str = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::invalid_params("loadtxoutset requires path (string)"))?;

        let path = Path::new(path_str);
        let manager = AssumeUtxoManager::new(".");
        let (utxo_set, metadata) = manager
            .load_snapshot_from_path(path)
            .map_err(|e| RpcError::internal_error(e.to_string()))?;

        if let Some(base) = params.get(1).filter(|value| !value.is_null()) {
            let base_hex = base
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("base block hash must be a string"))?;
            let base_hash = decode_hash32(base_hex)?;
            if base_hash != metadata.block_hash {
                return Err(RpcError::invalid_params(
                    "base block hash does not match the snapshot",
                )
                .into());
            }
        }

        storage.utxos().store_utxo_set(&utxo_set)?;
        storage
            .chain()
            .set_snapshot_tip(&metadata.block_hash, metadata.block_height)?;
        let total_amount =
            crate::storage::chainstate::utxo_money_total(utxo_set.values().map(|utxo| utxo.value));
        let stats = crate::storage::chainstate::UTXOStats {
            height: metadata.block_height,
            txouts: utxo_set.len() as u64,
            total_amount,
            muhash: metadata.utxo_hash,
            transactions: 0,
        };
        storage
            .chain()
            .store_utxo_stats(&metadata.block_hash, &stats)?;

        Ok(json!({
            "base_blockhash": encode_hash32_rpc(&metadata.block_hash),
            "height": metadata.block_height,
            "txout_count": metadata.utxo_count,
            "utxo_hash": hex::encode(metadata.utxo_hash),
        }))
    }

    /// Verify blockchain database
    ///
    /// Params: [checklevel (optional, default: 3), numblocks (optional, default: 288)]
    ///
    /// Bitcoin Core checklevel semantics:
    /// - Level 0: Check block exists
    /// - Level 1: Check block can be deserialized
    /// - Level 2: Check merkle root
    /// - Level 3: Check block header linkage (prev_block_hash)
    /// - Level 4: Full UTXO validation (requires rewinding UTXO set with undo logs)
    ///
    /// Note: Level 4 requires undo logs to be stored during initial sync. Without undo logs,
    /// level 4 validation will report an error and fall back to level 3 checks.
    pub async fn verify_chain(
        &self,
        checklevel: Option<u64>,
        numblocks: Option<u64>,
    ) -> Result<Value> {
        debug!(
            "RPC: verifychain checklevel={:?} numblocks={:?}",
            checklevel, numblocks
        );

        if let Some(ref storage) = self.storage {
            let check_level = checklevel.unwrap_or(3);
            let tip_height = storage.chain().get_height()?.unwrap_or(0);
            if tip_height == 0 {
                return Ok(json!(true)); // Empty chain is valid
            }
            let start_height = verify_chain_start_height(tip_height, numblocks);

            let mut errors = Vec::new();
            let blockstore = storage.blocks();

            // For level 4 (full UTXO validation), we need to rewind the UTXO set
            // to the state before start_height, then reconnect blocks.
            // This requires undo logs to be available.
            #[cfg(feature = "production")]
            let mut utxo_set_for_level4: Option<blvm_protocol::UtxoSet> = None;
            #[cfg(feature = "production")]
            let engine_arc_opt = self.protocol.as_ref();

            #[cfg(feature = "production")]
            if check_level >= 4 {
                if let Some(_engine_arc) = engine_arc_opt {
                    // Load the current tip UTXO set
                    let mut utxo_set = storage
                        .utxos()
                        .get_all_utxos()
                        .map_err(|e| anyhow::anyhow!("Failed to get UTXO set: {}", e))?;

                    // Rewind UTXO set from tip down to start_height using undo logs.
                    // Apply disconnect semantics matching blvm_consensus::reorganization::disconnect_block:
                    // One pass per entry: remove new_utxo then restore previous_utxo.
                    // This handles same-block create→spend correctly (restore wins over remove).
                    let mut rewind_failed = false;
                    for height in (start_height..=tip_height).rev() {
                        if let Ok(Some(block_hash)) = storage.blocks().get_hash_by_height(height) {
                            match blockstore.get_undo_log(&block_hash) {
                                Ok(Some(undo_log)) => {
                                    for entry in undo_log.entries.iter() {
                                        // Remove created output (if this entry created one)
                                        if entry.new_utxo.is_some() {
                                            utxo_set.remove(&entry.outpoint);
                                        }
                                        // Restore spent input (if this entry spent one)
                                        if let Some(ref prev_utxo) = entry.previous_utxo {
                                            utxo_set.insert(entry.outpoint, prev_utxo.clone());
                                        }
                                    }
                                }
                                Ok(None) => {
                                    // No undo log available - cannot do level 4 validation
                                    errors.push(format!(
                                        "Level 4 validation requires undo logs; missing for height {height}"
                                    ));
                                    rewind_failed = true;
                                    break;
                                }
                                Err(e) => {
                                    errors.push(format!(
                                        "Failed to load undo log for height {height}: {e}"
                                    ));
                                    rewind_failed = true;
                                    break;
                                }
                            }
                        }
                    }

                    if !rewind_failed {
                        utxo_set_for_level4 = Some(utxo_set);
                    }
                } else {
                    errors.push("Level 4 validation requires protocol engine".to_string());
                }
            }

            // Verify blocks from start_height to tip
            for height in start_height..=tip_height {
                if let Ok(Some(block_hash)) = storage.blocks().get_hash_by_height(height) {
                    if let Ok(Some(block)) = storage.blocks().get_block(&block_hash) {
                        // Check level 3+: Verify block header linkage
                        if check_level >= 3 && height > 0 {
                            match storage.blocks().get_hash_by_height(height - 1) {
                                Ok(Some(prev_hash))
                                    if block.header.prev_block_hash == prev_hash => {}
                                Ok(Some(prev_hash)) => {
                                    errors.push(format!(
                                        "Block at height {} has incorrect prev_block_hash: expected {}, got {}",
                                        height,
                                        hex::encode(prev_hash),
                                        hex::encode(block.header.prev_block_hash)
                                    ));
                                    if check_level >= 4 {
                                        break;
                                    }
                                }
                                _ => {
                                    errors.push(format!(
                                        "Block at height {height} has no stored parent"
                                    ));
                                }
                            }
                        }

                        // Check level 2+: Verify merkle root
                        if check_level >= 2 {
                            use blvm_protocol::mining::calculate_merkle_root;

                            if let Ok(calculated_root) = calculate_merkle_root(&block.transactions)
                            {
                                if calculated_root != block.header.merkle_root {
                                    errors.push(format!(
                                        "Block at height {height} has incorrect merkle root"
                                    ));
                                    if check_level >= 4 {
                                        break;
                                    }
                                }
                            }
                        }

                        // Check level 4: Full UTXO validation (reconnect block)
                        // Uses per-block height MTP (not tip-relative) for proper replay
                        #[cfg(feature = "production")]
                        if check_level >= 4 {
                            if let Some(ref mut utxo_set) = utxo_set_for_level4 {
                                if let Some(engine_arc) = engine_arc_opt {
                                    let protocol_version = engine_arc.get_protocol_version();

                                    // Load witnesses for this block
                                    let witnesses =
                                        match crate::node::block_processor::load_witnesses_for_block(
                                            blockstore.as_ref(),
                                            &block,
                                            height,
                                            protocol_version,
                                        ) {
                                            Ok(w) => w,
                                            Err(e) => {
                                                errors.push(format!(
                                                "Block at height {height} witness load error: {e}"
                                            ));
                                                break;
                                            }
                                        };

                                    // Get MTP headers at this block's height (not tip-relative)
                                    let mtp_headers = blockstore
                                        .headers_before_height_for_mtp(height)
                                        .ok()
                                        .filter(|h| !h.is_empty());

                                    let median_time_past = mtp_headers
                                        .as_ref()
                                        .map(|h| blvm_protocol::bip113::get_median_time_past(h))
                                        .unwrap_or(0);

                                    let network_time = crate::utils::current_timestamp();

                                    // Create validation context with correct per-height MTP
                                    let mut context = match blvm_protocol::validation::ProtocolValidationContext::new(
                                        protocol_version,
                                        height,
                                    ) {
                                        Ok(c) => c,
                                        Err(e) => {
                                            errors.push(format!(
                                                "Block at height {height} context error: {e}"
                                            ));
                                            break;
                                        }
                                    };
                                    context.context_data.insert(
                                        "median_time_past".to_string(),
                                        median_time_past.to_string(),
                                    );
                                    context.context_data.insert(
                                        "network_time".to_string(),
                                        network_time.to_string(),
                                    );

                                    // Validate and connect block
                                    let owned_utxo = std::mem::take(utxo_set);
                                    match engine_arc.validate_and_connect_block_with_difficulty(
                                        &block,
                                        &witnesses,
                                        &owned_utxo,
                                        height,
                                        mtp_headers.as_deref(),
                                        &context,
                                        Some(blockstore.difficulty_ancestor_lookup()),
                                    ) {
                                        Ok((blvm_protocol::ValidationResult::Valid, new_utxo)) => {
                                            *utxo_set = new_utxo;
                                        }
                                        Ok((
                                            blvm_protocol::ValidationResult::Invalid(reason),
                                            _,
                                        )) => {
                                            errors.push(format!(
                                                "Block at height {height} invalid: {reason}"
                                            ));
                                            break;
                                        }
                                        Err(e) => {
                                            errors.push(format!(
                                                "Block at height {height} validation error: {e}"
                                            ));
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        errors.push(format!("Block at height {height} not found in storage"));
                        if check_level >= 4 {
                            break;
                        }
                    }
                } else {
                    errors.push(format!("Block hash at height {height} not found"));
                    if check_level >= 4 {
                        break;
                    }
                }
            }

            if errors.is_empty() {
                Ok(Value::Bool(true))
            } else {
                Ok(json!({
                    "valid": false,
                    "errors": errors,
                    "checked_blocks": (tip_height - start_height + 1)
                }))
            }
        } else {
            Err(anyhow::anyhow!(STORAGE_NOT_AVAILABLE_MSG))
        }
    }

    /// Get chain tips
    ///
    /// Returns information about all known chain tips.
    /// Params: [] (no parameters)
    pub async fn get_chain_tips(&self) -> Result<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getchaintips");

        if let Some(ref storage) = self.storage {
            let (tip_hash, _tip_height) = storage.chain().get_tip_hash_and_height()?;
            let index = storage.chain().block_index();

            let indexed_tips = index.chain_tips()?;
            let mut tips = Vec::with_capacity(indexed_tips.len().max(1));

            if indexed_tips.is_empty() {
                if tip_hash != blvm_protocol::Hash::default() {
                    let height = storage.chain().get_height()?.unwrap_or(0);
                    tips.push(json!({
                        "height": height,
                        "hash": encode_hash32_rpc(&tip_hash),
                        "branchlen": 0,
                        "status": "active"
                    }));
                }
            } else {
                for (hash, entry) in indexed_tips {
                    let branchlen = index.branch_length(&hash, &tip_hash)?;
                    let status = if storage.chain().is_invalid(&hash).unwrap_or(false)
                        || entry.status == crate::storage::block_index::BlockIndexStatus::Invalid
                    {
                        "invalid"
                    } else if hash == tip_hash {
                        "active"
                    } else {
                        "valid-fork"
                    };

                    tips.push(json!({
                        "height": entry.height,
                        "hash": encode_hash32_rpc(&hash),
                        "branchlen": branchlen,
                        "status": status
                    }));
                }
                tips.sort_by(|a, b| {
                    let ha = a.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                    let hb = b.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                    hb.cmp(&ha)
                });
            }

            Ok(json!(tips))
        } else {
            Ok(json!([]))
        }
    }

    /// Get chain transaction statistics
    ///
    /// Params: ["nblocks"] (optional, default: 1 month of blocks)
    pub async fn get_chain_tx_stats(&self, params: &Value) -> Result<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getchaintxstats");

        let nblocks = if params.as_array().is_none_or(|items| items.is_empty()) {
            None
        } else {
            Some(param_u64_default(params, 0, 4320))
        };

        if let Some(ref storage) = self.storage {
            let tip_height = storage.chain().get_height()?.unwrap_or(0);

            let start_height = chain_tx_stats_start_height(tip_height, nblocks);

            let range_size = (tip_height - start_height + 1) as usize;

            // Use headers instead of full blocks (much faster - headers are ~80 bytes vs MB for blocks)
            let mut timestamps = Vec::with_capacity(range_size);
            let mut tx_counts = Vec::with_capacity(range_size);

            for height in start_height..=tip_height {
                if let Ok(Some(hash)) = storage.blocks().get_hash_by_height(height) {
                    if let Ok(Some(header)) = storage.blocks().get_header(&hash) {
                        timestamps.push(header.timestamp);

                        // Try to get TX count from metadata (if available), otherwise use header-only fallback
                        let n_tx = storage
                            .blocks()
                            .get_block_metadata(&hash)
                            .ok()
                            .flatten()
                            .map(|m| m.n_tx as u64)
                            .unwrap_or(0); // Fallback: 0 if metadata not available

                        tx_counts.push(n_tx);
                    }
                }
            }

            if timestamps.is_empty() {
                return Ok(json!({
                    "time": 0,
                    "txcount": 0,
                    "window_final_block_height": tip_height,
                    "window_block_count": 0,
                    "window_tx_count": 0,
                    "window_interval": 0,
                    "txrate": 0.0
                }));
            }

            let first_timestamp = timestamps[0];
            let last_timestamp = timestamps[timestamps.len() - 1];
            let window_interval = last_timestamp.saturating_sub(first_timestamp);
            let window_tx_count: u64 = tx_counts.iter().sum();
            let window_block_count = timestamps.len() as u64;

            let txrate = if window_interval > 0 {
                window_tx_count as f64 / window_interval as f64
            } else {
                0.0
            };

            // Cumulative tx count at chain tip (not limited to the stats window).
            let total_tx_count: u64 = (0..=tip_height)
                .filter_map(|height| {
                    storage
                        .blocks()
                        .get_hash_by_height(height)
                        .ok()
                        .flatten()
                        .and_then(|hash| {
                            storage
                                .blocks()
                                .get_block_metadata(&hash)
                                .ok()
                                .flatten()
                                .map(|m| m.n_tx as u64)
                        })
                })
                .sum();

            Ok(json!({
                "time": last_timestamp,
                "txcount": total_tx_count,
                "window_final_block_height": tip_height,
                "window_block_count": window_block_count,
                "window_tx_count": window_tx_count,
                "window_interval": window_interval,
                "txrate": txrate
            }))
        } else {
            Ok(json!({
                "time": 0,
                "txcount": 0,
                "window_final_block_height": 0,
                "window_block_count": 0,
                "window_tx_count": 0,
                "window_interval": 0,
                "txrate": 0.0
            }))
        }
    }

    /// Get block statistics
    ///
    /// Params: ["hash_or_height"] (block hash or height)
    pub async fn get_block_stats(&self, params: &Value) -> Result<Value> {
        debug!("RPC: getblockstats");

        let hash_or_height: Option<String> = params
            .get(0)
            .and_then(|p| p.as_u64().map(|h| h.to_string()))
            .or_else(|| {
                params
                    .get(0)
                    .and_then(|p| p.as_str())
                    .map(|s| s.to_string())
            });
        let hash_or_height = hash_or_height.as_deref();

        if let Some(ref storage) = self.storage {
            let block_hash = if let Some(hoh) = hash_or_height {
                // Try to parse as height first
                if let Ok(height) = hoh.parse::<u64>() {
                    storage
                        .blocks()
                        .get_hash_by_height(height)?
                        .ok_or_else(|| {
                            anyhow::Error::from(BlockNotFoundError::new(format!(
                                "at height {height}"
                            )))
                        })?
                } else {
                    decode_hash32(hoh).map_err(|e| anyhow::anyhow!("Invalid block hash: {e}"))?
                }
            } else {
                // Default to tip
                storage
                    .chain()
                    .get_tip_hash()?
                    .ok_or_else(|| anyhow::anyhow!("Chain not initialized"))?
            };

            if let Ok(Some(block)) = storage.blocks().get_block(&block_hash) {
                let tx_count = block.transactions.len();

                let witnesses = storage.blocks().get_witness(&block_hash).ok().flatten();
                let (stripped_size, block_size, block_weight) =
                    Self::compute_block_wire_sizes(&block, witnesses.as_deref());

                // Get block height
                let height = storage
                    .blocks()
                    .get_height_by_hash(&block_hash)?
                    .unwrap_or(0);

                // Count inputs and outputs
                let input_count: usize = block.transactions.iter().map(|tx| tx.inputs.len()).sum();
                let output_count: usize =
                    block.transactions.iter().map(|tx| tx.outputs.len()).sum();
                let spent_prevouts: usize = block
                    .transactions
                    .iter()
                    .skip(1)
                    .map(|tx| tx.inputs.len())
                    .sum();
                let utxo_increase = output_count as i64 - spent_prevouts as i64;

                // Sum output values inside the money range. A value outside that range is left out.
                let total_out = u64::try_from(crate::storage::chainstate::utxo_money_total(
                    block
                        .transactions
                        .iter()
                        .flat_map(|tx| tx.outputs.iter().map(|out| out.value)),
                ))
                .unwrap_or(u64::MAX);

                // Calculate block subsidy
                let subsidy = self.block_subsidy(height);

                // Total fees = coinbase outputs minus block subsidy (satoshis).
                let total_fees = if !block.transactions.is_empty() {
                    let coinbase_outputs =
                        u64::try_from(crate::storage::chainstate::utxo_money_total(
                            block.transactions[0].outputs.iter().map(|out| out.value),
                        ))
                        .unwrap_or(u64::MAX);
                    coinbase_outputs.saturating_sub(subsidy)
                } else {
                    0
                };

                let tx_vsize = |index: usize, tx: &blvm_protocol::Transaction| -> u64 {
                    let witness = witnesses
                        .as_ref()
                        .and_then(|all| all.get(index))
                        .map(|stack| stack.as_slice());
                    let (_, _, _, vsize) =
                        crate::rpc::rawtx::RawTxRpc::calculate_segwit_sizes(tx, witness);
                    (vsize as u64).max(1)
                };
                let non_coinbase_vsize: u64 = block
                    .transactions
                    .iter()
                    .enumerate()
                    .skip(1)
                    .map(|(index, tx)| tx_vsize(index, tx))
                    .sum();
                let avgfeerate = if non_coinbase_vsize > 0 {
                    total_fees as f64 / non_coinbase_vsize as f64 * 1000.0 / 100_000_000.0
                } else {
                    0.0
                };
                #[cfg(feature = "production")]
                let tx_fees = storage
                    .blocks()
                    .get_undo_log(&block_hash)
                    .ok()
                    .flatten()
                    .map(|undo| non_coinbase_fee_sats(&block, &undo))
                    .unwrap_or_default();
                #[cfg(not(feature = "production"))]
                let tx_fees: Vec<Option<u64>> = Vec::new();
                let mut sorted_fees: Vec<u64> = tx_fees.iter().copied().flatten().collect();
                sorted_fees.sort_unstable();
                let min_fee_sats = sorted_fees.first().copied().unwrap_or(0);
                let max_fee_sats = sorted_fees.last().copied().unwrap_or(0);
                let median_fee_sats = if sorted_fees.is_empty() {
                    0
                } else {
                    sorted_fees[sorted_fees.len() / 2]
                };
                let fee_btc = |sats: u64| sats as f64 / 100_000_000.0;
                let mut rates: Vec<f64> = block
                    .transactions
                    .iter()
                    .enumerate()
                    .skip(1)
                    .zip(tx_fees.iter())
                    .filter_map(|((index, tx), fee)| {
                        fee.map(|fee| {
                            fee as f64 / tx_vsize(index, tx) as f64 * 1000.0 / 100_000_000.0
                        })
                    })
                    .collect();
                rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let rate_at = |pct: usize| -> f64 {
                    if rates.is_empty() {
                        0.0
                    } else {
                        rates[pct * (rates.len() - 1) / 100]
                    }
                };
                let min_feerate = rates.first().copied().unwrap_or(0.0);
                let max_feerate = rates.last().copied().unwrap_or(0.0);
                let tx_sizes: Vec<usize> = block
                    .transactions
                    .iter()
                    .enumerate()
                    .map(|(index, tx)| {
                        let witness = witnesses.as_ref().and_then(|all| all.get(index));
                        if let Some(witness) = witness {
                            if witness.iter().any(|stack| !stack.is_empty()) {
                                if let Ok(bytes) = blvm_protocol::serialization::transaction::serialize_transaction_with_witness(tx, witness) {
                                    return bytes.len();
                                }
                            }
                        }
                        blvm_protocol::serialization::transaction::serialize_transaction(tx).len()
                    })
                    .collect();
                let mut ordered_sizes = tx_sizes.clone();
                ordered_sizes.sort_unstable();
                let mintxsize = ordered_sizes.first().copied().unwrap_or(0);
                let maxtxsize = ordered_sizes.last().copied().unwrap_or(0);
                let mediantxsize = ordered_sizes
                    .get(ordered_sizes.len() / 2)
                    .copied()
                    .unwrap_or(0);
                let swtxs = witnesses
                    .as_ref()
                    .map(|all| {
                        all.iter()
                            .filter(|stacks| stacks.iter().any(|stack| !stack.is_empty()))
                            .count()
                    })
                    .unwrap_or(0);
                let segwit_tx = |index: usize| {
                    witnesses
                        .as_ref()
                        .and_then(|all| all.get(index))
                        .is_some_and(|stacks| stacks.iter().any(|stack| !stack.is_empty()))
                };
                let swtotal_size: usize = tx_sizes
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| segwit_tx(*index))
                    .map(|(_, size)| *size)
                    .sum();
                let swtotal_weight: u64 = block
                    .transactions
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| segwit_tx(*index))
                    .map(|(index, tx)| {
                        let witness = witnesses
                            .as_ref()
                            .and_then(|all| all.get(index))
                            .map(|stacks| stacks.as_slice());
                        let (_, _, weight, _) =
                            crate::rpc::rawtx::RawTxRpc::calculate_segwit_sizes(tx, witness);
                        weight
                    })
                    .sum();

                Ok(json!({
                    "avgfee": if tx_count > 1 { total_fees as f64 / (tx_count - 1) as f64 / 100_000_000.0 } else { 0.0 },
                    "avgfeerate": avgfeerate,
                    "avgtxsize": if tx_count > 0 { block_size / tx_count } else { 0 },
                    "blockhash": encode_hash32_rpc(&block_hash),
                    "feerate_percentiles": [rate_at(10), rate_at(25), rate_at(50), rate_at(75), rate_at(90)],
                    "height": height,
                    "ins": input_count,
                    "maxfee": fee_btc(max_fee_sats),
                    "maxfeerate": max_feerate,
                    "maxtxsize": maxtxsize,
                    "medianfee": fee_btc(median_fee_sats),
                    "mediantime": self.median_time_through_height(storage, height, block.header.timestamp),
                    "mediantxsize": mediantxsize,
                    "minfee": fee_btc(min_fee_sats),
                    "minfeerate": min_feerate,
                    "mintxsize": mintxsize,
                    "outs": output_count,
                    "subsidy": subsidy,
                    "swtotal_size": swtotal_size,
                    "swtotal_weight": swtotal_weight,
                    "swtxs": swtxs,
                    "time": block.header.timestamp,
                    "total_out": total_out,
                    "total_size": block_size,
                    "total_weight": block_weight,
                    "totalfee": total_fees as f64 / 100_000_000.0,
                    "txs": tx_count,
                    "utxo_increase": utxo_increase,
                    "utxo_size_inc": 0
                }))
            } else {
                Err(BlockNotFoundError::new("").into())
            }
        } else {
            Err(anyhow::anyhow!(STORAGE_NOT_AVAILABLE_MSG))
        }
    }

    /// Prune blockchain
    ///
    /// Params: ["height"] (height to prune up to)
    pub async fn prune_blockchain(&self, params: &Value) -> Result<Value> {
        debug!("RPC: pruneblockchain");

        let height = params
            .get(0)
            .and_then(|p| p.as_u64())
            .ok_or_else(|| anyhow::anyhow!(HEIGHT_PARAM_REQUIRED_MSG))?;

        if let Some(ref storage) = self.storage {
            let tip_height = storage.chain().get_height()?.unwrap_or(0);

            // Check if IBD is in progress (height == 0 indicates no blocks synced)
            let is_ibd = tip_height == 0;

            if height >= tip_height {
                return Err(anyhow::anyhow!(
                    "Cannot prune to height >= tip height ({} >= {})",
                    height,
                    tip_height
                ));
            }

            // Get pruning manager
            if let Some(pruning_manager) = storage.pruning() {
                // Perform pruning
                let stats = pruning_manager.prune_to_height(height, tip_height, is_ibd)?;

                // Flush storage to persist changes
                storage.flush()?;

                Ok(json!({
                    "pruned_height": height,
                    "blocks_pruned": stats.blocks_pruned,
                    "blocks_kept": stats.blocks_kept,
                    "headers_kept": stats.headers_kept,
                    "storage_freed_bytes": stats.storage_freed,
                }))
            } else {
                Err(anyhow::anyhow!(
                    "Pruning is not enabled. Configure pruning in node configuration."
                ))
            }
        } else {
            Err(anyhow::anyhow!(STORAGE_NOT_AVAILABLE_MSG))
        }
    }

    /// Get pruning information
    ///
    /// Params: []
    pub async fn get_prune_info(&self, _params: &Value) -> Result<Value> {
        debug!("RPC: getpruneinfo");

        if let Some(ref storage) = self.storage {
            let tip_height = storage.chain().get_height()?.unwrap_or(0);
            let is_pruning_enabled = storage.is_pruning_enabled();

            if let Some(pruning_manager) = storage.pruning() {
                let stats = pruning_manager.get_stats();
                let config = &pruning_manager.config;

                // Determine pruning mode
                let mode_str = match &config.mode {
                    crate::config::PruningMode::Disabled => "disabled",
                    crate::config::PruningMode::Normal { .. } => "normal",
                    #[cfg(feature = "utxo-commitments")]
                    crate::config::PruningMode::Aggressive { .. } => "aggressive",
                    crate::config::PruningMode::Custom { .. } => "custom",
                    #[cfg(not(feature = "utxo-commitments"))]
                    _ => "unknown", // Fallback for Aggressive if feature not enabled
                };

                Ok(json!({
                    "pruning_enabled": is_pruning_enabled,
                    "mode": mode_str,
                    "auto_prune": config.auto_prune,
                    "auto_prune_interval": config.auto_prune_interval,
                    "min_blocks_to_keep": config.min_blocks_to_keep,
                    "current_height": tip_height,
                    "last_prune_height": stats.last_prune_height,
                    "total_blocks_pruned": stats.blocks_pruned,
                    "total_blocks_kept": stats.blocks_kept,
                    "total_headers_kept": stats.headers_kept,
                    "total_storage_freed_bytes": stats.storage_freed,
                }))
            } else {
                Ok(json!({
                    "pruning_enabled": false,
                    "mode": "disabled",
                    "auto_prune": false,
                    "current_height": tip_height,
                }))
            }
        } else {
            // Graceful degradation
            Ok(json!({
                "pruning_enabled": false,
                "mode": "disabled",
                "note": STORAGE_NOT_AVAILABLE_MSG
            }))
        }
    }

    /// Disconnect `target` and every descendant that is still the active chain.
    /// One walk from the tip collects that chain; each block is undone once.
    fn disconnect_invalid_block(
        &self,
        storage: &Storage,
        target: &blvm_protocol::Hash,
    ) -> Result<()> {
        let Some(tip) = storage.chain().get_tip_hash()? else {
            return Ok(());
        };
        let mut chain = Vec::new();
        let mut cursor = tip;
        let mut found = false;
        for _ in 0..100_000 {
            chain.push(cursor);
            if cursor == *target {
                found = true;
                break;
            }
            let Some(header) = storage.blocks().get_header(&cursor)? else {
                break;
            };
            if header.prev_block_hash == [0u8; 32] || header.prev_block_hash == cursor {
                break;
            }
            cursor = header.prev_block_hash;
        }
        if !found {
            return Err(anyhow::anyhow!(
                "invalidateblock target is not in the active chain"
            ));
        }
        for hash in chain {
            let Some(header) = storage.blocks().get_header(&hash)? else {
                break;
            };
            let undo = storage
                .blocks()
                .get_undo_log(&hash)?
                .ok_or_else(|| anyhow::anyhow!("missing undo log for invalidated block"))?;
            for entry in &undo.entries {
                if entry.new_utxo.is_some() {
                    storage.utxos().remove_utxo(&entry.outpoint)?;
                }
                if let Some(prev) = &entry.previous_utxo {
                    storage.utxos().add_utxo(&entry.outpoint, prev.as_ref())?;
                }
            }
            let height = storage.chain().get_height()?.unwrap_or(0);
            if height == 0 {
                break;
            }
            storage.blocks().clear_height_index(height)?;
            let parent = header.prev_block_hash;
            let Some(parent_header) = storage.blocks().get_header(&parent)? else {
                break;
            };
            storage
                .chain()
                .update_tip(&parent, &parent_header, height - 1)?;
            if hash != *target {
                storage.chain().mark_invalid(&hash)?;
            }
        }
        Ok(())
    }

    /// Invalidate block
    ///
    /// Params: ["blockhash"] (block hash to invalidate)
    ///
    /// Walks the active chain back to a stored header and disconnects it.
    /// A walk that misses the target returns an error. An undo error is returned.
    pub async fn invalidate_block(&self, params: &Value) -> Result<Value> {
        debug!("RPC: invalidateblock");

        let blockhash = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!(BLOCK_HASH_PARAM_REQUIRED_MSG))?;

        let hash =
            decode_hash32(blockhash).map_err(|e| anyhow::anyhow!("Invalid block hash: {}", e))?;

        if let Some(ref storage) = self.storage {
            self.disconnect_invalid_block(storage, &hash)?;
            storage.chain().mark_invalid(&hash)?;

            // Publish BlockDisconnected for payment reorg handling
            let height = storage
                .blocks()
                .get_height_by_hash(&hash)
                .ok()
                .flatten()
                .unwrap_or(0);
            if let Some(ref ep) = self.event_publisher {
                ep.publish_block_disconnected(&hash, height).await;

                if let Ok(Some(new_tip)) = storage.chain().get_tip_hash() {
                    if new_tip != hash {
                        ep.publish_chain_reorg(&hash, &new_tip).await;
                    }
                }
            }

            Ok(Value::Null)
        } else {
            // Graceful degradation: return informative error instead of failing silently
            Err(anyhow::anyhow!(STORAGE_NOT_AVAILABLE_MSG))
        }
    }

    /// Reconsider block
    ///
    /// Params: ["blockhash"] (block hash to reconsider)
    pub async fn reconsider_block(&self, params: &Value) -> Result<Value> {
        debug!("RPC: reconsiderblock");

        let blockhash = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!(BLOCK_HASH_PARAM_REQUIRED_MSG))?;

        let hash =
            decode_hash32(blockhash).map_err(|e| anyhow::anyhow!("Invalid block hash: {}", e))?;

        if let Some(ref storage) = self.storage {
            storage.chain().unmark_invalid(&hash)?;
            storage.chain().block_index().clear_invalid_status(&hash)?;
            Ok(Value::Null)
        } else {
            // Graceful degradation: return informative error instead of failing silently
            Err(anyhow::anyhow!(STORAGE_NOT_AVAILABLE_MSG))
        }
    }

    /// Wait for new block
    ///
    /// Params: ["timeout"] (optional, timeout in seconds, default: immediate check only)
    ///
    /// When timeout is set, polls until tip changes or timeout. Without timeout, returns current tip immediately.
    pub async fn wait_for_new_block(&self, params: &Value) -> Result<Value> {
        debug!("RPC: waitfornewblock");

        let timeout_secs = param_u64(params, 0);
        let storage = self
            .require_storage()
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let initial_tip = storage.chain().get_tip_hash()?;
        let initial_tip_clone = initial_tip;

        if timeout_secs.is_none() {
            if let Some(ref tip_hash) = initial_tip {
                let tip_height = storage.chain().get_height()?.unwrap_or(0);
                return Ok(json!({
                    "hash": encode_hash32_rpc(tip_hash),
                    "height": tip_height
                }));
            }
            return Err(anyhow::anyhow!("Chain not initialized"));
        }

        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(timeout_secs.unwrap_or(0));
        let poll_interval = POLL_INTERVAL_WAIT_FOR_BLOCK;

        while tokio::time::Instant::now() < deadline {
            let current = storage.chain().get_tip_hash()?;
            match (&initial_tip_clone, &current) {
                (Some(init), Some(cur)) if init != cur => {
                    let tip_height = storage.chain().get_height()?.unwrap_or(0);
                    return Ok(json!({
                        "hash": encode_hash32_rpc(cur),
                        "height": tip_height
                    }));
                }
                (None, Some(cur)) => {
                    let tip_height = storage.chain().get_height()?.unwrap_or(0);
                    return Ok(json!({
                        "hash": encode_hash32_rpc(cur),
                        "height": tip_height
                    }));
                }
                _ => {}
            }
            tokio::time::sleep(poll_interval).await;
        }

        if let Some(tip_hash) = storage.chain().get_tip_hash()? {
            let tip_height = storage.chain().get_height()?.unwrap_or(0);
            Ok(json!({
                "hash": encode_hash32_rpc(&tip_hash),
                "height": tip_height
            }))
        } else {
            Err(anyhow::anyhow!("Chain not initialized"))
        }
    }

    /// Wait for specific block
    ///
    /// Params: ["blockhash", "timeout"] (block hash, optional timeout in seconds)
    ///
    /// When timeout is set, polls until block appears or timeout. Without timeout, checks immediately.
    pub async fn wait_for_block(&self, params: &Value) -> Result<Value> {
        debug!("RPC: waitforblock");

        let blockhash =
            param_str_required(params, 0, "waitforblock").map_err(|e| anyhow::anyhow!("{}", e))?;

        let timeout_secs = param_u64(params, 1);
        let hash =
            decode_hash32(&blockhash).map_err(|e| anyhow::anyhow!("Invalid block hash: {}", e))?;

        let storage = self
            .require_storage()
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        if timeout_secs.is_none() {
            if let Ok(Some(_block)) = storage.blocks().get_block(&hash) {
                let height = storage.blocks().get_height_by_hash(&hash)?.unwrap_or(0);
                return Ok(json!({
                    "hash": blockhash,
                    "height": height
                }));
            }
            return Err(BlockNotFoundError::new("").into());
        }

        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(timeout_secs.unwrap_or(0));
        let poll_interval = POLL_INTERVAL_WAIT_FOR_BLOCK;

        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(_block)) = storage.blocks().get_block(&hash) {
                let height = storage.blocks().get_height_by_hash(&hash)?.unwrap_or(0);
                return Ok(json!({
                    "hash": blockhash,
                    "height": height
                }));
            }
            tokio::time::sleep(poll_interval).await;
        }

        Err(BlockNotFoundError::new("").into())
    }

    /// Wait for block height
    ///
    /// Params: ["height", "timeout"] (block height, optional timeout in seconds)
    ///
    /// When timeout is set, polls until block at height exists or timeout. Without timeout, checks immediately.
    pub async fn wait_for_block_height(&self, params: &Value) -> Result<Value> {
        debug!("RPC: waitforblockheight");

        let height = params
            .get(0)
            .and_then(|p| p.as_u64())
            .ok_or_else(|| anyhow::anyhow!(HEIGHT_PARAM_REQUIRED_MSG))?;

        let timeout_secs = param_u64(params, 1);

        let storage = self
            .require_storage()
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        if timeout_secs.is_none() {
            let tip_height = storage.chain().get_height()?.unwrap_or(0);
            if height <= tip_height {
                if let Ok(Some(hash)) = storage.blocks().get_hash_by_height(height) {
                    return Ok(json!({
                        "hash": encode_hash32_rpc(&hash),
                        "height": height
                    }));
                }
                return Err(BlockNotFoundError::new(format!("at height {height}")).into());
            }
            return Err(anyhow::anyhow!(
                "Block at height {} not yet available (tip: {})",
                height,
                tip_height
            ));
        }

        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(timeout_secs.unwrap_or(0));
        let poll_interval = POLL_INTERVAL_WAIT_FOR_BLOCK;

        while tokio::time::Instant::now() < deadline {
            let tip_height = storage.chain().get_height()?.unwrap_or(0);
            if height <= tip_height {
                if let Ok(Some(hash)) = storage.blocks().get_hash_by_height(height) {
                    return Ok(json!({
                        "hash": encode_hash32_rpc(&hash),
                        "height": height
                    }));
                }
            }
            tokio::time::sleep(poll_interval).await;
        }

        let tip_height = storage.chain().get_height()?.unwrap_or(0);
        Err(anyhow::anyhow!(
            "Block at height {} not yet available (tip: {})",
            height,
            tip_height
        ))
    }

    /// Get block filter (BIP158)
    ///
    /// Params: ["blockhash", "filtertype"] (block hash, filter type, default: 0 = Basic)
    pub async fn get_block_filter(&self, params: &Value) -> Result<Value> {
        debug!("RPC: getblockfilter");

        let blockhash = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!(BLOCK_HASH_PARAM_REQUIRED_MSG))?;

        let filtertype = param_u64_default(params, 1, 0); // Default: Basic filter

        if filtertype != 0 {
            return Err(anyhow::anyhow!("Only filter type 0 (Basic) is supported"));
        }

        let hash =
            decode_hash32(blockhash).map_err(|e| anyhow::anyhow!("Invalid block hash: {}", e))?;

        if let Some(ref storage) = self.storage {
            // Get block from storage
            if let Ok(Some(block)) = storage.blocks().get_block(&hash) {
                // Get filter service from network manager (if available)
                // For now, generate filter directly
                use blvm_protocol::bip158::build_block_filter;

                let previous_scripts = spent_scripts_from_undo(storage, &hash);
                match build_block_filter(&block.transactions, &previous_scripts) {
                    Ok(filter) => {
                        let prev_header = previous_filter_header(storage, &block.header)?;
                        let header = chain_filter_header(&filter.filter_data, &prev_header);
                        storage.blocks().store_filter_header(&hash, &header)?;
                        Ok(json!({
                            "filter": hex::encode(&filter.filter_data),
                            "header": hex::encode(header),
                        }))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to build filter: {}", e)),
                }
            } else {
                Err(BlockNotFoundError::new("").into())
            }
        } else {
            // Graceful degradation: return informative error instead of failing silently
            Err(anyhow::anyhow!(STORAGE_NOT_AVAILABLE_MSG))
        }
    }

    /// Get index information
    ///
    /// Params: [] (no parameters)
    pub async fn get_index_info(&self, _params: &Value) -> Result<Value> {
        debug!("RPC: getindexinfo");

        let storage = self.require_storage()?;

        // Get index statistics
        let index_stats = storage
            .transactions()
            .get_index_stats()
            .map_err(|e| RpcError::internal_error(format!("Failed to get index stats: {e}")))?;

        let best_block_height = storage.chain().get_height()?.unwrap_or(0);

        // Return available indexes with statistics
        Ok(json!({
            "txindex": {
                "synced": txindex_synced(storage, best_block_height),
                "best_block_height": best_block_height,
                "total_transactions": index_stats.total_transactions,
                "address_index_enabled": index_stats.address_index_enabled,
                "value_index_enabled": index_stats.value_index_enabled,
                "indexed_addresses": index_stats.indexed_addresses,
                "indexed_value_buckets": index_stats.indexed_value_buckets,
            },
            "basic block filter index": {
                "synced": filter_index_synced(storage, best_block_height),
                "best_block_height": best_block_height
            }
        }))
    }

    /// Get transaction IDs for an address
    ///
    /// Params: [address (string, hex-encoded script_pubkey)]
    /// Returns: Array of transaction IDs (hex strings)
    pub async fn getaddresstxids(&self, params: &Value) -> Result<Value> {
        debug!("RPC: getaddresstxids");

        let storage = self.require_storage()?;

        // Parse address parameter
        let address = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::missing_parameter("address", Some("string")))?;

        // Decode address to script_pubkey (hex script or Bech32/Base58 address).
        let script_pubkey = Self::resolve_script_pubkey_param(address)?;
        require_address_index(storage, "getaddresstxids")?;

        // Get transactions for this address
        let transactions = storage
            .transactions()
            .get_transactions_by_address(&script_pubkey)
            .map_err(|e| RpcError::internal_error(format!("Failed to query address: {e}")))?;

        // Extract transaction IDs
        let txids: Vec<String> = transactions
            .iter()
            .map(|tx| {
                let tx_hash = blvm_protocol::block::calculate_tx_id(tx);
                encode_hash32_rpc(&tx_hash)
            })
            .collect();

        Ok(json!(txids))
    }

    /// Get balance for an address
    ///
    /// Params: [address (string, hex-encoded script_pubkey)]
    /// Returns: Balance information
    pub async fn getaddressbalance(&self, params: &Value) -> Result<Value> {
        debug!("RPC: getaddressbalance");

        let storage = self.require_storage()?;

        // Parse address parameter
        let address = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::missing_parameter("address", Some("string")))?;

        // Decode address to script_pubkey (hex script or Bech32/Base58 address).
        let script_pubkey = Self::resolve_script_pubkey_param(address)?;
        require_address_index(storage, "getaddressbalance")?;

        // Get transactions for this address
        let transactions = storage
            .transactions()
            .get_transactions_by_address(&script_pubkey)
            .map_err(|e| RpcError::internal_error(format!("Failed to query address: {e}")))?;

        let mut balance: i64 = 0;
        let mut received: i64 = 0;
        let mut sent: i64 = 0;

        for tx in &transactions {
            use blvm_protocol::block::calculate_tx_id;
            let txid = calculate_tx_id(tx);

            for (idx, output) in tx.outputs.iter().enumerate() {
                if output.script_pubkey == script_pubkey {
                    received += output.value;
                    let outpoint = blvm_protocol::OutPoint {
                        hash: txid,
                        index: idx as u32,
                    };
                    if storage.utxos().get_utxo(&outpoint).ok().flatten().is_some() {
                        balance += output.value;
                    } else {
                        sent += output.value;
                    }
                }
            }
        }

        Ok(json!({
            "balance": balance,
            "received": received,
            "sent": sent,
        }))
    }

    /// Get comprehensive blockchain state in a single call
    ///
    /// Params: []
    /// Returns: Combined information from getblockchaininfo, getbestblockhash, getblockcount, getdifficulty
    pub async fn get_blockchain_state(&self) -> Result<Value> {
        debug!("RPC: getblockchainstate");

        if let Some(ref storage) = self.storage {
            let (tip_hash, height) = storage.chain().get_tip_hash_and_height()?;
            let tip_header = storage.chain().get_tip_header()?.unwrap_or({
                // Return genesis block header as fallback
                blvm_protocol::BlockHeader {
                    version: 1,
                    prev_block_hash: [0u8; 32],
                    merkle_root: [0u8; 32],
                    timestamp: 1231006505,
                    bits: 0x1d00ffff,
                    nonce: 0,
                }
            });

            let difficulty = Self::calculate_difficulty(tip_header.bits);
            let chainwork = storage
                .chain()
                .get_chainwork(&tip_hash)?
                .map(Self::format_chainwork)
                .unwrap_or_else(|| ZERO_HASH_STR.to_string());

            let headers = storage
                .blocks()
                .highest_stored_height()?
                .unwrap_or(height)
                .max(height);
            let mediantime = self.median_time_through_height(storage, height, tip_header.timestamp);
            Ok(json!({
                "chain": self.get_chain_name(),
                "blocks": height,
                "headers": headers,
                "bestblockhash": encode_hash32_rpc(&tip_hash),
                "difficulty": difficulty,
                "mediantime": mediantime,
                "verificationprogress": 0.0,
                "initialblockdownload": true,
                "chainwork": chainwork,
                "size_on_disk": storage.disk_size().unwrap_or(0),
                "pruned": storage.is_pruning_enabled(),
                "softforks": [],
                "warnings": ""
            }))
        } else {
            Ok(json!({
                "chain": "regtest",
                "blocks": 0,
                "headers": 0,
                "bestblockhash": ZERO_HASH_STR,
                "difficulty": 1.0,
                "mediantime": 0,
                "verificationprogress": 0.0,
                "initialblockdownload": true,
                "chainwork": ZERO_HASH_STR,
                "size_on_disk": 0,
                "pruned": false,
                "softforks": [],
                "warnings": ""
            }))
        }
    }

    /// Validate a Bitcoin address and return detailed information
    ///
    /// Params: ["address"]
    /// Returns: Address validation result with script type, network, etc.
    pub async fn validate_address(&self, params: &Value) -> Result<Value> {
        debug!("RPC: validateaddress");

        let address = crate::rpc::validation::validate_string_param(
            params,
            0,
            "address",
            Some(crate::rpc::validation::MAX_ADDRESS_STRING_LENGTH),
        )
        .map_err(anyhow::Error::new)?;

        use crate::rpc::rawtx::address_string_to_script_pubkey;
        use blvm_protocol::address::BitcoinAddress;

        if let Ok(decoded) = BitcoinAddress::decode(&address) {
            // Fail closed: with no protocol and no stored chain (e.g. a fresh datadir before the
            // genesis chain_info is written) the node's network is unknown, so no address is valid.
            if self
                .known_chain_name()
                .is_none_or(|chain| !Self::witness_address_on_this_chain(decoded.network, chain))
            {
                return Ok(json!({
                    "isvalid": false,
                    "address": address,
                }));
            }
            let Ok(script_pubkey) = address_string_to_script_pubkey(&address) else {
                return Ok(json!({
                    "isvalid": false,
                    "address": address,
                }));
            };
            let is_witness = true;
            let is_script = decoded.witness_version == 0 && decoded.witness_program.len() == 32;
            let script_type = match (decoded.witness_version, decoded.witness_program.len()) {
                (0, 20) => "witness_v0_keyhash",
                (0, 32) => "witness_v0_scripthash",
                (1, 32) => "witness_v1_taproot",
                _ => "witness_unknown",
            };
            return Ok(json!({
                "isvalid": true,
                "address": address,
                "scriptPubKey": hex::encode(&script_pubkey),
                "isscript": is_script,
                "iswitness": is_witness,
                "witness_version": decoded.witness_version,
                "witness_program": hex::encode(&decoded.witness_program),
                "script_type": script_type,
            }));
        }

        if let Ok(script_pubkey) = address_string_to_script_pubkey(&address) {
            let version =
                crate::rpc::rawtx::base58check_decode(&address).and_then(|d| d.first().copied());
            // Fail closed on an unknown chain, as for witness addresses above.
            if self.known_chain_name().is_none_or(|chain| {
                !version.is_some_and(|v| Self::base58_address_on_this_chain(v, chain))
            }) {
                return Ok(json!({
                    "isvalid": false,
                    "address": address,
                }));
            }
            let is_p2sh = address.starts_with('3') || address.starts_with('2');
            let script_type = if is_p2sh { "scripthash" } else { "pubkeyhash" };
            return Ok(json!({
                "isvalid": true,
                "address": address,
                "scriptPubKey": hex::encode(&script_pubkey),
                "isscript": is_p2sh,
                "iswitness": false,
                "witness_version": null,
                "witness_program": null,
                "script_type": script_type,
            }));
        }

        Ok(json!({
            "isvalid": false,
            "address": address,
        }))
    }

    /// Get comprehensive address information
    ///
    /// Params: ["address"]
    /// Returns: Address balance, transaction count, UTXO count, etc.
    pub async fn get_address_info(&self, params: &Value) -> Result<Value> {
        debug!("RPC: getaddressinfo");

        let address = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::missing_parameter("address", Some("string")))?;

        // Decode address to script_pubkey (hex script or Bech32/Base58 address).
        let script_pubkey = Self::resolve_script_pubkey_param(address)?;

        if let Some(ref storage) = self.storage {
            let index_on = storage
                .transactions()
                .get_index_stats()
                .map(|stats| stats.address_index_enabled)
                .unwrap_or(false);
            if !index_on {
                return Err(anyhow::anyhow!(
                    "getaddressinfo: address index is not enabled"
                ));
            }
            // Get transactions for this address
            let transactions = storage
                .transactions()
                .get_transactions_by_address(&script_pubkey)
                .map_err(|e| RpcError::internal_error(format!("Failed to query address: {e}")))?;

            let tx_count = transactions.len();

            // Calculate balance and UTXO count
            let mut balance: i64 = 0;
            let mut utxo_count = 0;
            let mut received: i64 = 0;
            let mut utxos = Vec::new();

            for tx in &transactions {
                use blvm_protocol::block::calculate_tx_id;
                let txid = calculate_tx_id(tx);

                for (idx, output) in tx.outputs.iter().enumerate() {
                    if output.script_pubkey.as_slice() == script_pubkey.as_slice()
                        && in_money_range(output.value)
                    {
                        received += output.value;
                        let outpoint = blvm_protocol::OutPoint {
                            hash: txid,
                            index: idx as u32,
                        };
                        match storage.utxos().get_utxo(&outpoint) {
                            Ok(Some(_)) => {
                                balance += output.value;
                                utxo_count += 1;
                                utxos.push(json!({
                                    "txid": encode_hash32_rpc(&txid),
                                    "vout": idx,
                                    "value": output.value,
                                }));
                            }
                            Ok(None) => {}
                            Err(err) => {
                                return Err(anyhow::anyhow!(
                                    "getaddressinfo: utxo read failed: {err}"
                                ));
                            }
                        }
                    }
                }
            }
            let sent = storage
                .transactions()
                .spent_value_for_script(&script_pubkey)
                .unwrap_or(0);
            let script_type = crate::rpc::rawtx::script_pubkey_type(&script_pubkey);
            let isscript = script_type == "scripthash" || script_type == "witness_v0_scripthash";
            let (witness_version, witness_program) = match script_type {
                "witness_v0_keyhash" | "witness_v0_scripthash" if script_pubkey.len() >= 2 => {
                    let n = script_pubkey[1] as usize;
                    if script_pubkey.len() >= 2 + n {
                        (
                            Value::from(0),
                            Value::String(hex::encode(&script_pubkey[2..2 + n])),
                        )
                    } else {
                        (Value::Null, Value::Null)
                    }
                }
                "witness_v1_taproot" if script_pubkey.len() >= 34 => (
                    Value::from(1),
                    Value::String(hex::encode(&script_pubkey[2..34])),
                ),
                _ => (Value::Null, Value::Null),
            };

            Ok(json!({
                "address": address,
                "scriptPubKey": hex::encode(&script_pubkey),
                "ismine": false,
                "iswatchonly": false,
                "isscript": isscript,
                "iswitness": script_type.starts_with("witness"),
                "witness_version": witness_version,
                "witness_program": witness_program,
                "script_type": script_type,
                "pubkey": Value::Null,
                "embedded": Value::Null,
                "iscompressed": Value::Null,
                "label": "",
                "timestamp": Value::Null,
                "hdkeypath": Value::Null,
                "hdseedid": Value::Null,
                "hdmasterfingerprint": Value::Null,
                "labels": [],
                "balance": balance as f64 / 100_000_000.0,
                "received": received as f64 / 100_000_000.0,
                "sent": sent as f64 / 100_000_000.0,
                "tx_count": tx_count,
                "utxo_count": utxo_count,
                "utxos": utxos
            }))
        } else {
            Err(anyhow::anyhow!(
                "getaddressinfo: address index is not enabled"
            ))
        }
    }
}

#[cfg(test)]
mod txoutset_tests {
    use super::*;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{OutPoint, UTXO};

    #[tokio::test]
    async fn txoutset_total_ignores_a_negative_value() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        storage
            .utxos()
            .add_utxo(
                &OutPoint {
                    hash: [1u8; 32],
                    index: 0,
                },
                &UTXO {
                    value: 50_000,
                    script_pubkey: vec![OP_1].into(),
                    height: 0,
                    is_coinbase: false,
                },
            )
            .unwrap();
        storage
            .utxos()
            .add_utxo(
                &OutPoint {
                    hash: [9u8; 32],
                    index: 0,
                },
                &UTXO {
                    value: -1,
                    script_pubkey: vec![OP_1].into(),
                    height: 0,
                    is_coinbase: false,
                },
            )
            .unwrap();

        let info = BlockchainRpc::with_dependencies(storage)
            .get_txoutset_info()
            .await
            .unwrap();
        let sats = (info["total_amount"].as_f64().unwrap() * 100_000_000.0).round() as u128;
        assert_eq!(info["txouts"], 2);
        assert_eq!(sats, 50_000);
    }
}

#[cfg(test)]
mod block_stats_tests {
    use super::*;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{
        Block, BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput,
    };

    fn tx_with_output(value: i64) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [1u8; 32],
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        }
    }

    #[tokio::test]
    async fn block_stats_ignore_a_negative_output() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 0,
                bits: 0,
                nonce: 0,
            },
            transactions: vec![tx_with_output(-1), tx_with_output(50_000)].into_boxed_slice(),
        };
        let hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_block(&block).unwrap();

        let stats = BlockchainRpc::with_dependencies(storage)
            .get_block_stats(&json!([crate::storage::hashing::hash_to_rpc_hex(&hash)]))
            .await
            .unwrap();
        assert_eq!(stats["outs"], 2);
        assert_eq!(stats["total_out"].as_u64(), Some(50_000));
        assert_eq!(stats["totalfee"].as_f64(), Some(0.0));
    }

    #[tokio::test]
    async fn utxo_increase_is_outputs_minus_spent_prevouts() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let coinbase = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [0u8; 32],
                    index: 0xffffffff,
                },
                script_sig: Vec::new(),
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 50_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 2,
                bits: 0,
                nonce: 1,
            },
            transactions: vec![coinbase, tx_with_output(40_000)].into_boxed_slice(),
        };
        let hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_block(&block).unwrap();
        let stats = BlockchainRpc::with_dependencies(storage)
            .get_block_stats(&json!([crate::storage::hashing::hash_to_rpc_hex(&hash)]))
            .await
            .unwrap();
        assert_eq!(stats["outs"], 2);
        assert_eq!(stats["ins"], 2);
        assert_eq!(stats["utxo_increase"].as_i64(), Some(1));
    }

    #[tokio::test]
    async fn header_without_a_height_is_not_height_zero() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let header = BlockHeader {
            version: 1,
            prev_block_hash: [9u8; 32],
            merkle_root: [3u8; 32],
            timestamp: 50,
            bits: 0x207fffff,
            nonce: 4,
        };
        let hash = [4u8; 32];
        storage.blocks().store_header(&hash, &header).unwrap();
        let rpc_hash = crate::storage::hashing::hash_to_rpc_hex(&hash);
        let err = BlockchainRpc::with_dependencies(storage)
            .get_block_header(&rpc_hash, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no height index"), "{err}");
    }

    #[tokio::test]
    async fn a_height_read_error_is_not_a_missing_block() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        storage.blocks().insert_short_height_row(100).unwrap();
        let err = BlockchainRpc::with_dependencies(storage)
            .get_block_hash(100)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("storage read failed"), "{err}");
        assert!(!err.contains("Block not found"), "{err}");
    }

    #[tokio::test]
    async fn segwit_totals_skip_the_legacy_transaction() {
        use blvm_protocol::serialization::transaction::serialize_transaction_with_witness;

        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let legacy = tx_with_output(50_000);
        let segwit = tx_with_output(40_000);
        let witness = vec![vec![vec![0x01u8]]];
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 3,
                bits: 0,
                nonce: 5,
            },
            transactions: vec![legacy, segwit.clone()].into_boxed_slice(),
        };
        let hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_block(&block).unwrap();
        storage
            .blocks()
            .store_witness(&hash, &[Vec::new(), witness.clone()])
            .unwrap();
        let stats = BlockchainRpc::with_dependencies(storage)
            .get_block_stats(&json!([crate::storage::hashing::hash_to_rpc_hex(&hash)]))
            .await
            .unwrap();
        let segwit_size = serialize_transaction_with_witness(&segwit, &witness)
            .unwrap()
            .len();
        assert_eq!(stats["swtxs"], 1);
        assert_eq!(stats["swtotal_size"].as_u64(), Some(segwit_size as u64));
        assert!(
            stats["swtotal_weight"].as_u64().unwrap() < stats["total_weight"].as_u64().unwrap()
        );
    }

    #[tokio::test]
    async fn getblock_verbosity_zero_is_hex_and_one_is_json() {
        use blvm_protocol::serialization::serialize_block_with_witnesses;

        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1,
                bits: 0,
                nonce: 2,
            },
            transactions: vec![tx_with_output(50_000)].into_boxed_slice(),
        };
        let hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_block(&block).unwrap();
        let rpc_hash = crate::storage::hashing::hash_to_rpc_hex(&hash);
        let rpc = BlockchainRpc::with_dependencies(storage);
        let hex_value = rpc.get_block_with_verbosity(&rpc_hash, 0).await.unwrap();
        let expected = hex::encode(serialize_block_with_witnesses(&block, &[], true));
        assert_eq!(hex_value.as_str(), Some(expected.as_str()));
        let object = rpc.get_block(&rpc_hash).await.unwrap();
        assert!(object.get("tx").is_some());
        assert!(object.as_str().is_none());
    }

    #[cfg(feature = "production")]
    #[tokio::test]
    async fn one_thousand_sat_fee_is_min_max_and_median() {
        use blvm_consensus::reorganization::{BlockUndoLog, UndoEntry};
        use blvm_protocol::UTXO;
        use std::sync::Arc;

        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let spent = OutPoint {
            hash: [7u8; 32],
            index: 0,
        };
        let spender = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: spent,
                script_sig: Vec::new(),
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 1_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1_600_000_000,
                bits: 0,
                nonce: 0,
            },
            transactions: vec![tx_with_output(50_000), spender].into_boxed_slice(),
        };
        let hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_block(&block).unwrap();
        let mut undo = BlockUndoLog::new();
        undo.push(UndoEntry {
            outpoint: spent,
            previous_utxo: Some(Arc::new(UTXO {
                value: 2_000,
                script_pubkey: vec![OP_1].into(),
                height: 1,
                is_coinbase: false,
            })),
            new_utxo: None,
        });
        storage.blocks().store_undo_log(&hash, &undo).unwrap();

        let stats = BlockchainRpc::with_dependencies(storage)
            .get_block_stats(&json!([crate::storage::hashing::hash_to_rpc_hex(&hash)]))
            .await
            .unwrap();
        assert_eq!(stats["minfee"].as_f64(), Some(0.00001));
        assert_eq!(stats["maxfee"].as_f64(), Some(0.00001));
        assert_eq!(stats["medianfee"].as_f64(), Some(0.00001));
    }

    #[cfg(feature = "production")]
    #[tokio::test]
    async fn spent_script_is_in_the_filter_and_the_header_is_not_zero() {
        use blvm_consensus::reorganization::{BlockUndoLog, UndoEntry};
        use blvm_protocol::bip158::{build_block_filter, match_filter};

        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let spent = OutPoint {
            hash: [9u8; 32],
            index: 0,
        };
        let script = vec![OP_1];
        let spender = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: spent,
                script_sig: Vec::new(),
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 1_000,
                script_pubkey: vec![0x51],
            }]
            .into(),
            lock_time: 0,
        };
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1_600_000_000,
                bits: 0,
                nonce: 1,
            },
            transactions: vec![tx_with_output(50_000), spender].into_boxed_slice(),
        };
        let hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_block(&block).unwrap();
        let mut undo = BlockUndoLog::new();
        undo.push(UndoEntry {
            outpoint: spent,
            previous_utxo: Some(std::sync::Arc::new(blvm_protocol::UTXO {
                value: 2_000,
                script_pubkey: script.clone().into(),
                height: 1,
                is_coinbase: false,
            })),
            new_utxo: None,
        });
        storage.blocks().store_undo_log(&hash, &undo).unwrap();

        let result = BlockchainRpc::with_dependencies(Arc::clone(&storage))
            .get_block_filter(&json!([crate::storage::hashing::hash_to_rpc_hex(&hash)]))
            .await
            .unwrap();
        let built = build_block_filter(block.transactions.as_ref(), &[script.clone()]).unwrap();
        assert!(match_filter(&built, &script));
        assert_eq!(
            result["filter"].as_str().unwrap(),
            hex::encode(&built.filter_data)
        );
        assert_ne!(result["header"].as_str().unwrap(), &"0".repeat(64));

        let rpc = BlockchainRpc::with_dependencies(Arc::clone(&storage));
        let before = rpc.get_index_info(&json!([])).await.unwrap();
        assert_eq!(
            before["basic block filter index"]["synced"].as_bool(),
            Some(false)
        );
        storage.chain().initialize(&block.header).unwrap();
        storage.blocks().store_height(0, &hash).unwrap();
        let after = rpc.get_index_info(&json!([])).await.unwrap();
        assert_eq!(
            after["basic block filter index"]["synced"].as_bool(),
            Some(true)
        );
    }

    #[cfg(feature = "production")]
    #[tokio::test]
    async fn invalidate_tip_moves_to_the_parent_and_drops_the_coinbase() {
        use blvm_consensus::reorganization::{BlockUndoLog, UndoEntry};
        use blvm_protocol::UTXO;
        use blvm_protocol::block::calculate_tx_id;
        use std::sync::Arc as StdArc;

        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let parent = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_000,
            bits: 0,
            nonce: 1,
        };
        storage.chain().initialize(&parent).unwrap();
        let parent_hash = storage.chain().get_tip_hash().unwrap().unwrap();
        storage
            .blocks()
            .store_header(&parent_hash, &parent)
            .unwrap();

        let coinbase = tx_with_output(50_000);
        let coinbase_id = calculate_tx_id(&coinbase);
        let child = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: parent_hash,
                merkle_root: [0u8; 32],
                timestamp: 1_600_000_001,
                bits: 0,
                nonce: 2,
            },
            transactions: vec![coinbase].into_boxed_slice(),
        };
        let child_hash = storage.blocks().get_block_hash(&child);
        storage.blocks().store_block(&child).unwrap();
        storage
            .blocks()
            .store_header(&child_hash, &child.header)
            .unwrap();
        storage.blocks().store_height(1, &child_hash).unwrap();
        let outpoint = OutPoint {
            hash: coinbase_id,
            index: 0,
        };
        let utxo = UTXO {
            value: 50_000,
            script_pubkey: vec![OP_1].into(),
            height: 1,
            is_coinbase: true,
        };
        storage.utxos().add_utxo(&outpoint, &utxo).unwrap();
        let mut undo = BlockUndoLog::new();
        undo.push(UndoEntry {
            outpoint,
            previous_utxo: None,
            new_utxo: Some(StdArc::new(utxo)),
        });
        storage.blocks().store_undo_log(&child_hash, &undo).unwrap();
        storage
            .chain()
            .update_tip(&child_hash, &child.header, 1)
            .unwrap();

        BlockchainRpc::with_dependencies(Arc::clone(&storage))
            .invalidate_block(&json!([crate::storage::hashing::hash_to_rpc_hex(
                &child_hash
            )]))
            .await
            .unwrap();

        assert_eq!(
            storage.chain().get_tip_hash().unwrap().unwrap(),
            parent_hash
        );
        assert!(storage.utxos().get_utxo(&outpoint).unwrap().is_none());
        let tips = BlockchainRpc::with_dependencies(storage)
            .get_chain_tips()
            .await
            .unwrap();
        let old_tip = tips
            .as_array()
            .unwrap()
            .iter()
            .find(|tip| {
                tip["hash"].as_str()
                    == Some(crate::storage::hashing::hash_to_rpc_hex(&child_hash).as_str())
            })
            .unwrap();
        assert_eq!(old_tip["status"], "invalid");
    }

    #[tokio::test]
    async fn side_tip_is_valid_fork() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let parent = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_000,
            bits: 0,
            nonce: 1,
        };
        storage.chain().initialize(&parent).unwrap();
        let parent_hash = storage.chain().get_tip_hash().unwrap().unwrap();
        let main = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: parent_hash,
                merkle_root: [0u8; 32],
                timestamp: 1_600_000_001,
                bits: 0,
                nonce: 2,
            },
            transactions: vec![tx_with_output(50_000)].into_boxed_slice(),
        };
        let main_hash = storage.blocks().get_block_hash(&main);
        storage
            .chain()
            .update_tip(&main_hash, &main.header, 1)
            .unwrap();
        let fork = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: parent_hash,
                merkle_root: [0u8; 32],
                timestamp: 1_600_000_002,
                bits: 0,
                nonce: 3,
            },
            transactions: vec![tx_with_output(50_000)].into_boxed_slice(),
        };
        let fork_hash = storage.blocks().get_block_hash(&fork);
        storage
            .chain()
            .block_index()
            .insert(
                &fork_hash,
                1,
                &parent_hash,
                crate::storage::block_index::BlockIndexStatus::Valid,
            )
            .unwrap();

        let tips = BlockchainRpc::with_dependencies(storage)
            .get_chain_tips()
            .await
            .unwrap();
        let tips = tips.as_array().unwrap();
        let status_of = |hash: &blvm_protocol::Hash| {
            let hex = crate::storage::hashing::hash_to_rpc_hex(hash);
            tips.iter()
                .find(|tip| tip["hash"].as_str() == Some(hex.as_str()))
                .unwrap()["status"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(status_of(&main_hash), "active");
        assert_eq!(status_of(&fork_hash), "valid-fork");
    }

    #[tokio::test]
    async fn verifychain_uses_the_stored_transactions() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let parent = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_000,
            bits: 0,
            nonce: 1,
        };
        storage.chain().initialize(&parent).unwrap();
        let parent_hash = storage.chain().get_tip_hash().unwrap().unwrap();
        let tx = tx_with_output(50_000);
        let root = blvm_protocol::mining::calculate_merkle_root(std::slice::from_ref(&tx)).unwrap();
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: parent_hash,
                merkle_root: root,
                timestamp: 1_600_000_001,
                bits: 0,
                nonce: 2,
            },
            transactions: vec![tx].into_boxed_slice(),
        };
        let hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_block(&block).unwrap();
        storage.blocks().store_height(1, &hash).unwrap();
        storage.chain().update_tip(&hash, &block.header, 1).unwrap();
        let rpc = BlockchainRpc::with_dependencies(Arc::clone(&storage));
        assert_eq!(
            rpc.verify_chain(Some(2), Some(1)).await.unwrap(),
            serde_json::Value::Bool(true)
        );

        let mut wrong = block.clone();
        wrong.header.merkle_root = [9u8; 32];
        wrong.header.nonce = 3;
        let wrong_hash = storage.blocks().get_block_hash(&wrong);
        storage.blocks().store_block(&wrong).unwrap();
        storage.blocks().store_height(1, &wrong_hash).unwrap();
        storage
            .chain()
            .update_tip(&wrong_hash, &wrong.header, 1)
            .unwrap();
        let report = rpc.verify_chain(Some(2), Some(1)).await.unwrap();
        let errors = report["errors"].as_array().unwrap();
        assert!(errors.iter().any(|err| {
            err.as_str()
                .is_some_and(|s| s.contains("incorrect merkle root"))
        }));
    }

    #[tokio::test]
    async fn missing_undo_log_does_not_move_the_tip() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let parent = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_000,
            bits: 0,
            nonce: 1,
        };
        storage.chain().initialize(&parent).unwrap();
        let parent_hash = storage.chain().get_tip_hash().unwrap().unwrap();
        storage
            .blocks()
            .store_header(&parent_hash, &parent)
            .unwrap();
        let child = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: parent_hash,
                merkle_root: [0u8; 32],
                timestamp: 1_600_000_001,
                bits: 0,
                nonce: 2,
            },
            transactions: vec![tx_with_output(50_000)].into_boxed_slice(),
        };
        let child_hash = storage.blocks().get_block_hash(&child);
        storage.blocks().store_block(&child).unwrap();
        storage
            .blocks()
            .store_header(&child_hash, &child.header)
            .unwrap();
        storage.blocks().store_height(1, &child_hash).unwrap();
        storage
            .chain()
            .update_tip(&child_hash, &child.header, 1)
            .unwrap();

        let err = BlockchainRpc::with_dependencies(Arc::clone(&storage))
            .invalidate_block(&json!([crate::storage::hashing::hash_to_rpc_hex(
                &child_hash
            )]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing undo log"), "{err}");
        assert_eq!(storage.chain().get_tip_hash().unwrap().unwrap(), child_hash);
    }

    #[tokio::test]
    async fn reconsider_clears_the_mark_and_leaves_the_tip() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let parent = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_000,
            bits: 0,
            nonce: 1,
        };
        storage.chain().initialize(&parent).unwrap();
        let parent_hash = storage.chain().get_tip_hash().unwrap().unwrap();
        let child = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: parent_hash,
                merkle_root: [0u8; 32],
                timestamp: 1_600_000_001,
                bits: 0,
                nonce: 2,
            },
            transactions: vec![tx_with_output(50_000)].into_boxed_slice(),
        };
        let child_hash = storage.blocks().get_block_hash(&child);
        storage
            .blocks()
            .store_header(&child_hash, &child.header)
            .unwrap();
        storage.chain().mark_invalid(&child_hash).unwrap();

        BlockchainRpc::with_dependencies(Arc::clone(&storage))
            .reconsider_block(&json!([crate::storage::hashing::hash_to_rpc_hex(
                &child_hash
            )]))
            .await
            .unwrap();

        assert!(!storage.chain().is_invalid(&child_hash).unwrap());
        assert_eq!(
            storage.chain().get_tip_hash().unwrap().unwrap(),
            parent_hash
        );
        assert_eq!(storage.chain().get_height().unwrap(), Some(0));
    }

    #[tokio::test]
    async fn p2wpkh_index_off_errors_and_a_full_spend_reports_both_sides() {
        use blvm_protocol::block::calculate_tx_id;

        let script = {
            let mut s = vec![0x00, 0x14];
            s.extend_from_slice(&[0x11u8; 20]);
            s
        };
        let temp = tempfile::TempDir::new().unwrap();
        let off = Arc::new(Storage::new(temp.path()).unwrap());
        assert!(
            BlockchainRpc::with_dependencies(off)
                .get_address_info(&json!([hex::encode(&script)]))
                .await
                .is_err()
        );

        let temp_on = tempfile::TempDir::new().unwrap();
        let mut indexing = crate::config::IndexingConfig::default();
        indexing.enable_address_index = true;
        let on = Arc::new(
            Storage::with_backend_pruning_and_indexing(
                temp_on.path(),
                crate::storage::database::default_backend(),
                None,
                Some(indexing),
                None,
                None,
                None,
            )
            .unwrap(),
        );
        let tx = tx_with_output(50_000);
        let mut paying = tx;
        paying.outputs[0].script_pubkey = script.clone();
        paying.outputs[0].value = 50_000;
        let txid = calculate_tx_id(&paying);
        on.transactions()
            .index_transaction(&paying, &[2u8; 32], 1, 0)
            .unwrap();
        on.transactions()
            .note_spend(&script, 50_000, &txid)
            .unwrap();
        let info = BlockchainRpc::with_dependencies(on)
            .get_address_info(&json!([hex::encode(&script)]))
            .await
            .unwrap();
        assert_eq!(info["received"].as_f64(), Some(0.0005));
        assert_eq!(info["sent"].as_f64(), Some(0.0005));
        assert_eq!(info["script_type"], "witness_v0_keyhash");
    }

    #[tokio::test]
    async fn a_utxo_read_error_is_not_reported_as_spent() {
        use blvm_protocol::block::calculate_tx_id;

        let script = {
            let mut s = vec![0x00, 0x14];
            s.extend_from_slice(&[0x11u8; 20]);
            s
        };
        let temp = tempfile::TempDir::new().unwrap();
        let mut indexing = crate::config::IndexingConfig::default();
        indexing.enable_address_index = true;
        let storage = Arc::new(
            Storage::with_backend_pruning_and_indexing(
                temp.path(),
                crate::storage::database::default_backend(),
                None,
                Some(indexing),
                None,
                None,
                None,
            )
            .unwrap(),
        );
        let mut paying = tx_with_output(50_000);
        paying.outputs[0].script_pubkey = script.clone();
        paying.outputs[0].value = 50_000;
        let txid = calculate_tx_id(&paying);
        storage
            .transactions()
            .index_transaction(&paying, &[2u8; 32], 1, 0)
            .unwrap();
        storage
            .utxos()
            .insert_undecodable_utxo(&blvm_protocol::OutPoint {
                hash: txid,
                index: 0,
            })
            .unwrap();
        let err = BlockchainRpc::with_dependencies(storage)
            .get_address_info(&json!([hex::encode(&script)]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("utxo read failed"), "{err}");
    }

    #[tokio::test]
    async fn signet_stays_unknown_and_segwit_follows_its_activation_height() {
        use crate::storage::chainstate::ChainParams;
        use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};

        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Signet).unwrap());
        let header = protocol.get_network_params().genesis_block.header.clone();
        let mut params = ChainParams::default();
        params.network = "signet".to_string();
        storage
            .chain()
            .initialize_with_params(&header, params)
            .unwrap();
        let rpc = BlockchainRpc::with_dependencies_and_protocol(
            Arc::clone(&storage),
            Arc::clone(&protocol),
        );
        let info = rpc.get_blockchain_info().await.unwrap();
        assert_eq!(info["chain"], "signet");
        assert_eq!(info["initialblockdownload"], true);
        assert_eq!(info["verificationprogress"].as_f64(), Some(0.0));
        assert_eq!(info["softforks"]["segwit"]["active"], false);
        assert_eq!(info["softforks"]["segwit"]["height"], 1);

        let tip = storage.chain().get_tip_hash().unwrap().unwrap();
        let mut child = header.clone();
        child.prev_block_hash = tip;
        storage.chain().update_tip(&tip, &child, 1).unwrap();
        let at_activation = rpc.get_blockchain_info().await.unwrap();
        assert_eq!(at_activation["chain"], "signet");
        assert_eq!(at_activation["initialblockdownload"], true);
        assert_eq!(at_activation["softforks"]["segwit"]["active"], true);

        let bare = tempfile::TempDir::new().unwrap();
        let bare_storage = Arc::new(Storage::new(bare.path()).unwrap());
        let testnet4 = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Testnet4).unwrap());
        let named = BlockchainRpc::with_dependencies_and_protocol(bare_storage, testnet4)
            .get_blockchain_info()
            .await
            .unwrap();
        assert_eq!(named["chain"], "testnet4");
        assert_eq!(named["initialblockdownload"], true);
    }
}

/// First height `getchaintxstats` includes.
/// An omitted count covers 4320 blocks ending at the tip.
pub(crate) fn chain_tx_stats_start_height(tip_height: u64, nblocks: Option<u64>) -> u64 {
    let nblocks = nblocks.unwrap_or(4320);
    if tip_height >= nblocks {
        tip_height - nblocks + 1
    } else {
        0
    }
}

/// First height `verifychain` checks.
/// An omitted count checks 288 blocks ending at the tip. `0` starts at genesis.
pub(crate) fn verify_chain_start_height(tip_height: u64, num_blocks: Option<u64>) -> u64 {
    match num_blocks.unwrap_or(288) {
        0 => 0,
        n => tip_height.saturating_sub(n.saturating_sub(1)),
    }
}

#[cfg(test)]
mod verify_chain_span_tests {
    use super::chain_tx_stats_start_height;
    use super::verify_chain_start_height;

    #[test]
    fn omitted_count_checks_288_blocks_and_zero_checks_the_whole_chain() {
        let tip = 10_000u64;
        let omitted = verify_chain_start_height(tip, None);
        assert_eq!(tip - omitted + 1, 288);
        assert_eq!(verify_chain_start_height(tip, Some(0)), 0);
        assert_eq!(verify_chain_start_height(tip, Some(1)), tip);
    }

    #[test]
    fn omitted_chain_tx_stats_covers_one_month() {
        let tip = 10_000u64;
        let omitted = chain_tx_stats_start_height(tip, None);
        assert_eq!(tip - omitted + 1, 4320);
        let explicit = chain_tx_stats_start_height(tip, Some(144));
        assert_eq!(tip - explicit + 1, 144);
    }
}

#[cfg(test)]
mod validate_address_network_tests {
    use super::BlockchainRpc;
    use crate::storage::Storage;
    use blvm_protocol::address::{BitcoinAddress, Network};
    use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn mainnet_rejects_a_testnet_witness_address() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::BitcoinV1).unwrap());
        let rpc = BlockchainRpc::with_dependencies_and_protocol(storage, protocol);
        let program = vec![0x11u8; 20];
        let testnet = BitcoinAddress::new(Network::Testnet, 0, program.clone())
            .unwrap()
            .encode()
            .unwrap();
        let mainnet = BitcoinAddress::new(Network::Mainnet, 0, program)
            .unwrap()
            .encode()
            .unwrap();
        let rejected = rpc.validate_address(&json!([testnet])).await.unwrap();
        assert_eq!(rejected["isvalid"], false);
        let accepted = rpc.validate_address(&json!([mainnet])).await.unwrap();
        assert_eq!(accepted["isvalid"], true);
    }

    #[tokio::test]
    async fn version_zero_program_of_the_wrong_length_is_invalid() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::BitcoinV1).unwrap());
        let rpc = BlockchainRpc::with_dependencies_and_protocol(storage, protocol);
        let p2wsh = BitcoinAddress::new(Network::Mainnet, 0, vec![0x22u8; 32])
            .unwrap()
            .encode()
            .unwrap();
        assert_eq!(
            rpc.validate_address(&json!([p2wsh])).await.unwrap()["isvalid"],
            true
        );
        let wrong_length = "bc1qzyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zymalz5m";
        assert_eq!(
            rpc.validate_address(&json!([wrong_length])).await.unwrap()["isvalid"],
            false
        );
    }
}
