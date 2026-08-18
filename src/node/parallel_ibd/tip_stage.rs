//! Per-height tip-stage wall-time diagnostics for WAN crawl.
//!
//! Tracks when the validation tip height (`next_needed`) is:
//! needed → first GetData → body received → reorder → feeder → validated.
//!
//! One `[IBD_TIP_STAGE]` line is emitted when validation consumes the height
//! (only if a network GetData was observed, or a soft-retry fired).
//!
//! ## W68 — pre-roll GetData / body credit
//!
//! Deep tip pipes `GetData` heights before they become `next_needed`. Historically
//! [`mark_getdata`] / [`mark_body`] no-op'd when `HEIGHT` was still behind, so on tip
//! roll `GETDATA_MS` stayed 0. [`tip_ahead_frozen_for_late_body`] then keyed off
//! `mark_needed` age and opened covering=2 at exactly **2s** → `(H,H)` failover
//! treadmill (`need→getdata≈2000` on ~58% of TIP_STAGE samples, live 2026-07-17
//! post-W67 soft-resume ~239k–252k). Credit pre-roll stamps from a recent map when
//! the tip height becomes tracked.
//!
//! ## W81 — freeze clock ignores pre-roll GetData age
//!
//! W77 keys freeze off GetData when `gap_missing`. Combined with W68, a 3–11s pipe
//! lead made every tip roll look already late → sticky dual / W35 ahead stayed frozen
//! while tip micro-advanced. Freeze trusts only `getdata >= needed` (post-roll).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use tracing::{debug, warn};

/// Recent GetData / body wall-ms by height (survives tip-tracker roll).
struct RecentStamps {
    getdata: HashMap<u64, u64>,
    body: HashMap<u64, u64>,
}

fn recent_stamps() -> &'static Mutex<RecentStamps> {
    static R: OnceLock<Mutex<RecentStamps>> = OnceLock::new();
    R.get_or_init(|| {
        Mutex::new(RecentStamps {
            getdata: HashMap::new(),
            body: HashMap::new(),
        })
    })
}

fn note_recent(map: &mut HashMap<u64, u64>, h: u64, ms: u64) {
    map.entry(h).or_insert(ms);
    // Bound memory: drop far-behind / far-ahead keys when the map grows.
    if map.len() > 768 {
        let tip = HEIGHT.load(Ordering::Relaxed);
        let lo = tip.saturating_sub(64);
        let hi = tip.saturating_add(512);
        map.retain(|&k, _| k >= lo && k <= hi);
        if map.len() > 768 {
            // Still oversized (tip=0 cold start) — keep highest keys.
            let mut keys: Vec<u64> = map.keys().copied().collect();
            keys.sort_unstable();
            for k in keys.into_iter().take(map.len().saturating_sub(512)) {
                map.remove(&k);
            }
        }
    }
}

/// W68: apply pre-roll GetData/body stamps for newly tracked tip `h`.
fn credit_pre_roll_stamps(h: u64) {
    let Ok(guard) = recent_stamps().lock() else {
        return;
    };
    if let Some(&ms) = guard.getdata.get(&h) {
        let _ = GETDATA_MS.compare_exchange(0, ms, Ordering::Relaxed, Ordering::Relaxed);
    }
    if let Some(&ms) = guard.body.get(&h) {
        if BODY_MS
            .compare_exchange(0, ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            AHEAD_FREEZE_SOFT.store(false, Ordering::Relaxed);
            TIP_FAILOVER_ARMED.store(false, Ordering::Relaxed);
        }
    }
}

static HEIGHT: AtomicU64 = AtomicU64::new(0);
/// Highest height removed from the feeder into validation (monotonic).
///
/// Set under the feeder lock **before** unlock so the coordinator cannot observe
/// "tip absent from feeder" with `tip_taken_by_validation == false` during the
/// gap before [`finish_validated`] advances [`HEIGHT`]. Live genesis W58: REWIND→PRUNE
/// ≤500ms on **98%** of rewinds (~1k/10k heights) collapsed early BPS to ~40–100.
static TAKEN_THROUGH: AtomicU64 = AtomicU64::new(0);
static NEEDED_MS: AtomicU64 = AtomicU64::new(0);
static GETDATA_MS: AtomicU64 = AtomicU64::new(0);
static BODY_MS: AtomicU64 = AtomicU64::new(0);
static REORDER_MS: AtomicU64 = AtomicU64::new(0);
static FEEDER_MS: AtomicU64 = AtomicU64::new(0);
static SOFT_RETRIES: AtomicU64 = AtomicU64::new(0);
/// Rolling getdata→body ms (α≈1/8). Used by A6m when tip-advance BPS is LOCAL_GAP-inflated
/// (E11: wall~64 / need→body p50=24ms while getdata→body p50~1.3s → A6M never armed).
static GETDATA_BODY_EWMA_MS: AtomicU64 = AtomicU64::new(0);
static GETDATA_BODY_EWMA_N: AtomicU64 = AtomicU64::new(0);

/// Coordinator `live_body_tip` mirror — validation is in tip-crawl when `HEIGHT >` this.
static WAN_BODY_TIP: AtomicU64 = AtomicU64::new(0);
/// Wall-ms of last `[IBD_TIP_LOCAL_STREAM]` (disk fill at tip).
static LOCAL_STREAM_LAST_MS: AtomicU64 = AtomicU64::new(0);

/// Dens-class getdata→body EWMA (ms) — A5 archive tip crawl sits ~30–50 ms.
const TIP_CRAWL_GD_HEALTHY_MS: u64 = 150;
/// Fresh LOCAL_STREAM window for supply-healthy (ms).
const TIP_CRAWL_STREAM_FRESH_MS: u64 = 2_000;
/// Consecutive tip-band `[IBD_PIPE_FILL] received=0` observations (A6m mute fast-path).
static PIPE_FILL_RECV0_STREAK: AtomicU64 = AtomicU64::new(0);
/// Freeze multi-peer tip-band ahead while tip soft-retry is in flight.
/// Set even when `mark_soft_retry` height is not tip_stage-tracked (live: ahead kept
/// assigning 3–12s after `[IBD_GAP_SOFT_RETRY]` because SOFT_RETRIES stayed 0).
static AHEAD_FREEZE_SOFT: AtomicBool = AtomicBool::new(false);
/// W28c: tip soft-retry arms a temporary second tip owner (failover race), not a standing N-way tip lottery.
static TIP_FAILOVER_ARMED: AtomicBool = AtomicBool::new(false);
/// W29: tip-SLA rotate fires at most once per tracked height.
static TIP_SLA_FIRED: AtomicBool = AtomicBool::new(false);
/// W178: wall-ms deadline — after soft-resume leaves local body ahead, tip peer scores
/// are cold (0.001). Unproven SLA=5s then thrash-rotates before getdata→body can land
/// (live W177: freeze @403622, TIP_SLA every 5s, high_ev≈100k). Grace uses floor SLA.
static POST_LOCAL_AHEAD_GRACE_UNTIL_MS: AtomicU64 = AtomicU64::new(0);

/// M1 past-body duty: wall-clock ms spent at tip-hole grown depth (not PIPE_FILL entry).
/// Buckets: 0=≤8, 1=9–16, 2=17–24, 3=25–32, 4=33–48, 5=>48.
static DUTY_DEPTH: AtomicUsize = AtomicUsize::new(0);
static DUTY_LAST_MS: AtomicU64 = AtomicU64::new(0);
static DUTY_MS_B0: AtomicU64 = AtomicU64::new(0);
static DUTY_MS_B1: AtomicU64 = AtomicU64::new(0);
static DUTY_MS_B2: AtomicU64 = AtomicU64::new(0);
static DUTY_MS_B3: AtomicU64 = AtomicU64::new(0);
static DUTY_MS_B4: AtomicU64 = AtomicU64::new(0);
static DUTY_MS_B5: AtomicU64 = AtomicU64::new(0);
static DUTY_TIP_LAST_MS: AtomicU64 = AtomicU64::new(0);
static DUTY_IA_SUM_MS: AtomicU64 = AtomicU64::new(0);
static DUTY_IA_N: AtomicU64 = AtomicU64::new(0);
static DUTY_LOG_LAST_MS: AtomicU64 = AtomicU64::new(0);
/// Sole GD_SLOW floor latch: after sole floor clamp, block deepen until EWMA
/// leaves the slow band (`SOLE_FLOOR_RECOVER_MS`, default = gd-slow gate).
static SOLE_FLOOR_LATCHED: AtomicBool = AtomicBool::new(false);
/// Sole no-FAST latch: after sole GD_SLOW, forbid FAST_CAP until getdata EWMA is
/// *sustainably* below the gd-fast gate (streak). Signal-only (no height). Cold
/// CAP may deepen once the floor clears; only FAST is held.
static SOLE_NO_FAST_LATCHED: AtomicBool = AtomicBool::new(false);
/// Consecutive tip-hole checks with EWMA &lt; gd-fast while no-FAST is armed.
/// A single lucky blip must not re-arm FAST (tc152: floor→grow64→cheese).
static SOLE_NO_FAST_CLEAR_STREAK: AtomicU32 = AtomicU32::new(0);
/// Wall-ms when no-FAST was last armed (0 = unset). Clear requires min hold + streak.
static SOLE_NO_FAST_ARMED_MS: AtomicU64 = AtomicU64::new(0);

/// Arm sole-floor grow lock (call when sole GD_SLOW floor clamps depth).
pub(crate) fn note_sole_floor_latch() {
    SOLE_FLOOR_LATCHED.store(true, Ordering::Relaxed);
}

/// Clear sole-floor grow lock (healthy EWMA recovery).
pub(crate) fn clear_sole_floor_latch() {
    SOLE_FLOOR_LATCHED.store(false, Ordering::Relaxed);
}

pub(crate) fn sole_floor_latched() -> bool {
    SOLE_FLOOR_LATCHED.load(Ordering::Relaxed)
}

/// Arm sole no-FAST (cold grow_cap only) until getdata is sustainably fast again.
pub(crate) fn note_sole_no_fast_latch() {
    SOLE_NO_FAST_LATCHED.store(true, Ordering::Relaxed);
    SOLE_NO_FAST_CLEAR_STREAK.store(0, Ordering::Relaxed);
    SOLE_NO_FAST_ARMED_MS.store(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        Ordering::Relaxed,
    );
}

pub(crate) fn clear_sole_no_fast_latch() {
    SOLE_NO_FAST_LATCHED.store(false, Ordering::Relaxed);
    SOLE_NO_FAST_CLEAR_STREAK.store(0, Ordering::Relaxed);
    SOLE_NO_FAST_ARMED_MS.store(0, Ordering::Relaxed);
}

pub(crate) fn sole_no_fast_latched() -> bool {
    SOLE_NO_FAST_LATCHED.load(Ordering::Relaxed)
}

/// Wall-ms since no-FAST was armed (None if unset / not latched).
pub(crate) fn sole_no_fast_armed_age_ms() -> Option<u64> {
    let armed = SOLE_NO_FAST_ARMED_MS.load(Ordering::Relaxed);
    if armed == 0 {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(armed);
    Some(now.saturating_sub(armed))
}

/// Record one tip-hole check while no-FAST is armed.
/// Returns new streak when `healthy` (EWMA &lt; gd-fast); resets to 0 otherwise.
pub(crate) fn sole_no_fast_note_clear_sample(healthy: bool) -> u32 {
    if !healthy {
        SOLE_NO_FAST_CLEAR_STREAK.store(0, Ordering::Relaxed);
        return 0;
    }
    SOLE_NO_FAST_CLEAR_STREAK.fetch_add(1, Ordering::Relaxed) + 1
}

#[cfg(test)]
pub(crate) fn test_reset_sole_floor_latch() {
    SOLE_FLOOR_LATCHED.store(false, Ordering::Relaxed);
    SOLE_NO_FAST_LATCHED.store(false, Ordering::Relaxed);
    SOLE_NO_FAST_CLEAR_STREAK.store(0, Ordering::Relaxed);
    SOLE_NO_FAST_ARMED_MS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn test_set_sole_no_fast_armed_ms_ago(ago_ms: u64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    SOLE_NO_FAST_ARMED_MS.store(now.saturating_sub(ago_ms), Ordering::Relaxed);
}

fn duty_bucket(depth: usize) -> usize {
    match depth {
        0..=8 => 0,
        9..=16 => 1,
        17..=24 => 2,
        25..=32 => 3,
        33..=48 => 4,
        _ => 5,
    }
}

fn duty_bucket_atomic(b: usize) -> &'static AtomicU64 {
    match b {
        0 => &DUTY_MS_B0,
        1 => &DUTY_MS_B1,
        2 => &DUTY_MS_B2,
        3 => &DUTY_MS_B3,
        4 => &DUTY_MS_B4,
        _ => &DUTY_MS_B5,
    }
}

fn duty_flush_elapsed(now: u64) {
    let last = DUTY_LAST_MS.load(Ordering::Relaxed);
    if last == 0 || now <= last {
        DUTY_LAST_MS.store(now, Ordering::Relaxed);
        return;
    }
    let dt = now.saturating_sub(last).min(60_000);
    let b = duty_bucket(DUTY_DEPTH.load(Ordering::Relaxed));
    duty_bucket_atomic(b).fetch_add(dt, Ordering::Relaxed);
    DUTY_LAST_MS.store(now, Ordering::Relaxed);
}

/// Record live tip-hole grown depth for wall-clock duty (M1).
pub(crate) fn note_tip_hole_duty(depth: usize) {
    let now = wall_ms();
    duty_flush_elapsed(now);
    DUTY_DEPTH.store(depth, Ordering::Relaxed);
    maybe_log_tip_hole_duty(now);
}

/// Tip height advanced — sample inter-arrival for duty log.
pub(crate) fn note_tip_duty_advance() {
    let now = wall_ms();
    duty_flush_elapsed(now);
    let prev = DUTY_TIP_LAST_MS.swap(now, Ordering::Relaxed);
    if prev > 0 && now > prev {
        let ia = now.saturating_sub(prev).min(60_000);
        DUTY_IA_SUM_MS.fetch_add(ia, Ordering::Relaxed);
        DUTY_IA_N.fetch_add(1, Ordering::Relaxed);
    }
    maybe_log_tip_hole_duty(now);
}

fn maybe_log_tip_hole_duty(now: u64) {
    let last = DUTY_LOG_LAST_MS.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < 5_000 {
        return;
    }
    if DUTY_LOG_LAST_MS
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    duty_flush_elapsed(now);
    let b0 = DUTY_MS_B0.load(Ordering::Relaxed);
    let b1 = DUTY_MS_B1.load(Ordering::Relaxed);
    let b2 = DUTY_MS_B2.load(Ordering::Relaxed);
    let b3 = DUTY_MS_B3.load(Ordering::Relaxed);
    let b4 = DUTY_MS_B4.load(Ordering::Relaxed);
    let b5 = DUTY_MS_B5.load(Ordering::Relaxed);
    let total = b0
        .saturating_add(b1)
        .saturating_add(b2)
        .saturating_add(b3)
        .saturating_add(b4)
        .saturating_add(b5);
    if total < 1_000 {
        return;
    }
    let pct = |ms: u64| -> f64 { 100.0 * (ms as f64) / (total as f64) };
    let ia_n = DUTY_IA_N.load(Ordering::Relaxed);
    let ia_avg = if ia_n > 0 {
        DUTY_IA_SUM_MS.load(Ordering::Relaxed) / ia_n
    } else {
        0
    };
    debug!(
        "[IBD_TIP_HOLE_DUTY] depth={} ms_total={} frac8={:.1}% frac16={:.1}% frac24={:.1}% frac32={:.1}% frac48={:.1}% frac_gt48={:.1}% tip_ia_avg_ms={} tip_ia_n={}",
        DUTY_DEPTH.load(Ordering::Relaxed),
        total,
        pct(b0),
        pct(b1),
        pct(b2),
        pct(b3),
        pct(b4),
        pct(b5),
        ia_avg,
        ia_n
    );
}

/// Arm tip failover so ChunkAssigner may allow covering=2 until the tip arrives.
pub(crate) fn arm_tip_failover() {
    TIP_FAILOVER_ARMED.store(true, Ordering::Relaxed);
}

#[inline]
pub(crate) fn tip_failover_armed() -> bool {
    TIP_FAILOVER_ARMED.load(Ordering::Relaxed)
}

/// In-flight tip soft-retry count (0 when tip body is flowing).
#[inline]
pub(crate) fn tip_soft_retries() -> u64 {
    SOFT_RETRIES.load(Ordering::Relaxed)
}

/// True while tip soft-retry should suppress multi-peer ahead (sticky latch).
#[inline]
pub(crate) fn tip_ahead_frozen_for_soft_retry() -> bool {
    AHEAD_FREEZE_SOFT.load(Ordering::Relaxed) || SOFT_RETRIES.load(Ordering::Relaxed) > 0
}

/// W42/W81: seconds waiting on tracked tip body (GetData stamp, else `mark_needed`).
///
/// Prefer **post-roll** GetData (`getdata >= needed`); fall back to needed so
/// covering=1 zombies without a GetData stamp still freeze ahead (live tip=330337:
/// covering=1, holes=0, 35s stall).
///
/// **W81:** W68 credits pre-roll GetData with `getdata < needed` (pipe lead often
/// 3–11s). Using that stamp for the freeze clock made every tip roll look late under
/// W77 `gap_missing` → sticky dual-pipe / multi-peer ahead stayed frozen while tip
/// micro-advanced (~30–40 tip60). Pre-roll GetData ages from `needed` instead.
#[inline]
pub(crate) fn tip_awaiting_body_secs() -> u64 {
    tip_awaiting_body_ms() / 1000
}

#[inline]
fn tip_awaiting_body_ms() -> u64 {
    let body = BODY_MS.load(Ordering::Relaxed);
    if body != 0 {
        return 0;
    }
    tip_awaiting_ms_from_stamps()
}

/// W89b: tip-wait age for short tip-hole CAP — same clock as late-body freeze when
/// `gap_missing` (ignores BODY_MS). Short CAP arms only after a real wait (≥ trigger),
/// not on every standing ahead-OOO hole sample.
#[inline]
pub(crate) fn tip_awaiting_secs_for_cap() -> u64 {
    tip_awaiting_ms_for_cap() / 1000
}

/// C1t: same tip-wait clock as [`tip_awaiting_secs_for_cap`], in milliseconds.
/// Integer seconds hide sub-second mid-gap stalls (good-day gd≈100 ms) where
/// soft-retry / late-body freeze (2 s) never arm → covering stuck at 1.
#[inline]
pub(crate) fn tip_awaiting_ms_for_cap() -> u64 {
    if crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed) {
        tip_awaiting_ms_ignore_body()
    } else {
        tip_awaiting_body_ms()
    }
}

/// W42/W77: freeze multi-peer ahead while tip body is late (default **2s**).
/// Live 2026-07-16: tip=330337 stuck ~32s with covering=1 / gap_missing while ahead
/// kept assigning 330625+ (holes=0) — bandwidth stolen from the tip gap.
///
/// **W77:** [`mark_body`] runs in the download worker when peer bytes arrive, *before*
/// the coordinator admits the tip into reorder/bridge. Live W76b ~370k:
/// `GAP_STREAM_RESEND`≈1706 vs first STREAM≈708, and **259/259** W35 ahead assigns had
/// `gap_missing=true` — `BODY_MS` cleared the freeze while tip never stuck. When
/// [`super::IBD_TIP_GAP_MISSING`] is set, ignore `BODY_MS` and key off GetData/needed age.
///
/// **W81:** see [`tip_awaiting_body_secs`] — pre-roll GetData must not start the clock.
///
/// **W96/W97 reverted:** freeze **2→5s** and needed-only clocks both hurt tip60
/// (W96 freeze steal; W97 tip60~15 vs W95 ~80–112). Keep W81 GetData-aware clock.
#[inline]
pub(crate) fn tip_ahead_frozen_for_late_body() -> bool {
    let freeze_secs = super::latch_env!(u64, {
        std::env::var("BLVM_IBD_TIP_AHEAD_FREEZE_BODY_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2)
            .clamp(1, 30)
    });
    let awaiting = if crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed) {
        tip_awaiting_secs_ignore_body()
    } else {
        tip_awaiting_body_secs()
    };
    awaiting >= freeze_secs
}

/// Seconds since post-roll GetData (else needed) for the tracked tip — ignores [`BODY_MS`].
#[inline]
fn tip_awaiting_secs_ignore_body() -> u64 {
    tip_awaiting_ms_ignore_body() / 1000
}

#[inline]
fn tip_awaiting_ms_ignore_body() -> u64 {
    tip_awaiting_ms_from_stamps()
}

/// Shared tip-wait clock (ms): post-roll GetData if present, else `mark_needed`.
#[inline]
fn tip_awaiting_ms_from_stamps() -> u64 {
    let getdata = GETDATA_MS.load(Ordering::Relaxed);
    let needed = NEEDED_MS.load(Ordering::Relaxed);
    // W81: only trust GetData stamped at/after tip became needed. Pre-roll credit
    // (getdata < needed) is pipe lead, not tip distress.
    let start = if getdata != 0 && needed != 0 && getdata >= needed {
        getdata
    } else if needed != 0 {
        needed
    } else if getdata != 0 {
        getdata
    } else {
        return 0;
    };
    wall_ms().saturating_sub(start)
}

#[inline]
pub(crate) fn clear_tip_ahead_soft_freeze() {
    AHEAD_FREEZE_SOFT.store(false, Ordering::Relaxed);
}

#[inline]
pub(crate) fn clear_tip_failover() {
    TIP_FAILOVER_ARMED.store(false, Ordering::Relaxed);
}

/// W29/A6g tip-SLA: seconds without tip body before rotating sticky owner (default **90**).
///
/// Must sit **above** tip-gap soft-retry budget (`tip_gap_timeout × soft_retries` ≈ 10×3=30s).
/// Live A6f: SLA=35s aborted owners while `getdata→body` p90≈27s / max≈57s and soft-retries
/// were still in flight → tip stuck ≥20s for ~30% of wall, ~2.9 blk/s. Breakthrough peers
/// finish in ~6s so a longer SLA does not slow the fast path.
pub(crate) fn tip_sla_secs() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_TIP_SLA_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(90)
            .clamp(45, 180)
    })
}

/// Tip-SLA for floor-score sticky owners (≈0.1). Default **15** (W82) — was 25; live
/// mid-chain WAN tip=347948 spent full floor SLA on score=0.001 owners while bridge_min
/// sat 20 ahead (tip60≈1). Still above tip CAP (12s) so soft-retry can fire once.
/// Env `BLVM_IBD_TIP_SLA_FLOOR_SECS` (clamp 12–base).
pub(crate) fn tip_sla_floor_secs() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let base = tip_sla_secs();
        std::env::var("BLVM_IBD_TIP_SLA_FLOOR_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15)
            .clamp(12, base)
    })
}

/// W165: tip-SLA for demoted/unproven sticky (tip_owner_score ≤0.05). Default **5**.
/// Live W164 @334087: demoted sticky score=0.001 held full floor SLA **15s** while tip60
/// collapsed (331→334k rate-fail). Rotate before CAP soft-retry window burns tip60.
/// Env `BLVM_IBD_TIP_SLA_UNPROVEN_SECS` (clamp 3–floor).
pub(crate) fn tip_sla_unproven_secs() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let floor = tip_sla_floor_secs();
        std::env::var("BLVM_IBD_TIP_SLA_UNPROVEN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5)
            .clamp(3, floor)
    })
}

/// W170: owner scores at/below this use [`tip_sla_floor_secs`] (default **0.20**).
/// Live W169 @317772: sticky score=**0.148** held mid-tier SLA **90s** (old ≤0.12 floor)
/// while holes=1 pending=61 covering=3 soft_retries=0 → tip60→0 → rate-fail @321k.
/// Env `BLVM_IBD_TIP_SLA_FLOOR_SCORE` (clamp 0.12–0.35).
pub(crate) fn tip_sla_floor_score_threshold() -> f64 {
    static CACHED: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_TIP_SLA_FLOOR_SCORE")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.20_f64)
            .clamp(0.12_f64, 0.35_f64)
    })
}

/// True when `owner_score` should use floor tip-SLA (not the 90s mid-tier).
pub(crate) fn tip_owner_uses_floor_sla(owner_score: f64) -> bool {
    owner_score <= tip_sla_floor_score_threshold()
}

/// W178/W179: arm after `IBD_LOCAL_BODY_AHEAD` clears (local inject → WAN tip).
/// Default **180s** (was 90) — tip peer scores stay ~0.001 for minutes after rewind;
/// wall-clock grace must outlast early warm-up, not just the first tip body.
pub(crate) fn arm_post_local_ahead_grace() {
    let secs: u64 = std::env::var("BLVM_IBD_POST_LOCAL_AHEAD_GRACE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(180)
        .clamp(60, 300);
    let until = wall_ms().saturating_add(secs.saturating_mul(1000));
    POST_LOCAL_AHEAD_GRACE_UNTIL_MS.store(until, Ordering::Relaxed);
}

/// True while post-local-ahead tip grace is active.
#[inline]
pub(crate) fn post_local_ahead_grace_active() -> bool {
    let until = POST_LOCAL_AHEAD_GRACE_UNTIL_MS.load(Ordering::Relaxed);
    until > 0 && wall_ms() < until
}

#[inline]
pub(crate) fn clear_post_local_ahead_grace() {
    POST_LOCAL_AHEAD_GRACE_UNTIL_MS.store(0, Ordering::Relaxed);
}

/// SLA seconds for logging / rotate messages (mirrors [`tip_sla_breached_for_owner_score`]).
pub(crate) fn tip_sla_secs_for_owner_score(owner_score: f64) -> u64 {
    // W178: cold scores after local catchup — don't use unproven 5s carousel.
    if owner_score <= 0.05 {
        if post_local_ahead_grace_active() {
            tip_sla_floor_secs()
        } else {
            tip_sla_unproven_secs()
        }
    } else if tip_owner_uses_floor_sla(owner_score) {
        tip_sla_floor_secs()
    } else {
        tip_sla_secs()
    }
}

/// W29/W30: tip-SLA breached. Caller clears sticky owner / failover micro-claims —
/// must NOT arm (H,H) failover on WAN gap.
///
/// After a rotate, [`rearm_tip_sla`] clears the latch so a new owner can also be
/// rotated (live: once-per-height latch left covering=1 zombie for 17 min).
///
/// `owner_score`: preferred tip owner score; ≤0.05 uses [`tip_sla_unproven_secs`],
/// ≤[`tip_sla_floor_score_threshold`] (default **0.20**) uses [`tip_sla_floor_secs`],
/// and does not let soft-retry latch block rotation forever on floor/unproven.
pub(crate) fn tip_sla_breached() -> bool {
    tip_sla_breached_for_owner_score(1.0)
}

/// Like [`tip_sla_breached`] with owner-score-aware SLA length.
pub(crate) fn tip_sla_breached_for_owner_score(owner_score: f64) -> bool {
    if TIP_SLA_FIRED.load(Ordering::Relaxed) {
        return false;
    }
    let needed = NEEDED_MS.load(Ordering::Relaxed);
    let body = BODY_MS.load(Ordering::Relaxed);
    if needed == 0 || body != 0 {
        return false;
    }
    let unproven = owner_score <= 0.05;
    let floor = tip_owner_uses_floor_sla(owner_score);
    let grace = post_local_ahead_grace_active();
    // A6k: do not rotate while tip soft-retry is in flight — soft-retry budget (~75s) and
    // SLA (90s) overlap; live W32d″ SLA-rotated owners mid soft-retry → owner carousel.
    // Floor/unproven sticky: soft-retry latch must not extend short SLA indefinitely.
    // W178 grace treats cold owners as floor-tier for soft-retry latch too.
    if !floor && !grace && SOFT_RETRIES.load(Ordering::Relaxed) > 0 {
        return false;
    }
    // W33: breach on NEEDED age — do not require GetData stamp (assign can race coordinator).
    let age_ms = wall_ms().saturating_sub(needed);
    let sla_secs = tip_sla_secs_for_owner_score(owner_score);
    if age_ms < sla_secs.saturating_mul(1000) {
        return false;
    }
    TIP_SLA_FIRED.store(true, Ordering::Relaxed);
    true
}

/// W36: after tip-owner rotate, allow another SLA window on the same height.
pub(crate) fn rearm_tip_sla() {
    TIP_SLA_FIRED.store(false, Ordering::Relaxed);
    // Restart the needed clock so the next breach waits a full SLA period.
    NEEDED_MS.store(wall_ms(), Ordering::Relaxed);
}

#[inline]
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Tip body→feeder handoff ms for `[IBD_TIP_STAGE]`.
///
/// Deep-pipe pre-roll credits `body` before `needed`. Raw `feeder - body` then reports
/// multi-second "stalls" (Mode T M5: p50 body→feeder ~14s) while post-needed handoff
/// stalls are ~0. When `body < needed`, measure from tip-needed instead.
#[inline]
pub(crate) fn handoff_body_feeder_ms(needed: u64, body: u64, feeder: u64) -> i64 {
    if body == 0 || feeder == 0 {
        return -1;
    }
    let start = if needed > 0 && body < needed {
        needed
    } else {
        body
    };
    feeder as i64 - start as i64
}

/// Begin (or refresh) tracking for `next_needed`. Resets stage stamps when height changes.
pub(crate) fn mark_needed(h: u64) {
    let prev = HEIGHT.swap(h, Ordering::Relaxed);
    if prev != h {
        NEEDED_MS.store(wall_ms(), Ordering::Relaxed);
        GETDATA_MS.store(0, Ordering::Relaxed);
        BODY_MS.store(0, Ordering::Relaxed);
        REORDER_MS.store(0, Ordering::Relaxed);
        FEEDER_MS.store(0, Ordering::Relaxed);
        SOFT_RETRIES.store(0, Ordering::Relaxed);
        TIP_SLA_FIRED.store(false, Ordering::Relaxed);
        HANDOFF_SLA_FIRED.store(false, Ordering::Relaxed);
        AHEAD_FREEZE_SOFT.store(false, Ordering::Relaxed);
        // Live 2026-07-16: TIP_FAILOVER_ARMED stuck across height rolls under
        // LOCAL_AHEAD → perpetual covering=2/2 (H,H) treadmill (~0.2 blk/s).
        TIP_FAILOVER_ARMED.store(false, Ordering::Relaxed);
        // W68: deep-pipe GetData/body may already be in flight for `h`.
        credit_pre_roll_stamps(h);
        note_tip_duty_advance();
    } else if NEEDED_MS.load(Ordering::Relaxed) == 0 {
        NEEDED_MS.store(wall_ms(), Ordering::Relaxed);
        credit_pre_roll_stamps(h);
    }
}

#[inline]
fn is_tracked(h: u64) -> bool {
    HEIGHT.load(Ordering::Relaxed) == h
}

fn note_getdata_body_ewma(ms: u64) {
    let ms = ms.min(60_000);
    let prev = GETDATA_BODY_EWMA_MS.load(Ordering::Relaxed);
    let next = if prev == 0 {
        ms
    } else {
        (prev.saturating_mul(7).saturating_add(ms)) / 8
    };
    GETDATA_BODY_EWMA_MS.store(next, Ordering::Relaxed);
    GETDATA_BODY_EWMA_N.fetch_add(1, Ordering::Relaxed);
}

/// Three IBD regimes — must never conflate on the tip90 / tip_crawl scoreboard.
///
/// Phase 0b.3 (docs/RBITCOIN_VS_BLVM_IBD_ARCHITECTURE.md): product tip_crawl / tip90
/// is **only** valid in [`IbdSyncRegime::TrueWanTipCrawl`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IbdSyncRegime {
    /// Lab single-hop dens (`BLVM_IBD_SYNTH_WAN=1`).
    SynthDens,
    /// Real peers, `next_needed ≤ body_tip` (disk burn; contaminates wall_bps).
    LocalAhead,
    /// Real peers, `next_needed > body_tip` — product tip_crawl / tip90.
    TrueWanTipCrawl,
}

/// Classify sync regime for scoreboard honesty (no DNA — pure predicate).
#[inline]
pub(crate) fn classify_ibd_regime(
    next_needed: u64,
    body_tip: u64,
    synth_wan: bool,
) -> IbdSyncRegime {
    if synth_wan {
        return IbdSyncRegime::SynthDens;
    }
    if next_needed == 0 || next_needed <= body_tip {
        IbdSyncRegime::LocalAhead
    } else {
        IbdSyncRegime::TrueWanTipCrawl
    }
}

/// Product tip_crawl / tip90 metrics are only honest under TRUE WAN tip_crawl.
///
/// `wall_bps` alone must not be labeled tip_crawl (synth dens / LOCAL_AHEAD poison).
#[inline]
pub(crate) fn product_tip_crawl_metric_ok(regime: IbdSyncRegime) -> bool {
    matches!(regime, IbdSyncRegime::TrueWanTipCrawl)
}

/// Publish assigner/coordinator WAN body tip for tip-crawl regime detection.
pub(crate) fn publish_wan_body_tip(height: u64) {
    WAN_BODY_TIP.store(height, Ordering::Relaxed);
}

#[inline]
pub(crate) fn wan_body_tip_published() -> u64 {
    WAN_BODY_TIP.load(Ordering::Relaxed)
}

/// Note a tip-adjacent local-disk stream fill (`[IBD_TIP_LOCAL_STREAM]`).
pub(crate) fn note_tip_local_stream() {
    LOCAL_STREAM_LAST_MS.store(wall_ms(), Ordering::Relaxed);
}

/// Land E: tip-crawl + healthy tip supply (fast GD and/or fresh LOCAL_STREAM).
///
/// Used to hold Critical engine demotes at Elevated while archive/dens-class bytes
/// already feed the tip — closes Chunk B without raising host RSS budgets.
pub(crate) fn tip_crawl_supply_healthy(next_height: u64) -> bool {
    let body = WAN_BODY_TIP.load(Ordering::Relaxed);
    if next_height == 0 || next_height <= body {
        return false;
    }
    if let Some((ms, _)) = getdata_body_ewma_ms() {
        if ms <= TIP_CRAWL_GD_HEALTHY_MS {
            return true;
        }
    }
    let last = LOCAL_STREAM_LAST_MS.load(Ordering::Relaxed);
    if last > 0 {
        let age = wall_ms().saturating_sub(last);
        if age <= TIP_CRAWL_STREAM_FRESH_MS {
            return true;
        }
    }
    false
}

/// Land E helper keyed off tracked tip height (`mark_needed`).
#[inline]
pub(crate) fn tip_crawl_supply_healthy_now() -> bool {
    let h = HEIGHT.load(Ordering::Relaxed);
    tip_crawl_supply_healthy(h)
}

/// Warm getdata→body EWMA for A6m (None until ≥16 network-path samples).
pub(crate) fn getdata_body_ewma_ms() -> Option<(u64, u64)> {
    getdata_body_ewma_ms_min_n(16)
}

/// C1o: tip-hole gd-fast may arm earlier than A6m (default caller min_n=8).
pub(crate) fn getdata_body_ewma_ms_min_n(min_n: u64) -> Option<(u64, u64)> {
    let n = GETDATA_BODY_EWMA_N.load(Ordering::Relaxed);
    if n < min_n {
        return None;
    }
    Some((GETDATA_BODY_EWMA_MS.load(Ordering::Relaxed), n))
}

#[cfg(test)]
pub(crate) fn test_reset_getdata_body_ewma() {
    GETDATA_BODY_EWMA_MS.store(0, Ordering::Relaxed);
    GETDATA_BODY_EWMA_N.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn test_seed_getdata_body_ewma(ms: u64, n: u64) {
    GETDATA_BODY_EWMA_MS.store(ms, Ordering::Relaxed);
    GETDATA_BODY_EWMA_N.store(n, Ordering::Relaxed);
}

/// Tip-band PIPE_FILL with `received=0` — arms A6m mute tenure bypass (§7.3).
pub(crate) fn note_pipe_fill_recv0() {
    PIPE_FILL_RECV0_STREAK.fetch_add(1, Ordering::Relaxed);
}

/// Tip-band network body arrived — clear PIPE_FILL mute streak.
pub(crate) fn clear_pipe_fill_recv0_streak() {
    PIPE_FILL_RECV0_STREAK.store(0, Ordering::Relaxed);
}

pub(crate) fn pipe_fill_recv0_streak() -> u64 {
    PIPE_FILL_RECV0_STREAK.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn test_set_pipe_fill_recv0_streak(n: u64) {
    PIPE_FILL_RECV0_STREAK.store(n, Ordering::Relaxed);
}

/// Mousetrap F1: last height contiguous from `tip` that has GetData **or** body stamped.
/// Returns `tip-1` when `tip` itself is uncovered (no pipe yet).
///
/// Distinct from in-flight **claims** (`assign_F`): primary may claim tip..tip+127 while
/// only tip..tip+7 have GetData — secondaries must schedule from this frontier, not claims.
pub(crate) fn pipe_frontier(tip: u64) -> u64 {
    let Ok(guard) = recent_stamps().lock() else {
        return tip.saturating_sub(1);
    };
    let mut h = tip;
    let mut frontier = tip.saturating_sub(1);
    // Cap walk to stamp retention window (tip+512) so assigner polls stay cheap.
    let hi = tip.saturating_add(512);
    while h <= hi {
        if guard.getdata.contains_key(&h) || guard.body.contains_key(&h) {
            frontier = h;
            h = h.saturating_add(1);
        } else {
            break;
        }
    }
    frontier
}

/// Contiguous pipe depth from `tip` (0 if tip uncovered).
pub(crate) fn pipe_contig(tip: u64) -> u64 {
    let f = pipe_frontier(tip);
    if f >= tip {
        f.saturating_sub(tip).saturating_add(1)
    } else {
        0
    }
}

/// First GetData / enqueue for height `h` (tracked tip, or pre-roll deep-pipe).
pub(crate) fn mark_getdata(h: u64) {
    let now = wall_ms();
    if let Ok(mut guard) = recent_stamps().lock() {
        note_recent(&mut guard.getdata, h, now);
    }
    if !is_tracked(h) {
        return;
    }
    let _ = GETDATA_MS.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
}

/// Soft-retry fired while waiting on this tip height.
pub(crate) fn mark_soft_retry(h: u64) {
    // Always latch ahead-freeze — do not require tip_stage tracking. Live 2026-07-15:
    // `[IBD_GAP_SOFT_RETRY]` logged while SOFT_RETRIES stayed 0 (untracked height) →
    // 69 ahead assigns within 12s of soft-retry, applied ~4 blk/s.
    AHEAD_FREEZE_SOFT.store(true, Ordering::Relaxed);
    if !is_tracked(h) {
        // Best-effort: align tracker so SLA / stage diag still work.
        mark_needed(h);
    }
    SOFT_RETRIES.fetch_add(1, Ordering::Relaxed);
}

/// Body bytes arrived from peer (or local) for height `h` (tracked tip, or pre-roll).
pub(crate) fn mark_body(h: u64) {
    let now = wall_ms();
    if let Ok(mut guard) = recent_stamps().lock() {
        note_recent(&mut guard.body, h, now);
    }
    if !is_tracked(h) {
        return;
    }
    let _ = BODY_MS.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
    // Tip body flowing — release ahead freeze / failover second slot.
    AHEAD_FREEZE_SOFT.store(false, Ordering::Relaxed);
    TIP_FAILOVER_ARMED.store(false, Ordering::Relaxed);
    // W179: do NOT clear post-local grace here. Live W178 rewind soak: first tip body
    // landed within seconds → grace dropped while tip_owner scores were still 0.001 →
    // unproven SLA=5s carousel (muteCAP×11 / tip60 43→22 by ~309k). Keep wall-clock
    // grace so PeerScorer can warm before unproven rotate returns.
}

/// W43d: tracked tip body has landed (walk-in may yield after this).
#[inline]
pub(crate) fn tip_body_landed() -> bool {
    BODY_MS.load(Ordering::Relaxed) != 0
}

/// Tip height entered coordinator reorder buffer.
pub(crate) fn mark_reorder(h: u64) {
    if !is_tracked(h) {
        return;
    }
    let _ = REORDER_MS.compare_exchange(0, wall_ms(), Ordering::Relaxed, Ordering::Relaxed);
}

/// Tip height entered feeder buffer.
pub(crate) fn mark_feeder(h: u64) {
    if !is_tracked(h) {
        return;
    }
    let _ = FEEDER_MS.compare_exchange(0, wall_ms(), Ordering::Relaxed, Ordering::Relaxed);
}

/// W34b: seconds without tip reaching feeder after body (default **2**).
pub(crate) fn handoff_sla_secs() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_HANDOFF_SLA_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2)
            .clamp(1, 30)
    })
}

static HANDOFF_SLA_FIRED: AtomicBool = AtomicBool::new(false);

/// W34b: body received but feeder not fed within SLA.
///
/// Deep-pipe pre-roll credits `BODY_MS` from before tip-needed — that lead is not a
/// handoff stall. SLA starts at `max(body, needed)`.
pub(crate) fn body_feeder_sla_breached() -> bool {
    if HANDOFF_SLA_FIRED.load(Ordering::Relaxed) {
        return false;
    }
    let body = BODY_MS.load(Ordering::Relaxed);
    let feeder = FEEDER_MS.load(Ordering::Relaxed);
    if body == 0 || feeder != 0 {
        return false;
    }
    let needed = NEEDED_MS.load(Ordering::Relaxed);
    let start = if needed > 0 { body.max(needed) } else { body };
    let age_ms = wall_ms().saturating_sub(start);
    if age_ms < handoff_sla_secs().saturating_mul(1000) {
        return false;
    }
    HANDOFF_SLA_FIRED.store(true, Ordering::Relaxed);
    true
}

#[inline]
pub(crate) fn tracked_tip_height() -> u64 {
    HEIGHT.load(Ordering::Relaxed)
}

/// Mark height `h` as taken from the feeder (call while holding the feeder lock).
#[inline]
pub(crate) fn mark_taken_from_feeder(h: u64) {
    TAKEN_THROUGH.fetch_max(h, Ordering::Relaxed);
}

/// True when tip `h` was already taken from the feeder into the validation pipeline.
///
/// Prefer [`mark_taken_from_feeder`] (set under the feeder lock on remove). [`HEIGHT`] alone
/// only advances in [`finish_validated`] *after* the feeder unlock — that race made the
/// coordinator treat in-flight validation as a lost ReadyItem (`IBD_TIP_REWIND` +
/// `IBD_FEEDER_PRUNE`). Also true when [`HEIGHT`] has rolled past `h` (tracker advance).
#[inline]
pub(crate) fn tip_taken_by_validation(h: u64) -> bool {
    TAKEN_THROUGH.load(Ordering::Relaxed) >= h || HEIGHT.load(Ordering::Relaxed) > h
}

/// Validation consumed height `h`. Emits `[IBD_TIP_STAGE]` when network wait was involved.
pub(crate) fn finish_validated(h: u64) {
    // Always record take — even when tip_stage tracking skipped this height.
    mark_taken_from_feeder(h);
    if !is_tracked(h) {
        return;
    }
    let needed = NEEDED_MS.load(Ordering::Relaxed);
    let getdata = GETDATA_MS.load(Ordering::Relaxed);
    let body = BODY_MS.load(Ordering::Relaxed);
    let reorder = REORDER_MS.load(Ordering::Relaxed);
    let feeder = FEEDER_MS.load(Ordering::Relaxed);
    let soft = SOFT_RETRIES.load(Ordering::Relaxed);
    let now = wall_ms();

    // Only log network-path tips (GetData seen) or soft-retried tips — skip pure local inject.
    if getdata == 0 && soft == 0 {
        // Advance tracker to next height without logging.
        let next = h.saturating_add(1);
        let _ = HEIGHT.compare_exchange(h, next, Ordering::Relaxed, Ordering::Relaxed);
        NEEDED_MS.store(now, Ordering::Relaxed);
        GETDATA_MS.store(0, Ordering::Relaxed);
        BODY_MS.store(0, Ordering::Relaxed);
        REORDER_MS.store(0, Ordering::Relaxed);
        FEEDER_MS.store(0, Ordering::Relaxed);
        SOFT_RETRIES.store(0, Ordering::Relaxed);
        TIP_SLA_FIRED.store(false, Ordering::Relaxed);
        HANDOFF_SLA_FIRED.store(false, Ordering::Relaxed);
        AHEAD_FREEZE_SOFT.store(false, Ordering::Relaxed);
        TIP_FAILOVER_ARMED.store(false, Ordering::Relaxed);
        credit_pre_roll_stamps(next);
        return;
    }

    let d = |a: u64, b: u64| -> i64 {
        if a == 0 || b == 0 {
            -1
        } else {
            b as i64 - a as i64
        }
    };

    let gd_body = d(getdata, body);
    if gd_body >= 0 {
        note_getdata_body_ewma(gd_body as u64);
    }
    let need_body = d(needed, body);
    let body_feeder = handoff_body_feeder_ms(needed, body, feeder);
    let feeder_done = d(feeder, now);
    super::ms_breakdown::note_tip_stage(need_body, gd_body, body_feeder, feeder_done);

    // W68: getdata→body includes pre-roll GetData lead; need→body is the true tip wait.
    warn!(
        "[IBD_TIP_STAGE] h={} total_ms={} need→getdata={} getdata→body={} need→body={} body→feeder={} feeder→done={} soft_retries={} reorder_ms_set={} (needed={} getdata={} body={} reorder={} feeder={})",
        h,
        d(needed, now),
        d(needed, getdata),
        gd_body,
        need_body,
        body_feeder,
        feeder_done,
        soft,
        if reorder == 0 { 0 } else { 1 },
        needed,
        getdata,
        body,
        reorder,
        feeder
    );

    // Roll tracker forward to the next tip.
    let next = h.saturating_add(1);
    HEIGHT.store(next, Ordering::Relaxed);
    NEEDED_MS.store(now, Ordering::Relaxed);
    GETDATA_MS.store(0, Ordering::Relaxed);
    BODY_MS.store(0, Ordering::Relaxed);
    REORDER_MS.store(0, Ordering::Relaxed);
    FEEDER_MS.store(0, Ordering::Relaxed);
    SOFT_RETRIES.store(0, Ordering::Relaxed);
    TIP_SLA_FIRED.store(false, Ordering::Relaxed);
    HANDOFF_SLA_FIRED.store(false, Ordering::Relaxed);
    AHEAD_FREEZE_SOFT.store(false, Ordering::Relaxed);
    TIP_FAILOVER_ARMED.store(false, Ordering::Relaxed);
    credit_pre_roll_stamps(next);
}

/// Test helper: backdate tip-wait stamps so CAP / freeze / empty-triple clocks advance.
#[cfg(test)]
pub(crate) fn test_backdate_awaiting_ms(ago_ms: u64) {
    let start = wall_ms().saturating_sub(ago_ms);
    NEEDED_MS.store(start, Ordering::Relaxed);
    GETDATA_MS.store(0, Ordering::Relaxed);
    BODY_MS.store(0, Ordering::Relaxed);
}

/// Serialize tests that mutate process-global tip-stage / bridge atomics.
#[cfg(test)]
pub(crate) fn test_tip_atomics_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Test helper: zero tip-stage atomics so parallel unit tests do not leak soft-retry /
/// freeze latches into assigner soak-shape tests.
#[cfg(test)]
pub(crate) fn test_reset_tip_stage() {
    HEIGHT.store(0, Ordering::Relaxed);
    TAKEN_THROUGH.store(0, Ordering::Relaxed);
    NEEDED_MS.store(0, Ordering::Relaxed);
    GETDATA_MS.store(0, Ordering::Relaxed);
    BODY_MS.store(0, Ordering::Relaxed);
    REORDER_MS.store(0, Ordering::Relaxed);
    FEEDER_MS.store(0, Ordering::Relaxed);
    SOFT_RETRIES.store(0, Ordering::Relaxed);
    WAN_BODY_TIP.store(0, Ordering::Relaxed);
    LOCAL_STREAM_LAST_MS.store(0, Ordering::Relaxed);
    TIP_SLA_FIRED.store(false, Ordering::Relaxed);
    HANDOFF_SLA_FIRED.store(false, Ordering::Relaxed);
    AHEAD_FREEZE_SOFT.store(false, Ordering::Relaxed);
    TIP_FAILOVER_ARMED.store(false, Ordering::Relaxed);
    DUTY_DEPTH.store(0, Ordering::Relaxed);
    DUTY_LAST_MS.store(0, Ordering::Relaxed);
    DUTY_MS_B0.store(0, Ordering::Relaxed);
    DUTY_MS_B1.store(0, Ordering::Relaxed);
    DUTY_MS_B2.store(0, Ordering::Relaxed);
    DUTY_MS_B3.store(0, Ordering::Relaxed);
    DUTY_MS_B4.store(0, Ordering::Relaxed);
    DUTY_MS_B5.store(0, Ordering::Relaxed);
    DUTY_TIP_LAST_MS.store(0, Ordering::Relaxed);
    DUTY_IA_SUM_MS.store(0, Ordering::Relaxed);
    DUTY_IA_N.store(0, Ordering::Relaxed);
    DUTY_LOG_LAST_MS.store(0, Ordering::Relaxed);
    if let Ok(mut g) = recent_stamps().lock() {
        g.getdata.clear();
        g.body.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn reset_tracker() {
        HEIGHT.store(0, Ordering::Relaxed);
        TAKEN_THROUGH.store(0, Ordering::Relaxed);
        NEEDED_MS.store(0, Ordering::Relaxed);
        GETDATA_MS.store(0, Ordering::Relaxed);
        BODY_MS.store(0, Ordering::Relaxed);
        REORDER_MS.store(0, Ordering::Relaxed);
        FEEDER_MS.store(0, Ordering::Relaxed);
        SOFT_RETRIES.store(0, Ordering::Relaxed);
        WAN_BODY_TIP.store(0, Ordering::Relaxed);
        LOCAL_STREAM_LAST_MS.store(0, Ordering::Relaxed);
        GETDATA_BODY_EWMA_MS.store(0, Ordering::Relaxed);
        GETDATA_BODY_EWMA_N.store(0, Ordering::Relaxed);
        TIP_SLA_FIRED.store(false, Ordering::Relaxed);
        HANDOFF_SLA_FIRED.store(false, Ordering::Relaxed);
        AHEAD_FREEZE_SOFT.store(false, Ordering::Relaxed);
        TIP_FAILOVER_ARMED.store(false, Ordering::Relaxed);
        clear_post_local_ahead_grace();
        if let Ok(mut g) = recent_stamps().lock() {
            g.getdata.clear();
            g.body.clear();
        }
    }

    /// Phase 0b.3: forbid scoring wall_bps / dens as product tip_crawl.
    #[test]
    fn regime_scoreboard_product_tip_crawl_requires_next_gt_body() {
        assert_eq!(
            classify_ibd_regime(400_001, 400_000, false),
            IbdSyncRegime::TrueWanTipCrawl
        );
        assert!(product_tip_crawl_metric_ok(IbdSyncRegime::TrueWanTipCrawl));

        assert_eq!(
            classify_ibd_regime(400_000, 400_000, false),
            IbdSyncRegime::LocalAhead
        );
        assert_eq!(
            classify_ibd_regime(399_999, 400_000, false),
            IbdSyncRegime::LocalAhead
        );
        assert!(!product_tip_crawl_metric_ok(IbdSyncRegime::LocalAhead));

        // Synth dens wins even when next > body (lab wall_bps ≠ tip90 product).
        assert_eq!(
            classify_ibd_regime(450_000, 400_000, true),
            IbdSyncRegime::SynthDens
        );
        assert!(!product_tip_crawl_metric_ok(IbdSyncRegime::SynthDens));
    }

    #[test]
    fn tip_crawl_supply_healthy_requires_past_body_and_fast_gd_or_stream() {
        // Share lock with index Land E demote test (same process-global atomics).
        let _lock = test_tip_atomics_lock();
        reset_tracker();
        publish_wan_body_tip(437_309);
        mark_needed(437_310);
        assert!(
            !tip_crawl_supply_healthy_now(),
            "no GD / STREAM yet → unhealthy"
        );
        test_seed_getdata_body_ewma(40, 32);
        assert!(
            tip_crawl_supply_healthy_now(),
            "fast GD under tip crawl → healthy"
        );
        test_seed_getdata_body_ewma(500, 32);
        assert!(
            !tip_crawl_supply_healthy_now(),
            "slow GD without STREAM → unhealthy"
        );
        note_tip_local_stream();
        assert!(
            tip_crawl_supply_healthy_now(),
            "fresh LOCAL_STREAM → healthy even if GD slow"
        );
        mark_needed(400_000);
        assert!(
            !tip_crawl_supply_healthy_now(),
            "LOCAL_AHEAD (next ≤ body_tip) → not tip-crawl hold"
        );
        reset_tracker();
    }

    #[test]
    #[serial_test::serial]
    fn mousetrap_pipe_frontier_ignores_uncovered_gap() {
        let _lock = test_lock();
        reset_tracker();
        let tip = 304_664u64;
        // GetData only tip..tip+7 (grown=8); no stamps at tip+8+.
        for h in tip..tip + 8 {
            mark_getdata(h);
        }
        assert_eq!(pipe_frontier(tip), tip + 7);
        assert_eq!(pipe_contig(tip), 8);
        // Hole at tip+8 — frontier must not jump even if a far height was stamped.
        mark_getdata(tip + 127);
        assert_eq!(pipe_frontier(tip), tip + 7, "must stop at first pipe hole");
        assert_eq!(pipe_contig(tip), 8);
        reset_tracker();
    }

    #[test]
    #[serial_test::serial]
    fn mousetrap_pipe_frontier_empty_when_tip_uncovered() {
        let _lock = test_lock();
        reset_tracker();
        let tip = 100u64;
        mark_getdata(101);
        assert_eq!(pipe_frontier(tip), tip.saturating_sub(1));
        assert_eq!(pipe_contig(tip), 0);
        reset_tracker();
    }

    #[test]
    fn w68_pre_roll_getdata_credited_on_tip_roll() {
        let _lock = test_lock();
        reset_tracker();
        // Deep pipe GetData while tip still at 0.
        mark_getdata(100);
        assert_eq!(GETDATA_MS.load(Ordering::Relaxed), 0, "untracked must not stamp live");
        // Tip rolls to 100 — credit original GetData time.
        mark_needed(100);
        let gd = GETDATA_MS.load(Ordering::Relaxed);
        assert!(gd > 0, "W68 must credit pre-roll GetData");
        // Late-body freeze must key off GetData, not open immediately.
        assert!(
            !tip_ahead_frozen_for_late_body(),
            "fresh credit must not look like a 2s late body"
        );
        reset_tracker();
    }

    #[test]
    fn w68_pre_roll_body_clears_failover_on_credit() {
        let _lock = test_lock();
        reset_tracker();
        mark_getdata(50);
        mark_body(50);
        TIP_FAILOVER_ARMED.store(true, Ordering::Relaxed);
        AHEAD_FREEZE_SOFT.store(true, Ordering::Relaxed);
        mark_needed(50);
        assert!(BODY_MS.load(Ordering::Relaxed) > 0);
        assert!(!tip_failover_armed());
        assert!(!tip_ahead_frozen_for_soft_retry());
        reset_tracker();
    }

    #[test]
    fn pre_roll_body_feeder_measures_from_needed_not_body_age() {
        // body 10s before needed; feeder 5ms after needed → handoff ~5ms, not ~10005ms.
        assert_eq!(handoff_body_feeder_ms(10_000, 0, 10_005), -1);
        assert_eq!(handoff_body_feeder_ms(10_000, 0, 0), -1);
        assert_eq!(handoff_body_feeder_ms(10_000, 50, 10_005), 5);
        // Just-in-time body (public WAN): body after needed → classic body→feeder.
        assert_eq!(handoff_body_feeder_ms(10_000, 10_100, 10_103), 3);
    }

    #[test]
    #[serial_test::serial]
    fn pre_roll_body_does_not_trip_handoff_sla() {
        let _lock = test_lock();
        reset_tracker();
        {
            let Ok(mut g) = recent_stamps().lock() else {
                panic!("recent_stamps");
            };
            let now = wall_ms();
            g.body.insert(201, now.saturating_sub(10_000));
            g.getdata.insert(201, now.saturating_sub(10_200));
        }
        mark_needed(201);
        assert!(BODY_MS.load(Ordering::Relaxed) > 0);
        assert!(BODY_MS.load(Ordering::Relaxed) < NEEDED_MS.load(Ordering::Relaxed));
        assert!(
            !body_feeder_sla_breached(),
            "pre-roll body age must not trip handoff SLA before tip-needed window"
        );
        reset_tracker();
    }

    #[test]
    fn w68_finish_validated_credits_next_height() {
        let _lock = test_lock();
        reset_tracker();
        mark_needed(10);
        mark_getdata(10);
        mark_body(10);
        mark_getdata(11); // pre-roll next
        mark_feeder(10);
        finish_validated(10);
        assert_eq!(tracked_tip_height(), 11);
        assert!(
            GETDATA_MS.load(Ordering::Relaxed) > 0,
            "roll must credit pre-roll GetData for h+1"
        );
        reset_tracker();
    }

    #[test]
    fn w68_missing_getdata_still_late_body_after_freeze_secs() {
        let _lock = test_lock();
        reset_tracker();
        mark_needed(77);
        // No GetData credit — after freeze window, late body must still fire.
        NEEDED_MS.store(wall_ms().saturating_sub(3_000), Ordering::Relaxed);
        assert!(tip_ahead_frozen_for_late_body());
        reset_tracker();
    }

    #[test]
    fn w77_download_mark_body_does_not_clear_late_freeze_while_gap_missing() {
        let _lock = test_lock();
        reset_tracker();
        mark_needed(370_588);
        mark_getdata(370_588);
        // Download stamped body, but coordinator never admitted tip (RESEND path).
        mark_body(370_588);
        assert!(BODY_MS.load(Ordering::Relaxed) > 0);
        // Post-roll GetData (getdata >= needed) aged past freeze window.
        let now = wall_ms();
        NEEDED_MS.store(now.saturating_sub(4_000), Ordering::Relaxed);
        GETDATA_MS.store(now.saturating_sub(3_000), Ordering::Relaxed);
        NEEDED_MS.store(now.saturating_sub(4_000), Ordering::Relaxed);
        GETDATA_MS.store(now.saturating_sub(3_000), Ordering::Relaxed);
        crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        assert!(
            !tip_ahead_frozen_for_late_body(),
            "without gap_missing, BODY_MS still clears freeze"
        );
        crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        assert!(
            tip_ahead_frozen_for_late_body(),
            "W77: gap_missing must ignore download BODY_MS for ahead freeze"
        );
        crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        reset_tracker();
    }

    #[test]
    fn w81_pre_roll_getdata_does_not_instant_freeze_under_gap_missing() {
        let _lock = test_lock();
        reset_tracker();
        // Deep pipe GetData 4s before tip roll (typical WAN getdata→body p50).
        mark_getdata(200);
        {
            let Ok(mut g) = recent_stamps().lock() else {
                panic!("recent_stamps lock");
            };
            if let Some(ms) = g.getdata.get_mut(&200) {
                *ms = wall_ms().saturating_sub(4_000);
            }
        }
        mark_needed(200);
        assert!(
            GETDATA_MS.load(Ordering::Relaxed) > 0,
            "W68 must credit pre-roll GetData"
        );
        assert!(
            GETDATA_MS.load(Ordering::Relaxed) < NEEDED_MS.load(Ordering::Relaxed),
            "pre-roll GetData must be older than mark_needed"
        );
        crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        assert!(
            !tip_ahead_frozen_for_late_body(),
            "W81: pre-roll GetData lead must not look like 2s tip distress"
        );
        // Real post-roll wait still freezes.
        NEEDED_MS.store(wall_ms().saturating_sub(3_000), Ordering::Relaxed);
        GETDATA_MS.store(0, Ordering::Relaxed);
        assert!(
            tip_ahead_frozen_for_late_body(),
            "W81: needed-age freeze still arms without GetData"
        );
        crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        reset_tracker();
    }

    #[test]
    fn w165_unproven_sla_breaches_before_floor() {
        // Live W164 @334087: demoted sticky score=0.001 held floor SLA 15s.
        let _lock = test_lock();
        reset_tracker();
        mark_needed(334_087);
        test_backdate_awaiting_ms(6_000);
        TIP_SLA_FIRED.store(false, Ordering::Relaxed);
        assert!(
            tip_sla_breached_for_owner_score(0.001),
            "W165: unproven owner breaches at 6s (SLA=5)"
        );
        TIP_SLA_FIRED.store(false, Ordering::Relaxed);
        assert!(
            !tip_sla_breached_for_owner_score(0.100),
            "W165: floor owner still needs 15s"
        );
        test_backdate_awaiting_ms(16_000);
        TIP_SLA_FIRED.store(false, Ordering::Relaxed);
        assert!(
            tip_sla_breached_for_owner_score(0.100),
            "W165: floor owner breaches at 16s"
        );
        reset_tracker();
    }

    #[test]
    fn w170_floor_sla_covers_mid_band_sticky() {
        // Live W169 @317772: score=0.148 held mid SLA 90s under old ≤0.12 threshold.
        let _lock = test_lock();
        reset_tracker();
        mark_needed(317_772);
        test_backdate_awaiting_ms(16_000);
        TIP_SLA_FIRED.store(false, Ordering::Relaxed);
        assert!(
            tip_owner_uses_floor_sla(0.148),
            "W170: 0.148 is floor-SLA band"
        );
        assert_eq!(tip_sla_secs_for_owner_score(0.148), tip_sla_floor_secs());
        assert!(
            tip_sla_breached_for_owner_score(0.148),
            "W170: score=0.148 breaches floor SLA at 16s (not 90s)"
        );
        TIP_SLA_FIRED.store(false, Ordering::Relaxed);
        assert!(
            !tip_sla_breached_for_owner_score(0.50),
            "W170: mid/high sticky still needs full SLA"
        );
        reset_tracker();
    }

    #[test]
    fn w178_post_local_ahead_grace_uses_floor_sla_for_unproven() {
        // Live W177 @403622: cold score=0.001 + unproven SLA=5s → TIP_SLA carousel → freeze.
        let _lock = test_lock();
        reset_tracker();
        arm_post_local_ahead_grace();
        assert!(post_local_ahead_grace_active());
        assert_eq!(
            tip_sla_secs_for_owner_score(0.001),
            tip_sla_floor_secs(),
            "W178 grace: unproven uses floor SLA not 5s"
        );
        mark_needed(403_622);
        test_backdate_awaiting_ms(6_000);
        TIP_SLA_FIRED.store(false, Ordering::Relaxed);
        assert!(
            !tip_sla_breached_for_owner_score(0.001),
            "W178 grace: 6s must not breach floor SLA"
        );
        test_backdate_awaiting_ms(16_000);
        TIP_SLA_FIRED.store(false, Ordering::Relaxed);
        assert!(
            tip_sla_breached_for_owner_score(0.001),
            "W178 grace: floor SLA still breaches at 16s"
        );
        // W179: first tip body must NOT clear wall-clock grace (scores still cold).
        TIP_SLA_FIRED.store(false, Ordering::Relaxed);
        mark_body(403_622);
        assert!(
            post_local_ahead_grace_active(),
            "W179: grace survives first tip body"
        );
        assert_eq!(tip_sla_secs_for_owner_score(0.001), tip_sla_floor_secs());
        clear_post_local_ahead_grace();
        assert_eq!(tip_sla_secs_for_owner_score(0.001), tip_sla_unproven_secs());
        reset_tracker();
    }
}
