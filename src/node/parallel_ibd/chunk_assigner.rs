//! ChunkAssigner assigns height-ordered chunks to workers. ChunkGuard ensures
//! chunks are re-queued on drop if not disarmed.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::ParallelIBDConfig;
use super::latch_env;
use super::types::ChunkWorkItem;

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

/// W4/N12: `get_work` `in_flight_per_peer` wait/hold (`BLVM_IBD_ASSIGNER_LOCK_TIMERS=1`).
static ASSIGNER_GW_WAIT_NS: AtomicU64 = AtomicU64::new(0);
static ASSIGNER_GW_HOLD_NS: AtomicU64 = AtomicU64::new(0);
static ASSIGNER_GW_SAMPLES: AtomicU64 = AtomicU64::new(0);
/// A31/A42: last frontier-dual arm (ms). Module-level so unit tests can reset.
static A31_FRONTIER_DUAL_LAST_ARM_MS: AtomicU64 = AtomicU64::new(0);
/// A51: last sole-EMPTY tip release (ms). Module-level so unit tests can reset.
static A51_SOLE_EMPTY_LAST_RELEASE_MS: AtomicU64 = AtomicU64::new(0);

fn assigner_lock_timers_enabled() -> bool {
    matches!(
        std::env::var("BLVM_IBD_ASSIGNER_LOCK_TIMERS")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
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
    retry_queue: Mutex<VecDeque<ChunkWorkItem>>,
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
    /// Bridge holes in `[next_expected, …]` from coordinator (`pending_diag`). Used to gate
    /// multi-peer tip-band ahead — **not** steady-state `gap_missing` (always true while
    /// waiting for the next body). Live 2026-07-15: `allow_ahead=!gap_missing` → covering=1
    /// forever with ready≈52 → tip ~5 blk/s vs ≥80 target.
    tip_bridge_holes: AtomicU64,
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
}

fn a6m_min_bps() -> f64 {
    std::env::var("BLVM_IBD_A6M_MIN_BPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6.0)
}

/// Floor-sticky recent-window floor (score≈0.1). Live: floor≥75% of assigns → tip p50 **10.8**
/// blk/s vs **34.2** when floor&lt;25%. Used to **attempt** tip-proven candidate rotate.
///
/// Default **22** (stretch toward 45–80). Must NOT alone open-slot/blacklist — live 2026-07-15
/// `tenure_bps=12.57` OPEN_SLOT churned a historical-healthy sticky (band 12–18) into score=0.100
/// pins with no tip-proven alt above bar. See [`a6m_floor_open_slot_min_bps`].
fn a6m_floor_min_bps() -> f64 {
    std::env::var("BLVM_IBD_A6M_FLOOR_MIN_BPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(22.0)
}

/// Only blacklist + open-slot when floor sticky recent BPS is below this **and** no tip-proven
/// alternate clears the A6n bar. Default **12** = prior healthy WAN floor (plan: floor≥75% →
/// tip p50 10.8; good stickies sat ~15–18). Env: `BLVM_IBD_A6M_FLOOR_OPEN_SLOT_MIN_BPS`.
fn a6m_floor_open_slot_min_bps() -> f64 {
    std::env::var("BLVM_IBD_A6M_FLOOR_OPEN_SLOT_MIN_BPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12.0)
}

fn a6m_tenure_secs() -> u64 {
    std::env::var("BLVM_IBD_A6M_TENURE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300)
}

/// Recent tip BPS window for A6m (not lifetime tenure). Default **60**.
/// Live: 90s windows at ~18 blk/s never tripped floor_min=12; shorter window + higher
/// floor_min (22) opens slot while still below the 45–80 target.
fn a6m_recent_window_secs() -> u64 {
    std::env::var("BLVM_IBD_A6M_RECENT_WINDOW_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60)
        .clamp(30, 300)
}

fn a6m_rotate_cooldown_secs() -> u64 {
    std::env::var("BLVM_IBD_A6M_ROTATE_COOLDOWN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600)
}

/// Cooldown after floor-sticky / open-slot rotate. Default **120** (F-P1).
fn a6m_floor_rotate_cooldown_secs() -> u64 {
    std::env::var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120)
}

/// A6m also arms when warm getdata→body EWMA exceeds this (ms), even if tip-advance
/// BPS looks healthy. Default **800**. E11: tip BPS~64 (LOCAL_GAP) masked sticky GetData
/// p50~1284 → `A6M_MIN_BPS=40` never fired. Env: `BLVM_IBD_A6M_MAX_GETDATA_MS`.
fn a6m_max_getdata_ms() -> u64 {
    std::env::var("BLVM_IBD_A6M_MAX_GETDATA_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(800)
        .clamp(200, 10_000)
}

/// E16: skip GD_SLOW rotate/OPEN when feeder runway ≥ this (default **4**).
/// Live C1u CPU @320k: tip_bps≈179 from `TIP_LOCAL_STREAM` while getdata EWMA≈5.9s
/// → A6m blacklisted the hero, post-OPEN trial thrashed the pin, `no_challenger` ~20s.
/// E11 LOCAL_GAP false-health has feeder=0; keep when buffer proves crawl health.
/// E16b: default 8→4 — live C1u-e16 rotated at feeder=5 / tip_bps=162 after KEEP@29.
fn a6m_gd_slow_feeder_keep() -> usize {
    std::env::var("BLVM_IBD_A6M_GD_SLOW_FEEDER_KEEP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4usize)
        .clamp(0, 64)
}

/// E16b: skip GD_SLOW rotate when recent tip BPS ≥ this (default **80**), even if
/// feeder briefly dips below keep. Live: tip_bps=162 feeder=5 ewma=554 (≥500) OPEN'd
/// the hero → same cascade. E11 illusory health was ~64 BPS with feeder=0.
fn a6m_gd_slow_tip_bps_keep() -> f64 {
    std::env::var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(80.0)
        .clamp(0.0, 400.0)
}

/// E16: seconds after A6m OPEN before tip trial may displace the new pin (default **8**).
/// Post-OPEN boost (`TIP_TRIAL_POST_OPEN_MS=500`) otherwise arms trials at await≈500ms
/// during handoff and cools the OPEN pin before first tip body.
fn tip_trial_post_open_settle_secs() -> u64 {
    std::env::var("BLVM_IBD_TIP_TRIAL_POST_OPEN_SETTLE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8)
        .clamp(0, 30)
}

/// W92 tip-owner cooldown after GD_SLOW rotate/open (default **180s**).
/// E12: OPEN_SLOT blacklisted sticky but `pinned=None` → TIP_PIN re-elected same peer
/// for 100% of tip-owner samples. Cooldown blocks `peer_may_take_tip_owner` even after
/// blacklist expires / when no score-map pin is available.
/// E13: 90s still allowed A↔B ping-pong (FORCE then ROTATE back within ~2m).
fn a6m_gd_slow_owner_cooldown_secs() -> u64 {
    std::env::var("BLVM_IBD_A6M_GD_SLOW_OWNER_COOLDOWN_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(180)
        .clamp(30, 600)
}

/// Min tip-stream BPS for a GD_SLOW force-rotate target (default **20**).
/// E13: FORCE pinned `24.253…` at candidate_tip_bps=3.86 — better than sticky GetData
/// mask but not a real tip hero. Below this → OPEN_SLOT pin path instead.
fn a6m_gd_slow_force_min_tip_bps() -> f64 {
    std::env::var("BLVM_IBD_A6M_GD_SLOW_FORCE_MIN_TIP_BPS")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(20.0)
        .clamp(1.0_f64, 200.0_f64)
}

impl ChunkAssigner {
    /// Deduplicate peer ids while preserving first-seen order.
    fn dedupe_workers(peers: Vec<String>) -> Vec<String> {
        let mut seen = HashSet::new();
        peers
            .into_iter()
            .filter(|p| !p.is_empty() && seen.insert(p.clone()))
            .collect()
    }

    /// Height ranges + active workers (independent lengths).
    ///
    /// - `work_stealing`: `workers` is only the active download set (deduped); no affinity.
    /// - `!work_stealing` and `workers.len() == chunks.len()`: treat `workers` as
    ///   preferred-per-range affinity (LAN / legacy call sites), then dedupe for membership.
    /// - otherwise: dedupe workers and round-robin affinity onto ranges.
    ///
    /// Prefer [`Self::from_block_chunks`] when constructing from [`create_chunks`].
    pub(crate) fn new(
        chunks: Vec<(u64, u64)>,
        workers: Vec<String>,
        validation_height: Arc<std::sync::atomic::AtomicU64>,
        start_height: u64,
        work_stealing: bool,
    ) -> Self {
        let (workers, preferred_peers) = if work_stealing {
            (Self::dedupe_workers(workers), Vec::new())
        } else if workers.len() == chunks.len() && !workers.is_empty() {
            let preferred = workers;
            let workers = Self::dedupe_workers(preferred.clone());
            (workers, preferred)
        } else {
            let workers = Self::dedupe_workers(workers);
            let preferred = if workers.is_empty() {
                Vec::new()
            } else {
                let n = workers.len();
                (0..chunks.len())
                    .map(|i| workers[i % n].clone())
                    .collect()
            };
            (workers, preferred)
        };
        Self::build(
            chunks,
            workers,
            preferred_peers,
            validation_height,
            start_height,
            work_stealing,
        )
    }

    /// From [`create_chunks`] / planned [`BlockChunk`] list. Workers = unique peer ids;
    /// preferred affinity kept for LAN (`!work_stealing`).
    pub(crate) fn from_block_chunks(
        planned: &[BlockChunk],
        validation_height: Arc<std::sync::atomic::AtomicU64>,
        start_height: u64,
        work_stealing: bool,
    ) -> Self {
        let chunks: Vec<(u64, u64)> = planned
            .iter()
            .map(|c| (c.start_height, c.end_height))
            .collect();
        let preferred: Vec<String> = planned.iter().map(|c| c.peer_id.clone()).collect();
        let workers = Self::dedupe_workers(preferred.clone());
        let preferred_peers = if work_stealing {
            Vec::new()
        } else {
            preferred
        };
        Self::build(
            chunks,
            workers,
            preferred_peers,
            validation_height,
            start_height,
            work_stealing,
        )
    }

    fn build(
        chunks: Vec<(u64, u64)>,
        workers: Vec<String>,
        preferred_peers: Vec<String>,
        validation_height: Arc<std::sync::atomic::AtomicU64>,
        start_height: u64,
        work_stealing: bool,
    ) -> Self {
        debug_assert!(
            work_stealing
                || preferred_peers.is_empty()
                || preferred_peers.len() == chunks.len(),
            "preferred_peers must match chunks when set"
        );
        // Always serialize the first chunk (contains `start_height`) before assigning later chunks.
        // On near-tip resume (`start_height > 0`) there is only one chunk — treat bootstrap done so
        // workers are not blocked waiting for a bootstrap marker that only applies to height 0.
        let bootstrap_complete = AtomicBool::new(start_height > 0);
        let ibd_end = chunks.iter().map(|(_, e)| *e).max().unwrap_or(0);
        Self {
            chunks,
            workers,
            extra_workers: Mutex::new(HashSet::new()),
            preferred_peers,
            next_index: AtomicUsize::new(0),
            retry_queue: Mutex::new(VecDeque::new()),
            validation_height,
            bootstrap_complete,
            start_height,
            in_flight_per_peer: Mutex::new(HashMap::new()),
            work_stealing,
            blacklisted_until: Mutex::new(HashMap::new()),
            last_stall_requeue: Mutex::new(None),
            peer_scores: Mutex::new(HashMap::new()),
            confirmed_body_height_at_start: AtomicU64::new(0),
            wan_body_tip: AtomicU64::new(0),
            tip_gap_missing: AtomicBool::new(false),
            tip_bridge_holes: AtomicU64::new(0),
            preferred_tip_owner: Mutex::new(None),
            tip_owner_fail_until: Mutex::new(HashMap::new()),
            tip_cover_claims: Mutex::new(Vec::new()),
            tip_owner_open: AtomicBool::new(false),
            tip_failover_once_h: AtomicU64::new(0),
            tip_failover_once_at_ms: AtomicU64::new(0),
            tip_ahead_hole_freeze: AtomicBool::new(false),
            tip_ahead_hole_clear_since_ms: AtomicU64::new(0),
            ibd_ready_peers: Mutex::new(HashSet::new()),
            sticky_wan_tenure: Mutex::new(None),
            last_a6m_rotate_at: Mutex::new(None),
            tip_progress_samples: Mutex::new(VecDeque::new()),
            peer_tip_streams: Mutex::new(HashMap::new()),
            tip_hole_depth: Mutex::new(HashMap::new()),
            tip_trial: Mutex::new(None),
            last_tip_trial_at: Mutex::new(None),
            tip_trial_post_open_at: Mutex::new(None),
            header_tip: AtomicU64::new(0),
            ibd_end_height: AtomicU64::new(ibd_end),
            shutdown: AtomicBool::new(false),
            synth_tip_dedup_block_since_ms: AtomicU64::new(0),
        }
    }

    /// C1c/C1k: sticky tip-hole depth across chunks for the same tip peer.
    ///
    /// Pre-C1i: sticky@32 REGRESS wall≈16 (cheese / ahead_ratio≈23). With C1i min_contig
    /// + C1j abort, harness retries default **on** (`BLVM_IBD_TIP_HOLE_STICKY=1`) — grow
    /// was stuck at p50=8 every chunk (4× depth binder vs cap 32). Opt out: `=0`.
    pub(crate) fn tip_hole_sticky_enabled() -> bool {
        match std::env::var("BLVM_IBD_TIP_HOLE_STICKY")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("0") | Some("false") | Some("off") | Some("no") => false,
            Some("1") | Some("true") | Some("on") | Some("yes") => true,
            // Unset: off in unit tests / non-harness; harness exports =1.
            _ => false,
        }
    }

    /// C1c/C1s: starting tip-hole depth for `peer_id` (sticky or grow_start).
    /// C1s: do **not** clamp sticky to live gd-fast effective cap — EWMA dips were
    /// ratcheting 48→32 every few chunks (live C1r: repeated `32→40→48`). Fill still
    /// mins against effective cap each request.
    pub(crate) fn tip_hole_depth_for(&self, peer_id: &str) -> usize {
        let start = super::download::tip_hole_grow_start();
        if !Self::tip_hole_sticky_enabled() {
            return start;
        }
        let abs_cap = super::download::tip_hole_sticky_abs_cap(
            self.peer_is_hot_tip_streamer(peer_id),
        );
        let g = self.tip_hole_depth.lock().unwrap();
        g.get(peer_id)
            .copied()
            .unwrap_or(start)
            .max(start)
            .min(abs_cap)
    }

    /// C1c/C1s: persist grown tip-hole depth (ratchet **up** only; mute → reset).
    pub(crate) fn note_tip_hole_depth(&self, peer_id: &str, depth: usize) {
        if !Self::tip_hole_sticky_enabled() {
            return;
        }
        let start = super::download::tip_hole_grow_start();
        let abs_cap = super::download::tip_hole_sticky_abs_cap(
            self.peer_is_hot_tip_streamer(peer_id),
        );
        let mut g = self.tip_hole_depth.lock().unwrap();
        let prev = g.get(peer_id).copied().unwrap_or(0);
        // Ratchet up only — never shrink sticky on a soft EWMA dip.
        let d = depth.clamp(start, abs_cap).max(prev.min(abs_cap));
        if prev == d {
            return;
        }
        g.insert(peer_id.to_string(), d);
        tracing::info!(
            "[IBD_TIP_HOLE_STICKY] peer={} depth {}→{} (abs_cap={})",
            peer_id,
            prev,
            d,
            abs_cap
        );
    }

    /// C1c: clear sticky depth on mute (do not reopen Swiss-cheese on a mute hero).
    pub(crate) fn reset_tip_hole_depth(&self, peer_id: &str) {
        let mut g = self.tip_hole_depth.lock().unwrap();
        if g.remove(peer_id).is_some() {
            tracing::info!(
                "[IBD_TIP_HOLE_STICKY] peer={} reset (mute/fail)",
                peer_id
            );
        }
    }

    /// C1u: hard GD_SLOW may shrink sticky (C1s forbids soft-EWMA shrink only).
    pub(crate) fn clamp_tip_hole_depth(&self, peer_id: &str, depth: usize) {
        if !Self::tip_hole_sticky_enabled() {
            return;
        }
        let start = super::download::tip_hole_grow_start();
        let d = depth.max(start).min(super::download::tip_hole_sticky_abs_cap(
            self.peer_is_hot_tip_streamer(peer_id),
        ));
        let mut g = self.tip_hole_depth.lock().unwrap();
        let prev = g.get(peer_id).copied().unwrap_or(0);
        if prev == 0 || prev <= d {
            return;
        }
        g.insert(peer_id.to_string(), d);
        tracing::info!(
            "[IBD_TIP_HOLE_STICKY] peer={} depth {}→{} (gd_slow clamp)",
            peer_id,
            prev,
            d
        );
    }

    /// Override / extend planned IBD end (e.g. tip-follow). Used for worker exit gating.
    pub(crate) fn set_ibd_end_height(&self, end: u64) {
        self.ibd_end_height.store(end, Ordering::Relaxed);
    }

    /// Signal all download workers to exit. Clears tip-gap keep-alive so `is_done()`
    /// becomes true even when past on-disk body tip.
    pub(crate) fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.tip_gap_missing.store(false, Ordering::Relaxed);
        super::tip_stage::clear_tip_failover();
        tracing::info!(
            "[IBD_ASSIGNER_SHUTDOWN] download workers signaled to exit (vh={} end={})",
            self.validation_height.load(Ordering::Relaxed),
            self.ibd_end_height.load(Ordering::Relaxed)
        );
    }

    /// True once IBD validation has reached the planned end height.
    fn validation_reached_ibd_end(&self) -> bool {
        let end = self.ibd_end_height.load(Ordering::Relaxed);
        end > 0 && self.validation_height.load(Ordering::Relaxed) >= end
    }

    /// Coordinator: refresh highest stored header so tip assigns never exceed available hashes.
    pub(crate) fn set_header_tip(&self, tip: u64) {
        self.header_tip.store(tip, Ordering::Relaxed);
    }

    pub(crate) fn header_tip(&self) -> u64 {
        self.header_tip.load(Ordering::Relaxed)
    }

    /// Best available tip reference for bulk-catchup gating: live stored headers, else IBD end.
    /// `header_tip` stays 0 until the first peer-ready refresh — without this fallback, mid-chain
    /// catch-up is misclassified as near-tip crawl (max_ahead=256, ahead peers blocked).
    fn header_tip_for_bulk(&self) -> u64 {
        self.header_tip()
            .max(self.ibd_end_height.load(Ordering::Relaxed))
    }

    /// Past body tip but still far from header tip — multi-peer catch-up (not tip crawl).
    fn wan_bulk_catchup(&self, next_needed: u64) -> bool {
        self.wan_tip_gap_crawl(next_needed)
            && super::wan_bulk_catchup(self.header_tip_for_bulk(), next_needed)
    }

    /// Clip a tip/ahead end height to the last stored header (0 = unknown → no clip).
    fn clip_end_to_headers(&self, start: u64, end: u64) -> Option<(u64, u64)> {
        let ht = self.header_tip();
        if ht == 0 {
            return Some((start, end));
        }
        if start > ht {
            return None;
        }
        Some((start, end.min(ht)))
    }

    /// P0-A: refresh from coordinator (async network poll → sync snapshot).
    pub(crate) fn set_ibd_ready_peers(&self, ready: HashSet<String>) {
        *self.ibd_ready_peers.lock().unwrap() = ready;
    }

    /// Peer ids scored for IBD — used to refresh the ready snapshot.
    pub(crate) fn peer_ids_for_ibd_ready(&self) -> Vec<String> {
        self.peer_scores.lock().unwrap().keys().cloned().collect()
    }

    /// P0-A: on WAN tip gap, tip-owner paths require a live handshook peer.
    /// Empty ready snapshot → deny (no allow-all carousel on dead owners).
    /// Dense-local unit setups (`wan_body_tip=0` and no coordinator `header_tip`) skip the
    /// ready gate so W16 mid-chunk tip-fill / main-queue tip-cover still assign.
    fn peer_is_ibd_ready(&self, peer_id: &str) -> bool {
        let next_needed = self.next_needed_height();
        if !self.wan_tip_gap_crawl(next_needed) {
            return true;
        }
        if self.wan_body_tip.load(Ordering::Relaxed) == 0 && self.header_tip() == 0 {
            return true;
        }
        let ready = self.ibd_ready_peers.lock().unwrap();
        if ready.is_empty() {
            return false;
        }
        ready.contains(peer_id)
    }

    /// True when `peer_id` is an ACTIVE_PEERS download worker (listed in workers).
    fn is_active_download_worker(&self, peer_id: &str) -> bool {
        self.workers.iter().any(|p| p == peer_id)
            || self.extra_workers.lock().unwrap().contains(peer_id)
    }

    /// Active download worker peer ids (construction set + peer-watcher replacements).
    pub(crate) fn active_download_worker_ids(&self) -> Vec<String> {
        let mut out = self.workers.clone();
        let extra = self.extra_workers.lock().unwrap();
        for p in extra.iter() {
            if !out.iter().any(|w| w == p) {
                out.push(p.clone());
            }
        }
        out
    }

    /// Register a download worker spawned after assigner construction (peer watcher).
    /// Without this, tip-owner / OPEN_STALL / mid-clear treat the peer as non-worker
    /// even while it polls `get_work` — live freeze: ready>0, ready_active_ok=0/0.
    pub(crate) fn register_download_worker(&self, peer_id: &str) {
        if peer_id.is_empty() || self.workers.iter().any(|p| p == peer_id) {
            return;
        }
        self.extra_workers
            .lock()
            .unwrap()
            .insert(peer_id.to_string());
    }

    /// P0-A A8: observability when a WAN tip owner is assigned.
    fn log_wan_tip_owner_ready(&self, peer_id: &str, start: u64, end: u64) {
        if !self.wan_tip_gap_crawl(start) {
            return;
        }
        let ready = self.peer_is_ibd_ready(peer_id);
        let worker = self.is_active_download_worker(peer_id);
        let score = self
            .peer_scores
            .lock()
            .unwrap()
            .get(peer_id)
            .copied()
            .unwrap_or(0.0);
        tracing::warn!(
            "[IBD_TIP_PEER_READY] owner={} ibd_ready={} active_worker={} score={:.3} span={}-{}",
            peer_id,
            ready,
            worker,
            score,
            start,
            end
        );
    }

    /// Observability: how many peers are currently marked IBD-ready.
    pub(crate) fn ibd_ready_peer_count(&self) -> usize {
        self.ibd_ready_peers.lock().unwrap().len()
    }

    /// WAN tip crawl: validation past on-disk body tip (live soak past ~689k).
    ///
    /// Gates on [`Self::wan_body_tip`] (coordinator live tip), not contiguous `confirmed`
    /// alone — see field docs. `confirmed > 0` was wrong for genesis (`confirmed=0`).
    fn wan_tip_gap_crawl(&self, next_needed: u64) -> bool {
        let body_tip = self.wan_body_tip.load(Ordering::Relaxed);
        self.bootstrap_complete.load(Ordering::Relaxed) && next_needed > body_tip
    }

    /// P0-B/W84: on WAN tip crawl, suppress *ahead* stall micro storms — but always allow
    /// micro recovery for the **exact tip height**.
    ///
    /// Pre-W84: `!wan_tip_gap_crawl(height)` made every stall requeue a no-op during
    /// genesis/WAN (`body_tip=0`). Live freeze tip=256687 (~8+ min): `IBD_STALL` while
    /// Case C saw covering≥1 (zombie) so skipped [`Self::requeue_stall_gaps_force`], and
    /// `wan_stall_micro_allowed` blocked the non-force path → tip never re-armed.
    ///
    /// W73 force path still covers covering=0. Debounce inside `requeue_stall_gaps` remains.
    pub(crate) fn wan_stall_micro_allowed(&self, height: u64) -> bool {
        if !self.wan_tip_gap_crawl(height) {
            return true;
        }
        height == self.next_needed_height()
    }

    pub(crate) fn set_confirmed_body_height_at_start(&self, height: u64) {
        self.confirmed_body_height_at_start
            .store(height, Ordering::Relaxed);
        // Default WAN tip gate to confirmed; raise to live/sparse tip via [`Self::set_wan_body_tip`].
        self.set_wan_body_tip(height);
    }

    /// Align assigner WAN tip crawl with coordinator `live_body_tip` (confirmed ∨ sparse ∨ probe).
    pub(crate) fn set_wan_body_tip(&self, height: u64) {
        self.wan_body_tip.store(height, Ordering::Relaxed);
        // Land E: validation / index consult tip-crawl supply via tip_stage mirror.
        super::tip_stage::publish_wan_body_tip(height);
    }

    /// How many local blocks before `body_tip` to start GetData-priming past tip.
    ///
    /// Binder soak (2026-07-28): local ahead ~450–690 BPS then cliff at body tip
    /// (`SUPPLY_TIP_HOLE` / `SUPPLY_GD_SLOW`, gd_ewma 5–9s). Priming `body_tip+1..`
    /// while still validating local bodies warms the tip pipe. Default **64**.
    /// Opt out: `BLVM_IBD_HANDOFF_PRIME=0`.
    /// Default **256** — local ahead fills to body tip while validation is still hundreds
    /// behind at ~500 BPS; 64 was only ~130ms and missed the window (C1u soak 2026-07-28).
    pub(crate) fn handoff_prime_blocks() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_HANDOFF_PRIME")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(256)
                .min(1024)
        })
    }

    /// True while validating inside the last `HANDOFF_PRIME` local bodies before WAN tip.
    pub(crate) fn handoff_prime_active(&self, next_needed: u64) -> bool {
        let prime = Self::handoff_prime_blocks();
        if prime == 0 || !self.bootstrap_complete.load(Ordering::Relaxed) {
            return false;
        }
        // Synth delay=0: bodies stay local — never open network prime.
        if super::synthetic_wan::enabled() && super::synthetic_wan::getdata_delay_ms() == 0 {
            return false;
        }
        let body_tip = self.wan_body_tip.load(Ordering::Relaxed);
        if body_tip == 0 || next_needed > body_tip {
            return false;
        }
        next_needed.saturating_add(prime) > body_tip
    }

    /// Tip-owner GetData for `body_tip+1..+stripe` while still local (C1u).
    /// Returns assigned range when inserted into `guard`; caller must `drop(guard)` + return.
    fn try_assign_handoff_prime(
        &self,
        peer_id: &str,
        next_needed: u64,
        guard: &mut HashMap<String, Vec<(u64, u64)>>,
        reason: &str,
    ) -> Option<(u64, u64)> {
        if Self::handoff_prime_blocks() == 0
            || !self.bootstrap_complete.load(Ordering::Relaxed)
            || (super::synthetic_wan::enabled() && super::synthetic_wan::getdata_delay_ms() == 0)
        {
            return None;
        }
        let body_tip = self.wan_body_tip.load(Ordering::Relaxed);
        if body_tip == 0 || next_needed > body_tip {
            return None;
        }
        let prime_start = body_tip.saturating_add(1);
        let stripe = Self::tip_runway_stripe();
        let mut prime_end = prime_start.saturating_add(stripe.saturating_sub(1));
        let headers_ok = match self.clip_end_to_headers(prime_start, prime_end) {
            Some((_, clipped)) => {
                prime_end = clipped;
                true
            }
            None => false,
        };
        if !headers_ok || prime_end < prime_start {
            return None;
        }
        let prime_covering = Self::covering_next_count(guard, prime_start);
        let claim_hit = {
            let claims = self.tip_cover_claims.lock().unwrap();
            claims
                .iter()
                .any(|(_, s, e)| *s <= prime_end && prime_start <= *e)
        };
        // Sticky tip-owner preferred; if at flight cap on local tip, idle top-scored
        // ready worker may prime (local ahead reaches body tip while sticky is busy).
        let may_prime = self.peer_may_take_tip_owner(peer_id, guard, 0)
            || self.handoff_prime_fallback_ok(peer_id, guard);
        // Never prime body_tip+1 while next_needed < body_tip — sticky max_in_flight=1
        // steals onto WAN tip while local gap remains (C0 T025719Z freeze wait 304664
        // @ next=304649; also blocks ahead_frontier when cover spans to body_tip).
        if next_needed < body_tip {
            return None;
        }
        // near_tip: also require local tip height covered in-flight.
        if reason == "near_tip" && Self::covering_next_count(guard, next_needed) == 0 {
            return None;
        }
        if prime_covering != 0
            || claim_hit
            || !may_prime
            || Self::range_overlaps_inflight(guard, prime_start, prime_end)
            || Self::peer_holds_tip_inflight(guard, peer_id, prime_start)
        {
            return None;
        }
        let seed = super::download::tip_hole_grow_start()
            .max(32)
            .min(super::download::tip_hole_grow_cap().max(32));
        self.note_tip_hole_depth(peer_id, seed);
        Self::insert_in_flight(guard, peer_id, prime_start, prime_end);
        self.note_tip_cover_claim(peer_id, prime_start, prime_end);
        self.note_tip_owner_assigned(peer_id);
        tracing::warn!(
            "[IBD_HANDOFF_PRIME] peer={} priming {}-{} while next_needed={} body_tip={} seed_depth={} reason={} — GetData warm before WAN tip",
            peer_id,
            prime_start,
            prime_end,
            next_needed,
            body_tip,
            seed,
            reason
        );
        Some((prime_start, prime_end))
    }

    /// When the tip-covering peer is at capacity on local bodies, allow one idle
    /// top-scored ready worker to GetData-prime past body tip (C1u ahead_frontier).
    fn handoff_prime_fallback_ok(
        &self,
        peer_id: &str,
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
    ) -> bool {
        if !self.peer_ok_for_gap_race(peer_id)
            || !self.peer_is_ibd_ready(peer_id)
            || !self.is_active_download_worker(peer_id)
            || !self.peer_has_flight_capacity(peer_id, in_flight)
        {
            return false;
        }
        let next_needed = self.validation_height.load(Ordering::Relaxed).saturating_add(1);
        // Tip already covered in-flight, and that cover peer has no spare slot.
        let tip_cover_busy = in_flight.iter().any(|(p, ranges)| {
            ranges.iter().any(|(s, e)| *s <= next_needed && next_needed <= *e)
                && !self.peer_has_flight_capacity(p, in_flight)
        });
        if !tip_cover_busy {
            return false;
        }
        let preferred = self.preferred_tip_owner.lock().unwrap().clone();
        let scores = self.peer_scores.lock().unwrap();
        let best_idle = scores
            .iter()
            .filter(|(p, _)| {
                // Skip tip-covering peers (busy on local tip).
                !in_flight.get(*p).is_some_and(|ranges| {
                    ranges
                        .iter()
                        .any(|(s, e)| *s <= next_needed && next_needed <= *e)
                }) && preferred.as_ref().map(|pref| pref != *p).unwrap_or(true)
                    && self.is_active_download_worker(p)
                    && self.peer_is_ibd_ready(p)
                    && !self.is_peer_blacklisted(p)
                    && self.peer_has_flight_capacity(p, in_flight)
            })
            .max_by(|(a, sa), (b, sb)| {
                sa.partial_cmp(sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            })
            .map(|(p, _)| p.as_str());
        best_idle == Some(peer_id)
    }

    /// W25a: coordinator mirrors `gap_missing` from reorder_buffer each loop.
    pub(crate) fn set_tip_gap_missing(&self, missing: bool) {
        self.tip_gap_missing.store(missing, Ordering::Relaxed);
        // W75: export thread reads the process-wide atomic (no assigner handle there).
        super::IBD_TIP_GAP_MISSING.store(missing, Ordering::Relaxed);
        // Tip arrived (or was never missing) — drop temporary failover covering.
        if !missing {
            super::tip_stage::clear_tip_failover();
            super::tip_stage::clear_tip_ahead_soft_freeze();
        }
    }

    /// Coordinator mirrors bridge `holes_in_span` for tip-band ahead gating.
    pub(crate) fn set_tip_bridge_holes(&self, holes: u64) {
        self.tip_bridge_holes.store(holes, Ordering::Relaxed);
        super::IBD_TIP_BRIDGE_HOLES.store(holes, Ordering::Relaxed);
    }

    /// Freeze multi-peer ahead when bridge holes ≥ N (optional hard gate).
    ///
    /// **W47:** holes alone ≠ tip distress (live: holes≈22 in both slow and fast
    /// buckets). **W123–W125:** sticky feeder-empty latch
    /// (`tip_ahead_hole_freeze`) — set `BLVM_IBD_TIP_AHEAD_MAX_HOLES=0` to disable.
    fn tip_ahead_max_holes_opt() -> Option<u64> {
        latch_env!(Option<u64>, {
            match std::env::var("BLVM_IBD_TIP_AHEAD_MAX_HOLES") {
                Ok(s) if s == "0" || s.eq_ignore_ascii_case("off") => None,
                Ok(s) => s.parse().ok().map(|v: u64| v.clamp(1, 512)),
                // W125: restore arm **24**. W124 arm=16 froze 58% of early TIP_CRAWL
                // samples → crawl≈25 h/s, fail @312k tip60=10.5.
                Err(_) => Some(24),
            }
        })
    }

    /// Clear sticky freeze only below this hole count (hysteresis).
    /// Env: `BLVM_IBD_TIP_AHEAD_HOLE_CLEAR` (default **8**).
    fn tip_ahead_hole_clear_opt(arm: u64) -> u64 {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_AHEAD_HOLE_CLEAR")
                .ok()
                .and_then(|s| s.parse().ok())
                // W125: keep clear@8 (W123 clear@12 released at holes=10–11 → W35 flood).
                // Do not default to arm/2 (that reverts to clear@12 when arm=24).
                .unwrap_or(8)
        });
        raw.clamp(4, arm.saturating_sub(1).max(4))
    }

    /// Sticky ahead-freeze while tip-band holes stay fat and feeder is empty.
    /// Survives tip+1 late-body clock resets.
    /// **W125:** arm **24** / clear **8**. **W181:** distress arm (default 16) when
    /// tip awaiting ≥ `BLVM_IBD_TIP_AHEAD_DISTRESS_AWAIT_SECS`. **W183:** feeder-empty
    /// clear is debounced (`BLVM_IBD_TIP_AHEAD_HOLE_CLEAR_MS`, default 5s).
    fn tip_ahead_hole_band_update(&self, feeder_len: usize) {
        let holes = self.tip_bridge_holes.load(Ordering::Relaxed);
        let Some(arm_default) = Self::tip_ahead_max_holes_opt() else {
            self.tip_ahead_hole_freeze.store(false, Ordering::Relaxed);
            self.tip_ahead_hole_clear_since_ms
                .store(0, Ordering::Relaxed);
            return;
        };
        let distress_arm = {
            let raw = latch_env!(u64, {
                std::env::var("BLVM_IBD_TIP_AHEAD_DISTRESS_HOLES")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(16u64)
            });
            raw.clamp(8, arm_default)
        };
        let distress_await_secs = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_AHEAD_DISTRESS_AWAIT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3u64)
                .clamp(0, 30)
        });
        let awaiting = super::tip_stage::tip_awaiting_secs_for_cap();
        let arm = if awaiting >= distress_await_secs {
            arm_default.min(distress_arm)
        } else {
            arm_default
        };
        let clear = Self::tip_ahead_hole_clear_opt(arm_default);
        // W183: require holes < clear for debounce before releasing sticky (default **5s**).
        // Live W182 330–345k: holes oscillated 0↔24 → freeze clear every tip+1 → W35 flood.
        // Env: `BLVM_IBD_TIP_AHEAD_HOLE_CLEAR_MS` (0 = immediate).
        let clear_debounce_ms = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_AHEAD_HOLE_CLEAR_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5_000)
                .clamp(0, 30_000)
        });
        let now_ms = Self::unix_now_ms();
        if holes >= arm && feeder_len == 0 {
            self.tip_ahead_hole_freeze.store(true, Ordering::Relaxed);
            self.tip_ahead_hole_clear_since_ms
                .store(0, Ordering::Relaxed);
        } else if holes < clear {
            // Feeder runway ⇒ tip is draining — release immediately (W125 / W143).
            // Debounce only when feeder stays empty (hole oscillation across tip+1).
            if !self.tip_ahead_hole_freeze.load(Ordering::Relaxed) {
                // already clear
            } else if feeder_len > 0 || clear_debounce_ms == 0 {
                self.tip_ahead_hole_freeze.store(false, Ordering::Relaxed);
                self.tip_ahead_hole_clear_since_ms
                    .store(0, Ordering::Relaxed);
            } else {
                let since = self.tip_ahead_hole_clear_since_ms.load(Ordering::Relaxed);
                if since == 0 {
                    self.tip_ahead_hole_clear_since_ms
                        .store(now_ms.max(1), Ordering::Relaxed);
                } else if now_ms.saturating_sub(since) >= clear_debounce_ms {
                    self.tip_ahead_hole_freeze.store(false, Ordering::Relaxed);
                    self.tip_ahead_hole_clear_since_ms
                        .store(0, Ordering::Relaxed);
                }
            }
        } else {
            // Mid-band holes while frozen — keep latch; cancel clear countdown.
            self.tip_ahead_hole_clear_since_ms
                .store(0, Ordering::Relaxed);
        }
        // W135: debounce-clear weak sticky whether or not freeze latched.
        self.nudge_weak_sticky();
    }

    /// W130/W132: preferred tip sticky is credible under hole-freeze (mid+ / holding tip).
    /// Floor stickies are not — open tip slot for STREAM/mid re-arm while ahead stays frozen.
    fn tip_owner_credible_for_hole_freeze(&self) -> bool {
        let Some(pref) = self.preferred_tip_owner() else {
            return false;
        };
        if !self.tip_sticky_usable(&pref) {
            return false;
        }
        if self.peer_score_of(&pref) > Self::TIP_OWNER_MID_SCORE {
            return true;
        }
        self.peer_holds_tip_download(&pref, self.next_needed_height())
    }

    /// W132: `BLVM_IBD_WEAK_STICKY_OPEN_MS` (default **15000**, clamp 0–60000).
    fn weak_sticky_open_debounce_ms() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WEAK_STICKY_OPEN_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(15_000)
                .clamp(0, 60_000)
        })
    }

    /// W130/W132/W135: under hole-freeze, drop unusable/floor sticky and open tip slot
    /// (debounced). Ahead freeze stays latched — only tip ownership is unlocked.
    fn nudge_weak_sticky(&self) {
        if !self.tip_ahead_hole_freeze.load(Ordering::Relaxed) {
            return;
        }
        // Function-static debounce clocks (tipfix binary: WEAK_SINCE_MS / LAST_CLEAR_MS).
        static WEAK_SINCE_MS: AtomicU64 = AtomicU64::new(0);
        static LAST_CLEAR_MS: AtomicU64 = AtomicU64::new(0);
        if self.tip_owner_credible_for_hole_freeze() {
            WEAK_SINCE_MS.store(0, Ordering::Relaxed);
            return;
        }
        let now = Self::unix_now_ms();
        let debounce_ms = Self::weak_sticky_open_debounce_ms();
        let since = WEAK_SINCE_MS.load(Ordering::Relaxed);
        if since == 0 {
            WEAK_SINCE_MS.store(now.max(1), Ordering::Relaxed);
            if debounce_ms > 0 {
                return; // first sample arms countdown only (W132)
            }
        } else if debounce_ms > 0 {
            if now.saturating_sub(since) < debounce_ms {
                return;
            }
            // Rate-limit repeat clears.
            let last = LAST_CLEAR_MS.load(Ordering::Relaxed);
            if last > 0 && now.saturating_sub(last) < debounce_ms {
                return;
            }
        }
        let pref = self.preferred_tip_owner();
        if pref.is_none() && self.tip_owner_open.load(Ordering::Relaxed) {
            return;
        }
        if let Some(ref p) = pref {
            tracing::warn!(
                "[IBD_TIP_WEAK_STICKY_OPEN] peer={} score={:.3} — hole-freeze unlocks tip slot",
                p,
                self.peer_score_of(p)
            );
            let mut g = self.preferred_tip_owner.lock().unwrap();
            if g.as_deref() == Some(p.as_str()) {
                *g = None;
            }
        }
        LAST_CLEAR_MS.store(now.max(1), Ordering::Relaxed);
        WEAK_SINCE_MS.store(0, Ordering::Relaxed);
        self.open_tip_owner_slot();
    }

    /// Optional emergency: shrink tip-owner preempt batch when holes ≥ N.
    ///
    /// **W47 default: unset / disabled.** Former default **1** permanently shrunk the
    /// tip pipe 128→32 on mid-chain WAN (holes==0 only ~7% of samples).
    /// Env: `BLVM_IBD_TIP_PIPE_SHRINK_HOLES`.
    fn tip_pipe_shrink_holes_opt() -> Option<u64> {
        latch_env!(Option<u64>, {
            std::env::var("BLVM_IBD_TIP_PIPE_SHRINK_HOLES")
                .ok()
                .and_then(|s| s.parse().ok())
                .map(|v: u64| v.clamp(1, 512))
        })
    }

    /// Optional emergency: tip-height `(H,H)` race when holes ≥ N.
    ///
    /// **W47 default: unset / disabled.** Former hole-keyed race + ahead-freeze produced
    /// covering≈2 treadmills while ahead stayed off (docs/IBD_WAN_TIP_ARCHITECTURE.md).
    /// Env: `BLVM_IBD_TIP_HOLE_RACE_HOLES`.
    fn tip_hole_race_holes_opt() -> Option<u64> {
        latch_env!(Option<u64>, {
            std::env::var("BLVM_IBD_TIP_HOLE_RACE_HOLES")
                .ok()
                .and_then(|s| s.parse().ok())
                .map(|v: u64| v.clamp(1, 512))
        })
    }

    /// C1g/C1h: after this many seconds of tip await while tip missing, arm `(H,H)`
    /// failover (W88 episode latch still caps storms). Default **0** (immediate) —
    /// C1g iter with await=2 left fetchers_cap=1 under deep stripe → ~3 BPS EMPTY_TIP.
    /// Env `BLVM_IBD_C1G_TIP_RACE_AWAIT_SECS` (clamp 0–30).
    fn c1g_tip_race_await_secs() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_C1G_TIP_RACE_AWAIT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
                .clamp(0, 30)
        })
    }

    /// C1t: tip-height `(H,H)` race after tip missing this many ms (default **120**).
    /// Good-day gd≈90 ms — 250 ms rarely armed (C1t@250 soak: covering≈1, wall≈331).
    /// Integer-second soft-retry / late-body freeze never arm on mid-gaps.
    /// `0` = off. Clamp 0–2000. Mute guard: also requires gd-fast elevated.
    fn c1t_tip_race_ms() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_C1T_TIP_RACE_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(120)
                .clamp(0, 2_000)
        })
    }

    /// C1t: open one tip-height racer (no past-tip ahead). W88 episode still applies.
    fn c1t_tip_height_race(&self) -> bool {
        let ms = Self::c1t_tip_race_ms();
        if ms == 0 {
            return false;
        }
        if !self.tip_gap_missing.load(Ordering::Relaxed)
            && !super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed)
        {
            return false;
        }
        // Mute peerday guard — same gate as C1n grow-fast (EWMA cold → no race).
        if super::download::tip_hole_grow_cap_effective() <= super::download::tip_hole_grow_cap()
        {
            return false;
        }
        let awaiting = super::tip_stage::tip_awaiting_ms_for_cap();
        if awaiting >= ms {
            return true;
        }
        // Phase 2 mid-gap: covering≥1 + feeder=0 but await clock stuck at 0 (stamp reset
        // while tip never lands). Live EMPTY_TIP SLOW_STRETCH had covering=1 await_ms=0
        // gd_ewma~180 — classic C1t never armed. Debounce ~80ms to avoid W172 storms.
        let feeder = super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
        if feeder > 0 || awaiting > 0 {
            return false;
        }
        let tip = self.next_needed_height();
        let covering = self.healthy_tip_cover_count(tip);
        // Cap is often 1 until tip_distress (C1t) raises it — don't gate on
        // max_gap_fetchers here or cold-clock never arms when covering=1/cap=1.
        if covering == 0 || covering >= 2 {
            return false;
        }
        static LAST_COLD: Mutex<Option<Instant>> = Mutex::new(None);
        let mut g = LAST_COLD.lock().unwrap();
        if let Some(t) = *g {
            if t.elapsed() < Duration::from_millis(80) {
                return false;
            }
        }
        *g = Some(Instant::now());
        tracing::warn!(
            "[IBD_C1T_COLD_CLOCK] tip={} covering={} await_ms=0 feeder=0 — tip-height race",
            tip,
            covering
        );
        true
    }

    /// True tip distress for race / ahead-cap (not bridge sparsity).
    #[inline]
    fn tip_is_distressed() -> bool {
        super::tip_stage::tip_ahead_frozen_for_soft_retry()
            || super::tip_stage::tip_ahead_frozen_for_late_body()
    }

    /// W112: empty-bridge tip starve → allow covering=3 (deep + 2× `(H,H)`).
    ///
    /// Live W111 @323780 / W110 @326324: covering=2 mute rotate still ~20–25s while
    /// a later peer STREAM'd tip in <1s. Escalate only after **two** mute CAP
    /// windows (default **12s**) — live W112a with trigger=5s / W121 with 8s opened
    /// covering=3 during soft-resume and collapsed tip60 (W121 peak~47, fail @318k).
    /// Keep **12s**. W122 opens a *single* `(H,H)` earlier via `mute_single_cover_reopen`
    /// (covering=1 + awaiting≥5s) without raising fetchers to 3.
    /// Env: `BLVM_IBD_EMPTY_TIP_TRIPLE_SECS` (clamp 5–30).
    fn empty_tip_triple_race(&self) -> bool {
        // Tipfix KEEP (2026-07-31): tip missing + awaiting≥trigger → covering=3.
        // W153: holey (BRIDGE_PENDING>0) still opens triple at ≥12s — do **not** gate
        // on pending==0 (that over-froze holey mute peerdays).
        if !self.tip_gap_missing.load(Ordering::Relaxed)
            && !super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed)
        {
            return false;
        }
        let trigger = latch_env!(u64, {
            std::env::var("BLVM_IBD_EMPTY_TIP_TRIPLE_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(12)
                .clamp(5, 30)
        });
        super::tip_stage::tip_awaiting_secs_for_cap() >= trigger
    }

    /// W122/W149: W88 episode latched + single mute tip cover → reopen one `(H,H)`.
    /// Live W120b @326368: no failover until empty_triple @12s; STREAM then &lt;200ms.
    /// Does **not** raise fetchers_cap to 3 (W121 8s triple thrash).
    ///
    /// **W149:** default trigger **5→3s**. Live W148 @329995–998 tip-stepped ~1h/5s
    /// with covering=1: late-body distress already true (≥2s) but W88 episode stayed
    /// latched and mute_reopen@5s was knife-edge with the dribble interval → no
    /// `(H,H)` race. Env: `BLVM_IBD_MUTE_SINGLE_REOPEN_SECS` (clamp 2–12).
    fn mute_single_cover_reopen(&self, raw_covering: usize) -> bool {
        if raw_covering != 1 {
            return false;
        }
        if !self.tip_gap_missing.load(Ordering::Relaxed)
            && !super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed)
        {
            return false;
        }
        let trigger = latch_env!(u64, {
            std::env::var("BLVM_IBD_MUTE_SINGLE_REOPEN_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3)
                .clamp(2, 12)
        });
        super::tip_stage::tip_awaiting_secs_for_cap() >= trigger
    }

    /// W88: tip heights that must advance before another distress failover (default 32).
    fn tip_failover_episode_advance() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_FAILOVER_EPISODE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(32)
                .clamp(1, 256)
        })
    }

    /// W88: max age of a failover episode before re-arm (default 30s).
    fn tip_failover_episode_ms() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_FAILOVER_EPISODE_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30_000)
                .clamp(5_000, 120_000)
        })
    }

    #[inline]
    fn unix_now_ms() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// W88: clear / query distress-failover episode latch.
    /// Returns true if a failover was already assigned in the current episode.
    fn tip_failover_episode_active(&self, next_needed: u64) -> bool {
        let latch_h = self.tip_failover_once_h.load(Ordering::Relaxed);
        if latch_h == 0 {
            return false;
        }
        let latch_at = self.tip_failover_once_at_ms.load(Ordering::Relaxed);
        let now = Self::unix_now_ms();
        let advanced =
            next_needed >= latch_h.saturating_add(Self::tip_failover_episode_advance());
        let aged = latch_at != 0 && now.saturating_sub(latch_at) >= Self::tip_failover_episode_ms();
        if advanced || aged {
            self.tip_failover_once_h.store(0, Ordering::Relaxed);
            self.tip_failover_once_at_ms.store(0, Ordering::Relaxed);
            return false;
        }
        true
    }

    fn latch_tip_failover_episode(&self, next_needed: u64) {
        self.tip_failover_once_h
            .store(next_needed, Ordering::Relaxed);
        self.tip_failover_once_at_ms
            .store(Self::unix_now_ms(), Ordering::Relaxed);
    }

    /// Max non-owner peers with in-flight ranges past tip on WAN (default **8**).
    /// Env: `BLVM_IBD_TIP_AHEAD_PEERS`. Prior hard-cap 3 left ready≈52 idle at ~5 blk/s.
    fn tip_ahead_peer_cap() -> usize {
        latch_env!(usize, {
            std::env::var("BLVM_IBD_TIP_AHEAD_PEERS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8)
                .clamp(1, 24)
        })
    }

    /// WAN tip-band multi-peer ahead: require deep tip cover; freeze on tip distress / starve.
    ///
    /// Do **not** key off steady-state `gap_missing` alone (`!reorder.contains(next_needed)`),
    /// which is true on every tip poll between receives and permanently disabled ahead
    /// (A6g+W31 single-pipe → ~5 blk/s).
    ///
    /// **W47:** do **not** freeze on bridge `holes` by default — that metric is ahead-OOO
    /// sparsity and stays high while tip is healthy (architecture doc). Freeze on:
    /// soft-retry (W31), late tip body (W42). Optional `BLVM_IBD_TIP_AHEAD_MAX_HOLES` rollback.
    ///
    /// WAN tip-band multi-peer ahead: require deep tip cover; freeze on tip distress.
    ///
    /// Do **not** key off steady-state `gap_missing` / brief `feeder==0` (A6g / W61
    /// regressions). Freeze on soft-retry (W31) and late tip body (W42).
    fn wan_allow_multi_peer_ahead(&self, effective_healthy: usize, feeder_len: usize) -> bool {
        if effective_healthy == 0 {
            return false;
        }
        // Soft-retry still freezes ahead (real tip failover / W31).
        // W102b + late-body ahead freeze narrowed (2026-07-31 tipfix DNA):
        // `feeder==0 && awaiting≥3s` (W102b) and late-body (≥2s) both match *steady-state*
        // single tip-owner WAN crawl. STREAM hole-storm protection remains:
        //   • C1g freezes past-tip stripes while tip is missing
        //   • tip_ahead_hole_band_update latches on awaiting≥3s ∧ holes≥distress (W181)
        //   • soft-retry / tip-SLA rotate a stuck owner
        // Do **not** hard-block multi-peer ahead on awaiting/late-body alone when tip
        // already has healthy cover.
        if super::tip_stage::tip_ahead_frozen_for_soft_retry() {
            self.tip_ahead_hole_band_update(feeder_len);
            return false;
        }
        // W123: sticky freeze — holes≥arm + feeder empty latches until holes < clear.
        // Do **not** hard-gate on holes alone while feeder>0 (W47: holes≠distress).
        self.tip_ahead_hole_band_update(feeder_len);
        if self.tip_ahead_hole_freeze.load(Ordering::Relaxed) {
            return false;
        }
        true
    }

    /// W28c: remember who owns the tip pipeline after a successful assign.
    pub(crate) fn note_tip_owner_assigned(&self, peer_id: &str) {
        let mut g = self.preferred_tip_owner.lock().unwrap();
        let prev = g.clone();
        *g = Some(peer_id.to_string());
        drop(g);
        self.tip_owner_open.store(false, Ordering::Relaxed);
        // W33: start tip-SLA clock when owner takes WAN gap work (coordinator mark_needed may lag).
        if self.wan_tip_gap_crawl(self.next_needed_height()) {
            super::tip_stage::mark_needed(self.next_needed_height());
            // Start tenure when sticky carried from LOCAL_AHEAD with the same peer —
            // peer *change* alone left WAN tenure None (A6m / sticky BPS blind).
            let need_tenure = prev.as_deref() != Some(peer_id)
                || self.sticky_wan_tenure.lock().unwrap().is_none();
            if need_tenure {
                self.reset_sticky_wan_tenure(peer_id, self.next_needed_height());
            }
        }
    }

    /// Current sticky tip owner (if any).
    pub(crate) fn preferred_tip_owner(&self) -> Option<String> {
        self.preferred_tip_owner.lock().unwrap().clone()
    }

    /// W28c: clear sticky owner after tip-covering failure so the next best peer can take over.
    pub(crate) fn note_tip_owner_failed(&self, peer_id: &str) {
        self.note_tip_owner_failed_with_cooldown(peer_id, Self::tip_owner_fail_cooldown_secs());
    }

    /// W103: mute tip-gap CAP abort — shorter cooldown + clear W88 failover episode so a
    /// (H,H) racer can arm immediately. Live W102b: 15s cooldown + episode latch left
    /// covering=1 mute peer for 8s×N inside tip60-watch 20s.
    ///
    /// **W111:** also `force_release_peer_inflight` — live W110 @326324 walk-promoted the
    /// mute-failed peer from residual in-flight in the same ms as mute (`TIP_WALK_PROMOTE`
    /// after `TIP_FAILOVER` armed), re-pinning sticky and burning another 5–15s.
    pub(crate) fn note_tip_owner_failed_mute(&self, peer_id: &str) {
        // P1e: tip-role ban after mute (PIPE_FILL / CAP). Default **120s** (was 5s) so
        // TIP_PIN cannot re-elect the mute hero; clamp 60–180. Failover still arms now.
        let secs = std::env::var("BLVM_IBD_TIP_OWNER_MUTE_COOLDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(120)
            .clamp(60, 180);
        // Drop W88 episode so want_tip_owner can assign failover micro on the next poll.
        self.tip_failover_once_h.store(0, Ordering::Relaxed);
        self.tip_failover_once_at_ms.store(0, Ordering::Relaxed);
        self.note_tip_owner_failed_with_cooldown(peer_id, secs);
        self.force_release_peer_inflight(peer_id);
        // C1c: mute hero must not keep a deep sticky tip-hole pipe.
        self.reset_tip_hole_depth(peer_id);
        if self.wan_tip_gap_crawl(self.next_needed_height()) {
            super::tip_stage::arm_tip_failover();
            tracing::warn!(
                "[IBD_TIP_FAILOVER] armed after mute CAP on tip {} (cleared W88 episode)",
                self.next_needed_height()
            );
        }
    }

    fn note_tip_owner_failed_with_cooldown(&self, peer_id: &str, cooldown_secs: u64) {
        let mut g = self.preferred_tip_owner.lock().unwrap();
        if g.as_deref() == Some(peer_id) {
            // Keep / re-pin forced tip owner — clearing lets ahead peer steal tip (tc168).
            if let Some(forced) = super::sole_tip_forced_owner() {
                *g = Some(forced);
            } else {
                *g = None;
            }
        }
        drop(g);
        // W92: soft cooldown so TIP_PIN / top_w cannot re-elect the CAP-aborted peer.
        // Sole ready peer skip lives inside [`Self::mark_tip_owner_fail_cooldown`].
        self.mark_tip_owner_fail_cooldown(peer_id, cooldown_secs);
        // Primary failed — allow a second covering peer immediately (also armed from soft-retry).
        // W31: never arm failover on WAN tip gap — keeps max_gap_fetchers at 1.
        // W103 mute path arms failover explicitly via [`Self::note_tip_owner_failed_mute`].
        if !self.wan_tip_gap_crawl(self.next_needed_height()) {
            super::tip_stage::arm_tip_failover();
        } else {
            // W36: open tip slot so any top-half peer can re-arm (not only the failed top-1).
            self.tip_owner_open.store(true, Ordering::Relaxed);
        }
        self.clear_tip_cover_claims_for_peer(peer_id);
    }

    /// Default **15s** — escapes CAP same-peer thrash (W91) without burning the mid-score
    /// peer pool into score=0.001 open-slot lottery (live W93 @314596).
    fn tip_owner_fail_cooldown_secs() -> u64 {
        std::env::var("BLVM_IBD_TIP_OWNER_FAIL_COOLDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15)
            .clamp(5, 300)
    }

    fn mark_tip_owner_fail_cooldown(&self, peer_id: &str, secs: u64) {
        // Mode T dual: never cool the forced tip pin (first BLVM_IBD_PEERS) — cooldown
        // lets TIP_PIN elect the ahead loopback peer (tc168 tip90≈7.8).
        if super::sole_tip_forced_owner().as_deref() == Some(peer_id) {
            tracing::warn!(
                "[IBD_TIP_OWNER_COOLDOWN_SKIP] peer={} — sole_tip forced owner (was {}s)",
                peer_id,
                secs
            );
            return;
        }
        // Mode T: assigner often has workers=6 slots but only one IBD-ready archive.
        // Cooldown on that peer freezes covering=0 (tc64/tc65). Skip when no alternate.
        if self.ibd_ready_peer_count() <= 1
            || self.any_ready_active_worker_except(peer_id).is_none()
        {
            tracing::warn!(
                "[IBD_TIP_OWNER_COOLDOWN_SKIP] peer={} — sole ready tip peer (was {}s)",
                peer_id,
                secs
            );
            return;
        }
        let until = Instant::now() + Duration::from_secs(secs);
        self.tip_owner_fail_until
            .lock()
            .unwrap()
            .insert(peer_id.to_string(), until);
        tracing::warn!(
            "[IBD_TIP_OWNER_COOLDOWN] peer={} {}s — skip tip-owner / TIP_PIN",
            peer_id,
            secs
        );
    }

    /// True while peer is inside a W92 tip-owner fail cooldown (expired entries purged).
    fn tip_owner_in_fail_cooldown(&self, peer_id: &str) -> bool {
        let mut g = self.tip_owner_fail_until.lock().unwrap();
        match g.get(peer_id).copied() {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                g.remove(peer_id);
                false
            }
            None => false,
        }
    }



    /// W128/W137: clear tip-owner fail cooldowns for mid+ peers (score > MID).
    /// Floor stickies stay cooled — mute thrash must not re-elect score=0.1 heroes.
    fn clear_mid_plus_tip_owner_fail_cooldowns(&self) -> usize {
        let scores = self.peer_scores.lock().unwrap().clone();
        let mut g = self.tip_owner_fail_until.lock().unwrap();
        let before = g.len();
        g.retain(|peer, _| {
            scores.get(peer).copied().unwrap_or(0.0) <= Self::TIP_OWNER_MID_SCORE
        });
        before.saturating_sub(g.len())
    }

    /// Covering=0: if every mid+ worker is fail-cooled, clear mid+ cooldowns so TIP_PIN
    /// / open-slot can re-arm (live mute CAP lockout). Logs `[IBD_TIP_MID_COOLDOWN_CLEAR]`.
    fn maybe_clear_mid_plus_fail_cooldowns_covering0(&self, tip: u64) {
        // Any mid+ exists (ignore cooldown) but none are live → clear mid+ cooldowns.
        if self
            .active_ready_worker_above(Self::TIP_OWNER_MID_SCORE, true)
            .is_none()
        {
            return;
        }
        if self
            .active_ready_worker_above(Self::TIP_OWNER_MID_SCORE, false)
            .is_some()
        {
            return;
        }
        let cleared = self.clear_mid_plus_tip_owner_fail_cooldowns();
        if cleared > 0 {
            tracing::warn!(
                "[IBD_TIP_MID_COOLDOWN_CLEAR] tip={} cleared={}",
                tip,
                cleared
            );
        }
    }

    /// E15: GD_SLOW ROTATE A→B cools A for 180s; OPEN on B 60s later finds no pin
    /// because A (only tip hero) is still fail-cooled — and `clear_mid_plus` skips
    /// score≫MID peers. Drop fail-cooldowns for everyone except `keep`.
    fn clear_tip_owner_fail_cooldowns_except(&self, keep: &str) -> usize {
        let mut g = self.tip_owner_fail_until.lock().unwrap();
        let before = g.len();
        g.retain(|peer, _| peer == keep);
        before.saturating_sub(g.len())
    }

    /// Clear preferred tip sticky when it is not usable (not ready / not worker / blacklisted)
    /// and open the tip slot. Returns true if sticky was dropped.
    ///
    /// Live wan10k: preferred stayed on a disconnected hero →
    /// [`Self::peer_may_take_wan_gap_retry`] refused every living peer → FORCE_REQUEUE
    /// `(H,H)` spun with covering=0 forever (nudge drop alone is rate-limited / not on
    /// the retry path).
    fn drop_unusable_preferred_tip_sticky(&self) -> bool {
        let Some(pref) = self.preferred_tip_owner() else {
            return false;
        };
        if self.tip_sticky_usable(&pref) {
            return false;
        }
        // Mode T dual: never drop the forced tip pin to None (TIP_PIN would elect ahead).
        if super::sole_tip_forced_owner().as_deref() == Some(pref.as_str()) {
            tracing::warn!(
                "[IBD_TIP_STICKY_DROP_SKIP] sticky={} — sole_tip forced owner (score={:.3})",
                pref,
                self.peer_score_of(&pref)
            );
            return false;
        }
        tracing::warn!(
            "[IBD_TIP_STICKY_DROP] sticky={} score={:.3} — not usable for tip (ready/worker/blacklist)",
            pref,
            self.peer_score_of(&pref)
        );
        let mut g = self.preferred_tip_owner.lock().unwrap();
        if g.as_deref() == Some(pref.as_str()) {
            *g = None;
        }
        drop(g);
        self.open_tip_owner_slot();
        true
    }

    /// W29/W36 SLA: clear sticky owner, release zombie in-flight, re-arm SLA, open tip slot
    /// so the next best live peer can take a deep pipeline.
    pub(crate) fn rotate_tip_owner_on_sla(&self) -> Option<String> {
        // Mode T dual: SLA rotate must not clear the forced tip pin (tc168 steal).
        if let Some(forced) = super::sole_tip_forced_owner() {
            let mut g = self.preferred_tip_owner.lock().unwrap();
            let prev = g.clone();
            *g = Some(forced.clone());
            drop(g);
            self.clear_all_tip_cover_claims();
            if let Some(ref p) = prev {
                if p != &forced {
                    self.force_release_peer_inflight(p);
                }
            }
            self.tip_owner_open.store(false, Ordering::Relaxed);
            self.reset_sticky_wan_tenure(&forced, self.next_needed_height());
            super::tip_stage::rearm_tip_sla();
            tracing::warn!(
                "[IBD_SLA_ROTATE_SKIP] kept forced tip owner {} (prev={:?})",
                forced,
                prev
            );
            return prev;
        }
        let mut g = self.preferred_tip_owner.lock().unwrap();
        let prev = g.take();
        drop(g);
        // W31: WAN gap uses clear-claims + deep re-arm, not failover (W30 coordinator path).
        if !self.wan_tip_gap_crawl(self.next_needed_height()) {
            super::tip_stage::arm_tip_failover();
        }
        self.clear_all_tip_cover_claims();
        if let Some(ref p) = prev {
            // Drop assigner in-flight so covering=0 and retry/tip-owner can reassign immediately.
            // Download aborts via blacklist poll (ChunkGuard drop is then a no-op pop).
            self.force_release_peer_inflight(p);
        }
        self.tip_owner_open.store(true, Ordering::Relaxed);
        super::tip_stage::rearm_tip_sla();
        prev
    }

    /// Remove all in-flight ranges for `peer_id` (SLA rotate / hard-fail recovery).
    pub(crate) fn force_release_peer_inflight(&self, peer_id: &str) {
        let mut g = self.in_flight_per_peer.lock().unwrap();
        g.remove(peer_id);
        drop(g);
        self.clear_tip_cover_claims_for_peer(peer_id);
    }

    /// W36: allow any top-half peer to take tip until the next owner is assigned.
    pub(crate) fn open_tip_owner_slot(&self) {
        self.tip_owner_open.store(true, Ordering::Relaxed);
    }

    /// A51: env `BLVM_IBD_SOLE_EMPTY_RELEASE=1` — under TOP=1 sole Mode T, EMPTY_TIP with
    /// covering=1 leaves sticky `flight=1/1` forever (`[IBD_STICKY_CAP]`; A50: 139 caps,
    /// 0 `TIP_INFLIGHT_TIMEOUT` / `EMPTY_REARM`). Dual opens a second slot (flooded). This
    /// releases the zombie tip cover and lets `get_work` re-issue tip GetData on the same
    /// peer. Height-gated (default `MIN_H=405000`) so cold-start thrash (tc215) stays parked.
    fn a51_sole_empty_release_enabled() -> bool {
        latch_env!(bool, {
            matches!(
                std::env::var("BLVM_IBD_SOLE_EMPTY_RELEASE").as_deref(),
                Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
            )
        })
    }

    fn a51_sole_empty_release_min_h() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_SOLE_EMPTY_RELEASE_MIN_H")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(405_000)
        })
    }

    fn a51_sole_empty_release_ms() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_SOLE_EMPTY_RELEASE_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(2_000)
                .clamp(200, 30_000)
        })
    }

    /// A51v3: skip shallow `(H,H)` / micro-span thrash (A51v2 tip90≈40 after ×15 releases).
    fn a51_sole_empty_release_min_span() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_SOLE_EMPTY_RELEASE_MIN_SPAN")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(64)
                .clamp(1, 256)
        })
    }

    /// Call under `in_flight_per_peer` lock. Returns true if tip flight was cleared.
    fn try_sole_empty_tip_release(
        &self,
        peer_id: &str,
        in_flight: &mut HashMap<String, Vec<(u64, u64)>>,
    ) -> bool {
        if !Self::a51_sole_empty_release_enabled() {
            return false;
        }
        if Self::top_peer_in_flight_cap() >= 2 {
            return false;
        }
        if self.preferred_tip_owner().as_deref() != Some(peer_id) {
            return false;
        }
        let tip = self.next_needed_height();
        if tip < Self::a51_sole_empty_release_min_h() {
            return false;
        }
        if !self.wan_tip_gap_crawl(tip) {
            return false;
        }
        let gap_missing = self.tip_gap_missing.load(Ordering::Relaxed)
            || super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed);
        if !gap_missing {
            return false;
        }
        let feeder = super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
        if feeder > 0 {
            return false;
        }
        let Some(ranges) = in_flight.get(peer_id) else {
            return false;
        };
        // Sole zombie tip cover: one flight that still spans tip.
        if ranges.len() != 1 {
            return false;
        }
        let (s, e) = ranges[0];
        if s > tip || tip > e {
            return false;
        }
        let span = e.saturating_sub(s).saturating_add(1);
        if span < Self::a51_sole_empty_release_min_span() {
            return false;
        }
        let covering = Self::covering_next_count(in_flight, tip);
        if covering != 1 {
            return false;
        }

        // Debounce: EMPTY+cap stuck for RELEASE_MS on this sticky *flight span*.
        // Key by (peer, span) — not tip height. Live A51: tip crept 405098→405182
        // every ~5s under the same zombie cover → tip-keyed stuck clock never matured.
        static STUCK: Mutex<Option<(String, u64, u64, Instant)>> = Mutex::new(None);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let need_ms = Self::a51_sole_empty_release_ms();
        let last = A51_SOLE_EMPTY_LAST_RELEASE_MS.load(Ordering::Relaxed);
        if last > 0 && now_ms.saturating_sub(last) < need_ms {
            return false;
        }
        {
            let mut g = STUCK.lock().unwrap();
            let reset = match g.as_ref() {
                Some((p, ss, ee, _)) if p == peer_id && *ss == s && *ee == e => false,
                _ => true,
            };
            if reset {
                *g = Some((peer_id.to_string(), s, e, Instant::now()));
                return false;
            }
            let since = g.as_ref().map(|(_, _, _, t)| t.elapsed()).unwrap_or_default();
            if since < Duration::from_millis(need_ms) {
                return false;
            }
            *g = None;
        }

        in_flight.remove(peer_id);
        self.clear_tip_cover_claims_for_peer(peer_id);
        self.tip_owner_open.store(true, Ordering::Relaxed);
        A51_SOLE_EMPTY_LAST_RELEASE_MS.store(now_ms, Ordering::Relaxed);
        tracing::warn!(
            "[IBD_SOLE_EMPTY_RELEASE] tip={} sticky={} span={}-{} — clear zombie tip cover under TOP=1",
            tip,
            peer_id,
            s,
            e
        );
        super::tip_stage::rearm_tip_sla();
        true
    }

    /// Phase 2 EMPTY_TIP: covering=0 while tip missing — open tip-owner + re-arm SLA.
    /// Rate-limited (~80ms) so COVERING_ZERO thrash does not storm assigns.
    ///
    /// Note (tc210–tc216): sole-archive often hits EMPTY_TIP with covering=1 + sticky
    /// flight=TOP cap. Aggressive sole `force_release_peer_inflight` here thrashed cold
    /// start (tc215) and gated variants never beat TOP=2 env (tc210 tip90≈74). Parked.
    /// A51 reopens a *height-gated* sole release via `try_sole_empty_tip_release` (env off).
    pub(crate) fn force_empty_tip_rearm(&self, next_needed: u64) {
        static LAST: Mutex<Option<Instant>> = Mutex::new(None);
        {
            let mut g = LAST.lock().unwrap();
            if let Some(t) = *g {
                if t.elapsed() < Duration::from_millis(80) {
                    return;
                }
            }
            *g = Some(Instant::now());
        }
        if !self.wan_tip_gap_crawl(next_needed) {
            return;
        }
        self.clear_all_tip_cover_claims();
        self.tip_owner_open.store(true, Ordering::Relaxed);
        // Prefer pinning a ready worker so get_work does not wait on lottery.
        if self.preferred_tip_owner.lock().unwrap().is_none() {
            let pin = super::sole_tip_forced_owner()
                .filter(|p| !self.is_peer_blacklisted(p))
                .or_else(|| {
                    self.best_covering0_tip_pin_candidate(next_needed)
                        .or_else(|| self.top_scored_active_ready_worker())
                });
            if let Some(pin) = pin {
                *self.preferred_tip_owner.lock().unwrap() = Some(pin.clone());
                self.tip_owner_open.store(false, Ordering::Relaxed);
                self.reset_sticky_wan_tenure(&pin, next_needed);
                tracing::warn!(
                    "[IBD_EMPTY_REARM] tip={} pinned={} covering=0 — deep tip-owner re-arm",
                    next_needed,
                    pin
                );
                super::tip_stage::rearm_tip_sla();
                return;
            }
        }
        tracing::warn!(
            "[IBD_EMPTY_REARM] tip={} covering=0 tip_owner_open=1 — await deep assign",
            next_needed
        );
        super::tip_stage::rearm_tip_sla();
    }

    fn reset_sticky_wan_tenure(&self, peer_id: &str, start_next_needed: u64) {
        if !self.wan_tip_gap_crawl(start_next_needed) {
            *self.sticky_wan_tenure.lock().unwrap() = None;
            self.tip_progress_samples.lock().unwrap().clear();
            return;
        }
        *self.sticky_wan_tenure.lock().unwrap() = Some(StickyWanTenure {
            peer: peer_id.to_string(),
            start_next_needed,
            started_at: Instant::now(),
        });
        let mut samples = self.tip_progress_samples.lock().unwrap();
        samples.clear();
        samples.push_back((Instant::now(), start_next_needed));
    }

    fn sticky_tenure_bps(&self, next_needed: u64) -> Option<(f64, String, f64)> {
        let tenure = self.sticky_wan_tenure.lock().unwrap().clone()?;
        let elapsed = tenure.started_at.elapsed().as_secs_f64();
        if elapsed < 1.0 {
            return None;
        }
        let blocks = next_needed.saturating_sub(tenure.start_next_needed);
        let bps = blocks as f64 / elapsed;
        Some((bps, tenure.peer, elapsed))
    }

    /// Record `next_needed` for recent-window A6m BPS (coordinator / rotate path).
    pub(crate) fn note_tip_progress(&self, next_needed: u64) {
        let mut samples = self.tip_progress_samples.lock().unwrap();
        let now = Instant::now();
        if let Some((last_t, last_nn)) = samples.back().copied() {
            if last_nn == next_needed && now.duration_since(last_t) < Duration::from_millis(200) {
                return;
            }
        }
        samples.push_back((now, next_needed));
        let keep = Duration::from_secs(a6m_recent_window_secs().saturating_mul(3).max(180));
        while let Some((t, _)) = samples.front().copied() {
            if now.duration_since(t) > keep && samples.len() > 1 {
                samples.pop_front();
            } else {
                break;
            }
        }
        while samples.len() > 512 {
            samples.pop_front();
        }
    }

    /// Tip BPS over the recent window (preferred). Falls back to lifetime tenure when
    /// sample history is shorter than ~80% of the window (unit tests / early tenure).
    fn sticky_recent_bps(
        &self,
        next_needed: u64,
        window_secs: u64,
    ) -> Option<(f64, String, f64)> {
        let tenure = self.sticky_wan_tenure.lock().unwrap().clone()?;
        self.note_tip_progress(next_needed);
        let samples = self.tip_progress_samples.lock().unwrap();
        let now = Instant::now();
        let target = now.checked_sub(Duration::from_secs(window_secs))?;
        let mut older: Option<(Instant, u64)> = None;
        for &(t, nn) in samples.iter() {
            if t <= target {
                older = Some((t, nn));
            } else {
                break;
            }
        }
        if let Some((t, nn)) = older {
            let elapsed = now.duration_since(t).as_secs_f64();
            if elapsed >= (window_secs as f64) * 0.8 {
                let bps = next_needed.saturating_sub(nn) as f64 / elapsed.max(1e-3);
                return Some((bps, tenure.peer, elapsed));
            }
        }
        drop(samples);
        // Insufficient recent history — lifetime tenure only after full tenure window
        // (preserves pre-F-P unit tests; env `BLVM_IBD_A6M_TENURE_SECS`).
        let life = self.sticky_tenure_bps(next_needed)?;
        if life.2 < a6m_tenure_secs() as f64 {
            return None;
        }
        Some(life)
    }

    /// A6n: record a WAN tip GAP_STREAM from `peer_id` (download path).
    pub(crate) fn note_wan_tip_stream(&self, peer_id: &str) {
        let mut g = self.peer_tip_streams.lock().unwrap();
        let now = Instant::now();
        // Hot path: tip-adjacent bodies credit every STREAM — avoid `to_string` on hits.
        if let Some(entry) = g.get_mut(peer_id) {
            if entry.started.elapsed() > Duration::from_secs(600) {
                *entry = TipStreamWindow {
                    streams: 1,
                    started: now,
                    last_stream: now,
                };
            } else {
                entry.streams = entry.streams.saturating_add(1);
                entry.last_stream = now;
            }
            return;
        }
        g.insert(
            peer_id.to_string(),
            TipStreamWindow {
                streams: 1,
                started: now,
                last_stream: now,
            },
        );
    }

    /// True when `peer_id` delivered a WAN tip `GAP_STREAM` within `within`.
    ///
    /// Live 2026-07-14: floor-sticky 2× upgrade cleared tip-cover claims on a peer that
    /// was mid-`GAP_STREAM` → `should_abort_tip_walk_in` killed the productive owner
    /// (~1s after first stream). Hold upgrades/walk-in aborts while the peer is hot.
    pub(crate) fn peer_recently_tip_streaming(&self, peer_id: &str, within: Duration) -> bool {
        let g = self.peer_tip_streams.lock().unwrap();
        g.get(peer_id)
            .map(|e| e.streams > 0 && e.last_stream.elapsed() <= within)
            .unwrap_or(false)
    }

    /// W113/W114: peer delivered a tip GAP_STREAM within [`Self::tip_stream_owner_hot_secs`].
    pub(crate) fn peer_is_hot_tip_streamer(&self, peer_id: &str) -> bool {
        self.peer_recently_tip_streaming(
            peer_id,
            Duration::from_secs(Self::tip_stream_owner_hot_secs()),
        )
    }

    /// A6n: recent WAN tip GAP_STREAM rate for `peer_id` (0 if unknown).
    pub(crate) fn wan_tip_stream_bps(&self, peer_id: &str) -> f64 {
        let g = self.peer_tip_streams.lock().unwrap();
        let Some(entry) = g.get(peer_id) else {
            return 0.0;
        };
        if entry.streams == 0 {
            return 0.0;
        }
        let secs = entry.started.elapsed().as_secs_f64().max(1.0);
        entry.streams as f64 / secs
    }

    /// W113: how long a tip GAP_STREAM keeps a peer eligible for empty-tip deep owner.
    /// Default **90s**. Env `BLVM_IBD_TIP_STREAM_OWNER_SECS`.
    fn tip_stream_owner_hot_secs() -> u64 {
        std::env::var("BLVM_IBD_TIP_STREAM_OWNER_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(90)
            .clamp(15, 300)
    }

    /// W113: empty tip + at least one ready tip-STREAM peer → prefer streamers for
    /// deep tip owner (not floor-score open-slot lottery).
    fn empty_tip_owner_prefer_streamer(&self) -> bool {
        let gap = self.tip_gap_missing.load(Ordering::Relaxed)
            || super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed);
        if !gap {
            return false;
        }
        self.best_a6n_tip_candidate("").is_some()
    }

    /// A6n: best ready worker by recent tip GAP_STREAM rate (never lifetime bulk BPS).
    fn best_a6n_tip_candidate(&self, exclude: &str) -> Option<(String, f64)> {
        // TPP L3 REVERT (L3-20260801T034357Z): last_tip_hedge GD_SLOW prefer →
        // wall 378 < C0 390; TIP_HOLE_AHEAD 7>5; A6M rotate=0 (path unused on hero day).
        let mut best: Option<(String, f64)> = None;
        for peer_id in self.active_download_worker_ids() {
            if peer_id == exclude {
                continue;
            }
            if !self.peer_is_ibd_ready(&peer_id)
                || self.is_peer_blacklisted(&peer_id)
                || self.tip_owner_in_fail_cooldown(&peer_id)
            {
                continue;
            }
            let bps = self.wan_tip_stream_bps(&peer_id);
            // Require real tip streams — lifetime bulk heroes have 0 here.
            if bps <= 0.0 {
                continue;
            }
            if best.as_ref().map(|(_, b)| bps > *b).unwrap_or(true) {
                best = Some((peer_id, bps));
            }
        }
        best
    }

    /// A6m/A6n: if sticky **recent-window** tip BPS stays below threshold, rotate to a peer with
    /// higher **recent tip GAP_STREAM rate**. Never use lifetime `delivery_blocks_per_sec`
    /// (live A6m: bulk hero at 165 blk/s → worse WAN BPS).
    ///
    /// Live 2026-07-15 soak proof:
    /// - Lifetime tenure BPS over 300s never dropped below **11** → `min_bps=6` never fired
    ///   (`IBD_A6M_ROTATE` / `IBD_A6N_OPEN_SLOT` count = 0) while floor sticky sat at score 0.100.
    /// - Sticky monopolizes tip `GAP_STREAM` counts → alternate peers fail the 1.25× bar even when
    ///   tip crawl is ~1 blk/s; must open-slot instead of returning false.
    pub(crate) fn maybe_rotate_slow_sticky_a6m(
        &self,
        next_needed: u64,
        _peer_scorer: &crate::network::peer_scoring::PeerScorer,
    ) -> bool {
        if !self.wan_tip_gap_crawl(next_needed) {
            return false;
        }
        let floor = self.preferred_is_floor_sticky();
        // Soft-retries must **not** block A6m on non-floor stickies. Live E10 (2026-07-25):
        // sticky@1.3 held 88% of tip assigns while wall ~17 BPS / getdata p50 ~2.7s and
        // soft_retry×39 — the old non-floor soft_retry gate returned false every poll so
        // `IBD_A6M_*=0`. Floor stickies already skipped that gate; mid-score slow owners
        // need the same escape (rotate / open-slot), not ahead flood (W3c FAIL).
        let cooldown = Duration::from_secs(if floor {
            a6m_floor_rotate_cooldown_secs()
        } else {
            a6m_rotate_cooldown_secs()
        });
        if let Some(last) = *self.last_a6m_rotate_at.lock().unwrap() {
            if last.elapsed() < cooldown {
                return false;
            }
        }
        let sticky = match self.preferred_tip_owner() {
            Some(p) if self.tip_sticky_usable(&p) => p,
            _ => return false,
        };
        let gd_ewma = super::tip_stage::getdata_body_ewma_ms();
        let gd_slow = gd_ewma
            .map(|(ms, _)| ms >= a6m_max_getdata_ms())
            .unwrap_or(false);
        let pipe_mute = super::tip_stage::pipe_fill_recv0_streak() > 0;
        let feeder = super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
        let gap = self.tip_gap_missing.load(Ordering::Relaxed)
            || super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed);
        // Slow-drip mute (Phase4 run2): covering=1 + bodies trickle → await_ms≈0 and
        // tip_gap_missing clears, so classic mute-fast never armed; A6m waited full
        // RECENT_WINDOW (~30s) at ~10 BPS. Treat feeder=0 ∧ GD_SLOW ∧ covering≥1 as mute.
        let covering = self.healthy_tip_cover_count(next_needed);
        let slow_drip = feeder == 0 && gd_slow && covering >= 1;
        // Mute-fast: skip 0.8×window tenure when tip is empty + gap + (GD_SLOW or PIPE_FILL)
        // — or slow-drip (gap cleared but crawl is GD_SLOW).
        // Still respects rotate cooldown above. Supply-design §7.2/§7.3 — no 48s wait.
        // Also bypass sticky_recent_bps's internal 0.8× gate (falls back to 300s lifetime).
        let mute_fast = feeder == 0 && (gap || slow_drip) && (gd_slow || pipe_mute);
        let window_secs = a6m_recent_window_secs();
        let (recent_bps, tenure_peer, elapsed_secs) = match self
            .sticky_recent_bps(next_needed, window_secs)
        {
            Some(t) if t.1 == sticky => t,
            _ if mute_fast => match self.sticky_tenure_bps(next_needed) {
                Some(t) if t.1 == sticky => t,
                _ => return false,
            },
            _ => return false,
        };
        let _ = tenure_peer;
        let min_bps = if floor {
            a6m_floor_min_bps()
        } else {
            a6m_min_bps()
        };
        if !mute_fast && elapsed_secs < (window_secs as f64) * 0.8 {
            return false;
        }
        if recent_bps >= min_bps && !gd_slow && !mute_fast {
            return false;
        }
        // Mute-fast with PIPE_FILL but healthy tip BPS and not gd_slow still needs a reason
        // to rotate — treat pipe mute as gd_slow for the rotate body.
        let gd_slow = gd_slow || (mute_fast && pipe_mute);
        if gd_slow && recent_bps >= min_bps {
            let (ms, n) = gd_ewma.unwrap_or((0, 0));
            let feeder_keep = a6m_gd_slow_feeder_keep();
            let tip_keep = a6m_gd_slow_tip_bps_keep();
            // E16/E16b: runway or strong tip crawl ⇒ do not OPEN/blacklist on EWMA alone.
            let keep_feeder = feeder_keep > 0 && feeder >= feeder_keep && !mute_fast;
            let keep_tip = tip_keep > 0.0 && recent_bps >= tip_keep && !mute_fast;
            if keep_feeder || keep_tip {
                tracing::warn!(
                    "[IBD_A6M_GD_SLOW_KEEP] sticky={} tip_bps={:.1} tip_keep={:.0} feeder={}≥{} gd_ewma={}ms (n={}) ≥ {} pipe_recv0={} reason={} — skip rotate",
                    sticky,
                    recent_bps,
                    tip_keep,
                    feeder,
                    feeder_keep,
                    ms,
                    n,
                    a6m_max_getdata_ms(),
                    pipe_mute,
                    if keep_tip && keep_feeder {
                        "tip+feeder"
                    } else if keep_tip {
                        "tip_bps"
                    } else {
                        "feeder"
                    }
                );
                return false;
            }
            tracing::warn!(
                "[IBD_A6M_GD_SLOW] sticky={} tip_bps={:.1} ≥ min={:.0} but getdata→body ewma={}ms (n={}) ≥ {} mute_fast={} pipe_recv0={} feeder={} — LOCAL_GAP/mute; rotate",
                sticky,
                recent_bps,
                min_bps,
                ms,
                n,
                a6m_max_getdata_ms(),
                mute_fast,
                pipe_mute,
                feeder
            );
        }
        self.a6m_do_rotate(next_needed, &sticky, recent_bps, elapsed_secs, floor, gd_slow)
    }

    /// Observability for mute-kill soaks (`[IBD_MUTE_KILL]`).
    /// C1u: also drop old peer sticky tip-hole — TRIAL/OPEN must not reopen depth=32
    /// on a drip hero after rotate (mute CAP already resets via `note_tip_owner_failed_mute`).
    fn log_mute_kill(
        &self,
        reason: &str,
        old: &str,
        new: Option<&str>,
        next_needed: u64,
        tip_bps: Option<f64>,
    ) {
        self.reset_tip_hole_depth(old);
        let await_ms = super::tip_stage::tip_awaiting_ms_for_cap();
        let gd = super::tip_stage::getdata_body_ewma_ms();
        let gd_ewma = gd.map(|(ms, _)| ms);
        let covering = self.healthy_tip_cover_count(next_needed);
        let tip_bps_s = tip_bps
            .map(|b| format!("{b:.1}"))
            .unwrap_or_else(|| "-".into());
        tracing::warn!(
            "[IBD_MUTE_KILL] reason={} old={} new={} await_ms={} gd_ewma={:?} tip_bps={} covering={} next_needed={} pipe_recv0={}",
            reason,
            old,
            new.unwrap_or("-"),
            await_ms,
            gd_ewma,
            tip_bps_s,
            covering,
            next_needed,
            super::tip_stage::pipe_fill_recv0_streak()
        );
    }

    /// P2: tip trials enabled (default **on**). Opt out: `BLVM_IBD_TIP_TRIAL=0`.
    fn tip_trial_enabled() -> bool {
        match std::env::var("BLVM_IBD_TIP_TRIAL")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("0") | Some("false") | Some("off") | Some("no") => false,
            _ => true,
        }
    }

    /// P2: challenger pin duration (default **12s**, clamp 8–20).
    fn tip_trial_secs() -> u64 {
        std::env::var("BLVM_IBD_TIP_TRIAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(12)
            .clamp(8, 20)
    }

    /// P2: tip awaiting before arming a trial (default **3s**).
    fn tip_trial_await_secs() -> u64 {
        std::env::var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3)
            .clamp(2, 15)
    }

    /// Rate-limited skip diagnostic (assigner polls ~50ms; avoid log storms).
    fn log_tip_trial_skip(
        reason: &str,
        sticky: &str,
        await_ms: u64,
        need_ms: u64,
        next_needed: u64,
    ) {
        static LAST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_MS.load(Ordering::Relaxed);
        if now.saturating_sub(prev) < 2000 {
            return;
        }
        LAST_MS.store(now, Ordering::Relaxed);
        tracing::warn!(
            "[IBD_TIP_TRIAL_SKIP] reason={} sticky={} await_ms={} need_ms={} tip={}",
            reason,
            sticky,
            await_ms,
            need_ms,
            next_needed
        );
    }

    /// Ms await gate for tip trial. Post-OPEN boost uses 500ms (or env) for 20s.
    fn tip_trial_need_await_ms(&self) -> u64 {
        let normal = Self::tip_trial_await_secs().saturating_mul(1000);
        let boost_ms = std::env::var("BLVM_IBD_TIP_TRIAL_POST_OPEN_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(500u64);
        if boost_ms == 0 {
            return normal;
        }
        let boost_ms = boost_ms.clamp(200, 2000);
        let Ok(g) = self.tip_trial_post_open_at.lock() else {
            return normal;
        };
        match *g {
            Some(t) if t.elapsed() < Duration::from_secs(20) => boost_ms,
            _ => normal,
        }
    }

    /// P2: min seconds between trial starts (default **30s**).
    fn tip_trial_cooldown_secs() -> u64 {
        std::env::var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30)
            .clamp(15, 300)
    }

    fn tip_stream_count(&self, peer_id: &str) -> u64 {
        self.peer_tip_streams
            .lock()
            .unwrap()
            .get(peer_id)
            .map(|e| e.streams)
            .unwrap_or(0)
    }

    /// P2: ready challenger for a tip trial (does **not** require prior tip streams).
    fn best_tip_trial_challenger(&self, exclude: &str) -> Option<String> {
        // Prefer a tip-proven alternate when one exists; else highest ready score.
        if let Some((p, _)) = self.best_a6n_tip_candidate(exclude) {
            return Some(p);
        }
        let mut best: Option<(String, f64)> = None;
        for peer_id in self.active_download_worker_ids() {
            if peer_id == exclude {
                continue;
            }
            if !self.peer_is_ibd_ready(&peer_id)
                || self.is_peer_blacklisted(&peer_id)
                || self.tip_owner_in_fail_cooldown(&peer_id)
            {
                continue;
            }
            let score = self.peer_score_of(&peer_id);
            if best.as_ref().map(|(_, s)| score > *s).unwrap_or(true) {
                best = Some((peer_id, score));
            }
        }
        best.map(|(p, _)| p)
            .or_else(|| self.any_ready_active_worker_except(exclude))
    }

    /// P2: poll tip trial — finish active trial or start one when tip is starving.
    /// Returns true when a trial started or finished (state changed).
    pub(crate) fn maybe_run_tip_trial(&self, next_needed: u64) -> bool {
        if !Self::tip_trial_enabled() || !self.wan_tip_gap_crawl(next_needed) {
            return false;
        }
        // Finish first so we never stack trials.
        if self.tip_trial.lock().unwrap().is_some() {
            return self.maybe_finish_tip_trial(next_needed);
        }
        self.maybe_start_tip_trial(next_needed)
    }

    /// Short-window tip crawl BPS from progress samples only (no lifetime tenure fallback).
    /// Used to detect slow-drip mute while covering=1 keeps await_ms≈0.
    fn tip_crawl_recent_bps(&self, next_needed: u64, window_secs: u64) -> Option<(f64, f64)> {
        self.note_tip_progress(next_needed);
        let samples = self.tip_progress_samples.lock().unwrap();
        let now = Instant::now();
        let target = now.checked_sub(Duration::from_secs(window_secs))?;
        let mut older: Option<(Instant, u64)> = None;
        for &(t, nn) in samples.iter() {
            if t <= target {
                older = Some((t, nn));
            } else {
                break;
            }
        }
        let (t, nn) = older?;
        let elapsed = now.duration_since(t).as_secs_f64();
        if elapsed < (window_secs as f64) * 0.8 {
            return None;
        }
        let bps = next_needed.saturating_sub(nn) as f64 / elapsed.max(1e-3);
        Some((bps, elapsed))
    }

    fn maybe_start_tip_trial(&self, next_needed: u64) -> bool {
        let feeder = super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
        let gap = self.tip_gap_missing.load(Ordering::Relaxed)
            || super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed);
        let await_ms = super::tip_stage::tip_awaiting_ms_for_cap();
        let need_ms = self.tip_trial_need_await_ms();
        let gd_slow = super::tip_stage::getdata_body_ewma_ms()
            .map(|(ms, _)| ms >= a6m_max_getdata_ms())
            .unwrap_or(false);
        let covering = self.healthy_tip_cover_count(next_needed);
        // Slow-drip: peer keeps covering=1 so await never hits tip_trial_await; arm when
        // short-window crawl stays below A6m min for ≥~6s (0.8×8s window).
        let drip_window = std::env::var("BLVM_IBD_TIP_SLOW_DRIP_WINDOW_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8u64)
            .clamp(5, 20);
        let slow_drip = feeder == 0
            && gd_slow
            && covering >= 1
            && self
                .tip_crawl_recent_bps(next_needed, drip_window)
                .is_some_and(|(bps, _)| bps < a6m_min_bps());
        // M4: tip-gd force — sticky getdata EWMA fat + challenger tip streams ahead.
        // No pin-vacuum blacklist wipe (PB1 REVERT). Prefers tip-window streams only.
        let sticky_for_gd = self.preferred_tip_owner();
        let tip_gd_force = feeder == 0
            && gd_slow
            && covering >= 1
            && sticky_for_gd.as_ref().is_some_and(|sticky| {
                self.best_tip_trial_challenger(sticky).is_some_and(|ch| {
                    let s = self.tip_stream_count(sticky);
                    let c = self.tip_stream_count(&ch);
                    c > s
                        || self.wan_tip_stream_bps(&ch)
                            > self.wan_tip_stream_bps(sticky) * 1.25
                })
            });
        // §7.2: feeder==0 ∧ gap_missing ∧ awaiting≥T — or slow-drip / tip-gd force.
        if feeder > 0 {
            return false;
        }
        if !slow_drip && !tip_gd_force && (!gap || await_ms < need_ms) {
            return false;
        }
        if let Some(last) = *self.last_tip_trial_at.lock().unwrap() {
            if last.elapsed() < Duration::from_secs(Self::tip_trial_cooldown_secs()) {
                return false;
            }
        }
        // E16: post-OPEN settle — do not displace the OPEN pin during handoff while
        // await briefly crosses the 500ms boost gate (C1u stall: OPEN→TRIAL 1.5s).
        let settle = tip_trial_post_open_settle_secs();
        if settle > 0 {
            if let Ok(g) = self.tip_trial_post_open_at.lock() {
                if let Some(t) = *g {
                    if t.elapsed() < Duration::from_secs(settle) {
                        Self::log_tip_trial_skip(
                            "post_open_settle",
                            self.preferred_tip_owner()
                                .as_deref()
                                .unwrap_or("-"),
                            await_ms,
                            need_ms,
                            next_needed,
                        );
                        return false;
                    }
                }
            }
        }
        // Prefer the pinned owner even when not currently "usable" (blacklisted / not
        // ready). Live Phase4: post-OPEN boost hit need_ms=500 but trials skipped
        // `no_usable_sticky` for seconds while mute burned wall — the whole point of a
        // trial is to replace a sticky that is failing tip delivery.
        let sticky = match self.preferred_tip_owner() {
            Some(p) => p,
            None => {
                Self::log_tip_trial_skip(
                    "no_preferred",
                    "-",
                    await_ms,
                    need_ms,
                    next_needed,
                );
                return false;
            }
        };
        let Some(challenger) = self.best_tip_trial_challenger(&sticky) else {
            Self::log_tip_trial_skip(
                "no_challenger",
                &sticky,
                await_ms,
                need_ms,
                next_needed,
            );
            return false;
        };
        if challenger == sticky {
            return false;
        }
        let awaiting = await_ms / 1000;
        let sticky_streams = self.tip_stream_count(&sticky);
        let chall_streams = self.tip_stream_count(&challenger);
        // Cool sticky for tip-only during the trial (ahead can continue on other peers).
        self.mark_tip_owner_fail_cooldown(&sticky, Self::tip_trial_secs().saturating_add(2));
        self.force_release_peer_inflight(&sticky);
        self.clear_tip_cover_claims_for_peer(&sticky);
        *self.preferred_tip_owner.lock().unwrap() = Some(challenger.clone());
        self.tip_owner_open.store(false, Ordering::Relaxed);
        self.reset_sticky_wan_tenure(&challenger, next_needed);
        *self.tip_trial.lock().unwrap() = Some(TipTrial {
            sticky: sticky.clone(),
            challenger: challenger.clone(),
            started: Instant::now(),
            sticky_streams_at_start: sticky_streams,
            challenger_streams_at_start: chall_streams,
            next_needed_at_start: next_needed,
        });
        *self.last_tip_trial_at.lock().unwrap() = Some(Instant::now());
        tracing::warn!(
            "[IBD_TIP_TRIAL_START] sticky={} challenger={} tip={} awaiting={}s trial={}s sticky_streams={} chall_streams={} slow_drip={} tip_gd_force={} await_ms={} covering={}",
            sticky,
            challenger,
            next_needed,
            awaiting,
            Self::tip_trial_secs(),
            sticky_streams,
            chall_streams,
            slow_drip,
            tip_gd_force,
            await_ms,
            covering
        );
        self.log_mute_kill(
            "TRIAL_START",
            &sticky,
            Some(&challenger),
            next_needed,
            Some(self.wan_tip_stream_bps(&sticky)),
        );
        true
    }

    fn maybe_finish_tip_trial(&self, next_needed: u64) -> bool {
        let trial = {
            let g = self.tip_trial.lock().unwrap();
            g.clone()
        };
        let Some(trial) = trial else {
            return false;
        };
        if trial.started.elapsed() < Duration::from_secs(Self::tip_trial_secs()) {
            return false;
        }
        let sticky_delta = self
            .tip_stream_count(&trial.sticky)
            .saturating_sub(trial.sticky_streams_at_start);
        let chall_delta = self
            .tip_stream_count(&trial.challenger)
            .saturating_sub(trial.challenger_streams_at_start);
        let height_delta = next_needed.saturating_sub(trial.next_needed_at_start);
        // Keep challenger if they delivered tip streams ≥ sticky×1.25, or sticky had
        // zero tip bodies while tip advanced / challenger streamed.
        let keep = chall_delta > 0
            && (chall_delta as f64 >= (sticky_delta as f64) * 1.25
                || (sticky_delta == 0 && (chall_delta > 0 || height_delta > 0)));
        *self.tip_trial.lock().unwrap() = None;
        if keep {
            // Challenger stays preferred; sticky already cooled for tip-role.
            tracing::warn!(
                "[IBD_TIP_TRIAL_KEEP] sticky={} challenger={} tip={} sticky_delta={} chall_delta={} height_delta={}",
                trial.sticky,
                trial.challenger,
                next_needed,
                sticky_delta,
                chall_delta,
                height_delta
            );
            self.log_mute_kill(
                "TRIAL_KEEP",
                &trial.sticky,
                Some(&trial.challenger),
                next_needed,
                Some(self.wan_tip_stream_bps(&trial.challenger)),
            );
        } else {
            // Revert to sticky — clear trial cooldown on sticky so they can re-arm tip.
            {
                let mut g = self.tip_owner_fail_until.lock().unwrap();
                g.remove(&trial.sticky);
            }
            *self.preferred_tip_owner.lock().unwrap() = Some(trial.sticky.clone());
            self.tip_owner_open.store(false, Ordering::Relaxed);
            self.reset_sticky_wan_tenure(&trial.sticky, next_needed);
            // Brief cool on failed challenger so we don't thrash the same alt.
            self.mark_tip_owner_fail_cooldown(&trial.challenger, Self::tip_trial_cooldown_secs());
            self.force_release_peer_inflight(&trial.challenger);
            self.clear_tip_cover_claims_for_peer(&trial.challenger);
            tracing::warn!(
                "[IBD_TIP_TRIAL_REVERT] sticky={} challenger={} tip={} sticky_delta={} chall_delta={} height_delta={}",
                trial.sticky,
                trial.challenger,
                next_needed,
                sticky_delta,
                chall_delta,
                height_delta
            );
        }
        true
    }

    /// Shared A6m rotate / open-slot body (after recent-or-lifetime BPS proved slow).
    fn a6m_do_rotate(
        &self,
        next_needed: u64,
        sticky: &str,
        tenure_bps: f64,
        elapsed_secs: f64,
        floor: bool,
        gd_slow: bool,
    ) -> bool {
        // Mode T dual: never rotate tip off the forced first PEERS pin (tc168).
        if super::sole_tip_forced_owner().as_deref() == Some(sticky) {
            tracing::warn!(
                "[IBD_A6M_ROTATE_SKIP] sticky={} — sole_tip forced owner (tenure_bps={:.2} gd_slow={})",
                sticky,
                tenure_bps,
                gd_slow
            );
            return false;
        }
        let sticky_tip_bps = self.wan_tip_stream_bps(sticky);
        let candidate = self.best_a6n_tip_candidate(sticky);
        if let Some((ref cand_id, candidate_bps)) = candidate {
            // Live 2026-07-15: sticky_tip_bps EWMA stayed ~30 while tenure_bps (height
            // advance) was 0.33 → bar=38.59 and no alternate could clear it. Prefer tenure
            // (what A6m already decided is slow); keep tip-stream rate in the log only.
            // E12 GD_SLOW: sticky_tip_bps ~384 while getdata→body ewma ≥800 (LOCAL_GAP
            // mask) — never use stream rate for the 1.25× bar when GD_SLOW armed.
            let bar_basis = if floor || gd_slow {
                tenure_bps
            } else {
                sticky_tip_bps.max(tenure_bps)
            };
            let bar = bar_basis * 1.25;
            // GD_SLOW: a tip-streaming alternate beats a sticky whose GetData is slow,
            // even when it fails the 1.25× bar (sticky monopolizes GAP_STREAM counts).
            // Require a real tip-stream floor (E13 FORCE@3.86 was noise).
            let force_gd = gd_slow && candidate_bps >= a6m_gd_slow_force_min_tip_bps();
            if candidate_bps > bar || force_gd {
                self.blacklist_peer(sticky, Duration::from_secs(120));
                if gd_slow {
                    self.mark_tip_owner_fail_cooldown(sticky, a6m_gd_slow_owner_cooldown_secs());
                }
                self.force_release_peer_inflight(sticky);
                *self.preferred_tip_owner.lock().unwrap() = Some(cand_id.clone());
                self.tip_owner_open.store(false, Ordering::Relaxed);
                self.reset_sticky_wan_tenure(cand_id, next_needed);
                *self.last_a6m_rotate_at.lock().unwrap() = Some(Instant::now());
                super::tip_stage::rearm_tip_sla();
                if force_gd && candidate_bps <= bar {
                    tracing::warn!(
                        "[IBD_A6M_GD_SLOW_FORCE] from={} to={} tenure_bps={:.2} sticky_tip_bps={:.2} candidate_tip_bps={:.2} bar={:.2} next_needed={} — tip-stream alt despite bar",
                        sticky,
                        cand_id,
                        tenure_bps,
                        sticky_tip_bps,
                        candidate_bps,
                        bar,
                        next_needed
                    );
                    if gd_slow {
                        self.log_mute_kill(
                            "FORCE",
                            sticky,
                            Some(cand_id.as_str()),
                            next_needed,
                            Some(tenure_bps),
                        );
                    }
                } else {
                    tracing::warn!(
                        "[IBD_A6M_ROTATE] from={} to={} tenure_bps={:.2} tenure_secs={:.0} sticky_tip_bps={:.2} candidate_tip_bps={:.2} bar={:.2} floor={} gd_slow={} next_needed={}",
                        sticky,
                        cand_id,
                        tenure_bps,
                        elapsed_secs,
                        sticky_tip_bps,
                        candidate_bps,
                        bar,
                        floor,
                        gd_slow,
                        next_needed
                    );
                    if gd_slow {
                        self.log_mute_kill(
                            "GD_SLOW",
                            sticky,
                            Some(cand_id.as_str()),
                            next_needed,
                            Some(tenure_bps),
                        );
                    }
                }
                return true;
            }
            // Live: sticky owns tip streams → alternates fail 1.25× bar while tip is slow.
            // Open slot instead of keeping the slow sticky — but only below the historical
            // healthy floor (default 12). Live 2026-07-15: tenure=12.57 OPEN_SLOT blacklisted a
            // delivering sticky → pinned score lottery / 0.100 treadmill (~11 blk/s).
            tracing::warn!(
                "[IBD_A6N_BAR_FAIL] sticky={} tenure_bps={:.2} sticky_tip_bps={:.2} best_alt={} alt_bps={:.2} bar={:.2} floor={} open_slot_min={:.2}",
                sticky,
                tenure_bps,
                sticky_tip_bps,
                cand_id,
                candidate_bps,
                bar,
                floor,
                a6m_floor_open_slot_min_bps()
            );
            if floor && !gd_slow && tenure_bps >= a6m_floor_open_slot_min_bps() {
                tracing::warn!(
                    "[IBD_A6N_KEEP] sticky={} tenure_bps={:.2} ≥ open_slot_min={:.2} — no tip-proven alt; keep sticky",
                    sticky,
                    tenure_bps,
                    a6m_floor_open_slot_min_bps()
                );
                return false;
            }
        } else if floor && !gd_slow && tenure_bps >= a6m_floor_open_slot_min_bps() {
            // No tip-stream alternate at all — same keep gate (not GD_SLOW).
            tracing::warn!(
                "[IBD_A6N_KEEP] sticky={} tenure_bps={:.2} ≥ open_slot_min={:.2} — no tip-proven candidate; keep sticky",
                sticky,
                tenure_bps,
                a6m_floor_open_slot_min_bps()
            );
            return false;
        }
        // Mode T sole archive (tc65 2026-08-04): GD_SLOW OPEN_SLOT with no challenger
        // blacklisted the only ready peer 120s + OWNER_COOLDOWN 180s → MUTE_KILL new=- /
        // covering=0 for the rest of tip90. Keep sticky when no alternate can take tip.
        // E15: when GD_SLOW and another tip-streamer exists but is cooled/blacklisted,
        // fall through to OPEN so `clear_*_except` can un-cool that hero (dens KEEP
        // `a6m_gd_slow_open_uncools_prior_hero_when_pin_empty`).
        if self.any_ready_active_worker_except(sticky).is_none()
            && self.best_a6n_tip_candidate(sticky).is_none()
        {
            let e15_uncool = gd_slow
                && self.active_download_worker_ids().iter().any(|p| {
                    p != sticky && self.tip_stream_count(p) > 0
                });
            if !e15_uncool {
                tracing::warn!(
                    "[IBD_A6N_KEEP] sticky={} tenure_bps={:.2} sticky_tip_bps={:.2} gd_slow={} — no alternate ready worker; keep sticky (sole tip peer)",
                    sticky,
                    tenure_bps,
                    sticky_tip_bps,
                    gd_slow
                );
                return false;
            }
        }
        // True stall / GD_SLOW / non-floor: open slot; pin a concrete ready worker.
        // E12: top_scored walked peer_scores only → often None while other download
        // workers were ready → preferred=None lottery re-elected the same sticky.
        // Cool sticky *after* pin attempt so we can un-cool prior GD_SLOW heroes first.
        // OPEN_SLOT always blacklists the opened sticky (dens KEEP a6m_gd_slow_open_uncools).
        // E16b NO_BL applies only when we *keep* the sticky (return false above), not OPEN.
        self.blacklist_peer(sticky, Duration::from_secs(120));
        self.force_release_peer_inflight(sticky);
        self.clear_all_tip_cover_claims();
        // Never pin the sticky we are opening away from (E12: score-map often only
        // lists the sticky → top_scored re-elected it; dens KEEP pins alt worker).
        let mut pinned = self
            .top_scored_active_ready_worker()
            .filter(|p| p != sticky)
            .or_else(|| self.any_ready_active_worker_except(sticky));
        let mut cleared_cd = 0usize;
        if pinned.is_none() && gd_slow {
            // E15: prior ROTATE cooled+blacklisted the only tip hero → pinned=None×3.
            let cleared_bl = self.clear_blacklist_except(sticky);
            cleared_cd = self.clear_tip_owner_fail_cooldowns_except(sticky);
            pinned = self
                .top_scored_active_ready_worker()
                .filter(|p| p != sticky)
                .or_else(|| self.any_ready_active_worker_except(sticky))
                .or_else(|| self.best_a6n_tip_candidate(sticky).map(|(p, _)| p));
            if cleared_cd > 0 || cleared_bl > 0 {
                tracing::warn!(
                    "[IBD_A6N_COOLDOWN_CLEAR] sticky={} cleared_cd={} cleared_bl={} — GD_SLOW OPEN pin retry",
                    sticky,
                    cleared_cd,
                    cleared_bl
                );
            }
        }
        if gd_slow {
            self.mark_tip_owner_fail_cooldown(sticky, a6m_gd_slow_owner_cooldown_secs());
        } else {
            // Always cool the opened-away sticky briefly so TIP_PIN cannot re-arm it
            // before an alternate polls (W92). Shorter than GD_SLOW path.
            self.mark_tip_owner_fail_cooldown(sticky, Self::tip_owner_fail_cooldown_secs());
        }
        {
            let mut g = self.preferred_tip_owner.lock().unwrap();
            *g = pinned.clone();
        }
        // Concrete pin → closed sticky; None → leave open for first ready poller.
        self.tip_owner_open
            .store(pinned.is_none(), Ordering::Relaxed);
        if let Some(ref p) = pinned {
            self.reset_sticky_wan_tenure(p, next_needed);
        }
        super::tip_stage::rearm_tip_sla();
        *self.last_a6m_rotate_at.lock().unwrap() = Some(Instant::now());
        tracing::warn!(
            "[IBD_A6N_OPEN_SLOT] sticky={} tenure_bps={:.2} tenure_secs={:.0} sticky_tip_bps={:.2} floor={} gd_slow={} next_needed={} pinned={:?} cleared_cd={} — no tip-proven candidate above bar",
            sticky,
            tenure_bps,
            elapsed_secs,
            sticky_tip_bps,
            floor,
            gd_slow,
            next_needed,
            pinned,
            cleared_cd
        );
        if gd_slow {
            self.log_mute_kill(
                "OPEN",
                sticky,
                pinned.as_deref(),
                next_needed,
                Some(tenure_bps),
            );
            // SLA rearm zeroed await — arm post-OPEN boost after settle (E16).
            // Do not call maybe_start_tip_trial here: await≈0 right after rearm, and
            // immediate trial displaced OPEN pins during C1u handoff thrash.
            *self.tip_trial_post_open_at.lock().unwrap() = Some(Instant::now());
        }
        true
    }

    /// Score stored for `peer_id` (0.0 if unknown).
    fn peer_score_of(&self, peer_id: &str) -> f64 {
        self.peer_scores
            .lock()
            .unwrap()
            .get(peer_id)
            .copied()
            .unwrap_or(0.0)
    }

    /// A6d/A6f: sticky loses only for a *clearly* faster ready active worker.
    /// Tip ranks use `PeerScorer::tip_owner_score` (unproven demoted ≪ 0.1 floor).
    /// Require candidate ≥ 0.5 so floor-cluster proven peers (0.1–0.2) cannot thrash sticky;
    /// breakthrough-class bandwidth (~1.3+) still upgrades.
    ///
    /// **Floor 2× exception (2026-07-14):** sticky@0.100 with covering=1 never upgraded
    /// (MIN_CANDIDATE=0.5 never fires in the 0.1–0.3 cluster) → ~10 blk/s WAN tip crawl.
    /// Allow upgrade when `cand ≥ 2× sticky` so 0.100→0.203 escapes without A6d thrash
    /// (0.100→0.191 stays blocked).
    const TIP_OWNER_UPGRADE_EPS: f64 = 0.05;
    const TIP_OWNER_UPGRADE_MIN_CANDIDATE: f64 = 0.5;
    /// Proven tip downloaders floor near this; treat as "floor sticky".
    const TIP_OWNER_FLOOR_SCORE: f64 = 0.12;
    /// Mid-band ceiling for 2× tip-owner escape (covers live sticky wobble 0.100–0.136).
    const TIP_OWNER_MID_SCORE: f64 = 0.15;
    /// W95: below this = unproven / demoted — never deep tip owner while any ready
    /// active worker scores higher (cooldown-ignorant alternative check).
    const TIP_OWNER_UNPROVEN_SCORE: f64 = 0.05;

    /// Sticky may exclusively own tip only while it can actually take tip work.
    ///
    /// Live A6g: preferred stayed `Some(45.147…)` after span end while its live score
    /// A6h/A6k: sticky tip owner is usable while ready + active download worker + not blacklisted.
    ///
    /// **Do not** require `peer_ok_for_gap_race`. Live W32d″ soak (~3 blk/s, 14 owners): tip
    /// downloaders floor at **~0.1** while lightly-proven ready workers sit at **~0.19**; WAN
    /// median floor (~0.18) made sticky fail `peer_ok` → `STICKY_DROP` → lottery. A6i breakthrough
    /// soak had **146 same-peer re-arms** vs **5** here; `need→getdata` p50 **3s → 29s**.
    ///
    /// A6h deadlock (covering=0 forever) was sticky *preferred* but unable to pass `peer_ok` on
    /// open-slot — exemption lets the delivering sticky take tip; hung sticky is rotated by tip
    /// SLA / blacklist, not by score-floor eviction.
    fn tip_sticky_usable(&self, pref: &str) -> bool {
        !self.is_peer_blacklisted(pref)
            && self.peer_is_ibd_ready(pref)
            && self.is_active_download_worker(pref)
    }

    /// Hold floor-sticky upgrade / walk-in abort this long after a tip `GAP_STREAM`.
    const TIP_STREAM_HOT_SECS: u64 = 15;

    /// True when `candidate` should replace `sticky` as tip owner.
    fn tip_owner_should_upgrade(&self, sticky: &str, candidate: &str) -> bool {
        if sticky == candidate {
            return false;
        }
        // Never score-upgrade away from a peer that is actively filling the tip gap —
        // unless measured tip BPS is below the stretch floor target. Live 2026-07-15:
        // tip-adjacent receive notes keep peer_recently_tip_streaming true at ~11 blk/s
        // while sticky_recent_bps was often None (cold samples / short tenure) →
        // `below_stretch=false` blocked 2× forever (TIP_UPGRADE=0, OPEN_STALL top_w@0.201).
        // Missing history must allow escape; only proven ≥ stretch BPS holds the sticky.
        if self.peer_recently_tip_streaming(sticky, Duration::from_secs(Self::TIP_STREAM_HOT_SECS))
        {
            let next = self.next_needed_height();
            let window = a6m_recent_window_secs();
            let hold_hot = self.sticky_recent_bps(next, window).is_some_and(|(bps, peer, elapsed)| {
                peer == sticky
                    && elapsed >= (window as f64) * 0.8
                    && bps >= a6m_floor_min_bps()
            });
            if hold_hot {
                return false;
            }
        }
        let cand = self.peer_score_of(candidate);
        let cur = self.peer_score_of(sticky);
        // Demoted/unproven sticky (tip_owner_score ~0.001): any clearly better active
        // worker may take tip. MIN_CANDIDATE=0.5 blocked live upgrades 0.001→0.288 and
        // left covering=1 zombies for the full 90s tip-SLA (genesis WAN stalls).
        // Boundary score==UNPROVEN (0.05): A6k keep — not unproven-upgrade, not 2×.
        if cur < Self::TIP_OWNER_UNPROVEN_SCORE && cand > cur + Self::TIP_OWNER_UPGRADE_EPS {
            return true;
        }
        // Floor/mid-band sticky (UNPROVEN < score ≤0.15): require ~2× jump.
        if cur > Self::TIP_OWNER_UNPROVEN_SCORE
            && cur <= Self::TIP_OWNER_MID_SCORE
            && cand >= (cur * 2.0).max(cur + Self::TIP_OWNER_UPGRADE_EPS)
        {
            return true;
        }
        cand >= Self::TIP_OWNER_UPGRADE_MIN_CANDIDATE
            && cand > cur + Self::TIP_OWNER_UPGRADE_EPS
    }

    /// Preferred sticky is floor/mid-band AND (not tip-streaming, OR below stretch BPS).
    pub(crate) fn preferred_is_idle_floor_sticky(&self) -> bool {
        let Some(pref) = self.preferred_tip_owner() else {
            return false;
        };
        if !self.preferred_is_floor_sticky() {
            return false;
        }
        if !self.peer_recently_tip_streaming(&pref, Duration::from_secs(Self::TIP_STREAM_HOT_SECS))
        {
            return true;
        }
        // Hot: idle for nudge when we lack proven stretch BPS (same polarity as upgrade gate).
        let next = self.next_needed_height();
        let window = a6m_recent_window_secs();
        !self.sticky_recent_bps(next, window).is_some_and(|(bps, peer, elapsed)| {
            peer == pref
                && elapsed >= (window as f64) * 0.8
                && bps >= a6m_floor_min_bps()
        })
    }

    /// Score of the current preferred tip owner, if any.
    pub(crate) fn preferred_tip_owner_score(&self) -> Option<f64> {
        self.preferred_tip_owner()
            .map(|p| self.peer_score_of(&p))
    }

    /// True when preferred sticky is in the floor/mid band eligible for 2× escape.
    pub(crate) fn preferred_is_floor_sticky(&self) -> bool {
        self.preferred_tip_owner_score()
            .is_some_and(|s| s <= Self::TIP_OWNER_MID_SCORE)
    }

    /// Rate-limited stall diag when covering=0 with open tip slot.
    fn log_tip_open_stall_diag(&self, tip: u64) {
        static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_LOG_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < 5_000 {
            return;
        }
        LAST_LOG_MS.store(now_ms, Ordering::Relaxed);

        let preferred = self.preferred_tip_owner();
        let top_w = self.top_scored_active_ready_worker();
        let floor = self.wan_active_worker_score_floor();
        let top_w_score = top_w.as_ref().map(|p| self.peer_score_of(p)).unwrap_or(0.0);
        let top_w_ok = top_w
            .as_ref()
            .map(|p| self.peer_ok_for_gap_race(p) && self.peer_is_ibd_ready(p))
            .unwrap_or(false);
        let mut ready_active = 0usize;
        let mut ready_active_ok = 0usize;
        for p in self.active_download_worker_ids() {
            if !self.peer_is_ibd_ready(&p) || self.is_peer_blacklisted(&p) {
                continue;
            }
            ready_active += 1;
            if self.peer_ok_for_gap_race(&p) {
                ready_active_ok += 1;
            }
        }
        let score_n = self.peer_scores.lock().unwrap().len();
        tracing::warn!(
            "[IBD_TIP_OPEN_STALL] tip={} preferred={:?} top_w={:?} top_w_score={:.3} floor={:.3} top_w_ok={} ready_active_ok={}/{} score_keys={} open={}",
            tip,
            preferred,
            top_w,
            top_w_score,
            floor,
            top_w_ok,
            ready_active_ok,
            ready_active,
            score_n,
            self.tip_owner_open.load(Ordering::Relaxed)
        );
    }

    /// WAN tip-gating score floor from **ready** active download workers when possible.
    ///
    /// Live A6i: floor over all scored workers (incl. not-ready / blacklisted) sat at 0.153
    /// while every ready worker was ≤0.127 → `ready_active_ok=0/9`, covering=0 forever.
    fn wan_active_worker_score_floor(&self) -> f64 {
        let mut vals = self.wan_gap_score_floor_vals();
        if vals.len() < 4 {
            return 0.0;
        }
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // A6k: match peer_ok Q1 (diag must agree with gate).
        vals[vals.len() / 4]
    }

    /// Unique active-worker scores for WAN peer_ok median (ready-first, else all).
    fn wan_gap_score_floor_vals(&self) -> Vec<f64> {
        let scored: Vec<(String, f64)> = {
            let scores = self.peer_scores.lock().unwrap();
            let mut out = Vec::new();
            for p in self.active_download_worker_ids() {
                if let Some(s) = scores.get(&p).copied() {
                    out.push((p, s));
                }
            }
            out
        };
        let mut ready_vals = Vec::new();
        let mut all_vals = Vec::new();
        for (p, s) in scored {
            all_vals.push(s);
            if !self.is_peer_blacklisted(&p) && self.peer_is_ibd_ready(&p) {
                ready_vals.push(s);
            }
        }
        if ready_vals.len() >= 4 {
            ready_vals
        } else {
            all_vals
        }
    }

    /// True when `peer_id` still owns tip via cover claim or in-flight range covering tip.
    fn peer_holds_tip_download(&self, peer_id: &str, next_needed: u64) -> bool {
        if self
            .tip_cover_claims
            .lock()
            .unwrap()
            .iter()
            .any(|(p, s, e)| p == peer_id && *s <= next_needed && next_needed <= *e)
        {
            return true;
        }
        let g = self.in_flight_per_peer.lock().unwrap();
        g.get(peer_id)
            .is_some_and(|ranges| {
                ranges
                    .iter()
                    .any(|&(s, e)| s <= next_needed && next_needed <= e)
            })
    }

    /// P1-A: coordinator nudge when WAN gap has ready peers but no tip covering flight.
    /// Returns `false` when not in WAN tip crawl (caller must not log a successful re-arm).
    pub(crate) fn nudge_wan_tip_owner(&self) -> bool {
        let next_needed = self.next_needed_height();
        if !self.wan_tip_gap_crawl(next_needed) {
            return false;
        }
        // Keep sticky while ready + active (A6k: score-floor must not evict tip downloaders).
        // Blind clear → score lottery (A6b); blind keep of not-ready sticky → covering=0 (A6h).
        // A6k dens KEEP: after not-ready STICKY_DROP, leave preferred=None for open-slot
        // get_work — same-nudge TIP_PIN would re-elect top_w and fail a6k asserts.
        let mut skip_covering0_pin = false;
        let mut upgraded_from: Option<String> = None;
        {
            let mut g = self.preferred_tip_owner.lock().unwrap();
            if let Some(ref p) = *g {
                if !self.tip_sticky_usable(p) {
                    tracing::warn!(
                        "[IBD_TIP_STICKY_DROP] sticky={} score={:.3} — not usable for tip (ready/worker/blacklist)",
                        p,
                        self.peer_score_of(p)
                    );
                    *g = None;
                    skip_covering0_pin = true;
                } else if let Some(top_w) = self.top_scored_active_ready_worker() {
                    if self.tip_owner_should_upgrade(p, &top_w) {
                        let sticky_score = self.peer_score_of(p);
                        let holds = self.peer_holds_tip_download(p, next_needed);
                        // Dens KEEP / nudge_defers: never score-upgrade away from a peer
                        // mid tip-download (incl. unproven@0.001). Tip-SLA rotates stuck pipes.
                        if holds {
                            tracing::warn!(
                                "[IBD_TIP_UPGRADE_DEFER] sticky={} score={:.3} holds tip download — not upgrading to {} ({:.3})",
                                p,
                                sticky_score,
                                top_w,
                                self.peer_score_of(&top_w)
                            );
                        } else {
                            tracing::warn!(
                                "[IBD_TIP_UPGRADE] sticky={} score={:.3} → better_worker={} score={:.3}",
                                p,
                                sticky_score,
                                top_w,
                                self.peer_score_of(&top_w)
                            );
                            upgraded_from = Some(p.clone());
                            *g = Some(top_w);
                        }
                    }
                }
            }
        }
        // Release weak covering claims after upgrade; blacklist demoted unproven only when
        // they are not mid tip-download (pipe abort thrash).
        if let Some(ref weak) = upgraded_from {
            let weak_score = self.peer_score_of(weak);
            let holds_tip = self.peer_holds_tip_download(weak, next_needed);
            if !holds_tip || weak_score < Self::TIP_OWNER_UNPROVEN_SCORE {
                self.clear_tip_cover_claims_for_peer(weak);
            }
            if weak_score < Self::TIP_OWNER_UNPROVEN_SCORE && !holds_tip {
                self.blacklist_peer(weak, Duration::from_secs(45));
            }
        }
        let (covering, _, _) = self.tip_flight_diag();
        if covering == 0 {
            // Live A6n: header-past-tip fail storm blacklisted every active worker
            // (ready_active_ok=0/0) while ready=58 — tip dead forever. Clear blacklists
            // so open slot can re-arm once pipes are clipped to header tip.
            let mut ready_active = 0usize;
            for p in self.active_download_worker_ids() {
                if self.peer_is_ibd_ready(&p) && !self.is_peer_blacklisted(&p) {
                    ready_active += 1;
                }
            }
            if ready_active == 0 {
                let cleared = self.clear_active_worker_blacklists();
                let cleared_ready = self.clear_ready_peer_blacklists();
                if cleared + cleared_ready > 0 {
                    tracing::warn!(
                        "[IBD_TIP_BLACKLIST_CLEAR] cleared {} worker + {} ready blacklist(s) — covering=0 ready_active=0",
                        cleared,
                        cleared_ready
                    );
                }
            }
            // Live 2026-07-16 h≈450k: OPEN_STALL preferred=None top_w_ok=true ready_active_ok=1/1
            // for ~18 min while covering stayed 0 (open-slot lottery never re-armed). Pin the
            // top active ready worker so sticky path + tip SLA can take the hole.
            // W126: prefer idle tip-STREAM / idle top score — not a peer mid W35 ahead
            // (live W125 @326975: pin 162.247 on 327039-70 → covering=0 for 16s).
            // W126b: select candidate + release ahead **before** taking preferred lock.
            // Live W126a @305703: TIP_PIN held preferred then locked in_flight while
            // get_work held in_flight then preferred → AB-BA deadlock (watchdog:
            // "mutex contended/unavailable"); zero assigns after mute CAP.
            // W137: mid+ mute-CAP cooldown clear **before** pin (not after + 5s debounce).
            self.maybe_clear_mid_plus_fail_cooldowns_covering0(next_needed);
            let need_pin = !skip_covering0_pin
                && self.preferred_tip_owner.lock().unwrap().is_none();
            if need_pin {
                // First BLVM_IBD_PEERS entry wins even if momentarily not ibd_ready —
                // filtering on ready let TIP_PIN elect :18334 (tc170 mid-cell).
                let forced_tip = super::sole_tip_forced_owner()
                    .filter(|p| !self.is_peer_blacklisted(p));
                let mut pin_target = forced_tip.clone().or_else(|| {
                    self.best_covering0_tip_pin_candidate(next_needed)
                        .or_else(|| self.top_scored_peer_id())
                });
                // E15b / wan10k: mute CAP cooled every scored ready peer → pin_target=None
                // (mid_clear=0 when heroes left workers). GD_SLOW OPEN already uncools;
                // covering=0 TIP_PIN must too or OPEN_STALL spins forever.
                if pin_target.is_none() {
                    let cleared_cd = self.clear_tip_owner_fail_cooldowns_except("");
                    if cleared_cd > 0 {
                        tracing::warn!(
                            "[IBD_TIP_PIN_COOLDOWN_CLEAR] tip={} cleared_cd={} — covering=0 pin retry after mute pool lockout",
                            next_needed,
                            cleared_cd
                        );
                    }
                    pin_target = forced_tip.clone().or_else(|| {
                        self.best_covering0_tip_pin_candidate(next_needed)
                            .or_else(|| self.top_scored_peer_id())
                            .or_else(|| self.any_ready_active_worker_except(""))
                    });
                }
                // W138: idle-floor pin while a mid+ worker exists (often mid-W35 ahead)
                // re-locks mute thrash. Prefer mid+ and release their ahead.
                // Skip when sole_tip forced owner is set (Mode T dual: tip stays on :18333).
                if forced_tip.is_none()
                    && pin_target
                        .as_ref()
                        .is_some_and(|p| self.peer_score_of(p) <= Self::TIP_OWNER_MID_SCORE)
                {
                    if let Some(mid) = self
                        .active_ready_worker_above(Self::TIP_OWNER_MID_SCORE, false)
                    {
                        tracing::warn!(
                            "[IBD_TIP_PIN_PREFER_MID] tip={} floor_cand={} ({:.3}) → mid={} ({:.3})",
                            next_needed,
                            pin_target.as_deref().unwrap_or("-"),
                            pin_target
                                .as_ref()
                                .map(|p| self.peer_score_of(p))
                                .unwrap_or(0.0),
                            mid,
                            self.peer_score_of(&mid)
                        );
                        pin_target = Some(mid);
                    }
                }
                if let Some(ref forced) = forced_tip {
                    if pin_target.as_deref() != Some(forced.as_str()) {
                        tracing::warn!(
                            "[IBD_TIP_PIN_FORCED] tip={} → {} (first BLVM_IBD_PEERS)",
                            next_needed,
                            forced
                        );
                        pin_target = Some(forced.clone());
                    }
                }
                if let Some(ref top_w) = pin_target {
                    let ahead_only = {
                        let g = self.in_flight_per_peer.lock().unwrap();
                        Self::peer_inflight_ahead_only_map(&g, top_w, next_needed)
                    };
                    if ahead_only {
                        tracing::warn!(
                            "[IBD_TIP_PIN_RELEASE_AHEAD] peer={} tip={} — free W35 so tip re-arm is not blocked behind max_in_flight=1",
                            top_w,
                            next_needed
                        );
                        self.force_release_peer_inflight(top_w);
                    }
                }
                let mut g = self.preferred_tip_owner.lock().unwrap();
                if g.is_none() {
                    if let Some(top_w) = pin_target {
                        tracing::warn!(
                            "[IBD_TIP_PIN] covering=0 preferred=None → pin top_w={} score={:.3}",
                            top_w,
                            self.peer_score_of(&top_w)
                        );
                        *g = Some(top_w.clone());
                        drop(g);
                        self.reset_sticky_wan_tenure(&top_w, next_needed);
                    }
                }
            }
            self.clear_all_tip_cover_claims();
            self.log_tip_open_stall_diag(next_needed);
        }
        super::tip_stage::clear_tip_failover();
        self.open_tip_owner_slot();
        super::tip_stage::rearm_tip_sla();
        true
    }

    /// E15: ROTATE also blacklists A for 120s — OPEN pin retry must un-blacklist
    /// prior heroes (keep current OPEN sticky blacklisted).
    fn clear_blacklist_except(&self, keep: &str) -> usize {
        let mut bl = self.blacklisted_until.lock().unwrap();
        let before = bl.len();
        bl.retain(|peer, _| peer == keep);
        before.saturating_sub(bl.len())
    }


    /// Clear blacklists for ACTIVE download workers only (tip-deadlock recovery).
    fn clear_active_worker_blacklists(&self) -> usize {
        let mut bl = self.blacklisted_until.lock().unwrap();
        let mut n = 0usize;
        for p in self.active_download_worker_ids() {
            if bl.remove(&p).is_some() {
                n += 1;
            }
        }
        n
    }

    /// Clear blacklists for IBD-ready peers (even if not currently in `workers`).
    ///
    /// Live 2026-07-16: after tip stall, `ready=16` but `ready_active_ok=0/0` /
    /// `workers` empty or all blacklisted — worker-only clear left covering=0 forever.
    fn clear_ready_peer_blacklists(&self) -> usize {
        let ready: Vec<String> = self.ibd_ready_peers.lock().unwrap().iter().cloned().collect();
        let mut bl = self.blacklisted_until.lock().unwrap();
        let mut n = 0usize;
        for p in ready {
            if bl.remove(&p).is_some() {
                n += 1;
            }
        }
        n
    }

    /// W28d: register an explicit tip-cover claim (tip owner / failover / tests).
    pub(crate) fn note_tip_cover_claim(&self, peer_id: &str, start: u64, end: u64) {
        let mut g = self.tip_cover_claims.lock().unwrap();
        g.retain(|(p, s, e)| !(p == peer_id && *s == start && *e == end));
        g.push((peer_id.to_string(), start, end));
    }

    fn clear_tip_cover_claim(&self, peer_id: &str, start: u64, end: u64) {
        let mut g = self.tip_cover_claims.lock().unwrap();
        g.retain(|(p, s, e)| !(p == peer_id && *s == start && *e == end));
    }

    fn clear_tip_cover_claims_for_peer(&self, peer_id: &str) {
        let mut g = self.tip_cover_claims.lock().unwrap();
        g.retain(|(p, _, _)| p != peer_id);
    }

    /// Count in-flight tip-owner/failover claims covering `next_needed` (not ahead walk-ins).
    pub(crate) fn healthy_tip_cover_count(&self, next_needed: u64) -> usize {
        Self::healthy_tip_cover_count_from(
            &self.tip_cover_claims.lock().unwrap(),
            next_needed,
        )
    }

    /// W4: count from a claims snapshot (avoids re-locking under `get_work`).
    fn healthy_tip_cover_count_from(
        claims: &[(String, u64, u64)],
        next_needed: u64,
    ) -> usize {
        claims
            .iter()
            .filter(|(_, s, e)| *s <= next_needed && next_needed <= *e)
            .count()
    }

    /// W4: clone tip-cover claims under the Mutex (one acquire for multiple counts).
    fn snapshot_tip_cover_claims(&self) -> Vec<(String, u64, u64)> {
        self.tip_cover_claims.lock().unwrap().clone()
    }

    /// Minimum remaining tip-cover depth that counts as a real deep tip pipe.
    ///
    /// Env: `BLVM_IBD_TIP_DEEP_COVER_MIN` (default **16**). Shallow walk-promote remnants
    /// (live tip=218: claim 218-224 after promote of ahead 193-224) must not block a new
    /// 128-deep owner through a full soft-retry budget.
    fn tip_deep_cover_min_depth() -> u64 {
        std::env::var("BLVM_IBD_TIP_DEEP_COVER_MIN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(16)
            .clamp(4, 128)
    }

    #[inline]
    fn claim_remaining_tip_depth(next_needed: u64, start: u64, end: u64) -> u64 {
        if start > next_needed || next_needed > end || start >= end {
            return 0;
        }
        end.saturating_sub(next_needed).saturating_add(1)
    }

    /// W65: preferred sticky still holds a substantial tip-cover claim.
    fn peer_holds_substantial_tip_cover(&self, peer_id: &str) -> bool {
        let next = self.next_needed_height();
        let min_depth = Self::tip_deep_cover_min_depth();
        self.tip_cover_claims.lock().unwrap().iter().any(|(p, s, e)| {
            p == peer_id && Self::claim_remaining_tip_depth(next, *s, *e) >= min_depth
        })
    }

    /// W30/W65: substantial deep tip-owner claims only.
    ///
    /// Ignores `(H,H)` failover micros **and** shallow walk-promote remnants whose
    /// remaining runway is below [`Self::tip_deep_cover_min_depth`]. Live genesis
    /// tip=218: promote of ahead `193-224` → claim `218-224` (depth 7) held tenure
    /// for ~40s of soft-retry while no 128-deep owner was assigned.
    fn deep_tip_cover_count(&self, next_needed: u64) -> usize {
        Self::deep_tip_cover_count_from(&self.tip_cover_claims.lock().unwrap(), next_needed)
    }

    /// W4: deep cover from a claims snapshot.
    fn deep_tip_cover_count_from(claims: &[(String, u64, u64)], next_needed: u64) -> usize {
        let min_depth = Self::tip_deep_cover_min_depth();
        claims
            .iter()
            .filter(|(_, s, e)| {
                Self::claim_remaining_tip_depth(next_needed, *s, *e) >= min_depth
            })
            .count()
    }

    /// W30: drop all tip-cover claims so WAN gap can re-arm one deep owner.
    pub(crate) fn clear_all_tip_cover_claims(&self) {
        self.tip_cover_claims.lock().unwrap().clear();
    }

    fn is_tip_cover_claim(&self, peer_id: &str, start: u64, end: u64) -> bool {
        self.tip_cover_claims
            .lock()
            .unwrap()
            .iter()
            .any(|(p, s, e)| p == peer_id && *s == start && *e == end)
    }

    /// W49: tip walked into an ahead partition — **promote**, do not abort+reassign.
    ///
    /// Live WAN 564k→574k @ ~13 blk/s: tip-owner span histogram **32×364 / 128×27** with
    /// W28d "after walk-in preempt" spam (same 32-high ranges at tens of Hz). Cause: W43d
    /// aborted walk-ins once `tip_body_landed` while `next_needed` was still inside the
    /// span, then `get_work` re-armed a short tip pipe. That destroyed tip tenure.
    ///
    /// W43 full promote-into-claim blocked sticky re-arm when the walk-in was a zombie.
    /// W49 promotes only while tip is **inside** the span, drops other overlapping deep
    /// claims, and aborts only after tip has walked **past** `end` (true leftover ahead).
    pub(crate) fn should_abort_tip_walk_in(&self, peer_id: &str, start: u64, end: u64) -> bool {
        let next_needed = self.next_needed_height();
        // C1j: tip body missing + range strictly ahead → abort cheese GetData.
        // Prior: `next_needed < start` returned false → ahead kept filling tip+32 while
        // tip empty (C1h ahead_buf_p50=40 / C1i samples still TIP_HOLE_AHEAD).
        if self.tip_gap_missing.load(Ordering::Relaxed) && start > next_needed {
            static C1J_ABORT_LOG: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let prev = C1J_ABORT_LOG.load(Ordering::Relaxed);
            if now.saturating_sub(prev) >= 5
                && C1J_ABORT_LOG
                    .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                tracing::warn!(
                    "[IBD_C1J_ABORT] peer={} span={}-{} tip={} — past-tip GetData aborted while tip missing",
                    peer_id,
                    start,
                    end,
                    next_needed
                );
            }
            return true;
        }
        // Tip still below this span — not a tip walk-in (tip present).
        if next_needed < start {
            return false;
        }
        // Tip walked past the span — free leftover ahead GetData.
        if next_needed > end {
            if self.peer_recently_tip_streaming(
                peer_id,
                Duration::from_secs(Self::TIP_STREAM_HOT_SECS),
            ) {
                return false;
            }
            // Drop stale claims that no longer cover tip (promoted walk-in finished).
            {
                let mut g = self.tip_cover_claims.lock().unwrap();
                g.retain(|(p, _, e)| !(p == peer_id && *e < next_needed));
            }
            return true;
        }
        // Tip inside [start, end] — promote to tip-cover tenure and keep GetData.
        // W111: mute/SLA cooldown peers must not re-sticky via walk-promote.
        if self.tip_owner_in_fail_cooldown(peer_id) {
            return true;
        }
        if self
            .tip_cover_claims
            .lock()
            .unwrap()
            .iter()
            .any(|(p, s, e)| p == peer_id && *s <= next_needed && next_needed <= *e)
        {
            return false;
        }
        self.promote_tip_walk_in(peer_id, start, end);
        false
    }

    /// Deep in-flight range (s < e) covering `next_needed` — ahead walk-in or tip pipe.
    /// Find an in-flight range that **substantially** covers tip (deep runway).
    ///
    /// W98: previously matched any `s < e` cover — live W97 freeze @312048 promoted
    /// shallow remnant 312018-312049 (remain=2) every 500ms via
    /// `[IBD_TIP_WALK_PROMOTE_SHALLOW]` while deep tip re-arm lagged ~7s.
    fn find_inflight_deep_covering(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        next_needed: u64,
    ) -> Option<(String, u64, u64)> {
        let min_depth = Self::tip_deep_cover_min_depth();
        let mut best: Option<(String, u64, u64, u64)> = None;
        for (peer, ranges) in in_flight {
            for &(s, e) in ranges {
                if s >= e || s > next_needed || next_needed > e {
                    continue;
                }
                let remain = Self::claim_remaining_tip_depth(next_needed, s, e);
                if remain < min_depth {
                    continue;
                }
                if best.as_ref().map_or(true, |(_, _, _, r)| remain > *r) {
                    best = Some((peer.clone(), s, e, remain));
                }
            }
        }
        best.map(|(p, s, e, _)| (p, s, e))
    }

    /// Convert an ahead walk-in that now covers `next_needed` into the tip-owner pipe.
    fn promote_tip_walk_in(&self, peer_id: &str, start: u64, end: u64) {
        let next_needed = self.next_needed_height();
        if next_needed < start || next_needed > end {
            return;
        }
        // W111: never re-sticky a mute/SLA-cooled peer from residual in-flight.
        if self.tip_owner_in_fail_cooldown(peer_id) {
            tracing::warn!(
                "[IBD_TIP_WALK_PROMOTE_SKIP] peer={} tip={} — tip-owner cooldown (mute/SLA)",
                peer_id,
                next_needed
            );
            return;
        }
        {
            let mut g = self.tip_cover_claims.lock().unwrap();
            // W51/W65: idempotent only when an existing claim still has substantial
            // remaining tip runway. A shallow remnant must not block promote of a
            // longer walk-in (or leave deep_tip_cover stuck at a useless stub).
            let min_depth = Self::tip_deep_cover_min_depth();
            if g.iter().any(|(_, s, e)| {
                Self::claim_remaining_tip_depth(next_needed, *s, *e) >= min_depth
            }) {
                return;
            }
            // Drop other peers' tip-covering claims (including shallow stubs).
            g.retain(|(p, s, e)| {
                p == peer_id || !(*s <= next_needed && next_needed <= *e && *s < *e)
            });
            g.retain(|(p, s, e)| !(p == peer_id && *s == next_needed && *e == end));
            g.push((peer_id.to_string(), next_needed, end));
        }
        let remain = Self::claim_remaining_tip_depth(next_needed, next_needed, end);
        let min_depth = Self::tip_deep_cover_min_depth();
        // W65: shallow remnants keep GetData via the claim above, but must NOT pin
        // preferred sticky — that blocked deep owners (live tip=218: preferred=walk
        // with claim 218-224 while soft-retry burned ~40s).
        if remain >= min_depth {
            self.note_tip_owner_assigned(peer_id);
        }
        static LAST_PROMOTE_MS: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_PROMOTE_MS.load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(prev) >= 500
            && LAST_PROMOTE_MS
                .compare_exchange(
                    prev,
                    now,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
        {
            if remain >= min_depth {
                tracing::warn!(
                    "[IBD_TIP_WALK_PROMOTE] peer={} span={}-{} → claim={}-{} tip={} — keep GetData as tip owner",
                    peer_id,
                    start,
                    end,
                    next_needed,
                    end,
                    next_needed
                );
            } else {
                tracing::warn!(
                    "[IBD_TIP_WALK_PROMOTE_SHALLOW] peer={} span={}-{} → claim={}-{} tip={} remain={} — GetData keep, not sticky (W65)",
                    peer_id,
                    start,
                    end,
                    next_needed,
                    end,
                    next_needed,
                    remain
                );
            }
        }
    }

    /// P5/A4: install peer scores used for gap routing and dual in-flight eligibility.
    pub(crate) fn set_peer_scores(&self, scores: &[(String, f64)]) {
        let mut g = self.peer_scores.lock().unwrap();
        g.clear();
        for (p, s) in scores {
            g.insert(p.clone(), *s);
        }
    }

    fn top_peer_in_flight_cap() -> usize {
        // Shared latch with `sole_tip_workers_per_peer` (mod.rs).
        super::top_peer_in_flight_cap()
    }

    /// A4: top-scoring half of peers may hold two chunks; others one.
    /// A6l: preferred sticky tip owner always gets the top-peer cap — tip downloaders
    /// floor at ~0.1 while idle ready workers sit ~0.19, so sticky falls below median,
    /// gets `max_in_flight=1`, cannot re-arm the next tip span while finishing the current
    /// chunk, and a higher-scored idle peer steals ownership (live A6k: 103.228→146.190
    /// at chunk boundary while sticky still streaming; sticky tenure BPS already only ~4.4).
    fn max_in_flight_for(&self, peer_id: &str) -> usize {
        // Synth bulk local-disk: single peer falls into scores.len()<4 → cap 1, which
        // serializes tip-owner complete→reassign. Cap 2 overlaps tip+next without the
        // 4-way ahead flood that buried tip in the FIFO (live collapse ~450→~8 @332k).
        if super::synthetic_wan::bulk_local_disk_stream()
            && super::download::is_local_disk_peer(peer_id)
        {
            return std::env::var("BLVM_IBD_SYNTH_LOCAL_IN_FLIGHT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(4)
                .clamp(2, 4);
        }
        if self.preferred_tip_owner().as_deref() == Some(peer_id) && self.tip_sticky_usable(peer_id)
        {
            // Mode T tip-priority: sticky max_in_flight from TOP_PEER_IN_FLIGHT (harness=1).
            // in_flight=2 flooded archive (tc172). Dual parked (tc168/171).
            // A2 attempt1 2026-08-09: sole-ready always-2 REVERT (tc220a2overlap tip90≈40
            // tip_gd@404≈1648 — flood; P0 baseline tip90≈59 with tip_gd@404 KEEP).
            // A2 v2 2026-08-09: in-place tip span extend REVERT (tc220a2v2cover tip90≈45.6
            // tip30≈126.7 < P0; gd_max=5144 — mega-span flood). Next: A1/A3 not cover-grow.
            if super::sole_tip_priority_enabled() {
                return Self::top_peer_in_flight_cap().clamp(1, 2);
            }
            return Self::top_peer_in_flight_cap();
        }
        let scores = self.peer_scores.lock().unwrap();
        Self::max_in_flight_for_scores(&scores, peer_id)
    }

    fn max_in_flight_for_scores(scores: &HashMap<String, f64>, peer_id: &str) -> usize {
        if scores.len() < 4 {
            return 1;
        }
        let my = scores.get(peer_id).copied().unwrap_or(0.0);
        let mut vals: Vec<f64> = scores.values().copied().collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = vals[vals.len() / 2];
        if my >= median {
            Self::top_peer_in_flight_cap()
        } else {
            1
        }
    }

    /// Rate-limited diag: sticky at flight cap cannot re-arm yet.
    fn log_sticky_cap_block(peer_id: &str, flight: usize, cap: usize) {
        static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_LOG_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < 2_000 {
            return;
        }
        LAST_LOG_MS.store(now_ms, Ordering::Relaxed);
        tracing::warn!(
            "[IBD_STICKY_CAP] sticky={} flight={}/{} — cannot re-arm tip yet",
            peer_id,
            flight,
            cap
        );
    }

    /// Rate-limited diag: non-sticky caller blocked while sticky finishes current span.
    fn log_sticky_busy_hold(sticky: &str, caller: &str) {
        static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_LOG_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < 2_000 {
            return;
        }
        LAST_LOG_MS.store(now_ms, Ordering::Relaxed);
        tracing::warn!(
            "[IBD_STICKY_HOLD] sticky={} at capacity — denying tip steal by {}",
            sticky,
            caller
        );
    }

    /// Whether `peer_id` currently has capacity for another in-flight range.
    fn peer_has_flight_capacity(
        &self,
        peer_id: &str,
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
    ) -> bool {
        if self.is_peer_blacklisted(peer_id) {
            return false;
        }
        let flight = Self::peer_flight_count(in_flight, peer_id);
        let cap = self.max_in_flight_for(peer_id);
        if flight < cap {
            return true;
        }
        // A31: TOP=1 sticky may take one after-tip stripe while tip cover is held.
        flight == 1 && self.a31_allow_frontier_dual(peer_id, in_flight)
    }

    /// A31 tip-frontier dual-slot (env `BLVM_IBD_TIP_FRONTIER_DUAL=1`).
    ///
    /// Rematch locks `TOP_PEER_IN_FLIGHT=1` → sticky `flight=1/1` blocks dens-shaped
    /// `C1g sticky after tip` (dens KEEP assigns after-tip every ~150ms @405k with TOP=2).
    /// A10 always-TOP=2 flooded Mode T. Allow a *second* in-flight only when the sole
    /// sticky flight already covers tip — `peer_already_tip` then forces the extra slot
    /// onto after-tip, not a second tip GetData.
    fn a31_frontier_dual_enabled() -> bool {
        latch_env!(bool, {
            matches!(
                std::env::var("BLVM_IBD_TIP_FRONTIER_DUAL").as_deref(),
                Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
            )
        })
    }

    fn a31_frontier_dual_cooldown_ms() -> u64 {
        // A42 distress default 1000ms; A41's 250ms still dens-rate dualed (×339).
        let default = if Self::a31_frontier_dual_distress_required() {
            1_000
        } else {
            250
        };
        let raw = latch_env!(Option<u64>, {
            std::env::var("BLVM_IBD_TIP_FRONTIER_DUAL_MS")
                .ok()
                .and_then(|s| s.parse().ok())
        });
        raw.unwrap_or(default).clamp(50, 5_000)
    }

    /// A42: dual only under tip_gd distress (default ON). Set
    /// `BLVM_IBD_TIP_FRONTIER_DUAL_DISTRESS=0` for A41-class always-on-when-landed.
    fn a31_frontier_dual_distress_required() -> bool {
        latch_env!(bool, {
            !matches!(
                std::env::var("BLVM_IBD_TIP_FRONTIER_DUAL_DISTRESS").as_deref(),
                Ok("0") | Ok("false") | Ok("FALSE") | Ok("off") | Ok("OFF") | Ok("no") | Ok("NO")
            )
        })
    }

    fn a31_frontier_dual_distress_gd_ms() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_FRONTIER_DUAL_GD_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(400)
                .clamp(100, 10_000)
        })
    }

    fn a31_frontier_dual_in_distress() -> bool {
        if super::download::tip_hole_gd_slow() {
            return true;
        }
        let thresh = Self::a31_frontier_dual_distress_gd_ms();
        match super::tip_stage::getdata_body_ewma_ms() {
            Some((ms, n)) if n >= 8 && ms >= thresh => true,
            _ => false,
        }
    }

    fn a31_allow_frontier_dual(
        &self,
        peer_id: &str,
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
    ) -> bool {
        if !Self::a31_frontier_dual_enabled() {
            return false;
        }
        if self.preferred_tip_owner().as_deref() != Some(peer_id) || !self.tip_sticky_usable(peer_id)
        {
            return false;
        }
        if Self::top_peer_in_flight_cap() >= 2 {
            return false;
        }
        let tip = self.next_needed_height();
        if !self.wan_tip_gap_crawl(tip) {
            return false;
        }
        // A39: always-on dual while tip covered → FORCE flood (tip90≈31). Dens C1g
        // after-tip runs when tip has *landed* (!gap_missing). Keep EMPTY freeze.
        let gap_missing = self.tip_gap_missing.load(Ordering::Relaxed)
            || super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed);
        if gap_missing {
            return false;
        }
        let Some(ranges) = in_flight.get(peer_id) else {
            return false;
        };
        if ranges.len() != 1 {
            return false;
        }
        let (s, e) = ranges[0];
        if s > tip || tip > e {
            return false;
        }
        let healthy = self.healthy_tip_cover_count(tip);
        let inflight_cover = Self::covering_next_count(in_flight, tip);
        if healthy == 0 && inflight_cover == 0 {
            return false;
        }
        // A42: A41 tip-landed+250ms still dualed through healthy tip_gd (~40ms) →
        // tip90≈45.6 < A34. Require getdata→body distress before second slot.
        let distress_req = Self::a31_frontier_dual_distress_required();
        let in_distress = Self::a31_frontier_dual_in_distress();
        if distress_req && !in_distress {
            return false;
        }
        // Dens spaces after-tip ~150ms; uncapped get_work polls stormed A31×55k.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let cooldown = Self::a31_frontier_dual_cooldown_ms();
        let prev = A31_FRONTIER_DUAL_LAST_ARM_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < cooldown {
            return false;
        }
        if A31_FRONTIER_DUAL_LAST_ARM_MS
            .compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        tracing::warn!(
            "[IBD_A31_FRONTIER_DUAL] sticky={} tip={} span={}-{} healthy={} inflight_cover={} cd_ms={} distress={} — allow after-tip slot under TOP=1",
            peer_id,
            tip,
            s,
            e,
            healthy,
            inflight_cover,
            cooldown,
            in_distress
        );
        true
    }

    /// W28c: tip ownership is sticky + best-scored, not first worker to call `get_work`.
    ///
    /// Live W28b: tip owner rotated across ~14 peers; slow peers kept winning the lottery.
    /// W33a: highest-scored non-blacklisted peer (WAN tip owner candidate).
    fn top_scored_peer_id(&self) -> Option<String> {
        let scores = self.peer_scores.lock().unwrap();
        let wan = self.wan_tip_gap_crawl(self.next_needed_height());
        scores
            .iter()
            .filter(|(p, _)| !self.is_peer_blacklisted(p))
            .filter(|(p, _)| !self.tip_owner_in_fail_cooldown(p))
            .filter(|(p, _)| !wan || self.peer_is_ibd_ready(p))
            .max_by(|(a, sa), (b, sb)| {
                sa.partial_cmp(sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            })
            .map(|(p, _)| p.clone())
    }

    /// Highest-scored ready ACTIVE download worker (workers). Used for open-slot
    /// re-arm so idle scored peers without get_work callers cannot win tip.
    fn top_scored_active_ready_worker(&self) -> Option<String> {
        let scores = self.peer_scores.lock().unwrap();
        scores
            .iter()
            .filter(|(p, _)| self.is_active_download_worker(p))
            .filter(|(p, _)| !self.is_peer_blacklisted(p))
            .filter(|(p, _)| !self.tip_owner_in_fail_cooldown(p))
            .filter(|(p, _)| self.peer_is_ibd_ready(p))
            .max_by(|(a, sa), (b, sb)| {
                sa.partial_cmp(sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            })
            .map(|(p, _)| p.clone())
    }

    /// E12/E13 OPEN_SLOT fallback: walk active download workers (not only `peer_scores`).
    /// Live E12: `top_scored` returned None while other ready workers existed → preferred=None
    /// lottery re-elected the GD_SLOW sticky after blacklist.
    fn any_ready_active_worker_except(&self, exclude: &str) -> Option<String> {
        let mut best: Option<(String, f64)> = None;
        for p in self.active_download_worker_ids() {
            if p == exclude
                || self.is_peer_blacklisted(&p)
                || self.tip_owner_in_fail_cooldown(&p)
                || !self.peer_is_ibd_ready(&p)
            {
                continue;
            }
            let s = self.peer_score_of(&p);
            if best.as_ref().map(|(_, bs)| s > *bs).unwrap_or(true) {
                best = Some((p, s));
            }
        }
        best.map(|(p, _)| p)
    }

    /// True when peer's in-flight ranges are all strictly ahead of tip (no tip cover).
    /// Must not lock `in_flight_per_peer` — callers may already hold it (`get_work`).
    fn peer_inflight_ahead_only_map(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        peer_id: &str,
        tip: u64,
    ) -> bool {
        let Some(ranges) = in_flight.get(peer_id) else {
            return false;
        };
        !ranges.is_empty() && ranges.iter().all(|(s, _)| *s > tip)
    }

    /// W126: covering=0 TIP_PIN candidate — prefer idle tip-STREAM, then idle top score.
    /// Live W125 @326975: pin top_w mid W35 `327039-70` (`max_in_flight=1`) → covering=0
    /// for 16s while ready=51; tip60-watch killed at rate 27.2.
    fn best_covering0_tip_pin_candidate(&self, tip: u64) -> Option<String> {
        let inflight = self.in_flight_per_peer.lock().unwrap().clone();
        // 1) Idle tip-STREAM hero.
        if let Some((cand, _)) = self.best_a6n_tip_candidate("") {
            if self.peer_has_flight_capacity(&cand, &inflight)
                && !Self::peer_inflight_ahead_only_map(&inflight, &cand, tip)
            {
                return Some(cand);
            }
        }
        // 2) Idle highest-scored ready active worker.
        let scores = self.peer_scores.lock().unwrap().clone();
        let mut idle: Option<(String, f64)> = None;
        for (p, s) in &scores {
            if !self.is_active_download_worker(p)
                || self.is_peer_blacklisted(p)
                || self.tip_owner_in_fail_cooldown(p)
                || !self.peer_is_ibd_ready(p)
            {
                continue;
            }
            if !self.peer_has_flight_capacity(p, &inflight)
                || Self::peer_inflight_ahead_only_map(&inflight, p, tip)
            {
                continue;
            }
            if idle.as_ref().map(|(_, bs)| *s > *bs).unwrap_or(true) {
                idle = Some((p.clone(), *s));
            }
        }
        if let Some((p, _)) = idle {
            return Some(p);
        }
        // 3) Fall back to top scored (caller may release ahead-only).
        self.top_scored_active_ready_worker()
    }

    /// W126: idle tip-STREAM peer exists (for W113 open-slot gate).
    /// Uses caller's in-flight map — never re-locks (W126a: get_work holds the mutex).
    fn idle_tip_streamer_exists(
        &self,
        tip: u64,
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
    ) -> bool {
        for peer_id in self.active_download_worker_ids() {
            if !self.peer_is_ibd_ready(&peer_id)
                || self.is_peer_blacklisted(&peer_id)
                || self.tip_owner_in_fail_cooldown(&peer_id)
            {
                continue;
            }
            if self.wan_tip_stream_bps(&peer_id) <= 0.0 {
                continue;
            }
            if self.peer_has_flight_capacity(&peer_id, in_flight)
                && !Self::peer_inflight_ahead_only_map(in_flight, &peer_id, tip)
            {
                return true;
            }
        }
        false
    }

    /// Ready active worker with score `> min_score`.
    /// `ignore_cooldown`: W95 — when gating *unproven* tip owners, still count cooled
    /// mid-peers as alternatives so CAP cooldowns cannot open a score=0.001 lottery
    /// (live W94 freeze @313142: mid pool cooled → 0.001 owners × CAP=8 → 23s).
    fn active_ready_worker_above(&self, min_score: f64, ignore_cooldown: bool) -> Option<String> {
        let scores = self.peer_scores.lock().unwrap();
        scores
            .iter()
            .filter(|(p, s)| {
                **s > min_score
                    && self.is_active_download_worker(p)
                    && !self.is_peer_blacklisted(p)
                    && (ignore_cooldown || !self.tip_owner_in_fail_cooldown(p))
                    && self.peer_is_ibd_ready(p)
            })
            .max_by(|(a, sa), (b, sb)| {
                sa.partial_cmp(sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            })
            .map(|(p, _)| p.clone())
    }

    /// W82: any ready active worker with score above the floor/mid band.
    fn active_ready_worker_above_mid(&self) -> Option<String> {
        let scores = self.peer_scores.lock().unwrap();
        scores
            .iter()
            .filter(|(p, s)| {
                **s > Self::TIP_OWNER_MID_SCORE
                    && self.is_active_download_worker(p)
                    && !self.is_peer_blacklisted(p)
                    && self.peer_is_ibd_ready(p)
            })
            .max_by(|(a, sa), (b, sb)| {
                sa.partial_cmp(sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            })
            .map(|(p, _)| p.clone())
    }

    /// Prefer `preferred_tip_owner` while available; otherwise the highest-scored peer with
    /// capacity. During failover (covering already ≥1), any score-ok peer may take the
    /// second covering slot.
    fn peer_may_take_tip_owner(
        &self,
        peer_id: &str,
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        covering_next: usize,
    ) -> bool {
        // Dead sticky must not block open-slot / score race (also opens tip slot).
        self.drop_unusable_preferred_tip_sticky();
        // W92: CAP/tip-fail cooldown — must beat TIP_PIN re-elect of same top_w.
        if self.tip_owner_in_fail_cooldown(peer_id) {
            return false;
        }
        let wan = self.wan_tip_gap_crawl(
            self.validation_height
                .load(Ordering::Relaxed)
                .saturating_add(1),
        );
        // W95: refuse unproven deep tip owners while any ready active worker (including
        // fail-cooled mid-peers) scores above the unproven band.
        // W127: when covering==0, do **not** count fail-cooled peers as blockers —
        // live W126b @337k / W126c mute-CAP thrash: mid pool cooled → floor denied →
        // OPEN_STALL ready=50 covering=0 with zero assignees until cooldown expired.
        let mid_alt_ignore_cooldown = covering_next > 0;
        // Strict `< UNPROVEN`: score==0.05 is A6k below-floor usable sticky (p0a_below_floor),
        // not the unproven@0.001 lottery W95 was written to block.
        if wan
            && self.peer_score_of(peer_id) < Self::TIP_OWNER_UNPROVEN_SCORE
            && self
                .active_ready_worker_above(
                    Self::TIP_OWNER_UNPROVEN_SCORE,
                    mid_alt_ignore_cooldown,
                )
                .is_some()
        {
            return false;
        }
        // W36/P0-A open slot: handshake-ready + top-half; sticky first unless A6d upgrade.
        // Live A6b soak: any top-half ready peer → score=1.0 lottery after covering=0 nudge
        // (8 owners / 5 min, ~4.6 blk/s). Idle scored peers must not win — no get_work caller.
        // A6d: sticky score=1.000 locked out breakthrough-class peers → ~6 blk/s.
        // A6e: preferred=None must NOT require sole top_w — that worker may not be polling
        // (live: post-SLA 1666 nudges / 0 assigns while ready=50, busy=0).
        if wan && self.tip_owner_open.load(Ordering::Relaxed) {
            if !self.peer_is_ibd_ready(peer_id) {
                return false;
            }
            if let Some(ref pref) = self.preferred_tip_owner.lock().unwrap().clone() {
                // A6h/A6k: usable sticky (ready+active) owns open tip — even below peer_ok floor.
                // W65: shallow walk-promote sticky does not own the open tip slot.
                if self.tip_sticky_usable(pref) && self.peer_holds_substantial_tip_cover(pref) {
                    if let Some(top_w) = self.top_scored_active_ready_worker() {
                        if self.tip_owner_should_upgrade(pref, &top_w) {
                            return peer_id == top_w && self.peer_ok_for_gap_race(peer_id);
                        }
                    }
                    return peer_id == pref;
                }
                // A6k dens KEEP (p0a_below_floor): usable preferred with covering=0 must
                // still re-arm tip — W82 floor refuse would leave preferred stuck forever.
                if self.tip_sticky_usable(pref) && peer_id == pref {
                    return true;
                }
            }
            // No usable sticky: top-half ready ACTIVE worker may re-arm (first caller).
            // A6e: do not require sole top_w — that worker may not be polling (post-SLA
            // covering=0 forever). W82: still refuse *floor-score* peers while any mid/high
            // ready active worker exists — live mid-chain WAN: open-slot lottery elected
            // score=0.001 stickies → 25s TIP_SLA stalls (tip=347948 holes=20, tip60≈1).
            if !self.is_active_download_worker(peer_id) || !self.peer_ok_for_gap_race(peer_id)
            {
                return false;
            }
            // W95: ignore fail-cooldown when detecting mid alternatives (covering>0).
            // W127: covering==0 → only live (non-cooled) mids block floor peers.
            if self.peer_score_of(peer_id) <= Self::TIP_OWNER_MID_SCORE
                && self
                    .active_ready_worker_above(
                        Self::TIP_OWNER_MID_SCORE,
                        mid_alt_ignore_cooldown,
                    )
                    .is_some()
            {
                return false;
            }
            // W113: empty tip + proven tip-STREAM peer ready → refuse non-streamers for
            // deep owner. Live W112b @331209: open-slot elected score=0.100 while ready=62
            // included tip STREAM heroes; covering=3 still rate-failed 33.5 vs 35.
            // W126: only refuse when an *idle* streamer exists — if the STREAM hero is
            // mid W35 ahead (`max_in_flight=1`), blocking others freezes covering=0
            // (live W125 @326975, 16s OPEN_STALL).
            let tip_h = self.next_needed_height();
            if covering_next == 0
                && self.empty_tip_owner_prefer_streamer()
                && self.idle_tip_streamer_exists(tip_h, in_flight)
                && !self.peer_recently_tip_streaming(
                    peer_id,
                    Duration::from_secs(Self::tip_stream_owner_hot_secs()),
                )
            {
                return false;
            }
            return true;
        }
        // A6k: preferred sticky re-arms next span without clearing peer_ok floor.
        if wan {
            if let Some(ref pref) = self.preferred_tip_owner.lock().unwrap().clone() {
                if pref == peer_id && self.tip_sticky_usable(pref) {
                    return true;
                }
            }
        }
        if !self.peer_ok_for_gap_race(peer_id) {
            return false;
        }
        if wan && !self.peer_is_ibd_ready(peer_id) {
            return false;
        }
        // W33a/W47: on WAN tip gap only the top-scored peer may own the deep pipeline —
        // except during tip-distress race (soft-retry / late body; not bridge holes).
        // TPP L1 REVERT (300→320 L1 cell): wall 377<C0 390 — manual undo peer_may C1g.
        let tip_distress = Self::tip_is_distressed()
            || self.c1t_tip_height_race()
            || Self::tip_hole_race_holes_opt().is_some_and(|thr| {
                self.tip_bridge_holes.load(Ordering::Relaxed) >= thr
            });
        if wan {
            let distress_race = covering_next >= 1 && tip_distress;
            if !distress_race {
                if let Some(top) = self.top_scored_peer_id() {
                    if peer_id != top {
                        return false;
                    }
                }
            }
        }
        // Failover racer: primary already covers tip; score-ok peer may take slot 2.
        // W30/W37/W47: second slot only while tip is distressed (soft-retry / late body).
        // C1t: also after tip missing ≥C1T_MS under gd-fast (tip height only).
        // W180: still apply W95 floor/unproven gates — live W179b mute CAP + distress armed
        // score=0.001 owners while mid+ workers existed (ready_active_ok=10).
        if covering_next >= 1 && tip_distress {
            if wan
                && self.peer_score_of(peer_id) <= Self::TIP_OWNER_MID_SCORE
                && self
                    .active_ready_worker_above(Self::TIP_OWNER_MID_SCORE, true)
                    .is_some()
            {
                return false;
            }
            return true;
        }

        // Preferred sticky while covering is empty.
        let preferred = self.preferred_tip_owner.lock().unwrap().clone();
        if let Some(ref pref) = preferred {
            if pref == peer_id && self.tip_sticky_usable(pref) {
                return true;
            }
            // A6l: wait for usable sticky even when it is at capacity (finishing current span).
            // Prior logic only waited while sticky had spare capacity — so a busy sticky with
            // max_in_flight=1 yielded tip to idle higher-scored peers at every chunk boundary
            // (live A6k: STICKY_DROP=0 but ownership lottery after first sticky stretch).
            // Hung sticky is rotated by tip-SLA / blacklist / tip_owner_open, not by steal.
            // W65: do not wait on a sticky whose only tip claim is a shallow walk-promote.
            if self.tip_sticky_usable(pref) && self.peer_holds_substantial_tip_cover(pref) {
                if !self.peer_has_flight_capacity(pref, in_flight) {
                    Self::log_sticky_busy_hold(pref, peer_id);
                }
                return false;
            }
        }

        let scores = self.peer_scores.lock().unwrap().clone();
        if scores.len() < 2 {
            return true;
        }
        let my = scores.get(peer_id).copied().unwrap_or(0.0);
        // Deny if any higher-scored (or tie-break winner) peer has capacity.
        for (p, s) in &scores {
            if p.as_str() == peer_id {
                continue;
            }
            let better = *s > my
                || ((*s - my).abs() < f64::EPSILON && p.as_str() < peer_id);
            if !better {
                continue;
            }
            if self.is_peer_blacklisted(p) {
                continue;
            }
            // P0-A: higher-scored peer must be IBD-ready to block this peer on WAN gap.
            if wan && !self.peer_is_ibd_ready(p) {
                continue;
            }
            let cap = Self::max_in_flight_for_scores(&scores, p);
            if Self::peer_flight_count(in_flight, p) >= cap {
                continue;
            }
            // Better peer must also clear the gap-race floor.
            let mut vals: Vec<f64> = scores.values().copied().collect();
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let wan = self.wan_tip_gap_crawl(
                self.validation_height
                    .load(Ordering::Relaxed)
                    .saturating_add(1),
            );
            let floor = if wan {
                vals[vals.len() / 2]
            } else {
                vals[vals.len() / 4]
            };
            if *s < floor {
                continue;
            }
            return false;
        }
        true
    }

    /// P5: bottom-quartile peers skip gap preempt / single-height micro races.
    ///
    /// **W28:** during WAN tip crawl, require **top-half** scores (median+) — bottom half
    /// still gets main-queue ahead work, but not the tip pipeline. Live: 8 racers included
    /// slow peers that soft-retried for 12s while covering looked "healthy".
    ///
    /// **A6e:** on WAN, median is taken over **active download workers only**. Live A6d score
    /// refresh injected idle network peers at default score=1.0 into `peer_scores`; tip
    /// workers dropped to ~0.1–0.2 after `record_block`, failed the polluted median, and
    /// open-slot assign died (covering=0 forever after tip-SLA).
    ///
    /// **A6j:** WAN median prefers **ready** active workers. Live A6i OPEN_STALL:
    /// `top_w_score=0.127 floor=0.153 ready_active_ok=0/9` — disconnected high scorers
    /// inflated the floor above every live worker → permanent covering=0.
    fn peer_ok_for_gap_race(&self, peer_id: &str) -> bool {
        let my = {
            let scores = self.peer_scores.lock().unwrap();
            scores.get(peer_id).copied().unwrap_or(0.0)
        };
        let wan = self.wan_tip_gap_crawl(
            self.validation_height
                .load(Ordering::Relaxed)
                .saturating_add(1),
        );
        if wan {
            let mut vals = self.wan_gap_score_floor_vals();
            if vals.len() < 4 {
                return true;
            }
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            // A6k: use Q1 (same as non-WAN), not median. Tip downloaders floor at ~0.1 while
            // lightly-proven ready workers sit at ~0.19; median (~0.18) excluded the delivering
            // sticky and forced owner lottery (live: same-peer re-arms 5 vs 146).
            let floor = vals[vals.len() / 4];
            return my >= floor;
        }
        let scores = self.peer_scores.lock().unwrap();
        let mut vals: Vec<f64> = scores.values().copied().collect();
        if vals.len() < 4 {
            return true;
        }
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let floor = vals[vals.len() / 4]; // Q1 — not bottom quartile
        my >= floor
    }

    fn stall_requeue_debounce() -> Duration {
        std::env::var("BLVM_IBD_STALL_REQUEUE_DEBOUNCE")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(5))
    }

    /// Blacklist a peer for `duration`. During this window, `get_work` will not assign it chunks.
    pub(crate) fn blacklist_peer(&self, peer_id: &str, duration: Duration) {
        let until = Instant::now() + duration;
        let mut bl = self.blacklisted_until.lock().unwrap();
        let entry = bl.entry(peer_id.to_string()).or_insert(until);
        if until > *entry {
            *entry = until;
        }
        tracing::warn!(
            "IBD: blacklisted peer {} for {}s",
            peer_id,
            duration.as_secs()
        );
    }

    /// Returns true if the peer is currently blacklisted.
    pub(crate) fn is_peer_blacklisted(&self, peer_id: &str) -> bool {
        let mut bl = self.blacklisted_until.lock().unwrap();
        if let Some(until) = bl.get(peer_id) {
            if Instant::now() < *until {
                return true;
            }
            bl.remove(peer_id);
        }
        false
    }

    /// Mark bootstrap chunk (0..N) complete — enables parallel chunk assignment for start_height > 0.
    pub(crate) fn mark_bootstrap_complete(&self) {
        self.bootstrap_complete.store(true, Ordering::Relaxed);
    }

    /// Max peers whose in-flight range may **cover** `next_needed` (tip owners).
    ///
    /// **W28b:** default **1**. Racing N peers on the same tip height wastes bandwidth and
    /// still waits on the slowest useful response. Other peers should take **non-overlapping
    /// ahead partitions** (see tip-fill path), not duplicate the tip. Override via
    /// `BLVM_IBD_GAP_FETCHERS`.
    ///
    /// **W28c:** when tip soft-retry / tip-owner failure arms failover, allow **2** so a
    /// second peer can cover the tip without standing 8-way tip races.
    ///
    /// **W31:** on WAN tip gap default **1**. During tip soft-retry ahead-freeze, allow **2**
    /// so a second peer can race the tip height (live: covering=1 zombie while soft-retrying
    /// 30–40s → applied stalls; ahead freeze alone only lifted mean ~4→~8 blk/s).
    ///
    /// **W37:** non-WAN (LOCAL_AHEAD soft-resume) must also require ahead-freeze — armed
    /// alone sticks across polls and opens a permanent covering=2/2 (H,H) treadmill.
    fn max_gap_fetchers_per_height(&self) -> usize {
        let next_needed = self.next_needed_height();
        if self.wan_tip_gap_crawl(next_needed) {
            // W112: empty-bridge tip starve — deep + 2 failover micros.
            if self.empty_tip_triple_race() {
                return 3;
            }
            // W47: soft-retry or late tip body — allow one failover micro.
            if Self::tip_is_distressed() {
                return 2;
            }
            // W41: no deep tip owner → allow a second racer to re-arm (COVERING_ZERO /
            // boundary gap). Once a deep claim exists, back to single tip-height pipe.
            if self.deep_tip_cover_count(next_needed) == 0 {
                return 2;
            }
            // C1h: tip body still missing — deep owner stripe must NOT pin fetchers_cap=1.
            // Live C1g: freeze past-tip + cap=1 → no (H,H) race → ~3 BPS EMPTY_TIP.
            if self.tip_gap_missing.load(Ordering::Relaxed)
                && super::tip_stage::tip_awaiting_secs_for_cap()
                    >= Self::c1g_tip_race_await_secs()
            {
                return 2;
            }
            // C1t: sub-second tip-height race (gd-fast only).
            if self.c1t_tip_height_race() {
                return 2;
            }
            // Optional emergency only (`BLVM_IBD_TIP_HOLE_RACE_HOLES`).
            if Self::tip_hole_race_holes_opt().is_some_and(|thr| {
                self.tip_bridge_holes.load(Ordering::Relaxed) >= thr
            }) {
                return 2;
            }
            return 1;
        }
        let base = latch_env!(usize, {
            std::env::var("BLVM_IBD_GAP_FETCHERS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1)
                .clamp(1, 32)
        });
        if super::tip_stage::tip_ahead_frozen_for_soft_retry() {
            base.max(2)
        } else {
            base
        }
    }

    /// W31: on WAN tip gap, stall-recovery retry covering `next_needed` goes to sticky owner only.
    fn peer_may_take_wan_gap_retry(&self, peer_id: &str, start: u64, end: u64) -> bool {
        let next_needed = self.next_needed_height();
        if !self.wan_tip_gap_crawl(next_needed) {
            return true;
        }
        if start > next_needed || end < next_needed {
            return true;
        }
        self.drop_unusable_preferred_tip_sticky();
        match self.preferred_tip_owner() {
            Some(ref owner) if self.tip_sticky_usable(owner) => owner.as_str() == peer_id,
            // P0-B: no usable sticky — do not fan tip *bulks*. Tip-height `(H,H)` micro
            // (FORCE_REQUEUE covering=0) may go to any ready active worker.
            _ => {
                start == next_needed
                    && end == next_needed
                    && self.peer_is_ibd_ready(peer_id)
                    && self.is_active_download_worker(peer_id)
                    && !self.tip_owner_in_fail_cooldown(peer_id)
                    && !self.is_peer_blacklisted(peer_id)
            }
        }
    }

    /// W31: retry-path tip claims on WAN gap only for sticky owner (avoids phantom healthy=2).
    fn maybe_note_tip_cover_claim_retry(&self, peer_id: &str, start: u64, end: u64) {
        let next_needed = self.next_needed_height();
        if !(start <= next_needed && next_needed <= end) {
            return;
        }
        if !self.wan_tip_gap_crawl(next_needed) {
            self.note_tip_cover_claim(peer_id, start, end);
            return;
        }
        let pref = self.preferred_tip_owner();
        let ok = match pref.as_deref() {
            Some(owner) if self.tip_sticky_usable(owner) => owner == peer_id,
            _ => {
                start == next_needed
                    && end == next_needed
                    && self.is_active_download_worker(peer_id)
            }
        };
        if ok {
            self.note_tip_cover_claim(peer_id, start, end);
        }
    }

    /// Highest end among in-flight ranges that overlap `[next_needed, next_needed+window]`.
    /// Used to place the next non-overlapping ahead partition after the tip pipeline.
    ///
    /// **Caveat (C1e):** this is the *assigned* span end. Tip owner often claims tip..tip+127
    /// while download only GetData's tip_hole depth (8–32) → frontier jumps to tip+127 and
    /// other peers open tip+128 with tip still empty (Swiss cheese). Prefer
    /// [`Self::tip_contiguous_assign_frontier`] + [`Self::tip_runway_cap`] while tip missing.
    fn tip_pipeline_frontier(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        next_needed: u64,
        window: u64,
    ) -> u64 {
        let hi = next_needed.saturating_add(window);
        let mut frontier = next_needed.saturating_sub(1);
        for (s, e) in in_flight.values().flatten() {
            if *e >= next_needed && *s <= hi {
                frontier = frontier.max(*e);
            }
        }
        frontier
    }

    /// C1e: max contiguous assign band from tip while tip body missing (multi-peer stripes).
    /// Default **96**. Env `BLVM_IBD_TIP_RUNWAY_CAP` (clamp 32–256).
    pub(crate) fn tip_runway_cap() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_RUNWAY_CAP")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(96)
                .clamp(32, 256)
        })
    }

    /// C1e: stripe width for tip-owner / ahead peers inside the runway (default **32** = C1b).
    pub(crate) fn tip_runway_stripe() -> u64 {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_RUNWAY_STRIPE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(32)
                .clamp(8, 128)
        });
        raw.min(Self::tip_runway_cap())
    }

    /// Raw `GAP_PREEMPT_BATCH` when set (defaults applied at call sites).
    fn gap_preempt_batch_raw() -> Option<u64> {
        latch_env!(Option<u64>, {
            std::env::var("BLVM_IBD_GAP_PREEMPT_BATCH")
                .ok()
                .and_then(|s| s.parse().ok())
        })
    }

    /// Raw `TIP_PARTITION_WINDOW` when set (WAN/local apply different defaults/clamps).
    fn tip_partition_window_raw() -> Option<u64> {
        latch_env!(Option<u64>, {
            std::env::var("BLVM_IBD_TIP_PARTITION_WINDOW")
                .ok()
                .and_then(|s| s.parse().ok())
        })
    }

    /// Mousetrap: log assign_F vs pipe_F vs body_F on tip-band secondary opens.
    fn log_pipe_f(
        tip: u64,
        assign_f: u64,
        pipe_f: u64,
        body_f: u64,
        peer_id: &str,
        reason: &str,
    ) {
        let gap = assign_f.saturating_sub(pipe_f);
        tracing::warn!(
            "[IBD_PIPE_F] tip={} assign_f={} pipe_f={} body_f={} gap={} peer={} reason={}",
            tip,
            assign_f,
            pipe_f,
            body_f,
            gap,
            peer_id,
            reason
        );
    }

    /// C1e: walk assigned in-flight ranges from `next_needed` while contiguous; stop at hole
    /// or `runway_end`. Returns last covered end, or `next_needed-1` if tip uncovered.
    /// Multiple peers' non-overlapping stripes compose one contiguous runway.
    fn tip_contiguous_assign_frontier(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        next_needed: u64,
        runway_end: u64,
    ) -> u64 {
        if runway_end < next_needed {
            return next_needed.saturating_sub(1);
        }
        let mut ranges: Vec<(u64, u64)> = in_flight
            .values()
            .flatten()
            .filter_map(|&(s, e)| {
                let cs = s.max(next_needed);
                let ce = e.min(runway_end);
                (cs <= ce).then_some((cs, ce))
            })
            .collect();
        if ranges.is_empty() {
            return next_needed.saturating_sub(1);
        }
        ranges.sort_by_key(|&(s, _)| s);
        let mut cursor = next_needed;
        let mut frontier = next_needed.saturating_sub(1);
        for (s, e) in ranges {
            if s > cursor {
                break;
            }
            if e >= cursor {
                frontier = frontier.max(e);
                cursor = e.saturating_add(1);
            }
            if cursor > runway_end {
                break;
            }
        }
        frontier
    }

    fn range_overlaps_inflight(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        start: u64,
        end: u64,
    ) -> bool {
        in_flight
            .values()
            .flatten()
            .any(|(s, e)| *s <= end && start <= *e)
    }

    /// P1c: true when `peer_id` already holds an in-flight range covering `next_needed`.
    fn peer_holds_tip_inflight(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        peer_id: &str,
        next_needed: u64,
    ) -> bool {
        in_flight.get(peer_id).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|&(s, e)| s <= next_needed && next_needed <= e)
        })
    }

    /// On coordinator stall, prefetch this many consecutive gap heights so peers can race ahead
    /// of validation instead of one micro-chunk per stall tick.
    fn gap_micro_chunk_batch(&self) -> u64 {
        std::env::var("BLVM_IBD_GAP_MICRO_BATCH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(if self.work_stealing { 32 } else { 8 })
            .clamp(1, 128)
    }

    pub(crate) fn is_bootstrap_complete(&self) -> bool {
        self.bootstrap_complete.load(Ordering::Relaxed)
    }

    /// Returns true if any worker is already downloading this exact chunk range.
    ///
    /// W15: for multi-block ranges that cover `next_needed`, also treat *overlapping*
    /// covering ranges as in-flight — stall recovery used to enqueue 684849-864,
    /// 684850-864, 684851-864 as tip advanced; each exact `(s,e)` differed so all
    /// assigned, blowing past `max_gap_fetchers` (live: 10+ overlapping bulks).
    fn chunk_range_in_flight(
        &self,
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        start: u64,
        end: u64,
    ) -> bool {
        if start == end {
            let n = in_flight
                .values()
                .flatten()
                .filter(|(s, e)| *s == start && *e == end)
                .count();
            return n >= self.max_gap_fetchers_per_height();
        }
        if in_flight
            .values()
            .flatten()
            .any(|(s, e)| *s == start && *e == end)
        {
            return true;
        }
        // W15: overlapping multi-block covering the same tip height counts toward cap.
        let next_needed = self.validation_height.load(Ordering::Relaxed).saturating_add(1);
        if start <= next_needed && next_needed <= end {
            let covering = Self::covering_next_count(in_flight, next_needed);
            if covering >= self.max_gap_fetchers_per_height() {
                return true;
            }
        }
        false
    }

    fn covering_next_count(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        next_needed: u64,
    ) -> usize {
        in_flight
            .values()
            .flatten()
            .filter(|(s, e)| *s <= next_needed && next_needed <= *e)
            .count()
    }

    fn peer_flight_count(in_flight: &HashMap<String, Vec<(u64, u64)>>, peer_id: &str) -> usize {
        in_flight.get(peer_id).map(|v| v.len()).unwrap_or(0)
    }

    fn insert_in_flight(
        in_flight: &mut HashMap<String, Vec<(u64, u64)>>,
        peer_id: &str,
        start: u64,
        end: u64,
    ) {
        in_flight
            .entry(peer_id.to_string())
            .or_default()
            .push((start, end));
    }

    /// Returns the next assignable chunk for this peer, or None if nothing ready.
    /// Per-peer serial: returns None if this peer already has a chunk in flight (eliminates chunk-boundary stalls).
    /// Round-robin: prioritizes critical chunk (containing next_needed) from retry, then earliest available.
    /// CRITICAL: Entire operation under one lock to prevent duplicate chunk assignment (race: two workers
    /// for same peer both getting chunk 116240-116255, both requesting same blocks, one starves).
    pub(crate) fn get_work(&self, peer_id: &str, max_ahead: u64) -> Option<(u64, u64)> {
        // IBD range complete or explicit teardown — no new work (Phase 3 must proceed).
        if self.shutdown.load(Ordering::Acquire) || self.validation_reached_ibd_end() {
            return None;
        }
        // E1: while ladder export isolation is on, do not issue new GetData — let
        // checkpoint compact/write own the disk. In-flight chunks may still finish.
        if crate::node::parallel_ibd::export_isolation_active() {
            return None;
        }

        let bootstrap_done = self.bootstrap_complete.load(Ordering::Relaxed);
        let current_validation = self.validation_height.load(Ordering::Relaxed);
        let next_needed = current_validation + 1;
        let max_start = current_validation.saturating_add(max_ahead);

        // Bootstrap serialization: until bootstrap chunk completes, only assign chunks with start==0
        let allow_chunk = |start: u64| bootstrap_done || start == self.start_height;

        // Blacklisted peers get no work until their cooldown expires.
        if self.is_peer_blacklisted(peer_id) {
            return None;
        }

        // W80: clear behind-tip retry junk before tip fill / retry / main queue.
        self.purge_obsolete_retries(next_needed);

        // Single lock: check in-flight + find chunk + insert. Prevents duplicate assignment.
        let t_inflight_wait = Instant::now();
        let mut guard = self.in_flight_per_peer.lock().unwrap();
        let _gw_timer = AssignerGetWorkTimer::start(
            t_inflight_wait.elapsed().as_nanos() as u64,
        );
        let cap = self.max_in_flight_for(peer_id);
        let mut flight = Self::peer_flight_count(&guard, peer_id);
        if flight >= cap && !(flight == 1 && self.a31_allow_frontier_dual(peer_id, &guard)) {
            // A51: release zombie tip cover under EMPTY+TOP=1 (not dual second slot).
            if self.try_sole_empty_tip_release(peer_id, &mut guard) {
                flight = Self::peer_flight_count(&guard, peer_id);
            } else {
                if self.preferred_tip_owner().as_deref() == Some(peer_id) {
                    Self::log_sticky_cap_block(peer_id, flight, cap);
                }
                return None;
            }
            if flight >= cap {
                if self.preferred_tip_owner().as_deref() == Some(peer_id) {
                    Self::log_sticky_cap_block(peer_id, flight, cap);
                }
                return None;
            }
        }

        // W4/W16/W28b/W28c/W28d tip fill: sticky tip owner + non-overlapping ahead partitions.
        //
        // Multi-peer is useful when peers download **different** heights. Racing the same tip
        // (W18 covering=8) is not. Hornet pipelines deep on one peer; we keep multi-peer by
        // partitioning the ahead window. W28c: sticky best owner; soft-retry arms one failover
        // covering peer (tip height only), not a standing N-way tip lottery.
        //
        // W28d: ahead partitions that tip walks into do NOT count as healthy tip coverage —
        // only explicit tip-owner/failover claims do. Live W28c: 0 deep tip owners past body
        // tip because covering stayed at 2 from walk-ins; only (H,H) failover ran.
        //
        // Synth bulk: tip-fill stays enabled (skipping it regressed wall to ~7–9 BPS — W16
        // tip_band + tip-reserve spin). Overlap guard below blocks same-span reassign storms
        // (live ~30k W28c reassigns + GAP_STREAM ~3.4× overread → 350→400 wall 178).
        if allow_chunk(next_needed) {
            let containing = self
                .chunks
                .iter()
                .find(|(s, e)| *s <= next_needed && next_needed <= *e)
                .copied();
            // P0: on WAN tip gap, tip owner must assign even if next_needed walked past the
            // static chunk map (headers advanced after IBD start). Synthetic containing range.
            let containing = containing.or_else(|| {
                if self.wan_tip_gap_crawl(next_needed) || self.handoff_prime_active(next_needed) {
                    let ht = self.header_tip();
                    let end = if ht >= next_needed {
                        next_needed
                            .saturating_add(255)
                            .min(ht)
                    } else {
                        // No headers past tip yet — still advertise tip height so assign
                        // can wait / clip rather than invent tip+255 past store.
                        next_needed
                    };
                    Some((next_needed, end))
                } else {
                    None
                }
            });
            if let Some((cs, ce)) = containing {
                let raw_covering = Self::covering_next_count(&guard, next_needed);
                let at_chunk_start = next_needed == cs;
                let wan_gap = self.wan_tip_gap_crawl(next_needed);
                let handoff_prime = self.handoff_prime_active(next_needed);
                // W30/W37: tip owner gating uses deep claims only — (H,H) failover micros
                // must not block deep re-arm on WAN gap *or* LOCAL_AHEAD soft-resume
                // (live 2026-07-16: covering=2/2 (H,H) treadmill, 0 deep owners, ~0.2 blk/s).
                // W4/N12: one tip_cover_claims snapshot for deep+healthy counts (re-snap after promote).
                // C1u: GetData-prime body_tip+1 while still local.
                // (1) near_tip window (HANDOFF_PRIME of body) — only after local tip is
                //     already covered. Live FAIL true-wan-…T004239Z: early near_tip prime
                //     stole sticky max_in_flight=1 onto body_tip+1 while next_needed≪tip
                //     and W28c ahead cheese'd the hole → freeze wait 437206 (tip60 never).
                // (2) ahead frontier already at body tip (far local dens KEEP) — otherwise
                //     tip-fill re-arms next_needed forever and never reaches ahead_frontier.
                if !wan_gap {
                    let body_tip_c1u = self.wan_body_tip.load(Ordering::Relaxed);
                    let frontier_at_body = body_tip_c1u > 0
                        && next_needed <= body_tip_c1u
                        && Self::tip_pipeline_frontier(&guard, next_needed, 2048) >= body_tip_c1u;
                    if handoff_prime || frontier_at_body {
                        let tip_covered = Self::covering_next_count(&guard, next_needed) > 0;
                        // near_tip: only on the last local height (next>=body_tip) with cover.
                        // frontier_at_body may still prime earlier (contig already at body tip).
                        let near_tip_ready =
                            tip_covered && next_needed >= body_tip_c1u;
                        if frontier_at_body || (handoff_prime && near_tip_ready) {
                            let reason = if handoff_prime && near_tip_ready {
                                "near_tip"
                            } else {
                                "ahead_frontier"
                            };
                            if let Some(range) = self.try_assign_handoff_prime(
                                peer_id,
                                next_needed,
                                &mut guard,
                                reason,
                            ) {
                                drop(guard);
                                return Some(range);
                            }
                        }
                    }
                }
                // Stale tip_cover prune: tried 2026-08-03 (exact + tip-cover variants).
                // CLAIM_STALE never armed on tip-now; claimfix tip90≈61–63 REVERT vs
                // forcedeb 108.9. Manual REVERT — keep force debounce + tip_nudge gate.
                let mut tip_claims = self.snapshot_tip_cover_claims();
                let mut effective_healthy =
                    Self::deep_tip_cover_count_from(&tip_claims, next_needed);
                // W49b: ahead walk-in already covers tip in-flight — promote before assigning
                // a competing tip owner (closes race: promote only on abort tick after W28d).
                // W111: skip cooldown peers (mute residual in-flight must not re-sticky).
                if wan_gap && effective_healthy == 0 {
                    if let Some((wp, ws, we)) =
                        Self::find_inflight_deep_covering(&guard, next_needed)
                    {
                        if !self.tip_owner_in_fail_cooldown(&wp) {
                            self.promote_tip_walk_in(&wp, ws, we);
                            tip_claims = self.snapshot_tip_cover_claims();
                            effective_healthy =
                                Self::deep_tip_cover_count_from(&tip_claims, next_needed);
                        }
                    }
                }
                // W28c/W32/W35‴: WAN tip owner deep pipe in one download session.
                // Near tip: tip-owner batch must match GetData pipe depth (default 128).
                // Live 2026-07-15: `wan_bulk_catchup` is true for most of IBD (header tip ≫
                // next_needed) so this path used **64** while `IBD_TIP_PIPE` showed
                // pipe_depth=128 span=64 — half the pipe idle; reassign cadence ~64/2.6s
                // ≈24 blk/s ceiling (observed tip ~17–34). Always use 128 on WAN tip owner
                // regardless of bulk; ahead partitions stay small (32) below.
                // Env `BLVM_IBD_GAP_PREEMPT_BATCH` overrides.
                let bulk = self.wan_bulk_catchup(next_needed);
                // W40: LOCAL_AHEAD soft-resume tip holes need a deep pipe too (bodies sparse
                // under body_tip). Live 2026-07-16: default 16 + chunk-start gate → 0 tip-owner
                // assigns, behind-tip main-queue storm, ~0.06 blk/s.
                // Synth bulk (GETDATA_DELAY_MS=0): bodies are already local — W40 tip-hole pipe
                // re-preempts the same spans 8–17×/s, feeder stays 0, wall ~6–8 blk/s (2026-07-23).
                // Keep W40 for real soft-resume and for synth tip-crawl with delay>0.
                let tip_missing = self.tip_gap_missing.load(Ordering::Relaxed);
                let local_tip_hole = !wan_gap
                    && tip_missing
                    && !(super::synthetic_wan::enabled()
                        && super::synthetic_wan::getdata_delay_ms() == 0);
                // Dense local W16 tip-fill (bootstrap / gap_preempt dens KEEP): body_tip=0
                // makes wan_tip_gap_crawl true for genesis stall/nudge, but without a
                // coordinator body/header tip and without tip_missing, tip-owner stays on
                // the non-tip_pipe batch=16 path (not WAN 128 / C1e stripe).
                let assign_wan_gap = wan_gap
                    && (tip_missing
                        || self.header_tip() > 0
                        || self.wan_body_tip.load(Ordering::Relaxed) > 0);
                // C1u: handoff_prime uses tip-pipe GetData depth while still local.
                let tip_pipe = assign_wan_gap || local_tip_hole || handoff_prime;
                // C1e: while tip missing, tip-owner takes a *stripe* (default 32), not 128.
                // Assigned tip..tip+127 with GetData depth 8 left frontier at tip+127 and
                // other peers opened tip+128 — multi-peer Swiss cheese. Multiple peers fill
                // contiguous stripes inside TIP_RUNWAY_CAP instead.
                let runway_cap = Self::tip_runway_cap();
                let runway_stripe = Self::tip_runway_stripe();
                // C1g/C1i: freeze past-tip stripes until tip is present AND contig runway
                // reaches min (default grow-start 8). C1h still cheese'd: tip lands 1 block
                // → ahead opens tip+32 → tip drains → TIP_HOLE_AHEAD (ahead_buf_p50=40).
                // C1i KEEP: freeze past-tip until contig≥8 (iter ~114 BPS). min_contig=0/1
                // reopened ahead and C1j-alone peerday collapsed ~4 BPS. C1j abort still
                // kills cheese GetData while tip missing. Env `BLVM_IBD_C1I_MIN_CONTIG`.
                //
                // C1q REVERT (2026-07-27): allowing ahead on tip-in-feeder / feeder≥min
                // reopened CHEESE on mute peerdays (cheese=4–11 vs C1p cheese=0). Public
                // WAN stays tip-owner-serial under C1i; squeeze via pipe depth / gd-fast.
                let contig_now = super::IBD_TIP_CONTIG_RUNWAY.load(Ordering::Relaxed);
                // Coordinator contig can lag unit tests / refresh; deep in-flight tip cover
                // already proves runway (w49 ahead after owner stripe).
                // Mousetrap F2 REVERT (2026-08-01): pipe_contig C1i flattened mid-gap wall
                // (365<390) and past-body tip_crawl 47<<B0 81 — restore claim credit.
                let contig_from_claims = tip_claims
                    .iter()
                    .map(|(_, s, e)| Self::claim_remaining_tip_depth(next_needed, *s, *e))
                    .max()
                    .unwrap_or(0);
                let contig_eff = contig_now.max(contig_from_claims);
                let min_contig_for_ahead = {
                    let raw = latch_env!(u64, {
                        std::env::var("BLVM_IBD_C1I_MIN_CONTIG")
                            .ok()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(8u64)
                    });
                    raw.clamp(0, runway_stripe)
                };
                // Freeze past-tip only on real WAN assign (not dense-local body_tip=0 unit).
                let c1g_freeze_past_tip = assign_wan_gap
                    && (tip_missing
                        || (min_contig_for_ahead > 0 && contig_eff < min_contig_for_ahead));
                let holes_now = self.tip_bridge_holes.load(Ordering::Relaxed);

                // --- Tip owner: sticky/best peer (gated on healthy claims, not walk-ins) ---
                // W40: never skip tip owner at chunk-map starts when covering=0 — that gate
                // left LOCAL_AHEAD uncovered at every chunk boundary.
                let fetchers_cap = self.max_gap_fetchers_per_height();
                let healthy_claims =
                    Self::healthy_tip_cover_count_from(&tip_claims, next_needed);
                // W65: shallow tip-cover remnants (deep==0, healthy>0) need a full deep
                // re-arm (≥64), not C1e stripe-32 (dens KEEP w65 expects end≥tip+63).
                let shallow_rearm = tip_missing && effective_healthy == 0 && healthy_claims > 0;
                let default_batch = if tip_pipe && assign_wan_gap && tip_missing && !shallow_rearm
                {
                    runway_stripe
                } else if tip_pipe {
                    128
                } else {
                    16
                };
                let mut preempt_batch: u64 = Self::gap_preempt_batch_raw()
                    .unwrap_or(default_batch)
                    .clamp(1, if tip_pipe { 256 } else { 128 });
                if tip_pipe && assign_wan_gap && tip_missing && !shallow_rearm {
                    // Explicit GAP_PREEMPT_BATCH must not re-open phantom 128 assign while tip empty.
                    preempt_batch = preempt_batch.min(runway_stripe);
                }
                // W47: tip-pipe shrink on holes is opt-in only (was default holes≥1 → 32 forever).
                if tip_pipe
                    && Self::tip_pipe_shrink_holes_opt()
                        .is_some_and(|thr| holes_now >= thr)
                {
                    preempt_batch = preempt_batch.min(32);
                }
                let c1g_tip_race = wan_gap
                    && tip_missing
                    && super::tip_stage::tip_awaiting_secs_for_cap()
                        >= Self::c1g_tip_race_await_secs();
                let c1t_tip_race = wan_gap && tip_missing && self.c1t_tip_height_race();
                let tip_distress = Self::tip_is_distressed()
                    || Self::tip_hole_race_holes_opt()
                        .is_some_and(|thr| holes_now >= thr)
                    || c1g_tip_race
                    || c1t_tip_race;
                // W120: revert W117–W119 shallow_tip_cover / cover_for_gate. Live W117–W119
                // all rate-failed @306–311k (W116 DNA reached 344k). Accept CAP-soft wait
                // on end-of-pipe shallow cover (W116 @344580 ~9s) rather than early regress.
                // Keep overlaps_ok failover-first below (harmless on W116 path; required if
                // failover ever races a shallow remnant while deep cover exists).
                // W88: one failover per tip-*stall episode* (not per tip height).
                // W87 cleared on +1 advance → CAP at H then failover H,H+1,… every ~2s
                // (live 2026-07-18: 10× cascade, tip60 45→9).
                let failover_already = self.tip_failover_episode_active(next_needed);
                // W112: empty triple race may assign a second (H,H) despite W88 episode
                // latch — still hard-capped by fetchers_cap / raw_covering.
                let empty_triple = self.empty_tip_triple_race();
                // W122/W149: covering=1 mute + awaiting≥3s reopens one (H,H) under W88
                // latch without empty_triple covering=3 (W121 soft-resume regress).
                let mute_reopen = self.mute_single_cover_reopen(raw_covering);
                // W86: mute_reopen must not stack another (H,H) while a *deep* owner still
                // covers tip (failover peer dropped in-flight → raw_covering=1). Only reopen
                // under episode latch when deep cover is gone (true mute) or empty_triple.
                // TPP L1 REVERT: understudy pierce undone with peer_may C1g.
                let mute_reopen_open =
                    mute_reopen && effective_healthy == 0 && raw_covering < fetchers_cap;
                // empty_triple may open a second (H,H) under episode latch (w112/w153) even
                // with a deep owner — W86 stacking is blocked on the main-queue path instead.
                let failover_slot_open = !failover_already
                    || (empty_triple && raw_covering < fetchers_cap)
                    || mute_reopen_open;
                // W30: deep==0 must re-arm even with (H,H) micros present.
                // W41c/W47: failover only under tip distress (not standing hole-keyed race).
                // W86: also gate on raw_covering — healthy/deep counts ignore (H,H), so
                // distress + `overlaps_ok=failover` previously stacked unbounded tip micros
                // (live W85 fail: 2241× W28c tip failover on only 17 tip heights → tip60~30).
                // W87/W88: episode latch (advance≥32 or ~30s) — not one-per-height.
                // P1d/H6: GAP_STREAM DEDUP hold blocks tip-owner re-preempt (WAN + synth).
                let want_tip_owner = self.peer_may_take_tip_owner(peer_id, &guard, effective_healthy)
                    && (effective_healthy == 0
                        || (tip_distress
                            && healthy_claims < fetchers_cap
                            && raw_covering < fetchers_cap
                            && failover_slot_open))
                    && !self.tip_owner_blocked_by_dedup(next_needed)
                    // Dense local (!tip_pipe): mid-chunk W16 only. At chunk-map starts the
                    // main queue owns the full span (max_ahead=0 / A4 / sequential assign).
                    && (tip_pipe || !at_chunk_start);
                let _ = at_chunk_start;
                if want_tip_owner {
                    // W30/W37/W41c: (H,H) failover only while tip is in distress.
                    // W112: empty triple may open a second failover micro under fetchers_cap=3.
                    let failover = effective_healthy >= 1
                        && tip_distress
                        && failover_slot_open
                        && raw_covering < fetchers_cap;
                    let mut preempt_end = if failover {
                        // Failover races only the tip height — primary keeps the deep pipeline.
                        next_needed
                    } else {
                        next_needed.saturating_add(preempt_batch.saturating_sub(1))
                    };
                    // W32a/W40: tip-pipe (WAN gap or LOCAL_AHEAD hole) skips chunk-map clip.
                    if !failover && !tip_pipe {
                        // Dense local replay: clip to chunk-map boundaries.
                        if ce > next_needed {
                            preempt_end = preempt_end.min(ce);
                        } else if let Some((_, nce)) =
                            self.chunks.iter().find(|(s, _)| *s == ce + 1).copied()
                        {
                            preempt_end = preempt_end.min(nce);
                        }
                    }
                    // C1u: while still local, tip-fill must not claim past on-disk body tip
                    // even when tip_pipe=handoff_prime (batch 128). Past-tip warm is only
                    // via try_assign_handoff_prime — else next..next+127 spans the hole and
                    // abandons local cover (true-wan freeze class @437205).
                    if !failover && !wan_gap {
                        let body_tip = self.wan_body_tip.load(Ordering::Relaxed);
                        if body_tip > 0 && next_needed <= body_tip {
                            preempt_end = preempt_end.min(body_tip);
                        }
                    }
                    // W35‴-h: never assign past stored headers (live: 912× "hash not found
                    // for height 957742" → mass blacklist → tip deadlock for hours).
                    let tip_headers_ok = match self.clip_end_to_headers(next_needed, preempt_end) {
                        Some((_, clipped)) => {
                            preempt_end = clipped;
                            true
                        }
                        None => false,
                    };
                    if !tip_headers_ok {
                        // next_needed past header tip — skip tip assign this poll.
                    } else {
                    // W28d: when healthy==0, allow overlap with walk-in ahead ranges — those
                    // peers will abort via should_abort_tip_walk_in. Only block on another
                    // tip-cover claim overlapping the new range.
                    let claim_overlap = {
                        let min_depth = Self::tip_deep_cover_min_depth();
                        let claims = self.tip_cover_claims.lock().unwrap();
                        claims.iter().any(|(_, s, e)| {
                            // W30/W37: (H,H) failover micro-claims must not block deep re-arm.
                            if *s == *e {
                                return false;
                            }
                            // W65: shallow walk-promote remnants must not block a real
                            // deep tip pipe (live tip=218 claim 218-224 vs owner 218-345).
                            if !failover
                                && Self::claim_remaining_tip_depth(next_needed, *s, *e)
                                    < min_depth
                            {
                                return false;
                            }
                            *s <= preempt_end && next_needed <= *e
                        })
                    };
                    // W117: check failover first — `effective_healthy==0` + shallow claim
                    // made claim_overlap true under failover (shallow not skipped when
                    // failover=true) and blocked the (H,H) assign (live W116 @344580).
                    let overlaps_ok = if failover {
                        // W86: allow overlap with deep/shallow tip owner, but never stack a
                        // second (H,H) while raw covering already meets fetchers_cap.
                        raw_covering < fetchers_cap
                    } else if effective_healthy == 0 {
                        // Refuse overlap with another *deep* tip pipe (synth same-span
                        // storms). W28d/W65: shallow walk-in cover must NOT block deep
                        // re-arm — raw_covering==0 froze owner behind promote remnants
                        // (live tip=218 claim 218-224 → owner only got (H,H)).
                        !claim_overlap
                            && Self::find_inflight_deep_covering(&guard, next_needed).is_none()
                    } else {
                        !Self::range_overlaps_inflight(&guard, next_needed, preempt_end)
                    };
                    // P1c: never re-preempt tip onto a peer that already holds tip in-flight
                    // (even failover (H,H) or top_peer max_in_flight=2). Ahead dual-pipe OK.
                    let peer_already_tip =
                        Self::peer_holds_tip_inflight(&guard, peer_id, next_needed);
                    if preempt_end >= next_needed && overlaps_ok && !peer_already_tip {
                        let walk_in = raw_covering > effective_healthy;
                        Self::insert_in_flight(&mut guard, peer_id, next_needed, preempt_end);
                        self.note_tip_cover_claim(peer_id, next_needed, preempt_end);
                        if effective_healthy == 0 {
                            drop(guard);
                            self.note_tip_owner_assigned(peer_id);
                            if wan_gap || local_tip_hole {
                                let score = self
                                    .peer_scores
                                    .lock()
                                    .unwrap()
                                    .get(peer_id)
                                    .copied()
                                    .unwrap_or(0.0);
                                tracing::debug!(
                                    "[IBD_TIP_PEER] owner={} score={:.3} span={}-{} ({})",
                                    peer_id,
                                    score,
                                    next_needed,
                                    preempt_end,
                                    if wan_gap {
                                        "W33 top-peer pipe"
                                    } else {
                                        "W40 local tip-hole pipe"
                                    }
                                );
                                if wan_gap {
                                    self.log_wan_tip_owner_ready(peer_id, next_needed, preempt_end);
                                }
                            }
                            if walk_in {
                                tracing::debug!(
                                    "gap preempt: assigning {}-{} to {} (W28d tip owner after walk-in preempt, raw_covering={})",
                                    next_needed,
                                    preempt_end,
                                    peer_id,
                                    raw_covering
                                );
                            } else {
                                tracing::debug!(
                                    "gap preempt: assigning {}-{} to {} (W28c tip owner, covering={}/{})",
                                    next_needed,
                                    preempt_end,
                                    peer_id,
                                    1,
                                    self.max_gap_fetchers_per_height()
                                );
                            }
                            return Some((next_needed, preempt_end));
                        }
                        self.latch_tip_failover_episode(next_needed);
                        tracing::debug!(
                            "gap preempt: assigning {}-{} to {} (W28c tip failover{}, covering={}/{})",
                            next_needed,
                            preempt_end,
                            peer_id,
                            if c1t_tip_race { " C1t" } else { "" },
                            raw_covering.saturating_add(1),
                            self.max_gap_fetchers_per_height()
                        );
                        return Some((next_needed, preempt_end));
                    }
                    } // tip_headers_ok
                }

                // --- Ahead partition: any free peer, non-overlapping after tip frontier ---
                // Require a *healthy* tip cover (not walk-in) before handing out more ahead.
                //
                // C1g supersedes A6g "ahead while gap_missing": opening tip+32 while tip
                // empty produced TIP_HOLE_AHEAD binder (C1f). Multi-peer ahead runs when
                // tip is present in reorder (`!tip_missing`); between tip bodies the tip
                // owner stripe + optional `(H,H)` race fill the hole.
                //
                // W47: gate on deep tip cover + tip distress, not bridge holes alone.
                // Soft-retry freezes multi-peer ahead; late-body alone does not when tip
                // already has healthy cover (2026-07-31 W102b/late-body narrow).
                let gap_missing = self.tip_gap_missing.load(Ordering::Relaxed);
                let feeder_len = super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
                if c1g_freeze_past_tip {
                    static C1G_FREEZE_LOG: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let prev = C1G_FREEZE_LOG.load(Ordering::Relaxed);
                    if now.saturating_sub(prev) >= 5
                        && C1G_FREEZE_LOG
                            .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                    {
                        tracing::warn!(
                            "[IBD_C1G_FREEZE] tip={} covering={} ready={} — past-tip stripes frozen while tip missing; race tip H only",
                            next_needed,
                            raw_covering,
                            self.ibd_ready_peer_count()
                        );
                    }
                }
                // Mousetrap F3 REVERT (2026-08-01): secondary under tip_missing + pipe
                // solid → past-body cheese=45 / tip_crawl regress. C1g freeze stays absolute.
                if assign_wan_gap
                    && !c1g_freeze_past_tip
                    && effective_healthy > 0
                    && self.wan_allow_multi_peer_ahead(effective_healthy, feeder_len)
                    && super::tip_stage::tip_soft_retries() == 0
                    && !super::tip_stage::tip_ahead_frozen_for_soft_retry()
                    && self.preferred_tip_owner().as_deref() == Some(peer_id)
                    && self.tip_sticky_usable(peer_id)
                {
                    // Sticky dual-pipe after tip lands (C1g: not while tip_missing).
                    let sticky_window: u64 = latch_env!(u64, {
                        std::env::var("BLVM_IBD_STICKY_PIPE_WINDOW")
                            .ok()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(512)
                            .clamp(256, 2048)
                    });
                    // Mousetrap F1 REVERT: sticky frontier = claim pipeline (pre-F KEEP).
                    let frontier =
                        Self::tip_pipeline_frontier(&guard, next_needed, sticky_window);
                    let assign_f = frontier;
                    let pipe_f = super::tip_stage::pipe_frontier(next_needed);
                    let body_f = if contig_now > 0 {
                        next_needed.saturating_add(contig_now.saturating_sub(1))
                    } else {
                        next_needed.saturating_sub(1)
                    };
                    let sticky_batch: u64 = Self::gap_preempt_batch_raw()
                        .unwrap_or(128)
                        .clamp(64, 256);
                    let band_end = next_needed.saturating_add(sticky_window);
                    let part_start = frontier.saturating_add(1);
                    let part_end_raw = part_start
                        .saturating_add(sticky_batch.saturating_sub(1))
                        .min(band_end);
                    // Manual REVERT 2026-08-03: part_start>tip gate cratered tip90 (c1ggate
                    // 77.8 / c1gclaim 100 vs forcedeb 108.9). Tip reassigns were accidental
                    // re-arm when claims outlived in_flight; fix is stale-claim prune above,
                    // not blocking C1g when frontier falls to tip-1.
                    if part_start <= band_end {
                        if let Some((_, part_end)) =
                            self.clip_end_to_headers(part_start, part_end_raw)
                        {
                            if part_start <= max_start
                                && part_end >= part_start
                                && !Self::range_overlaps_inflight(&guard, part_start, part_end)
                                && !self.chunk_range_in_flight(&guard, part_start, part_end)
                            {
                                Self::insert_in_flight(&mut guard, peer_id, part_start, part_end);
                                Self::log_pipe_f(
                                    next_needed,
                                    assign_f,
                                    pipe_f,
                                    body_f,
                                    peer_id,
                                    "sticky",
                                );
                                tracing::debug!(
                                    "gap preempt: assigning {}-{} to {} (C1g sticky after tip, tip={}, frontier={}, holes={})",
                                    part_start,
                                    part_end,
                                    peer_id,
                                    next_needed,
                                    frontier,
                                    self.tip_bridge_holes.load(Ordering::Relaxed),
                                );
                                return Some((part_start, part_end));
                            }
                        }
                    }
                }
                // Dense-local (wan crawl with body_tip=0 / no headers): use LOCAL ahead
                // gating — WAN distress/contig freeze must not starve dens KEEP partitions.
                // During HANDOFF_PRIME, freeze local ahead too: multi-peer W28c partitions
                // through body tip while sticky primes past tip leave Swiss-cheese holes
                // (true-wan freeze @437205 / wait 437206, reorder≈108, tip60 never armed).
                let allow_ahead = if assign_wan_gap {
                    !c1g_freeze_past_tip
                        && self.wan_allow_multi_peer_ahead(effective_healthy, feeder_len)
                } else {
                    !gap_missing && !handoff_prime
                };
                if effective_healthy > 0 && allow_ahead {
                    // Multi-peer ahead after tip lands (C1g). Contiguous assign frontier
                    // still used so stripes do not jump holes.
                    let runway_end = next_needed.saturating_add(runway_cap.saturating_sub(1));
                    let part_window: u64 = if assign_wan_gap {
                        Self::tip_partition_window_raw()
                            .unwrap_or(256)
                            .clamp(64, 512)
                    } else {
                        Self::tip_partition_window_raw()
                            .unwrap_or(512)
                            .clamp(64, 2048)
                    };
                    // Mousetrap F1 REVERT (2026-08-01): stripe frontier = claim contig
                    // (pre-F KEEP). Still log assign_F vs pipe_F for forensics.
                    let assign_f_wan = if assign_wan_gap {
                        Self::tip_contiguous_assign_frontier(&guard, next_needed, runway_end)
                    } else {
                        0
                    };
                    let pipe_f_wan = if assign_wan_gap {
                        super::tip_stage::pipe_frontier(next_needed)
                    } else {
                        0
                    };
                    let body_f_wan = if contig_now > 0 {
                        next_needed.saturating_add(contig_now.saturating_sub(1))
                    } else {
                        next_needed.saturating_sub(1)
                    };
                    let frontier = if assign_wan_gap {
                        // Tip present: walk contiguous in-flight from tip inside runway,
                        // then legacy window past runway.
                        let contig =
                            Self::tip_contiguous_assign_frontier(&guard, next_needed, runway_end);
                        if contig >= runway_end {
                            Self::tip_pipeline_frontier(&guard, next_needed, part_window)
                        } else {
                            contig
                        }
                    } else {
                        Self::tip_pipeline_frontier(&guard, next_needed, part_window)
                    };
                    let part_start = frontier.saturating_add(1);
                    if part_start <= max_start && part_start >= next_needed {
                        let ahead_peers = guard
                            .iter()
                            .filter(|(p, ranges)| {
                                p.as_str() != peer_id
                                    && ranges.iter().any(|(s, _)| *s > next_needed)
                            })
                            .count();
                        let holes_now = self.tip_bridge_holes.load(Ordering::Relaxed);
                        let ahead_cap = if assign_wan_gap {
                            let floor_stall = self.preferred_is_floor_sticky()
                                && self
                                    .sticky_recent_bps(next_needed, a6m_recent_window_secs())
                                    .map(|(bps, _, elapsed)| {
                                        elapsed >= (a6m_recent_window_secs() as f64) * 0.8
                                            && bps < a6m_floor_open_slot_min_bps()
                                    })
                                    .unwrap_or(false);
                            // W123: sticky hole-band freeze (not raw holes — W47).
                            self.tip_ahead_hole_band_update(feeder_len);
                            let tip_distress = Self::tip_is_distressed()
                                || self.tip_ahead_hole_freeze.load(Ordering::Relaxed);
                            // No ahead_cap bypass on under-target (W3c ahead flood FAIL; W3 REVERT).
                            if floor_stall || tip_distress {
                                0
                            } else {
                                Self::tip_ahead_peer_cap()
                            }
                        } else {
                            usize::MAX
                        };
                        if assign_wan_gap && ahead_peers >= ahead_cap {
                            // Tip owner / retry only — ahead cap reached.
                        } else if assign_wan_gap {
                            // C1g: tip already in reorder here; stripe from contiguous frontier.
                            let ahead_batch = runway_stripe.min(32u64.min(preempt_batch));
                            let part_end =
                                part_start.saturating_add(ahead_batch.saturating_sub(1));
                            if part_end >= part_start
                                && !Self::range_overlaps_inflight(&guard, part_start, part_end)
                                && !self.chunk_range_in_flight(&guard, part_start, part_end)
                            {
                                Self::insert_in_flight(&mut guard, peer_id, part_start, part_end);
                                Self::log_pipe_f(
                                    next_needed,
                                    assign_f_wan,
                                    pipe_f_wan,
                                    body_f_wan,
                                    peer_id,
                                    "stripe",
                                );
                                tracing::debug!(
                                    "gap preempt: assigning {}-{} to {} (C1g runway stripe, tip={}, frontier={}, ahead_peers={}, holes={}, runway={})",
                                    part_start,
                                    part_end,
                                    peer_id,
                                    next_needed,
                                    frontier,
                                    ahead_peers + 1,
                                    holes_now,
                                    runway_cap
                                );
                                return Some((part_start, part_end));
                            }
                        } else {
                            // Non-WAN: ahead partitions stay at 32 with chunk-map clip.
                            // C1u: never multi-peer past body tip (live cheese: ahead
                            // GetData'd 304672 while tip=304418 → TIP_HOLE_AHEAD cliff).
                            // When frontier reaches body tip, tip-owner primes WAN GetData.
                            let body_tip = self.wan_body_tip.load(Ordering::Relaxed);
                            if body_tip > 0 && part_start > body_tip {
                                if !super::synthetic_wan::bulk_local_disk_stream() {
                                    if let Some(range) = self.try_assign_handoff_prime(
                                        peer_id,
                                        next_needed,
                                        &mut guard,
                                        "ahead_frontier",
                                    ) {
                                        drop(guard);
                                        return Some(range);
                                    }
                                }
                            } else {
                                // C1u: far-local fill to body tip — allow up to 256 so two
                                // max_in_flight=1 peers can cover HANDOFF_PRIME without
                                // completes (dens KEEP c1u_local_ahead…). Default stays 32.
                                let ahead_batch = if body_tip > 0 && part_start <= body_tip {
                                    body_tip
                                        .saturating_sub(part_start)
                                        .saturating_add(1)
                                        .min(256)
                                        .max(preempt_batch.min(32))
                                } else {
                                    preempt_batch.min(32)
                                };
                                let mut part_end =
                                    part_start.saturating_add(ahead_batch.saturating_sub(1));
                                if body_tip > 0 {
                                    part_end = part_end.min(body_tip);
                                }
                                if let Some((_, pe)) = self
                                    .chunks
                                    .iter()
                                    .find(|(s, e)| *s <= part_start && part_start <= *e)
                                    .copied()
                                {
                                    part_end = part_end.min(pe);
                                }
                                if part_end >= part_start
                                    && !Self::range_overlaps_inflight(&guard, part_start, part_end)
                                    && !self.chunk_range_in_flight(&guard, part_start, part_end)
                                {
                                    Self::insert_in_flight(
                                        &mut guard,
                                        peer_id,
                                        part_start,
                                        part_end,
                                    );
                                    tracing::debug!(
                                        "gap preempt: assigning {}-{} to {} (W28c ahead partition, tip={}, frontier={})",
                                        part_start,
                                        part_end,
                                        peer_id,
                                        next_needed,
                                        frontier
                                    );
                                    return Some((part_start, part_end));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Try retry queue first (critical chunk, then earliest).
        //
        // IMPORTANT: retry-queue chunks are NOT filtered by max_start. These are stall-recovery
        // chunks — the coordinator explicitly decided they're needed to unblock progress. Applying
        // the max_ahead window to retry chunks causes a deadlock when the missing chunk starts just
        // past max_start: validation stalls (can't advance), max_start can't grow (validation stuck),
        // and the chunk can never be taken (max_start check fails). The retry_queue is always small
        // (0–1 entries in practice), so skipping the window check here poses no memory risk.
        {
            let mut retry = self.retry_queue.lock().unwrap();
            // W4: prefer multi-block critical ranges that cover next_needed BEFORE single-height
            // micros. Live WAN: gap-preempt + micro-first drained the pipeline into 1-block
            // GetData storms (~0.3 BPS). Bulk GetData recovers network efficiency; keep micros
            // as fallback when no bulk critical is queued.
            let critical = retry.iter().enumerate().find(|(_, (s, e, ex))| {
                *s <= next_needed
                    && next_needed <= *e
                    && *s != *e
                    && ex.as_ref() != Some(&peer_id.to_string())
                    && allow_chunk(*s)
            });
            if let Some((i, _)) = critical {
                let (start, end, ex) = retry.remove(i).unwrap();
                // H6: synth tip already GAP_STREAM'd — do not re-download via retry either.
                if start <= next_needed
                    && next_needed <= end
                    && self.tip_owner_blocked_by_dedup(next_needed)
                {
                    retry.push_back((start, end, ex));
                    return None;
                }
                if !self.peer_may_take_wan_gap_retry(peer_id, start, end) {
                    retry.push_back((start, end, ex));
                    return None;
                }
                if self.chunk_range_in_flight(&guard, start, end) {
                    retry.push_back((start, end, ex));
                    return None;
                }
                Self::insert_in_flight(&mut guard, peer_id, start, end);
                // Critical retry covering tip = tip claim (WAN gap: owner only — W31d).
                self.maybe_note_tip_cover_claim_retry(peer_id, start, end);
                return Some((start, end));
            }
            // Gap micro-chunks (H,H) — coalesce consecutive singles into a merged range.
            //
            // Micro-batch coalescing: instead of returning a single-block (H,H) range and
            // making the worker issue a separate GetData round-trip per block, we scan the retry
            // queue for consecutive single-block micro-chunks and return a merged (H, H+k) range.
            // This converts N serial RTTs into a single pipelined GetData for up to
            // MICRO_BATCH_COALESCE blocks — the download function naturally handles the range.
            //
            // Coalescing is safe because:
            //  - All heights in [H, H+k] are already present in the retry queue (assigner owns them).
            //  - The merged range replaces the N individual entries atomically (single lock).
            //  - On failure the whole range is re-queued, preserving stall-recovery semantics.
            //  - The exclude filter from the first entry applies to the merged range only;
            //    remaining entries in the queue may have different excludes (handled by rotation).
            const MICRO_BATCH_COALESCE: u64 = 16; // merge up to 16 consecutive micro-chunks
            let peer_str = peer_id.to_string();
            let gap_micro_start = retry
                .iter()
                .enumerate()
                .filter(|(_, (s, e, ex))| {
                    *s == *e
                        && *s >= next_needed
                        && ex.as_ref() != Some(&peer_str)
                        && allow_chunk(*s)
                })
                .min_by_key(|(_, (s, _, _))| *s)
                .map(|(i, _)| i);
            if let Some(i) = gap_micro_start {
                // W32e: on WAN gap, non-owner peers must not coalesce retry micros on the tip.
                let wan_gap_retry = self.wan_tip_gap_crawl(next_needed);
                let non_owner_micro = wan_gap_retry
                    && self
                        .preferred_tip_owner()
                        .is_some_and(|o| o.as_str() != peer_id);
                if non_owner_micro {
                    // fall through — owner or main queue only
                } else if self.peer_ok_for_gap_race(peer_id) {
                    let (start, _end, _ex) = retry.remove(i).unwrap();
                    if !self.peer_may_take_wan_gap_retry(peer_id, start, start) {
                        retry.push_back((start, start, _ex));
                        return None;
                    }
                    if self.chunk_range_in_flight(&guard, start, start) {
                        retry.push_back((start, start, _ex));
                        return None;
                    }
                    // Coalesce consecutive (H+1,H+1), (H+2,H+2), ... micro-chunks for this peer.
                    // Scan for consecutive heights; remove each found entry by value (one entry
                    // per height) to avoid index-shifting bugs when removing from a VecDeque.
                    let mut merged_end = start;
                    while merged_end - start < MICRO_BATCH_COALESCE - 1 {
                        let next_h = merged_end + 1;
                        let found_idx = retry.iter().position(|(s, e, ex)| {
                            *s == next_h
                                && *e == next_h
                                && ex.as_ref() != Some(&peer_str)
                                && allow_chunk(next_h)
                        });
                        if let Some(idx) = found_idx {
                            retry.remove(idx);
                            merged_end = next_h;
                        } else {
                            break;
                        }
                    }
                    Self::insert_in_flight(&mut guard, &peer_str, start, merged_end);
                    self.maybe_note_tip_cover_claim_retry(&peer_str, start, merged_end);
                    if merged_end > start {
                        tracing::debug!(
                            "micro-batch coalesced {}-{} ({} blocks) for peer {}",
                            start,
                            merged_end,
                            merged_end - start + 1,
                            peer_id
                        );
                    }
                    return Some((start, merged_end));
                }
            }
            let ok_gap = self.peer_ok_for_gap_race(peer_id);
            let candidate = retry
                .iter()
                .enumerate()
                .filter(|(_, (_, _, ex))| ex.as_ref() != Some(&peer_id.to_string()))
                .filter(|(_, (s, _, _))| allow_chunk(*s))
                // P5: bottom-quartile peers skip single-height micros in the fallback path too.
                .filter(|(_, (s, e, _))| ok_gap || *s != *e)
                .min_by_key(|(_, (s, _, _))| *s);
            if let Some((i, _)) = candidate {
                let (start, end, ex) = retry.remove(i).unwrap();
                if !self.peer_may_take_wan_gap_retry(peer_id, start, end) {
                    retry.push_back((start, end, ex));
                    return None;
                }
                if self.chunk_range_in_flight(&guard, start, end) {
                    retry.push_back((start, end, ex));
                    return None;
                }
                Self::insert_in_flight(&mut guard, peer_id, start, end);
                return Some((start, end));
            }
        }

        // Main queue — try the next sequential chunk.
        //
        // Peer binding: enforced for LAN/single-peer modes so a fast LAN peer isn't displaced by
        // slow WAN peers stealing its pre-assigned chunks. For WAN multi-peer (work_stealing=true),
        // binding is skipped — any free peer takes the next available chunk, giving us work-stealing
        // semantics that maximize throughput when peers have heterogeneous speeds.
        let idx = self.next_index.load(Ordering::Relaxed);
        if idx >= self.chunks.len() {
            return None;
        }
        // Peer binding check: skip in work_stealing mode (WAN multi-peer).
        if !self.work_stealing
            && idx < self.preferred_peers.len()
            && self.preferred_peers[idx] != peer_id
        {
            return None;
        }
        let (start, end) = self.chunks[idx];
        if self.chunk_range_in_flight(&guard, start, end) {
            return None;
        }
        // Allow the chunk that contains the next validation height even when max_ahead is 0
        // (near-tip catch-up). Otherwise workers poll forever and the retry queue never drains.
        if start > max_start && start > next_needed {
            return None;
        }
        // W16: while tip is uncovered **and tip is inside the chunk map**, do not hand out
        // far main-queue work — force peers through tip fill / retry first (live: 685057
        // assigned with need=684805). Skip when next_needed is outside the map (tests /
        // pre-start) so sequential assignment still works.
        let tip_in_map = self
            .chunks
            .iter()
            .any(|(s, e)| *s <= next_needed && next_needed <= *e);
        if tip_in_map {
            let tip_band: u64 = latch_env!(u64, {
                std::env::var("BLVM_IBD_TIP_ASSIGN_BAND")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(64)
                    .clamp(16, 256)
            });
            let covering_next = Self::covering_next_count(&guard, next_needed);
            let healthy = self.healthy_tip_cover_count(next_needed);
            let gap_missing = self.tip_gap_missing.load(Ordering::Relaxed);
            // W40/W80: never assign a chunk that ends entirely behind tip (even when
            // gap_missing is briefly false). Live loop-1: 309798-309925 recirculated
            // while tip≈321k via main-queue/retry while tip hole looked covered.
            if end < next_needed {
                return None;
            }
            // C1g/C1h: while tip body missing, block ANY main-queue start past tip
            // (was tip_band=64 → tip+32.. still assigned → cheese / TIP_HOLE_AHEAD).
            // Tip fill + (H,H) race own the hole; ahead stripes reopen when tip lands.
            if gap_missing && start > next_needed {
                return None;
            }
            // W25a/W28d: when tip covered but healthy==0, still keep far main-queue off.
            if !gap_missing
                && healthy == 0
                && start > next_needed.saturating_add(tip_band)
            {
                return None;
            }
            // P1c: peer already holding tip in-flight must not take tip-covering MQ/(H,H).
            if start <= next_needed
                && next_needed <= end
                && Self::peer_holds_tip_inflight(&guard, peer_id, next_needed)
            {
                return None;
            }
            // W28c: a main-queue chunk that covers the tip *is* tip ownership — do not hand it
            // to a peer that failed sticky/best tip selection (live: slow peer took 1000-1200
            // via main queue while "fast" still had capacity).
            if healthy == 0 && start <= next_needed && next_needed <= end
                && !self.peer_may_take_tip_owner(peer_id, &guard, healthy)
            {
                return None;
            }
            // H6: tip-owner DEDUP hold must also block main-queue tip-covering reassign
            // (tip fill skips W28c, then main queue re-issued the same span — live short).
            if start <= next_needed
                && next_needed <= end
                && self.tip_owner_blocked_by_dedup(next_needed)
            {
                return None;
            }
            let _ = covering_next; // retained for possible future diag in this path
        }
        if !allow_chunk(start) {
            return None;
        }
        // C1g/C1h: while tip missing on WAN, never hand the map-span chunk as ahead cheese.
        // - No deep owner yet → clip to tip runway stripe (owner path).
        // - Deep owner already → tip-height `(H,H)` only (race), not tip..header_tip.
        let mut assign_start = start;
        let mut assign_end = end;
        if self.tip_gap_missing.load(Ordering::Relaxed)
            && self.wan_tip_gap_crawl(next_needed)
            && start <= next_needed
            && next_needed <= end
        {
            let healthy_mq = self.healthy_tip_cover_count(next_needed);
            if healthy_mq >= 1 {
                // W86 dens KEEP: tip-fill owns distress failover micros + W88 episode latch.
                // MQ `(H,H)` bypassed fetchers_cap/episode and re-stacked tip micros after
                // the failover peer dropped in-flight (claim remained, tip-fill blocked).
                if self.tip_failover_episode_active(next_needed)
                    || Self::covering_next_count(&guard, next_needed)
                        >= self.max_gap_fetchers_per_height()
                {
                    return None;
                }
                assign_start = next_needed;
                assign_end = next_needed;
            } else {
                let stripe_end = next_needed
                    .saturating_add(Self::tip_runway_stripe().saturating_sub(1));
                assign_end = assign_end.min(stripe_end);
            }
        }
        if self.chunk_range_in_flight(&guard, assign_start, assign_end) {
            return None;
        }
        self.next_index.store(idx + 1, Ordering::Relaxed);
        Self::insert_in_flight(&mut guard, peer_id, assign_start, assign_end);
        // W28d: main-queue tip-covering work is an explicit tip claim (not an ahead walk-in).
        if assign_start <= next_needed && next_needed <= assign_end {
            self.note_tip_cover_claim(peer_id, assign_start, assign_end);
            if assign_end > assign_start {
                self.note_tip_owner_assigned(peer_id);
                self.log_wan_tip_owner_ready(peer_id, assign_start, assign_end);
            }
        }
        Some((assign_start, assign_end))
    }

    /// Called when a worker completes (or fails) a chunk. Clears in-flight so peer can get next chunk.
    pub(crate) fn on_chunk_complete(&self, peer_id: &str) {
        // Pop one in-flight range (A4 may hold two). Callers complete one chunk at a time.
        let mut g = self.in_flight_per_peer.lock().unwrap();
        if let Some(v) = g.get_mut(peer_id) {
            if let Some((s, e)) = v.pop() {
                drop(g);
                self.clear_tip_cover_claim(peer_id, s, e);
                return;
            }
            if v.is_empty() {
                g.remove(peer_id);
            }
        }
    }

    /// TRUE WAN tip_crawl: after download complete, keep tip-cover claim only while tip
    /// still lies in the span **and** tip body is already present (`!tip_gap_missing`).
    /// Blind keep (tc2ck) blocked re-fetch while tip still missing → tip90≈44.
    /// Present-only keep (tc9ck2) tip90≈102 but FORCE/gd_max worse vs tc4gpu — opt-in.
    fn wan_tip_claim_keep_enabled() -> bool {
        match std::env::var("BLVM_IBD_WAN_TIP_CLAIM_KEEP")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("1") | Some("true") | Some("on") | Some("yes") => true,
            _ => false,
        }
    }

    pub(crate) fn on_chunk_complete_range(&self, peer_id: &str, start: u64, end: u64) {
        let mut g = self.in_flight_per_peer.lock().unwrap();
        if let Some(v) = g.get_mut(peer_id) {
            if let Some(i) = v.iter().position(|(s, e)| *s == start && *e == end) {
                v.remove(i);
            }
            if v.is_empty() {
                g.remove(peer_id);
            }
        }
        drop(g);
        let next = self.next_needed_height();
        // Drop claims tip has walked past (kept claims from prior stripes).
        {
            let mut claims = self.tip_cover_claims.lock().unwrap();
            claims.retain(|(_, _, e)| *e >= next);
        }
        let tip_present = !self.tip_gap_missing.load(Ordering::Relaxed);
        let wan_keep = Self::wan_tip_claim_keep_enabled()
            && self.wan_tip_gap_crawl(next)
            && !super::synthetic_wan::bulk_local_disk_stream()
            && tip_present
            && start <= next
            && next <= end;
        if wan_keep {
            tracing::debug!(
                "[IBD_WAN_TIP_CLAIM_KEEP] peer={} {}-{} next_needed={} — tip present, cover until advance",
                peer_id,
                start,
                end,
                next
            );
        } else {
            self.clear_tip_cover_claim(peer_id, start, end);
        }
        // W91: CHUNK_OBSOLETE returns Ok (not Err), so the download worker never called
        // note_tip_owner_failed. Sticky preferred stayed pinned to a peer that just
        // released a behind-tip pipe → covering=0 / no TIP_PEER re-arm for tens of
        // seconds (live W90b: obsolete 315327-315454 then validation freeze @315613).
        //
        // Synth bulk: do not TIP_OWNER_OPEN on obsolete — clear sticky only.
        // Keep-claim + OPEN both tip-crawled ignition again (2026-07-23 keepclaim2:
        // first block ~11s, wall ~5). Burst path is Case C inject-without-rewind +
        // tip-reserve 1ms sleep (not yield_now).
        let synth_bulk = super::synthetic_wan::bulk_local_disk_stream();
        if end < next {
            let mut pref = self.preferred_tip_owner.lock().unwrap();
            if pref.as_deref() == Some(peer_id) {
                *pref = None;
                drop(pref);
                if !synth_bulk {
                    self.tip_owner_open.store(true, Ordering::Relaxed);
                    tracing::warn!(
                        "[IBD_TIP_OWNER_OPEN] after obsolete/behind-tip complete {}-{} peer={} next_needed={}",
                        start,
                        end,
                        peer_id,
                        next
                    );
                }
            }
        }
    }

    /// Synth recovery: drop tip-cover claims that outlived delivery (download complete kept
    /// the claim while tip never entered reorder/bridge — Case C waited tip_inflight_grace
    /// with covering≥1 → LOCAL_GAP ~1/10s → ~6 BPS).
    pub(crate) fn clear_tip_cover_claims_for_synth_recovery(&self, next_needed: u64) {
        if !super::synthetic_wan::bulk_local_disk_stream() {
            return;
        }
        let mut g = self.tip_cover_claims.lock().unwrap();
        let before = g.len();
        g.retain(|(_, s, e)| !(*s <= next_needed && next_needed <= *e));
        if g.len() != before {
            tracing::warn!(
                "[IBD_SYNTH_TIP_CLAIM_CLEAR] dropped tip-cover claim(s) at next_needed={} \
                 (tip absent from reorder/bridge/feeder)",
                next_needed
            );
        }
    }

    /// Tip-owner must not re-download a tip already GAP_STREAM'd (synth **and** WAN).
    ///
    /// Synth short-band (2026-07-26): local-disk finishes tip chunks in tens of ms, clears
    /// in_flight, then W28c reassigns the same span (~9.8k assigns / 10k heights).
    /// True-WAN dens-hash160 iter10k (2026-07-28): same class — `same_start` W28c p50≈19ms
    /// (P1c only blocks while in_flight still held; obsolete→complete clears it).
    ///
    /// Escape: `tip_gap_missing` for rearm grace **only when the tip is not already in
    /// validation**. Env: `BLVM_IBD_TIP_DEDUP_REARM_MS` (fallback `BLVM_IBD_SYNTH_DEDUP_REARM_MS`,
    /// default 5000).
    fn tip_owner_blocked_by_dedup(&self, next_needed: u64) -> bool {
        let dedup = super::memory::GAP_STREAM_DEDUP_HEIGHT.load(Ordering::Relaxed);
        if dedup < next_needed {
            self.synth_tip_dedup_block_since_ms.store(0, Ordering::Relaxed);
            return false;
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // Tip already in validation — not a lost body. Keep DEDUP hold and refresh the
        // grace clock so a slow AV=0 height cannot accumulate into a false REARM.
        if super::tip_stage::tip_taken_by_validation(next_needed) {
            self.synth_tip_dedup_block_since_ms
                .store(now_ms.max(1), Ordering::Relaxed);
            return true;
        }
        // Default 5s: kill complete→reassign storms (tens of ms); true tip-loss (not
        // tip_taken) can still recover via tip-owner. LOCAL_GAP remains the primary path.
        let grace_ms: u64 = std::env::var("BLVM_IBD_TIP_DEDUP_REARM_MS")
            .ok()
            .or_else(|| std::env::var("BLVM_IBD_SYNTH_DEDUP_REARM_MS").ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(5_000)
            .clamp(250, 30_000);
        let since = self.synth_tip_dedup_block_since_ms.load(Ordering::Relaxed);
        if since == 0 {
            self.synth_tip_dedup_block_since_ms
                .store(now_ms.max(1), Ordering::Relaxed);
            return true;
        }
        let waited = now_ms.saturating_sub(since);
        if waited >= grace_ms && self.tip_gap_missing.load(Ordering::Relaxed) {
            // Rewind DEDUP so the hold does not immediately re-engage on the same tip.
            let rewind = next_needed.saturating_sub(1);
            let _ = super::memory::GAP_STREAM_DEDUP_HEIGHT.compare_exchange(
                dedup,
                rewind,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            tracing::warn!(
                "[IBD_TIP_DEDUP_REARM] tip={} dedup={}→{} after {}ms gap_missing — \
                 allow tip-owner re-download",
                next_needed,
                dedup,
                rewind,
                waited
            );
            self.synth_tip_dedup_block_since_ms.store(0, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Backward-compatible alias for unit tests / older call sites.
    #[cfg(test)]
    fn synth_tip_owner_blocked_by_dedup(&self, next_needed: u64) -> bool {
        self.tip_owner_blocked_by_dedup(next_needed)
    }

    pub(crate) fn requeue(&self, start: u64, end: u64, exclude_peer: Option<String>) {
        // W80: ChunkGuard Drop re-queues every aborted range. Behind-tip spans
        // (live: 309798-309925 while tip≈321k) recirculated as W40 "local tip-hole"
        // owner work and stole peers from the real tip. Drop obsolete ranges.
        let next = self.next_needed_height();
        if end < next {
            return;
        }
        // Use exclude_peer to avoid immediate retry with same peer, but stall recovery can clear it
        self.retry_queue
            .lock()
            .unwrap()
            .push_back((start, end, exclude_peer));
    }

    /// Drop retry-queue entries entirely behind the live tip (W80).
    fn purge_obsolete_retries(&self, next_needed: u64) {
        let mut rq = self.retry_queue.lock().unwrap();
        let before = rq.len();
        if before == 0 {
            return;
        }
        rq.retain(|(_, e, _)| *e >= next_needed);
        let dropped = before.saturating_sub(rq.len());
        if dropped > 0 {
            tracing::warn!(
                "[IBD_RETRY_PURGE] dropped {} obsolete behind-tip retry chunk(s) (next_needed={})",
                dropped,
                next_needed
            );
        }
    }

    pub(crate) fn next_needed_height(&self) -> u64 {
        self.validation_height.load(Ordering::Relaxed) + 1
    }

    /// W17/W28d diagnostics: `(raw_covering, in_flight_ranges, busy_peers)`.
    /// Prefer [`Self::tip_flight_diag_healthy`] when distinguishing walk-ins.
    pub(crate) fn tip_flight_diag(&self) -> (usize, usize, usize) {
        let next_needed = self.next_needed_height();
        let guard = self.in_flight_per_peer.lock().unwrap();
        let covering = Self::covering_next_count(&guard, next_needed);
        let ranges = guard.values().map(|v| v.len()).sum();
        let busy = guard.values().filter(|v| !v.is_empty()).count();
        (covering, ranges, busy)
    }

    /// W28d: `(healthy_tip_claims, raw_covering, in_flight_ranges)`.
    pub(crate) fn tip_flight_diag_healthy(&self) -> (usize, usize, usize) {
        let next_needed = self.next_needed_height();
        let healthy = self.healthy_tip_cover_count(next_needed);
        let guard = self.in_flight_per_peer.lock().unwrap();
        let raw = Self::covering_next_count(&guard, next_needed);
        let ranges = guard.values().map(|v| v.len()).sum();
        (healthy, raw, ranges)
    }

    /// Enqueue single-height micro-chunks at the front of the retry queue so multiple peers can
    /// race missing blocks without re-downloading entire 128-block chunks.
    ///
    /// `count` is not clamped — callers that know the full remaining range of a failed chunk
    /// (e.g. [`Self::requeue_stall_gaps`]) pass the exact block count so recovery covers the
    /// entire chunk in one stall event rather than drip-feeding 8 blocks per event.
    pub(crate) fn requeue_gap_heights(&self, from: u64, count: u64, exclude: Option<String>) {
        let count = count.max(1);
        let mut rq = self.retry_queue.lock().unwrap();
        let mut added = 0u64;
        for h in from..from.saturating_add(count) {
            if rq.iter().any(|(s, e, ex)| *s == h && *e == h && ex == &exclude) {
                continue;
            }
            rq.push_front((h, h, exclude.clone()));
            added += 1;
        }
        if added > 0 {
            tracing::warn!(
                "stall recovery: gap micro-chunks {}-{} ({} new, up to {} fetchers/height)",
                from,
                from.saturating_add(count).saturating_sub(1),
                added,
                self.max_gap_fetchers_per_height()
            );
        }
    }

    pub(crate) fn requeue_gap_height(&self, height: u64) {
        self.requeue_gap_heights(height, 1, None);
    }

    /// Coordinator stall recovery: dispatch single-height micro-chunks for the full remaining
    /// range of the chunk containing `height`.
    ///
    /// **Why single-height micro-chunks (not one big range):**
    /// Single-height micro-chunks are fault-tolerant — each block is independently retried by
    /// a different peer. One big multi-block range fails atomically: if any block in the range
    /// stalls, the whole range re-queues and generates another stall, causing cascading recovery.
    ///
    /// `get_work()` coalesces consecutive `(H,H)` singles into a merged `(H, H+k)` range before
    /// assignment, so the network efficiency (16-block GetData batches) is recovered without
    /// sacrificing fault tolerance.
    ///
    /// `exclude` — pass `Some(peer_id)` when a specific peer caused the drop so it can't
    /// immediately re-claim the heights it just failed on.
    pub(crate) fn requeue_stall_gaps(&self, height: u64, exclude: Option<String>) {
        self.requeue_stall_gaps_inner(height, exclude, false);
    }

    /// W24: bypass debounce when tip covering collapsed to 0 (nothing in flight for the gap).
    pub(crate) fn requeue_stall_gaps_force(&self, height: u64, exclude: Option<String>) {
        self.requeue_stall_gaps_inner(height, exclude, true);
    }

    fn requeue_stall_gaps_inner(&self, height: u64, exclude: Option<String>, force: bool) {
        let wan_gap = self.wan_tip_gap_crawl(height);
        // Real WAN assign (body/header tip set): non-force stall stays preempt/nudge/SLA-only.
        // Dense-local unit (body_tip=0, header_tip=0) still enqueues stall micros — dens KEEP
        // requeue_chunk / debounce / exclude-clear tests rely on that path.
        let assign_wan = wan_gap
            && (self.wan_body_tip.load(Ordering::Relaxed) > 0 || self.header_tip() > 0);
        // P0-B: non-force WAN tip recovery stays preempt/nudge/SLA-only.
        // W73: force + covering==0 arms a single (H,H) tip hole (genesis 262716 hard stall).
        if assign_wan && !force {
            tracing::debug!(
                "stall recovery: skipped WAN tip gap height {} (gap preempt/nudge/SLA only)",
                height
            );
            return;
        }
        if assign_wan && force {
            let (covering, _, _) = self.tip_flight_diag();
            if covering > 1 {
                tracing::debug!(
                    "stall recovery: skip WAN force at {} — tip covering={}",
                    height,
                    covering
                );
                return;
            }
            let debounce = Self::stall_requeue_debounce();
            let mut last = self.last_stall_requeue.lock().unwrap();
            if let Some((h, t)) = *last {
                if t.elapsed() < debounce {
                    // Dens parity (2026-08-03): debounce was same-height only, so tip
                    // 400288→289→290 each got a fresh (H,H) force while covering flapped 0
                    // (nudgegate FORCE×32 vs grow80 dens FORCE×0). Cross-height suppress
                    // inside the window; true stall (same height after debounce) still arms.
                    tracing::debug!(
                        "stall recovery: debounced WAN force requeue for height {} (last={} within {}s)",
                        height,
                        h,
                        debounce.as_secs()
                    );
                    return;
                }
            }
            *last = Some((height, Instant::now()));
            drop(last);
            // W73: covering=0 → single (H,H). Stripe-32 FORCE (2026-08) bypasses C1g
            // "race tip H only" and re-cheeses TIP_HOLE_AHEAD (Land E soak 12 FORCE@403747
            // cratered dens 53/30; soak 15 C1G@402000 with ahead_buf=68). C1e owner stripe
            // still assigns via get_work when covering=0; recovery must not enqueue tip+31.
            tracing::warn!(
                "[IBD_WAN_TIP_FORCE_REQUEUE] height {} covering=0 — enqueue tip H (H,H)",
                height
            );
            self.requeue_gap_heights(height, 1, exclude);
            return;
        }
        let debounce = Self::stall_requeue_debounce();
        let mut last = self.last_stall_requeue.lock().unwrap();
        if !force {
            if let Some((h, t)) = *last {
                if h == height && t.elapsed() < debounce {
                    tracing::debug!(
                        "stall recovery: debounced requeue for height {} (within {}s)",
                        height,
                        debounce.as_secs()
                    );
                    return;
                }
            }
        }
        *last = Some((height, Instant::now()));
        let containing = self
            .chunks
            .iter()
            .find(|(s, e)| height >= *s && height <= *e)
            .copied();
        let Some((start, end)) = containing else {
            tracing::warn!(
                "stall recovery: height {} not in any assigner chunk (chunks={})",
                height,
                self.chunks.len()
            );
            // No containing chunk found — fall back to a small batch so recovery doesn't stall
            // forever on a gap that slipped out of the chunk map (e.g. very old retry).
            self.requeue_gap_heights(height, self.gap_micro_chunk_batch(), exclude);
            return;
        };
        // W4 / W9: enqueue one bulk multi-block range first. Do NOT also flood (H,H) micros
        // when bulk is queued — live: peer took bulk then 13 separate (H,H) assigns in 1ms
        // (coalesce lost across get_work cycles). Keep a single (H,H) race only when bulk
        // cannot be formed (narrow_count == 1).
        let narrow_count = 16u64.min(end.saturating_sub(height).saturating_add(1));
        if narrow_count > 1 {
            let bulk_end = height.saturating_add(narrow_count.saturating_sub(1));
            let mut rq = self.retry_queue.lock().unwrap();
            let already = rq
                .iter()
                .any(|(s, e, ex)| *s == height && *e == bulk_end && ex == &exclude);
            if !already {
                rq.push_front((height, bulk_end, exclude.clone()));
                tracing::warn!(
                    "stall recovery: bulk gap chunk {}-{} ({} blocks)",
                    height,
                    bulk_end,
                    narrow_count
                );
            }
            // One (H,H) race for the exact gap alongside bulk — no H+1..H+15 micro flood.
            drop(rq);
            self.requeue_gap_heights(height, 1, exclude.clone());
        } else {
            self.requeue_gap_heights(height, 1, exclude.clone());
        }
        // Also clear the exclude on any existing full-chunk retry so a different peer can take
        // the full chunk in parallel with the micro-chunks.
        let mut rq = self.retry_queue.lock().unwrap();
        if let Some(entry) = rq.iter_mut().find(|(s, e, _)| *s == start && *e == end) {
            if entry.2.is_some() {
                entry.2 = None;
                tracing::warn!(
                    "stall recovery: cleared exclude on chunk {}-{} for missing height {}",
                    start,
                    end,
                    height
                );
            }
        }
    }

    /// When validation/coordinator stalls on a missing height, workers may have no in-flight chunk
    /// covering that height (chunk was already marked complete after a bad download).
    pub(crate) fn requeue_chunk_containing_height(&self, height: u64) {
        self.requeue_stall_gaps(height, None);
    }

    pub(crate) fn is_done(&self) -> bool {
        // Explicit teardown or IBD end reached: workers must exit so Phase 3 / join
        // cannot hang. Mid-IBD tip-crawl keep-alive below must NOT apply past end.
        if self.shutdown.load(Ordering::Acquire) || self.validation_reached_ibd_end() {
            return true;
        }
        // W36: keep workers alive while tip is missing or we are past body tip — otherwise
        // free workers exit on empty main queue and only the stuck tip owner remains
        // (live: covering=1 zombie, no reassign for 17 min after SLA).
        if self.tip_gap_missing.load(Ordering::Relaxed) {
            return false;
        }
        let next = self.next_needed_height();
        if self.wan_tip_gap_crawl(next) {
            return false;
        }
        {
            let guard = self.in_flight_per_peer.lock().unwrap();
            if guard.values().any(|v| !v.is_empty()) {
                return false;
            }
        }
        let idx = self.next_index.load(Ordering::Relaxed);
        idx >= self.chunks.len() && self.retry_queue.lock().unwrap().is_empty()
    }

    pub(crate) fn total_chunks(&self) -> usize {
        self.chunks.len()
    }

    pub(crate) fn remaining_count(&self) -> usize {
        let idx = self.next_index.load(Ordering::Relaxed);
        let retry_len = self.retry_queue.lock().unwrap().len();
        self.chunks.len().saturating_sub(idx) + retry_len
    }
}

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
            self.assigner.requeue(start, end, exclude);
        } else if let Some(peer_id) = self.peer_id.take() {
            self.assigner.on_chunk_complete(&peer_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::atomic::AtomicU64;

    fn assigner_for_heights(
        chunks: &[(u64, u64)],
        peers: &[&str],
        start_height: u64,
        work_stealing: bool,
    ) -> ChunkAssigner {
        ChunkAssigner::new(
            chunks.to_vec(),
            peers.iter().map(|s| (*s).to_string()).collect(),
            Arc::new(AtomicU64::new(0)),
            start_height,
            work_stealing,
        )
    }

    #[test]
    fn note_wan_tip_stream_increments_on_hit_without_reset() {
        let a = assigner_for_heights(&[(100, 200)], &["p"], 100, true);
        a.note_wan_tip_stream("p");
        a.note_wan_tip_stream("p");
        a.note_wan_tip_stream("p");
        assert_eq!(a.tip_stream_count("p"), 3);
        assert_eq!(a.tip_stream_count("other"), 0);
    }

    /// Build a WAN work-stealing assigner for tip/gap tests: one covering range + peer workers.
    /// No fake peer-per-range padding — ranges and workers are independent.
    fn wan_tip_assigner(
        validation_height: u64,
        body_tip: u64,
        header_tip: u64,
        peers: &[&str],
    ) -> ChunkAssigner {
        let start = body_tip.min(validation_height);
        let assigner = ChunkAssigner::new(
            vec![(start, header_tip)],
            peers.iter().map(|s| (*s).to_string()).collect(),
            Arc::new(AtomicU64::new(validation_height)),
            start,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(body_tip);
        assigner.set_header_tip(header_tip);
        assigner
    }

    fn mark_scored_peers_ibd_ready(assigner: &ChunkAssigner) {
        assigner.set_ibd_ready_peers(
            assigner.peer_ids_for_ibd_ready().into_iter().collect(),
        );
    }

    fn mark_peers_ibd_ready(assigner: &ChunkAssigner, peers: &[&str]) {
        assigner.set_ibd_ready_peers(peers.iter().map(|s| s.to_string()).collect());
    }

    /// W4/N12: snapshot deep/healthy counts match live Mutex readers.
    #[test]
    fn w4_tip_cover_snapshot_counts_match_live() {
        let assigner = wan_tip_assigner(300_000, 300_000, 301_000, &["pA", "pB"]);
        let tip = 300_001;
        // Shallow failover micro — healthy but not deep (min depth default 16).
        assigner.note_tip_cover_claim("pA", tip, tip);
        // Deep pipe claim.
        assigner.note_tip_cover_claim("pB", tip, tip + 127);
        let snap = assigner.snapshot_tip_cover_claims();
        assert_eq!(
            ChunkAssigner::healthy_tip_cover_count_from(&snap, tip),
            assigner.healthy_tip_cover_count(tip)
        );
        assert_eq!(
            ChunkAssigner::deep_tip_cover_count_from(&snap, tip),
            assigner.deep_tip_cover_count(tip)
        );
        assert_eq!(assigner.healthy_tip_cover_count(tip), 2);
        assert_eq!(assigner.deep_tip_cover_count(tip), 1);
    }

    #[test]
    fn get_work_assigns_sequential_chunks_per_peer() {
        let chunks = vec![(200, 263), (264, 327)];
        let assigner = assigner_for_heights(&chunks, &["p1", "p2"], 200, false);
        let w0 = assigner.get_work("p1", 1000).expect("chunk 0");
        assert_eq!(w0, (200, 263));
        assert!(
            assigner.get_work("p1", 1000).is_none(),
            "one in flight per peer"
        );
        assigner.on_chunk_complete("p1");
        assigner.mark_bootstrap_complete();
        let w1 = assigner.get_work("p2", 1000).expect("chunk 1");
        assert_eq!(w1, (264, 327));
    }

    #[test]
    fn main_queue_assigns_next_height_when_max_ahead_zero() {
        let chunks = vec![(955186, 955244)];
        let vh = Arc::new(AtomicU64::new(955185));
        let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], Arc::clone(&vh), 955186, true);
        assert_eq!(
            assigner.get_work("p1", 0),
            Some((955186, 955244)),
            "next block must be assignable even when max_ahead=0"
        );
    }

    #[test]
    fn bootstrap_serializes_until_marked_complete() {
        let chunks = vec![(0, 127), (128, 255)];
        let assigner = assigner_for_heights(&chunks, &["p1"], 0, false);
        assert_eq!(assigner.get_work("p1", 1000), Some((0, 127)));
        assigner.on_chunk_complete("p1");
        assert!(
            assigner.get_work("p1", 1000).is_none(),
            "second chunk blocked until bootstrap done"
        );
        assigner.mark_bootstrap_complete();
        // vh=0 → next_needed=1 mid first-chunk range → W16 tip-fills before main queue.
        assert_eq!(assigner.get_work("p1", 1000), Some((1, 16)));
    }

    #[test]
    fn work_stealing_gap_fetcher_defaults() {
        // W28b/W28c: one tip owner by default (failover may raise to 2 at runtime).
        // start_height>0 auto-completes bootstrap; pin body tip so this is not WAN gap
        // (WAN + deep_cover==0 → fetchers=2 by W41 design).
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        let prev = std::env::var("BLVM_IBD_GAP_FETCHERS").ok();
        unsafe { std::env::remove_var("BLVM_IBD_GAP_FETCHERS") };
        let ws = assigner_for_heights(&[(100, 199)], &["p1"], 100, true);
        ws.set_confirmed_body_height_at_start(10_000);
        assert_eq!(ws.max_gap_fetchers_per_height(), 1);
        assert_eq!(ws.gap_micro_chunk_batch(), 32);
        let lan = assigner_for_heights(&[(100, 199)], &["p1"], 100, false);
        lan.set_confirmed_body_height_at_start(10_000);
        assert_eq!(lan.max_gap_fetchers_per_height(), 1);
        assert_eq!(lan.gap_micro_chunk_batch(), 8);
        match prev {
            Some(v) => unsafe { std::env::set_var("BLVM_IBD_GAP_FETCHERS", v) },
            None => unsafe { std::env::remove_var("BLVM_IBD_GAP_FETCHERS") },
        }
    }

    #[test]
    fn w28c_sticky_tip_owner_prefers_best_scored() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(1000));
        let chunks = vec![(1000, 1200)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["bind".into()],
            Arc::clone(&vh),
            1000,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(900);
        assigner.set_peer_scores(&[("slow".into(), 1.0), ("fast".into(), 9.0)]);
        mark_scored_peers_ibd_ready(&assigner);
        // Low-score peer must not win tip while high-score has capacity.
        assert_eq!(
            assigner.get_work("slow", 1000),
            None,
            "slow peer must not take tip ownership while fast is free"
        );
        let tip = assigner.get_work("fast", 1000);
        assert!(tip.is_some(), "fast peer should take tip ownership");
        let (s, e) = tip.unwrap();
        assert_eq!(s, 1001);
        assert!(e >= s + 31, "WAN tip owner should pipeline deeply, got {s}-{e}");
        // Sticky: after assign, slow still shouldn't steal tip.
        let slow2 = assigner.get_work("slow", 1000);
        if let Some((ss, ee)) = slow2 {
            assert!(ss > e, "slow gets ahead partition only, got {ss}-{ee}");
        }
    }

    #[test]
    fn w28c_failover_allows_second_tip_cover() {
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        let vh = Arc::new(AtomicU64::new(1000));
        let chunks = vec![(1000, 1200)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["bind".into()],
            Arc::clone(&vh),
            1000,
            true,
        );
        assigner.mark_bootstrap_complete();
        // Pre-body tip: failover path still valid (not WAN gap crawl).
        assigner.set_confirmed_body_height_at_start(2000);
        assert_eq!(assigner.get_work("pA", 1000).map(|(s, _)| s), Some(1001));
        // Without failover, pB must not cover tip.
        let before = assigner.get_work("pB", 1000);
        if let Some((s, _)) = before {
            assert!(s > 1001, "no failover yet — ahead only, got start={s}");
        }
        assigner.on_chunk_complete("pB");
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            // Keep pA as tip owner only.
            g.retain(|p, _| p == "pA");
        }
        // W37: armed alone is not enough — soft-retry freeze must be latched.
        super::super::tip_stage::arm_tip_failover();
        assert_eq!(
            assigner.max_gap_fetchers_per_height(),
            1,
            "armed without freeze must not open covering=2"
        );
        super::super::tip_stage::mark_needed(1001);
        // mark_needed clears armed latch on height roll — re-arm as download.rs does
        // after soft-retry (arm follows mark_soft_retry in production).
        super::super::tip_stage::mark_soft_retry(1001);
        super::super::tip_stage::arm_tip_failover();
        assert_eq!(assigner.max_gap_fetchers_per_height(), 2);
        assert_eq!(
            assigner.get_work("pB", 1000),
            Some((1001, 1001)),
            "failover assigns tip height only"
        );
        // W86: with covering already at fetchers_cap (deep+failover), no more tip micros.
        let third = assigner.get_work("pC", 1000);
        if let Some((s, e)) = third {
            assert!(
                !(s == 1001 && e == 1001),
                "W86: must not stack another tip failover, got {s}-{e}"
            );
        }
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn w86_wan_distress_does_not_stack_unbounded_tip_failover() {
        // Live W85: tip_distress + healthy-only gate + overlaps_ok=failover stacked
        // thousands of (H,H) assigns on a handful of tip heights.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        let vh = Arc::new(AtomicU64::new(300_000));
        let assigner = ChunkAssigner::new(
            vec![(300_001, 300_200)],
            vec!["pA".into(), "pB".into(), "pC".into(), "pD".into()],
            Arc::clone(&vh),
            1,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(0);
        assigner.set_peer_scores(&[
            ("pA".into(), 1.0),
            ("pB".into(), 0.9),
            ("pC".into(), 0.8),
            ("pD".into(), 0.7),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "pA".into(),
            "pB".into(),
            "pC".into(),
            "pD".into(),
        ]));
        assigner.set_tip_gap_missing(true);
        super::super::tip_stage::mark_needed(300_001);
        // Force distress via soft-retry latch (deterministic in tests).
        super::super::tip_stage::mark_soft_retry(300_001);
        assert_eq!(assigner.max_gap_fetchers_per_height(), 2);
        let owner = assigner.get_work("pA", 1000);
        assert!(owner.is_some(), "deep tip owner must assign");
        let (os, oe) = owner.unwrap();
        assert!(oe > os, "deep tip pipe expected, got {os}-{oe}");
        // First failover ok.
        assert_eq!(
            assigner.get_work("pB", 1000),
            Some((300_001, 300_001)),
            "one tip failover micro under distress"
        );
        // W87: even after failover peer drops in-flight (fail→requeue), no second (H,H).
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            if let Some(v) = g.get_mut("pB") {
                v.retain(|(s, e)| !(*s == 300_001 && *e == 300_001));
            }
        }
        let mut tip_micros = 0usize;
        for peer in ["pC", "pD", "pB", "pC", "pD"] {
            if let Some((s, e)) = assigner.get_work(peer, 1000) {
                if s == 300_001 && e == 300_001 {
                    tip_micros += 1;
                }
            }
        }
        assert_eq!(
            tip_micros, 0,
            "W86/W87: must not stack/reassign tip failover micros, got {tip_micros}"
        );
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::mark_needed(0);
    }

    fn c1u_tests_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        L.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn c1u_handoff_prime_assigns_past_body_tip_while_local() {
        // Binder cliff: local ahead ~690 BPS then body tip GetData cold → ~13 BPS.
        // Near_tip prime only on the last local height (next>=body_tip) with cover —
        // mid-window cover+prime freezes (C0 T025719Z next=304649 body_tip=304663).
        let _env = c1u_tests_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_HANDOFF_PRIME", "64");
            std::env::set_var("BLVM_IBD_TIP_RUNWAY_STRIPE", "32");
            std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_CAP", "32");
            std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_START", "8");
            std::env::set_var("BLVM_IBD_TIP_HOLE_STICKY", "1");
            std::env::remove_var("BLVM_IBD_GAP_PREEMPT_BATCH");
            std::env::remove_var("BLVM_IBD_SYNTH_WAN");
            std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
        }
        let body_tip = 304_663u64;
        let vh = Arc::new(AtomicU64::new(body_tip - 1)); // next = body_tip (last local)
        let assigner = ChunkAssigner::new(
            vec![(300_000, 320_000)],
            vec!["pA".into(), "pB".into()],
            Arc::clone(&vh),
            300_000,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(body_tip);
        assigner.set_wan_body_tip(body_tip);
        assigner.set_header_tip(400_000);
        assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9)]);
        assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into()]));
        assigner.set_tip_gap_missing(false); // local tip filled via LOCAL_GAP

        let next = vh.load(Ordering::Relaxed).saturating_add(1);
        assert_eq!(next, body_tip);
        assert!(
            assigner.handoff_prime_active(next),
            "next_needed={next} must be inside HANDOFF_PRIME of body_tip={body_tip}"
        );
        assert!(
            !assigner.handoff_prime_active(body_tip - 200),
            "far local must not prime via near_tip window alone"
        );
        assert!(
            !assigner.handoff_prime_active(body_tip + 1),
            "already past body tip is WAN crawl, not handoff prime"
        );

        // Uncovered tip: must take local cover, not steal onto body_tip+1.
        let cover = assigner.get_work("pA", 1000);
        assert!(cover.is_some(), "tip owner must cover local tip first");
        let (cs, ce) = cover.unwrap();
        assert!(
            cs <= next && ce >= next && ce <= body_tip,
            "uncovered near_tip must assign local tip cover, got {cs}-{ce}"
        );

        // Sticky often has top_peer cap≥2 → primes on second poll; else fallback / after complete.
        let mut prime = assigner
            .get_work("pA", 1000)
            .or_else(|| assigner.get_work("pB", 1000))
            .filter(|(s, _)| *s == body_tip + 1);
        if prime.is_none() {
            assigner.on_chunk_complete_range("pA", cs, ce);
            prime = assigner
                .get_work("pA", 1000)
                .filter(|(s, _)| *s == body_tip + 1);
        }
        assert_eq!(
            prime,
            Some((body_tip + 1, body_tip + 32)),
            "after tip cover on last local, handoff prime must assign body_tip+1..+stripe, got {prime:?}"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_HANDOFF_PRIME");
            std::env::remove_var("BLVM_IBD_TIP_RUNWAY_STRIPE");
        }
    }

    #[test]
    fn c1u_near_tip_prime_blocked_while_local_gap_remains() {
        // Live C0 freeze: next=304649 body_tip=304663 covering>0 → prime stole sticky.
        let _env = c1u_tests_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_HANDOFF_PRIME", "256");
            std::env::set_var("BLVM_IBD_TIP_RUNWAY_STRIPE", "32");
            std::env::remove_var("BLVM_IBD_SYNTH_WAN");
            std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
        }
        let body_tip = 304_663u64;
        let vh = Arc::new(AtomicU64::new(304_648)); // next=304649
        let assigner = ChunkAssigner::new(
            vec![(300_000, 320_000)],
            vec!["pA".into(), "pB".into()],
            Arc::clone(&vh),
            300_000,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(body_tip);
        assigner.set_wan_body_tip(body_tip);
        assigner.set_header_tip(400_000);
        assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9)]);
        assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into()]));
        assigner.set_tip_gap_missing(false);

        let next = vh.load(Ordering::Relaxed) + 1;
        assert!(assigner.handoff_prime_active(next));
        let cover = assigner.get_work("pA", 1000).expect("local cover");
        assert!(
            cover.0 <= next && cover.1 >= next && cover.1 <= body_tip,
            "must cover local gap, got {}-{}",
            cover.0,
            cover.1
        );
        // Even with cover, mid-window must not prime body_tip+1.
        for peer in ["pA", "pB"] {
            if let Some((s, e)) = assigner.get_work(peer, 1000) {
                assert!(
                    s != body_tip + 1,
                    "{peer} must not near_tip-prime while next={next}<body_tip, got {s}-{e}"
                );
                assigner.on_chunk_complete_range(peer, s, e);
            }
        }
        unsafe {
            std::env::remove_var("BLVM_IBD_HANDOFF_PRIME");
            std::env::remove_var("BLVM_IBD_TIP_RUNWAY_STRIPE");
        }
    }

    #[test]
    fn c1u_near_tip_prime_requires_tip_cover() {
        // Regression: dens early near_tip prime with covering=0 → freeze hole under cheese.
        // FAIL DNA: next=437080, body_tip=437309, HANDOFF_PRIME=256.
        let _env = c1u_tests_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_HANDOFF_PRIME", "256");
            std::env::set_var("BLVM_IBD_TIP_RUNWAY_STRIPE", "32");
            std::env::remove_var("BLVM_IBD_SYNTH_WAN");
            std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
        }
        let body_tip = 437_309u64;
        let vh = Arc::new(AtomicU64::new(437_079)); // next=437080 inside prime=256
        let assigner = ChunkAssigner::new(
            vec![(400_000, 450_000)],
            vec!["pA".into(), "pB".into()],
            Arc::clone(&vh),
            400_000,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(body_tip);
        assigner.set_wan_body_tip(body_tip);
        assigner.set_header_tip(500_000);
        assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9)]);
        assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into()]));
        assigner.set_tip_gap_missing(false);

        let next = vh.load(Ordering::Relaxed) + 1;
        assert!(
            assigner.handoff_prime_active(next),
            "FAIL DNA next={next} body_tip={body_tip} must arm near_tip"
        );
        let work = assigner.get_work("pA", 1000);
        let (s, e) = work.expect("must assign");
        assert!(
            e <= body_tip && s <= next && e >= next,
            "covering=0 near_tip must return local tip span, not prime; got {s}-{e}"
        );
        assert!(
            s != body_tip + 1,
            "must not HANDOFF_PRIME while next_needed uncovered; got {s}-{e}"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_HANDOFF_PRIME");
            std::env::remove_var("BLVM_IBD_TIP_RUNWAY_STRIPE");
        }
    }

    #[test]
    fn c1u_handoff_prime_blocks_local_ahead_partitions() {
        // During HANDOFF_PRIME, second peer must not W28c-ahead cheese ≤ body_tip.
        let _env = c1u_tests_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_HANDOFF_PRIME", "256");
            std::env::set_var("BLVM_IBD_TIP_RUNWAY_STRIPE", "32");
            std::env::set_var("BLVM_IBD_TIP_PARTITION_WINDOW", "256");
            std::env::remove_var("BLVM_IBD_SYNTH_WAN");
            std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
        }
        let body_tip = 437_309u64;
        let vh = Arc::new(AtomicU64::new(437_079));
        let assigner = ChunkAssigner::new(
            vec![(400_000, 450_000)],
            vec!["pA".into(), "pB".into(), "pC".into()],
            Arc::clone(&vh),
            400_000,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(body_tip);
        assigner.set_wan_body_tip(body_tip);
        assigner.set_header_tip(500_000);
        assigner.set_peer_scores(&[
            ("pA".into(), 1.0),
            ("pB".into(), 0.9),
            ("pC".into(), 0.8),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "pA".into(),
            "pB".into(),
            "pC".into(),
        ]));
        assigner.set_tip_gap_missing(false);

        let tip = assigner.get_work("pA", 512);
        assert!(tip.is_some(), "sticky must take tip cover");
        let (ts, te) = tip.unwrap();
        assert!(te <= body_tip, "tip cover must stay local, got {ts}-{te}");

        // pB may fallback-prime past tip, but must NOT get ahead partition ≤ body_tip.
        let b = assigner.get_work("pB", 512);
        if let Some((s, e)) = b {
            assert!(
                s > body_tip,
                "handoff_prime must block local ahead partitions; pB got {s}-{e}"
            );
        }
        let c = assigner.get_work("pC", 512);
        if let Some((s, e)) = c {
            assert!(
                s > body_tip,
                "handoff_prime must block local ahead partitions; pC got {s}-{e}"
            );
        }
        unsafe {
            std::env::remove_var("BLVM_IBD_HANDOFF_PRIME");
            std::env::remove_var("BLVM_IBD_TIP_RUNWAY_STRIPE");
            std::env::remove_var("BLVM_IBD_TIP_PARTITION_WINDOW");
        }
    }

    #[test]
    fn c1u_local_ahead_clips_to_body_tip_and_primes_via_frontier() {
        // Live fail: ahead assigned 304672 while tip=304418 (past body_tip=304663) → cheese.
        // Local ahead must clip at body tip; once frontier is there, tip-owner primes WAN.
        unsafe {
            std::env::set_var("BLVM_IBD_HANDOFF_PRIME", "256");
            std::env::set_var("BLVM_IBD_TIP_RUNWAY_STRIPE", "32");
            std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_CAP", "32");
            std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_START", "8");
            std::env::remove_var("BLVM_IBD_SYNTH_WAN");
            std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
        }
        let body_tip = 304_663u64;
        // Far behind near_tip window (PRIME=256) — only ahead_frontier may prime.
        let vh = Arc::new(AtomicU64::new(body_tip - 400));
        let assigner = ChunkAssigner::new(
            vec![(300_000, 320_000)],
            vec!["pA".into(), "pB".into(), "pC".into()],
            Arc::clone(&vh),
            300_000,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(body_tip);
        assigner.set_wan_body_tip(body_tip);
        assigner.set_header_tip(400_000);
        assigner.set_peer_scores(&[
            ("pA".into(), 1.0),
            ("pB".into(), 0.9),
            ("pC".into(), 0.8),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "pA".into(),
            "pB".into(),
            "pC".into(),
        ]));
        assigner.set_tip_gap_missing(false);

        assert!(
            !assigner.handoff_prime_active(vh.load(Ordering::Relaxed) + 1),
            "far local must not arm near_tip window"
        );

        // Tip-owner takes local tip cover (clipped at body tip).
        let tip_work = assigner.get_work("pA", 512);
        assert!(
            tip_work.is_some(),
            "tip owner should get local tip cover, got {tip_work:?}"
        );
        let (ts, te) = tip_work.unwrap();
        assert!(
            te <= body_tip,
            "tip-owner must not claim past body tip, got {ts}-{te}"
        );
        assert!(
            ts < body_tip,
            "far next_needed must start as local cover, not prime, got {ts}-{te}"
        );

        // Fill local ahead up to body tip with other peers (clipped).
        for _ in 0..40 {
            let w = assigner.get_work("pB", 512).or_else(|| assigner.get_work("pC", 512));
            if let Some((s, e)) = w {
                assert!(
                    e <= body_tip,
                    "local ahead must clip to body tip, got {s}-{e}"
                );
                if e >= body_tip {
                    break;
                }
            } else {
                break;
            }
        }

        // Prime only once next reaches the last local height (C0 freeze: mid-gap prime).
        assigner.on_chunk_complete_range("pA", ts, te);
        vh.store(body_tip - 1, Ordering::Relaxed);
        // Frontier may already cover body_tip — first poll can be the WAN prime.
        let mut got_prime = assigner
            .get_work("pA", 512)
            .or_else(|| assigner.get_work("pB", 512));
        if let Some((s, e)) = got_prime {
            if s <= body_tip && e >= body_tip && s != body_tip + 1 {
                assigner.on_chunk_complete_range("pA", s, e);
                got_prime = assigner
                    .get_work("pA", 512)
                    .or_else(|| assigner.get_work("pB", 512));
            } else {
                got_prime = Some((s, e));
            }
        }
        assert_eq!(
            got_prime,
            Some((body_tip + 1, body_tip + 32)),
            "on last local height, handoff-prime must assign, got {got_prime:?}"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_HANDOFF_PRIME");
            std::env::remove_var("BLVM_IBD_TIP_RUNWAY_STRIPE");
        }
    }

    #[test]
    fn c1t_gd_fast_subsecond_tip_height_race() {
        // Good-day mid-gaps: tip missing ~250–500ms, covering=1, soft-retry=0.
        // peer_may_take_tip_owner previously required soft/late-body (≥2s) → failover=0.
        let _tip_atomics = super::super::tip_stage::test_tip_atomics_lock();
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::test_reset_getdata_body_ewma();
        unsafe {
            std::env::set_var("BLVM_IBD_C1T_TIP_RACE_MS", "250");
            // Disable C1g await=0 (would open fetchers_cap whenever tip missing).
            std::env::set_var("BLVM_IBD_C1G_TIP_RACE_AWAIT_SECS", "30");
            std::env::set_var("BLVM_IBD_TIP_HOLE_PIPE", "128");
            std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_CAP", "32");
            std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_FAST_CAP", "48");
            std::env::set_var("BLVM_IBD_TIP_HOLE_GD_FAST", "1");
        }
        let vh = Arc::new(AtomicU64::new(300_000));
        let assigner = ChunkAssigner::new(
            vec![(300_001, 300_200)],
            vec!["pA".into(), "pB".into(), "pC".into()],
            Arc::clone(&vh),
            1,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(0);
        assigner.set_peer_scores(&[
            ("pA".into(), 1.0),
            ("pB".into(), 0.9),
            ("pC".into(), 0.8),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "pA".into(),
            "pB".into(),
            "pC".into(),
        ]));
        assigner.set_tip_gap_missing(true);
        super::super::tip_stage::mark_needed(300_001);
        // Mute EWMA — C1t gate must stay off (assert helper, not assign path).
        super::super::tip_stage::test_seed_getdata_body_ewma(3_000, 32);
        super::super::tip_stage::test_backdate_awaiting_ms(400);
        assert!(
            !assigner.c1t_tip_height_race(),
            "C1t mute guard: slow EWMA must not arm"
        );
        let owner = assigner.get_work("pA", 1000);
        assert!(owner.is_some(), "deep tip owner");
        let (os, oe) = owner.unwrap();
        assert!(oe > os, "deep tip pipe expected");
        // No tip-height race under mute (C1g disabled for this test).
        let mute_race = assigner.get_work("pB", 1000);
        if let Some((s, e)) = mute_race {
            assert!(
                !(s == 300_001 && e == 300_001),
                "mute must not tip-race, got {s}-{e}"
            );
        }
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.remove("pB");
        }
        assigner.tip_failover_once_h.store(0, Ordering::Relaxed);
        assigner.tip_failover_once_at_ms.store(0, Ordering::Relaxed);
        // Fast EWMA + tip missing 400ms → one (H,H).
        super::super::tip_stage::test_seed_getdata_body_ewma(100, 16);
        super::super::tip_stage::test_backdate_awaiting_ms(400);
        assert!(
            assigner.c1t_tip_height_race(),
            "C1t should arm under gd-fast + awaiting≥250ms"
        );
        assert_eq!(assigner.max_gap_fetchers_per_height(), 2);
        assert_eq!(
            assigner.get_work("pB", 1000),
            Some((300_001, 300_001)),
            "C1t tip-height failover under gd-fast"
        );
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::mark_needed(0);
        assigner.set_tip_gap_missing(false);
        unsafe {
            std::env::remove_var("BLVM_IBD_C1T_TIP_RACE_MS");
            std::env::remove_var("BLVM_IBD_C1G_TIP_RACE_AWAIT_SECS");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_PIPE");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GROW_CAP");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GROW_FAST_CAP");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_FAST");
        }
    }

    #[test]
    fn w112_empty_tip_triple_race_allows_second_failover_micro() {
        // Live W111 @323780: covering=2 mute rotate ~25s; third racer STREAM'd tip
        // in <1s once assigned. Empty bridge + awaiting≥12s → fetchers=3.
        let _tip_atomics = super::super::tip_stage::test_tip_atomics_lock();
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
        let vh = Arc::new(AtomicU64::new(323_779));
        let assigner = ChunkAssigner::new(
            vec![(323_780, 324_000)],
            vec!["pA".into(), "pB".into(), "pC".into(), "pD".into()],
            Arc::clone(&vh),
            1,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(0);
        assigner.set_peer_scores(&[
            ("pA".into(), 1.0),
            ("pB".into(), 0.9),
            ("pC".into(), 0.8),
            ("pD".into(), 0.7),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "pA".into(),
            "pB".into(),
            "pC".into(),
            "pD".into(),
        ]));
        assigner.set_tip_gap_missing(true);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(323_780);
        super::super::tip_stage::test_backdate_awaiting_ms(6_000);
        assert!(
            !assigner.empty_tip_triple_race(),
            "W112b: awaiting=6s < 12s default — keep covering=2"
        );
        assert_eq!(
            assigner.max_gap_fetchers_per_height(),
            2,
            "W112b: distress alone → covering=2"
        );
        // W122/W149: covering=1 mute reopen arms at 3s (before empty_triple @12s).
        super::super::tip_stage::test_backdate_awaiting_ms(3_000);
        assert!(
            assigner.mute_single_cover_reopen(1),
            "W149: covering=1 + awaiting≥3s"
        );
        assert!(
            !assigner.mute_single_cover_reopen(2),
            "W122: covering=2 must not mute-reopen"
        );
        assert!(
            !assigner.empty_tip_triple_race(),
            "W122: mute-reopen must not imply empty_triple"
        );
        super::super::tip_stage::test_backdate_awaiting_ms(13_000);
        assert!(
            assigner.empty_tip_triple_race(),
            "W112b: empty bridge + awaiting≥12s"
        );
        assert_eq!(
            assigner.max_gap_fetchers_per_height(),
            3,
            "W112: empty tip → covering=3"
        );
        let owner = assigner.get_work("pA", 1000);
        assert!(owner.is_some(), "deep tip owner");
        assert_eq!(
            assigner.get_work("pB", 1000),
            Some((323_780, 323_780)),
            "first failover micro"
        );
        // W88 episode latched — empty triple still opens a second micro.
        assert_eq!(
            assigner.get_work("pC", 1000),
            Some((323_780, 323_780)),
            "W112: second failover under covering=3"
        );
        let fourth = assigner.get_work("pD", 1000);
        if let Some((s, e)) = fourth {
            assert!(
                !(s == 323_780 && e == 323_780),
                "must not exceed covering=3, got {s}-{e}"
            );
        }
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::mark_needed(0);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    }

    #[test]
    fn w149_mute_reopen_at_3s_under_w88_episode() {
        // Live W148 tip-step ~5s/h: mute_reopen@5s never won the race under W88 latch.
        let _tip_atomics = super::super::tip_stage::test_tip_atomics_lock();
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
        let vh = Arc::new(AtomicU64::new(329_994));
        let assigner = ChunkAssigner::new(
            vec![(329_995, 330_200)],
            vec!["pA".into(), "pB".into(), "pC".into()],
            Arc::clone(&vh),
            1,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(0);
        assigner.set_peer_scores(&[
            ("pA".into(), 1.0),
            ("pB".into(), 0.9),
            ("pC".into(), 0.8),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "pA".into(),
            "pB".into(),
            "pC".into(),
        ]));
        assigner.set_tip_gap_missing(true);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(329_995);
        // Latch W88 episode as if a prior mute CAP already failover'd.
        assigner.latch_tip_failover_episode(329_995);
        super::super::tip_stage::test_backdate_awaiting_ms(3_000);
        assert!(
            assigner.mute_single_cover_reopen(1),
            "W149: covering=1 + awaiting≥3s reopens under W88"
        );
        // Awaiting=2s must NOT reopen (keep W88 cascade protection).
        super::super::tip_stage::test_backdate_awaiting_ms(2_000);
        assert!(
            !assigner.mute_single_cover_reopen(1),
            "W149: awaiting=2s stays under reopen trigger"
        );
        // get_work failover path is covered by w112 (serial); atomics race under
        // parallel download soft-budget tests.
        let _ = assigner;
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::mark_needed(0);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    }

    #[test]
    fn w120_shallow_end_of_pipe_deep_rearms_not_failover() {
        // W120: shallow cover (deep=0, raw=1) must deep re-arm, not (H,H) failover.
        // W117–W119 shallow-failover soaks rate-failed @306–311k; W116 DNA preferred.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::memory::BRIDGE_PENDING_COUNT.store(32, Ordering::Relaxed);
        let tip = 344_580u64;
        let vh = Arc::new(AtomicU64::new(tip - 1));
        let assigner = ChunkAssigner::new(
            vec![(tip + 1_000, tip + 1_100)],
            vec!["pDeep".into(), "pRace".into(), "pIdle".into()],
            Arc::clone(&vh),
            1,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(0);
        assigner.set_peer_scores(&[
            ("pDeep".into(), 1.0),
            ("pRace".into(), 0.9),
            ("pIdle".into(), 0.8),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "pDeep".into(),
            "pRace".into(),
            "pIdle".into(),
        ]));
        assigner.set_tip_gap_missing(true);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        assigner.set_tip_bridge_holes(1);
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.insert("pDeep".into(), vec![(tip - 3, tip)]);
        }
        assigner.note_tip_cover_claim("pDeep", tip - 3, tip);
        assigner.note_tip_owner_assigned("pDeep");
        assigner.tip_failover_once_h.store(0, Ordering::Relaxed);
        assigner.tip_failover_once_at_ms.store(0, Ordering::Relaxed);
        assert_eq!(assigner.deep_tip_cover_count(tip), 0, "shallow depth=4");
        super::super::tip_stage::mark_needed(tip);
        super::super::tip_stage::test_backdate_awaiting_ms(5_000);
        super::super::tip_stage::mark_soft_retry(tip);
        assert!(ChunkAssigner::tip_is_distressed());
        assigner.set_header_tip(tip + 500);
        let got = assigner.get_work("pRace", 1000);
        // Must not open W117-style shallow (H,H). Deep re-arm may be None in this
        // harness (open-slot streamer preference / sticky) — that's OK for W120.
        assert!(
            !matches!(got, Some((s, e)) if s == tip && e == tip),
            "W120: shallow must not open (H,H) failover, got {got:?}"
        );
        if let Some((s, e)) = got {
            assert!(s == tip && e > tip, "deep re-arm span, got {got:?}");
        }
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::mark_needed(0);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        assigner.set_tip_bridge_holes(0);
    }

    #[test]
    fn w37_local_ahead_sticky_failover_does_not_block_deep_owner() {
        // Live 2026-07-16: LOCAL_AHEAD soft-resume with tip_failover_armed stuck →
        // covering=2/2 (H,H) forever and 0 deep tip owners.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        let vh = Arc::new(AtomicU64::new(1000));
        let chunks = vec![(1000, 1200)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["pA".into(), "pB".into(), "pC".into()],
            Arc::clone(&vh),
            1000,
            true,
        );
        assigner.mark_bootstrap_complete();
        // Soft-resume: next_needed under confirmed body tip (not WAN gap crawl).
        assigner.set_confirmed_body_height_at_start(2000);
        assigner.set_peer_scores(&[
            ("pA".into(), 9.0),
            ("pB".into(), 8.0),
            ("pC".into(), 7.0),
        ]);
        mark_scored_peers_ibd_ready(&assigner);
        super::super::tip_stage::arm_tip_failover();
        // Stuck (H,H) micros from a prior soft-retry; freeze already cleared.
        assigner.note_tip_cover_claim("pB", 1001, 1001);
        assigner.note_tip_cover_claim("pC", 1001, 1001);
        assert_eq!(assigner.healthy_tip_cover_count(1001), 2);
        assert_eq!(assigner.deep_tip_cover_count(1001), 0);
        assert_eq!(assigner.max_gap_fetchers_per_height(), 1);

        let work = assigner.get_work("pA", 1000);
        assert!(work.is_some(), "deep owner must re-arm despite sticky failover micros");
        let (s, e) = work.unwrap();
        assert_eq!(s, 1001);
        assert!(e > s, "must be deep pipeline not (H,H), got {s}-{e}");
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn w31_wan_gap_max_fetchers_one_even_when_failover_armed() {
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        unsafe { std::env::set_var("BLVM_IBD_GAP_FETCHERS", "2") };
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["pA".into(), "pB".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[("pA".into(), 9.0), ("pB".into(), 1.0)]);
        mark_scored_peers_ibd_ready(&assigner);
        // Deep tip claim first — otherwise W41 deep_cover==0 keeps fetchers at 2.
        let tip = assigner.get_work("pA", 1000).expect("deep tip owner");
        assert!(tip.1 > tip.0, "deep tip pipe, got {}-{}", tip.0, tip.1);
        super::super::tip_stage::arm_tip_failover();
        assert_eq!(
            assigner.max_gap_fetchers_per_height(),
            1,
            "WAN gap stays single-fetcher when failover armed but soft-retry freeze is off"
        );
        // Soft-retry freeze opens a temporary second tip slot.
        super::super::tip_stage::mark_needed(901);
        super::super::tip_stage::mark_soft_retry(901);
        assert_eq!(
            assigner.max_gap_fetchers_per_height(),
            2,
            "WAN soft-retry must allow tip-height race (covering slot 2)"
        );
        unsafe { std::env::remove_var("BLVM_IBD_GAP_FETCHERS") };
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn w31_wan_gap_retry_covering_tip_sticky_owner_only() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["owner".into(), "other".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.note_tip_owner_assigned("owner");
        mark_peers_ibd_ready(&assigner, &["owner"]);
        assigner.requeue(901, 916, None);
        assert_eq!(
            assigner.get_work("other", 1000),
            None,
            "non-owner must not take WAN gap retry covering tip"
        );
        let work = assigner
            .get_work("owner", 1000)
            .expect("sticky owner takes tip-covering work on WAN gap");
        assert!(
            work.0 <= 901 && work.1 >= 901,
            "owner range must cover tip 901, got {}-{}",
            work.0,
            work.1
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn w51_promote_idempotent_when_deep_claim_already_covers_tip() {
        // Live W50: two in-flight covers → parallel promote steals tenure from each other.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(1000));
        let assigner = ChunkAssigner::new(
            vec![(1000, 1400)],
            vec!["a".into(), "b".into()],
            Arc::clone(&vh),
            1000,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(900);
        assigner.set_peer_scores(&[("a".into(), 9.0), ("b".into(), 8.0)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.note_tip_cover_claim("a", 1001, 1128);
        assert_eq!(assigner.deep_tip_cover_count(1001), 1);
        // Second peer must not overwrite A's deep tenure.
        assigner.promote_tip_walk_in("b", 1001, 1128);
        assert_eq!(assigner.deep_tip_cover_count(1001), 1);
        assert!(
            assigner
                .tip_cover_claims
                .lock()
                .unwrap()
                .iter()
                .any(|(p, s, e)| p == "a" && *s == 1001 && *e == 1128),
            "first deep claim must survive competing promote"
        );
        assert!(
            !assigner
                .tip_cover_claims
                .lock()
                .unwrap()
                .iter()
                .any(|(p, _, _)| p == "b"),
            "competing promote must be a no-op"
        );
    }

    #[test]
    fn w49_tip_walk_in_promotes_instead_of_abort_thrash() {
        // Live WAN: abort-after-body + W28d short preempt → span=32 storms. W49 promotes
        // the walk-in to tip-cover tenure while tip is inside the span.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        let vh = Arc::new(AtomicU64::new(1000));
        let chunks = vec![(1000, 1400)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["bind".into()],
            Arc::clone(&vh),
            1000,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(900);
        assigner.set_peer_scores(&[
            ("owner".into(), 9.0),
            ("ahead".into(), 5.0),
            ("c".into(), 5.0),
            ("d".into(), 5.0),
        ]);
        mark_scored_peers_ibd_ready(&assigner);

        let tip = assigner.get_work("owner", 1000).expect("tip owner");
        assert_eq!(tip.0, 1001);
        assert!(tip.1 >= 1001 + 31, "deep tip pipe, got {:?}", tip);

        let ahead = assigner.get_work("ahead", 1000).expect("ahead");
        assert!(ahead.0 > tip.1, "ahead after tip owner end {:?}", ahead);

        assigner.on_chunk_complete_range("owner", tip.0, tip.1);
        vh.store(ahead.0 + 5, Ordering::Relaxed);
        let need = ahead.0 + 6;
        assert!(ahead.0 <= need && need <= ahead.1);

        assert!(
            !assigner.should_abort_tip_walk_in("ahead", ahead.0, ahead.1),
            "W49: never abort while tip inside walk-in span"
        );
        assert!(
            assigner.healthy_tip_cover_count(need) >= 1,
            "W49: walk-in must be promoted to tip-cover claim"
        );
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("ahead"),
            "promoted walk-in becomes preferred tip owner"
        );
        // Sticky owner must not open a competing tip-covering pipe (the W28d thrash).
        // Far-ahead partitions are OK.
        let again = assigner.get_work("owner", 1000);
        if let Some((s, e)) = again {
            assert!(
                !(s <= need && need <= e && e > s),
                "must not assign competing deep tip pipe under promoted walk-in, got {s}-{e} tip={need}"
            );
        }
        // Tip walked past span → abort leftover ahead.
        vh.store(ahead.1, Ordering::Relaxed);
        assert!(
            assigner.should_abort_tip_walk_in("ahead", ahead.0, ahead.1),
            "W49: abort only after tip walks past span end"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn c1e_tip_contiguous_assign_frontier_stripes_multi_peer() {
        // Peer A: tip..tip+31, Peer B: tip+32..tip+63 → frontier tip+63 (contiguous).
        // Phantom claim tip..tip+127 alone would lie; we only walk contiguous cover.
        let mut inflight: HashMap<String, Vec<(u64, u64)>> = HashMap::new();
        let tip = 300_000u64;
        inflight.insert("a".into(), vec![(tip, tip + 31)]);
        inflight.insert("b".into(), vec![(tip + 32, tip + 63)]);
        let runway_end = tip + 95;
        assert_eq!(
            ChunkAssigner::tip_contiguous_assign_frontier(&inflight, tip, runway_end),
            tip + 63
        );
        // Hole between stripes → stop at first stripe end.
        inflight.insert("c".into(), vec![(tip + 80, tip + 95)]);
        assert_eq!(
            ChunkAssigner::tip_contiguous_assign_frontier(&inflight, tip, runway_end),
            tip + 63,
            "must not jump hole to c's stripe"
        );
        // Tip uncovered → frontier tip-1.
        let empty = HashMap::new();
        assert_eq!(
            ChunkAssigner::tip_contiguous_assign_frontier(&empty, tip, runway_end),
            tip - 1
        );
        // Phantom deep assign without covering tip from next_needed:
        // range starts at tip+10 → not contiguous from tip.
        let mut phantom = HashMap::new();
        phantom.insert("p".into(), vec![(tip + 10, tip + 127)]);
        assert_eq!(
            ChunkAssigner::tip_contiguous_assign_frontier(&phantom, tip, tip + 127),
            tip - 1,
            "assign starting past tip is not runway"
        );
    }

    #[test]
    fn w41_wan_allows_two_fetchers_when_deep_owner_absent() {
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        let vh = Arc::new(AtomicU64::new(900));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007)],
            vec!["pA".into(), "pB".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assert_eq!(
            assigner.max_gap_fetchers_per_height(),
            2,
            "WAN with no deep owner must allow tip race"
        );
        assigner.set_peer_scores(&[("pA".into(), 9.0), ("pB".into(), 1.0)]);
        mark_scored_peers_ibd_ready(&assigner);
        let tip = assigner.get_work("pA", 1000).expect("deep owner");
        assert!(tip.1 > tip.0);
        assert_eq!(
            assigner.max_gap_fetchers_per_height(),
            1,
            "after deep claim, back to single tip pipe"
        );
        // W47: bridge holes alone must NOT reopen tip race (was covering≈2 treadmill).
        // Ahead partitions for pB are fine; tip-height (H,H) micros are not.
        assigner.set_tip_bridge_holes(64);
        assert_eq!(
            assigner.max_gap_fetchers_per_height(),
            1,
            "W47: holes alone must not arm tip race"
        );
        if let Some((s, e)) = assigner.get_work("pB", 1000) {
            assert!(
                !(s == e && s == tip.0),
                "must not (H,H) tip race on holes alone, got {s}-{e}"
            );
            assigner.on_chunk_complete_range("pB", s, e);
        }
        // Soft-retry is real tip distress → one failover micro.
        super::super::tip_stage::mark_needed(tip.0);
        super::super::tip_stage::mark_soft_retry(tip.0);
        assert_eq!(
            assigner.max_gap_fetchers_per_height(),
            2,
            "W47: soft-retry must reopen tip race"
        );
        let failover = assigner.get_work("pB", 1000);
        assert!(failover.is_some(), "second peer tip failover under soft-retry");
        let (s, e) = failover.unwrap();
        assert_eq!(s, e, "failover must be tip-height micro, got {s}-{e}");
        assert_eq!(s, tip.0, "failover races current tip");
        assert!(
            assigner.get_work("pB", 1000).is_none(),
            "must not assign unlimited failover micros"
        );
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn w47_ahead_ok_with_high_holes_frozen_on_tip_distress() {
        // W47: high bridge holes alone must NOT block ahead while feeder>0.
        // W125: holes≥24 + feeder=0 sticky-freezes ahead until holes < 8.
        // Soft-retry still freezes ahead (real tip distress — A6g/W31).
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 943), (944, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["owner".into(), "ahead".into(), "spare".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
        mark_scored_peers_ibd_ready(&assigner);

        let tip = assigner.get_work("owner", 1000);
        assert!(tip.is_some(), "tip owner must get work");
        let (ts, te) = tip.unwrap();
        assert_eq!(ts, 901);
        assert!(te > ts, "deep tip pipeline");

        // C1g: ahead only after tip lands in reorder (not while tip_gap_missing).
        assigner.set_tip_gap_missing(false);
        // C1i: ahead also requires contig runway ≥ min (default 8) — tipfix DNA.
        super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
        assigner.set_tip_bridge_holes(64);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
        let ahead = assigner.get_work("ahead", 1000);
        assert!(
            ahead.is_some(),
            "W47: ahead partition must work with holes=64 when feeder>0"
        );
        let (s, e) = ahead.unwrap();
        assert!(
            s > te,
            "ahead must start after tip owner end, got {s}-{e} tip_end={te}"
        );
        assigner.on_chunk_complete_range("ahead", s, e);

        // W125: arm@24 / clear@8 — holes=16 must NOT freeze; holes=10 stays frozen.
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        assigner.set_tip_bridge_holes(16);
        assert!(
            assigner.wan_allow_multi_peer_ahead(1, 0),
            "W125: holes=16 < arm=24 must allow ahead (W124 over-froze here)"
        );
        assigner.set_tip_bridge_holes(24);
        assert!(
            !assigner.wan_allow_multi_peer_ahead(1, 0),
            "W125: holes≥24 + feeder=0 must freeze ahead"
        );
        assert!(
            assigner.tip_ahead_hole_freeze.load(Ordering::Relaxed),
            "W125: sticky latch armed"
        );
        assigner.set_tip_bridge_holes(10);
        assert!(
            !assigner.wan_allow_multi_peer_ahead(1, 0),
            "W125: holes=10 still frozen (clear only &lt;8; W123 released @12)"
        );
        assigner.set_tip_bridge_holes(7);
        assert!(
            assigner.wan_allow_multi_peer_ahead(1, 8),
            "W125: holes&lt;8 + feeder>0 releases sticky"
        );

        // W181: distress arm must sticky-latch at holes=16 when awaiting≥3s.
        assigner.tip_ahead_hole_freeze.store(false, Ordering::Relaxed);
        assigner
            .tip_ahead_hole_clear_since_ms
            .store(0, Ordering::Relaxed);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        assigner.set_tip_bridge_holes(16);
        super::super::tip_stage::mark_needed(901);
        super::super::tip_stage::test_backdate_awaiting_ms(3_500);
        assert!(
            !assigner.wan_allow_multi_peer_ahead(1, 0),
            "W181: distress arm must sticky-latch at holes=16"
        );
        assert!(
            assigner.tip_ahead_hole_freeze.load(Ordering::Relaxed),
            "W181: sticky latch armed under distress"
        );
        assigner.set_tip_bridge_holes(7);
        assert!(
            assigner.wan_allow_multi_peer_ahead(1, 8),
            "W143: holes&lt;8 + feeder>0 releases distress sticky"
        );

        // W183: feeder-empty clear is debounced — brief holes&lt;8 must not reopen W35.
        assigner.tip_ahead_hole_freeze.store(true, Ordering::Relaxed);
        assigner
            .tip_ahead_hole_clear_since_ms
            .store(0, Ordering::Relaxed);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        assigner.set_tip_bridge_holes(7);
        assert!(
            !assigner.wan_allow_multi_peer_ahead(1, 0),
            "W183: holes&lt;8 + feeder=0 must not clear sticky on first poll"
        );
        assert!(
            assigner.tip_ahead_hole_freeze.load(Ordering::Relaxed),
            "W183: clear countdown armed, freeze still latched"
        );
        assigner.set_tip_bridge_holes(16);
        let _ = assigner.wan_allow_multi_peer_ahead(1, 0);
        assert_eq!(
            assigner.tip_ahead_hole_clear_since_ms.load(Ordering::Relaxed),
            0,
            "W183: holes back mid-band cancels clear countdown"
        );
        assert!(assigner.tip_ahead_hole_freeze.load(Ordering::Relaxed));

        // Soft-retry: no new far-ahead past tip frontier (main-queue behind-tip OK).
        assigner.set_tip_bridge_holes(0);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(901);
        super::super::tip_stage::mark_soft_retry(901);
        assert!(
            !assigner.wan_allow_multi_peer_ahead(1, 8),
            "soft-retry must freeze multi-peer ahead"
        );
        let blocked = assigner.get_work("spare", 1000);
        if let Some((bs, be)) = blocked {
            assert!(
                !(bs > te),
                "during soft-retry must not assign far ahead, got {bs}-{be} tip_end={te}"
            );
        }
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        super::super::tip_stage::clear_tip_failover();
        super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    }


    #[test]
    fn w102b_narrow_allows_ahead_when_awaiting_healthy_cover_low_holes() {
        // True-WAN 400→500 (20260731T154656Z): feeder=0 ~79% of samples with tip
        // covering≥1 is the *normal* single-owner crawl. Old W102b hard-denied ahead
        // on awaiting≥3s ∧ feeder=0 alone → ahead_partition 21 vs tip_owner 562.
        // Hole-band (W181) + C1g still freeze STREAM storms; awaiting alone must not.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 943), (944, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["owner".into(), "ahead".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.tip_ahead_hole_freeze.store(false, Ordering::Relaxed);
        assigner.set_tip_gap_missing(false);
        assigner.set_tip_bridge_holes(0);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(901);
        super::super::tip_stage::test_backdate_awaiting_ms(3_500);
        assert!(
            assigner.wan_allow_multi_peer_ahead(1, 0),
            "W102b narrow: awaiting≥3s + feeder=0 + holes=0 + covering≥1 must allow ahead"
        );
        assert!(
            !assigner.tip_ahead_hole_freeze.load(Ordering::Relaxed),
            "W102b narrow: low holes must not latch hole-band freeze"
        );
        // W181 still armed when holes enter distress under the same awaiting clock.
        assigner.set_tip_bridge_holes(16);
        assert!(
            !assigner.wan_allow_multi_peer_ahead(1, 0),
            "W181: awaiting≥3s + holes=16 must still freeze ahead"
        );
        super::super::tip_stage::mark_needed(0);
        super::super::tip_stage::clear_tip_failover();
    }


    #[test]
    fn w127_covering0_allows_floor_when_mid_pool_fail_cooled() {
        // Live W126b @337k: mute CAP cooled mid peers; W95 ignore_cooldown still treated
        // them as alternatives → floor open-slot denied → covering=0 OPEN_STALL.
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 943), (944, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["mid".into(), "floor".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[("mid".into(), 0.20), ("floor".into(), 0.10)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        assigner.tip_owner_open.store(true, Ordering::Relaxed);
        assigner.mark_tip_owner_fail_cooldown("mid", 30);

        let g = assigner.in_flight_per_peer.lock().unwrap();
        assert!(
            !assigner.peer_may_take_tip_owner("mid", &g, 0),
            "cooled mid must still be denied"
        );
        assert!(
            assigner.peer_may_take_tip_owner("floor", &g, 0),
            "W127: covering=0 must allow floor when only mid is fail-cooled"
        );
        // covering>0 keeps W95: cooled mid still blocks floor lottery.
        assert!(
            !assigner.peer_may_take_tip_owner("floor", &g, 1),
            "W95: covering>0 must still refuse floor while cooled mid exists"
        );
        drop(g);
    }

    #[test]
    fn w128_covering0_clears_mid_cooldown_keeps_floor_gate() {
        // Tipfix DNA: W95 counts cooled mid when covering>0; covering=0 MID_CLEAR
        // uncools mid+; floor stays refused once live mid exists; floor cooldown retained.
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 943), (944, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["mid".into(), "floor".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[("mid".into(), 0.25), ("floor".into(), 0.10)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        assigner.tip_owner_open.store(true, Ordering::Relaxed);
        assigner.mark_tip_owner_fail_cooldown("mid", 120);
        assert!(assigner.tip_owner_in_fail_cooldown("mid"));
        {
            let g = assigner.in_flight_per_peer.lock().unwrap();
            assert!(
                !assigner.peer_may_take_tip_owner("mid", &g, 0),
                "cooled mid denied before clear"
            );
            assert!(
                !assigner.peer_may_take_tip_owner("floor", &g, 1),
                "W95/W128: floor denied while cooled mid still counts as alternative"
            );
            // W127: covering=0 allows floor while mid is cooled — MID_CLEAR is for when
            // we *want* mid back, not to keep floor locked out forever.
            assert!(
                assigner.peer_may_take_tip_owner("floor", &g, 0),
                "covering=0 floor ok while mid cooled (W127)"
            );
        }
        // covering=0 MID_CLEAR — mid re-arms; floor refused once live mid exists.
        assigner.maybe_clear_mid_plus_fail_cooldowns_covering0(901);
        assert!(
            !assigner.tip_owner_in_fail_cooldown("mid"),
            "W128: mid re-arms after mid+ cooldown clear"
        );
        {
            let g = assigner.in_flight_per_peer.lock().unwrap();
            assert!(
                assigner.peer_may_take_tip_owner("mid", &g, 0),
                "W128: mid re-arms after mid+ cooldown clear"
            );
            assert!(
                !assigner.peer_may_take_tip_owner("floor", &g, 0),
                "W128: floor still refused once live mid exists"
            );
        }
        // mid_clear must not wipe a floor cooldown.
        assigner.mark_tip_owner_fail_cooldown("floor", 120);
        assigner.mark_tip_owner_fail_cooldown("mid", 120);
        assigner.maybe_clear_mid_plus_fail_cooldowns_covering0(901);
        assert!(
            assigner.tip_owner_in_fail_cooldown("floor"),
            "floor cooldown retained"
        );
        assert!(
            !assigner.tip_owner_in_fail_cooldown("mid"),
            "W128: mid re-arms after mid+ cooldown clear"
        );
    }


    #[test]
    fn w126_covering0_pin_prefers_idle_over_ahead_busy() {
        // Live W125 @326975: TIP_PIN elected top_w mid W35 ahead → covering=0 for 16s.
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 943), (944, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["busy".into(), "idle".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[("busy".into(), 9.0), ("idle".into(), 8.0)]);
        mark_scored_peers_ibd_ready(&assigner);
        // busy holds ahead-only in-flight past tip.
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.insert("busy".into(), vec![(950, 981)]);
        }
        let tip = 901u64;
        let inflight = assigner.in_flight_per_peer.lock().unwrap().clone();
        assert!(
            ChunkAssigner::peer_inflight_ahead_only_map(&inflight, "busy", tip),
            "busy must be ahead-only"
        );
        assert!(
            assigner.peer_has_flight_capacity("idle", &inflight),
            "idle must have capacity"
        );
        let pin = assigner
            .best_covering0_tip_pin_candidate(tip)
            .expect("pin candidate");
        assert_eq!(pin, "idle", "W126: must prefer idle over ahead-busy top score");

        // W126a: peer_may_take_tip_owner must not deadlock while caller holds in_flight.
        assigner.tip_owner_open.store(true, Ordering::Relaxed);
        assigner.set_tip_gap_missing(true);
        let g = assigner.in_flight_per_peer.lock().unwrap();
        let _ = assigner.peer_may_take_tip_owner("idle", &g, 0);
        drop(g);
    }

    #[test]
    fn a6g_ahead_ok_with_gap_missing_low_holes() {
        // C1g: tip-band ahead requires tip in reorder (`tip_gap_missing=false`). Opening
        // stripes while tip empty caused TIP_HOLE_AHEAD (C1f). Soft-retry still freezes.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0); // clear any leftover soft-retry latch
        let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead", "spare"]);
        assigner.set_peer_scores(&[
            ("owner".into(), 9.0),
            ("ahead".into(), 8.0),
            ("spare".into(), 7.0),
        ]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        assigner.set_tip_bridge_holes(0);
        // W61: gap_missing+feeder==0 freezes ahead; simulate healthy pipe runway.
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);

        let tip = assigner.get_work("owner", 4096);
        assert!(tip.is_some(), "tip owner must get work");
        let (ts, te) = tip.unwrap();
        assert_eq!(ts, 901);

        // While tip missing: C1g freezes past-tip stripes (tip-height race OK).
        if let Some((s, e)) = assigner.get_work("ahead", 4096) {
            assert!(
                s == 901 && e == 901,
                "C1g: only tip-height race while tip_gap_missing, got {s}-{e}"
            );
            // Free the race micro so tip-band ahead can arm after tip lands.
            assigner.on_chunk_complete_range("ahead", s, e);
        }
        assigner.set_tip_gap_missing(false);
        super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
        let ahead = assigner.get_work("ahead", 4096);
        assert!(
            ahead.is_some(),
            "multi-peer tip-band ahead after tip lands in reorder"
        );
        let (s, e) = ahead.unwrap();
        assert!(
            s > te,
            "ahead must start after tip frontier, got {s}-{e} tip_end={te}"
        );

        // Soft-retry: freeze multi-peer ahead; tip-height failover race is allowed (W31).
        super::super::tip_stage::mark_needed(901);
        super::super::tip_stage::mark_soft_retry(901);
        assert!(super::super::tip_stage::tip_ahead_frozen_for_soft_retry());
        let raced = assigner.get_work("spare", 4096);
        match raced {
            None => {}
            Some((s, e)) => {
                assert_eq!(
                    (s, e),
                    (901, 901),
                    "during soft-retry only tip-height failover is allowed, got {s}-{e}"
                );
            }
        }
        // A third peer must not get a far ahead partition while freeze is latched.
        let blocked = assigner.get_work("ahead", 4096);
        // "ahead" already holds an ahead range from before soft-retry — may be at cap.
        // Use a fresh peer name that only appears now… spare already used. Check via
        // wan_allow directly is enough: any new partition past tip frontier is forbidden.
        if let Some((s, e)) = blocked {
            assert!(
                s <= 901 && e <= te,
                "must not assign far ahead during soft-retry, got {s}-{e} tip_end={te}"
            );
        }

        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        super::super::tip_stage::clear_tip_failover();
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    }

    #[test]
    fn w98_find_inflight_deep_skips_shallow_remnant() {
        let mut inflight = HashMap::new();
        // Live W97: tip=312048 covered by ahead remnant 312018-312049 (remain=2).
        inflight.insert("shallow".into(), vec![(312_018u64, 312_049u64)]);
        assert!(
            ChunkAssigner::find_inflight_deep_covering(&inflight, 312_048).is_none(),
            "W98: shallow remain=2 must not promote-as-deep"
        );
        inflight.insert("deep".into(), vec![(312_048u64, 312_175u64)]);
        let found = ChunkAssigner::find_inflight_deep_covering(&inflight, 312_048);
        assert_eq!(
            found.as_ref().map(|(p, _, _)| p.as_str()),
            Some("deep"),
            "W98: prefer substantial tip runway"
        );
    }

    #[test]
    fn w113_empty_tip_open_slot_prefers_tip_streamer() {
        // Live W112b @331209: tip_owner_open elected score=0.100 while ready=62
        // included tip STREAM heroes → empty mute lottery rate-fail 33.5 vs 35.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        let vh = Arc::new(AtomicU64::new(331_208));
        let assigner = ChunkAssigner::new(
            vec![(331_209, 331_500)],
            vec!["floor".into(), "hero".into()],
            Arc::clone(&vh),
            1,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(0);
        // Both floored (live mid-chain score collapse) — W95 mid-gate cannot help;
        // tip-STREAM history must break the lottery.
        assigner.set_peer_scores(&[("floor".into(), 0.100), ("hero".into(), 0.100)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        assigner.tip_owner_open.store(true, Ordering::Relaxed);
        assigner.note_wan_tip_stream("hero");
        assert!(
            assigner.empty_tip_owner_prefer_streamer(),
            "W113: proven tip streamer ready"
        );
        assert!(
            assigner.get_work("floor", 1000).is_none(),
            "W113: non-streamer must not deep-own empty tip while streamer ready"
        );
        let hero = assigner.get_work("hero", 1000);
        assert!(hero.is_some(), "W113: tip streamer takes deep owner");
        let (s, e) = hero.unwrap();
        assert_eq!(s, 331_209);
        assert!(e > s, "deep tip pipe, got {s}-{e}");
        assigner.tip_owner_open.store(false, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(0);
    }

    #[test]
    fn w111_mute_cooldown_blocks_walk_promote_resticky() {
        // Live W110 @326324: mute CAP → TIP_FAILOVER armed, then same-ms
        // TIP_WALK_PROMOTE re-stickied the mute-failed peer from residual in-flight.
        let vh = Arc::new(AtomicU64::new(326_323));
        let assigner = ChunkAssigner::new(
            vec![(326_000, 327_000)],
            vec!["mute".into(), "other".into()],
            Arc::clone(&vh),
            1,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(0);
        assigner.set_tip_gap_missing(true);
        // Cooldown skips when ≤1 IBD-ready peer — both must be ready so mute stays cooled.
        assigner.set_ibd_ready_peers(HashSet::from(["mute".into(), "other".into()]));
        assigner.mark_tip_owner_fail_cooldown("mute", 5);
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.insert("mute".into(), vec![(326_316, 326_347)]);
        }
        assigner.promote_tip_walk_in("mute", 326_316, 326_347);
        assert_eq!(
            assigner.preferred_tip_owner(),
            None,
            "W111: mute-cooled peer must not become preferred via walk-promote"
        );
        assert_eq!(
            assigner.deep_tip_cover_count(326_324),
            0,
            "W111: no deep tip claim from mute-cooled walk-promote"
        );
        assert!(
            assigner.should_abort_tip_walk_in("mute", 326_316, 326_347),
            "W111: cooldown walk-in aborts rather than re-sticky"
        );
    }

    #[test]
    fn w65_shallow_walk_promote_allows_deep_tip_rearm() {
        // Live genesis tip=218: TIP_WALK_PROMOTE ahead 193-224 → claim 218-224 (depth 7)
        // plus (H,H) failover covering=2/2 held tip tenure through 3× soft-retry (~40s).
        // Deep owner 218-345 then streamed tip immediately. Shallow remnants must not
        // count as deep tip cover / block claim_overlap.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        let vh = Arc::new(AtomicU64::new(217));
        let assigner = ChunkAssigner::new(
            vec![(1, 400)],
            vec!["walk".into(), "failover".into(), "owner".into()],
            Arc::clone(&vh),
            1,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(0);
        assigner.set_peer_scores(&[
            ("owner".into(), 9.0),
            ("walk".into(), 5.0),
            ("failover".into(), 4.0),
        ]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);

        // Ahead walk-in still in-flight; tip has walked into it.
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.insert("walk".into(), vec![(193, 224)]);
        }
        assigner.promote_tip_walk_in("walk", 193, 224);
        assert_eq!(
            assigner.deep_tip_cover_count(218),
            0,
            "W65: shallow promote remnant 218-224 must not count as deep cover"
        );
        assert!(
            assigner.healthy_tip_cover_count(218) >= 1,
            "promote still registers a tip-cover claim (GetData keep)"
        );
        // Failover micros as in live covering=2/2.
        assigner.note_tip_cover_claim("failover", 218, 218);
        super::super::tip_stage::arm_tip_failover();
        super::super::tip_stage::mark_needed(218);
        super::super::tip_stage::mark_soft_retry(218);

        let work = assigner.get_work("owner", 4096);
        assert!(work.is_some(), "deep owner must re-arm over shallow walk-promote");
        let (s, e) = work.unwrap();
        assert_eq!(s, 218);
        assert!(
            e >= 218 + 63,
            "must be substantial deep pipe not (H,H)/shallow, got {s}-{e}"
        );
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        super::super::tip_stage::clear_tip_failover();
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    }

    #[test]
    fn w30_wan_gap_ignores_failover_micro_for_deep_owner() {
        // covering=2 from (H,H) failovers must not block a new deep tip owner on WAN gap.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007), (1008, 1071), (1072, 1135)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["pA".into(), "pB".into(), "pC".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("pA".into(), 9.0),
            ("pB".into(), 8.0),
            ("pC".into(), 7.0),
        ]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        super::super::tip_stage::arm_tip_failover();
        // Simulate two stuck (H,H) failover claims at tip 901.
        assigner.note_tip_cover_claim("pB", 901, 901);
        assigner.note_tip_cover_claim("pC", 901, 901);
        assert_eq!(assigner.healthy_tip_cover_count(901), 2);
        assert_eq!(assigner.deep_tip_cover_count(901), 0);

        let work = assigner.get_work("pA", 1000);
        assert!(work.is_some(), "deep owner must re-arm despite micro failover claims");
        let (s, e) = work.unwrap();
        assert_eq!(s, 901);
        assert!(e > s, "must be deep pipeline not (H,H), got {s}-{e}");
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn gap_preempt_skips_when_next_needed_at_chunk_start() {
        // Tip uncovered → tip owner bulk; second peer gets non-overlapping ahead partition.
        // LOCAL_AHEAD (body tip past next): empty ibd_ready must not block tip owner.
        let vh = Arc::new(AtomicU64::new(505_153));
        let chunks = vec![(505_153, 505_184), (505_185, 505_216)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["pA".into(), "pB".into()],
            Arc::clone(&vh),
            505_153,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(600_000);
        assert_eq!(
            assigner.get_work("pA", 1000),
            Some((505_154, 505_169)),
            "pA tip owner from next_needed"
        );
        let second = assigner.get_work("pB", 1000);
        assert_eq!(
            second,
            Some((505_170, 505_184)),
            "pB ahead partition after tip owner, not overlapping tip race"
        );
    }

#[test]
    fn w130_hole_freeze_opens_weak_sticky_keeps_ahead_frozen() {
        // RECONSTRUCTED from blvm_node-0faf3b9b3ecfa01e assert strings (2026-07-28).
        // Full body was NOT present in agent-transcript StrReplace blobs — only fn name
        // anchors (512e3125 L8799) and production DNA tip_owner_credible/nudge_weak_sticky.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 943), (944, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["floor".into(), "mid".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[("floor".into(), 0.10), ("mid".into(), 0.25)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.note_tip_owner_assigned("floor");
        assigner.set_tip_gap_missing(false);
        // W125/W130: holes≥24 + feeder=0 must freeze ahead
        unsafe { std::env::set_var("BLVM_IBD_WEAK_STICKY_OPEN_MS", "0"); }
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        assigner.set_tip_bridge_holes(24);
        assert!(
            !assigner.wan_allow_multi_peer_ahead(1, 0),
            "W125/W130: holes≥24 + feeder=0 must freeze ahead"
        );
        assert!(assigner.tip_ahead_hole_freeze.load(Ordering::Relaxed));
        // Floor sticky is not credible under hole-freeze → open tip slot; ahead stays frozen.
        assert!(
            assigner.preferred_tip_owner().is_none(),
            "W130: floor sticky cleared during hole-freeze"
        );
        assert!(
            assigner.tip_owner_open.load(Ordering::Relaxed),
            "W130: tip slot open for mid+/STREAM re-arm"
        );
        assert!(
            !assigner.wan_allow_multi_peer_ahead(1, 0),
            "W130: ahead must stay frozen under holes≥24"
        );
        unsafe { std::env::remove_var("BLVM_IBD_WEAK_STICKY_OPEN_MS"); }
        super::super::tip_stage::mark_needed(0);
        super::super::tip_stage::clear_tip_failover();
    }


#[test]
    fn w132_weak_sticky_open_debounced_under_hole_freeze() {
        // RECONSTRUCTED from binary asserts: "W132: first freeze sample must not clear
        // sticky (15s debounce)"; tip_owner_open false; wan_allow false.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 943), (944, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["floor".into(), "mid".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[("floor".into(), 0.10), ("mid".into(), 0.25)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.note_tip_owner_assigned("floor");
        assigner.tip_owner_open.store(false, Ordering::Relaxed);
        unsafe { std::env::set_var("BLVM_IBD_WEAK_STICKY_OPEN_MS", "15000"); }
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        assigner.set_tip_bridge_holes(24);
        // First sample arms freeze + starts weak-sticky debounce — must NOT clear yet.
        assert!(
            !assigner.wan_allow_multi_peer_ahead(1, 0),
            "assertion failed: !assigner.wan_allow_multi_peer_ahead(1, 0)"
        );
        assert!(
            assigner.preferred_tip_owner().as_deref() == Some("floor"),
            "W132: first freeze sample must not clear sticky (15s debounce)"
        );
        assert!(
            !assigner.tip_owner_open.load(Ordering::Relaxed),
            "assertion failed: !assigner.tip_owner_open.load(Ordering::Relaxed)"
        );
        unsafe { std::env::remove_var("BLVM_IBD_WEAK_STICKY_OPEN_MS"); }
        super::super::tip_stage::mark_needed(0);
        super::super::tip_stage::clear_tip_failover();
    }


#[test]
    fn w138_tip_pin_prefers_mid_over_idle_floor() {
        // RECONSTRUCTED from binary asserts + TIP_PIN_PREFER_MID DNA (transcript L8978).
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 943), (944, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["floor".into(), "mid".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[("floor".into(), 0.10), ("mid".into(), 0.25)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        // Mid holds ahead-only; floor is idle — covering=0 pin must prefer mid and release ahead.
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.insert("mid".into(), vec![(950, 981)]);
        }
        // preferred empty → nudge/TIP_PIN path
        assert!(assigner.preferred_tip_owner().is_none());
        assert!(
            assigner.nudge_wan_tip_owner(),
            "assertion failed: assigner.nudge_wan_tip_owner()"
        );
        let pref = assigner.preferred_tip_owner();
        assert_eq!(
            pref.as_deref(),
            Some("mid"),
            "W138: covering=0 must prefer mid+ over idle floor"
        );
        let inflight = assigner.in_flight_per_peer.lock().unwrap().clone();
        assert!(
            !ChunkAssigner::peer_inflight_ahead_only_map(&inflight, "mid", 901)
                || assigner.peer_has_flight_capacity("mid", &inflight),
            "W138: mid ahead must be released so tip can arm"
        );
        super::super::tip_stage::clear_tip_failover();
    }


#[test]
    fn w153_holey_tip_triple_race_at_12s() {
        // RECONSTRUCTED from binary asserts near w112. Dens-era empty_tip_triple may have
        // allowed covering=3 with BRIDGE_PENDING>0 (holey); CURRENT empty_tip_triple_race
        // returns false when pending>0 — this test documents dens intent / may need DNA.
        let _tip_atomics = super::super::tip_stage::test_tip_atomics_lock();
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::memory::BRIDGE_PENDING_COUNT.store(32, Ordering::Relaxed); // holey
        super::super::IBD_TIP_BRIDGE_HOLES.store(8, Ordering::Relaxed);
        let vh = Arc::new(AtomicU64::new(323_779));
        let assigner = ChunkAssigner::new(
            vec![(323_780, 324_000)],
            vec!["pA".into(), "pB".into(), "pC".into(), "pD".into()],
            Arc::clone(&vh),
            1,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(0);
        assigner.set_peer_scores(&[
            ("pA".into(), 1.0),
            ("pB".into(), 0.9),
            ("pC".into(), 0.8),
            ("pD".into(), 0.7),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "pA".into(),
            "pB".into(),
            "pC".into(),
            "pD".into(),
        ]));
        assigner.set_tip_gap_missing(true);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(323_780);
        super::super::tip_stage::test_backdate_awaiting_ms(6_000);
        assert_eq!(
            assigner.max_gap_fetchers_per_height(),
            2,
            "W153: awaiting=6s < 12s — keep covering=2 on holey"
        );
        super::super::tip_stage::test_backdate_awaiting_ms(13_000);
        assert_eq!(
            assigner.max_gap_fetchers_per_height(),
            3,
            "W153: holey pending>0 + awaiting≥12s → covering=3"
        );
        let owner = assigner.get_work("pA", 1000);
        assert!(owner.is_some(), "deep tip owner");
        assert_eq!(assigner.get_work("pB", 1000), Some((323_780, 323_780)));
        assert_eq!(
            assigner.get_work("pC", 1000),
            Some((323_780, 323_780)),
            "W153: second failover under covering=3"
        );
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::mark_needed(0);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    }


    #[test]
    fn w180_mute_distress_refuses_floor_and_clears_mid_cooldown() {
        // Tipfix DNA (binary asserts): mute arms failover + cools mid; MID_CLEAR then
        // uncools mid+ so mid can take failover; distress race still refuses floor.
        let _tip_atomics = super::super::tip_stage::test_tip_atomics_lock();
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 943), (944, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["mid".into(), "floor".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[("mid".into(), 0.25), ("floor".into(), 0.10)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        assigner.note_tip_owner_assigned("mid");
        assigner.note_tip_owner_failed_mute("mid");
        assert!(
            super::super::tip_stage::tip_failover_armed(),
            "assertion failed: tip_failover_armed()"
        );
        assert!(
            assigner.tip_owner_in_fail_cooldown("mid"),
            "mute CAP must cool mid before MID_CLEAR"
        );
        // Covering=0 MID_CLEAR path — uncool mid+ so mid can take failover.
        assigner.maybe_clear_mid_plus_fail_cooldowns_covering0(901);
        assert!(
            !assigner.tip_owner_in_fail_cooldown("mid"),
            "W180: mute CAP must MID_CLEAR so mid can take failover"
        );
        super::super::tip_stage::mark_needed(901);
        super::super::tip_stage::mark_soft_retry(901);
        assert!(
            super::super::tip_stage::tip_ahead_frozen_for_soft_retry(),
            "assertion failed: tip_ahead_frozen_for_soft_retry()"
        );
        let g = assigner.in_flight_per_peer.lock().unwrap();
        assert!(
            !assigner.peer_may_take_tip_owner("floor", &g, 1),
            "W180: distress race must refuse floor while mid+ exists"
        );
        drop(g);
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::mark_needed(0);
    }

    #[test]
    fn c1j_aborts_past_tip_while_tip_missing() {
        super::super::tip_stage::clear_tip_failover();
        let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead"]);
        assigner.set_tip_gap_missing(true);
        assert!(
            assigner.should_abort_tip_walk_in("ahead", 933, 964),
            "C1j: must abort tip+32.. while tip missing"
        );
        assigner.set_tip_gap_missing(false);
        assert!(
            !assigner.should_abort_tip_walk_in("ahead", 933, 964),
            "C1j: must not abort ahead span when tip present and tip below span"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn synth_bulk_clears_tip_cover_claim_on_complete() {
        let _guard = SYNTH_BULK_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("BLVM_IBD_SYNTH_WAN", "1");
            std::env::set_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT", "1");
            std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
            std::env::remove_var("BLVM_IBD_SYNTH_WAN_FORCE_PEERS");
        }
        assert!(super::super::synthetic_wan::bulk_local_disk_stream());
        let vh = Arc::new(AtomicU64::new(300_300));
        let assigner = ChunkAssigner::new(
            vec![(300_288, 300_351)],
            vec!["local-disk".into()],
            Arc::clone(&vh),
            300_288,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.note_tip_cover_claim("local-disk", 300_288, 300_351);
        assigner.on_chunk_complete_range("local-disk", 300_288, 300_351);
        assert_eq!(
            assigner.healthy_tip_cover_count(300_300),
            0,
            "synth must clear tip-cover claim on complete (keep-claim muted tip-owner)"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_SYNTH_WAN");
            std::env::remove_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT");
        }
    }


#[test]
    fn synth_bulk_dedup_blocks_same_span_tip_owner_reassign() {
        // H6: DEDUP gate + get_work must not W28c-reassign tip after GAP_STREAM while
        // validation lags (in_flight/claims already cleared on complete).
        let _guard = SYNTH_BULK_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("BLVM_IBD_SYNTH_WAN", "1");
            std::env::set_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT", "1");
            std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
            std::env::remove_var("BLVM_IBD_SYNTH_WAN_FORCE_PEERS");
            std::env::set_var("BLVM_IBD_SYNTH_DEDUP_REARM_MS", "60000");
            std::env::set_var("BLVM_IBD_GAP_PREEMPT_BATCH", "128");
        }
        assert!(super::super::synthetic_wan::bulk_local_disk_stream());
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
        let vh = Arc::new(AtomicU64::new(300_287));
        let assigner = ChunkAssigner::new(
            vec![(300_288, 300_351), (300_352, 300_415)],
            vec!["local-disk".into()],
            Arc::clone(&vh),
            300_288,
            true,
        );
        assigner.mark_bootstrap_complete();
        // Match live synth short: bodies far above tip, pin creates crawl gate above band.
        assigner.set_confirmed_body_height_at_start(503_656);
        assigner.set_wan_body_tip(400_000);
        assigner.set_header_tip(400_000);
        assigner.set_tip_gap_missing(true);
        assigner.set_peer_scores(&[("local-disk".into(), 1.0)]);
        assert!(
            !assigner.synth_tip_owner_blocked_by_dedup(300_288),
            "DEDUP=0 must not block first tip-owner"
        );
        let first = assigner.get_work("local-disk", 1000);
        assert!(
            first.is_some_and(|(s, e)| s == 300_288 && e >= 300_300),
            "initial tip-owner assign, got {first:?}"
        );
        let (fs, fe) = first.unwrap();
        assigner.on_chunk_complete_range("local-disk", fs, fe);
        // Simulate GAP_STREAM having delivered tip (and more) while validation lags.
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(300_351, Ordering::Relaxed);
        assert!(
            assigner.synth_tip_owner_blocked_by_dedup(300_288),
            "DEDUP past tip must block tip-owner re-arm"
        );
        let second = assigner.get_work("local-disk", 1000);
        assert!(
            second.map(|(s, _)| s != 300_288).unwrap_or(true),
            "H6: must not reassign tip-covering span after DEDUP, got {second:?}"
        );
        // Validation caught up — tip-owner for next height is allowed.
        vh.store(300_351, Ordering::Relaxed);
        assigner.synth_tip_dedup_block_since_ms.store(0, Ordering::Relaxed);
        assert!(
            !assigner.synth_tip_owner_blocked_by_dedup(300_352),
            "DEDUP below next tip must allow"
        );
        let third = assigner.get_work("local-disk", 1000);
        assert!(
            third.is_some_and(|(s, _)| s == 300_352),
            "after tip advance, next tip-owner span assigns, got {third:?}"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_SYNTH_WAN");
            std::env::remove_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT");
            std::env::remove_var("BLVM_IBD_SYNTH_DEDUP_REARM_MS");
            std::env::remove_var("BLVM_IBD_GAP_PREEMPT_BATCH");
            super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
        }
    }

    // Shared across synth-bulk env tests (parallel cargo test races otherwise).
    static SYNTH_BULK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn synth_bulk_obsolete_does_not_tip_owner_open() {
        // obsolete/behind-tip must clear sticky without TIP_OWNER_OPEN under synth bulk.
        let _guard = SYNTH_BULK_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("BLVM_IBD_SYNTH_WAN", "1");
            std::env::set_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT", "1");
            std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
            std::env::remove_var("BLVM_IBD_SYNTH_WAN_FORCE_PEERS");
        }
        assert!(super::super::synthetic_wan::bulk_local_disk_stream());
        let vh = Arc::new(AtomicU64::new(505_200));
        let assigner = ChunkAssigner::new(
            vec![(505_153, 505_184)],
            vec!["local-disk".into()],
            Arc::clone(&vh),
            505_153,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.note_tip_owner_assigned("local-disk");
        assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("local-disk"));
        assigner.on_chunk_complete_range("local-disk", 505_153, 505_184);
        assert!(assigner.preferred_tip_owner().is_none());
        assert!(
            !assigner.tip_owner_open.load(Ordering::Relaxed),
            "synth bulk must not TIP_OWNER_OPEN after obsolete complete"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_SYNTH_WAN");
            std::env::remove_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT");
        }
    }




    #[test]
    fn w40_local_tip_hole_owner_at_chunk_start() {
        // Soft-resume: next_needed == chunk start, tip missing — must still deep-own tip.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        let vh = Arc::new(AtomicU64::new(1000));
        let chunks = vec![(1001, 1032), (1033, 1064)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["pA".into(), "pB".into()],
            Arc::clone(&vh),
            1001,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(2000); // LOCAL_AHEAD (not WAN gap)
        assigner.set_tip_gap_missing(true);
        assigner.set_peer_scores(&[("pA".into(), 9.0), ("pB".into(), 1.0)]);
        mark_scored_peers_ibd_ready(&assigner);
        let work = assigner.get_work("pA", 1000);
        assert!(work.is_some(), "LOCAL tip-hole owner must assign at chunk start");
        let (s, e) = work.unwrap();
        assert_eq!(s, 1001);
        assert!(
            e >= s + 15,
            "deep tip pipe under local tip hole, got {s}-{e}"
        );
        // Entirely-behind main-queue work must not be handed out while tip missing.
        // Advance index past tip chunk by completing owner; pB must not get a behind span.
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    #[serial]
    fn wan_tip_gap_preempt_bulk_pipeline() {
        // W28b/W32: past body tip → contiguous tip-owner bulk (up to 128), not chunk-map clips.
        // Claim-frontier dens KEEP: second peer ahead after owner end.
        super::super::tip_stage::test_reset_tip_stage();
        let vh = Arc::new(AtomicU64::new(698_999));
        let chunks = vec![(698_953, 698_984), (698_985, 699_016)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["pA".into(), "pB".into()],
            Arc::clone(&vh),
            698_953,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(695_359);
        assigner.set_peer_scores(&[("pA".into(), 9.0), ("pB".into(), 1.0)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(false);
        super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
        let work = assigner.get_work("pA", 1000);
        assert!(work.is_some(), "WAN tip owner must assign");
        let (s, e) = work.unwrap();
        assert_eq!(s, 699_000);
        assert!(
            e - s + 1 >= 64,
            "W32: WAN owner must pipeline deeply across chunk map, got {s}-{e}"
        );
        let second = assigner.get_work("pB", 1000);
        assert!(
            second.is_some(),
            "second peer should get ahead partition or main-queue work"
        );
        let (s2, e2) = second.unwrap();
        assert!(
            s2 > e,
            "ahead partition must start after tip owner end, got {s2}-{e2}"
        );
        super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
        super::super::tip_stage::test_reset_tip_stage();
    }

    #[test]
    fn gap_preempt_bulk_when_peer_stuck_mid_chunk() {
        // Tip fill when next_needed is last height of containing chunk → extend into next.
        let vh = Arc::new(AtomicU64::new(505_183));
        let chunks = vec![(505_153, 505_184), (505_185, 505_216)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["pA".into(), "pB".into()],
            Arc::clone(&vh),
            505_153,
            true,
        );
        assigner.mark_bootstrap_complete();
        assert_eq!(
            assigner.get_work("pA", 1000),
            Some((505_184, 505_199)),
            "tip owner extends into next chunk (not (H,H))"
        );
        let second = assigner.get_work("pB", 1000);
        assert!(second.is_some());
        let (s, _e) = second.unwrap();
        assert!(
            s >= 505_200,
            "second peer ahead of tip owner, got start={s}"
        );
    }

    #[test]
    fn gap_preempt_caps_fan_out_to_max_gap_fetchers() {
        // Cap at 2 tip owners for this test.
        unsafe { std::env::set_var("BLVM_IBD_GAP_FETCHERS", "2") };
        let vh = Arc::new(AtomicU64::new(100));
        let chunks = vec![(80, 200), (201, 250), (251, 300)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["pA".into(), "pB".into(), "pC".into()],
            Arc::clone(&vh),
            80,
            true,
        );
        assigner.mark_bootstrap_complete();
        assert_eq!(
            assigner.get_work("pA", 1000),
            Some((101, 116)),
            "first peer tip-fills"
        );
        let _ = assigner.get_work("pB", 1000);
        let third = assigner.get_work("pC", 1000);
        assert!(third.is_some());
        let (s, e) = third.unwrap();
        // With default max=1 we'd never have 2 tip owners; with env=2, pB may cover tip.
        // Either way pC must not also cover next_needed=101 once two covering ranges exist,
        // OR if pB took ahead partition, pC still shouldn't duplicate tip owner range.
        assert!(
            s > 116 || s == 80,
            "third peer should be ahead partition or main queue, got {s}-{e}"
        );
        unsafe { std::env::remove_var("BLVM_IBD_GAP_FETCHERS") };
    }

    #[test]
    fn gap_preempt_bulk_range_when_mid_chunk_has_runway() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(100));
        let chunks = vec![(80, 200), (201, 250)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["pA".into(), "pB".into()],
            Arc::clone(&vh),
            80,
            true,
        );
        assigner.mark_bootstrap_complete();
        // pA already owns tip-covering range (80-200).
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.insert("pA".into(), vec![(80, 200)]);
        }
        assigner.note_tip_cover_claim("pA", 80, 200);
        // covering=1 at max → pB gets ahead partition after frontier 200, not a tip race.
        assert_eq!(
            assigner.get_work("pB", 1000),
            Some((201, 216)),
            "pB ahead partition after tip owner frontier"
        );
    }

    #[test]
    fn requeue_gap_height_push_front_micro_chunk() {
        let chunks = vec![(100, 199)];
        let vh = Arc::new(AtomicU64::new(149));
        let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], vh, 100, true);
        assigner.requeue_gap_height(150);
        // W16 tip fill runs before retry micros and assigns bulk from next_needed.
        assert_eq!(assigner.get_work("p1", 1000), Some((150, 165)));
    }

    #[test]
    fn w80_requeue_drops_obsolete_behind_tip_ranges() {
        // Live loop-1: ChunkGuard Drop re-queued 309798-309925 while tip≈321k.
        let chunks = vec![(300_000, 300_127), (321_000, 321_127)];
        let vh = Arc::new(AtomicU64::new(321_000)); // next_needed = 321001
        let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], vh, 300_000, true);
        assigner.set_wan_body_tip(312_499);
        assigner.requeue(309_798, 309_925, None);
        assert!(
            assigner.retry_queue.lock().unwrap().is_empty(),
            "behind-tip retry must not enter the queue"
        );
        assigner.requeue(321_001, 321_128, None);
        assert_eq!(assigner.retry_queue.lock().unwrap().len(), 1);
    }

    #[test]
    fn requeue_gap_heights_batches_micro_chunks() {
        let chunks = vec![(100, 199)];
        let vh = Arc::new(AtomicU64::new(149));
        let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], vh, 100, true);
        assigner.requeue_gap_heights(150, 4, None);
        // W16 tip fill prefers bulk 150-165 over coalesced micros.
        assert_eq!(assigner.get_work("p1", 1000), Some((150, 165)));
    }

    #[test]
    fn requeue_chunk_containing_height_is_idempotent() {
        let chunks = vec![(100, 199)];
        let assigner = assigner_for_heights(&chunks, &["p1"], 100, false);
        assigner.requeue_chunk_containing_height(150);
        let after_first = assigner.remaining_count();
        assigner.requeue_chunk_containing_height(150);
        assert_eq!(
            assigner.remaining_count(),
            after_first,
            "second stall recovery must not duplicate micro-chunks"
        );
        // 1 main chunk (100-199) + 1 bulk (150-165) + 1 gap micro (150) per W9.
        assert_eq!(
            after_first, 3,
            "main chunk + bulk gap + single (H,H) race"
        );
    }

    #[test]
    fn stall_recovery_clears_exclude_on_existing_retry_entry() {
        let chunks = vec![(100, 199)];
        let vh = Arc::new(AtomicU64::new(149));
        let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], vh, 100, true);
        assigner.requeue(100, 199, Some("p1".into()));
        assigner.requeue_stall_gaps(150, None);
        // Stall recovery must clear exclude on the containing full-chunk retry entry.
        let rq = assigner.retry_queue.lock().unwrap();
        let full = rq.iter().find(|(s, e, _)| *s == 100 && *e == 199);
        assert!(
            full.is_some_and(|(_, _, ex)| ex.is_none()),
            "exclude must be cleared so a peer can retry the containing chunk, got {full:?}"
        );
    }

    #[test]
    fn requeue_stall_gaps_debounces_same_height_within_window() {
        let chunks = vec![(100, 199)];
        let vh = Arc::new(AtomicU64::new(149));
        let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], vh, 100, true);
        assigner.requeue_stall_gaps(150, None);
        let after_first = assigner.remaining_count();
        assigner.requeue_stall_gaps(150, None);
        assert_eq!(
            assigner.remaining_count(),
            after_first,
            "duplicate stall requeue within debounce window must not add micro-chunks"
        );
        assigner.requeue_stall_gaps(150, Some("p1".into()));
        assert_eq!(
            assigner.remaining_count(),
            after_first,
            "exclude must not bypass debounce for same height"
        );
        assigner.requeue_stall_gaps(151, None);
        assert!(
            assigner.remaining_count() > after_first,
            "different stall height may requeue within debounce window"
        );
    }

    #[test]
    fn p1a_open_tip_slot_requires_ready_snapshot() {
        // Live W34′ soak: open slot assigned ibd_ready=false workers → handshake hard-fail carousel.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["worker".into(), "other".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("worker".into(), 1.0),
            ("other".into(), 1.0),
            ("idle-ready".into(), 9.0),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from(["idle-ready".into()]));
        assigner.open_tip_owner_slot();
        assert!(
            assigner.get_work("worker", 1000).is_none(),
            "open tip slot must not assign worker missing from ready snapshot"
        );
        assigner.set_ibd_ready_peers(HashSet::from(["worker".into(), "idle-ready".into()]));
        assert_eq!(
            assigner.get_work("worker", 1000).map(|(s, _)| s),
            Some(901),
            "open tip slot assigns ready top-half worker"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn mode_t_sole_peer_gd_slow_still_assigns_tip_span() {
        // Tip-band cliff plan Phase 3: sole ready + elevated gd_ewma must keep tip span;
        // no blacklist / tip-owner fail cooldown on the only archive peer.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::test_seed_getdata_body_ewma(1_500, 32);
        let vh = Arc::new(AtomicU64::new(400_287));
        let assigner = ChunkAssigner::new(
            vec![(400_288, 400_415), (400_416, 400_543)],
            vec!["127.0.0.1:18333".into()],
            Arc::clone(&vh),
            400_288,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(400_287);
        assigner.set_peer_scores(&[("127.0.0.1:18333".into(), 1.0)]);
        assigner.set_ibd_ready_peers(HashSet::from(["127.0.0.1:18333".into()]));
        assigner.set_tip_gap_missing(true);
        super::super::tip_stage::mark_needed(400_288);
        assert_eq!(assigner.ibd_ready_peer_count(), 1);
        assert!(super::super::download::tip_hole_gd_slow_sole_keep(1));
        let work = assigner.get_work("127.0.0.1:18333", 1000);
        assert!(work.is_some(), "sole ready peer must get tip work under GD_SLOW");
        let (s, e) = work.unwrap();
        assert!(e >= s, "tip span end≥start");
        assert!(
            e.saturating_sub(s) + 1 >= 32,
            "sole GD_SLOW must assign tip span, got {s}-{e}"
        );
        assigner.mark_tip_owner_fail_cooldown("127.0.0.1:18333", 180);
        assert!(
            !assigner.tip_owner_in_fail_cooldown("127.0.0.1:18333"),
            "sole peer must not enter tip-owner fail cooldown"
        );
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::mark_needed(0);
        assigner.set_tip_gap_missing(false);
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p1a_open_tip_slot_not_blocked_by_idle_higher_peer() {
        // Equal scores: lex-earlier "idle" peers have capacity but no get_work caller.
        // Open slot must let a later active peer take tip (live 714261 deadlock).
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007), (1008, 1071), (1072, 1135)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec![
                "100.0.0.1:8333".into(),
                "162.55.195.152:8333".into(),
                "170.75.166.57:8333".into(),
            ],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("100.0.0.1:8333".into(), 1.0),
            ("162.55.195.152:8333".into(), 1.0),
            ("163.0.0.1:8333".into(), 1.0), // scored, no worker
            ("170.75.166.57:8333".into(), 1.0),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "100.0.0.1:8333".into(),
            "162.55.195.152:8333".into(),
            "163.0.0.1:8333".into(),
            "170.75.166.57:8333".into(),
        ]));
        assigner.blacklist_peer("100.0.0.1:8333", Duration::from_secs(60));
        assigner.blacklist_peer("162.55.195.152:8333", Duration::from_secs(60));
        assigner.open_tip_owner_slot();
        assert_eq!(
            assigner
                .get_work("170.75.166.57:8333", 1000)
                .map(|(s, _)| s),
            Some(901),
            "open tip slot must not wait on idle higher-tiebreak peer 163.0.0.1"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn w36_sla_rotate_releases_inflight_and_opens_tip_slot() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["owner".into(), "other".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[("owner".into(), 9.0), ("other".into(), 8.0)]);
        assigner.set_ibd_ready_peers(HashSet::from(["owner".into(), "other".into()]));
        assert_eq!(
            assigner.get_work("owner", 1000).map(|(s, e)| (s, e)),
            Some((901, 1028)),
            "WAN tip owner gets 128-deep session (W48 64-deep reverted)"
        );
        let (healthy, raw, _) = assigner.tip_flight_diag_healthy();
        assert!(healthy >= 1 && raw >= 1);
        let prev = assigner.rotate_tip_owner_on_sla();
        assert_eq!(prev.as_deref(), Some("owner"));
        assigner.blacklist_peer("owner", Duration::from_secs(60));
        let (healthy2, raw2, _) = assigner.tip_flight_diag_healthy();
        assert_eq!(healthy2, 0, "claims cleared");
        assert_eq!(raw2, 0, "inflight released");
        assert!(
            assigner.is_done() == false,
            "workers must stay alive on WAN tip gap"
        );
        // Post-SLA open slot: non-top peer (other) may take tip.
        assert_eq!(
            assigner.get_work("other", 1000).map(|(s, _)| s),
            Some(901),
            "open tip slot lets next peer take tip after SLA"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn w82_open_slot_denies_floor_score_when_mid_ready_exists() {
        // Live mid-chain: open-slot lottery elected score=0.001 → 25s TIP_SLA stalls.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1000)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["floor".into(), "mid".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[("floor".into(), 0.001), ("mid".into(), 0.50)]);
        assigner.set_ibd_ready_peers(HashSet::from(["floor".into(), "mid".into()]));
        assigner.open_tip_owner_slot();
        assert!(
            assigner.get_work("floor", 1000).is_none(),
            "W82: floor-score peer must not win open tip slot while mid ready"
        );
        assert_eq!(
            assigner.get_work("mid", 1000).map(|(s, _)| s),
            Some(901),
            "W82: mid/high ready worker takes open tip slot"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn is_done_when_validation_reaches_ibd_end_despite_wan_tip_gap() {
        // Live 2026-07-13: after vh==effective_end past body tip, wan_tip_gap_crawl kept
        // is_done()==false forever → download_handles.await blocked Phase 3.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1000)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["p1".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800); // past body tip once next>800
        assigner.set_ibd_end_height(1000);
        assigner.set_tip_gap_missing(true);

        vh.store(999, Ordering::Relaxed);
        // Without end-height gate, W36 tip keep-alive would hold is_done==false here.
        assert!(
            !assigner.shutdown.load(Ordering::Relaxed),
            "precondition: not shut down"
        );
        // Mid-IBD: validation_reached_ibd_end is false, so tip-gap keep-alive wins.
        assert!(
            !assigner.is_done(),
            "mid-IBD: tip-gap keep-alive still applies (vh=999 < end=1000)"
        );

        vh.store(1000, Ordering::Relaxed);
        assert!(
            assigner.is_done(),
            "validation at IBD end must unblock worker exit despite wan_tip_gap / tip_gap_missing"
        );
        assert!(
            assigner.get_work("p1", 1000).is_none(),
            "no new work past IBD end"
        );

        // Explicit shutdown also forces done even before end.
        vh.store(900, Ordering::Relaxed);
        assigner.request_shutdown();
        assert!(assigner.is_done());
        assert!(assigner.get_work("p1", 1000).is_none());
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0b_wan_stall_retry_blocked_without_owner() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["bind".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("owner".into(), 9.0),
            ("other".into(), 8.0),
            ("racer".into(), 7.0),
        ]);
        // Non-force still must not enqueue WAN bulk/micro storms.
        assigner.requeue_stall_gaps(901, None);
        assert!(
            assigner.get_work("racer", 1000).is_none(),
            "WAN non-force stall must not assign to non-owner when preferred=None"
        );
        let ready = HashSet::from(["other".into()]);
        assigner.set_ibd_ready_peers(ready);
        assert_eq!(
            assigner.get_work("other", 1000).map(|(s, _)| s),
            Some(901),
            "gap preempt still arms ready owner"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0b_wan_stall_recovery_skips_micro_enqueue() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["owner".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.requeue_stall_gaps(901, None);
        let rq = assigner.retry_queue.lock().unwrap();
        assert!(
            rq.is_empty(),
            "WAN tip gap non-force must not enqueue stall micro/bulk — gap preempt only"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    /// W73: force + covering=0 arms a single (H,H) tip hole on WAN.
    /// Stripe-32 FORCE re-cheesed TIP_HOLE_AHEAD (Land E 2026-08-13 soak 12).
    #[test]
    fn w73_wan_force_requeue_enqueues_tip_hole_when_covering_zero() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["owner".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.requeue_stall_gaps_force(901, None);
        let rq: Vec<_> = assigner.retry_queue.lock().unwrap().iter().cloned().collect();
        let tip_heights: Vec<u64> = rq
            .iter()
            .filter(|(s, e, _)| *s == *e)
            .map(|(s, _, _)| *s)
            .collect();
        assert_eq!(
            tip_heights,
            vec![901],
            "WAN force covering=0 must enqueue (H,H) only; got {rq:?}"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    /// TRUE WAN: download complete must not clear tip-cover while tip still in span.
    #[test]
    fn wan_tip_claim_keep_until_tip_advances_past_span() {
        let _env = c1u_tests_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_WAN_TIP_CLAIM_KEEP", "1");
            std::env::remove_var("BLVM_IBD_SYNTH_WAN");
            std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
        }
        let vh = Arc::new(AtomicU64::new(900));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007)],
            vec!["owner".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_wan_body_tip(800); // next=901 > body → WAN tip crawl
        assigner.note_tip_cover_claim("owner", 901, 932);
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.insert("owner".into(), vec![(901, 932)]);
        }
        // Tip body present → keep; tip still missing → clear (allow re-fetch).
        assigner
            .tip_gap_missing
            .store(false, Ordering::Relaxed);
        assert_eq!(assigner.healthy_tip_cover_count(901), 1);
        assigner.on_chunk_complete_range("owner", 901, 932);
        assert_eq!(
            assigner.healthy_tip_cover_count(901),
            1,
            "claim must survive complete while tip present in span"
        );
        assigner.note_tip_cover_claim("owner", 901, 932);
        assigner
            .tip_gap_missing
            .store(true, Ordering::Relaxed);
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.insert("owner".into(), vec![(901, 932)]);
        }
        assigner.on_chunk_complete_range("owner", 901, 932);
        assert_eq!(
            assigner.healthy_tip_cover_count(901),
            0,
            "must clear when tip still missing after complete"
        );
        assigner.note_tip_cover_claim("owner", 901, 932);
        assigner
            .tip_gap_missing
            .store(false, Ordering::Relaxed);
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.insert("owner".into(), vec![(901, 932)]);
        }
        assigner.on_chunk_complete_range("owner", 901, 932);
        assert_eq!(assigner.healthy_tip_cover_count(901), 1);
        // Tip walks past span → prune on next complete (or retain filter).
        vh.store(933, Ordering::Relaxed);
        assigner.on_chunk_complete_range("owner", 940, 950);
        assert_eq!(
            assigner.healthy_tip_cover_count(934),
            0,
            "claims ending before tip must prune"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_WAN_TIP_CLAIM_KEEP");
        }
    }

    /// Dens: cross-height force debounce — tip 901 then 902 within window must not storm.
    #[test]
    fn w73_wan_force_requeue_debounces_across_tip_advance() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["owner".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.requeue_stall_gaps_force(901, None);
        let n1 = assigner.retry_queue.lock().unwrap().len();
        assert!(n1 > 0, "first force must enqueue");
        vh.store(901, Ordering::Relaxed); // tip advanced
        assigner.requeue_stall_gaps_force(902, None);
        let n2 = assigner.retry_queue.lock().unwrap().len();
        assert_eq!(
            n2, n1,
            "second force within debounce must not enqueue more (cross-height); n1={n1} n2={n2}"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    /// W73: force must not storm while two tip covers are already in flight.
    #[test]
    fn w73_wan_force_requeue_skips_when_covering_nonzero() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["owner".into(), "other".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            ChunkAssigner::insert_in_flight(&mut g, "owner", 901, 1028);
            ChunkAssigner::insert_in_flight(&mut g, "other", 901, 901);
        }
        assigner.note_tip_cover_claim("owner", 901, 1028);
        assigner.note_tip_cover_claim("other", 901, 901);
        assigner.requeue_stall_gaps_force(901, None);
        assert!(
            assigner.retry_queue.lock().unwrap().is_empty(),
            "WAN force must not enqueue while tip covering>1"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0b_non_wan_stall_recovery_still_enqueues_micro() {
        let vh = Arc::new(AtomicU64::new(50));
        let chunks = vec![(0, 199)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["p1".into()],
            Arc::clone(&vh),
            0,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(100);
        assigner.requeue_stall_gaps(51, None);
        let rq = assigner.retry_queue.lock().unwrap();
        assert!(
            !rq.is_empty(),
            "pre-body-tip gap should still use stall micro recovery"
        );
    }

    #[test]
    fn p0b_wan_stall_skipped_while_deep_owner_in_flight() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["owner".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            ChunkAssigner::insert_in_flight(&mut g, "owner", 901, 1028);
        }
        assigner.note_tip_cover_claim("owner", 901, 1028);
        assigner.requeue_stall_gaps(901, None);
        assert!(
            assigner.retry_queue.lock().unwrap().is_empty(),
            "must not micro-requeue while deep owner covers tip"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn blacklist_blocks_peer_until_expired() {
        let chunks = vec![(0, 63)];
        let assigner = assigner_for_heights(&chunks, &["p1"], 0, false);
        assigner.blacklist_peer("p1", Duration::from_secs(3600));
        assert!(assigner.get_work("p1", 1000).is_none());
    }

    #[test]
    fn work_stealing_ignores_peer_binding() {
        let chunks = vec![(0, 63)];
        let assigner = assigner_for_heights(&chunks, &["p1"], 0, true);
        assert_eq!(assigner.get_work("other-peer", 1000), Some((0, 63)));
    }

    #[test]
    fn chunk_guard_requeues_on_drop() {
        let chunks = vec![(0, 63)];
        let assigner = Arc::new(assigner_for_heights(&chunks, &["p1"], 0, false));
        let work = assigner.get_work("p1", 1000).unwrap();
        {
            let _guard = ChunkGuard::new(work.0, work.1, None, "p1".into(), Arc::clone(&assigner));
        }
        assert_eq!(assigner.remaining_count(), 1);
    }

    #[test]
    fn a4_top_scored_peer_may_hold_two_in_flight() {
        let vh = Arc::new(AtomicU64::new(99));
        let chunks = vec![(100, 115), (116, 131), (132, 147), (148, 163)];
        let peers = vec!["fast".into(), "mid".into(), "slow".into(), "worse".into()];
        let assigner = ChunkAssigner::new(chunks, peers, vh, 100, true);
        assigner.mark_bootstrap_complete();
        assigner.set_peer_scores(&[
            ("fast".into(), 10.0),
            ("mid".into(), 5.0),
            ("slow".into(), 2.0),
            ("worse".into(), 1.0),
        ]);
        assert_eq!(assigner.get_work("fast", 1000), Some((100, 115)));
        assert_eq!(
            assigner.get_work("fast", 1000),
            Some((116, 131)),
            "top-half scorer may pipeline a second chunk"
        );
        assert!(
            assigner.get_work("fast", 1000).is_none(),
            "still capped at dual in-flight"
        );
        assert_eq!(assigner.get_work("worse", 1000), Some((132, 147)));
        assert!(
            assigner.get_work("worse", 1000).is_none(),
            "bottom-half scorer stays single in-flight"
        );
    }

    #[test]
    fn p5_bottom_quartile_skips_gap_preempt() {
        let vh = Arc::new(AtomicU64::new(100));
        let chunks = vec![(80, 200), (201, 250), (251, 300), (301, 350)];
        let peers = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let assigner = ChunkAssigner::new(chunks, peers, Arc::clone(&vh), 80, true);
        assigner.mark_bootstrap_complete();
        assigner.set_peer_scores(&[
            ("a".into(), 10.0),
            ("b".into(), 8.0),
            ("c".into(), 6.0),
            ("d".into(), 1.0),
        ]);
        // Mid-chunk tip (next=101) → high scorer tip-fills.
        assert_eq!(assigner.get_work("a", 1000), Some((101, 116)));
        // Low-score peer skips tip ownership but still gets ahead partition (use peers).
        assert_eq!(
            assigner.get_work("d", 1000),
            Some((117, 132)),
            "low-score peer takes ahead partition, not tip race"
        );
        // Another peer continues partitioning ahead.
        let b = assigner.get_work("b", 1000);
        assert!(b.is_some());
        let (s, _) = b.unwrap();
        assert!(s >= 133, "b continues ahead of d, got start={s}");
    }

    #[test]
    fn w16_refuses_far_main_queue_while_tip_uncovered() {
        let vh = Arc::new(AtomicU64::new(100));
        // Tip at 101 inside first chunk; far chunk starts at 300 (> tip+64 band).
        let chunks = vec![(80, 200), (300, 363)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["pA".into(), "pB".into()],
            Arc::clone(&vh),
            80,
            true,
        );
        assigner.mark_bootstrap_complete();
        // Force next_index to the far chunk with tip uncovered.
        assigner.next_index.store(1, Ordering::Relaxed);
        let w = assigner.get_work("pA", 1000);
        assert_eq!(w, Some((101, 116)), "W16 tip fill before far main queue");
    }

    #[test]
    fn w33_wan_gap_top_peer_only_tip_owner() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(710_000));
        let chunks = vec![(710_000, 710_100), (710_101, 710_200)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["pA".into(), "pB".into()],
            Arc::clone(&vh),
            710_000,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(700_000);
        assigner.set_peer_scores(&[("pA".into(), 9.0), ("pB".into(), 3.0)]);
        mark_scored_peers_ibd_ready(&assigner);
        assert_eq!(
            assigner.get_work("pB", 1000),
            None,
            "W33a: non-top peer must not take WAN tip owner"
        );
        let tip = assigner.get_work("pA", 1000);
        assert!(tip.is_some(), "top peer must take tip owner");
        let (s, e) = tip.unwrap();
        assert_eq!(s, 710_001);
        assert!(e - s + 1 >= 64, "deep pipe expected, got {s}-{e}");
    }

    #[test]
    fn w15_overlapping_bulk_counts_toward_gap_fetcher_cap() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(149));
        let chunks = vec![(100, 199)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["pA".into()],
            vh,
            100,
            true,
        );
        assigner.mark_bootstrap_complete();
        // First tip fill 150-165 (mid-chunk).
        assert_eq!(assigner.get_work("pA", 1000), Some((150, 165)));
        assigner.on_chunk_complete("pA");
        // Simulate two overlapping bulks already covering tip (cap=2).
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.insert("x".into(), vec![(150, 165)]);
            g.insert("y".into(), vec![(151, 166)]);
        }
        assigner.note_tip_cover_claim("x", 150, 165);
        assigner.note_tip_cover_claim("y", 151, 166);
        assigner.requeue(152, 167, None);
        // Cap reached — must not assign another overlapping tip bulk to pA.
        let w = assigner.get_work("pA", 1000);
        if let Some((s, e)) = w {
            assert!(
                !(s <= 150 && 150 <= e),
                "W15: overlapping tip range must not assign when cap reached, got {s}-{e}"
            );
        }
    }

    #[test]
    fn p0a_empty_ready_denies_non_worker_on_wan() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["worker".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("worker".into(), 9.0),
            ("scored-idle".into(), 8.0),
        ]);
        assigner.set_ibd_ready_peers(HashSet::new());
        assert!(
            assigner.get_work("scored-idle", 1000).is_none(),
            "empty ready must deny non-worker tip owner on WAN"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_tip_owner_open_denies_active_worker_not_in_ready() {
        // Live W34′ soak: 11/42 assigns ibd_ready=false → hard-fail nudge carousel ~4 blk/s.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["worker".into(), "other".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("worker".into(), 1.0),
            ("other".into(), 1.0),
            ("idle-ready".into(), 9.0),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from(["idle-ready".into()]));
        assigner.open_tip_owner_slot();
        assert!(
            assigner.get_work("worker", 1000).is_none(),
            "open tip slot must not assign active worker missing from ready snapshot"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_tip_owner_open_denies_scored_non_worker_not_ready() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["worker".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("worker".into(), 1.0),
            ("scored-idle".into(), 9.0),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from(["idle-ready".into()]));
        assigner.open_tip_owner_slot();
        assert!(
            assigner.get_work("scored-idle", 1000).is_none(),
            "open tip slot must not assign scored non-workers missing from ready snapshot"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_nudge_keeps_ready_sticky_owner() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["sticky".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("sticky".into(), 9.0),
            ("other".into(), 1.0),
            ("mid".into(), 5.0),
            ("low".into(), 0.0),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "sticky".into(),
            "other".into(),
            "mid".into(),
        ]));
        assigner.note_tip_owner_assigned("sticky");
        assigner.nudge_wan_tip_owner();
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("sticky"),
            "nudge must keep ready sticky owner"
        );
        assert_eq!(
            assigner.get_work("sticky", 1000).map(|(s, _)| s),
            Some(901),
            "open slot after nudge must re-arm sticky owner"
        );
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("sticky"),
            "sticky must remain preferred after assign"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_nudge_upgrades_mediocre_sticky_to_better_worker() {
        // Live A6c: sticky score=1.000 @ ~15s/chunk locked out breakthrough-class peers.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["sticky".into(), "fast".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("sticky".into(), 1.0),
            ("fast".into(), 1.365),
            ("mid".into(), 1.1),
            ("low".into(), 0.5),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "sticky".into(),
            "fast".into(),
            "mid".into(),
        ]));
        assigner.note_tip_owner_assigned("sticky");
        assigner.nudge_wan_tip_owner();
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("fast"),
            "nudge must pin preferred to better active worker (not None lottery)"
        );
        assert_eq!(
            assigner.get_work("fast", 1000).map(|(s, _)| s),
            Some(901),
            "open slot must arm better-scored active worker"
        );
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("fast"),
            "better worker becomes new sticky"
        );
        // Sticky may still take ahead partitions; tip cover must stay with fast.
        let (covering, _, _) = assigner.tip_flight_diag();
        assert!(covering >= 1, "fast must hold tip cover after upgrade");
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_nudge_ignores_floor_noise_upgrade() {
        // Live A6d: sticky@0.100 → better@0.191 thrash cleared owners mid-pipe.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["sticky".into(), "other".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("sticky".into(), 0.100),
            ("other".into(), 0.191),
            ("mid".into(), 0.190),
            ("low".into(), 0.100),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "sticky".into(),
            "other".into(),
            "mid".into(),
        ]));
        assigner.note_tip_owner_assigned("sticky");
        assigner.nudge_wan_tip_owner();
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("sticky"),
            "floor-noise score delta must not clear sticky"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn nudge_defers_upgrade_while_sticky_holds_tip_download() {
        // Live genesis 2026-07-17: every ~1s UPGRADE sticky@0.001→better_worker blacklisted
        // the in-flight tip peer → IBD_TIP_BLACKLIST abort → tip freeze. Mid-download must
        // defer score upgrade; tip-SLA is the abort path.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["sticky".into(), "faster".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("sticky".into(), 0.001),
            ("faster".into(), 0.210),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "sticky".into(),
            "faster".into(),
        ]));
        assigner.note_tip_owner_assigned("sticky");
        assigner.note_tip_cover_claim("sticky", 901, 1028);
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.insert("sticky".into(), vec![(901, 1028)]);
        }
        assert_eq!(assigner.tip_flight_diag().0, 1, "tip covering in-flight");
        assigner.nudge_wan_tip_owner();
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("sticky"),
            "must not upgrade away from peer mid tip-download"
        );
        assert!(
            !assigner.is_peer_blacklisted("sticky"),
            "must not blacklist mid tip-download peer (that aborts the pipe)"
        );
        // After flight ends, upgrade + blacklist of demoted sticky is allowed.
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.remove("sticky");
        }
        assigner.clear_tip_cover_claims_for_peer("sticky");
        assigner.nudge_wan_tip_owner();
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("faster"),
            "after tip flight ends, 0.001 sticky may upgrade to better_worker"
        );
        assert!(
            assigner.is_peer_blacklisted("sticky"),
            "demoted sticky without tip flight may be cooloff-blacklisted"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_nudge_upgrades_floor_sticky_on_2x_jump() {
        // Live 2026-07-14: sticky@0.100 vs top_w@0.203 — must upgrade (2× rule).
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["sticky".into(), "faster".into(), "low".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("sticky".into(), 0.100),
            ("faster".into(), 0.210),
            ("mid".into(), 0.190),
            ("low".into(), 0.001),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "sticky".into(),
            "faster".into(),
            "mid".into(),
            "low".into(),
        ]));
        assigner.note_tip_owner_assigned("sticky");
        assigner.nudge_wan_tip_owner();
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("faster"),
            "2× floor jump must pin preferred to better_worker@0.210 (not None lottery)"
        );
        // Live 2026-07-15: demoted peer_ok (score=0.001, floor=0.001) must not win *tip*
        // ahead of the pinned upgrade target — probe *before* faster arms.
        if let Some((s, _)) = assigner.get_work("low", 1000) {
            assert_ne!(
                s, 901,
                "demoted/floor peer must not take tip span on open slot (got start={s})"
            );
        }
        assert_eq!(
            assigner.get_work("faster", 1000).map(|(s, _)| s),
            Some(901),
            "open slot must arm 2×-better worker"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_nudge_keeps_hot_floor_sticky_streamer() {
        // Live 2026-07-14: sticky@0.100 mid-GAP_STREAM upgraded to idle@0.211 → walk-in abort.
        // Hold only when recent tip BPS is proven ≥ stretch floor_min (missing samples escape).
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(9999));
        let chunks = vec![(880, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["sticky".into(), "faster".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("sticky".into(), 0.100),
            ("faster".into(), 0.210),
            ("mid".into(), 0.190),
            ("low".into(), 0.100),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "sticky".into(),
            "faster".into(),
            "mid".into(),
        ]));
        assigner.note_tip_owner_assigned("sticky");
        test_set_sticky_tenure(&assigner, "sticky", 1000, 600);
        // note_tip_owner_assigned seeds a "now" sample — clear so ago-samples stay time-ordered.
        assigner.tip_progress_samples.lock().unwrap().clear();
        // Sample older than recent window (default 60s). +2700 / 90s = 30 ≥ stretch floor_min=22.
        test_push_tip_sample(&assigner, 7300, 90);
        test_push_tip_sample(&assigner, 10000, 0);
        assigner.note_wan_tip_stream("sticky");
        assert!(
            assigner.peer_recently_tip_streaming("sticky", Duration::from_secs(15)),
            "just-streamed sticky must be hot"
        );
        assert!(
            !assigner.preferred_is_idle_floor_sticky(),
            "hot sticky with proven stretch BPS is not idle"
        );
        assigner.nudge_wan_tip_owner();
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("sticky"),
            "hot tip streamer with ≥stretch BPS must not be score-upgraded away"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_nudge_upgrades_hot_but_stalling_floor_sticky() {
        // Live 2026-07-15: receive-path tip-hot + score=0.100 @ ~5 blk/s blocked 2× upgrade
        // while OPEN_STALL top_w@0.197. Hot+below stretch floor_min must escape.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(9999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec!["sticky".into(), "faster".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("sticky".into(), 0.100),
            ("faster".into(), 0.210),
            ("mid".into(), 0.190),
            ("low".into(), 0.100),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "sticky".into(),
            "faster".into(),
            "mid".into(),
        ]));
        test_set_sticky_tenure(&assigner, "sticky", 1000, 600);
        // Recent ~5 blk/s (< stretch floor_min 22).
        test_push_tip_sample(&assigner, 9700, 60);
        test_push_tip_sample(&assigner, 10000, 0);
        assigner.note_wan_tip_stream("sticky");
        assert!(assigner.peer_recently_tip_streaming("sticky", Duration::from_secs(15)));
        assert!(
            assigner.preferred_is_idle_floor_sticky(),
            "hot-but-below-stretch floor sticky is idle for nudge"
        );
        assigner.nudge_wan_tip_owner();
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("faster"),
            "hot below-stretch sticky must 2×-upgrade to faster worker"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_nudge_upgrades_hot_floor_sticky_below_stretch() {
        // Live 2026-07-15 ~h670k: ~11–15 blk/s hot sticky@0.100 vs OPEN_STALL top_w@0.197.
        // open_slot_min=12 correctly keeps A6N; stretch floor_min=22 must still allow 2× escape.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(9999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec!["sticky".into(), "faster".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("sticky".into(), 0.100),
            ("faster".into(), 0.210),
            ("mid".into(), 0.190),
            ("low".into(), 0.100),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "sticky".into(),
            "faster".into(),
            "mid".into(),
        ]));
        test_set_sticky_tenure(&assigner, "sticky", 1000, 600);
        // Recent +900 / 60s = 15 blk/s — ≥ open_slot_min, < stretch floor_min.
        test_push_tip_sample(&assigner, 9100, 60);
        test_push_tip_sample(&assigner, 10000, 0);
        assigner.note_wan_tip_stream("sticky");
        assert!(
            assigner.preferred_is_idle_floor_sticky(),
            "15 blk/s hot floor sticky is below stretch for nudge"
        );
        assigner.nudge_wan_tip_owner();
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("faster"),
            "below-stretch hot sticky must 2×-upgrade"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn w28d_hot_tip_streamer_survives_walk_in_after_claim_clear() {
        // After upgrade clears exact tip-cover claim, hot streamer must not abort.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007)],
            vec!["streamer".into(), "ahead".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[("streamer".into(), 0.100), ("ahead".into(), 0.210)]);
        assigner.set_ibd_ready_peers(HashSet::from(["streamer".into(), "ahead".into()]));
        assigner.note_tip_cover_claim("streamer", 901, 964);
        assigner.note_wan_tip_stream("streamer");
        // Simulate upgrade clearing the claim while streamer still holds the range.
        assigner.clear_all_tip_cover_claims();
        assert!(
            !assigner.should_abort_tip_walk_in("streamer", 901, 964),
            "hot GAP_STREAM peer must not walk-in-abort after claim clear"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_nudge_ignores_unproven_default_score_upgrade() {
        // Live A6e: 13/13 upgrades sticky@0.100 → unproven@1.000 (blocks_received==0).
        // tip_owner_score demotes unproven; min-candidate 0.5 also blocks raw default 1.0
        // only when... wait, raw 1.0 would still pass min 0.5. Refresh demotion is required.
        // Simulate post-refresh demoted ranks:
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["sticky".into(), "unproven".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("sticky".into(), 0.100),
            ("unproven".into(), 0.001),
            ("mid".into(), 0.001),
            ("low".into(), 0.001),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "sticky".into(),
            "unproven".into(),
            "mid".into(),
        ]));
        assigner.note_tip_owner_assigned("sticky");
        assigner.nudge_wan_tip_owner();
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("sticky"),
            "demoted unproven must not clear delivering sticky"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_below_floor_sticky_does_not_deadlock_open_tip() {
        // Live A6g: preferred stayed after span end while score fell below WAN median
        // (OPEN_STALL: preferred≠top_w, floor=0.190, open=true, covering=0, busy=0).
        // Upgrade min 0.5 never fires in tip_owner_score demotion world → exclusive sticky
        // blocks all peer_ok workers forever.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![
            (880, 1007),
            (1008, 1071),
            (1072, 1135),
            (1136, 1199),
        ];
        let assigner = ChunkAssigner::new(
            chunks,
            vec![
                "sticky".into(),
                "top_w".into(),
                "mid".into(),
                "low".into(),
            ],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("sticky".into(), 0.050), // below median floor
            ("top_w".into(), 0.195),
            ("mid".into(), 0.190),
            ("low".into(), 0.100),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "sticky".into(),
            "top_w".into(),
            "mid".into(),
            "low".into(),
        ]));
        assigner.note_tip_owner_assigned("sticky");
        assert!(
            assigner.tip_sticky_usable("sticky"),
            "A6k: ready+active sticky remains usable even below peer_ok floor"
        );
        assigner.nudge_wan_tip_owner();
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("sticky"),
            "nudge must keep ready sticky (score-floor must not STICKY_DROP)"
        );
        // Sticky must be able to re-arm tip despite floor.
        let tip = assigner.get_work("sticky", 1000);
        assert_eq!(tip.map(|(s, _)| s), Some(901), "below-floor sticky must still take tip");
        let te = tip.unwrap().1;
        // Non-sticky may take non-overlapping ahead/main-queue, but not tip cover.
        if let Some((s, e)) = assigner.get_work("top_w", 1000) {
            assert!(
                s > te || e < 901,
                "top_w must not steal tip cover from usable sticky, got {s}-{e} tip_end={te}"
            );
        }
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn a6l_sticky_below_median_gets_top_in_flight_cap() {
        // Live A6k: sticky@0.1 < median → max_in_flight=1 → cannot re-arm next tip span.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![
            (880, 1007),
            (1008, 1135),
            (1136, 1263),
            (1264, 1391),
        ];
        let assigner = ChunkAssigner::new(
            chunks,
            vec![
                "sticky".into(),
                "top_w".into(),
                "mid".into(),
                "low".into(),
            ],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("sticky".into(), 0.100),
            ("top_w".into(), 0.195),
            ("mid".into(), 0.190),
            ("low".into(), 0.185),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "sticky".into(),
            "top_w".into(),
            "mid".into(),
            "low".into(),
        ]));
        assigner.note_tip_owner_assigned("sticky");
        assert_eq!(
            assigner.max_in_flight_for("sticky"),
            ChunkAssigner::top_peer_in_flight_cap(),
            "A6l: preferred sticky must get top in-flight cap even below score median"
        );
        assert_eq!(
            assigner.max_in_flight_for("low"),
            1,
            "non-sticky below median stays at 1"
        );
        // Fill one span, sticky must still re-arm tip with second slot.
        let first = assigner.get_work("sticky", 1000);
        assert!(first.is_some(), "sticky first tip assign");
        let second = assigner.get_work("sticky", 1000);
        assert!(
            second.is_some(),
            "A6l: sticky must re-arm second tip span while first still in flight"
        );
        // Idle higher-scored peer must not steal tip while sticky holds / is usable.
        if let Some((s, e)) = assigner.get_work("top_w", 1000) {
            let tip = vh.load(Ordering::Relaxed) + 1;
            assert!(
                s > tip && !(s <= tip && tip <= e),
                "top_w must not steal tip cover while sticky busy, got {s}-{e}"
            );
        }
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    #[serial]
    fn a31_frontier_dual_allows_c1g_after_tip_under_top1() {
        // Rematch TOP=1 blocks dens C1g sticky-after-tip; A31 opens one after-tip slot only.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::test_reset_tip_stage();
        A31_FRONTIER_DUAL_LAST_ARM_MS.store(0, Ordering::Relaxed);
        let prev_top = std::env::var("BLVM_IBD_TOP_PEER_IN_FLIGHT").ok();
        let prev_dual = std::env::var("BLVM_IBD_TIP_FRONTIER_DUAL").ok();
        let prev_distress = std::env::var("BLVM_IBD_TIP_FRONTIER_DUAL_DISTRESS").ok();
        unsafe {
            std::env::set_var("BLVM_IBD_TOP_PEER_IN_FLIGHT", "1");
            std::env::set_var("BLVM_IBD_TIP_FRONTIER_DUAL", "1");
            // Unit proves slot geometry; A42 distress gate covered separately.
            std::env::set_var("BLVM_IBD_TIP_FRONTIER_DUAL_DISTRESS", "0");
        }
        super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);

        let assigner = wan_tip_assigner(900, 800, 100_000, &["sticky", "other", "mid"]);
        assigner.set_peer_scores(&[
            ("sticky".into(), 9.0),
            ("other".into(), 8.0),
            ("mid".into(), 7.0),
        ]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        assigner.note_tip_owner_assigned("sticky");

        let tip = assigner.get_work("sticky", 4096).expect("tip owner under TOP=1");
        assert_eq!(tip.0, 901);
        // Tip lands — dens C1g after-tip path.
        assigner.set_tip_gap_missing(false);
        let after = assigner
            .get_work("sticky", 4096)
            .expect("A31: sticky after-tip under TOP=1 while tip flight held");
        assert!(
            after.0 > tip.1,
            "A31 second slot must be after tip end, tip={}-{} after={}-{}",
            tip.0,
            tip.1,
            after.0,
            after.1
        );

        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_FRONTIER_DUAL");
        }
        // Without A31, TOP=1 sticky stays capped at flight=1.
        let assigner2 = wan_tip_assigner(900, 800, 100_000, &["sticky", "other", "mid"]);
        assigner2.set_peer_scores(&[
            ("sticky".into(), 9.0),
            ("other".into(), 8.0),
            ("mid".into(), 7.0),
        ]);
        mark_scored_peers_ibd_ready(&assigner2);
        assigner2.set_tip_gap_missing(true);
        assigner2.note_tip_owner_assigned("sticky");
        let tip2 = assigner2.get_work("sticky", 4096).expect("tip owner");
        assigner2.set_tip_gap_missing(false);
        assert!(
            assigner2.get_work("sticky", 4096).is_none(),
            "without A31, TOP=1 sticky must not take after-tip while tip flight held"
        );
        let _ = tip2;

        super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        unsafe {
            match prev_top {
                Some(v) => std::env::set_var("BLVM_IBD_TOP_PEER_IN_FLIGHT", v),
                None => std::env::remove_var("BLVM_IBD_TOP_PEER_IN_FLIGHT"),
            }
            match prev_dual {
                Some(v) => std::env::set_var("BLVM_IBD_TIP_FRONTIER_DUAL", v),
                None => std::env::remove_var("BLVM_IBD_TIP_FRONTIER_DUAL"),
            }
            match prev_distress {
                Some(v) => std::env::set_var("BLVM_IBD_TIP_FRONTIER_DUAL_DISTRESS", v),
                None => std::env::remove_var("BLVM_IBD_TIP_FRONTIER_DUAL_DISTRESS"),
            }
        }
    }

    #[test]
    #[serial]
    fn a51_sole_empty_release_reissues_tip_under_top1() {
        // A50 cliff: EMPTY + covering=1 + TOP=1 → STICKY_CAP forever. A51 clears zombie
        // tip flight (not dual) so sticky can re-GetData tip.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::test_reset_tip_stage();
        A51_SOLE_EMPTY_LAST_RELEASE_MS.store(0, Ordering::Relaxed);
        let prev_top = std::env::var("BLVM_IBD_TOP_PEER_IN_FLIGHT").ok();
        let prev_rel = std::env::var("BLVM_IBD_SOLE_EMPTY_RELEASE").ok();
        let prev_min = std::env::var("BLVM_IBD_SOLE_EMPTY_RELEASE_MIN_H").ok();
        let prev_ms = std::env::var("BLVM_IBD_SOLE_EMPTY_RELEASE_MS").ok();
        let prev_span = std::env::var("BLVM_IBD_SOLE_EMPTY_RELEASE_MIN_SPAN").ok();
        let prev_dual = std::env::var("BLVM_IBD_TIP_FRONTIER_DUAL").ok();
        unsafe {
            std::env::set_var("BLVM_IBD_TOP_PEER_IN_FLIGHT", "1");
            std::env::set_var("BLVM_IBD_SOLE_EMPTY_RELEASE", "1");
            std::env::set_var("BLVM_IBD_SOLE_EMPTY_RELEASE_MIN_H", "0");
            std::env::set_var("BLVM_IBD_SOLE_EMPTY_RELEASE_MS", "200");
            // Unit tip stripes are deep; keep min_span low so geometry still proves.
            std::env::set_var("BLVM_IBD_SOLE_EMPTY_RELEASE_MIN_SPAN", "1");
            std::env::set_var("BLVM_IBD_TIP_FRONTIER_DUAL", "0");
        }
        super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);

        let assigner = wan_tip_assigner(900, 800, 100_000, &["sticky", "other"]);
        assigner.set_peer_scores(&[("sticky".into(), 9.0), ("other".into(), 8.0)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        assigner.note_tip_owner_assigned("sticky");
        let tip = assigner
            .get_work("sticky", 4096)
            .expect("initial tip assign");
        assert_eq!(tip.0, 901);
        // Cap blocks; arms stuck clock on flight span (no release yet).
        assert!(
            assigner.get_work("sticky", 4096).is_none(),
            "first re-get under debounce must stay capped"
        );
        // Tip creep under the same zombie span must not reset stuck (A51 live miss
        // keyed stuck on tip height). Advance tip within the held deep cover.
        assert!(
            tip.1 >= tip.0 + 2,
            "need deep tip span for creep test, got {}-{}",
            tip.0,
            tip.1
        );
        assigner
            .validation_height
            .store(tip.0 + 1, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(250));
        let re = assigner
            .get_work("sticky", 4096)
            .expect("A51: re-issue tip after sole EMPTY release");
        let expect = tip.0 + 2;
        assert_eq!(
            re.0, expect,
            "must re-arm crept tip after span-keyed release, got {}-{}",
            re.0,
            re.1
        );

        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        unsafe {
            match prev_top {
                Some(v) => std::env::set_var("BLVM_IBD_TOP_PEER_IN_FLIGHT", v),
                None => std::env::remove_var("BLVM_IBD_TOP_PEER_IN_FLIGHT"),
            }
            match prev_rel {
                Some(v) => std::env::set_var("BLVM_IBD_SOLE_EMPTY_RELEASE", v),
                None => std::env::remove_var("BLVM_IBD_SOLE_EMPTY_RELEASE"),
            }
            match prev_min {
                Some(v) => std::env::set_var("BLVM_IBD_SOLE_EMPTY_RELEASE_MIN_H", v),
                None => std::env::remove_var("BLVM_IBD_SOLE_EMPTY_RELEASE_MIN_H"),
            }
            match prev_ms {
                Some(v) => std::env::set_var("BLVM_IBD_SOLE_EMPTY_RELEASE_MS", v),
                None => std::env::remove_var("BLVM_IBD_SOLE_EMPTY_RELEASE_MS"),
            }
            match prev_span {
                Some(v) => std::env::set_var("BLVM_IBD_SOLE_EMPTY_RELEASE_MIN_SPAN", v),
                None => std::env::remove_var("BLVM_IBD_SOLE_EMPTY_RELEASE_MIN_SPAN"),
            }
            match prev_dual {
                Some(v) => std::env::set_var("BLVM_IBD_TIP_FRONTIER_DUAL", v),
                None => std::env::remove_var("BLVM_IBD_TIP_FRONTIER_DUAL"),
            }
        }
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    #[serial]
    fn a42_frontier_dual_requires_gd_distress() {
        // A42: healthy tip_gd must not open dual; seeded gd_ewma≥400 must.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::test_reset_getdata_body_ewma();
        A31_FRONTIER_DUAL_LAST_ARM_MS.store(0, Ordering::Relaxed);
        let prev_top = std::env::var("BLVM_IBD_TOP_PEER_IN_FLIGHT").ok();
        let prev_dual = std::env::var("BLVM_IBD_TIP_FRONTIER_DUAL").ok();
        let prev_distress = std::env::var("BLVM_IBD_TIP_FRONTIER_DUAL_DISTRESS").ok();
        let prev_ms = std::env::var("BLVM_IBD_TIP_FRONTIER_DUAL_MS").ok();
        unsafe {
            std::env::set_var("BLVM_IBD_TOP_PEER_IN_FLIGHT", "1");
            std::env::set_var("BLVM_IBD_TIP_FRONTIER_DUAL", "1");
            std::env::remove_var("BLVM_IBD_TIP_FRONTIER_DUAL_DISTRESS"); // default ON
            std::env::set_var("BLVM_IBD_TIP_FRONTIER_DUAL_MS", "50");
        }
        super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);

        let assigner = wan_tip_assigner(900, 800, 100_000, &["sticky", "other", "mid"]);
        assigner.set_peer_scores(&[
            ("sticky".into(), 9.0),
            ("other".into(), 8.0),
            ("mid".into(), 7.0),
        ]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        assigner.note_tip_owner_assigned("sticky");
        let tip = assigner.get_work("sticky", 4096).expect("tip owner");
        assigner.set_tip_gap_missing(false);
        // Healthy EWMA — dual must stay capped.
        super::super::tip_stage::test_seed_getdata_body_ewma(40, 16);
        assert!(
            assigner.get_work("sticky", 4096).is_none(),
            "A42: healthy tip_gd must not arm frontier dual"
        );
        // Distress EWMA — dual opens after-tip.
        super::super::tip_stage::test_seed_getdata_body_ewma(800, 16);
        let after = assigner
            .get_work("sticky", 4096)
            .expect("A42: distress gd opens after-tip dual");
        assert!(after.0 > tip.1, "distress dual after tip");

        super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        super::super::tip_stage::test_reset_getdata_body_ewma();
        unsafe {
            match prev_top {
                Some(v) => std::env::set_var("BLVM_IBD_TOP_PEER_IN_FLIGHT", v),
                None => std::env::remove_var("BLVM_IBD_TOP_PEER_IN_FLIGHT"),
            }
            match prev_dual {
                Some(v) => std::env::set_var("BLVM_IBD_TIP_FRONTIER_DUAL", v),
                None => std::env::remove_var("BLVM_IBD_TIP_FRONTIER_DUAL"),
            }
            match prev_distress {
                Some(v) => std::env::set_var("BLVM_IBD_TIP_FRONTIER_DUAL_DISTRESS", v),
                None => std::env::remove_var("BLVM_IBD_TIP_FRONTIER_DUAL_DISTRESS"),
            }
            match prev_ms {
                Some(v) => std::env::set_var("BLVM_IBD_TIP_FRONTIER_DUAL_MS", v),
                None => std::env::remove_var("BLVM_IBD_TIP_FRONTIER_DUAL_MS"),
            }
        }
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    #[serial]
    fn w35ppp_sticky_tip_session_is_deep() {
        // Near tip (header tip close): WAN tip owner gets a deep session (~128 default).
        // Dual-pipe second get_work is dead on 1-worker/peer WAN without bulk window.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::test_reset_tip_stage();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![
            (880, 1007),
            (1008, 1135),
            (1136, 1263),
            (1264, 1391),
        ];
        let assigner = ChunkAssigner::new(
            chunks,
            vec![
                "sticky".into(),
                "other".into(),
                "mid".into(),
                "low".into(),
            ],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        // Near tip — not bulk catch-up (header only ~490 ahead of next).
        assigner.set_header_tip(1391);
        assigner.set_peer_scores(&[
            ("sticky".into(), 0.100),
            ("other".into(), 0.195),
            ("mid".into(), 0.190),
            ("low".into(), 0.185),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "sticky".into(),
            "other".into(),
            "mid".into(),
            "low".into(),
        ]));
        // C1e/C1g: while tip missing, tip-owner takes runway stripe (default 32), not 128.
        assigner.set_tip_gap_missing(true);
        assigner.note_tip_owner_assigned("sticky");

        let first = assigner.get_work("sticky", 256);
        assert!(first.is_some(), "sticky tip span");
        let (s0, e0) = first.unwrap();
        assert_eq!(s0, 901);
        let span = e0.saturating_sub(s0).saturating_add(1);
        assert!(
            (8..=96).contains(&span),
            "tip-missing owner stripe must be runway-sized, got {s0}-{e0} span={span}"
        );

        // After tip lands: deep pipe on a fresh assigner; ahead OK with high holes.
        assigner.on_chunk_complete_range("sticky", s0, e0);
        assigner.set_tip_gap_missing(false);
        // C1i: contig≥8 before deep/ahead (tipfix DNA).
        super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
        let deep = assigner.get_work("sticky", 256).expect("deep tip after tip lands");
        let deep_span = deep.1.saturating_sub(deep.0).saturating_add(1);
        assert!(
            deep_span >= 100,
            "near-tip session after tip lands must be ~128 deep, got {}-{} span={deep_span}",
            deep.0,
            deep.1
        );
        // W47: ahead OK with high holes after tip lands.
        assigner.set_tip_bridge_holes(64);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
        let ahead = assigner.get_work("other", 256);
        assert!(
            ahead.is_some(),
            "W47: other must get tip-band ahead with holes=64 after tip lands"
        );
        let (s, e) = ahead.unwrap();
        assert!(s > deep.1, "ahead after tip end, got {s}-{e} tip_end={}", deep.1);
        super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    #[serial]
    fn c1g_freezes_past_tip_stripes_while_tip_missing() {
        // C1f live: tip_hole_ahead×20 / ahead_buf_p50=115 — stripes past tip while tip empty.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::mark_needed(0);
        let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead", "spare"]);
        assigner.set_peer_scores(&[
            ("owner".into(), 9.0),
            ("ahead".into(), 8.0),
            ("spare".into(), 7.0),
        ]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);

        let tip = assigner.get_work("owner", 4096).expect("tip owner");
        assert_eq!(tip.0, 901);
        assert!(tip.1 > tip.0, "tip owner stripe");

        for peer in ["ahead", "spare"] {
            match assigner.get_work(peer, 4096) {
                None => {}
                // C1h: tip-height race (tip fill or clipped main-queue) OK.
                Some((s, e)) if s == 901 && e == 901 => {
                    assigner.on_chunk_complete_range(peer, s, e);
                }
                Some((s, e)) => panic!(
                    "C1g: past-tip stripe while tip missing, {peer} got {s}-{e}"
                ),
            }
        }

        assigner.set_tip_gap_missing(false);
        // C1i: ahead also requires contig runway ≥ min (default 8).
        super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
        let ahead = assigner.get_work("ahead", 4096).expect("ahead after tip lands");
        assert!(
            ahead.0 > tip.1,
            "after tip lands, ahead starts past tip stripe end, got {}-{}",
            ahead.0,
            ahead.1
        );
        super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    #[serial]
    fn bulk_catchup_ahead_ok_with_high_holes() {
        // W47: bulk catch-up keeps multi-peer ahead under high holes when tip healthy.
        // Tip owner deep pipe (≥128); ahead fills past that. Soft-retry freezes ahead.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::mark_needed(0);
        let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead", "spare"]);
        assigner.set_peer_scores(&[
            ("owner".into(), 9.0),
            ("ahead".into(), 8.0),
            ("spare".into(), 7.0),
        ]);
        mark_scored_peers_ibd_ready(&assigner);
        // C1g: tip in reorder → deep tip pipe + multi-peer ahead (not while tip missing).
        assigner.set_tip_gap_missing(false);
        // C1i: contig≥8 before deep/ahead (tip form DNA).
        super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
        assigner.set_tip_bridge_holes(64);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);

        let tip = assigner.get_work("owner", 4096);
        assert!(tip.is_some(), "tip owner must get work");
        let (ts, te) = tip.unwrap();
        assert_eq!(ts, 901);
        let tip_span = te.saturating_sub(ts).saturating_add(1);
        assert!(
            tip_span >= 128,
            "WAN tip owner must get ≥128 deep pipe in bulk catch-up, got {tip_span}"
        );

        let ahead = assigner.get_work("ahead", 4096);
        assert!(
            ahead.is_some(),
            "W47: multi-peer ahead must work with holes=64 when tip healthy"
        );
        let (s, e) = ahead.unwrap();
        assert!(
            s > te,
            "ahead past tip owner end, got {s}-{e} tip_end={te}"
        );
        assert!(
            s <= ts.saturating_add(400),
            "WAN ahead must stay near tip, got start={s} tip={ts}"
        );
        super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn a6k_not_ready_sticky_still_dropped_on_nudge() {
        // A6h safety: not-ready sticky must still clear so open slot can re-arm.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![
            (880, 1007),
            (1008, 1071),
            (1072, 1135),
            (1136, 1199),
        ];
        let assigner = ChunkAssigner::new(
            chunks,
            vec![
                "sticky".into(),
                "top_w".into(),
                "mid".into(),
                "low".into(),
            ],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("sticky".into(), 0.100),
            ("top_w".into(), 0.195),
            ("mid".into(), 0.190),
            ("low".into(), 0.100),
        ]);
        // sticky NOT in ready set
        assigner.set_ibd_ready_peers(HashSet::from([
            "top_w".into(),
            "mid".into(),
            "low".into(),
        ]));
        assigner.note_tip_owner_assigned("sticky");
        assert!(!assigner.tip_sticky_usable("sticky"));
        assigner.nudge_wan_tip_owner();
        assert_eq!(assigner.preferred_tip_owner().as_deref(), None);
        assert_eq!(
            assigner.get_work("top_w", 1000).map(|(s, _)| s),
            Some(901),
            "open slot must arm a ready peer_ok worker after not-ready sticky drop"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_ready_floor_not_inflated_by_unready_high_scorers() {
        // Live A6i: floor=0.153 from unready scorers, all ready ≤0.127 → ready_active_ok=0/9.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![
            (880, 1007),
            (1008, 1071),
            (1072, 1135),
            (1136, 1199),
            (1200, 1263),
            (1264, 1327),
            (1328, 1391),
            (1392, 1455),
        ];
        let assigner = ChunkAssigner::new(
            chunks,
            vec![
                "live0".into(),
                "live1".into(),
                "live2".into(),
                "live3".into(),
                "gone0".into(),
                "gone1".into(),
                "gone2".into(),
                "gone3".into(),
            ],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("live0".into(), 0.127),
            ("live1".into(), 0.120),
            ("live2".into(), 0.115),
            ("live3".into(), 0.110),
            ("gone0".into(), 0.195),
            ("gone1".into(), 0.190),
            ("gone2".into(), 0.185),
            ("gone3".into(), 0.180),
        ]);
        // Only low-scored peers are ready (gone* disconnected).
        assigner.set_ibd_ready_peers(HashSet::from([
            "live0".into(),
            "live1".into(),
            "live2".into(),
            "live3".into(),
        ]));
        assigner.open_tip_owner_slot();
        assert!(
            assigner.peer_ok_for_gap_race("live0"),
            "ready-only floor must admit top live worker"
        );
        assert_eq!(
            assigner.get_work("live0", 1000).map(|(s, _)| s),
            Some(901),
            "open tip must arm despite unready high scorers"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_idle_score_pollution_does_not_block_active_tip_owner() {
        // Live A6d: set_peer_scores(all network) injected idle peers at 1.0; tip workers
        // at ~0.2 failed peer_ok median → post-SLA covering=0 forever.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![
            (880, 1007),
            (1008, 1071),
            (1072, 1135),
            (1136, 1199),
        ];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["w0".into(), "w1".into(), "w2".into(), "w3".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        let mut scores = vec![
            ("w0".into(), 0.19),
            ("w1".into(), 0.18),
            ("w2".into(), 0.17),
            ("w3".into(), 0.16),
        ];
        for i in 0..40 {
            scores.push((format!("idle{i}:8333"), 1.0));
        }
        assigner.set_peer_scores(&scores);
        assigner.set_ibd_ready_peers(HashSet::from([
            "w0".into(),
            "w1".into(),
            "w2".into(),
            "w3".into(),
        ]));
        assigner.open_tip_owner_slot();
        assert_eq!(
            assigner.get_work("w0", 1000).map(|(s, _)| s),
            Some(901),
            "active worker at 0.19 must pass WAN peer_ok despite idle 1.0 pollution"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_open_slot_without_sticky_allows_any_active_ready_worker() {
        // Live A6d post-SLA: preferred=None + sole top_w gate → deadlock if top_w not polling.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007), (1008, 1071)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["aaaa".into(), "zzzz".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        // Equal scores: lex-max "zzzz" would be sole top_w under old gate.
        assigner.set_peer_scores(&[
            ("aaaa".into(), 0.2),
            ("zzzz".into(), 0.2),
            ("mid".into(), 0.15),
            ("low".into(), 0.1),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from(["aaaa".into(), "zzzz".into()]));
        assigner.open_tip_owner_slot();
        assert_eq!(
            assigner.get_work("aaaa", 1000).map(|(s, _)| s),
            Some(901),
            "non-top_w active ready worker must re-arm open tip slot after SLA"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p0a_tip_owner_open_denies_score_zero_bottom_half() {
        // Live regression: tip_owner_open lotteried score=0 ready peers → ~2 blk/s.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![(880, 1007)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec!["top".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            ("top".into(), 9.0),
            ("good".into(), 8.0),
            ("mid".into(), 5.0),
            ("zero".into(), 0.0),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "top".into(),
            "good".into(),
            "mid".into(),
            "zero".into(),
        ]));
        assigner.open_tip_owner_slot();
        assert!(
            assigner.get_work("zero", 1000).is_none(),
            "open tip slot must not assign bottom-half score=0 peers"
        );
        assert_eq!(
            assigner.get_work("top", 1000).map(|(s, _)| s),
            Some(901),
            "open tip slot must still assign top-half ready peer"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    fn test_set_sticky_tenure(assigner: &ChunkAssigner, peer: &str, start_h: u64, ago_secs: u64) {
        *assigner.preferred_tip_owner.lock().unwrap() = Some(peer.to_string());
        *assigner.sticky_wan_tenure.lock().unwrap() = Some(StickyWanTenure {
            peer: peer.to_string(),
            start_next_needed: start_h,
            started_at: Instant::now() - Duration::from_secs(ago_secs),
        });
    }

    fn test_push_tip_sample(assigner: &ChunkAssigner, next_needed: u64, ago_secs: u64) {
        assigner
            .tip_progress_samples
            .lock()
            .unwrap()
            .push_back((Instant::now() - Duration::from_secs(ago_secs), next_needed));
    }

    #[test]
    fn a6n_rotates_to_tip_stream_peer_not_bulk_hero() {
        use crate::network::peer_scoring::PeerScorer;

        super::super::tip_stage::clear_tip_failover();
        let slow = "10.0.0.1:8333";
        let tip_fast = "10.0.0.2:8333";
        let bulk_hero = "10.0.0.3:8333";
        let vh = Arc::new(AtomicU64::new(999));
        let chunks = vec![(880, 1007), (1008, 1071), (1072, 1135)];
        let assigner = ChunkAssigner::new(
            chunks,
            vec![slow.into(), tip_fast.into(), bulk_hero.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[
            (slow.into(), 0.10),
            (tip_fast.into(), 0.11),
            (bulk_hero.into(), 0.19),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            slow.into(),
            tip_fast.into(),
            bulk_hero.into(),
        ]));
        test_set_sticky_tenure(&assigner, slow, 901, 301);

        // Simulate tip streams: sticky slow, tip_fast clearly better. Bulk hero has none.
        for _ in 0..10 {
            assigner.note_wan_tip_stream(slow);
        }
        for _ in 0..80 {
            assigner.note_wan_tip_stream(tip_fast);
        }

        let scorer = PeerScorer::new();
        // Bulk hero would win on lifetime delivery_blocks_per_sec — must be ignored.
        let bulk_addr: std::net::SocketAddr = bulk_hero.parse().unwrap();
        for _ in 0..500 {
            scorer.record_block(bulk_addr, 500_000, 10.0);
        }

        assert!(
            assigner.maybe_rotate_slow_sticky_a6m(1000, &scorer),
            "A6n must rotate when a tip-proven faster peer exists"
        );
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some(tip_fast),
            "must pick tip-stream peer, not bulk IBD hero"
        );
        assert_ne!(
            assigner.preferred_tip_owner().as_deref(),
            Some(bulk_hero),
            "lifetime bulk hero must not win tip ownership"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn w35ppph_clips_tip_pipe_to_header_tip() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let chunks = vec![
            (880, 1007),
            (1008, 1135),
            (1136, 1263),
            (1264, 1391),
        ];
        let assigner = ChunkAssigner::new(
            chunks,
            vec![
                "sticky".into(),
                "other".into(),
                "mid".into(),
                "low".into(),
            ],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_header_tip(920); // only 20 headers past tip
        assigner.set_peer_scores(&[
            ("sticky".into(), 0.100),
            ("other".into(), 0.195),
            ("mid".into(), 0.190),
            ("low".into(), 0.185),
        ]);
        assigner.set_ibd_ready_peers(HashSet::from([
            "sticky".into(),
            "other".into(),
            "mid".into(),
            "low".into(),
        ]));
        assigner.set_tip_gap_missing(true);
        assigner.note_tip_owner_assigned("sticky");

        let first = assigner.get_work("sticky", 256).expect("tip assign");
        assert_eq!(first.0, 901);
        assert_eq!(
            first.1, 920,
            "must clip tip pipe to header tip, got {}-{}",
            first.0, first.1
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn nudge_clears_blacklists_when_ready_active_zero() {
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec!["w0".into(), "w1".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_header_tip(1200);
        assigner.set_peer_scores(&[("w0".into(), 0.2), ("w1".into(), 0.19)]);
        assigner.set_ibd_ready_peers(HashSet::from(["w0".into(), "w1".into()]));
        assigner.blacklist_peer("w0", Duration::from_secs(300));
        assigner.blacklist_peer("w1", Duration::from_secs(300));
        assert!(assigner.is_peer_blacklisted("w0"));
        assert!(assigner.nudge_wan_tip_owner());
        assert!(
            !assigner.is_peer_blacklisted("w0"),
            "nudge must clear active blacklists when covering=0 and ready_active=0"
        );
        assert!(!assigner.is_peer_blacklisted("w1"));
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn nudge_pins_top_w_when_covering_zero_preferred_none() {
        // Live 2026-07-16: OPEN_STALL preferred=None + top_w_ok left covering=0 for ~18 min.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec!["w0".into(), "w1".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_header_tip(1200);
        assigner.set_peer_scores(&[("w0".into(), 0.100), ("w1".into(), 0.201)]);
        assigner.set_ibd_ready_peers(HashSet::from(["w0".into(), "w1".into()]));
        assigner.set_tip_gap_missing(true);
        assert!(assigner.preferred_tip_owner().is_none());
        assert!(assigner.nudge_wan_tip_owner());
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("w1"),
            "covering=0 nudge must pin top scored ready worker"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn wan10k_replacement_peer_must_register_to_take_tip() {
        // Live wan10k-c4 @438479:
        //   TIP_CRAWL ready=2 covering=0 busy=0
        //   OPEN_STALL preferred=None top_w=None ready_active_ok=0/0 score_keys=2
        //   CHEESE: tip missing, ahead in reorder
        // Peer watcher spawned replacements that polled get_work but were never added to
        // assigner.workers. Open-slot + tip_sticky_usable require is_active_download_worker
        // → tip hole forever while handshake-ready peers existed.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        let stale = "35.182.131.76:8333";
        let repl = "188.214.129.139:8333";
        let vh = Arc::new(AtomicU64::new(438_478));
        let assigner = ChunkAssigner::new(
            vec![(437_309, 500_000)],
            vec![stale.into()], // construction-time workers only
            Arc::clone(&vh),
            437_309,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(437_309);
        assigner.set_wan_body_tip(437_309);
        assigner.set_header_tip(500_000);
        assigner.set_tip_gap_missing(true);
        // Stale tip hero: fail-cooled (mute CAP) and not ready — score still in map.
        assigner.set_peer_scores(&[(stale.into(), 466.0), (repl.into(), 400.0)]);
        assigner.mark_tip_owner_fail_cooldown(stale, 120);
        assigner.set_ibd_ready_peers(HashSet::from([repl.into()]));
        assigner.open_tip_owner_slot();

        assert!(
            !assigner.is_active_download_worker(repl),
            "replacement must start outside construction workers"
        );
        assert!(
            assigner.get_work(repl, 256).is_none(),
            "unregistered replacement must not win tip (ready_active_ok=0/0 freeze)"
        );

        assigner.register_download_worker(repl);
        assert!(assigner.is_active_download_worker(repl));
        assert!(
            assigner.nudge_wan_tip_owner(),
            "covering=0 nudge must run after register"
        );
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some(repl),
            "nudge must pin registered ready replacement, not cooled stale hero"
        );
        let work = assigner.get_work(repl, 256);
        assert!(
            work.is_some_and(|(s, _)| s == 438_479),
            "registered replacement must cover tip hole, got {:?}",
            work
        );
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn covering0_tip_pin_uncools_when_every_hero_fail_cooled() {
        // Live wan10k @438022: mute CAP → mid_clear=0 → OPEN_STALL preferred=None
        // top_w=None while score_keys=2 (both fail-cooled). E15 existed for GD_SLOW OPEN
        // only; covering=0 TIP_PIN must clear cooldowns and pin.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        let a = "35.182.131.76:8333";
        let b = "188.214.129.139:8333";
        let vh = Arc::new(AtomicU64::new(438_021));
        let assigner = ChunkAssigner::new(
            vec![(437_309, 500_000)],
            vec![a.into(), b.into()],
            Arc::clone(&vh),
            437_309,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(437_309);
        assigner.set_wan_body_tip(437_309);
        assigner.set_header_tip(500_000);
        assigner.set_tip_gap_missing(true);
        assigner.set_peer_scores(&[(a.into(), 466.0), (b.into(), 400.0)]);
        assigner.set_ibd_ready_peers(HashSet::from([a.into(), b.into()]));
        assigner.mark_tip_owner_fail_cooldown(a, 120);
        assigner.mark_tip_owner_fail_cooldown(b, 120);
        assert!(assigner.preferred_tip_owner().is_none());
        assert!(assigner.nudge_wan_tip_owner());
        let pref = assigner.preferred_tip_owner();
        assert!(
            pref.as_deref() == Some(a) || pref.as_deref() == Some(b),
            "covering=0 must uncool and pin a tip hero, got {:?}",
            pref
        );
        assert!(
            !assigner.tip_owner_in_fail_cooldown(pref.as_deref().unwrap()),
            "pinned hero must leave fail-cooldown"
        );
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn dead_sticky_allows_force_requeue_tip_micro_to_ready_worker() {
        // Live wan10k: preferred=disconnected hero → peer_may_take_wan_gap_retry only
        // matched that peer → FORCE_REQUEUE (H,H) never assigned while covering=0.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        let dead = "35.182.131.76:8333";
        let live = "188.214.129.139:8333";
        let vh = Arc::new(AtomicU64::new(438_478));
        let assigner = ChunkAssigner::new(
            vec![(437_309, 500_000)],
            vec![dead.into(), live.into()],
            Arc::clone(&vh),
            437_309,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(437_309);
        assigner.set_wan_body_tip(437_309);
        assigner.set_header_tip(500_000);
        assigner.set_tip_gap_missing(true);
        assigner.set_peer_scores(&[(dead.into(), 466.0), (live.into(), 400.0)]);
        // Dead sticky still preferred; only `live` is handshake-ready.
        {
            let mut g = assigner.preferred_tip_owner.lock().unwrap();
            *g = Some(dead.into());
        }
        assigner.set_ibd_ready_peers(HashSet::from([live.into()]));
        assert!(!assigner.tip_sticky_usable(dead));
        assigner.requeue_stall_gaps_force(438_479, None);
        let work = assigner.get_work(live, 256);
        assert!(
            work.is_some_and(|(s, _)| s == 438_479),
            "living ready worker must cover tip after dead sticky drop (retry micro or tip stripe), got {:?}",
            work
        );
        assert!(
            assigner.preferred_tip_owner().as_deref() != Some(dead),
            "dead sticky must be cleared"
        );
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn covering0_blacklist_clear_includes_registered_replacements() {
        super::super::tip_stage::clear_tip_failover();
        let stale = "stale:8333";
        let repl = "repl:8333";
        let vh = Arc::new(AtomicU64::new(900));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007)],
            vec![stale.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_wan_body_tip(800);
        assigner.set_header_tip(1200);
        assigner.set_tip_gap_missing(true);
        assigner.register_download_worker(repl);
        assigner.set_peer_scores(&[(repl.into(), 0.40)]);
        assigner.set_ibd_ready_peers(HashSet::from([repl.into()]));
        assigner.blacklist_peer(repl, Duration::from_secs(300));
        assert!(assigner.is_peer_blacklisted(repl));
        assert!(assigner.nudge_wan_tip_owner());
        assert!(
            !assigner.is_peer_blacklisted(repl),
            "covering=0 ready_active=0 must clear blacklists on registered replacements"
        );
        assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(repl));
        super::super::tip_stage::clear_tip_failover();
    }

    /// Live 2026-07-14 genesis stall: confirmed=0 while live bodies existed at 64.
    /// Old `wan_tip_gap_crawl` required `confirmed > 0` → always false → nudge no-op.
    /// New path gates on `wan_body_tip` (coordinator live tip).
    #[test]
    fn genesis_confirmed_zero_uses_wan_body_tip_for_crawl() {
        let vh = Arc::new(AtomicU64::new(512));
        let assigner = ChunkAssigner::new(
            vec![(1, 64), (65, 128), (513, 576)],
            vec!["pA".into(), "pB".into(), "pC".into()],
            Arc::clone(&vh),
            1,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(0);
        // Empty store (wan_body_tip=0): past tip is WAN crawl (true genesis download).
        assert!(assigner.wan_tip_gap_crawl(513));

        // Live tip raised to 64 (GAP_PERSIST race) — still WAN for next=513.
        assigner.set_wan_body_tip(64);
        assert!(assigner.wan_tip_gap_crawl(513));
        assert!(
            !assigner.wan_tip_gap_crawl(64),
            "at body tip boundary must not be WAN tip crawl"
        );
        // W84: tip height allowed; far-ahead height still suppressed.
        vh.store(512, Ordering::Relaxed);
        assert!(
            assigner.wan_stall_micro_allowed(513),
            "W84: WAN tip height must allow stall micro recovery"
        );
        assert!(
            !assigner.wan_stall_micro_allowed(600),
            "WAN tip crawl must still suppress ahead stall micro storms"
        );
        assert!(assigner.nudge_wan_tip_owner());
    }

    #[test]
    fn w84_wan_stall_micro_allows_tip_height_only() {
        let vh = Arc::new(AtomicU64::new(256_686));
        let assigner = ChunkAssigner::new(
            vec![(256_687, 256_750)],
            vec!["pA".into()],
            Arc::clone(&vh),
            1,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(0);
        assert!(assigner.wan_tip_gap_crawl(256_687));
        assert!(
            assigner.wan_stall_micro_allowed(256_687),
            "exact tip must requeue on stall (live freeze 256687)"
        );
        assert!(
            !assigner.wan_stall_micro_allowed(256_800),
            "ahead of tip must stay suppressed"
        );
    }

    #[test]
    fn chunk_guard_drop_clears_matching_range_not_lifo() {
        let vh = Arc::new(AtomicU64::new(100));
        let assigner = Arc::new(ChunkAssigner::new(
            vec![(101, 164), (165, 228)],
            vec!["p1".into(), "p1".into()],
            Arc::clone(&vh),
            101,
            true,
        ));
        assigner.mark_bootstrap_complete();
        assigner.set_peer_scores(&[("p1".into(), 1.0)]);
        // Force dual in-flight capacity.
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            ChunkAssigner::insert_in_flight(&mut g, "p1", 101, 164);
            ChunkAssigner::insert_in_flight(&mut g, "p1", 165, 228);
        }
        {
            let mut guard = ChunkGuard::new(165, 228, None, "p1".into(), Arc::clone(&assigner));
            // Drop without disarm — must clear 165-228, leave 101-164.
            drop(guard);
        }
        let g = assigner.in_flight_per_peer.lock().unwrap();
        let ranges = g.get("p1").cloned().unwrap_or_default();
        assert_eq!(ranges, vec![(101, 164)]);
    }

    #[test]
    fn a6n_opens_slot_when_no_tip_proven_candidate() {
        use crate::network::peer_scoring::PeerScorer;

        super::super::tip_stage::clear_tip_failover();
        let slow = "10.0.0.1:8333";
        let bulk = "10.0.0.9:8333";
        let vh = Arc::new(AtomicU64::new(999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec![slow.into(), bulk.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[(slow.into(), 0.10), (bulk.into(), 0.19)]);
        assigner.set_ibd_ready_peers(HashSet::from([slow.into(), bulk.into()]));
        test_set_sticky_tenure(&assigner, slow, 901, 301);
        for _ in 0..5 {
            assigner.note_wan_tip_stream(slow);
        }
        // bulk has zero tip streams
        let scorer = PeerScorer::new();
        let bulk_addr: std::net::SocketAddr = bulk.parse().unwrap();
        for _ in 0..200 {
            scorer.record_block(bulk_addr, 500_000, 10.0);
        }
        assert!(assigner.maybe_rotate_slow_sticky_a6m(1000, &scorer));
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some(bulk),
            "no tip-proven candidate → open slot pinned to top scored ready worker (not None lottery)"
        );
        assert!(assigner.is_peer_blacklisted(slow));
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn e16_a6m_gd_slow_keeps_sticky_when_feeder_runway() {
        // C1u @320k: tip_bps≈179 + feeder≈18 + gd_ewma≈5.9s must NOT OPEN/blacklist.
        use crate::network::peer_scoring::PeerScorer;

        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::test_seed_getdata_body_ewma(5_900, 32);
        super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(18, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        unsafe {
            std::env::set_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS", "30");
            std::env::set_var("BLVM_IBD_A6M_ROTATE_COOLDOWN", "0");
            std::env::set_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN", "0");
            std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
            std::env::set_var("BLVM_IBD_A6M_GD_SLOW_FEEDER_KEEP", "8");
            // Disable tip_bps keep so this test isolates feeder keep.
            std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "0");
        }
        let slow = "10.0.0.1:8333";
        let alt = "10.0.0.2:8333";
        let vh = Arc::new(AtomicU64::new(999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec![slow.into(), alt.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_tip_gap_missing(true);
        assigner.set_peer_scores(&[(slow.into(), 0.10), (alt.into(), 0.19)]);
        assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
        // tip_bps ≈ (2201-901)/30 ≈ 43 ≥ min 40; tenure full window.
        test_set_sticky_tenure(&assigner, slow, 901, 30);
        test_push_tip_sample(&assigner, 901, 30);
        test_push_tip_sample(&assigner, 2201, 0);
        for _ in 0..40 {
            assigner.note_wan_tip_stream(slow);
        }
        for _ in 0..5 {
            assigner.note_wan_tip_stream(alt);
        }
        let scorer = PeerScorer::new();
        assert!(
            !assigner.maybe_rotate_slow_sticky_a6m(2201, &scorer),
            "E16: feeder runway must keep sticky despite GD_SLOW EWMA"
        );
        assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(slow));
        assert!(!assigner.is_peer_blacklisted(slow));

        // feeder=0 → LOCAL_GAP path may still rotate (tip_bps keep off).
        *assigner.last_a6m_rotate_at.lock().unwrap() = None;
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        assert!(
            assigner.maybe_rotate_slow_sticky_a6m(2201, &scorer),
            "feeder=0 + GD_SLOW + tip_bps≥min must still rotate (E11)"
        );
        assert_ne!(assigner.preferred_tip_owner().as_deref(), Some(slow));

        unsafe {
            std::env::remove_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS");
            std::env::remove_var("BLVM_IBD_A6M_ROTATE_COOLDOWN");
            std::env::remove_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN");
            std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
            std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_FEEDER_KEEP");
            std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        }
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn e16b_a6m_gd_slow_keeps_on_tip_bps_when_feeder_dips() {
        // Live C1u-e16: KEEP@feeder=29 then OPEN at feeder=5 tip_bps=162 ewma=554.
        use crate::network::peer_scoring::PeerScorer;

        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::test_seed_getdata_body_ewma(554, 32);
        super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(5, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        unsafe {
            std::env::set_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS", "30");
            std::env::set_var("BLVM_IBD_A6M_ROTATE_COOLDOWN", "0");
            std::env::set_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN", "0");
            std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
            std::env::set_var("BLVM_IBD_A6M_GD_SLOW_FEEDER_KEEP", "8");
            std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
        }
        let slow = "10.0.0.1:8333";
        let alt = "10.0.0.2:8333";
        let vh = Arc::new(AtomicU64::new(999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec![slow.into(), alt.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_tip_gap_missing(true);
        assigner.set_peer_scores(&[(slow.into(), 0.10), (alt.into(), 0.19)]);
        assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
        // tip_bps ≈ (5801-901)/30 ≈ 163 ≥ tip_keep 80; feeder=5 < feeder_keep 8.
        test_set_sticky_tenure(&assigner, slow, 901, 30);
        test_push_tip_sample(&assigner, 901, 30);
        test_push_tip_sample(&assigner, 5801, 0);
        for _ in 0..40 {
            assigner.note_wan_tip_stream(slow);
        }
        let scorer = PeerScorer::new();
        assert!(
            !assigner.maybe_rotate_slow_sticky_a6m(5801, &scorer),
            "E16b: tip_bps keep must hold when feeder dips below feeder_keep"
        );
        assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(slow));
        assert!(!assigner.is_peer_blacklisted(slow));

        unsafe {
            std::env::remove_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS");
            std::env::remove_var("BLVM_IBD_A6M_ROTATE_COOLDOWN");
            std::env::remove_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN");
            std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
            std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_FEEDER_KEEP");
            std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        }
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn a6m_mute_fast_bypasses_tenure_window_when_feeder_empty_gd_slow() {
        // Mute-fast Phase 1: feeder=0 ∧ gap ∧ gd_slow skips 0.8×window (default ≥24s).
        use crate::network::peer_scoring::PeerScorer;

        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::test_seed_getdata_body_ewma(900, 32);
        super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        unsafe {
            std::env::set_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS", "30");
            std::env::set_var("BLVM_IBD_A6M_ROTATE_COOLDOWN", "0");
            std::env::set_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN", "0");
            std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
        }
        let slow = "10.0.0.1:8333";
        let alt = "10.0.0.2:8333";
        let vh = Arc::new(AtomicU64::new(999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec![slow.into(), alt.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_tip_gap_missing(true);
        assigner.set_peer_scores(&[(slow.into(), 0.10), (alt.into(), 0.19)]);
        assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
        // Only 5s tenure — classic A6m would return false (< 0.8×30 = 24s).
        test_set_sticky_tenure(&assigner, slow, 901, 5);
        test_push_tip_sample(&assigner, 901, 5);
        test_push_tip_sample(&assigner, 910, 0);
        for _ in 0..5 {
            assigner.note_wan_tip_stream(slow);
        }
        for _ in 0..25 {
            assigner.note_wan_tip_stream(alt);
        }
        let scorer = PeerScorer::new();
        assert!(
            assigner.maybe_rotate_slow_sticky_a6m(910, &scorer),
            "mute-fast must rotate at elapsed=5s when feeder=0 + gd_slow"
        );
        assert_ne!(assigner.preferred_tip_owner().as_deref(), Some(slow));

        // Healthy feeder + healthy gd → still gated by tenure at elapsed=5s.
        *assigner.last_a6m_rotate_at.lock().unwrap() = None;
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::test_seed_getdata_body_ewma(100, 32);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(32, Ordering::Relaxed);
        test_set_sticky_tenure(&assigner, slow, 901, 5);
        test_push_tip_sample(&assigner, 901, 5);
        test_push_tip_sample(&assigner, 920, 0);
        assert!(
            !assigner.maybe_rotate_slow_sticky_a6m(920, &scorer),
            "feeder>0 + healthy gd must still require 0.8×window tenure"
        );

        unsafe {
            std::env::remove_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS");
            std::env::remove_var("BLVM_IBD_A6M_ROTATE_COOLDOWN");
            std::env::remove_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN");
            std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
        }
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn a6m_mute_fast_slow_drip_without_gap_missing() {
        // Live Phase4: covering=1 drip clears tip_gap_missing; await≈0; classic mute-fast
        // never armed. feeder=0 ∧ gd_slow ∧ covering≥1 must still rotate early.
        use crate::network::peer_scoring::PeerScorer;

        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::test_seed_getdata_body_ewma(900, 32);
        super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        unsafe {
            std::env::set_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS", "30");
            std::env::set_var("BLVM_IBD_A6M_ROTATE_COOLDOWN", "0");
            std::env::set_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN", "0");
            std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
        }
        let slow = "10.0.0.1:8333";
        let alt = "10.0.0.2:8333";
        let vh = Arc::new(AtomicU64::new(999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec![slow.into(), alt.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_tip_gap_missing(false); // drip cleared gap
        assigner.note_tip_cover_claim(slow, 901, 1028); // covering=1
        assigner.set_peer_scores(&[(slow.into(), 0.10), (alt.into(), 0.19)]);
        assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
        test_set_sticky_tenure(&assigner, slow, 901, 5);
        test_push_tip_sample(&assigner, 901, 5);
        test_push_tip_sample(&assigner, 910, 0);
        for _ in 0..5 {
            assigner.note_wan_tip_stream(slow);
        }
        for _ in 0..25 {
            assigner.note_wan_tip_stream(alt);
        }
        let scorer = PeerScorer::new();
        assert!(
            assigner.maybe_rotate_slow_sticky_a6m(910, &scorer),
            "slow-drip mute-fast must rotate with gap=false covering=1 gd_slow"
        );
        assert_ne!(assigner.preferred_tip_owner().as_deref(), Some(slow));
        unsafe {
            std::env::remove_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS");
            std::env::remove_var("BLVM_IBD_A6M_ROTATE_COOLDOWN");
            std::env::remove_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN");
            std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
        }
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn a6m_rotates_when_getdata_ewma_slow_despite_fast_tip_bps() {
        // E11: tip-advance BPS ≥ min (LOCAL_GAP) while getdata→body EWMA stays slow.
        // E13: must pin a different ready peer + tip-owner cooldown (E12 re-elect bug).
        use crate::network::peer_scoring::PeerScorer;

        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::test_seed_getdata_body_ewma(1_500, 32);
        let slow = "10.0.0.1:8333";
        let alt = "10.0.0.2:8333";
        let vh = Arc::new(AtomicU64::new(9999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec![slow.into(), alt.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[(slow.into(), 1.3), (alt.into(), 1.2)]);
        assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
        // Lifetime tip advance ≈ 50 blk/s ≫ min_bps=6 — old A6m would skip.
        test_set_sticky_tenure(&assigner, slow, 1000, 200);
        test_push_tip_sample(&assigner, 9000, 90);
        test_push_tip_sample(&assigner, 10000, 0);
        for _ in 0..5 {
            assigner.note_wan_tip_stream(slow);
        }
        // Alt tip-stream BPS = notes/max(1s) — need ≥ FORCE min (default 20).
        for _ in 0..25 {
            assigner.note_wan_tip_stream(alt);
        }
        let scorer = PeerScorer::new();
        assert!(
            assigner.maybe_rotate_slow_sticky_a6m(10000, &scorer),
            "slow getdata EWMA must arm A6m even when tip-advance BPS looks healthy"
        );
        assert!(assigner.is_peer_blacklisted(slow));
        assert!(
            assigner.tip_owner_in_fail_cooldown(slow),
            "GD_SLOW must tip-owner-cooldown sticky so TIP_PIN cannot re-elect"
        );
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some(alt),
            "GD_SLOW must pin a different ready peer (E12 pinned=None re-elect)"
        );
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn a6m_gd_slow_open_uncools_prior_hero_when_pin_empty() {
        // E15: ROTATE A→B blacklists+cools A 180s; OPEN on B 60s later pinned=None.
        use crate::network::peer_scoring::PeerScorer;

        super::super::tip_stage::clear_tip_failover();
        let a = "10.0.0.1:8333";
        let b = "10.0.0.2:8333";
        let vh = Arc::new(AtomicU64::new(9999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec![a.into(), b.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[(a.into(), 1.3), (b.into(), 1.2)]);
        assigner.set_ibd_ready_peers(HashSet::from([a.into(), b.into()]));
        // Simulate post-ROTATE: A blacklisted + tip-owner cooled; B is sticky.
        assigner.blacklist_peer(a, Duration::from_secs(120));
        assigner.mark_tip_owner_fail_cooldown(a, 180);
        test_set_sticky_tenure(&assigner, b, 1000, 200);
        test_push_tip_sample(&assigner, 9000, 90);
        test_push_tip_sample(&assigner, 10000, 0);
        for _ in 0..5 {
            assigner.note_wan_tip_stream(b);
        }
        // A has tip streams but is cooled/blacklisted until OPEN retry clears.
        for _ in 0..30 {
            assigner.note_wan_tip_stream(a);
        }
        let scorer = PeerScorer::new();
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::test_seed_getdata_body_ewma(1_500, 32);
        assert!(assigner.maybe_rotate_slow_sticky_a6m(10000, &scorer));
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some(a),
            "GD_SLOW OPEN must un-cool/un-blacklist prior tip hero to pin"
        );
        assert!(assigner.is_peer_blacklisted(b));
        assert!(assigner.tip_owner_in_fail_cooldown(b));
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn a6m_gd_slow_open_slot_pins_ready_worker_not_in_score_map() {
        // E12: top_scored walked peer_scores only → pinned=None while another download
        // worker was ready. Fallback must pin via active-worker walk.
        // Sequential with EWMA seed immediately before rotate (tip_stage statics).
        use crate::network::peer_scoring::PeerScorer;

        super::super::tip_stage::clear_tip_failover();
        let slow = "10.0.0.1:8333";
        let alt = "10.0.0.2:8333";
        let vh = Arc::new(AtomicU64::new(9999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec![slow.into(), alt.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        // Only sticky scored — alt ready but absent from peer_scores map.
        assigner.set_peer_scores(&[(slow.into(), 1.3)]);
        assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
        test_set_sticky_tenure(&assigner, slow, 1000, 200);
        test_push_tip_sample(&assigner, 9000, 90);
        test_push_tip_sample(&assigner, 10000, 0);
        for _ in 0..5 {
            assigner.note_wan_tip_stream(slow);
        }
        let scorer = PeerScorer::new();
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::test_seed_getdata_body_ewma(1_500, 32);
        assert!(
            assigner.maybe_rotate_slow_sticky_a6m(10000, &scorer),
            "GD_SLOW OPEN_SLOT must arm when tip BPS looks healthy"
        );
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some(alt),
            "OPEN_SLOT must fall back to any ready active worker when score-map pin is None"
        );
        assert!(assigner.tip_owner_in_fail_cooldown(slow));
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn a6m_mid_score_sticky_rotates_despite_soft_retry() {
        // E10: non-floor sticky@~1.3 + soft_retry>0 used to hard-block A6m (IBD_A6M=0).
        use crate::network::peer_scoring::PeerScorer;

        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::mark_needed(1000);
        super::super::tip_stage::mark_soft_retry(1000);
        assert!(super::super::tip_stage::tip_soft_retries() > 0);
        let slow = "10.0.0.1:8333";
        let alt = "10.0.0.2:8333";
        let vh = Arc::new(AtomicU64::new(999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec![slow.into(), alt.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        // Mid-band score (not floor 0.10) — the path E10 hit.
        assigner.set_peer_scores(&[(slow.into(), 1.3), (alt.into(), 1.2)]);
        assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
        test_set_sticky_tenure(&assigner, slow, 901, 301);
        for _ in 0..5 {
            assigner.note_wan_tip_stream(slow);
        }
        let scorer = PeerScorer::new();
        assert!(
            assigner.maybe_rotate_slow_sticky_a6m(1000, &scorer),
            "soft_retry must not block A6m on mid-score sticky"
        );
        assert!(assigner.is_peer_blacklisted(slow));
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::mark_needed(0);
    }

    #[test]
    fn a6m_opens_slot_on_recent_stall_despite_fast_lifetime() {
        // Live 2026-07-15: lifetime tenure ≥11 blk/s over 300s hid minute-scale stalls
        // (tip ~0.8 blk/s @ 04:08) — A6m never fired. Recent window must catch this.
        use crate::network::peer_scoring::PeerScorer;

        super::super::tip_stage::clear_tip_failover();
        let slow = "10.0.0.1:8333";
        let other = "10.0.0.2:8333";
        let vh = Arc::new(AtomicU64::new(9999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec![slow.into(), other.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[(slow.into(), 0.10), (other.into(), 0.11)]);
        assigner.set_ibd_ready_peers(HashSet::from([slow.into(), other.into()]));
        // Lifetime: 9000 blocks / 600s = 15 blk/s (≥ floor min 12) — old A6m would skip.
        test_set_sticky_tenure(&assigner, slow, 1000, 600);
        // Recent: only +40 blocks in 90s ≈ 0.44 blk/s.
        test_push_tip_sample(&assigner, 9960, 90);
        test_push_tip_sample(&assigner, 10000, 0);
        for _ in 0..50 {
            assigner.note_wan_tip_stream(slow);
        }
        // Other has tip streams but loses 1.25× bar (sticky monopoly) → must open slot.
        for _ in 0..5 {
            assigner.note_wan_tip_stream(other);
        }
        let scorer = PeerScorer::new();
        assert!(
            assigner.maybe_rotate_slow_sticky_a6m(10000, &scorer),
            "recent-window stall must rotate/open even when lifetime BPS looks healthy"
        );
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some(other),
            "bar-fail / true stall → open slot pinned to top scored ready worker"
        );
        assert!(assigner.is_peer_blacklisted(slow));
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn a6n_keeps_healthy_floor_sticky_when_no_tip_proven_alt() {
        // Live 2026-07-15: tenure_bps=12.57 OPEN_SLOT blacklisted a delivering sticky.
        use crate::network::peer_scoring::PeerScorer;

        super::super::tip_stage::clear_tip_failover();
        let sticky = "10.0.0.1:8333";
        let other = "10.0.0.2:8333";
        let vh = Arc::new(AtomicU64::new(9999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1071)],
            vec![sticky.into(), other.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[(sticky.into(), 0.10), (other.into(), 0.20)]);
        assigner.set_ibd_ready_peers(HashSet::from([sticky.into(), other.into()]));
        test_set_sticky_tenure(&assigner, sticky, 1000, 600);
        // Recent: +900 / 60s = 15 blk/s — below stretch floor_min=22, above open_slot_min=12.
        test_push_tip_sample(&assigner, 9100, 60);
        test_push_tip_sample(&assigner, 10000, 0);
        for _ in 0..40 {
            assigner.note_wan_tip_stream(sticky);
        }
        for _ in 0..3 {
            assigner.note_wan_tip_stream(other);
        }
        let scorer = PeerScorer::new();
        assert!(
            !assigner.maybe_rotate_slow_sticky_a6m(10000, &scorer),
            "healthy-band floor sticky must not open-slot without tip-proven alt"
        );
        assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(sticky));
        assert!(!assigner.is_peer_blacklisted(sticky));
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn a6m_keeps_sticky_when_recent_bps_healthy() {
        use crate::network::peer_scoring::PeerScorer;

        super::super::tip_stage::clear_tip_failover();
        let sticky = "10.0.0.1:8333";
        let vh = Arc::new(AtomicU64::new(9999));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007)],
            vec![sticky.into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_peer_scores(&[(sticky.into(), 0.10)]);
        assigner.set_ibd_ready_peers(HashSet::from([sticky.into()]));
        test_set_sticky_tenure(&assigner, sticky, 1000, 600);
        // Recent: +1800 blocks / 90s = 20 blk/s — below stretch floor_min=22 but ≥ open_slot_min=12.
        test_push_tip_sample(&assigner, 8200, 90);
        test_push_tip_sample(&assigner, 10000, 0);
        for _ in 0..20 {
            assigner.note_wan_tip_stream(sticky);
        }
        let scorer = PeerScorer::new();
        assert!(
            !assigner.maybe_rotate_slow_sticky_a6m(10000, &scorer),
            "healthy-band recent tip BPS must not open-slot without tip-proven alt"
        );
        assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(sticky));
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p1c_no_tip_repreempt_while_peer_holds_tip_inflight() {
        // P1c: sticky with tip in-flight must not get a second overlapping tip span
        // (max_in_flight=2 dual-pipe is ahead-only).
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        let vh = Arc::new(AtomicU64::new(900));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007), (1008, 1135), (1136, 1263)],
            vec!["owner".into(), "ahead".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_wan_body_tip(800);
        assigner.set_header_tip(2000);
        assigner.set_peer_scores(&[("owner".into(), 0.50), ("ahead".into(), 0.40)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        let tip = assigner.get_work("owner", 1000).expect("tip owner");
        assert_eq!(tip.0, 901);
        assert!(
            ChunkAssigner::peer_holds_tip_inflight(
                &assigner.in_flight_per_peer.lock().unwrap(),
                "owner",
                901
            ),
            "owner must hold tip in-flight after assign"
        );
        let again = assigner.get_work("owner", 1000);
        if let Some((s, e)) = again {
            assert!(
                !(s <= 901 && 901 <= e),
                "P1c: must not re-preempt tip-covering span, got {s}-{e}"
            );
        }
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
    }

    #[test]
    fn wan_tip_dedup_blocks_same_span_reassign_after_gap_stream() {
        // WAN (not synth): obsolete→complete clears in_flight; P1c alone cannot stop
        // W28c same-start storms (live dens-hash160: same_start p50≈19ms).
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
        unsafe {
            std::env::remove_var("BLVM_IBD_SYNTH_WAN");
            std::env::set_var("BLVM_IBD_TIP_DEDUP_REARM_MS", "60000");
        }
        assert!(!super::super::synthetic_wan::bulk_local_disk_stream());
        let vh = Arc::new(AtomicU64::new(300_287));
        let assigner = ChunkAssigner::new(
            vec![(300_288, 300_415)],
            vec!["hero".into(), "alt".into()],
            Arc::clone(&vh),
            300_288,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(300_000);
        assigner.set_wan_body_tip(300_000);
        assigner.set_header_tip(400_000);
        assigner.set_tip_gap_missing(true);
        assigner.set_peer_scores(&[("hero".into(), 1.0), ("alt".into(), 0.5)]);
        mark_scored_peers_ibd_ready(&assigner);
        let first = assigner.get_work("hero", 1000);
        assert!(
            first.is_some_and(|(s, _)| s == 300_288),
            "first tip-owner, got {first:?}"
        );
        let (fs, fe) = first.unwrap();
        assigner.on_chunk_complete_range("hero", fs, fe);
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(300_351, Ordering::Relaxed);
        assert!(
            assigner.tip_owner_blocked_by_dedup(300_288),
            "WAN DEDUP past tip must block tip-owner re-arm"
        );
        let second = assigner.get_work("hero", 1000);
        assert!(
            second.map(|(s, _)| s != 300_288).unwrap_or(true),
            "WAN: must not reassign tip-covering span after DEDUP, got {second:?}"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_DEDUP_REARM_MS");
            super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
            super::super::tip_stage::test_reset_tip_stage();
        }
    }

    #[test]
    fn sole_ready_peer_skips_tip_owner_fail_cooldown() {
        // Mode T: workers may be 6 slots but only one IBD-ready archive.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007)],
            vec![
                "solo".into(),
                "slot2".into(),
                "slot3".into(),
                "slot4".into(),
                "slot5".into(),
                "slot6".into(),
            ],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_wan_body_tip(800);
        assigner.set_header_tip(2000);
        assigner.set_peer_scores(&[("solo".into(), 1.0)]);
        assigner.set_ibd_ready_peers(HashSet::from(["solo".into()]));
        assigner.set_tip_gap_missing(true);
        *assigner.preferred_tip_owner.lock().unwrap() = Some("solo".into());
        assigner.note_tip_cover_claim("solo", 901, 1028);
        assigner.note_tip_owner_failed("solo");
        assert!(
            !assigner.tip_owner_in_fail_cooldown("solo"),
            "sole ready peer must not enter tip-owner fail cooldown"
        );
        assert!(
            assigner.preferred_tip_owner().is_none(),
            "sticky still cleared so tip slot can re-arm"
        );
        assert!(
            assigner.tip_owner_open.load(Ordering::Relaxed),
            "WAN tip slot must open for immediate sole-peer re-arm"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn a6m_gd_slow_open_keeps_sole_ready_sticky() {
        // tc65: A6N_OPEN_SLOT with no challenger must not blacklist the sole archive.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::test_reset_tip_stage();
        let vh = Arc::new(AtomicU64::new(401_190));
        let sticky = "127.0.0.1:18333";
        let assigner = ChunkAssigner::new(
            vec![(401_191, 401_318)],
            vec![sticky.into(), "slot2".into()],
            Arc::clone(&vh),
            401_191,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(400_287);
        assigner.set_wan_body_tip(400_287);
        assigner.set_header_tip(451_000);
        assigner.set_peer_scores(&[(sticky.into(), 1200.0)]);
        assigner.set_ibd_ready_peers(HashSet::from([sticky.into()]));
        assigner.set_tip_gap_missing(true);
        *assigner.preferred_tip_owner.lock().unwrap() = Some(sticky.into());
        test_set_sticky_tenure(&assigner, sticky, 401_000, 40);
        let rotated = assigner.a6m_do_rotate(401_191, sticky, 28.0, 40.0, false, true);
        assert!(
            !rotated,
            "sole ready peer must KEEP on GD_SLOW OPEN (no alternate)"
        );
        assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(sticky));
        assert!(
            !assigner.is_peer_blacklisted(sticky),
            "must not blacklist sole archive on OPEN with new=-"
        );
        assert!(
            !assigner.tip_owner_in_fail_cooldown(sticky),
            "must not cool sole archive on aborted OPEN"
        );
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::test_reset_tip_stage();
    }

    #[test]
    fn p1e_mute_fail_applies_long_tip_role_ban() {
        // P1e: mute path default ban ≥60s (tip-role), not the old 5s CAP cooldown.
        super::super::tip_stage::clear_tip_failover();
        let vh = Arc::new(AtomicU64::new(900));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007)],
            vec!["mute".into(), "alt".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_wan_body_tip(800);
        assigner.set_header_tip(2000);
        assigner.set_peer_scores(&[("mute".into(), 0.50), ("alt".into(), 0.40)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        *assigner.preferred_tip_owner.lock().unwrap() = Some("mute".into());
        {
            let mut g = assigner.in_flight_per_peer.lock().unwrap();
            g.insert("mute".into(), vec![(901, 1028)]);
        }
        assigner.note_tip_cover_claim("mute", 901, 1028);
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_OWNER_MUTE_COOLDOWN_SECS");
        }
        assigner.note_tip_owner_failed_mute("mute");
        assert!(
            assigner.tip_owner_in_fail_cooldown("mute"),
            "mute peer must be tip-role banned"
        );
        let until = assigner
            .tip_owner_fail_until
            .lock()
            .unwrap()
            .get("mute")
            .copied();
        let remaining = until
            .map(|t| t.saturating_duration_since(Instant::now()).as_secs())
            .unwrap_or(0);
        assert!(
            remaining >= 55,
            "P1e: mute tip-role ban remaining ≥55s (default 120), got {remaining}s"
        );
        assert!(
            assigner.preferred_tip_owner().is_none(),
            "mute clears preferred sticky"
        );
        super::super::tip_stage::clear_tip_failover();
    }

    #[test]
    fn p2_tip_trial_starts_on_slow_drip_without_await() {
        // covering=1 drip: gap=false, await≈0, gd_slow, crawl << min_bps → trial without await gate.
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::test_seed_getdata_body_ewma(2000, 32);
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_TRIAL");
            std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
            std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
            std::env::set_var("BLVM_IBD_TIP_SLOW_DRIP_WINDOW_SECS", "8");
            std::env::set_var("BLVM_IBD_A6M_MIN_BPS", "40");
            std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
        }
        let vh = Arc::new(AtomicU64::new(910));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007)],
            vec!["sticky".into(), "challenger".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_wan_body_tip(800);
        assigner.set_header_tip(2000);
        assigner.set_peer_scores(&[("sticky".into(), 0.50), ("challenger".into(), 0.40)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(false);
        assigner.note_tip_cover_claim("sticky", 901, 1028);
        *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
        assigner.reset_sticky_wan_tenure("sticky", 901);
        // reset_sticky clears samples — rebuild chronological crawl history.
        assigner.tip_progress_samples.lock().unwrap().clear();
        // ~9 blocks / 8s ≈ 1.1 BPS < min_bps.
        test_push_tip_sample(&assigner, 901, 8);
        test_push_tip_sample(&assigner, 910, 0);
        assert!(
            assigner.maybe_start_tip_trial(910),
            "slow-drip trial must start with await=0 gap=false"
        );
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("challenger")
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
            std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
            std::env::remove_var("BLVM_IBD_TIP_SLOW_DRIP_WINDOW_SECS");
            std::env::remove_var("BLVM_IBD_A6M_MIN_BPS");
            std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
        }
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::clear_tip_failover();
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    }

    #[test]
    fn p2_tip_trial_starts_when_feeder_empty_and_awaiting() {
        super::super::tip_stage::clear_tip_failover();
        super::super::tip_stage::clear_tip_ahead_soft_freeze();
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(901);
        super::super::tip_stage::test_backdate_awaiting_ms(5_000);
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_TRIAL");
            std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
            std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        }
        let vh = Arc::new(AtomicU64::new(900));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007)],
            vec!["sticky".into(), "challenger".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_wan_body_tip(800);
        assigner.set_header_tip(2000);
        assigner.set_peer_scores(&[("sticky".into(), 0.50), ("challenger".into(), 0.40)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
        assigner.reset_sticky_wan_tenure("sticky", 901);
        assert!(
            assigner.maybe_run_tip_trial(901),
            "P2: trial must start on feeder=0 + awaiting"
        );
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("challenger"),
            "challenger pinned for trial"
        );
        assert!(assigner.tip_trial.lock().unwrap().is_some());
        // Mid-trial: no finish yet.
        assert!(
            !assigner.maybe_run_tip_trial(901),
            "trial must not finish before TRIAL_SECS"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
            std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        }
        super::super::tip_stage::clear_tip_failover();
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    }

    #[test]
    fn p2_tip_trial_keeps_challenger_with_tip_streams() {
        super::super::tip_stage::clear_tip_failover();
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(901);
        super::super::tip_stage::test_backdate_awaiting_ms(5_000);
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_TRIAL");
            std::env::set_var("BLVM_IBD_TIP_TRIAL_SECS", "8");
            std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
            std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        }
        let vh = Arc::new(AtomicU64::new(900));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007)],
            vec!["sticky".into(), "challenger".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_wan_body_tip(800);
        assigner.set_header_tip(2000);
        assigner.set_peer_scores(&[("sticky".into(), 0.50), ("challenger".into(), 0.40)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
        assert!(assigner.maybe_start_tip_trial(901));
        // Challenger delivered tip streams during trial; sticky delivered none.
        for _ in 0..3 {
            assigner.note_wan_tip_stream("challenger");
        }
        // Backdate trial start so finish fires.
        if let Some(ref mut t) = *assigner.tip_trial.lock().unwrap() {
            t.started = Instant::now() - Duration::from_secs(9);
        }
        assert!(assigner.maybe_finish_tip_trial(910));
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("challenger"),
            "P2 KEEP when challenger tip-streams and sticky does not"
        );
        assert!(assigner.tip_trial.lock().unwrap().is_none());
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_TRIAL_SECS");
            std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
            std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        }
        super::super::tip_stage::clear_tip_failover();
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    }

    #[test]
    fn p2_tip_trial_reverts_when_challenger_silent() {
        super::super::tip_stage::clear_tip_failover();
        super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(901);
        super::super::tip_stage::test_backdate_awaiting_ms(5_000);
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_TRIAL");
            std::env::set_var("BLVM_IBD_TIP_TRIAL_SECS", "8");
            std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
            std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        }
        let vh = Arc::new(AtomicU64::new(900));
        let assigner = ChunkAssigner::new(
            vec![(880, 1007)],
            vec!["sticky".into(), "challenger".into()],
            Arc::clone(&vh),
            880,
            true,
        );
        assigner.mark_bootstrap_complete();
        assigner.set_confirmed_body_height_at_start(800);
        assigner.set_wan_body_tip(800);
        assigner.set_header_tip(2000);
        assigner.set_peer_scores(&[("sticky".into(), 0.50), ("challenger".into(), 0.40)]);
        mark_scored_peers_ibd_ready(&assigner);
        assigner.set_tip_gap_missing(true);
        *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
        assert!(assigner.maybe_start_tip_trial(901));
        if let Some(ref mut t) = *assigner.tip_trial.lock().unwrap() {
            t.started = Instant::now() - Duration::from_secs(9);
        }
        assert!(assigner.maybe_finish_tip_trial(901));
        assert_eq!(
            assigner.preferred_tip_owner().as_deref(),
            Some("sticky"),
            "P2 REVERT when challenger delivers nothing"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_TRIAL_SECS");
            std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
            std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        }
        super::super::tip_stage::clear_tip_failover();
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    }
}
