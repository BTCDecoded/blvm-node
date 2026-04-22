//! IBD validation loop — runs on dedicated std::thread.
//!
//! Reads blocks from the feeder buffer, validates them, applies UTXO deltas,
//! and flushes to storage in batches. Extracted from parallel_ibd/mod.rs.

use super::feeder::FeederState;
use super::memory::{self, MemoryGuard, PressureLevel};
use crate::storage::blockstore::BlockStore;
use crate::storage::disk_utxo::{
    block_input_keys_batch_into_arc, block_input_keys_into_filtered, key_to_outpoint,
    outpoint_to_key, OutPointKey,
};
use crate::storage::ibd_utxo_store::IbdUtxoStore;
use crate::storage::Storage;
use anyhow::Result;
use blvm_protocol::bip_validation::Bip30Index;
use blvm_protocol::{segwit::Witness, BitcoinProtocolEngine, Block, BlockHeader, Hash, UtxoSet};
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};

/// Reuse `Arc<Vec<Vec<Witness>>>` of empty stacks for pre-segwit blocks (same `n` as tx count).
/// Validation runs on one thread — `thread_local` avoids a global mutex on this hot path.
thread_local! {
    static EMPTY_WITNESS_STACKS: RefCell<FxHashMap<usize, Arc<Vec<Vec<Witness>>>>> =
        RefCell::new(FxHashMap::default());
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

/// Wall-clock ms at last `mi_collect` / `malloc_trim` (lock-free throttle for RSS-pressure path).
static LAST_IBD_HEAP_TRIM_WALL_MS: AtomicU64 = AtomicU64::new(0);
const IBD_HEAP_TRIM_MIN_INTERVAL_MS: u64 = 10_000;

fn ibd_maybe_heap_trim() {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    loop {
        let prev = LAST_IBD_HEAP_TRIM_WALL_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < IBD_HEAP_TRIM_MIN_INTERVAL_MS {
            return;
        }
        if LAST_IBD_HEAP_TRIM_WALL_MS
            .compare_exchange_weak(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break;
        }
    }
    #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
}

use super::ParallelIBD;

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

/// Parameters for the validation loop. Holds all captured state from the spawn closure.
pub struct ValidationParams {
    pub feeder_state: FeederState,
    pub ibd_store: Arc<IbdUtxoStore>,
    pub blockstore: Arc<BlockStore>,
    pub storage: Arc<Storage>,
    pub parallel_ibd: Arc<ParallelIBD>,
    pub protocol: Arc<BitcoinProtocolEngine>,
    pub utxo_mutex: Arc<std::sync::Mutex<UtxoSet>>,
    pub effective_end_height: u64,
    pub start_height: u64,
    pub validation_height: Arc<std::sync::atomic::AtomicU64>,
    pub mem_guard: MemoryGuard,
    pub max_ahead_live: Arc<std::sync::atomic::AtomicU64>,
    pub nominal_max_ahead: u64,
    /// Resolved **nominal** UTXO cache cap (from [`MemoryGuard::utxo_max_entries`] at IBD start).
    pub utxo_nominal_max_entries: usize,
    /// UTXO prefetch lookahead: **env > ibd.toml > default** (see [`super::ParallelIBDConfig::from_config`]).
    pub utxo_prefetch_lookahead: usize,
}

/// Run the IBD validation loop. Called from std::thread::spawn.
pub fn run_validation_loop(params: ValidationParams) -> Result<()> {
    let feeder_state = params.feeder_state;
    let ibd_store_v2_for_validation = params.ibd_store;
    let blockstore = params.blockstore;
    let storage_clone = params.storage;
    let parallel_ibd = params.parallel_ibd;
    let protocol = params.protocol;
    let _utxo_mutex = params.utxo_mutex;
    let effective_end_height = params.effective_end_height;
    let start_height = params.start_height;
    let validation_height = params.validation_height;
    let mut mem_guard = params.mem_guard;
    let max_ahead_live = params.max_ahead_live;
    let nominal_max_ahead = params.nominal_max_ahead;
    let utxo_nominal_max_entries = params.utxo_nominal_max_entries;
    let nominal_prefetch_lookahead = params.utxo_prefetch_lookahead.clamp(1, 128);
    let utxo_prefetch_lookahead_live = AtomicUsize::new(nominal_prefetch_lookahead);

    //
    // Blocks may arrive out of order. We maintain a small reorder buffer
    // and flush in-order blocks immediately to minimize memory usage.
    //
    // PERFORMANCE OPTIMIZATION: We use deferred (batched) storage to avoid
    // per-block database writes. Validated blocks are stored in a pending
    // buffer and flushed in batches of 1000 blocks. This improves IBD
    // performance from ~2 blocks/sec to ~50+ blocks/sec.
    let mut blocks_synced = 0;
    let validation_start = std::time::Instant::now();

    // IBD Profiling (profile feature): BLVM_IBD_DEBUG=profile,blocked,disk or =profile:100,blocked or =full
    // Format: comma-separated. profile[:sample][:slow_ms] (e.g. profile:100 = every 100th block; profile:1:50 = slow threshold 50ms)
    #[cfg(feature = "profile")]
    let (ibd_profile_sample, ibd_profile_slow_ms, ibd_profile, ibd_disk_profile, ibd_blocked_log) = {
        let mut sample: u64 = 0;
        let mut slow: u64 = 0;
        let mut disk = false;
        let mut blocked_log = false;
        if let Ok(val) = std::env::var("BLVM_IBD_DEBUG") {
            let parts: Vec<&str> = val.split(',').map(|s| s.trim()).collect();
            let full = parts.iter().any(|p| *p == "full");
            for p in &parts {
                let p = *p;
                if p == "full" {
                    sample = sample.max(1);
                    disk = true;
                    blocked_log = true;
                } else if p == "profile" {
                    sample = sample.max(1);
                } else if let Some(rest_s) = p.strip_prefix("profile:") {
                    // Skip full "profile:" (8 chars); p[7..] wrongly kept a leading ':' and broke "profile:100"
                    let rest: Vec<&str> = rest_s.split(':').collect();
                    if !rest.is_empty() && !rest[0].is_empty() {
                        if let Ok(n) = rest[0].parse::<u64>() {
                            if rest.len() >= 2 && !rest[1].is_empty() {
                                // profile:sample:slow (e.g. profile:100:50)
                                sample = sample.max(n.max(1));
                                if let Ok(s) = rest[1].parse::<u64>() {
                                    slow = s;
                                }
                            } else if n < 100 {
                                // profile:50 = slow threshold 50ms (plan compat)
                                sample = sample.max(1);
                                slow = n;
                            } else {
                                // profile:100 = sample every 100 blocks
                                sample = sample.max(n);
                            }
                        }
                    }
                } else if p == "blocked" {
                    blocked_log = true;
                } else if p == "disk" {
                    disk = true;
                }
            }
            if full && sample == 0 {
                sample = 1;
                disk = true;
                blocked_log = true;
            }
            if sample > 0 && !blocked_log {
                blocked_log = true; // default blocked_log=ON when profile sampling is on
            }
        }
        let on = sample > 0 || disk;
        if on {
            info!("IBD profiling ENABLED (BLVM_IBD_DEBUG): sample_interval={}, slow_threshold_ms={}, disk_io={}, blocked_log={}", sample, slow, disk, blocked_log);
        }
        if blocked_log {
            info!("IBD_BLOCKED_LOG ENABLED: every validation-blocking phase will be logged");
        }
        (sample, slow, on, disk, blocked_log)
    };
    #[cfg(not(feature = "profile"))]
    let (ibd_profile_sample, ibd_profile_slow_ms, ibd_profile, ibd_disk_profile, ibd_blocked_log) =
        (0u64, 0u64, false, false, false);

    // Track last 11 block headers for BIP113 median-time-past calculation
    // Vec + drain keeps contiguity; avoids VecDeque::make_contiguous() per-block alloc
    let mut recent_headers_buf: VecDeque<Arc<BlockHeader>> = VecDeque::with_capacity(12);

    // DEFERRED STORAGE: Buffer validated blocks for batch commit
    // Keep flush interval small to avoid OOM on systems with limited RAM (16GB)
    let storage_flush_interval = mem_guard.storage_flush_interval;
    let mut pending_blocks: Vec<(Arc<Block>, Arc<Vec<Vec<Witness>>>, u64)> =
        Vec::with_capacity(storage_flush_interval);
    /// Sum of feeder `est_bytes` for entries in `pending_blocks` (same heuristic as [`super::types::estimate_block_bytes`]; pressure-path flush only).
    let mut pending_storage_bytes: u64 = 0;
    let skip_storage = false;
    let initial_buffer_limit = mem_guard.buffer_limit(start_height);

    info!(
        "Validation loop starting (deferred storage: flush every ~{} blocks [pressure-scaled], extra flush under Critical/Emergency when pending bytes exceed budget cap, initial buffer limit: {}, utxo_prefetch_lookahead_nominal: {})...",
        storage_flush_interval,
        initial_buffer_limit,
        nominal_prefetch_lookahead,
    );

    let mut next_validation_height = start_height;
    // Cache network_time for block header validation (reject future blocks). Refresh every 1000
    // blocks to avoid 800k+ SystemTime::now() syscalls during IBD while staying correct near tip.
    let mut cached_network_time = crate::utils::time::current_timestamp();

    // FEEDER BUFFER: Block feeder drains ready_rx into shared state. We read next block and
    // lookahead blocks for protect_keys. Buffer fills while validation runs.

    // Async flush: block batches on std::thread (validation runs off tokio).
    let mut flush_handles: VecDeque<std::thread::JoinHandle<Result<()>>> = VecDeque::new();
    let mut utxo_flush_handles: VecDeque<std::thread::JoinHandle<Result<()>>> = VecDeque::new();
    const MAX_UTXO_FLUSHES_IN_FLIGHT: usize = 1024;
    const MAX_UTXO_FLUSHES_UNDER_RSS_PRESSURE: usize = 2;
    let max_block_flushes_in_flight = mem_guard.max_block_flushes;

    let ibd_defer_flush = mem_guard.defer_flush;
    let ibd_defer_checkpoint = mem_guard.defer_checkpoint_interval;

    // Reusable buffers for protect_keys (avoids 2–4 Vec+HashSet allocs per block).
    let mut blocks_buf: Vec<Arc<Block>> = Vec::with_capacity(nominal_prefetch_lookahead.max(8));
    let mut keys_buf: Vec<OutPointKey> = Vec::new();
    let mut keys_seen: rustc_hash::FxHashSet<OutPointKey> = rustc_hash::FxHashSet::default();
    // IBD v2: reuse buffer for block_input_keys (avoids ~80KB alloc per block).
    let mut keys_v2_buf: Vec<OutPointKey> = Vec::new();
    // IBD v2: reuse utxo_base buffer (avoids UtxoSet alloc + ~2000 map ops per block).
    let mut utxo_base_buf: UtxoSet = UtxoSet::default();
    // Per-block map: retain + insert (avoids full clear when overlap high; reduces allocs).

    let mut keys_missing_buf: Vec<OutPointKey> = Vec::new();
    let mut supplement_cache_buf: Vec<OutPointKey> = Vec::new();
    // Cache BLVM_IBD_SNAPSHOT_DIR once at loop init (was std::env::var per block)
    let snapshot_dir_base: Option<String> = std::env::var("BLVM_IBD_SNAPSHOT_DIR").ok();
    // Same for optional BPS CSV (read on periodic IBD log intervals only, but avoid env lookup each time)
    let ibd_bps_csv_path: Option<String> = std::env::var("BLVM_IBD_BPS_CSV").ok();
    // #48: Tunable yield interval (default 500 for 5–10K BPS; fewer yields = less validation interruption)
    let yield_interval: u64 = std::env::var("BLVM_IBD_YIELD_INTERVAL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);

    // BIP30 O(1) index: for non-disk path, maintain locally. For disk path, DiskBackedUtxoSet owns it.
    let mut bip30_index = Bip30Index::default();
    // Recent rate: blocks since last status / elapsed since last status. Shows burst vs wait (avg can overstate when mostly waiting).
    let mut last_log_blocks: u64 = 0;
    let mut last_log_instant = std::time::Instant::now();
    // 5a: Adaptive mi_collect — run more often when RSS grows fast
    let mut last_rss_mb: u64 = 0;
    let mut last_collect_block: u64 = 0;
    // EMA of utxo-base build time for prefetch lookahead (single validation thread — no mutex).
    let mut prefetch_base_ema: Option<f64> = None;

    // Incremental UTXO commitment during IBD (Core-style; no full scan)
    #[cfg(all(feature = "utxo-commitments", feature = "production"))]
    let (mut commitment_tree_opt, commitment_store_opt) = {
        let pm = storage_clone.pruning();
        let tree = pm
            .as_ref()
            .and_then(|p| p.commitment_store())
            .and_then(|_| blvm_protocol::utxo_commitments::merkle_tree::UtxoMerkleTree::new().ok());
        let store = pm.and_then(|p| p.commitment_store());
        if tree.is_some() && store.is_some() {
            info!("IBD: incremental UTXO commitment enabled (applying delta per block)");
        }
        (tree, store)
    };
    #[cfg(not(all(feature = "utxo-commitments", feature = "production")))]
    // Placeholder `Option<()>` — real types only exist when `utxo-commitments` is enabled; this
    // branch is mutually exclusive with that code (avoids `None` type inference failure).
    #[allow(unused_variables)]
    let (mut commitment_tree_opt, commitment_store_opt) = (None::<()>, None::<()>);

    loop {
        // VALIDATION: Read from feeder buffer. Wait on Condvar when next block not yet arrived.
        // Feeder fills buffer while we validate; buffer grows to 20–50+ blocks when pipeline keeps up.
        // Use wait_timeout (5s) so we can log when stalled — helps diagnose freezes around 90k+.
        const FEEDER_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        let next_block = loop {
            let mut guard = feeder_state.0.lock();
            if let Some((arc_b, w, mut input_keys, u, tx_ids, est_bytes)) =
                guard.0.remove(next_validation_height)
            {
                guard.2 = guard.2.saturating_sub(est_bytes);
                feeder_state.1.notify_one();
                break Some((
                    next_validation_height,
                    arc_b,
                    w,
                    input_keys,
                    u,
                    tx_ids,
                    est_bytes,
                ));
            }
            if guard.1 && guard.0.is_empty() {
                break None;
            }
            #[cfg(feature = "profile")]
            let wait_start = std::time::Instant::now();
            let wait = feeder_state.1.wait_for(&mut guard, FEEDER_WAIT_TIMEOUT);
            #[cfg(feature = "profile")]
            if ibd_profile {
                let wait_ms = wait_start.elapsed().as_millis() as u64;
                if wait_ms >= 1 {
                    let buffer_len_after = guard.0.len();
                    let ts_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    blvm_protocol::profile_log!(
                        "[IBD_STALL_WAIT] next_height={} duration_ms={} buffer_after={} ts_ms={}",
                        next_validation_height,
                        wait_ms,
                        buffer_len_after,
                        ts_ms
                    );
                }
            }
            if wait.timed_out() {
                let cur_min = guard.0.min_buffered_height();
                warn!(
                    "[IBD_STALL] Validation waiting for block {} (buffer has {} blocks, min_height={:?}) — coordinator/feeder may be blocked",
                    next_validation_height, guard.0.len(), cur_min
                );
            }
        };
        let (
            next_height,
            block_arc,
            witnesses,
            mut input_keys_from_feeder,
            prefetched_utxos,
            tx_ids_precomputed,
            feeder_est_bytes,
        ) = match next_block {
            Some(t) => t,
            None => break, // Channel closed, buffer drained
        };
        next_validation_height = next_height + 1;
        if blocks_synced == 0 {
            info!("Validation: first block received, height {}", next_height);
        }

        // 4d: Single feeder_state lock for lookahead blocks (protect_keys). When dynamic eviction (needs protect_keys + evict_if_needed).
        let need_blocks_buf = ibd_store_v2_for_validation.is_dynamic_eviction();
        if need_blocks_buf {
            blocks_buf.clear();
            let guard = feeder_state.0.lock();
            let prefetch_look = utxo_prefetch_lookahead_live
                .load(Ordering::Relaxed)
                .clamp(1, 128);
            for off in 1..=prefetch_look {
                let h = next_height + off as u64;
                if let Some((b, _, _, _, _, _)) = guard.0.get(h) {
                    blocks_buf.push(Arc::clone(b));
                }
            }
        }

        // #21: tx_ids precomputed once in coordinator — feeder forwards; connect_block_ibd skips duplicate hash pass.

        // IBD v2: prefetch provides full map — no gap-fill needed.
        let prefetched_for_v2 = prefetched_utxos;
        #[cfg(feature = "profile")]
        let gap_fill_ms = 0u64;
        #[cfg(not(feature = "profile"))]
        #[allow(dead_code)]
        let gap_fill_ms = 0u64;

        // Build recent headers slice for BIP113 median-time-past
        let recent_headers_opt: Option<&[Arc<BlockHeader>]> = if recent_headers_buf.is_empty() {
            None
        } else {
            recent_headers_buf.make_contiguous();
            Some(recent_headers_buf.as_slices().0)
        };

        // One Arc<Vec<…>> per block: shared by connect (avoids `to_vec` in consensus) and block flush.
        let witnesses_storage: Arc<Vec<Vec<Witness>>> = if witnesses.is_empty() {
            shared_empty_witness_stacks(block_arc.transactions.len())
        } else if witnesses.len() != block_arc.transactions.len() {
            return Err(anyhow::anyhow!(
                "Witness count mismatch at height {}: {} witnesses for {} transactions",
                next_height,
                witnesses.len(),
                block_arc.transactions.len()
            ));
        } else {
            Arc::new(witnesses)
        };
        let witnesses_to_use: &[Vec<Witness>] = witnesses_storage.as_slice();
        let witnesses_arc_for_connect: Option<&Arc<Vec<Vec<Witness>>>> = Some(&witnesses_storage);

        // Validate + sync/evict (validation runs on dedicated thread — no tokio).
        #[cfg(feature = "profile")]
        if ibd_blocked_log {
            blvm_protocol::profile_log!(
                "[IBD_VALIDATION] height={} phase=start (validate+suggested sync)",
                next_height
            );
        }
        let (
            prefetch_ms,
            apply_pending_ms,
            validation_result,
            sync_ms,
            evict_ms,
            validation_time,
            utxo_base_ms,
            apply_utxo_ms,
            utxo_flush_batch,
            rss_pressure,
            utxo_base_tune_ms,
        ) = {
            let prefetch_ms = 0u64;
            #[cfg(feature = "profile")]
            let apply_pending_ms = 0u64;
            #[cfg(not(feature = "profile"))]
            let apply_pending_ms = 0u64;

            let (validation_result, validation_time, utxo_base_ms, utxo_base_tune_ms_holder) = {
                // Reuse coordinator/prefetch input keys — avoids a second O(inputs) scan per block.
                // Coordinator already filters intra-block spends; fallback replicates that filter.
                if input_keys_from_feeder.is_empty() {
                    block_input_keys_into_filtered(block_arc.as_ref(), &mut keys_v2_buf);
                } else {
                    std::mem::swap(&mut keys_v2_buf, &mut input_keys_from_feeder);
                }
                let keys_v2 = &keys_v2_buf;
                let store = &ibd_store_v2_for_validation;
                if next_height <= 200 {
                    debug!(
                        "[IBD_V2] height={} keys_needed={} store_len={}",
                        next_height,
                        keys_v2.len(),
                        store.len()
                    );
                }
                let prefetched = &prefetched_for_v2;
                let t_utxo_base = std::time::Instant::now();
                if prefetched.is_empty() {
                    store.build_utxo_map_into_with_buf(
                        keys_v2,
                        &mut utxo_base_buf,
                        &mut supplement_cache_buf,
                    );
                } else {
                    // Clear + rebuild: prefetch provides most keys; supplement handles misses.
                    // Faster than retain() which scans entire map capacity.
                    utxo_base_buf.clear();
                    utxo_base_buf.reserve(keys_v2.len());
                    keys_missing_buf.clear();
                    for k in keys_v2.iter() {
                        if let Some(arc) = prefetched.get(k) {
                            utxo_base_buf.insert(key_to_outpoint(k), Arc::clone(arc));
                        } else {
                            keys_missing_buf.push(*k);
                        }
                    }
                    if !keys_missing_buf.is_empty() {
                        store.supplement_utxo_map_with_buf(
                            &mut utxo_base_buf,
                            &keys_missing_buf,
                            &mut supplement_cache_buf,
                        );
                    }
                }
                let utxo_base_tune_ms_holder = t_utxo_base.elapsed().as_millis() as u64;
                #[cfg(feature = "profile")]
                let utxo_base_ms = utxo_base_tune_ms_holder;
                #[cfg(not(feature = "profile"))]
                let utxo_base_ms = 0u64;
                if next_height <= 200 && utxo_base_buf.len() < keys_v2.len() {
                    let first_missing = keys_v2
                        .iter()
                        .find(|k| !utxo_base_buf.contains_key(&key_to_outpoint(k)));
                    warn!(
                        "[IBD_V2] height={} utxo_base.len()={} keys_needed={} store_len={} first_missing={:?}",
                        next_height,
                        utxo_base_buf.len(),
                        keys_v2.len(),
                        store.len(),
                        first_missing.map(hex::encode)
                    );
                }
                if let Some(ref base) = snapshot_dir_base {
                    const SNAPSHOT_HEIGHTS: &[u64] = &[
                        50_000, 90_000, 125_000, 133_000, 145_000, 175_000, 181_000, 190_000,
                        200_000,
                    ];
                    if SNAPSHOT_HEIGHTS.contains(&next_height) {
                        let utxo_set = store.to_utxo_set_snapshot();
                        ParallelIBD::dump_ibd_snapshot(
                            next_height,
                            block_arc.as_ref(),
                            witnesses_to_use,
                            &utxo_set,
                            base,
                        );
                    }
                }
                let validation_start = std::time::Instant::now();
                let r = parallel_ibd.validate_block_only(
                    &blockstore,
                    protocol.as_ref(),
                    &mut utxo_base_buf,
                    Some(&mut bip30_index),
                    block_arc.as_ref(),
                    Some(Arc::clone(&block_arc)),
                    witnesses_to_use,
                    witnesses_arc_for_connect,
                    next_height,
                    recent_headers_opt,
                    cached_network_time,
                    Some(&tx_ids_precomputed),
                );
                (
                    r,
                    validation_start.elapsed(),
                    utxo_base_ms,
                    utxo_base_tune_ms_holder,
                )
            };

            let (
                sync_ms,
                evict_ms,
                utxo_flush_batch,
                rss_pressure,
                apply_utxo_ms,
                validation_result,
            ) = match validation_result {
                Ok((tx_ids_cow, utxo_delta_opt)) => {
                    #[cfg(feature = "profile")]
                    let t_apply_utxo = std::time::Instant::now();
                    #[cfg(feature = "profile")]
                    let mut protect_evict_ms: u64 = 0;
                    #[cfg(not(feature = "profile"))]
                    let protect_evict_ms: u64 = 0;
                    let flush = if let Some(delta) = utxo_delta_opt {
                        let store = &ibd_store_v2_for_validation;
                        #[cfg(all(feature = "utxo-commitments", feature = "production"))]
                        if let (Some(ref mut tree), Some(_)) =
                            (&mut commitment_tree_opt, &commitment_store_opt)
                        {
                            for dk in &delta.deletions {
                                let op =
                                    blvm_protocol::utxo_overlay::utxo_deletion_key_to_outpoint(dk);
                                let key = outpoint_to_key(&op);
                                if let Some(utxo) = store.get(&key) {
                                    if let Err(e) = tree.remove(&op, &utxo) {
                                        warn!(
                                            "IBD commitment: remove failed at height {}: {}",
                                            next_height, e
                                        );
                                    }
                                }
                            }
                            for (op, arc) in &delta.additions {
                                if let Err(e) = tree.insert(*op, arc.as_ref().clone()) {
                                    warn!(
                                        "IBD commitment: insert failed at height {}: {}",
                                        next_height, e
                                    );
                                }
                            }
                        }
                        store.apply_utxo_delta(delta, next_height);
                        if store.is_dynamic_eviction() {
                            #[cfg(feature = "profile")]
                            let t_protect_evict = std::time::Instant::now();
                            block_input_keys_batch_into_arc(
                                &blocks_buf,
                                &mut keys_buf,
                                &mut keys_seen,
                            );
                            store.protect_keys_for_next_blocks(&keys_buf);
                            store.evict_if_needed(next_height);
                            #[cfg(feature = "profile")]
                            {
                                protect_evict_ms = t_protect_evict.elapsed().as_millis() as u64;
                            }
                        }
                        let pressure_level =
                            mem_guard.should_flush(Some((&max_ahead_live, nominal_max_ahead)));
                        let rss_pressure = pressure_level >= PressureLevel::Elevated;
                        if rss_pressure {
                            info!(
                                    "[IBD_V2] height={} RSS pressure (cache={}, pending={}), forcing flush",
                                    next_height,
                                    store.len(),
                                    store.pending_len()
                                );
                            let batch = store.take_flush_batch_force();
                            ibd_maybe_heap_trim();
                            (0u64, protect_evict_ms, batch, true)
                        } else if ibd_defer_flush {
                            let at_checkpoint =
                                next_height > 0 && next_height % ibd_defer_checkpoint == 0;
                            let batch = if at_checkpoint {
                                store.take_flush_batch_force()
                            } else {
                                None
                            };
                            (0u64, protect_evict_ms, batch, false)
                        } else {
                            let batch = store.maybe_take_flush_batch();
                            (0u64, protect_evict_ms, batch, false)
                        }
                    } else {
                        (0u64, 0u64, None, false)
                    };
                    #[cfg(feature = "profile")]
                    let apply_utxo_ms = (t_apply_utxo.elapsed().as_millis())
                        .saturating_sub(protect_evict_ms as u128)
                        as u64;
                    #[cfg(not(feature = "profile"))]
                    let apply_utxo_ms = 0u64;
                    (
                        flush.0,
                        flush.1,
                        flush.2,
                        flush.3,
                        apply_utxo_ms,
                        Ok((tx_ids_cow, None::<blvm_protocol::block::UtxoDelta>)),
                    )
                }
                Err(e) => (0u64, 0u64, None, false, 0u64, Err(e)),
            };

            (
                prefetch_ms,
                apply_pending_ms,
                validation_result,
                sync_ms,
                evict_ms,
                validation_time,
                utxo_base_ms,
                apply_utxo_ms,
                utxo_flush_batch,
                rss_pressure,
                utxo_base_tune_ms_holder,
            )
        };

        // One read per block: should_flush (inside validation) may have updated the atomic.
        let ibd_pressure = memory::last_reported_pressure_level(&mem_guard);

        // Prefetch lookahead: EMA on utxo-base build time (no /proc); widen when supplement is slow.
        let ms = utxo_base_tune_ms as f64;
        let ema = match prefetch_base_ema {
            None => {
                prefetch_base_ema = Some(ms);
                ms
            }
            Some(prev) => {
                let n = prev * (63.0 / 64.0) + ms * (1.0 / 64.0);
                prefetch_base_ema = Some(n);
                n
            }
        };
        let mut target = nominal_prefetch_lookahead;
        if ema > 12.0 {
            target = (nominal_prefetch_lookahead + 32).min(128);
        } else if ema > 8.0 {
            target = (nominal_prefetch_lookahead * 4 / 3).min(128);
        } else if ema > 5.0 {
            target = (nominal_prefetch_lookahead + 16).min(128);
        } else if ema < 0.75 && blocks_synced > 1_000 {
            target = nominal_prefetch_lookahead.saturating_sub(8).max(48);
        }
        let with_pressure = dynamic_prefetch_lookahead(ibd_pressure, target);
        utxo_prefetch_lookahead_live.store(with_pressure, Ordering::Relaxed);

        // V2: no pipelined sync (overlay delta applied directly).

        #[cfg(feature = "profile")]
        if ibd_blocked_log {
            blvm_protocol::profile_log!(
                "[IBD_VALIDATION] height={} phase=end utxo_base_ms={} validation_ms={} apply_utxo_ms={} apply_pending_ms={} sync_ms={} evict_ms={}",
                next_height,
                utxo_base_ms,
                validation_time.as_millis(),
                apply_utxo_ms,
                apply_pending_ms,
                sync_ms,
                evict_ms
            );
            if apply_pending_ms > 2 {
                blvm_protocol::profile_log!(
                    "[IBD_BLOCKED] phase=apply_pending height={} duration_ms={} (pending_writes/flushing scan for cache hits)",
                    next_height, apply_pending_ms
                );
            }
            if sync_ms > 5 {
                blvm_protocol::profile_log!(
                    "[IBD_BLOCKED] phase=sync_await height={} duration_ms={} (validation waited for previous block sync+evict)",
                    next_height, sync_ms
                );
            }
        }
        if let Err(ref e) = validation_result {
            error!(
                "Failed to prefetch/validate block at height {}: {}",
                next_height, e
            );
        }

        match validation_result {
            Ok((_tx_ids, _utxo_delta)) => {
                // Sync/evict already done in block_in_place
                blocks_synced += 1;
                // Store commitment for this block (incremental; tree already updated)
                #[cfg(all(feature = "utxo-commitments", feature = "production"))]
                if let (Some(ref tree), Some(ref cstore)) =
                    (&commitment_tree_opt, &commitment_store_opt)
                {
                    let block_hash = blockstore.get_block_hash(block_arc.as_ref());
                    let commitment = tree.generate_commitment(block_hash, next_height);
                    if let Err(e) = cstore.store_commitment(&block_hash, next_height, &commitment) {
                        warn!(
                            "IBD commitment: store failed at height {}: {}",
                            next_height, e
                        );
                    }
                }
                let n_txs = block_arc.transactions.len();
                let n_inputs: usize = block_arc
                    .transactions
                    .iter()
                    .map(|tx| tx.inputs.len())
                    .sum();

                // Async UTXO flush: spawn per batch (parallel disk commits; overlaps validation).
                if let Some(pkg) = utxo_flush_batch {
                    let store = &ibd_store_v2_for_validation;
                    let flush_limit = if rss_pressure {
                        MAX_UTXO_FLUSHES_UNDER_RSS_PRESSURE
                    } else {
                        MAX_UTXO_FLUSHES_IN_FLIGHT
                    };
                    while utxo_flush_handles.len() >= flush_limit {
                        let Some(handle) = utxo_flush_handles.pop_front() else {
                            return Err(anyhow::anyhow!(
                                "IBD invariant violated: UTXO flush wait queue empty under backpressure"
                            ));
                        };
                        match handle.join() {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => return Err(e),
                            Err(e) => {
                                return Err(anyhow::anyhow!("UTXO flush panicked: {:?}", e));
                            }
                        }
                    }
                    let store_clone = Arc::clone(store);
                    let batch_size = pkg.ops.len();
                    utxo_flush_handles.push_back(std::thread::spawn(move || {
                        let prepared = pkg.prepare_for_disk()?;
                        store_clone.flush_prepared_package(&prepared)?;
                        store_clone.note_utxo_flush_completed(prepared.max_block_height);
                        Ok(())
                    }));
                    debug!(
                        "[IBD_DEBUG] Block {}: spawned UTXO flush (batch_size={}, in_flight={})",
                        next_height,
                        batch_size,
                        utxo_flush_handles.len()
                    );
                }

                // Track recent headers for BIP113 MTP (keep last 11). Clone header before moving
                // `block_arc` into `pending_blocks` so flush `Arc::try_unwrap` usually succeeds.
                let header_rc = Arc::new(block_arc.header.clone());
                if !skip_storage {
                    pending_storage_bytes =
                        pending_storage_bytes.saturating_add(feeder_est_bytes as u64);
                    pending_blocks.push((block_arc, Arc::clone(&witnesses_storage), next_height));
                }
                recent_headers_buf.push_back(header_rc);
                if recent_headers_buf.len() > 11 {
                    recent_headers_buf.pop_front();
                }

                // Update shared validation height (allows download workers to track progress)
                validation_height.store(next_height, Ordering::Relaxed);

                let flush_interval_live = mem_guard.storage_flush_interval_live(ibd_pressure);
                let byte_cap = mem_guard.storage_flush_pending_bytes_pressure_cap(ibd_pressure);
                let pressure_min_blocks =
                    MemoryGuard::storage_flush_pressure_min_blocks(flush_interval_live);
                let flush_by_interval = pending_blocks.len() >= flush_interval_live;
                let flush_by_pressure_bytes = byte_cap.is_some_and(|cap| {
                    pending_storage_bytes >= cap && pending_blocks.len() >= pressure_min_blocks
                });
                let (flush_ms, flushed_block_count) = if !skip_storage
                    && (flush_by_interval || flush_by_pressure_bytes)
                {
                    let flush_start = std::time::Instant::now();
                    // Fully overlapped async flush: backpressure when at MemoryGuard cap
                    while flush_handles.len() >= max_block_flushes_in_flight {
                        let in_flight = flush_handles.len();
                        let wait_start = std::time::Instant::now();
                        debug!(
                            "[IBD_DEBUG] Block {}: awaiting block storage flush slot (in_flight={}, pending_blocks={})",
                            next_height,
                            in_flight,
                            pending_blocks.len()
                        );
                        let Some(handle) = flush_handles.pop_front() else {
                            return Err(anyhow::anyhow!(
                                "IBD invariant violated: block storage flush wait queue empty under backpressure"
                            ));
                        };
                        match handle.join() {
                            Ok(Ok(())) => {
                                let waited_ms = wait_start.elapsed().as_millis() as u64;
                                debug!(
                                    "[IBD_DEBUG] Block {}: block storage flush slot free (waited {}ms)",
                                    next_height, waited_ms
                                );
                                #[cfg(feature = "profile")]
                                if ibd_blocked_log && waited_ms > 0 {
                                    blvm_protocol::profile_log!(
                                        "[IBD_BLOCKED]                                                 phase=block_flush_await height={} duration_ms={} in_flight={} utxo_flush={} (validation waited for block storage write)",
                                        next_height,
                                        waited_ms,
                                        in_flight,
                                        utxo_flush_handles.len()
                                    );
                                }
                            }
                            Ok(Err(e)) => return Err(e),
                            Err(e) => {
                                return Err(anyhow::anyhow!(
                                    "Block storage flush thread panicked: {:?}",
                                    e
                                ));
                            }
                        }
                    }
                    // UTXO flushes run in parallel (fire-and-forget); no barrier here.
                    // On crash, min(chain_tip, watermark) rewinds to the last safe point.
                    let to_flush = std::mem::take(&mut pending_blocks);
                    pending_storage_bytes = 0;
                    let blockstore_clone = Arc::clone(&blockstore);
                    let storage_for_flush = storage_clone.clone();
                    let to_flush_count = to_flush.len();
                    #[cfg(feature = "profile")]
                    if ibd_profile
                        && ibd_profile_height_matches_sample(ibd_profile_sample, next_height)
                    {
                        blvm_protocol::profile_log!(
                            "[IBD_BLOCK_FLUSH_SPAWN] height={} blocks={} in_flight={}",
                            next_height,
                            to_flush_count,
                            flush_handles.len(),
                        );
                    }
                    flush_handles.push_back(std::thread::spawn(move || {
                        ParallelIBD::do_flush_to_storage(
                            blockstore_clone.as_ref(),
                            Some(&storage_for_flush),
                            to_flush,
                        )
                    }));
                    let flush_elapsed = flush_start.elapsed().as_millis() as u64;
                    debug!(
                        "[IBD_DEBUG] Block {}: spawned block storage flush (blocks={}, in_flight={}, await_took={}ms)",
                        next_height,
                        to_flush_count,
                        flush_handles.len(),
                        flush_elapsed
                    );
                    (flush_elapsed, to_flush_count)
                } else {
                    (0, 0)
                };
                if !skip_storage && pending_blocks.is_empty() && flush_ms > 0 {
                    debug!(
                        "Started async flush ({} blocks, interval_live={}, pressure={:?}, by_bytes={}, {} in flight)",
                        flushed_block_count,
                        flush_interval_live,
                        ibd_pressure,
                        flush_by_pressure_bytes,
                        flush_handles.len()
                    );
                }

                // IBD Profiling: log per-block breakdown when enabled (profile feature)
                // Ready-queue: prefetch_await=0 by design (validation never awaits prefetch).
                #[cfg(feature = "profile")]
                if ibd_profile {
                    let prefetch_await_ms = 0u64; // Ready-queue: no prefetch_await
                    let val_ms = validation_time.as_millis() as u64;
                    let total_ms = prefetch_await_ms
                        + gap_fill_ms
                        + prefetch_ms
                        + utxo_base_ms
                        + val_ms
                        + apply_utxo_ms
                        + sync_ms
                        + evict_ms
                        + flush_ms;
                    let disk_total = prefetch_await_ms
                        + gap_fill_ms
                        + prefetch_ms
                        + sync_ms
                        + evict_ms
                        + flush_ms;
                    let should_log =
                        ibd_profile_height_matches_sample(ibd_profile_sample, next_height)
                            || (ibd_disk_profile
                                && (prefetch_await_ms > 0
                                    || gap_fill_ms > 0
                                    || prefetch_ms > 0
                                    || sync_ms > 0
                                    || evict_ms > 0))
                            || (ibd_profile_slow_ms > 0
                                && (prefetch_await_ms >= ibd_profile_slow_ms
                                    || gap_fill_ms >= ibd_profile_slow_ms
                                    || prefetch_ms >= ibd_profile_slow_ms
                                    || utxo_base_ms >= ibd_profile_slow_ms
                                    || val_ms >= ibd_profile_slow_ms
                                    || apply_utxo_ms >= ibd_profile_slow_ms
                                    || sync_ms >= ibd_profile_slow_ms
                                    || evict_ms >= ibd_profile_slow_ms
                                    || flush_ms >= ibd_profile_slow_ms));
                    if should_log && total_ms > 0 {
                        blvm_protocol::profile_log!(
                            "[IBD_PROFILE] height={} total_ms={} prefetch_await={} gap_fill={} prefetch={} utxo_base={} validation={} apply_utxo={} sync={} evict={} flush_coord={} disk_total={} txs={} inputs={}",
                            next_height,
                            total_ms,
                            prefetch_await_ms,
                            gap_fill_ms,
                            prefetch_ms,
                            utxo_base_ms,
                            val_ms,
                            apply_utxo_ms,
                            sync_ms,
                            evict_ms,
                            flush_ms,
                            disk_total,
                            n_txs,
                            n_inputs
                        );
                        let (dl, ch, ev, _ph) = ibd_store_v2_for_validation.stats();
                        let utxo_stats = (ibd_store_v2_for_validation.len(), dl, ch, ev);
                        blvm_protocol::profile_log!(
                            "[IBD_PIPELINE] height={} utxo_flush={} block_flush={} pending={} utxo_cache={} disk_loads={} cache_hits={} evictions={}",
                            next_height,
                            utxo_flush_handles.len(),
                            flush_handles.len(),
                            pending_blocks.len(),
                            utxo_stats.0,
                            utxo_stats.1,
                            utxo_stats.2,
                            utxo_stats.3
                        );
                    }
                }
            }
            Err(e) => {
                for handle in utxo_flush_handles.drain(..) {
                    let _ = handle.join();
                }
                for handle in flush_handles.drain(..) {
                    let _ = handle.join();
                }
                if !skip_storage && !pending_blocks.is_empty() {
                    let _ = parallel_ibd.flush_pending_blocks(
                        &blockstore,
                        Some(&storage_clone),
                        &mut pending_blocks,
                    );
                }
                error!("Failed to validate block at height {}: {}", next_height, e);
                // Diagnostic: identify which UTXOs were missing from utxo_base_buf
                // keys_v2_buf already filled in the validation path above
                {
                    let store = &ibd_store_v2_for_validation;
                    let prefetched = &prefetched_for_v2;
                    for k in keys_v2_buf.iter() {
                        let op = key_to_outpoint(k);
                        let in_base = utxo_base_buf.contains_key(&op);
                        if !in_base {
                            let in_prefetch = prefetched.contains_key(k);
                            let in_cache = store.cache_get(k).is_some();
                            error!(
                                "[IBD_MISSING_UTXO] height={} key={} in_prefetch={} in_cache={} (not in utxo_base_buf at validation time)",
                                next_height,
                                hex::encode(k),
                                in_prefetch,
                                in_cache,
                            );
                        }
                    }
                }
                let utxo_for_dump = ibd_store_v2_for_validation.build_utxo_map(&keys_v2_buf);
                ParallelIBD::dump_failed_block(
                    next_height,
                    block_arc.as_ref(),
                    witnesses_to_use,
                    &utxo_for_dump,
                    &e,
                );
                return Err(e);
            }
        }

        // CRITICAL: Yield to the runtime (BLVM_IBD_YIELD_INTERVAL, default 100)
        // Allows download workers to progress; fewer yields = less validation interruption
        if yield_interval > 0 && blocks_synced % yield_interval == 0 {
            #[cfg(feature = "profile")]
            if ibd_profile && ibd_profile_height_matches_sample(ibd_profile_sample, next_height) {
                blvm_protocol::profile_log!(
                    "[IBD_YIELD] blocks_synced={} utxo_flush={} block_flush={} (yielding to runtime)",
                    blocks_synced,
                    utxo_flush_handles.len(),
                    flush_handles.len()
                );
            }
            std::thread::yield_now();
        }

        // Periodic mimalloc page return. 5a: adaptive — every 1000 blocks, or sooner
        // when RSS grew >50MB since last collect (high allocation phase).
        if blocks_synced > 0 && blocks_synced % 500 == 0 {
            #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
            {
                let current_rss_mb = mem_guard.current_rss_mb();
                let rss_growth_mb = current_rss_mb.saturating_sub(last_rss_mb);
                let blocks_since_collect = blocks_synced.saturating_sub(last_collect_block);
                if rss_growth_mb > 50 || blocks_since_collect >= 1000 {
                    ibd_maybe_heap_trim();
                    last_rss_mb = mem_guard.current_rss_mb();
                    last_collect_block = blocks_synced;
                }
            }
        }

        // Progress logging: early (1, 10, 100), then every 100 until 10k (so monitors/logs
        // aren't stuck showing ~99 for hundreds of blocks), then every 1000.
        let should_log = blocks_synced == 1
            || blocks_synced == 10
            || blocks_synced == 100
            || (blocks_synced > 100
                && blocks_synced < 10_000
                && blocks_synced % 100 == 0
                && blocks_synced % 1000 != 0)
            || (blocks_synced > 0 && blocks_synced % 1000 == 0);
        if should_log {
            cached_network_time = crate::utils::time::current_timestamp();
            // Don't show BPS at blocks 1, 10: elapsed includes header sync + handshake (~15-20s),
            // which makes rate look absurdly low (1/17 = 0.06 blocks/s). From block 100 we have
            // meaningful validation throughput to measure.
            let total_elapsed = validation_start.elapsed().as_secs_f64();
            let average_rate = if blocks_synced >= 100 && total_elapsed > 0.0 {
                blocks_synced as f64 / total_elapsed
            } else {
                0.0
            };
            // Recent rate: blocks since last status / time since last status. Shows actual burst vs wait.
            // When avg >> recent, we're mostly waiting (download bottleneck). When avg ≈ recent, pipeline is full.
            let blocks_since_last = blocks_synced.saturating_sub(last_log_blocks);
            let recent_elapsed = last_log_instant.elapsed().as_secs_f64();
            let recent_rate = if blocks_since_last > 0 && recent_elapsed > 0.01 {
                blocks_since_last as f64 / recent_elapsed
            } else {
                0.0
            };
            last_log_blocks = blocks_synced;
            last_log_instant = std::time::Instant::now();

            let remaining = effective_end_height.saturating_sub(next_height);
            let eta = if average_rate > 0.0 {
                remaining as f64 / average_rate
            } else {
                f64::INFINITY
            };
            let buffer_size = feeder_state.0.lock().0.len();

            // Show avg (sustained) rate: blocks/total_time. Matches actual throughput.
            // Add recent rate so user sees: when height creeps slowly, recent << avg; when bursting, recent ≈ avg.
            let rate_str = if blocks_synced < 100 {
                "warming up (rate after block 100)".to_string()
            } else if blocks_synced >= 1000 && blocks_since_last > 0 {
                format!(
                    "{average_rate:.1} blocks/s avg (last {blocks_since_last} blocks: {recent_rate:.1} blocks/s)"
                )
            } else {
                format!("{average_rate:.1} blocks/s")
            };
            info!(
                "IBD: {} / {} ({:.1}%) - {} - buffer: {} - ETA: {:.0}s",
                next_height,
                effective_end_height,
                (next_height as f64 / effective_end_height as f64) * 100.0,
                rate_str,
                buffer_size,
                eta
            );
            // Memory diagnostics: log RSS breakdown and data structure sizes
            if blocks_synced % 5000 == 0 {
                let (rss_kb, swap_kb) = {
                    #[cfg(target_os = "linux")]
                    {
                        let rss = std::fs::read_to_string("/proc/self/status")
                            .ok()
                            .and_then(|s| {
                                s.lines()
                                    .find(|l| l.starts_with("VmRSS:"))
                                    .and_then(|l| l.split_whitespace().nth(1))
                                    .and_then(|v| v.parse::<u64>().ok())
                            })
                            .unwrap_or(0);
                        let swap = std::fs::read_to_string("/proc/self/status")
                            .ok()
                            .and_then(|s| {
                                s.lines()
                                    .find(|l| l.starts_with("VmSwap:"))
                                    .and_then(|l| l.split_whitespace().nth(1))
                                    .and_then(|v| v.parse::<u64>().ok())
                            })
                            .unwrap_or(0);
                        (rss, swap)
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        (0u64, 0u64)
                    }
                };
                let store_info = format!(
                    "utxo_cache={} pending={}",
                    ibd_store_v2_for_validation.len(),
                    ibd_store_v2_for_validation.pending_len()
                );
                info!(
                    "[MEM] h={} rss={}MB swap={}MB {} feeder={} threads={}",
                    next_height,
                    rss_kb / 1024,
                    swap_kb / 1024,
                    store_info,
                    buffer_size,
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(0)
                );
            }
            // BPS CSV for Core-comparable metrics (height,elapsed_sec) — same format as bitcoin-core-ibd-bench.sh
            if let Some(ref path) = ibd_bps_csv_path {
                let elapsed_sec = validation_start.elapsed().as_secs();
                let create_header = !std::path::Path::new(path).exists()
                    || std::fs::metadata(path)
                        .map(|m| m.len() == 0)
                        .unwrap_or(true);
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    use std::io::Write;
                    if create_header {
                        let _ = writeln!(f, "height,elapsed_sec");
                    }
                    let _ = writeln!(f, "{next_height},{elapsed_sec}");
                }
            }
            #[cfg(feature = "profile")]
            if ibd_profile {
                blvm_protocol::profile_log!(
                    "[IBD_PREFETCH_STATS] height={} utxo_flush={} block_flush={}",
                    next_height,
                    utxo_flush_handles.len(),
                    flush_handles.len()
                );
                if blocks_synced > 0 && blocks_synced % 5000 == 0 {
                    // IBD_UTXO_PATH: cumulative UTXO path stats for overlap/eviction analysis
                    let (dl, ch, ev, ph) = ibd_store_v2_for_validation.stats();
                    blvm_protocol::profile_log!(
                        "[IBD_UTXO_PATH] height={} disk_loads={} cache_hits={} evictions={} pending_hits={} cache_len={} (cumulative since start)",
                        next_height,
                        dl,
                        ch,
                        ev,
                        ph,
                        ibd_store_v2_for_validation.len()
                    );
                }
                if let Some((rss_mb, avail_mb)) = mem_guard.memory_diag() {
                    blvm_protocol::profile_log!(
                        "[IBD_DIAG] height={} rss_mb={} avail_mb={} utxo_flush={} block_flush={}",
                        next_height,
                        rss_mb,
                        avail_mb,
                        utxo_flush_handles.len(),
                        flush_handles.len()
                    );
                }
            }
        }
    }

    // Final UTXO flush: drain remaining pending ops, then join all in-flight handles.
    if let Some(pkg) = ibd_store_v2_for_validation.take_remaining_flush_package() {
        let store_clone = Arc::clone(&ibd_store_v2_for_validation);
        utxo_flush_handles.push_back(std::thread::spawn(move || {
            let prepared = pkg.prepare_for_disk()?;
            store_clone.flush_prepared_package(&prepared)?;
            store_clone.note_utxo_flush_completed(prepared.max_block_height);
            Ok(())
        }));
    }
    for handle in utxo_flush_handles.drain(..) {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => {
                return Err(anyhow::anyhow!("UTXO flush panicked at shutdown: {:?}", e));
            }
        }
    }
    let last_validated = next_validation_height.saturating_sub(1);
    if let Err(e) = ibd_store_v2_for_validation.flush_disk() {
        warn!(
            "Failed to flush ibd_utxos memtable at final shutdown (height {}): {}",
            last_validated, e
        );
    }

    for handle in flush_handles.drain(..) {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Block storage flush thread panicked: {:?}",
                    e
                ));
            }
        }
    }
    if !skip_storage && !pending_blocks.is_empty() {
        info!("Flushing final {} pending blocks", pending_blocks.len());
        parallel_ibd.flush_pending_blocks(
            &blockstore,
            Some(&storage_clone),
            &mut pending_blocks,
        )?;
    }

    let tip = storage_clone
        .chain()
        .get_height()
        .ok()
        .flatten()
        .unwrap_or(0);
    let _ = storage_clone.chain().set_utxo_watermark(tip);

    Ok(())
}
