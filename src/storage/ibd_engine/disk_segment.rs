//! `DiskSegment`: an immutable, sorted `OutputKV` run evicted from the age-tiered memory index.
//!
//! When the deepest memory age overflows (K_AGES-1 fills to K_FAN_IN runs), the merged result
//! is written here instead of being dropped. Memory is freed; the bloom filter and directory
//! are kept in RAM (~2 MB per segment for 1M entries) for fast lookup routing.
//!
//! ## File format
//! ```text
//! [8 bytes]  magic = DISK_SEG_MAGIC (little-endian)
//! [4 bytes]  entry_count (u32, little-endian)
//! [4 bytes]  min_height  (i32, little-endian)
//! [4 bytes]  max_height  (i32, little-endian)
//! [4 bytes]  padding
//! [entry_count × OutputKV::SIZE bytes]  sorted entries (repr(C), written raw)
//! ```
//!
//! ## Lookup
//! 1. Bloom filter check (in RAM, 7 probes) — cheap O(1) miss short-circuit.
//! 2. Directory lookup — narrows to a ~4 KB bucket range.
//! 3. `pread64` of the bucket from disk — lock-free, parallel-safe.
//! 4. Binary search + scan within the bucket bytes.

use super::file_io;
use super::memory_run::{BloomFilter, Directory};
use super::types::{OUTPUT_ID_DELETED, OutputId, OutputKV};
use std::cell::{Cell, RefCell};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Cold `batch_lookup` candidate (TLS-reused across segs/blocks).
struct Candidate {
    lo: usize,
    hi: usize,
    idx: usize,
    key: [u8; 36],
}

/// Merged pread/mmap range covering candidates[ci..cj].
struct Range {
    lo: usize,
    hi: usize,
    ci: usize,
    cj: usize,
}

thread_local! {
    /// Accumulators for one `DiskIndex::batch_query` (reset/take there).
    static ACC_DISK_PREADS: Cell<u64> = const { Cell::new(0) };
    static ACC_DISK_PREAD_BYTES: Cell<u64> = const { Cell::new(0) };
    static ACC_DISK_MAX_PREAD: Cell<u64> = const { Cell::new(0) };
    static ACC_DISK_CANDS: Cell<u64> = const { Cell::new(0) };
    static ACC_DISK_SEGS: Cell<u64> = const { Cell::new(0) };
    /// S0: last `write_from_iter` pass split (ms).
    static ACC_WRITE_PASS1_MS: Cell<u64> = const { Cell::new(0) };
    static ACC_WRITE_DIR_MS: Cell<u64> = const { Cell::new(0) };
    /// C7: reuse cold-path shells (candidates / ranges / pread decode).
    static TLS_CANDIDATES: RefCell<Vec<Candidate>> = const { RefCell::new(Vec::new()) };
    static TLS_RANGES: RefCell<Vec<Range>> = const { RefCell::new(Vec::new()) };
    static TLS_BUCKET: RefCell<Vec<OutputKV>> = const { RefCell::new(Vec::new()) };
    static TLS_RAW: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Reset S0 `write_from_iter` timers (call before compact tee write).
pub fn reset_write_from_iter_stats() {
    ACC_WRITE_PASS1_MS.with(|c| c.set(0));
    ACC_WRITE_DIR_MS.with(|c| c.set(0));
}

/// `(pass1_ms, directory_ms)` from the last `write_from_iter` on this thread.
pub fn take_write_from_iter_stats() -> (u64, u64) {
    (
        ACC_WRITE_PASS1_MS.with(Cell::get),
        ACC_WRITE_DIR_MS.with(Cell::get),
    )
}

/// Reset per-query DiskSegment I/O counters (call at start of `DiskIndex::batch_query`).
pub fn reset_disk_io_stats() {
    ACC_DISK_PREADS.with(|c| c.set(0));
    ACC_DISK_PREAD_BYTES.with(|c| c.set(0));
    ACC_DISK_MAX_PREAD.with(|c| c.set(0));
    ACC_DISK_CANDS.with(|c| c.set(0));
    ACC_DISK_SEGS.with(|c| c.set(0));
}

/// `(preads, pread_kb, max_pread_kb, cands, segs_touched)` since last reset.
pub fn take_disk_io_stats() -> (u64, u64, u64, u64, u64) {
    let preads = ACC_DISK_PREADS.with(Cell::get);
    let bytes = ACC_DISK_PREAD_BYTES.with(Cell::get);
    let max_b = ACC_DISK_MAX_PREAD.with(Cell::get);
    let cands = ACC_DISK_CANDS.with(Cell::get);
    let segs = ACC_DISK_SEGS.with(Cell::get);
    (preads, bytes / 1024, max_b / 1024, cands, segs)
}

fn note_pread(byte_count: usize) {
    let n = byte_count as u64;
    ACC_DISK_PREADS.with(|c| c.set(c.get() + 1));
    ACC_DISK_PREAD_BYTES.with(|c| c.set(c.get() + n));
    ACC_DISK_MAX_PREAD.with(|c| {
        if n > c.get() {
            c.set(n);
        }
    });
}

const DISK_SEG_MAGIC: u64 = 0xD15C_DEAD_B10C_0001;
const HEADER_SIZE: u64 = 24; // magic(8) + count(4) + min_h(4) + max_h(4) + pad(4)
pub(super) const HEADER_SIZE_USIZE: usize = HEADER_SIZE as usize;

/// Opt-in: `BLVM_IBD_DISK_BUCKET_WILLNEED=1` → `posix_fadvise(WILLNEED)` on each
/// merged bucket range **before** `pread` in `batch_lookup` (F5b). Distinct from
/// whole-segment WILLNEED after write (F3), which did not help tip BPS.
/// Default-on-with-SEGMENT_WILLNEED REVERT S10 187.9 vs champ 197.9 (2026-07-31).
fn bucket_willneed_from_env() -> bool {
    matches!(
        std::env::var("BLVM_IBD_DISK_BUCKET_WILLNEED")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Opt-in: `BLVM_IBD_DISK_PARALLEL_PREAD=1` → rayon-parallel `pread` of merged
/// bucket ranges when there are ≥8 ranges (F5c volume-mode tip outliers).
fn disk_parallel_pread_from_env() -> bool {
    matches!(
        std::env::var("BLVM_IBD_DISK_PARALLEL_PREAD")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

const PARALLEL_PREAD_MIN_RANGES: usize = 8;

/// F19: cap merged DiskIndex pread span (KiB). Empty/unset/`0` = unlimited (legacy glue).
///
/// dest-bc 660k HOTPATH is **not** F19 glue: `preads≈cands` (6374 vs 6698) at 251 KiB
/// average — each candidate's directory bucket is already that wide. F19 never shrinks a
/// single `[lo,hi)`. July F19 HOLD used 4096 KiB and did not move the tip wall. The later-
/// height lever is `directory_prefix_bits` max 20 (mega-seg buckets), not a default cap.
/// `BLVM_IBD_DISK_PREAD_MAX_KB=64` remains opt-in for true adjacent-bucket glue.
pub(crate) const DISK_PREAD_MAX_KB_DEFAULT: u64 = 0;

/// Adjacent directory buckets are still coalesced until the merged entry span would
/// exceed this many KiB; a single candidate's `[lo,hi)` is never shrunk.
pub(crate) fn disk_pread_max_kb_from_env() -> u64 {
    std::env::var("BLVM_IBD_DISK_PREAD_MAX_KB")
        .ok()
        .and_then(|s| {
            let t = s.trim();
            if t.is_empty() { None } else { t.parse().ok() }
        })
        .unwrap_or(DISK_PREAD_MAX_KB_DEFAULT)
}

pub(crate) fn disk_pread_max_entries() -> usize {
    let kb = disk_pread_max_kb_from_env();
    if kb == 0 {
        return usize::MAX;
    }
    let bytes = kb.saturating_mul(1024);
    (bytes as usize / OutputKV::SIZE).max(1)
}

/// R-342: single-candidate directory buckets wider than this many KiB are resolved with
/// a page-wise binary search ([`probe_narrow`]) instead of one whole-bucket `pread`.
/// Default **64**; `0` disables (legacy whole-bucket read). Env `BLVM_IBD_DISK_PROBE_KB`.
///
/// The directory prefix is the first 4 bytes of the txid, so every output of one
/// transaction lands in one bucket regardless of `prefix_bits`. 2015 fan-out transactions
/// (thousands of outputs) make 200–440 KiB buckets: R-341 `[IBD_HOTPATH]` at 340–370k read
/// 14–32 MB per block in 400–670 preads averaging 35–47 KiB, `max_pread_kb` 166–437, and
/// the cold ones were the engine tail (h=358000 `disk_ms=308`). A probe reads
/// `log2(bucket/page)` 4 KiB pages plus a ≤ 5-page window, so a 400 KiB bucket costs
/// ~7 × 4 KiB + 20 KiB instead of 400 KiB.
pub(crate) const DISK_PROBE_KB_DEFAULT: u64 = 64;

pub(crate) fn disk_probe_min_entries() -> usize {
    static CACHED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let kb = std::env::var("BLVM_IBD_DISK_PROBE_KB")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(DISK_PROBE_KB_DEFAULT);
        if kb == 0 {
            return usize::MAX;
        }
        ((kb.saturating_mul(1024)) as usize / OutputKV::SIZE).max(1)
    })
}

/// Entries per probe page (4 KiB of `OutputKV`).
pub(crate) const PROBE_PAGE_ENTRIES: usize = 4096 / OutputKV::SIZE;

/// R-345: probing `k` candidates costs about `k × (log2(pages) + 3)` page reads
/// (binary search + a ≤ 3-page window); true when that is under the span's own page count.
pub(crate) fn probe_cheaper_than_span(k: usize, span_entries: usize, page: usize) -> bool {
    let page = page.max(1);
    let pages = span_entries / page + 1;
    let per = (usize::BITS - pages.leading_zeros()) as usize + 3;
    k.saturating_mul(per) < pages
}

/// Page-wise binary search inside one sorted bucket `[lo, hi)`.
///
/// `read_page(a, b)` returns the first and last key of entries `[a, b)` (`a < b`). Returns a
/// sub-range that contains every entry whose key equals `key`, assuming a run of equal keys
/// never exceeds `page` entries (a UTXO key has at most a few Add/Delete entries per segment).
/// Invariant kept per step: entries below the range are `< key`, entries above are `> key`.
/// The result is padded by one page on each side so a run straddling a page edge is whole.
pub(crate) fn probe_narrow<F>(
    lo: usize,
    hi: usize,
    page: usize,
    key: &[u8; 36],
    mut read_page: F,
) -> anyhow::Result<(usize, usize)>
where
    F: FnMut(usize, usize) -> anyhow::Result<([u8; 36], [u8; 36])>,
{
    let page = page.max(1);
    let (mut a, mut b) = (lo, hi);
    while b.saturating_sub(a) > 3 * page {
        // Page-aligned relative to `a`; `(b - a) / 2 >= 1.5 page` so `mid > a` and `mid < b`.
        let mid = a + ((b - a) / 2 / page) * page;
        let mid_end = (mid + page).min(b);
        let (first, last) = read_page(mid, mid_end)?;
        if last < *key {
            a = mid_end;
        } else if first > *key {
            b = mid;
        } else {
            a = mid;
            b = mid_end;
            break;
        }
    }
    Ok((
        a.saturating_sub(page).max(lo),
        b.saturating_add(page).min(hi),
    ))
}

/// Coalesce sorted `[lo, hi)` spans. Shared by `coalesce_pread_spans` and cold `batch_lookup`.
///
/// Merge while the next span overlaps or abuts and the combined entry count is
/// `≤ max_entries`. A single span is never shrunk. `usize::MAX` is unlimited.
/// `push(lo, hi, ci, cj)` covers input indices `[ci, cj)`.
#[inline(always)]
fn coalesce_sorted_spans<L, P>(len: usize, lo_hi: L, max_entries: usize, mut push: P)
where
    L: Fn(usize) -> (usize, usize),
    P: FnMut(usize, usize, usize, usize),
{
    let mut ci = 0;
    while ci < len {
        let (read_lo, mut read_hi) = lo_hi(ci);
        let mut cj = ci + 1;
        while cj < len {
            let (next_lo, next_hi) = lo_hi(cj);
            if next_lo > read_hi {
                break;
            }
            let new_hi = read_hi.max(next_hi);
            if new_hi.saturating_sub(read_lo) > max_entries {
                break;
            }
            read_hi = new_hi;
            cj += 1;
        }
        push(read_lo, read_hi, ci, cj);
        ci = cj;
    }
}

/// Coalesce sorted candidate `[lo, hi)` spans into pread ranges.
///
/// Returns `(lo, hi, ci, cj)` covering `sorted_lo_hi[ci..cj]`.
pub(crate) fn coalesce_pread_spans(
    sorted_lo_hi: &[(usize, usize)],
    max_entries: usize,
) -> Vec<(usize, usize, usize, usize)> {
    let mut out = Vec::new();
    coalesce_sorted_spans(
        sorted_lo_hi.len(),
        |i| sorted_lo_hi[i],
        max_entries,
        |lo, hi, ci, cj| out.push((lo, hi, ci, cj)),
    );
    out
}

fn advise_willneed_range(file: &File, byte_offset: u64, byte_count: usize) {
    if byte_count == 0 {
        return;
    }
    #[cfg(all(unix, feature = "libc"))]
    {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::posix_fadvise(
                file.as_raw_fd(),
                byte_offset as libc::off_t,
                byte_count as libc::off_t,
                libc::POSIX_FADV_WILLNEED,
            );
        }
    }
    #[cfg(not(all(unix, feature = "libc")))]
    {
        let _ = (file, byte_offset, byte_count);
    }
}

/// Compact input stream only. Default off (R-368 view regression).
/// `BLVM_IBD_COMPACT_DONTNEED=1/on/true` enables it.
fn compact_dontneed_enabled() -> bool {
    matches!(
        std::env::var("BLVM_IBD_COMPACT_DONTNEED")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("on") | Some("true") | Some("yes")
    )
}

/// One line when the UTXO engine opens, so a dest log shows the default without a compact.
pub(super) fn log_compact_dontneed_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::info!(
            "[IBD_COMPACT_DONTNEED] compact input DONTNEED {}",
            if compact_dontneed_enabled() {
                "on"
            } else {
                "off"
            }
        );
    });
}

#[cfg(test)]
static TEST_DONTNEED_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Drop a sequential compact-input range from the page cache. Failure is non-fatal.
fn advise_dontneed_range(file: &File, byte_offset: u64, byte_count: usize) {
    if byte_count == 0 {
        return;
    }
    #[cfg(test)]
    {
        TEST_DONTNEED_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(all(unix, feature = "libc"))]
    {
        use std::os::unix::io::AsRawFd;
        let _rc = unsafe {
            libc::posix_fadvise(
                file.as_raw_fd(),
                byte_offset as libc::off_t,
                byte_count as libc::off_t,
                libc::POSIX_FADV_DONTNEED,
            )
        };
        let _ = _rc;
    }
    #[cfg(not(all(unix, feature = "libc")))]
    {
        let _ = (file, byte_offset, byte_count);
    }
}

/// After a mega spill, validation often faults cold `pread`s on the new segment
/// (F1: disk_ms spikes; F2 denser bloom did not help). Opt-in page-cache warm:
/// `BLVM_IBD_SEGMENT_WILLNEED=1` and entry_count ≥ min → background `posix_fadvise(WILLNEED)`.
/// Default min 20M (5M REVERT S10 187.8 vs champ 197.9). Override:
/// `BLVM_IBD_SEGMENT_WILLNEED_MIN_ENTRIES`.
fn maybe_warm_segment_pages(file: &File, entry_count: usize) {
    let min_entries = std::env::var("BLVM_IBD_SEGMENT_WILLNEED_MIN_ENTRIES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000_000usize);
    if entry_count < min_entries {
        return;
    }
    match std::env::var("BLVM_IBD_SEGMENT_WILLNEED").ok().as_deref() {
        Some("1") | Some("true") | Some("yes") => {}
        _ => return,
    }
    let byte_len = HEADER_SIZE + (entry_count as u64) * (OutputKV::SIZE as u64);
    #[cfg(all(unix, feature = "libc"))]
    {
        use std::os::unix::io::AsRawFd;
        let raw = file.as_raw_fd();
        let dup = unsafe { libc::dup(raw) };
        if dup < 0 {
            return;
        }
        let _ = std::thread::Builder::new()
            .name("utxo-seg-willneed".into())
            .spawn(move || {
                // Chunked hint — kernel may ignore under MemoryHigh pressure.
                const CHUNK: u64 = 256 * 1024 * 1024;
                let mut off = 0u64;
                while off < byte_len {
                    let n = CHUNK.min(byte_len - off);
                    unsafe {
                        libc::posix_fadvise(
                            dup,
                            off as libc::off_t,
                            n as libc::off_t,
                            libc::POSIX_FADV_WILLNEED,
                        );
                    }
                    off += n;
                }
                unsafe {
                    libc::close(dup);
                }
                tracing::info!(
                    "DiskSegment: posix_fadvise(WILLNEED) advised bytes={} entries={}",
                    byte_len,
                    (byte_len.saturating_sub(HEADER_SIZE)) / OutputKV::SIZE as u64
                );
            });
    }
    #[cfg(not(all(unix, feature = "libc")))]
    {
        let _ = (file, byte_len);
    }
}

/// Opt-in file-backed mmap for segment entry bytes (`BLVM_IBD_DISK_MMAP=1`).
///
/// F5d: F2 denser bloom failed (tip cost is true disk hits after spill, not FPR).
/// File-backed maps are reclaimable and excluded from `RssAnon` MemoryGuard pressure.
fn disk_mmap_from_env() -> bool {
    matches!(
        std::env::var("BLVM_IBD_DISK_MMAP")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Opt-in: keep newest mega segment's `OutputKV` body in RAM after spill (`BLVM_IBD_HOT_PIN=1`).
/// F10: tip DiskIndex cost is body pread after bloom already resident — pin avoids pread.
pub(super) fn hot_pin_from_env() -> bool {
    matches!(
        std::env::var("BLVM_IBD_HOT_PIN")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

fn hot_pin_min_entries() -> usize {
    // Default 20M (5M REVERT S10 184.8 vs champ 197.9). Override via env.
    std::env::var("BLVM_IBD_HOT_PIN_MIN_ENTRIES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000_000)
}

fn hot_pin_max_entries() -> usize {
    std::env::var("BLVM_IBD_HOT_PIN_MAX_ENTRIES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(150_000_000)
}

/// How many mega segments may keep a HotPin body at once (default 1 = F10).
/// With keep-oldest trim, `MAX_SEGS=2` retains seed + newest spill (score H4/S2).
/// F16 dual-newest and S1 largest-first both REVERT’d (seed thrash).
pub(super) fn hot_pin_max_segs() -> usize {
    std::env::var("BLVM_IBD_HOT_PIN_MAX_SEGS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .clamp(1, 8)
}

pub(super) fn hot_pin_eligible(entry_count: usize) -> bool {
    hot_pin_from_env()
        && entry_count >= hot_pin_min_entries()
        && entry_count <= hot_pin_max_entries()
}

/// Read-only `mmap` of a segment file (header + entries).
struct SegMmap {
    ptr: *mut u8,
    len: usize,
}

// mmap region is shared read-only across worker threads.
unsafe impl Send for SegMmap {}
unsafe impl Sync for SegMmap {}

impl SegMmap {
    #[cfg(all(unix, feature = "libc"))]
    fn map_file(file: &File, len: usize) -> Option<Arc<Self>> {
        if len == 0 {
            return None;
        }
        use std::os::unix::io::AsRawFd;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            tracing::warn!(
                "DiskSegment: mmap failed len={} — falling back to pread",
                len
            );
            return None;
        }
        Some(Arc::new(Self {
            ptr: ptr as *mut u8,
            len,
        }))
    }

    #[cfg(not(all(unix, feature = "libc")))]
    fn map_file(_file: &File, _len: usize) -> Option<Arc<Self>> {
        None
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for SegMmap {
    fn drop(&mut self) {
        #[cfg(all(unix, feature = "libc"))]
        if !self.ptr.is_null() && self.len > 0 {
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, self.len);
            }
        }
    }
}

fn maybe_mmap_segment(file: &File, entry_count: usize) -> Option<Arc<SegMmap>> {
    if !disk_mmap_from_env() {
        return None;
    }
    let len = HEADER_SIZE as usize + entry_count.saturating_mul(OutputKV::SIZE);
    let mapped = SegMmap::map_file(file, len)?;
    tracing::info!(
        "DiskSegment: mmap enabled bytes={} entries={}",
        len,
        entry_count
    );
    Some(mapped)
}

pub struct DiskSegment {
    pub(super) path: PathBuf,
    pub(super) height_range: (i32, i32),
    pub(super) entry_count: usize,
    /// In-memory bloom filter (~12 bits/entry). Used for fast misses.
    filter: BloomFilter,
    /// In-memory directory (prefix buckets). Narrows binary search to ~4 KB.
    directory: Directory,
    /// Lock-free read handle. `pread64` is thread-safe on Linux.
    file: Arc<File>,
    /// Optional file-backed mmap of header+entries (`BLVM_IBD_DISK_MMAP=1`).
    mmap: Option<Arc<SegMmap>>,
    /// F10: optional pinned body for RAM `batch_lookup` (newest mega seg only).
    /// Interior mutability so `DiskIndex` can clear under memory pressure.
    hot_body: parking_lot::RwLock<Option<Arc<[OutputKV]>>>,
}

impl DiskSegment {
    /// Write `run` to `{seg_dir}/seg_{idx:06}.bin` and return the opened segment.
    ///
    /// The run must be already sorted and frozen (built by `MemoryRun::merge`).
    pub fn write(
        seg_dir: &Path,
        idx: usize,
        run: &super::memory_run::MemoryRun,
    ) -> anyhow::Result<Self> {
        // Clone path for callers that only have `&MemoryRun` (tests / legacy).
        Self::write_owned(
            seg_dir,
            idx,
            run.height_range,
            run.entries.clone(),
            /* pin */ false,
        )
    }

    /// Write entries to a new segment. When `pin` is true, upgrades `entries` into
    /// `hot_body` via `Arc::from(Vec)` (no extra copy) for RAM lookups.
    pub fn write_owned(
        seg_dir: &Path,
        idx: usize,
        height_range: (i32, i32),
        entries: Vec<OutputKV>,
        pin: bool,
    ) -> anyhow::Result<Self> {
        let path = seg_dir.join(format!("seg_{idx:06}.bin"));
        let entry_count = entries.len();

        // Write header + entries to disk.
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            f.write_all(&DISK_SEG_MAGIC.to_le_bytes())?;
            f.write_all(&(entry_count as u32).to_le_bytes())?;
            f.write_all(&height_range.0.to_le_bytes())?;
            f.write_all(&height_range.1.to_le_bytes())?;
            f.write_all(&0u32.to_le_bytes())?; // padding
            // Safety: OutputKV is repr(C) with no padding bits. Writing raw bytes is correct.
            let entry_bytes = unsafe {
                std::slice::from_raw_parts(
                    entries.as_ptr() as *const u8,
                    entry_count * OutputKV::SIZE,
                )
            };
            f.write_all(entry_bytes)?;
            f.flush()?;
        }

        let file = OpenOptions::new().read(true).open(&path)?;
        maybe_warm_segment_pages(&file, entry_count);
        let mmap = maybe_mmap_segment(&file, entry_count);
        let filter = BloomFilter::build(&entries);
        let directory = Directory::build(&entries);
        let hot_body = if pin {
            tracing::info!(
                "DiskSegment: hot-pin install entries={} (~{} MiB)",
                entry_count,
                (entry_count * OutputKV::SIZE) / (1024 * 1024)
            );
            Some(Arc::<[OutputKV]>::from(entries))
        } else {
            drop(entries);
            None
        };

        Ok(Self {
            path,
            height_range,
            entry_count,
            filter,
            directory,
            file: Arc::new(file),
            mmap,
            hot_body: parking_lot::RwLock::new(hot_body),
        })
    }

    pub(super) fn clear_hot_body(&self) {
        if self.hot_body.write().take().is_some() {
            tracing::info!("DiskSegment: hot-pin cleared path={}", self.path.display());
        }
    }

    pub(super) fn has_hot_body(&self) -> bool {
        self.hot_body.read().is_some()
    }

    /// Attach a HotPin body after an async spill wrote the file without pinning
    /// (entries stayed queryable in a pending `MemoryRun` during the write).
    pub(super) fn attach_hot_pin(&self, entries: Vec<OutputKV>) {
        let entry_count = entries.len();
        tracing::info!(
            "DiskSegment: hot-pin install entries={} (~{} MiB)",
            entry_count,
            (entry_count * OutputKV::SIZE) / (1024 * 1024)
        );
        *self.hot_body.write() = Some(Arc::<[OutputKV]>::from(entries));
    }

    /// Bulk-load all segment entries (used to HotPin a streaming seed segment that
    /// was written without an in-RAM `Vec` — see `DiskIndex::maybe_hot_pin_segment`).
    pub(super) fn load_all_entries(&self) -> anyhow::Result<Vec<OutputKV>> {
        self.read_bucket(0, self.entry_count)
    }

    /// Write a segment from a borrowed entry slice (no HotPin). Used by async spill
    /// so the source `MemoryRun` can stay queryable in `DiskIndex::pending_spills`.
    pub fn write_from_slice(
        seg_dir: &Path,
        idx: usize,
        height_range: (i32, i32),
        entries: &[OutputKV],
    ) -> anyhow::Result<Self> {
        let path = seg_dir.join(format!("seg_{idx:06}.bin"));
        let entry_count = entries.len();
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            f.write_all(&DISK_SEG_MAGIC.to_le_bytes())?;
            f.write_all(&(entry_count as u32).to_le_bytes())?;
            f.write_all(&height_range.0.to_le_bytes())?;
            f.write_all(&height_range.1.to_le_bytes())?;
            f.write_all(&0u32.to_le_bytes())?;
            let entry_bytes = unsafe {
                std::slice::from_raw_parts(
                    entries.as_ptr() as *const u8,
                    entry_count * OutputKV::SIZE,
                )
            };
            f.write_all(entry_bytes)?;
            f.flush()?;
        }
        let file = OpenOptions::new().read(true).open(&path)?;
        maybe_warm_segment_pages(&file, entry_count);
        let mmap = maybe_mmap_segment(&file, entry_count);
        let filter = BloomFilter::build(entries);
        let directory = Directory::build(entries);
        Ok(Self {
            path,
            height_range,
            entry_count,
            filter,
            directory,
            file: Arc::new(file),
            mmap,
            hot_body: parking_lot::RwLock::new(None),
        })
    }

    /// Read segment header only (cheap resume hint).
    pub fn peek_max_height(path: &Path) -> anyhow::Result<i32> {
        let (_, _, max_height, _) = Self::read_header(path)?;
        Ok(max_height)
    }

    fn read_header(path: &Path) -> anyhow::Result<(usize, i32, i32, std::fs::File)> {
        let file = OpenOptions::new().read(true).open(path)?;
        let mut hdr = [0u8; 24];
        file_io::read_at(&file, &mut hdr, 0)?;
        let magic = u64::from_le_bytes(hdr[0..8].try_into().unwrap());
        if magic != DISK_SEG_MAGIC {
            anyhow::bail!("bad segment magic in {path:?}: {magic:#x}");
        }
        let entry_count = u32::from_le_bytes(hdr[8..12].try_into().unwrap()) as usize;
        let min_height = i32::from_le_bytes(hdr[12..16].try_into().unwrap());
        let max_height = i32::from_le_bytes(hdr[16..20].try_into().unwrap());
        Ok((entry_count, min_height, max_height, file))
    }

    /// Open an existing on-disk segment (resume path — rebuilds bloom + directory from file).
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let (entry_count, min_height, max_height, file) = Self::read_header(path)?;
        let file_end = HEADER_SIZE + (entry_count as u64) * (OutputKV::SIZE as u64);

        let mut filter = BloomFilter::new_for_capacity(entry_count.max(1));
        {
            let mut reader = SegmentReader {
                file: Arc::new(file.try_clone()?),
                buf: vec![],
                buf_pos: 0,
                file_offset: HEADER_SIZE,
                file_end,
            };
            while let Some(kv) = reader.advance()? {
                filter.insert(&kv.key);
            }
        }

        let file = Arc::new(file);
        let directory = {
            let mut reader = SegmentReader {
                file: Arc::clone(&file),
                buf: vec![],
                buf_pos: 0,
                file_offset: HEADER_SIZE,
                file_end,
            };
            Directory::build_streaming(&mut reader, entry_count)?
        };

        let mmap = maybe_mmap_segment(&file, entry_count);
        Ok(Self {
            path: path.to_path_buf(),
            height_range: (min_height, max_height),
            entry_count,
            filter,
            directory,
            file,
            mmap,
            hot_body: parking_lot::RwLock::new(None),
        })
    }

    /// Height range of entries in this segment (inclusive).
    pub fn height_range(&self) -> (i32, i32) {
        self.height_range
    }

    /// Zero-copy view of `[lo, hi)` entries in the segment mmap.
    ///
    /// Header is 24 bytes (8-aligned); `OutputKV` is 56 bytes / align 8 — entry
    /// offsets are always aligned. Bytes were written as valid `OutputKV` values.
    fn mmap_bucket_slice(&self, lo: usize, hi: usize) -> anyhow::Result<&[OutputKV]> {
        let count = hi - lo;
        if count == 0 {
            return Ok(&[]);
        }
        let mmap = self
            .mmap
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("mmap_bucket_slice without mmap"))?;
        let byte_offset = HEADER_SIZE as usize + lo * OutputKV::SIZE;
        let byte_count = count * OutputKV::SIZE;
        let slice = mmap.as_slice();
        let end = byte_offset.saturating_add(byte_count);
        if end > slice.len() {
            anyhow::bail!(
                "DiskSegment mmap short read: need {}..{} len={}",
                byte_offset,
                end,
                slice.len()
            );
        }
        let raw = &slice[byte_offset..end];
        debug_assert_eq!(raw.as_ptr() as usize % std::mem::align_of::<OutputKV>(), 0);
        // Safety: aligned repr(C) OutputKV image written by this crate.
        Ok(unsafe { std::slice::from_raw_parts(raw.as_ptr() as *const OutputKV, count) })
    }

    /// Read `count` raw `OutputKV` entries from disk starting at entry index `lo`
    /// into `out` (cleared first). Prefer [`Self::mmap_bucket_slice`] when mmap is on.
    fn read_bucket_into(
        &self,
        lo: usize,
        hi: usize,
        out: &mut Vec<OutputKV>,
    ) -> anyhow::Result<()> {
        out.clear();
        let count = hi - lo;
        if count == 0 {
            return Ok(());
        }
        let byte_offset = HEADER_SIZE as usize + lo * OutputKV::SIZE;
        let byte_count = count * OutputKV::SIZE;

        if let Some(mmap) = self.mmap.as_ref() {
            let slice = mmap.as_slice();
            let end = byte_offset.saturating_add(byte_count);
            if end > slice.len() {
                anyhow::bail!(
                    "DiskSegment mmap short read: need {}..{} len={}",
                    byte_offset,
                    end,
                    slice.len()
                );
            }
            let raw = &slice[byte_offset..end];
            out.reserve(count);
            for chunk in raw.chunks_exact(OutputKV::SIZE) {
                // Safety: OutputKV is repr(C); bytes were written as valid OutputKV values.
                let kv = unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const OutputKV) };
                out.push(kv);
            }
            return Ok(());
        }

        TLS_RAW.with(|cell| {
            let mut raw = cell.borrow_mut();
            raw.resize(byte_count, 0);
            file_io::read_at(&self.file, &mut raw[..byte_count], byte_offset as u64)?;
            out.reserve(count);
            for chunk in raw[..byte_count].chunks_exact(OutputKV::SIZE) {
                let kv = unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const OutputKV) };
                out.push(kv);
            }
            Ok(())
        })
    }

    /// Read `count` raw `OutputKV` entries from disk starting at entry index `lo`.
    ///
    /// Uses segment mmap when enabled, else `pread64` — lock-free / parallel-safe.
    fn read_bucket(&self, lo: usize, hi: usize) -> anyhow::Result<Vec<OutputKV>> {
        let mut out = Vec::new();
        self.read_bucket_into(lo, hi, &mut out)?;
        Ok(out)
    }

    /// Look up `key` in this segment within the `[since, before)` height window.
    ///
    /// Returns:
    /// - `Some(id)` if an unspent Add is found.
    /// - `Some(OUTPUT_ID_DELETED)` if a Delete is found (key was spent in this segment).
    /// - `None` if the key is not in this segment.
    pub fn lookup_key(
        &self,
        key: &[u8; 36],
        since: i32,
        before: i32,
    ) -> anyhow::Result<Option<OutputId>> {
        // Fast exits (no disk read).
        if self.height_range.1 < since || self.height_range.0 >= before {
            return Ok(None);
        }
        if !self.filter.may_contain(key) {
            return Ok(None);
        }
        let (lo, hi) = self.directory.lookup_range(key);
        if lo >= hi || hi > self.entry_count {
            return Ok(None);
        }

        let hi = hi.min(self.entry_count);
        note_pread((hi - lo) * OutputKV::SIZE);
        let bucket = self.read_bucket(lo, hi)?;

        let pos = bucket.partition_point(|e| e.key < *key);
        let mut i = pos;
        while i < bucket.len() {
            let e = &bucket[i];
            if e.key != *key {
                break;
            }
            if e.height < since || e.height >= before {
                i += 1;
                continue;
            }
            if e.is_add() {
                let next = bucket.get(i + 1);
                if let Some(n) = next {
                    if n.key == *key && n.height == e.height && n.is_delete() {
                        i += 2; // same-height create+spend: cancelled
                        continue;
                    }
                }
                return Ok(Some(e.id));
            } else if e.is_delete() {
                return Ok(Some(OUTPUT_ID_DELETED));
            }
            i += 1;
        }
        Ok(None)
    }

    /// Read all entries from this segment into a `Vec<OutputKV>`.
    ///
    /// Used by `DiskIndex::compact_oldest_if_needed` to merge old segments together.
    /// The returned entries are in the same sorted order as they were written.
    pub fn read_all_entries(&self) -> anyhow::Result<Vec<OutputKV>> {
        if self.entry_count == 0 {
            return Ok(Vec::new());
        }
        let byte_count = self.entry_count * OutputKV::SIZE;
        let mut raw = vec![0u8; byte_count];

        // A single pread64 syscall is capped by the Linux kernel at 0x7FFFF000 bytes
        // (~2 GiB). Compacted segments can exceed this when 8× fan-in produces >40M
        // entries (~2.24 GiB). Loop until all bytes are read.
        let mut file_offset = HEADER_SIZE;
        let mut buf_offset: usize = 0;
        while buf_offset < byte_count {
            let n = file_io::read_at(&self.file, &mut raw[buf_offset..], file_offset)?;
            if n == 0 {
                anyhow::bail!(
                    "read_all_entries: unexpected EOF from {:?}: read {} of {} bytes",
                    self.path,
                    buf_offset,
                    byte_count,
                );
            }
            buf_offset += n;
            file_offset += n as u64;
        }

        let mut entries = Vec::with_capacity(self.entry_count);
        for chunk in raw.chunks_exact(OutputKV::SIZE) {
            // Safety: OutputKV is repr(C); bytes were written as valid OutputKV values.
            let kv = unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const OutputKV) };
            entries.push(kv);
        }
        Ok(entries)
    }

    /// Write a new segment from a **streaming iterator** of already-sorted `OutputKV` entries.
    ///
    /// Unlike `write`, this never accumulates all entries in RAM. Peak memory:
    ///   - write buffer: `WRITER_CHUNK × OutputKV::SIZE` (≈ 448 KB)
    ///   - bloom filter: `~12 bits × actual entries` (built on pass 2 — dest-bc pair
    ///     GC survivors must not inherit a 20M `capacity` bloom)
    ///   - directory:    `≤ 4 MB` (20-bit prefix; dest-bc mega segs)
    ///
    /// After streaming all entries, the file header is updated in-place and the directory
    /// is built with a second sequential pass — O(N) time, O(buckets) memory.
    ///
    /// `capacity` is an upper bound from the caller (compact chunk). Bloom is sized
    /// to the **actual** write count on pass 2 (dest-bc pair GC).
    pub fn write_from_iter<I>(
        seg_dir: &Path,
        idx: usize,
        _capacity: usize,
        iter: I,
    ) -> anyhow::Result<Self>
    where
        I: Iterator<Item = OutputKV>,
    {
        const WRITER_CHUNK: usize = 8192;
        let tmp_path = seg_dir.join(format!("seg_{idx:06}.bin.tmp"));
        let final_path = seg_dir.join(format!("seg_{idx:06}.bin"));

        // ── Pass 1: stream entries to file ───────────────────────────────────
        let t_pass1 = std::time::Instant::now();
        let mut entry_count = 0u64;
        let mut min_height = i32::MAX;
        let mut max_height = i32::MIN;
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;

            // Placeholder header — will be updated after streaming.
            file.write_all(&DISK_SEG_MAGIC.to_le_bytes())?;
            file.write_all(&0u32.to_le_bytes())?; // count
            file.write_all(&0i32.to_le_bytes())?; // min_height
            file.write_all(&0i32.to_le_bytes())?; // max_height
            file.write_all(&0u32.to_le_bytes())?; // padding

            let mut write_buf: Vec<u8> = Vec::with_capacity(WRITER_CHUNK * OutputKV::SIZE);
            for entry in iter {
                if entry.height < min_height {
                    min_height = entry.height;
                }
                if entry.height > max_height {
                    max_height = entry.height;
                }
                entry_count += 1;
                // Safety: OutputKV is repr(C); writing raw bytes is correct.
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        &entry as *const OutputKV as *const u8,
                        OutputKV::SIZE,
                    )
                };
                write_buf.extend_from_slice(bytes);
                if write_buf.len() >= WRITER_CHUNK * OutputKV::SIZE {
                    file.write_all(&write_buf)?;
                    write_buf.clear();
                }
            }
            if !write_buf.is_empty() {
                file.write_all(&write_buf)?;
            }

            // Update header in-place.
            file.seek(SeekFrom::Start(8))?;
            file.write_all(&(entry_count as u32).to_le_bytes())?;
            file.write_all(&min_height.to_le_bytes())?;
            file.write_all(&max_height.to_le_bytes())?;
            file.flush()?;
        } // file closed here
        ACC_WRITE_PASS1_MS.with(|c| c.set(t_pass1.elapsed().as_millis() as u64));

        // ── Pass 2: directory + bloom sized to actual n (not caller capacity) ─
        let t_dir = std::time::Instant::now();
        let (directory, filter) = {
            let file = OpenOptions::new().read(true).open(&tmp_path)?;
            let mut reader = SegmentReader {
                file: Arc::new(file),
                buf: vec![],
                buf_pos: 0,
                file_offset: HEADER_SIZE,
                file_end: HEADER_SIZE + entry_count * OutputKV::SIZE as u64,
            };
            Directory::build_streaming_with_bloom(&mut reader, entry_count as usize)?
        };
        ACC_WRITE_DIR_MS.with(|c| c.set(t_dir.elapsed().as_millis() as u64));

        // ── Atomically rename to final path ───────────────────────────────────
        std::fs::rename(&tmp_path, &final_path)?;

        let file = OpenOptions::new().read(true).open(&final_path)?;
        maybe_warm_segment_pages(&file, entry_count as usize);
        let mmap = maybe_mmap_segment(&file, entry_count as usize);
        Ok(Self {
            path: final_path,
            height_range: (min_height, max_height),
            entry_count: entry_count as usize,
            filter,
            directory,
            file: Arc::new(file),
            mmap,
            hot_body: parking_lot::RwLock::new(None),
        })
    }

    /// Approximate resident bytes for this segment's in-RAM structures (bloom + directory + pin).
    pub(super) fn ram_bytes(&self) -> usize {
        let pin = self
            .hot_body
            .read()
            .as_ref()
            .map(|b| b.len() * OutputKV::SIZE)
            .unwrap_or(0);
        self.filter.mem_bytes() + self.directory.mem_bytes() + pin
    }

    /// Open a streaming reader over this segment's entries (sorted order, no full-load).
    pub(super) fn stream(&self) -> SegmentReader {
        SegmentReader {
            file: Arc::clone(&self.file),
            buf: vec![],
            buf_pos: 0,
            file_offset: HEADER_SIZE,
            file_end: HEADER_SIZE + (self.entry_count as u64) * (OutputKV::SIZE as u64),
        }
    }

    /// Batch lookup — fills `ids[i]` for any unresolved `keys[i]` in this segment.
    ///
    /// Significantly more efficient than per-key random reads: collects all unresolved
    /// keys that pass the bloom filter, sorts them by directory bucket (disk offset),
    /// then reads each bucket at most once regardless of how many keys land in it.
    /// Adjacent buckets are merged into a single `pread64` call.
    ///
    /// Complexity: O(N log N) sort + O(unique_buckets) disk reads, vs the naive
    /// O(N × pread64) that the single-key path would require.
    pub fn batch_lookup(
        &self,
        keys: &[[u8; 36]],
        ids: &mut [OutputId],
        since: i32,
        before: i32,
    ) -> anyhow::Result<()> {
        if self.height_range.1 < since || self.height_range.0 >= before {
            return Ok(());
        }

        // F10: pinned body — hold read guard (no Arc clone per lookup).
        {
            let guard = self.hot_body.read();
            if let Some(body) = guard.as_ref() {
                return self.batch_lookup_hot(body.as_ref(), keys, ids, since, before);
            }
        }

        // Phase 1–3 use TLS shells (C7): candidates / ranges / decode buffers.
        TLS_CANDIDATES.with(|cand_cell| {
            TLS_RANGES.with(|range_cell| {
                let mut candidates = cand_cell.borrow_mut();
                let mut ranges = range_cell.borrow_mut();
                candidates.clear();
                ranges.clear();

                for (idx, (key, id)) in keys.iter().zip(ids.iter()).enumerate() {
                    if *id != OutputId::MAX {
                        continue;
                    }
                    if !self.filter.may_contain(key) {
                        continue;
                    }
                    let (lo, hi) = self.directory.lookup_range(key);
                    if lo >= hi || hi > self.entry_count {
                        continue;
                    }
                    candidates.push(Candidate {
                        lo,
                        hi,
                        idx,
                        key: *key,
                    });
                }
                if candidates.is_empty() {
                    return Ok(());
                }
                ACC_DISK_SEGS.with(|c| c.set(c.get() + 1));
                ACC_DISK_CANDS.with(|c| c.set(c.get() + candidates.len() as u64));

                candidates.sort_unstable_by_key(|c| c.lo);

                let max_entries = disk_pread_max_entries();
                coalesce_sorted_spans(
                    candidates.len(),
                    |i| (candidates[i].lo, candidates[i].hi),
                    max_entries,
                    |read_lo, read_hi, span_ci, span_cj| {
                        ranges.push(Range {
                            lo: read_lo,
                            hi: read_hi.min(self.entry_count),
                            ci: span_ci,
                            cj: span_cj,
                        });
                    },
                );
                if bucket_willneed_from_env() {
                    for r in ranges.iter() {
                        let byte_offset = HEADER_SIZE + (r.lo * OutputKV::SIZE) as u64;
                        let byte_count = (r.hi - r.lo) * OutputKV::SIZE;
                        advise_willneed_range(&self.file, byte_offset, byte_count);
                    }
                }

                // Phase 3: mmap = zero-copy resolve; else pread into TLS bucket.
                if self.mmap.is_some() {
                    for r in ranges.iter() {
                        note_pread((r.hi - r.lo) * OutputKV::SIZE);
                        let bucket = self.mmap_bucket_slice(r.lo, r.hi)?;
                        for c in &candidates[r.ci..r.cj] {
                            let sub_lo = c.lo.saturating_sub(r.lo);
                            let sub_hi = (c.hi.min(self.entry_count)).saturating_sub(r.lo);
                            if sub_lo >= sub_hi || sub_lo >= bucket.len() {
                                continue;
                            }
                            let slice = &bucket[sub_lo..sub_hi.min(bucket.len())];
                            resolve_key_in_slice(slice, &c.key, c.idx, ids, since, before);
                        }
                    }
                    return Ok(());
                }

                let parallel =
                    disk_parallel_pread_from_env() && ranges.len() >= PARALLEL_PREAD_MIN_RANGES;
                // R-342: single fat bucket → page probe. R-345: also glued multi-candidate
                // ranges when the candidates are sparse in the span — a block spending k
                // outputs of one fan-out tx glues k buckets into one 200–440 KiB read; R-344
                // 360k+ still read 8–24 MB/block with `cands − preads` negative. Probe each
                // candidate while the probe cost (k × (log2 pages + 3) pages) stays under the
                // span in pages; denser spends read the whole bucket once, which is cheaper.
                let probe_min = disk_probe_min_entries();
                let probe_ok = |r: &Range| {
                    if parallel || r.hi - r.lo <= probe_min {
                        return false;
                    }
                    let k = r.cj - r.ci;
                    k == 1 || probe_cheaper_than_span(k, r.hi - r.lo, PROBE_PAGE_ENTRIES)
                };
                for r in ranges.iter() {
                    if !probe_ok(r) {
                        note_pread((r.hi - r.lo) * OutputKV::SIZE);
                    }
                }
                #[cfg(feature = "rayon")]
                if parallel {
                    use rayon::prelude::*;
                    let buckets: Vec<Vec<OutputKV>> = ranges
                        .par_iter()
                        .map(|r| self.read_bucket(r.lo, r.hi))
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    for (r, bucket) in ranges.iter().zip(buckets.iter()) {
                        for c in &candidates[r.ci..r.cj] {
                            let sub_lo = c.lo.saturating_sub(r.lo);
                            let sub_hi = (c.hi.min(self.entry_count)).saturating_sub(r.lo);
                            if sub_lo >= sub_hi || sub_lo >= bucket.len() {
                                continue;
                            }
                            let slice = &bucket[sub_lo..sub_hi.min(bucket.len())];
                            resolve_key_in_slice(slice, &c.key, c.idx, ids, since, before);
                        }
                    }
                    return Ok(());
                }
                let _ = parallel;
                TLS_BUCKET.with(|bucket_cell| {
                    let mut bucket = bucket_cell.borrow_mut();
                    for r in ranges.iter() {
                        if probe_ok(r) {
                            // R-342/R-345: fat bucket(s) — page-wise probe per candidate.
                            for c in &candidates[r.ci..r.cj] {
                                let (clo, chi) = (c.lo.max(r.lo), c.hi.min(r.hi));
                                if clo >= chi {
                                    continue;
                                }
                                let (nlo, nhi) =
                                    probe_narrow(clo, chi, PROBE_PAGE_ENTRIES, &c.key, |a, b| {
                                        self.read_bucket_into(a, b, &mut bucket)?;
                                        note_pread((b - a) * OutputKV::SIZE);
                                        let first = bucket.first().map(|e| e.key);
                                        let last = bucket.last().map(|e| e.key);
                                        match (first, last) {
                                            (Some(f), Some(l)) => Ok((f, l)),
                                            _ => anyhow::bail!(
                                                "DiskSegment probe: empty page {a}..{b}"
                                            ),
                                        }
                                    })?;
                                self.read_bucket_into(nlo, nhi, &mut bucket)?;
                                note_pread((nhi - nlo) * OutputKV::SIZE);
                                resolve_key_in_slice(&bucket, &c.key, c.idx, ids, since, before);
                            }
                            continue;
                        }
                        self.read_bucket_into(r.lo, r.hi, &mut bucket)?;
                        for c in &candidates[r.ci..r.cj] {
                            let sub_lo = c.lo.saturating_sub(r.lo);
                            let sub_hi = (c.hi.min(self.entry_count)).saturating_sub(r.lo);
                            if sub_lo >= sub_hi || sub_lo >= bucket.len() {
                                continue;
                            }
                            let slice = &bucket[sub_lo..sub_hi.min(bucket.len())];
                            resolve_key_in_slice(slice, &c.key, c.idx, ids, since, before);
                        }
                    }
                    Ok(())
                })
            })
        })
    }

    /// Hot-pin path: bloom + directory, then binary search in pinned `OutputKV` body.
    fn batch_lookup_hot(
        &self,
        body: &[OutputKV],
        keys: &[[u8; 36]],
        ids: &mut [OutputId],
        since: i32,
        before: i32,
    ) -> anyhow::Result<()> {
        debug_assert_eq!(body.len(), self.entry_count);
        let mut touched = false;
        let mut cands = 0u64;
        for (idx, key) in keys.iter().enumerate() {
            if ids[idx] != OutputId::MAX {
                continue;
            }
            if !self.filter.may_contain(key) {
                continue;
            }
            let (lo, hi) = self.directory.lookup_range(key);
            if lo >= hi || hi > body.len() {
                continue;
            }
            touched = true;
            cands += 1;
            resolve_key_in_slice(&body[lo..hi], key, idx, ids, since, before);
        }
        if touched {
            // Count as a seg touch for HOTPATH attribution; preads stay 0.
            ACC_DISK_SEGS.with(|c| c.set(c.get() + 1));
            ACC_DISK_CANDS.with(|c| c.set(c.get() + cands));
        }
        Ok(())
    }
}

fn resolve_key_in_slice(
    slice: &[OutputKV],
    key: &[u8; 36],
    idx: usize,
    ids: &mut [OutputId],
    since: i32,
    before: i32,
) {
    let pos = slice.partition_point(|e| e.key < *key);
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
            let next = slice.get(i + 1);
            if let Some(n) = next {
                if n.key == e.key && n.height == e.height && n.is_delete() {
                    i += 2;
                    continue;
                }
            }
            ids[idx] = e.id;
        } else if e.is_delete() {
            ids[idx] = OUTPUT_ID_DELETED;
        }
        break;
    }
}

impl std::fmt::Debug for DiskSegment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskSegment")
            .field("path", &self.path)
            .field("entry_count", &self.entry_count)
            .field("height_range", &self.height_range)
            .finish()
    }
}

// ─── SegmentReader ────────────────────────────────────────────────────────────

const READER_CHUNK: usize = 8192; // entries per read call (~448 KB)

/// Streaming iterator over a `DiskSegment`'s entries in sorted order.
///
/// Reads entries in chunks of `READER_CHUNK` rather than loading the full segment
/// into RAM. Used by `DiskIndex::do_compact` to implement a streaming k-way merge
/// that is O(output_entries) in memory instead of O(total_input_entries).
pub(super) struct SegmentReader {
    file: Arc<File>,
    buf: Vec<OutputKV>,
    buf_pos: usize,
    file_offset: u64,
    file_end: u64,
}

impl SegmentReader {
    fn fill(&mut self) -> anyhow::Result<()> {
        let remaining_bytes = self.file_end.saturating_sub(self.file_offset) as usize;
        let to_read = READER_CHUNK.min(remaining_bytes / OutputKV::SIZE);
        if to_read == 0 {
            self.buf.clear();
            self.buf_pos = 0;
            return Ok(());
        }
        let byte_count = to_read * OutputKV::SIZE;
        let mut raw = vec![0u8; byte_count];
        let mut off = 0usize;
        let mut foff = self.file_offset;
        while off < byte_count {
            // pread64 is capped at ~2 GiB per call; loop to handle large reads.
            let n = file_io::read_at(&self.file, &mut raw[off..], foff)?;
            if n == 0 {
                anyhow::bail!("SegmentReader: unexpected EOF at offset {foff}");
            }
            off += n;
            foff += n as u64;
        }
        let consumed_off = self.file_offset;
        self.file_offset += byte_count as u64;
        if compact_dontneed_enabled() {
            advise_dontneed_range(&self.file, consumed_off, byte_count);
        }
        self.buf.clear();
        self.buf.reserve(to_read);
        for chunk in raw.chunks_exact(OutputKV::SIZE) {
            // Safety: OutputKV is repr(C); bytes were written as valid OutputKV values.
            let kv = unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const OutputKV) };
            self.buf.push(kv);
        }
        self.buf_pos = 0;
        Ok(())
    }

    /// Returns the current head entry without consuming it, or `None` if exhausted.
    pub fn peek(&mut self) -> anyhow::Result<Option<OutputKV>> {
        if self.buf_pos >= self.buf.len() {
            self.fill()?;
        }
        Ok(self.buf.get(self.buf_pos).copied())
    }

    /// Consumes and returns the current head entry, or `None` if exhausted.
    pub fn advance(&mut self) -> anyhow::Result<Option<OutputKV>> {
        if self.buf_pos >= self.buf.len() {
            self.fill()?;
        }
        if self.buf_pos >= self.buf.len() {
            return Ok(None);
        }
        let e = self.buf[self.buf_pos];
        self.buf_pos += 1;
        Ok(Some(e))
    }
}

#[cfg(test)]
#[path = "disk_segment_tests.rs"]
mod tests;
