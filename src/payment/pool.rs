//! Payment Pool Engine for Shared UTXO Management
//!
//! Implements payment pools using CTV covenants to enable multiple participants
//! to share ownership of a single UTXO:
//! - Shared spending from pool
//! - Reduced on-chain transactions
//! - Privacy for individual balances
//! - Cost-efficient batch payments

use crate::payment::covenant::{CovenantEngine, CovenantProof};
use crate::payment::processor::PaymentError;
use crate::{Hash, Transaction};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;

/// Pool configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    /// Minimum contribution to join pool
    pub min_contribution: u64,
    /// Maximum number of participants
    pub max_participants: usize,
    /// Pool fee (percentage, 0-100)
    pub pool_fee_percent: u8,
    /// Minimum balance to remain in pool
    pub min_balance: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            min_contribution: 1000, // 1000 sats minimum
            max_participants: 100,
            pool_fee_percent: 1, // 1% pool fee
            min_balance: 100,    // 100 sats minimum balance
        }
    }
}

/// Pool participant
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolParticipant {
    pub participant_id: String,
    pub balance: u64,
    pub script_pubkey: Vec<u8>,
    pub joined_at: u64,
}

/// Pool transaction type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PoolTransaction {
    /// Add new participant to pool
    Join {
        participant: PoolParticipant,
        contribution: u64,
    },
    /// Update participant balances (off-chain)
    BalanceUpdate {
        updates: Vec<(String, u64)>, // (participant_id, new_balance)
    },
    /// Distribute funds to participants (on-chain)
    Distribute {
        distribution: Vec<(String, u64)>, // (participant_id, amount)
    },
    /// Participant exits pool
    Exit { participant_id: String, amount: u64 },
}

/// Pool state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolState {
    pub pool_id: String,
    pub pool_utxo: Option<(Hash, u32)>, // (tx_hash, vout) - None if not yet created on-chain
    pub participants: Vec<PoolParticipant>,
    pub total_balance: u64,
    pub covenant_template: Option<CovenantProof>,
    pub config: PoolConfig,
    pub created_at: u64,
    pub last_updated: u64,
}

/// Payment Pool Engine
pub struct PoolEngine {
    covenant_engine: Arc<CovenantEngine>,
    /// Storage for pool states (optional)
    storage: Option<Arc<crate::storage::Storage>>,
    /// In-memory pool states cache
    pools: Arc<std::sync::Mutex<std::collections::HashMap<String, PoolState>>>,
}

impl PoolEngine {
    /// Create a new pool engine
    pub fn new(covenant_engine: Arc<CovenantEngine>) -> Self {
        Self {
            covenant_engine,
            storage: None,
            pools: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Create pool engine with storage
    pub fn with_storage(
        covenant_engine: Arc<CovenantEngine>,
        storage: Arc<crate::storage::Storage>,
    ) -> Self {
        let engine = Self {
            covenant_engine,
            storage: Some(storage),
            pools: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        };
        // Load existing pools from storage
        if let Err(e) = engine.load_all_pools() {
            tracing::warn!("Failed to load pools from storage: {}", e);
        }
        engine
    }

    /// Load all pools from storage
    fn load_all_pools(&self) -> Result<(), PaymentError> {
        let storage = self
            .storage
            .as_ref()
            .ok_or_else(|| PaymentError::ProcessingError("Storage not available".to_string()))?;

        let pools_tree = storage.open_tree("pools").map_err(|e| {
            PaymentError::ProcessingError(format!("Failed to open pools tree: {}", e))
        })?;

        let mut pools = self.pools.lock().unwrap();
        for result in pools_tree.iter() {
            let (key, value) = result.map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to read pool: {}", e))
            })?;
            let pool_id = String::from_utf8(key)
                .map_err(|e| PaymentError::ProcessingError(format!("Invalid pool ID: {}", e)))?;
            let pool_state: PoolState = bincode::deserialize(&value).map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to deserialize pool: {}", e))
            })?;
            pools.insert(pool_id, pool_state);
        }
        Ok(())
    }

    /// Save pool state to storage
    fn save_pool(&self, pool_state: &PoolState) -> Result<(), PaymentError> {
        // Update in-memory cache
        let mut pools = self.pools.lock().unwrap();
        pools.insert(pool_state.pool_id.clone(), pool_state.clone());

        // Persist to storage if available
        if let Some(storage) = &self.storage {
            let pools_tree = storage.open_tree("pools").map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to open pools tree: {}", e))
            })?;

            let key = pool_state.pool_id.as_bytes();
            let value = bincode::serialize(pool_state).map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to serialize pool: {}", e))
            })?;

            pools_tree.insert(key, &value).map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to save pool: {}", e))
            })?;
        }

        Ok(())
    }

    /// Get pool state by ID
    pub fn get_pool(&self, pool_id: &str) -> Result<Option<PoolState>, PaymentError> {
        // Check in-memory cache first
        let pools = self.pools.lock().unwrap();
        if let Some(pool) = pools.get(pool_id) {
            return Ok(Some(pool.clone()));
        }

        // If not in cache, try loading from storage
        if let Some(storage) = &self.storage {
            let pools_tree = storage.open_tree("pools").map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to open pools tree: {}", e))
            })?;

            if let Some(value) = pools_tree
                .get(pool_id.as_bytes())
                .map_err(|e| PaymentError::ProcessingError(format!("Failed to read pool: {}", e)))?
            {
                let pool_state: PoolState = bincode::deserialize(&value).map_err(|e| {
                    PaymentError::ProcessingError(format!("Failed to deserialize pool: {}", e))
                })?;
                // Update cache
                drop(pools);
                let mut pools = self.pools.lock().unwrap();
                pools.insert(pool_id.to_string(), pool_state.clone());
                return Ok(Some(pool_state));
            }
        }

        Ok(None)
    }

    /// Create a new payment pool with initial participants
    ///
    /// # Arguments
    ///
    /// * `pool_id` - Unique identifier for the pool
    /// * `initial_participants` - Initial participants with contributions
    /// * `config` - Pool configuration
    ///
    /// # Returns
    ///
    /// Pool state with covenant template
    pub fn create_pool(
        &self,
        pool_id: &str,
        initial_participants: Vec<(String, u64, Vec<u8>)>, // (id, contribution, script_pubkey)
        config: PoolConfig,
    ) -> Result<PoolState, PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "Payment pools require CTV feature".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            if initial_participants.is_empty() {
                return Err(PaymentError::ProcessingError(
                    "Pool must have at least one participant".to_string(),
                ));
            }

            if initial_participants.len() > config.max_participants {
                return Err(PaymentError::ProcessingError(format!(
                    "Too many participants: {} > {}",
                    initial_participants.len(),
                    config.max_participants
                )));
            }

            let created_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            let mut participants = Vec::new();
            let mut total_balance = 0u64;

            for (participant_id, contribution, script_pubkey) in initial_participants {
                if contribution < config.min_contribution {
                    return Err(PaymentError::ProcessingError(format!(
                        "Contribution {} below minimum {}",
                        contribution, config.min_contribution
                    )));
                }

                total_balance += contribution;

                participants.push(PoolParticipant {
                    participant_id,
                    balance: contribution,
                    script_pubkey,
                    joined_at: created_at,
                });
            }

            // Create covenant template for pool distribution
            let covenant_template = self.create_distribution_covenant(pool_id, &participants)?;

            let pool_state = PoolState {
                pool_id: pool_id.to_string(),
                pool_utxo: None,
                participants,
                total_balance,
                covenant_template: Some(covenant_template),
                config,
                created_at,
                last_updated: created_at,
            };

            // Save pool state
            self.save_pool(&pool_state)?;

            Ok(pool_state)
        }
    }

    /// Add new participant to pool
    ///
    /// # Arguments
    ///
    /// * `pool_state` - Current pool state
    /// * `participant_id` - ID of new participant
    /// * `contribution` - Contribution amount
    /// * `script_pubkey` - Participant's script pubkey
    ///
    /// # Returns
    ///
    /// Updated pool state
    pub fn join_pool(
        &self,
        pool_state: &PoolState,
        participant_id: &str,
        contribution: u64,
        script_pubkey: Vec<u8>,
    ) -> Result<PoolState, PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "Payment pools require CTV feature".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            // Check if participant already exists
            if pool_state
                .participants
                .iter()
                .any(|p| p.participant_id == participant_id)
            {
                return Err(PaymentError::ProcessingError(format!(
                    "Participant {} already in pool",
                    participant_id
                )));
            }

            // Check limits
            if pool_state.participants.len() >= pool_state.config.max_participants {
                return Err(PaymentError::ProcessingError(format!(
                    "Pool is full: {} participants",
                    pool_state.config.max_participants
                )));
            }

            if contribution < pool_state.config.min_contribution {
                return Err(PaymentError::ProcessingError(format!(
                    "Contribution {} below minimum {}",
                    contribution, pool_state.config.min_contribution
                )));
            }

            let joined_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            let mut new_participants = pool_state.participants.clone();
            new_participants.push(PoolParticipant {
                participant_id: participant_id.to_string(),
                balance: contribution,
                script_pubkey,
                joined_at,
            });

            let new_total = pool_state.total_balance + contribution;

            // Update covenant template
            let covenant_template =
                self.create_distribution_covenant(&pool_state.pool_id, &new_participants)?;

            let mut new_state = pool_state.clone();
            new_state.participants = new_participants;
            new_state.total_balance = new_total;
            new_state.covenant_template = Some(covenant_template);
            new_state.last_updated = joined_at;

            // Save pool state
            self.save_pool(&new_state)?;

            info!(
                "Participant {} joined pool {} with contribution {}",
                participant_id, pool_state.pool_id, contribution
            );

            Ok(new_state)
        }
    }

    /// Update participant balances (off-chain)
    ///
    /// Updates balances without creating on-chain transaction.
    /// Used for internal pool operations.
    ///
    /// # Arguments
    ///
    /// * `pool_state` - Current pool state
    /// * `updates` - Balance updates (participant_id, new_balance)
    ///
    /// # Returns
    ///
    /// Updated pool state
    pub fn update_balances(
        &self,
        pool_state: &PoolState,
        updates: Vec<(String, u64)>,
    ) -> Result<PoolState, PaymentError> {
        let mut new_participants = pool_state.participants.clone();
        let mut new_total = 0u64;

        for (participant_id, new_balance) in updates {
            let participant = new_participants
                .iter_mut()
                .find(|p| p.participant_id == participant_id)
                .ok_or_else(|| {
                    PaymentError::ProcessingError(format!(
                        "Participant {} not found in pool",
                        participant_id
                    ))
                })?;

            if new_balance < pool_state.config.min_balance {
                return Err(PaymentError::ProcessingError(format!(
                    "Balance {} below minimum {}",
                    new_balance, pool_state.config.min_balance
                )));
            }

            participant.balance = new_balance;
            new_total += new_balance;
        }

        // Recalculate total from all participants
        new_total = new_participants.iter().map(|p| p.balance).sum();

        let mut new_state = pool_state.clone();
        new_state.participants = new_participants;
        new_state.total_balance = new_total;
        new_state.last_updated = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        Ok(new_state)
    }

    /// Create distribution transaction (on-chain)
    ///
    /// Creates a covenant that commits to distributing funds to participants.
    ///
    /// # Arguments
    ///
    /// * `pool_state` - Current pool state
    /// * `distribution` - Distribution amounts (participant_id, amount)
    ///
    /// # Returns
    ///
    /// Updated pool state with distribution covenant
    pub fn distribute(
        &self,
        pool_state: &PoolState,
        distribution: Vec<(String, u64)>,
    ) -> Result<(PoolState, CovenantProof), PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "Payment pools require CTV feature".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            use blvm_protocol::payment::PaymentOutput;

            let mut total_distributed = 0u64;
            let mut distribution_outputs = Vec::new();
            let distribution_clone = distribution.clone();

            for (participant_id, amount) in &distribution {
                let participant = pool_state
                    .participants
                    .iter()
                    .find(|p| p.participant_id == *participant_id)
                    .ok_or_else(|| {
                        PaymentError::ProcessingError(format!(
                            "Participant {} not found in pool",
                            participant_id
                        ))
                    })?;

                // Apply pool fee
                let fee_amount = (*amount as u64 * pool_state.config.pool_fee_percent as u64) / 100;
                let net_amount = *amount - fee_amount;

                distribution_outputs.push(PaymentOutput {
                    script: participant.script_pubkey.clone(),
                    amount: Some(net_amount),
                });

                total_distributed += *amount;
            }

            if total_distributed > pool_state.total_balance {
                return Err(PaymentError::ProcessingError(format!(
                    "Distribution amount {} exceeds pool balance {}",
                    total_distributed, pool_state.total_balance
                )));
            }

            // Create distribution covenant
            let distribution_covenant = self.covenant_engine.create_payment_covenant(
                &format!("{}_distribute", pool_state.pool_id),
                &distribution_outputs,
                None,
            )?;

            // Update pool state (reduce balances)
            let mut new_state = pool_state.clone();
            for (participant_id, amount) in distribution_clone {
                if let Some(participant) = new_state
                    .participants
                    .iter_mut()
                    .find(|p| p.participant_id == participant_id)
                {
                    participant.balance = participant.balance.saturating_sub(amount);
                }
            }
            new_state.total_balance = new_state.participants.iter().map(|p| p.balance).sum();
            new_state.last_updated = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            // Save pool state
            self.save_pool(&new_state)?;

            info!(
                "Distribution created for pool {}: {} sats to {} participants",
                pool_state.pool_id,
                total_distributed,
                distribution.len()
            );

            Ok((new_state, distribution_covenant))
        }
    }

    /// Allow participant to exit pool
    ///
    /// # Arguments
    ///
    /// * `pool_state` - Current pool state
    /// * `participant_id` - ID of participant exiting
    /// * `amount` - Amount to withdraw (None for full balance)
    ///
    /// # Returns
    ///
    /// Updated pool state and exit covenant
    pub fn exit_pool(
        &self,
        pool_state: &PoolState,
        participant_id: &str,
        amount: Option<u64>,
    ) -> Result<(PoolState, CovenantProof), PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "Payment pools require CTV feature".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            let participant = pool_state
                .participants
                .iter()
                .find(|p| p.participant_id == participant_id)
                .ok_or_else(|| {
                    PaymentError::ProcessingError(format!(
                        "Participant {} not found in pool",
                        participant_id
                    ))
                })?;

            let exit_amount = amount.unwrap_or(participant.balance);

            if exit_amount > participant.balance {
                return Err(PaymentError::ProcessingError(format!(
                    "Exit amount {} exceeds balance {}",
                    exit_amount, participant.balance
                )));
            }

            if exit_amount < pool_state.config.min_balance {
                return Err(PaymentError::ProcessingError(format!(
                    "Exit amount {} below minimum balance {}",
                    exit_amount, pool_state.config.min_balance
                )));
            }

            use blvm_protocol::payment::PaymentOutput;

            // Create exit covenant
            let exit_outputs = vec![PaymentOutput {
                script: participant.script_pubkey.clone(),
                amount: Some(exit_amount),
            }];

            let exit_covenant = self.covenant_engine.create_payment_covenant(
                &format!("{}_exit_{}", pool_state.pool_id, participant_id),
                &exit_outputs,
                None,
            )?;

            // Update pool state
            let mut new_participants = pool_state.participants.clone();
            if let Some(p) = new_participants
                .iter_mut()
                .find(|p| p.participant_id == participant_id)
            {
                p.balance -= exit_amount;
            }

            // Remove participant if balance is zero
            new_participants.retain(|p| p.balance >= pool_state.config.min_balance);

            let mut new_state = pool_state.clone();
            new_state.participants = new_participants;
            new_state.total_balance = new_state.participants.iter().map(|p| p.balance).sum();
            new_state.last_updated = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            // Save pool state
            self.save_pool(&new_state)?;

            info!(
                "Participant {} exited pool {} with {} sats",
                participant_id, pool_state.pool_id, exit_amount
            );

            Ok((new_state, exit_covenant))
        }
    }

    /// Verify pool transaction matches pool covenant
    ///
    /// # Arguments
    ///
    /// * `pool_state` - Pool state
    /// * `tx` - Transaction to verify
    ///
    /// # Returns
    ///
    /// `true` if transaction matches covenant, `false` otherwise
    pub fn verify_pool_transaction(
        &self,
        pool_state: &PoolState,
        tx: &Transaction,
    ) -> Result<bool, PaymentError> {
        if let Some(ref covenant) = pool_state.covenant_template {
            self.covenant_engine
                .verify_transaction_matches_covenant(tx, covenant, 0)
        } else {
            Ok(false)
        }
    }

    /// Create distribution covenant for pool
    fn create_distribution_covenant(
        &self,
        pool_id: &str,
        participants: &[PoolParticipant],
    ) -> Result<CovenantProof, PaymentError> {
        use blvm_protocol::payment::PaymentOutput;

        let outputs: Vec<PaymentOutput> = participants
            .iter()
            .map(|p| PaymentOutput {
                script: p.script_pubkey.clone(),
                amount: Some(p.balance),
            })
            .collect();

        self.covenant_engine.create_payment_covenant(
            &format!("{}_template", pool_id),
            &outputs,
            None,
        )
    }
}

impl Default for PoolEngine {
    fn default() -> Self {
        Self::new(Arc::new(CovenantEngine::new()))
    }
}
