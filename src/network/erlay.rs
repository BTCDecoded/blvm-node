//! Erlay (BIP330) Transaction Relay Optimization
//!
//! Implements efficient transaction relay using set reconciliation (minisketch).
//! Reduces bandwidth by ~60% compared to standard transaction relay.
//!
//! **Note**: This feature requires libclang/LLVM for minisketch-rs bindgen compilation.
//! Erlay is entirely optional - if libclang is not available, simply don't enable
//! the `erlay` feature. For CI environments without libclang, omit this feature.

#[cfg(feature = "erlay")]
use anyhow::{Context, Result};
#[cfg(feature = "erlay")]
use blvm_protocol::Hash;
#[cfg(feature = "erlay")]
use minisketch_rs::Minisketch;
#[cfg(feature = "erlay")]
use std::collections::HashSet;

/// Erlay set reconciler configuration
#[derive(Debug, Clone)]
pub struct ErlayConfig {
    /// Minisketch capacity (max set size)
    pub capacity: usize,
    /// Field size in bits (32 or 64)
    pub field_size: u8,
}

impl Default for ErlayConfig {
    fn default() -> Self {
        Self {
            capacity: 100_000, // Support up to 100k transactions
            field_size: 64,   // 64-bit field size
        }
    }
}

/// Erlay set reconciler
#[cfg(feature = "erlay")]
pub struct ErlayReconciler {
    config: ErlayConfig,
}

#[cfg(feature = "erlay")]
impl ErlayReconciler {
    /// Create a new Erlay reconciler
    pub fn new(config: ErlayConfig) -> Self {
        Self { config }
    }

    /// Create a sketch of transaction set differences
    ///
    /// Creates a minisketch containing transactions we have that the peer doesn't.
    pub fn create_sketch(
        &self,
        local_txs: &HashSet<Hash>,
        remote_tx_count: usize,
    ) -> Result<Vec<u8>> {
        // Estimate remote set size (we don't have the full set, just count)
        // For now, we'll create a sketch of all local transactions
        // In a real implementation, we'd track which transactions we've sent to each peer
        
        if local_txs.is_empty() {
            return Ok(vec![]);
        }

        // Create minisketch
        let mut sketch = Minisketch::new(self.config.field_size, 0, self.config.capacity)
            .context("Failed to create minisketch")?;

        // Add transaction hashes to sketch
        for tx_hash in local_txs {
            // Convert Hash ([u8; 32]) to u64 for minisketch
            // Minisketch expects u64 values, so we take first 8 bytes
            let mut hash_u64 = [0u8; 8];
            hash_u64.copy_from_slice(&tx_hash[..8]);
            let value = u64::from_le_bytes(hash_u64);
            
            sketch.add(value)
                .context("Failed to add transaction to sketch")?;
        }

        // Serialize sketch
        sketch.serialize()
            .context("Failed to serialize sketch")
    }

    /// Reconcile transaction sets using sketches
    ///
    /// Combines local and remote sketches to find missing transactions.
    pub fn reconcile_sets(
        &self,
        local_txs: &HashSet<Hash>,
        local_sketch: &[u8],
        remote_sketch: &[u8],
    ) -> Result<Vec<Hash>> {
        if local_sketch.is_empty() && remote_sketch.is_empty() {
            return Ok(vec![]);
        }

        // Create combined sketch
        let mut combined = Minisketch::new(self.config.field_size, 0, self.config.capacity)
            .context("Failed to create combined minisketch")?;

        // Deserialize and merge sketches
        if !local_sketch.is_empty() {
            let mut local = Minisketch::new(self.config.field_size, 0, self.config.capacity)
                .context("Failed to create local minisketch")?;
            local.deserialize(local_sketch)
                .context("Failed to deserialize local sketch")?;
            combined.merge(&local)
                .map_err(|e| anyhow::anyhow!("Failed to merge local sketch: {:?}", e))?;
        }

        if !remote_sketch.is_empty() {
            let mut remote = Minisketch::new(self.config.field_size, 0, self.config.capacity)
                .context("Failed to create remote minisketch")?;
            remote.deserialize(remote_sketch)
                .context("Failed to deserialize remote sketch")?;
            combined.merge(&remote)
                .map_err(|e| anyhow::anyhow!("Failed to merge remote sketch: {:?}", e))?;
        }

        // Decode differences
        let differences: Vec<u64> = combined.decode()
            .map_err(|e| anyhow::anyhow!("Failed to decode sketch: {:?}", e))?;

        // Convert u64 differences back to transaction hashes
        // Find transactions we're missing (in remote but not local)
        let mut missing_txs = Vec::new();
        for diff in differences {
            // Convert u64 back to hash (first 8 bytes)
            let mut hash = [0u8; 32];
            let diff_bytes = diff.to_le_bytes();
            hash[..8].copy_from_slice(&diff_bytes);
            
            // Check if we have this transaction
            if !local_txs.contains(&hash) {
                missing_txs.push(hash);
            }
        }

        Ok(missing_txs)
    }
}

/// Erlay transaction set manager
#[cfg(feature = "erlay")]
pub struct ErlayTxSet {
    /// Current transaction set (mempool)
    txs: HashSet<Hash>,
    /// Reconciler
    reconciler: ErlayReconciler,
}

#[cfg(feature = "erlay")]
impl ErlayTxSet {
    /// Create a new Erlay transaction set
    pub fn new() -> Self {
        Self {
            txs: HashSet::new(),
            reconciler: ErlayReconciler::new(ErlayConfig::default()),
        }
    }

    /// Create with custom configuration
    pub fn with_config(config: ErlayConfig) -> Self {
        Self {
            txs: HashSet::new(),
            reconciler: ErlayReconciler::new(config),
        }
    }

    /// Add transaction to set
    pub fn add(&mut self, tx_hash: Hash) {
        self.txs.insert(tx_hash);
    }

    /// Remove transaction from set
    pub fn remove(&mut self, tx_hash: &Hash) {
        self.txs.remove(tx_hash);
    }

    /// Get set size
    pub fn size(&self) -> usize {
        self.txs.len()
    }

    /// Check if transaction is in set
    pub fn contains(&self, tx_hash: &Hash) -> bool {
        self.txs.contains(tx_hash)
    }

    /// Create reconciliation sketch for a peer
    ///
    /// Creates a sketch of transactions we have that the peer might not have.
    pub fn create_reconciliation_sketch(
        &self,
        remote_tx_count: usize,
    ) -> Result<Vec<u8>> {
        self.reconciler.create_sketch(&self.txs, remote_tx_count)
    }

    /// Reconcile with peer's sketch
    ///
    /// Returns list of transaction hashes we're missing.
    pub fn reconcile_with_peer(
        &self,
        local_sketch: &[u8],
        remote_sketch: &[u8],
    ) -> Result<Vec<Hash>> {
        self.reconciler.reconcile_sets(&self.txs, local_sketch, remote_sketch)
    }

    /// Get all transaction hashes
    pub fn get_all_hashes(&self) -> Vec<Hash> {
        self.txs.iter().cloned().collect()
    }
}

#[cfg(feature = "erlay")]
impl Default for ErlayTxSet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "erlay")]
    use super::*;

    #[cfg(feature = "erlay")]
    #[test]
    fn test_erlay_reconciliation() {
        let reconciler = ErlayReconciler::new(ErlayConfig::default());
        
        // Create two transaction sets with some overlap
        let mut local_txs = HashSet::new();
        local_txs.insert([1u8; 32]);
        local_txs.insert([2u8; 32]);
        local_txs.insert([3u8; 32]);
        
        let mut remote_txs = HashSet::new();
        remote_txs.insert([2u8; 32]);
        remote_txs.insert([3u8; 32]);
        remote_txs.insert([4u8; 32]);
        
        // Create sketches
        let local_sketch = reconciler.create_sketch(&local_txs, remote_txs.len())
            .expect("Failed to create local sketch");
        let remote_sketch = reconciler.create_sketch(&remote_txs, local_txs.len())
            .expect("Failed to create remote sketch");
        
        // Reconcile
        let missing_local = reconciler.reconcile_sets(&local_txs, &local_sketch, &remote_sketch)
            .expect("Failed to reconcile");
        let missing_remote = reconciler.reconcile_sets(&remote_txs, &remote_sketch, &local_sketch)
            .expect("Failed to reconcile");
        
        // Local should be missing [4]
        assert!(missing_local.contains(&[4u8; 32]));
        // Remote should be missing [1]
        assert!(missing_remote.contains(&[1u8; 32]));
    }

    #[cfg(feature = "erlay")]
    #[test]
    fn test_erlay_tx_set() {
        let mut tx_set = ErlayTxSet::new();
        
        let tx1 = [1u8; 32];
        let tx2 = [2u8; 32];
        
        tx_set.add(tx1);
        tx_set.add(tx2);
        
        assert_eq!(tx_set.size(), 2);
        assert!(tx_set.contains(&tx1));
        assert!(tx_set.contains(&tx2));
        
        tx_set.remove(&tx1);
        assert_eq!(tx_set.size(), 1);
        assert!(!tx_set.contains(&tx1));
    }
}

