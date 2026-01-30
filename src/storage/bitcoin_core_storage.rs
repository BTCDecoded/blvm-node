//! Bitcoin Core storage integration
//!
//! Integrates Bitcoin Core detection and format parsing with the storage layer.

use crate::storage::bitcoin_core_detection::{BitcoinCoreDetection, BitcoinCoreNetwork};
use crate::storage::database::{create_database, DatabaseBackend};
use anyhow::Result;
use std::path::Path;

/// Storage initialization with Bitcoin Core detection
pub struct BitcoinCoreStorage;

impl BitcoinCoreStorage {
    /// Detect and initialize storage from Bitcoin Core data
    ///
    /// This function:
    /// 1. Detects if Bitcoin Core data exists
    /// 2. If found, opens it with RocksDB (which can read LevelDB format)
    /// 3. Returns the appropriate database backend
    #[cfg(feature = "rocksdb")]
    pub fn detect_and_open(
        data_dir: &Path,
        network: BitcoinCoreNetwork,
    ) -> Result<Option<DatabaseBackend>> {
        // Check for existing BLVM database first
        if Self::has_blvm_database(data_dir) {
            // Use existing BLVM database (don't try Bitcoin Core)
            return Ok(None);
        }

        // Try to detect Bitcoin Core data
        if let Some(core_dir) = BitcoinCoreDetection::detect_data_dir(network)? {
            // Verify database format
            let chainstate = core_dir.join("chainstate");
            if BitcoinCoreDetection::detect_db_format(&chainstate).is_ok() {
                // Bitcoin Core LevelDB database found
                // Return RocksDB backend (can read LevelDB format)
                return Ok(Some(DatabaseBackend::RocksDB));
            }
        }

        Ok(None)
    }

    #[cfg(not(feature = "rocksdb"))]
    pub fn detect_and_open(
        _data_dir: &Path,
        _network: BitcoinCoreNetwork,
    ) -> Result<Option<DatabaseBackend>> {
        // RocksDB not available, cannot detect Bitcoin Core
        Ok(None)
    }

    /// Check if BLVM database already exists
    fn has_blvm_database(data_dir: &Path) -> bool {
        // Check for redb database
        let redb_path = data_dir.join("redb.db");
        if redb_path.exists() {
            return true;
        }

        // Check for sled database
        let sled_path = data_dir.join("sled");
        if sled_path.exists() {
            return true;
        }

        // Check for rocksdb database (BLVM format, not Bitcoin Core)
        let rocksdb_path = data_dir.join("rocksdb");
        if rocksdb_path.exists() {
            return true;
        }

        false
    }

    /// Open Bitcoin Core database with RocksDB
    ///
    /// Opens the Bitcoin Core chainstate database using RocksDB's LevelDB compatibility.
    #[cfg(feature = "rocksdb")]
    pub fn open_bitcoin_core_database(
        data_dir: &Path,
        network: BitcoinCoreNetwork,
    ) -> Result<Box<dyn crate::storage::database::Database>> {
        #[cfg(feature = "rocksdb")]
        {
            use crate::storage::database::rocksdb_impl::RocksDBDatabase;

            if let Some(core_dir) = BitcoinCoreDetection::detect_data_dir(network)? {
                // Verify database integrity
                BitcoinCoreDetection::verify_database(&core_dir)?;

                // Open with RocksDB (can read LevelDB format)
                let db = RocksDBDatabase::open_bitcoin_core(&core_dir)?;
                Ok(Box::new(db))
            } else {
                Err(anyhow::anyhow!("Bitcoin Core data directory not found"))
            }
        }
        #[cfg(not(feature = "rocksdb"))]
        {
            Err(anyhow::anyhow!("RocksDB feature not enabled"))
        }
    }
}

