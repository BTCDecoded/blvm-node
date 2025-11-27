//! Payment RPC commands
//!
//! Provides JSON-RPC methods for payment operations including:
//! - Creating payment requests
//! - Creating CTV covenant proofs
//! - Querying payment state
//! - Settlement monitoring

use crate::payment::processor::PaymentError;
use crate::payment::state_machine::{PaymentState, PaymentStateMachine};
use bllvm_protocol::payment::PaymentOutput;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, error};

/// Payment RPC handler
#[derive(Clone)]
pub struct PaymentRpc {
    state_machine: Option<Arc<PaymentStateMachine>>,
}

impl PaymentRpc {
    /// Create a new payment RPC handler
    pub fn new() -> Self {
        Self {
            state_machine: None,
        }
    }

    /// Create with payment state machine
    pub fn with_state_machine(state_machine: Arc<PaymentStateMachine>) -> Self {
        Self {
            state_machine: Some(state_machine),
        }
    }

    /// Get payment state machine (returns error if not available)
    fn get_state_machine(&self) -> Result<Arc<PaymentStateMachine>, PaymentError> {
        self.state_machine
            .as_ref()
            .ok_or_else(|| {
                PaymentError::ProcessingError("Payment state machine not available".to_string())
            })
            .map(|sm| Arc::clone(sm))
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
            .map_err(|e| PaymentError::ProcessingError(format!("Invalid outputs format: {}", e)))?;

        // Parse merchant_data (optional)
        let merchant_data = params
            .get(1)
            .and_then(|v| v.as_str())
            .and_then(|s| hex::decode(s).ok());

        // Parse create_covenant (optional, default: false)
        let create_covenant = params.get(2).and_then(|v| v.as_bool()).unwrap_or(false);

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

        let payment_request_id = params.get(0).and_then(|v| v.as_str()).ok_or_else(|| {
            PaymentError::ProcessingError("Missing 'payment_request_id' parameter".to_string())
        })?;

        let state = state_machine.get_payment_state(payment_request_id).await?;

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
            } => {
                json!({
                    "state": "settled",
                    "request_id": request_id,
                    "tx_hash": hex::encode(tx_hash),
                    "block_hash": hex::encode(block_hash),
                    "confirmation_count": confirmation_count,
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
            let participant_id = p[0]
                .as_str()
                .ok_or_else(|| {
                    PaymentError::ProcessingError("participant_id required".to_string())
                })?
                .to_string();
            let contribution = p[1].as_u64().ok_or_else(|| {
                PaymentError::ProcessingError("contribution required".to_string())
            })?;
            let script_hex = p[2].as_str().ok_or_else(|| {
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
            let participant_id = d[0]
                .as_str()
                .ok_or_else(|| {
                    PaymentError::ProcessingError("participant_id required".to_string())
                })?
                .to_string();
            let amount = d[1]
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
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
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
