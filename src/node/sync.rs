//! Block sync coordinator
//!
//! Handles blockchain synchronization, header download, block validation,
//! and chain reorganization.

use crate::node::block_processor::{
    parse_block_from_wire, prepare_block_validation_context, store_block_with_context_and_index,
    validate_block_with_context,
};
use crate::node::metrics::MetricsCollector;
use crate::node::performance::{OperationType, PerformanceProfiler, PerformanceTimer};
use crate::storage::blockstore::BlockStore;
use crate::storage::Storage;
use anyhow::Result;
use blvm_protocol::{BitcoinProtocolEngine, Block, BlockHeader, UtxoSet, ValidationResult};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, warn};

/// Abstraction for reading and writing blocks/headers by hash.
/// Implemented by [InMemoryBlockProvider] (default sync path) and [MockBlockProvider] (tests).
pub trait BlockProvider {
    fn get_block(&self, hash: &[u8; 32]) -> Result<Option<Block>>;
    fn get_block_header(&self, hash: &[u8; 32]) -> Result<Option<BlockHeader>>;
    fn get_best_header(&self) -> Result<Option<BlockHeader>>;
    fn store_block(&mut self, block: &Block) -> Result<()>;
    fn store_block_header(&mut self, header: &BlockHeader) -> Result<()>;
    fn get_block_count(&self) -> Result<u64>;
}

/// In-memory block provider for dependency injection (default sync coordinator).
pub struct InMemoryBlockProvider {
    blocks: std::collections::HashMap<[u8; 32], Block>,
    headers: std::collections::HashMap<[u8; 32], BlockHeader>,
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

impl SyncState {
    /// String representation for event payloads (e.g. SyncStateChanged).
    pub fn as_event_str(&self) -> &'static str {
        match self {
            SyncState::Initial => "Initial",
            SyncState::Headers => "Headers",
            SyncState::Blocks => "Blocks",
            SyncState::Synced => "Synced",
            SyncState::Error(_) => "Error",
        }
    }
}

/// Sync coordinator that manages blockchain synchronization
pub struct SyncCoordinator {
    state_machine: SyncStateMachine,
    block_provider: InMemoryBlockProvider,
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
            block_provider: InMemoryBlockProvider::new(),
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

    /// Start parallel IBD sync (if enabled and peers available).
    ///
    /// Attempts to use parallel IBD for faster initial block download.
    /// Sequential sync is not supported; IBD must succeed in parallel mode.
    ///
    /// - `synced_chain_height`: last flushed tip `H` from `Storage::chain().get_height()` (0 if none).
    /// - `first_block_height`: first block to validate this session — `0` if no tip yet, else `H + 1`.
    #[cfg(feature = "production")]
    pub async fn start_parallel_ibd(
        &mut self,
        synced_chain_height: u64,
        first_block_height: u64,
        target_height: u64,
        blockstore: Arc<BlockStore>,
        storage: Option<Arc<Storage>>,
        protocol: Arc<BitcoinProtocolEngine>,
        utxo_set: &mut UtxoSet,
        network: Option<Arc<crate::network::NetworkManager>>,
        peer_addresses: Vec<String>,
        ibd_config: Option<&crate::config::IbdConfig>,
        event_publisher: Option<Arc<crate::node::event_publisher::EventPublisher>>,
        ibd_data_dir: Option<&std::path::Path>,
    ) -> Result<bool> {
        use crate::node::parallel_ibd::{ParallelIBD, ParallelIBDConfig};

        // Check if we have enough peers for parallel IBD.
        // Allow 1 peer when preferred_peers is set (e.g. LAN-only IBD).
        let config = ParallelIBDConfig::from_config(ibd_config);
        let min_peers = if config.preferred_peers.is_empty() {
            2
        } else {
            1
        };
        if peer_addresses.len() < min_peers {
            debug!(
                "Not enough peers for parallel IBD (have {}, need {}). Sequential sync is not supported.",
                peer_addresses.len(), min_peers
            );
            return Ok(false);
        }

        // Check if we're actually in IBD (significant height difference)
        if target_height <= synced_chain_height {
            debug!(
                "Already synced (synced tip {} >= target {}), skipping parallel IBD",
                synced_chain_height, target_height
            );
            return Ok(false);
        }

        info!(
            "Attempting parallel IBD from block height {} (synced tip {}) to {} with {} peers",
            first_block_height,
            synced_chain_height,
            target_height,
            peer_addresses.len()
        );

        // Transition to Blocks and emit SyncStateChanged before starting block download
        let old_state = self.state_machine.state().as_event_str();
        self.state_machine.transition_to(SyncState::Blocks);
        if let Some(ref ep) = event_publisher {
            ep.publish_sync_state_changed(old_state, "Blocks").await;
        }

        // Create parallel IBD coordinator (Arc allows dedicated validation thread to call validate_block_only)
        let mut parallel_ibd = ParallelIBD::new(config);
        parallel_ibd.initialize_peers(&peer_addresses);
        let parallel_ibd = std::sync::Arc::new(parallel_ibd);

        // Attempt parallel sync (clone event_publisher so we can use it after for SyncStateChanged)
        let ep_for_completion = event_publisher.clone();
        match parallel_ibd
            .sync_parallel(
                first_block_height,
                target_height,
                &peer_addresses,
                blockstore,
                storage.as_ref(),
                std::sync::Arc::clone(&protocol),
                utxo_set,
                network,
                event_publisher,
            )
            .await
        {
            Ok(()) => {
                info!("Parallel IBD completed successfully");
                self.state_machine.transition_to(SyncState::Synced);
                if let Some(ref ep) = ep_for_completion {
                    ep.publish_sync_state_changed("Blocks", "Synced").await;
                }
                Ok(true)
            }
            Err(e) => {
                if let Some(dir) = ibd_data_dir {
                    if crate::storage::ibd_autorepair::validation_error_suggests_utxo_repair(&e) {
                        if let Err(flag_e) =
                            crate::storage::ibd_autorepair::set_ibd_utxo_repair_flag(dir)
                        {
                            tracing::warn!("Could not write IBD UTXO repair marker: {}", flag_e);
                        }
                    }
                }
                error!("Parallel IBD failed: {}. Sequential sync is not supported - IBD must succeed in parallel mode.", e);
                Err(e)
            }
        }
    }

    #[cfg(not(feature = "production"))]
    pub async fn start_parallel_ibd(
        &mut self,
        _synced_chain_height: u64,
        _first_block_height: u64,
        _target_height: u64,
        _blockstore: Arc<BlockStore>,
        _storage: Option<Arc<Storage>>,
        _protocol: Arc<BitcoinProtocolEngine>,
        _utxo_set: &mut UtxoSet,
        _network: Option<Arc<crate::network::NetworkManager>>,
        _peer_addresses: Vec<String>,
        _ibd_config: Option<&crate::config::IbdConfig>,
        _event_publisher: Option<Arc<crate::node::event_publisher::EventPublisher>>,
        _ibd_data_dir: Option<&std::path::Path>,
    ) -> Result<bool> {
        // Parallel IBD not available without production feature
        Ok(false)
    }

    /// Get sync progress
    pub fn progress(&self) -> f64 {
        self.state_machine.progress()
    }

    /// Check if sync is complete
    pub fn is_synced(&self) -> bool {
        self.state_machine.is_synced()
    }

    /// Current sync phase (for module APIs, e.g. [`crate::module::traits::NodeAPI::get_sync_status`]).
    pub fn current_sync_state(&self) -> SyncState {
        self.state_machine.state().clone()
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
        protocol: &BitcoinProtocolEngine,
        storage: Option<&Arc<Storage>>,
        block_data: &[u8],
        current_height: u64,
        utxo_set: &mut UtxoSet,
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

        // Validate block with witness data and headers using protocol validation
        let validation_result = validate_block_with_context(
            blockstore,
            protocol,
            &block,
            witnesses_to_use,
            utxo_set,
            current_height,
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

impl Default for InMemoryBlockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryBlockProvider {
    /// Create a new in-memory block provider
    pub fn new() -> Self {
        Self {
            blocks: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
            block_count: 0,
        }
    }

    fn calculate_block_hash(&self, block: &Block) -> [u8; 32] {
        let mut hash = [0u8; 32];
        hash[0] = block.header.version as u8;
        hash[1] = block.transactions.len() as u8;
        hash
    }

    fn calculate_header_hash(&self, header: &BlockHeader) -> [u8; 32] {
        let mut hash = [0u8; 32];
        hash[0] = header.version as u8;
        hash[1] = header.timestamp as u8;
        hash
    }
}

impl BlockProvider for InMemoryBlockProvider {
    fn get_block(&self, hash: &[u8; 32]) -> Result<Option<Block>> {
        Ok(self.blocks.get(hash).cloned())
    }

    fn get_block_header(&self, hash: &[u8; 32]) -> Result<Option<BlockHeader>> {
        Ok(self.headers.get(hash).cloned())
    }

    fn get_best_header(&self) -> Result<Option<BlockHeader>> {
        Ok(self.headers.values().last().cloned())
    }

    fn store_block(&mut self, block: &Block) -> Result<()> {
        let hash = self.calculate_block_hash(block);
        self.blocks.insert(hash, block.clone());
        self.block_count += 1;
        Ok(())
    }

    fn store_block_header(&mut self, header: &BlockHeader) -> Result<()> {
        let hash = self.calculate_header_hash(header);
        self.headers.insert(hash, header.clone());
        Ok(())
    }

    fn get_block_count(&self) -> Result<u64> {
        Ok(self.block_count)
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

impl BlockProvider for MockBlockProvider {
    fn get_block(&self, hash: &[u8; 32]) -> Result<Option<Block>> {
        Ok(self.blocks.get(hash).cloned())
    }

    fn get_block_header(&self, hash: &[u8; 32]) -> Result<Option<BlockHeader>> {
        Ok(self.headers.get(hash).cloned())
    }

    fn get_best_header(&self) -> Result<Option<BlockHeader>> {
        Ok(self.best_header.clone())
    }

    fn store_block(&mut self, block: &Block) -> Result<()> {
        let hash = self.calculate_block_hash(block);
        self.blocks.insert(hash, block.clone());
        self.block_count += 1;
        Ok(())
    }

    fn store_block_header(&mut self, header: &BlockHeader) -> Result<()> {
        let hash = self.calculate_header_hash(header);
        self.headers.insert(hash, header.clone());
        self.best_header = Some(header.clone());
        Ok(())
    }

    fn get_block_count(&self) -> Result<u64> {
        Ok(self.block_count)
    }
}

/// Run UTXO commitments initial sync
///
/// Fetches the UTXO commitment from peers via consensus. Used when the node wants to
/// verify its locally-generated commitment against network consensus, or when starting
/// with an existing chain (non-IBD) and needs the current commitment.
///
/// **When to call:**
/// - After IBD completes: spawn as background task to verify commitment matches peers
/// - On startup with existing chain: if commitment store is empty, fetch from peers
///
/// **Requirements:**
/// - `headers`: Header chain from genesis to tip (from storage blockstore/chain)
/// - `peers`: Vec of (PeerInfo, peer_id) — build from `network.peer_socket_addresses()`
///   with `PeerInfo { address: addr.ip(), asn: None, country: None, implementation: None,
///   subnet: PeerInfo::extract_subnet(addr.ip()) }` and peer_id e.g. `format!("tcp:{}", addr)`
///
/// **Entry point:** Not yet wired. Call from `node/mod.rs` after IBD or on startup.
/// See `docs/UTXO_COMMITMENTS_IBD_INTEGRATION.md`.
#[cfg(feature = "utxo-commitments")]
pub async fn run_utxo_commitments_initial_sync(
    network_manager: std::sync::Arc<tokio::sync::RwLock<crate::network::NetworkManager>>,
    headers: &[blvm_consensus::BlockHeader],
    peers: Vec<(
        blvm_protocol::utxo_commitments::peer_consensus::PeerInfo,
        String,
    )>,
) -> anyhow::Result<blvm_protocol::utxo_commitments::data_structures::UtxoCommitment> {
    use crate::network::utxo_commitments_client::UtxoCommitmentsClient;
    use blvm_protocol::utxo_commitments::initial_sync::InitialSync;
    use blvm_protocol::utxo_commitments::peer_consensus::ConsensusConfig;

    let client = UtxoCommitmentsClient::new(std::sync::Arc::clone(&network_manager));
    let config = ConsensusConfig::default();
    let initial_sync = InitialSync::new(config);

    let commitment = initial_sync
        .execute_initial_sync(&peers, headers, &client)
        .await?;

    Ok(commitment)
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
