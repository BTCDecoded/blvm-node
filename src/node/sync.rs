//! Block sync coordinator
//!
//! Handles blockchain synchronization, header download, block validation,
//! and chain reorganization.

use crate::node::block_processor::{
    parse_block_from_wire, prepare_block_validation_context, protocol_version_to_network,
    store_block_with_context_and_index, validate_block_with_context,
};
use crate::node::metrics::MetricsCollector;
use crate::node::performance::{OperationType, PerformanceProfiler, PerformanceTimer};
use crate::storage::blockstore::BlockStore;
use crate::storage::Storage;
use anyhow::Result;
use bllvm_protocol::{Block, BlockHeader, ProtocolVersion, UtxoSet, ValidationResult};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info};

/// Block provider for dependency injection
pub struct BlockProvider {
    /// Mock block storage
    blocks: std::collections::HashMap<[u8; 32], Block>,
    /// Mock header storage
    headers: std::collections::HashMap<[u8; 32], BlockHeader>,
    /// Mock block count
    block_count: u64,
}

/// Sync state machine
pub struct SyncStateMachine {
    /// Current sync state
    state: SyncState,
    /// Best known header
    best_header: Option<BlockHeader>,
    /// Current chain tip
    chain_tip: Option<BlockHeader>,
    /// Sync progress (0.0 to 1.0)
    progress: f64,
    /// Error message if in error state
    error_message: Option<String>,
}

impl Default for SyncStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncStateMachine {
    /// Create a new sync state machine
    pub fn new() -> Self {
        Self {
            state: SyncState::Initial,
            best_header: None,
            chain_tip: None,
            progress: 0.0,
            error_message: None,
        }
    }

    /// Transition to a new state
    pub fn transition_to(&mut self, new_state: SyncState) {
        debug!("Sync state transition: {:?} -> {:?}", self.state, new_state);
        self.state = new_state;
        self.update_progress();
    }

    /// Set error state
    pub fn set_error(&mut self, error: String) {
        self.state = SyncState::Error(error.clone());
        self.error_message = Some(error);
        self.progress = 0.0;
    }

    /// Update best header
    pub fn update_best_header(&mut self, header: BlockHeader) {
        self.best_header = Some(header);
    }

    /// Update chain tip
    pub fn update_chain_tip(&mut self, header: BlockHeader) {
        self.chain_tip = Some(header);
    }

    /// Get current state
    pub fn state(&self) -> &SyncState {
        &self.state
    }

    /// Get sync progress
    pub fn progress(&self) -> f64 {
        self.progress
    }

    /// Check if sync is complete
    pub fn is_synced(&self) -> bool {
        matches!(self.state, SyncState::Synced)
    }

    /// Get best header
    pub fn best_header(&self) -> Option<&BlockHeader> {
        self.best_header.as_ref()
    }

    /// Get chain tip
    pub fn chain_tip(&self) -> Option<&BlockHeader> {
        self.chain_tip.as_ref()
    }

    /// Update progress based on current state
    fn update_progress(&mut self) {
        self.progress = match self.state {
            SyncState::Initial => 0.0,
            SyncState::Headers => 0.3,
            SyncState::Blocks => 0.6,
            SyncState::Synced => 1.0,
            SyncState::Error(_) => 0.0,
        };
    }
}

/// Sync states
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncState {
    Initial,
    Headers,
    Blocks,
    Synced,
    Error(String),
}

/// Sync coordinator that manages blockchain synchronization
pub struct SyncCoordinator {
    state_machine: SyncStateMachine,
    block_provider: BlockProvider,
}

impl Default for SyncCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for SyncCoordinator {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl SyncCoordinator {
    /// Create a new sync coordinator
    pub fn new() -> Self {
        Self {
            state_machine: SyncStateMachine::new(),
            block_provider: BlockProvider::new(),
        }
    }

    /// Start sync process
    pub fn start_sync(&mut self) -> Result<()> {
        info!("Starting blockchain sync");
        self.state_machine.transition_to(SyncState::Headers);

        // In a real implementation, we would download and validate blocks
        // For now, just transition to synced state
        self.state_machine.transition_to(SyncState::Synced);

        Ok(())
    }

    /// Get sync progress
    pub fn progress(&self) -> f64 {
        self.state_machine.progress()
    }

    /// Check if sync is complete
    pub fn is_synced(&self) -> bool {
        self.state_machine.is_synced()
    }

    /// Process an incoming block from the network
    ///
    /// This function:
    /// 1. Parses the block from wire format (extracting witness data)
    /// 2. Validates the block with proper witnesses and headers
    /// 3. Stores the block with witnesses and updates headers
    /// 4. Indexes transactions if storage is provided
    pub fn process_block(
        &mut self,
        blockstore: &BlockStore,
        storage: Option<&Arc<Storage>>,
        block_data: &[u8],
        current_height: u64,
        utxo_set: &mut UtxoSet,
        protocol_version: ProtocolVersion,
        metrics: Option<Arc<MetricsCollector>>,
        profiler: Option<Arc<PerformanceProfiler>>,
    ) -> Result<bool> {
        let _timer = profiler
            .as_ref()
            .map(|p| PerformanceTimer::start(Arc::clone(p), OperationType::BlockProcessing));
        let start_time = Instant::now();

        // Parse block from wire format (extracts witness data)
        let (block, witnesses) = parse_block_from_wire(block_data)?;

        // Prepare validation context (get witnesses and headers)
        let (stored_witnesses, recent_headers) =
            prepare_block_validation_context(blockstore, &block, current_height)?;

        // Use witnesses from wire format (they may not be stored yet)
        let witnesses_to_use = if !witnesses.is_empty() {
            &witnesses
        } else {
            &stored_witnesses
        };

        // Log recent headers availability for BIP113 median time validation
        if let Some(ref headers) = recent_headers {
            debug!(
                "Using {} recent headers for BIP113 median time validation",
                headers.len()
            );
        }

        // Validate block with witness data and headers
        let network = protocol_version_to_network(protocol_version);
        let validation_result = validate_block_with_context(
            blockstore,
            &block,
            witnesses_to_use,
            utxo_set,
            current_height,
            network,
        )?;

        let processing_time = start_time.elapsed();

        if matches!(validation_result, ValidationResult::Valid) {
            // Store block with witnesses, update headers, and index transactions
            store_block_with_context_and_index(
                blockstore,
                storage,
                &block,
                witnesses_to_use,
                current_height,
            )?;

            // Update metrics
            if let Some(ref metrics) = metrics {
                metrics.update_storage(|m| {
                    m.block_count += 1;
                    m.transaction_count += block.transactions.len();
                });
                metrics.update_performance(|m| {
                    let time_ms = processing_time.as_secs_f64() * 1000.0;
                    // Update average block processing time (exponential moving average)
                    m.avg_block_processing_time_ms =
                        (m.avg_block_processing_time_ms * 0.9) + (time_ms * 0.1);
                    // Update blocks per second
                    if processing_time.as_secs_f64() > 0.0 {
                        m.blocks_per_second = 1.0 / processing_time.as_secs_f64();
                    }
                });
            }

            info!(
                "Block validated and stored at height {} (took {:?})",
                current_height, processing_time
            );
            Ok(true)
        } else {
            error!("Block validation failed at height {}", current_height);
            Ok(false)
        }
    }
}

impl Default for BlockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockProvider {
    /// Create a new block provider
    pub fn new() -> Self {
        Self {
            blocks: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
            block_count: 0,
        }
    }

    /// Get a block by hash
    pub fn get_block(&self, hash: &[u8; 32]) -> Result<Option<Block>> {
        Ok(self.blocks.get(hash).cloned())
    }

    /// Get a block header by hash
    pub fn get_block_header(&self, hash: &[u8; 32]) -> Result<Option<BlockHeader>> {
        Ok(self.headers.get(hash).cloned())
    }

    /// Get the best block header
    pub fn get_best_header(&self) -> Result<Option<BlockHeader>> {
        Ok(self.headers.values().last().cloned())
    }

    /// Store a block
    pub fn store_block(&mut self, block: &Block) -> Result<()> {
        // Simplified hash calculation
        let hash = self.calculate_block_hash(block);
        self.blocks.insert(hash, block.clone());
        self.block_count += 1;
        Ok(())
    }

    /// Store a block header
    pub fn store_block_header(&mut self, header: &BlockHeader) -> Result<()> {
        // Simplified hash calculation
        let hash = self.calculate_header_hash(header);
        self.headers.insert(hash, header.clone());
        Ok(())
    }

    /// Get block count
    pub fn get_block_count(&self) -> Result<u64> {
        Ok(self.block_count)
    }

    /// Calculate block hash (simplified)
    fn calculate_block_hash(&self, block: &Block) -> [u8; 32] {
        let mut hash = [0u8; 32];
        hash[0] = block.header.version as u8;
        hash[1] = block.transactions.len() as u8;
        hash
    }

    /// Calculate header hash (simplified)
    fn calculate_header_hash(&self, header: &BlockHeader) -> [u8; 32] {
        let mut hash = [0u8; 32];
        hash[0] = header.version as u8;
        hash[1] = header.timestamp as u8;
        hash
    }
}

/// Mock block provider for testing
pub struct MockBlockProvider {
    blocks: HashMap<[u8; 32], Block>,
    headers: HashMap<[u8; 32], BlockHeader>,
    best_header: Option<BlockHeader>,
    block_count: u64,
}

impl Default for MockBlockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MockBlockProvider {
    /// Create a new mock block provider
    pub fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            headers: HashMap::new(),
            best_header: None,
            block_count: 0,
        }
    }

    /// Get a block by hash
    pub fn get_block(&self, hash: &[u8; 32]) -> Result<Option<Block>> {
        Ok(self.blocks.get(hash).cloned())
    }

    /// Get a block header by hash
    pub fn get_block_header(&self, hash: &[u8; 32]) -> Result<Option<BlockHeader>> {
        Ok(self.headers.get(hash).cloned())
    }

    /// Get the best block header
    pub fn get_best_header(&self) -> Result<Option<BlockHeader>> {
        Ok(self.best_header.clone())
    }

    /// Store a block
    pub fn store_block(&mut self, block: &Block) -> Result<()> {
        // Simplified hash calculation
        let hash = self.calculate_block_hash(block);
        self.blocks.insert(hash, block.clone());
        self.block_count += 1;
        Ok(())
    }

    /// Store a block header
    pub fn store_block_header(&mut self, header: &BlockHeader) -> Result<()> {
        // Simplified hash calculation
        let hash = self.calculate_header_hash(header);
        self.headers.insert(hash, header.clone());
        self.best_header = Some(header.clone());
        Ok(())
    }

    /// Get block count
    pub fn get_block_count(&self) -> Result<u64> {
        Ok(self.block_count)
    }

    /// Calculate block hash (simplified)
    fn calculate_block_hash(&self, block: &Block) -> [u8; 32] {
        let mut hash = [0u8; 32];
        hash[0] = block.header.version as u8;
        hash[1] = block.transactions.len() as u8;
        hash
    }

    /// Calculate header hash (simplified)
    fn calculate_header_hash(&self, header: &BlockHeader) -> [u8; 32] {
        let mut hash = [0u8; 32];
        hash[0] = header.version as u8;
        hash[1] = header.timestamp as u8;
        hash
    }

    /// Add block (for testing)
    pub fn add_block(&mut self, block: Block) {
        let hash = self.calculate_block_hash(&block);
        self.blocks.insert(hash, block);
        self.block_count += 1;
    }

    /// Add header (for testing)
    pub fn add_header(&mut self, header: BlockHeader) {
        let hash = self.calculate_header_hash(&header);
        self.headers.insert(hash, header.clone());
        if self.best_header.is_none() {
            self.best_header = Some(header);
        }
    }

    /// Set best header (for testing)
    pub fn set_best_header(&mut self, header: BlockHeader) {
        self.best_header = Some(header);
    }

    /// Set block count (for testing)
    pub fn set_block_count(&mut self, count: u64) {
        self.block_count = count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_coordinator_new() {
        let coordinator = SyncCoordinator::new();
        assert_eq!(coordinator.progress(), 0.0);
    }
}
