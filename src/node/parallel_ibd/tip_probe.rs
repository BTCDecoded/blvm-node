//! Non-owner IBD probe (endgame §1).
//!
//! Background GetData on peers that do **not** hold tip. Bodies never enter
//! reorder / bridge / feeder. Rank is a separate axis from owner B1 EWMA.
//!
//! Wave-implied BPS = `32 * 1000 / sojourn_ms` (C1p grow depth). ≥80 iff
//! sojourn ≤ 400 ms. LIMITED last-288 is a different class and does not
//! share this rank (IBD peer set is already NODE_NETWORK).
//!
//! Default **off**. Enable: `BLVM_IBD_TIP_PROBE=1` / `true` / `on`.
//! Mesh abort: `BLVM_IBD_PROBE_MESH_ABORT`
//! (default on). Do not restore dest-aq 180s keep-hot if this is reverted.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::network::NetworkManager;
use crate::node::parallel_ibd::chunk_assigner::ChunkAssigner;
use crate::storage::blockstore::BlockStore;
use crate::storage::hashing::double_sha256;

/// C1p grow depth used to turn one-block sojourn into implied tip BPS.
const WAVE_DEPTH: f64 = 32.0;
const KEEP_BPS: f64 = 80.0;
/// Used only when `next_needed+1000` clamped to `header_tip-144` is unavailable.
const PROBE_HEIGHT_FALLBACK: [u64; 4] = [180_000, 100_000, 50_000, 10_000];
const PROBE_STALE_SECS_DEFAULT: u64 = 300;
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
const PER_PEER_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INFLIGHT: usize = 4;

#[derive(Debug, Clone)]
struct ProbeSample {
    ewma_ms: u64,
    n: u64,
    wave_bps: f64,
    last_ok: Option<Instant>,
    last_fail: Option<Instant>,
    inflight: bool,
}

fn table() -> &'static Mutex<HashMap<String, ProbeSample>> {
    static M: OnceLock<Mutex<HashMap<String, ProbeSample>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

static FIRST_PROBE_MS: AtomicU64 = AtomicU64::new(0);
static MESH_FAIL: AtomicBool = AtomicBool::new(false);
static PROBE_OK: AtomicU64 = AtomicU64::new(0);
static PROBE_FAIL: AtomicU64 = AtomicU64::new(0);
/// Last validation height seen by the probe loop, and when it increased.
/// dest-be 05:23:18: MESH_ABORT `request_shutdown` at vh=360583/end=963953
/// while crawl was still advancing — workers exited, coordinator drained
/// reorder, then sat covering=0 on LOCAL_GAP 360635.
static LAST_PROGRESS_VH: AtomicU64 = AtomicU64::new(0);
static LAST_PROGRESS_MS: AtomicU64 = AtomicU64::new(0);
/// Do not kill download workers while validation moved inside this window.
const MESH_ABORT_STALL_MS: u64 = 30_000;

pub(crate) fn enabled() -> bool {
    match std::env::var("BLVM_IBD_TIP_PROBE")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some("1") | Some("true") | Some("on") | Some("yes") => true,
        _ => false,
    }
}

fn mesh_abort_enabled() -> bool {
    match std::env::var("BLVM_IBD_PROBE_MESH_ABORT")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some("0") | Some("false") | Some("off") | Some("no") => false,
        _ => true,
    }
}

/// Drop a rank entry whose last OK is older than this (default 300s).
fn probe_stale_secs() -> u64 {
    std::env::var("BLVM_IBD_PROBE_STALE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(PROBE_STALE_SECS_DEFAULT)
}

fn sample_fresh(e: &ProbeSample) -> bool {
    match e.last_ok {
        Some(t) => t.elapsed() <= Duration::from_secs(probe_stale_secs()),
        None => false,
    }
}

/// Wave-implied BPS from one-block GetData sojourn (ms).
pub(crate) fn wave_bps_from_sojourn_ms(ms: u64) -> f64 {
    let ms = ms.max(1) as f64;
    WAVE_DEPTH * 1000.0 / ms
}

fn header_hash(header: &blvm_protocol::BlockHeader) -> blvm_protocol::Hash {
    let mut header_bytes = [0u8; 80];
    header_bytes[0..4].copy_from_slice(&(header.version as i32).to_le_bytes());
    header_bytes[4..36].copy_from_slice(&header.prev_block_hash);
    header_bytes[36..68].copy_from_slice(&header.merkle_root);
    header_bytes[68..72].copy_from_slice(&(header.timestamp as u32).to_le_bytes());
    header_bytes[72..76].copy_from_slice(&(header.bits as u32).to_le_bytes());
    header_bytes[76..80].copy_from_slice(&(header.nonce as u32).to_le_bytes());
    double_sha256(&header_bytes)
}

fn note_ok(peer: &str, sojourn_ms: u64) {
    let ms = sojourn_ms.min(60_000);
    if let Ok(mut g) = table().lock() {
        let e = g.entry(peer.to_string()).or_insert(ProbeSample {
            ewma_ms: 0,
            n: 0,
            wave_bps: 0.0,
            last_ok: None,
            last_fail: None,
            inflight: false,
        });
        e.ewma_ms = if e.n == 0 {
            ms
        } else {
            (e.ewma_ms.saturating_mul(7).saturating_add(ms)) / 8
        };
        e.n = e.n.saturating_add(1);
        e.wave_bps = wave_bps_from_sojourn_ms(e.ewma_ms);
        e.last_ok = Some(Instant::now());
        e.inflight = false;
    }
    PROBE_OK.fetch_add(1, Ordering::Relaxed);
}

fn note_fail(peer: &str) {
    if let Ok(mut g) = table().lock() {
        let e = g.entry(peer.to_string()).or_insert(ProbeSample {
            ewma_ms: 0,
            n: 0,
            wave_bps: 0.0,
            last_ok: None,
            last_fail: None,
            inflight: false,
        });
        e.last_fail = Some(Instant::now());
        e.inflight = false;
    }
    PROBE_FAIL.fetch_add(1, Ordering::Relaxed);
}

fn mark_inflight(peer: &str, on: bool) {
    if let Ok(mut g) = table().lock() {
        let e = g.entry(peer.to_string()).or_insert(ProbeSample {
            ewma_ms: 0,
            n: 0,
            wave_bps: 0.0,
            last_ok: None,
            last_fail: None,
            inflight: false,
        });
        e.inflight = on;
    }
}

/// Ranked NODE_NETWORK probes (highest wave BPS first). `n == 0` is unranked.
/// Drops stale samples (default 300s). `skip` drops blacklisted (or test) peers.
pub(crate) fn ranked_probes(exclude: Option<&str>) -> Vec<(String, f64)> {
    ranked_probes_skip(exclude, |_| false)
}

pub(crate) fn ranked_probes_skip(
    exclude: Option<&str>,
    skip: impl Fn(&str) -> bool,
) -> Vec<(String, f64)> {
    let Ok(g) = table().lock() else {
        return Vec::new();
    };
    let mut v: Vec<(String, f64)> = g
        .iter()
        .filter(|(p, e)| {
            e.n > 0
                && e.wave_bps > 0.0
                && sample_fresh(e)
                && exclude.is_none_or(|x| x != p.as_str())
                && !skip(p.as_str())
        })
        .map(|(p, e)| (p.clone(), e.wave_bps))
        .collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    v
}

/// Best NODE_NETWORK probe rank (highest wave BPS with n≥1).
pub(crate) fn best_probe_rank(exclude: Option<&str>) -> Option<(String, f64)> {
    ranked_probes(exclude).into_iter().next()
}

pub(crate) fn best_probe_rank_skip(
    exclude: Option<&str>,
    skip: impl Fn(&str) -> bool,
) -> Option<(String, f64)> {
    ranked_probes_skip(exclude, skip).into_iter().next()
}

/// Probe GetData EWMA sojourn (ms). Unranked → `None`.
pub(crate) fn probe_ewma_ms(peer: &str) -> Option<u64> {
    let Ok(g) = table().lock() else {
        return None;
    };
    g.get(peer).filter(|e| e.n > 0 && e.ewma_ms > 0).map(|e| e.ewma_ms)
}

/// Wave-implied BPS for one peer (`n≥1`). Unranked → `None`.
pub(crate) fn probe_wave_bps(peer: &str) -> Option<f64> {
    let Ok(g) = table().lock() else {
        return None;
    };
    g.get(peer)
        .filter(|e| e.n > 0 && e.wave_bps > 0.0)
        .map(|e| e.wave_bps)
}

/// Size-matched probe says this peer can hold ≥80 (independent of current stream).
pub(crate) fn probe_keep_hero(peer: &str) -> bool {
    probe_wave_bps(peer).is_some_and(|b| b >= KEEP_BPS)
}

/// Successful size-matched probes (`n≥1`). Fail/timeout does not count.
pub(crate) fn probe_n(peer: &str) -> u64 {
    let Ok(g) = table().lock() else {
        return 0;
    };
    g.get(peer).map(|e| e.n).unwrap_or(0)
}

/// Successful size-matched probes (`n≥1`). Fail/timeout does not count.
pub(crate) fn peer_probed(peer: &str) -> bool {
    let Ok(g) = table().lock() else {
        return false;
    };
    g.get(peer).is_some_and(|e| e.n > 0)
}

pub(crate) fn probe_ok_count() -> u64 {
    PROBE_OK.load(Ordering::Relaxed)
}

/// True when any probed NODE_NETWORK peer has wave-implied ≥80.
pub(crate) fn any_probe_ge80() -> bool {
    let Ok(g) = table().lock() else {
        return false;
    };
    g.values().any(|e| e.n > 0 && e.wave_bps >= KEEP_BPS)
}

pub(crate) fn mesh_fail() -> bool {
    MESH_FAIL.load(Ordering::Relaxed)
}

/// Prefer a size-matched block near H: `next_needed + 1000` clamped to
/// `header_tip - 144`. FALLBACK only when that height is unavailable.
fn pick_probe_height(header_tip: u64, next_needed: u64) -> Option<u64> {
    if header_tip >= 144 {
        let cap = header_tip - 144;
        let h = next_needed.saturating_add(1000).min(cap);
        if h > 0 {
            return Some(h);
        }
    }
    for h in PROBE_HEIGHT_FALLBACK {
        if header_tip > h.saturating_add(288) {
            return Some(h);
        }
    }
    None
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn verify_probe_body(
    block: &blvm_protocol::Block,
    expected_hash: blvm_protocol::Hash,
    stored_merkle: blvm_protocol::Hash,
) -> bool {
    if header_hash(&block.header) != expected_hash {
        return false;
    }
    if block.header.merkle_root != stored_merkle {
        return false;
    }
    #[cfg(feature = "production")]
    {
        match blvm_consensus::mining::calculate_merkle_root(&block.transactions) {
            Ok(root) if root == stored_merkle => true,
            _ => false,
        }
    }
    #[cfg(not(feature = "production"))]
    {
        true
    }
}

/// Coordinator ~1 Hz: arm one inflight probe per non-owner; mesh-fail at 5k / 3 min.
pub(crate) fn coord_tick(
    network: Arc<NetworkManager>,
    blockstore: Arc<BlockStore>,
    assigner: Arc<ChunkAssigner>,
    validation_height: u64,
) {
    if !enabled() {
        return;
    }
    let header_tip = assigner.header_tip();
    let next_needed = assigner.next_needed_height();
    let Some(height) = pick_probe_height(header_tip, next_needed) else {
        return;
    };
    let Ok(Some(hash)) = blockstore.get_hash_by_height(height) else {
        return;
    };
    let Ok(Some(stored)) = blockstore.get_header(&hash) else {
        return;
    };
    let stored_merkle = stored.merkle_root;
    let sticky = assigner.preferred_tip_owner();
    let ready = assigner.peer_ids_for_ibd_ready();
    let inflight_n = table()
        .lock()
        .ok()
        .map(|g| g.values().filter(|e| e.inflight).count())
        .unwrap_or(0);

    let mut launched = 0usize;
    for peer in ready {
        if sticky.as_deref() == Some(peer.as_str()) {
            continue;
        }
        if assigner.is_peer_blacklisted(&peer) {
            continue;
        }
        let Ok(addr) = peer.parse::<SocketAddr>() else {
            continue;
        };
        let due = table().lock().ok().and_then(|g| {
            let e = g.get(&peer)?;
            if e.inflight {
                return Some(false);
            }
            let last = e.last_ok.or(e.last_fail);
            Some(last.is_none_or(|t| t.elapsed() >= PER_PEER_INTERVAL))
        });
        if due == Some(false) {
            continue;
        }
        if inflight_n + launched >= MAX_INFLIGHT {
            break;
        }
        mark_inflight(&peer, true);
        launched += 1;
        if FIRST_PROBE_MS.load(Ordering::Relaxed) == 0 {
            FIRST_PROBE_MS.store(now_ms(), Ordering::Relaxed);
        }
        let net = Arc::clone(&network);
        let peer_s = peer.clone();
        tokio::spawn(async move {
            let t0 = Instant::now();
            match net.request_block_from_peer(addr, hash, PROBE_TIMEOUT).await {
                Some(block) if verify_probe_body(&block, hash, stored_merkle) => {
                    let ms = t0.elapsed().as_millis() as u64;
                    note_ok(&peer_s, ms);
                    tracing::info!(
                        "[IBD_PROBE_OK] peer={} h={} sojourn_ms={} wave_bps={:.1}",
                        peer_s,
                        height,
                        ms,
                        wave_bps_from_sojourn_ms(ms)
                    );
                }
                Some(_) => {
                    note_fail(&peer_s);
                    tracing::warn!(
                        "[IBD_PROBE_FAIL] peer={} h={} — hash/merkle mismatch (not a mute-BPS sample)",
                        peer_s,
                        height
                    );
                }
                None => {
                    note_fail(&peer_s);
                    tracing::info!(
                        "[IBD_PROBE_FAIL] peer={} h={} — timeout/no-body (not LIMITED)",
                        peer_s,
                        height
                    );
                }
            }
        });
    }

    maybe_mesh_fail(validation_height, &assigner);
}

fn mesh_fail_ready(validation_height: u64, first_probe_age_ms: u64) -> bool {
    let aged = first_probe_age_ms >= 180_000;
    let past_5k = validation_height >= 5_000;
    // dest-aw: empty-block hit 5k at first_probe_age_s=4 (`ge80=0`) and
    // aborted a live KEEP sticky @23 BPS. Gate is 5k AND 3 min, not OR.
    past_5k && aged
}

fn note_validation_progress(validation_height: u64) {
    let prev = LAST_PROGRESS_VH.load(Ordering::Relaxed);
    if validation_height > prev {
        LAST_PROGRESS_VH.store(validation_height, Ordering::Relaxed);
        LAST_PROGRESS_MS.store(now_ms(), Ordering::Relaxed);
    }
}

fn mesh_abort_shutdown_allowed_at(now: u64, last_progress_ms: u64) -> bool {
    now.saturating_sub(last_progress_ms) >= MESH_ABORT_STALL_MS
}

fn mesh_abort_shutdown_allowed() -> bool {
    mesh_abort_shutdown_allowed_at(now_ms(), LAST_PROGRESS_MS.load(Ordering::Relaxed))
}

fn maybe_mesh_fail(validation_height: u64, assigner: &ChunkAssigner) {
    note_validation_progress(validation_height);
    let first = FIRST_PROBE_MS.load(Ordering::Relaxed);
    if first == 0 {
        return;
    }
    if !mesh_fail_ready(validation_height, now_ms().saturating_sub(first)) {
        return;
    }
    let hash_fetch = crate::node::parallel_ibd::hash_fetch::enabled();
    let supply_ok = if hash_fetch {
        crate::node::parallel_ibd::hash_fetch::aggregate_can_fill_ahead()
    } else {
        any_probe_ge80()
    };
    if supply_ok {
        MESH_FAIL.store(false, Ordering::Relaxed);
        return;
    }
    if MESH_FAIL.swap(true, Ordering::Relaxed) {
        return;
    }
    if hash_fetch {
        tracing::warn!(
            "[IBD_PROBE_MESH_FAIL] validation={} first_probe_age_s={} — aggregate byte-rate cannot fill fetch_ahead",
            validation_height,
            now_ms().saturating_sub(first) / 1000
        );
    } else {
        tracing::warn!(
            "[IBD_PROBE_MESH_FAIL] validation={} first_probe_age_s={} ge80=0 — no NODE_NETWORK size-matched ≥80",
            validation_height,
            now_ms().saturating_sub(first) / 1000
        );
    }
    if mesh_abort_enabled() {
        // dest-be: 200k size-matched probes rarely print ≥80 on public WAN
        // (400ms sojourn) while crawl still holds 20–150 BPS. Killing workers
        // mid-IBD left GetData dead and validation frozen on the next hole.
        // dest-as 5k fail-fast still aborts after 30s with no height change.
        if !mesh_abort_shutdown_allowed() {
            tracing::warn!(
                "[IBD_PROBE_MESH_ABORT_SKIP] validation={} advancing — not shutting assigner (dest-be 360k suicide)",
                validation_height
            );
            return;
        }
        assigner.request_shutdown();
        tracing::warn!("[IBD_PROBE_MESH_ABORT] assigner shutdown — rotate addrs / new dest");
    }
}

#[cfg(test)]
pub(crate) fn test_reset_probes() {
    if let Ok(mut g) = table().lock() {
        g.clear();
    }
    FIRST_PROBE_MS.store(0, Ordering::Relaxed);
    MESH_FAIL.store(false, Ordering::Relaxed);
    PROBE_OK.store(0, Ordering::Relaxed);
    PROBE_FAIL.store(0, Ordering::Relaxed);
    LAST_PROGRESS_VH.store(0, Ordering::Relaxed);
    LAST_PROGRESS_MS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn test_seed_probe(peer: &str, sojourn_ms: u64) {
    unsafe {
        std::env::set_var("BLVM_IBD_TIP_PROBE", "1");
    }
    note_ok(peer, sojourn_ms);
}

#[cfg(test)]
pub(crate) fn test_age_last_ok(peer: &str, age_secs: u64) {
    if let Ok(mut g) = table().lock() {
        if let Some(e) = g.get_mut(peer) {
            e.last_ok = Instant::now().checked_sub(Duration::from_secs(age_secs));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[serial_test::serial(ibd)]
    #[test]
    fn enabled_default_off_env_on() {
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_PROBE");
        }
        assert!(!enabled(), "probe default off");
        unsafe {
            std::env::set_var("BLVM_IBD_TIP_PROBE", "1");
        }
        assert!(enabled());
        unsafe {
            std::env::set_var("BLVM_IBD_TIP_PROBE", "true");
        }
        assert!(enabled());
        unsafe {
            std::env::set_var("BLVM_IBD_TIP_PROBE", "on");
        }
        assert!(enabled());
        unsafe {
            std::env::set_var("BLVM_IBD_TIP_PROBE", "0");
        }
        assert!(!enabled());
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_PROBE");
        }
        assert!(!enabled());
    }

    #[test]
    fn wave_bps_400ms_is_keep() {
        assert!((wave_bps_from_sojourn_ms(400) - 80.0).abs() < 0.1);
        assert!(wave_bps_from_sojourn_ms(200) > 80.0);
        assert!(wave_bps_from_sojourn_ms(1000) < 80.0);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn rank_prefers_faster_sojourn() {
        test_reset_probes();
        test_seed_probe("fast", 200);
        test_seed_probe("slow", 1000);
        let (p, bps) = best_probe_rank(None).expect("rank");
        assert_eq!(p, "fast");
        assert!(bps > 80.0);
        let (p2, _) = best_probe_rank(Some("fast")).expect("second");
        assert_eq!(p2, "slow");
        assert!(any_probe_ge80());
        test_reset_probes();
    }

    #[test]
    fn probe_keep_hero_is_wave_ge80() {
        test_reset_probes();
        test_seed_probe("fast", 200);
        test_seed_probe("slow", 1000);
        assert!(probe_keep_hero("fast"));
        assert!(!probe_keep_hero("slow"));
        assert!(!probe_keep_hero("missing"));
        assert!(probe_wave_bps("fast").unwrap() > 80.0);
        test_reset_probes();
    }

    #[test]
    fn exclude_and_empty() {
        test_reset_probes();
        assert!(best_probe_rank(None).is_none());
        test_seed_probe("only", 150);
        assert!(best_probe_rank(Some("only")).is_none());
        test_reset_probes();
    }

    #[test]
    fn pick_height_prefers_near_h() {
        assert_eq!(pick_probe_height(960_000, 1), Some(1_001));
        assert_eq!(pick_probe_height(960_000, 10_000), Some(11_000));
        assert_eq!(pick_probe_height(960_000, 200_000), Some(201_000));
        assert_eq!(pick_probe_height(960_000, 370_000), Some(371_000));
        assert_eq!(pick_probe_height(200_144, 200_000), Some(200_000));
        assert_eq!(pick_probe_height(180_400, 200_000), Some(180_256));
        assert_eq!(pick_probe_height(100, 0), None);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn rank_skips_blacklisted_or_stale_top() {
        test_reset_probes();
        unsafe {
            std::env::remove_var("BLVM_IBD_PROBE_STALE_SECS");
        }
        test_seed_probe("ghost", 37);
        test_seed_probe("second", 200);
        let (p, _) = best_probe_rank(None).expect("fresh ghost still #1");
        assert_eq!(p, "ghost");
        let (p2, _) = best_probe_rank_skip(None, |p| p == "ghost").expect("blacklist skip");
        assert_eq!(p2, "second", "blacklisted #1 must yield rank #2");
        test_age_last_ok("ghost", 301);
        let (p3, _) = best_probe_rank(None).expect("stale skipped");
        assert_eq!(p3, "second", "stale #1 must yield rank #2");
        test_reset_probes();
    }

    #[test]
    fn unranked_fail_is_not_keep() {
        test_reset_probes();
        note_fail("mute");
        assert!(best_probe_rank(None).is_none());
        assert!(!any_probe_ge80());
        assert!(!peer_probed("mute"));
        test_reset_probes();
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn peer_probed_after_ok() {
        test_reset_probes();
        assert_eq!(probe_ok_count(), 0);
        test_seed_probe("p", 200);
        assert!(peer_probed("p"));
        assert_eq!(probe_ok_count(), 1);
        assert_eq!(probe_n("p"), 1);
        test_seed_probe("p", 200);
        assert_eq!(probe_n("p"), 2);
        test_reset_probes();
    }

    #[test]
    fn inflight_budget_is_four() {
        assert_eq!(MAX_INFLIGHT, 4);
    }

    #[test]
    fn mesh_fail_waits_three_min_after_5k() {
        // dest-aw: validation=5696 first_probe_age_s=4 aborted KEEP.
        assert!(!mesh_fail_ready(5_696, 4_000));
        assert!(!mesh_fail_ready(5_696, 179_999));
        assert!(mesh_fail_ready(5_696, 180_000));
        assert!(!mesh_fail_ready(4_999, 180_000));
    }

    #[test]
    fn dest_be_mesh_abort_must_not_shutdown_while_validation_advances() {
        // dest-be 05:23:18: ge80=0 after 180s, vh=360583 still climbing,
        // MESH_ABORT killed every download worker.
        assert!(!mesh_abort_shutdown_allowed_at(10_000, 9_500));
        assert!(!mesh_abort_shutdown_allowed_at(30_000, 1));
        assert!(mesh_abort_shutdown_allowed_at(40_000, 9_000));
        test_reset_probes();
        note_validation_progress(360_583);
        assert!(
            !mesh_abort_shutdown_allowed(),
            "fresh progress must skip assigner shutdown"
        );
        test_reset_probes();
    }
}
