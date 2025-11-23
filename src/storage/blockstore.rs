//! Block storage implementation
//!
//! Stores blocks by hash and maintains block index by height.

use crate::storage::database::{Database, Tree};
use anyhow::Result;
use bllvm_protocol::segwit::Witness;
use bllvm_protocol::{Block, BlockHeader, Hash};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Block metadata stored separately from block data for fast RPC lookups
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockMetadata {
    pub n_tx: u32,
    // Could add more metadata here: size, weight, etc.
}

/// Block storage manager
pub struct BlockStore {
    #[allow(dead_code)]
    db: Arc<dyn Database>,
    blocks: Arc<dyn Tree>,
    headers: Arc<dyn Tree>,
    height_index: Arc<dyn Tree>,   // height → hash
    hash_to_height: Arc<dyn Tree>, // hash → height (reverse index for O(1) lookup)
    witnesses: Arc<dyn Tree>,
    recent_headers: Arc<dyn Tree>, // For median time-past: stores last 11+ headers by height
    block_metadata: Arc<dyn Tree>, // hash → BlockMetadata (for fast TX count lookup)
}

impl BlockStore {
    /// Create a new block store
    pub fn new(db: Arc<dyn Database>) -> Result<Self> {
        let blocks = Arc::from(db.open_tree("blocks")?);
        let headers = Arc::from(db.open_tree("headers")?);
        let height_index = Arc::from(db.open_tree("height_index")?);
        let hash_to_height = Arc::from(db.open_tree("hash_to_height")?);
        let witnesses = Arc::from(db.open_tree("witnesses")?);
        let recent_headers = Arc::from(db.open_tree("recent_headers")?);
        let block_metadata = Arc::from(db.open_tree("block_metadata")?);

        Ok(Self {
            db,
            blocks,
            headers,
            height_index,
            hash_to_height,
            witnesses,
            recent_headers,
            block_metadata,
        })
    }

    /// Store a block
    pub fn store_block(&self, block: &Block) -> Result<()> {
        let block_hash = self.block_hash(block);
        let block_data = bincode::serialize(block)?;

        self.blocks.insert(block_hash.as_slice(), &block_data)?;
        let header_data = bincode::serialize(&block.header)?;
        self.headers.insert(block_hash.as_slice(), &header_data)?;

        // Store block metadata separately for fast RPC lookups (TX count, etc.)
        let metadata = BlockMetadata {
            n_tx: block.transactions.len() as u32,
        };
        let metadata_data = bincode::serialize(&metadata)?;
        self.block_metadata
            .insert(block_hash.as_slice(), &metadata_data)?;

        // Store header for median time-past calculation
        // We'll need height passed separately, so this will be called after store_height
        // For now, just store the header - height will be set via store_recent_header

        Ok(())
    }

    /// Store a block with witness data and height
    pub fn store_block_with_witness(
        &self,
        block: &Block,
        witnesses: &[Witness],
        height: u64,
    ) -> Result<()> {
        let block_hash = self.block_hash(block);

        // Store block
        self.store_block(block)?;

        // Store witnesses
        if !witnesses.is_empty() {
            self.store_witness(&block_hash, witnesses)?;
        }

        // Store header for median time-past
        self.store_recent_header(height, &block.header)?;

        Ok(())
    }

    /// Store witness data for a block
    pub fn store_witness(&self, block_hash: &Hash, witness: &[Witness]) -> Result<()> {
        let witness_data = bincode::serialize(witness)?;
        self.witnesses
            .insert(block_hash.as_slice(), &witness_data)?;
        Ok(())
    }

    /// Get witness data for a block
    pub fn get_witness(&self, block_hash: &Hash) -> Result<Option<Vec<Witness>>> {
        if let Some(data) = self.witnesses.get(block_hash.as_slice())? {
            let witnesses: Vec<Witness> = bincode::deserialize(&data)?;
            Ok(Some(witnesses))
        } else {
            Ok(None)
        }
    }

    /// Store recent headers for median time-past calculation
    /// Maintains a sliding window of the last 11+ headers by height
    pub fn store_recent_header(&self, height: u64, header: &BlockHeader) -> Result<()> {
        let height_bytes = height.to_be_bytes();
        let header_data = bincode::serialize(header)?;
        self.recent_headers.insert(&height_bytes, &header_data)?;

        // Clean up old headers (keep only last 11 for median time-past)
        // Remove headers older than height - 11
        if height > 11 {
            let remove_height = height - 12;
            let remove_bytes = remove_height.to_be_bytes();
            self.recent_headers.remove(&remove_bytes)?;
        }

        Ok(())
    }

    /// Get recent headers for median time-past calculation (BIP113)
    /// Returns up to `count` most recent headers, ordered from oldest to newest
    pub fn get_recent_headers(&self, count: usize) -> Result<Vec<BlockHeader>> {
        let mut headers = Vec::new();

        // Get current height (from height_index)
        let mut current_height: Option<u64> = None;
        let mut items: Vec<_> = self.height_index.iter().collect();
        items.reverse();
        for item in items.into_iter().flatten() {
            let (height_bytes, _hash) = item;
            let mut height_bytes_array = [0u8; 8];
            height_bytes_array.copy_from_slice(&height_bytes);
            current_height = Some(u64::from_be_bytes(height_bytes_array));
            break;
        }

        if let Some(mut height) = current_height {
            // Collect headers from current_height backwards
            for _ in 0..count {
                let height_bytes = height.to_be_bytes();
                if let Some(data) = self.recent_headers.get(&height_bytes)? {
                    if let Ok(header) = bincode::deserialize::<BlockHeader>(&data) {
                        headers.push(header);
                    }
                }
                if height == 0 {
                    break;
                }
                height -= 1;
            }
        }

        // Reverse to get oldest-to-newest order (required for get_median_time_past)
        headers.reverse();
        Ok(headers)
    }

    /// Get a block by hash
    pub fn get_block(&self, hash: &Hash) -> Result<Option<Block>> {
        if let Some(data) = self.blocks.get(hash.as_slice())? {
            let block: Block = bincode::deserialize(&data)?;
            Ok(Some(block))
        } else {
            Ok(None)
        }
    }

    /// Get a block header by hash
    pub fn get_header(&self, hash: &Hash) -> Result<Option<BlockHeader>> {
        if let Some(data) = self.headers.get(hash.as_slice())? {
            let header: BlockHeader = bincode::deserialize(&data)?;
            Ok(Some(header))
        } else {
            Ok(None)
        }
    }

    /// Store block height index
    /// Maintains both height→hash and hash→height indices for O(1) lookups
    pub fn store_height(&self, height: u64, hash: &Hash) -> Result<()> {
        let height_bytes = height.to_be_bytes();
        // Store height → hash mapping
        self.height_index.insert(&height_bytes, hash.as_slice())?;
        // Store hash → height reverse mapping for O(1) lookup
        self.hash_to_height.insert(hash.as_slice(), &height_bytes)?;
        Ok(())
    }

    /// Get block hash by height
    pub fn get_hash_by_height(&self, height: u64) -> Result<Option<Hash>> {
        let height_bytes = height.to_be_bytes();
        if let Some(data) = self.height_index.get(&height_bytes)? {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&data);
            Ok(Some(hash))
        } else {
            Ok(None)
        }
    }

    /// Get block height by hash (reverse lookup)
    /// Optimized: O(1) lookup using hash_to_height index instead of O(n) iteration
    pub fn get_height_by_hash(&self, hash: &Hash) -> Result<Option<u64>> {
        // Use reverse index for O(1) lookup instead of O(n) iteration
        if let Some(data) = self.hash_to_height.get(hash.as_slice())? {
            let mut height_bytes_array = [0u8; 8];
            height_bytes_array.copy_from_slice(&data);
            return Ok(Some(u64::from_be_bytes(height_bytes_array)));
        }
        Ok(None)
    }

    /// Get block metadata (TX count, etc.) without loading full block
    pub fn get_block_metadata(&self, hash: &Hash) -> Result<Option<BlockMetadata>> {
        if let Some(data) = self.block_metadata.get(hash.as_slice())? {
            let metadata: BlockMetadata = bincode::deserialize(&data)?;
            Ok(Some(metadata))
        } else {
            Ok(None)
        }
    }

    /// Get all blocks in a height range
    pub fn get_blocks_by_height_range(&self, start: u64, end: u64) -> Result<Vec<Block>> {
        let mut blocks = Vec::new();

        for height in start..=end {
            if let Some(hash) = self.get_hash_by_height(height)? {
                if let Some(block) = self.get_block(&hash)? {
                    blocks.push(block);
                }
            }
        }

        Ok(blocks)
    }

    /// Check if a block exists
    pub fn has_block(&self, hash: &Hash) -> Result<bool> {
        self.blocks.contains_key(hash.as_slice())
    }

    /// Get total number of blocks stored
    pub fn block_count(&self) -> Result<usize> {
        self.blocks.len()
    }

    /// Calculate block hash using proper Bitcoin double SHA256
    /// Get the hash of a block
    pub fn get_block_hash(&self, block: &Block) -> Hash {
        self.block_hash(block)
    }

    fn block_hash(&self, block: &Block) -> Hash {
        use crate::storage::hashing::double_sha256;

        // Serialize block header for hashing
        let mut header_data = Vec::new();
        header_data.extend_from_slice(&block.header.version.to_le_bytes());
        header_data.extend_from_slice(&block.header.prev_block_hash);
        header_data.extend_from_slice(&block.header.merkle_root);
        header_data.extend_from_slice(&block.header.timestamp.to_le_bytes());
        header_data.extend_from_slice(&block.header.bits.to_le_bytes());
        header_data.extend_from_slice(&block.header.nonce.to_le_bytes());

        // Calculate Bitcoin double SHA256 hash
        double_sha256(&header_data)
    }

    /// Remove block body (keep header for PoW verification)
    pub fn remove_block_body(&self, hash: &Hash) -> Result<()> {
        self.blocks.remove(hash.as_slice())?;
        Ok(())
    }

    /// Remove witness data for a block
    pub fn remove_witness(&self, hash: &Hash) -> Result<()> {
        self.witnesses.remove(hash.as_slice())?;
        Ok(())
    }

    /// Remove block by height (removes body, keeps header)
    pub fn remove_block_by_height(&self, height: u64) -> Result<()> {
        if let Some(hash) = self.get_hash_by_height(height)? {
            self.remove_block_body(&hash)?;
        }
        Ok(())
    }

    /// Remove blocks in a height range (removes bodies, keeps headers)
    pub fn remove_blocks_by_height_range(&self, start: u64, end: u64) -> Result<u64> {
        let mut removed = 0;
        for height in start..=end {
            if self.remove_block_by_height(height).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Check if a block body exists (not just header)
    pub fn has_block_body(&self, hash: &Hash) -> Result<bool> {
        self.blocks.contains_key(hash.as_slice())
    }
}
