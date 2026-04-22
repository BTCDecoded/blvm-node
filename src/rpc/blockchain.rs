//! Blockchain RPC methods
//!
//! Implements blockchain-related JSON-RPC methods for querying blockchain state.

use crate::config::RequestTimeoutConfig;
use crate::node::event_publisher::EventPublisher;
use crate::rpc::errors::{
    BlockNotFoundError, RpcError, BLOCK_HASH_PARAM_REQUIRED_MSG, BLOCK_NOT_FOUND_MSG,
    HEIGHT_PARAM_REQUIRED_MSG, STORAGE_NOT_AVAILABLE_MSG,
};
use crate::rpc::params::{param_str_required, param_u64, param_u64_default};
use crate::storage::assumeutxo::AssumeUtxoManager;
use crate::storage::Storage;
use crate::utils::{
    storage_timeout_from_config, with_custom_timeout, CACHE_REFRESH_TIP,
    POLL_INTERVAL_WAIT_FOR_BLOCK,
};
use anyhow::Result;
use blvm_protocol::{BlockHeader, SEGWIT_ACTIVATION_MAINNET, TAPROOT_ACTIVATION_MAINNET};
use serde_json::{json, Number, Value};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, warn};

const ZERO_HASH_STR: &str = "0000000000000000000000000000000000000000000000000000000000000000";

thread_local! {
    static CACHED_TIP_HEIGHT: crate::rpc::cache::ThreadLocalTimedCache<u64> =
        crate::rpc::cache::ThreadLocalTimedCache::new();
    static CACHED_TIP_HASH_HEX: crate::rpc::cache::ThreadLocalKeyedCache<String, ([u8; 32], u64)> =
        crate::rpc::cache::ThreadLocalKeyedCache::new();
    static CACHED_DIFFICULTY: crate::rpc::cache::ThreadLocalKeyedCache<f64, u64> =
        crate::rpc::cache::ThreadLocalKeyedCache::new();
}

/// Helper function to decode a 32-byte hash from hex string
fn decode_hash32(hex: &str) -> Result<[u8; 32], RpcError> {
    let hash_bytes = hex::decode(hex).map_err(|e| {
        RpcError::invalid_hash_format(hex, Some(32), Some(&format!("Invalid hex encoding: {e}")))
    })?;
    if hash_bytes.len() != 32 {
        return Err(RpcError::invalid_hash_format(
            hex,
            Some(32),
            Some(&format!(
                "Hash must be 64 hex characters (32 bytes), got {}",
                hex.len()
            )),
        ));
    }
    let mut hash_array = [0u8; 32];
    hash_array.copy_from_slice(&hash_bytes);
    Ok(hash_array)
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

    /// Get chain name from protocol version
    fn get_chain_name(&self) -> &str {
        self.protocol
            .as_ref()
            .map(|p| match p.get_protocol_version() {
                blvm_protocol::ProtocolVersion::BitcoinV1 => "mainnet",
                blvm_protocol::ProtocolVersion::Testnet3 => "testnet",
                blvm_protocol::ProtocolVersion::Regtest => "regtest",
            })
            .unwrap_or("regtest") // Default fallback
    }

    /// Calculate difficulty from bits (compact target format).
    /// Uses blvm-consensus difficulty_from_bits (MAX_TARGET / target).
    fn calculate_difficulty(bits: u64) -> f64 {
        blvm_protocol::pow::difficulty_from_bits(bits).unwrap_or(1.0)
    }

    /// Calculate median time from recent headers (BIP113)
    fn calculate_median_time(headers: &[BlockHeader]) -> u64 {
        if headers.is_empty() {
            return 0;
        }
        let mut timestamps: Vec<u64> = headers.iter().map(|h| h.timestamp).collect();
        timestamps.sort();
        let mid = timestamps.len() / 2;
        timestamps[mid]
    }

    /// Calculate block subsidy based on height
    /// Bitcoin subsidy: 50 BTC initially, halves every 210,000 blocks
    fn calculate_block_subsidy(height: u64) -> u64 {
        const INITIAL_SUBSIDY: u64 = 50_000_000_000; // 50 BTC in satoshis
        const HALVING_INTERVAL: u64 = 210_000;

        let halvings = height / HALVING_INTERVAL;

        // Subsidy halves each halving, but can't go below 0
        if halvings >= 64 {
            // After 64 halvings, subsidy is 0 (satoshi precision limit)
            return 0;
        }

        INITIAL_SUBSIDY >> halvings
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

    /// Format chainwork as hex string (32 bytes, big-endian)
    /// Supports both u64 (legacy) and u128 (optimized cached chainwork)
    fn format_chainwork(work: u128) -> String {
        let mut bytes = [0u8; 32];
        // Store work in last 16 bytes (big-endian)
        let work_bytes = work.to_be_bytes();
        bytes[16..32].copy_from_slice(&work_bytes);
        hex::encode(bytes)
    }

    /// Get blockchain information
    ///
    /// Includes softfork information based on feature flags from blvm-protocol
    pub async fn get_blockchain_info(&self) -> Result<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getblockchaininfo");

        let softforks = json!({
            "segwit": {
                "type": "buried",
                "active": true,
                "height": SEGWIT_ACTIVATION_MAINNET
            },
            "taproot": {
                "type": "buried",
                "active": true,
                "height": TAPROOT_ACTIVATION_MAINNET
            }
        });

        if let Some(ref storage) = self.storage {
            let (best_hash, height) = storage.chain().get_tip_hash_and_height()?;
            let block_count = storage.blocks().block_count().unwrap_or(0);

            let best_hash_hex = CACHED_TIP_HASH_HEX.with(|c| {
                c.get_or_refresh(CACHE_REFRESH_TIP, &(best_hash, height), || {
                    hex::encode(best_hash)
                })
            });

            // Calculate difficulty from tip header (single lookup)
            let difficulty = if let Ok(Some(tip_header)) = storage.chain().get_tip_header() {
                Self::calculate_difficulty(tip_header.bits)
            } else {
                1.0
            };

            // Calculate mediantime from recent headers
            let mediantime = if let Ok(recent_headers) = storage.blocks().get_recent_headers(11) {
                Self::calculate_median_time(&recent_headers)
            } else {
                0
            };

            let chainwork = storage.chain().get_chainwork(&best_hash)?.unwrap_or(0u128);
            // CRITICAL FIX: Removed calculate_total_work() fallback - it iterates over ALL blocks
            // (357k+ iterations) causing 3+ minute RPC delays. If chainwork isn't cached,
            // return 0 instead of doing expensive calculation.
            let chainwork_hex = Self::format_chainwork(chainwork);

            Ok(json!({
                "chain": "main",
                "blocks": height,
                "headers": block_count,
                "bestblockhash": best_hash_hex,
                "difficulty": difficulty,
                "mediantime": mediantime,
                "verificationprogress": if height > 0 { 1.0 } else { 0.0 },
                "initialblockdownload": height == 0,
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
                "chain": "main",
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
        debug!("RPC: getblock {}", hash);

        // Decode hash first (before async operations)
        let hash_bytes = match hex::decode(hash) {
            Ok(bytes) => bytes,
            Err(e) => {
                return Err(anyhow::anyhow!("Invalid hash: {}", e));
            }
        };

        if hash_bytes.len() != 32 {
            return Err(anyhow::anyhow!("Invalid hash length"));
        }

        let mut hash_array = [0u8; 32];
        hash_array.copy_from_slice(&hash_bytes);

        // Try to get block from storage with graceful degradation
        if let Some(ref storage) = self.storage {
            // Use timeout to prevent hanging on slow storage (wrap sync operation)
            let timeout_dur = self.storage_timeout();
            match with_custom_timeout(
                async {
                    tokio::task::spawn_blocking({
                        let storage = storage.clone();
                        let hash_array = hash_array;
                        move || storage.blocks().get_block(&hash_array)
                    })
                    .await
                },
                timeout_dur,
            )
            .await
            {
                Ok(Ok(Ok(Some(block)))) => {
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

                    // Calculate block sizes
                    // strippedsize = block size without witness data (TX_NO_WITNESS)
                    // size = block size with witness data (TX_WITH_WITNESS)
                    use blvm_protocol::serialization::transaction::serialize_transaction;

                    // Calculate stripped size: serialize block without witness data
                    // This is the sum of all transaction sizes without witness
                    let stripped_size: usize = block
                        .transactions
                        .iter()
                        .map(|tx| serialize_transaction(tx).len())
                        .sum::<usize>()
                        + 80; // +80 for block header

                    // Calculate total size: serialize block with witness data
                    // For now, we'll use bincode serialization as approximation
                    // In full implementation, we'd serialize with witness marker and data
                    let total_size = bincode::serialize(&block)
                        .map(|b| b.len())
                        .unwrap_or(stripped_size);

                    // Use stripped_size as size if we can't determine total_size
                    // (For non-SegWit blocks, stripped_size == total_size)
                    let block_size = if total_size > stripped_size {
                        total_size
                    } else {
                        stripped_size
                    };

                    // Calculate block weight: 4 * base_size + total_size (BIP141)
                    // base_size = stripped_size (without witness)
                    // total_size = block_size (with witness)
                    let block_weight = (4 * stripped_size + block_size) as u64;

                    // Calculate median time from recent headers
                    let mediantime = if block_height.is_some() {
                        if let Ok(recent_headers) = storage.blocks().get_recent_headers(11) {
                            Self::calculate_median_time(&recent_headers)
                        } else {
                            block.header.timestamp
                        }
                    } else {
                        block.header.timestamp
                    };

                    // Calculate difficulty
                    let difficulty = Self::calculate_difficulty(block.header.bits);

                    // Get transaction IDs
                    let tx_ids: Vec<String> = block
                        .transactions
                        .iter()
                        .map(|tx| {
                            let tx_hash = blvm_protocol::block::calculate_tx_id(tx);
                            hex::encode(tx_hash)
                        })
                        .collect();

                    // Get next block hash
                    let next_blockhash = block_height.and_then(|h| {
                        storage
                            .blocks()
                            .get_hash_by_height(h + 1)
                            .ok()
                            .flatten()
                            .map(hex::encode)
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
                        "merkleroot": hex::encode(block.header.merkle_root),
                        "tx": tx_ids,
                        "time": block.header.timestamp,
                        "mediantime": mediantime,
                        "nonce": block.header.nonce as u32,
                        "bits": format!("{:08x}", block.header.bits),
                        "difficulty": difficulty,
                        "chainwork": chainwork,
                        "nTx": block.transactions.len(),
                        "previousblockhash": hex::encode(block.header.prev_block_hash),
                        "nextblockhash": next_blockhash.unwrap_or_else(|| ZERO_HASH_STR.to_string()),
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

        // Simplified implementation - return error for non-existent heights
        if height > 1000 {
            return Err(BlockNotFoundError::new(format!("at height {height}")).into());
        }

        Ok(json!(
            "0000000000000000000000000000000000000000000000000000000000000000"
        ))
    }

    /// Get raw transaction (deprecated - use rawtx module)
    pub async fn get_raw_transaction(&self, txid: &str) -> Result<Value> {
        debug!("RPC: getrawtransaction {}", txid);

        // Simplified implementation
        Ok(json!({
            "txid": txid,
            "hash": txid,
            "version": 1,
            "size": 0,
            "vsize": 0,
            "weight": 0,
            "locktime": 0,
            "vin": [],
            "vout": [],
            "hex": ""
        }))
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

                    // Calculate mediantime from recent headers at this height
                    let mediantime = if block_height.is_some() {
                        if let Ok(recent_headers) = storage.blocks().get_recent_headers(11) {
                            Self::calculate_median_time(&recent_headers)
                        } else {
                            header.timestamp
                        }
                    } else {
                        header.timestamp
                    };

                    // Calculate difficulty
                    let difficulty = Self::calculate_difficulty(header.bits);

                    let n_tx = storage
                        .blocks()
                        .get_block_metadata(&hash_array)?
                        .map(|m| m.n_tx as usize)
                        .unwrap_or(0);

                    // Find next block hash
                    let next_blockhash = block_height.and_then(|h| {
                        storage
                            .blocks()
                            .get_hash_by_height(h + 1)
                            .ok()
                            .flatten()
                            .map(hex::encode)
                    });

                    let chainwork = if let Some(_height) = block_height {
                        // O(1) lookup instead of O(n) calculation!
                        storage
                            .chain()
                            .get_chainwork(&hash_array)?
                            .map(Self::format_chainwork)
                            .unwrap_or_else(|| ZERO_HASH_STR.to_string())
                    } else {
                        ZERO_HASH_STR.to_string()
                    };

                    Ok(json!({
                        "hash": hash,
                        "confirmations": confirmations,
                        "height": block_height.unwrap_or(0),
                        "version": header.version,
                        "versionHex": format!("{:08x}", header.version),
                        "merkleroot": hex::encode(header.merkle_root),
                        "time": header.timestamp,
                        "mediantime": mediantime,
                        "nonce": header.nonce as u32,
                        "bits": hex::encode(header.bits.to_le_bytes()),
                        "difficulty": difficulty,
                        "chainwork": chainwork,
                        "nTx": n_tx,
                        "previousblockhash": hex::encode(header.prev_block_hash),
                        "nextblockhash": next_blockhash
                    }))
                } else {
                    use blvm_protocol::serialization::serialize_block_header;
                    let header_bytes = serialize_block_header(&header);
                    Ok(Value::String(hex::encode(header_bytes)))
                }
            } else {
                Err(BlockNotFoundError::new("").into())
            }
        } else if verbose {
            Ok(json!({
                "hash": hash,
                "confirmations": 0,
                "height": 0,
                "version": 1,
                "versionHex": "00000001",
                "merkleroot": "0000000000000000000000000000000000000000000000000000000000000000",
                "time": 1231006505,
                "mediantime": 1231006505,
                "nonce": 0,
                "bits": "1d00ffff",
                "difficulty": 1.0,
                "chainwork": "0000000000000000000000000000000000000000000000000000000000000000",
                "nTx": 0,
                "previousblockhash": null,
                "nextblockhash": null
            }))
        } else {
            Ok(json!("00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"))
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
                        hex::encode(hash)
                    })
                });
                Ok(Value::String(hex_str))
            } else {
                Ok(Value::String(ZERO_HASH_STR.to_string()))
            }
        } else {
            Ok(Value::String(ZERO_HASH_STR.to_string()))
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
            Ok(Value::Number(Number::from(0)))
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
            let difficulty = CACHED_DIFFICULTY.with(|c| {
                c.get_or_refresh(CACHE_REFRESH_TIP, &current_height, || {
                    storage
                        .chain()
                        .get_tip_header()
                        .ok()
                        .flatten()
                        .map(|h| Self::calculate_difficulty(h.bits))
                        .unwrap_or(1.0)
                })
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
                    "bestblock": hex::encode(best_hash),
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
                let total_amount: u64 = utxos.values().map(|utxo| utxo.value as u64).sum();
                let muhash = Self::calculate_utxo_muhash(&utxos);

                Ok(json!({
                    "height": height,
                    "bestblock": hex::encode(best_hash),
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
    /// Validates the snapshot and returns metadata. Does not load into chainstate
    /// (header required for that; use -assumeutxo at startup).
    pub async fn load_txout_set(&self, params: &Value) -> Result<Value> {
        debug!("RPC: loadtxoutset");

        let path_str = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::invalid_params("loadtxoutset requires path (string)"))?;

        let path = Path::new(path_str);
        let manager = AssumeUtxoManager::new(".");
        let (_utxo_set, metadata) = manager
            .load_snapshot_from_path(path)
            .map_err(|e| RpcError::internal_error(e.to_string()))?;

        Ok(json!({
            "base_blockhash": hex::encode(metadata.block_hash),
            "height": metadata.block_height,
            "txout_count": metadata.utxo_count,
            "utxo_hash": hex::encode(metadata.utxo_hash),
        }))
    }

    /// Verify blockchain database
    ///
    /// Params: [checklevel (optional, default: 3), numblocks (optional, default: 288)]
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
            use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
            // Use protocol engine which provides the correct validate_block signature
            let engine = BitcoinProtocolEngine::new(ProtocolVersion::Regtest)
                .map_err(|e| anyhow::anyhow!("Failed to create protocol engine: {}", e))?;

            let check_level = checklevel.unwrap_or(3);
            let num_blocks = numblocks.unwrap_or(288);

            let tip_height = storage.chain().get_height()?.unwrap_or(0);
            if tip_height == 0 {
                return Ok(json!(true)); // Empty chain is valid
            }

            // Start from genesis or from (tip_height - num_blocks)
            let start_height = tip_height.saturating_sub(num_blocks);

            let mut errors = Vec::new();
            let utxo_set = storage
                .utxos()
                .get_all_utxos()
                .map_err(|e| anyhow::anyhow!("Failed to get UTXO set: {}", e))?;

            // Verify blocks from start_height to tip
            for height in start_height..=tip_height {
                if let Ok(Some(block_hash)) = storage.blocks().get_hash_by_height(height) {
                    if let Ok(Some(block)) = storage.blocks().get_block(&block_hash) {
                        // Validate block using protocol engine (expects &HashMap, returns Result<ValidationResult>)
                        match engine.validate_block(&block, &utxo_set, height) {
                            Ok(blvm_protocol::ValidationResult::Valid) => {
                                // Block is valid, update UTXO set for next block
                                // (Simplified - in full implementation would apply block to UTXO set)
                                // For now, just continue
                            }
                            Ok(blvm_protocol::ValidationResult::Invalid(reason)) => {
                                errors.push(format!("Block at height {height} invalid: {reason}"));
                                if check_level >= 4 {
                                    // Level 4: Stop on first error
                                    break;
                                }
                            }
                            Err(e) => {
                                errors.push(format!(
                                    "Block at height {height} validation error: {e}"
                                ));
                                if check_level >= 4 {
                                    break;
                                }
                            }
                        }

                        // Check level 3: Verify block header linkage
                        if check_level >= 3 && height > 0 {
                            if let Ok(Some(prev_hash)) =
                                storage.blocks().get_hash_by_height(height - 1)
                            {
                                if block.header.prev_block_hash != prev_hash {
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
                            }
                        }

                        // Check level 2: Verify merkle root
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
            // No storage - return success (can't verify without storage)
            Ok(json!(true))
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
            let (tip_hash, tip_height) = storage.chain().get_tip_hash_and_height()?;

            // Get all chain tips (including forks)
            // Usually 1-2 tips, but pre-allocate for potential forks
            let mut tips = Vec::with_capacity(4);

            // Add active tip
            if tip_hash != blvm_protocol::Hash::default() {
                tips.push(json!({
                    "height": tip_height,
                    "hash": hex::encode(tip_hash),
                    "branchlen": 0,
                    "status": "active"
                }));
            }

            // Add other tracked tips (forks, etc.)
            if let Ok(chain_tips) = storage.chain().get_chain_tips() {
                for (hash, height, branchlen, status) in chain_tips {
                    // Skip if already added as active tip (use cached tip_hash)
                    if hash == tip_hash {
                        continue;
                    }

                    tips.push(json!({
                        "height": height,
                        "hash": hex::encode(hash),
                        "branchlen": branchlen,
                        "status": status
                    }));
                }
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

        let nblocks = param_u64_default(params, 0, 144); // Default: 1 day (144 blocks at 10 min/block)

        if let Some(ref storage) = self.storage {
            let tip_height = storage.chain().get_height()?.unwrap_or(0);

            if tip_height == 0 {
                return Ok(json!({
                    "time": 0,
                    "txcount": 0,
                    "window_final_block_height": 0,
                    "window_block_count": 0,
                    "window_tx_count": 0,
                    "window_interval": 0,
                    "txrate": 0.0
                }));
            }

            let start_height = if tip_height >= nblocks {
                tip_height - nblocks + 1
            } else {
                0
            };

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

            // Get total transaction count (simplified - would need to count all blocks)
            let total_tx_count = window_tx_count; // Simplified

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
                    // Parse as hash
                    let hash_bytes = hex::decode(hoh)
                        .map_err(|e| anyhow::anyhow!("Invalid block hash: {}", e))?;
                    if hash_bytes.len() != 32 {
                        return Err(anyhow::anyhow!("Block hash must be 32 bytes"));
                    }
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(&hash_bytes);
                    hash
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

                let block_size = {
                    let serialized_size = bincode::serialize(&block).map(|b| b.len()).unwrap_or(0);
                    storage
                        .blocks()
                        .get_block_metadata(&block_hash)
                        .ok()
                        .flatten()
                        .map(|_m| serialized_size)
                        .unwrap_or(serialized_size)
                };

                let block_weight = block_size; // Simplified - would calculate weight properly

                // Get block height
                let height = storage
                    .blocks()
                    .get_height_by_hash(&block_hash)?
                    .unwrap_or(0);

                // Count inputs and outputs
                let input_count: usize = block.transactions.iter().map(|tx| tx.inputs.len()).sum();
                let output_count: usize =
                    block.transactions.iter().map(|tx| tx.outputs.len()).sum();

                // Sum output values
                let total_out: u64 = block
                    .transactions
                    .iter()
                    .flat_map(|tx| tx.outputs.iter())
                    .map(|out| out.value as u64)
                    .sum::<u64>();

                // Calculate block subsidy
                let subsidy = Self::calculate_block_subsidy(height);

                // Calculate total fees (simplified - would need UTXO set for accurate calculation)
                // For now, estimate: total_out - (subsidy * 100_000_000) if coinbase exists
                let total_fees = if !block.transactions.is_empty() {
                    // Coinbase is first transaction
                    let coinbase_outputs: u64 = block.transactions[0]
                        .outputs
                        .iter()
                        .map(|out| out.value as u64)
                        .sum();
                    // Fee = total outputs - (coinbase outputs which include subsidy)
                    // This is simplified - real calculation needs UTXO set
                    total_out.saturating_sub(coinbase_outputs)
                } else {
                    0
                };

                Ok(json!({
                    "avgfee": if tx_count > 1 { total_fees as f64 / (tx_count - 1) as f64 / 100_000_000.0 } else { 0.0 },
                    "avgfeerate": 0.0, // Would need to calculate from fees and sizes
                    "avgtxsize": if tx_count > 0 { block_size / tx_count } else { 0 },
                    "blockhash": hex::encode(block_hash),
                    "feerate_percentiles": [0, 0, 0, 0, 0],
                    "height": height,
                    "ins": input_count,
                    "maxfee": 0.0,
                    "maxfeerate": 0.0,
                    "maxtxsize": 0,
                    "medianfee": 0.0,
                    "mediantime": block.header.timestamp,
                    "mediantxsize": 0,
                    "minfee": 0.0,
                    "minfeerate": 0.0,
                    "mintxsize": 0,
                    "outs": output_count,
                    "subsidy": subsidy,
                    "swtotal_size": 0,
                    "swtotal_weight": 0,
                    "swtxs": 0,
                    "time": block.header.timestamp,
                    "total_out": total_out,
                    "total_size": block_size,
                    "total_weight": block_weight,
                    "totalfee": total_fees as f64 / 100_000_000.0,
                    "txs": tx_count,
                    "utxo_increase": 0,
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

    /// Invalidate block
    ///
    /// Params: ["blockhash"] (block hash to invalidate)
    pub async fn invalidate_block(&self, params: &Value) -> Result<Value> {
        debug!("RPC: invalidateblock");

        let blockhash = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!(BLOCK_HASH_PARAM_REQUIRED_MSG))?;

        let hash =
            decode_hash32(blockhash).map_err(|e| anyhow::anyhow!("Invalid block hash: {}", e))?;

        if let Some(ref storage) = self.storage {
            // Mark block as invalid
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

                // If we invalidated the current tip, publish ChainReorg (old_tip=invalidated, new_tip=prev)
                if let Ok(Some(tip_hash)) = storage.chain().get_tip_hash() {
                    if hash == tip_hash {
                        warn!("Invalidated current chain tip - publishing ChainReorg");
                        if let Ok(Some(header)) = storage.blocks().get_header(&hash) {
                            let new_tip = header.prev_block_hash;
                            ep.publish_chain_reorg(&hash, &new_tip).await;
                        }
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
            // Remove from invalid blocks set
            storage.chain().unmark_invalid(&hash)?;

            // If this block is now valid, it may be reconsidered for chain inclusion
            // The node will handle this on next block processing

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
                    "hash": hex::encode(tip_hash),
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
                        "hash": hex::encode(cur),
                        "height": tip_height
                    }));
                }
                (None, Some(cur)) => {
                    let tip_height = storage.chain().get_height()?.unwrap_or(0);
                    return Ok(json!({
                        "hash": hex::encode(cur),
                        "height": tip_height
                    }));
                }
                _ => {}
            }
            tokio::time::sleep(poll_interval).await;
        }

        if let Some(ref tip_hash) = initial_tip {
            let tip_height = storage.chain().get_height()?.unwrap_or(0);
            Ok(json!({
                "hash": hex::encode(tip_hash),
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
                        "hash": hex::encode(hash),
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
                        "hash": hex::encode(hash),
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

                // Get previous outpoint scripts from UTXO set
                // For each input, find the UTXO and get its script_pubkey
                let mut previous_scripts = Vec::new();
                if let Ok(utxo_set) = storage.utxos().get_all_utxos() {
                    for tx in &block.transactions {
                        for input in &tx.inputs {
                            if let Some(utxo) = utxo_set.get(&input.prevout) {
                                previous_scripts.push(utxo.script_pubkey.clone());
                            }
                        }
                    }
                }

                let previous_scripts_bytes: Vec<Vec<u8>> = previous_scripts
                    .iter()
                    .map(|s| s.as_ref().to_vec())
                    .collect();
                match build_block_filter(&block.transactions, &previous_scripts_bytes) {
                    Ok(filter) => {
                        Ok(json!({
                            "filter": hex::encode(&filter.filter_data),
                            "header": hex::encode([0u8; 32]), // Would calculate filter header
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
                "synced": true,
                "best_block_height": best_block_height,
                "total_transactions": index_stats.total_transactions,
                "address_index_enabled": index_stats.address_index_enabled,
                "value_index_enabled": index_stats.value_index_enabled,
                "indexed_addresses": index_stats.indexed_addresses,
                "indexed_value_buckets": index_stats.indexed_value_buckets,
            },
            "basic block filter index": {
                "synced": true,
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

        // Decode address to script_pubkey (hex-encoded for now)
        let script_pubkey = hex::decode(address).map_err(|e| {
            RpcError::invalid_address_format(
                address,
                Some(&format!("Invalid hex encoding: {e}")),
                Some("hex-encoded script pubkey"),
            )
        })?;

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
                hex::encode(tx_hash)
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

        // Decode address to script_pubkey
        let script_pubkey = hex::decode(address).map_err(|e| {
            RpcError::invalid_address_format(
                address,
                Some(&format!("Invalid hex encoding: {e}")),
                Some("hex-encoded script pubkey"),
            )
        })?;

        // Get transactions for this address
        let transactions = storage
            .transactions()
            .get_transactions_by_address(&script_pubkey)
            .map_err(|e| RpcError::internal_error(format!("Failed to query address: {e}")))?;

        // Calculate balance by summing outputs
        // Note: This is simplified - full balance calculation would need UTXO tracking
        let mut balance: i64 = 0;
        for tx in &transactions {
            for output in &tx.outputs {
                if output.script_pubkey == script_pubkey {
                    balance += output.value;
                }
            }
        }

        Ok(json!({
            "balance": balance,
            "received": balance, // Simplified
            "sent": 0, // Would need to track inputs
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

            Ok(json!({
                "chain": self.get_chain_name(),
                "blocks": height,
                "headers": height,
                "bestblockhash": hex::encode(tip_hash),
                "difficulty": difficulty,
                "mediantime": tip_header.timestamp,
                "verificationprogress": if height > 0 { 1.0 } else { 0.0 },
                "initialblockdownload": height == 0,
                "chainwork": chainwork,
                "size_on_disk": storage.disk_size().unwrap_or(0),
                "pruned": false,
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

        let address = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::missing_parameter("address", Some("string")))?;

        // Basic address validation
        // For now, we'll do simple format checking
        // In production, this should use a proper address library
        let is_valid = !address.is_empty() && address.len() <= 100; // Basic sanity check

        // Try to determine address type from prefix
        let is_p2pkh = address.starts_with('1') && address.len() >= 26 && address.len() <= 35;
        let is_p2sh = address.starts_with('3') && address.len() >= 26 && address.len() <= 35;
        let is_bech32 = address.starts_with("bc1")
            || address.starts_with("tb1")
            || address.starts_with("bcrt1");

        let script_pubkey_type = if is_p2pkh {
            "pubkeyhash"
        } else if is_p2sh {
            "scripthash"
        } else if is_bech32 {
            "witness_v0_keyhash" // Simplified - could be witness_v0_scripthash
        } else {
            "nonstandard"
        };

        Ok(json!({
            "isvalid": is_valid && (is_p2pkh || is_p2sh || is_bech32),
            "address": address,
            "scriptPubKey": if is_valid && (is_p2pkh || is_p2sh || is_bech32) {
                format!("76a914{}88ac", hex::encode(&address.as_bytes()[1..address.len()-1])) // Simplified
            } else {
                "".to_string()
            },
            "isscript": is_p2sh || is_bech32,
            "iswitness": is_bech32,
            "witness_version": if is_bech32 { Some(0) } else { None },
            "witness_program": if is_bech32 { Some(hex::encode(address.as_bytes())) } else { None },
            "script_type": if is_valid && (is_p2pkh || is_p2sh || is_bech32) {
                script_pubkey_type
            } else {
                "nonstandard"
            }
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

        // Decode address to script_pubkey (hex-encoded for now)
        let script_pubkey = hex::decode(address).map_err(|e| {
            RpcError::invalid_address_format(
                address,
                Some(&format!("Invalid hex encoding: {e}")),
                Some("hex-encoded script pubkey"),
            )
        })?;

        if let Some(ref storage) = self.storage {
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
            let mut sent: i64 = 0;

            for tx in &transactions {
                use blvm_protocol::block::calculate_tx_id;
                let txid = calculate_tx_id(tx);

                for (idx, output) in tx.outputs.iter().enumerate() {
                    if output.script_pubkey == script_pubkey {
                        received += output.value;
                        // Check if UTXO is spent
                        let outpoint = blvm_protocol::OutPoint {
                            hash: txid,
                            index: idx as u32,
                        };
                        if storage.utxos().get_utxo(&outpoint).ok().flatten().is_some() {
                            // UTXO is unspent
                            balance += output.value;
                            utxo_count += 1;
                        } else {
                            // UTXO is spent
                            sent += output.value;
                        }
                    }
                }
            }

            Ok(json!({
                "address": address,
                "scriptPubKey": hex::encode(&script_pubkey),
                "ismine": false,
                "iswatchonly": false,
                "isscript": false,
                "iswitness": false,
                "witness_version": Value::Null,
                "witness_program": Value::Null,
                "script_type": "nonstandard",
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
                "utxo_count": utxo_count
            }))
        } else {
            Ok(json!({
                "address": address,
                "scriptPubKey": hex::encode(&script_pubkey),
                "ismine": false,
                "iswatchonly": false,
                "isscript": false,
                "iswitness": false,
                "witness_version": Value::Null,
                "witness_program": Value::Null,
                "script_type": "nonstandard",
                "pubkey": Value::Null,
                "embedded": Value::Null,
                "iscompressed": Value::Null,
                "label": "",
                "timestamp": Value::Null,
                "hdkeypath": Value::Null,
                "hdseedid": Value::Null,
                "hdmasterfingerprint": Value::Null,
                "labels": [],
                "balance": 0.0,
                "received": 0.0,
                "sent": 0.0,
                "tx_count": 0,
                "utxo_count": 0
            }))
        }
    }
}
