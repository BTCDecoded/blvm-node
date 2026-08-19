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
use super::types::{OutputHeader, OutputKV, OutputKey, outpoint_to_output_key};
use crate::storage::database::Tree;
use crate::storage::disk_utxo::key_to_outpoint;
use crate::storage::utxo_value_codec::{ValueCodec, decode_utxo_with_codec};
use anyhow::Result;
use blvm_protocol::types::UTXO;
use tracing::{info, warn};

#[cfg(feature = "heed3")]
use crate::storage::rkyv_codec::access_utxo;

#[cfg(target_os = "linux")]
use libc;
#[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
use libmimalloc_sys;

/// Number of UTXOs processed per batch (table write + channel send).
/// 50 k × 56 B ≈ 2.8 MB per batch; channel holds 2 batches ≈ 5.6 MB total.
const SEED_BATCH: usize = 50_000;

fn push_seed_batch(
    db: &UtxoDatabase,
    checkpoint_height: i32,
    batch_items: &mut Vec<(OutputKey, OutputHeader, Vec<u8>)>,
    entry_buf: &mut Vec<OutputKV>,
    tx: &std::sync::mpsc::SyncSender<OutputKV>,
    total: &mut usize,
) -> Result<bool> {
    if batch_items.is_empty() {
        return Ok(false);
    }
    entry_buf.clear();
    db.import_utxos(batch_items, checkpoint_height, entry_buf)?;
    *total += batch_items.len();
    batch_items.clear();
    for kv in entry_buf.drain(..) {
        if tx.send(kv).is_err() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn seed_entry_from_utxo(
    out_key: OutputKey,
    utxo: &UTXO,
    batch_items: &mut Vec<(OutputKey, OutputHeader, Vec<u8>)>,
) {
    let header = OutputHeader {
        height: utxo.height.min(i32::MAX as u64) as i32,
        flags: if utxo.is_coinbase { 1 } else { 0 },
        amount: utxo.value,
    };
    batch_items.push((out_key, header, utxo.script_pubkey.as_ref().to_vec()));
}

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
    codec: ValueCodec,
) -> Result<usize> {
    use std::cell::Cell;
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
    let channel_closed = Cell::new(false);

    #[cfg(feature = "heed3")]
    {
        if codec == ValueCodec::Rkyv {
            if let Some(h3) = tree.as_heed3_tree() {
                let scan = h3.scan_heed3(|key_bytes, val_bytes| {
                    if key_bytes.len() != 40 {
                        return Ok(());
                    }
                    let mut op_key = [0u8; 40];
                    op_key.copy_from_slice(key_bytes);
                    let op = key_to_outpoint(&op_key);
                    let out_key = outpoint_to_output_key(&op);
                    let archived = access_utxo(val_bytes)?;
                    let header = OutputHeader {
                        height: u64::from(archived.height).min(i32::MAX as u64) as i32,
                        flags: if archived.is_coinbase { 1 } else { 0 },
                        amount: archived.value.into(),
                    };
                    batch_items.push((out_key, header, archived.script_pubkey.to_vec()));
                    if batch_items.len() >= SEED_BATCH
                        && push_seed_batch(
                            db,
                            checkpoint_height,
                            &mut batch_items,
                            &mut entry_buf,
                            &tx,
                            &mut total,
                        )?
                    {
                        channel_closed.set(true);
                        return Ok(());
                    }
                    Ok(())
                });
                if let Err(e) = scan {
                    warn!(
                        "IBD engine seed: heed3 scan failed ({e:#}); falling back to tree.iter()"
                    );
                } else if !channel_closed.get() {
                    if push_seed_batch(
                        db,
                        checkpoint_height,
                        &mut batch_items,
                        &mut entry_buf,
                        &tx,
                        &mut total,
                    )? {
                        channel_closed.set(true);
                    }
                    if !channel_closed.get() {
                        drop(tx);
                        let seg = writer
                            .join()
                            .map_err(|_| anyhow::anyhow!("ibd-seed-writer thread panicked"))??;
                        if total == 0 && checkpoint_height > 0 {
                            anyhow::bail!(
                                "checkpoint tree empty at height {} — export incomplete or wrong slot",
                                checkpoint_height
                            );
                        }
                        if let Some(expected) = expected_count {
                            if expected > 0 && total as u64 != expected {
                                let delta = (total as u64).abs_diff(expected);
                                // Live 2026-07-16: tree 98581399 vs meta 98581401 (Δ=2) after
                                // Emergency checkpoint race — exact match refused seed forever.
                                const MAX_EXPORT_META_SLACK: u64 = 16;
                                if delta > MAX_EXPORT_META_SLACK {
                                    anyhow::bail!(
                                        "IBD engine seed: imported {} UTXOs but chain_info expected {} \
                                         at height {} (Δ={}) — checkpoint incomplete/poisoned (refusing seed)",
                                        total,
                                        expected,
                                        checkpoint_height,
                                        delta
                                    );
                                }
                                warn!(
                                    "IBD engine seed: imported {} UTXOs vs chain_info expected {} at h={} \
                                     (Δ={}) — accepting within slack {}",
                                    total,
                                    expected,
                                    checkpoint_height,
                                    delta,
                                    MAX_EXPORT_META_SLACK
                                );
                            }
                        }
                        if !crate::storage::ibd_autorepair::checkpoint_utxo_count_plausible(
                            checkpoint_height as u64,
                            total as u64,
                        ) {
                            anyhow::bail!(
                                "IBD engine seed: imported {} UTXOs at height {} fails \
                                 plausibility — checkpoint poisoned (refusing seed)",
                                total,
                                checkpoint_height
                            );
                        }
                        db.finalize_seed(seg, checkpoint_height);
                        db.flush_table_tail()?;
                        #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
                        unsafe {
                            libmimalloc_sys::mi_collect(true);
                        }
                        #[cfg(target_os = "linux")]
                        unsafe {
                            libc::malloc_trim(0);
                        }
                        info!(
                            "IBD engine: seeded {} UTXOs from checkpoint h={} in {:.1}s \
                             (heed3 zero-copy scan, peak UTXO buffer ≈ 6 MB)",
                            total,
                            checkpoint_height,
                            t0.elapsed().as_secs_f64()
                        );
                        return Ok(total);
                    }
                } else {
                    send_err = true;
                }
            }
        }
    }

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

        let utxo: UTXO = decode_utxo_with_codec(codec, &val_bytes)?;
        seed_entry_from_utxo(out_key, &utxo, &mut batch_items);

        if batch_items.len() >= SEED_BATCH
            && push_seed_batch(
                db,
                checkpoint_height,
                &mut batch_items,
                &mut entry_buf,
                &tx,
                &mut total,
            )?
        {
            send_err = true;
            break 'outer;
        }
    }

    if !send_err
        && push_seed_batch(
            db,
            checkpoint_height,
            &mut batch_items,
            &mut entry_buf,
            &tx,
            &mut total,
        )?
    {
        send_err = true;
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
            let delta = (total as u64).abs_diff(expected);
            // Live 2026-07-16: tree 98581399 vs meta 98581401 (Δ=2) after
            // Emergency checkpoint race — exact match refused seed forever.
            const MAX_EXPORT_META_SLACK: u64 = 16;
            if delta > MAX_EXPORT_META_SLACK {
                anyhow::bail!(
                    "IBD engine seed: imported {} UTXOs but chain_info expected {} \
                     at height {} (Δ={}) — checkpoint incomplete/poisoned (refusing seed)",
                    total,
                    expected,
                    checkpoint_height,
                    delta
                );
            }
            warn!(
                "IBD engine seed: imported {} UTXOs vs chain_info expected {} at h={} \
                 (Δ={}) — accepting within slack {}",
                total, expected, checkpoint_height, delta, MAX_EXPORT_META_SLACK
            );
        }
    }
    if !crate::storage::ibd_autorepair::checkpoint_utxo_count_plausible(
        checkpoint_height as u64,
        total as u64,
    ) {
        anyhow::bail!(
            "IBD engine seed: imported {} UTXOs at height {} fails \
             plausibility — checkpoint poisoned (refusing seed)",
            total,
            checkpoint_height
        );
    }

    // Register segment + commit watermark (contiguous_length + GC fence).
    db.finalize_seed(seg, checkpoint_height);

    // Flush all buffered tail entries to disk. The table flusher fires only for entries
    // with height < (max_seen − 512), but all seed entries share the same checkpoint_height
    // so it never fires. Without this call, ~12 GB of script data stays in anonymous
    // memory until the process exits.
    db.flush_table_tail()?;

    // Explicitly return freed pages to the OS before validation starts.
    #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
    #[cfg(target_os = "linux")]
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

/// One-shot migration: export legacy standalone `ibd_utxo_store/` into an engine checkpoint tree.
///
/// Used when engine mode is enabled but no ping-pong ckpt exists yet (in-flight legacy IBD).
/// Returns `Ok(true)` when a durable ckpt was written and is ready for [`seed_from_ibd_utxos`].
pub fn bootstrap_ckpt_from_legacy_standalone(
    storage: &crate::storage::Storage,
    utxo_store_dir: &std::path::Path,
    checkpoint_height: i32,
    codec: ValueCodec,
) -> Result<bool> {
    use crate::storage::database::create_ibd_utxo_standalone_db;
    use crate::storage::disk_utxo::key_to_outpoint;
    use crate::storage::ibd_engine::{
        ckpt_inactive_slot, ckpt_tree_for_slot, sync_tree_after_persist,
    };
    use blvm_muhash::{MuHash3072, serialize_coin_for_muhash};

    if !utxo_store_dir.exists() {
        return Ok(false);
    }
    let standalone_db = match create_ibd_utxo_standalone_db(utxo_store_dir) {
        Ok(d) => d,
        Err(e) => {
            warn!(
                "IBD engine migration: could not open legacy standalone at {}: {e:#}",
                utxo_store_dir.display()
            );
            return Ok(false);
        }
    };
    let legacy_tree = standalone_db.open_tree("ibd_utxos")?;
    if legacy_tree.is_empty().unwrap_or(true) {
        return Ok(false);
    }

    let write_slot = ckpt_inactive_slot(storage.chain().get_engine_ckpt_slot().unwrap_or(0));
    let ckpt_tree_name = ckpt_tree_for_slot(write_slot);
    let ckpt_tree = storage.open_tree(ckpt_tree_name)?;
    ckpt_tree.clear()?;

    let mut muhash = MuHash3072::new();
    let mut kv_pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(200_000);
    let mut total = 0usize;
    const CHUNK: usize = 200_000;

    info!(
        "IBD engine migration: exporting legacy standalone LMDB at {} → {} (h={})",
        utxo_store_dir.display(),
        ckpt_tree_name,
        checkpoint_height
    );

    for kv in legacy_tree.iter() {
        let (key_bytes, val_bytes) = kv?;
        if key_bytes.len() != 40 {
            continue;
        }
        let mut op_key = [0u8; 40];
        op_key.copy_from_slice(&key_bytes);
        let op = key_to_outpoint(&op_key);
        let utxo: UTXO = decode_utxo_with_codec(codec, &val_bytes)?;
        let preimage = serialize_coin_for_muhash(
            &op.hash,
            op.index,
            utxo.height.min(i32::MAX as u64) as u32,
            utxo.is_coinbase,
            utxo.value,
            utxo.script_pubkey.as_ref(),
        );
        muhash.insert_mut(&preimage);
        kv_pairs.push((key_bytes, val_bytes));
        if kv_pairs.len() >= CHUNK {
            kv_pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            ckpt_tree.bulk_load_sorted_kv(&kv_pairs)?;
            total += kv_pairs.len();
            kv_pairs.clear();
        }
    }
    if !kv_pairs.is_empty() {
        kv_pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        ckpt_tree.bulk_load_sorted_kv(&kv_pairs)?;
        total += kv_pairs.len();
    }

    if total == 0 {
        anyhow::bail!(
            "legacy standalone LMDB at {} appeared non-empty but exported 0 UTXOs",
            utxo_store_dir.display()
        );
    }

    let muhash_bytes = muhash.serialize_running_state();
    storage.chain().persist_engine_checkpoint_complete(
        checkpoint_height as u64,
        write_slot,
        total as u64,
        0,
        &muhash_bytes,
    )?;
    sync_tree_after_persist(ckpt_tree.as_ref())?;

    info!(
        "IBD engine migration: wrote {} UTXOs to {} (slot {}, h={})",
        total, ckpt_tree_name, write_slot, checkpoint_height
    );
    Ok(true)
}
