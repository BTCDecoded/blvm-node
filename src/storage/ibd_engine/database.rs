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
    outpoint_to_output_key, to_output_key, IdCodec, OutputDetail, OutputId, OutputKey, OutputKV,
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
    pub fn open(table_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let table = UtxoTable::open(table_path)?;
        let index = UtxoIndex::new();
        Ok(Self { table, index })
    }

    /// Pre-append all outputs and record all spends for a block.
    ///
    /// Called on the **orchestrator thread** (sequentially). Returns a `Pin` that prevents
    /// the compacter from merging `height` away until the validation worker finishes.
    ///
    /// `tx_ids[i]` = sha256d(transactions[i]) — pre-computed, no re-hashing.
    pub fn append(&self, block: &Block, tx_ids: &[[u8; 32]], height: i32) -> anyhow::Result<Pin> {
        let mut entries: Vec<OutputKV> = Vec::with_capacity(
            block.transactions.iter().map(|tx| tx.outputs.len() + tx.inputs.len()).sum(),
        );

        // Phase 1: encode outputs → Add entries with table IDs.
        self.table.append_outputs(block, tx_ids, height, &mut entries)?;

        // Phase 2: build Delete entries from inputs.
        // Filter: skip coinbase tx (tx_idx == 0), skip intra-block inputs (prevout.hash ∈ tx_ids).
        let tx_id_set: std::collections::HashSet<[u8; 32]> =
            tx_ids[1..].iter().copied().collect(); // tx_ids[0] is coinbase; [1..] are spendable

        for (tx_idx, tx) in block.transactions.iter().enumerate() {
            if is_coinbase(tx) {
                continue; // skip entire coinbase transaction (tx_idx == 0)
            }
            let _ = tx_idx;
            for input in tx.inputs.iter() {
                let prev_hash = input.prevout.hash;
                if tx_id_set.contains(&prev_hash) {
                    // Intra-block: funded by an earlier non-coinbase tx in this block.
                    // Resolved locally by the engine's tail — no Delete needed in index.
                    continue;
                }
                let key = outpoint_to_output_key(&input.prevout);
                entries.push(OutputKV::new_delete(key, height));
            }
        }

        let pin = self.index.append(entries, height);
        Ok(pin)
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

    /// Convert a legacy `OutPointKey = [u8; 40]` to `OutputKey = [u8; 36]`.
    #[inline]
    pub fn to_output_key(k: &[u8; 40]) -> OutputKey {
        to_output_key(k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blvm_protocol::{Block, BlockHeader, Transaction, TransactionInput, TransactionOutput, OutPoint};
    use tempfile::NamedTempFile;

    fn make_txid(n: u8) -> [u8; 32] {
        let mut id = [0u8; 32];
        id[0] = n;
        id
    }

    fn coinbase_input() -> TransactionInput {
        TransactionInput {
            prevout: OutPoint { hash: [0u8; 32], index: 0xFFFFFFFF },
            sequence: 0xFFFFFFFF,
            script_sig: vec![].into(),
        }
    }

    fn spend_input(prev_hash: [u8; 32], prev_vout: u32) -> TransactionInput {
        TransactionInput {
            prevout: OutPoint { hash: prev_hash, index: prev_vout },
            sequence: 0xFFFFFFFF,
            script_sig: vec![].into(),
        }
    }

    fn dummy_coinbase_tx(value: i64) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![coinbase_input()].into(),
            outputs: vec![TransactionOutput {
                value,
                script_pubkey: vec![0x76, 0xa9, 0x14, 0x00].into(),
            }].into(),
            lock_time: 0,
        }
    }

    fn dummy_spend_tx(prev_hash: [u8; 32], prev_vout: u32, value: i64) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![spend_input(prev_hash, prev_vout)].into(),
            outputs: vec![TransactionOutput {
                value,
                script_pubkey: vec![0x51].into(),
            }].into(),
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
        let db = UtxoDatabase::open(tmp.path()).unwrap();

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
        let db = UtxoDatabase::open(tmp.path()).unwrap();

        // 3-tx block: coinbase, tx1 (creates output), tx2 (spends tx1's output intra-block).
        let txid_cb = make_txid(10);  // coinbase — skipped (is_coinbase)
        let txid1 = make_txid(11);   // non-coinbase, creates an output
        let txid2 = make_txid(12);   // non-coinbase, spends txid1:0 (intra-block)

        let block = make_block(vec![
            dummy_coinbase_tx(5_000_000_000),
            // tx1: creates an output (will be the intra-block source)
            Transaction {
                version: 1,
                inputs: vec![spend_input(make_txid(99), 0)].into(), // external input
                outputs: vec![TransactionOutput { value: 4_900_000_000, script_pubkey: vec![0x51].into() }].into(),
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
        assert_ne!(ids[0], OutputId::MAX, "tx1 Add should be in index (intra-block Delete for tx2 filtered)");
    }

    #[test]
    fn test_erase_since() {
        let tmp = NamedTempFile::new().unwrap();
        let db = UtxoDatabase::open(tmp.path()).unwrap();

        let txid = make_txid(20);
        let block = make_block(vec![dummy_coinbase_tx(5_000_000_000)]);
        let _pin = db.append(&block, &[txid], 200).unwrap();

        db.erase_since(200);

        let mut key: OutputKey = [0u8; 36];
        key[..32].copy_from_slice(&txid);
        let mut ids = [OutputId::MAX; 1];
        db.query(&[key], &mut ids, i32::MAX);
        assert_eq!(ids[0], OutputId::MAX, "entry should be gone after erase_since");
    }

    #[test]
    fn test_scan_live_basic() {
        let tmp = NamedTempFile::new().unwrap();
        let db = UtxoDatabase::open(tmp.path()).unwrap();

        let txid = make_txid(30);
        let block = make_block(vec![dummy_coinbase_tx(5_000_000_000)]);
        let _pin = db.append(&block, &[txid], 300).unwrap();

        let live = db.scan_live();
        assert!(!live.is_empty());
        assert!(live.iter().any(|e| e.key[..32] == txid));
    }
}
