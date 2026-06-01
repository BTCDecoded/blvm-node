//! `SpendSession`: one-block UTXO resolution session.
//!
//! Replaces the IBD prefetch pool + `build_spec_adds` + `utxo_base` HashMap build.
//!
//! ## Protocol
//! Called sequentially on the orchestrator/dispatch thread (h=1, h=2, h=3, ...). This
//! ordering is guaranteed by the single-threaded dispatch, not by any bridge or barrier.
//!
//! ## Intra-block filtering
//! Replicates `block_input_keys_into_filtered_with_tx_ids` logic:
//! - Skip entire coinbase transaction (`is_coinbase(tx)`)
//! - For non-coinbase txs: skip inputs whose `prevout.hash ∈ {tx_ids[1..spending_idx]}`
//!   (i.e., funded by an earlier non-coinbase tx in this block). These are resolved from
//!   the engine's in-memory tail via `local_spends`.
//!
//! ## Building `UtxoSet` for consensus
//! Phase 2 uses `session_to_utxo_set` which creates one `Arc<UTXO>` per entry — down
//! from 2–3 in the previous prefetch path. `SharedByteString::from` is inline-only for
//! P2PKH/P2SH/P2WPKH (~97% of UTXOs, scripts ≤ 25 bytes).
//!
//! Phase 2B (future): implement `UtxoLookup` over `SpendSession` and add a
//! `connect_block_ibd_view` entry point in consensus to eliminate the `UtxoSet` entirely.

use super::database::UtxoDatabase;
use super::memory_age::Pin;
use super::types::{output_key_to_outpoint, outpoint_to_output_key, IdCodec, OutputDetail, OutputId, OutputKey};
use blvm_protocol::transaction::is_coinbase;
use blvm_protocol::{Block, OutPoint, UTXO};
use blvm_protocol::types::SharedByteString;
use rustc_hash::FxHashMap;
use std::sync::Arc;

/// Resolved inputs for one block. Built by `SpendSession::resolve` on the dispatch thread;
/// consumed on the worker thread to build the `UtxoSet` for `validate_block_only`.
pub struct SpendSession {
    /// Resolved external inputs (from prior blocks), in query order.
    pub details: Vec<OutputDetail>,
    /// Maps `OutputKey → index into details`. Worker uses this for UTXO lookup.
    pub key_to_idx: FxHashMap<OutputKey, usize>,
    /// Intra-block inputs (funded by earlier non-coinbase txs in the same block).
    /// Resolved from the engine's in-memory tail outputs appended in `db.append`.
    pub local_spends: FxHashMap<OutputKey, OutputDetail>,
    /// Pin keeping `height` in the mutable window until the worker drops it.
    pub pin: Pin,
}

impl SpendSession {
    /// Resolve all external inputs for `block` using the UTXO engine.
    ///
    /// Steps:
    /// 1. `db.append(block, tx_ids, height)` — preemptively records outputs + external deletes.
    /// 2. Classify inputs (coinbase skip, intra-block → `local_spends`, external → query).
    /// 3. `db.query(sorted_external_keys)` → `OutputId[]`.
    /// 4. `db.fetch(resolved_ids)` → `OutputDetail[]`.
    /// 5. Build `key_to_idx` for worker O(1) lookup.
    pub fn resolve(
        db: &UtxoDatabase,
        block: &Block,
        tx_ids: &[[u8; 32]],
        height: i32,
    ) -> anyhow::Result<Self> {
        // Step 1: append outputs + external deletes.
        let pin = db.append(block, tx_ids, height)?;

        // Step 2: classify inputs.
        // tx_ids[1..] = non-coinbase txids in this block (spendable intra-block sources).
        let tx_id_set: std::collections::HashSet<[u8; 32]> =
            tx_ids[1..].iter().copied().collect();

        let mut external_keys: Vec<OutputKey> = Vec::new();
        let mut intra_block_keys: Vec<OutputKey> = Vec::new();

        for tx in block.transactions.iter() {
            if is_coinbase(tx) {
                continue; // entire coinbase tx skipped
            }
            for input in tx.inputs.iter() {
                let key = outpoint_to_output_key(&input.prevout);
                if tx_id_set.contains(&input.prevout.hash) {
                    // Intra-block: funded by an earlier non-coinbase tx in this block.
                    intra_block_keys.push(key);
                } else {
                    external_keys.push(key);
                }
            }
        }

        // Step 3: sort external keys and query.
        // Sort for binary search in db.query (MemoryRun.batch_lookup expects sorted keys).
        #[cfg(feature = "rayon")]
        {
            use rayon::prelude::*;
            external_keys.par_sort_unstable();
        }
        #[cfg(not(feature = "rayon"))]
        external_keys.sort_unstable();

        // Deduplicate (same outpoint spent by multiple txs — corner case, legal in pre-BIP30 era).
        external_keys.dedup();

        let mut ids: Vec<OutputId> = vec![OutputId::MAX; external_keys.len()];
        // Use `before = height` so Deletes appended for this block don't shadow prior Adds.
        // External UTXOs always come from blocks with height < current; the Deletes are at `height`.
        db.query(&external_keys, &mut ids, height);

        // Step 4: fetch resolved IDs.
        // Sort by offset before fetch for sequential read locality.
        let mut fetch_order: Vec<(usize, OutputId)> = ids
            .iter()
            .enumerate()
            .filter(|(_, &id)| id != OutputId::MAX)
            .map(|(i, &id)| (i, id))
            .collect();
        fetch_order.sort_unstable_by_key(|&(_, id)| IdCodec::decode(id).0);

        let fetch_ids: Vec<OutputId> = fetch_order.iter().map(|&(_, id)| id).collect();
        let mut raw_details: Vec<OutputDetail> = Vec::with_capacity(fetch_ids.len());
        db.fetch(&fetch_ids, &mut raw_details)?;

        // Step 5: build key_to_idx and ordered details.
        let mut details: Vec<OutputDetail> = Vec::with_capacity(external_keys.len());
        let mut key_to_idx: FxHashMap<OutputKey, usize> =
            FxHashMap::with_capacity_and_hasher(external_keys.len(), Default::default());

        for (fetch_rank, (key_idx, _id)) in fetch_order.iter().enumerate() {
            let key = external_keys[*key_idx];
            let idx = details.len();
            details.push(raw_details[fetch_rank].clone());
            key_to_idx.insert(key, idx);
        }

        // Step 6: resolve intra-block keys from engine tail.
        let mut local_spends: FxHashMap<OutputKey, OutputDetail> =
            FxHashMap::with_capacity_and_hasher(intra_block_keys.len(), Default::default());

        for key in intra_block_keys {
            if local_spends.contains_key(&key) {
                continue; // already resolved
            }
            // Look up in engine index (the Add entry was recorded in step 1 at `height`).
            // Use `before = height + 1` so the Add at `height` is visible.
            let mut local_ids = [OutputId::MAX; 1];
            db.query(&[key], &mut local_ids, height + 1);
            if local_ids[0] != OutputId::MAX {
                let mut local_detail = Vec::new();
                db.fetch(&[local_ids[0]], &mut local_detail)?;
                if let Some(d) = local_detail.into_iter().next() {
                    local_spends.insert(key, d);
                }
            }
        }

        Ok(SpendSession { details, key_to_idx, local_spends, pin })
    }
}

/// Build a `UtxoSet` from a `SpendSession`. Called on the worker thread.
///
/// One `Arc<UTXO>` per entry (down from 2–3 in prefetch path).
/// `SharedByteString::from` is inline (no heap alloc) for P2PKH/P2SH/P2WPKH (≤25B scripts).
///
/// The returned `UtxoSet` is passed to `validate_block_only`. The Phase 2 note from the plan:
/// after `validate_block_only` returns the mutated utxo_set, it is immediately dropped
/// (we only use `utxo_delta`). This allocation is intentional for Phase 2 and will be
/// eliminated in Phase 2B.
#[cfg(feature = "production")]
pub fn session_to_utxo_set(session: &SpendSession) -> blvm_protocol::UtxoSet {
    let total = session.details.len() + session.local_spends.len();
    let mut map: blvm_protocol::UtxoSet = Default::default();
    map.reserve(total);

    for (key, idx) in &session.key_to_idx {
        let d = &session.details[*idx];
        let utxo = UTXO {
            value: d.header.amount,
            script_pubkey: SharedByteString::from(d.script.as_slice()),
            height: d.header.height as u64,
            is_coinbase: d.header.is_coinbase(),
        };
        let op = output_key_to_outpoint(key);
        map.insert(op, Arc::new(utxo));
    }

    for (key, d) in &session.local_spends {
        let utxo = UTXO {
            value: d.header.amount,
            script_pubkey: SharedByteString::from(d.script.as_slice()),
            height: d.header.height as u64,
            is_coinbase: d.header.is_coinbase(),
        };
        let op = output_key_to_outpoint(key);
        map.insert(op, Arc::new(utxo));
    }

    map
}

#[cfg(not(feature = "production"))]
pub fn session_to_utxo_set(session: &SpendSession) -> blvm_protocol::UtxoSet {
    let total = session.details.len() + session.local_spends.len();
    let mut map = std::collections::HashMap::with_capacity(total);

    for (key, idx) in &session.key_to_idx {
        let d = &session.details[*idx];
        let utxo = UTXO {
            value: d.header.amount,
            script_pubkey: SharedByteString::from(d.script.as_slice()),
            height: d.header.height as u64,
            is_coinbase: d.header.is_coinbase(),
        };
        let op = output_key_to_outpoint(key);
        map.insert(op, Arc::new(utxo));
    }

    for (key, d) in &session.local_spends {
        let utxo = UTXO {
            value: d.header.amount,
            script_pubkey: SharedByteString::from(d.script.as_slice()),
            height: d.header.height as u64,
            is_coinbase: d.header.is_coinbase(),
        };
        let op = output_key_to_outpoint(key);
        map.insert(op, Arc::new(utxo));
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::database::UtxoDatabase;
    use blvm_protocol::{Block, BlockHeader, Transaction, TransactionInput, TransactionOutput, OutPoint};
    use tempfile::NamedTempFile;

    fn make_txid(n: u8) -> [u8; 32] {
        let mut id = [0u8; 32];
        id[0] = n;
        id
    }

    fn coinbase_tx(value: i64) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint { hash: [0u8; 32], index: 0xFFFFFFFF },
                sequence: 0xFFFFFFFF,
                script_sig: vec![].into(),
            }].into(),
            outputs: vec![TransactionOutput {
                value,
                script_pubkey: vec![0x76, 0xa9, 0x14, 0xab].into(),
            }].into(),
            lock_time: 0,
        }
    }

    fn spend_tx(prev_hash: [u8; 32], prev_vout: u32, value: i64) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint { hash: prev_hash, index: prev_vout },
                sequence: 0xFFFFFFFF,
                script_sig: vec![].into(),
            }].into(),
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
    fn test_resolve_simple_external() {
        let tmp = NamedTempFile::new().unwrap();
        let db = UtxoDatabase::open(tmp.path()).unwrap();

        // Block 100: coinbase creates output.
        let txid100 = make_txid(100);
        let block100 = make_block(vec![coinbase_tx(5_000_000_000)]);
        let _pin = db.append(&block100, &[txid100], 100).unwrap();

        // Block 101: spend block100's coinbase output.
        let txid101_cb = make_txid(101);
        let txid101_spend = make_txid(102);
        let block101 = make_block(vec![
            coinbase_tx(5_000_000_000),
            spend_tx(txid100, 0, 4_999_000_000),
        ]);
        let tx_ids = vec![txid101_cb, txid101_spend];
        let session = SpendSession::resolve(&db, &block101, &tx_ids, 101).unwrap();

        // The session should have resolved txid100:0 externally.
        let mut key: OutputKey = [0u8; 36];
        key[..32].copy_from_slice(&txid100);
        // vout 0 in BE
        assert!(
            session.key_to_idx.contains_key(&key) || !session.local_spends.is_empty(),
            "should have resolved the external spend"
        );
    }

    #[test]
    fn test_session_to_utxo_set_has_entry() {
        let tmp = NamedTempFile::new().unwrap();
        let db = UtxoDatabase::open(tmp.path()).unwrap();

        // Block 200: creates 2 outputs.
        let txid = make_txid(200);
        let block = make_block(vec![Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint { hash: [0u8; 32], index: 0xFFFFFFFF },
                sequence: 0xFFFFFFFF,
                script_sig: vec![].into(),
            }].into(),
            outputs: vec![
                TransactionOutput { value: 1_000, script_pubkey: vec![0x51].into() },
                TransactionOutput { value: 2_000, script_pubkey: vec![0x52].into() },
            ].into(),
            lock_time: 0,
        }]);
        let _pin = db.append(&block, &[txid], 200).unwrap();

        // Block 201: spend output 0 from block 200.
        let txid201_cb = make_txid(201);
        let txid201_sp = make_txid(202);
        let block201 = make_block(vec![
            coinbase_tx(5_000_000_000),
            spend_tx(txid, 0, 900),
        ]);
        let tx_ids = vec![txid201_cb, txid201_sp];
        let session = SpendSession::resolve(&db, &block201, &tx_ids, 201).unwrap();

        let utxo_set = session_to_utxo_set(&session);
        // At least one UTXO should be in the set (txid:0 that block201 spends).
        assert!(!utxo_set.is_empty(), "utxo_set should have at least the spent output");
    }
}
