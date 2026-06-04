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

use super::memory_run::{BloomFilter, Directory};
use super::types::{OutputId, OutputKV, OUTPUT_ID_DELETED};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write as _};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DISK_SEG_MAGIC: u64 = 0xD15C_DEAD_B10C_0001;
const HEADER_SIZE: u64 = 24; // magic(8) + count(4) + min_h(4) + max_h(4) + pad(4)
pub(super) const HEADER_SIZE_USIZE: usize = HEADER_SIZE as usize;

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
        let path = seg_dir.join(format!("seg_{idx:06}.bin"));

        // Write header + entries to disk.
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            f.write_all(&DISK_SEG_MAGIC.to_le_bytes())?;
            f.write_all(&(run.entries.len() as u32).to_le_bytes())?;
            f.write_all(&run.height_range.0.to_le_bytes())?;
            f.write_all(&run.height_range.1.to_le_bytes())?;
            f.write_all(&0u32.to_le_bytes())?; // padding
                                               // Safety: OutputKV is repr(C) with no padding bits. Writing raw bytes is correct.
            let entry_bytes = unsafe {
                std::slice::from_raw_parts(
                    run.entries.as_ptr() as *const u8,
                    run.entries.len() * OutputKV::SIZE,
                )
            };
            f.write_all(entry_bytes)?;
            f.flush()?;
        }

        let file = OpenOptions::new().read(true).open(&path)?;

        Ok(Self {
            path,
            height_range: run.height_range,
            entry_count: run.entries.len(),
            // Build bloom + directory from the run's existing in-memory structures.
            // These are the same structures already built during MemoryRun::merge; we clone
            // them here to avoid rebuilding from the (now-to-be-freed) entries Vec.
            filter: BloomFilter::build(&run.entries),
            directory: Directory::build(&run.entries),
            file: Arc::new(file),
        })
    }

    /// Height range of entries in this segment (inclusive).
    pub fn height_range(&self) -> (i32, i32) {
        self.height_range
    }

    /// Read `count` raw `OutputKV` entries from disk starting at entry index `lo`.
    ///
    /// Uses `pread64` — lock-free and safe to call from multiple threads.
    fn read_bucket(&self, lo: usize, hi: usize) -> anyhow::Result<Vec<OutputKV>> {
        let count = hi - lo;
        if count == 0 {
            return Ok(Vec::new());
        }
        let byte_offset = HEADER_SIZE + (lo * OutputKV::SIZE) as u64;
        let byte_count = count * OutputKV::SIZE;

        let mut raw = vec![0u8; byte_count];
        self.file.read_at(&mut raw, byte_offset)?;

        // Safety: OutputKV is repr(C). The bytes were written as valid OutputKV values.
        // `read_at` fills exactly `byte_count` bytes. Alignment: Vec<u8> may not align to 8;
        // we copy into a properly-aligned Vec<OutputKV>.
        let mut entries = Vec::with_capacity(count);
        for chunk in raw.chunks_exact(OutputKV::SIZE) {
            let kv = unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const OutputKV) };
            entries.push(kv);
        }
        Ok(entries)
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

        let bucket = self.read_bucket(lo, hi.min(self.entry_count))?;

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
            let n = self.file.read_at(&mut raw[buf_offset..], file_offset)?;
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
    ///   - bloom filter: `~12 bits × capacity` (≈ 300 MB for 200 M entries)
    ///   - directory:    `≤ 256 KB`
    ///
    /// After streaming all entries, the file header is updated in-place and the directory
    /// is built with a second sequential pass — O(N) time, O(buckets) memory.
    ///
    /// `capacity` should be an upper bound on the number of entries that will be written
    /// (used to size the bloom filter; over-provisioning is safe but wastes memory).
    pub fn write_from_iter<I>(
        seg_dir: &Path,
        idx: usize,
        capacity: usize,
        iter: I,
    ) -> anyhow::Result<Self>
    where
        I: Iterator<Item = OutputKV>,
    {
        const WRITER_CHUNK: usize = 8192;
        let tmp_path = seg_dir.join(format!("seg_{idx:06}.bin.tmp"));
        let final_path = seg_dir.join(format!("seg_{idx:06}.bin"));

        // ── Pass 1: stream entries to file ───────────────────────────────────
        let mut filter = BloomFilter::new_for_capacity(capacity);
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
                filter.insert(&entry.key);
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

        // ── Pass 2: build directory (streaming, O(buckets) memory) ───────────
        let directory = {
            let file = OpenOptions::new().read(true).open(&tmp_path)?;
            let mut reader = SegmentReader {
                file: Arc::new(file),
                buf: vec![],
                buf_pos: 0,
                file_offset: HEADER_SIZE,
                file_end: HEADER_SIZE + entry_count * OutputKV::SIZE as u64,
            };
            Directory::build_streaming(&mut reader, entry_count as usize)?
        };

        // ── Atomically rename to final path ───────────────────────────────────
        std::fs::rename(&tmp_path, &final_path)?;

        let file = OpenOptions::new().read(true).open(&final_path)?;
        Ok(Self {
            path: final_path,
            height_range: (min_height, max_height),
            entry_count: entry_count as usize,
            filter,
            directory,
            file: Arc::new(file),
        })
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

        // Phase 1: collect candidates — keys that are unresolved and pass bloom filter.
        // Store (bucket_lo, bucket_hi, original_index, key).
        struct Candidate {
            lo: usize,
            hi: usize,
            idx: usize,
            key: [u8; 36],
        }
        let mut candidates: Vec<Candidate> = Vec::new();
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

        // Phase 2: sort by lo (disk offset) so we can sweep sequentially.
        candidates.sort_unstable_by_key(|c| c.lo);

        // Phase 3: sweep through sorted candidates, merging adjacent/overlapping bucket reads.
        let mut ci = 0;
        while ci < candidates.len() {
            // Determine the contiguous read range covering this group.
            let read_lo = candidates[ci].lo;
            let mut read_hi = candidates[ci].hi;
            let mut cj = ci + 1;
            while cj < candidates.len() && candidates[cj].lo <= read_hi {
                read_hi = read_hi.max(candidates[cj].hi);
                cj += 1;
            }
            // One pread64 covers all candidates in [ci, cj).
            let bucket = self.read_bucket(read_lo, read_hi.min(self.entry_count))?;

            for c in &candidates[ci..cj] {
                // Narrow within the merged buffer to this candidate's sub-range.
                let sub_lo = c.lo.saturating_sub(read_lo);
                let sub_hi = (c.hi.min(self.entry_count)).saturating_sub(read_lo);
                if sub_lo >= sub_hi || sub_lo >= bucket.len() {
                    continue;
                }
                let slice = &bucket[sub_lo..sub_hi.min(bucket.len())];
                let pos = slice.partition_point(|e| e.key < c.key);
                let mut i = pos;
                while i < slice.len() {
                    let e = &slice[i];
                    if e.key != c.key {
                        break;
                    }
                    if e.height < since || e.height >= before {
                        i += 1;
                        continue;
                    }
                    if e.is_add() {
                        // Check if immediately followed by a Delete at the same height (cancelled).
                        let next = slice.get(i + 1);
                        if let Some(n) = next {
                            if n.key == e.key && n.height == e.height && n.is_delete() {
                                i += 2;
                                continue;
                            }
                        }
                        ids[c.idx] = e.id;
                    } else if e.is_delete() {
                        ids[c.idx] = OUTPUT_ID_DELETED;
                    }
                    break;
                }
            }
            ci = cj;
        }
        Ok(())
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
            let n = self.file.read_at(&mut raw[off..], foff)?;
            if n == 0 {
                anyhow::bail!("SegmentReader: unexpected EOF at offset {foff}");
            }
            off += n;
            foff += n as u64;
        }
        self.file_offset += byte_count as u64;
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
