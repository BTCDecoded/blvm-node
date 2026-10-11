//! heed3 (LMDB mdb.master3) storage backend.
//!
//! Uses LMDB MVCC: many concurrent read transactions (`RoTxn`) without blocking writers,
//! and a single writer at a time. `WithoutTls` read transactions are `Send` so IBD
//! validation workers can load UTXOs in parallel.

use super::{BatchWriter, Database, KNOWN_TREE_NAMES, Tree};
use anyhow::{Context, Result};
use heed3::types::Bytes;
use heed3::{
    Database as HeedDatabase, Env, EnvFlags, EnvOpenOptions, FlagSetMode, PutFlags, WithoutTls,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

type ByteDb = HeedDatabase<Bytes, Bytes>;
type HeedEnv = Env<WithoutTls>;

/// Release LMDB `data.mdb` mmap pages from the process's RSS after each fdatasync.
///
/// Release file-backed RSS from a specific LMDB `data.mdb` mmap via `madvise(MADV_DONTNEED)`.
///
/// `posix_fadvise(DONTNEED)` is NOT sufficient for mmap'd files — the kernel ignores it for
/// pages reachable through live mmap VAs. `madvise(MADV_DONTNEED)` drops the physical backing
/// of those virtual pages; LMDB re-faults them from disk on next access (safe after fdatasync).
///
/// This function targets ONLY the data.mdb belonging to `env_path`, identified by matching
/// the full pathname in `/proc/self/maps`. It never touches other LMDB environments.
///
/// Design note — why we stopped evicting the main chain store:
///   The old implementation used virtual-size heuristics (< 512 GiB = IBD UTXO store,
///   ≥ 512 GiB = main chain) and evicted both with FORCE_DUAL_MADVISE=1. That caused
///   the block-storage write thread (which writes to the main chain heed3 at every block
///   flush) to re-fault every B-tree node it touched — producing 43-second IBD_BLOCK_FLUSH
///   stalls at h>300k. Those stalls filled the durability channel, forced RSS to 47-49 GB,
///   and eventually disconnected the durability thread (restart from watermark).
///   Now each flush_to_disk() call is scoped to the store it was called on, so the main
///   chain heed3 pages stay warm and block flushes remain fast (<1 s).
#[cfg(all(target_os = "linux", feature = "libc"))]
fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .unwrap_or(0)
}

/// Shared implementation for `lmdb_madvise_dontneed` (post-fdatasync at checkpoints).
/// Per-ADD-batch noflush madvise was removed: it evicted ~10 GB and refaulted
/// (adds_ms 1.0–1.7s). Checkpoint-only is the KEEP cadence.
#[cfg(all(target_os = "linux", feature = "libc"))]
fn lmdb_madvise_for_path(env_path: &Path) {
    use std::io::BufRead;
    // Resolve the full canonical path so symlinks don't confuse the maps match.
    let data_mdb = env_path.join("data.mdb");
    let target = std::fs::canonicalize(&data_mdb).unwrap_or(data_mdb);
    let target_str = target.to_string_lossy().into_owned();

    let rss_before = rss_kb();
    let Ok(maps) = std::fs::File::open("/proc/self/maps") else {
        return;
    };
    let mut evicted_ranges: usize = 0;
    let mut evicted_bytes: usize = 0;
    for line in std::io::BufReader::new(maps).lines().map_while(Result::ok) {
        // Format: "addr_start-addr_end perms offset dev ino pathname"
        // Only match the exact target path.
        if !line.ends_with(target_str.as_str()) {
            continue;
        }
        let Some(range) = line.split_whitespace().next() else {
            continue;
        };
        let mut parts = range.splitn(2, '-');
        let (Some(s), Some(e)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(start), Ok(end)) = (usize::from_str_radix(s, 16), usize::from_str_radix(e, 16))
        else {
            continue;
        };
        if end <= start {
            continue;
        }
        unsafe {
            libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_DONTNEED);
        }
        evicted_ranges += 1;
        evicted_bytes += end - start;
    }
    let rss_after = rss_kb();
    let freed_mb = rss_before.saturating_sub(rss_after) / 1024;
    let mapped_gb = evicted_bytes / (1024 * 1024 * 1024);
    tracing::info!(
        "[MADVISE] dontneed {} range(s) ({} GiB virtual) for {:?}: \
         rss {}MB → {}MB (freed {}MB)",
        evicted_ranges,
        mapped_gb,
        target.file_name().unwrap_or_default(),
        rss_before / 1024,
        rss_after / 1024,
        freed_mb,
    );
}

/// Returns MemAvailable from /proc/meminfo in kilobytes, or 0 on failure.
#[cfg(all(target_os = "linux", feature = "libc"))]
fn mem_available_kb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemAvailable:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .unwrap_or(0)
}

/// Called from `flush_to_disk()` — after fdatasync, conditionally evict this env's data.mdb pages.
///
/// Only evicts when MemAvailable is below a threshold (default 12 GB). Below that threshold,
/// the kernel manages page eviction naturally — UTXO B-tree pages that are referenced by
/// validation workers will stay warm and re-faults are avoided. Above the threshold the
/// UTXO store's file-backed RSS (3–8 GB at h=300–400k) is not threatening; evicting and
/// immediately re-faulting the same pages wastes 10–20ms of validation time per checkpoint.
///
/// The threshold is tunable via BLVM_IBD_MADVISE_THRESHOLD_GB (default 12).
fn lmdb_madvise_dontneed(env_path: &Path) {
    #[cfg(all(target_os = "linux", feature = "libc"))]
    {
        let threshold_gb: u64 = std::env::var("BLVM_IBD_MADVISE_THRESHOLD_GB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(12);
        let avail_kb = mem_available_kb();
        let avail_gb = avail_kb / (1024 * 1024);
        if avail_gb >= threshold_gb {
            tracing::debug!(
                "[MADVISE] skip (MemAvailable={}GB >= threshold={}GB)",
                avail_gb,
                threshold_gb
            );
            return;
        }
        tracing::info!(
            "[MADVISE] threshold triggered (MemAvailable={}GB < {}GB), evicting {:?}",
            avail_gb,
            threshold_gb,
            env_path.file_name().unwrap_or_default()
        );
        lmdb_madvise_for_path(env_path);
    }
    #[cfg(not(all(target_os = "linux", feature = "libc")))]
    let _ = env_path;
}

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
    /// Open the LMDB environment for normal (post-IBD) operation.
    pub fn new<P: AsRef<Path>>(
        data_dir: P,
        storage_config: Option<&crate::config::StorageConfig>,
    ) -> Result<Self> {
        Self::new_inner(data_dir, storage_config, false, None)
    }

    /// Open the LMDB environment in IBD mode: `MDB_NOSYNC` is set so every commit skips
    /// `fdatasync`. This drops per-commit durability to zero, but IBD does not need it —
    /// blocks can be re-downloaded on crash and the UTXO state is protected by the explicit
    /// `flush_disk()` → `force_sync()` calls at each UTXO watermark boundary.
    ///
    /// Without `MDB_NOSYNC` a 400-block IBD flush spawns ~160 write transactions (blocks,
    /// chain index, undo logs), each calling `fdatasync`. At 200 ms/fsync on an NVMe under
    /// IBD write pressure that is ~32 s per flush cycle, causing the `IBD_WATCHDOG` stalls.
    /// With `MDB_NOSYNC` the same cycle takes ≤ 1 s.
    pub fn new_for_ibd<P: AsRef<Path>>(
        data_dir: P,
        storage_config: Option<&crate::config::StorageConfig>,
    ) -> Result<Self> {
        Self::new_inner(data_dir, storage_config, true, None)
    }

    /// Like `new_for_ibd` but with an explicit LMDB map size (in MiB), bypassing the env-var.
    /// Used by `create_ibd_utxo_standalone_db` to open a fresh UTXO-only environment without
    /// mutating process-global env variables (which is unsafe in Rust 2024).
    pub fn new_for_ibd_with_map_size_mb<P: AsRef<Path>>(
        data_dir: P,
        storage_config: Option<&crate::config::StorageConfig>,
        map_size_mb: usize,
    ) -> Result<Self> {
        Self::new_inner(data_dir, storage_config, true, Some(map_size_mb))
    }

    fn new_inner<P: AsRef<Path>>(
        data_dir: P,
        storage_config: Option<&crate::config::StorageConfig>,
        ibd_nosync: bool,
        map_size_mb_override: Option<usize>,
    ) -> Result<Self> {
        let heed_cfg = storage_config.and_then(|s| s.heed3.as_ref());
        let dbcache_mb: usize = std::env::var("BLVM_DBCACHE_MB")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| storage_config.map(|s| s.dbcache_mb))
            .unwrap_or(450);

        let map_size_mb: usize = resolve_heed3_map_size_mb(
            data_dir.as_ref(),
            storage_config,
            map_size_mb_override,
            dbcache_mb,
        );

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
            "[HEED3] opening LMDB at {:?} map_size={} MB max_readers={} max_dbs={} nosync={}",
            data_path,
            map_size_mb,
            max_readers,
            max_dbs,
            ibd_nosync,
        );

        let mut options = EnvOpenOptions::new().read_txn_without_tls();
        if ibd_nosync {
            // SAFETY: MDB_NOSYNC | MDB_NOMETASYNC for IBD. Safe because:
            // 1. Blocks can be re-downloaded on crash (no durability required).
            // 2. UTXO state uses explicit flush_disk() / force_sync() at watermark boundaries.
            // 3. MDB_NOMETASYNC skips the per-commit meta-page sync in addition to MDB_NOSYNC's
            //    data-page skip. Eliminates the last per-commit fdatasync call, further reducing
            //    commit latency for the high-frequency UTXO flush path. On crash the meta page
            //    may be stale; LMDB recovers by scanning backwards through valid meta pages, or
            //    the node restarts IBD from the last durable watermark checkpoint.
            //
            // MDB_WRITEMAP is intentionally NOT set here: with WRITEMAP, LMDB writes B-tree
            // pages directly through its mmap, making those pages impossible to evict via
            // posix_fadvise(DONTNEED) (the kernel ignores fadvise on mmap'd pages). Without
            // WRITEMAP, write() calls produce normal file-backed pages that become clean after
            // fdatasync and can then be evicted by fadvise, keeping RSS bounded during IBD.
            // The extra kernel copy overhead of pwrite() vs mmap-write is negligible compared
            // to the LMDB B-tree traversal cost (~2-6s per checkpoint batch).
            unsafe {
                options.flags(EnvFlags::NO_SYNC | EnvFlags::NO_META_SYNC);
            }
        }
        let env = unsafe {
            options
                .map_size(map_size)
                .max_readers(max_readers)
                .max_dbs(max_dbs)
                .open(&data_path)
                .context("heed3 EnvOpenOptions::open failed")?
        };
        let env = Arc::new(env);

        // Warn if the existing data.mdb is already near the map limit.  When LMDB
        // approaches its map size its page-reclamation scan becomes O(map_size) and
        // write transactions freeze rather than returning MDB_MAP_FULL promptly.
        {
            let mdb_path = data_path.join("data.mdb");
            if let Ok(meta) = std::fs::metadata(&mdb_path) {
                let file_mb = meta.len() / (1024 * 1024);
                let pct = (file_mb as f64 / map_size_mb as f64 * 100.0) as u64;
                if pct >= 80 {
                    tracing::warn!(
                        "[HEED3] data.mdb is {}% of the LMDB map limit ({}/{} MB). \
                         LMDB writes will freeze when the map is exhausted. \
                         Set a larger map via BLVM_HEED3_MAP_SIZE_MB or [storage.heed3] map_size_mb.",
                        pct,
                        file_mb,
                        map_size_mb,
                    );
                }
            }
        }

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

    /// Grow the LMDB map so `data.mdb` has at least `headroom_mb` MiB of unused map
    /// capacity. No-op if already sufficient. Must be called with no active LMDB txns
    /// on this env (Phase 3 runs after validation/download join — safe).
    ///
    /// Live 2026-07-13: Phase 3 piggyback export hit `MDB_MAP_FULL` with map=1 TiB at
    /// 99.998% full; warn-only sink kept compact running for 11h.
    pub fn ensure_map_headroom_mb(&self, headroom_mb: usize) -> Result<()> {
        let info = self.env.info();
        let map_bytes = info.map_size;
        let map_mb = map_bytes / (1024 * 1024);
        let mdb_path = self.data_path.join("data.mdb");
        let file_mb = std::fs::metadata(&mdb_path)
            .map(|m| m.len() / (1024 * 1024))
            .unwrap_or(0);
        let free_in_map = map_mb.saturating_sub(file_mb as usize);
        if free_in_map >= headroom_mb {
            tracing::info!(
                "[HEED3] map headroom OK: file={} MB map={} MB free_in_map={} MB (need ≥{})",
                file_mb,
                map_mb,
                free_in_map,
                headroom_mb
            );
            return Ok(());
        }
        let target_mb = (file_mb as usize)
            .saturating_add(headroom_mb)
            .min(4 * 1024 * 1024);
        if target_mb <= map_mb {
            return Ok(());
        }
        let page = {
            #[cfg(all(target_os = "linux", feature = "libc"))]
            {
                let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
                if p > 0 { p as usize } else { 4096usize }
            }
            #[cfg(not(all(target_os = "linux", feature = "libc")))]
            {
                4096usize
            }
        };
        let mut target_bytes = target_mb.saturating_mul(1024).saturating_mul(1024);
        // mdb_env_set_mapsize requires a multiple of the system page size.
        target_bytes = target_bytes.div_ceil(page) * page;
        tracing::warn!(
            "[HEED3] growing LMDB map {} MB → {} MB (file={} MB, need headroom={} MB)",
            map_mb,
            target_bytes / (1024 * 1024),
            file_mb,
            headroom_mb
        );
        let _guard = self.write_lock.lock();
        // SAFETY: no other write txn held; callers must not hold RoTxn across this call.
        unsafe {
            self.env
                .resize(target_bytes)
                .context("heed3 Env::resize failed")?;
        }
        Ok(())
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

fn disk_based_map_size_mb(disk_free_mb: u64) -> usize {
    if disk_free_mb > 0 {
        // 70% of free space, in MiB
        let target = (disk_free_mb * 70 / 100) as usize;
        // 48 GiB min, 4 TiB max
        target.clamp(48 * 1024, 4 * 1024 * 1024)
    } else {
        // Fallback: previous hardcoded default (256 GiB).
        262_144usize
    }
}

/// Resolve LMDB map size: explicit override > env > config > auto-tune from free disk.
/// Always applies `data.mdb + 128 GiB` headroom when resuming an existing store.
fn resolve_heed3_map_size_mb(
    data_dir: &std::path::Path,
    storage_config: Option<&crate::config::StorageConfig>,
    map_size_mb_override: Option<usize>,
    dbcache_mb: usize,
) -> usize {
    if let Some(mb) = map_size_mb_override {
        tracing::info!(
            "[HEED3] map_size {} MB from caller override (auto-tune skipped)",
            mb
        );
        return apply_existing_mdb_headroom(mb, Some(data_dir));
    }
    if let Some(mb) = std::env::var("BLVM_HEED3_MAP_SIZE_MB")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        tracing::info!(
            "[HEED3] map_size {} MB from BLVM_HEED3_MAP_SIZE_MB (auto-tune skipped)",
            mb
        );
        return apply_existing_mdb_headroom(mb, Some(data_dir));
    }
    if let Some(mb) = storage_config
        .and_then(|s| s.heed3.as_ref())
        .and_then(|c| c.map_size_mb)
    {
        tracing::info!(
            "[HEED3] map_size {} MB from [storage.heed3] map_size_mb (auto-tune skipped)",
            mb
        );
        return apply_existing_mdb_headroom(mb, Some(data_dir));
    }

    let disk_free_mb = disk_free_mb_for_path(Some(data_dir));
    let disk_based = disk_based_map_size_mb(disk_free_mb);
    let final_mb = apply_existing_mdb_headroom(disk_based, Some(data_dir));
    let _ = dbcache_mb; // kept for API compatibility
    tracing::info!(
        "[HEED3] map_size auto-tuned to {} MB (70% of {} MB free on {:?}; disk_based={} MB; \
         +data.mdb+128GiB headroom when present)",
        final_mb,
        disk_free_mb,
        data_dir,
        disk_based
    );
    final_mb
}

fn map_size_mb_default(dbcache_mb: usize, data_dir: Option<&std::path::Path>) -> usize {
    let disk_free_mb = disk_free_mb_for_path(data_dir);
    let disk_based = disk_based_map_size_mb(disk_free_mb);
    let _ = dbcache_mb; // kept for API compatibility
    apply_existing_mdb_headroom(disk_based, data_dir)
}

/// Raise `map_size_mb` to at least `data.mdb` size + headroom (default **128 GiB**,
/// capped at 4 TiB).
///
/// Env:
/// - `BLVM_HEED3_MDB_HEADROOM_MB` — override headroom (MiB) when HARD is unset.
/// - `BLVM_HEED3_MAP_SIZE_HARD=1` — skip the **128 GiB** auto-raise (wan-bench: that
///   alone turned a 64 GiB cap into ~data.mdb+128 GiB VmSize). Still ensures a **min**
///   headroom above the on-disk file so we never open at 99% full → `MDB_MAP_FULL`
///   mid-IBD (live 2026-07-14: HARD=1 + map=64 GiB + data.mdb=64 GiB → CATCH_UP restart
///   from stale `synced_tip`).
/// - `BLVM_HEED3_MDB_MIN_HEADROOM_MB` — min free map when HARD=1 (default **32768** = 32 GiB).
fn apply_existing_mdb_headroom(map_size_mb: usize, data_dir: Option<&std::path::Path>) -> usize {
    let Some(dir) = data_dir else {
        return map_size_mb;
    };
    let mdb = dir.join("heed3").join("data.mdb");
    let Ok(meta) = std::fs::metadata(&mdb) else {
        return map_size_mb;
    };
    let file_mb = (meta.len() / (1024 * 1024)) as usize;
    let hard = std::env::var("BLVM_HEED3_MAP_SIZE_HARD")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let headroom_mb: usize = if hard {
        std::env::var("BLVM_HEED3_MDB_MIN_HEADROOM_MB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(32 * 1024)
    } else {
        std::env::var("BLVM_HEED3_MDB_HEADROOM_MB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(128 * 1024)
    };
    let need = file_mb.saturating_add(headroom_mb);
    let raised = map_size_mb.max(need).min(4 * 1024 * 1024);
    if raised > map_size_mb {
        if hard {
            tracing::warn!(
                "[HEED3] HARD cap {} MB < data.mdb={} MB + min_headroom={} MB — raising to {} MB \
                 (avoids open-at-full MDB_MAP_FULL; set BLVM_HEED3_MDB_MIN_HEADROOM_MB to tune)",
                map_size_mb,
                file_mb,
                headroom_mb,
                raised
            );
        } else {
            tracing::warn!(
                "[HEED3] raising map_size {} MB → {} MB (data.mdb={} MB + {} MB headroom; \
                 set BLVM_HEED3_MAP_SIZE_HARD=1 to keep a tight cap + min headroom only)",
                map_size_mb,
                raised,
                file_mb,
                headroom_mb
            );
        }
    }
    raised
}

/// Pure helper for tests: required map size given file size + free disk.
#[cfg(test)]
pub(crate) fn map_size_mb_for_existing_file(file_mb: usize, free_mb: u64) -> usize {
    let disk_based = disk_based_map_size_mb(free_mb);
    let need = file_mb.saturating_add(128 * 1024);
    disk_based.max(need).min(4 * 1024 * 1024)
}

/// Available disk space in MiB for the filesystem containing `path`
/// (or "." if `path` is None).  Returns 0 on error.
///
/// Uses `statvfs(2)` on Linux/Unix (via the `libc` crate which is a default feature).
/// Falls back to 0 on platforms where it is unavailable, which causes the caller to
/// use the legacy 256 GiB default.
fn disk_free_mb_for_path(path: Option<&std::path::Path>) -> u64 {
    #[cfg(all(target_os = "linux", feature = "libc"))]
    {
        use std::ffi::CString;
        use std::mem::MaybeUninit;
        let raw_path = path
            .map(|p| p.as_os_str().as_encoded_bytes())
            .unwrap_or(b".");
        let c_path = match CString::new(raw_path) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        let mut st: MaybeUninit<libc::statvfs64> = MaybeUninit::uninit();
        // SAFETY: `st` is immediately initialised by `statvfs64` on rc == 0.
        let rc = unsafe { libc::statvfs64(c_path.as_ptr(), st.as_mut_ptr()) };
        if rc == 0 {
            // SAFETY: rc == 0 guarantees the struct was fully written.
            let st = unsafe { st.assume_init() };
            // f_bavail: blocks available to unprivileged callers; f_frsize: fragment size.
            let free_bytes = st.f_bavail.saturating_mul(st.f_frsize);
            return free_bytes / (1024 * 1024);
        }
        0
    }
    #[cfg(not(all(target_os = "linux", feature = "libc")))]
    {
        let _ = path;
        0
    }
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

    fn storage_root_path(&self) -> Option<std::path::PathBuf> {
        // data_path = <root>/heed3; parent = <root>
        self.data_path.parent().map(|p| p.to_path_buf())
    }

    fn db_file_size_mb(&self) -> u64 {
        // data.mdb lives directly under data_path (which is already <root>/heed3/).
        let mdb = self.data_path.join("data.mdb");
        std::fs::metadata(&mdb)
            .map(|m| m.len() / (1024 * 1024))
            .unwrap_or(0)
    }

    fn set_ibd_nosync(&self, enable: bool) -> crate::storage::database::Result<()> {
        let (flags, mode) = (
            EnvFlags::NO_SYNC,
            if enable {
                FlagSetMode::Enable
            } else {
                FlagSetMode::Disable
            },
        );
        // SAFETY: MDB_NOSYNC only affects durability guarantees, not memory safety.
        // We restore normal sync before IBD completion via flush().
        unsafe { self.env.set_flags(flags, mode) }.context("heed3 set_flags(NO_SYNC) failed")?;
        if !enable {
            // Sync everything that was written without fsync.
            self.env
                .force_sync()
                .context("heed3 force_sync (after nosync disable) failed")?;
        }
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

    fn bulk_load_sorted_kv(&self, sorted_kv: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        if sorted_kv.is_empty() {
            return Ok(());
        }
        // One LMDB write txn per bulk_load call. Export chunks at 500k keys (~60 MB pairs) —
        // splitting at MAX_BATCH_OPS tripled commit overhead without RSS benefit on ckpt trees.
        // E4 APPEND is NOT used here: per-chunk key order is not globally monotonic.
        // Export merge path calls [`Heed3Tree::write_slice_batch_append`] after a k-way merge.
        let iter = sorted_kv
            .iter()
            .map(|(k, v)| (k.as_slice(), Some(v.as_slice())));
        self.write_slice_batch(iter)
            .context("heed3 bulk_load_sorted_kv write_slice_batch failed")?;
        Ok(())
    }

    fn flush_to_disk(&self) -> crate::storage::database::Result<()> {
        // In NOSYNC mode every commit skips fdatasync. force_sync flushes all
        // pending writes to disk so callers (watermark checkpoints, IBD teardown)
        // can trust durability.
        self.env
            .force_sync()
            .context("heed3 flush_to_disk force_sync failed")?;
        // After fdatasync, release LMDB mmap pages from RSS via madvise(MADV_DONTNEED).
        // fadvise alone is insufficient because it only marks pages as evictable in the
        // kernel's page cache — it cannot evict pages still reachable via a live mmap.
        // madvise on the mmap range directly drops physical backing from those virtual
        // pages; the next LMDB access re-faults them from disk (safe post-fdatasync).
        lmdb_madvise_dontneed(self.env.path());
        Ok(())
    }

    #[cfg(feature = "heed3")]
    fn as_heed3_tree(&self) -> Option<&super::heed3_impl::Heed3Tree> {
        Some(self)
    }
}

impl Heed3Tree {
    /// Zero-copy batch write for the IBD UTXO flush hot path.
    ///
    /// Accepts `(key: &[u8], value: Option<&[u8]>)` pairs and writes them directly into a single
    /// LMDB write transaction, bypassing the intermediate `Vec<(Vec<u8>, Option<Vec<u8>>)>`
    /// buffer that `BatchWriter::put` uses. At 200k ops/batch this eliminates ~50–100 MB of
    /// `to_vec()` copies and the corresponding heap allocations.
    ///
    /// Returns the number of operations committed.
    pub fn write_slice_batch<'a>(
        &self,
        ops: impl IntoIterator<Item = (&'a [u8], Option<&'a [u8]>)>,
    ) -> anyhow::Result<usize> {
        let _guard = self.write_lock.lock();
        let mut wtxn = self.env.write_txn()?;
        let mut count = 0usize;
        for (key, value_opt) in ops {
            match value_opt {
                Some(v) => {
                    self.db.put(&mut wtxn, key, v)?;
                }
                None => {
                    self.db.delete(&mut wtxn, key)?;
                }
            }
            count += 1;
        }
        if count > 0 {
            wtxn.commit()?;
        }
        Ok(count)
    }

    /// E4: bulk-load **globally** sorted keys with `MDB_APPEND` (empty / append-only DB).
    ///
    /// Keys must be strictly ascending vs any existing key in the DB. Used by checkpoint
    /// export after a k-way merge of per-chunk sorted runs — not by ordinary `bulk_load`.
    pub fn write_slice_batch_append<'a>(
        &self,
        ops: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
    ) -> anyhow::Result<usize> {
        let _guard = self.write_lock.lock();
        let mut wtxn = self.env.write_txn()?;
        let mut count = 0usize;
        for (key, value) in ops {
            self.db
                .put_with_flags(&mut wtxn, PutFlags::APPEND, key, value)
                .with_context(|| {
                    format!("heed3 MDB_APPEND put failed at op {count} (keys not globally sorted?)")
                })?;
            count += 1;
        }
        if count > 0 {
            wtxn.commit()?;
        }
        Ok(count)
    }

    /// E5: stream globally sorted pairs into one (or few) `MDB_APPEND` write txn(s).
    ///
    /// `commit_every == 0` → single commit after all puts (fastest; dirty-page peak higher).
    /// `commit_every == N` → commit every N puts (APPEND still valid across commits when
    /// keys remain globally increasing).
    ///
    /// `next` yields the next sorted `(key, value)` or `Ok(None)` at end.
    pub fn write_append_from_fn<F>(&self, commit_every: usize, mut next: F) -> anyhow::Result<usize>
    where
        F: FnMut() -> anyhow::Result<Option<(Vec<u8>, Vec<u8>)>>,
    {
        let _guard = self.write_lock.lock();
        let mut wtxn = self.env.write_txn()?;
        let mut count = 0usize;
        let mut since_commit = 0usize;
        while let Some((key, value)) = next()? {
            self.db
                .put_with_flags(&mut wtxn, PutFlags::APPEND, &key, &value)
                .with_context(|| {
                    format!(
                        "heed3 MDB_APPEND stream put failed at op {count} (keys not globally sorted?)"
                    )
                })?;
            count += 1;
            since_commit += 1;
            if commit_every > 0 && since_commit >= commit_every {
                wtxn.commit()?;
                wtxn = self.env.write_txn()?;
                since_commit = 0;
            }
        }
        if count > 0 && (commit_every == 0 || since_commit > 0) {
            wtxn.commit()?;
        }
        Ok(count)
    }

    /// The directory path of this LMDB environment (data.mdb lives here).
    pub fn env_path(&self) -> &Path {
        self.env.path()
    }

    /// fdatasync only — no madvise. Use for the DEL-phase sync in the del-backlog
    /// loop where the preceding ADD-phase `flush_disk()` already evicted pages.
    pub fn force_sync_only(&self) -> anyhow::Result<()> {
        self.env
            .force_sync()
            .context("heed3 force_sync_only failed")
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

#[cfg(test)]
mod map_size_tests {
    use super::map_size_mb_for_existing_file;

    #[test]
    fn map_size_bumps_for_near_full_1tib_file() {
        // Live soak: file ~984652 MB at open with 1 TiB cap — need file+128GiB.
        let file_mb = 984_652usize;
        let free_mb = 300_000u64; // would clamp to ~205 GiB from free alone
        let got = map_size_mb_for_existing_file(file_mb, free_mb);
        assert!(
            got >= file_mb + 128 * 1024,
            "got {got} MB, need ≥{} MB",
            file_mb + 128 * 1024
        );
        assert!(got <= 4 * 1024 * 1024);
    }

    #[test]
    fn map_size_cap_is_4tib() {
        let got = map_size_mb_for_existing_file(3 * 1024 * 1024, 10_000_000);
        assert_eq!(got, 4 * 1024 * 1024);
    }
}
