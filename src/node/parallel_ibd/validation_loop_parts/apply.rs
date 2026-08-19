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
    let mut keys_buf: Vec<OutPointKey> = Vec::new();
    let mut keys_seen = rustc_hash::FxHashSet::default();
    let mut evict_scratch: Vec<(OutPointKey, u64)> = Vec::new();
    let mut local_replay_bulk_write_done = local_replay_no_lmdb_max == 0 || engine_mode;
    if local_replay_no_lmdb_max > 0 {
        store.set_no_evict_for_local_replay();
    }
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
                if engine_mode {
                    continue;
                }
                let level = memory::ibd_memory_pressure_maintenance(
                    &mem_mtx,
                    &max_ahead_live,
                    nominal_max_ahead,
                    storage_wm.as_ref(),
                    utxo_engine.as_deref(),
                );
                let pending_len = store.pending_len();
                let cap = max_pending_ops.load(Ordering::Relaxed);
                let over_cap = pending_len > cap;
                if level >= PressureLevel::Critical || over_cap {
                    let evictable = store.len().saturating_sub(store.protected_len());
                    if evictable >= IBD_EMERGENCY_EVICT_MIN_UNPROTECTED {
                        store.evict_aggressive_for_rss();
                    }
                    if ibd_idle_flush_pending_over_cap(
                        "idle_flush_A",
                        retire_shard_index,
                        level,
                        pending_len,
                        cap,
                        over_cap,
                        &store,
                        &storage_wm,
                        &utxo_flush_handles,
                        &retire_flush_counter,
                        max_utxo_flushes_under_pressure,
                        &ibd_muhash,
                        durability_tx.as_ref(),
                        publisher.as_ref(),
                        local_replay_bulk_write_done,
                        &retire_err,
                    ) {
                        return;
                    }
                }
                continue;
            }
        };
        let h = work.height;
        // Workers have already mutated cache + pending log for this height;
        // the retire thread no longer needs to read or apply the delta. The commitment tree is
        // the only consumer that still wants the delta, so we look it up under a short lock.
        if !engine_mode {
            let delta_arc = {
                let g = staged.lock();
                g.get(&h).cloned()
            };
            if let (Some(cref), Some(_), Some(delta_arc)) = (
                commitment_tree.as_ref(),
                commitment_cstore.as_ref(),
                delta_arc.as_ref(),
            ) {
                let mut t = cref.lock();
                let store_r = store.as_ref();
                for dk in &delta_arc.deletions {
                    let op = blvm_protocol::utxo_overlay::utxo_deletion_key_to_outpoint(dk);
                    let key = outpoint_to_key(&op);
                    if let Some(utxo) = store_r.get(&key) {
                        if let Err(e) = t.remove(&op, &utxo) {
                            warn!("IBD commitment: remove failed at height {}: {}", h, e);
                        }
                    }
                }
                for (op, arc) in &delta_arc.additions {
                    if let Err(e) = t.insert(*op, arc.as_ref().clone()) {
                        warn!("IBD commitment: insert failed at height {}: {}", h, e);
                    }
                }
            }
        }

        if engine_mode {
            if h % 64 == 0 {
                memory::ibd_memory_pressure_maintenance(
                    &mem_mtx,
                    &max_ahead_live,
                    nominal_max_ahead,
                    storage_wm.as_ref(),
                    utxo_engine.as_deref(),
                );
            }
            // Skip incremental UTXO commitments during engine IBD: legacy shell store is
            // empty (removes miss), and 2× heed3 inserts/block contend with block flushes.
            publisher.publish(&local_last_retired, h);
            adapt_max_pending_ops_tick(
                &max_pending_ops,
                max_pending_ops_nominal,
                memory::ibd_pressure_level_snapshot(),
                store.pending_len(),
                &max_pending_ops_last_adapt_ms,
            );
            continue;
        }

        // Pre-lock: DashMap eviction scans (can take several ms at h=400k+). Running these
        // before acquiring mem_mtx keeps the critical section to MemoryGuard work only.
        ibd_v2_retire_pre_lock(
            h,
            store.as_ref(),
            &work.blocks_buf,
            &mut keys_buf,
            &mut keys_seen,
            &mut evict_scratch,
        );
        let (opt_pkg, is_defer_checkpoint, cap_change) = {
            let mut mem = mem_mtx.lock();
            let (_s, _e, p, r, cap) = ibd_v2_retire_apply_utxo_delta(
                h,
                store.as_ref(),
                &mut mem,
                &max_ahead_live,
                nominal_max_ahead,
                ibd_defer_flush,
                ibd_defer_checkpoint,
            );
            (p, r, cap)
        };
        // Post-lock: apply new cap and heap-trim without holding mem_mtx.
        if let Some((new_cap, pre_tune_len)) = cap_change {
            ibd_v2_retire_post_lock(store.as_ref(), new_cap, pre_tune_len, h);
        }
        if let (Some(cref), Some(cstore)) = (commitment_tree.as_ref(), commitment_cstore.as_ref()) {
            // work.block is Some only in non-engine mode (engine mode passes None to
            // avoid holding Arc<Block> in the channel during fast local replay).
            let block_hash = blockstore.get_block_hash(
                work.block
                    .as_deref()
                    .expect("commitment tree requires non-engine mode block"),
            );
            let commitment = {
                let t = cref.lock();
                t.generate_commitment(block_hash, h)
            };
            if let Err(e) = cstore.store_commitment(&block_hash, h, &commitment) {
                warn!("IBD commitment: store failed at height {}: {}", h, e);
                *retire_err.lock() = Some(e);
                return;
            }
        }
        // Update this shard's local cursor and recompute the dispatcher-wide
        // `global_last_retired = min(local across shards)`. With N=1 the publisher is a
        // no-op trivially and the orchestrator's fold check `sh <= lr_now` sees the same
        // value the original single-thread `last_retired.store(h)` would have produced.
        publisher.publish(&local_last_retired, h);
        // Adaptive cap tick: cheap (atomic loads + early-out via 500 ms throttle).
        // `ibd_pressure_level_snapshot()` reads what `ibd_v2_retire_apply_utxo_delta`
        // just published — same value the memory guard observed for this height.
        adapt_max_pending_ops_tick(
            &max_pending_ops,
            max_pending_ops_nominal,
            memory::ibd_pressure_level_snapshot(),
            store.pending_len(),
            &max_pending_ops_last_adapt_ms,
        );
        // Safe to release staged[h] now: store has the data and `local_last_retired`
        // covers it. Each shard owns disjoint heights (height % N), so no two shards
        // ever touch the same staged entry.
        staged.lock().remove(&h);
        staged_count.fetch_sub(1, Ordering::Relaxed);
        if let Some(pkg) = opt_pkg {
            if !local_replay_bulk_write_done {
                store.drain_in_flight_for_batch(&pkg.ops);
                store.release_protected_heights(&pkg.heights);
                store.note_utxo_flush_completed(pkg.max_block_height);
            } else if let Err(e) = push_utxo_flush_from_retire(
                &store,
                &storage_wm,
                &utxo_flush_handles,
                &retire_flush_counter,
                h,
                max_utxo_flushes_under_pressure,
                pkg,
                &ibd_muhash,
                is_defer_checkpoint,
                durability_tx.as_ref(),
            ) {
                *retire_err.lock() = Some(e);
                return;
            }
        }
        if !engine_mode && !local_replay_bulk_write_done && h > local_replay_no_lmdb_max {
            // CAS: only one shard across all retire threads performs the bulk write.
            let won = local_replay_transition_done
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
            local_replay_bulk_write_done = true;
            if won {
                while let Some(tail_pkg) = store.maybe_take_flush_batch_adds_only() {
                    store.drain_in_flight_for_batch(&tail_pkg.ops);
                    store.release_protected_heights(&tail_pkg.heights);
                    store.note_utxo_flush_completed(tail_pkg.max_block_height);
                }
                let n_cache = store.len();
                info!(
                    "[IBD_REPLAY_TRANSITION] h={h}: local replay ended \
                     (replay_max={local_replay_no_lmdb_max}); streaming {n_cache} live UTXOs \
                     to LMDB (one-time bulk hydration); other shards are paused until complete"
                );
                let t0 = std::time::Instant::now();
                match store.flush_full_cache_to_lmdb() {
                    Ok(written) => {
                        let elapsed_s = t0.elapsed().as_secs();
                        info!(
                            "[IBD_REPLAY_TRANSITION] h={h}: bulk LMDB hydration complete \
                             ({written} entries, {elapsed_s}s); signalling losing shards to resume"
                        );
                    }
                    Err(e) => {
                        // Signal losing shards so they don't hang forever.
                        local_replay_hydration_done.store(true, Ordering::Release);
                        *retire_err.lock() = Some(e);
                        return;
                    }
                }
                store.restore_evict_after_local_replay(8_000_000);
                if let Err(e) = storage_wm
                    .chain()
                    .force_set_ibd_utxo_watermark(local_replay_no_lmdb_max)
                {
                    warn!("[IBD_REPLAY_TRANSITION] force_set_ibd_utxo_watermark failed: {e}");
                }
                if let Err(e) = storage_wm.flush() {
                    warn!("[IBD_REPLAY_TRANSITION] storage flush after watermark failed: {e}");
                }
                // Release losing shards AFTER hydration and watermark are committed to disk.
                local_replay_hydration_done.store(true, Ordering::Release);
            } else {
                // Losing shard: WAIT for the winning shard to finish flush_full_cache_to_lmdb
                // before processing any block h > local_replay_no_lmdb_max. Without this wait,
                // concurrent cache modifications (spending UTXOs) race with the DashMap iterator
                // inside flush_full_cache_to_lmdb and cause silent UTXO misses → UTXO_TOTAL_MISS.
                info!(
                    "[IBD_REPLAY_TRANSITION] shard pausing at h={h} until bulk hydration \
                     completes (another shard won CAS; replay_max={local_replay_no_lmdb_max})"
                );
                while !local_replay_hydration_done.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                info!("[IBD_REPLAY_TRANSITION] shard resuming at h={h} after bulk hydration");
                store.restore_evict_after_local_replay(8_000_000);
            }
        }
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
    durability_tx: Option<std::sync::mpsc::SyncSender<DurabilityRequest>>,
    /// When non-zero: skip all LMDB durability writes while `h <= local_replay_no_lmdb_max`
    /// (safe after a genesis restart where the LMDB UTXO store is empty).  A one-shot bulk
    /// cache→LMDB write fires at `h = local_replay_no_lmdb_max + 1` to hydrate LMDB before
    /// normal incremental durability resumes.  Zero means normal durability from the start.
    local_replay_no_lmdb_max: u64,
    // Shared across all retire shards. CAS from false→true wins the bulk write.
    local_replay_transition_done: Arc<std::sync::atomic::AtomicBool>,
    // Set to `true` by the winning shard only AFTER flush_full_cache_to_lmdb() completes.
    // Losing shards spin on this before processing blocks h > local_replay_no_lmdb_max.
    local_replay_hydration_done: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut keys_buf: Vec<OutPointKey> = Vec::new();
    let mut keys_seen = rustc_hash::FxHashSet::default();
    let mut evict_scratch: Vec<(OutPointKey, u64)> = Vec::new();
    let mut local_replay_bulk_write_done = local_replay_no_lmdb_max == 0 || engine_mode;
    if local_replay_no_lmdb_max > 0 {
        store.set_no_evict_for_local_replay();
        info!(
            "[IBD_REPLAY_NOLMDB] Disabling UTXO cache eviction during local replay \
             (heights 1–{local_replay_no_lmdb_max}): LMDB is empty so eviction would \
             permanently lose UTXOs. Will re-enable at replay→download transition."
        );
    }
    loop {
        let work = match work_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(w) => w,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Retire went idle: flush pending when pending > cap regardless of pressure.
                // Without this, defer_flush=true creates a deadlock: workers park on the
                // cap, no staged blocks arrive, and the old Emergency-only guard never fires.
                // Also re-read /proc so the pressure atomic stays current.
                if engine_mode {
                    continue;
                }
                let level = {
                    let mut mem = mem_mtx.lock();
                    let level = mem.should_flush(Some((&max_ahead_live, nominal_max_ahead)));
                    memory::publish_ibd_pressure(level);
                    level
                };
                let pending_len = store.pending_len();
                let cap = max_pending_ops.load(Ordering::Relaxed);
                let over_cap = pending_len > cap;
                if level >= PressureLevel::Critical || over_cap {
                    let evictable = store.len().saturating_sub(store.protected_len());
                    if evictable >= IBD_EMERGENCY_EVICT_MIN_UNPROTECTED {
                        store.evict_aggressive_for_rss();
                    }
                    if ibd_idle_flush_pending_over_cap(
                        "idle_flush_B",
                        retire_shard_index,
                        level,
                        pending_len,
                        cap,
                        over_cap,
                        &store,
                        &storage_wm,
                        &utxo_flush_handles,
                        &retire_flush_counter,
                        max_utxo_flushes_under_pressure,
                        &ibd_muhash,
                        durability_tx.as_ref(),
                        publisher.as_ref(),
                        local_replay_bulk_write_done,
                        &retire_err,
                    ) {
                        return;
                    }
                }
                continue;
            }
        };
        let h = work.height;

        if engine_mode {
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
            continue;
        }

        // Workers have already mutated cache + pending log for this height;
        // retire only runs the *coordinated* per-block work (eviction + flush decisions).
        //
        // Per-block timing: log whenever any phase takes >500ms so we can attribute stalls.
        let t_retire_start = std::time::Instant::now();

        // Pre-lock: DashMap eviction scans. Running outside mem_mtx keeps the critical
        // section to only the MemoryGuard pressure evaluation and flush selection.
        ibd_v2_retire_pre_lock(
            h,
            store.as_ref(),
            &work.blocks_buf,
            &mut keys_buf,
            &mut keys_seen,
            &mut evict_scratch,
        );
        let t_after_prelock = t_retire_start.elapsed().as_millis();

        let (opt_pkg, is_defer_checkpoint, cap_change) = {
            let mut mem = mem_mtx.lock();
            let (_s, _e, p, r, cap) = ibd_v2_retire_apply_utxo_delta(
                h,
                store.as_ref(),
                &mut mem,
                &max_ahead_live,
                nominal_max_ahead,
                ibd_defer_flush,
                ibd_defer_checkpoint,
            );
            (p, r, cap)
        };
        let t_after_memlock = t_retire_start.elapsed().as_millis();

        // Post-lock: apply new UTXO cache cap and optional heap trim without holding mem_mtx.
        if let Some((new_cap, pre_tune_len)) = cap_change {
            ibd_v2_retire_post_lock(store.as_ref(), new_cap, pre_tune_len, h);
        }
        // Update this shard's local cursor and recompute the dispatcher-wide
        // `global_last_retired = min(local across shards)`. With N=1 the publisher
        // is a no-op trivially and `global == local` always.
        publisher.publish(&local_last_retired, h);
        // Adaptive cap tick: cheap (atomic loads + early-out via 500 ms throttle).
        // `ibd_pressure_level_snapshot()` reads what `ibd_v2_retire_apply_utxo_delta`
        // just published — same value the memory guard observed for this height.
        adapt_max_pending_ops_tick(
            &max_pending_ops,
            max_pending_ops_nominal,
            memory::ibd_pressure_level_snapshot(),
            store.pending_len(),
            &max_pending_ops_last_adapt_ms,
        );
        // Safe to release staged[h] now: store has the data and `local_last_retired`
        // covers it. Each shard owns disjoint heights (height % N), so no two shards
        // ever touch the same staged entry.
        staged.lock().remove(&h);
        // Decrement the lock-free mirror after removal so the orchestrator's
        // dispatch-backpressure cap sees the freed slot immediately (Relaxed is
        // sufficient — the orchestrator only reads this for a soft throttle).
        staged_count.fetch_sub(1, Ordering::Relaxed);

        // Log slow retire cycles to diagnose stalls.
        let t_total_pre_flush = t_retire_start.elapsed().as_millis();
        if t_total_pre_flush > 500 {
            warn!(
                "[IBD_RETIRE_SLOW] h={h} pre_lock={t_after_prelock}ms \
                 mem_lock={t_after_memlock}ms total_pre_flush={t_total_pre_flush}ms \
                 (has_flush={})",
                opt_pkg.is_some()
            );
        }

        if let Some(pkg) = opt_pkg {
            if !local_replay_bulk_write_done {
                // Local-replay no-LMDB mode: drain the pending log but skip all LMDB writes.
                // The LMDB UTXO store is empty (wiped before this genesis restart) so:
                //   • ADD ops: UTXO already lives in the DashMap (workers put it there).
                //   • DEL ops: UTXO already removed from DashMap at spend time.
                // Discarding the package is fully correct and keeps `pending_len` near zero,
                // preventing the worker backpressure spin that otherwise freezes validation
                // for 90 s every 90 s (channel fills in ~43 s at 3000 BPS local replay speed).
                // drain_in_flight_for_batch clears the eagerly-registered in_flight_insertions
                // entries that workers insert for every UTXO add (apply_utxo_delta hot path).
                // Without this, in_flight_insertions grows to 20 M entries / ~5 GB by h=390k.
                store.drain_in_flight_for_batch(&pkg.ops);
                store.release_protected_heights(&pkg.heights);
                store.note_utxo_flush_completed(pkg.max_block_height);
                // pkg dropped here; no LMDB write.
            } else {
                if let Err(e) = push_utxo_flush_from_retire(
                    &store,
                    &storage_wm,
                    &utxo_flush_handles,
                    &retire_flush_counter,
                    h,
                    max_utxo_flushes_under_pressure,
                    pkg,
                    &ibd_muhash,
                    is_defer_checkpoint,
                    durability_tx.as_ref(),
                ) {
                    *retire_err.lock() = Some(e);
                    return;
                }
            }
        }

        // One-time transition: first block past the local-replay window triggers the bulk
        // cache→LMDB write that hydrates the empty LMDB store with all live UTXOs.
        // CAS on the shared atomic ensures exactly ONE shard does the write — previously
        // all N shards fired independently, causing N concurrent ~6 GB Vec allocations → OOM.
        if !engine_mode && !local_replay_bulk_write_done && h > local_replay_no_lmdb_max {
            local_replay_bulk_write_done = true;
            let won = local_replay_transition_done
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
            if won {
                while let Some(tail_pkg) = store.maybe_take_flush_batch_adds_only() {
                    store.drain_in_flight_for_batch(&tail_pkg.ops);
                    store.release_protected_heights(&tail_pkg.heights);
                    store.note_utxo_flush_completed(tail_pkg.max_block_height);
                }
                let n_cache = store.len();
                info!(
                    "[IBD_REPLAY_TRANSITION] h={h}: local replay ended \
                     (replay_max={local_replay_no_lmdb_max}); streaming {n_cache} live UTXOs \
                     to LMDB (one-time bulk hydration); other shards paused until complete"
                );
                let t0 = std::time::Instant::now();
                match store.flush_full_cache_to_lmdb() {
                    Ok(written) => {
                        let elapsed_s = t0.elapsed().as_secs();
                        info!(
                            "[IBD_REPLAY_TRANSITION] h={h}: bulk LMDB hydration complete \
                             ({written} entries, {elapsed_s}s); signalling losing shards to resume"
                        );
                    }
                    Err(e) => {
                        // Signal losing shards so they don't hang forever.
                        local_replay_hydration_done.store(true, Ordering::Release);
                        *retire_err.lock() = Some(e);
                        return;
                    }
                }
                store.restore_evict_after_local_replay(utxo_nominal_max_entries);
                info!(
                    "[IBD_REPLAY_TRANSITION] h={h}: cache eviction re-enabled \
                     (nominal cap={utxo_nominal_max_entries}); MemoryGuard will tune further."
                );
                if let Err(e) = storage_wm
                    .chain()
                    .force_set_ibd_utxo_watermark(local_replay_no_lmdb_max)
                {
                    warn!("[IBD_REPLAY_TRANSITION] force_set_ibd_utxo_watermark failed: {e}");
                }
                if let Err(e) = storage_wm.flush() {
                    warn!("[IBD_REPLAY_TRANSITION] storage flush after watermark failed: {e}");
                }
                // Release losing shards AFTER hydration and watermark are committed to disk.
                local_replay_hydration_done.store(true, Ordering::Release);
            } else {
                // Losing shard: WAIT for the winning shard to finish flush_full_cache_to_lmdb
                // before processing any block h > local_replay_no_lmdb_max. Without this wait,
                // concurrent cache modifications (spending UTXOs) race with the DashMap iterator
                // inside flush_full_cache_to_lmdb, causing silent UTXO misses → UTXO_TOTAL_MISS.
                info!(
                    "[IBD_REPLAY_TRANSITION] shard pausing at h={h} until bulk hydration \
                     completes (another shard won CAS; replay_max={local_replay_no_lmdb_max})"
                );
                while !local_replay_hydration_done.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                info!("[IBD_REPLAY_TRANSITION] shard resuming at h={h} after bulk hydration");
                store.restore_evict_after_local_replay(utxo_nominal_max_entries);
            }
        }
    }
}
