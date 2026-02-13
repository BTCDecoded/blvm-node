//! Database abstraction layer
//!
//! Provides a unified interface for different database backends (tidesdb, redb, sled, rocksdb).
//! Allows switching between storage engines via feature flags.
//!
//! All backends must support the same set of tree names. Module trees (module_{id}_{name})
//! use a shared "modules" storage with key prefixes for isolation.

use anyhow::Result;
use std::path::Path;

/// All known tree names (excluding dynamic module_* which use shared "modules").
/// Used by RocksDB for open_cf_descriptors and for backend parity validation.
pub const KNOWN_TREE_NAMES: &[&str] = &[
    "blocks",
    "headers",
    "height_index",
    "hash_to_height",
    "witnesses",
    "recent_headers",
    "utxos",
    "ibd_utxos",
    "spent_outputs",
    "chain_info",
    "work_cache",
    "chainwork_cache",
    "utxo_stats_cache",
    "network_hashrate_cache",
    "invalid_blocks",
    "chain_tips",
    "block_metadata",
    "tx_by_hash",
    "tx_by_block",
    "tx_metadata",
    "address_tx_index",
    "address_output_index",
    "address_input_index",
    "value_index",
    "utxo_commitments",
    "commitment_height_index",
    "vaults",
    "pools",
    "batches",
    "modules",
];

/// Database abstraction trait
///
/// Provides a unified interface for key-value storage operations
/// that can be implemented by different backends (sled, redb).
pub trait Database: Send + Sync {
    /// Open a named tree/table
    fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>>;

    /// Flush all pending writes
    fn flush(&self) -> Result<()>;
}

/// Tree/Table abstraction trait
///
/// Represents a named collection of key-value pairs within a database.
pub trait Tree: Send + Sync {
    /// Insert a key-value pair
    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()>;

    /// Get a value by key
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

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

    /// Iterate over all key-value pairs
    fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_>;

    /// Create a batch writer for efficient bulk operations
    ///
    /// Batch writes are 10-100x faster than individual inserts because they
    /// commit all operations in a single transaction instead of one per operation.
    ///
    /// # Example
    /// ```ignore
    /// let mut batch = tree.batch();
    /// for (key, value) in items {
    ///     batch.put(key, value);
    /// }
    /// batch.commit()?;  // Single atomic commit
    /// ```
    fn batch(&self) -> Box<dyn BatchWriter + '_>;
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
        DatabaseBackend::Redb => Ok(Box::new(redb_impl::RedbDatabase::new(data_dir)?)),
        #[cfg(not(feature = "redb"))]
        DatabaseBackend::Redb => Err(anyhow::anyhow!(
            "Redb backend not available (feature not enabled)"
        )),
        #[cfg(feature = "rocksdb")]
        DatabaseBackend::RocksDB => Ok(Box::new(rocksdb_impl::RocksDBDatabase::new(data_dir)?)),
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
    #[cfg(feature = "tidesdb")]
    {
        return DatabaseBackend::TidesDB;
    }
    #[cfg(feature = "redb")]
    {
        return DatabaseBackend::Redb;
    }
    #[cfg(feature = "sled")]
    {
        return DatabaseBackend::Sled;
    }
    #[cfg(feature = "rocksdb")]
    {
        return DatabaseBackend::RocksDB;
    }
    DatabaseBackend::Redb // fallback (redb usually always enabled)
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
            #[cfg(all(
                not(feature = "redb"),
                not(feature = "rocksdb"),
                not(feature = "sled")
            ))]
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
            #[cfg(all(
                not(feature = "tidesdb"),
                not(feature = "redb"),
                not(feature = "sled")
            ))]
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
        fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>> {
            // Module trees use shared "modules" tree with key prefix (same as redb)
            if name.starts_with("module_") {
                let parts: Vec<&str> = name.splitn(3, '_').collect();
                if parts.len() == 3 && parts[0] == "module" {
                    let module_id = parts[1].to_string();
                    let tree_name = parts[2].to_string();
                    let modules_tree = self.db.open_tree("modules")?;
                    let inner = SledTree {
                        tree: Arc::new(modules_tree),
                    };
                    return Ok(Box::new(SledModuleTree {
                        inner: Arc::new(inner),
                        module_id,
                        tree_name,
                    }));
                }
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

        fn batch(&self) -> Box<dyn BatchWriter + '_> {
            Box::new(SledBatchWriter {
                tree: Arc::clone(&self.tree),
                batch: sled::Batch::default(),
                op_count: 0,
            })
        }
    }

    /// Module tree wrapper - uses shared "modules" tree with key prefix (parity with redb)
    struct SledModuleTree {
        inner: Arc<SledTree>,
        module_id: String,
        tree_name: String,
    }

    impl SledModuleTree {
        fn key_prefix(&self) -> Vec<u8> {
            format!("module_{}_{}_", self.module_id, self.tree_name).into_bytes()
        }
        fn namespace_key(&self, key: &[u8]) -> Vec<u8> {
            let mut n = self.key_prefix();
            n.extend_from_slice(key);
            n
        }
    }

    impl Tree for SledModuleTree {
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
            Box::new(
                self.inner
                    .iter()
                    .filter_map(move |item| match item {
                        Ok((k, v)) if k.starts_with(&prefix) => {
                            Some(Ok((k[prefix.len()..].to_vec(), v)))
                        }
                        Ok(_) => None,
                        Err(e) => Some(Err(e)),
                    }),
            )
        }
        fn batch(&self) -> Box<dyn BatchWriter + '_> {
            Box::new(SledModuleBatchWriter {
                inner: self.inner.batch(),
                key_prefix: self.key_prefix(),
            })
        }
    }

    struct SledModuleBatchWriter<'a> {
        inner: Box<dyn BatchWriter + 'a>,
        key_prefix: Vec<u8>,
    }

    impl<'a> BatchWriter for SledModuleBatchWriter<'a> {
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
mod redb_impl {
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
    // Module storage table (shared table for all modules with namespaced keys)
    static MODULES_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("modules");

    pub struct RedbDatabase {
        db: Arc<RedbDb>,
    }

    impl RedbDatabase {
        pub fn new<P: AsRef<Path>>(data_dir: P) -> Result<Self> {
            use std::sync::Mutex;
            // Global mutex to serialize database creation (prevents lock conflicts in tests)
            static DB_CREATE_MUTEX: Mutex<()> = Mutex::new(());
            tracing::info!("[REDB] Acquiring DB_CREATE_MUTEX...");
            let _guard = DB_CREATE_MUTEX.lock().unwrap();
            tracing::info!("[REDB] DB_CREATE_MUTEX acquired");

            // redb cache size: BLVM_DBCACHE_MB env (default 450, matches Core -dbcache)
            let dbcache_mb: usize = std::env::var("BLVM_DBCACHE_MB")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(450);
            let dbcache_bytes = dbcache_mb.saturating_mul(1024).saturating_mul(1024);
            let mut builder = RedbDb::builder();
            builder.set_cache_size(dbcache_bytes);
            tracing::info!("[REDB] Cache size: {} MB (set via BLVM_DBCACHE_MB or config)", dbcache_mb);

            let db_path = data_dir.as_ref().join("redb.db");
            tracing::info!("[REDB] Database path: {:?}", db_path);
            tracing::info!("[REDB] Database path absolute: {:?}", std::fs::canonicalize(&db_path).unwrap_or_else(|_| db_path.clone()));
            let exists = db_path.exists();
            tracing::info!("[REDB] db_path.exists() = {}", exists);
            // Try to open existing database first, then create if it doesn't exist
            let db = if exists {
                // Gather diagnostic information about the database file
                let file_size = std::fs::metadata(&db_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                let file_size_mb = file_size / (1024 * 1024);
                tracing::info!("[REDB] Database file exists, size: {} MB ({})", file_size_mb, file_size);
                
                // Check if database is locked by another process
                // redb uses file locking, so if another process has it open, open() will fail immediately
                tracing::info!("[REDB] Attempting to open database (this may take time for large databases)...");
                tracing::info!("[REDB] Note: redb validates checksums on open, which can be slow for large databases");
                tracing::info!("[REDB] If this hangs, redb may be performing crash recovery validation");
                
                use std::time::Instant;
                let start_time = Instant::now();
                
                // Open the database - this may take time for large databases
                // redb performs checksum validation during open, especially after crashes
                let open_result = builder.open(&db_path);
                
                let elapsed = start_time.elapsed();
                tracing::info!("[REDB] Database open completed in {:?}", elapsed);
                
                match open_result {
                    Ok(db) => {
                        tracing::info!("[REDB] Database opened successfully in {:?}, opening tables...", elapsed);
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
                            let _ = write_txn.open_table(MODULES_TABLE)?;
                            let _ = write_txn.open_table(INVALID_BLOCKS_TABLE)?;
                            let _ = write_txn.open_table(CHAIN_TIPS_TABLE)?;
                            let _ = write_txn.open_table(BLOCK_METADATA_TABLE)?;
                            let _ = write_txn.open_table(CHAINWORK_CACHE_TABLE)?;
                            let _ = write_txn.open_table(UTXO_STATS_CACHE_TABLE)?;
                            let _ = write_txn.open_table(NETWORK_HASHRATE_CACHE_TABLE)?;
                            let _ = write_txn.open_table(UTXO_COMMITMENTS_TABLE)?;
                            let _ = write_txn.open_table(COMMITMENT_HEIGHT_INDEX_TABLE)?;
                        }
                        write_txn.commit()?;
                        let table_elapsed = table_start.elapsed();
                        tracing::info!("[REDB] Tables opened and committed in {:?}", table_elapsed);
                        db
                    }
                    Err(e) => {
                        tracing::warn!("[REDB] Failed to open existing database after {:?}: {}", elapsed, e);
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
                // Module storage table
                let _ = write_txn.open_table(MODULES_TABLE)?;
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
                // Module storage table
                "modules" => Some(&MODULES_TABLE),
                // Module trees (dynamic names) use MODULES_TABLE with namespaced keys
                name if name.starts_with("module_") => Some(&MODULES_TABLE),
                _ => None,
            }
        }
    }

    impl Database for RedbDatabase {
        fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>> {
            // Handle module trees specially
            if name.starts_with("module_") {
                // Parse module_id and tree_name from format: module_{module_id}_{tree_name}
                let parts: Vec<&str> = name.splitn(3, '_').collect();
                if parts.len() == 3 && parts[0] == "module" {
                    let module_id = parts[1].to_string();
                    let tree_name = parts[2].to_string();

                    // Get MODULES_TABLE directly (avoid recursion)
                    let table_def = self
                        .get_table_def("modules")
                        .ok_or_else(|| anyhow::anyhow!("MODULES_TABLE not defined"))?;

                    // Create a RedbTree for the MODULES_TABLE (bypass open_tree to avoid recursion)
                    let inner_tree = RedbTree {
                        db: Arc::clone(&self.db),
                        table_def,
                        name: "modules".to_string(),
                    };

                    // Return wrapped tree with namespacing
                    return Ok(Box::new(ModuleTree {
                        inner: Arc::new(inner_tree),
                        module_id,
                        tree_name,
                    }));
                }
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

    /// ModuleTree wrapper that provides namespaced keys for module storage
    /// All module trees share the MODULES_TABLE but use key prefixes for isolation
    struct ModuleTree {
        inner: Arc<dyn Tree>,
        module_id: String,
        tree_name: String,
    }

    impl ModuleTree {
        fn namespace_key(&self, key: &[u8]) -> Vec<u8> {
            let mut namespaced = self.key_prefix();
            namespaced.extend_from_slice(key);
            namespaced
        }

        fn key_prefix(&self) -> Vec<u8> {
            format!("module_{}_{}_", self.module_id, self.tree_name).into_bytes()
        }
    }

    impl Tree for ModuleTree {
        fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
            let namespaced_key = self.namespace_key(key);
            self.inner.insert(&namespaced_key, value)
        }

        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            let namespaced_key = self.namespace_key(key);
            self.inner.get(&namespaced_key)
        }

        fn remove(&self, key: &[u8]) -> Result<()> {
            let namespaced_key = self.namespace_key(key);
            self.inner.remove(&namespaced_key)
        }

        fn contains_key(&self, key: &[u8]) -> Result<bool> {
            // Direct lookup with namespaced key - efficient
            let namespaced_key = self.namespace_key(key);
            self.inner.contains_key(&namespaced_key)
        }

        fn clear(&self) -> Result<()> {
            // Remove only keys with this module/tree prefix
            let prefix = self.key_prefix();
            let keys: Vec<Vec<u8>> = {
                let mut collected = Vec::new();
                for item in self.inner.iter() {
                    match item {
                        Ok((k, _)) => {
                            if k.starts_with(&prefix) {
                                collected.push(k);
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                collected
            };

            for key in keys {
                self.inner.remove(&key)?;
            }
            Ok(())
        }

        fn len(&self) -> Result<usize> {
            // Count only keys with this module/tree prefix
            let prefix = self.key_prefix();
            let mut count = 0;
            for item in self.inner.iter() {
                match item {
                    Ok((k, _)) if k.starts_with(&prefix) => count += 1,
                    Ok(_) => {} // Skip keys from other modules
                    Err(e) => return Err(e),
                }
            }
            Ok(count)
        }

        fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
            // Filter iterator to only return keys for this module/tree
            let prefix = self.key_prefix();
            Box::new(
                self.inner
                    .iter()
                    .filter_map(move |item| {
                        match item {
                            Ok((k, v)) if k.starts_with(&prefix) => {
                                // Remove prefix from key
                                let unprefixed_key = k[prefix.len()..].to_vec();
                                Some(Ok((unprefixed_key, v)))
                            }
                            Ok(_) => None, // Skip keys from other modules
                            Err(e) => Some(Err(e)),
                        }
                    }),
            )
        }

        fn batch(&self) -> Box<dyn BatchWriter + '_> {
            // Create a namespacing wrapper around the inner batch
            Box::new(ModuleBatchWriter {
                inner: self.inner.batch(),
                key_prefix: self.key_prefix(),
            })
        }
    }

    /// Batch writer wrapper that adds namespace prefix to all keys
    struct ModuleBatchWriter<'a> {
        inner: Box<dyn BatchWriter + 'a>,
        key_prefix: Vec<u8>,
    }

    impl<'a> BatchWriter for ModuleBatchWriter<'a> {
        fn put(&mut self, key: &[u8], value: &[u8]) {
            let mut namespaced_key = self.key_prefix.clone();
            namespaced_key.extend_from_slice(key);
            self.inner.put(&namespaced_key, value);
        }

        fn delete(&mut self, key: &[u8]) {
            let mut namespaced_key = self.key_prefix.clone();
            namespaced_key.extend_from_slice(key);
            self.inner.delete(&namespaced_key);
        }

        fn commit(self: Box<Self>) -> Result<()> {
            self.inner.commit()
        }

        fn len(&self) -> usize {
            self.inner.len()
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

        fn batch(&self) -> Box<dyn BatchWriter + '_> {
            Box::new(RedbBatchWriter {
                db: Arc::clone(&self.db),
                table_def: self.table_def,
                pending: Vec::new(),
            })
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
    use rocksdb::{DB, Options, ColumnFamilyDescriptor, ColumnFamily};
    use std::path::Path;
    use std::sync::Arc;

    pub struct RocksDBDatabase {
        db: Arc<DB>,
    }

    impl RocksDBDatabase {
        /// Create a new RocksDB database
        pub fn new<P: AsRef<Path>>(data_dir: P) -> Result<Self> {
            let db_path = data_dir.as_ref().join("rocksdb");
            let mut opts = Options::default();
            opts.create_if_missing(true);
            opts.create_missing_column_families(true);

            // Build CF list: default + all known trees (parity with redb)
            let mut cfs = vec![ColumnFamilyDescriptor::new("default", Options::default())];
            cfs.extend(
                super::KNOWN_TREE_NAMES
                    .iter()
                    .map(|n| ColumnFamilyDescriptor::new(*n, Options::default())),
            );

            // If existing DB may have extra CFs (e.g. old module_* per-CF), merge for reopen
            let db = if db_path.exists() {
                let mut cf_descriptors = cfs.clone();
                let known: std::collections::HashSet<_> = ["default"]
                    .iter()
                    .chain(super::KNOWN_TREE_NAMES)
                    .map(|s| (*s).to_string())
                    .collect();
                if let Ok(existing) = rocksdb::DB::list_cf(&opts, &db_path) {
                    for name in existing {
                        if !known.contains(&name) {
                            cf_descriptors.push(ColumnFamilyDescriptor::new(
                                name,
                                Options::default(),
                            ));
                        }
                    }
                }
                DB::open_cf_descriptors(&opts, &db_path, cf_descriptors)?
            } else {
                DB::open_cf_descriptors(&opts, &db_path, cfs)?
            };

            Ok(Self { db: Arc::new(db) })
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
            let cfs = vec![
                ColumnFamilyDescriptor::new("default", Options::default()),
            ];

            let db = DB::open_cf_descriptors(&opts, &chainstate_path, cfs)?;

            Ok(Self { db: Arc::new(db) })
        }
    }

    impl Database for RocksDBDatabase {
        fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>> {
            // Module trees use shared "modules" CF with key prefix (same as redb)
            if name.starts_with("module_") {
                let parts: Vec<&str> = name.splitn(3, '_').collect();
                if parts.len() == 3 && parts[0] == "module" {
                    let module_id = parts[1].to_string();
                    let tree_name = parts[2].to_string();
                    let modules_cf = self
                        .db
                        .cf_handle("modules")
                        .ok_or_else(|| anyhow::anyhow!("modules column family not found"))?;
                    let inner = RocksDBTree {
                        db: Arc::clone(&self.db),
                        cf: Arc::new(modules_cf),
                        name: "modules".to_string(),
                    };
                    return Ok(Box::new(RocksDBModuleTree {
                        inner: Arc::new(inner),
                        module_id,
                        tree_name,
                    }));
                }
            }

            // Known trees use pre-created CF. Others create on demand (e.g. WAL, tests).
            let cf = if let Some(handle) = self.db.cf_handle(name) {
                handle
            } else if super::KNOWN_TREE_NAMES.contains(&name) {
                return Err(anyhow::anyhow!(
                    "Column family {} should exist but was not found",
                    name
                ));
            } else {
                let opts = Options::default();
                self.db.create_cf(name, &opts)?;
                self.db
                    .cf_handle(name)
                    .ok_or_else(|| anyhow::anyhow!("Failed to create column family: {}", name))?
            };

            Ok(Box::new(RocksDBTree {
                db: Arc::clone(&self.db),
                cf: Arc::new(cf),
                name: name.to_string(),
            }))
        }

        fn flush(&self) -> Result<()> {
            self.db.flush()?;
            Ok(())
        }
    }

    struct RocksDBTree {
        db: Arc<DB>,
        cf: Arc<ColumnFamily>,
        name: String,
    }

    impl Tree for RocksDBTree {
        fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
            self.db.put_cf(&self.cf, key, value)?;
            Ok(())
        }

        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            Ok(self.db.get_cf(&self.cf, key)?.map(|v| v.to_vec()))
        }

        fn remove(&self, key: &[u8]) -> Result<()> {
            self.db.delete_cf(&self.cf, key)?;
            Ok(())
        }

        fn contains_key(&self, key: &[u8]) -> Result<bool> {
            Ok(self.db.get_cf(&self.cf, key)?.is_some())
        }

        fn clear(&self) -> Result<()> {
            // Delete all keys in this column family
            let mut iter = self.db.iterator_cf(&self.cf, rocksdb::IteratorMode::Start);
            let mut batch = rocksdb::WriteBatch::default();

            while let Some(item) = iter.next() {
                let (key, _) = item?;
                batch.delete_cf(&self.cf, &key);
            }

            self.db.write(batch)?;
            Ok(())
        }

        fn len(&self) -> Result<usize> {
            // Count keys in column family
            let mut count = 0;
            let iter = self.db.iterator_cf(&self.cf, rocksdb::IteratorMode::Start);
            for item in iter {
                let _ = item?;
                count += 1;
            }
            Ok(count)
        }

        fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
            // Use RocksDB iterator for efficient iteration
            // Note: We need to collect into Vec because the iterator borrows from self
            let iter = self.db.iterator_cf(&self.cf, rocksdb::IteratorMode::Start);
            let items: Vec<_> = iter
                .map(|item| {
                    item.map(|(k, v)| (k.to_vec(), v.to_vec()))
                        .map_err(|e| anyhow::anyhow!("RocksDB iteration error: {}", e))
                })
                .collect();

            Box::new(items.into_iter())
        }

        fn batch(&self) -> Box<dyn BatchWriter + '_> {
            Box::new(RocksDBBatchWriter {
                db: Arc::clone(&self.db),
                cf: Arc::clone(&self.cf),
                batch: rocksdb::WriteBatch::default(),
                op_count: 0,
            })
        }
    }

    /// Module tree wrapper - uses shared "modules" CF with key prefix (parity with redb)
    struct RocksDBModuleTree {
        inner: Arc<RocksDBTree>,
        module_id: String,
        tree_name: String,
    }

    impl RocksDBModuleTree {
        fn key_prefix(&self) -> Vec<u8> {
            format!("module_{}_{}_", self.module_id, self.tree_name).into_bytes()
        }
        fn namespace_key(&self, key: &[u8]) -> Vec<u8> {
            let mut n = self.key_prefix();
            n.extend_from_slice(key);
            n
        }
    }

    impl Tree for RocksDBModuleTree {
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
            Box::new(
                self.inner
                    .iter()
                    .filter_map(move |item| match item {
                        Ok((k, v)) if k.starts_with(&prefix) => {
                            Some(Ok((k[prefix.len()..].to_vec(), v)))
                        }
                        Ok(_) => None,
                        Err(e) => Some(Err(e)),
                    }),
            )
        }
        fn batch(&self) -> Box<dyn BatchWriter + '_> {
            Box::new(RocksDBModuleBatchWriter {
                inner: self.inner.batch(),
                key_prefix: self.key_prefix(),
            })
        }
    }

    struct RocksDBModuleBatchWriter<'a> {
        inner: Box<dyn BatchWriter + 'a>,
        key_prefix: Vec<u8>,
    }

    impl<'a> BatchWriter for RocksDBModuleBatchWriter<'a> {
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

    /// RocksDB batch writer using native WriteBatch
    ///
    /// RocksDB's WriteBatch is highly optimized for bulk operations.
    struct RocksDBBatchWriter {
        db: Arc<DB>,
        cf: Arc<ColumnFamily>,
        batch: rocksdb::WriteBatch,
        op_count: usize,
    }

    impl BatchWriter for RocksDBBatchWriter {
        fn put(&mut self, key: &[u8], value: &[u8]) {
            self.batch.put_cf(&self.cf, key, value);
            self.op_count += 1;
        }

        fn delete(&mut self, key: &[u8]) {
            self.batch.delete_cf(&self.cf, key);
            self.op_count += 1;
        }

        fn commit(self: Box<Self>) -> Result<()> {
            self.db.write(self.batch)?;
            Ok(())
        }

        fn len(&self) -> usize {
            self.op_count
        }
    }
}

// TidesDB implementation
#[cfg(feature = "tidesdb")]
mod tidesdb_impl {
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

            let flush_threads: i32 = std::env::var("BLVM_TIDESDB_FLUSH_THREADS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(4);
            let compact_threads: i32 = std::env::var("BLVM_TIDESDB_COMPACT_THREADS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(4);
            let config = Config::new(&db_path)
                .block_cache_size(dbcache_bytes)
                .num_flush_threads(flush_threads)
                .num_compaction_threads(compact_threads)
                .log_level(LogLevel::Warn);

            let db = TidesDB::open(config)
                .map_err(|e| anyhow::anyhow!("TidesDB open failed: {}", e))?;

            Ok(Self {
                db: Arc::new(db),
                tidesdb_config: tidesdb_config.cloned(),
            })
        }

        /// Tuned config per tree for IBD/block sync performance.
        fn cf_config_for_tree(&self, name: &str) -> ColumnFamilyConfig {
            let base = ColumnFamilyConfig::default().compression_algorithm(CompressionAlgorithm::None);
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
    }

    impl Database for TidesDBDatabase {
        fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>> {
            if name.starts_with("module_") {
                let parts: Vec<&str> = name.splitn(3, '_').collect();
                if parts.len() == 3 && parts[0] == "module" {
                    let module_id = parts[1].to_string();
                    let tree_name = parts[2].to_string();
                    let modules_cf = self.get_or_create_cf("modules")?;
                    let inner = TidesDBTree {
                        db: Arc::clone(&self.db),
                        cf: Arc::new(modules_cf),
                        name: "modules".to_string(),
                    };
                    return Ok(Box::new(TidesDBModuleTree {
                        inner: Arc::new(inner),
                        module_id,
                        tree_name,
                    }));
                }
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
            txn.commit().map_err(|e| anyhow::anyhow!("TidesDB commit failed: {}", e))
        }

        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            let txn = self.db.begin_transaction()?;
            tidesdb_get_to_option(&txn, &self.cf, key)
        }

        fn remove(&self, key: &[u8]) -> Result<()> {
            let mut txn = self.db.begin_transaction()?;
            txn.delete(&self.cf, key)?;
            txn.commit().map_err(|e| anyhow::anyhow!("TidesDB commit failed: {}", e))
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
            txn.commit().map_err(|e| anyhow::anyhow!("TidesDB commit failed: {}", e))
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

        fn batch(&self) -> Box<dyn BatchWriter + '_> {
            Box::new(TidesDBBatchWriter {
                db: Arc::clone(&self.db),
                cf: Arc::clone(&self.cf),
                pending: Vec::new(),
            })
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
            Box::new(
                self.inner
                    .iter()
                    .filter_map(move |item| match item {
                        Ok((k, v)) if k.starts_with(&prefix) => {
                            Some(Ok((k[prefix.len()..].to_vec(), v)))
                        }
                        Ok(_) => None,
                        Err(e) => Some(Err(e)),
                    }),
            )
        }
        fn batch(&self) -> Box<dyn BatchWriter + '_> {
            Box::new(TidesDBModuleBatchWriter {
                inner: self.inner.batch(),
                key_prefix: self.key_prefix(),
            })
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
            txn.commit().map_err(|e| anyhow::anyhow!("TidesDB batch commit failed: {}", e))
        }
        fn len(&self) -> usize {
            self.pending.len()
        }
    }
}
