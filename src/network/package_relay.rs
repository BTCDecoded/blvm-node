//! BIP 331: Package Relay
//!
//! Specification: https://github.com/bitcoin/bips/blob/master/bip-0331.mediawiki
//!
//! Package relay allows nodes to relay and validate groups of transactions together.
//! This is particularly useful for:
//! - Fee-bumping (RBF) transactions (parent + child)
//! - CPFP (Child Pays For Parent) scenarios
//! - Atomic transaction sets
//!
//! Benefits:
//! - Better fee rate calculation for package
//! - Reduces orphan transactions in mempool
//! - More efficient validation (package as unit)

use crate::network::txhash::calculate_txid;
use bllvm_protocol::{Hash, Transaction};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Package relay manager
pub struct PackageRelay {
    /// Pending package requests
    pending_packages: HashMap<PackageId, PackageState>,
    /// Package validator
    validator: PackageValidator,
}

/// Package ID (combined hash of all transactions)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackageId(pub Hash);

/// Transaction package
#[derive(Debug, Clone)]
pub struct TransactionPackage {
    /// Transactions in package (ordered: parents first)
    pub transactions: Vec<Transaction>,
    /// Package ID
    pub package_id: PackageId,
    /// Combined fee (sum of all transaction fees)
    pub combined_fee: u64,
    /// Combined weight (for fee rate calculation)
    pub combined_weight: usize,
}

/// Package state
#[derive(Debug, Clone)]
struct PackageState {
    /// Package data
    package: TransactionPackage,
    /// When package was received
    received_at: u64,
    /// Package status
    status: PackageStatus,
}

/// Package status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageStatus {
    /// Pending validation
    Pending,
    /// Validated and accepted
    Accepted,
    /// Rejected (validation failed)
    Rejected { reason: PackageRejectReason },
}

/// Package rejection reason
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageRejectReason {
    /// Package too large (transaction count)
    TooManyTransactions,
    /// Package weight exceeds limit
    WeightExceedsLimit,
    /// Invalid fee rate (below minimum)
    FeeRateTooLow,
    /// Transactions not properly ordered (parent before child)
    InvalidOrder,
    /// Duplicate transactions
    DuplicateTransactions,
    /// Invalid package structure
    InvalidStructure,
}

/// Package validator
#[derive(Debug, Clone)]
pub struct PackageValidator {
    /// Maximum transactions in package (BIP 331: 25)
    pub max_package_size: usize,
    /// Maximum package weight in WU (BIP 331: 404000)
    pub max_package_weight: usize,
    /// Minimum package fee rate (sat/vB)
    pub min_fee_rate: u64,
}

impl Default for PackageValidator {
    fn default() -> Self {
        Self {
            max_package_size: 25,
            max_package_weight: 404_000, // 404k WU = ~101k vB
            min_fee_rate: 1000,          // 1 sat/vB minimum
        }
    }
}

impl PackageId {
    /// Calculate package ID from transactions
    pub fn from_transactions(transactions: &[Transaction]) -> Self {
        let mut hasher = Sha256::new();

        // Hash all transactions in order (placeholder: serialize structure)
        // Full implementation should hash txids
        for tx in transactions {
            hasher.update(tx.version.to_le_bytes());
            hasher.update((tx.inputs.len() as u64).to_le_bytes());
            hasher.update((tx.outputs.len() as u64).to_le_bytes());
            hasher.update(tx.lock_time.to_le_bytes());
        }

        let hash_bytes = hasher.finalize();
        let mut package_hash = [0u8; 32];
        package_hash.copy_from_slice(&hash_bytes);

        // Double hash for package ID
        let mut hasher2 = Sha256::new();
        hasher2.update(package_hash);
        let final_hash = hasher2.finalize();
        let mut final_package_hash = [0u8; 32];
        final_package_hash.copy_from_slice(&final_hash);

        PackageId(final_package_hash)
    }
}

impl TransactionPackage {
    /// Create a new transaction package
    pub fn new(transactions: Vec<Transaction>) -> Result<Self, PackageError> {
        Self::new_with_utxo_set(transactions, None)
    }

    /// Create a new transaction package with UTXO set for fee calculation
    pub fn new_with_utxo_set(
        transactions: Vec<Transaction>,
        utxo_set: Option<&bllvm_protocol::UtxoSet>,
    ) -> Result<Self, PackageError> {
        if transactions.is_empty() {
            return Err(PackageError::EmptyPackage);
        }

        // Validate ordering (parents before children)
        Self::validate_ordering(&transactions)?;

        // Calculate package ID from txids
        let mut hasher = sha2::Sha256::new();
        for tx in &transactions {
            let txid = calculate_txid(tx);
            hasher.update(txid);
        }
        let first = hasher.finalize();
        let mut hasher2 = sha2::Sha256::new();
        hasher2.update(first);
        let final_bytes = hasher2.finalize();
        let mut pkg_hash = [0u8; 32];
        pkg_hash.copy_from_slice(&final_bytes);
        let package_id = PackageId(pkg_hash);

        // Calculate combined fee from UTXO set if provided
        let combined_fee = if let Some(utxo_set) = utxo_set {
            // Calculate fee for each transaction: sum(inputs) - sum(outputs)
            transactions
                .iter()
                .map(|tx| {
                    let input_total: u64 = tx
                        .inputs
                        .iter()
                        .filter_map(|inp| utxo_set.get(&inp.prevout))
                        .map(|utxo| utxo.value as u64)
                        .sum();
                    let output_total: u64 = tx.outputs.iter().map(|out| out.value as u64).sum();
                    input_total.saturating_sub(output_total)
                })
                .sum()
        } else {
            0 // Fee calculation requires UTXO set
        };

        // Calculate combined weight
        let combined_weight: usize = transactions
            .iter()
            .map(|tx| {
                // Simplified weight calculation
                // In real implementation, would use proper witness weight
                tx.inputs.len() * 68 + tx.outputs.len() * 31 + 10
            })
            .sum();

        Ok(Self {
            transactions,
            package_id,
            combined_fee,
            combined_weight,
        })
    }

    /// Validate transaction ordering (parents before children)
    fn validate_ordering(transactions: &[Transaction]) -> Result<(), PackageError> {
        // Build index of txids to position
        let mut idx = std::collections::HashMap::new();
        for (i, tx) in transactions.iter().enumerate() {
            idx.insert(calculate_txid(tx), i);
        }

        // Check each transaction: inputs that reference in-package parents must be earlier
        for (i, tx) in transactions.iter().enumerate() {
            for input in &tx.inputs {
                if let Some(&parent_pos) = idx.get(&input.prevout.hash) {
                    if parent_pos >= i {
                        return Err(PackageError::InvalidOrder);
                    }
                }
            }
        }

        Ok(())
    }

    /// Calculate package fee rate (sat/vB)
    pub fn fee_rate(&self) -> f64 {
        if self.combined_weight == 0 {
            return 0.0;
        }

        // Convert weight to virtual bytes (weight / 4)
        let vbytes = self.combined_weight as f64 / 4.0;

        if vbytes == 0.0 {
            return 0.0;
        }

        self.combined_fee as f64 / vbytes
    }
}

impl Default for PackageRelay {
    fn default() -> Self {
        Self::new()
    }
}

impl PackageRelay {
    /// Create a new package relay manager
    pub fn new() -> Self {
        Self {
            pending_packages: HashMap::new(),
            validator: PackageValidator::default(),
        }
    }

    /// Create package from transactions
    pub fn create_package(
        &self,
        transactions: Vec<Transaction>,
    ) -> Result<TransactionPackage, PackageError> {
        TransactionPackage::new(transactions)
    }

    /// Validate package against limits
    pub fn validate_package(
        &self,
        package: &TransactionPackage,
    ) -> Result<(), PackageRejectReason> {
        // Check package size
        if package.transactions.len() > self.validator.max_package_size {
            return Err(PackageRejectReason::TooManyTransactions);
        }

        // Check package weight
        if package.combined_weight > self.validator.max_package_weight {
            return Err(PackageRejectReason::WeightExceedsLimit);
        }

        // Check fee rate (if fee calculated)
        if package.combined_fee > 0 {
            let fee_rate = package.fee_rate();
            if fee_rate < self.validator.min_fee_rate as f64 {
                return Err(PackageRejectReason::FeeRateTooLow);
            }
        }

        // Check for duplicates by txid
        let mut seen = std::collections::HashSet::new();
        for tx in &package.transactions {
            let txid = calculate_txid(tx);
            if !seen.insert(txid) {
                return Err(PackageRejectReason::DuplicateTransactions);
            }
        }

        // Validate ordering
        TransactionPackage::validate_ordering(&package.transactions)
            .map_err(|_| PackageRejectReason::InvalidOrder)?;

        Ok(())
    }

    /// Register package for relay
    pub fn register_package(
        &mut self,
        package: TransactionPackage,
    ) -> Result<PackageId, PackageError> {
        // Validate package
        self.validate_package(&package)
            .map_err(PackageError::ValidationFailed)?;

        let package_id = package.package_id;
        let tx_count = package.transactions.len();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let state = PackageState {
            package,
            received_at: now,
            status: PackageStatus::Pending,
        };

        self.pending_packages.insert(package_id, state);
        debug!(
            "Registered package {} with {} transactions",
            hex::encode(package_id.0),
            tx_count
        );

        Ok(package_id)
    }

    /// Get package by ID
    pub fn get_package(&self, package_id: &PackageId) -> Option<&TransactionPackage> {
        self.pending_packages.get(package_id).map(|s| &s.package)
    }

    /// Mark package as accepted
    pub fn mark_accepted(&mut self, package_id: &PackageId) {
        if let Some(state) = self.pending_packages.get_mut(package_id) {
            state.status = PackageStatus::Accepted;
            info!("Package {} accepted", hex::encode(package_id.0));
        }
    }

    /// Mark package as rejected
    pub fn mark_rejected(&mut self, package_id: &PackageId, reason: PackageRejectReason) {
        if let Some(state) = self.pending_packages.get_mut(package_id) {
            state.status = PackageStatus::Rejected { reason };
            warn!(
                "Package {} rejected: {:?}",
                hex::encode(package_id.0),
                reason
            );
        }
    }

    /// Clean up old packages
    pub fn cleanup_old_packages(&mut self, max_age: u64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let expired: Vec<PackageId> = self
            .pending_packages
            .iter()
            .filter(|(_, state)| now - state.received_at > max_age)
            .map(|(id, _)| *id)
            .collect();

        for id in expired {
            self.pending_packages.remove(&id);
            debug!("Cleaned up expired package {}", hex::encode(id.0));
        }
    }
}

/// Package error
#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error("Empty package (no transactions)")]
    EmptyPackage,

    #[error("Invalid transaction ordering (children before parents)")]
    InvalidOrder,

    #[error("Package validation failed: {0:?}")]
    ValidationFailed(PackageRejectReason),

    #[error("Package not found")]
    PackageNotFound,
}
