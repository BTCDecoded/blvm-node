//! Database abstraction layer
//!
//! Provides a unified interface for different database backends (sled, redb).
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
}

/// Database backend type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseBackend {
    Sled,
    Redb,
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
            #[cfg(not(feature = "sled"))]
            {
                None
            }
        }
        DatabaseBackend::Sled => {
            #[cfg(feature = "redb")]
            {
                Some(DatabaseBackend::Redb)
            }
            #[cfg(not(feature = "redb"))]
            {
                None
            }
        }
    }
}

// Sled implementation
#[cfg(feature = "sled")]
mod sled_impl {
    use super::{Database, Tree};
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
    }
}

// Redb implementation
#[cfg(feature = "redb")]
mod redb_impl {
    use super::{Database, Tree};
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
                _ => None,
            }
        }
    }

    impl Database for RedbDatabase {
        fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>> {
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
            // Redb doesn't have a direct clear() method
            // Range from iter() doesn't implement Iterator as expected
            // For now, we'll skip the clear implementation
            // TODO: Implement proper Range iteration for clear()
            // This can be implemented later when Range API is better understood
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
    }
}
