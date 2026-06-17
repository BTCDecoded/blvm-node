//! `MemoryRun`: sorted, immutable-once-built slice of `OutputKV` with bloom + directory acceleration.
//!
//! Core read path for the IBD UTXO engine: sorted runs with bloom and directory acceleration.
//!
//! ## Acceleration structures
//! - **`Directory`**: prefix index → narrows binary search to ~4KB per bucket.
//! - **`BloomFilter`**: blocked bloom, 7 probes, ~12 bits/entry, ~1% FPR.
//!
//! ## Parallelism
//! `query` is parallelized with rayon (8 sub-ranges when `rayon` feature is enabled).
//! When rayon is absent the loop runs single-threaded — correctness unaffected.

use super::types::{OUTPUT_ID_DELETED, OutputId, OutputKV};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

/// GC fence for cross-checkpoint Add+Delete pair cancellation.
///
/// The compacter's GC in `MemoryRun::merge` must not cancel an Add+Delete pair when the
/// Delete height exceeds the last committed checkpoint. Otherwise, `scan_live_at_height(H)`
/// run during a concurrent checkpoint export would miss UTXOs created before H but spent
/// after H — producing an incomplete checkpoint that causes "UTXO not found" on resume.
///
/// Set this to the checkpoint height (inclusive) immediately **before** starting a
/// checkpoint export. The GC will only cancel pairs where `Delete.height <= fence`.
/// After the export is committed, advance the fence to the new checkpoint height.
///
/// Initialized to `i32::MAX` so GC runs freely before the first checkpoint is committed.
/// GC fence: compaction may only cancel an Add+Delete pair when Delete.height <= this value.
///
/// Starts at 0 (no GC until the first checkpoint export sets a real height). After each
/// checkpoint export the fence is left at the exported height — NOT reset to i32::MAX.
/// This prevents compaction from cancelling Add+Delete pairs for UTXOs that were live at
/// the last checkpoint but spent after it (the Add would be missing from the next scan).
///
/// Advancing the fence to the next checkpoint height happens just before `scan_live_at_height`
/// is called, so at that point GC is allowed to cancel all pairs spent at or before that height.
/// Checkpoint export height, or `i32::MAX` when no export is in progress (unrestricted GC).
static CHECKPOINT_GC_FENCE: AtomicI32 = AtomicI32::new(i32::MAX);

/// Update the GC fence to `checkpoint_height`.
///
/// Call this **before** calling `run_checkpoint_export_replace` for the given height.
/// Any concurrent or future GC merge will refuse to cancel pairs where
/// `Delete.height > checkpoint_height`, keeping those Add entries visible to the
/// concurrent `scan_live_at_height(checkpoint_height)`.
pub fn set_gc_fence(checkpoint_height: i32) {
    CHECKPOINT_GC_FENCE.store(checkpoint_height, Ordering::Release);
    tracing::debug!(
        "IBD engine GC fence set to {} — cross-checkpoint GC disabled for Delete > {}",
        checkpoint_height,
        checkpoint_height
    );
}

/// Read the current GC fence value. Used by disk-level compaction to apply the
/// same GC rules as memory-level merges.
pub fn gc_fence_snapshot() -> i32 {
    CHECKPOINT_GC_FENCE.load(Ordering::Acquire)
}

// ─── Directory ───────────────────────────────────────────────────────────────

/// Prefix index that narrows binary search from O(n) to O(bucket_size).
///
/// Stores one start offset per `1 << prefix_bits` prefix buckets.
/// `lookup_range` returns a `[lo, hi)` slice of `entries` containing all keys with the
/// given prefix — limiting binary search to ~4 KB of entries.
#[derive(Debug, Clone)]
pub struct Directory {
    /// `buckets[b]` = first index in entries[] where prefix == b. Length = (1 << prefix_bits) + 1.
    buckets: Vec<u32>,
    prefix_bits: u32,
}

impl Directory {
    pub fn build(entries: &[OutputKV]) -> Self {
        if entries.is_empty() {
            return Self {
                buckets: vec![0, 0],
                prefix_bits: 1,
            };
        }
        // Target ~85 entries per bucket (85 × 52B ≈ 4420B ≈ 4 KB).
        let n = entries.len();
        let raw_bits = if n <= 128 {
            4u32
        } else {
            // ceil_log2(n / 85) clamped to [4, 16]
            let ratio = (n / 85).max(1);
            (usize::BITS - ratio.leading_zeros()).clamp(4, 16)
        };
        let prefix_bits = raw_bits;
        let num_buckets = 1usize << prefix_bits;
        let mut buckets = vec![0u32; num_buckets + 1];

        for (i, kv) in entries.iter().enumerate() {
            let prefix = key_prefix(&kv.key, prefix_bits) as usize;
            // Count entries per bucket (will prefix-sum below)
            buckets[prefix + 1] = buckets[prefix + 1].max((i + 1) as u32);
        }

        // Build proper start-offset array: buckets[b] = first index with prefix == b.
        // Since entries are sorted, we do a single linear scan.
        let mut bucket_start = vec![0u32; num_buckets + 1];
        let mut cur_bucket = 0usize;
        for (i, kv) in entries.iter().enumerate() {
            let prefix = key_prefix(&kv.key, prefix_bits) as usize;
            while cur_bucket <= prefix {
                bucket_start[cur_bucket] = i as u32;
                cur_bucket += 1;
            }
        }
        while cur_bucket <= num_buckets {
            bucket_start[cur_bucket] = entries.len() as u32;
            cur_bucket += 1;
        }

        Self {
            buckets: bucket_start,
            prefix_bits,
        }
    }

    /// Returns `(lo, hi)` index range in `entries` that may contain `key`.
    /// Caller does binary search within `entries[lo..hi]`.
    #[inline]
    pub fn lookup_range(&self, key: &[u8; 36]) -> (usize, usize) {
        let prefix = key_prefix(key, self.prefix_bits) as usize;
        let lo = self.buckets[prefix] as usize;
        let hi = self.buckets[prefix + 1] as usize;
        (lo, hi)
    }

    /// Build a directory by streaming `entry_count` entries from `reader`.
    /// The reader must be positioned at the first entry (i.e. just after the
    /// segment header). Uses O(`2^prefix_bits`) memory — at most 256 KB.
    pub(super) fn build_streaming(
        reader: &mut super::disk_segment::SegmentReader,
        entry_count: usize,
    ) -> anyhow::Result<Self> {
        if entry_count == 0 {
            return Ok(Self {
                buckets: vec![0, 0],
                prefix_bits: 1,
            });
        }
        let prefix_bits = if entry_count <= 128 {
            4u32
        } else {
            let ratio = (entry_count / 85).max(1);
            (usize::BITS - ratio.leading_zeros()).clamp(4, 16)
        };
        let num_buckets = 1usize << prefix_bits;
        let mut bucket_start = vec![0u32; num_buckets + 1];
        let mut cur_bucket = 0usize;
        let mut i = 0usize;
        while let Some(kv) = reader.advance()? {
            let prefix = key_prefix(&kv.key, prefix_bits) as usize;
            while cur_bucket <= prefix {
                bucket_start[cur_bucket] = i as u32;
                cur_bucket += 1;
            }
            i += 1;
        }
        while cur_bucket <= num_buckets {
            bucket_start[cur_bucket] = entry_count as u32;
            cur_bucket += 1;
        }
        Ok(Self {
            buckets: bucket_start,
            prefix_bits,
        })
    }
}

#[inline]
fn key_prefix(key: &[u8; 36], bits: u32) -> u32 {
    // Use first 4 bytes of txid (big-endian) as prefix source.
    let raw = u32::from_be_bytes(key[..4].try_into().unwrap());
    raw >> (32 - bits)
}

// ─── BloomFilter ─────────────────────────────────────────────────────────────

/// Blocked bloom filter for negative lookup acceleration.
///
/// - 64-byte cache-aligned blocks, 7 probes per key, ~12 bits/entry → ~1% FPR.
/// - Hash: `block_idx` from txid[0..4], `bit_pattern` from txid[4..12] XOR (vout × GOLDEN_RATIO).
/// - `may_contain` returns `false` only when the key is definitely absent (no false negatives).
#[derive(Debug, Clone)]
pub struct BloomFilter {
    /// Raw bits. Length = num_blocks × 64 bytes. Always a multiple of 64.
    data: Vec<u64>,
    /// Number of 64-byte (8 × u64) blocks.
    num_blocks: usize,
}

const BLOOM_WORDS_PER_BLOCK: usize = 8; // 64 bytes / 8 bytes per u64

impl BloomFilter {
    const GOLDEN_RATIO_64: u64 = 0x9e3779b97f4a7c15;

    pub fn build(entries: &[OutputKV]) -> Self {
        if entries.is_empty() {
            return Self {
                data: vec![0u64; BLOOM_WORDS_PER_BLOCK],
                num_blocks: 1,
            };
        }
        // ~12 bits/entry → num_blocks = ceil(entries.len() * 12 / (64 * 8))
        let bits_needed = entries.len() * 12;
        let num_blocks = bits_needed.div_ceil(512).max(1);
        let mut data = vec![0u64; num_blocks * BLOOM_WORDS_PER_BLOCK];

        for kv in entries {
            let (block_idx, word_bits) = Self::hash_key(&kv.key, num_blocks);
            let base = block_idx * BLOOM_WORDS_PER_BLOCK;
            for probe in 0..7usize {
                let bit = ((word_bits >> (probe * 9)) & 0x1FF) as usize;
                let word = bit / 64;
                let shift = bit % 64;
                data[base + word % BLOOM_WORDS_PER_BLOCK] |= 1u64 << shift;
            }
        }

        Self { data, num_blocks }
    }

    /// Allocate a bloom filter sized for `capacity` entries (may be over-provisioned).
    /// Use `insert` to add entries one-by-one during streaming writes.
    pub fn new_for_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let bits_needed = capacity * 12;
        let num_blocks = bits_needed.div_ceil(512).max(1);
        Self {
            data: vec![0u64; num_blocks * BLOOM_WORDS_PER_BLOCK],
            num_blocks,
        }
    }

    /// Insert a single key. Used by streaming segment writers to build the filter
    /// incrementally without holding all entries in memory.
    #[inline]
    pub fn insert(&mut self, key: &[u8; 36]) {
        let (block_idx, word_bits) = Self::hash_key(key, self.num_blocks);
        let base = block_idx * BLOOM_WORDS_PER_BLOCK;
        for probe in 0..7usize {
            let bit = ((word_bits >> (probe * 9)) & 0x1FF) as usize;
            let word = bit / 64;
            let shift = bit % 64;
            self.data[base + word % BLOOM_WORDS_PER_BLOCK] |= 1u64 << shift;
        }
    }

    /// Returns `false` if the key is definitely not in the set. Returns `true` if it may be.
    #[inline]
    pub fn may_contain(&self, key: &[u8; 36]) -> bool {
        let (block_idx, word_bits) = Self::hash_key(key, self.num_blocks);
        let base = block_idx * BLOOM_WORDS_PER_BLOCK;
        for probe in 0..7usize {
            let bit = ((word_bits >> (probe * 9)) & 0x1FF) as usize;
            let word = bit / 64;
            let shift = bit % 64;
            if self.data[base + word % BLOOM_WORDS_PER_BLOCK] & (1u64 << shift) == 0 {
                return false;
            }
        }
        true
    }

    #[inline]
    fn hash_key(key: &[u8; 36], num_blocks: usize) -> (usize, u64) {
        // Mix the full key into two independent 64-bit hashes using a Murmur3/xxHash-style
        // finalizer. This gives good distribution even for degenerate keys (e.g., only the
        // first 4 bytes differ). LE interpretation gives better low-bit distribution than BE
        // when keys are sequential integers stored in the high bytes (i*2^32 pattern).
        let r0 = u64::from_le_bytes(key[0..8].try_into().unwrap());
        let r1 = u64::from_le_bytes(key[8..16].try_into().unwrap());
        let r2 = u64::from_le_bytes(key[16..24].try_into().unwrap());
        let r3 = u64::from_le_bytes(key[24..32].try_into().unwrap());
        let r4 = u32::from_le_bytes(key[32..36].try_into().unwrap()) as u64;

        // Combine all words into a single 64-bit accumulator.
        let acc = r0
            .wrapping_add(r1.rotate_left(17))
            .wrapping_add(r2.rotate_right(11))
            .wrapping_add(r3.rotate_left(29))
            .wrapping_add(r4.wrapping_mul(Self::GOLDEN_RATIO_64));

        // Apply Murmur3 64-bit finalizer (high quality, one-to-one mapping).
        let fmix = |mut x: u64| -> u64 {
            x ^= x >> 33;
            x = x.wrapping_mul(0xff51afd7ed558ccd);
            x ^= x >> 33;
            x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
            x ^= x >> 33;
            x
        };

        let h0 = fmix(acc);
        let h1 = fmix(acc.wrapping_add(Self::GOLDEN_RATIO_64));

        let block_idx = (h0 % num_blocks as u64) as usize;
        (block_idx, h1)
    }
}

// ─── MemoryRun ───────────────────────────────────────────────────────────────

/// Result of a batch query against a `MemoryRun`.
#[derive(Debug, Default, Clone)]
pub struct QueryResult {
    /// Number of keys resolved (Add entry found, no covering Delete).
    pub resolved: usize,
    /// Number of keys with a Delete entry (spent in this run).
    pub deleted: usize,
    /// Number of keys definitely absent from this run (bloom negative or not found).
    pub absent: usize,
}

/// Sorted, immutable-once-built collection of `OutputKV` entries with bloom + directory.
///
/// Built by `MemoryAge::append`. Queried by `MemoryIndex::query`. Merged by `Compacter`.
#[derive(Debug, Clone)]
pub struct MemoryRun {
    pub(super) entries: Vec<OutputKV>,
    pub(super) height_range: (i32, i32),
    pub(super) directory: Directory,
    pub(super) filter: BloomFilter,
    /// `true` while the run is the mutable tip (appends allowed). Frozen once pushed to `runs`.
    pub(super) is_mutable: bool,
}

impl MemoryRun {
    /// Build a new `MemoryRun` from entries that may not be sorted.
    pub fn build(mut entries: Vec<OutputKV>) -> Self {
        entries.sort_unstable();
        let height_range = height_range_of(&entries);
        let directory = Directory::build(&entries);
        let filter = BloomFilter::build(&entries);
        Self {
            entries,
            height_range,
            directory,
            filter,
            is_mutable: false,
        }
    }

    /// Build a `MemoryRun` from entries that are **already sorted**.
    ///
    /// Skips the `sort_unstable` step. Used by the compacter after k-way merge, which
    /// produces sorted output by construction.
    pub fn build_presorted(entries: Vec<OutputKV>) -> Self {
        debug_assert!(
            entries.windows(2).all(|w| w[0] <= w[1]),
            "build_presorted: entries must be sorted"
        );
        let height_range = height_range_of(&entries);
        let directory = Directory::build(&entries);
        let filter = BloomFilter::build(&entries);
        Self {
            entries,
            height_range,
            directory,
            filter,
            is_mutable: false,
        }
    }

    /// Build an empty mutable run for the tip age (block-by-block append target).
    pub fn new_mutable() -> Self {
        Self {
            entries: Vec::new(),
            height_range: (i32::MAX, i32::MIN),
            directory: Directory::build(&[]),
            filter: BloomFilter::build(&[]),
            is_mutable: true,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn height_range(&self) -> (i32, i32) {
        self.height_range
    }

    /// Append entries (for the mutable tip run only). Sorts in place and rebuilds acceleration structures.
    ///
    /// Called from `MemoryAge::append` while holding the write lock.
    pub fn append_and_rebuild(&mut self, new_entries: &[OutputKV]) {
        debug_assert!(self.is_mutable, "cannot append to frozen run");
        self.entries.extend_from_slice(new_entries);
        self.entries.sort_unstable();
        self.height_range = height_range_of(&self.entries);
        self.directory = Directory::build(&self.entries);
        self.filter = BloomFilter::build(&self.entries);
    }

    /// Freeze the mutable run (called by `MemoryAge` before creating a new mutable run).
    pub fn freeze(&mut self) {
        self.is_mutable = false;
    }

    /// Look up `key` in this run within `[since, before)` height window.
    ///
    /// Returns `Some(id)` if an Add entry is found (non-deleted), `None` otherwise.
    #[inline]
    pub fn lookup_key(&self, key: &[u8; 36], since: i32, before: i32) -> Option<OutputId> {
        // Fast exits
        if self.height_range.1 < since || self.height_range.0 >= before {
            return None;
        }
        if !self.filter.may_contain(key) {
            return None;
        }
        let (lo, hi) = self.directory.lookup_range(key);
        if lo >= hi {
            return None;
        }
        // Binary search for first entry with key >= target.
        let slice = &self.entries[lo..hi];
        let pos = slice.partition_point(|e| e.key < *key);
        // Scan entries with matching key, newest-to-oldest.
        // Sort order: (key, height desc, Add before Delete at same height).
        // For same (key, height): Add appears before Delete. If we see an Add then
        // immediately a Delete at the same height, the UTXO was created and spent at
        // the same height (intra-block) — should not happen after intra-block filtering,
        // but handled defensively: Delete invalidates the paired Add.
        let mut result: Option<OutputId> = None;
        let mut i = pos;
        while i < slice.len() {
            let e = &slice[i];
            if e.key != *key {
                break;
            }
            if e.height < since || e.height >= before {
                i += 1;
                continue;
            }
            if e.is_add() {
                // Peek at next entry: if it's a Delete at the same height, both cancel.
                let next = slice.get(i + 1);
                if let Some(n) = next {
                    if n.key == *key && n.height == e.height && n.is_delete() {
                        i += 2; // skip both (same-height create+spend)
                        continue;
                    }
                }
                result = Some(e.id);
                break;
            } else if e.is_delete() {
                // Delete at a newer height than any Add below — key is spent.
                // Return OUTPUT_ID_DELETED so callers (batch_query) know to skip
                // the disk fallback for this key. UtxoIndex filters the sentinel
                // to None before returning to external callers.
                result = Some(OUTPUT_ID_DELETED);
                break;
            }
            i += 1;
        }
        result
    }

    /// Batch lookup for a sorted slice of keys. Fills `ids[i]` with the Add id for `keys[i]`,
    /// or leaves it as `OutputId::MAX` (sentinel for "not found in this run").
    ///
    /// When the `rayon` feature is enabled this splits the key range across 8 rayon workers.
    pub fn batch_lookup(&self, keys: &[[u8; 36]], ids: &mut [OutputId], since: i32, before: i32) {
        debug_assert_eq!(keys.len(), ids.len());
        #[cfg(feature = "rayon")]
        {
            use rayon::prelude::*;
            keys.par_iter()
                .zip(ids.par_iter_mut())
                .for_each(|(key, id)| {
                    if *id == OutputId::MAX {
                        if let Some(found) = self.lookup_key(key, since, before) {
                            *id = found;
                        }
                    }
                    // OUTPUT_ID_DELETED is treated as "resolved — skip" (same as real id != MAX)
                });
        }
        #[cfg(not(feature = "rayon"))]
        {
            for (key, id) in keys.iter().zip(ids.iter_mut()) {
                // Skip keys already resolved (real id) or confirmed deleted.
                // Only MAX means "not yet found anywhere".
                if *id == OutputId::MAX {
                    if let Some(found) = self.lookup_key(key, since, before) {
                        *id = found; // either a real id or OUTPUT_ID_DELETED
                    }
                }
            }
        }
    }

    /// K-way merge of multiple `MemoryRun`s into one frozen run.
    ///
    /// Entries with matching (key, height, op=Add) and a corresponding (key, height, op=Delete)
    /// in the same merge set are cancelled (both dropped) — removing spent UTXOs from frozen storage.
    pub fn merge(inputs: &[Arc<MemoryRun>]) -> Self {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;

        if inputs.is_empty() {
            return Self::build(vec![]);
        }

        // Estimate capacity
        let total: usize = inputs.iter().map(|r| r.entries.len()).sum();
        let mut merged: Vec<OutputKV> = Vec::with_capacity(total);

        // K-way merge via min-heap. Heap item: (entry, run_idx, entry_idx).
        #[derive(PartialEq, Eq)]
        struct HeapItem {
            entry: OutputKV,
            run_idx: usize,
            entry_idx: usize,
        }
        impl Ord for HeapItem {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                // Min-heap: smallest entry first. OutputKV::Ord: key asc, height desc.
                other.entry.cmp(&self.entry)
            }
        }
        impl PartialOrd for HeapItem {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        let mut heap = BinaryHeap::with_capacity(inputs.len());
        for (ri, run) in inputs.iter().enumerate() {
            if let Some(e) = run.entries.first() {
                heap.push(HeapItem {
                    entry: *e,
                    run_idx: ri,
                    entry_idx: 0,
                });
            }
        }

        while let Some(HeapItem {
            entry,
            run_idx,
            entry_idx,
        }) = heap.pop()
        {
            // Push next from the same run.
            let next_idx = entry_idx + 1;
            if let Some(next) = inputs[run_idx].entries.get(next_idx) {
                heap.push(HeapItem {
                    entry: *next,
                    run_idx,
                    entry_idx: next_idx,
                });
            }
            merged.push(entry);
        }

        // GC pass: cancel Add+Delete pairs, both same-height and cross-height.
        //
        // Sort order: key asc → height desc → Add before Delete at same height.
        // Within each key group entries arrive newest→oldest. A Delete at height D
        // followed by an Add at height A (where A < D) means: UTXO created at A,
        // spent at D — both entries are permanently dead and can be discarded.
        //
        // This is safe for forward-only IBD: blocks are processed in order, so a
        // spent UTXO will never be queried again. Discarding both entries frees
        // memory proportional to the number of spent UTXOs in the merge set,
        // keeping RSS bounded rather than growing with cumulative UTXO history.
        //
        // Correctness exception: if there is a newer Add at height B > D (the output
        // was recreated after spending — rare but valid in early Bitcoin), that Add
        // appears BEFORE the Delete in sort order and is kept separately; the GC
        // only cancels the Delete and its older matching Add.
        // GC pass: cancel Add+Delete pairs for the same key, at any heights.
        //
        // Two orderings exist within a key group (key asc, height desc, Add before Delete):
        //
        //  1. Same-height:   Add(h=H) arrives first, Delete(h=H) arrives second.
        //     → When we see the Delete, pop the last gc entry if it's an Add for the same key
        //       at the same height.
        //
        //  2. Cross-height:  Delete(h=D) arrives first (D > A), Add(h=A) arrives second.
        //     → Stash the Delete; when we see the Add, cancel both.
        //
        // A "dangling Delete" (no Add in this merge tier — its Add is on disk) must be
        // kept so it can shadow the disk-resident Add during queries.
        let mut gc: Vec<OutputKV> = Vec::with_capacity(merged.len());
        let mut pending_del: Option<OutputKV> = None;

        for e in merged {
            // Key boundary: flush any pending cross-height Delete.
            if let Some(d) = pending_del {
                if d.key != e.key {
                    gc.push(d); // dangling — no Add found for this key
                    pending_del = None;
                } else {
                    pending_del = Some(d);
                }
            }

            if e.is_delete() {
                // Case 1 (same-height): Add was already pushed to gc last.
                if let Some(last) = gc.last() {
                    if last.key == e.key && last.height == e.height && last.is_add() {
                        // Same-height Add+Delete: always dead (spent in the same block).
                        gc.pop();
                        continue;
                    }
                }
                // Case 2 (cross-height): Add comes later. Stash this Delete.
                pending_del = Some(e);
            } else {
                // Add entry.
                if let Some(d) = pending_del.take() {
                    if d.key == e.key && d.height > e.height {
                        // Cross-height cancel: created at e.height, spent at d.height.
                        //
                        // Safety check: if d.height > CHECKPOINT_GC_FENCE we must NOT
                        // cancel. A concurrent `scan_live_at_height(fence)` needs to see
                        // the Add (e) because the UTXO was alive at the checkpoint height.
                        // Cancelling here would make it invisible, producing an incomplete
                        // checkpoint and "UTXO not found" errors on resume.
                        let fence = CHECKPOINT_GC_FENCE.load(Ordering::Acquire);
                        if d.height <= fence {
                            continue; // drop both (safe: spent at or before checkpoint)
                        }
                        // Delete is after fence — keep both to preserve checkpoint correctness.
                        gc.push(d);
                        gc.push(e);
                        continue;
                    }
                    // Unexpected ordering (invalid Bitcoin, e.g. Delete at lower height
                    // than its Add). Keep both defensively.
                    gc.push(d);
                    gc.push(e);
                } else {
                    gc.push(e); // live UTXO
                }
            }
        }
        if let Some(d) = pending_del {
            gc.push(d); // dangling Delete — keep to shadow disk Add
        }

        // gc is sorted (GC only removes pairs, preserving relative order).
        Self::build_presorted(gc)
    }

    /// Remove all entries with `height >= since`. Mutable runs only (reorg recovery).
    ///
    /// Rebuilds directory and filter after removal.
    pub fn erase_since(&mut self, since: i32) {
        debug_assert!(self.is_mutable, "erase_since on frozen run");
        self.entries.retain(|e| e.height < since);
        self.height_range = height_range_of(&self.entries);
        self.directory = Directory::build(&self.entries);
        self.filter = BloomFilter::build(&self.entries);
    }
}

fn height_range_of(entries: &[OutputKV]) -> (i32, i32) {
    if entries.is_empty() {
        return (i32::MAX, i32::MIN);
    }
    let min = entries.iter().map(|e| e.height).min().unwrap_or(i32::MAX);
    let max = entries.iter().map(|e| e.height).max().unwrap_or(i32::MIN);
    (min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(n: u8) -> [u8; 36] {
        let mut k = [0u8; 36];
        k[0] = n;
        k
    }

    #[test]
    fn test_bloom_no_false_negatives() {
        let keys: Vec<[u8; 36]> = (0..100).map(make_key).collect();
        let entries: Vec<OutputKV> = keys.iter().map(|k| OutputKV::new_add(*k, 1, 42)).collect();
        let bloom = BloomFilter::build(&entries);
        for k in &keys {
            assert!(bloom.may_contain(k), "false negative for key {:?}", k[0]);
        }
    }

    #[test]
    fn test_bloom_fpr_under_2pct() {
        // Build with 1000 entries, check 10000 random non-overlapping keys.
        let entries: Vec<OutputKV> = (0u32..1000)
            .map(|i| {
                let mut k = [0u8; 36];
                k[..4].copy_from_slice(&i.to_be_bytes());
                OutputKV::new_add(k, 1, i as u64)
            })
            .collect();
        let bloom = BloomFilter::build(&entries);
        let mut false_positives = 0usize;
        for i in 1000u32..11000u32 {
            let mut k = [0u8; 36];
            k[..4].copy_from_slice(&i.to_be_bytes());
            if bloom.may_contain(&k) {
                false_positives += 1;
            }
        }
        let fpr = false_positives as f64 / 10000.0;
        assert!(fpr < 0.02, "FPR too high: {:.2}%", fpr * 100.0);
    }

    #[test]
    fn test_directory_matches_linear_scan() {
        let entries: Vec<OutputKV> = (0u32..500)
            .map(|i| {
                let mut k = [0u8; 36];
                k[..4].copy_from_slice(&i.to_be_bytes());
                OutputKV::new_add(k, 1, i as u64)
            })
            .collect();
        let run = MemoryRun::build(entries);
        for i in 0u32..500 {
            let mut k = [0u8; 36];
            k[..4].copy_from_slice(&i.to_be_bytes());
            let (lo, hi) = run.directory.lookup_range(&k);
            // Key must be within [lo, hi)
            let found = run.entries[lo..hi].iter().any(|e| e.key == k);
            assert!(found, "directory missed key {i}");
        }
    }

    #[test]
    fn test_lookup_key_basic() {
        let k1 = make_key(1);
        let k2 = make_key(2);
        let entries = vec![
            OutputKV::new_add(k1, 100, 42),
            OutputKV::new_add(k2, 200, 99),
        ];
        let run = MemoryRun::build(entries);
        assert_eq!(run.lookup_key(&k1, 0, i32::MAX), Some(42));
        assert_eq!(run.lookup_key(&k2, 0, i32::MAX), Some(99));
        assert_eq!(run.lookup_key(&make_key(3), 0, i32::MAX), None);
    }

    #[test]
    fn test_lookup_height_window() {
        let k = make_key(1);
        let entries = vec![OutputKV::new_add(k, 100, 42)];
        let run = MemoryRun::build(entries);
        assert_eq!(run.lookup_key(&k, 0, 101), Some(42));
        // Height 100 is outside [101, MAX) window — should not be found.
        assert_eq!(run.lookup_key(&k, 101, i32::MAX), None);
    }

    #[test]
    fn test_delete_hides_add() {
        let k = make_key(1);
        // Add at h=100, Delete at h=200 — Delete is newer so lookup returns OUTPUT_ID_DELETED
        // (not None). The sentinel tells disk-index callers to skip the disk fallback.
        let entries = vec![OutputKV::new_delete(k, 200), OutputKV::new_add(k, 100, 42)];
        let run = MemoryRun::build(entries);
        // sorted: delete (h=200, newest) before add (h=100)
        assert_eq!(run.lookup_key(&k, 0, i32::MAX), Some(OUTPUT_ID_DELETED));
    }

    #[test]
    fn test_merge_cancellation_same_height() {
        let k = make_key(1);
        let run_a = Arc::new(MemoryRun::build(vec![OutputKV::new_add(k, 100, 42)]));
        let run_b = Arc::new(MemoryRun::build(vec![OutputKV::new_delete(k, 100)]));
        let merged = MemoryRun::merge(&[run_a, run_b]);
        assert!(
            merged.entries.is_empty(),
            "same-height cancel failed: {:?}",
            merged.entries.len()
        );
    }

    #[test]
    fn test_merge_cancellation_cross_height() {
        let k = make_key(1);
        // UTXO created at h=100, spent at h=290000 — different heights.
        let run_a = Arc::new(MemoryRun::build(vec![OutputKV::new_add(k, 100, 42)]));
        let run_b = Arc::new(MemoryRun::build(vec![OutputKV::new_delete(k, 290_000)]));
        let merged = MemoryRun::merge(&[run_a, run_b]);
        // Both dead — the UTXO lifecycle is fully committed.
        assert!(
            merged.entries.is_empty(),
            "cross-height cancel failed: {:?}",
            merged.entries.len()
        );
    }

    #[test]
    fn test_merge_cross_height_with_recreation() {
        let k = make_key(1);
        // UTXO created h=100, spent h=200, recreated h=300 (early Bitcoin P2PKH reuse).
        let run_a = Arc::new(MemoryRun::build(vec![
            OutputKV::new_add(k, 100, 11),
            OutputKV::new_delete(k, 200),
            OutputKV::new_add(k, 300, 99),
        ]));
        let merged = MemoryRun::merge(&[run_a]);
        // Add(h=100) + Delete(h=200) cancel; Add(h=300) survives as the live UTXO.
        assert_eq!(
            merged.entries.len(),
            1,
            "expected 1 live entry, got {:?}",
            merged.entries
        );
        assert_eq!(merged.entries[0].height, 300);
        assert_eq!(merged.entries[0].id, 99);
        assert!(merged.entries[0].is_add());
    }

    #[test]
    fn test_merge_dangling_delete_preserved() {
        let k = make_key(1);
        // Delete only (Add was already evicted to disk) — must survive to shadow disk Add.
        let run_a = Arc::new(MemoryRun::build(vec![OutputKV::new_delete(k, 200)]));
        let merged = MemoryRun::merge(&[run_a]);
        assert_eq!(merged.entries.len(), 1);
        assert!(merged.entries[0].is_delete());
    }

    #[test]
    fn test_erase_since() {
        let k1 = make_key(1);
        let k2 = make_key(2);
        let mut run = MemoryRun::new_mutable();
        run.append_and_rebuild(&[OutputKV::new_add(k1, 50, 1), OutputKV::new_add(k2, 100, 2)]);
        run.erase_since(75);
        assert_eq!(run.entries.len(), 1);
        assert_eq!(run.entries[0].key, k1);
    }
}
