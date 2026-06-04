//! `UtxoDatabase`: combines `UtxoIndex` + `UtxoTable` into the complete IBD UTXO engine.
//!
//! The primary interface used by `SpendSession` (Phase 2) and the watermark export (Phase 3).
//!
//! ## Append flow (per block, on orchestrator thread)
//! 1. `table.append_outputs(block, tx_ids, height, &mut entries)` — encodes outputs to flat file.
//!    Returns `OutputKV::Add` entries with table IDs.
//! 2. Build `OutputKV::Delete` entries from block inputs (filtering coinbase + intra-block).
//! 3. `index.append(all_entries, height)` — inserts into age-0. Returns a `Pin`.
//!
//! ## Query flow (per block, on worker thread)
//! 1. `index.batch_query(sorted_keys, ids)` — fills ids from age layers.
//! 2. `table.fetch(ids, details)` — decodes `OutputDetail` from flat file / tail.
//!
//! ## Reorg flow
//! `erase_since(height)` — removes from mutable ages and rolls back table tail.

use super::index::UtxoIndex;
use super::memory_age::Pin;
use super::table::UtxoTable;
use super::types::{
    outpoint_to_output_key, to_output_key, IdCodec, OutputDetail, OutputHeader, OutputId, OutputKV,
    OutputKey,
};
use blvm_protocol::{transaction::is_coinbase, Block};
use std::path::Path;
use std::sync::Arc;

pub struct UtxoDatabase {
    pub(super) table: Arc<UtxoTable>,
    pub(super) index: UtxoIndex,
}

impl UtxoDatabase {
    /// Open or create the database at `table_path`.
    ///
    /// `avail_mb`: available system RAM in MiB. Pass `mem_guard.avail_mb_at_boot()` at the
    /// call site (parallel_ibd) so the UTXO index eviction age is derived from the same
    /// hardware snapshot that drives the rest of the IBD pipeline.  Pass `0` to fall back to
    /// `ram_tier::probe_avail_ram_mib()` (auto-detect at open time).
    ///
    /// Disk-evicted segment files are stored in `{table_path}.segs/`.
    /// Forward memory-guard pressure to the age-tiered index (`level_u8`: 0–3).
    pub fn memory_pressure_tick(&self, level_u8: u8) {
        self.index.memory_pressure_tick(level_u8);
    }

    pub fn open(table_path: impl AsRef<Path>, avail_mb: u64) -> anyhow::Result<Self> {
        let avail_mb = if avail_mb > 0 {
            avail_mb
        } else {
            crate::utils::ram_tier::probe_avail_ram_mib()
        };
        let table_path = table_path.as_ref();
        let seg_dir = {
            let mut p = table_path.as_os_str().to_owned();
            p.push(".segs");
            std::path::PathBuf::from(p)
        };
        let table = UtxoTable::open(table_path)?;
        let index = UtxoIndex::open(&seg_dir, avail_mb)?;
        Ok(Self { table, index })
    }

    /// Pre-append all outputs and record all spends for a block.
    ///
    /// Called on the **orchestrator thread** (sequentially). Returns a `Pin` that prevents
    /// the compacter from merging `height` away until the validation worker finishes.
    ///
    /// `tx_ids[i]` = sha256d(transactions[i]) — pre-computed, no re-hashing.
    pub fn append(&self, block: &Block, tx_ids: &[[u8; 32]], height: i32) -> anyhow::Result<Pin> {
        let (pin, _, _) = self.append_and_classify(block, tx_ids, height)?;
        Ok(pin)
    }

    /// Like `append` but also classifies inputs into external vs intra-block in a single pass.
    ///
    /// Eliminates the duplicate `HashSet` construction + input iteration that `SpendSession::append`
    /// previously performed after calling `UtxoDatabase::append`.  The caller receives the sorted
    /// `external_keys` (ready for `db.query`) and unsorted `intra_block_keys` (resolved from tail).
    pub fn append_and_classify(
        &self,
        block: &Block,
        tx_ids: &[[u8; 32]],
        height: i32,
    ) -> anyhow::Result<(Pin, Vec<OutputKey>, Vec<OutputKey>)> {
        let mut entries: Vec<OutputKV> = Vec::with_capacity(
            block
                .transactions
                .iter()
                .map(|tx| tx.outputs.len() + tx.inputs.len())
                .sum(),
        );

        // Phase 1: encode outputs → Add entries with table IDs.
        self.table
            .append_outputs(block, tx_ids, height, &mut entries)?;

        // Phase 2: build Delete entries + classify inputs in a single walk.
        // tx_ids[0] is the coinbase txid; tx_ids[1..] are the spendable non-coinbase txids.
        let tx_id_set: rustc_hash::FxHashSet<[u8; 32]> = tx_ids[1..].iter().copied().collect();

        let mut external_keys: Vec<OutputKey> = Vec::new();
        let mut intra_block_keys: Vec<OutputKey> = Vec::new();

        for tx in block.transactions.iter() {
            if is_coinbase(tx) {
                continue;
            }
            for input in tx.inputs.iter() {
                let key = outpoint_to_output_key(&input.prevout);
                if tx_id_set.contains(&input.prevout.hash) {
                    // Intra-block spend: resolved from engine tail, no Delete needed.
                    intra_block_keys.push(key);
                } else {
                    external_keys.push(key);
                    entries.push(OutputKV::new_delete(key, height));
                }
            }
        }

        // Sort + dedup external keys for Phase-2 batch_query (binary search).
        #[cfg(feature = "rayon")]
        {
            use rayon::prelude::*;
            external_keys.par_sort_unstable();
        }
        #[cfg(not(feature = "rayon"))]
        external_keys.sort_unstable();
        external_keys.dedup();

        let pin = self.index.append(entries, height);
        Ok((pin, external_keys, intra_block_keys))
    }

    /// Batch-resolve `sorted_keys` to `OutputId`s across all index ages.
    ///
    /// `ids` must be pre-filled with `OutputId::MAX`. After this call, `ids[i]` is the
    /// table ID for `sorted_keys[i]`, or `OutputId::MAX` if not found (disk fallback needed).
    ///
    /// `before` is an exclusive upper bound on entry height. `SpendSession` passes `before =
    /// current_height` so that Deletes appended for the current block do not shadow the Adds.
    pub fn query(&self, sorted_keys: &[OutputKey], ids: &mut [OutputId], before: i32) {
        debug_assert_eq!(sorted_keys.len(), ids.len());
        self.index.batch_query(sorted_keys, ids, before);
    }

    /// Fetch `OutputDetail` for resolved IDs. IDs are sorted by offset internally for read locality.
    pub fn fetch(
        &self,
        ids: &[OutputId],
        details: &mut Vec<OutputDetail>,
    ) -> anyhow::Result<usize> {
        self.table.fetch(ids, details)
    }

    /// Highest height that has been fully appended (all blocks 0..=height committed).
    /// Useful for partial-query logic: callers can query with `before=contiguous_length+1`
    /// to resolve any currently-committed outputs without waiting for future appends.
    pub fn contiguous_length(&self) -> i32 {
        self.index.contiguous_length()
    }

    /// Block until `contiguous_length >= height`. Watermark export only — NOT on hot path.
    pub fn wait_for_height(&self, height: i32) {
        self.index.wait_for_height(height);
    }

    /// Reorg: remove all data at `height >= since` from mutable ages.
    pub fn erase_since(&self, since: i32) {
        self.index.erase_since(since);
        let _ = self.table.commit_before(since); // flush tail before erase
    }

    /// Iterate all live (non-spent) entries across all ages. For watermark export.
    pub fn scan_live(&self) -> Vec<OutputKV> {
        self.index.scan_all_live()
    }

    /// Like `scan_live` but returns the UTXO set as of `max_height` even if the engine has
    /// advanced beyond that height. Used for periodic mid-IBD checkpoint exports.
    pub fn scan_live_at_height(&self, max_height: i32) -> Vec<OutputKV> {
        self.index.scan_live_at_height(max_height)
    }

    /// Streaming version of `scan_live_at_height`.
    ///
    /// Returns a `CheckpointStream` that yields live UTXOs one at a time via a k-way merge
    /// of (small) memory-age entries and (large) disk-segment entries. Peak extra allocation
    /// is ~330 MB rather than O(UTXO_count × 56 B) for the full-Vec path.
    pub fn iter_live_at_height(
        &self,
        max_height: i32,
    ) -> anyhow::Result<super::index::CheckpointStream> {
        self.index.iter_live_at_height(max_height)
    }

    /// Bulk-import UTXOs from `ibd_utxos` checkpoint (resume path). Writes table bytes and
    /// fills `entries` with Add OutputKVs tagged at `tag_height`.
    pub fn import_utxos(
        &self,
        items: &[(OutputKey, OutputHeader, Vec<u8>)],
        tag_height: i32,
        entries: &mut Vec<OutputKV>,
    ) -> anyhow::Result<()> {
        self.table.import_outputs_batch(items, tag_height, entries)
    }

    /// Append seed entries to the index and set `contiguous_length = checkpoint_height`.
    pub fn commit_seed_batch(&self, entries: &mut Vec<OutputKV>, checkpoint_height: i32) {
        self.index
            .seed_checkpoint(std::mem::take(entries), checkpoint_height);
    }

    /// Allocate a disk-segment slot for the streaming seed writer thread.
    /// Returns `(seg_idx, seg_dir)` — caller writes file, then calls `finalize_seed`.
    pub fn alloc_seed_seg(&self) -> (usize, std::path::PathBuf) {
        self.index.alloc_seed_seg()
    }

    /// Register the streaming-seed segment and commit the checkpoint watermark.
    pub fn finalize_seed(&self, seg: super::disk_segment::DiskSegment, checkpoint_height: i32) {
        self.index.finalize_seed(seg, checkpoint_height);
    }

    /// Flush all buffered table tail entries to disk.
    ///
    /// Must be called after `finalize_seed`: the table flusher never fires for seed entries
    /// because they all share the same height, so they accumulate as ~12 GB of anonymous
    /// memory until this is called.
    pub fn flush_table_tail(&self) -> anyhow::Result<()> {
        self.table.flush_all()
    }

    /// Convert a legacy `OutPointKey = [u8; 40]` to `OutputKey = [u8; 36]`.
    #[inline]
    pub fn to_output_key(k: &[u8; 40]) -> OutputKey {
        to_output_key(k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blvm_protocol::{
        Block, BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput,
    };
    use tempfile::NamedTempFile;

    fn make_txid(n: u8) -> [u8; 32] {
        let mut id = [0u8; 32];
        id[0] = n;
        id
    }

    fn coinbase_input() -> TransactionInput {
        TransactionInput {
            prevout: OutPoint {
                hash: [0u8; 32],
                index: 0xFFFFFFFF,
            },
            sequence: 0xFFFFFFFF,
            script_sig: vec![],
        }
    }

    fn spend_input(prev_hash: [u8; 32], prev_vout: u32) -> TransactionInput {
        TransactionInput {
            prevout: OutPoint {
                hash: prev_hash,
                index: prev_vout,
            },
            sequence: 0xFFFFFFFF,
            script_sig: vec![],
        }
    }

    fn dummy_coinbase_tx(value: i64) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![coinbase_input()].into(),
            outputs: vec![TransactionOutput {
                value,
                script_pubkey: vec![0x76, 0xa9, 0x14, 0x00],
            }]
            .into(),
            lock_time: 0,
        }
    }

    fn dummy_spend_tx(prev_hash: [u8; 32], prev_vout: u32, value: i64) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![spend_input(prev_hash, prev_vout)].into(),
            outputs: vec![TransactionOutput {
                value,
                script_pubkey: vec![0x51],
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
    fn test_append_query_basic() {
        let tmp = NamedTempFile::new().unwrap();
        let db = UtxoDatabase::open(tmp.path(), 0).unwrap();

        let block = make_block(vec![dummy_coinbase_tx(5_000_000_000)]);
        let tx_ids = vec![make_txid(1)];
        let _pin = db.append(&block, &tx_ids, 100).unwrap();

        let mut key: OutputKey = [0u8; 36];
        key[..32].copy_from_slice(&tx_ids[0]);
        // vout 0, big-endian
        key[32..36].copy_from_slice(&0u32.to_be_bytes());

        let mut ids = [OutputId::MAX; 1];
        db.query(&[key], &mut ids, i32::MAX);
        assert_ne!(ids[0], OutputId::MAX, "coinbase output should be in index");

        let mut details = Vec::new();
        let resolved = db.fetch(&ids, &mut details).unwrap();
        assert_eq!(resolved, 1);
        assert_eq!(details[0].header.amount, 5_000_000_000);
        assert!(details[0].header.is_coinbase());
    }

    #[test]
    fn test_intra_block_spend_filtered() {
        let tmp = NamedTempFile::new().unwrap();
        let db = UtxoDatabase::open(tmp.path(), 0).unwrap();

        // 3-tx block: coinbase, tx1 (creates output), tx2 (spends tx1's output intra-block).
        let txid_cb = make_txid(10); // coinbase — skipped (is_coinbase)
        let txid1 = make_txid(11); // non-coinbase, creates an output
        let txid2 = make_txid(12); // non-coinbase, spends txid1:0 (intra-block)

        let block = make_block(vec![
            dummy_coinbase_tx(5_000_000_000),
            // tx1: creates an output (will be the intra-block source)
            Transaction {
                version: 1,
                inputs: vec![spend_input(make_txid(99), 0)].into(), // external input
                outputs: vec![TransactionOutput {
                    value: 4_900_000_000,
                    script_pubkey: vec![0x51],
                }]
                .into(),
                lock_time: 0,
            },
            // tx2: spends tx1's output (txid1:0) — this is the intra-block spend
            dummy_spend_tx(txid1, 0, 4_800_000_000),
        ]);
        let tx_ids = vec![txid_cb, txid1, txid2];
        let _pin = db.append(&block, &tx_ids, 101).unwrap();

        // The Delete for txid1:0 should NOT be in the index (filtered as intra-block).
        // The Add for txid1:0 WAS recorded. So the key should be found.
        let mut key: OutputKey = [0u8; 36];
        key[..32].copy_from_slice(&txid1);
        key[32..36].copy_from_slice(&0u32.to_be_bytes());

        let mut ids = [OutputId::MAX; 1];
        db.query(&[key], &mut ids, i32::MAX);
        assert_ne!(
            ids[0],
            OutputId::MAX,
            "tx1 Add should be in index (intra-block Delete for tx2 filtered)"
        );
    }

    #[test]
    fn test_erase_since() {
        let tmp = NamedTempFile::new().unwrap();
        let db = UtxoDatabase::open(tmp.path(), 0).unwrap();

        let txid = make_txid(20);
        let block = make_block(vec![dummy_coinbase_tx(5_000_000_000)]);
        let _pin = db.append(&block, &[txid], 200).unwrap();

        db.erase_since(200);

        let mut key: OutputKey = [0u8; 36];
        key[..32].copy_from_slice(&txid);
        let mut ids = [OutputId::MAX; 1];
        db.query(&[key], &mut ids, i32::MAX);
        assert_eq!(
            ids[0],
            OutputId::MAX,
            "entry should be gone after erase_since"
        );
    }

    #[test]
    fn test_scan_live_basic() {
        let tmp = NamedTempFile::new().unwrap();
        let db = UtxoDatabase::open(tmp.path(), 0).unwrap();

        let txid = make_txid(30);
        let block = make_block(vec![dummy_coinbase_tx(5_000_000_000)]);
        let _pin = db.append(&block, &[txid], 300).unwrap();

        let live = db.scan_live();
        assert!(!live.is_empty());
        assert!(live.iter().any(|e| e.key[..32] == txid));
    }
}
