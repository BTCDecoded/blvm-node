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
        if let Ok((passes, stall_alls)) = &result {
            if *passes > 0 || *stall_alls > 0 {
                tracing::info!(
                    "DiskIndex: compact finished passes={} stall_all={} segs_now={} compact_ms={}",
                    passes,
                    stall_alls,
                    self.segments.read().len(),
                    compact_ms,
                );
            }
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
mod tests {
    use super::super::types::{OutputId, OutputKV};
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    fn hot_pin_env_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[serial_test::serial(ibd)]
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

    /// dest-bc after M0+M6 fold: 20M chunks at front, remaining megas. Next pair
    /// must eat a mega, not re-merge chunk 0 with the newest mega.
    #[test]
    fn dest_bc_stall_pair_indices_eats_mega_not_front_chunk() {
        let counts = [4usize, 4, 4, 9, 9, 9, 9, 9];
        assert_eq!(stall_pair_indices(&counts, 4), Some((3, 7)));
    }

    /// dest-bc 144G: old Adds in a tiny front seg, Deletes in the newest mega.
    #[test]
    fn dest_bc_stall_pair_indices_oldest_add_plus_newest_mega() {
        let counts = [1usize, 1, 1, 9];
        assert_eq!(stall_pair_indices(&counts, 4), Some((0, 3)));
    }

    /// dest-bc Deletes live in later megas, not the newest 20M spill. Pair both
    /// megas — `(1, 3)` would merge a mega with a chunk and leave the spend.
    #[test]
    fn dest_bc_stall_pair_indices_last_oversized_not_newest_chunk() {
        let counts = [4usize, 9, 9, 4];
        assert_eq!(stall_pair_indices(&counts, 4), Some((1, 2)));
        let front_mega = [9usize, 4, 4, 4];
        assert_eq!(stall_pair_indices(&front_mega, 4), Some((0, 3)));
    }

    #[test]
    fn dest_bc_checkpoint_allcold_write_ok_dest_ba_not_144g() {
        assert!(checkpoint_allcold_write_ok(1, 9, 8, 4));
        assert!(checkpoint_allcold_write_ok(
            5,
            20_000_000 * 5,
            8,
            20_000_000
        ));
        assert!(checkpoint_allcold_write_ok(9, 18, 8, 20_000_000));
        assert!(
            !checkpoint_allcold_write_ok(18, 72, 8, 4),
            "dest-bc leftover 20M chunks must not AllCold-write 144G"
        );
    }

    #[serial_test::serial(ibd)]
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

    #[serial_test::serial(ibd)]
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
        assert!(
            disk.segments.read()[0].has_hot_body(),
            "seed pinned before compact"
        );
        assert!(
            disk.compactable_count() >= 3,
            "cold={}",
            disk.compactable_count()
        );
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

    #[serial_test::serial(ibd)]
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

    #[serial_test::serial(ibd)]
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

    #[serial_test::serial(ibd)]
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

    #[serial_test::serial(ibd)]
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

    #[serial_test::serial(ibd)]
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

    #[serial_test::serial(ibd)]
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

    #[serial_test::serial(ibd)]
    #[test]
    fn compact_max_entries_default_20m_zero_unlimited() {
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
            assert_eq!(compact_max_entries_from_env(), COMPACT_MAX_ENTRIES_DEFAULT);
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "");
            assert_eq!(compact_max_entries_from_env(), COMPACT_MAX_ENTRIES_DEFAULT);
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "0");
            assert_eq!(compact_max_entries_from_env(), 0);
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
            assert_eq!(compact_max_entries_from_env(), 4);
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
    }

    /// dest-bc 650k: fan-in packed 8 segs into one mega. Cap must split compact output
    /// (not HP-M4 spill-split) and still resolve every key.
    #[serial_test::serial(ibd)]
    #[test]
    fn dest_bc_compact_max_splits_fanin_mega_and_resolves() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::set_var("BLVM_IBD_DISK_FAN_IN", "3");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |i: u8| {
            let mut k = [0u8; 36];
            k[0] = i;
            k
        };
        // 3 cold segs × 3 Adds = 9 live entries. fan_in=3 compact → 4+4+1 under cap=4.
        for s in 0u8..3 {
            let entries: Vec<OutputKV> = (0u8..3)
                .map(|j| {
                    let i = s * 3 + j + 1;
                    OutputKV::new_add(mk(i), 10 + i32::from(i), 1000 + u64::from(i))
                })
                .collect();
            disk.push_run_no_compact(MemoryRun::build(entries))
                .expect("spill");
        }
        assert_eq!(disk.segment_count(), 3);
        disk.do_compact_plain().expect("compact");
        assert_eq!(
            disk.segment_count(),
            3,
            "9 live entries @ compact_max=4 → 3 chunks (dest-bc mega split)"
        );
        let keys: Vec<_> = (1u8..=9).map(mk).collect();
        let mut ids = vec![OutputId::MAX; 9];
        disk.batch_query(&keys, &mut ids, 100);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(
                *id,
                1001 + i as u64,
                "miss after compact split at {}",
                i + 1
            );
        }

        unsafe {
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "0");
        }
        for s in 0u8..3 {
            let i = 20 + s;
            disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
                mk(i),
                50,
                2000 + u64::from(i),
            )]))
            .expect("spill2");
        }
        disk.do_compact_plain().expect("unlimited compact");
        let n = disk.segment_count();
        assert!(
            n <= 4,
            "unlimited compact must not keep dest-bc-style 3-way split, segs={n}"
        );

        unsafe {
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }

    /// dest-bc 660k sat at 7 megas — `fan_in=8` never ran. Oversized cold at
    /// compactable ≥4 folds oldest+newest (20M cap), not wait for 8 segs.
    #[serial_test::serial(ibd)]
    #[test]
    fn dest_bc_fanin_stall_compacts_all_cold_below_fan_in() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN"); // default 8 → stall min 4
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |i: u8| {
            let mut k = [0u8; 36];
            k[0] = i;
            k
        };
        // dest-bc stall: ≥4 cold segs, one mega. 3 tiny + 9-entry mega.
        for t in 0u8..3 {
            disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
                mk(100 + t),
                5,
                50 + u64::from(t),
            )]))
            .expect("tiny");
        }
        let entries: Vec<OutputKV> = (1u8..=9)
            .map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i)))
            .collect();
        disk.push_run_no_compact(MemoryRun::build(entries))
            .expect("one mega");
        assert_eq!(
            disk.segment_count(),
            4,
            "below fan_in=8 — dest-bc stall shape"
        );
        disk.compact_oldest_if_needed()
            .expect("stall pair compact oversized below fan_in");
        let n = disk.segment_count();
        assert_eq!(
            n, 5,
            "oldest tiny+mega @ cap=4 → 3 chunks + 2 leftover tinies (not take-all to 3)"
        );
        let keys: Vec<_> = (1u8..=9).chain(100u8..=102).map(mk).collect();
        let mut ids = vec![OutputId::MAX; keys.len()];
        disk.batch_query(&keys, &mut ids, 100);
        for i in 0..9 {
            assert_eq!(
                ids[i],
                1001 + i as u64,
                "miss after stall pair at {}",
                i + 1
            );
        }
        for i in 0..3 {
            assert_eq!(ids[9 + i], 50 + i as u64, "tiny miss at {}", i);
        }

        unsafe {
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "0");
        }
        let tmp2 = tempfile::tempdir().expect("tempdir2");
        let (disk2, _) = DiskIndex::new_empty(tmp2.path()).expect("DiskIndex");
        let disk2 = Arc::new(disk2);
        for t in 0u8..3 {
            disk2
                .push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
                    mk(100 + t),
                    5,
                    50 + u64::from(t),
                )]))
                .expect("tiny2");
        }
        disk2
            .push_run_no_compact(MemoryRun::build(
                (1u8..=9)
                    .map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i)))
                    .collect(),
            ))
            .expect("unlimited mega");
        disk2.compact_oldest_if_needed().expect("no stall at 0");
        assert_eq!(
            disk2.segment_count(),
            4,
            "COMPACT_MAX=0 must leave dest-bc mega (no stall pair)"
        );

        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
        std::mem::forget(tmp2);
    }

    /// HP-M4 REVERT: a lone mega spill at 1–3 segs must not size-split (300–400k tip).
    #[serial_test::serial(ibd)]
    #[test]
    fn dest_bc_lone_mega_spill_must_not_hp_m4_split() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |i: u8| {
            let mut k = [0u8; 36];
            k[0] = i;
            k
        };
        disk.push_run_no_compact(MemoryRun::build(
            (1u8..=9)
                .map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i)))
                .collect(),
        ))
        .expect("lone mega");
        disk.compact_oldest_if_needed().expect("must no-op");
        assert_eq!(
            disk.segment_count(),
            1,
            "1 cold mega < stall min 4 must not HP-M4 split"
        );
        for t in 0u8..2 {
            disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
                mk(50 + t),
                5,
                10 + u64::from(t),
            )]))
            .expect("tiny");
        }
        assert_eq!(disk.segment_count(), 3);
        disk.compact_oldest_if_needed().expect("still no-op at 3");
        assert_eq!(
            disk.segment_count(),
            3,
            "3 cold segs is HP-M4 band, not dest-bc 5–7 stall"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }

    /// dest-bc 144G vs dest-ba 67G: spends of old Adds live in later megas.
    /// Stall folds oldest+newest (not take-all of every mega — peak extra is two segs).
    #[serial_test::serial(ibd)]
    #[test]
    fn dest_bc_stall_take_all_gcs_spend_in_later_mega() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        super::super::set_gc_fence(100);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |i: u8| {
            let mut k = [0u8; 36];
            k[0] = i;
            k
        };
        disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(mk(1), 10, 1000)]))
            .expect("old add");
        for t in 0u8..2 {
            disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
                mk(100 + t),
                5,
                50 + u64::from(t),
            )]))
            .expect("tiny");
        }
        let mut later: Vec<OutputKV> = vec![OutputKV::new_delete(mk(1), 50)];
        later.extend((2u8..=9).map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i))));
        disk.push_run_no_compact(MemoryRun::build(later))
            .expect("later mega with spend");
        assert_eq!(disk.segment_count(), 4);
        let mut pre = [OutputId::MAX];
        disk.batch_query(&[mk(1)], &mut pre, 40);
        assert_eq!(
            pre[0], 1000,
            "Add must resolve before stall pair (before Delete height)"
        );
        disk.compact_oldest_if_needed().expect("stall pair GC");
        let mut post = [OutputId::MAX];
        disk.batch_query(&[mk(1)], &mut post, 40);
        assert_eq!(
            post[0],
            OutputId::MAX,
            "Delete in later mega must GC the dest-bc old Add"
        );
        let keys: Vec<_> = (2u8..=9).chain(100u8..=101).map(mk).collect();
        let mut ids = vec![OutputId::MAX; keys.len()];
        disk.batch_query(&keys, &mut ids, 100);
        for i in 0..8 {
            assert_eq!(ids[i], 1002 + i as u64, "live miss at {}", i + 2);
        }
        assert_eq!(ids[8], 50);
        assert_eq!(ids[9], 51);
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        super::super::set_gc_fence(i32::MAX);
        std::mem::forget(tmp);
    }

    /// dest-bc 144G: Delete sits in a later mega; newest is a 20M-shaped chunk.
    /// Pairing mega+chunk would miss the spend. Last-oversized pair must GC.
    #[serial_test::serial(ibd)]
    #[test]
    fn dest_bc_stall_pair_gcs_spend_in_later_mega_not_newest_chunk() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        super::super::set_gc_fence(100);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |i: u8| {
            let mut k = [0u8; 36];
            k[0] = i;
            k
        };
        disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(mk(200), 5, 200)]))
            .expect("tiny front");
        let mega_add: Vec<OutputKV> = std::iter::once(OutputKV::new_add(mk(1), 10, 1000))
            .chain((2u8..=9).map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i))))
            .collect();
        disk.push_run_no_compact(MemoryRun::build(mega_add))
            .expect("mega with old Add");
        let mega_del: Vec<OutputKV> = std::iter::once(OutputKV::new_delete(mk(1), 50))
            .chain((10u8..=17).map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i))))
            .collect();
        disk.push_run_no_compact(MemoryRun::build(mega_del))
            .expect("later mega with spend");
        disk.push_run_no_compact(MemoryRun::build(
            (80u8..=82)
                .map(|i| OutputKV::new_add(mk(i), 90, 80 + u64::from(i)))
                .collect(),
        ))
        .expect("newest 20M-shaped chunk");
        assert_eq!(disk.segment_count(), 4);
        let mut pre = [OutputId::MAX];
        disk.batch_query(&[mk(1)], &mut pre, 40);
        assert_eq!(pre[0], 1000, "Add must resolve before pair");
        disk.compact_oldest_if_needed()
            .expect("stall pair last-oversized");
        let mut post = [OutputId::MAX];
        disk.batch_query(&[mk(1)], &mut post, 40);
        assert_eq!(
            post[0],
            OutputId::MAX,
            "Delete in later mega must GC even when newest is a chunk"
        );
        let keys: Vec<_> = (2u8..=17)
            .chain(80u8..=82)
            .chain(std::iter::once(200u8))
            .map(mk)
            .collect();
        let mut ids = vec![OutputId::MAX; keys.len()];
        disk.batch_query(&keys, &mut ids, 100);
        for i in 0..16 {
            assert_eq!(ids[i], 1002 + i as u64, "live miss at {}", i + 2);
        }
        assert_eq!(ids[16], 160);
        assert_eq!(ids[17], 161);
        assert_eq!(ids[18], 162);
        assert_eq!(ids[19], 200);
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        super::super::set_gc_fence(i32::MAX);
        std::mem::forget(tmp);
    }

    /// Live IBD calls `compact_oldest_async` after spill, not `compact_oldest_if_needed`.
    #[serial_test::serial(ibd)]
    #[test]
    fn dest_bc_compact_oldest_async_stall_compacts() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |i: u8| {
            let mut k = [0u8; 36];
            k[0] = i;
            k
        };
        for t in 0u8..3 {
            disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
                mk(100 + t),
                5,
                50 + u64::from(t),
            )]))
            .expect("tiny");
        }
        disk.push_run_no_compact(MemoryRun::build(
            (1u8..=9)
                .map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i)))
                .collect(),
        ))
        .expect("mega");
        assert_eq!(disk.segment_count(), 4);
        disk.compact_oldest_async();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if disk.segment_count() == 5 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            disk.segment_count(),
            5,
            "async compact must stall-pair dest-bc 4-seg mega (WAN path)"
        );
        let keys: Vec<_> = (1u8..=9).map(mk).collect();
        let mut ids = vec![OutputId::MAX; 9];
        disk.batch_query(&keys, &mut ids, 100);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(*id, 1001 + i as u64, "async miss at {}", i + 1);
        }
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }

    #[serial_test::serial(ibd)]
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

    /// dest-bc 144G take-all would ENOSPC (65G free). 5 tinies + 1 mega: pair
    /// oldest+newest, leave the middle tinies. Take-all would merge all 6.
    #[serial_test::serial(ibd)]
    #[test]
    fn dest_bc_stall_pair_leaves_middle_segs() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |i: u8| {
            let mut k = [0u8; 36];
            k[0] = i;
            k
        };
        for t in 0u8..5 {
            disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
                mk(100 + t),
                5,
                50 + u64::from(t),
            )]))
            .expect("tiny");
        }
        disk.push_run_no_compact(MemoryRun::build(
            (1u8..=9)
                .map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i)))
                .collect(),
        ))
        .expect("mega");
        assert_eq!(disk.segment_count(), 6);
        disk.compact_oldest_if_needed().expect("stall pair");
        let n = disk.segment_count();
        assert!(
            n >= 6,
            "take-all of 6 segs → 4 chunks; pair must leave middle tinies, segs={n}"
        );
        let keys: Vec<_> = (1u8..=9).chain(100u8..=104).map(mk).collect();
        let mut ids = vec![OutputId::MAX; keys.len()];
        disk.batch_query(&keys, &mut ids, 100);
        for i in 0..9 {
            assert_eq!(ids[i], 1001 + i as u64);
        }
        for i in 0..5 {
            assert_eq!(ids[9 + i], 50 + i as u64, "middle tiny {} dropped", i);
        }
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }

    /// dest-bc 7 megas: loop must drain every >cap file (not stall with leftover megas).
    #[serial_test::serial(ibd)]
    #[test]
    fn dest_bc_four_megas_drain_to_compact_cap() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |i: u8| {
            let mut k = [0u8; 36];
            k[0] = i;
            k
        };
        let mut expect: Vec<([u8; 36], u64)> = Vec::new();
        for s in 0u8..4 {
            let entries: Vec<OutputKV> = (1u8..=9)
                .map(|j| {
                    let i = s * 10 + j;
                    expect.push((mk(i), 2000 + u64::from(i)));
                    OutputKV::new_add(mk(i), 10, 2000 + u64::from(i))
                })
                .collect();
            disk.push_run_no_compact(MemoryRun::build(entries))
                .expect("mega");
        }
        assert_eq!(disk.segment_count(), 4);
        disk.compact_oldest_if_needed().expect("drain megas");
        for s in disk.segments.read().iter() {
            assert!(
                s.entry_count <= 4,
                "leftover dest-bc mega {} entries",
                s.entry_count
            );
        }
        let keys: Vec<_> = expect.iter().map(|(k, _)| *k).collect();
        let mut ids = vec![OutputId::MAX; keys.len()];
        disk.batch_query(&keys, &mut ids, 100);
        for (i, (_, id)) in expect.iter().enumerate() {
            assert_eq!(ids[i], *id, "miss after mega drain at {i}");
        }
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }

    /// dest-bc 01:31Z FanIn 8×776M; 20M cap turns that into at-cap chunks. Fan-in
    /// of 8×cap with no GC must not rewrite 32× (32 × 160M × 56B during ts 7.4).
    #[serial_test::serial(ibd)]
    #[test]
    fn dest_bc_fanin_at_cap_must_not_rewrite_loop() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
        LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |i: u8| {
            let mut k = [0u8; 36];
            k[0] = i;
            k
        };
        for s in 0u8..8 {
            let entries: Vec<OutputKV> = (0u8..4)
                .map(|j| {
                    let i = s * 4 + j + 1;
                    OutputKV::new_add(mk(i), 10, 3000 + u64::from(i))
                })
                .collect();
            disk.push_run_no_compact(MemoryRun::build(entries))
                .expect("at-cap chunk");
        }
        assert_eq!(disk.segment_count(), 8);
        disk.compact_oldest_if_needed().expect("one fan-in");
        let calls = TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed);
        assert!(
            calls <= 2,
            "at-cap fan-in with no GC must not loop 32 rewrites, calls={calls}"
        );
        assert_eq!(disk.segment_count(), 8, "8×4 @ cap=4 stays 8 segs");
        let keys: Vec<_> = (1u8..=32).map(mk).collect();
        let mut ids = vec![OutputId::MAX; 32];
        disk.batch_query(&keys, &mut ids, 100);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(*id, 3001 + i as u64, "miss after no-op fan-in at {}", i + 1);
        }
        TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
        disk.compact_oldest_if_needed()
            .expect("second kick skipped");
        assert_eq!(
            TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed),
            0,
            "fence unchanged after first at-cap pass must not rewrite again"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }

    fn spill_n_adds_at(disk: &Arc<DiskIndex>, start: u8, n: u8, h: i32, id0: u64) {
        let mk = |b: u8| {
            let mut k = [0u8; 36];
            k[0] = b;
            k
        };
        let entries: Vec<OutputKV> = (0..n)
            .map(|j| OutputKV::new_add(mk(start + j), h, id0 + u64::from(j)))
            .collect();
        disk.push_run_no_compact(MemoryRun::build(entries))
            .expect("spill");
    }

    /// R-268 358k: 5 overlapping segs sat below fan_in=8. dest-bc was 2.
    /// Mid-band compact must collapse 5 tiny segs (GC can shrink / count can fall).
    #[serial_test::serial(ibd)]
    #[test]
    fn r268_five_segs_below_fan_in_compact_to_dest_bc_shape() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        super::super::set_gc_fence(i32::MAX);
        LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
        TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        for s in 0u8..5 {
            spill_n_adds_at(&disk, s * 3 + 1, 3, 10, 6000 + u64::from(s) * 3);
        }
        assert_eq!(disk.segment_count(), 5);
        disk.compact_oldest_if_needed().expect("mid-band compact");
        assert!(
            disk.segment_count() <= 2,
            "5 segs below fan_in=8 must collapse toward dest-bc 358k (≤2), segs={}",
            disk.segment_count()
        );
        let mk = |b: u8| {
            let mut k = [0u8; 36];
            k[0] = b;
            k
        };
        let keys: Vec<_> = (1u8..=15).map(mk).collect();
        let mut ids = vec![OutputId::MAX; keys.len()];
        disk.batch_query(&keys, &mut ids, 100);
        for (i, id) in ids.iter().enumerate() {
            assert_ne!(
                *id,
                OutputId::MAX,
                "key {} dropped by mid-band compact",
                i + 1
            );
        }
        super::super::set_gc_fence(i32::MAX);
        std::mem::forget(tmp);
    }

    /// HP-M4: 1–3 segs must not fold (stall_split_min = 4 @ fan_in=8).
    #[serial_test::serial(ibd)]
    #[test]
    fn hp_m4_three_segs_do_not_midband_fold() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        for s in 0u8..3 {
            spill_n_adds_at(&disk, s * 3 + 1, 3, 10, 7000 + u64::from(s) * 3);
        }
        assert_eq!(disk.segment_count(), 3);
        disk.compact_oldest_if_needed().expect("no mid-band at 3");
        assert_eq!(disk.segment_count(), 3, "HP-M4 1–3 stay");
        std::mem::forget(tmp);
    }

    /// Matching-height 358k: candidates scale with live overlapping segs, not prefix bits.
    /// Keys live in every seg at h=100; query `before=50` so they stay unresolved and
    /// every seg still emits directory candidates (dest-bc 2 segs / 245 cands vs R-268 5 / 670).
    #[serial_test::serial(ibd)]
    #[test]
    fn matching_height_358k_candidate_fanout() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        fn key_at(i: u16) -> [u8; 36] {
            let mut k = [0u8; 36];
            k[0] = (i >> 8) as u8;
            k[1] = i as u8;
            k
        }
        fn measure(n_segs: usize) -> (u64, u64, u64) {
            let tmp = tempfile::tempdir().expect("tempdir");
            let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
            let disk = Arc::new(disk);
            let n_keys = 64u16;
            for s in 0..n_segs {
                let mut entries: Vec<OutputKV> = (0..n_keys)
                    .map(|i| OutputKV::new_add(key_at(i), 100, 10_000 + u64::from(i)))
                    .collect();
                // Height window overlap: filler below `before` so the seg is scanned.
                entries.push(OutputKV::new_add(key_at(1000 + s as u16), 10, 1));
                entries.sort_unstable();
                disk.push_run_no_compact(MemoryRun::build(entries))
                    .expect("spill");
            }
            assert_eq!(disk.segment_count(), n_segs);
            let keys: Vec<_> = (0..n_keys).map(key_at).collect();
            let mut ids = vec![OutputId::MAX; keys.len()];
            super::super::disk_segment::reset_disk_io_stats();
            disk.batch_query(&keys, &mut ids, 50);
            for id in &ids {
                assert_eq!(
                    *id,
                    OutputId::MAX,
                    "h=100 must stay unresolved at before=50"
                );
            }
            let (_preads, pread_kb, _max, cands, segs) =
                super::super::disk_segment::take_disk_io_stats();
            std::mem::forget(tmp);
            (cands, segs, pread_kb)
        }
        let (c2, s2, kb2) = measure(2);
        let (c5, s5, kb5) = measure(5);
        assert_eq!(s2, 2, "dest-bc 358k segs");
        assert_eq!(s5, 5, "R-268 358k segs");
        assert_eq!(c2, 64 * 2, "cands scale with segs (2)");
        assert_eq!(c5, 64 * 5, "cands scale with segs (5)");
        assert!(
            c5 as f64 / c2 as f64 > 2.0,
            "5-seg fan-out must dominate 2-seg, c2={c2} c5={c5} kb2={kb2} kb5={kb5}"
        );
    }

    /// 400k apply: fence stuck below journal min_height. First at-cap FanIn must
    /// not run (the photocopier). Writes stay 0.
    #[serial_test::serial(ibd)]
    #[test]
    fn fanin_photocopy_skip_when_journal_above_stuck_fence() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        super::super::set_gc_fence(5);
        TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
        LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        for s in 0u8..8 {
            spill_n_adds_at(&disk, s * 4 + 1, 4, 10, 3000 + u64::from(s) * 4);
        }
        assert_eq!(disk.segment_count(), 8);
        disk.compact_oldest_if_needed().expect("skip photocopy");
        assert_eq!(
            TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed),
            0,
            "at-cap + min_h>fence must not FanIn"
        );
        assert_eq!(disk.segment_count(), 8);
        super::super::set_gc_fence(i32::MAX);
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }

    /// 7×20M + 2.1M remainder: ceil(sum/cap) >= n even though one file is small.
    #[serial_test::serial(ibd)]
    #[test]
    fn fanin_photocopy_skip_remainder_shape() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        super::super::set_gc_fence(5);
        TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
        LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        for s in 0u8..7 {
            spill_n_adds_at(&disk, s * 4 + 1, 4, 10, 4000 + u64::from(s) * 4);
        }
        spill_n_adds_at(&disk, 29, 1, 10, 4028);
        assert_eq!(disk.segment_count(), 8);
        disk.compact_oldest_if_needed()
            .expect("skip remainder photocopy");
        assert_eq!(
            TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed),
            0,
            "7×cap + leftover still 8→8 without GC"
        );
        super::super::set_gc_fence(i32::MAX);
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }

    /// R-366: oldest 8 at the cap is a photocopy. Young window of smaller segs
    /// merges; those oldest entry counts stay. A young window that is also
    /// count-neutral still skips.
    #[serial_test::serial(ibd)]
    #[test]
    fn r366_young_fanin_when_oldest_is_photocopy_skip() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        super::super::set_gc_fence(5);
        TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
        LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        for s in 0u8..8 {
            spill_n_adds_at(&disk, s * 4 + 1, 4, 10, 8000 + u64::from(s) * 4);
        }
        for s in 0u8..8 {
            spill_n_adds_at(&disk, 80 + s, 1, 30, 9000 + u64::from(s));
        }
        assert_eq!(disk.segment_count(), 16);
        let (oldest_n, oldest_ptr) = {
            let r = disk.segments.read();
            let n: Vec<usize> = r.iter().take(8).map(|s| s.entry_count).collect();
            let ptr: Vec<usize> = r.iter().take(8).map(|s| Arc::as_ptr(s) as usize).collect();
            (n, ptr)
        };
        assert_eq!(oldest_n, vec![4; 8]);
        disk.compact_oldest_if_needed().expect("young fan-in");
        assert!(
            TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed) >= 1,
            "young window below cap must compact"
        );
        assert!(
            disk.segment_count() < 16,
            "young merge must drop segment count, got {}",
            disk.segment_count()
        );
        {
            let r = disk.segments.read();
            let n: Vec<usize> = r.iter().take(8).map(|s| s.entry_count).collect();
            let ptr: Vec<usize> = r.iter().take(8).map(|s| Arc::as_ptr(s) as usize).collect();
            assert_eq!(n, oldest_n, "oldest 8 entry counts must stay");
            assert_eq!(ptr, oldest_ptr, "oldest 8 segments must not be rewritten");
        }
        std::mem::forget(tmp);

        TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
        LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
        let tmp2 = tempfile::tempdir().expect("tempdir");
        let (disk2, _) = DiskIndex::new_empty(tmp2.path()).expect("DiskIndex");
        let disk2 = Arc::new(disk2);
        for s in 0u8..16 {
            spill_n_adds_at(&disk2, s * 4 + 1, 4, 10, 10000 + u64::from(s) * 4);
        }
        assert_eq!(disk2.segment_count(), 16);
        disk2
            .compact_oldest_if_needed()
            .expect("both windows count-neutral");
        assert_eq!(
            TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed),
            0,
            "young window at cap must skip"
        );
        assert_eq!(disk2.segment_count(), 16);
        super::super::set_gc_fence(i32::MAX);
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        std::mem::forget(tmp2);
    }

    /// Compact 1/2 shape: 8×3 @ cap=4 → 6 files, count falls, must still run.
    #[serial_test::serial(ibd)]
    #[test]
    fn fanin_below_cap_still_runs_with_stuck_fence() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        super::super::set_gc_fence(5);
        TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
        LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        for s in 0u8..8 {
            spill_n_adds_at(&disk, s * 3 + 1, 3, 10, 5000 + u64::from(s) * 3);
        }
        disk.compact_oldest_if_needed().expect("below-cap fan-in");
        assert!(
            TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed) >= 1,
            "8×3 @ cap=4 must still merge (6 out)"
        );
        assert!(
            disk.segment_count() < 8,
            "below-cap fan-in must reduce count, got {}",
            disk.segment_count()
        );
        super::super::set_gc_fence(i32::MAX);
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }

    /// Fence advance into the journal: min_h <= fence, last is MIN → FanIn may run.
    #[serial_test::serial(ibd)]
    #[test]
    fn fanin_runs_after_fence_advances_into_journal() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        super::super::set_gc_fence(5);
        TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
        LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        for s in 0u8..8 {
            spill_n_adds_at(&disk, s * 4 + 1, 4, 10, 6000 + u64::from(s) * 4);
        }
        disk.compact_oldest_if_needed().expect("skip");
        assert_eq!(TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed), 0);
        super::super::set_gc_fence(20);
        disk.compact_oldest_if_needed()
            .expect("fan-in after fence move");
        assert!(
            TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed) >= 1,
            "fence into journal must allow FanIn"
        );
        super::super::set_gc_fence(i32::MAX);
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        std::mem::forget(tmp);
    }

    fn spill_two_adds(disk: &Arc<DiskIndex>, b0: u8, h: i32, id0: u64) {
        let mk = |b: u8| {
            let mut k = [0u8; 36];
            k[0] = b;
            k
        };
        disk.push_run_no_compact(MemoryRun::build(vec![
            OutputKV::new_add(mk(b0), h, id0),
            OutputKV::new_add(mk(b0 + 1), h, id0 + 1),
        ]))
        .expect("spill");
    }

    /// dest-bc: last compact pass no-op'd when cold < fan_in=8. 1-seg must 1→1 tee.
    #[serial_test::serial(ibd)]
    #[test]
    fn checkpoint_sink_tees_one_seg_below_fan_in() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::set_var("BLVM_IBD_DISK_FAN_IN", "8");
        }
        super::super::set_gc_fence(100);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        spill_two_adds(&disk, 1, 10, 100);
        assert_eq!(disk.segment_count(), 1);
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen_cb = std::sync::Arc::clone(&seen);
        let (tee, segs) = disk
            .compact_for_checkpoint_sync_with_sink(
                50,
                Some(move |kv: OutputKV| {
                    if kv.is_add() {
                        seen_cb.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(())
                }),
            )
            .expect("sink");
        assert_eq!(segs, 1, "one cold seg at start");
        assert_eq!(tee, 2, "1→1 tee must visit both Adds");
        assert_eq!(seen.load(Ordering::Relaxed), 2);
        unsafe {
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        }
        super::super::set_gc_fence(i32::MAX);
        std::mem::forget(tmp);
    }

    /// dest-bc leftover after a good 8-seg export: 2 cold segs < fan_in=8 must still tee.
    #[serial_test::serial(ibd)]
    #[test]
    fn checkpoint_sink_tees_two_segs_below_fan_in() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::set_var("BLVM_IBD_DISK_FAN_IN", "8");
        }
        super::super::set_gc_fence(100);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        spill_two_adds(&disk, 1, 10, 100);
        spill_two_adds(&disk, 10, 20, 200);
        assert_eq!(disk.segment_count(), 2);
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen_cb = std::sync::Arc::clone(&seen);
        let (tee, segs) = disk
            .compact_for_checkpoint_sync_with_sink(
                50,
                Some(move |kv: OutputKV| {
                    if kv.is_add() {
                        seen_cb.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(())
                }),
            )
            .expect("sink");
        assert_eq!(segs, 2);
        assert_eq!(tee, 4, "2-seg last pass must merge and tee all Adds");
        assert_eq!(seen.load(Ordering::Relaxed), 4);
        unsafe {
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        }
        super::super::set_gc_fence(i32::MAX);
        std::mem::forget(tmp);
    }

    /// dest-bc leftover after a good fan-in: 9 cold segs → 1 plain 8-way merge + 2 left.
    /// Last pass must take_all_cold (not wait for fan_in=8) or persist is overlay-only.
    #[serial_test::serial(ibd)]
    #[test]
    fn checkpoint_sink_tees_nine_segs_leftover_after_fan_in() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::set_var("BLVM_IBD_DISK_FAN_IN", "8");
        }
        super::super::set_gc_fence(100);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        for i in 0..9u8 {
            spill_two_adds(
                &disk,
                1 + i * 10,
                10 + i32::from(i) * 10,
                100 + u64::from(i) * 100,
            );
        }
        assert_eq!(disk.segment_count(), 9);
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen_cb = std::sync::Arc::clone(&seen);
        let (tee, segs) = disk
            .compact_for_checkpoint_sync_with_sink(
                200,
                Some(move |kv: OutputKV| {
                    if kv.is_add() {
                        seen_cb.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(())
                }),
            )
            .expect("sink");
        assert_eq!(segs, 9, "nine cold segs at start (8 fan-in + leftover)");
        assert_eq!(tee, 18, "leftover after fan-in must still tee every Add");
        assert_eq!(seen.load(Ordering::Relaxed), 18);
        unsafe {
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        }
        super::super::set_gc_fence(i32::MAX);
        std::mem::forget(tmp);
    }

    /// dest-bc 8×300M AllCold/FanIn would ENOSPC (144G extra). Checkpoint must
    /// pair-drain (peak two megas) then tee-scan leftover chunks.
    #[serial_test::serial(ibd)]
    #[test]
    fn dest_bc_checkpoint_must_not_allcold_eight_megas() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        super::super::set_gc_fence(100);
        TEST_MAX_COMPACT_WRITE_INPUT.store(0, Ordering::Relaxed);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |i: u8| {
            let mut k = [0u8; 36];
            k[0] = i;
            k
        };
        let mut expect = 0usize;
        for s in 0u8..8 {
            let entries: Vec<OutputKV> = (1u8..=9)
                .map(|j| {
                    expect += 1;
                    OutputKV::new_add(mk(s * 10 + j), 10, 2000 + u64::from(s * 10 + j))
                })
                .collect();
            disk.push_run_no_compact(MemoryRun::build(entries))
                .expect("mega");
        }
        assert_eq!(disk.segment_count(), 8);
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen_cb = std::sync::Arc::clone(&seen);
        let (tee, segs) = disk
            .compact_for_checkpoint_sync_with_sink(
                50,
                Some(move |kv: OutputKV| {
                    if kv.is_add() {
                        seen_cb.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(())
                }),
            )
            .expect("sink");
        assert_eq!(segs, 8);
        assert_eq!(tee, expect as u64, "tee-scan must visit every dest-bc Add");
        assert_eq!(seen.load(Ordering::Relaxed), expect);
        let peak = TEST_MAX_COMPACT_WRITE_INPUT.load(Ordering::Relaxed);
        assert!(
            peak <= 18,
            "checkpoint write must be a stall pair (2×9), not 8-way/AllCold 72, peak={peak}"
        );
        for s in disk.segments.read().iter() {
            assert!(
                s.entry_count <= 4,
                "pair drain must split dest-bc megas, leftover {}",
                s.entry_count
            );
        }
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        super::super::set_gc_fence(i32::MAX);
        std::mem::forget(tmp);
    }

    /// dest-bc pair of old Adds vs newest Deletes can tee 0. Drain must keep
    /// eating middle megas (`wrote==0` used to stop).
    #[serial_test::serial(ibd)]
    #[test]
    fn dest_bc_checkpoint_drain_continues_after_pair_gcs_all_adds() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
            std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        }
        super::super::set_gc_fence(100);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |i: u8| {
            let mut k = [0u8; 36];
            k[0] = i;
            k
        };
        disk.push_run_no_compact(MemoryRun::build(
            (1u8..=9)
                .map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i)))
                .collect(),
        ))
        .expect("old adds mega");
        for s in 1u8..=2 {
            disk.push_run_no_compact(MemoryRun::build(
                (1u8..=9)
                    .map(|j| {
                        let i = s * 10 + j;
                        OutputKV::new_add(mk(i), 10, 2000 + u64::from(i))
                    })
                    .collect(),
            ))
            .expect("middle mega");
        }
        disk.push_run_no_compact(MemoryRun::build(
            (1u8..=9).map(|i| OutputKV::new_delete(mk(i), 50)).collect(),
        ))
        .expect("newest deletes mega");
        assert_eq!(disk.segment_count(), 4);
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen_cb = std::sync::Arc::clone(&seen);
        let (tee, _) = disk
            .compact_for_checkpoint_sync_with_sink(
                50,
                Some(move |kv: OutputKV| {
                    if kv.is_add() {
                        seen_cb.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(())
                }),
            )
            .expect("sink");
        assert_eq!(tee, 18, "spent keys 1-9 GC; middle 18 Adds must still tee");
        assert_eq!(seen.load(Ordering::Relaxed), 18);
        for s in disk.segments.read().iter() {
            assert!(
                s.entry_count <= 4,
                "middle dest-bc megas must drain after a 0-tee pair, leftover {}",
                s.entry_count
            );
        }
        let mut spent = [OutputId::MAX];
        disk.batch_query(&[mk(1)], &mut spent, 40);
        assert_eq!(spent[0], OutputId::MAX, "GCed Add must stay spent");
        unsafe {
            std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        }
        super::super::set_gc_fence(i32::MAX);
        std::mem::forget(tmp);
    }

    /// dest-ba 650k shape: `n_segs` × 20M (or `BLVM_IBD_SYNTH_SEG_ENTRIES`) on disk,
    /// then `batch_query` dest-ba cand counts. This is the KEEP **measurement**, not a
    /// stall-pair unit. `#[ignore]` — ~9 G artifact, not CI.
    ///
    /// Gauge: dest-ba 650k `preads=240` / `disk_ms=8` at 244 cands; dest-ba 660k
    /// `preads=2138` / `disk_ms=33` at 2219 cands. Falsify if preads ≫ cands after
    /// blooms (union FPR from extra segs) or KiB/pread ≫ dest-ba ~33 at 20M.
    #[serial_test::serial(ibd)]
    #[ignore]
    #[test]
    fn dest_ba_shape_synthetic_pread_budget() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_SPILL_MAX_ENTRIES");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        }
        let per_seg: usize = std::env::var("BLVM_IBD_SYNTH_SEG_ENTRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20_000_000);
        let n_segs: usize = std::env::var("BLVM_IBD_SYNTH_N_SEGS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8);
        let tmp = tempfile::Builder::new()
            .prefix("diskindex-synth-")
            .tempdir()
            .expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let mk = |i: u64| {
            let mut k = [0u8; 36];
            // Spread into the directory prefix (first 18–20 bits). Sequential
            // u64-in-low-bytes put every dest-ba-sized key in bucket 0 and F19
            // coalesced the whole 20M file (1 GiB/pread) — not dest-ba's layout.
            let mixed = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            k[..8].copy_from_slice(&mixed.to_be_bytes());
            k
        };
        let t_write = std::time::Instant::now();
        for s in 0..n_segs {
            let base = (s as u64) * (per_seg as u64);
            let mut entries: Vec<OutputKV> = (0..per_seg as u64)
                .map(|j| OutputKV::new_add(mk(base + j), 100, base + j + 1))
                .collect();
            entries.sort_unstable();
            disk.push_run_no_compact(MemoryRun::build_presorted(entries))
                .expect("spill");
            eprintln!(
                "synth wrote seg {}/{} entries={} segs_on_disk={}",
                s + 1,
                n_segs,
                per_seg,
                disk.segment_count()
            );
        }
        let write_s = t_write.elapsed().as_secs_f64();
        assert_eq!(
            disk.segment_count(),
            n_segs,
            "must not compact during spill"
        );
        let ram = disk.bloom_bytes_total();
        let bits = super::super::memory_run::directory_prefix_bits(per_seg);
        let bucket = per_seg / (1usize << bits);
        let kb_bucket = bucket * OutputKV::SIZE / 1024;
        eprintln!(
            "synth_index segs={} per_seg={} write_s={:.1} ram_miB={:.1} prefix_bits={} kb_per_bucket={}",
            n_segs,
            per_seg,
            write_s,
            ram as f64 / (1024.0 * 1024.0),
            bits,
            kb_bucket
        );

        let query = |label: &str, keys: Vec<[u8; 36]>| {
            let n = keys.len();
            let mut ids = vec![OutputId::MAX; n];
            super::super::disk_segment::reset_disk_io_stats();
            let t0 = std::time::Instant::now();
            disk.batch_query(&keys, &mut ids, 10_000);
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            let (preads, pread_kb, max_kb, cands, segs) =
                super::super::disk_segment::take_disk_io_stats();
            let hits = ids.iter().filter(|id| **id != OutputId::MAX).count();
            let kib_per = if preads > 0 {
                pread_kb as f64 / preads as f64
            } else {
                0.0
            };
            eprintln!(
                "synth_query {label} keys={n} hits={hits} disk_ms={ms:.1} preads={preads} cands={cands} segs={segs} pread_kb={pread_kb} max_pread_kb={max_kb} kib/pread={kib_per:.1}"
            );
            (preads, cands, ms)
        };

        // dest-ba 650k cand count: keys that live in the newest seg (best case).
        let newest_base = ((n_segs - 1) as u64) * (per_seg as u64);
        let k244: Vec<_> = (0..244u64).map(|j| mk(newest_base + j)).collect();
        let (p244, c244, ms244) = query("244_newest", k244);
        let k2219: Vec<_> = (0..2219u64).map(|j| mk(newest_base + j)).collect();
        let (p2219, c2219, ms2219) = query("2219_newest", k2219);
        // Oldest-seg hits: newest-to-oldest walk; blooms should skip 7 segs.
        let k_old: Vec<_> = (0..244u64).map(mk).collect();
        let (p_old, c_old, ms_old) = query("244_oldest", k_old);
        // Total misses: union FPR across n_segs.
        let miss_base = (n_segs as u64) * (per_seg as u64) + 1_000_000;
        let k_miss: Vec<_> = (0..244u64).map(|j| mk(miss_base + j)).collect();
        let (p_miss, c_miss, ms_miss) = query("244_miss", k_miss);

        eprintln!(
            "synth_vs_dest_ba 650k_preads_gauge=240 got_244_newest={p244} cands={c244} disk_ms={ms244:.1}"
        );
        eprintln!(
            "synth_vs_dest_ba 660k_preads_gauge=2138 got_2219_newest={p2219} cands={c2219} disk_ms={ms2219:.1}"
        );
        eprintln!(
            "synth_fpr oldest_preads={p_old} oldest_cands={c_old} miss_preads={p_miss} miss_cands={c_miss} oldest_ms={ms_old:.1} miss_ms={ms_miss:.1}"
        );

        // Blooms must keep miss preads ≪ n_segs × keys (otherwise 20M cap without GC is net negative).
        assert!(
            p_miss < 244 * n_segs as u64 / 4,
            "union FPR too high: miss preads={p_miss} vs 8×244"
        );
        assert!(p244 > 0 && p2219 > 0, "expected disk preads on hits");
        drop(tmp);
    }

    /// One large segment at the count-policy file size (70–150M). Prefix 20 must
    /// hold bucket cost vs dest-bc 776M @ prefix 16 (251 KiB, 6374 preads).
    /// `#[ignore]` — ~4.5 GiB artifact. Keys are prefix-sorted so the directory
    /// spreads (sequential low-bytes would pack bucket 0).
    #[serial_test::serial(ibd)]
    #[ignore]
    #[test]
    fn prefix20_large_seg_pread_budget() {
        let _guard = hot_pin_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_DISK_MMAP");
            std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
            std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        }
        let n: usize = std::env::var("BLVM_IBD_SYNTH_SEG_ENTRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(80_000_000);
        let buckets = 1usize << super::super::memory_run::DIRECTORY_PREFIX_BITS_MAX;
        let per_bucket = n.div_ceil(buckets).max(1);
        let mk = |i: u64| {
            let mut k = [0u8; 36];
            let p = i / per_bucket as u64;
            let rem = i % per_bucket as u64;
            k[0] = (p >> 12) as u8;
            k[1] = (p >> 4) as u8;
            k[2] = ((p & 0xF) << 4) as u8;
            k[3] = rem as u8;
            k
        };
        let tmp = tempfile::Builder::new()
            .prefix("prefix20-large-")
            .tempdir()
            .expect("tempdir");
        let t_write = std::time::Instant::now();
        let iter = (0..n as u64).map(|i| OutputKV::new_add(mk(i), 100, i + 1));
        let seg = super::super::disk_segment::DiskSegment::write_from_iter(tmp.path(), 0, n, iter)
            .expect("write 80M");
        let write_s = t_write.elapsed().as_secs_f64();
        let bits = super::super::memory_run::directory_prefix_bits(seg.entry_count);
        let bucket = seg.entry_count / (1usize << bits);
        let kib_bucket = bucket * OutputKV::SIZE / 1024;
        eprintln!(
            "prefix20_seg entries={} write_s={:.1} prefix_bits={} entries/bucket={} kib/bucket={} file_mib={:.1}",
            seg.entry_count,
            write_s,
            bits,
            bucket,
            kib_bucket,
            (seg.entry_count * OutputKV::SIZE) as f64 / (1024.0 * 1024.0)
        );

        let query = |label: &str, keys: Vec<[u8; 36]>| {
            let n = keys.len();
            let mut ids = vec![OutputId::MAX; n];
            super::super::disk_segment::reset_disk_io_stats();
            let t0 = std::time::Instant::now();
            seg.batch_lookup(&keys, &mut ids, 0, 10_000)
                .expect("lookup");
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            let (preads, pread_kb, max_kb, cands, segs) =
                super::super::disk_segment::take_disk_io_stats();
            let hits = ids.iter().filter(|id| **id != OutputId::MAX).count();
            let kib_per = if preads > 0 {
                pread_kb as f64 / preads as f64
            } else {
                0.0
            };
            eprintln!(
                "prefix20_query {label} keys={n} hits={hits} disk_ms={ms:.1} preads={preads} \
                 cands={cands} segs={segs} pread_kb={pread_kb} max_pread_kb={max_kb} kib/pread={kib_per:.1}"
            );
            (preads, cands, ms, kib_per)
        };

        let k244: Vec<_> = (0..244u64).map(|j| mk(j * (n as u64 / 244))).collect();
        let (p244, c244, ms244, kib244) = query("244_spread", k244);
        let k2219: Vec<_> = (0..2219u64).map(|j| mk(j * (n as u64 / 2219))).collect();
        let (p2219, c2219, ms2219, kib2219) = query("2219_spread", k2219);
        // High prefix 0xFF.. is past the 80M span (p < 2^20-1). Do not use
        // mk(n+…) — that wraps the 20-bit prefix and collides with live keys.
        let k_miss: Vec<_> = (0..244u64)
            .map(|j| {
                let mut k = [0xFFu8; 36];
                k[35] = j as u8;
                k
            })
            .collect();
        let (p_miss, c_miss, ms_miss, _) = query("244_miss", k_miss);

        eprintln!(
            "prefix20_vs_dest_bc dest_bc=251KiB/6374preads got_kib/bucket={kib_bucket} \
             244_preads={p244} cands={c244} disk_ms={ms244:.1} kib/pread={kib244:.1} \
             2219_preads={p2219} cands={c2219} disk_ms={ms2219:.1} kib/pread={kib2219:.1} \
             miss_preads={p_miss} miss_cands={c_miss} miss_ms={ms_miss:.1}"
        );
        assert_eq!(bits, 20, "80M must saturate prefix 20");
        assert!(
            kib_bucket <= 16,
            "prefix 20 must hold dest-ba-class buckets, got {kib_bucket} KiB"
        );
        assert!(p244 > 0 && p2219 > 0, "expected disk preads on hits");
        assert!(
            p_miss < 32,
            "bloom should skip almost all misses, preads={p_miss}"
        );
        drop(tmp);
    }
}
