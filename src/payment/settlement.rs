//! Settlement Monitoring and Verification
//!
//! Monitors payment transactions from mempool through on-chain confirmation.
//! Tracks UTXO state, mempool transactions, and block confirmations to update
//! payment state machine.

use crate::node::mempool::MempoolManager;
use crate::payment::processor::PaymentError;
use crate::payment::state_machine::PaymentStateMachine;
use crate::storage::Storage;
use crate::{Hash, Transaction};
use bllvm_protocol::payment::PaymentOutput;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

/// Settlement monitor for payment transactions
pub struct SettlementMonitor {
    /// Payment state machine to update
    state_machine: Arc<PaymentStateMachine>,
    /// Mempool manager for transaction monitoring
    mempool_manager: Option<Arc<MempoolManager>>,
    /// Storage for blockchain access
    storage: Option<Arc<Storage>>,
    /// Track payment outputs we're monitoring (payment_id -> outputs)
    monitored_payments: Arc<tokio::sync::RwLock<HashMap<String, Vec<PaymentOutput>>>>,
    /// Track expected transaction hashes (payment_id -> tx_hash)
    expected_transactions: Arc<tokio::sync::RwLock<HashMap<String, Hash>>>,
}

impl SettlementMonitor {
    /// Create a new settlement monitor
    pub fn new(state_machine: Arc<PaymentStateMachine>) -> Self {
        Self {
            state_machine,
            mempool_manager: None,
            storage: None,
            monitored_payments: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            expected_transactions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }

    /// Set mempool manager for transaction monitoring
    pub fn with_mempool_manager(mut self, mempool_manager: Arc<MempoolManager>) -> Self {
        self.mempool_manager = Some(mempool_manager);
        self
    }

    /// Set storage for blockchain access
    pub fn with_storage(mut self, storage: Arc<Storage>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Start monitoring a payment for settlement
    ///
    /// # Arguments
    ///
    /// * `payment_id` - Payment request ID to monitor
    /// * `expected_outputs` - Payment outputs to match against transactions
    /// * `expected_tx_hash` - Optional expected transaction hash (if known)
    pub async fn start_monitoring(
        &self,
        payment_id: &str,
        expected_outputs: Vec<PaymentOutput>,
        expected_tx_hash: Option<Hash>,
    ) -> Result<(), PaymentError> {
        debug!("Starting settlement monitoring for payment: {}", payment_id);

        // Store monitored outputs
        {
            let mut monitored = self.monitored_payments.write().await;
            monitored.insert(payment_id.to_string(), expected_outputs);
        }

        // Store expected transaction hash if provided
        if let Some(tx_hash) = expected_tx_hash {
            let mut expected = self.expected_transactions.write().await;
            expected.insert(payment_id.to_string(), tx_hash);
        }

        // Start background monitoring task
        let monitor = self.clone_for_monitoring();
        let payment_id_clone = payment_id.to_string();
        tokio::spawn(async move {
            monitor.monitor_payment_settlement(&payment_id_clone).await;
        });

        Ok(())
    }

    /// Stop monitoring a payment
    pub async fn stop_monitoring(&self, payment_id: &str) {
        let mut monitored = self.monitored_payments.write().await;
        monitored.remove(payment_id);
        let mut expected = self.expected_transactions.write().await;
        expected.remove(payment_id);
    }

    /// Monitor payment settlement (background task)
    async fn monitor_payment_settlement(&self, payment_id: &str) {
        info!("Monitoring settlement for payment: {}", payment_id);

        // Get expected outputs
        let expected_outputs = {
            let monitored = self.monitored_payments.read().await;
            monitored.get(payment_id).cloned()
        };

        if expected_outputs.is_none() {
            warn!("No expected outputs found for payment: {}", payment_id);
            return;
        }

        let expected_outputs = expected_outputs.unwrap();
        let expected_tx_hash = {
            let expected = self.expected_transactions.read().await;
            expected.get(payment_id).copied()
        };

        // Monitor for up to 24 hours (86400 seconds)
        let max_duration = Duration::from_secs(86400);
        let start_time = std::time::Instant::now();
        let check_interval = Duration::from_secs(10); // Check every 10 seconds

        loop {
            // Check timeout
            if start_time.elapsed() > max_duration {
                warn!("Settlement monitoring timeout for payment: {}", payment_id);
                let _ = self
                    .state_machine
                    .mark_failed(
                        payment_id,
                        "Settlement monitoring timeout (24 hours)".to_string(),
                    )
                    .await;
                break;
            }

            // Check mempool first
            if let Some(mempool_manager) = &self.mempool_manager {
                if let Ok(tx_hash) = self
                    .check_mempool(
                        mempool_manager,
                        payment_id,
                        &expected_outputs,
                        expected_tx_hash,
                    )
                    .await
                {
                    // Transaction found in mempool
                    let _ = self
                        .state_machine
                        .mark_in_mempool(payment_id, tx_hash)
                        .await;
                }
            }

            // Check blockchain for confirmation
            if let Some(storage) = &self.storage {
                if let Ok(Some((tx_hash, block_hash, confirmations))) = self
                    .check_blockchain(storage, payment_id, &expected_outputs, expected_tx_hash)
                    .await
                {
                    // Transaction confirmed
                    let _ = self
                        .state_machine
                        .mark_settled(payment_id, tx_hash, block_hash, confirmations)
                        .await;
                    break; // Stop monitoring once settled
                }
            }

            // Wait before next check
            sleep(check_interval).await;
        }

        // Clean up monitoring
        self.stop_monitoring(payment_id).await;
    }

    /// Check mempool for matching transaction
    async fn check_mempool(
        &self,
        mempool_manager: &MempoolManager,
        payment_id: &str,
        expected_outputs: &[PaymentOutput],
        expected_tx_hash: Option<Hash>,
    ) -> Result<Hash, PaymentError> {
        // If we have an expected transaction hash, check for it directly
        if let Some(expected_hash) = expected_tx_hash {
            if let Some(_tx) = mempool_manager.get_transaction(&expected_hash) {
                return Ok(expected_hash);
            }
        }

        // Otherwise, search for transactions matching expected outputs
        let transactions = mempool_manager.get_transactions();
        for tx in transactions {
            if self.transaction_matches_outputs(&tx, expected_outputs) {
                let tx_hash = crate::network::txhash::calculate_txid(&tx);
                debug!(
                    "Found matching transaction in mempool for payment {}: {}",
                    payment_id,
                    hex::encode(tx_hash)
                );
                return Ok(tx_hash);
            }
        }

        Err(PaymentError::ProcessingError(
            "Transaction not found in mempool".to_string(),
        ))
    }

    /// Check blockchain for confirmed transaction
    async fn check_blockchain(
        &self,
        storage: &Storage,
        payment_id: &str,
        expected_outputs: &[PaymentOutput],
        expected_tx_hash: Option<Hash>,
    ) -> Result<Option<(Hash, Hash, u32)>, PaymentError> {
        // If we have an expected transaction hash, check for it directly
        if let Some(expected_hash) = expected_tx_hash {
            if let Ok(Some(_tx)) = storage.transactions().get_transaction(&expected_hash) {
                // Get transaction metadata
                if let Ok(Some(metadata)) = storage.transactions().get_metadata(&expected_hash) {
                    let block_hash = metadata.block_hash;
                    let block_height = storage
                        .blocks()
                        .get_height_by_hash(&block_hash)
                        .map_err(|e| {
                            PaymentError::ProcessingError(format!(
                                "Failed to get block height: {}",
                                e
                            ))
                        })?
                        .ok_or_else(|| {
                            PaymentError::ProcessingError("Block height not found".to_string())
                        })?;
                    let tip_height = storage
                        .chain()
                        .get_height()
                        .map_err(|e| {
                            PaymentError::ProcessingError(format!(
                                "Failed to get tip height: {}",
                                e
                            ))
                        })?
                        .unwrap_or(0);
                    let confirmations = (tip_height.saturating_sub(block_height) + 1) as u32;

                    debug!(
                        "Found confirmed transaction for payment {}: {} (block: {}, confirmations: {})",
                        payment_id,
                        hex::encode(expected_hash),
                        hex::encode(block_hash),
                        confirmations
                    );

                    return Ok(Some((expected_hash, block_hash, confirmations)));
                }
            }
        }

        // Otherwise, search for transactions matching expected outputs
        // This is more expensive, so we only do it if we don't have an expected hash
        // In practice, we'd maintain an index of payment outputs -> transactions
        // For now, we'll rely on the expected transaction hash

        Ok(None)
    }

    /// Check if transaction matches expected payment outputs
    fn transaction_matches_outputs(
        &self,
        tx: &Transaction,
        expected_outputs: &[PaymentOutput],
    ) -> bool {
        // Convert expected outputs to transaction outputs for comparison
        use bllvm_consensus::types::{ByteString, Integer, TransactionOutput};
        let expected_tx_outputs: Vec<TransactionOutput> = expected_outputs
            .iter()
            .filter_map(|po| {
                // PaymentOutput.amount might be Option<u64> or u64, handle both
                let amount = if let Some(amt) = po.amount {
                    amt
                } else {
                    return None; // Skip if amount is None
                };
                Some(TransactionOutput {
                    value: Integer::from(amount as i64),
                    script_pubkey: ByteString::from(po.script.clone()),
                })
            })
            .collect();

        // Check if transaction has matching outputs
        if tx.outputs.len() < expected_tx_outputs.len() {
            return false;
        }

        // Check if all expected outputs are present (order-independent)
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

    /// Clone for background monitoring task
    fn clone_for_monitoring(&self) -> Self {
        Self {
            state_machine: Arc::clone(&self.state_machine),
            mempool_manager: self.mempool_manager.as_ref().map(Arc::clone),
            storage: self.storage.as_ref().map(Arc::clone),
            monitored_payments: Arc::clone(&self.monitored_payments),
            expected_transactions: Arc::clone(&self.expected_transactions),
        }
    }
}
