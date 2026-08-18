//! Phase 3: watermark export — scan all live UTXOs from the age-tiered engine and write
//! them into a checkpoint tree for SIGKILL-safe resume.
//!
//! Periodic mid-IBD exports use **ping-pong** trees (`ibd_utxos_ckpt_a` / `ibd_utxos_ckpt_b`):
//! write a full snapshot to the inactive tree (clear + PUT), then flip the active slot in
//! chain_info only after all PUTs succeed. An interrupted export leaves the previous snapshot
//! intact.
//!
//! Called once at IBD completion when `BLVM_IBD_ENGINE=1`. The engine's `scan_live()` returns
//! all Add-without-paired-Delete entries. For each, we:
//!   1. Fetch the `OutputDetail` from the flat table.
//!   2. Encode via [`encode_utxo_with_codec`](crate::storage::utxo_value_codec::encode_utxo_with_codec)
//!      (rkyv on heed3, bincode otherwise — matching `flush_batch_to_disk` format).
//!   3. Batch-write to the tree via `Tree::batch()`.
//!   4. Accumulate MuHash3072 in the same pass (no per-op disk reads).
//!
//! After writing, the normal `IbdUtxoStore` retire path takes over from the watermark height.
//!
//! Export uses the existing `Tree` abstraction so snapshots are backend-agnostic (RocksDB,
//! TidesDB, or Redb). RocksDB SST ingestion (`SstFileWriter` + `ingest_external_file`) is used
//! for high-throughput bulk loads at large UTXO counts.

use super::database::UtxoDatabase;
use super::types::{IdCodec, OutputId, OutputKV, output_key_to_outpoint};
use crate::storage::database::Tree;
use crate::storage::disk_utxo::outpoint_to_key;
use crate::storage::utxo_value_codec::{ValueCodec, encode_utxo_with_codec};
use anyhow::{Context, Result};
use blvm_muhash::{MuHash3072, serialize_coin_for_muhash};
use blvm_protocol::types::{SharedByteString, UTXO};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;

/// Per-leg timing for checkpoint export (W0-1 observability).
#[derive(Debug, Clone, Copy, Default)]
pub struct CheckpointExportTimings {
    pub compact_ms: u64,
    pub scan_prep_ms: u64,
    pub stream_ms: u64,
    pub clear_ms: u64,
    pub fetch_ms: u64,
    pub encode_ms: u64,
    pub write_ms: u64,
    pub overlay_ms: u64,
    pub trim_ms: u64,
    pub wall_ms: u64,
}
#[cfg(target_os = "linux")]
use libc;
#[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
use libmimalloc_sys;
use std::sync::Arc;
use tracing::{info, warn};

const EXPORT_CHUNK_SIZE: usize = 500_000;

/// E3c: spill live Adds during compact, globally sort by flat-table offset, then fetch.
///
/// Without this, each key-order chunk of 500k is offset-sorted locally but still spans the
/// whole `utxo_table.bin` — ~N_chunks full-file random sweeps. Global sort → one monotonic pass.
fn export_global_offset_sort() -> bool {
    matches!(
        std::env::var("BLVM_IBD_EXPORT_GLOBAL_OFFSET_SORT")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// E4: spill key-sorted encode runs, k-way merge, `MDB_APPEND` into empty ping-pong slot.
fn export_append_load() -> bool {
    matches!(
        std::env::var("BLVM_IBD_EXPORT_APPEND_LOAD")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// E5: APPEND commit cadence. Unset → `500_000` (E4 KEEP). `0` = single txn (E5 REVERT @502287).
fn export_append_commit_every() -> usize {
    std::env::var("BLVM_IBD_EXPORT_APPEND_COMMIT")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(500_000)
}

fn export_tmpdir() -> PathBuf {
    std::env::temp_dir()
}

/// Piggyback disk compact + memory overlay export (default). Set `BLVM_IBD_EXPORT_PIGGYBACK=0`
/// to use the legacy post-compact `CheckpointStream` path.
fn export_piggyback_enabled() -> bool {
    match std::env::var("BLVM_IBD_EXPORT_PIGGYBACK") {
        Ok(v) => !(v.trim() == "0" || v.eq_ignore_ascii_case("false")),
        Err(_) => true,
    }
}

/// Batched checkpoint writer used by piggyback export and memory overlay.
struct CheckpointChunkWriter<'a> {
    db: &'a UtxoDatabase,
    tree: &'a dyn Tree,
    codec: ValueCodec,
    checkpoint_height: i32,
    muhash: MuHash3072,
    live_count: usize,
    chunk_kvs: Vec<OutputKV>,
    chunk_ids: Vec<OutputId>,
    details: Vec<super::types::OutputDetail>,
    kv_pairs: Vec<(Vec<u8>, Vec<u8>)>,
    ser_buf: Vec<u8>,
    fetch_ms: u64,
    encode_ms: u64,
    write_ms: u64,
    /// E3c: defer fetch until after compact; spill Adds, global offset-sort, one IO pass.
    global_offset_sort: bool,
    spill: Option<BufWriter<File>>,
    spill_path: Option<PathBuf>,
    spill_count: usize,
    sort_ms: u64,
    /// E4: defer LMDB puts; spill key-sorted runs; merge + MDB_APPEND.
    append_load: bool,
    append_runs: Vec<PathBuf>,
    append_dir: Option<PathBuf>,
}

impl<'a> CheckpointChunkWriter<'a> {
    fn new(
        db: &'a UtxoDatabase,
        tree: &'a dyn Tree,
        codec: ValueCodec,
        checkpoint_height: i32,
    ) -> Self {
        Self {
            db,
            tree,
            codec,
            checkpoint_height,
            muhash: MuHash3072::new(),
            live_count: 0,
            chunk_kvs: Vec::with_capacity(EXPORT_CHUNK_SIZE),
            chunk_ids: Vec::with_capacity(EXPORT_CHUNK_SIZE),
            details: Vec::with_capacity(EXPORT_CHUNK_SIZE),
            kv_pairs: Vec::with_capacity(EXPORT_CHUNK_SIZE),
            ser_buf: Vec::with_capacity(200),
            fetch_ms: 0,
            encode_ms: 0,
            write_ms: 0,
            global_offset_sort: export_global_offset_sort(),
            spill: None,
            spill_path: None,
            spill_count: 0,
            sort_ms: 0,
            append_load: export_append_load(),
            append_runs: Vec::new(),
            append_dir: None,
        }
    }

    fn ensure_spill(&mut self) -> Result<()> {
        if self.spill.is_some() {
            return Ok(());
        }
        // Prefer disk-backed dir — std::env::temp_dir() is often tmpfs; a 9 GiB spill
        // there doubles as anon and fights the later load Vec.
        let mut path = export_tmpdir();
        path.push(format!(
            "blvm-export-e3c-{}-{}.bin",
            self.checkpoint_height,
            std::process::id()
        ));
        let f = File::create(&path).with_context(|| format!("E3c spill create {}", path.display()))?;
        self.spill = Some(BufWriter::with_capacity(8 << 20, f));
        self.spill_path = Some(path);
        Ok(())
    }

    fn ensure_append_dir(&mut self) -> Result<&PathBuf> {
        if self.append_dir.is_none() {
            let mut dir = export_tmpdir();
            dir.push(format!(
                "blvm-export-e4-{}-{}",
                self.checkpoint_height,
                std::process::id()
            ));
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("E4 append dir create {}", dir.display()))?;
            self.append_dir = Some(dir);
        }
        Ok(self.append_dir.as_ref().expect("append_dir"))
    }

    /// Spill already key-sorted `kv_pairs` as one merge run (E4).
    fn spill_append_run(&mut self) -> Result<()> {
        if self.kv_pairs.is_empty() {
            return Ok(());
        }
        let dir = self.ensure_append_dir()?.clone();
        let path = dir.join(format!("run-{:05}.bin", self.append_runs.len()));
        let mut w = BufWriter::with_capacity(8 << 20, File::create(&path)?);
        for (k, v) in &self.kv_pairs {
            let klen = u16::try_from(k.len()).context("E4 key len > u16")?;
            let vlen = u32::try_from(v.len()).context("E4 value len > u32")?;
            w.write_all(&klen.to_le_bytes())?;
            w.write_all(k)?;
            w.write_all(&vlen.to_le_bytes())?;
            w.write_all(v)?;
        }
        w.flush()?;
        self.append_runs.push(path);
        Ok(())
    }

    fn absorb_live_add(&mut self, kv: OutputKV) -> Result<()> {
        if !kv.is_add() || kv.id == 0 || kv.height > self.checkpoint_height {
            return Ok(());
        }
        if self.global_offset_sort {
            self.ensure_spill()?;
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    &kv as *const OutputKV as *const u8,
                    OutputKV::SIZE,
                )
            };
            self.spill
                .as_mut()
                .expect("spill")
                .write_all(bytes)
                .context("E3c spill write")?;
            self.spill_count += 1;
            return Ok(());
        }
        self.chunk_kvs.push(kv);
        if self.chunk_kvs.len() >= EXPORT_CHUNK_SIZE {
            self.flush_chunk()?;
        }
        Ok(())
    }

    fn flush_chunk(&mut self) -> Result<()> {
        if self.chunk_kvs.is_empty() {
            return Ok(());
        }
        self.chunk_ids.clear();
        self.chunk_ids.extend(self.chunk_kvs.iter().map(|kv| kv.id));
        self.details.clear();
        let t_fetch = std::time::Instant::now();
        let fetched = self.db.fetch(&self.chunk_ids, &mut self.details)?;
        self.fetch_ms += t_fetch.elapsed().as_millis() as u64;
        if fetched != self.chunk_kvs.len() {
            warn!(
                "checkpoint export chunk: fetched {} details but expected {}",
                fetched,
                self.chunk_kvs.len()
            );
        }

        self.kv_pairs.clear();
        let t_encode = std::time::Instant::now();
        // E2.2/E2.3: encode + MuHash in parallel (production/rayon).
        // E2.2 only parallelized row/preimage then serially `insert_mut` × chunk —
        // S0@401287 still saw encode_ms≈154s. E2.3 folds per-shard MuHash via
        // `multiply_mut` (commutative). Opt out: `BLVM_IBD_EXPORT_SERIAL_MUHASH=1`.
        #[cfg(feature = "rayon")]
        {
            use rayon::prelude::*;
            let codec = self.codec;
            let kvs = &self.chunk_kvs;
            let details = &self.details;
            let serial_muhash = matches!(
                std::env::var("BLVM_IBD_EXPORT_SERIAL_MUHASH")
                    .ok()
                    .as_deref()
                    .map(str::trim),
                Some("1") | Some("true") | Some("yes") | Some("on")
            );
            if serial_muhash {
                let encoded: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = (0..kvs.len())
                    .into_par_iter()
                    .map(|rank| -> Result<Option<(Vec<u8>, Vec<u8>, Vec<u8>)>> {
                        let Some(detail) = details.get(rank) else {
                            return Ok(None);
                        };
                        let kv = &kvs[rank];
                        let op = output_key_to_outpoint(&kv.key);
                        let rocks_key = outpoint_to_key(&op);
                        let utxo = detail.utxo.as_ref();
                        let preimage = serialize_coin_for_muhash(
                            &op.hash,
                            op.index,
                            utxo.height as u32,
                            utxo.is_coinbase,
                            utxo.value,
                            utxo.script_pubkey.as_ref(),
                        );
                        let row = encode_utxo_with_codec(codec, utxo)?;
                        Ok(Some((rocks_key.to_vec(), row, preimage)))
                    })
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect();
                for (key, row, preimage) in encoded {
                    self.muhash.insert_mut(&preimage);
                    self.kv_pairs.push((key, row));
                }
            } else {
                // Shard by thread count so each worker owns a contiguous range
                // (better cache + one MuHash state per shard).
                let n = kvs.len();
                let shards = rayon::current_num_threads().clamp(1, 64).min(n.max(1));
                let shard_len = n.div_ceil(shards);
                let parts: Vec<Result<(MuHash3072, Vec<(Vec<u8>, Vec<u8>)>)>> = (0..shards)
                    .into_par_iter()
                    .map(|shard| -> Result<(MuHash3072, Vec<(Vec<u8>, Vec<u8>)>)> {
                        let start = shard * shard_len;
                        let end = (start + shard_len).min(n);
                        let mut mh = MuHash3072::new();
                        let mut pairs = Vec::with_capacity(end.saturating_sub(start));
                        for rank in start..end {
                            let Some(detail) = details.get(rank) else {
                                continue;
                            };
                            let kv = &kvs[rank];
                            let op = output_key_to_outpoint(&kv.key);
                            let rocks_key = outpoint_to_key(&op);
                            let utxo = detail.utxo.as_ref();
                            let preimage = serialize_coin_for_muhash(
                                &op.hash,
                                op.index,
                                utxo.height as u32,
                                utxo.is_coinbase,
                                utxo.value,
                                utxo.script_pubkey.as_ref(),
                            );
                            mh.insert_mut(&preimage);
                            let row = encode_utxo_with_codec(codec, utxo)?;
                            pairs.push((rocks_key.to_vec(), row));
                        }
                        Ok((mh, pairs))
                    })
                    .collect();
                for part in parts {
                    let (mh, pairs) = part?;
                    self.muhash.multiply_mut(&mh);
                    self.kv_pairs.extend(pairs);
                }
            }
        }
        #[cfg(not(feature = "rayon"))]
        {
            for (rank, kv) in self.chunk_kvs.iter().enumerate() {
                let Some(detail) = self.details.get(rank) else {
                    continue;
                };
                let op = output_key_to_outpoint(&kv.key);
                let rocks_key = outpoint_to_key(&op);
                let utxo = detail.utxo.as_ref();
                let preimage = serialize_coin_for_muhash(
                    &op.hash,
                    op.index,
                    utxo.height as u32,
                    utxo.is_coinbase,
                    utxo.value,
                    utxo.script_pubkey.as_ref(),
                );
                self.muhash.insert_mut(&preimage);
                self.ser_buf.clear();
                let row = encode_utxo_with_codec(self.codec, utxo)?;
                self.kv_pairs.push((rocks_key.to_vec(), row));
            }
        }
        self.encode_ms += t_encode.elapsed().as_millis() as u64;

        self.kv_pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        self.live_count += self.kv_pairs.len();
        let t_write = std::time::Instant::now();
        if self.append_load {
            // E4: defer LMDB put — spill run for later k-way merge + MDB_APPEND.
            self.spill_append_run()?;
        } else {
            self.tree.bulk_load_sorted_kv(&self.kv_pairs)?;
        }
        self.write_ms += t_write.elapsed().as_millis() as u64;
        self.chunk_kvs.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<(MuHash3072, usize, u64, u64, u64)> {
        if self.global_offset_sort {
            self.finish_global_offset_sort()?;
        } else {
            self.flush_chunk()?;
        }
        if self.append_load {
            self.merge_append_load()?;
        }
        Ok((
            self.muhash,
            self.live_count,
            self.fetch_ms,
            self.encode_ms,
            self.write_ms,
        ))
    }

    /// E3c: load spilled Adds, sort by flat-table offset, fetch+encode in chunk order.
    fn finish_global_offset_sort(&mut self) -> Result<()> {
        if let Some(mut w) = self.spill.take() {
            w.flush().context("E3c spill flush")?;
        }
        let path = match self.spill_path.take() {
            Some(p) => p,
            None => return Ok(()),
        };
        let count = self.spill_count;
        if count == 0 {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        let t_load = std::time::Instant::now();
        let mut file = BufReader::with_capacity(8 << 20, File::open(&path)?);
        let mut all = Vec::with_capacity(count);
        let mut buf = [0u8; OutputKV::SIZE];
        for _ in 0..count {
            file.read_exact(&mut buf).context("E3c spill read")?;
            let kv = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const OutputKV) };
            all.push(kv);
        }
        let load_ms = t_load.elapsed().as_millis() as u64;
        let t_sort = std::time::Instant::now();
        all.sort_unstable_by_key(|kv| IdCodec::decode(kv.id).0);
        self.sort_ms = t_sort.elapsed().as_millis() as u64;
        info!(
            "[IBD_EXPORT_E3C] buffered={} load_ms={} sort_ms={} (global offset fetch)",
            count, load_ms, self.sort_ms
        );
        let _ = std::fs::remove_file(&path);
        for chunk in all.chunks(EXPORT_CHUNK_SIZE) {
            self.chunk_kvs.clear();
            self.chunk_kvs.extend_from_slice(chunk);
            self.flush_chunk()?;
        }
        Ok(())
    }

    /// E4: k-way merge of key-sorted runs → `MDB_APPEND` into the (cleared) ping-pong tree.
    fn merge_append_load(&mut self) -> Result<()> {
        let runs = std::mem::take(&mut self.append_runs);
        let dir = self.append_dir.take();
        if runs.is_empty() {
            if let Some(d) = dir {
                let _ = std::fs::remove_dir_all(d);
            }
            return Ok(());
        }
        let t0 = std::time::Instant::now();
        let mut readers: Vec<AppendRunReader> = runs
            .iter()
            .map(|p| AppendRunReader::open(p))
            .collect::<Result<_>>()?;

        // Min-heap of (key, run_idx). Reverse Ordering → BinaryHeap as min-heap.
        let mut heap: BinaryHeap<AppendHeapItem> = BinaryHeap::new();
        for (idx, r) in readers.iter_mut().enumerate() {
            if let Some((k, v)) = r.next_pair()? {
                heap.push(AppendHeapItem {
                    key: k,
                    value: v,
                    run: idx,
                });
            }
        }

        let commit_every = export_append_commit_every();
        let t_append = std::time::Instant::now();
        let mut readers = readers;
        let mut heap = heap;
        let written = {
            #[cfg(feature = "heed3")]
            {
                if let Some(heed) = self.tree.as_heed3_tree() {
                    heed.write_append_from_fn(commit_every, || {
                        let Some(AppendHeapItem { key, value, run }) = heap.pop() else {
                            return Ok(None);
                        };
                        if let Some((k, v)) = readers[run].next_pair()? {
                            heap.push(AppendHeapItem {
                                key: k,
                                value: v,
                                run,
                            });
                        }
                        Ok(Some((key, value)))
                    })?
                } else {
                    merge_append_bulk_fallback(self.tree, &mut readers, &mut heap)?
                }
            }
            #[cfg(not(feature = "heed3"))]
            {
                merge_append_bulk_fallback(self.tree, &mut readers, &mut heap)?
            }
        };
        let append_commit_ms = t_append.elapsed().as_millis() as u64;

        // Spill time was already counted in write_ms during flush_chunk; replace with
        // merge+APPEND wall so PROFILE write_ms reflects the real LMDB cost.
        let merge_wall = t0.elapsed().as_millis() as u64;
        self.write_ms = merge_wall;
        info!(
            "[IBD_EXPORT_E4] runs={} written={} merge_wall_ms={} append_commit_ms={} commit_every={}",
            runs.len(),
            written,
            merge_wall,
            append_commit_ms,
            commit_every
        );

        for p in &runs {
            let _ = std::fs::remove_file(p);
        }
        if let Some(d) = dir {
            let _ = std::fs::remove_dir_all(d);
        }
        Ok(())
    }
}

/// Non-heed3 fallback for E4/E5 merge (chunked `bulk_load_sorted_kv`).
fn merge_append_bulk_fallback(
    tree: &dyn Tree,
    readers: &mut [AppendRunReader],
    heap: &mut BinaryHeap<AppendHeapItem>,
) -> Result<usize> {
    let mut n = 0usize;
    let mut batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(EXPORT_CHUNK_SIZE);
    while let Some(AppendHeapItem { key, value, run }) = heap.pop() {
        batch.push((key, value));
        if let Some((k, v)) = readers[run].next_pair()? {
            heap.push(AppendHeapItem {
                key: k,
                value: v,
                run,
            });
        }
        if batch.len() >= EXPORT_CHUNK_SIZE {
            n += batch.len();
            tree.bulk_load_sorted_kv(&batch)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        n += batch.len();
        tree.bulk_load_sorted_kv(&batch)?;
    }
    Ok(n)
}

/// One record from an E4 append run file.
struct AppendRunReader {
    reader: BufReader<File>,
}

impl AppendRunReader {
    fn open(path: &PathBuf) -> Result<Self> {
        Ok(Self {
            reader: BufReader::with_capacity(4 << 20, File::open(path)?),
        })
    }

    fn next_pair(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut klen_buf = [0u8; 2];
        match self.reader.read_exact(&mut klen_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let klen = u16::from_le_bytes(klen_buf) as usize;
        let mut key = vec![0u8; klen];
        self.reader.read_exact(&mut key)?;
        let mut vlen_buf = [0u8; 4];
        self.reader.read_exact(&mut vlen_buf)?;
        let vlen = u32::from_le_bytes(vlen_buf) as usize;
        let mut value = vec![0u8; vlen];
        self.reader.read_exact(&mut value)?;
        Ok(Some((key, value)))
    }
}

/// Min-heap item for E4 k-way merge (BinaryHeap is max-heap → reverse Ord).
struct AppendHeapItem {
    key: Vec<u8>,
    value: Vec<u8>,
    run: usize,
}

impl PartialEq for AppendHeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.run == other.run
    }
}
impl Eq for AppendHeapItem {}
impl PartialOrd for AppendHeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for AppendHeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse: smaller key = greater priority for max-heap → pops first.
        match other.key.cmp(&self.key) {
            Ordering::Equal => other.run.cmp(&self.run),
            o => o,
        }
    }
}

/// GC a pre-compact memory-age snapshot into live Adds + Delete keys at `checkpoint_height`.
///
/// Same per-key rule as [`CheckpointStream`]: first op at or below the checkpoint wins;
/// a Delete(h>ckpt) must not suppress Add(h<=ckpt).
fn partition_memory_overlay(
    mut mem: Vec<OutputKV>,
    checkpoint_height: i32,
) -> (Vec<OutputKV>, Vec<[u8; 36]>) {
    if mem.is_empty() {
        return (Vec::new(), Vec::new());
    }
    mem.sort_unstable();
    let mut adds = Vec::new();
    let mut deletes: Vec<[u8; 36]> = Vec::new();
    let mut i = 0usize;
    while i < mem.len() {
        let key = mem[i].key;
        let mut decided = false;
        while i < mem.len() && mem[i].key == key {
            let entry = mem[i];
            i += 1;
            if entry.height > checkpoint_height {
                continue;
            }
            if decided {
                continue;
            }
            decided = true;
            if entry.is_add() && entry.id != 0 {
                adds.push(entry);
            } else if entry.is_delete() {
                deletes.push(key);
            }
        }
    }
    (adds, deletes)
}

/// Apply memory-age Deletes after disk piggyback + batched Adds.
///
/// Uses LMDB/heed3 `batch()` commits (4k keys) instead of per-key `remove` — late-chain
/// overlays were spending ~20 min on point inserts (live export_h=500287 overlay_ms≈1176s).
fn apply_memory_overlay_deletes(
    tree: &dyn Tree,
    deletes: &[[u8; 36]],
    live_count: &mut usize,
) -> Result<()> {
    use crate::storage::disk_utxo::OutPointKey;
    const BATCH: usize = 4096;
    for chunk in deletes.chunks(BATCH) {
        let mut to_del: Vec<OutPointKey> = Vec::with_capacity(chunk.len());
        for key in chunk {
            let rocks_key = outpoint_to_key(&output_key_to_outpoint(key));
            if tree.contains_key(&rocks_key)? {
                to_del.push(rocks_key);
            }
        }
        if to_del.is_empty() {
            continue;
        }
        let mut batch = tree.batch()?;
        for rk in &to_del {
            batch.delete(rk);
        }
        batch.commit()?;
        *live_count = live_count.saturating_sub(to_del.len());
    }
    Ok(())
}

fn checkpoint_export_trim() -> u64 {
    let t_trim = std::time::Instant::now();
    #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
    t_trim.elapsed().as_millis() as u64
}

/// Ping-pong checkpoint tree names (must match `KNOWN_TREE_NAMES`).
pub const CKPT_TREE_A: &str = "ibd_utxos_ckpt_a";
pub const CKPT_TREE_B: &str = "ibd_utxos_ckpt_b";
/// Production / legacy IBD UTXO tree (used when Phase 3 copies instead of aliasing).
pub const IBD_UTXOS_TREE: &str = "ibd_utxos";

/// Return the checkpoint tree name for slot 0 or 1.
pub fn ckpt_tree_for_slot(slot: u8) -> &'static str {
    if slot & 1 == 0 {
        CKPT_TREE_A
    } else {
        CKPT_TREE_B
    }
}

/// Inactive slot for the next export (flip ping-pong).
pub fn ckpt_inactive_slot(active: u8) -> u8 {
    1 - (active & 1)
}

/// Whether `name` is a valid IBD UTXO snapshot tree (canonical or ckpt).
pub fn is_ibd_utxo_tree_name(name: &str) -> bool {
    matches!(name, IBD_UTXOS_TREE | CKPT_TREE_A | CKPT_TREE_B)
}

/// How Phase 3 finished (for logs / tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase3Finish {
    /// Tip already checkpointed — aliased canonical tree to active ckpt (no re-export).
    PromotedAlias,
    /// Wrote tip into inactive ckpt, then aliased (one mid-IBD-sized export, no second copy).
    CatchupThenAlias,
    /// Fallback: full streaming export into `ibd_utxos` (empty/missing ckpt).
    FullWatermarkExport,
}

/// Decide Phase 3 path from durable export height vs tip and whether the active ckpt is usable.
pub fn phase3_path(
    export_h: u64,
    tip_h: u64,
    active_ckpt_slot_height: u64,
    active_ckpt_nonempty: bool,
) -> Phase3Finish {
    if tip_h == 0 {
        return Phase3Finish::FullWatermarkExport;
    }
    let tip_ckpt_ready =
        active_ckpt_nonempty && active_ckpt_slot_height == tip_h && export_h >= tip_h;
    if tip_ckpt_ready {
        return Phase3Finish::PromotedAlias;
    }
    if export_h > 0 && active_ckpt_nonempty {
        // Have a prior snapshot — catch up to tip into the inactive ping-pong slot, then alias.
        return Phase3Finish::CatchupThenAlias;
    }
    Phase3Finish::FullWatermarkExport
}

/// fdatasync checkpoint tree bytes after `persist_engine_checkpoint_complete`.
///
/// On heed3 uses `force_sync_only` (no full-env madvise). Other backends use `flush_to_disk`.
pub fn sync_tree_after_persist(tree: &dyn Tree) -> Result<()> {
    #[cfg(feature = "heed3")]
    {
        if let Some(h3) = tree.as_heed3_tree() {
            h3.force_sync_only()
                .map_err(|e| anyhow::anyhow!("engine ckpt sync failed: {e:#}"))?;
            return Ok(());
        }
    }
    tree.flush_to_disk()
        .map_err(|e| anyhow::anyhow!("engine ckpt sync failed: {e}"))
}

/// Small-scale / unit-test helper: materialize live KVs then write.
///
/// **Do not use at tip scale.** `scan_live()` / `scan_all_live()` loads all disk-segment
/// Add+Delete ops into one `Vec` (O(total_ops) ≈ multi-GB near tip). Live 2026-07-13 soak
/// OOM'd during Phase 3: anon ~10→59 GB in ~7s. Production callers must use
/// [`run_watermark_export`] / [`run_checkpoint_export_replace`] (streaming / piggyback).
pub fn watermark_export(
    db: &UtxoDatabase,
    tree: &dyn Tree,
    tip_height: i32,
    codec: ValueCodec,
) -> Result<MuHash3072> {
    db.wait_for_height(tip_height);
    let live_kvs = db.scan_live();
    let result = write_live_kvs(db, tree, &live_kvs, tip_height, codec);
    drop(live_kvs);
    #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
    result
}

/// Phase 3 / tip watermark export — **streaming** path only.
///
/// Historically this called [`watermark_export`] (`scan_all_live` + 2M-row chunks), which
/// OOMs when the engine holds tip-scale history. Reuse the mid-IBD checkpoint exporter
/// ([`run_checkpoint_export_replace`]): disk compact + piggyback/stream write with peak
/// extra allocation of hundreds of MB, not tens of GB.
///
/// Caller must set the GC fence to `tip_height` before calling (same as periodic export).
pub fn run_watermark_export(
    db: &UtxoDatabase,
    tree: &Arc<dyn Tree>,
    tip_height: i32,
    codec: ValueCodec,
) -> Result<MuHash3072> {
    let t = std::time::Instant::now();
    db.wait_for_height(tip_height);
    info!(
        "IBD engine watermark export via streaming checkpoint path (height={}) \
         — not scan_all_live (OOM-prone at tip)",
        tip_height
    );
    let (muhash, count, timings) =
        run_checkpoint_export_replace(db, tree, tip_height, codec)?;
    info!(
        "IBD engine watermark export finished in {:.1}s (height={}, utxos={}, wall_ms={})",
        t.elapsed().as_secs_f64(),
        tip_height,
        count,
        timings.wall_ms
    );
    Ok(muhash)
}

/// Export the UTXO set as of `checkpoint_height` into `tree`, replacing any prior contents.
///
/// Uses a streaming k-way merge (`CheckpointStream`) so the peak extra allocation is
/// O(memory_age_entries + export_chunk) ≈ 330 MB rather than O(UTXO_count × 56 B) ≈ 14 GB+.
/// This prevents OOM at heights where the live UTXO set exceeds available RAM.
///
/// Clears `tree` first so the on-disk snapshot matches the live scan exactly.
/// The GC fence must be set to `checkpoint_height` by the caller before this function returns
/// from the scan phase (handled inside `iter_live_at_height → compact_for_checkpoint_sync`).
pub fn run_checkpoint_export_replace(
    db: &UtxoDatabase,
    tree: &Arc<dyn Tree>,
    checkpoint_height: i32,
    codec: ValueCodec,
) -> Result<(MuHash3072, usize, CheckpointExportTimings)> {
    if export_piggyback_enabled() {
        run_checkpoint_export_piggyback(db, tree, checkpoint_height, codec)
    } else {
        run_checkpoint_export_legacy(db, tree, checkpoint_height, codec)
    }
}

fn run_checkpoint_export_piggyback(
    db: &UtxoDatabase,
    tree: &Arc<dyn Tree>,
    checkpoint_height: i32,
    codec: ValueCodec,
) -> Result<(MuHash3072, usize, CheckpointExportTimings)> {
    let t_wall = std::time::Instant::now();
    info!(
        "IBD engine checkpoint export (piggyback): writing UTXOs at height {}",
        checkpoint_height
    );

    // Snapshot in-memory entries *before* the long disk compact. Concurrent validation
    // spills age tiers to new disk segments during compact; those new segments are not
    // included in the piggyback sink. Live overlay after compact would miss Adds that
    // left memory mid-export (live 2026-07-12: export_h=720000 poisoned while
    // engine_height=721386, compact ~900s).
    let mem_snapshot = db.collect_memory_entries_at_or_below(checkpoint_height);

    // Skip clear when the inactive ping-pong tree is already empty (first export /
    // after a wiped slot). After a successful flip the inactive tree still holds the
    // previous snapshot (~50M keys → multi-second LMDB clear); that path stays.
    let t_clear = std::time::Instant::now();
    let clear_ms = if tree.is_empty().unwrap_or(false) {
        0
    } else {
        tree.clear()?;
        t_clear.elapsed().as_millis() as u64
    };

    let (mem_adds, mem_deletes) = partition_memory_overlay(mem_snapshot, checkpoint_height);
    let mut writer = CheckpointChunkWriter::new(db, tree.as_ref(), codec, checkpoint_height);
    let compact_ms = db.compact_for_checkpoint_sync_with_sink(
        checkpoint_height,
        Some(|kv: OutputKV| writer.absorb_live_add(kv)),
    )?;

    // E2.1: fold memory-age Adds into the same chunked bulk_load path as disk piggyback.
    // Per-key tree.insert overlay was the late-chain wall (export_h=500287 overlay≈1176s).
    // With E3c, `finish()` owns all fetch/encode/write — do not attribute that to overlay_ms.
    for kv in mem_adds {
        writer.absorb_live_add(kv)?;
    }
    let (mut muhash, mut live_count, fetch_ms, encode_ms, write_ms) = writer.finish()?;
    let t_overlay = std::time::Instant::now();
    apply_memory_overlay_deletes(tree.as_ref(), &mem_deletes, &mut live_count)?;
    let overlay_ms = t_overlay.elapsed().as_millis() as u64;
    let trim_ms = checkpoint_export_trim();
    let wall_ms = t_wall.elapsed().as_millis() as u64;
    let timings = CheckpointExportTimings {
        compact_ms,
        scan_prep_ms: 0,
        stream_ms: 0,
        clear_ms,
        fetch_ms,
        encode_ms,
        write_ms,
        overlay_ms,
        trim_ms,
        wall_ms,
    };
    log_checkpoint_export_timings(checkpoint_height, live_count, &timings, "piggyback");
    maybe_purge_after_export(checkpoint_height);
    Ok((muhash, live_count, timings))
}

fn run_checkpoint_export_legacy(
    db: &UtxoDatabase,
    tree: &Arc<dyn Tree>,
    checkpoint_height: i32,
    codec: ValueCodec,
) -> Result<(MuHash3072, usize, CheckpointExportTimings)> {
    let t_wall = std::time::Instant::now();

    let (mut stream, compact_ms, scan_prep_ms) = db.iter_live_at_height(checkpoint_height)?;

    let t_clear = std::time::Instant::now();
    let clear_ms = if tree.is_empty().unwrap_or(false) {
        0
    } else {
        tree.clear()?;
        t_clear.elapsed().as_millis() as u64
    };

    let t_stream = std::time::Instant::now();
    let (muhash, count, fetch_ms, encode_ms, write_ms) =
        write_live_kvs_streaming(db, tree.as_ref(), &mut stream, checkpoint_height, codec)?;
    let stream_ms = t_stream.elapsed().as_millis() as u64;

    drop(stream);
    let trim_ms = checkpoint_export_trim();
    let wall_ms = t_wall.elapsed().as_millis() as u64;
    let timings = CheckpointExportTimings {
        compact_ms,
        scan_prep_ms,
        stream_ms,
        clear_ms,
        fetch_ms,
        encode_ms,
        write_ms,
        overlay_ms: 0,
        trim_ms,
        wall_ms,
    };
    log_checkpoint_export_timings(checkpoint_height, count, &timings, "replace");
    maybe_purge_after_export(checkpoint_height);
    Ok((muhash, count, timings))
}

fn log_checkpoint_export_timings(
    checkpoint_height: i32,
    count: usize,
    timings: &CheckpointExportTimings,
    label: &str,
) {
    info!(
        "IBD engine checkpoint export ({label}) in {:.1}s (height={}, utxos={}, \
         compact_ms={}, scan_prep_ms={}, stream_ms={}, clear_ms={}, fetch_ms={}, encode_ms={}, \
         write_ms={}, overlay_ms={}, trim_ms={}, wall_ms={})",
        timings.wall_ms as f64 / 1000.0,
        checkpoint_height,
        count,
        timings.compact_ms,
        timings.scan_prep_ms,
        timings.stream_ms,
        timings.clear_ms,
        timings.fetch_ms,
        timings.encode_ms,
        timings.write_ms,
        timings.overlay_ms,
        timings.trim_ms,
        timings.wall_ms,
    );
}

fn maybe_purge_after_export(checkpoint_height: i32) {
    #[cfg(feature = "jemalloc")]
    {
        if crate::node::parallel_ibd::maybe_purge_jemalloc_retained("checkpoint_export") {
            info!(
                "[JEMALLOC_RETAINED_PURGE] triggered after checkpoint export at height {}",
                checkpoint_height
            );
        }
    }
}

/// Streaming checkpoint writer: reads live UTXOs from `stream`, fetches their `OutputDetail`
/// from the flat table, encodes them, and bulk-loads into `tree` in sorted chunks.
///
/// Peak allocation per iteration: `CHUNK_SIZE × (56 + 8 + ~100 + ~120) B ≈ 142 MB`.
fn write_live_kvs_streaming(
    db: &UtxoDatabase,
    tree: &dyn Tree,
    stream: &mut super::index::CheckpointStream,
    tip_height: i32,
    codec: ValueCodec,
) -> Result<(MuHash3072, usize, u64, u64, u64)> {
    const CHUNK_SIZE: usize = EXPORT_CHUNK_SIZE;
    let mut muhash = MuHash3072::new();
    let mut total_written = 0usize;
    let mut ser_buf: Vec<u8> = Vec::with_capacity(200);

    info!(
        "IBD engine checkpoint export (streaming): writing UTXOs at height {}",
        tip_height
    );

    let mut fetch_ms: u64 = 0;
    let mut encode_ms: u64 = 0;
    let mut write_ms: u64 = 0;
    let mut chunk_kvs: Vec<super::types::OutputKV> = Vec::with_capacity(CHUNK_SIZE);
    let mut chunk_ids: Vec<OutputId> = Vec::with_capacity(CHUNK_SIZE);
    let mut details: Vec<super::types::OutputDetail> = Vec::with_capacity(CHUNK_SIZE);
    let mut kv_pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(CHUNK_SIZE);

    loop {
        // Fill one chunk from the stream.
        chunk_kvs.clear();
        while chunk_kvs.len() < CHUNK_SIZE {
            match stream.next_live()? {
                Some(e) => chunk_kvs.push(e),
                None => break,
            }
        }
        if chunk_kvs.is_empty() {
            break;
        }

        // All entries from next_live() are Add entries with id != 0 — no filter needed.
        chunk_ids.clear();
        chunk_ids.extend(chunk_kvs.iter().map(|kv| kv.id));
        details.clear();
        let t_fetch = std::time::Instant::now();
        let fetched = db.fetch(&chunk_ids, &mut details)?;
        fetch_ms += t_fetch.elapsed().as_millis() as u64;
        if fetched != chunk_kvs.len() {
            warn!(
                "checkpoint export chunk: fetched {} details but expected {}",
                fetched,
                chunk_kvs.len()
            );
        }

        kv_pairs.clear();
        let t_encode = std::time::Instant::now();
        for (rank, kv) in chunk_kvs.iter().enumerate() {
            let Some(detail) = details.get(rank) else {
                continue;
            };
            let op = output_key_to_outpoint(&kv.key);
            let rocks_key = outpoint_to_key(&op);
            let utxo = detail.utxo.as_ref();

            let preimage = serialize_coin_for_muhash(
                &op.hash,
                op.index,
                utxo.height as u32,
                utxo.is_coinbase,
                utxo.value,
                utxo.script_pubkey.as_ref(),
            );
            muhash.insert_mut(&preimage);

            ser_buf.clear();
            let row = encode_utxo_with_codec(codec, utxo)?;
            kv_pairs.push((rocks_key.to_vec(), row));
        }
        encode_ms += t_encode.elapsed().as_millis() as u64;

        // Sort by rocks_key (LE vout) within chunk — required by bulk_load_sorted_kv.
        kv_pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        total_written += kv_pairs.len();
        let t_write = std::time::Instant::now();
        tree.bulk_load_sorted_kv(&kv_pairs)?;
        write_ms += t_write.elapsed().as_millis() as u64;
    }

    info!(
        "IBD engine checkpoint export complete: wrote {} UTXOs (fetch_ms={} encode_ms={} write_ms={})",
        total_written, fetch_ms, encode_ms, write_ms
    );
    Ok((muhash, total_written, fetch_ms, encode_ms, write_ms))
}

fn write_live_kvs(
    db: &UtxoDatabase,
    tree: &dyn Tree,
    live_kvs: &[super::types::OutputKV],
    tip_height: i32,
    codec: ValueCodec,
) -> Result<MuHash3072> {
    let total = live_kvs.len();
    info!(
        "IBD engine checkpoint export: {} live UTXOs at height {}",
        total, tip_height
    );

    // `live_kvs` arrives pre-sorted by output key from `scan_live_at_height` / `scan_all_live`.
    // Process directly in CHUNK_SIZE slices — no extra sort or full-Vec copy needed.
    // Each chunk is independently in ascending key order, satisfying `bulk_load_sorted_kv`.
    // Keep chunks aligned with EXPORT_CHUNK_SIZE: 2M details at once was an OOM amplifier
    // on the legacy Phase 3 path (details + encoded rows peak alongside the live Vec).
    const CHUNK_SIZE: usize = EXPORT_CHUNK_SIZE;
    let mut muhash = MuHash3072::new();
    let mut total_written = 0usize;
    let mut ser_buf: Vec<u8> = Vec::with_capacity(200);

    for chunk in live_kvs.chunks(CHUNK_SIZE) {
        // Filter to entries that have a real table id (Add entries only; id==0 are Deletes
        // that slipped through — should not happen but guard defensively).
        let chunk_ids: Vec<OutputId> = chunk
            .iter()
            .filter(|kv| kv.id != 0)
            .map(|kv| kv.id)
            .collect();
        let chunk_kvs: Vec<&super::types::OutputKV> =
            chunk.iter().filter(|kv| kv.id != 0).collect();

        let mut details = Vec::with_capacity(chunk_kvs.len());
        let fetched = db.fetch(&chunk_ids, &mut details)?;
        if fetched != chunk_kvs.len() {
            warn!(
                "IBD engine export chunk: fetched {} details but expected {} — engine/table mismatch",
                fetched,
                chunk_kvs.len()
            );
        }

        let mut kv_pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(chunk_kvs.len());
        for (rank, kv) in chunk_kvs.iter().enumerate() {
            let Some(detail) = details.get(rank) else {
                continue;
            };
            let op = output_key_to_outpoint(&kv.key);
            let rocks_key = outpoint_to_key(&op);
            let utxo = detail.utxo.as_ref();

            let preimage = serialize_coin_for_muhash(
                &op.hash,
                op.index,
                utxo.height as u32,
                utxo.is_coinbase,
                utxo.value,
                utxo.script_pubkey.as_ref(),
            );
            muhash.insert_mut(&preimage);

            ser_buf.clear();
            let row = encode_utxo_with_codec(codec, utxo)?;
            kv_pairs.push((rocks_key.to_vec(), row));
        }

        // Sort by rocks_key within each chunk. The input is sorted by output_key (BE vout)
        // but rocks_key uses LE vout encoding — for most entries these agree, but we sort
        // explicitly to guarantee the ascending order required by bulk_load_sorted_kv.
        kv_pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        total_written += kv_pairs.len();
        tree.bulk_load_sorted_kv(&kv_pairs)?;
        drop(kv_pairs);
        drop(details);
        drop(chunk_kvs);
        drop(chunk_ids);
    }

    info!(
        "IBD engine checkpoint export complete: wrote {} UTXOs",
        total_written
    );
    Ok(muhash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::ibd_engine::UtxoDatabase;
    use blvm_protocol::{
        Block, BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput,
    };
    use std::collections::HashMap;
    use tempfile::NamedTempFile;

    struct MockTree {
        data: std::sync::Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    }

    impl MockTree {
        fn new() -> Self {
            Self {
                data: std::sync::Mutex::new(HashMap::new()),
            }
        }
        fn get_value(&self, key: &[u8]) -> Option<Vec<u8>> {
            self.data.lock().unwrap().get(key).cloned()
        }
    }

    impl Tree for MockTree {
        fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
            self.data
                .lock()
                .unwrap()
                .insert(key.to_vec(), value.to_vec());
            Ok(())
        }
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            Ok(self.data.lock().unwrap().get(key).cloned())
        }
        fn remove(&self, key: &[u8]) -> Result<()> {
            self.data.lock().unwrap().remove(key);
            Ok(())
        }
        fn contains_key(&self, key: &[u8]) -> Result<bool> {
            Ok(self.data.lock().unwrap().contains_key(key))
        }
        fn clear(&self) -> Result<()> {
            self.data.lock().unwrap().clear();
            Ok(())
        }
        fn len(&self) -> Result<usize> {
            Ok(self.data.lock().unwrap().len())
        }
        fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
            let snapshot: Vec<_> = self
                .data
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| Ok((k.clone(), v.clone())))
                .collect();
            Box::new(snapshot.into_iter())
        }
        fn batch(&self) -> Result<Box<dyn crate::storage::database::BatchWriter + '_>> {
            Ok(Box::new(MockBatch {
                tree: self,
                ops: Vec::new(),
            }))
        }
    }

    struct MockBatch<'a> {
        tree: &'a MockTree,
        ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    }

    impl crate::storage::database::BatchWriter for MockBatch<'_> {
        fn put(&mut self, key: &[u8], value: &[u8]) {
            self.ops.push((key.to_vec(), Some(value.to_vec())));
        }
        fn delete(&mut self, key: &[u8]) {
            self.ops.push((key.to_vec(), None));
        }
        fn commit(self: Box<Self>) -> Result<()> {
            let mut data = self.tree.data.lock().unwrap();
            for (k, v_opt) in self.ops {
                match v_opt {
                    Some(v) => {
                        data.insert(k, v);
                    }
                    None => {
                        data.remove(&k);
                    }
                }
            }
            Ok(())
        }
        fn len(&self) -> usize {
            self.ops.len()
        }
    }

    fn make_coinbase(value: i64) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [0u8; 32],
                    index: 0xFFFFFFFF,
                },
                sequence: 0xFFFFFFFF,
                script_sig: vec![],
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value,
                script_pubkey: vec![0x76, 0xa9, 0x14, 0xde],
            }]
            .into(),
            lock_time: 0,
        }
    }

    fn make_block(txs: Vec<Transaction>) -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 0,
                bits: 0,
                nonce: 0,
            },
            transactions: txs.into_boxed_slice(),
        }
    }

    #[test]
    fn test_watermark_export_writes_utxos() {
        let tmp = NamedTempFile::new().unwrap();
        let db = UtxoDatabase::open(tmp.path(), 0).unwrap();
        let tree = Arc::new(MockTree::new());

        // Append a block with one coinbase output.
        let txid = [1u8; 32];
        let block = make_block(vec![make_coinbase(5_000_000_000)]);
        let _pin = db.append(&block, &[txid], 100).unwrap();

        let muhash = watermark_export(&db, tree.as_ref(), 100, ValueCodec::Bincode).unwrap();

        // The coinbase output's key in disk format is [txid || vout_le4 || pad4] = 40 bytes.
        let op = OutPoint {
            hash: txid,
            index: 0,
        };
        let key = outpoint_to_key(&op);
        let val = tree.get_value(&key);
        assert!(
            val.is_some(),
            "coinbase UTXO should have been written to tree"
        );

        // MuHash should be non-default (at least one entry was inserted).
        let empty = MuHash3072::new();
        // Comparing via serialized state (MuHash3072 doesn't impl PartialEq directly).
        let exported = muhash.serialize_running_state();
        let empty_state = empty.serialize_running_state();
        assert_ne!(
            exported, empty_state,
            "MuHash should have at least one entry"
        );
    }

    #[test]
    fn test_run_watermark_export_streaming_writes_utxos() {
        // Phase 3 production path: run_watermark_export must succeed via streaming
        // checkpoint export (not scan_all_live).
        let tmp = NamedTempFile::new().unwrap();
        let db = UtxoDatabase::open(tmp.path(), 0).unwrap();
        let tree: Arc<dyn Tree> = Arc::new(MockTree::new());

        let txid = [2u8; 32];
        let block = make_block(vec![make_coinbase(2_500_000_000)]);
        let _pin = db.append(&block, &[txid], 50).unwrap();

        crate::storage::ibd_engine::set_gc_fence(50);
        let muhash =
            run_watermark_export(&db, &tree, 50, ValueCodec::Bincode).unwrap();

        let op = OutPoint {
            hash: txid,
            index: 0,
        };
        let key = outpoint_to_key(&op);
        let val = tree.get(&key).unwrap();
        assert!(
            val.is_some(),
            "streaming Phase 3 path must write coinbase UTXO"
        );
        assert_ne!(
            muhash.serialize_running_state(),
            MuHash3072::new().serialize_running_state()
        );
    }

    #[test]
    fn partition_memory_overlay_respects_checkpoint_height() {
        let key_a = [1u8; 36];
        let key_b = [2u8; 36];
        let mem = vec![
            OutputKV::new_add(key_a, 10, 1),
            OutputKV::new_delete(key_a, 20), // above ckpt — must not suppress Add@10
            OutputKV::new_add(key_b, 5, 2),
            OutputKV::new_delete(key_b, 8), // at/below ckpt — Delete wins
        ];
        let (adds, deletes) = partition_memory_overlay(mem, 10);
        assert_eq!(adds.len(), 1);
        assert_eq!(adds[0].key, key_a);
        assert_eq!(deletes, vec![key_b]);
    }
}
