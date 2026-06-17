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
        let heed3_path = data_dir.join("heed3");
        if heed3_path.exists() {
            return true;
        }
        false
    }

    /// Open Core chainstate at `core_dir/chainstate`.
    ///
    /// Automatically selects the correct reader:
    /// - `.ldb` files → `rusty-leveldb` (true LevelDB format, produced by all standard Bitcoin
    ///   Core releases).  RocksDB segfaults when trying to open these.
    /// - `.sst` files → RocksDB reader (produced by patched builds or regtest fixtures
    ///   generated with a RocksDB-linked Core).
    #[cfg(feature = "rocksdb")]
    pub fn open_bitcoin_core_database(
        core_dir: &Path,
        _network: CoreDataNetwork,
    ) -> Result<Box<dyn crate::storage::database::Database>> {
        use crate::storage::bitcoin_core_leveldb::{LevelDbDatabase, dir_is_leveldb};
        use crate::storage::database::rocksdb_impl::RocksDBDatabase;

        BitcoinCoreDetection::verify_database(core_dir).with_context(|| {
            format!("Bitcoin Core chainstate not found or invalid under {core_dir:?}")
        })?;

        let chainstate_path = core_dir.join("chainstate");
        if dir_is_leveldb(&chainstate_path) {
            tracing::debug!("[CORE_IMPORT] chainstate has .ldb files — using rusty-leveldb reader");
            let db = LevelDbDatabase::open(&chainstate_path)?;
            return Ok(Box::new(db));
        }

        tracing::debug!("[CORE_IMPORT] chainstate has .sst files — using RocksDB reader");
        let db = RocksDBDatabase::open_bitcoin_core(core_dir)?;
        Ok(Box::new(db))
    }

    /// Open Core `blocks/index` at `core_dir/blocks/index`.
    ///
    /// Same format detection as [`open_bitcoin_core_database`].
    ///
    /// Returns `Err` (without crashing) when the database contains both LevelDB `.ldb` files
    /// and RocksDB `.sst` files.  This mixed state arises when a standard bitcoind datadir is
    /// opened by a RocksDB-linked node (or BLVM tooling) which compacts _some_ SSTs to `.sst`
    /// format but leaves the original `.ldb` files referenced by the MANIFEST.  RocksDB 10.x
    /// segfaults when it encounters the `.ldb` SSTs; rusty-leveldb misses all `.sst` entries.
    /// The caller should treat `Err` as "block index unavailable" and fall back gracefully.
    #[cfg(feature = "rocksdb")]
    pub fn open_bitcoin_core_block_index(
        core_dir: &Path,
    ) -> Result<Box<dyn crate::storage::database::Database>> {
        use crate::storage::bitcoin_core_leveldb::{
            LevelDbDatabase, dir_has_mixed_sst_formats, dir_is_leveldb,
        };
        use crate::storage::database::rocksdb_impl::RocksDBDatabase;

        let index_path = core_dir.join("blocks").join("index");
        if !index_path.exists() {
            anyhow::bail!("Bitcoin Core block index not found at {:?}", index_path);
        }
        BitcoinCoreDetection::detect_db_format(&index_path)
            .with_context(|| format!("Invalid LevelDB format for block index at {index_path:?}"))?;

        // Guard against the mixed-format state before handing off to either reader.
        if dir_has_mixed_sst_formats(&index_path) {
            anyhow::bail!(
                "blocks/index at {:?} contains both LevelDB (.ldb) and RocksDB (.sst) SSTable \
                 files.  This mixed state cannot be safely read by either reader.  \
                 Run `bitcoin-cli -datadir=<dir> -chain=<net> stop` then restart bitcoind \
                 briefly (it will compact on startup) to resolve the mixed state, \
                 or pass BLVM_SKIP_BLOCK_INDEX=1 to migrate chainstate only.",
                index_path
            );
        }

        if dir_is_leveldb(&index_path) {
            tracing::debug!(
                "[CORE_IMPORT] blocks/index has .ldb files — using rusty-leveldb reader"
            );
            let db = LevelDbDatabase::open(&index_path)?;
            return Ok(Box::new(db));
        }

        tracing::debug!("[CORE_IMPORT] blocks/index has .sst files — using RocksDB reader");
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
