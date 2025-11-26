//! Mempool manager
//!
//! Handles transaction mempool management, validation, and relay.

use crate::config::{MempoolPolicyConfig, RbfConfig};
use anyhow::Result;
use bllvm_protocol::mempool::{has_conflict_with_tx, replacement_checks, signals_rbf, Mempool};
use bllvm_protocol::{Hash, OutPoint, Transaction, UtxoSet};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// RBF tracking information for a transaction
#[derive(Debug, Clone)]
struct RbfTracking {
    /// Number of times this transaction has been replaced
    replacement_count: u32,
    /// Timestamp of last replacement (Unix timestamp)
    last_replacement_time: u64,
    /// Original transaction hash (before any replacements)
    original_tx_hash: Hash,
}

/// Mempool manager
pub struct MempoolManager {
    /// Transaction mempool - stores full transactions by hash
    pub(crate) transactions: HashMap<Hash, Transaction>,
    /// Legacy mempool (HashSet of hashes) for compatibility
    #[allow(dead_code)]
    mempool: Mempool,
    #[allow(dead_code)]
    utxo_set: UtxoSet,
    /// Track spent outputs to detect conflicts
    pub(crate) spent_outputs: HashSet<OutPoint>,
    /// Sorted index by fee rate (descending) - Reverse<u64> for descending order
    /// Maps fee_rate -> Vec<Hash> (multiple transactions can have same fee rate)
    /// Uses RwLock for interior mutability to allow &self methods
    fee_index: RwLock<BTreeMap<Reverse<u64>, Vec<Hash>>>,
    /// Cache fee rates per transaction hash
    /// Uses RwLock for interior mutability to allow &self methods
    fee_cache: RwLock<HashMap<Hash, u64>>,
    /// RBF configuration (optional)
    /// Uses RwLock for interior mutability to allow setting config after Arc sharing
    rbf_config: RwLock<Option<RbfConfig>>,
    /// Mempool policy configuration (optional)
    /// Uses RwLock for interior mutability to allow setting config after Arc sharing
    policy_config: RwLock<Option<MempoolPolicyConfig>>,
    /// RBF tracking: transaction hash -> RBF tracking info
    /// Uses RwLock for interior mutability
    rbf_tracking: RwLock<HashMap<Hash, RbfTracking>>,
    /// Transaction timestamps: when each transaction was added
    /// Uses RwLock for interior mutability
    tx_timestamps: RwLock<HashMap<Hash, u64>>,
    /// Transaction dependency graph: child -> parent relationships
    /// Maps transaction hash to set of parent transaction hashes (transactions it depends on)
    /// Uses RwLock for interior mutability
    tx_dependencies: RwLock<HashMap<Hash, HashSet<Hash>>>,
    /// Reverse dependency graph: parent -> children relationships
    /// Maps transaction hash to set of child transaction hashes (transactions that depend on it)
    /// Uses RwLock for interior mutability
    tx_descendants: RwLock<HashMap<Hash, HashSet<Hash>>>,
}

impl MempoolManager {
    /// Create a new mempool manager
    pub fn new() -> Self {
        Self {
            transactions: HashMap::new(),
            mempool: Mempool::new(),
            utxo_set: HashMap::new(),
            spent_outputs: HashSet::new(),
            fee_index: RwLock::new(BTreeMap::new()),
            fee_cache: RwLock::new(HashMap::new()),
            rbf_config: RwLock::new(None),
            policy_config: RwLock::new(None),
            rbf_tracking: RwLock::new(HashMap::new()),
            tx_timestamps: RwLock::new(HashMap::new()),
            tx_dependencies: RwLock::new(HashMap::new()),
            tx_descendants: RwLock::new(HashMap::new()),
        }
    }

    /// Create a new mempool manager with RBF configuration
    pub fn with_rbf_config(rbf_config: Option<RbfConfig>) -> Self {
        Self {
            transactions: HashMap::new(),
            mempool: Mempool::new(),
            utxo_set: HashMap::new(),
            spent_outputs: HashSet::new(),
            fee_index: RwLock::new(BTreeMap::new()),
            fee_cache: RwLock::new(HashMap::new()),
            rbf_config: RwLock::new(rbf_config),
            policy_config: RwLock::new(None),
            rbf_tracking: RwLock::new(HashMap::new()),
            tx_timestamps: RwLock::new(HashMap::new()),
            tx_dependencies: RwLock::new(HashMap::new()),
            tx_descendants: RwLock::new(HashMap::new()),
        }
    }

    /// Set RBF configuration
    /// Uses interior mutability so it can be called even when MempoolManager is in an Arc
    pub fn set_rbf_config(&self, rbf_config: Option<RbfConfig>) {
        *self.rbf_config.write().unwrap() = rbf_config;
    }

    /// Set mempool policy configuration
    /// Uses interior mutability so it can be called even when MempoolManager is in an Arc
    pub fn set_policy_config(&self, policy_config: Option<MempoolPolicyConfig>) {
        *self.policy_config.write().unwrap() = policy_config;
    }

    /// Get current timestamp (Unix seconds)
    fn current_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs()
    }

    /// Start the mempool manager
    pub async fn start(&mut self) -> Result<()> {
        info!("Starting mempool manager");

        // Initialize mempool
        self.initialize_mempool().await?;

        // Start mempool processing loop
        self.process_loop().await?;

        Ok(())
    }

    /// Run mempool processing once (for testing)
    pub async fn process_once(&mut self) -> Result<()> {
        // Process pending transactions
        self.process_pending_transactions().await?;

        // Clean up old transactions
        self.cleanup_old_transactions().await?;

        Ok(())
    }

    /// Initialize mempool
    async fn initialize_mempool(&mut self) -> Result<()> {
        debug!("Initializing mempool");

        // Load existing mempool from storage if available
        // In a real implementation, this would restore mempool state

        Ok(())
    }

    /// Main mempool processing loop
    async fn process_loop(&mut self) -> Result<()> {
        loop {
            // Process pending transactions
            self.process_pending_transactions().await?;

            // Clean up old transactions
            self.cleanup_old_transactions().await?;

            // Small delay to prevent busy waiting
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }

    /// Process pending transactions
    async fn process_pending_transactions(&mut self) -> Result<()> {
        // In a real implementation, this would:
        // 1. Get new transactions from network
        // 2. Validate transactions using consensus-proof
        // 3. Add valid transactions to mempool
        // 4. Relay transactions to peers

        debug!("Processing pending transactions");
        Ok(())
    }

    /// Clean up old transactions
    async fn cleanup_old_transactions(&mut self) -> Result<()> {
        // Remove transactions that are too old
        let expiry_time = {
            if let Some(ref policy) = *self.policy_config.read().unwrap() {
                policy.mempool_expiry_hours * 3600
            } else {
                // No policy config, skip expiry check
                return Ok(());
            }
        };

        let current_time = Self::current_timestamp();
        let mut to_remove = Vec::new();

        {
            let timestamps = self.tx_timestamps.read().unwrap();
            for (hash, timestamp) in timestamps.iter() {
                if current_time.saturating_sub(*timestamp) > expiry_time {
                    to_remove.push(*hash);
                }
            }
        }

        for hash in to_remove {
            debug!("Removing expired transaction {}", hex::encode(hash));
            self.remove_transaction(&hash);
        }

        // Check mempool size limits and evict if necessary
        self.enforce_mempool_limits().await?;

        Ok(())
    }

    /// Enforce mempool size limits by evicting transactions if necessary
    async fn enforce_mempool_limits(&mut self) -> Result<()> {
        let policy = match self.policy_config.read().unwrap().as_ref() {
            Some(p) => p.clone(),
            None => return Ok(()), // No policy config, no limits
        };

        // Calculate current mempool size
        let current_size_mb = self.calculate_mempool_size_mb();
        let current_tx_count = self.transactions.len();

        // Check if we need to evict
        let needs_eviction =
            current_size_mb > policy.max_mempool_mb || current_tx_count > policy.max_mempool_txs;

        if !needs_eviction {
            return Ok(());
        }

        debug!(
            "Mempool size limit exceeded: {} MB / {} MB, {} txs / {} txs. Evicting transactions...",
            current_size_mb, policy.max_mempool_mb, current_tx_count, policy.max_mempool_txs
        );

        // Evict transactions based on strategy
        let target_size_mb = policy.max_mempool_mb;
        let target_tx_count = policy.max_mempool_txs;
        let policy = &policy;

        match &policy.eviction_strategy {
            crate::config::EvictionStrategy::LowestFeeRate => {
                self.evict_lowest_fee_rate(target_size_mb, target_tx_count)
                    .await?;
            }
            crate::config::EvictionStrategy::OldestFirst => {
                self.evict_oldest_first(target_size_mb, target_tx_count)
                    .await?;
            }
            crate::config::EvictionStrategy::LargestFirst => {
                self.evict_largest_first(target_size_mb, target_tx_count)
                    .await?;
            }
            crate::config::EvictionStrategy::NoDescendantsFirst => {
                self.evict_no_descendants_first(target_size_mb, target_tx_count)
                    .await?;
            }
            crate::config::EvictionStrategy::Hybrid => {
                self.evict_hybrid(target_size_mb, target_tx_count).await?;
            }
        }

        Ok(())
    }

    /// Calculate current mempool size in MB
    fn calculate_mempool_size_mb(&self) -> u64 {
        use bllvm_protocol::serialization::transaction::serialize_transaction;

        let total_bytes: usize = self
            .transactions
            .values()
            .map(|tx| serialize_transaction(tx).len())
            .sum();

        // Convert to MB (1 MB = 1,048,576 bytes)
        (total_bytes as u64) / 1_048_576
    }

    /// Evict transactions with lowest fee rate
    async fn evict_lowest_fee_rate(
        &mut self,
        target_size_mb: u64,
        target_tx_count: usize,
    ) -> Result<()> {
        use bllvm_protocol::serialization::transaction::serialize_transaction;

        // Get all transactions sorted by fee rate (ascending - lowest first)
        let mut tx_fee_rates: Vec<(Hash, u64, usize)> = self
            .transactions
            .iter()
            .map(|(hash, tx)| {
                let fee_rate = self
                    .fee_cache
                    .read()
                    .unwrap()
                    .get(hash)
                    .copied()
                    .unwrap_or(0);
                let size = serialize_transaction(tx).len();
                (*hash, fee_rate, size)
            })
            .collect();

        // Sort by fee rate (ascending) - lowest fee rate first
        tx_fee_rates.sort_by_key(|(_, fee_rate, _)| *fee_rate);

        // Evict until we're under limits
        let mut current_size_mb = self.calculate_mempool_size_mb();
        let mut current_tx_count = self.transactions.len();

        for (hash, _fee_rate, size) in tx_fee_rates {
            if current_size_mb <= target_size_mb && current_tx_count <= target_tx_count {
                break;
            }

            // Don't evict if it has descendants (would orphan them)
            let has_descendants = {
                let descendants = self.tx_descendants.read().unwrap();
                descendants
                    .get(&hash)
                    .map(|d| !d.is_empty())
                    .unwrap_or(false)
            };

            if !has_descendants {
                debug!("Evicting low fee rate transaction {}", hex::encode(hash));
                self.remove_transaction(&hash);
                current_size_mb = current_size_mb.saturating_sub((size as u64) / 1_048_576);
                current_tx_count -= 1;
            }
        }

        Ok(())
    }

    /// Evict oldest transactions first (FIFO)
    async fn evict_oldest_first(
        &mut self,
        target_size_mb: u64,
        target_tx_count: usize,
    ) -> Result<()> {
        use bllvm_protocol::serialization::transaction::serialize_transaction;

        // Get all transactions with timestamps, sorted by age (oldest first)
        let mut tx_ages: Vec<(Hash, u64, usize)> = {
            let timestamps = self.tx_timestamps.read().unwrap();
            self.transactions
                .iter()
                .filter_map(|(hash, tx)| {
                    timestamps.get(hash).map(|&timestamp| {
                        let size = serialize_transaction(tx).len();
                        (*hash, timestamp, size)
                    })
                })
                .collect()
        };

        // Sort by timestamp (ascending) - oldest first
        tx_ages.sort_by_key(|(_, timestamp, _)| *timestamp);

        // Evict until we're under limits
        let mut current_size_mb = self.calculate_mempool_size_mb();
        let mut current_tx_count = self.transactions.len();

        for (hash, _timestamp, size) in tx_ages {
            if current_size_mb <= target_size_mb && current_tx_count <= target_tx_count {
                break;
            }

            // Don't evict if it has descendants
            let has_descendants = {
                let descendants = self.tx_descendants.read().unwrap();
                descendants
                    .get(&hash)
                    .map(|d| !d.is_empty())
                    .unwrap_or(false)
            };

            if !has_descendants {
                debug!("Evicting old transaction {}", hex::encode(hash));
                self.remove_transaction(&hash);
                current_size_mb = current_size_mb.saturating_sub((size as u64) / 1_048_576);
                current_tx_count -= 1;
            }
        }

        Ok(())
    }

    /// Evict largest transactions first
    async fn evict_largest_first(
        &mut self,
        target_size_mb: u64,
        target_tx_count: usize,
    ) -> Result<()> {
        use bllvm_protocol::serialization::transaction::serialize_transaction;

        // Get all transactions sorted by size (descending - largest first)
        let mut tx_sizes: Vec<(Hash, usize)> = self
            .transactions
            .iter()
            .map(|(hash, tx)| {
                let size = serialize_transaction(tx).len();
                (*hash, size)
            })
            .collect();

        // Sort by size (descending) - largest first
        tx_sizes.sort_by_key(|(_, size)| std::cmp::Reverse(*size));

        // Evict until we're under limits
        let mut current_size_mb = self.calculate_mempool_size_mb();
        let mut current_tx_count = self.transactions.len();

        for (hash, size) in tx_sizes {
            if current_size_mb <= target_size_mb && current_tx_count <= target_tx_count {
                break;
            }

            // Don't evict if it has descendants
            let has_descendants = {
                let descendants = self.tx_descendants.read().unwrap();
                descendants
                    .get(&hash)
                    .map(|d| !d.is_empty())
                    .unwrap_or(false)
            };

            if !has_descendants {
                debug!(
                    "Evicting large transaction {} ({} bytes)",
                    hex::encode(hash),
                    size
                );
                self.remove_transaction(&hash);
                current_size_mb = current_size_mb.saturating_sub((size as u64) / 1_048_576);
                current_tx_count -= 1;
            }
        }

        Ok(())
    }

    /// Evict transactions with no descendants first (safest)
    async fn evict_no_descendants_first(
        &mut self,
        target_size_mb: u64,
        target_tx_count: usize,
    ) -> Result<()> {
        use bllvm_protocol::serialization::transaction::serialize_transaction;

        // Get all transactions with no descendants, sorted by fee rate (lowest first)
        let mut tx_no_descendants: Vec<(Hash, u64, usize)> = {
            let descendants = self.tx_descendants.read().unwrap();
            let fee_cache = self.fee_cache.read().unwrap();

            self.transactions
                .iter()
                .filter_map(|(hash, tx)| {
                    let has_descendants = descendants
                        .get(hash)
                        .map(|d| !d.is_empty())
                        .unwrap_or(false);

                    if !has_descendants {
                        let fee_rate = fee_cache.get(hash).copied().unwrap_or(0);
                        let size = serialize_transaction(tx).len();
                        Some((*hash, fee_rate, size))
                    } else {
                        None
                    }
                })
                .collect()
        };

        // Sort by fee rate (ascending) - lowest fee rate first
        tx_no_descendants.sort_by_key(|(_, fee_rate, _)| *fee_rate);

        // Evict until we're under limits
        let mut current_size_mb = self.calculate_mempool_size_mb();
        let mut current_tx_count = self.transactions.len();

        for (hash, _fee_rate, size) in tx_no_descendants {
            if current_size_mb <= target_size_mb && current_tx_count <= target_tx_count {
                break;
            }

            debug!(
                "Evicting transaction with no descendants {}",
                hex::encode(hash)
            );
            self.remove_transaction(&hash);
            current_size_mb = current_size_mb.saturating_sub((size as u64) / 1_048_576);
            current_tx_count -= 1;
        }

        Ok(())
    }

    /// Hybrid eviction: combine fee rate and age
    async fn evict_hybrid(&mut self, target_size_mb: u64, target_tx_count: usize) -> Result<()> {
        use bllvm_protocol::serialization::transaction::serialize_transaction;

        // Calculate score: lower fee rate + older age = higher eviction priority
        let current_time = Self::current_timestamp();
        let mut tx_scores: Vec<(Hash, u64, usize)> = {
            let timestamps = self.tx_timestamps.read().unwrap();
            let fee_cache = self.fee_cache.read().unwrap();

            self.transactions
                .iter()
                .filter_map(|(hash, tx)| {
                    let fee_rate = fee_cache.get(hash).copied().unwrap_or(0);
                    let age = timestamps
                        .get(hash)
                        .map(|&t| current_time.saturating_sub(t))
                        .unwrap_or(0);

                    // Score: normalize fee rate (lower = higher score) + age weight
                    // Use inverse fee rate (higher for lower fees) + age in seconds
                    // Normalize fee rate: use 1 / (fee_rate + 1) to avoid division by zero
                    let fee_score = if fee_rate > 0 {
                        1_000_000 / (fee_rate + 1) // Higher score for lower fee
                    } else {
                        1_000_000 // Max score for zero fee
                    };

                    // Age weight: 1 point per hour old
                    let age_score = age / 3600;

                    // Combined score (higher = evict first)
                    let score = fee_score + age_score;

                    let size = serialize_transaction(tx).len();
                    Some((*hash, score, size))
                })
                .collect()
        };

        // Sort by score (descending) - highest score (most evictable) first
        tx_scores.sort_by_key(|(_, score, _)| std::cmp::Reverse(*score));

        // Evict until we're under limits
        let mut current_size_mb = self.calculate_mempool_size_mb();
        let mut current_tx_count = self.transactions.len();

        for (hash, _score, size) in tx_scores {
            if current_size_mb <= target_size_mb && current_tx_count <= target_tx_count {
                break;
            }

            // Don't evict if it has descendants
            let has_descendants = {
                let descendants = self.tx_descendants.read().unwrap();
                descendants
                    .get(&hash)
                    .map(|d| !d.is_empty())
                    .unwrap_or(false)
            };

            if !has_descendants {
                debug!(
                    "Evicting transaction (hybrid strategy) {}",
                    hex::encode(hash)
                );
                self.remove_transaction(&hash);
                current_size_mb = current_size_mb.saturating_sub((size as u64) / 1_048_576);
                current_tx_count -= 1;
            }
        }

        Ok(())
    }

    /// Check if a transaction can replace an existing one (RBF)
    ///
    /// This wraps the consensus layer replacement_checks with RBF mode-specific logic
    ///
    /// `storage` is optional - if provided, can be used for conservative mode confirmation checks
    pub fn check_rbf_replacement(
        &self,
        new_tx: &Transaction,
        existing_tx: &Transaction,
        utxo_set: &UtxoSet,
        storage: Option<&crate::storage::Storage>,
    ) -> Result<bool> {
        use bllvm_protocol::block::calculate_tx_id;

        let rbf_config = match self.rbf_config.read().unwrap().as_ref() {
            Some(config) => config.clone(),
            None => {
                // No RBF config - use default BIP125 behavior
                return replacement_checks(new_tx, existing_tx, utxo_set, &self.mempool)
                    .map_err(|e| anyhow::anyhow!("RBF check failed: {}", e));
            }
        };

        // Check if RBF is disabled
        if matches!(rbf_config.mode, crate::config::RbfMode::Disabled) {
            return Ok(false);
        }

        // Use the cloned config for the rest of the function
        let rbf_config = &rbf_config;

        // Check if existing transaction signals RBF
        if !signals_rbf(existing_tx) {
            return Ok(false);
        }

        let existing_tx_hash = calculate_tx_id(existing_tx);
        let _new_tx_hash = calculate_tx_id(new_tx);

        // Check replacement count limit
        if let Some(tracking) = self.rbf_tracking.read().unwrap().get(&existing_tx_hash) {
            if tracking.replacement_count >= rbf_config.max_replacements_per_tx {
                warn!(
                    "RBF replacement rejected: max replacements ({}) exceeded for tx {}",
                    rbf_config.max_replacements_per_tx,
                    hex::encode(existing_tx_hash)
                );
                return Ok(false);
            }

            // Check cooldown period
            let current_time = Self::current_timestamp();
            let time_since_last = current_time.saturating_sub(tracking.last_replacement_time);
            if time_since_last < rbf_config.cooldown_seconds {
                warn!(
                    "RBF replacement rejected: cooldown period not met ({}s remaining) for tx {}",
                    rbf_config.cooldown_seconds - time_since_last,
                    hex::encode(existing_tx_hash)
                );
                return Ok(false);
            }
        }

        // Calculate fees and fee rates using the same method as MempoolManager
        let new_fee = self.calculate_transaction_fee(new_tx, utxo_set) as i64;
        let existing_fee = self.calculate_transaction_fee(existing_tx, utxo_set) as i64;

        // Calculate transaction sizes (simplified - use serialization length)
        use bllvm_protocol::serialization::transaction::serialize_transaction;
        let new_tx_size = serialize_transaction(new_tx).len();
        let existing_tx_size = serialize_transaction(existing_tx).len();

        if new_tx_size == 0 || existing_tx_size == 0 {
            return Ok(false);
        }

        // Check fee rate multiplier (mode-specific)
        let new_fee_scaled = (new_fee as u128)
            .checked_mul(existing_tx_size as u128)
            .ok_or_else(|| anyhow::anyhow!("Fee rate calculation overflow"))?;
        let existing_fee_scaled = (existing_fee as u128)
            .checked_mul(new_tx_size as u128)
            .ok_or_else(|| anyhow::anyhow!("Fee rate calculation overflow"))?;

        // Apply mode-specific multiplier
        let required_fee_scaled =
            (existing_fee_scaled as f64 * rbf_config.min_fee_rate_multiplier) as u128;
        if new_fee_scaled <= required_fee_scaled {
            warn!(
                "RBF replacement rejected: fee rate increase insufficient (required: {:.2}x, got: {:.2}x) for tx {}",
                rbf_config.min_fee_rate_multiplier,
                (new_fee_scaled as f64) / (existing_fee_scaled as f64),
                hex::encode(existing_tx_hash)
            );
            return Ok(false);
        }

        // Check absolute fee bump
        let min_fee_bump = rbf_config.min_fee_bump_satoshis as i64;
        if new_fee <= existing_fee + min_fee_bump {
            warn!(
                "RBF replacement rejected: absolute fee bump insufficient (required: {} sat, got: {} sat) for tx {}",
                min_fee_bump,
                new_fee - existing_fee,
                hex::encode(existing_tx_hash)
            );
            return Ok(false);
        }

        // Conservative mode: Check minimum confirmations
        // Note: Transactions in mempool have 0 confirmations. This check ensures that
        // if a transaction has been confirmed (which shouldn't be in mempool), we require
        // it to have minimum confirmations before allowing replacement.
        // In practice, mempool transactions will always have 0 confirmations, so this
        // check mainly serves as a safety mechanism.
        if matches!(rbf_config.mode, crate::config::RbfMode::Conservative)
            && rbf_config.min_confirmations > 0
        {
            if let Some(storage) = storage {
                // Check if transaction is in blockchain and has enough confirmations
                if let Ok(Some(metadata)) = storage.transactions().get_metadata(&existing_tx_hash) {
                    // Transaction is in a block - check confirmations
                    let block_hash = metadata.block_hash;
                    if let Ok(Some(block_height)) = storage.blocks().get_height_by_hash(&block_hash)
                    {
                        if let Ok(Some(tip_height)) = storage.chain().get_height() {
                            let confirmations = tip_height.saturating_sub(block_height) + 1;
                            if confirmations < rbf_config.min_confirmations as u64 {
                                warn!(
                                    "RBF replacement rejected: conservative mode requires {} confirmations, tx {} has {}",
                                    rbf_config.min_confirmations,
                                    hex::encode(existing_tx_hash),
                                    confirmations
                                );
                                return Ok(false);
                            }
                        }
                    }
                }
                // If transaction is not in blockchain (mempool only), confirmations = 0
                // For conservative mode, we might want to reject replacements of unconfirmed transactions
                // if min_confirmations > 0, but that would prevent all mempool RBF replacements.
                // So we allow it - the transaction is still in mempool and can be replaced.
            }
        }

        // Check conflict (must spend at least one input from existing tx)
        if !has_conflict_with_tx(new_tx, existing_tx) {
            return Ok(false);
        }

        // Aggressive mode: Check for package replacement support
        // Package replacement = replacing parent + child transactions together
        if matches!(rbf_config.mode, crate::config::RbfMode::Aggressive)
            && rbf_config.allow_package_replacements
        {
            // Check if new_tx has dependencies (child transactions) that should be replaced together
            // For now, we allow the replacement if the new transaction has higher fees
            // Full package replacement logic would require tracking transaction packages
            // This is a simplified implementation
            debug!(
                "Aggressive mode: allowing package replacement for tx {}",
                hex::encode(existing_tx_hash)
            );
        }

        // For the remaining BIP125 checks (new dependencies), use the consensus replacement_checks
        // but we've already applied our mode-specific fee requirements above
        // Note: replacement_checks will re-check fee rate, but we've already validated with our multiplier
        // So we call it to verify the other BIP125 rules (dependencies, etc.)
        // However, since we've already done stricter checks, if replacement_checks passes, we're good
        let bip125_result = replacement_checks(new_tx, existing_tx, utxo_set, &self.mempool)?;
        if !bip125_result {
            // BIP125 check failed (likely new dependencies issue)
            return Ok(false);
        }

        // All checks passed
        Ok(true)
    }

    /// Check ancestor/descendant limits for a transaction
    fn check_ancestor_descendant_limits(
        &self,
        tx: &Transaction,
        tx_hash: &Hash,
        policy: &MempoolPolicyConfig,
    ) -> Result<bool> {
        use bllvm_protocol::serialization::transaction::serialize_transaction;

        // Calculate transaction size
        let tx_size = serialize_transaction(tx).len() as u64;

        // Find all ancestors (transactions this tx depends on)
        let mut ancestors = HashSet::new();
        let mut to_process = vec![tx_hash.clone()];
        let mut processed = HashSet::new();

        while let Some(current_hash) = to_process.pop() {
            if processed.contains(&current_hash) {
                continue;
            }
            processed.insert(current_hash);

            if let Some(current_tx) = self.transactions.get(&current_hash) {
                for input in &current_tx.inputs {
                    // Find parent transaction that created this output
                    for (parent_hash, _parent_tx) in &self.transactions {
                        if parent_hash == &input.prevout.hash {
                            if !ancestors.contains(parent_hash) {
                                ancestors.insert(*parent_hash);
                                to_process.push(*parent_hash);
                            }
                            break;
                        }
                    }
                }
            }
        }

        // Calculate ancestor count and size
        let ancestor_count = ancestors.len() as u32;
        let ancestor_size: u64 = ancestors
            .iter()
            .filter_map(|h| self.transactions.get(h))
            .map(|t| serialize_transaction(t).len() as u64)
            .sum();

        // Check ancestor limits
        if ancestor_count + 1 > policy.max_ancestor_count {
            warn!(
                "Transaction {} exceeds max ancestor count: {} > {}",
                hex::encode(tx_hash),
                ancestor_count + 1,
                policy.max_ancestor_count
            );
            return Ok(false);
        }

        if ancestor_size + tx_size > policy.max_ancestor_size {
            warn!(
                "Transaction {} exceeds max ancestor size: {} > {}",
                hex::encode(tx_hash),
                ancestor_size + tx_size,
                policy.max_ancestor_size
            );
            return Ok(false);
        }

        // Find all descendants (transactions that depend on this tx)
        let mut descendants = HashSet::new();
        let mut to_process = vec![tx_hash.clone()];
        let mut processed = HashSet::new();

        while let Some(current_hash) = to_process.pop() {
            if processed.contains(&current_hash) {
                continue;
            }
            processed.insert(current_hash);

            if let Some(current_tx) = self.transactions.get(&current_hash) {
                // Find all outputs of current tx
                let output_outpoints: Vec<_> = (0..current_tx.outputs.len())
                    .map(|idx| OutPoint {
                        hash: current_hash,
                        index: idx as u64,
                    })
                    .collect();

                // Find transactions that spend these outputs
                for (child_hash, child_tx) in &self.transactions {
                    for input in &child_tx.inputs {
                        if output_outpoints.contains(&input.prevout) {
                            if !descendants.contains(child_hash) {
                                descendants.insert(*child_hash);
                                to_process.push(*child_hash);
                            }
                            break;
                        }
                    }
                }
            }
        }

        // Calculate descendant count and size
        let descendant_count = descendants.len() as u32;
        let descendant_size: u64 = descendants
            .iter()
            .filter_map(|h| self.transactions.get(h))
            .map(|t| serialize_transaction(t).len() as u64)
            .sum();

        // Check descendant limits
        if descendant_count + 1 > policy.max_descendant_count {
            warn!(
                "Transaction {} exceeds max descendant count: {} > {}",
                hex::encode(tx_hash),
                descendant_count + 1,
                policy.max_descendant_count
            );
            return Ok(false);
        }

        if descendant_size + tx_size > policy.max_descendant_size {
            warn!(
                "Transaction {} exceeds max descendant size: {} > {}",
                hex::encode(tx_hash),
                descendant_size + tx_size,
                policy.max_descendant_size
            );
            return Ok(false);
        }

        Ok(true)
    }

    /// Update dependency graph when a transaction is added
    fn update_dependency_graph(&self, tx: &Transaction, tx_hash: &Hash) {
        let mut dependencies = self.tx_dependencies.write().unwrap();
        let mut descendants = self.tx_descendants.write().unwrap();

        // Initialize empty sets for this transaction
        dependencies.entry(*tx_hash).or_insert_with(HashSet::new);
        descendants.entry(*tx_hash).or_insert_with(HashSet::new);

        // Find parent transactions (ancestors) - transactions that created inputs
        for input in &tx.inputs {
            // Find transaction that created this output
            for (parent_hash, _parent_tx) in &self.transactions {
                if parent_hash == &input.prevout.hash {
                    // This transaction depends on parent
                    dependencies
                        .entry(*tx_hash)
                        .or_insert_with(HashSet::new)
                        .insert(*parent_hash);

                    // Parent has this as a descendant
                    descendants
                        .entry(*parent_hash)
                        .or_insert_with(HashSet::new)
                        .insert(*tx_hash);

                    break;
                }
            }
        }
    }

    /// Add transaction to mempool
    pub async fn add_transaction(&mut self, tx: Transaction) -> Result<bool> {
        debug!("Adding transaction to mempool");

        use bllvm_protocol::block::calculate_tx_id;
        let tx_hash = calculate_tx_id(&tx);

        // Check for conflicts with existing mempool transactions
        // If conflict exists, check if RBF replacement is allowed
        let mut conflicting_tx_hash: Option<Hash> = None;
        for input in &tx.inputs {
            if let Some(existing_tx) = self
                .transactions
                .values()
                .find(|t| t.inputs.iter().any(|i| i.prevout == input.prevout))
            {
                let existing_hash = calculate_tx_id(existing_tx);
                conflicting_tx_hash = Some(existing_hash);
                break;
            }
        }

        // If there's a conflict, try RBF replacement
        if let Some(existing_hash) = conflicting_tx_hash {
            if let Some(existing_tx) = self.transactions.get(&existing_hash) {
                // Check if RBF replacement is allowed
                // Note: Storage is not available in MempoolManager context
                // Conservative mode confirmation checks will be skipped if storage is None
                if self.check_rbf_replacement(&tx, existing_tx, &self.utxo_set, None)? {
                    debug!(
                        "RBF replacement allowed, replacing transaction {}",
                        hex::encode(existing_hash)
                    );

                    // Remove existing transaction
                    self.remove_transaction(&existing_hash);

                    // Update RBF tracking
                    let original_hash = {
                        let tracking = self.rbf_tracking.read().unwrap();
                        tracking
                            .get(&existing_hash)
                            .map(|t| t.original_tx_hash)
                            .unwrap_or(existing_hash)
                    };

                    let replacement_count = {
                        let tracking = self.rbf_tracking.read().unwrap();
                        tracking
                            .get(&existing_hash)
                            .map(|t| t.replacement_count + 1)
                            .unwrap_or(1)
                    };

                    {
                        let mut tracking = self.rbf_tracking.write().unwrap();
                        tracking.insert(
                            tx_hash,
                            RbfTracking {
                                replacement_count,
                                last_replacement_time: Self::current_timestamp(),
                                original_tx_hash: original_hash,
                            },
                        );
                        tracking.remove(&existing_hash);
                    }
                } else {
                    debug!("Transaction conflicts with existing mempool transaction and RBF replacement not allowed");
                    return Ok(false);
                }
            }
        } else {
            // No conflict - check if inputs are already spent
            for input in &tx.inputs {
                if self.spent_outputs.contains(&input.prevout) {
                    debug!("Transaction conflicts with existing mempool transaction");
                    return Ok(false);
                }
            }
        }

        // Check ancestor/descendant limits before adding
        if let Some(ref policy) = *self.policy_config.read().unwrap() {
            if !self.check_ancestor_descendant_limits(&tx, &tx_hash, policy)? {
                warn!(
                    "Transaction {} rejected: exceeds ancestor/descendant limits",
                    hex::encode(tx_hash)
                );
                return Ok(false);
            }
        }

        // Add transaction to mempool (store full transaction)
        self.transactions.insert(tx_hash, tx.clone());
        self.mempool.insert(tx_hash);

        // Track spent outputs
        for input in &tx.inputs {
            self.spent_outputs.insert(input.prevout.clone());
        }

        // Update dependency graph
        self.update_dependency_graph(&tx, &tx_hash);

        // Record timestamp
        self.tx_timestamps
            .write()
            .unwrap()
            .insert(tx_hash, Self::current_timestamp());

        // Calculate and cache fee rate (will be updated when UTXO set is available)
        // For now, set to 0 - will be recalculated in get_prioritized_transactions
        let fee_rate = 0u64;
        self.fee_cache.write().unwrap().insert(tx_hash, fee_rate);
        self.fee_index
            .write()
            .unwrap()
            .entry(Reverse(fee_rate))
            .or_default()
            .push(tx_hash);

        Ok(true)
    }

    /// Get mempool size
    pub fn size(&self) -> usize {
        self.transactions.len()
    }

    /// Get mempool transaction hashes
    pub fn transaction_hashes(&self) -> Vec<Hash> {
        self.transactions.keys().cloned().collect()
    }

    /// Get transaction by hash
    pub fn get_transaction(&self, hash: &Hash) -> Option<Transaction> {
        self.transactions.get(hash).cloned()
    }

    /// Get all transactions
    pub fn get_transactions(&self) -> Vec<Transaction> {
        self.transactions.values().cloned().collect()
    }

    /// Get prioritized transactions by fee rate
    ///
    /// Returns transactions sorted by fee rate (satoshis per vbyte) in descending order.
    /// Requires UTXO set to calculate fee rates.
    ///
    /// Optimization: Uses sorted index (BTreeMap) for O(log n) insertion, O(1) top-N retrieval
    /// instead of O(n log n) sort on every call.
    pub fn get_prioritized_transactions(
        &self,
        limit: usize,
        utxo_set: &UtxoSet,
    ) -> Vec<Transaction> {
        // Recalculate fee rates and update index
        // Note: In a production system, we'd track UTXO set changes and only recalculate when needed
        self.update_fee_index(utxo_set);

        // Use sorted index to get top N transactions (already sorted by fee rate descending)
        let mut result = Vec::with_capacity(limit);
        let fee_index = self.fee_index.read().unwrap();
        for (Reverse(_fee_rate), tx_hashes) in fee_index.iter() {
            for tx_hash in tx_hashes {
                if let Some(tx) = self.transactions.get(tx_hash) {
                    result.push(tx.clone());
                    if result.len() >= limit {
                        return result;
                    }
                }
            }
        }
        result
    }

    /// Update fee index with current UTXO set
    ///
    /// Recalculates fee rates for all transactions and rebuilds the sorted index.
    ///
    /// Optimization: Batch UTXO lookups across all transactions for better cache locality
    fn update_fee_index(&self, utxo_set: &UtxoSet) {
        // Clear existing index
        let mut fee_index = self.fee_index.write().unwrap();
        fee_index.clear();
        drop(fee_index);

        let mut fee_cache = self.fee_cache.write().unwrap();
        fee_cache.clear();

        // Optimization: Pre-collect all prevouts from all transactions for batch UTXO lookup
        let all_prevouts: Vec<(&Hash, &OutPoint)> = self
            .transactions
            .iter()
            .flat_map(|(tx_hash, tx)| tx.inputs.iter().map(move |input| (tx_hash, &input.prevout)))
            .collect();

        // Batch UTXO lookup for all transactions (single pass through HashMap)
        let mut utxo_cache: HashMap<&OutPoint, u64> = HashMap::with_capacity(all_prevouts.len());
        for (_, prevout) in &all_prevouts {
            if let Some(utxo) = utxo_set.get(prevout) {
                utxo_cache.insert(prevout, utxo.value as u64);
            }
        }

        // Recalculate fee rates for all transactions using cached UTXOs
        for (tx_hash, tx) in &self.transactions {
            // Calculate fee using cached UTXOs
            let mut input_total = 0u64;
            for input in &tx.inputs {
                if let Some(&value) = utxo_cache.get(&input.prevout) {
                    input_total += value;
                }
            }

            // Sum output values
            let output_total: u64 = tx.outputs.iter().map(|out| out.value as u64).sum();

            // Calculate fee
            let fee = input_total.saturating_sub(output_total);

            // Calculate transaction size
            let size = self.estimate_transaction_size(tx);

            // Calculate fee rate (satoshis per vbyte)
            let fee_rate = if size > 0 {
                fee * 1000 / size as u64
            } else {
                0
            };

            // Update cache
            fee_cache.insert(*tx_hash, fee_rate);

            // Add to sorted index
            let mut fee_index = self.fee_index.write().unwrap();
            fee_index
                .entry(Reverse(fee_rate))
                .or_default()
                .push(*tx_hash);
        }
    }

    /// Calculate transaction fee
    ///
    /// Fee = sum of inputs - sum of outputs
    ///
    /// Optimization: Uses batch UTXO lookup pattern for better cache locality
    pub fn calculate_transaction_fee(&self, tx: &Transaction, utxo_set: &UtxoSet) -> u64 {
        // Optimization: Batch UTXO lookups - collect all prevouts first, then lookup
        // This improves cache locality and reduces HashMap traversal overhead
        let prevouts: Vec<&OutPoint> = tx.inputs.iter().map(|input| &input.prevout).collect();

        // Batch UTXO lookup (single pass through HashMap)
        let mut input_total = 0u64;
        for prevout in prevouts {
            if let Some(utxo) = utxo_set.get(prevout) {
                input_total += utxo.value as u64;
            }
        }

        // Sum output values
        let output_total: u64 = tx.outputs.iter().map(|out| out.value as u64).sum();

        // Fee is difference (inputs - outputs)
        input_total.saturating_sub(output_total)
    }

    /// Estimate transaction size in vbytes
    ///
    /// Simplified estimation - in production, would use actual serialized size
    pub fn estimate_transaction_size(&self, tx: &Transaction) -> usize {
        // Base transaction size: version (4) + locktime (4) = 8 bytes
        let mut size = 8;

        // Input size: prevout (36) + script_sig (var) + sequence (4)
        for input in &tx.inputs {
            size += 36; // prevout
            size += input.script_sig.len();
            size += 4; // sequence
        }

        // Output size: value (8) + script_pubkey (var)
        for output in &tx.outputs {
            size += 8; // value
            size += output.script_pubkey.len();
        }

        // Add witness discount if segwit (simplified - assume no witness for now)
        size
    }

    /// Remove transaction from mempool
    pub fn remove_transaction(&mut self, hash: &Hash) -> bool {
        if let Some(tx) = self.transactions.remove(hash) {
            self.mempool.remove(hash);

            // Remove spent outputs tracking
            for input in &tx.inputs {
                self.spent_outputs.remove(&input.prevout);
            }

            // Remove from fee index
            if let Some(fee_rate) = self.fee_cache.write().unwrap().remove(hash) {
                let mut fee_index = self.fee_index.write().unwrap();
                if let Some(tx_hashes) = fee_index.get_mut(&Reverse(fee_rate)) {
                    tx_hashes.retain(|&h| h != *hash);
                    if tx_hashes.is_empty() {
                        fee_index.remove(&Reverse(fee_rate));
                    }
                }
            }

            // Remove RBF tracking
            self.rbf_tracking.write().unwrap().remove(hash);

            // Remove timestamp
            self.tx_timestamps.write().unwrap().remove(hash);

            // Remove from dependency graph
            {
                let mut dependencies = self.tx_dependencies.write().unwrap();
                let mut descendants = self.tx_descendants.write().unwrap();

                // Remove this transaction from all its descendants' dependency lists
                if let Some(children) = descendants.remove(hash) {
                    for child_hash in children {
                        if let Some(parents) = dependencies.get_mut(&child_hash) {
                            parents.remove(hash);
                        }
                    }
                }

                // Remove this transaction from all its ancestors' descendant lists
                if let Some(parents) = dependencies.remove(hash) {
                    for parent_hash in parents {
                        if let Some(children) = descendants.get_mut(&parent_hash) {
                            children.remove(hash);
                        }
                    }
                }
            }

            true
        } else {
            false
        }
    }

    /// Clear mempool
    pub fn clear(&mut self) {
        self.transactions.clear();
        self.mempool.clear();
        self.spent_outputs.clear();
        self.fee_index.write().unwrap().clear();
        self.fee_cache.write().unwrap().clear();
        self.rbf_tracking.write().unwrap().clear();
        self.tx_timestamps.write().unwrap().clear();
    }

    /// Save mempool to disk for persistence
    pub fn save_to_disk<P: AsRef<std::path::Path>>(&self, path: P) -> Result<()> {
        use bllvm_protocol::serialization::transaction::serialize_transaction;
        use std::fs::File;
        use std::io::Write;

        let transactions = self.get_transactions();
        let mut file = File::create(path)?;

        // Write transaction count
        file.write_all(&(transactions.len() as u32).to_le_bytes())?;

        // Write each transaction
        for tx in transactions {
            let serialized = serialize_transaction(&tx);
            file.write_all(&(serialized.len() as u32).to_le_bytes())?;
            file.write_all(&serialized)?;
        }

        file.sync_all()?;
        Ok(())
    }
}

impl Default for MempoolManager {
    fn default() -> Self {
        Self::new()
    }
}

// Implement MempoolProvider trait for integration with MiningCoordinator
impl crate::node::miner::MempoolProvider for MempoolManager {
    fn get_transactions(&self) -> Vec<bllvm_protocol::Transaction> {
        self.get_transactions()
    }

    fn get_transaction(&self, hash: &[u8; 32]) -> Option<bllvm_protocol::Transaction> {
        use bllvm_protocol::Hash;
        let hash_array: Hash = *hash;
        self.get_transaction(&hash_array)
    }

    fn get_mempool_size(&self) -> usize {
        self.size()
    }

    fn get_prioritized_transactions(
        &self,
        limit: usize,
        utxo_set: &bllvm_protocol::UtxoSet,
    ) -> Vec<bllvm_protocol::Transaction> {
        self.get_prioritized_transactions(limit, utxo_set)
    }

    fn remove_transaction(&mut self, hash: &[u8; 32]) -> bool {
        use bllvm_protocol::Hash;
        let hash_array: Hash = *hash;
        self.remove_transaction(&hash_array)
    }
}

impl MempoolManager {
    /// Load mempool from disk
    pub fn load_from_disk<P: AsRef<std::path::Path>>(&mut self, path: P) -> Result<()> {
        use bllvm_protocol::serialization::transaction::deserialize_transaction;
        use std::fs::File;
        use std::io::Read;

        let mut file = File::open(path)?;
        let mut count_bytes = [0u8; 4];
        file.read_exact(&mut count_bytes)?;
        let count = u32::from_le_bytes(count_bytes) as usize;

        for _ in 0..count {
            let mut len_bytes = [0u8; 4];
            file.read_exact(&mut len_bytes)?;
            let len = u32::from_le_bytes(len_bytes) as usize;

            let mut tx_bytes = vec![0u8; len];
            file.read_exact(&mut tx_bytes)?;

            let tx = deserialize_transaction(&tx_bytes)?;
            drop(self.add_transaction(tx));
        }

        Ok(())
    }
}
