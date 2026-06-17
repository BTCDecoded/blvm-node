//! `UtxoIndex`: age-tiered UTXO index with disk overflow.
//!
//! Architecture:
//! - `K_AGES = 7` tiers allocated, but only ages `0..K_DISK_EVICTION_AGE` are used in memory.
//! - `K_MUTABLE_AGES = 3`: ages 0–2 accept appends and merged pushes from younger ages.
//! - `K_FAN_IN = 8`: each age merges once it holds ≥8 runs.
//! - `K_DISK_EVICTION_AGE = K_MUTABLE_AGES = 3`: merges from age 2 go to `DiskIndex` directly,
//!   bounding RSS to ≈ (8 + 64 + 512) × 50k ≈ 1.7 GB max (GC reduces this further).
//! - **Compacter**: 7 shared worker threads, one `crossbeam::channel<usize>` (age index).
//!   Any thread handles any age; the thread re-enqueues cascaded merges as needed.
//!
//! `contiguous_length` is updated per-append so the Table flusher can use it as a stable
//! watermark for `commit_before(h)` without an extra barrier.

use super::disk_index::DiskIndex;
use super::memory_age::{MemoryAge, Pin};
use super::memory_run::MemoryRun;
use super::types::{OutputId, OutputKV};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

#[cfg(test)]
extern crate tempfile;

/// Number of age tiers.
const K_AGES: usize = 7;
/// Oldest mutable age (ages 0..K_MUTABLE are mutable).
const K_MUTABLE_AGES: usize = 3;
/// Fan-in: trigger merge after this many runs in one age.
const K_FAN_IN: usize = 8;
/// Number of compacter worker threads.
const K_COMPACTER_THREADS: usize = 7;

/// Choose the disk eviction age.
///
/// `avail_mb`: available RAM in MiB at the time the index is opened (from the
/// caller's [`MemoryGuard`] or `ram_tier::probe_avail_ram_mib()`).
///
/// The UTXO index entry count in memory is bounded by:
///   Σ_{i=1}^{age} K_FAN_IN^i × MUTABLE_RUN_MAX_ENTRIES entries
///
/// `MUTABLE_RUN_MAX_ENTRIES = 10_000` and `K_FAN_IN = 8`.
///
/// During a fan-in merge at the deepest age, K_FAN_IN source runs stay resident
/// (queryable) while the merged output is built — a transient **2× spike**.
///
/// | Eviction age | Max index entries | Index RSS  | Merge-spike peak |
/// |---|---|---|---|
/// | 3 (min)  |   5.84M entries |  ~303 MB  |  ~532 MB  |
/// | 4        |  46.8M entries  |   ~2.4 GB |   ~4.3 GB |
/// | 5        |   374M entries  |  ~19.4 GB |   ~34 GB  |
/// | 6        |    ~3B entries  | ~156 GB   |  ~272 GB  |
///
/// Threshold: age N is safe when `avail_mb > merge_peak_for_N + ~10 GB` (non-index blvm).
/// We use `avail_mb` (MemAvailable), not total RAM — this already accounts for OS overhead
/// and co-resident services. Using MemTotal over-estimates headroom on shared machines.
///
/// `BLVM_IBD_ENGINE_EVICTION_AGE` overrides auto-detection.
fn choose_eviction_age(avail_mb: u64) -> usize {
    if let Ok(s) = std::env::var("BLVM_IBD_ENGINE_EVICTION_AGE") {
        if let Ok(n) = s.trim().parse::<usize>() {
            let clamped = n.clamp(K_MUTABLE_AGES, K_AGES);
            tracing::info!(
                "UTXO engine: eviction age = {} (from BLVM_IBD_ENGINE_EVICTION_AGE)",
                clamped
            );
            return clamped;
        }
    }

    // Use available RAM (MemAvailable from /proc/meminfo) as the headroom budget.
    // This already accounts for OS overhead, page cache, and co-resident processes
    // (vLLM, IDE, etc.) that have claimed physical pages.  Using MemTotal here
    // over-estimates the headroom on shared machines, causing OOM at age=4 or age=5.
    //
    // Headroom required per age (steady-state + merge-spike + non-index blvm ~10 GB):
    //   age=5: ~19.4 GB steady + ~34 GB spike + 10 GB other ≈ 63 GB available
    //   age=4:  ~2.4 GB steady +  ~4.3 GB spike + 10 GB other ≈ 17 GB available
    //   age=3:  ~0.3 GB steady +  ~0.5 GB spike + 10 GB other ≈ 11 GB (always safe)
    //
    // `avail_mb` is the MemAvailable at engine open, which is the right signal.
    // Fall back to MemAvailable from /proc/meminfo directly if not provided.
    // Read total physical RAM for the log line only — decisions use avail_mb.
    let total_mb = proc_mem_total_mb().unwrap_or(avail_mb);

    let age = if avail_mb >= 65 * 1024 {
        // ≥65 GiB available: age-5 merge peak (~34 GB) fits with room to spare.
        5
    } else if avail_mb >= 20 * 1024 {
        // ≥20 GiB available: age-4 merge peak (~4.3 GB) fits comfortably.
        4
    } else {
        // <20 GiB available: evict at age 3 (spike ~530 MB, OOM-safe).
        // Common on machines where GPU LLMs or other services consume most RAM.
        3
    };
    tracing::info!(
        "UTXO engine: eviction age = {} (auto-detected: {:.1} GiB physical RAM, {:.1} GiB available)",
        age,
        total_mb as f64 / 1024.0,
        avail_mb as f64 / 1024.0,
    );
    age
}

/// Read `MemTotal` from `/proc/meminfo` and return it in MiB. Returns `None` on any I/O error.
fn proc_mem_total_mb() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in content.lines() {
        if line.starts_with("MemTotal:") {
            let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

/// Manages background merges across all age tiers.
///
/// One `crossbeam::channel` shared by all `K_COMPACTER_THREADS` worker threads.
/// Any thread picks up any age index posted to the channel and runs its merge.
struct Compacter {
    tx: crossbeam_channel::Sender<usize>,
    /// Live eviction tier (ages at or above this spill to disk). Updated under memory pressure.
    eviction_age_live: Arc<AtomicUsize>,
    _threads: Vec<std::thread::JoinHandle<()>>,
}

impl Compacter {
    fn start(
        ages: Arc<[MemoryAge; K_AGES]>,
        disk_index: Arc<DiskIndex>,
        boot_eviction_age: usize,
    ) -> Self {
        let eviction_age_live = Arc::new(AtomicUsize::new(boot_eviction_age));
        let (tx, rx) = crossbeam_channel::unbounded::<usize>();
        let mut threads = Vec::with_capacity(K_COMPACTER_THREADS);
        for _ in 0..K_COMPACTER_THREADS {
            let rx = rx.clone();
            let tx = tx.clone();
            let ages = Arc::clone(&ages);
            let disk_index = Arc::clone(&disk_index);
            let eviction_age_live = Arc::clone(&eviction_age_live);
            let handle = std::thread::Builder::new()
                .name("utxo-compacter".to_string())
                .spawn(move || {
                    while let Ok(age_idx) = rx.recv() {
                        if age_idx == usize::MAX {
                            break; // shutdown sentinel
                        }
                        let eviction_age = eviction_age_live.load(Ordering::Acquire);
                        run_merge_for_age(&ages, age_idx, &disk_index, &tx, eviction_age);
                    }
                })
                .expect("spawn compacter thread");
            threads.push(handle);
        }
        Self {
            tx,
            eviction_age_live,
            _threads: threads,
        }
    }

    fn enqueue(&self, age_idx: usize) {
        let _ = self.tx.try_send(age_idx);
    }

    fn shutdown(&self) {
        for _ in 0..K_COMPACTER_THREADS {
            let _ = self.tx.send(usize::MAX);
        }
    }
}

fn run_merge_for_age(
    ages: &[MemoryAge; K_AGES],
    age_idx: usize,
    disk_index: &DiskIndex,
    tx: &crossbeam_channel::Sender<usize>,
    eviction_age: usize,
) {
    let age = &ages[age_idx];
    // Snapshot the oldest runs for merge. They remain in the age and are still queryable
    // throughout this merge pass — preventing the "UTXO invisible during merge" race.
    let Some(runs) = age.take_for_merge() else {
        return;
    };

    // K-way merge — all expensive work (sort, bloom, directory build) happens here,
    // outside any lock. `MemoryRun::merge` calls `build_presorted` (no redundant sort).
    let merged = MemoryRun::merge(&runs);
    let max_h = runs
        .iter()
        .map(|r| r.height_range().1)
        .max()
        .unwrap_or(i32::MIN);

    if !merged.is_empty() {
        let next_idx = age_idx + 1;
        if next_idx < eviction_age {
            // Push the already-built merged run to the next memory age BEFORE removing
            // source runs. `push_frozen_run` holds the write lock only for an Arc push.
            ages[next_idx].push_frozen_run(Arc::new(merged));

            // *** CRITICAL: cascade the merge. ***
            // After pushing to age N+1, age N+1 may now be merge_ready. Without re-enqueueing
            // it here, frozen ages accumulate runs indefinitely — Add+Delete pairs never cancel,
            // memory grows without bound, and disk eviction never fires.
            if ages[next_idx].merge_ready() {
                let _ = tx.send(next_idx);
            }
        } else {
            // Age K_DISK_EVICTION_AGE or beyond: evict to disk.
            //
            // By the time data reaches this tier, all heights in the merged run have been
            // fully validated and their pins released (merge_ready is pin-aware). The GC pass
            // inside MemoryRun::merge has already cancelled committed Add+Delete pairs.
            // RSS is now bounded to ≈ K_FAN_IN^K_DISK_EVICTION_AGE × MUTABLE_RUN_MAX_ENTRIES.
            tracing::debug!(
                "UTXO engine: evicting {} entries to disk (age {} overflow)",
                merged.len(),
                age_idx,
            );
            if let Err(e) = disk_index.push_run(&merged) {
                tracing::error!("UTXO engine: disk eviction failed — data may be lost: {e}");
            }
        }
    }

    // Atomically remove the source runs. Merged data is now in the next age or on disk.
    age.complete_merge(max_h, &runs);
}

/// 7-age UTXO index with disk overflow.
///
/// The primary lookup structure for the IBD engine. Memory ages hold recent data;
/// when the deepest age overflows it evicts to `DiskIndex` (cold storage on disk).
/// This bounds memory to approximately `K_FAN_IN^K_AGES` mutable-run-cap worth of entries
/// rather than growing linearly with all blocks processed.
pub struct UtxoIndex {
    ages: Arc<[MemoryAge; K_AGES]>,
    compacter: Compacter,
    /// Cold storage for entries evicted from the deepest memory age.
    disk_index: Arc<DiskIndex>,
    /// Highest height for which all blocks up to and including it have been appended.
    contiguous_length: AtomicI32,
    /// Eviction age chosen at open (from `choose_eviction_age`).
    boot_eviction_age: usize,
}

impl UtxoIndex {
    /// Open (or create) the index with a `seg_dir` for disk-evicted segment files.
    ///
    /// `avail_mb`: available system RAM in MiB (from `MemoryGuard` or
    /// `ram_tier::probe_avail_ram_mib()`).  Used by `choose_eviction_age` to decide
    /// how many index tiers remain in memory vs. are spilled to disk.
    pub fn open(seg_dir: &Path, avail_mb: u64) -> anyhow::Result<Self> {
        let eviction_age = choose_eviction_age(avail_mb);
        let ages_raw: [MemoryAge; K_AGES] = std::array::from_fn(|i| {
            let is_mutable = i < K_MUTABLE_AGES;
            MemoryAge::new(is_mutable, K_FAN_IN)
        });

        let ages = Arc::new(ages_raw);
        let disk_index = Arc::new(DiskIndex::new(seg_dir)?);
        let compacter = Compacter::start(Arc::clone(&ages), Arc::clone(&disk_index), eviction_age);

        Ok(Self {
            ages,
            compacter,
            disk_index,
            contiguous_length: AtomicI32::new(-1),
            boot_eviction_age: eviction_age,
        })
    }

    /// React to IBD memory-guard pressure (see `parallel_ibd::memory::MemoryGuard`).
    ///
    /// `level_u8`: `0`=None, `1`=Elevated, `2`=Critical, `3`=Emergency (matches `PressureLevel`).
    ///
    /// Lowers the in-memory eviction tier and floods the compacter so index tiers spill to
    /// disk before process RSS exceeds the guard's `rss_budget_mb`. Restores the boot tier
    /// when pressure returns to None.
    pub fn memory_pressure_tick(&self, level_u8: u8) {
        let boot = self.boot_eviction_age;
        // Higher pressure → lower eviction age (more aggressive disk spill).
        // K_MUTABLE_AGES (3) is the floor — below it there are no frozen ages to evict.
        let target = match level_u8 {
            3 => K_MUTABLE_AGES,               // Emergency: force minimum, spill everything
            2 => boot.min(K_MUTABLE_AGES + 1), // Critical: cap at age 4
            1 => boot.min(K_MUTABLE_AGES + 2), // Elevated: cap at age 5
            _ => boot,                         // None: restore boot age
        };
        let live = &self.compacter.eviction_age_live;
        let prev = live.load(Ordering::Relaxed);
        if target != prev {
            live.store(target, Ordering::Release);
            if target < prev {
                tracing::warn!(
                    "UTXO engine: memory pressure level {} — eviction age {} → {} (spilling index to disk)",
                    level_u8,
                    prev,
                    target
                );
            } else if level_u8 == 0 {
                tracing::info!(
                    "UTXO engine: memory pressure cleared — eviction age restored to {}",
                    target
                );
            }
        }
        if level_u8 >= 2 {
            for i in 0..K_AGES {
                if self.ages[i].merge_ready() {
                    self.compacter.enqueue(i);
                }
            }
        }
        if level_u8 >= 3 {
            #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
            unsafe {
                libmimalloc_sys::mi_collect(true);
            }
            #[cfg(target_os = "linux")]
            unsafe {
                libc::malloc_trim(0);
            }
        }
    }

    /// In-memory-only index for tests (uses a temp directory, cleaned up on drop).
    #[cfg(test)]
    pub fn new_for_test() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let idx = Self::open(tmp.path(), 8 * 1024).expect("UtxoIndex::open"); // avail_mb hint (age chosen by proc_mem_total_mb)
        // The TempDir would be dropped here, deleting the directory. To prevent that while
        // keeping the test simple, we deliberately leak it — it's a test-only temp dir.
        std::mem::forget(tmp);
        idx
    }

    /// Append a block's UTXO ops (Add + Delete entries) into the mutable tip (age 0).
    ///
    /// Returns a `Pin` keeping `height` resident in the mutable window until dropped.
    pub fn append(&self, entries: Vec<OutputKV>, height: i32) -> Pin {
        let pin = self.ages[0].pin_height(height);
        self.ages[0].append(entries, height);
        self.contiguous_length.fetch_max(height, Ordering::Relaxed);

        // Check if any of the mutable ages is ready for merge; post to compacter.
        for i in 0..K_MUTABLE_AGES {
            if self.ages[i].merge_ready() {
                self.compacter.enqueue(i);
            }
        }
        pin
    }

    /// Query all ages for `key`. Returns `Some(id)` from the newest age that has it.
    ///
    /// Used by `UtxoDatabase::query` (sorted batch path). For single-key lookup during
    /// intra-block resolution.
    pub fn lookup_key(&self, key: &[u8; 36]) -> Option<OutputId> {
        for age in self.ages.iter() {
            if let Some(id) = age.lookup_key(key, 0, i32::MAX) {
                // Filter internal sentinel — callers expect Some(real_id) or None.
                if id == super::types::OUTPUT_ID_DELETED {
                    return None;
                }
                return Some(id);
            }
        }
        None
    }

    /// Batch query: fills `ids[i]` for each `keys[i]` across all ages and disk overflow.
    ///
    /// `ids` must be pre-filled with `OutputId::MAX` (sentinel for "not yet resolved").
    /// Ages are queried newest-to-oldest; the disk index is the cold fallback for
    /// any keys still unresolved (MAX) after all memory ages.
    ///
    /// `before` is an exclusive upper bound on `entry.height`. `SpendSession` passes
    /// `before = height` so that Deletes recorded for the current block are invisible.
    ///
    /// On return, `ids[i]` is either a real `OutputId` (found) or `OutputId::MAX` (not found).
    pub fn batch_query(&self, keys: &[[u8; 36]], ids: &mut [OutputId], before: i32) {
        debug_assert_eq!(keys.len(), ids.len());
        for age in self.ages.iter() {
            if ids.iter().all(|id| *id != OutputId::MAX) {
                break; // all resolved (real id or confirmed-deleted sentinel)
            }
            age.batch_query(keys, ids, 0, before);
        }
        // Disk fallback — keys with OUTPUT_ID_DELETED are already resolved (spent in
        // memory) so disk_index skips them. disk_index.batch_query also normalizes
        // any remaining OUTPUT_ID_DELETED → OutputId::MAX before returning.
        self.disk_index.batch_query(keys, ids, before);
    }

    /// Block the calling thread until `contiguous_length >= height`.
    ///
    /// Used only by the watermark export path — NOT on the validation hot path.
    pub fn wait_for_height(&self, height: i32) {
        while self.contiguous_length.load(Ordering::Relaxed) < height {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    pub fn contiguous_length(&self) -> i32 {
        self.contiguous_length.load(Ordering::Relaxed)
    }

    /// Seed the index from a checkpoint import (resume after SIGKILL). Sets contiguous_length
    /// exactly to `checkpoint_height` without posting compacter work.
    ///
    /// Large seed batches (e.g. 250M entries = 14 GB at h=640k) are written **directly to a
    /// disk segment** rather than through age-0 → age-1 → age-2 → disk.  The cascade creates
    /// multiple full copies of the UTXO set in RAM (each tier merge materialises a new Vec),
    /// causing 70+ GB RSS and an OOM kill before the next checkpoint export can run.
    ///
    /// The direct-to-disk path keeps peak extra RSS at O(entries.len() × 56 B) — exactly one
    /// Vec, sorted in-place and streamed to disk, then freed.  Memory ages stay empty, so the
    /// next `iter_live_at_height` call's `mem_entries` collection stays tiny.
    pub fn seed_checkpoint(&self, mut entries: Vec<OutputKV>, checkpoint_height: i32) {
        if !entries.is_empty() {
            // Sort in-place (no extra allocation beyond the existing Vec).
            entries.sort_unstable();
            match self.disk_index.push_sorted_segment(&entries) {
                Ok(()) => {
                    // Drop the Vec immediately to free the 14 GB before validation starts.
                    drop(entries);
                }
                Err(e) => {
                    // Disk write failed — fall back to the age-0 path so we don't lose data.
                    tracing::error!(
                        "seed_checkpoint: disk write failed ({e:#}), falling back to age-0 (may cause memory pressure)"
                    );
                    self.ages[0].append(entries, checkpoint_height);
                }
            }
        }
        self.contiguous_length
            .store(checkpoint_height, Ordering::Release);
        // Initialise the GC fence to the checkpoint height. This prevents compaction from
        // cancelling Add+Delete pairs for UTXOs that were live at the checkpoint but spent
        // after it — those Add entries must survive until the next checkpoint's scan.
        // The fence is advanced when the next checkpoint export starts.
        super::set_gc_fence(checkpoint_height);
    }

    /// Allocate a disk-segment slot for the streaming seed writer thread.
    ///
    /// Returns `(seg_idx, seg_dir)` — the caller writes the segment file and then calls
    /// `finalize_seed` to register it and commit the checkpoint height.
    pub fn alloc_seed_seg(&self) -> (usize, std::path::PathBuf) {
        self.disk_index.alloc_seg()
    }

    /// Register the streaming-seed segment and commit `contiguous_length` / GC fence.
    pub fn finalize_seed(&self, seg: super::disk_segment::DiskSegment, checkpoint_height: i32) {
        self.disk_index.register_seg(seg);
        self.contiguous_length
            .store(checkpoint_height, Ordering::Release);
        super::set_gc_fence(checkpoint_height);
    }

    /// Remove all UTXO ops at `height >= since` from mutable ages. Reorg recovery.
    pub fn erase_since(&self, since: i32) {
        for i in 0..K_MUTABLE_AGES {
            self.ages[i].erase_since(since);
        }
        // Roll back contiguous_length.
        self.contiguous_length
            .fetch_min(since - 1, Ordering::Relaxed);
    }

    /// Iterate all non-cancelled Add entries across all ages and disk segments.
    ///
    /// Used by the watermark export at IBD completion. Scans disk segments (oldest) then
    /// memory ages (newest), resolving Deletes against Adds to yield only live UTXOs.
    pub fn scan_all_live(&self) -> Vec<OutputKV> {
        // Atomically snapshot disk segments AND all memory-age runs while holding the
        // DiskIndex read lock. This closes the race where a compacter thread evicts a
        // MemoryAge run to DiskIndex between the disk scan and the memory-age scan.
        let (disk_segs, mem_snapshots) = {
            let guard = self.disk_index.segments.read();
            let disk = guard.clone();
            let mem: Vec<_> = self.ages.iter().rev().map(|a| a.snapshot_runs()).collect();
            (disk, mem)
            // read lock drops here — compaction is unblocked for the long scan below
        };

        // Collect ALL entries (adds AND deletes) into a flat Vec.
        // This avoids a HashMap whose per-entry overhead exceeds the entry itself
        // (~120 bytes/entry vs 56 bytes for OutputKV), saving 10–15 GB at 170M UTXOs.
        let mut all_entries: Vec<OutputKV> = Vec::new();

        for seg in &disk_segs {
            let entries = match seg.read_all_entries() {
                Ok(e) => e,
                Err(err) => {
                    tracing::warn!("scan_all_live: skipping segment {:?}: {err}", seg.path);
                    continue;
                }
            };
            all_entries.extend_from_slice(&entries);
        }

        for snapshot in &mem_snapshots {
            for run in snapshot.iter() {
                all_entries.extend_from_slice(&run.entries);
            }
        }

        // Sort by OutputKV natural order: key ASC, height DESC, Add before Delete.
        // After sorting, the FIRST entry for each key is the most recent operation.
        // A single linear pass then determines whether each key is live (Add) or spent (Delete).
        all_entries.sort_unstable();

        let mut result: Vec<OutputKV> = Vec::new();
        let mut i = 0;
        while i < all_entries.len() {
            // First entry for this key = most recent (highest height, or Add before Delete).
            let first = all_entries[i];
            if first.is_add() {
                result.push(first);
            }
            // Skip all remaining entries for this key.
            let key = first.key;
            i += 1;
            while i < all_entries.len() && all_entries[i].key == key {
                i += 1;
            }
        }

        result
    }

    /// Like `scan_all_live` but only considers entries with `height <= max_height`.
    ///
    /// This returns the UTXO set as it was at `max_height` even if the engine has already
    /// advanced beyond that height. Used by periodic mid-IBD watermark exports so we can
    /// snapshot an earlier height while validation continues running concurrently.
    ///
    /// Correctness: entries created after `max_height` are excluded; deletions after
    /// `max_height` are also excluded, so UTXOs spent after `max_height` still appear live —
    /// exactly the UTXO set at `max_height`.
    pub fn scan_live_at_height(&self, max_height: i32) -> Vec<OutputKV> {
        // Pre-scan compaction: merge all disk segments into one with GC applied.
        // The caller has already set CHECKPOINT_GC_FENCE = max_height, so all
        // Add+Delete pairs where Delete.height <= max_height are cancelled during
        // compaction.  After this call, disk segments contain only live UTXOs
        // (Add entries without a Delete at or below max_height), shrinking the
        // scan's working set from O(total_spends) to O(live_UTXOs) and preventing
        // the multi-GB allocation spike that caused OOMs at h > 200k.
        self.disk_index.compact_for_checkpoint_sync();

        // Atomically snapshot disk segments AND all memory-age runs while holding the
        // DiskIndex read lock. Prevents the race where a compacter evicts a MemoryAge
        // run to DiskIndex between the disk scan and the memory-age scan, dropping
        // UTXOs from the checkpoint snapshot entirely.
        let (disk_segs, mem_snapshots) = {
            let guard = self.disk_index.segments.read();
            let disk = guard.clone();
            let mem: Vec<_> = self.ages.iter().rev().map(|a| a.snapshot_runs()).collect();
            (disk, mem)
            // read lock drops here — compaction is unblocked for the long scan below
        };

        // Collect all entries with height <= max_height into a flat Vec.
        // Use streaming reads (SegmentReader) instead of read_all_entries() to avoid
        // loading the full segment (potentially 11 GB for 200 M entries) into RAM.
        // After compact_for_checkpoint_sync, the segment contains only live UTXOs at
        // max_height (~20 M entries = ~1.1 GB) — well within budget.
        let mut all_entries: Vec<OutputKV> = Vec::new();

        for seg in &disk_segs {
            let mut reader = seg.stream();
            loop {
                match reader.advance() {
                    Ok(Some(entry)) => {
                        if entry.height <= max_height {
                            all_entries.push(entry);
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        tracing::warn!(
                            "scan_live_at_height: read error on segment {:?}: {err}",
                            seg.path
                        );
                        break;
                    }
                }
            }
        }

        for snapshot in &mem_snapshots {
            for run in snapshot.iter() {
                for entry in &run.entries {
                    if entry.height <= max_height {
                        all_entries.push(*entry);
                    }
                }
            }
        }

        // Sort by OutputKV natural order: key ASC, height DESC, Add before Delete.
        // After sorting, the FIRST entry for each key is the most recent operation
        // at or below max_height. If it's an Add the UTXO is live; Delete means spent.
        all_entries.sort_unstable();

        let mut result: Vec<OutputKV> = Vec::new();
        let mut i = 0;
        while i < all_entries.len() {
            let first = all_entries[i];
            if first.is_add() {
                result.push(first);
            }
            let key = first.key;
            i += 1;
            while i < all_entries.len() && all_entries[i].key == key {
                i += 1;
            }
        }

        result
    }

    /// Return a streaming iterator over live UTXOs as of `max_height`.
    ///
    /// Unlike `scan_live_at_height` (which builds a full `Vec<OutputKV>` — ~14 GB at 250M
    /// UTXOs), this streams disk entries one chunk at a time via `SegmentReader` and collects
    /// memory entries into a small sorted Vec (≤5.84M entries ≈ 327 MB at `eviction_age=3`).
    ///
    /// Peak memory beyond baseline RSS: ~330 MB (stream state) + ~150 MB (export chunk buffers).
    pub fn iter_live_at_height(&self, max_height: i32) -> anyhow::Result<CheckpointStream> {
        // Compact disk segments with GC applied at max_height.
        self.disk_index.compact_for_checkpoint_sync();

        // Atomic snapshot under read lock — prevents the race where a compacter evicts a
        // MemoryAge run between the disk scan and the memory-age scan.
        let (disk_segs, mem_snapshots) = {
            let guard = self.disk_index.segments.read();
            let disk = guard.clone();
            let mem: Vec<_> = self.ages.iter().rev().map(|a| a.snapshot_runs()).collect();
            (disk, mem)
        };

        // Collect memory entries at or below max_height into a small sorted Vec.
        // At eviction_age=3 this is ≤5.84M entries = ~327 MB.
        let mut mem_entries: Vec<OutputKV> = Vec::new();
        for snapshot in &mem_snapshots {
            for run in snapshot.iter() {
                for &entry in &run.entries {
                    if entry.height <= max_height {
                        mem_entries.push(entry);
                    }
                }
            }
        }
        mem_entries.sort_unstable();

        // Open one streaming reader per disk segment. After compact_for_checkpoint_sync,
        // typically just one segment. Each reader buffers READER_CHUNK entries (~1 MB).
        let mut readers: Vec<super::disk_segment::SegmentReader> =
            disk_segs.iter().map(|seg| seg.stream()).collect();

        // Pre-fetch first entry from each reader.
        let mut heads: Vec<Option<OutputKV>> = Vec::with_capacity(readers.len());
        for reader in &mut readers {
            heads.push(reader.advance()?);
        }

        Ok(CheckpointStream {
            mem: mem_entries,
            mem_pos: 0,
            readers,
            heads,
            max_height,
            last_key: None,
        })
    }
}

/// Streaming live-UTXO iterator for checkpoint exports.
///
/// Performs a k-way merge of (small) in-memory age entries with (large) on-disk
/// `DiskSegment` entries. Both sources are pre-sorted by `OutputKV::Ord` (key ASC,
/// height DESC, Add before Delete). Entries beyond `max_height` are skipped without
/// consuming the key slot. Each key is deduplicated: the first valid entry determines
/// whether the UTXO is live (Add → yield) or spent (Delete → skip).
///
/// Peak allocations:
///   - `mem`: ≤5.84M × 56 B ≈ 327 MB  (eviction_age=3)
///   - per disk reader: `READER_CHUNK` entries ≈ 1 MB
pub struct CheckpointStream {
    mem: Vec<OutputKV>,
    mem_pos: usize,
    readers: Vec<super::disk_segment::SegmentReader>,
    heads: Vec<Option<OutputKV>>,
    max_height: i32,
    last_key: Option<[u8; 36]>,
}

impl CheckpointStream {
    /// Advance to the next live UTXO (Add, height ≤ max_height, per-key deduplicated).
    /// Returns `Ok(None)` when all sources are exhausted.
    pub fn next_live(&mut self) -> anyhow::Result<Option<OutputKV>> {
        loop {
            let entry = match self.pick_min()? {
                Some(e) => e,
                None => return Ok(None),
            };

            // Skip entries beyond the checkpoint — do NOT mark key as seen.
            // A later entry for the same key at valid height is still the deciding one.
            // Example: Delete(h=700k) then Add(h=100) → UTXO is live at fence=640k.
            if entry.height > self.max_height {
                continue;
            }

            // Dedup: first entry per key (highest valid height) determines live/spent.
            if Some(entry.key) == self.last_key {
                continue;
            }
            self.last_key = Some(entry.key);

            if entry.is_add() {
                return Ok(Some(entry));
            }
            // Delete at or before max_height → UTXO is spent, do not yield.
        }
    }

    fn pick_min(&mut self) -> anyhow::Result<Option<OutputKV>> {
        let mut best: Option<OutputKV> = None;
        let mut best_is_disk = false;
        let mut best_disk_idx: usize = 0;

        // Check memory cursor.
        if let Some(&me) = self.mem.get(self.mem_pos) {
            best = Some(me);
        }

        // Check each disk head; pick the global minimum under OutputKV::Ord.
        for i in 0..self.heads.len() {
            if let Some(de) = self.heads[i] {
                let take = match best {
                    None => true,
                    Some(cur) => de < cur,
                };
                if take {
                    best = Some(de);
                    best_is_disk = true;
                    best_disk_idx = i;
                }
            }
        }

        match best {
            None => Ok(None),
            Some(e) => {
                if best_is_disk {
                    self.heads[best_disk_idx] = self.readers[best_disk_idx].advance()?;
                } else {
                    self.mem_pos += 1;
                }
                Ok(Some(e))
            }
        }
    }
}

impl Drop for UtxoIndex {
    fn drop(&mut self) {
        self.compacter.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::OutputKV;
    use super::*;

    fn make_key(n: u8) -> [u8; 36] {
        let mut k = [0u8; 36];
        k[0] = n;
        k
    }

    #[test]
    fn test_append_and_query() {
        let idx = UtxoIndex::new_for_test();
        let k = make_key(1);
        let _pin = idx.append(vec![OutputKV::new_add(k, 100, 42)], 100);
        assert_eq!(idx.lookup_key(&k), Some(42));
        assert_eq!(idx.lookup_key(&make_key(2)), None);
    }

    #[test]
    fn test_batch_query() {
        let idx = UtxoIndex::new_for_test();
        let k1 = make_key(1);
        let k2 = make_key(2);
        let _p1 = idx.append(vec![OutputKV::new_add(k1, 100, 10)], 100);
        let _p2 = idx.append(vec![OutputKV::new_add(k2, 101, 20)], 101);
        let mut ids = [OutputId::MAX; 2];
        idx.batch_query(&[k1, k2], &mut ids, i32::MAX);
        assert_eq!(ids[0], 10);
        assert_eq!(ids[1], 20);
    }

    #[test]
    fn test_contiguous_length() {
        let idx = UtxoIndex::new_for_test();
        assert_eq!(idx.contiguous_length(), -1);
        let k = make_key(1);
        let _pin = idx.append(vec![OutputKV::new_add(k, 50, 1)], 50);
        assert_eq!(idx.contiguous_length(), 50);
    }

    #[test]
    fn test_erase_since() {
        let idx = UtxoIndex::new_for_test();
        let k1 = make_key(1);
        let k2 = make_key(2);
        let _p1 = idx.append(vec![OutputKV::new_add(k1, 50, 1)], 50);
        let _p2 = idx.append(vec![OutputKV::new_add(k2, 100, 2)], 100);
        idx.erase_since(75);
        assert_eq!(idx.lookup_key(&k1), Some(1));
        assert_eq!(idx.lookup_key(&k2), None);
    }
}
