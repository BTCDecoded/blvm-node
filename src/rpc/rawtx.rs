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
use crate::network::NetworkManager;
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
pub fn base58check_decode(s: &str) -> Option<Vec<u8>> {
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

pub fn base58check_encode(payload: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let hash1 = Sha256::digest(payload);
    let hash2 = Sha256::digest(hash1);
    let mut data = payload.to_vec();
    data.extend_from_slice(&hash2[..4]);
    let mut zeros = 0usize;
    while zeros < data.len() && data[zeros] == 0 {
        zeros += 1;
    }
    let mut digits = vec![0u8];
    for byte in &data[zeros..] {
        let mut carry = *byte as u32;
        for digit in digits.iter_mut() {
            carry += (*digit as u32) * 256;
            *digit = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut out = "1".repeat(zeros);
    for digit in digits.iter().rev() {
        out.push(ALPHABET[*digit as usize] as char);
    }
    out
}

/// Decode a Bitcoin address to `script_pubkey` (Bech32/Bech32m and legacy Base58Check).
pub fn address_string_to_script_pubkey(address: &str) -> Result<Vec<u8>, RpcError> {
    use blvm_protocol::address::BitcoinAddress;
    use blvm_protocol::opcodes::{
        OP_CHECKSIG, OP_DUP, OP_EQUAL, OP_EQUALVERIFY, OP_HASH160, PUSH_20_BYTES,
    };

    if let Ok(addr) = BitcoinAddress::decode(address) {
        match (addr.witness_version, addr.witness_program.len()) {
            (0, 20) | (0, 32) => {
                let mut script = Vec::with_capacity(2 + addr.witness_program.len());
                script.push(0x00);
                script.push(addr.witness_program.len() as u8);
                script.extend_from_slice(&addr.witness_program);
                Ok(script)
            }
            (1, 32) => {
                let mut script = Vec::with_capacity(2 + addr.witness_program.len());
                script.push(blvm_protocol::opcodes::OP_1);
                script.push(addr.witness_program.len() as u8);
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
        let p2pkh = matches!(version, 0x00 | 0x6f);
        let p2sh = matches!(version, 0x05 | 0xc4);
        if p2pkh {
            let mut script = vec![OP_DUP, OP_HASH160, PUSH_20_BYTES];
            script.extend_from_slice(hash);
            script.extend_from_slice(&[OP_EQUALVERIFY, OP_CHECKSIG]);
            Ok(script)
        } else if p2sh {
            let mut script = vec![OP_HASH160, PUSH_20_BYTES];
            script.extend_from_slice(hash);
            script.push(OP_EQUAL);
            Ok(script)
        } else {
            Err(RpcError::invalid_address_format(
                address,
                Some("Base58Check version is not a P2PKH or P2SH version for this network"),
                None,
            ))
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
    network: Option<Arc<NetworkManager>>,
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
            network: None,
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
            network: None,
        }
    }

    /// Announce an accepted transaction when a network manager is wired.
    pub fn with_network(mut self, network: Option<Arc<NetworkManager>>) -> Self {
        self.network = network;
        self
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

        // Parse optional parameters. An omitted cap is 0.10 BTC/kvB unless high fees are allowed.
        let maxfeerate_btc_per_kvb: Option<f64> = param_f64(params, 1)
            .or_else(|| param_str(params, 1).and_then(|s| s.parse::<f64>().ok()));

        let allowhighfees: bool = param_bool_default(params, 2, false);
        let max_feerate_cap = if allowhighfees {
            None
        } else {
            Some(maxfeerate_btc_per_kvb.unwrap_or(0.10))
        };

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

            if spendable_output_already_unspent(storage, &txid, &tx)? {
                return Err(RpcError::new(
                    RpcErrorCode::TxAlreadyInChain,
                    "Transaction outputs already in utxo set",
                ));
            }

            if mempool.get_transaction(&txid).is_some() {
                return Err(RpcError::tx_already_in_mempool(&txid_hex));
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

                    // Look up this transaction's prevouts. A bad row elsewhere in the tree
                    // is not part of this check.
                    let mut utxo_set = blvm_protocol::UtxoSet::default();
                    for input in &tx.inputs {
                        match storage.read_chain_prevout(&input.prevout) {
                            Ok(Some(utxo)) => {
                                utxo_set.insert(input.prevout, std::sync::Arc::new(utxo));
                            }
                            Ok(None) => {}
                            Err(err) => {
                                return Err(RpcError::internal_error(format!(
                                    "sendrawtransaction: utxo read failed: {err}"
                                )));
                            }
                        }
                    }

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
                            return Err(RpcError::new(
                                RpcErrorCode::TxMissingInputs,
                                "bad-txns-inputs-missingorspent",
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

                    // Check the fee cap. allowhighfees skips it. An omitted maxfeerate is 0.10 BTC/kvB.
                    if let Some(max_feerate) = max_feerate_cap {
                        if !allowhighfees && fee_rate_btc_per_kvb > max_feerate {
                            return Err(RpcError::with_data(
                                RpcErrorCode::VerifyRejected,
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
                            if let Some(ref network) = self.network {
                                let fee = mempool.cached_fee_basis(&txid);
                                network.announce_accepted_transaction(txid, fee).await;
                            }
                        }
                        Ok(false) => {
                            return Err(RpcError::new(
                                RpcErrorCode::VerifyRejected,
                                "Transaction rejected by mempool policy",
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

    /// Submit a package. Parents must appear before the children that spend them.
    ///
    /// Params: [["hex", ...], maxfeerate (optional)]
    /// Each result is the txid and, when admission failed, `error`.
    pub async fn submitpackage(&self, params: &Value) -> RpcResult<Value> {
        let rawtxs = param_array(params, 0).ok_or_else(|| {
            RpcError::missing_parameter("package", Some("array of raw transaction hex strings"))
        })?;
        if rawtxs.is_empty() {
            return Err(RpcError::invalid_params("package is empty"));
        }
        let mut decoded = Vec::with_capacity(rawtxs.len());
        for value in rawtxs {
            let hex_string = value
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("package entries must be hex strings"))?;
            let bytes = hex::decode(hex_string)
                .map_err(|err| RpcError::invalid_params(format!("Invalid hex string: {err}")))?;
            let (tx, witnesses) = Self::deserialize_transaction_with_witness(&bytes)?;
            decoded.push((tx, witnesses));
        }
        let txids: Vec<_> = decoded
            .iter()
            .map(|(tx, _)| blvm_protocol::block::calculate_tx_id(tx))
            .collect();
        for (index, (tx, _)) in decoded.iter().enumerate() {
            for input in &tx.inputs {
                if let Some(parent) = txids.iter().position(|id| *id == input.prevout.hash) {
                    if parent > index {
                        return Err(RpcError::invalid_params(
                            "package parents must appear before their children",
                        ));
                    }
                }
            }
        }
        let maxfeerate = params.get(1).cloned().unwrap_or(Value::Null);
        let mut results = Vec::with_capacity(decoded.len());
        for (tx, witnesses) in &decoded {
            let txid = blvm_protocol::block::calculate_tx_id(tx);
            let txid_hex = Self::rpc_txid(&txid);
            let bytes = Self::serialize_transaction_with_witness(tx, Some(witnesses));
            let one = json!([bytes, maxfeerate]);
            match self.sendrawtransaction(&one).await {
                Ok(value) => results.push(json!({"txid": value, "error": Value::Null})),
                Err(err) => results.push(json!({"txid": txid_hex, "error": err.to_string()})),
            }
        }
        Ok(json!(results))
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

        let mempool = self
            .mempool
            .clone()
            .ok_or_else(|| RpcError::internal_error("mempool is not available"))?;
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        if let Some(storage) = &self.storage {
            for tx in &transactions {
                for input in &tx.inputs {
                    match storage.read_chain_prevout(&input.prevout) {
                        Ok(Some(utxo)) => {
                            utxo_set.insert(input.prevout, Arc::new(utxo));
                        }
                        Ok(None) => {}
                        Err(err) => {
                            return Err(RpcError::internal_error(format!(
                                "UTXO lookup failed: {err}"
                            )));
                        }
                    }
                }
            }
        }
        let armed = package_error.is_none() && transactions.len() > 1;
        if armed {
            let package_pairs: Vec<(
                blvm_protocol::Transaction,
                Option<Vec<blvm_protocol::segwit::Witness>>,
            )> = transactions
                .iter()
                .zip(all_witnesses.iter())
                .map(|(tx, witnesses)| (tx.clone(), Some(witnesses.clone())))
                .collect();
            mempool.arm_package_preview(&package_pairs, &utxo_set);
        }

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

            let consensus_ok = matches!(
                validation_result,
                Ok(blvm_protocol::ValidationResult::Valid)
            );
            let (fee_satoshis, admission_reason) =
                match mempool.preview_accept(tx, Some(tx_witnesses), &utxo_set) {
                    Ok(fee) if mempool.script_rejects(tx, Some(tx_witnesses), &utxo_set) => {
                        (0, Some("failed consensus checks".to_string()))
                    }
                    Ok(fee) => (fee, None),
                    Err(reason) => (0, Some(reason.to_string())),
                };
            let allowed = consensus_ok && admission_reason.is_none();
            let reject_reason = if !allowed {
                if !consensus_ok {
                    match validation_result {
                        Ok(blvm_protocol::ValidationResult::Invalid(reason)) => Some(reason),
                        Err(e) => Some(format!("Validation error: {e}")),
                        _ => Some("rejected".to_string()),
                    }
                } else {
                    admission_reason
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

            let fee_btc = fee_satoshis as f64 / 100_000_000.0;

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
            if allowed {
                if armed {
                    mempool.note_package_preview(tx, Some(tx_witnesses.as_slice()), &utxo_set);
                }
                for (index, output) in tx.outputs.iter().enumerate() {
                    utxo_set.insert(
                        blvm_protocol::OutPoint {
                            hash: txid,
                            index: index as u32,
                        },
                        Arc::new(blvm_protocol::UTXO {
                            value: output.value,
                            script_pubkey: output.script_pubkey.clone().into(),
                            height: mempool.unconfirmed_coin_height(),
                            is_coinbase: false,
                        }),
                    );
                }
            }
        }
        if armed {
            mempool.clear_package_preview();
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
                    "type": script_pubkey_type(output.script_pubkey.as_ref()),
                    "desc": script_pubkey_desc(output.script_pubkey.as_ref())
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
            let vin = tx
                .inputs
                .iter()
                .enumerate()
                .map(|(i, input)| {
                    let txinwitness: Vec<String> = witnesses
                        .and_then(|stacks| stacks.get(i))
                        .map(|stack| {
                            stack
                                .iter()
                                .map(|item| hex::encode(item.as_slice()))
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({
                        "txid": Self::rpc_txid(&input.prevout.hash),
                        "vout": input.prevout.index,
                        "scriptSig": {
                            "asm": script_asm(input.script_sig.as_ref()),
                            "hex": hex::encode(&input.script_sig)
                        },
                        "txinwitness": txinwitness,
                        "sequence": input.sequence
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({
                "txid": txid_hex,
                "hash": hash_hex,
                "version": tx.version,
                "size": total_size,
                "vsize": vsize,
                "weight": weight,
                "locktime": tx.lock_time,
                "vin": vin,
                "vout": tx.outputs.iter().enumerate().map(|(i, output)| json!({
                    "value": output.value as f64 / 100_000_000.0,
                    "n": i,
                    "scriptPubKey": {
                        "asm": "",
                        "hex": hex::encode(&output.script_pubkey),
                        "reqSigs": required_signatures(output.script_pubkey.as_ref()),
                        "type": script_pubkey_type(output.script_pubkey.as_ref()),
                    "desc": script_pubkey_desc(output.script_pubkey.as_ref()),
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
            match storage.transactions().get_transaction(&txid_array) {
                Ok(Some(tx)) => {
                    let block_witness = Self::block_witness_for_tx(storage, &txid_array);
                    let mempool_witness = self
                        .mempool
                        .as_ref()
                        .and_then(|m| m.get_transaction_witnesses(&txid_array));
                    let witnesses = block_witness.or(mempool_witness);
                    Self::format_getrawtransaction_response(&tx, witnesses.as_deref(), verbose)
                }
                Err(err) => Err(RpcError::internal_error(format!(
                    "getrawtransaction: storage read failed: {err}"
                ))),
                Ok(None) => {
                    if let Some((block_hash, height)) =
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
                }
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
        if n > u32::MAX as u64 {
            return Err(RpcError::invalid_params(
                "n does not fit in 4 bytes".to_string(),
            ));
        }

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
                                            "type": script_pubkey_type(output.script_pubkey.as_ref()),
                            "desc": script_pubkey_desc(output.script_pubkey.as_ref())
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
                        move || storage.read_chain_prevout(&outpoint)
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
                                    let best_hash =
                                        storage.chain().get_tip_hash()?.ok_or_else(|| {
                                            anyhow::anyhow!("chain tip hash is missing")
                                        })?;
                                    let tip_height =
                                        storage.chain().get_height()?.ok_or_else(|| {
                                            anyhow::anyhow!("chain tip height is missing")
                                        })?;
                                    Ok((best_hash, tip_height))
                                }
                            })
                            .await
                        },
                        timeout_dur2,
                    )
                    .await
                    {
                        Ok(Ok(Ok(pair))) => pair,
                        Ok(Ok(Err(e))) => {
                            return Err(RpcError::internal_error(format!("tip read failed: {e}")));
                        }
                        Ok(Err(e)) => {
                            return Err(RpcError::internal_error(format!(
                                "tip read task failed: {e}"
                            )));
                        }
                        Err(_) => {
                            return Err(RpcError::internal_error(
                                "Timeout reading chain tip".to_string(),
                            ));
                        }
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
                            "type": script_pubkey_type(utxo.script_pubkey.as_ref()),
                    "desc": script_pubkey_desc(utxo.script_pubkey.as_ref())
                        },
                        "coinbase": utxo.is_coinbase
                    }))
                }
                Ok(Ok(Ok(None))) => Ok(Value::Null),
                Ok(Ok(Err(e))) => Err(RpcError::internal_error(format!("UTXO lookup failed: {e}"))),
                Ok(Err(e)) => Err(RpcError::internal_error(format!(
                    "UTXO lookup task failed: {e}"
                ))),
                Err(_) => Err(RpcError::internal_error(
                    "Timeout getting UTXO from storage".to_string(),
                )),
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
                let expected = crate::storage::hashing::hash_from_rpc_hex(blockhash_str)
                    .map_err(|e| RpcError::invalid_params(format!("Invalid blockhash: {e}")))?;
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

            let txids: Vec<String> = matched.iter().map(Self::rpc_txid).collect();
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
                    let mut utxo_set = blvm_protocol::UtxoSet::default();
                    for input in &tx.inputs {
                        if let Ok(Some(utxo)) = storage.read_chain_prevout(&input.prevout) {
                            utxo_set.insert(input.prevout, Arc::new(utxo));
                        }
                    }
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
                            "type": script_pubkey_type(output.script_pubkey.as_ref()),
                    "desc": script_pubkey_desc(output.script_pubkey.as_ref())
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
                            "type": script_pubkey_type(output.script_pubkey.as_ref()),
                    "desc": script_pubkey_desc(output.script_pubkey.as_ref())
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

    fn block_witness_for_tx(
        storage: &Storage,
        txid: &blvm_protocol::Hash,
    ) -> Option<Vec<blvm_protocol::segwit::Witness>> {
        use blvm_protocol::block::calculate_tx_id;
        let block_hash = storage
            .transactions()
            .get_metadata(txid)
            .ok()
            .flatten()?
            .block_hash;
        let block = storage.blocks().get_block(&block_hash).ok().flatten()?;
        let index = block
            .transactions
            .iter()
            .position(|stored| calculate_tx_id(stored) == *txid)?;
        storage
            .blocks()
            .get_witness(&block_hash)
            .ok()
            .flatten()
            .and_then(|all| all.get(index).cloned())
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
        if sats > u64::MAX as f64 || sats > blvm_protocol::constants::MAX_MONEY as f64 {
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
        if locktime > u32::MAX as u64 {
            return Err(RpcError::invalid_params(
                "locktime does not fit in 4 bytes".to_string(),
            ));
        }
        let replaceable = param_bool_default(params, 3, true);
        let version = param_u64_default(params, 4, 2);
        if version > u32::MAX as u64 {
            return Err(RpcError::invalid_params(
                "version does not fit in 4 bytes".to_string(),
            ));
        }

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
            if vout > u32::MAX as u64 {
                return Err(RpcError::invalid_params(format!(
                    "Input {idx} vout does not fit in 4 bytes"
                )));
            }

            // Sequence: use provided value, or set based on RBF
            let sequence = if let Some(seq_val) = input_obj.get("sequence").and_then(|v| v.as_u64())
            {
                if seq_val > u32::MAX as u64 {
                    return Err(RpcError::invalid_params(format!(
                        "Input {idx} sequence does not fit in 4 bytes"
                    )));
                }
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

                    let amount = output_obj
                        .get("amount")
                        .and_then(|v| {
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

pub(crate) fn script_asm(script: &[u8]) -> String {
    let mut words = Vec::new();
    let mut i = 0;
    while i < script.len() {
        let op = script[i];
        i += 1;
        let push_len = if (1..=75).contains(&op) {
            Some((0, op as usize))
        } else if op == 0x4c && i < script.len() {
            Some((1, script[i] as usize))
        } else if op == 0x4d && i + 1 < script.len() {
            Some((2, u16::from_le_bytes([script[i], script[i + 1]]) as usize))
        } else if op == 0x4e && i + 3 < script.len() {
            Some((
                4,
                u32::from_le_bytes(script[i..i + 4].try_into().unwrap()) as usize,
            ))
        } else {
            None
        };
        if let Some((len_bytes, data_len)) = push_len {
            let start = i + len_bytes;
            if script.len() < start + data_len {
                words.push(opcode_name(op));
                break;
            }
            words.push(hex::encode(&script[start..start + data_len]));
            i = start + data_len;
        } else {
            words.push(opcode_name(op));
        }
    }
    words.join(" ")
}

fn opcode_name(op: u8) -> String {
    if (0x51..=0x60).contains(&op) {
        return format!("OP_{}", op - 0x50);
    }
    let name = match op {
        0x00 => "OP_0",
        0x4f => "OP_1NEGATE",
        0x50 => "OP_RESERVED",
        0x61 => "OP_NOP",
        0x62 => "OP_VER",
        0x63 => "OP_IF",
        0x64 => "OP_NOTIF",
        0x65 => "OP_VERIF",
        0x66 => "OP_VERNOTIF",
        0x67 => "OP_ELSE",
        0x68 => "OP_ENDIF",
        0x69 => "OP_VERIFY",
        0x6a => "OP_RETURN",
        0x6b => "OP_TOALTSTACK",
        0x6c => "OP_FROMALTSTACK",
        0x6d => "OP_2DROP",
        0x6e => "OP_2DUP",
        0x6f => "OP_3DUP",
        0x70 => "OP_2OVER",
        0x71 => "OP_2ROT",
        0x72 => "OP_2SWAP",
        0x73 => "OP_IFDUP",
        0x74 => "OP_DEPTH",
        0x75 => "OP_DROP",
        0x76 => "OP_DUP",
        0x77 => "OP_NIP",
        0x78 => "OP_OVER",
        0x79 => "OP_PICK",
        0x7a => "OP_ROLL",
        0x7b => "OP_ROT",
        0x7c => "OP_SWAP",
        0x7d => "OP_TUCK",
        0x7e => "OP_CAT",
        0x7f => "OP_SUBSTR",
        0x80 => "OP_LEFT",
        0x81 => "OP_RIGHT",
        0x82 => "OP_SIZE",
        0x83 => "OP_INVERT",
        0x84 => "OP_AND",
        0x85 => "OP_OR",
        0x86 => "OP_XOR",
        0x87 => "OP_EQUAL",
        0x88 => "OP_EQUALVERIFY",
        0x89 => "OP_RESERVED1",
        0x8a => "OP_RESERVED2",
        0x8b => "OP_1ADD",
        0x8c => "OP_1SUB",
        0x8d => "OP_2MUL",
        0x8e => "OP_2DIV",
        0x8f => "OP_NEGATE",
        0x90 => "OP_ABS",
        0x91 => "OP_NOT",
        0x92 => "OP_0NOTEQUAL",
        0x93 => "OP_ADD",
        0x94 => "OP_SUB",
        0x95 => "OP_MUL",
        0x96 => "OP_DIV",
        0x97 => "OP_MOD",
        0x98 => "OP_LSHIFT",
        0x99 => "OP_RSHIFT",
        0x9a => "OP_BOOLAND",
        0x9b => "OP_BOOLOR",
        0x9c => "OP_NUMEQUAL",
        0x9d => "OP_NUMEQUALVERIFY",
        0x9e => "OP_NUMNOTEQUAL",
        0x9f => "OP_LESSTHAN",
        0xa0 => "OP_GREATERTHAN",
        0xa1 => "OP_LESSTHANOREQUAL",
        0xa2 => "OP_GREATERTHANOREQUAL",
        0xa3 => "OP_MIN",
        0xa4 => "OP_MAX",
        0xa5 => "OP_WITHIN",
        0xa6 => "OP_RIPEMD160",
        0xa7 => "OP_SHA1",
        0xa8 => "OP_SHA256",
        0xa9 => "OP_HASH160",
        0xaa => "OP_HASH256",
        0xab => "OP_CODESEPARATOR",
        0xac => "OP_CHECKSIG",
        0xad => "OP_CHECKSIGVERIFY",
        0xae => "OP_CHECKMULTISIG",
        0xaf => "OP_CHECKMULTISIGVERIFY",
        0xb0 => "OP_NOP1",
        0xb1 => "OP_CHECKLOCKTIMEVERIFY",
        0xb2 => "OP_CHECKSEQUENCEVERIFY",
        0xb3 => "OP_CHECKTEMPLATEVERIFY",
        0xb4 => "OP_NOP5",
        0xb5 => "OP_NOP6",
        0xb6 => "OP_NOP7",
        0xb7 => "OP_NOP8",
        0xb8 => "OP_NOP9",
        0xb9 => "OP_NOP10",
        0xba => "OP_CHECKSIGADD",
        0xcc => "OP_CHECKSIGFROMSTACK",
        _ => return format!("OP_{op:02x}"),
    };
    name.to_string()
}

pub(crate) fn required_signatures(script: &[u8]) -> u64 {
    if script.last() != Some(&0xae) || script.len() < 3 {
        return 1;
    }
    let Some(required) = small_push_int(script[0]) else {
        return 1;
    };
    let Some(keys) = small_push_int(script[script.len() - 2]) else {
        return 1;
    };
    if required > keys {
        return 1;
    }
    let mut index = 1;
    let mut seen = 0u64;
    while index < script.len() - 2 {
        let op = script[index];
        index += 1;
        if !(1..=75).contains(&op) {
            return 1;
        }
        let data_len = op as usize;
        if index + data_len > script.len() - 2 {
            return 1;
        }
        index += data_len;
        seen += 1;
    }
    if seen == keys { required } else { 1 }
}

fn small_push_int(op: u8) -> Option<u64> {
    if op == 0x00 {
        Some(0)
    } else if (0x51..=0x60).contains(&op) {
        Some(u64::from(op - 0x50))
    } else {
        None
    }
}

const MAX_SCRIPT_SIZE: usize = 10_000;

fn output_is_unspendable(script: &[u8]) -> bool {
    script.len() > MAX_SCRIPT_SIZE || script.first() == Some(&blvm_protocol::opcodes::OP_RETURN)
}

fn spendable_output_already_unspent(
    storage: &crate::storage::Storage,
    txid: &blvm_protocol::Hash,
    tx: &blvm_protocol::Transaction,
) -> Result<bool, RpcError> {
    if is_genesis_coinbase(storage, txid) {
        return Ok(false);
    }
    for (index, output) in tx.outputs.iter().enumerate() {
        if output_is_unspendable(output.script_pubkey.as_ref()) {
            continue;
        }
        let outpoint = blvm_protocol::OutPoint {
            hash: *txid,
            index: index as u32,
        };
        match storage.read_chain_prevout(&outpoint) {
            Ok(Some(_)) => return Ok(true),
            Ok(None) => {}
            Err(err) => {
                return Err(RpcError::internal_error(format!(
                    "sendrawtransaction: output lookup failed: {err}"
                )));
            }
        }
    }
    Ok(false)
}

fn is_genesis_coinbase(storage: &crate::storage::Storage, txid: &blvm_protocol::Hash) -> bool {
    let Ok(Some(info)) = storage.chain().load_chain_info() else {
        return false;
    };
    let network =
        crate::storage::chainstate::consensus_network_from_stored_name(&info.chain_params.network);
    let block = blvm_consensus::block::genesis_block(network);
    block
        .transactions
        .first()
        .is_some_and(|coinbase| blvm_protocol::block::calculate_tx_id(coinbase) == *txid)
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
    } else if bare_pubkey(script).is_some() {
        "pubkey"
    } else if bare_multisig(script).is_some() {
        "multisig"
    } else if is_push_only_nulldata(script) {
        "nulldata"
    } else {
        "nonstandard"
    }
}

pub(crate) fn script_pubkey_desc(script: &[u8]) -> String {
    let body = match script_pubkey_type(script) {
        "pubkeyhash" => format!("pkh({})", hex::encode(&script[3..23])),
        "scripthash" => format!("sh({})", hex::encode(&script[2..22])),
        "witness_v0_keyhash" => format!("wpkh({})", hex::encode(&script[2..22])),
        "witness_v0_scripthash" => format!("wsh({})", hex::encode(&script[2..34])),
        "witness_v1_taproot" => format!("tr({})", hex::encode(&script[2..34])),
        "pubkey" => format!(
            "pk({})",
            hex::encode(bare_pubkey(script).unwrap_or_default())
        ),
        "multisig" => bare_multisig(script)
            .map(|(required, keys)| {
                let keys: Vec<String> = keys.iter().map(hex::encode).collect();
                format!("multi({required},{})", keys.join(","))
            })
            .unwrap_or_else(|| format!("raw({})", hex::encode(script))),
        _ => format!("raw({})", hex::encode(script)),
    };
    match descriptor_checksum(&body) {
        Some(sum) => format!("{body}#{sum}"),
        None => body,
    }
}

fn bare_pubkey(script: &[u8]) -> Option<&[u8]> {
    let (&len, rest) = script.split_first()?;
    let (&last, key) = rest.split_last()?;
    let sized = (len == 33 || len == 65) && key.len() == usize::from(len);
    if !(sized && last == blvm_protocol::opcodes::OP_CHECKSIG && pubkey_size(key)) {
        return None;
    }
    Some(key)
}

fn pubkey_size(key: &[u8]) -> bool {
    match key.first() {
        Some(2 | 3) => key.len() == 33,
        Some(4 | 6 | 7) => key.len() == 65,
        _ => false,
    }
}

fn bare_multisig(script: &[u8]) -> Option<(u64, Vec<&[u8]>)> {
    use blvm_protocol::opcodes::OP_CHECKMULTISIG;
    if script.last() != Some(&OP_CHECKMULTISIG) || script.len() < 3 {
        return None;
    }
    let required = small_push_int(script[0])?;
    let keys_n = small_push_int(script[script.len() - 2])?;
    if required == 0 || required > keys_n || keys_n > 20 {
        return None;
    }
    let mut index = 1;
    let mut keys = Vec::new();
    while index < script.len() - 2 {
        let len = *script.get(index)? as usize;
        if !(1..=75).contains(&len) {
            return None;
        }
        index += 1;
        let key = script.get(index..index + len)?;
        if !pubkey_size(key) {
            return None;
        }
        keys.push(key);
        index += len;
    }
    if index != script.len() - 2 || keys.len() as u64 != keys_n {
        return None;
    }
    Some((required, keys))
}

fn is_push_only_nulldata(script: &[u8]) -> bool {
    let Some((&first, mut rest)) = script.split_first() else {
        return false;
    };
    if first != blvm_protocol::opcodes::OP_RETURN {
        return false;
    }
    while !rest.is_empty() {
        let Some(consumed) = push_len(rest) else {
            return false;
        };
        rest = &rest[consumed..];
    }
    true
}

fn push_len(script: &[u8]) -> Option<usize> {
    let op = *script.first()?;
    if op <= 75 {
        let len = op as usize;
        (script.len() > len).then_some(1 + len)
    } else if (0x51..=0x60).contains(&op) || op == 0x00 {
        Some(1)
    } else if op == 0x4c {
        let len = *script.get(1)? as usize;
        (script.len() >= 2 + len).then_some(2 + len)
    } else if op == 0x4d {
        let len = u16::from_le_bytes(script.get(1..3)?.try_into().ok()?) as usize;
        (script.len() >= 3 + len).then_some(3 + len)
    } else if op == 0x4e {
        let len = u32::from_le_bytes(script.get(1..5)?.try_into().ok()?) as usize;
        (script.len() >= 5 + len).then_some(5 + len)
    } else {
        None
    }
}

const DESC_INPUT_CHARSET: &str = "0123456789()[],'/*abcdefgh@:$%{}\
IJKLMNOPQRSTUVWXYZ&+-.;<=>?!^_|~\
ijklmnopqrstuvwxyzABCDEFGH`#\"\\ ";
const DESC_CHECKSUM_CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

fn descriptor_checksum(body: &str) -> Option<String> {
    let mut chk = 1u64;
    let mut class = 0u64;
    let mut class_count = 0u32;
    for ch in body.chars() {
        let pos = DESC_INPUT_CHARSET.find(ch)? as u64;
        chk = desc_polymod(chk, pos & 31);
        class = class * 3 + (pos >> 5);
        class_count += 1;
        if class_count == 3 {
            chk = desc_polymod(chk, class);
            class = 0;
            class_count = 0;
        }
    }
    if class_count > 0 {
        chk = desc_polymod(chk, class);
    }
    for _ in 0..8 {
        chk = desc_polymod(chk, 0);
    }
    chk ^= 1;
    Some(
        (0..8)
            .map(|i| char::from(DESC_CHECKSUM_CHARSET[((chk >> (5 * (7 - i))) & 31) as usize]))
            .collect(),
    )
}

fn desc_polymod(chk: u64, value: u64) -> u64 {
    const GENERATOR: [u64; 5] = [
        0xf5dee51989,
        0xa9fdca3312,
        0x1bab10e32d,
        0x3706b1677a,
        0x644d626ffd,
    ];
    let top = chk >> 35;
    let mut chk = ((chk & 0x7_ffff_ffff) << 5) ^ value;
    for (i, generator) in GENERATOR.iter().enumerate() {
        if top & (1 << i) != 0 {
            chk ^= generator;
        }
    }
    chk
}

#[cfg(test)]
#[path = "rawtx_createraw_locks.rs"]
mod createraw_locks;

#[cfg(test)]
#[path = "rawtx_gettxout_tip_tests.rs"]
mod gettxout_tip_tests;

#[cfg(test)]
mod raw_tx_read_tests {
    use super::RawTxRpc;
    use crate::node::mempool::MempoolManager;
    use crate::storage::Storage;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn a_storage_read_error_is_not_a_missing_transaction() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let txid = [8u8; 32];
        storage
            .transactions()
            .insert_undecodable_transaction(&txid)
            .unwrap();
        let rpc = RawTxRpc::with_dependencies(
            Arc::clone(&storage),
            Arc::new(MempoolManager::new()),
            None,
            None,
        );
        let hex_id = crate::storage::hashing::hash_to_rpc_hex(&txid);
        let err = rpc
            .getrawtransaction(&json!([hex_id]))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("storage read failed"), "{err}");
        assert!(!err.contains("Transaction not found"), "{err}");

        let missing = [9u8; 32];
        let missing_err = rpc
            .getrawtransaction(&json!([crate::storage::hashing::hash_to_rpc_hex(&missing)]))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            missing_err.contains("Transaction not found"),
            "{missing_err}"
        );
    }
}

#[cfg(test)]
mod script_sig_asm_tests {
    use super::RawTxRpc;
    use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};

    #[test]
    fn script_sig_asm_names_op_1() {
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [1u8; 32],
                    index: 0,
                },
                script_sig: vec![0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 1,
                script_pubkey: vec![0x51],
            }]
            .into(),
            lock_time: 0,
        };
        let value = RawTxRpc::format_getrawtransaction_response(&tx, None, true).unwrap();
        assert_eq!(value["vin"][0]["scriptSig"]["hex"], "51");
        assert_eq!(value["vin"][0]["scriptSig"]["asm"], "OP_1");
    }

    #[test]
    fn bare_multisig_req_sigs_is_the_threshold() {
        let mut script = vec![0x52];
        for byte in [0x02u8, 0x03, 0x04] {
            script.push(0x21);
            script.push(byte);
            script.extend(std::iter::repeat(0x11).take(32));
        }
        script.extend([0x53, 0xae]);
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [1u8; 32],
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 1,
                script_pubkey: script,
            }]
            .into(),
            lock_time: 0,
        };
        let value = RawTxRpc::format_getrawtransaction_response(&tx, None, true).unwrap();
        assert_eq!(value["vout"][0]["scriptPubKey"]["reqSigs"], 2);
    }
}
