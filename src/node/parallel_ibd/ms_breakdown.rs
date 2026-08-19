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
//! Opt in: `BLVM_IBD_MS_BREAKDOWN=1` (or `true`/`on`). Default **off**.
//! Soak harness (`wan-bench-common.sh`) forces `=1`. Emit cadence:
//! `BLVM_IBD_MS_BREAKDOWN_SECS` (default **2**).

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
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
        matches!(
            std::env::var("BLVM_IBD_MS_BREAKDOWN")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("1") | Some("true") | Some("on") | Some("yes")
        )
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
    /// Collect entered with head already in `pending_results` (no blocking recv).
    collect_ready_n: u64,
    /// Collect had to block on `valres_rx` for the head height.
    collect_block_n: u64,
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
            "SUPPLY_TIP_HOLE" => {
                self.binder_tip_hole_ms = self.binder_tip_hole_ms.saturating_add(ms)
            }
            "SUPPLY_GD_SLOW" => self.binder_gd_slow_ms = self.binder_gd_slow_ms.saturating_add(ms),
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
            collect_ready_n: self.collect_ready_n.saturating_sub(prev.collect_ready_n),
            collect_block_n: self.collect_block_n.saturating_sub(prev.collect_block_n),
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
}

thread_local! {
    static WALL: RefCell<WallLocal> = RefCell::new(WallLocal {
        state: WallState::Other,
        since: Instant::now(),
        started: false,
    });
}

static ARMED: AtomicBool = AtomicBool::new(false);

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
    let ms = now.saturating_duration_since(local.since).as_millis() as u64;
    cum.add_wall(local.state, ms);
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

/// Collect phase: head already buffered vs needed a blocking recv.
pub(crate) fn note_collect_outcome(ready: bool) {
    if !enabled() {
        return;
    }
    if let Ok(mut s) = shared().lock() {
        if ready {
            s.cum.collect_ready_n = s.cum.collect_ready_n.saturating_add(1);
        } else {
            s.cum.collect_block_n = s.cum.collect_block_n.saturating_add(1);
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
    info!(
        "[IBD_MS_BREAKDOWN] {} h={} window_ms={} | wall_wait_feeder={}ms({:.1}%) dispatch={}ms({:.1}%) eng_append_wall={}ms({:.1}%) collect_wait={}ms({:.1}%) drain={}ms({:.1}%) other={}ms({:.1}%) | wait_binder tip_hole={}ms gd_slow={}ms empty_tip={}ms starve={}ms thin={}ms failover={}ms engine={}ms pressure={}ms | tip_n={} tip_need_body_sum={}ms tip_gd_body_sum={}ms tip_body_feeder_sum={}ms tip_feeder_done_sum={}ms tip_need_body_avg={:.1} | eng_n={} eng_append_sum={}ms eng_view_sum={}ms eng_validate_sum={}ms eng_validate_avg={:.1} collect_ready_n={} collect_block_n={} | tip_supply_vs_wall={:.1}%",
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
        pct(tip_supply, wall),
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
    s.last_emit = s.cum.clone();
    s.last_emit_at = Instant::now();
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
    }
}
