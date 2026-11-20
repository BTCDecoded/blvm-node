/// Losing shards resume only after the watermark and the storage flush both succeed.
pub(crate) fn replay_shards_may_resume(watermark_ok: bool, storage_flush_ok: bool) -> bool {
    watermark_ok && storage_flush_ok
}

/// `local_last_retired` + `publisher`: see [`run_ibd_retire_loop_no_commitment`] — same
/// sharding semantics. This loop publishes the retired height. The engine checkpoint
/// export is the durable UTXO write.
///
/// `max_pending_ops` + `max_pending_ops_nominal` + `max_pending_ops_last_adapt_ms`: the
/// adaptive backpressure cap (see [`adapt_max_pending_ops_tick`]). Updated at most once
/// per 500 ms from this loop, read by every validation worker.
#[cfg(all(feature = "utxo-commitments", feature = "production"))]
#[allow(clippy::too_many_arguments)]
fn run_ibd_retire_loop_with_commitment(
    work_rx: mpsc::Receiver<IbdRetireWork>,
    staged: Arc<Mutex<BTreeMap<u64, Arc<UtxoDelta>>>>,
    staged_count: Arc<AtomicUsize>,
    local_last_retired: Arc<AtomicU64>,
    publisher: Arc<super::retire_dispatcher::GlobalProgressPublisher>,
    retire_shard_index: usize,
    store: Arc<IbdUtxoStore>,
    storage_wm: Arc<Storage>,
    mem_mtx: Arc<Mutex<MemoryGuard>>,
    max_ahead_live: Arc<AtomicU64>,
    nominal_max_ahead: u64,
    ibd_defer_flush: bool,
    ibd_defer_checkpoint: u64,
    max_utxo_flushes_under_pressure: usize,
    utxo_flush_handles: Arc<Mutex<VecDeque<JoinHandle<Result<blvm_muhash::MuHash3072>>>>>,
    retire_flush_counter: Arc<AtomicUsize>,
    retire_err: Arc<Mutex<Option<anyhow::Error>>>,
    blockstore: Arc<BlockStore>,
    commitment_tree: Option<
        Arc<Mutex<blvm_protocol::utxo_commitments::merkle_tree::UtxoMerkleTree>>,
    >,
    commitment_cstore: Option<Arc<crate::storage::commitment_store::CommitmentStore>>,
    ibd_muhash: Arc<Mutex<blvm_muhash::MuHash3072>>,
    max_pending_ops: Arc<AtomicUsize>,
    max_pending_ops_nominal: usize,
    max_pending_ops_last_adapt_ms: Arc<AtomicU64>,
    engine_mode: bool,
    utxo_engine: Option<Arc<UtxoDatabase>>,
    local_replay_no_lmdb_max: u64,
    // Shared across all retire shards. CAS from false→true wins the bulk write.
    local_replay_transition_done: Arc<std::sync::atomic::AtomicBool>,
    // Set to `true` by the winning shard only AFTER flush_full_cache_to_lmdb() completes.
    // Losing shards spin on this before processing blocks h > local_replay_no_lmdb_max.
    local_replay_hydration_done: Arc<std::sync::atomic::AtomicBool>,
) {
    let _ = (
        engine_mode,
        local_replay_no_lmdb_max,
        local_replay_transition_done,
        local_replay_hydration_done,
    );
    loop {
        let work = match work_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(w) => w,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        };
        let h = work.height;
        if h % 64 == 0 {
            memory::ibd_memory_pressure_maintenance(
                &mem_mtx,
                &max_ahead_live,
                nominal_max_ahead,
                storage_wm.as_ref(),
                utxo_engine.as_deref(),
            );
        }
        publisher.publish(&local_last_retired, h);
        adapt_max_pending_ops_tick(
            &max_pending_ops,
            max_pending_ops_nominal,
            memory::ibd_pressure_level_snapshot(),
            store.pending_len(),
            &max_pending_ops_last_adapt_ms,
        );
    }
}

#[cfg(not(all(feature = "utxo-commitments", feature = "production")))]
/// `local_last_retired` is the per-shard cursor. Publishing through `publisher` stores
/// `min` across shards as `global_last_retired`. With `BLVM_IBD_RETIRE_SHARDS=1` that
/// min is this shard's cursor. The loop does not flush the legacy UTXO store.
///
/// `max_pending_ops` + `max_pending_ops_nominal` + `max_pending_ops_last_adapt_ms`: the
/// adaptive backpressure cap (see [`adapt_max_pending_ops_tick`]). Updated at most once
/// per 500 ms from this loop, read by every validation worker.
#[allow(clippy::too_many_arguments)]
fn run_ibd_retire_loop_no_commitment(
    work_rx: mpsc::Receiver<IbdRetireWork>,
    staged: Arc<Mutex<BTreeMap<u64, Arc<UtxoDelta>>>>,
    staged_count: Arc<AtomicUsize>,
    local_last_retired: Arc<AtomicU64>,
    publisher: Arc<super::retire_dispatcher::GlobalProgressPublisher>,
    retire_shard_index: usize,
    store: Arc<IbdUtxoStore>,
    storage_wm: Arc<Storage>,
    mem_mtx: Arc<Mutex<MemoryGuard>>,
    max_ahead_live: Arc<AtomicU64>,
    nominal_max_ahead: u64,
    ibd_defer_flush: bool,
    ibd_defer_checkpoint: u64,
    max_utxo_flushes_under_pressure: usize,
    utxo_flush_handles: Arc<Mutex<VecDeque<JoinHandle<Result<blvm_muhash::MuHash3072>>>>>,
    retire_flush_counter: Arc<AtomicUsize>,
    retire_err: Arc<Mutex<Option<anyhow::Error>>>,
    ibd_muhash: Arc<Mutex<blvm_muhash::MuHash3072>>,
    max_pending_ops: Arc<AtomicUsize>,
    max_pending_ops_nominal: usize,
    max_pending_ops_last_adapt_ms: Arc<AtomicU64>,
    engine_mode: bool,
    utxo_engine: Option<Arc<UtxoDatabase>>,
    // When non-zero: skip all LMDB durability writes while `h <= local_replay_no_lmdb_max`
    // (safe after a genesis restart where the LMDB UTXO store is empty).  A one-shot bulk
    // cache→LMDB write fires at `h = local_replay_no_lmdb_max + 1` to hydrate LMDB before
    // normal incremental durability resumes.  Zero means normal durability from the start.
    local_replay_no_lmdb_max: u64,
    // Shared across all retire shards. CAS from false→true wins the bulk write.
    local_replay_transition_done: Arc<std::sync::atomic::AtomicBool>,
    // Set to `true` by the winning shard only AFTER flush_full_cache_to_lmdb() completes.
    // Losing shards spin on this before processing blocks h > local_replay_no_lmdb_max.
    local_replay_hydration_done: Arc<std::sync::atomic::AtomicBool>,
    utxo_nominal_max_entries: usize,
) {
    let _ = (
        engine_mode,
        local_replay_no_lmdb_max,
        local_replay_transition_done,
        local_replay_hydration_done,
        utxo_nominal_max_entries,
    );
    loop {
        let work = match work_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(w) => w,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        };
        let h = work.height;
        if h % 64 == 0 {
            memory::ibd_memory_pressure_maintenance(
                &mem_mtx,
                &max_ahead_live,
                nominal_max_ahead,
                storage_wm.as_ref(),
                utxo_engine.as_deref(),
            );
        }
        publisher.publish(&local_last_retired, h);
        adapt_max_pending_ops_tick(
            &max_pending_ops,
            max_pending_ops_nominal,
            memory::ibd_pressure_level_snapshot(),
            store.pending_len(),
            &max_pending_ops_last_adapt_ms,
        );
    }
}

#[cfg(test)]
mod replay_release_tests {
    use super::replay_shards_may_resume;

    #[test]
    fn failed_watermark_or_flush_does_not_release_shards() {
        assert!(!replay_shards_may_resume(false, true));
        assert!(!replay_shards_may_resume(true, false));
        assert!(!replay_shards_may_resume(false, false));
        assert!(replay_shards_may_resume(true, true));
    }
}
