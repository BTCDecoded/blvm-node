//! Block chunk download for parallel IBD.
//!
//! Downloads blocks from a peer using pipelined, batched GetData requests.
//! Up to GETDATA_BATCH_SIZE (64, a31 peak; override `BLVM_IBD_GETDATA_BATCH`)
//! block hashes are sent per GetData message to reduce per-message overhead.
//! Max blocks_in_transit_per_peer across all workers is
//! configurable (default 128).

use super::types::{SharedBlock, SharedWitnesses};
use crate::network::NetworkManager;
use crate::network::inventory::{MSG_BLOCK, MSG_WITNESS_BLOCK};
use crate::network::protocol::{GetDataMessage, InventoryVector, ProtocolMessage, ProtocolParser};
use crate::storage::blockstore::BlockStore;
use anyhow::{Context, Result};
use super::local_block::{
    has_real_witnesses, empty_witness_unacceptable, ibd_stall_aborts_inflight_gap_fetch,
    cached_feature_registry, is_local_witness_hole, try_load_local_ibd_block,
    try_persist_gap_block_for_local_inject, try_persist_gap_block_for_local_inject_with_wire,
    try_repair_missing_witness,
};
use blvm_protocol::{Block, Hash, ProtocolVersion, segwit::Witness};
use blvm_protocol::types::ARC_BLOCK_CREATED;
use futures::stream::{FuturesUnordered, StreamExt};
use hex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::sync::broadcast;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};


use super::ParallelIBDConfig;
use super::latch_env;

/// B11v5: off-select GAP_PERSIST + per-height disk-ready latch before STREAM.
///
/// B11 unbounded restored READY gd≈32 by moving `store_block*` off the download select,
/// but STREAM'd before disk → dens tip30≈43 / Emergency. Cap/serial kept dens or READY
/// cold because STREAM still raced or select still blocked.
///
/// Opt-in (`BLVM_IBD_GAP_PERSIST_OFFLOAD=1`): RAM/`received` + GetData drain immediately;
/// persist via bounded `spawn_blocking`; [`try_stream_validation_gap`] only after ACK.
fn gap_persist_offload_enabled() -> bool {
    latch_env!(bool, {
        matches!(
            std::env::var("BLVM_IBD_GAP_PERSIST_OFFLOAD")
                .ok()
                .as_deref()
                .map(|s| s.trim()),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    })
}

fn gap_persist_offload_concurrency() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_GAP_PERSIST_OFFLOAD_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(16)
            .clamp(1, 64)
    })
}

/// With OFFLOAD on: tip-adjacent persists use a dedicated semaphore (not the far-ahead
/// pool) so STREAM disk-ready is not queued behind home write storms (Land E dens off4:
/// tip_gd hot but 408k rate 237→47 h/s). Still async (no select-loop sync — that re-froze
/// READY gd≈893). Opt-in `BLVM_IBD_GAP_PERSIST_TIP_SYNC=1`.
fn gap_persist_tip_sync_enabled() -> bool {
    latch_env!(bool, {
        matches!(
            std::env::var("BLVM_IBD_GAP_PERSIST_TIP_SYNC")
                .ok()
                .as_deref()
                .map(|s| s.trim()),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    })
}

fn gap_persist_tip_lane_concurrency() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_GAP_PERSIST_TIP_SYNC_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2)
            .clamp(1, 8)
    })
}

/// Heights in `[tip, tip+span]` (and `next_to_send`) use the tip OFFLOAD lane.
/// Default 64 covers tip-hole FAST_CAP so bodies that arrive in the tip pipe are not
/// stuck on the far-ahead sem when the tip later reaches them (408k cliff dig).
fn gap_persist_tip_lane_span() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_GAP_PERSIST_TIP_SYNC_SPAN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64)
            .clamp(1, 256)
    })
}

/// Land E dens off4: tip_gd hot but tip30/tip90 cliff when tip-band OFFLOAD storms
/// (confirm …T041527Z: 87 OFFLOADs @404–406k + tip_stage total_ms=7326; peak …T021013Z
/// had n=3 in that band and held ~100 h/s). Opt-in: keep far-ahead bodies in RAM and
/// only spawn GAP_PERSIST once `height <= tip+span` (soft-cap still sync-persists under
/// memory pressure). Default off — dig only.
fn gap_persist_defer_far_enabled() -> bool {
    latch_env!(bool, {
        matches!(
            std::env::var("BLVM_IBD_GAP_PERSIST_DEFER_FAR")
                .ok()
                .as_deref()
                .map(|s| s.trim()),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    })
}

fn gap_persist_defer_far_span() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_GAP_PERSIST_DEFER_FAR_SPAN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(128)
            .clamp(8, 512)
    })
}

/// When DEFER_FAR is on: only spawn OFFLOAD for heights within `tip+kick_span`
/// (default = defer span — unchanged). Dig: smaller kick (8–32) mimics KEEP tip30
/// silent early window (0 OFFLOAD for ~20s) while still holding far-ahead RAM.
/// `BLVM_IBD_GAP_PERSIST_DEFER_FAR_KICK_SPAN` clamped to `[1, defer_span]`.
fn gap_persist_defer_far_kick_span() -> u64 {
    let defer = gap_persist_defer_far_span();
    latch_env!(u64, {
        std::env::var("BLVM_IBD_GAP_PERSIST_DEFER_FAR_KICK_SPAN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(defer)
            .clamp(1, defer)
    })
}

/// Dig: apply tight `KICK_SPAN` only once tip reaches this height (default 0 =
/// always use kick_span). Avoids KICK16 STREAM starve during READY settle
/// (dk16 INVALID:ready_gd_late) while still cutting tip30 OFFLOAD backlog @402–407k.
fn gap_persist_defer_far_kick_min_h() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_GAP_PERSIST_DEFER_FAR_KICK_MIN_H")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    })
}

/// Effective OFFLOAD kick window for current tip.
fn gap_persist_defer_far_kick_span_for_tip(tip_now: u64) -> u64 {
    let defer = gap_persist_defer_far_span();
    let kick = gap_persist_defer_far_kick_span();
    let min_h = gap_persist_defer_far_kick_min_h();
    if min_h > 0 && tip_now < min_h {
        defer
    } else {
        kick
    }
}

fn gap_persist_in_tip_window(height: u64, tip_now: u64, next_to_send: u64, span: u64) -> bool {
    height <= tip_now.saturating_add(span) || height <= next_to_send.saturating_add(span)
}

type GapPersistAckFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = (u64, Result<()>)> + Send>,
>;

fn spawn_gap_persist_offload(
    persist_inflight: &mut FuturesUnordered<GapPersistAckFuture>,
    persist_sem: &Arc<Semaphore>,
    blockstore: &BlockStore,
    validation_height: &Option<Arc<AtomicU64>>,
    height: u64,
    block_hash: Hash,
    block: SharedBlock,
    witnesses: SharedWitnesses,
    protocol_version: ProtocolVersion,
    wire: Option<Vec<u8>>,
    tip_lane: bool,
) {
    let sem = Arc::clone(persist_sem);
    let bs = blockstore.clone();
    let vh = validation_height.clone();
    persist_inflight.push(Box::pin(async move {
        let _permit = match sem.acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                return (
                    height,
                    Err(anyhow::anyhow!("GAP_PERSIST offload semaphore closed")),
                );
            }
        };
        let t0 = Instant::now();
        let persist_res = tokio::task::spawn_blocking(move || {
            try_persist_gap_block_for_local_inject_with_wire(
                &bs,
                vh.as_ref(),
                height,
                block_hash,
                block.as_ref(),
                witnesses.as_ref(),
                protocol_version,
                wire.as_deref(),
            )
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .await;
        let res = match persist_res {
            Ok(inner) => inner,
            Err(e) => Err(anyhow::anyhow!("GAP_PERSIST join: {e}")),
        };
        let ms = t0.elapsed().as_millis();
        let tag = if tip_lane {
            "IBD_GAP_PERSIST_TIP_SYNC"
        } else {
            "IBD_GAP_PERSIST_OFFLOAD"
        };
        if ms >= 50 || tip_lane && ms >= 20 {
            tracing::info!(
                "[{}] height={} persist_ms={} ok={}",
                tag,
                height,
                ms,
                res.is_ok()
            );
        } else {
            tracing::debug!(
                "[{}] height={} persist_ms={} ok={}",
                tag,
                height,
                ms,
                res.is_ok()
            );
        }
        (height, res)
    }));
}

/// Spawn OFFLOAD for received heights that entered the tip window while DEFER_FAR held them.
fn kick_deferred_gap_persists(
    received: &BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    disk_ready: &HashSet<u64>,
    persist_spawned: &mut HashSet<u64>,
    persist_inflight: &mut FuturesUnordered<GapPersistAckFuture>,
    persist_sem: &Arc<Semaphore>,
    tip_persist_sem: &Arc<Semaphore>,
    blockstore: &BlockStore,
    validation_height: &Option<Arc<AtomicU64>>,
    tip_now: u64,
    next_to_send: u64,
    protocol_version: ProtocolVersion,
) {
    if !gap_persist_defer_far_enabled() {
        return;
    }
    // Kick only tip-near heights (kick_span ≤ defer_span). Far-ahead stays in RAM.
    let kick = gap_persist_defer_far_kick_span_for_tip(tip_now);
    let horizon = tip_now
        .saturating_add(kick)
        .max(next_to_send.saturating_add(kick));
    let mut pending: Vec<(u64, SharedBlock, SharedWitnesses)> = Vec::new();
    for (&h, (block, wit)) in received.range(..=horizon) {
        if h < tip_now {
            continue;
        }
        if disk_ready.contains(&h) || persist_spawned.contains(&h) {
            continue;
        }
        if !gap_persist_in_tip_window(h, tip_now, next_to_send, kick) {
            continue;
        }
        pending.push((h, Arc::clone(block), Arc::clone(wit)));
    }
    for (h, block, wit) in pending {
        persist_spawned.insert(h);
        let hash = blockstore.get_block_hash(block.as_ref());
        let tip_lane = gap_persist_tip_sync_enabled()
            && (h <= tip_now.saturating_add(1)
                || h == next_to_send
                || gap_persist_in_tip_window(h, tip_now, next_to_send, gap_persist_tip_lane_span()));
        let sem = if tip_lane {
            tip_persist_sem
        } else {
            persist_sem
        };
        tracing::debug!(
            "[IBD_GAP_PERSIST_DEFER_KICK] height={} tip={} next={} tip_lane={}",
            h,
            tip_now,
            next_to_send,
            tip_lane
        );
        spawn_gap_persist_offload(
            persist_inflight,
            sem,
            blockstore,
            validation_height,
            h,
            hash,
            block,
            wit,
            protocol_version,
            None,
            tip_lane,
        );
    }
}

/// Synthetic peer id for zero-peer local replay (`BLVM_IBD_ALLOW_ZERO_PEERS`).
/// Workers use `try_load_local_ibd_block` only — never GetData / connect.
pub(crate) const LOCAL_DISK_PEER_ID: &str = "local-disk";

pub(crate) fn is_local_disk_peer(peer_id: &str) -> bool {
    peer_id == LOCAL_DISK_PEER_ID
}

fn tip_need_from(validation_height: Option<&Arc<AtomicU64>>) -> Option<u64> {
    validation_height.map(|vh| vh.load(Ordering::Relaxed).saturating_add(1))
}

fn block_tx_tip_reserve() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_BLOCK_TX_TIP_RESERVE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64)
            .clamp(8, 512)
    })
}

/// Wait until the channel has headroom for a non-tip send (keeps slots for tip).
///
/// Unlike `try_send`+break (live ~6 BPS), park briefly so the coordinator can drain while
/// the worker holds the tip-contiguous cursor. Exact tip never waits here.
///
/// Important: do **not** busy-`yield_now` here. Under synth local-disk, a full channel +
/// tip already queued (`GAP_STREAM` at t≈0) left two download workers spinning `yield_now`
/// while the coordinator starved — Validation first block at ~10.8s, wall ~6–10 BPS
/// (burst path had first block ~1.0s and no TIP_RESERVE waits). A 1ms sleep yields the
/// runtime properly so `recv_many` can drain tip.
async fn await_block_tx_tip_reserve(
    tx: &tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>,
    height: u64,
    tip_need: Option<u64>,
) {
    if tip_need == Some(height) || tip_need.is_none() {
        return;
    }
    // Keep tip-reserve for synth bulk too: disabling it re-starved tip (Validation
    // first block ~11s). Use 1ms sleep — not yield_now — so the coordinator can drain
    // (yield_now spin left first block ~10.8s). Burst ~500 wall had no RESERVE waits
    // because the channel never filled; when it does, reserve protects tip slots.
    let reserve = block_tx_tip_reserve();
    let mut logged = false;
    while tx.capacity() <= reserve {
        if !logged {
            logged = true;
            info!(
                "[IBD_BLOCK_TX_TIP_RESERVE] wait h={} free={} (reserve={})",
                height,
                tx.capacity(),
                reserve
            );
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Disk-backed block source: offline local replay or synthetic WAN harness.
pub(crate) fn is_snapshot_sourced_peer(peer_id: &str) -> bool {
    is_local_disk_peer(peer_id) || super::synthetic_wan::is_synthetic_peer(peer_id)
}

/// Per-worker soft cap on `received` (Arc<Block> bodies). Live W55 jemalloc: 1 MiB class
/// grew to ~4k live objs (~4 GiB) while reorder≪1k — tip-stall + W50 no-ahead-drain left
/// parallel chunk maps holding far-ahead bodies. Persist-then-drop beyond the tip window.
///
/// Default **160** (≥ tip-owner pipe 128). Live W65 genesis: cap=96 + tip pipe 128 +
/// persist lookahead 64 → continuous trim of tip+65.. without successful persist →
/// **106M** drops / 1.6M log lines → ~60–90s coordinator silence plateaus (~0 blk/s).
fn download_received_soft_cap() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_DOWNLOAD_RECEIVED_CAP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(160)
            .clamp(64, 512)
    })
}

/// Hard ceiling: drop even without persist to protect RAM (per worker).
fn download_received_hard_cap() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_DOWNLOAD_RECEIVED_HARD_CAP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(320)
            .clamp(128, 1024)
            .max(download_received_soft_cap())
    })
}

fn note_received_insert(n: u64) {
    super::memory::DOWNLOAD_RECEIVED_BLOCKS.fetch_add(n, Ordering::Relaxed);
}

fn note_received_remove(n: u64) {
    let _ = super::memory::DOWNLOAD_RECEIVED_BLOCKS.fetch_update(
        Ordering::Relaxed,
        Ordering::Relaxed,
        |v| Some(v.saturating_sub(n)),
    );
}

fn received_put(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    h: u64,
    v: (SharedBlock, SharedWitnesses),
) {
    if received.insert(h, v).is_none() {
        note_received_insert(1);
    }
}

fn received_take(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    h: u64,
) -> Option<(SharedBlock, SharedWitnesses)> {
    received.remove(&h).inspect(|_| note_received_remove(1))
}

/// Cheap Arc clone of a buffered height — does **not** remove it from `received`.
///
/// H5/A1: tip STREAM must leave the body keyed until validation advances (`trim` drops
/// `h < need`). Taking on STREAM forced LOCAL_GAP reinject + `GAP_STREAM_RESEND` when the
/// coordinator lost the tip (synth cliff ~315→340k: RESEND≈6k, instant BPS 137→9).
fn received_clone(
    received: &BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    h: u64,
) -> Option<(SharedBlock, SharedWitnesses)> {
    received
        .get(&h)
        .map(|(b, w)| (Arc::clone(b), Arc::clone(w)))
}

fn received_drain_all(received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>) {
    let n = received.len() as u64;
    received.clear();
    if n > 0 {
        note_received_remove(n);
    }
}

/// Hard-cap eviction for per-worker `received` (OOM guard).
///
/// Drops farthest-ahead bodies while `len > hard` and `h > need`.
/// **Never** removes tip-adjacent `h <= need` — deliberate diverge from rbitcoin's soft
/// "never drop already-received" (we drop far-ahead; we still protect the tip window).
/// Phase 0b.1 policy surface (docs/RBITCOIN_VS_BLVM_IBD_ARCHITECTURE.md).
fn hard_trim_download_received_far_ahead(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    need: u64,
    hard: usize,
) -> u64 {
    let mut forced = 0u64;
    while received.len() > hard {
        let Some((&h, _)) = received.iter().next_back() else {
            break;
        };
        if h <= need {
            break;
        }
        let _ = received_take(received, h);
        forced = forced.saturating_add(1);
    }
    forced
}

/// Bound per-worker `received` RAM without the W66 re-download thrash.
///
/// Live W65 genesis: trim dropped far-ahead even when `GAP_PERSIST` refused (height >
/// val+lookahead), while **below-tip** entries were never evicted (`h <= need` break).
/// Maps stayed over soft-cap → every body arrival trimmed 1 → 106M drops + log storm.
///
/// Order:
/// 1. Drop stale `h < need` (already past validation; safe without persist).
/// 2. Soft-cap: persist-then-drop farthest ahead; **put back** if persist fails.
/// 3. Hard-cap: drop farthest anyway (memory guard).
fn trim_download_received(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    blockstore: &BlockStore,
    validation_height: Option<&Arc<AtomicU64>>,
    protocol_version: ProtocolVersion,
) {
    let soft = download_received_soft_cap();
    let hard = download_received_hard_cap();
    let need = validation_height
        .map(|v| v.load(Ordering::Relaxed).saturating_add(1))
        .unwrap_or(0);
    let mut trimmed = 0u64;
    let mut forced = 0u64;

    // (1) Evict below tip — these blocked the old trim loop and kept len > soft forever.
    while let Some((&h, _)) = received.iter().next() {
        if h >= need {
            break;
        }
        let _ = received_take(received, h);
        trimmed = trimmed.saturating_add(1);
    }

    // (2) Soft trim: persist-then-drop only.
    while received.len() > soft {
        let Some((&h, _)) = received.iter().next_back() else {
            break;
        };
        // Never drop the validation tip from the worker buffer.
        if h <= need {
            break;
        }
        let Some((block, witnesses)) = received_take(received, h) else {
            break;
        };
        let hash = blockstore.get_block_hash(block.as_ref());
        let persisted = try_persist_gap_block_for_local_inject(
            blockstore,
            validation_height,
            h,
            hash,
            block.as_ref(),
            witnesses.as_ref(),
            protocol_version,
        )
        .unwrap_or(false);
        if persisted {
            trimmed = trimmed.saturating_add(1);
            drop((block, witnesses));
            continue;
        }
        // Persist refused (outside lookahead / witness) — keep unless hard-capped.
        received_put(received, h, (block, witnesses));
        break;
    }

    // (3) Hard cap: drop farthest ahead even without persist (OOM guard).
    // Never removes tip-adjacent `h <= need` (rbitcoin diverge: we *do* drop far-ahead).
    let hard_forced = hard_trim_download_received_far_ahead(received, need, hard);
    trimmed = trimmed.saturating_add(hard_forced);
    forced = forced.saturating_add(hard_forced);

    if trimmed > 0 {
        super::memory::DOWNLOAD_RECEIVED_TRIM_BLOCKS.fetch_add(trimmed, Ordering::Relaxed);
        // Time-rate-limit: ≤1 warn / 5s (old n%64 still logged ~3.6k/s under thrash).
        static LAST_TRIM_LOG_MS: AtomicU64 = AtomicU64::new(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_TRIM_LOG_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) >= 5_000
            && LAST_TRIM_LOG_MS
                .compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let n = super::memory::DOWNLOAD_RECEIVED_TRIM_BLOCKS.load(Ordering::Relaxed);
            warn!(
                "[IBD_DOWNLOAD_RECEIVED_TRIM] dropped {} block(s) (forced={}, soft={}, hard={}, received={}, need={}, total_trimmed={})",
                trimmed,
                forced,
                soft,
                hard,
                received.len(),
                need,
                n
            );
        }
    }
}

/// Result of [`download_chunk`]: when streaming to the coordinator, `blocks` is empty and
/// `streamed_block_count` holds the number sent; otherwise `blocks` contains the full chunk.
/// Blocks and witnesses are Arc-wrapped so they can be passed cheaply through further pipeline stages.
pub(crate) struct DownloadChunkResult {
    pub blocks: Vec<(u64, SharedBlock, SharedWitnesses)>,
    pub streamed_block_count: usize,
}

impl DownloadChunkResult {
    #[inline]
    pub fn block_count(&self) -> usize {
        if self.blocks.is_empty() {
            self.streamed_block_count
        } else {
            self.blocks.len()
        }
    }
}

/// Tracks block download progress for stalling detection
struct BlockDownloadProgress {
    last_block_hash: Option<Hash>,
    last_progress_time: std::time::Instant,
    current_timeout_seconds: u64,
    disconnected_peers_count: usize,
}

impl BlockDownloadProgress {
    fn new() -> Self {
        Self {
            last_block_hash: None,
            last_progress_time: std::time::Instant::now(),
            current_timeout_seconds: 120,
            disconnected_peers_count: 0,
        }
    }

    fn record_progress(&mut self, block_hash: Hash) {
        self.last_block_hash = Some(block_hash);
        self.last_progress_time = std::time::Instant::now();
    }

    fn reset_timeout(&mut self) {
        self.current_timeout_seconds = 120;
        self.disconnected_peers_count = 0;
    }
}

/// `send_to_peer` can fail transiently (`Peer not found`, disconnected, send channel closed).
/// Failing the entire chunk on first error deadlocks IBD when `BLVM_IBD_PEERS` is a single LAN node:
/// the assigner requeues to the same peer, validation stalls, and `/common` profile runs hit timeout.
async fn send_block_getdata_with_retry(
    network: Arc<NetworkManager>,
    peer_addr: SocketAddr,
    wire_msg: Vec<u8>,
    height: u64,
) -> Result<()> {
    const MAX_ATTEMPTS: u32 = 30;
    const BASE_MS: u64 = 100;
    const MAX_WAIT_MS: u64 = 5_000;
    let mut attempt: u32 = 0;
    let mut reconnect_spawned = false;
    loop {
        match network.send_to_peer(peer_addr, wire_msg.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let msg = e.to_string();
                let is_gone = msg.contains("not found") || msg.contains("disconnected");
                if !reconnect_spawned && is_gone {
                    reconnect_spawned = true;
                    NetworkManager::spawn_outbound_reconnect_attempt(
                        Arc::clone(&network),
                        peer_addr,
                    );
                }
                attempt += 1;
                let wait_ms = BASE_MS
                    .saturating_mul(1u64 << (attempt - 1).min(6))
                    .min(MAX_WAIT_MS);
                if attempt >= MAX_ATTEMPTS {
                    return Err(e).with_context(|| {
                        format!(
                            "Failed to send GetData for block at height {height} after {MAX_ATTEMPTS} attempts"
                        )
                    });
                }
                if attempt <= 3 || attempt % 5 == 0 {
                    warn!(
                        "GetData send failed for height {} (attempt {}/{}): {} — retrying in {}ms",
                        height, attempt, MAX_ATTEMPTS, e, wait_ms
                    );
                }
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            }
        }
    }
}

/// First height to request when retrying a chunk after partial validation progress.
pub(crate) fn resume_download_height(
    start_height: u64,
    end_height: u64,
    validated_tip: u64,
) -> Option<u64> {
    if validated_tip >= end_height {
        return None;
    }
    let resume = start_height.max(validated_tip.saturating_add(1));
    if resume > end_height {
        None
    } else {
        Some(resume)
    }
}

/// Outer wall-clock budget for one chunk download (per-block timeout × remaining blocks).
///
/// Single-height micro-chunks (`start == end`) arise from stall recovery and use a longer
/// minimum (4× per-block timeout, ≥120s) so the block can actually arrive over slow WAN
/// before the outer `tokio::timeout` kills the future.  The standard 35s minimum fires
/// before the inner per-block deadline can even report back, causing the entire retry storm:
/// all 8 racing peers die at 35s simultaneously, re-queue, and repeat.
pub(crate) fn chunk_outer_deadline_secs(
    start_height: u64,
    end_height: u64,
    resume_from: u64,
    per_block_timeout_secs: u64,
) -> u64 {
    if start_height == end_height {
        // Stall-recovery micro-chunk: give the block room to arrive over slow WAN.
        // Inner chunk_deadline_secs still handles genuine stuck peers.
        return per_block_timeout_secs
            .saturating_mul(4)
            .clamp(120, 7200);
    }
    let blocks_remaining = end_height
        .saturating_sub(resume_from)
        .saturating_add(1)
        .max(1);
    per_block_timeout_secs
        .saturating_mul(blocks_remaining)
        .clamp(35, 7200)
}

/// Worker-level outer budget passed into `download_chunk` (cooperative + safety-net).
///
/// WAN deep tip-owner pipes (`span ≥ 64` past body tip) must not use uncapped
/// `per_block × 128` (live: 5760s safety-net while coordinator shows covering=0).
pub(crate) fn worker_chunk_outer_deadline_secs(
    start_height: u64,
    end_height: u64,
    resume_from: u64,
    per_block_timeout_secs: u64,
    confirmed_body_height: u64,
) -> u64 {
    let base = chunk_outer_deadline_secs(
        start_height,
        end_height,
        resume_from,
        per_block_timeout_secs,
    );
    if confirmed_body_height > 0
        && start_height > confirmed_body_height
        && end_height.saturating_sub(start_height) >= 63
    {
        // Belt with D0: cap cooperative/safety-net outer on WAN tip pipes.
        let cap = wan_deep_tip_pipe_chunk_deadline_secs(
            start_height,
            end_height,
            confirmed_body_height,
            per_block_timeout_secs,
        )
        .saturating_mul(2)
        .clamp(60, 120);
        base.min(cap)
    } else {
        base
    }
}

/// Wait for outbound peer connection, spawning reconnect if needed.
async fn wait_for_peer_connected(
    network: &Arc<NetworkManager>,
    peer_addr: SocketAddr,
    peer_id: &str,
    max_wait: Duration,
    tip_enter: &Option<Arc<super::chunk_assigner::ChunkAssigner>>,
) -> Result<()> {
    // Check eviction FIRST — an IP that has been permanently evicted this session
    // must be rejected even if it briefly reconnects.
    {
        let evicted = network.ibd_evicted_ips.read().unwrap();
        if evicted.contains(&peer_addr.ip()) {
            return Err(anyhow::anyhow!(
                "Peer {peer_id} evicted (NODE_NETWORK_LIMITED) — chunk needs retry on another peer"
            ));
        }
    }
    if network.is_peer_connected(peer_addr).await {
        return Ok(());
    }
    NetworkManager::spawn_outbound_reconnect_attempt(Arc::clone(network), peer_addr);
    let deadline = tokio::time::Instant::now() + max_wait;
    let mut poll = tokio::time::interval(Duration::from_millis(200));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await;
    while tokio::time::Instant::now() < deadline {
        if tip_enter
            .as_ref()
            .is_some_and(|a| a.is_peer_blacklisted(peer_id))
        {
            return Err(anyhow::anyhow!(
                "Peer {peer_id} blacklisted during connect wait — chunk needs retry"
            ));
        }
        if network.is_peer_connected(peer_addr).await {
            return Ok(());
        }
        poll.tick().await;
    }
    Err(anyhow::anyhow!(
        "Peer {peer_id} not connected after {}s — chunk needs retry",
        max_wait.as_secs()
    ))
}

/// P0-A: wait for Bitcoin handshake (VerAck) before sending GetData.
async fn wait_for_peer_ibd_ready(
    network: &Arc<NetworkManager>,
    peer_addr: SocketAddr,
    peer_id: &str,
    max_wait: Duration,
    tip_enter: &Option<Arc<super::chunk_assigner::ChunkAssigner>>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + max_wait;
    let mut poll = tokio::time::interval(Duration::from_millis(200));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await;
    while tokio::time::Instant::now() < deadline {
        if tip_enter
            .as_ref()
            .is_some_and(|a| a.is_peer_blacklisted(peer_id))
        {
            return Err(anyhow::anyhow!(
                "Peer {peer_id} blacklisted during handshake wait — chunk needs retry"
            ));
        }
        if network.peer_ibd_ready(peer_addr).await {
            return Ok(());
        }
        poll.tick().await;
    }
    Err(anyhow::anyhow!(
        "Peer {peer_id} handshake not complete after {}s — chunk needs retry",
        max_wait.as_secs()
    ))
}

/// Register a block download and send GetData. Fails fast when the peer is gone (avoids orphan
/// pending requests that time out and blacklist the whole chunk).
async fn register_and_request_block(
    network: Arc<NetworkManager>,
    peer_addr: SocketAddr,
    peer_id: &str,
    block_hash: Hash,
    height: u64,
) -> Result<tokio::sync::oneshot::Receiver<(Block, Vec<Vec<Witness>>, Option<Vec<u8>>)>> {
    if !network.is_peer_connected(peer_addr).await {
        return Err(anyhow::anyhow!(
            "Peer {peer_id} not connected — cannot request block at height {height}"
        ));
    }
    let block_rx = network.register_block_request(peer_addr, block_hash);
    let inventory = vec![InventoryVector {
        inv_type: MSG_WITNESS_BLOCK,
        hash: block_hash,
    }];
    let wire_msg =
        ProtocolParser::serialize_message(&ProtocolMessage::GetData(GetDataMessage { inventory }))?;
    if let Err(e) =
        send_block_getdata_with_retry(Arc::clone(&network), peer_addr, wire_msg, height).await
    {
        network.cancel_block_request(peer_addr, block_hash);
        return Err(e);
    }
    super::tip_stage::mark_getdata(height);
    Ok(block_rx)
}

type PendingBlockResult = (
    u64,
    [u8; 32],
    std::time::Instant,
    Result<
        Result<(Block, Vec<Vec<Witness>>, Option<Vec<u8>>), tokio::sync::oneshot::error::RecvError>,
        tokio::time::error::Elapsed,
    >,
    Option<tokio::sync::OwnedSemaphorePermit>,
);

type PendingBlockFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = PendingBlockResult> + Send>>;

/// Maximum block hashes per `getdata` message for batched IBD download.
///
/// Default **64** matches a31 peak (`GETDATA_BATCH_SIZE`). Override
/// `BLVM_IBD_GETDATA_BATCH` (1..=64).
fn getdata_batch_size() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_GETDATA_BATCH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64)
            .clamp(1, 64)
    })
}

/// Enqueue a batch of network block downloads using a single `getdata` message.
///
/// All heights in the batch must not be locally available (call the standard
/// `enqueue_chunk_block` for heights where `try_load_local_ibd_block` succeeds).
///
/// On any error all registered receivers for this batch are cancelled before returning.
#[allow(clippy::too_many_arguments)]
async fn enqueue_network_block_batch(
    heights_and_hashes: Vec<(u64, [u8; 32])>,
    permits: Vec<Option<tokio::sync::OwnedSemaphorePermit>>,
    network: &Arc<NetworkManager>,
    peer_addr: SocketAddr,
    peer_id: &str,
    validation_tip: u64,
    confirmed_body_height: u64,
    chunk_default_secs: u64,
    in_flight: &mut FuturesUnordered<PendingBlockFuture>,
    in_flight_heights: &mut HashSet<u64>,
    inflight_deadlines: &mut HashMap<u64, Arc<AtomicU64>>,
    first_block_logged: &mut bool,
    start_height: u64,
    end_height: u64,
) -> Result<()> {
    debug_assert_eq!(heights_and_hashes.len(), permits.len());
    if heights_and_hashes.is_empty() {
        return Ok(());
    }

    let hashes: Vec<[u8; 32]> = heights_and_hashes.iter().map(|(_, h)| *h).collect();
    let mut rxs = network.register_block_requests_batch(peer_addr, &hashes);

    // Build a single GetData message for all blocks in the batch.
    let inventory: Vec<InventoryVector> = hashes
        .iter()
        .map(|&hash| InventoryVector {
            inv_type: MSG_WITNESS_BLOCK,
            hash,
        })
        .collect();
    let wire_msg = ProtocolParser::serialize_message(&ProtocolMessage::GetData(GetDataMessage {
        inventory,
    }))?;

    let first_height = heights_and_hashes[0].0;
    if let Err(e) =
        send_block_getdata_with_retry(Arc::clone(network), peer_addr, wire_msg, first_height).await
    {
        // Cancel all registered receivers before propagating the error.
        for &hash in &hashes {
            network.cancel_block_request(peer_addr, hash);
        }
        return Err(e);
    }
    for &(height, _) in &heights_and_hashes {
        super::tip_stage::mark_getdata(height);
    }

    if !*first_block_logged {
        info!(
            "[IBD] {} chunk {}-{}: batch-requested {} blocks starting at height {} (hash {})",
            peer_id,
            start_height,
            end_height,
            heights_and_hashes.len(),
            first_height,
            hex::encode(hashes[0])
        );
        *first_block_logged = true;
    }

    for (((height, block_hash), permit), rx) in heights_and_hashes
        .into_iter()
        .zip(permits.into_iter())
        .zip(rxs.drain(..))
    {
        let secs = block_gap_timeout_secs(
            height,
            validation_tip,
            confirmed_body_height,
            start_height,
            end_height,
            chunk_default_secs,
        );
        push_network_inflight(
            in_flight,
            in_flight_heights,
            inflight_deadlines,
            height,
            block_hash,
            rx,
            permit,
            secs,
        );
    }
    Ok(())
}

/// Non-blocking take of a per-peer in-flight permit.
///
/// Returns `Ok(None)` when the semaphore is exhausted — caller must return to `select!` and
/// poll `in_flight` (those futures hold the permits). Blocking `acquire_owned` here while
/// `in_flight` is unpolled self-deadlocks when `pipeline_depth == sem capacity` (live WAN
/// tip pipes: 128-deep, 5760s safety-net, 0 soft timeouts).
fn try_take_blocks_permit(
    blocks_sem: &Option<Arc<Semaphore>>,
) -> Result<Option<Option<tokio::sync::OwnedSemaphorePermit>>> {
    match blocks_sem {
        None => Ok(Some(None)),
        Some(sem) => match sem.clone().try_acquire_owned() {
            Ok(p) => Ok(Some(Some(p))),
            Err(tokio::sync::TryAcquireError::NoPermits) => Ok(None),
            Err(tokio::sync::TryAcquireError::Closed) => {
                Err(anyhow::anyhow!("blocks semaphore closed"))
            }
        },
    }
}

/// Enqueue one block download (local replay or network GetData). Skips if already in flight.
async fn enqueue_chunk_block(
    height: u64,
    block_hash: [u8; 32],
    network: &Arc<NetworkManager>,
    peer_addr: SocketAddr,
    peer_id: &str,
    blockstore: &BlockStore,
    protocol_version: ProtocolVersion,
    validation_tip: u64,
    confirmed_body_height: u64,
    chunk_default_secs: u64,
    blocks_sem: &Option<Arc<Semaphore>>,
    in_flight: &mut FuturesUnordered<PendingBlockFuture>,
    in_flight_heights: &mut HashSet<u64>,
    inflight_deadlines: &mut HashMap<u64, Arc<AtomicU64>>,
    first_block_logged: &mut bool,
    start_height: u64,
    end_height: u64,
    local_sourced_heights: &mut HashSet<u64>,
) -> Result<()> {
    if in_flight_heights.contains(&height) {
        return Ok(());
    }
    let Some(permit) = try_take_blocks_permit(blocks_sem)? else {
        // Pipe full — leave height free for a later fill_pipeline after in_flight drains.
        return Ok(());
    };
    if let Some((block, block_witnesses)) =
        try_load_local_ibd_block(blockstore, height, block_hash, protocol_version)?
    {
        if !*first_block_logged {
            info!(
                "[IBD] {} chunk {}-{}: local block height {} (hash {})",
                peer_id,
                start_height,
                end_height,
                height,
                hex::encode(block_hash)
            );
            *first_block_logged = true;
        }
        let request_start = std::time::Instant::now();
        in_flight_heights.insert(height);
        local_sourced_heights.insert(height);
        in_flight.push(Box::pin(async move {
            let r = Ok(Ok((block, block_witnesses, None)));
            (height, block_hash, request_start, r, permit)
        }));
    } else {
        if !*first_block_logged {
            info!(
                "[IBD] {} chunk {}-{}: registered block height {} (hash {})",
                peer_id,
                start_height,
                end_height,
                height,
                hex::encode(block_hash)
            );
            *first_block_logged = true;
        }
        let block_rx =
            register_and_request_block(Arc::clone(network), peer_addr, peer_id, block_hash, height)
                .await?;
        let secs = block_gap_timeout_secs(
            height,
            validation_tip,
            confirmed_body_height,
            start_height,
            end_height,
            chunk_default_secs,
        );
        push_network_inflight(
            in_flight,
            in_flight_heights,
            inflight_deadlines,
            height,
            block_hash,
            block_rx,
            permit,
            secs,
        );
    }
    Ok(())
}

/// After [`try_stream_validation_gap`] pushes tip (+ optional consecutive drain) to the
/// coordinator, advance the download cursor past everything already streamed.
///
/// Tip may still be clone-held in `received` (H5/A1); cursor must still advance past
/// `GAP_STREAM_DEDUP` so W101b tip-hole pipe caps disarm (`next_to_send` past tip).
///
/// Land E: cheese drain can leave DEDUP *ahead* of a still-missing validation gap
/// (STREAM H, drain H+1…, coordinator drops H). Jumping `next_to_send` to DEDUP+1
/// completed the 128-span empty (~170ms), cancelled GetData for H, and re-assigned
/// the same map chunk. When the gap is still missing, only advance when DEDUP is
/// exactly this cursor (we just streamed it).
fn sync_next_to_send_after_gap_stream(next_to_send: &mut u64, end_height: u64) {
    let dedup = super::memory::GAP_STREAM_DEDUP_HEIGHT.load(Ordering::Relaxed);
    if dedup < *next_to_send {
        return;
    }
    let advanced = dedup.saturating_add(1).min(end_height.saturating_add(1));
    if advanced > *next_to_send {
        *next_to_send = advanced;
    }
}

/// Rewind [`GAP_STREAM_DEDUP_HEIGHT`] when cheese-ahead DEDUP covers a hole that is
/// not in `received`. W50 LOCAL_GAP wait never fills WAN-stripped heights.
///
/// Returns the rewind height when DEDUP moved. Caller must already know the tip is
/// not validation-taken / release-latched and not buffered.
#[allow(dead_code)] // kept for unit tests; a31 STREAM path does not call this
pub(crate) fn rewind_gap_stream_dedup_over_missing_hole(
    gap: u64,
    last_streamed: u64,
) -> Option<u64> {
    if last_streamed < gap {
        return None;
    }
    if !super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed) {
        return None;
    }
    let rewind = gap.saturating_sub(1);
    match super::memory::GAP_STREAM_DEDUP_HEIGHT.compare_exchange(
        last_streamed,
        rewind,
        Ordering::Relaxed,
        Ordering::Relaxed,
    ) {
        Ok(_) => Some(rewind),
        Err(_) => None,
    }
}

/// When validation races ahead of this worker's sequential `next_to_send`, skip already-
/// validated heights so we fetch the live gap immediately (OOM wedge at h=499231: worker
/// held chunk 499201-499232 while validation sat at 499230 waiting for 499231).
/// W13: returns true when the entire chunk is behind the validation tip (obsolete).
/// Caller should flush and return Ok so the peer can take tip work.
fn resync_next_to_send_with_validation_tip(
    validation_height: Option<&Arc<AtomicU64>>,
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    next_to_send: &mut u64,
    end_height: u64,
    network: &Arc<NetworkManager>,
    peer_addr: SocketAddr,
    block_hash_by_height: &BTreeMap<u64, [u8; 32]>,
    in_flight_heights: &HashSet<u64>,
    last_gap_at: &mut std::time::Instant,
) -> bool {
    let Some(vh) = validation_height else {
        return false;
    };
    let tip = vh.load(Ordering::Relaxed);
    let need = tip.saturating_add(1);
    // W13: whole chunk obsolete (common for stall-recovery (H,H) after tip advanced).
    // Old code returned early when need > end_height, leaving next_to_send stuck and
    // soft-retrying forever (live: 30 behind-tip soft retries at 6848xx).
    if tip >= end_height || need > end_height {
        for &h in in_flight_heights.iter() {
            if let Some(&hash) = block_hash_by_height.get(&h) {
                network.cancel_block_request(peer_addr, hash);
            }
        }
        // W55: do NOT `received.clear()` here — caller `flush_received_on_abort` must drain
        // first. Pre-W55 clear dropped hash-verified bodies; flush then saw an empty map
        // (silent re-download). Leave entries for flush; drop leftovers when chunk returns.
        *next_to_send = end_height.saturating_add(1);
        *last_gap_at = std::time::Instant::now();
        return true;
    }
    if need <= *next_to_send {
        return false;
    }
    while *next_to_send < need {
        let h = *next_to_send;
        // G5: preserve the validation gap block (and height `need`) in `received` so
        // `try_stream_validation_gap` can deliver it — silent remove caused multi-minute stalls.
        if h == need || (h == need.saturating_sub(1) && received.contains_key(&h)) {
            if in_flight_heights.contains(&h) {
                if let Some(&hash) = block_hash_by_height.get(&h) {
                    network.cancel_block_request(peer_addr, hash);
                }
            }
            *next_to_send += 1;
            continue;
        }
        let _ = received_take(received, h);
        if in_flight_heights.contains(&h) {
            if let Some(&hash) = block_hash_by_height.get(&h) {
                network.cancel_block_request(peer_addr, hash);
            }
        }
        *next_to_send += 1;
    }
    *last_gap_at = std::time::Instant::now();
    false
}

/// Stream the block validation is waiting on (`validation_height + 1`) as soon as it
/// arrives, even when earlier heights in this chunk are still in flight.  Without this,
/// in-order `next_to_send` streaming traps the gap block behind a slow first block in the
/// chunk — validation sits in a 30s feeder stall while witnesses for the gap height are
/// already on disk (observed h≈489536: chunk 489505-489536 batch-requested, gap waited 69s).
///
/// A6l: after streaming the gap, also drain any **consecutive** heights already buffered in
/// `received` into the feeder. Live A6k vs A6i: inter-GAP_STREAM burst rate (dt<50ms) was
/// **1% vs 8.8%** — out-of-order arrivals left tip+1..tip+N sitting in `received` until the
/// next network event called try_stream again (vh still points at the just-streamed gap, so
/// a single-shot try_stream cannot advance). Draining consecutive fills the feeder without
/// waiting for another block arrival.
///
/// **W100 (reverted 2026-07-18):** tip-only STREAM/RESEND (no consecutive drain) left
/// `GAP_DRAIN=0` but `bridge_min>nn` still ~79% and tip60 collapsed to slow-fail ~31 @330k.
/// Ahead fill is not drain-dominated; RESEND storm (tip lost after STREAM) remains. Restore
/// A6l drain for throughput.
async fn try_stream_validation_gap(
    validation_height: Option<&Arc<AtomicU64>>,
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    block_tx: Option<&tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>>,
    start_height: u64,
    end_height: u64,
) -> Result<bool> {
    let Some(vh) = validation_height else {
        return Ok(false);
    };
    let gap = vh.load(Ordering::Relaxed).saturating_add(1);
    if gap < start_height || gap > end_height {
        return Ok(false);
    }
    // W27/A6l/W38: GAP_STREAM_DEDUP tracks the highest height pushed to the coordinator.
    // Consecutive drain can advance it far ahead of validation. If validation still needs
    // `gap` and we have that height buffered again (local reload / re-download), we MUST
    // re-send — skipping left live 2026-07-16 soft-resume frozen at h=440399 for >5 min
    // while download logged "local block height 440400" thousands of times with no
    // IBD_GAP_STREAM (DEDUP had advanced through chunk 440370-440433).
    let last_streamed = super::memory::GAP_STREAM_DEDUP_HEIGHT.load(Ordering::Relaxed);
    let Some(tx) = block_tx else {
        return Ok(false);
    };
    if gap <= last_streamed {
        // Already STREAM'd once. Skip if validation/latch already owns the tip — clone-held
        // bodies in `received` must not become a 750ms RESEND pump on the healthy path.
        if super::tip_stage::tip_taken_by_validation(gap)
            || super::tip_release::tip_release_holds(gap)
        {
            return Ok(false);
        }
        let Some((block, block_witnesses)) = received_clone(received, gap) else {
            // W50: tip DEDUP'd but not buffered — wait for LOCAL_GAP/reload; do not drain ahead.
            return Ok(false);
        };
        // W42b: at most one resend per height / 750ms (inject races used to hit 599k RESEND).
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last_h = super::memory::GAP_STREAM_LAST_RESEND_HEIGHT.load(Ordering::Relaxed);
        let last_ms = super::memory::GAP_STREAM_LAST_RESEND_MS.load(Ordering::Relaxed);
        if gap == last_h && now_ms.saturating_sub(last_ms) < 750 {
            return Ok(false);
        }
        let tip_on_channel = stream_tip_to_coordinator(tx, gap, block, block_witnesses).await?;
        super::memory::GAP_STREAM_LAST_RESEND_HEIGHT.store(gap, Ordering::Relaxed);
        super::memory::GAP_STREAM_LAST_RESEND_MS.store(now_ms, Ordering::Relaxed);
        tracing::warn!(
            "[IBD_GAP_STREAM_RESEND] height {} (last_streamed={}) — tip still missing after prior drain",
            gap,
            last_streamed
        );
        // NOTE: Do NOT set SYNTH_FORCE_TIP_RELOAD here. Live 2026-07-23: RESEND→claim
        // clear every ~10s became the cliff cadence (~6.4 BPS from ignition).
        super::memory::bump_gap_stream_dedup(gap);
        // W6/W50: if tip was latched (channel full), do not drain ahead past a missing tip.
        if tip_on_channel {
            let drained_n =
                drain_consecutive_received_after(received, block_tx, gap, end_height).await?;
            if drained_n > 0 {
                tracing::warn!(
                    "[IBD_GAP_DRAIN] after={} drained={} through={} (chunk {}-{})",
                    gap,
                    drained_n,
                    gap.saturating_add(drained_n),
                    start_height,
                    end_height
                );
            }
        }
        return Ok(true);
    }
    // H5/A1 first STREAM: clone to coordinator; leave tip keyed in `received` until
    // validation advances (trim evicts `h < need`). Removes the reinject/RESEND tax when
    // the tip is dropped from reorder/bridge before apply.
    let Some((block, block_witnesses)) = received_clone(received, gap) else {
        return Ok(false);
    };
    let tip_on_channel = stream_tip_to_coordinator(tx, gap, block, block_witnesses).await?;
    super::memory::bump_gap_stream_dedup(gap);
    // W6/W50: latched tip is not yet in block_tx — do not push tip+N ahead of it.
    if tip_on_channel {
        let drained_n =
            drain_consecutive_received_after(received, block_tx, gap, end_height).await?;
        if drained_n > 0 {
            tracing::warn!(
                "[IBD_GAP_DRAIN] after={} drained={} through={} (chunk {}-{})",
                gap,
                drained_n,
                gap.saturating_add(drained_n),
                start_height,
                end_height
            );
        }
    }
    Ok(true)
}

/// Stream validation tip to the coordinator without stranding on admission backpressure.
///
/// Returns `true` when the tip is on `block_tx` (safe to consecutive-drain). Returns `false`
/// when W6 latched the tip because the channel was full (release-side; never `send.await`).
async fn stream_tip_to_coordinator(
    tx: &tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>,
    gap: u64,
    block: SharedBlock,
    witnesses: SharedWitnesses,
) -> Result<bool> {
    if !super::tip_release::release_side_drain_enabled() {
        if tx.send((gap, block, witnesses)).await.is_err() {
            return Err(anyhow::anyhow!(
                "block_tx closed during gap stream - chunk needs retry"
            ));
        }
        return Ok(true);
    }
    match tx.try_send((gap, block, witnesses)) {
        Ok(()) => Ok(true),
        Err(tokio::sync::mpsc::error::TrySendError::Full((h, b, w))) => {
            let prev = super::tip_release::offer_tip_release(h, b, w);
            warn!(
                "[IBD_TIP_RELEASE_LATCH] h={} channel_full capacity={} prev_latched={:?} — release-side (no send.await)",
                h,
                tx.capacity(),
                prev
            );
            Ok(false)
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(anyhow::anyhow!(
            "block_tx closed during gap stream - chunk needs retry"
        )),
    }
}

/// Push tip+1, tip+2, … already in `received` into the feeder (stops at first hole).
/// Returns the number of consecutive heights drained.
async fn drain_consecutive_received_after(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    block_tx: Option<&tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>>,
    after_height: u64,
    end_height: u64,
) -> Result<u64> {
    let Some(tx) = block_tx else {
        return Ok(0);
    };
    let mut next = after_height.saturating_add(1);
    let mut drained = 0u64;
    // Prefer leaving headroom for tip; drain is tip+1… so always non-tip relative to
    // validation tip at stream time (after_height was just streamed as tip).
    while next <= end_height {
        let Some((block, block_witnesses)) = received_take(received, next) else {
            break;
        };
        // after_height was just streamed as tip; validation tip_need is still that height
        // until applied — treat drain heights as non-tip for reserve.
        await_block_tx_tip_reserve(tx, next, Some(after_height)).await;
        if tx.send((next, block, block_witnesses)).await.is_err() {
            return Err(anyhow::anyhow!(
                "block_tx closed during consecutive gap drain - chunk needs retry"
            ));
        }
        super::memory::bump_gap_stream_dedup(next);
        drained = drained.saturating_add(1);
        next = next.saturating_add(1);
    }
    Ok(drained)
}

/// Before aborting a chunk, stream every block already sitting in `received` to the coordinator.
///
/// Without this, blocks already downloaded and hash-verified are silently dropped at chunk abort.
/// The coordinator re-queues and re-downloads them from scratch — visible as repeated
/// `received=N` counts in hard-deadline log lines (e.g. `received=13, next_to_send=H`).
///
/// Drains tip-contiguous blocks from `received` into the coordinator on abort.
///
/// **W60:** only flush `[next_needed, next_needed+1, …]` while contiguous. Dumping sparse
/// ahead (old behavior) hole-fills OrderedReadyBridge — live W59 genesis: `FLUSH_ON_ABORT`
/// 37× by h≈6k with `bmin>nn+1` 71% while tip crawl stalled. Matches W50 GAP_STREAM policy.
/// Returns the number of blocks successfully flushed.
async fn flush_received_on_abort(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    block_tx: Option<&tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>>,
    start_height: u64,
    end_height: u64,
    next_to_send: u64,
    validation_height: Option<&AtomicU64>,
) -> usize {
    let Some(tx) = block_tx else {
        return 0;
    };
    if received.is_empty() {
        return 0;
    }
    let buffered = received.len();
    let mut flushed = 0usize;
    if let Some(vh) = validation_height {
        let tip_needed = vh.load(Ordering::Relaxed).saturating_add(1);
        let min_h = received.keys().next().copied();
        let max_h = received.keys().next_back().copied();
        // W60b:
        // - tip in buffer → flush contiguous from tip (never sparse ahead past a hole)
        // - all buffered < tip → flush all (obsolete behind-tip; persist/reuse, no hole-fill)
        // - all buffered > tip → flush nothing (ahead-only would hole-fill the bridge)
        let ahead_only = min_h.is_some_and(|m| m > tip_needed);
        if ahead_only {
            let skipped = received.len();
            // Keep ahead bodies — clear() was the 409 leak (fetched at +113, discarded,
            // then empty 128 reassign ×35). Caller may still drop the map on abort.
            info!(
                "[IBD_FLUSH_ON_ABORT] chunk {}-{}: flushed 0 tip-contiguous block(s) (kept_ahead={}, buffered={}, next_to_send={}, tip_needed={})",
                start_height, end_height, skipped, buffered, next_to_send, tip_needed
            );
        } else if min_h.is_some_and(|m| m == tip_needed) || received.contains_key(&tip_needed) {
            let mut h = tip_needed;
            while let Some((block, witnesses)) = received_take(received, h) {
                if tx.send((h, block, witnesses)).await.is_err() {
                    break;
                }
                flushed += 1;
                h = h.saturating_add(1);
            }
            let skipped = received.len();
            received.clear();
            crate::node::parallel_ibd::memory::GAP_FLUSH_ON_ABORT_BLOCKS
                .fetch_add(flushed as u64, Ordering::Relaxed);
            info!(
                "[IBD_FLUSH_ON_ABORT] chunk {}-{}: flushed {} tip-contiguous block(s) (skipped_ahead={}, buffered={}, next_to_send={}, tip_needed={})",
                start_height, end_height, flushed, skipped, buffered, next_to_send, tip_needed
            );
        } else {
            // Behind tip (max < tip) or tip hole with some behind — drain ascending behind/at tip only.
            let _ = max_h;
            while let Some((&h, _)) = received.iter().next() {
                if h > tip_needed {
                    break;
                }
                let Some((block, witnesses)) = received_take(received, h) else {
                    break;
                };
                if h < tip_needed {
                    // Behind tip: still deliver for store/inject; coordinator drops if obsolete.
                }
                if tx.send((h, block, witnesses)).await.is_err() {
                    break;
                }
                flushed += 1;
            }
            let skipped = received.len();
            received.clear();
            if flushed > 0 || skipped > 0 {
                crate::node::parallel_ibd::memory::GAP_FLUSH_ON_ABORT_BLOCKS
                    .fetch_add(flushed as u64, Ordering::Relaxed);
                info!(
                    "[IBD_FLUSH_ON_ABORT] chunk {}-{}: flushed {} block(s) (skipped_ahead={}, buffered={}, next_to_send={}, tip_needed={})",
                    start_height, end_height, flushed, skipped, buffered, next_to_send, tip_needed
                );
            }
        }
    } else {
        // No validation cursor — legacy ascending drain.
        while let Some((&h, _)) = received.iter().next() {
            let Some((block, witnesses)) = received_take(received, h) else {
                break;
            };
            if tx.send((h, block, witnesses)).await.is_err() {
                break;
            }
            flushed += 1;
        }
        if flushed > 0 {
            crate::node::parallel_ibd::memory::GAP_FLUSH_ON_ABORT_BLOCKS
                .fetch_add(flushed as u64, Ordering::Relaxed);
            info!(
                "[IBD_FLUSH_ON_ABORT] chunk {}-{}: flushed {} buffered block(s) to coordinator (next_to_send={})",
                start_height, end_height, flushed, next_to_send
            );
        }
    }
    flushed
}

/// Cancel in-flight GETDATA, flush buffered blocks, return outer-deadline Err (S1).
async fn abort_on_outer_deadline(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    block_tx: Option<&tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>>,
    start_height: u64,
    end_height: u64,
    next_to_send: u64,
    validation_height: Option<&AtomicU64>,
    in_flight_heights: &HashSet<u64>,
    block_hash_by_height: &BTreeMap<u64, [u8; 32]>,
    network: &NetworkManager,
    peer_addr: SocketAddr,
    peer_scorer: &crate::network::peer_scoring::PeerScorer,
    outer_secs: u64,
) -> Result<DownloadChunkResult> {
    warn!(
        "[IBD] chunk {}-{} cooperative outer deadline ({}s) — flushing {} buffered block(s) before abort",
        start_height,
        end_height,
        outer_secs,
        received.len()
    );
    peer_scorer.record_failure(peer_addr);
    flush_received_on_abort(
        received,
        block_tx,
        start_height,
        end_height,
        next_to_send,
        validation_height,
    )
    .await;
    received_drain_all(received);
    for &h in in_flight_heights {
        if let Some(&h_hash) = block_hash_by_height.get(&h) {
            network.cancel_block_request(peer_addr, h_hash);
        }
    }
    Err(anyhow::anyhow!(
        "Chunk {}-{}: outer deadline {}s",
        start_height,
        end_height,
        outer_secs
    ))
}

/// W35‴ soft outer: extend deadline while tip is still streaming (>1 blk/s or recent GAP_STREAM).
/// Live: 119s cap aborted sticky mid-pipe while GAP_STREAM was live (~2 blk/s).
pub(crate) fn should_extend_outer_while_streaming(
    last_gap_at: std::time::Instant,
    gap_streams: u64,
    chunk_started: std::time::Instant,
    extensions_used: u32,
) -> bool {
    const MAX_EXTENDS: u32 = 4; // +60s × 4 = +240s ceiling
    if extensions_used >= MAX_EXTENDS {
        return false;
    }
    // Never extend before the first tip GAP_STREAM (avoids padding hung first-block waits).
    if gap_streams == 0 {
        return false;
    }
    // Recent tip stream — peer is delivering.
    if last_gap_at.elapsed() < Duration::from_secs(8) {
        return true;
    }
    let secs = chunk_started.elapsed().as_secs_f64().max(1.0);
    gap_streams as f64 / secs >= 1.0
}

fn outer_extend_secs() -> u64 {
    std::env::var("BLVM_IBD_OUTER_EXTEND_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60)
        .clamp(30, 120)
}

/// Returns true if outer deadline was extended (caller should continue the download loop).
fn try_soft_extend_outer(
    outer_deadline: &mut Option<tokio::time::Instant>,
    outer_extends: &mut u32,
    outer_deadline_secs: &mut u64,
    last_gap_at: std::time::Instant,
    gap_streams: u64,
    chunk_start_time: std::time::Instant,
    start_height: u64,
    end_height: u64,
    peer_id: &str,
) -> bool {
    if !should_extend_outer_while_streaming(
        last_gap_at,
        gap_streams,
        chunk_start_time,
        *outer_extends,
    ) {
        return false;
    }
    let add = outer_extend_secs();
    *outer_extends = outer_extends.saturating_add(1);
    *outer_deadline_secs = outer_deadline_secs.saturating_add(add);
    *outer_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(add));
    warn!(
        "[IBD_OUTER_EXTEND] chunk {}-{} peer={} extend={}s total_budget={}s streams={} extends={}/4",
        start_height,
        end_height,
        peer_id,
        add,
        *outer_deadline_secs,
        gap_streams,
        *outer_extends
    );
    true
}

/// Sleep until cooperative outer deadline, or pending forever when unset.
async fn wait_cooperative_outer(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}

/// Max empty-witness re-requests per peer/chunk before abort (A6b).
/// Override via `BLVM_IBD_EMPTY_WITNESS_MAX` (default 4, clamp 2..=32).
pub(crate) fn empty_witness_hit_cap() -> u32 {
    std::env::var("BLVM_IBD_EMPTY_WITNESS_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
        .clamp(2, 32)
}

/// W10: heights beyond `validation_tip + band` are far-ahead (default band 128).
pub(crate) fn far_ahead_band() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_FAR_AHEAD_BAND")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(128)
            .clamp(32, 1024)
    })
}

/// W10/W28/P1-T/W80/W89: per-block timeout when the chunk covers the validation tip gap
/// (default **12s**). Soft-retry re-requests in place; shorter tip timeout rotates dead
/// owners faster. Live 2026-07-14 mid-chain: default 25s left getdata→body at 46s.
/// Breakthrough peers finish ~6s (SLA notes); W76 tried default **5** and regresseds
/// pure-WAN tip60 ~40→~7 (CAP thrash / abort-before-delivery). W74/W76 kept **8**,
/// but loop-1 @313–320k had tip `getdata→body` p90≈**11.5s** with CAP=8 →
/// `soft exhausted, aborting` ~2/min (abort-before-delivery). Default **12** sits
/// just above that p90; floor **8** keeps early-height rotate snappy.
///
/// **W89/W89b/W93/W94:** shorter CAP (default **8s**, `BLVM_IBD_TIP_HOLE_GAP_TIMEOUT_SECS`)
/// when tip distress is proven, not on every holey poll.
/// Live W88 @328690: covering=2 / holes=23 waited full 12s → freeze 21s. Live W89:
/// standing ahead-OOO holes kept CAP=5 always → tip60 **69→27**. Trigger at 4s + holes
/// keeps healthy holey pipes on CAP=12 while rotating true tip holes before watcher 20s.
/// **W93:** hole CAP default **5→8**.
/// **W94:** live W93 freeze @314596 had **holes=0 / bridge_pending=0** (pure tip starve)
/// so W89b never shortened CAP — waited full 12s then still missed watcher 20s.
/// Empty-bridge tip gap (`BRIDGE_PENDING_COUNT==0` + `IBD_TIP_GAP_MISSING`) uses the
/// short CAP after a longer trigger (default **8s**) so healthy awaiting≈4–6s pipes
/// stay on base CAP=12.
pub(crate) fn tip_gap_timeout_secs() -> u64 {
    tip_gap_timeout_secs_ex(false)
}

/// Tip-gap CAP. `ahead_buffered` is accepted for call-site symmetry with soft budget
/// (W108/W109) but does **not** lengthen mute CAP — live W109 soak: elevating CAP on
/// ahead made soft-resume mute rotate sluggish; W109b keeps mute at **5s** and relies on
/// soft=1 (keep pipe / re-GetData tip) when ahead is already buffered.
///
/// Prefer [`tip_gap_timeout_secs_for_chunk`] when chunk span is known (W139: empty deep
/// uses 8s; empty `(H,H)` stays mute 5s).
pub(crate) fn tip_gap_timeout_secs_ex(_ahead_buffered: bool) -> u64 {
    tip_gap_timeout_secs_for_chunk(_ahead_buffered, 0, 0)
}

/// Tip-gap CAP with chunk span (W139).
///
/// Empty-bridge **deep** pipes use `holey_cap` (default **16s**, W184) — live W138
/// @316114: empty mute CAP=5s + soft=1 burned by progressive → mute abort before body.
/// Empty `(H,H)` micros stay on mute **5s**.
pub(crate) fn tip_gap_timeout_secs_for_chunk(
    ahead_buffered: bool,
    chunk_start: u64,
    chunk_end: u64,
) -> u64 {
    let base = latch_env!(u64, {
        std::env::var("BLVM_IBD_TIP_GAP_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            // W184: **12→16**. Live W182 354–362k getdata→body p90≈15.4s under CAP=12
            // → abort-before-delivery → covering=0 → rate-fail @361k.
            .unwrap_or(16)
            .clamp(8, 60)
    });
    let holes = crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
    // W103: hole/empty mute CAP default **5s**.
    let hole_cap = {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_HOLE_GAP_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5)
        });
        raw.clamp(3, base)
    };
    // W182: holey **8→12**. W184: **12→16** (same class — mid/late p90 above CAP).
    // Match base CAP when runway is already buffered (pending>0); empty mute stays 5s.
    let holey_cap = {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_HOLEY_PENDING_CAP_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(16)
        });
        raw.clamp(hole_cap, base)
    };
    let trigger = {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_HOLE_CAP_TRIGGER_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3)
        });
        raw.clamp(2, hole_cap)
    };
    // W104: empty-bridge trigger default **0** (was 5).
    let empty_trigger = {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_EMPTY_BRIDGE_CAP_TRIGGER_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        });
        raw.clamp(0, base)
    };
    let awaiting = super::tip_stage::tip_awaiting_secs_for_cap();
    let pending = crate::node::parallel_ibd::memory::BRIDGE_PENDING_COUNT.load(Ordering::Relaxed);
    let gap_missing = crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed);
    let deep = chunk_end > chunk_start;
    // W109: ahead already buffered (this pipe OR coordinator reorder cheese) must not
    // wait holey 16s — that is soak 13/17 tip90≈50–93 (406–411 leftover). Soft-retry
    // re-GetData's tip H in place (W72/W108). Empty deep with no cheese stays 16s (W184).
    let cheese = ahead_buffered
        || crate::node::parallel_ibd::IBD_REORDER_AHEAD.load(Ordering::Relaxed) > 0;
    if holes > 0 {
        // W89b: standing holes without await keep base (do not fall through to empty).
        if awaiting >= trigger {
            // W170: single tip hole with runway already buffered (= empty-hole class).
            // Live W169 @317772: holes=1 pending=61 used holey CAP 8s + mid SLA 90s;
            // mute CAP 5s matches empty tip hole and pairs with floor-SLA rotate.
            if holes == 1 && pending > 0 {
                return hole_cap;
            }
            // W106/W184: holes + bridge pending → holey CAP (default 16s); empty tip
            // hole keeps mute CAP 5s. W176 export floor is a no-op when holey==base.
            if pending > 0 && !cheese {
                return tip_cap_during_export(holey_cap, base);
            }
            return tip_cap_during_export(hole_cap, base);
        }
        return base;
    }
    if gap_missing && pending == 0 && awaiting >= empty_trigger {
        // W139/W184: empty deep → holey CAP (16s) only when no cheese. TIP_HOLE_AHEAD
        // and (H,H) stay mute 5s (W109).
        return if deep && !cheese {
            tip_cap_during_export(holey_cap, base)
        } else {
            tip_cap_during_export(hole_cap, base)
        };
    }
    base
}

/// W176: while checkpoint export holds the compact lock, tip bodies often land after
/// the steady-state holey CAP. Floor tracks holey default (**16s**, W184).
///
/// W179b: do **not** apply this floor during post-local-ahead grace. Live W179 rewind
/// soak: grace+CAP12 held dead tip peers → tip60 stuck ~15–22 while sla5=0. Grace is
/// for tip-SLA (don't 5s-kill cold scores); CAP must still mute-rotate slow peers.
#[inline]
fn tip_cap_during_export(cap: u64, base: u64) -> u64 {
    if !crate::node::parallel_ibd::IBD_CHECKPOINT_EXPORT_ACTIVE.load(Ordering::Relaxed) {
        return cap;
    }
    let floor = {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_EXPORT_ACTIVE_CAP_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(16)
        });
        raw.clamp(8, base)
    };
    cap.max(floor).min(base)
}

/// W10: per-block timeout for far-ahead chunks (default 10s) — abort fast to free the peer.
pub(crate) fn far_ahead_timeout_secs() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_FAR_AHEAD_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10)
            .clamp(5, 30)
    })
}

/// W24/W28/P1-T: soft-retry budget for the exact validation tip gap (default **3**).
/// Deep WAN tip owner pipes need several in-place re-requests before peer rotate.
pub(crate) fn tip_gap_soft_retries() -> u32 {
    latch_env!(u32, {
        std::env::var("BLVM_IBD_TIP_GAP_SOFT_RETRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3)
            .clamp(1, 12)
    })
}

/// W2/N11: Hornet-style download **byte** budget (in addition to block-count caps).
///
/// `BLVM_IBD_DOWNLOAD_BYTE_BUDGET=0` / unset → off (W1b DNA). When set (bytes),
/// GetData fill depth tracks `budget / est_block_bytes` so small-block eras deepen
/// and fat-block eras stay memory-bounded. Clamp 8 MiB..=256 MiB.
pub(crate) fn download_byte_budget() -> Option<u64> {
    let raw = std::env::var("BLVM_IBD_DOWNLOAD_BYTE_BUDGET").ok()?;
    let n: u64 = raw.parse().ok()?;
    if n == 0 {
        return None;
    }
    Some(n.clamp(8 * 1024 * 1024, 256 * 1024 * 1024))
}

/// Running estimate of serialized block size for W2 fill-depth (default 1 MiB).
fn download_est_block_bytes() -> u64 {
    static EST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_000_000);
    EST.load(std::sync::atomic::Ordering::Relaxed).max(50_000)
}

fn note_download_block_bytes(nbytes: u64) {
    if nbytes == 0 {
        return;
    }
    static EST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_000_000);
    let old = EST.load(std::sync::atomic::Ordering::Relaxed).max(50_000);
    // EMA: 7/8 old + 1/8 new
    let next = old.saturating_mul(7).saturating_add(nbytes) / 8;
    EST.store(next.max(50_000), std::sync::atomic::Ordering::Relaxed);
}

/// W10/W12/W14/W24: soft-retry budget for a chunk-local gap height relative to the live tip.
/// - Behind tip (`height < tip+1`): **0** (obsolete — abort; live: 30/43 soft retries)
/// - Far ahead of tip: **0** (abort on first timeout — free peer for tip work)
/// - Exact tip gap: **tip_gap_soft_retries()** (default 3 — rotate after soft retries exhausted)
/// - Otherwise: **3** (existing P4)
///
/// Prefer [`gap_soft_retry_budget_for_chunk`] at the download soft-retry site so tip-gap
/// heights rotate after one soft miss (W69/W70).
pub(crate) fn gap_soft_retry_budget(height: u64, validation_tip: u64) -> u32 {
    let tip_needed = validation_tip.saturating_add(1);
    if height < tip_needed {
        0
    } else if height > tip_needed.saturating_add(far_ahead_band()) {
        0
    } else if height == tip_needed {
        tip_gap_soft_retries()
    } else {
        3
    }
}

/// Soft-retry budget with chunk span awareness (W69/W70/W74/W83/W107/W108/W138).
///
/// **Tip-gap height** (`height == tip+1`):
/// - Deep tip pipe (`chunk_end > chunk_start`): **1** (W83/W138) — one in-place soft then rotate
///   (empty bridge included; W107 soft=0 mute-stormed @310k before STREAM EWMA).
/// - `(H,H)` failover micro: **0** unless hot tip-STREAM (W115) — abort/rotate (W74).
/// - Empty bridge + ahead already buffered in this pipe: **1** (W108) — re-GetData tip,
///   keep ahead `received` (live W107 @355392: abort discarded 25 ahead × peer rotate).
///
/// Prefer [`gap_soft_retry_budget_for_chunk_ex`] at download sites that have `received`.
pub(crate) fn gap_soft_retry_budget_for_chunk(
    height: u64,
    validation_tip: u64,
    chunk_start: u64,
    chunk_end: u64,
) -> u32 {
    gap_soft_retry_budget_for_chunk_ex(
        height,
        validation_tip,
        chunk_start,
        chunk_end,
        false,
        false,
    )
}

/// Like [`gap_soft_retry_budget_for_chunk`] with pipe-local ahead visibility (W108)
/// and hot tip-STREAM tenure (W114).
pub(crate) fn gap_soft_retry_budget_for_chunk_ex(
    height: u64,
    validation_tip: u64,
    chunk_start: u64,
    chunk_end: u64,
    ahead_buffered: bool,
    hot_tip_streamer: bool,
) -> u32 {
    let tip_needed = validation_tip.saturating_add(1);
    if height == tip_needed {
        let pending =
            crate::node::parallel_ibd::memory::BRIDGE_PENDING_COUNT.load(Ordering::Relaxed);
        let gap_missing =
            crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed);
        if gap_missing && pending == 0 {
            // W158: empty deep soft=**2** always (revert W156/W157 awaiting soft=1).
            // W157@16s reached 338k peak tip60 99 — knife-edge vs W154@346k/89; soft=1
            // awaiting path not a clear win. Keep W146 empty (H,H) soft=2.
            if chunk_end > chunk_start {
                let _ = (ahead_buffered, hot_tip_streamer);
                return 2;
            }
            // W146: empty (H,H) soft=2. Soft=0 mute micro-rotate was W108.
            let _ = (ahead_buffered, hot_tip_streamer);
            return 2;
        }
        if chunk_end > chunk_start {
            // W171: soft=1 @holes≥20 only when bridge pending==0 (no runway). Live W170
            // @319140–154: holes=20–31 pending=33–47 soft=1 → (H,H) tip-step every ~2s
            // covering=2/2 → tip60~14 → rate-fail @321k. Keep soft=1 for holes≥32 always
            // (deep standing). W170 floor-SLA ≤0.20 + holes=1 CAP kept.
            let _ = (ahead_buffered, hot_tip_streamer);
            let holes =
                crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
            if holes >= 32 || (holes >= 20 && pending == 0) {
                return 1;
            }
            return 2;
        }
        // W171: holey (H,H) — same pending gate as deep.
        let _ = (hot_tip_streamer, ahead_buffered);
        let holes = crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
        if holes >= 32 || (holes >= 20 && pending == 0) {
            return 1;
        }
        return 2;
    }
    let _ = (chunk_start, chunk_end, ahead_buffered, hot_tip_streamer);
    gap_soft_retry_budget(height, validation_tip)
}

/// True when this download pipe has buffered any height strictly above the tip gap.
pub(crate) fn tip_pipe_has_ahead_buffered(
    received: &std::collections::BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    tip_needed: u64,
) -> bool {
    received.keys().any(|&h| h > tip_needed)
}

/// W110: tip-covering download failures that should use mute cooldown + failover arm.
///
/// Live W109b @311585: empty soft=0 CAP and "no first block" aborts returned
/// `Block timeout for gap height…` / `no first block in…` — worker only matched
/// `tip-gap timeout` for [`crate::node::parallel_ibd::chunk_assigner::ChunkAssigner::note_tip_owner_failed_mute`].
/// Plain tip fails used 15s cooldown and **skipped** WAN failover arm (W31) →
/// covering=1 mute thrash (~26s) until watcher rate-fail (32.4 < 35).
pub(crate) fn tip_covering_fail_is_mute(err_str: &str) -> bool {
    if err_str.contains("tip-SLA") || err_str.contains("tip-enter walk-in") {
        return false;
    }
    err_str.contains("tip-gap timeout")
        || err_str.contains("Block timeout for gap height")
        || err_str.contains("no first block in")
        // P1d: PIPE_FILL recv=0 mute eject (CAP-independent).
        || err_str.contains("PIPE_FILL mute")
}

/// P1d: after tip pipe is filled with network GetData and no network body arrives, abort.
/// Default **3000** ms. Env `BLVM_IBD_MUTE_PIPE_MS` (clamp 1000–15000).
pub(crate) fn mute_pipe_ms() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_MUTE_PIPE_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3000)
            .clamp(1000, 15_000)
    })
}

/// P1d clock policy (unit-tested): keep across local tip advances; clear only on tip-band
/// network body or when the mute episode ends.
pub(crate) fn pipe_mute_episode_active(
    gap_streams: u64,
    pipe_fill_recv0: bool,
    saw_network_body: bool,
    next_in_chunk: bool,
) -> bool {
    (gap_streams > 0 || pipe_fill_recv0) && !saw_network_body && next_in_chunk
}

/// Whether to clear `pipe_mute_armed_at` when the episode is not active.
pub(crate) fn pipe_mute_should_clear_clock(
    episode_active: bool,
    gap_streams: u64,
    pipe_fill_recv0: bool,
    saw_network_body: bool,
) -> bool {
    !episode_active && (saw_network_body || !(gap_streams > 0 || pipe_fill_recv0))
}

/// Whether mute may *fire* (clock elapsed) — defer while tip is already arriving.
pub(crate) fn pipe_mute_may_fire(tip_buffered: bool, tip_is_local_inflight: bool) -> bool {
    !(tip_buffered || tip_is_local_inflight)
}

/// C1: grow tip-hole GetData depth only as network bodies arrive (inverse mute).
/// Default **on**. Opt out: `BLVM_IBD_TIP_HOLE_GROW=0`.
pub(crate) fn tip_hole_grow_enabled() -> bool {
    latch_env!(bool, {
        match std::env::var("BLVM_IBD_TIP_HOLE_GROW")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("0") | Some("false") | Some("off") | Some("no") => false,
            _ => true,
        }
    })
}

/// Cap for tip-hole pipe (DNA / env `BLVM_IBD_TIP_HOLE_PIPE`, default 32, clamp 2–128).
pub(crate) fn tip_hole_pipe_cap() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_TIP_HOLE_PIPE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(32)
            .clamp(2, 128)
    })
}

/// C1b: cold max depth under grow-on-delivery (default **32**).
/// Soft peer-day: DNA `TIP_HOLE_PIPE=128` + grow→128 recreated Swiss cheese (iter10k wall≈7.5).
/// Mode T can raise via `BLVM_IBD_TIP_HOLE_GROW_CAP=128`. Clamp 2–pipe_cap.
/// C1n may temporarily raise the *effective* cap when getdata→body EWMA is fast.
pub(crate) fn tip_hole_grow_cap() -> usize {
    if !tip_hole_grow_enabled() {
        return tip_hole_pipe_cap();
    }
    latch_env!(usize, {
        std::env::var("BLVM_IBD_TIP_HOLE_GROW_CAP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(32)
            .clamp(2, 128)
            .min(tip_hole_pipe_cap())
    })
}

/// C1n: allow deepen past cold grow_cap only while getdata→body EWMA is fast.
/// C1m always-cap64 REGRESS on mute peerday; warm128 REGRESS on public WAN.
/// Default **on** — mute days stay at cold 32 (ewma ≥ gate or cold samples).
pub(crate) fn tip_hole_gd_fast_enabled() -> bool {
    latch_env!(bool, {
        match std::env::var("BLVM_IBD_TIP_HOLE_GD_FAST")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("0") | Some("false") | Some("off") | Some("no") => false,
            _ => true,
        }
    })
}

/// C1n/C1p: getdata→body EWMA must be **below** this (ms) to use fast cap (default **150**).
/// C1o@64 with gate=200 false-armed on mute day (EWMA briefly 196 while gd_p50≈2.2s).
pub(crate) fn tip_hole_gd_fast_ms() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_TIP_HOLE_GD_FAST_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(150)
            .clamp(50, 800)
    })
}

/// C1p: min EWMA samples before tip-hole may elevate (default **16**, match A6m).
/// C1o early n=8 let a short delivery burst arm FAST_CAP on a mute peerday.
pub(crate) fn tip_hole_gd_fast_n() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_TIP_HOLE_GD_FAST_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(16)
            .clamp(4, 64)
    })
}

/// C1n KEEP: fast grow cap when EWMA qualifies (default **48**).
/// C1n@48: wall≈294 / bursts~500 on good peerday. C1o@64 REGRESS on soft peerday
/// (false-positive EWMA arm). Stay at 48 on public WAN; Mode T may override.
pub(crate) fn tip_hole_grow_fast_cap() -> usize {
    let cold = tip_hole_grow_cap();
    latch_env!(usize, {
        std::env::var("BLVM_IBD_TIP_HOLE_GROW_FAST_CAP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(48)
            .clamp(cold, 96)
            .min(tip_hole_pipe_cap())
            .max(cold)
    })
}

/// Min tracked tip height before GD_FAST may elevate tip-hole cap (default **0**).
/// KEEP opened FAST @~405825 after surviving 403–405k at depth≤32. Rematch with
/// early FAST (tc293/298) dens-peaks then cliffs @~403.7k. Set e.g. **405000**.
pub(crate) fn tip_hole_gd_fast_min_h() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_TIP_HOLE_GD_FAST_MIN_H")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    })
}

/// C1n/C1o: cold grow_cap, or fast_cap when getdata EWMA is warm+fast.
pub(crate) fn tip_hole_grow_cap_effective() -> usize {
    let cold = tip_hole_grow_cap();
    if !tip_hole_grow_enabled() || !tip_hole_gd_fast_enabled() {
        return cold;
    }
    let min_h = tip_hole_gd_fast_min_h();
    if min_h > 0 && super::tip_stage::tracked_tip_height() < min_h {
        return cold;
    }
    let fast = tip_hole_grow_fast_cap();
    if fast <= cold {
        return cold;
    }
    match super::tip_stage::getdata_body_ewma_ms_min_n(tip_hole_gd_fast_n()) {
        Some((ms, _n)) if ms < tip_hole_gd_fast_ms() => fast,
        _ => cold,
    }
}

/// C1u: freeze tip-hole deepen + clamp fill while getdata→body EWMA is slow.
/// Default **on**. Post-DEDUP GPU ignition cliffs: grow 8→32 continued at
/// `gd_ewma≈5–8s` → `in_flight≈32` + ~10 BPS (`depth/gd`). Soft EWMA dips must
/// not thrash sticky (C1s); this gate matches A6m GD_SLOW (**800** ms).
pub(crate) fn tip_hole_gd_slow_enabled() -> bool {
    latch_env!(bool, {
        match std::env::var("BLVM_IBD_TIP_HOLE_GD_SLOW")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("0") | Some("false") | Some("off") | Some("no") => false,
            _ => true,
        }
    })
}

/// C1u: EWMA ≥ this (ms) → slow clamp (default **800**, same as `A6M_MAX_GETDATA_MS`).
pub(crate) fn tip_hole_gd_slow_ms() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_TIP_HOLE_GD_SLOW_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(800)
            .clamp(200, 5_000)
    })
}

/// C1u: min EWMA samples before slow clamp (default **16**).
pub(crate) fn tip_hole_gd_slow_n() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_TIP_HOLE_GD_SLOW_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(16)
            .clamp(4, 64)
    })
}

/// C1u: tip-hole fill / grown ceiling while GD_SLOW (default = grow_start **8**).
pub(crate) fn tip_hole_slow_fill_cap() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_TIP_HOLE_SLOW_FILL_CAP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(tip_hole_grow_start)
            .clamp(2, tip_hole_grow_cap())
    })
}

/// C1u′ ratchet (opt-in): step tip-hole depth down while GD_SLOW instead of cliff
/// `32→8`. Soak `T172520Z` with default-on + fill-clamp removed: tip60 FAIL @480k,
/// past-body **71.8** &lt; prior **81.1** — REVERT default. Opt in:
/// `BLVM_IBD_TIP_HOLE_GD_SLOW_RATCHET=1` (fill-time slow clamp stays on).
pub(crate) fn tip_hole_gd_slow_ratchet_enabled() -> bool {
    latch_env!(bool, {
        match std::env::var("BLVM_IBD_TIP_HOLE_GD_SLOW_RATCHET")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("1") | Some("true") | Some("on") | Some("yes") => true,
            _ => false,
        }
    })
}

/// Next tip-hole grown under GD_SLOW (ratchet or legacy cliff).
/// TPP L0b REVERT (300→320 L0b-20260801T032132Z): wall 378&lt;C0 390 — manual undo.
pub(crate) fn tip_hole_gd_slow_next_depth(grown: usize) -> usize {
    let slow = tip_hole_slow_fill_cap();
    if grown <= slow {
        return grown;
    }
    if !tip_hole_gd_slow_ratchet_enabled() {
        return slow;
    }
    grown.saturating_sub(tip_hole_grow_step()).max(slow)
}

/// Mode T sole archive: GD_SLOW arms the sole-floor path (see
/// [`tip_hole_sole_gd_slow_floor`]) instead of the multi-peer 32→8 cliff.
pub(crate) fn tip_hole_gd_slow_sole_keep(ibd_ready: usize) -> bool {
    ibd_ready <= 1 && tip_hole_gd_slow()
}

/// Sole-peer GD_SLOW depth floor (default **16**). Full keep-at-32 floods a slow
/// archive (tc105+ tip30≈3–20); cliff-to-8 starves. Env: `BLVM_IBD_TIP_HOLE_SOLE_FLOOR`.
pub(crate) fn tip_hole_sole_gd_slow_floor() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_TIP_HOLE_SOLE_FLOOR")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(16)
            .clamp(tip_hole_slow_fill_cap(), tip_hole_grow_cap())
    })
}

/// After sole GD_SLOW floor, require EWMA below this (ms) before deepen above floor.
/// Default = **gd-slow gate** (800): once not slow, allow cold CAP deepen.
/// FAST stays blocked separately by [`tip_hole_sole_no_fast_active`] until gd-fast.
/// Override lower only if you want a stricter floor hold (≤ gd-slow).
pub(crate) fn tip_hole_sole_floor_recover_ms() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_TIP_HOLE_SOLE_FLOOR_RECOVER_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(tip_hole_gd_slow_ms)
            .clamp(50, tip_hole_gd_slow_ms())
    })
}

/// True while sole-floor latch is armed and EWMA has not left the slow band.
/// Clears when EWMA &lt; recover_ms (default = gd-slow). Mid-band (e.g. 359–765)
/// may then grow to cold CAP; [`tip_hole_sole_no_fast_active`] still blocks FAST.
pub(crate) fn tip_hole_sole_floor_blocks_grow() -> bool {
    if !super::tip_stage::sole_floor_latched() {
        return false;
    }
    match super::tip_stage::getdata_body_ewma_ms_min_n(tip_hole_gd_slow_n()) {
        Some((ms, _)) if ms < tip_hole_sole_floor_recover_ms() => {
            super::tip_stage::clear_sole_floor_latch();
            false
        }
        _ => true,
    }
}

/// Consecutive tip-hole checks with EWMA &lt; gd-fast required to clear no-FAST.
/// Default **16** (match gd-fast N). tc152: single blip &lt;150ms re-armed FAST_CAP.
pub(crate) fn tip_hole_sole_no_fast_clear_n() -> u32 {
    latch_env!(u32, {
        std::env::var("BLVM_IBD_TIP_HOLE_SOLE_NO_FAST_CLEAR_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(16)
            .clamp(4, 64)
    })
}

/// Min wall-ms no-FAST stays armed after sole GD_SLOW before streak clear may fire.
/// Default **120_000** (covers a tip90 cell). tc155: 10s hold expired → FAST @+10.2s
/// → dens tip30 then gd_max≈5s cheese. Re-arming each sole floor resets this clock.
pub(crate) fn tip_hole_sole_no_fast_min_hold_ms() -> u64 {
    // Default **120s** covers tip90 cell. tc173 tried 15s → FAST re-armed mid-window
    // (grown=64) and tip90 fell to ≈28.9. Keep FAST blocked after sole floor.
    latch_env!(u64, {
        std::env::var("BLVM_IBD_TIP_HOLE_SOLE_NO_FAST_MIN_HOLD_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(120_000)
            .clamp(1_000, 600_000)
    })
}

/// Min height before sole GD_SLOW arms the no-FAST latch (default **0** = always).
/// Rematch: READY-crawl SOLE_FLOOR @~400.3–401.0k re-armed 30s hold and blocked
/// dens FAST_CAP 64 (KEEP opened FAST @~405.8k). Set e.g. **401000** so cold
/// crawl still floors depth but does not block later dens deepen.
pub(crate) fn tip_hole_sole_no_fast_arm_min_h() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_TIP_HOLE_SOLE_NO_FAST_ARM_MIN_H")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    })
}

fn maybe_note_sole_no_fast_latch(height: u64) {
    let min_h = tip_hole_sole_no_fast_arm_min_h();
    if min_h > 0 && height < min_h {
        return;
    }
    super::tip_stage::note_sole_no_fast_latch();
}

/// Skip GD_SLOW sole-floor (32→16) at/after this height. Default **0** = KEEP
/// (always apply). Cold READY still needs the floor (tc105 / #8 SOLE_FLOOR=32
/// flood). Dens death at 406787 is a poisoned getdata→body EWMA on a still-fed
/// pipe — do not re-clamp there. Env: `BLVM_IBD_TIP_HOLE_SOLE_FLOOR_MAX_H`.
pub(crate) fn tip_hole_sole_floor_max_h() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_TIP_HOLE_SOLE_FLOOR_MAX_H")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    })
}

/// Whether sole-floor / floor-hold may run at `height`. Crossing max_h clears
/// a leftover 400.3k latch so HOLD cannot 32→16 on the dens pipe.
pub(crate) fn tip_hole_sole_floor_applies(height: u64) -> bool {
    let max_h = tip_hole_sole_floor_max_h();
    if max_h == 0 {
        return true;
    }
    if height >= max_h {
        if super::tip_stage::sole_floor_latched() {
            super::tip_stage::clear_sole_floor_latch();
        }
        return false;
    }
    true
}

/// True while sole no-FAST latch is armed and getdata is not *sustainably* gd-fast.
/// Clears only after min wall hold **and** [`tip_hole_sole_no_fast_clear_n`]
/// consecutive healthy checks (signal-only; no height cliff).
pub(crate) fn tip_hole_sole_no_fast_active() -> bool {
    if !super::tip_stage::sole_no_fast_latched() {
        return false;
    }
    let held_long_enough = super::tip_stage::sole_no_fast_armed_age_ms()
        .map(|age| age >= tip_hole_sole_no_fast_min_hold_ms())
        .unwrap_or(false);
    match super::tip_stage::getdata_body_ewma_ms_min_n(tip_hole_gd_fast_n()) {
        Some((ms, _)) if ms < tip_hole_gd_fast_ms() => {
            if !held_long_enough {
                // Still inside hold — do not accumulate clear streak yet.
                let _ = super::tip_stage::sole_no_fast_note_clear_sample(false);
                return true;
            }
            let streak = super::tip_stage::sole_no_fast_note_clear_sample(true);
            if streak >= tip_hole_sole_no_fast_clear_n() {
                super::tip_stage::clear_sole_no_fast_latch();
                false
            } else {
                true
            }
        }
        _ => {
            let _ = super::tip_stage::sole_no_fast_note_clear_sample(false);
            true
        }
    }
}

/// Sole peer: while no-FAST latch is active, clamp tip-hole cap to cold GROW_CAP
/// so GD_SLOW → brief not-slow flicker cannot re-arm FAST_CAP.
pub(crate) fn tip_hole_cap_for_sole(sole_ready: bool, cap: usize) -> usize {
    if sole_ready && tip_hole_sole_no_fast_active() {
        cap.min(tip_hole_grow_cap())
    } else {
        cap
    }
}

/// C1u: true when grow is enabled and getdata→body EWMA is warm+slow.
pub(crate) fn tip_hole_gd_slow() -> bool {
    if !tip_hole_grow_enabled() || !tip_hole_gd_slow_enabled() {
        return false;
    }
    match super::tip_stage::getdata_body_ewma_ms_min_n(tip_hole_gd_slow_n()) {
        Some((ms, _n)) if ms >= tip_hole_gd_slow_ms() => true,
        _ => false,
    }
}

/// C1d: warm peers (hot tip streamer) may deepen to pipe cap (default **off**).
/// iter10k: warm→128 regressed wall≈22 &lt; C1b floor 40 (`tip_hole_grown_p50=128`).
/// Opt in: `BLVM_IBD_TIP_HOLE_WARM=1` (Mode T / serving peer only).
pub(crate) fn tip_hole_warm_enabled() -> bool {
    latch_env!(bool, {
        match std::env::var("BLVM_IBD_TIP_HOLE_WARM")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("1") | Some("true") | Some("on") | Some("yes") => true,
            _ => false,
        }
    })
}

fn tip_hole_warm_cap_raw() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_TIP_HOLE_WARM_CAP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(tip_hole_pipe_cap)
            .clamp(2, 128)
            .min(tip_hole_pipe_cap())
    })
}

pub(crate) fn tip_hole_grow_cap_for_peer(hot_tip_streamer: bool) -> usize {
    // C1n: base is gd-gated effective (cold 32, or 48 when EWMA fast).
    let cold = tip_hole_grow_cap_effective();
    if !tip_hole_grow_enabled() || !tip_hole_warm_enabled() || !hot_tip_streamer {
        return cold;
    }
    tip_hole_warm_cap_raw()
        .clamp(cold, 128)
        .min(tip_hole_pipe_cap())
        .max(cold)
}

/// C1: initial tip-hole depth when grow-on-delivery is on (default **8**).
pub(crate) fn tip_hole_grow_start() -> usize {
    if !tip_hole_grow_enabled() {
        return tip_hole_pipe_cap();
    }
    latch_env!(usize, {
        std::env::var("BLVM_IBD_TIP_HOLE_GROW_START")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8)
            .clamp(2, 32)
            .min(tip_hole_grow_cap())
    })
}

/// C1/C1r: deepen by this many slots per tip-band network body (default **8**).
/// C1r: while gd-fast effective cap is elevated, step at least **16** (bake often
/// sets `GROW_STEP=8` — that must not disable the fast step).
pub(crate) fn tip_hole_grow_step() -> usize {
    let configured = latch_env!(usize, {
        std::env::var("BLVM_IBD_TIP_HOLE_GROW_STEP")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(8)
            .clamp(2, 32)
    });
    if tip_hole_gd_fast_enabled()
        && tip_hole_grow_enabled()
        && tip_hole_grow_cap_effective() > tip_hole_grow_cap()
    {
        return configured.max(16);
    }
    configured
}

/// Absolute sticky ceiling — independent of live EWMA (C1s).
/// Fill still mins against `tip_hole_grow_cap_effective()` each request.
pub(crate) fn tip_hole_sticky_abs_cap(hot_tip_streamer: bool) -> usize {
    let cold = tip_hole_grow_cap();
    let mut cap = if tip_hole_gd_fast_enabled() {
        tip_hole_grow_fast_cap().max(cold)
    } else {
        cold
    };
    if tip_hole_warm_enabled() && hot_tip_streamer {
        let warm = tip_hole_warm_cap_raw()
            .clamp(cap, 128)
            .min(tip_hole_pipe_cap());
        cap = cap.max(warm);
    }
    cap.min(tip_hole_pipe_cap())
}

/// C1/C1n: deepen tip-hole fill after tip-gap network body; returns new depth.
pub(crate) fn tip_hole_grow_on_delivery(current: usize) -> usize {
    tip_hole_grow_on_delivery_capped(current, tip_hole_grow_cap_effective())
}

pub(crate) fn tip_hole_grow_on_delivery_capped(current: usize, cap: usize) -> usize {
    if !tip_hole_grow_enabled() {
        return tip_hole_pipe_cap();
    }
    current.saturating_add(tip_hole_grow_step()).min(cap)
}

/// W10: pick per-block timeout from chunk position vs validation tip.
pub(crate) fn gap_timeout_for_chunk(
    start_height: u64,
    end_height: u64,
    validation_tip: u64,
    default_secs: u64,
) -> u64 {
    let tip_needed = validation_tip.saturating_add(1);
    if start_height <= tip_needed && tip_needed <= end_height {
        // W139: empty deep 8s / empty (H,H) 5s — match CAP.
        tip_gap_timeout_secs_for_chunk(false, start_height, end_height)
    } else if start_height > tip_needed.saturating_add(far_ahead_band()) {
        far_ahead_timeout_secs()
    } else {
        default_secs
    }
}

/// W32d″: tiered per-position timeout on WAN deep tip pipe.
///
/// Tip gap stays short (soft-retry / rotate). Mid/deep must be **≥ tip soft-retry window**
/// so ahead GetData does not expire while tip is soft-retrying.
/// Live W32d′ regression (~13→4.7 blk/s): short 8/12s mid/deep + park emptied the pipe.
/// Defaults: tip=`tip_gap_timeout_secs()`, +1..31 → **30s**, +32.. → **45s**.
pub(crate) fn wan_deep_pipe_timeout_secs(
    height: u64,
    validation_tip: u64,
    confirmed_body_height: u64,
) -> Option<u64> {
    if confirmed_body_height == 0 || height <= confirmed_body_height {
        return None;
    }
    let tip_needed = validation_tip.saturating_add(1);
    if height < tip_needed {
        return None;
    }
    let offset = height.saturating_sub(tip_needed);
    let secs = match offset {
        0 => tip_gap_timeout_secs(),
        1..=31 => latch_env!(u64, {
            std::env::var("BLVM_IBD_PIPE_MID_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30)
                .clamp(15, 60)
        }),
        _ => latch_env!(u64, {
            std::env::var("BLVM_IBD_PIPE_DEEP_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(45)
                .clamp(25, 90)
        }),
    };
    Some(secs)
}

/// Per-height gap timeout: W32d WAN pipe tiers override chunk-level defaults.
pub(crate) fn block_gap_timeout_secs(
    height: u64,
    validation_tip: u64,
    confirmed_body_height: u64,
    chunk_start: u64,
    chunk_end: u64,
    default_secs: u64,
) -> u64 {
    wan_deep_pipe_timeout_secs(height, validation_tip, confirmed_body_height).unwrap_or_else(|| {
        gap_timeout_for_chunk(chunk_start, chunk_end, validation_tip, default_secs)
    })
}

/// W71: true when an in-flight tip-gap GetData has waited ≥ tip-gap timeout.
///
/// Tokio timeouts were historically baked at enqueue (`timeout(secs, rx)`). Mid/deep pipe
/// tiers use 30–45s, so when the validation tip walks into a previously-ahead height the
/// in-flight future kept waiting the long timeout. Live 2026-07-17 tip=322456: soft-retry
/// logged `after 45s (limit 10s)`. W72 uses a shorten-able deadline atomic instead.
pub(crate) fn tip_gap_inflight_exceeded(started: Instant, tip_gap_secs: u64) -> bool {
    started.elapsed().as_secs() >= tip_gap_secs.max(1)
}

/// W85: when validation tip first walks onto `tip_needed`, reset that height's CAP
/// clock so the 12s budget measures **post-roll** wait only.
///
/// Deep tip pipes GetData heights seconds before they become the tip (`need→getdata`
/// often largely negative). Without rebase, CAP=12s expires from pre-roll age and
/// soft-retries / rotates peers that would deliver within true tip SLA.
pub(crate) fn rebase_tip_cap_clock(
    tip_needed: u64,
    tip_cap_clock_h: &mut Option<u64>,
    in_flight_heights: &HashSet<u64>,
    inflight_started: &mut HashMap<u64, Instant>,
) -> bool {
    if Some(tip_needed) == *tip_cap_clock_h {
        return false;
    }
    *tip_cap_clock_h = Some(tip_needed);
    if !in_flight_heights.contains(&tip_needed) {
        return false;
    }
    if let Some(started) = inflight_started.get_mut(&tip_needed) {
        *started = Instant::now();
        return true;
    }
    false
}

fn wall_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Keep `inflight_started` / deadline map aligned with `in_flight_heights`.
fn sync_inflight_started(
    inflight_started: &mut HashMap<u64, Instant>,
    inflight_deadlines: &mut HashMap<u64, Arc<AtomicU64>>,
    in_flight_heights: &HashSet<u64>,
) {
    inflight_started.retain(|h, _| in_flight_heights.contains(h));
    inflight_deadlines.retain(|h, _| in_flight_heights.contains(h));
    for &h in in_flight_heights {
        inflight_started.entry(h).or_insert_with(Instant::now);
    }
}

/// Await a block oneshot until the shared deadline (ms since epoch) elapses.
///
/// W72: tip-cap can store `now` into `deadline_ms` to force a tip-gap timeout without
/// aborting the whole deep pipe (in-place soft-retry keeps ahead `received`).
async fn await_block_with_deadline(
    mut rx: tokio::sync::oneshot::Receiver<(Block, Vec<Vec<Witness>>, Option<Vec<u8>>)>,
    deadline_ms: Arc<AtomicU64>,
) -> Result<
    Result<(Block, Vec<Vec<Witness>>, Option<Vec<u8>>), tokio::sync::oneshot::error::RecvError>,
    tokio::time::error::Elapsed,
> {
    loop {
        let now = wall_ms_now();
        let dl = deadline_ms.load(Ordering::Relaxed);
        if now >= dl {
            return Err(timeout(Duration::ZERO, std::future::pending::<()>())
                .await
                .unwrap_err());
        }
        let slice = Duration::from_millis((dl - now).min(500));
        tokio::select! {
            biased;
            r = &mut rx => return Ok(r),
            _ = tokio::time::sleep(slice) => {}
        }
    }
}

fn push_network_inflight(
    in_flight: &mut FuturesUnordered<PendingBlockFuture>,
    in_flight_heights: &mut HashSet<u64>,
    inflight_deadlines: &mut HashMap<u64, Arc<AtomicU64>>,
    height: u64,
    block_hash: [u8; 32],
    rx: tokio::sync::oneshot::Receiver<(Block, Vec<Vec<Witness>>, Option<Vec<u8>>)>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    timeout_secs: u64,
) {
    let deadline = Arc::new(AtomicU64::new(
        wall_ms_now().saturating_add(timeout_secs.saturating_mul(1000)),
    ));
    inflight_deadlines.insert(height, Arc::clone(&deadline));
    in_flight_heights.insert(height);
    let request_start = Instant::now();
    in_flight.push(Box::pin(async move {
        let r = await_block_with_deadline(rx, deadline).await;
        (height, block_hash, request_start, r, permit)
    }));
}

/// W28d: poll until tip walks into this chunk's range and it is not a tip-cover claim.
async fn wait_tip_enter_abort(
    tip_enter: &Option<Arc<super::chunk_assigner::ChunkAssigner>>,
    peer_id: &str,
    start_height: u64,
    end_height: u64,
) {
    let Some(assigner) = tip_enter.as_ref() else {
        std::future::pending::<()>().await;
        return;
    };
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if assigner.should_abort_tip_walk_in(peer_id, start_height, end_height) {
            return;
        }
    }
}

/// W36: abort when coordinator blacklists this peer (tip-SLA rotate) so in-flight
/// does not remain a covering zombie for minutes.
async fn wait_blacklist_abort(
    tip_enter: &Option<Arc<super::chunk_assigner::ChunkAssigner>>,
    peer_id: &str,
) {
    let Some(assigner) = tip_enter.as_ref() else {
        std::future::pending::<()>().await;
        return;
    };
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if assigner.is_peer_blacklisted(peer_id) {
            return;
        }
    }
}

/// W33/A6g: WAN deep tip-owner pipe uses a bounded gap deadline — ahead blocks must not
/// extend to 600s. Floor tracks tip-SLA (default 90s) so soft-retries are not aborted by
/// the outer chunk deadline before SLA can rotate.
pub(crate) fn wan_deep_tip_pipe_chunk_deadline_secs(
    start_height: u64,
    end_height: u64,
    confirmed_body_height: u64,
    default_secs: u64,
) -> u64 {
    if confirmed_body_height > 0
        && start_height > confirmed_body_height
        && end_height.saturating_sub(start_height) >= 63
    {
        super::tip_stage::tip_sla_secs()
            .saturating_mul(2)
            .clamp(90, 180)
    } else {
        default_secs
    }
}

/// WAN tip-stream score credits for one received body (assigner present).
///
/// Tip-adjacent arrival credits once (covers GAP_STREAM no-op). Successful GAP_STREAM
/// credits only when the body was not tip-adjacent — never both (avoids ~2× sticky tip BPS).
pub(crate) fn wan_tip_stream_credit_count(
    from_local: bool,
    tip_adjacent: bool,
    gap_streamed: bool,
) -> u8 {
    if from_local {
        return 0;
    }
    let mut n = 0u8;
    if tip_adjacent {
        n = n.saturating_add(1);
    }
    if gap_streamed && !tip_adjacent {
        n = n.saturating_add(1);
    }
    n
}

/// Download a chunk of blocks from a peer.
///
/// When block_tx is Some, streams each block immediately so validation doesn't wait for full chunk.
/// blocks_sem: Core-style limit — max 16 blocks in flight per peer across all workers.
/// stall_rx: When coordinator stalls, it broadcasts the needed height; worker aborts if our chunk contains it.
///
/// `confirmed_body_height` — uncapped body-confirmed height at IBD start; used to avoid stall-abort on resume gaps.
///
/// `outer_deadline` — cooperative wall-clock budget (S1). When set, the select loop races
/// `sleep_until(deadline)` and flushes `received` via [`flush_received_on_abort`] before
/// returning Err. Prefer this over wrapping the future in `tokio::timeout`, which drops
/// the task and loses buffered blocks (live: outer deadline at 546964/549065 with no flush).
pub(crate) async fn download_chunk(
    start_height: u64,
    end_height: u64,
    peer_id: &str,
    network: Option<Arc<NetworkManager>>,
    blockstore: &BlockStore,
    config: &ParallelIBDConfig,
    peer_scorer: Arc<crate::network::peer_scoring::PeerScorer>,
    block_tx: Option<tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>>,
    blocks_sem: Option<Arc<Semaphore>>,
    mut stall_rx: Option<&mut broadcast::Receiver<u64>>,
    protocol_version: ProtocolVersion,
    validation_height: Option<Arc<AtomicU64>>,
    confirmed_body_height: u64,
    wan_multi_peer: bool,
    outer_deadline: Option<tokio::time::Instant>,
    // W28d: when set, abort ahead partitions that tip has walked into.
    tip_enter: Option<Arc<super::chunk_assigner::ChunkAssigner>>,
) -> Result<DownloadChunkResult> {
    let streaming = block_tx.is_some();
    // Process-latched registry — avoid per-chunk Vec+String rebuild of feature table.
    let feature_registry = cached_feature_registry(protocol_version);
    let mut blocks = Vec::new();
    let mut streamed_block_count: usize = 0;
    let mut progress = BlockDownloadProgress::new();
    // Used to detect genuinely stuck partial chunks: abort if stall signal arrives
    // and we have been active for >PARTIAL_STALL_ABORT_SECS without delivering the needed block.
    let chunk_start_time = std::time::Instant::now();
    let mut outer_deadline = outer_deadline;
    let mut outer_extends: u32 = 0;
    let mut gap_streams: u64 = 0;

    // Drain stale stall broadcasts accumulated while this worker was finishing its previous chunk.
    // Workers hold stall_rx across the entire worker-task lifetime (one subscription, many chunks).
    // A broadcast sent during the previous chunk's work sits unread in the channel. Without
    // draining, the very first select! poll in the "no first block yet" branch fires the stale
    // signal immediately — "no first block yet" → abort → re-queue → same broadcast fires again.
    // Draining here gives this chunk a clean slate; only broadcasts sent AFTER we start are relevant.
    if let Some(ref mut rx) = stall_rx {
        loop {
            match rx.try_recv() {
                Ok(_) => continue,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break, // Empty or Closed
            }
        }
    }

    info!(
        "Downloading chunk from peer {}: heights {} to {}",
        peer_id, start_height, end_height
    );

    let local_disk = is_snapshot_sourced_peer(peer_id);
    let network = match network {
        Some(n) => n,
        None => {
            warn!("NetworkManager not available, skipping block download");
            return Ok(DownloadChunkResult {
                blocks,
                streamed_block_count: 0,
            });
        }
    };

    // Dummy addr for local-disk (never used for send); real peers must parse.
    let peer_addr = if local_disk {
        SocketAddr::from(([127, 0, 0, 1], 9))
    } else {
        peer_id
            .parse::<SocketAddr>()
            .map_err(|_| anyhow::anyhow!("Invalid peer address: {}", peer_id))?
    };

    if !local_disk {
        let connect_wait = Duration::from_secs(config.download_timeout_secs.max(15));
        wait_for_peer_connected(&network, peer_addr, peer_id, connect_wait, &tip_enter).await?;
        let handshake_wait = Duration::from_secs(15);
        wait_for_peer_ibd_ready(&network, peer_addr, peer_id, handshake_wait, &tip_enter)
            .await?;
    }

    let validated_tip = validation_height
        .as_ref()
        .map(|h| h.load(Ordering::Relaxed))
        .unwrap_or(start_height.saturating_sub(1));
    let resume_from = match resume_download_height(start_height, end_height, validated_tip) {
        Some(h) => h,
        None => {
            return Ok(DownloadChunkResult {
                blocks,
                streamed_block_count: 0,
            });
        }
    };
    if resume_from > start_height {
        info!(
            "[IBD] {} chunk {}-{}: resuming download at height {} (validated tip {})",
            peer_id, start_height, end_height, resume_from, validated_tip
        );
    }

    let mut block_hashes = Vec::new();
    let mut effective_end = end_height;
    for height in start_height..=end_height {
        if let Ok(Some(hash)) = blockstore.get_hash_by_height(height) {
            block_hashes.push((height, hash));
        } else {
            // W35‴-h: clip to contiguous headers instead of failing the whole pipe
            // (live: tip..tip+255 assigned past header tip → 912× instant fails → mass blacklist).
            if height == start_height {
                warn!(
                    "Block hash not found for height {} - header may not be stored yet",
                    height
                );
                return Err(anyhow::anyhow!(
                    "Block hash not found for height {} - headers must be downloaded first",
                    height
                ));
            }
            effective_end = height.saturating_sub(1);
            warn!(
                "[IBD_HEADER_CLIP] chunk {}-{} clipped to {}-{} — missing header at {}",
                start_height, end_height, start_height, effective_end, height
            );
            break;
        }
    }
    let end_height = effective_end;

    if block_hashes.is_empty() {
        return Err(anyhow::anyhow!(
            "No block hashes found for heights {} to {}",
            start_height,
            end_height
        ));
    }

    let base_timeout_secs = config.download_timeout_secs;
    // For single-height (H,H) gap micro-chunks, use a dedicated per-block timeout.
    // W4/W6: default 45s. Do NOT clamp to download_timeout_secs (default 30) — that made the
    // 45s default a no-op (live: bulk+micro still aborted at 30s). Env override still applies.
    // W10: tip-covering / far-ahead chunks override below (12s / 10s) so soft-retry storms
    // cannot pin peers for 3×45s while the validation tip starves.
    let gap_micro_timeout_secs: u64 = std::env::var("BLVM_IBD_GAP_MICRO_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(45)
        .clamp(15, 180);
    let tip_aware_secs = gap_timeout_for_chunk(
        start_height,
        end_height,
        validated_tip,
        gap_micro_timeout_secs,
    );
    // W2: body on disk but witness missing/empty — only need MSG_WITNESS_BLOCK. Give the peer
    // longer than the default micro-timeout (live: 71% of gap timeouts ∩ witness-miss heights;
    // 25s aborts discarded late witness payloads). Default 75s; env BLVM_IBD_WITNESS_HOLE_TIMEOUT_SECS.
    let witness_hole = start_height == end_height
        && block_hashes
            .iter()
            .find(|(h, _)| *h == start_height)
            .map(|(_, hash)| {
                is_local_witness_hole(blockstore, start_height, *hash, protocol_version)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
    let (mut timeout_duration, first_block_wait) = if witness_hole {
        let hole_secs = std::env::var("BLVM_IBD_WITNESS_HOLE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(75)
            .clamp(30, 180);
        if hole_secs > tip_aware_secs {
            info!(
                "[IBD_WITNESS_HOLE] height {}: extending micro-timeout {}s → {}s (body on disk, witness missing)",
                start_height, tip_aware_secs, hole_secs
            );
        }
        let d = Duration::from_secs(hole_secs);
        (d, d)
    } else if start_height == end_height {
        let d = Duration::from_secs(tip_aware_secs);
        (d, d)
    } else {
        // W6: multi-block gap/bulk downloads used download_timeout_secs=30 by default.
        // Live WAN: 16-block stall-recovery chunks aborted at 30s on the gap height, flushed
        // 1–8 blocks, then re-queued — timeout storms at ~0.2 BPS. Use ≥ gap micro timeout.
        // W10: tip/far-ahead overrides still apply via tip_aware_secs.
        let secs = if tip_aware_secs < gap_micro_timeout_secs {
            tip_aware_secs
        } else {
            base_timeout_secs.max(gap_micro_timeout_secs)
        };
        let d = Duration::from_secs(secs);
        (d, d)
    };
    if tip_aware_secs != gap_micro_timeout_secs {
        // W175: rate-limit — live W174 logged TIP_GAP 267×/s during mute-reopen spin.
        static LAST_TIP_GAP_LOG_MS: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_TIP_GAP_LOG_MS.load(std::sync::atomic::Ordering::Relaxed);
        if now_ms.saturating_sub(prev) >= 2_000
            && LAST_TIP_GAP_LOG_MS
                .compare_exchange(
                    prev,
                    now_ms,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
        {
            info!(
                "[IBD_TIP_GAP] chunk {}-{} tip={} — per-block timeout {}s (default micro {}s)",
                start_height, end_height, validated_tip, tip_aware_secs, gap_micro_timeout_secs
            );
        }
    }

    let pipeline_depth: usize = blocks_sem
        .as_ref()
        .map(|_| config.max_blocks_in_transit_per_peer)
        .unwrap_or(config.max_concurrent_per_peer);

    // W32f: observability for deep WAN tip-owner pipe.
    if confirmed_body_height > 0
        && start_height > confirmed_body_height
        && end_height.saturating_sub(start_height) >= 63
    {
        warn!(
            "[IBD_TIP_PIPE] chunk {}-{} peer={} pipe_depth={} span={}",
            start_height,
            end_height,
            peer_id,
            pipeline_depth,
            end_height.saturating_sub(start_height).saturating_add(1)
        );
    }

    let chunk_default_secs = timeout_duration.as_secs();

    let mut in_flight: FuturesUnordered<PendingBlockFuture> = FuturesUnordered::new();
    let block_hash_by_height: BTreeMap<u64, [u8; 32]> = block_hashes.into_iter().collect();
    let mut in_flight_heights: HashSet<u64> = HashSet::new();
    // W71/W72: first-seen Instant + shorten-able deadline per in-flight network GetData.
    let mut inflight_started: HashMap<u64, Instant> = HashMap::new();
    let mut inflight_deadlines: HashMap<u64, Arc<AtomicU64>> = HashMap::new();
    // W85: last tip height whose CAP clock was rebased to tip-roll (not pre-roll GetData).
    let mut tip_cap_clock_h: Option<u64> = None;
    // Arc-wrap immediately so downstream pipeline stages never deep-copy block bytes.
    let mut received: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let mut next_to_send = resume_from;
    // P4: soft gap timeouts re-request without aborting; hard deadline still aborts.
    // W10/W12: per-timeout budget from gap_soft_retry_budget (tip/far-ahead aware).
    let mut gap_soft_retries: u32 = 0;
    // W29c: after tip GAP_STREAM succeeds, lengthen per-block timeout for remaining pipe.
    let mut progressive_timeout_applied = false;
    // P1a/P1b: heights loaded via try_load_local (wire_payload=None). Do not credit WAN
    // tip-stream score or arm progressive 45s shelter for these (G-W forensics).
    let mut local_sourced_heights: HashSet<u64> = HashSet::new();
    // Heights that arrived via network GetData (for drain-path tip-stream credit).
    let mut network_body_heights: HashSet<u64> = HashSet::new();
    // B11v5: heights whose GAP_PERSIST has completed (or local / sync path).
    let mut disk_ready: HashSet<u64> = HashSet::new();
    let gap_persist_offload = gap_persist_offload_enabled();
    let persist_sem = Arc::new(Semaphore::new(gap_persist_offload_concurrency()));
    let tip_persist_sem = Arc::new(Semaphore::new(gap_persist_tip_lane_concurrency()));
    let mut persist_inflight: FuturesUnordered<GapPersistAckFuture> = FuturesUnordered::new();
    // Heights with an OFFLOAD future already spawned (DEFER_FAR kick must not double-spawn).
    let mut persist_spawned: HashSet<u64> = HashSet::new();
    let mut network_tip_streamed = false;
    // P1d: clock for PIPE_FILL mute — armed when network GetData is outstanding with no
    // network body yet; reset on any network-sourced body.
    let mut pipe_mute_armed_at: Option<Instant> = None;
    let mut saw_network_body = false;
    // Set after initial fill when GetData pipe is full and `received` still empty.
    let mut pipe_fill_recv0 = false;
    // C1c: tip-hole depth — sticky across chunks for same peer; grow only on tip-gap bodies.
    // C1d: hot tip streamers may warm up to pipe cap (128); cold stays at grow_cap (32).
    // C1n: recompute cap each fill/grow — deepen to FAST_CAP only while getdata EWMA is fast.
    let tip_hole_hot = tip_enter
        .as_ref()
        .map(|a| a.peer_is_hot_tip_streamer(peer_id))
        .unwrap_or(false);
    let mut tip_hole_cap = tip_hole_grow_cap_for_peer(tip_hole_hot);
    let mut tip_hole_grown: usize = tip_enter
        .as_ref()
        .map(|a| a.tip_hole_depth_for(peer_id))
        .unwrap_or_else(tip_hole_grow_start)
        .min(tip_hole_cap);
    // C1u: enter with shallow fill if GetData EWMA already slow (sticky may still be 32).
    // Sole ready peer: pin to sole_gd_slow_floor (16) — raise starve-8, lower flood-32.
    // Do not hard-cap FAST_CAP 64 (tc109 tip90≈67.8 used FAST_CAP=64; sole abs-cap regressed).
    let sole_ready = tip_enter
        .as_ref()
        .map(|a| a.ibd_ready_peer_count() <= 1)
        .unwrap_or(false);
    // Sole no-FAST: after GD_SLOW, keep tip-hole at cold grow_cap until gd is fast.
    tip_hole_cap = tip_hole_cap_for_sole(sole_ready, tip_hole_cap);
    if tip_hole_grown > tip_hole_cap {
        let prev = tip_hole_grown;
        tip_hole_grown = tip_hole_cap;
        info!(
            "[IBD_TIP_HOLE_CAP_CLAMP] peer={} height={} depth {}→{} (cap={})",
            peer_id, next_to_send, prev, tip_hole_grown, tip_hole_cap
        );
        if let Some(ref a) = tip_enter {
            a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
        }
    }
    if tip_hole_gd_slow() && sole_ready && tip_hole_sole_floor_applies(next_to_send) {
        let floor = tip_hole_sole_gd_slow_floor();
        super::tip_stage::note_sole_floor_latch();
        maybe_note_sole_no_fast_latch(next_to_send);
        if tip_hole_grown != floor {
            let prev = tip_hole_grown;
            tip_hole_grown = floor;
            info!(
                "[IBD_TIP_HOLE_GD_SLOW_SOLE_FLOOR] peer={} depth {}→{} — sole ready; avoid 32-flood/8-starve",
                peer_id, prev, tip_hole_grown
            );
            if let Some(ref a) = tip_enter {
                a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
            }
        }
    } else if sole_ready && tip_hole_sole_floor_applies(next_to_send) && tip_hole_sole_floor_blocks_grow() {
        let floor = tip_hole_sole_gd_slow_floor();
        if tip_hole_grown > floor {
            tip_hole_grown = floor;
            if let Some(ref a) = tip_enter {
                a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
            }
        }
    } else if tip_hole_gd_slow() && !sole_ready {
        let slow = tip_hole_slow_fill_cap();
        if tip_hole_grown > slow {
            tip_hole_grown = slow;
            if let Some(ref a) = tip_enter {
                a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
            }
        }
    }
    super::tip_stage::note_tip_hole_duty(tip_hole_grown);
    // W32d″: after tip streams, remaining pipe uses a long timeout (not 12s) so mid/deep
    // slots do not thrash while validation walks the buffered runway.
    let tip_progressive_secs: u64 = std::env::var("BLVM_IBD_TIP_PROGRESSIVE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(45)
        .clamp(25, 90);
    // A6: empty-witness hits on this peer/chunk before demotion / abort.
    // Live soak: peers that only serve MSG_BLOCK re-request forever (~38 hits/s) and pin
    // validation at the gap. Cap retries then abort so the worker excludes/blacklists the peer.
    let mut empty_witness_hits: u32 = 0;
    let max_empty_witness_hits: u32 = empty_witness_hit_cap();

    let mut first_block_logged = false;
    let initial_outer_secs = outer_deadline
        .map(|d| {
            d.saturating_duration_since(tokio::time::Instant::now())
                .as_secs()
                .max(1)
        })
        .unwrap_or(0);
    let mut outer_deadline_secs = initial_outer_secs;

    async fn fill_pipeline(
        next_to_send: u64,
        end_height: u64,
        pipeline_depth: usize,
        received: &BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
        in_flight: &mut FuturesUnordered<PendingBlockFuture>,
        in_flight_heights: &mut HashSet<u64>,
        inflight_deadlines: &mut HashMap<u64, Arc<AtomicU64>>,
        block_hash_by_height: &BTreeMap<u64, [u8; 32]>,
        network: &Arc<NetworkManager>,
        peer_addr: SocketAddr,
        peer_id: &str,
        blockstore: &BlockStore,
        protocol_version: ProtocolVersion,
        validation_tip: u64,
        confirmed_body_height: u64,
        chunk_default_secs: u64,
        blocks_sem: &Option<Arc<Semaphore>>,
        first_block_logged: &mut bool,
        start_height: u64,
        local_sourced_heights: &mut HashSet<u64>,
        tip_hole_grown: usize,
        tip_hole_cap: usize,
    ) -> Result<()> {
        // Batch network block requests into groups of GETDATA_BATCH_SIZE. Local cache hits
        // (blocks already in blockstore) are enqueued immediately without batching.
        let mut net_batch_heights: Vec<(u64, [u8; 32])> = Vec::new();
        let mut net_batch_permits: Vec<Option<tokio::sync::OwnedSemaphorePermit>> = Vec::new();
        let mut scan = next_to_send;
        // W102/W102b: while sequential cursor is still the validation tip and tip is not
        // buffered, cap GetData depth. STREAM + [`sync_next_to_send_after_gap_stream`]
        // disarms the cap.
        // C1/C1d: tip-hole depth is `tip_hole_grown` capped by cold/warm `tip_hole_cap`.
        let tip_needed = validation_tip.saturating_add(1);
        let tip_hole = next_to_send == tip_needed
            && tip_needed <= end_height
            && !received.contains_key(&tip_needed);
        let mut fill_depth = if tip_hole {
            tip_hole_grown.min(pipeline_depth).min(tip_hole_cap)
        } else {
            pipeline_depth
        };
        // C1u: do not hold depth=32 GetDatas while gd_ewma is drip-slow.
        // Keep fill clamp even when ratchet is opt-in (T172520Z REVERT: dropping
        // this over-piped mute stretches and tip60-failed @480k).
        if tip_hole && tip_hole_gd_slow() {
            fill_depth = fill_depth.min(tip_hole_slow_fill_cap());
        }
        // Mode T tip-priority: tip-cover chunks stay glued to tip_needed (never
        // mid-span fill→256 on the tip TCP stream). Ahead chunks on a second
        // loopback peer must NOT be clamped (tc165 sole-only starved tip90).
        let sole_tip_pri = super::sole_tip_priority_enabled();
        let near_tip = next_to_send <= tip_needed.saturating_add(tip_hole_cap as u64);
        let tip_glue = sole_tip_pri && (tip_hole || near_tip);
        if tip_glue {
            let mut sole_fill = tip_hole_grown.min(pipeline_depth).min(tip_hole_cap).max(1);
            if tip_hole_gd_slow() {
                sole_fill = sole_fill.min(tip_hole_slow_fill_cap());
            }
            fill_depth = sole_fill;
            if tip_needed <= end_height {
                scan = tip_needed;
            }
        }
        // M3 C2 keep-pipe: tip-hole underfilled vs grown / pipe_F — refill under same
        // sticky cover (no assign re-roll, no secondaries). Forensic only + fill below.
        if tip_hole && fill_depth > 0 && in_flight.len() < fill_depth {
            let pipe_f = super::tip_stage::pipe_frontier(tip_needed);
            let target = tip_needed.saturating_add((fill_depth as u64).saturating_sub(1));
            if pipe_f < target {
                static C2_LAST: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let prev = C2_LAST.load(Ordering::Relaxed);
                if now.saturating_sub(prev) >= 2
                    && C2_LAST
                        .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    warn!(
                        "[IBD_C2_PREFLIGHT] peer={} tip={} grown={} fill_depth={} in_flight={} pipe_f={} target={} — keep-pipe refill",
                        peer_id,
                        tip_needed,
                        tip_hole_grown,
                        fill_depth,
                        in_flight.len(),
                        pipe_f,
                        target
                    );
                }
            }
        }
        // W2/N11: adapt fill depth to byte budget × recent block size (opt-in).
        let byte_budget = download_byte_budget();
        if let Some(budget) = byte_budget {
            if !tip_glue {
                let est = download_est_block_bytes();
                let by_bytes = (budget / est).max(2) as usize;
                fill_depth = by_bytes.clamp(2, pipeline_depth);
            }
        }
        let sole_scan_end = if tip_glue {
            tip_needed
                .saturating_add((fill_depth as u64).saturating_sub(1))
                .min(end_height)
        } else {
            end_height
        };

        while in_flight.len() + net_batch_heights.len() < fill_depth {
            if let Some(budget) = byte_budget {
                let pending = (in_flight.len() + net_batch_heights.len()) as u64;
                let pending_bytes = pending.saturating_mul(download_est_block_bytes());
                if pending_bytes >= budget {
                    break;
                }
            }
            let Some(height) = (scan..=sole_scan_end)
                .find(|h| !received.contains_key(h) && !in_flight_heights.contains(h))
            else {
                break;
            };
            scan = height + 1;

            let block_hash = *block_hash_by_height
                .get(&height)
                .ok_or_else(|| anyhow::anyhow!("Block hash missing for height {height}"))?;

            // D1: never block on the peer semaphore while in_flight holds permits unpolled.
            let Some(permit) = try_take_blocks_permit(blocks_sem)? else {
                break;
            };

            if super::synthetic_wan::is_synthetic_peer(peer_id) {
                let delay_ms = super::synthetic_wan::getdata_delay_ms();
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }

            if let Some((block, block_witnesses)) =
                try_load_local_ibd_block(blockstore, height, block_hash, protocol_version)?
            {
                // Local block: flush any pending network batch first (preserve order of
                // in_flight_heights insertions — local and network blocks are interleaved).
                if !net_batch_heights.is_empty() {
                    enqueue_network_block_batch(
                        std::mem::take(&mut net_batch_heights),
                        std::mem::take(&mut net_batch_permits),
                        network,
                        peer_addr,
                        peer_id,
                        validation_tip,
                        confirmed_body_height,
                        chunk_default_secs,
                        in_flight,
                        in_flight_heights,
                        inflight_deadlines,
                        first_block_logged,
                        start_height,
                        end_height,
                    )
                    .await?;
                }
                if !*first_block_logged {
                    info!(
                        "[IBD] {} chunk {}-{}: local block height {} (hash {})",
                        peer_id,
                        start_height,
                        end_height,
                        height,
                        hex::encode(block_hash)
                    );
                    *first_block_logged = true;
                }
                let request_start = std::time::Instant::now();
                in_flight_heights.insert(height);
                local_sourced_heights.insert(height);
                in_flight.push(Box::pin(async move {
                    let r = Ok(Ok((block, block_witnesses, None)));
                    (height, block_hash, request_start, r, permit)
                }));
            } else if is_snapshot_sourced_peer(peer_id) {
                // Zero-peer local replay: stop at first missing body — never GetData.
                drop(permit);
                break;
            } else {
                net_batch_heights.push((height, block_hash));
                net_batch_permits.push(permit);

                // Flush batch when it reaches GETDATA_BATCH_SIZE.
                if net_batch_heights.len() >= getdata_batch_size() {
                    enqueue_network_block_batch(
                        std::mem::take(&mut net_batch_heights),
                        std::mem::take(&mut net_batch_permits),
                        network,
                        peer_addr,
                        peer_id,
                        validation_tip,
                        confirmed_body_height,
                        chunk_default_secs,
                        in_flight,
                        in_flight_heights,
                        inflight_deadlines,
                        first_block_logged,
                        start_height,
                        end_height,
                    )
                    .await?;
                }
            }
        }

        // Flush any remaining network blocks (fewer than GETDATA_BATCH_SIZE).
        if !net_batch_heights.is_empty() {
            enqueue_network_block_batch(
                net_batch_heights,
                net_batch_permits,
                network,
                peer_addr,
                peer_id,
                validation_tip,
                confirmed_body_height,
                chunk_default_secs,
                in_flight,
                in_flight_heights,
                inflight_deadlines,
                first_block_logged,
                start_height,
                end_height,
            )
            .await?;
        }
        Ok(())
    }

    tip_hole_cap = tip_hole_cap_for_sole(sole_ready, tip_hole_grow_cap_for_peer(tip_hole_hot));
    fill_pipeline(
        next_to_send,
        end_height,
        pipeline_depth,
        &received,
        &mut in_flight,
        &mut in_flight_heights,
        &mut inflight_deadlines,
        &block_hash_by_height,
        &network,
        peer_addr,
        peer_id,
        blockstore,
        protocol_version,
        validated_tip,
        confirmed_body_height,
        chunk_default_secs,
        &blocks_sem,
        &mut first_block_logged,
        start_height,
        &mut local_sourced_heights,
        tip_hole_grown,
        tip_hole_cap,
    )
    .await?;
    // W35″ observability: how full is the GetData pipe after initial fill?
    if confirmed_body_height > 0
        && start_height > confirmed_body_height
        && end_height.saturating_sub(start_height) >= 63
    {
        if received.is_empty() && !in_flight.is_empty() {
            pipe_fill_recv0 = true;
            super::tip_stage::note_pipe_fill_recv0();
        }
        warn!(
            "[IBD_PIPE_FILL] peer={} chunk={}-{} in_flight={}/{} received={} tip_hole_grown={}",
            peer_id,
            start_height,
            end_height,
            in_flight.len(),
            pipeline_depth,
            received.len(),
            tip_hole_grown
        );
    }

    // Hard deadline: abort only when the gap height (next_to_send) makes no progress.
    // Out-of-order receives ahead of the gap must not extend the deadline indefinitely.
    // W8: multi-block chunks used the same 45s as micros (W6), aborting after receiving
    // 13/14 blocks (live h=646643). Give multi-block ≥90s (or 3× per-block) so a slow gap
    // peer can finish while ahead blocks stay buffered.
    let chunk_deadline_secs: u64 = wan_deep_tip_pipe_chunk_deadline_secs(
        start_height,
        end_height,
        confirmed_body_height,
        if start_height == end_height {
            timeout_duration.as_secs()
        } else {
            let blocks = end_height.saturating_sub(start_height).saturating_add(1);
            timeout_duration
                .as_secs()
                .saturating_mul(3)
                .max(90)
                .min(timeout_duration.as_secs().saturating_mul(blocks.max(1)))
                .clamp(90, 600)
        },
    );
    let mut last_gap_at = chunk_start_time;
    let mut deadline_poll = tokio::time::interval(Duration::from_secs(1));
    deadline_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the immediate first tick so the first poll is ~1s out.
    deadline_poll.tick().await;

    loop {
        // W32d: tiered per-position timeout tracks the live gap cursor.
        let tip_now = validation_height
            .as_ref()
            .map(|h| h.load(Ordering::Relaxed))
            .unwrap_or(validated_tip);
        // DEFER_FAR: tip advanced → spawn previously held far-ahead GAP_PERSIST work.
        if gap_persist_offload {
            kick_deferred_gap_persists(
                &received,
                &disk_ready,
                &mut persist_spawned,
                &mut persist_inflight,
                &persist_sem,
                &tip_persist_sem,
                blockstore,
                &validation_height,
                tip_now,
                next_to_send,
                protocol_version,
            );
        }
        // W29c/P1b: once network tip STREAM armed progressive, keep it for the chunk
        // (do not overwrite with per-height tier each poll — that made the assign dead).
        if progressive_timeout_applied {
            timeout_duration = Duration::from_secs(tip_progressive_secs);
        } else {
            timeout_duration = Duration::from_secs(block_gap_timeout_secs(
                next_to_send,
                tip_now,
                confirmed_body_height,
                start_height,
                end_height,
                chunk_default_secs,
            ));
        }
        sync_inflight_started(
            &mut inflight_started,
            &mut inflight_deadlines,
            &in_flight_heights,
        );
        // W71/W72: tip walked into a height still waiting on a mid/deep timeout.
        // W72: shorten the in-flight deadline so the existing soft-retry path re-GetData's
        // the tip in place (keep ahead `received`) instead of aborting the whole deep pipe.
        // Live post-W71: CAP abort recover p50≈4s × ~5/min ≈ ⅓ wall — tip60 fell to ~30.
        //
        // W85: rebase CAP clock on tip-roll — deep-pipe GetData often precedes tip by 3–11s
        // (`need→getdata` p50≈-8s). Measuring from pre-roll GetData made CAP=12s fire after
        // ~4s of true tip wait (live W84: `need→body` p50≈14ms but CAP soft-retry thrash →
        // tip60 ~20).
        let tip_needed = tip_now.saturating_add(1);
        rebase_tip_cap_clock(
            tip_needed,
            &mut tip_cap_clock_h,
            &in_flight_heights,
            &mut inflight_started,
        );
        let ahead_buffered = tip_pipe_has_ahead_buffered(&received, tip_needed);
        // W109/W139/W184: CAP — empty deep 16s; empty (H,H) mute 5s; holey pending 16s.
        let tip_cap_secs =
            tip_gap_timeout_secs_for_chunk(ahead_buffered, start_height, end_height);
        if next_to_send == tip_needed
            && in_flight_heights.contains(&next_to_send)
            && inflight_started
                .get(&next_to_send)
                .is_some_and(|started| tip_gap_inflight_exceeded(*started, tip_cap_secs))
        {
            let waited = inflight_started
                .get(&next_to_send)
                .map(|s| s.elapsed().as_secs())
                .unwrap_or(0);
            // W108/W109: if ahead already buffered, allow one soft tip re-GetData (keep pipe).
            // W114: hot tip-STREAM deep pipe gets a longer soft budget (empty/holey).
            let hot_tip_streamer = tip_enter
                .as_ref()
                .is_some_and(|a| a.peer_is_hot_tip_streamer(peer_id));
            let max_soft = gap_soft_retry_budget_for_chunk_ex(
                next_to_send,
                tip_now,
                start_height,
                end_height,
                ahead_buffered,
                hot_tip_streamer,
            );
            if gap_soft_retries < max_soft {
                if let Some(dl) = inflight_deadlines.get(&next_to_send) {
                    let now = wall_ms_now();
                    // Only shorten once (deadline still in the future).
                    let prev = dl.load(Ordering::Relaxed);
                    if prev > now
                        && dl
                            .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                    {
                        // W139: count CAP softs (was log-only → progressive + CAP shared
                        // budget under-counted CAP path).
                        gap_soft_retries = gap_soft_retries.saturating_add(1);
                        warn!(
                            "[IBD_TIP_TIMEOUT_CAP] gap {} in-flight {}s ≥ tip cap {}s (chunk {}-{}, peer={}, ahead_buf={}) — shortening deadline for soft-retry ({}/{})",
                            next_to_send,
                            waited,
                            tip_cap_secs,
                            start_height,
                            end_height,
                            peer_id,
                            ahead_buffered,
                            gap_soft_retries,
                            max_soft
                        );
                    } else if prev <= now && waited >= tip_cap_secs {
                        // W170: deadline already past but soft not counted (select lag /
                        // race). Still arm soft so covering=3 zombies cannot sit mute.
                        gap_soft_retries = gap_soft_retries.saturating_add(1);
                        warn!(
                            "[IBD_TIP_TIMEOUT_CAP] gap {} in-flight {}s ≥ tip cap {}s (chunk {}-{}, peer={}, ahead_buf={}) — deadline already expired, counting soft-retry ({}/{})",
                            next_to_send,
                            waited,
                            tip_cap_secs,
                            start_height,
                            end_height,
                            peer_id,
                            ahead_buffered,
                            gap_soft_retries,
                            max_soft
                        );
                    }
                }
            } else {
                warn!(
                    "[IBD_TIP_TIMEOUT_CAP] gap {} in-flight {}s ≥ tip cap {}s (chunk {}-{}, peer={}, ahead_buf={}) — soft exhausted, aborting for tip rotate",
                    next_to_send,
                    waited,
                    tip_cap_secs,
                    start_height,
                    end_height,
                    peer_id,
                    ahead_buffered
                );
                peer_scorer.record_failure(peer_addr);
                for &h in &in_flight_heights {
                    if let Some(&h_hash) = block_hash_by_height.get(&h) {
                        network.cancel_block_request(peer_addr, h_hash);
                    }
                }
                flush_received_on_abort(
                    &mut received,
                    block_tx.as_ref(),
                    start_height,
                    end_height,
                    next_to_send,
                    validation_height.as_deref(),
                )
                .await;
                return Err(anyhow::anyhow!(
                    "tip-gap timeout cap: gap {} waited {}s in chunk {}-{}",
                    next_to_send,
                    waited,
                    start_height,
                    end_height
                ));
            }
        }

        // P1d: PIPE_FILL mute eject — tip-band network body starved for MUTE_PIPE_MS after
        // STREAM and/or PIPE_FILL recv=0. Ahead net bodies must not disarm.
        //
        // 2026-07-28 dens-hash160 WAN: every PIPE_FILL recv=0 sat under GAP_PERSIST local
        // tip advances; `tip_buffered` / `tip_is_local_inflight` cleared the clock each
        // height so mute never reached 3s (`pipe_fill_mute=0` / `pipe_fill_recv0=522`).
        // Keep the clock across local tip crawl; only clear on tip-band *network* body
        // (or when the mute episode ends). Defer *fire* while tip is already arriving
        // so we do not cancel an in-flight local load — once local runway ends and tip
        // still needs network, an already-elapsed clock ejects immediately.
        {
            let tip_buffered = received.contains_key(&next_to_send);
            let tip_is_local_inflight = local_sourced_heights.contains(&next_to_send);
            let next_in_chunk =
                next_to_send >= start_height && next_to_send <= end_height;
            let episode_active = pipe_mute_episode_active(
                gap_streams,
                pipe_fill_recv0,
                saw_network_body,
                next_in_chunk,
            );
            if episode_active {
                let armed = pipe_mute_armed_at.get_or_insert_with(Instant::now);
                if pipe_mute_may_fire(tip_buffered, tip_is_local_inflight)
                    && armed.elapsed() >= Duration::from_millis(mute_pipe_ms())
                {
                    warn!(
                        "[IBD_PIPE_FILL_MUTE] peer={} chunk={}-{} tip={} in_flight={} received={} gap_streams={} pipe_fill_recv0={} after {}ms — mute eject",
                        peer_id,
                        start_height,
                        end_height,
                        next_to_send,
                        in_flight.len(),
                        received.len(),
                        gap_streams,
                        pipe_fill_recv0,
                        mute_pipe_ms()
                    );
                    peer_scorer.record_failure(peer_addr);
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    return Err(anyhow::anyhow!(
                        "PIPE_FILL mute: no network body in {}ms (chunk {}-{})",
                        mute_pipe_ms(),
                        start_height,
                        end_height
                    ));
                }
            } else if pipe_mute_should_clear_clock(
                episode_active,
                gap_streams,
                pipe_fill_recv0,
                saw_network_body,
            ) {
                pipe_mute_armed_at = None;
            }
        }

        if resync_next_to_send_with_validation_tip(
            validation_height.as_ref(),
            &mut received,
            &mut next_to_send,
            end_height,
            &network,
            peer_addr,
            &block_hash_by_height,
            &in_flight_heights,
            &mut last_gap_at,
        ) {
            // Drain immediately-ready in_flight (local blocks) into `received` before exit.
            // Without this, CHUNK_OBSOLETE drops local tip bodies still sitting unpolled in
            // FuturesUnordered (live: 698200 local loaded → obsolete → never GAP_STREAM).
            {
                use std::task::{Context, Poll};
                let waker = futures::task::noop_waker_ref();
                let mut cx = Context::from_waker(waker);
                while let Poll::Ready(Some((height, block_hash, _, block_result, permit))) =
                    in_flight.poll_next_unpin(&mut cx)
                {
                    drop(permit);
                    in_flight_heights.remove(&height);
                    if let Ok(Ok((block, block_witnesses, _wire))) = block_result {
                        let received_hash = blockstore.get_block_hash(&block);
                        if received_hash == block_hash {
                            ARC_BLOCK_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            received_put(
                                &mut received,
                                height,
                                (Arc::new(block), Arc::new(block_witnesses)),
                            );
                        }
                    }
                }
                // Cancel any still-pending network requests left unpolled.
                for &h in &in_flight_heights {
                    if let Some(&h_hash) = block_hash_by_height.get(&h) {
                        network.cancel_block_request(peer_addr, h_hash);
                    }
                }
                in_flight_heights.clear();
            }
            // W13: chunk entirely behind tip — flush anything left and free the peer.
            flush_received_on_abort(
                &mut received,
                block_tx.as_ref(),
                start_height,
                end_height,
                next_to_send,
                validation_height.as_deref(),
            )
            .await;
            received_drain_all(&mut received);
            info!(
                "[IBD_CHUNK_OBSOLETE] chunk {}-{} behind tip — releasing peer {}",
                start_height, end_height, peer_id
            );
            return Ok(DownloadChunkResult {
                blocks,
                streamed_block_count: if streaming {
                    streamed_block_count
                } else {
                    0
                },
            });
        }
        // W13: also exit cleanly if streaming cursor past end after resync.
        if next_to_send > end_height {
            received_drain_all(&mut received);
            return Ok(DownloadChunkResult {
                blocks,
                streamed_block_count: if streaming {
                    streamed_block_count
                } else {
                    0
                },
            });
        }
        let next_result = if progress.last_block_hash.is_none() {
            if let Some(ref mut rx) = stall_rx {
                // biased: poll in_flight first so a locally-cached block (resolved immediately)
                // is never preempted by a stall signal that arrived in the broadcast channel
                // before this select runs. Without biased, tokio may pick stall_rx non-deterministically
                // and abort a chunk we're already holding the first block for.
                tokio::select! {
                    biased;
                    r = in_flight.next() => r,
                    p = persist_inflight.next(), if !persist_inflight.is_empty() => {
                        if let Some((ph, persist_res)) = p {
                            if let Err(e) = persist_res {
                                warn!(
                                    "[IBD_GAP_PERSIST] height {}: persist failed: {e}",
                                    ph
                                );
                            }
                            disk_ready.insert(ph);
                            if try_stream_validation_gap(
                                validation_height.as_ref(),
                                &mut received,
                                block_tx.as_ref(),
                                start_height,
                                end_height,
                            )
                            .await?
                            {
                                last_gap_at = std::time::Instant::now();
                                gap_streams = gap_streams.saturating_add(1);
                                gap_soft_retries = 0;
                                sync_next_to_send_after_gap_stream(&mut next_to_send, end_height);
                                network_tip_streamed = true;
                                if let Some(ref a) = tip_enter {
                                    // Body path may already have tip-credited; STREAM-only
                                    // credit for non-adjacent heights that drained a tip gap.
                                    let tip_adjacent = ph <= tip_now.saturating_add(1)
                                        || ph == next_to_send;
                                    if !tip_adjacent {
                                        a.note_wan_tip_stream(peer_id);
                                    }
                                }
                                if gap_streams == 1 {
                                    info!(
                                        "[IBD_GAP_STREAM] streamed tip gap from chunk {}-{} to coordinator (peer={})",
                                        start_height, end_height, peer_id
                                    );
                                }
                                if !progressive_timeout_applied
                                    && network_tip_streamed
                                    && end_height > start_height
                                    && tip_progressive_secs > tip_aware_secs
                                {
                                    progressive_timeout_applied = true;
                                    warn!(
                                        "[IBD_TIP_PROGRESSIVE_TIMEOUT] tip streamed from {}-{} — remaining blocks timeout {}s (was {}s)",
                                        start_height,
                                        end_height,
                                        tip_progressive_secs,
                                        tip_aware_secs
                                    );
                                }
                            }
                        }
                        continue;
                    }
                    stall_res = rx.recv() => {
                        if let Ok(stall_h) = stall_res {
                            if stall_h >= start_height && stall_h <= end_height {
                                // Ignore premature coordinator stalls while the first block is
                                // still in flight (common at height 1 before peer responds).
                                if chunk_start_time.elapsed() < first_block_wait {
                                    continue;
                                }
                                if !ibd_stall_aborts_inflight_gap_fetch(
                                    wan_multi_peer,
                                    confirmed_body_height,
                                    stall_h,
                                ) {
                                    continue;
                                }
                                warn!("Coordinator stall at {}: aborting chunk {}-{} (no first block yet)", stall_h, start_height, end_height);
                                for &h in &in_flight_heights {
                                    if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                        network.cancel_block_request(peer_addr, h_hash);
                                    }
                                }
                                return Err(anyhow::anyhow!(
                                    "Coordinator stall: aborting chunk {}-{} for retry",
                                    start_height, end_height
                                ));
                            }
                        }
                        continue;
                    }
                    _ = wait_tip_enter_abort(&tip_enter, peer_id, start_height, end_height) => {
                        warn!(
                            "[IBD_TIP_ENTER] aborting ahead chunk {}-{} (peer={}) — tip walked in",
                            start_height, end_height, peer_id
                        );
                        flush_received_on_abort(
                            &mut received,
                            block_tx.as_ref(),
                            start_height,
                            end_height,
                            next_to_send,
                            validation_height.as_deref(),
                        )
                        .await;
                        for &h in &in_flight_heights {
                            if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                network.cancel_block_request(peer_addr, h_hash);
                            }
                        }
                        return Err(anyhow::anyhow!(
                            "tip-enter walk-in: aborting chunk {}-{} for tip owner reassign",
                            start_height, end_height
                        ));
                    }
                    _ = wait_blacklist_abort(&tip_enter, peer_id) => {
                        warn!(
                            "[IBD_TIP_BLACKLIST] aborting chunk {}-{} (peer={}) — tip-SLA rotate",
                            start_height, end_height, peer_id
                        );
                        flush_received_on_abort(
                            &mut received,
                            block_tx.as_ref(),
                            start_height,
                            end_height,
                            next_to_send,
                            validation_height.as_deref(),
                        )
                        .await;
                        for &h in &in_flight_heights {
                            if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                network.cancel_block_request(peer_addr, h_hash);
                            }
                        }
                        return Err(anyhow::anyhow!(
                            "tip-SLA blacklist: aborting chunk {}-{} for peer rotate",
                            start_height, end_height
                        ));
                    }
                    _ = tokio::time::sleep(first_block_wait) => {
                        let waited = first_block_wait.as_secs();
                        warn!(
                            "Chunk {} to {}: no first block in {}s, failing for retry",
                            start_height, end_height, waited
                        );
                        for &h in &in_flight_heights {
                            if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                network.cancel_block_request(peer_addr, h_hash);
                            }
                        }
                        return Err(anyhow::anyhow!(
                            "Block download stalled (no first block in {waited}s)"
                        ));
                    }
                    _ = wait_cooperative_outer(outer_deadline) => {
                        if try_soft_extend_outer(
                            &mut outer_deadline,
                            &mut outer_extends,
                            &mut outer_deadline_secs,
                            last_gap_at,
                            gap_streams,
                            chunk_start_time,
                            start_height,
                            end_height,
                            peer_id,
                        ) {
                            continue;
                        }
                        return abort_on_outer_deadline(
                            &mut received,
                            block_tx.as_ref(),
                            start_height,
                            end_height,
                            next_to_send,
                            validation_height.as_deref(),
                            &in_flight_heights,
                            &block_hash_by_height,
                            &network,
                            peer_addr,
                            &peer_scorer,
                            outer_deadline_secs,
                        )
                        .await;
                    }
                }
            } else {
                tokio::select! {
                    r = in_flight.next() => r,
                    p = persist_inflight.next(), if !persist_inflight.is_empty() => {
                        if let Some((ph, persist_res)) = p {
                            if let Err(e) = persist_res {
                                warn!(
                                    "[IBD_GAP_PERSIST] height {}: persist failed: {e}",
                                    ph
                                );
                            }
                            disk_ready.insert(ph);
                            if try_stream_validation_gap(
                                validation_height.as_ref(),
                                &mut received,
                                block_tx.as_ref(),
                                start_height,
                                end_height,
                            )
                            .await?
                            {
                                last_gap_at = std::time::Instant::now();
                                gap_streams = gap_streams.saturating_add(1);
                                gap_soft_retries = 0;
                                sync_next_to_send_after_gap_stream(&mut next_to_send, end_height);
                                network_tip_streamed = true;
                                if let Some(ref a) = tip_enter {
                                    // Body path may already have tip-credited; STREAM-only
                                    // credit for non-adjacent heights that drained a tip gap.
                                    let tip_adjacent = ph <= tip_now.saturating_add(1)
                                        || ph == next_to_send;
                                    if !tip_adjacent {
                                        a.note_wan_tip_stream(peer_id);
                                    }
                                }
                                if gap_streams == 1 {
                                    info!(
                                        "[IBD_GAP_STREAM] streamed tip gap from chunk {}-{} to coordinator (peer={})",
                                        start_height, end_height, peer_id
                                    );
                                }
                                if !progressive_timeout_applied
                                    && network_tip_streamed
                                    && end_height > start_height
                                    && tip_progressive_secs > tip_aware_secs
                                {
                                    progressive_timeout_applied = true;
                                    warn!(
                                        "[IBD_TIP_PROGRESSIVE_TIMEOUT] tip streamed from {}-{} — remaining blocks timeout {}s (was {}s)",
                                        start_height,
                                        end_height,
                                        tip_progressive_secs,
                                        tip_aware_secs
                                    );
                                }
                            }
                        }
                        continue;
                    }
                    _ = tokio::time::sleep(first_block_wait) => {
                        let waited = first_block_wait.as_secs();
                        warn!(
                            "Chunk {} to {}: no first block in {}s, failing for retry",
                            start_height, end_height, waited
                        );
                        for &h in &in_flight_heights {
                            if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                network.cancel_block_request(peer_addr, h_hash);
                            }
                        }
                        return Err(anyhow::anyhow!(
                            "Block download stalled (no first block in {waited}s)"
                        ));
                    }
                    _ = wait_cooperative_outer(outer_deadline) => {
                        if try_soft_extend_outer(
                            &mut outer_deadline,
                            &mut outer_extends,
                            &mut outer_deadline_secs,
                            last_gap_at,
                            gap_streams,
                            chunk_start_time,
                            start_height,
                            end_height,
                            peer_id,
                        ) {
                            continue;
                        }
                        return abort_on_outer_deadline(
                            &mut received,
                            block_tx.as_ref(),
                            start_height,
                            end_height,
                            next_to_send,
                            validation_height.as_deref(),
                            &in_flight_heights,
                            &block_hash_by_height,
                            &network,
                            peer_addr,
                            &peer_scorer,
                            outer_deadline_secs,
                        )
                        .await;
                    }
                }
            }
        } else if let Some(ref mut rx) = stall_rx {
            // We have started receiving blocks. Race in_flight, stall signal, and hard deadline.
            tokio::select! {
                r = in_flight.next() => r,
                    p = persist_inflight.next(), if !persist_inflight.is_empty() => {
                        if let Some((ph, persist_res)) = p {
                            if let Err(e) = persist_res {
                                warn!(
                                    "[IBD_GAP_PERSIST] height {}: persist failed: {e}",
                                    ph
                                );
                            }
                            disk_ready.insert(ph);
                            if try_stream_validation_gap(
                                validation_height.as_ref(),
                                &mut received,
                                block_tx.as_ref(),
                                start_height,
                                end_height,
                            )
                            .await?
                            {
                                last_gap_at = std::time::Instant::now();
                                gap_streams = gap_streams.saturating_add(1);
                                gap_soft_retries = 0;
                                sync_next_to_send_after_gap_stream(&mut next_to_send, end_height);
                                network_tip_streamed = true;
                                if let Some(ref a) = tip_enter {
                                    // Body path may already have tip-credited; STREAM-only
                                    // credit for non-adjacent heights that drained a tip gap.
                                    let tip_adjacent = ph <= tip_now.saturating_add(1)
                                        || ph == next_to_send;
                                    if !tip_adjacent {
                                        a.note_wan_tip_stream(peer_id);
                                    }
                                }
                                if gap_streams == 1 {
                                    info!(
                                        "[IBD_GAP_STREAM] streamed tip gap from chunk {}-{} to coordinator (peer={})",
                                        start_height, end_height, peer_id
                                    );
                                }
                                if !progressive_timeout_applied
                                    && network_tip_streamed
                                    && end_height > start_height
                                    && tip_progressive_secs > tip_aware_secs
                                {
                                    progressive_timeout_applied = true;
                                    warn!(
                                        "[IBD_TIP_PROGRESSIVE_TIMEOUT] tip streamed from {}-{} — remaining blocks timeout {}s (was {}s)",
                                        start_height,
                                        end_height,
                                        tip_progressive_secs,
                                        tip_aware_secs
                                    );
                                }
                            }
                        }
                        continue;
                    }
                stall_res = rx.recv() => {
                    // Coordinator detected validation waiting on our gap height — fail fast so
                    // assigner can requeue a gap micro-chunk to another peer instead of burning
                    // another chunk_deadline_secs on the same slow peer.
                    if let Ok(stall_h) = stall_res {
                        if stall_h >= start_height
                            && stall_h <= end_height
                            && stall_h == next_to_send
                        {
                            if !ibd_stall_aborts_inflight_gap_fetch(
                                wan_multi_peer,
                                confirmed_body_height,
                                stall_h,
                            ) {
                                continue;
                            }
                            warn!(
                                "Coordinator stall at gap height {}: aborting chunk {}-{} for immediate peer retry",
                                stall_h, start_height, end_height
                            );
                            peer_scorer.record_failure(peer_addr);
                            // Flush buffered blocks to coordinator before cancelling so
                            // already-downloaded heights don't need to be re-fetched.
                            flush_received_on_abort(
                                &mut received,
                                block_tx.as_ref(),
                                start_height,
                                end_height,
                                next_to_send,
                                validation_height.as_deref(),
                            )
                            .await;
                            // Cancel all in-flight requests: their rx handles will be
                            // dropped at function exit but tx entries remain in
                            // pending_block_requests.  A late peer response would find
                            // a stale tx, silently discard the block, and stall IBD.
                            for &h in &in_flight_heights {
                                if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                    network.cancel_block_request(peer_addr, h_hash);
                                }
                            }
                            return Err(anyhow::anyhow!(
                                "Coordinator stall at gap height {} - chunk {}-{} needs retry",
                                stall_h, start_height, end_height
                            ));
                        }
                    }
                    continue;
                }
                _ = wait_tip_enter_abort(&tip_enter, peer_id, start_height, end_height) => {
                    warn!(
                        "[IBD_TIP_ENTER] aborting ahead chunk {}-{} (peer={}) — tip walked in",
                        start_height, end_height, peer_id
                    );
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "tip-enter walk-in: aborting chunk {}-{} for tip owner reassign",
                        start_height, end_height
                    ));
                }
                _ = wait_blacklist_abort(&tip_enter, peer_id) => {
                    warn!(
                        "[IBD_TIP_BLACKLIST] aborting chunk {}-{} (peer={}) — tip-SLA rotate",
                        start_height, end_height, peer_id
                    );
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "tip-SLA blacklist: aborting chunk {}-{} for peer rotate",
                        start_height, end_height
                    ));
                }
                _ = deadline_poll.tick() => {
                    if last_gap_at.elapsed() < Duration::from_secs(chunk_deadline_secs) {
                        continue;
                    }
                    warn!(
                        "Chunk {}-{}: hard {}s gap deadline expired (next_to_send={}, in_flight={}, received={}) — aborting for retry",
                        start_height, end_height, chunk_deadline_secs,
                        next_to_send, in_flight.len(), received.len()
                    );
                    peer_scorer.record_failure(peer_addr);
                    // Flush blocks we already have so they don't need re-downloading.
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "Chunk hard deadline {}-{}: stuck at height {} after {}s",
                        start_height, end_height, next_to_send, chunk_deadline_secs
                    ));
                }
                _ = wait_cooperative_outer(outer_deadline) => {
                    if try_soft_extend_outer(
                        &mut outer_deadline,
                        &mut outer_extends,
                        &mut outer_deadline_secs,
                        last_gap_at,
                        gap_streams,
                        chunk_start_time,
                        start_height,
                        end_height,
                        peer_id,
                    ) {
                        continue;
                    }
                    return abort_on_outer_deadline(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                        &in_flight_heights,
                        &block_hash_by_height,
                        &network,
                        peer_addr,
                        &peer_scorer,
                        outer_deadline_secs,
                    )
                    .await;
                }
            }
        } else {
            tokio::select! {
                r = in_flight.next() => r,
                    p = persist_inflight.next(), if !persist_inflight.is_empty() => {
                        if let Some((ph, persist_res)) = p {
                            if let Err(e) = persist_res {
                                warn!(
                                    "[IBD_GAP_PERSIST] height {}: persist failed: {e}",
                                    ph
                                );
                            }
                            disk_ready.insert(ph);
                            if try_stream_validation_gap(
                                validation_height.as_ref(),
                                &mut received,
                                block_tx.as_ref(),
                                start_height,
                                end_height,
                            )
                            .await?
                            {
                                last_gap_at = std::time::Instant::now();
                                gap_streams = gap_streams.saturating_add(1);
                                gap_soft_retries = 0;
                                sync_next_to_send_after_gap_stream(&mut next_to_send, end_height);
                                network_tip_streamed = true;
                                if let Some(ref a) = tip_enter {
                                    // Body path may already have tip-credited; STREAM-only
                                    // credit for non-adjacent heights that drained a tip gap.
                                    let tip_adjacent = ph <= tip_now.saturating_add(1)
                                        || ph == next_to_send;
                                    if !tip_adjacent {
                                        a.note_wan_tip_stream(peer_id);
                                    }
                                }
                                if gap_streams == 1 {
                                    info!(
                                        "[IBD_GAP_STREAM] streamed tip gap from chunk {}-{} to coordinator (peer={})",
                                        start_height, end_height, peer_id
                                    );
                                }
                                if !progressive_timeout_applied
                                    && network_tip_streamed
                                    && end_height > start_height
                                    && tip_progressive_secs > tip_aware_secs
                                {
                                    progressive_timeout_applied = true;
                                    warn!(
                                        "[IBD_TIP_PROGRESSIVE_TIMEOUT] tip streamed from {}-{} — remaining blocks timeout {}s (was {}s)",
                                        start_height,
                                        end_height,
                                        tip_progressive_secs,
                                        tip_aware_secs
                                    );
                                }
                            }
                        }
                        continue;
                    }
                _ = wait_blacklist_abort(&tip_enter, peer_id) => {
                    warn!(
                        "[IBD_TIP_BLACKLIST] aborting chunk {}-{} (peer={}) — tip-SLA rotate",
                        start_height, end_height, peer_id
                    );
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "tip-SLA blacklist: aborting chunk {}-{} for peer rotate",
                        start_height, end_height
                    ));
                }
                _ = deadline_poll.tick() => {
                    if last_gap_at.elapsed() < Duration::from_secs(chunk_deadline_secs) {
                        continue;
                    }
                    warn!(
                        "Chunk {}-{}: hard {}s gap deadline expired (no stall_rx, next_to_send={}) — aborting for retry",
                        start_height, end_height, chunk_deadline_secs, next_to_send
                    );
                    peer_scorer.record_failure(peer_addr);
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "Chunk hard deadline {}-{}: no stall_rx, stuck at height {} after {}s",
                        start_height, end_height, next_to_send, chunk_deadline_secs
                    ));
                }
                _ = wait_cooperative_outer(outer_deadline) => {
                    if try_soft_extend_outer(
                        &mut outer_deadline,
                        &mut outer_extends,
                        &mut outer_deadline_secs,
                        last_gap_at,
                        gap_streams,
                        chunk_start_time,
                        start_height,
                        end_height,
                        peer_id,
                    ) {
                        continue;
                    }
                    return abort_on_outer_deadline(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                        &in_flight_heights,
                        &block_hash_by_height,
                        &network,
                        peer_addr,
                        &peer_scorer,
                        outer_deadline_secs,
                    )
                    .await;
                }
            }
        };

        let Some((height, block_hash, request_start, block_result, permit)) = next_result else {
            // B11v5: GetData may be drained while GAP_PERSIST ACKs are still in flight —
            // do not end the chunk until those ACKs can STREAM tip.
            if !persist_inflight.is_empty() {
                if let Some((ph, persist_res)) = persist_inflight.next().await {
                    if let Err(e) = persist_res {
                        warn!(
                            "[IBD_GAP_PERSIST] height {}: persist failed: {e}",
                            ph
                        );
                    }
                    disk_ready.insert(ph);
                    if try_stream_validation_gap(
                        validation_height.as_ref(),
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                    )
                    .await?
                    {
                        last_gap_at = std::time::Instant::now();
                        gap_streams = gap_streams.saturating_add(1);
                        gap_soft_retries = 0;
                        sync_next_to_send_after_gap_stream(&mut next_to_send, end_height);
                        network_tip_streamed = true;
                        if gap_streams == 1 {
                            info!(
                                "[IBD_GAP_STREAM] streamed tip gap from chunk {}-{} to coordinator (peer={})",
                                start_height, end_height, peer_id
                            );
                        }
                        if !progressive_timeout_applied
                            && network_tip_streamed
                            && end_height > start_height
                            && tip_progressive_secs > tip_aware_secs
                        {
                            progressive_timeout_applied = true;
                            warn!(
                                "[IBD_TIP_PROGRESSIVE_TIMEOUT] tip streamed from {}-{} — remaining blocks timeout {}s (was {}s)",
                                start_height,
                                end_height,
                                tip_progressive_secs,
                                tip_aware_secs
                            );
                        }
                    }
                }
                continue;
            }
            break;
        };
        // D0: release the in-flight permit BEFORE fill_pipeline / enqueue_chunk_block.
        // Holding it across acquire_owned while in_flight is at capacity self-deadlocks
        // (WAN tip pipes: pipeline_depth == sem == 128 → 5760s safety-net, 0 soft timeouts).
        drop(permit);
        in_flight_heights.remove(&height);
        match block_result {
            Ok(Ok((block, block_witnesses, wire_payload))) => {
                // P1a: local enqueue inserts height; network GetData does not.
                let from_local = local_sourced_heights.remove(&height);
                if !from_local {
                    network_body_heights.insert(height);
                    // P1d: only tip-band net bodies cancel mute (ahead delivery must not
                    // shelter a mute tip — thermo gd_p50≈2.7s on ahead reset the 3s clock).
                    if height == next_to_send || height <= tip_now.saturating_add(1) {
                        saw_network_body = true;
                        pipe_mute_armed_at = None;
                        super::tip_stage::clear_pipe_fill_recv0_streak();
                    }
                    // C1b/C1d/C1n: deepen only on exact tip-gap body.
                    // Cap: cold=32, gd-fast=48 (EWMA), warm=128 (Mode T only).
                    // C1u: freeze/shrink while GD_SLOW — do not deepen a drip pipe.
                    // C1u′: ratchet down by GROW_STEP (not cliff to fill_cap) so EWMA
                    // flicker ≥gate does not pin depth=8 for the whole tip crawl.
                    if height == next_to_send {
                        let prev = tip_hole_grown;
                        let gd = super::tip_stage::getdata_body_ewma_ms();
                        let sole_ready = tip_enter
                            .as_ref()
                            .map(|a| a.ibd_ready_peer_count() <= 1)
                            .unwrap_or(false);
                        tip_hole_cap = tip_hole_cap_for_sole(
                            sole_ready,
                            tip_hole_grow_cap_for_peer(tip_hole_hot),
                        );
                        if tip_hole_grown > tip_hole_cap {
                            tip_hole_grown = tip_hole_cap;
                            info!(
                                "[IBD_TIP_HOLE_CAP_CLAMP] peer={} height={} depth {}→{} (cap={})",
                                peer_id, height, prev, tip_hole_grown, tip_hole_cap
                            );
                            if let Some(ref a) = tip_enter {
                                a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
                            }
                            super::tip_stage::note_tip_hole_duty(tip_hole_grown);
                        }
                        if tip_hole_gd_slow() && sole_ready && tip_hole_sole_floor_applies(height) {
                            let floor = tip_hole_sole_gd_slow_floor();
                            super::tip_stage::note_sole_floor_latch();
                            maybe_note_sole_no_fast_latch(height);
                            if tip_hole_grown != floor {
                                tip_hole_grown = floor;
                                info!(
                                    "[IBD_TIP_HOLE_GD_SLOW_SOLE_FLOOR] peer={} height={} depth {}→{} gd_ewma_ms={:?}",
                                    peer_id,
                                    height,
                                    prev,
                                    tip_hole_grown,
                                    gd.map(|(ms, _)| ms)
                                );
                                if let Some(ref a) = tip_enter {
                                    a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
                                }
                                super::tip_stage::note_tip_hole_duty(tip_hole_grown);
                            }
                        } else if sole_ready && tip_hole_sole_floor_applies(height) && tip_hole_sole_floor_blocks_grow() {
                            // Hold floor until recover; no-FAST latch separately blocks FAST_CAP.
                            let floor = tip_hole_sole_gd_slow_floor();
                            if tip_hole_grown > floor {
                                tip_hole_grown = floor;
                                info!(
                                    "[IBD_TIP_HOLE_SOLE_FLOOR_HOLD] peer={} height={} depth {}→{} gd_ewma_ms={:?} recover_ms={}",
                                    peer_id,
                                    height,
                                    prev,
                                    tip_hole_grown,
                                    gd.map(|(ms, _)| ms),
                                    tip_hole_sole_floor_recover_ms()
                                );
                                if let Some(ref a) = tip_enter {
                                    a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
                                }
                                super::tip_stage::note_tip_hole_duty(tip_hole_grown);
                            }
                        } else if tip_hole_gd_slow() && !sole_ready {
                            let slow = tip_hole_slow_fill_cap();
                            let next = tip_hole_gd_slow_next_depth(tip_hole_grown);
                            if next < tip_hole_grown {
                                tip_hole_grown = next;
                                debug!(
                                    "[IBD_TIP_HOLE_GD_SLOW] peer={} height={} depth {}→{} (fill_cap={} ratchet={} gd_ewma_ms={:?} gate_ms={})",
                                    peer_id,
                                    height,
                                    prev,
                                    tip_hole_grown,
                                    slow,
                                    tip_hole_gd_slow_ratchet_enabled(),
                                    gd.map(|(ms, _)| ms),
                                    tip_hole_gd_slow_ms()
                                );
                                if let Some(ref a) = tip_enter {
                                    a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
                                }
                                super::tip_stage::note_tip_hole_duty(tip_hole_grown);
                            }
                        } else {
                            tip_hole_grown =
                                tip_hole_grow_on_delivery_capped(tip_hole_grown, tip_hole_cap);
                            if tip_hole_grown > prev {
                                debug!(
                                    "[IBD_TIP_HOLE_GROW] peer={} height={} depth {}→{} (cap={} hot={} gd_ewma_ms={:?})",
                                    peer_id,
                                    height,
                                    prev,
                                    tip_hole_grown,
                                    tip_hole_cap,
                                    tip_hole_hot,
                                    gd.map(|(ms, _)| ms)
                                );
                                if tip_hole_cap > tip_hole_grow_cap() {
                                    debug!(
                                        "[IBD_TIP_HOLE_GD_FAST] peer={} depth={} cap={} gd_ewma_ms={:?} gate_ms={}",
                                        peer_id,
                                        tip_hole_grown,
                                        tip_hole_cap,
                                        gd.map(|(ms, _)| ms),
                                        tip_hole_gd_fast_ms()
                                    );
                                }
                                if let Some(ref a) = tip_enter {
                                    a.note_tip_hole_depth(peer_id, tip_hole_grown);
                                }
                                super::tip_stage::note_tip_hole_duty(tip_hole_grown);
                            }
                        }
                    }
                }
                let received_hash = blockstore.get_block_hash(&block);
                if received_hash != block_hash {
                    // BUGFIX: Previously we continued without inserting; next_to_send stayed at this
                    // height while the pipeline requested later heights, so we could return Ok with a
                    // gap and mark the chunk complete — permanent reorder-buffer stall (e.g. min 547).
                    warn!(
                        "Block hash mismatch for height {} (expected {}, got {}) — failing chunk for retry",
                        height,
                        hex::encode(block_hash),
                        hex::encode(received_hash)
                    );
                    peer_scorer.record_failure(peer_addr);
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "Block hash mismatch at height {} - chunk needs retry",
                        height
                    ));
                }
                progress.record_progress(received_hash);
                progress.reset_timeout();
                let latency_ms = request_start.elapsed().as_secs_f64() * 1000.0;
                // Rough wire-size for W2 EMA + peer scorer (not consensus-critical).
                let mut block_size = 80u64;
                for tx in &block.transactions {
                    block_size = block_size.saturating_add(40).saturating_add(
                        (tx.inputs.len() * 40 + tx.outputs.len() * 34) as u64,
                    );
                }
                for stack in block_witnesses.iter() {
                    for wit in stack.iter() {
                        for item in wit.iter() {
                            block_size = block_size.saturating_add(item.len() as u64);
                        }
                    }
                }
                note_download_block_bytes(block_size);
                peer_scorer.record_block(peer_addr, block_size, latency_ms);
                // W7: empty-witness MSG_BLOCK of a *commitment* block must not enter
                // `received` (stripped payload). Blocks without BIP141 commitment may
                // legitimately have empty stacks (live h=640022) — accept those.
                let segwit_on = feature_registry.is_feature_active(
                    "segwit",
                    height,
                    block.header.timestamp,
                );
                if empty_witness_unacceptable(&block, &block_witnesses, segwit_on) {
                    empty_witness_hits = empty_witness_hits.saturating_add(1);
                    // A6: demote peer after repeated empty-witness (likely MSG_BLOCK-only).
                    if empty_witness_hits >= 2 {
                        peer_scorer.record_failure(peer_addr);
                    }
                    // Cap re-requests: same peer looping forever pins validation + holds
                    // reorder/bridge memory. Abort so worker requeues with exclude + blacklist.
                    if empty_witness_hits >= max_empty_witness_hits {
                        warn!(
                            "[IBD_EMPTY_WITNESS_ABORT] height {}: {} empty-witness hits from {} — aborting chunk (max={})",
                            height, empty_witness_hits, peer_id, max_empty_witness_hits
                        );
                        for &h in &in_flight_heights {
                            if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                network.cancel_block_request(peer_addr, h_hash);
                            }
                        }
                        flush_received_on_abort(
                            &mut received,
                            block_tx.as_ref(),
                            start_height,
                            end_height,
                            next_to_send,
                            validation_height.as_deref(),
                        )
                        .await;
                        if height == next_to_send {
                            return Err(anyhow::anyhow!(
                                "empty-witness at gap height {} after {} hits - chunk needs retry",
                                height,
                                empty_witness_hits
                            ));
                        }
                        return Err(anyhow::anyhow!(
                            "empty-witness at height {} after {} hits - chunk needs retry",
                            height,
                            empty_witness_hits
                        ));
                    }
                    // Rate-limit spam: first 3 hits + every 8th thereafter.
                    if empty_witness_hits <= 3 || empty_witness_hits % 8 == 0 {
                        warn!(
                            "[IBD_EMPTY_WITNESS] height {}: rejecting empty-witness payload — re-requesting MSG_WITNESS_BLOCK (hit={}/{})",
                            height, empty_witness_hits, max_empty_witness_hits
                        );
                    }
                    enqueue_chunk_block(
                        height,
                        block_hash,
                        &network,
                        peer_addr,
                        peer_id,
                        blockstore,
                        protocol_version,
                        tip_now,
                        confirmed_body_height,
                        chunk_default_secs,
                        &blocks_sem,
                        &mut in_flight,
                        &mut in_flight_heights,
                        &mut inflight_deadlines,
                        &mut first_block_logged,
                        start_height,
                        end_height,
                        &mut local_sourced_heights,
                    )
                    .await?;
                    tip_hole_cap = tip_hole_cap_for_sole(sole_ready, tip_hole_grow_cap_for_peer(tip_hole_hot));
                    fill_pipeline(
                        next_to_send,
                        end_height,
                        pipeline_depth,
                        &received,
                        &mut in_flight,
                        &mut in_flight_heights,
                        &mut inflight_deadlines,
                        &block_hash_by_height,
                        &network,
                        peer_addr,
                        peer_id,
                        blockstore,
                        protocol_version,
                        tip_now,
                        confirmed_body_height,
                        chunk_default_secs,
                        &blocks_sem,
                        &mut first_block_logged,
                        start_height,
                        &mut local_sourced_heights,
                        tip_hole_grown,
                        tip_hole_cap,
                    )
                    .await?;
                    continue;
                }
                // B11v5 OFFLOAD: RAM handoff first, STREAM after persist ACK.
                // !OFFLOAD (H3 default): a31 order — repair + persist, then received_put
                // (including from_local). Persist still short-circuits when body is on disk.
                ARC_BLOCK_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let block_arc = Arc::new(block);
                let wit_arc = Arc::new(block_witnesses);
                if !gap_persist_offload {
                    if let Err(e) = try_repair_missing_witness(
                        blockstore,
                        height,
                        block_hash,
                        wit_arc.as_ref(),
                        protocol_version,
                        Some(block_arc.as_ref()),
                    ) {
                        warn!(
                            "[IBD_WITNESS_REPAIR] height {}: repair failed: {e}",
                            height
                        );
                    }
                    let sync_log = matches!(
                        std::env::var("BLVM_IBD_GAP_PERSIST_SYNC_LOG")
                            .ok()
                            .as_deref()
                            .map(|s| s.trim()),
                        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
                    );
                    let t_persist = Instant::now();
                    if let Err(e) = try_persist_gap_block_for_local_inject_with_wire(
                        blockstore,
                        validation_height.as_ref(),
                        height,
                        block_hash,
                        block_arc.as_ref(),
                        wit_arc.as_ref(),
                        protocol_version,
                        wire_payload.as_deref(),
                    ) {
                        warn!(
                            "[IBD_GAP_PERSIST] height {}: persist failed: {e}",
                            height
                        );
                    }
                    if sync_log {
                        let ms = t_persist.elapsed().as_millis();
                        if ms >= 20 {
                            tracing::info!(
                                "[IBD_GAP_PERSIST_SYNC] height={} persist_ms={} from_local={}",
                                height,
                                ms,
                                from_local
                            );
                        }
                    }
                }
                received_put(
                    &mut received,
                    height,
                    (Arc::clone(&block_arc), Arc::clone(&wit_arc)),
                );
                trim_download_received(
                    &mut received,
                    blockstore,
                    validation_height.as_ref(),
                    protocol_version,
                );
                super::tip_stage::mark_body(height);
                // Keep tip-hot window on tip-adjacent body arrival even when coordinator
                // already advanced via LOCAL_GAP inject of GAP_PERSIST (try_stream no-ops).
                // Live 2026-07-15: 15s without GAP_STREAM note → idle floor upgrade mid-pipe
                // → TIP_ENTER abort of a 30+ blk/s owner (since_GS=38–115s while TIP_PIPE live).
                // P1a: never credit WAN tip-stream score for try_load_local bodies.
                // a31: tip-adjacent body credits once, then GAP_STREAM credits again
                // (double-credit sticky tip BPS).
                let tip_adjacent_for_credit =
                    height <= tip_now.saturating_add(1) || height == next_to_send;
                if from_local {
                    if tip_adjacent_for_credit {
                        debug!(
                            "[IBD_TIP_LOCAL_STREAM] height={} peer={} — no tip-stream credit",
                            height, peer_id
                        );
                        // Land E: fresh STREAM arms tip-crawl supply-healthy hold.
                        super::tip_stage::note_tip_local_stream();
                    }
                } else if let Some(ref a) = tip_enter {
                    if tip_adjacent_for_credit {
                        a.note_wan_tip_stream(peer_id);
                    }
                }
                // !OFFLOAD (H3 default): a31 — STREAM immediately. No disk_ready latch.
                // OFFLOAD: spawn persist and STREAM on ACK (Land E, env off on H3).
                let mut stream_now = true;
                if gap_persist_offload && !from_local {
                    // DEFER_FAR: hold far-ahead persist until tip kick window (avoids
                    // tip-band OFFLOAD storms — Land E 404–408k cliff). Kick span may be
                    // tighter than defer span (KEEP tip30 silent-early dig).
                    let defer_far = gap_persist_defer_far_enabled();
                    let defer_span = gap_persist_defer_far_span();
                    let kick_span = gap_persist_defer_far_kick_span_for_tip(tip_now);
                    let defer_hold = defer_far
                        && !tip_adjacent_for_credit
                        && !gap_persist_in_tip_window(
                            height,
                            tip_now,
                            next_to_send,
                            kick_span,
                        );
                    if defer_hold {
                        tracing::debug!(
                            "[IBD_GAP_PERSIST_DEFER] height={} tip={} next={} kick={} defer={}",
                            height,
                            tip_now,
                            next_to_send,
                            kick_span,
                            defer_span
                        );
                        stream_now = false;
                    } else {
                        // Tip lane: dedicated sem for tip-pipe heights (not only tip+1 at arrival).
                        let tip_lane = gap_persist_tip_sync_enabled()
                            && (tip_adjacent_for_credit
                                || height
                                    <= tip_now.saturating_add(gap_persist_tip_lane_span())
                                || height
                                    <= next_to_send.saturating_add(gap_persist_tip_lane_span()));
                        let sem = if tip_lane {
                            &tip_persist_sem
                        } else {
                            &persist_sem
                        };
                        persist_spawned.insert(height);
                        spawn_gap_persist_offload(
                            &mut persist_inflight,
                            sem,
                            blockstore,
                            &validation_height,
                            height,
                            block_hash,
                            Arc::clone(&block_arc),
                            Arc::clone(&wit_arc),
                            protocol_version,
                            wire_payload,
                            tip_lane,
                        );
                        // Drain GetData / in_flight; STREAM waits for persist ACK select arm.
                        stream_now = false;
                    }
                }
                if stream_now && try_stream_validation_gap(
                    validation_height.as_ref(),
                    &mut received,
                    block_tx.as_ref(),
                    start_height,
                    end_height,
                )
                .await?
                {
                    last_gap_at = std::time::Instant::now();
                    gap_streams = gap_streams.saturating_add(1);
                    gap_soft_retries = 0;
                    // W102: STREAM advanced DEDUP (tip may still be clone-held) — advance
                    // cursor so tip-hole pipe cap disarms and fill can open full pipeline_depth.
                    sync_next_to_send_after_gap_stream(&mut next_to_send, end_height);
                    if !from_local {
                        network_tip_streamed = true;
                        if let Some(ref a) = tip_enter {
                            a.note_wan_tip_stream(peer_id);
                        }
                    }
                    // Once per chunk — logging every streamed height flooded the log
                    // (~128 lines/ms live 2026-07-15) and competed with tip crawl metrics.
                    if gap_streams == 1 {
                        info!(
                            "[IBD_GAP_STREAM] streamed tip gap from chunk {}-{} to coordinator (peer={})",
                            start_height, end_height, peer_id
                        );
                    }
                    // W29c / P1b: progressive 45s only after a *network*-sourced tip STREAM.
                    // Local tip + mute net pipe must not get shelter (G-W ×483 progressive).
                    if !progressive_timeout_applied
                        && network_tip_streamed
                        && end_height > start_height
                        && tip_progressive_secs > tip_aware_secs
                    {
                        // Armed for subsequent loop polls (see progressive_timeout_applied above).
                        progressive_timeout_applied = true;
                        warn!(
                            "[IBD_TIP_PROGRESSIVE_TIMEOUT] tip streamed from {}-{} — remaining blocks timeout {}s (was {}s)",
                            start_height,
                            end_height,
                            tip_progressive_secs,
                            tip_aware_secs
                        );
                    }
                }
                if !first_block_logged {
                    info!(
                        "[IBD] {} chunk {}-{}: first block received (h={}, {}ms)",
                        peer_id, start_height, end_height, height, latency_ms as u64
                    );
                    first_block_logged = true;
                }
            }
            Ok(Err(_)) => {
                warn!("Block channel closed for height {}", height);
                peer_scorer.record_failure(peer_addr);
                flush_received_on_abort(
                    &mut received,
                    block_tx.as_ref(),
                    start_height,
                    end_height,
                    next_to_send,
                    validation_height.as_deref(),
                )
                .await;
                for &h in &in_flight_heights {
                    if let Some(&h_hash) = block_hash_by_height.get(&h) {
                        network.cancel_block_request(peer_addr, h_hash);
                    }
                }
                return Err(anyhow::anyhow!(
                    "Block channel closed for height {} - chunk needs retry",
                    height
                ));
            }
            Err(_) => {
                if height == next_to_send {
                    // P4: soft-timeout on the gap — re-request in place (keep ahead `received`)
                    // instead of aborting the whole chunk. Hard deadline (W8) still aborts.
                    // W10/W12: budget is tip-aware — far-ahead aborts immediately; tip gap gets
                    // one soft retry then peer rotate (live: 3×45s on same peer → 0.1 BPS).
                    let tip_now = validation_height
                        .as_ref()
                        .map(|h| h.load(Ordering::Relaxed))
                        .unwrap_or(validated_tip);
                    let tip_needed_h = tip_now.saturating_add(1);
                    let ahead_buffered = tip_pipe_has_ahead_buffered(&received, tip_needed_h);
                    let hot_tip_streamer = tip_enter
                        .as_ref()
                        .is_some_and(|a| a.peer_is_hot_tip_streamer(peer_id));
                    let max_soft = gap_soft_retry_budget_for_chunk_ex(
                        height,
                        tip_now,
                        start_height,
                        end_height,
                        ahead_buffered,
                        hot_tip_streamer,
                    );
                    gap_soft_retries = gap_soft_retries.saturating_add(1);
                    network.cancel_block_request(peer_addr, block_hash);
                    if gap_soft_retries > max_soft {
                        warn!(
                            "Block timeout for gap height {} after {}s (soft retries exhausted {}/{}, tip={}, ahead_buf={}) — aborting chunk",
                            height,
                            timeout_duration.as_secs(),
                            gap_soft_retries.saturating_sub(1),
                            max_soft,
                            tip_now,
                            ahead_buffered
                        );
                        peer_scorer.record_failure(peer_addr);
                        for &h in &in_flight_heights {
                            if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                network.cancel_block_request(peer_addr, h_hash);
                            }
                        }
                        flush_received_on_abort(
                            &mut received,
                            block_tx.as_ref(),
                            start_height,
                            end_height,
                            next_to_send,
                            validation_height.as_deref(),
                        )
                        .await;
                        // W110: tag tip-gap soft-exhaust so worker mute-match stays robust
                        // even if string heuristics drift (empty soft=0 → rotate).
                        if height == tip_needed_h {
                            return Err(anyhow::anyhow!(
                                "tip-gap timeout: Block timeout for gap height {} after {}s - chunk needs retry",
                                height,
                                timeout_duration.as_secs()
                            ));
                        }
                        return Err(anyhow::anyhow!(
                            "Block timeout for gap height {} after {}s - chunk needs retry",
                            height,
                            timeout_duration.as_secs()
                        ));
                    }
                    warn!(
                        "[IBD_GAP_SOFT_RETRY] height {} after {}s (limit {}s) — re-requesting (attempt {}/{}, tip={})",
                        height,
                        request_start.elapsed().as_secs(),
                        timeout_duration.as_secs(),
                        gap_soft_retries,
                        max_soft,
                        tip_now
                    );
                    super::tip_stage::mark_soft_retry(height);
                    // W28c: tip soft-retry arms a second covering peer (failover), while this
                    // peer re-requests in place. Cleared when tip arrives in reorder.
                    // W31: never arm failover past body tip — opens 2-fetcher stall recovery on WAN.
                    if height == tip_now.saturating_add(1) {
                        // WAN: arm failover so ChunkAssigner may assign a second tip-height
                        // racer while soft-retry ahead-freeze is latched (W31 exception).
                        // Non-WAN: same as before.
                        super::tip_stage::arm_tip_failover();
                        warn!(
                            "[IBD_TIP_FAILOVER] armed after soft-retry on tip {} (peer={}, wan={})",
                            height,
                            peer_id,
                            confirmed_body_height > 0 && height > confirmed_body_height
                        );
                    }
                    // A6i: do NOT record_failure on soft-retry — expected WAN tip latency.
                    // Live: soft-retry failure penalties floored tip owners at 0.1 and fed
                    // sticky churn; only hard-abort / exhausted soft-retries demote.
                    enqueue_chunk_block(
                        height,
                        block_hash,
                        &network,
                        peer_addr,
                        peer_id,
                        blockstore,
                        protocol_version,
                        tip_now,
                        confirmed_body_height,
                        chunk_default_secs,
                        &blocks_sem,
                        &mut in_flight,
                        &mut in_flight_heights,
                        &mut inflight_deadlines,
                        &mut first_block_logged,
                        start_height,
                        end_height,
                        &mut local_sourced_heights,
                    )
                    .await?;
                    // W116: do not rebase CAP age here — live W115/W115b: spacing softs to
                    // full tip-cap windows made mute peers sticky (16–24s) and rate-failed
                    // earlier than W114 (soft burn ≈ fast rotate). Keep W114 soft budgets +
                    // W115 hot (H,H) soft=1 only.
                    tip_hole_cap = tip_hole_cap_for_sole(sole_ready, tip_hole_grow_cap_for_peer(tip_hole_hot));
                    fill_pipeline(
                        next_to_send,
                        end_height,
                        pipeline_depth,
                        &received,
                        &mut in_flight,
                        &mut in_flight_heights,
                        &mut inflight_deadlines,
                        &block_hash_by_height,
                        &network,
                        peer_addr,
                        peer_id,
                        blockstore,
                        protocol_version,
                        tip_now,
                        confirmed_body_height,
                        chunk_default_secs,
                        &blocks_sem,
                        &mut first_block_logged,
                        start_height,
                        &mut local_sourced_heights,
                        tip_hole_grown,
                        tip_hole_cap,
                    )
                    .await?;
                    continue;
                }
                // W32d: log the tier actually used (not download_timeout_secs=45).
                let used_secs = block_gap_timeout_secs(
                    height,
                    tip_now,
                    confirmed_body_height,
                    start_height,
                    end_height,
                    chunk_default_secs,
                );
                warn!(
                    "Block timeout for height {} ahead of gap {} after {}s — re-requesting",
                    height, next_to_send, used_secs
                );
                enqueue_chunk_block(
                    height,
                    block_hash,
                    &network,
                    peer_addr,
                    peer_id,
                    blockstore,
                    protocol_version,
                    tip_now,
                    confirmed_body_height,
                    chunk_default_secs,
                    &blocks_sem,
                    &mut in_flight,
                    &mut in_flight_heights,
                    &mut inflight_deadlines,
                    &mut first_block_logged,
                    start_height,
                    end_height,
                    &mut local_sourced_heights,
                )
                .await?;
                tip_hole_cap = tip_hole_cap_for_sole(sole_ready, tip_hole_grow_cap_for_peer(tip_hole_hot));
                fill_pipeline(
                    next_to_send,
                    end_height,
                    pipeline_depth,
                    &received,
                    &mut in_flight,
                    &mut in_flight_heights,
                    &mut inflight_deadlines,
                    &block_hash_by_height,
                    &network,
                    peer_addr,
                    peer_id,
                    blockstore,
                    protocol_version,
                    tip_now,
                    confirmed_body_height,
                    chunk_default_secs,
                    &blocks_sem,
                    &mut first_block_logged,
                    start_height,
                    &mut local_sourced_heights,
                    tip_hole_grown,
                    tip_hole_cap,
                )
                .await?;
                continue;
            }
        }

        while let Some((block, block_witnesses)) = received_take(&mut received, next_to_send) {
            if let Some(ref tx) = block_tx {
                let tip_need = tip_need_from(validation_height.as_ref());
                await_block_tx_tip_reserve(tx, next_to_send, tip_need).await;
                let t0 = std::time::Instant::now();
                let send_r = tx.send((next_to_send, block, block_witnesses)).await;
                let wait_ms = t0.elapsed().as_millis() as u64;
                if wait_ms >= 10 {
                    info!(
                        "[IBD_BLOCK_TX_SEND_WAIT] height={} wait_ms={} (download→coordinator channel)",
                        next_to_send, wait_ms
                    );
                }
                if send_r.is_err() {
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "block_tx closed during stream - chunk needs retry"
                    ));
                }
                streamed_block_count += 1;
                gap_streams = gap_streams.saturating_add(1);
                // P1a: drain credit only for network-sourced bodies.
                if network_body_heights.contains(&next_to_send) {
                    network_tip_streamed = true;
                    if let Some(ref a) = tip_enter {
                        a.note_wan_tip_stream(peer_id);
                    }
                }
            } else {
                blocks.push((next_to_send, block, block_witnesses));
            }
            next_to_send += 1;
            last_gap_at = std::time::Instant::now();
            gap_soft_retries = 0;
        }

        tip_hole_cap = tip_hole_cap_for_sole(sole_ready, tip_hole_grow_cap_for_peer(tip_hole_hot));
        fill_pipeline(
            next_to_send,
            end_height,
            pipeline_depth,
            &received,
            &mut in_flight,
            &mut in_flight_heights,
            &mut inflight_deadlines,
            &block_hash_by_height,
            &network,
            peer_addr,
            peer_id,
            blockstore,
            protocol_version,
            tip_now,
            confirmed_body_height,
            chunk_default_secs,
            &blocks_sem,
            &mut first_block_logged,
            start_height,
            &mut local_sourced_heights,
            tip_hole_grown,
            tip_hole_cap,
        )
        .await?;
    }

    // C1c: persist tip-hole depth for next chunk on this peer.
    if let Some(ref a) = tip_enter {
        a.note_tip_hole_depth(peer_id, tip_hole_grown);
    }

    while let Some((block, block_witnesses)) = received_take(&mut received, next_to_send) {
        if let Some(ref tx) = block_tx {
            let tip_need = tip_need_from(validation_height.as_ref());
            await_block_tx_tip_reserve(tx, next_to_send, tip_need).await;
            let t0 = std::time::Instant::now();
            let send_r = tx.send((next_to_send, block, block_witnesses)).await;
            let wait_ms = t0.elapsed().as_millis() as u64;
            if wait_ms >= 10 {
                info!(
                    "[IBD_BLOCK_TX_SEND_WAIT] height={} wait_ms={} (download→coordinator channel)",
                    next_to_send, wait_ms
                );
            }
            if send_r.is_err() {
                received_drain_all(&mut received);
                return Err(anyhow::anyhow!(
                    "block_tx closed during stream - chunk needs retry"
                ));
            }
            streamed_block_count += 1;
        } else {
            blocks.push((next_to_send, block, block_witnesses));
        }
        next_to_send += 1;
    }

    // BUGFIX: If in_flight drained but we could not stream start_height..=end_height in order
    // (e.g. hash mismatch previously skipped without Err), we must not report success — that
    // left permanent gaps in the coordinator reorder buffer.
    if next_to_send != end_height + 1 {
        let still = received.len();
        received_drain_all(&mut received);
        return Err(anyhow::anyhow!(
            "Incomplete chunk {}-{}: stuck before height {} ({} heights still buffered) — chunk needs retry",
            start_height,
            end_height,
            next_to_send,
            still
        ));
    }
    // Success path: no leftovers expected; free counter if any remain.
    received_drain_all(&mut received);

    Ok(DownloadChunkResult {
        blocks,
        streamed_block_count: if streaming { streamed_block_count } else { 0 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize tests that poke bridge/tip-stage atomics (shared with assigner tests).
    fn tip_soft_atomic_lock() -> std::sync::MutexGuard<'static, ()> {
        super::super::tip_stage::test_tip_atomics_lock()
    }

    #[test]
    fn resume_download_height_skips_validated_prefix() {
        assert_eq!(resume_download_height(955186, 955241, 955194), Some(955195));
        assert_eq!(resume_download_height(955186, 955241, 955185), Some(955186));
        assert_eq!(resume_download_height(955186, 955241, 955241), None);
    }

    #[test]
    fn wan_tip_stream_credit_dedupes_tip_adjacent_gap_stream() {
        // Common tip crawl: tip-adjacent WAN body + successful GAP_STREAM → one credit.
        assert_eq!(wan_tip_stream_credit_count(false, true, true), 1);
        // STREAM no-op (LOCAL_GAP already advanced): still one body credit.
        assert_eq!(wan_tip_stream_credit_count(false, true, false), 1);
        // Non-adjacent body that drains tip gap: STREAM-only credit.
        assert_eq!(wan_tip_stream_credit_count(false, false, true), 1);
        // Local disk: never WAN tip-stream credit.
        assert_eq!(wan_tip_stream_credit_count(true, true, true), 0);
        // Neither tip-adjacent nor streamed.
        assert_eq!(wan_tip_stream_credit_count(false, false, false), 0);
    }

    #[test]
    fn w66_received_soft_cap_covers_tip_owner_pipe() {
        // Tip-owner GetData depth is 128; soft cap must not sit below that (W65 live: 96).
        let soft = download_received_soft_cap();
        let hard = download_received_hard_cap();
        assert!(soft >= 128, "soft={soft}");
        assert!(hard >= soft, "hard={hard} soft={soft}");
    }

    /// Phase 0b.1: hard-trim may drop far-ahead under OOM; never tip-adjacent `h <= need`.
    #[test]
    fn hard_trim_never_drops_tip_adjacent_need() {
        use blvm_protocol::{Block, BlockHeader, Transaction, TransactionOutput};

        let dummy = || -> (SharedBlock, SharedWitnesses) {
            let block = Block {
                header: BlockHeader {
                    version: 1,
                    timestamp: 1,
                    ..Default::default()
                },
                transactions: vec![Transaction {
                    version: 1,
                    inputs: blvm_protocol::tx_inputs![],
                    outputs: blvm_protocol::tx_outputs![TransactionOutput {
                        value: 50,
                        script_pubkey: vec![0x51],
                    }],
                    lock_time: 0,
                }]
                .into(),
            };
            (Arc::new(block), Arc::new(vec![vec![]]))
        };

        let need = 100u64;
        let mut received: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
        // Tip window + far ahead — hard=1 forces drops of farthest only.
        for h in [need, need + 1, need + 50, need + 200] {
            received_put(&mut received, h, dummy());
        }
        let forced = hard_trim_download_received_far_ahead(&mut received, need, 1);
        assert_eq!(forced, 3, "drop three far-ahead only");
        assert!(received.contains_key(&need), "must keep h==need");
        assert_eq!(received.len(), 1);

        // Over hard with only tip-adjacent / behind-tip heights: keep all (break on h<=need).
        let mut tip_only: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
        for h in [need.saturating_sub(2), need.saturating_sub(1), need] {
            received_put(&mut tip_only, h, dummy());
        }
        let forced2 = hard_trim_download_received_far_ahead(&mut tip_only, need, 1);
        assert_eq!(forced2, 0, "must not hard-drop h<=need");
        assert_eq!(tip_only.len(), 3);
    }

    #[test]
    fn soft_outer_extend_requires_gap_streams() {
        let start = std::time::Instant::now();
        assert!(
            !should_extend_outer_while_streaming(start, 0, start, 0),
            "no streams → no extend"
        );
        assert!(
            should_extend_outer_while_streaming(start, 10, start, 0),
            "recent streams → extend"
        );
        assert!(
            !should_extend_outer_while_streaming(start, 10, start, 4),
            "max extends exhausted"
        );
    }

    #[test]
    fn chunk_outer_deadline_scales_with_remaining_blocks() {
        assert_eq!(
            chunk_outer_deadline_secs(955186, 955241, 955186, 30),
            56 * 30
        );
        assert_eq!(
            chunk_outer_deadline_secs(955186, 955241, 955195, 30),
            47 * 30
        );
        // Multi-block chunk with 2 remaining: 2 × 30 = 60 ≥ 35 min.
        assert_eq!(chunk_outer_deadline_secs(100, 101, 100, 30), 60);
        // Multi-block chunk, only 1 block remaining after resume: formula gives 1s → clamped to 35.
        assert_eq!(chunk_outer_deadline_secs(100, 101, 101, 1), 35, "multi-block minimum is 35");
    }

    #[test]
    fn empty_witness_hit_cap_defaults_to_four() {
        // Don't assert env-free if the soak set BLVM_IBD_EMPTY_WITNESS_MAX; just check clamp.
        let c = empty_witness_hit_cap();
        assert!((2..=32).contains(&c), "cap must be in 2..=32, got {c}");
    }

    #[test]
    fn gap_soft_retry_budget_tip_far_and_mid() {
        let tip = 684_955;
        assert_eq!(
            gap_soft_retry_budget(tip + 1, tip),
            tip_gap_soft_retries(),
            "tip gap: keep soft retries before abort/rotate"
        );
        assert_eq!(
            gap_soft_retry_budget(tip + 1 + far_ahead_band() + 1, tip),
            0,
            "far ahead: abort on first timeout"
        );
        assert_eq!(
            gap_soft_retry_budget(tip + 2, tip),
            3,
            "near-ahead of tip (not exact gap): keep P4 budget"
        );
        assert_eq!(
            gap_soft_retry_budget(tip, tip),
            0,
            "W14: behind tip must abort immediately"
        );
        assert_eq!(
            gap_soft_retry_budget(tip.saturating_sub(10), tip),
            0,
            "W14: far behind tip must abort immediately"
        );
    }

    #[test]
    fn w70_tip_gap_soft_budget_one_for_hh_and_deep() {
        // W171: holey soft=1 @holes≥20 only if pending==0; soft=1 @holes≥32 always.
        let tip = 261_654;
        let tip_needed = tip + 1;
        let _g = tip_soft_atomic_lock();
        // pending>0 → holey path (not empty-bridge soft).
        super::super::memory::BRIDGE_PENDING_COUNT.store(1, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
            2,
            "W156: holey (H,H) soft=2 when holes&lt;20"
        );
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
            2,
            "W156: holey deep soft=2 when holes&lt;20"
        );
        crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(19, Ordering::Relaxed);
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
            2,
            "W167: holes=19 deep soft=2"
        );
        crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(20, Ordering::Relaxed);
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
            2,
            "W171: holes≥20 + pending>0 deep soft=2 (no tip-step)"
        );
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
            2,
            "W171: holes≥20 + pending>0 (H,H) soft=2"
        );
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        // empty path: pending==0 gap_missing → soft=2 (W158), not holey soft=1.
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
            2,
            "W158: empty deep soft=2"
        );
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        super::super::memory::BRIDGE_PENDING_COUNT.store(1, Ordering::Relaxed);
        crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(32, Ordering::Relaxed);
        assert_eq!(
            gap_soft_retry_budget_for_chunk_ex(
                tip_needed,
                tip,
                tip_needed,
                tip_needed + 127,
                true,
                false,
            ),
            1,
            "W171: holes≥32 soft=1 even with pending>0"
        );
        crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(7, Ordering::Relaxed);
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
            2,
            "W156: holes=7 still soft=2"
        );
        // W171: holes≥20 + pending==0 + !gap_missing → soft=1 (no runway holey).
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(20, Ordering::Relaxed);
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
            1,
            "W171: holes≥20 pending==0 soft=1"
        );
        crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
        super::super::memory::BRIDGE_PENDING_COUNT.store(1, Ordering::Relaxed);
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed + 75, tip, tip_needed, tip_needed + 127),
            3,
            "near-ahead inside deep tip pipe keeps P4 budget"
        );
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed + 5, tip, tip_needed, tip_needed),
            3,
            "non-tip height in a (weird) singleton still uses near-ahead budget"
        );
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    }

    #[test]
    fn w83_deep_pipe_tip_soft_one_hh_stays_zero() {
        // W147: holey deep soft=2; holey (H,H) soft=2; empty HH soft=2.
        let tip = 333_000;
        let tip_needed = tip + 1;
        let _g = tip_soft_atomic_lock();
        super::super::memory::BRIDGE_PENDING_COUNT.store(1, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
            2,
            "W154: deep holey tip soft=2 (holes&lt;32)"
        );
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
            2,
            "W154: holey (H,H) soft=2 (holes&lt;32)"
        );
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(tip_needed);
        super::super::tip_stage::test_backdate_awaiting_ms(1_000);
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
            2,
            "W146: empty (H,H) soft=2"
        );
        super::super::tip_stage::mark_needed(0);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    }

    #[test]
    fn w139_empty_bridge_deep_soft_two_hh_still_zero() {
        // W158: empty deep soft=2 always; empty HH soft=2 (W154 DNA re-soak).
        let tip = 322_610;
        let tip_needed = tip + 1;
        let _g = tip_soft_atomic_lock();
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(tip_needed);
        super::super::tip_stage::test_backdate_awaiting_ms(17_000);
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
            2,
            "W158: empty deep soft=2 even when awaiting≥16s"
        );
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
            2,
            "W146: empty (H,H) soft=2 (survives progressive+CAP)"
        );
        super::super::tip_stage::mark_needed(0);
        super::super::memory::BRIDGE_PENDING_COUNT.store(4, Ordering::Relaxed);
        crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
            2,
            "W152: pending>0 holey deep soft=2 (holes&lt;8)"
        );
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    }

    #[test]
    fn w139_empty_deep_cap_eight_hh_five() {
        let _g = tip_soft_atomic_lock();
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
        assert_eq!(
            tip_gap_timeout_secs_for_chunk(false, 316_114, 316_241),
            16,
            "W184: empty deep CAP=16 (W182 was 12)"
        );
        assert_eq!(
            tip_gap_timeout_secs_for_chunk(false, 316_114, 316_114),
            5,
            "W139: empty (H,H) mute CAP=5"
        );
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    }

    #[test]
    fn w71_tip_gap_inflight_exceeded_respects_cap() {
        let started = Instant::now() - Duration::from_secs(45);
        assert!(
            tip_gap_inflight_exceeded(started, 10),
            "W71: 45s wait exceeds tip-gap cap 10s (live tip=322456 after 45s/limit 10s)"
        );
        assert!(!tip_gap_inflight_exceeded(Instant::now(), 10));
    }

    #[test]
    fn w89b_tip_hole_cap_requires_await_trigger() {
        let _g = tip_soft_atomic_lock();
        let prev_holes = super::super::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
        super::super::tip_stage::mark_needed(0);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
        let cold = tip_gap_timeout_secs();
        assert!(cold >= 8, "cold tip CAP floor 8 (W80); got {cold}");
        // Standing holes with fresh tip clock must NOT shorten CAP.
        super::super::IBD_TIP_BRIDGE_HOLES.store(23, Ordering::Relaxed);
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(328_690);
        assert_eq!(
            tip_gap_timeout_secs(),
            cold,
            "W89b: holes without ≥trigger wait keep base CAP"
        );
        // W103/W109: hole trigger default 3s; empty/hole mute CAP default 5s.
        std::thread::sleep(std::time::Duration::from_millis(3100));
        let hole = tip_gap_timeout_secs();
        assert_eq!(hole, 5, "W103/W109: holes + awaiting≥3s + pending=0 → CAP 5s; got {hole}");
        assert!(hole < cold);
        // W106/W184: holes + pending>0 → CAP 16s (W182 late-band getdata→body p90≈15.4s).
        super::super::memory::BRIDGE_PENDING_COUNT.store(28, Ordering::Relaxed);
        let holey = tip_gap_timeout_secs();
        assert_eq!(holey, 16, "W184: holes + pending>0 → CAP 16s; got {holey}");
        // W176/W184: export-active floor tracks holey default (16s).
        super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(true, Ordering::Relaxed);
        let during_export = tip_gap_timeout_secs();
        assert_eq!(
            during_export, 16,
            "W184: export-active holey CAP floor 16s; got {during_export}"
        );
        super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(false, Ordering::Relaxed);
        // W104/W109: empty-bridge mute CAP 5s immediately (trigger default 0).
        super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(314_596);
        let empty = tip_gap_timeout_secs();
        assert_eq!(empty, 5, "W109: empty mute → CAP 5s; got {empty}");
        // W109b: ahead_buffered must NOT lengthen mute CAP (soft=1 handles live pipes).
        assert_eq!(
            tip_gap_timeout_secs_ex(true),
            5,
            "W109b: empty + ahead_buffered still CAP 5s (mute rotate)"
        );
        // Land E: empty deep stripe + TIP_HOLE_AHEAD cheese must not wait holey 16s.
        super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
        let empty_deep = tip_gap_timeout_secs_for_chunk(false, 406_000, 406_031);
        assert_eq!(
            empty_deep, 16,
            "W139: empty deep, no cheese → holey 16s; got {empty_deep}"
        );
        assert_eq!(
            tip_gap_timeout_secs_for_chunk(true, 406_000, 406_031),
            5,
            "W109: empty deep + pipe ahead_buffered → mute 5s"
        );
        super::super::IBD_REORDER_AHEAD.store(68, Ordering::Relaxed);
        assert_eq!(
            tip_gap_timeout_secs_for_chunk(false, 406_000, 406_031),
            5,
            "Land E: empty deep + reorder cheese → mute 5s (not 16s leftover)"
        );
        super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(0);
        super::super::IBD_TIP_BRIDGE_HOLES.store(prev_holes, Ordering::Relaxed);
    }

    #[test]
    fn w110_tip_covering_fail_is_mute_matches_empty_rotate() {
        assert!(tip_covering_fail_is_mute(
            "tip-gap timeout cap: gap 311585 waited 5s in chunk 311585-311712"
        ));
        assert!(tip_covering_fail_is_mute(
            "tip-gap timeout: Block timeout for gap height 311585 after 5s - chunk needs retry"
        ));
        assert!(tip_covering_fail_is_mute(
            "Block timeout for gap height 311585 after 5s - chunk needs retry"
        ));
        assert!(tip_covering_fail_is_mute(
            "Block download stalled (no first block in 5s)"
        ));
        // P1d: PIPE_FILL mute eject must take the mute fail path (tip-role ban).
        assert!(tip_covering_fail_is_mute(
            "PIPE_FILL mute: gap streamed but no network body in 3000ms (chunk 304697-304824)"
        ));
        assert!(tip_covering_fail_is_mute(
            "PIPE_FILL mute: no network body in 3000ms (chunk 304697-304824)"
        ));
        // P1d clock: local tip advances must not clear; network tip-band body does.
        assert!(pipe_mute_episode_active(1, true, false, true));
        assert!(!pipe_mute_should_clear_clock(false, 1, true, false));
        assert!(pipe_mute_should_clear_clock(false, 1, true, true));
        assert!(!pipe_mute_may_fire(true, false));
        assert!(!pipe_mute_may_fire(false, true));
        assert!(pipe_mute_may_fire(false, false));
        assert!(
            !tip_covering_fail_is_mute(
                "tip-SLA blacklist: aborting chunk 311585-311712 for peer rotate"
            ),
            "SLA keep long cooldown / blacklist path"
        );
        assert!(!tip_covering_fail_is_mute(
            "tip-enter walk-in abort: keeping sticky owner"
        ));
        assert!(!tip_covering_fail_is_mute(
            "Peer disconnected during chunk download"
        ));
        // Mode T: GetData cancel from tip re-arm is not a mute CAP (mod.rs skips
        // note_tip_owner_failed entirely for this string).
        assert!(!tip_covering_fail_is_mute(
            "Block channel closed for height 402430 - chunk needs retry"
        ));
    }

    #[test]
    fn p1d_mute_pipe_ms_default_and_clamp() {
        let _g = std::sync::Mutex::new(());
        // Avoid racing other tests that touch env — only assert clamp helper bounds.
        let ms = mute_pipe_ms();
        assert!(
            (1000..=15_000).contains(&ms),
            "mute_pipe_ms must clamp to 1–15s, got {ms}"
        );
    }

    #[test]
    fn c1_tip_hole_grow_on_delivery_deepens_to_cap() {
        // Pure arithmetic under ambient env (grow default on; grow_cap default 32).
        // Reset EWMA so C1n gd-fast does not elevate the cap mid-test.
        super::super::tip_stage::test_reset_getdata_body_ewma();
        let start = tip_hole_grow_start();
        let grow_cap = tip_hole_grow_cap();
        let pipe_cap = tip_hole_pipe_cap();
        assert!((2..=128).contains(&pipe_cap));
        assert!(grow_cap <= pipe_cap);
        if tip_hole_grow_enabled() {
            assert!(start <= grow_cap);
            let next = tip_hole_grow_on_delivery(start);
            assert!(next >= start);
            assert!(next <= grow_cap);
            let mut d = start;
            for _ in 0..32 {
                d = tip_hole_grow_on_delivery(d);
            }
            assert_eq!(d, grow_cap, "repeated grow must hit tip_hole_grow_cap");
            // C1d warm default off — hot/cold both use grow_cap unless WARM=1.
            // C1n: without warm EWMA, effective == cold.
            let cold = tip_hole_grow_cap_for_peer(false);
            assert_eq!(cold, grow_cap);
            if tip_hole_warm_enabled() {
                let warm = tip_hole_grow_cap_for_peer(true);
                assert!(warm >= grow_cap);
                assert!(warm <= pipe_cap);
            } else {
                assert_eq!(tip_hole_grow_cap_for_peer(true), grow_cap);
            }
        } else {
            assert_eq!(tip_hole_grow_on_delivery(start), pipe_cap);
        }
    }

    #[test]
    fn c1n_gd_fast_elevates_cap_only_when_ewma_fast() {
        super::super::tip_stage::test_reset_getdata_body_ewma();
        // Pipe default 32 clamps FAST_CAP — bake uses PIPE=128.
        unsafe {
            std::env::set_var("BLVM_IBD_TIP_HOLE_PIPE", "128");
            std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_CAP", "32");
            std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_FAST_CAP", "48");
            std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_STEP", "8");
        }
        let cold = tip_hole_grow_cap();
        let fast = tip_hole_grow_fast_cap();
        assert!(fast > cold, "pipe must allow FAST_CAP > cold");
        // Cold samples → stay at cold.
        assert_eq!(tip_hole_grow_cap_effective(), cold);
        // Slow EWMA → stay at cold (C1m mute thrash guard).
        super::super::tip_stage::test_seed_getdata_body_ewma(3_000, 32);
        assert_eq!(tip_hole_grow_cap_effective(), cold);
        // Abs sticky cap stays at FAST_CAP even when EWMA is slow (C1s).
        assert_eq!(tip_hole_sticky_abs_cap(false), fast.max(cold));
        // Fast EWMA → elevate fill cap + step ≥16 (C1r, even if GROW_STEP baked to 8).
        super::super::tip_stage::test_seed_getdata_body_ewma(100, tip_hole_gd_fast_n());
        if tip_hole_gd_fast_enabled() && tip_hole_grow_enabled() {
            assert_eq!(tip_hole_grow_cap_effective(), fast);
            assert!(tip_hole_grow_step() >= 16, "C1r gd-fast step");
            let mut d = tip_hole_grow_start();
            for _ in 0..16 {
                d = tip_hole_grow_on_delivery(d);
            }
            assert_eq!(d, fast, "gd-fast grow must reach FAST_CAP");
        }
        super::super::tip_stage::test_reset_getdata_body_ewma();
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_HOLE_PIPE");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GROW_CAP");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GROW_FAST_CAP");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GROW_STEP");
        }
    }

    #[test]
    fn c1u_gd_slow_arms_and_clamps_fill_cap() {
        super::super::tip_stage::test_reset_getdata_body_ewma();
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_SLOW");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_SLOW_MS");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_SLOW_FILL_CAP");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_SLOW_RATCHET");
        }
        assert!(tip_hole_gd_slow_enabled());
        assert!(!tip_hole_gd_slow(), "no EWMA → not slow");
        // Below A6m gate → not slow.
        super::super::tip_stage::test_seed_getdata_body_ewma(400, tip_hole_gd_slow_n());
        assert!(!tip_hole_gd_slow());
        // Ignition cliff territory (5–8s) → slow; fill clamp = grow_start.
        super::super::tip_stage::test_seed_getdata_body_ewma(5_000, tip_hole_gd_slow_n());
        assert!(tip_hole_gd_slow());
        assert_eq!(tip_hole_slow_fill_cap(), tip_hole_grow_start());
        assert!(
            tip_hole_gd_slow_sole_keep(1),
            "sole ready + GD_SLOW arms sole floor path"
        );
        assert!(
            !tip_hole_gd_slow_sole_keep(2),
            "multi-ready still uses GD_SLOW shrink"
        );
        assert_eq!(tip_hole_sole_gd_slow_floor(), 16, "default sole floor");
        assert_eq!(
            tip_hole_gd_slow_next_depth(32),
            8,
            "multi-peer legacy cliff unchanged"
        );
        // Mid-band: release floor (cold deepen OK) but keep no-FAST (death spiral).
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_HOLE_SOLE_FLOOR_RECOVER_MS");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_FAST_MS");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_FAST_N");
        }
        assert_eq!(
            tip_hole_sole_floor_recover_ms(),
            tip_hole_gd_slow_ms(),
            "default floor recover = gd-slow (not gd-fast)"
        );
        super::super::tip_stage::test_reset_sole_floor_latch();
        super::super::tip_stage::note_sole_floor_latch();
        super::super::tip_stage::note_sole_no_fast_latch();
        super::super::tip_stage::test_seed_getdata_body_ewma(765, tip_hole_gd_slow_n().max(tip_hole_gd_fast_n()));
        assert!(
            !tip_hole_sole_floor_blocks_grow(),
            "765ms < gd-slow → floor clears; cold CAP may deepen"
        );
        assert!(
            tip_hole_sole_no_fast_active(),
            "765ms > gd-fast → no FAST_CAP"
        );
        assert_eq!(
            tip_hole_cap_for_sole(true, tip_hole_grow_fast_cap()),
            tip_hole_grow_cap(),
            "sole no-FAST clamps to cold grow_cap"
        );
        super::super::tip_stage::note_sole_floor_latch();
        super::super::tip_stage::note_sole_no_fast_latch();
        super::super::tip_stage::test_seed_getdata_body_ewma(359, tip_hole_gd_slow_n().max(tip_hole_gd_fast_n()));
        assert!(
            !tip_hole_sole_floor_blocks_grow(),
            "359ms mid-band: floor clear"
        );
        assert!(
            tip_hole_sole_no_fast_active(),
            "359ms still blocks FAST"
        );
        // Healthy blips inside min-hold must NOT clear no-FAST (tc152/tc153).
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_HOLE_SOLE_NO_FAST_CLEAR_N");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_SOLE_NO_FAST_MIN_HOLD_MS");
        }
        assert_eq!(tip_hole_sole_no_fast_clear_n(), 16);
        assert_eq!(tip_hole_sole_no_fast_min_hold_ms(), 120_000);
        super::super::tip_stage::test_seed_getdata_body_ewma(80, tip_hole_gd_slow_n().max(tip_hole_gd_fast_n()));
        assert!(!tip_hole_sole_floor_blocks_grow());
        for _ in 0..tip_hole_sole_no_fast_clear_n() {
            assert!(
                tip_hole_sole_no_fast_active(),
                "inside min-hold, streak must not clear no-FAST"
            );
        }
        assert!(super::super::tip_stage::sole_no_fast_latched());
        assert_eq!(
            tip_hole_cap_for_sole(true, tip_hole_grow_fast_cap()),
            tip_hole_grow_cap(),
            "still cold-capped inside hold"
        );
        // After hold expires + sustained streak → clear.
        super::super::tip_stage::test_set_sole_no_fast_armed_ms_ago(tip_hole_sole_no_fast_min_hold_ms() + 1);
        for _ in 0..tip_hole_sole_no_fast_clear_n() {
            let _ = tip_hole_sole_no_fast_active();
        }
        assert!(!tip_hole_sole_no_fast_active());
        assert!(!super::super::tip_stage::sole_no_fast_latched());
        assert!(!tip_hole_gd_slow());
        assert!(!tip_hole_gd_slow_sole_keep(1));
        assert_eq!(
            tip_hole_cap_for_sole(true, tip_hole_grow_fast_cap()),
            tip_hole_grow_fast_cap(),
            "after hold+streak clear, FAST_CAP allowed again"
        );
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::test_reset_sole_floor_latch();
    }

    #[test]
    fn c1u_prime_gd_slow_ratchets_not_cliff() {
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_HOLE_SLOW_FILL_CAP");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GROW_STEP");
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_SLOW_RATCHET");
        }
        assert!(!tip_hole_gd_slow_ratchet_enabled(), "default off after T172520Z");
        assert_eq!(tip_hole_gd_slow_next_depth(32), 8, "default = legacy cliff");
        unsafe {
            std::env::set_var("BLVM_IBD_TIP_HOLE_GD_SLOW_RATCHET", "1");
        }
        assert_eq!(tip_hole_gd_slow_next_depth(32), 24, "opt-in 32→24 one step");
        assert_eq!(tip_hole_gd_slow_next_depth(24), 16);
        assert_eq!(tip_hole_gd_slow_next_depth(16), 8);
        assert_eq!(tip_hole_gd_slow_next_depth(8), 8, "floor at slow fill cap");
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_SLOW_RATCHET");
        }
    }

    #[test]
    fn sole_floor_max_h_default_keeps_floor_everywhere() {
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_HOLE_SOLE_FLOOR_MAX_H");
        }
        assert_eq!(tip_hole_sole_floor_max_h(), 0, "default 0 = KEEP always-on");
        assert!(tip_hole_sole_floor_applies(400_300));
        assert!(tip_hole_sole_floor_applies(406_787));
    }

    #[test]
    fn sole_floor_max_h_405k_floors_cold_skips_dens() {
        unsafe {
            std::env::set_var("BLVM_IBD_TIP_HOLE_SOLE_FLOOR_MAX_H", "405000");
        }
        assert_eq!(tip_hole_sole_floor_max_h(), 405_000);
        assert!(
            tip_hole_sole_floor_applies(400_300),
            "cold READY still floors (do not repeat #8)"
        );
        assert!(
            !tip_hole_sole_floor_applies(405_000),
            "at cutoff skip 32→16"
        );
        super::super::tip_stage::test_reset_sole_floor_latch();
        super::super::tip_stage::note_sole_floor_latch();
        assert!(super::super::tip_stage::sole_floor_latched());
        assert!(
            !tip_hole_sole_floor_applies(406_787),
            "406787 dens death must not re-clamp"
        );
        assert!(
            !super::super::tip_stage::sole_floor_latched(),
            "crossing max_h clears leftover 400.3k latch"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_HOLE_SOLE_FLOOR_MAX_H");
        }
    }

    #[test]
    fn c1_tip_covering_fail_pipe_fill_mute_still_matches() {
        // Regress: C1 must not break mute string matching used for tip-role ban.
        assert!(tip_covering_fail_is_mute(
            "PIPE_FILL mute: no network body in 3000ms (chunk 304697-304824)"
        ));
    }

    #[test]
    fn w108_empty_bridge_ahead_buffered_allows_deep_soft() {
        let tip = 355_391;
        let tip_needed = tip + 1;
        let _g = tip_soft_atomic_lock();
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        super::super::tip_stage::mark_needed(tip_needed);
        super::super::tip_stage::test_backdate_awaiting_ms(1_000);
        assert_eq!(
            gap_soft_retry_budget_for_chunk_ex(
                tip_needed,
                tip,
                tip_needed,
                tip_needed + 127,
                false,
                false,
            ),
            2,
            "W147: empty deep soft=2"
        );
        assert_eq!(
            gap_soft_retry_budget_for_chunk_ex(
                tip_needed,
                tip,
                tip_needed,
                tip_needed + 127,
                true,
                false,
            ),
            2,
            "W147: empty deep soft=2 even with ahead_buf"
        );
        assert_eq!(
            gap_soft_retry_budget_for_chunk_ex(
                tip_needed,
                tip,
                tip_needed,
                tip_needed,
                true,
                false,
            ),
            2,
            "W146: empty (H,H) soft=2 even with ahead flag"
        );
        assert_eq!(
            gap_soft_retry_budget_for_chunk_ex(
                tip_needed,
                tip,
                tip_needed,
                tip_needed + 127,
                false,
                true,
            ),
            2,
            "W147: empty deep soft=2 (hot STREAM included)"
        );
        assert_eq!(
            gap_soft_retry_budget_for_chunk_ex(
                tip_needed,
                tip,
                tip_needed,
                tip_needed,
                false,
                true,
            ),
            2,
            "W146: hot tip-STREAM (H,H) empty → soft=2"
        );
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    }

    #[test]
    fn w114_hot_streamer_holey_tip_soft_budget() {
        let tip = 332_237;
        let tip_needed = tip + 1;
        let _g = tip_soft_atomic_lock();
        // Holey tip: pending > 0 (bridge ahead of tip hole); holes&lt;8 → soft=2/2.
        super::super::memory::BRIDGE_PENDING_COUNT.store(23, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
        assert_eq!(
            gap_soft_retry_budget_for_chunk_ex(
                tip_needed,
                tip,
                tip_needed,
                tip_needed + 127,
                false,
                false,
            ),
            2,
            "W152: deep holey tip soft=2 (holes&lt;8)"
        );
        assert_eq!(
            gap_soft_retry_budget_for_chunk_ex(
                tip_needed,
                tip,
                tip_needed,
                tip_needed + 127,
                false,
                true,
            ),
            2,
            "W152: hot tip-STREAM deep holey → soft=2"
        );
        assert_eq!(
            gap_soft_retry_budget_for_chunk_ex(
                tip_needed,
                tip,
                tip_needed,
                tip_needed,
                false,
                true,
            ),
            2,
            "W152: hot tip-STREAM (H,H) holey → soft=2"
        );
        assert_eq!(
            gap_soft_retry_budget_for_chunk_ex(
                tip_needed,
                tip,
                tip_needed,
                tip_needed,
                false,
                false,
            ),
            2,
            "W152: non-hot holey (H,H) soft=2"
        );
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    }

    #[test]
    fn w142_holey_hh_soft_one_empty_hh_stays_zero() {
        let tip = 315_110;
        let tip_needed = tip + 1;
        let _g = tip_soft_atomic_lock();
        // Holey: pending ahead of tip hole (W141 death shape).
        super::super::memory::BRIDGE_PENDING_COUNT.store(39, Ordering::Relaxed);
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
            2,
            "W152: holey (H,H) soft=2 (holes&lt;8)"
        );
        // Empty (H,H): pending=0 + gap_missing — W146 soft=2.
        super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
        assert_eq!(
            gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
            2,
            "W146: empty (H,H) soft=2"
        );
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    }

    #[test]
    fn w85_rebase_tip_cap_clock_clears_pre_roll_age() {
        let tip_needed = 322_054u64;
        let mut clock_h = None;
        let mut heights = HashSet::new();
        heights.insert(tip_needed);
        let mut started = HashMap::new();
        started.insert(tip_needed, Instant::now() - Duration::from_secs(8));
        assert!(
            rebase_tip_cap_clock(tip_needed, &mut clock_h, &heights, &mut started),
            "first tip-roll must rebase"
        );
        assert!(!tip_gap_inflight_exceeded(started[&tip_needed], 12));
        assert!(
            !rebase_tip_cap_clock(tip_needed, &mut clock_h, &heights, &mut started),
            "idempotent per tip height"
        );
        // Pre-roll age must not survive rebase.
        assert!(started[&tip_needed].elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn gap_timeout_for_chunk_tip_and_far() {
        let tip = 684_955;
        let tip_needed = tip + 1;
        assert_eq!(
            gap_timeout_for_chunk(tip_needed, tip_needed + 15, tip, 45),
            tip_gap_timeout_secs()
        );
        assert_eq!(
            gap_timeout_for_chunk(
                tip_needed + far_ahead_band() + 10,
                tip_needed + far_ahead_band() + 73,
                tip,
                45
            ),
            far_ahead_timeout_secs()
        );
        assert_eq!(
            gap_timeout_for_chunk(tip_needed + 32, tip_needed + 95, tip, 45),
            45,
            "mid-window keeps default"
        );
    }

    #[test]
    fn wan_deep_pipe_chunk_deadline_capped_on_wan_gap() {
        assert_eq!(
            super::wan_deep_tip_pipe_chunk_deadline_secs(700_001, 700_128, 700_000, 600),
            super::super::tip_stage::tip_sla_secs()
                .saturating_mul(2)
                .clamp(90, 180)
        );
        assert_eq!(
            super::wan_deep_tip_pipe_chunk_deadline_secs(100, 200, 0, 600),
            600,
            "non-WAN keeps default"
        );
    }

    #[test]
    fn wan_deep_pipe_timeout_tiers() {
        let tip = 710_000;
        let confirmed = 700_000;
        let tip_needed = tip + 1;
        assert_eq!(
            wan_deep_pipe_timeout_secs(tip_needed, tip, confirmed),
            Some(tip_gap_timeout_secs())
        );
        // W32d″: mid/deep ≥ tip soft-retry window (not 8/12).
        assert_eq!(
            wan_deep_pipe_timeout_secs(tip_needed + 1, tip, confirmed),
            Some(30)
        );
        assert_eq!(
            wan_deep_pipe_timeout_secs(tip_needed + 31, tip, confirmed),
            Some(30)
        );
        assert_eq!(
            wan_deep_pipe_timeout_secs(tip_needed + 32, tip, confirmed),
            Some(45)
        );
        assert!(wan_deep_pipe_timeout_secs(confirmed, tip, confirmed).is_none());
        assert!(wan_deep_pipe_timeout_secs(tip_needed, tip, 0).is_none());
    }

    #[test]
    fn chunk_outer_deadline_single_height_micro_chunk_gets_120s_minimum() {
        // (H,H) stall-recovery micro-chunks use 4× per-block timeout, ≥120s.
        // With default 30s per-block this gives 120s.
        assert_eq!(chunk_outer_deadline_secs(558211, 558211, 558211, 30), 120);
        // Larger per-block timeout scales: 4×40 = 160.
        assert_eq!(chunk_outer_deadline_secs(100, 100, 100, 40), 160);
        // Minimum floor is always 120 even with tiny per-block timeout.
        assert_eq!(chunk_outer_deadline_secs(1, 1, 1, 1), 120);
        // start == end == 0 (bootstrap sentinel) also gets 120.
        assert_eq!(chunk_outer_deadline_secs(0, 0, 0, 30), 120);
    }

    #[test]
    fn worker_chunk_outer_deadline_caps_wan_tip_pipe() {
        let confirmed = 714_450;
        let start = 714_451;
        let end = 714_578;
        let uncapped = chunk_outer_deadline_secs(start, end, start, 45);
        assert!(
            uncapped >= 5000,
            "uncapped WAN tip pipe should be huge (live 5760), got {uncapped}"
        );
        let capped = worker_chunk_outer_deadline_secs(start, end, start, 45, confirmed);
        assert!(
            capped <= 120,
            "WAN tip pipe worker outer must be capped, got {capped}"
        );
        assert!(
            capped >= 60,
            "WAN tip pipe worker outer floor 60s, got {capped}"
        );
        // Non-WAN chunk keeps full budget.
        assert_eq!(
            worker_chunk_outer_deadline_secs(100, 200, 100, 30, 0),
            chunk_outer_deadline_secs(100, 200, 100, 30)
        );
    }

    #[tokio::test]
    async fn wait_cooperative_outer_none_stays_pending() {
        let raced = tokio::select! {
            _ = wait_cooperative_outer(None) => "resolved",
            _ = tokio::time::sleep(Duration::from_millis(20)) => "timeout",
        };
        assert_eq!(raced, "timeout");
    }

    /// D0: completing a full pipe moves the permit into the worker frame. Acquiring another
    /// permit *before* dropping it deadlocks when capacity == in-flight count (WAN tip pipe).
    #[tokio::test(flavor = "current_thread")]
    async fn completed_permit_must_drop_before_refill_or_deadlock() {
        let capacity = 4usize;
        let sem = Arc::new(Semaphore::new(capacity));
        let mut in_flight_permits = Vec::new();
        for _ in 0..capacity {
            in_flight_permits.push(sem.clone().acquire_owned().await.unwrap());
        }
        assert_eq!(sem.available_permits(), 0);

        // Simulate in_flight.next() completing one future — permit lands in the stack frame.
        let completed = in_flight_permits.pop().unwrap();
        assert_eq!(sem.available_permits(), 0, "permit still held by completed binding");

        // Bug path: acquire while `completed` lives → hang (this is what tip pipes hit).
        let blocked = tokio::time::timeout(
            Duration::from_millis(50),
            sem.clone().acquire_owned(),
        )
        .await;
        assert!(
            blocked.is_err(),
            "acquire while completed permit held must not succeed (would not be a deadlock otherwise)"
        );

        // Fix path (D0): drop before refill.
        drop(completed);
        let got = tokio::time::timeout(Duration::from_millis(200), sem.clone().acquire_owned())
            .await
            .expect("acquire after drop must complete")
            .expect("semaphore open");
        drop(got);
        drop(in_flight_permits);
    }

    /// D1: try_take returns None when full instead of awaiting.
    #[test]
    fn try_take_blocks_permit_returns_none_when_full() {
        let sem = Arc::new(Semaphore::new(1));
        let held = sem.clone().try_acquire_owned().unwrap();
        let blocks_sem = Some(sem);
        assert!(
            try_take_blocks_permit(&blocks_sem).unwrap().is_none(),
            "full semaphore must report None (stop fill, return to select)"
        );
        drop(held);
        assert!(
            try_take_blocks_permit(&blocks_sem).unwrap().is_some(),
            "after release, take must succeed"
        );
    }

    #[test]
    fn try_take_blocks_permit_none_sem_is_unbounded() {
        assert!(matches!(
            try_take_blocks_permit(&None).unwrap(),
            Some(None)
        ));
    }

    #[test]
    fn w102_sync_next_to_send_after_gap_stream_advances_past_dedup() {
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(100, Ordering::Relaxed);
        let mut next = 100u64;
        sync_next_to_send_after_gap_stream(&mut next, 200);
        assert_eq!(next, 101, "cursor must pass streamed tip so tip-hole pipe disarms");
        // Already ahead of dedup — no-op.
        next = 150;
        sync_next_to_send_after_gap_stream(&mut next, 200);
        assert_eq!(next, 150);
        // Drain advanced dedup past tip+N (gap not missing — healthy W102 disarm).
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(120, Ordering::Relaxed);
        next = 100;
        sync_next_to_send_after_gap_stream(&mut next, 200);
        assert_eq!(next, 121);
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
    }

    #[test]
    fn a31_sync_always_jumps_to_dedup_plus_one_even_when_gap_missing() {
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(409671, Ordering::Relaxed);
        let mut next = 409600u64;
        sync_next_to_send_after_gap_stream(&mut next, 409727);
        assert_eq!(
            next, 409672,
            "a31 always advances next_to_send to DEDUP+1"
        );
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
    }

    #[test]
    fn land_e_rewind_dedup_over_unbuffered_hole() {
        super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(409671, Ordering::Relaxed);
        let got = rewind_gap_stream_dedup_over_missing_hole(409600, 409671);
        assert_eq!(got, Some(409599));
        assert_eq!(
            super::super::memory::GAP_STREAM_DEDUP_HEIGHT.load(Ordering::Relaxed),
            409599
        );
        // Already behind the gap — no-op.
        assert!(rewind_gap_stream_dedup_over_missing_hole(409600, 409500).is_none());
        super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(409671, Ordering::Relaxed);
        assert!(
            rewind_gap_stream_dedup_over_missing_hole(409600, 409671).is_none(),
            "must not rewind when the gap is not missing"
        );
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
    }

    #[test]
    fn h5_received_clone_keeps_tip_keyed() {
        use blvm_protocol::{BlockHeader, Transaction, TransactionOutput};
        let block = Arc::new(Block {
            header: BlockHeader {
                version: 1,
                timestamp: 1,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        });
        let w: SharedWitnesses = Arc::new(vec![]);
        let mut received = BTreeMap::new();
        received_put(&mut received, 300_288, (Arc::clone(&block), Arc::clone(&w)));
        let cloned = received_clone(&received, 300_288).expect("clone");
        assert!(received.contains_key(&300_288), "clone must not remove tip");
        assert!(Arc::ptr_eq(&cloned.0, &block));
        let _taken = received_take(&mut received, 300_288).expect("take");
        assert!(received_clone(&received, 300_288).is_none());
    }
}
