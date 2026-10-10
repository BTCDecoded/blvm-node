//! Mempool RPC Methods
//!
//! Implements mempool-related JSON-RPC methods:
//! - getmempoolinfo
//! - getrawmempool
//! - savemempool

use crate::node::mempool::MempoolManager;
use crate::rpc::errors::RpcResult;
use crate::rpc::params::{param_bool_default, param_str, param_str_required};
use crate::storage::Storage;
use crate::utils::current_timestamp;
use blvm_protocol::Hash;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use tracing::debug;

/// Walk parent or child edges already stored on the pool. `descendants` follows children.
fn follow_pool_edges(mempool: &MempoolManager, roots: Vec<Hash>, descendants: bool) -> Vec<Hash> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = roots;
    while let Some(hash) = stack.pop() {
        if !seen.insert(hash) {
            continue;
        }
        let next = if descendants {
            mempool.descendant_hashes(&hash)
        } else {
            mempool.dependency_hashes(&hash)
        };
        stack.extend(next);
        out.push(hash);
    }
    out
}

/// Mempool RPC methods
#[derive(Clone)]
pub struct MempoolRpc {
    mempool: Option<Arc<MempoolManager>>,
    storage: Option<Arc<Storage>>,
}

impl MempoolRpc {
    /// Create a new mempool RPC handler
    pub fn new() -> Self {
        Self {
            mempool: None,
            storage: None,
        }
    }

    /// Create with dependencies
    pub fn with_dependencies(mempool: Arc<MempoolManager>, storage: Arc<Storage>) -> Self {
        Self {
            mempool: Some(mempool),
            storage: Some(storage),
        }
    }

    /// Get mempool information
    ///
    /// Params: []
    pub async fn getmempoolinfo(&self, _params: &Value) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getmempoolinfo");

        if let Some(ref mempool) = self.mempool {
            let size = mempool.size();

            // This is much faster for large mempools (approximate: avg tx size ~250 bytes)
            let bytes = if size == 0 {
                0
            } else {
                // Fast path: estimate from size (good enough for RPC)
                // For exact calculation, would need to serialize all, but that's expensive
                size * 250 // Approximate average transaction size
            };

            Ok(json!({
                "loaded": true,
                "size": size,
                "bytes": bytes,
                "usage": bytes,
                "maxmempool": 300000000,
                "mempoolminfee": 0.00001000,
                "minrelaytxfee": 0.00001000
            }))
        } else {
            // Graceful degradation: return empty mempool info when mempool unavailable
            tracing::debug!(
                "getmempoolinfo called but mempool not available, returning empty mempool"
            );
            Ok(json!({
                "loaded": false,
                "size": 0,
                "bytes": 0,
                "usage": 0,
                "maxmempool": 300000000,
                "mempoolminfee": 0.00001000,
                "minrelaytxfee": 0.00001000,
                "note": "Mempool not available - returning empty mempool"
            }))
        }
    }

    /// Get all transaction IDs in mempool
    ///
    /// Params: [verbose (optional, default: false)]
    pub async fn getrawmempool(&self, params: &Value) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getrawmempool");

        let verbose = param_bool_default(params, 0, false);

        if let Some(ref mempool) = self.mempool {
            let transactions = mempool.get_transactions();
            use blvm_protocol::block::calculate_tx_id;
            use blvm_protocol::serialization::transaction::serialize_transaction;

            if verbose {
                let mut result = serde_json::Map::new();

                let utxo_set = if let (Some(_mempool), Some(storage)) =
                    (self.mempool.as_ref(), self.storage.as_ref())
                {
                    Some(storage.utxos().get_all_utxos().unwrap_or_default())
                } else {
                    None
                };

                for tx in transactions {
                    let txid = calculate_tx_id(&tx);
                    let txid_hex = crate::storage::hashing::hash_to_rpc_hex(&txid);
                    let witnesses = mempool.get_transaction_witnesses(&txid);
                    let wtxid = crate::rpc::txwire::tx_wire(&tx, witnesses.as_deref()).hash_hex;
                    let size = serialize_transaction(&tx).len();

                    let fee_btc = if let (Some(mempool), Some(utxo_set)) =
                        (self.mempool.as_ref(), utxo_set.as_ref())
                    {
                        mempool.calculate_transaction_fee(&tx, utxo_set) as f64 / 100_000_000.0
                    } else {
                        0.0
                    };
                    let ancestors = follow_pool_edges(
                        mempool,
                        mempool.dependency_hashes(&txid),
                        false,
                    );
                    let descendants = follow_pool_edges(
                        mempool,
                        mempool.descendant_hashes(&txid),
                        true,
                    );
                    let fee_sats = |hash: &blvm_protocol::Hash| -> f64 {
                        let Some(ref set) = utxo_set else {
                            return 0.0;
                        };
                        mempool
                            .get_transaction(hash)
                            .map(|tx| mempool.calculate_transaction_fee(&tx, set) as f64 / 100_000_000.0)
                            .unwrap_or(0.0)
                    };
                    let size_of = |hash: &blvm_protocol::Hash| -> usize {
                        mempool
                            .get_transaction(hash)
                            .map(|tx| serialize_transaction(&tx).len())
                            .unwrap_or(0)
                    };
                    let ancestor_fees = fee_btc + ancestors.iter().map(fee_sats).sum::<f64>();
                    let descendant_fees = fee_btc + descendants.iter().map(fee_sats).sum::<f64>();
                    let ancestor_size = size + ancestors.iter().map(size_of).sum::<usize>();
                    let descendant_size = size + descendants.iter().map(size_of).sum::<usize>();
                    let depends: Vec<String> = ancestors
                        .iter()
                        .map(crate::storage::hashing::hash_to_rpc_hex)
                        .collect();
                    let spentby: Vec<String> = descendants
                        .iter()
                        .map(crate::storage::hashing::hash_to_rpc_hex)
                        .collect();
                    result.insert(txid_hex, json!({
                        "size": size,
                        "fee": fee_btc,
                        "modifiedfee": fee_btc,
                        "time": mempool.accepted_at(&txid),
                        "height": -1,
                        "descendantcount": 1 + descendants.len(),
                        "descendantsize": descendant_size,
                        "descendantfees": descendant_fees,
                        "ancestorcount": 1 + ancestors.len(),
                        "ancestorsize": ancestor_size,
                        "ancestorfees": ancestor_fees,
                        "wtxid": wtxid,
                        "fees": {
                            "base": fee_btc,
                            "modified": fee_btc,
                            "ancestor": ancestor_fees,
                            "descendant": descendant_fees
                        },
                        "depends": depends,
                        "spentby": spentby,
                        "bip125-replaceable": blvm_protocol::mempool::signals_rbf(&tx)
                    }));
                }
                Ok(json!(result))
            } else {
                let txids: Vec<String> = transactions
                    .iter()
                    .map(|tx| crate::storage::hashing::hash_to_rpc_hex(&calculate_tx_id(tx)))
                    .collect();
                Ok(json!(txids))
            }
        } else if verbose {
            Ok(json!({}))
        } else {
            Ok(json!([]))
        }
    }

    /// Save mempool to disk (for node restart persistence)
    ///
    /// Params: []
    pub async fn savemempool(&self, _params: &Value) -> RpcResult<Value> {
        debug!("RPC: savemempool");

        if let Some(mempool) = &self.mempool {
            use crate::utils::env_or_default;
            let data_dir = env_or_default("DATA_DIR", "data");
            let mempool_path = std::path::Path::new(&data_dir).join("mempool.dat");

            if let Some(parent) = mempool_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    crate::rpc::errors::RpcError::internal_error(format!(
                        "Failed to create mempool directory: {e}"
                    ))
                })?;
            }

            // Arc implements Deref, so we can call methods directly
            if let Err(e) = mempool.save_to_disk(&mempool_path) {
                return Err(crate::rpc::errors::RpcError::internal_error(format!(
                    "Failed to save mempool: {e}"
                )));
            }

            Ok(Value::Null)
        } else {
            Err(crate::rpc::errors::RpcError::internal_error(
                "Mempool not initialized".to_string(),
            ))
        }
    }

    /// Get mempool ancestors for a transaction
    ///
    /// Params: ["txid", verbose (optional, default: false)]
    pub async fn getmempoolancestors(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: getmempoolancestors");

        let txid = param_str_required(params, 0, "getmempoolancestors")?;

        let verbose = param_bool_default(params, 1, false);

        let hash = crate::storage::hashing::hash_from_rpc_hex(&txid).map_err(|e| {
            crate::rpc::errors::RpcError::invalid_hash_format(&txid, Some(32), Some(&e))
        })?;

        if let Some(ref mempool) = self.mempool {
            // Find ancestors: transactions that this transaction depends on (spends their outputs)
            let ancestors = self.get_ancestors(mempool, &hash);

            if verbose {
                // Return detailed ancestor information
                let mut result = serde_json::Map::new();
                for ancestor_hash in ancestors {
                    if let Some(ancestor_tx) = mempool.get_transaction(&ancestor_hash) {
                        let ancestor_txid =
                            crate::storage::hashing::hash_to_rpc_hex(&ancestor_hash);
                        result.insert(
                            ancestor_txid,
                            self.build_mempool_entry_json(mempool, &ancestor_hash, &ancestor_tx),
                        );
                    }
                }
                Ok(json!(result))
            } else {
                // Return just transaction IDs
                let txids: Vec<String> = ancestors
                    .iter()
                    .map(crate::storage::hashing::hash_to_rpc_hex)
                    .collect();
                Ok(json!(txids))
            }
        } else if verbose {
            Ok(json!({}))
        } else {
            Ok(json!([]))
        }
    }

    /// Get mempool descendants for a transaction
    ///
    /// Params: ["txid", verbose (optional, default: false)]
    pub async fn getmempooldescendants(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: getmempooldescendants");

        let txid = param_str_required(params, 0, "getmempooldescendants")?;

        let verbose = param_bool_default(params, 1, false);

        let hash = crate::storage::hashing::hash_from_rpc_hex(&txid).map_err(|e| {
            crate::rpc::errors::RpcError::invalid_hash_format(&txid, Some(32), Some(&e))
        })?;

        if let Some(ref mempool) = self.mempool {
            // Find descendants by checking which transactions spend outputs created by this transaction which transactions spend outputs created by this transaction which transactions spend outputs created by this transaction
            let mut descendants = Vec::new();

            if let Some(tx) = mempool.get_transaction(&hash) {
                // Get all output outpoints from this transaction
                let mut output_outpoints = Vec::new();
                for (idx, _output) in tx.outputs.iter().enumerate() {
                    output_outpoints.push(blvm_protocol::OutPoint {
                        hash,
                        index: idx as u32,
                    });
                }

                // Find transactions that spend these outputs
                use blvm_protocol::block::calculate_tx_id;
                let transactions = mempool.get_transactions();
                for descendant_tx in transactions {
                    let descendant_hash = calculate_tx_id(&descendant_tx);
                    for input in &descendant_tx.inputs {
                        if output_outpoints.contains(&input.prevout) {
                            descendants.push(descendant_hash);
                            break;
                        }
                    }
                }
            }

            if verbose {
                // Return detailed descendant information
                let mut result = serde_json::Map::new();
                for descendant_hash in descendants {
                    if let Some(descendant_tx) = mempool.get_transaction(&descendant_hash) {
                        let descendant_txid =
                            crate::storage::hashing::hash_to_rpc_hex(&descendant_hash);
                        result.insert(
                            descendant_txid,
                            self.build_mempool_entry_json(
                                mempool,
                                &descendant_hash,
                                &descendant_tx,
                            ),
                        );
                    }
                }
                Ok(json!(result))
            } else {
                // Return just transaction IDs
                let txids: Vec<String> = descendants
                    .iter()
                    .map(crate::storage::hashing::hash_to_rpc_hex)
                    .collect();
                Ok(json!(txids))
            }
        } else if verbose {
            Ok(json!({}))
        } else {
            Ok(json!([]))
        }
    }

    /// Get specific mempool entry
    ///
    /// Params: ["txid"]
    pub async fn getmempoolentry(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: getmempoolentry");

        let txid = param_str_required(params, 0, "getmempoolentry")?;

        let hash = crate::storage::hashing::hash_from_rpc_hex(&txid).map_err(|e| {
            crate::rpc::errors::RpcError::invalid_hash_format(&txid, Some(32), Some(&e))
        })?;

        if let Some(ref mempool) = self.mempool {
            if let Some(tx) = mempool.get_transaction(&hash) {
                Ok(self.build_mempool_entry_json(mempool, &hash, &tx))
            } else {
                Err(crate::rpc::errors::RpcError::invalid_params(format!(
                    "Transaction {txid} not found in mempool"
                )))
            }
        } else {
            Err(crate::rpc::errors::RpcError::internal_error(
                "Mempool not initialized".to_string(),
            ))
        }
    }

    /// Build a Core-shaped mempool entry with package fee totals.
    fn build_mempool_entry_json(
        &self,
        mempool: &MempoolManager,
        hash: &Hash,
        tx: &blvm_protocol::Transaction,
    ) -> Value {
        use blvm_protocol::serialization::transaction::serialize_transaction;

        let size = serialize_transaction(tx).len();
        let ancestors = self.get_ancestors(mempool, hash);
        let descendants = self.get_descendants(mempool, hash);

        let ancestor_size: usize = ancestors
            .iter()
            .filter_map(|h| mempool.get_transaction(h))
            .map(|tx| serialize_transaction(&tx).len())
            .sum();
        let descendant_size: usize = descendants
            .iter()
            .filter_map(|h| mempool.get_transaction(h))
            .map(|tx| serialize_transaction(&tx).len())
            .sum();

        let base_fee_btc = self.mempool_tx_fee_btc(mempool, tx);
        let modified_fee_btc = self.mempool_modified_fee_btc(mempool, hash, tx);
        let ancestor_fees_btc =
            self.sum_mempool_modified_fees_btc(mempool, &ancestors) + modified_fee_btc;
        let descendant_fees_btc =
            self.sum_mempool_modified_fees_btc(mempool, &descendants) + modified_fee_btc;

        let witnesses = mempool.get_transaction_witnesses(hash);
        let wtxid = crate::rpc::txwire::tx_wire(tx, witnesses.as_deref()).hash_hex;

        json!({
            "size": size,
            "fee": base_fee_btc,
            "modifiedfee": modified_fee_btc,
            "time": current_timestamp(),
            "height": -1,
            "descendantcount": descendants.len() + 1,
            "descendantsize": descendant_size + size,
            "descendantfees": descendant_fees_btc,
            "ancestorcount": ancestors.len() + 1,
            "ancestorsize": ancestor_size + size,
            "ancestorfees": ancestor_fees_btc,
            "wtxid": wtxid,
            "fees": {
                "base": base_fee_btc,
                "modified": modified_fee_btc,
                "ancestor": ancestor_fees_btc,
                "descendant": descendant_fees_btc
            },
            "depends": ancestors
                .iter()
                .map(crate::storage::hashing::hash_to_rpc_hex)
                .collect::<Vec<_>>(),
            "spentby": descendants
                .iter()
                .map(crate::storage::hashing::hash_to_rpc_hex)
                .collect::<Vec<_>>(),
            "bip125-replaceable": false
        })
    }

    fn mempool_tx_fee_sat(&self, mempool: &MempoolManager, tx: &blvm_protocol::Transaction) -> u64 {
        if let Some(ref storage) = self.storage {
            let utxo_set = storage.utxos().get_all_utxos().unwrap_or_default();
            mempool.calculate_transaction_fee(tx, &utxo_set)
        } else {
            0
        }
    }

    fn mempool_tx_fee_btc(&self, mempool: &MempoolManager, tx: &blvm_protocol::Transaction) -> f64 {
        self.mempool_tx_fee_sat(mempool, tx) as f64 / 100_000_000.0
    }

    fn mempool_modified_fee_sat(
        &self,
        mempool: &MempoolManager,
        hash: &Hash,
        tx: &blvm_protocol::Transaction,
    ) -> u64 {
        let base = self.mempool_tx_fee_sat(mempool, tx);
        let delta = mempool.get_fee_delta(hash).max(0) as u64;
        base.saturating_add(delta)
    }

    fn mempool_modified_fee_btc(
        &self,
        mempool: &MempoolManager,
        hash: &Hash,
        tx: &blvm_protocol::Transaction,
    ) -> f64 {
        self.mempool_modified_fee_sat(mempool, hash, tx) as f64 / 100_000_000.0
    }

    fn sum_mempool_modified_fees_btc(&self, mempool: &MempoolManager, hashes: &[Hash]) -> f64 {
        hashes
            .iter()
            .filter_map(|h| {
                mempool
                    .get_transaction(h)
                    .map(|tx| self.mempool_modified_fee_btc(mempool, h, &tx))
            })
            .sum()
    }

    /// Helper: Get ancestors for a transaction
    fn get_ancestors(&self, mempool: &MempoolManager, tx_hash: &Hash) -> Vec<Hash> {
        let mut ancestors = Vec::new();

        if let Some(tx) = mempool.get_transaction(tx_hash) {
            // Find transactions that this transaction depends on (spends their outputs)
            use blvm_protocol::block::calculate_tx_id;
            for input in &tx.inputs {
                // Find transaction that created this output by checking all transactions
                let transactions = mempool.get_transactions();
                for ancestor_tx in transactions {
                    let ancestor_hash = calculate_tx_id(&ancestor_tx);
                    for (idx, _output) in ancestor_tx.outputs.iter().enumerate() {
                        if input.prevout.hash == ancestor_hash
                            && input.prevout.index == idx as u32
                            && !ancestors.contains(&ancestor_hash)
                        {
                            ancestors.push(ancestor_hash);
                        }
                    }
                }
            }
        }

        ancestors
    }

    /// Helper: Get descendants for a transaction
    fn get_descendants(&self, mempool: &MempoolManager, tx_hash: &Hash) -> Vec<Hash> {
        let mut descendants = Vec::new();

        if let Some(tx) = mempool.get_transaction(tx_hash) {
            // Get all output outpoints from this transaction
            let mut output_outpoints = Vec::new();
            for (idx, _output) in tx.outputs.iter().enumerate() {
                output_outpoints.push(blvm_protocol::OutPoint {
                    hash: *tx_hash,
                    index: idx as u32,
                });
            }

            // Find transactions that spend these outputs
            use blvm_protocol::block::calculate_tx_id;
            let transactions = mempool.get_transactions();
            for descendant_tx in transactions {
                let descendant_hash = calculate_tx_id(&descendant_tx);
                for input in &descendant_tx.inputs {
                    if output_outpoints.contains(&input.prevout) {
                        if !descendants.contains(&descendant_hash) {
                            descendants.push(descendant_hash);
                        }
                        break;
                    }
                }
            }
        }

        descendants
    }
}

impl Default for MempoolRpc {
    fn default() -> Self {
        Self::new()
    }
}
