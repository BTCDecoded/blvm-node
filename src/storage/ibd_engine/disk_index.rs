//! `DiskIndex`: ordered collection of `DiskSegment`s for the age-overflow layer.
//!
//! Segments are accumulated as the deepest memory age overflows during IBD.
//! They are queried newest-to-oldest after all memory ages come up empty.
//!
//! ## Memory usage
//! Each segment stores only a bloom filter (~12 bits/entry) and directory in RAM;
//! the sorted entries live on disk. For a segment of 10M entries:
//!   - bloom: 10M × 1.5 bytes ≈ 15 MB
//!   - directory: negligible
//!
//! With ~10 segments over a full IBD: ~150 MB overhead — bounded and predictable.
//!
//! ## Correctness
//! `OUTPUT_ID_DELETED` (set by memory-age `lookup_key` when a Delete is found) prevents
//! disk lookup from returning a stale Add for a UTXO that was spent in memory.

use super::disk_segment::DiskSegment;
use super::memory_run::MemoryRun;
use super::types::{OutputId, OutputKV};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default merge fan-in: compact when this many segments have accumulated.
///
/// With fan-in = 8, the index holds at most ~8 segments in steady state:
/// after the 8th push, the 8 oldest are merged into one (with GC), reducing count to 1.
/// Each subsequent push brings it to 2, 3, … up to 8, then compacts again.
///
/// Memory bound: 8 bloom filters in RAM at ~8 MB each ≈ 64 MB max disk-tier overhead.
/// Lookup bound: O(8) pread64 calls per block instead of O(all-time evictions).
///
/// Override with `BLVM_IBD_DISK_FAN_IN` (clamped 2..=32) for tip A/B — mega ~100M
/// segments often stall at 5 segs and never hit the default threshold.
const K_DISK_FAN_IN: usize = 8;

/// Minimum seconds between background compactions when segment count is below `2 × fan_in`.
/// Reduces validation BPS dips from CPU-heavy 500M+ entry merges during gap replay.
const COMPACT_MIN_INTERVAL_SECS: u64 = 45;

fn disk_fan_in_from_env() -> usize {
    std::env::var("BLVM_IBD_DISK_FAN_IN")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|n| n.clamp(2, 32))
        .unwrap_or(K_DISK_FAN_IN)
}

/// HP-M3: write mega DiskIndex spills on a background thread so age-tier `is_merging`
/// can clear after ~merge_ms instead of merge+write (~14s+12s). Pending `MemoryRun`s
/// stay queryable until the segment is registered. Default off (sync write).
fn async_disk_spill_from_env() -> bool {
    matches!(
        std::env::var("BLVM_IBD_ASYNC_DISK_SPILL")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// HP-M4: cap each DiskIndex spill segment by entry count. `0` = disabled (one segment
/// per merged run). When set, mega runs are written as consecutive segments so tip can
/// query early parts while later chunks still write, and compact can fire sooner.
fn spill_max_entries_from_env() -> usize {
    std::env::var("BLVM_IBD_SPILL_MAX_ENTRIES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
}

fn height_range_of_entries(entries: &[OutputKV]) -> Option<(i32, i32)> {
    let mut min_h = i32::MAX;
    let mut max_h = i32::MIN;
    for e in entries {
        min_h = min_h.min(e.height);
        max_h = max_h.max(e.height);
    }
    if min_h == i32::MAX {
        None
    } else {
        Some((min_h, max_h))
    }
}

static LAST_COMPACT_FINISH: AtomicU64 = AtomicU64::new(0);

pub struct DiskIndex {
    /// Segments oldest-to-newest. New segments are pushed to the back.
    /// Outer `Arc` lets readers snapshot with one refcount bump (batch_query / export scan).
    pub(super) segments: parking_lot::RwLock<Arc<Vec<Arc<DiskSegment>>>>,
    /// In-flight async spills: merged runs queryable until the segment file is registered.
    pub(super) pending_spills: parking_lot::RwLock<Vec<Arc<MemoryRun>>>,
    /// Directory for segment files.
    seg_dir: PathBuf,
    /// Monotonically increasing segment index (never reused, avoids filename collisions).
    next_idx: AtomicUsize,
    /// CAS guard: only one compaction runs at a time.
    is_compacting: AtomicBool,
    /// At most one background mega-spill write (avoid 2× ~100M-entry RAM).
    async_spill_busy: AtomicBool,
    /// True while any DiskIndex segment file write is in progress (sync, split, or async).
    /// Used by `BLVM_IBD_SPILL_IO_GATE` to park validation dispatch during mega writes.
    spill_write_busy: AtomicBool,
    /// GC fence applied by the last successful `compact_for_checkpoint_sync` pass.
    /// Used to skip redundant re-compaction when segment count is already 1.
    last_checkpoint_compact_fence: AtomicI32,
}

/// RAII: holds `spill_write_busy` for the duration of a segment file write.
struct SpillWriteGuard<'a>(&'a AtomicBool);

impl Drop for SpillWriteGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl DiskIndex {
    pub fn new(seg_dir: &Path) -> anyhow::Result<(Self, i32)> {
        Self::new_impl(seg_dir, true)
    }

    /// Create an empty index (no segment load). Used before checkpoint re-seed to avoid
    /// reading hundreds of millions of on-disk entries that will be wiped immediately.
    pub fn new_empty(seg_dir: &Path) -> anyhow::Result<(Self, i32)> {
        Self::new_impl(seg_dir, false)
    }

    /// Scan segment headers only — max block height durably on disk (no bloom load).
    pub fn peek_segment_dir_max_height(seg_dir: &Path) -> i32 {
        let mut max_height = -1i32;
        let Ok(entries) = std::fs::read_dir(seg_dir) else {
            return max_height;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let is_seg = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("seg_") && n.ends_with(".bin"));
            if !is_seg {
                continue;
            }
            if let Ok(h) = DiskSegment::peek_max_height(&path) {
                max_height = max_height.max(h);
            }
        }
        max_height
    }

    fn new_impl(seg_dir: &Path, load_segments: bool) -> anyhow::Result<(Self, i32)> {
        std::fs::create_dir_all(seg_dir)?;
        let mut loaded: Vec<Arc<DiskSegment>> = Vec::new();
        let mut max_height = -1i32;
        let mut next_idx = 0usize;
        if load_segments {
            if let Ok(entries) = std::fs::read_dir(seg_dir) {
                let mut paths: Vec<PathBuf> = entries
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("seg_") && n.ends_with(".bin"))
                    })
                    .collect();
                paths.sort();
                for path in paths {
                    let Some(idx) = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.strip_prefix("seg_"))
                        .and_then(|n| n.strip_suffix(".bin"))
                        .and_then(|n| n.parse::<usize>().ok())
                    else {
                        continue;
                    };
                    let seg = DiskSegment::open(&path)?;
                    max_height = max_height.max(seg.height_range().1);
                    next_idx = next_idx.max(idx + 1);
                    loaded.push(Arc::new(seg));
                }
            }
            if !loaded.is_empty() {
                tracing::info!(
                    "DiskIndex: loaded {} on-disk segment(s) from {} (max_height={})",
                    loaded.len(),
                    seg_dir.display(),
                    max_height,
                );
            }
        }
        Ok((
            Self {
                segments: parking_lot::RwLock::new(Arc::new(loaded)),
                pending_spills: parking_lot::RwLock::new(Vec::new()),
                seg_dir: seg_dir.to_owned(),
                next_idx: AtomicUsize::new(next_idx),
                is_compacting: AtomicBool::new(false),
                async_spill_busy: AtomicBool::new(false),
                spill_write_busy: AtomicBool::new(false),
                last_checkpoint_compact_fence: AtomicI32::new(-1),
            },
            max_height,
        ))
    }

    fn begin_spill_write(&self) -> SpillWriteGuard<'_> {
        self.spill_write_busy.store(true, Ordering::Release);
        SpillWriteGuard(&self.spill_write_busy)
    }

    /// True while a DiskIndex spill segment file is being written (HP-M5 gate).
    pub fn spill_io_busy(&self) -> bool {
        self.spill_write_busy.load(Ordering::Acquire)
            || self.async_spill_busy.load(Ordering::Acquire)
    }

    /// Write a pre-sorted entry Vec directly to a new disk segment.
    ///
    /// Does **not** trigger `compact_oldest_if_needed`. Used by `seed_checkpoint` so that
    /// a large initial UTXO set (e.g. 250M entries = 14 GB) bypasses the memory-age cascade
    /// entirely, keeping peak RSS at O(1) rather than O(cascade_copies × UTXO_count).
    ///
    /// When `BLVM_IBD_HOT_PIN=1` and the seed is mega-eligible, keeps the body in RAM
    /// (same residency as spill eviction — seed previously skipped HotPin and left ages
    /// empty → cold DiskIndex `pread` from the first post-resume block).
    pub fn push_sorted_segment_owned(&self, entries: Vec<OutputKV>) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let entry_count = entries.len();
        let mut min_h = i32::MAX;
        let mut max_h = i32::MIN;
        for e in &entries {
            min_h = min_h.min(e.height);
            max_h = max_h.max(e.height);
        }
        let height_range = (min_h, max_h);
        let idx = self.next_idx.fetch_add(1, Ordering::Relaxed);
        let pin = super::disk_segment::hot_pin_eligible(entry_count);
        let seg = DiskSegment::write_owned(&self.seg_dir, idx, height_range, entries, pin)?;
        tracing::info!(
            "DiskIndex: seed segment {} — {} entries written directly to disk (hot_pin={})",
            idx,
            entry_count,
            pin,
        );
        // Trim prior pins before publish (same order as spill eviction).
        if pin {
            self.trim_hot_pins_for_new(super::disk_segment::hot_pin_max_segs());
        }
        {
            let mut w = self.segments.write();
            Arc::make_mut(&mut *w).push(Arc::new(seg));
        }
        Ok(())
    }

    /// Slice wrapper — clones when HotPin is eligible (prefer [`Self::push_sorted_segment_owned`]).
    pub fn push_sorted_segment(&self, entries: &[OutputKV]) -> anyhow::Result<()> {
        self.push_sorted_segment_owned(entries.to_vec())
    }

    /// If HotPin is enabled and `seg` is mega-eligible, load entries into `hot_body`.
    /// Used after streaming seed (`register_seg`) where no in-RAM Vec survived the write.
    ///
    /// Call **before** publishing `seg` into `self.segments` (trim clears pins already
    /// in the list; the new pin must not be visible yet).
    fn maybe_hot_pin_segment_before_publish(&self, seg: &DiskSegment) {
        if seg.has_hot_body() || !super::disk_segment::hot_pin_eligible(seg.entry_count) {
            return;
        }
        let t0 = std::time::Instant::now();
        match seg.load_all_entries() {
            Ok(entries) => {
                self.trim_hot_pins_for_new(super::disk_segment::hot_pin_max_segs());
                seg.attach_hot_pin(entries);
                tracing::info!(
                    "DiskIndex: seed HotPin loaded entries={} in {:.1}s",
                    seg.entry_count,
                    t0.elapsed().as_secs_f64()
                );
            }
            Err(e) => {
                tracing::warn!(
                    "DiskIndex: seed HotPin load failed (entries={}): {e:#} — staying on pread",
                    seg.entry_count
                );
            }
        }
    }

    /// Allocate a segment slot (index + directory path) for use by a caller that will write
    /// the segment file itself (e.g. the streaming seed writer thread).
    ///
    /// The caller must eventually pass the finished `DiskSegment` to `register_seg`.
    pub fn alloc_seg(&self) -> (usize, PathBuf) {
        let idx = self.next_idx.fetch_add(1, Ordering::Relaxed);
        (idx, self.seg_dir.clone())
    }

    /// Register a pre-built `DiskSegment` that was written externally (e.g. by the streaming
    /// seed writer thread). Does **not** trigger compaction.
    ///
    /// When `BLVM_IBD_HOT_PIN=1` and the segment is mega-eligible, loads the body into RAM
    /// so post-reseed queries hit HotPin instead of cold DiskIndex `pread` (AV=0 @400k cliff).
    pub fn register_seg(&self, seg: DiskSegment) {
        self.maybe_hot_pin_segment_before_publish(&seg);
        {
            let mut w = self.segments.write();
            Arc::make_mut(&mut *w).push(Arc::new(seg));
        }
    }

    /// Evict `run` to a new disk segment **without** running segment compaction.
    ///
    /// Callers that hold a memory-age `is_merging` lock must use this, then
    /// release the age lock via `complete_merge`, then call
    /// [`Self::compact_oldest_async`]. Compacting 8×~30M-entry segments can take
    /// minutes; doing it while age-3 holds `is_merging` freezes spill drain
    /// (`COMPACTER_GATE` 79–180s pauses observed at h≈394k–490k).
    ///
    /// With `BLVM_IBD_ASYNC_DISK_SPILL=1`, the file write runs on a background thread
    /// while `run` stays in [`Self::pending_spills`] for queries (HP-M3). Falls back to
    /// sync if another async spill is already in flight.
    ///
    /// `BLVM_IBD_SPILL_MAX_ENTRIES` (HP-M4) takes precedence over async: oversized runs
    /// are size-split synchronously so each chunk is published before the next write.
    pub fn push_run_no_compact(self: &Arc<Self>, run: MemoryRun) -> anyhow::Result<()> {
        let max = spill_max_entries_from_env();
        if max > 0 && run.entries.len() > max {
            return self.push_run_no_compact_split(run, max);
        }
        if async_disk_spill_from_env()
            && self
                .async_spill_busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            return self.push_run_no_compact_async(run);
        }
        self.push_run_no_compact_sync(run)
    }

    fn push_run_no_compact_sync(&self, mut run: MemoryRun) -> anyhow::Result<()> {
        let _io = self.begin_spill_write();
        let t0 = std::time::Instant::now();
        let idx = self.next_idx.fetch_add(1, Ordering::Relaxed);
        let entry_count = run.entries.len();
        let height_range = run.height_range;
        let pin = super::disk_segment::hot_pin_eligible(entry_count);
        let entries = std::mem::take(&mut run.entries);
        let seg = DiskSegment::write_owned(&self.seg_dir, idx, height_range, entries, pin)?;
        let write_ms = t0.elapsed().as_millis() as u64;
        tracing::info!(
            "DiskIndex: evicted segment {} — {} entries, heights {}–{} (write_ms={} hot_pin={})",
            idx,
            entry_count,
            height_range.0,
            height_range.1,
            write_ms,
            pin,
        );
        if pin {
            self.trim_hot_pins_for_new(super::disk_segment::hot_pin_max_segs());
        }
        {
            let mut w = self.segments.write();
            Arc::make_mut(&mut *w).push(Arc::new(seg));
        }
        Ok(())
    }

    /// Write `run` as consecutive segments of at most `max` entries each (HP-M4).
    fn push_run_no_compact_split(&self, mut run: MemoryRun, max: usize) -> anyhow::Result<()> {
        debug_assert!(max > 0);
        let parent_hr = run.height_range;
        let mut entries = std::mem::take(&mut run.entries);
        let total = entries.len();
        let parts = total.div_ceil(max);
        tracing::info!(
            "DiskIndex: size-split spill — {} entries into {} part(s) (max={})",
            total,
            parts,
            max,
        );
        let _io = self.begin_spill_write();
        let t_all = std::time::Instant::now();
        let mut part = 0usize;
        while !entries.is_empty() {
            part += 1;
            let chunk = if entries.len() <= max {
                std::mem::take(&mut entries)
            } else {
                let rest = entries.split_off(max);
                std::mem::replace(&mut entries, rest)
            };
            let entry_count = chunk.len();
            let height_range = height_range_of_entries(&chunk).unwrap_or(parent_hr);
            let pin = super::disk_segment::hot_pin_eligible(entry_count);
            let t0 = std::time::Instant::now();
            let idx = self.next_idx.fetch_add(1, Ordering::Relaxed);
            let seg = DiskSegment::write_owned(&self.seg_dir, idx, height_range, chunk, pin)?;
            let write_ms = t0.elapsed().as_millis() as u64;
            tracing::info!(
                "DiskIndex: evicted segment {} — {} entries, heights {}–{} \
                 (write_ms={} hot_pin={} split={}/{})",
                idx,
                entry_count,
                height_range.0,
                height_range.1,
                write_ms,
                pin,
                part,
                parts,
            );
            if pin {
                self.trim_hot_pins_for_new(super::disk_segment::hot_pin_max_segs());
            }
            {
                let mut w = self.segments.write();
                Arc::make_mut(&mut *w).push(Arc::new(seg));
            }
        }
        tracing::info!(
            "DiskIndex: size-split spill done — {} part(s), total_write_ms={}",
            parts,
            t_all.elapsed().as_millis() as u64,
        );
        Ok(())
    }

    fn push_run_no_compact_async(self: &Arc<Self>, run: MemoryRun) -> anyhow::Result<()> {
        let run = Arc::new(run);
        let entry_count = run.len();
        let height_range = run.height_range();
        {
            self.pending_spills.write().push(Arc::clone(&run));
        }
        tracing::info!(
            "DiskIndex: async spill queued — {} entries, heights {}–{}",
            entry_count,
            height_range.0,
            height_range.1,
        );
        let disk = Arc::clone(self);
        let run_for_err = Arc::clone(&run);
        let spawn_res = std::thread::Builder::new()
            .name("ibd-async-spill".into())
            .spawn(move || {
                let t0 = std::time::Instant::now();
                let idx = disk.next_idx.fetch_add(1, Ordering::Relaxed);
                let pin = super::disk_segment::hot_pin_eligible(entry_count);
                let write_res = DiskSegment::write_from_slice(
                    &disk.seg_dir,
                    idx,
                    height_range,
                    &run.entries,
                );
                match write_res {
                    Ok(seg) => {
                        let seg = Arc::new(seg);
                        // Publish segment BEFORE dropping pending — otherwise queries see a
                        // gap (HP-M3 MISSING_UTXO @ first async spill, h=349546).
                        if pin {
                            disk.trim_hot_pins_for_new(super::disk_segment::hot_pin_max_segs());
                        }
                        {
                            let mut p = disk.pending_spills.write();
                            let mut w = disk.segments.write();
                            if let Some(i) = p.iter().position(|r| Arc::ptr_eq(r, &run)) {
                                p.remove(i);
                            }
                            Arc::make_mut(&mut *w).push(Arc::clone(&seg));
                        }
                        // HotPin after publish (pread path already serves the segment).
                        if pin {
                            match Arc::try_unwrap(run) {
                                Ok(mut owned) => {
                                    let entries = std::mem::take(&mut owned.entries);
                                    seg.attach_hot_pin(entries);
                                }
                                Err(shared) => {
                                    tracing::warn!(
                                        "DiskIndex: async spill hot-pin skipped (run still shared, strong={})",
                                        Arc::strong_count(&shared)
                                    );
                                }
                            }
                        } else {
                            drop(run);
                        }
                        let write_ms = t0.elapsed().as_millis() as u64;
                        tracing::info!(
                            "DiskIndex: evicted segment {} — {} entries, heights {}–{} \
                             (write_ms={} hot_pin={} async=1)",
                            idx,
                            entry_count,
                            height_range.0,
                            height_range.1,
                            write_ms,
                            pin,
                        );
                        disk.compact_oldest_async();
                    }
                    Err(e) => {
                        tracing::error!(
                            "DiskIndex: async spill FAILED — re-queue as sync risk; data may be lost: {e:#}"
                        );
                        {
                            let mut p = disk.pending_spills.write();
                            if let Some(i) = p.iter().position(|r| Arc::ptr_eq(r, &run)) {
                                p.remove(i);
                            }
                        }
                        if let Ok(owned) = Arc::try_unwrap(run) {
                            if let Err(e2) = disk.push_run_no_compact_sync(owned) {
                                tracing::error!("DiskIndex: async spill sync fallback failed: {e2:#}");
                            }
                        }
                    }
                }
                disk.async_spill_busy.store(false, Ordering::Release);
            });
        if let Err(e) = spawn_res {
            self.async_spill_busy.store(false, Ordering::Release);
            {
                let mut p = self.pending_spills.write();
                p.retain(|r| !Arc::ptr_eq(r, &run_for_err));
            }
            return Err(anyhow::anyhow!("spawn ibd-async-spill: {e}"));
        }
        Ok(())
    }

    /// Block until in-flight async spills finish (checkpoint / wipe paths).
    pub fn wait_pending_spills(&self) {
        for _ in 0..6000 {
            // up to ~60s
            if self.pending_spills.read().is_empty() && !self.async_spill_busy.load(Ordering::Acquire)
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        tracing::warn!(
            "DiskIndex: wait_pending_spills timed out (pending={} busy={})",
            self.pending_spills.read().len(),
            self.async_spill_busy.load(Ordering::Relaxed)
        );
    }

    /// Before installing a new pin, drop pins so that after the push
    /// `pinned_count ≤ max_segs`.
    ///
    /// Retain **seed (oldest) + newest** among existing pins; drop the middle.
    /// F16 dropped oldest (seed thrash). S1 largest-first dropped seed when
    /// spill entry_count > seed. Plain keep-oldest with `MAX_SEGS>2` would
    /// retain the first spill forever instead of recent megas.
    fn trim_hot_pins_for_new(&self, max_segs: usize) {
        let keep_prior = max_segs.saturating_sub(1);
        let snapshot = Arc::clone(&*self.segments.read());
        // Segment list order = age (oldest first).
        let pinned: Vec<&Arc<DiskSegment>> = snapshot
            .iter()
            .filter(|s| s.has_hot_body())
            .collect();
        if pinned.len() <= keep_prior {
            return;
        }
        let n = pinned.len();
        // Keep index 0 (seed) + the last (keep_prior-1) newest; clear middle.
        let mut keep = vec![false; n];
        keep[0] = true;
        let mut slots = keep_prior.saturating_sub(1);
        for i in (1..n).rev() {
            if slots == 0 {
                break;
            }
            keep[i] = true;
            slots -= 1;
        }
        for (i, seg) in pinned.iter().enumerate() {
            if !keep[i] {
                seg.clear_hot_body();
            }
        }
    }

    /// Drop all HotPin bodies (Critical/Emergency pressure). Segments stay on disk.
    pub fn clear_all_hot_pins(&self) {
        let snapshot = Arc::clone(&*self.segments.read());
        for seg in snapshot.iter() {
            seg.clear_hot_body();
        }
    }

    /// Pressure clear that keeps the seed (oldest) HotPin — dens late-view needle.
    /// Confirm re-score after first fair-Q miss wall 183.9≺185 (view gate passed).
    pub fn clear_hot_pins_keep_seed(&self) {
        let snapshot = Arc::clone(&*self.segments.read());
        let mut kept_seed = false;
        for seg in snapshot.iter() {
            if !seg.has_hot_body() {
                continue;
            }
            if !kept_seed {
                kept_seed = true;
                continue;
            }
            seg.clear_hot_body();
        }
    }

    /// Evict `run` to disk and kick async segment compaction (legacy combined path).
    ///
    /// Prefer [`Self::push_run_no_compact`] + [`Self::compact_oldest_async`] from the
    /// age-tiered compacter so the age merge lock is not held across multi-minute compact.
    pub fn push_run(self: &Arc<Self>, run: MemoryRun) -> anyhow::Result<()> {
        self.push_run_no_compact(run)?;
        // Sync path used by tests / non-compacter callers — keep blocking compact.
        // Async spill already kicks compact_oldest_async when the write finishes.
        if !async_disk_spill_from_env() {
            if let Err(e) = self.compact_oldest_if_needed() {
                tracing::error!("DiskIndex: segment compaction failed: {e}");
            }
        }
        Ok(())
    }

    /// Whether a background / sync disk-segment compaction is in progress.
    pub fn is_compacting(&self) -> bool {
        self.is_compacting.load(Ordering::Relaxed)
    }

    /// Current on-disk segment count (for COMPACTER_GATE diagnostics).
    pub fn segment_count(&self) -> usize {
        self.segments.read().len()
    }

    /// Segments eligible for fan-in merge (not currently HotPinned).
    ///
    /// S2 keep-seed HotPin was destroyed by fan-in merging the seed into a ~590M cold
    /// mega (compact_ms≈260s @~466k). Never merge a live HotPin body (seed or newest).
    fn compactable_count(&self) -> usize {
        self.segments
            .read()
            .iter()
            .filter(|s| !s.has_hot_body())
            .count()
    }

    /// Kick segment compaction on a dedicated thread so age-tier merge workers stay free.
    ///
    /// No-op if below fan-in, within the min interval, or another compact is already running.
    pub fn compact_oldest_async(self: &Arc<Self>) {
        let fan_in = disk_fan_in_from_env();
        let len = self.compactable_count();
        if len < fan_in {
            return;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last = LAST_COMPACT_FINISH.load(Ordering::Relaxed);
        if len < fan_in * 2 && now.saturating_sub(last) < COMPACT_MIN_INTERVAL_SECS {
            return;
        }
        if self
            .is_compacting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let disk = Arc::clone(self);
        if let Err(e) = std::thread::Builder::new()
            .name("utxo-disk-compact".into())
            .spawn(move || {
                let t0 = std::time::Instant::now();
                let mut passes = 0u32;
                let mut result = Ok(());
                let fan_in = disk_fan_in_from_env();
                while disk.compactable_count() >= fan_in {
                    result = disk.do_compact_plain();
                    passes += 1;
                    if result.is_err() {
                        break;
                    }
                }
                disk.is_compacting.store(false, Ordering::Release);
                let compact_ms = t0.elapsed().as_millis() as u64;
                if let Err(e) = result {
                    tracing::error!(
                        "DiskIndex: async compact failed after {} passes ({}ms): {e:#}",
                        passes,
                        compact_ms
                    );
                } else if passes > 0 {
                    tracing::info!(
                        "DiskIndex: async compact finished passes={} segs_now={} compact_ms={}",
                        passes,
                        disk.segments.read().len(),
                        compact_ms,
                    );
                }
            })
        {
            self.is_compacting.store(false, Ordering::Release);
            tracing::error!("DiskIndex: failed to spawn async compact thread: {e}");
        }
    }

    /// Merge the oldest `fan_in` segments into one when enough have accumulated.
    ///
    /// Reads entries from the oldest segments, performs a K-way merge with the same
    /// Add+Delete GC used by the memory tier, writes the merged result as a new segment,
    /// and deletes the old files. Only one compaction runs at a time (CAS guard).
    ///
    /// Called synchronously from `push_run` — since `push_run` is always called from a
    /// background compacter thread, this does not block IBD validation.
    /// Compact the segments that exist right now into one, blocking until done.
    ///
    /// Called by the checkpoint exporter *before* scanning so that GC (via the
    /// `CHECKPOINT_GC_FENCE` already set to `checkpoint_height`) has been applied
    /// to the existing disk segments.  After this call those segments have been
    /// merged and GC'd; segments that validation adds *during* this compaction
    /// contain only entries with height > checkpoint_height and will be filtered
    /// out by `scan_live_at_height` anyway — they do not need pre-scan GC.
    ///
    /// **Termination guarantee**: we run exactly as many passes as are needed to
    /// merge the *initial* segment count down to 1, then stop.  We do NOT loop
    /// on the live segment count; that would absorb newly-pushed validation
    /// segments indefinitely, growing the merged result and causing OOMs.
    pub fn compact_for_checkpoint_sync(&self) {
        self.wait_pending_spills();
        if let Err(e) = self.compact_for_checkpoint_sync_with_sink(
            -1,
            None::<fn(OutputKV) -> anyhow::Result<()>>,
        ) {
            tracing::warn!("compact_for_checkpoint_sync failed: {e:#}");
        }
    }

    /// Like [`Self::compact_for_checkpoint_sync`], but on the final re-GC pass optionally
    /// invokes `on_live` for each live `Add` entry written to the merged segment (piggyback export).
    ///
    /// Returns `Err` if the piggyback sink fails (e.g. `MDB_MAP_FULL`) — callers must not
    /// treat a failed sink as a successful export (live 2026-07-13: warn-only hang for 11h).
    pub fn compact_for_checkpoint_sync_with_sink<F>(
        &self,
        checkpoint_height: i32,
        on_live: Option<F>,
    ) -> anyhow::Result<()>
    where
        F: FnMut(super::types::OutputKV) -> anyhow::Result<()>,
    {
        self.wait_pending_spills();
        // Background compact skips HotPin seed; checkpoint must merge everything for fence GC.
        self.clear_all_hot_pins();
        let fence = super::memory_run::gc_fence_snapshot();
        let initial_count = self.segments.read().len();
        if initial_count <= 1
            && self.last_checkpoint_compact_fence.load(Ordering::Acquire) == fence
            && !self.is_compacting.load(Ordering::Relaxed)
        {
            tracing::info!(
                "compact_for_checkpoint_sync: skipped ({} segment(s), fence={} unchanged)",
                initial_count.max(1),
                fence
            );
            return Ok(());
        }
        // Spin until we own the CAS lock exclusively, waiting for any concurrent
        // background compaction to complete first.
        loop {
            if self
                .is_compacting
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // Snapshot segment count *after* acquiring the lock so we see the result
        // of any background compaction that just finished.
        let initial_count = self.segments.read().len();

        // CHECKPOINT_GC_FENCE was advanced before this call. Intermediate
        // `do_compact_plain` already uses `gc_fence_snapshot()`, so fan-in merges
        // apply the new fence. C1: do **not** force an extra 1→1 rewrite after
        // reduction — tee `ExportSink` on the *last* pass (last fan-in merge, or
        // the sole single-segment fence pass).
        let mut remaining = initial_count.max(1);
        let mut reduction_passes = 0usize;
        let fan_in = disk_fan_in_from_env();
        while remaining > 1 {
            remaining = remaining.saturating_sub(fan_in).saturating_add(1);
            reduction_passes += 1;
        }
        // At least one pass when a single segment must advance the fence for export.
        let total_passes = reduction_passes.max(1);

        tracing::debug!(
            "compact_for_checkpoint_sync: {} segments, {} passes (fence={}, C1 no +1 rewrite)",
            initial_count,
            total_passes,
            super::memory_run::gc_fence_snapshot(),
        );

        let t_fanin = std::time::Instant::now();
        let result = (|| -> anyhow::Result<()> {
            if total_passes > 1 {
                for _ in 0..(total_passes - 1) {
                    if self.segments.read().is_empty() {
                        break;
                    }
                    self.do_compact_plain()?;
                }
            }
            let fanin_ms = t_fanin.elapsed().as_millis() as u64;
            if !self.segments.read().is_empty() {
                match on_live {
                    Some(cb) => self.do_compact_with_sink_s0(checkpoint_height, cb, fanin_ms)?,
                    None => {
                        self.do_compact_plain()?;
                        tracing::info!(
                            "[IBD_COMPACT_S0] ckpt={} fanin_ms={} merge_ms=0 sink_ms=0 \
                             seg_pass1_ms=0 bloom_write_ms=0 directory_ms=0 swap_ms=0 \
                             (no piggyback sink)",
                            checkpoint_height,
                            fanin_ms,
                        );
                    }
                }
            }
            Ok(())
        })();

        self.last_checkpoint_compact_fence.store(fence, Ordering::Release);
        self.is_compacting.store(false, Ordering::Release);
        result
    }

    /// Compact oldest segments when compactable count ≥ `fan_in` (default 8). Safe to call
    /// after releasing any memory-age `is_merging` lock — this is the multi-minute path.
    pub fn compact_oldest_if_needed(&self) -> anyhow::Result<()> {
        let fan_in = disk_fan_in_from_env();
        let len = self.compactable_count();
        if len < fan_in {
            return Ok(());
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last = LAST_COMPACT_FINISH.load(Ordering::Relaxed);
        if len < fan_in * 2 && now.saturating_sub(last) < COMPACT_MIN_INTERVAL_SECS {
            return Ok(());
        }
        // CAS: only one compaction at a time. Skip if another is in progress —
        // that compaction will re-check after finishing.
        if self
            .is_compacting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            tracing::debug!(
                "DiskIndex: compact deferred (already in progress, compactable={})",
                len
            );
            return Ok(());
        }
        let t0 = std::time::Instant::now();
        // Loop: each pass merges the oldest `fan_in` cold segments (HotPin bodies skipped).
        let mut result = Ok(());
        let mut passes = 0u32;
        while self.compactable_count() >= fan_in {
            result = self.do_compact_plain();
            passes += 1;
            if result.is_err() {
                break;
            }
        }
        self.is_compacting.store(false, Ordering::Release);
        let compact_ms = t0.elapsed().as_millis() as u64;
        if passes > 0 {
            tracing::info!(
                "DiskIndex: compact finished passes={} segs_now={} compact_ms={}",
                passes,
                self.segments.read().len(),
                compact_ms,
            );
        }
        result
    }

    fn do_compact_with_sink_s0<F>(
        &self,
        checkpoint_height: i32,
        on_live: F,
        fanin_ms: u64,
    ) -> anyhow::Result<()>
    where
        F: FnMut(OutputKV) -> anyhow::Result<()>,
    {
        self.do_compact_impl(checkpoint_height, on_live, fanin_ms)
    }

    fn do_compact_plain(&self) -> anyhow::Result<()> {
        // Plain fan-in: noop sink; ckpt=-1 means ExportTee still visits Adds (legacy).
        self.do_compact_impl(-1, |_| Ok(()), 0)
    }

    fn do_compact_impl<F>(
        &self,
        checkpoint_height: i32,
        mut on_live: F,
        fanin_ms: u64,
    ) -> anyhow::Result<()>
    where
        F: FnMut(OutputKV) -> anyhow::Result<()>,
    {
        // Snapshot the oldest `fan_in` cold (non-HotPin) segments; pinned seed/newest stay.
        // They remain queryable during compaction.
        let to_compact: Vec<Arc<DiskSegment>> = {
            let r = self.segments.read();
            let fan = disk_fan_in_from_env();
            let cold: Vec<Arc<DiskSegment>> = r
                .iter()
                .filter(|s| !s.has_hot_body())
                .cloned()
                .collect();
            if cold.len() < fan {
                Vec::new()
            } else {
                cold[..fan].to_vec()
            }
        };
        if to_compact.is_empty() {
            return Ok(());
        }

        let total_in: usize = to_compact.iter().map(|s| s.entry_count).sum();
        tracing::info!(
            "DiskIndex: compacting {} segments ({} entries total)...",
            to_compact.len(),
            total_in,
        );

        // ── Streaming k-way merge with GC ────────────────────────────────────
        //
        // Peak memory is now O(bloom_filter) ≈ 300 MB regardless of input size.
        //
        // Previously: load all entries from every segment into RAM, then merge.
        //   8 segs × 30 M entries × 56 B = 13 GB  (OOM, or worse during Vec doubling).
        //
        // Now: one SegmentReader per segment (~500 KB buffers total), k-way merge
        // entry-by-entry, GC applied per-key-group, survivors streamed directly to the
        // output file via DiskSegment::write_from_iter.  No output Vec is ever
        // accumulated; the bloom filter (~300 MB for 200 M entries) is the only
        // significant allocation.  Vec-doubling OOMs are permanently eliminated.

        // ── GcMergeIter: k-way merge + GC, streaming to disk ─────────────────
        //
        // Processes one key group at a time (at most 2 entries: one Add, one Delete).
        // Survivors are streamed directly to the output file via write_from_iter.
        // No large Vec is ever accumulated — peak RAM is the bloom filter alone (~300 MB).

        struct GcMergeIter {
            readers: Vec<super::disk_segment::SegmentReader>,
            fence: i32,
            lookahead: Option<OutputKV>, // one-slot buffer for key-group handling
            out_buf: VecDeque<OutputKV>, // at most 2 entries (one key group)
            exhausted: bool,
        }

        impl GcMergeIter {
            /// Pop the globally minimum entry from all readers (plus lookahead).
            fn pop_raw(&mut self) -> Option<OutputKV> {
                if let Some(e) = self.lookahead.take() {
                    return Some(e);
                }
                let mut min_e: Option<OutputKV> = None;
                let mut min_i = 0usize;
                for (i, r) in self.readers.iter_mut().enumerate() {
                    match r.peek() {
                        Ok(Some(h)) if min_e.is_none_or(|m| h < m) => {
                            min_e = Some(h);
                            min_i = i;
                        }
                        Err(e) => {
                            tracing::warn!("GcMergeIter: read error on segment {i}: {e:#}");
                            return None;
                        }
                        _ => {}
                    }
                }
                if min_e.is_some() {
                    if let Err(e) = self.readers[min_i].advance() {
                        tracing::warn!("GcMergeIter: advance error: {e:#}");
                        return None;
                    }
                }
                min_e
            }

            /// Collect all entries for the key group starting at `first`, apply GC,
            /// push survivors to `out_buf`.
            fn process_group(&mut self, first: OutputKV) {
                let key = first.key;
                // Collect up to 4 entries for this key (normally 1-2).
                let mut group = [None::<OutputKV>; 4];
                group[0] = Some(first);
                let mut count = 1usize;
                loop {
                    match self.pop_raw() {
                        Some(e) if e.key == key && count < 4 => {
                            group[count] = Some(e);
                            count += 1;
                        }
                        other => {
                            self.lookahead = other; // save non-key entry (or None)
                            break;
                        }
                    }
                }
                // Apply GC per the same rules as MemoryRun::merge.
                match count {
                    1 => {
                        // Single entry — keep unconditionally.
                        self.out_buf.push_back(group[0].unwrap());
                    }
                    2 => {
                        let a = group[0].unwrap();
                        let b = group[1].unwrap();
                        // Sort order: key ASC, height DESC, Add before Delete for same h.
                        // Case 1: same-height Add + Delete  →  [Add(h), Delete(h)]
                        // Case 2: cross-height Delete + Add  →  [Delete(hd), Add(ha)] hd>ha
                        if (a.is_add() && b.is_delete() && a.height == b.height)
                            || (a.is_delete() && b.is_add() && a.height > b.height)
                        {
                            if a.height > self.fence {
                                self.out_buf.push_back(a);
                                self.out_buf.push_back(b);
                            }
                            // else: cancel both (Delete at or below fence)
                        }
                        // Unexpected ordering — keep both defensively.
                        else {
                            self.out_buf.push_back(a);
                            self.out_buf.push_back(b);
                        }
                    }
                    _ => {
                        // More than 2 entries for the same key (shouldn't happen in a
                        // valid UTXO index). Keep all defensively.
                        for slot in group.iter().take(count) {
                            if let Some(e) = *slot {
                                self.out_buf.push_back(e);
                            }
                        }
                    }
                }
            }
        }

        impl Iterator for GcMergeIter {
            type Item = OutputKV;

            fn next(&mut self) -> Option<OutputKV> {
                // Drain buffered output from the last key group first.
                if let Some(e) = self.out_buf.pop_front() {
                    return Some(e);
                }
                if self.exhausted {
                    return None;
                }
                // Fetch the first entry of the next key group.
                loop {
                    let first = match self.pop_raw() {
                        Some(e) => e,
                        None => {
                            self.exhausted = true;
                            return None;
                        }
                    };
                    self.process_group(first);
                    if let Some(e) = self.out_buf.pop_front() {
                        return Some(e);
                    }
                    // Group was fully GC'd — continue to next key.
                }
            }
        }

        let readers: Vec<super::disk_segment::SegmentReader> =
            to_compact.iter().map(|s| s.stream()).collect();
        let fence = super::memory_run::gc_fence_snapshot();

        let merge_iter = GcMergeIter {
            readers,
            fence,
            lookahead: None,
            out_buf: VecDeque::with_capacity(4),
            exhausted: false,
        };

        struct ExportTee<I, F> {
            inner: I,
            ckpt: i32,
            on_live: F,
            /// Shared with caller: first piggyback sink failure (e.g. MDB_MAP_FULL).
            /// When set, iteration stops so compact does not spin for hours while writes fail.
            sink_err: std::sync::Arc<std::sync::Mutex<Option<anyhow::Error>>>,
            merge_ns: std::sync::Arc<AtomicU64>,
            sink_ns: std::sync::Arc<AtomicU64>,
        }

        impl<I, F> Iterator for ExportTee<I, F>
        where
            I: Iterator<Item = OutputKV>,
            F: FnMut(OutputKV) -> anyhow::Result<()>,
        {
            type Item = OutputKV;

            fn next(&mut self) -> Option<Self::Item> {
                if self.sink_err.lock().ok()?.is_some() {
                    return None;
                }
                let t_merge = std::time::Instant::now();
                let e = self.inner.next()?;
                self.merge_ns
                    .fetch_add(t_merge.elapsed().as_nanos() as u64, Ordering::Relaxed);
                if e.is_add() && e.id != 0 && (self.ckpt < 0 || e.height <= self.ckpt) {
                    let t_sink = std::time::Instant::now();
                    let sink_res = (self.on_live)(e);
                    self.sink_ns
                        .fetch_add(t_sink.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    if let Err(err) = sink_res {
                        tracing::error!(
                            "checkpoint piggyback export sink failed — aborting compact: {err:#}"
                        );
                        if let Ok(mut g) = self.sink_err.lock() {
                            *g = Some(err);
                        }
                        return None;
                    }
                }
                Some(e)
            }
        }

        // Stream survivors directly to a new segment file. No output Vec.
        let idx = self.next_idx.fetch_add(1, Ordering::Relaxed);
        let sink_err = std::sync::Arc::new(std::sync::Mutex::new(None));
        super::disk_segment::reset_write_from_iter_stats();
        let merge_ns = std::sync::Arc::new(AtomicU64::new(0));
        let sink_ns = std::sync::Arc::new(AtomicU64::new(0));
        let new_seg = {
            let iter = ExportTee {
                inner: merge_iter,
                ckpt: checkpoint_height,
                on_live,
                sink_err: std::sync::Arc::clone(&sink_err),
                merge_ns: std::sync::Arc::clone(&merge_ns),
                sink_ns: std::sync::Arc::clone(&sink_ns),
            };
            let mut peekable = iter.peekable();
            if peekable.peek().is_some() {
                Some(Arc::new(DiskSegment::write_from_iter(
                    &self.seg_dir,
                    idx,
                    total_in, // bloom filter capacity (upper bound)
                    peekable,
                )?))
            } else {
                None
            }
        };
        let merge_ns = merge_ns.load(Ordering::Relaxed);
        let sink_ns = sink_ns.load(Ordering::Relaxed);
        if let Some(err) = sink_err.lock().ok().and_then(|mut g| g.take()) {
            // Drop the new segment file if we wrote a partial merge before sink failure.
            if let Some(seg) = new_seg.as_ref() {
                let _ = std::fs::remove_file(&seg.path);
            }
            return Err(err.context(
                "piggyback export sink failed during disk compact (often MDB_MAP_FULL — \
                 grow LMDB map / free freelist before Phase 3)",
            ));
        }

        let total_out = new_seg.as_ref().map_or(0, |s| s.entry_count);
        let (seg_pass1_ms, directory_ms) = super::disk_segment::take_write_from_iter_stats();
        let merge_ms = merge_ns / 1_000_000;
        let sink_ms = sink_ns / 1_000_000;
        let bloom_write_ms = seg_pass1_ms.saturating_sub(merge_ms + sink_ms);

        // Atomically swap old segments for the new merged one.
        let t_swap = std::time::Instant::now();
        {
            let mut w = self.segments.write();
            let segs = Arc::make_mut(&mut *w);
            let compact_ptrs: std::collections::HashSet<*const DiskSegment> =
                to_compact.iter().map(Arc::as_ptr).collect();
            let insert_pos = segs
                .iter()
                .position(|s| compact_ptrs.contains(&Arc::as_ptr(s)))
                .unwrap_or(0);
            segs.retain(|s| !compact_ptrs.contains(&Arc::as_ptr(s)));
            if let Some(seg) = new_seg {
                segs.insert(insert_pos, seg);
            }
        }

        // Delete old segment files.
        for seg in &to_compact {
            if let Err(e) = std::fs::remove_file(&seg.path) {
                tracing::warn!(
                    "DiskIndex: could not remove old segment {:?}: {e}",
                    seg.path
                );
            }
        }
        let swap_ms = t_swap.elapsed().as_millis() as u64;

        if checkpoint_height >= 0 {
            tracing::info!(
                "[IBD_COMPACT_S0] ckpt={} fanin_ms={} merge_ms={} sink_ms={} \
                 seg_pass1_ms={} bloom_write_ms={} directory_ms={} swap_ms={} \
                 entries_in={} entries_out={}",
                checkpoint_height,
                fanin_ms,
                merge_ms,
                sink_ms,
                seg_pass1_ms,
                bloom_write_ms,
                directory_ms,
                swap_ms,
                total_in,
                total_out,
            );
        }

        tracing::info!(
            "DiskIndex: compaction done — {total_in} entries in, {total_out} out (GC'd {})",
            total_in.saturating_sub(total_out),
        );
        LAST_COMPACT_FINISH.store(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            Ordering::Relaxed,
        );
        Ok(())
    }

    /// Batch query all disk segments (oldest-to-newest) for unresolved keys.
    ///
    /// Only called for keys with `ids[i] == OutputId::MAX` after all memory ages.
    /// Keys with `OUTPUT_ID_DELETED` are skipped (already resolved as spent in memory).
    pub fn batch_query(&self, keys: &[[u8; 36]], ids: &mut [OutputId], before: i32) {
        // F5a: attribute pread fan-out for HOTPATH timers (reset every query).
        super::disk_segment::reset_disk_io_stats();
        // Skip segment lookup when all keys are already resolved (none remaining as MAX).
        if ids.iter().any(|id| *id == OutputId::MAX) {
            // HP-M3: pending async spills are newer than on-disk segments — query first.
            {
                let pending = self.pending_spills.read();
                for run in pending.iter().rev() {
                    if !ids.iter().any(|id| *id == OutputId::MAX) {
                        break;
                    }
                    run.batch_lookup(keys, ids, 0, before);
                }
            }
            let snapshot = Arc::clone(&*super::timed_segments_read(&self.segments));

            // Query newest-to-oldest: last segment first (most recent overflow data).
            for seg in snapshot.iter().rev() {
                if !ids.iter().any(|id| *id == OutputId::MAX) {
                    break;
                }
                if let Err(e) = seg.batch_lookup(keys, ids, 0, before) {
                    tracing::error!("DiskIndex: segment read error: {}", e);
                    // Continue — partial results are better than none.
                }
            }
        }

        // Always normalize OUTPUT_ID_DELETED → OutputId::MAX for callers, even when the
        // segment lookup was skipped. Callers (SpendSession) filter MAX as "not found";
        // a spent-in-memory sentinel must not reach UtxoTable::fetch.
        use super::types::OUTPUT_ID_DELETED;
        for id in ids.iter_mut() {
            if *id == OUTPUT_ID_DELETED {
                *id = OutputId::MAX;
            }
        }
    }

    /// Total approximate resident bytes for in-RAM bloom filters + directories across all segments.
    pub fn bloom_bytes_total(&self) -> usize {
        self.segments
            .read()
            .iter()
            .map(|s| s.ram_bytes())
            .sum()
    }

    /// Call `f` with a snapshot of all segments (oldest-to-newest) for scanning.
    ///
    /// Used by `UtxoIndex::scan_all_live` and `scan_live_at_height` for checkpoint exports.
    ///
    /// **Critical**: the read lock is released **before** calling `f`. Checkpoint exports
    /// hold `with_segments` for minutes; if the read lock were held throughout, compacter
    /// threads trying to push evicted segments (`push_run` → write lock) would block
    /// indefinitely, allowing age-3 to accumulate unbounded frozen runs (40+ GB of RSS).
    ///
    /// Safety: `Arc<DiskSegment>` keeps each segment file open even if compaction removes
    /// its path entry — callers must use `seg.read_all_entries()` (existing `Arc<File>`
    /// handle) rather than `File::open(&seg.path)` to avoid TOCTOU races.
    pub fn with_segments<F>(&self, f: F)
    where
        F: FnOnce(&[Arc<DiskSegment>]),
    {
        // Clone the Vec<Arc<DiskSegment>> (cheap: only Arc refcount bumps) then drop the guard.
        let snapshot = Arc::clone(&*self.segments.read());
        f(snapshot.as_slice());
    }
}

impl std::fmt::Debug for DiskIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskIndex")
            .field("seg_dir", &self.seg_dir)
            .field("segment_count", &self.segment_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::{OutputId, OutputKV};
    use std::sync::{Mutex, MutexGuard};

    fn hot_pin_env_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn disk_fan_in_env_default_and_clamp() {
        // SAFETY: single-threaded test; env restored before exit.
        unsafe {
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            assert_eq!(disk_fan_in_from_env(), 8);
            std::env::set_var("BLVM_IBD_DISK_FAN_IN", "4");
            assert_eq!(disk_fan_in_from_env(), 4);
            std::env::set_var("BLVM_IBD_DISK_FAN_IN", "1");
            assert_eq!(disk_fan_in_from_env(), 2);
            std::env::set_var("BLVM_IBD_DISK_FAN_IN", "99");
            assert_eq!(disk_fan_in_from_env(), 32);
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        }
    }

    #[test]
    fn register_seg_hot_pins_seed_when_eligible() {
        let _guard = hot_pin_env_lock();
        // SAFETY: exclusive via hot_pin_env_lock; env restored before exit.
        unsafe {
            std::env::set_var("BLVM_IBD_HOT_PIN", "1");
            std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let k0 = {
            let mut k = [0u8; 36];
            k[0] = 9;
            k
        };
        let k1 = {
            let mut k = [0u8; 36];
            k[0] = 10;
            k
        };
        let entries = vec![
            OutputKV::new_add(k0, 100, 1000),
            OutputKV::new_add(k1, 101, 1001),
        ];
        // Streaming seed path: write without pin, then register_seg installs HotPin.
        let seg = DiskSegment::write_from_slice(tmp.path(), 0, (100, 101), &entries)
            .expect("write_from_slice");
        assert!(!seg.has_hot_body());
        disk.register_seg(seg);
        assert!(disk.segments.read()[0].has_hot_body());

        let keys = [k0, k1];
        let mut ids = [OutputId::MAX, OutputId::MAX];
        disk.batch_query(&keys, &mut ids, 200);
        assert_eq!(ids[0], 1000);
        assert_eq!(ids[1], 1001);

        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }

    #[test]
    fn compact_skips_hot_pin_seed_prefix() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_HOT_PIN", "1");
            std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_SEGS", "2");
            std::env::set_var("BLVM_IBD_DISK_FAN_IN", "3");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |b: u8| {
            let mut k = [0u8; 36];
            k[0] = b;
            k
        };
        disk.register_seg(
            DiskSegment::write_from_slice(
                tmp.path(),
                0,
                (1, 1),
                &[
                    OutputKV::new_add(mk(1), 1, 10),
                    OutputKV::new_add(mk(2), 1, 11),
                ],
            )
            .expect("seed"),
        );
        // MAX_SEGS=2 → seed + newest hot; need ≥3 cold spills → push 4 megas.
        for i in 0u8..4 {
            let b = 10 + i * 2;
            let h = 10 + i32::from(i);
            disk.push_run_no_compact(MemoryRun::build(vec![
                OutputKV::new_add(mk(b), h, 100 + u64::from(i)),
                OutputKV::new_add(mk(b + 1), h, 200 + u64::from(i)),
            ]))
            .expect("spill");
        }
        assert!(disk.segments.read()[0].has_hot_body(), "seed pinned before compact");
        assert!(disk.compactable_count() >= 3, "cold={}", disk.compactable_count());
        // Bypass min-interval throttle used by compact_oldest_if_needed.
        disk.do_compact_plain().expect("compact");
        let segs = disk.segments.read();
        assert!(segs[0].has_hot_body(), "seed HotPin must survive compact");
        assert!(
            segs.iter().filter(|s| s.has_hot_body()).count() >= 1,
            "at least seed still pinned"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        }
        std::mem::forget(tmp);
    }

    #[test]
    fn hot_pin_max_segs2_keeps_seed_and_newest() {
        let _guard = hot_pin_env_lock();
        // SAFETY: exclusive via hot_pin_env_lock; env restored before exit.
        unsafe {
            std::env::set_var("BLVM_IBD_HOT_PIN", "1");
            std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_SEGS", "2");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |b: u8| {
            let mut k = [0u8; 36];
            k[0] = b;
            k
        };
        let seed = vec![
            OutputKV::new_add(mk(1), 1, 10),
            OutputKV::new_add(mk(2), 1, 11),
        ];
        let seg = DiskSegment::write_from_slice(tmp.path(), 0, (1, 1), &seed).expect("seed write");
        disk.register_seg(seg);
        disk.push_run_no_compact(MemoryRun::build(vec![
            OutputKV::new_add(mk(3), 2, 20),
            OutputKV::new_add(mk(4), 2, 21),
        ]))
        .expect("spill1");
        disk.push_run_no_compact(MemoryRun::build(vec![
            OutputKV::new_add(mk(5), 3, 30),
            OutputKV::new_add(mk(6), 3, 31),
        ]))
        .expect("spill2");
        let segs = disk.segments.read();
        assert_eq!(segs.len(), 3);
        assert!(segs[0].has_hot_body(), "seed must stay pinned");
        assert!(!segs[1].has_hot_body(), "middle spill must be trimmed");
        assert!(segs[2].has_hot_body(), "newest spill must be pinned");
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
        }
        std::mem::forget(tmp);
    }

    #[test]
    fn hot_pin_max_segs3_keeps_seed_and_two_newest() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_HOT_PIN", "1");
            std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_SEGS", "3");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |b: u8| {
            let mut k = [0u8; 36];
            k[0] = b;
            k
        };
        let seed = vec![
            OutputKV::new_add(mk(1), 1, 10),
            OutputKV::new_add(mk(2), 1, 11),
        ];
        disk.register_seg(
            DiskSegment::write_from_slice(tmp.path(), 0, (1, 1), &seed).expect("seed"),
        );
        for (i, base) in [(2u8, 20u64), (3, 30), (4, 40)].into_iter().enumerate() {
            let h = (i + 2) as i32;
            disk.push_run_no_compact(MemoryRun::build(vec![
                OutputKV::new_add(mk(base.0), h, base.1),
                OutputKV::new_add(mk(base.0 + 1), h, base.1 + 1),
            ]))
            .expect("spill");
        }
        let segs = disk.segments.read();
        assert_eq!(segs.len(), 4);
        assert!(segs[0].has_hot_body(), "seed");
        assert!(!segs[1].has_hot_body(), "oldest spill trimmed");
        assert!(segs[2].has_hot_body(), "2nd newest");
        assert!(segs[3].has_hot_body(), "newest");
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
        }
        std::mem::forget(tmp);
    }

    #[test]
    fn hot_pin_serves_batch_query_and_clears_on_demand() {
        let _guard = hot_pin_env_lock();
        // SAFETY: exclusive via hot_pin_env_lock; env restored before exit.
        unsafe {
            std::env::set_var("BLVM_IBD_HOT_PIN", "1");
            std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let k0 = {
            let mut k = [0u8; 36];
            k[0] = 1;
            k
        };
        let k1 = {
            let mut k = [0u8; 36];
            k[0] = 2;
            k
        };
        let run = MemoryRun::build(vec![
            OutputKV::new_add(k0, 10, 100),
            OutputKV::new_add(k1, 11, 101),
        ]);
        disk.push_run_no_compact(run).expect("push");
        assert!(disk.segments.read()[0].has_hot_body());

        let keys = [k0, k1];
        let mut ids = [OutputId::MAX, OutputId::MAX];
        disk.batch_query(&keys, &mut ids, 100);
        assert_eq!(ids[0], 100);
        assert_eq!(ids[1], 101);

        disk.clear_all_hot_pins();
        assert!(!disk.segments.read()[0].has_hot_body());
        // Still correct via pread after pin drop.
        let mut ids2 = [OutputId::MAX, OutputId::MAX];
        disk.batch_query(&keys, &mut ids2, 100);
        assert_eq!(ids2[0], 100);
        assert_eq!(ids2[1], 101);

        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }

    #[test]
    fn clear_hot_pins_keep_seed_preserves_oldest() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_HOT_PIN", "1");
            std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_SEGS", "2");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |b: u8| {
            let mut k = [0u8; 36];
            k[0] = b;
            k
        };
        disk.push_run_no_compact(MemoryRun::build(vec![
            OutputKV::new_add(mk(1), 10, 100),
            OutputKV::new_add(mk(2), 11, 101),
        ]))
        .expect("seed");
        disk.push_run_no_compact(MemoryRun::build(vec![
            OutputKV::new_add(mk(3), 12, 102),
            OutputKV::new_add(mk(4), 13, 103),
        ]))
        .expect("newest");
        disk.clear_hot_pins_keep_seed();
        let segs = disk.segments.read();
        assert!(segs[0].has_hot_body(), "seed survives pressure clear");
        assert!(!segs[1].has_hot_body(), "newest dropped");
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
        }
        std::mem::forget(tmp);
    }

    #[test]
    fn hot_pin_max_segs_keeps_prior_mega() {
        let _guard = hot_pin_env_lock();
        // SAFETY: exclusive via hot_pin_env_lock; env restored before exit.
        unsafe {
            std::env::set_var("BLVM_IBD_HOT_PIN", "1");
            std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_SEGS", "2");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let k0 = {
            let mut k = [0u8; 36];
            k[0] = 10;
            k
        };
        let k1 = {
            let mut k = [0u8; 36];
            k[0] = 20;
            k
        };
        let k0b = {
            let mut k = [0u8; 36];
            k[0] = 11;
            k
        };
        let k1b = {
            let mut k = [0u8; 36];
            k[0] = 21;
            k
        };
        disk.push_run_no_compact(MemoryRun::build(vec![
            OutputKV::new_add(k0, 10, 100),
            OutputKV::new_add(k0b, 11, 101),
        ]))
        .expect("push0");
        disk.push_run_no_compact(MemoryRun::build(vec![
            OutputKV::new_add(k1, 20, 200),
            OutputKV::new_add(k1b, 21, 201),
        ]))
        .expect("push1");
        let segs = disk.segments.read();
        assert!(segs[0].has_hot_body(), "prior mega should stay pinned");
        assert!(segs[1].has_hot_body(), "newest mega should be pinned");
        drop(segs);

        // Third pin under max=2: keep seed (oldest) + newest; drop middle.
        let k2 = {
            let mut k = [0u8; 36];
            k[0] = 30;
            k
        };
        let k2b = {
            let mut k = [0u8; 36];
            k[0] = 31;
            k
        };
        disk.push_run_no_compact(MemoryRun::build(vec![
            OutputKV::new_add(k2, 30, 300),
            OutputKV::new_add(k2b, 31, 301),
        ]))
        .expect("push2");
        let segs = disk.segments.read();
        assert!(segs[0].has_hot_body(), "seed/oldest stays pinned");
        assert!(!segs[1].has_hot_body(), "middle spill trimmed");
        assert!(segs[2].has_hot_body(), "newest pinned");

        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
        }
        std::mem::forget(tmp);
    }

    #[test]
    fn spill_max_entries_size_splits_into_multiple_segments() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_SPILL_MAX_ENTRIES", "2");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::set_var("BLVM_IBD_HOT_PIN", "1");
            std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "1");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mut kvs = Vec::new();
        for i in 0u8..5 {
            let mut k = [0u8; 36];
            k[0] = i + 1;
            kvs.push(OutputKV::new_add(k, 10 + i32::from(i), 100 + u64::from(i)));
        }
        disk.push_run_no_compact(MemoryRun::build(kvs))
            .expect("size-split push");
        assert_eq!(disk.segment_count(), 3, "5 entries @ max=2 → 3 segments");
        let keys: Vec<_> = (0u8..5)
            .map(|i| {
                let mut k = [0u8; 36];
                k[0] = i + 1;
                k
            })
            .collect();
        let mut ids = vec![OutputId::MAX; 5];
        disk.batch_query(&keys, &mut ids, 100);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(*id, 100 + i as u64);
        }
        unsafe {
            std::env::remove_var("BLVM_IBD_SPILL_MAX_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }

    #[test]
    fn async_spill_serves_queries_via_pending_then_registers() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_ASYNC_DISK_SPILL", "1");
            std::env::set_var("BLVM_IBD_HOT_PIN", "1");
            std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
            std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let k0 = {
            let mut k = [0u8; 36];
            k[0] = 7;
            k
        };
        let k1 = {
            let mut k = [0u8; 36];
            k[0] = 8;
            k
        };
        disk.push_run_no_compact(MemoryRun::build(vec![
            OutputKV::new_add(k0, 10, 100),
            OutputKV::new_add(k1, 11, 101),
        ]))
        .expect("async push");
        // Immediately queryable (pending and/or registered).
        let keys = [k0, k1];
        let mut ids = [OutputId::MAX, OutputId::MAX];
        disk.batch_query(&keys, &mut ids, 100);
        assert_eq!(ids[0], 100);
        assert_eq!(ids[1], 101);
        disk.wait_pending_spills();
        assert_eq!(disk.segment_count(), 1);
        assert!(disk.segments.read()[0].has_hot_body());
        unsafe {
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
            std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }
}
