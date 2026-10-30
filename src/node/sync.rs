//! Block sync coordinator
//!
//! Handles blockchain synchronization, header download, block validation,
//! and chain reorganization.

use crate::node::block_processor::{
    mtp_headers_before, parse_block_from_wire, prepare_block_validation_context,
    store_block_with_context_and_index, validate_block_with_context,
};
#[cfg(feature = "production")]
use crate::node::block_processor::{
    validate_block_protocol_only, validate_block_with_context_and_undo,
};
use crate::node::event_publisher::EventPublisher;
use crate::node::metrics::MetricsCollector;
use crate::node::performance::{OperationType, PerformanceProfiler, PerformanceTimer};
#[cfg(feature = "production")]
use crate::node::reorg_executor::{
    commit_utxo_snapshot_delta, note_snapshot_height, read_snapshot_inputs,
    try_activate_heavier_fork,
};
use crate::storage::Storage;
use crate::storage::blockstore::BlockStore;
use anyhow::Result;
use blvm_protocol::{BitcoinProtocolEngine, Block, BlockHeader, UtxoSet, ValidationResult};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
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
/// A heavier-fork body whose parent block is not indexed yet.
///
/// Peers send the child before the parent. Dropping it leaves the heavier
/// chain one block short, so it never activates.
const FORK_BODY_HOLD_LIMIT: usize = 16;

struct HeldForkBody {
    block: Block,
    witnesses: Vec<Vec<blvm_protocol::segwit::Witness>>,
}

#[derive(Default)]
struct ForkBodyHold {
    by_parent: HashMap<blvm_protocol::Hash, Vec<HeldForkBody>>,
    order: std::collections::VecDeque<blvm_protocol::Hash>,
    len: usize,
}

impl ForkBodyHold {
    fn park(
        &mut self,
        parent: blvm_protocol::Hash,
        block: Block,
        witnesses: Vec<Vec<blvm_protocol::segwit::Witness>>,
    ) -> bool {
        let hash = blvm_consensus::block::block_header_hash(&block.header);
        if self.by_parent.values().any(|held| {
            held.iter()
                .any(|item| blvm_consensus::block::block_header_hash(&item.block.header) == hash)
        }) {
            return true;
        }
        if self.len >= FORK_BODY_HOLD_LIMIT {
            self.drop_oldest();
        }
        self.by_parent
            .entry(parent)
            .or_default()
            .push(HeldForkBody { block, witnesses });
        self.order.push_back(hash);
        self.len += 1;
        true
    }

    fn drop_oldest(&mut self) {
        while let Some(hash) = self.order.pop_front() {
            let mut removed = false;
            for children in self.by_parent.values_mut() {
                if let Some(index) = children.iter().position(|item| {
                    blvm_consensus::block::block_header_hash(&item.block.header) == hash
                }) {
                    children.remove(index);
                    self.len = self.len.saturating_sub(1);
                    removed = true;
                    break;
                }
            }
            self.by_parent.retain(|_, children| !children.is_empty());
            if removed {
                return;
            }
        }
    }

    fn take_children(&mut self, parent: &blvm_protocol::Hash) -> Vec<HeldForkBody> {
        let children = self.by_parent.remove(parent).unwrap_or_default();
        if !children.is_empty() {
            let taken: Vec<blvm_protocol::Hash> = children
                .iter()
                .map(|item| blvm_consensus::block::block_header_hash(&item.block.header))
                .collect();
            self.order.retain(|hash| !taken.contains(hash));
            self.len = self.len.saturating_sub(children.len());
        }
        children
    }
}

pub struct SyncCoordinator {
    state_machine: SyncStateMachine,
    block_provider: InMemoryBlockProvider,
    event_publisher: Option<Arc<EventPublisher>>,
    mempool: Option<Arc<crate::node::mempool::MempoolManager>>,
    fork_hold: Arc<Mutex<ForkBodyHold>>,
}

impl Default for SyncCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for SyncCoordinator {
    fn clone(&self) -> Self {
        Self {
            state_machine: SyncStateMachine::new(),
            block_provider: InMemoryBlockProvider::new(),
            event_publisher: self.event_publisher.clone(),
            mempool: self.mempool.clone(),
            fork_hold: Arc::clone(&self.fork_hold),
        }
    }
}

impl SyncCoordinator {
    /// Create a new sync coordinator
    pub fn new() -> Self {
        Self {
            state_machine: SyncStateMachine::new(),
            block_provider: InMemoryBlockProvider::new(),
            event_publisher: None,
            mempool: None,
            fork_hold: Arc::new(Mutex::new(ForkBodyHold::default())),
        }
    }

    /// Attach an event publisher for fork-choice reorg notifications.
    pub fn set_event_publisher(&mut self, publisher: Option<Arc<EventPublisher>>) {
        self.event_publisher = publisher;
    }

    /// Pool updated when a heavier fork becomes the active chain.
    pub fn set_mempool(&mut self, mempool: Option<Arc<crate::node::mempool::MempoolManager>>) {
        self.mempool = mempool;
    }

    /// Mark sync complete when chainstate is already current (no IBD download).
    ///
    /// Does not fetch blocks from peers. Production callers must only use this when the local
    /// tip is known to match the desired chain (e.g. regtest after local mining, restored node).
    pub fn mark_chain_current(&mut self) {
        info!("Marking sync state as current (no block download)");
        self.state_machine.transition_to(SyncState::Synced);
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

        let mut config = ParallelIBDConfig::resolve_for_session(
            ibd_config,
            synced_chain_height,
            &peer_addresses,
        );
        config.network = protocol.get_protocol_version().consensus_network();
        let min_peers = config.min_peers_for_ibd();
        if peer_addresses.len() < min_peers {
            debug!(
                "Not enough peers for parallel IBD (have {}, need {}). Sequential sync is not supported.",
                peer_addresses.len(),
                min_peers
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
                error!(
                    "Parallel IBD failed: {:#}. Sequential sync is not supported - IBD must succeed in parallel mode.",
                    e
                );
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

        // Use wire witnesses when present; otherwise load from blockstore (fail closed post-SegWit).
        let (stored_witnesses, recent_headers) = if witnesses.len() == block.transactions.len() {
            (
                witnesses.clone(),
                mtp_headers_before(blockstore, current_height),
            )
        } else {
            prepare_block_validation_context(
                blockstore,
                &block,
                current_height,
                protocol.get_protocol_version(),
            )?
        };

        let witnesses_to_use = if witnesses.len() == block.transactions.len() {
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
        #[cfg(feature = "production")]
        if let Some(storage) = storage.as_ref() {
            return self.process_block_with_fork_choice(
                blockstore,
                protocol,
                storage,
                &block,
                witnesses_to_use,
                current_height,
                utxo_set,
                metrics,
                start_time,
            );
        }

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
                true,
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
            error!(
                "Block validation failed at height {}: {validation_result:?}",
                current_height
            );
            Ok(false)
        }
    }

    #[cfg(feature = "production")]
    fn read_table_inputs(
        storage: &Storage,
        block: &Block,
        utxo_set: &mut UtxoSet,
    ) -> Result<usize> {
        let mut loaded = 0usize;
        for tx in block.transactions.iter() {
            if blvm_consensus::transaction::is_coinbase(tx) {
                continue;
            }
            for input in tx.inputs.iter() {
                if utxo_set.contains_key(&input.prevout) {
                    continue;
                }
                if let Some(utxo) = storage.utxos().get_utxo(&input.prevout)? {
                    utxo_set.insert(input.prevout, Arc::new(utxo));
                    loaded += 1;
                }
            }
        }
        Ok(loaded)
    }

    #[cfg(feature = "production")]
    fn apply_connect_undo_to_chainstate(
        storage: &Storage,
        undo: &blvm_consensus::reorganization::BlockUndoLog,
    ) -> Result<()> {
        for entry in undo.entries.iter().rev() {
            if entry.previous_utxo.is_some() {
                storage.utxos().remove_utxo(&entry.outpoint)?;
            }
            if let Some(created) = &entry.new_utxo {
                storage
                    .utxos()
                    .add_utxo(&entry.outpoint, created.as_ref())?;
            }
        }
        Ok(())
    }

    #[cfg(feature = "production")]
    fn process_block_with_fork_choice(
        &self,
        blockstore: &BlockStore,
        protocol: &BitcoinProtocolEngine,
        storage: &Arc<Storage>,
        block: &Block,
        witnesses: &[Vec<blvm_protocol::segwit::Witness>],
        _current_height: u64,
        utxo_set: &mut UtxoSet,
        metrics: Option<Arc<MetricsCollector>>,
        start_time: Instant,
    ) -> Result<bool> {
        let mut pending: VecDeque<(Block, Vec<Vec<blvm_protocol::segwit::Witness>>)> =
            VecDeque::from([(block.clone(), witnesses.to_vec())]);
        let mut stored_any = false;
        let mut held_any = false;

        while let Some((block, witnesses)) = pending.pop_front() {
            let parent_hash = block.header.prev_block_hash;
            let (active_tip, active_height) = storage.chain().get_tip_hash_and_height()?;

            // A side block is stored without claiming the active height, so the
            // chain index does not contain it. The body is still the parent.
            // Parking that child drops the heavier chain on the floor.
            let parent_body_height = if parent_hash != active_tip
                && storage.chain().block_index().get(&parent_hash)?.is_none()
                && blockstore.has_block(&parent_hash)?
            {
                blockstore.get_height_by_hash(&parent_hash)?
            } else {
                None
            };
            let connect_height = if parent_hash == active_tip {
                active_height + 1
            } else if let Some(entry) = storage.chain().block_index().get(&parent_hash)? {
                entry.height + 1
            } else if let Some(parent_height) = parent_body_height {
                parent_height.saturating_add(1)
            } else if blockstore.get_header(&parent_hash)?.is_some() {
                let parked = self
                    .fork_hold
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .park(parent_hash, block, witnesses);
                if parked {
                    held_any = true;
                    warn!(
                        "[FORK_HOLD] parent {} is a header only — holding the child until that body is indexed",
                        hex::encode(parent_hash)
                    );
                } else {
                    warn!(
                        "Rejecting block: fork hold is full and parent {:?} is not indexed",
                        parent_hash
                    );
                }
                continue;
            } else {
                warn!(
                    "Rejecting block: parent {:?} not in block index",
                    parent_hash
                );
                continue;
            };

            let extends_active = parent_hash == active_tip;
            let processing_time;
            let block_hash;

            if extends_active {
                // An empty set means the coins are on disk. Filling it with only
                // this block's inputs and then `store_utxo_set` deletes every
                // other row in the chainstate table.
                let from_disk = utxo_set.is_empty();
                let mut utxo_for_connect = std::mem::take(utxo_set);
                let snapshot = if from_disk {
                    storage.open_engine_resume_snapshot()?
                } else {
                    None
                };
                if let Some((name, tree)) = &snapshot {
                    let loaded = read_snapshot_inputs(
                        tree.as_ref(),
                        storage.utxo_value_codec(),
                        std::slice::from_ref(&block),
                        &mut utxo_for_connect,
                    )?;
                    info!("[CONNECT] read {loaded} inputs from UTXO snapshot {name}");
                } else if from_disk {
                    let loaded = Self::read_table_inputs(storage, &block, &mut utxo_for_connect)?;
                    info!("[CONNECT] read {loaded} inputs from chainstate table");
                }
                let (validation_result, connected_utxo, undo_log) =
                    validate_block_with_context_and_undo(
                        blockstore,
                        protocol,
                        &block,
                        &witnesses,
                        &mut utxo_for_connect,
                        connect_height,
                    )?;

                processing_time = start_time.elapsed();
                if !matches!(validation_result, ValidationResult::Valid) {
                    *utxo_set = if from_disk {
                        UtxoSet::default()
                    } else {
                        utxo_for_connect
                    };
                    error!(
                        "Block validation failed at height {}: {validation_result:?}",
                        connect_height
                    );
                    if !stored_any {
                        return Ok(false);
                    }
                    continue;
                }

                store_block_with_context_and_index(
                    blockstore,
                    Some(storage),
                    &block,
                    &witnesses,
                    connect_height,
                    true,
                )?;

                block_hash = blockstore.get_block_hash(&block);
                blockstore.store_undo_log(&block_hash, &undo_log)?;
                storage.chain().cache_block_chainwork(
                    &block_hash,
                    &block.header,
                    connect_height,
                )?;

                if let Some((name, tree)) = &snapshot {
                    // Coins first, then the tip. The journal is in the coin
                    // batch, so a crash here is finished on the next start
                    // instead of validating against coins that are already spent.
                    let mut muhash = storage
                        .chain()
                        .get_ibd_utxo_muhash_running()
                        .ok()
                        .flatten()
                        .map(|bytes| blvm_muhash::MuHash3072::deserialize_running_state(&bytes));
                    let written = commit_utxo_snapshot_delta(
                        tree.as_ref(),
                        storage.utxo_value_codec(),
                        &[],
                        &[&undo_log],
                        Some((connect_height, block_hash)),
                        muhash.as_mut(),
                    )?;
                    storage
                        .chain()
                        .update_tip(&block_hash, &block.header, connect_height)?;
                    note_snapshot_height(storage, connect_height)?;
                    if let Some(mh) = muhash.as_ref() {
                        let bytes = mh.serialize_running_state();
                        storage
                            .chain()
                            .persist_ibd_utxo_muhash_running_only(&bytes)?;
                    }
                    let _ = tree.remove(crate::node::reorg_executor::SNAPSHOT_REORG_JOURNAL_KEY);
                    let _ = tree.remove(crate::node::reorg_executor::SNAPSHOT_MUHASH_KEY);
                    info!("[CONNECT] wrote {written} UTXO rows in snapshot {name}");
                    *utxo_set = UtxoSet::default();
                } else if from_disk {
                    let export_height_set = storage
                        .chain()
                        .get_engine_export_height()
                        .ok()
                        .flatten()
                        .unwrap_or(0)
                        > 0;
                    if crate::node::reorg_executor::disk_utxo_source(false, export_height_set)
                        == crate::node::reorg_executor::DiskUtxoSource::MissingSnapshot
                    {
                        anyhow::bail!("connect refused: IBD UTXO snapshot is empty");
                    }
                    storage
                        .chain()
                        .update_tip(&block_hash, &block.header, connect_height)?;
                    Self::apply_connect_undo_to_chainstate(storage, &undo_log)?;
                    *utxo_set = UtxoSet::default();
                } else {
                    storage
                        .chain()
                        .update_tip(&block_hash, &block.header, connect_height)?;
                    *utxo_set = connected_utxo;
                    storage.utxos().store_utxo_set(utxo_set)?;
                }
            } else {
                let validation_result = validate_block_protocol_only(
                    blockstore,
                    protocol,
                    &block,
                    &witnesses,
                    connect_height,
                    utxo_set,
                )?;

                processing_time = start_time.elapsed();
                if !matches!(validation_result, ValidationResult::Valid) {
                    error!("Block validation failed at height {}", connect_height);
                    if !stored_any {
                        return Ok(false);
                    }
                    continue;
                }

                store_block_with_context_and_index(
                    blockstore,
                    Some(storage),
                    &block,
                    &witnesses,
                    connect_height,
                    false,
                )?;

                block_hash = blockstore.get_block_hash(&block);
                storage.chain().cache_block_chainwork(
                    &block_hash,
                    &block.header,
                    connect_height,
                )?;

                if !try_activate_heavier_fork(
                    storage,
                    blockstore,
                    protocol,
                    &block_hash,
                    utxo_set,
                    self.event_publisher.as_ref(),
                    self.mempool.as_deref(),
                )? {
                    // Side-chain block stored; active tip unchanged.
                }
            }

            stored_any = true;
            if let Some(ref metrics) = metrics {
                metrics.update_storage(|m| {
                    m.block_count += 1;
                    m.transaction_count += block.transactions.len();
                });
                metrics.update_performance(|m| {
                    let time_ms = processing_time.as_secs_f64() * 1000.0;
                    m.avg_block_processing_time_ms =
                        (m.avg_block_processing_time_ms * 0.9) + (time_ms * 0.1);
                    if processing_time.as_secs_f64() > 0.0 {
                        m.blocks_per_second = 1.0 / processing_time.as_secs_f64();
                    }
                });
            }

            info!(
                "Block validated and stored at height {} (took {:?})",
                connect_height, processing_time
            );

            let children = self
                .fork_hold
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take_children(&block_hash);
            for child in children {
                pending.push_back((child.block, child.witnesses));
            }
        }

        Ok(stored_any || held_any)
    }

    /// Connect a block from wire bytes (P2P run loop, regtest mining RPC, integration tests).
    pub fn connect_block_wire(
        &mut self,
        blockstore: &BlockStore,
        protocol: &BitcoinProtocolEngine,
        storage: &Arc<Storage>,
        wire: &[u8],
        connect_height: u64,
        utxo_set: &mut UtxoSet,
    ) -> Result<bool> {
        self.process_block(
            blockstore,
            protocol,
            Some(storage),
            wire,
            connect_height,
            utxo_set,
            None,
            None,
        )
    }

    /// Serialize a mined block with witnesses and connect via [`Self::connect_block_wire`].
    pub fn connect_mined_block(
        &mut self,
        blockstore: &BlockStore,
        protocol: &BitcoinProtocolEngine,
        storage: &Arc<Storage>,
        block: &Block,
        witnesses: &[Vec<blvm_protocol::segwit::Witness>],
        connect_height: u64,
        utxo_set: &mut UtxoSet,
    ) -> Result<bool> {
        let wire =
            blvm_protocol::serialization::serialize_block_with_witnesses(block, witnesses, true);
        self.connect_block_wire(
            blockstore,
            protocol,
            storage,
            &wire,
            connect_height,
            utxo_set,
        )
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

    /// Test mock: approximate header hash from header fields (not consensus hash).
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
/// **Entry point:** No caller in this crate yet. Wire from node orchestration after IBD or on
/// startup when the commitment store needs a network-fetched commitment (e.g. `node/mod.rs`).
#[cfg(feature = "utxo-commitments")]
pub async fn run_utxo_commitments_initial_sync(
    network_manager: std::sync::Arc<tokio::sync::RwLock<crate::network::NetworkManager>>,
    headers: &[blvm_protocol::BlockHeader],
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
    use blvm_protocol::{Block, BlockHeader, Transaction};

    #[test]
    fn test_sync_coordinator_new() {
        let coordinator = SyncCoordinator::new();
        assert_eq!(coordinator.progress(), 0.0);
    }

    #[test]
    fn test_in_memory_block_provider_round_trip() {
        let mut provider = InMemoryBlockProvider::new();
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [1u8; 32],
                timestamp: 1_231_006_505,
                bits: 0x0f00ffff,
                nonce: 0,
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: vec![].into(),
                outputs: vec![].into(),
                lock_time: 0,
            }]
            .into_boxed_slice(),
        };
        provider.store_block(&block).unwrap();
        let hash = provider.calculate_block_hash(&block);
        assert!(provider.get_block(&hash).unwrap().is_some());
        assert_eq!(provider.get_block_count().unwrap(), 1);
    }

    #[test]
    fn test_mock_block_provider_best_header() {
        let mut provider = MockBlockProvider::new();
        let header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [2u8; 32],
            timestamp: 1_231_006_505,
            bits: 0x0f00ffff,
            nonce: 0,
        };
        provider.add_header(header.clone());
        assert_eq!(
            provider.get_best_header().unwrap().unwrap().timestamp,
            header.timestamp
        );
        provider.set_block_count(5);
        assert_eq!(provider.get_block_count().unwrap(), 5);
    }

    fn fork_block(prev: [u8; 32], merkle: u8) -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: prev,
                merkle_root: [merkle; 32],
                timestamp: 1_700_000_000,
                bits: 0x207fffff,
                nonce: u64::from(merkle),
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: vec![].into(),
                outputs: vec![].into(),
                lock_time: 0,
            }]
            .into_boxed_slice(),
        }
    }

    /// The child of a heavier fork can arrive before its parent body is indexed.
    /// It stays held, then comes back in parent-before-child order.
    #[test]
    fn child_body_waits_until_the_parent_body_is_indexed() {
        let parent = [0x11u8; 32];
        let sibling = fork_block(parent, 2);
        let sibling_hash = blvm_consensus::block::block_header_hash(&sibling.header);
        let child = fork_block(sibling_hash, 3);
        let grandchild = fork_block(blvm_consensus::block::block_header_hash(&child.header), 4);

        let mut hold = ForkBodyHold::default();
        assert!(hold.park(sibling_hash, child.clone(), Vec::new()));
        assert!(hold.park(
            blvm_consensus::block::block_header_hash(&child.header),
            grandchild.clone(),
            Vec::new()
        ));
        assert!(
            hold.take_children(&parent).is_empty(),
            "the active parent has no held body"
        );

        let ready = hold.take_children(&sibling_hash);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].block.header.merkle_root, child.header.merkle_root);
        let child_hash = blvm_consensus::block::block_header_hash(&ready[0].block.header);
        let next = hold.take_children(&child_hash);
        assert_eq!(next.len(), 1);
        assert_eq!(
            next[0].block.header.merkle_root,
            grandchild.header.merkle_root
        );
        assert!(hold.take_children(&child_hash).is_empty());

        let mut full = ForkBodyHold::default();
        for i in 0..FORK_BODY_HOLD_LIMIT {
            assert!(full.park(parent, fork_block(parent, i as u8 + 1), Vec::new()));
        }
        assert!(full.park(parent, fork_block(parent, 0xab), Vec::new()));
        assert!(
            full.park(parent, fork_block(parent, 2), Vec::new()),
            "the same child is already held and must not take another slot"
        );
        let held = full.take_children(&parent);
        assert_eq!(held.len(), FORK_BODY_HOLD_LIMIT);
        assert!(
            held.iter()
                .any(|item| item.block.header.merkle_root == [0xab; 32]),
            "the newest child stays"
        );
        assert!(
            held.iter()
                .all(|item| item.block.header.merkle_root != [1; 32]),
            "the oldest child is the one dropped"
        );

        for i in 0..FORK_BODY_HOLD_LIMIT {
            assert!(full.park(parent, fork_block(parent, i as u8 + 1), Vec::new()));
        }
        assert!(full.park(parent, fork_block(parent, 0xcd), Vec::new()));
        let refilled = full.take_children(&parent);
        assert_eq!(
            refilled.len(),
            FORK_BODY_HOLD_LIMIT,
            "taking the held blocks has to free their slots"
        );
    }
}
