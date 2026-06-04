//! Checkpoint import: rebuild the in-memory engine index from a checkpoint tree on SIGKILL resume.
//!
//! The age-tiered index (ages 0–2) is purely in-memory and cannot survive a kill. Periodic
//! checkpoint exports write an exact live UTXO snapshot to a ping-pong tree (`ibd_utxos_ckpt_a`
//! / `ibd_utxos_ckpt_b`). On resume we open a fresh engine, bulk-import those entries, and set
//! `contiguous_length = checkpoint_height` so validation can continue with engine-mode performance.
//!
//! ## Memory model
//!
//! A naïve implementation reads all checkpoint UTXOs into a `Vec<OutputKV>` before sorting and
//! writing the disk segment — at 250 M entries × 56 B = **14 GB** that OOMs an 8 GB machine.
//!
//! This implementation uses a bounded `mpsc::sync_channel` to pipe entries from the RocksDB
//! iterator to a background writer thread that calls `DiskSegment::write_from_iter` directly.
//! Peak RSS from the UTXO buffer is O(SEED_BATCH × 56 B × 2) ≈ **6 MB** regardless of UTXO count.
//!
//! No sorting is needed because RocksDB iterates keys in byte order and all seed entries are
//! `Add` ops at the same `checkpoint_height` — so `OutputKV` sort order equals key order.

use super::database::UtxoDatabase;
use super::disk_segment::DiskSegment;
use super::types::{outpoint_to_output_key, OutputHeader, OutputKV, OutputKey};
use crate::storage::database::Tree;
use crate::storage::disk_utxo::key_to_outpoint;
use anyhow::Result;
use blvm_protocol::types::UTXO;
use tracing::{info, warn};

#[allow(unused_imports)]
use libc;
#[allow(unused_imports)]
use libmimalloc_sys;

/// Number of UTXOs processed per batch (table write + channel send).
/// 50 k × 56 B ≈ 2.8 MB per batch; channel holds 2 batches ≈ 5.6 MB total.
const SEED_BATCH: usize = 50_000;

/// Rebuild engine state from the durable checkpoint tree at `checkpoint_height`.
///
/// The tree must be an exact snapshot (clear + write export). Returns the number of UTXOs imported.
///
/// Peak memory: O(SEED_BATCH × 56 B × 2) ≈ 6 MB — safe on an 8 GB Raspberry Pi 5.
pub fn seed_from_ibd_utxos(
    db: &UtxoDatabase,
    tree: &dyn Tree,
    checkpoint_height: i32,
    expected_count: Option<u64>,
) -> Result<usize> {
    use std::sync::mpsc;
    use std::thread;

    let t0 = std::time::Instant::now();

    // Bloom-filter capacity: over/under-estimating by 2× only changes FPR, not correctness.
    let capacity = expected_count.unwrap_or(300_000_000) as usize;

    // Allocate the disk-segment slot before spawning the writer thread.
    let (seg_idx, seg_dir) = db.alloc_seed_seg();

    // Bounded channel: producer sends OutputKVs in SEED_BATCH-sized bursts; the bound keeps
    // in-flight memory at ≤ 2 × SEED_BATCH × 56 B ≈ 5.6 MB.
    let (tx, rx) = mpsc::sync_channel::<OutputKV>(SEED_BATCH * 2);

    // Writer thread: receives OutputKVs from the channel and writes a single disk segment.
    // RocksDB iterates keys in byte order; all entries are same-height Adds — already sorted.
    let writer = thread::Builder::new()
        .name("ibd-seed-writer".to_string())
        .spawn(move || -> anyhow::Result<DiskSegment> {
            DiskSegment::write_from_iter(&seg_dir, seg_idx, capacity, rx.into_iter())
        })?;

    // Producer: iterate tree → import to flat table → send OutputKVs to writer.
    let mut total = 0usize;
    let mut batch_items: Vec<(OutputKey, OutputHeader, Vec<u8>)> = Vec::with_capacity(SEED_BATCH);
    let mut entry_buf = Vec::<OutputKV>::with_capacity(SEED_BATCH);
    let mut send_err = false;

    'outer: for kv in tree.iter() {
        let (key_bytes, val_bytes) = kv?;
        if key_bytes.len() != 40 {
            continue;
        }
        let mut op_key = [0u8; 40];
        op_key.copy_from_slice(&key_bytes);
        // ibd_utxos keys write vout as u64 BE; go via OutPoint to avoid corruption.
        let op = key_to_outpoint(&op_key);
        let out_key = outpoint_to_output_key(&op);

        let utxo: UTXO = bincode::deserialize(&val_bytes)
            .map_err(|e| anyhow::anyhow!("seed deserialize UTXO: {e}"))?;
        let header = OutputHeader {
            height: utxo.height.min(i32::MAX as u64) as i32,
            flags: if utxo.is_coinbase { 1 } else { 0 },
            amount: utxo.value,
        };
        batch_items.push((out_key, header, utxo.script_pubkey.as_ref().to_vec()));

        if batch_items.len() >= SEED_BATCH {
            entry_buf.clear();
            db.import_utxos(&batch_items, checkpoint_height, &mut entry_buf)?;
            total += batch_items.len();
            batch_items.clear();
            for kv in entry_buf.drain(..) {
                if tx.send(kv).is_err() {
                    send_err = true;
                    break 'outer;
                }
            }
        }
    }

    if !send_err && !batch_items.is_empty() {
        entry_buf.clear();
        db.import_utxos(&batch_items, checkpoint_height, &mut entry_buf)?;
        total += batch_items.len();
        for kv in entry_buf.drain(..) {
            if tx.send(kv).is_err() {
                send_err = true;
                break;
            }
        }
    }

    // Close sender → writer thread sees end-of-iterator → finalises segment file.
    drop(tx);
    let seg = writer
        .join()
        .map_err(|_| anyhow::anyhow!("ibd-seed-writer thread panicked"))??;

    if send_err {
        anyhow::bail!("ibd-seed-writer thread exited early; disk segment may be incomplete");
    }

    if total == 0 && checkpoint_height > 0 {
        anyhow::bail!(
            "checkpoint tree empty at height {} — export incomplete or wrong slot",
            checkpoint_height
        );
    }

    if let Some(expected) = expected_count {
        if expected > 0 && total as u64 != expected {
            warn!(
                "IBD engine seed: imported {} UTXOs but chain_info expected {} at height {}",
                total, expected, checkpoint_height
            );
        }
    }

    // Register segment + commit watermark (contiguous_length + GC fence).
    db.finalize_seed(seg, checkpoint_height);

    // Flush all buffered tail entries to disk. The table flusher fires only for entries
    // with height < (max_seen − 512), but all seed entries share the same checkpoint_height
    // so it never fires. Without this call, ~12 GB of script data stays in anonymous
    // memory until the process exits.
    db.flush_table_tail()?;

    // Explicitly return freed pages to the OS before validation starts.
    #[cfg(feature = "mimalloc")]
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
    unsafe {
        libc::malloc_trim(0);
    }

    info!(
        "IBD engine: seeded {} UTXOs from checkpoint h={} in {:.1}s \
         (streaming, peak UTXO buffer ≈ 6 MB)",
        total,
        checkpoint_height,
        t0.elapsed().as_secs_f64()
    );
    Ok(total)
}
