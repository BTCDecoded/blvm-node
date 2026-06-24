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
use blvm_protocol::payment::PaymentOutput;
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
    /// Tx cache for re-broadcast on reorg (optional)
    #[cfg(feature = "ctv")]
    tx_cache: Option<Arc<crate::payment::PaymentTxCache>>,
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
            #[cfg(feature = "ctv")]
            tx_cache: None,
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

    /// Set tx cache for re-broadcast on reorg
    #[cfg(feature = "ctv")]
    pub fn with_tx_cache(mut self, tx_cache: Arc<crate::payment::PaymentTxCache>) -> Self {
        self.tx_cache = Some(tx_cache);
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
                        .mark_settled(
                            payment_id,
                            tx_hash,
                            block_hash,
                            confirmations,
                            Some(expected_outputs.to_vec()),
                        )
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
            if let Some(tx) = mempool_manager.get_transaction(&expected_hash) {
                #[cfg(feature = "ctv")]
                if let Some(ref cache) = self.tx_cache {
                    cache.store(expected_hash, tx.clone());
                }
                return Ok(expected_hash);
            }
        }

        // Otherwise, search for transactions matching expected outputs
        let transactions = mempool_manager.get_transactions();
        for tx in transactions {
            if self.transaction_matches_outputs(&tx, expected_outputs) {
                let tx_hash = crate::network::txhash::calculate_txid(&tx);
                #[cfg(feature = "ctv")]
                if let Some(ref cache) = self.tx_cache {
                    cache.store(tx_hash, tx.clone());
                }
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
            if let Some(found) = self.lookup_confirmed_transaction(
                storage,
                payment_id,
                expected_hash,
                expected_outputs,
            )? {
                return Ok(Some(found));
            }
        }

        // Search the tx index by expected output scripts when hash is unknown or lookup failed.
        self.find_confirmed_by_outputs(storage, payment_id, expected_outputs)
    }

    fn lookup_confirmed_transaction(
        &self,
        storage: &Storage,
        payment_id: &str,
        tx_hash: Hash,
        expected_outputs: &[PaymentOutput],
    ) -> Result<Option<(Hash, Hash, u32)>, PaymentError> {
        let tx_index = storage.transactions();
        let tx = match tx_index.get_transaction(&tx_hash) {
            Ok(Some(tx)) => tx,
            Ok(None) => return Ok(None),
            Err(e) => {
                return Err(PaymentError::ProcessingError(format!(
                    "Failed to load transaction {}: {e}",
                    hex::encode(tx_hash)
                )));
            }
        };

        if !self.transaction_matches_outputs(&tx, expected_outputs) {
            return Ok(None);
        }

        #[cfg(feature = "ctv")]
        if let Some(ref cache) = self.tx_cache {
            cache.store(tx_hash, tx);
        }

        let Some(metadata) = tx_index.get_metadata(&tx_hash).map_err(|e| {
            PaymentError::ProcessingError(format!(
                "Failed to load metadata for {}: {e}",
                hex::encode(tx_hash)
            ))
        })?
        else {
            return Ok(None);
        };

        let tip_height = storage
            .chain()
            .get_height()
            .map_err(|e| PaymentError::ProcessingError(format!("Failed to get tip height: {e}")))?
            .unwrap_or(0);
        let confirmations = (tip_height.saturating_sub(metadata.block_height) + 1) as u32;

        debug!(
            "Found confirmed transaction for payment {}: {} (block: {}, confirmations: {})",
            payment_id,
            hex::encode(tx_hash),
            hex::encode(metadata.block_hash),
            confirmations
        );

        Ok(Some((tx_hash, metadata.block_hash, confirmations)))
    }

    fn find_confirmed_by_outputs(
        &self,
        storage: &Storage,
        payment_id: &str,
        expected_outputs: &[PaymentOutput],
    ) -> Result<Option<(Hash, Hash, u32)>, PaymentError> {
        if expected_outputs.is_empty() {
            return Ok(None);
        }

        use std::collections::HashSet;

        let tx_index = storage.transactions();
        let mut seen: HashSet<Hash> = HashSet::new();

        for output in expected_outputs {
            if output.script.is_empty() {
                continue;
            }
            let candidates = tx_index
                .get_transactions_by_address(&output.script)
                .map_err(|e| {
                    PaymentError::ProcessingError(format!(
                        "Failed to query transactions for payment output script: {e}"
                    ))
                })?;
            for tx in candidates {
                let tx_hash = blvm_protocol::block::calculate_tx_id(&tx);
                if !seen.insert(tx_hash) {
                    continue;
                }
                if let Some(found) = self.lookup_confirmed_transaction(
                    storage,
                    payment_id,
                    tx_hash,
                    expected_outputs,
                )? {
                    return Ok(Some(found));
                }
            }
        }

        Ok(None)
    }

    /// Check if transaction matches expected payment outputs
    fn transaction_matches_outputs(
        &self,
        tx: &Transaction,
        expected_outputs: &[PaymentOutput],
    ) -> bool {
        // Convert expected outputs to transaction outputs for comparison
        use blvm_protocol::types::{ByteString, Integer, TransactionOutput};
        let expected_tx_outputs: Vec<TransactionOutput> = expected_outputs
            .iter()
            .filter_map(|po| {
                // PaymentOutput.amount might be Option<u64> or u64, handle both
                let amount = po.amount?;
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
            #[cfg(feature = "ctv")]
            tx_cache: self.tx_cache.as_ref().map(Arc::clone),
            monitored_payments: Arc::clone(&self.monitored_payments),
            expected_transactions: Arc::clone(&self.expected_transactions),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PaymentConfig;
    use crate::config::{IndexingConfig, IndexingStrategy};
    use crate::payment::processor::PaymentProcessor;
    use crate::storage::database::DatabaseBackend;
    use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn sample_payment_tx(script: Vec<u8>, amount: u64) -> Transaction {
        Transaction {
            version: 2,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [1u8; 32],
                    index: 0,
                },
                script_sig: vec![],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: amount as i64,
                script_pubkey: script,
            }]
            .into(),
            lock_time: 0,
        }
    }

    #[test]
    fn transaction_matches_outputs_requires_all_scripts_and_amounts() {
        let processor =
            Arc::new(PaymentProcessor::new(PaymentConfig::default()).expect("payment processor"));
        let monitor = SettlementMonitor::new(Arc::new(PaymentStateMachine::new(processor)));

        let script_a = vec![0x51, 0x87];
        let script_b = vec![0x52, 0x88];
        let tx = Transaction {
            version: 2,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![
                TransactionOutput {
                    value: 10_000,
                    script_pubkey: script_a.clone(),
                },
                TransactionOutput {
                    value: 20_000,
                    script_pubkey: script_b.clone(),
                },
            ],
            lock_time: 0,
        };

        let expected = vec![
            PaymentOutput {
                script: script_a,
                amount: Some(10_000),
            },
            PaymentOutput {
                script: script_b,
                amount: Some(20_000),
            },
        ];

        assert!(monitor.transaction_matches_outputs(&tx, &expected));
        assert!(!monitor.transaction_matches_outputs(
            &tx,
            &[PaymentOutput {
                script: vec![0x53],
                amount: Some(10_000),
            }]
        ));
    }

    #[test]
    fn find_confirmed_by_outputs_matches_indexed_transaction() {
        let dir = TempDir::new().unwrap();
        let storage = Arc::new(
            Storage::with_backend_pruning_and_indexing(
                dir.path(),
                DatabaseBackend::Heed3,
                None,
                Some(IndexingConfig {
                    enable_address_index: true,
                    strategy: IndexingStrategy::Eager,
                    ..Default::default()
                }),
                None,
                None,
                None,
            )
            .expect("storage"),
        );

        let script = vec![0x51, 0x87];
        let tx = sample_payment_tx(script.clone(), 50_000);
        let block_hash = [0x42u8; 32];
        storage
            .transactions()
            .index_transaction(&tx, &block_hash, 5, 0)
            .expect("index tx");

        let processor =
            Arc::new(PaymentProcessor::new(PaymentConfig::default()).expect("payment processor"));
        let monitor = SettlementMonitor::new(Arc::new(PaymentStateMachine::new(processor)))
            .with_storage(storage.clone());

        let outputs = vec![PaymentOutput {
            script,
            amount: Some(50_000),
        }];
        let found = monitor
            .find_confirmed_by_outputs(&storage, "payment-index", &outputs)
            .expect("lookup")
            .expect("confirmed tx");

        let expected_hash = blvm_protocol::block::calculate_tx_id(&tx);
        assert_eq!(found.0, expected_hash);
        assert_eq!(found.1, block_hash);
        assert!(found.2 >= 1);
    }
}
