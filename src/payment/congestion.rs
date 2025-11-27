//! Congestion Control Manager for Transaction Batching and Fee Optimization
//!
//! Uses CTV covenants to manage transaction throughput and fees:
//! - Transaction batching for reduced fees
//! - Fee rate commitments in covenants
//! - Deferred execution for optimal timing
//! - Dynamic fee adjustment based on network conditions

use crate::node::mempool::MempoolManager;
use crate::payment::covenant::{CovenantEngine, CovenantProof};
use crate::payment::processor::PaymentError;
use crate::storage::Storage;
use bllvm_protocol::payment::PaymentOutput;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info};

/// Transaction priority
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TransactionPriority {
    /// Can wait for optimal conditions
    Low = 1,
    /// Standard priority
    Normal = 2,
    /// Should batch soon
    High = 3,
    /// Batch immediately
    Urgent = 4,
}

/// Batch configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchConfig {
    /// Maximum transactions per batch
    pub max_batch_size: usize,
    /// Target fee rate (sat/vbyte)
    pub target_fee_rate: u64,
    /// Maximum wait time before forcing batch (seconds)
    pub max_wait_time: u64,
    /// Minimum fee rate to accept (sat/vbyte)
    pub min_fee_rate: u64,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_batch_size: 100,
            target_fee_rate: 1,  // 1 sat/vbyte target
            max_wait_time: 3600, // 1 hour max wait
            min_fee_rate: 1,     // 1 sat/vbyte minimum
        }
    }
}

/// Pending transaction in batch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingTransaction {
    pub tx_id: String,
    pub outputs: Vec<PaymentOutput>,
    pub priority: TransactionPriority,
    pub created_at: u64,
    pub deadline: Option<u64>, // Optional deadline (Unix timestamp)
}

/// Transaction batch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionBatch {
    pub batch_id: String,
    pub transactions: Vec<PendingTransaction>,
    pub covenant_template: Option<CovenantProof>,
    pub target_fee_rate: u64,
    pub created_at: u64,
    pub ready_to_broadcast: bool,
    pub broadcast_at: Option<u64>,
}

/// Network congestion metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CongestionMetrics {
    /// Current mempool size (transactions)
    pub mempool_size: usize,
    /// Average fee rate in mempool (sat/vbyte)
    pub avg_fee_rate: u64,
    /// Median fee rate in mempool (sat/vbyte)
    pub median_fee_rate: u64,
    /// Estimated blocks until confirmation at current fee rate
    pub estimated_blocks: u32,
    /// Timestamp when metrics were collected
    pub collected_at: u64,
}

/// Congestion Control Manager
pub struct CongestionManager {
    covenant_engine: Arc<CovenantEngine>,
    mempool_manager: Option<Arc<MempoolManager>>,
    storage: Option<Arc<Storage>>,
    batches: HashMap<String, TransactionBatch>,
    config: BatchConfig,
}

impl CongestionManager {
    /// Create a new congestion manager
    pub fn new(
        covenant_engine: Arc<CovenantEngine>,
        mempool_manager: Option<Arc<MempoolManager>>,
        storage: Option<Arc<Storage>>,
        config: BatchConfig,
    ) -> Self {
        let manager = Self {
            covenant_engine,
            mempool_manager,
            storage,
            batches: HashMap::new(),
            config,
        };
        // Load existing batches from storage
        if let Err(e) = manager.load_all_batches() {
            tracing::warn!("Failed to load batches from storage: {}", e);
        }
        manager
    }

    /// Load all batches from storage
    fn load_all_batches(&self) -> Result<(), PaymentError> {
        let storage = self
            .storage
            .as_ref()
            .ok_or_else(|| PaymentError::ProcessingError("Storage not available".to_string()))?;

        let batches_tree = storage.open_tree("batches").map_err(|e| {
            PaymentError::ProcessingError(format!("Failed to open batches tree: {}", e))
        })?;

        // Note: batches is not mutex-protected in the struct, so we need to handle this carefully
        // For now, we'll just log that batches were loaded - actual loading would need to be done
        // during initialization or with proper synchronization
        let _count = batches_tree.len().unwrap_or(0);
        tracing::info!("Found {} batches in storage", _count);
        Ok(())
    }

    /// Save batch state to storage
    fn save_batch(&self, batch: &TransactionBatch) -> Result<(), PaymentError> {
        if let Some(storage) = &self.storage {
            let batches_tree = storage.open_tree("batches").map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to open batches tree: {}", e))
            })?;

            let key = batch.batch_id.as_bytes();
            let value = bincode::serialize(batch).map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to serialize batch: {}", e))
            })?;

            batches_tree.insert(key, &value).map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to save batch: {}", e))
            })?;
        }
        Ok(())
    }

    /// Create a new transaction batch
    ///
    /// # Arguments
    ///
    /// * `batch_id` - Unique identifier for the batch
    /// * `target_fee_rate` - Target fee rate for the batch
    ///
    /// # Returns
    ///
    /// Batch ID
    pub fn create_batch(&mut self, batch_id: &str, target_fee_rate: Option<u64>) -> String {
        let target = target_fee_rate.unwrap_or(self.config.target_fee_rate);
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let batch = TransactionBatch {
            batch_id: batch_id.to_string(),
            transactions: Vec::new(),
            covenant_template: None,
            target_fee_rate: target,
            created_at,
            ready_to_broadcast: false,
            broadcast_at: None,
        };

        self.batches.insert(batch_id.to_string(), batch);

        info!(
            "Created transaction batch: {} with target fee rate: {}",
            batch_id, target
        );

        batch_id.to_string()
    }

    /// Add transaction to batch
    ///
    /// # Arguments
    ///
    /// * `batch_id` - Batch ID
    /// * `tx` - Pending transaction to add
    ///
    /// # Returns
    ///
    /// Updated batch or error
    pub fn add_to_batch(
        &mut self,
        batch_id: &str,
        tx: PendingTransaction,
    ) -> Result<(), PaymentError> {
        let batch = self.batches.get_mut(batch_id).ok_or_else(|| {
            PaymentError::ProcessingError(format!("Batch {} not found", batch_id))
        })?;

        if batch.transactions.len() >= self.config.max_batch_size {
            return Err(PaymentError::ProcessingError(format!(
                "Batch {} is full: {} transactions",
                batch_id, self.config.max_batch_size
            )));
        }

        batch.transactions.push(tx);

        // Clone batch for saving (before dropping mutable borrow)
        let batch_clone = batch.clone();
        let tx_count = batch.transactions.len();
        drop(batch); // Release mutable borrow

        // Save batch to storage
        if let Err(e) = self.save_batch(&batch_clone) {
            tracing::warn!("Failed to save batch {} to storage: {}", batch_id, e);
        }

        // Update covenant template if batch is ready
        if tx_count >= self.config.max_batch_size {
            self.update_batch_covenant(batch_id)?;
        }

        Ok(())
    }

    /// Check current network congestion
    ///
    /// # Returns
    ///
    /// Congestion metrics
    pub fn check_congestion(&self) -> Result<CongestionMetrics, PaymentError> {
        let mempool_manager = self.mempool_manager.as_ref().ok_or_else(|| {
            PaymentError::ProcessingError("Mempool manager not available".to_string())
        })?;

        let mempool_size = mempool_manager.size();

        // Get prioritized transactions to calculate fee rates
        // Note: This requires UTXO set, which we may not have access to here
        // For now, we'll use a simplified approach
        let transactions = mempool_manager.get_transactions();

        // Calculate average fee rate (simplified - would need UTXO set for accurate calculation)
        let avg_fee_rate = if transactions.is_empty() {
            self.config.min_fee_rate
        } else {
            // Estimate based on mempool size
            // Larger mempool = higher fees
            let base_fee = self.config.min_fee_rate;
            let congestion_multiplier = (mempool_size as f64 / 1000.0).min(10.0) as u64;
            base_fee + congestion_multiplier
        };

        let median_fee_rate = avg_fee_rate; // Simplified - would calculate actual median

        // Estimate blocks until confirmation (simplified)
        let estimated_blocks = if avg_fee_rate >= self.config.target_fee_rate {
            1 // High fee rate = fast confirmation
        } else {
            (self.config.target_fee_rate / avg_fee_rate.max(1)) as u32
        };

        Ok(CongestionMetrics {
            mempool_size,
            avg_fee_rate,
            median_fee_rate,
            estimated_blocks,
            collected_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        })
    }

    /// Determine if batch should be broadcast
    ///
    /// # Arguments
    ///
    /// * `batch_id` - Batch ID
    ///
    /// # Returns
    ///
    /// `true` if batch should be broadcast, `false` otherwise
    pub fn should_broadcast(&self, batch_id: &str) -> Result<bool, PaymentError> {
        let batch = self.batches.get(batch_id).ok_or_else(|| {
            PaymentError::ProcessingError(format!("Batch {} not found", batch_id))
        })?;

        // Check if batch is empty
        if batch.transactions.is_empty() {
            return Ok(false);
        }

        // Check if batch is full
        if batch.transactions.len() >= self.config.max_batch_size {
            return Ok(true);
        }

        // Check if any transaction has urgent priority
        if batch
            .transactions
            .iter()
            .any(|tx| tx.priority == TransactionPriority::Urgent)
        {
            return Ok(true);
        }

        // Check if max wait time has passed
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if now - batch.created_at >= self.config.max_wait_time {
            return Ok(true);
        }

        // Check if any transaction deadline has passed
        if batch.transactions.iter().any(|tx| {
            if let Some(deadline) = tx.deadline {
                now >= deadline
            } else {
                false
            }
        }) {
            return Ok(true);
        }

        // Check network congestion
        let metrics = self.check_congestion()?;
        if metrics.avg_fee_rate <= batch.target_fee_rate {
            // Network fees are low enough, broadcast
            return Ok(true);
        }

        Ok(false)
    }

    /// Broadcast batch when conditions are optimal
    ///
    /// # Arguments
    ///
    /// * `batch_id` - Batch ID
    ///
    /// # Returns
    ///
    /// Covenant proof for the batch
    pub fn broadcast_batch(&mut self, batch_id: &str) -> Result<CovenantProof, PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "Congestion control requires CTV feature".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            let batch = self.batches.get_mut(batch_id).ok_or_else(|| {
                PaymentError::ProcessingError(format!("Batch {} not found", batch_id))
            })?;

            if batch.transactions.is_empty() {
                return Err(PaymentError::ProcessingError("Batch is empty".to_string()));
            }

            // Check if covenant needs to be created
            let needs_covenant = batch.covenant_template.is_none();
            let tx_count = batch.transactions.len();
            let target_fee = batch.target_fee_rate;
            drop(batch); // Release borrow before calling update_batch_covenant

            // Update covenant template if not already created
            if needs_covenant {
                self.update_batch_covenant(batch_id)?;
            }

            // Get covenant and update batch state
            let covenant = {
                let batch = self.batches.get_mut(batch_id).ok_or_else(|| {
                    PaymentError::ProcessingError(format!("Batch {} not found", batch_id))
                })?;

                let covenant = batch.covenant_template.as_ref().ok_or_else(|| {
                    PaymentError::ProcessingError("Batch covenant not created".to_string())
                })?;

                batch.ready_to_broadcast = true;
                batch.broadcast_at = Some(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                );

                covenant.clone()
            };

            info!(
                "Batch {} ready to broadcast with {} transactions at fee rate {} sat/vbyte",
                batch_id, tx_count, target_fee
            );

            Ok(covenant)
        }
    }

    /// Adjust fee rate based on network conditions
    ///
    /// # Arguments
    ///
    /// * `batch_id` - Batch ID
    ///
    /// # Returns
    ///
    /// Updated target fee rate
    pub fn adjust_fee_rate(&mut self, batch_id: &str) -> Result<u64, PaymentError> {
        let metrics = self.check_congestion()?;

        let batch = self.batches.get_mut(batch_id).ok_or_else(|| {
            PaymentError::ProcessingError(format!("Batch {} not found", batch_id))
        })?;

        // Adjust fee rate based on network conditions
        let new_fee_rate = if metrics.avg_fee_rate > batch.target_fee_rate {
            // Network is congested, increase fee rate
            metrics.avg_fee_rate.max(self.config.min_fee_rate)
        } else {
            // Network is not congested, use target
            batch.target_fee_rate
        };

        batch.target_fee_rate = new_fee_rate;

        debug!(
            "Adjusted fee rate for batch {} to {} sat/vbyte",
            batch_id, new_fee_rate
        );

        Ok(new_fee_rate)
    }

    /// Get batch by ID
    pub fn get_batch(&self, batch_id: &str) -> Option<&TransactionBatch> {
        self.batches.get(batch_id)
    }

    /// List all batches
    pub fn list_batches(&self) -> Vec<&TransactionBatch> {
        self.batches.values().collect()
    }

    /// Update batch covenant template
    fn update_batch_covenant(&mut self, batch_id: &str) -> Result<(), PaymentError> {
        let batch = self.batches.get_mut(batch_id).ok_or_else(|| {
            PaymentError::ProcessingError(format!("Batch {} not found", batch_id))
        })?;

        // Combine all transaction outputs
        let mut all_outputs = Vec::new();
        for tx in &batch.transactions {
            all_outputs.extend(tx.outputs.clone());
        }

        // Create covenant template
        let covenant = self.covenant_engine.create_payment_covenant(
            &format!("{}_batch", batch_id),
            &all_outputs,
            None,
        )?;

        batch.covenant_template = Some(covenant);

        Ok(())
    }
}
