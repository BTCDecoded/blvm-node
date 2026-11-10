//! Pure-Rust LevelDB reader for Bitcoin Core chainstate.
//!
//! Bitcoin Core uses LevelDB with `.ldb` SSTable files (LevelDB magic `0xdb4775248b80fb57`).
//! RocksDB (`rocksdb` crate) cannot open these files — it segfaults when parsing a LevelDB
//! MANIFEST/SST since it expects RocksDB-format files.
//!
//! This module provides a thin `Database` + `Tree` wrapper around `rusty-leveldb` so the
//! migration code can read Core chainstate and block-index transparently, regardless of
//! whether the on-disk format is LevelDB (`.ldb`) or RocksDB (`.sst`).

use anyhow::{Context, Result};
use rusty_leveldb::{DB, LdbIterator, Options};
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::storage::database::{BatchWriter, Database, Tree};

// ─── Public helpers ────────────────────────────────────────────────────────────

/// Return `true` when `dir` contains `.sst` files mixed with `.ldb` files — a state that
/// occurs when a LevelDB database was partially compacted by RocksDB.  Neither reader can
/// safely read all entries in this state: rusty-leveldb misses the `.sst` files, and
/// RocksDB 10.x segfaults when it tries to open the `.ldb` files via its own SST reader.
pub fn dir_has_mixed_sst_formats(dir: &Path) -> bool {
    let entries = match dir.read_dir() {
        Ok(e) => e,
        Err(_) => return false,
    };

    let mut has_ldb = false;
    let mut has_sst_or_rocksdb_marker = false;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if n == "IDENTITY" || n.starts_with("OPTIONS-") {
            has_sst_or_rocksdb_marker = true;
        }
        let ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_owned())
            .unwrap_or_default();
        if ext == "ldb" {
            has_ldb = true;
        }
        if ext == "sst" {
            has_sst_or_rocksdb_marker = true;
        }
    }

    has_ldb && has_sst_or_rocksdb_marker
}

/// Return `true` when `dir` contains a **true LevelDB** database (`.ldb` SSTable format).
///
/// Decision rules (in priority order):
///
/// 1. If `OPTIONS-*` or `IDENTITY` exists → RocksDB database, return `false`.
///    RocksDB writes these files; standard LevelDB does not.  A mixed directory
///    (old `.ldb` files alongside newer `.sst` files) is still a RocksDB database
///    once RocksDB has taken it over.
///
/// 2. If at least one `.ldb` file exists and rule 1 did not fire → true LevelDB.
///
/// 3. Otherwise → not recognised as LevelDB.
pub fn dir_is_leveldb(dir: &Path) -> bool {
    let entries = match dir.read_dir() {
        Ok(e) => e,
        Err(_) => return false,
    };

    let mut has_ldb = false;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();

        // RocksDB marker files — their presence means RocksDB owns this directory.
        if n == "IDENTITY" || n.starts_with("OPTIONS-") {
            return false;
        }

        if entry
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|s| s == "ldb")
            .unwrap_or(false)
        {
            has_ldb = true;
        }
    }

    has_ldb
}

// ─── Database impl ─────────────────────────────────────────────────────────────

/// Read-only LevelDB database backed by `rusty-leveldb`.
///
/// Used as the source when migrating from a Bitcoin Core chainstate that stores
/// SSTable data in LevelDB `.ldb` format (all standard pre-2025 Bitcoin Core
/// nodes).  Write operations return `Err("read-only")` immediately.
pub struct LevelDbDatabase {
    path: PathBuf,
    // The DB is opened once; we keep it behind Arc<Mutex<…>> so the Tree wrapper
    // (which also holds a clone of the Arc) can lock it for individual gets/iters.
    db: Arc<Mutex<DB>>,
}

impl LevelDbDatabase {
    /// Open the LevelDB database rooted at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let mut opt = Options::default();
        opt.create_if_missing = false;
        opt.error_if_exists = false;

        let db =
            DB::open(path, opt).with_context(|| format!("Failed to open LevelDB at {path:?}"))?;

        Ok(Self {
            path: path.to_path_buf(),
            db: Arc::new(Mutex::new(db)),
        })
    }
}

impl Database for LevelDbDatabase {
    fn open_tree(&self, _name: &str) -> Result<Box<dyn Tree>> {
        // LevelDB has a single flat keyspace; all names map to the same store.
        Ok(Box::new(LevelDbTree {
            db: Arc::clone(&self.db),
        }))
    }

    fn flush(&self) -> Result<()> {
        Ok(()) // read-only; nothing to flush
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ─── Tree impl ─────────────────────────────────────────────────────────────────

/// Read-only view of a LevelDB database, implementing the `Tree` trait.
pub struct LevelDbTree {
    db: Arc<Mutex<DB>>,
}

/// Iterator returned by [`LevelDbTree::iter`].
///
/// Spawns a background thread that holds the DB lock and streams all entries
/// through a sync-channel.  This avoids loading all UTXO entries (~170M on
/// mainnet) into RAM at once, while also avoiding `unsafe` lifetime games.
struct LevelDbChannelIter {
    rx: std::sync::mpsc::Receiver<Result<(Vec<u8>, Vec<u8>)>>,
}

impl Iterator for LevelDbChannelIter {
    type Item = Result<(Vec<u8>, Vec<u8>)>;
    fn next(&mut self) -> Option<Self::Item> {
        self.rx.recv().ok()
    }
}

impl Tree for LevelDbTree {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut db = self.db.lock().unwrap();
        Ok(db.get(key).map(|b| b.to_vec()))
    }

    fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(Vec<u8>, Vec<u8>)>>(2048);
        let db_arc = Arc::clone(&self.db);

        std::thread::spawn(move || {
            let mut db = match db_arc.lock() {
                Ok(g) => g,
                Err(e) => {
                    let _ = tx.send(Err(anyhow::anyhow!("LevelDB lock poisoned: {e}")));
                    return;
                }
            };
            let mut iter = match db.new_iter() {
                Ok(it) => it,
                Err(status) => {
                    let _ = tx.send(Err(anyhow::anyhow!(
                        "LevelDB new_iter failed: {:?}",
                        status
                    )));
                    return;
                }
            };
            iter.seek_to_first();
            while iter.valid() {
                if let Some((k, v)) = iter.current() {
                    if tx.send(Ok((k.to_vec(), v.to_vec()))).is_err() {
                        break; // consumer dropped; stop early
                    }
                }
                iter.advance();
            }
            // tx is dropped here, causing the receiver to stop.
        });

        Box::new(LevelDbChannelIter { rx })
    }

    // ── Read-only stubs ──────────────────────────────────────────────────────

    fn contains_key(&self, key: &[u8]) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    fn len(&self) -> Result<usize> {
        // Counting by full scan is O(n); avoid unless truly necessary.
        // Migration code never calls len() on source trees.
        let mut db = self.db.lock().unwrap();
        let mut iter = db
            .new_iter()
            .map_err(|s| anyhow::anyhow!("LevelDB iter (len): {:?}", s))?;
        let mut count = 0usize;
        iter.seek_to_first();
        while iter.valid() {
            if iter.current().is_some() {
                count += 1;
            }
            iter.advance();
        }
        Ok(count)
    }

    // ── Write stubs — always error ───────────────────────────────────────────

    fn insert(&self, _key: &[u8], _value: &[u8]) -> Result<()> {
        anyhow::bail!("LevelDbTree is read-only (Bitcoin Core chainstate source)")
    }

    fn remove(&self, _key: &[u8]) -> Result<()> {
        anyhow::bail!("LevelDbTree is read-only")
    }

    fn clear(&self) -> Result<()> {
        anyhow::bail!("LevelDbTree is read-only")
    }

    fn batch(&self) -> Result<Box<dyn BatchWriter + '_>> {
        anyhow::bail!("LevelDbTree is read-only")
    }
}
