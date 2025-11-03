//! Block sync coordinator
//! 
//! Handles blockchain synchronization, header download, block validation,
//! and chain reorganization.

use anyhow::Result;
use consensus_proof::{BlockHeader, Block, UtxoSet, segwit::Witness, ValidationResult};
use std::collections::HashMap;
use tracing::{debug, info, error};
use crate::storage::blockstore::BlockStore;
use crate::node::block_processor::{parse_block_from_wire, store_block_with_context, prepare_block_validation_context, validate_block_with_context};

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
    pub fn process_block(
        &mut self,
        blockstore: &BlockStore,
        block_data: &[u8],
        current_height: u64,
        utxo_set: &mut UtxoSet,
    ) -> Result<bool> {
        // Parse block from wire format (extracts witness data)
        let (block, witnesses) = parse_block_from_wire(block_data)?;
        
        // Prepare validation context (get witnesses and headers)
        let (stored_witnesses, recent_headers) = prepare_block_validation_context(
            blockstore,
            &block,
            current_height,
        )?;
        
        // Use witnesses from wire format (they may not be stored yet)
        let witnesses_to_use = if !witnesses.is_empty() {
            &witnesses
        } else {
            &stored_witnesses
        };
        
        // Validate block with witness data and headers
        let validation_result = validate_block_with_context(
            blockstore,
            &block,
            witnesses_to_use,
            utxo_set,
            current_height,
        )?;
        
        if matches!(validation_result, ValidationResult::Valid) {
            // Store block with witnesses and update headers
            store_block_with_context(
                blockstore,
                &block,
                witnesses_to_use,
                current_height,
            )?;
            
            info!("Block validated and stored at height {}", current_height);
            Ok(true)
        } else {
            error!("Block validation failed at height {}", current_height);
            Ok(false)
        }
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
    use consensus_proof::types::BlockHeader;
    
    #[test]
    fn test_sync_coordinator_new() {
        let coordinator = SyncCoordinator::new();
        assert_eq!(coordinator.progress(), 0.0);
    }
}
