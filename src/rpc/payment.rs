//! Payment RPC commands
//!
//! Provides JSON-RPC methods for payment operations including:
//! - Creating payment requests
//! - Creating CTV covenant proofs
//! - Querying payment state
//! - Settlement monitoring

use crate::node::mempool::MempoolManager;
#[cfg(feature = "ctv")]
use crate::payment::covenant::CovenantEngine;
use crate::payment::processor::PaymentError;
use crate::payment::state_machine::{PaymentState, PaymentStateMachine};
use crate::rpc::params::{param_bool_default, param_str};
use crate::storage::Storage;
use crate::utils::current_timestamp;
use crate::{Hash, Transaction};
use blvm_protocol::payment::PaymentOutput;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::{debug, error};

/// Default number of confirmations before considering a payment "safe for release" (RPC and REST).
pub const DEFAULT_SAFE_DEPTH: u32 = 6;

/// Payment RPC handler
#[derive(Clone)]
pub struct PaymentRpc {
    state_machine: Option<Arc<PaymentStateMachine>>,
    mempool: Option<Arc<MempoolManager>>,
    storage: Option<Arc<Storage>>,
}

impl Default for PaymentRpc {
    fn default() -> Self {
        Self::new()
    }
}

impl PaymentRpc {
    /// Create a new payment RPC handler
    pub fn new() -> Self {
        Self {
            state_machine: None,
            mempool: None,
            storage: None,
        }
    }

    /// Create with payment state machine
    pub fn with_state_machine(state_machine: Arc<PaymentStateMachine>) -> Self {
        Self {
            state_machine: Some(state_machine),
            mempool: None,
            storage: None,
        }
    }

    /// Attach mempool + storage for chain-level path-3 verify (`verifyonchainpaymentbytx`).
    pub fn with_chain_access(
        mut self,
        mempool: Arc<MempoolManager>,
        storage: Arc<Storage>,
    ) -> Self {
        self.mempool = Some(mempool);
        self.storage = Some(storage);
        self
    }

    /// Get payment state machine (returns error if not available)
    fn get_state_machine(&self) -> Result<Arc<PaymentStateMachine>, PaymentError> {
        self.state_machine
            .as_ref()
            .ok_or_else(|| {
                PaymentError::ProcessingError("Payment state machine not available".to_string())
            })
            .map(Arc::clone)
    }

    /// Create a payment request
    ///
    /// Params: ["outputs", "merchant_data", "create_covenant"]
    /// - outputs: Array of payment outputs [{amount, script_pubkey}, ...]
    /// - merchant_data: Optional merchant data (hex string)
    /// - create_covenant: Whether to create CTV proof immediately (default: false)
    ///
    /// Returns: {payment_id, covenant_proof (optional)}
    pub async fn create_payment_request(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: createpaymentrequest");

        let state_machine = self.get_state_machine()?;

        // Parse outputs
        let outputs_value = params.get(0).ok_or_else(|| {
            PaymentError::ProcessingError("Missing 'outputs' parameter".to_string())
        })?;

        let outputs: Vec<PaymentOutput> = serde_json::from_value(outputs_value.clone())
            .map_err(|e| PaymentError::ProcessingError(format!("Invalid outputs format: {e}")))?;

        // Parse merchant_data (optional)
        let merchant_data = params
            .get(1)
            .and_then(|v| v.as_str())
            .and_then(|s| hex::decode(s).ok());

        // Parse create_covenant (optional, default: false)
        let create_covenant = param_bool_default(params, 2, false);

        // Create payment request
        let (payment_id, covenant_proof) = state_machine
            .create_payment_request(outputs, merchant_data, create_covenant)
            .await?;

        // Build response
        let mut response = json!({
            "payment_id": payment_id,
        });

        #[cfg(feature = "ctv")]
        {
            if let Some(proof) = covenant_proof {
                response["covenant_proof"] = serde_json::to_value(&proof).map_err(|e| {
                    PaymentError::ProcessingError(format!(
                        "Failed to serialize covenant proof: {}",
                        e
                    ))
                })?;
            }
        }

        Ok(response)
    }

    /// Create CTV covenant proof for existing payment request
    ///
    /// Params: ["payment_request_id"]
    /// - payment_request_id: ID of the payment request
    ///
    /// Returns: {covenant_proof}
    #[cfg(feature = "ctv")]
    pub async fn create_covenant_proof(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: createcovenantproof");

        let state_machine = self.get_state_machine()?;

        let payment_request_id = params.get(0).and_then(|v| v.as_str()).ok_or_else(|| {
            PaymentError::ProcessingError("Missing 'payment_request_id' parameter".to_string())
        })?;

        let covenant_proof = state_machine
            .create_covenant_proof(payment_request_id)
            .await?;

        Ok(serde_json::to_value(&covenant_proof).map_err(|e| {
            PaymentError::ProcessingError(format!("Failed to serialize covenant proof: {}", e))
        })?)
    }

    /// Get payment state
    ///
    /// Params: ["payment_request_id"]
    /// - payment_request_id: ID of the payment request
    ///
    /// Returns: {state, details}
    pub async fn get_payment_state(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: getpaymentstate");

        let state_machine = self.get_state_machine()?;

        let payment_request_id = param_str(params, 0).map(String::from).ok_or_else(|| {
            PaymentError::ProcessingError("Missing 'payment_request_id' parameter".to_string())
        })?;

        let state = state_machine.get_payment_state(&payment_request_id).await?;

        // Convert state to JSON
        let state_json = match &state {
            PaymentState::RequestCreated { request_id } => {
                json!({
                    "state": "request_created",
                    "request_id": request_id,
                })
            }
            #[cfg(feature = "ctv")]
            PaymentState::ProofCreated {
                request_id,
                covenant_proof,
            } => {
                json!({
                    "state": "proof_created",
                    "request_id": request_id,
                    "covenant_proof": serde_json::to_value(covenant_proof)
                        .map_err(|e| PaymentError::ProcessingError(
                            format!("Failed to serialize covenant proof: {}", e)
                        ))?,
                })
            }
            #[cfg(feature = "ctv")]
            PaymentState::ProofBroadcast {
                request_id,
                covenant_proof,
                broadcast_peers,
            } => {
                json!({
                    "state": "proof_broadcast",
                    "request_id": request_id,
                    "covenant_proof": serde_json::to_value(covenant_proof)
                        .map_err(|e| PaymentError::ProcessingError(
                            format!("Failed to serialize covenant proof: {}", e)
                        ))?,
                    "broadcast_peers": broadcast_peers.len(),
                })
            }
            PaymentState::InMempool {
                request_id,
                tx_hash,
            } => {
                json!({
                    "state": "in_mempool",
                    "request_id": request_id,
                    "tx_hash": hex::encode(tx_hash),
                })
            }
            PaymentState::Settled {
                request_id,
                tx_hash,
                block_hash,
                confirmation_count,
                ..
            } => {
                json!({
                    "state": "settled",
                    "request_id": request_id,
                    "tx_hash": hex::encode(tx_hash),
                    "block_hash": hex::encode(block_hash),
                    "confirmation_count": confirmation_count,
                    "safe_for_release": *confirmation_count >= DEFAULT_SAFE_DEPTH,
                })
            }
            PaymentState::ReorgPending {
                request_id,
                tx_hash,
                reason,
                ..
            } => {
                json!({
                    "state": "reorg_pending",
                    "request_id": request_id,
                    "tx_hash": hex::encode(tx_hash),
                    "reason": reason,
                })
            }
            PaymentState::Failed { request_id, reason } => {
                json!({
                    "state": "failed",
                    "request_id": request_id,
                    "reason": reason,
                })
            }
            #[cfg(not(feature = "ctv"))]
            #[allow(unreachable_patterns)]
            PaymentState::ProofCreated { .. } | PaymentState::ProofBroadcast { .. } => {
                unreachable!("CTV variants should not exist when CTV feature is disabled")
            }
        };

        Ok(state_json)
    }

    /// Verify an on-chain payment against node state (path 3 / mesh adapters).
    ///
    /// Params: [payment_request_id, tx_hash_hex]
    ///
    /// Returns:
    /// ```json
    /// {
    ///   "verified": bool,
    ///   "state": "in_mempool" | "settled" | "pending" | "failed" | "not_found",
    ///   "amount_sats": u64 | null,
    ///   "tx_hash": "hex" | null,
    ///   "reason": "string" | null
    /// }
    /// ```
    pub async fn verify_on_chain_payment(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: verifyonchainpayment");

        let state_machine = self.get_state_machine()?;

        let payment_request_id = param_str(params, 0).map(String::from).ok_or_else(|| {
            PaymentError::ProcessingError("Missing 'payment_request_id' parameter".to_string())
        })?;
        let tx_hash_hex = param_str(params, 1).map(String::from).ok_or_else(|| {
            PaymentError::ProcessingError("Missing 'tx_hash' parameter".to_string())
        })?;

        let tx_hash = decode_tx_hash(&tx_hash_hex)?;

        let amount_sats = state_machine
            .payment_request_amount_sats(&payment_request_id)
            .await
            .ok();

        let state = match state_machine.get_payment_state(&payment_request_id).await {
            Ok(state) => state,
            Err(PaymentError::RequestNotFound(_)) => {
                return Ok(json!({
                    "verified": false,
                    "state": "not_found",
                    "amount_sats": amount_sats,
                    "tx_hash": null,
                    "reason": "payment request not found",
                }));
            }
            Err(e) => return Err(e),
        };

        let response = match state {
            PaymentState::InMempool {
                request_id: _,
                tx_hash: stored,
            } if stored == tx_hash => json!({
                "verified": true,
                "state": "in_mempool",
                "amount_sats": amount_sats,
                "tx_hash": tx_hash_hex,
                "reason": null,
            }),
            PaymentState::Settled {
                request_id: _,
                tx_hash: stored,
                ..
            } if stored == tx_hash => json!({
                "verified": true,
                "state": "settled",
                "amount_sats": amount_sats,
                "tx_hash": tx_hash_hex,
                "reason": null,
            }),
            PaymentState::InMempool {
                tx_hash: stored, ..
            }
            | PaymentState::Settled {
                tx_hash: stored, ..
            } => json!({
                "verified": false,
                "state": "tx_mismatch",
                "amount_sats": amount_sats,
                "tx_hash": hex::encode(stored),
                "reason": "transaction hash does not match payment state",
            }),
            PaymentState::Failed { reason, .. } => json!({
                "verified": false,
                "state": "failed",
                "amount_sats": amount_sats,
                "tx_hash": null,
                "reason": reason,
            }),
            PaymentState::ReorgPending {
                tx_hash: stored,
                reason,
                ..
            } => {
                if stored == tx_hash {
                    json!({
                        "verified": false,
                        "state": "reorg_pending",
                        "amount_sats": amount_sats,
                        "tx_hash": tx_hash_hex,
                        "reason": reason,
                    })
                } else {
                    json!({
                        "verified": false,
                        "state": "tx_mismatch",
                        "amount_sats": amount_sats,
                        "tx_hash": hex::encode(stored),
                        "reason": "transaction hash does not match payment state",
                    })
                }
            }
            _ => json!({
                "verified": false,
                "state": "pending",
                "amount_sats": amount_sats,
                "tx_hash": null,
                "reason": "payment not yet in mempool or settled",
            }),
        };

        Ok(response)
    }

    /// Chain-level path-3 verify when local payment-request state is absent (two-node topology).
    ///
    /// Params: `[payment_request_id, tx_hash_hex, min_amount_sats]`
    ///
    /// When the payment request exists locally, outputs from the stored BIP70 request are used.
    /// Otherwise the tx is resolved from mempool/chain and total output value must meet
    /// `min_amount_sats` (weaker binding — see ops docs).
    pub async fn verify_on_chain_payment_by_tx(
        &self,
        params: &Value,
    ) -> Result<Value, PaymentError> {
        debug!("RPC: verifyonchainpaymentbytx");

        let state_machine = self.get_state_machine()?;

        let payment_request_id = param_str(params, 0).map(String::from).ok_or_else(|| {
            PaymentError::ProcessingError("Missing 'payment_request_id' parameter".to_string())
        })?;
        let tx_hash_hex = param_str(params, 1).map(String::from).ok_or_else(|| {
            PaymentError::ProcessingError("Missing 'tx_hash' parameter".to_string())
        })?;
        let min_amount_sats = params.get(2).and_then(|v| v.as_u64()).ok_or_else(|| {
            PaymentError::ProcessingError("Missing 'min_amount_sats' parameter".to_string())
        })?;

        if min_amount_sats == 0 {
            return Ok(json!({
                "verified": false,
                "state": "failed",
                "amount_sats": null,
                "tx_hash": null,
                "reason": "min_amount_sats must be non-zero",
            }));
        }

        let tx_hash = decode_tx_hash(&tx_hash_hex)?;

        // Fast path: local state machine tracks mempool/settled for this PR.
        if let Ok(state) = state_machine.get_payment_state(&payment_request_id).await {
            let use_local_state = matches!(
                state,
                PaymentState::InMempool { .. }
                    | PaymentState::Settled { .. }
                    | PaymentState::ReorgPending { .. }
            );
            if use_local_state {
                return self
                    .verify_on_chain_payment(&json!([payment_request_id, tx_hash_hex]))
                    .await;
            }
        }

        let expected_outputs = state_machine
            .payment_request_outputs(&payment_request_id)
            .await
            .ok();

        let Some((tx, chain_state)) = self.lookup_tx_chain_state(&tx_hash).await? else {
            return Ok(json!({
                "verified": false,
                "state": "not_found",
                "amount_sats": null,
                "tx_hash": null,
                "reason": "transaction not found in mempool or chain",
            }));
        };

        let amount_sats = if let Some(ref outputs) = expected_outputs {
            if !transaction_matches_outputs(&tx, outputs) {
                return Ok(json!({
                    "verified": false,
                    "state": "output_mismatch",
                    "amount_sats": null,
                    "tx_hash": tx_hash_hex,
                    "reason": "transaction outputs do not match payment request",
                }));
            }
            outputs.iter().filter_map(|o| o.amount).sum()
        } else {
            tx_output_total_sats(&tx)
        };

        if amount_sats < min_amount_sats {
            return Ok(json!({
                "verified": false,
                "state": "underpaid",
                "amount_sats": amount_sats,
                "tx_hash": tx_hash_hex,
                "reason": format!(
                    "output amount {amount_sats} sats below min_amount_sats {min_amount_sats}"
                ),
            }));
        }

        let state = match chain_state {
            TxChainState::InMempool => "in_mempool",
            TxChainState::Confirmed { .. } => "confirmed",
        };

        Ok(json!({
            "verified": true,
            "state": state,
            "amount_sats": amount_sats,
            "tx_hash": tx_hash_hex,
            "reason": null,
        }))
    }

    async fn lookup_tx_chain_state(
        &self,
        tx_hash: &Hash,
    ) -> Result<Option<(Transaction, TxChainState)>, PaymentError> {
        if let Some(ref mempool) = self.mempool {
            if let Some(tx) = mempool.get_transaction(tx_hash) {
                return Ok(Some((tx, TxChainState::InMempool)));
            }
        }

        if let Some(ref storage) = self.storage {
            if let Ok(Some(tx)) = storage.transactions().get_transaction(tx_hash) {
                let confirmations = storage
                    .transactions()
                    .get_metadata(tx_hash)
                    .ok()
                    .flatten()
                    .and_then(|meta| {
                        let block_height = storage
                            .blocks()
                            .get_height_by_hash(&meta.block_hash)
                            .ok()
                            .flatten()?;
                        let tip = storage.chain().get_height().ok().flatten().unwrap_or(0);
                        Some((tip.saturating_sub(block_height) + 1) as u32)
                    })
                    .unwrap_or(1);
                return Ok(Some((tx, TxChainState::Confirmed { confirmations })));
            }
        }

        if self.mempool.is_none() && self.storage.is_none() {
            return Err(PaymentError::ProcessingError(
                "chain access not available (mempool/storage not configured)".to_string(),
            ));
        }

        Ok(None)
    }

    /// Verify a CTV covenant proof for gate path 2 (external app / mesh adapters).
    ///
    /// Params: [covenant_proof_hex, output_index, amount_sats]
    ///
    /// Returns: `{ "verified": bool, "reason": string | null }`
    pub async fn verify_covenant_proof(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: verifycovenantproof");

        let proof_hex = param_str(params, 0).map(String::from).ok_or_else(|| {
            PaymentError::ProcessingError("Missing 'covenant_proof_hex' parameter".to_string())
        })?;
        let output_index = params.get(1).and_then(|v| v.as_u64()).ok_or_else(|| {
            PaymentError::ProcessingError("Missing 'output_index' parameter".to_string())
        })? as u32;
        let amount_sats = params.get(2).and_then(|v| v.as_u64()).ok_or_else(|| {
            PaymentError::ProcessingError("Missing 'amount_sats' parameter".to_string())
        })?;

        let proof_bytes = hex::decode(proof_hex.trim()).map_err(|e| {
            PaymentError::ProcessingError(format!("Invalid covenant_proof_hex: {e}"))
        })?;

        if proof_bytes.is_empty() {
            return Ok(json!({
                "verified": false,
                "reason": "covenant proof is empty",
            }));
        }

        #[cfg(not(feature = "ctv"))]
        {
            let _ = (output_index, amount_sats, proof_bytes);
            Ok(json!({
                "verified": false,
                "reason": "CTV feature not enabled on node",
            }))
        }

        #[cfg(feature = "ctv")]
        {
            use blvm_protocol::payment::CovenantProof;

            let proof: CovenantProof = bincode::deserialize(&proof_bytes)
                .or_else(|_| {
                    serde_json::from_slice(&proof_bytes).map_err(|e| {
                        PaymentError::ProcessingError(format!("covenant proof decode: {e}"))
                    })
                })
                .map_err(|e: PaymentError| e)?;

            let idx = output_index as usize;
            if idx >= proof.transaction_template.outputs.len() {
                return Ok(json!({
                    "verified": false,
                    "reason": "output_index out of range",
                }));
            }

            let template_output = &proof.transaction_template.outputs[idx];
            if template_output.value != amount_sats {
                return Ok(json!({
                    "verified": false,
                    "reason": "amount_sats mismatch at output_index",
                }));
            }

            let expected = vec![PaymentOutput {
                amount: Some(amount_sats),
                script: template_output.script_pubkey.clone(),
            }];

            let engine = CovenantEngine::new();
            let verified = engine.verify_covenant_proof(&proof, &expected)?;

            Ok(json!({
                "verified": verified,
                "reason": if verified { Value::Null } else { json!("template verification failed") },
            }))
        }
    }

    /// List all payment states
    ///
    /// Params: [] (no parameters)
    ///
    /// Returns: {payments: [{payment_id, state, ...}, ...]}
    pub async fn list_payments(&self, _params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: listpayments");

        let state_machine = self.get_state_machine()?;

        let states = state_machine.list_payment_states();

        let payments: Vec<Value> = states
            .iter()
            .map(|(payment_id, state)| {
                let state_str = match state {
                    PaymentState::RequestCreated { .. } => "request_created",
                    #[cfg(feature = "ctv")]
                    PaymentState::ProofCreated { .. } => "proof_created",
                    #[cfg(feature = "ctv")]
                    PaymentState::ProofBroadcast { .. } => "proof_broadcast",
                    #[cfg(not(feature = "ctv"))]
                    PaymentState::ProofCreated { .. } | PaymentState::ProofBroadcast { .. } => {
                        unreachable!("CTV variants should not exist when CTV feature is disabled")
                    }
                    PaymentState::InMempool { .. } => "in_mempool",
                    PaymentState::Settled { .. } => "settled",
                    PaymentState::ReorgPending { .. } => "reorg_pending",
                    PaymentState::Failed { .. } => "failed",
                };

                json!({
                    "payment_id": payment_id,
                    "state": state_str,
                })
            })
            .collect();

        Ok(json!({
            "payments": payments,
            "count": payments.len(),
        }))
    }

    // ========== VAULT RPC METHODS ==========

    /// Create a vault
    ///
    /// Params: ["vault_id", "deposit_amount", "withdrawal_script", "config"]
    /// - vault_id: Unique identifier for the vault
    /// - deposit_amount: Amount to deposit (satoshis)
    /// - withdrawal_script: Script pubkey for withdrawal (hex)
    /// - config: Vault configuration (optional)
    ///
    /// Returns: {vault_id, vault_state}
    #[cfg(feature = "ctv")]
    pub async fn create_vault(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: createvault");

        if !params.is_object() {
            return Err(PaymentError::ProcessingError(
                "Params must be a JSON object".to_string(),
            ));
        }

        let state_machine = self.get_state_machine()?;
        let vault_engine = state_machine.vault_engine().ok_or_else(|| {
            PaymentError::ProcessingError("Vault engine not available".to_string())
        })?;

        let vault_id = params["vault_id"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("vault_id required".to_string()))?
            .to_string();

        let deposit_amount = params["deposit_amount"]
            .as_u64()
            .ok_or_else(|| PaymentError::ProcessingError("deposit_amount required".to_string()))?;

        let withdrawal_script_hex = params["withdrawal_script"].as_str().ok_or_else(|| {
            PaymentError::ProcessingError("withdrawal_script required".to_string())
        })?;
        let withdrawal_script = hex::decode(withdrawal_script_hex).map_err(|e| {
            PaymentError::ProcessingError(format!("Invalid withdrawal_script: {}", e))
        })?;

        let config = if params["config"].is_object() {
            serde_json::from_value(params["config"].clone())
                .unwrap_or_else(|_| crate::payment::vault::VaultConfig::default())
        } else {
            crate::payment::vault::VaultConfig::default()
        };

        let vault_state =
            vault_engine.create_vault(&vault_id, deposit_amount, withdrawal_script, config)?;

        Ok(json!({
            "vault_id": vault_state.vault_id,
            "vault_state": serde_json::to_value(&vault_state)
                .map_err(|e| PaymentError::ProcessingError(format!("Serialization error: {}", e)))?,
        }))
    }

    /// Unvault funds (first step of withdrawal)
    ///
    /// Params: ["vault_id", "unvault_script"]
    /// - vault_id: Vault identifier
    /// - unvault_script: Script pubkey for unvault output (hex)
    ///
    /// Returns: {vault_id, vault_state}
    #[cfg(feature = "ctv")]
    pub async fn unvault(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: unvault");

        if !params.is_object() {
            return Err(PaymentError::ProcessingError(
                "Params must be a JSON object".to_string(),
            ));
        }

        let state_machine = self.get_state_machine()?;
        let vault_engine = state_machine.vault_engine().ok_or_else(|| {
            PaymentError::ProcessingError("Vault engine not available".to_string())
        })?;

        let vault_id = params["vault_id"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("vault_id required".to_string()))?
            .to_string();

        let unvault_script_hex = params["unvault_script"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("unvault_script required".to_string()))?;
        let unvault_script = hex::decode(unvault_script_hex)
            .map_err(|e| PaymentError::ProcessingError(format!("Invalid unvault_script: {}", e)))?;

        let vault_state = vault_engine
            .get_vault(&vault_id)?
            .ok_or_else(|| PaymentError::ProcessingError("Vault not found".to_string()))?;

        let updated_vault_state = vault_engine.unvault(&vault_state, unvault_script)?;

        Ok(json!({
            "vault_id": updated_vault_state.vault_id,
            "vault_state": serde_json::to_value(&updated_vault_state)
                .map_err(|e| PaymentError::ProcessingError(format!("Serialization error: {}", e)))?,
        }))
    }

    /// Withdraw from vault
    ///
    /// Params: ["vault_id", "withdrawal_script", "current_block_height"]
    /// - vault_id: Vault identifier
    /// - withdrawal_script: Final destination script (hex)
    /// - current_block_height: Current blockchain height
    ///
    /// Returns: {vault_id, vault_state}
    #[cfg(feature = "ctv")]
    pub async fn withdraw_from_vault(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: withdrawfromvault");

        if !params.is_object() {
            return Err(PaymentError::ProcessingError(
                "Params must be a JSON object".to_string(),
            ));
        }

        let state_machine = self.get_state_machine()?;
        let vault_engine = state_machine.vault_engine().ok_or_else(|| {
            PaymentError::ProcessingError("Vault engine not available".to_string())
        })?;

        let vault_id = params["vault_id"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("vault_id required".to_string()))?
            .to_string();

        let vault_state = vault_engine
            .get_vault(&vault_id)?
            .ok_or_else(|| PaymentError::ProcessingError("Vault not found".to_string()))?;

        let withdrawal_script_hex = params["withdrawal_script"].as_str().ok_or_else(|| {
            PaymentError::ProcessingError("withdrawal_script required".to_string())
        })?;
        let withdrawal_script = hex::decode(withdrawal_script_hex).map_err(|e| {
            PaymentError::ProcessingError(format!("Invalid withdrawal_script: {}", e))
        })?;

        let current_block_height = params["current_block_height"].as_u64().ok_or_else(|| {
            PaymentError::ProcessingError("current_block_height required".to_string())
        })?;

        let updated_vault_state =
            vault_engine.withdraw(&vault_state, withdrawal_script, current_block_height)?;

        Ok(json!({
            "vault_id": updated_vault_state.vault_id,
            "vault_state": serde_json::to_value(&updated_vault_state)
                .map_err(|e| PaymentError::ProcessingError(format!("Serialization error: {}", e)))?,
        }))
    }

    /// Get vault state
    ///
    /// Params: ["vault_id"]
    /// - vault_id: Vault identifier
    ///
    /// Returns: {vault_id, vault_state}
    #[cfg(feature = "ctv")]
    pub async fn get_vault_state(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: getvaultstate");

        if !params.is_object() {
            return Err(PaymentError::ProcessingError(
                "Params must be a JSON object".to_string(),
            ));
        }

        let state_machine = self.get_state_machine()?;
        let vault_engine = state_machine.vault_engine().ok_or_else(|| {
            PaymentError::ProcessingError("Vault engine not available".to_string())
        })?;

        let vault_id = params["vault_id"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("vault_id required".to_string()))?
            .to_string();

        let vault_state = vault_engine
            .get_vault(&vault_id)?
            .ok_or_else(|| PaymentError::ProcessingError("Vault not found".to_string()))?;

        Ok(json!({
            "vault_id": vault_state.vault_id,
            "vault_state": serde_json::to_value(&vault_state)
                .map_err(|e| PaymentError::ProcessingError(format!("Serialization error: {}", e)))?,
        }))
    }

    // ========== POOL RPC METHODS ==========

    /// Create a payment pool
    ///
    /// Params: ["pool_id", "initial_participants", "config"]
    /// - pool_id: Unique identifier for the pool
    /// - initial_participants: Array of [participant_id, contribution, script_pubkey_hex]
    /// - config: Pool configuration (optional)
    ///
    /// Returns: {pool_id, pool_state}
    #[cfg(feature = "ctv")]
    pub async fn create_pool(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: createpool");

        if !params.is_object() {
            return Err(PaymentError::ProcessingError(
                "Params must be a JSON object".to_string(),
            ));
        }

        let state_machine = self.get_state_machine()?;
        let pool_engine = state_machine.pool_engine().ok_or_else(|| {
            PaymentError::ProcessingError("Pool engine not available".to_string())
        })?;

        let pool_id = params["pool_id"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("pool_id required".to_string()))?
            .to_string();

        let participants_array = params["initial_participants"].as_array().ok_or_else(|| {
            PaymentError::ProcessingError("initial_participants required".to_string())
        })?;

        let mut initial_participants = Vec::new();
        for p in participants_array {
            let p_arr = p.as_array().ok_or_else(|| {
                PaymentError::ProcessingError("Each participant must be an array".to_string())
            })?;
            if p_arr.len() < 3 {
                return Err(PaymentError::ProcessingError(
                    "Each participant must have [participant_id, contribution, script_pubkey]"
                        .to_string(),
                ));
            }
            let participant_id = p_arr[0]
                .as_str()
                .ok_or_else(|| {
                    PaymentError::ProcessingError("participant_id required".to_string())
                })?
                .to_string();
            let contribution = p_arr[1].as_u64().ok_or_else(|| {
                PaymentError::ProcessingError("contribution required".to_string())
            })?;
            let script_hex = p_arr[2].as_str().ok_or_else(|| {
                PaymentError::ProcessingError("script_pubkey required".to_string())
            })?;
            let script = hex::decode(script_hex).map_err(|e| {
                PaymentError::ProcessingError(format!("Invalid script_pubkey: {}", e))
            })?;

            initial_participants.push((participant_id, contribution, script));
        }

        let config = if params["config"].is_object() {
            serde_json::from_value(params["config"].clone())
                .unwrap_or_else(|_| crate::payment::pool::PoolConfig::default())
        } else {
            crate::payment::pool::PoolConfig::default()
        };

        let pool_state = pool_engine.create_pool(&pool_id, initial_participants, config)?;

        Ok(json!({
            "pool_id": pool_state.pool_id,
            "pool_state": serde_json::to_value(&pool_state)
                .map_err(|e| PaymentError::ProcessingError(format!("Serialization error: {}", e)))?,
        }))
    }

    /// Join a payment pool
    ///
    /// Params: ["pool_id", "participant_id", "contribution", "script_pubkey"]
    /// - pool_id: Pool identifier
    /// - participant_id: ID of new participant
    /// - contribution: Contribution amount (satoshis)
    /// - script_pubkey: Participant's script pubkey (hex)
    ///
    /// Returns: {pool_id, pool_state}
    #[cfg(feature = "ctv")]
    pub async fn join_pool(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: joinpool");

        if !params.is_object() {
            return Err(PaymentError::ProcessingError(
                "Params must be a JSON object".to_string(),
            ));
        }

        let state_machine = self.get_state_machine()?;
        let pool_engine = state_machine.pool_engine().ok_or_else(|| {
            PaymentError::ProcessingError("Pool engine not available".to_string())
        })?;

        let pool_id = params["pool_id"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("pool_id required".to_string()))?
            .to_string();

        let participant_id = params["participant_id"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("participant_id required".to_string()))?
            .to_string();

        let contribution = params["contribution"]
            .as_u64()
            .ok_or_else(|| PaymentError::ProcessingError("contribution required".to_string()))?;

        let script_hex = params["script_pubkey"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("script_pubkey required".to_string()))?;
        let script_pubkey = hex::decode(script_hex)
            .map_err(|e| PaymentError::ProcessingError(format!("Invalid script_pubkey: {}", e)))?;

        let pool_state = pool_engine
            .get_pool(&pool_id)?
            .ok_or_else(|| PaymentError::ProcessingError("Pool not found".to_string()))?;

        let updated_pool_state =
            pool_engine.join_pool(&pool_state, &participant_id, contribution, script_pubkey)?;

        Ok(json!({
            "pool_id": updated_pool_state.pool_id,
            "pool_state": serde_json::to_value(&updated_pool_state)
                .map_err(|e| PaymentError::ProcessingError(format!("Serialization error: {}", e)))?,
        }))
    }

    /// Distribute funds from pool
    ///
    /// Params: ["pool_id", "distribution"]
    /// - pool_id: Pool identifier
    /// - distribution: Array of [participant_id, amount]
    ///
    /// Returns: {pool_id, pool_state, covenant_proof}
    #[cfg(feature = "ctv")]
    pub async fn distribute_pool(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: distributepool");

        if !params.is_object() {
            return Err(PaymentError::ProcessingError(
                "Params must be a JSON object".to_string(),
            ));
        }

        let state_machine = self.get_state_machine()?;
        let pool_engine = state_machine.pool_engine().ok_or_else(|| {
            PaymentError::ProcessingError("Pool engine not available".to_string())
        })?;

        let pool_id = params["pool_id"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("pool_id required".to_string()))?
            .to_string();

        let distribution_array = params["distribution"].as_array().ok_or_else(|| {
            PaymentError::ProcessingError("distribution array required".to_string())
        })?;

        let mut distribution = Vec::new();
        for d in distribution_array {
            let d_arr = d.as_array().ok_or_else(|| {
                PaymentError::ProcessingError(
                    "Each distribution entry must be an array".to_string(),
                )
            })?;
            if d_arr.len() < 2 {
                return Err(PaymentError::ProcessingError(
                    "Each distribution entry must have [participant_id, amount]".to_string(),
                ));
            }
            let participant_id = d_arr[0]
                .as_str()
                .ok_or_else(|| {
                    PaymentError::ProcessingError("participant_id required".to_string())
                })?
                .to_string();
            let amount = d_arr[1]
                .as_u64()
                .ok_or_else(|| PaymentError::ProcessingError("amount required".to_string()))?;
            distribution.push((participant_id, amount));
        }

        let pool_state = pool_engine
            .get_pool(&pool_id)?
            .ok_or_else(|| PaymentError::ProcessingError("Pool not found".to_string()))?;

        let (updated_pool_state, covenant_proof) =
            pool_engine.distribute(&pool_state, distribution)?;

        Ok(json!({
            "pool_id": updated_pool_state.pool_id,
            "pool_state": serde_json::to_value(&updated_pool_state)
                .map_err(|e| PaymentError::ProcessingError(format!("Serialization error: {}", e)))?,
            "covenant_proof": serde_json::to_value(&covenant_proof)
                .map_err(|e| PaymentError::ProcessingError(format!("Serialization error: {}", e)))?,
        }))
    }

    /// Get payment pool state
    ///
    /// Params: ["pool_id"]
    /// - pool_id: Pool identifier
    ///
    /// Returns: {pool_id, pool_state}
    #[cfg(feature = "ctv")]
    pub async fn get_pool_state(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: getpoolstate");

        if !params.is_object() {
            return Err(PaymentError::ProcessingError(
                "Params must be a JSON object".to_string(),
            ));
        }

        let state_machine = self.get_state_machine()?;
        let pool_engine = state_machine.pool_engine().ok_or_else(|| {
            PaymentError::ProcessingError("Pool engine not available".to_string())
        })?;

        let pool_id = params["pool_id"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("pool_id required".to_string()))?
            .to_string();

        let pool_state = pool_engine
            .get_pool(&pool_id)?
            .ok_or_else(|| PaymentError::ProcessingError("Pool not found".to_string()))?;

        Ok(json!({
            "pool_id": pool_state.pool_id,
            "pool_state": serde_json::to_value(&pool_state)
                .map_err(|e| PaymentError::ProcessingError(format!("Serialization error: {}", e)))?,
        }))
    }

    // ========== CONGESTION RPC METHODS ==========

    /// Create a transaction batch
    ///
    /// Params: ["batch_id", "target_fee_rate"]
    /// - batch_id: Unique identifier for the batch
    /// - target_fee_rate: Target fee rate (sat/vbyte, optional)
    ///
    /// Returns: {batch_id}
    #[cfg(feature = "ctv")]
    pub async fn create_batch(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: createbatch");

        if !params.is_object() {
            return Err(PaymentError::ProcessingError(
                "Params must be a JSON object".to_string(),
            ));
        }

        let state_machine = self.get_state_machine()?;
        let congestion_manager = state_machine.congestion_manager().ok_or_else(|| {
            PaymentError::ProcessingError("Congestion manager not available".to_string())
        })?;

        let batch_id = params["batch_id"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("batch_id required".to_string()))?
            .to_string();

        let target_fee_rate = params["target_fee_rate"].as_u64();

        let mut manager = congestion_manager.lock().await;
        let created_id = manager.create_batch(&batch_id, target_fee_rate);

        Ok(json!({
            "batch_id": created_id,
        }))
    }

    /// Add transaction to batch
    ///
    /// Params: ["batch_id", "tx_id", "outputs", "priority", "deadline"]
    /// - batch_id: Batch identifier
    /// - tx_id: Transaction ID
    /// - outputs: Array of payment outputs
    /// - priority: Transaction priority (low, normal, high, urgent)
    /// - deadline: Optional deadline (Unix timestamp)
    ///
    /// Returns: {batch_id, batch_size}
    #[cfg(feature = "ctv")]
    pub async fn add_to_batch(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: addtobatch");

        if !params.is_object() {
            return Err(PaymentError::ProcessingError(
                "Params must be a JSON object".to_string(),
            ));
        }

        let state_machine = self.get_state_machine()?;
        let congestion_manager = state_machine.congestion_manager().ok_or_else(|| {
            PaymentError::ProcessingError("Congestion manager not available".to_string())
        })?;

        let batch_id = params["batch_id"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("batch_id required".to_string()))?
            .to_string();

        let tx_id = params["tx_id"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("tx_id required".to_string()))?
            .to_string();

        // Parse outputs
        let outputs_array = params["outputs"]
            .as_array()
            .ok_or_else(|| PaymentError::ProcessingError("outputs required".to_string()))?;

        let mut outputs = Vec::new();
        for o in outputs_array {
            let amount = o["amount"].as_u64();
            let script_hex = o["script_pubkey"].as_str().ok_or_else(|| {
                PaymentError::ProcessingError("script_pubkey required".to_string())
            })?;
            let script = hex::decode(script_hex).map_err(|e| {
                PaymentError::ProcessingError(format!("Invalid script_pubkey: {}", e))
            })?;

            outputs.push(PaymentOutput { script, amount });
        }

        let priority_str = params["priority"]
            .as_str()
            .unwrap_or("normal")
            .to_lowercase();
        let priority = match priority_str.as_str() {
            "low" => crate::payment::congestion::TransactionPriority::Low,
            "normal" => crate::payment::congestion::TransactionPriority::Normal,
            "high" => crate::payment::congestion::TransactionPriority::High,
            "urgent" => crate::payment::congestion::TransactionPriority::Urgent,
            _ => crate::payment::congestion::TransactionPriority::Normal,
        };

        let deadline = params["deadline"].as_u64();

        let pending_tx = crate::payment::congestion::PendingTransaction {
            tx_id,
            outputs,
            priority,
            created_at: current_timestamp(),
            deadline,
        };

        let mut manager = congestion_manager.lock().await;
        manager.add_to_batch(&batch_id, pending_tx)?;

        let batch = manager
            .get_batch(&batch_id)
            .ok_or_else(|| PaymentError::ProcessingError("Batch not found".to_string()))?;

        Ok(json!({
            "batch_id": batch_id,
            "batch_size": batch.transactions.len(),
        }))
    }

    /// Get congestion metrics
    ///
    /// Params: []
    ///
    /// Returns: {mempool_size, avg_fee_rate, median_fee_rate, estimated_blocks}
    #[cfg(feature = "ctv")]
    pub async fn get_congestion(&self, _params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: getcongestion");

        let state_machine = self.get_state_machine()?;
        let congestion_manager = state_machine.congestion_manager().ok_or_else(|| {
            PaymentError::ProcessingError("Congestion manager not available".to_string())
        })?;

        let manager = congestion_manager.lock().await;
        let metrics = manager.check_congestion()?;

        Ok(json!({
            "mempool_size": metrics.mempool_size,
            "avg_fee_rate": metrics.avg_fee_rate,
            "median_fee_rate": metrics.median_fee_rate,
            "estimated_blocks": metrics.estimated_blocks,
            "collected_at": metrics.collected_at,
        }))
    }

    /// Get congestion metrics (alias for get_congestion)
    #[cfg(feature = "ctv")]
    pub async fn get_congestion_metrics(&self, params: &Value) -> Result<Value, PaymentError> {
        self.get_congestion(params).await
    }

    /// Broadcast batch when conditions are optimal
    ///
    /// Params: ["batch_id"]
    /// - batch_id: Batch identifier
    ///
    /// Returns: {batch_id, covenant_proof, ready_to_broadcast}
    #[cfg(feature = "ctv")]
    pub async fn broadcast_batch(&self, params: &Value) -> Result<Value, PaymentError> {
        debug!("RPC: broadcastbatch");

        let state_machine = self.get_state_machine()?;
        let congestion_manager = state_machine.congestion_manager().ok_or_else(|| {
            PaymentError::ProcessingError("Congestion manager not available".to_string())
        })?;

        let batch_id = params["batch_id"]
            .as_str()
            .ok_or_else(|| PaymentError::ProcessingError("batch_id required".to_string()))?
            .to_string();

        let mut manager = congestion_manager.lock().await;
        let covenant_proof = manager.broadcast_batch(&batch_id)?;

        Ok(json!({
            "batch_id": batch_id,
            "covenant_proof": serde_json::to_value(&covenant_proof)
                .map_err(|e| PaymentError::ProcessingError(format!("Serialization error: {}", e)))?,
            "ready_to_broadcast": true,
        }))
    }
}

#[derive(Debug, Clone, Copy)]
enum TxChainState {
    InMempool,
    Confirmed { confirmations: u32 },
}

fn decode_tx_hash(hex_str: &str) -> Result<crate::Hash, PaymentError> {
    let bytes = hex::decode(hex_str)
        .map_err(|e| PaymentError::ProcessingError(format!("Invalid tx_hash hex: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| PaymentError::ProcessingError("tx_hash must be 32 bytes".to_string()))
}

fn tx_output_total_sats(tx: &Transaction) -> u64 {
    tx.outputs.iter().map(|o| o.value.max(0) as u64).sum()
}

fn transaction_matches_outputs(tx: &Transaction, expected_outputs: &[PaymentOutput]) -> bool {
    use blvm_protocol::types::{ByteString, Integer, TransactionOutput};

    let expected_tx_outputs: Vec<TransactionOutput> = expected_outputs
        .iter()
        .filter_map(|po| {
            let amount = po.amount?;
            Some(TransactionOutput {
                value: Integer::from(amount as i64),
                script_pubkey: ByteString::from(po.script.clone()),
            })
        })
        .collect();

    if expected_tx_outputs.is_empty() || tx.outputs.len() < expected_tx_outputs.len() {
        return false;
    }

    for expected_output in &expected_tx_outputs {
        let mut found = false;
        for tx_output in &tx.outputs {
            if tx_output.value == expected_output.value
                && tx_output.script_pubkey == expected_output.script_pubkey
            {
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }

    true
}
