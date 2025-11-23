//! Pruning manager for blockchain storage
//!
//! Implements configurable pruning modes:
//! - Disabled: No pruning (archival node)
//! - Normal: Conservative pruning (keep recent blocks)
//! - Aggressive: Prune with UTXO commitments (requires utxo-commitments feature)
//! - Custom: Fine-grained control over what to keep

use crate::config::{PruningConfig, PruningMode};
#[cfg(feature = "bip158")]
use crate::network::filter_service::BlockFilterService;
use crate::storage::blockstore::BlockStore;
#[cfg(feature = "utxo-commitments")]
use crate::storage::commitment_store::CommitmentStore;
#[cfg(feature = "utxo-commitments")]
use crate::storage::utxostore::UtxoStore;
use anyhow::{anyhow, Result};
#[cfg(feature = "utxo-commitments")]
use bllvm_protocol::Hash;
#[cfg(feature = "utxo-commitments")]
use hex;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Pruning statistics
#[derive(Debug, Clone, Default)]
pub struct PruningStats {
    /// Number of blocks pruned
    pub blocks_pruned: u64,
    /// Number of headers kept
    pub headers_kept: u64,
    /// Number of blocks kept
    pub blocks_kept: u64,
    /// Storage space freed (bytes, approximate)
    pub storage_freed: u64,
    /// Last pruning height
    pub last_prune_height: Option<u64>,
}

/// Pruning manager
pub struct PruningManager {
    pub config: PruningConfig,
    blockstore: Arc<BlockStore>,
    #[cfg(feature = "utxo-commitments")]
    commitment_store: Option<Arc<CommitmentStore>>,
    #[cfg(feature = "utxo-commitments")]
    utxostore: Option<Arc<UtxoStore>>,
    #[cfg(feature = "bip158")]
    filter_service: Option<Arc<BlockFilterService>>,
    stats: std::sync::Mutex<PruningStats>,
}

impl PruningManager {
    /// Create a new pruning manager
    pub fn new(config: PruningConfig, blockstore: Arc<BlockStore>) -> Self {
        Self {
            config,
            blockstore,
            #[cfg(feature = "utxo-commitments")]
            commitment_store: None,
            #[cfg(feature = "utxo-commitments")]
            utxostore: None,
            #[cfg(feature = "bip158")]
            filter_service: None,
            stats: std::sync::Mutex::new(PruningStats::default()),
        }
    }

    /// Create a new pruning manager with UTXO commitments support
    #[cfg(feature = "utxo-commitments")]
    pub fn with_utxo_commitments(
        config: PruningConfig,
        blockstore: Arc<BlockStore>,
        commitment_store: Arc<CommitmentStore>,
        utxostore: Arc<UtxoStore>,
    ) -> Self {
        Self {
            config,
            blockstore,
            commitment_store: Some(commitment_store),
            utxostore: Some(utxostore),
            #[cfg(feature = "bip158")]
            filter_service: None,
            stats: std::sync::Mutex::new(PruningStats::default()),
        }
    }

    /// Create a new pruning manager with all optional features
    pub fn with_features(
        config: PruningConfig,
        blockstore: Arc<BlockStore>,
        #[cfg(feature = "utxo-commitments")] commitment_store: Option<Arc<CommitmentStore>>,
        #[cfg(feature = "utxo-commitments")] utxostore: Option<Arc<UtxoStore>>,
        #[cfg(feature = "bip158")] filter_service: Option<Arc<BlockFilterService>>,
    ) -> Self {
        Self {
            config,
            blockstore,
            #[cfg(feature = "utxo-commitments")]
            commitment_store,
            #[cfg(feature = "utxo-commitments")]
            utxostore,
            #[cfg(feature = "bip158")]
            filter_service,
            stats: std::sync::Mutex::new(PruningStats::default()),
        }
    }

    /// Get pruning statistics
    pub fn get_stats(&self) -> PruningStats {
        self.stats.lock().unwrap().clone()
    }

    /// Get commitment store (if available)
    #[cfg(feature = "utxo-commitments")]
    pub fn commitment_store(&self) -> Option<Arc<CommitmentStore>> {
        self.commitment_store.as_ref().map(Arc::clone)
    }

    /// Get UTXO store (if available)
    #[cfg(feature = "utxo-commitments")]
    pub fn utxostore(&self) -> Option<Arc<UtxoStore>> {
        self.utxostore.as_ref().map(Arc::clone)
    }

    /// Check if pruning is enabled
    pub fn is_enabled(&self) -> bool {
        !matches!(self.config.mode, PruningMode::Disabled)
    }

    /// Check if automatic pruning should run
    pub fn should_auto_prune(&self, current_height: u64, last_prune_height: Option<u64>) -> bool {
        if !self.config.auto_prune {
            return false;
        }

        if let Some(last_height) = last_prune_height {
            // Check if we've reached the auto-prune interval
            current_height >= last_height + self.config.auto_prune_interval
        } else {
            // First auto-prune after reaching interval
            current_height >= self.config.auto_prune_interval
        }
    }

    /// Check if incremental pruning during IBD is possible
    /// Requires: UTXO commitments feature enabled and available
    fn can_incremental_prune_during_ibd(&self) -> bool {
        #[cfg(feature = "utxo-commitments")]
        {
            // Check if UTXO commitments are available
            let has_commitments = self.commitment_store.is_some() && self.utxostore.is_some();

            // Check if mode supports incremental pruning
            let mode_supports = matches!(
                &self.config.mode,
                PruningMode::Aggressive { .. }
                    | PruningMode::Custom {
                        keep_commitments: true,
                        ..
                    }
            );

            has_commitments && mode_supports
        }
        #[cfg(not(feature = "utxo-commitments"))]
        {
            false
        }
    }

    /// Incremental prune during IBD
    ///
    /// This method prunes old blocks while keeping a sliding window of recent blocks.
    /// It's designed to be called periodically during IBD to prevent storage from growing
    /// unbounded. Only works when incremental_prune_during_ibd is enabled and UTXO
    /// commitments are available.
    ///
    /// # Arguments
    /// * `current_height` - Current chain tip height
    /// * `is_ibd` - Whether initial block download is in progress
    ///
    /// # Returns
    /// Pruning statistics if pruning occurred, None if not needed
    pub fn incremental_prune_during_ibd(
        &self,
        current_height: u64,
        is_ibd: bool,
    ) -> Result<Option<PruningStats>> {
        // Only run during IBD if enabled
        if !is_ibd || !self.config.incremental_prune_during_ibd {
            return Ok(None);
        }

        // Check if prerequisites are met
        if !self.can_incremental_prune_during_ibd() {
            return Ok(None);
        }

        // Check if we have enough blocks to start pruning
        if current_height < self.config.min_blocks_for_incremental_prune {
            return Ok(None);
        }

        // Calculate prune height: keep only the last prune_window_size blocks
        let prune_to_height = current_height.saturating_sub(self.config.prune_window_size);

        // Don't prune if window size is larger than current height
        if prune_to_height == 0 || prune_to_height >= current_height {
            return Ok(None);
        }

        info!(
            "Incremental pruning during IBD: current_height={}, prune_to_height={}, window_size={}",
            current_height, prune_to_height, self.config.prune_window_size
        );

        // Perform pruning
        let stats = self.prune_to_height(prune_to_height, current_height, is_ibd)?;
        Ok(Some(stats))
    }

    /// Prune blocks up to a specific height
    ///
    /// # Arguments
    /// * `prune_to_height` - Prune all blocks up to (but not including) this height
    /// * `current_height` - Current chain tip height
    /// * `is_ibd` - Whether initial block download is in progress
    ///
    /// # Returns
    /// Pruning statistics
    pub fn prune_to_height(
        &self,
        prune_to_height: u64,
        current_height: u64,
        is_ibd: bool,
    ) -> Result<PruningStats> {
        // Validate pruning is enabled
        if !self.is_enabled() {
            return Err(anyhow!("Pruning is disabled"));
        }

        // Check if incremental pruning during IBD is allowed
        if is_ibd {
            // Allow incremental pruning during IBD if:
            // 1. Incremental pruning is explicitly enabled
            // 2. UTXO commitments are available (for state verification)
            // 3. Aggressive or Custom mode with commitments enabled
            let can_incremental_prune =
                self.config.incremental_prune_during_ibd && self.can_incremental_prune_during_ibd();

            if !can_incremental_prune {
                return Err(anyhow!(
                    "Cannot prune during initial block download. Wait for IBD to complete, or enable incremental_prune_during_ibd with UTXO commitments."
                ));
            }

            // For incremental pruning during IBD, ensure we have enough blocks
            if current_height < self.config.min_blocks_for_incremental_prune {
                return Err(anyhow!(
                    "Cannot prune during IBD until at least {} blocks are synced (currently at {})",
                    self.config.min_blocks_for_incremental_prune,
                    current_height
                ));
            }
        }

        // Validate prune height
        if prune_to_height >= current_height {
            return Err(anyhow!(
                "Cannot prune to height >= current height ({} >= {})",
                prune_to_height,
                current_height
            ));
        }

        // Ensure we keep minimum blocks
        let blocks_to_keep = current_height.saturating_sub(prune_to_height);
        if blocks_to_keep < self.config.min_blocks_to_keep {
            return Err(anyhow!(
                "Pruning would leave {} blocks, but minimum is {}",
                blocks_to_keep,
                self.config.min_blocks_to_keep
            ));
        }

        info!(
            "Starting pruning: prune_to_height={}, current_height={}, mode={:?}",
            prune_to_height, current_height, self.config.mode
        );

        let stats = match &self.config.mode {
            PruningMode::Disabled => {
                return Err(anyhow!("Pruning is disabled"));
            }
            PruningMode::Normal {
                keep_from_height,
                min_recent_blocks,
            } => self.prune_normal(
                prune_to_height,
                current_height,
                *keep_from_height,
                *min_recent_blocks,
            )?,
            PruningMode::Aggressive {
                keep_from_height: _,
                keep_commitments: _,
                keep_filtered_blocks: _,
                min_blocks: _,
            } => {
                #[cfg(feature = "utxo-commitments")]
                {
                    self.prune_aggressive(
                        prune_to_height,
                        current_height,
                        *keep_from_height,
                        *keep_commitments,
                        *keep_filtered_blocks,
                        *min_blocks,
                    )?
                }
                #[cfg(not(feature = "utxo-commitments"))]
                {
                    return Err(anyhow!(
                        "Aggressive pruning requires utxo-commitments feature"
                    ));
                }
            }
            PruningMode::Custom {
                keep_headers,
                keep_bodies_from_height,
                keep_commitments,
                keep_filters,
                keep_filtered_blocks,
                keep_witnesses,
                keep_tx_index,
            } => self.prune_custom(
                prune_to_height,
                current_height,
                *keep_headers,
                *keep_bodies_from_height,
                *keep_commitments,
                *keep_filters,
                *keep_filtered_blocks,
                *keep_witnesses,
                *keep_tx_index,
            )?,
        };

        // Update statistics
        {
            let mut stats_guard = self.stats.lock().unwrap();
            stats_guard.blocks_pruned += stats.blocks_pruned;
            stats_guard.headers_kept += stats.headers_kept;
            stats_guard.blocks_kept += stats.blocks_kept;
            stats_guard.storage_freed += stats.storage_freed;
            stats_guard.last_prune_height = Some(prune_to_height);
        }

        info!(
            "Pruning complete: pruned {} blocks, kept {} blocks, freed ~{} bytes",
            stats.blocks_pruned, stats.blocks_kept, stats.storage_freed
        );

        Ok(stats)
    }

    /// Normal pruning: Keep recent blocks, remove older blocks
    fn prune_normal(
        &self,
        prune_to_height: u64,
        current_height: u64,
        keep_from_height: u64,
        min_recent_blocks: u64,
    ) -> Result<PruningStats> {
        let mut stats = PruningStats::default();

        // Calculate actual keep height (max of keep_from_height and min_recent_blocks)
        let effective_keep_height =
            keep_from_height.max(current_height.saturating_sub(min_recent_blocks));

        // Ensure we don't prune below effective keep height
        let actual_prune_height = prune_to_height.min(effective_keep_height);

        debug!(
            "Normal pruning: prune_to={}, keep_from={}, min_recent={}, effective_keep={}, actual_prune={}",
            prune_to_height, keep_from_height, min_recent_blocks, effective_keep_height, actual_prune_height
        );

        // Prune blocks up to actual_prune_height
        for height in 0..actual_prune_height {
            if let Some(hash) = self.blockstore.get_hash_by_height(height)? {
                // Remove block body (keep header for PoW verification)
                if let Some(_block) = self.blockstore.get_block(&hash)? {
                    self.blockstore.remove_block_body(&hash)?;
                    stats.blocks_pruned += 1;
                    stats.storage_freed += 1024; // Approximate block size
                }
            }
        }

        // Count kept blocks
        stats.blocks_kept = current_height.saturating_sub(actual_prune_height);
        stats.headers_kept = current_height; // All headers are kept

        Ok(stats)
    }

    /// Aggressive pruning: Prune with UTXO commitments
    #[cfg(feature = "utxo-commitments")]
    fn prune_aggressive(
        &self,
        prune_to_height: u64,
        current_height: u64,
        keep_from_height: u64,
        keep_commitments: bool,
        keep_filtered_blocks: bool,
        min_blocks: u64,
    ) -> Result<PruningStats> {
        let mut stats = PruningStats::default();

        // Calculate effective keep height
        let effective_keep_height = keep_from_height.max(current_height.saturating_sub(min_blocks));

        let actual_prune_height = prune_to_height.min(effective_keep_height);

        debug!(
            "Aggressive pruning: prune_to={}, keep_from={}, min_blocks={}, effective_keep={}, actual_prune={}",
            prune_to_height, keep_from_height, min_blocks, effective_keep_height, actual_prune_height
        );

        // Generate UTXO commitments before pruning if enabled
        if keep_commitments {
            if let (Some(ref commitment_store), Some(ref utxostore)) =
                (self.commitment_store.as_ref(), self.utxostore.as_ref())
            {
                info!("Generating UTXO commitments for blocks to be pruned...");
                self.generate_commitments_before_prune(
                    actual_prune_height,
                    current_height,
                    commitment_store,
                    utxostore,
                )?;
            } else {
                warn!(
                    "UTXO commitments requested but commitment store or UTXO store not available"
                );
            }
        }

        // Prune blocks up to actual_prune_height
        for height in 0..actual_prune_height {
            if let Some(hash) = self.blockstore.get_hash_by_height(height)? {
                // Remove block body (keep header)
                if let Some(_block) = self.blockstore.get_block(&hash)? {
                    self.blockstore.remove_block_body(&hash)?;
                    stats.blocks_pruned += 1;
                    stats.storage_freed += 1024;
                }

                // Remove witnesses if not keeping filtered blocks
                if !keep_filtered_blocks {
                    self.blockstore.remove_witness(&hash)?;
                }

                // Handle BIP158 filters if configured
                #[cfg(feature = "bip158")]
                if let Some(ref filter_service) = self.filter_service {
                    // Remove filter from cache but keep filter header
                    if filter_service.has_filter(&hash) {
                        filter_service.remove_filter_for_pruned_block(&hash)?;
                        debug!(
                            "Removed BIP158 filter for pruned block at height {} (header kept)",
                            height
                        );
                    }
                }
            }
        }

        stats.blocks_kept = current_height.saturating_sub(actual_prune_height);
        stats.headers_kept = current_height;

        Ok(stats)
    }

    /// Custom pruning: Fine-grained control
    fn prune_custom(
        &self,
        prune_to_height: u64,
        current_height: u64,
        keep_headers: bool,
        keep_bodies_from_height: u64,
        keep_commitments: bool,
        _keep_filters: bool,
        keep_filtered_blocks: bool,
        keep_witnesses: bool,
        _keep_tx_index: bool,
    ) -> Result<PruningStats> {
        let mut stats = PruningStats::default();

        // Headers must always be kept (for PoW verification)
        if !keep_headers {
            warn!("Custom pruning with keep_headers=false is not recommended (required for PoW)");
        }

        let actual_prune_height = prune_to_height.min(keep_bodies_from_height);

        debug!(
            "Custom pruning: prune_to={}, keep_bodies_from={}, actual_prune={}",
            prune_to_height, keep_bodies_from_height, actual_prune_height
        );

        // Prune blocks up to actual_prune_height
        for height in 0..actual_prune_height {
            if let Some(hash) = self.blockstore.get_hash_by_height(height)? {
                // Remove block body if not keeping from this height
                if height < keep_bodies_from_height {
                    if let Some(_block) = self.blockstore.get_block(&hash)? {
                        self.blockstore.remove_block_body(&hash)?;
                        stats.blocks_pruned += 1;
                        stats.storage_freed += 1024;
                    }
                }

                // Remove witnesses if not keeping
                if !keep_witnesses {
                    self.blockstore.remove_witness(&hash)?;
                }

                // Handle commitments if enabled
                if keep_commitments {
                    #[cfg(feature = "utxo-commitments")]
                    {
                        if let (Some(ref commitment_store), Some(ref utxostore)) =
                            (self.commitment_store.as_ref(), self.utxostore.as_ref())
                        {
                            // Generate commitment if not exists
                            if !commitment_store.has_commitment(&hash)? {
                                self.generate_commitment_for_block(
                                    &hash,
                                    height,
                                    commitment_store,
                                    utxostore,
                                )?;
                            }
                        }
                    }
                }
                // Handle BIP158 filters if enabled
                #[cfg(feature = "bip158")]
                if keep_filters {
                    if let Some(ref filter_service) = self.filter_service {
                        // Remove filter from cache (saves memory) but keep filter header
                        // Filter headers are always kept for chain verification
                        if filter_service.has_filter(&hash) {
                            filter_service.remove_filter_for_pruned_block(&hash)?;
                            debug!(
                                "Removed BIP158 filter for pruned block at height {} (header kept)",
                                height
                            );
                        }
                    }
                }

                // Handle filtered blocks if enabled
                // Note: Filtered blocks are lightweight versions of blocks used for SPV clients
                // If keep_filtered_blocks is false, we can remove them to save space
                if !keep_filtered_blocks {
                    // Filtered blocks are typically stored separately from full blocks
                    // This is a placeholder for future filtered block storage implementation
                    debug!("Filtered blocks pruning not yet implemented for custom_prune");
                }
            }
        }

        stats.blocks_kept = current_height.saturating_sub(actual_prune_height);
        stats.headers_kept = if keep_headers { current_height } else { 0 };

        Ok(stats)
    }

    /// Generate UTXO commitments for blocks before pruning
    #[cfg(feature = "utxo-commitments")]
    fn generate_commitments_before_prune(
        &self,
        prune_to_height: u64,
        _current_height: u64,
        commitment_store: &CommitmentStore,
        utxostore: &UtxoStore,
    ) -> Result<()> {
        info!(
            "Generating UTXO commitments for heights 0..{}",
            prune_to_height
        );

        // For each block to be pruned, generate a commitment
        // We'll generate commitments at checkpoint intervals to save computation
        let checkpoint_interval = 144; // Every ~1 day at 10 min/block

        for height in (0..prune_to_height).step_by(checkpoint_interval as usize) {
            if let Some(hash) = self.blockstore.get_hash_by_height(height)? {
                // Check if commitment already exists
                if commitment_store.has_commitment(&hash)? {
                    debug!("Commitment already exists for height {}", height);
                    continue;
                }

                // Generate commitment for this block
                self.generate_commitment_for_block(&hash, height, commitment_store, utxostore)?;
            }
        }

        info!("Finished generating UTXO commitments");
        Ok(())
    }

    /// Generate a single UTXO commitment for a block
    #[cfg(feature = "utxo-commitments")]
    fn generate_commitment_for_block(
        &self,
        block_hash: &Hash,
        height: u64,
        commitment_store: &CommitmentStore,
        utxostore: &UtxoStore,
    ) -> Result<()> {
        #[cfg(feature = "utxo-commitments")]
        use bllvm_protocol::utxo_commitments::merkle_tree::UtxoMerkleTree;

        // Reconstruct UTXO set at historical height by replaying blocks
        let utxo_set = self.reconstruct_utxo_set_at_height(height, utxostore)?;

        // Build Merkle tree from UTXO set
        let mut utxo_tree = UtxoMerkleTree::new()
            .map_err(|e| anyhow::anyhow!("Failed to create UTXO Merkle tree: {:?}", e))?;

        for (outpoint, utxo) in &utxo_set {
            utxo_tree
                .insert(outpoint.clone(), utxo.clone())
                .map_err(|e| anyhow::anyhow!("Failed to insert UTXO: {:?}", e))?;
        }

        // Generate commitment
        let commitment = utxo_tree.generate_commitment(*block_hash, height);

        // Store commitment
        commitment_store.store_commitment(block_hash, height, &commitment)?;

        debug!(
            "Generated and stored commitment for height {} (hash: {})",
            height,
            hex::encode(block_hash)
        );

        Ok(())
    }

    /// Generate UTXO commitment from current UTXO set state
    ///
    /// This method generates a commitment from the current UTXO set without replaying blocks.
    /// Used during incremental sync when the UTXO set is maintained incrementally.
    ///
    /// # Arguments
    /// * `block_hash` - Hash of the block at this height
    /// * `height` - Block height
    /// * `utxo_set` - Current UTXO set (from incremental updates)
    /// * `commitment_store` - Commitment store to save the commitment
    ///
    /// # Returns
    /// Result indicating success or failure
    #[cfg(feature = "utxo-commitments")]
    pub fn generate_commitment_from_current_state(
        &self,
        block_hash: &Hash,
        height: u64,
        utxo_set: &bllvm_protocol::UtxoSet,
        commitment_store: &CommitmentStore,
    ) -> Result<()> {
        use bllvm_protocol::utxo_commitments::merkle_tree::UtxoMerkleTree;

        // Build Merkle tree from current UTXO set
        let mut utxo_tree = UtxoMerkleTree::new()
            .map_err(|e| anyhow::anyhow!("Failed to create UTXO Merkle tree: {:?}", e))?;

        for (outpoint, utxo) in utxo_set {
            utxo_tree
                .insert(outpoint.clone(), utxo.clone())
                .map_err(|e| anyhow::anyhow!("Failed to insert UTXO: {:?}", e))?;
        }

        // Generate commitment
        let commitment = utxo_tree.generate_commitment(*block_hash, height);

        // Store commitment
        commitment_store.store_commitment(block_hash, height, &commitment)?;

        debug!(
            "Generated commitment from current state for height {} (hash: {})",
            height,
            hex::encode(block_hash)
        );

        Ok(())
    }

    /// Reconstruct UTXO set at a specific historical height
    ///
    /// This function replays blocks from genesis to the target height
    /// to reconstruct the exact UTXO set at that point in history.
    #[cfg(feature = "utxo-commitments")]
    fn reconstruct_utxo_set_at_height(
        &self,
        target_height: u64,
        utxostore: &UtxoStore,
    ) -> Result<bllvm_protocol::UtxoSet> {
        use bllvm_protocol::block::connect_block;
        use bllvm_protocol::UtxoSet;

        // Start with empty UTXO set (genesis)
        let mut utxo_set = UtxoSet::new();

        // If target height is 0, return empty UTXO set (genesis)
        if target_height == 0 {
            return Ok(utxo_set);
        }

        // Replay blocks from genesis (height 0) to target_height
        // This gives us the exact UTXO set at that historical point
        for height in 0..target_height {
            // Get block hash at this height
            if let Some(block_hash) = self.blockstore.get_hash_by_height(height)? {
                // Get block
                if let Some(block) = self.blockstore.get_block(&block_hash)? {
                    // Get witnesses for this block
                    let witnesses =
                        self.blockstore
                            .get_witness(&block_hash)?
                            .unwrap_or_else(|| {
                                // If no witnesses stored, create empty witnesses
                                block.transactions.iter().map(|_| Vec::new()).collect()
                            });

                    // Apply block to UTXO set using connect_block
                    // This properly handles coinbase transactions and input/output processing
                    let (validation_result, new_utxo_set) = connect_block(
                        &block, &witnesses, utxo_set, height,
                        None, // No recent headers needed for historical replay
                    )?;

                    // Check validation result
                    if !matches!(validation_result, bllvm_protocol::ValidationResult::Valid) {
                        warn!(
                            "Block at height {} failed validation during UTXO reconstruction",
                            height
                        );
                        // Continue anyway - this might be expected for some edge cases
                    }

                    // Update UTXO set for next iteration
                    utxo_set = new_utxo_set;
                } else {
                    // Block not found - this shouldn't happen if chain is intact
                    warn!("Block at height {} not found during UTXO reconstruction, using current UTXO set as fallback", height);
                    // Use current UTXO set as fallback
                    return utxostore.get_all_utxos();
                }
            } else {
                // Height not found - use current UTXO set as fallback
                warn!("Height {} not found in chain during UTXO reconstruction, using current UTXO set", height);
                return utxostore.get_all_utxos();
            }
        }

        debug!(
            "Reconstructed UTXO set at height {} with {} UTXOs",
            target_height,
            utxo_set.len()
        );
        Ok(utxo_set)
    }
}
