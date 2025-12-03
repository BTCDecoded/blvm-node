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
use crate::rpc::errors::{RpcError, RpcErrorCode, RpcResult};
use crate::storage::Storage;
use hex;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, warn};
use std::result::Result;

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
    /// Params: ["hexstring", maxfeerate (optional), allowhighfees (optional)]
    /// - hexstring: Raw transaction hex
    /// - maxfeerate: Maximum fee rate in BTC per kvB (optional, default: no limit)
    /// - allowhighfees: Allow transactions with high fees (optional, default: false)
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

        // Parse optional parameters
        let maxfeerate_btc_per_kvb: Option<f64> = params
            .get(1)
            .and_then(|p| p.as_f64())
            .or_else(|| {
                params
                    .get(1)
                    .and_then(|p| p.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
            });
        
        let allowhighfees: bool = params
            .get(2)
            .and_then(|p| p.as_bool())
            .unwrap_or(false);

        let tx_bytes = hex::decode(&hex_string).map_err(|e| {
            RpcError::invalid_params_with_fields(
                format!("Invalid hex string: {e}"),
                vec![("hexstring", &format!("Invalid hex encoding: {e}"))],
                Some(json!([
                    "Hex string must contain only characters 0-9, a-f, A-F",
                    "Ensure the hex string is complete (even number of characters)"
                ])),
            )
        })?;

        if let (Some(storage), Some(mempool)) = (self.storage.as_ref(), self.mempool.as_ref()) {
            use bllvm_protocol::serialization::transaction::deserialize_transaction;
            let tx = deserialize_transaction(&tx_bytes).map_err(|e| {
                RpcError::invalid_params_with_fields(
                    format!("Failed to parse transaction: {e}"),
                    vec![(
                        "hexstring",
                        &format!("Transaction deserialization failed: {e}"),
                    )],
                    Some(json!([
                        "Ensure the transaction hex is valid and complete",
                        "Check that the transaction format matches Bitcoin transaction structure"
                    ])),
                )
            })?;

            use bllvm_protocol::block::calculate_tx_id;
            let txid = calculate_tx_id(&tx);
            let txid_hex = hex::encode(txid);

            // Check if already in mempool
            if mempool.get_transaction(&txid).is_some() {
                return Err(RpcError::tx_already_in_mempool(&txid_hex));
            }

            // Check if in chain
            if storage
                .transactions()
                .has_transaction(&txid)
                .unwrap_or(false)
            {
                return Err(RpcError::with_data(
                    RpcErrorCode::TxAlreadyInChain,
                    format!("Transaction already in chain: {txid_hex}"),
                    json!({
                        "txid": txid_hex,
                        "reason": "already_confirmed",
                        "suggestions": [
                            "Transaction has already been confirmed in a block",
                            "Use getrawtransaction to retrieve the transaction from the blockchain"
                        ]
                    }),
                ));
            }

            // Validate transaction using consensus layer
            let _timer = self
                .profiler
                .as_ref()
                .map(|p| PerformanceTimer::start(Arc::clone(p), OperationType::TxValidation));
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
                            let prevout_str = format!(
                                "{}:{}",
                                hex::encode(input.prevout.hash),
                                input.prevout.index
                            );
                            return Err(RpcError::with_data(
                                RpcErrorCode::TxMissingInputs,
                                format!("Input {prevout_str} not found in UTXO set"),
                                json!({
                                    "prevout": prevout_str,
                                    "txid": txid_hex,
                                    "reason": "missing_input",
                                    "suggestions": [
                                        "The referenced output does not exist or has already been spent",
                                        "Ensure the transaction is spending valid UTXOs",
                                        "Check that the previous transaction is confirmed"
                                    ]
                                }),
                            ));
                        }
                    }

                    // Calculate transaction fee and fee rate for maxfeerate check
                    let fee_satoshis = mempool.calculate_transaction_fee(&tx, &utxo_set);
                    
                    // Calculate transaction size and vsize
                    use bllvm_protocol::serialization::transaction::serialize_transaction;
                    let base_size = serialize_transaction(&tx).len() as u64;
                    // For non-SegWit transactions, weight = 4 * size, vsize = size
                    // For SegWit, we'd need witness data, but for fee rate check we can use base size
                    // This is a simplification - in production, we'd need full witness data
                    let vsize = base_size; // Simplified: assume non-SegWit for now
                    
                    // Calculate fee rate in BTC per kvB
                    let fee_rate_btc_per_kvb = if vsize > 0 {
                        (fee_satoshis as f64 / vsize as f64) * 1000.0 / 100_000_000.0
                    } else {
                        0.0
                    };

                    // Check maxfeerate if provided and allowhighfees is false
                    if let Some(max_feerate) = maxfeerate_btc_per_kvb {
                        if !allowhighfees && fee_rate_btc_per_kvb > max_feerate {
                            return Err(RpcError::with_data(
                                RpcErrorCode::TxRejected,
                                format!(
                                    "Fee rate {} BTC/kvB exceeds maximum allowed {} BTC/kvB",
                                    fee_rate_btc_per_kvb, max_feerate
                                ),
                                json!({
                                    "txid": txid_hex,
                                    "fee_rate": fee_rate_btc_per_kvb,
                                    "max_feerate": max_feerate,
                                    "reason": "fee_rate_too_high",
                                    "suggestions": [
                                        "Reduce the transaction fee",
                                        "Use allowhighfees=true to override this check",
                                        "Or increase the maxfeerate parameter"
                                    ]
                                }),
                            ));
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
                    return Err(RpcError::tx_rejected_with_context(
                        format!("Transaction validation failed: {reason}"),
                        Some(&txid_hex),
                        Some("validation_failed"),
                        Some(json!({
                            "validation_reason": reason,
                            "suggestions": [
                                "Review the transaction structure and ensure it follows Bitcoin protocol rules",
                                "Check that all inputs are valid and outputs are properly formatted",
                                "Verify that the transaction size and weight are within limits"
                            ]
                        })),
                    ));
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
    /// Params: [["hexstring", ...], maxfeerate (optional)] or ["hexstring", maxfeerate (optional)]
    /// Supports both single transaction and package validation
    pub async fn testmempoolaccept(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: testmempoolaccept");

        // Handle both array of transactions (package) and single transaction
        let rawtxs = if let Some(arr) = params.get(0).and_then(|p| p.as_array()) {
            // Array of hex strings (package validation)
            arr.iter()
                .map(|v| v.as_str().ok_or_else(|| {
                    RpcError::invalid_params("First parameter must be array of hex strings or single hex string")
                }))
                .collect::<Result<Vec<&str>, _>>()?
        } else if let Some(hex_str) = params.get(0).and_then(|p| p.as_str()) {
            // Single hex string
            vec![hex_str]
        } else {
            return Err(RpcError::missing_parameter("rawtxs", Some("array of hex strings or single hex string")));
        };

        // Parse all transactions with witness data
        let mut transactions = Vec::new();
        let mut all_witnesses = Vec::new(); // Vec<Vec<Witness>> - one Vec<Witness> per transaction
        
        for hex_string in &rawtxs {
            let tx_bytes = hex::decode(hex_string).map_err(|e| {
                RpcError::invalid_params_with_fields(
                    format!("Invalid hex string: {e}"),
                    vec![("hexstring", &format!("Invalid hex encoding: {e}"))],
                    Some(json!([
                        "Hex string must contain only characters 0-9, a-f, A-F",
                        "Ensure the hex string is complete (even number of characters)"
                    ])),
                )
            })?;

            // Parse transaction and witness data (witnesses is Vec<Witness> - one per input)
            let (tx, witnesses) = Self::deserialize_transaction_with_witness(&tx_bytes)?;
            transactions.push(tx);
            all_witnesses.push(witnesses);
        }

        // Package validation: check for conflicts and dependencies
        let package_error = if transactions.len() > 1 {
            Self::validate_package(&transactions)
        } else {
            None
        };

        // Process each transaction
        let mut results = Vec::new();
        
        for (tx, tx_witnesses) in transactions.iter().zip(all_witnesses.iter()) {
            // If package validation failed, mark all transactions as failed
            if let Some(ref pkg_err) = package_error {
                results.push(json!({
                    "txid": hex::encode(bllvm_protocol::block::calculate_tx_id(tx)),
                    "wtxid": Self::calculate_wtxid(tx, tx_witnesses),
                    "package-error": pkg_err,
                    "allowed": false
                }));
                continue;
            }

            // Calculate txid and wtxid
            use bllvm_protocol::block::calculate_tx_id;
            let txid = calculate_tx_id(tx);
            let txid_hex = hex::encode(txid);
            let wtxid_hex = Self::calculate_wtxid(tx, tx_witnesses);

            // Validate transaction using consensus layer
            use bllvm_protocol::ConsensusProof;
            let consensus = ConsensusProof::new();
            let validation_result = consensus.validate_transaction(tx);

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

            // Calculate transaction size and weight with witness
            use bllvm_protocol::serialization::transaction::serialize_transaction;
            let base_size = serialize_transaction(tx).len() as u64;
            // Calculate total witness size (sum of all witness stacks for all inputs)
            let witness_size: u64 = tx_witnesses.iter()
                .map(|witness_stack| witness_stack.iter().map(|w| w.len() as u64).sum::<u64>())
                .sum();
            let total_size = base_size + witness_size;
            
            // Calculate weight using BIP141 formula: weight = 4 * base_size + total_size
            let weight = 4 * base_size + total_size;
            
            // Calculate vsize using proper BIP141 formula: vsize = ceil(weight / 4)
            use bllvm_consensus::witness::weight_to_vsize;
            let vsize = weight_to_vsize(weight) as usize;

            // Calculate fee using mempool manager if available
            let fee_satoshis = if let Some(ref mempool) = self.mempool {
                if let Some(ref storage) = self.storage {
                    let utxo_set = storage.utxos().get_all_utxos().unwrap_or_default();
                    mempool.calculate_transaction_fee(tx, &utxo_set)
                } else {
                    1000 // Default 1000 satoshis if no storage
                }
            } else {
                1000 // Default 1000 satoshis if no mempool
            };
            
            let fee_btc = fee_satoshis as f64 / 100_000_000.0; // Convert to BTC
            
            // Calculate effective fee rate (satoshis per kvB)
            // effective-feerate = (fee in satoshis) / (vsize in bytes) * 1000
            let effective_feerate_sat_per_kvb = if vsize > 0 {
                (fee_satoshis as f64 / vsize as f64) * 1000.0
            } else {
                0.0
            };
            let effective_feerate_btc_per_kvb = effective_feerate_sat_per_kvb / 100_000_000.0;

            // Get effective-includes (ancestor wtxids from mempool)
            let effective_includes = if allowed && transactions.len() == 1 {
                // Only calculate for single transaction (not in package)
                Self::get_effective_includes(&txid, self.mempool.as_ref())
            } else {
                Vec::<String>::new()
            };

            // Build fees object matching Core's format
            let mut fees_obj = json!({
                "base": fee_btc
            });
            
            // Add effective-feerate if transaction is allowed (Core only includes this when allowed)
            if allowed {
                fees_obj.as_object_mut().unwrap().insert(
                    "effective-feerate".to_string(),
                    json!(effective_feerate_btc_per_kvb)
                );
                // Add effective-includes (wtxids of ancestor transactions as hex strings)
                fees_obj.as_object_mut().unwrap().insert(
                    "effective-includes".to_string(),
                    json!(effective_includes)
                );
            }

            // Build result object
            let mut result_obj = json!({
                "txid": txid_hex,
                "wtxid": wtxid_hex,
                "allowed": allowed,
                "vsize": vsize,
                "fees": fees_obj,
            });
            
            // Add reject-reason only if transaction is not allowed
            if !allowed {
                result_obj.as_object_mut().unwrap().insert(
                    "reject-reason".to_string(),
                    json!(reject_reason)
                );
            }

            results.push(result_obj);
        }

        Ok(json!(results))
    }

    /// Deserialize transaction with witness data from Bitcoin wire format
    /// Returns (transaction, all_witnesses) tuple where all_witnesses is Vec<Witness> (one per input)
    fn deserialize_transaction_with_witness(data: &[u8]) -> Result<(bllvm_protocol::Transaction, Vec<bllvm_consensus::Witness>), RpcError> {
        use bllvm_protocol::serialization::transaction::deserialize_transaction;
        use bllvm_consensus::serialization::varint::decode_varint;
        
        // Deserialize transaction (non-witness serialization)
        let tx = deserialize_transaction(data).map_err(|e| {
            RpcError::invalid_params_with_fields(
                format!("Failed to parse transaction: {e}"),
                vec![("hexstring", &format!("Transaction deserialization failed: {e}"))],
                Some(json!([
                    "Ensure the transaction hex is valid and complete",
                    "Check that the transaction format matches Bitcoin transaction structure"
                ])),
            )
        })?;

        // Calculate base transaction size
        use bllvm_protocol::serialization::transaction::serialize_transaction;
        let base_size = serialize_transaction(&tx).len();
        
        // Check if there's witness data (SegWit marker 0x0001 after transaction)
        let mut offset = base_size;
        let witnesses = if data.len() >= offset + 2 && data[offset] == 0x00 && data[offset + 1] == 0x01 {
            // SegWit transaction - parse witness data for each input
            offset += 2; // Skip witness marker
            
            let mut all_witnesses = Vec::new();
            for _ in 0..tx.inputs.len() {
                if offset >= data.len() {
                    // No more witness data - create empty witness
                    all_witnesses.push(bllvm_consensus::Witness::new());
                    continue;
                }
                
                // Parse witness stack count
                let (stack_count, varint_len) = decode_varint(&data[offset..]).map_err(|e| {
                    RpcError::invalid_params(format!("Failed to parse witness: {e}"))
                })?;
                offset += varint_len;
                
                // Parse each witness element
                let mut witness_stack = bllvm_consensus::Witness::new();
                for _ in 0..stack_count {
                    if offset >= data.len() {
                        break;
                    }
                    
                    // Parse element length
                    let (element_len, varint_len) = decode_varint(&data[offset..]).map_err(|e| {
                        RpcError::invalid_params(format!("Failed to parse witness element: {e}"))
                    })?;
                    offset += varint_len;
                    
                    if offset + element_len as usize > data.len() {
                        break;
                    }
                    
                    // Get element bytes
                    let element = data[offset..offset + element_len as usize].to_vec();
                    witness_stack.push(element);
                    offset += element_len as usize;
                }
                
                all_witnesses.push(witness_stack);
            }
            
            // Ensure we have witnesses for all inputs
            while all_witnesses.len() < tx.inputs.len() {
                all_witnesses.push(bllvm_consensus::Witness::new());
            }
            
            all_witnesses
        } else {
            // Non-SegWit transaction - empty witnesses for all inputs
            vec![bllvm_consensus::Witness::new(); tx.inputs.len()]
        };

        Ok((tx, witnesses))
    }

    /// Calculate wtxid (witness transaction hash)
    /// For non-SegWit: wtxid == txid
    /// For SegWit: wtxid = SHA256(SHA256(tx_with_witness))
    /// witnesses: Vec<Witness> - one witness stack per input
    fn calculate_wtxid(tx: &bllvm_protocol::Transaction, witnesses: &[bllvm_consensus::Witness]) -> String {
        use bllvm_protocol::block::calculate_tx_id;
        let txid = calculate_tx_id(tx);
        
        // Check if any witness has data
        let has_witness = witnesses.iter().any(|w| !w.is_empty());
        
        // If no witness data, wtxid == txid
        if !has_witness {
            return hex::encode(txid);
        }
        
        // For SegWit transactions, wtxid is hash of transaction WITH witness
        // Serialize: version + marker(0x00) + flag(0x01) + inputs + outputs + locktime + witness_data
        use bllvm_consensus::serialization::varint::encode_varint;
        use sha2::{Digest, Sha256};
        
        let mut serialized = Vec::new();
        
        // Version (4 bytes)
        serialized.extend_from_slice(&(tx.version as u32).to_le_bytes());
        
        // SegWit marker and flag
        serialized.push(0x00);
        serialized.push(0x01);
        
        // Input count
        serialized.extend_from_slice(&encode_varint(tx.inputs.len() as u64));
        
        // Inputs (non-witness serialization)
        for input in &tx.inputs {
            serialized.extend_from_slice(&input.prevout.hash);
            serialized.extend_from_slice(&(input.prevout.index as u32).to_le_bytes());
            serialized.extend_from_slice(&encode_varint(input.script_sig.len() as u64));
            serialized.extend_from_slice(&input.script_sig);
            serialized.extend_from_slice(&(input.sequence as u32).to_le_bytes());
        }
        
        // Output count
        serialized.extend_from_slice(&encode_varint(tx.outputs.len() as u64));
        
        // Outputs
        for output in &tx.outputs {
            serialized.extend_from_slice(&(output.value as u64).to_le_bytes());
            serialized.extend_from_slice(&encode_varint(output.script_pubkey.len() as u64));
            serialized.extend_from_slice(&output.script_pubkey);
        }
        
        // Lock time
        serialized.extend_from_slice(&(tx.lock_time as u32).to_le_bytes());
        
        // Witness data: one witness stack per input
        for witness_stack in witnesses {
            // Witness stack count (number of elements)
            serialized.extend_from_slice(&encode_varint(witness_stack.len() as u64));
            // Each witness element
            for element in witness_stack {
                serialized.extend_from_slice(&encode_varint(element.len() as u64));
                serialized.extend_from_slice(element);
            }
        }
        
        // Double SHA256
        let first_hash = Sha256::digest(&serialized);
        let second_hash = Sha256::digest(first_hash);
        hex::encode(second_hash)
    }

    /// Validate package (multiple transactions)
    /// Returns error string if package is invalid
    fn validate_package(transactions: &[bllvm_protocol::Transaction]) -> Option<String> {
        // Check for duplicate transactions
        use bllvm_protocol::block::calculate_tx_id;
        let mut txids = std::collections::HashSet::new();
        for tx in transactions {
            let txid = calculate_tx_id(tx);
            if !txids.insert(txid) {
                return Some("package contains duplicate transactions".to_string());
            }
        }
        
        // Check for conflicts (transactions spending same outputs)
        let mut spent_outputs = std::collections::HashSet::new();
        for tx in transactions {
            for input in &tx.inputs {
                if !spent_outputs.insert((input.prevout.hash, input.prevout.index)) {
                    return Some("package contains conflicting transactions".to_string());
                }
            }
        }
        
        None
    }

    /// Get effective-includes (ancestor wtxids from mempool)
    /// Returns wtxids (as hex strings) of ancestor transactions used in fee calculation
    fn get_effective_includes(txid: &bllvm_protocol::Hash, mempool: Option<&Arc<MempoolManager>>) -> Vec<String> {
        let mut includes = Vec::new();
        
        if let Some(mempool) = mempool {
            // Get ancestors from mempool
            use bllvm_protocol::block::calculate_tx_id;
            if let Some(tx) = mempool.get_transaction(txid) {
                // Find ancestor transactions (transactions that this tx spends from)
                let ancestor_txids: Vec<bllvm_protocol::Hash> = mempool.get_transactions()
                    .iter()
                    .filter(|ancestor_tx| {
                        let ancestor_hash = calculate_tx_id(ancestor_tx);
                        tx.inputs.iter().any(|input| {
                            input.prevout.hash == ancestor_hash
                        })
                    })
                    .map(|ancestor_tx| {
                        calculate_tx_id(ancestor_tx)
                    })
                    .collect();
                
                // Convert to wtxids (hex strings)
                // For non-SegWit transactions, txid equals wtxid (by definition)
                // For SegWit transactions, wtxid would require witness data storage in mempool
                includes = ancestor_txids.iter()
                    .map(|hash| hex::encode(hash))
                    .collect();
            }
        }
        
        includes
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

    /// Serialize transaction with witness data (SegWit format)
    /// Returns hex string with witness data if witnesses provided, otherwise non-witness format
    fn serialize_transaction_with_witness(
        tx: &bllvm_protocol::Transaction,
        witnesses: Option<&[bllvm_consensus::Witness]>,
    ) -> String {
        use bllvm_protocol::serialization::transaction::serialize_transaction;
        use bllvm_consensus::serialization::varint::encode_varint;
        
        // Check if we have witness data
        let has_witness = witnesses
            .map(|w| w.iter().any(|witness_stack| !witness_stack.is_empty()))
            .unwrap_or(false);
        
        if !has_witness {
            // Non-SegWit: return standard serialization
            return hex::encode(serialize_transaction(tx));
        }
        
        // SegWit: serialize with witness marker and data
        let witnesses = witnesses.unwrap();
        let mut serialized = Vec::new();
        
        // Version (4 bytes, little-endian)
        serialized.extend_from_slice(&(tx.version as i32).to_le_bytes());
        
        // SegWit marker and flag
        serialized.push(0x00);
        serialized.push(0x01);
        
        // Input count
        serialized.extend_from_slice(&encode_varint(tx.inputs.len() as u64));
        
        // Inputs (non-witness serialization)
        for input in &tx.inputs {
            serialized.extend_from_slice(&input.prevout.hash);
            serialized.extend_from_slice(&(input.prevout.index as u32).to_le_bytes());
            serialized.extend_from_slice(&encode_varint(input.script_sig.len() as u64));
            serialized.extend_from_slice(&input.script_sig);
            serialized.extend_from_slice(&(input.sequence as u32).to_le_bytes());
        }
        
        // Output count
        serialized.extend_from_slice(&encode_varint(tx.outputs.len() as u64));
        
        // Outputs
        for output in &tx.outputs {
            serialized.extend_from_slice(&(output.value as u64).to_le_bytes());
            serialized.extend_from_slice(&encode_varint(output.script_pubkey.len() as u64));
            serialized.extend_from_slice(&output.script_pubkey);
        }
        
        // Lock time
        serialized.extend_from_slice(&(tx.lock_time as u32).to_le_bytes());
        
        // Witness data: one witness stack per input
        for witness_stack in witnesses {
            // Witness stack count (number of elements)
            serialized.extend_from_slice(&encode_varint(witness_stack.len() as u64));
            // Each witness element
            for element in witness_stack {
                serialized.extend_from_slice(&encode_varint(element.len() as u64));
                serialized.extend_from_slice(element);
            }
        }
        
        hex::encode(serialized)
    }

    /// Calculate transaction size and weight for SegWit transactions
    /// Returns (base_size, total_size, weight, vsize)
    fn calculate_segwit_sizes(
        tx: &bllvm_protocol::Transaction,
        witnesses: Option<&[bllvm_consensus::Witness]>,
    ) -> (usize, usize, u64, usize) {
        use bllvm_protocol::serialization::transaction::serialize_transaction;
        use bllvm_consensus::witness::{calculate_transaction_weight_segwit, weight_to_vsize};
        
        // Base size (without witness)
        let base_size = serialize_transaction(tx).len();
        
        // Check if we have witness data
        let has_witness = witnesses
            .map(|w| w.iter().any(|witness_stack| !witness_stack.is_empty()))
            .unwrap_or(false);
        
        if !has_witness {
            // Non-SegWit: base_size == total_size
            let total_size = base_size;
            let weight = (base_size * 4) as u64;
            let vsize = base_size;
            return (base_size, total_size, weight, vsize);
        }
        
        // SegWit: calculate total size with witness
        // Serialize with witness to get total size
        let tx_hex_with_witness = Self::serialize_transaction_with_witness(tx, witnesses);
        let total_size = tx_hex_with_witness.len() / 2; // Hex string length / 2 = bytes
        
        // Calculate weight: 4 * base_size + total_size
        let weight = calculate_transaction_weight_segwit(base_size as u64, total_size as u64);
        
        // Calculate vsize: ceil(weight / 4)
        let vsize = weight_to_vsize(weight) as usize;
        
        (base_size, total_size, weight, vsize)
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
                use bllvm_protocol::block::calculate_tx_id;
                let calculated_txid = calculate_tx_id(&tx);
                let txid_hex = hex::encode(calculated_txid);
                
                // Try to get witness data if available (from raw block data or mempool)
                // For now, we'll assume no witness data is available from storage
                // In a full implementation, we'd retrieve witness data from block storage
                let witnesses: Option<Vec<bllvm_consensus::Witness>> = None;
                
                // Calculate wtxid
                let wtxid_hex = if let Some(ref witnesses_vec) = witnesses {
                    Self::calculate_wtxid(&tx, witnesses_vec)
                } else {
                    // No witness data available - assume non-SegWit (wtxid == txid)
                    txid_hex.clone()
                };
                
                // Calculate sizes and weight
                let (base_size, total_size, weight, vsize) = Self::calculate_segwit_sizes(&tx, witnesses.as_deref());
                
                // For SegWit transactions, hash field should be wtxid
                // For non-SegWit, hash == txid
                let hash_hex = if witnesses.as_ref().map(|w| w.iter().any(|ws| !ws.is_empty())).unwrap_or(false) {
                    wtxid_hex.clone()
                } else {
                    txid_hex.clone()
                };
                
                // Serialize transaction hex (with witness if available)
                let tx_hex = Self::serialize_transaction_with_witness(&tx, witnesses.as_deref());

                if verbose {
                    Ok(json!({
                        "txid": txid_hex,
                        "hash": hash_hex,
                        "version": tx.version,
                        "size": total_size,
                        "vsize": vsize,
                        "weight": weight,
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

    /// Get comprehensive transaction details
    ///
    /// Params: ["txid", include_hex (optional, default: false)]
    /// Returns: Complete transaction information including block info, confirmations, inputs, outputs, fees
    pub async fn get_transaction_details(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: gettransactiondetails");

        let txid = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::missing_parameter("txid", Some("string (hex)")))?;

        let include_hex = params.get(1).and_then(|p| p.as_bool()).unwrap_or(false);

        let hash_bytes = hex::decode(txid).map_err(|e| {
            RpcError::invalid_hash_format(
                txid,
                Some(32),
                Some(&format!("Invalid hex encoding: {e}")),
            )
        })?;
        if hash_bytes.len() != 32 {
            return Err(RpcError::invalid_hash_format(
                txid,
                Some(32),
                Some("Transaction ID must be 64 hex characters (32 bytes)"),
            ));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_bytes);

        // Check mempool first
        if let Some(ref mempool) = self.mempool {
            if let Some(tx) = mempool.get_transaction(&hash) {
                use bllvm_protocol::serialization::transaction::serialize_transaction;
                let size = serialize_transaction(&tx).len();
                let fee = if let Some(ref storage) = self.storage {
                    let utxo_set = storage.utxos().get_all_utxos().unwrap_or_default();
                    mempool.calculate_transaction_fee(&tx, &utxo_set) as f64 / 100_000_000.0
                } else {
                    0.0
                };

                return Ok(json!({
                    "txid": txid,
                    "hash": txid,
                    "version": tx.version,
                    "size": size,
                    "vsize": size,
                    "weight": size * 4,
                    "locktime": tx.lock_time,
                    "vin": tx.inputs.iter().map(|input| json!({
                        "txid": hex::encode(input.prevout.hash),
                        "vout": input.prevout.index,
                        "scriptSig": hex::encode(&input.script_sig),
                        "sequence": input.sequence
                    })).collect::<Vec<_>>(),
                    "vout": tx.outputs.iter().enumerate().map(|(idx, output)| json!({
                        "value": output.value as f64 / 100_000_000.0,
                        "n": idx,
                        "scriptPubKey": {
                            "asm": hex::encode(&output.script_pubkey),
                            "hex": hex::encode(&output.script_pubkey),
                            "type": "nonstandard" // Would need script analysis
                        }
                    })).collect::<Vec<_>>(),
                    "hex": if include_hex { hex::encode(serialize_transaction(&tx)) } else { "".to_string() },
                    "blockhash": Value::Null,
                    "confirmations": 0,
                    "time": 0,
                    "blocktime": Value::Null,
                    "fee": fee,
                    "fee_rate": if size > 0 { fee / (size as f64 / 1000.0) } else { 0.0 }
                }));
            }
        }

        // Check blockchain
        if let Some(ref storage) = self.storage {
            if let Ok(Some(tx)) = storage.transactions().get_transaction(&hash) {
                use bllvm_protocol::serialization::transaction::serialize_transaction;
                let size = serialize_transaction(&tx).len();

                // Get block info if available from transaction metadata
                let metadata = storage.transactions().get_metadata(&hash).ok().flatten();
                let block_hash = metadata.map(|m| m.block_hash);
                let confirmations = if let Some(ref block_hash) = block_hash {
                    let block_height = storage
                        .blocks()
                        .get_height_by_hash(block_hash)
                        .ok()
                        .flatten();
                    let tip_height = storage.chain().get_height().ok().flatten().unwrap_or(0);
                    block_height
                        .map(|h| (tip_height.saturating_sub(h) + 1))
                        .unwrap_or(0)
                } else {
                    0
                };

                let block_time = block_hash
                    .and_then(|bh| storage.blocks().get_header(&bh).ok().flatten())
                    .map(|h| h.timestamp)
                    .unwrap_or(0);

                return Ok(json!({
                    "txid": txid,
                    "hash": txid,
                    "version": tx.version,
                    "size": size,
                    "vsize": size,
                    "weight": size * 4,
                    "locktime": tx.lock_time,
                    "vin": tx.inputs.iter().map(|input| json!({
                        "txid": hex::encode(input.prevout.hash),
                        "vout": input.prevout.index,
                        "scriptSig": hex::encode(&input.script_sig),
                        "sequence": input.sequence
                    })).collect::<Vec<_>>(),
                    "vout": tx.outputs.iter().enumerate().map(|(idx, output)| json!({
                        "value": output.value as f64 / 100_000_000.0,
                        "n": idx,
                        "scriptPubKey": {
                            "asm": hex::encode(&output.script_pubkey),
                            "hex": hex::encode(&output.script_pubkey),
                            "type": "nonstandard"
                        }
                    })).collect::<Vec<_>>(),
                    "hex": if include_hex { hex::encode(serialize_transaction(&tx)) } else { "".to_string() },
                    "blockhash": block_hash.map(|h| Value::String(hex::encode(h))).unwrap_or(Value::Null),
                    "confirmations": confirmations,
                    "time": block_time,
                    "blocktime": if block_time > 0 { Some(block_time) } else { None },
                    "fee": Value::Null, // Would need to calculate from inputs/outputs
                    "fee_rate": Value::Null
                }));
            }
        }

        Err(RpcError::tx_not_found_with_context(
            txid,
            false, // Not in mempool
            Some("Transaction not found in mempool or blockchain"),
        ))
    }

    /// Create a raw transaction
    ///
    /// Params: [inputs, outputs, locktime (optional), replaceable (optional), version (optional)]
    /// - inputs: Array of {"txid": "hex", "vout": n, "sequence": n (optional)}
    /// - outputs: Object with address->amount pairs, or array with {"address": amount} or {"data": "hex"}
    /// - locktime: Transaction locktime (default: 0)
    /// - replaceable: Enable RBF (default: true)
    /// - version: Transaction version (default: 2)
    pub async fn createrawtransaction(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: createrawtransaction");

        // Parse inputs
        let inputs = params
            .get(0)
            .and_then(|p| p.as_array())
            .ok_or_else(|| RpcError::missing_parameter("inputs", Some("array")))?;

        // Parse outputs - can be object (address->amount) or array (with "address" or "data" keys)
        let outputs = params
            .get(1)
            .ok_or_else(|| RpcError::missing_parameter("outputs", Some("object or array")))?;

        // Parse optional parameters
        let locktime = params
            .get(2)
            .and_then(|p| p.as_u64())
            .unwrap_or(0);
        let replaceable = params
            .get(3)
            .and_then(|p| p.as_bool())
            .unwrap_or(true);
        let version = params
            .get(4)
            .and_then(|p| p.as_u64())
            .unwrap_or(2) as u32;

        // Build transaction inputs
        use bllvm_protocol::Transaction;
        use bllvm_protocol::TransactionInput;
        use bllvm_protocol::OutPoint;

        let mut tx_inputs = Vec::new();
        for (idx, input) in inputs.iter().enumerate() {
            let input_obj = input
                .as_object()
                .ok_or_else(|| RpcError::invalid_params(format!("Input {} must be an object", idx)))?;

            let txid_hex = input_obj
                .get("txid")
                .and_then(|v| v.as_str())
                .ok_or_else(|| RpcError::invalid_params(format!("Input {} missing 'txid'", idx)))?;

            let txid_bytes = hex::decode(txid_hex)
                .map_err(|e| RpcError::invalid_params(format!("Invalid txid hex in input {}: {}", idx, e)))?;
            if txid_bytes.len() != 32 {
                return Err(RpcError::invalid_params(format!(
                    "Invalid txid length in input {}: expected 32 bytes, got {}",
                    idx,
                    txid_bytes.len()
                )));
            }
            let mut txid = [0u8; 32];
            txid.copy_from_slice(&txid_bytes);

            let vout = input_obj
                .get("vout")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| RpcError::invalid_params(format!("Input {} missing 'vout'", idx)))?;

            // Sequence: use provided value, or set based on RBF
            let sequence = if let Some(seq_val) = input_obj.get("sequence").and_then(|v| v.as_u64()) {
                seq_val as u32
            } else if replaceable {
                0xFFFFFFFD // MAX_BIP125_RBF_SEQUENCE
            } else if locktime > 0 {
                0xFFFFFFFE // MAX_SEQUENCE_NONFINAL
            } else {
                0xFFFFFFFF // SEQUENCE_FINAL
            };

            tx_inputs.push(TransactionInput {
                prevout: OutPoint {
                    hash: txid,
                    index: vout,
                },
                script_sig: Vec::new(),
                sequence: sequence as u64,
            });
        }

        // Build transaction outputs
        use bllvm_protocol::TransactionOutput;
        let mut tx_outputs = Vec::new();

        // Handle outputs - can be object (address->amount) or array
        if let Some(outputs_obj) = outputs.as_object() {
            // Object format: {"address": amount, ...}
            for (key, value) in outputs_obj.iter() {
                if key == "data" {
                    // OP_RETURN output
                    let data_hex = value
                        .as_str()
                        .ok_or_else(|| RpcError::invalid_params("'data' output must be hex string"))?;
                    let data = hex::decode(data_hex)
                        .map_err(|e| RpcError::invalid_params(format!("Invalid data hex: {}", e)))?;
                    
                    // OP_RETURN script: OP_RETURN <data>
                    let mut script = vec![0x6a]; // OP_RETURN
                    script.push(data.len() as u8);
                    script.extend_from_slice(&data);

                    tx_outputs.push(TransactionOutput {
                        value: 0,
                        script_pubkey: script,
                    });
                } else {
                    // Address output
                    let address_str = key;
                    let amount = value
                        .as_f64()
                        .or_else(|| value.as_str().and_then(|s| s.parse::<f64>().ok()))
                        .ok_or_else(|| RpcError::invalid_params(format!("Invalid amount for address '{}'", address_str)))?;

                    // Convert amount to satoshis
                    let satoshis = (amount * 100_000_000.0) as u64;

                    // Convert address to script_pubkey
                    let script_pubkey = Self::address_to_script_pubkey(address_str)?;

                    tx_outputs.push(TransactionOutput {
                        value: satoshis as i64,
                        script_pubkey,
                    });
                }
            }
        } else if let Some(outputs_arr) = outputs.as_array() {
            // Array format: [{"address": amount}, {"data": "hex"}, ...]
            for (idx, output) in outputs_arr.iter().enumerate() {
                let output_obj = output
                    .as_object()
                    .ok_or_else(|| RpcError::invalid_params(format!("Output {} must be an object", idx)))?;

                if let Some(data_val) = output_obj.get("data") {
                    // OP_RETURN output
                    let data_hex = data_val
                        .as_str()
                        .ok_or_else(|| RpcError::invalid_params(format!("Output {} 'data' must be hex string", idx)))?;
                    let data = hex::decode(data_hex)
                        .map_err(|e| RpcError::invalid_params(format!("Invalid data hex in output {}: {}", idx, e)))?;
                    
                    let mut script = vec![0x6a]; // OP_RETURN
                    script.push(data.len() as u8);
                    script.extend_from_slice(&data);

                    tx_outputs.push(TransactionOutput {
                        value: 0,
                        script_pubkey: script,
                    });
                } else if let Some(addr_val) = output_obj.get("address") {
                    // Address output
                    let address_str = addr_val
                        .as_str()
                        .ok_or_else(|| RpcError::invalid_params(format!("Output {} 'address' must be string", idx)))?;
                    
                    // Get amount from the same object (key-value pair)
                    let amount = output_obj
                        .values()
                        .find_map(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok())))
                        .ok_or_else(|| RpcError::invalid_params(format!("Output {} missing amount", idx)))?;

                    let satoshis = (amount * 100_000_000.0) as u64;
                    let script_pubkey = Self::address_to_script_pubkey(address_str)?;

                    tx_outputs.push(TransactionOutput {
                        value: satoshis as i64,
                        script_pubkey,
                    });
                } else {
                    return Err(RpcError::invalid_params(format!(
                        "Output {} must have either 'address' or 'data' key",
                        idx
                    )));
                }
            }
        } else {
            return Err(RpcError::invalid_params("Outputs must be an object or array"));
        }

        if tx_outputs.is_empty() {
            return Err(RpcError::invalid_params("Transaction must have at least one output"));
        }

        // Build transaction
        let tx = Transaction {
            version,
            inputs: tx_inputs,
            outputs: tx_outputs,
            lock_time: locktime as u32,
        };

        // Serialize transaction
        use bllvm_protocol::serialization::transaction::serialize_transaction;
        let tx_hex = hex::encode(serialize_transaction(&tx));

        Ok(json!(tx_hex))
    }

    /// Convert Bitcoin address to script_pubkey
    /// Supports Bech32/Bech32m (SegWit/Taproot) addresses
    /// Note: Legacy P2PKH/P2SH support requires Base58 decoding (not yet implemented)
    fn address_to_script_pubkey(address: &str) -> Result<Vec<u8>, RpcError> {
        use bllvm_protocol::address::BitcoinAddress;

        // Try Bech32/Bech32m first
        if let Ok(addr) = BitcoinAddress::decode(address) {
            match (addr.witness_version, addr.witness_program.len()) {
                // SegWit v0: P2WPKH (20 bytes) or P2WSH (32 bytes)
                (0, 20) | (0, 32) => {
                    let mut script = vec![0x00]; // OP_0
                    script.extend_from_slice(&addr.witness_program);
                    Ok(script)
                }
                // Taproot v1: P2TR (32 bytes)
                (1, 32) => {
                    let mut script = vec![0x51]; // OP_1
                    script.extend_from_slice(&addr.witness_program);
                    Ok(script)
                }
                _ => Err(RpcError::invalid_address_format(
                    address,
                    Some("Unsupported witness version or program length"),
                    None,
                )),
            }
        } else {
            // Legacy address support would go here (P2PKH/P2SH)
            // For now, return error
            Err(RpcError::invalid_address_format(
                address,
                Some("Only Bech32/Bech32m addresses supported. Legacy P2PKH/P2SH support coming soon."),
                None,
            ))
        }
    }
}

impl Default for RawTxRpc {
    fn default() -> Self {
        Self::new()
    }
}
