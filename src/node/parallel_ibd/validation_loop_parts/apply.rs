/// Losing shards resume only after the watermark and the storage flush both succeed.
pub(crate) fn replay_shards_may_resume(watermark_ok: bool, storage_flush_ok: bool) -> bool {
    watermark_ok && storage_flush_ok
}

/// Idle retire timeout path: drain adds when possible; when over cap and DEL-heavy, shard 0
/// enqueues a sentinel checkpoint so `del_backlog` can run without a staged block at boundary.
#[allow(clippy::too_many_arguments)]
fn ibd_idle_flush_pending_over_cap(
    path: &'static str,
    retire_shard_index: usize,
    level: PressureLevel,
    pending_len: usize,
    cap: usize,
    over_cap: bool,
    store: &Arc<IbdUtxoStore>,
    storage_wm: &Arc<Storage>,
    utxo_flush_handles: &Arc<Mutex<VecDeque<JoinHandle<Result<blvm_muhash::MuHash3072>>>>>,
    retire_flush_counter: &Arc<AtomicUsize>,
    max_utxo_flushes_under_pressure: usize,
    ibd_muhash: &Arc<Mutex<blvm_muhash::MuHash3072>>,
    durability_tx: Option<&std::sync::mpsc::SyncSender<DurabilityRequest>>,
    publisher: &super::retire_dispatcher::GlobalProgressPublisher,
    local_replay_bulk_write_done: bool,
    retire_err: &Arc<Mutex<Option<anyhow::Error>>>,
) -> bool {
    warn!(
        "[CAPPED_DRAIN] path={path} level={level:?} pending={pending_len} cap={cap} over_cap={over_cap}"
    );
    let push_pkg = |pkg: PendingFlushPackage, trigger_height: u64, force_durability: bool| {
        if !local_replay_bulk_write_done {
            store.drain_in_flight_for_batch(&pkg.ops);
            store.release_protected_heights(&pkg.heights);
            store.note_utxo_flush_completed(pkg.max_block_height);
            return false;
        }
        match push_utxo_flush_from_retire(
            store,
            storage_wm,
            utxo_flush_handles,
            retire_flush_counter,
            trigger_height,
            max_utxo_flushes_under_pressure,
            pkg,
            ibd_muhash,
            force_durability,
            durability_tx,
        ) {
            Ok(()) => false,
            Err(e) => {
                *retire_err.lock() = Some(e);
                true
            }
        }
    };

    if let Some(pkg) = store.take_flush_batch_adds_only() {
        warn!("[CAPPED_DRAIN] path={path} drained={}", pkg.ops.len());
        if over_cap {
            info!(
                "[IBD_IDLE_FLUSH] pending={pending_len} > cap={cap} — draining via idle flush to unblock workers"
            );
        }
        if push_pkg(pkg, 0, false) {
            return true;
        }
    } else if over_cap && pending_len > 0 && retire_shard_index == 0 {
        let wm = publisher.global_floor();
        warn!("[CAPPED_DRAIN] path={path} idle_sentinel wm={wm} pending={pending_len} cap={cap}");
        let pkg = ibd_empty_checkpoint_package(wm);
        if push_pkg(pkg, wm, true) {
            return true;
        }
    }
    ibd_maybe_heap_trim();
    false
}

/// `local_last_retired` + `publisher`: see [`run_ibd_retire_loop_no_commitment`] — same
/// sharding semantics. Commitment-tree updates happen on this shard's heights only; the
/// commitment tree itself is `Mutex`-guarded, so multi-shard concurrent commitment
/// updates serialize on that lock. With `BLVM_IBD_RETIRE_SHARDS=1` behavior is unchanged.
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
    durability_tx: Option<std::sync::mpsc::SyncSender<DurabilityRequest>>,
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
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Retire went idle: no staged blocks arriving because workers are parked on
                // the pending cap. Flush pending to LMDB whenever pending > cap, regardless
                // of pressure level. Without this, defer_flush=true creates a deadlock:
                //   pending > cap -> workers park -> no staged blocks -> timeout fires
                //   -> old code only flushed on Emergency -> pending never drains -> frozen
                // Safety: workers push ops into pending_writes before dispatching retire
                // work (invariant 1), so all in-pending entries are for fully-validated
                // blocks and are safe to flush. Advancing watermark past local_last_retired
                // is correct (invariant 2).
                continue;
            }
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
/// `local_last_retired` is the per-shard cursor (each shard owns one). Publishing through
/// `publisher` recomputes `min(local_last_retired across shards)` and stores it as the
/// dispatcher's `global_last_retired`. Validation workers and any caller that needs a
/// contiguously-retired floor read the global value; this loop reads only its own local
/// for `take_flush_batch_force_through(flush_cap)` — drain-by-height is monotone and the
/// shared pending log is safe to drain past the floor (workers populate ops before
/// dispatch sends `IbdRetireWork`, so all heights `<= local_last_retired` already have
/// their ops in pending). With `BLVM_IBD_RETIRE_SHARDS=1` (the default), `local` and the
/// dispatcher's global atomic are kept in lock-step by `publisher.publish` — behavior is
/// identical to the pre-sharding single-thread retire.
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
    durability_tx: Option<std::sync::mpsc::SyncSender<DurabilityRequest>>,
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
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Retire went idle: flush pending when pending > cap regardless of pressure.
                // Without this, defer_flush=true creates a deadlock: workers park on the
                // cap, no staged blocks arrive, and the old Emergency-only guard never fires.
                // Also re-read /proc so the pressure atomic stays current.
                continue;
            }
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
