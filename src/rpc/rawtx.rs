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

use crate::config::RequestTimeoutConfig;
use crate::node::mempool::MempoolManager;
use crate::node::metrics::MetricsCollector;
use crate::node::performance::{OperationType, PerformanceProfiler, PerformanceTimer};
use crate::rpc::errors::{RpcError, RpcErrorCode, RpcResult};
use crate::rpc::params::{
    param_array, param_bool_default, param_f64, param_str, param_u64_default,
};
use crate::storage::Storage;
use crate::utils::{storage_timeout_from_config, with_custom_timeout};
use hex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::result::Result;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, warn};

/// Decode a Base58Check-encoded string.
///
/// Returns the decoded payload (without the 4-byte checksum suffix).
/// Returns `None` if the input is not valid Base58 or the checksum does not match.
fn base58check_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

    // Count leading '1's (each encodes a leading zero byte).
    let leading_zeros = s.bytes().take_while(|&b| b == b'1').count();

    // Decode each character to its Base58 digit.
    let mut big: Vec<u8> = Vec::new();
    for &byte in s.as_bytes() {
        let digit = ALPHABET.iter().position(|&a| a == byte)? as u64;
        let mut carry = digit;
        for val in big.iter_mut().rev() {
            carry += (*val as u64) * 58;
            *val = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            big.insert(0, (carry & 0xff) as u8);
            carry >>= 8;
        }
    }

    // Prepend the leading zero bytes.
    let mut decoded = vec![0u8; leading_zeros];
    decoded.extend_from_slice(&big);

    // Last 4 bytes are the checksum.
    if decoded.len() < 4 {
        return None;
    }
    let (payload, checksum) = decoded.split_at(decoded.len() - 4);
    let hash1 = Sha256::digest(payload);
    let hash2 = Sha256::digest(hash1);
    if &hash2[..4] != checksum {
        return None;
    }
    Some(payload.to_vec())
}

/// Decode a Bitcoin address to `script_pubkey` (Bech32/Bech32m and legacy Base58Check).
pub(crate) fn address_string_to_script_pubkey(address: &str) -> Result<Vec<u8>, RpcError> {
    use blvm_protocol::address::BitcoinAddress;
    use blvm_protocol::opcodes::{
        OP_CHECKSIG, OP_DUP, OP_EQUAL, OP_EQUALVERIFY, OP_HASH160, PUSH_20_BYTES,
    };

    if let Ok(addr) = BitcoinAddress::decode(address) {
        match (addr.witness_version, addr.witness_program.len()) {
            (0, 20) | (0, 32) => {
                let mut script = vec![0x00];
                script.extend_from_slice(&addr.witness_program);
                Ok(script)
            }
            (1, 32) => {
                let mut script = vec![blvm_protocol::opcodes::OP_1];
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
        let decoded = match base58check_decode(address) {
            Some(v) => v,
            None => {
                return Err(RpcError::invalid_address_format(
                    address,
                    Some("Invalid address: not Bech32/Bech32m and not valid Base58Check"),
                    None,
                ));
            }
        };
        if decoded.len() != 21 {
            return Err(RpcError::invalid_address_format(
                address,
                Some("Base58Check address must decode to 21 bytes (version + 20-byte hash)"),
                None,
            ));
        }
        let version = decoded[0];
        let hash: &[u8; 20] = decoded[1..21]
            .try_into()
            .expect("slice length checked above");
        match version {
            0x00 => {
                let mut script = vec![OP_DUP, OP_HASH160, PUSH_20_BYTES];
                script.extend_from_slice(hash);
                script.extend_from_slice(&[OP_EQUALVERIFY, OP_CHECKSIG]);
                Ok(script)
            }
            0x05 => {
                let mut script = vec![OP_HASH160, PUSH_20_BYTES];
                script.extend_from_slice(hash);
                script.push(OP_EQUAL);
                Ok(script)
            }
            _ => Err(RpcError::invalid_address_format(
                address,
                Some("Base58Check version must be 0x00 (P2PKH) or 0x05 (P2SH)"),
                None,
            )),
        }
    }
}

/// Raw Transaction RPC methods
pub struct RawTxRpc {
    storage: Option<Arc<Storage>>,
    mempool: Option<Arc<MempoolManager>>,
    metrics: Option<Arc<MetricsCollector>>,
    profiler: Option<Arc<PerformanceProfiler>>,
    /// Request timeout config (storage/network/rpc timeouts)
    request_timeouts: Option<RequestTimeoutConfig>,
}

impl RawTxRpc {
    /// Create a new raw transaction RPC handler
    pub fn new() -> Self {
        Self {
            storage: None,
            mempool: None,
            metrics: None,
            profiler: None,
            request_timeouts: None,
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
            request_timeouts: None,
        }
    }

    /// Set request timeout config (storage/network/rpc timeouts from config)
    pub fn with_request_timeouts(mut self, config: Option<RequestTimeoutConfig>) -> Self {
        self.request_timeouts = config;
        self
    }

    fn storage_timeout(&self) -> std::time::Duration {
        storage_timeout_from_config(self.request_timeouts.as_ref())
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
        let maxfeerate_btc_per_kvb: Option<f64> = param_f64(params, 1)
            .or_else(|| param_str(params, 1).and_then(|s| s.parse::<f64>().ok()));

        let allowhighfees: bool = param_bool_default(params, 2, false);

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
            let (tx, tx_witnesses) = Self::deserialize_transaction_with_witness(&tx_bytes)
                .map_err(|e| {
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

            use blvm_protocol::block::calculate_tx_id;
            let txid = calculate_tx_id(&tx);
            let txid_hex = Self::rpc_txid(&txid);

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
            use blvm_protocol::ConsensusProof;
            let consensus = ConsensusProof::new();
            match consensus.validate_transaction(&tx) {
                Ok(blvm_protocol::ValidationResult::Valid) => {
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

                    // Check if all inputs exist in the chain set or an unspent pool output.
                    for input in &tx.inputs {
                        let in_chain = utxo_set.contains_key(&input.prevout);
                        let in_pool = !in_chain
                            && mempool
                                .get_transaction(&input.prevout.hash)
                                .and_then(|parent| {
                                    parent.outputs.get(input.prevout.index as usize).cloned()
                                })
                                .is_some()
                            && !mempool.spends_outpoint(&input.prevout);
                        if !in_chain && !in_pool {
                            let prevout_str = format!(
                                "{}:{}",
                                Self::rpc_txid(&input.prevout.hash),
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

                    let has_witness = tx_witnesses.iter().any(|w| !w.is_empty());
                    let witness_slices = if has_witness {
                        Some(tx_witnesses.as_slice())
                    } else {
                        None
                    };

                    // Calculate transaction fee and fee rate for maxfeerate check
                    let fee_satoshis = mempool.calculate_transaction_fee(&tx, &utxo_set);

                    let (_, _, _, vsize) = Self::calculate_segwit_sizes(&tx, witness_slices);
                    let vsize = vsize as u64;

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
                                    "Fee rate {fee_rate_btc_per_kvb} BTC/kvB exceeds maximum allowed {max_feerate} BTC/kvB"
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

                    // Add to mempool — store witness stacks when present (SegWit).
                    let witness_arg = if has_witness {
                        Some(tx_witnesses)
                    } else {
                        None
                    };
                    match mempool.add_transaction_with_witness(tx.clone(), witness_arg) {
                        Ok(true) => {
                            debug!("Transaction {} accepted to mempool", txid_hex);
                        }
                        Ok(false) => {
                            return Err(RpcError::tx_rejected_with_context(
                                "Transaction rejected by mempool policy".to_string(),
                                Some(&txid_hex),
                                Some("rejected"),
                                Some(json!({
                                    "txid": txid_hex,
                                    "reason": "policy",
                                    "suggestions": [
                                        "Transaction may already be in the mempool",
                                        "Transaction fee rate may be below min-relay-fee",
                                        "Transaction conflicts with existing mempool transaction"
                                    ]
                                })),
                            ));
                        }
                        Err(e) => {
                            return Err(RpcError::internal_error(format!(
                                "Failed to add transaction to mempool: {e}"
                            )));
                        }
                    }
                }
                Ok(blvm_protocol::ValidationResult::Invalid(reason)) => {
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

            Ok(json!(Self::rpc_txid(&txid)))
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
        let rawtxs = if let Some(arr) = param_array(params, 0) {
            // Array of hex strings (package validation)
            arr.iter()
                .map(|v| {
                    v.as_str().ok_or_else(|| {
                        RpcError::invalid_params(
                            "First parameter must be array of hex strings or single hex string",
                        )
                    })
                })
                .collect::<Result<Vec<&str>, _>>()?
        } else if let Some(hex_str) = param_str(params, 0) {
            // Single hex string
            vec![hex_str]
        } else {
            return Err(RpcError::missing_parameter(
                "rawtxs",
                Some("array of hex strings or single hex string"),
            ));
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
                let wire = crate::rpc::txwire::tx_wire(tx, Some(tx_witnesses));
                results.push(json!({
                    "txid": wire.txid_hex,
                    "wtxid": wire.hash_hex,
                    "package-error": pkg_err,
                    "allowed": false
                }));
                continue;
            }

            // Calculate txid and wtxid in BIP145 display order.
            use blvm_protocol::block::calculate_tx_id;
            let txid = calculate_tx_id(tx);
            let wire = crate::rpc::txwire::tx_wire(tx, Some(tx_witnesses));
            let txid_hex = wire.txid_hex;
            let wtxid_hex = wire.hash_hex;

            // Validate transaction using consensus layer
            use blvm_protocol::ConsensusProof;
            let consensus = ConsensusProof::new();
            let validation_result = consensus.validate_transaction(tx);

            let allowed = matches!(
                validation_result,
                Ok(blvm_protocol::ValidationResult::Valid)
            );
            let reject_reason = if !allowed {
                match validation_result {
                    Ok(blvm_protocol::ValidationResult::Invalid(reason)) => Some(reason),
                    Err(e) => Some(format!("Validation error: {e}")),
                    _ => None,
                }
            } else {
                None
            };

            // Calculate transaction size and weight with witness
            let witness_arg = if tx_witnesses.iter().any(|w| !w.is_empty()) {
                Some(tx_witnesses.as_slice())
            } else {
                None
            };
            let (base_size, total_size, weight, vsize) =
                Self::calculate_segwit_sizes(tx, witness_arg);
            let _ = (base_size, total_size, weight);

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

            // Build fees object (standard format)
            let mut fees_obj = json!({
                "base": fee_btc
            });

            // Add effective-feerate if transaction is allowed
            if allowed {
                fees_obj.as_object_mut().unwrap().insert(
                    "effective-feerate".to_string(),
                    json!(effective_feerate_btc_per_kvb),
                );
                // Add effective-includes (wtxids of ancestor transactions as hex strings)
                fees_obj
                    .as_object_mut()
                    .unwrap()
                    .insert("effective-includes".to_string(), json!(effective_includes));
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
                result_obj
                    .as_object_mut()
                    .unwrap()
                    .insert("reject-reason".to_string(), json!(reject_reason));
            }

            results.push(result_obj);
        }

        Ok(json!(results))
    }

    /// Deserialize transaction with witness data from Bitcoin wire format
    /// Returns (transaction, all_witnesses) tuple where all_witnesses is Vec<Witness> (one per input)
    fn deserialize_transaction_with_witness(
        data: &[u8],
    ) -> Result<
        (
            blvm_protocol::Transaction,
            Vec<blvm_protocol::segwit::Witness>,
        ),
        RpcError,
    > {
        use blvm_protocol::serialization::transaction::deserialize_transaction_with_witness as parse;

        let (tx, witnesses, _consumed) = parse(data).map_err(|e| {
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

        // Ensure we have a witness vec entry for every input (pad with empty stacks if necessary)
        let mut witnesses = witnesses;
        while witnesses.len() < tx.inputs.len() {
            witnesses.push(blvm_protocol::segwit::Witness::new());
        }

        Ok((tx, witnesses))
    }

    /// Validate package (multiple transactions)
    /// Returns error string if package is invalid
    fn validate_package(transactions: &[blvm_protocol::Transaction]) -> Option<String> {
        // Check for duplicate transactions
        use blvm_protocol::block::calculate_tx_id;
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
    fn get_effective_includes(
        txid: &blvm_protocol::Hash,
        mempool: Option<&Arc<MempoolManager>>,
    ) -> Vec<String> {
        let mut includes = Vec::new();

        if let Some(mempool) = mempool {
            // Get ancestors from mempool
            use blvm_protocol::block::calculate_tx_id;
            if let Some(tx) = mempool.get_transaction(txid) {
                // Find ancestor transactions (transactions that this tx spends from)
                let ancestor_txids: Vec<blvm_protocol::Hash> = mempool
                    .get_transactions()
                    .iter()
                    .filter(|ancestor_tx| {
                        let ancestor_hash = calculate_tx_id(ancestor_tx);
                        tx.inputs
                            .iter()
                            .any(|input| input.prevout.hash == ancestor_hash)
                    })
                    .map(calculate_tx_id)
                    .collect();

                // Convert to wtxids (hex strings)
                // For non-SegWit transactions, txid equals wtxid (by definition)
                // For SegWit transactions, wtxid would require witness data storage in mempool
                includes = ancestor_txids.iter().map(hex::encode).collect();
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

        let (tx, witnesses) = Self::deserialize_transaction_with_witness(&tx_bytes)?;

        let wire = crate::rpc::txwire::tx_wire(&tx, Some(&witnesses));
        let txid_hex = wire.txid_hex;
        let hash_hex = wire.hash_hex;
        let (_, total_size, weight, vsize) =
            Self::calculate_segwit_sizes(&tx, Some(witnesses.as_slice()));
        let tx_hex_out = Self::serialize_transaction_with_witness(&tx, Some(&witnesses));

        // Pre-allocate and build vin
        let mut vin = Vec::with_capacity(tx.inputs.len());
        for (i, input) in tx.inputs.iter().enumerate() {
            let txinwitness: Vec<String> = witnesses
                .get(i)
                .map(|stack| {
                    stack
                        .iter()
                        .map(|item| hex::encode(item.as_slice()))
                        .collect()
                })
                .unwrap_or_default();
            vin.push(json!({
                "txid": Self::rpc_txid(&input.prevout.hash),
                "vout": input.prevout.index,
                "scriptSig": {
                    "asm": "",
                    "hex": hex::encode(&input.script_sig)
                },
                "txinwitness": txinwitness,
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
                    "type": script_pubkey_type(output.script_pubkey.as_ref())
                }
            }));
        }

        Ok(json!({
            "txid": txid_hex.clone(),
            "hash": hash_hex,
            "version": tx.version,
            "size": total_size,
            "vsize": vsize,
            "weight": weight,
            "locktime": tx.lock_time,
            "vin": vin,
            "vout": vout,
            "hex": tx_hex_out
        }))
    }

    /// Serialize transaction with witness data (SegWit format)
    /// Returns hex string with witness data if witnesses provided, otherwise non-witness format
    fn serialize_transaction_with_witness(
        tx: &blvm_protocol::Transaction,
        witnesses: Option<&[blvm_protocol::segwit::Witness]>,
    ) -> String {
        hex::encode(crate::rpc::txwire::tx_wire(tx, witnesses).bytes)
    }

    /// Calculate transaction size and weight for SegWit transactions
    /// Returns (base_size, total_size, weight, vsize)
    pub(crate) fn calculate_segwit_sizes(
        tx: &blvm_protocol::Transaction,
        witnesses: Option<&[blvm_protocol::segwit::Witness]>,
    ) -> (usize, usize, u64, usize) {
        use blvm_protocol::serialization::transaction::serialize_transaction;
        use blvm_protocol::witness::{calculate_transaction_weight_segwit, weight_to_vsize};

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

        let total_size = crate::rpc::txwire::tx_wire(tx, witnesses).bytes.len();

        // Calculate weight: 4 * base_size + total_size
        let weight = calculate_transaction_weight_segwit(base_size as u64, total_size as u64);

        // Calculate vsize: ceil(weight / 4)
        let vsize = weight_to_vsize(weight) as usize;

        (base_size, total_size, weight, vsize)
    }

    fn format_getrawtransaction_response(
        tx: &blvm_protocol::Transaction,
        witnesses: Option<&[blvm_protocol::segwit::Witness]>,
        verbose: bool,
    ) -> RpcResult<Value> {
        let wire = crate::rpc::txwire::tx_wire(tx, witnesses);
        let txid_hex = wire.txid_hex.clone();
        let has_witness = witnesses
            .map(|stacks| stacks.iter().any(|stack| !stack.is_empty()))
            .unwrap_or(false);
        let hash_hex = if has_witness {
            wire.hash_hex
        } else {
            txid_hex.clone()
        };
        let (_, total_size, weight, vsize) = Self::calculate_segwit_sizes(tx, witnesses);
        let tx_hex = Self::serialize_transaction_with_witness(tx, witnesses);

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
                    "txid": Self::rpc_txid(&input.prevout.hash),
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

        let verbose = param_bool_default(params, 1, false);

        let txid_array = Self::parse_rpc_txid(txid)?;

        if let Some(ref mempool) = self.mempool {
            if let Some(tx) = mempool.get_transaction(&txid_array) {
                let witnesses = mempool.get_transaction_witnesses(&txid_array);
                return Self::format_getrawtransaction_response(&tx, witnesses.as_deref(), verbose);
            }
        }

        if let Some(ref storage) = self.storage {
            if let Ok(Some(tx)) = storage.transactions().get_transaction(&txid_array) {
                let witnesses = self
                    .mempool
                    .as_ref()
                    .and_then(|m| m.get_transaction_witnesses(&txid_array));
                Self::format_getrawtransaction_response(&tx, witnesses.as_deref(), verbose)
            } else if let Some((block_hash, height)) =
                crate::module::pipeline::try_lookup_block_for_txids(&[txid.to_string()])
            {
                if let Ok(Some(block)) = storage.blocks().get_block(&block_hash) {
                    let (block, witnesses) =
                        crate::module::pipeline::try_rehydrate_block_for_consensus(
                            height,
                            block_hash,
                            block,
                            Vec::new(),
                        );
                    use blvm_protocol::block::calculate_tx_id;
                    if let Some((idx, tx)) = block
                        .transactions
                        .iter()
                        .enumerate()
                        .find(|(_, tx)| calculate_tx_id(tx) == txid_array)
                    {
                        let w = witnesses.get(idx).cloned();
                        Self::format_getrawtransaction_response(tx, w.as_deref(), verbose)
                    } else {
                        Err(RpcError::tx_not_found(""))
                    }
                } else {
                    Err(RpcError::tx_not_found(""))
                }
            } else {
                Err(RpcError::tx_not_found(""))
            }
        } else {
            Err(RpcError::tx_not_found(txid))
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

        let include_mempool = param_bool_default(params, 2, true);

        let txid_array = Self::parse_rpc_txid(txid)?;

        use blvm_protocol::OutPoint;
        let outpoint = OutPoint {
            hash: txid_array,
            index: n as u32,
        };

        if let Some(ref storage) = self.storage {
            // Check mempool first if requested
            if include_mempool {
                if let Some(ref mempool) = self.mempool {
                    if mempool.spends_outpoint(&outpoint) {
                        return Ok(Value::Null);
                    }
                    if let Some(tx) = mempool.get_transaction(&txid_array) {
                        if (n as usize) < tx.outputs.len() {
                            let output = &tx.outputs[n as usize];
                            let (best_hash, _) = storage.chain().get_tip_hash_and_height()?;
                            return Ok(json!({
                                "bestblock": Self::rpc_txid(&best_hash),
                                "confirmations": 0,
                                "value": output.value as f64 / 100_000_000.0,
                                "scriptPubKey": {
                                    "asm": "",
                                    "hex": hex::encode(&output.script_pubkey),
                                    "type": script_pubkey_type(output.script_pubkey.as_ref())
                                },
                                "coinbase": false
                            }));
                        }
                    }
                }
            }

            // Check storage with timeout to prevent hanging (wrap sync operations)
            let timeout_dur = self.storage_timeout();
            match with_custom_timeout(
                async {
                    tokio::task::spawn_blocking({
                        let storage = storage.clone();
                        move || storage.utxos().get_utxo(&outpoint)
                    })
                    .await
                },
                timeout_dur,
            )
            .await
            {
                Ok(Ok(Ok(Some(utxo)))) => {
                    // UTXO found - get chain info with timeout
                    let timeout_dur2 = self.storage_timeout();
                    let (best_hash, tip_height) = match with_custom_timeout(
                        async {
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
                        },
                        timeout_dur2,
                    )
                    .await
                    {
                        Ok(Ok(Ok((hash, height)))) => (hash, height),
                        _ => ([0u8; 32], 0), // Fallback on error/timeout
                    };

                    let tx_height = Some(utxo.height);

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
                        "bestblock": Self::rpc_txid(&best_hash),
                        "confirmations": confirmations,
                        "value": utxo.value as f64 / 100_000_000.0,
                        "scriptPubKey": {
                            "asm": "",
                            "hex": hex::encode(&utxo.script_pubkey),
                            "type": script_pubkey_type(utxo.script_pubkey.as_ref())
                        },
                        "coinbase": utxo.is_coinbase
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

    /// Get merkle proof that a transaction is in a block
    ///
    /// Params: ["txids", blockhash (optional)]
    pub async fn gettxoutproof(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: gettxoutproof");

        let txids = params
            .get(0)
            .and_then(|p| p.as_array())
            .ok_or_else(|| RpcError::invalid_params("Missing txids parameter"))?;

        if txids.is_empty() {
            return Err(RpcError::invalid_params(
                "Parameter 'txids' cannot be empty",
            ));
        }

        let blockhash_opt = param_str(params, 1);

        if let Some(ref storage) = self.storage {
            // Find block containing the transactions
            let mut block: Option<blvm_protocol::Block> = None;
            let mut resolved_height: Option<u64> = None;
            let tip_height = storage.chain().get_height()?.unwrap_or(0);

            if let Some(blockhash_str) = blockhash_opt {
                // Use specified blockhash
                let blockhash_array = Self::parse_rpc_txid(blockhash_str)?;
                if let Ok(Some(b)) = storage.blocks().get_block(&blockhash_array) {
                    resolved_height = storage
                        .blocks()
                        .get_height_by_hash(&blockhash_array)
                        .ok()
                        .flatten();
                    block = Some(b);
                }
            } else if let Some((hash, height)) = crate::module::pipeline::try_lookup_block_for_txids(
                &txids
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>(),
            ) {
                if let Ok(Some(b)) = storage.blocks().get_block(&hash) {
                    resolved_height = Some(height);
                    block = Some(b);
                }
            } else {
                // Search for block containing any of the txids
                for h in 0..=tip_height {
                    if let Ok(Some(block_hash)) = storage.blocks().get_hash_by_height(h) {
                        if let Ok(Some(b)) = storage.blocks().get_block(&block_hash) {
                            // Check if block contains any of the requested txids
                            use blvm_protocol::block::calculate_tx_id;
                            for tx in &b.transactions {
                                let txid = calculate_tx_id(tx);
                                let txid_hex = Self::rpc_txid(&txid);
                                if txids
                                    .iter()
                                    .any(|tid| tid.as_str() == Some(txid_hex.as_str()))
                                {
                                    resolved_height = Some(h);
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
                use crate::rpc::merkle_block::MerkleBlock;
                use blvm_protocol::block::calculate_tx_id;

                let block_hash = storage.blocks().get_block_hash(&block);
                let height = resolved_height.or_else(|| {
                    storage
                        .blocks()
                        .get_height_by_hash(&block_hash)
                        .ok()
                        .flatten()
                });
                // Unknown height (orphan / not in index): do not invent 0 for the module.
                let tx_hashes: Vec<[u8; 32]> = height
                    .and_then(|h| crate::module::pipeline::try_get_canonical_txids(h, block_hash))
                    .unwrap_or_else(|| block.transactions.iter().map(calculate_tx_id).collect());

                let requested: std::collections::HashSet<String> = txids
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();

                let mut match_flags = vec![false; tx_hashes.len()];
                let mut found = 0usize;
                for (idx, hash) in tx_hashes.iter().enumerate() {
                    if requested.contains(&Self::rpc_txid(hash)) {
                        match_flags[idx] = true;
                        found += 1;
                    }
                }

                if found != requested.len() {
                    return Err(RpcError::invalid_params(
                        "Not all transactions found in specified or retrieved block",
                    ));
                }

                let merkle_block = MerkleBlock::new(block.header.clone(), &tx_hashes, &match_flags)
                    .map_err(|e| RpcError::internal_error(format!("Failed to build proof: {e}")))?;

                Ok(json!(hex::encode(merkle_block.serialize())))
            } else {
                Err(RpcError::block_not_found(""))
            }
        } else {
            Err(RpcError::invalid_params(
                "RPC not initialized with dependencies",
            ))
        }
    }

    /// Verify a merkle proof (Core-compatible: returns matched txid hex strings).
    ///
    /// Params: ["proof", blockhash (optional)]
    pub async fn verifytxoutproof(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: verifytxoutproof");

        let proof_hex = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::invalid_params("Missing proof parameter"))?;

        let blockhash_opt = param_str(params, 1);

        if let Some(ref storage) = self.storage {
            let proof_bytes = hex::decode(proof_hex)
                .map_err(|e| RpcError::invalid_params(format!("Invalid proof hex: {e}")))?;

            if proof_bytes.is_empty() {
                return Err(RpcError::invalid_params("Empty proof"));
            }

            use crate::rpc::merkle_block::{MerkleBlock, block_hash_from_header};

            let merkle_block = MerkleBlock::deserialize(&proof_bytes).map_err(|e| {
                RpcError::invalid_params(format!("Invalid merkle block proof: {e}"))
            })?;

            let mut pmt = merkle_block.txn;
            let (extracted_root, matched) = pmt.extract_matches();
            if extracted_root != merkle_block.header.merkle_root {
                return Ok(json!([]));
            }

            let header_hash = block_hash_from_header(&merkle_block.header);
            if let Some(blockhash_str) = blockhash_opt {
                let blockhash_bytes = hex::decode(blockhash_str)
                    .map_err(|e| RpcError::invalid_params(format!("Invalid blockhash: {e}")))?;
                if blockhash_bytes.len() != 32 {
                    return Err(RpcError::invalid_params("Invalid blockhash length"));
                }
                let mut expected = [0u8; 32];
                expected.copy_from_slice(&blockhash_bytes);
                if expected != header_hash {
                    return Ok(json!([]));
                }
            }

            let block_in_chain = storage
                .blocks()
                .get_header(&header_hash)
                .ok()
                .flatten()
                .is_some();
            if !block_in_chain {
                return Err(RpcError::block_not_found("Block not found in chain"));
            }

            if let Ok(Some(block)) = storage.blocks().get_block(&header_hash) {
                if block.transactions.len() as u32 != pmt.n_transactions() {
                    return Ok(json!([]));
                }
            } else {
                return Ok(json!([]));
            }

            let txids: Vec<String> = matched.into_iter().map(hex::encode).collect();
            Ok(json!(txids))
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

        let include_hex = param_bool_default(params, 1, false);

        let hash = Self::parse_rpc_txid(txid)?;

        // Check mempool first
        if let Some(ref mempool) = self.mempool {
            if let Some(tx) = mempool.get_transaction(&hash) {
                let witnesses = mempool.get_transaction_witnesses(&hash);
                let wire = crate::rpc::txwire::tx_wire(&tx, witnesses.as_deref());
                let (size, _, weight, vsize) =
                    Self::calculate_segwit_sizes(&tx, witnesses.as_deref());
                let fee = if let Some(ref storage) = self.storage {
                    let utxo_set = storage.utxos().get_all_utxos().unwrap_or_default();
                    mempool.calculate_transaction_fee(&tx, &utxo_set) as f64 / 100_000_000.0
                } else {
                    0.0
                };

                return Ok(json!({
                    "txid": txid,
                    "hash": wire.hash_hex,
                    "version": tx.version,
                    "size": size,
                    "vsize": vsize,
                    "weight": weight,
                    "locktime": tx.lock_time,
                    "vin": tx.inputs.iter().map(|input| json!({
                        "txid": Self::rpc_txid(&input.prevout.hash),
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
                    "hex": if include_hex { hex::encode(&wire.bytes) } else { "".to_string() },
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
                use blvm_protocol::block::calculate_tx_id;

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
                        .map(|h| tip_height.saturating_sub(h) + 1)
                        .unwrap_or(0)
                } else {
                    0
                };

                let block_time = block_hash
                    .and_then(|bh| storage.blocks().get_header(&bh).ok().flatten())
                    .map(|h| h.timestamp)
                    .unwrap_or(0);
                let witnesses = block_hash.and_then(|block_id| {
                    let block = storage.blocks().get_block(&block_id).ok().flatten()?;
                    let index = block
                        .transactions
                        .iter()
                        .position(|stored| calculate_tx_id(stored) == hash)?;
                    storage
                        .blocks()
                        .get_witness(&block_id)
                        .ok()
                        .flatten()
                        .and_then(|all| all.get(index).cloned())
                });
                let wire = crate::rpc::txwire::tx_wire(&tx, witnesses.as_deref());
                let (size, _, weight, vsize) =
                    Self::calculate_segwit_sizes(&tx, witnesses.as_deref());

                return Ok(json!({
                    "txid": txid,
                    "hash": wire.hash_hex,
                    "version": tx.version,
                    "size": size,
                    "vsize": vsize,
                    "weight": weight,
                    "locktime": tx.lock_time,
                    "vin": tx.inputs.iter().map(|input| json!({
                        "txid": Self::rpc_txid(&input.prevout.hash),
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
                    "hex": if include_hex { hex::encode(&wire.bytes) } else { "".to_string() },
                    "blockhash": block_hash.map(|h| Value::String(Self::rpc_txid(&h))).unwrap_or(Value::Null),
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

    fn rpc_txid(hash: &[u8; 32]) -> String {
        crate::storage::hashing::hash_to_rpc_hex(hash)
    }

    fn parse_rpc_txid(hex_str: &str) -> RpcResult<[u8; 32]> {
        crate::storage::hashing::hash_from_rpc_hex(hex_str)
            .map_err(|e| RpcError::invalid_params(format!("Invalid txid: {e}")))
    }

    fn btc_amount_to_sats(amount: f64) -> RpcResult<u64> {
        if !amount.is_finite() || amount < 0.0 {
            return Err(RpcError::invalid_params(
                "amount must be a non-negative number".to_string(),
            ));
        }
        let sats = (amount * 100_000_000.0).round();
        if sats > u64::MAX as f64 {
            return Err(RpcError::invalid_params("amount is too large".to_string()));
        }
        Ok(sats as u64)
    }

    fn op_return_script(data: &[u8]) -> RpcResult<Vec<u8>> {
        use blvm_protocol::opcodes::{OP_PUSHDATA1, OP_RETURN};
        if data.len() > 255 {
            return Err(RpcError::invalid_params(
                "OP_RETURN payload longer than 255 bytes".to_string(),
            ));
        }
        let mut script = vec![OP_RETURN];
        if data.len() <= 75 {
            script.push(data.len() as u8);
        } else {
            script.push(OP_PUSHDATA1);
            script.push(data.len() as u8);
        }
        script.extend_from_slice(data);
        Ok(script)
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
        let locktime = param_u64_default(params, 2, 0);
        let replaceable = param_bool_default(params, 3, true);
        let version = param_u64_default(params, 4, 2);

        // Build transaction inputs
        use blvm_protocol::OutPoint;
        use blvm_protocol::TransactionInput;

        let mut tx_inputs = Vec::new();
        for (idx, input) in inputs.iter().enumerate() {
            let input_obj = input.as_object().ok_or_else(|| {
                RpcError::invalid_params(format!("Input {idx} must be an object"))
            })?;

            let txid_hex = input_obj
                .get("txid")
                .and_then(|v| v.as_str())
                .ok_or_else(|| RpcError::invalid_params(format!("Input {idx} missing 'txid'")))?;

            let txid_bytes = Self::parse_rpc_txid(txid_hex).map_err(|e| {
                RpcError::invalid_params(format!("Invalid txid hex in input {idx}: {e}"))
            })?;
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
                .ok_or_else(|| RpcError::invalid_params(format!("Input {idx} missing 'vout'")))?;

            // Sequence: use provided value, or set based on RBF
            let sequence = if let Some(seq_val) = input_obj.get("sequence").and_then(|v| v.as_u64())
            {
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
                    index: vout as u32,
                },
                script_sig: Vec::new(),
                sequence: sequence as u64,
            });
        }

        // Build transaction outputs
        use blvm_protocol::TransactionOutput;
        let mut tx_outputs = Vec::new();

        // Handle outputs - can be object (address->amount) or array
        if let Some(outputs_obj) = outputs.as_object() {
            // Object format: {"address": amount, ...}
            for (key, value) in outputs_obj.iter() {
                if key == "data" {
                    // OP_RETURN output
                    let data_hex = value.as_str().ok_or_else(|| {
                        RpcError::invalid_params("'data' output must be hex string")
                    })?;
                    let data = hex::decode(data_hex)
                        .map_err(|e| RpcError::invalid_params(format!("Invalid data hex: {e}")))?;

                    let script = Self::op_return_script(&data)?;

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
                        .ok_or_else(|| {
                            RpcError::invalid_params(format!(
                                "Invalid amount for address '{address_str}'"
                            ))
                        })?;

                    let satoshis = Self::btc_amount_to_sats(amount)?;

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
                let output_obj = output.as_object().ok_or_else(|| {
                    RpcError::invalid_params(format!("Output {idx} must be an object"))
                })?;

                if let Some(data_val) = output_obj.get("data") {
                    // OP_RETURN output
                    let data_hex = data_val.as_str().ok_or_else(|| {
                        RpcError::invalid_params(format!("Output {idx} 'data' must be hex string"))
                    })?;
                    let data = hex::decode(data_hex).map_err(|e| {
                        RpcError::invalid_params(format!("Invalid data hex in output {idx}: {e}"))
                    })?;

                    let script = Self::op_return_script(&data)?;

                    tx_outputs.push(TransactionOutput {
                        value: 0,
                        script_pubkey: script,
                    });
                } else if let Some(addr_val) = output_obj.get("address") {
                    // Address output
                    let address_str = addr_val.as_str().ok_or_else(|| {
                        RpcError::invalid_params(format!("Output {idx} 'address' must be string"))
                    })?;

                    // Get amount from the same object (key-value pair)
                    let amount = output_obj
                        .values()
                        .find_map(|v| {
                            v.as_f64()
                                .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
                        })
                        .ok_or_else(|| {
                            RpcError::invalid_params(format!("Output {idx} missing amount"))
                        })?;

                    let satoshis = Self::btc_amount_to_sats(amount)?;
                    let script_pubkey = Self::address_to_script_pubkey(address_str)?;

                    tx_outputs.push(TransactionOutput {
                        value: satoshis as i64,
                        script_pubkey,
                    });
                } else {
                    return Err(RpcError::invalid_params(format!(
                        "Output {idx} must have either 'address' or 'data' key"
                    )));
                }
            }
        } else {
            return Err(RpcError::invalid_params(
                "Outputs must be an object or array",
            ));
        }

        if tx_outputs.is_empty() {
            return Err(RpcError::invalid_params(
                "Transaction must have at least one output",
            ));
        }

        let tx_bytes =
            Self::serialize_createrawtransaction_bytes(version, tx_inputs, tx_outputs, locktime)?;
        // Bitcoin Core returns the raw transaction as a single hex string.
        Ok(Value::String(hex::encode(&tx_bytes)))
    }
}

/// Build Transaction for createrawtransaction; production uses SmallVec, non-production uses Vec.
macro_rules! createrawtransaction_tx {
    ($version:expr, $inputs:expr, $outputs:expr, $locktime:expr) => {{
        use blvm_protocol::Transaction;
        #[cfg(feature = "production")]
        {
            use smallvec::SmallVec;
            Transaction {
                version: $version,
                inputs: SmallVec::from_vec($inputs),
                outputs: SmallVec::from_vec($outputs),
                lock_time: $locktime,
            }
        }
        #[cfg(not(feature = "production"))]
        {
            use smallvec::SmallVec;
            Transaction {
                version: $version,
                inputs: SmallVec::from_vec($inputs),
                outputs: SmallVec::from_vec($outputs),
                lock_time: $locktime,
            }
        }
    }};
}

impl RawTxRpc {
    /// Build Transaction (SmallVec vs Vec by feature) and return serialized bytes.
    /// Single place for serialize; production/non-production differ only in container type (macro).
    fn serialize_createrawtransaction_bytes(
        version: u64,
        tx_inputs: Vec<blvm_protocol::TransactionInput>,
        tx_outputs: Vec<blvm_protocol::TransactionOutput>,
        locktime: u64,
    ) -> RpcResult<Vec<u8>> {
        use blvm_protocol::serialization::transaction::serialize_transaction;

        let tx = createrawtransaction_tx!(version, tx_inputs, tx_outputs, locktime);
        Ok(serialize_transaction(&tx))
    }

    /// Convert Bitcoin address to script_pubkey
    /// Supports Bech32/Bech32m (SegWit/Taproot) and legacy Base58Check (P2PKH/P2SH)
    fn address_to_script_pubkey(address: &str) -> Result<Vec<u8>, RpcError> {
        address_string_to_script_pubkey(address)
    }
}

impl Default for RawTxRpc {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn script_pubkey_type(script: &[u8]) -> &'static str {
    use blvm_protocol::opcodes::{
        OP_0, OP_1, OP_CHECKSIG, OP_DUP, OP_EQUAL, OP_EQUALVERIFY, OP_HASH160, OP_RETURN,
    };
    if script.len() == 25
        && script[0] == OP_DUP
        && script[1] == OP_HASH160
        && script[2] == 20
        && script[23] == OP_EQUALVERIFY
        && script[24] == OP_CHECKSIG
    {
        "pubkeyhash"
    } else if script.len() == 23
        && script[0] == OP_HASH160
        && script[1] == 20
        && script[22] == OP_EQUAL
    {
        "scripthash"
    } else if script.len() == 22 && script[0] == OP_0 && script[1] == 20 {
        "witness_v0_keyhash"
    } else if script.len() == 34 && script[0] == OP_0 && script[1] == 32 {
        "witness_v0_scripthash"
    } else if script.len() == 34 && script[0] == OP_1 && script[1] == 32 {
        "witness_v1_taproot"
    } else if script.first() == Some(&OP_RETURN) {
        "nulldata"
    } else {
        "nonstandard"
    }
}

#[cfg(test)]
mod createraw_locks {
    use super::{RawTxRpc, script_pubkey_type};
    use blvm_protocol::opcodes::{OP_PUSHDATA1, OP_RETURN};
    use serde_json::json;

    #[test]
    fn three_satoshi_dust_and_eighty_byte_op_return() {
        assert_eq!(RawTxRpc::btc_amount_to_sats(0.00000003).unwrap(), 3);
        assert!(RawTxRpc::btc_amount_to_sats(-1.0).is_err());
        let payload = vec![0xab; 80];
        let script = RawTxRpc::op_return_script(&payload).unwrap();
        assert_eq!(script[0], OP_RETURN);
        assert_eq!(script[1], OP_PUSHDATA1);
        assert_eq!(script[2], 80);
        assert_eq!(&script[3..], payload.as_slice());
        assert_eq!(script_pubkey_type(&script), "nulldata");
    }

    #[tokio::test]
    async fn both_writers_emit_three_sats_and_an_eighty_byte_op_return() {
        use blvm_protocol::opcodes::OP_PUSHDATA1;
        let rpc = RawTxRpc::new();
        let txid = "11".repeat(32);
        let payload = "ab".repeat(80);
        let address = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";
        let object = rpc
            .createrawtransaction(&json!([
                [{"txid": txid, "vout": 0}],
                {address: 0.00000003, "data": payload}
            ]))
            .await
            .unwrap();
        let array = rpc
            .createrawtransaction(&json!([
                [{"txid": txid, "vout": 0}],
                [{"address": address, "amount": 0.00000003}, {"data": payload}]
            ]))
            .await
            .unwrap();
        for hex_tx in [object.as_str().unwrap(), array.as_str().unwrap()] {
            let (tx, _) =
                RawTxRpc::deserialize_transaction_with_witness(&hex::decode(hex_tx).unwrap())
                    .unwrap();
            assert!(tx.outputs.iter().any(|output| output.value == 3));
            let data = tx
                .outputs
                .iter()
                .find(|output| output.script_pubkey.first() == Some(&OP_RETURN))
                .unwrap();
            assert_eq!(data.script_pubkey[1], OP_PUSHDATA1);
            assert_eq!(data.script_pubkey[2], 80);
            assert_eq!(script_pubkey_type(data.script_pubkey.as_ref()), "nulldata");
        }
        assert!(
            rpc.createrawtransaction(&json!([
                [{"txid": txid, "vout": 0}],
                {address: -1.0}
            ]))
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn pool_parent_is_admitted_and_a_missing_output_is_rejected() {
        use crate::node::mempool::MempoolManager;
        use crate::storage::Storage;
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::{
            OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO, UtxoSet,
        };
        use std::sync::Arc;

        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let mempool = Arc::new(MempoolManager::new());
        let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
        policy.min_tx_fee = 0;
        mempool.set_policy_config(Some(policy));
        let funding = OutPoint {
            hash: [5u8; 32],
            index: 0,
        };
        let utxo = UTXO {
            value: 50_000,
            script_pubkey: vec![OP_1].into(),
            height: 1,
            is_coinbase: false,
        };
        storage.utxos().add_utxo(&funding, &utxo).unwrap();
        let mut set = UtxoSet::default();
        set.insert(funding, Arc::new(utxo));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
        let parent = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: funding,
                script_sig: Vec::new(),
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 40_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        assert!(mempool.add_transaction(parent.clone()).unwrap());
        let child = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: blvm_protocol::block::calculate_tx_id(&parent),
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 30_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        let child_hex = RawTxRpc::serialize_transaction_with_witness(&child, None);
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let admitted = rpc.sendrawtransaction(&json!([child_hex])).await.unwrap();
        assert_eq!(admitted.as_str().unwrap().len(), 64);
        assert!(
            mempool
                .get_transaction(&blvm_protocol::block::calculate_tx_id(&child))
                .is_some()
        );

        let missing = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [8u8; 32],
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 1_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        let missing_hex = RawTxRpc::serialize_transaction_with_witness(&missing, None);
        assert!(rpc.sendrawtransaction(&json!([missing_hex])).await.is_err());
    }

    #[tokio::test]
    async fn output_at_height_1001_reports_confirmations_and_a_spent_pool_output_is_null() {
        use crate::node::mempool::MempoolManager;
        use crate::storage::Storage;
        use crate::storage::hashing::hash_to_rpc_hex;
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::{
            BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO, UtxoSet,
        };
        use std::sync::Arc;

        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [1u8; 32],
            timestamp: 1_600_000_000,
            bits: 0x207fffff,
            nonce: 1,
        };
        storage.chain().initialize(&header).unwrap();
        let tip = storage.chain().get_tip_hash().unwrap().unwrap();
        storage.chain().update_tip(&tip, &header, 2000).unwrap();
        let created = OutPoint {
            hash: [6u8; 32],
            index: 0,
        };
        storage
            .utxos()
            .add_utxo(
                &created,
                &UTXO {
                    value: 50_000,
                    script_pubkey: vec![OP_1].into(),
                    height: 1001,
                    is_coinbase: true,
                },
            )
            .unwrap();
        let mempool = Arc::new(MempoolManager::new());
        let rpc =
            RawTxRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&mempool), None, None);
        let shown = rpc
            .gettxout(&json!([hash_to_rpc_hex(&created.hash), 0, false]))
            .await
            .unwrap();
        assert_eq!(shown["confirmations"].as_i64(), Some(2000 - 1001 + 1));
        assert_eq!(shown["coinbase"].as_bool(), Some(true));
        assert_eq!(shown["scriptPubKey"]["type"], "nonstandard");

        let parent = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [2u8; 32],
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 20_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        let parent_id = blvm_protocol::block::calculate_tx_id(&parent);
        let spent = OutPoint {
            hash: parent_id,
            index: 0,
        };
        let child = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: spent,
                script_sig: Vec::new(),
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 10_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        mempool.add_transaction(parent).unwrap();
        mempool.add_transaction(child).unwrap();
        let hidden = rpc
            .gettxout(&json!([hash_to_rpc_hex(&parent_id), 0, true]))
            .await
            .unwrap();
        assert!(hidden.is_null());
    }

    #[tokio::test]
    async fn unknown_txid_errors_and_pool_fee_is_the_base_fee() {
        use crate::node::mempool::MempoolManager;
        use crate::rpc::mempool::MempoolRpc;
        use crate::storage::Storage;
        use crate::storage::hashing::hash_to_rpc_hex;
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::{
            OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO, UtxoSet,
        };
        use std::sync::Arc;

        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let mempool = Arc::new(MempoolManager::new());
        let rpc =
            RawTxRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&mempool), None, None);
        assert!(
            rpc.getrawtransaction(&json!([hash_to_rpc_hex(&[9u8; 32])]))
                .await
                .is_err()
        );

        let funding = OutPoint {
            hash: [7u8; 32],
            index: 0,
        };
        let utxo = UTXO {
            value: 50_000,
            script_pubkey: vec![OP_1].into(),
            height: 1,
            is_coinbase: false,
        };
        storage.utxos().add_utxo(&funding, &utxo).unwrap();
        let mut set = UtxoSet::default();
        set.insert(funding, Arc::new(utxo.clone()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: funding,
                script_sig: Vec::new(),
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 40_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        assert!(mempool.add_transaction(tx.clone()).unwrap());
        let listing = MempoolRpc::with_dependencies(mempool.clone(), storage)
            .getrawmempool(&json!([true]))
            .await
            .unwrap();
        let entry = listing.as_object().unwrap().values().next().unwrap();
        let mut priced = UtxoSet::default();
        priced.insert(funding, Arc::new(utxo));
        let fee = mempool.calculate_transaction_fee(&tx, &priced) as f64 / 100_000_000.0;
        assert_eq!(entry["fees"]["base"].as_f64(), Some(fee));
    }

    #[tokio::test]
    async fn decoderaw_puts_one_op_1_witness_on_vin() {
        use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};
        let tx = Transaction {
            version: 2,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [1u8; 32],
                    index: 0,
                },
                script_sig: Vec::new().into(),
                sequence: 0xfffffffe,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 1,
                script_pubkey: RawTxRpc::op_return_script(&[0xab; 80]).unwrap().into(),
            }]
            .into(),
            lock_time: 0,
        };
        let witnesses = vec![vec![vec![blvm_protocol::opcodes::OP_1]]];
        let hex = RawTxRpc::serialize_transaction_with_witness(&tx, Some(&witnesses));
        let decoded = RawTxRpc::new()
            .decoderawtransaction(&json!([hex]))
            .await
            .unwrap();
        assert_eq!(decoded["vin"][0]["txinwitness"][0], "51");
        assert_eq!(decoded["vout"][0]["scriptPubKey"]["type"], "nulldata");
    }
}
