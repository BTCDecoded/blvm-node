//! Storage layer for reference-node
//!
//! This module provides persistent storage for blocks, UTXO set, and chain state.
//! Supports multiple database backends via feature flags (sled, redb).

pub mod blockstore;
pub mod chainstate;
#[cfg(kani)]
pub mod chainstate_proofs;
#[cfg(feature = "utxo-commitments")]
pub mod commitment_store;
#[cfg(kani)]
pub mod cryptographic_proofs;
pub mod database;
pub mod hashing;
#[cfg(kani)]
pub mod kani_helpers;
pub mod pruning;
pub mod txindex;
pub mod utxostore;
#[cfg(kani)]
pub mod utxostore_proofs;

use crate::config::PruningConfig;
use anyhow::Result;
use database::{create_database, default_backend, fallback_backend, Database, DatabaseBackend};
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

/// Storage manager that coordinates all storage operations
pub struct Storage {
    db: Arc<dyn Database>,
    blockstore: Arc<blockstore::BlockStore>,
    utxostore: Arc<utxostore::UtxoStore>,
    chainstate: chainstate::ChainState,
    txindex: Arc<txindex::TxIndex>,
    pruning_manager: Option<Arc<pruning::PruningManager>>,
}

impl Storage {
    /// Create a new storage instance with default backend
    ///
    /// Attempts to use the default backend (redb), and gracefully falls back
    /// to sled if redb fails and sled is available.
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Result<Self> {
        let default = default_backend();

        // Try default backend first
        match Self::with_backend(data_dir.as_ref(), default) {
            Ok(storage) => Ok(storage),
            Err(e) => {
                // If default backend fails, try fallback
                if let Some(fallback_backend) = fallback_backend(default) {
                    warn!(
                        "Failed to initialize {:?} backend: {}. Falling back to {:?}.",
                        default, e, fallback_backend
                    );
                    info!(
                        "Attempting to initialize storage with fallback backend: {:?}",
                        fallback_backend
                    );
                    Self::with_backend(data_dir, fallback_backend)
                } else {
                    Err(anyhow::anyhow!(
                        "Failed to initialize {:?} backend: {}. No fallback backend available.",
                        default,
                        e
                    ))
                }
            }
        }
    }

    /// Create a new storage instance with specified backend
    pub fn with_backend<P: AsRef<Path>>(data_dir: P, backend: DatabaseBackend) -> Result<Self> {
        Self::with_backend_and_pruning(data_dir, backend, None)
    }

    /// Create a new storage instance with specified backend and pruning config
    pub fn with_backend_and_pruning<P: AsRef<Path>>(
        data_dir: P,
        backend: DatabaseBackend,
        pruning_config: Option<PruningConfig>,
    ) -> Result<Self> {
        Self::with_backend_pruning_and_indexing(data_dir, backend, pruning_config, None)
    }

    /// Create a new storage instance with backend, pruning, and indexing config
    pub fn with_backend_pruning_and_indexing<P: AsRef<Path>>(
        data_dir: P,
        backend: DatabaseBackend,
        pruning_config: Option<PruningConfig>,
        indexing_config: Option<crate::config::IndexingConfig>,
    ) -> Result<Self> {
        let db = Arc::from(create_database(data_dir, backend)?);

        let blockstore = Arc::new(blockstore::BlockStore::new(Arc::clone(&db))?);
        let utxostore = Arc::new(utxostore::UtxoStore::new(Arc::clone(&db))?);
        let chainstate = chainstate::ChainState::new(Arc::clone(&db))?;

        // Configure transaction indexing based on config
        let txindex = if let Some(indexing) = indexing_config {
            Arc::new(txindex::TxIndex::with_indexing(
                Arc::clone(&db),
                indexing.enable_address_index,
                indexing.enable_value_index,
            )?)
        } else {
            Arc::new(txindex::TxIndex::new(Arc::clone(&db))?)
        };

        let pruning_manager = pruning_config.map(|config| {
            #[cfg(feature = "utxo-commitments")]
            {
                // Check if aggressive mode requires UTXO commitments
                let needs_commitments = matches!(config.mode, crate::config::PruningMode::Aggressive { keep_commitments: true, .. })
                    || matches!(config.mode, crate::config::PruningMode::Custom { keep_commitments: true, .. });
                if needs_commitments {
                    let commitment_store = match commitment_store::CommitmentStore::new(Arc::clone(&db)) {
                        Ok(store) => Arc::new(store),
                        Err(e) => {
                            warn!("Failed to create commitment store: {}. Pruning will continue without commitments.", e);
                            return Arc::new(pruning::PruningManager::new(config, Arc::clone(&blockstore)));
                        }
                    };
                    Arc::new(pruning::PruningManager::with_utxo_commitments(
                        config,
                        Arc::clone(&blockstore),
                        commitment_store,
                        Arc::clone(&utxostore),
                    ))
                } else {
                    Arc::new(pruning::PruningManager::new(config, Arc::clone(&blockstore)))
                }
            }
            #[cfg(not(feature = "utxo-commitments"))]
            {
                Arc::new(pruning::PruningManager::new(config, Arc::clone(&blockstore)))
            }
        });

        Ok(Self {
            db,
            blockstore,
            utxostore,
            chainstate,
            txindex,
            pruning_manager,
        })
    }

    /// Get the block store (as Arc for sharing)
    pub fn blocks(&self) -> Arc<blockstore::BlockStore> {
        Arc::clone(&self.blockstore)
    }

    /// Get the UTXO store
    pub fn utxos(&self) -> &utxostore::UtxoStore {
        &self.utxostore
    }

    /// Get the UTXO store as Arc (for sharing)
    pub fn utxos_arc(&self) -> Arc<utxostore::UtxoStore> {
        Arc::clone(&self.utxostore)
    }

    /// Get the chain state
    pub fn chain(&self) -> &chainstate::ChainState {
        &self.chainstate
    }

    /// Get the transaction index (as Arc for sharing)
    pub fn transactions(&self) -> Arc<txindex::TxIndex> {
        Arc::clone(&self.txindex)
    }

    /// Flush all pending writes to disk
    pub fn flush(&self) -> Result<()> {
        self.db.flush()
    }

    /// Get approximate disk size used by storage (in bytes)
    ///
    /// Returns an estimate based on tree sizes. If any operation fails,
    /// returns 0 gracefully rather than erroring.
    /// Includes bounds checking to prevent overflow.
    pub fn disk_size(&self) -> Result<u64> {
        // Estimate based on tree sizes (graceful degradation if counts fail)
        let mut size = 0u64;

        // Block size estimate (gracefully handle errors, with bounds checking)
        if let Ok(count) = self.blockstore.block_count() {
            const MAX_BLOCKS: u64 = 10_000_000; // 10M blocks max (safety limit)
            let safe_count = count.min(MAX_BLOCKS as usize) as u64;
            const BYTES_PER_BLOCK: u64 = 1_024_000; // ~1MB per block
            size = size.saturating_add(safe_count.saturating_mul(BYTES_PER_BLOCK));
        }

        // UTXO size estimate (gracefully handle errors, with bounds checking)
        if let Ok(count) = self.utxostore.utxo_count() {
            const MAX_UTXOS: u64 = 1_000_000_000; // 1B UTXOs max (safety limit)
            let safe_count = count.min(MAX_UTXOS as usize) as u64;
            const BYTES_PER_UTXO: u64 = 100; // ~100 bytes per UTXO
            size = size.saturating_add(safe_count.saturating_mul(BYTES_PER_UTXO));
        }

        // Transaction size estimate (gracefully handle errors, with bounds checking)
        if let Ok(count) = self.txindex.transaction_count() {
            const MAX_TXS: u64 = 1_000_000_000; // 1B transactions max (safety limit)
            let safe_count = count.min(MAX_TXS as usize) as u64;
            const BYTES_PER_TX: u64 = 500; // ~500 bytes per transaction
            size = size.saturating_add(safe_count.saturating_mul(BYTES_PER_TX));
        }

        // Final bounds check: prevent returning unrealistic values
        const MAX_DISK_SIZE: u64 = 10_000_000_000_000; // 10TB max (safety limit)
        Ok(size.min(MAX_DISK_SIZE))
    }

    /// Check storage bounds before operations
    /// Returns true if storage is within safe bounds, false if approaching limits
    pub fn check_storage_bounds(&self) -> Result<bool> {
        const MAX_BLOCKS: usize = 10_000_000; // 10M blocks
        const MAX_UTXOS: usize = 1_000_000_000; // 1B UTXOs
        const MAX_TXS: usize = 1_000_000_000; // 1B transactions

        let block_count = self.blockstore.block_count().unwrap_or(0);
        let utxo_count = self.utxostore.utxo_count().unwrap_or(0);
        let tx_count = self.txindex.transaction_count().unwrap_or(0);

        // Check if we're approaching limits (80% threshold)
        let blocks_ok = block_count < (MAX_BLOCKS * 8 / 10);
        let utxos_ok = utxo_count < (MAX_UTXOS * 8 / 10);
        let txs_ok = tx_count < (MAX_TXS * 8 / 10);

        if !blocks_ok {
            warn!(
                "Storage bounds: block count ({}) approaching limit ({})",
                block_count, MAX_BLOCKS
            );
        }
        if !utxos_ok {
            warn!(
                "Storage bounds: UTXO count ({}) approaching limit ({})",
                utxo_count, MAX_UTXOS
            );
        }
        if !txs_ok {
            warn!(
                "Storage bounds: transaction count ({}) approaching limit ({})",
                tx_count, MAX_TXS
            );
        }

        Ok(blocks_ok && utxos_ok && txs_ok)
    }

    /// Get transaction count from txindex
    pub fn transaction_count(&self) -> Result<usize> {
        self.txindex.transaction_count()
    }

    /// Index a block's transactions (optimized batch indexing)
    /// This should be called after a block is stored to index all its transactions
    pub fn index_block(
        &self,
        block: &bllvm_protocol::Block,
        block_hash: &bllvm_protocol::Hash,
        block_height: u64,
    ) -> Result<()> {
        self.txindex.index_block(block, block_hash, block_height)
    }

    /// Get pruning manager (if pruning is configured)
    pub fn pruning(&self) -> Option<Arc<pruning::PruningManager>> {
        self.pruning_manager.as_ref().map(Arc::clone)
    }

    /// Check if pruning is enabled
    pub fn is_pruning_enabled(&self) -> bool {
        self.pruning_manager
            .as_ref()
            .map(|pm| pm.is_enabled())
            .unwrap_or(false)
    }
}
