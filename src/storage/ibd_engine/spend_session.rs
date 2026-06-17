//! `SpendSession`: one-block UTXO resolution session.
//!
//! ## Two-phase design
//!
//! **Phase 1 — `SpendSession::append` (dispatch thread, sequential)**
//! Calls `db.append()` to record this block's outputs + input delete-markers in the index.
//! Returns a `PartialSpendSession` that carries the `Pin` (prevents compacter eviction),
//! the classified input keys, and a back-reference to the engine. This phase is write-only
//! and completes in O(outputs + inputs) time with no disk reads.
//!
//! **Phase 2 — `SpendSession::complete` (worker thread, parallel)**
//! Consumes the `PartialSpendSession` and performs:
//! - `db.query(sorted_external_keys)` → `OutputId[]`   (binary search, read-only)
//! - `db.fetch(sorted_ids)` → `OutputDetail[]`          (tail/disk read, batched lock)
//! - Build `key_to_idx`, `local_spends`
//!
//! Moving Phase 2 to workers means all N workers can run query+fetch in parallel, while the
//! dispatch thread only does the cheap sequential append (no disk reads). Height ordering is
//! preserved because Phase 1 appends run on the single dispatch thread.
//!
//! ## Intra-block filtering
//! Replicates `block_input_keys_into_filtered_with_tx_ids` logic:
//! - Skip entire coinbase transaction (`is_coinbase(tx)`)
//! - For non-coinbase txs: skip inputs whose `prevout.hash ∈ {tx_ids[1..spending_idx]}`
//!   (i.e., funded by an earlier non-coinbase tx in this block). These are resolved from
//!   the engine's in-memory tail outputs appended in `db.append`.
//!
//! ## Ordering guarantee
//! Phase 1 appends happen on the single-threaded dispatch thread in height order. By the time
//! a worker's Phase 2 runs for block h, all blocks 1..=h have already been appended (dispatch
//! is ahead of workers). Workers query with `before=h` so they never see their own deletes.

use super::database::UtxoDatabase;
use super::memory_age::Pin;
use super::types::{
    IdCodec, OUTPUT_ID_DELETED, OutputDetail, OutputId, OutputKey, outpoint_to_output_key,
    output_key_to_outpoint,
};
use blvm_protocol::transaction::is_coinbase;
use blvm_protocol::types::SharedByteString;
use blvm_protocol::{Block, OutPoint, UTXO};
use rustc_hash::FxHashMap;
use std::sync::Arc;

// ─── Phase 1 result ─────────────────────────────────────────────────────────

/// Output of Phase 1 (`SpendSession::append`). Carries everything needed to complete Phase 2
/// on a worker thread. Cheap to send across a channel.
pub struct PartialSpendSession {
    /// RAII pin that prevents the compacter from merging height `h` away.
    pub pin: Pin,
    /// Sorted external input keys (keys from blocks prior to this one).
    pub external_keys: Vec<OutputKey>,
    /// Intra-block input keys (funded by earlier non-coinbase txs in the same block).
    pub intra_block_keys: Vec<OutputKey>,
    /// Block height (used for `before` filtering in query and for local-spend lookup).
    pub height: i32,
    /// Reference to the engine (needed for Phase 2 query+fetch).
    pub db: Arc<UtxoDatabase>,
}

// ─── Phase 2 result ─────────────────────────────────────────────────────────

/// Resolved inputs for one block. Built by `SpendSession::complete` on the worker thread;
/// consumed to build the `UtxoSet` for `validate_block_only`.
pub struct SpendSession {
    /// Resolved external inputs (from prior blocks), in fetch order.
    pub details: Vec<OutputDetail>,
    /// Maps `OutputKey → index into details`. Worker uses this for UTXO lookup.
    pub key_to_idx: FxHashMap<OutputKey, usize>,
    /// Intra-block inputs (funded by earlier non-coinbase txs in the same block).
    /// Resolved from the engine's in-memory tail outputs appended in `db.append`.
    pub local_spends: FxHashMap<OutputKey, OutputDetail>,
    /// Pin keeping `height` in the mutable window until the worker drops this session.
    pub pin: Pin,
}

impl PartialSpendSession {
    /// Phase 2: query the index and fetch UTXO details. Runs on a worker thread.
    ///
    /// Safe to call in parallel across workers because:
    /// 1. All Phase 1 appends are done before any Phase 2 starts (dispatch is ahead).
    /// 2. Workers use `before=height` which filters out concurrent same-height additions.
    /// 3. The table's fetch path is lock-safe (one mutex per fetch call; no per-record lock).
    pub fn complete(self) -> anyhow::Result<SpendSession> {
        let PartialSpendSession {
            pin,
            external_keys,
            intra_block_keys,
            height,
            db,
        } = self;

        // Step 1: query external keys (batch binary-search in age-tiered index).
        let mut ids: Vec<OutputId> = vec![OutputId::MAX; external_keys.len()];
        db.query(&external_keys, &mut ids, height);

        // Step 2: fetch resolved IDs. Sort by offset for sequential read locality.
        // Filter out OutputId::MAX (not found) and OUTPUT_ID_DELETED (spent in a prior block —
        // the sentinel must not reach UtxoTable::fetch since it decodes to a ~1TB offset).
        let mut fetch_order: Vec<(usize, OutputId)> = ids
            .iter()
            .enumerate()
            .filter(|&(_, id)| *id != OutputId::MAX && *id != OUTPUT_ID_DELETED)
            .map(|(i, id)| (i, *id))
            .collect();
        fetch_order.sort_unstable_by_key(|&(_, id)| IdCodec::decode(id).0);

        let fetch_ids: Vec<OutputId> = fetch_order.iter().map(|&(_, id)| id).collect();
        let mut raw_details: Vec<OutputDetail> = Vec::with_capacity(fetch_ids.len());
        db.fetch(&fetch_ids, &mut raw_details)?;

        // Step 3: build key_to_idx.  raw_details is already in fetch_order order, so the
        // index into raw_details IS the index into the final details vec.  Move raw_details
        // directly instead of cloning every OutputDetail (Arc bump + header copy per entry).
        let mut key_to_idx: FxHashMap<OutputKey, usize> =
            FxHashMap::with_capacity_and_hasher(fetch_order.len(), Default::default());
        for (fetch_rank, (key_idx, _id)) in fetch_order.iter().enumerate() {
            let key = external_keys[*key_idx];
            key_to_idx.insert(key, fetch_rank);
        }
        let details = raw_details; // move, no per-entry clone

        // Step 4: resolve intra-block keys from engine tail — batched.
        let local_spends = resolve_intra_block(&db, intra_block_keys, height)?;

        Ok(SpendSession {
            details,
            key_to_idx,
            local_spends,
            pin,
        })
    }
}

impl SpendSession {
    /// Phase 1: append this block's outputs + delete-markers to the engine index.
    ///
    /// Called sequentially on the dispatch thread. Returns a `PartialSpendSession` to be
    /// sent to the worker for Phase 2 completion.
    ///
    /// Delegates to `UtxoDatabase::append_and_classify` which builds the `tx_id_set` and
    /// classifies inputs in a single pass — eliminating the duplicate `HashSet` that this
    /// function previously constructed after the `db.append` call.
    pub fn append(
        db: Arc<UtxoDatabase>,
        block: &Block,
        tx_ids: &[[u8; 32]],
        height: i32,
    ) -> anyhow::Result<PartialSpendSession> {
        let (pin, external_keys, intra_block_keys) =
            db.append_and_classify(block, tx_ids, height)?;
        Ok(PartialSpendSession {
            pin,
            external_keys,
            intra_block_keys,
            height,
            db,
        })
    }

    /// Legacy combined path (dispatch thread does everything). Kept for compatibility
    /// and used in tests. In production IBD the two-phase path is used.
    pub fn resolve(
        db: &UtxoDatabase,
        block: &Block,
        tx_ids: &[[u8; 32]],
        height: i32,
    ) -> anyhow::Result<Self> {
        // Wrap db in a dummy Arc for the append call (tests only).
        // We use a workaround: call db.append directly, then do Phase 2 inline.
        let pin = db.append(block, tx_ids, height)?;

        let tx_id_set: std::collections::HashSet<[u8; 32]> = tx_ids[1..].iter().copied().collect();

        let mut external_keys: Vec<OutputKey> = Vec::new();
        let mut intra_block_keys: Vec<OutputKey> = Vec::new();

        for tx in block.transactions.iter() {
            if is_coinbase(tx) {
                continue;
            }
            for input in tx.inputs.iter() {
                let key = outpoint_to_output_key(&input.prevout);
                if tx_id_set.contains(&input.prevout.hash) {
                    intra_block_keys.push(key);
                } else {
                    external_keys.push(key);
                }
            }
        }

        #[cfg(feature = "rayon")]
        {
            use rayon::prelude::*;
            external_keys.par_sort_unstable();
        }
        #[cfg(not(feature = "rayon"))]
        external_keys.sort_unstable();
        external_keys.dedup();

        let mut ids: Vec<OutputId> = vec![OutputId::MAX; external_keys.len()];
        db.query(&external_keys, &mut ids, height);

        let mut fetch_order: Vec<(usize, OutputId)> = ids
            .iter()
            .enumerate()
            .filter(|&(_, id)| *id != OutputId::MAX && *id != OUTPUT_ID_DELETED)
            .map(|(i, id)| (i, *id))
            .collect();
        fetch_order.sort_unstable_by_key(|&(_, id)| IdCodec::decode(id).0);

        let fetch_ids: Vec<OutputId> = fetch_order.iter().map(|&(_, id)| id).collect();
        let mut raw_details: Vec<OutputDetail> = Vec::with_capacity(fetch_ids.len());
        db.fetch(&fetch_ids, &mut raw_details)?;

        let mut key_to_idx: FxHashMap<OutputKey, usize> =
            FxHashMap::with_capacity_and_hasher(fetch_order.len(), Default::default());
        for (fetch_rank, (key_idx, _id)) in fetch_order.iter().enumerate() {
            let key = external_keys[*key_idx];
            key_to_idx.insert(key, fetch_rank);
        }
        let details = raw_details; // move, no per-entry clone

        let local_spends = resolve_intra_block(db, intra_block_keys, height)?;

        Ok(SpendSession {
            details,
            key_to_idx,
            local_spends,
            pin,
        })
    }
}

// ─── Intra-block batch resolver ──────────────────────────────────────────────

/// Resolve `intra_block_keys` in one batch query + one batch fetch.
///
/// Previously this did N × (single-key query + single-entry fetch) calls sequentially —
/// O(N) round-trips through the engine. This version:
///   1. Deduplicates and sorts the keys.
///   2. Calls `db.query` once for all keys with `before = height + 1` (so the Add at
///      `height` recorded in Phase 1 is visible).
///   3. Sorts resolved IDs by file offset for sequential / io_uring-friendly access.
///   4. Calls `db.fetch` once for all resolved IDs.
///
/// Total: 1 query + 1 fetch regardless of how many intra-block spends there are.
fn resolve_intra_block(
    db: &UtxoDatabase,
    mut keys: Vec<OutputKey>,
    height: i32,
) -> anyhow::Result<FxHashMap<OutputKey, OutputDetail>> {
    if keys.is_empty() {
        return Ok(FxHashMap::default());
    }

    // Deduplicate (sort required by db.query; dedup removes repeated spends of the same output).
    keys.sort_unstable();
    keys.dedup();

    // Batch query.
    let mut ids = vec![OutputId::MAX; keys.len()];
    db.query(&keys, &mut ids, height + 1);

    // Collect resolved (key_idx, id) pairs, sort by file offset for read locality.
    let mut fetch_pairs: Vec<(usize, OutputId)> = ids
        .iter()
        .enumerate()
        .filter(|&(_, id)| *id != OutputId::MAX && *id != OUTPUT_ID_DELETED)
        .map(|(i, id)| (i, *id))
        .collect();
    fetch_pairs.sort_unstable_by_key(|&(_, id)| IdCodec::decode(id).0);

    let fetch_ids: Vec<OutputId> = fetch_pairs.iter().map(|&(_, id)| id).collect();
    let mut raw: Vec<OutputDetail> = Vec::with_capacity(fetch_ids.len());
    db.fetch(&fetch_ids, &mut raw)?;

    // raw is in fetch_pairs order; drain it to avoid per-entry clone.
    let mut map = FxHashMap::with_capacity_and_hasher(fetch_pairs.len(), Default::default());
    let mut raw_iter = raw.into_iter();
    for &(key_idx, _) in fetch_pairs.iter() {
        if let Some(d) = raw_iter.next() {
            map.insert(keys[key_idx], d); // move, no clone
        }
    }
    Ok(map)
}

// ─── session_to_utxo_set ────────────────────────────────────────────────────

/// Fill `out` with the UTXOs from a completed `SpendSession`. Called on the worker thread.
/// Clears `out` first, then fills in-place to reuse the existing HashMap allocation across
/// blocks (avoids one heap alloc + dealloc per block in the engine validation hot path).
///
/// One `Arc<UTXO>` per entry.
/// `SharedByteString::from` is inline (no heap alloc) for P2PKH/P2SH/P2WPKH (≤25B scripts).
#[cfg(feature = "production")]
pub fn session_to_utxo_set(session: &SpendSession) -> blvm_protocol::UtxoSet {
    let total = session.details.len() + session.local_spends.len();
    let mut map: blvm_protocol::UtxoSet = Default::default();
    map.reserve(total);
    session_fill_utxo_set(session, &mut map);
    map
}

/// In-place variant: clears `out` and fills it from the session. Reuses the HashMap
/// allocation across blocks when the caller keeps `out` alive between calls.
#[cfg(feature = "production")]
pub fn session_fill_utxo_set(session: &SpendSession, out: &mut blvm_protocol::UtxoSet) {
    let total = session.details.len() + session.local_spends.len();
    out.clear();
    out.reserve(total);

    for (key, idx) in &session.key_to_idx {
        let d = &session.details[*idx];
        let utxo = UTXO {
            value: d.header.amount,
            script_pubkey: SharedByteString::from(d.script.as_slice()),
            height: d.header.height as u64,
            is_coinbase: d.header.is_coinbase(),
        };
        let op = output_key_to_outpoint(key);
        out.insert(op, Arc::new(utxo));
    }

    for (key, d) in &session.local_spends {
        let utxo = UTXO {
            value: d.header.amount,
            script_pubkey: SharedByteString::from(d.script.as_slice()),
            height: d.header.height as u64,
            is_coinbase: d.header.is_coinbase(),
        };
        let op = output_key_to_outpoint(key);
        out.insert(op, Arc::new(utxo));
    }
}

#[cfg(not(feature = "production"))]
pub fn session_to_utxo_set(session: &SpendSession) -> blvm_protocol::UtxoSet {
    let total = session.details.len() + session.local_spends.len();
    let mut map = std::collections::HashMap::with_capacity(total);
    session_fill_utxo_set_non_prod(session, &mut map);
    map
}

#[cfg(not(feature = "production"))]
pub fn session_fill_utxo_set(session: &SpendSession, out: &mut blvm_protocol::UtxoSet) {
    session_fill_utxo_set_non_prod(session, out);
}

#[cfg(not(feature = "production"))]
fn session_fill_utxo_set_non_prod(session: &SpendSession, map: &mut blvm_protocol::UtxoSet) {
    let total = session.details.len() + session.local_spends.len();
    map.clear();
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
}

#[cfg(test)]
mod tests {
    use super::super::database::UtxoDatabase;
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

    fn coinbase_tx(value: i64) -> Transaction {
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
                script_pubkey: vec![0x76, 0xa9, 0x14, 0xab],
            }]
            .into(),
            lock_time: 0,
        }
    }

    fn spend_tx(prev_hash: [u8; 32], prev_vout: u32, value: i64) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: prev_hash,
                    index: prev_vout,
                },
                sequence: 0xFFFFFFFF,
                script_sig: vec![],
            }]
            .into(),
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
    fn test_resolve_simple_external() {
        let tmp = NamedTempFile::new().unwrap();
        let db = UtxoDatabase::open(tmp.path(), 0).unwrap();

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
        let db = UtxoDatabase::open(tmp.path(), 0).unwrap();

        // Block 200: creates 2 outputs.
        let txid = make_txid(200);
        let block = make_block(vec![Transaction {
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
            outputs: vec![
                TransactionOutput {
                    value: 1_000,
                    script_pubkey: vec![0x51],
                },
                TransactionOutput {
                    value: 2_000,
                    script_pubkey: vec![0x52],
                },
            ]
            .into(),
            lock_time: 0,
        }]);
        let _pin = db.append(&block, &[txid], 200).unwrap();

        // Block 201: spend output 0 from block 200.
        let txid201_cb = make_txid(201);
        let txid201_sp = make_txid(202);
        let block201 = make_block(vec![coinbase_tx(5_000_000_000), spend_tx(txid, 0, 900)]);
        let tx_ids = vec![txid201_cb, txid201_sp];
        let session = SpendSession::resolve(&db, &block201, &tx_ids, 201).unwrap();

        let utxo_set = session_to_utxo_set(&session);
        // At least one UTXO should be in the set (txid:0 that block201 spends).
        assert!(
            !utxo_set.is_empty(),
            "utxo_set should have at least the spent output"
        );
    }
}
