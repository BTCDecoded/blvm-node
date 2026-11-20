/// Parent headers for the median-time check. Height 0 has none. A shorter
/// window than the chain height, up to 11, means a parent header is missing.
fn mtp_parent_headers<H>(height: u64, headers: &[H]) -> Result<&[H]> {
    let expected = if height == 0 {
        0
    } else {
        (height as usize).min(blvm_protocol::bip113::MEDIAN_TIME_BLOCKS)
    };
    if headers.len() < expected {
        anyhow::bail!("missing parent headers for median time at height {height}");
    }
    Ok(headers)
}

fn shared_empty_witness_stacks(n_tx: usize) -> Arc<Vec<Vec<Witness>>> {
    EMPTY_WITNESS_STACKS.with(|cell| {
        let mut g = cell.borrow_mut();
        if let Some(a) = g.get(&n_tx) {
            return Arc::clone(a);
        }
        let arc = Arc::new(vec![Vec::new(); n_tx]);
        if g.len() > 512 {
            g.clear();
        }
        g.insert(n_tx, Arc::clone(&arc));
        arc
    })
}


/// Opt out: `BLVM_IBD_ASYNC_ENGINE_APPEND=0`. Default **on** — Phase 1 `SpendSession::append`
/// runs on a dedicated serial thread so the orchestrator can keep taking from the feeder /
/// waiting on validate results while append of prior heights proceeds. Height order is preserved
/// (single consumer); workers still see appends completed for all heights ≤ their job.
fn async_engine_append_enabled() -> bool {
    match std::env::var("BLVM_IBD_ASYNC_ENGINE_APPEND") {
        Ok(v) => {
            let t = v.trim();
            !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
        }
        Err(_) => true,
    }
}

/// R-360: number of `ibd-engine-prep` threads between the orchestrator and the serial append
/// thread (txid SHA256d + output cache, formerly on the orchestrator's dispatch). `0` keeps
/// the R-359 behaviour (prep on the orchestrator). Default **2**, clamp 0–8.
/// `BLVM_IBD_PREP_THREADS`.
fn engine_prep_threads() -> usize {
    match std::env::var("BLVM_IBD_PREP_THREADS") {
        Ok(v) => v.trim().parse::<usize>().unwrap_or(2).min(8),
        Err(_) => 2,
    }
}

/// Opt in: `BLVM_IBD_BINDER_LOG=1` (or `true`/`on`). Default **off**.
/// Soak harness (`wan-bench-common.sh`) forces `=1` — emit `[IBD_BINDER]` / `[IBD_SLOW_STRETCH]`.
fn binder_log_enabled() -> bool {
    latch_env!(bool, {
        matches!(
            std::env::var("BLVM_IBD_BINDER_LOG")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("1") | Some("true") | Some("on") | Some("yes")
        )
    })
}

/// Classify what is binding the validation tip right now (supply vs engine).
/// Used to attribute wall-BPS tax when recent ≪ median/peak instant.
fn classify_ibd_binder(
    feeder: usize,
    holes: u64,
    contig: u64,
    await_ms: u64,
    gd_ewma_ms: Option<u64>,
    pressure: PressureLevel,
    tip_failover: bool,
) -> &'static str {
    if matches!(pressure, PressureLevel::Critical | PressureLevel::Emergency) {
        return "ENGINE_PRESSURE";
    }
    // Tip missing on the wire / awaiting getdata→body.
    if await_ms >= 200 {
        return "SUPPLY_TIP_HOLE_ABSENT";
    }
    // Body may already be in reorder/bridge, but the feeder has a hole below it.
    if feeder == 0 && holes > 0 && contig == 0 {
        return "SUPPLY_TIP_HOLE_STAGED";
    }
    if tip_failover {
        return "SUPPLY_FAILOVER";
    }
    if feeder == 0 && contig == 0 {
        return "SUPPLY_EMPTY_TIP";
    }
    if feeder == 0 {
        // H3 C3: feeder drained with healthy tip_gd + contig runway is pipe/engine HOL,
        // not a cold-supply starve (mid-window false FEEDER_STARVE on KEEP green).
        let gd_ok = gd_ewma_ms.map(|ms| ms < 200).unwrap_or(false);
        if holes == 0 && contig > 0 && gd_ok {
            return "PIPE_DRAINED";
        }
        // Tip may be in reorder while feeder drained — classic FEEDER_STARVE shape.
        return "SUPPLY_FEEDER_STARVE";
    }
    if let Some(ms) = gd_ewma_ms
        && ms >= 200 && feeder < 16 {
            return "SUPPLY_GD_SLOW";
        }
    if contig == 0 && feeder < 8 {
        return "SUPPLY_THIN_RUNWAY";
    }
    // Feeder stocked, pressure calm — scripts/engine/retire path.
    "ENGINE_OR_SCRIPTS"
}

/// R-302: one line per >2s tip_hole wait (R-301: 114 events owned 54% of hole seconds).
pub(crate) fn maybe_log_tip_hole_anatomy(
    h: u64,
    wait_ms: u64,
    binder: &str,
    feeder: usize,
    holes: u64,
    contig: u64,
) {
    if wait_ms < 2000 || !binder.starts_with("SUPPLY_TIP_HOLE") {
        return;
    }
    let gd_peer = super::tip_stage::getdata_peer_for(h);
    let gd_outstanding_ms = super::ms_breakdown::assigned_ms_ago(h);
    let in_reorder = super::IBD_TIP_IN_REORDER.load(Ordering::Relaxed);
    let bridge_pending = memory::BRIDGE_PENDING_COUNT.load(Ordering::Relaxed);
    let reorder_ahead = super::IBD_REORDER_AHEAD.load(Ordering::Relaxed);
    let body_await_ms = super::tip_stage::tip_awaiting_ms_for_cap();
    let soft_retry = super::tip_stage::tip_ahead_frozen_for_soft_retry();
    let late_body = super::tip_stage::tip_ahead_frozen_for_late_body();
    let soft_retries = super::tip_stage::tip_soft_retries();
    let peer_rx = 0usize; // R-303: dropped the underflowed depth counter.
    let prior_discard = crate::node::parallel_ibd::body_dup::height_was_discarded(h);
    tracing::warn!(
        "[IBD_TIP_HOLE_ANATOMY] h={} wait_ms={} binder={} gd_peer={} gd_outstanding_ms={} in_reorder={} bridge_pending={} reorder_ahead={} feeder={} holes={} contig={} body_await_ms={} soft_retry={} late_body_freeze={} soft_retries={} peer_rx_depth={} prior_discard={}",
        h,
        wait_ms,
        binder,
        gd_peer.as_deref().unwrap_or("-"),
        gd_outstanding_ms,
        in_reorder,
        bridge_pending,
        reorder_ahead,
        feeder,
        holes,
        contig,
        body_await_ms,
        soft_retry,
        late_body,
        soft_retries,
        peer_rx,
        prior_discard
    );
}


use super::IbdBlockFlushOpts;
use super::ParallelIBD;

use blvm_protocol::block::UtxoDelta;


/// True when this height should emit a profile sample line for interval `sample`.
/// `sample == 0` means interval sampling is off (e.g. only `disk` / `blocked` in BLVM_IBD_DEBUG)
/// — never use `% sample` in that case.
#[cfg(feature = "profile")]
#[inline]
fn ibd_profile_height_matches_sample(sample: u64, height: u64) -> bool {
    sample == 1 || (sample > 0 && height % sample == 0)
}

#[inline]
fn dynamic_utxo_cap(level: PressureLevel, nominal: usize) -> usize {
    if nominal == usize::MAX {
        return usize::MAX;
    }
    match level {
        PressureLevel::Emergency => (nominal / 4).max(8_192),
        PressureLevel::Critical => (nominal * 2 / 3).max(nominal / 2),
        PressureLevel::Elevated => (nominal * 9 / 10).max(nominal * 4 / 5),
        PressureLevel::None => nominal,
    }
}

#[inline]
fn dynamic_prefetch_lookahead(level: PressureLevel, nominal: usize) -> usize {
    let n = nominal.clamp(1, 128);
    match level {
        PressureLevel::Emergency => 8,
        PressureLevel::Critical => (n / 2).clamp(12, 48),
        PressureLevel::Elevated => ((n * 2 / 3).max(24)).min(n),
        PressureLevel::None => n,
    }
}

/// Shrink validation pipeline depth when engine append is stuck on Arc-contention clones.
///
/// Throttle uses **contention_pct** only (shared outer/tip Arc). Structural freeze-and-new
/// is ~30% of appends at mid-IBD with 25k run caps and must not shrink depth — that was the
/// BPS regression when total `slow_pct≥15%` halved the pipeline.
#[inline]
fn pipeline_depth_for_engine_append(nominal: usize) -> usize {
    let slow_pct = crate::storage::ibd_engine::memory_age::memory_age_throttle_slow_pct();
    crate::node::parallel_ibd::ms_breakdown::note_slow_pct(slow_pct);
    if slow_pct == 0 {
        return nominal;
    }
    if slow_pct >= 95 {
        1
    } else if slow_pct >= 50 {
        (nominal / 4).max(4)
    } else if slow_pct >= 25 {
        (nominal / 2).max(8)
    } else {
        nominal
    }
}

/// P1: when tip DiskIndex is hot (`segs≥2` or recent `disk_ms` high), shrink pipeline
/// so fewer workers thrash the page cache (opposite of F5c parallel-pread).
/// Opt-in only (`=1`). C2 default-on under tip-crawl hurt tip30 (`tc220a48h3c21`).
fn tip_concurrency_adapt_from_env() -> bool {
    #[cfg(not(test))]
    {
        false
    }
    #[cfg(test)]
    {
        latch_env!(bool, {
            matches!(
                std::env::var("BLVM_IBD_TIP_CONCURRENCY_ADAPT")
                    .ok()
                    .as_deref()
                    .map(str::trim),
                Some("1") | Some("true") | Some("yes") | Some("on")
            )
        })
    }
}

/// Land E: under tip-crawl with healthy supply, treat Critical like Elevated for
/// pipeline/pending/poll policy (Emergency unchanged). Avoids depth/2 + spill thrash
/// when STREAM/gd already dens-class (A5 Chunk B).
#[inline]
fn effective_pressure_for_tip_crawl(level: PressureLevel) -> PressureLevel {
    if matches!(level, PressureLevel::Critical) && super::tip_stage::tip_crawl_supply_healthy_now()
    {
        PressureLevel::Elevated
    } else {
        level
    }
}

/// Shrink validation pipeline depth under RSS pressure. Engine-mode workers hold
/// `MemoryRun` query snapshots while `MemoryAge::append` falls back to a full run clone
/// when `Arc::strong_count(tip) != 1` — depth 32 at ~800 BPS was measured at ~700 KB anon/block.
///
/// Peak (`57054fba`) + Land E: Critical + healthy supply holds at Elevated depth (24/32).
/// C2v2 extra raw-Critical cap (24→16) is off — it cut the window that printed 246/205.
/// r28 small-anon Elevated→nominal hold is off — peak Elevated is always 3/4.
#[inline]
fn pipeline_depth_for_pressure(level: PressureLevel, nominal: usize) -> usize {
    let mut nominal = pipeline_depth_for_engine_append(nominal);
    if tip_concurrency_adapt_from_env() {
        let segs = crate::storage::ibd_engine::tip_disk_segs_hint();
        let disk_ms = crate::storage::ibd_engine::tip_disk_ms_hint();
        if segs >= 2 || disk_ms >= 8 {
            nominal = (nominal / 2).max(8);
        }
    }
    let live = match effective_pressure_for_tip_crawl(level) {
        PressureLevel::Emergency => (nominal / 4).max(4),
        PressureLevel::Critical => (nominal / 2).max(8),
        PressureLevel::Elevated => (nominal * 3 / 4).max(12),
        PressureLevel::None => nominal,
    };
    static LAST_LIVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
    let prev = LAST_LIVE.swap(live as u64, std::sync::atomic::Ordering::Relaxed);
    crate::node::parallel_ibd::ms_breakdown::note_eff_depth(live);
    if prev != live as u64 {
        crate::node::parallel_ibd::memory::log_pressure_behavior(
            "pipeline_depth",
            if live < nominal { "shrink" } else { "hold_configured" },
            &format!("configured={nominal} live={live}"),
        );
    }
    live
}

/// How often the orchestrator polls MemoryGuard + engine spill in engine mode.
#[inline]
fn engine_pressure_poll_interval(level: PressureLevel) -> u64 {
    match effective_pressure_for_tip_crawl(level) {
        PressureLevel::Emergency => 1,
        PressureLevel::Critical => 4,
        PressureLevel::Elevated => 16,
        PressureLevel::None => 32,
    }
}

/// One block handed to the background retire thread (after validation enqueues the delta in `staged`).
///
/// Fields are `pub(crate)` so the sibling `retire_dispatcher` module can route on `height`
/// without importing the (mostly internal) validation pipeline. Construction stays
/// inside `validation_loop` (the only producer).
pub(crate) struct IbdRetireWork {
    pub(crate) height: u64,
    /// Empty. Retire only publishes the height; block bytes stay on the store path.
    pub(crate) blocks_buf: Vec<Arc<Block>>,
    pub(crate) block: Option<Arc<Block>>,
}


/// Join one UTXO flush worker and return its sub-MuHash (for batching before a global fold).
fn join_utxo_flush_handle_collect_sub_mh(
    handle: JoinHandle<Result<blvm_muhash::MuHash3072>>,
) -> Result<blvm_muhash::MuHash3072> {
    match handle.join() {
        Ok(Ok(sub_mh)) => Ok(sub_mh),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(anyhow::anyhow!("UTXO flush panicked: {e:?}")),
    }
}

/// RAII guard: joins all pending UTXO flush threads on drop (any exit path — success or error).
///
/// Without this guard, an early-return `Err` from `run_validation_loop` detaches the flush
/// threads (Rust drops `JoinHandle` by detaching, not joining).  The detached threads hold
/// `Arc<IbdUtxoStore>` clones that keep the standalone ibd_utxo_store LMDB environment open.
/// `try_catch_up_ibd` then calls `remove_dir_all` on that directory while the threads are still
/// running, deleting the files out from under the live env (`/proc/self/maps` shows `(deleted)`).
/// The subsequent session's `create_ibd_utxo_standalone_db` fails because the leaked threads
/// still hold `lock.mdb`, forcing a fallback to the slow main-storage LMDB.  Under the extra
/// memory pressure from the leaked ~30 GB mapping, prefetch reads miss single UTXOs → validation
/// failure → infinite retry loop.
struct UtxoFlushGuard(Arc<Mutex<VecDeque<JoinHandle<Result<blvm_muhash::MuHash3072>>>>>);
impl Drop for UtxoFlushGuard {
    fn drop(&mut self) {
        let handles: Vec<_> = self.0.lock().drain(..).collect();
        if handles.is_empty() {
            return;
        }
        warn!(
            "[IBD_FLUSH_GUARD] joining {} leaked UTXO flush thread(s) on validation-loop exit \
             (prevents ibd_utxo_store LMDB handle leak into next IBD session)",
            handles.len()
        );
        for h in handles {
            if let Err(e) = h.join() {
                warn!(
                    "[IBD_FLUSH_GUARD] UTXO flush thread panicked during cleanup: {:?}",
                    e
                );
            }
        }
    }
}

/// Drain and join all in-flight UTXO flush threads. Never hold `utxo_flush_handles` across `join()`.
fn join_all_utxo_flush_handles(
    utxo_flush_handles: &Arc<Mutex<VecDeque<JoinHandle<Result<blvm_muhash::MuHash3072>>>>>,
    log_label: &str,
) -> Result<blvm_muhash::MuHash3072> {
    let handles: Vec<_> = utxo_flush_handles.lock().drain(..).collect();
    let n = handles.len();
    if n > 0 {
        info!("IBD shutdown: joining {n} in-flight UTXO flush thread(s) ({log_label})");
    }
    let mut combined = blvm_muhash::MuHash3072::new();
    for (i, handle) in handles.into_iter().enumerate() {
        debug!(
            "IBD shutdown: UTXO flush join {}/{} ({log_label})",
            i + 1,
            n
        );
        let sub = join_utxo_flush_handle_collect_sub_mh(handle)?;
        combined = combined.multiply(&sub);
    }
    Ok(combined)
}

/// Drop all work channels (so each retire shard exits `recv`), join every shard, then
/// propagate the first stored error (if any). Equivalent to the pre-sharding behavior
/// for `BLVM_IBD_RETIRE_SHARDS=1`; for `>=2` it serially shuts down each shard. Errors
/// from `JoinHandle::join()` (panic in a retire thread) take precedence over `retire_err`,
/// because a panic indicates a programming bug we want to surface, while `retire_err`
/// is the path errors take when retire returned cleanly with a stored error.
fn retire_thread_shutdown(
    retire_dispatcher: &mut super::retire_dispatcher::RetireDispatcher,
    retire_err: &Arc<Mutex<Option<anyhow::Error>>>,
) -> Result<()> {
    retire_dispatcher.shutdown_and_join()?;
    if let Some(e) = retire_err.lock().take() {
        return Err(e);
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Parallel validation: dedicated `ibd-validate` worker thread.
//
// The orchestrator (main validation loop) builds the UTXO view for block h
// and hands it off to the worker. While the worker runs script verification,
// the orchestrator begins the store lookup for block h+1 (`overlap_prep`).
// This overlaps I/O (cold UTXO cache read) with CPU (script evaluation),
// giving a real throughput boost at heights 200k+ where both are non-trivial.
//
// max_in_flight = 1: at most one `ValidateJob` is live at any time, so BIP30
// state is simply moved into the job and returned in the result — no cloning.
// ──────────────────────────────────────────────────────────────────────────────

/// Everything the validation worker needs to build View(h) AND run `validate_block_only`.
///
/// Pipeline pattern: the orchestrator only takes cheap snapshots (Arc clones) and ships them to the
/// worker. The expensive work — UTXO cache lookups, disk supplement, staged-delta fold,
/// speculative-additions overlay, script verification — all happens on the worker thread, in
/// parallel across the N-thread worker pool.
struct EngineValidateJob {
    height: u64,
    block_arc: Arc<Block>,
    witnesses_storage: Arc<Vec<Vec<Witness>>>,
    bip30_index: Bip30Index,
    recent_headers: Arc<Vec<Arc<BlockHeader>>>,
    tx_ids: Vec<Hash>,
    best_header_chainwork: blvm_consensus::pow::U256,
    cached_network_time: u64,
    partial_session: PartialSpendSession,
    engine_append_ms: u64,
    /// I2: pre-built block output Arc map (built once on dispatch; reused in connect overlay apply).
    ibd_block_outputs:
        Option<Arc<rustc_hash::FxHashMap<blvm_consensus::OutPoint, Arc<blvm_consensus::UTXO>>>>,
}

/// Pre-append payload for the serial engine-append thread (`BLVM_IBD_ASYNC_ENGINE_APPEND`).
struct EngineAppendJob {
    height: u64,
    db: Arc<UtxoDatabase>,
    block_arc: Arc<Block>,
    witnesses_storage: Arc<Vec<Vec<Witness>>>,
    bip30_index: Bip30Index,
    recent_headers: Arc<Vec<Arc<BlockHeader>>>,
    tx_ids: Vec<Hash>,
    best_header_chainwork: blvm_consensus::pow::U256,
    cached_network_time: u64,
    ibd_block_outputs:
        Option<Arc<rustc_hash::FxHashMap<blvm_consensus::OutPoint, Arc<blvm_consensus::UTXO>>>>,
    /// R-360: when true the orchestrator skipped txid SHA and the output cache; the append
    /// thread computes both (`tx_ids` arrives empty, `ibd_block_outputs` arrives `None`).
    prep_deferred: bool,
}

/// R-361: the orchestrator held the **last** reference to every validated block (`[ARC_BLOCK_ENTRY_SC]
/// sc_after_block_arc_drop=1` on the skip path) and paid its deallocation — ~10–20k small frees
/// for a 2015 block — inside `drain` (0.40 / 0.60 ms/block at 300k+, the whole "drain residual"
/// after the R-360 timers found `skipchk` 0.01 and `flush` 0.00). Hand the last refs to one
/// `ibd-drop` thread instead. `BLVM_IBD_DEFERRED_DROP=0` restores the inline drop.
struct DeferredDrop {
    block: Arc<Block>,
    witnesses: Arc<Vec<Vec<Witness>>>,
    undo_log: Option<blvm_consensus::reorganization::BlockUndoLog>,
}

fn deferred_drop_enabled() -> bool {
    match std::env::var("BLVM_IBD_DEFERRED_DROP") {
        Ok(v) => {
            let t = v.trim();
            !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
        }
        Err(_) => true,
    }
}

/// Owns the `ibd-drop` thread; `Drop` closes the queue and joins (all exit paths).
struct DeferredDropper {
    tx: Option<crossbeam_channel::Sender<DeferredDrop>>,
    handle: Option<JoinHandle<()>>,
}

impl DeferredDropper {
    fn spawn() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<DeferredDrop>();
        let handle = std::thread::Builder::new()
            .name("ibd-drop".into())
            .spawn(move || {
                while let Ok(item) = rx.recv() {
                    let t0 = Instant::now();
                    drop(item);
                    crate::node::parallel_ibd::ms_breakdown::DEFERRED_DROP_QUEUE
                        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    crate::node::parallel_ibd::ms_breakdown::note_deferred_drop_us(
                        t0.elapsed().as_micros() as u64,
                    );
                }
            })
            .expect("spawn IBD deferred-drop thread");
        Self {
            tx: Some(tx),
            handle: Some(handle),
        }
    }
    fn disabled() -> Self {
        Self {
            tx: None,
            handle: None,
        }
    }
    /// Queue the block's last references for off-thread deallocation; returns `false`
    /// (and drops inline) when the dropper is off or gone.
    fn send(&self, item: DeferredDrop) -> bool {
        match self.tx.as_ref() {
            Some(tx) => {
                // R-362: gauge the backlog (`drop_q=` on MS_BREAKDOWN). Count before the send so
                // the thread's decrement can never race it below zero.
                let q = &crate::node::parallel_ibd::ms_breakdown::DEFERRED_DROP_QUEUE;
                q.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                match tx.send(item) {
                    Ok(()) => true,
                    Err(_) => {
                        q.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        false
                    }
                }
            }
            None => false,
        }
    }
    fn close_and_join(&mut self) {
        drop(self.tx.take());
        if let Some(h) = self.handle.take()
            && let Err(e) = h.join() {
                warn!("IBD deferred-drop join error: {:?}", e);
            }
    }
}

impl Drop for DeferredDropper {
    fn drop(&mut self) {
        self.close_and_join();
    }
}

/// R-362: the `[IBD_TIP_SKIP]` path (every 1000 blocks, and every block in the last 1024)
/// called `Storage::flush()` on the orchestrator. For heed3 that is `env.force_sync()` — under
/// `MDB_NOSYNC` an fsync of **every dirty page in the env**, i.e. the ~1000 block bodies the
/// persist lane wrote since the previous one (≈ 57 MB at 120–200k, ≈ 450 MB at 340–370k),
/// untimed, inside `drain`. R-361 drain/win: min 150–190 ms in every band (the per-block work),
/// median 338 / 454 / 366 / 492 / 721 ms, p90 up to 1304, max 2254 — one variable event per
/// window that scales with dirty bytes, which is exactly the fsync. `update_tip` (three small
/// write txns) stays inline; the sync moves to one `ibd-tip-sync` thread with a depth-1 queue
/// (a request that arrives while one is queued is folded into it — `force_sync` covers every
/// commit before it, so the tip it was asked for is always included). Durability cadence is
/// unchanged: same syncs, same heights, off the serial thread. Production always
/// uses the background thread. Tests may set `BLVM_IBD_TIP_SYNC=inline` so
/// `request` returns `false` and the caller flushes inline.
fn tip_sync_inline() -> bool {
    #[cfg(not(test))]
    {
        false
    }
    #[cfg(test)]
    {
        match std::env::var("BLVM_IBD_TIP_SYNC") {
            Ok(v) => {
                let t = v.trim();
                t.eq_ignore_ascii_case("inline") || t == "0" || t.eq_ignore_ascii_case("off")
            }
            Err(_) => false,
        }
    }
}

/// Owns the `ibd-tip-sync` thread; `Drop` closes the queue and joins (all exit paths).
struct TipSyncer {
    tx: Option<crossbeam_channel::Sender<u64>>,
    handle: Option<JoinHandle<()>>,
}

impl TipSyncer {
    fn spawn(storage: Arc<Storage>) -> Self {
        Self::spawn_with(move |_h| storage.flush())
    }
    /// `sync` runs on the `ibd-tip-sync` thread once per accepted request (coalesced).
    fn spawn_with<F>(sync: F) -> Self
    where
        F: Fn(u64) -> anyhow::Result<()> + Send + 'static,
    {
        let (tx, rx) = crossbeam_channel::bounded::<u64>(1);
        let handle = std::thread::Builder::new()
            .name("ibd-tip-sync".into())
            .spawn(move || {
                while let Ok(mut h) = rx.recv() {
                    // Fold anything that queued while we were syncing: one sync covers all.
                    while let Ok(h2) = rx.try_recv() {
                        h = h.max(h2);
                    }
                    let t0 = Instant::now();
                    match sync(h) {
                        Ok(()) => crate::node::parallel_ibd::ms_breakdown::note_tip_sync_bg_us(
                            t0.elapsed().as_micros() as u64,
                        ),
                        Err(e) => warn!(
                            "[IBD_TIP_SKIP] background sync after tip {} failed: {e:#}",
                            h
                        ),
                    }
                }
            })
            .expect("spawn IBD tip-sync thread");
        Self {
            tx: Some(tx),
            handle: Some(handle),
        }
    }
    fn inline() -> Self {
        Self {
            tx: None,
            handle: None,
        }
    }
    /// Ask for a sync covering `tip`. `true` = the sync will happen off-thread (either this
    /// request was queued, or one already queued will run after this tip's commit and so covers
    /// it). `false` = inline mode or the thread is gone; the caller must flush itself.
    fn request(&self, tip: u64) -> bool {
        match self.tx.as_ref() {
            Some(tx) => match tx.try_send(tip) {
                Ok(()) => true,
                Err(crossbeam_channel::TrySendError::Full(_)) => true,
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
            },
            None => false,
        }
    }
    fn close_and_join(&mut self) {
        drop(self.tx.take());
        if let Some(h) = self.handle.take()
            && let Err(e) = h.join() {
                warn!("IBD tip-sync join error: {:?}", e);
            }
    }
}

impl Drop for TipSyncer {
    fn drop(&mut self) {
        self.close_and_join();
    }
}

/// R-360: height-ordered reassembly for the serial append thread. The prep pool returns jobs
/// out of order; `SpendSession::append` must see `first, first+1, …` exactly once each.
struct InOrderJobs<T> {
    next: u64,
    pending: std::collections::BTreeMap<u64, T>,
}

impl<T> InOrderJobs<T> {
    fn new(first_height: u64) -> Self {
        Self {
            next: first_height,
            pending: std::collections::BTreeMap::new(),
        }
    }
    /// `Err((next, job))` when `height` is below the next expected height (already consumed).
    fn push(&mut self, height: u64, job: T) -> Result<(), (u64, T)> {
        if height < self.next {
            return Err((self.next, job));
        }
        self.pending.insert(height, job);
        Ok(())
    }
    /// The next in-order job, if it has arrived. Advances `next`.
    fn pop_ready(&mut self) -> Option<T> {
        let job = self.pending.remove(&self.next)?;
        self.next += 1;
        Some(job)
    }
    fn next_height(&self) -> u64 {
        self.next
    }
    fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// R-360: the per-block prep the orchestrator used to do at dispatch — serial txid SHA256d
/// and (below assume-valid) the block output cache — now run on the append thread.
/// Fills `tx_ids` when empty; returns the output cache exactly as dispatch built it.
fn engine_prep_deferred(
    block: &blvm_consensus::Block,
    height: u64,
    assume_valid_height: u64,
    tx_ids: &mut Vec<Hash>,
) -> Option<Arc<rustc_hash::FxHashMap<blvm_consensus::OutPoint, Arc<blvm_consensus::UTXO>>>> {
    if tx_ids.is_empty() {
        crate::storage::disk_utxo::compute_tx_ids_only(block, tx_ids);
    }
    // Queued jobs must not hold the output map. Connect builds it and drops it.
    let _ = (height, assume_valid_height);
    None
}

struct LegacyValidateJob {
    height: u64,
    block_arc: Arc<Block>,
    witnesses_storage: Arc<Vec<Vec<Witness>>>,
    bip30_index: Bip30Index,
    recent_headers: Arc<Vec<Arc<BlockHeader>>>,
    tx_ids: Vec<Hash>,
    best_header_chainwork: blvm_consensus::pow::U256,
    cached_network_time: u64,
    keys: Vec<OutPointKey>,
    spec_adds_snapshot: Vec<(u64, Arc<UtxoSet>)>,
    prefetched: PrefetchedUtxoMap,
    ibd_block_outputs:
        Option<Arc<rustc_hash::FxHashMap<blvm_consensus::OutPoint, Arc<blvm_consensus::UTXO>>>>,
}

enum ValidateJob {
    Engine(EngineValidateJob),
    Legacy(LegacyValidateJob),
}

/// Results returned from the validation worker for one block.
struct ValidateResult {
    height: u64,
    /// Tx ids are already available on the job and unused after validation; drop them here to
    /// avoid an `into_owned()` alloc (Vec<Hash> memcpy) per block on the IBD hot path.
    result: Result<Option<UtxoDelta>>,
    undo_log: blvm_consensus::reorganization::BlockUndoLog,
    /// BIP30 index after applying this block's coinbase rules.
    bip30_post: Bip30Index,
    /// Wall time spent inside the worker (view-build + `validate_block_only`).
    elapsed: std::time::Duration,
    /// Wall time spent building the view only (cache + supplement + fold + overlay).
    view_build_ms: u64,
    /// Engine Phase 1 append time (dispatch thread).
    engine_append_ms: u64,
    /// Engine Phase 2 query+fetch time (worker thread).
    engine_complete_ms: u64,
}

// ──────────────────────────────────────────────────────────────────────────────
// N-parallel validation pipeline:
//   - Up to N blocks are dispatched to N worker threads simultaneously.
//   - Each block h+k gets View(h+k) built using the store + staged deltas up to
//     h-1 PLUS speculative outputs from in-flight blocks h..h+k-1.
//   - Speculative outputs = all UTXO additions a block creates, computable
//     directly from its transaction outputs without running validation.
//     They equal D(h).additions for any valid block, so correctness holds.
//   - Results arrive in any order; the orchestrator retires in strict ascending
//     order so IbdUtxoStore invariants are preserved.
//   - Heights 91710..=91855: force pipeline_depth_live=1 so the two historical
//     duplicate-coinbase blocks update the index before the next height is dispatched.
//   - pipeline_depth (max in-flight) is decoupled from n_validate_workers (concurrent execution).
//     A deeper pipeline lets the dispatcher front-run so a single slow block at the head of the
//     in-order queue doesn't starve other workers.
// ──────────────────────────────────────────────────────────────────────────────

/// Per-block data carried from dispatch through result processing.
struct InFlightEntry {
    height: u64,
    block_arc: Arc<Block>,
    witnesses_storage: Arc<Vec<Vec<Witness>>>,
    feeder_est_bytes: usize,
    utxo_base_ms: u64,
    utxo_base_tune_ms: u64,
    prefetch_ms: u64,
    apply_pending_ms: u64,
    /// Input keys for this block — populated only on the error dump path (re-derived from
    /// the block). Kept as `Option` so we never clone ~5k keys per dispatched block just for
    /// a path that is hit at most once per run.
    input_keys: Option<Vec<OutPointKey>>,
}

// `speculative_additions_from_block` lives in `prefetch::build_spec_adds` now: building this
// `UtxoSet` ran on the validation dispatcher (single-threaded hot path) and was ~O(outputs)
// HashMap inserts + `Arc::new(UTXO)` allocations per block. We moved it onto the prefetch
// worker pool so the `cpus * 2` workers (otherwise stalled on RocksDB MultiGet RTTs) absorb it
// in parallel — i.e. spend-side prep on worker threads before validation consumes the view.

/// Worker thread loop: build the per-block UTXO view, then validate.
///
/// Each worker owns its own scratch buffers (`utxo_base`, key buffers) so allocations amortise
/// across the worker's lifetime. With N workers and N concurrent jobs, view-build runs N-way
/// parallel; the orchestrator stays a thin dispatcher.
///
/// `max_pending_ops` bounds `pending_shards` (UTXO ops awaiting disk flush). When the pending
/// log exceeds this, the worker yields/sleeps so the retire thread can drain. Without this,
/// validation can race tens of thousands of blocks ahead of retire, accumulating millions of
/// pending ops in RAM (→ OOM on 16 GiB hosts).
///
/// The cap is an `Arc<AtomicUsize>` so the retire-loop controller (Tier 3) can **adapt**
/// it online based on observed RSS pressure and drain throughput — see
/// [`adapt_max_pending_ops_tick`] for the policy. Workers reload the cap on every
/// backpressure check, so adaptive shrink/grow takes effect within one block. A loaded
/// value of `0` disables the limit entirely (high-RAM hosts where the full pipeline
/// trivially fits in RAM).
#[allow(clippy::too_many_arguments)]
fn run_validation_worker_shared(
    rx: crossbeam_channel::Receiver<ValidateJob>,
    tx: crossbeam_channel::Sender<ValidateResult>,
    // R-359: per-block MuHash subs leave on this side channel *after* the result is sent, so
    // the fold is off the head-of-line path the orchestrator blocks on.
    mh_tx: crossbeam_channel::Sender<super::muhash_fold::MuHashSub>,
    parallel_ibd: Arc<super::ParallelIBD>,
    blockstore: Arc<crate::storage::blockstore::BlockStore>,
    protocol: Arc<blvm_protocol::BitcoinProtocolEngine>,
    store: Arc<IbdUtxoStore>,
    last_retired: Arc<AtomicU64>,
    max_pending_ops: Arc<AtomicUsize>,
) {
    // Per-worker scratch buffers. UtxoSet capacity carries over (~peak inputs of recent block).
    let mut utxo_base: UtxoSet = UtxoSet::default();
    let mut supplement_cache_buf: Vec<OutPointKey> = Vec::new();
    let mut keys_missing_buf: Vec<OutPointKey> = Vec::new();
    // Per-worker apply scratch (one-per-thread; capacity grows to peak block size and amortises).
    // Each worker mutates the UTXO cache + pending log for its own block, so the
    // 8k DashMap ops + 16k pending pushes that used to bottleneck a single retire thread now run
    // N-way parallel. Pending-state mutex still serialises the bulk pushes, but the cache work
    // (deletions, worker_preinserted retire) is fully concurrent across workers.
    let mut del_scratch: Vec<OutPointKey> = Vec::new();
    let mut add_scratch: Vec<(OutPointKey, Arc<UTXO>)> = Vec::new();

    // A3 two-phase ECDSA wave: hold ValidateResult until wave hub finishes secp.
    // Worker can take another Engine job while prior wave verify runs.
    struct WavePending {
        vr: ValidateResult,
        rx: std::sync::mpsc::Receiver<std::result::Result<(), blvm_consensus::ConsensusError>>,
    }
    let mut wave_pending: Vec<WavePending> = Vec::new();
    let wave_depth: usize = std::env::var("BLVM_ECDSA_WAVE_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
        .clamp(1, 32);
    let flush_wave_ready =
        |pending: &mut Vec<WavePending>, out: &crossbeam_channel::Sender<ValidateResult>| {
            let mut i = 0;
            while i < pending.len() {
                match pending[i].rx.try_recv() {
                    Ok(Ok(())) => {
                        let WavePending { vr, .. } = pending.swap_remove(i);
                        let _ = out.send(vr);
                    }
                    Ok(Err(e)) => {
                        let WavePending { mut vr, .. } = pending.swap_remove(i);
                        vr.result = Err(anyhow::anyhow!("ECDSA wave: {e:?}"));
                        let _ = out.send(vr);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => i += 1,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        let WavePending { mut vr, .. } = pending.swap_remove(i);
                        vr.result = Err(anyhow::anyhow!("ECDSA wave disconnected"));
                        let _ = out.send(vr);
                    }
                }
            }
        };
    let block_wave_oldest =
        |pending: &mut Vec<WavePending>, out: &crossbeam_channel::Sender<ValidateResult>| {
            if pending.is_empty() {
                return;
            }
            let WavePending { mut vr, rx } = pending.remove(0);
            match rx.recv() {
                Ok(Ok(())) => {
                    let _ = out.send(vr);
                }
                Ok(Err(e)) => {
                    vr.result = Err(anyhow::anyhow!("ECDSA wave: {e:?}"));
                    let _ = out.send(vr);
                }
                Err(_) => {
                    vr.result = Err(anyhow::anyhow!("ECDSA wave disconnected"));
                    let _ = out.send(vr);
                }
            }
        };

    loop {
        flush_wave_ready(&mut wave_pending, &tx);
        while wave_pending.len() >= wave_depth {
            block_wave_oldest(&mut wave_pending, &tx);
            flush_wave_ready(&mut wave_pending, &tx);
        }

        let job = match rx.recv() {
            Ok(j) => j,
            Err(_) => {
                while !wave_pending.is_empty() {
                    block_wave_oldest(&mut wave_pending, &tx);
                }
                break;
            }
        };

        match job {
            ValidateJob::Legacy(mut lj) => {
                let height = lj.height;
                let t_view = std::time::Instant::now();
                utxo_base.clear();

                let n_keys = lj.keys.len();
                let n_prefetched = lj.prefetched.len();
                utxo_base.reserve(n_prefetched);
                let still_missing = &mut keys_missing_buf;
                still_missing.clear();

                if n_prefetched > 0 {
                    match Arc::try_unwrap(lj.prefetched) {
                        Ok(mut map) => {
                            for (k, arc) in map.drain() {
                                utxo_base.insert(key_to_outpoint(&k), arc);
                            }
                        }
                        Err(shared) => {
                            for (k, arc) in shared.iter() {
                                utxo_base.insert(key_to_outpoint(k), Arc::clone(arc));
                            }
                        }
                    }
                }

                if n_prefetched < n_keys {
                    for k in lj.keys.iter() {
                        if !utxo_base.contains_key(&key_to_outpoint(k)) {
                            still_missing.push(*k);
                        }
                    }

                    if !still_missing.is_empty() {
                        still_missing.retain(|k| {
                            if let Some(ref r) = store.cache_get(k) {
                                let op = key_to_outpoint(k);
                                utxo_base.insert(op, Arc::clone(&r.utxo));
                                return false;
                            }
                            true
                        });
                    }

                    if !still_missing.is_empty() && !lj.spec_adds_snapshot.is_empty() {
                        still_missing.retain(|k| {
                            let op = key_to_outpoint(k);
                            for (_sh, set) in lj.spec_adds_snapshot.iter().rev() {
                                if let Some(u) = set.get(&op) {
                                    utxo_base.insert(op, Arc::clone(u));
                                    return false;
                                }
                            }
                            true
                        });
                    }

                    if !still_missing.is_empty() {
                        store.supplement_utxo_map_with_buf(
                            &mut utxo_base,
                            still_missing,
                            &mut supplement_cache_buf,
                        );
                    }
                }
                let view_build_ms = t_view.elapsed().as_millis() as u64;

                let recent_opt = mtp_parent_headers(lj.height, lj.recent_headers.as_slice());
                let t_val = std::time::Instant::now();
                let raw = match recent_opt {
                    Err(e) => Err(e),
                    Ok(headers) => parallel_ibd.validate_block_only(
                    &blockstore,
                    protocol.as_ref(),
                    &mut utxo_base,
                    Some(&mut lj.bip30_index),
                    lj.block_arc.as_ref(),
                    Some(Arc::clone(&lj.block_arc)),
                    lj.witnesses_storage.as_slice(),
                    Some(&lj.witnesses_storage),
                    lj.height,
                    Some(headers),
                    lj.cached_network_time,
                    Some(&lj.tx_ids),
                    Some(lj.best_header_chainwork),
                    None,
                    lj.ibd_block_outputs.clone(),
                ),
                };
                let elapsed = t_val.elapsed();
                let (result, undo_log) = match raw {
                    Ok((_ids, delta, undo)) => (Ok(delta), undo),
                    Err(e) => (Err(e), blvm_consensus::reorganization::BlockUndoLog::new()),
                };
                if let Ok(Some(delta)) = &result {
                    store.worker_cache_put_protected(&delta.additions, height);
                    store.apply_utxo_delta(delta, height, &mut del_scratch, &mut add_scratch, true);
                }
                let _ = tx.send(ValidateResult {
                    height,
                    result,
                    undo_log,
                    bip30_post: lj.bip30_index,
                    elapsed,
                    view_build_ms,
                    engine_append_ms: 0,
                    engine_complete_ms: 0,
                });

                let cap = max_pending_ops.load(Ordering::Relaxed);
                if cap > 0 {
                    let mut spins = 0u32;
                    let spin_start = std::time::Instant::now();
                    const WORKER_SPIN_MAX: std::time::Duration = std::time::Duration::from_secs(60);
                    const WORKER_SPIN_LOG_EVERY: std::time::Duration =
                        std::time::Duration::from_secs(5);
                    let mut last_log = spin_start;
                    loop {
                        let pending_now = store.pending_len();
                        let cap_now = max_pending_ops.load(Ordering::Relaxed);
                        if pending_now <= cap_now {
                            break;
                        }
                        if spin_start.elapsed() >= WORKER_SPIN_MAX {
                            warn!(
                                "IBD worker: pending cap spin timeout at h={height} pending={pending_now} cap={cap_now}"
                            );
                            break;
                        }
                        if last_log.elapsed() >= WORKER_SPIN_LOG_EVERY {
                            warn!(
                                "IBD worker: waiting for pending drain at h={height} pending={pending_now} cap={cap_now} spins={spins}"
                            );
                            last_log = std::time::Instant::now();
                        }
                        spins = spins.saturating_add(1);
                        std::thread::yield_now();
                    }
                }
            }
            ValidateJob::Engine(mut ej) => {
                let height = ej.height;
                let t_view = std::time::Instant::now();
                utxo_base.clear();
                let t_complete = std::time::Instant::now();
                let session = match ej.partial_session.complete() {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx.send(ValidateResult {
                            height,
                            result: Err(e.context("IBD engine Phase 2 failed")),
                            undo_log: blvm_consensus::reorganization::BlockUndoLog::new(),
                            bip30_post: ej.bip30_index,
                            elapsed: std::time::Duration::ZERO,
                            view_build_ms: 0,
                            engine_append_ms: ej.engine_append_ms,
                            engine_complete_ms: 0,
                        });
                        continue;
                    }
                };
                // N28: SpendSessionLookup opt-in via BLVM_IBD_SPEND_LOOKUP=1 after
                // session_lookup_matches_fill gate. Default = fill (W2-1 resume-safe).
                let use_lookup = crate::storage::ibd_engine::spend_session_lookup_enabled();
                let t_fill = std::time::Instant::now();
                let fill_ms = if use_lookup {
                    0u64
                } else {
                    session_fill_utxo_set(&session, &mut utxo_base);
                    t_fill.elapsed().as_millis() as u64
                };
                let engine_complete_ms = t_complete.elapsed().as_millis() as u64;
                let view_build_ms = t_view.elapsed().as_millis() as u64;
                crate::storage::ibd_engine::note_tip_disk_hints(session.disk_segs, session.disk_ms);
                let hotpath_n = hotpath_timer_sample();
                if hotpath_n > 0 && height % hotpath_n == 0 {
                    info!(
                        "[IBD_HOTPATH] height={} query_ms={} ages_ms={} disk_ms={} preads={} pread_kb={} max_pread_kb={} cands={} segs={} fetch_ms={} map_ms={} fill_ms={} complete_ms={} inputs={}",
                        height,
                        session.query_ms,
                        session.ages_ms,
                        session.disk_ms,
                        session.disk_preads,
                        session.disk_pread_kb,
                        session.disk_max_pread_kb,
                        session.disk_cands,
                        session.disk_segs,
                        session.fetch_ms,
                        session.map_ms,
                        fill_ms,
                        engine_complete_ms,
                        session.details.len() + session.local_spends.len()
                    );
                }

                let recent_opt = mtp_parent_headers(ej.height, ej.recent_headers.as_slice());
                let t_val = std::time::Instant::now();
                let lookup = crate::storage::ibd_engine::SpendSessionLookup(&session);
                let raw = match recent_opt {
                    Err(e) => Err(e),
                    Ok(headers) if use_lookup => {
                        session_fill_coinbase_utxos(&session, &mut utxo_base);
                        parallel_ibd.validate_block_only(
                            &blockstore,
                            protocol.as_ref(),
                            &mut utxo_base,
                            Some(&mut ej.bip30_index),
                            ej.block_arc.as_ref(),
                            Some(Arc::clone(&ej.block_arc)),
                            ej.witnesses_storage.as_slice(),
                            Some(&ej.witnesses_storage),
                            ej.height,
                            Some(headers),
                            ej.cached_network_time,
                            Some(&ej.tx_ids),
                            Some(ej.best_header_chainwork),
                            Some(&lookup),
                            ej.ibd_block_outputs.clone(),
                        )
                    }
                    Ok(headers) => parallel_ibd.validate_block_only(
                        &blockstore,
                        protocol.as_ref(),
                        &mut utxo_base,
                        Some(&mut ej.bip30_index),
                        ej.block_arc.as_ref(),
                        Some(Arc::clone(&ej.block_arc)),
                        ej.witnesses_storage.as_slice(),
                        Some(&ej.witnesses_storage),
                        ej.height,
                        Some(headers),
                        ej.cached_network_time,
                        Some(&ej.tx_ids),
                        Some(ej.best_header_chainwork),
                        None,
                        ej.ibd_block_outputs.clone(),
                    ),
                };
                let elapsed = t_val.elapsed();
                let (result, undo_log) = match raw {
                    Ok((_ids, delta, undo)) => (Ok(delta), undo),
                    Err(e) => (Err(e), blvm_consensus::reorganization::BlockUndoLog::new()),
                };
                // R-359: the MuHash fold (~2 µs/element, ≈ 7.5 ms at 340–370k) used to run
                // here, before the send — i.e. on the head-of-line path the orchestrator blocks
                // on in `collect_wait`. Send (or park) the result first; fold afterwards and
                // ship the sub on the side channel. The orchestrator folds in height order.
                let result_ok = result.is_ok();
                let fold_muhash = result_ok && crate::config::ibd::ibd_engine_muhash_enabled();
                let mut vr_opt = Some(ValidateResult {
                    height,
                    result,
                    undo_log,
                    bip30_post: ej.bip30_index,
                    elapsed,
                    view_build_ms,
                    engine_append_ms: ej.engine_append_ms,
                    engine_complete_ms,
                });
                // A3: if connect parked ECDSA SoA, submit to wave and continue collecting.
                #[cfg(all(feature = "production"))]
                {
                    if result_ok
                        && let Some(soa) = blvm_consensus::ecdsa_wave::take_parked() {
                            let wrx = blvm_consensus::ecdsa_wave::submit(height, soa);
                            if let Some(vr) = vr_opt.take() {
                                wave_pending.push(WavePending { vr, rx: wrx });
                            }
                            // Join this height before taking more work when pending==1 would
                            // otherwise HOL the orch (in_flight=1 VALRES_STALL). Overlap only
                            // when wave_depth allows multiple pending ECDSA joins.
                            flush_wave_ready(&mut wave_pending, &tx);
                            if wave_pending.len() >= wave_depth.max(1) {
                                block_wave_oldest(&mut wave_pending, &tx);
                            } else if wave_pending.len() == 1 {
                                // Safe default: complete ECDSA before next collect.
                                block_wave_oldest(&mut wave_pending, &tx);
                            }
                        }
                }
                if let Some(vr) = vr_opt {
                    let _ = tx.send(vr);
                }
                if fold_muhash {
                    let t_mh = std::time::Instant::now();
                    let mut sub = blvm_muhash::MuHash3072::new();
                    crate::storage::ibd_utxo_muhash::fold_block_engine_muhash(
                        ej.block_arc.as_ref(),
                        &ej.tx_ids,
                        height,
                        &session,
                        &mut sub,
                    );
                    let _ = mh_tx.send(super::muhash_fold::MuHashSub {
                        height,
                        sub,
                        compute_us: t_mh.elapsed().as_micros() as u64,
                    });
                }
                continue;
            }
        }
    }
}

/// R-359: non-blocking — pull every sub that has arrived and fold the contiguous prefix.
fn muhash_fold_ready(
    folder: &mut super::muhash_fold::InOrderMuHashFolder,
    rx: &crossbeam_channel::Receiver<super::muhash_fold::MuHashSub>,
    acc: &Arc<Mutex<blvm_muhash::MuHash3072>>,
) {
    while let Ok(s) = rx.try_recv() {
        folder.push(s);
    }
    let us = {
        let mut g = acc.lock();
        folder.fold_ready(&mut g)
    };
    super::ms_breakdown::note_engine_muhash_us(us);
}

/// R-359 barrier: block until every sub for heights ≤ `through` is folded. Used before the
/// periodic running-state persist and at shutdown, so a persisted MuHash never lags the
/// height it is stamped with. Errors if the channel closes (or 60 s pass) with a gap left.
fn muhash_fold_through(
    folder: &mut super::muhash_fold::InOrderMuHashFolder,
    rx: &crossbeam_channel::Receiver<super::muhash_fold::MuHashSub>,
    acc: &Arc<Mutex<blvm_muhash::MuHash3072>>,
    through: u64,
) -> Result<()> {
    const MUHASH_BARRIER_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);
    muhash_fold_ready(folder, rx, acc);
    let started = std::time::Instant::now();
    while folder.folded_through() < through {
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(s) => {
                folder.push(s);
                muhash_fold_ready(folder, rx, acc);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                warn!(
                    "[IBD_MUHASH_BARRIER] waiting for MuHash sub h={} (need through {}, pending={}, {}s)",
                    folder.folded_through() + 1,
                    through,
                    folder.pending_len(),
                    started.elapsed().as_secs()
                );
                if started.elapsed() >= MUHASH_BARRIER_LIMIT {
                    return Err(anyhow::anyhow!(
                        "IBD MuHash fold: sub for h={} never arrived (need through {})",
                        folder.folded_through() + 1,
                        through
                    ));
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                muhash_fold_ready(folder, rx, acc);
                if folder.folded_through() < through {
                    return Err(anyhow::anyhow!(
                        "IBD MuHash fold: workers gone with sub for h={} missing (need through {})",
                        folder.folded_through() + 1,
                        through
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Adapt `max_pending_ops` online based on RSS pressure and pending-log fill ratio.
///
/// The nominal cap at IBD start comes from RAM tier defaults or `BLVM_IBD_MAX_PENDING_OPS`; it is
/// only the **anchor**. On real workloads the right live cap depends on (a) what RSS the host is
/// using now (pressure climbs as the UTXO cache grows past h≈200 k) and (b) whether retire keeps
/// up with validation.
/// Fixing the live cap at nominal alone breaks both ways: under pressure cap too high → OOM
/// risk; under calm cap too low → unnecessary validation throttle although retire could absorb more.
///
/// **Policy.**
/// - `Emergency` → halve the cap (floor `nominal/16`, hard floor `100 k`). RSS is
///   seconds from OOM, so the **only** safe action is shrinking the headroom validators
///   are allowed to occupy.
/// - `Critical` → multiply by 0.75 (floor `nominal/8`, hard floor `500 k`). Memory
///   guard is recommending eviction + flush; lowering the cap helps both happen sooner.
/// - `Elevated` → hold. The pressure response (lower `max_ahead_live`, more frequent
///   flushes) is enough; further cap-shrink would just stall workers without helping RSS.
/// - `None` → if `pending_len < cap/4` retire is keeping up trivially, grow the cap by
///   10% (capped at `1.1 × nominal`). Otherwise hold.
///
/// **Throttle.** Adaptation runs at most once per ~500 ms (controlled by
/// `last_adapt_ms`). Adjusting more often produces oscillation when pressure flicks
/// between bands every few blocks.
///
/// **Policy.**
fn adapt_max_pending_ops_tick(
    cap: &AtomicUsize,
    nominal: usize,
    pressure: PressureLevel,
    pending_len: usize,
    last_adapt_ms: &AtomicU64,
) {
    const TICK_INTERVAL_MS: u64 = 500;
    let now_ms = crate::utils::time::current_timestamp_millis();
    let last = last_adapt_ms.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) < TICK_INTERVAL_MS {
        return;
    }
    // CAS so two retire shards racing this don't both adapt in the same window.
    if last_adapt_ms
        .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    let current = cap.load(Ordering::Relaxed);
    let pressure = effective_pressure_for_tip_crawl(pressure);
    let new = match pressure {
        PressureLevel::Emergency => {
            // Pending log entries are small (~160 bytes each). Even 8M entries ≈ 1.3 GB,
            // which is small compared to the UTXO cache or pipeline buffers. Aggressive
            // halving (old: /2 per tick, floor=nominal/16) collapsed the cap to ≤500k on
            // large hosts, causing all workers to spin at the 60s safety valve → <1 BPS.
            // The actual memory relief comes from UTXO cache eviction and the coordinator
            // admission pause — not from throttling pending ops.
            //
            // New policy: gentle 10% trim, floor at nominal/2. This keeps workers moving
            // (they wait at most a few seconds before pending drains below the 2M–4M floor)
            // while still providing mild backpressure when retire falls behind.
            (current * 9 / 10).max(nominal / 2).max(1_000_000)
        }
        PressureLevel::Critical => (current * 3 / 4).max(nominal / 4).max(500_000),
        PressureLevel::Elevated => current,
        PressureLevel::None => {
            // Grow only when retire is *clearly* ahead — pending_len well below current.
            // Ceiling is 1.1× nominal (not 2×) to prevent a fast-drain burst from
            // granting workers a massive head-start that fills RAM once blocks get heavier
            // (the regression seen at h≈130k where 16M ops accumulated and retire crawled
            // at 6 BPS for hours). A 10% buffer above the budgeted nominal is enough to
            // absorb transient bursts without blowing the memory budget.
            if pending_len < current / 4 {
                let grown = (current as u128).saturating_mul(11) / 10;
                let max = (nominal as u128).saturating_mul(11) / 10;
                grown.min(max).max(nominal as u128 / 4) as usize
            } else {
                current
            }
        }
    };

    if new != current {
        cap.store(new, Ordering::Relaxed);
        if matches!(pressure, PressureLevel::Critical | PressureLevel::Emergency)
            || (pressure == PressureLevel::None && new > current)
        {
            // Log meaningful transitions only — Elevated holds wouldn't appear here, and
            // None-with-no-change returns early. Helps correlate observed BPS dips with
            // cap shrinks during post-mortems.
            tracing::debug!(
                "[IBD_ADAPT] max_pending_ops {} → {} (pressure={:?}, pending={}, nominal={})",
                current,
                new,
                pressure,
                pending_len,
                nominal
            );
        }
    }
}

/// Whether MTP seeding may fall back to the tip `recent_headers` sliding window.
///
/// Deep gap resume (export_h ≪ tip) must **not** use tip timestamps — that fails BIP113 H05.
#[inline]
pub(crate) fn mtp_tip_window_fallback_ok(start_height: u64, tip_height: u64) -> bool {
    tip_height > 0 && start_height.saturating_add(64) > tip_height
}

/// BIP113 window length: `MTP(h-1)` is the median of the 11 headers `h-11..=h-1`.
pub(crate) const MTP_WINDOW_HEADERS: usize = 11;

/// Advance the orchestrator's BIP113 window by the header of the block just **dispatched**.
///
/// Invariant (R-365): the window snapshot handed to the job for height `h` holds exactly the
/// headers `h-11..=h-1` (fewer only below height 11), so the worker computes `MTP(h-1)` — the
/// value Bitcoin's `ContextualCheckBlock` uses for the locktime cutoff and the H05 timestamp
/// rule. Pushing at drain instead lags by the pipeline depth and produces a stale, lower MTP.
#[inline]
pub(crate) fn push_dispatched_header(
    window: &mut VecDeque<Arc<BlockHeader>>,
    header: Arc<BlockHeader>,
) {
    window.push_back(header);
    while window.len() > MTP_WINDOW_HEADERS {
        window.pop_front();
    }
}

/// Parameters for the validation loop. Holds all captured state from the spawn closure.
pub struct ValidationParams {
    pub feeder_state: FeederState,
    pub ibd_store: Arc<IbdUtxoStore>,
    pub blockstore: Arc<BlockStore>,
    pub storage: Arc<Storage>,
    pub parallel_ibd: Arc<ParallelIBD>,
    pub protocol: Arc<BitcoinProtocolEngine>,
    pub utxo_mutex: Arc<std::sync::Mutex<UtxoSet>>,
    pub effective_end_live: Arc<std::sync::atomic::AtomicU64>,
    pub start_height: u64,
    pub validation_height: Arc<std::sync::atomic::AtomicU64>,
    pub mem_guard: MemoryGuard,
    pub max_ahead_live: Arc<std::sync::atomic::AtomicU64>,
    pub nominal_max_ahead: u64,
    /// Resolved **nominal** UTXO cache cap (from [`MemoryGuard::utxo_max_entries`] at IBD start).
    pub utxo_nominal_max_entries: usize,
    /// UTXO prefetch lookahead: **env > ibd.toml > default** (see [`super::ParallelIBDConfig::from_config`]).
    pub utxo_prefetch_lookahead: usize,
    /// Broadcast sender: validation loop broadcasts the height it is waiting for when stalled.
    /// Download workers subscribe and abort/retry stuck chunks that contain the stall height.
    pub stall_tx: tokio::sync::broadcast::Sender<u64>,
    /// Age-tiered UTXO engine. Parallel IBD always opens it. The heed checkpoint is the
    /// durable set this engine is seeded from.
    pub utxo_engine: Option<Arc<UtxoDatabase>>,
    /// Sender for periodic mid-IBD checkpoint heights. The retire loop sends a height here
    /// when it reaches each checkpoint multiple; the background export thread in mod.rs
    /// picks it up and runs `run_checkpoint_export`. `None` in non-engine mode.
    pub checkpoint_tx: Option<std::sync::mpsc::SyncSender<u64>>,
    /// Highest block height already stored in the block store at IBD startup.
    ///
    /// Blocks at heights ≤ this value are already persisted on disk (loaded via
    /// `try_load_local_ibd_block`).  Writing them back during local replay would be
    /// pure redundant I/O — serialise + LMDB-upsert of data that is already there —
    /// taking 37-76 seconds per 600-block flush batch and creating severe sawtooth
    /// BPS stalls.  Set to 0 when there are no pre-stored blocks (fresh IBD from
    /// genesis).
    pub local_replay_max_height: u64,
    /// When `Some(h)`, periodic checkpoint export is deferred until validation passes `h`.
    pub engine_gap_export_defer_until: Option<u64>,
    /// Assigner handle so feeder stalls under leftover cheese can arm GetData
    /// without waiting on a silent coordinator (live 70705 / 1080s abort).
    pub assigner: Arc<super::chunk_assigner::ChunkAssigner>,
}

/// Join in-flight async block flushes and persist any deferred batch before validation workers
/// retire. Avoids a single giant synchronous flush at process shutdown (observed wedge at ~954k).
fn drain_ibd_pending_blocks_before_shutdown(
    skip_storage: bool,
    parallel_ibd: &ParallelIBD,
    blockstore: &Arc<BlockStore>,
    storage: &Arc<Storage>,
    pending_blocks: &mut Vec<(
        Arc<Block>,
        Arc<Vec<Vec<Witness>>>,
        u64,
        blvm_consensus::reorganization::BlockUndoLog,
    )>,
    pending_storage_bytes: &mut u64,
    flush_handles: &mut VecDeque<std::thread::JoinHandle<Result<()>>>,
) -> Result<()> {
    while let Some(handle) = flush_handles.pop_front() {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Block storage flush thread panicked at IBD completion: {e:?}"
                ));
            }
        }
    }
    if skip_storage {
        pending_blocks.clear();
        *pending_storage_bytes = 0;
        return Ok(());
    }
    if !pending_blocks.is_empty() {
        info!(
            "Flushing {} deferred blocks before IBD shutdown",
            pending_blocks.len()
        );
        parallel_ibd.flush_pending_blocks_with_opts(
            blockstore,
            Some(storage),
            pending_blocks,
            IbdBlockFlushOpts::shutdown_sync(),
        )?;
    }
    Ok(())
}
