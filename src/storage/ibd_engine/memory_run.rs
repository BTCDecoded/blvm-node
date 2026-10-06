//! `MemoryRun`: sorted, immutable-once-built slice of `OutputKV` with bloom + directory acceleration.
//!
//! Core read path for the IBD UTXO engine: sorted runs with bloom and directory acceleration.
//!
//! ## Acceleration structures
//! - **`Directory`**: prefix index → narrows binary search to ~4KB per bucket.
//! - **`BloomFilter`**: blocked bloom, 7 probes, ~12 bits/entry, ~1% FPR.
//!
//! ## Parallelism
//! `batch_lookup` uses rayon only when `keys.len() >= RAYON_BATCH_THRESHOLD`. Below that,
//! pool steal/pin overhead dominated early-height IBD (~300 BPS); sequential is faster.

use super::types::{OUTPUT_ID_DELETED, OutputId, OutputKV, OutputKey};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicUsize, Ordering};

/// Live MemoryRun instance count (including the mutable tip). Counts the MemoryRun struct itself,
/// NOT the Arc wrapper. Incremented in build/build_presorted/new_mutable, decremented in Drop.
/// Using AtomicI64 so underflow is visible as a negative number (previous AtomicUsize wrapped).
pub static MEMORY_RUN_LIVE: AtomicI64 = AtomicI64::new(0);
/// Cumulative MemoryRun builds (for rate logging). Never decremented.
pub static MEMORY_RUN_TOTAL: AtomicUsize = AtomicUsize::new(0);

/// Below this key count, `batch_lookup` runs sequentially (rayon overhead >> work).
pub(super) const RAYON_BATCH_THRESHOLD: usize = 64;
/// Below this length, external-key sorts use `sort_unstable` instead of `par_sort_unstable`.
pub(super) const RAYON_SORT_THRESHOLD: usize = 128;

/// Default on. `BLVM_IBD_QUERY_RAYON=0` forces serial `batch_lookup` (findings bisect).
fn query_rayon_enabled() -> bool {
    !matches!(
        std::env::var("BLVM_IBD_QUERY_RAYON")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("0") | Some("false") | Some("no") | Some("off")
    )
}

/// Sort output keys for batch_query; parallel only when the slice is large enough.
#[inline]
pub(super) fn sort_external_keys(keys: &mut [OutputKey]) {
    #[cfg(feature = "rayon")]
    {
        if keys.len() >= RAYON_SORT_THRESHOLD {
            use rayon::prelude::*;
            keys.par_sort_unstable();
        } else {
            keys.sort_unstable();
        }
    }
    #[cfg(not(feature = "rayon"))]
    keys.sort_unstable();
}

/// GC fence for cross-checkpoint Add+Delete pair cancellation.
///
/// Starts at `i32::MAX`. No finite fence has been stored yet.
/// `advance_gc_fence_to` and `advance_gc_fence_between_exports` use `fetch_max`
/// and cannot leave `MAX`. `set_gc_fence` is the store. An export that never
/// starts leaves the fence at `MAX`.
///
/// `MAX` means no finite fence yet. It does not mean the oldest fan-in window
/// will shrink: that window does not contain Deletes that live in younger segments.
///
/// Once `set_gc_fence` has stored a checkpoint height, compaction may cancel an
/// Add+Delete pair only when `Delete.height <= fence`.
static CHECKPOINT_GC_FENCE: AtomicI32 = AtomicI32::new(i32::MAX);

/// Update the GC fence to `checkpoint_height`.
///
/// Call this **before** calling `run_checkpoint_export_replace` for the given height.
/// Any concurrent or future GC merge will refuse to cancel pairs where
/// `Delete.height > checkpoint_height`, keeping those Add entries visible to the
/// concurrent `scan_live_at_height(checkpoint_height)`.
pub fn set_gc_fence(checkpoint_height: i32) {
    CHECKPOINT_GC_FENCE.store(checkpoint_height, Ordering::Release);
    note_gc_fence_high_water(checkpoint_height);
    tracing::debug!(
        "IBD engine GC fence set to {} — cross-checkpoint GC disabled for Delete > {}",
        checkpoint_height,
        checkpoint_height
    );
}

/// Monotonically advance the GC fence during gap replay (export deferred to tip).
///
/// Without this, `CHECKPOINT_GC_FENCE` stays at the last export height while validation
/// runs millions of blocks ahead — disk compactions merge Add-only segments with `GC'd 0`
/// (15–30 s stalls, no space benefit). Advancing the fence lets compactions cancel spent
/// pairs for validated heights. Safe on SIGKILL: resume still re-seeds from last export.
pub fn advance_gc_fence_to(height: i32) {
    if height <= 0 {
        return;
    }
    let prev = CHECKPOINT_GC_FENCE.fetch_max(height, Ordering::AcqRel);
    note_gc_fence_high_water(height);
    if height > prev {
        tracing::info!(
            "IBD engine GC fence advanced {} → {} (gap replay — disk/memory compaction may GC spent pairs)",
            prev,
            height
        );
    }
}

/// Read the current GC fence value. Used by disk-level compaction to apply the
/// same GC rules as memory-level merges.
pub fn gc_fence_snapshot() -> i32 {
    CHECKPOINT_GC_FENCE.load(Ordering::Acquire)
}

/// Highest **finite** fence ever applied in this process (`i32::MAX` = "no fence" is not
/// recorded). Pairs with `Delete.height <= high_water` may already be gone, so no snapshot
/// may ever be labelled **below** this height — the export scheduler checks it (R-352).
static GC_FENCE_HIGH_WATER: AtomicI32 = AtomicI32::new(0);

fn note_gc_fence_high_water(height: i32) {
    if height > 0 && height != i32::MAX {
        GC_FENCE_HIGH_WATER.fetch_max(height, Ordering::AcqRel);
    }
}

/// See [`GC_FENCE_HIGH_WATER`]. `0` when no finite fence was ever set.
pub fn gc_fence_high_water() -> i32 {
    GC_FENCE_HIGH_WATER.load(Ordering::Acquire)
}

/// Next scheduled checkpoint height (`last_exported + interval`), published by the export
/// thread every tick. The between-export fence advance never passes it, so a lagged or
/// refused-and-retried export at that height still finds every Add it needs (R-351: every
/// export ran 20k behind validation via LAG_EXEMPT; with a 100k interval a refused persist
/// would otherwise retry below the high-water forever and the thread could never exit).
static NEXT_CHECKPOINT_TARGET: AtomicI32 = AtomicI32::new(0);

pub fn set_next_checkpoint_target(height: i32) {
    NEXT_CHECKPOINT_TARGET.store(height.max(0), Ordering::Release);
}

pub fn next_checkpoint_target() -> i32 {
    NEXT_CHECKPOINT_TARGET.load(Ordering::Acquire)
}

/// R-352: let the fence follow validation **between** exports, not only during gap replay.
///
/// With exports 100k apart (`BLVM_IBD_CHECKPOINT_INTERVAL`), a fence parked at the last
/// export height means every spend after it is un-GC-able: memory merges and disk
/// compactions report `GC'd 0`, the cold journal grows toward
/// `CHECKPOINT_COMPACT_INPUT_TARGET` and the journal scaler drags the interval back to 10k.
/// Advancing to the validated height is safe **only while no export scan is running** —
/// the caller passes that (`IBD_CHECKPOINT_EXPORT_ACTIVE`); during a scan the fence must
/// stay at the checkpoint height so Adds live at `ckpt` but spent later survive the scan.
/// The next export must then be labelled at or above [`gc_fence_high_water`].
pub fn advance_gc_fence_between_exports(height: i32, export_active: bool) -> bool {
    if export_active || height <= 0 {
        return false;
    }
    // Clamp to the next scheduled checkpoint; unknown target (thread not ticked yet) → wait.
    let target = next_checkpoint_target();
    if target <= 0 {
        return false;
    }
    let height = height.min(target);
    let prev = CHECKPOINT_GC_FENCE.fetch_max(height, Ordering::AcqRel);
    note_gc_fence_high_water(height);
    if height > prev {
        tracing::info!(
            "IBD engine GC fence advanced {} → {} (between exports — compaction may GC spent pairs)",
            prev,
            height
        );
        true
    } else {
        false
    }
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

/// Ceiling on directory prefix bits — a **RAM bound**, not a sizing policy.
///
/// `directory_prefix_bits` self-sizes to ~85 entries per bucket; this only caps how far
/// it may go. Whenever the cap binds, bucket width — and therefore bytes pread per point
/// lookup — grows linearly with segment size.
///
/// **20 was still binding.** R-273 live 340–370k averaged **29.9 KiB per pread** (max 273
/// KiB) against the ~4.7 KB design target, which is 587 GB read between 180k and 370k and
/// 1.13 GB/s sustained through the last band. Synthetic sweep at 100M entries in one
/// segment, varying only this ceiling:
///
/// | bits | KiB/pread | disk_ms | directory RAM |
/// |------|-----------|---------|---------------|
/// | 16   | 83.5      | 11.5    | 0.25 MiB      |
/// | 18   | 20.9      |  4.1    | 1 MiB         |
/// | 20   |  5.2      |  2.3    | 4 MiB         |
/// | 21   |  2.6      |  1.4    | 8 MiB         |
///
/// Every bit halves bytes read. 24 lets the formula self-size to ~1.4B entries and costs
/// at most 64 MiB per mega segment (4 B per bucket); only mega segments ever reach it.
///
/// Safe to change: `prefix_bits` is derived from `entry_count` on every directory build,
/// including segment load, and is never persisted.
pub(crate) const DIRECTORY_PREFIX_BITS_MAX: u32 = 20;

/// Sweep override for [`DIRECTORY_PREFIX_BITS_MAX`] (`BLVM_IBD_DIR_PREFIX_BITS_MAX`).
///
/// Safe to retune: `prefix_bits` is **derived** from `entry_count` every time a
/// directory is built, including [`Directory::build_streaming`] on segment load, and is
/// never persisted - there is no on-disk format to migrate.
pub(crate) fn directory_prefix_bits_max() -> u32 {
    std::env::var("BLVM_IBD_DIR_PREFIX_BITS_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DIRECTORY_PREFIX_BITS_MAX)
        .clamp(4, 28)
}

/// Target ~85 entries per bucket (85 × 56 B ≈ 4.7 KB).
pub(crate) fn directory_prefix_bits(entry_count: usize) -> u32 {
    if entry_count <= 128 {
        4
    } else {
        let ratio = (entry_count / 85).max(1);
        (usize::BITS - ratio.leading_zeros()).clamp(4, directory_prefix_bits_max())
    }
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
        let prefix_bits = directory_prefix_bits(n);
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

    /// Approximate bytes occupied by this directory in RAM.
    pub(super) fn mem_bytes(&self) -> usize {
        self.buckets.capacity() * 4
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
    /// segment header). Uses O(`2^prefix_bits`) memory — at most 4 MiB at 20 bits.
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
        let prefix_bits = directory_prefix_bits(entry_count);
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

    /// dest-bc pair GC: size the bloom to **actual** entries, not `COMPACT_MAX` capacity
    /// (20M-cap bloom for a 2M survivor chunk was leftover dest-bc index RAM).
    pub(super) fn build_streaming_with_bloom(
        reader: &mut super::disk_segment::SegmentReader,
        entry_count: usize,
    ) -> anyhow::Result<(Self, BloomFilter)> {
        if entry_count == 0 {
            return Ok((
                Self {
                    buckets: vec![0, 0],
                    prefix_bits: 1,
                },
                BloomFilter::new_for_capacity(1),
            ));
        }
        let prefix_bits = directory_prefix_bits(entry_count);
        let num_buckets = 1usize << prefix_bits;
        let mut bucket_start = vec![0u32; num_buckets + 1];
        let mut cur_bucket = 0usize;
        let mut i = 0usize;
        let mut filter = BloomFilter::new_for_capacity(entry_count);
        while let Some(kv) = reader.advance()? {
            filter.insert(&kv.key);
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
        Ok((
            Self {
                buckets: bucket_start,
                prefix_bits,
            },
            filter,
        ))
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
///
/// Note: F2 tried 16 bits/entry on DiskSegment only — no tip BPS win (HOLD/slight
/// regress vs 12-bit); kept at 12. Hot path cost is cold mega-segment I/O, not FPR.
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
    /// Approximate bytes occupied by this bloom filter in RAM.
    pub(super) fn mem_bytes(&self) -> usize {
        self.data.capacity() * 8
    }

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
#[derive(Debug)]
pub struct MemoryRun {
    pub(super) entries: Vec<OutputKV>,
    /// Sorted copy of the incoming append. Kept so the next block does not allocate it again.
    batch_scratch: Vec<OutputKV>,
    /// Destination of the tip merge. After `swap` with `entries` this holds the previous tip.
    merge_scratch: Vec<OutputKV>,
    pub(super) height_range: (i32, i32),
    pub(super) directory: Directory,
    pub(super) filter: BloomFilter,
    /// `true` while the run is the mutable tip (appends allowed). Frozen once pushed to `runs`.
    pub(super) is_mutable: bool,
}

impl Clone for MemoryRun {
    fn clone(&self) -> Self {
        // Derived Clone skipped Drop/build counters → MEMORY_RUN_LIVE went deeply negative
        // (observed −193k) while total kept climbing. Count clones as live instances.
        MEMORY_RUN_LIVE.fetch_add(1, Ordering::Relaxed);
        MEMORY_RUN_TOTAL.fetch_add(1, Ordering::Relaxed);
        // Slow-path append clones the tip outside the lock. The scratches are spare
        // capacity, not run state — copying them would allocate another tip buffer.
        Self {
            entries: self.entries.clone(),
            batch_scratch: Vec::new(),
            merge_scratch: Vec::new(),
            height_range: self.height_range,
            directory: self.directory.clone(),
            filter: self.filter.clone(),
            is_mutable: self.is_mutable,
        }
    }
}

impl Drop for MemoryRun {
    fn drop(&mut self) {
        MEMORY_RUN_LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

impl MemoryRun {
    /// Build a new `MemoryRun` from entries that may not be sorted.
    pub fn build(mut entries: Vec<OutputKV>) -> Self {
        entries.sort_unstable();
        let height_range = height_range_of(&entries);
        let directory = Directory::build(&entries);
        let filter = BloomFilter::build(&entries);
        MEMORY_RUN_LIVE.fetch_add(1, Ordering::Relaxed);
        MEMORY_RUN_TOTAL.fetch_add(1, Ordering::Relaxed);
        Self {
            entries,
            batch_scratch: Vec::new(),
            merge_scratch: Vec::new(),
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
        MEMORY_RUN_LIVE.fetch_add(1, Ordering::Relaxed);
        MEMORY_RUN_TOTAL.fetch_add(1, Ordering::Relaxed);
        Self {
            entries,
            batch_scratch: Vec::new(),
            merge_scratch: Vec::new(),
            height_range,
            directory,
            filter,
            is_mutable: false,
        }
    }

    /// Build an empty mutable run for the tip age (block-by-block append target).
    pub fn new_mutable() -> Self {
        MEMORY_RUN_LIVE.fetch_add(1, Ordering::Relaxed);
        MEMORY_RUN_TOTAL.fetch_add(1, Ordering::Relaxed);
        Self {
            entries: Vec::new(),
            batch_scratch: Vec::new(),
            merge_scratch: Vec::new(),
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

    /// Approximate resident memory in bytes: entries Vec + merge scratches + bloom + directory.
    pub fn mem_bytes(&self) -> usize {
        let entries_bytes = (self.entries.capacity()
            + self.batch_scratch.capacity()
            + self.merge_scratch.capacity())
            * super::types::OutputKV::SIZE;
        entries_bytes + self.filter.mem_bytes() + self.directory.mem_bytes()
    }

    /// Append entries (for the mutable tip run only). Sorts in place and rebuilds acceleration structures.
    ///
    /// Called from `MemoryAge::append` while holding the write lock.
    pub fn append_and_rebuild(&mut self, new_entries: &[OutputKV]) {
        debug_assert!(self.is_mutable, "cannot append to frozen run");
        if new_entries.is_empty() {
            return;
        }
        if self.entries.is_empty() {
            self.entries.extend_from_slice(new_entries);
            self.entries.sort_unstable();
        } else {
            merge_sorted_output_kvs(
                &mut self.entries,
                &mut self.batch_scratch,
                &mut self.merge_scratch,
                new_entries,
            );
        }
        self.height_range = height_range_of(&self.entries);
        // Mutable tip: skip directory/bloom rebuild every block — lookup uses direct
        // binary search on sorted entries. Structures are built once on freeze().
    }

    /// Freeze the mutable run (called by `MemoryAge` before creating a new mutable run).
    pub fn freeze(&mut self) {
        self.is_mutable = false;
        self.directory = Directory::build(&self.entries);
        self.filter = BloomFilter::build(&self.entries);
        // Spare buffers are only for the mutable tip. Drop them so a frozen run
        // does not keep a second copy of the tip.
        self.batch_scratch.clear();
        self.batch_scratch.shrink_to_fit();
        self.merge_scratch.clear();
        self.merge_scratch.shrink_to_fit();
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
        let slice = if self.is_mutable {
            // Mutable tip: entries are sorted; bloom/dir are stale until freeze().
            &self.entries
        } else {
            if !self.filter.may_contain(key) {
                return None;
            }
            let (lo, hi) = self.directory.lookup_range(key);
            if lo >= hi {
                return None;
            }
            &self.entries[lo..hi]
        };
        // Binary search for first entry with key >= target.
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
    /// Parallel when `keys.len() >= RAYON_BATCH_THRESHOLD` and rayon is enabled.
    /// `OUTPUT_ID_DELETED` is treated as resolved (same as a real id ≠ MAX).
    ///
    /// N22 `remaining` counter REVERT on synth dens (S10 floor ~189 vs champ 197.9) —
    /// callers early-exit with short-circuit `any(MAX)` instead.
    pub fn batch_lookup(&self, keys: &[[u8; 36]], ids: &mut [OutputId], since: i32, before: i32) {
        debug_assert_eq!(keys.len(), ids.len());
        let lookup_one = |key: &[u8; 36], id: &mut OutputId| {
            if *id == OutputId::MAX {
                if let Some(found) = self.lookup_key(key, since, before) {
                    *id = found; // real id or OUTPUT_ID_DELETED
                }
            }
        };
        // Opt out: `BLVM_IBD_QUERY_RAYON=0` — serial lookup.
        #[cfg(feature = "rayon")]
        if keys.len() >= RAYON_BATCH_THRESHOLD && query_rayon_enabled() {
            use rayon::prelude::*;
            keys.par_iter()
                .zip(ids.par_iter_mut())
                .for_each(|(key, id)| lookup_one(key, id));
            return;
        }
        for (key, id) in keys.iter().zip(ids.iter_mut()) {
            lookup_one(key, id);
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

        // GC often cancels a large fraction of entries; without shrink, capacity stays at
        // sum(source lengths) and inflates ENGINE_MEM / jemalloc large bins for the life of
        // the frozen run (and any slow-path tip clones that copy that Vec).
        gc.shrink_to_fit();

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

/// Merge `new_entries` into sorted `base` without a full re-sort of `base`.
///
/// `batch_scratch` and `merge_scratch` are reused across appends. Callers must not
/// shrink them here — the next block needs the capacity. `freeze` drops them.
fn merge_sorted_output_kvs(
    base: &mut Vec<OutputKV>,
    batch_scratch: &mut Vec<OutputKV>,
    merge_scratch: &mut Vec<OutputKV>,
    new_entries: &[OutputKV],
) {
    if new_entries.is_empty() {
        return;
    }
    batch_scratch.clear();
    batch_scratch.extend_from_slice(new_entries);
    batch_scratch.sort_unstable();
    if base.is_empty() {
        std::mem::swap(base, batch_scratch);
        return;
    }
    merge_scratch.clear();
    merge_scratch.reserve(base.len() + batch_scratch.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < base.len() && j < batch_scratch.len() {
        if base[i] <= batch_scratch[j] {
            merge_scratch.push(base[i]);
            i += 1;
        } else {
            merge_scratch.push(batch_scratch[j]);
            j += 1;
        }
    }
    if i < base.len() {
        merge_scratch.extend_from_slice(&base[i..]);
    }
    if j < batch_scratch.len() {
        merge_scratch.extend_from_slice(&batch_scratch[j..]);
    }
    std::mem::swap(base, merge_scratch);
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

    #[serial_test::serial(ibd)]
    #[test]
    fn test_advance_gc_fence_monotonic() {
        set_gc_fence(260_000);
        advance_gc_fence_to(250_000);
        assert_eq!(gc_fence_snapshot(), 260_000);
        advance_gc_fence_to(270_000);
        assert_eq!(gc_fence_snapshot(), 270_000);
        advance_gc_fence_to(265_000);
        assert_eq!(gc_fence_snapshot(), 270_000);
        set_gc_fence(i32::MAX);
    }

    /// R-352: between exports the fence follows validation unless an export scan is active;
    /// the high-water mark remembers the highest finite fence so a later export cannot be
    /// labelled below a height whose spent pairs may already be GC'd.
    #[serial_test::serial(ibd)]
    #[test]
    fn r352_fence_follows_validation_between_exports_and_keeps_high_water() {
        set_gc_fence(300_000);
        let hw0 = gc_fence_high_water();
        assert!(hw0 >= 300_000);
        // Export scan running: no advance.
        set_next_checkpoint_target(400_000);
        assert!(!advance_gc_fence_between_exports(310_000, true));
        assert_eq!(gc_fence_snapshot(), 300_000);
        // Unknown target: no advance.
        set_next_checkpoint_target(0);
        assert!(!advance_gc_fence_between_exports(310_000, false));
        assert_eq!(gc_fence_snapshot(), 300_000);
        // No export: advance, and the high-water follows.
        set_next_checkpoint_target(400_000);
        assert!(advance_gc_fence_between_exports(310_000, false));
        assert_eq!(gc_fence_snapshot(), 310_000);
        assert!(gc_fence_high_water() >= 310_000);
        // Never past the next scheduled checkpoint (lagged export at 400k must stay valid).
        assert!(advance_gc_fence_between_exports(450_000, false));
        assert_eq!(gc_fence_snapshot(), 400_000);
        assert!(!advance_gc_fence_between_exports(460_000, false));
        assert_eq!(gc_fence_snapshot(), 400_000);
        set_gc_fence(310_000);
        // Monotonic.
        assert!(!advance_gc_fence_between_exports(305_000, false));
        assert_eq!(gc_fence_snapshot(), 310_000);
        // A refused export lowers the live fence but not the high-water.
        set_gc_fence(300_000);
        assert_eq!(gc_fence_snapshot(), 300_000);
        assert!(gc_fence_high_water() >= 310_000);
        // "No fence" is not a high-water.
        set_gc_fence(i32::MAX);
        assert!(gc_fence_high_water() < i32::MAX);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn test_bloom_no_false_negatives() {
        let keys: Vec<[u8; 36]> = (0..100).map(make_key).collect();
        let entries: Vec<OutputKV> = keys.iter().map(|k| OutputKV::new_add(*k, 1, 42)).collect();
        let bloom = BloomFilter::build(&entries);
        for k in &keys {
            assert!(bloom.may_contain(k), "false negative for key {:?}", k[0]);
        }
    }

    #[serial_test::serial(ibd)]
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

    #[serial_test::serial(ibd)]
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

    /// dest-bc 660k avg 251 KiB pread with `preads≈cands` is a 16-bit mega-seg bucket,
    /// not F19 glue. 20 bits brings that size back to dest-ba ~16 KiB.
    #[test]
    fn dest_bc_660k_megaseg_prefix_bits_20_matches_dest_ba_bucket() {
        let kv = OutputKV::SIZE;
        // dest-ba 660k ~15 KiB buckets stay under the old 16-bit clamp.
        let small = directory_prefix_bits(10_000);
        assert!(
            small <= 16,
            "10k-entry segs must not take the dest-bc mega clamp"
        );

        // dest-bc 251 KiB avg ≈ 300M uniform entries at 16 bits.
        let n = 300_000_000usize;
        // R-276 raised this ceiling to 24 so 300M would self-size to 22 instead of
        // clamping. Live 340-370k went the wrong way (29.9 -> 34.1 KiB/pread, wall
        // 1580s -> 1900s): the ceiling was not what bound the live mega-seg, so the
        // synthetic model above does not describe it. Back at 20, which clamps.
        assert_eq!(DIRECTORY_PREFIX_BITS_MAX, 20);
        assert_eq!(directory_prefix_bits(n), DIRECTORY_PREFIX_BITS_MAX);
        let bucket_16 = n / (1usize << 16);
        let bucket_20 = n / (1usize << 20);
        let kb_16 = bucket_16 * kv / 1024;
        let kb_20 = bucket_20 * kv / 1024;
        assert!(
            kb_16 >= 200,
            "16-bit clamp must reproduce dest-bc 251 KiB buckets, got {kb_16}"
        );
        assert!(
            kb_20 <= 20,
            "20-bit clamp must match dest-ba ~15 KiB buckets, got {kb_20}"
        );
        // 20M-entry spill (common mega) was 17 KiB @16 bits; 20 bits keeps ~4 KiB target.
        let n20m = 20_000_000usize;
        let bits_20m = directory_prefix_bits(n20m);
        assert!(bits_20m >= 18);
        let kb_20m = n20m / (1usize << bits_20m) * kv / 1024;
        assert!(kb_20m <= 8, "20M-entry mega must stay ~4 KiB, got {kb_20m}");

        // dest-bc leftover index RAM: 7×300M blooms vs dest-ba-like 20M chunks.
        let bloom_mega = BloomFilter::new_for_capacity(n).mem_bytes();
        let bloom_20m = BloomFilter::new_for_capacity(n20m).mem_bytes();
        let bloom_2m = BloomFilter::new_for_capacity(2_000_000).mem_bytes();
        assert!(
            bloom_mega >= 400 * 1024 * 1024,
            "300M dest-bc mega bloom must be ~450 MiB, got {bloom_mega}"
        );
        assert!(
            bloom_20m * 10 < bloom_mega,
            "20M compact-cap bloom must be ≪ mega bloom: 20M={bloom_20m} mega={bloom_mega}"
        );
        assert!(
            bloom_2m < bloom_20m,
            "pair-GC survivor bloom must not inherit 20M cap: 2M={bloom_2m} 20M={bloom_20m}"
        );
    }

    #[serial_test::serial(ibd)]
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

    #[serial_test::serial(ibd)]
    #[test]
    fn test_lookup_height_window() {
        let k = make_key(1);
        let entries = vec![OutputKV::new_add(k, 100, 42)];
        let run = MemoryRun::build(entries);
        assert_eq!(run.lookup_key(&k, 0, 101), Some(42));
        // Height 100 is outside [101, MAX) window — should not be found.
        assert_eq!(run.lookup_key(&k, 101, i32::MAX), None);
    }

    #[serial_test::serial(ibd)]
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

    #[serial_test::serial(ibd)]
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

    #[serial_test::serial(ibd)]
    #[test]
    fn test_merge_cancellation_cross_height() {
        // Other --lib tests mutate the process-wide fence (`set_gc_fence` / `UtxoDatabase::open`).
        set_gc_fence(i32::MAX);
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

    #[serial_test::serial(ibd)]
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

    #[serial_test::serial(ibd)]
    #[test]
    fn test_merge_dangling_delete_preserved() {
        let k = make_key(1);
        // Delete only (Add was already evicted to disk) — must survive to shadow disk Add.
        let run_a = Arc::new(MemoryRun::build(vec![OutputKV::new_delete(k, 200)]));
        let merged = MemoryRun::merge(&[run_a]);
        assert_eq!(merged.entries.len(), 1);
        assert!(merged.entries[0].is_delete());
    }

    #[serial_test::serial(ibd)]
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

    #[serial_test::serial(ibd)]
    #[test]
    fn test_mutable_append_reuses_merge_scratch() {
        fn add(n: u8, height: i32) -> OutputKV {
            OutputKV::new_add(make_key(n), height, n as u64)
        }

        let mut run = MemoryRun::new_mutable();
        // Empty tip sorts in place and does not merge.
        run.append_and_rebuild(&[add(30, 1), add(10, 1), add(20, 1)]);
        assert!(run.entries.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(run.merge_scratch.capacity(), 0);

        // `reserve` doubles. Once `merge_scratch` already fits the next tip, that
        // buffer is swapped into `entries` and its capacity must stay put.
        let mut saw_stable = false;
        for n in 4u8..40 {
            let batch = [add(n.wrapping_mul(7).wrapping_add(1), i32::from(n))];
            let spare = run.merge_scratch.capacity();
            let need = run.entries.len() + batch.len();
            run.append_and_rebuild(&batch);
            assert!(
                run.entries.windows(2).all(|w| w[0] <= w[1]),
                "append {n} broke sort"
            );
            if spare >= need {
                assert_eq!(
                    run.entries.capacity(),
                    spare,
                    "spare fit ({spare} >= {need}) but the merge buffer grew"
                );
                saw_stable = true;
                break;
            }
        }
        assert!(saw_stable, "no merge reused merge_scratch");

        let cloned = run.clone();
        assert_eq!(cloned.entries, run.entries);
        assert_eq!(cloned.batch_scratch.capacity(), 0);
        assert_eq!(cloned.merge_scratch.capacity(), 0);

        assert!(run.merge_scratch.capacity() > 0 || run.batch_scratch.capacity() > 0);
        run.freeze();
        assert!(!run.is_mutable);
        assert_eq!(run.batch_scratch.capacity(), 0);
        assert_eq!(run.merge_scratch.capacity(), 0);
        assert_eq!(run.lookup_key(&make_key(10), 0, i32::MAX), Some(10));
    }
}
