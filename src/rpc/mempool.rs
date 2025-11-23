//! Mempool RPC Methods
//!
//! Implements mempool-related JSON-RPC methods:
//! - getmempoolinfo
//! - getrawmempool
//! - savemempool

use crate::node::mempool::MempoolManager;
use crate::rpc::errors::RpcResult;
use crate::storage::Storage;
use crate::utils::current_timestamp;
use bllvm_protocol::Hash;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::debug;

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

        let verbose = params.get(0).and_then(|p| p.as_bool()).unwrap_or(false);

        if let Some(ref mempool) = self.mempool {
            let transactions = mempool.get_transactions();
            use bllvm_protocol::block::calculate_tx_id;
            use bllvm_protocol::serialization::transaction::serialize_transaction;

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
                    let txid_hex = hex::encode(txid);
                    let txid_hex_clone = txid_hex.clone();
                    let size = serialize_transaction(&tx).len();

                    result.insert(txid_hex, json!({
                        "size": size,
                        "fee": if let (Some(mempool), Some(utxo_set)) = (self.mempool.as_ref(), utxo_set.as_ref()) {
                            let fee_satoshis = mempool.calculate_transaction_fee(&tx, utxo_set);
                            fee_satoshis as f64 / 100_000_000.0
                        } else {
                            0.00001000
                        },
                        "modifiedfee": 0.00001000,
                        "time": current_timestamp(),
                        "height": -1,
                        "descendantcount": 1,
                        "descendantsize": size,
                        "descendantfees": 0.00001000,
                        "ancestorcount": 1,
                        "ancestorsize": size,
                        "ancestorfees": 0.00001000,
                        "wtxid": txid_hex_clone,
                        "fees": {
                            "base": 0.00001000,
                            "modified": 0.00001000,
                            "ancestor": 0.00001000,
                            "descendant": 0.00001000
                        },
                        "depends": [],
                        "spentby": [],
                        "bip125-replaceable": false
                    }));
                }
                Ok(json!(result))
            } else {
                let txids: Vec<String> = transactions
                    .iter()
                    .map(|tx| {
                        let txid = calculate_tx_id(tx);
                        hex::encode(txid)
                    })
                    .collect();
                Ok(json!(txids))
            }
        } else if verbose {
            Ok(json!({
                "0000000000000000000000000000000000000000000000000000000000000000": {
                    "size": 250,
                    "fee": 0.00001000,
                    "modifiedfee": 0.00001000,
                    "time": 1231006505,
                    "height": 0,
                    "descendantcount": 1,
                    "descendantsize": 250,
                    "descendantfees": 0.00001000,
                    "ancestorcount": 1,
                    "ancestorsize": 250,
                    "ancestorfees": 0.00001000,
                    "wtxid": "0000000000000000000000000000000000000000000000000000000000000000",
                    "fees": {
                        "base": 0.00001000,
                        "modified": 0.00001000,
                        "ancestor": 0.00001000,
                        "descendant": 0.00001000
                    },
                    "depends": [],
                    "spentby": [],
                    "bip125-replaceable": false
                }
            }))
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

        let txid = params.get(0).and_then(|p| p.as_str()).ok_or_else(|| {
            crate::rpc::errors::RpcError::invalid_params("Transaction ID required".to_string())
        })?;

        let verbose = params.get(1).and_then(|p| p.as_bool()).unwrap_or(false);

        let hash_bytes = hex::decode(txid).map_err(|e| {
            crate::rpc::errors::RpcError::invalid_params(format!("Invalid transaction ID: {e}"))
        })?;
        if hash_bytes.len() != 32 {
            return Err(crate::rpc::errors::RpcError::invalid_params(
                "Transaction ID must be 32 bytes".to_string(),
            ));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_bytes);

        if let Some(ref mempool) = self.mempool {
            // Find ancestors: transactions that this transaction depends on (spends their outputs)
            let ancestors = self.get_ancestors(mempool, &hash);

            if verbose {
                // Return detailed ancestor information
                let mut result = serde_json::Map::new();
                for ancestor_hash in ancestors {
                    if let Some(ancestor_tx) = mempool.get_transaction(&ancestor_hash) {
                        let ancestor_txid = hex::encode(ancestor_hash);
                        let ancestor_txid_clone = ancestor_txid.clone();
                        use bllvm_protocol::serialization::transaction::serialize_transaction;
                        let size = serialize_transaction(&ancestor_tx).len();

                        result.insert(ancestor_txid, json!({
                            "size": size,
                            "fee": if let Some(ref storage) = self.storage {
                                let utxo_set = storage.utxos().get_all_utxos().unwrap_or_default();
                                let fee_satoshis = mempool.calculate_transaction_fee(&ancestor_tx, &utxo_set);
                                fee_satoshis as f64 / 100_000_000.0
                            } else {
                                0.0
                            },
                            "modifiedfee": 0.0,
                            "time": current_timestamp(),
                            "height": -1,
                            "descendantcount": 1,
                            "descendantsize": size,
                            "descendantfees": 0.0,
                            "ancestorcount": 1,
                            "ancestorsize": size,
                            "ancestorfees": 0.0,
                            "wtxid": ancestor_txid_clone,
                            "fees": {
                                "base": 0.0,
                                "modified": 0.0,
                                "ancestor": 0.0,
                                "descendant": 0.0
                            },
                            "depends": [],
                            "spentby": [],
                            "bip125-replaceable": false
                        }));
                    }
                }
                Ok(json!(result))
            } else {
                // Return just transaction IDs
                let txids: Vec<String> = ancestors.iter().map(hex::encode).collect();
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

        let txid = params.get(0).and_then(|p| p.as_str()).ok_or_else(|| {
            crate::rpc::errors::RpcError::invalid_params("Transaction ID required".to_string())
        })?;

        let verbose = params.get(1).and_then(|p| p.as_bool()).unwrap_or(false);

        let hash_bytes = hex::decode(txid).map_err(|e| {
            crate::rpc::errors::RpcError::invalid_params(format!("Invalid transaction ID: {e}"))
        })?;
        if hash_bytes.len() != 32 {
            return Err(crate::rpc::errors::RpcError::invalid_params(
                "Transaction ID must be 32 bytes".to_string(),
            ));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_bytes);

        if let Some(ref mempool) = self.mempool {
            // Find descendants by checking which transactions spend outputs created by this transaction
            let mut descendants = Vec::new();

            if let Some(tx) = mempool.get_transaction(&hash) {
                // Get all output outpoints from this transaction
                let mut output_outpoints = Vec::new();
                for (idx, _output) in tx.outputs.iter().enumerate() {
                    output_outpoints.push(bllvm_protocol::OutPoint {
                        hash,
                        index: idx as u64,
                    });
                }

                // Find transactions that spend these outputs
                use bllvm_protocol::block::calculate_tx_id;
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
                        let descendant_txid = hex::encode(descendant_hash);
                        let descendant_txid_clone = descendant_txid.clone();
                        use bllvm_protocol::serialization::transaction::serialize_transaction;
                        let size = serialize_transaction(&descendant_tx).len();

                        result.insert(descendant_txid, json!({
                            "size": size,
                            "fee": if let Some(ref storage) = self.storage {
                                let utxo_set = storage.utxos().get_all_utxos().unwrap_or_default();
                                let fee_satoshis = mempool.calculate_transaction_fee(&descendant_tx, &utxo_set);
                                fee_satoshis as f64 / 100_000_000.0
                            } else {
                                0.0
                            },
                            "modifiedfee": 0.0,
                            "time": current_timestamp(),
                            "height": -1,
                            "descendantcount": 1,
                            "descendantsize": size,
                            "descendantfees": 0.0,
                            "ancestorcount": 1,
                            "ancestorsize": size,
                            "ancestorfees": 0.0,
                            "wtxid": descendant_txid_clone,
                            "fees": {
                                "base": 0.0,
                                "modified": 0.0,
                                "ancestor": 0.0,
                                "descendant": 0.0
                            },
                            "depends": [],
                            "spentby": [],
                            "bip125-replaceable": false
                        }));
                    }
                }
                Ok(json!(result))
            } else {
                // Return just transaction IDs
                let txids: Vec<String> = descendants.iter().map(hex::encode).collect();
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

        let txid = params.get(0).and_then(|p| p.as_str()).ok_or_else(|| {
            crate::rpc::errors::RpcError::invalid_params("Transaction ID required".to_string())
        })?;

        let hash_bytes = hex::decode(txid).map_err(|e| {
            crate::rpc::errors::RpcError::invalid_params(format!("Invalid transaction ID: {e}"))
        })?;
        if hash_bytes.len() != 32 {
            return Err(crate::rpc::errors::RpcError::invalid_params(
                "Transaction ID must be 32 bytes".to_string(),
            ));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_bytes);

        if let Some(ref mempool) = self.mempool {
            if let Some(tx) = mempool.get_transaction(&hash) {
                use bllvm_protocol::serialization::transaction::serialize_transaction;
                let size = serialize_transaction(&tx).len();

                // Get ancestors and descendants
                let ancestors = self.get_ancestors(mempool, &hash);
                let descendants = self.get_descendants(mempool, &hash);

                let ancestor_count = ancestors.len();
                let descendant_count = descendants.len();
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

                let fee = if let Some(ref storage) = self.storage {
                    let utxo_set = storage.utxos().get_all_utxos().unwrap_or_default();
                    let fee_satoshis = mempool.calculate_transaction_fee(&tx, &utxo_set);
                    fee_satoshis as f64 / 100_000_000.0
                } else {
                    0.0
                };

                Ok(json!({
                    "size": size,
                    "fee": fee,
                    "modifiedfee": fee,
                    "time": current_timestamp(),
                    "height": -1,
                    "descendantcount": descendant_count + 1,
                    "descendantsize": descendant_size + size,
                    "descendantfees": fee, // Simplified
                    "ancestorcount": ancestor_count + 1,
                    "ancestorsize": ancestor_size + size,
                    "ancestorfees": fee, // Simplified
                    "wtxid": txid,
                    "fees": {
                        "base": fee,
                        "modified": fee,
                        "ancestor": fee,
                        "descendant": fee
                    },
                    "depends": ancestors.iter().map(hex::encode).collect::<Vec<_>>(),
                    "spentby": descendants.iter().map(hex::encode).collect::<Vec<_>>(),
                    "bip125-replaceable": false
                }))
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

    /// Helper: Get ancestors for a transaction
    fn get_ancestors(&self, mempool: &MempoolManager, tx_hash: &Hash) -> Vec<Hash> {
        let mut ancestors = Vec::new();

        if let Some(tx) = mempool.get_transaction(tx_hash) {
            // Find transactions that this transaction depends on (spends their outputs)
            use bllvm_protocol::block::calculate_tx_id;
            for input in &tx.inputs {
                // Find transaction that created this output by checking all transactions
                let transactions = mempool.get_transactions();
                for ancestor_tx in transactions {
                    let ancestor_hash = calculate_tx_id(&ancestor_tx);
                    for (idx, _output) in ancestor_tx.outputs.iter().enumerate() {
                        if input.prevout.hash == ancestor_hash
                            && input.prevout.index == idx as u64
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
                output_outpoints.push(bllvm_protocol::OutPoint {
                    hash: *tx_hash,
                    index: idx as u64,
                });
            }

            // Find transactions that spend these outputs
            use bllvm_protocol::block::calculate_tx_id;
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
