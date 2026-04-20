//! Database abstraction layer
//!
//! Provides a unified interface for different database backends (tidesdb, redb, sled, rocksdb).
//! Allows switching between storage engines via feature flags.
//!
//! All backends must support the same set of tree names.
//! Module storage has been removed; modules use their own DB at {data_dir}/db/.

use anyhow::Result;
use std::any::Any;
use std::path::Path;
use std::sync::Arc;

mod known_trees;
pub use known_trees::KNOWN_TREE_NAMES;

/// Database abstraction trait
///
/// Provides a unified interface for key-value storage operations
/// that can be implemented by different backends (sled, redb).
pub trait Database: Send + Sync {
    /// Open a named tree/table
    fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>>;

    /// Flush all pending writes
    fn flush(&self) -> Result<()>;

    /// Optional: reduce RocksDB background work / shrink caches when IBD reports memory pressure.
    /// `level_u8` is `PressureLevel` as `u8` (see `parallel_ibd::memory`).
    fn ibd_memory_pressure_tick(&self, _level_u8: u8) {}

    /// For backend-specific fast paths (e.g. cross-column-family RocksDB `WriteBatch`).
    fn as_any(&self) -> &dyn Any;
}

/// Tree/Table abstraction trait
///
/// Represents a named collection of key-value pairs within a database.
pub trait Tree: Send + Sync {
    /// Insert a key-value pair
    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()>;

    /// Get a value by key
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Batch get: fetch multiple keys in one call. Default impl does sequential get.
    /// RocksDB overrides with multi_get_cf for much faster bulk reads (avoids per-key overhead).
    fn get_many(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>> {
        let mut results = Vec::with_capacity(keys.len());
        for key in keys {
            results.push(self.get(key)?);
        }
        Ok(results)
    }

    /// Remove a key-value pair
    fn remove(&self, key: &[u8]) -> Result<()>;

    /// Check if a key exists
    fn contains_key(&self, key: &[u8]) -> Result<bool>;

    /// Clear all entries
    fn clear(&self) -> Result<()>;

    /// Get number of entries
    fn len(&self) -> Result<usize>;

    /// Check if tree is empty
    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Flush in-memory (memtable) data for this tree to durable on-disk storage.
    /// Required before writing a persistence marker when writes used `commit_no_wal`.
    fn flush_to_disk(&self) -> Result<()> {
        Ok(())
    }

    /// Iterate over all key-value pairs
    fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_>;

    /// Create a batch writer for efficient bulk operations
    ///
    /// Batch writes are 10-100x faster than individual inserts because they
    /// commit all operations in a single transaction instead of one per operation.
    ///
    /// # Example
    /// ```ignore
    /// let mut batch = tree.batch()?;
    /// for (key, value) in items {
    ///     batch.put(key, value);
    /// }
    /// batch.commit()?;  // Single atomic commit
    /// ```
    ///
    /// Returns `Err` if a batch cannot be created (e.g. RocksDB column family missing).
    fn batch(&self) -> Result<Box<dyn BatchWriter + '_>>;
}

/// Batch writer for efficient bulk database operations
///
/// Accumulates multiple put/delete operations and commits them atomically.
/// This is critical for IBD performance where we need to update thousands
/// of UTXO entries per block.
///
/// # Performance
/// - Individual Tree::insert(): ~1ms per operation (transaction overhead)
/// - BatchWriter: ~1ms total for thousands of operations (single transaction)
///
/// # Atomicity
/// All operations in a batch are committed atomically - either all succeed
/// or none do. This ensures database consistency even on crash.
pub trait BatchWriter {
    /// Add a key-value pair to the batch
    fn put(&mut self, key: &[u8], value: &[u8]);

    /// Mark a key for deletion in the batch
    fn delete(&mut self, key: &[u8]);

    /// Commit all batched operations atomically
    ///
    /// Returns Ok(()) if all operations were applied successfully.
    /// On error, no operations are applied (atomic rollback).
    fn commit(self: Box<Self>) -> Result<()>;

    /// Commit without Write-Ahead Log (WAL).
    /// Safe for IBD where crash recovery re-downloads from peers.
    /// Default: falls back to `commit()`.
    fn commit_no_wal(self: Box<Self>) -> Result<()> {
        self.commit()
    }

    /// Get the number of pending operations in the batch
    fn len(&self) -> usize;

    /// Check if the batch is empty
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Database backend type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseBackend {
    Sled,
    Redb,
    RocksDB,
    TidesDB,
}

/// Resolve config backend to concrete DatabaseBackend.
/// Returns Err if the requested backend's feature is not enabled.
pub fn backend_from_config(
    config: crate::config::DatabaseBackendConfig,
) -> Result<DatabaseBackend> {
    use crate::config::DatabaseBackendConfig;
    match config {
        DatabaseBackendConfig::Sled => {
            #[cfg(feature = "sled")]
            return Ok(DatabaseBackend::Sled);
            #[cfg(not(feature = "sled"))]
            return Err(anyhow::anyhow!(
                "Sled backend not available (feature not enabled)"
            ));
        }
        DatabaseBackendConfig::Redb => {
            #[cfg(feature = "redb")]
            return Ok(DatabaseBackend::Redb);
            #[cfg(not(feature = "redb"))]
            return Err(anyhow::anyhow!(
                "Redb backend not available (feature not enabled)"
            ));
        }
        DatabaseBackendConfig::Rocksdb => {
            #[cfg(feature = "rocksdb")]
            return Ok(DatabaseBackend::RocksDB);
            #[cfg(not(feature = "rocksdb"))]
            return Err(anyhow::anyhow!(
                "RocksDB backend not available (build with --features rocksdb)"
            ));
        }
        DatabaseBackendConfig::Tidesdb => {
            #[cfg(feature = "tidesdb")]
            return Ok(DatabaseBackend::TidesDB);
            #[cfg(not(feature = "tidesdb"))]
            return Err(anyhow::anyhow!(
                "TidesDB backend not available (build with --features tidesdb)"
            ));
        }
        DatabaseBackendConfig::Auto => Ok(default_backend()),
    }
}

/// Create a database instance based on backend type.
/// When `storage_config` is provided and backend is TidesDB, uses tidesdb.* config options.
pub fn create_database<P: AsRef<Path>>(
    data_dir: P,
    backend: DatabaseBackend,
    storage_config: Option<&crate::config::StorageConfig>,
) -> Result<Box<dyn Database>> {
    match backend {
        #[cfg(feature = "sled")]
        DatabaseBackend::Sled => Ok(Box::new(sled_impl::SledDatabase::new(data_dir)?)),
        #[cfg(not(feature = "sled"))]
        DatabaseBackend::Sled => Err(anyhow::anyhow!(
            "Sled backend not available (feature not enabled)"
        )),
        #[cfg(feature = "redb")]
        DatabaseBackend::Redb => Ok(Box::new(redb_impl::RedbDatabase::new(
            data_dir,
            storage_config,
        )?)),
        #[cfg(not(feature = "redb"))]
        DatabaseBackend::Redb => Err(anyhow::anyhow!(
            "Redb backend not available (feature not enabled)"
        )),
        #[cfg(feature = "rocksdb")]
        DatabaseBackend::RocksDB => Ok(Box::new(rocksdb_impl::RocksDBDatabase::new(
            data_dir,
            storage_config,
        )?)),
        #[cfg(not(feature = "rocksdb"))]
        DatabaseBackend::RocksDB => Err(anyhow::anyhow!(
            "RocksDB backend not available (feature not enabled)"
        )),
        #[cfg(feature = "tidesdb")]
        DatabaseBackend::TidesDB => Ok(Box::new(tidesdb_impl::TidesDBDatabase::new(
            data_dir,
            storage_config.and_then(|s| s.tidesdb.as_ref()),
        )?)),
        #[cfg(not(feature = "tidesdb"))]
        DatabaseBackend::TidesDB => Err(anyhow::anyhow!(
            "TidesDB backend not available (build with --features tidesdb)"
        )),
    }
}

/// Get default database backend
///
/// Returns the preferred backend: TidesDB (if available) > Redb > Sled.
pub fn default_backend() -> DatabaseBackend {
    #[cfg(feature = "rocksdb")]
    return DatabaseBackend::RocksDB;
    #[cfg(all(not(feature = "rocksdb"), feature = "tidesdb"))]
    return DatabaseBackend::TidesDB;
    #[cfg(all(not(feature = "rocksdb"), not(feature = "tidesdb"), feature = "redb"))]
    return DatabaseBackend::Redb;
    #[cfg(all(
        not(feature = "rocksdb"),
        not(feature = "tidesdb"),
        not(feature = "redb"),
        feature = "sled"
    ))]
    return DatabaseBackend::Sled;
    #[cfg(all(
        not(feature = "rocksdb"),
        not(feature = "tidesdb"),
        not(feature = "redb"),
        not(feature = "sled")
    ))]
    compile_error!(
        "At least one storage backend must be enabled (rocksdb, redb, sled, or tidesdb)"
    );
    #[allow(unreachable_code)]
    DatabaseBackend::Redb // Only for cfg exhaustiveness; one of the above returns always runs
}

/// Get fallback database backend
///
/// Returns an alternative backend if the primary fails.
/// Returns None if no fallback is available.
pub fn fallback_backend(primary: DatabaseBackend) -> Option<DatabaseBackend> {
    match primary {
        DatabaseBackend::TidesDB => {
            #[cfg(feature = "redb")]
            {
                Some(DatabaseBackend::Redb)
            }
            #[cfg(all(not(feature = "redb"), feature = "rocksdb"))]
            {
                Some(DatabaseBackend::RocksDB)
            }
            #[cfg(all(not(feature = "redb"), not(feature = "rocksdb"), feature = "sled"))]
            {
                Some(DatabaseBackend::Sled)
            }
            #[cfg(all(not(feature = "redb"), not(feature = "rocksdb"), not(feature = "sled")))]
            {
                None
            }
        }
        DatabaseBackend::Redb => {
            #[cfg(feature = "tidesdb")]
            {
                Some(DatabaseBackend::TidesDB)
            }
            #[cfg(all(not(feature = "tidesdb"), feature = "sled"))]
            {
                Some(DatabaseBackend::Sled)
            }
            #[cfg(all(not(feature = "tidesdb"), not(feature = "sled"), feature = "rocksdb"))]
            {
                Some(DatabaseBackend::RocksDB)
            }
            #[cfg(all(
                not(feature = "tidesdb"),
                not(feature = "sled"),
                not(feature = "rocksdb")
            ))]
            {
                None
            }
        }
        DatabaseBackend::Sled => {
            #[cfg(feature = "redb")]
            {
                Some(DatabaseBackend::Redb)
            }
            #[cfg(all(not(feature = "redb"), feature = "rocksdb"))]
            {
                Some(DatabaseBackend::RocksDB)
            }
            #[cfg(all(not(feature = "redb"), not(feature = "rocksdb")))]
            {
                None
            }
        }
        DatabaseBackend::RocksDB => {
            #[cfg(feature = "tidesdb")]
            {
                Some(DatabaseBackend::TidesDB)
            }
            #[cfg(all(not(feature = "tidesdb"), feature = "redb"))]
            {
                Some(DatabaseBackend::Redb)
            }
            #[cfg(all(not(feature = "tidesdb"), not(feature = "redb"), feature = "sled"))]
            {
                Some(DatabaseBackend::Sled)
            }
            #[cfg(all(not(feature = "tidesdb"), not(feature = "redb"), not(feature = "sled")))]
            {
                None
            }
        }
    }
}

// Sled implementation
#[cfg(feature = "sled")]
mod sled_impl {
    use super::{BatchWriter, Database, Tree};
    use anyhow::Result;
    use sled::Db;
    use std::path::Path;
    use std::sync::Arc;

    pub struct SledDatabase {
        db: Arc<Db>,
    }

    impl SledDatabase {
        pub fn new<P: AsRef<Path>>(data_dir: P) -> Result<Self> {
            let db = sled::open(data_dir)?;
            Ok(Self { db: Arc::new(db) })
        }
    }

    impl Database for SledDatabase {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>> {
            if name.starts_with("module_") || name == "modules" {
                return Err(anyhow::anyhow!(
                    "Module storage has been removed. Use blvm_sdk::module::open_module_db."
                ));
            }

            let tree = self.db.open_tree(name)?;
            Ok(Box::new(SledTree {
                tree: Arc::new(tree),
            }))
        }

        fn flush(&self) -> Result<()> {
            self.db.flush()?;
            Ok(())
        }
    }

    struct SledTree {
        tree: Arc<sled::Tree>,
    }

    impl Tree for SledTree {
        fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
            self.tree.insert(key, value)?;
            Ok(())
        }

        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            Ok(self.tree.get(key)?.map(|v| v.to_vec()))
        }

        fn remove(&self, key: &[u8]) -> Result<()> {
            self.tree.remove(key)?;
            Ok(())
        }

        fn contains_key(&self, key: &[u8]) -> Result<bool> {
            Ok(self.tree.contains_key(key)?)
        }

        fn clear(&self) -> Result<()> {
            self.tree.clear()?;
            Ok(())
        }

        fn len(&self) -> Result<usize> {
            Ok(self.tree.len())
        }

        fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
            Box::new(self.tree.iter().map(|item| {
                item.map(|(k, v)| (k.to_vec(), v.to_vec()))
                    .map_err(|e| anyhow::anyhow!("Sled iteration error: {}", e))
            }))
        }

        fn batch(&self) -> Result<Box<dyn BatchWriter + '_>> {
            Ok(Box::new(SledBatchWriter {
                tree: Arc::clone(&self.tree),
                batch: sled::Batch::default(),
                op_count: 0,
            }))
        }
    }

    /// Sled batch writer using native sled::Batch
    struct SledBatchWriter {
        tree: Arc<sled::Tree>,
        batch: sled::Batch,
        op_count: usize,
    }

    impl BatchWriter for SledBatchWriter {
        fn put(&mut self, key: &[u8], value: &[u8]) {
            self.batch.insert(key, value);
            self.op_count += 1;
        }

        fn delete(&mut self, key: &[u8]) {
            self.batch.remove(key);
            self.op_count += 1;
        }

        fn commit(self: Box<Self>) -> Result<()> {
            self.tree.apply_batch(self.batch)?;
            Ok(())
        }

        fn len(&self) -> usize {
            self.op_count
        }
    }
}

// Redb implementation
#[cfg(feature = "redb")]
pub(crate) mod redb_impl {
    use super::{BatchWriter, Database, Tree};
    use anyhow::Result;
    use redb::{Database as RedbDb, ReadableTable, TableDefinition};
    use std::path::Path;
    use std::sync::Arc;

    // Pre-defined table definitions for all known trees
    // Redb requires static table definitions, so we pre-define all possible tables
    static BLOCKS_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("blocks");
    static HEADERS_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("headers");
    static HEIGHT_INDEX_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("height_index");
    static HASH_TO_HEIGHT_TABLE: TableDefinition<&[u8], &[u8]> =
        TableDefinition::new("hash_to_height");
    static WITNESSES_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("witnesses");
    static RECENT_HEADERS_TABLE: TableDefinition<&[u8], &[u8]> =
        TableDefinition::new("recent_headers");
    static UTXOS_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("utxos");
    static IBD_UTXOS_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("ibd_utxos");
    static SPENT_OUTPUTS_TABLE: TableDefinition<&[u8], &[u8]> =
        TableDefinition::new("spent_outputs");
    static CHAIN_INFO_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("chain_info");
    static WORK_CACHE_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("work_cache");
    static TX_BY_HASH_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tx_by_hash");
    static TX_BY_BLOCK_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tx_by_block");
    static TX_METADATA_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tx_metadata");
    static ADDRESS_TX_INDEX_TABLE: TableDefinition<&[u8], &[u8]> =
        TableDefinition::new("address_tx_index");
    static ADDRESS_OUTPUT_INDEX_TABLE: TableDefinition<&[u8], &[u8]> =
        TableDefinition::new("address_output_index");
    static ADDRESS_INPUT_INDEX_TABLE: TableDefinition<&[u8], &[u8]> =
        TableDefinition::new("address_input_index");
    static VALUE_INDEX_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("value_index");
    static INVALID_BLOCKS_TABLE: TableDefinition<&[u8], &[u8]> =
        TableDefinition::new("invalid_blocks");
    static CHAIN_TIPS_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("chain_tips");
    static BLOCK_METADATA_TABLE: TableDefinition<&[u8], &[u8]> =
        TableDefinition::new("block_metadata");
    static CHAINWORK_CACHE_TABLE: TableDefinition<&[u8], &[u8]> =
        TableDefinition::new("chainwork_cache");
    static UTXO_STATS_CACHE_TABLE: TableDefinition<&[u8], &[u8]> =
        TableDefinition::new("utxo_stats_cache");
    static NETWORK_HASHRATE_CACHE_TABLE: TableDefinition<&[u8], &[u8]> =
        TableDefinition::new("network_hashrate_cache");
    static UTXO_COMMITMENTS_TABLE: TableDefinition<&[u8], &[u8]> =
        TableDefinition::new("utxo_commitments");
    static COMMITMENT_HEIGHT_INDEX_TABLE: TableDefinition<&[u8], &[u8]> =
        TableDefinition::new("commitment_height_index");
    // Payment system tables
    static VAULTS_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("vaults");
    static POOLS_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("pools");
    static BATCHES_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("batches");
    // Module DB tables (used by blvm-sdk open_module_db)
    static SCHEMA_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("schema");
    static ITEMS_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("items");
    pub struct RedbDatabase {
        db: Arc<RedbDb>,
    }

    impl RedbDatabase {
        pub fn new<P: AsRef<Path>>(
            data_dir: P,
            storage_config: Option<&crate::config::StorageConfig>,
        ) -> Result<Self> {
            use std::sync::Mutex;
            // Global mutex to serialize database creation (prevents lock conflicts in tests)
            static DB_CREATE_MUTEX: Mutex<()> = Mutex::new(());
            tracing::info!("[REDB] Acquiring DB_CREATE_MUTEX...");
            let _guard = DB_CREATE_MUTEX.lock().unwrap();
            tracing::info!("[REDB] DB_CREATE_MUTEX acquired");

            // redb cache size: ENV > config > default 450 (matches Core -dbcache, 12-factor)
            let dbcache_mb: usize = std::env::var("BLVM_DBCACHE_MB")
                .ok()
                .and_then(|s| s.parse().ok())
                .or_else(|| storage_config.map(|s| s.dbcache_mb))
                .unwrap_or(450);
            let dbcache_bytes = dbcache_mb.saturating_mul(1024).saturating_mul(1024);
            let mut builder = RedbDb::builder();
            builder.set_cache_size(dbcache_bytes);
            tracing::info!(
                "[REDB] Cache size: {} MB (set via BLVM_DBCACHE_MB or config)",
                dbcache_mb
            );

            let db_path = data_dir.as_ref().join("redb.db");
            tracing::info!("[REDB] Database path: {:?}", db_path);
            tracing::info!(
                "[REDB] Database path absolute: {:?}",
                std::fs::canonicalize(&db_path).unwrap_or_else(|_| db_path.clone())
            );
            let exists = db_path.exists();
            tracing::info!("[REDB] db_path.exists() = {}", exists);
            // Try to open existing database first, then create if it doesn't exist
            let db = if exists {
                // Gather diagnostic information about the database file
                let file_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
                let file_size_mb = file_size / (1024 * 1024);
                tracing::info!(
                    "[REDB] Database file exists, size: {} MB ({})",
                    file_size_mb,
                    file_size
                );

                // Check if database is locked by another process
                // redb uses file locking, so if another process has it open, open() will fail immediately
                tracing::info!("[REDB] Attempting to open database (this may take time for large databases)...");
                tracing::info!("[REDB] Note: redb validates checksums on open, which can be slow for large databases");
                tracing::info!(
                    "[REDB] If this hangs, redb may be performing crash recovery validation"
                );

                use std::time::Instant;
                let start_time = Instant::now();

                // Open the database - this may take time for large databases
                // redb performs checksum validation during open, especially after crashes
                let open_result = builder.open(&db_path);

                let elapsed = start_time.elapsed();
                tracing::info!("[REDB] Database open completed in {:?}", elapsed);

                match open_result {
                    Ok(db) => {
                        tracing::info!(
                            "[REDB] Database opened successfully in {:?}, opening tables...",
                            elapsed
                        );
                        // Database exists and is openable, use it
                        let table_start = Instant::now();
                        let write_txn = db.begin_write()?;
                        {
                            // Open all tables to ensure they exist
                            let _ = write_txn.open_table(BLOCKS_TABLE)?;
                            let _ = write_txn.open_table(HEADERS_TABLE)?;
                            let _ = write_txn.open_table(HEIGHT_INDEX_TABLE)?;
                            let _ = write_txn.open_table(HASH_TO_HEIGHT_TABLE)?;
                            let _ = write_txn.open_table(WITNESSES_TABLE)?;
                            let _ = write_txn.open_table(RECENT_HEADERS_TABLE)?;
                            let _ = write_txn.open_table(UTXOS_TABLE)?;
                            let _ = write_txn.open_table(IBD_UTXOS_TABLE)?;
                            let _ = write_txn.open_table(SPENT_OUTPUTS_TABLE)?;
                            let _ = write_txn.open_table(CHAIN_INFO_TABLE)?;
                            let _ = write_txn.open_table(WORK_CACHE_TABLE)?;
                            let _ = write_txn.open_table(TX_BY_HASH_TABLE)?;
                            let _ = write_txn.open_table(TX_BY_BLOCK_TABLE)?;
                            let _ = write_txn.open_table(TX_METADATA_TABLE)?;
                            let _ = write_txn.open_table(ADDRESS_TX_INDEX_TABLE)?;
                            let _ = write_txn.open_table(ADDRESS_OUTPUT_INDEX_TABLE)?;
                            let _ = write_txn.open_table(ADDRESS_INPUT_INDEX_TABLE)?;
                            let _ = write_txn.open_table(VALUE_INDEX_TABLE)?;
                            // Payment system tables
                            let _ = write_txn.open_table(VAULTS_TABLE)?;
                            let _ = write_txn.open_table(POOLS_TABLE)?;
                            let _ = write_txn.open_table(BATCHES_TABLE)?;
                            // Module storage table
                            let _ = write_txn.open_table(INVALID_BLOCKS_TABLE)?;
                            let _ = write_txn.open_table(CHAIN_TIPS_TABLE)?;
                            let _ = write_txn.open_table(BLOCK_METADATA_TABLE)?;
                            let _ = write_txn.open_table(CHAINWORK_CACHE_TABLE)?;
                            let _ = write_txn.open_table(UTXO_STATS_CACHE_TABLE)?;
                            let _ = write_txn.open_table(NETWORK_HASHRATE_CACHE_TABLE)?;
                            let _ = write_txn.open_table(UTXO_COMMITMENTS_TABLE)?;
                            let _ = write_txn.open_table(COMMITMENT_HEIGHT_INDEX_TABLE)?;
                            // Module DB tables (blvm-sdk open_module_db)
                            let _ = write_txn.open_table(SCHEMA_TABLE)?;
                            let _ = write_txn.open_table(ITEMS_TABLE)?;
                        }
                        write_txn.commit()?;
                        let table_elapsed = table_start.elapsed();
                        tracing::info!("[REDB] Tables opened and committed in {:?}", table_elapsed);
                        db
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[REDB] Failed to open existing database after {:?}: {}",
                            elapsed,
                            e
                        );
                        tracing::warn!("[REDB] Error details: {:?}", e);
                        tracing::info!("[REDB] Creating new database...");
                        builder.create(&db_path)?
                    }
                }
            } else {
                tracing::info!("[REDB] Database doesn't exist, creating new one...");
                // Database doesn't exist, create new one
                builder.create(&db_path)?
            };
            tracing::info!("[REDB] Database created/opened, initializing tables...");

            // Initialize all tables in a write transaction
            tracing::info!("[REDB] Beginning write transaction to initialize tables...");
            let write_txn = db.begin_write()?;
            {
                tracing::info!("[REDB] Opening all tables...");
                // Open all tables to ensure they exist
                let _ = write_txn.open_table(BLOCKS_TABLE)?;
                let _ = write_txn.open_table(HEADERS_TABLE)?;
                let _ = write_txn.open_table(HEIGHT_INDEX_TABLE)?;
                let _ = write_txn.open_table(HASH_TO_HEIGHT_TABLE)?;
                let _ = write_txn.open_table(WITNESSES_TABLE)?;
                let _ = write_txn.open_table(RECENT_HEADERS_TABLE)?;
                let _ = write_txn.open_table(UTXOS_TABLE)?;
                let _ = write_txn.open_table(IBD_UTXOS_TABLE)?;
                let _ = write_txn.open_table(SPENT_OUTPUTS_TABLE)?;
                let _ = write_txn.open_table(CHAIN_INFO_TABLE)?;
                let _ = write_txn.open_table(WORK_CACHE_TABLE)?;
                let _ = write_txn.open_table(TX_BY_HASH_TABLE)?;
                let _ = write_txn.open_table(TX_BY_BLOCK_TABLE)?;
                let _ = write_txn.open_table(TX_METADATA_TABLE)?;
                let _ = write_txn.open_table(ADDRESS_TX_INDEX_TABLE)?;
                let _ = write_txn.open_table(ADDRESS_OUTPUT_INDEX_TABLE)?;
                let _ = write_txn.open_table(ADDRESS_INPUT_INDEX_TABLE)?;
                let _ = write_txn.open_table(VALUE_INDEX_TABLE)?;
                let _ = write_txn.open_table(INVALID_BLOCKS_TABLE)?;
                let _ = write_txn.open_table(CHAIN_TIPS_TABLE)?;
                let _ = write_txn.open_table(BLOCK_METADATA_TABLE)?;
                let _ = write_txn.open_table(CHAINWORK_CACHE_TABLE)?;
                let _ = write_txn.open_table(UTXO_STATS_CACHE_TABLE)?;
                let _ = write_txn.open_table(NETWORK_HASHRATE_CACHE_TABLE)?;
                let _ = write_txn.open_table(UTXO_COMMITMENTS_TABLE)?;
                let _ = write_txn.open_table(COMMITMENT_HEIGHT_INDEX_TABLE)?;
                // Payment system tables
                let _ = write_txn.open_table(VAULTS_TABLE)?;
                let _ = write_txn.open_table(POOLS_TABLE)?;
                let _ = write_txn.open_table(BATCHES_TABLE)?;
                // Module DB tables (blvm-sdk open_module_db)
                let _ = write_txn.open_table(SCHEMA_TABLE)?;
                let _ = write_txn.open_table(ITEMS_TABLE)?;
            }
            write_txn.commit()?;

            Ok(Self { db: Arc::new(db) })
        }

        fn get_table_def(
            &self,
            name: &str,
        ) -> Option<&'static TableDefinition<'static, &'static [u8], &'static [u8]>> {
            match name {
                "blocks" => Some(&BLOCKS_TABLE),
                "headers" => Some(&HEADERS_TABLE),
                "height_index" => Some(&HEIGHT_INDEX_TABLE),
                "hash_to_height" => Some(&HASH_TO_HEIGHT_TABLE),
                "witnesses" => Some(&WITNESSES_TABLE),
                "recent_headers" => Some(&RECENT_HEADERS_TABLE),
                "utxos" => Some(&UTXOS_TABLE),
                "ibd_utxos" => Some(&IBD_UTXOS_TABLE),
                "spent_outputs" => Some(&SPENT_OUTPUTS_TABLE),
                "chain_info" => Some(&CHAIN_INFO_TABLE),
                "work_cache" => Some(&WORK_CACHE_TABLE),
                "tx_by_hash" => Some(&TX_BY_HASH_TABLE),
                "tx_by_block" => Some(&TX_BY_BLOCK_TABLE),
                "tx_metadata" => Some(&TX_METADATA_TABLE),
                "address_tx_index" => Some(&ADDRESS_TX_INDEX_TABLE),
                "address_output_index" => Some(&ADDRESS_OUTPUT_INDEX_TABLE),
                "address_input_index" => Some(&ADDRESS_INPUT_INDEX_TABLE),
                "value_index" => Some(&VALUE_INDEX_TABLE),
                "invalid_blocks" => Some(&INVALID_BLOCKS_TABLE),
                "chain_tips" => Some(&CHAIN_TIPS_TABLE),
                "block_metadata" => Some(&BLOCK_METADATA_TABLE),
                "chainwork_cache" => Some(&CHAINWORK_CACHE_TABLE),
                "utxo_stats_cache" => Some(&UTXO_STATS_CACHE_TABLE),
                "network_hashrate_cache" => Some(&NETWORK_HASHRATE_CACHE_TABLE),
                "utxo_commitments" => Some(&UTXO_COMMITMENTS_TABLE),
                "commitment_height_index" => Some(&COMMITMENT_HEIGHT_INDEX_TABLE),
                // Payment system tables
                "vaults" => Some(&VAULTS_TABLE),
                "pools" => Some(&POOLS_TABLE),
                "batches" => Some(&BATCHES_TABLE),
                // Module DB tables (blvm-sdk open_module_db)
                "schema" => Some(&SCHEMA_TABLE),
                "items" => Some(&ITEMS_TABLE),
                _ => None,
            }
        }

        /// Single Redb write transaction for one parallel IBD block batch: all blockstore tables plus
        /// recent-header MTP rows. Matches per-tree `commit_no_wal` semantics in `parallel_ibd`.
        pub(crate) fn write_ibd_blockstore_flush_no_wal(
            &self,
            flush_order: &[usize],
            heights: &[u64],
            block_hashes: &[blvm_protocol::Hash],
            block_data: &[Vec<u8>],
            header_data: &[std::sync::Arc<Vec<u8>>],
            witness_blobs: &[Option<Vec<u8>>],
            metadata_blobs: &[Vec<u8>],
            recent_entries: &[(u64, Vec<u8>)],
        ) -> Result<()> {
            use crate::storage::blockstore::block_height_row_key;

            let n = flush_order.len();
            let mut blocks_ops: Vec<(
                [u8; crate::storage::blockstore::BLOCK_HEIGHT_ROW_KEY_LEN],
                usize,
            )> = Vec::with_capacity(n);
            let mut headers_ops: Vec<(
                [u8; crate::storage::blockstore::BLOCK_HEIGHT_ROW_KEY_LEN],
                usize,
            )> = Vec::with_capacity(n);
            let mut witness_ops: Vec<(
                [u8; crate::storage::blockstore::BLOCK_HEIGHT_ROW_KEY_LEN],
                usize,
            )> = Vec::new();
            let mut height_ops: Vec<([u8; 8], usize)> = Vec::with_capacity(n);
            let mut h2h_ops: Vec<(usize, [u8; 8])> = Vec::with_capacity(n);
            let mut meta_ops: Vec<(
                [u8; crate::storage::blockstore::BLOCK_HEIGHT_ROW_KEY_LEN],
                usize,
            )> = Vec::with_capacity(n);

            for &i in flush_order {
                let height = heights[i];
                let key = block_height_row_key(height, &block_hashes[i]);
                blocks_ops.push((key, i));
                headers_ops.push((key, i));
                if witness_blobs[i].is_some() {
                    witness_ops.push((key, i));
                }
                height_ops.push((height.to_be_bytes(), i));
                h2h_ops.push((i, height.to_be_bytes()));
                meta_ops.push((key, i));
            }

            let write_txn = self.db.begin_write()?;
            {
                {
                    let mut t = write_txn.open_table(BLOCKS_TABLE)?;
                    for (key, i) in blocks_ops {
                        t.insert(key.as_slice(), block_data[i].as_slice())?;
                    }
                }
                {
                    let mut t = write_txn.open_table(HEADERS_TABLE)?;
                    for (key, i) in headers_ops {
                        t.insert(key.as_slice(), header_data[i].as_slice())?;
                    }
                }
                if !witness_ops.is_empty() {
                    let mut t = write_txn.open_table(WITNESSES_TABLE)?;
                    for (key, i) in witness_ops {
                        let w = witness_blobs[i].as_ref().ok_or_else(|| {
                            anyhow::anyhow!("IBD Redb flush: witness_ops index missing blob")
                        })?;
                        t.insert(key.as_slice(), w.as_slice())?;
                    }
                }
                {
                    let mut t = write_txn.open_table(HEIGHT_INDEX_TABLE)?;
                    for (height_key, i) in height_ops {
                        t.insert(height_key.as_slice(), block_hashes[i].as_slice())?;
                    }
                }
                {
                    let mut t = write_txn.open_table(HASH_TO_HEIGHT_TABLE)?;
                    for (i, height_key) in h2h_ops {
                        t.insert(block_hashes[i].as_slice(), height_key.as_slice())?;
                    }
                }
                {
                    let mut t = write_txn.open_table(BLOCK_METADATA_TABLE)?;
                    for (key, i) in meta_ops {
                        t.insert(key.as_slice(), metadata_blobs[i].as_slice())?;
                    }
                }
                {
                    let mut t = write_txn.open_table(RECENT_HEADERS_TABLE)?;
                    for &(height, ref header_bytes) in recent_entries {
                        let height_bytes = height.to_be_bytes();
                        t.insert(height_bytes.as_slice(), header_bytes.as_slice())?;
                        if height > 11 {
                            let rm = (height - 12).to_be_bytes();
                            let _ = t.remove(rm.as_slice());
                        }
                    }
                }
            }
            write_txn.commit()?;
            Ok(())
        }
    }

    impl Database for RedbDatabase {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>> {
            if name.starts_with("module_") || name == "modules" {
                return Err(anyhow::anyhow!(
                    "Module storage has been removed. Use blvm_sdk::module::open_module_db."
                ));
            }

            // Existing static table logic
            let table_def = self.get_table_def(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "Unknown table name: {}. Redb requires pre-defined tables.",
                    name
                )
            })?;

            Ok(Box::new(RedbTree {
                db: Arc::clone(&self.db),
                table_def,
                name: name.to_string(),
            }))
        }

        fn flush(&self) -> Result<()> {
            // Redb flushes automatically on transaction commit
            // For explicit flush, we can trigger a write transaction
            let write_txn = self.db.begin_write()?;
            write_txn.commit()?;
            Ok(())
        }
    }

    struct RedbTree {
        db: Arc<RedbDb>,
        table_def: &'static TableDefinition<'static, &'static [u8], &'static [u8]>,
        name: String,
    }

    impl Tree for RedbTree {
        fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
            let write_txn = self.db.begin_write()?;
            {
                let mut table = write_txn.open_table(*self.table_def)?;
                table.insert(key, value)?;
            }
            write_txn.commit()?;
            Ok(())
        }

        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            let read_txn = self.db.begin_read()?;
            let table = read_txn.open_table(*self.table_def)?;
            let result = table.get(key)?.map(|v| v.value().to_vec());
            Ok(result)
        }

        fn remove(&self, key: &[u8]) -> Result<()> {
            let write_txn = self.db.begin_write()?;
            {
                let mut table = write_txn.open_table(*self.table_def)?;
                table.remove(key)?;
            }
            write_txn.commit()?;
            Ok(())
        }

        fn contains_key(&self, key: &[u8]) -> Result<bool> {
            let read_txn = self.db.begin_read()?;
            let table = read_txn.open_table(*self.table_def)?;
            let result = table.get(key)?.is_some();
            Ok(result)
        }

        fn clear(&self) -> Result<()> {
            // Redb clear implementation: delete all entries in a write transaction
            // We need to collect keys in a read transaction first, then delete in write transaction
            let keys: Vec<Vec<u8>> = {
                let read_txn = self.db.begin_read()?;
                let table = read_txn.open_table(*self.table_def)?;
                let mut collected_keys = Vec::new();
                // Collect all keys from the iterator
                match table.range::<&[u8]>(..) {
                    Ok(range_iter) => {
                        for item_result in range_iter {
                            match item_result {
                                Ok((key, _)) => {
                                    collected_keys.push(key.value().to_vec());
                                }
                                Err(e) => {
                                    return Err(anyhow::anyhow!("Redb iteration error: {}", e));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to create range: {}", e));
                    }
                }
                collected_keys
            };

            // Delete all keys in write transaction
            if !keys.is_empty() {
                let write_txn = self.db.begin_write()?;
                {
                    let mut table = write_txn.open_table(*self.table_def)?;
                    for key in keys {
                        // Remove using key as &[u8] (same API as remove() method above)
                        let _ = table.remove(key.as_slice());
                    }
                }
                write_txn.commit()?;
            }
            Ok(())
        }

        fn len(&self) -> Result<usize> {
            let read_txn = self.db.begin_read()?;
            let table = read_txn.open_table(*self.table_def)?;
            Ok(table.len()? as usize)
        }

        fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
            // Redb iteration requires a read transaction
            // We need to collect all items into a vector since the transaction must outlive the iterator
            let read_txn = match self.db.begin_read() {
                Ok(txn) => txn,
                Err(e) => {
                    return Box::new(std::iter::once(Err(anyhow::anyhow!(
                        "Failed to begin read transaction: {}",
                        e
                    ))));
                }
            };

            let table = match read_txn.open_table(*self.table_def) {
                Ok(tbl) => tbl,
                Err(e) => {
                    return Box::new(std::iter::once(Err(anyhow::anyhow!(
                        "Failed to open table: {}",
                        e
                    ))));
                }
            };

            // Collect all items into a vector
            // Redb Range implements IntoIterator, but we need to collect into a vector
            // because the read transaction must outlive the iterator
            let mut items = Vec::new();
            // Redb's range() returns a Result<Range, Error>
            // Each iteration over the Range yields a Result<(Key, Value), Error>
            // Use turbofish syntax to specify the type parameter for the range bounds
            match table.range::<&[u8]>(..) {
                Ok(range_iter) => {
                    for item_result in range_iter {
                        match item_result {
                            Ok((key, value)) => {
                                items.push(Ok((key.value().to_vec(), value.value().to_vec())));
                            }
                            Err(e) => {
                                items.push(Err(anyhow::anyhow!("Redb iteration error: {}", e)));
                            }
                        }
                    }
                }
                Err(e) => {
                    items.push(Err(anyhow::anyhow!("Failed to create range: {}", e)));
                }
            }

            Box::new(items.into_iter())
        }

        fn batch(&self) -> Result<Box<dyn BatchWriter + '_>> {
            Ok(Box::new(RedbBatchWriter {
                db: Arc::clone(&self.db),
                table_def: self.table_def,
                pending: Vec::new(),
            }))
        }
    }

    /// Redb batch writer - buffers operations and commits in single transaction
    ///
    /// This is the key optimization for IBD: instead of one transaction per insert,
    /// we buffer all operations and commit them atomically in a single transaction.
    struct RedbBatchWriter {
        db: Arc<RedbDb>,
        table_def: &'static TableDefinition<'static, &'static [u8], &'static [u8]>,
        /// Pending operations: (key, Some(value)) for put, (key, None) for delete
        pending: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    }

    impl BatchWriter for RedbBatchWriter {
        fn put(&mut self, key: &[u8], value: &[u8]) {
            self.pending.push((key.to_vec(), Some(value.to_vec())));
        }

        fn delete(&mut self, key: &[u8]) {
            self.pending.push((key.to_vec(), None));
        }

        fn commit(self: Box<Self>) -> Result<()> {
            if self.pending.is_empty() {
                return Ok(());
            }

            // Single write transaction for all operations
            let write_txn = self.db.begin_write()?;
            {
                let mut table = write_txn.open_table(*self.table_def)?;
                for (key, value) in self.pending {
                    match value {
                        Some(v) => {
                            table.insert(key.as_slice(), v.as_slice())?;
                        }
                        None => {
                            let _ = table.remove(key.as_slice());
                        }
                    }
                }
            }
            write_txn.commit()?;
            Ok(())
        }

        fn len(&self) -> usize {
            self.pending.len()
        }
    }
}

// RocksDB implementation
#[cfg(feature = "rocksdb")]
pub mod rocksdb_impl {
    use super::{BatchWriter, Database, Tree};
    use anyhow::Result;
    use rocksdb::{
        BlockBasedOptions, Cache, ColumnFamily, ColumnFamilyDescriptor, Options, WriteOptions, DB,
    };
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    pub struct RocksDBDatabase {
        cache: std::sync::Mutex<Option<Cache>>,
        cache_nominal_bytes: usize,
        db: Arc<DB>,
    }

    impl RocksDBDatabase {
        /// Create a new RocksDB database
        pub fn new<P: AsRef<Path>>(
            data_dir: P,
            storage_config: Option<&crate::config::StorageConfig>,
        ) -> Result<Self> {
            let db_path = data_dir.as_ref().join("rocksdb");
            let mut opts = Options::default();
            opts.create_if_missing(true);
            opts.create_missing_column_families(true);

            // Detect system RAM first so every tunable below can scale with it.
            // /proc/meminfo gives kB; round to nearest GB.
            let total_ram_gb: u64 = {
                #[cfg(target_os = "linux")]
                {
                    std::fs::read_to_string("/proc/meminfo")
                        .ok()
                        .and_then(|s| {
                            s.lines()
                                .find(|l| l.starts_with("MemTotal:"))
                                .and_then(|l| l.split_whitespace().nth(1))
                                .and_then(|v| v.parse::<u64>().ok())
                        })
                        .map(|kb| (kb / 1024 + 512) / 1024)
                        .unwrap_or(16)
                }
                #[cfg(not(target_os = "linux"))]
                {
                    std::env::var("BLVM_RAM_GB")
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(16)
                }
            };

            let default_parallelism = std::thread::available_parallelism()
                .map(|p| p.get().max(4) as i32)
                .unwrap_or(4);
            let parallelism: i32 = storage_config
                .and_then(|s| s.rocksdb.as_ref())
                .and_then(|r| r.parallelism)
                .or_else(|| {
                    std::env::var("BLVM_ROCKSDB_PARALLELISM")
                        .ok()
                        .and_then(|s| s.parse().ok())
                })
                .unwrap_or(default_parallelism);
            opts.increase_parallelism(parallelism);
            let max_open = if total_ram_gb >= 32 {
                256
            } else if total_ram_gb >= 24 {
                192
            } else {
                64
            };
            opts.set_max_open_files(max_open);

            // Each compaction thread holds ~64MB of input/output buffers in memory.
            // 16 GB: 1+1=2 bg jobs to keep ~64 MB in compaction instead of ~128 MB.
            let default_compactions = if total_ram_gb >= 32 {
                4
            } else if total_ram_gb >= 24 {
                2
            } else {
                1
            };
            let default_flushes = if total_ram_gb >= 32 {
                4
            } else if total_ram_gb >= 24 {
                2
            } else {
                1
            };
            let rocksdb_cfg = storage_config.and_then(|s| s.rocksdb.as_ref());
            let max_compactions: i32 = rocksdb_cfg
                .and_then(|r| r.max_background_compactions)
                .or_else(|| {
                    std::env::var("BLVM_ROCKSDB_MAX_BACKGROUND_COMPACTIONS")
                        .ok()
                        .and_then(|s| s.parse().ok())
                })
                .unwrap_or(default_compactions);
            let max_flushes: i32 = rocksdb_cfg
                .and_then(|r| r.max_background_flushes)
                .or_else(|| {
                    std::env::var("BLVM_ROCKSDB_MAX_BACKGROUND_FLUSHES")
                        .ok()
                        .and_then(|s| s.parse().ok())
                })
                .unwrap_or(default_flushes);
            let level0_trigger: i32 = rocksdb_cfg
                .map(|r| r.level0_compaction_trigger)
                .or_else(|| {
                    std::env::var("BLVM_ROCKSDB_LEVEL0_COMPACTION_TRIGGER")
                        .ok()
                        .and_then(|s| s.parse().ok())
                })
                .unwrap_or(8);
            // RocksDB uses max_background_jobs; it allocates between flushes and compactions
            opts.set_max_background_jobs(max_compactions + max_flushes);
            opts.set_level_zero_file_num_compaction_trigger(level0_trigger);
            let max_subcompactions: u32 = std::env::var("BLVM_ROCKSDB_MAX_SUBCOMPACTIONS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| {
                    if total_ram_gb >= 32 {
                        4
                    } else if total_ram_gb >= 24 {
                        3
                    } else {
                        2
                    }
                })
                .clamp(1, 64);
            opts.set_max_subcompactions(max_subcompactions);
            if let Ok(bps) = std::env::var("BLVM_ROCKSDB_BYTES_PER_SYNC") {
                if let Ok(n) = bps.parse::<u64>() {
                    if n > 0 {
                        opts.set_bytes_per_sync(n);
                    }
                }
            }
            let default_write_buffer = if total_ram_gb >= 32 {
                256
            } else if total_ram_gb >= 24 {
                192
            } else if total_ram_gb >= 16 {
                16
            } else {
                8
            };
            let default_block_cache = if total_ram_gb >= 32 {
                512
            } else if total_ram_gb >= 24 {
                384
            } else if total_ram_gb >= 16 {
                32
            } else {
                16
            };

            // Precedence: config > ENV > default
            let db_write_buffer_mb: usize = rocksdb_cfg
                .and_then(|r| r.write_buffer_mb)
                .or_else(|| {
                    std::env::var("BLVM_ROCKSDB_WRITE_BUFFER_MB")
                        .ok()
                        .and_then(|s| s.parse().ok())
                })
                .unwrap_or(default_write_buffer);
            opts.set_db_write_buffer_size(db_write_buffer_mb * 1024 * 1024);

            tracing::info!("[ROCKSDB] parallelism={} max_compactions={} max_flushes={} level0_trigger={} write_buffer={}MB (ram={}GB)",
                parallelism, max_compactions, max_flushes, level0_trigger, db_write_buffer_mb, total_ram_gb);

            // Block cache: ENV > config (capped to RAM-tier) > RAM-tiered default.
            let dbcache_mb: usize = std::env::var("BLVM_DBCACHE_MB")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| {
                    let from_config = storage_config.map(|s| s.dbcache_mb).unwrap_or(0);
                    if from_config > 0 {
                        from_config.min(default_block_cache)
                    } else {
                        default_block_cache
                    }
                });
            let dbcache_bytes = dbcache_mb.saturating_mul(1024).saturating_mul(1024);
            let cache = Cache::new_lru_cache(dbcache_bytes);
            let mut block_opts = BlockBasedOptions::default();
            block_opts.set_block_cache(&cache);
            block_opts.set_cache_index_and_filter_blocks(true);
            block_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);
            opts.set_block_based_table_factory(&block_opts);
            tracing::info!(
                "[ROCKSDB] block_cache={}MB (ram={}GB)",
                dbcache_mb,
                total_ram_gb
            );

            // WriteBufferManager: hard cap on total memtable memory across ALL CFs.
            // allow_stall=true blocks writes instead of exceeding the cap.
            let wbm_mb: usize = if total_ram_gb >= 32 {
                512
            } else if total_ram_gb >= 24 {
                384
            } else if total_ram_gb >= 16 {
                48
            } else {
                32
            };
            let wbm =
                rocksdb::WriteBufferManager::new_write_buffer_manager(wbm_mb * 1024 * 1024, true);
            opts.set_write_buffer_manager(&wbm);
            tracing::info!(
                "[ROCKSDB] WriteBufferManager: {}MB memtable cap (allow_stall=true)",
                wbm_mb
            );

            // Dedicated block cache for ibd_utxos/utxos CFs.
            // The shared `cache` above covers all other CFs. A separate UTXO cache prevents
            // block and header reads from evicting hot UTXO SST blocks during IBD.
            // At h=270k the UTXO SST is several GB; the shared 32 MB cache has ~0% hit rate.
            // A 128 MB dedicated cache on 16 GB covers more of the recently-written UTXO SST
            // blocks, reducing multi_get SSD round-trips in the prefetch workers.
            let utxo_block_cache_mb: usize = std::env::var("BLVM_ROCKSDB_UTXO_BLOCK_CACHE_MB")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&n| n > 0 && n <= 16_384)
                .unwrap_or_else(|| {
                    if total_ram_gb >= 64 {
                        2048
                    } else if total_ram_gb >= 32 {
                        1024
                    } else if total_ram_gb >= 24 {
                        384
                    } else if total_ram_gb >= 16 {
                        128
                    } else {
                        64
                    }
                });
            let utxo_cache = Cache::new_lru_cache(utxo_block_cache_mb * 1024 * 1024);
            tracing::info!(
                "[ROCKSDB] utxo_block_cache={}MB (dedicated, ram={}GB)",
                utxo_block_cache_mb,
                total_ram_gb
            );

            // Per-CF options for IBD-hot column families.
            let make_utxo_cf_opts = |uc: &Cache| -> Options {
                let mut o = Options::default();
                let mut bbo = BlockBasedOptions::default();
                bbo.set_bloom_filter(10.0, false);
                bbo.set_block_cache(uc);
                bbo.set_cache_index_and_filter_blocks(true);
                bbo.set_pin_l0_filter_and_index_blocks_in_cache(true);
                o.set_block_based_table_factory(&bbo);
                let wb = if total_ram_gb >= 32 {
                    256
                } else if total_ram_gb >= 24 {
                    128
                } else if total_ram_gb >= 16 {
                    8
                } else {
                    4
                };
                o.set_write_buffer_size(wb * 1024 * 1024);
                o.set_max_write_buffer_number(2);
                o.set_level_zero_file_num_compaction_trigger(12);
                o.set_target_file_size_base(
                    if total_ram_gb >= 32 { 128 } else { 64 } * 1024 * 1024,
                );
                o.set_compression_type(rocksdb::DBCompressionType::Zstd);
                o.set_bottommost_compression_type(rocksdb::DBCompressionType::Zstd);
                o
            };
            let make_bulk_cf_opts = |cache: &Cache| -> Options {
                let mut o = Options::default();
                let mut bbo = BlockBasedOptions::default();
                bbo.set_block_cache(cache);
                bbo.set_cache_index_and_filter_blocks(true);
                o.set_block_based_table_factory(&bbo);
                let wb = if total_ram_gb >= 32 {
                    64
                } else if total_ram_gb >= 24 {
                    32
                } else if total_ram_gb >= 16 {
                    8
                } else {
                    4
                };
                o.set_write_buffer_size(wb * 1024 * 1024);
                o.set_level_zero_file_num_compaction_trigger(12);
                o.set_compression_type(rocksdb::DBCompressionType::Zstd);
                o.set_bottommost_compression_type(rocksdb::DBCompressionType::Zstd);
                o
            };
            let cf_opts_for = |name: &str| -> Options {
                match name {
                    "ibd_utxos" | "utxos" => make_utxo_cf_opts(&utxo_cache),
                    "blocks" | "headers" | "witnesses" | "height_index" => {
                        make_bulk_cf_opts(&cache)
                    }
                    _ => Options::default(),
                }
            };
            tracing::info!("[ROCKSDB] per-CF: ibd_utxos/utxos bloom+zstd+dedicated_cache, blocks/headers/witnesses zstd, WAL disabled for IBD");

            let mut cfs = vec![ColumnFamilyDescriptor::new("default", Options::default())];
            cfs.extend(
                super::KNOWN_TREE_NAMES
                    .iter()
                    .map(|n| ColumnFamilyDescriptor::new(*n, cf_opts_for(n))),
            );

            let db = if db_path.exists() {
                let known: std::collections::HashSet<_> = ["default"]
                    .iter()
                    .chain(super::KNOWN_TREE_NAMES)
                    .map(|s| (*s).to_string())
                    .collect();
                let cf_descriptors: Vec<ColumnFamilyDescriptor> = cfs
                    .into_iter()
                    .chain(
                        rocksdb::DB::list_cf(&opts, &db_path)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|name| !known.contains(name))
                            .map(|name| {
                                let o = cf_opts_for(&name);
                                ColumnFamilyDescriptor::new(name, o)
                            }),
                    )
                    .collect();
                DB::open_cf_descriptors(&opts, &db_path, cf_descriptors)?
            } else {
                DB::open_cf_descriptors(&opts, &db_path, cfs)?
            };

            Ok(Self {
                cache: std::sync::Mutex::new(Some(cache)),
                cache_nominal_bytes: dbcache_bytes,
                db: Arc::new(db),
            })
        }

        /// Open RocksDB with LevelDB format
        ///
        /// Opens an existing chainstate database (LevelDB format).
        /// RocksDB can read LevelDB databases directly (backward compatible).
        pub fn open_bitcoin_core<P: AsRef<Path>>(data_dir: P) -> Result<Self> {
            // Open existing chainstate database (LevelDB format)
            // RocksDB can read LevelDB databases directly
            let chainstate_path = data_dir.as_ref().join("chainstate");

            let mut opts = Options::default();
            opts.create_if_missing(false); // Don't create, must exist

            // RocksDB will automatically detect LevelDB format
            // Note: LevelDB uses a single "default" column family
            let cfs = vec![ColumnFamilyDescriptor::new("default", Options::default())];

            let db = DB::open_cf_descriptors(&opts, &chainstate_path, cfs)?;

            Ok(Self {
                cache: std::sync::Mutex::new(None),
                cache_nominal_bytes: 0,
                db: Arc::new(db),
            })
        }
    }

    impl RocksDBDatabase {
        /// One cross-CF `WriteBatch` + `write_opt` (no WAL) for parallel IBD block flush.
        /// Semantics match separate per-tree `commit_no_wal` batches in `parallel_ibd::do_flush_to_storage`.
        pub(crate) fn write_ibd_blockstore_flush_no_wal(
            &self,
            flush_order: &[usize],
            heights: &[u64],
            block_hashes: &[blvm_protocol::Hash],
            block_data: &[Vec<u8>],
            header_data: &[std::sync::Arc<Vec<u8>>],
            witness_blobs: &[Option<Vec<u8>>],
            metadata_blobs: &[Vec<u8>],
            recent_entries: &[(u64, Vec<u8>)],
        ) -> Result<()> {
            use crate::storage::blockstore::block_height_row_key;

            let cf_blocks = self
                .db
                .cf_handle("blocks")
                .ok_or_else(|| anyhow::anyhow!("RocksDB column family \"blocks\" not found"))?;
            let cf_headers = self
                .db
                .cf_handle("headers")
                .ok_or_else(|| anyhow::anyhow!("RocksDB column family \"headers\" not found"))?;
            let cf_witnesses = self
                .db
                .cf_handle("witnesses")
                .ok_or_else(|| anyhow::anyhow!("RocksDB column family \"witnesses\" not found"))?;
            let cf_height = self.db.cf_handle("height_index").ok_or_else(|| {
                anyhow::anyhow!("RocksDB column family \"height_index\" not found")
            })?;
            let cf_h2h = self.db.cf_handle("hash_to_height").ok_or_else(|| {
                anyhow::anyhow!("RocksDB column family \"hash_to_height\" not found")
            })?;
            let cf_meta = self.db.cf_handle("block_metadata").ok_or_else(|| {
                anyhow::anyhow!("RocksDB column family \"block_metadata\" not found")
            })?;
            let cf_recent = self.db.cf_handle("recent_headers").ok_or_else(|| {
                anyhow::anyhow!("RocksDB column family \"recent_headers\" not found")
            })?;

            let mut batch = rocksdb::WriteBatch::default();

            for &i in flush_order {
                let height = heights[i];
                let key = block_height_row_key(height, &block_hashes[i]);
                batch.put_cf(cf_blocks, key, &block_data[i]);
                batch.put_cf(cf_headers, key, header_data[i].as_slice());
                if let Some(w) = witness_blobs[i].as_ref() {
                    batch.put_cf(cf_witnesses, key, w.as_slice());
                }
                let height_key = height.to_be_bytes();
                batch.put_cf(cf_height, height_key, block_hashes[i]);
                batch.put_cf(cf_h2h, block_hashes[i], height_key);
                batch.put_cf(cf_meta, key, &metadata_blobs[i]);
            }

            for &(height, ref header_bytes) in recent_entries {
                let height_bytes = height.to_be_bytes();
                batch.put_cf(cf_recent, height_bytes, header_bytes.as_slice());
                if height > 11 {
                    let rm = (height - 12).to_be_bytes();
                    batch.delete_cf(cf_recent, rm);
                }
            }

            let mut wo = WriteOptions::default();
            wo.set_sync(false);
            wo.disable_wal(true);
            self.db.write_opt(batch, &wo)?;
            Ok(())
        }
    }

    impl Database for RocksDBDatabase {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn ibd_memory_pressure_tick(&self, level_u8: u8) {
            static THROTTLED: AtomicBool = AtomicBool::new(false);
            static CACHE_SHRUNK: AtomicBool = AtomicBool::new(false);

            let critical_plus = level_u8 >= 2;
            let emergency = level_u8 >= 3;
            let throttled = THROTTLED.load(Ordering::Relaxed);

            if emergency && !throttled {
                if let Ok(()) = self.db.set_options(&[("max_background_jobs", "1")]) {
                    self.db.cancel_all_background_work(false);
                    THROTTLED.store(true, Ordering::Relaxed);
                    tracing::warn!(
                        "[ROCKSDB] EMERGENCY: max_background_jobs -> 1, cancelled pending bg work"
                    );
                }
            } else if critical_plus && !throttled {
                if let Ok(()) = self.db.set_options(&[("max_background_jobs", "1")]) {
                    THROTTLED.store(true, Ordering::Relaxed);
                    tracing::warn!("[ROCKSDB] IBD: max_background_jobs -> 1 under Critical+");
                }
            } else if !critical_plus && throttled {
                if let Ok(()) = self.db.set_options(&[("max_background_jobs", "2")]) {
                    THROTTLED.store(false, Ordering::Relaxed);
                    tracing::info!("[ROCKSDB] IBD: max_background_jobs restored to 2");
                }
            }

            let cache_is_shrunk = CACHE_SHRUNK.load(Ordering::Relaxed);
            if emergency && !cache_is_shrunk {
                if let Ok(mut guard) = self.cache.lock() {
                    if let Some(c) = guard.as_mut() {
                        c.set_capacity(8 * 1024 * 1024);
                        CACHE_SHRUNK.store(true, Ordering::Relaxed);
                        tracing::warn!(
                            "[ROCKSDB] EMERGENCY: block_cache shrunk to 8MB (was {}MB)",
                            self.cache_nominal_bytes / (1024 * 1024)
                        );
                    }
                }
            } else if !emergency && cache_is_shrunk {
                if let Ok(mut guard) = self.cache.lock() {
                    if let Some(c) = guard.as_mut() {
                        c.set_capacity(self.cache_nominal_bytes);
                        CACHE_SHRUNK.store(false, Ordering::Relaxed);
                        tracing::info!(
                            "[ROCKSDB] block_cache restored to {}MB",
                            self.cache_nominal_bytes / (1024 * 1024)
                        );
                    }
                }
            }
        }

        fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>> {
            if name.starts_with("module_") || name == "modules" {
                return Err(anyhow::anyhow!(
                    "Module storage has been removed. Use blvm_sdk::module::open_module_db."
                ));
            }

            // Known trees use pre-created CF. Arc<DB> can't provide &mut for create_cf.
            let _ = self.db.cf_handle(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "Column family {} not found; RocksDB requires pre-creation at open time",
                    name
                )
            })?;

            Ok(Box::new(RocksDBTree {
                db: Arc::clone(&self.db),
                cf_name: name.to_string(),
            }))
        }

        fn flush(&self) -> Result<()> {
            self.db.flush()?;
            Ok(())
        }
    }

    struct RocksDBTree {
        db: Arc<DB>,
        cf_name: String,
    }

    impl RocksDBTree {
        fn cf(&self) -> Result<&ColumnFamily> {
            self.db
                .cf_handle(&self.cf_name)
                .ok_or_else(|| anyhow::anyhow!("Column family {} not found", self.cf_name))
        }
    }

    impl Tree for RocksDBTree {
        fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
            self.db.put_cf(self.cf()?, key, value)?;
            Ok(())
        }

        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            Ok(self.db.get_cf(self.cf()?, key)?.map(|v| v.to_vec()))
        }

        fn get_many(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>> {
            if keys.is_empty() {
                return Ok(Vec::new());
            }
            let cf = self.cf()?;
            let mut pairs = Vec::with_capacity(keys.len());
            pairs.extend(keys.iter().map(|k| (cf, *k)));
            let raw = self.db.multi_get_cf(pairs);
            let mut results = Vec::with_capacity(raw.len());
            for r in raw {
                results.push(r.map_err(|e| anyhow::anyhow!("RocksDB multi_get: {}", e))?);
            }
            Ok(results)
        }

        fn remove(&self, key: &[u8]) -> Result<()> {
            self.db.delete_cf(self.cf()?, key)?;
            Ok(())
        }

        fn contains_key(&self, key: &[u8]) -> Result<bool> {
            Ok(self.db.get_cf(self.cf()?, key)?.is_some())
        }

        fn flush_to_disk(&self) -> Result<()> {
            self.db
                .flush_cf(self.cf()?)
                .map_err(|e| anyhow::anyhow!("RocksDB flush_cf failed: {}", e))
        }

        fn clear(&self) -> Result<()> {
            let cf = self.cf()?;
            let mut iter = self.db.iterator_cf(cf, rocksdb::IteratorMode::Start);
            let mut batch = rocksdb::WriteBatch::default();
            for item in iter {
                let (key, _) = item?;
                batch.delete_cf(cf, &key);
            }
            self.db.write(batch)?;
            Ok(())
        }

        fn len(&self) -> Result<usize> {
            let mut count = 0;
            let iter = self
                .db
                .iterator_cf(self.cf()?, rocksdb::IteratorMode::Start);
            for item in iter {
                let _ = item?;
                count += 1;
            }
            Ok(count)
        }

        fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
            let cf = match self.cf() {
                Ok(c) => c,
                Err(e) => return Box::new(std::iter::once(Err(e))),
            };
            let iter = self.db.iterator_cf(cf, rocksdb::IteratorMode::Start);
            let items: Vec<_> = iter
                .map(|item| {
                    item.map(|(k, v)| (k.to_vec(), v.to_vec()))
                        .map_err(|e| anyhow::anyhow!("RocksDB iteration error: {}", e))
                })
                .collect();
            Box::new(items.into_iter())
        }

        fn batch(&self) -> Result<Box<dyn BatchWriter + '_>> {
            let cf = self.cf().map_err(|e| {
                anyhow::anyhow!(
                    "Column family '{}' not found; RocksDB schema may be corrupted or mismatched: {}",
                    self.cf_name,
                    e
                )
            })?;
            Ok(Box::new(RocksDBBatchWriter {
                db: Arc::clone(&self.db),
                cf,
                batch: rocksdb::WriteBatch::default(),
                op_count: 0,
            }))
        }
    }

    /// RocksDB batch writer using native WriteBatch
    ///
    /// RocksDB's WriteBatch is highly optimized for bulk operations.
    /// CF handle is cached at batch creation to avoid repeated lookups and panic-on-missing.
    struct RocksDBBatchWriter<'a> {
        db: Arc<DB>,
        cf: &'a ColumnFamily,
        batch: rocksdb::WriteBatch,
        op_count: usize,
    }

    impl BatchWriter for RocksDBBatchWriter<'_> {
        fn put(&mut self, key: &[u8], value: &[u8]) {
            self.batch.put_cf(self.cf, key, value);
            self.op_count += 1;
        }

        fn delete(&mut self, key: &[u8]) {
            self.batch.delete_cf(self.cf, key);
            self.op_count += 1;
        }

        fn commit(self: Box<Self>) -> Result<()> {
            self.db.write(self.batch)?;
            Ok(())
        }

        fn commit_no_wal(self: Box<Self>) -> Result<()> {
            let mut wo = WriteOptions::default();
            wo.set_sync(false);
            wo.disable_wal(true);
            self.db.write_opt(self.batch, &wo)?;
            Ok(())
        }

        fn len(&self) -> usize {
            self.op_count
        }
    }
}

// TidesDB implementation
#[cfg(feature = "tidesdb")]
pub(crate) mod tidesdb_impl {
    use super::{BatchWriter, Database, Tree};
    use anyhow::Result;
    use std::path::Path;
    use std::sync::Arc;
    use tidesdb::{ColumnFamilyConfig, CompressionAlgorithm, Config, LogLevel, SyncMode, TidesDB};

    pub struct TidesDBDatabase {
        db: Arc<TidesDB>,
        tidesdb_config: Option<crate::config::TidesDBConfig>,
    }

    impl TidesDBDatabase {
        pub fn new<P: AsRef<Path>>(
            data_dir: P,
            tidesdb_config: Option<&crate::config::TidesDBConfig>,
        ) -> Result<Self> {
            let db_path = data_dir.as_ref().join("tidesdb");
            std::fs::create_dir_all(&db_path)?;

            let dbcache_mb: usize = std::env::var("BLVM_DBCACHE_MB")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(450);
            let dbcache_bytes = dbcache_mb.saturating_mul(1024).saturating_mul(1024);

            // ENV > config > defaults
            let flush_threads: i32 = std::env::var("BLVM_TIDESDB_FLUSH_THREADS")
                .ok()
                .and_then(|s| s.parse().ok())
                .or_else(|| tidesdb_config.map(|c| c.flush_threads))
                .unwrap_or(4);
            let compact_threads: i32 = std::env::var("BLVM_TIDESDB_COMPACT_THREADS")
                .ok()
                .and_then(|s| s.parse().ok())
                .or_else(|| tidesdb_config.map(|c| c.compact_threads))
                .unwrap_or(4);
            let config = Config::new(&db_path)
                .block_cache_size(dbcache_bytes)
                .num_flush_threads(flush_threads)
                .num_compaction_threads(compact_threads)
                .log_level(LogLevel::Warn);

            let db =
                TidesDB::open(config).map_err(|e| anyhow::anyhow!("TidesDB open failed: {}", e))?;

            Ok(Self {
                db: Arc::new(db),
                tidesdb_config: tidesdb_config.cloned(),
            })
        }

        /// Tuned config per tree for IBD/block sync performance.
        fn cf_config_for_tree(&self, name: &str) -> ColumnFamilyConfig {
            let base =
                ColumnFamilyConfig::default().compression_algorithm(CompressionAlgorithm::None);
            let utxo_threshold = self
                .tidesdb_config
                .as_ref()
                .map(|c| c.utxo_klog_threshold)
                .unwrap_or(0);

            match name {
                "ibd_utxos" => base
                    .klog_value_threshold(utxo_threshold)
                    .write_buffer_size(256 * 1024 * 1024) // 256MB memtable, fewer flushes
                    .enable_bloom_filter(true)
                    .bloom_fpr(0.01)
                    .sync_mode(SyncMode::Interval)
                    .sync_interval_us(1_000_000), // 1s sync interval during IBD
                "blocks" => base
                    .klog_value_threshold(4 * 1024 * 1024) // blocks up to 4MB to vlog
                    .write_buffer_size(256 * 1024 * 1024)
                    .enable_bloom_filter(true)
                    .sync_mode(SyncMode::Interval)
                    .sync_interval_us(1_000_000),
                "utxos" => base
                    .klog_value_threshold(utxo_threshold)
                    .write_buffer_size(128 * 1024 * 1024)
                    .enable_bloom_filter(true),
                _ => base,
            }
        }

        fn get_or_create_cf(&self, name: &str) -> Result<tidesdb::ColumnFamily> {
            if let Ok(cf) = self.db.get_column_family(name) {
                return Ok(cf);
            }
            let cf_config = self.cf_config_for_tree(name);
            self.db
                .create_column_family(name, cf_config)
                .map_err(|e| anyhow::anyhow!("TidesDB create_column_family failed: {}", e))?;
            self.db
                .get_column_family(name)
                .map_err(|e| anyhow::anyhow!("TidesDB get_column_family failed: {}", e))
        }

        /// One transaction for all blockstore column families plus recent headers (IBD, no extra sync).
        /// Matches the per-CF batch sequence in `parallel_ibd::do_flush_to_storage`.
        pub(crate) fn write_ibd_blockstore_flush_no_wal(
            &self,
            flush_order: &[usize],
            heights: &[u64],
            block_hashes: &[blvm_protocol::Hash],
            block_data: &[Vec<u8>],
            header_data: &[std::sync::Arc<Vec<u8>>],
            witness_blobs: &[Option<Vec<u8>>],
            metadata_blobs: &[Vec<u8>],
            recent_entries: &[(u64, Vec<u8>)],
        ) -> Result<()> {
            use crate::storage::blockstore::block_height_row_key;

            let cf_blocks = self.get_or_create_cf("blocks")?;
            let cf_headers = self.get_or_create_cf("headers")?;
            let cf_witnesses = self.get_or_create_cf("witnesses")?;
            let cf_height = self.get_or_create_cf("height_index")?;
            let cf_h2h = self.get_or_create_cf("hash_to_height")?;
            let cf_meta = self.get_or_create_cf("block_metadata")?;
            let cf_recent = self.get_or_create_cf("recent_headers")?;

            let mut txn = self.db.begin_transaction()?;
            for &i in flush_order {
                let height = heights[i];
                let key = block_height_row_key(height, &block_hashes[i]);
                txn.put(&cf_blocks, key.as_slice(), &block_data[i], -1)?;
                txn.put(&cf_headers, key.as_slice(), header_data[i].as_slice(), -1)?;
                if let Some(w) = witness_blobs[i].as_ref() {
                    txn.put(&cf_witnesses, key.as_slice(), w.as_slice(), -1)?;
                }
                let height_key = height.to_be_bytes();
                txn.put(&cf_height, &height_key, block_hashes[i].as_slice(), -1)?;
                txn.put(&cf_h2h, block_hashes[i].as_slice(), &height_key, -1)?;
                txn.put(&cf_meta, key.as_slice(), &metadata_blobs[i], -1)?;
            }
            for &(height, ref header_bytes) in recent_entries {
                let height_bytes = height.to_be_bytes();
                txn.put(
                    &cf_recent,
                    height_bytes.as_slice(),
                    header_bytes.as_slice(),
                    -1,
                )?;
                if height > 11 {
                    let rm = (height - 12).to_be_bytes();
                    txn.delete(&cf_recent, rm.as_slice())?;
                }
            }
            txn.commit()
                .map_err(|e| anyhow::anyhow!("TidesDB IBD blockstore flush commit failed: {}", e))
        }
    }

    impl Database for TidesDBDatabase {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>> {
            if name.starts_with("module_") || name == "modules" {
                return Err(anyhow::anyhow!(
                    "Module storage has been removed. Use blvm_sdk::module::open_module_db."
                ));
            }

            let cf = self.get_or_create_cf(name)?;
            Ok(Box::new(TidesDBTree {
                db: Arc::clone(&self.db),
                cf: Arc::new(cf),
                name: name.to_string(),
            }))
        }

        fn flush(&self) -> Result<()> {
            // TidesDB has no global flush(); no-op per implementation plan.
            Ok(())
        }
    }

    struct TidesDBTree {
        db: Arc<TidesDB>,
        cf: Arc<tidesdb::ColumnFamily>,
        name: String,
    }

    fn tidesdb_get_to_option(
        txn: &tidesdb::Transaction,
        cf: &tidesdb::ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        match txn.get(cf, key) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(anyhow::anyhow!("TidesDB get failed: {}", e)),
        }
    }

    impl Tree for TidesDBTree {
        fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
            let mut txn = self.db.begin_transaction()?;
            txn.put(&self.cf, key, value, -1)?;
            txn.commit()
                .map_err(|e| anyhow::anyhow!("TidesDB commit failed: {}", e))
        }

        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            let txn = self.db.begin_transaction()?;
            tidesdb_get_to_option(&txn, &self.cf, key)
        }

        fn remove(&self, key: &[u8]) -> Result<()> {
            let mut txn = self.db.begin_transaction()?;
            txn.delete(&self.cf, key)?;
            txn.commit()
                .map_err(|e| anyhow::anyhow!("TidesDB commit failed: {}", e))
        }

        fn contains_key(&self, key: &[u8]) -> Result<bool> {
            Ok(self.get(key)?.is_some())
        }

        fn clear(&self) -> Result<()> {
            let txn = self.db.begin_transaction()?;
            let mut iter = txn.new_iterator(&self.cf)?;
            iter.seek_to_first()?;
            let mut keys = Vec::new();
            while iter.is_valid() {
                keys.push(iter.key()?);
                iter.next()?;
            }
            drop(iter);
            drop(txn);

            if keys.is_empty() {
                return Ok(());
            }
            let mut txn = self.db.begin_transaction()?;
            for k in keys {
                txn.delete(&self.cf, &k)?;
            }
            txn.commit()
                .map_err(|e| anyhow::anyhow!("TidesDB commit failed: {}", e))
        }

        fn len(&self) -> Result<usize> {
            let stats = self.cf.get_stats()?;
            Ok(stats.total_keys as usize)
        }

        fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
            let txn = match self.db.begin_transaction() {
                Ok(t) => t,
                Err(e) => {
                    return Box::new(std::iter::once(Err(anyhow::anyhow!(
                        "TidesDB begin_transaction failed: {}",
                        e
                    ))));
                }
            };
            let mut iter = match txn.new_iterator(&self.cf) {
                Ok(i) => i,
                Err(e) => {
                    return Box::new(std::iter::once(Err(anyhow::anyhow!(
                        "TidesDB new_iterator failed: {}",
                        e
                    ))));
                }
            };
            let _ = iter.seek_to_first();
            let mut items = Vec::new();
            while iter.is_valid() {
                match (iter.key(), iter.value()) {
                    (Ok(k), Ok(v)) => items.push(Ok((k, v))),
                    (Err(e), _) | (_, Err(e)) => {
                        items.push(Err(anyhow::anyhow!("TidesDB iter: {}", e)));
                        break;
                    }
                }
                if let Err(e) = iter.next() {
                    items.push(Err(anyhow::anyhow!("TidesDB iter next: {}", e)));
                    break;
                }
            }
            Box::new(items.into_iter())
        }

        fn batch(&self) -> Result<Box<dyn BatchWriter + '_>> {
            Ok(Box::new(TidesDBBatchWriter {
                db: Arc::clone(&self.db),
                cf: Arc::clone(&self.cf),
                pending: Vec::new(),
            }))
        }
    }

    struct TidesDBModuleTree {
        inner: Arc<TidesDBTree>,
        module_id: String,
        tree_name: String,
    }

    impl TidesDBModuleTree {
        fn key_prefix(&self) -> Vec<u8> {
            format!("module_{}_{}_", self.module_id, self.tree_name).into_bytes()
        }
        fn namespace_key(&self, key: &[u8]) -> Vec<u8> {
            let mut n = self.key_prefix();
            n.extend_from_slice(key);
            n
        }
    }

    impl Tree for TidesDBModuleTree {
        fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
            self.inner.insert(&self.namespace_key(key), value)
        }
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            self.inner.get(&self.namespace_key(key))
        }
        fn remove(&self, key: &[u8]) -> Result<()> {
            self.inner.remove(&self.namespace_key(key))
        }
        fn contains_key(&self, key: &[u8]) -> Result<bool> {
            self.inner.contains_key(&self.namespace_key(key))
        }
        fn clear(&self) -> Result<()> {
            let prefix = self.key_prefix();
            let keys: Vec<Vec<u8>> = self
                .inner
                .iter()
                .filter_map(|r| match r {
                    Ok((k, _)) if k.starts_with(&prefix) => Some(Ok(k)),
                    Ok(_) => None,
                    Err(e) => Some(Err(e)),
                })
                .collect::<Result<_>>()?;
            for k in keys {
                self.inner.remove(&k)?;
            }
            Ok(())
        }
        fn len(&self) -> Result<usize> {
            let prefix = self.key_prefix();
            let mut count = 0;
            for item in self.inner.iter() {
                match item {
                    Ok((k, _)) if k.starts_with(&prefix) => count += 1,
                    Ok(_) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(count)
        }
        fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
            let prefix = self.key_prefix();
            Box::new(self.inner.iter().filter_map(move |item| match item {
                Ok((k, v)) if k.starts_with(&prefix) => Some(Ok((k[prefix.len()..].to_vec(), v))),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            }))
        }
        fn batch(&self) -> Result<Box<dyn BatchWriter + '_>> {
            Ok(Box::new(TidesDBModuleBatchWriter {
                inner: self.inner.batch()?,
                key_prefix: self.key_prefix(),
            }))
        }
    }

    struct TidesDBModuleBatchWriter<'a> {
        inner: Box<dyn BatchWriter + 'a>,
        key_prefix: Vec<u8>,
    }

    impl<'a> BatchWriter for TidesDBModuleBatchWriter<'a> {
        fn put(&mut self, key: &[u8], value: &[u8]) {
            let mut k = self.key_prefix.clone();
            k.extend_from_slice(key);
            self.inner.put(&k, value);
        }
        fn delete(&mut self, key: &[u8]) {
            let mut k = self.key_prefix.clone();
            k.extend_from_slice(key);
            self.inner.delete(&k);
        }
        fn commit(self: Box<Self>) -> Result<()> {
            self.inner.commit()
        }
        fn len(&self) -> usize {
            self.inner.len()
        }
    }

    struct TidesDBBatchWriter {
        db: Arc<TidesDB>,
        cf: Arc<tidesdb::ColumnFamily>,
        pending: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    }

    impl BatchWriter for TidesDBBatchWriter {
        fn put(&mut self, key: &[u8], value: &[u8]) {
            self.pending.push((key.to_vec(), Some(value.to_vec())));
        }
        fn delete(&mut self, key: &[u8]) {
            self.pending.push((key.to_vec(), None));
        }
        fn commit(self: Box<Self>) -> Result<()> {
            if self.pending.is_empty() {
                return Ok(());
            }
            let mut txn = self.db.begin_transaction()?;
            for (key, value) in self.pending {
                match value {
                    Some(v) => txn.put(&self.cf, &key, &v, -1)?,
                    None => txn.delete(&self.cf, &key)?,
                }
            }
            txn.commit()
                .map_err(|e| anyhow::anyhow!("TidesDB batch commit failed: {}", e))
        }
        fn len(&self) -> usize {
            self.pending.len()
        }
    }
}
