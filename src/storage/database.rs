//! Database abstraction layer
//!
//! Provides a unified interface for different database backends (sled, redb, rocksdb).
//! Allows switching between storage engines via feature flags.

use anyhow::Result;
use std::path::Path;

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
}

/// Create a database instance based on backend type
pub fn create_database<P: AsRef<Path>>(
    data_dir: P,
    backend: DatabaseBackend,
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
    }
}

/// Get default database backend
///
/// Returns the preferred backend (redb if available, otherwise sled).
/// This function will not panic - it returns a backend if at least one is available.
pub fn default_backend() -> DatabaseBackend {
    #[cfg(feature = "redb")]
    {
        DatabaseBackend::Redb
    }
    #[cfg(all(not(feature = "redb"), feature = "sled"))]
    {
        DatabaseBackend::Sled
    }
    #[cfg(all(not(feature = "redb"), not(feature = "sled")))]
    {
        // This should never happen if features are properly configured,
        // but we return Redb as a sentinel value that will fail gracefully
        // in create_database() with a clear error message
        DatabaseBackend::Redb
    }
}

/// Get fallback database backend
///
/// Returns an alternative backend if the primary fails.
/// Returns None if no fallback is available.
pub fn fallback_backend(primary: DatabaseBackend) -> Option<DatabaseBackend> {
    match primary {
        DatabaseBackend::Redb => {
            #[cfg(feature = "sled")]
            {
                Some(DatabaseBackend::Sled)
            }
            #[cfg(all(not(feature = "sled"), feature = "rocksdb"))]
            {
                Some(DatabaseBackend::RocksDB)
            }
            #[cfg(all(not(feature = "sled"), not(feature = "rocksdb")))]
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
            #[cfg(feature = "redb")]
            {
                Some(DatabaseBackend::Redb)
            }
            #[cfg(all(not(feature = "redb"), feature = "sled"))]
            {
                Some(DatabaseBackend::Sled)
            }
            #[cfg(all(not(feature = "redb"), not(feature = "sled")))]
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
            let _guard = DB_CREATE_MUTEX.lock().unwrap();

            let db_path = data_dir.as_ref().join("redb.db");
            // Try to open existing database first, then create if it doesn't exist
            let db = if db_path.exists() {
                // Database exists, try to open it
                match RedbDb::open(&db_path) {
                    Ok(db) => {
                        // Database exists and is openable, use it
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
                            // Payment system tables
                            let _ = write_txn.open_table(VAULTS_TABLE)?;
                            let _ = write_txn.open_table(POOLS_TABLE)?;
                            let _ = write_txn.open_table(BATCHES_TABLE)?;
                            // Module storage table
                            let _ = write_txn.open_table(MODULES_TABLE)?;
                        }
                        write_txn.commit()?;
                        db
                    }
                    Err(_) => {
                        // Can't open existing database, create new one
                        RedbDb::create(&db_path)?
                    }
                }
            } else {
                // Database doesn't exist, create new one
                RedbDb::create(&db_path)?
            };

            // Initialize all tables in a write transaction
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

            // Define column families for each tree type
            let cfs = vec![
                ColumnFamilyDescriptor::new("default", Options::default()),
                ColumnFamilyDescriptor::new("blocks", Options::default()),
                ColumnFamilyDescriptor::new("headers", Options::default()),
                ColumnFamilyDescriptor::new("height_index", Options::default()),
                ColumnFamilyDescriptor::new("hash_to_height", Options::default()),
                ColumnFamilyDescriptor::new("witnesses", Options::default()),
                ColumnFamilyDescriptor::new("recent_headers", Options::default()),
                ColumnFamilyDescriptor::new("utxos", Options::default()),
                ColumnFamilyDescriptor::new("spent_outputs", Options::default()),
                ColumnFamilyDescriptor::new("chain_info", Options::default()),
                ColumnFamilyDescriptor::new("work_cache", Options::default()),
                ColumnFamilyDescriptor::new("chainwork_cache", Options::default()),
                ColumnFamilyDescriptor::new("utxo_stats_cache", Options::default()),
                ColumnFamilyDescriptor::new("network_hashrate_cache", Options::default()),
                ColumnFamilyDescriptor::new("invalid_blocks", Options::default()),
                ColumnFamilyDescriptor::new("chain_tips", Options::default()),
                ColumnFamilyDescriptor::new("tx_by_hash", Options::default()),
                ColumnFamilyDescriptor::new("tx_by_block", Options::default()),
                ColumnFamilyDescriptor::new("tx_metadata", Options::default()),
                ColumnFamilyDescriptor::new("modules", Options::default()),
            ];

            let db = DB::open_cf_descriptors(&opts, &db_path, cfs)?;

            Ok(Self { db: Arc::new(db) })
        }

        /// Open RocksDB with Bitcoin Core LevelDB format
        ///
        /// Opens an existing Bitcoin Core chainstate database (LevelDB format).
        /// RocksDB can read LevelDB databases directly (backward compatible).
        pub fn open_bitcoin_core<P: AsRef<Path>>(data_dir: P) -> Result<Self> {
            // Open existing Bitcoin Core chainstate database (LevelDB format)
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
            // Get or create column family
            let cf = if let Some(cf) = self.db.cf_handle(name) {
                cf
            } else {
                // Create new column family if it doesn't exist
                let opts = Options::default();
                self.db.create_cf(name, &opts)?;
                self.db.cf_handle(name)
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
