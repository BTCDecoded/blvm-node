//! ChunkAssigner assigns height-ordered chunks to workers. ChunkGuard ensures
//! chunks are re-queued on drop if not disarmed.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::ParallelIBDConfig;
use super::latch_env;
use super::types::{ChunkWorkItem, NoprogComplete, RetryEntry, RetryTrack};

/// Tracks sticky WAN tenure for A6m/A6n measured-BPS rotation.
#[derive(Debug, Clone)]
struct StickyWanTenure {
    peer: String,
    start_next_needed: u64,
    started_at: Instant,
}

/// A6n: rolling WAN tip GAP_STREAM delivery window (not lifetime bulk IBD).
#[derive(Debug, Clone)]
struct TipStreamWindow {
    streams: u64,
    started: Instant,
    /// Wall clock of the most recent `GAP_STREAM` from this peer.
    last_stream: Instant,
}

/// P2: short tip-role trial — challenger pinned for `TRIAL_SECS` without 48s A6m tenure.
#[derive(Debug, Clone)]
struct TipTrial {
    sticky: String,
    challenger: String,
    started: Instant,
    sticky_streams_at_start: u64,
    challenger_streams_at_start: u64,
    next_needed_at_start: u64,
}

/// W4/N12: `get_work` `in_flight_per_peer` wait/hold. Default **on**; `=0` disables.
static ASSIGNER_GW_WAIT_NS: AtomicU64 = AtomicU64::new(0);
static ASSIGNER_GW_HOLD_NS: AtomicU64 = AtomicU64::new(0);
static ASSIGNER_GW_SAMPLES: AtomicU64 = AtomicU64::new(0);
fn assigner_lock_timers_enabled() -> bool {
    match std::env::var("BLVM_IBD_ASSIGNER_LOCK_TIMERS")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        None => true,
        Some("0") | Some("false") | Some("off") | Some("no") => false,
        _ => true,
    }
}

/// Records `in_flight` wait + hold for one `get_work` that acquired the outer lock.
struct AssignerGetWorkTimer {
    wait_ns: u64,
    held_at: Instant,
}

impl AssignerGetWorkTimer {
    fn start(wait_ns: u64) -> Option<Self> {
        if !assigner_lock_timers_enabled() {
            return None;
        }
        Some(Self {
            wait_ns,
            held_at: Instant::now(),
        })
    }
}

impl Drop for AssignerGetWorkTimer {
    fn drop(&mut self) {
        let hold_ns = self.held_at.elapsed().as_nanos() as u64;
        ASSIGNER_GW_WAIT_NS.fetch_add(self.wait_ns, Ordering::Relaxed);
        ASSIGNER_GW_HOLD_NS.fetch_add(hold_ns, Ordering::Relaxed);
        let n = ASSIGNER_GW_SAMPLES.fetch_add(1, Ordering::Relaxed) + 1;
        if n % 4096 == 0 {
            let wait = ASSIGNER_GW_WAIT_NS.load(Ordering::Relaxed);
            let hold = ASSIGNER_GW_HOLD_NS.load(Ordering::Relaxed);
            tracing::info!(
                "[IBD_ASSIGNER_LOCK] samples={} avg_wait_us={:.1} avg_hold_us={:.1}",
                n,
                (wait as f64 / n as f64) / 1000.0,
                (hold as f64 / n as f64) / 1000.0
            );
        }
    }
}

/// leftover_TRACE: who currently holds `in_flight_per_peer` (get_work holds it
/// for the whole assign). leftover_force_arm try_lock_fail reads this.
struct InFlightLockOwner {
    site: &'static str,
}

static IN_FLIGHT_LOCK_SITE: Mutex<Option<(&'static str, String, u64, Instant)>> = Mutex::new(None);

fn note_in_flight_holder(site: &'static str, peer: &str, next: u64) -> InFlightLockOwner {
    if let Ok(mut g) = IN_FLIGHT_LOCK_SITE.lock() {
        *g = Some((site, peer.to_string(), next, Instant::now()));
    }
    InFlightLockOwner { site }
}

impl Drop for InFlightLockOwner {
    fn drop(&mut self) {
        if let Ok(mut g) = IN_FLIGHT_LOCK_SITE.lock() {
            if g.as_ref().is_some_and(|(s, ..)| *s == self.site) {
                *g = None;
            }
        }
    }
}

fn format_in_flight_holder() -> String {
    match IN_FLIGHT_LOCK_SITE.lock() {
        Ok(g) => match &*g {
            Some((site, peer, next, at)) => format!(
                "site={site} peer={peer} next={next} held_ms={}",
                at.elapsed().as_millis()
            ),
            None => "site=unknown".to_string(),
        },
        Err(_) => "site=holder_poison".to_string(),
    }
}

/// leftover_TRACE inside leftover `get_work` after `in_flight` is held.
/// One line per phase on the leftover-band call — last phase is the wait.
fn leftover_get_work_phase(
    peer_id: &str,
    next: u64,
    body_tip: u64,
    leftover_force: bool,
    phase: &'static str,
) {
    if super::leftover_trace_watch(leftover_force, next, body_tip) {
        leftover_trace_rate(
            "get_work_phase",
            format!(
                "peer={peer_id} next={next} body_tip={body_tip} leftover_force={leftover_force} phase={phase}"
            ),
        );
    }
}

pub(crate) fn leftover_trace_rate(step: &'static str, msg: String) {
    static LAST_MS: Mutex<Option<HashMap<&'static str, u64>>> = Mutex::new(None);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if let Ok(mut slot) = LAST_MS.lock() {
        let map = slot.get_or_insert_with(HashMap::new);
        let prev = map.get(step).copied().unwrap_or(0);
        if now.saturating_sub(prev) < 5_000 {
            return;
        }
        map.insert(step, now);
    }
    tracing::warn!("[IBD_LEFTOVER_TRACE] step={step} {msg}");
}

/// Chunk of blocks to download, assigned to a specific peer.
#[derive(Debug, Clone)]
pub struct BlockChunk {
    pub start_height: u64,
    pub end_height: u64,
    pub peer_id: String,
}

/// Create chunks for parallel download.
///
/// When scored_peers is Some and BLVM_IBD_MODE=earliest: assign all chunks to fastest peer
/// (Core-like, avoids chunk-boundary stalls when slow peer holds next chunk).
/// Otherwise: round-robin (chunk i → peer i % num_peers).
pub fn create_chunks(
    config: &ParallelIBDConfig,
    start_height: u64,
    end_height: u64,
    peer_ids: &[String],
    scored_peers: Option<&[(String, f64)]>,
) -> Vec<BlockChunk> {
    let mut chunks = Vec::new();
    let mut current_height = start_height;
    let num_peers = peer_ids.len().max(1);
    let mut chunk_index: usize = 0;

    let use_fastest = (config.mode.eq_ignore_ascii_case("earliest") || config.earliest_first)
        && num_peers > 1
        && scored_peers.map(|s| !s.is_empty()).unwrap_or(false);

    let fastest_peer = if use_fastest {
        scored_peers.and_then(|s| {
            s.iter()
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(p, _)| p.clone())
        })
    } else {
        None
    };

    if use_fastest && fastest_peer.is_some() {
        tracing::info!("IBD: earliest-first — all chunks to fastest peer");
    } else {
        tracing::info!(
            "Round-robin chunk assignment: {} peers, chunk_size={}",
            num_peers,
            config.chunk_size
        );
    }

    while current_height <= end_height {
        let (chunk_sz, is_bootstrap) = if current_height == 0 && start_height == 0 {
            let sz = 128.min(end_height.saturating_add(1));
            (sz, true)
        } else {
            (config.chunk_size, false)
        };
        let chunk_end = (current_height + chunk_sz - 1).min(end_height);
        if is_bootstrap {
            tracing::info!(
                "IBD: bootstrap chunk 0-{} (99 and 100 in same chunk)",
                chunk_end
            );
        }

        let peer_id = fastest_peer.clone().unwrap_or_else(|| {
            if peer_ids.is_empty() {
                String::new()
            } else {
                peer_ids[chunk_index % num_peers].clone()
            }
        });

        chunks.push(BlockChunk {
            start_height: current_height,
            end_height: chunk_end,
            peer_id,
        });

        current_height = chunk_end + 1;
        chunk_index += 1;
    }

    chunks
}

/// Sequential chunk assigner: assigns height ranges in order so validation never starves.
/// Workers call get_work(peer_id); assigner returns next range when start ≤ validation + max_ahead.
///
/// **Ranges vs workers are independent.** `create_chunks` still stamps a preferred peer per
/// range for LAN sequential affinity; WAN work-stealing ignores affinity and uses `workers`
/// only as the active download-worker set. Tests no longer pad a fake peer-per-range vector.
pub(crate) struct ChunkAssigner {
    /// Height ranges to download (main queue), ordered by start height.
    chunks: Vec<(u64, u64)>,
    /// Unique ACTIVE download worker peer ids (P1-capped). Membership / scoring floor / open-slot.
    workers: Vec<String>,
    /// Peers spawned after construction (peer-watcher replacements). Merged into
    /// [`Self::is_active_download_worker`]. Live wan10k-c4 @438479: replacements called
    /// `get_work` but stayed outside `workers` → OPEN_STALL `ready_active_ok=0/0` while
    /// TIP_CRAWL `ready≥2`, tip hole forever.
    extra_workers: Mutex<HashSet<String>>,
    /// Preferred peer for `chunks[i]` when `!work_stealing`. Empty under work-stealing.
    preferred_peers: Vec<String>,
    next_index: AtomicUsize,
    retry_queue: Mutex<VecDeque<RetryEntry>>,
    /// R-283: attempts/sticky exclude keyed by (start,end). Survives get_work pop.
    retry_track: Mutex<HashMap<(u64, u64), RetryTrack>>,
    /// R-336 window assigner: heights whose chunk completed Ok (delivered), ≥ next_needed.
    window_done: Mutex<std::collections::BTreeMap<u64, Instant>>,
    /// R-336: `(peer, start, end)` → issue time, for front-of-window stall duplicates.
    window_started: Mutex<HashMap<(String, u64, u64), Instant>>,
    /// R-336: `(start, end)` → (failing peer, when). That peer skips these heights briefly.
    window_fail: Mutex<HashMap<(u64, u64), (String, Instant)>>,
    /// R-336: front-stall strikes per holder `(count, last)`.
    window_strikes: Mutex<HashMap<String, (u32, Instant)>>,
    /// R-336: peers queued for eviction by the window path; drained by the worker loop.
    window_evict: Mutex<Vec<String>>,
    /// R-336: last window eviction (rate limit).
    window_last_evict: Mutex<Option<Instant>>,
    /// R-337: front-stallers benched off the window until `Instant`.
    window_bench: Mutex<HashMap<String, Instant>>,
    /// R-344: struck-out front holders kept off the front reserve until `Instant` (stay on the roster).
    window_front_cool: Mutex<HashMap<String, Instant>>,
    /// R-345: strike-out history per peer `(count, last)`; drives bench escalation 15 → 60 → 300 s.
    window_offense: Mutex<HashMap<String, (u32, Instant)>>,
    /// R-338: EMA of window tile wall time (ms); 0 until the first completion.
    window_tile_ms_ema: AtomicU64,
    /// R-341: roster EMA of per-block tile time (ms).
    window_blk_ms_ema: AtomicU64,
    /// R-341: per-peer `(per-block ms EMA, completions)`.
    window_peer_blk_ms: Mutex<HashMap<String, (u64, u32)>>,
    /// R-284: last `Ok(chunk)` per `(peer, start, end)`. Skip re-issue when
    /// `net_block_count==0` and `next_needed` is unchanged. Evict `end < next`.
    noprog_ok: Mutex<HashMap<(String, u64, u64), NoprogComplete>>,
    validation_height: Arc<std::sync::atomic::AtomicU64>,
    /// When true, only chunks with start==0 are assignable. Set when start_height==0; cleared when bootstrap chunk completes.
    bootstrap_complete: AtomicBool,
    start_height: u64,
    /// Per-peer in-flight ranges (usually 1; top scorers may hold 2 — A4).
    in_flight_per_peer: Mutex<HashMap<String, Vec<(u64, u64)>>>,
    /// When true (WAN multi-peer), ignore peer binding: any peer worker takes any available chunk.
    work_stealing: bool,
    /// Peer blacklist: peer_id -> blacklisted_until. Blacklisted peers get no work from get_work().
    blacklisted_until: Mutex<HashMap<String, Instant>>,
    /// Last stall requeue (height, time) — debounces duplicate micro-chunk storms.
    last_stall_requeue: Mutex<Option<(u64, Instant)>>,
    /// P5/A5: peer_id → score for gap/preempt routing.
    peer_scores: Mutex<HashMap<String, f64>>,
    /// Contiguous body-confirmed height at IBD start (local-replay / gap-fill gating).
    confirmed_body_height_at_start: AtomicU64,
    /// On-disk body tip for WAN tip-crawl gating — must match coordinator `live_body_tip`.
    ///
    /// Distinct from [`Self::confirmed_body_height_at_start`]: genesis IBD often has
    /// `confirmed=0` while GAP_PERSIST has already written sparse bodies. Using confirmed
    /// alone makes [`Self::wan_tip_gap_crawl`] always false → tip-nudge no-ops while the
    /// coordinator still thinks it is past body tip (live 2026-07-14 stall at h=513).
    wan_body_tip: AtomicU64,
    /// Coordinator sets when validation tip (val+1) is absent from reorder_buffer.
    tip_gap_missing: AtomicBool,
    /// Validation stall / leftover inject-miss — not spawn tip_gap_missing.
    leftover_force_getdata: AtomicBool,
    /// Bridge holes in `[next_expected, …]` from coordinator (`pending_diag`). Used to gate
    /// multi-peer tip-band ahead — **not** steady-state `gap_missing` (always true while
    /// waiting for the next body). Live 2026-07-15: `allow_ahead=!gap_missing` → covering=1
    /// forever with ready≈52 → tip ~5 blk/s vs ≥80 target.
    tip_bridge_holes: AtomicU64,
    /// Exclusive end of the reorder hole in front of `first_ahead`.
    /// `0` means no ahead body, so a tip unmark stays one height and does not
    /// open the frontier. Heights `>= first_ahead` stay in `window_done`.
    window_hole_until: AtomicU64,
    /// Tip body is in neither reorder, the feeder, nor the bridge.
    /// `tip_taken` is not enough: resume21 had contig=1 from that latch while
    /// the body was gone, so ahead peers never released.
    window_tip_uncovered: AtomicBool,
    /// W28c: sticky tip-owner peer — prefer until they fail / become unavailable.
    preferred_tip_owner: Mutex<Option<String>>,
    /// W92: peer_id → Instant until which they must not win tip-owner / TIP_PIN.
    /// Live W91: CAP=5 abort cleared sticky, then TIP_PIN immediately re-elected the
    /// same top_w (`34.48.38.29`) → 5s×N freeze @313369 until tip60-watch killed IBD.
    tip_owner_fail_until: Mutex<HashMap<String, Instant>>,
    /// W28d: ranges assigned as tip owner or tip failover. Ahead partitions are **not**
    /// listed — when tip walks into an ahead range, raw covering > 0 but claims == 0,
    /// so we can assign a real tip-owner pipeline instead of treating the ahead peer as healthy.
    tip_cover_claims: Mutex<Vec<(String, u64, u64)>>,
    /// W36: after tip-SLA rotate, any top-half peer may take tip until a new owner is assigned
    /// (W33a top-1 alone left no live worker when the sole top peer was blacklisted in-flight).
    tip_owner_open: AtomicBool,

    /// W87/W88: tip height where a distress `(H,H)` failover was assigned (episode latch).
    /// W87 cleared on every +1 tip advance → live cascade of 10 failovers / 10 heights
    /// after CAP (~2s each) crushed tip60 45→9. W88 keeps the latch for an *episode*
    /// (advance ≥32 or ~30s) so one stall cannot re-arm failover on each new tip height.
    tip_failover_once_h: AtomicU64,
    /// W88: unix-ms when `tip_failover_once_h` was latched (0 = inactive).
    tip_failover_once_at_ms: AtomicU64,
    /// W123/W124: sticky W35 ahead-freeze while tip-band holes stay fat + feeder empty.
    tip_ahead_hole_freeze: AtomicBool,
    /// W183: unix-ms when holes first dropped below clear while freeze latched (0 = not clearing).
    /// Prevents freeze flap when holes oscillate 0↔20 across tip+1 (live W182 @330–345k).
    tip_ahead_hole_clear_since_ms: AtomicU64,
    /// P0-A: coordinator-refreshed set of peers with transport up + Bitcoin handshake complete.
    ibd_ready_peers: Mutex<HashSet<String>>,
    /// A6m: sticky WAN tenure for measured-BPS rotation.
    sticky_wan_tenure: Mutex<Option<StickyWanTenure>>,
    /// A6m: last measured-BPS sticky rotation (cooldown).
    last_a6m_rotate_at: Mutex<Option<Instant>>,
    /// A6m: rolling `(Instant, next_needed)` samples for **recent-window** tip BPS.
    /// Live 2026-07-15: lifetime tenure BPS stayed ≥11 over 300s while tip crawled at
    /// ~0.8 blk/s for a minute — A6m `min_bps=6` never fired (`IBD_A6M_ROTATE` count=0).
    tip_progress_samples: Mutex<VecDeque<(Instant, u64)>>,
    /// A6n: per-peer WAN tip GAP_STREAM counts (avoids bulk IBD contamination).
    peer_tip_streams: Mutex<HashMap<String, TipStreamWindow>>,
    /// dest-az 154k: last peer that crossed KEEP stream in this STREAM window.
    /// Survives decay / 600s roll. Not dest-aq 180s keep-hot — only blocks P1e 120s.
    last_stream_keep_hero: Mutex<Option<String>>,
    /// dest-az 179k: GD_SLOW `new=` pin time. First-H mute must not P1e-120s them.
    last_gd_slow_pin: Mutex<Option<(String, Instant)>>,
    /// C1c: sticky tip-hole GetData depth per peer across chunks (cold-start was always 8).
    /// Reset on mute tip fail. Opt out: `BLVM_IBD_TIP_HOLE_STICKY=0`.
    tip_hole_depth: Mutex<HashMap<String, usize>>,
    /// P2: active tip-role trial (challenger vs sticky).
    tip_trial: Mutex<Option<TipTrial>>,
    /// P2: cooldown between tip trials.
    last_tip_trial_at: Mutex<Option<Instant>>,
    /// After A6n GD_SLOW OPEN, tip SLA rearms to await=0 — trial then waited full
    /// `TIP_TRIAL_AWAIT` again (live Phase4: OPEN→TRIAL awaiting=6s). Boost window
    /// shortens await to 500ms so a challenger can steal sooner. Opt out:
    /// `BLVM_IBD_TIP_TRIAL_POST_OPEN_MS=0`.
    tip_trial_post_open_at: Mutex<Option<Instant>>,
    /// Highest stored header height (coordinator-refreshed). Tip pipes must not extend past this.
    header_tip: AtomicU64,
    /// Planned IBD end height (last chunk end / `effective_end_height`). When
    /// `validation_height >= ibd_end_height`, workers must exit — otherwise
    /// `wan_tip_gap_crawl` keeps `is_done()==false` forever past body tip and
    /// Phase 3 blocks on `download_handles.await` (live 2026-07-13 soak: 18+ min stall).
    ibd_end_height: AtomicU64,
    /// Explicit shutdown (IBD complete / teardown). Overrides tip-crawl keep-alive.
    shutdown: AtomicBool,
    /// Synth bulk: unix-ms when tip-owner assign was first refused because
    /// [`super::memory::GAP_STREAM_DEDUP_HEIGHT`] already covered `next_needed`.
    /// 0 = not blocking. Escape after grace while `tip_gap_missing` (true tip loss).
    synth_tip_dedup_block_since_ms: AtomicU64,
    /// R-28: last MUTE_DROP tip height. Same-H walk (R-26: 31 drops @323067 in 5 ms)
    /// is a cover leak, not 31 sits.
    last_mute_drop_h: AtomicU64,
    /// R-28: unix-ms of that drop. Same height may drop again after 8s if still mute.
    last_mute_drop_at_ms: AtomicU64,
    /// R-29: exclusive extra-pipe claims. Survive C1j abort / on_chunk_complete
    /// so the next get_work cannot stampede the same stripe (R-28: two peers
    /// 2364–2427 in 5 ms, then cheese @2366).
    latched_ahead: Mutex<Vec<(String, u64, u64)>>,
    /// R-248: satd priority zone — disjoint tiles in (H, H+256]. Not R-27 same
    /// stripe. Not 50k shuffle. C1j holds these like latch.
    priority_zone: Mutex<Vec<(String, u64, u64)>>,
    /// R-58: reserved disjoint lookahead stripes (cap 2). Not mesh extras / latch.
    /// One owner per stripe. Rank is STREAM on these heights, not throwaway probe.
    lookahead_stripes: Mutex<Vec<(String, u64, u64)>>,
    /// R-142: one fat-entry probe retitle per dest (180–210k window).
    fat_probe_retitle_done: AtomicBool,
    /// One farm-recv promote per dest (15s mute sticky + fat farm stripe).
    farm_recv_promote_done: AtomicBool,
    /// R-241 dump spent the one-shot @113k; fat 180–200k **144** had covering=0
    /// sticky recv 0.7 vs top **1021** with no second promote. Rearm once at 180k.
    farm_recv_fat_rearmed: AtomicBool,
    farm_recv_mute_streak: AtomicU32,
    farm_recv_last_tick: Mutex<Option<Instant>>,
    /// Entered farm ranges still ahead of apply. Stripes drop so farms leapfrog;
    /// hold stays reserved so evict/trim do not delete have (R-99 enter @10243).
    lookahead_have_hold: Mutex<Vec<(u64, u64)>>,
    lookahead_streams: Mutex<HashMap<String, TipStreamWindow>>,
}

/// Production returns `$prod`. Tests still read the env so a suite can arm a knob.
macro_rules! prod_or_test {
    ($prod:expr, $test:expr) => {{
        #[cfg(not(test))]
        {
            $prod
        }
        #[cfg(test)]
        {
            $test
        }
    }};
}

fn a6m_min_bps() -> f64 {
    prod_or_test!(
        6.0,
        std::env::var("BLVM_IBD_A6M_MIN_BPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6.0)
    )
}

/// Floor-sticky recent-window floor (score≈0.1). Live: floor≥75% of assigns → tip p50 **10.8**
/// blk/s vs **34.2** when floor&lt;25%. Used to **attempt** tip-proven candidate rotate.
///
/// Default **22** (stretch toward 45–80). Must NOT alone open-slot/blacklist — live 2026-07-15
/// `tenure_bps=12.57` OPEN_SLOT churned a historical-healthy sticky (band 12–18) into score=0.100
/// pins with no tip-proven alt above bar. See [`a6m_floor_open_slot_min_bps`].
fn a6m_floor_min_bps() -> f64 {
    prod_or_test!(
        22.0,
        std::env::var("BLVM_IBD_A6M_FLOOR_MIN_BPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(22.0)
    )
}

/// Only blacklist + open-slot when floor sticky recent BPS is below this **and** no tip-proven
/// alternate clears the A6n bar. Default **12** = prior healthy WAN floor (plan: floor≥75% →
/// tip p50 10.8; good stickies sat ~15–18). Env: `BLVM_IBD_A6M_FLOOR_OPEN_SLOT_MIN_BPS`.
fn a6m_floor_open_slot_min_bps() -> f64 {
    prod_or_test!(
        12.0,
        std::env::var("BLVM_IBD_A6M_FLOOR_OPEN_SLOT_MIN_BPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(12.0)
    )
}

fn a6m_tenure_secs() -> u64 {
    prod_or_test!(
        300,
        std::env::var("BLVM_IBD_A6M_TENURE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300)
    )
}

/// Recent tip BPS window for A6m (not lifetime tenure). Default **60**.
/// Live: 90s windows at ~18 blk/s never tripped floor_min=12; shorter window + higher
/// floor_min (22) opens slot while still below the 45–80 target.
fn a6m_recent_window_secs() -> u64 {
    prod_or_test!(
        60,
        std::env::var("BLVM_IBD_A6M_RECENT_WINDOW_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60)
            .clamp(30, 300)
    )
}

fn a6m_rotate_cooldown_secs() -> u64 {
    prod_or_test!(
        600,
        std::env::var("BLVM_IBD_A6M_ROTATE_COOLDOWN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(600)
    )
}

/// Cooldown after floor-sticky / open-slot rotate. Default **120** (F-P1).
fn a6m_floor_rotate_cooldown_secs() -> u64 {
    prod_or_test!(
        120,
        std::env::var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(120)
    )
}

/// A6m also arms when warm getdata→body EWMA exceeds this (ms), even if tip-advance
/// BPS looks healthy. Default **800**. E11: tip BPS~64 (LOCAL_GAP) masked sticky GetData
/// p50~1284 → `A6M_MIN_BPS=40` never fired. Env: `BLVM_IBD_A6M_MAX_GETDATA_MS`.
fn a6m_max_getdata_ms() -> u64 {
    prod_or_test!(
        800,
        std::env::var("BLVM_IBD_A6M_MAX_GETDATA_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(800)
            .clamp(200, 10_000)
    )
}

/// E16: skip GD_SLOW rotate/OPEN when feeder runway ≥ this (default **4**).
/// Live C1u CPU @320k: tip_bps≈179 from `TIP_LOCAL_STREAM` while getdata EWMA≈5.9s
/// → A6m blacklisted the hero, post-OPEN trial thrashed the pin, `no_challenger` ~20s.
/// E11 LOCAL_GAP false-health has feeder=0; keep when buffer proves crawl health.
/// E16b: default 8→4 — live C1u-e16 rotated at feeder=5 / tip_bps=162 after KEEP@29.
fn a6m_gd_slow_feeder_keep() -> usize {
    prod_or_test!(
        4,
        std::env::var("BLVM_IBD_A6M_GD_SLOW_FEEDER_KEEP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4usize)
            .clamp(0, 64)
    )
}

/// E16b: skip GD_SLOW rotate when recent *or stream* tip BPS ≥ this (default **0** = off).
/// Ship: KEEP-hold is opt-in (`BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP=80`). Live: tip_bps=162
/// feeder=5 ewma=554 (≥500) OPEN'd the hero → same cascade. dest-ax 226k: cheese sit
/// dropped height recent_bps below min while stream was 92; KEEP uses `wan_tip_stream_bps`.
fn a6m_gd_slow_tip_bps_keep() -> f64 {
    prod_or_test!(
        0.0,
        std::env::var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0)
            .clamp(0.0, 400.0)
    )
}

/// E16: seconds after A6m OPEN before tip trial may displace the new pin (default **8**).
/// Post-OPEN boost (`TIP_TRIAL_POST_OPEN_MS=500`) otherwise arms trials at await≈500ms
/// during handoff and cools the OPEN pin before first tip body.
fn tip_trial_post_open_settle_secs() -> u64 {
    prod_or_test!(
        8,
        std::env::var("BLVM_IBD_TIP_TRIAL_POST_OPEN_SETTLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8)
            .clamp(0, 30)
    )
}

/// W92 tip-owner cooldown after GD_SLOW rotate/open (default **180s**).
/// E12: OPEN_SLOT blacklisted sticky but `pinned=None` → TIP_PIN re-elected same peer
/// for 100% of tip-owner samples. Cooldown blocks `peer_may_take_tip_owner` even after
/// blacklist expires / when no score-map pin is available.
/// E13: 90s still allowed A↔B ping-pong (FORCE then ROTATE back within ~2m).
fn a6m_gd_slow_owner_cooldown_secs() -> u64 {
    prod_or_test!(
        180,
        std::env::var("BLVM_IBD_A6M_GD_SLOW_OWNER_COOLDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(180)
            .clamp(30, 600)
    )
}

/// Min tip-stream BPS for a GD_SLOW force-rotate target (default **20**).
/// E13: FORCE pinned `24.253…` at candidate_tip_bps=3.86 — better than sticky GetData
/// mask but not a real tip hero. Below this → OPEN_SLOT pin path instead.
fn a6m_gd_slow_force_min_tip_bps() -> f64 {
    prod_or_test!(
        20.0,
        std::env::var("BLVM_IBD_A6M_GD_SLOW_FORCE_MIN_TIP_BPS")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(20.0)
            .clamp(1.0_f64, 200.0_f64)
    )
}

/// dest-az: protect a just-pinned GD_SLOW `new=` from P1e 120s on first-H mute.
/// Default **30s** (live gap was 3s). Not dest-aq 180s keep-hot.
fn gd_slow_pin_protect_secs() -> u64 {
    prod_or_test!(
        30,
        std::env::var("BLVM_IBD_GD_SLOW_PIN_PROTECT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30)
            .clamp(8, 90)
    )
}

/// R-218 dump freeze: `feeder==0 && rtt>1500 && bps<150` is true for the
/// whole 0–50k hole (`tip_gd` 373s / sticky 2.5). RTT path stays fat-only.
const H_SLOW_MIN_HEIGHT: u64 = 180_000;
/// Dump warehouse rotate: R-222 5–6k **109 BPS** with `reorder=4096`,
/// `feeder=0`, GetData ewma **169ms** (RTT path silent). Same WIDTH as farms.
const DUMP_WAREHOUSE_REORDER: u64 = 2048;

/// R-219: default 15s `tip_owner_fail` expired and `74.167` re-won exclusive-H
/// four times (`[IBD_H_SLOW]` 10, 180–200k **60**). Mute already uses **120s**.
const H_SLOW_COOLDOWN_SECS: u64 = 120;

/// R-334: fat-RTT H rotation is **off** in production. Tests may arm
/// `BLVM_IBD_H_SLOW_ROTATE=1`. The predicate (`feeder==0 && gd>1500ms &&
/// bps<60`, ≥180k) was derived when tip GetData→body was ~700ms (R-213); in
/// the R-328–R-333 crawl p50 GetData→body is 4–8s and BPS 32–94, so it is
/// simply true: `[IBD_H_SLOW]` 18× in R-273 (1580s) vs 226/249/**333**× in
/// R-328/R-332/R-333, each one a 120s tip ban plus
/// `force_release_peer_inflight` — the owner's ranges leave the assigner
/// book and are re-issued elsewhere while its fetch task still has them in
/// flight (RESEND / PRIORITY_ZONE duplicates). `[IBD_MS_BREAKDOWN]` binder
/// `gd_slow` grew 31.8% → 39.5% → 46.8% of wall over the same runs. Same
/// negative-sign family as the closed aborts / 10s far-ahead re-request /
/// tip trials. Dump-warehouse rotate (<180k) is unchanged.
pub(crate) fn h_slow_rotate_enabled() -> bool {
    prod_or_test!(
        false,
        matches!(
            std::env::var("BLVM_IBD_H_SLOW_ROTATE").ok().as_deref(),
            Some("1") | Some("true") | Some("on") | Some("yes")
        )
    )
}

static LAST_H_SLOW_GD_MS: AtomicU64 = AtomicU64::new(0);

/// Height whose quiet tip cover has already been re-raced once (R-273). One racer per H.
static TIP_RERACE_HEIGHT: AtomicU64 = AtomicU64::new(0);

/// Exclusive-H GetData analog of `header_batch_rtt_should_rotate`.
///
/// R-218: same sample (`rtt_ms=762279`) rotated 24× and left `sticky=-`.
/// One shot per `last_getdata_body_ms`. Dump RTT path stays off.
///
/// Replay:
/// - R-218 h=66, 373s, bps 2.5, feeder 0 → false
/// - R-213 200–248k: 700ms / 300 BPS → false
/// - R-250 191773: 1716ms / **115.3** BPS → **false** (was true at cutoff 150)
/// - R-250 194348: 1540ms / **16.1** BPS → true (mute still rotates)
/// - R-217 2750ms / 70 BPS → **false** (70 is line-rate; cutoff is 60 not 150)
pub(crate) fn body_h_rtt_should_rotate(
    latency_ms: f64,
    sticky_bps: f64,
    feeder: u64,
    next_needed: u64,
    already_rotated_this_gd: bool,
) -> bool {
    next_needed >= H_SLOW_MIN_HEIGHT
        && feeder == 0
        && latency_ms > 1500.0
        && sticky_bps < 60.0
        && !already_rotated_this_gd
}

/// R-224: `[IBD_H_SLOW]` rotated `73.164` (sticky_recv **64.9** > top **42.4**)
/// onto farm-score `216.230` → 194–195k **34 BPS**. Recv leader is already
/// the H pipe. Unknown cache is not a hold (R-217 2750/70 still rotates).
pub(crate) fn h_slow_recv_leader_holds(sticky_recv_mbps: f64, top_recv_mbps: f64) -> bool {
    sticky_recv_mbps >= top_recv_mbps
}

/// R-226 190–200k **176**: H-slow pinned score-successor `18.194`, then sticky
/// became `36.225` recv **0.1** vs farms **947–2053**. Only rotate onto a
/// ready worker whose CRAWL recv is **strictly above** sticky. Equal/unknown
/// cand is not a beat (hold the pipe).
pub(crate) fn h_slow_recv_successor_beats(sticky_recv_mbps: f64, cand_recv_mbps: f64) -> bool {
    cand_recv_mbps > sticky_recv_mbps
}

/// Dump Face-2: farms already hold ≥WIDTH while apply sleeps on H.
/// R-218 empty-hole (`reorder=0`, 373s GetData) stays off. Flood dump
/// (`bps≥150`) stays off. One shot per GetData sample + successor pin.
pub(crate) fn body_dump_warehouse_should_rotate(
    sticky_bps: f64,
    feeder: u64,
    next_needed: u64,
    reorder_ahead: u64,
    already_rotated_this_gd: bool,
) -> bool {
    next_needed < H_SLOW_MIN_HEIGHT
        && feeder == 0
        && sticky_bps < 150.0
        && reorder_ahead >= DUMP_WAREHOUSE_REORDER
        && !already_rotated_this_gd
}

#[cfg(test)]
pub(crate) fn test_reset_h_slow_gd() {
    LAST_H_SLOW_GD_MS.store(0, Ordering::Relaxed);
}

include!("chunk_assigner_parts/impl_assign.rs");
include!("chunk_assigner_parts/impl_tip_hole.rs");
include!("chunk_assigner_parts/impl_flight.rs");
include!("chunk_assigner_parts/impl_window.rs");

/// Re-queues chunk on drop if not disarmed. Prevents chunk loss on panic/task-cancel/any exit.
pub(crate) struct ChunkGuard {
    chunk: Option<ChunkWorkItem>,
    peer_id: Option<String>,
    assigner: Arc<ChunkAssigner>,
}

impl ChunkGuard {
    pub(crate) fn new(
        start: u64,
        end: u64,
        exclude: Option<String>,
        peer_id: String,
        assigner: Arc<ChunkAssigner>,
    ) -> Self {
        Self {
            chunk: Some((start, end, exclude)),
            peer_id: Some(peer_id),
            assigner,
        }
    }
    pub(crate) fn disarm(&mut self) {
        self.chunk = None;
        self.peer_id = None; // Don't call on_chunk_complete on Drop; caller will do it
    }
}

impl Drop for ChunkGuard {
    fn drop(&mut self) {
        // Clear the *this* range (not LIFO pop). A4 dual in-flight: popping the wrong
        // range orphans tip cover (live: tip 513-576 cleared while peer still downloading).
        if let Some((start, end, exclude)) = self.chunk.take() {
            if let Some(peer_id) = self.peer_id.take() {
                self.assigner.on_chunk_complete_range(&peer_id, start, end);
            }
            self.assigner
                .requeue_reason(start, end, exclude, "guard_drop");
        } else if let Some(peer_id) = self.peer_id.take() {
            self.assigner.on_chunk_complete(&peer_id);
        }
    }
}

#[cfg(test)]
#[path = "chunk_assigner_tests.rs"]
mod tests;
