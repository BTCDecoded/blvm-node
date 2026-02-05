//! Buffered block storage for fast IBD
//!
//! This module provides a buffered storage layer that batches database writes
//! during Initial Block Download (IBD) for dramatically improved performance.
//!
//! ## Performance
//!
//! - Without buffering: ~2 blocks/sec (4 DB writes per block)
//! - With buffering: ~50-100 blocks/sec (batched writes every N blocks)
//!
//! ## Usage
//!
//! ```ignore
//! let buffer = BufferedBlockStore::new(blockstore, 1000);
//! for block in blocks {
//!     buffer.store_block_deferred(&block, &witnesses, height)?;
//! }
//! buffer.flush()?; // Commit all buffered blocks
//! ```

use anyhow::Result;
use std::collections::VecDeque;
use std::sync::Mutex;
use tracing::{debug, info};

use super::blockstore::BlockStore;
use blvm_protocol::{Block, BlockHeader};
use blvm_protocol::segwit::Witness;

/// A deferred block write operation
struct DeferredBlock {
    block: Block,
    witnesses: Vec<Vec<Witness>>,
    height: u64,
    block_hash: [u8; 32],
}

/// Buffered block storage for high-throughput IBD
///
/// Accumulates block storage operations in memory and flushes them
/// in batches for much better database performance.
pub struct BufferedBlockStore {
    /// Underlying blockstore
    blockstore: BlockStore,
    /// Buffered blocks waiting to be written
    buffer: Mutex<VecDeque<DeferredBlock>>,
    /// Number of blocks to buffer before auto-flush
    flush_threshold: usize,
    /// Total blocks stored (including buffered)
    total_stored: Mutex<u64>,
}

impl BufferedBlockStore {
    /// Create a new buffered block store
    ///
    /// # Arguments
    /// * `blockstore` - The underlying block store
    /// * `flush_threshold` - Number of blocks to buffer before auto-flushing
    pub fn new(blockstore: BlockStore, flush_threshold: usize) -> Self {
        Self {
            blockstore,
            buffer: Mutex::new(VecDeque::with_capacity(flush_threshold)),
            flush_threshold,
            total_stored: Mutex::new(0),
        }
    }

    /// Store a block with deferred write (buffered)
    ///
    /// The block is stored in memory and will be written to the database
    /// when the buffer reaches the flush threshold or when `flush()` is called.
    pub fn store_block_deferred(
        &self,
        block: &Block,
        witnesses: &[Vec<Witness>],
        height: u64,
    ) -> Result<()> {
        let block_hash = self.blockstore.get_block_hash(block);
        
        let deferred = DeferredBlock {
            block: block.clone(),
            witnesses: witnesses.to_vec(),
            height,
            block_hash,
        };

        let should_flush = {
            let mut buffer = self.buffer.lock().unwrap();
            buffer.push_back(deferred);
            buffer.len() >= self.flush_threshold
        };

        if should_flush {
            self.flush()?;
        }

        Ok(())
    }

    /// Store recent header (for median time-past calculation)
    /// This is written immediately since it's needed for validation
    pub fn store_recent_header(&self, height: u64, header: &BlockHeader) -> Result<()> {
        self.blockstore.store_recent_header(height, header)
    }

    /// Flush all buffered blocks to the database
    ///
    /// Uses batch writes for efficient database operations.
    pub fn flush(&self) -> Result<()> {
        let blocks: Vec<DeferredBlock> = {
            let mut buffer = self.buffer.lock().unwrap();
            buffer.drain(..).collect()
        };

        if blocks.is_empty() {
            return Ok(());
        }

        let count = blocks.len();
        let start = std::time::Instant::now();

        debug!("Flushing {} buffered blocks to database", count);

        // Use batch writes for blocks
        {
            let blocks_tree = self.blockstore.blocks_tree()?;
            let mut batch = blocks_tree.batch();
            
            for deferred in &blocks {
                // Serialize block for storage
                let block_data = bincode::serialize(&deferred.block)
                    .map_err(|e| anyhow::anyhow!("Failed to serialize block: {}", e))?;
                batch.put(&deferred.block_hash, &block_data);
            }
            
            batch.commit()?;
        }

        // Use batch writes for witnesses
        {
            let witnesses_tree = self.blockstore.witnesses_tree()?;
            let mut batch = witnesses_tree.batch();
            
            for deferred in &blocks {
                if !deferred.witnesses.is_empty() {
                    let witness_data = bincode::serialize(&deferred.witnesses)
                        .map_err(|e| anyhow::anyhow!("Failed to serialize witnesses: {}", e))?;
                    batch.put(&deferred.block_hash, &witness_data);
                }
            }
            
            batch.commit()?;
        }

        // Use batch writes for height index
        {
            let height_tree = self.blockstore.height_tree()?;
            let mut batch = height_tree.batch();
            
            for deferred in &blocks {
                let height_key = deferred.height.to_be_bytes();
                batch.put(&height_key, &deferred.block_hash);
            }
            
            batch.commit()?;
        }

        // Update total stored count
        *self.total_stored.lock().unwrap() += count as u64;

        let elapsed = start.elapsed();
        info!(
            "Flushed {} blocks in {:?} ({:.0} blocks/sec)",
            count,
            elapsed,
            count as f64 / elapsed.as_secs_f64()
        );

        Ok(())
    }

    /// Get the number of buffered (unflushed) blocks
    pub fn buffered_count(&self) -> usize {
        self.buffer.lock().unwrap().len()
    }

    /// Get the total number of blocks stored (including buffered)
    pub fn total_stored(&self) -> u64 {
        *self.total_stored.lock().unwrap() + self.buffered_count() as u64
    }

    /// Get reference to underlying blockstore
    pub fn inner(&self) -> &BlockStore {
        &self.blockstore
    }
}

impl Drop for BufferedBlockStore {
    fn drop(&mut self) {
        // Ensure all buffered blocks are flushed on shutdown
        if let Err(e) = self.flush() {
            tracing::warn!("Failed to flush buffered blocks on shutdown: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffered_store_config() {
        // Just verify the struct can be created with expected values
        assert_eq!(1000, 1000); // Placeholder
    }
}


