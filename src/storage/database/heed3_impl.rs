//! heed3 (LMDB mdb.master3) storage backend.
//!
//! Uses LMDB MVCC: many concurrent read transactions (`RoTxn`) without blocking writers,
//! and a single writer at a time. `WithoutTls` read transactions are `Send` so IBD
//! validation workers can load UTXOs in parallel.

use super::{BatchWriter, Database, KNOWN_TREE_NAMES, Tree};
use anyhow::{Context, Result};
use heed3::types::Bytes;
use heed3::{Database as HeedDatabase, Env, EnvOpenOptions, WithoutTls};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

type ByteDb = HeedDatabase<Bytes, Bytes>;
type HeedEnv = Env<WithoutTls>;

/// Streaming LMDB iterator: pulls keys in batches (O(batch) memory, not full tree).
const HEED3_ITER_BATCH: usize = 4096;

struct Heed3TreeIter {
    env: Arc<HeedEnv>,
    db: ByteDb,
    resume_after: Option<Vec<u8>>,
    buffer: std::vec::IntoIter<Result<(Vec<u8>, Vec<u8>)>>,
    exhausted: bool,
}

impl Heed3TreeIter {
    fn new(env: Arc<HeedEnv>, db: ByteDb) -> Self {
        Self {
            env,
            db,
            resume_after: None,
            buffer: Vec::new().into_iter(),
            exhausted: false,
        }
    }

    fn refill(&mut self) -> Result<()> {
        if self.exhausted {
            return Ok(());
        }
        let rtxn = self.env.read_txn()?;
        let mut batch = Vec::with_capacity(HEED3_ITER_BATCH);
        let resume_after = self.resume_after.clone();

        if let Some(after) = resume_after {
            use std::ops::Bound;
            let range = (Bound::Excluded(after.as_slice()), Bound::Unbounded);
            for item in self.db.range(&rtxn, &range)?.take(HEED3_ITER_BATCH) {
                let (k, v) = item?;
                let key = k.to_vec();
                self.resume_after = Some(key.clone());
                batch.push(Ok((key, v.to_vec())));
            }
        } else {
            for item in self.db.iter(&rtxn)?.take(HEED3_ITER_BATCH) {
                let (k, v) = item?;
                let key = k.to_vec();
                self.resume_after = Some(key.clone());
                batch.push(Ok((key, v.to_vec())));
            }
        }

        if batch.len() < HEED3_ITER_BATCH {
            self.exhausted = true;
        }
        self.buffer = batch.into_iter();
        Ok(())
    }
}

impl Iterator for Heed3TreeIter {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.buffer.next() {
            return Some(item);
        }
        if self.exhausted {
            return None;
        }
        match self.refill() {
            Err(e) => {
                self.exhausted = true;
                Some(Err(e))
            }
            Ok(()) => self.buffer.next(),
        }
    }
}

/// heed3 / LMDB environment + pre-opened named sub-DBs (one per tree).
pub struct Heed3Database {
    env: Arc<HeedEnv>,
    /// LMDB allows one write txn per environment; serialize writers.
    write_lock: Arc<Mutex<()>>,
    trees: HashMap<String, ByteDb>,
    data_path: PathBuf,
}

impl Heed3Database {
    pub fn new<P: AsRef<Path>>(
        data_dir: P,
        storage_config: Option<&crate::config::StorageConfig>,
    ) -> Result<Self> {
        let heed_cfg = storage_config.and_then(|s| s.heed3.as_ref());
        let dbcache_mb: usize = std::env::var("BLVM_DBCACHE_MB")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| storage_config.map(|s| s.dbcache_mb))
            .unwrap_or(450);

        let map_size_mb: usize = std::env::var("BLVM_HEED3_MAP_SIZE_MB")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| heed_cfg.and_then(|c| c.map_size_mb))
            .unwrap_or_else(|| map_size_mb_default(dbcache_mb));

        let max_readers: u32 = std::env::var("BLVM_HEED3_MAX_READERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| heed_cfg.and_then(|c| c.max_readers))
            .unwrap_or(512);

        let max_dbs: u32 = std::env::var("BLVM_HEED3_MAX_DBS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| heed_cfg.and_then(|c| c.max_dbs))
            .unwrap_or_else(|| (KNOWN_TREE_NAMES.len() as u32).saturating_add(8));

        let data_path = data_dir.as_ref().join("heed3");
        std::fs::create_dir_all(&data_path)?;

        let map_size = map_size_mb.saturating_mul(1024).saturating_mul(1024);

        tracing::info!(
            "[HEED3] opening LMDB at {:?} map_size={} MB max_readers={} max_dbs={}",
            data_path,
            map_size_mb,
            max_readers,
            max_dbs
        );

        let mut options = EnvOpenOptions::new().read_txn_without_tls();
        let env = unsafe {
            options
                .map_size(map_size)
                .max_readers(max_readers)
                .max_dbs(max_dbs)
                .open(&data_path)
                .context("heed3 EnvOpenOptions::open failed")?
        };
        let env = Arc::new(env);

        let mut trees = HashMap::new();
        {
            let mut wtxn = env.write_txn().context("heed3 initial write_txn failed")?;
            for name in KNOWN_TREE_NAMES {
                let db = env
                    .create_database(&mut wtxn, Some(name))
                    .with_context(|| format!("heed3 create_database({name}) failed"))?;
                trees.insert((*name).to_string(), db);
            }
            wtxn.commit()
                .context("heed3 initial write_txn commit failed")?;
        }

        Ok(Self {
            env,
            write_lock: Arc::new(Mutex::new(())),
            trees,
            data_path,
        })
    }

    pub fn data_path(&self) -> &Path {
        &self.data_path
    }

    fn tree_db(&self, name: &str) -> Result<ByteDb> {
        self.trees
            .get(name)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Unknown heed3 tree name: {name}"))
    }

    /// One LMDB write transaction for all blockstore sub-DBs + recent headers (IBD flush).
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

        let db_blocks = self.tree_db("blocks")?;
        let db_headers = self.tree_db("headers")?;
        let db_witnesses = self.tree_db("witnesses")?;
        let db_height = self.tree_db("height_index")?;
        let db_h2h = self.tree_db("hash_to_height")?;
        let db_meta = self.tree_db("block_metadata")?;
        let db_recent = self.tree_db("recent_headers")?;

        let _guard = self.write_lock.lock();
        let mut wtxn = self.env.write_txn()?;

        for &i in flush_order {
            let height = heights[i];
            let key = block_height_row_key(height, &block_hashes[i]);
            db_blocks.put(&mut wtxn, key.as_slice(), block_data[i].as_slice())?;
            db_headers.put(&mut wtxn, key.as_slice(), header_data[i].as_slice())?;
            if let Some(w) = witness_blobs[i].as_ref() {
                db_witnesses.put(&mut wtxn, key.as_slice(), w.as_slice())?;
            }
            let height_key = height.to_be_bytes();
            db_height.put(&mut wtxn, &height_key, block_hashes[i].as_slice())?;
            db_h2h.put(&mut wtxn, block_hashes[i].as_slice(), &height_key)?;
            db_meta.put(&mut wtxn, key.as_slice(), metadata_blobs[i].as_slice())?;
        }

        for &(height, ref header_bytes) in recent_entries {
            let height_bytes = height.to_be_bytes();
            db_recent.put(&mut wtxn, &height_bytes, header_bytes.as_slice())?;
            if height > 11 {
                let rm = (height - 12).to_be_bytes();
                db_recent.delete(&mut wtxn, &rm)?;
            }
        }

        wtxn.commit()?;
        Ok(())
    }
}

fn map_size_mb_default(dbcache_mb: usize) -> usize {
    // Mainnet-scale UTXO + indexes: default 64 GiB unless operator overrides.
    dbcache_mb.saturating_mul(128).max(65_536)
}

impl Database for Heed3Database {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>> {
        if name.starts_with("module_") || name == "modules" {
            return Err(anyhow::anyhow!(
                "Module storage has been removed. Use blvm_sdk::module::open_module_db."
            ));
        }
        let db = self.tree_db(name)?;
        Ok(Box::new(Heed3Tree {
            env: Arc::clone(&self.env),
            write_lock: Arc::clone(&self.write_lock),
            db,
            name: name.to_string(),
        }))
    }

    fn flush(&self) -> Result<()> {
        self.env.force_sync().context("heed3 force_sync failed")?;
        Ok(())
    }
}

pub struct Heed3Tree {
    env: Arc<HeedEnv>,
    write_lock: Arc<Mutex<()>>,
    db: ByteDb,
    name: String,
}

impl Heed3Tree {
    /// Return the underlying LMDB environment. Callers can open a `RoTxn` and use
    /// [`Self::get_many_heed3`] to read values as `&[u8]` slices backed directly by
    /// mmap'd LMDB pages — zero intermediate `Vec<u8>` allocation per value.
    #[inline]
    pub fn env(&self) -> &Arc<HeedEnv> {
        &self.env
    }

    /// Batch-read `keys` inside an existing read transaction.
    ///
    /// Returns slices into LMDB's mmap'd pages. The slices are valid until `rtxn` drops.
    /// Caller must not hold any references past the end of `rtxn`'s scope.
    pub fn get_many_heed3<'txn>(
        &self,
        keys: &[&[u8]],
        rtxn: &'txn heed3::RoTxn<'_, heed3::WithoutTls>,
    ) -> Result<Vec<Option<&'txn [u8]>>> {
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            out.push(self.db.get(rtxn, key)?);
        }
        Ok(out)
    }

    /// Stream all entries in this tree as `(&[u8], &[u8])` slices backed by mmap'd LMDB
    /// pages — no `Vec<u8>` allocation per entry.
    ///
    /// Opens its own `RoTxn` internally so the caller does not need to manage one. The
    /// closure `f` receives `(key_bytes, value_bytes)` and must not retain references past
    /// each call (slices point into a short-lived read transaction window).
    ///
    /// Returns early with the first error from either LMDB or the closure.
    pub fn scan_heed3(&self, mut f: impl FnMut(&[u8], &[u8]) -> Result<()>) -> Result<()> {
        let rtxn = self.env.read_txn()?;
        for result in self.db.iter(&rtxn)? {
            let (k, v) = result?;
            f(k, v)?;
        }
        Ok(())
    }
}

impl Tree for Heed3Tree {
    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let _guard = self.write_lock.lock();
        let mut wtxn = self.env.write_txn()?;
        self.db.put(&mut wtxn, key, value)?;
        wtxn.commit()?;
        Ok(())
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let rtxn = self.env.read_txn()?;
        match self.db.get(&rtxn, key)? {
            Some(v) => Ok(Some(v.to_vec())),
            None => Ok(None),
        }
    }

    fn get_many(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>> {
        let rtxn = self.env.read_txn()?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            out.push(self.db.get(&rtxn, key)?.map(|v| v.to_vec()));
        }
        Ok(out)
    }

    fn get_many_no_cache(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>> {
        // LMDB has no block cache knob; single read txn is the efficient bulk path.
        self.get_many(keys)
    }

    fn remove(&self, key: &[u8]) -> Result<()> {
        let _guard = self.write_lock.lock();
        let mut wtxn = self.env.write_txn()?;
        self.db.delete(&mut wtxn, key)?;
        wtxn.commit()?;
        Ok(())
    }

    fn contains_key(&self, key: &[u8]) -> Result<bool> {
        let rtxn = self.env.read_txn()?;
        Ok(self.db.get(&rtxn, key)?.is_some())
    }

    fn clear(&self) -> Result<()> {
        let _guard = self.write_lock.lock();
        let mut wtxn = self.env.write_txn()?;
        self.db.clear(&mut wtxn)?;
        wtxn.commit()?;
        Ok(())
    }

    fn len(&self) -> Result<usize> {
        let rtxn = self.env.read_txn()?;
        Ok(self.db.len(&rtxn)? as usize)
    }

    fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
        Box::new(Heed3TreeIter::new(Arc::clone(&self.env), self.db))
    }

    fn batch(&self) -> Result<Box<dyn BatchWriter + '_>> {
        Ok(Box::new(Heed3BatchWriter {
            env: Arc::clone(&self.env),
            write_lock: Arc::clone(&self.write_lock),
            db: self.db,
            pending: Vec::new(),
        }))
    }

    #[cfg(feature = "heed3")]
    fn as_heed3_tree(&self) -> Option<&super::heed3_impl::Heed3Tree> {
        Some(self)
    }
}

/// heed3 / LMDB for **module** KV (`open_module_db`): dynamic named sub-DBs on `open_tree`.
pub struct Heed3ModuleDatabase {
    env: Arc<HeedEnv>,
    write_lock: Arc<Mutex<()>>,
    trees: Mutex<HashMap<String, ByteDb>>,
    data_path: PathBuf,
}

impl Heed3ModuleDatabase {
    pub fn new<P: AsRef<Path>>(
        data_dir: P,
        storage_config: Option<&crate::config::StorageConfig>,
    ) -> Result<Self> {
        let heed_cfg = storage_config.and_then(|s| s.heed3.as_ref());
        let map_size_mb: usize = std::env::var("BLVM_MODULE_HEED3_MAP_SIZE_MB")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| heed_cfg.and_then(|c| c.map_size_mb))
            .unwrap_or(1024);

        let max_readers: u32 = std::env::var("BLVM_HEED3_MAX_READERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| heed_cfg.and_then(|c| c.max_readers))
            .unwrap_or(128);

        let max_dbs: u32 = std::env::var("BLVM_MODULE_HEED3_MAX_DBS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| heed_cfg.and_then(|c| c.max_dbs))
            .unwrap_or(256);

        let data_path = data_dir.as_ref().join("heed3_module");
        std::fs::create_dir_all(&data_path)?;

        let map_size = map_size_mb.saturating_mul(1024).saturating_mul(1024);

        tracing::info!(
            "[HEED3_MODULE] opening LMDB at {:?} map_size={} MB max_readers={} max_dbs={}",
            data_path,
            map_size_mb,
            max_readers,
            max_dbs
        );

        let mut options = EnvOpenOptions::new().read_txn_without_tls();
        let env = unsafe {
            options
                .map_size(map_size)
                .max_readers(max_readers)
                .max_dbs(max_dbs)
                .open(&data_path)
                .context("heed3 module EnvOpenOptions::open failed")?
        };

        Ok(Self {
            env: Arc::new(env),
            write_lock: Arc::new(Mutex::new(())),
            trees: Mutex::new(HashMap::new()),
            data_path,
        })
    }

    fn get_or_create_tree(&self, name: &str) -> Result<ByteDb> {
        if let Some(db) = self.trees.lock().get(name) {
            return Ok(*db);
        }
        let _guard = self.write_lock.lock();
        if let Some(db) = self.trees.lock().get(name) {
            return Ok(*db);
        }
        let mut wtxn = self.env.write_txn()?;
        let db = self
            .env
            .create_database(&mut wtxn, Some(name))
            .with_context(|| format!("heed3 module create_database({name}) failed"))?;
        wtxn.commit()?;
        self.trees.lock().insert(name.to_string(), db);
        Ok(db)
    }
}

impl Database for Heed3ModuleDatabase {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>> {
        let db = self.get_or_create_tree(name)?;
        Ok(Box::new(Heed3Tree {
            env: Arc::clone(&self.env),
            write_lock: Arc::clone(&self.write_lock),
            db,
            name: name.to_string(),
        }))
    }

    fn flush(&self) -> Result<()> {
        self.env
            .force_sync()
            .context("heed3 module force_sync failed")?;
        Ok(())
    }
}

struct Heed3BatchWriter {
    env: Arc<HeedEnv>,
    write_lock: Arc<Mutex<()>>,
    db: ByteDb,
    pending: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl BatchWriter for Heed3BatchWriter {
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
        let _guard = self.write_lock.lock();
        let mut wtxn = self.env.write_txn()?;
        for (key, value) in self.pending {
            match value {
                Some(v) => {
                    self.db.put(&mut wtxn, key.as_slice(), v.as_slice())?;
                }
                None => {
                    self.db.delete(&mut wtxn, key.as_slice())?;
                }
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    fn commit_no_wal(self: Box<Self>) -> Result<()> {
        // LMDB has no separate WAL; commit is already durable on sync policy.
        self.commit()
    }

    fn len(&self) -> usize {
        self.pending.len()
    }
}
