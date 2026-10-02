//! Millisecond wall / supply / engine breakdown for IBD tip crawl.
//!
//! **Wall (exclusive, validation orchestrator thread):** time spent waiting on the
//! feeder, dispatching jobs, draining/retiring results, and residual overhead.
//!
//! **Tip supply (from [`super::tip_stage`], network tips only):** summed stage
//! segments (`need→body`, `getdata→body`, …) — not exclusive of wall (overlaps).
//!
//! **Engine (worker CPU):** summed `engine_append` + validate elapsed — may run
//! parallel to wall wait when the pipeline is deep.
//!
//! Default **on** (2026-08-22 soak). Set `BLVM_IBD_MS_BREAKDOWN=0` to disable.
//! Emit cadence: `BLVM_IBD_MS_BREAKDOWN_SECS` (default **2**).

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use tracing::info;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WallState {
    WaitFeeder,
    /// Take from feeder → job setup (excludes serial engine append).
    Dispatch,
    /// Orchestrator-thread `SpendSession::append` (serial before validate workers).
    EngineAppend,
    /// Blocking wait for next in-order validate result (was miscounted as Dispatch).
    CollectWait,
    Drain,
    Other,
}

fn enabled() -> bool {
    super::latch_env!(bool, {
        match std::env::var("BLVM_IBD_MS_BREAKDOWN")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            None => true,
            Some("0") | Some("false") | Some("off") | Some("no") => false,
            _ => true,
        }
    })
}

fn emit_secs() -> u64 {
    super::latch_env!(u64, {
        std::env::var("BLVM_IBD_MS_BREAKDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2)
            .clamp(1, 60)
    })
}

/// Per-event wait-duration buckets for `[IBD_TIP_HOLE_HIST]` / `[IBD_GD_SLOW_HIST]`.
/// Read-only: these never feed scheduling. Seconds live in the summed-ms column.
const HIST_LABELS: [&str; 10] = [
    "<10",
    "10-25",
    "25-50",
    "50-100",
    "100-200",
    "200-300",
    "300-500",
    "500-1000",
    "1000-2000",
    ">2000",
];

fn hist_idx(ms: u64) -> usize {
    match ms {
        0..=9 => 0,
        10..=24 => 1,
        25..=49 => 2,
        50..=99 => 3,
        100..=199 => 4,
        200..=299 => 5,
        300..=499 => 6,
        500..=999 => 7,
        1000..=1999 => 8,
        _ => 9,
    }
}

fn hist_add(n: &mut [u64; 10], sum: &mut [u64; 10], ms: u64) {
    let i = hist_idx(ms);
    n[i] = n[i].saturating_add(1);
    sum[i] = sum[i].saturating_add(ms);
}

fn hist_sub(cur: [u64; 10], prev: [u64; 10]) -> [u64; 10] {
    let mut out = [0u64; 10];
    for i in 0..10 {
        out[i] = cur[i].saturating_sub(prev[i]);
    }
    out
}

#[derive(Default, Clone)]
struct Buckets {
    wait_feeder_ms: u64,
    dispatch_ms: u64,
    engine_append_wall_ms: u64,
    collect_wait_ms: u64,
    drain_ms: u64,
    other_ms: u64,
    /// Wait-feeder ms attributed to binder class at wake.
    binder_tip_hole_ms: u64,
    binder_gd_slow_ms: u64,
    binder_empty_tip_ms: u64,
    binder_feeder_starve_ms: u64,
    binder_thin_runway_ms: u64,
    binder_failover_ms: u64,
    binder_engine_ms: u64,
    binder_pressure_ms: u64,
    binder_tip_hole_absent_ms: u64,
    binder_tip_hole_staged_ms: u64,
    parse_n: u64,
    parse_ms: u64,
    parse_offload_n: u64,
    parse_inline_n: u64,
    /// R-347: synchronous GAP_PERSIST (bincode + one LMDB write txn) inside the per-peer
    /// download worker — count, summed ms, max ms. `persist_ms / window_ms` is the share of
    /// wall the single LMDB writer lock was busy; near 1.0 = the persist stage is the wire
    /// ceiling (R-346 block wire flat 65–70 MB/s while busy rose 42 → 54).
    persist_n: u64,
    persist_ms: u64,
    persist_max_ms: u64,
    peer_rx_depth_sum: u64,
    peer_rx_depth_n: u64,
    peer_rx_depth_max: u64,
    /// Per-event tip_hole wait: count and summed ms per bucket (R-301).
    tip_hole_hist_n: [u64; 10],
    tip_hole_hist_ms: [u64; 10],
    gd_slow_hist_n: [u64; 10],
    gd_slow_hist_ms: [u64; 10],
    /// Exact thresholds for the hedge-MS question (100-200 straddles 150).
    tip_hole_ge150_ms: u64,
    tip_hole_ge300_ms: u64,
    tip_hole_ge500_ms: u64,
    gd_slow_ge150_ms: u64,
    gd_slow_ge300_ms: u64,
    gd_slow_ge500_ms: u64,
    tip_n: u64,
    tip_need_body_ms: u64,
    tip_gd_body_ms: u64,
    tip_body_feeder_ms: u64,
    tip_feeder_done_ms: u64,
    eng_n: u64,
    eng_append_ms: u64,
    /// Worker Phase 2 view-build (query/fetch/fill) ms sum.
    eng_view_ms: u64,
    /// Worker `validate_block_only` ms sum (scripts + connect checks).
    eng_validate_ms: u64,
    /// R-359: worker per-block MuHash fold µs sum (off the head path since R-359).
    eng_muhash_us: u64,
    /// R-359: orchestrator dispatch split — serial txid SHA256d µs sum.
    disp_txid_us: u64,
    /// R-359: orchestrator dispatch split — `build_block_output_utxo_cache` µs sum.
    disp_outcache_us: u64,
    /// R-360: append-thread prep (txid SHA + output cache moved off the orchestrator) µs sum.
    eng_prep_us: u64,
    /// R-360: drain split — `should_skip_block_store_write` (LMDB body probe) µs sum.
    drain_skipchk_us: u64,
    /// R-360: drain split — block-flush reap/decide/spawn section µs sum.
    drain_flush_us: u64,
    /// R-361: `ibd-drop` thread — deallocation of the block's last refs, off the orchestrator.
    eng_drop_us: u64,
    /// R-362: drain split — `[IBD_TIP_SKIP]` section on the orchestrator (`get_height` +
    /// `update_tip` + sync request, or the inline `force_sync` when `BLVM_IBD_TIP_SYNC=inline`).
    drain_tipskip_us: u64,
    /// R-362: `ibd-tip-sync` thread — `Storage::flush()` (heed3 `force_sync`) time, off the orchestrator.
    tipsync_bg_us: u64,
    /// Collect entered with head already in `pending_results` (no blocking recv).
    collect_ready_n: u64,
    /// Collect had to block on `valres_rx` for the head height.
    collect_block_n: u64,
    /// Pipeline-occupancy samples (one per collect). New subsystem: worker scheduling.
    collect_occ_n: u64,
    inflight_sum: u64,
    inflight_max: u64,
    inflight_full_n: u64,
    /// When head was not ready: how many later results were already buffered.
    pending_block_sum: u64,
    pending_block_n: u64,
    pending0_block_n: u64,
    /// in_flight buckets: 1–4, 5–8, 9–12, 13–16, 17+.
    inflight_h0: u64,
    inflight_h1: u64,
    inflight_h2: u64,
    inflight_h3: u64,
    inflight_h4: u64,
    /// Non-tip chunk assignment→first-body (R-280 supply). Same shape as tip_gd_body.
    bulk_n: u64,
    bulk_gd_body_ms: u64,
}

impl Buckets {
    fn wall_total(&self) -> u64 {
        self.wait_feeder_ms
            .saturating_add(self.dispatch_ms)
            .saturating_add(self.engine_append_wall_ms)
            .saturating_add(self.collect_wait_ms)
            .saturating_add(self.drain_ms)
            .saturating_add(self.other_ms)
    }

    fn add_wall(&mut self, state: WallState, ms: u64) {
        if ms == 0 {
            return;
        }
        match state {
            WallState::WaitFeeder => self.wait_feeder_ms = self.wait_feeder_ms.saturating_add(ms),
            WallState::Dispatch => self.dispatch_ms = self.dispatch_ms.saturating_add(ms),
            WallState::EngineAppend => {
                self.engine_append_wall_ms = self.engine_append_wall_ms.saturating_add(ms)
            }
            WallState::CollectWait => {
                self.collect_wait_ms = self.collect_wait_ms.saturating_add(ms)
            }
            WallState::Drain => self.drain_ms = self.drain_ms.saturating_add(ms),
            WallState::Other => self.other_ms = self.other_ms.saturating_add(ms),
        }
    }

    fn add_binder_wait(&mut self, binder: &str, ms: u64) {
        if ms == 0 {
            return;
        }
        match binder {
            "SUPPLY_TIP_HOLE" | "SUPPLY_TIP_HOLE_ABSENT" | "SUPPLY_TIP_HOLE_STAGED" => {
                self.binder_tip_hole_ms = self.binder_tip_hole_ms.saturating_add(ms);
                hist_add(&mut self.tip_hole_hist_n, &mut self.tip_hole_hist_ms, ms);
                if ms >= 150 {
                    self.tip_hole_ge150_ms = self.tip_hole_ge150_ms.saturating_add(ms);
                }
                if ms >= 300 {
                    self.tip_hole_ge300_ms = self.tip_hole_ge300_ms.saturating_add(ms);
                }
                if ms >= 500 {
                    self.tip_hole_ge500_ms = self.tip_hole_ge500_ms.saturating_add(ms);
                }
                if binder == "SUPPLY_TIP_HOLE_STAGED" {
                    self.binder_tip_hole_staged_ms =
                        self.binder_tip_hole_staged_ms.saturating_add(ms);
                } else {
                    // ABSENT, or legacy combined name: count as absent (H not arrived).
                    self.binder_tip_hole_absent_ms =
                        self.binder_tip_hole_absent_ms.saturating_add(ms);
                }
            }
            "SUPPLY_GD_SLOW" => {
                self.binder_gd_slow_ms = self.binder_gd_slow_ms.saturating_add(ms);
                hist_add(&mut self.gd_slow_hist_n, &mut self.gd_slow_hist_ms, ms);
                if ms >= 150 {
                    self.gd_slow_ge150_ms = self.gd_slow_ge150_ms.saturating_add(ms);
                }
                if ms >= 300 {
                    self.gd_slow_ge300_ms = self.gd_slow_ge300_ms.saturating_add(ms);
                }
                if ms >= 500 {
                    self.gd_slow_ge500_ms = self.gd_slow_ge500_ms.saturating_add(ms);
                }
            }
            "SUPPLY_EMPTY_TIP" => {
                self.binder_empty_tip_ms = self.binder_empty_tip_ms.saturating_add(ms)
            }
            "SUPPLY_FEEDER_STARVE" => {
                self.binder_feeder_starve_ms = self.binder_feeder_starve_ms.saturating_add(ms)
            }
            "SUPPLY_THIN_RUNWAY" => {
                self.binder_thin_runway_ms = self.binder_thin_runway_ms.saturating_add(ms)
            }
            "SUPPLY_FAILOVER" => {
                self.binder_failover_ms = self.binder_failover_ms.saturating_add(ms)
            }
            "ENGINE_PRESSURE" => {
                self.binder_pressure_ms = self.binder_pressure_ms.saturating_add(ms)
            }
            _ => self.binder_engine_ms = self.binder_engine_ms.saturating_add(ms),
        }
    }

    fn sub_snapshot(&self, prev: &Buckets) -> Buckets {
        Buckets {
            wait_feeder_ms: self.wait_feeder_ms.saturating_sub(prev.wait_feeder_ms),
            dispatch_ms: self.dispatch_ms.saturating_sub(prev.dispatch_ms),
            engine_append_wall_ms: self
                .engine_append_wall_ms
                .saturating_sub(prev.engine_append_wall_ms),
            collect_wait_ms: self.collect_wait_ms.saturating_sub(prev.collect_wait_ms),
            drain_ms: self.drain_ms.saturating_sub(prev.drain_ms),
            other_ms: self.other_ms.saturating_sub(prev.other_ms),
            binder_tip_hole_ms: self
                .binder_tip_hole_ms
                .saturating_sub(prev.binder_tip_hole_ms),
            binder_gd_slow_ms: self
                .binder_gd_slow_ms
                .saturating_sub(prev.binder_gd_slow_ms),
            binder_empty_tip_ms: self
                .binder_empty_tip_ms
                .saturating_sub(prev.binder_empty_tip_ms),
            binder_feeder_starve_ms: self
                .binder_feeder_starve_ms
                .saturating_sub(prev.binder_feeder_starve_ms),
            binder_thin_runway_ms: self
                .binder_thin_runway_ms
                .saturating_sub(prev.binder_thin_runway_ms),
            binder_failover_ms: self
                .binder_failover_ms
                .saturating_sub(prev.binder_failover_ms),
            binder_engine_ms: self.binder_engine_ms.saturating_sub(prev.binder_engine_ms),
            binder_pressure_ms: self
                .binder_pressure_ms
                .saturating_sub(prev.binder_pressure_ms),
            binder_tip_hole_absent_ms: self
                .binder_tip_hole_absent_ms
                .saturating_sub(prev.binder_tip_hole_absent_ms),
            binder_tip_hole_staged_ms: self
                .binder_tip_hole_staged_ms
                .saturating_sub(prev.binder_tip_hole_staged_ms),
            parse_n: self.parse_n.saturating_sub(prev.parse_n),
            parse_ms: self.parse_ms.saturating_sub(prev.parse_ms),
            parse_offload_n: self.parse_offload_n.saturating_sub(prev.parse_offload_n),
            parse_inline_n: self.parse_inline_n.saturating_sub(prev.parse_inline_n),
            persist_n: self.persist_n.saturating_sub(prev.persist_n),
            persist_ms: self.persist_ms.saturating_sub(prev.persist_ms),
            persist_max_ms: self.persist_max_ms,
            peer_rx_depth_sum: self
                .peer_rx_depth_sum
                .saturating_sub(prev.peer_rx_depth_sum),
            peer_rx_depth_n: self.peer_rx_depth_n.saturating_sub(prev.peer_rx_depth_n),
            peer_rx_depth_max: self.peer_rx_depth_max,
            tip_hole_hist_n: hist_sub(self.tip_hole_hist_n, prev.tip_hole_hist_n),
            tip_hole_hist_ms: hist_sub(self.tip_hole_hist_ms, prev.tip_hole_hist_ms),
            gd_slow_hist_n: hist_sub(self.gd_slow_hist_n, prev.gd_slow_hist_n),
            gd_slow_hist_ms: hist_sub(self.gd_slow_hist_ms, prev.gd_slow_hist_ms),
            tip_hole_ge150_ms: self
                .tip_hole_ge150_ms
                .saturating_sub(prev.tip_hole_ge150_ms),
            tip_hole_ge300_ms: self
                .tip_hole_ge300_ms
                .saturating_sub(prev.tip_hole_ge300_ms),
            tip_hole_ge500_ms: self
                .tip_hole_ge500_ms
                .saturating_sub(prev.tip_hole_ge500_ms),
            gd_slow_ge150_ms: self.gd_slow_ge150_ms.saturating_sub(prev.gd_slow_ge150_ms),
            gd_slow_ge300_ms: self.gd_slow_ge300_ms.saturating_sub(prev.gd_slow_ge300_ms),
            gd_slow_ge500_ms: self.gd_slow_ge500_ms.saturating_sub(prev.gd_slow_ge500_ms),
            tip_n: self.tip_n.saturating_sub(prev.tip_n),
            tip_need_body_ms: self.tip_need_body_ms.saturating_sub(prev.tip_need_body_ms),
            tip_gd_body_ms: self.tip_gd_body_ms.saturating_sub(prev.tip_gd_body_ms),
            tip_body_feeder_ms: self
                .tip_body_feeder_ms
                .saturating_sub(prev.tip_body_feeder_ms),
            tip_feeder_done_ms: self
                .tip_feeder_done_ms
                .saturating_sub(prev.tip_feeder_done_ms),
            eng_n: self.eng_n.saturating_sub(prev.eng_n),
            eng_append_ms: self.eng_append_ms.saturating_sub(prev.eng_append_ms),
            eng_view_ms: self.eng_view_ms.saturating_sub(prev.eng_view_ms),
            eng_validate_ms: self.eng_validate_ms.saturating_sub(prev.eng_validate_ms),
            eng_muhash_us: self.eng_muhash_us.saturating_sub(prev.eng_muhash_us),
            disp_txid_us: self.disp_txid_us.saturating_sub(prev.disp_txid_us),
            disp_outcache_us: self.disp_outcache_us.saturating_sub(prev.disp_outcache_us),
            eng_prep_us: self.eng_prep_us.saturating_sub(prev.eng_prep_us),
            drain_skipchk_us: self.drain_skipchk_us.saturating_sub(prev.drain_skipchk_us),
            drain_flush_us: self.drain_flush_us.saturating_sub(prev.drain_flush_us),
            eng_drop_us: self.eng_drop_us.saturating_sub(prev.eng_drop_us),
            drain_tipskip_us: self.drain_tipskip_us.saturating_sub(prev.drain_tipskip_us),
            tipsync_bg_us: self.tipsync_bg_us.saturating_sub(prev.tipsync_bg_us),
            collect_ready_n: self.collect_ready_n.saturating_sub(prev.collect_ready_n),
            collect_block_n: self.collect_block_n.saturating_sub(prev.collect_block_n),
            collect_occ_n: self.collect_occ_n.saturating_sub(prev.collect_occ_n),
            inflight_sum: self.inflight_sum.saturating_sub(prev.inflight_sum),
            inflight_max: 0,
            inflight_full_n: self.inflight_full_n.saturating_sub(prev.inflight_full_n),
            pending_block_sum: self
                .pending_block_sum
                .saturating_sub(prev.pending_block_sum),
            pending_block_n: self.pending_block_n.saturating_sub(prev.pending_block_n),
            pending0_block_n: self.pending0_block_n.saturating_sub(prev.pending0_block_n),
            inflight_h0: self.inflight_h0.saturating_sub(prev.inflight_h0),
            inflight_h1: self.inflight_h1.saturating_sub(prev.inflight_h1),
            inflight_h2: self.inflight_h2.saturating_sub(prev.inflight_h2),
            inflight_h3: self.inflight_h3.saturating_sub(prev.inflight_h3),
            inflight_h4: self.inflight_h4.saturating_sub(prev.inflight_h4),
            bulk_n: self.bulk_n.saturating_sub(prev.bulk_n),
            bulk_gd_body_ms: self.bulk_gd_body_ms.saturating_sub(prev.bulk_gd_body_ms),
        }
    }
}

struct Shared {
    cum: Buckets,
    last_emit: Buckets,
    last_emit_at: Instant,
}

fn shared() -> &'static Mutex<Shared> {
    static S: OnceLock<Mutex<Shared>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(Shared {
            cum: Buckets::default(),
            last_emit: Buckets::default(),
            last_emit_at: Instant::now(),
        })
    })
}

struct WallLocal {
    state: WallState,
    since: Instant,
    started: bool,
    /// R-363: sub-millisecond remainder per state. Slices were truncated with `as_millis()` at
    /// every state switch (≈ 4 per block), so sub-ms states read 0 and Σ`window_ms` fell short
    /// of the band wall by 6–26 % (R-362: 16.6 / 13.6 / 13.0 / 9.0 / 10.2 s per band). Carry
    /// the remainder so each state's sum is exact to < 1 ms over the run.
    carry_us: [u64; 6],
}

#[inline]
fn wall_state_idx(state: WallState) -> usize {
    match state {
        WallState::WaitFeeder => 0,
        WallState::Dispatch => 1,
        WallState::EngineAppend => 2,
        WallState::CollectWait => 3,
        WallState::Drain => 4,
        WallState::Other => 5,
    }
}

thread_local! {
    static WALL: RefCell<WallLocal> = RefCell::new(WallLocal {
        state: WallState::Other,
        since: Instant::now(),
        started: false,
        carry_us: [0; 6],
    });
}

static ARMED: AtomicBool = AtomicBool::new(false);
static PEERS_CONN: AtomicUsize = AtomicUsize::new(0);
static PEERS_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
static EFF_DEPTH: AtomicUsize = AtomicUsize::new(0);
static SLOW_PCT: AtomicU64 = AtomicU64::new(0);

/// Last-assigned covering ranges: (start, end, wall_ms). Newest at back.
fn assigned_ranges() -> &'static Mutex<VecDeque<(u64, u64, u64)>> {
    static R: OnceLock<Mutex<VecDeque<(u64, u64, u64)>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(VecDeque::with_capacity(512)))
}

fn bulk_counted() -> &'static Mutex<HashSet<u64>> {
    static C: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashSet::new()))
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Drop covering ranges so a unit test does not see another test's GetData.
#[cfg(test)]
pub(crate) fn test_reset_assigned() {
    if let Ok(mut g) = assigned_ranges().lock() {
        g.clear();
    }
}

/// Record that `start..=end` was assigned (GetData path). Last-wins for covering lookup.
pub(crate) fn note_assigned(start: u64, end: u64) {
    if !enabled() {
        return;
    }
    let now = wall_ms();
    if let Ok(mut g) = assigned_ranges().lock() {
        g.push_back((start, end, now));
        while g.len() > 4096 {
            g.pop_front();
        }
    }
}

/// Snapshot IBD-ready peer count (connected).
pub(crate) fn note_peers_conn(n: usize) {
    PEERS_CONN.store(n, Ordering::Relaxed);
}

/// Snapshot peers holding at least one in-flight assignment.
pub(crate) fn note_peers_inflight(n: usize) {
    PEERS_INFLIGHT.store(n, Ordering::Relaxed);
}

/// Live pipeline depth returned by `pipeline_depth_for_pressure`.
pub(crate) fn note_eff_depth(depth: usize) {
    EFF_DEPTH.store(depth, Ordering::Relaxed);
}

/// `memory_age_throttle_slow_pct()` at the same tick as `eff_depth`.
pub(crate) fn note_slow_pct(pct: u64) {
    SLOW_PCT.store(pct, Ordering::Relaxed);
}

/// Ms since the chunk covering `h` was last assigned, or -1 if unassigned.
pub(crate) fn assigned_ms_ago(h: u64) -> i64 {
    let Ok(g) = assigned_ranges().lock() else {
        return -1;
    };
    for (s, e, ms) in g.iter().rev() {
        if *s <= h && h <= *e {
            return wall_ms().saturating_sub(*ms) as i64;
        }
    }
    -1
}

/// First body for a non-tip height: assignment→body sojourn into bulk counters.
pub(crate) fn note_bulk_first_body(h: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut counted) = bulk_counted().lock() {
        if !counted.insert(h) {
            return;
        }
        if counted.len() > 768 {
            let lo = h.saturating_sub(512);
            counted.retain(|&k| k >= lo);
        }
    }
    let ago = assigned_ms_ago(h);
    if ago < 0 {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.bulk_n = s.cum.bulk_n.saturating_add(1);
        s.cum.bulk_gd_body_ms = s.cum.bulk_gd_body_ms.saturating_add(ago as u64);
    }
}

/// Begin wall tracking on the validation orchestrator thread.
pub(crate) fn arm() {
    if !enabled() {
        return;
    }
    ARMED.store(true, Ordering::Relaxed);
    WALL.with(|w| {
        let mut g = w.borrow_mut();
        g.state = WallState::Other;
        g.since = Instant::now();
        g.started = true;
        g.carry_us = [0; 6];
    });
    if let Ok(mut s) = shared().lock() {
        s.last_emit_at = Instant::now();
    }
}

fn flush_wall_locked(local: &mut WallLocal, cum: &mut Buckets) {
    if !local.started {
        return;
    }
    let now = Instant::now();
    let idx = wall_state_idx(local.state);
    let total_us = local.carry_us[idx]
        .saturating_add(now.saturating_duration_since(local.since).as_micros() as u64);
    cum.add_wall(local.state, total_us / 1000);
    local.carry_us[idx] = total_us % 1000;
    local.since = now;
}

/// Switch exclusive wall state (validation orchestrator only).
pub(crate) fn wall_enter(state: WallState) {
    if !ARMED.load(Ordering::Relaxed) || !enabled() {
        return;
    }
    WALL.with(|w| {
        let mut local = w.borrow_mut();
        if !local.started {
            return;
        }
        if local.state == state {
            return;
        }
        if let Ok(mut s) = shared().lock() {
            flush_wall_locked(&mut local, &mut s.cum);
        }
        local.state = state;
    });
}

/// Attribute the just-finished WaitFeeder interval to a binder class (call after wake).
pub(crate) fn note_wait_feeder_binder(binder: &str, wait_ms: u64) {
    if !ARMED.load(Ordering::Relaxed) || !enabled() || wait_ms == 0 {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.add_binder_wait(binder, wait_ms);
    }
}

/// Time inside `deserialize_block_with_witnesses` (inline or worker).
pub(crate) fn note_block_parse(ms: u64, offload: bool) {
    if !enabled() {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.parse_n = s.cum.parse_n.saturating_add(1);
        s.cum.parse_ms = s.cum.parse_ms.saturating_add(ms);
        if offload {
            s.cum.parse_offload_n = s.cum.parse_offload_n.saturating_add(1);
        } else {
            s.cum.parse_inline_n = s.cum.parse_inline_n.saturating_add(1);
        }
    }
}

/// R-347: one synchronous GAP_PERSIST in a download worker took `ms`.
pub(crate) fn note_gap_persist(ms: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.persist_n = s.cum.persist_n.saturating_add(1);
        s.cum.persist_ms = s.cum.persist_ms.saturating_add(ms);
        if ms > s.cum.persist_max_ms {
            s.cum.persist_max_ms = ms;
        }
    }
}

/// R-348: one persist-lane batch of `n` bodies took `ms` (one LMDB txn).
pub(crate) fn note_gap_persist_batch(n: u64, ms: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.persist_n = s.cum.persist_n.saturating_add(n);
        s.cum.persist_ms = s.cum.persist_ms.saturating_add(ms);
        if ms > s.cum.persist_max_ms {
            s.cum.persist_max_ms = ms;
        }
    }
}

/// Sample of the shared `peer_rx` channel depth (Finding 1 falsifier).
pub(crate) fn note_peer_rx_depth(depth: usize) {
    if !enabled() {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.peer_rx_depth_sum = s.cum.peer_rx_depth_sum.saturating_add(depth as u64);
        s.cum.peer_rx_depth_n = s.cum.peer_rx_depth_n.saturating_add(1);
        if (depth as u64) > s.cum.peer_rx_depth_max {
            s.cum.peer_rx_depth_max = depth as u64;
        }
    }
}

/// Tip-stage network tip finished validation — add supply latency segments (ms; -1 skipped).
pub(crate) fn note_tip_stage(
    need_body_ms: i64,
    gd_body_ms: i64,
    body_feeder_ms: i64,
    feeder_done_ms: i64,
) {
    if !enabled() {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.tip_n = s.cum.tip_n.saturating_add(1);
        if need_body_ms >= 0 {
            s.cum.tip_need_body_ms = s.cum.tip_need_body_ms.saturating_add(need_body_ms as u64);
        }
        if gd_body_ms >= 0 {
            s.cum.tip_gd_body_ms = s.cum.tip_gd_body_ms.saturating_add(gd_body_ms as u64);
        }
        if body_feeder_ms >= 0 {
            s.cum.tip_body_feeder_ms = s
                .cum
                .tip_body_feeder_ms
                .saturating_add(body_feeder_ms as u64);
        }
        if feeder_done_ms >= 0 {
            s.cum.tip_feeder_done_ms = s
                .cum
                .tip_feeder_done_ms
                .saturating_add(feeder_done_ms as u64);
        }
    }
}

/// One block's engine/validate worker times (may overlap wall).
pub(crate) fn note_engine(append_ms: u64, view_ms: u64, validate_ms: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.eng_n = s.cum.eng_n.saturating_add(1);
        s.cum.eng_append_ms = s.cum.eng_append_ms.saturating_add(append_ms);
        s.cum.eng_view_ms = s.cum.eng_view_ms.saturating_add(view_ms);
        s.cum.eng_validate_ms = s.cum.eng_validate_ms.saturating_add(validate_ms);
    }
}

/// R-359: worker MuHash fold time for one or more blocks (µs), noted by the orchestrator as
/// it folds subs in order (`eng_muhash_sum` in MS_BREAKDOWN).
pub(crate) fn note_engine_muhash_us(us: u64) {
    if !enabled() || us == 0 {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.eng_muhash_us = s.cum.eng_muhash_us.saturating_add(us);
    }
}

/// R-359: orchestrator dispatch split for one block (µs): serial txid SHA256d and the
/// per-block output cache build (`disp_txid_sum` / `disp_outcache_sum`).
pub(crate) fn note_dispatch_split_us(txid_us: u64, outcache_us: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.disp_txid_us = s.cum.disp_txid_us.saturating_add(txid_us);
        s.cum.disp_outcache_us = s.cum.disp_outcache_us.saturating_add(outcache_us);
    }
}

/// R-360: append-thread prep time for one block (µs): txid SHA + output cache, formerly on the
/// orchestrator (`eng_prep_sum`).
pub(crate) fn note_engine_prep_us(us: u64) {
    if !enabled() || us == 0 {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.eng_prep_us = s.cum.eng_prep_us.saturating_add(us);
    }
}

/// R-361: `ibd-drop` thread time for one block's deallocation (µs) (`eng_drop_sum`).
pub(crate) fn note_deferred_drop_us(us: u64) {
    if !enabled() || us == 0 {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.eng_drop_us = s.cum.eng_drop_us.saturating_add(us);
    }
}

/// R-362: orchestrator time inside the `[IBD_TIP_SKIP]` section for one block (µs)
/// (`drain_tipskip_sum`).
pub(crate) fn note_drain_tipskip_us(us: u64) {
    if !enabled() || us == 0 {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.drain_tipskip_us = s.cum.drain_tipskip_us.saturating_add(us);
    }
}

/// R-362: `ibd-tip-sync` thread time for one `Storage::flush()` (heed3 `force_sync`) (µs)
/// (`tipsync_bg_sum`) — the cost that left the orchestrator.
pub(crate) fn note_tip_sync_bg_us(us: u64) {
    if !enabled() || us == 0 {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.tipsync_bg_us = s.cum.tipsync_bg_us.saturating_add(us);
    }
}

/// R-362: live depth of the `ibd-drop` queue (blocks handed off, not yet freed). Sampled on
/// every MS_BREAKDOWN line as `drop_q=`; a value that grows across windows = dropper behind.
pub(crate) static DEFERRED_DROP_QUEUE: AtomicU64 = AtomicU64::new(0);

/// R-360: drain split for one block (µs): body-presence probe and the block-flush section
/// (`drain_skipchk_sum` / `drain_flush_sum`).
pub(crate) fn note_drain_split_us(skipchk_us: u64, flush_us: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        s.cum.drain_skipchk_us = s.cum.drain_skipchk_us.saturating_add(skipchk_us);
        s.cum.drain_flush_us = s.cum.drain_flush_us.saturating_add(flush_us);
    }
}

/// Collect phase: head already buffered vs needed a blocking recv.
/// `in_flight` / `pending` / `at_depth` are occupancy (worker-scheduling subsystem).
pub(crate) fn note_collect_outcome(ready: bool, in_flight: usize, pending: usize, at_depth: bool) {
    if !enabled() {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        if ready {
            s.cum.collect_ready_n = s.cum.collect_ready_n.saturating_add(1);
        } else {
            s.cum.collect_block_n = s.cum.collect_block_n.saturating_add(1);
            s.cum.pending_block_n = s.cum.pending_block_n.saturating_add(1);
            s.cum.pending_block_sum = s.cum.pending_block_sum.saturating_add(pending as u64);
            if pending == 0 {
                s.cum.pending0_block_n = s.cum.pending0_block_n.saturating_add(1);
            }
        }
        s.cum.collect_occ_n = s.cum.collect_occ_n.saturating_add(1);
        s.cum.inflight_sum = s.cum.inflight_sum.saturating_add(in_flight as u64);
        if (in_flight as u64) > s.cum.inflight_max {
            s.cum.inflight_max = in_flight as u64;
        }
        if at_depth {
            s.cum.inflight_full_n = s.cum.inflight_full_n.saturating_add(1);
        }
        match in_flight {
            0..=4 => s.cum.inflight_h0 = s.cum.inflight_h0.saturating_add(1),
            5..=8 => s.cum.inflight_h1 = s.cum.inflight_h1.saturating_add(1),
            9..=12 => s.cum.inflight_h2 = s.cum.inflight_h2.saturating_add(1),
            13..=16 => s.cum.inflight_h3 = s.cum.inflight_h3.saturating_add(1),
            _ => s.cum.inflight_h4 = s.cum.inflight_h4.saturating_add(1),
        }
    }
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * (part as f64) / (whole as f64)
    }
}

fn emit_line(tag: &str, w: &Buckets, h: u64) {
    let wall = w.wall_total().max(1);
    let tip_supply = w.tip_need_body_ms.max(w.tip_gd_body_ms); // need→body is the true tip wait; gd is subset-ish
    let (body_ia_avg, body_ia_n) = super::tip_stage::peek_body_ia_window();
    let body_ia_last = super::tip_stage::last_body_ia_ms();
    info!(
        "[IBD_MS_BREAKDOWN] {} h={} window_ms={} | wall_wait_feeder={}ms({:.1}%) dispatch={}ms({:.1}%) eng_append_wall={}ms({:.1}%) collect_wait={}ms({:.1}%) drain={}ms({:.1}%) other={}ms({:.1}%) | wait_binder tip_hole={}ms gd_slow={}ms empty_tip={}ms starve={}ms thin={}ms failover={}ms engine={}ms pressure={}ms hole_absent={}ms hole_staged={}ms | tip_n={} tip_need_body_sum={}ms tip_gd_body_sum={}ms tip_body_feeder_sum={}ms tip_feeder_done_sum={}ms tip_need_body_avg={:.1} | eng_n={} eng_append_sum={}ms eng_view_sum={}ms eng_validate_sum={}ms eng_validate_avg={:.1} collect_ready_n={} collect_block_n={} | inflight_avg={:.1} inflight_max={} inflight_full_n={} inflight_hist={}/{}/{}/{}/{} | block_pending_avg={:.1} block_pending0_n={} | tip_supply_vs_wall={:.1}% | tip_body_ia_last_ms={} tip_body_ia_win_avg={} tip_body_ia_n={} | bulk_n={} bulk_gd_body_sum={}ms bulk_gd_body_avg={:.1} | peers_conn={} peers_inflight={} | eff_depth={} slow_pct={} | eng_muhash_sum={}ms disp_txid_sum={}ms disp_outcache_sum={}ms | eng_prep_sum={}ms drain_skipchk_sum={}ms drain_flush_sum={}ms eng_drop_sum={}ms | drain_tipskip_sum={}ms tipsync_bg_sum={}ms drop_q={}",
        tag,
        h,
        wall,
        w.wait_feeder_ms,
        pct(w.wait_feeder_ms, wall),
        w.dispatch_ms,
        pct(w.dispatch_ms, wall),
        w.engine_append_wall_ms,
        pct(w.engine_append_wall_ms, wall),
        w.collect_wait_ms,
        pct(w.collect_wait_ms, wall),
        w.drain_ms,
        pct(w.drain_ms, wall),
        w.other_ms,
        pct(w.other_ms, wall),
        w.binder_tip_hole_ms,
        w.binder_gd_slow_ms,
        w.binder_empty_tip_ms,
        w.binder_feeder_starve_ms,
        w.binder_thin_runway_ms,
        w.binder_failover_ms,
        w.binder_engine_ms,
        w.binder_pressure_ms,
        w.binder_tip_hole_absent_ms,
        w.binder_tip_hole_staged_ms,
        w.tip_n,
        w.tip_need_body_ms,
        w.tip_gd_body_ms,
        w.tip_body_feeder_ms,
        w.tip_feeder_done_ms,
        if w.tip_n > 0 {
            w.tip_need_body_ms as f64 / w.tip_n as f64
        } else {
            0.0
        },
        w.eng_n,
        w.eng_append_ms,
        w.eng_view_ms,
        w.eng_validate_ms,
        if w.eng_n > 0 {
            w.eng_validate_ms as f64 / w.eng_n as f64
        } else {
            0.0
        },
        w.collect_ready_n,
        w.collect_block_n,
        if w.collect_occ_n > 0 {
            w.inflight_sum as f64 / w.collect_occ_n as f64
        } else {
            0.0
        },
        w.inflight_max,
        w.inflight_full_n,
        w.inflight_h0,
        w.inflight_h1,
        w.inflight_h2,
        w.inflight_h3,
        w.inflight_h4,
        if w.pending_block_n > 0 {
            w.pending_block_sum as f64 / w.pending_block_n as f64
        } else {
            0.0
        },
        w.pending0_block_n,
        pct(tip_supply, wall),
        body_ia_last,
        body_ia_avg,
        body_ia_n,
        w.bulk_n,
        w.bulk_gd_body_ms,
        if w.bulk_n > 0 {
            w.bulk_gd_body_ms as f64 / w.bulk_n as f64
        } else {
            0.0
        },
        PEERS_CONN.load(Ordering::Relaxed),
        PEERS_INFLIGHT.load(Ordering::Relaxed),
        EFF_DEPTH.load(Ordering::Relaxed),
        SLOW_PCT.load(Ordering::Relaxed),
        w.eng_muhash_us / 1000,
        w.disp_txid_us / 1000,
        w.disp_outcache_us / 1000,
        w.eng_prep_us / 1000,
        w.drain_skipchk_us / 1000,
        w.drain_flush_us / 1000,
        w.eng_drop_us / 1000,
        w.drain_tipskip_us / 1000,
        w.tipsync_bg_us / 1000,
        DEFERRED_DROP_QUEUE.load(Ordering::Relaxed),
    );
}

fn emit_hist(
    kind: &str,
    tag: &str,
    h: u64,
    n: &[u64; 10],
    ms: &[u64; 10],
    ge150_ms: u64,
    ge300_ms: u64,
    ge500_ms: u64,
) {
    let total_n: u64 = n.iter().copied().sum();
    let total_ms: u64 = ms.iter().copied().sum();
    let mut buckets = String::new();
    for i in 0..10 {
        if i > 0 {
            buckets.push_str(" | ");
        }
        buckets.push_str(&format!("{} n={} ms={}", HIST_LABELS[i], n[i], ms[i]));
    }
    info!(
        "[{}] {} h={} total_n={} total_ms={} ge150_ms={} ge300_ms={} ge500_ms={} | {}",
        kind, tag, h, total_n, total_ms, ge150_ms, ge300_ms, ge500_ms, buckets
    );
}

/// Emit window + cumulative lines if cadence elapsed (or `force`).
pub(crate) fn maybe_emit(h: u64, force: bool) {
    if !ARMED.load(Ordering::Relaxed) || !enabled() {
        return;
    }
    // Flush pending wall slice into cum before snapshot.
    WALL.with(|w| {
        let mut local = w.borrow_mut();
        if let Ok(mut s) = shared().lock() {
            flush_wall_locked(&mut local, &mut s.cum);
        }
    });

    let Ok(mut s) = shared().lock() else {
        return;
    };
    let due = force || s.last_emit_at.elapsed().as_secs() >= emit_secs();
    if !due {
        return;
    }

    let window = s.cum.sub_snapshot(&s.last_emit);
    emit_line("win", &window, h);
    emit_line("cum", &s.cum, h);
    emit_hist(
        "IBD_TIP_HOLE_HIST",
        "win",
        h,
        &window.tip_hole_hist_n,
        &window.tip_hole_hist_ms,
        window.tip_hole_ge150_ms,
        window.tip_hole_ge300_ms,
        window.tip_hole_ge500_ms,
    );
    emit_hist(
        "IBD_GD_SLOW_HIST",
        "win",
        h,
        &window.gd_slow_hist_n,
        &window.gd_slow_hist_ms,
        window.gd_slow_ge150_ms,
        window.gd_slow_ge300_ms,
        window.gd_slow_ge500_ms,
    );
    emit_hist(
        "IBD_TIP_HOLE_HIST",
        "cum",
        h,
        &s.cum.tip_hole_hist_n,
        &s.cum.tip_hole_hist_ms,
        s.cum.tip_hole_ge150_ms,
        s.cum.tip_hole_ge300_ms,
        s.cum.tip_hole_ge500_ms,
    );
    emit_hist(
        "IBD_GD_SLOW_HIST",
        "cum",
        h,
        &s.cum.gd_slow_hist_n,
        &s.cum.gd_slow_hist_ms,
        s.cum.gd_slow_ge150_ms,
        s.cum.gd_slow_ge300_ms,
        s.cum.gd_slow_ge500_ms,
    );
    emit_parse_lane("win", h, &window);
    emit_parse_lane("cum", h, &s.cum);
    super::body_dup::emit(h);
    super::wire_hash_gate::emit(h);
    s.last_emit = s.cum.clone();
    s.last_emit_at = Instant::now();
}

fn emit_parse_lane(tag: &str, h: u64, w: &Buckets) {
    info!(
        "[IBD_PARSE_LANE] {} h={} parse_n={} parse_ms={} offload_n={} inline_n={} hole_absent_ms={} hole_staged_ms={} persist_n={} persist_ms={} persist_max_ms={}",
        tag,
        h,
        w.parse_n,
        w.parse_ms,
        w.parse_offload_n,
        w.parse_inline_n,
        w.binder_tip_hole_absent_ms,
        w.binder_tip_hole_staged_ms,
        w.persist_n,
        w.persist_ms,
        w.persist_max_ms,
    );
    let (batches, blocks, ms, max_ms, max_depth, fallback, depth) = super::persist_lane::stats();
    if batches > 0 || fallback > 0 {
        info!(
            "[IBD_PERSIST_LANE] {} h={} cum_batches={} cum_blocks={} cum_ms={} max_ms={} win_max_depth={} cum_fallback={} depth={}",
            tag, h, batches, blocks, ms, max_ms, max_depth, fallback, depth
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_buckets_add_and_window_delta() {
        let mut a = Buckets::default();
        a.add_wall(WallState::WaitFeeder, 100);
        a.add_wall(WallState::Drain, 50);
        a.add_binder_wait("SUPPLY_EMPTY_TIP", 80);
        assert_eq!(a.wall_total(), 150);
        assert_eq!(a.binder_empty_tip_ms, 80);
        let mut b = a.clone();
        b.add_wall(WallState::WaitFeeder, 20);
        let d = b.sub_snapshot(&a);
        assert_eq!(d.wait_feeder_ms, 20);
        assert_eq!(d.drain_ms, 0);
        assert_eq!(d.bulk_n, 0);
    }

    /// R-363: sub-ms slices must not be truncated away — the per-state remainder carries, so
    /// Σ buckets + Σ carry equals the measured wall to the microsecond.
    #[test]
    fn r363_wall_slices_carry_sub_ms_remainder() {
        let mut local = WallLocal {
            state: WallState::Dispatch,
            since: Instant::now(),
            started: true,
            carry_us: [0; 6],
        };
        let mut cum = Buckets::default();
        let t0 = local.since;
        // 40 slices of ~300 µs alternating Dispatch / Drain: the old `as_millis()` path read
        // 0 for every one of them.
        for i in 0..40 {
            std::thread::sleep(std::time::Duration::from_micros(300));
            flush_wall_locked(&mut local, &mut cum);
            local.state = if i % 2 == 0 {
                WallState::Drain
            } else {
                WallState::Dispatch
            };
        }
        let measured_us = local.since.duration_since(t0).as_micros() as u64;
        let accounted_us = cum.wall_total() * 1000 + local.carry_us.iter().sum::<u64>();
        assert!(measured_us >= 12_000, "40 × 300 µs slept: {measured_us}");
        // `as_micros()` still drops < 1 µs per slice; 40 slices → ≤ 40 µs (was ≤ 40 ms).
        assert!(
            measured_us - accounted_us <= 40,
            "buckets + carry must equal the wall to ≤ 1 µs per slice: {accounted_us} vs {measured_us}"
        );
        assert!(
            cum.wall_total() >= 12,
            "≥ 12 ms must land in buckets (got {})",
            cum.wall_total()
        );
        assert!(local.carry_us.iter().all(|&c| c < 1000));
    }

    #[test]
    fn assigned_ms_ago_unassigned_is_minus_one() {
        assert_eq!(assigned_ms_ago(u64::MAX - 7), -1);
    }

    #[test]
    fn assigned_covering_last_wins() {
        test_reset_assigned();
        note_assigned(100, 115);
        let ago = assigned_ms_ago(108);
        assert!(ago >= 0, "covering range must resolve");
        note_assigned(200, 215);
        assert_eq!(assigned_ms_ago(108) >= 0, true);
        assert!(assigned_ms_ago(205) >= 0);
        assert_eq!(assigned_ms_ago(50), -1);
    }

    #[test]
    fn r301_tip_hole_hist_puts_seconds_in_summed_ms_not_event_count() {
        // R-301: tip_hole wall is 942s on R-298. Event count alone cannot set
        // HEDGE_MS — a thousand 9ms waits are 9s, one 2001ms wait is the second.
        let mut a = Buckets::default();
        a.add_binder_wait("SUPPLY_TIP_HOLE", 9);
        a.add_binder_wait("SUPPLY_TIP_HOLE", 10);
        a.add_binder_wait("SUPPLY_TIP_HOLE", 149);
        a.add_binder_wait("SUPPLY_TIP_HOLE", 150);
        a.add_binder_wait("SUPPLY_TIP_HOLE", 299);
        a.add_binder_wait("SUPPLY_TIP_HOLE", 300);
        a.add_binder_wait("SUPPLY_TIP_HOLE", 500);
        a.add_binder_wait("SUPPLY_TIP_HOLE", 2001);
        a.add_binder_wait("SUPPLY_GD_SLOW", 400);
        assert_eq!(a.tip_hole_hist_n[0], 1, "<10 count");
        assert_eq!(a.tip_hole_hist_n[1], 1, "10-25 count (10ms inclusive)");
        assert_eq!(a.tip_hole_hist_n[4], 2, "100-200 holds 149 and 150");
        assert_eq!(a.tip_hole_hist_n[5], 1, "200-300 holds 299");
        assert_eq!(a.tip_hole_hist_n[6], 1, "300-500 holds 300");
        assert_eq!(a.tip_hole_hist_n[7], 1, "500-1000 holds 500");
        assert_eq!(a.tip_hole_hist_n[9], 1, ">2000 holds 2001");
        assert_eq!(
            a.tip_hole_hist_ms[9], 2001,
            "R-301: the 2001ms event must dominate its bucket's summed-ms (not share it with count)"
        );
        assert_eq!(
            a.binder_tip_hole_ms,
            9 + 10 + 149 + 150 + 299 + 300 + 500 + 2001,
            "hist ms sum must equal wait_binder tip_hole"
        );
        assert_eq!(
            a.tip_hole_ge150_ms,
            150 + 299 + 300 + 500 + 2001,
            "ge150 is exact (149 stays out; 100-200 straddles so we cannot derive this from buckets)"
        );
        assert_eq!(a.tip_hole_ge300_ms, 300 + 500 + 2001);
        assert_eq!(a.tip_hole_ge500_ms, 500 + 2001);
        assert_eq!(a.gd_slow_hist_n[6], 1, "gd_slow 400ms → 300-500");
        assert_eq!(a.gd_slow_ge150_ms, 400);
        let mut b = a.clone();
        b.add_binder_wait("SUPPLY_TIP_HOLE", 12);
        let d = b.sub_snapshot(&a);
        assert_eq!(
            d.tip_hole_hist_n[1], 1,
            "window delta counts only the new 12ms"
        );
        assert_eq!(d.tip_hole_hist_ms[1], 12);
        assert_eq!(
            d.tip_hole_hist_n[9], 0,
            "window must not carry the 2001ms event"
        );
        assert_eq!(
            d.tip_hole_ge150_ms, 0,
            "12ms is below 150; window ge150 stays 0"
        );
    }
}
