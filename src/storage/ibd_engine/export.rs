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
use super::types::{IdCodec, OutputId, output_key_to_outpoint};
use crate::storage::database::Tree;
use crate::storage::disk_utxo::outpoint_to_key;
use crate::storage::utxo_value_codec::{ValueCodec, encode_utxo_with_codec};
use anyhow::Result;
use blvm_muhash::{MuHash3072, serialize_coin_for_muhash};
use blvm_protocol::types::{SharedByteString, UTXO};
#[cfg(target_os = "linux")]
use libc;
#[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
use libmimalloc_sys;
use std::sync::Arc;
use tracing::{info, warn};

/// Ping-pong checkpoint tree names (must match `KNOWN_TREE_NAMES`).
pub const CKPT_TREE_A: &str = "ibd_utxos_ckpt_a";
pub const CKPT_TREE_B: &str = "ibd_utxos_ckpt_b";

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

/// Export all live UTXOs from the engine to `tree` and return the final `MuHash3072`.
///
/// `tip_height` must equal the engine's `contiguous_length()` — i.e. all blocks have been
/// appended. Blocks the caller until the engine reaches `tip_height`.
///
/// The tree must be the `ibd_utxos` tree (same as used by `IbdUtxoStore`).
/// The caller is responsible for persisting the returned MuHash to chain_info.
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

/// Convenience wrapper: export, persist MuHash, and log the result.
/// Returns the MuHash for the caller to set as the watermark in chain_info.
pub fn run_watermark_export(
    db: &UtxoDatabase,
    tree: &Arc<dyn Tree>,
    tip_height: i32,
    codec: ValueCodec,
) -> Result<MuHash3072> {
    let t = std::time::Instant::now();
    let muhash = watermark_export(db, tree.as_ref(), tip_height, codec)?;
    info!(
        "IBD engine watermark export finished in {:.1}s (height={})",
        t.elapsed().as_secs_f64(),
        tip_height
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
) -> Result<(MuHash3072, usize)> {
    let t = std::time::Instant::now();

    // Build a streaming iterator — compact disk + collect small memory Vec, no full-Vec scan.
    let mut stream = db.iter_live_at_height(checkpoint_height)?;

    tree.clear()?;
    let (muhash, count) =
        write_live_kvs_streaming(db, tree.as_ref(), &mut stream, checkpoint_height, codec)?;

    let elapsed = t.elapsed().as_secs_f64();

    // Drop stream state, then return freed pages to the OS immediately.
    drop(stream);
    #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
    info!(
        "IBD engine checkpoint export (replace) in {:.1}s (height={}, utxos={})",
        elapsed, checkpoint_height, count,
    );
    Ok((muhash, count))
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
) -> Result<(MuHash3072, usize)> {
    // 500k entries per chunk: ~28 MB (kvs) + ~50 MB (details) + ~60 MB (kv_pairs) ≈ 138 MB peak.
    const CHUNK_SIZE: usize = 500_000;
    let mut muhash = MuHash3072::new();
    let mut total_written = 0usize;
    let mut ser_buf: Vec<u8> = Vec::with_capacity(200);

    info!(
        "IBD engine checkpoint export (streaming): writing UTXOs at height {}",
        tip_height
    );

    let mut chunk_kvs: Vec<super::types::OutputKV> = Vec::with_capacity(CHUNK_SIZE);

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
        let chunk_ids: Vec<OutputId> = chunk_kvs.iter().map(|kv| kv.id).collect();
        let mut details = Vec::with_capacity(chunk_kvs.len());
        let fetched = db.fetch(&chunk_ids, &mut details)?;
        if fetched != chunk_kvs.len() {
            warn!(
                "checkpoint export chunk: fetched {} details but expected {}",
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

            let utxo = UTXO {
                value: detail.header.amount,
                script_pubkey: SharedByteString::from(detail.script.as_slice()),
                height: detail.header.height as u64,
                is_coinbase: detail.header.is_coinbase(),
            };

            let preimage = serialize_coin_for_muhash(
                &op.hash,
                op.index,
                detail.header.height as u32,
                detail.header.is_coinbase(),
                detail.header.amount,
                detail.script.as_slice(),
            );
            muhash.insert_mut(&preimage);

            ser_buf.clear();
            let row = encode_utxo_with_codec(codec, &utxo)?;
            kv_pairs.push((rocks_key.to_vec(), row));
        }

        // Sort by rocks_key (LE vout) within chunk — required by bulk_load_sorted_kv.
        kv_pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        total_written += kv_pairs.len();
        tree.bulk_load_sorted_kv(&kv_pairs)?;
    }

    info!(
        "IBD engine checkpoint export complete: wrote {} UTXOs",
        total_written
    );
    Ok((muhash, total_written))
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
    const CHUNK_SIZE: usize = 2_000_000;
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

            let utxo = UTXO {
                value: detail.header.amount,
                script_pubkey: SharedByteString::from(detail.script.as_slice()),
                height: detail.header.height as u64,
                is_coinbase: detail.header.is_coinbase(),
            };

            let preimage = serialize_coin_for_muhash(
                &op.hash,
                op.index,
                detail.header.height as u32,
                detail.header.is_coinbase(),
                detail.header.amount,
                detail.script.as_slice(),
            );
            muhash.insert_mut(&preimage);

            ser_buf.clear();
            let row = encode_utxo_with_codec(codec, &utxo)?;
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
}
