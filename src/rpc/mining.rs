//! Mining RPC methods
//!
//! Implements mining-related JSON-RPC methods for block template generation and mining.
//! Uses formally verified blvm-consensus mining functions.

use crate::network::NetworkManager;
use crate::node::event_publisher::EventPublisher;
use crate::node::mempool::MempoolManager;
use crate::node::sync::SyncCoordinator;
use crate::rpc::errors::{RpcError, RpcResult, TIP_BLOCK_NOT_FOUND_MSG};
use crate::rpc::params::{param_str_required, param_u64_default, param_u64_required};
use crate::rpc::rawtx::address_string_to_script_pubkey;
use crate::storage::Storage;
use crate::utils::{current_timestamp, CACHE_REFRESH_TIP};
use blvm_protocol::mining::BlockTemplate;
use blvm_protocol::serialization::deserialize_block_with_witnesses;
use blvm_protocol::serialization::serialize_block_with_witnesses;
use blvm_protocol::serialization::serialize_transaction;
use blvm_protocol::segwit::Witness;
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use blvm_consensus::mining::MiningResult;
use blvm_consensus::opcodes::{OP_CHECKSIG, OP_CHECKSIGVERIFY, OP_CHECKMULTISIG, OP_CHECKMULTISIGVERIFY};
use blvm_consensus::types::Network as ConsensusNetwork;
use blvm_consensus::{
    BIP112_CSV_ACTIVATION_MAINNET, BIP112_CSV_ACTIVATION_REGTEST, BIP112_CSV_ACTIVATION_TESTNET,
    SEGWIT_ACTIVATION_MAINNET, SEGWIT_ACTIVATION_TESTNET, TAPROOT_ACTIVATION_MAINNET,
    TAPROOT_ACTIVATION_TESTNET,
};
use blvm_protocol::{
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
    /// Event publisher for BlockMined, BlockTemplateUpdated (optional)
    event_publisher: Option<Arc<EventPublisher>>,
    /// When set, `submitblock` queues wire bytes for the node run loop (same as a P2P block).
    network_manager: Option<Arc<NetworkManager>>,
    /// Protocol engine (required for `generatetoaddress` regtest mining).
    protocol_engine: Option<Arc<BitcoinProtocolEngine>>,
}

impl MiningRpc {
    /// Create a new mining RPC handler
    pub fn new() -> Self {
        Self {
            consensus: ConsensusProof::new(),
            storage: None,
            mempool: None,
            event_publisher: None,
            network_manager: None,
            protocol_engine: None,
        }
    }

    /// Create with dependencies (storage and mempool)
    pub fn with_dependencies(storage: Arc<Storage>, mempool: Arc<MempoolManager>) -> Self {
        Self {
            consensus: ConsensusProof::new(),
            storage: Some(storage),
            mempool: Some(mempool),
            event_publisher: None,
            network_manager: None,
            protocol_engine: None,
        }
    }

    /// Set event publisher for BlockMined, BlockTemplateUpdated
    pub fn with_event_publisher(mut self, event_publisher: Option<Arc<EventPublisher>>) -> Self {
        self.event_publisher = event_publisher;
        self
    }

    /// Attach P2P stack so `submitblock` can extend the local chain via the main run loop.
    pub fn with_network_manager(mut self, network_manager: Option<Arc<NetworkManager>>) -> Self {
        self.network_manager = network_manager;
        self
    }

    /// Attach protocol engine (needed for `generatetoaddress` on regtest).
    pub fn with_protocol_engine(mut self, engine: Arc<BitcoinProtocolEngine>) -> Self {
        self.protocol_engine = Some(engine);
        self
    }

    /// Get mining information
    pub async fn get_mining_info(&self) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getmininginfo");

        use std::time::Instant;

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
                || cache.1.elapsed() >= CACHE_REFRESH_TIP
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
    /// Uses formally verified blvm-consensus::mining::create_block_template() function
    /// which has spec-lock verification ensuring correctness per Orange Paper Section 12.4
    pub async fn get_block_template(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: getblocktemplate");

        // 1. Chain tip + height index for the *next* block to mine.
        // Must match `SyncCoordinator::process_block` / run-loop indexing: the first block
        // stored on a fresh datadir (genesis in chainstate only) uses height 0; then 1, 2, …
        // `chain_info.height` stays on the tip header's logical index — using it here was
        // one block behind after the first connect. Blockstore count matches the next index.
        let template_block_height: Natural = if let Some(ref storage) = self.storage {
            storage
                .blocks()
                .block_count()
                .map_err(|e| RpcError::internal_error(format!("Failed to count blocks: {e}")))?
                as u64
        } else {
            return Err(RpcError::internal_error(
                "getblocktemplate requires storage".to_string(),
            ));
        };
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

        // 5. Use formally verified function from blvm-consensus
        // This function has spec-lock verification for block template completeness
        let template = match self.consensus.create_block_template(
            &utxo_set,
            &mempool_txs,
            template_block_height,
            &prev_header,
            &prev_headers,
            &coinbase_script,
            &coinbase_address,
        ) {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to create block template: {}", e);
                return Err(RpcError::internal_error(format!(
                    "Template creation failed: {e}"
                )));
            }
        };

        // Publish BlockTemplateUpdated for module subscribers
        if let Some(ref ep) = self.event_publisher {
            let prev_hash = prev_header.prev_block_hash;
            let tx_count = template.transactions.len();
            ep.publish_block_template_updated(&prev_hash, template_block_height, tx_count)
                .await;
        }

        // 6. Convert to JSON-RPC format (BIP 22/23)
        self.template_to_json_rpc(&template, &prev_header, template_block_height)
    }

    /// Convert BlockTemplate to JSON-RPC format
    fn template_to_json_rpc(
        &self,
        template: &blvm_protocol::mining::BlockTemplate,
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

    /// Calculate difficulty from bits (compact target format).
    /// Uses blvm-consensus difficulty_from_bits (MAX_TARGET / target).
    fn calculate_difficulty(bits: u64) -> f64 {
        blvm_consensus::pow::difficulty_from_bits(bits).unwrap_or(1.0)
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
            .ok_or_else(|| anyhow::anyhow!(TIP_BLOCK_NOT_FOUND_MSG))?;
        let tip_block = storage
            .blocks()
            .get_block(&tip_hash)?
            .ok_or_else(|| anyhow::anyhow!(TIP_BLOCK_NOT_FOUND_MSG))?;
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
            Ok(UtxoSet::default())
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
            // Transaction types are the same between blvm_protocol and blvm_consensus
            // (blvm_protocol re-exports them), so we can use tx directly
            use blvm_protocol::sigop::get_legacy_sigop_count;
            get_legacy_sigop_count(tx)
        }
        #[cfg(not(feature = "sigop"))]
        {
            // Fallback: basic counting
            let mut count = 0u32;
            for output in &tx.outputs {
                for &byte in &output.script_pubkey {
                    match byte {
                        OP_CHECKSIG => count += 1,
                        OP_CHECKSIGVERIFY => count += 1,
                        OP_CHECKMULTISIG => count += 1,
                        OP_CHECKMULTISIGVERIFY => count += 20,
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
        // Use blvm-consensus's get_block_subsidy (formally verified)
        let subsidy = self.consensus.get_block_subsidy(template.height) as u64;

        // Calculate total fees from transactions
        let fees: u64 = template
            .transactions
            .iter()
            .map(|tx| self.calculate_transaction_fee(tx))
            .sum();

        subsidy + fees
    }

    /// Active BIP9-style `rules` for `getblocktemplate`, aligned with [`ForkActivationTable`]
    /// (`blvm-consensus::activation`) and shared activation constants.
    fn get_active_rules(&self, height: Natural) -> Vec<String> {
        let network = self.consensus_network_from_storage();
        // Match `ForkActivationTable::from_network` (Core testnet3 vs mainnet);
        // regtest activates CSV/segwit/taproot from genesis (0).
        let (csv_h, segwit_h, taproot_h) = match network {
            ConsensusNetwork::Mainnet => (
                BIP112_CSV_ACTIVATION_MAINNET,
                SEGWIT_ACTIVATION_MAINNET,
                TAPROOT_ACTIVATION_MAINNET,
            ),
            ConsensusNetwork::Testnet => (
                BIP112_CSV_ACTIVATION_TESTNET,
                SEGWIT_ACTIVATION_TESTNET,
                TAPROOT_ACTIVATION_TESTNET,
            ),
            ConsensusNetwork::Regtest => (
                BIP112_CSV_ACTIVATION_REGTEST,
                0u64,
                0u64,
            ),
        };

        let mut rules = Vec::new();
        if height >= csv_h {
            rules.push("csv".to_string());
        }
        if height >= segwit_h {
            rules.push("segwit".to_string());
        }
        if height >= taproot_h {
            rules.push("taproot".to_string());
        }
        rules
    }

    fn consensus_network_from_storage(&self) -> ConsensusNetwork {
        let Some(ref storage) = self.storage else {
            return ConsensusNetwork::Mainnet;
        };
        let Ok(Some(info)) = storage.chain().load_chain_info() else {
            return ConsensusNetwork::Mainnet;
        };
        match info.chain_params.network.as_str() {
            "mainnet" => ConsensusNetwork::Mainnet,
            "testnet" => ConsensusNetwork::Testnet,
            "regtest" => ConsensusNetwork::Regtest,
            _ => ConsensusNetwork::Mainnet,
        }
    }

    fn get_min_time(&self, _height: Natural) -> Natural {
        // Get minimum time (median time of last 11 blocks + 1)
        // For now, return current time
        current_timestamp() as Natural
    }

    /// BIP34 height in coinbase `scriptSig` (regtest has BIP34 from height 0).
    fn regtest_coinbase_script_sig(height: u64) -> Vec<u8> {
        if height == 0 {
            return vec![0x00, 0xff];
        }
        let mut n = height;
        let mut height_bytes = Vec::new();
        while n > 0 {
            height_bytes.push((n & 0xff) as u8);
            n >>= 8;
        }
        if height_bytes.last().map_or(false, |&b| b & 0x80 != 0) {
            height_bytes.push(0x00);
        }
        let mut script_sig = Vec::with_capacity(1 + height_bytes.len());
        script_sig.push(height_bytes.len() as u8);
        script_sig.extend_from_slice(&height_bytes);
        if script_sig.len() < 2 {
            script_sig.push(0x00);
        }
        script_sig
    }

    /// Mine blocks on regtest and attach them to the local chain (Core-style `generatetoaddress`).
    ///
    /// Params: `[nblocks, address, maxtries?]`. Uses the same block construction path as the
    /// regtest integration test (`create_new_block`, version 4, `SyncCoordinator::process_block`).
    pub async fn generate_to_address(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: generatetoaddress");

        const MAX_BLOCKS: u64 = 10_000;

        let protocol = self.protocol_engine.as_ref().ok_or_else(|| {
            RpcError::internal_error("generatetoaddress requires protocol engine (node misconfigured)")
        })?;
        if protocol.get_protocol_version() != ProtocolVersion::Regtest {
            return Err(RpcError::invalid_params(
                "generatetoaddress is only supported when the node runs regtest protocol",
            ));
        }

        let storage = self
            .storage
            .as_ref()
            .ok_or_else(|| RpcError::internal_error("generatetoaddress requires storage"))?;

        let nblocks = param_u64_required(params, 0, "generatetoaddress")?;
        if nblocks > MAX_BLOCKS {
            return Err(RpcError::invalid_params(format!(
                "generatetoaddress: nblocks must be <= {MAX_BLOCKS}"
            )));
        }

        let address = param_str_required(params, 1, "generatetoaddress")?;
        let coinbase_address = address_string_to_script_pubkey(&address)?;
        let max_tries = param_u64_default(params, 2, 2_000_000);

        let mut coord = SyncCoordinator::new();
        let mut utxo = storage
            .utxos()
            .get_all_utxos()
            .map_err(|e| RpcError::internal_error(format!("Failed to load UTXO set: {e}")))?;

        let mut out_hashes: Vec<Value> = Vec::with_capacity(nblocks as usize);

        for _ in 0..nblocks {
            let connect_height = storage
                .blocks()
                .block_count()
                .map_err(|e| RpcError::internal_error(format!("Failed to count blocks: {e}")))?
                as u64;

            let prev_header = storage
                .chain()
                .get_tip_header()
                .map_err(|e| RpcError::internal_error(format!("Failed to get tip header: {e}")))?
                .ok_or_else(|| RpcError::internal_error("No chain tip"))?;

            let mut prev_headers = storage
                .blocks()
                .get_recent_headers(2016)
                .unwrap_or_default();
            if prev_headers.len() < 2 {
                prev_headers = vec![prev_header.clone(), prev_header.clone()];
            }

            let coinbase_script = Self::regtest_coinbase_script_sig(connect_height);

            let mut block = self
                .consensus
                .create_new_block(
                    &utxo,
                    &[],
                    connect_height,
                    &prev_header,
                    &prev_headers,
                    &coinbase_script,
                    &coinbase_address,
                )
                .map_err(|e| {
                    RpcError::internal_error(format!("generatetoaddress: template failed: {e}"))
                })?;
            block.header.version = 4;

            let (mined, result) = self
                .consensus
                .mine_block(block, max_tries)
                .map_err(|e| RpcError::internal_error(format!("generatetoaddress: mine failed: {e}")))?;
            if !matches!(result, MiningResult::Success) {
                return Err(RpcError::internal_error(format!(
                    "generatetoaddress: proof-of-work failed after {max_tries} attempts (height {connect_height})"
                )));
            }

            let witnesses: Vec<Vec<Witness>> = mined
                .transactions
                .iter()
                .map(|tx| tx.inputs.iter().map(|_| Witness::default()).collect())
                .collect();
            let wire = serialize_block_with_witnesses(&mined, &witnesses, true);

            let accepted = coord
                .process_block(
                    storage.blocks().as_ref(),
                    protocol.as_ref(),
                    Some(storage),
                    &wire,
                    connect_height,
                    &mut utxo,
                    None,
                    None,
                )
                .map_err(|e| RpcError::internal_error(format!("generatetoaddress: {e}")))?;
            if !accepted {
                return Err(RpcError::internal_error(format!(
                    "generatetoaddress: block rejected at height {connect_height}"
                )));
            }

            let block_hash = storage.blocks().as_ref().get_block_hash(&mined);
            storage
                .chain()
                .update_tip(&block_hash, &mined.header, connect_height)
                .map_err(|e| RpcError::internal_error(format!("Failed to update chain tip: {e}")))?;
            storage
                .utxos()
                .store_utxo_set(&utxo)
                .map_err(|e| RpcError::internal_error(format!("Failed to store UTXO set: {e}")))?;

            out_hashes.push(Value::String(hex::encode(block_hash)));

            if let Some(ref ep) = self.event_publisher {
                ep.publish_block_mined(&block_hash, connect_height, None).await;
            }

            if let Some(ref nm) = self.network_manager {
                use crate::network::protocol::{BlockMessage, ProtocolMessage, ProtocolParser};
                let p2p_msg = ProtocolMessage::Block(BlockMessage {
                    block: mined.clone(),
                    witnesses: witnesses.clone(),
                });
                match ProtocolParser::serialize_message(&p2p_msg) {
                    Ok(framed) => {
                        if let Err(e) = nm.broadcast(framed).await {
                            warn!(
                                "generatetoaddress: P2P broadcast failed at height {connect_height}: {e}"
                            );
                        }
                    }
                    Err(e) => warn!(
                        "generatetoaddress: serialize P2P block message at height {connect_height}: {e}"
                    ),
                }
            }
        }

        Ok(Value::Array(out_hashes))
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
        let (block, witnesses) = deserialize_block_with_witnesses(&block_bytes)
            .map_err(|e| RpcError::invalid_params(format!("Failed to deserialize block: {e}")))?;

        // Validate serialized size to match consensus serialization (defensive check)
        // Uses consensus serialization via blvm_protocol::serialization re-exports.
        let include_witness = true;
        if !blvm_protocol::serialization::block::validate_block_serialized_size(
            &block,
            &witnesses,
            include_witness,
            block_bytes.len(),
        ) {
            return Err(RpcError::invalid_params(
                "Block size mismatch: serialized block does not match wire size".to_string(),
            ));
        }

        // Full attach path: queue for the main run loop (regtest mining + any submitblock user).
        if let Some(ref nm) = self.network_manager {
            if let Some(ref storage) = self.storage {
                if let Ok(Some(tip)) = storage.chain().get_tip_hash() {
                    if block.header.prev_block_hash != tip {
                        return Err(RpcError::invalid_params(
                            "submitblock: prev_block_hash does not match current chain tip"
                                .to_string(),
                        ));
                    }
                }
            }
            let queued_len = block_bytes.len();
            nm.queue_block(block_bytes);
            debug!("submitblock: queued {queued_len} bytes for run-loop processing");
            if let Some(ref ep) = self.event_publisher {
                if let Some(ref storage) = self.storage {
                    let block_hash = storage.blocks().get_block_hash(&block);
                    let height = self.get_current_height()?.unwrap_or(0);
                    ep.publish_block_mined(&block_hash, height, None).await;
                }
            }
            return Ok(Value::Null);
        }

        // Headless / tests: legacy consensus-only check (does not attach to chain).
        let height = self
            .get_current_height()?
            .ok_or_else(|| RpcError::internal_error("Chain not initialized"))?;
        let utxo_set = self.get_utxo_set()?;

        let network_time = current_timestamp();
        let time_context = Some(blvm_consensus::types::TimeContext {
            network_time,
            median_time_past: 0,
        });
        let network = blvm_consensus::types::Network::Mainnet;

        match self.consensus.validate_block_with_time_context(
            &block,
            &[],
            utxo_set,
            height,
            time_context,
            network,
        ) {
            Ok((ValidationResult::Valid, _)) => {
                debug!("submitblock: validation-only success (no network manager)");
                if let Some(ref ep) = self.event_publisher {
                    if let Some(ref storage) = self.storage {
                        let block_hash = storage.blocks().get_block_hash(&block);
                        ep.publish_block_mined(&block_hash, height, None).await;
                    }
                }
                Ok(Value::Null)
            }
            Ok((ValidationResult::Invalid(reason), _)) => {
                if let Some(ref ep) = self.event_publisher {
                    if let Some(ref storage) = self.storage {
                        let block_hash = storage.blocks().get_block_hash(&block);
                        ep.publish_consensus_rule_violation(
                            "block_validation",
                            Some(&block_hash),
                            None,
                            &reason,
                        )
                        .await;
                    }
                }
                Err(RpcError::invalid_params(format!("Invalid block: {reason}")))
            }
            Err(e) => {
                let err_msg = format!("Validation error: {e}");
                if let Some(ref ep) = self.event_publisher {
                    if let Some(ref storage) = self.storage {
                        let block_hash = storage.blocks().get_block_hash(&block);
                        ep.publish_consensus_rule_violation(
                            "block_validation",
                            Some(&block_hash),
                            None,
                            &err_msg,
                        )
                        .await;
                    }
                }
                Err(RpcError::internal_error(err_msg))
            }
        }
    }

    /// Estimate smart fee rate
    ///
    /// Params: [conf_target (optional, default: 6), estimate_mode (optional, default: "conservative")]
    pub async fn estimate_smart_fee(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: estimatesmartfee");

        let conf_target = crate::rpc::params::param_u64_default(params, 0, 6);

        let estimate_mode = crate::rpc::params::param_str(params, 1).unwrap_or("conservative");

        // Validate estimate_mode
        match estimate_mode {
            "unset" | "economical" | "conservative" => {}
            _ => {
                return Err(RpcError::invalid_params(format!(
                    "Invalid estimate_mode: {estimate_mode}. Must be 'unset', 'economical', or 'conservative'"
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
                    "Transaction {txid} not found in mempool"
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
