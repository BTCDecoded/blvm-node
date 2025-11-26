//! Block Filter Service for BIP157/158
//!
//! Generates, caches, and serves compact block filters for light client support.
//! Maintains filter header chain for efficient verification.

use anyhow::{anyhow, Result};
use bllvm_protocol::bip157;
use bllvm_protocol::bip158::{build_block_filter, CompactBlockFilter};
use bllvm_protocol::{Block, BlockHeader, Hash};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Block filter service for generating and serving BIP158 filters
#[derive(Debug, Clone)]
pub struct BlockFilterService {
    /// Cache of filters by block hash
    filters: Arc<RwLock<HashMap<Hash, CompactBlockFilter>>>,
    /// Filter header chain (indexed by height)
    filter_headers: Arc<RwLock<Vec<bip157::FilterHeader>>>,
    /// Block hash to height mapping (for lookups)
    block_hash_to_height: Arc<RwLock<HashMap<Hash, u32>>>,
    /// Current chain height
    current_height: Arc<RwLock<u32>>,
}

impl BlockFilterService {
    /// Create a new block filter service
    pub fn new() -> Self {
        BlockFilterService {
            filters: Arc::new(RwLock::new(HashMap::new())),
            filter_headers: Arc::new(RwLock::new(Vec::new())),
            block_hash_to_height: Arc::new(RwLock::new(HashMap::new())),
            current_height: Arc::new(RwLock::new(0)),
        }
    }

    /// Get filter for a block hash
    pub fn get_filter(&self, block_hash: &Hash) -> Option<CompactBlockFilter> {
        self.filters.read().unwrap().get(block_hash).cloned()
    }

    /// Generate and cache filter for a block
    ///
    /// # Arguments
    /// * `block` - The block to generate a filter for
    /// * `previous_outpoint_scripts` - Scripts from UTXOs being spent by this block
    /// * `height` - Block height
    pub fn generate_and_cache_filter(
        &self,
        block: &Block,
        previous_outpoint_scripts: &[Vec<u8>],
        height: u32,
    ) -> Result<CompactBlockFilter> {
        // Generate filter
        let filter = build_block_filter(&block.transactions, previous_outpoint_scripts)
            .map_err(|e| anyhow!("Failed to build filter: {}", e))?;

        // Calculate block hash (simplified - in production would use proper block hash calculation)
        let block_hash = self.calculate_block_hash(&block.header);

        // Cache filter
        self.filters
            .write()
            .unwrap()
            .insert(block_hash, filter.clone());
        self.block_hash_to_height
            .write()
            .unwrap()
            .insert(block_hash, height);

        // Update filter header chain
        // Get previous header before acquiring write lock to avoid deadlock
        let prev_header = if height > 0 {
            self.filter_headers
                .read()
                .unwrap()
                .get((height - 1) as usize)
                .cloned()
        } else {
            None
        };

        let filter_header = bip157::FilterHeader::new(&filter, prev_header.as_ref());

        // Extend filter header chain if needed
        // Release read lock before acquiring write lock to avoid deadlock
        let mut headers = self.filter_headers.write().unwrap();
        let current_len = headers.len() as u32;
        if height >= current_len {
            headers.resize((height + 1) as usize, filter_header.clone());
        } else {
            headers[height as usize] = filter_header.clone();
        }

        // Update current height (avoid holding read and write locks simultaneously)
        let current = *self.current_height.read().unwrap();
        *self.current_height.write().unwrap() = height.max(current);

        Ok(filter)
    }

    /// Get filter header at a specific height
    pub fn get_filter_header(&self, height: u32) -> Option<bip157::FilterHeader> {
        self.filter_headers
            .read()
            .unwrap()
            .get(height as usize)
            .cloned()
    }

    /// Get filter headers in a range
    ///
    /// # Arguments
    /// * `start_height` - Start height (inclusive)
    /// * `stop_hash` - Stop block hash
    ///
    /// # Returns
    /// Vector of filter header hashes (header_hash() from FilterHeader)
    pub fn get_filter_headers_range(
        &self,
        start_height: u32,
        stop_hash: Hash,
    ) -> Result<Vec<Hash>> {
        let headers = self.filter_headers.read().unwrap();
        let height_to_hash = self.block_hash_to_height.read().unwrap();

        // Find stop height
        let stop_height = height_to_hash
            .get(&stop_hash)
            .copied()
            .ok_or_else(|| anyhow!("Stop hash not found"))?;

        if start_height > stop_height {
            return Err(anyhow!("Start height > stop height"));
        }

        let mut header_hashes = Vec::new();
        for height in start_height..=stop_height {
            if let Some(header) = headers.get(height as usize) {
                header_hashes.push(header.header_hash());
            }
        }

        Ok(header_hashes)
    }

    /// Get filter checkpoints (every 1000 blocks per BIP157)
    ///
    /// # Arguments
    /// * `stop_hash` - Stop block hash
    ///
    /// # Returns
    /// Vector of filter header hashes at checkpoint intervals
    pub fn get_filter_checkpoints(&self, stop_hash: Hash) -> Result<Vec<Hash>> {
        let height_to_hash = self.block_hash_to_height.read().unwrap();
        let stop_height = height_to_hash
            .get(&stop_hash)
            .copied()
            .ok_or_else(|| anyhow!("Stop hash not found"))?;

        let headers = self.filter_headers.read().unwrap();
        let mut checkpoints = Vec::new();

        // Checkpoints every 1000 blocks (per BIP157)
        let checkpoint_interval = 1000;

        for height in (0..=stop_height).step_by(checkpoint_interval as usize) {
            if let Some(header) = headers.get(height as usize) {
                checkpoints.push(header.header_hash());
            }
        }

        Ok(checkpoints)
    }

    /// Get previous filter header (for Cfheaders response)
    pub fn get_prev_filter_header(&self, start_height: u32) -> Option<bip157::FilterHeader> {
        if start_height == 0 {
            return None;
        }
        self.filter_headers
            .read()
            .unwrap()
            .get((start_height - 1) as usize)
            .cloned()
    }

    /// Calculate block hash from header using proper Bitcoin double SHA256
    fn calculate_block_hash(&self, header: &BlockHeader) -> Hash {
        use crate::storage::hashing::double_sha256;

        // Serialize header
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header.version.to_le_bytes());
        bytes.extend_from_slice(&header.prev_block_hash);
        bytes.extend_from_slice(&header.merkle_root);
        bytes.extend_from_slice(&header.timestamp.to_le_bytes());
        bytes.extend_from_slice(&header.bits.to_le_bytes());
        bytes.extend_from_slice(&header.nonce.to_le_bytes());

        // Double SHA256
        double_sha256(&bytes)
    }

    /// Get current chain height
    pub fn current_height(&self) -> u32 {
        *self.current_height.read().unwrap()
    }

    /// Remove filter for a pruned block (keeps filter header for verification)
    ///
    /// When a block is pruned, we can remove the filter data to save memory,
    /// but we must keep the filter header for chain verification.
    pub fn remove_filter_for_pruned_block(&self, block_hash: &Hash) -> Result<()> {
        // Remove filter from cache
        self.filters.write().unwrap().remove(block_hash);

        // Note: We do NOT remove the filter header - it's required for verification
        // The filter header chain must remain intact even after pruning

        Ok(())
    }

    /// Check if a filter exists for a block
    pub fn has_filter(&self, block_hash: &Hash) -> bool {
        self.filters.read().unwrap().contains_key(block_hash)
    }

    /// Get all block hashes that have filters cached
    pub fn get_cached_filter_hashes(&self) -> Vec<Hash> {
        self.filters.read().unwrap().keys().cloned().collect()
    }
}

impl Default for BlockFilterService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_service_creation() {
        let service = BlockFilterService::new();
        assert_eq!(service.current_height(), 0);
    }

    #[test]
    fn test_get_filter_nonexistent() {
        let service = BlockFilterService::new();
        let hash = [0u8; 32];
        assert!(service.get_filter(&hash).is_none());
    }
}
