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
/// after the 8th push, the 8 oldest are merged (with GC) and written as one or
/// more chunks of `COMPACT_MAX_ENTRIES` (default 20M — dest-bc mega-seg cap).
/// Each subsequent push brings it to 2, 3, … up to 8, then compacts again.
///
/// R-268 358k: export GC'd then two spills left **5** overlapping segs. Background
/// compact required `len >= fan_in` so 5 sat; dest-bc checkpoint compact had
/// already collapsed to 1–2 (`cands ≈ 130 × segs`). After `GC'd>0`, compact the
/// mid-band `[stall_split_min, fan_in)` too. HP-M4: 1–3 segs still do not fold.
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

/// dest-bc 650k: fan-in of 8 packed ~300M-entry megas (251 KiB directory buckets,
/// 3570 ms / 1.6 GiB HOTPATH). dest-ba same height kept ~15 KiB buckets across 5–8
/// smaller segs (33 ms / 31 MiB). HP-M4 `SPILL_MAX_ENTRIES` REVERT was spill-time
/// split (more cold segs at 300–400k tip). This cap is **compact-output only**.
/// `0` = unlimited (legacy one mega). Default 20M ≈ dest-ba / HotPin-min shape.
pub(crate) const COMPACT_MAX_ENTRIES_DEFAULT: usize = 20_000_000;

pub(crate) fn compact_max_entries_from_env() -> usize {
    std::env::var("BLVM_IBD_COMPACT_MAX_ENTRIES")
        .ok()
        .and_then(|s| {
            let t = s.trim();
            if t.is_empty() { None } else { t.parse().ok() }
        })
        .unwrap_or(COMPACT_MAX_ENTRIES_DEFAULT)
}

/// Which cold segs a background/export compact pass merges.
enum CompactScope {
    /// Oldest `fan_in` cold segs (steady-state 20M chunks).
    FanIn,
    /// Youngest `fan_in` cold segs. Oldest window stayed a photocopy (R-366).
    FanInYoung,
    /// Every cold seg (checkpoint last pass / 1→1 tee). Writes output.
    AllCold,
    /// dest-bc 144G: k-way GC merge for the piggyback sink, **no** combined write.
    /// AllCold of leftover 20M chunks after mega drain is still ENOSPC (67G extra).
    TeeScan,
    /// dest-bc megas: oldest oversized + last oversized (eat 300M files; do not
    /// pair a mega with a trailing 20M spill — dest-bc Deletes live in later
    /// megas). If only one mega exists, pair oldest + that mega (or mega + newest
    /// when the mega is already cold[0]) so old Adds still GC (dest-bc 144G).
    OldestAndNewest,
}

/// Inputs for `[IBD_FANIN_PHOTOCOPY_SKIP]` — count-neutral FanIn that cannot GC.
struct FaninPhotocopySkip {
    n_in: usize,
    entries: usize,
    cap: usize,
    out_chunks: usize,
    fence: i32,
    last_fence: i32,
    min_h: i32,
}

/// Peak extra disk for an AllCold write: dest-ba-like leftover (≤`fan_in` × 20M).
fn checkpoint_allcold_write_ok(
    cold_len: usize,
    cold_entries: usize,
    fan: usize,
    max: usize,
) -> bool {
    if cold_len <= fan {
        return true;
    }
    if max == 0 {
        return cold_len <= fan;
    }
    cold_entries <= max.saturating_mul(fan.max(1))
}

/// Indices into a cold-seg entry-count list for [`CompactScope::OldestAndNewest`].
fn stall_pair_indices(entry_counts: &[usize], max: usize) -> Option<(usize, usize)> {
    let n = entry_counts.len();
    if n < 2 {
        return None;
    }
    let Some(oi) = entry_counts.iter().position(|&c| max > 0 && c > max) else {
        return Some((0, n - 1));
    };
    let last_over = entry_counts
        .iter()
        .rposition(|&c| max > 0 && c > max)
        .expect("oi exists");
    if oi == last_over {
        if oi == 0 {
            Some((0, n - 1))
        } else {
            Some((0, oi))
        }
    } else {
        Some((oi, last_over))
    }
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

#[cfg(test)]
static TEST_MAX_COMPACT_WRITE_INPUT: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_COMPACT_WRITE_CALLS: AtomicUsize = AtomicUsize::new(0);

fn note_compact_write_input(n: usize) {
    let _ = n;
    #[cfg(test)]
    {
        TEST_MAX_COMPACT_WRITE_INPUT.fetch_max(n, Ordering::Relaxed);
        TEST_COMPACT_WRITE_CALLS.fetch_add(1, Ordering::Relaxed);
    }
}

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
    /// GC fence observed at the last background FanIn that actually ran.
    /// `i32::MIN` = no fan-in yet. Used with [`Self::fanin_photocopy_skip`] so a
    /// stuck fence cannot schedule an 8→8 rewrite of at-cap chunks.
    last_fanin_gc_fence: AtomicI32,
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
        super::disk_segment::log_compact_dontneed_once();
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
                last_fanin_gc_fence: AtomicI32::new(i32::MIN),
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
                        if let Ok(owned) = Arc::try_unwrap(run)
                            && let Err(e2) = disk.push_run_no_compact_sync(owned) {
                                tracing::error!("DiskIndex: async spill sync fallback failed: {e2:#}");
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
            if self.pending_spills.read().is_empty()
                && !self.async_spill_busy.load(Ordering::Acquire)
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
        let pinned: Vec<&Arc<DiskSegment>> = snapshot.iter().filter(|s| s.has_hot_body()).collect();
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
        if !async_disk_spill_from_env()
            && let Err(e) = self.compact_oldest_if_needed()
        {
            tracing::error!("DiskIndex: segment compaction failed: {e}");
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

    /// Cold (non-HotPin) journal entries across on-disk segments.
    /// Slice A: TeeScan input is this sum; 313M returned, 448M was still scanning at SIGKILL.
    pub fn cold_entry_count(&self) -> u64 {
        self.cold_len_and_entries().1 as u64
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

    fn cold_len_and_entries(&self) -> (usize, usize) {
        let r = self.segments.read();
        let mut n = 0usize;
        let mut entries = 0usize;
        for s in r.iter() {
            if s.has_hot_body() {
                continue;
            }
            n += 1;
            entries += s.entry_count;
        }
        (n, entries)
    }

    fn has_oversized_cold(&self) -> bool {
        let max = compact_max_entries_from_env();
        if max == 0 {
            return false;
        }
        let fan = disk_fan_in_from_env();
        // HP-M4: 1–3 segs must not fold. dest-bc 9 megas @666k is n≥fan_in — still fold
        // pairs (do not 8-way 300M files; peak extra disk).
        if self.compactable_count() < Self::stall_split_min_compactable(fan) {
            return false;
        }
        self.oldest_oversized_cold(max).is_some()
    }

    /// dest-bc sat at 5–7 megas (`fan_in=8` never fired). Do **not** fold a lone
    /// mega spill at 1–3 segs — that is HP-M4 (REVERT, 300–400k tip).
    fn stall_split_min_compactable(fan_in: usize) -> usize {
        fan_in.div_ceil(2).max(2)
    }

    /// Oldest cold (non-HotPin) segment larger than `max` — dest-bc 7-mega stall.
    fn oldest_oversized_cold(&self, max: usize) -> Option<Arc<DiskSegment>> {
        self.segments
            .read()
            .iter()
            .find(|s| !s.has_hot_body() && s.entry_count > max)
            .cloned()
    }

    /// Kick segment compaction on a dedicated thread so age-tier merge workers stay free.
    ///
    /// dest-bc 660k sat at 7 megas (`fan_in=8` never fired). Stall-band oversized
    /// cold folds **oldest oversized + last oversized** (GC + 20M cap). Peak extra
    /// disk is two segs — take-all of dest-bc 144G would ENOSPC on 65G free.
    /// HP-M4: 1–3 stay.
    pub fn compact_oldest_async(self: &Arc<Self>) {
        let fan_in = disk_fan_in_from_env();
        let len = self.compactable_count();
        let oversize = self.has_oversized_cold();
        // HP-M4: 1–3 stay. Mid-band [stall_min, fan_in) is R-268 358k (5 segs).
        if !oversize && len < Self::stall_split_min_compactable(fan_in) {
            return;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last = LAST_COMPACT_FINISH.load(Ordering::Relaxed);
        if !oversize
            && len >= Self::stall_split_min_compactable(fan_in)
            && len < fan_in * 2
            && now.saturating_sub(last) < COMPACT_MIN_INTERVAL_SECS
        {
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
                let result = disk.run_fanin_and_stall_split();
                disk.is_compacting.store(false, Ordering::Release);
                let compact_ms = t0.elapsed().as_millis() as u64;
                match result {
                    Err(e) => tracing::error!(
                        "DiskIndex: async compact failed ({}ms): {e:#}",
                        compact_ms
                    ),
                    Ok((passes, stall_alls)) if passes > 0 || stall_alls > 0 => {
                        tracing::info!(
                            "DiskIndex: async compact finished passes={} stall_all={} segs_now={} compact_ms={}",
                            passes,
                            stall_alls,
                            disk.segments.read().len(),
                            compact_ms,
                        );
                    }
                    Ok(_) => {}
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
        if let Err(e) = self
            .compact_for_checkpoint_sync_with_sink(-1, None::<fn(OutputKV) -> anyhow::Result<()>>)
        {
            tracing::warn!("compact_for_checkpoint_sync failed: {e:#}");
        }
    }

    /// Like [`Self::compact_for_checkpoint_sync`], but on the final re-GC pass optionally
    /// invokes `on_live` for each live `Add` entry written to the merged segment (piggyback export).
    ///
    /// Returns `Err` if the piggyback sink fails (e.g. `MDB_MAP_FULL`) — callers must not
    /// treat a failed sink as a successful export (live 2026-07-13: warn-only hang for 11h).
    /// Returns `(tee_merged_entries, cold_segs_at_start)`.
    ///
    /// `tee_merged_entries` is the number of live `Add`s the piggyback sink actually
    /// saw. dest-bc: last pass no-op'd under `fan_in=8` → tee 0 → persist was memory overlay.
    pub fn compact_for_checkpoint_sync_with_sink<F>(
        &self,
        checkpoint_height: i32,
        on_live: Option<F>,
    ) -> anyhow::Result<(u64, usize)>
    where
        F: FnMut(super::types::OutputKV) -> anyhow::Result<()>,
    {
        self.wait_pending_spills();
        // Do not clear HotPin. The tee reads those files in place; dropping the
        // pin forces every lookup back to pread for the whole export wall.
        let fence = super::memory_run::gc_fence_snapshot();
        let cold_segs_at_start = self.compactable_count();
        let initial_count = self.segments.read().len();
        let need_tee = on_live.is_some();
        // Need the 1→1 rewrite whenever a sink must visit disk Adds. Skipping when
        // fence is unchanged is why C1's 1-seg tee never ran (dest-bc overlay-only persists).
        if !need_tee
            && initial_count <= 1
            && self.last_checkpoint_compact_fence.load(Ordering::Acquire) == fence
            && !self.is_compacting.load(Ordering::Relaxed)
        {
            tracing::info!(
                "compact_for_checkpoint_sync: skipped ({} segment(s), fence={} unchanged)",
                initial_count.max(1),
                fence
            );
            return Ok((0, cold_segs_at_start));
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

        tracing::debug!(
            "compact_for_checkpoint_sync: {} segments (fence={}, dest-bc pair drain then tee)",
            initial_count,
            super::memory_run::gc_fence_snapshot(),
        );

        let t_fanin = std::time::Instant::now();
        let still_hold = std::cell::Cell::new(true);
        let result = (|| -> anyhow::Result<u64> {
            // Decide before the stall drain. A set that is already a tee must not
            // hold the compact lock while stall pairs try to shrink it into AllCold.
            // AllCold omits HotPin segments, and those UTXOs are not in the memory
            // overlay, so a pin forces TeeScan.
            let max = compact_max_entries_from_env();
            let fan = disk_fan_in_from_env();
            let fits_allcold = {
                let r = self.segments.read();
                let any_hot = r.iter().any(|s| s.has_hot_body());
                let cold: Vec<_> = r.iter().filter(|s| !s.has_hot_body()).collect();
                let cold_entries = cold.iter().map(|s| s.entry_count).sum::<usize>();
                !any_hot && checkpoint_allcold_write_ok(cold.len(), cold_entries, fan, max)
            };
            // dest-bc 91s / ENOSPC: stall pairs only when AllCold will rewrite.
            // TeeScan does not write segments, so it does not need the shrink.
            if fits_allcold && max > 0 {
                for _ in 0..128 {
                    let over = self
                        .segments
                        .read()
                        .iter()
                        .filter(|s| !s.has_hot_body() && s.entry_count > max)
                        .count();
                    if over == 0 {
                        break;
                    }
                    self.do_compact_stall_pair_plain()?;
                    let over_after = self
                        .segments
                        .read()
                        .iter()
                        .filter(|s| !s.has_hot_body() && s.entry_count > max)
                        .count();
                    // Pair of old Adds vs new Deletes can tee 0 (all GC'd / leftover
                    // Deletes). Stopping on wrote==0 left dest-bc middle megas.
                    if over_after >= over {
                        break;
                    }
                }
            }
            let fanin_ms = t_fanin.elapsed().as_millis() as u64;
            let (last, preset) = {
                let r = self.segments.read();
                if r.is_empty() {
                    (CompactScope::TeeScan, Some(Vec::new()))
                } else {
                    let any_hot = r.iter().any(|s| s.has_hot_body());
                    let cold: Vec<_> = r.iter().filter(|s| !s.has_hot_body()).collect();
                    let cold_entries = cold.iter().map(|s| s.entry_count).sum::<usize>();
                    if !any_hot && checkpoint_allcold_write_ok(cold.len(), cold_entries, fan, max) {
                        (CompactScope::AllCold, None)
                    } else {
                        // Clone while the compact lock is still held. Background
                        // compact may unlink these files after we release it.
                        (CompactScope::TeeScan, Some(r.iter().cloned().collect()))
                    }
                }
            };
            if preset.as_ref().is_some_and(|v| v.is_empty()) {
                return Ok(0);
            }
            if matches!(last, CompactScope::TeeScan) {
                self.is_compacting.store(false, Ordering::Release);
                still_hold.set(false);
                tracing::info!(
                    "[IBD_CKPT_TEE] compact lock released before tee-scan ckpt={} segs={}",
                    checkpoint_height,
                    preset.as_ref().map(|v| v.len()).unwrap_or(0)
                );
            }
            match on_live {
                Some(cb) => self.do_compact_impl(checkpoint_height, cb, fanin_ms, last, preset),
                None => {
                    if matches!(last, CompactScope::AllCold) {
                        let _ = self.do_compact_impl(-1, |_| Ok(()), fanin_ms, last, None)?;
                    }
                    tracing::info!(
                        "[IBD_COMPACT_S0] ckpt={} fanin_ms={} merge_ms=0 sink_ms=0 \
                         seg_pass1_ms=0 bloom_write_ms=0 directory_ms=0 swap_ms=0 \
                         (no piggyback sink)",
                        checkpoint_height,
                        fanin_ms,
                    );
                    Ok(0)
                }
            }
        })();

        self.last_checkpoint_compact_fence
            .store(fence, Ordering::Release);
        // TeeScan handed the lock to the background compacter. Clearing it
        // here would drop a lock that compact now owns.
        if still_hold.get() {
            self.is_compacting.store(false, Ordering::Release);
        }
        result.map(|tee| (tee, cold_segs_at_start))
    }

    /// Compact oldest segments when compactable count ≥ `fan_in` (default 8). Also
    /// fold dest-bc megas (oldest+last oversized GC) when compactable ≥ stall min (`7` @660k).
    /// Safe to call after releasing any memory-age `is_merging` lock.
    pub fn compact_oldest_if_needed(&self) -> anyhow::Result<()> {
        let fan_in = disk_fan_in_from_env();
        let len = self.compactable_count();
        let oversize = self.has_oversized_cold();
        // HP-M4: 1–3 stay. Mid-band [stall_min, fan_in) is R-268 358k (5 segs).
        if !oversize && len < Self::stall_split_min_compactable(fan_in) {
            return Ok(());
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last = LAST_COMPACT_FINISH.load(Ordering::Relaxed);
        if !oversize
            && len >= Self::stall_split_min_compactable(fan_in)
            && len < fan_in * 2
            && now.saturating_sub(last) < COMPACT_MIN_INTERVAL_SECS
        {
            return Ok(());
        }
        // CAS: only one compaction runs at a time. Skip if another is in progress —
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
        let result = self.run_fanin_and_stall_split();
        self.is_compacting.store(false, Ordering::Release);
        let compact_ms = t0.elapsed().as_millis() as u64;
        if let Ok((passes, stall_alls)) = &result
            && (*passes > 0 || *stall_alls > 0)
        {
            tracing::info!(
                "DiskIndex: compact finished passes={} stall_all={} segs_now={} compact_ms={}",
                passes,
                stall_alls,
                self.segments.read().len(),
                compact_ms,
            );
        }
        result.map(|_| ())
    }

    /// dest-bc 7-mega stall: take-all of 144G dest would ENOSPC (65G free). Fold
    /// oldest oversized + last oversized until no mega remains, then fan-in 20M
    /// chunks. dest-bc 01:31Z FanIn of 8×776M is forbidden while oversized; after
    /// 20M-cap drain, fan-in of at-cap chunks with no GC must not rewrite 32×.
    fn run_fanin_and_stall_split(&self) -> anyhow::Result<(u32, u32)> {
        let fan_in = disk_fan_in_from_env();
        let max = compact_max_entries_from_env();
        let mut passes = 0u32;
        let mut stall_alls = 0u32;
        for _ in 0..128 {
            if self.has_oversized_cold() {
                let over = self
                    .segments
                    .read()
                    .iter()
                    .filter(|s| !s.has_hot_body() && max > 0 && s.entry_count > max)
                    .count();
                self.do_compact_stall_pair_plain()?;
                stall_alls += 1;
                let over_after = self
                    .segments
                    .read()
                    .iter()
                    .filter(|s| !s.has_hot_body() && max > 0 && s.entry_count > max)
                    .count();
                if over_after >= over {
                    break;
                }
                continue;
            }
            if self.compactable_count() >= fan_in {
                if let Some(skip) = self.fanin_photocopy_skip(fan_in, max) {
                    tracing::info!(
                        "[IBD_FANIN_PHOTOCOPY_SKIP] n_in={} entries={} cap={} out_chunks={} \
                         fence={} last_fanin_fence={} min_h={} — count-neutral without GC, not running",
                        skip.n_in,
                        skip.entries,
                        skip.cap,
                        skip.out_chunks,
                        skip.fence,
                        skip.last_fence,
                        skip.min_h,
                    );
                    // R-366: oldest 8 stays on disk. Compact the youngest fan_in
                    // when at least two windows exist and that slice is not also
                    // count-neutral.
                    let cold_n = self.compactable_count();
                    if cold_n >= fan_in.saturating_mul(2) {
                        if let Some(yskip) = self.fanin_photocopy_skip_window(fan_in, max, true) {
                            tracing::info!(
                                "[IBD_FANIN_PHOTOCOPY_SKIP] n_in={} entries={} cap={} out_chunks={} \
                                 fence={} last_fanin_fence={} min_h={} — count-neutral without GC, not running",
                                yskip.n_in,
                                yskip.entries,
                                yskip.cap,
                                yskip.out_chunks,
                                yskip.fence,
                                yskip.last_fence,
                                yskip.min_h,
                            );
                            break;
                        }
                        let (n_in, entries, out_chunks) = self
                            .fanin_window_chunk_stat(fan_in, max, true)
                            .expect("young window exists when cold >= 2*fan_in");
                        tracing::info!(
                            "[IBD_FANIN_YOUNG] n_in={} entries={} out_chunks={} segs_before={}",
                            n_in,
                            entries,
                            out_chunks,
                            cold_n,
                        );
                        let (n0, e0) = self.cold_len_and_entries();
                        self.do_compact_impl(-1, |_| Ok(()), 0, CompactScope::FanInYoung, None)?;
                        self.last_fanin_gc_fence
                            .store(super::memory_run::gc_fence_snapshot(), Ordering::Release);
                        passes += 1;
                        let (n1, e1) = self.cold_len_and_entries();
                        if n1 >= n0 && e1 >= e0 {
                            break;
                        }
                        continue;
                    }
                    break;
                }
                let (n0, e0) = self.cold_len_and_entries();
                self.do_compact_plain()?;
                self.last_fanin_gc_fence
                    .store(super::memory_run::gc_fence_snapshot(), Ordering::Release);
                passes += 1;
                let (n1, e1) = self.cold_len_and_entries();
                // 8×20M with no GC: n and entries unchanged — dest-bc 7.4 must not
                // rewrite that 32 times (32 × 160M × 56B). Pre-check should have
                // skipped; this is the last line of defense.
                if n1 >= n0 && e1 >= e0 {
                    break;
                }
                continue;
            }
            break;
        }
        // R-268 358k: 5 overlapping segs sat below fan_in=8 after export GC + re-spill.
        // dest-bc checkpoint compact had already collapsed to 1–2. HP-M4 1–3 stay.
        // Do not chain after a mega stall-pair drain (middle tinies must remain).
        if stall_alls == 0 {
            let len = self.compactable_count();
            let min = Self::stall_split_min_compactable(fan_in);
            if len >= min && len < fan_in {
                if let Some(skip) = self.fanin_photocopy_skip(len, max) {
                    tracing::info!(
                        "[IBD_FANIN_PHOTOCOPY_SKIP] n_in={} entries={} cap={} out_chunks={} \
                         fence={} last_fanin_fence={} min_h={} — mid-band count-neutral without GC, not running",
                        skip.n_in,
                        skip.entries,
                        skip.cap,
                        skip.out_chunks,
                        skip.fence,
                        skip.last_fence,
                        skip.min_h,
                    );
                } else {
                    tracing::info!(
                        "DiskIndex: mid-band compact n={} (below fan_in={}, dest-bc 358k 2-seg shape)",
                        len,
                        fan_in,
                    );
                    self.do_compact_impl(-1, |_| Ok(()), 0, CompactScope::AllCold, None)?;
                    self.last_fanin_gc_fence
                        .store(super::memory_run::gc_fence_snapshot(), Ordering::Release);
                    passes += 1;
                }
            }
        }
        Ok((passes, stall_alls))
    }

    /// Skip FanIn when output chunk count cannot fall and GC cannot shrink the inputs.
    ///
    /// 400k apply: 142111092 / 20e6 = 7.11 → 8 files, fence stuck at 180000, nine
    /// 50s photocopies. The leftover after a 20M split is ~2.1M, so "every file
    /// near cap" misses that shape. Load-bearing test: `ceil(sum/cap) >= n_in`.
    ///
    /// GC-can't-help: all input `min_height > fence` (journal entirely above the
    /// snapshot export), or fence equals the last fan-in that already ran.
    /// Unlimited cap (`0`) still 8→1 and is never skipped here.
    fn fanin_photocopy_skip(&self, fan_in: usize, cap: usize) -> Option<FaninPhotocopySkip> {
        self.fanin_photocopy_skip_window(fan_in, cap, false)
    }

    /// `young`: last `fan_in` cold segs. Oldest window is `cold[..fan_in]`.
    fn fanin_window_chunk_stat(
        &self,
        fan_in: usize,
        cap: usize,
        young: bool,
    ) -> Option<(usize, usize, usize)> {
        let r = self.segments.read();
        let cold: Vec<&Arc<DiskSegment>> = r.iter().filter(|s| !s.has_hot_body()).collect();
        if cold.len() < fan_in || fan_in == 0 {
            return None;
        }
        let inputs = if young {
            &cold[cold.len() - fan_in..]
        } else {
            &cold[..fan_in]
        };
        let n_in = inputs.len();
        let entries: usize = inputs.iter().map(|s| s.entry_count).sum();
        let out_chunks = if cap == 0 {
            n_in
        } else {
            entries.div_ceil(cap)
        };
        Some((n_in, entries, out_chunks))
    }

    /// Same predicate as [`Self::fanin_photocopy_skip`], on the oldest or youngest slice.
    fn fanin_photocopy_skip_window(
        &self,
        fan_in: usize,
        cap: usize,
        young: bool,
    ) -> Option<FaninPhotocopySkip> {
        if cap == 0 {
            return None;
        }
        let fence = super::memory_run::gc_fence_snapshot();
        let last = self.last_fanin_gc_fence.load(Ordering::Acquire);
        let r = self.segments.read();
        let cold: Vec<&Arc<DiskSegment>> = r.iter().filter(|s| !s.has_hot_body()).collect();
        if cold.len() < fan_in {
            return None;
        }
        let inputs = if young {
            &cold[cold.len() - fan_in..]
        } else {
            &cold[..fan_in]
        };
        let n_in = inputs.len();
        let entries: usize = inputs.iter().map(|s| s.entry_count).sum();
        let min_h = inputs
            .iter()
            .map(|s| s.height_range().0)
            .min()
            .unwrap_or(i32::MIN);
        let out_chunks = entries.div_ceil(cap);
        if out_chunks < n_in {
            return None;
        }
        let fence_stuck = last != i32::MIN && fence == last;
        let journal_above_fence = min_h > fence;
        if !fence_stuck && !journal_above_fence {
            return None;
        }
        Some(FaninPhotocopySkip {
            n_in,
            entries,
            cap,
            out_chunks,
            fence,
            last_fence: last,
            min_h,
        })
    }

    fn do_compact_plain(&self) -> anyhow::Result<u64> {
        // Plain fan-in: noop sink; ckpt=-1 means ExportTee still visits Adds (legacy).
        self.do_compact_impl(-1, |_| Ok(()), 0, CompactScope::FanIn, None)
    }

    /// dest-bc megas: merge oldest oversized + last oversized (GC + 20M cap). Leaves HotPin.
    fn do_compact_stall_pair_plain(&self) -> anyhow::Result<u64> {
        self.do_compact_impl(-1, |_| Ok(()), 0, CompactScope::OldestAndNewest, None)
    }

    fn do_compact_impl<F>(
        &self,
        checkpoint_height: i32,
        mut on_live: F,
        fanin_ms: u64,
        scope: CompactScope,
        preset: Option<Vec<Arc<DiskSegment>>>,
    ) -> anyhow::Result<u64>
    where
        F: FnMut(OutputKV) -> anyhow::Result<()>,
    {
        let tee_only = matches!(scope, CompactScope::TeeScan);
        // `preset` is the checkpoint tee list, cloned while `is_compacting` was held.
        // Do not re-read `self.segments` for that scan: the lock is already released.
        // Otherwise snapshot cold (non-HotPin) segments; pinned seed/newest stay queryable.
        let to_compact: Vec<Arc<DiskSegment>> = if let Some(preset) = preset {
            preset
        } else {
            let r = self.segments.read();
            let fan = disk_fan_in_from_env();
            let cold: Vec<Arc<DiskSegment>> =
                r.iter().filter(|s| !s.has_hot_body()).cloned().collect();
            match scope {
                CompactScope::AllCold => cold,
                // Include HotPin segments. Their UTXOs are not in the memory
                // overlay. stream() reads the file; the pin stays for lookups.
                CompactScope::TeeScan => r.iter().cloned().collect(),
                CompactScope::FanIn | CompactScope::FanInYoung => {
                    if cold.len() < fan {
                        Vec::new()
                    } else if matches!(scope, CompactScope::FanInYoung) {
                        cold[cold.len() - fan..].to_vec()
                    } else {
                        cold[..fan].to_vec()
                    }
                }
                CompactScope::OldestAndNewest => {
                    let max = compact_max_entries_from_env();
                    let counts: Vec<usize> = cold.iter().map(|s| s.entry_count).collect();
                    match stall_pair_indices(&counts, max) {
                        Some((i, j)) if i != j => {
                            vec![cold[i].clone(), cold[j].clone()]
                        }
                        _ => Vec::new(),
                    }
                }
            }
        };
        if to_compact.is_empty() {
            return Ok(0);
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
                if min_e.is_some()
                    && let Err(e) = self.readers[min_i].advance()
                {
                    tracing::warn!("GcMergeIter: advance error: {e:#}");
                    return None;
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
            tee_merged: std::sync::Arc<AtomicU64>,
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
                    self.tee_merged.fetch_add(1, Ordering::Relaxed);
                }
                Some(e)
            }
        }

        // Stream survivors to one or more segment files (dest-bc compact-output cap).
        let sink_err = std::sync::Arc::new(std::sync::Mutex::new(None));
        super::disk_segment::reset_write_from_iter_stats();
        let merge_ns = std::sync::Arc::new(AtomicU64::new(0));
        let sink_ns = std::sync::Arc::new(AtomicU64::new(0));
        let tee_merged = std::sync::Arc::new(AtomicU64::new(0));
        let compact_max = compact_max_entries_from_env();
        let chunk_max = if compact_max == 0 {
            usize::MAX
        } else {
            compact_max
        };
        if tee_only {
            let iter = ExportTee {
                inner: merge_iter,
                ckpt: checkpoint_height,
                on_live,
                sink_err: std::sync::Arc::clone(&sink_err),
                merge_ns: std::sync::Arc::clone(&merge_ns),
                sink_ns: std::sync::Arc::clone(&sink_ns),
                tee_merged: std::sync::Arc::clone(&tee_merged),
            };
            for _ in iter {}
            if let Some(err) = sink_err.lock().ok().and_then(|mut g| g.take()) {
                return Err(err.context(
                    "piggyback export sink failed during dest-bc tee-scan (often MDB_MAP_FULL)",
                ));
            }
            let tee = tee_merged.load(Ordering::Relaxed);
            tracing::info!(
                "[IBD_COMPACT_S0] ckpt={} fanin_ms={} merge_ms={} sink_ms={} \
                 dest-bc tee-scan (no AllCold write, {} entries in {} segs) tee_merged={}",
                checkpoint_height,
                fanin_ms,
                merge_ns.load(Ordering::Relaxed) / 1_000_000,
                sink_ns.load(Ordering::Relaxed) / 1_000_000,
                total_in,
                to_compact.len(),
                tee,
            );
            return Ok(tee);
        }
        note_compact_write_input(total_in);
        let mut new_segs: Vec<Arc<DiskSegment>> = Vec::new();
        {
            let iter = ExportTee {
                inner: merge_iter,
                ckpt: checkpoint_height,
                on_live,
                sink_err: std::sync::Arc::clone(&sink_err),
                merge_ns: std::sync::Arc::clone(&merge_ns),
                sink_ns: std::sync::Arc::clone(&sink_ns),
                tee_merged: std::sync::Arc::clone(&tee_merged),
            };
            let mut peekable = iter.peekable();
            while peekable.peek().is_some() {
                let idx = self.next_idx.fetch_add(1, Ordering::Relaxed);
                let cap = if chunk_max == usize::MAX {
                    total_in.max(1)
                } else {
                    chunk_max
                };
                match DiskSegment::write_from_iter(
                    &self.seg_dir,
                    idx,
                    cap,
                    peekable.by_ref().take(chunk_max),
                ) {
                    Ok(seg) => {
                        if seg.entry_count == 0 {
                            let _ = std::fs::remove_file(&seg.path);
                            break;
                        }
                        new_segs.push(Arc::new(seg));
                    }
                    Err(e) => {
                        for s in &new_segs {
                            let _ = std::fs::remove_file(&s.path);
                        }
                        return Err(e);
                    }
                }
            }
        };
        let merge_ns = merge_ns.load(Ordering::Relaxed);
        let sink_ns = sink_ns.load(Ordering::Relaxed);
        if let Some(err) = sink_err.lock().ok().and_then(|mut g| g.take()) {
            for seg in &new_segs {
                let _ = std::fs::remove_file(&seg.path);
            }
            return Err(err.context(
                "piggyback export sink failed during disk compact (often MDB_MAP_FULL — \
                 grow LMDB map / free freelist before Phase 3)",
            ));
        }

        let total_out: usize = new_segs.iter().map(|s| s.entry_count).sum();
        let (seg_pass1_ms, directory_ms) = super::disk_segment::take_write_from_iter_stats();
        let merge_ms = merge_ns / 1_000_000;
        let sink_ms = sink_ns / 1_000_000;
        let bloom_write_ms = seg_pass1_ms.saturating_sub(merge_ms + sink_ms);

        // Atomically swap old segments for the new merged chunk(s).
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
            for (i, seg) in new_segs.into_iter().enumerate() {
                segs.insert(insert_pos + i, seg);
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
        Ok(tee_merged.load(Ordering::Relaxed))
    }

    /// Batch query all disk segments (oldest-to-newest) for unresolved keys.
    ///
    /// Only called for keys with `ids[i] == OutputId::MAX` after all memory ages.
    /// Keys with `OUTPUT_ID_DELETED` are skipped (already resolved as spent in memory).
    pub fn batch_query(&self, keys: &[[u8; 36]], ids: &mut [OutputId], before: i32) {
        // F5a: attribute pread fan-out for HOTPATH timers (reset every query).
        super::disk_segment::reset_disk_io_stats();
        // Skip segment lookup when all keys are already resolved (none remaining as MAX).
        if ids.contains(&OutputId::MAX) {
            // HP-M3: pending async spills are newer than on-disk segments — query first.
            {
                let pending = self.pending_spills.read();
                for run in pending.iter().rev() {
                    if !ids.contains(&OutputId::MAX) {
                        break;
                    }
                    run.batch_lookup(keys, ids, 0, before);
                }
            }
            let snapshot = Arc::clone(&*super::timed_segments_read(&self.segments));

            // Query newest-to-oldest: last segment first (most recent overflow data).
            for seg in snapshot.iter().rev() {
                if !ids.contains(&OutputId::MAX) {
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
        self.segments.read().iter().map(|s| s.ram_bytes()).sum()
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
#[path = "disk_index_tests.rs"]
mod tests;
