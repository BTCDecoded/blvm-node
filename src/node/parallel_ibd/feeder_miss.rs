//! Diagnosis-only: why the orchestrator fill loop stops short of `pipeline_depth`.
//!
//! No dispatch / inject / assigner policy. Opt out with `BLVM_IBD_FEEDER_MISS=0`.

use crate::storage::blockstore::BlockStore;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use tracing::info;

const REASON_STORE_ABSENT: usize = 0;
const REASON_FEEDER_OOO: usize = 1;
const REASON_IN_REORDER: usize = 2;
const REASON_SEQ_TIP_MISSING: usize = 3;
const REASON_INJECT_GATED: usize = 4;
const REASON_STORE_NOT_MOVED: usize = 5;
const REASON_TIMER_PARK: usize = 6;
const REASON_STAGED_CAP: usize = 7;
const N_REASONS: usize = 8;

const LAT_LT_1: usize = 0;
const LAT_1_5: usize = 1;
const LAT_5_25: usize = 2;
const LAT_25_100: usize = 3;
const LAT_100_250: usize = 4;
const LAT_GE_250: usize = 5;
const N_LAT: usize = 6;

pub static COORD_SEQUENTIAL: AtomicBool = AtomicBool::new(false);
pub static SEQ_TIP_MISSING: AtomicBool = AtomicBool::new(false);
pub static INJECT_GATED: AtomicBool = AtomicBool::new(false);
pub static GAP_POLL_TRUE: AtomicBool = AtomicBool::new(false);
pub static REORDER_HAS_ORCH: AtomicBool = AtomicBool::new(false);
pub static REORDER_LEN: AtomicUsize = AtomicUsize::new(0);
pub static REORDER_CONTIG: AtomicU64 = AtomicU64::new(0);
pub static LAST_DISPATCH_EMIT: AtomicU64 = AtomicU64::new(0);
pub static LAST_INJECT_NEW: AtomicU64 = AtomicU64::new(0);
pub static LAST_RECV_WAIT_MS: AtomicU64 = AtomicU64::new(0);
pub static COORD_LOOP_NS: AtomicU64 = AtomicU64::new(0);
pub static SEQ_SKIP_TIP_MISSING: AtomicU64 = AtomicU64::new(0);
pub static SEQ_SKIP_BAND: AtomicU64 = AtomicU64::new(0);
pub static ORCH_WANT: AtomicU64 = AtomicU64::new(0);
pub static NUM_PEERS: AtomicUsize = AtomicUsize::new(0);

static ENABLED: AtomicBool = AtomicBool::new(true);
static SHORT_N: AtomicU64 = AtomicU64::new(0);
static REASONS: [AtomicU64; N_REASONS] = [const { AtomicU64::new(0) }; N_REASONS];
static LAT_N: AtomicU64 = AtomicU64::new(0);
static LAT_SUM_NS: AtomicU64 = AtomicU64::new(0);
static LAT_MAX_NS: AtomicU64 = AtomicU64::new(0);
static LAT_MIN_NS: AtomicU64 = AtomicU64::new(u64::MAX);
static LAT_BUCKETS: [AtomicU64; N_LAT] = [const { AtomicU64::new(0) }; N_LAT];
static NEEDED_H: AtomicU64 = AtomicU64::new(0);
static NEEDED_NS: AtomicU64 = AtomicU64::new(0);
static LAST_STORE_H: AtomicU64 = AtomicU64::new(0);
static LAST_STORE_HAS: AtomicBool = AtomicBool::new(false);
static LAST_SAMPLE_NS: AtomicU64 = AtomicU64::new(0);
static LAST_DIST_NS: AtomicU64 = AtomicU64::new(0);

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn init_from_env() {
    let on = !matches!(
        std::env::var("BLVM_IBD_FEEDER_MISS").as_deref(),
        Ok("0") | Ok("false") | Ok("FALSE") | Ok("off")
    );
    ENABLED.store(on, Ordering::Relaxed);
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub fn note_coord_loop() {
    COORD_LOOP_NS.store(now_ns(), Ordering::Relaxed);
}

pub fn note_recv_wait_ms(ms: u64) {
    LAST_RECV_WAIT_MS.store(ms, Ordering::Relaxed);
}

pub fn note_dispatch_emit(emitted: u64, tip_missing: bool) {
    LAST_DISPATCH_EMIT.store(emitted, Ordering::Relaxed);
    SEQ_TIP_MISSING.store(tip_missing, Ordering::Relaxed);
}

pub fn note_inject(newly: u64, _from: u64, tip_in_pipeline: bool) {
    LAST_INJECT_NEW.store(newly, Ordering::Relaxed);
    if tip_in_pipeline || newly > 0 {
        INJECT_GATED.store(false, Ordering::Relaxed);
    }
}

pub fn note_inject_gated() {
    INJECT_GATED.store(true, Ordering::Relaxed);
}

pub fn publish_reorder(has_orch: bool, len: usize, contig: u64) {
    REORDER_HAS_ORCH.store(has_orch, Ordering::Relaxed);
    REORDER_LEN.store(len, Ordering::Relaxed);
    REORDER_CONTIG.store(contig, Ordering::Relaxed);
}

pub fn publish_orch_want(h: u64) {
    ORCH_WANT.store(h, Ordering::Relaxed);
}

/// Mark `h` as needed the first time the fill loop waits for it.
pub fn note_needed(h: u64) {
    if !enabled() {
        return;
    }
    if NEEDED_H.load(Ordering::Relaxed) != h {
        NEEDED_H.store(h, Ordering::Relaxed);
        NEEDED_NS.store(now_ns(), Ordering::Relaxed);
    }
}

/// Call on every feeder insert (in-order emit or contiguous flush).
pub fn note_feeder_insert(h: u64) {
    if !enabled() {
        return;
    }
    if NEEDED_H.load(Ordering::Relaxed) != h {
        return;
    }
    let start = NEEDED_NS.swap(0, Ordering::Relaxed);
    NEEDED_H.store(0, Ordering::Relaxed);
    if start == 0 {
        return;
    }
    let dt = now_ns().saturating_sub(start);
    LAT_N.fetch_add(1, Ordering::Relaxed);
    LAT_SUM_NS.fetch_add(dt, Ordering::Relaxed);
    LAT_MAX_NS.fetch_max(dt, Ordering::Relaxed);
    let mut min = LAT_MIN_NS.load(Ordering::Relaxed);
    while dt < min {
        match LAT_MIN_NS.compare_exchange_weak(min, dt, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(cur) => min = cur,
        }
    }
    let bucket = if dt < 1_000_000 {
        LAT_LT_1
    } else if dt < 5_000_000 {
        LAT_1_5
    } else if dt < 25_000_000 {
        LAT_5_25
    } else if dt < 100_000_000 {
        LAT_25_100
    } else if dt < 250_000_000 {
        LAT_100_250
    } else {
        LAT_GE_250
    };
    LAT_BUCKETS[bucket].fetch_add(1, Ordering::Relaxed);
}

/// Cached `has_block_body` for `wanted`. Coordinator leftover inject uses this
/// so genesis persist-on-disk (R-197 publisher off) can reload H without a
/// published `live_body_tip`.
pub(crate) fn wanted_body_on_disk(blockstore: &BlockStore, h: u64) -> bool {
    store_has_body(blockstore, h)
}

fn store_has_body(blockstore: &BlockStore, h: u64) -> bool {
    if LAST_STORE_H.load(Ordering::Relaxed) == h {
        return LAST_STORE_HAS.load(Ordering::Relaxed);
    }
    let has = (|| {
        let hash = blockstore.get_hash_by_height(h).ok().flatten()?;
        blockstore.has_block_body(&hash).ok()
    })()
    .unwrap_or(false);
    LAST_STORE_H.store(h, Ordering::Relaxed);
    LAST_STORE_HAS.store(has, Ordering::Relaxed);
    has
}

fn classify(
    store_has: bool,
    feeder_len: usize,
    in_reorder: bool,
    seq: bool,
    tip_missing: bool,
    inject_gated: bool,
    recv_wait_ms: u64,
    coord_age_ms: u64,
    staged_cap: bool,
) -> usize {
    if staged_cap {
        return REASON_STAGED_CAP;
    }
    if !store_has {
        return REASON_STORE_ABSENT;
    }
    if feeder_len > 0 {
        return REASON_FEEDER_OOO;
    }
    if in_reorder {
        return REASON_IN_REORDER;
    }
    if seq && tip_missing {
        return REASON_SEQ_TIP_MISSING;
    }
    if inject_gated {
        return REASON_INJECT_GATED;
    }
    if recv_wait_ms >= 40 || coord_age_ms >= 40 {
        return REASON_TIMER_PARK;
    }
    REASON_STORE_NOT_MOVED
}

fn reason_name(r: usize) -> &'static str {
    match r {
        REASON_STORE_ABSENT => "store_absent",
        REASON_FEEDER_OOO => "feeder_has_other",
        REASON_IN_REORDER => "in_reorder_not_feeder",
        REASON_SEQ_TIP_MISSING => "seq_tip_missing",
        REASON_INJECT_GATED => "inject_gated",
        REASON_STORE_NOT_MOVED => "store_has_not_moved",
        REASON_TIMER_PARK => "timer_park",
        REASON_STAGED_CAP => "staged_cap",
        _ => "unknown",
    }
}

fn path_label(seq: bool, tip_missing: bool) -> &'static str {
    if seq && tip_missing {
        "store→LOCAL_GAP→reorder→seq_dispatch(tip-only)→try_emit"
    } else if seq {
        "store→LOCAL_GAP→reorder→seq_dispatch(band32)→try_emit"
    } else {
        "store→LOCAL_GAP→reorder→dispatch_all→try_emit"
    }
}

/// Fill loop stopped short of configured depth.
pub fn on_fill_short(
    blockstore: &BlockStore,
    wanted: u64,
    in_flight: usize,
    depth: usize,
    feeder_len: usize,
    staged_cap: bool,
) {
    if !enabled() {
        return;
    }
    note_needed(wanted);
    let store_has = store_has_body(blockstore, wanted);
    let in_reorder = REORDER_HAS_ORCH.load(Ordering::Relaxed);
    let seq = COORD_SEQUENTIAL.load(Ordering::Relaxed);
    let tip_missing = SEQ_TIP_MISSING.load(Ordering::Relaxed);
    let inject_gated = INJECT_GATED.load(Ordering::Relaxed);
    let recv_wait_ms = LAST_RECV_WAIT_MS.load(Ordering::Relaxed);
    let loop_ns = COORD_LOOP_NS.load(Ordering::Relaxed);
    let coord_age_ms = if loop_ns == 0 {
        0
    } else {
        now_ns().saturating_sub(loop_ns) / 1_000_000
    };
    let reason = classify(
        store_has,
        feeder_len,
        in_reorder,
        seq,
        tip_missing,
        inject_gated,
        recv_wait_ms,
        coord_age_ms,
        staged_cap,
    );
    REASONS[reason].fetch_add(1, Ordering::Relaxed);
    let n = SHORT_N.fetch_add(1, Ordering::Relaxed) + 1;

    let now = now_ns();
    let last = LAST_SAMPLE_NS.load(Ordering::Relaxed);
    if n == 1 || now.saturating_sub(last) >= 100_000_000 {
        LAST_SAMPLE_NS.store(now, Ordering::Relaxed);
        info!(
            "[IBD_FEEDER_MISS] wanted={} inflight={}/{} store={} feeder={} reorder_len={} \
             reorder_has={} contig={} seq={} tip_missing={} inject_gated={} emit={} \
             inject_new={} recv_wait_ms={} coord_age_ms={} gap_poll={} seq_skip_miss={} \
             seq_skip_band={} reason={} path={} h_assigned_ms={}",
            wanted,
            in_flight,
            depth,
            u8::from(store_has),
            feeder_len,
            REORDER_LEN.load(Ordering::Relaxed),
            u8::from(in_reorder),
            REORDER_CONTIG.load(Ordering::Relaxed),
            u8::from(seq),
            u8::from(tip_missing),
            u8::from(inject_gated),
            LAST_DISPATCH_EMIT.load(Ordering::Relaxed),
            LAST_INJECT_NEW.load(Ordering::Relaxed),
            recv_wait_ms,
            coord_age_ms,
            u8::from(GAP_POLL_TRUE.load(Ordering::Relaxed)),
            SEQ_SKIP_TIP_MISSING.load(Ordering::Relaxed),
            SEQ_SKIP_BAND.load(Ordering::Relaxed),
            reason_name(reason),
            path_label(seq, tip_missing),
            if reason == REASON_STORE_ABSENT {
                crate::node::parallel_ibd::ms_breakdown::assigned_ms_ago(wanted)
            } else {
                -1
            },
        );
    }
    maybe_dump_dist(false);
}

pub fn dump_dist(force: bool) {
    if !enabled() {
        return;
    }
    LAST_DIST_NS.store(0, Ordering::Relaxed);
    maybe_dump_dist(force);
}

fn maybe_dump_dist(force: bool) {
    let now = now_ns();
    let last = LAST_DIST_NS.load(Ordering::Relaxed);
    if !force && now.saturating_sub(last) < 1_000_000_000 {
        return;
    }
    LAST_DIST_NS.store(now, Ordering::Relaxed);
    let n = SHORT_N.load(Ordering::Relaxed);
    if n == 0 && !force {
        return;
    }
    let lat_n = LAT_N.load(Ordering::Relaxed);
    let sum = LAT_SUM_NS.load(Ordering::Relaxed);
    let max_ns = LAT_MAX_NS.load(Ordering::Relaxed);
    let min_ns = LAT_MIN_NS.load(Ordering::Relaxed);
    let avg_ms = if lat_n == 0 {
        0.0
    } else {
        (sum as f64 / lat_n as f64) / 1_000_000.0
    };
    let min_ms = if min_ns == u64::MAX {
        0.0
    } else {
        min_ns as f64 / 1_000_000.0
    };
    let max_ms = max_ns as f64 / 1_000_000.0;
    let waiting = if NEEDED_NS.load(Ordering::Relaxed) > 0 {
        1
    } else {
        0
    };
    info!(
        "[IBD_FEEDER_MISS_DIST] short_n={} store_absent={} feeder_ooo={} in_reorder={} \
         seq_tip_missing={} inject_gated={} store_not_moved={} timer_park={} staged_cap={} \
         lat_n={} lat_avg_ms={:.3} lat_min_ms={:.3} lat_max_ms={:.3} \
         lat_hist=<1/1-5/5-25/25-100/100-250/>=250ms {}/{}/{}/{}/{}/{} still_waiting={} \
         seq={} peers={} seq_skip_miss={} seq_skip_band={}",
        n,
        REASONS[REASON_STORE_ABSENT].load(Ordering::Relaxed),
        REASONS[REASON_FEEDER_OOO].load(Ordering::Relaxed),
        REASONS[REASON_IN_REORDER].load(Ordering::Relaxed),
        REASONS[REASON_SEQ_TIP_MISSING].load(Ordering::Relaxed),
        REASONS[REASON_INJECT_GATED].load(Ordering::Relaxed),
        REASONS[REASON_STORE_NOT_MOVED].load(Ordering::Relaxed),
        REASONS[REASON_TIMER_PARK].load(Ordering::Relaxed),
        REASONS[REASON_STAGED_CAP].load(Ordering::Relaxed),
        lat_n,
        avg_ms,
        min_ms,
        max_ms,
        LAT_BUCKETS[LAT_LT_1].load(Ordering::Relaxed),
        LAT_BUCKETS[LAT_1_5].load(Ordering::Relaxed),
        LAT_BUCKETS[LAT_5_25].load(Ordering::Relaxed),
        LAT_BUCKETS[LAT_25_100].load(Ordering::Relaxed),
        LAT_BUCKETS[LAT_100_250].load(Ordering::Relaxed),
        LAT_BUCKETS[LAT_GE_250].load(Ordering::Relaxed),
        waiting,
        u8::from(COORD_SEQUENTIAL.load(Ordering::Relaxed)),
        NUM_PEERS.load(Ordering::Relaxed),
        SEQ_SKIP_TIP_MISSING.load(Ordering::Relaxed),
        SEQ_SKIP_BAND.load(Ordering::Relaxed),
    );
}

/// Startup banner — proves the local-disk peer made sequential mode.
pub fn log_startup(num_peers: usize, sequential: bool, inject_lookahead: u64) {
    init_from_env();
    NUM_PEERS.store(num_peers, Ordering::Relaxed);
    COORD_SEQUENTIAL.store(sequential, Ordering::Relaxed);
    if !enabled() {
        return;
    }
    info!(
        "[IBD_FEEDER_MISS] startup peers={} sequential={} seq_band=32 inject_lookahead={} \
         gap_poll_default_ms=250 path=store→LOCAL_GAP→reorder→seq_dispatch→try_emit",
        num_peers, sequential, inject_lookahead
    );
}
