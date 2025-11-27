//! Vault Engine for CTV-Based Security Mechanisms
//!
//! Implements Bitcoin vaults using CTV covenants to enforce spending conditions:
//! - Time delays before withdrawal
//! - Multi-step spending (unvault → withdraw)
//! - Recovery paths for key loss scenarios
//! - Security layers against hot wallet compromises

use crate::payment::covenant::{CovenantEngine, CovenantProof};
use crate::payment::processor::PaymentError;
use crate::Hash;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;

/// Vault configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultConfig {
    /// Time delay in blocks before withdrawal allowed
    pub withdrawal_delay_blocks: u32,
    /// Recovery path script (alternative spending path)
    pub recovery_script: Option<Vec<u8>>,
    /// Maximum withdrawal amount per transaction
    pub max_withdrawal: Option<u64>,
    /// Whether to require unvaulting step
    pub require_unvault: bool,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            withdrawal_delay_blocks: 144, // ~1 day on mainnet
            recovery_script: None,
            max_withdrawal: None,
            require_unvault: true,
        }
    }
}

/// Vault lifecycle state
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VaultLifecycle {
    /// Funds deposited, waiting for unvault
    Deposited,
    /// Unvault transaction in mempool
    Unvaulting,
    /// Funds in unvault output, waiting for withdrawal delay
    Unvaulted { unvaulted_at_block: u64 },
    /// Withdrawal transaction in mempool
    Withdrawing,
    /// Funds successfully withdrawn
    Withdrawn,
    /// Funds recovered via recovery path
    Recovered,
}

/// Vault state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultState {
    pub vault_id: String,
    pub deposit_tx_hash: Hash,
    pub deposit_amount: u64,
    pub unvault_tx_hash: Option<Hash>,
    pub withdrawal_tx_hash: Option<Hash>,
    pub recovery_tx_hash: Option<Hash>,
    pub state: VaultLifecycle,
    pub config: VaultConfig,
    pub deposit_covenant: CovenantProof,
    pub unvault_covenant: Option<CovenantProof>,
    pub withdrawal_covenant: Option<CovenantProof>,
    pub created_at: u64,
    pub withdrawal_available_at: Option<u64>, // Block height
}

/// Vault Engine
pub struct VaultEngine {
    covenant_engine: Arc<CovenantEngine>,
    /// Storage for vault states (optional)
    storage: Option<Arc<crate::storage::Storage>>,
    /// In-memory vault states cache
    vaults: Arc<std::sync::Mutex<std::collections::HashMap<String, VaultState>>>,
}

impl VaultEngine {
    /// Create a new vault engine
    pub fn new(covenant_engine: Arc<CovenantEngine>) -> Self {
        Self {
            covenant_engine,
            storage: None,
            vaults: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Create vault engine with storage
    pub fn with_storage(
        covenant_engine: Arc<CovenantEngine>,
        storage: Arc<crate::storage::Storage>,
    ) -> Self {
        let engine = Self {
            covenant_engine,
            storage: Some(storage),
            vaults: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        };
        // Load existing vaults from storage
        if let Err(e) = engine.load_all_vaults() {
            tracing::warn!("Failed to load vaults from storage: {}", e);
        }
        engine
    }

    /// Load all vaults from storage
    fn load_all_vaults(&self) -> Result<(), PaymentError> {
        let storage = self
            .storage
            .as_ref()
            .ok_or_else(|| PaymentError::ProcessingError("Storage not available".to_string()))?;

        let vaults_tree = storage.open_tree("vaults").map_err(|e| {
            PaymentError::ProcessingError(format!("Failed to open vaults tree: {}", e))
        })?;

        let mut vaults = self.vaults.lock().unwrap();
        for result in vaults_tree.iter() {
            let (key, value) = result.map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to read vault: {}", e))
            })?;
            let vault_id = String::from_utf8(key)
                .map_err(|e| PaymentError::ProcessingError(format!("Invalid vault ID: {}", e)))?;
            let vault_state: VaultState = bincode::deserialize(&value).map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to deserialize vault: {}", e))
            })?;
            vaults.insert(vault_id, vault_state);
        }
        Ok(())
    }

    /// Save vault state to storage
    fn save_vault(&self, vault_state: &VaultState) -> Result<(), PaymentError> {
        // Update in-memory cache
        let mut vaults = self.vaults.lock().unwrap();
        vaults.insert(vault_state.vault_id.clone(), vault_state.clone());

        // Persist to storage if available
        if let Some(storage) = &self.storage {
            let vaults_tree = storage.open_tree("vaults").map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to open vaults tree: {}", e))
            })?;

            let key = vault_state.vault_id.as_bytes();
            let value = bincode::serialize(vault_state).map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to serialize vault: {}", e))
            })?;

            vaults_tree.insert(key, &value).map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to save vault: {}", e))
            })?;
        }

        Ok(())
    }

    /// Get vault state by ID
    pub fn get_vault(&self, vault_id: &str) -> Result<Option<VaultState>, PaymentError> {
        // Check in-memory cache first
        let vaults = self.vaults.lock().unwrap();
        if let Some(vault) = vaults.get(vault_id) {
            return Ok(Some(vault.clone()));
        }

        // If not in cache, try loading from storage
        if let Some(storage) = &self.storage {
            let vaults_tree = storage.open_tree("vaults").map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to open vaults tree: {}", e))
            })?;

            if let Some(value) = vaults_tree.get(vault_id.as_bytes()).map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to read vault: {}", e))
            })? {
                let vault_state: VaultState = bincode::deserialize(&value).map_err(|e| {
                    PaymentError::ProcessingError(format!("Failed to deserialize vault: {}", e))
                })?;
                // Update cache
                drop(vaults);
                let mut vaults = self.vaults.lock().unwrap();
                vaults.insert(vault_id.to_string(), vault_state.clone());
                return Ok(Some(vault_state));
            }
        }

        Ok(None)
    }

    /// Create a new vault with deposit covenant
    ///
    /// # Arguments
    ///
    /// * `vault_id` - Unique identifier for the vault
    /// * `deposit_amount` - Amount to deposit into vault
    /// * `withdrawal_script` - Script where funds can be withdrawn
    /// * `config` - Vault configuration
    ///
    /// # Returns
    ///
    /// Vault state with deposit covenant
    pub fn create_vault(
        &self,
        vault_id: &str,
        deposit_amount: u64,
        withdrawal_script: Vec<u8>,
        config: VaultConfig,
    ) -> Result<VaultState, PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "Vaults require CTV feature".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            use bllvm_protocol::payment::PaymentOutput;

            // Create deposit covenant that commits to unvaulting transaction
            let deposit_outputs = if config.require_unvault {
                // If unvaulting required, deposit goes to unvault script
                // For now, we'll create a placeholder - actual unvault script would be generated
                vec![PaymentOutput {
                    script: withdrawal_script.clone(), // Placeholder - would be unvault script
                    amount: Some(deposit_amount),
                }]
            } else {
                // Direct withdrawal (no unvault step)
                vec![PaymentOutput {
                    script: withdrawal_script,
                    amount: Some(deposit_amount),
                }]
            };

            let deposit_covenant =
                self.covenant_engine
                    .create_payment_covenant(vault_id, &deposit_outputs, None)?;

            let created_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            let vault_state = VaultState {
                vault_id: vault_id.to_string(),
                deposit_tx_hash: [0u8; 32], // Will be set when deposit tx is created
                deposit_amount,
                unvault_tx_hash: None,
                withdrawal_tx_hash: None,
                recovery_tx_hash: None,
                state: VaultLifecycle::Deposited,
                config,
                deposit_covenant,
                unvault_covenant: None,
                withdrawal_covenant: None,
                created_at,
                withdrawal_available_at: None,
            };

            // Save vault state
            self.save_vault(&vault_state)?;

            Ok(vault_state)
        }
    }

    /// Create unvaulting transaction (first step of withdrawal)
    ///
    /// Moves funds from vault deposit to unvault output, which can then be
    /// withdrawn after the time delay.
    ///
    /// # Arguments
    ///
    /// * `vault_state` - Current vault state
    /// * `unvault_script` - Script for unvault output
    ///
    /// # Returns
    ///
    /// Updated vault state with unvault covenant
    pub fn unvault(
        &self,
        vault_state: &VaultState,
        unvault_script: Vec<u8>,
    ) -> Result<VaultState, PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "Vaults require CTV feature".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            if !vault_state.config.require_unvault {
                return Err(PaymentError::ProcessingError(
                    "Vault does not require unvaulting step".to_string(),
                ));
            }

            if vault_state.state != VaultLifecycle::Deposited {
                return Err(PaymentError::ProcessingError(format!(
                    "Vault must be in Deposited state, current: {:?}",
                    vault_state.state
                )));
            }

            use bllvm_protocol::payment::PaymentOutput;

            // Create unvault covenant that commits to withdrawal transaction
            let unvault_outputs = vec![PaymentOutput {
                script: unvault_script,
                amount: Some(vault_state.deposit_amount),
            }];

            let unvault_covenant = self.covenant_engine.create_payment_covenant(
                &format!("{}_unvault", vault_state.vault_id),
                &unvault_outputs,
                None,
            )?;

            let mut new_state = vault_state.clone();
            new_state.unvault_covenant = Some(unvault_covenant);
            new_state.state = VaultLifecycle::Unvaulting;

            // Save vault state
            self.save_vault(&new_state)?;

            info!("Unvaulting created for vault: {}", vault_state.vault_id);

            Ok(new_state)
        }
    }

    /// Create withdrawal transaction (final step)
    ///
    /// Withdraws funds from unvault output to final destination.
    /// Requires that the withdrawal delay has passed.
    ///
    /// # Arguments
    ///
    /// * `vault_state` - Current vault state
    /// * `withdrawal_script` - Final destination script
    /// * `current_block_height` - Current blockchain height
    ///
    /// # Returns
    ///
    /// Updated vault state with withdrawal covenant
    pub fn withdraw(
        &self,
        vault_state: &VaultState,
        withdrawal_script: Vec<u8>,
        current_block_height: u64,
    ) -> Result<VaultState, PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "Vaults require CTV feature".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            // Check if withdrawal is allowed
            if let Some(available_at) = vault_state.withdrawal_available_at {
                if current_block_height < available_at {
                    return Err(PaymentError::ProcessingError(format!(
                        "Withdrawal not yet available. Available at block {}, current: {}",
                        available_at, current_block_height
                    )));
                }
            }

            // Check state
            let state_valid = if vault_state.config.require_unvault {
                matches!(vault_state.state, VaultLifecycle::Unvaulted { .. })
            } else {
                matches!(vault_state.state, VaultLifecycle::Deposited)
            };

            if !state_valid {
                return Err(PaymentError::ProcessingError(format!(
                    "Vault state invalid for withdrawal. Current: {:?}",
                    vault_state.state
                )));
            }

            // Check max withdrawal if configured
            if let Some(max) = vault_state.config.max_withdrawal {
                if vault_state.deposit_amount > max {
                    return Err(PaymentError::ProcessingError(format!(
                        "Withdrawal amount {} exceeds maximum {}",
                        vault_state.deposit_amount, max
                    )));
                }
            }

            use bllvm_protocol::payment::PaymentOutput;

            // Create withdrawal covenant
            let withdrawal_outputs = vec![PaymentOutput {
                script: withdrawal_script,
                amount: Some(vault_state.deposit_amount),
            }];

            let withdrawal_covenant = self.covenant_engine.create_payment_covenant(
                &format!("{}_withdrawal", vault_state.vault_id),
                &withdrawal_outputs,
                None,
            )?;

            let mut new_state = vault_state.clone();
            new_state.withdrawal_covenant = Some(withdrawal_covenant);
            new_state.state = VaultLifecycle::Withdrawing;

            // Save vault state
            self.save_vault(&new_state)?;

            info!("Withdrawal created for vault: {}", vault_state.vault_id);

            Ok(new_state)
        }
    }

    /// Use recovery path to access funds
    ///
    /// Allows recovery of funds if primary keys are lost, using the
    /// recovery script specified in vault configuration.
    ///
    /// # Arguments
    ///
    /// * `vault_state` - Current vault state
    /// * `recovery_script` - Recovery destination script
    ///
    /// # Returns
    ///
    /// Updated vault state with recovery transaction
    pub fn recover(
        &self,
        vault_state: &VaultState,
        recovery_script: Vec<u8>,
    ) -> Result<VaultState, PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "Vaults require CTV feature".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            if vault_state.config.recovery_script.is_none() {
                return Err(PaymentError::ProcessingError(
                    "Vault does not have recovery path configured".to_string(),
                ));
            }

            // Recovery can be used from any state except already recovered/withdrawn
            match vault_state.state {
                VaultLifecycle::Withdrawn | VaultLifecycle::Recovered => {
                    return Err(PaymentError::ProcessingError(
                        "Vault already withdrawn or recovered".to_string(),
                    ));
                }
                _ => {}
            }

            use bllvm_protocol::payment::PaymentOutput;

            // Create recovery covenant
            let recovery_outputs = vec![PaymentOutput {
                script: recovery_script,
                amount: Some(vault_state.deposit_amount),
            }];

            let _recovery_covenant = self.covenant_engine.create_payment_covenant(
                &format!("{}_recovery", vault_state.vault_id),
                &recovery_outputs,
                None,
            )?;

            let mut new_state = vault_state.clone();
            new_state.state = VaultLifecycle::Recovered;

            // Save vault state
            self.save_vault(&new_state)?;

            info!("Recovery initiated for vault: {}", vault_state.vault_id);

            Ok(new_state)
        }
    }

    /// Check if withdrawal is eligible based on block height
    ///
    /// # Arguments
    ///
    /// * `vault_state` - Current vault state
    /// * `current_block_height` - Current blockchain height
    ///
    /// # Returns
    ///
    /// `true` if withdrawal is allowed, `false` otherwise
    pub fn check_withdrawal_eligibility(
        vault_state: &VaultState,
        current_block_height: u64,
    ) -> bool {
        if let Some(available_at) = vault_state.withdrawal_available_at {
            current_block_height >= available_at
        } else {
            // If no delay configured, always eligible (for non-unvault vaults)
            !vault_state.config.require_unvault
        }
    }

    /// Update vault state after unvault transaction is confirmed
    ///
    /// Sets the withdrawal_available_at block height based on delay.
    ///
    /// # Arguments
    ///
    /// * `vault_state` - Current vault state
    /// * `unvault_tx_hash` - Hash of confirmed unvault transaction
    /// * `unvault_block_height` - Block height where unvault was confirmed
    ///
    /// # Returns
    ///
    /// Updated vault state
    pub fn mark_unvaulted(
        &self,
        vault_state: &VaultState,
        unvault_tx_hash: Hash,
        unvault_block_height: u64,
    ) -> Result<VaultState, PaymentError> {
        let mut new_state = vault_state.clone();
        new_state.unvault_tx_hash = Some(unvault_tx_hash);
        new_state.withdrawal_available_at =
            Some(unvault_block_height + vault_state.config.withdrawal_delay_blocks as u64);
        new_state.state = VaultLifecycle::Unvaulted {
            unvaulted_at_block: unvault_block_height,
        };

        // Save vault state
        self.save_vault(&new_state)?;

        info!(
            "Vault {} unvaulted at block {}, withdrawal available at block {}",
            vault_state.vault_id,
            unvault_block_height,
            new_state.withdrawal_available_at.unwrap()
        );

        Ok(new_state)
    }
}

impl Default for VaultEngine {
    fn default() -> Self {
        Self::new(Arc::new(CovenantEngine::new()))
    }
}
