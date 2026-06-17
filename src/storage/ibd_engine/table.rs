//! `UtxoTable`: append-only flat file + in-memory tail for UTXO script data.
//!
//! Stores `{OutputHeader || raw_script_bytes}` in an append-only flat file plus in-memory tail.
//! per UTXO. The flat file grows monotonically; the in-memory tail holds recent blocks not yet
//! committed to disk.
//!
//! ## Fetch algorithm
//! 1. Decode all IDs to `(offset, length)`.
//! 2. Partition: tail-resident (offset ≥ committed_fence) resolved in-memory; rest go to disk.
//! 3. Disk reads via **io_uring** (Linux): all reads submitted in one batch, reaped together.
//!    Falls back to sequential `pread64` on non-Linux or if io_uring init fails.
//!
//! ## Tail flusher
//! Background flusher wakes on a **100 ms timer** and flushes any tail block older than
//! `(max_height_seen − mutable_window)`. The continuous background flusher
//! eliminates the periodic stalls caused by large tail accumulation.

use super::file_io;
use super::types::{IdCodec, OutputDetail, OutputHeader, OutputId, OutputKV, OutputKey};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

// Alias for the tail snapshot type.  Readers get an `Arc` clone of the snapshot under a
// brief read-lock; no data is copied.  Writers replace the inner `Arc<Vec<…>>` atomically.
type TailSnap = Arc<Vec<Arc<BlockOutputs>>>;

// ─── BlockOutputs ─────────────────────────────────────────────────────────────

/// In-memory block output buffer: raw `{OutputHeader || script_bytes}` for one block.
#[derive(Debug, Clone)]
pub(super) struct BlockOutputs {
    pub begin_offset: u64,
    pub height: i32,
    pub data: Vec<u8>,
}

impl BlockOutputs {
    pub fn get(&self, offset: u64, length: usize) -> Option<(OutputHeader, &[u8])> {
        let rel = offset.checked_sub(self.begin_offset)? as usize;
        let end = rel + length;
        if end > self.data.len() || length < OutputHeader::SIZE {
            return None;
        }
        // SAFETY: data contains valid `{OutputHeader || script}` bytes.
        let header =
            unsafe { std::ptr::read_unaligned(self.data[rel..].as_ptr() as *const OutputHeader) };
        Some((header, &self.data[rel + OutputHeader::SIZE..rel + length]))
    }
}

// ─── Disk-read batch entry ─────────────────────────────────────────────────────

/// One pending disk read: the `slot` is the index into `result_slots` for the final answer.
struct DiskRead {
    slot: usize,
    offset: u64,
    length: usize,
}

// ─── UtxoTable ────────────────────────────────────────────────────────────────

enum FlushMsg {
    Shutdown,
}

pub struct UtxoTable {
    /// Read handle: lock-free `pread64` / io_uring reads.
    read_file: File,
    /// Write handle: only the flusher thread writes.
    write_file: Arc<parking_lot::Mutex<File>>,
    /// Next write offset (monotonically increasing).
    next_offset: AtomicU64,
    /// Bytes committed to disk. Entries with `offset < fence` are on disk.
    committed_fence: AtomicU64,
    /// In-memory tail: RCU snapshot — readers clone the outer `Arc` under a brief read-lock
    /// and iterate without holding any lock.  Writers replace the `Arc<Vec<…>>` atomically.
    /// Previously `Mutex<Vec<Arc<BlockOutputs>>>` whose `.clone()` in the fetch hot-path
    /// copied up to `mutable_window` (512) `Arc` pointers under the exclusive lock.  Now the
    /// hot-path clone costs exactly one atomic reference-count increment.
    tail: parking_lot::RwLock<TailSnap>,
    /// Keep this many recent heights in the tail before flushing older ones.
    mutable_window: i32,
    /// Highest height seen by `append_outputs`.
    max_height_seen: AtomicI32,
    flusher_tx: crossbeam_channel::Sender<FlushMsg>,
}

impl UtxoTable {
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Arc<Self>> {
        let write_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.as_ref())?;
        let read_file = OpenOptions::new().read(true).open(path.as_ref())?;
        let existing_len = write_file.metadata()?.len();
        let (tx, rx) = crossbeam_channel::bounded::<FlushMsg>(4);

        let table = Arc::new(Self {
            read_file,
            write_file: Arc::new(parking_lot::Mutex::new(write_file)),
            next_offset: AtomicU64::new(existing_len),
            committed_fence: AtomicU64::new(existing_len),
            tail: parking_lot::RwLock::new(Arc::new(Vec::new())),
            mutable_window: 512,
            max_height_seen: AtomicI32::new(i32::MIN),
            flusher_tx: tx,
        });

        // Continuous timer-based tail flusher (100 ms wake interval).
        // Wakes every 100 ms; flushes tail blocks older than (max_h − mutable_window).
        {
            let weak = Arc::downgrade(&table);
            let mw = table.mutable_window;
            std::thread::Builder::new()
                .name("utxo-table-flusher".to_string())
                .spawn(move || {
                    loop {
                        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                            Ok(FlushMsg::Shutdown)
                            | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        }
                        let Some(t) = weak.upgrade() else { break };
                        let max_h = t.max_height_seen.load(Ordering::Relaxed);
                        if max_h > i32::MIN {
                            let _ = t.flush_before(max_h.saturating_sub(mw));
                        }
                    }
                })
                .expect("spawn table flusher");
        }

        Ok(table)
    }

    /// Append a block's outputs to the tail.
    pub fn append_outputs(
        &self,
        block: &blvm_protocol::Block,
        tx_ids: &[[u8; 32]],
        height: i32,
        entries: &mut Vec<OutputKV>,
    ) -> anyhow::Result<usize> {
        debug_assert_eq!(block.transactions.len(), tx_ids.len());

        let mut local_buf: Vec<u8> = Vec::with_capacity(block.transactions.len() * 64);
        let first_entry_idx = entries.len();

        for (tx_idx, (tx, txid)) in block.transactions.iter().zip(tx_ids.iter()).enumerate() {
            for (vout, output) in tx.outputs.iter().enumerate() {
                let script: &[u8] = output.script_pubkey.as_ref();
                let header = OutputHeader {
                    height,
                    flags: if tx_idx == 0 { 1 } else { 0 },
                    amount: output.value,
                };
                let entry_len = OutputHeader::SIZE + script.len();
                let header_bytes = unsafe {
                    std::slice::from_raw_parts(
                        &header as *const OutputHeader as *const u8,
                        OutputHeader::SIZE,
                    )
                };
                local_buf.extend_from_slice(header_bytes);
                local_buf.extend_from_slice(script);

                let mut key: OutputKey = [0u8; 36];
                key[..32].copy_from_slice(txid);
                key[32..36].copy_from_slice(&(vout as u32).to_be_bytes());
                entries.push(OutputKV::new_add(
                    key,
                    height,
                    IdCodec::encode(0, entry_len),
                ));
            }
        }

        let total = local_buf.len() as u64;
        let block_base = self.next_offset.fetch_add(total, Ordering::Relaxed);

        // Fix up entry offsets now that block_base is known.
        let mut file_offset = block_base;
        let mut entry_idx = first_entry_idx;
        for tx in block.transactions.iter() {
            for output in tx.outputs.iter() {
                let entry_len = OutputHeader::SIZE + output.script_pubkey.len();
                entries[entry_idx].id = IdCodec::encode(file_offset, entry_len);
                file_offset += entry_len as u64;
                entry_idx += 1;
            }
        }

        {
            let new_block = Arc::new(BlockOutputs {
                begin_offset: block_base,
                height,
                data: local_buf,
            });
            let mut w = self.tail.write();
            // Append to the snapshot: clone the inner Vec (cheap: Vec of Arc), push, replace.
            let mut new_vec: Vec<Arc<BlockOutputs>> = (**w).clone();
            new_vec.push(new_block);
            *w = Arc::new(new_vec);
        }
        self.max_height_seen.fetch_max(height, Ordering::Relaxed);

        Ok(entries.len())
    }

    /// Bulk-import outputs for checkpoint seeding (SIGKILL resume). Same layout as `append_outputs`.
    pub fn import_outputs_batch(
        &self,
        items: &[(OutputKey, OutputHeader, Vec<u8>)],
        tag_height: i32,
        entries: &mut Vec<OutputKV>,
    ) -> anyhow::Result<()> {
        if items.is_empty() {
            return Ok(());
        }

        let mut local_buf: Vec<u8> = Vec::with_capacity(items.len() * 64);
        let first_entry_idx = entries.len();

        for (key, header, script) in items {
            let entry_len = OutputHeader::SIZE + script.len();
            let header_bytes = unsafe {
                std::slice::from_raw_parts(
                    header as *const OutputHeader as *const u8,
                    OutputHeader::SIZE,
                )
            };
            local_buf.extend_from_slice(header_bytes);
            local_buf.extend_from_slice(script.as_slice());
            entries.push(OutputKV::new_add(
                *key,
                tag_height,
                IdCodec::encode(0, entry_len),
            ));
        }

        let total = local_buf.len() as u64;
        let block_base = self.next_offset.fetch_add(total, Ordering::Relaxed);

        let mut file_offset = block_base;
        let mut entry_idx = first_entry_idx;
        for (_, _header, script) in items {
            let entry_len = OutputHeader::SIZE + script.len();
            entries[entry_idx].id = IdCodec::encode(file_offset, entry_len);
            file_offset += entry_len as u64;
            entry_idx += 1;
        }

        {
            let new_block = Arc::new(BlockOutputs {
                begin_offset: block_base,
                height: tag_height,
                data: local_buf,
            });
            let mut w = self.tail.write();
            let mut new_vec: Vec<Arc<BlockOutputs>> = (**w).clone();
            new_vec.push(new_block);
            *w = Arc::new(new_vec);
        }
        self.max_height_seen
            .fetch_max(tag_height, Ordering::Relaxed);
        Ok(())
    }

    /// Fetch `OutputDetail` for each ID in `ids`.
    ///
    /// Tail-resident entries are resolved from the in-memory snapshot.
    /// Disk-resident entries are read via **io_uring** batch (or sequential `pread64` fallback).
    pub fn fetch(
        &self,
        ids: &[OutputId],
        details: &mut Vec<OutputDetail>,
    ) -> anyhow::Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }

        let fence = self.committed_fence.load(Ordering::Acquire);
        // RCU snapshot: one atomic refcount increment, no Vec clone, no exclusive lock.
        let tail_snap: TailSnap = Arc::clone(&*self.tail.read());

        let n = ids.len();
        let mut result_slots: Vec<Option<OutputDetail>> = vec![None; n];
        let mut disk: Vec<DiskRead> = Vec::new();

        for (slot, &id) in ids.iter().enumerate() {
            let (offset, length) = IdCodec::decode(id);

            // Check tail first.
            if offset >= fence && !tail_snap.is_empty() {
                let pos = tail_snap.partition_point(|b| b.begin_offset <= offset);
                if pos > 0 {
                    if let Some((hdr, script)) = tail_snap[pos - 1].get(offset, length) {
                        result_slots[slot] = Some(OutputDetail {
                            header: hdr,
                            script: script.to_vec(),
                        });
                        continue;
                    }
                }
            }

            disk.push(DiskRead {
                slot,
                offset,
                length,
            });
        }

        if !disk.is_empty() {
            // Sort by file offset for sequential / io_uring-friendly access.
            disk.sort_unstable_by_key(|e| e.offset);

            // Allocate one contiguous staging buffer for all reads.
            let total_bytes: usize = disk.iter().map(|e| e.length).sum();
            let mut staging = vec![0u8; total_bytes];

            // Pre-compute each read's region in the staging buffer.
            let mut stage_offs: Vec<usize> = Vec::with_capacity(disk.len());
            {
                let mut cursor = 0usize;
                for e in &disk {
                    stage_offs.push(cursor);
                    cursor += e.length;
                }
            }

            // Perform all reads in one batch.
            read_disk_batch(&self.read_file, &disk, &stage_offs, &mut staging)?;

            // Unpack staging into result slots.
            for (e, &soff) in disk.iter().zip(stage_offs.iter()) {
                let data = &staging[soff..soff + e.length];
                if data.len() >= OutputHeader::SIZE {
                    let hdr =
                        unsafe { std::ptr::read_unaligned(data.as_ptr() as *const OutputHeader) };
                    result_slots[e.slot] = Some(OutputDetail {
                        header: hdr,
                        script: data[OutputHeader::SIZE..].to_vec(),
                    });
                }
            }
        }

        let mut resolved = 0;
        for d in result_slots.into_iter().flatten() {
            details.push(d);
            resolved += 1;
        }
        Ok(resolved)
    }

    /// Flush tail blocks with `height < limit` to disk.
    /// Flush all tail entries to disk immediately, regardless of height.
    ///
    /// Called once after checkpoint seeding: all 250M+ seed entries share the same
    /// `tag_height` so the normal timer-based flusher (`flush_before(max_h - 512)`)
    /// never fires for them, leaving ~12 GB of script data in anonymous memory.
    /// This brings RSS down to the bloom-filter + RocksDB baseline (~1–2 GB) before
    /// validation resumes.
    pub fn flush_all(&self) -> anyhow::Result<()> {
        self.flush_before(i32::MAX)
    }

    fn flush_before(&self, limit: i32) -> anyhow::Result<()> {
        let to_flush: Vec<Arc<BlockOutputs>> = {
            let snap = self.tail.read();
            snap.iter().filter(|b| b.height < limit).cloned().collect()
        };
        if to_flush.is_empty() {
            return Ok(());
        }

        let mut ordered = to_flush.clone();
        ordered.sort_by_key(|b| b.begin_offset);

        {
            let mut file = self.write_file.lock();
            for b in &ordered {
                file.write_all(&b.data)?;
            }
            file.flush()?;
        }

        // Advance committed_fence.
        let max_end = ordered
            .iter()
            .map(|b| b.begin_offset + b.data.len() as u64)
            .max()
            .unwrap_or(0);
        let mut cur = self.committed_fence.load(Ordering::Acquire);
        loop {
            if max_end <= cur {
                break;
            }
            match self.committed_fence.compare_exchange_weak(
                cur,
                max_end,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => cur = v,
            }
        }

        // Remove flushed entries from the RCU tail snapshot.
        let flushed: std::collections::HashSet<u64> =
            ordered.iter().map(|b| b.begin_offset).collect();
        {
            let mut w = self.tail.write();
            let new_vec: Vec<Arc<BlockOutputs>> = w
                .iter()
                .filter(|b| !flushed.contains(&b.begin_offset))
                .cloned()
                .collect();
            *w = Arc::new(new_vec);
        }
        Ok(())
    }

    /// Synchronous flush: used at shutdown or for watermark export.
    pub fn commit_before(&self, limit: i32) -> anyhow::Result<()> {
        self.flush_before(limit)
    }
}

impl Drop for UtxoTable {
    fn drop(&mut self) {
        let _ = self.flusher_tx.try_send(FlushMsg::Shutdown);
    }
}

// ─── Batch disk read ──────────────────────────────────────────────────────────

/// Read all `entries` into `staging` using io_uring (Linux) or sequential `pread64`.
fn read_disk_batch(
    file: &File,
    entries: &[DiskRead],
    stage_offs: &[usize],
    staging: &mut Vec<u8>,
) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        if uring::read_batch(file, entries, stage_offs, staging).is_ok() {
            return Ok(());
        }
    }

    // Sequential pread64 fallback.
    let mut buf = Vec::new();
    for (e, &soff) in entries.iter().zip(stage_offs.iter()) {
        buf.resize(e.length, 0u8);
        file_io::read_at(file, &mut buf, e.offset)?;
        staging[soff..soff + e.length].copy_from_slice(&buf);
    }
    Ok(())
}

// ─── io_uring (Linux only) ───────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod uring {
    use io_uring::{IoUring, opcode, types};
    use std::cell::RefCell;
    use std::os::unix::io::AsRawFd;

    const QUEUE_DEPTH: u32 = 1024;

    thread_local! {
        static RING: RefCell<Option<IoUring>> = const { RefCell::new(None) };
    }

    /// Submit all `entries` as a single io_uring read batch.
    ///
    /// Each entry `i` is read from `file` at `entries[i].offset` (length `entries[i].length`)
    /// into `staging[stage_offs[i]..stage_offs[i]+length]`.
    ///
    /// Processes in chunks of `QUEUE_DEPTH` so the submission queue never overflows.
    pub fn read_batch(
        file: &std::fs::File,
        entries: &[super::DiskRead],
        stage_offs: &[usize],
        staging: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let fd = file.as_raw_fd();
        // We capture the raw pointer BEFORE handing staging to io_uring.
        // staging is exclusively owned and not moved until after submit_and_wait.
        let staging_ptr = staging.as_mut_ptr();

        RING.with(|cell| -> anyhow::Result<()> {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                *opt = Some(IoUring::new(QUEUE_DEPTH)?);
            }
            let ring = opt.as_mut().unwrap();

            let chunk = QUEUE_DEPTH as usize;
            let mut i = 0;
            while i < entries.len() {
                let end = (i + chunk).min(entries.len());

                // Push SQEs.
                {
                    let mut sq = ring.submission();
                    for j in i..end {
                        let e = &entries[j];
                        // SAFETY: staging_ptr is valid and exclusively owned.
                        // Each (j, stage_off) covers a non-overlapping region.
                        // staging is not touched until submit_and_wait returns.
                        let ptr = unsafe { staging_ptr.add(stage_offs[j]) };
                        let sqe = opcode::Read::new(types::Fd(fd), ptr, e.length as u32)
                            .offset(e.offset)
                            .build()
                            .user_data(j as u64);
                        // SAFETY: buf_ptr is valid for the duration of the kernel op.
                        unsafe {
                            sq.push(&sqe)
                                .map_err(|_| anyhow::anyhow!("io_uring sq full"))?;
                        }
                    }
                } // drop sq borrow before submit

                ring.submit_and_wait(end - i)?;

                for cqe in ring.completion() {
                    if cqe.result() < 0 {
                        tracing::warn!(
                            "io_uring read err={} idx={}",
                            cqe.result(),
                            cqe.user_data()
                        );
                        // Staging region stays zeroed → will be skipped in unpack.
                    }
                }

                i = end;
            }
            Ok(())
        })
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use blvm_protocol::{
        Block, BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput,
    };
    use tempfile::NamedTempFile;

    fn make_output(value: i64, tag: u8) -> TransactionOutput {
        TransactionOutput {
            value,
            script_pubkey: vec![0x76, 0xa9, 0x14, tag],
        }
    }
    fn make_coinbase_input() -> TransactionInput {
        TransactionInput {
            prevout: OutPoint {
                hash: [0u8; 32],
                index: 0xFFFFFFFF,
            },
            sequence: 0xFFFFFFFF,
            script_sig: vec![],
        }
    }
    fn dummy_block(n_tx: usize, n_out_each: usize, start_value: i64) -> Block {
        let txs: Box<[Transaction]> = (0..n_tx)
            .map(|_| {
                let outputs = (0..n_out_each)
                    .map(|i| make_output(start_value + i as i64, i as u8))
                    .collect::<Vec<_>>();
                Transaction {
                    version: 1,
                    inputs: vec![make_coinbase_input()].into(),
                    outputs: outputs.into(),
                    lock_time: 0,
                }
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 0,
                bits: 0,
                nonce: 0,
            },
            transactions: txs,
        }
    }

    #[test]
    fn test_append_and_fetch() {
        let tmp = NamedTempFile::new().unwrap();
        let table = UtxoTable::open(tmp.path()).unwrap();

        let block = dummy_block(2, 3, 1000);
        let tx_ids: Vec<[u8; 32]> = block
            .transactions
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let mut id = [0u8; 32];
                id[0] = i as u8;
                id
            })
            .collect();
        let mut entries = Vec::new();
        table
            .append_outputs(&block, &tx_ids, 100, &mut entries)
            .unwrap();
        assert_eq!(entries.len(), 6);

        let ids: Vec<OutputId> = entries.iter().map(|e| e.id).collect();
        let mut details = Vec::new();
        let resolved = table.fetch(&ids, &mut details).unwrap();
        assert_eq!(resolved, 6);
        for d in &details {
            assert_eq!(d.header.height, 100);
        }
    }
}
