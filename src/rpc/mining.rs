//! Mining RPC methods
//!
//! Implements mining-related JSON-RPC methods for block template generation and mining.
//! Uses the blvm-consensus mining functions, which are covered by tests. They are not
//! formally verified: see [`docs/VERIFICATION.md`](https://github.com/BTCDecoded/blvm-consensus/blob/main/docs/VERIFICATION.md) in blvm-consensus.

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

    /// Coinbase the template would add in front of the mempool transactions.
    ///
    /// The output value does not change the serialized length. Height does, through the BIP34 script.
    fn template_coinbase(height: u64) -> Transaction {
        let script_sig = blvm_protocol::bip_validation::encode_bip34_coinbase_script(height);
        Transaction {
            version: 1,
            inputs: vec![blvm_protocol::TransactionInput {
                prevout: blvm_protocol::OutPoint {
                    hash: [0u8; 32],
                    index: 0xffffffff,
                },
                script_sig,
                sequence: 0xfffffffe,
            }]
            .into(),
            outputs: blvm_protocol::tx_outputs![blvm_protocol::TransactionOutput {
                value: 0,
                script_pubkey: vec![
                    blvm_protocol::opcodes::OP_DUP,
                    blvm_protocol::opcodes::OP_HASH160,
                    blvm_protocol::opcodes::PUSH_20_BYTES,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    blvm_protocol::opcodes::OP_EQUALVERIFY,
                    blvm_protocol::opcodes::OP_CHECKSIG,
                ],
            }],
            lock_time: height.saturating_sub(1),
        }
    }

    /// `(size, weight, tx count)` for the next block. An empty pool stays at zero.
    ///
    /// Size is the non-witness serialization. Weight is the BIP141 weight, including
    /// witness stacks stored for each pooled transaction.
    fn template_field_snapshot(&self, tip_height: u64) -> (u64, u64, u64) {
        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::segwit::transaction_weight_from_stacks;

        let Some(mempool) = self.mempool.as_ref() else {
            return (0, 0, 0);
        };
        let txs = mempool.get_transactions();
        if txs.is_empty() {
            return (0, 0, 0);
        }
        let coinbase = Self::template_coinbase(tip_height.saturating_add(1));
        let coinbase_len = serialize_transaction(&coinbase).len() as u64;
        let mut size = coinbase_len;
        let mut weight = transaction_weight_from_stacks(&coinbase, None)
            .unwrap_or(coinbase_len.saturating_mul(4));
        for tx in &txs {
            let len = serialize_transaction(tx).len() as u64;
            size = size.saturating_add(len);
            let stacks = mempool.get_transaction_witnesses(&calculate_tx_id(tx));
            let tx_weight = transaction_weight_from_stacks(tx, stacks.as_deref())
                .unwrap_or(len.saturating_mul(4));
            weight = weight.saturating_add(tx_weight);
        }
        (size, weight, txs.len() as u64 + 1)
    }

    /// Get mining information
    pub async fn get_mining_info(&self) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getmininginfo");

        use std::time::Instant;

        // This avoids multiple storage lookups for height, tip_header, chain_info
        thread_local! {
            static CACHED_MINING_INFO: std::cell::RefCell<(
                Option<Value>,
                Instant,
                Option<u64>,
                Option<usize>,
                Option<usize>,
            )> = {
                std::cell::RefCell::new((None, Instant::now(), None, None, None))
            };
        }

        // Check if we should refresh (cache miss, expired, or chain advanced)
        let current_height = if let Some(ref storage) = self.storage {
            storage.chain().get_height().ok().flatten().unwrap_or(0)
        } else {
            0
        };

        let pooled_now = self.mempool.as_ref().map(|mempool| mempool.size());
        let pool_id = self
            .mempool
            .as_ref()
            .map(|mempool| Arc::as_ptr(mempool) as usize);
        let should_refresh = CACHED_MINING_INFO.with(|cache| {
            let cache = cache.borrow();
            cache.0.is_none()
                || cache.1.elapsed() >= CACHE_REFRESH_TIP
                || cache.2 != Some(current_height)
                || cache.3 != pool_id
                || cache.4 != pooled_now
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

            // An empty pool is not a mined block. A non-empty pool is the coinbase plus those transactions.
            let (currentblocksize, currentblockweight, currentblocktx) =
                self.template_field_snapshot(blocks);

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
                *cache = (
                    Some(value.clone()),
                    Instant::now(),
                    Some(current_height),
                    pool_id,
                    pooled_now,
                );
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

        // One table scan when the table is the chain. A checkpoint is priced
        // from the inputs the pool actually spends.
        let pooled = self.pooled_transactions();
        let utxo_set = self.template_utxos(&pooled)?;
        let mempool_txs: Vec<Transaction> = self.mempool_transactions_priced(&utxo_set)?;

        // 4. Extract coinbase parameters from request or use defaults.
        // The script must begin with this block's height and be 2 to 100 bytes.
        let mut coinbase_script = self.extract_coinbase_script(params).unwrap_or_default();
        if !(2..=100).contains(&coinbase_script.len())
            || !Self::coinbase_script_starts_with_height(&coinbase_script, template_block_height)
        {
            coinbase_script =
                blvm_protocol::bip_validation::encode_bip34_coinbase_script(template_block_height);
        }
        let coinbase_address = self.extract_coinbase_address(params)?;

        let network = self.consensus_network()?;
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
            .unwrap_or_else(|_| template.target.gbt_target_hex());

        // Convert bits to hex (8 characters)
        let bits_hex = format!("{:08x}", template.header.bits);

        // Convert transactions to JSON array
        let transactions_json: Vec<Value> = template
            .transactions
            .iter()
            .map(|tx| self.transaction_to_json(tx))
            .collect();

        // Calculate coinbase value (subsidy + fees)
        let coinbase_value = self.calculate_coinbase_value(template, height)?;

        // Get active rules (BIP 9 feature flags)
        let rules = self.get_active_rules(height)?;

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
        if let Some(script) = Self::witness_commitment_script(&template.coinbase_tx) {
            body["default_witness_commitment"] = json!(hex::encode(script));
        }
        if include_coinbasetxn {
            let data = hex::encode(Self::coinbase_txn_bytes(&template.coinbase_tx));
            body["coinbasetxn"] = json!({ "data": data });
        }
        Ok(body)
    }

    /// Last BIP141 commitment script on a coinbase, if one was appended.
    fn witness_commitment_script(tx: &Transaction) -> Option<&[u8]> {
        tx.outputs.iter().rev().find_map(|output| {
            let script: &[u8] = output.script_pubkey.as_ref();
            (script.len() >= 38
                && script[0] == 0x6a
                && script[1] == 0x24
                && script[2..6] == [0xaa, 0x21, 0xa9, 0xed])
            .then_some(script)
        })
    }

    /// BIP141 coinbase for `coinbasetxn`. The commitment was hashed with 32 zero
    /// bytes, so that reserved value is part of the published transaction.
    fn coinbase_txn_bytes(tx: &Transaction) -> Vec<u8> {
        use blvm_protocol::serialization::serialize_transaction_with_witness;

        let committed = Self::witness_commitment_script(tx).is_some();
        if committed {
            let reserved: Vec<Witness> = tx.inputs.iter().map(|_| vec![vec![0u8; 32]]).collect();
            if let Ok(bytes) = serialize_transaction_with_witness(tx, &reserved) {
                return bytes;
            }
        }
        serialize_transaction(tx)
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

    fn pooled_transactions(&self) -> Vec<Transaction> {
        self.mempool
            .as_ref()
            .map(|mempool| mempool.get_transactions())
            .unwrap_or_default()
    }

    /// Coins the template needs. The full table when that table is the chain.
    /// Otherwise only the prevouts of `txs`, read from the checkpoint.
    fn template_utxos(&self, txs: &[Transaction]) -> RpcResult<UtxoSet> {
        let Some(storage) = self.storage.as_ref() else {
            return Ok(UtxoSet::default());
        };
        match storage.open_engine_resume_snapshot() {
            Ok(Some(_)) => Ok(self.utxos_for_prevouts(txs)),
            Ok(None) => self.get_utxo_set(),
            Err(err) => Err(RpcError::internal_error(format!(
                "Failed to open UTXO snapshot: {err}"
            ))),
        }
    }

    /// Set passed into block connect. Empty when a checkpoint is the chain, so
    /// connect writes that checkpoint instead of replacing it with a fragment.
    fn connect_utxos(&self) -> RpcResult<UtxoSet> {
        let Some(storage) = self.storage.as_ref() else {
            return Ok(UtxoSet::default());
        };
        match storage.open_engine_resume_snapshot() {
            Ok(Some(_)) => Ok(UtxoSet::default()),
            Ok(None) => self.get_utxo_set(),
            Err(err) => Err(RpcError::internal_error(format!(
                "Failed to open UTXO snapshot: {err}"
            ))),
        }
    }

    fn get_mempool_transactions(&self) -> RpcResult<Vec<Transaction>> {
        let pooled = self.pooled_transactions();
        let utxo_set = self.template_utxos(&pooled)?;
        self.mempool_transactions_priced(&utxo_set)
    }

    fn mempool_transactions_priced(&self, utxo_set: &UtxoSet) -> RpcResult<Vec<Transaction>> {
        if let Some(ref mempool) = self.mempool {
            let limit = mempool.size();
            Ok(mempool.get_prioritized_transactions(limit, utxo_set))
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

    fn utxos_for_prevouts(&self, txs: &[Transaction]) -> UtxoSet {
        let mut set = UtxoSet::default();
        let Some(storage) = self.storage.as_ref() else {
            return set;
        };
        for tx in txs {
            for input in &tx.inputs {
                if set.contains_key(&input.prevout) {
                    continue;
                }
                if let Ok(Some(utxo)) = storage.read_chain_prevout(&input.prevout) {
                    set.insert(input.prevout, Arc::new(utxo));
                }
            }
        }
        set
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
        // Default: empty script. The template replaces a script that skips the height.
        Some(vec![])
    }

    /// BIP34: height 0 is `OP_0`, heights 1 through 16 are `OP_1` through `OP_16`,
    /// and a taller height is the length byte plus the minimal little-endian magnitude.
    fn coinbase_script_starts_with_height(script: &[u8], height: u64) -> bool {
        if height == 0 {
            return script.first().copied() == Some(0x00);
        }
        if (1..=16).contains(&height) {
            return script.first().copied() == Some(0x50 + height as u8);
        }
        let prefix = blvm_protocol::bip_validation::encode_bip34_coinbase_script(height);
        script.starts_with(&prefix)
    }

    fn extract_coinbase_address(&self, params: &Value) -> RpcResult<ByteString> {
        if let Some(template_request) = params.get(0) {
            if let Some(addr) = template_request.get("coinbaseaddress") {
                if let Some(addr_str) = addr.as_str() {
                    return address_string_to_script_pubkey(addr_str)
                        .map_err(|e| RpcError::invalid_params(format!("coinbaseaddress: {e}")));
                }
            }
        }
        Ok(vec![])
    }

    fn transaction_to_json(&self, tx: &Transaction) -> Value {
        use blvm_protocol::block::calculate_tx_id;

        let witnesses = self.mempool.as_ref().and_then(|mempool| {
            let txid = calculate_tx_id(tx);
            mempool.get_transaction_witnesses(&txid)
        });
        let wire = crate::rpc::txwire::tx_wire(tx, witnesses.as_deref());
        let fee = self.calculate_transaction_fee(tx);
        let sigops = self.count_sigops(tx, witnesses.as_deref());
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
        let Ok(utxo_set) = self.template_utxos(std::slice::from_ref(tx)) else {
            return 0;
        };
        mempool.calculate_transaction_fee(tx, &utxo_set)
    }

    fn count_sigops(&self, tx: &Transaction, witnesses: Option<&[Witness]>) -> u32 {
        #[cfg(feature = "sigop")]
        {
            use blvm_protocol::UTXO;
            use blvm_protocol::sigop::get_transaction_sigop_cost_with_utxos;

            let owned: Vec<Option<UTXO>> = tx
                .inputs
                .iter()
                .map(|input| {
                    if let Some(utxo) = self.storage.as_ref().and_then(|storage| {
                        storage.read_chain_prevout(&input.prevout).ok().flatten()
                    }) {
                        return Some(utxo);
                    }
                    // An output created by a pooled parent is in this template too.
                    let parent = self
                        .mempool
                        .as_ref()
                        .and_then(|mempool| mempool.get_transaction(&input.prevout.hash))?;
                    let output = parent.outputs.get(input.prevout.index as usize)?;
                    Some(UTXO {
                        value: output.value,
                        script_pubkey: output.script_pubkey.clone().into(),
                        height: 0,
                        is_coinbase: false,
                    })
                })
                .collect();
            let refs: Vec<Option<&UTXO>> = owned.iter().map(|utxo| utxo.as_ref()).collect();
            if let Ok(cost) =
                get_transaction_sigop_cost_with_utxos(tx, &refs, witnesses, 0x01 | 0x800)
            {
                return u32::try_from(cost).unwrap_or(u32::MAX);
            }
            0
        }
        #[cfg(not(feature = "sigop"))]
        {
            let _ = witnesses;
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

    fn calculate_coinbase_value(
        &self,
        template: &BlockTemplate,
        _height: Natural,
    ) -> RpcResult<u64> {
        // Regtest halves every 150 blocks. Other networks stay on 210,000.
        let network = self.consensus_network()?;
        let subsidy = self
            .consensus
            .get_block_subsidy_for_network(template.height, network) as u64;

        // Calculate total fees from transactions
        let fees: u64 = template
            .transactions
            .iter()
            .map(|tx| self.calculate_transaction_fee(tx))
            .sum();

        Ok(subsidy + fees)
    }

    /// Active BIP9-style `rules` for `getblocktemplate`, aligned with [`ForkActivationTable`]
    /// (`blvm-consensus::activation`) and shared activation constants.
    fn get_active_rules(&self, height: Natural) -> RpcResult<Vec<String>> {
        let network = self.consensus_network()?;
        // Match `ForkActivationTable::from_network`. Regtest CSV starts at height 1.
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
        Ok(rules)
    }

    fn consensus_network(&self) -> RpcResult<ConsensusNetwork> {
        crate::storage::resolve_consensus_network(
            self.protocol_engine.as_deref(),
            self.storage.as_deref(),
        )
        .map_err(|err| RpcError::internal_error(err.to_string()))
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
        let uses_snapshot = storage
            .open_engine_resume_snapshot()
            .map(|snapshot| snapshot.is_some())
            .unwrap_or(false);
        let mut connect_utxo = if uses_snapshot {
            UtxoSet::default()
        } else {
            storage
                .utxos()
                .get_all_utxos()
                .map_err(|e| RpcError::internal_error(format!("Failed to load UTXO set: {e}")))?
        };

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
            // The table was loaded once, into `connect_utxo`. Pricing the pool
            // through `template_utxos` would load it again on every block.
            let pooled = self.pooled_transactions();
            let snapshot_prevouts = if uses_snapshot {
                Some(self.utxos_for_prevouts(&pooled))
            } else {
                None
            };
            let template_utxos = snapshot_prevouts.as_ref().unwrap_or(&connect_utxo);
            let pool_txs = self.mempool_transactions_priced(template_utxos)?;
            let mempool_witnesses =
                mempool_witnesses_for_template(self.mempool.as_deref(), template_utxos, &pool_txs)
                    .map_err(|e| RpcError::internal_error(format!("generatetoaddress: {e}")))?;

            let outputs = match self.try_commons_gbt_outputs().await? {
                Some(outputs) => outputs,
                None => vec![(0, coinbase_address.clone())],
            };
            let template = self
                .consensus
                .create_block_template_with_outputs(
                    template_utxos,
                    &pool_txs,
                    connect_height,
                    &prev_header,
                    &prev_headers,
                    &coinbase_script,
                    &outputs,
                    ConsensusNetwork::Regtest,
                    Some(&mempool_witnesses),
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

            let mut stacks_by_txid = std::collections::HashMap::new();
            for (tx, stacks) in pool_txs.iter().zip(mempool_witnesses.iter()) {
                if let Some(stacks) = stacks {
                    stacks_by_txid
                        .insert(blvm_protocol::block::calculate_tx_id(tx), stacks.clone());
                }
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
                    } else if let Some(stacks) =
                        stacks_by_txid.get(&blvm_protocol::block::calculate_tx_id(tx))
                    {
                        stacks.clone()
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
                    &mut connect_utxo,
                )
                .map_err(|e| RpcError::internal_error(format!("generatetoaddress: {e}")))?;
            if !accepted {
                return Err(RpcError::internal_error(format!(
                    "generatetoaddress: block rejected at height {connect_height}"
                )));
            }
            let block_hash = storage.blocks().as_ref().get_block_hash(&mined);
            if let Some(ref mempool) = self.mempool {
                let obs = crate::node::fee_estimator::block_observation(
                    &mined,
                    &witnesses,
                    connect_height,
                    block_hash,
                );
                mempool.remove_for_connected_block_at(obs, &mined.transactions);
            }

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
        let tip_height = storage.chain().get_height().map_err(|e| {
            RpcError::internal_error(format!("submitblock: failed to read chain height: {e}"))
        })?;
        let Some(tip_height) = tip_height else {
            return Err(RpcError::invalid_params(
                "submitblock: chain not initialized (no tip)".to_string(),
            ));
        };
        let parent_hash = block.header.prev_block_hash;
        let parent_stored = storage
            .blocks()
            .get_header(&parent_hash)
            .map_err(|e| {
                RpcError::internal_error(format!("submitblock: failed to read parent header: {e}"))
            })?
            .is_some();
        if !parent_stored {
            return Err(RpcError::invalid_params(
                "submitblock: prev_block_hash is not a stored header".to_string(),
            ));
        }
        let block_hash = storage.blocks().get_block_hash(&block);
        if storage
            .blocks()
            .get_header(&block_hash)
            .ok()
            .flatten()
            .is_some()
        {
            return Err(RpcError::invalid_params(
                "submitblock: block header is already stored".to_string(),
            ));
        }
        let protocol = self.protocol_engine.as_ref().ok_or_else(|| {
            RpcError::internal_error(
                "submitblock: protocol engine is required to connect the block".to_string(),
            )
        })?;
        let parent_height = storage
            .blocks()
            .get_height_by_hash(&parent_hash)
            .map_err(|e| {
                RpcError::internal_error(format!("submitblock: failed to read parent height: {e}"))
            })?;
        // A sibling's parent is stored and is not the tip. Connect one past
        // that parent, not one past the current tip.
        let connect_height = parent_height.unwrap_or(tip_height).saturating_add(1);
        let mut utxo = self.connect_utxos()?;
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
            let obs = crate::node::fee_estimator::block_observation(
                &block,
                &witnesses,
                connect_height,
                block_hash,
            );
            mempool.remove_for_connected_block_at(obs, &block.transactions);
        }
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

    /// The attached fee estimator, when the mempool has one.
    pub fn fee_estimator(&self) -> Option<Arc<crate::node::fee_estimator::FeeEstimator>> {
        self.mempool.as_ref().and_then(|pool| pool.fee_estimator())
    }

    /// Estimate smart fee rate.
    ///
    /// Params: `[conf_target` (default 6), `estimate_mode` (default `"conservative"`),
    /// `fee_rate_estimator` (default `"none"`), `verbosity` (default 0)].
    /// Mode and estimator names are case-insensitive. `"none"` is the combined column.
    pub async fn estimate_smart_fee(&self, params: &Value) -> RpcResult<Value> {
        use crate::node::fee_estimator::{FeeMode, FeeUnavailable};

        debug!("RPC: estimatesmartfee");

        let conf_target = crate::rpc::params::param_u64_default(params, 0, 6);
        let estimate_mode = crate::rpc::params::param_str(params, 1).unwrap_or("conservative");
        let mode = match estimate_mode.to_ascii_lowercase().as_str() {
            "unset" | "conservative" => FeeMode::Conservative,
            "economical" => FeeMode::Economical,
            _ => {
                return Err(RpcError::invalid_params(format!(
                    "Invalid estimate_mode: {estimate_mode}. Must be 'unset', 'economical', or 'conservative'"
                )));
            }
        };
        let choice = fee_rate_estimator_param(params)?;
        let verbosity = verbosity_param(params)?;

        if !(1..=1008).contains(&conf_target) {
            return Err(RpcError::invalid_params(format!(
                "Invalid conf_target {conf_target}. Must be between 1 and 1008"
            )));
        }

        let Some(estimator) = self.mempool.as_ref().and_then(|pool| pool.fee_estimator()) else {
            return Ok(fee_estimate_failure(
                conf_target,
                verbosity,
                FeeUnavailable::Disabled,
                None,
            ));
        };
        if let FeeRateChoice::Named(name) = &choice {
            if !estimator.has_estimator(name) {
                return Err(RpcError::invalid_params(format!(
                    "Invalid fee_rate_estimator: {name}. Must be 'none', 'history', 'mempool', or a registered estimator"
                )));
            }
        }
        let labels = estimator.estimator_labels();
        let selection = Some((labels.0.as_str(), labels.1.as_str(), labels.2));

        let quote = match choice {
            FeeRateChoice::View(crate::node::fee_estimator::SourceView::Combined) => {
                estimator
                    .quote(
                        conf_target as u32,
                        mode,
                        crate::node::fee_estimator::SourceView::Combined,
                    )
                    .await
            }
            FeeRateChoice::View(view) => {
                let table = match estimator.table().await {
                    Ok(table) => table,
                    Err(err) => {
                        return Ok(fee_estimate_failure(conf_target, verbosity, err, selection));
                    }
                };
                table.quote(conf_target as u32, mode, view)
            }
            FeeRateChoice::Named(name) => {
                estimator.quote_named(&name, conf_target as u32, mode).await
            }
        };
        let table = match estimator.table().await {
            Ok(table) => table,
            Err(err) => return Ok(fee_estimate_failure(conf_target, verbosity, err, selection)),
        };
        match quote {
            Ok(quote) => Ok(fee_estimate_success(&quote, &table, verbosity, selection)),
            Err(err) => Ok(fee_estimate_failure(conf_target, verbosity, err, selection)),
        }
    }

    /// `native` plus every registered estimator.
    pub fn list_fee_estimators(&self) -> RpcResult<Value> {
        debug!("RPC: listfeeestimators");
        let Some(estimator) = self.fee_estimator() else {
            return Ok(json!([{
                "name": "native",
                "module": null,
                "description": "built-in",
                "resolved": true,
                "fresh": false,
                "last_submit_age_secs": null,
                "active": false
            }]));
        };
        let rows = estimator
            .fee_estimator_rows()
            .into_iter()
            .map(|row| {
                json!({
                    "name": row.name,
                    "module": row.module,
                    "description": row.description,
                    "resolved": row.resolved,
                    "fresh": row.fresh,
                    "last_submit_age_secs": row.last_submit_age_secs,
                    "active": row.active,
                })
            })
            .collect::<Vec<_>>();
        Ok(Value::Array(rows))
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

enum FeeRateChoice {
    View(crate::node::fee_estimator::SourceView),
    Named(String),
}

fn fee_rate_estimator_param(params: &Value) -> RpcResult<FeeRateChoice> {
    use crate::node::fee_estimator::SourceView;
    match params.get(2) {
        None | Some(Value::Null) => Ok(FeeRateChoice::View(SourceView::Combined)),
        Some(value) => {
            let name = value.as_str().ok_or_else(|| {
                RpcError::invalid_params(
                    "Invalid fee_rate_estimator. Must be 'none', 'history', 'mempool', or a registered estimator"
                        .to_string(),
                )
            })?;
            match name.to_ascii_lowercase().as_str() {
                "none" => Ok(FeeRateChoice::View(SourceView::Combined)),
                "history" => Ok(FeeRateChoice::View(SourceView::History)),
                "mempool" => Ok(FeeRateChoice::View(SourceView::Mempool)),
                other => Ok(FeeRateChoice::Named(other.to_string())),
            }
        }
    }
}

fn verbosity_param(params: &Value) -> RpcResult<u64> {
    match params.get(3) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => {
            let verbosity = value.as_u64().ok_or_else(|| {
                RpcError::invalid_params("Invalid verbosity. Must be 0, 1, or 2".to_string())
            })?;
            if verbosity > 2 {
                return Err(RpcError::invalid_params(format!(
                    "Invalid verbosity {verbosity}. Must be 0, 1, or 2"
                )));
            }
            Ok(verbosity)
        }
    }
}

fn fee_estimate_success(
    quote: &crate::node::fee_estimator::FeeQuote,
    table: &crate::node::fee_estimator::FeeTable,
    verbosity: u64,
    selection: Option<(&str, &str, bool)>,
) -> Value {
    use crate::node::fee_estimator::FeeSource;
    let mut value = json!({
        "feerate": quote.sat_per_kvb as f64 / 100_000_000.0,
        "blocks": quote.blocks,
    });
    if verbosity >= 1 {
        value["source"] = json!(match quote.source {
            FeeSource::History => "history",
            FeeSource::Mempool => "mempool",
            FeeSource::Min => "min",
            FeeSource::External => "external",
            FeeSource::ExternalMin => "external_min",
        });
        value["mempool_healthy"] = json!(table.mempool_healthy);
        if let Some((requested, active, degraded)) = selection {
            value["requested_estimator"] = json!(requested);
            value["active_estimator"] = json!(active);
            value["degraded"] = json!(degraded);
        }
    }
    if verbosity >= 2 {
        value["tip_height"] = json!(table.tip_height);
        value["tip_hash"] = json!(crate::storage::hashing::hash_to_rpc_hex(&table.tip_hash));
        value["table_age_secs"] = json!(table.generated_at.elapsed().as_secs_f64());
        value["health"] = json!({
            "window": table.health_window,
            "ratio": table.health_ratio,
            "required_ratio": table.health_required_ratio,
        });
    }
    value
}

fn fee_estimate_failure(
    blocks: u64,
    verbosity: u64,
    err: crate::node::fee_estimator::FeeUnavailable,
    selection: Option<(&str, &str, bool)>,
) -> Value {
    use crate::node::fee_estimator::FeeUnavailable;
    let mut value = json!({
        "errors": ["Insufficient data or no feerate found"],
        "blocks": blocks,
    });
    if verbosity >= 1 {
        value["reason"] = json!(match err {
            FeeUnavailable::WarmingUp => "warming_up",
            FeeUnavailable::MempoolUnhealthy => "mempool_unhealthy",
            FeeUnavailable::EmptyMempool => "empty_mempool",
            FeeUnavailable::Disabled => "disabled",
            FeeUnavailable::ExternalStale => "external_stale",
        });
        if let Some((requested, active, degraded)) = selection {
            value["requested_estimator"] = json!(requested);
            value["active_estimator"] = json!(active);
            value["degraded"] = json!(degraded);
        }
    }
    value
}

impl Default for MiningRpc {
    fn default() -> Self {
        Self::new()
    }
}

fn spends_witness_utxo(tx: &Transaction, utxo_set: &UtxoSet) -> bool {
    use blvm_consensus::witness::is_standard_witness_program;
    tx.inputs.iter().any(|input| {
        utxo_set
            .get(&input.prevout)
            .is_some_and(|utxo| is_standard_witness_program(utxo.script_pubkey.as_ref()))
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
            Ok(Some(vec![Vec::new(); tx.inputs.len()]))
        })
        .collect()
}

#[cfg(test)]
#[path = "mining_tests.rs"]
mod tests;
