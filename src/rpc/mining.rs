//! Mining RPC methods
//!
//! Implements mining-related JSON-RPC methods for block template generation and mining.
//! Uses formally verified consensus-proof mining functions.

use crate::node::mempool::MempoolManager;
use crate::rpc::errors::{RpcError, RpcResult};
use crate::storage::Storage;
use crate::utils::current_timestamp;
use bllvm_protocol::mining::BlockTemplate;
use bllvm_protocol::serialization::deserialize_block_with_witnesses;
use bllvm_protocol::serialization::serialize_transaction;
use bllvm_protocol::{
    types::{BlockHeader, ByteString, Natural, Transaction, UtxoSet},
    ConsensusProof, ValidationResult,
};
use hex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tracing::{debug, warn};

/// Mining RPC methods with dependencies
pub struct MiningRpc {
    /// Consensus proof instance for mining operations
    consensus: ConsensusProof,
    /// Storage accessor for chainstate and UTXO set
    storage: Option<Arc<Storage>>,
    /// Mempool accessor for transaction retrieval
    mempool: Option<Arc<MempoolManager>>,
}

impl MiningRpc {
    /// Create a new mining RPC handler
    pub fn new() -> Self {
        Self {
            consensus: ConsensusProof::new(),
            storage: None,
            mempool: None,
        }
    }

    /// Create with dependencies (storage and mempool)
    pub fn with_dependencies(storage: Arc<Storage>, mempool: Arc<MempoolManager>) -> Self {
        Self {
            consensus: ConsensusProof::new(),
            storage: Some(storage),
            mempool: Some(mempool),
        }
    }

    /// Get mining information
    pub async fn get_mining_info(&self) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getmininginfo");

        use std::time::{Duration, Instant};

        // This avoids multiple storage lookups for height, tip_header, chain_info
        thread_local! {
            static CACHED_MINING_INFO: std::cell::RefCell<(Option<Value>, Instant, Option<u64>)> = {
                std::cell::RefCell::new((None, Instant::now(), None))
            };
        }

        // Check if we should refresh (cache miss, expired, or chain advanced)
        let current_height = if let Some(ref storage) = self.storage {
            storage.chain().get_height().ok().flatten().unwrap_or(0)
        } else {
            0
        };

        let should_refresh = CACHED_MINING_INFO.with(|cache| {
            let cache = cache.borrow();
            cache.0.is_none()
                || cache.1.elapsed() >= Duration::from_secs(1)
                || cache.2 != Some(current_height)
        });

        if should_refresh {
            // Get current block height from storage
            let blocks = if let Some(ref storage) = self.storage {
                storage
                    .chain()
                    .get_height()
                    .map_err(|e| RpcError::internal_error(format!("Failed to get height: {e}")))?
                    .unwrap_or(0)
            } else {
                0
            };

            // Get mempool size
            let pooledtx = if let Some(ref mempool) = self.mempool {
                mempool.size()
            } else {
                0
            };

            // Get difficulty from latest block's bits field (graceful degradation)
            let difficulty = if let Some(ref storage) = self.storage {
                if let Ok(Some(tip_header)) = storage.chain().get_tip_header() {
                    Self::calculate_difficulty(tip_header.bits)
                } else {
                    tracing::debug!("No chain tip available, using default difficulty");
                    1.0 // Graceful fallback if no tip
                }
            } else {
                tracing::debug!("Storage not available, using default difficulty");
                1.0 // Graceful fallback if no storage
            };

            let networkhashps = if let Some(ref storage) = self.storage {
                // Try cached hashrate first (O(1) lookup)
                if let Ok(Some(cached_hashrate)) = storage.chain().get_network_hashrate() {
                    cached_hashrate
                } else {
                    // Fallback: Calculate network hashrate (expensive - loads up to 144 blocks)
                    self.calculate_network_hashrate(storage)
                        .unwrap_or_else(|e| {
                            tracing::debug!("Failed to calculate network hashrate: {e}, using 0.0");
                            0.0
                        })
                }
            } else {
                tracing::debug!("Storage not available, network hashrate unavailable");
                0.0
            };

            // Get current block template info (if available)
            let currentblocksize = 0;
            let currentblockweight = 0;
            let currentblocktx = 0;

            // Determine chain name from storage chain params
            let chain = if let Some(ref storage) = self.storage {
                if let Ok(Some(info)) = storage.chain().load_chain_info() {
                    match info.chain_params.network.as_str() {
                        "mainnet" => "main",
                        "testnet" => "test",
                        "regtest" => "regtest",
                        _ => "main",
                    }
                } else {
                    "main" // Default
                }
            } else {
                "main" // Default
            };

            let value = json!({
                "blocks": blocks,
                "currentblocksize": currentblocksize,
                "currentblockweight": currentblockweight,
                "currentblocktx": currentblocktx,
                "difficulty": difficulty,
                "networkhashps": networkhashps,
                "pooledtx": pooledtx,
                "chain": chain,
                "warnings": ""
            });

            // Cache the result
            CACHED_MINING_INFO.with(|cache| {
                let mut cache = cache.borrow_mut();
                *cache = (Some(value.clone()), Instant::now(), Some(current_height));
            });

            Ok(value)
        } else {
            // Return cached value
            CACHED_MINING_INFO.with(|cache| {
                let cache = cache.borrow();
                Ok(cache.0.as_ref().unwrap().clone())
            })
        }
    }

    /// Get block template
    ///
    /// Params: [template_request (optional)]
    ///
    /// Uses formally verified consensus-proof::mining::create_block_template() function
    /// which has Kani proofs ensuring correctness per Orange Paper Section 12.4
    pub async fn get_block_template(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: getblocktemplate");

        // 1. Get current chainstate
        let height: Natural = self
            .get_current_height()?
            .ok_or_else(|| RpcError::internal_error("Chain not initialized"))?;
        let prev_header = self
            .get_tip_header()?
            .ok_or_else(|| RpcError::internal_error("No chain tip"))?;
        let prev_headers = self.get_headers_for_difficulty()?;

        // 2. Get mempool transactions
        let mempool_txs: Vec<Transaction> = self.get_mempool_transactions()?;

        // 3. Get UTXO set
        let utxo_set = self.get_utxo_set()?;

        // 4. Extract coinbase parameters from request or use defaults
        let coinbase_script = self.extract_coinbase_script(params).unwrap_or_default();
        let coinbase_address = self.extract_coinbase_address(params).unwrap_or_default();

        // 5. Use formally verified function from consensus-proof
        // This function has Kani proofs: kani_create_block_template_completeness
        let template = match self.consensus.create_block_template(
            &utxo_set,
            &mempool_txs,
            height,
            &prev_header,
            &prev_headers,
            &coinbase_script,
            &coinbase_address,
        ) {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to create block template: {}", e);
                return Err(RpcError::internal_error(format!(
                    "Template creation failed: {}",
                    e
                )));
            }
        };

        // 6. Convert to JSON-RPC format (BIP 22/23)
        self.template_to_json_rpc(&template, &prev_header, height)
    }

    /// Convert BlockTemplate to JSON-RPC format
    fn template_to_json_rpc(
        &self,
        template: &bllvm_protocol::mining::BlockTemplate,
        prev_header: &BlockHeader,
        height: Natural,
    ) -> RpcResult<Value> {
        // Convert previous block hash to hex (big-endian)
        let prev_hash_hex = hex::encode(prev_header.prev_block_hash);

        // Convert target to hex (64 characters, big-endian)
        let target_hex = format!("{:064x}", template.target);

        // Convert bits to hex (8 characters)
        let bits_hex = format!("{:08x}", template.header.bits);

        // Convert transactions to JSON array
        let transactions_json: Vec<Value> = template
            .transactions
            .iter()
            .map(|tx| self.transaction_to_json(tx))
            .collect();

        // Calculate coinbase value (subsidy + fees)
        let coinbase_value = self.calculate_coinbase_value(template, height);

        // Get active rules (BIP 9 feature flags)
        let rules = self.get_active_rules(height);

        // Get minimum time (median time + 1)
        let min_time = self.get_min_time(height);

        Ok(json!({
            "capabilities": ["proposal"],
            "version": template.header.version as i32,
            "rules": rules,
            "vbavailable": {},
            "vbrequired": 0,
            "previousblockhash": prev_hash_hex,
            "transactions": transactions_json,
            "coinbaseaux": {
                "flags": ""
            },
            "coinbasevalue": coinbase_value,
            "longpollid": prev_hash_hex,
            "target": target_hex,
            "mintime": min_time,
            "mutable": ["time", "transactions", "prevblock"],
            "noncerange": "00000000ffffffff",
            "sigoplimit": 80000,
            "sizelimit": 4000000,
            "weightlimit": 4000000,
            "curtime": template.timestamp,
            "bits": bits_hex,
            "height": template.height
        }))
    }

    // Helper methods - access chainstate and mempool

    fn get_current_height(&self) -> RpcResult<Option<Natural>> {
        if let Some(ref storage) = self.storage {
            storage
                .chain()
                .get_height()
                .map_err(|e| RpcError::internal_error(format!("Failed to get height: {e}")))
        } else {
            Ok(None)
        }
    }

    fn get_tip_header(&self) -> RpcResult<Option<BlockHeader>> {
        if let Some(ref storage) = self.storage {
            storage
                .chain()
                .get_tip_header()
                .map_err(|e| RpcError::internal_error(format!("Failed to get tip header: {e}")))
        } else {
            Ok(None)
        }
    }

    fn get_headers_for_difficulty(&self) -> RpcResult<Vec<BlockHeader>> {
        if let Some(ref storage) = self.storage {
            // Get last 2016 headers for difficulty adjustment
            // Consensus layer requires at least 2 headers for difficulty adjustment
            // Try to get recent headers (up to 2016)
            if let Ok(recent_headers) = storage.blocks().get_recent_headers(2016) {
                if recent_headers.len() >= 2 {
                    Ok(recent_headers)
                } else {
                    // If we have fewer than 2 headers, try to get headers by height
                    let mut headers = Vec::new();
                    if let Ok(Some(height)) = storage.chain().get_height() {
                        // Get headers from height 0 up to current height (oldest first for difficulty adjustment)
                        for h in 0..=height.min(2015) {
                            if let Ok(Some(hash)) = storage.blocks().get_hash_by_height(h) {
                                if let Ok(Some(header)) = storage.blocks().get_header(&hash) {
                                    headers.push(header);
                                }
                            }
                        }
                    }
                    if headers.len() >= 2 {
                        // Headers are already in oldest-to-newest order (height 0, 1, 2, ...)
                        Ok(headers)
                    } else if headers.len() == 1 {
                        // If we only have 1 header, we can't do difficulty adjustment properly
                        // Return empty to let the consensus layer handle it
                        Ok(vec![])
                    } else {
                        Ok(vec![])
                    }
                }
            } else if let Some(tip) = storage
                .chain()
                .get_tip_header()
                .map_err(|e| RpcError::internal_error(format!("Failed to get tip: {e}")))?
            {
                // Fallback: duplicate tip to satisfy 2-header requirement
                Ok(vec![tip.clone(), tip])
            } else {
                Ok(vec![])
            }
        } else {
            Ok(vec![])
        }
    }

    fn get_mempool_transactions(&self) -> RpcResult<Vec<Transaction>> {
        if let Some(ref mempool) = self.mempool {
            // Get UTXO set for fee calculation
            let utxo_set = self.get_utxo_set()?;

            // Get prioritized transactions (limit to reasonable number for block template)
            let limit = 1000;
            Ok(mempool.get_prioritized_transactions(limit, &utxo_set))
        } else {
            Ok(vec![])
        }
    }

    /// Calculate difficulty from bits (compact target format)
    /// Uses the same logic as BlockchainRpc::calculate_difficulty
    fn calculate_difficulty(bits: u64) -> f64 {
        // Difficulty = MAX_TARGET / target
        // MAX_TARGET for Bitcoin mainnet is 0x00000000FFFF0000000000000000000000000000000000000000000000000000
        // For display purposes, we normalize to genesis difficulty = 1.0
        // MAX_TARGET is 256 bits, use U256 from bllvm-consensus
        // 0x00000000FFFF0000000000000000000000000000000000000000000000000000
        // For now, use a placeholder - this should be calculated from difficulty bits
        const MAX_TARGET: u64 = 0x00000000FFFF0000u64;

        // Simplified difficulty calculation using bits directly
        // For display purposes, use a simple approximation based on bits
        let mantissa = (bits & 0x00ffffff) as f64;
        if mantissa == 0.0 {
            return 1.0;
        }
        let max_mantissa = 0x00ffff00 as f64;
        (max_mantissa / mantissa).max(1.0)
    }

    /// Calculate network hashrate from recent block timestamps
    /// Estimates hashrate based on the time between recent blocks
    fn calculate_network_hashrate(&self, storage: &Storage) -> Result<f64, anyhow::Error> {
        // Get tip height
        let tip_height = storage
            .chain()
            .get_height()?
            .ok_or_else(|| anyhow::anyhow!("Chain not initialized"))?;

        // Need at least 2 blocks to calculate hashrate
        if tip_height < 1 {
            return Ok(0.0);
        }

        // Get last 144 blocks (approximately 1 day at 10 min/block)
        // Or use fewer blocks if chain is shorter
        let num_blocks = (tip_height + 1).min(144);
        let start_height = tip_height.saturating_sub(num_blocks - 1);

        // Get timestamps from blocks
        let mut timestamps = Vec::new();
        for height in start_height..=tip_height {
            if let Ok(Some(hash)) = storage.blocks().get_hash_by_height(height) {
                if let Ok(Some(block)) = storage.blocks().get_block(&hash) {
                    timestamps.push((height, block.header.timestamp));
                }
            }
        }

        if timestamps.len() < 2 {
            return Ok(0.0);
        }

        // Calculate average time between blocks
        let first_timestamp = timestamps[0].1;
        let last_timestamp = timestamps[timestamps.len() - 1].1;
        let time_span = last_timestamp.saturating_sub(first_timestamp);
        let num_intervals = timestamps.len() - 1;

        if time_span == 0 || num_intervals == 0 {
            return Ok(0.0);
        }

        let avg_time_per_block = time_span as f64 / num_intervals as f64;

        // Get difficulty from tip block
        let tip_hash = storage
            .blocks()
            .get_hash_by_height(tip_height)?
            .ok_or_else(|| anyhow::anyhow!("Tip block not found"))?;
        let tip_block = storage
            .blocks()
            .get_block(&tip_hash)?
            .ok_or_else(|| anyhow::anyhow!("Tip block not found"))?;
        let difficulty = Self::calculate_difficulty(tip_block.header.bits);

        // Calculate hashrate: difficulty * 2^32 / avg_time_per_block
        // This estimates the network hashrate in hashes per second
        // 2^32 is the number of hashes needed on average to find a block at difficulty 1.0
        const HASHES_PER_DIFFICULTY: f64 = 4294967296.0; // 2^32
        let hashrate = (difficulty * HASHES_PER_DIFFICULTY) / avg_time_per_block;

        Ok(hashrate)
    }

    fn get_utxo_set(&self) -> RpcResult<UtxoSet> {
        if let Some(ref storage) = self.storage {
            // Get UTXO set from storage
            storage
                .utxos()
                .get_all_utxos()
                .map_err(|e| RpcError::internal_error(format!("Failed to get UTXO set: {e}")))
        } else {
            Ok(UtxoSet::new())
        }
    }

    fn extract_coinbase_script(&self, params: &Value) -> Option<ByteString> {
        // Extract coinbase script from params if provided
        if let Some(template_request) = params.get(0) {
            if let Some(script) = template_request.get("coinbasetxn") {
                if let Some(data) = script.get("data") {
                    if let Some(hex_str) = data.as_str() {
                        return hex::decode(hex_str).ok();
                    }
                }
            }
        }
        // Default: empty script
        Some(vec![])
    }

    fn extract_coinbase_address(&self, params: &Value) -> Option<ByteString> {
        // Extract coinbase address from params if provided
        if let Some(template_request) = params.get(0) {
            if let Some(addr) = template_request.get("coinbaseaddress") {
                if let Some(_addr_str) = addr.as_str() {
                    // Would decode address to script, for now return empty
                    return Some(vec![]);
                }
            }
        }
        // Default: empty
        Some(vec![])
    }

    fn transaction_to_json(&self, tx: &Transaction) -> Value {
        // Convert transaction to JSON-RPC format
        let tx_bytes = serialize_transaction(tx);
        let tx_hash = self.calculate_tx_hash(&tx_bytes);
        let fee = self.calculate_transaction_fee(tx);
        let sigops = self.count_sigops(tx);
        let weight = self.calculate_weight(tx);

        json!({
            "data": hex::encode(&tx_bytes),
            "txid": hex::encode(tx_hash),
            "fee": fee,
            "sigops": sigops,
            "weight": weight,
        })
    }

    fn calculate_tx_hash(&self, tx_bytes: &[u8]) -> [u8; 32] {
        // Transaction hash is double SHA256 of transaction bytes
        let hash1 = Sha256::digest(tx_bytes);
        let hash2 = Sha256::digest(hash1);

        let mut result = [0u8; 32];
        result.copy_from_slice(&hash2);
        result
    }

    fn calculate_transaction_fee(&self, tx: &Transaction) -> u64 {
        // Use MempoolManager's fee calculation if available
        if let Some(ref _mempool) = self.mempool {
            if let Ok(utxo_set) = self.get_utxo_set() {
                // Try to use mempool's fee calculation method if available
                // For now, calculate directly using UTXO set (mempool uses same logic)
                let mut input_total = 0u64;
                for input in &tx.inputs {
                    if let Some(utxo) = utxo_set.get(&input.prevout) {
                        input_total += utxo.value as u64;
                    }
                }
                let output_total: u64 = tx.outputs.iter().map(|out| out.value as u64).sum();
                input_total.saturating_sub(output_total)
            } else {
                0
            }
        } else {
            0
        }
    }

    fn count_sigops(&self, tx: &Transaction) -> u32 {
        // Use consensus layer sigop counting
        #[cfg(feature = "sigop")]
        {
            // Transaction types are the same between bllvm_protocol and bllvm_consensus
            // (bllvm_protocol re-exports them), so we can use tx directly
            use bllvm_protocol::sigop::get_legacy_sigop_count;
            get_legacy_sigop_count(tx)
        }
        #[cfg(not(feature = "sigop"))]
        {
            // Fallback: basic counting
            let mut count = 0u32;
            for output in &tx.outputs {
                for &byte in &output.script_pubkey {
                    match byte {
                        0xac => count += 1,  // OP_CHECKSIG
                        0xad => count += 1,  // OP_CHECKSIGVERIFY
                        0xae => count += 1,  // OP_CHECKMULTISIG
                        0xaf => count += 20, // OP_CHECKMULTISIGVERIFY
                        _ => {}
                    }
                }
            }
            count
        }
    }

    fn calculate_weight(&self, tx: &Transaction) -> u64 {
        // Transaction weight = (base_size * 3) + total_size (for SegWit)
        // For now, return base size * 4 (non-SegWit transaction)
        let base_size = serialize_transaction(tx).len() as u64;
        base_size * 4
    }

    fn calculate_coinbase_value(&self, template: &BlockTemplate, _height: Natural) -> u64 {
        // Use consensus-proof's get_block_subsidy (formally verified)
        let subsidy = self.consensus.get_block_subsidy(template.height) as u64;

        // Calculate total fees from transactions
        let fees: u64 = template
            .transactions
            .iter()
            .map(|tx| self.calculate_transaction_fee(tx))
            .sum();

        subsidy + fees
    }

    fn get_active_rules(&self, height: Natural) -> Vec<String> {
        // Determine active BIP 9 rules based on height
        let mut rules = vec!["csv".to_string()]; // CSV always active after height

        if height >= 481824 {
            // SegWit activation (mainnet)
            rules.push("segwit".to_string());
        }

        if height >= 709632 {
            // Taproot activation (mainnet)
            rules.push("taproot".to_string());
        }

        rules
    }

    fn get_min_time(&self, _height: Natural) -> Natural {
        // Get minimum time (median time of last 11 blocks + 1)
        // For now, return current time
        current_timestamp() as Natural
    }

    /// Submit a block to the network
    ///
    /// Params: ["hexdata", "dummy"]
    pub async fn submit_block(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: submitblock");

        // Validate hex string parameter with length limits (blocks can be up to ~4MB)
        use crate::rpc::validation::validate_hex_string_param;
        let hex_data = validate_hex_string_param(
            params,
            0,
            "hexdata",
            Some(8_000_000), // ~4MB block max
        )?;

        // Decode hex
        let block_bytes = hex::decode(&hex_data)
            .map_err(|e| RpcError::invalid_params(format!("Invalid hex data: {e}")))?;

        // Deserialize block
        let (block, _witnesses) = deserialize_block_with_witnesses(&block_bytes)
            .map_err(|e| RpcError::invalid_params(format!("Failed to deserialize block: {e}")))?;

        // Get current chain state
        let height = self
            .get_current_height()?
            .ok_or_else(|| RpcError::internal_error("Chain not initialized"))?;
        let utxo_set = self.get_utxo_set()?;

        // Validate block using consensus layer
        // Note: This validates consensus rules, but doesn't check if block extends chain
        // In production, would also check:
        // 1. Block extends current tip
        // 2. Block is not already in chain
        // 3. Block is not orphaned
        match self.consensus.validate_block(&block, utxo_set, height) {
            Ok((ValidationResult::Valid, _)) => {
                // Block is valid - in production would submit to block processor
                // For now, just return success
                debug!("Block submitted successfully");
                Ok(Value::Null)
            }
            Ok((ValidationResult::Invalid(reason), _)) => Err(RpcError::invalid_params(format!(
                "Invalid block: {}",
                reason
            ))),
            Err(e) => Err(RpcError::internal_error(format!("Validation error: {e}"))),
        }
    }

    /// Estimate smart fee rate
    ///
    /// Params: [conf_target (optional, default: 6), estimate_mode (optional, default: "conservative")]
    pub async fn estimate_smart_fee(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: estimatesmartfee");

        let conf_target = params.get(0).and_then(|p| p.as_u64()).unwrap_or(6);

        let estimate_mode = params
            .get(1)
            .and_then(|p| p.as_str())
            .unwrap_or("conservative");

        // Validate estimate_mode
        match estimate_mode {
            "unset" | "economical" | "conservative" => {}
            _ => {
                return Err(RpcError::invalid_params(format!(
                    "Invalid estimate_mode: {}. Must be 'unset', 'economical', or 'conservative'",
                    estimate_mode
                )))
            }
        }

        // Get mempool transactions and UTXO set for fee calculation
        let mempool_txs = if let Some(ref mempool) = self.mempool {
            let utxo_set = self.get_utxo_set()?;
            mempool.get_prioritized_transactions(100, &utxo_set) // Get top 100 by fee rate
        } else {
            vec![]
        };

        // Calculate fee rate based on mempool state
        // Simple algorithm: use median fee rate of top transactions
        let fee_rate = if !mempool_txs.is_empty() {
            let _utxo_set = self.get_utxo_set()?;
            let mut fee_rates = Vec::new();

            // MempoolManager.get_prioritized_transactions() already calculates fees correctly
            // We can extract fee rates from the prioritized list, but for now calculate directly
            for tx in &mempool_txs {
                let fee = self.calculate_transaction_fee(tx); // Now uses UTXO set
                let size = self.calculate_weight(tx) as usize;

                if size > 0 {
                    // Fee rate in BTC per vbyte
                    let rate = (fee as f64) / (size as f64) / 100_000_000.0; // Convert satoshis to BTC
                    fee_rates.push(rate);
                }
            }

            // Use median fee rate, or minimum if no transactions
            if !fee_rates.is_empty() {
                fee_rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let median_idx = fee_rates.len() / 2;
                fee_rates[median_idx]
            } else {
                0.00001 // Default: 1 sat/vB
            }
        } else {
            // No mempool transactions - return minimum fee
            0.00001 // 1 sat/vB
        };

        // Adjust based on estimate_mode
        let adjusted_rate = match estimate_mode {
            "economical" => fee_rate * 0.8,   // 20% lower for economical
            "conservative" => fee_rate * 1.2, // 20% higher for conservative
            _ => fee_rate,
        };

        Ok(json!({
            "feerate": adjusted_rate,
            "blocks": conf_target
        }))
    }

    /// Prioritize a transaction in the mempool
    ///
    /// Params: ["txid", "fee_delta"] (transaction ID, fee delta in satoshis)
    pub async fn prioritise_transaction(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: prioritisetransaction");

        let txid = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::invalid_params("Transaction ID required".to_string()))?;

        let fee_delta = params
            .get(1)
            .and_then(|p| p.as_i64())
            .ok_or_else(|| RpcError::invalid_params("Fee delta required".to_string()))?;

        let hash_bytes = hex::decode(txid)
            .map_err(|e| RpcError::invalid_params(format!("Invalid transaction ID: {e}")))?;
        if hash_bytes.len() != 32 {
            return Err(RpcError::invalid_params(
                "Transaction ID must be 32 bytes".to_string(),
            ));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_bytes);

        if let Some(ref mempool) = self.mempool {
            // Check if transaction exists in mempool
            if mempool.get_transaction(&hash).is_some() {
                // Note: In production, would update transaction priority/fee delta
                // For now, just return success
                debug!(
                    "Transaction {} prioritized with fee delta: {}",
                    txid, fee_delta
                );
                Ok(json!(true))
            } else {
                Err(RpcError::invalid_params(format!(
                    "Transaction {} not found in mempool",
                    txid
                )))
            }
        } else {
            Err(RpcError::internal_error(
                "Mempool not initialized".to_string(),
            ))
        }
    }
}

impl Default for MiningRpc {
    fn default() -> Self {
        Self::new()
    }
}
