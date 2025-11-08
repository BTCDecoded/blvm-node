//! UTXO Commitment Storage
//!
//! Stores UTXO commitments for pruned blocks, enabling state verification
//! without requiring full block history.

#[cfg(feature = "utxo-commitments")]
use anyhow::Result;
#[cfg(feature = "utxo-commitments")]
use bllvm_consensus::utxo_commitments::data_structures::UtxoCommitment;
#[cfg(feature = "utxo-commitments")]
use bllvm_protocol::Hash;
#[cfg(feature = "utxo-commitments")]
use crate::storage::database::{Database, Tree};
#[cfg(feature = "utxo-commitments")]
use std::sync::Arc;

/// UTXO Commitment storage manager
#[cfg(feature = "utxo-commitments")]
pub struct CommitmentStore {
    #[allow(dead_code)]
    db: Arc<dyn Database>,
    commitments: Arc<dyn Tree>,
    height_index: Arc<dyn Tree>, // Maps height -> block_hash for quick lookup
}

#[cfg(feature = "utxo-commitments")]
impl CommitmentStore {
    /// Create a new commitment store
    pub fn new(db: Arc<dyn Database>) -> Result<Self> {
        let commitments = Arc::from(db.open_tree("utxo_commitments")?);
        let height_index = Arc::from(db.open_tree("commitment_height_index")?);

        Ok(Self {
            db,
            commitments,
            height_index,
        })
    }

    /// Store a UTXO commitment for a block
    pub fn store_commitment(
        &self,
        block_hash: &Hash,
        height: u64,
        commitment: &UtxoCommitment,
    ) -> Result<()> {
        // Serialize commitment
        let commitment_data = bincode::serialize(commitment)?;

        // Store by block hash
        self.commitments.insert(block_hash.as_slice(), commitment_data)?;

        // Store height index for quick lookup
        let height_bytes = height.to_be_bytes();
        self.height_index.insert(height_bytes, block_hash.as_slice())?;

        Ok(())
    }

    /// Get UTXO commitment for a block hash
    pub fn get_commitment(&self, block_hash: &Hash) -> Result<Option<UtxoCommitment>> {
        if let Some(data) = self.commitments.get(block_hash.as_slice())? {
            let commitment: UtxoCommitment = bincode::deserialize(&data)?;
            Ok(Some(commitment))
        } else {
            Ok(None)
        }
    }

    /// Get UTXO commitment for a block height
    pub fn get_commitment_by_height(&self, height: u64) -> Result<Option<UtxoCommitment>> {
        // Look up block hash from height index
        let height_bytes = height.to_be_bytes();
        if let Some(hash_data) = self.height_index.get(height_bytes)? {
            let mut block_hash = [0u8; 32];
            block_hash.copy_from_slice(&hash_data);
            self.get_commitment(&block_hash)
        } else {
            Ok(None)
        }
    }

    /// Check if a commitment exists for a block
    pub fn has_commitment(&self, block_hash: &Hash) -> Result<bool> {
        Ok(self.commitments.contains_key(block_hash.as_slice())?)
    }

    /// Remove commitment for a block (cleanup)
    pub fn remove_commitment(&self, block_hash: &Hash, height: u64) -> Result<()> {
        self.commitments.remove(block_hash.as_slice())?;
        let height_bytes = height.to_be_bytes();
        self.height_index.remove(height_bytes)?;
        Ok(())
    }

    /// Get all commitments in a height range
    pub fn get_commitments_by_height_range(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, Hash, UtxoCommitment)>> {
        let mut results = Vec::new();

        for height in start..=end {
            if let Some(commitment) = self.get_commitment_by_height(height)? {
                // Get block hash from commitment
                let block_hash = commitment.block_hash;
                results.push((height, block_hash, commitment));
            }
        }

        Ok(results)
    }

    /// Get count of stored commitments
    pub fn commitment_count(&self) -> Result<usize> {
        Ok(self.commitments.len()?)
    }
}

// Placeholder implementation when utxo-commitments feature is disabled
#[cfg(not(feature = "utxo-commitments"))]
pub struct CommitmentStore;

#[cfg(not(feature = "utxo-commitments"))]
impl CommitmentStore {
    pub fn new(_db: Arc<dyn Database>) -> Result<Self> {
        Err(anyhow::anyhow!("UTXO commitments feature not enabled"))
    }
}

