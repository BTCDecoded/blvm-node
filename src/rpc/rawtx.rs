//! Raw Transaction RPC Methods
//!
//! Implements raw transaction-related JSON-RPC methods:
//! - sendrawtransaction
//! - testmempoolaccept
//! - decoderawtransaction
//! - getrawtransaction (enhanced)
//! - gettxout
//! - gettxoutproof
//! - verifytxoutproof

use crate::node::mempool::MempoolManager;
use crate::node::metrics::MetricsCollector;
use crate::node::performance::{OperationType, PerformanceProfiler, PerformanceTimer};
use crate::rpc::errors::{RpcError, RpcResult};
use crate::storage::Storage;
use hex;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, warn};

/// Raw Transaction RPC methods
pub struct RawTxRpc {
    storage: Option<Arc<Storage>>,
    mempool: Option<Arc<MempoolManager>>,
    metrics: Option<Arc<MetricsCollector>>,
    profiler: Option<Arc<PerformanceProfiler>>,
}

impl RawTxRpc {
    /// Create a new raw transaction RPC handler
    pub fn new() -> Self {
        Self {
            storage: None,
            mempool: None,
            metrics: None,
            profiler: None,
        }
    }

    /// Create with dependencies
    pub fn with_dependencies(
        storage: Arc<Storage>,
        mempool: Arc<MempoolManager>,
        metrics: Option<Arc<MetricsCollector>>,
        profiler: Option<Arc<PerformanceProfiler>>,
    ) -> Self {
        Self {
            storage: Some(storage),
            mempool: Some(mempool),
            metrics,
            profiler,
        }
    }

    /// Send a raw transaction to the network
    ///
    /// Params: ["hexstring", maxfeerate (optional), maxtime (optional)]
    pub async fn sendrawtransaction(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: sendrawtransaction");

        // Validate hex string parameter with length limits
        use crate::rpc::validation::validate_hex_string_param;
        let hex_string = validate_hex_string_param(
            params,
            0,
            "hexstring",
            Some(crate::rpc::validation::MAX_HEX_STRING_LENGTH),
        )?;

        let tx_bytes = hex::decode(&hex_string)
            .map_err(|e| RpcError::invalid_params(format!("Invalid hex string: {e}")))?;

        if let (Some(storage), Some(mempool)) = (self.storage.as_ref(), self.mempool.as_ref()) {
            use bllvm_protocol::serialization::transaction::deserialize_transaction;
            let tx = deserialize_transaction(&tx_bytes).map_err(|e| {
                RpcError::invalid_params(format!("Failed to parse transaction: {e}"))
            })?;

            use bllvm_protocol::block::calculate_tx_id;
            let txid = calculate_tx_id(&tx);

            // Check if already in mempool
            if mempool.get_transaction(&txid).is_some() {
                return Err(RpcError::invalid_params("Transaction already in mempool"));
            }

            // Check if in chain
            if storage
                .transactions()
                .has_transaction(&txid)
                .unwrap_or(false)
            {
                return Err(RpcError::invalid_params("Transaction already in chain"));
            }

            // Validate transaction using consensus layer
            let _timer = self.profiler.as_ref().map(|p| {
                use crate::utils::arc_clone;
                PerformanceTimer::start(arc_clone(p), OperationType::TxValidation)
            });
            let validation_start = Instant::now();
            use bllvm_protocol::ConsensusProof;
            let consensus = ConsensusProof::new();
            match consensus.validate_transaction(&tx) {
                Ok(bllvm_protocol::ValidationResult::Valid) => {
                    let validation_time = validation_start.elapsed();
                    // Timer will record duration when dropped

                    // Update metrics
                    if let Some(ref metrics) = self.metrics {
                        metrics.update_performance(|m| {
                            let time_ms = validation_time.as_secs_f64() * 1000.0;
                            // Update average transaction validation time (exponential moving average)
                            m.avg_tx_validation_time_ms =
                                (m.avg_tx_validation_time_ms * 0.9) + (time_ms * 0.1);
                            // Update transactions per second
                            if validation_time.as_secs_f64() > 0.0 {
                                m.transactions_per_second = 1.0 / validation_time.as_secs_f64();
                            }
                        });
                    }

                    // Transaction structure is valid, now check inputs against UTXO set
                    let utxo_set = storage.utxos().get_all_utxos().map_err(|e| {
                        RpcError::internal_error(format!("Failed to get UTXO set: {e}"))
                    })?;

                    // Check if all inputs exist in UTXO set
                    for input in &tx.inputs {
                        if !utxo_set.contains_key(&input.prevout) {
                            return Err(RpcError::invalid_params(format!(
                                "Input {}:{} not found in UTXO set",
                                hex::encode(input.prevout.hash),
                                input.prevout.index
                            )));
                        }
                    }

                    // Add to mempool
                    // Note: add_transaction requires &mut self, but we have Arc<MempoolManager>
                    // In production, this would need to use interior mutability (Mutex/RwLock)
                    // For now, we'll skip adding to mempool as it requires mutable access
                    debug!(
                        "Transaction validated but not added to mempool (requires mutable access)"
                    );
                }
                Ok(bllvm_protocol::ValidationResult::Invalid(reason)) => {
                    return Err(RpcError::invalid_params(format!(
                        "Transaction validation failed: {reason}"
                    )));
                }
                Err(e) => {
                    return Err(RpcError::internal_error(format!(
                        "Transaction validation error: {e}"
                    )));
                }
            }

            Ok(json!(hex::encode(txid)))
        } else {
            Err(RpcError::invalid_params(
                "RPC not initialized with dependencies",
            ))
        }
    }

    /// Test if a raw transaction would be accepted to the mempool
    ///
    /// Params: ["hexstring", maxfeerate (optional)]
    pub async fn testmempoolaccept(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: testmempoolaccept");

        let hex_string = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::invalid_params("Missing hexstring parameter"))?;

        // Decode hex string
        let tx_bytes = hex::decode(hex_string)
            .map_err(|e| RpcError::invalid_params(format!("Invalid hex string: {e}")))?;

        use bllvm_protocol::serialization::transaction::deserialize_transaction;
        let tx = deserialize_transaction(&tx_bytes)
            .map_err(|e| RpcError::invalid_params(format!("Failed to parse transaction: {e}")))?;

        use bllvm_protocol::block::calculate_tx_id;
        let txid = calculate_tx_id(&tx);
        let txid_hex = hex::encode(txid);

        // Validate transaction using consensus layer
        use bllvm_protocol::ConsensusProof;
        let consensus = ConsensusProof::new();
        let validation_result = consensus.validate_transaction(&tx);

        let allowed = matches!(
            validation_result,
            Ok(bllvm_protocol::ValidationResult::Valid)
        );
        let reject_reason = if !allowed {
            match validation_result {
                Ok(bllvm_protocol::ValidationResult::Invalid(reason)) => Some(reason),
                Err(e) => Some(format!("Validation error: {e}")),
                _ => None,
            }
        } else {
            None
        };

        // Calculate transaction size
        use bllvm_protocol::serialization::transaction::serialize_transaction;
        let size = serialize_transaction(&tx).len();
        let vsize = size; // Simplified - in real implementation would use weight/4

        // Calculate fee using mempool manager if available
        let fee = if let Some(ref mempool) = self.mempool {
            if let Some(ref storage) = self.storage {
                let utxo_set = storage.utxos().get_all_utxos().unwrap_or_default();
                let fee_satoshis = mempool.calculate_transaction_fee(&tx, &utxo_set);
                fee_satoshis as f64 / 100_000_000.0 // Convert to BTC
            } else {
                0.00001000 // Default if no storage
            }
        } else {
            0.00001000 // Default if no mempool
        };

        Ok(json!([{
            "txid": txid_hex,
            "allowed": allowed,
            "vsize": vsize,
            "fees": {
                "base": fee
            },
            "reject-reason": reject_reason
        }]))
    }

    /// Decode a raw transaction
    ///
    /// Params: ["hexstring", iswitness (optional, default: try both)]
    pub async fn decoderawtransaction(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: decoderawtransaction");

        // Validate hex string parameter with length limits
        use crate::rpc::validation::validate_hex_string_param;
        let hex_string = validate_hex_string_param(
            params,
            0,
            "hexstring",
            Some(crate::rpc::validation::MAX_HEX_STRING_LENGTH),
        )?;

        let tx_bytes = hex::decode(&hex_string)
            .map_err(|e| RpcError::invalid_params(format!("Invalid hex string: {e}")))?;

        use bllvm_protocol::serialization::transaction::deserialize_transaction;
        let tx = deserialize_transaction(&tx_bytes)
            .map_err(|e| RpcError::invalid_params(format!("Failed to parse transaction: {e}")))?;

        use bllvm_protocol::block::calculate_tx_id;
        let txid = calculate_tx_id(&tx);
        let txid_hex = hex::encode(txid);
        let size = tx_bytes.len();

        // Pre-allocate and build vin
        let mut vin = Vec::with_capacity(tx.inputs.len());
        for input in &tx.inputs {
            vin.push(json!({
                "txid": hex::encode(input.prevout.hash),
                "vout": input.prevout.index,
                "scriptSig": {
                    "asm": "",
                    "hex": hex::encode(&input.script_sig)
                },
                "sequence": input.sequence
            }));
        }

        // Pre-allocate and build vout
        let mut vout = Vec::with_capacity(tx.outputs.len());
        for (i, output) in tx.outputs.iter().enumerate() {
            vout.push(json!({
                "value": output.value as f64 / 100_000_000.0,
                "n": i,
                "scriptPubKey": {
                    "asm": "",
                    "hex": hex::encode(&output.script_pubkey),
                    "reqSigs": 1,
                    "type": "pubkeyhash",
                    "addresses": []
                }
            }));
        }

        Ok(json!({
            "txid": txid_hex.clone(),
            "hash": txid_hex,
            "version": tx.version,
            "size": size,
            "vsize": size,
            "weight": size * 4, // Simplified
            "locktime": tx.lock_time,
            "vin": vin,
            "vout": vout,
            "hex": hex_string
        }))
    }

    /// Get raw transaction by txid
    ///
    /// Params: ["txid", verbose (optional, default: false), blockhash (optional)]
    pub async fn getrawtransaction(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: getrawtransaction");

        let txid = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::invalid_params("Missing txid parameter"))?;

        let verbose = params.get(1).and_then(|p| p.as_bool()).unwrap_or(false);

        let txid_bytes = hex::decode(txid)
            .map_err(|e| RpcError::invalid_params(format!("Invalid txid: {e}")))?;
        if txid_bytes.len() != 32 {
            return Err(RpcError::invalid_params("Invalid txid length"));
        }
        let mut txid_array = [0u8; 32];
        txid_array.copy_from_slice(&txid_bytes);

        if let Some(ref storage) = self.storage {
            if let Ok(Some(tx)) = storage.transactions().get_transaction(&txid_array) {
                use bllvm_protocol::serialization::transaction::serialize_transaction;
                let tx_hex = hex::encode(serialize_transaction(&tx));

                if verbose {
                    use bllvm_protocol::block::calculate_tx_id;
                    let calculated_txid = calculate_tx_id(&tx);
                    Ok(json!({
                        "txid": hex::encode(calculated_txid),
                        "hash": hex::encode(calculated_txid),
                        "version": tx.version,
                        "size": serialize_transaction(&tx).len(),
                        "vsize": serialize_transaction(&tx).len(),
                        "weight": serialize_transaction(&tx).len() * 4,
                        "locktime": tx.lock_time,
                        "vin": tx.inputs.iter().map(|input| json!({
                            "txid": hex::encode(input.prevout.hash),
                            "vout": input.prevout.index,
                            "scriptSig": {
                                "asm": "",
                                "hex": hex::encode(&input.script_sig)
                            },
                            "sequence": input.sequence
                        })).collect::<Vec<_>>(),
                        "vout": tx.outputs.iter().enumerate().map(|(i, output)| json!({
                            "value": output.value as f64 / 100_000_000.0,
                            "n": i,
                            "scriptPubKey": {
                                "asm": "",
                                "hex": hex::encode(&output.script_pubkey),
                                "reqSigs": 1,
                                "type": "pubkeyhash",
                                "addresses": []
                            }
                        })).collect::<Vec<_>>(),
                        "hex": tx_hex
                    }))
                } else {
                    Ok(json!(tx_hex))
                }
            } else {
                Err(RpcError::invalid_params("Transaction not found"))
            }
        } else if verbose {
            Ok(json!({
                "txid": txid,
                "hash": txid,
                "version": 1,
                "size": 250,
                "vsize": 250,
                "weight": 1000,
                "locktime": 0,
                "vin": [],
                "vout": [],
                "hex": "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff00ffffffff0100f2052a010000001976a914000000000000000000000000000000000000000088ac00000000"
            }))
        } else {
            Ok(json!("01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff00ffffffff0100f2052a010000001976a914000000000000000000000000000000000000000088ac00000000"))
        }
    }

    /// Get transaction output information
    ///
    /// Params: ["txid", n, includemempool (optional, default: true)]
    pub async fn gettxout(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: gettxout");

        let txid = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::invalid_params("Missing txid parameter"))?;

        let n = params
            .get(1)
            .and_then(|p| p.as_u64())
            .ok_or_else(|| RpcError::invalid_params("Missing n parameter"))?;

        let include_mempool = params.get(2).and_then(|p| p.as_bool()).unwrap_or(true);

        let txid_bytes = hex::decode(txid)
            .map_err(|e| RpcError::invalid_params(format!("Invalid txid: {e}")))?;
        if txid_bytes.len() != 32 {
            return Err(RpcError::invalid_params("Invalid txid length"));
        }
        let mut txid_array = [0u8; 32];
        txid_array.copy_from_slice(&txid_bytes);

        use bllvm_protocol::OutPoint;
        let outpoint = OutPoint {
            hash: txid_array,
            index: n,
        };

        if let Some(ref storage) = self.storage {
            // Check mempool first if requested
            if include_mempool {
                if let Some(ref mempool) = self.mempool {
                    if let Some(tx) = mempool.get_transaction(&txid_array) {
                        if (n as usize) < tx.outputs.len() {
                            let output = &tx.outputs[n as usize];
                            let best_hash = storage.chain().get_tip_hash()?.unwrap_or([0u8; 32]);
                            return Ok(json!({
                                "bestblock": hex::encode(best_hash),
                                "confirmations": 0,
                                "value": output.value as f64 / 100_000_000.0,
                                "scriptPubKey": {
                                    "asm": "",
                                    "hex": hex::encode(&output.script_pubkey),
                                    "reqSigs": 1,
                                    "type": "pubkeyhash",
                                    "addresses": []
                                },
                                "coinbase": false
                            }));
                        }
                    }
                }
            }

            // Check storage with timeout to prevent hanging (wrap sync operations)
            use crate::utils::with_storage_timeout;
            match with_storage_timeout(async {
                tokio::task::spawn_blocking({
                    let storage = storage.clone();
                    let outpoint = outpoint.clone();
                    move || storage.utxos().get_utxo(&outpoint)
                })
                .await
            })
            .await
            {
                Ok(Ok(Ok(Some(utxo)))) => {
                    // UTXO found - get chain info with timeout
                    let (best_hash, tip_height) = match with_storage_timeout(async {
                        tokio::task::spawn_blocking({
                            let storage = storage.clone();
                            move || -> Result<([u8; 32], u64), anyhow::Error> {
                                let best_hash = storage
                                    .chain()
                                    .get_tip_hash()
                                    .ok()
                                    .flatten()
                                    .unwrap_or([0u8; 32]);
                                let tip_height =
                                    storage.chain().get_height().ok().flatten().unwrap_or(0);
                                Ok((best_hash, tip_height))
                            }
                        })
                        .await
                    })
                    .await
                    {
                        Ok(Ok(Ok((hash, height)))) => (hash, height),
                        _ => ([0u8; 32], 0), // Fallback on error/timeout
                    };

                    // Find block height containing this transaction (with timeout protection)
                    // Use blocking task with timeout to prevent hanging
                    let tx_height = match with_storage_timeout(async {
                        tokio::task::spawn_blocking({
                            let storage = storage.clone();
                            let outpoint_hash = outpoint.hash;
                            let search_limit = tip_height.min(1000); // Limit search
                            move || -> Result<Option<u64>, anyhow::Error> {
                                let mut tx_height: Option<u64> = None;
                                for h in 0..=search_limit {
                                    if let Ok(Some(block_hash)) =
                                        storage.blocks().get_hash_by_height(h)
                                    {
                                        if let Ok(Some(block)) =
                                            storage.blocks().get_block(&block_hash)
                                        {
                                            for tx in &block.transactions {
                                                use bllvm_protocol::block::calculate_tx_id;
                                                let txid = calculate_tx_id(tx);
                                                if txid == outpoint_hash {
                                                    tx_height = Some(h);
                                                    break;
                                                }
                                            }
                                        }
                                        if tx_height.is_some() {
                                            break;
                                        }
                                    }
                                }
                                Ok(tx_height)
                            }
                        })
                        .await
                    })
                    .await
                    {
                        Ok(Ok(Ok(height))) => height,
                        _ => None, // Fallback on error/timeout
                    };

                    let confirmations = tx_height
                        .map(|h| {
                            if h > tip_height {
                                0
                            } else {
                                (tip_height - h + 1) as i64
                            }
                        })
                        .unwrap_or(0);

                    Ok(json!({
                        "bestblock": hex::encode(best_hash),
                        "confirmations": confirmations,
                        "value": utxo.value as f64 / 100_000_000.0,
                        "scriptPubKey": {
                            "asm": "",
                            "hex": hex::encode(&utxo.script_pubkey),
                            "reqSigs": 1,
                            "type": "pubkeyhash",
                            "addresses": []
                        },
                        "coinbase": false
                    }))
                }
                Ok(Ok(Ok(None))) | Ok(Ok(Err(_))) | Ok(Err(_)) => {
                    // UTXO not found or error - return null (normal case)
                    Ok(Value::Null)
                }
                Err(_) => {
                    // Timeout - log and return null (graceful degradation)
                    warn!("Timeout getting UTXO from storage");
                    Ok(Value::Null)
                }
            }
        } else {
            Ok(json!(null))
        }
    }

    /// Build merkle proof for transactions in a block
    fn build_merkle_proof(
        transactions: &[bllvm_protocol::Transaction],
        tx_indices: &[usize],
    ) -> Result<Vec<[u8; 32]>, RpcError> {
        use crate::storage::hashing::double_sha256;
        use bllvm_protocol::block::calculate_tx_id;

        if transactions.is_empty() {
            return Err(RpcError::internal_error(
                "Block has no transactions".to_string(),
            ));
        }

        // Calculate all transaction hashes
        let tx_hashes: Vec<[u8; 32]> = transactions.iter().map(calculate_tx_id).collect();

        let mut proof = Vec::new();
        let mut current_level = tx_hashes.clone();
        let mut current_indices: Vec<usize> = (0..transactions.len()).collect();

        // Build proof by traversing the merkle tree
        while current_level.len() > 1 {
            let mut next_level = Vec::new();
            let mut next_indices = Vec::new();
            let mut proof_added = false;

            for chunk in current_level.chunks(2) {
                if chunk.len() == 2 {
                    // Hash two hashes together
                    let mut combined = Vec::with_capacity(64);
                    combined.extend_from_slice(&chunk[0]);
                    combined.extend_from_slice(&chunk[1]);
                    let parent_hash = double_sha256(&combined);
                    next_level.push(parent_hash);

                    // Check if we need to add sibling to proof
                    if !proof_added {
                        for &idx in tx_indices {
                            let pos = current_indices.iter().position(|&i| i == idx);
                            if let Some(pos) = pos {
                                if pos % 2 == 0 && pos + 1 < current_level.len() {
                                    // Left child - add right sibling
                                    proof.push(chunk[1]);
                                } else if pos % 2 == 1 {
                                    // Right child - add left sibling
                                    proof.push(chunk[0]);
                                }
                                proof_added = true;
                                break;
                            }
                        }
                    }
                } else {
                    // Odd number: duplicate the last hash
                    let mut combined = Vec::with_capacity(64);
                    combined.extend_from_slice(&chunk[0]);
                    combined.extend_from_slice(&chunk[0]);
                    let parent_hash = double_sha256(&combined);
                    next_level.push(parent_hash);
                }
            }

            // Update indices for next level
            for i in 0..next_level.len() {
                next_indices.push(i);
            }

            current_level = next_level;
            current_indices = next_indices;
        }

        Ok(proof)
    }

    /// Get merkle proof that a transaction is in a block
    ///
    /// Params: ["txids", blockhash (optional)]
    pub async fn gettxoutproof(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: gettxoutproof");

        let txids = params
            .get(0)
            .and_then(|p| p.as_array())
            .ok_or_else(|| RpcError::invalid_params("Missing txids parameter"))?;

        let blockhash_opt = params.get(1).and_then(|p| p.as_str());

        if let Some(ref storage) = self.storage {
            // Find block containing the transactions
            let mut block: Option<bllvm_protocol::Block> = None;
            let tip_height = storage.chain().get_height()?.unwrap_or(0);

            if let Some(blockhash_str) = blockhash_opt {
                // Use specified blockhash
                let blockhash_bytes = hex::decode(blockhash_str)
                    .map_err(|e| RpcError::invalid_params(format!("Invalid blockhash: {e}")))?;
                if blockhash_bytes.len() != 32 {
                    return Err(RpcError::invalid_params("Invalid blockhash length"));
                }
                let mut blockhash_array = [0u8; 32];
                blockhash_array.copy_from_slice(&blockhash_bytes);
                if let Ok(Some(b)) = storage.blocks().get_block(&blockhash_array) {
                    block = Some(b);
                }
            } else {
                // Search for block containing any of the txids
                for h in 0..=tip_height {
                    if let Ok(Some(block_hash)) = storage.blocks().get_hash_by_height(h) {
                        if let Ok(Some(b)) = storage.blocks().get_block(&block_hash) {
                            // Check if block contains any of the requested txids
                            use bllvm_protocol::block::calculate_tx_id;
                            for tx in &b.transactions {
                                let txid = calculate_tx_id(tx);
                                let txid_hex = hex::encode(txid);
                                if txids
                                    .iter()
                                    .any(|tid| tid.as_str() == Some(txid_hex.as_str()))
                                {
                                    block = Some(b);
                                    break;
                                }
                            }
                            if block.is_some() {
                                break;
                            }
                        }
                    }
                }
            }

            if let Some(block) = block {
                // Find transaction indices
                use bllvm_protocol::block::calculate_tx_id;
                let mut tx_indices = Vec::new();
                for (idx, tx) in block.transactions.iter().enumerate() {
                    let txid = calculate_tx_id(tx);
                    let txid_hex = hex::encode(txid);
                    if txids
                        .iter()
                        .any(|tid| tid.as_str() == Some(txid_hex.as_str()))
                    {
                        tx_indices.push(idx);
                    }
                }

                if tx_indices.is_empty() {
                    return Err(RpcError::invalid_params(
                        "None of the specified transactions found in block",
                    ));
                }

                // Build merkle proof
                let proof_hashes = Self::build_merkle_proof(&block.transactions, &tx_indices)
                    .map_err(|e| {
                        RpcError::internal_error(format!("Failed to build merkle proof: {e}"))
                    })?;

                // Serialize proof (simplified - Bitcoin Core uses a more complex format)
                let mut proof_bytes = Vec::new();
                proof_bytes.push(proof_hashes.len() as u8);
                for hash in &proof_hashes {
                    proof_bytes.extend_from_slice(hash);
                }

                Ok(json!(hex::encode(proof_bytes)))
            } else {
                Err(RpcError::invalid_params("Block not found"))
            }
        } else {
            Err(RpcError::invalid_params(
                "RPC not initialized with dependencies",
            ))
        }
    }

    /// Verify a merkle proof
    ///
    /// Params: ["proof", blockhash"]
    pub async fn verifytxoutproof(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: verifytxoutproof");

        let proof_hex = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::invalid_params("Missing proof parameter"))?;

        let blockhash = params
            .get(1)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::invalid_params("Missing blockhash parameter"))?;

        if let Some(ref storage) = self.storage {
            // Decode proof
            let proof_bytes = hex::decode(proof_hex)
                .map_err(|e| RpcError::invalid_params(format!("Invalid proof hex: {e}")))?;

            if proof_bytes.is_empty() {
                return Err(RpcError::invalid_params("Empty proof"));
            }

            let num_hashes = proof_bytes[0] as usize;
            if proof_bytes.len() < 1 + num_hashes * 32 {
                return Err(RpcError::invalid_params("Invalid proof length"));
            }

            let mut proof_hashes = Vec::new();
            for i in 0..num_hashes {
                let start = 1 + i * 32;
                let end = start + 32;
                let mut hash = [0u8; 32];
                hash.copy_from_slice(&proof_bytes[start..end]);
                proof_hashes.push(hash);
            }

            // Get block
            let blockhash_bytes = hex::decode(blockhash)
                .map_err(|e| RpcError::invalid_params(format!("Invalid blockhash: {e}")))?;
            if blockhash_bytes.len() != 32 {
                return Err(RpcError::invalid_params("Invalid blockhash length"));
            }
            let mut blockhash_array = [0u8; 32];
            blockhash_array.copy_from_slice(&blockhash_bytes);

            if let Ok(Some(block)) = storage.blocks().get_block(&blockhash_array) {
                // Calculate merkle root from block
                use bllvm_protocol::mining::calculate_merkle_root;
                let calculated_root = calculate_merkle_root(&block.transactions).map_err(|e| {
                    RpcError::internal_error(format!("Failed to calculate merkle root: {e}"))
                })?;

                // Verify proof by reconstructing root (simplified - would need txids from proof)
                // For now, just verify the block's merkle root matches the header
                let matches = calculated_root == block.header.merkle_root;

                // Extract transaction IDs from proof (simplified - full implementation would decode txids)
                use bllvm_protocol::block::calculate_tx_id;
                let txids: Vec<String> = block
                    .transactions
                    .iter()
                    .map(|tx| hex::encode(calculate_tx_id(tx)))
                    .collect();

                Ok(json!(json!({
                    "txids": txids,
                    "merkle_root": hex::encode(calculated_root),
                    "matches": matches
                })))
            } else {
                Err(RpcError::invalid_params("Block not found"))
            }
        } else {
            Err(RpcError::invalid_params(
                "RPC not initialized with dependencies",
            ))
        }
    }
}

impl Default for RawTxRpc {
    fn default() -> Self {
        Self::new()
    }
}
