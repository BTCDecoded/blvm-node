//! Bitcoin Core storage integration
//!
//! Integrates Bitcoin Core detection and format parsing with the storage layer.

use crate::storage::bitcoin_detection::{BitcoinCoreDetection, CoreDataNetwork};
use crate::storage::database::DatabaseBackend;
use anyhow::{Context, Result};
use std::path::Path;

/// Storage initialization with Bitcoin Core detection
pub struct BitcoinCoreStorage;

impl BitcoinCoreStorage {
    /// Detect and initialize storage from Bitcoin Core data
    #[cfg(feature = "rocksdb")]
    pub fn detect_and_open(
        data_dir: &Path,
        network: CoreDataNetwork,
    ) -> Result<Option<DatabaseBackend>> {
        if Self::has_blvm_database(data_dir) {
            return Ok(None);
        }

        if BitcoinCoreDetection::is_core_layout_at(data_dir) {
            let chainstate = data_dir.join("chainstate");
            if BitcoinCoreDetection::detect_db_format(&chainstate).is_ok() {
                return Ok(Some(DatabaseBackend::RocksDB));
            }
        }

        if let Some(core_dir) = BitcoinCoreDetection::detect_data_dir(network)? {
            let chainstate = core_dir.join("chainstate");
            if BitcoinCoreDetection::detect_db_format(&chainstate).is_ok() {
                return Ok(Some(DatabaseBackend::RocksDB));
            }
        }

        Ok(None)
    }

    #[cfg(not(feature = "rocksdb"))]
    pub fn detect_and_open(
        _data_dir: &Path,
        _network: CoreDataNetwork,
    ) -> Result<Option<DatabaseBackend>> {
        Ok(None)
    }

    pub fn has_blvm_database(data_dir: &Path) -> bool {
        let redb_path = data_dir.join("redb.db");
        if redb_path.exists() {
            return true;
        }
        let sled_path = data_dir.join("sled");
        if sled_path.exists() {
            return true;
        }
        let rocksdb_path = data_dir.join("rocksdb");
        if rocksdb_path.exists() {
            return true;
        }
        false
    }

    /// Open Core chainstate LevelDB at `core_dir/chainstate`.
    #[cfg(feature = "rocksdb")]
    pub fn open_bitcoin_core_database(
        core_dir: &Path,
        _network: CoreDataNetwork,
    ) -> Result<Box<dyn crate::storage::database::Database>> {
        use crate::storage::database::rocksdb_impl::RocksDBDatabase;

        BitcoinCoreDetection::verify_database(core_dir).with_context(|| {
            format!("Bitcoin Core chainstate not found or invalid under {core_dir:?}")
        })?;

        let db = RocksDBDatabase::open_bitcoin_core(core_dir)?;
        Ok(Box::new(db))
    }

    /// Open Core `blocks/index` LevelDB at `core_dir/blocks/index`.
    #[cfg(feature = "rocksdb")]
    pub fn open_bitcoin_core_block_index(
        core_dir: &Path,
    ) -> Result<Box<dyn crate::storage::database::Database>> {
        use crate::storage::database::rocksdb_impl::RocksDBDatabase;

        let index_path = core_dir.join("blocks").join("index");
        if !index_path.exists() {
            anyhow::bail!("Bitcoin Core block index not found at {:?}", index_path);
        }
        BitcoinCoreDetection::detect_db_format(&index_path)
            .with_context(|| format!("Invalid LevelDB format for block index at {index_path:?}"))?;
        let db = RocksDBDatabase::open_bitcoin_core_block_index(core_dir)?;
        Ok(Box::new(db))
    }

    /// Fail if Bitcoin Core appears to hold a lock on the datadir (bitcoind still running).
    #[cfg(feature = "rocksdb")]
    pub fn ensure_not_locked(core_dir: &Path) -> Result<()> {
        Self::ensure_bitcoind_not_running(core_dir)?;

        if Self::leveldb_lock_held(&core_dir.join("chainstate").join("LOCK"))? {
            anyhow::bail!(
                "Bitcoin Core chainstate is locked at {:?}. Stop bitcoind before migrating or starting BLVM against this datadir.",
                core_dir
            );
        }
        if Self::leveldb_lock_held(&core_dir.join("blocks").join("index").join("LOCK"))? {
            anyhow::bail!(
                "Bitcoin Core block index is locked at {:?}. Stop bitcoind before migrating or starting BLVM against this datadir.",
                core_dir
            );
        }
        Ok(())
    }

    /// True when `bitcoind.pid` points at a live process.
    fn ensure_bitcoind_not_running(core_dir: &Path) -> Result<()> {
        let pid_file = core_dir.join("bitcoind.pid");
        if !pid_file.is_file() {
            return Ok(());
        }
        let Ok(contents) = std::fs::read_to_string(&pid_file) else {
            return Ok(());
        };
        let Ok(pid) = contents.trim().parse::<u32>() else {
            return Ok(());
        };
        if Self::process_alive(pid) {
            anyhow::bail!(
                "Bitcoin Core appears to be running (pid {pid}, {:?}). Stop bitcoind before migrating or starting BLVM against this datadir.",
                pid_file
            );
        }
        Ok(())
    }

    fn process_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        #[cfg(unix)]
        {
            #[cfg(feature = "libc")]
            {
                unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
            }
            #[cfg(not(feature = "libc"))]
            {
                let _ = pid;
                return false;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            false
        }
    }

    /// LevelDB leaves a `LOCK` file on disk after shutdown; only an active `flock` means bitcoind is open.
    fn leveldb_lock_held(lock_path: &Path) -> Result<bool> {
        if !lock_path.is_file() {
            return Ok(false);
        }
        #[cfg(all(unix, feature = "libc"))]
        {
            use std::fs::OpenOptions;
            use std::os::unix::io::AsRawFd;

            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(lock_path)
                .with_context(|| format!("open LevelDB LOCK at {lock_path:?}"))?;
            let fd = file.as_raw_fd();
            let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if ret == 0 {
                unsafe {
                    libc::flock(fd, libc::LOCK_UN);
                }
                return Ok(false);
            }
            if ret == -1 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    return Ok(true);
                }
                // EINTR etc. — treat as locked to be safe.
                if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                    return Ok(true);
                }
            }
            Ok(true)
        }
        #[cfg(not(all(unix, feature = "libc")))]
        {
            let _ = lock_path;
            Ok(false)
        }
    }

    #[cfg(not(feature = "rocksdb"))]
    pub fn ensure_not_locked(_core_dir: &Path) -> Result<()> {
        Ok(())
    }

    #[cfg(not(feature = "rocksdb"))]
    pub fn open_bitcoin_core_database(
        _data_dir: &Path,
        _network: CoreDataNetwork,
    ) -> Result<Box<dyn crate::storage::database::Database>> {
        Err(anyhow::anyhow!("RocksDB feature not enabled"))
    }

    #[cfg(not(feature = "rocksdb"))]
    pub fn open_bitcoin_core_block_index(
        _core_dir: &Path,
    ) -> Result<Box<dyn crate::storage::database::Database>> {
        Err(anyhow::anyhow!("RocksDB feature not enabled"))
    }
}
