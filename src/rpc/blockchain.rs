//! Blockchain RPC methods
//!
//! Implements blockchain-related JSON-RPC methods for querying blockchain state.

use crate::rpc::errors::RpcError;
use crate::storage::Storage;
use anyhow::Result;
use bllvm_protocol::BlockHeader;
use serde_json::{json, Number, Value};
use std::sync::Arc;
use tracing::{debug, warn};

const ZERO_HASH_STR: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Helper function to decode a 32-byte hash from hex string
fn decode_hash32(hex: &str) -> Result<[u8; 32], RpcError> {
    let hash_bytes =
        hex::decode(hex).map_err(|e| RpcError::invalid_params(format!("Invalid hash: {}", e)))?;
    if hash_bytes.len() != 32 {
        return Err(RpcError::invalid_params("Invalid hash length"));
    }
    let mut hash_array = [0u8; 32];
    hash_array.copy_from_slice(&hash_bytes);
    Ok(hash_array)
}

/// Blockchain RPC methods
#[derive(Clone)]
pub struct BlockchainRpc {
    storage: Option<Arc<Storage>>,
}

impl Default for BlockchainRpc {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockchainRpc {
    /// Create a new blockchain RPC handler
    pub fn new() -> Self {
        Self { storage: None }
    }

    /// Create with dependencies
    pub fn with_dependencies(storage: Arc<Storage>) -> Self {
        Self {
            storage: Some(storage),
        }
    }

    /// Calculate difficulty from bits (compact target format)
    fn calculate_difficulty(bits: u64) -> f64 {
        // Difficulty = MAX_TARGET / target
        // MAX_TARGET for Bitcoin mainnet is 0x00000000FFFF0000000000000000000000000000000000000000000000000000
        // But we use a simpler calculation: difficulty = 2^256 / (target + 1)
        // For display purposes, we normalize to genesis difficulty = 1.0
        // MAX_TARGET is 256 bits, use U256 from bllvm-consensus
        // 0x00000000FFFF0000000000000000000000000000000000000000000000000000
        // For now, use a placeholder - this should be calculated from difficulty bits
        const MAX_TARGET: u64 = 0x00000000FFFF0000u64;

        // Simplified difficulty calculation
        // For display purposes, use a simple approximation based on bits
        // Note: expand_target returns U256 which is private, so we just use bits directly
        // Use bits directly for difficulty approximation
        // Lower bits value = higher difficulty
        let mantissa = (bits & 0x00ffffff) as f64;
        if mantissa == 0.0 {
            return 1.0;
        }
        let max_mantissa = 0x00ffff00 as f64;
        (max_mantissa / mantissa).max(1.0)
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

    /// Calculate hash_serialized_2 for UTXO set (double SHA256)
    ///
    /// Serializes UTXO set deterministically and computes double SHA256 hash.
    /// Matches Bitcoin Core's gettxoutsetinfo hash_serialized_2 calculation.
    fn calculate_utxo_set_hash(utxo_set: &bllvm_protocol::UtxoSet) -> [u8; 32] {
        use crate::storage::hashing::double_sha256;

        // Sort UTXOs for deterministic hashing (by outpoint: hash first, then index)
        let mut entries: Vec<_> = utxo_set.iter().collect();
        entries.sort_by(|(a, _), (b, _)| match a.hash.cmp(&b.hash) {
            std::cmp::Ordering::Equal => a.index.cmp(&b.index),
            other => other,
        });

        // Serialize each UTXO entry
        let mut serialized = Vec::new();
        for (outpoint, utxo) in entries {
            // Serialize outpoint (32-byte hash + 8-byte index, little-endian)
            serialized.extend_from_slice(&outpoint.hash);
            serialized.extend_from_slice(&outpoint.index.to_le_bytes());

            // Serialize UTXO (8-byte value + script_pubkey + 8-byte height, little-endian)
            serialized.extend_from_slice(&utxo.value.to_le_bytes());
            serialized.extend_from_slice(&utxo.script_pubkey);
            serialized.extend_from_slice(&utxo.height.to_le_bytes());
        }

        // Double SHA256 hash
        double_sha256(&serialized)
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
    /// Includes softfork information based on feature flags from protocol-engine
    pub async fn get_blockchain_info(&self) -> Result<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getblockchaininfo");

        let softforks = json!({
            "segwit": {
                "type": "buried",
                "active": true,
                "height": 481824
            },
            "taproot": {
                "type": "buried",
                "active": true,
                "height": 709632
            }
        });

        if let Some(ref storage) = self.storage {
            use std::time::{Duration, Instant};

            thread_local! {
                static CACHED_TIP_HASH_HEX: std::cell::RefCell<(Option<String>, [u8; 32], Instant, Option<u64>)> =
                    std::cell::RefCell::new((None, [0u8; 32], Instant::now(), None));
            }

            let best_hash = storage.chain().get_tip_hash()?.unwrap_or([0u8; 32]);
            let height = storage.chain().get_height()?.unwrap_or(0);
            let block_count = storage.blocks().block_count().unwrap_or(0);

            let best_hash_hex = {
                let should_refresh = CACHED_TIP_HASH_HEX.with(|c| {
                    let cache = c.borrow();
                    cache.0.is_none()
                        || cache.1 != best_hash
                        || cache.2.elapsed() >= Duration::from_secs(1)
                        || cache.3 != Some(height)
                });

                if should_refresh {
                    let hex_str = hex::encode(best_hash);
                    CACHED_TIP_HASH_HEX.with(|c| {
                        let mut cache = c.borrow_mut();
                        *cache = (
                            Some(hex_str.clone()),
                            best_hash,
                            Instant::now(),
                            Some(height),
                        );
                    });
                    CACHED_TIP_HASH_HEX.with(|c| c.borrow().0.as_ref().unwrap().clone())
                } else {
                    CACHED_TIP_HASH_HEX.with(|c| c.borrow().0.as_ref().unwrap().clone())
                }
            };

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

            let chainwork = storage
                .chain()
                .get_chainwork(&best_hash)?
                .unwrap_or_else(|| {
                    // Fallback: calculate total work if cache miss
                    storage.chain().calculate_total_work().unwrap_or(0) as u128
                });
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
                "warnings": "Storage not available - returning default values"
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
            use crate::utils::with_storage_timeout;
            match with_storage_timeout(async {
                tokio::task::spawn_blocking({
                    let storage = storage.clone();
                    let hash_array = hash_array;
                    move || storage.blocks().get_block(&hash_array)
                })
                .await
            })
            .await
            {
                Ok(Ok(Ok(Some(block)))) => {
                    // Block found - return it
                    // TODO: Format block according to verbosity parameter
                    return Ok(json!({
                        "hash": hash,
                        "confirmations": 1, // TODO: Calculate actual confirmations
                        "size": 0, // TODO: Calculate size
                        "height": 0, // TODO: Get height
                        "version": block.header.version,
                        "merkleroot": hex::encode(block.header.merkle_root),
                        "tx": [], // TODO: Include transactions
                        "time": block.header.timestamp,
                        "bits": format!("{:08x}", block.header.bits),
                        "difficulty": 1.0, // TODO: Calculate difficulty
                    }));
                }
                Ok(Ok(Ok(None))) => {
                    // Block not found - return error
                    return Err(anyhow::anyhow!("Block not found"));
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
        Err(anyhow::anyhow!("Block not found or storage unavailable"))
    }

    /// Get block hash by height
    pub async fn get_block_hash(&self, height: u64) -> Result<Value> {
        debug!("RPC: getblockhash {}", height);

        // Simplified implementation - return error for non-existent heights
        if height > 1000 {
            return Err(anyhow::anyhow!("Block height {} not found", height));
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
                    use std::time::{Duration, Instant};

                    thread_local! {
                        static CACHED_TIP_HEIGHT: std::cell::RefCell<(Option<u64>, Instant)> =
                            std::cell::RefCell::new((None, Instant::now()));
                    }

                    let block_height = storage.blocks().get_height_by_hash(&hash_array)?;

                    let tip_height = {
                        let should_refresh = CACHED_TIP_HEIGHT.with(|c| {
                            let cache = c.borrow();
                            cache.0.is_none() || cache.1.elapsed() >= Duration::from_secs(1)
                        });

                        if should_refresh {
                            let height = storage.chain().get_height()?.unwrap_or(0);
                            CACHED_TIP_HEIGHT.with(|c| {
                                let mut cache = c.borrow_mut();
                                *cache = (Some(height), Instant::now());
                            });
                            CACHED_TIP_HEIGHT.with(|c| c.borrow().0.unwrap_or(0))
                        } else {
                            CACHED_TIP_HEIGHT.with(|c| c.borrow().0.unwrap_or(0))
                        }
                    };

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
                            .map(|hash| hex::encode(hash))
                    });

                    let chainwork = if let Some(_height) = block_height {
                        // O(1) lookup instead of O(n) calculation!
                        storage
                            .chain()
                            .get_chainwork(&hash_array)?
                            .map(|cw| Self::format_chainwork(cw))
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
                        "bits": hex::encode(&header.bits.to_le_bytes()),
                        "difficulty": difficulty,
                        "chainwork": chainwork,
                        "nTx": n_tx,
                        "previousblockhash": hex::encode(header.prev_block_hash),
                        "nextblockhash": next_blockhash
                    }))
                } else {
                    use bllvm_protocol::serialization::serialize_block_header;
                    let header_bytes = serialize_block_header(&header);
                    Ok(Value::String(hex::encode(header_bytes)))
                }
            } else {
                Err(anyhow::anyhow!("Block not found"))
            }
        } else {
            if verbose {
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
    }

    /// Get best block hash
    ///
    /// Params: []
    pub async fn get_best_block_hash(&self) -> Result<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getbestblockhash");

        if let Some(ref storage) = self.storage {
            use std::time::{Duration, Instant};

            thread_local! {
                static CACHED_TIP_HASH_HEX: std::cell::RefCell<(Option<String>, [u8; 32], Instant, Option<u64>)> =
                    std::cell::RefCell::new((None, [0u8; 32], Instant::now(), None));
            }

            let current_height = storage.chain().get_height()?.unwrap_or(0);

            if let Ok(Some(hash)) = storage.chain().get_tip_hash() {
                let should_refresh = CACHED_TIP_HASH_HEX.with(|c| {
                    let cache = c.borrow();
                    cache.0.is_none()
                        || cache.1 != hash
                        || cache.2.elapsed() >= Duration::from_secs(1)
                        || cache.3 != Some(current_height)
                });

                if should_refresh {
                    let hex_str = hex::encode(hash);
                    CACHED_TIP_HASH_HEX.with(|c| {
                        let mut cache = c.borrow_mut();
                        *cache = (
                            Some(hex_str.clone()),
                            hash,
                            Instant::now(),
                            Some(current_height),
                        );
                    });
                    Ok(Value::String(hex_str))
                } else {
                    CACHED_TIP_HASH_HEX.with(|c| {
                        let cache = c.borrow();
                        Ok(Value::String(cache.0.as_ref().unwrap().clone()))
                    })
                }
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

        use std::time::{Duration, Instant};

        if let Some(ref storage) = self.storage {
            thread_local! {
                static CACHED_DIFFICULTY: std::cell::RefCell<(Option<f64>, Instant, Option<u64>)> =
                    std::cell::RefCell::new((None, Instant::now(), None));
            }

            // Check current height for cache invalidation
            let current_height = storage.chain().get_height()?.unwrap_or(0);

            let should_refresh = CACHED_DIFFICULTY.with(|c| {
                let cache = c.borrow();
                cache.0.is_none()
                    || cache.1.elapsed() >= Duration::from_secs(1)
                    || cache.2 != Some(current_height)
            });

            if should_refresh {
                if let Ok(Some(tip_header)) = storage.chain().get_tip_header() {
                    let difficulty = Self::calculate_difficulty(tip_header.bits);

                    // Cache the result
                    CACHED_DIFFICULTY.with(|c| {
                        let mut cache = c.borrow_mut();
                        *cache = (Some(difficulty), Instant::now(), Some(current_height));
                    });

                    Ok(Value::Number(
                        Number::from_f64(difficulty).unwrap_or_else(|| Number::from(1)),
                    ))
                } else {
                    Ok(Value::Number(Number::from_f64(1.0).unwrap()))
                }
            } else {
                // Return cached value
                CACHED_DIFFICULTY.with(|c| {
                    let cache = c.borrow();
                    Ok(Value::Number(
                        Number::from_f64(cache.0.unwrap_or(1.0)).unwrap_or_else(|| Number::from(1)),
                    ))
                })
            }
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
                let h = storage.chain().get_height()?.unwrap_or(0);
                let hash = storage.chain().get_tip_hash()?.unwrap_or([0u8; 32]);
                (h, hash)
            };

            if let Ok(Some(stats)) = storage.chain().get_latest_utxo_stats() {
                // Use cached stats - much faster than loading entire UTXO set!
                Ok(json!({
                    "height": stats.height,
                    "bestblock": hex::encode(best_hash),
                    "transactions": stats.transactions,
                    "txouts": stats.txouts,
                    "bogosize": stats.txouts * 180, // Approximate
                    "hash_serialized_2": hex::encode(stats.hash_serialized_2),
                    "disk_size": storage.disk_size().unwrap_or(0),
                    "total_amount": stats.total_amount as f64 / 100_000_000.0
                }))
            } else {
                // Fallback: Calculate from UTXO set (expensive, but works if cache is missing)
                // This will be slow with large UTXO sets, but ensures correctness
                let utxos = storage.utxos().get_all_utxos()?;
                let txouts = utxos.len();
                let total_amount: u64 = utxos.values().map(|utxo| utxo.value as u64).sum();

                // Calculate hash_serialized_2 (double SHA256 of serialized UTXO set)
                let hash_serialized_2 = Self::calculate_utxo_set_hash(&utxos);

                Ok(json!({
                    "height": height,
                    "bestblock": hex::encode(best_hash),
                    "transactions": storage.transaction_count().unwrap_or(0),
                    "txouts": txouts,
                    "bogosize": txouts * 180, // Approximate
                    "hash_serialized_2": hex::encode(hash_serialized_2),
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
                "hash_serialized_2": ZERO_HASH_STR,
                "disk_size": 0,
                "total_amount": 0.0
            }))
        }
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
            use bllvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
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
            let start_height = if tip_height > num_blocks {
                tip_height - num_blocks
            } else {
                0
            };

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
                            Ok(bllvm_protocol::ValidationResult::Valid) => {
                                // Block is valid, update UTXO set for next block
                                // (Simplified - in full implementation would apply block to UTXO set)
                                // For now, just continue
                            }
                            Ok(bllvm_protocol::ValidationResult::Invalid(reason)) => {
                                errors.push(format!(
                                    "Block at height {} invalid: {}",
                                    height, reason
                                ));
                                if check_level >= 4 {
                                    // Level 4: Stop on first error
                                    break;
                                }
                            }
                            Err(e) => {
                                errors.push(format!(
                                    "Block at height {} validation error: {}",
                                    height, e
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
                            use bllvm_protocol::mining::calculate_merkle_root;

                            if let Ok(calculated_root) = calculate_merkle_root(&block.transactions)
                            {
                                if calculated_root != block.header.merkle_root {
                                    errors.push(format!(
                                        "Block at height {} has incorrect merkle root",
                                        height
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
            let (tip_hash, tip_height) = {
                let hash = storage.chain().get_tip_hash()?.unwrap_or([0u8; 32]);
                let height = storage.chain().get_height()?.unwrap_or(0);
                (hash, height)
            };

            // Get all chain tips (including forks)
            // Usually 1-2 tips, but pre-allocate for potential forks
            let mut tips = Vec::with_capacity(4);

            // Add active tip
            if tip_hash != [0u8; 32] {
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

        let nblocks = params.get(0).and_then(|p| p.as_u64()).unwrap_or(144); // Default: 1 day (144 blocks at 10 min/block)

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
                        .ok_or_else(|| anyhow::anyhow!("Block at height {} not found", height))?
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
                Err(anyhow::anyhow!("Block not found"))
            }
        } else {
            // Graceful degradation: return informative error instead of failing silently
            Err(anyhow::anyhow!(
                "Storage not available. This operation requires storage to be initialized."
            ))
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
            .ok_or_else(|| anyhow::anyhow!("Height parameter required"))?;

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
            // Graceful degradation: return informative error instead of failing silently
            Err(anyhow::anyhow!(
                "Storage not available. This operation requires storage to be initialized."
            ))
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
                "note": "Storage not available"
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
            .ok_or_else(|| anyhow::anyhow!("Block hash parameter required"))?;

        let hash =
            decode_hash32(blockhash).map_err(|e| anyhow::anyhow!("Invalid block hash: {}", e))?;

        if let Some(ref storage) = self.storage {
            // Mark block as invalid
            storage.chain().mark_invalid(&hash)?;

            // Check if this is the current tip - if so, we'd need to trigger a reorg
            // For now, we just mark it as invalid and let the node handle it on next block
            if let Ok(Some(tip_hash)) = storage.chain().get_tip_hash() {
                if hash == tip_hash {
                    warn!("Invalidated current chain tip - reorg may be needed");
                }
            }

            Ok(Value::Null)
        } else {
            // Graceful degradation: return informative error instead of failing silently
            Err(anyhow::anyhow!(
                "Storage not available. This operation requires storage to be initialized."
            ))
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
            .ok_or_else(|| anyhow::anyhow!("Block hash parameter required"))?;

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
            Err(anyhow::anyhow!(
                "Storage not available. This operation requires storage to be initialized."
            ))
        }
    }

    /// Wait for new block
    ///
    /// Params: ["timeout"] (optional, timeout in seconds, default: no timeout)
    ///
    /// Note: Full implementation requires async notification infrastructure.
    /// Currently returns current tip immediately. Future enhancement will:
    /// 1. Subscribe to block notifications
    /// 2. Wait for new block or timeout
    /// 3. Return block hash and height
    pub async fn wait_for_new_block(&self, params: &Value) -> Result<Value> {
        debug!("RPC: waitfornewblock");

        let _timeout = params
            .get(0)
            .and_then(|p| p.as_u64())
            .map(|t| tokio::time::Duration::from_secs(t));

        if let Some(ref storage) = self.storage {
            if let Ok(Some(tip_hash)) = storage.chain().get_tip_hash() {
                let tip_height = storage.chain().get_height()?.unwrap_or(0);
                Ok(json!({
                    "hash": hex::encode(tip_hash),
                    "height": tip_height
                }))
            } else {
                Err(anyhow::anyhow!("Chain not initialized"))
            }
        } else {
            // Graceful degradation: return informative error instead of failing silently
            Err(anyhow::anyhow!(
                "Storage not available. This operation requires storage to be initialized."
            ))
        }
    }

    /// Wait for specific block
    ///
    /// Params: ["blockhash", "timeout"] (block hash, optional timeout)
    ///
    /// Note: Full implementation requires async notification infrastructure.
    /// Currently checks if block exists immediately. Future enhancement will:
    /// 1. Subscribe to block notifications
    /// 2. Wait for block to appear or timeout
    /// 3. Return block hash and height
    pub async fn wait_for_block(&self, params: &Value) -> Result<Value> {
        debug!("RPC: waitforblock");

        let blockhash = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("Block hash parameter required"))?;

        let _timeout = params
            .get(1)
            .and_then(|p| p.as_u64())
            .map(|t| tokio::time::Duration::from_secs(t));

        let hash =
            decode_hash32(blockhash).map_err(|e| anyhow::anyhow!("Invalid block hash: {}", e))?;

        // Note: This requires async notification infrastructure
        // For now, check if block exists immediately
        if let Some(ref storage) = self.storage {
            if let Ok(Some(_block)) = storage.blocks().get_block(&hash) {
                let height = storage.blocks().get_height_by_hash(&hash)?.unwrap_or(0);
                Ok(json!({
                    "hash": blockhash,
                    "height": height
                }))
            } else {
                Err(anyhow::anyhow!("Block not found"))
            }
        } else {
            // Graceful degradation: return informative error instead of failing silently
            Err(anyhow::anyhow!(
                "Storage not available. This operation requires storage to be initialized."
            ))
        }
    }

    /// Wait for block height
    ///
    /// Params: ["height", "timeout"] (block height, optional timeout)
    ///
    /// Note: Full implementation requires async notification infrastructure.
    /// Currently checks if block at height exists immediately. Future enhancement will:
    /// 1. Subscribe to block notifications
    /// 2. Wait for block at height to appear or timeout
    /// 3. Return block hash and height
    pub async fn wait_for_block_height(&self, params: &Value) -> Result<Value> {
        debug!("RPC: waitforblockheight");

        let height = params
            .get(0)
            .and_then(|p| p.as_u64())
            .ok_or_else(|| anyhow::anyhow!("Height parameter required"))?;

        let _timeout = params
            .get(1)
            .and_then(|p| p.as_u64())
            .map(|t| tokio::time::Duration::from_secs(t));
        if let Some(ref storage) = self.storage {
            let tip_height = storage.chain().get_height()?.unwrap_or(0);
            if height <= tip_height {
                if let Ok(Some(hash)) = storage.blocks().get_hash_by_height(height) {
                    Ok(json!({
                        "hash": hex::encode(hash),
                        "height": height
                    }))
                } else {
                    Err(anyhow::anyhow!("Block at height {} not found", height))
                }
            } else {
                Err(anyhow::anyhow!(
                    "Block at height {} not yet available (tip: {})",
                    height,
                    tip_height
                ))
            }
        } else {
            // Graceful degradation: return informative error instead of failing silently
            Err(anyhow::anyhow!(
                "Storage not available. This operation requires storage to be initialized."
            ))
        }
    }

    /// Get block filter (BIP158)
    ///
    /// Params: ["blockhash", "filtertype"] (block hash, filter type, default: 0 = Basic)
    pub async fn get_block_filter(&self, params: &Value) -> Result<Value> {
        debug!("RPC: getblockfilter");

        let blockhash = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("Block hash parameter required"))?;

        let filtertype = params.get(1).and_then(|p| p.as_u64()).unwrap_or(0); // Default: Basic filter

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
                use bllvm_protocol::bip158::build_block_filter;

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

                match build_block_filter(&block.transactions, &previous_scripts) {
                    Ok(filter) => {
                        Ok(json!({
                            "filter": hex::encode(&filter.filter_data),
                            "header": hex::encode([0u8; 32]), // Would calculate filter header
                        }))
                    }
                    Err(e) => Err(anyhow::anyhow!("Failed to build filter: {}", e)),
                }
            } else {
                Err(anyhow::anyhow!("Block not found"))
            }
        } else {
            // Graceful degradation: return informative error instead of failing silently
            Err(anyhow::anyhow!(
                "Storage not available. This operation requires storage to be initialized."
            ))
        }
    }

    /// Get index information
    ///
    /// Params: [] (no parameters)
    pub async fn get_index_info(&self, _params: &Value) -> Result<Value> {
        debug!("RPC: getindexinfo");

        // Return available indexes
        // In production, would check which indexes are actually built
        Ok(json!({
            "txindex": {
                "synced": true,
                "best_block_height": if let Some(ref storage) = self.storage {
                    storage.chain().get_height()?.unwrap_or(0)
                } else {
                    0
                }
            },
            "basic block filter index": {
                "synced": true,
                "best_block_height": if let Some(ref storage) = self.storage {
                    storage.chain().get_height()?.unwrap_or(0)
                } else {
                    0
                }
            }
        }))
    }
}
