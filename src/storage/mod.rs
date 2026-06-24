//! Storage layer for blvm-node
//!
//! This module provides persistent storage for blocks, UTXO set, and chain state.
//! Supports multiple database backends via feature flags (tidesdb, redb, sled, rocksdb, heed3).

pub mod assumeutxo;
pub mod bitcoin_core_blocks;
pub mod bitcoin_core_format;
#[cfg(feature = "rocksdb")]
pub mod bitcoin_core_leveldb;
#[cfg(feature = "rocksdb")]
pub mod bitcoin_core_migrate;
#[cfg(feature = "rocksdb")]
pub mod bitcoin_core_obfuscation;
pub mod bitcoin_core_storage;
pub mod bitcoin_detection;
pub mod block_index;
pub mod blockstore;
pub mod buffered_store;
pub mod chainstate;
#[cfg(feature = "utxo-commitments")]
pub mod commitment_store;
pub mod database;
pub mod disk_utxo;
pub mod hashing;
pub mod ibd_autorepair;
/// Age-tiered IBD UTXO engine for high-throughput Initial Block Download validation.
pub mod ibd_engine;
#[cfg(feature = "production")]
pub mod ibd_utxo_muhash;
#[cfg(feature = "production")]
pub mod ibd_utxo_store;
pub mod pruning;
#[cfg(feature = "heed3")]
pub mod rkyv_codec;
pub mod serialization_cache;
pub mod txindex;
pub mod utxo_value_codec;
pub mod utxostore;
pub mod wal;

use crate::config::PruningConfig;
use anyhow::Result;
use database::{Database, DatabaseBackend, create_database, default_backend, fallback_backend};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

#[cfg(feature = "rocksdb")]
use bitcoin_core_storage::BitcoinCoreStorage;
#[cfg(feature = "rocksdb")]
use bitcoin_detection::BitcoinCoreDetection;
#[cfg(feature = "rocksdb")]
use bitcoin_detection::CoreDataNetwork;

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
    /// Create a new storage instance with explicit Bitcoin network for Core data detection.
    ///
    /// Pass the runtime network (mainnet / testnet / regtest) so that existing Bitcoin Core
    /// data directories are detected for the correct network variant.  All callers that know
    /// the network at construction time should prefer this over [`Storage::new`].
    pub fn new_with_network<P: AsRef<Path>>(
        data_dir: P,
        network: blvm_protocol::types::Network,
    ) -> Result<Self> {
        #[cfg(feature = "rocksdb")]
        {
            use bitcoin_detection::CoreDataNetwork;
            let core_network = match network {
                blvm_protocol::types::Network::Mainnet => CoreDataNetwork::Mainnet,
                blvm_protocol::types::Network::Testnet => CoreDataNetwork::Testnet,
                blvm_protocol::types::Network::Regtest => CoreDataNetwork::Regtest,
                blvm_protocol::types::Network::Signet => CoreDataNetwork::Signet,
            };
            Storage::new_inner(data_dir, core_network)
        }
        #[cfg(not(feature = "rocksdb"))]
        {
            let _ = network;
            Storage::new(data_dir)
        }
    }

    /// Create a new storage instance with default backend.
    ///
    /// Attempts to use the default backend (TidesDB if available, else Redb), and gracefully
    /// falls back to alternatives if the primary fails.
    ///
    /// If existing node data is detected, will use RocksDB to read it.
    ///
    /// **Prefer [`Storage::new_with_network`] whenever the runtime network is known.**
    /// This function defaults to [`CoreDataNetwork::Mainnet`] for Bitcoin Core data detection,
    /// which means testnet/regtest Core datadirs will never be auto-detected when called
    /// through this path.  Only use `Storage::new` for tests or when the network is
    /// genuinely unavailable at construction time.
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Result<Self> {
        #[cfg(feature = "rocksdb")]
        {
            use bitcoin_detection::CoreDataNetwork;
            Storage::new_inner(data_dir, CoreDataNetwork::Mainnet)
        }
        #[cfg(not(feature = "rocksdb"))]
        {
            let default = default_backend();
            match Self::with_backend(data_dir.as_ref(), default) {
                Ok(storage) => Ok(storage),
                Err(e) => {
                    if let Some(fallback_backend) = fallback_backend(default) {
                        warn!(
                            "Failed to initialize {:?} backend: {}. Falling back to {:?}.",
                            default, e, fallback_backend
                        );
                        Self::with_backend(data_dir, fallback_backend)
                    } else {
                        Err(anyhow::anyhow!(
                            "Failed to initialize {:?} backend: {}. No fallback available.",
                            default,
                            e
                        ))
                    }
                }
            }
        }
    }

    #[cfg(feature = "rocksdb")]
    fn new_inner<P: AsRef<Path>>(
        data_dir: P,
        core_network: bitcoin_detection::CoreDataNetwork,
    ) -> Result<Self> {
        use bitcoin_core_storage::BitcoinCoreStorage;
        {
            if let Ok(Some(backend)) =
                BitcoinCoreStorage::detect_and_open(data_dir.as_ref(), core_network)
            {
                if backend == DatabaseBackend::RocksDB {
                    info!("Existing node data detected, opening with RocksDB backend");
                    let core_dir = if BitcoinCoreDetection::is_core_layout_at(data_dir.as_ref()) {
                        data_dir.as_ref().to_path_buf()
                    } else if let Some(d) = BitcoinCoreDetection::detect_data_dir(core_network)? {
                        d
                    } else {
                        data_dir.as_ref().to_path_buf()
                    };
                    let db = Arc::from(BitcoinCoreStorage::open_bitcoin_core_database(
                        &core_dir,
                        core_network,
                    )?);

                    let block_reader = {
                        let blocks_dir = core_dir.join("blocks");
                        if blocks_dir.exists() {
                            match bitcoin_core_blocks::BitcoinCoreBlockReader::new_with_cache(
                                &blocks_dir,
                                core_network,
                                Some(data_dir.as_ref()),
                            ) {
                                Ok(reader) => {
                                    info!(
                                        "Block files detected, enabling block file reader with index cache"
                                    );
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

    /// Resolve and optionally migrate the BLVM store path for node startup.
    #[cfg(feature = "rocksdb")]
    pub fn prepare_node_store<P: AsRef<Path>>(
        data_dir: P,
        network: blvm_protocol::types::Network,
        storage_config: Option<&crate::config::StorageConfig>,
    ) -> Result<PathBuf> {
        Self::resolve_node_store_path(
            data_dir.as_ref(),
            Self::protocol_to_core_network(network),
            storage_config,
        )
    }

    /// Same as [`prepare_node_store`] using [`ProtocolVersion`].
    #[cfg(feature = "rocksdb")]
    pub fn prepare_node_store_from_protocol<P: AsRef<Path>>(
        data_dir: P,
        protocol: blvm_protocol::ProtocolVersion,
        storage_config: Option<&crate::config::StorageConfig>,
    ) -> Result<PathBuf> {
        let network = protocol.consensus_network();
        Self::prepare_node_store(data_dir, network, storage_config)
    }

    /// Create storage for node startup: resolves Core auto-migrate, then opens the BLVM store.
    pub fn open_for_node<P: AsRef<Path>>(
        data_dir: P,
        network: blvm_protocol::types::Network,
        storage_config: Option<&crate::config::StorageConfig>,
    ) -> Result<Self> {
        #[cfg(feature = "rocksdb")]
        let store_path = Self::resolve_node_store_path(
            data_dir.as_ref(),
            Self::protocol_to_core_network(network),
            storage_config,
        )?;
        #[cfg(not(feature = "rocksdb"))]
        let store_path = data_dir.as_ref().to_path_buf();

        let (backend, pruning_config, indexing_config) = if let Some(sc) = storage_config {
            let backend = database::backend_from_config(sc.database_backend)?;
            (backend, sc.pruning.clone(), sc.indexing.clone())
        } else {
            (default_backend(), None, None)
        };

        info!(
            "[NODE_INIT] Opening BLVM store at {:?} (backend {:?})",
            store_path, backend
        );
        Self::with_backend_pruning_and_indexing(
            store_path,
            backend,
            pruning_config,
            indexing_config,
            storage_config,
            Some(data_dir.as_ref()),
            Some(network),
        )
    }

    #[cfg(feature = "rocksdb")]
    fn protocol_to_core_network(network: blvm_protocol::types::Network) -> CoreDataNetwork {
        match network {
            blvm_protocol::types::Network::Mainnet => CoreDataNetwork::Mainnet,
            blvm_protocol::types::Network::Testnet => CoreDataNetwork::Testnet,
            blvm_protocol::types::Network::Regtest => CoreDataNetwork::Regtest,
            blvm_protocol::types::Network::Signet => CoreDataNetwork::Signet,
        }
    }

    /// Decide where the BLVM native database lives; auto-migrate from Core when needed.
    #[cfg(feature = "rocksdb")]
    fn resolve_node_store_path(
        data_dir: &Path,
        core_network: CoreDataNetwork,
        storage_config: Option<&crate::config::StorageConfig>,
    ) -> Result<PathBuf> {
        use bitcoin_core_migrate::{
            MigrateCoreArgs, has_migration_marker, migrate_core_data, read_migration_marker,
        };

        let auto_migrate = storage_config
            .map(|s| s.auto_migrate_core_effective())
            .unwrap_or_else(|| !crate::utils::env_bool("BLVM_NO_AUTO_MIGRATE_CORE"));

        let dest = storage_config
            .map(|s| s.resolve_core_migrate_destination(data_dir))
            .unwrap_or_else(|| data_dir.join("blvm"));

        if BitcoinCoreStorage::has_blvm_database(data_dir) {
            info!(
                "[CORE_IMPORT] layout=blvm-native action=open path={:?}",
                data_dir
            );
            return Ok(data_dir.to_path_buf());
        }

        if BitcoinCoreStorage::has_blvm_database(&dest) || has_migration_marker(&dest) {
            Self::warn_if_core_chainstate_newer_than_marker(data_dir, &dest);
            let marker_tip = read_migration_marker(&dest)
                .ok()
                .flatten()
                .map(|m| format!(" height={} tip={}", m.height, m.tip_hash))
                .unwrap_or_default();
            info!(
                "[CORE_IMPORT] layout=core-migrated action=open path={:?}{}",
                dest, marker_tip
            );
            return Ok(dest);
        }

        if auto_migrate && BitcoinCoreDetection::is_core_layout_at(data_dir) {
            BitcoinCoreStorage::ensure_not_locked(data_dir)?;

            info!(
                "[CORE_IMPORT] layout=core action=migrate source={:?} dest={:?} network={:?}",
                data_dir, dest, core_network
            );

            let backend = if let Some(sc) = storage_config {
                database::backend_from_config(sc.database_backend)?
            } else {
                default_backend()
            };

            migrate_core_data(MigrateCoreArgs {
                source: data_dir.to_path_buf(),
                destination: dest.clone(),
                network: core_network,
                verify: false,
                verbose: false,
                dest_backend: Some(backend),
                stop_after: None,
                reuse_core_block_files: storage_config
                    .map(|s| s.reuse_core_block_files_effective())
                    .unwrap_or_else(|| {
                        crate::config::StorageConfig::default().reuse_core_block_files_effective()
                    }),
            })?;

            return Ok(dest);
        }

        Ok(data_dir.to_path_buf())
    }

    #[cfg(feature = "rocksdb")]
    fn warn_if_core_chainstate_newer_than_marker(core_dir: &Path, blvm_store: &Path) {
        use bitcoin_core_migrate::{read_core_best_block_hash, read_migration_marker};

        let Ok(Some(marker)) = read_migration_marker(blvm_store) else {
            return;
        };

        if marker.source != core_dir {
            warn!(
                "[CORE_IMPORT] Migration marker source {:?} != current datadir {:?}; \
                 ensure you are using the same Core datadir that was migrated.",
                marker.source, core_dir
            );
        }

        if !marker.tip_hash.is_empty() {
            if let Ok(marker_tip) = crate::storage::hashing::hash_from_rpc_hex(&marker.tip_hash) {
                if let Ok(Some(core_tip)) = read_core_best_block_hash(core_dir) {
                    if core_tip != marker_tip {
                        warn!(
                            "[CORE_IMPORT] Core tip {} differs from migration marker {}; \
                             Core may have re-synced. Re-run `blvm migrate core --verify` to refresh.",
                            crate::storage::hashing::hash_to_rpc_hex(&core_tip),
                            marker.tip_hash
                        );
                    }
                }
            }
        }

        let Ok(secs) = marker.migrated_at.parse::<u64>() else {
            return;
        };
        let chainstate = core_dir.join("chainstate");
        let Ok(meta) = std::fs::metadata(&chainstate) else {
            return;
        };
        let Ok(modified) = meta.modified() else {
            return;
        };
        let Some(migrated) =
            std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(secs))
        else {
            return;
        };
        if modified > migrated {
            warn!(
                "[CORE_IMPORT] Core chainstate at {:?} is newer than migration marker at {:?}; \
                 Core may have re-synced. Re-run `blvm migrate core --verify` if tip should match Core.",
                chainstate, blvm_store
            );
        }
    }

    #[cfg(feature = "rocksdb")]
    fn open_core_block_reader_for_store(
        blvm_store: &Path,
        core_datadir: Option<&Path>,
        storage_config: Option<&crate::config::StorageConfig>,
    ) -> Option<Arc<bitcoin_core_blocks::BitcoinCoreBlockReader>> {
        use bitcoin_core_migrate::read_migration_marker;
        use std::str::FromStr;

        let marker = read_migration_marker(blvm_store).ok().flatten();
        let reuse = storage_config
            .map(|s| s.reuse_core_block_files_effective())
            .unwrap_or(false)
            || marker
                .as_ref()
                .and_then(|m| m.reuse_core_blocks)
                .unwrap_or(false);
        if !reuse {
            return None;
        }

        let core_dir = marker
            .as_ref()
            .map(|m| m.source.clone())
            .or_else(|| core_datadir.map(|p| p.to_path_buf()))?;
        let blocks_dir = core_dir.join("blocks");
        if !blocks_dir.is_dir() {
            warn!(
                "[CORE_IMPORT] reuse_core_blocks enabled but Core blocks dir missing at {:?}",
                blocks_dir
            );
            return None;
        }

        let network = marker
            .as_ref()
            .and_then(|m| CoreDataNetwork::from_str(&m.network).ok())
            .or_else(|| BitcoinCoreDetection::detect_network(&core_dir))
            .unwrap_or(CoreDataNetwork::Mainnet);

        match bitcoin_core_blocks::BitcoinCoreBlockReader::new_with_cache(
            &blocks_dir,
            network,
            Some(blvm_store),
        ) {
            Ok(reader) => {
                info!(
                    "[CORE_IMPORT] Reading Core block files in place from {:?}",
                    blocks_dir
                );
                Some(Arc::new(reader))
            }
            Err(e) => {
                warn!("[CORE_IMPORT] Failed to open Core block file reader: {e}");
                None
            }
        }
    }

    fn open_blockstore(
        db: Arc<dyn database::Database>,
        blvm_store: &Path,
        core_datadir: Option<&Path>,
        storage_config: Option<&crate::config::StorageConfig>,
    ) -> Result<Arc<blockstore::BlockStore>> {
        #[cfg(feature = "rocksdb")]
        {
            let reader =
                Self::open_core_block_reader_for_store(blvm_store, core_datadir, storage_config);
            if let Some(reader) = reader {
                return Ok(Arc::new(
                    blockstore::BlockStore::new_with_bitcoin_core_reader(db, Some(reader))?,
                ));
            }
        }
        Ok(Arc::new(blockstore::BlockStore::new(db)?))
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
        Self::with_backend_pruning_and_indexing(
            data_dir,
            backend,
            pruning_config,
            None,
            None,
            None,
            None,
        )
    }

    /// Create a new storage instance with backend, pruning, and indexing config
    pub fn with_backend_pruning_and_indexing<P: AsRef<Path>>(
        data_dir: P,
        backend: DatabaseBackend,
        pruning_config: Option<PruningConfig>,
        indexing_config: Option<crate::config::IndexingConfig>,
        storage_config: Option<&crate::config::StorageConfig>,
        core_datadir: Option<&Path>,
        consensus_network: Option<blvm_protocol::types::Network>,
    ) -> Result<Self> {
        let network = consensus_network.unwrap_or(blvm_protocol::types::Network::Mainnet);
        #[cfg(feature = "compression")]
        {
            let compression_config = storage_config.and_then(|sc| sc.compression.clone());
            Self::with_backend_pruning_indexing_and_compression(
                data_dir,
                backend,
                pruning_config,
                indexing_config,
                compression_config,
                storage_config,
                core_datadir,
                Some(network),
            )
        }
        #[cfg(not(feature = "compression"))]
        {
            // When compression feature is disabled, use the internal implementation
            let store_path = data_dir.as_ref();
            let db = Arc::from(create_database(store_path, backend, storage_config)?);
            let blockstore =
                Self::open_blockstore(Arc::clone(&db), store_path, core_datadir, storage_config)?;
            let utxostore = Arc::new(utxostore::UtxoStore::new(Arc::clone(&db))?);
            let chainstate = chainstate::ChainState::new(Arc::clone(&db))?;

            let txindex = if let Some(indexing) = indexing_config {
                Arc::new(txindex::TxIndex::with_config(Arc::clone(&db), indexing)?)
            } else {
                Arc::new(txindex::TxIndex::new(Arc::clone(&db))?)
            };

            let pruning_manager = Self::build_pruning_manager(
                pruning_config,
                Arc::clone(&db),
                Arc::clone(&blockstore),
                Arc::clone(&utxostore),
                network,
            );

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

    fn build_pruning_manager(
        pruning_config: Option<PruningConfig>,
        db: Arc<dyn database::Database>,
        blockstore: Arc<blockstore::BlockStore>,
        utxostore: Arc<utxostore::UtxoStore>,
        network: blvm_protocol::types::Network,
    ) -> Option<Arc<pruning::PruningManager>> {
        pruning_config.map(|config| {
            #[cfg(feature = "utxo-commitments")]
            {
                let needs_commitments = matches!(
                    config.mode,
                    crate::config::PruningMode::Aggressive {
                        keep_commitments: true,
                        ..
                    }
                ) || matches!(
                    config.mode,
                    crate::config::PruningMode::Custom {
                        keep_commitments: true,
                        ..
                    }
                );
                if needs_commitments {
                    let commitment_store = match commitment_store::CommitmentStore::new(Arc::clone(&db))
                    {
                        Ok(store) => Arc::new(store),
                        Err(e) => {
                            warn!(
                                "Failed to create commitment store: {}. Pruning will continue without commitments.",
                                e
                            );
                            return Arc::new(
                                pruning::PruningManager::new(config, Arc::clone(&blockstore))
                                    .with_network(network),
                            );
                        }
                    };
                    Arc::new(
                        pruning::PruningManager::with_utxo_commitments(
                            config,
                            Arc::clone(&blockstore),
                            commitment_store,
                            Arc::clone(&utxostore),
                        )
                        .with_network(network),
                    )
                } else {
                    Arc::new(
                        pruning::PruningManager::new(config, Arc::clone(&blockstore))
                            .with_network(network),
                    )
                }
            }
            #[cfg(not(feature = "utxo-commitments"))]
            {
                Arc::new(
                    pruning::PruningManager::new(config, Arc::clone(&blockstore))
                        .with_network(network),
                )
            }
        })
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
        core_datadir: Option<&Path>,
        consensus_network: Option<blvm_protocol::types::Network>,
    ) -> Result<Self> {
        let network = consensus_network.unwrap_or(blvm_protocol::types::Network::Mainnet);
        let store_path = data_dir.as_ref();
        let db = Arc::from(create_database(store_path, backend, storage_config)?);

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
            #[cfg(feature = "rocksdb")]
            {
                let reader = Self::open_core_block_reader_for_store(
                    store_path,
                    core_datadir,
                    storage_config,
                );
                Arc::new(
                    blockstore::BlockStore::new_with_compression_and_bitcoin_core_reader(
                        Arc::clone(&db),
                        block_compression_enabled,
                        block_compression_level,
                        witness_compression_enabled,
                        witness_compression_level,
                        reader,
                    )?,
                )
            }
            #[cfg(not(feature = "rocksdb"))]
            {
                Arc::new(blockstore::BlockStore::new_with_compression(
                    Arc::clone(&db),
                    block_compression_enabled,
                    block_compression_level,
                    witness_compression_enabled,
                    witness_compression_level,
                )?)
            }
        };

        #[cfg(not(feature = "compression"))]
        let blockstore =
            Self::open_blockstore(Arc::clone(&db), store_path, core_datadir, storage_config)?;
        let utxostore = if let Some(compression) = &compression_config {
            Arc::new(utxostore::UtxoStore::new_with_compression(
                Arc::clone(&db),
                compression.utxo_compression_enabled,
                compression.utxo_compression_level,
            )?)
        } else {
            Arc::new(utxostore::UtxoStore::new(Arc::clone(&db))?)
        };
        let chainstate = chainstate::ChainState::new(Arc::clone(&db))?;

        // Configure transaction indexing based on config
        let txindex = if let Some(indexing) = indexing_config {
            Arc::new(txindex::TxIndex::with_config(Arc::clone(&db), indexing)?)
        } else {
            Arc::new(txindex::TxIndex::new(Arc::clone(&db))?)
        };

        let pruning_manager = Self::build_pruning_manager(
            pruning_config,
            Arc::clone(&db),
            Arc::clone(&blockstore),
            Arc::clone(&utxostore),
            network,
        );

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
    /// Uses `tip_header` when provided; otherwise fetches header from blockstore by metadata.block_hash.
    pub fn load_assumeutxo_snapshot(
        &self,
        utxo_set: &blvm_protocol::UtxoSet,
        metadata: &crate::storage::assumeutxo::SnapshotMetadata,
        tip_header: Option<&blvm_protocol::BlockHeader>,
    ) -> Result<()> {
        self.utxostore.store_utxo_set(utxo_set)?;
        let header = match tip_header {
            Some(h) => h.clone(),
            None => self
                .blockstore
                .get_header(&metadata.block_hash)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "AssumeUTXO requires block header. Block {} not in store. Run IBD first or include header in snapshot.",
                        hex::encode(metadata.block_hash)
                    )
                })?,
        };
        let chain_info = chainstate::ChainInfo {
            tip_hash: metadata.block_hash,
            tip_header: header,
            height: metadata.block_height,
            total_work: 0, // Would need chainwork from header chain
            chain_params: chainstate::ChainParams::default(),
        };
        self.chainstate.store_chain_info(&chain_info)?;
        Ok(())
    }

    /// If `chain_info` is missing but the block index has blocks (e.g. crash before metadata
    /// flush, or legacy parallel IBD that never wrote `chain_info`), rebuild tip from the
    /// highest stored height so `get_height()` and IBD resume match on-disk state.
    pub fn recover_chain_tip_from_blockstore(&self) -> Result<()> {
        use crate::storage::chainstate::{ChainInfo, ChainParams};

        if self.chain().load_chain_info()?.is_some() {
            return Ok(());
        }
        let Some(max_h) = self.blockstore.highest_stored_height()? else {
            return Ok(());
        };
        let Some(tip_hash) = self.blockstore.get_hash_by_height(max_h)? else {
            return Ok(());
        };
        let Some(block) = self.blockstore.get_block(&tip_hash)? else {
            return Ok(());
        };
        let genesis_hash = self.blockstore.get_hash_by_height(0)?.unwrap_or_default();
        let mut params = ChainParams::default();
        params.genesis_hash = genesis_hash;
        let info = ChainInfo {
            tip_hash,
            tip_header: block.header.clone(),
            height: max_h,
            total_work: 0,
            chain_params: params,
        };
        self.chainstate.store_chain_info(&info)?;
        info!(
            "Recovered chain_info from block index (tip_height={}, tip_hash prefix {:02x}{:02x}{:02x}{:02x})",
            max_h, tip_hash[0], tip_hash[1], tip_hash[2], tip_hash[3]
        );
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

    /// UTXO row encoding for this store (rkyv on heed3/LMDB, bincode otherwise).
    pub fn utxo_value_codec(&self) -> utxo_value_codec::ValueCodec {
        utxo_value_codec::ValueCodec::for_database(self.db.as_ref())
    }

    /// Flush all pending writes to disk
    pub fn flush(&self) -> Result<()> {
        self.db.flush()
    }

    /// Forward IBD memory pressure to the database backend (RocksDB may reduce background jobs when opted in).
    #[inline]
    pub fn ibd_memory_pressure_tick(&self, level_u8: u8) {
        self.db.ibd_memory_pressure_tick(level_u8);
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

    /// Set the active network on the pruning manager so UTXO reconstruction
    /// uses the correct consensus rules. Must be called before the Storage is
    /// shared (i.e. before wrapping in Arc).
    pub fn set_pruning_network(&mut self, network: blvm_protocol::types::Network) {
        if let Some(pm) = self.pruning_manager.take() {
            if let Ok(mut inner) = Arc::try_unwrap(pm) {
                inner.network = network;
                self.pruning_manager = Some(Arc::new(inner));
            } else {
                warn!("set_pruning_network: PruningManager already shared; network not updated");
            }
        }
    }
}
