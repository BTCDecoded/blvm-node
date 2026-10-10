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
use crate::utils::{CACHE_REFRESH_TIP, current_timestamp};
use async_trait::async_trait;
use blvm_protocol::mining::BlockTemplate;
use blvm_protocol::mining::MiningResult;
use blvm_protocol::opcodes::{
    OP_CHECKMULTISIG, OP_CHECKMULTISIGVERIFY, OP_CHECKSIG, OP_CHECKSIGVERIFY,
};
use blvm_protocol::segwit::Witness;
use blvm_protocol::serialization::deserialize_block_with_witnesses;
use blvm_protocol::serialization::serialize_block_with_witnesses;
use blvm_protocol::serialization::serialize_transaction;
use blvm_protocol::types::Network as ConsensusNetwork;
use blvm_protocol::{
    BIP112_CSV_ACTIVATION_MAINNET, BIP112_CSV_ACTIVATION_REGTEST, BIP112_CSV_ACTIVATION_TESTNET,
    SEGWIT_ACTIVATION_MAINNET, SEGWIT_ACTIVATION_TESTNET, TAPROOT_ACTIVATION_MAINNET,
    TAPROOT_ACTIVATION_TESTNET,
};
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use blvm_protocol::{
    ConsensusProof,
    types::{BlockHeader, ByteString, Natural, Transaction, UtxoSet},
};
use hex;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::{debug, warn};

/// Late-bound Commons GBT source. Shared across JSON-RPC and REST `MiningRpc` copies.
#[derive(Clone, Default)]
pub struct CommonsGbtSlot {
    inner: Arc<std::sync::RwLock<Option<Arc<dyn CommonsGbtCaller>>>>,
}

impl CommonsGbtSlot {
    /// Install the caller used by `getblocktemplate` when Commons is loaded.
    pub fn set(&self, caller: Arc<dyn CommonsGbtCaller>) {
        *self.inner.write().unwrap_or_else(|e| e.into_inner()) = Some(caller);
    }

    pub(crate) fn get(&self) -> Option<Arc<dyn CommonsGbtCaller>> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Fetches Commons coinbase outputs for GBT. Hold must be an error.
#[async_trait]
pub trait CommonsGbtCaller: Send + Sync {
    async fn fetch_commons_gbt_outputs(&self) -> RpcResult<Option<Vec<(i64, Vec<u8>)>>>;
}

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
    /// Shared Commons GBT slot (empty = node-default coinbase).
    commons_gbt: CommonsGbtSlot,
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
            commons_gbt: CommonsGbtSlot::default(),
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
            commons_gbt: CommonsGbtSlot::default(),
        }
    }

    /// Share the manager's Commons GBT slot (JSON-RPC and REST must see the same bind).
    pub fn with_commons_gbt(mut self, slot: CommonsGbtSlot) -> Self {
        self.commons_gbt = slot;
        self
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

    /// Commons outputs when the module is issuing work. `None` = use the caller address.
    async fn try_commons_gbt_outputs(&self) -> RpcResult<Option<Vec<(i64, ByteString)>>> {
        let Some(caller) = self.commons_gbt.get() else {
            return Ok(None);
        };
        match caller.fetch_commons_gbt_outputs().await? {
            Some(outs) if !outs.is_empty() => Ok(Some(outs.into_iter().collect())),
            _ => Ok(None),
        }
    }

    /// Commons outputs when bound; otherwise one 0-value output at `fallback` (fit tops up).
    async fn resolve_gbt_outputs(&self, fallback: ByteString) -> RpcResult<Vec<(i64, ByteString)>> {
        Ok(self
            .try_commons_gbt_outputs()
            .await?
            .unwrap_or_else(|| vec![(0, fallback)]))
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
    /// Uses `create_block_template_with_outputs` (subsidy+fees + BIP141).
    /// Spec-locked single-output `create_block_template` is unchanged.
    pub async fn get_block_template(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: getblocktemplate");

        // 1. Chain tip + height index for the *next* block to mine.
        // Must match `SyncCoordinator::process_block` / run-loop indexing: the first block
        // stored on a fresh datadir (genesis in chainstate only) uses height 0; then 1, 2, …
        // `chain_info.height` stays on the tip header's logical index — using it here was
        // one block behind after the first connect. Blockstore count matches the next index.
        let template_block_height: Natural = if let Some(ref storage) = self.storage {
            storage
                .chain()
                .get_height()
                .map_err(|e| RpcError::internal_error(format!("Failed to read tip height: {e}")))?
                .unwrap_or(0)
                .saturating_add(1)
        } else {
            return Err(RpcError::internal_error(
                "getblocktemplate requires storage".to_string(),
            ));
        };
        let prev_header = self
            .get_tip_header()?
            .ok_or_else(|| RpcError::internal_error("No chain tip"))?;
        let prev_headers = self.headers_for_work(template_block_height.saturating_sub(1))?;

        // 2. Get mempool transactions
        let mempool_txs: Vec<Transaction> = self.get_mempool_transactions()?;

        // 3. Get UTXO set
        let utxo_set = self.get_utxo_set()?;

        // 4. Extract coinbase parameters from request or use defaults
        let coinbase_script = self.extract_coinbase_script(params).unwrap_or_default();
        let coinbase_address = self.extract_coinbase_address(params).unwrap_or_default();

        let network = self.consensus_network();
        let mempool_witnesses =
            self.build_mempool_witnesses_for_template(&utxo_set, &mempool_txs)?;
        // Commons outputs when the module is loaded; else value 0 tops up the caller address.
        // Hold is an error so GBT does not issue a node-default coinbase.
        let outputs = self.resolve_gbt_outputs(coinbase_address).await?;
        let template = match self.consensus.create_block_template_with_outputs(
            &utxo_set,
            &mempool_txs,
            template_block_height,
            &prev_header,
            &prev_headers,
            &coinbase_script,
            &outputs,
            network,
            Some(&mempool_witnesses),
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
            if let Some(tip_hash) = self
                .storage
                .as_ref()
                .and_then(|s| s.chain().get_tip_hash().ok().flatten())
            {
                let tx_count = template.transactions.len();
                ep.publish_block_template_updated(&tip_hash, template_block_height, tx_count)
                    .await;
            }
        }

        // 6. Convert to JSON-RPC format (BIP 22/23)
        let tip_hash = if let Some(ref storage) = self.storage {
            storage
                .chain()
                .get_tip_hash()
                .map_err(|e| RpcError::internal_error(format!("Failed to get tip hash: {e}")))?
                .ok_or_else(|| RpcError::internal_error("No chain tip hash".to_string()))?
        } else {
            return Err(RpcError::internal_error(
                "getblocktemplate requires storage".to_string(),
            ));
        };

        self.template_to_json_rpc(
            &template,
            template_block_height,
            &tip_hash,
            Self::client_wants_coinbasetxn(params),
        )
    }

    /// Convert BlockTemplate to JSON-RPC format
    fn template_to_json_rpc(
        &self,
        template: &blvm_protocol::mining::BlockTemplate,
        height: Natural,
        tip_hash: &blvm_protocol::Hash,
        include_coinbasetxn: bool,
    ) -> RpcResult<Value> {
        // BIP22/ckpool: hash of the block we build on top of (current chain tip).
        let prev_hash_hex = crate::storage::hashing::hash_to_rpc_hex(tip_hash);

        // Bitcoin Core `hashTarget.GetHex()` — full 256-bit target from nBits.
        let target_hex = blvm_protocol::pow::expand_target(template.header.bits)
            .map(|t| t.gbt_target_hex())
            .unwrap_or_else(|_| format!("{:064x}", template.target));

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

        let mut body = json!({
            "capabilities": ["proposal", "coinbasetxn"],
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
        });
        if include_coinbasetxn {
            let data = hex::encode(serialize_transaction(&template.coinbase_tx));
            body["coinbasetxn"] = json!({ "data": data });
        }
        Ok(body)
    }

    fn client_wants_coinbasetxn(params: &Value) -> bool {
        params
            .get(0)
            .and_then(|r| r.get("capabilities"))
            .and_then(|c| c.as_array())
            .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some("coinbasetxn")))
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

    /// Oldest-to-newest headers ending at `parent_height` (up to one difficulty period).
    fn headers_for_work(&self, parent_height: u64) -> RpcResult<Vec<BlockHeader>> {
        let Some(ref storage) = self.storage else {
            return Ok(vec![]);
        };
        storage
            .blocks()
            .headers_back_from(parent_height, 2016)
            .map_err(|e| {
                RpcError::internal_error(format!("Failed to load difficulty headers: {e}"))
            })
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

    /// Per-mempool-tx witness stacks aligned with `get_mempool_transactions` ordering.
    fn build_mempool_witnesses_for_template(
        &self,
        utxo_set: &UtxoSet,
        mempool_txs: &[Transaction],
    ) -> RpcResult<Vec<Option<Vec<Witness>>>> {
        mempool_witnesses_for_template(self.mempool.as_deref(), utxo_set, mempool_txs)
            .map_err(RpcError::internal_error)
    }

    /// Calculate difficulty from bits (compact target format).
    /// Uses blvm-consensus difficulty_from_bits (MAX_TARGET / target).
    fn calculate_difficulty(bits: u64) -> f64 {
        blvm_protocol::pow::difficulty_from_bits(bits).unwrap_or(1.0)
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
        if let Some(template_request) = params.get(0) {
            if let Some(addr) = template_request.get("coinbaseaddress") {
                if let Some(addr_str) = addr.as_str() {
                    if let Ok(script) = address_string_to_script_pubkey(addr_str) {
                        return Some(script);
                    }
                }
            }
        }
        Some(vec![])
    }

    fn transaction_to_json(&self, tx: &Transaction) -> Value {
        use blvm_protocol::block::calculate_tx_id;

        let witnesses = self.mempool.as_ref().and_then(|mempool| {
            let txid = calculate_tx_id(tx);
            mempool.get_transaction_witnesses(&txid)
        });
        let wire = crate::rpc::txwire::tx_wire(tx, witnesses.as_deref());
        let fee = self.calculate_transaction_fee(tx);
        let sigops = self.count_sigops(tx);
        let weight = self.calculate_weight(tx);

        json!({
            "data": hex::encode(&wire.bytes),
            "txid": wire.txid_hex,
            "hash": wire.hash_hex,
            "fee": fee,
            "sigops": sigops,
            "weight": weight,
        })
    }

    fn calculate_transaction_fee(&self, tx: &Transaction) -> u64 {
        let Some(mempool) = self.mempool.as_ref() else {
            return 0;
        };
        let Ok(utxo_set) = self.get_utxo_set() else {
            return 0;
        };
        mempool.calculate_transaction_fee(tx, &utxo_set)
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
        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::segwit::transaction_weight_from_stacks;

        let stacks = self.mempool.as_ref().and_then(|mempool| {
            let txid = calculate_tx_id(tx);
            mempool.get_transaction_witnesses(&txid)
        });
        transaction_weight_from_stacks(tx, stacks.as_deref())
            .unwrap_or_else(|_| transaction_weight_from_stacks(tx, None).unwrap_or(0))
    }

    fn calculate_coinbase_value(&self, template: &BlockTemplate, _height: Natural) -> u64 {
        // Regtest halves every 150 blocks. Other networks stay on 210,000.
        let network = self.consensus_network();
        let subsidy = self
            .consensus
            .get_block_subsidy_for_network(template.height, network) as u64;

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
        let network = self.consensus_network();
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
            ConsensusNetwork::Regtest | ConsensusNetwork::Signet => {
                (BIP112_CSV_ACTIVATION_REGTEST, 0u64, 0u64)
            }
            ConsensusNetwork::Testnet4 => (1, 1, 1),
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

    fn consensus_network(&self) -> ConsensusNetwork {
        crate::storage::resolve_consensus_network(
            self.protocol_engine.as_deref(),
            self.storage.as_deref(),
        )
    }

    fn get_min_time(&self, height: Natural) -> Natural {
        // BIP113: median of the 11 headers before this template, plus one.
        if let Some(ref storage) = self.storage {
            if height > 0 {
                if let Ok(headers) = storage.blocks().headers_back_from(height - 1, 11) {
                    if !headers.is_empty() {
                        return blvm_protocol::bip113::get_median_time_past(&headers)
                            .saturating_add(1);
                    }
                }
            }
        }
        current_timestamp() as Natural
    }

    /// Mine blocks on regtest and attach them to the local chain (`generatetoaddress`).
    ///
    /// Params: `[nblocks, address, maxtries?]`. Uses the same block construction path as the
    /// regtest integration test (`create_new_block`, version 4, `SyncCoordinator::process_block`).
    pub async fn generate_to_address(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: generatetoaddress");

        const MAX_BLOCKS: u64 = 10_000;

        let protocol = self.protocol_engine.as_ref().ok_or_else(|| {
            RpcError::internal_error(
                "generatetoaddress requires protocol engine (node misconfigured)",
            )
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

        // Required-work checks read the parent from the blockstore, and the
        // median-time check reads `recent_headers`. `chain().initialize` writes
        // chain info only, so index genesis in both places before mining.
        if let Ok(Some(info)) = storage.chain().load_chain_info() {
            if info.height == 0 {
                let blocks = storage.blocks();
                if matches!(blocks.get_hash_by_height(0), Ok(None)) {
                    blocks
                        .store_header(&info.tip_hash, &info.tip_header)
                        .map_err(|e| {
                            RpcError::internal_error(format!(
                                "generatetoaddress: index genesis: {e}"
                            ))
                        })?;
                    blocks.store_height(0, &info.tip_hash).map_err(|e| {
                        RpcError::internal_error(format!("generatetoaddress: index genesis: {e}"))
                    })?;
                }
                blocks
                    .store_recent_header(0, &info.tip_header)
                    .map_err(|e| {
                        RpcError::internal_error(format!("generatetoaddress: index genesis: {e}"))
                    })?;
            }
        }

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
        if let Some(ref mempool) = self.mempool {
            coord.set_mempool(Some(Arc::clone(mempool)));
        }
        if let Some(ref ep) = self.event_publisher {
            coord.set_event_publisher(Some(Arc::clone(ep)));
        }
        let mut utxo = storage
            .utxos()
            .get_all_utxos()
            .map_err(|e| RpcError::internal_error(format!("Failed to load UTXO set: {e}")))?;

        let mut out_hashes: Vec<Value> = Vec::with_capacity(nblocks as usize);

        for _ in 0..nblocks {
            let (_, tip_height) = storage
                .chain()
                .get_tip_hash_and_height()
                .map_err(|e| RpcError::internal_error(format!("Failed to get tip height: {e}")))?;
            let connect_height = tip_height + 1;

            let prev_header = storage
                .chain()
                .get_tip_header()
                .map_err(|e| RpcError::internal_error(format!("Failed to get tip header: {e}")))?
                .ok_or_else(|| RpcError::internal_error("No chain tip"))?;

            let mut prev_headers = storage
                .blocks()
                .headers_back_from(connect_height.saturating_sub(1), 2016)
                .unwrap_or_default();
            if prev_headers.len() < 2 {
                prev_headers = vec![prev_header.clone(), prev_header.clone()];
            }

            let coinbase_script =
                blvm_protocol::bip_validation::encode_bip34_coinbase_script(connect_height);
            let pool_txs = self.get_mempool_transactions()?;

            let mut block = if let Some(outputs) = self.try_commons_gbt_outputs().await? {
                let template = self
                    .consensus
                    .create_block_template_with_outputs(
                        &utxo,
                        &pool_txs,
                        connect_height,
                        &prev_header,
                        &prev_headers,
                        &coinbase_script,
                        &outputs,
                        ConsensusNetwork::Regtest,
                        None,
                    )
                    .map_err(|e| {
                        RpcError::internal_error(format!("generatetoaddress: template failed: {e}"))
                    })?;
                let mut txs = Vec::with_capacity(1 + template.transactions.len());
                txs.push(template.coinbase_tx);
                txs.extend(template.transactions);
                let mut block = blvm_protocol::Block {
                    header: template.header,
                    transactions: txs.into_boxed_slice(),
                };
                block.header.timestamp = current_timestamp();
                block
            } else {
                self.consensus
                    .create_new_block_with_time(
                        &utxo,
                        &pool_txs,
                        connect_height,
                        &prev_header,
                        &prev_headers,
                        &coinbase_script,
                        &coinbase_address,
                        current_timestamp(),
                        ConsensusNetwork::Regtest,
                        None,
                    )
                    .map_err(|e| {
                        RpcError::internal_error(format!("generatetoaddress: template failed: {e}"))
                    })?
            };
            // Connect checks this timestamp against the 11 headers before the
            // block, not the 2016-header template window.
            let mtp_headers = if connect_height > 0 {
                storage
                    .blocks()
                    .headers_back_from(connect_height - 1, 11)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let median_time_past = if mtp_headers.is_empty() {
                blvm_protocol::bip113::get_median_time_past(&prev_headers)
            } else {
                blvm_protocol::bip113::get_median_time_past(&mtp_headers)
            };
            if block.header.timestamp <= median_time_past {
                block.header.timestamp = median_time_past.saturating_add(1);
            }
            block.header.version = 4;

            let (mined, result) = self.consensus.mine_block(block, max_tries).map_err(|e| {
                RpcError::internal_error(format!("generatetoaddress: mine failed: {e}"))
            })?;
            if !matches!(result, MiningResult::Success) {
                return Err(RpcError::internal_error(format!(
                    "generatetoaddress: proof-of-work failed after {max_tries} attempts (height {connect_height})"
                )));
            }

            let witnesses: Vec<Vec<Witness>> = mined
                .transactions
                .iter()
                .enumerate()
                .map(|(i, tx)| {
                    // The template commitment is hashed with 32 zero bytes.
                    let commitment = i == 0
                        && tx.outputs.iter().any(|output| {
                            let script: &[u8] = output.script_pubkey.as_ref();
                            script.len() >= 6
                                && script[0] == 0x6a
                                && script[2..6] == [0xaa, 0x21, 0xa9, 0xed]
                        });
                    if commitment {
                        tx.inputs.iter().map(|_| vec![vec![0u8; 32]]).collect()
                    } else {
                        tx.inputs.iter().map(|_| Witness::default()).collect()
                    }
                })
                .collect();

            let accepted = coord
                .connect_mined_block(
                    storage.blocks().as_ref(),
                    protocol.as_ref(),
                    storage,
                    &mined,
                    &witnesses,
                    connect_height,
                    &mut utxo,
                )
                .map_err(|e| RpcError::internal_error(format!("generatetoaddress: {e}")))?;
            if !accepted {
                return Err(RpcError::internal_error(format!(
                    "generatetoaddress: block rejected at height {connect_height}"
                )));
            }
            if let Some(ref mempool) = self.mempool {
                mempool.remove_for_connected_block(&mined.transactions);
            }

            let block_hash = storage.blocks().as_ref().get_block_hash(&mined);

            out_hashes.push(Value::String(crate::storage::hashing::hash_to_rpc_hex(
                &block_hash,
            )));

            if let Some(ref ep) = self.event_publisher {
                ep.publish_block_mined(&block_hash, connect_height, None)
                    .await;
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

        let storage = self.storage.as_ref().ok_or_else(|| {
            RpcError::internal_error(
                "submitblock: storage is required to connect the block".to_string(),
            )
        })?;
        let tip = storage
            .chain()
            .get_tip_hash()
            .map_err(|e| RpcError::internal_error(format!("Failed to get chain tip: {e}")))?
            .ok_or_else(|| {
                RpcError::internal_error("submitblock: chain not initialized (no tip)".to_string())
            })?;
        let prev_ok = if let Ok(Some(tip_header)) = storage.blocks().get_header(&tip) {
            blvm_consensus::block::validate_prev_block_hash(&block.header, &tip_header)
        } else {
            block.header.prev_block_hash == tip
        };
        if !prev_ok {
            return Err(RpcError::invalid_params(
                "submitblock: prev_block_hash does not match current chain tip".to_string(),
            ));
        }
        let protocol = self.protocol_engine.as_ref().ok_or_else(|| {
            RpcError::internal_error(
                "submitblock: protocol engine is required to connect the block".to_string(),
            )
        })?;
        let tip_height = self.get_current_height()?.unwrap_or(0);
        let connect_height = tip_height.saturating_add(1);
        let mut utxo = self.get_utxo_set()?;
        let mut coord = SyncCoordinator::new();
        if let Some(ref mempool) = self.mempool {
            coord.set_mempool(Some(Arc::clone(mempool)));
        }
        if let Some(ref ep) = self.event_publisher {
            coord.set_event_publisher(Some(Arc::clone(ep)));
        }
        let accepted = coord
            .connect_mined_block(
                storage.blocks().as_ref(),
                protocol.as_ref(),
                storage,
                &block,
                &witnesses,
                connect_height,
                &mut utxo,
            )
            .map_err(|e| RpcError::invalid_params(format!("submitblock: {e}")))?;
        if !accepted {
            return Err(RpcError::invalid_params(
                "submitblock: block was not connected".to_string(),
            ));
        }
        if let Some(ref mempool) = self.mempool {
            mempool.remove_for_connected_block(&block.transactions);
        }
        let block_hash = storage.blocks().get_block_hash(&block);
        if let Some(ref ep) = self.event_publisher {
            ep.publish_block_mined(&block_hash, connect_height, None)
                .await;
        }
        if let Some(ref nm) = self.network_manager {
            use crate::network::protocol::{BlockMessage, ProtocolMessage, ProtocolParser};
            let p2p_msg = ProtocolMessage::Block(BlockMessage {
                block: block.clone(),
                witnesses: witnesses.clone(),
            });
            if let Ok(framed) = ProtocolParser::serialize_message(&p2p_msg) {
                if let Err(e) = nm.broadcast(framed).await {
                    warn!("submitblock: P2P broadcast failed at height {connect_height}: {e}");
                }
            }
        }
        let _ = block_bytes;
        Ok(Value::Null)
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
                )));
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

            // MempoolManager.get_prioritized_transactions() already ranks by fee; compute rates here.
            for tx in &mempool_txs {
                let fee = self.calculate_transaction_fee(tx); // Now uses UTXO set
                let size = self.calculate_weight(tx) as usize;

                if size > 0 {
                    let vsize = (size as u64).div_ceil(4).max(1);
                    let rate = (fee as f64) / (vsize as f64) * 1000.0 / 100_000_000.0;
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
    /// Params: ["txid", fee_delta] or Core-compatible ["txid", dummy, fee_delta]
    pub async fn prioritise_transaction(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: prioritisetransaction");

        let txid = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::invalid_params("Transaction ID required".to_string()))?;

        let fee_delta = match params.as_array().map(|a| a.len()) {
            Some(len) if len >= 3 => params.get(2),
            _ => params.get(1),
        }
        .and_then(|p| p.as_i64())
        .ok_or_else(|| RpcError::invalid_params("Fee delta required".to_string()))?;

        let hash = crate::storage::hashing::hash_from_rpc_hex(txid)
            .map_err(|e| RpcError::invalid_params(format!("Invalid transaction ID: {e}")))?;

        if let Some(ref mempool) = self.mempool {
            if mempool.prioritise_transaction(&hash, fee_delta) {
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

fn spends_witness_utxo(tx: &Transaction, utxo_set: &UtxoSet) -> bool {
    use blvm_consensus::witness::{
        extract_witness_program, extract_witness_version, validate_witness_program_length,
    };
    tx.inputs.iter().any(|input| {
        utxo_set.get(&input.prevout).is_some_and(|utxo| {
            let script = utxo.script_pubkey.as_ref().to_vec();
            extract_witness_version(&script)
                .and_then(|version| {
                    extract_witness_program(&script, version).map(|program| (version, program))
                })
                .is_some_and(|(version, program)| {
                    validate_witness_program_length(&program, version)
                })
        })
    })
}

/// `Some(stacks)` when the mempool has them. `None` is a legacy empty-stack.
/// A witness spend without stored stacks is an error (no stale BIP141).
pub(crate) fn mempool_witnesses_for_template(
    mempool: Option<&MempoolManager>,
    utxo_set: &UtxoSet,
    mempool_txs: &[Transaction],
) -> Result<Vec<Option<Vec<Witness>>>, String> {
    use blvm_protocol::block::calculate_tx_id;

    mempool_txs
        .iter()
        .map(|tx| {
            let txid = calculate_tx_id(tx);
            if let Some(mp) = mempool {
                if let Some(wits) = mp.get_transaction_witnesses(&txid) {
                    if wits.len() != tx.inputs.len() {
                        return Err(format!(
                            "witness count {} != input count {} for tx {}",
                            wits.len(),
                            tx.inputs.len(),
                            hex::encode(txid)
                        ));
                    }
                    return Ok(Some(wits));
                }
            }
            if spends_witness_utxo(tx, utxo_set) {
                return Err(format!(
                    "missing mempool witnesses for witness spend tx {}",
                    hex::encode(txid)
                ));
            }
            Ok(None)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::MiningRpc;
    use crate::node::mempool::MempoolManager;
    use crate::storage::Storage;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{
        OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO, UtxoSet,
    };
    use std::sync::Arc;

    fn spend(prevout: OutPoint, output_value: i64) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: Vec::new(),
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: output_value,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        }
    }

    #[test]
    fn mining_fee_counts_a_mempool_parent_and_ignores_an_out_of_range_value() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let mempool = Arc::new(MempoolManager::new());

        let funding = OutPoint {
            hash: [1u8; 32],
            index: 0,
        };
        let negative = OutPoint {
            hash: [9u8; 32],
            index: 0,
        };
        let mut utxo_set = UtxoSet::default();
        utxo_set.insert(
            funding,
            Arc::new(UTXO {
                value: 50_000,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        utxo_set.insert(
            negative,
            Arc::new(UTXO {
                value: -1,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        for (outpoint, utxo) in &utxo_set {
            storage.utxos().add_utxo(outpoint, utxo).unwrap();
        }
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));

        let parent = spend(funding, 40_000);
        assert!(mempool.add_transaction(parent.clone()).unwrap());
        let child = spend(
            OutPoint {
                hash: blvm_protocol::block::calculate_tx_id(&parent),
                index: 0,
            },
            25_000,
        );

        let mining = MiningRpc::with_dependencies(storage, mempool);
        assert_eq!(mining.calculate_transaction_fee(&child), 15_000);
        assert_eq!(mining.calculate_transaction_fee(&spend(negative, 1_000)), 0);
    }

    #[test]
    fn mining_fee_does_not_wrap_a_negative_output() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let mempool = Arc::new(MempoolManager::new());

        let funding = OutPoint {
            hash: [1u8; 32],
            index: 0,
        };
        let utxo = UTXO {
            value: 50_000,
            script_pubkey: vec![OP_1].into(),
            height: 0,
            is_coinbase: false,
        };
        storage.utxos().add_utxo(&funding, &utxo).unwrap();
        let mut utxo_set = UtxoSet::default();
        utxo_set.insert(funding, Arc::new(utxo));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));

        let mut wrapped = spend(funding, 1_000);
        wrapped.outputs.push(TransactionOutput {
            value: -1,
            script_pubkey: vec![OP_1],
        });

        let mining = MiningRpc::with_dependencies(storage, mempool);
        assert_eq!(mining.calculate_transaction_fee(&wrapped), 0);
        assert_eq!(
            mining.calculate_transaction_fee(&spend(funding, 40_000)),
            10_000
        );
    }

    #[tokio::test]
    async fn submitblock_without_storage_returns_an_error() {
        let mining = MiningRpc::new();
        assert!(
            mining
                .submit_block(&serde_json::json!(["00"]))
                .await
                .is_err()
        );
    }

    fn regtest_mining() -> (
        tempfile::TempDir,
        Arc<Storage>,
        Arc<MempoolManager>,
        MiningRpc,
    ) {
        use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let genesis = protocol.get_network_params().genesis_block.header.clone();
        let dir = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(dir.path()).unwrap());
        storage.chain().initialize(&genesis).unwrap();
        let mempool = Arc::new(MempoolManager::new());
        let mining = MiningRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&mempool))
            .with_protocol_engine(protocol);
        (dir, storage, mempool, mining)
    }

    fn one_block_params() -> serde_json::Value {
        serde_json::json!([
            1u64,
            "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4",
            2_000_000u64
        ])
    }

    #[tokio::test]
    async fn empty_pool_is_coinbase_only_and_the_hash_matches_get_block() {
        use crate::rpc::blockchain::BlockchainRpc;
        use crate::storage::hashing::hash_to_rpc_hex;
        use blvm_protocol::block::calculate_tx_id;

        let (_dir, storage, _mempool, mining) = regtest_mining();
        let mined = mining
            .generate_to_address(&one_block_params())
            .await
            .unwrap();
        let block_hash = mined[0].as_str().unwrap().to_string();
        let tip = storage.chain().get_tip_hash().unwrap().unwrap();
        let block = storage.blocks().get_block(&tip).unwrap().unwrap();
        assert_eq!(block.transactions.len(), 1);
        assert_eq!(block_hash, hash_to_rpc_hex(&tip));

        let chain = BlockchainRpc::with_dependencies(Arc::clone(&storage));
        let best = chain.get_best_block_hash().await.unwrap();
        assert_eq!(best.as_str(), Some(block_hash.as_str()));
        let shown = chain.get_block(&block_hash).await.unwrap();
        assert_eq!(
            shown["merkleroot"].as_str(),
            Some(hash_to_rpc_hex(&block.header.merkle_root).as_str())
        );
        assert_eq!(
            shown["tx"][0].as_str(),
            Some(hash_to_rpc_hex(&calculate_tx_id(&block.transactions[0])).as_str())
        );
    }

    #[tokio::test]
    async fn fee_paying_pool_transaction_is_mined() {
        let (_dir, storage, mempool, mining) = regtest_mining();
        let funding = OutPoint {
            hash: [3u8; 32],
            index: 0,
        };
        let utxo = UTXO {
            value: 50_000,
            script_pubkey: vec![OP_1].into(),
            height: 0,
            is_coinbase: false,
        };
        storage.utxos().add_utxo(&funding, &utxo).unwrap();
        let mut set = UtxoSet::default();
        set.insert(funding, Arc::new(utxo));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
        let tx = spend(funding, 40_000);
        assert!(mempool.add_transaction(tx.clone()).unwrap());

        mining
            .generate_to_address(&one_block_params())
            .await
            .unwrap();
        let tip = storage.chain().get_tip_hash().unwrap().unwrap();
        let block = storage.blocks().get_block(&tip).unwrap().unwrap();
        assert!(
            block.transactions.len() > 1,
            "pool transaction must follow the coinbase"
        );
        assert_eq!(block.transactions[1].outputs[0].value, 40_000);
    }

    #[tokio::test]
    async fn pruned_body_keeps_the_template_at_the_next_height() {
        let (_dir, storage, _mempool, mining) = regtest_mining();
        mining
            .generate_to_address(&one_block_params())
            .await
            .unwrap();
        let tip = storage.chain().get_tip_hash().unwrap().unwrap();
        let height = storage.chain().get_height().unwrap().unwrap();
        storage.blocks().remove_block_body(&tip).unwrap();
        let template = mining
            .get_block_template(&serde_json::json!([]))
            .await
            .unwrap();
        assert_eq!(template["height"].as_u64(), Some(height + 1));
    }

    #[tokio::test]
    async fn submitblock_returns_null_only_after_the_block_connects() {
        use crate::module::api::events::EventManager;
        use crate::module::traits::EventType;
        use crate::node::event_publisher::EventPublisher;
        use blvm_protocol::serialization::serialize_block_with_witnesses;
        use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};

        let (_dir, storage, _mempool, mining) = regtest_mining();
        mining
            .generate_to_address(&one_block_params())
            .await
            .unwrap();
        let tip = storage.chain().get_tip_hash().unwrap().unwrap();
        let block = storage.blocks().get_block(&tip).unwrap().unwrap();
        let witnesses = storage
            .blocks()
            .get_witness(&tip)
            .unwrap()
            .unwrap_or_else(|| vec![vec![]; block.transactions.len()]);
        let bytes = serialize_block_with_witnesses(&block, &witnesses, true);

        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let genesis = protocol.get_network_params().genesis_block.header.clone();
        let dir = tempfile::TempDir::new().unwrap();
        let fresh = Arc::new(Storage::new(dir.path()).unwrap());
        fresh.chain().initialize(&genesis).unwrap();
        let genesis_hash = fresh.chain().get_tip_hash().unwrap().unwrap();
        fresh
            .blocks()
            .store_header(&genesis_hash, &genesis)
            .unwrap();
        fresh.blocks().store_height(0, &genesis_hash).unwrap();
        fresh.blocks().store_recent_header(0, &genesis).unwrap();
        let events = Arc::new(EventManager::new());
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        events
            .subscribe_module("lock".into(), vec![EventType::BlockMined], tx)
            .await
            .unwrap();
        let publisher = Arc::new(EventPublisher::new(Arc::clone(&events)));
        let submit =
            MiningRpc::with_dependencies(Arc::clone(&fresh), Arc::new(MempoolManager::new()))
                .with_protocol_engine(protocol)
                .with_event_publisher(Some(publisher));
        let result = submit
            .submit_block(&serde_json::json!([hex::encode(&bytes)]))
            .await
            .unwrap();
        assert!(result.is_null());
        assert_eq!(fresh.chain().get_height().unwrap().unwrap(), 1);
        assert!(rx.try_recv().is_ok());

        let rejected = submit
            .submit_block(&serde_json::json!([hex::encode(&bytes)]))
            .await;
        assert!(rejected.is_err());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn one_sat_per_virtual_byte_quotes_before_the_mode_multiplier() {
        use blvm_protocol::segwit::transaction_weight_from_stacks;

        let (_dir, storage, mempool, mining) = regtest_mining();
        let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
        policy.min_tx_fee = 0;
        policy.min_relay_fee_rate = 0;
        let funding = OutPoint {
            hash: [4u8; 32],
            index: 0,
        };
        let mut tx = spend(funding, 50_000);
        tx.inputs[0].script_sig = vec![OP_1];
        let vsize = transaction_weight_from_stacks(&tx, None)
            .unwrap()
            .div_ceil(4)
            .max(1) as i64;
        tx.outputs[0].value = 50_000;
        let utxo = UTXO {
            value: 50_000 + vsize,
            script_pubkey: vec![OP_1].into(),
            height: 0,
            is_coinbase: false,
        };
        storage.utxos().add_utxo(&funding, &utxo).unwrap();
        let mut set = UtxoSet::default();
        set.insert(funding, Arc::new(utxo));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
        mempool.set_policy_config(Some(policy));
        assert!(mempool.add_transaction(tx).unwrap());

        let quote = mining
            .estimate_smart_fee(&serde_json::json!([6u64, "unset"]))
            .await
            .unwrap();
        assert!((quote["feerate"].as_f64().unwrap() - 0.00001).abs() < 1e-12);
        assert_eq!(quote["blocks"].as_u64(), Some(6));
    }
}
