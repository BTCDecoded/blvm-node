//! `SpendSession`: one-block UTXO resolution session.
//!
//! ## Two-phase design
//!
//! **Phase 1 — `SpendSession::append` (serial; dispatch or dedicated append thread)**
//! Calls `db.append()` to record this block's outputs + input delete-markers in the index.
//! Returns a `PartialSpendSession` that carries the `Pin` (prevents compacter eviction),
//! the classified input keys, and a back-reference to the engine. This phase is write-only
//! and completes in O(outputs + inputs) time with no disk reads. Height order is preserved by
//! a single consumer (orchestrator or `ibd-engine-append`).
//!
//! **Phase 2 — `SpendSession::complete` (worker thread, parallel)**
//! Consumes the `PartialSpendSession` and performs:
//! - `db.query(sorted_external_keys)` → `OutputId[]`   (binary search, read-only)
//! - `db.fetch(sorted_ids)` → `OutputDetail[]`          (tail/disk read, batched lock)
//! - Build `key_to_idx`, `local_spends`
//!
//! Moving Phase 2 to workers means all N workers can run query+fetch in parallel, while Phase 1
//! stays sequential on one thread (orchestrator or dedicated append). Height ordering is
//! preserved because that single consumer appends in height order before enqueueing workers.
//!
//! ## Intra-block filtering
//! Replicates `block_input_keys_into_filtered_with_tx_ids` logic:
//! - Skip entire coinbase transaction (`is_coinbase(tx)`)
//! - For non-coinbase txs: skip inputs whose `prevout.hash ∈ {tx_ids[1..spending_idx]}`
//!   (i.e., funded by an earlier non-coinbase tx in this block). These are resolved from
//!   the engine's in-memory tail outputs appended in `db.append`.
//!
//! ## Ordering guarantee
//! Phase 1 appends happen on a single thread in height order. By the time a worker's Phase 2
//! runs for block h, all blocks 1..=h have already been appended (append is ahead of workers).
//! Workers query with `before=h` so they never see their own deletes.

use super::database::UtxoDatabase;
use super::memory_age::Pin;
use super::types::{
    IdCodec, OUTPUT_ID_DELETED, OutputDetail, OutputId, OutputKey, outpoint_to_output_key,
    output_key_to_outpoint,
};
use blvm_consensus::utxo_overlay::UtxoLookup;
use blvm_protocol::transaction::is_coinbase;
use blvm_protocol::types::SharedByteString;
use blvm_protocol::{Block, OutPoint, UTXO};
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

thread_local! {
    static FETCH_IDS_SCRATCH: RefCell<Vec<OutputId>> = const { RefCell::new(Vec::new()) };
    /// Reuse per-block query shells (plan backlog: SpendSession::complete alloc).
    static IDS_SCRATCH: RefCell<Vec<OutputId>> = const { RefCell::new(Vec::new()) };
    static FETCH_ORDER_SCRATCH: RefCell<Vec<(usize, OutputId)>> = const { RefCell::new(Vec::new()) };
    static KEY_TO_IDX_SCRATCH: RefCell<FxHashMap<OutputKey, usize>> =
        RefCell::new(FxHashMap::with_hasher(Default::default()));
}

/// `BLVM_IBD_HOTPATH_TIMERS=N` → log `[IBD_HOTPATH]` every N heights (0 = off).
/// Works without `--features profile` so release-fast can attribute query/fetch/fill.
pub fn hotpath_timer_sample() -> u64 {
    static SAMPLE: OnceLock<u64> = OnceLock::new();
    *SAMPLE.get_or_init(|| {
        std::env::var("BLVM_IBD_HOTPATH_TIMERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    })
}

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
///
/// The age-0 pin is released inside `complete()` after query+fetch — validation no longer
/// needs the pin, and holding it through `validate_block_only` was the main PIN_BLOCKED
/// amplifier (pipeline_depth tip heights blocking age-0 merges).
pub struct SpendSession {
    /// Resolved external inputs (from prior blocks), in fetch order.
    pub details: Vec<OutputDetail>,
    /// Maps `OutputKey → index into details`. Worker uses this for UTXO lookup.
    pub key_to_idx: FxHashMap<OutputKey, usize>,
    /// Intra-block inputs (funded by earlier non-coinbase txs in the same block).
    /// Resolved from the engine's in-memory tail outputs appended in `db.append`.
    pub local_spends: FxHashMap<OutputKey, OutputDetail>,
    /// Phase-2 sub-timers (ms). Always filled; logged when `BLVM_IBD_HOTPATH_TIMERS` is set.
    pub query_ms: u64,
    /// Split of `query_ms`: in-RAM ages vs DiskIndex fallback (from `take_last_query_split_ms`).
    pub ages_ms: u64,
    pub disk_ms: u64,
    /// F5a: DiskIndex I/O from the query that filled `disk_ms`.
    pub disk_preads: u64,
    pub disk_pread_kb: u64,
    pub disk_max_pread_kb: u64,
    pub disk_cands: u64,
    pub disk_segs: u64,
    pub fetch_ms: u64,
    /// key_to_idx build + intra-block resolve.
    pub map_ms: u64,
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
        let t_query = Instant::now();
        let (
            query_ms,
            ages_ms,
            disk_ms,
            disk_preads,
            disk_pread_kb,
            disk_max_pread_kb,
            disk_cands,
            disk_segs,
            details,
            key_to_idx,
            fetch_ms,
            map_ms,
        ) = IDS_SCRATCH.with(|ids_cell| {
            FETCH_ORDER_SCRATCH.with(|order_cell| {
                KEY_TO_IDX_SCRATCH.with(|map_cell| {
                    let mut ids = ids_cell.borrow_mut();
                    ids.clear();
                    ids.resize(external_keys.len(), OutputId::MAX);
                    db.query(&external_keys, &mut ids, height);
                    let query_ms = t_query.elapsed().as_millis() as u64;
                    let (ages_ms, disk_ms) = super::index::take_last_query_split_ms();
                    let (disk_preads, disk_pread_kb, disk_max_pread_kb, disk_cands, disk_segs) =
                        super::index::take_last_disk_io_stats();

                    let t_fetch = Instant::now();
                    let mut fetch_order = order_cell.borrow_mut();
                    fetch_order.clear();
                    for (i, id) in ids.iter().enumerate() {
                        if *id != OutputId::MAX && *id != OUTPUT_ID_DELETED {
                            fetch_order.push((i, *id));
                        }
                    }
                    fetch_order.sort_unstable_by_key(|&(_, id)| IdCodec::decode(id).0);

                    let mut raw_details: Vec<OutputDetail> = Vec::new();
                    FETCH_IDS_SCRATCH.with(|cell| {
                        let mut fetch_ids = cell.borrow_mut();
                        fetch_ids.clear();
                        fetch_ids.reserve(fetch_order.len());
                        fetch_ids.extend(fetch_order.iter().map(|&(_, id)| id));
                        raw_details.reserve(fetch_ids.len());
                        db.fetch(&fetch_ids, &mut raw_details)?;
                        Ok::<(), anyhow::Error>(())
                    })?;
                    let fetch_ms = t_fetch.elapsed().as_millis() as u64;

                    let t_map = Instant::now();
                    let mut key_to_idx = map_cell.borrow_mut();
                    key_to_idx.clear();
                    key_to_idx.reserve(fetch_order.len());
                    for (fetch_rank, (key_idx, _id)) in fetch_order.iter().enumerate() {
                        let key = external_keys[*key_idx];
                        key_to_idx.insert(key, fetch_rank);
                    }
                    let key_to_idx = std::mem::take(&mut *key_to_idx);
                    let map_ms = t_map.elapsed().as_millis() as u64;

                    Ok::<_, anyhow::Error>((
                        query_ms,
                        ages_ms,
                        disk_ms,
                        disk_preads,
                        disk_pread_kb,
                        disk_max_pread_kb,
                        disk_cands,
                        disk_segs,
                        raw_details,
                        key_to_idx,
                        fetch_ms,
                        map_ms,
                    ))
                })
            })
        })?;

        // Step 4: resolve intra-block keys from engine tail — batched.
        let local_spends = resolve_intra_block(&db, intra_block_keys, height)?;

        // Pin only protects query+fetch from merging away this height's Add entries.
        // Drop before returning so validation / result queue do not stall age-0 merges.
        drop(pin);

        Ok(SpendSession {
            details,
            key_to_idx,
            local_spends,
            query_ms,
            ages_ms,
            disk_ms,
            disk_preads,
            disk_pread_kb,
            disk_max_pread_kb,
            disk_cands,
            disk_segs,
            fetch_ms,
            map_ms,
        })
    }
}

impl SpendSession {
    /// Phase 1: append this block's outputs + delete-markers to the engine index.
    ///
    /// Called sequentially on the append consumer (dispatch or `ibd-engine-append`). Returns a
    /// `PartialSpendSession` to be
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

        super::memory_run::sort_external_keys(&mut external_keys);
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

        // Same as two-phase path: pin only needed through query+fetch.
        drop(pin);

        Ok(SpendSession {
            details,
            key_to_idx,
            local_spends,
            query_ms: 0,
            ages_ms: 0,
            disk_ms: 0,
            disk_preads: 0,
            disk_pread_kb: 0,
            disk_max_pread_kb: 0,
            disk_cands: 0,
            disk_segs: 0,
            fetch_ms: 0,
            map_ms: 0,
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

    let mut raw: Vec<OutputDetail> = Vec::new();
    FETCH_IDS_SCRATCH.with(|cell| {
        let mut fetch_ids = cell.borrow_mut();
        fetch_ids.clear();
        fetch_ids.reserve(fetch_pairs.len());
        fetch_ids.extend(fetch_pairs.iter().map(|&(_, id)| id));
        raw.reserve(fetch_ids.len());
        db.fetch(&fetch_ids, &mut raw)?;
        Ok::<(), anyhow::Error>(())
    })?;

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

/// Zero-copy [`UtxoLookup`] view over a completed [`SpendSession`] (W2-1).
#[cfg(feature = "production")]
pub struct SpendSessionLookup<'a>(pub &'a SpendSession);

#[cfg(feature = "production")]
impl UtxoLookup for SpendSessionLookup<'_> {
    #[inline]
    fn get(&self, outpoint: &OutPoint) -> Option<&UTXO> {
        let key = outpoint_to_output_key(outpoint);
        if let Some(&idx) = self.0.key_to_idx.get(&key) {
            return Some(self.0.details[idx].utxo.as_ref());
        }
        self.0.local_spends.get(&key).map(|d| d.utxo.as_ref())
    }

    #[inline]
    fn len(&self) -> usize {
        self.0.key_to_idx.len() + self.0.local_spends.len()
    }
}

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
        let op = output_key_to_outpoint(key);
        out.insert(op, Arc::clone(&session.details[*idx].utxo));
    }

    for (key, d) in &session.local_spends {
        let op = output_key_to_outpoint(key);
        out.insert(op, Arc::clone(&d.utxo));
    }
}

/// N28: prove `SpendSessionLookup` ≡ `session_fill_utxo_set` for every key in the session.
/// Must pass before enabling `BLVM_IBD_SPEND_LOOKUP=1` on live IBD.
#[cfg(feature = "production")]
pub fn session_lookup_matches_fill(session: &SpendSession) -> bool {
    let mut filled = blvm_protocol::UtxoSet::default();
    session_fill_utxo_set(session, &mut filled);
    let lookup = SpendSessionLookup(session);
    if lookup.len() != filled.len() {
        return false;
    }
    for (op, arc) in filled.iter() {
        match lookup.get(op) {
            Some(u)
                if std::ptr::eq(u as *const _, arc.as_ref() as *const _)
                    || (u.value == arc.value
                        && u.height == arc.height
                        && u.is_coinbase == arc.is_coinbase
                        && u.script_pubkey.as_ref() == arc.script_pubkey.as_ref()) => {}
            _ => return false,
        }
    }
    true
}

/// Opt-in zero-copy Lookup path (default off — W2-1 resume incident).
#[cfg(feature = "production")]
pub fn spend_session_lookup_enabled() -> bool {
    matches!(
        std::env::var("BLVM_IBD_SPEND_LOOKUP").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes")
    )
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
        let op = output_key_to_outpoint(key);
        map.insert(op, Arc::clone(&session.details[*idx].utxo));
    }

    for (key, d) in &session.local_spends {
        let op = output_key_to_outpoint(key);
        map.insert(op, Arc::clone(&d.utxo));
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
        assert!(
            session_lookup_matches_fill(&session),
            "SpendSessionLookup must match fill (N28 gate)"
        );
    }
}
