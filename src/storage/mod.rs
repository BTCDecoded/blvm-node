//! Storage layer for blvm-node
//!
//! This module provides persistent storage for blocks, UTXO set, and chain state.
//! Supports multiple database backends via feature flags (tidesdb, redb, sled, rocksdb).

pub mod assumeutxo;
pub mod bitcoin_core_blocks;
pub mod bitcoin_core_detection;
pub mod bitcoin_core_format;
pub mod bitcoin_core_storage;
pub mod blockstore;
pub mod buffered_store;
pub mod chainstate;
#[cfg(feature = "utxo-commitments")]
pub mod commitment_store;
pub mod database;
pub mod disk_utxo;
pub mod hashing;
#[cfg(feature = "production")]
pub mod ibd_utxo_store;
pub mod pruning;
pub mod serialization_cache;
pub mod txindex;
pub mod utxostore;
pub mod wal;

use crate::config::PruningConfig;
use anyhow::Result;
use database::{create_database, default_backend, fallback_backend, Database, DatabaseBackend};
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

#[cfg(feature = "rocksdb")]
use bitcoin_core_detection::BitcoinCoreDetection;
#[cfg(feature = "rocksdb")]
use bitcoin_core_detection::BitcoinCoreNetwork;
#[cfg(feature = "rocksdb")]
use bitcoin_core_storage::BitcoinCoreStorage;

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
    /// Attempts to use the default backend (TidesDB if available, else Redb), and gracefully
    /// falls back to alternatives if the primary fails.
    ///
    /// If existing node data is detected, will use RocksDB to read it.
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Result<Self> {
        // Check for existing node data first (if RocksDB is available)
        #[cfg(feature = "rocksdb")]
        {
            use bitcoin_core_detection::BitcoinCoreNetwork;
            use bitcoin_core_storage::BitcoinCoreStorage;

            // Try to detect existing mainnet data
            if let Ok(Some(backend)) =
                BitcoinCoreStorage::detect_and_open(data_dir.as_ref(), BitcoinCoreNetwork::Mainnet)
            {
                if backend == DatabaseBackend::RocksDB {
                    info!("Existing node data detected, opening with RocksDB backend");
                    // Open existing database directly and initialize storage
                    let db = Arc::from(BitcoinCoreStorage::open_bitcoin_core_database(
                        data_dir.as_ref(),
                        BitcoinCoreNetwork::Mainnet,
                    )?);

                    // Create block file reader if blocks directory exists
                    // Use cache directory for index persistence
                    let block_reader = if let Some(core_dir) =
                        BitcoinCoreDetection::detect_data_dir(BitcoinCoreNetwork::Mainnet)?
                    {
                        let blocks_dir = core_dir.join("blocks");
                        if blocks_dir.exists() {
                            // Use data_dir for index cache
                            match bitcoin_core_blocks::BitcoinCoreBlockReader::new_with_cache(
                                &blocks_dir,
                                BitcoinCoreNetwork::Mainnet,
                                Some(data_dir.as_ref()),
                            ) {
                                Ok(reader) => {
                                    info!("Block files detected, enabling block file reader with index cache");
                                    Some(Arc::new(reader))
                                }
                                Err(e) => {
                                    warn!("Failed to initialize block file reader: {}", e);
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Initialize storage components with the opened database and block reader
                    let blockstore =
                        Arc::new(blockstore::BlockStore::new_with_bitcoin_core_reader(
                            Arc::clone(&db),
                            block_reader,
                        )?);
                    let utxostore = Arc::new(utxostore::UtxoStore::new(Arc::clone(&db))?);
                    let chainstate = chainstate::ChainState::new(Arc::clone(&db))?;
                    let txindex = Arc::new(txindex::TxIndex::new(Arc::clone(&db))?);
                    return Ok(Self {
                        db,
                        blockstore,
                        utxostore,
                        chainstate,
                        txindex,
                        pruning_manager: None,
                    });
                }
            }
        }

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
        Self::with_backend_pruning_and_indexing(data_dir, backend, pruning_config, None, None)
    }

    /// Create a new storage instance with backend, pruning, and indexing config
    pub fn with_backend_pruning_and_indexing<P: AsRef<Path>>(
        data_dir: P,
        backend: DatabaseBackend,
        pruning_config: Option<PruningConfig>,
        indexing_config: Option<crate::config::IndexingConfig>,
        storage_config: Option<&crate::config::StorageConfig>,
    ) -> Result<Self> {
        #[cfg(feature = "compression")]
        {
            Self::with_backend_pruning_indexing_and_compression(
                data_dir,
                backend,
                pruning_config,
                indexing_config,
                None,
                storage_config,
            )
        }
        #[cfg(not(feature = "compression"))]
        {
            // When compression feature is disabled, use the internal implementation
            let db = Arc::from(create_database(data_dir, backend, storage_config)?);
            let blockstore = Arc::new(blockstore::BlockStore::new(Arc::clone(&db))?);
            let utxostore = Arc::new(utxostore::UtxoStore::new(Arc::clone(&db))?);
            let chainstate = chainstate::ChainState::new(Arc::clone(&db))?;

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
    }

    /// Create a new storage instance with backend, pruning, indexing, and compression config
    #[cfg(feature = "compression")]
    pub fn with_backend_pruning_indexing_and_compression<P: AsRef<Path>>(
        data_dir: P,
        backend: DatabaseBackend,
        pruning_config: Option<PruningConfig>,
        indexing_config: Option<crate::config::IndexingConfig>,
        compression_config: Option<crate::config::CompressionConfig>,
        storage_config: Option<&crate::config::StorageConfig>,
    ) -> Result<Self> {
        let db = Arc::from(create_database(data_dir, backend, storage_config)?);

        // Configure block store with compression settings
        #[cfg(feature = "compression")]
        let blockstore = {
            let (
                block_compression_enabled,
                block_compression_level,
                witness_compression_enabled,
                witness_compression_level,
            ) = if let Some(compression) = &compression_config {
                (
                    compression.block_compression_enabled,
                    compression.block_compression_level,
                    compression.witness_compression_enabled,
                    compression.witness_compression_level,
                )
            } else {
                (false, 3, false, 2) // Defaults: disabled
            };
            Arc::new(blockstore::BlockStore::new_with_compression(
                Arc::clone(&db),
                block_compression_enabled,
                block_compression_level,
                witness_compression_enabled,
                witness_compression_level,
            )?)
        };

        #[cfg(not(feature = "compression"))]
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

    /// Load AssumeUTXO snapshot into storage.
    /// Requires `tip_header` for chain state; when None, returns Err (header fetch not yet implemented).
    pub fn load_assumeutxo_snapshot(
        &self,
        utxo_set: &blvm_protocol::UtxoSet,
        metadata: &crate::storage::assumeutxo::SnapshotMetadata,
        tip_header: Option<&blvm_protocol::BlockHeader>,
    ) -> Result<()> {
        self.utxostore.store_utxo_set(utxo_set)?;
        let header = tip_header.ok_or_else(|| {
            anyhow::anyhow!(
                "AssumeUTXO chain state requires block header. Implement header fetch (getdata) or add header to snapshot format."
            )
        })?;
        let chain_info = chainstate::ChainInfo {
            tip_hash: metadata.block_hash,
            tip_header: header.clone(),
            height: metadata.block_height,
            total_work: 0, // Would need chainwork from header chain
            chain_params: chainstate::ChainParams::default(),
        };
        self.chainstate.store_chain_info(&chain_info)?;
        Ok(())
    }

    /// Get the transaction index (as Arc for sharing)
    pub fn transactions(&self) -> Arc<txindex::TxIndex> {
        Arc::clone(&self.txindex)
    }

    /// Open a custom tree for application-specific data
    ///
    /// This allows modules to store their own key-value data in the database.
    /// The tree name should be unique and descriptive (e.g., "payment_states", "vaults").
    pub fn open_tree(&self, name: &str) -> Result<Arc<dyn database::Tree>> {
        Ok(Arc::from(self.db.open_tree(name)?))
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
        block: &blvm_protocol::Block,
        block_hash: &blvm_protocol::Hash,
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
