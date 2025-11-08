//! Storage layer for reference-node
//!
//! This module provides persistent storage for blocks, UTXO set, and chain state.
//! Supports multiple database backends via feature flags (sled, redb).

pub mod blockstore;
#[cfg(feature = "utxo-commitments")]
pub mod commitment_store;
pub mod chainstate;
pub mod database;
pub mod hashing;
pub mod pruning;
pub mod txindex;
pub mod utxostore;

use anyhow::Result;
use database::{create_database, default_backend, fallback_backend, Database, DatabaseBackend};
use std::path::Path;
use std::sync::Arc;
use tracing::{warn, info};
use crate::config::PruningConfig;

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
                    warn!("Failed to initialize {:?} backend: {}. Falling back to {:?}.", default, e, fallback_backend);
                    info!("Attempting to initialize storage with fallback backend: {:?}", fallback_backend);
                    Self::with_backend(data_dir, fallback_backend)
                } else {
                    Err(anyhow::anyhow!(
                        "Failed to initialize {:?} backend: {}. No fallback backend available.",
                        default, e
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
        let db = Arc::from(create_database(data_dir, backend)?);

        let blockstore = Arc::new(blockstore::BlockStore::new(Arc::clone(&db))?);
        let utxostore = Arc::new(utxostore::UtxoStore::new(Arc::clone(&db))?);
        let chainstate = chainstate::ChainState::new(Arc::clone(&db))?;
        let txindex = Arc::new(txindex::TxIndex::new(Arc::clone(&db))?);

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
    pub fn disk_size(&self) -> Result<u64> {
        // Estimate based on tree sizes (graceful degradation if counts fail)
        let mut size = 0u64;
        
        // Block size estimate (gracefully handle errors)
        if let Ok(count) = self.blockstore.block_count() {
            size += count as u64 * 1024; // Approximate block size
        }
        
        // UTXO size estimate (gracefully handle errors)
        if let Ok(count) = self.utxostore.utxo_count() {
            size += count as u64 * 100; // Approximate UTXO size
        }
        
        // Transaction size estimate (gracefully handle errors)
        if let Ok(count) = self.txindex.transaction_count() {
            size += count as u64 * 500; // Approximate tx size
        }
        
        Ok(size)
    }
    
    /// Get transaction count from txindex
    pub fn transaction_count(&self) -> Result<usize> {
        self.txindex.transaction_count()
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
