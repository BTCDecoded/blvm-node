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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Merge oldest segments when this many have accumulated.
///
/// With K_DISK_FAN_IN = 8, the index holds at most ~8 segments in steady state:
/// after the 8th push, the 8 oldest are merged into one (with GC), reducing count to 1.
/// Each subsequent push brings it to 2, 3, … up to 8, then compacts again.
///
/// Memory bound: 8 bloom filters in RAM at ~8 MB each ≈ 64 MB max disk-tier overhead.
/// Lookup bound: O(8) pread64 calls per block instead of O(all-time evictions).
const K_DISK_FAN_IN: usize = 8;

pub struct DiskIndex {
    /// Segments oldest-to-newest. New segments are pushed to the back.
    pub(super) segments: parking_lot::RwLock<Vec<Arc<DiskSegment>>>,
    /// Directory for segment files.
    seg_dir: PathBuf,
    /// Monotonically increasing segment index (never reused, avoids filename collisions).
    next_idx: AtomicUsize,
    /// CAS guard: only one compaction runs at a time.
    is_compacting: AtomicBool,
}

impl DiskIndex {
    pub fn new(seg_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(seg_dir)?;
        Ok(Self {
            segments: parking_lot::RwLock::new(Vec::new()),
            seg_dir: seg_dir.to_owned(),
            next_idx: AtomicUsize::new(0),
            is_compacting: AtomicBool::new(false),
        })
    }

    /// Write a pre-sorted slice of entries directly to a new disk segment.
    ///
    /// Does **not** trigger `compact_oldest_if_needed`. Used by `seed_checkpoint` so that
    /// a large initial UTXO set (e.g. 250M entries = 14 GB) bypasses the memory-age cascade
    /// entirely, keeping peak RSS at O(1) rather than O(cascade_copies × UTXO_count).
    pub fn push_sorted_segment(&self, entries: &[OutputKV]) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let idx = self.next_idx.fetch_add(1, Ordering::Relaxed);
        let seg = DiskSegment::write_from_iter(
            &self.seg_dir,
            idx,
            entries.len(),
            entries.iter().copied(),
        )?;
        tracing::info!(
            "DiskIndex: seed segment {} — {} entries written directly to disk",
            idx,
            entries.len(),
        );
        self.segments.write().push(Arc::new(seg));
        Ok(())
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
    pub fn register_seg(&self, seg: DiskSegment) {
        self.segments.write().push(Arc::new(seg));
    }

    /// Evict `run` to disk and add to the index, then compact if too many segments.
    ///
    /// Called by the compacter when the deepest memory age overflows.
    pub fn push_run(&self, run: &MemoryRun) -> anyhow::Result<()> {
        let idx = self.next_idx.fetch_add(1, Ordering::Relaxed);
        let seg = DiskSegment::write(&self.seg_dir, idx, run)?;
        tracing::info!(
            "DiskIndex: evicted segment {} — {} entries, heights {}–{}",
            idx,
            run.entries.len(),
            run.height_range.0,
            run.height_range.1
        );
        self.segments.write().push(Arc::new(seg));

        // Compact oldest segments into one when we've accumulated enough.
        // This bounds the segment count at ~K_DISK_FAN_IN in steady state, keeping
        // per-block lookup cost O(K_DISK_FAN_IN) instead of O(total_evictions).
        if let Err(e) = self.compact_oldest_if_needed() {
            tracing::error!("DiskIndex: segment compaction failed: {e}");
        }
        Ok(())
    }

    /// Merge the oldest `K_DISK_FAN_IN` segments into one when enough have accumulated.
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

        // Always run at least 1 pass, even when only 1 segment exists.
        //
        // The CHECKPOINT_GC_FENCE has been advanced to the current checkpoint
        // height before this call. A single re-compaction of the one merged
        // segment applies the new fence, cancelling pairs that were above the
        // old fence but at or below the new one. This collapses ~200 M entries
        // → ~20 M entries (live UTXOs at checkpoint height) before the scan,
        // preventing the 11 GB read_all_entries spike in scan_live_at_height.
        let mut remaining = initial_count.max(1);
        let mut needed_passes = 0usize;
        while remaining > 1 {
            remaining = remaining.saturating_sub(K_DISK_FAN_IN).saturating_add(1);
            needed_passes += 1;
        }
        needed_passes += 1; // +1 final re-GC pass with the current fence

        tracing::debug!(
            "compact_for_checkpoint_sync: {} segments, {} passes (fence={})",
            initial_count,
            needed_passes,
            super::memory_run::gc_fence_snapshot(),
        );

        for _ in 0..needed_passes {
            if self.segments.read().is_empty() {
                break; // nothing left to compact
            }
            if let Err(e) = self.do_compact() {
                tracing::warn!("compact_for_checkpoint_sync: pass failed: {e:#}");
                break;
            }
        }

        self.is_compacting.store(false, Ordering::Release);
    }

    fn compact_oldest_if_needed(&self) -> anyhow::Result<()> {
        if self.segments.read().len() < K_DISK_FAN_IN {
            return Ok(());
        }
        // CAS: only one compaction at a time. Skip if another is in progress —
        // that compaction will re-check after finishing.
        if self
            .is_compacting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return Ok(());
        }
        // Loop: each compaction pass merges the oldest K_DISK_FAN_IN segments into one.
        // If multiple push_run calls accumulated segments while a previous compaction was
        // running (skipped due to the CAS guard), one compaction pass may not be enough
        // to get back under K_DISK_FAN_IN. Keep compacting until the count is low enough.
        let mut result = Ok(());
        while self.segments.read().len() >= K_DISK_FAN_IN {
            result = self.do_compact();
            if result.is_err() {
                break;
            }
        }
        self.is_compacting.store(false, Ordering::Release);
        result
    }

    fn do_compact(&self) -> anyhow::Result<()> {
        // Snapshot the oldest K_DISK_FAN_IN segments (they remain queryable during compaction).
        let to_compact: Vec<Arc<DiskSegment>> = {
            let r = self.segments.read();
            let n = K_DISK_FAN_IN.min(r.len());
            r[..n].to_vec()
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

        // Stream survivors directly to a new segment file. No output Vec.
        let idx = self.next_idx.fetch_add(1, Ordering::Relaxed);
        let new_seg = {
            let mut peekable = merge_iter.peekable();
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

        let total_out = new_seg.as_ref().map_or(0, |s| s.entry_count);

        // Atomically swap old segments for the new merged one.
        {
            let mut w = self.segments.write();
            let compact_ptrs: std::collections::HashSet<*const DiskSegment> =
                to_compact.iter().map(Arc::as_ptr).collect();
            let insert_pos = w
                .iter()
                .position(|s| compact_ptrs.contains(&Arc::as_ptr(s)))
                .unwrap_or(0);
            w.retain(|s| !compact_ptrs.contains(&Arc::as_ptr(s)));
            if let Some(seg) = new_seg {
                w.insert(insert_pos, seg);
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

        tracing::info!(
            "DiskIndex: compaction done — {total_in} entries in, {total_out} out (GC'd {})",
            total_in.saturating_sub(total_out),
        );
        Ok(())
    }

    /// Batch query all disk segments (oldest-to-newest) for unresolved keys.
    ///
    /// Only called for keys with `ids[i] == OutputId::MAX` after all memory ages.
    /// Keys with `OUTPUT_ID_DELETED` are skipped (already resolved as spent in memory).
    pub fn batch_query(&self, keys: &[[u8; 36]], ids: &mut [OutputId], before: i32) {
        // Skip segment lookup when all keys are already resolved (none remaining as MAX).
        if ids.contains(&OutputId::MAX) {
            let snapshot = {
                let r = self.segments.read();
                r.clone()
            };

            // Query newest-to-oldest: last segment first (most recent overflow data).
            for seg in snapshot.iter().rev() {
                if ids.iter().all(|id| *id != OutputId::MAX) {
                    break; // all resolved
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

    /// Number of segments currently on disk.
    pub fn segment_count(&self) -> usize {
        self.segments.read().len()
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
        let snapshot: Vec<Arc<DiskSegment>> = self.segments.read().clone();
        f(&snapshot);
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
