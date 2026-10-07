//! Parallel Initial Block Download (IBD)
//!
//! Implements parallel block downloading from multiple peers during initial sync.
//! This significantly speeds up IBD by downloading blocks concurrently from different peers.
//!
//! ## Header Sync Optimization
//!
//! Uses hardcoded checkpoints to parallelize header download:
//! - Headers are downloaded in parallel for ranges between checkpoints
//! - Each range uses the checkpoint hash as its starting locator
//! - Verification ensures continuity and checkpoint hash matching

mod blocks;
mod checkpoints;
mod chunk_assigner;
mod download;
mod env_latch;
pub(crate) use env_latch::latch_env;
pub(crate) mod body_dup;
mod feeder;
mod feeder_miss;
mod headers;
#[cfg(feature = "production")]
mod ibd_staging;
pub(crate) mod late_body;
pub mod local_block;
mod memory;
mod ms_breakdown;
mod muhash_fold;
pub(crate) mod persist_lane;
pub(crate) mod wire_hash_gate;
pub(crate) fn note_block_parse(ms: u64, offload: bool) {
    ms_breakdown::note_block_parse(ms, offload);
}
pub(crate) fn note_peer_rx_depth(depth: usize) {
    ms_breakdown::note_peer_rx_depth(depth);
}
mod policy;
mod prefetch;
#[cfg(feature = "production")]
mod retire_dispatcher;
#[cfg(any(feature = "ibd-dev", test))]
mod synthetic_wan;
#[cfg(not(any(feature = "ibd-dev", test)))]
#[path = "synthetic_wan_stub.rs"]
mod synthetic_wan;
mod tip_probe;
mod tip_release;
pub(crate) mod tip_stage;
mod types;
#[cfg(feature = "production")]
mod validation_loop;

#[cfg(feature = "production")]
pub(crate) use memory::maybe_purge_jemalloc_retained;
#[cfg(feature = "production")]
pub(crate) use validation_loop::IbdRetireWork;

use chunk_assigner::{ChunkAssigner, ChunkGuard, create_chunks as create_chunks_impl};

pub use chunk_assigner::BlockChunk;
use download::{download_chunk, is_snapshot_sourced_peer, local_disk_peer_ids};
use feeder::{new_feeder_state, run_feeder_thread};
use local_block::{
    body_warehouse_enabled, contiguous_body_range_tip, coordinator_inject_local_gap,
    flush_path_body_tip, ibd_local_gap_fill_enabled, ibd_local_gap_fill_max_height,
    probe_confirmed_body_height, probe_highest_stored_body_height,
};
use memory::{IbdTuningContext, MemoryGuard};
#[cfg(feature = "production")]
use types::PrefetchWorkItemV2;
use types::{ReadyItem, SharedBlock, SharedWitnesses};

use crate::network::NetworkManager;
use crate::network::peer_scoring::is_lan_peer;
use crate::storage::Storage;
use crate::storage::blockstore::{BlockMetadata, BlockStore, block_height_row_key};
use crate::storage::database::IBD_UTXO_STORE_SUBDIR;
use crate::storage::disk_utxo::{
    OutPointKey, block_input_keys_and_tx_ids_filtered, compute_tx_ids_only,
};
#[cfg(feature = "production")]
use crate::storage::ibd_utxo_store::IbdUtxoStore;
use crate::utils::{IBD_YIELD_SLEEP, MESSAGE_PROCESSOR_POLL_SLEEP};
use anyhow::{Context, Result};
use blvm_protocol::bip_validation::Bip30Index;
use blvm_protocol::{
    BitcoinProtocolEngine, Block, BlockHeader, Hash, UtxoSet, ValidationResult, segwit::Witness,
};

use crossbeam_channel;
/// Set to `true` by the process-level signal handler when SIGTERM/SIGINT is received.
///
/// The IBD validation loop polls this on its feeder condvar timeout (every 5 s) and, when
/// set, marks the feeder as done so the pipeline drains cleanly and the watermark checkpoint
/// is flushed before returning.  The main binary waits for `node.start()` to complete rather
/// than dropping it, so the join of the validation thread and the final `persist_ibd_utxo_flush_checkpoint`
/// call both execute before the process exits.
pub static IBD_SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// True while `ParallelIBD::sync_parallel` is running (startup IBD or run-loop catch-up).
pub static PARALLEL_IBD_SESSION_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Monotonic session id; coordinator tasks exit when this no longer matches their id.
pub(crate) static PARALLEL_IBD_SESSION_ID: AtomicU64 = AtomicU64::new(0);

/// Feeder buffer block count (updated by validation loop) for export start gating.
pub(crate) static IBD_FEEDER_BUFFER_BLOCKS: AtomicUsize = AtomicUsize::new(0);
/// N13: validation publishes the tip height it is waiting for. Inserts of other
/// heights skip Condvar notify (ahead flush storms); tip insert / close still wake.
pub(crate) static IBD_FEEDER_WAIT_TIP: AtomicU64 = AtomicU64::new(0);
/// Feeder buffer cap at IBD start (updated by validation loop).
pub(crate) static IBD_FEEDER_BUFFER_CAP: AtomicUsize = AtomicUsize::new(0);
/// W75: coordinator mirrors tip-gap distress for export start gating.
pub(crate) static IBD_TIP_GAP_MISSING: AtomicBool = AtomicBool::new(false);
/// Coordinator: `tip_runway_mode == EMPTY_TIP` (no tip body, no ahead). C1t may
/// race a mute cover without PIPE_FILL recv0 — genesis dest never logs PIPE_FILL
/// while `confirmed_body_height==0` (prune-window persist skip).
pub(crate) static IBD_EMPTY_TIP: AtomicBool = AtomicBool::new(false);
/// C1i: contiguous bodies in reorder from `next_needed` (0 ⇒ tip hole). Assigner freezes
/// past-tip stripes until this reaches the min runway (default tip stripe / grow start).
pub(crate) static IBD_TIP_CONTIG_RUNWAY: AtomicU64 = AtomicU64::new(0);
/// TIP_HOLE_AHEAD: bodies in reorder strictly above next_needed (not contig from tip).
/// Download mute CAP uses 5s hole_cap instead of empty-deep holey 16s — 16s mute is
/// why fast cells die at 406–411 (soak 13 tip90≈93 vs soak 5 tip90≈165).
pub(crate) static IBD_REORDER_AHEAD: AtomicUsize = AtomicUsize::new(0);
/// W89: coordinator mirrors bridge tip-band holes for tip-hole CAP (download path).
pub(crate) static IBD_TIP_BRIDGE_HOLES: AtomicU64 = AtomicU64::new(0);
/// Phase 2 EMPTY_TIP attribution: tip peers covering `next_needed` (healthy claims).
pub(crate) static IBD_TIP_COVERING: AtomicUsize = AtomicUsize::new(0);
/// Tip-band in-flight assign ranges (from `tip_flight_diag`).
pub(crate) static IBD_TIP_IN_FLIGHT_RANGES: AtomicUsize = AtomicUsize::new(0);
/// Tip height present in reorder buffer (not yet feeder).
pub(crate) static IBD_TIP_IN_REORDER: AtomicBool = AtomicBool::new(false);
/// First height ≥ `next_needed` with no body (reorder ∪ feeder ∪ tip_taken).
/// Not inflight. Not a reserved stripe. Not a stale contig gauge.
pub(crate) static IBD_FIRST_HOLE: AtomicU64 = AtomicU64::new(0);
/// `next_needed` at the poll that published [`IBD_FIRST_HOLE`]. Stale hole is ignored.
pub(crate) static IBD_FIRST_HOLE_AT: AtomicU64 = AtomicU64::new(0);
/// L1 leapfrog: exclusive farm ranges. Admit/evict bypass these heights only.
/// Unreserved far heights still drop (B2 / W28d). Published by the assigner.
static IBD_LOOKAHEAD_RESERVED: std::sync::Mutex<Vec<(u64, u64)>> =
    std::sync::Mutex::new(Vec::new());

pub(crate) fn publish_lookahead_reserved(ranges: Vec<(u64, u64)>) {
    *IBD_LOOKAHEAD_RESERVED
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = ranges;
}

pub(crate) fn lookahead_height_reserved(h: u64) -> bool {
    lookahead_reserved_stripe_containing(h).is_some()
}

/// Reserved `(s,e)` that contains `h` (live stripe ∪ R-100 hold). None if unreserved.
fn lookahead_reserved_stripe_containing(h: u64) -> Option<(u64, u64)> {
    IBD_LOOKAHEAD_RESERVED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .find(|&(s, e)| s <= h && h <= e)
}

/// L2b: apply is inside a reserved stripe — emit the rest of **that** stripe only.
fn lookahead_same_reserved_stripe(next_needed: u64, h: u64) -> bool {
    match lookahead_reserved_stripe_containing(next_needed) {
        Some((s, e)) => s <= h && h <= e,
        None => false,
    }
}

#[cfg(test)]
pub(crate) fn test_clear_lookahead_reserved() {
    IBD_LOOKAHEAD_RESERVED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}
/// W176: piggyback checkpoint export in flight — tip CAP lengthens (disk contention
/// stretches getdata→body; live W175 p90≈7s / max≈20s while export wall ran).
/// E1 (`BLVM_IBD_EXPORT_ISOLATION=1`): also pauses new chunk assign + validation
/// dispatch so export owns the nvme (ladder snap collection).
pub(crate) static IBD_CHECKPOINT_EXPORT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Ladder / matched-snap collection: pause peer GetData + validation while a
/// checkpoint export holds the disk. Off by default (tip60 soaks want crawl+export
/// overlap). Enable via `BLVM_IBD_EXPORT_ISOLATION=1` (alias: `…_LADDER_EXPORT_ISOLATION`).
pub(crate) fn export_isolation_enabled() -> bool {
    for key in [
        "BLVM_IBD_EXPORT_ISOLATION",
        "BLVM_IBD_LADDER_EXPORT_ISOLATION",
    ] {
        if let Ok(v) = std::env::var(key) {
            let t = v.trim();
            if t == "1"
                || t.eq_ignore_ascii_case("true")
                || t.eq_ignore_ascii_case("yes")
                || t.eq_ignore_ascii_case("on")
            {
                return true;
            }
            if t == "0"
                || t.eq_ignore_ascii_case("false")
                || t.eq_ignore_ascii_case("no")
                || t.eq_ignore_ascii_case("off")
            {
                return false;
            }
        }
    }
    false
}

/// True when export is in flight **and** isolation mode is enabled.
#[inline]
pub(crate) fn export_isolation_active() -> bool {
    export_isolation_enabled() && IBD_CHECKPOINT_EXPORT_ACTIVE.load(Ordering::Relaxed)
}

pub(crate) struct ExportOwnerHoldSnap {
    pub peer: String,
    pub grown: usize,
}

pub(crate) static EXPORT_OWNER_HOLD: Mutex<Option<ExportOwnerHoldSnap>> = Mutex::new(None);
pub(crate) static EXPORT_OWNER_HOLD_ARMED: AtomicBool = AtomicBool::new(false);

/// Checkpoint export must not MUTE_KILL / LIMITED / trial the KEEP owner
/// (dest-bc 91s compact without isolation; dest-an 96s isolation).
pub(crate) fn export_owner_hold_protects(peer: &str) -> bool {
    if !IBD_CHECKPOINT_EXPORT_ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    match EXPORT_OWNER_HOLD.lock() {
        Ok(g) => g.as_ref().is_some_and(|s| s.peer == peer),
        Err(_) => false,
    }
}

pub(crate) fn export_owner_hold_clear() {
    EXPORT_OWNER_HOLD_ARMED.store(false, Ordering::Release);
    if let Ok(mut g) = EXPORT_OWNER_HOLD.lock() {
        *g = None;
    }
}
/// W177: `next_needed ≤ body_tip` (soft-resume / local inject). Export during this
/// window fights DiskIndex compact + MemoryHigh (`high_ev` tens of thousands) and
/// the BPS>200 defer is knife-edge (live W176 started export @179.8 BPS at h=372134).
pub(crate) static IBD_LOCAL_BODY_AHEAD: AtomicBool = AtomicBool::new(false);
/// W34d: target feeder depth when reorder has tip+ahead on WAN crawl.
const W34_FEEDER_PREFETCH_TARGET: usize = 16;

/// W34h: true when reorder has any height in `[next_needed, next_needed + target)`.
fn reorder_has_feeder_prefetch_band(
    reorder_buffer: &BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    next_needed: u64,
    target: usize,
) -> bool {
    let band_end = next_needed.saturating_add(target as u64);
    reorder_buffer.range(next_needed..band_end).next().is_some()
}

/// W34h: WAN tip crawl with feeder below prefetch target.
#[inline]
fn wan_feeder_prefetch_starved(wan_tip_crawl: bool, feeder_len: usize) -> bool {
    wan_tip_crawl && feeder_len < W34_FEEDER_PREFETCH_TARGET
}

/// W75/W78: tip body is handoff-ready (bridge pending at cursor, or tip in feeder).
///
/// `bridge_next == tip` alone must **not** count — that is the hole the download path
/// still needs to fill (live freeze at 344348: pending=0, bridge_next=tip).
///
/// **W78:** `feeder_len > 0` is **not** enough — live freeze at 381335 had `feeder=46`
/// with `gap_missing=true` / `bridge_next=381396` (cursor 61 ahead). Treating any feeder
/// occupancy as in-pipeline tight-looped `yield`/`continue` and skipped Case C / TIP_REWIND
/// for 20+ min.
#[inline]
pub(crate) fn tip_gap_body_in_pipeline(
    tip_in_bridge_pending_at_cursor: bool,
    tip_in_feeder: bool,
) -> bool {
    tip_in_bridge_pending_at_cursor || tip_in_feeder
}

/// Contiguous bodies in `reorder` starting at `next_needed` (0 ⇒ tip hole).
pub(crate) fn reorder_contig_runway(
    reorder: &std::collections::BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    next_needed: u64,
) -> u64 {
    have_contig_runway(reorder, |_| false, next_needed)
}

/// Contiguous **have** from `next_needed` (reorder ∪ feeder). Inflight / reserved
/// stripes are not have. L2b moves the warehouse into the feeder; walking reorder
/// alone left `FIRST_HOLE` at H+1 and the hero GetData'd the sprint (R-102 max skip
/// **128**, desert 13–20k **151** after a 34k in-stripe burst).
pub(crate) fn have_contig_runway(
    reorder: &std::collections::BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    feeder_has: impl Fn(u64) -> bool,
    next_needed: u64,
) -> u64 {
    let mut n = 0u64;
    let mut h = next_needed;
    while reorder.contains_key(&h) || feeder_has(h) {
        n = n.saturating_add(1);
        h = h.saturating_add(1);
        if n >= 4096 {
            break;
        }
    }
    n
}

/// Heights in reorder strictly above `next_needed` (ahead of tip cursor).
pub(crate) fn reorder_ahead_buffered(
    reorder: &std::collections::BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    next_needed: u64,
) -> usize {
    reorder.range((next_needed.saturating_add(1))..).count()
}

/// Lowest ahead height in reorder, if any.
pub(crate) fn reorder_first_ahead(
    reorder: &std::collections::BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    next_needed: u64,
) -> Option<u64> {
    reorder
        .range((next_needed.saturating_add(1))..)
        .next()
        .map(|(h, _)| *h)
}

/// True when P1-A TIP_NUDGE may fire: tip body is not in reorder/feeder/bridge and
/// validation has not already taken it. `!reorder.contains(tip)` alone is **not** a gap —
/// tip_taken / feeder / bridge-pending are the healthy handoff shape.
pub(crate) fn tip_nudge_true_body_gap(
    tip_in_reorder: bool,
    tip_in_feeder: bool,
    tip_in_bridge: bool,
    tip_taken: bool,
) -> bool {
    !tip_in_reorder && !tip_in_feeder && !tip_in_bridge && !tip_taken
}

/// Live WAN body tip for assigner crawl.
///
/// `probe_confirmed_body_height` is a monotonic binary search. After incremental
/// prune + partial re-download it lands on the *highest* leftover body and skips
/// holes (live: confirmed=185817, first hole at 70713). Promoting that cheese
/// makes `wan_tip_gap_crawl` false, so GetData never arms. Use the contiguous
/// walk from height 1 (or confirmed if lower), never sparse-max.
pub(crate) fn wan_live_body_tip(confirmed: u64, contiguous: u64) -> u64 {
    // Contiguous range is the instrument. `confirmed==0` used to zero the tip
    // (empty spawn probe + any store that does not start at 1) and freeze CRAWL
    // for the whole run. Cheese stays excluded: walk stops at the first hole,
    // so min(confirmed, contiguous) still wins when both are nonzero.
    if contiguous == 0 {
        return 0;
    }
    if confirmed == 0 {
        return contiguous;
    }
    contiguous.min(confirmed)
}

/// Drop warehouse tip to just below a proven disk miss so GetData owns the hole.
pub(crate) fn pull_wan_body_tip_for_hole(live_tip: u64, next_needed: u64) -> u64 {
    if next_needed == 0 {
        return live_tip;
    }
    if live_tip >= next_needed {
        next_needed.saturating_sub(1)
    } else {
        live_tip
    }
}

/// `LOCAL_GAP_FILL=0` makes inject always return false — that is not a disk miss.
/// Pulling the warehouse tip there (live: 70669→20000) turns the soak into
/// wan_gap + (H,H) FORCE and collapses midchain from ~400 BPS to ~50.
pub(crate) fn should_pull_wan_body_tip_on_inject(
    local_fill_enabled: bool,
    injected: bool,
    live_tip: u64,
    next_needed: u64,
) -> bool {
    local_fill_enabled && !injected && live_tip >= next_needed && next_needed > 0
}

/// Leftover-disk hole: next is at or under warehouse cheese **and** recovery armed.
///
/// Do **not** key this on spawn `tip_gap_missing` (true from height 1) — that assigned
/// GetData `(1,1)` on every worker (live 2026-08-21T12:16Z). Arm only after validation
/// stall or leftover inject-miss. `covering` leftover stripes are not a feeder pipeline.
pub(crate) fn leftover_hole_needs_getdata(
    leftover_force: bool,
    next_needed: u64,
    body_tip: u64,
    covering: usize,
) -> bool {
    let _ = covering;
    leftover_force
        && next_needed > 0
        && if body_tip == 0 {
            // Genesis TRUE WAN: leftover_force is stall-armed. Do not keep (H,H)
            // after the tip lands (that would tip-serialize the rest of IBD).
            IBD_TIP_GAP_MISSING.load(Ordering::Relaxed)
        } else {
            next_needed <= body_tip.saturating_add(1)
        }
}

/// Cursor-ahead with no feeder, no GetData of H, not taken by validation.
///
/// R-256 300–340k: `bridge_next=H+1 ∧ feeder=0` **76%**, `HANDOFF_MISS` **0**. W22 treated
/// that as delivered and never hit COVERING_ZERO. `flight_tip≥1` is seated GetData —
/// always-rewind of that is W26b **0.7** (R-235 fat drip).
pub(crate) fn leftover_w22_cursor_lie(
    next_needed: u64,
    bridge_next: Option<u64>,
    tip_in_feeder: bool,
    tip_taken: bool,
    flight_tip: usize,
) -> bool {
    // R-245 restore: W22 always delivered. R-257–R-261 dested the lie and
    // either missed fat (248k) or killed dump (180k / occupancy rematch).
    // Step 2 leftover improve is leftover_inject_should_feeder, not this.
    let _ = (
        next_needed,
        bridge_next,
        tip_in_feeder,
        tip_taken,
        flight_tip,
    );
    false
}

/// Cursor jumped more than one height past validation and the tip is not in
/// the feeder or the bridge. That is a lost emit. One-ahead stays the W26b
/// pipeline case and is not lost.
pub(crate) fn cursor_far_ahead_tip_lost(
    next_needed: u64,
    bridge_next: u64,
    tip_in_feeder: bool,
    tip_in_pending: bool,
    tip_taken: bool,
) -> bool {
    bridge_next > next_needed.saturating_add(1) && !tip_in_feeder && !tip_in_pending && !tip_taken
}

/// W22: `bridge_next > H` is delivered unless leftover_w22_cursor_lie.
pub(crate) fn leftover_w22_cursor_is_delivered(
    next_needed: u64,
    bridge_next: Option<u64>,
    tip_in_feeder: bool,
    tip_taken: bool,
    flight_tip: usize,
) -> bool {
    let cursor_ahead = bridge_next.is_some_and(|n| n > next_needed);
    if !cursor_ahead {
        return false;
    }
    !leftover_w22_cursor_lie(
        next_needed,
        bridge_next,
        tip_in_feeder,
        tip_taken,
        flight_tip,
    )
}

/// leftover_TRACE window: leftover_force **or** next is in the last leftover
/// 512 heights through leftover tip+1. Do not use `next ≤ body_tip+1` alone —
/// that is true from height 1 when spawn tip is 70735 and floods 1→70k.
pub(crate) fn leftover_trace_watch(leftover_force: bool, next: u64, body_tip: u64) -> bool {
    leftover_force
        || (body_tip > 0
            && next > 0
            && next > body_tip.saturating_sub(512)
            && next <= body_tip.saturating_add(1))
}

/// Leftover-force must abort an in-flight leftover stripe even when the stall
/// height is **outside** the stripe.
///
/// Live 2026-08-21 stay-in-band: worker took `70625–70735`, leftover_stall
/// sent stall=70736. First-block abort required `stall ∈ [start,end]`, so
/// leftover_cheese never ran. Worker sat 4h; leftover_hole `(70736,70736)`
/// never got a free worker. Do **not** abort the WAN handoff chunk
/// (`start > body_tip`).
pub(crate) fn leftover_force_aborts_inflight_stripe(
    leftover_force: bool,
    body_tip: u64,
    chunk_start: u64,
    stall_or_need: u64,
) -> bool {
    leftover_force
        && body_tip > 0
        && chunk_start > 0
        && stall_or_need > 0
        && chunk_start <= body_tip
        && stall_or_need <= body_tip.saturating_add(1)
}

/// Live WAN assigner for TCP-down (Weak so IBD end cannot leak).
static LIVE_ASSIGNER: std::sync::OnceLock<
    std::sync::Mutex<Option<std::sync::Weak<chunk_assigner::ChunkAssigner>>>,
> = std::sync::OnceLock::new();

fn live_assigner_slot()
-> &'static std::sync::Mutex<Option<std::sync::Weak<chunk_assigner::ChunkAssigner>>> {
    LIVE_ASSIGNER.get_or_init(|| std::sync::Mutex::new(None))
}

pub(crate) fn register_live_assigner(assigner: &Arc<chunk_assigner::ChunkAssigner>) {
    *live_assigner_slot().lock().unwrap() = Some(Arc::downgrade(assigner));
}

pub(crate) fn clear_live_assigner() {
    *live_assigner_slot().lock().unwrap() = None;
}

/// Header tip the coordinator last published. `0` before the assigner is registered.
pub(crate) fn live_header_tip() -> u64 {
    live_assigner_slot()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|w| w.upgrade()))
        .map(|assigner| assigner.header_tip())
        .unwrap_or(0)
}

/// TCP down: drop assigner inflight and sticky state for this peer.
pub(crate) fn ibd_peer_gone(peer: &str) {
    let Some(assigner) = live_assigner_slot()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|w| w.upgrade()))
    else {
        return;
    };
    assigner.note_peer_tcp_gone(peer);
}

/// Do **not** clear leftover_force after a leftover-disk inject once validation
/// is already at leftover tip+1 (WAN handoff).
///
/// Live 2026-08-21 abort-stripe: leftover_stall armed leftover_force at 70736,
/// coordinator `bridge_next` stayed 70657, leftover inject succeeded there and
/// `set_leftover_force_getdata(false)` every loop. leftover_cheese abort and
/// leftover_hole GetData both saw leftover_force off. Worker sat on 70613–70735.
pub(crate) fn leftover_force_survives_disk_inject(
    leftover_force: bool,
    validation_next: u64,
    body_tip: u64,
) -> bool {
    leftover_force && body_tip > 0 && validation_next > body_tip
}

/// WAN handoff (`next > leftover body_tip`) has no leftover disk body.
/// leftover_stall_try_load must not run — live 2026-08-22 leftover_HANDOFF at
/// 70736 then hung on heed3 (header present, body missing) so stall_tx never
/// fired and leftover workers never aborted into leftover_hole GetData.
pub(crate) fn leftover_stall_skips_disk_load(next: u64, body_tip: u64) -> bool {
    next > 0 && next > body_tip
}

/// Stage 1: validation-stall leftover_force GetData only when the store does
/// not already hold the body. Assignment-side FORCE / covering=0 is unchanged.
pub(crate) fn stall_may_arm_leftover_force(store_has_body: bool) -> bool {
    !store_has_body
}

/// Reload H from disk this coordinator pass (leftover inject + Case B).
///
/// Classic leftover: `next` is under a published warehouse tip. Genesis TRUE WAN
/// after R-197: persist writes bodies but does **not** publish `FLUSH_PATH_TIP`,
/// so `live_body_tip` stays 0. R-223 `IBD: 1` was `store=1 feeder=0 reorder_has=0`
/// and leftover inject never ran because of that tip gate. `store_has` is the
/// genesis analog. Do not enter on an empty store — that would leftover_FORCE
/// `(H,H)` from height 1.
pub(crate) fn leftover_disk_hole_should_inject(
    live_body_tip: u64,
    next_needed: u64,
    store_has: bool,
    in_reorder: bool,
    in_bridge: bool,
    in_feeder: bool,
) -> bool {
    if next_needed == 0 || in_reorder || in_bridge || in_feeder {
        return false;
    }
    if live_body_tip > 0 && next_needed <= live_body_tip {
        return true;
    }
    store_has
}

/// Backoff before a worker may take work again after an all-local chunk (R-273 spin).
///
/// The chunk map never retires a range; `in_flight_per_peer` only excludes it while a
/// peer holds it. A chunk whose bodies are all on disk therefore completes in
/// microseconds and is immediately re-assignable. R-273 h=137000→138000: 8 peers took
/// the same 58 ranges **185,683 times in 60s** (3082/s, ~15k repeats per range) while
/// validation sat at 137154. Every take re-runs `try_load_local_ibd_block` per height —
/// a heed3 read, a full wire deser, and a double-SHA256 re-hash — and holds the block
/// permits the tip needs. R-258 is the terminal case: 7h20m stuck at 22k, 9.8 GB of log,
/// 99.8% this loop.
///
/// A range covering `next_needed` is never delayed: the tip must stay re-takeable.
///
/// R-274 confirmed takes/log fall when paired with re-race. R-275 dested backoff
/// **alone** (re-race default 0) and **FAILED**: 1→370k **5493s** vs R-273 **1580s**.
/// Default is now **0** (R-273 DNA). Do not try a smaller positive backoff.
///
/// **Why it failed — the re-take loop is load-bearing.** It is not waste, it is the local
/// body delivery pump: taking a chunk runs `try_load_local_ibd_block` per height and
/// pushes whatever is already on disk into `in_flight` → coordinator → feeder. Re-taking
/// is how the trailing edge drains (R-273: **62,131** heights, 16.8% of the chain, were
/// missing when validation reached them). Throttling the pump starves validation.
///
/// Exempting only the range *containing* `next_needed` was not enough. Chunks are **16**
/// wide, so at ~90 BPS the tip crosses into the next range every ~178 ms — shorter than
/// the 250 ms sleep, so the range the tip was about to need had just been put to sleep.
/// The sleep shows up directly as tip-stage latency: p90 **22 → 58 → 123 ms** across
/// R-273/R-274/R-275, staging total **849 → 2038 → 3338s**. The UTXO hotpath was
/// identical in all three (340–370k: 439/478/430 preads, 12.8/14.5/13.7 MB) — this was
/// never storage, it was supply.
///
/// Any future variant must exempt a **window ahead** of the tip, not a single range, and
/// back off only far-ahead spans. Untested.
pub(crate) fn all_local_retake_backoff_ms(
    net_block_count: usize,
    start: u64,
    end: u64,
    next_needed: u64,
) -> u64 {
    if net_block_count > 0 {
        return 0;
    }
    if start <= next_needed && next_needed <= end {
        return 0;
    }
    all_local_retake_backoff_base_ms()
}

/// Tunable for [`all_local_retake_backoff_ms`]. Default **0** after R-275 FAIL.
pub(crate) fn all_local_retake_backoff_base_ms() -> u64 {
    std::env::var("BLVM_IBD_RETAKE_BACKOFF_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Re-race the tip GetData when the seated cover has gone quiet (R-273 staging tail).
///
/// R-273: **849s of 1577s wall (54%)** is the validation loop waiting for one tip body,
/// and it is a tail, not a mean — p50 **0 ms**, p90 **22 ms**, p99 **205 ms**, but 579
/// stages cost 452s (29% of the whole run) and one block (h=137509) cost 31.6s alone.
///
/// The existing soft-retry only arms on the tip-gap CAP (5–16s), so it fired **8 times
/// in 57,961 stages** and on 5 of the 167 stages over 500 ms. Everything between 200 ms
/// and 5s waits out a quiet peer with no second request.
///
/// Unlike `hole_any_uncovered_ok`, this is the *covered* case: H has a seated GetData
/// that has gone quiet. `max_covering` keeps it to one extra racer.
///
/// **R-274 FAILED this at 300 ms — default is now 0 (off).** 1→370k took **3350s** vs
/// R-273 **1580s**, with a clean dose-response against re-race density: 248–300k 0.39×
/// (342 re-races), 340–370k 0.33× (405), while 1–10k ran 6.57× (2) and 120–180k 1.27×
/// (16). A second cover on H breaks the one-WAN-tip-fetcher invariant this assigner is
/// built on (`w31_wan_gap_max_fetchers_one_even_when_failover_armed`; the W18 note in
/// `get_work` — racing the same tip is not useful, pipeline deep on one peer). It flips
/// `covering_next_count` to 2, which suppresses `hole_any_uncovered_ok` and the other
/// uncovered-tip repair paths, and churns the assigner: lock acquisitions 16.3M → 61.2M,
/// avg wait 12.8 → 19.7 µs. It did not even pay locally — re-raced heights resolved at
/// p50 407 ms (77 still over 1s) while total staging rose 849s → 2038s and staging events
/// rose 41% (57,961 → 81,817).
///
/// Kept behind the env so a much higher threshold can be tried cheaply: the >2s tail is
/// only 40 events / 129s on R-273, far below the density that caused this failure. Do not
/// re-enable near 300 ms.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tip_stale_cover_should_rerace(
    enabled: bool,
    awaiting_ms: u64,
    threshold_ms: u64,
    covering: usize,
    max_covering: usize,
    peer_covers_h: bool,
    incumbent_is_self: bool,
    already_reraced_h: bool,
) -> bool {
    enabled
        && threshold_ms > 0
        && awaiting_ms >= threshold_ms
        && covering > 0
        && covering < max_covering
        && !peer_covers_h
        && !incumbent_is_self
        && !already_reraced_h
}

/// Quiet-cover threshold for [`tip_stale_cover_should_rerace`]. `0` disables the re-race.
///
/// Default **0** after the R-274 failure documented above.
pub(crate) fn tip_stale_cover_rerace_ms() -> u64 {
    std::env::var("BLVM_IBD_TIP_RERACE_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Total peers allowed to cover H once a re-race arms (incumbent + one racer).
pub(crate) fn tip_stale_cover_max_covering() -> usize {
    std::env::var("BLVM_IBD_TIP_RERACE_MAX_COVER")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2)
}

/// R-300: extra peers allowed to GetData the same next-needed height H after a
/// stall. **1 = off** (today's exclusive-H pipe, R-298 DNA). 2 or 3 turns the
/// hedge on. Not `BLVM_IBD_TIP_RERACE_MS` (R-274 dested 300ms on the awaiting
/// clock and lost 1580s→3350s). This path is `(H,H)` only, never a second
/// stripe on the sticky (A2/A4/tc172), never far-ahead, never the sticky
/// owner, and it keys off assignment age not the awaiting clock that resets.
pub(crate) fn tip_hedge_n() -> usize {
    let v = latch_env!(usize, {
        std::env::var("BLVM_IBD_TIP_HEDGE_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
            .clamp(1, 3)
    });
    #[cfg(not(test))]
    {
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| {
            tracing::info!("[IBD_TIP_HEDGE_BOOT] n={} ms={}", v, tip_hedge_ms());
        });
    }
    v
}

/// How long H must have been outstanding before a hedge is allowed.
/// Unused when [`tip_hedge_n`] is 1. Never hedge immediately.
pub(crate) fn tip_hedge_ms() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_TIP_HEDGE_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300)
            .clamp(50, 10_000)
    })
}

/// Pure hedge gate. Evidence belongs in the assertion message (R-289 style).
pub(crate) fn tip_hedge_should_fire(
    n: usize,
    outstanding_ms: u64,
    threshold_ms: u64,
    covering: usize,
    peer_is_sticky: bool,
    peer_covers_h: bool,
    has_delivered_bytes: bool,
    hedges_issued: usize,
) -> bool {
    n >= 2
        && outstanding_ms >= threshold_ms
        && covering > 0
        && covering < n
        && hedges_issued < n.saturating_sub(1)
        && !peer_is_sticky
        && !peer_covers_h
        && has_delivered_bytes
}

struct TipHedgeLive {
    h: u64,
    since: std::time::Instant,
    original: String,
    issued: usize,
    hedge_peers: Vec<String>,
}

fn tip_hedge_live() -> &'static std::sync::Mutex<Option<TipHedgeLive>> {
    static S: std::sync::OnceLock<std::sync::Mutex<Option<TipHedgeLive>>> =
        std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(None))
}

/// Observe the current cover of `h` and decide whether `peer_id` may take `(H,H)`.
pub(crate) fn tip_hedge_observe_and_should_assign(
    h: u64,
    peer_id: &str,
    covering: usize,
    covering_peer: Option<&str>,
    peer_is_sticky: bool,
    peer_covers_h: bool,
    has_delivered_bytes: bool,
) -> bool {
    let n = tip_hedge_n();
    if n < 2 || covering == 0 {
        return false;
    }
    let mut g = tip_hedge_live().lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_some_and(|l| l.h != h) {
        *g = None;
    }
    if g.is_none() {
        *g = Some(TipHedgeLive {
            h,
            since: std::time::Instant::now(),
            original: covering_peer.unwrap_or("").to_string(),
            issued: 0,
            hedge_peers: Vec::new(),
        });
    }
    let live = g.as_ref().expect("hedge live stamped");
    if !live.original.is_empty() && live.original == peer_id {
        return false;
    }
    tip_hedge_should_fire(
        n,
        live.since.elapsed().as_millis() as u64,
        tip_hedge_ms(),
        covering,
        peer_is_sticky,
        peer_covers_h,
        has_delivered_bytes,
        live.issued,
    )
}

pub(crate) fn tip_hedge_mark_issued(h: u64, peer_id: &str) {
    let mut g = tip_hedge_live().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(live) = g.as_mut() {
        if live.h == h {
            live.issued = live.issued.saturating_add(1);
            live.hedge_peers.push(peer_id.to_string());
        }
    }
}

/// First body for a hedged height: WIN if a non-original peer landed first.
pub(crate) fn tip_hedge_note_body(h: u64, peer: &str) {
    let mut g = tip_hedge_live().lock().unwrap_or_else(|e| e.into_inner());
    let Some(live) = g.as_ref() else {
        return;
    };
    if live.h != h || live.issued == 0 {
        return;
    }
    let original = live.original.as_str();
    if peer != original {
        tracing::warn!(
            "[IBD_TIP_HEDGE_WIN] h={} winner={} original={} hedges={}",
            h,
            peer,
            original,
            live.issued
        );
    } else {
        tracing::warn!(
            "[IBD_TIP_HEDGE_LOSS] h={} original={} arrived first hedges={}",
            h,
            original,
            live.issued
        );
    }
    *g = None;
}

pub(crate) fn tip_hedge_outstanding_ms(h: u64) -> u64 {
    tip_hedge_live()
        .lock()
        .ok()
        .and_then(|g| {
            g.as_ref()
                .filter(|l| l.h == h)
                .map(|l| l.since.elapsed().as_millis() as u64)
        })
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn test_tip_hedge_reset() {
    *tip_hedge_live().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
pub(crate) fn test_tip_hedge_seed(h: u64, original: &str, age_ms: u64) {
    *tip_hedge_live().lock().unwrap_or_else(|e| e.into_inner()) = Some(TipHedgeLive {
        h,
        since: std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(age_ms))
            .unwrap_or_else(std::time::Instant::now),
        original: original.to_string(),
        issued: 0,
        hedge_peers: Vec::new(),
    });
}

/// R-260: leftover inject already put H on disk into reorder. Case B then HANDOFF
/// `flushed=0` / `bridge_next=H+1` / feeder=0. Insert H into the feeder.
///
/// R-272 required `flight_tip==0` (W26b). That is why 248–300k moved (136 vs R-271
/// 43) and 300–340k reprinted 37: crawl median `flight_tip=1` while inject still
/// `store_has=true`. A seated GetData must not block a local body already in
/// reorder. Duplicate GetData arrival stays hash-admitted; this does not retitle.
/// leftover_w22 / swarm stay false.
///
/// R-287: the floor and the stall gate each solve a different band, so this is an
/// **OR**, not a single threshold.
///
/// Above the floor, promote immediately. R-273 ran floor-only and is the only run
/// in the series with `in_reorder_not_feeder` **17** across 248–370k (R-283 350,
/// R-284 276, R-285 1493) and the fastest wall (1580s vs 2112/2294/4841).
///
/// Below the floor, require a real stall. r278a froze 90s at h=176199 with
/// `store_has=true injected=true` and `[IBD_FEEDER_MISS] reason=in_reorder_not_feeder`,
/// 72k blocks under the floor, so the floor alone leaves that freeze unfixed.
/// A bare stall gate is not the answer either: R-286 ran `stalled_ms >= 50` with no
/// floor, fired 8 `[IBD_LEFTOVER_FEEDER]` by 50k where R-273's must-hold is 0, and
/// cost every fast band (10–50k 1337 vs 3246; 50–120k 865 vs 1748). In dump
/// `next_needed` advances every few ms, so the 1000ms default keeps this off the
/// fast path while still clearing an r278a-class freeze.
pub(crate) fn leftover_inject_should_feeder(
    injected: bool,
    tip_in_feeder: bool,
    flight_tip: usize,
    stalled_ms: u64,
    next_needed: u64,
) -> bool {
    let _ = flight_tip;
    if !injected || tip_in_feeder {
        return false;
    }
    next_needed >= leftover_feeder_min_height() || stalled_ms >= leftover_feeder_stall_ms()
}

/// Milliseconds of the last IBD slow-peer rotation (0 = never).
pub(crate) static IBD_LAST_PEER_ROTATE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Seconds between slow-peer rotations (`BLVM_IBD_PEER_ROTATE_SECS`, 0 = off).
pub(crate) fn ibd_peer_rotate_secs() -> u64 {
    std::env::var("BLVM_IBD_PEER_ROTATE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30)
}

/// Seated peers that must be scored before any rotation
/// (`BLVM_IBD_PEER_ROTATE_MIN_SCORED`). R-289 defaulted 16 against the connected
/// mesh (~45), so the bench filled the count. R-290 counts SEATED peers
/// (R-280 inflight 13.6); 8 is a 13-seat roster with a few unsampled, which a
/// 13-seat roster can actually meet. Guards against evicting out of a thin roster.
pub(crate) fn ibd_peer_rotate_min_scored() -> usize {
    std::env::var("BLVM_IBD_PEER_ROTATE_MIN_SCORED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8)
}

/// Non-sticky in-flight stripe cap (`BLVM_IBD_PEER_DEPTH`, default 1).
/// Same-peer sequential pipelining: the worker may hold this many assigned
/// ranges so the next contiguous stripe is queued before the socket idles.
/// Not A4 (top-scoring half cap 2 = N-way ahead). Sticky/tip still uses
/// `max_in_flight_for` / `TOP_PEER_IN_FLIGHT` / `sole_tip_priority`.
/// Clamp 1..=16 = Core `MAX_BLOCKS_IN_TRANSIT_PER_PEER`.
pub(crate) fn ibd_peer_depth() -> usize {
    std::env::var("BLVM_IBD_PEER_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .clamp(1, 16)
}

/// One rotation per interval, process-wide. `last_ms` 0 means never rotated — seed
/// the clock instead of firing immediately, so a cold mesh gets one full window to
/// produce samples before anyone is judged.
pub(crate) fn should_rotate_slow_peer(now_ms: u64, last_ms: u64, secs: u64) -> bool {
    if secs == 0 || last_ms == 0 {
        return false;
    }
    now_ms.saturating_sub(last_ms) >= secs.saturating_mul(1000)
}

/// Height at or above which a locally-present body is promoted out of reorder
/// without waiting for a stall (`BLVM_IBD_LEFTOVER_FEEDER_MIN_HEIGHT`).
///
/// 248_000 is R-273's floor, unchanged. `u64::MAX` disables the immediate path and
/// leaves only the stall gate (the R-279–R-286 shape).
pub(crate) fn leftover_feeder_min_height() -> u64 {
    std::env::var("BLVM_IBD_LEFTOVER_FEEDER_MIN_HEIGHT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(248_000)
}

/// How long `next_needed` must sit unchanged before a locally-present body is
/// promoted out of reorder into the feeder (`BLVM_IBD_LEFTOVER_FEEDER_STALL_MS`).
///
/// 0 disables the stall requirement (fires whenever the body is in reorder — the
/// R-265 shape). The watchdog only reports a freeze after 30s, so anything well
/// under that still collapses the freeze while leaving healthy bands untouched.
pub(crate) fn leftover_feeder_stall_ms() -> u64 {
    std::env::var("BLVM_IBD_LEFTOVER_FEEDER_STALL_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
}

/// Validation-loop stall under leftover cheese (`next ≤ local_replay_max`).
///
/// Live 2026-08-21: inject stopped at 70705, coordinator went silent, `tip_gap_missing`
/// stayed false so assigner never GetData'd, then 24 feeder stalls aborted IBD
/// because 70705 ≤ body_tip looked like "local replay" not WAN. Validation is the
/// thread that still runs — it must arm GetData and must not abort.
pub(crate) fn leftover_replay_stall_is_disk_hole(next: u64, local_replay_max: u64) -> bool {
    local_replay_max > 0 && next > 0 && next <= local_replay_max.saturating_add(1)
}

/// dest-ag @340221: mute, holes=21, 55s sit then leftover_force (45s).
/// dest-ai @244590: same shape, leftover still 45s — `IBD_TIP_GAP_MISSING`
/// was false while CRAWL `gap_missing=true` / holes=24. dest-y/aa pin-mute
/// FAIL. Do not require the gap atomic. KEEP hero stays 45s.
pub(crate) fn wan_mute_cheese_fast_leftover_wait_secs(
    wan_tip_crawl: bool,
    preferred_keep: bool,
    holes: u64,
) -> Option<u64> {
    if wan_tip_crawl && !preferred_keep && holes >= 5 {
        Some(15)
    } else {
        None
    }
}

/// True when a LOCAL_AHEAD assign start is inside the inject/handoff window.
///
/// Live: ahead partitions walked leftover `70657–70735` while next was `70001`
/// (656 > 256) — workers dumped cheese, validation froze at the first leftover hole.
pub(crate) fn local_ahead_start_within_window(start: u64, next_needed: u64) -> bool {
    start <= next_needed.saturating_add(local_body_ahead_cap())
}

/// Binder class for tip-crawl / starve logs (do **not** trust `holes=0` alone).
///
/// - `TIP_HOLE_AHEAD`: tip body unavailable, ahead bodies in reorder — true hole
/// - `EMPTY_TIP`: tip body unavailable, nothing buffered — pure tip-serial wait
/// - `FILLED_RUNWAY`: tip in reorder (contig≥1) **or** tip in feeder with ahead —
///   healthy pipeline (C1q: tip often leaves reorder for feeder during fast crawl)
/// - `CHEESE`: tip present/missing with bridge holes≥5
///
/// `tip_in_feeder`: tip height is in the validation feeder (body available).
pub(crate) fn tip_runway_mode(
    tip_in_reorder: bool,
    contig_runway: u64,
    ahead_buffered: usize,
    bridge_holes: u64,
    tip_in_feeder: bool,
) -> &'static str {
    if bridge_holes >= 5 {
        return "CHEESE";
    }
    // C1q: tip already in feeder = body available — not a tip hole even if reorder
    // lacks tip (validation consumed it). Ahead buffered + feeder = filled runway.
    if tip_in_feeder {
        if ahead_buffered > 0 || contig_runway >= 1 || tip_in_reorder {
            return "FILLED_RUNWAY";
        }
        // Feeder has tip but no ahead yet — still not TIP_HOLE_AHEAD.
        return "FILLED_RUNWAY";
    }
    if !tip_in_reorder && ahead_buffered > 0 {
        "TIP_HOLE_AHEAD"
    } else if !tip_in_reorder {
        "EMPTY_TIP"
    } else if contig_runway >= 1 {
        "FILLED_RUNWAY"
    } else {
        "UNKNOWN"
    }
}

/// dest-ab @255073: ≥80 hero, holes=17, first=+64, reorder=3, 49s sit.
/// dest-ae FAIL: holes≥5 || first≥H+8 (no reorder bar) → extra pins,
/// 180–200k **304** ≺ dest-ab **522**. dest-ac mute-ahead stays reverted.
/// Only the dest-ab miss: KEEP hero + reorder<8 (starve timer never
/// armed) + autopsy cheese (holes≥12 and first≥H+32).
pub(crate) fn cheese_fast_hero_sparse_sit(
    gap_missing: bool,
    holes: u64,
    first_ahead: Option<u64>,
    tip: u64,
    preferred_keep: bool,
    reorder_len: usize,
) -> bool {
    if !preferred_keep || !gap_missing || reorder_len >= 8 {
        return false;
    }
    holes >= 12 && first_ahead.is_some_and(|a| a >= tip.saturating_add(32))
}

/// Wall-clock ms of the most recent validation feeder stall (`IBD_STALL`).
pub(crate) static IBD_VALIDATION_STALL_WALL_MS: AtomicU64 = AtomicU64::new(0);

/// Whether the checkpoint export thread should defer starting a new export.
pub(crate) fn export_start_gate_allows() -> bool {
    export_start_gate_allows_at(0, 0, 0)
}

/// True when validation lag must start an export even under Critical / W176.
pub(crate) fn checkpoint_lag_exempt(vh: i32, last_exported: i32, interval: i32) -> bool {
    let lag = vh.saturating_sub(last_exported);
    let exempt_need = interval.max(LAG_EXEMPT_MIN_BLOCKS);
    interval > 0 && lag >= exempt_need
}

fn export_start_gate_refuse(reason: &'static str, vh: i32, last_exported: i32, interval: i32) {
    let lag = vh.saturating_sub(last_exported);
    tracing::warn!(
        "[IBD_CKPT_START_REFUSE] reason={} vh={} last_exported={} interval={} lag={}",
        reason,
        vh,
        last_exported,
        interval,
        lag
    );
}

/// W176/stall defer, with lag-exempt when validation has walked a full interval
/// past `last_exported` (dest-bc 96s isolation stall after 594973).
///
/// LAG_EXEMPT is evaluated **before** Critical pressure. dest-bc 376389: a
/// None→Critical latch sat in front of exempt and disabled checkpointing for
/// 2.3h / 133k blocks with no log line.
pub(crate) fn export_start_gate_allows_at(vh: i32, last_exported: i32, interval: i32) -> bool {
    let lag = vh.saturating_sub(last_exported);
    let exempt_need = interval.max(LAG_EXEMPT_MIN_BLOCKS);
    if checkpoint_lag_exempt(vh, last_exported, interval) {
        tracing::info!(
            "[IBD_CKPT_LAG_EXEMPT] vh={} last_exported={} interval={} need={} lag={} — skipping pressure/W176/stall defer",
            vh,
            last_exported,
            interval,
            exempt_need,
            lag
        );
        return true;
    }
    if crate::node::parallel_ibd::memory::ibd_pressure_level_snapshot()
        >= crate::node::parallel_ibd::memory::PressureLevel::Critical
    {
        export_start_gate_refuse("pressure_critical", vh, last_exported, interval);
        return false;
    }
    let kill_mb = std::env::var("BLVM_PROC_ANON_KILL_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0);
    if let Some(kill_mb) = kill_mb {
        let (anon_mb, swap_mb) = crate::node::parallel_ibd::memory::read_proc_anon_and_swap_mb();
        if anon_mb.saturating_add(swap_mb).saturating_add(10_000) >= kill_mb {
            export_start_gate_refuse("proc_anon_kill", vh, last_exported, interval);
            return false;
        }
    }
    // W79: W75 deferred on `IBD_TIP_GAP_MISSING` and empty feeder. During healthy WAN tip
    // crawl those are nearly always true (gap_missing between receives; feeder drains to 0
    // between tips) — live genesis→250k: **0** checkpoint exports, tip60≈100, gap_missing≈100%,
    // feeder0≈97%. Only defer on *actual* tip distress / recent validation stall.
    // Live W75 freeze @344348 still trips late-body / soft-retry freeze here.
    if tip_stage::tip_ahead_frozen_for_late_body() || tip_stage::tip_ahead_frozen_for_soft_retry() {
        export_start_gate_refuse("tip_ahead_frozen", vh, last_exported, interval);
        return false;
    }
    // W174/W176: defer during tip-hole storms. Piggyback export holds `is_compacting`
    // for 90–200s and contends with tip body persist/inject (W173 @371420; W175 2nd
    // export started at holes=15 → tip60 64→32 + high_ev 151→913). Threshold **32→16**.
    let gap_missing = IBD_TIP_GAP_MISSING.load(Ordering::Relaxed);
    if gap_missing && IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed) >= 16 {
        export_start_gate_refuse("tip_holes", vh, last_exported, interval);
        return false;
    }
    // W176: also defer when tip is already awaiting a body — starting a 145s+ export
    // into a stuck tip burns the tip60 rate window (live W175 @372k export start).
    if gap_missing && tip_stage::tip_awaiting_secs_for_cap() >= 5 {
        export_start_gate_refuse("tip_awaiting", vh, last_exported, interval);
        return false;
    }
    // W177: soft-resume local body replay — gap_missing is false so W176 tip gates
    // never fire; export still stalls validation + spikes high_ev (live W176: export
    // @372134 while next_needed≪body_tip, wall>226s, high_ev→74k before WAN crawl).
    if IBD_LOCAL_BODY_AHEAD.load(Ordering::Relaxed) {
        export_start_gate_refuse("local_body_ahead", vh, last_exported, interval);
        return false;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last_stall = IBD_VALIDATION_STALL_WALL_MS.load(Ordering::Relaxed);
    if last_stall > 0 && now_ms.saturating_sub(last_stall) < 30_000 {
        export_start_gate_refuse("recent_val_stall", vh, last_exported, interval);
        return false;
    }
    true
}

struct ParallelIbdSessionGuard;

impl Drop for ParallelIbdSessionGuard {
    fn drop(&mut self) {
        PARALLEL_IBD_SESSION_ACTIVE.store(false, Ordering::Release);
        memory::reset_ibd_pressure_on_session_end();
    }
}

/// N15: prepare dispatch buffers for coordinator→feeder/prefetch.
///
/// Engine mode **defers** serial txid SHA to the validation dispatcher so admit/handoff
/// is not taxed (empty `tx_ids` → filled before `SpendSession::append`). Legacy mode still
/// extracts keys+txids here for prefetch workers.
fn prepare_coord_dispatch_bufs(
    engine_mode: bool,
    block: &Block,
    tx_ids_buf: &mut Vec<Hash>,
    keys_buf: &mut Vec<crate::storage::disk_utxo::OutPointKey>,
) {
    tx_ids_buf.clear();
    keys_buf.clear();
    if engine_mode {
        return;
    }
    block_input_keys_and_tx_ids_filtered(block, tx_ids_buf, keys_buf);
}

/// Under Emergency, try to pull `next_needed` from `block_rx` into `reorder_buffer` without
/// running the bulk `recv_many` path. Returns true when the gap height is buffered.
///
/// `admit_window` must match the coordinator's [`effective_gap_admit_window`] — hardcoding
/// [`gap_admit_window`] (256) caused live GAP_ADMIT_DROP storms during bulk catch-up while
/// the main loop already admitted with `wan_bulk_admit_window` (1024).
pub(crate) fn emergency_drain_block_rx_for_gap(
    block_rx: &mut tokio::sync::mpsc::Receiver<(u64, SharedBlock, SharedWitnesses)>,
    reorder_buffer: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    next_needed: u64,
    max_reorder_len: usize,
    admit_limit: usize,
    total_received: &mut u64,
    bridge_pending_max: usize,
    admit_window: u64,
) -> bool {
    if reorder_buffer.contains_key(&next_needed) {
        return true;
    }
    // S2a: always use coordinator-scale `admit_limit` for half-cap throttle — never
    // `reorder.len()+1`, which weakens S2 as the buffer bloats past the nominal cap.
    while let Ok((h, block, witnesses)) = block_rx.try_recv() {
        *total_received += 1;
        if !insert_reorder_gap_aware(
            reorder_buffer,
            h,
            block,
            witnesses,
            next_needed,
            admit_limit,
            admit_window,
            bridge_pending_max,
        ) {
            continue;
        }
        if h == next_needed {
            return true;
        }
        if reorder_buffer.len() >= max_reorder_len {
            break;
        }
    }
    reorder_buffer.contains_key(&next_needed)
}

/// W25c: non-blocking drain that admits `next_needed` before other heights in the batch.
///
/// Note: multi-batch "drain until tip found" (cliff @323k plan) correlated with
/// `block_tx closed` storms on synth — kept single-batch tip-sort. Channel cap this
/// harness is 1056 with `BATCH_DRAIN_LIMIT=2000`, so one batch already covers the FIFO.
pub(crate) fn drain_block_rx_tip_first(
    block_rx: &mut tokio::sync::mpsc::Receiver<(u64, SharedBlock, SharedWitnesses)>,
    reorder_buffer: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    dispatched: &mut rustc_hash::FxHashSet<u64>,
    next_needed: u64,
    admit_limit: usize,
    admit_window: u64,
    bridge_pending_max: usize,
    total_received: &mut u64,
    max_items: usize,
) -> usize {
    let mut batch = Vec::new();
    while batch.len() < max_items {
        match block_rx.try_recv() {
            Ok(item) => batch.push(item),
            Err(_) => break,
        }
    }
    if batch.is_empty() {
        return 0;
    }
    batch.sort_by(|(a, _, _), (b, _, _)| {
        let a_tip = *a == next_needed;
        let b_tip = *b == next_needed;
        b_tip.cmp(&a_tip).then_with(|| a.cmp(b))
    });
    let mut admitted = 0usize;
    for (h, block, witnesses) in batch {
        *total_received += 1;
        if dispatched.contains(&h) {
            dispatched.remove(&h);
        }
        if insert_reorder_gap_aware(
            reorder_buffer,
            h,
            block,
            witnesses,
            next_needed,
            admit_limit,
            admit_window,
            bridge_pending_max,
        ) {
            admitted += 1;
        }
    }
    admitted
}

/// True when the reorder buffer has headroom for bulk `recv_many` under Emergency.
#[inline]
pub(crate) fn emergency_may_bulk_recv(
    reorder_buffer: &BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    pipeline_quarter: usize,
) -> bool {
    reorder_buffer.len() < pipeline_quarter
}

/// Max height above `next_needed` admitted into reorder while the gap is missing (S2).
/// Default 256; override with `BLVM_IBD_GAP_ADMIT_WINDOW` (≥32).
pub(crate) fn gap_admit_window() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_GAP_ADMIT_WINDOW")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n >= 32)
            .unwrap_or(256)
    })
}

/// W29: tighter admit/evict window past on-disk body tip while the validation tip is missing.
/// Default **64** (inject-chain friendly); override with `BLVM_IBD_WAN_GAP_ADMIT_WINDOW` (32–256).
/// Dest R-81 uses **192** (hero 64 + two packed 64s). Do not dest default 256 (W28d
/// throttle-at-half grew reorder ~270 and stalled feeder 37–74s).
pub(crate) fn wan_gap_admit_window() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_WAN_GAP_ADMIT_WINDOW")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64)
            .clamp(32, 256)
    })
}

/// Mid-chain catch-up past body tip: admit window must track the multi-peer ahead pipe.
/// Default **1024** (was 256). Live: max_ahead 400–528 with admit=256 → 20k+ GAP_ADMIT_DROP
/// of ahead bodies while tip starved (~few BPS despite busy_peers>1).
/// Env `BLVM_IBD_WAN_BULK_ADMIT_WINDOW` (clamp 256–8192).
///
/// R-342: default follows [`wan_bulk_ahead_cap`] (never below 1024). R-341 ran
/// `max_ahead` 4096 against admit 1024: `total_drops=158336` — 63% of every body fetched
/// was written to disk and re-read through the 20 ms inject poll, and on-disk (`store=1`)
/// holes were 60–75% of the feeder hole wait in every band past 200k. Whatever the
/// assigner is allowed to fetch, the reorder must be allowed to keep.
pub(crate) fn wan_bulk_admit_window() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_WAN_BULK_ADMIT_WINDOW")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| wan_bulk_ahead_cap().max(1024))
            .clamp(256, 8192)
    })
}

/// Header tip must be at least this far past `next_needed` to treat WAN as bulk catch-up
/// (multi-peer) rather than near-tip crawl. Env `BLVM_IBD_WAN_BULK_CATCHUP_GAP` (default **2048**).
pub(crate) fn wan_bulk_catchup_gap() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_WAN_BULK_CATCHUP_GAP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2048)
            .clamp(512, 100_000)
    })
}

/// Past body tip but still far from header tip — open multi-peer pipeline (not tip-crawl).
#[inline]
pub(crate) fn wan_bulk_catchup(header_tip: u64, next_needed: u64) -> bool {
    header_tip >= next_needed && header_tip.saturating_sub(next_needed) >= wan_bulk_catchup_gap()
}

/// F-D: spawn 2 download workers per WAN peer so sticky dual-pipe can arm.
/// Default **off** — live 2026-07-15: enabling for all peers coincided with tip BPS
/// drop (~17→~9) and tip-owner churn (many owners, few sticky tenure). Span=128 alone
/// is the primary pipe-fill fix. Set `BLVM_IBD_STICKY_DUAL_WORKER=1` to experiment.
pub(crate) fn sticky_dual_worker_enabled() -> bool {
    policy::sticky_dual_worker()
}

/// Mode T tip-glue / sticky flight clamp. Opt-in: `BLVM_IBD_SOLE_TIP_PRIORITY=1`.
///
/// Default **off** (2026-08-06): default-on + TOP=1 locked tip90≈55 (tc175).
/// tc177 TOP=4 without tip-glue flooded archive (tip90≈30). KEEP: SOLE_TIP=0 TOP=1.
pub(crate) fn sole_tip_priority_enabled() -> bool {
    policy::sole_tip_priority()
}

/// First `BLVM_IBD_PEERS` entry when sole tip-priority is on.
///
/// Mode T dual loopback (`:18333` tip + `:18334` ahead via socat): tip-owner must stay
/// on the first pin. tc168 connected both peers + 1 worker each, then TIP_OWNER_COOLDOWN
/// on `:18333` let TIP_PIN elect `:18334` → TIP_PIPE=256/grown=64 incomplete storm →
/// tip90≈7.8.
pub(crate) fn sole_tip_forced_owner() -> Option<String> {
    if !sole_tip_priority_enabled() {
        return None;
    }
    std::env::var("BLVM_IBD_PEERS").ok().and_then(|s| {
        s.split(',')
            .map(|p| p.trim().to_string())
            .find(|p| !p.is_empty())
    })
}

/// Shared latch for `BLVM_IBD_TOP_PEER_IN_FLIGHT` (assigner max_in_flight + sole workers).
pub(crate) fn top_peer_in_flight_cap() -> usize {
    policy::top_peer_in_flight()
}

/// Workers per peer under sole tip-priority. Matches sticky `max_in_flight` intent:
/// TOP_PEER_IN_FLIGHT≥2 → 2 workers (tip+next overlap); else 1.
pub(crate) fn sole_tip_workers_per_peer() -> usize {
    if top_peer_in_flight_cap() >= 2 { 2 } else { 1 }
}

/// Ahead cap during bulk catch-up past body tip. Env `BLVM_IBD_WAN_BULK_AHEAD` (default **2048**).
pub(crate) fn wan_bulk_ahead_cap() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_WAN_BULK_AHEAD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2048)
            .clamp(512, 8192)
    })
}

/// R-305: raise [`wan_bulk_tip_gap_ahead_cap`] ceiling 2048→8192 (`RUNWAY_SPAN`).
/// Default **off** — clamp stays 128..=2048 (arm A byte-identical). Opt in:
/// `BLVM_IBD_WIDE_RUNWAY=1` then `BLVM_IBD_WAN_BULK_TIP_GAP_AHEAD=8192`.
pub(crate) fn wide_runway_enabled() -> bool {
    latch_env!(bool, {
        matches!(
            std::env::var("BLVM_IBD_WIDE_RUNWAY")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("1") | Some("true") | Some("on") | Some("yes")
        )
    })
}

/// Bulk catch-up ahead while the feeder is empty (true tip starvation).
///
/// **W76:** default follows [`wan_tip_gap_ahead_cap`] (**256**). Live W75 soft-resume
/// ~350–360k: `wan_bulk_catchup` is always true once headers are at network tip, so the
/// old **1024** `wan_bulk_gap` path kept `max_ahead≈1024` while tip never entered the
/// bridge (`gap_missing`+ahead-only pending ≈92%, tip60≈35–50). Env
/// `BLVM_IBD_WAN_BULK_TIP_GAP_AHEAD` still overrides.
/// **R-305:** clamp hi is 2048 unless [`wide_runway_enabled`] (then 8192 = `RUNWAY_SPAN`).
/// Does **not** change [`wan_bulk_ahead_cap`] (non-starve path).
pub(crate) fn wan_bulk_tip_gap_ahead_cap() -> u64 {
    latch_env!(u64, {
        let hi = if wide_runway_enabled() { 8192 } else { 2048 };
        std::env::var("BLVM_IBD_WAN_BULK_TIP_GAP_AHEAD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(wan_tip_gap_ahead_cap)
            .clamp(128, hi)
    })
}

/// W76: choose WAN ahead cap kind past body tip.
///
/// When the feeder is empty, always return the tip-starve window — even if
/// [`wan_bulk_catchup`] is true. Otherwise mid-chain IBD (headers already at tip) never
/// takes the tight [`wan_tip_gap_ahead_cap`] path.
pub(crate) fn wan_ahead_policy(
    bulk: bool,
    tip_feeder_starve: bool,
    tip_gap_open: bool,
    ibd_ready: usize,
) -> (&'static str, u64) {
    // A5 KEEP (opt-in TIP_ADMIT_TIGHT): align ahead with admit window (64).
    // A6 tip-first=8 REVERT 2026-08-03: `…a6-tipfirst-…T023759Z` tip_crawl **94.83** ≺ A5
    // **103.72** (falsifier: tip GetData RTT, not assign depth past admit).
    if tip_admit_tight_enabled() && (tip_feeder_starve || tip_gap_open) {
        // Mode T sole archive (tc92): wan_tip_tight(64) + tip_hole GD_SLOW cliff(32→8)
        // deepens starve under elevated getdata EWMA. Keep tip-gap ahead so pipe-fill
        // stays ahead of next_needed; never blacklist/cooldown the sole peer.
        if ibd_ready <= 1 && download::tip_hole_gd_slow() {
            let kind = if bulk {
                "wan_bulk_gap_sole"
            } else {
                "wan_tip_sole"
            };
            return (kind, wan_bulk_tip_gap_ahead_cap());
        }
        return ("wan_tip_tight", wan_gap_admit_window());
    }
    if tip_feeder_starve {
        let kind = if bulk { "wan_bulk_gap" } else { "wan_tip" };
        (kind, wan_bulk_tip_gap_ahead_cap())
    } else if tip_gap_open && !bulk {
        ("wan_tip", wan_tip_gap_ahead_cap())
    } else if bulk {
        ("wan_bulk", wan_bulk_ahead_cap())
    } else {
        ("wan", wan_body_ahead_cap())
    }
}

/// A4 tip-tight admit: when on, tip crawl always uses [`wan_gap_admit_window`] (default 64)
/// even if [`wan_bulk_catchup`] is true (headers-at-tip).
///
/// Default **off** (2026-08-03): archive soak `…a4-tipadmit-…T224906Z` tip_crawl **73.44**
/// (KEEP mech vs A2 **20.60**), but two public keepconfirms tip60-failed (soft / unconfirmed).
/// Manual undo of default-on — opt in: `BLVM_IBD_TIP_ADMIT_TIGHT=1` (archive fabric A/B).
pub(crate) fn tip_admit_tight_enabled() -> bool {
    latch_env!(bool, {
        matches!(
            std::env::var("BLVM_IBD_TIP_ADMIT_TIGHT")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("1") | Some("true") | Some("on") | Some("yes")
        )
    })
}

/// Effective reorder admit window: near-tip crawl uses tight W29; bulk catch-up uses deeper
/// unless [`tip_admit_tight_enabled`].
///
/// **A4 evidence:** `wan_tip_crawl && bulk_catchup` → [`wan_bulk_admit_window`] (1024) lets
/// fast peers fill `TIP_HOLE_AHEAD` while tip body is missing. Tight path is opt-in until a
/// healthy public keepconfirm clears the Manual REVERT gate.
#[inline]
pub(crate) fn effective_gap_admit_window(wan_tip_crawl: bool, bulk_catchup: bool) -> u64 {
    if wan_tip_crawl {
        if tip_admit_tight_enabled() || !bulk_catchup {
            return wan_gap_admit_window();
        }
        wan_bulk_admit_window()
    } else {
        gap_admit_window()
    }
}

/// R-344: the admit = ahead default (R-342, `wan_bulk_admit_window`) applies only where the
/// window assigner governs. Below `WINDOW_FROM` the legacy assigner keeps the pre-R-342
/// 1024 admit: R-343 parked 578 s at 10–20k with `reorder=2240`, `busy_peers=0`,
/// `in_flight_ranges=0`, `duty_ia_ms=60000` — a deep reorder the legacy crawl never had
/// (R-341 dropped everything past 1024 there and ran 0–120k without incident).
#[inline]
pub(crate) fn admit_window_for_height(next_needed: u64, admit: u64) -> u64 {
    if ChunkAssigner::window_path_height(next_needed) {
        admit
    } else {
        admit.min(1024)
    }
}

/// A1/W5: shared floor for stall / bridge-full `max_ahead` clamps.
/// Default **512** so ≥16 peers stay busy at `chunk_size=32`. Env `BLVM_IBD_GAP_AHEAD_FLOOR`.
pub(crate) fn gap_ahead_floor() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_GAP_AHEAD_FLOOR")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(512)
            .clamp(128, 4096)
    })
}

/// P1: max WAN download peers that receive worker slots (top scored). Default 24.
pub(crate) fn ibd_active_peer_cap() -> usize {
    static CACHED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_ACTIVE_PEERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24)
            .clamp(4, 128)
    })
}

/// W33d: fewer active workers past body tip — less retry noise, more depth per owner.
pub(crate) fn ibd_wan_gap_active_peer_cap() -> usize {
    static CACHED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_WAN_GAP_ACTIVE_PEERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10)
            .clamp(4, 24)
    })
}

/// L1: when validating inside the on-disk body tip, cap download ahead so workers do not
/// prefetch thousands of local heights into the bridge (live: 4k ahead → bridge wedge at 680k).
pub(crate) fn local_body_ahead_cap() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_LOCAL_AHEAD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(256)
            .clamp(64, 1024)
    })
}

/// W11: past on-disk body tip, keep WAN download ahead bounded so workers do not open a
/// 2k window while the tip gap starves (live: soft-retries at 685k–686k with tip≈6849xx).
pub(crate) fn wan_body_ahead_cap() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_WAN_AHEAD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(384)
            .clamp(128, 2048)
    })
}

/// W11/W35″: when feeder empty / tip gap missing past body tip, bound ahead near the tip.
///
/// Default **256** (was 128). A6l gave sticky `max_in_flight=2`, but with max_ahead=128 the
/// second 128-span starts at `next+128` which is past `max_start` — dual-pipe was a no-op.
/// 256 lets sticky hold tip..tip+127 **and** tip+128..tip+255. Still tighter than
/// `wan_body_ahead_cap` (384). Env `BLVM_IBD_WAN_TIP_AHEAD`.
pub(crate) fn wan_tip_gap_ahead_cap() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_WAN_TIP_AHEAD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(256)
            .clamp(128, 512)
    })
}

/// W18: contiguous heights above tip allowed into the bridge during WAN catchup so inject
/// chains (live mean chained≈30) can flush in one burst. Beyond this band → defer (avoid holes).
/// Env `BLVM_IBD_WAN_TIP_DISPATCH_BAND` (default 64, clamp 8–256).
pub(crate) fn wan_tip_dispatch_band() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_WAN_TIP_DISPATCH_BAND")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64)
            .clamp(8, 256)
    })
}

/// GAP-7 / L2: refuse dispatch of far-ahead heights into OrderedReadyBridge while the gap
/// cannot drain.
///
/// - Reorder `gap_missing`: keep the original far window (`next + admit_window`).
/// - Bridge `next_expected_missing`: gap may already be in reorder but not in pending —
///   only allow a tight near-gap band so ahead work cannot fill the bridge (live 680k).
/// - **W17/W57/W58 WAN tip crawl:** when tip is *missing* from reorder+bridge, defer all
///   ahead (no hole-fill) — **including `bulk_catchup`** (W58). Mid-chain header≪tip gap
///   always sets bulk; the old `!bulk` gate left W17 dead (live ~497k: `bmin>>tip` 87%).
///   When tip is present and not bulk, allow `[next, next+band]` so inject chains drain
///   (W18). When tip is present and bulk, fall through to the multi-peer tight/window rules.
/// - **B-3 HASH_FETCH (retracted R-182):** skipping W58 reprinted Face 2 (HOLE +
///   reorder 2048, dump 1484). W58 stays on under the flag. Do **not** publish
///   `wan_body_tip`. W18 / admit / evict / CRAWL log still see `wan_tip_crawl`.
/// - **W34h (narrowed W57):** feeder-starve near-ahead only when tip is *present*
///   (reorder and/or bridge). The old W34h exception under `gap_missing &&
///   next_expected_missing` hole-filled OrderedReadyBridge (live ~497k: BPS≈0.6,
///   `bmin>>tip` 86%, `gap_flush_on_abort`≈10k).
/// - **Bulk catch-up (tip present):** far below header tip — use the admit `window`
///   (multi-peer pipe), not W18 tip-band serialization (live: 8–40 BPS at ~50–60k).
///
/// Resume9: the fill loop is blocked on the bridge cursor. That body is in
/// reorder, and the atomic tip is already taken into the validation pipeline,
/// so W58's `gap_missing` defer is looking at a tip that is no longer in
/// reorder or the feeder. Emitting the cursor is the next in-order block,
/// not a hole-fill of the frontier.
#[inline]
pub(crate) fn bridge_cursor_dispatch_allowed(
    h: u64,
    next_needed: u64,
    bridge_next: Option<u64>,
) -> bool {
    bridge_next == Some(h) && h > next_needed && tip_stage::tip_taken_by_validation(next_needed)
}

/// Cursor body is in reorder and not in the feeder or bridge pending.
/// `dispatched` still hides it from the fill loop (engine dispatch never
/// puts a dropped body back, and a later insert can return it to reorder).
#[inline]
pub(crate) fn release_cursor_stuck_in_reorder(
    dispatched: &mut rustc_hash::FxHashSet<u64>,
    reorder_has_cursor: bool,
    cursor: Option<u64>,
    cursor_in_feeder: bool,
    cursor_in_pending: bool,
) {
    let Some(h) = cursor else {
        return;
    };
    if cursor_in_feeder || cursor_in_pending || !reorder_has_cursor {
        return;
    }
    dispatched.remove(&h);
}

#[inline]
fn defer_bridge_ahead_dispatch(
    h: u64,
    next_needed: u64,
    gap_missing: bool,
    next_expected_missing: bool,
    window: u64,
    wan_tip_crawl: bool,
    feeder_starved: bool,
    bulk_catchup: bool,
) -> bool {
    let _ = feeder_starved; // reserved: tip-present starve uses same band as W18 today
    if h == next_needed {
        return false;
    }
    // L2b: apply is in a reserved stripe (R-100 hold ∪ live). Do not W58 the
    // rest of **that** `(s,e)`. Farm B at +LEAD stays deferred.
    if lookahead_same_reserved_stripe(next_needed, h) {
        return false;
    }
    // W58 / P1: tip nowhere on WAN crawl → never hole-fill, even under bulk_catchup.
    // R-16 dest cheese'd — W58 revert. Do not publish wan_body_tip.
    // R-182: HASH_FETCH skip of this defer reprinted Face 2 (fat HOLE 13/16,
    // reorder 2048, dump 1484). W58 stays on when the flag is on.
    if wan_tip_crawl && gap_missing && next_expected_missing {
        return true;
    }
    if wan_tip_crawl && !bulk_catchup {
        // Tip in reorder and/or bridge — allow contiguous band (W18).
        return h > next_needed.saturating_add(wan_tip_dispatch_band());
    }
    if next_expected_missing {
        let tight = window.clamp(16, 64);
        return h > next_needed.saturating_add(tight);
    }
    if wan_tip_crawl {
        // R-340: bulk catch-up with a healthy front used to admit up to the bridge cap
        // (512) and then evict everything above `tight_keep` at the first hole
        // (R-339: 1466 × 32 blocks reloaded from disk). Never hand the bridge more than
        // it keeps.
        return h > next_needed.saturating_add(prefetch::wan_bridge_tight_keep());
    }
    gap_missing && h > next_needed.saturating_add(window)
}

/// Insert into reorder under S2/S2b gap-aware admission. Returns false if dropped / below tip.
///
/// Throttle far-ahead admission when the buffer is ≥ half capacity **and** either:
/// - S2: validation gap (`next_needed`) is missing from reorder, or
/// - S2b: OrderedReadyBridge is at `bridge_pending_max` (gap may already be in reorder
///   but cannot drain while `bridge_pending=512` — live: reorder≈5–6GB + feeder=0).
///
/// Only heights in `[next_needed, next_needed + window]` are admitted under throttle.
/// `bridge_pending_max == 0` disables the S2b bridge check (tests / no-bridge paths).
#[inline]
pub(crate) fn insert_reorder_gap_aware(
    reorder_buffer: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    h: u64,
    block: SharedBlock,
    witnesses: SharedWitnesses,
    next_needed: u64,
    buffer_limit: usize,
    window: u64,
    bridge_pending_max: usize,
) -> bool {
    if h < next_needed {
        body_dup::note_discard(body_dup::DiscardReason::AlreadyValidated, Some(h), 0);
        return false;
    }
    let gap_missing = !reorder_buffer.contains_key(&next_needed);
    let bridge_full = bridge_pending_max > 0
        && memory::BRIDGE_PENDING_COUNT.load(Ordering::Relaxed) as usize >= bridge_pending_max;
    let half = (buffer_limit / 2).max(1);
    // W29: while tip is missing, always enforce the admit window — do not wait until the
    // buffer is half full (live: reorder≈270 with limit≈2k never hit half-throttle).
    let throttle = gap_missing || (bridge_full && reorder_buffer.len() >= half);
    if throttle && h > next_needed.saturating_add(window) && !lookahead_height_reserved(h) {
        body_dup::note_discard(body_dup::DiscardReason::AdmitDrop, Some(h), 0);
        let n = memory::GAP_ADMIT_DROP_BLOCKS.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n % 64 == 0 {
            warn!(
                "[IBD_GAP_ADMIT_DROP] dropped h={} (next_needed={}, reorder={}, limit={}, window={}, bridge_pending={}, bridge_max={}, gap_missing={}, total_drops={})",
                h,
                next_needed,
                reorder_buffer.len(),
                buffer_limit,
                window,
                memory::BRIDGE_PENDING_COUNT.load(Ordering::Relaxed),
                bridge_pending_max,
                gap_missing,
                n
            );
        }
        return false;
    }
    reorder_buffer.insert(h, (block, witnesses));
    tip_stage::mark_reorder(h);
    true
}

/// S2c: shrink reorder working set when S2/S2b throttle is active.
///
/// Prunes heights `< next_needed` (stale undispatched entries) and evicts the highest keys
/// `> next_needed + window` until `len < half` or only the near-gap window remains.
/// Complements S2/S2b drop-on-admit, which cannot free already-buffered `Arc<Block>` bodies.
///
/// **S2e:** When the bridge is full, target deeper than `half−64` (live: reorder stuck at 936
/// with bridge_pending=512, 1–2 evicts/tick, anon→14 GB). Aim for `max(window, half/4)`.
pub(crate) fn evict_reorder_gap_pressure(
    reorder_buffer: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    next_needed: u64,
    buffer_limit: usize,
    window: u64,
    bridge_pending_max: usize,
) -> usize {
    let gap_missing = !reorder_buffer.contains_key(&next_needed);
    let bridge_full = bridge_pending_max > 0
        && memory::BRIDGE_PENDING_COUNT.load(Ordering::Relaxed) as usize >= bridge_pending_max;
    let half = (buffer_limit / 2).max(1);
    // Evict down to `pressure_target`, not `half`. With half=1000 the old `len < half` guard
    // no-oped at reorder=999 while WAN kept refilling → 1-block treadmill (~1 GB Arc blocks).
    const REORDER_PRESSURE_SLACK: usize = 64;
    let pressure_target = if gap_missing {
        // W29: tip missing — keep only the near-gap window (live: reorder≈270 while tip empty).
        (window as usize).saturating_add(8).max(32)
    } else if bridge_full {
        // S2e: bridge saturated — free far-ahead reorder aggressively so admit-drop + RAM ease.
        (half / 4).max(window as usize).max(64)
    } else {
        half.saturating_sub(REORDER_PRESSURE_SLACK)
    };
    if !(gap_missing || bridge_full) || reorder_buffer.len() <= pressure_target {
        return 0;
    }

    let mut evicted = 0usize;
    let stale: Vec<u64> = reorder_buffer
        .keys()
        .filter(|&&h| h < next_needed)
        .copied()
        .collect();
    for h in stale {
        body_dup::note_discard(body_dup::DiscardReason::ReorderEvict, Some(h), 0);
        reorder_buffer.remove(&h);
        evicted += 1;
    }

    let ceiling = next_needed.saturating_add(window);
    let mut batch_evicted = 0usize;
    const REORDER_EVICT_BATCH_MAX: usize = 32;
    while reorder_buffer.len() > pressure_target && batch_evicted < REORDER_EVICT_BATCH_MAX {
        let Some(max_h) = reorder_buffer
            .keys()
            .rev()
            .copied()
            .find(|&h| h > ceiling && !lookahead_height_reserved(h))
        else {
            break;
        };
        body_dup::note_discard(body_dup::DiscardReason::ReorderEvict, Some(max_h), 0);
        reorder_buffer.remove(&max_h);
        evicted += 1;
        batch_evicted += 1;
    }

    if evicted > 0 {
        let total = memory::REORDER_EVICT_BLOCKS.fetch_add(evicted as u64, Ordering::Relaxed)
            + evicted as u64;
        if total == evicted as u64 || total % 64 == 0 {
            warn!(
                "[IBD_REORDER_EVICT] evicted {} block(s) (next_needed={}, reorder={}, limit={}, window={}, bridge_pending={}, bridge_max={}, gap_missing={}, total_evicted={})",
                evicted,
                next_needed,
                reorder_buffer.len(),
                buffer_limit,
                window,
                memory::BRIDGE_PENDING_COUNT.load(Ordering::Relaxed),
                bridge_pending_max,
                gap_missing,
                total
            );
        }
    }
    evicted
}

/// W34/W54: peel bridge + extract tip block for forced handoff when tip is stranded in reorder.
///
/// **W54:** Do **not** gate on feeder depth. Live soft-resume (2026-07-16): feeder held ~383
/// ahead blocks while tip sat in reorder / LOCAL_GAP reinject loop; `feeder_len > 16` made
/// Case B a no-op → VALRES stuck 600s on tip, ~610k `IBD_LOCAL_GAP` injects. Tip not in feeder
/// must always leave reorder (feeder tip-bypass already admits `h < min_buffered_height`).
#[allow(clippy::too_many_arguments)]
fn prepare_coordinator_tip_handoff(
    next_needed: u64,
    gap_missing: bool,
    feeder_len: usize,
    peel_bridge: bool,
    reorder_buffer: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    dispatched: &mut rustc_hash::FxHashSet<u64>,
    ready_bridge: Option<&prefetch::OrderedReadyBridge>,
    admit_window: u64,
    bridge_pending_max: usize,
    wan_tip_crawl: bool,
    tip_in_feeder: bool,
) -> Option<(SharedBlock, SharedWitnesses, usize, bool)> {
    if gap_missing || tip_in_feeder {
        return None;
    }
    let in_bridge_pending = ready_bridge.is_some_and(|b| b.pending_contains(next_needed));
    if in_bridge_pending || !reorder_buffer.contains_key(&next_needed) {
        return None;
    }
    let mut peeled = 0usize;
    // Peel aggressively when feeder is already deep — ahead pending is what stranded the tip.
    if peel_bridge {
        if let Some(bridge) = ready_bridge {
            peeled = bridge.evict_far_ahead_pending_ex(
                next_needed,
                admit_window.min(64),
                true,
                bridge_pending_max,
                wan_tip_crawl,
            );
            for h in bridge.take_evicted_heights() {
                dispatched.remove(&h);
            }
            if peeled > 0 {
                warn!(
                    "[IBD_TIP_FASTPATH] peeled {peeled} far-ahead bridge pending before tip {next_needed} handoff (feeder={feeder_len})"
                );
            }
        }
    }
    let was_dispatched = dispatched.remove(&next_needed);
    let (block, witnesses) = reorder_buffer.remove(&next_needed)?;
    dispatched.insert(next_needed);
    Some((block, witnesses, peeled, was_dispatched))
}

#[inline]
pub(crate) fn emergency_has_gap_block(
    reorder_buffer: &BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    next_needed: u64,
) -> bool {
    reorder_buffer.contains_key(&next_needed)
}

/// Legacy helper kept for unit tests — bulk recv is allowed when buffer has headroom.
#[inline]
pub(crate) fn emergency_gap_admission_unblocked(
    reorder_buffer: &BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    next_needed: u64,
    pipeline_quarter: usize,
) -> bool {
    emergency_has_gap_block(reorder_buffer, next_needed)
        && emergency_may_bulk_recv(reorder_buffer, pipeline_quarter)
}

/// Height through which periodic engine checkpoint export is deferred during fast local replay.
///
/// Local replay loads bodies from disk and skips heed3 block writes; exporting a UTXO snapshot
/// mid-replay would capture state that does not yet match persisted blocks. Defer only while
/// validation is replaying heights in `[start_height, local_replay_max_height]`.
///
/// Returns **0** when checkpoints should run immediately (resume start already above the
/// RAM-capped local replay window — the prior fallback to `actual_synced_height` deferred
/// export until header tip ~957k and left wm stuck at 230k across restarts).
pub(crate) fn engine_gap_export_defer_until_height(
    start_height: u64,
    local_replay_max_height: u64,
    actual_synced_height: u64,
) -> u64 {
    if local_replay_max_height < start_height {
        return 0;
    }
    local_replay_max_height
        .min(actual_synced_height)
        .max(start_height.saturating_sub(1))
}

/// When the periodic checkpoint export thread may exit so `sync_parallel` can join.
///
/// Do **not** join because `validation_height` or `contiguous_length` reached `end_h`.
/// Skip-path can race the durable tip to `END_HEIGHT` while `last_exported` is still
/// an interval behind (Slice A 2026-08-28T02:22: vh=669992 cl=670000 last_exported=468301
/// → four-hour pause, dirty rewind of ~200k blocks). Gate on the last *committed*
/// export being within one schedule interval of `end_h`. If validation is already at
/// end, the caller keeps looping so exports can catch up.
///
/// `interval <= 0` means "must have committed at `end_h` itself".
/// `end_h <= 0` still exits (no IBD range).
#[inline]
pub(crate) fn checkpoint_export_thread_should_exit(
    _validation_height: u64,
    _contiguous_length: i32,
    end_h: i32,
    last_ckpt: i32,
    interval: i32,
) -> bool {
    if end_h <= 0 {
        return true;
    }
    let near = if interval > 0 {
        end_h.saturating_sub(interval)
    } else {
        end_h
    };
    last_ckpt >= near
}

/// Wall-time threshold used to decide that the last export was "expensive".
///
/// `checkpoint_target_secs` alone is a poor signal: wan-bench sets
/// `BLVM_IBD_CHECKPOINT_TARGET_SECS=300`, so 90–200s mid-chain piggyback exports never
/// triggered scale-up and we exported every ~5k blocks (W173: 10×90–208s exports in
/// ~26 min → almost all wall clock spent in export).
#[inline]
pub(crate) fn export_cost_scale_threshold_secs(target_secs: u64) -> f64 {
    // W175: live W174 restored last_export_wall_secs=81 under TARGET=300; old cap 90
    // treated that as "cheap" → BPS undercut first interval to 7890. Mid-chain piggyback
    // walls are routinely 80–200s — treat ≥60s as expensive.
    target_secs.clamp(45, 60) as f64
}

/// dest-be: piggyback tee walls of 8–38s are still below
/// [`export_cost_scale_threshold_secs`] (60s / W173), so
/// [`bps_scaled_checkpoint_interval_cap`] treated them as cheap and
/// shrank the interval to `validation_bps × target_secs` (~1k at 20 BPS,
/// **500** at 0.1 BPS during compact). LAG_EXEMPT then skipped W176 every
/// interval during WAN crawl (holes≥16 is normal). dest-bc `compact_ms=0`
/// so that undercut was free; dest-be real tee made a 30s export every
/// 500–2k after the 40M UTXO ceiling (`interval=500 utxo_iv=50000` @345853).
/// Below VERY_HIGH_UTXO (100M), a ≥2s last export is a real tee — keep
/// `utxo_iv`. 640M resume tightness still BPS-caps on a cheap 30s export.
pub(crate) const TEE_WALL_SKIP_BPS_UNDERCUT_SECS: f64 = 2.0;
/// dest-bd @83k: overlay 18ms + cheese-sit sample **bps=5.9** → interval=500
/// while `utxo_iv=10000`. LAG_EXEMPT then aligned every 500 into 180k.
/// Overlay sits must not BPS-cap. dest-bc-like 20 BPS overlay may still cap.
pub(crate) const CHECKPOINT_BPS_SIT_FLOOR: f64 = 15.0;
/// W75: defer periodic export while the export-thread 30s EMA is a catch-up
/// burst. dest-x 150–180k ts ~556 / dest-bc ~1579 stayed above this, so
/// `last_exported` stayed 30336 (dest-x) / 41814 (dest-bc) through 180–200k.
/// dest-bi @180000 logged `bps=197.3` and released; dest-bf logged 29.6.
/// Instant IBD prints can dip under 200 (dest-x 180k inst 164.5) while this
/// EMA does not — do not treat a 1k print as the export gate.
pub(crate) const CHECKPOINT_EXPORT_BURST_BPS: f64 = 200.0;
/// Apply 4 / dest-bf: a sit sample (21.8 / 29.6) is still ≥ [`CHECKPOINT_BPS_SIT_FLOOR`]
/// so it used to replace a burst EMA and clear W75. Require this much to *leave*
/// burst; dest-bi 197.3 still may. dest-x 556 stays. Overlay 20 BPS (prev ≪ 200)
/// is unchanged.
pub(crate) const CHECKPOINT_BPS_BURST_CLEAR: f64 = 80.0;
/// LAG_EXEMPT must not fire on a collapsed interval (dest-be 500). dest-bc
/// 96s stall was a 20k interval / 20k lag — that still exempts.
pub(crate) const LAG_EXEMPT_MIN_BLOCKS: i32 = 20_000;
/// dest-be sit-sample / BPS undercut floor. Journal scaling may shrink the
/// HIGH_UTXO 50k *ceiling* but must not re-open the dest-bd 500-step collapse.
pub(crate) const DEST_BE_INTERVAL_FLOOR: i32 = 10_000;
/// Maximum compact/tee input. `checkpoint_allcold_write_ok` AllCold budget is
/// `compact_max (20M) × fan_in (8) = 160M`. Slice A: 313M TeeScan returned in
/// 19 min (`merge_ms=62s sink_ms=1086s`); 448M same path was still scanning at
/// the 20 min disk SIGKILL. Stay at or under this so AllCold can write.
pub(crate) const CHECKPOINT_COMPACT_INPUT_TARGET: u64 = 160_000_000;

/// UTXO-count / export-duration scaled checkpoint interval (blocks between exports).
pub(crate) fn utxo_scaled_checkpoint_interval(
    last_utxo_count: u64,
    last_export_secs: f64,
    durability: &crate::config::ibd::IbdEngineDurabilityConfig,
) -> i32 {
    const BASE_UTXOS: u64 = 25_000_000;
    const BASE_INTERVAL: i32 = 10_000;
    // Mid-chain (~40M+ UTXOs) already pays 90–200s per piggyback export. Prefer the
    // high-UTXO ceiling here rather than waiting until 60M (W173 death band).
    const HIGH_UTXO_THRESHOLD: u64 = 40_000_000;
    const VERY_HIGH_UTXO_THRESHOLD: u64 = 100_000_000;
    let max_interval = if last_utxo_count >= VERY_HIGH_UTXO_THRESHOLD {
        std::env::var("BLVM_IBD_CHECKPOINT_MAX_INTERVAL_HIGH_UTXO")
            .ok()
            .and_then(|s| s.parse::<i32>().ok())
            .filter(|&n| n >= durability.checkpoint_min_interval)
            .unwrap_or(80_000)
            .max(durability.checkpoint_max_interval)
    } else if last_utxo_count >= HIGH_UTXO_THRESHOLD {
        std::env::var("BLVM_IBD_CHECKPOINT_MAX_INTERVAL_HIGH_UTXO")
            .ok()
            .and_then(|s| s.parse::<i32>().ok())
            .filter(|&n| n >= durability.checkpoint_min_interval)
            .unwrap_or(50_000)
            .max(durability.checkpoint_max_interval)
    } else {
        durability.checkpoint_max_interval
    };
    let mut iv = if last_utxo_count >= HIGH_UTXO_THRESHOLD {
        // Export wall dominates mid/late chain — sparse checkpoints.
        max_interval
    } else if last_utxo_count == 0 {
        BASE_INTERVAL.min(max_interval)
    } else {
        // Grow interval with UTXO count (more UTXOs → rarer exports). The old
        // `BASE * BASE_UTXOS / count` formula *shrank* toward min as the set grew,
        // which is the opposite of export cost (W173: 28M→58M UTXOs, ~5k interval).
        let scaled = (BASE_INTERVAL as u64).saturating_mul(last_utxo_count.max(1)) / BASE_UTXOS;
        (scaled as i32).max(BASE_INTERVAL)
    };
    iv = iv.clamp(durability.checkpoint_min_interval, max_interval);
    let scale_threshold = export_cost_scale_threshold_secs(durability.checkpoint_target_secs);
    if last_export_secs >= scale_threshold && last_export_secs > 0.0 {
        let scale = (last_export_secs / scale_threshold).ceil() as i32;
        iv = (iv * scale.max(2)).clamp(durability.checkpoint_min_interval, max_interval);
    }
    iv
}

/// Cap interval from live validation BPS so wall-clock gap between checkpoints stays bounded.
/// Slower BPS → smaller block interval (more frequent checkpoints). Returns `ceiling` when
/// `validation_bps` is unknown (warmup).
pub(crate) fn bps_scaled_checkpoint_interval_cap(
    validation_bps: f64,
    target_wall_secs: u64,
    min_interval: i32,
    ceiling: i32,
) -> i32 {
    if validation_bps <= 0.0 || target_wall_secs == 0 {
        return ceiling;
    }
    let raw = (validation_bps * target_wall_secs as f64).round() as i32;
    raw.clamp(min_interval, ceiling)
}

/// Combined adaptive interval: UTXO/export cost sets the floor/ceiling; BPS may shrink
/// it only when the last export was cheap (resume-window tightness). When exports already
/// cost minutes, BPS must not undercut back to ~5k-block thrash (W173).
pub(crate) fn adaptive_checkpoint_interval(
    last_utxo_count: u64,
    last_export_secs: f64,
    validation_bps: f64,
    durability: &crate::config::ibd::IbdEngineDurabilityConfig,
) -> i32 {
    if let Some(fixed) = durability.checkpoint_interval {
        return fixed;
    }
    let utxo_iv = utxo_scaled_checkpoint_interval(last_utxo_count, last_export_secs, durability);
    let scale_threshold = export_cost_scale_threshold_secs(durability.checkpoint_target_secs);
    if crate::node::parallel_ibd::memory::ibd_pressure_level_snapshot()
        >= crate::node::parallel_ibd::memory::PressureLevel::Critical
    {
        let iv = if last_export_secs >= scale_threshold {
            utxo_iv.min(10_000).max(durability.checkpoint_min_interval)
        } else {
            durability.checkpoint_min_interval
        };
        crate::node::parallel_ibd::memory::log_pressure_behavior(
            "checkpoint_interval",
            "shrink_for_pressure",
            &format!("interval={iv} utxo_iv={utxo_iv}"),
        );
        return iv;
    }
    if last_export_secs >= scale_threshold {
        return utxo_iv;
    }
    // dest-be: 8–38s tee is "cheap" vs W173's 60s bar. First floor was
    // <40M only — crossing HIGH_UTXO re-enabled BPS undercut to min 500
    // (`interval=500 utxo_iv=50000` @345853, bps=0.1 during compact).
    // 640M resume tightness still BPS-caps (`VERY_HIGH` 100M).
    const VERY_HIGH_UTXO_THRESHOLD: u64 = 100_000_000;
    if last_utxo_count < VERY_HIGH_UTXO_THRESHOLD
        && last_export_secs >= TEE_WALL_SKIP_BPS_UNDERCUT_SECS
    {
        return utxo_iv;
    }
    // dest-bd: 18ms overlay + sit sample 5.9 BPS → min_interval 500. Keep
    // utxo_iv so LAG_EXEMPT cannot align a 500-block cadence.
    if last_utxo_count < VERY_HIGH_UTXO_THRESHOLD
        && validation_bps > 0.0
        && validation_bps < CHECKPOINT_BPS_SIT_FLOOR
    {
        return utxo_iv;
    }
    let bps_iv = bps_scaled_checkpoint_interval_cap(
        validation_bps,
        durability.checkpoint_target_secs,
        durability.checkpoint_min_interval,
        utxo_iv,
    );
    utxo_iv.min(bps_iv)
}

/// Keep the last healthy export-thread BPS through a cheese sit.
/// dest-bd logged `bps=5.9` from a 30s window that included a hole; that
/// sample must not replace ~55–135.
/// Apply 4 (2026-08-26): 181–190k 10k-print ts **192**, then `bps=21.8` at
/// 190k export start (`engine_height=191859`). The 30s EMA that held W75
/// 130s was >200; sit floor 15 then let 21.8 replace it and dump on
/// covering=0. Burst EMA (>200) only yields to a sample ≥80.
pub(crate) fn adopt_checkpoint_bps_sample(prev: f64, sample: f64) -> f64 {
    if prev > CHECKPOINT_EXPORT_BURST_BPS
        && sample <= CHECKPOINT_EXPORT_BURST_BPS
        && sample < CHECKPOINT_BPS_BURST_CLEAR
    {
        return prev;
    }
    if sample >= CHECKPOINT_BPS_SIT_FLOOR || prev <= 0.0 {
        sample
    } else {
        prev
    }
}

/// Arm the 30s BPS window at the current validation height.
/// Apply 6 (sit-keep on): `bps_sample_vh=0` at resume 180000 sampled
/// `vh/30s` ≈6000 BPS, then sit <80 could not clear W75 (`last_exported`
/// stayed 180000 at 200k). Genesis still arms at first nonzero vh.
pub(crate) fn checkpoint_bps_arm_sample_origin(prev_origin: u64, vh_now: u64) -> Option<u64> {
    if prev_origin == 0 && vh_now > 0 {
        Some(vh_now)
    } else {
        None
    }
}

/// W75 burst gate used by the checkpoint export thread.
#[inline]
pub(crate) fn checkpoint_export_defer_for_burst_bps(validation_bps: f64) -> bool {
    validation_bps > CHECKPOINT_EXPORT_BURST_BPS
}

/// Do not schedule exports on a BPS-collapsed interval while UTXOs are still
/// below the 640M resume band. dest-bd `interval=500 utxo_iv=10000` aligned
/// `last+500` and LAG_EXEMPT'd it; dest-be 640M still BPS-caps (`interval`).
pub(crate) fn checkpoint_schedule_interval(
    interval: i32,
    utxo_iv: i32,
    last_utxo_count: u64,
) -> i32 {
    const VERY_HIGH_UTXO_THRESHOLD: u64 = 100_000_000;
    if last_utxo_count < VERY_HIGH_UTXO_THRESHOLD && interval < utxo_iv {
        utxo_iv
    } else {
        interval
    }
}

/// Shrink the scheduled block interval when the DiskIndex journal exceeds
/// [`CHECKPOINT_COMPACT_INPUT_TARGET`]. dest-be floor is the lower bound
/// (sit-sample collapse). A constant 50k HIGH_UTXO ceiling let 50k blocks of
/// add-delete churn pile up to 448M (Slice A 449546).
pub(crate) fn journal_scaled_checkpoint_interval(
    schedule_iv: i32,
    journal_entries: u64,
    dest_be_floor: i32,
) -> i32 {
    if schedule_iv <= 0 {
        return schedule_iv;
    }
    let floor = dest_be_floor.max(1);
    if journal_entries <= CHECKPOINT_COMPACT_INPUT_TARGET {
        return schedule_iv;
    }
    let scaled = (schedule_iv as u128).saturating_mul(CHECKPOINT_COMPACT_INPUT_TARGET as u128)
        / journal_entries.max(1) as u128;
    (scaled as i32).clamp(floor, schedule_iv)
}

/// Keep the next checkpoint at or above [`gc_fence_high_water`].
///
/// Journal scaling shrinks the interval so a huge cold journal exports sooner.
/// The scoreboard has already advanced the GC fence up to the previous target,
/// and that high water only moves forward. A shrunk slot under the fence is
/// not a legal snapshot (Deletes at or below the fence may already be gone),
/// and waiting for the following alignment leaves the journal in place
/// (R-376: fence 2_760_000, interval 200_000 → 106_593, 31 minutes at 2–8 BPS).
/// The soonest complete checkpoint is the fence itself, so the interval must
/// not shrink below `fence - last_exported`.
pub(crate) fn checkpoint_interval_respecting_fence(
    last_exported: i32,
    schedule_iv: i32,
    fence_hw: i32,
) -> i32 {
    if schedule_iv <= 0 || fence_hw <= last_exported {
        return schedule_iv;
    }
    schedule_iv.max(fence_hw.saturating_sub(last_exported))
}

/// Whether periodic export may label a snapshot at `ckpt` given current validation height.
///
/// `contiguous_length` alone is insufficient: leftover segments/sidecar can report a high CL
/// while validation has only applied a few thousand blocks after a genesis re-open.
#[inline]
pub(crate) fn checkpoint_export_validation_caught_up(ckpt: i32, validation_height: u64) -> bool {
    ckpt > 0 && validation_height >= ckpt as u64
}

/// Next checkpoint height ≤ `contiguous_length`, stepping from `last_exported` (not global zero).
pub(crate) fn aligned_checkpoint_height(
    contiguous_length: i32,
    last_exported: i32,
    interval: i32,
) -> i32 {
    if interval <= 0 || contiguous_length <= last_exported {
        return last_exported;
    }
    let span = contiguous_length - last_exported;
    let steps = span / interval;
    if steps <= 0 {
        return last_exported;
    }
    last_exported.saturating_add(steps.saturating_mul(interval))
}

/// F-C2: whether the already-persisted skip path should advance durable `chain_info` tip.
///
/// Historically every 1000 heights. Near `effective_end` there may be no `% 1000` boundary
/// (live: tip stuck at 957632 while validation reached 957804) — advance every block in the
/// last 1024 heights of the IBD range.
#[inline]
pub(crate) fn should_advance_tip_on_skip_path(next_height: u64, effective_end_height: u64) -> bool {
    if next_height == 0 {
        return false;
    }
    if next_height % 1_000 == 0 {
        return true;
    }
    next_height.saturating_add(1_024) > effective_end_height
}

/// F-C4: mid-IBD soak tip-follow — extend `effective_end` as peers advance (RC-B).
///
/// Without this, `target_height` is frozen at startup and WAN windows stay ~seconds after
/// long local replay. Enable via `BLVM_IBD_FOLLOW_TIP=1` (soak script sets this).
#[inline]
pub(crate) fn ibd_follow_tip_enabled() -> bool {
    match std::env::var("BLVM_IBD_FOLLOW_TIP") {
        Ok(v) => !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("no")),
        Err(_) => false,
    }
}

#[inline]
pub(crate) fn ibd_follow_tip_poll_secs() -> u64 {
    std::env::var("BLVM_IBD_FOLLOW_TIP_POLL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(30)
}

/// Follow only when apply has food **and** validation is near the assigned
/// end. R-238 dump 20–30k **948**: `[IBD_TIP_FOLLOW_TIMEOUT]` parked the
/// coordinator **5s** at `tip=25493` FILLED_RUNWAY `feeder=0` `reorder=128`.
/// END=370k cannot extend past the cap — the await is a no-op that still
/// stalls Case B. dest-bc 0–10k **298** lives (skip cannot 3000@10k-kill).
pub(crate) const TIP_FOLLOW_NEAR_END: u64 = 10_000;

pub(crate) fn tip_follow_may_block_coord(
    feeder_len: u64,
    next_needed: u64,
    current_end: u64,
    end_cap: Option<u64>,
) -> bool {
    if feeder_len == 0 {
        return false;
    }
    if end_cap.is_some_and(|c| current_end >= c) {
        return false;
    }
    current_end.saturating_sub(next_needed) < TIP_FOLLOW_NEAR_END
}

/// Hard validation/download end for local/synth soaks (`BLVM_IBD_END_HEIGHT`).
///
/// `WAN_BENCH_STOP_H` alone only stops the watch script — without this, synth
/// headers still drive `effective_end` to chain tip (~959k) and validation
/// crawls past the measured band while a piggyback export runs.
#[inline]
pub(crate) fn ibd_end_height_cap() -> Option<u64> {
    std::env::var("BLVM_IBD_END_HEIGHT")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&h| h > 0)
}

#[inline]
pub(crate) fn apply_ibd_end_height_cap(height: u64) -> u64 {
    match ibd_end_height_cap() {
        Some(cap) => height.min(cap),
        None => height,
    }
}

/// Compute new effective end when peer tip advances (pure logic for tests).
#[inline]
pub(crate) fn tip_follow_new_effective_end(
    current_end: u64,
    peer_tip: u64,
    header_tip: u64,
) -> Option<u64> {
    if peer_tip <= current_end {
        return None;
    }
    // Never assign/download past stored headers (W35‴-h).
    let capped = apply_ibd_end_height_cap(peer_tip.min(header_tip.max(current_end)));
    if capped > current_end {
        Some(capped)
    } else {
        None
    }
}

/// Disables IBD nosync mode and issues a final flush when `sync_parallel` exits (on any path).
struct IbdNosyncGuard<'a>(&'a crate::storage::Storage);

impl Drop for IbdNosyncGuard<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.0.set_ibd_nosync(false) {
            warn!("IBD nosync disable failed on exit: {e}");
        }
    }
}
use dashmap::DashMap;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use tokio::sync::Semaphore;
use tokio::sync::broadcast;
use tokio::time::{Duration, timeout};
use tracing::{debug, error, info, warn};

/// Parallel IBD configuration
#[derive(Debug, Clone)]
pub struct ParallelIBDConfig {
    /// Network (mainnet/testnet/regtest) for consensus rule selection
    pub network: blvm_protocol::types::Network,
    /// Number of parallel workers (default: CPU count)
    pub num_workers: usize,
    /// Chunk size in blocks (default: 16)
    pub chunk_size: u64,
    /// Maximum concurrent downloads per peer (default: 64)
    pub max_concurrent_per_peer: usize,
    /// Checkpoint interval in blocks (default: 10,000)
    pub checkpoint_interval: u64,
    /// Timeout for block download in seconds (default: 30)
    pub download_timeout_secs: u64,
    /// Preferred peer addresses (ENV > config > empty)
    pub preferred_peers: Vec<String>,
    /// Mode: parallel, sequential, earliest (default: parallel)
    pub mode: String,
    /// Max blocks download can race ahead (None = auto from RAM)
    pub max_ahead_blocks: Option<u64>,
    /// Skip disk reads during IBD from genesis (default: false)
    pub memory_only: bool,
    /// Failure dump directory (None = platform temp)
    pub dump_dir: Option<String>,
    /// Snapshot directory for debug dumps (None = unset)
    pub snapshot_dir: Option<String>,
    /// Tokio yield interval (default: 1000)
    pub yield_interval: u64,
    /// Eviction: dynamic, fifo, lifo (default: fifo)
    pub eviction: String,
    /// Assign all chunks to fastest peer (default: false)
    pub earliest_first: bool,
    /// Prefetch workers (None = auto from nproc)
    pub prefetch_workers: Option<usize>,
    /// Prefetch queue size (None = auto)
    pub prefetch_queue_size: Option<usize>,
    /// UTXO prefetch lookahead (default: 64)
    pub utxo_prefetch_lookahead: u64,
    /// Max blocks in transit per peer (default: 16)
    pub max_blocks_in_transit_per_peer: usize,
    /// Headers download timeout (seconds, default: 30)
    pub headers_timeout_secs: u64,
    /// Headers max failures before peer switch (default: 10)
    pub headers_max_failures: u32,
    /// Dedicated headless IBD — larger RSS envelope (`[ibd] dedicated` or BLVM_IBD_EXCLUSIVE).
    pub dedicated: bool,
    /// Age-tiered UTXO engine (resolved from env + `[ibd].utxo_engine` in `from_config`).
    pub utxo_engine: bool,
}

impl Default for ParallelIBDConfig {
    fn default() -> Self {
        Self::from_config(None)
    }
}

/// When `BLVM_IBD_PEERS` pins download peers (archive tip-now / LAN), skip public
/// archive DNS seeding. DNS discovery was racing pinned `127.0.0.1:18333` and
/// setting `target_height` to public tip (~1.4M) + polluting GetData (supply health).
pub(crate) fn skip_ibd_archive_dns_seed() -> bool {
    ParallelIBDConfig::ibd_peers_env_explicit() || crate::network::ibd_peers_pinned()
}

/// R-346: connected-peer count the IBD session keeps topping the roster up to
/// (`BLVM_IBD_ROSTER_TARGET`; default `BLVM_TARGET_OUTBOUND_PEERS` if set, else **96**;
/// 0 disables). The roster was filled once at startup and then only drained: R-341 ready
/// 45 → 38, R-345 40 → 34 with 7–16 benched, busy 21 at 300k. Block wire rate is
/// Σ per-peer bandwidth (~1.1–1.5 MB/s per busy peer in every band, flat across 0.6–2.1 MB
/// tiles), so every peer the roster loses is ~5 BPS at 300k. R-346 ran 96: ready 60,
/// busy 54, block wire 65–70 MB/s (R-345 32–41), 200–370k **283.2** PASS — locked in.
pub(crate) fn ibd_roster_target() -> Option<usize> {
    let raw = std::env::var("BLVM_IBD_ROSTER_TARGET")
        .ok()
        .or_else(|| std::env::var("BLVM_TARGET_OUTBOUND_PEERS").ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(96);
    (raw > 0).then_some(raw.min(100))
}

/// Seconds between roster top-up checks (`BLVM_IBD_ROSTER_TOPUP_SECS`, default 30, min 10).
/// R-346 at 60 s: `peers=` 52 → 66 over 11 ticks, never 96 — each tick connected 3–10 of
/// the 30–44 needed (stale address-DB candidates). First check runs after 10 s so a node
/// that started with the 8-peer default is not stuck on it for a whole interval.
pub(crate) fn ibd_roster_topup_secs() -> u64 {
    std::env::var("BLVM_IBD_ROSTER_TOPUP_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(30)
        .max(10)
}

impl ParallelIBDConfig {
    /// True when `BLVM_IBD_PEERS` is set to a non-empty value (empty string = unset).
    fn ibd_peers_env_explicit() -> bool {
        std::env::var("BLVM_IBD_PEERS")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }

    /// Build config from optional IbdConfig. ENV overrides config file.
    pub fn from_config(ibd_config: Option<&crate::config::IbdConfig>) -> Self {
        let chunk_size = std::env::var("BLVM_IBD_CHUNK_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(|n: u64| n.clamp(16, 2000))
            .or_else(|| ibd_config.map(|c| c.chunk_size))
            .unwrap_or(128);
        let download_timeout_secs = std::env::var("BLVM_IBD_DOWNLOAD_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.map(|c| c.download_timeout_secs))
            .unwrap_or(30);
        let preferred_peers = std::env::var("BLVM_IBD_PEERS")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect()
            })
            .or_else(|| {
                ibd_config
                    .filter(|c| !c.preferred_peers.is_empty())
                    .map(|c| c.preferred_peers.clone())
            })
            .unwrap_or_default();
        let mode = std::env::var("BLVM_IBD_MODE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| ibd_config.map(|c| c.mode.clone()))
            .unwrap_or_else(|| "parallel".to_string());
        let max_ahead_blocks = std::env::var("BLVM_IBD_MAX_AHEAD")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.and_then(|c| c.max_ahead_blocks));
        let memory_only = std::env::var("BLVM_IBD_MEMORY_ONLY")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or_else(|_| ibd_config.map(|c| c.memory_only).unwrap_or(false));
        let dump_dir = std::env::var("BLVM_IBD_DUMP_DIR")
            .ok()
            .or_else(|| ibd_config.and_then(|c| c.dump_dir.clone()));
        let snapshot_dir = std::env::var("BLVM_IBD_SNAPSHOT_DIR")
            .ok()
            .or_else(|| ibd_config.and_then(|c| c.snapshot_dir.clone()));
        let yield_interval = std::env::var("BLVM_IBD_YIELD_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.map(|c| c.yield_interval))
            .unwrap_or(1000);
        let eviction = std::env::var("BLVM_IBD_EVICTION")
            .ok()
            .or_else(|| ibd_config.map(|c| c.eviction.clone()))
            .unwrap_or_else(|| "fifo".to_string());
        let earliest_first = std::env::var("BLVM_IBD_EARLIEST_FIRST")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or_else(|_| ibd_config.map(|c| c.earliest_first).unwrap_or(false));
        let prefetch_workers = std::env::var("BLVM_PREFETCH_WORKERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n: &usize| n > 0 && n <= 64)
            .or_else(|| ibd_config.and_then(|c| c.prefetch_workers));
        let prefetch_queue_size = std::env::var("BLVM_PREFETCH_QUEUE_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.and_then(|c| c.prefetch_queue_size));
        let utxo_prefetch_lookahead = std::env::var("BLVM_UTXO_PREFETCH_LOOKAHEAD")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| ibd_config.map(|c| c.utxo_prefetch_lookahead))
            .unwrap_or(128)
            // Allow up to 512 blocks lookahead on high-RAM machines. The old cap of 128 was
            // conservative; the prefetch pool is bounded by max_prefetches_in_flight (derived
            // from available RAM) so increasing the lookahead distance only helps on machines
            // where that pool is large enough to accommodate more in-flight prefetch jobs.
            .clamp(1, 512);
        // Env wins (peel/Mode T: BLVM_IBD_MAX_BLOCKS_IN_TRANSIT=16). T1 toml-only
        // dropped that and silently used 128 whenever `[ibd]` was missing.
        let max_blocks_in_transit = std::env::var("BLVM_IBD_MAX_BLOCKS_IN_TRANSIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(|n: usize| n.clamp(1, 512))
            .or_else(|| ibd_config.map(|c| c.max_blocks_in_transit_per_peer))
            .unwrap_or(128);
        info!(
            "IBD: max_blocks_in_transit_per_peer={} (env>toml>128)",
            max_blocks_in_transit
        );
        let headers_timeout = std::env::var("BLVM_IBD_HEADERS_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.map(|c| c.headers_timeout_secs))
            .unwrap_or(5); // 5s: fail fast on unresponsive peers; parallel download handles retries
        let headers_max_failures = std::env::var("BLVM_IBD_HEADERS_MAX_FAILURES")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.map(|c| c.headers_max_failures))
            .unwrap_or(10);
        let dedicated = std::env::var("BLVM_IBD_EXCLUSIVE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or_else(|_| {
                std::env::var("BLVM_DEDICATED_NODE")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or_else(|_| ibd_config.map(|c| c.dedicated).unwrap_or(false))
            });
        Self {
            network: blvm_protocol::types::Network::Mainnet,
            num_workers: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            chunk_size,
            max_concurrent_per_peer: 64,
            checkpoint_interval: 10_000,
            download_timeout_secs,
            preferred_peers,
            mode,
            max_ahead_blocks,
            memory_only,
            dump_dir,
            snapshot_dir,
            yield_interval,
            eviction,
            earliest_first,
            prefetch_workers,
            prefetch_queue_size,
            utxo_prefetch_lookahead,
            max_blocks_in_transit_per_peer: max_blocks_in_transit,
            headers_timeout_secs: headers_timeout,
            headers_max_failures,
            dedicated,
            utxo_engine: crate::config::ibd::ibd_engine_enabled(ibd_config),
        }
    }

    /// Build config for an IBD session: file/ENV defaults, then LAN auto-prefer.
    /// Does not change `[ibd].mode` — parallel stays parallel unless set in config/ENV.
    pub fn resolve_for_session(
        ibd_config: Option<&crate::config::IbdConfig>,
        _synced_chain_height: u64,
        peer_addresses: &[String],
    ) -> Self {
        let mut config = Self::from_config(ibd_config);

        if config.preferred_peers.is_empty() && !Self::ibd_peers_env_explicit() {
            let lan_peers: Vec<String> = peer_addresses
                .iter()
                .filter(|p| {
                    p.parse::<SocketAddr>()
                        .ok()
                        .is_some_and(|a| is_lan_peer(&a))
                })
                .cloned()
                .collect();
            if !lan_peers.is_empty() {
                info!(
                    "IBD: auto-preferring {} LAN peer(s) for download (set BLVM_IBD_PEERS to override): {}",
                    lan_peers.len(),
                    lan_peers.join(", ")
                );
                config.preferred_peers = lan_peers;
            }
        }

        config
    }

    /// Minimum connected peers before starting block download.
    ///
    /// Always 1: IBD can proceed with a single full-history peer and the peer-watcher task will
    /// dynamically add more workers as new peers connect.  Requiring 2+ peers causes an
    /// indefinite stall when the address database contains mostly pruned nodes — the node keeps
    /// spawning reconnection tasks that find pruned peers, never reaching the threshold, while the
    /// 1 good peer sits idle.  With min=1 we start immediately and scale up organically.
    pub fn min_peers_for_ibd(&self) -> usize {
        // Local-replay / offline soak: bodies already on disk; no GetData peers required.
        if synthetic_wan::allow_zero_real_peers() {
            return 0;
        }
        1
    }

    /// WAN-only multi-peer mode: true when multiple WAN peers are available.
    /// Default ON. Set BLVM_IBD_WAN_SINGLE_PEER=1 to force single-peer.
    fn is_wan_only_multi_peer(peers: &[String]) -> bool {
        let all_wan = peers.len() > 1
            && peers.iter().all(|p| {
                p.parse::<SocketAddr>()
                    .ok()
                    .is_none_or(|a| !is_lan_peer(&a))
            });
        all_wan
            && !std::env::var("BLVM_IBD_WAN_SINGLE_PEER")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
    }

    /// WAN-only: collapse to single fastest peer only if BLVM_IBD_WAN_SINGLE_PEER=1.
    fn collapse_wan_only_download_peers(mut peers: Vec<String>) -> Vec<String> {
        if Self::is_wan_only_multi_peer(&peers) {
            return peers; // multi-peer: no collapse
        }
        // Single-peer forced or only one peer
        if peers.len() > 1
            && peers.iter().all(|p| {
                p.parse::<SocketAddr>()
                    .ok()
                    .is_none_or(|a| !is_lan_peer(&a))
            })
        {
            peers.truncate(1);
        }
        peers
    }
}

/// When `preferred` is non-empty, restrict to matching connected peers.
/// If none match yet, warn and fall back to all connected peers so IBD can proceed.
pub(crate) fn filter_ibd_download_peers(
    preferred: &[String],
    connected: Vec<String>,
) -> Vec<String> {
    if preferred.is_empty() {
        return connected;
    }
    let matches_preferred = |peer: &str| -> bool {
        preferred.iter().any(|pref| {
            peer == pref.as_str()
                || (!pref.contains(':')
                    && peer.starts_with(pref)
                    && peer.as_bytes().get(pref.len()) == Some(&b':'))
        })
    };
    let matched: Vec<String> = connected
        .iter()
        .filter(|p| matches_preferred(p.as_str()))
        .cloned()
        .collect();
    if matched.is_empty() {
        warn!(
            "IBD preferred_peers={preferred:?} but none are connected yet (connected: {connected:?}); \
             continuing with all connected peers"
        );
        connected
    } else if matched.len() < 2 && connected.len() >= 2 {
        // One preferred peer connected is worse than zero: it disables WAN multi-peer
        // (collapse to single-peer) and drops BPS from ~100 to ~5 on sparse replay.
        warn!(
            "IBD preferred_peers matched only {} of {} connected peer(s) ({}) — \
             need >=2 for parallel download; using all {} connected peers",
            matched.len(),
            preferred.len(),
            matched.join(", "),
            connected.len()
        );
        connected
    } else {
        info!(
            "IBD preferred_peers: using {} ({})",
            matched.len(),
            matched.join(", ")
        );
        matched
    }
}

/// Options for [`ParallelIBD::do_flush_to_storage`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct IbdBlockFlushOpts {
    /// Use rayon for bincode serialization. Disabled on the validation-thread shutdown path
    /// to avoid pool contention / nested-par_iter deadlocks while P2P is still active.
    pub parallel_serialize: bool,
    /// Emit `info!` per 50-block chunk (terminal flush diagnostics).
    pub log_progress: bool,
}

impl Default for IbdBlockFlushOpts {
    fn default() -> Self {
        Self {
            parallel_serialize: true,
            log_progress: false,
        }
    }
}

impl IbdBlockFlushOpts {
    pub(crate) fn shutdown_sync() -> Self {
        Self {
            parallel_serialize: false,
            log_progress: true,
        }
    }
}

/// All state captured by one IBD download worker.
/// Bundled into a struct so the initial spawn loop and the dynamic peer-watcher task
/// can share the same worker function without code duplication.
#[cfg(feature = "production")]
struct IbdWorkerCtx {
    peer_id: String,
    config: ParallelIBDConfig,
    blockstore: Arc<crate::storage::blockstore::BlockStore>,
    network: Option<Arc<NetworkManager>>,
    tx: tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>,
    peer_scorer: Arc<crate::network::peer_scoring::PeerScorer>,
    assigner: Arc<ChunkAssigner>,
    workers_current_chunks: Arc<tokio::sync::Mutex<Vec<(String, u64, u64)>>>,
    num_peers: usize,
    peer_blocks_semaphores: Arc<DashMap<String, Arc<Semaphore>>>,
    max_ahead_live: Arc<AtomicU64>,
    ibd_pv: blvm_protocol::ProtocolVersion,
    stall_rx: broadcast::Receiver<u64>,
    semaphore: Arc<Semaphore>,
    validation_height: Arc<AtomicU64>,
    /// Highest height with block body on disk at IBD start (uncapped; used for local gap fill / stall policy).
    confirmed_body_height: u64,
    /// IBD start height — used to detect when the bootstrap chunk completes.
    ibd_start_height: u64,
    /// WAN multi-peer mode — affects stall-abort policy in download workers.
    wan_multi_peer: bool,
}

/// Download loop for one IBD peer worker.  Shared between the initial spawn loop and
/// the dynamic peer-watcher so replacement workers run exactly the same code path.
#[cfg(feature = "production")]
async fn run_ibd_download_worker(ctx: IbdWorkerCtx) -> anyhow::Result<()> {
    let IbdWorkerCtx {
        peer_id,
        config,
        blockstore,
        network,
        tx,
        peer_scorer,
        assigner,
        workers_current_chunks,
        num_peers,
        peer_blocks_semaphores,
        max_ahead_live,
        ibd_pv,
        mut stall_rx,
        semaphore,
        validation_height,
        confirmed_body_height,
        ibd_start_height,
        wan_multi_peer,
    } = ctx;

    // Rename locals to match the original worker variable names so the body compiles unchanged.
    let blockstore_clone = blockstore;
    let network_clone = network;
    let peer_scorer_clone = peer_scorer;
    let assigner_clone = assigner;
    let workers_current_clone = workers_current_chunks;
    let num_peers_clone = num_peers;
    let peer_blocks_semaphores_clone = peer_blocks_semaphores;
    let max_ahead_live_clone = max_ahead_live;
    let validation_height_clone = validation_height;
    let start_height = ibd_start_height; // for bootstrap detection (start == start_height)
    // R-348: batched off-worker persist (idempotent start).
    persist_lane::start(
        Arc::clone(&blockstore_clone),
        Some(Arc::clone(&validation_height_clone)),
        ibd_pv,
    );

    let mut chunks_completed = 0u64;
    let mut blocks_downloaded = 0u64;
    let mut consecutive_failures = 0u32;
    let mut consecutive_timeout_failures = 0u32;
    // With download_timeout_secs=10, 3 failures = 30s of wasted time before eviction.
    // Reducing from 5→3 cuts the stall window in half for pruned peers.
    // Good peers with transient issues rarely hit 3 consecutive failures.
    const MAX_CONSECUTIVE_FAILURES: u32 = 3;
    // 1 = 10s wasted before eviction. Aggressive: prune detection is fast and
    // we have dynamic peer replacement, so fail-fast is preferable.
    const MAX_CONSECUTIVE_TIMEOUT_FAILURES: u32 = 1;

    loop {
        let maybe_work = loop {
            // Exit early if this worker's own peer has been permanently evicted.
            // Staying alive just causes the worker to grab a chunk, hit the
            // wait_for_peer_connected eviction check, requeue, loop forever.
            if let Some(net) = network_clone.as_ref() {
                if let Ok(peer_sa) = peer_id.parse::<std::net::SocketAddr>() {
                    let evicted = net.ibd_evicted_ips.read().unwrap();
                    if evicted.contains(&peer_sa.ip()) {
                        tracing::info!(
                            "[IBD] Worker {} exiting: peer is permanently evicted (NODE_NETWORK_LIMITED)",
                            peer_id
                        );
                        break None;
                    }
                }
            }
            let got = assigner_clone.get_work(
                &peer_id,
                max_ahead_live_clone.load(std::sync::atomic::Ordering::Relaxed),
            );
            // R-336: window path queues front-stalling holders for eviction; any
            // worker drains it (the assigner has no network handle).
            for evict in assigner_clone.window_take_evictions() {
                if let Some(net) = network_clone.as_ref() {
                    if let Ok(sa) = evict.parse::<std::net::SocketAddr>() {
                        download::download_forget_peer(&evict);
                        let netc = net.clone();
                        tokio::task::spawn_blocking(move || netc.evict_ibd_peer(sa));
                    }
                }
            }
            if let Some((chunk_start, chunk_end)) = got {
                break Some((chunk_start, chunk_end));
            }
            // Leftover-force GetData still needs a worker after the chunk map
            // looks empty (live 70736: leftover cheese done, is_done, nobody
            // took (H,H) WAN handoff).
            let leftover_force = assigner_clone.leftover_force_armed();
            let next = assigner_clone.next_needed_height();
            let body_tip = assigner_clone.wan_body_tip();
            let done = assigner_clone.is_done();
            if leftover_trace_watch(leftover_force, next, body_tip) {
                chunk_assigner::leftover_trace_rate(
                    "get_work_none",
                    format!(
                        "peer={peer_id} leftover_force={leftover_force} is_done={done} next={next} body_tip={body_tip}"
                    ),
                );
            }
            if done && !leftover_force {
                if leftover_trace_watch(leftover_force, next, body_tip) {
                    tracing::warn!(
                        "[IBD_LEFTOVER_TRACE] step=worker_exit peer={peer_id} leftover_force={leftover_force} is_done={done} next={next} body_tip={body_tip} chunks_completed={chunks_completed}"
                    );
                }
                break None;
            }
            tokio::time::sleep(MESSAGE_PROCESSOR_POLL_SLEEP).await;
        };
        let (mut start, mut end) = match maybe_work {
            Some(x) => x,
            None => {
                info!(
                    "[IBD] Worker {} exiting: queue empty (chunks_completed={}, blocks_downloaded={})",
                    peer_id, chunks_completed, blocks_downloaded
                );
                break;
            }
        };
        let stripe0_end = end;
        let mut extra_guards: Vec<ChunkGuard> = Vec::new();
        let mut extra_ranges: Vec<(u64, u64)> = Vec::new();
        let depth = ibd_peer_depth();
        let is_sticky = assigner_clone.preferred_tip_owner().as_deref() == Some(peer_id.as_str());
        if depth > 1 && !is_sticky {
            while extra_guards.len() + 1 < depth {
                match assigner_clone.get_work(
                    &peer_id,
                    max_ahead_live_clone.load(std::sync::atomic::Ordering::Relaxed),
                ) {
                    Some((s, e)) if s == end.saturating_add(1) => {
                        extra_guards.push(ChunkGuard::new(
                            s,
                            e,
                            Some(peer_id.clone()),
                            peer_id.clone(),
                            assigner_clone.clone(),
                        ));
                        extra_ranges.push((s, e));
                        end = e;
                    }
                    Some((s, e)) => {
                        // Disjoint = ahead farm. Do not pack it (A4). Drop the seat.
                        assigner_clone.on_chunk_complete_range(&peer_id, s, e);
                        break;
                    }
                    None => break,
                }
            }
            if end != stripe0_end {
                tracing::warn!(
                    "[IBD_PEER_PIPE] peer={} extras={} span={}-{} — same-peer next-stripe, not A4",
                    peer_id,
                    extra_ranges.len(),
                    start,
                    end
                );
            }
        }
        let mut _guard = Some(ChunkGuard::new(
            start,
            end,
            Some(peer_id.clone()),
            peer_id.clone(),
            assigner_clone.clone(),
        ));
        if start != end {
            info!("[IBD] {} took chunk {}-{}", peer_id, start, end);
        }
        {
            let bt = assigner_clone.wan_body_tip();
            let lf = assigner_clone.leftover_force_armed();
            if lf || (bt > 0 && start <= bt && start.saturating_add(512) > bt) {
                tracing::warn!(
                    "[IBD_LEFTOVER_TRACE] step=worker_took peer={peer_id} start={start} end={end} leftover_force={lf} body_tip={bt} next={}",
                    assigner_clone.next_needed_height()
                );
            }
        }
        workers_current_clone
            .lock()
            .await
            .push((peer_id.clone(), start, end));
        {
            let lf = assigner_clone.leftover_force_armed();
            let next = assigner_clone.next_needed_height();
            let bt = assigner_clone.wan_body_tip();
            if leftover_trace_watch(lf, next, bt) {
                chunk_assigner::leftover_trace_rate(
                    "sema_wait",
                    format!(
                        "peer={peer_id} start={start} end={end} leftover_force={lf} next={next} body_tip={bt}"
                    ),
                );
            }
        }
        let _permit = match semaphore.acquire().await {
            Ok(permit) => permit,
            Err(_) => {
                warn!(
                    "[IBD] Worker {} semaphore acquire failed — ChunkGuard will re-queue",
                    peer_id
                );
                break;
            }
        };
        {
            let lf = assigner_clone.leftover_force_armed();
            let next = assigner_clone.next_needed_height();
            let bt = assigner_clone.wan_body_tip();
            if leftover_trace_watch(lf, next, bt) {
                chunk_assigner::leftover_trace_rate(
                    "sema_ok",
                    format!(
                        "peer={peer_id} start={start} end={end} leftover_force={lf} next={next} body_tip={bt}"
                    ),
                );
            }
        }

        // Bootstrap (start==0): no per-peer semaphore so we don't starve the first chunk.
        let blocks_sem = if start == 0 {
            None
        } else {
            peer_blocks_semaphores_clone
                .get(&peer_id)
                .map(|r| Arc::clone(&*r))
        };
        let validated_tip = validation_height_clone.load(std::sync::atomic::Ordering::Relaxed);
        let resume_from =
            download::resume_download_height(start, end, validated_tip).unwrap_or(start);
        let outer_secs = download::worker_chunk_outer_deadline_secs(
            start,
            end,
            resume_from,
            config.download_timeout_secs,
            confirmed_body_height,
        );
        // S1: cooperative outer deadline inside download_chunk flushes `received` before
        // abort. Safety-net timeout is outer+60s so a wedged select still dies; cooperative
        // path should almost always win (live: bare tokio::timeout dropped buffered blocks).
        let outer_deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(outer_secs);
        let safety_net_secs = outer_secs.saturating_add(60);
        let dl_result = match tokio::time::timeout(
            std::time::Duration::from_secs(safety_net_secs),
            download_chunk(
                start,
                end,
                &peer_id,
                network_clone.clone(),
                &blockstore_clone,
                &config,
                peer_scorer_clone.clone(),
                Some(tx.clone()),
                blocks_sem,
                Some(&mut stall_rx),
                ibd_pv,
                Some(Arc::clone(&validation_height_clone)),
                confirmed_body_height,
                wan_multi_peer,
                Some(outer_deadline),
                Some(Arc::clone(&assigner_clone)),
            ),
        )
        .await
        {
            Ok(r) => r,
            Err(_elapsed) => {
                warn!(
                    "[IBD] chunk {}-{} safety-net outer deadline ({}s, budget {}s) expired — aborting for retry (cooperative flush missed)",
                    start, end, safety_net_secs, outer_secs
                );
                peer_scorer_clone.record_failure(
                    peer_id
                        .parse::<std::net::SocketAddr>()
                        .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap()),
                );
                Err(anyhow::anyhow!(
                    "Chunk {}-{}: outer deadline {}s",
                    start,
                    end,
                    outer_secs
                ))
            }
        };
        workers_current_clone
            .lock()
            .await
            .retain(|(p, s, _)| !(*p == peer_id && *s == start));
        {
            let bt = assigner_clone.wan_body_tip();
            let lf = assigner_clone.leftover_force_armed();
            if lf || (bt > 0 && start <= bt && start.saturating_add(512) > bt) {
                tracing::warn!(
                    "[IBD_LEFTOVER_TRACE] step=download_return peer={peer_id} start={start} end={end} leftover_force={lf} body_tip={bt} ok={} err={}",
                    dl_result.is_ok(),
                    dl_result
                        .as_ref()
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_default()
                );
                tracing::warn!(
                    "[IBD_LEFTOVER_TRACE] step=worker_reloop peer={peer_id} leftover_force={lf} next={} body_tip={bt} is_done={}",
                    assigner_clone.next_needed_height(),
                    assigner_clone.is_done()
                );
            }
        }
        match dl_result {
            Ok(chunk) => {
                consecutive_failures = 0;
                consecutive_timeout_failures = 0;
                let block_count = chunk.block_count();
                if start == 0 {
                    info!(
                        "IBD: bootstrap chunk 0-{} downloaded, coordinator enables parallel when received",
                        end
                    );
                }
                #[cfg(feature = "profile")]
                if block_count > 0
                    && (chunks_completed == 0 || chunks_completed % 10 == 0 || block_count > 400)
                {
                    let remaining = assigner_clone.remaining_count();
                    blvm_protocol::profile_log!(
                        "[IBD_DOWNLOAD] peer={} chunk={}-{} blocks={} assigner_remaining={}",
                        peer_id,
                        start,
                        end,
                        block_count,
                        remaining
                    );
                }
                // Blocks already streamed during download_chunk; no second send needed.
                _guard.as_mut().expect("assigner ChunkGuard").disarm();
                for g in extra_guards.iter_mut() {
                    g.disarm();
                }
                assigner_clone.on_chunk_complete_range(&peer_id, start, stripe0_end);
                for (s, e) in &extra_ranges {
                    assigner_clone.on_chunk_complete_range(&peer_id, *s, *e);
                }
                // R-336: window assigner — these heights are delivered; never re-issue.
                // R-355: only wire blocks are timing samples (local completions poisoned the EMAs).
                let net_blocks = chunk.net_block_count as u64;
                assigner_clone.window_note_complete(&peer_id, start, stripe0_end, net_blocks);
                for (s, e) in &extra_ranges {
                    assigner_clone.window_note_complete(&peer_id, *s, *e, net_blocks);
                }
                // R-289 slow-peer rotation. Eviction otherwise only fires on hard
                // failure, so a slow draw is held for the whole run. Claimed by CAS
                // so exactly one worker rotates per interval.
                if let Some(net) = network_clone.as_ref() {
                    let secs = ibd_peer_rotate_secs();
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let last = IBD_LAST_PEER_ROTATE_MS.load(Ordering::Relaxed);
                    if last == 0 {
                        // Seed the clock; never judge a mesh with no window yet.
                        let _ = IBD_LAST_PEER_ROTATE_MS.compare_exchange(
                            0,
                            now_ms,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        );
                    } else if should_rotate_slow_peer(now_ms, last, secs)
                        && IBD_LAST_PEER_ROTATE_MS
                            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                    {
                        let sticky = assigner_clone.preferred_tip_owner().unwrap_or_default();
                        if let Some(v) =
                            download::download_rotate_slowest(&sticky, ibd_peer_rotate_min_scored())
                            && let Ok(slow_sa) = v.peer.parse::<std::net::SocketAddr>()
                        {
                            warn!(
                                "[IBD_PEER_ROTATE] evict {} recv={:.2}mbps assigned={} seated={} bench={} top3={:.2} sticky={} — roster swap, not bench",
                                v.peer,
                                v.recv_mbps,
                                v.assigned,
                                v.seated,
                                v.bench,
                                v.top3_frac,
                                sticky
                            );
                            // Prune first: if this is a ghost (disconnected, bytes frozen)
                            // it must not be re-picked as slowest on every interval.
                            download::download_forget_peer(&v.peer);
                            let netc = net.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                netc.evict_ibd_peer(slow_sa);
                            })
                            .await;
                        }
                    }
                }
                #[cfg(feature = "profile")]
                {
                    let ts_ms = crate::utils::time::current_timestamp_millis();
                    blvm_protocol::profile_log!(
                        "[IBD_CHUNK_COMPLETE] chunk_start={} chunk_end={} peer={} blocks={} ts_ms={}",
                        start,
                        end,
                        peer_id,
                        block_count,
                        ts_ms
                    );
                }
                assigner_clone.note_ok_chunk_complete(&peer_id, start, end, chunk.net_block_count);
                info!(
                    "[IBD] {} complete chunk {}-{} net={} local={}",
                    peer_id,
                    start,
                    end,
                    chunk.net_block_count,
                    block_count.saturating_sub(chunk.net_block_count)
                );
                if start == start_height {
                    assigner_clone.mark_bootstrap_complete();
                }
                chunks_completed += 1;
                blocks_downloaded += block_count as u64;
                let backoff = all_local_retake_backoff_ms(
                    chunk.net_block_count,
                    start,
                    end,
                    assigner_clone.next_needed_height(),
                );
                if backoff > 0 {
                    crate::node::parallel_ibd::memory::ALL_LOCAL_RETAKE_BACKOFFS
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                }
            }
            Err(e) => {
                let err_str = e.to_string();
                let is_eviction =
                    err_str.contains("evicted") && err_str.contains("NODE_NETWORK_LIMITED");
                if is_eviction {
                    tracing::debug!(
                        "Peer {} chunk {}-{}: eviction fast-fail — requeuing without backoff",
                        peer_id,
                        start,
                        end
                    );
                    assigner_clone.requeue_reason(
                        start,
                        stripe0_end,
                        Some(peer_id.clone()),
                        "eviction",
                    );
                    for (s, e) in &extra_ranges {
                        assigner_clone.requeue_reason(*s, *e, Some(peer_id.clone()), "eviction");
                    }
                    _guard.as_mut().expect("assigner ChunkGuard").disarm();
                    for g in extra_guards.iter_mut() {
                        g.disarm();
                    }
                    assigner_clone.on_chunk_complete_range(&peer_id, start, stripe0_end);
                    for (s, e) in &extra_ranges {
                        assigner_clone.on_chunk_complete_range(&peer_id, *s, *e);
                    }
                    tokio::task::yield_now().await;
                    continue;
                }

                if err_str.contains("tip-dup yield") {
                    assigner_clone.requeue_reason(
                        start,
                        stripe0_end,
                        Some(peer_id.clone()),
                        "tip-dup yield",
                    );
                    for (s, e) in &extra_ranges {
                        assigner_clone.requeue_reason(
                            *s,
                            *e,
                            Some(peer_id.clone()),
                            "tip-dup yield",
                        );
                    }
                    _guard.as_mut().expect("assigner ChunkGuard").disarm();
                    for g in extra_guards.iter_mut() {
                        g.disarm();
                    }
                    assigner_clone.on_chunk_complete_range(&peer_id, start, stripe0_end);
                    for (s, e) in &extra_ranges {
                        assigner_clone.on_chunk_complete_range(&peer_id, *s, *e);
                    }
                    continue;
                }

                consecutive_failures += 1;
                if err_str.contains("Block timeout")
                    || err_str.contains("block timeout")
                    || err_str.contains("no first block in")
                {
                    consecutive_timeout_failures += 1;
                } else {
                    consecutive_timeout_failures = 0;
                }
                // W35‴-h / genesis 280–294k: header lag and tip-enter walk-in are
                // assign artifacts — do not burn the peer toward 3/3 blacklist.
                // dest-ap: cheese `(H,H)` 5s on a ≥80 is the same class — not pruned.
                let header_lag = err_str.contains("headers must be downloaded first")
                    || err_str.contains("header may not be stored");
                let walk_in = err_str.contains("tip-enter walk-in");
                let cheese_hero =
                    assigner_clone.cheese_hero_h_timeout_no_strike(&peer_id, start, end, &err_str);
                if cheese_hero || !download::chunk_fail_counts_toward_blacklist(&err_str) {
                    consecutive_failures = consecutive_failures.saturating_sub(1);
                    if cheese_hero {
                        consecutive_timeout_failures =
                            consecutive_timeout_failures.saturating_sub(1);
                    }
                    warn!(
                        "[IBD_{}] Peer {} failed chunk {}-{} ({}, not counted): {} - will retry",
                        if cheese_hero {
                            "CHEESE_HERO_NOSTRIKE"
                        } else {
                            "WALKIN_NOSTRIKE"
                        },
                        peer_id,
                        start,
                        end,
                        if cheese_hero {
                            "cheese H timeout"
                        } else if header_lag {
                            "header lag"
                        } else if walk_in {
                            "tip-enter walk-in"
                        } else {
                            "not-counted"
                        },
                        e
                    );
                } else {
                    warn!(
                        "Peer {} failed chunk {}-{} ({}/{}): {} - will retry with different peer",
                        peer_id, start, end, consecutive_failures, MAX_CONSECUTIVE_FAILURES, e
                    );
                }
                // W28c/W28d: tip-covering failure clears sticky owner and arms failover —
                // but not tip-enter walk-in aborts (those are ahead partitions, not tip owners).
                // W35‴-h: also skip sticky drop on header lag (not peer fault).
                // Mode T (tc64 2026-08-04): rapid W28c tip re-arm cancels in-flight GetData →
                // "Block channel closed" on the tip span. Treating that as tip-owner fail put
                // the sole archive peer in 15s OWNER_COOLDOWN → covering=0 FORCE storm while
                // ahead cheese sat in reorder (TIP_HOLE_AHEAD). Cancel artifact, not mute.
                let block_chan_closed = err_str.contains("Block channel closed");
                if !err_str.contains("tip-enter walk-in") && !header_lag && !block_chan_closed {
                    let tip_needed = assigner_clone.next_needed_height();
                    if start <= tip_needed && tip_needed <= end {
                        // W103/W110: mute tip-gap CAP / empty soft=0 / no-first-block →
                        // short cooldown + clear W88 episode + arm (H,H) failover.
                        // Live W109b: only `tip-gap timeout` matched mute; Block timeout
                        // and no-first-block used 15s cooldown and skipped WAN failover.
                        if download::tip_covering_fail_is_mute(&err_str) {
                            assigner_clone.note_tip_owner_failed_mute(&peer_id);
                        } else {
                            assigner_clone.note_tip_owner_failed(&peer_id);
                        }
                        // P1-H: on WAN gap, blacklist only connection/handshake hard-fails —
                        // soft tip timeouts rotate without 60s burn (live: 714095 ready=63 idle).
                        let wan_gap =
                            confirmed_body_height > 0 && tip_needed > confirmed_body_height;
                        if wan_gap {
                            let connection_fail = err_str.contains("not connected")
                                || err_str.contains("handshake not complete");
                            if connection_fail {
                                let blacklist_secs = 60u64;
                                assigner_clone.blacklist_peer(
                                    &peer_id,
                                    std::time::Duration::from_secs(blacklist_secs),
                                );
                                warn!(
                                    "[IBD_TIP_PEER] hard-fail peer {} blacklisted {}s on tip {} (P1-H connection)",
                                    peer_id, blacklist_secs, tip_needed
                                );
                            }
                        }
                    }
                }
                // Empty-witness peers keep serving MSG_BLOCK; short-blacklist so gap races
                // rotate to other peers instead of re-winning the same height immediately.
                if err_str.contains("empty-witness") && num_peers_clone > 1 {
                    let blacklist_secs = 120u64;
                    assigner_clone
                        .blacklist_peer(&peer_id, std::time::Duration::from_secs(blacklist_secs));
                    warn!(
                        "[IBD_EMPTY_WITNESS] peer {} blacklisted {}s after empty-witness abort",
                        peer_id, blacklist_secs
                    );
                }
                if err_str.contains("gap height")
                    || err_str.contains("stuck at height")
                    || err_str.contains("Coordinator stall at gap")
                    || err_str.contains("Coordinator stall:")
                    || err_str.contains("empty-witness")
                {
                    let gap_h = assigner_clone.next_needed_height();
                    let micro_exclude = if num_peers_clone > 1 {
                        Some(peer_id.clone())
                    } else {
                        None
                    };
                    if assigner_clone.wan_stall_micro_allowed(gap_h) {
                        assigner_clone.requeue_stall_gaps(gap_h, micro_exclude);
                    }
                }
                let exclude = if num_peers_clone > 1 && assigner_clone.total_chunks() > 1 {
                    Some(peer_id.clone())
                } else {
                    if num_peers_clone == 1 {
                        info!(
                            "[IBD] Single peer: re-queuing chunk {}-{} without exclude (no fallback)",
                            start, end
                        );
                    }
                    None
                };
                if exclude.is_some() {
                    info!(
                        "[IBD] Re-queuing chunk {}-{} exclude={}",
                        start, end, peer_id
                    );
                }
                assigner_clone.requeue_reason(start, end, exclude, "chunk_fail");
                _guard.as_mut().expect("assigner ChunkGuard").disarm();
                assigner_clone.on_chunk_complete_range(&peer_id, start, end);

                if num_peers_clone > 1 && consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    if crate::node::parallel_ibd::export_owner_hold_protects(&peer_id) {
                        warn!(
                            "[IBD_EXPORT_OWNER_HOLD] skip LIMITED/blacklist peer={} during checkpoint export",
                            peer_id
                        );
                        consecutive_failures = 0;
                        consecutive_timeout_failures = 0;
                        continue;
                    }
                    let all_timeouts =
                        consecutive_timeout_failures >= MAX_CONSECUTIVE_TIMEOUT_FAILURES;
                    if all_timeouts {
                        if let (Ok(peer_sa), Some(net)) = (
                            peer_id.parse::<std::net::SocketAddr>(),
                            network_clone.clone(),
                        ) {
                            warn!(
                                "Peer {} exceeded max failures ({} block-timeouts) — evicting as NODE_NETWORK_LIMITED",
                                peer_id, consecutive_timeout_failures
                            );
                            let _ = tokio::task::spawn_blocking(move || {
                                net.evict_ibd_peer(peer_sa);
                            })
                            .await;
                        }
                    } else {
                        let blacklist_secs = 300u64;
                        assigner_clone.blacklist_peer(
                            &peer_id,
                            std::time::Duration::from_secs(blacklist_secs),
                        );
                        warn!(
                            "Peer {} exceeded max failures — blacklisted for {}s (worker stays alive)",
                            peer_id, blacklist_secs
                        );
                    }
                    consecutive_failures = 0;
                    consecutive_timeout_failures = 0;
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }

                if num_peers_clone == 1 && consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    warn!(
                        "Peer {} at {} consecutive failures (single-peer mode) — waiting for reconnect",
                        peer_id, consecutive_failures
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    consecutive_failures = 0;
                    continue;
                }

                let backoff_secs = (1u64 << (consecutive_failures - 1).min(3)).min(8);
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
            }
        }
    }

    info!(
        "Peer {} done: {} chunks, {} blocks",
        peer_id, chunks_completed, blocks_downloaded
    );
    Ok(())
}

/// Parallel IBD coordinator
pub struct ParallelIBD {
    config: ParallelIBDConfig,
    /// Earliest BIP54 activation height from version-bits lock-in along the validated chain (mainnet).
    /// Lock-free: `u64::MAX` sentinel = `None`. The merge semantics (`min` of present values) are
    /// expressible as a lock-free `fetch_min`, eliminating the per-block parking_lot::Mutex
    /// contention that previously serialized 8 validation workers through this code path.
    bip54_activation_from_version_bits: std::sync::atomic::AtomicU64,
    /// Semaphore to limit concurrent chunk downloads per peer (DashMap for dynamic peer replacement)
    peer_semaphores: Arc<DashMap<String, Arc<Semaphore>>>,
    /// Core-style: max blocks in flight per peer (shared across all workers). Prevents 6 workers × 64 pipeline = 384 requests to one peer.
    peer_blocks_semaphores: Arc<DashMap<String, Arc<Semaphore>>>,
    /// Peer scorer for bandwidth-based peer selection
    peer_scorer: Arc<crate::network::peer_scoring::PeerScorer>,
}

struct IbdCoordinatorCtx {
    assigner_for_coord: Arc<ChunkAssigner>,
    block_rx: tokio::sync::mpsc::Receiver<(u64, SharedBlock, SharedWitnesses)>,
    blockstore_for_coord: Arc<BlockStore>,
    bootstrap_end: u64,
    bridge_pending_max: usize,
    chunk_size_for_ahead: u64,
    confirmed_body_height_for_coord: u64,
    coord_buffer_limit: usize,
    coord_engine_mode: bool,
    coord_session_id_for_task: u64,
    download_timeout_secs_for_coord: u64,
    effective_end_live_for_coord: Arc<AtomicU64>,
    feeder_state_for_coord: feeder::FeederState,
    follow_tip_peer_ids_for_coord: Vec<String>,
    gap_fill_tx_v2_for_coord: crossbeam_channel::Sender<PrefetchWorkItemV2>,
    headers_max_failures_for_coord: u32,
    headers_timeout_secs_for_coord: u64,
    ibd_pv_for_coord: blvm_protocol::ProtocolVersion,
    ibd_store_v2_for_coord: Arc<IbdUtxoStore>,
    live_body_tip_for_coord: u64,
    max_ahead_blocks: u64,
    max_ahead_live: Arc<AtomicU64>,
    network_for_coord: Option<Arc<NetworkManager>>,
    peer_scorer_for_coord: Arc<crate::network::peer_scoring::PeerScorer>,
    prefetch_input_tx_v2_for_coord: crossbeam_channel::Sender<PrefetchWorkItemV2>,
    ready_bridge_for_coord: Option<Arc<prefetch::OrderedReadyBridge>>,
    sequential: bool,
    stall_tx_for_coord: broadcast::Sender<u64>,
    start_height: u64,
    validation_height_for_coord: Arc<AtomicU64>,
    wan_multi_peer: bool,
}

struct ProactiveGapUnblock<'a, Inject, Dispatch> {
    admit_window: u64,
    assigner_for_coord: &'a Arc<ChunkAssigner>,
    block_rx: &'a mut tokio::sync::mpsc::Receiver<(u64, SharedBlock, SharedWitnesses)>,
    blockstore_for_coord: &'a Arc<BlockStore>,
    bridge_ahead_since: &'a mut Option<(u64, std::time::Instant)>,
    bridge_pending_max: usize,
    cliff_iter_t0: &'a std::time::Instant,
    coord_engine_mode: bool,
    coord_keys_buf: &'a mut Vec<OutPointKey>,
    coord_tx_ids_buf: &'a mut Vec<Hash>,
    dispatch_to_prefetch: &'a Dispatch,
    dispatched: &'a mut rustc_hash::FxHashSet<u64>,
    dynamic_buffer_limit: usize,
    effective_end_live_for_coord: &'a Arc<AtomicU64>,
    feeder_state_for_coord: &'a feeder::FeederState,
    ibd_store_v2_for_coord: &'a Arc<IbdUtxoStore>,
    live_body_tip_for_coord: u64,
    next_prefetch_height: &'a mut u64,
    ready_bridge_for_coord: &'a Option<Arc<prefetch::OrderedReadyBridge>>,
    reorder_buffer: &'a mut std::collections::BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    tip_inflight_grace: Duration,
    tip_inflight_since: &'a mut Option<(u64, std::time::Instant)>,
    total_received: &'a mut u64,
    try_inject_local_gap: &'a mut Inject,
    validation_height_for_coord: &'a Arc<AtomicU64>,
}

fn proactive_gap_unblock<Inject, Dispatch>(ctx: ProactiveGapUnblock<'_, Inject, Dispatch>)
where
    Inject: FnMut(
        &mut std::collections::BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
        u64,
        &rustc_hash::FxHashSet<u64>,
        bool,
    ) -> bool,
    Dispatch: Fn(
        (
            Arc<IbdUtxoStore>,
            Vec<OutPointKey>,
            Vec<Hash>,
            u64,
            SharedBlock,
            SharedWitnesses,
            bool,
        ),
    ) -> Option<(u64, SharedBlock, SharedWitnesses)>,
{
    let ProactiveGapUnblock {
        admit_window,
        assigner_for_coord,
        mut block_rx,
        blockstore_for_coord,
        bridge_ahead_since,
        bridge_pending_max,
        cliff_iter_t0,
        coord_engine_mode,
        mut coord_keys_buf,
        mut coord_tx_ids_buf,
        dispatch_to_prefetch,
        mut dispatched,
        dynamic_buffer_limit,
        effective_end_live_for_coord,
        feeder_state_for_coord,
        ibd_store_v2_for_coord,
        live_body_tip_for_coord,
        next_prefetch_height,
        ready_bridge_for_coord,
        mut reorder_buffer,
        tip_inflight_grace,
        tip_inflight_since,
        mut total_received,
        try_inject_local_gap,
        validation_height_for_coord,
    } = ctx;
    let val_h = validation_height_for_coord.load(Ordering::Relaxed);
    let next_needed = val_h.saturating_add(1);
    let effective_end_now = effective_end_live_for_coord.load(Ordering::Relaxed);
    if next_needed <= effective_end_now && assigner_for_coord.is_bootstrap_complete() {
        // Hole repair FIRST (before handoff/dispatch): missing bridge_next while
        // tip sits in pending wedges Case C forever (live: 698199 hole, tip 698202).
        if let Some(bridge) = ready_bridge_for_coord {
            let _ = bridge.repair_missing_cursor_hole(next_needed);
        }

        let bridge_waiting = ready_bridge_for_coord
            .as_ref()
            .and_then(|b| b.next_expected())
            == Some(next_needed);
        let in_bridge_pending = ready_bridge_for_coord
            .as_ref()
            .is_some_and(|b| b.pending_contains(next_needed));

        // W22: bridge cursor ahead of validation with tip stranded in pending.
        if let Some(bridge) = ready_bridge_for_coord {
            let _ = bridge.recover_stranded_tip(next_needed);
        }

        // W26b: if validation is ahead of the bridge cursor, fast-forward —
        // do NOT rewind when feeder is briefly empty after a successful pipeline
        // emit (live W26: rewind 640066→640001 wedged resume for 10+ min).
        //
        // Cursor one ahead + tip back in reorder = duplicate GAP_STREAM/inject
        // while validation still holds the tip — discard duplicate, do not rewind
        // (live W26b: 1-height rewind storms capped local replay at ~0.7 BPS).
        // Only rewind when cursor jumped further ahead (real lost ReadyItem).
        if let Some(bridge) = ready_bridge_for_coord {
            let bnext = bridge.next_expected();
            if bnext.is_some_and(|n| n < next_needed) {
                let _ = bridge.fast_forward_cursor_to(next_needed);
            } else if let Some(n) = bnext.filter(|&n| n > next_needed) {
                if reorder_buffer.contains_key(&next_needed) {
                    let tip_in_feeder = {
                        let g = feeder_state_for_coord.0.lock();
                        g.0.get(next_needed).is_some()
                    };
                    // W26b/W39: only discard reorder tip when it is *actually*
                    // in the feeder. Treating `next_expected == tip+1` alone as
                    // "delivered" dropped tip on LOCAL_AHEAD soft-resume while
                    // feeder held 84 ahead blocks and validation froze at tip−1
                    // (live 2026-07-16 h=441599, 2.7k GAP_STREAM_RESEND of tip
                    // discarded as duplicates).
                    if tip_in_feeder {
                        reorder_buffer.remove(&next_needed);
                        dispatched.insert(next_needed);
                    } else if cursor_far_ahead_tip_lost(
                        next_needed,
                        n,
                        false,
                        in_bridge_pending,
                        tip_stage::tip_taken_by_validation(next_needed),
                    ) && bridge.rewind_cursor_to(next_needed)
                    {
                        // R-376: cursor ~60 ahead, pending empty, tip sitting
                        // in reorder. W49 left the cursor for Case B, and
                        // try_emit never logged a re-emit — the body stayed
                        // out of the feeder. Pull the cursor back so Case B
                        // emits at next_expected.
                        warn!(
                            "[IBD_TIP_REWIND] next_expected {} → {} (cursor far ahead, tip in reorder not feeder)",
                            n, next_needed
                        );
                        dispatched.remove(&next_needed);
                    }
                    // else W49: one-ahead, leave cursor; Case B handoff emits tip.
                } else {
                    // W50/W56: tip already left reorder (Case B handoff /
                    // try_emit). Cursor one-ahead is success when tip is in
                    // feeder — or still in-flight (`dispatched`).
                    // Live W55 WAN ~466k @ ~6.5 blk/s: gap_missing=96%,
                    // REWIND~92/1k, RESEND~84/1k, feeder0=62%. W50 checked
                    // feeder/reorder/tip_taken but NOT `dispatched` — Case B
                    // removed tip → prefetch, then this path rewound and
                    // cleared dispatched → LOCAL_GAP reinject fight.
                    let tip_in_feeder = {
                        let g = feeder_state_for_coord.0.lock();
                        g.0.get(next_needed).is_some()
                    };
                    let tip_taken_now = tip_stage::tip_taken_by_validation(next_needed);
                    // Far-ahead + tip absent is a lost emit. The 10s
                    // inflight grace is only for a one-ahead handoff
                    // whose cursor has not moved yet.
                    let far_lost = cursor_far_ahead_tip_lost(
                        next_needed,
                        n,
                        tip_in_feeder,
                        in_bridge_pending,
                        tip_taken_now,
                    );
                    let tip_inflight = !far_lost
                        && dispatched.contains(&next_needed)
                        && tip_inflight_since.is_some_and(|(h, t)| {
                            h == next_needed && t.elapsed() < tip_inflight_grace
                        });
                    if !tip_in_feeder
                        && !in_bridge_pending
                        && !tip_taken_now
                        && !tip_inflight
                        && bridge.rewind_cursor_to(next_needed)
                    {
                        warn!(
                            "[IBD_TIP_REWIND] next_expected {} → {} (cursor ahead, tip absent from reorder+feeder+dispatched)",
                            n, next_needed
                        );
                        dispatched.remove(&next_needed);
                    }
                }
            }
        }

        // Case A: gap block already in bridge pending — force flush.
        if in_bridge_pending {
            *tip_inflight_since = None;
            *bridge_ahead_since = None;
            if let Some(bridge) = ready_bridge_for_coord {
                let _ = bridge.try_flush();
            }
        }

        // W29 tip-SLA: sticky owner produced no tip body within SLA — rotate so
        // the next best peer can take a deep pipeline (live: 97.89 owned ~60% of
        // stall wall time). Short blacklist so the slow peer does not re-win.
        let wan_gap = next_needed > live_body_tip_for_coord;
        // Live 2026-07-15: preferred=None (OPEN_STALL) used unwrap_or(1.0) → 90s
        // SLA while tip stalled 55–80s on soft-retry; floor SLA is 25s.
        let owner_score = assigner_for_coord
            .preferred_tip_owner_score()
            .unwrap_or(0.10);
        if wan_gap
            && !reorder_buffer.contains_key(&next_needed)
            && tip_stage::tip_sla_breached_for_owner_score(owner_score)
        {
            if let Some(prev) = assigner_for_coord.rotate_tip_owner_on_sla() {
                // D4: WAN SLA cooloff must cover worker abort latency after rotate.
                let cooloff = std::time::Duration::from_secs(if wan_gap {
                    tip_stage::tip_sla_secs().saturating_add(30).clamp(60, 120)
                } else {
                    60
                });
                assigner_for_coord.blacklist_peer(&prev, cooloff);
                // W73: force (H,H) tip hole when covering=0 (WAN-safe; no bulk).
                assigner_for_coord.requeue_stall_gaps_force(next_needed, Some(prev.clone()));
                warn!(
                    "[IBD_TIP_SLA] rotated tip owner {} after {}s without body (tip={}, owner_score={:.3}) — blacklisted {}s, released in-flight, re-armed SLA",
                    prev,
                    tip_stage::tip_sla_secs_for_owner_score(owner_score),
                    next_needed,
                    owner_score,
                    cooloff.as_secs()
                );
            } else {
                // W30: clear (H,H) failover claims and force deep-owner re-arm —
                // do NOT arm failover (live: perpetual covering=2/2 treadmill).
                tip_stage::clear_tip_failover();
                assigner_for_coord.clear_all_tip_cover_claims();
                assigner_for_coord.open_tip_owner_slot();
                tip_stage::rearm_tip_sla();
                // W73: force tip hole requeue on WAN when covering collapsed.
                assigner_for_coord.requeue_stall_gaps_force(next_needed, None);
                warn!(
                    "[IBD_TIP_SLA] cleared failover claims — forcing deep owner re-arm after {}s (tip={})",
                    tip_stage::tip_sla_secs(),
                    next_needed
                );
            }
        }

        // Leftover-disk hole (live 2026-08-21): next=70657 under
        // body_tip=70735, leftover stripe covering, feeder=0,
        // no INJECT/HANDOFF for 9+ min. Covering leftover is not
        // a pipeline — reload from disk and Case B this loop.
        // R-223 genesis: persist wrote H (`store=1`) but
        // `live_body_tip` stays 0 (R-197 publisher off), so the
        // cheese-tip gate never fired. Store-has is enough.
        let tip_in_feeder_leftover = {
            let g = feeder_state_for_coord.0.lock();
            g.0.get(next_needed).is_some()
        };
        let store_has_leftover =
            feeder_miss::wanted_body_on_disk(blockstore_for_coord.as_ref(), next_needed);
        let mut leftover_injected = false;
        if leftover_disk_hole_should_inject(
            live_body_tip_for_coord,
            next_needed,
            store_has_leftover,
            reorder_buffer.contains_key(&next_needed),
            in_bridge_pending,
            tip_in_feeder_leftover,
        ) {
            dispatched.remove(&next_needed);
            let injected = try_inject_local_gap(reorder_buffer, val_h, dispatched, false);
            leftover_injected = injected;
            if !injected {
                if stall_may_arm_leftover_force(store_has_leftover) {
                    assigner_for_coord.set_tip_gap_missing(true);
                    assigner_for_coord.set_leftover_force_getdata(true);
                    assigner_for_coord.requeue_stall_gaps_force(next_needed, None);
                }
            } else if leftover_force_survives_disk_inject(
                assigner_for_coord.leftover_force_armed(),
                assigner_for_coord.next_needed_height(),
                live_body_tip_for_coord,
            ) {
                // Keep leftover_force — key on assigner next, not
                // stale coordinator val_h (live 70656 inject vs 70736).
            } else {
                assigner_for_coord.set_leftover_force_getdata(false);
            }
            static LEFTOVER_HOLE_LOG_MS: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let prev = LEFTOVER_HOLE_LOG_MS.load(Ordering::Relaxed);
            if now_ms.saturating_sub(prev) >= 5000 {
                LEFTOVER_HOLE_LOG_MS.store(now_ms, Ordering::Relaxed);
                let (covering, _, _) = assigner_for_coord.tip_flight_diag();
                warn!(
                    "[IBD_LEFTOVER_INJECT] next={} val_h={} assigner_next={} body_tip={} store_has={} injected={} force={} covering={}",
                    next_needed,
                    val_h,
                    assigner_for_coord.next_needed_height(),
                    live_body_tip_for_coord,
                    store_has_leftover,
                    injected,
                    assigner_for_coord.leftover_force_armed(),
                    covering
                );
            }
        }

        // R-260: inject put H in reorder; skip Case B flushed=0 cursor-lie.
        let (flight_tip_leftover, _, _) = assigner_for_coord.tip_flight_split();
        // How long `next_needed` has sat unchanged. In dump this resets
        // every few ms; a real stall (r278a h=176199) holds it for 90s.
        let stalled_ms = {
            static AT_HEIGHT: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(u64::MAX);
            static SINCE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if AT_HEIGHT.swap(next_needed, Ordering::Relaxed) == next_needed {
                now_ms.saturating_sub(SINCE_MS.load(Ordering::Relaxed))
            } else {
                SINCE_MS.store(now_ms, Ordering::Relaxed);
                0
            }
        };
        if leftover_inject_should_feeder(
            leftover_injected,
            tip_in_feeder_leftover,
            flight_tip_leftover,
            stalled_ms,
            next_needed,
        ) {
            if let Some((block, witnesses)) = reorder_buffer.remove(&next_needed) {
                prepare_coord_dispatch_bufs(
                    coord_engine_mode,
                    &block,
                    coord_tx_ids_buf,
                    coord_keys_buf,
                );
                let keys_owned = std::mem::take(coord_keys_buf);
                let tx_ids_owned = std::mem::take(coord_tx_ids_buf);
                let ready: ReadyItem = (
                    next_needed,
                    block,
                    witnesses,
                    keys_owned,
                    prefetch::engine_empty_prefetch_arc(),
                    tx_ids_owned,
                    prefetch::engine_empty_spec_adds(),
                );
                if let Some(bridge) = ready_bridge_for_coord {
                    bridge.force_emit_tip_to_feeder(ready, feeder_state_for_coord);
                    static LEFTOVER_FEEDER_LOG_MS: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let prev = LEFTOVER_FEEDER_LOG_MS.load(Ordering::Relaxed);
                    if now_ms.saturating_sub(prev) >= 5000 {
                        LEFTOVER_FEEDER_LOG_MS.store(now_ms, Ordering::Relaxed);
                        warn!(
                            "[IBD_LEFTOVER_FEEDER] next={} val_h={} store_has={} flight_tip={}",
                            next_needed, val_h, store_has_leftover, flight_tip_leftover
                        );
                    }
                } else {
                    reorder_buffer.insert(next_needed, (ready.1, ready.2));
                }
            }
        }

        // Case B / W19/W26/W29b/W34a/W54: tip in reorder and not in feeder —
        // always hand off (W54: feeder-depth gate deadlocked soft-resume).
        let feeder_len_case_b = IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
        let tip_in_feeder_case_b = {
            let g = feeder_state_for_coord.0.lock();
            g.0.get(next_needed).is_some()
        };
        let case_b_ok = reorder_buffer.contains_key(&next_needed)
            && !tip_in_feeder_case_b
            && !in_bridge_pending;
        if case_b_ok {
            let case_b_t0 = std::time::Instant::now();
            if let Some((block, witnesses, _, was_dispatched)) = prepare_coordinator_tip_handoff(
                next_needed,
                false,
                feeder_len_case_b,
                true,
                reorder_buffer,
                dispatched,
                ready_bridge_for_coord.as_deref(),
                admit_window,
                bridge_pending_max,
                wan_gap,
                tip_in_feeder_case_b,
            ) {
                *tip_inflight_since = Some((next_needed, std::time::Instant::now()));
                if next_needed >= *next_prefetch_height {
                    *next_prefetch_height = next_needed + 1;
                }
                prepare_coord_dispatch_bufs(
                    coord_engine_mode,
                    &block,
                    coord_tx_ids_buf,
                    coord_keys_buf,
                );
                let store = &ibd_store_v2_for_coord;
                let keys_owned = std::mem::take(coord_keys_buf);
                let tx_ids_owned = std::mem::take(coord_tx_ids_buf);
                if let Some((h, block, witnesses)) = dispatch_to_prefetch((
                    Arc::clone(store),
                    keys_owned,
                    tx_ids_owned,
                    next_needed,
                    block,
                    witnesses,
                    coord_engine_mode,
                )) {
                    dispatched.remove(&h);
                    reorder_buffer.insert(h, (block, witnesses));
                }
                if synthetic_wan::bulk_local_disk_stream()
                    && case_b_t0.elapsed() >= Duration::from_millis(500)
                {
                    warn!(
                        "[IBD_CLIFF_CASE_B] tip={} dispatch_ms={} was_dispatched={}",
                        next_needed,
                        case_b_t0.elapsed().as_millis(),
                        was_dispatched,
                    );
                }
                if let Some(bridge) = ready_bridge_for_coord {
                    let flushed = bridge.try_flush();
                    let bnext = bridge.next_expected();
                    let bridge_waiting = bnext == Some(next_needed);
                    let tip_in_feeder = {
                        let g = feeder_state_for_coord.0.lock();
                        g.0.get(next_needed).is_some()
                    };
                    if bnext == Some(next_needed) && !tip_in_feeder {
                        warn!(
                            "[IBD_TIP_HANDOFF_MISS] tip {} still at bridge_next after dispatch (flushed={}, was_dispatched={})",
                            next_needed, flushed, was_dispatched
                        );
                    } else if was_dispatched {
                        warn!(
                            "[IBD_TIP_HANDOFF] forced tip {} into bridge (cleared sticky dispatched, flushed={}, bridge_waiting={}, bridge_next={:?})",
                            next_needed, flushed, bridge_waiting, bnext
                        );
                    } else {
                        info!(
                            "[IBD_TIP_HANDOFF] tip {} → bridge (flushed={}, bridge_next={:?})",
                            next_needed, flushed, bnext
                        );
                    }
                }
            }
        }

        // I3 Case D: gap height already in-pipeline (bridge / feeder) —
        // still run inject so lookahead can chain H+1..H+N from disk into
        // reorder. Without this, Case C's `!in_bridge_pending` guard skips
        // inject entirely while H sits in the bridge → one inject per 250ms
        // poll (live crawl ~4 BPS, inject≈validate ratio≈1, lookahead unused).
        // `coordinator_inject_one` treats already_dispatched as success and
        // continues the chain — it will not reload H from disk.
        //
        // W24: do NOT chain when tip is only marked `dispatched` (not in
        // bridge/feeder) — live: INJECT_CHAIN 24 while tip missing, then
        // covering collapse after soft-retry abort.
        let in_bridge_pending = ready_bridge_for_coord
            .as_ref()
            .is_some_and(|b| b.pending_contains(next_needed));
        let tip_in_feeder_raw = {
            let g = feeder_state_for_coord.0.lock();
            g.0.get(next_needed).is_some()
        };
        let synth_tip_taken = synthetic_wan::bulk_local_disk_stream()
            && tip_stage::tip_taken_by_validation(next_needed);
        // Synth: tip_taken + tip still keyed in feeder is a stale/race state
        // (DIAG live cliff). Do not treat that as durable in-pipeline.
        let tip_in_feeder = tip_in_feeder_raw && !synth_tip_taken;
        let tip_in_pipeline = in_bridge_pending || tip_in_feeder;
        // Case D: tip in bridge/feeder, or synth tip already in validation
        // with empty feeder (engine direct-feed). Ghost-clear lookahead
        // dispatched holes so chain can reload (else INJECT_CHAIN no-ops).
        // Ignition tip-crawl after bare ghost-clear was a TIP_RESERVE
        // yield-spin starvation (fixed); keep this for mid-band fill.
        // Cliff probe: tip absent from reorder (Case C may still be gated by
        // in_bridge_pending). 1 Hz.
        if synthetic_wan::bulk_local_disk_stream() && !reorder_buffer.contains_key(&next_needed) {
            static CLIFF_GATE_AT: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let prev = CLIFF_GATE_AT.load(Ordering::Relaxed);
            if now_ms.saturating_sub(prev) >= 1000 {
                CLIFF_GATE_AT.store(now_ms, Ordering::Relaxed);
                let bnext = ready_bridge_for_coord
                    .as_ref()
                    .and_then(|b| b.next_expected());
                warn!(
                    "[IBD_CLIFF_GATE] tip={} in_pending={} in_feeder={} tip_taken={} dispatched={} bnext={:?} case_c={} ch_len={} reorder={} iter_ms={}",
                    next_needed,
                    in_bridge_pending,
                    tip_in_feeder,
                    synth_tip_taken,
                    dispatched.contains(&next_needed),
                    bnext,
                    !in_bridge_pending,
                    block_rx.len(),
                    reorder_buffer.len(),
                    cliff_iter_t0.elapsed().as_millis(),
                );
            }
        }
        // Only ghost-clear when tip is taken but not in feeder/bridge.
        // Broader "always clear lookahead while tip_in_pipeline" tip-crawled
        // ~6 BPS from ignition (2026-07-23) — reverted.
        if synth_tip_taken && !tip_in_pipeline {
            let look = local_block::gap_inject_lookahead_pub().max(1);
            for h in next_needed..=next_needed.saturating_add(look) {
                if !reorder_buffer.contains_key(&h) {
                    dispatched.remove(&h);
                }
            }
        }
        // H5b: pass real pipeline presence. Stale tip_taken with empty
        // feeder/bridge must reload tip (W24); durable in-pipeline still
        // skips. Short A/B 2026-07-26: CLIFF_* dips to 10 BPS with
        // tip_taken=true+in_feeder=false — forced `true` blocked reload.
        // H7 (tip_taken⇒pipeline) regressed short wall 113→90 — reverted.
        if tip_in_pipeline || synth_tip_taken {
            let _ = try_inject_local_gap(reorder_buffer, val_h, dispatched, tip_in_pipeline);
        }

        // Case C: missing from reorder and bridge — inject / drain / micro-requeue.
        //
        // W20: when tip is `dispatched` it is in-flight on the prefetch/gap-fill
        // queue (W19 handoff). Live W19 soak: Case C under `bridge_waiting`
        // cleared `dispatched` every loop and called `requeue_stall_gaps` →
        // GAP_STREAM flood (~80k/min during local replay) and tip crawl collapsed
        // to ~0.3 BPS past ~686344. Trust in-flight until grace expires.
        //
        // W22 (engine direct-feed): worker_complete fast-paths tip into the ready
        // channel (pending stays empty, next_expected advances). Live W21 WAN:
        // 59× IBD_TIP_INFLIGHT_TIMEOUT while tip was already emitted — false
        // positive cleared dispatched + requeue_stall_gaps → ~0.2 BPS. Treat
        // bridge_next > next_needed as delivered unless leftover cursor-lie
        // (R-259: feeder empty, flight_tip=0, ≥248k — R-258 180k dump occupancy FAIL).
        if !reorder_buffer.contains_key(&next_needed) && !in_bridge_pending {
            let bnext = ready_bridge_for_coord
                .as_ref()
                .and_then(|b| b.next_expected());
            let (flight_tip, _, _) = assigner_for_coord.tip_flight_split();
            let tip_taken_now = tip_stage::tip_taken_by_validation(next_needed);
            let leftover_lie = leftover_w22_cursor_lie(
                next_needed,
                bnext,
                tip_in_feeder,
                tip_taken_now,
                flight_tip,
            );
            let bridge_delivered = leftover_w22_cursor_is_delivered(
                next_needed,
                bnext,
                tip_in_feeder,
                tip_taken_now,
                flight_tip,
            );
            if leftover_lie {
                static LEFTOVER_LIE_AT: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                if LEFTOVER_LIE_AT.swap(next_needed, Ordering::Relaxed) != next_needed {
                    warn!(
                        "[IBD_TIP_HANDOFF_MISS] leftover cursor lie tip={} bridge_next={:?} flight_tip={} dispatched={} feeder={}",
                        next_needed,
                        bnext,
                        flight_tip,
                        dispatched.contains(&next_needed),
                        tip_in_feeder,
                    );
                    if let Some(bridge) = ready_bridge_for_coord {
                        let _ = bridge.rewind_cursor_to(next_needed);
                    }
                }
            }
            let mut allow_requeue = !dispatched.contains(&next_needed);
            // Cliff probe (2026-07-23): rate-limited state dump while tip missing.
            // Identifies which Case C arm holds recovery for ~10s.
            if synthetic_wan::bulk_local_disk_stream() {
                static CLIFF_DIAG_AT: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let prev = CLIFF_DIAG_AT.load(Ordering::Relaxed);
                if now_ms.saturating_sub(prev) >= 1000 {
                    CLIFF_DIAG_AT.store(now_ms, Ordering::Relaxed);
                    let bnext = ready_bridge_for_coord
                        .as_ref()
                        .and_then(|b| b.next_expected());
                    let one_ahead = bnext == Some(next_needed.saturating_add(1));
                    let taken = tip_stage::tip_taken_by_validation(next_needed);
                    let ahead_age_ms = bridge_ahead_since
                        .filter(|(h, _)| *h == next_needed)
                        .map(|(_, t)| t.elapsed().as_millis())
                        .unwrap_or(0);
                    let inflight_age_ms = tip_inflight_since
                        .filter(|(h, _)| *h == next_needed)
                        .map(|(_, t)| t.elapsed().as_millis())
                        .unwrap_or(0);
                    warn!(
                        "[IBD_CLIFF_DIAG] tip={} bnext={:?} bridge_del={} one_ahead={} dispatched={} tip_feeder={} tip_taken={} ahead_age_ms={} inflight_age_ms={} ch_len={} reorder={}",
                        next_needed,
                        bnext,
                        bridge_delivered,
                        one_ahead,
                        dispatched.contains(&next_needed),
                        tip_in_feeder,
                        taken,
                        ahead_age_ms,
                        inflight_age_ms,
                        block_rx.len(),
                        reorder_buffer.len(),
                    );
                }
            }
            if bridge_delivered {
                // Tip already emitted to ready channel. Give feeder/validation a
                // short window; if still stuck *one* height ahead, rewind cursor
                // and requeue (lost ReadyItem — live: bridge_next=need+1, feeder=0).
                // W26b: do NOT rewind when cursor is many heights ahead — that is
                // healthy inject/try_emit pipelining (live: rewind 642820→642756
                // collapsed local replay to ~0.7 BPS).
                match *bridge_ahead_since {
                    Some((h, _)) if h == next_needed => {}
                    _ => {
                        *bridge_ahead_since = Some((next_needed, std::time::Instant::now()));
                    }
                }
                *tip_inflight_since = None;
                let one_ahead = ready_bridge_for_coord
                    .as_ref()
                    .and_then(|b| b.next_expected())
                    == Some(next_needed.saturating_add(1));
                if one_ahead {
                    // Synth cliff (2026-07-23): one_ahead + tip_taken blocked
                    // grace rewind for tip_inflight_grace (10s) → LOCAL_GAP
                    // once/~10s ≈ 6 BPS. Live DIAG also showed tip_feeder=true
                    // with tip_taken=true (stale/race) which blocked the
                    // !tip_in_feeder inject bypass — treat tip_taken as
                    // "not durable in feeder" for synth inject.
                    let synth_tip_hole = synthetic_wan::bulk_local_disk_stream()
                        && !reorder_buffer.contains_key(&next_needed)
                        && (!tip_in_feeder || tip_stage::tip_taken_by_validation(next_needed));
                    if synth_tip_hole {
                        dispatched.remove(&next_needed);
                        *bridge_ahead_since = None;
                        allow_requeue = true;
                    } else if bridge_ahead_since
                        .is_some_and(|(h, t)| h == next_needed && t.elapsed() >= tip_inflight_grace)
                        && !tip_stage::tip_taken_by_validation(next_needed)
                    {
                        // W39/W49/W50: rewind only when tip is truly lost.
                        if tip_in_feeder || reorder_buffer.contains_key(&next_needed) {
                            allow_requeue = false;
                        } else if let Some(bridge) = ready_bridge_for_coord {
                            let _ = bridge.rewind_cursor_to(next_needed);
                            dispatched.remove(&next_needed);
                            *bridge_ahead_since = None;
                            allow_requeue = true;
                        } else {
                            allow_requeue = false;
                        }
                    } else {
                        allow_requeue = false;
                    }
                } else if !one_ahead {
                    // W26b: do NOT rewind far-ahead when the tip is in the
                    // feeder (healthy try_emit pipelining; rewind
                    // 642820→642756 → ~0.7 BPS).
                    // R-376: cursor ~60 ahead, pending empty, tip NOT in
                    // the feeder. That pipeline is not healthy — rewind
                    // and let inject/Case B place the body.
                    if tip_in_feeder {
                        allow_requeue = false;
                    } else if cursor_far_ahead_tip_lost(
                        next_needed,
                        bnext.unwrap_or(next_needed),
                        false,
                        false,
                        tip_taken_now,
                    ) {
                        if let Some(bridge) = ready_bridge_for_coord {
                            let _ = bridge.rewind_cursor_to(next_needed);
                        }
                        dispatched.remove(&next_needed);
                        *bridge_ahead_since = None;
                        allow_requeue = true;
                    } else if synthetic_wan::bulk_local_disk_stream() {
                        allow_requeue = true; // inject only; no rewind
                    } else {
                        allow_requeue = false;
                    }
                } else {
                    allow_requeue = false;
                }
            } else if dispatched.contains(&next_needed) {
                *bridge_ahead_since = None;
                // W24: dispatched but no peer covers tip → false in-flight
                // (abort thrash cleared in_flight while sticky dispatched).
                let (covering, flight_ranges, _) = assigner_for_coord.tip_flight_diag();
                IBD_TIP_COVERING.store(covering, Ordering::Relaxed);
                IBD_TIP_IN_FLIGHT_RANGES.store(flight_ranges, Ordering::Relaxed);
                if covering == 0 {
                    warn!(
                        "[IBD_TIP_COVERING_ZERO] tip {} dispatched but covering=0 — clearing for recovery",
                        next_needed
                    );
                    dispatched.remove(&next_needed);
                    *tip_inflight_since = None;
                    allow_requeue = true;
                    // Phase 2 EMPTY_TIP: force open tip-owner slot so a deep
                    // assign arms quickly (gd already warm; not GetData-cold).
                    assigner_for_coord.force_empty_tip_rearm(next_needed);
                } else if synthetic_wan::bulk_local_disk_stream() && !tip_in_feeder {
                    // Synth bulk: sticky dispatched+covering must not impose
                    // tip_inflight_grace (default 10s → INJECT_CHAIN ~10s /
                    // wall ~8 BPS; burst chain ~40ms). Ghost-clear tip..look
                    // then inject with tip_in_pipeline=false so tip reloads
                    // (tip_in_pipeline=true skips disk on dispatched ghosts).
                    // allow_requeue=false — W20: requeue under covering storms.
                    *tip_inflight_since = None;
                    let look = local_block::gap_inject_lookahead_pub().max(1);
                    for h in next_needed..=next_needed.saturating_add(look) {
                        if !reorder_buffer.contains_key(&h) {
                            dispatched.remove(&h);
                        }
                    }
                    let _ = try_inject_local_gap(reorder_buffer, val_h, dispatched, false);
                    allow_requeue = false;
                } else {
                    match *tip_inflight_since {
                        Some((h, _)) if h == next_needed => {}
                        _ => {
                            *tip_inflight_since = Some((next_needed, std::time::Instant::now()));
                        }
                    }
                    // Engine mode completes sync into the ready channel — a long
                    // "inflight" with bridge_next still == tip means true loss.
                    // Keep grace, but never fire when cursor has already advanced.
                    if tip_inflight_since
                        .is_some_and(|(h, t)| h == next_needed && t.elapsed() >= tip_inflight_grace)
                    {
                        warn!(
                            "[IBD_TIP_INFLIGHT_TIMEOUT] tip {} dispatched but not in bridge after {:?} — clearing for recovery",
                            next_needed, tip_inflight_grace
                        );
                        dispatched.remove(&next_needed);
                        *tip_inflight_since = None;
                        allow_requeue = true;
                    } else {
                        allow_requeue = false;
                    }
                }
            } else {
                *tip_inflight_since = None;
                *bridge_ahead_since = None;
            }

            if allow_requeue {
                feeder_miss::INJECT_GATED.store(false, Ordering::Relaxed);
                let _ = try_inject_local_gap(reorder_buffer, val_h, dispatched, false);
                if !reorder_buffer.contains_key(&next_needed) {
                    let drained = emergency_drain_block_rx_for_gap(
                        block_rx,
                        reorder_buffer,
                        next_needed,
                        dynamic_buffer_limit.min(64),
                        dynamic_buffer_limit,
                        total_received,
                        bridge_pending_max,
                        admit_window,
                    );
                    if !drained && !reorder_buffer.contains_key(&next_needed) {
                        let (covering, _, _) = assigner_for_coord.tip_flight_diag();
                        if covering == 0 {
                            // W73: WAN tip holes need force (H,H) too — P0-B
                            // non-force skip left genesis stuck at 262716.
                            assigner_for_coord.requeue_stall_gaps_force(next_needed, None);
                        } else {
                            // R-212: covering≥1 zombie (TIP_WALK_PROMOTE
                            // 250126-251888) + non-force is P0-B no-op on
                            // WAN → STALL 11 / FORCE 0 / sit @250127.
                            // W73 force already allows covering≤1 (H,H).
                            if assigner_for_coord.wan_stall_micro_allowed(next_needed) {
                                assigner_for_coord.requeue_stall_gaps_force(next_needed, None);
                            }
                        }
                    }
                }
            } else {
                // In-flight: lookahead only when tip is confirmed in feeder
                // (or bridge_delivered — channel transit). Never chain over a
                // tip that is only sticky-dispatched (W24).
                let tip_in_feeder = {
                    let g = feeder_state_for_coord.0.lock();
                    g.0.get(next_needed).is_some()
                };
                if tip_in_feeder || bridge_delivered {
                    feeder_miss::INJECT_GATED.store(false, Ordering::Relaxed);
                    let _ = try_inject_local_gap(reorder_buffer, val_h, dispatched, true);
                } else {
                    feeder_miss::note_inject_gated();
                    // R-208: covering≥1 + sticky-dispatched must not hide an
                    // on-disk H (R-207 18–19k wait_feeder 98.8% then TIP_SKIP
                    // already_persisted). tip_in_pipeline=false reloads tip;
                    // chain stops at first miss (W24).
                    let _ = try_inject_local_gap(reorder_buffer, val_h, dispatched, false);
                }
            }
        } else if in_bridge_pending {
            *tip_inflight_since = None;
        }
    }
}

struct IbdSessionTail<'a> {
    actual_synced_height: u64,
    assigner: Arc<ChunkAssigner>,
    block_sync_start: std::time::Instant,
    blockstore: Arc<BlockStore>,
    confirmed_body_height_at_start: u64,
    coord_handle: tokio::task::JoinHandle<()>,
    download_handles: Vec<(i32, tokio::task::JoinHandle<()>)>,
    effective_end_height: u64,
    effective_end_live: Arc<AtomicU64>,
    engine_durability: crate::config::ibd::IbdEngineDurabilityConfig,
    event_publisher: Option<Arc<crate::node::event_publisher::EventPublisher>>,
    feeder_state: feeder::FeederState,
    gap_fill_tx_v2: crossbeam_channel::Sender<PrefetchWorkItemV2>,
    ibd_store_v2: Arc<IbdUtxoStore>,
    max_ahead_blocks: u64,
    max_ahead_live_for_validation: Arc<AtomicU64>,
    mem_guard: MemoryGuard,
    prefetch_input_tx_v2: crossbeam_channel::Sender<PrefetchWorkItemV2>,
    prefetch_join_handles: Vec<std::thread::JoinHandle<()>>,
    protocol: Arc<BitcoinProtocolEngine>,
    ready_rx: crossbeam_channel::Receiver<ReadyItem>,
    stall_tx: broadcast::Sender<u64>,
    self_ibd: Arc<ParallelIBD>,
    sparse_local_body_max: u64,
    start_height: u64,
    storage: &'a Arc<Storage>,
    utxo_engine: Option<Arc<crate::storage::ibd_engine::UtxoDatabase>>,
    utxo_set: &'a mut UtxoSet,
    validation_height: Arc<AtomicU64>,
}

async fn finish_parallel_ibd_session(ctx: IbdSessionTail<'_>) -> Result<()> {
    let IbdSessionTail {
        actual_synced_height,
        assigner,
        block_sync_start,
        blockstore,
        confirmed_body_height_at_start,
        coord_handle,
        download_handles,
        effective_end_height,
        effective_end_live,
        engine_durability,
        event_publisher,
        feeder_state,
        gap_fill_tx_v2,
        ibd_store_v2,
        max_ahead_blocks,
        max_ahead_live_for_validation,
        mem_guard,
        prefetch_input_tx_v2,
        mut prefetch_join_handles,
        protocol,
        ready_rx,
        stall_tx,
        self_ibd,
        sparse_local_body_max,
        start_height,
        storage,
        utxo_engine,
        utxo_set,
        validation_height,
    } = ctx;
    // Block feeder: drains ready_rx into shared buffer so validation can run while buffer fills.
    // Feeder runs on std::thread (crossbeam recv is blocking). Buffer fills while validation works.
    // feeder_state was created earlier (before coordinator spawn) so the coordinator could reference it.
    let mut feeder_buffer_limit = mem_guard.buffer_limit(start_height);
    if utxo_engine.is_some() && sparse_local_body_max > start_height {
        feeder_buffer_limit = feeder_buffer_limit.max(400);
    }
    let feeder_buffer_bytes_limit = mem_guard.feeder_buffer_bytes_limit;
    // Lets the teardown path unblock the feeder (which otherwise parks on backpressure / a
    // blocking recv kept open by the detached coordinator's sender clones) so the real
    // validation error surfaces instead of `feeder_handle.join()` hanging forever.
    let feeder_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let feeder_handle = run_feeder_thread(
        ready_rx,
        Arc::clone(&feeder_state),
        feeder_buffer_limit,
        feeder_buffer_bytes_limit,
        Arc::clone(&feeder_shutdown),
    );

    // Validation worker thread: reads shared buffer, waits on Condvar when empty.
    let storage_clone = Arc::clone(storage);
    let utxo_mutex = Arc::new(std::sync::Mutex::new(std::mem::take(utxo_set)));

    let feeder_state_valid = Arc::clone(&feeder_state);
    let ibd_store_v2_valid = Arc::clone(&ibd_store_v2);
    let blockstore_valid = Arc::clone(&blockstore);
    let storage_clone_valid = storage_clone.clone();
    let self_clone_valid = Arc::clone(&self_ibd);
    let protocol_valid = Arc::clone(&protocol);
    let utxo_mutex_valid = Arc::clone(&utxo_mutex);
    let utxo_nominal_max_entries = mem_guard.utxo_max_entries;
    let utxo_pf = self_ibd.config.utxo_prefetch_lookahead.clamp(1, 512) as usize;

    // `confirmed_body_height_at_start` was probed before the coordinator spawn (see above).
    // Cap `local_replay_max_height` (write-skip bound) separately — it may be lower than
    // confirmed bodies when RAM limits no-LMDB replay.
    let local_replay_max_height: u64 = if confirmed_body_height_at_start > 0 {
        // Read both MemTotal and MemAvailable from /proc/meminfo.
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let read_kb = |key: &str| -> u64 {
            meminfo
                .lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let total_ram_bytes = read_kb("MemTotal:").saturating_mul(1024);
        let avail_bytes = read_kb("MemAvailable:").saturating_mul(1024);
        let _ = avail_bytes; // used in log below

        // Empirical RSS cost per UTXO in no-LMDB mode: ~2300 bytes (UTXO heap alone).
        // Measured on 94 GB machine: h=346k → 19M UTXOs → blvm RSS = 54 GB → 2842 bytes/UTXO
        // (includes Arc<UTXO> heap, DashMap bucket overhead, allocator fragmentation).
        // The process also carries ~10 GB of non-UTXO base overhead (debug binary, jemalloc
        // arenas, code, thread stacks), which the formula must subtract before computing
        // the UTXO budget, or the cap is too high and OOM occurs.
        let bytes_per_utxo_rss: u64 = std::env::var("BLVM_IBD_BYTES_PER_UTXO")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2800);
        // UTXO set at height H: cumulative net additions ≈ 55 UTXOs/block on average
        // from genesis through the SegWit era (empirically validated).
        let utxos_per_block: u64 = 55;
        // Base process overhead that is NOT part of the UTXO cache: debug binary sections,
        // thread stacks, jemalloc arenas, DashMap metadata, LMDB env handle.
        // Observed: ~10 GB on a debug build with 94 GB RAM. Use 12 GB to be safe.
        let base_process_overhead_bytes: u64 = std::env::var("BLVM_IBD_BASE_OVERHEAD_MB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(12_288) // 12 GB default
            .saturating_mul(1024 * 1024);
        // Budget: 40% of total RAM minus base overhead.  Remainder covers OS page cache,
        // LMDB mmap pages for block reads (~30-40 GB on this machine), and other processes.
        // On 94 GB: 40% = 37.7 GB - 12 GB base = 25.7 GB for UTXOs → ~11.2M UTXOs → 204k blocks.
        let budget_pct: u64 = std::env::var("BLVM_IBD_REPLAY_RAM_PCT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(40);
        let budget_bytes =
            (total_ram_bytes * budget_pct / 100).saturating_sub(base_process_overhead_bytes);
        let max_utxos = budget_bytes / bytes_per_utxo_rss;
        let ram_safe_height = max_utxos / utxos_per_block;

        let capped = confirmed_body_height_at_start.min(ram_safe_height);
        if capped < confirmed_body_height_at_start {
            info!(
                "IBD: no-LMDB local replay capped at height {} (total RAM {} MB, \
                     budget {}% = {} MB minus {} MB base overhead = {} MB UTXO budget, \
                     {} bytes/UTXO RSS, {} UTXOs/block → {} max UTXOs → {} block cap); \
                     heights {}-{} will use normal LMDB durability",
                capped,
                total_ram_bytes / 1024 / 1024,
                budget_pct,
                total_ram_bytes * budget_pct / 100 / 1024 / 1024,
                base_process_overhead_bytes / 1024 / 1024,
                budget_bytes / 1024 / 1024,
                bytes_per_utxo_rss,
                utxos_per_block,
                max_utxos,
                ram_safe_height,
                capped + 1,
                confirmed_body_height_at_start
            );
        }
        capped
    } else {
        0
    };

    if ibd_local_gap_fill_enabled() {
        let fill_max = ibd_local_gap_fill_max_height(confirmed_body_height_at_start);
        if fill_max == u64::MAX {
            info!(
                "IBD: local gap fill enabled uncapped (start confirmed bodies={}, write-skip cap={}; \
                     GAP_PERSIST heights remain injectable past start watermark)",
                confirmed_body_height_at_start, local_replay_max_height
            );
        } else {
            info!(
                "IBD: local gap fill enabled for heights ≤ {} (confirmed bodies={}, write-skip cap={})",
                fill_max, confirmed_body_height_at_start, local_replay_max_height
            );
        }
    }

    if local_replay_max_height > 0 {
        info!(
            "IBD: block store has body-confirmed blocks up to height {}; local replay \
                 will skip redundant storage writes for heights 1-{} to eliminate LMDB I/O stalls",
            local_replay_max_height, local_replay_max_height
        );
    }

    // Keep a clone of the engine Arc for Phase 3 watermark export after IBD completes.
    // The engine is also moved into ValidationParams so the dispatch thread can call
    // SpendSession::resolve. Both arcs point to the same UtxoDatabase.
    let utxo_engine_for_export = utxo_engine.clone();

    let engine_stored_export_height = storage
        .chain()
        .get_engine_export_height()
        .ok()
        .flatten()
        .unwrap_or(0);
    let engine_stored_export_utxo_count = storage
        .chain()
        .get_engine_export_utxo_count()
        .ok()
        .flatten()
        .unwrap_or(0);
    let engine_resume_gap_replay =
        utxo_engine.is_some() && start_height > 1 && engine_stored_export_height > 0;
    let engine_gap_export_defer_until = if engine_resume_gap_replay {
        engine_gap_export_defer_until_height(
            start_height,
            local_replay_max_height,
            actual_synced_height,
        )
    } else {
        0
    };
    if engine_resume_gap_replay && engine_gap_export_defer_until > 0 {
        let gap_blocks =
            engine_gap_export_defer_until.saturating_sub(start_height.saturating_sub(1));
        info!(
            "[IBD_ENGINE_REPLAY] gap replay {}..={} ({} blocks) — deferring periodic \
                 checkpoint export until validation_height>={}",
            start_height, engine_gap_export_defer_until, gap_blocks, engine_gap_export_defer_until,
        );
    } else if engine_resume_gap_replay {
        info!(
            "[IBD_ENGINE_REPLAY] resume at h={} above local replay cap {} — periodic \
                 checkpoint export active from export_h={}",
            start_height, local_replay_max_height, engine_stored_export_height,
        );
    }
    {
        use types::{IbdPhaseCtx, derive_ibd_phase};
        let phase_ctx = IbdPhaseCtx {
            validation_h: start_height,
            start_height,
            local_replay_max_height,
            confirmed_body_height_at_start,
            sparse_local_body_max,
            engine_export_height: engine_stored_export_height,
            effective_end_height,
        };
        info!(
            "[IBD_PHASE] {:?} at startup (vh={}, local_replay_max={}, export_h={})",
            derive_ibd_phase(&phase_ctx),
            start_height,
            local_replay_max_height,
            engine_stored_export_height,
        );
    }

    // Adaptive interval: UTXO count + export duration set the ceiling; live validation BPS
    // can shrink the interval so slow replay does not leave huge dirty-restart windows.
    // Target ~checkpoint_target_secs wall clock between exports. Override via BLVM_IBD_CHECKPOINT_INTERVAL.
    let periodic_checkpoint_handle: Option<std::thread::JoinHandle<()>> = if let Some(
        ref engine_arc,
    ) =
        utxo_engine_for_export
    {
        let engine_clone = Arc::clone(engine_arc);
        let storage_ckpt = Arc::clone(storage);
        let durability = engine_durability;
        let effective_end_live_ckpt = Arc::clone(&effective_end_live);
        let stored_export_h = engine_stored_export_height as i32;
        let stored_export_utxo_count = engine_stored_export_utxo_count;
        let gap_export_defer_until = engine_gap_export_defer_until;
        let resume_gap_replay = engine_resume_gap_replay;
        let validation_height_export = Arc::clone(&validation_height);
        let handle = std::thread::Builder::new()
                    .name("ibd-checkpoint-export".to_string())
                    .spawn(move || {
                        // Lower this thread's I/O scheduling class to "best-effort, priority 7"
                        // (lowest within BE class) so validation workers' io_uring reads are
                        // preferred over the export's sequential flat-file reads under contention.
                        #[cfg(target_os = "linux")]
                        {
                            // IOPRIO_WHO_PROCESS=1, IOPRIO_CLASS_BE=2, prio=7 (lowest)
                            // IOPRIO_PRIO_VALUE(class, prio) = (class << 13) | prio
                            let ioprio: libc::c_long = (2 << 13) | 7;
                            unsafe { libc::syscall(libc::SYS_ioprio_set, 1i32, 0i32, ioprio) };
                        }
                        let mut last_exported: i32 = stored_export_h;
                        let mut last_utxo_count: u64 = stored_export_utxo_count;
                        let stored_export_wall_secs = storage_ckpt
                            .chain()
                            .get_engine_export_wall_secs()
                            .ok()
                            .flatten()
                            .unwrap_or(0) as f64;
                        let mut last_export_secs: f64 = stored_export_wall_secs;
                        let mut refuse_streak: u32 = 0;
                        if stored_export_wall_secs > 0.0 {
                            info!(
                                "[IBD_EXPORT_INTERVAL] restored last_export_wall_secs={:.0} \
                                 utxos={} export_h={}",
                                stored_export_wall_secs,
                                stored_export_utxo_count,
                                stored_export_h,
                            );
                        }
                        let mut gap_defer_logged = resume_gap_replay;
                        let mut bps_sample_vh: u64 = 0;
                        let mut bps_sample_at = std::time::Instant::now();
                        let mut validation_bps: f64 = 0.0;
                        let mut ckpt_hold_logged_at: Option<std::time::Instant> = None;
                        let mut fence_floor_logged: i32 = -1;
                        loop {
                            std::thread::sleep(std::time::Duration::from_secs(5));
                            let cl = engine_clone.contiguous_length();
                            let vh_now = validation_height_export.load(Ordering::Relaxed);
                            if let Some(origin) =
                                checkpoint_bps_arm_sample_origin(bps_sample_vh, vh_now)
                            {
                                bps_sample_vh = origin;
                                bps_sample_at = std::time::Instant::now();
                            }
                            let bps_elapsed = bps_sample_at.elapsed().as_secs_f64();
                            if bps_elapsed >= 30.0 && vh_now > bps_sample_vh {
                                let sample =
                                    (vh_now - bps_sample_vh) as f64 / bps_elapsed;
                                validation_bps =
                                    adopt_checkpoint_bps_sample(validation_bps, sample);
                            }
                            if bps_elapsed >= 60.0 {
                                bps_sample_vh = vh_now;
                                bps_sample_at = std::time::Instant::now();
                            }
                            // Exit only when last committed export is within one interval of
                            // end. vh/cl at END_HEIGHT is not enough (skip-path race).
                            let end_h_now = effective_end_live_ckpt
                                .load(Ordering::Relaxed)
                                .min(i32::MAX as u64) as i32;
                            let utxo_iv = utxo_scaled_checkpoint_interval(
                                last_utxo_count,
                                last_export_secs,
                                &durability,
                            );
                            let interval = adaptive_checkpoint_interval(
                                last_utxo_count,
                                last_export_secs,
                                validation_bps,
                                &durability,
                            );
                            let schedule_iv = checkpoint_schedule_interval(
                                interval,
                                utxo_iv,
                                last_utxo_count,
                            );
                            let journal_entries = engine_clone.disk_cold_entry_count();
                            let journal_floor = durability
                                .checkpoint_min_interval
                                .max(DEST_BE_INTERVAL_FLOOR);
                            let pre_journal_iv = schedule_iv;
                            let journal_iv = journal_scaled_checkpoint_interval(
                                schedule_iv,
                                journal_entries,
                                journal_floor,
                            );
                            let fence_hw = crate::storage::ibd_engine::gc_fence_high_water();
                            let schedule_iv = checkpoint_interval_respecting_fence(
                                last_exported,
                                journal_iv,
                                fence_hw,
                            );
                            if schedule_iv != journal_iv && fence_floor_logged != schedule_iv {
                                info!(
                                    "[IBD_CKPT_FENCE_FLOOR] interval {journal_iv} → {schedule_iv} \
                                     so next ckpt {} >= fence {fence_hw}",
                                    last_exported.saturating_add(schedule_iv)
                                );
                                fence_floor_logged = schedule_iv;
                            }
                            // R-352: publish the next scheduled checkpoint so the scoreboard's
                            // between-export GC fence advance never passes it. The fence floor
                            // above keeps that target from sliding back under the high water.
                            crate::storage::ibd_engine::set_next_checkpoint_target(
                                last_exported.saturating_add(schedule_iv.max(0)),
                            );
                            if checkpoint_export_thread_should_exit(
                                vh_now,
                                cl,
                                end_h_now,
                                last_exported,
                                schedule_iv,
                            ) {
                                info!(
                                    "[IBD_CKPT_EXIT] validation_h={} cl={} end_h={} last_exported={} interval={} — joining",
                                    vh_now, cl, end_h_now, last_exported, schedule_iv
                                );
                                break;
                            }
                            if (vh_now as i64) >= i64::from(end_h_now) && end_h_now > 0 {
                                let should_log = ckpt_hold_logged_at
                                    .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(60));
                                if should_log {
                                    info!(
                                        "[IBD_CKPT_HOLD] validation at end (vh={} cl={} end_h={}) \
                                         last_exported={} need>={} — exports catching up, not joining",
                                        vh_now,
                                        cl,
                                        end_h_now,
                                        last_exported,
                                        end_h_now.saturating_sub(schedule_iv.max(0))
                                    );
                                    ckpt_hold_logged_at = Some(std::time::Instant::now());
                                }
                            }
                            if cl <= 0 {
                                continue;
                            }
                            if resume_gap_replay && gap_export_defer_until > 0 {
                                if vh_now < gap_export_defer_until {
                                    continue;
                                }
                                if gap_defer_logged {
                                    info!(
                                        "[IBD_ENGINE_REPLAY] gap replay complete at height {} \
                                         — resuming periodic checkpoint export",
                                        gap_export_defer_until
                                    );
                                    gap_defer_logged = false;
                                }
                            }
                            let ckpt = aligned_checkpoint_height(cl, last_exported, schedule_iv);
                            if ckpt <= last_exported || ckpt <= 0 {
                                continue;
                            }
                            // Only export if the engine has fully processed the checkpoint height.
                            if cl < ckpt {
                                continue;
                            }
                            // Also require validation to have reached `ckpt`. `contiguous_length`
                            // can be restored from stale segments/sidecar after a metadata reset
                            // to export_h=0 (live 2026-07-14: CL=49716 while vh≈5800 → poisoned
                            // export at 40000 with 5581 UTXOs).
                            if !checkpoint_export_validation_caught_up(ckpt, vh_now) {
                                continue;
                            }
                            // Never scan/export past durable block tip — labeling a higher-height
                            // UTXO snapshot as tip poisons resume (UTXO miss at tip+1).
                            let block_tip =
                                storage_ckpt.chain().get_height().ok().flatten().unwrap_or(0) as i32;
                            if ckpt > block_tip {
                                continue;
                            }
                            // W75: defer during local-inject / catch-up bursts (WAN tip crawl
                            // is typically ≪ 200 BPS; soft-resume disk inject hits thousands).
                            // dest-x/bc 180–200k stayed above this EMA → no in-band export;
                            // dest-bf…bl 10k grid released at 180000 when it dipped.
                            // LAG_EXEMPT must also skip W75 burst defer — genesis 300+ BPS
                            // would otherwise silent-continue forever in front of the gate.
                            if checkpoint_export_defer_for_burst_bps(validation_bps)
                                && !checkpoint_lag_exempt(
                                    vh_now as i32,
                                    last_exported,
                                    schedule_iv,
                                )
                            {
                                continue;
                            }
                            if !export_start_gate_allows_at(
                                vh_now as i32,
                                last_exported,
                                schedule_iv,
                            ) {
                                continue;
                            }
                            let active_slot =
                                storage_ckpt.chain().get_engine_ckpt_slot().unwrap_or(0);
                            let write_slot =
                                crate::storage::ibd_engine::ckpt_inactive_slot(active_slot);
                            let ckpt_tree_name =
                                crate::storage::ibd_engine::ckpt_tree_for_slot(write_slot);
                            let tree = match storage_ckpt.open_tree(ckpt_tree_name) {
                                Ok(t) => t,
                                Err(e) => {
                                    warn!(
                                        "IBD checkpoint: failed to open {}: {e:#}",
                                        ckpt_tree_name
                                    );
                                    continue;
                                }
                            };
                            // R-352: between exports the scoreboard advances the fence with
                            // validation (every 10k, only while no export is active). Pairs with
                            // Delete <= that high-water may already be GC'd, so a snapshot
                            // labelled below it would be incomplete. Wait for an aligned height
                            // at or above the high-water instead.
                            let fence_hw = crate::storage::ibd_engine::gc_fence_high_water();
                            if ckpt < fence_hw {
                                info!(
                                    "[IBD_CKPT_FENCE_WAIT] ckpt={} < gc_fence_high_water={} \
                                     (last_exported={} interval={} cl={}) — waiting for the next aligned height",
                                    ckpt, fence_hw, last_exported, schedule_iv, cl
                                );
                                continue;
                            }
                            // W176: tip CAP sees export-active so holey pipes wait through
                            // disk contention instead of mute-rotating every 8s. R-352: set
                            // **before** the fence so the scoreboard cannot advance it past
                            // `ckpt` between here and the scan.
                            struct ExportActiveGuard;
                            impl Drop for ExportActiveGuard {
                                fn drop(&mut self) {
                                    IBD_CHECKPOINT_EXPORT_ACTIVE
                                        .store(false, Ordering::Release);
                                }
                            }
                            IBD_CHECKPOINT_EXPORT_ACTIVE.store(true, Ordering::Release);
                            let _export_active = ExportActiveGuard;
                            // Set the GC fence *before* starting the scan so that
                            // concurrent MemoryRun compactions do not cancel Add+Delete
                            // pairs where the Delete is above this checkpoint height.
                            // Without the fence, UTXOs created before `ckpt` but spent
                            // after `ckpt` can be GC'd out of the engine while
                            // `scan_live_at_height(ckpt)` is running, producing an
                            // incomplete checkpoint that fails on resume.
                            crate::storage::ibd_engine::set_gc_fence(ckpt);
                            if journal_iv != pre_journal_iv {
                                info!(
                                    "[IBD_CKPT_JOURNAL_SCALE] journal_entries={} segs={} \
                                     interval {} → {} (target={} floor={})",
                                    journal_entries,
                                    engine_clone.disk_segment_count(),
                                    pre_journal_iv,
                                    journal_iv,
                                    CHECKPOINT_COMPACT_INPUT_TARGET,
                                    journal_floor
                                );
                            }
                            info!(
                                "IBD engine: starting periodic checkpoint export at height {} \
                                 (engine_height={}, tree={}, interval={}, utxo_iv={}, bps={:.1}, \
                                 target_secs={}, journal_entries={}) [GC fence={}]",
                                ckpt,
                                cl,
                                ckpt_tree_name,
                                schedule_iv,
                                utxo_iv,
                                validation_bps,
                                durability.checkpoint_target_secs,
                                journal_entries,
                                ckpt
                            );
                            if export_isolation_enabled() {
                                info!(
                                    "[IBD_EXPORT_ISOLATION] pausing peer assign + validation \
                                     dispatch for checkpoint export at height {}",
                                    ckpt
                                );
                            }
                            match crate::storage::ibd_engine::run_checkpoint_export_replace(
                                &engine_clone,
                                &tree,
                                ckpt,
                                storage_ckpt.utxo_value_codec(),
                            ) {
                                Ok((muhash, count, timings)) => {
                                    let is_first = last_exported <= 0 || last_utxo_count == 0;
                                    if !crate::storage::ibd_autorepair::checkpoint_persist_acceptable(
                                        ckpt as u64,
                                        count as u64,
                                        last_utxo_count,
                                        timings.tee_merged_entries,
                                        timings.cold_segs_at_start > 0,
                                        is_first,
                                    ) {
                                        warn!(
                                            "[IBD_CKPT_REFUSE] height={} utxo_count={} last_accepted={} \
                                             tee_merged={} cold_segs={} first={} — not advancing last_exported={}",
                                            ckpt,
                                            count,
                                            last_utxo_count,
                                            timings.tee_merged_entries,
                                            timings.cold_segs_at_start,
                                            is_first,
                                            last_exported
                                        );
                                        crate::storage::ibd_engine::set_gc_fence(last_exported);
                                        refuse_streak = refuse_streak.saturating_add(1);
                                        if crate::storage::ibd_autorepair::checkpoint_refuse_hard_stop(
                                            refuse_streak,
                                        ) {
                                            warn!(
                                                "[IBD_CKPT_EXPORT_FAIL] {} consecutive refused persists \
                                                 — stopping export path (not setting ibd_utxo_repair_required)",
                                                refuse_streak
                                            );
                                            break;
                                        }
                                        continue;
                                    }
                                    refuse_streak = 0;
                                    info!(
                                        "[IBD_PROFILE] export_h={} wall_ms={} compact_ms={} scan_prep_ms={} stream_ms={} clear_ms={} fetch_ms={} encode_ms={} write_ms={} overlay_ms={} trim_ms={} utxos={} tee_merged={} cold_segs={}",
                                        ckpt,
                                        timings.wall_ms,
                                        timings.compact_ms,
                                        timings.scan_prep_ms,
                                        timings.stream_ms,
                                        timings.clear_ms,
                                        timings.fetch_ms,
                                        timings.encode_ms,
                                        timings.write_ms,
                                        timings.overlay_ms,
                                        timings.trim_ms,
                                        count,
                                        timings.tee_merged_entries,
                                        timings.cold_segs_at_start
                                    );
                                    let muhash_bytes = muhash.serialize_running_state();
                                    match crate::storage::ibd_engine::sync_tree_after_persist(
                                        tree.as_ref(),
                                    ) {
                                        Ok(()) => {
                                            match storage_ckpt
                                                .chain()
                                                .persist_engine_checkpoint_complete(
                                                    ckpt as u64,
                                                    write_slot,
                                                    count as u64,
                                                    timings.wall_ms / 1000,
                                                    &muhash_bytes,
                                                ) {
                                                Ok(()) => {
                                                    info!(
                                                        "IBD engine: checkpoint persisted at height {} \
                                                         (slot {}, {} UTXOs)",
                                                        ckpt, write_slot, count
                                                    );
                                                    last_exported = ckpt;
                                                    last_utxo_count = count as u64;
                                                    last_export_secs =
                                                        timings.wall_ms as f64 / 1000.0;
                                                    // Clamp fence to last committed export.
                                                    // set_gc_fence(ckpt+next_interval) let compact
                                                    // GC above the committed snapshot and poisoned
                                                    // the next lagged piggyback.
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        "IBD engine: failed to persist checkpoint \
                                                         metadata at {ckpt} after sync: {e:#}"
                                                    );
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            warn!(
                                                "IBD engine: checkpoint sync before metadata at \
                                                 h={ckpt} failed (metadata not advanced): {e:#}"
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        "IBD engine: checkpoint export failed at height {ckpt}: \
                                         {e:#}"
                                    );
                                    // On failure the fence stays at ckpt so the retry scan is
                                    // protected. The retry will re-advance the fence when it
                                    // starts the next export attempt.
                                }
                            }
                        }
                    })
                    .expect("spawn ibd-checkpoint-export");
        Some(handle)
    } else {
        None
    };

    let params = validation_loop::ValidationParams {
        feeder_state: feeder_state_valid,
        ibd_store: ibd_store_v2_valid,
        blockstore: blockstore_valid,
        storage: storage_clone_valid,
        parallel_ibd: self_clone_valid,
        protocol: protocol_valid,
        utxo_mutex: utxo_mutex_valid,
        effective_end_live: Arc::clone(&effective_end_live),
        start_height,
        validation_height: Arc::clone(&validation_height),
        mem_guard,
        max_ahead_live: max_ahead_live_for_validation,
        nominal_max_ahead: max_ahead_blocks,
        utxo_nominal_max_entries,
        utxo_prefetch_lookahead: utxo_pf,
        stall_tx: stall_tx.clone(),
        utxo_engine,
        checkpoint_tx: None,
        local_replay_max_height,
        assigner: Arc::clone(&assigner),
        engine_gap_export_defer_until: if engine_resume_gap_replay
            && engine_gap_export_defer_until > 0
        {
            Some(engine_gap_export_defer_until)
        } else {
            None
        },
    };
    let validation_handle =
        std::thread::spawn(move || validation_loop::run_validation_loop(params));

    // Spawn BlockSyncProgress publisher — polls validation_height every 2s for module event subscribers
    let progress_handle = if let Some(ref ep) = event_publisher {
        let ep = Arc::clone(ep);
        let vh = Arc::clone(&validation_height);
        let start = start_height;
        let end_live = Arc::clone(&effective_end_live);
        let sync_start = block_sync_start;
        Some(tokio::spawn(async move {
            let mut last_height = start;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let current = vh.load(Ordering::Relaxed);
                let end = end_live.load(Ordering::Relaxed);
                if current > last_height {
                    let elapsed = sync_start.elapsed().as_secs_f64();
                    let progress_percent = if end > start && elapsed > 0.0 {
                        ((current - start) as f64 / (end - start + 1) as f64) * 100.0
                    } else {
                        0.0
                    };
                    let blocks_per_second = if elapsed > 0.0 {
                        (current - start) as f64 / elapsed
                    } else {
                        0.0
                    };
                    ep.publish_block_sync_progress(
                        current,
                        end,
                        progress_percent,
                        blocks_per_second,
                    )
                    .await;
                    last_height = current;
                }
                if current >= end {
                    break;
                }
            }
        }))
    } else {
        None
    };

    // Wait for validation thread (block_in_place keeps tokio worker free)
    let mut ibd_pipeline_shutdown = || {
        coord_handle.abort();
        drop(prefetch_input_tx_v2);
        drop(gap_fill_tx_v2);
        // Signal + wake the feeder so it exits even if it is parked on backpressure or a
        // blocking recv that the detached coordinator's sender clones keep open. Without this
        // the join below deadlocks and the real validation error is never returned.
        feeder_shutdown.store(true, std::sync::atomic::Ordering::Release);
        feeder_state.1.notify_all();
        let _ = feeder_handle.join();
        // Join all prefetch workers now that their input channels are closed (senders dropped
        // above). Workers detect channel closure and exit; joining here ensures the
        // Arc<IbdUtxoStore> → Arc<heed3::Env> chain is fully released before this function
        // returns, so the next session's create_ibd_utxo_standalone_db can open the same
        // LMDB path without "EnvOpenOptions::open failed".
        let n = prefetch_join_handles.len();
        if n > 0 {
            tracing::debug!("[IBD_SHUTDOWN] joining {} prefetch worker thread(s)…", n);
            for h in prefetch_join_handles.drain(..) {
                let _ = h.join();
            }
            tracing::debug!("[IBD_SHUTDOWN] all prefetch workers exited");
        }
    };
    match tokio::task::block_in_place(|| validation_handle.join()) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            ibd_pipeline_shutdown();
            return Err(e);
        }
        Err(panic) => {
            ibd_pipeline_shutdown();
            return Err(anyhow::anyhow!("Validation thread panicked: {:?}", panic));
        }
    }
    let validated_through = validation_height.load(Ordering::Relaxed);
    let effective_end_final = effective_end_live.load(Ordering::Relaxed);
    if validated_through < effective_end_final {
        ibd_pipeline_shutdown();
        return Err(anyhow::anyhow!(
            "Parallel IBD ended early: validated through height {} but need {}",
            validated_through,
            effective_end_final
        ));
    }
    if let Some(h) = progress_handle {
        let _ = h.await;
    }
    ibd_pipeline_shutdown();
    // Checkpoint thread exits when last committed export is within one interval of
    // end_h (see `checkpoint_export_thread_should_exit`). Do **not** rely on Arc drop
    // / cl==0 — that path only `continue`s and cannot terminate the loop.
    if let Some(h) = periodic_checkpoint_handle {
        let _ = h.join();
    }
    *utxo_set = match Arc::try_unwrap(utxo_mutex) {
        Ok(mutex) => mutex
            .into_inner()
            .map_err(|e| anyhow::anyhow!("IBD UTXO mutex poisoned: {e:?}"))?,
        Err(arc) => arc
            .lock()
            .map_err(|e| anyhow::anyhow!("IBD UTXO mutex poisoned: {e:?}"))?
            .clone(),
    };

    // F-C2: catch durable chain_info tip up to validated height before Phase 3.
    // Skip-path validation can leave tip hundreds of blocks behind (already_persisted).
    if let Err(e) = storage.sync_chain_info_to_height(validated_through) {
        warn!("[IBD_TIP_SYNC] sync_chain_info_to_height({validated_through}) failed: {e:#}");
    } else if let Ok(Some(tip)) = storage.chain().get_height() {
        if tip < validated_through {
            warn!(
                "[IBD_TIP_SYNC] chain tip {} still < validated {} after sync",
                tip, validated_through
            );
        } else {
            info!("[IBD_TIP_SYNC] durable chain tip caught up to {}", tip);
        }
    }
    // F-C3: refuse Phase 3 / Ok if durable tip still lags IBD end (export poison risk).
    let durable_tip = storage.chain().get_height().ok().flatten().unwrap_or(0);
    if durable_tip < effective_end_final {
        return Err(anyhow::anyhow!(
            "Parallel IBD durable tip lag: chain_tip={} validated={} need={} \
                 (refuse Phase 3 / export_h advance past block tip)",
            durable_tip,
            validated_through,
            effective_end_final
        ));
    }

    // Join download workers *before* Phase 3 so peer buffers / reorder leftovers
    // are freed before the tip-scale UTXO export. Validation already drained the range.
    //
    // Signal shutdown first: past body tip, `wan_tip_gap_crawl` used to keep
    // `is_done()==false` forever so workers never exited (live 2026-07-13: 18+ min
    // stall after tip sync with no Phase 3 log). Abort any still-hung tasks after grace.
    clear_live_assigner();
    assigner.request_shutdown();
    let n_dl = download_handles.len();
    info!(
        "[IBD_DOWNLOAD_JOIN] joining {} download worker(s) before Phase 3",
        n_dl
    );
    let join_grace = std::time::Duration::from_secs(
        std::env::var("BLVM_IBD_DOWNLOAD_JOIN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(45),
    );
    let join_deadline = std::time::Instant::now() + join_grace;
    let mut pending = download_handles;
    let mut joined = 0usize;
    let mut aborted = 0usize;
    while !pending.is_empty() {
        let mut i = 0;
        while i < pending.len() {
            if pending[i].1.is_finished() {
                let (chunk_start, handle) = pending.swap_remove(i);
                match handle.await {
                    Ok(()) => joined += 1,
                    Err(e) => {
                        debug!("Download task for chunk {} ended: {}", chunk_start, e);
                        joined += 1;
                    }
                }
            } else {
                i += 1;
            }
        }
        if pending.is_empty() {
            break;
        }
        if std::time::Instant::now() >= join_deadline {
            warn!(
                "[IBD_DOWNLOAD_JOIN] {} worker(s) still in-flight after {:?} — aborting so Phase 3 can proceed",
                pending.len(),
                join_grace
            );
            for (chunk_start, handle) in pending.drain(..) {
                handle.abort();
                let _ = handle.await;
                aborted += 1;
                debug!(
                    "Download task for chunk {} aborted after join grace",
                    chunk_start
                );
            }
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    info!(
        "[IBD_DOWNLOAD_JOIN] done (joined={}, aborted={})",
        joined, aborted
    );
    // Reclaim jemalloc retained from validation workers before tip export (coexist with
    // other large processes — blvm must adapt, not assume the host is empty).
    let _ = memory::maybe_purge_jemalloc_retained("phase3_pre_export");

    // Phase 3: promote last tip checkpoint to canonical UTXO tree (alias) — no monolithic
    // re-scan of the engine into a separate `ibd_utxos` when a tip ckpt already exists.
    // If export_h lags tip, one mid-IBD-sized catch-up export into the inactive ckpt, then alias.
    // Fallback: full streaming watermark into `ibd_utxos` only when no usable ckpt exists.
    if let Some(ref db) = utxo_engine_for_export {
        let tip_i32 = effective_end_final as i32;
        let tip_u = effective_end_final;
        let export_h = storage
            .chain()
            .get_engine_export_height()
            .ok()
            .flatten()
            .unwrap_or(0);
        let active_slot = storage.chain().get_engine_ckpt_slot().unwrap_or(0);
        let active_name = crate::storage::ibd_engine::ckpt_tree_for_slot(active_slot);
        let slot_h = storage
            .chain()
            .get_engine_ckpt_slot_height(active_slot)
            .unwrap_or(0);
        let ckpt_nonempty = storage
            .open_tree(active_name)
            .ok()
            .and_then(|t| t.is_empty().ok().map(|e| !e))
            .unwrap_or(false);
        let path = crate::storage::ibd_engine::phase3_path(export_h, tip_u, slot_h, ckpt_nonempty);
        info!(
            "IBD engine: Phase 3 path={path:?} export_h={export_h} tip={tip_u} \
                 active_ckpt={active_name} slot_h={slot_h} nonempty={ckpt_nonempty}"
        );

        let muhash_bytes: [u8; blvm_muhash::MUHASH_RUNNING_STATE_BYTES] = match path {
            crate::storage::ibd_engine::Phase3Finish::PromotedAlias => {
                storage
                    .prepare_heed3_for_phase3_promote()
                    .context("prepare heed3 for Phase 3 promote")?;
                storage
                    .chain()
                    .set_ibd_utxo_canonical_tree(active_name)
                    .context("set canonical IBD UTXO tree to active ckpt")?;
                info!(
                    "IBD engine: Phase 3 promote — canonical UTXO tree → {active_name} \
                         at height {tip_u} (no re-export)"
                );
                storage
                    .chain()
                    .get_engine_export_muhash()
                    .ok()
                    .flatten()
                    .unwrap_or([0u8; blvm_muhash::MUHASH_RUNNING_STATE_BYTES])
            }
            crate::storage::ibd_engine::Phase3Finish::CatchupThenAlias => {
                crate::storage::ibd_engine::set_gc_fence(tip_i32);
                storage
                    .prepare_heed3_for_phase3_promote()
                    .context("prepare heed3 for Phase 3 catch-up")?;
                let write_slot = crate::storage::ibd_engine::ckpt_inactive_slot(active_slot);
                let write_name = crate::storage::ibd_engine::ckpt_tree_for_slot(write_slot);
                info!(
                    "IBD engine: Phase 3 catch-up export {export_h} → {tip_u} into {write_name} \
                         (then promote; not a second full copy into ibd_utxos)"
                );
                let tree = storage
                    .open_tree(write_name)
                    .with_context(|| format!("open {write_name} for Phase 3 catch-up"))?;
                let (muhash, count, timings) =
                    crate::storage::ibd_engine::run_checkpoint_export_replace(
                        db,
                        &tree,
                        tip_i32,
                        storage.utxo_value_codec(),
                    )
                    .context("Phase 3 catch-up checkpoint export")?;
                crate::storage::ibd_engine::sync_tree_after_persist(tree.as_ref())
                    .context("Phase 3 catch-up sync")?;
                let bytes = muhash.serialize_running_state();
                storage
                    .chain()
                    .persist_engine_checkpoint_complete(
                        tip_u,
                        write_slot,
                        count as u64,
                        timings.wall_ms / 1000,
                        &bytes,
                    )
                    .context("persist Phase 3 catch-up checkpoint")?;
                storage
                    .chain()
                    .set_ibd_utxo_canonical_tree(write_name)
                    .context("set canonical IBD UTXO tree after catch-up")?;
                info!(
                    "IBD engine: Phase 3 catch-up done — canonical → {write_name} \
                         utxos={count} wall_ms={}",
                    timings.wall_ms
                );
                bytes
            }
            crate::storage::ibd_engine::Phase3Finish::FullWatermarkExport => {
                crate::storage::ibd_engine::set_gc_fence(tip_i32);
                storage
                    .prepare_heed3_for_tip_utxo_export()
                    .context("prepare heed3 for Phase 3 full watermark")?;
                info!(
                    "IBD engine: Phase 3 full watermark export (no usable ckpt; \
                         streaming; GC fence={tip_i32})..."
                );
                let tree = storage
                    .open_tree(crate::storage::ibd_engine::IBD_UTXOS_TREE)
                    .context("open ibd_utxos for Phase 3 watermark export")?;
                let muhash = crate::storage::ibd_engine::run_watermark_export(
                    db,
                    &tree,
                    tip_i32,
                    storage.utxo_value_codec(),
                )
                .context("Phase 3 watermark export")?;
                crate::storage::ibd_engine::sync_tree_after_persist(tree.as_ref())
                    .context("Phase 3 watermark export sync")?;
                storage
                    .chain()
                    .set_ibd_utxo_canonical_tree(crate::storage::ibd_engine::IBD_UTXOS_TREE)
                    .context("set canonical IBD UTXO tree to ibd_utxos")?;
                let bytes = muhash.serialize_running_state();
                storage
                    .chain()
                    .persist_engine_export_muhash_snapshot(&bytes)
                    .context("persist Phase 3 export MuHash snapshot")?;
                bytes
            }
        };

        storage
            .chain()
            .persist_ibd_utxo_flush_checkpoint(effective_end_final, &muhash_bytes)
            .context("persist Phase 3 watermark checkpoint")?;
        storage
            .chain()
            .persist_engine_export_height(effective_end_final)
            .context("persist Phase 3 engine export height")?;
        storage.flush().context("flush after Phase 3")?;
        info!(
            "IBD engine: Phase 3 complete at height {} (canonical={}, export_height durable)",
            effective_end_final,
            storage
                .chain()
                .get_ibd_utxo_canonical_tree()
                .unwrap_or_else(|_| "?".into()),
        );

        // IBD completed cleanly — remove the dirty flag so the next restart knows
        // this was a clean shutdown and won't force-wipe the engine unnecessarily.
        {
            let engine_path_clean =
                crate::config::ibd::ibd_engine_path(storage.data_dir().as_deref());
            let mut dirty_os = engine_path_clean.as_os_str().to_owned();
            dirty_os.push(".dirty");
            let _ = std::fs::remove_file(std::path::PathBuf::from(&dirty_os));
        }
    }

    // Isolated validation: coordinator drained all blocks; no local reorder buffer to check.

    // Log peer scoring summary
    info!("Peer scoring: {}", self_ibd.peer_scorer.summary());

    let blocks_synced = effective_end_final.saturating_sub(start_height) + 1;
    info!(
        "Parallel IBD completed: {} blocks synced (heights {} to {})",
        blocks_synced, start_height, effective_end_final
    );
    if let Some(ref ep) = event_publisher {
        let duration_secs = block_sync_start.elapsed().as_secs();
        ep.publish_block_sync_completed(effective_end_final, duration_secs)
            .await;
    }
    Ok(())
}

async fn run_ibd_coordinator(ctx: IbdCoordinatorCtx) {
    let IbdCoordinatorCtx {
        assigner_for_coord,
        mut block_rx,
        blockstore_for_coord,
        bootstrap_end,
        bridge_pending_max,
        chunk_size_for_ahead,
        confirmed_body_height_for_coord,
        coord_buffer_limit,
        coord_engine_mode,
        coord_session_id_for_task,
        download_timeout_secs_for_coord,
        effective_end_live_for_coord,
        feeder_state_for_coord,
        follow_tip_peer_ids_for_coord,
        gap_fill_tx_v2_for_coord,
        headers_max_failures_for_coord,
        headers_timeout_secs_for_coord,
        ibd_pv_for_coord,
        ibd_store_v2_for_coord,
        mut live_body_tip_for_coord,
        max_ahead_blocks,
        max_ahead_live,
        network_for_coord,
        peer_scorer_for_coord,
        prefetch_input_tx_v2_for_coord,
        ready_bridge_for_coord,
        sequential,
        stall_tx_for_coord,
        start_height,
        validation_height_for_coord,
        wan_multi_peer,
    } = ctx;
    let mut reorder_buffer: std::collections::BTreeMap<u64, (SharedBlock, SharedWitnesses)> =
        std::collections::BTreeMap::new();
    // `next_prefetch_height`: tracks the watermark for stale-duplicate detection and
    // stall logging. With out-of-order dispatch (below) this is the *max* height + 1
    // of all blocks we have dispatched so far, used only for stale-dup filtering.
    let mut next_prefetch_height = start_height;
    // Heights already dispatched to prefetch workers. Prevents re-dispatching stale
    // duplicates from re-queued chunks. Pruned periodically against validation_height.
    let mut dispatched: rustc_hash::FxHashSet<u64> = rustc_hash::FxHashSet::default();
    let mut total_received = 0u64;
    let mut coord_stall_count = 0u64;
    const BATCH_DRAIN_LIMIT: usize = 2000; // 10K BPS: larger batches reduce recv overhead
    let mut batch: Vec<(u64, SharedBlock, SharedWitnesses)> = Vec::with_capacity(BATCH_DRAIN_LIMIT);
    // S2: Reuse buffer for block_input_keys (avoids alloc per block)
    let mut coord_keys_buf: Vec<OutPointKey> = Vec::new();
    let mut coord_tx_ids_buf: Vec<Hash> = Vec::new();
    // Scratch buffer for out-of-order dispatch: heights to dispatch this iteration.
    // Reused across loop iterations to avoid a per-loop Vec<u64> allocation.
    let mut dispatch_heights_buf: Vec<u64> = Vec::new();
    // W29: admit window is recomputed each loop (WAN tip crawl uses tight 64).
    let mut local_gap_miss_logged: rustc_hash::FxHashSet<u64> = rustc_hash::FxHashSet::default();
    let mut try_inject_local_gap = |reorder_buffer: &mut std::collections::BTreeMap<
        u64,
        (SharedBlock, SharedWitnesses),
    >,
                                    validation_height: u64,
                                    already_dispatched: &rustc_hash::FxHashSet<u64>,
                                    tip_in_pipeline: bool|
     -> bool {
        let inject = tokio::task::block_in_place(|| {
            coordinator_inject_local_gap(
                blockstore_for_coord.as_ref(),
                ibd_pv_for_coord,
                validation_height.saturating_add(1),
                confirmed_body_height_for_coord,
                validation_height,
                reorder_buffer,
                already_dispatched,
                &mut local_gap_miss_logged,
                tip_in_pipeline,
            )
        });
        let injected = match inject {
            Ok(true) => true,
            Ok(false) => false,
            Err(e) => {
                warn!(
                    "[IBD_LOCAL_GAP] inject failed at height {}: {e:#}",
                    validation_height.saturating_add(1)
                );
                false
            }
        };
        let next = validation_height.saturating_add(1);
        let live = assigner_for_coord.wan_body_tip();
        if should_pull_wan_body_tip_on_inject(ibd_local_gap_fill_enabled(), injected, live, next) {
            let pulled = pull_wan_body_tip_for_hole(live, next);
            if pulled < live {
                warn!(
                    "[IBD_HOLE_PULL_TIP] live_body_tip {} → {} (disk miss at {})",
                    live, pulled, next
                );
                assigner_for_coord.set_wan_body_tip(pulled);
            }
        }
        injected
    };
    // Dispatch a block to prefetch workers. The prefetch pool warm-loads input UTXOs
    // (cache miss → RocksDB MultiGet) on N background threads before the validation
    // worker ever sees the block — so the validation worker only has to do CPU work
    // (script/sig/state checks) and never blocks on disk IO. Order is preserved by
    // the OrderedReadyBridge wrapping the workers' output channel.
    //
    // Channel strategy: try the primary prefetch queue first; on Full, overflow to the
    // gap-fill pool (small bounded queue with the same worker pool semantics). Both
    // full → block on prefetch (natural backpressure to the coordinator). All sends
    // are wrapped in `block_in_place` because crossbeam's `send` is sync-blocking and
    // would otherwise block the tokio runtime worker.
    // `Some` = both prefetch queues full — caller must put the body back in
    // reorder. Live genesis-b 91698: unbounded `.send` after CHEESE dump
    // froze the coordinator (CRAWL silent, SLA/C1T never ran).
    let dispatch_to_prefetch = |item: (
        Arc<IbdUtxoStore>,
        Vec<OutPointKey>,
        Vec<Hash>,
        u64,
        SharedBlock,
        SharedWitnesses,
        bool,
    )|
     -> Option<(u64, SharedBlock, SharedWitnesses)> {
        let (_store, keys, tx_ids, h, block, witnesses, engine_mode) = item;
        if engine_mode {
            let bridge = ready_bridge_for_coord
                .as_ref()
                .expect("engine coordinator bridge");
            let ready: ReadyItem = (
                h,
                block,
                witnesses,
                keys,
                prefetch::engine_empty_prefetch_arc(),
                tx_ids,
                prefetch::engine_empty_spec_adds(),
            );
            // W23: in-order tip → feeder directly (skip bounded ready channel).
            tokio::task::block_in_place(|| {
                let vh = validation_height_for_coord.load(Ordering::Relaxed);
                if let Some(ready) =
                    bridge.try_emit_in_order_to_feeder(h, ready, &feeder_state_for_coord, vh)
                {
                    bridge.worker_complete(h, ready, vh);
                }
            });
            return None;
        }
        // W21: validation tip / bridge next_expected must not sit behind thousands of
        // ahead prefetches on the bulk queue (live WAN TIP_CRAWL: tip in reorder,
        // bridge_pending=0, feeder=0, ~0.1 BPS while tip_covering=8+). Prefer the
        // small gap-fill pool so tip reaches OrderedReadyBridge first.
        let tip_priority = ready_bridge_for_coord
            .as_ref()
            .and_then(|b| b.next_expected())
            == Some(h)
            || validation_height_for_coord
                .load(Ordering::Relaxed)
                .saturating_add(1)
                == h;
        tokio::task::block_in_place(|| {
            let item = (_store, keys, tx_ids, h, block, witnesses, false);
            let reject_full = |it: (
                Arc<IbdUtxoStore>,
                Vec<OutPointKey>,
                Vec<Hash>,
                u64,
                SharedBlock,
                SharedWitnesses,
                bool,
            )|
             -> Option<(u64, SharedBlock, SharedWitnesses)> {
                static LAST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let prev = LAST_MS.load(Ordering::Relaxed);
                if now_ms.saturating_sub(prev) >= 5_000 {
                    LAST_MS.store(now_ms, Ordering::Relaxed);
                    warn!(
                        "[IBD_PREFETCH_FULL] h={} — both queues full, put back (do not block coordinator)",
                        it.3
                    );
                }
                Some((it.3, it.4, it.5))
            };
            if tip_priority {
                let item = match gap_fill_tx_v2_for_coord.try_send(item) {
                    Ok(()) => return None,
                    Err(crossbeam_channel::TrySendError::Full(it)) => it,
                    Err(crossbeam_channel::TrySendError::Disconnected(_)) => return None,
                };
                return match prefetch_input_tx_v2_for_coord.try_send(item) {
                    Ok(()) => None,
                    Err(crossbeam_channel::TrySendError::Full(it)) => reject_full(it),
                    Err(crossbeam_channel::TrySendError::Disconnected(_)) => None,
                };
            }
            let item = match prefetch_input_tx_v2_for_coord.try_send(item) {
                Ok(()) => return None,
                Err(crossbeam_channel::TrySendError::Full(it)) => it,
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => return None,
            };
            match gap_fill_tx_v2_for_coord.try_send(item) {
                Ok(()) => None,
                Err(crossbeam_channel::TrySendError::Full(it)) => reject_full(it),
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => None,
            }
        })
    };
    info!("Coordinator: started, awaiting blocks from download workers");
    // Stall timeout: 90s for LAN (Bitcoin Core needs 40-80s for dense Satoshi Dice era
    // chunks at h=285k-310k). WAN: must clear tip soft-retry budget (25s×3≈75) and
    // approach tip SLA (90s). Live A6h: clamp 15–30s fired "Coordinator stall at gap"
    // mid soft-retry → aborted deep tip pipes → owner tenure p50≈5s → ~4.5 blk/s.
    // Override via BLVM_IBD_COORD_STALL_SECS.
    let coord_stall_secs: u64 = if wan_multi_peer {
        download_timeout_secs_for_coord.clamp(75, 120)
    } else {
        90
    };
    let coord_stall_secs = std::env::var("BLVM_IBD_COORD_STALL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(coord_stall_secs);
    let coord_stall_log_secs = coord_stall_secs;
    let mut coord_buffer_full_since: Option<std::time::Instant> = None;
    // B2b: debounce AHEAD_CLAMP logs; track feeder-nonempty duration before restore.
    let mut ahead_clamp_logged_at: Option<std::time::Instant> = None;
    let mut feeder_nonempty_since: Option<std::time::Instant> = None;
    let mut feeder_starve_since: Option<std::time::Instant> = None;
    let mut feeder_starve_logged_at: Option<std::time::Instant> = None;
    let mut tip_crawl_logged_at: Option<std::time::Instant> = None;
    let mut crawl_recv_bytes: u64 = 0;
    let mut crawl_recv_at = std::time::Instant::now();
    let mut crawl_peer_bytes: HashMap<String, u64> = HashMap::new();
    let mut a6m_check_at: Option<std::time::Instant> = None;
    let mut tip_nudge_last: Option<std::time::Instant> = None;
    let mut ibd_ready_refresh_at = std::time::Instant::now() - Duration::from_secs(10);
    let mut tip_follow_poll_at =
        std::time::Instant::now() - Duration::from_secs(ibd_follow_tip_poll_secs());
    let mut tip_follow_headers_busy = false;
    // W20: tip handed to prefetch (dispatched, not yet in bridge). Case C must not
    // clear/requeue during this window or GAP_STREAM storms kill tip crawl.
    let mut tip_inflight_since: Option<(u64, std::time::Instant)> = None;
    // R-190: covering=0 and H not dispatched (STREAM then drop).
    // W22: bridge cursor ahead of validation (tip emitted, not yet consumed / lost).
    let mut bridge_ahead_since: Option<(u64, std::time::Instant)> = None;
    let tip_inflight_grace = Duration::from_secs(
        std::env::var("BLVM_IBD_TIP_INFLIGHT_GRACE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10),
    );
    #[cfg(target_os = "linux")]
    let mut coord_emergency_log = std::time::Instant::now();
    loop {
        // Inject-miss may have pulled the warehouse tip so GetData owns a leftover hole.
        live_body_tip_for_coord = assigner_for_coord
            .wan_body_tip()
            .min(live_body_tip_for_coord);
        memory::sync_reorder_buffer_stats(&reorder_buffer);
        let cliff_iter_t0 = std::time::Instant::now();
        // Cliff probe: 1 Hz heartbeat so silence windows still show coordinator alive
        // and whether tip sits in reorder / pending / channel.
        if synthetic_wan::bulk_local_disk_stream() {
            static CLIFF_HB_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let prev = CLIFF_HB_AT.load(Ordering::Relaxed);
            if now_ms.saturating_sub(prev) >= 1000 {
                CLIFF_HB_AT.store(now_ms, Ordering::Relaxed);
                let tip = validation_height_for_coord
                    .load(Ordering::Relaxed)
                    .saturating_add(1);
                let bnext = ready_bridge_for_coord
                    .as_ref()
                    .and_then(|b| b.next_expected());
                let in_ro = reorder_buffer.contains_key(&tip);
                let in_pend = ready_bridge_for_coord
                    .as_ref()
                    .is_some_and(|b| b.pending_contains(tip));
                let in_feed = {
                    let g = feeder_state_for_coord.0.lock();
                    g.0.get(tip).is_some()
                };
                warn!(
                    "[IBD_CLIFF_HB] tip={} bnext={:?} reorder_has={} pending_has={} feeder_has={} dispatched={} ch_len={} reorder_len={} feeder_len={}",
                    tip,
                    bnext,
                    in_ro,
                    in_pend,
                    in_feed,
                    dispatched.contains(&tip),
                    block_rx.len(),
                    reorder_buffer.len(),
                    IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed),
                );
            }
        }
        // W67: drain download→coordinator channel *before* peer_manager awaits.
        // Live 2026-07-17 h=239217: workers filled `block_tx` and blocked on
        // `send().await` while this loop sat in `peer_ibd_ready` (holds
        // `peer_manager`) → 7+ min freeze, TIP_CRAWL silent, feeder=0,
        // reorder≈292 ahead of missing tip. Drain first; defer ready refresh
        // when the channel is still non-empty.
        {
            let next_needed = validation_height_for_coord
                .load(Ordering::Relaxed)
                .saturating_add(1);
            let wan_tip_crawl_early = next_needed > live_body_tip_for_coord;
            let bulk_early = wan_bulk_catchup(
                assigner_for_coord
                    .header_tip()
                    .max(effective_end_live_for_coord.load(Ordering::Relaxed)),
                next_needed,
            );
            let admit_win = admit_window_for_height(
                next_needed,
                effective_gap_admit_window(wan_tip_crawl_early, bulk_early),
            );
            let early_drain_t0 = std::time::Instant::now();
            // W6/N14: release-side tip latch before channel drain / bulk admit.
            if let Some((h, block, witnesses)) = tip_release::take_tip_release() {
                total_received += 1;
                if dispatched.contains(&h) {
                    dispatched.remove(&h);
                }
                let _ = insert_reorder_gap_aware(
                    &mut reorder_buffer,
                    h,
                    block,
                    witnesses,
                    next_needed,
                    coord_buffer_limit,
                    admit_win,
                    bridge_pending_max,
                );
            }
            late_body::drain_late_admits(
                &mut reorder_buffer,
                next_needed,
                coord_buffer_limit,
                admit_win,
                bridge_pending_max,
            );
            let _ = drain_block_rx_tip_first(
                &mut block_rx,
                &mut reorder_buffer,
                &mut dispatched,
                next_needed,
                coord_buffer_limit,
                admit_win,
                bridge_pending_max,
                &mut total_received,
                BATCH_DRAIN_LIMIT,
            );
            if synthetic_wan::bulk_local_disk_stream()
                && cliff_iter_t0.elapsed() >= Duration::from_millis(500)
            {
                warn!(
                    "[IBD_CLIFF_POST_DRAIN] iter_ms={} drain_ms={} tip={} ch_len={} reorder={}",
                    cliff_iter_t0.elapsed().as_millis(),
                    early_drain_t0.elapsed().as_millis(),
                    next_needed,
                    block_rx.len(),
                    reorder_buffer.len(),
                );
            }
        }
        if PARALLEL_IBD_SESSION_ID.load(Ordering::Acquire) != coord_session_id_for_task {
            info!(
                "Coordinator: session {} superseded — exiting",
                coord_session_id_for_task
            );
            break;
        }
        // F-C4: extend effective_end while peers advance (soak G0 needs ≥15 min WAN).
        // W73: never park the coordinator here without a deadline — a hung
        // `download_headers` / peer_manager await stops W67 channel drains and
        // freezes tip crawl (live: TIP_CRAWL silent after 13:51:32, IBD abort at
        // 262716 after ~1080s).
        if ibd_follow_tip_enabled()
            && tip_follow_poll_at.elapsed() >= Duration::from_secs(ibd_follow_tip_poll_secs())
        {
            let follow_next = validation_height_for_coord
                .load(Ordering::Relaxed)
                .saturating_add(1);
            let follow_end = effective_end_live_for_coord.load(Ordering::Relaxed);
            let follow_feeder = IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed) as u64;
            if tip_follow_may_block_coord(
                follow_feeder,
                follow_next,
                follow_end,
                ibd_end_height_cap(),
            ) {
                tip_follow_poll_at = std::time::Instant::now();
                if let Some(ref net) = network_for_coord {
                    let follow = async {
                        let Some(peer_tip) = net.get_highest_peer_start_height_async().await else {
                            return;
                        };
                        let current_end = effective_end_live_for_coord.load(Ordering::Relaxed);
                        let stored_ht = blockstore_for_coord
                            .highest_stored_height()
                            .ok()
                            .flatten()
                            .unwrap_or(0);
                        if peer_tip > stored_ht && !tip_follow_headers_busy {
                            tip_follow_headers_busy = true;
                            let header_start = stored_ht.saturating_add(1);
                            match headers::download_headers(
                                Arc::clone(&peer_scorer_for_coord),
                                header_start,
                                peer_tip,
                                &follow_tip_peer_ids_for_coord,
                                blockstore_for_coord.as_ref(),
                                Some(Arc::clone(net)),
                                headers_timeout_secs_for_coord,
                                headers_max_failures_for_coord,
                                None,
                            )
                            .await
                            {
                                Ok(result) => {
                                    assigner_for_coord.set_header_tip(result.tip_height);
                                    info!(
                                        "[IBD_TIP_FOLLOW] headers refreshed {} → {}",
                                        stored_ht, result.tip_height
                                    );
                                }
                                Err(e) => {
                                    warn!("[IBD_TIP_FOLLOW] header refresh failed: {e:#}");
                                }
                            }
                            tip_follow_headers_busy = false;
                        }
                        let header_tip = blockstore_for_coord
                            .highest_stored_height()
                            .ok()
                            .flatten()
                            .unwrap_or(stored_ht);
                        if let Some(new_end) =
                            tip_follow_new_effective_end(current_end, peer_tip, header_tip)
                        {
                            effective_end_live_for_coord.store(new_end, Ordering::Release);
                            assigner_for_coord.set_ibd_end_height(new_end);
                            let vh = validation_height_for_coord.load(Ordering::Relaxed);
                            info!(
                                "[IBD_TIP_FOLLOW] extended effective_end {} → {} \
                                     (vh={} peer_tip={} header_tip={})",
                                current_end, new_end, vh, peer_tip, header_tip
                            );
                        }
                    };
                    if tokio::time::timeout(Duration::from_secs(5), follow)
                        .await
                        .is_err()
                    {
                        tip_follow_headers_busy = false;
                        warn!(
                            "[IBD_TIP_FOLLOW_TIMEOUT] 5s — skipped to keep tip channel draining (W73)"
                        );
                    }
                }
            }
        }
        let effective_end_now = effective_end_live_for_coord.load(Ordering::Relaxed);
        if validation_height_for_coord.load(Ordering::Relaxed) >= effective_end_now {
            info!(
                "Coordinator: validation reached effective end height {} — stopping block dispatch",
                effective_end_now
            );
            // Unblock download workers: without this, wan_tip_gap_crawl keeps
            // is_done()==false forever past body tip and Phase 3 hangs on join.
            assigner_for_coord.request_shutdown();
            break;
        }
        // P0-A: refresh handshake-ready peers for WAN tip-owner assign (~1/s).
        // Include live IBD peer addresses (not only scored keys) so new/replacement
        // peers can become tip owners after SLA rotate.
        // A6d: also refresh ChunkAssigner scores from live PeerScorer.
        // W67: skip / time-box while download channel has backlog (deadlock risk).
        let channel_backlog = block_rx.len();
        if ibd_ready_refresh_at.elapsed() >= Duration::from_secs(1) && channel_backlog == 0 {
            ibd_ready_refresh_at = std::time::Instant::now();
            let ready_refresh_t0 = std::time::Instant::now();
            if let Some(ref net) = network_for_coord {
                let net = Arc::clone(net);
                let assigner = Arc::clone(&assigner_for_coord);
                let scorer = Arc::clone(&peer_scorer_for_coord);
                let blockstore = Arc::clone(&blockstore_for_coord);
                let live_body = live_body_tip_for_coord;
                let vh = Arc::clone(&validation_height_for_coord);
                let refresh = async move {
                    let mut candidates: HashSet<String> =
                        assigner.peer_ids_for_ibd_ready().into_iter().collect();
                    // Local-disk bulk: never scan TCP peers (vacuous LIMITED → DNS).
                    // TRUE WAN: connected-only `.await` (R-241). The sync IBD
                    // peer-address scan is block_in_place and defeats the 250ms
                    // timeout (R-240: CRAWL silent after failover @107124, no
                    // IBD_READY_REFRESH_TIMEOUT).
                    if !synthetic_wan::bulk_local_disk_stream() {
                        for addr in net.peer_addresses_for_ibd_connected().await {
                            candidates.insert(addr.to_string());
                        }
                    }
                    let mut ready = HashSet::new();
                    for p in candidates.iter() {
                        if is_snapshot_sourced_peer(p) {
                            ready.insert(p.clone());
                            continue;
                        }
                        if let Ok(addr) = p.parse::<std::net::SocketAddr>() {
                            if net.peer_ibd_ready(addr).await {
                                ready.insert(p.clone());
                            }
                        }
                    }
                    // Active local-disk workers are always ready (no handshake).
                    for p in assigner.active_download_worker_ids() {
                        if is_snapshot_sourced_peer(&p) {
                            ready.insert(p);
                        }
                    }
                    let n = ready.len();
                    let mut scored: Vec<(String, f64)> = assigner
                        .active_download_worker_ids()
                        .into_iter()
                        .filter_map(|p| {
                            let addr = p.parse::<std::net::SocketAddr>().ok()?;
                            Some((p, scorer.tip_owner_score(&addr)))
                        })
                        .collect();
                    if let Some(pref) = assigner.preferred_tip_owner() {
                        if let Ok(addr) = pref.parse::<std::net::SocketAddr>() {
                            if !scored.iter().any(|(p, _)| p == &pref) {
                                scored.push((pref, scorer.tip_owner_score(&addr)));
                            }
                        }
                    }
                    if !scored.is_empty() {
                        scored.sort_by(|a, b| {
                            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                        });
                        assigner.set_peer_scores(&scored);
                    }
                    assigner.set_ibd_ready_peers(ready);
                    if let Ok(Some(ht)) = blockstore.highest_stored_height() {
                        assigner.set_header_tip(ht);
                    }
                    if n == 0
                        && live_body > 0
                        && vh.load(Ordering::Relaxed).saturating_add(1) > live_body
                    {
                        static READY_EMPTY_LOG: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let prev = READY_EMPTY_LOG.load(Ordering::Relaxed);
                        if now.saturating_sub(prev) >= 30 {
                            READY_EMPTY_LOG.store(now, Ordering::Relaxed);
                            warn!(
                                "[IBD_READY_EMPTY] no handshake-complete peers — tip-owner assign gated until VerAck"
                            );
                        }
                    }
                };
                if tokio::time::timeout(Duration::from_millis(250), refresh)
                    .await
                    .is_err()
                {
                    warn!(
                        "[IBD_READY_REFRESH_TIMEOUT] 250ms — skipped to keep tip channel draining (W67)"
                    );
                }
                if !synthetic_wan::bulk_local_disk_stream() {
                    if let Some(n) = network_for_coord.as_ref() {
                        tip_probe::coord_tick(
                            Arc::clone(n),
                            Arc::clone(&blockstore_for_coord),
                            Arc::clone(&assigner_for_coord),
                            validation_height_for_coord.load(Ordering::Relaxed),
                        );
                    }
                }
                // Flush-path tip: copy only if something live published.
                // GAP_PERSIST must not publish (R-196 leftover genesis).
                // Do not walk or range-probe (closed cascade).
                if body_warehouse_enabled() && synthetic_wan::body_tip_override().is_none() {
                    let written = flush_path_body_tip();
                    if written > live_body_tip_for_coord {
                        info!(
                            "[IBD_FLUSH_PATH_TIP] live_body_tip {} → {} (flush path)",
                            live_body_tip_for_coord, written
                        );
                        live_body_tip_for_coord = written;
                        assigner_for_coord.set_wan_body_tip(written);
                    }
                }
                if synthetic_wan::bulk_local_disk_stream()
                    && ready_refresh_t0.elapsed() >= Duration::from_millis(200)
                {
                    warn!(
                        "[IBD_CLIFF_READY_REFRESH] ms={} ch_len={}",
                        ready_refresh_t0.elapsed().as_millis(),
                        block_rx.len(),
                    );
                }
            }
        } else if channel_backlog > 0 && ibd_ready_refresh_at.elapsed() >= Duration::from_secs(1) {
            // Keep trying drain path; don't stampede peer_manager while workers block on send.
            static BACKLOG_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let prev = BACKLOG_LOG.load(Ordering::Relaxed);
            if now.saturating_sub(prev) >= 5 {
                BACKLOG_LOG.store(now, Ordering::Relaxed);
                warn!(
                    "[IBD_DOWNLOAD_CHANNEL_BACKLOG] len={} — deferring peer-ready refresh (W67)",
                    channel_backlog
                );
            }
        }
        // P1-A: WAN tip gap idle — force tip-owner re-arm when covering=0 but peers ready.
        // Dens parity (2026-08-03): do NOT treat `!reorder.contains(tip)` alone as a gap.
        // Healthy tip_taken / feeder / bridge-pending also leave tip out of reorder; nudging
        // every 1s there thrashes covering (grow64j TIP_NUDGE×81 vs grow80×18) while
        // wait_feeder% climbs 27→47. Only nudge on a true tip-body gap.
        // Manual REVERT: pairing nudge with WAN_TIP_FORCE_REQUEUE on ahead>0 regressed
        // tip90 ~97→69 (nudgefix cell) — leave (H,H) force to W73 SLA / covering0 Case C.
        {
            let next_needed = validation_height_for_coord
                .load(Ordering::Relaxed)
                .saturating_add(1);
            let wan_gap = next_needed > live_body_tip_for_coord;
            let tip_in_reorder_nudge = reorder_buffer.contains_key(&next_needed);
            let tip_in_feeder_nudge = {
                let g = feeder_state_for_coord.0.lock();
                g.0.get(next_needed).is_some()
            };
            let tip_in_bridge_nudge = ready_bridge_for_coord
                .as_ref()
                .is_some_and(|b| b.pending_contains(next_needed));
            let tip_taken_nudge = tip_stage::tip_taken_by_validation(next_needed);
            if wan_gap
                && assigner_for_coord.is_bootstrap_complete()
                && tip_nudge_true_body_gap(
                    tip_in_reorder_nudge,
                    tip_in_feeder_nudge,
                    tip_in_bridge_nudge,
                    tip_taken_nudge,
                )
            {
                let (covering, _, _) = assigner_for_coord.tip_flight_diag();
                let ready = assigner_for_coord.ibd_ready_peer_count();
                // Re-arm only when covering=0. Do NOT nudge while a tip flight exists:
                // floor_sticky "idle" is true for mid-download peers that have not
                // GAP_STREAM'd yet. Live genesis evidence (2026-07-17): every ~1s
                // UPGRADE sticky@0.001→better_worker + blacklist(score<0.05) →
                // IBD_TIP_BLACKLIST abort of the in-flight tip chunk → early BPS
                // collapse / 35s+ tip freezes. Tip-SLA rotate remains the stuck-owner path.
                if covering == 0 {
                    let should_nudge = tip_nudge_last
                        .map(|t| t.elapsed() >= Duration::from_secs(1))
                        .unwrap_or(true);
                    if should_nudge {
                        tip_nudge_last = Some(std::time::Instant::now());
                        if assigner_for_coord.nudge_wan_tip_owner() {
                            warn!(
                                "[IBD_TIP_NUDGE] covering={} ready={} tip={} — forced tip-owner re-arm",
                                covering, ready, next_needed
                            );
                        }
                    }
                }
            }
        }
        let dynamic_buffer_limit = coord_buffer_limit;
        // W29: near-tip crawl uses tight admit (64); bulk catch-up uses deep admit;
        // local/body uses 256. Genesis body_tip=0 is still WAN crawl (match assigner).
        let wan_tip_crawl_now = validation_height_for_coord
            .load(Ordering::Relaxed)
            .saturating_add(1)
            > live_body_tip_for_coord;
        let next_for_admit = validation_height_for_coord
            .load(Ordering::Relaxed)
            .saturating_add(1);
        let bulk_catchup_now = wan_bulk_catchup(
            assigner_for_coord
                .header_tip()
                .max(effective_end_live_for_coord.load(Ordering::Relaxed)),
            next_for_admit,
        );
        let admit_window = admit_window_for_height(
            next_for_admit,
            effective_gap_admit_window(wan_tip_crawl_now, bulk_catchup_now),
        );
        let coord_stall_effective_secs = if assigner_for_coord.is_bootstrap_complete() {
            coord_stall_log_secs
        } else {
            // Bootstrap is single-worker; short stall + requeue only churns micro-chunks.
            coord_stall_log_secs.saturating_mul(6).max(30)
        };
        if synthetic_wan::bulk_local_disk_stream()
            && cliff_iter_t0.elapsed() >= Duration::from_millis(500)
        {
            warn!(
                "[IBD_CLIFF_PRE_BOOT] ms={} tip={} ch_len={} reorder={}",
                cliff_iter_t0.elapsed().as_millis(),
                validation_height_for_coord
                    .load(Ordering::Relaxed)
                    .saturating_add(1),
                block_rx.len(),
                reorder_buffer.len(),
            );
        }
        // S2c: evict far-ahead reorder entries when gap/bridge pressure is active.
        let cliff_boot_t0 = std::time::Instant::now();
        if assigner_for_coord.is_bootstrap_complete() {
            let val_h = validation_height_for_coord.load(Ordering::Relaxed);
            let next_needed = val_h.saturating_add(1);
            let gap_missing_coord = !reorder_buffer.contains_key(&next_needed);
            let feeder_len_early = IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
            let wan_tip_crawl_early = next_needed > live_body_tip_for_coord;

            // W34j: log sustained feeder starvation with reorder runway.
            // dest-ab @255073: reorder=3 reset this timer 49s while a
            // 438 BPS hero held a 127-wide cover. dest-ac FAIL armed
            // that path for mute preferred too. KEEP hero only.
            let ahead_n = reorder_ahead_buffered(&reorder_buffer, next_needed);
            let first_ah = reorder_first_ahead(&reorder_buffer, next_needed);
            let holes_now = IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
            let fast_sparse = cheese_fast_hero_sparse_sit(
                gap_missing_coord,
                holes_now,
                first_ah,
                next_needed,
                assigner_for_coord.preferred_meets_keep_bps(),
                reorder_buffer.len(),
            );
            if wan_tip_crawl_early
                && feeder_len_early == 0
                && (reorder_buffer.len() >= 8 || fast_sparse)
            {
                let since = feeder_starve_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= Duration::from_secs(2) {
                    let (covering, flight_ranges, busy_peers) =
                        assigner_for_coord.tip_flight_diag();
                    let should_log = feeder_starve_logged_at
                        .map(|t| t.elapsed() >= Duration::from_secs(10))
                        .unwrap_or(true);
                    if should_log {
                        feeder_starve_logged_at = Some(std::time::Instant::now());
                        let tip_in = reorder_buffer.contains_key(&next_needed);
                        let contig = reorder_contig_runway(&reorder_buffer, next_needed);
                        // FEEDER_STARVE implies feeder empty — tip not in feeder.
                        let mode = tip_runway_mode(tip_in, contig, ahead_n, holes_now, false);
                        warn!(
                            "[IBD_FEEDER_STARVE] mode={} tip={} tip_in_reorder={} contig_runway={} ahead_buffered={} first_ahead={:?} feeder=0 reorder={} holes={} covering={} in_flight_ranges={} busy={} ready={} gap_missing={} — {}",
                            mode,
                            next_needed,
                            tip_in,
                            contig,
                            ahead_n,
                            first_ah,
                            reorder_buffer.len(),
                            holes_now,
                            covering,
                            flight_ranges,
                            busy_peers,
                            assigner_for_coord.ibd_ready_peer_count(),
                            gap_missing_coord,
                            if gap_missing_coord && ahead_n > 0 {
                                "TIP_HOLE_AHEAD: tip missing while ahead sits in reorder — multi-peer stripe delivered past tip"
                            } else if gap_missing_coord {
                                "EMPTY_TIP: tip missing, nothing ahead — tip-serial wait"
                            } else {
                                "validation waiting on commit (W34h prefetch active)"
                            }
                        );
                    }
                    // Pin maze deleted. Starve preds deleted. Starve logs only.
                }
            } else {
                feeder_starve_since = None;
            }

            // W34b: body→feeder SLA — evict/peel before housekeeping stalls handoff.
            if tip_stage::body_feeder_sla_breached()
                && tip_stage::tracked_tip_height() == next_needed
                && !gap_missing_coord
            {
                warn!(
                    "[IBD_HANDOFF_SLA] tip {} body→feeder >{}s — evicting far reorder + peeling bridge",
                    next_needed,
                    tip_stage::handoff_sla_secs()
                );
                let _ = evict_reorder_gap_pressure(
                    &mut reorder_buffer,
                    next_needed,
                    dynamic_buffer_limit,
                    admit_window.min(64),
                    bridge_pending_max,
                );
                if let Some(ref bridge) = ready_bridge_for_coord {
                    let _ = bridge.evict_far_ahead_pending_ex(
                        next_needed,
                        admit_window.min(64),
                        true,
                        bridge_pending_max,
                        wan_tip_crawl_early,
                    );
                    for h in bridge.take_evicted_heights() {
                        dispatched.remove(&h);
                    }
                }
            }

            // W34e/W54: tip handoff before admit/evict when tip stranded in reorder.
            let tip_in_feeder_early = {
                let g = feeder_state_for_coord.0.lock();
                g.0.get(next_needed).is_some()
            };
            let w34_t0 = std::time::Instant::now();
            if let Some((block, witnesses, _, was_dispatched)) = prepare_coordinator_tip_handoff(
                next_needed,
                gap_missing_coord,
                feeder_len_early,
                true,
                &mut reorder_buffer,
                &mut dispatched,
                ready_bridge_for_coord.as_deref(),
                admit_window,
                bridge_pending_max,
                wan_tip_crawl_early,
                tip_in_feeder_early,
            ) {
                tip_inflight_since = Some((next_needed, std::time::Instant::now()));
                if next_needed >= next_prefetch_height {
                    next_prefetch_height = next_needed + 1;
                }
                prepare_coord_dispatch_bufs(
                    coord_engine_mode,
                    &block,
                    &mut coord_tx_ids_buf,
                    &mut coord_keys_buf,
                );
                let store = &ibd_store_v2_for_coord;
                let keys_owned = std::mem::take(&mut coord_keys_buf);
                let tx_ids_owned = std::mem::take(&mut coord_tx_ids_buf);
                if let Some((h, block, witnesses)) = dispatch_to_prefetch((
                    Arc::clone(store),
                    keys_owned,
                    tx_ids_owned,
                    next_needed,
                    block,
                    witnesses,
                    coord_engine_mode,
                )) {
                    dispatched.remove(&h);
                    reorder_buffer.insert(h, (block, witnesses));
                }
                if synthetic_wan::bulk_local_disk_stream()
                    && w34_t0.elapsed() >= Duration::from_millis(500)
                {
                    warn!(
                        "[IBD_CLIFF_W34] tip={} dispatch_ms={} was_dispatched={}",
                        next_needed,
                        w34_t0.elapsed().as_millis(),
                        was_dispatched,
                    );
                }
                if let Some(ref bridge) = ready_bridge_for_coord {
                    let flushed = bridge.try_flush();
                    let bnext = bridge.next_expected();
                    let bridge_waiting = bnext == Some(next_needed);
                    if bnext == Some(next_needed) {
                        let tip_in_feeder = {
                            let g = feeder_state_for_coord.0.lock();
                            g.0.get(next_needed).is_some()
                        };
                        if !tip_in_feeder {
                            warn!(
                                "[IBD_TIP_HANDOFF_MISS] tip {} still at bridge_next after W34 dispatch (flushed={}, was_dispatched={})",
                                next_needed, flushed, was_dispatched
                            );
                        } else if was_dispatched {
                            warn!(
                                "[IBD_TIP_HANDOFF] W34 forced tip {} into bridge (flushed={}, bridge_waiting={})",
                                next_needed, flushed, bridge_waiting
                            );
                        } else {
                            info!(
                                "[IBD_TIP_HANDOFF] W34 tip {} → bridge (flushed={}, bridge_next={:?})",
                                next_needed, flushed, bnext
                            );
                        }
                    }
                }
            }

            let _ = evict_reorder_gap_pressure(
                &mut reorder_buffer,
                next_needed,
                dynamic_buffer_limit,
                admit_window,
                bridge_pending_max,
            );
            // GAP-8 / B1 / W18: proactive bridge eviction + stale-below-floor purge.
            // W18: always invoke so stale purge runs even when tip is healthy
            // (live: bridge_min=640001 while tip≈6859xx — never purged under old gate).
            if let Some(ref bridge) = ready_bridge_for_coord {
                let wan_tip_crawl = next_needed > live_body_tip_for_coord;
                let _ = bridge.evict_far_ahead_pending_ex(
                    next_needed,
                    admit_window,
                    gap_missing_coord,
                    bridge_pending_max,
                    wan_tip_crawl,
                );
                // R-338: evicted ReadyItems are gone; let the disk-reload chain
                // re-inject them instead of one-per-poll `dispatched` reloads.
                for h in bridge.take_evicted_heights() {
                    dispatched.remove(&h);
                }
            }
            // B2 / B2b: when bridge is saturated and the gap is missing (or feeder empty),
            // clamp download ahead to admit_window so workers stop fetching +3–4k
            // heights that only hit GAP_ADMIT_DROP / pin 14 GB anon (live :51–:52).
            // B2b: do not restore to full max_ahead while gap_missing or feeder empty;
            // hold clamp until feeder has been fed for 2s+ (stops 272↔128 oscillation).
            // L1: while next_needed is still within on-disk bodies, hold ahead ≤ local
            // cap (default 256) so assigner cannot open a 4k prefetch window into the
            // bridge (live: 680001 assigned at val≈675k → next_expected_missing thrash).
            {
                let feeder_len = IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
                if feeder_len > 0 {
                    feeder_nonempty_since.get_or_insert_with(std::time::Instant::now);
                } else {
                    feeder_nonempty_since = None;
                }
                let bridge_half = (bridge_pending_max / 2).max(1);
                let bridge_len = memory::BRIDGE_PENDING_COUNT.load(Ordering::Relaxed) as usize;
                let should_clamp = bridge_pending_max > 0
                    && bridge_len >= bridge_pending_max
                    && (gap_missing_coord || feeder_len == 0);
                // L1 local / W11 WAN ahead policy (proactive — do not wait for bridge-full).
                let local_body_ahead =
                    next_needed <= live_body_tip_for_coord && live_body_tip_for_coord > 0;
                let was_local_ahead = IBD_LOCAL_BODY_AHEAD.load(Ordering::Relaxed);
                IBD_LOCAL_BODY_AHEAD.store(local_body_ahead, Ordering::Relaxed);
                // W178: leaving local inject → WAN tip with cold peer scores.
                if was_local_ahead && !local_body_ahead {
                    tip_stage::arm_post_local_ahead_grace();
                    info!(
                        "[IBD_POST_LOCAL_AHEAD] grace armed — tip-SLA uses floor for \
                                 unproven owners for wall-clock grace (CAP mute rotate unchanged)"
                    );
                }
                let ahead_cap = if local_body_ahead {
                    Some(("local", local_body_ahead_cap()))
                } else if next_needed > live_body_tip_for_coord {
                    // Past bodies tip: always bound WAN ahead. W76: feeder-empty tip
                    // starve uses the tip window even when wan_bulk_catchup (headers
                    // at network tip make bulk≈always-true mid-chain).
                    let tip_feeder_starve = feeder_len == 0;
                    let tip_gap_open = gap_missing_coord || tip_feeder_starve;
                    let bulk = wan_bulk_catchup(
                        assigner_for_coord
                            .header_tip()
                            .max(effective_end_live_for_coord.load(Ordering::Relaxed)),
                        next_needed,
                    );
                    let (kind, cap) = wan_ahead_policy(
                        bulk,
                        tip_feeder_starve,
                        tip_gap_open,
                        assigner_for_coord.ibd_ready_peer_count(),
                    );
                    Some((kind, cap))
                } else {
                    None
                };
                let clamp_to = match ahead_cap {
                    Some((_, lc)) => admit_window.max(gap_ahead_floor()).min(lc).max(64),
                    None => admit_window.max(gap_ahead_floor()),
                };
                let current = max_ahead_live.load(Ordering::Relaxed);
                // L1/W11 proactive: even without bridge-full, hold ahead cap.
                if let Some((kind, lc)) = ahead_cap {
                    if current > lc {
                        max_ahead_live.store(lc, Ordering::Relaxed);
                        let log_now = ahead_clamp_logged_at
                            .map(|t| t.elapsed() > Duration::from_secs(30))
                            .unwrap_or(true);
                        if log_now {
                            if kind == "local" {
                                info!(
                                    "[IBD_LOCAL_AHEAD] next_needed={} ≤ body_tip={} — max_ahead {} → {} (BLVM_IBD_LOCAL_AHEAD)",
                                    next_needed, live_body_tip_for_coord, current, lc
                                );
                            } else {
                                info!(
                                    "[IBD_WAN_AHEAD] next_needed={} body_tip={} feeder={} gap_missing={} — max_ahead {} → {} ({})",
                                    next_needed,
                                    live_body_tip_for_coord,
                                    feeder_len,
                                    gap_missing_coord,
                                    current,
                                    lc,
                                    kind
                                );
                            }
                            ahead_clamp_logged_at = Some(std::time::Instant::now());
                        }
                    }
                }
                let current = max_ahead_live.load(Ordering::Relaxed);
                if should_clamp && current > clamp_to {
                    max_ahead_live.store(clamp_to, Ordering::Relaxed);
                    let log_now = ahead_clamp_logged_at
                        .map(|t| t.elapsed() > Duration::from_secs(5))
                        .unwrap_or(true);
                    if log_now {
                        warn!(
                            "[IBD_AHEAD_CLAMP] bridge_pending={} feeder={} gap_missing={} — max_ahead {} → {} (admit_window/floor)",
                            bridge_len, feeder_len, gap_missing_coord, current, clamp_to
                        );
                        ahead_clamp_logged_at = Some(std::time::Instant::now());
                    }
                } else if ahead_cap.is_none() {
                    let feeder_recovered = feeder_nonempty_since
                        .is_some_and(|t| t.elapsed() >= Duration::from_secs(2));
                    let can_restore = !gap_missing_coord
                        && feeder_recovered
                        && bridge_len < bridge_half
                        && current < max_ahead_blocks
                        && current == clamp_to;
                    if can_restore {
                        max_ahead_live.store(max_ahead_blocks, Ordering::Relaxed);
                        info!(
                            "[IBD_AHEAD_CLAMP] gap cleared, feeder fed 2s+ (pending={}) — restoring max_ahead to {}",
                            bridge_len, max_ahead_blocks
                        );
                        ahead_clamp_logged_at = None;
                        feeder_nonempty_since = None;
                    }
                } else if let Some((_, lc)) = ahead_cap {
                    // W11: never restore above the WAN/local policy cap while past tip /
                    // inside local bodies — restoring to max_ahead_blocks (2k+) reopens
                    // the far-ahead soft-retry storm.
                    let feeder_recovered = feeder_nonempty_since
                        .is_some_and(|t| t.elapsed() >= Duration::from_secs(2));
                    let target = lc.min(max_ahead_blocks);
                    let can_restore = !gap_missing_coord
                        && feeder_recovered
                        && bridge_len < bridge_half
                        && current < target;
                    if can_restore {
                        max_ahead_live.store(target, Ordering::Relaxed);
                        info!(
                            "[IBD_AHEAD_CLAMP] gap cleared, feeder fed 2s+ (pending={}) — restoring max_ahead to {} (policy cap)",
                            bridge_len, target
                        );
                        ahead_clamp_logged_at = None;
                        feeder_nonempty_since = None;
                    }
                }
            }
        }
        if synthetic_wan::bulk_local_disk_stream()
            && cliff_boot_t0.elapsed() >= Duration::from_millis(500)
        {
            warn!(
                "[IBD_CLIFF_BOOTSTRAP_BLK] ms={} tip={} reorder={}",
                cliff_boot_t0.elapsed().as_millis(),
                validation_height_for_coord
                    .load(Ordering::Relaxed)
                    .saturating_add(1),
                reorder_buffer.len(),
            );
        }
        // Under Emergency memory pressure, pause bulk block_rx admission unless the
        // feeder is starved for the next validation height.  A blind full pause
        // deadlocked at h≈476304/470316: download had the gap block in flight but
        // the coordinator never recv'd block_rx, so validation could not retire memory.
        #[cfg(target_os = "linux")]
        let emergency_pipeline_quarter = if memory::ibd_pressure_is_emergency() {
            let q = (dynamic_buffer_limit / 4).max(16);
            memory::log_pressure_behavior(
                "coord_admit",
                "emergency_quarter",
                &format!("pipeline_quarter={q}"),
            );
            Some(q)
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        if let Some(pipeline_quarter) = emergency_pipeline_quarter {
            let val_h = validation_height_for_coord.load(Ordering::Relaxed);
            let next_needed = val_h.saturating_add(1);
            if !emergency_has_gap_block(&reorder_buffer, next_needed) {
                // W24: clear sticky dispatched so tip_in_pipeline=false inject can
                // reload from disk (same pattern as stall recovery). Do not clear
                // *after* a successful inject — that caused the LOCAL_GAP re-inject storm.
                dispatched.remove(&next_needed);
                let _ = try_inject_local_gap(&mut reorder_buffer, val_h, &dispatched, false);
            }
            let _ = emergency_drain_block_rx_for_gap(
                &mut block_rx,
                &mut reorder_buffer,
                next_needed,
                pipeline_quarter,
                dynamic_buffer_limit,
                &mut total_received,
                bridge_pending_max,
                admit_window,
            );
            if !emergency_has_gap_block(&reorder_buffer, next_needed) {
                // Blocking recv: try_recv alone spins while download holds the gap block
                // behind earlier heights in the same chunk (in-order stream_to block_rx).
                match timeout(Duration::from_millis(500), block_rx.recv()).await {
                    Ok(Some((h, block, witnesses))) => {
                        total_received += 1;
                        let next_needed = validation_height_for_coord.load(Ordering::Relaxed) + 1;
                        if dispatched.contains(&h) {
                            dispatched.remove(&h);
                        }
                        let _ = insert_reorder_gap_aware(
                            &mut reorder_buffer,
                            h,
                            block,
                            witnesses,
                            next_needed,
                            dynamic_buffer_limit,
                            admit_window,
                            bridge_pending_max,
                        );
                    }
                    Ok(None) => break,
                    Err(_) => {
                        if coord_emergency_log.elapsed() > Duration::from_secs(5) {
                            warn!(
                                "Coordinator: EMERGENCY — waiting for gap block {} (reorder_buf={})",
                                next_needed,
                                reorder_buffer.len(),
                            );
                            if assigner_for_coord.is_bootstrap_complete() {
                                let _ = stall_tx_for_coord.send(next_needed);
                                // W73: force tip hole on WAN; non-force is P0-B no-op.
                                assigner_for_coord.requeue_stall_gaps_force(next_needed, None);
                            }
                            coord_emergency_log = std::time::Instant::now();
                        }
                    }
                }
            }
            // Always fall through to dispatch below — never `continue` here. A prior
            // bug skipped dispatch while paused, trapping blocks already in reorder_buffer.
        }
        // Periodically prune the dispatched set to keep it bounded.
        // Heights below the current validation cursor have been retired; duplicates at
        // those heights can no longer reach the prefetch pipeline.
        {
            let val_h = validation_height_for_coord.load(Ordering::Relaxed);
            if val_h > start_height + 1024 {
                dispatched.retain(|&h| h >= val_h.saturating_sub(512));
            }
        }

        if synthetic_wan::bulk_local_disk_stream()
            && cliff_iter_t0.elapsed() >= Duration::from_millis(500)
        {
            warn!(
                "[IBD_CLIFF_PRE_GAP] iter_ms={} tip={} ch_len={} reorder={}",
                cliff_iter_t0.elapsed().as_millis(),
                validation_height_for_coord
                    .load(Ordering::Relaxed)
                    .saturating_add(1),
                block_rx.len(),
                reorder_buffer.len(),
            );
        }
        // Proactive gap unblock (PR-GAP-3 + PR-GAP-5): feeder stalls while block_rx
        // keeps delivering H+1, H+2, … so recv_many never times out and stall-path
        // inject never runs. GAP_PERSIST writes the body to disk; try inject + a light
        // block_rx drain every loop so the reorder_buffer can dispatch the gap to the
        // bridge without waiting for a 30s coordinator stall or WAN re-download.
        //
        // PR-GAP-5: if neither inject nor drain resolves next_needed, immediately
        // requeue_stall_gaps so a gap micro-chunk is dispatched to download workers
        // without waiting 30s for the validation-loop stall timer to fire. The
        // debounce in ChunkAssigner (5s) prevents micro-chunk storms.
        //
        // Dispatched-but-lost: inject treats `already_dispatched` as success without
        // reloading. Live soak crawled at ~0.033 BPS (1 LOCAL_GAP / 30s stall timer)
        // with bridge_pending=336, feeder=0, reorder=1 — gap height stuck in
        // `dispatched` after ReadyItem loss, or buffered in bridge pending with no
        // further worker_complete to trigger flush.
        //
        // Dispatched-stuck-in-reorder (2026-07-09 IBD_STALL): bridge_next=H, feeder=0,
        // reorder≈900+, buffer=0. Gap block sits in reorder marked `dispatched` but is
        // absent from bridge pending — dispatch skips it (`!dispatched` filter) and the
        // inject path never runs (`reorder.contains`). Clear dispatched so the block
        // is re-handed to the bridge (may_accept always allows next_expected).
        proactive_gap_unblock(ProactiveGapUnblock {
            admit_window,
            assigner_for_coord: &assigner_for_coord,
            block_rx: &mut block_rx,
            blockstore_for_coord: &blockstore_for_coord,
            bridge_ahead_since: &mut bridge_ahead_since,
            bridge_pending_max,
            cliff_iter_t0: &cliff_iter_t0,
            coord_engine_mode,
            coord_keys_buf: &mut coord_keys_buf,
            coord_tx_ids_buf: &mut coord_tx_ids_buf,
            dispatch_to_prefetch: &dispatch_to_prefetch,
            dispatched: &mut dispatched,
            dynamic_buffer_limit,
            effective_end_live_for_coord: &effective_end_live_for_coord,
            feeder_state_for_coord: &feeder_state_for_coord,
            ibd_store_v2_for_coord: &ibd_store_v2_for_coord,
            live_body_tip_for_coord,
            next_prefetch_height: &mut next_prefetch_height,
            ready_bridge_for_coord: &ready_bridge_for_coord,
            reorder_buffer: &mut reorder_buffer,
            tip_inflight_grace,
            tip_inflight_since: &mut tip_inflight_since,
            total_received: &mut total_received,
            try_inject_local_gap: &mut try_inject_local_gap,
            validation_height_for_coord: &validation_height_for_coord,
        });

        // W17: periodic tip-crawl diagnostics (WAN catchup). Surfaces bridge holes,
        // tip covering, feeder starvation — the ~1 BPS signature from 2026-07-10 soak.
        {
            let next_needed = validation_height_for_coord
                .load(Ordering::Relaxed)
                .saturating_add(1);
            let wan_tip_crawl = next_needed > live_body_tip_for_coord;
            tip_stage::mark_needed(next_needed);
            if wan_tip_crawl {
                // B-7: default 5s hid holes shorter than the sample.
                // Dest can set BLVM_IBD_CRAWL_LOG_MS=1000.
                let crawl_every = Duration::from_millis(latch_env!(u64, {
                    std::env::var("BLVM_IBD_CRAWL_LOG_MS")
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(5000)
                        .clamp(200, 10_000)
                }));
                let should_log = tip_crawl_logged_at
                    .map(|t| t.elapsed() >= crawl_every)
                    .unwrap_or(true);
                if should_log {
                    tip_crawl_logged_at = Some(std::time::Instant::now());
                    let feeder_len = IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
                    let max_ahead_now = max_ahead_live.load(Ordering::Relaxed);
                    let (covering, flight_ranges, busy_peers) =
                        assigner_for_coord.tip_flight_diag();
                    IBD_TIP_COVERING.store(covering, Ordering::Relaxed);
                    IBD_TIP_IN_FLIGHT_RANGES.store(flight_ranges, Ordering::Relaxed);
                    let (healthy, _raw, _) = assigner_for_coord.tip_flight_diag_healthy();
                    let tip_in_reorder = reorder_buffer.contains_key(&next_needed);
                    IBD_TIP_IN_REORDER.store(tip_in_reorder, Ordering::Relaxed);
                    let tip_in_feeder = {
                        let g = feeder_state_for_coord.0.lock();
                        g.0.get(next_needed).is_some()
                    };
                    let gap_missing = !tip_in_reorder && !tip_in_feeder;
                    let contig_runway = reorder_contig_runway(&reorder_buffer, next_needed);
                    let ahead_buffered = reorder_ahead_buffered(&reorder_buffer, next_needed);
                    let first_ahead = reorder_first_ahead(&reorder_buffer, next_needed);
                    assigner_for_coord.set_window_hole_until(first_ahead.unwrap_or(0));
                    let (bridge_next, bridge_len, bridge_min, bridge_max, holes) =
                        ready_bridge_for_coord
                            .as_ref()
                            .map(|b| b.pending_diag())
                            .unwrap_or((None, 0, None, None, 0));
                    let runway_mode = tip_runway_mode(
                        tip_in_reorder,
                        contig_runway,
                        ahead_buffered,
                        holes as u64,
                        tip_in_feeder,
                    );
                    IBD_EMPTY_TIP.store(runway_mode == "EMPTY_TIP", Ordering::Relaxed);
                    let sticky = assigner_for_coord
                        .preferred_tip_owner()
                        .unwrap_or_else(|| "-".to_string());
                    let sticky_bps = if sticky != "-" {
                        assigner_for_coord.wan_tip_stream_bps(&sticky)
                    } else {
                        0.0
                    };
                    let grown = if sticky != "-" {
                        assigner_for_coord.tip_hole_depth_for(&sticky)
                    } else {
                        0
                    };
                    let gd_ewma = tip_stage::getdata_body_ewma_ms();
                    let ahead_busy = busy_peers.saturating_sub(covering);
                    let (flight_tip, flight_ahead, flight_other) =
                        assigner_for_coord.tip_flight_split();
                    let (body_ia_avg, body_ia_n) = tip_stage::take_body_ia_window();
                    let pipe_w = tip_stage::take_pipe_w();
                    let (duty_ms, duty_ia, duty_ia_n, duty8, duty32) =
                        tip_stage::last_duty_window();
                    let est_bytes = download::download_est_block_bytes();
                    let export_running = IBD_CHECKPOINT_EXPORT_ACTIVE.load(Ordering::Relaxed);
                    let (win_recv_mb, win_mbps, sticky_recv_mbps, top_recv, top_recv_mbps) = {
                        let now = std::time::Instant::now();
                        let dt = now
                            .saturating_duration_since(crawl_recv_at)
                            .as_secs_f64()
                            .max(0.001);
                        let b = download::download_bytes_total();
                        crawl_recv_at = now;
                        let delta = b.saturating_sub(crawl_recv_bytes);
                        crawl_recv_bytes = b;
                        let mb = delta as f64 / (1024.0 * 1024.0);
                        let (sticky_recv_mbps, top_recv, top_recv_mbps) =
                            download::download_peer_recv_window(&sticky, &mut crawl_peer_bytes, dt);
                        (mb, mb * 8.0 / dt, sticky_recv_mbps, top_recv, top_recv_mbps)
                    };
                    let tip_gd_ms = tip_stage::last_getdata_body_ms();
                    warn!(
                        "[IBD_TIP_CRAWL] next_needed={} body_tip={} applied={} gap_missing={} reorder={} feeder={} max_ahead={} | bridge_next={:?} bridge_pending={} bridge_min={:?} bridge_max={:?} holes={} | tip_healthy={} tip_covering={} in_flight_ranges={} busy_peers={} ahead_busy={} flight_tip={} flight_ahead={} flight_other={} pipe_w={:.1} ready={} | sticky={} sticky_bps={:.1} grown={} gd_ewma_ms={:?} gd_ewma_n={} c1u={} tip_gd_ms={} | body_ia_avg_ms={} body_ia_n={} est_block_bytes={} win_recv_mb={:.2} win_mbps={:.1} sticky_recv_mbps={:.1} top_recv={} top_recv_mbps={:.1} export_running={} | duty_ms={} duty8={:.1}% duty32={:.1}% duty_ia_ms={} duty_ia_n={}",
                        next_needed,
                        live_body_tip_for_coord,
                        validation_height_for_coord.load(Ordering::Relaxed),
                        gap_missing,
                        reorder_buffer.len(),
                        feeder_len,
                        max_ahead_now,
                        bridge_next,
                        bridge_len,
                        bridge_min,
                        bridge_max,
                        holes,
                        healthy,
                        covering,
                        flight_ranges,
                        busy_peers,
                        ahead_busy,
                        flight_tip,
                        flight_ahead,
                        flight_other,
                        pipe_w,
                        assigner_for_coord.ibd_ready_peer_count(),
                        sticky,
                        sticky_bps,
                        grown,
                        gd_ewma.map(|(ms, _)| ms),
                        gd_ewma.map(|(_, n)| n).unwrap_or(0),
                        download::tip_hole_gd_slow(),
                        tip_gd_ms,
                        body_ia_avg,
                        body_ia_n,
                        est_bytes,
                        win_recv_mb,
                        win_mbps,
                        sticky_recv_mbps,
                        top_recv,
                        top_recv_mbps,
                        export_running as u8,
                        duty_ms,
                        duty8,
                        duty32,
                        duty_ia,
                        duty_ia_n
                    );
                    // R-336: window assigner fill/seat gauge (only while it owns get_work).
                    if let Some(g) = assigner_for_coord.window_gauge() {
                        warn!("{}", g);
                    }
                    // C1f: first-class runway binder — holes=0 is NOT "filled runway".
                    warn!(
                        "[IBD_TIP_RUNWAY] mode={} tip={} tip_in_reorder={} contig_runway={} ahead_buffered={} first_ahead={:?} feeder={} reorder={} holes={} covering={} ready={} busy={}",
                        runway_mode,
                        next_needed,
                        tip_in_reorder,
                        contig_runway,
                        ahead_buffered,
                        first_ahead,
                        feeder_len,
                        reorder_buffer.len(),
                        holes,
                        covering,
                        assigner_for_coord.ibd_ready_peer_count(),
                        busy_peers
                    );
                    assigner_for_coord.note_tip_progress(next_needed);
                }
                let should_a6m = a6m_check_at
                    .map(|t| t.elapsed() >= Duration::from_secs(30))
                    .unwrap_or(true);
                // P2: tip trials on every crawl tick (finish ≤TRIAL_SECS); A6m stays ~30s.
                assigner_for_coord.maybe_run_tip_trial(next_needed);
                if should_a6m {
                    a6m_check_at = Some(std::time::Instant::now());
                    assigner_for_coord
                        .maybe_rotate_slow_sticky_a6m(next_needed, peer_scorer_for_coord.as_ref());
                }
            }
        }

        // Out-of-order dispatch: drain ALL blocks in the reorder_buffer to prefetch
        // workers, regardless of whether they are contiguous from next_prefetch_height.
        // The OrderedReadyBridge enforces ascending delivery to the feeder, so it is safe
        // to dispatch future heights first — they will be buffered in the bridge's pending
        // map until any gaps are filled (i.e. when a retried chunk finally arrives).
        // This eliminates HOL (head-of-line) blocking entirely: one slow/failed peer can
        // no longer stall all heights above its chunk.
        //
        // Backpressure: if buffer is full AND we've dispatched everything in it already,
        // receive more blocks before looping. If buffer has undispatched entries, drain them.
        let undispatched_in_buffer = reorder_buffer.keys().any(|h| !dispatched.contains(h));
        if reorder_buffer.len() >= dynamic_buffer_limit && !undispatched_in_buffer {
            // Buffer full, nothing new to dispatch. Try to receive more blocks.
            let mut gap_drained = 0usize;
            while let Ok((h, block, witnesses)) = block_rx.try_recv() {
                total_received += 1;
                let next_needed = validation_height_for_coord.load(Ordering::Relaxed) + 1;
                if dispatched.contains(&h) {
                    // Gap recovery: re-queued chunk re-streams heights validation still needs.
                    dispatched.remove(&h);
                }
                if insert_reorder_gap_aware(
                    &mut reorder_buffer,
                    h,
                    block,
                    witnesses,
                    next_needed,
                    dynamic_buffer_limit,
                    admit_window,
                    bridge_pending_max,
                ) {
                    gap_drained += 1;
                }
                if gap_drained >= 200 {
                    break;
                }
            }
            if !reorder_buffer.keys().any(|h| !dispatched.contains(h)) {
                // Still nothing undispatched. Check for stall.
                let min_undispatched_needed = {
                    let val_h = validation_height_for_coord.load(Ordering::Relaxed);
                    // The bridge's pending holds dispatched-but-not-yet-released blocks.
                    // If validation is stalled, the gap height is around val_h+1.
                    val_h + 1
                };
                let now = std::time::Instant::now();
                let stall_start = *coord_buffer_full_since.get_or_insert(now);
                let stuck_secs = now.duration_since(stall_start).as_secs();
                if stuck_secs >= coord_stall_effective_secs {
                    if assigner_for_coord.is_bootstrap_complete() {
                        warn!(
                            "Coordinator stall: buffer full ({}) but height {} not in buffer for {}s — requeuing",
                            reorder_buffer.len(),
                            min_undispatched_needed,
                            stuck_secs
                        );
                        let _ = stall_tx_for_coord.send(min_undispatched_needed);
                        if assigner_for_coord.wan_stall_micro_allowed(min_undispatched_needed) {
                            assigner_for_coord.requeue_stall_gaps(min_undispatched_needed, None);
                        }
                    }
                    coord_buffer_full_since = None;
                }
                tokio::time::sleep(IBD_YIELD_SLEEP).await;
                continue;
            }
            coord_buffer_full_since = None;
        }
        // Dispatch all undispatched heights from the reorder_buffer.
        // Reuse scratch buffer to avoid a per-loop Vec allocation.
        // Engine mode: stop dumping ahead heights into OrderedReadyBridge once
        // pending_max is hit — leave them in reorder_buffer (cheaper to hold there
        // than as ReadyItems; gap height is always allowed so the bridge can drain).
        {
            let reorder_dispatch_t0 = std::time::Instant::now();
            let mut reorder_emitted = 0usize;
            let val_h_dispatch = validation_height_for_coord.load(Ordering::Relaxed);
            let next_needed_dispatch = val_h_dispatch.saturating_add(1);
            // Resume20: 2820664–2820780 completed at 23:01:49 and were marked
            // delivered. By 23:05 the bodies were gone and first_ahead was ~50
            // above the tip, but stall unmark cleared one height because
            // window_hole_until was the previous sample. The next get_work
            // started at 2820772. Clear that absent span before a bridge-cap
            // continue skips the gap-poll reissue.
            {
                let first_ahead = reorder_first_ahead(&reorder_buffer, next_needed_dispatch);
                assigner_for_coord.set_window_hole_until(first_ahead.unwrap_or(0));
                let in_reorder = reorder_buffer.contains_key(&next_needed_dispatch);
                let in_feeder = {
                    let g = feeder_state_for_coord.0.lock();
                    g.0.get(next_needed_dispatch).is_some()
                };
                let in_bridge = ready_bridge_for_coord
                    .as_ref()
                    .is_some_and(|b| b.pending_contains(next_needed_dispatch));
                // Resume21: the hole was re-marked delivered and every peer
                // was on a tile ~400 above it. covers==0, so the old yield
                // (which requires a holder) never released them.
                assigner_for_coord
                    .set_window_tip_uncovered(!in_reorder && !in_feeder && !in_bridge);
                if !in_reorder {
                    let until = first_ahead.unwrap_or(next_needed_dispatch.saturating_add(1));
                    assigner_for_coord.window_reissue_absent(next_needed_dispatch, until);
                }
            }
            // Repair bridge hole before dispatch so tip in pending can flush.
            if let Some(ref bridge) = ready_bridge_for_coord {
                if bridge.repair_missing_cursor_hole(next_needed_dispatch) {
                    let _ = bridge.try_flush();
                } else if bridge
                    .next_expected()
                    .is_some_and(|n| n < next_needed_dispatch)
                {
                    let _ = bridge.fast_forward_cursor_to(next_needed_dispatch);
                }
            }
            let cursor_now = ready_bridge_for_coord
                .as_ref()
                .and_then(|b| b.next_expected());
            if let Some(h) = cursor_now {
                let cursor_in_feeder = {
                    let g = feeder_state_for_coord.0.lock();
                    g.0.get(h).is_some()
                };
                let cursor_in_pending = ready_bridge_for_coord
                    .as_ref()
                    .is_some_and(|b| b.pending_contains(h));
                release_cursor_stuck_in_reorder(
                    &mut dispatched,
                    reorder_buffer.contains_key(&h),
                    cursor_now,
                    cursor_in_feeder,
                    cursor_in_pending,
                );
            }
            dispatch_heights_buf.clear();
            dispatch_heights_buf.extend(
                reorder_buffer
                    .keys()
                    .filter(|&&h| !dispatched.contains(&h))
                    .copied(),
            );
            let mut bridge_capped = false;
            let gap_missing_dispatch = !reorder_buffer.contains_key(&next_needed_dispatch);
            let next_expected_missing_dispatch = ready_bridge_for_coord
                .as_ref()
                .is_some_and(|b| b.next_expected().is_some_and(|n| !b.pending_contains(n)));
            let wan_tip_crawl_dispatch = next_needed_dispatch > live_body_tip_for_coord;
            let bulk_catchup_dispatch = wan_bulk_catchup(
                assigner_for_coord
                    .header_tip()
                    .max(effective_end_live_for_coord.load(Ordering::Relaxed)),
                next_needed_dispatch,
            );
            // W34d/W34h: prioritize next 16 heights when feeder starved on WAN crawl.
            let feeder_len_dispatch = IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
            let feeder_starved_dispatch =
                wan_feeder_prefetch_starved(wan_tip_crawl_dispatch, feeder_len_dispatch);
            if wan_tip_crawl_dispatch
                && feeder_starved_dispatch
                && (!gap_missing_dispatch
                    || reorder_has_feeder_prefetch_band(
                        &reorder_buffer,
                        next_needed_dispatch,
                        W34_FEEDER_PREFETCH_TARGET,
                    ))
            {
                let band_end =
                    next_needed_dispatch.saturating_add(W34_FEEDER_PREFETCH_TARGET as u64);
                dispatch_heights_buf.sort_by_key(|h| {
                    if *h >= next_needed_dispatch && *h < band_end {
                        *h - next_needed_dispatch
                    } else {
                        u64::MAX / 2 + *h
                    }
                });
            }
            for h in dispatch_heights_buf.drain(..) {
                // Sequential: never try_emit-dump the whole reorder while tip is
                // missing (10s/loop cliff). Absolute tip-only also tip-crawled
                // (~9 h/min) — allow a small contiguous band when tip is present.
                if sequential {
                    let tip = next_needed_dispatch;
                    let tip_missing = gap_missing_dispatch;
                    const SEQ_REORDER_BAND: u64 = 32;
                    if tip_missing && h != tip {
                        feeder_miss::SEQ_SKIP_TIP_MISSING.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if !tip_missing && h > tip.saturating_add(SEQ_REORDER_BAND) {
                        feeder_miss::SEQ_SKIP_BAND.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }
                if !bridge_cursor_dispatch_allowed(h, next_needed_dispatch, cursor_now)
                    && defer_bridge_ahead_dispatch(
                        h,
                        next_needed_dispatch,
                        gap_missing_dispatch,
                        next_expected_missing_dispatch,
                        admit_window,
                        wan_tip_crawl_dispatch,
                        feeder_starved_dispatch,
                        bulk_catchup_dispatch,
                    )
                {
                    continue;
                }
                if let Some(ref bridge) = ready_bridge_for_coord {
                    if !bridge.may_accept_height(h, bridge_pending_max) {
                        // Do NOT break: ascending order hits refused ahead heights
                        // before next_expected when the gap sits later in reorder.
                        // may_accept always allows next_expected — skip and keep looking
                        // (live: inject 9111 then stall forever with bridge_pending=512).
                        bridge_capped = true;
                        continue;
                    }
                }
                let Some((block, witnesses)) = reorder_buffer.remove(&h) else {
                    continue;
                };
                dispatched.insert(h);
                if h >= next_prefetch_height {
                    next_prefetch_height = h + 1;
                }
                if h == bootstrap_end {
                    assigner_for_coord.mark_bootstrap_complete();
                    info!(
                        "IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled",
                        h
                    );
                }
                // In engine mode skip key extraction: keys are unused by the engine
                // validation path. Only tx_ids are needed for SpendSession::append.
                prepare_coord_dispatch_bufs(
                    coord_engine_mode,
                    &block,
                    &mut coord_tx_ids_buf,
                    &mut coord_keys_buf,
                );
                let store = &ibd_store_v2_for_coord;
                let keys_owned = std::mem::take(&mut coord_keys_buf);
                let tx_ids_owned = std::mem::take(&mut coord_tx_ids_buf);
                let item = (
                    Arc::clone(store),
                    keys_owned,
                    tx_ids_owned,
                    h,
                    block,
                    witnesses,
                    coord_engine_mode,
                );
                if let Some((h, block, witnesses)) = dispatch_to_prefetch(item) {
                    dispatched.remove(&h);
                    reorder_buffer.insert(h, (block, witnesses));
                    break;
                }
                reorder_emitted += 1;
            }
            feeder_miss::note_dispatch_emit(reorder_emitted as u64, gap_missing_dispatch);
            {
                let want = feeder_miss::ORCH_WANT.load(Ordering::Relaxed);
                let has = want > 0 && reorder_buffer.contains_key(&want);
                feeder_miss::publish_reorder(
                    has,
                    reorder_buffer.len(),
                    reorder_contig_runway(&reorder_buffer, next_needed_dispatch),
                );
                feeder_miss::note_coord_loop();
            }
            if synthetic_wan::bulk_local_disk_stream()
                && reorder_dispatch_t0.elapsed() >= Duration::from_millis(500)
            {
                warn!(
                    "[IBD_CLIFF_REORDER_DISPATCH] tip={} emitted={} ms={} reorder={} sequential={}",
                    next_needed_dispatch,
                    reorder_emitted,
                    reorder_dispatch_t0.elapsed().as_millis(),
                    reorder_buffer.len(),
                    sequential,
                );
            }
            if bridge_capped {
                // Bridge pending full of ahead blocks — wait for gap fill / feeder drain.
                // Still try to inject the validation gap: it is always may_accept'd and
                // is the only height that can drain the pending map.
                let val_h = validation_height_for_coord.load(Ordering::Relaxed);
                let next_needed = val_h.saturating_add(1);
                let gap_missing = !reorder_buffer.contains_key(&next_needed);
                if let Some(ref bridge) = ready_bridge_for_coord {
                    let wan_tip_crawl = next_needed > live_body_tip_for_coord;
                    let _ = bridge.evict_far_ahead_pending_ex(
                        next_needed,
                        admit_window,
                        gap_missing,
                        bridge_pending_max,
                        wan_tip_crawl,
                    );
                    for h in bridge.take_evicted_heights() {
                        dispatched.remove(&h);
                    }
                }
                if !reorder_buffer.contains_key(&next_needed) {
                    dispatched.remove(&next_needed);
                    let _ = try_inject_local_gap(&mut reorder_buffer, val_h, &dispatched, false);
                }
                // If the gap is now in reorder (inject or prior), dispatch it this
                // iteration — do not sleep past a ready next_expected.
                if !dispatched.contains(&next_needed) {
                    if let Some(ref bridge) = ready_bridge_for_coord {
                        if bridge.may_accept_height(next_needed, bridge_pending_max) {
                            if let Some((block, witnesses)) = reorder_buffer.remove(&next_needed) {
                                dispatched.insert(next_needed);
                                if next_needed >= next_prefetch_height {
                                    next_prefetch_height = next_needed + 1;
                                }
                                prepare_coord_dispatch_bufs(
                                    coord_engine_mode,
                                    &block,
                                    &mut coord_tx_ids_buf,
                                    &mut coord_keys_buf,
                                );
                                let store = &ibd_store_v2_for_coord;
                                let keys_owned = std::mem::take(&mut coord_keys_buf);
                                let tx_ids_owned = std::mem::take(&mut coord_tx_ids_buf);
                                if let Some((h, block, witnesses)) = dispatch_to_prefetch((
                                    Arc::clone(store),
                                    keys_owned,
                                    tx_ids_owned,
                                    next_needed,
                                    block,
                                    witnesses,
                                    coord_engine_mode,
                                )) {
                                    dispatched.remove(&h);
                                    reorder_buffer.insert(h, (block, witnesses));
                                } else {
                                    coord_buffer_full_since = None;
                                }
                                continue;
                            }
                        }
                    }
                }
                tokio::time::sleep(IBD_YIELD_SLEEP).await;
                continue;
            }
        }
        if reorder_buffer.len() >= dynamic_buffer_limit {
            tokio::time::sleep(IBD_YIELD_SLEEP).await;
            continue;
        }
        #[cfg(target_os = "linux")]
        if let Some(pipeline_quarter) = emergency_pipeline_quarter {
            if !emergency_may_bulk_recv(&reorder_buffer, pipeline_quarter) {
                // Buffer quarter full — skip bulk recv_many but keep dispatching above.
                tokio::time::sleep(IBD_YIELD_SLEEP).await;
                continue;
            }
        }
        // When the validation gap is missing from reorder, do not park on the full
        // coordinator stall timeout (15–30s). That made LOCAL_GAP inject run once per
        // stall tick → live crawl ~0.033 BPS even with bodies already on disk.
        // Short poll lets the proactive inject / try_flush / micro-requeue path above
        // run every few hundred ms until next_needed is supplied.
        //
        // I3: After a successful LOCAL_GAP inject+dispatch, next_needed has left
        // reorder (in bridge/feeder) so gap_poll stays true — the old 250ms wait
        // capped crawl at ~4 BPS (live: inject p50=253ms, inject≈validate ratio≈1).
        // When the gap height is already in-pipeline (dispatched / bridge pending /
        // feeder), skip the park and tight-loop so the next disk height injects ASAP.
        let gap_poll = {
            let val_h = validation_height_for_coord.load(Ordering::Relaxed);
            let next_needed = val_h.saturating_add(1);
            // Tip-missing poll must run even before bootstrap_complete. Gating on
            // bootstrap left gap_poll=false during the first chunk: tip-first drain
            // skipped, recv_wait inflated to 30s, and with TIP_RESERVE yield-spin
            // the coordinator could starve until ~10s (synth tip-crawl).
            let missing = !reorder_buffer.contains_key(&next_needed);
            // Assigner hole-fill reads this every poll — do not only store in the
            // 5s TIP_CRAWL log (250ms reassign storms need a fresh bit).
            IBD_TIP_IN_REORDER.store(!missing, Ordering::Relaxed);
            // C1i / N19a: mirror contig runway for assigner past-tip freeze.
            // Credit tip only when in reorder or tip_taken — never tip_in_feeder
            // alone (C1q cheese). When tip_taken, count 1 + contig from tip+1.
            let reorder_c = reorder_contig_runway(&reorder_buffer, next_needed);
            let tip_in_feeder_for_hole = {
                let g = feeder_state_for_coord.0.lock();
                g.0.get(next_needed).is_some()
            };
            let pipeline_c = if reorder_c > 0 {
                reorder_c
            } else if tip_stage::tip_taken_by_validation(next_needed) || tip_in_feeder_for_hole {
                1u64.saturating_add(reorder_contig_runway(
                    &reorder_buffer,
                    next_needed.saturating_add(1),
                ))
            } else {
                0
            };
            IBD_TIP_CONTIG_RUNWAY.store(pipeline_c, Ordering::Relaxed);
            // L2b lands have in the feeder. `pipeline_c` stays reorder-only
            // (C1q / C1i freeze). Hero hole walks feeder ∪ reorder.
            let have_c = {
                let g = feeder_state_for_coord.0.lock();
                have_contig_runway(&reorder_buffer, |h| g.0.get(h).is_some(), next_needed)
            };
            let first_hole = next_needed.saturating_add(have_c.max(pipeline_c));
            IBD_FIRST_HOLE.store(first_hole, Ordering::Relaxed);
            IBD_FIRST_HOLE_AT.store(next_needed, Ordering::Relaxed);
            IBD_REORDER_AHEAD.store(
                reorder_ahead_buffered(&reorder_buffer, next_needed),
                Ordering::Relaxed,
            );
            // Assigner distress = body truly unavailable. Tip ∉ reorder alone is
            // true for healthy tip_taken / feeder occupancy (AV=0 scripts) and was
            // driving H6b DEDUP REARM every grace ms @~3065xx (CLIFF_GATE tip_taken).
            let tip_in_feeder_for_assigner = {
                let g = feeder_state_for_coord.0.lock();
                g.0.get(next_needed).is_some()
            };
            let tip_body_available =
                tip_in_feeder_for_assigner || tip_stage::tip_taken_by_validation(next_needed);
            // Assigner tip-gap flag still waits for bootstrap (avoid W28c thrash
            // before the first chunk is admitted).
            assigner_for_coord.set_tip_gap_missing(
                missing && !tip_body_available && assigner_for_coord.is_bootstrap_complete(),
            );
            let holes = ready_bridge_for_coord
                .as_ref()
                .map(|b| b.pending_diag().4 as u64)
                .unwrap_or(0);
            assigner_for_coord.set_tip_bridge_holes(holes);
            missing
        };
        let next_needed_poll = validation_height_for_coord.load(Ordering::Relaxed) + 1;
        // Tip is "in pipeline" only when the body is actually present for handoff:
        // bridge cursor AT tip **and** tip in pending, or **tip key** in feeder.
        // W75 (live 344348): `bridge_next==tip` with `pending=0` is a *hole*, not
        // in-pipeline — the old check tight-looped `yield`/`continue` and skipped
        // Case C / FORCE_REQUEUE forever (same silence signature as W67).
        // W78 (live 381335): `feeder_len>0` with tip absent spun the same way while
        // `bridge_next` sat 61 ahead of validation tip.
        // `pending_contains(tip)` alone is still NOT enough — tip can sit in pending
        // behind a missing `bridge_next` hole (live: bridge_next=678425, tip=678432).
        // Do **not** treat `dispatched.contains(tip)` as in-pipeline: a wedged
        // download worker leaves dispatched sticky while the channel fills.
        let tip_in_feeder_poll = {
            let g = feeder_state_for_coord.0.lock();
            g.0.get(next_needed_poll).is_some()
        };
        // Resume17: a delivered mark with no body blocks get_work until the
        // 5s unmark, and that unmark only ran when an ahead chunk's 10s
        // deadline called stall requeue. Re-issue as soon as the gap poll
        // sees the tip absent from reorder, feeder, and bridge.
        if gap_poll
            && !tip_in_feeder_poll
            && !ready_bridge_for_coord
                .as_ref()
                .is_some_and(|b| b.pending_contains(next_needed_poll))
            && !tip_stage::tip_taken_by_validation(next_needed_poll)
        {
            let until = reorder_first_ahead(&reorder_buffer, next_needed_poll)
                .unwrap_or(next_needed_poll.saturating_add(1));
            assigner_for_coord.window_reissue_absent(next_needed_poll, until);
        }
        let gap_in_pipeline = gap_poll
            && tip_gap_body_in_pipeline(
                ready_bridge_for_coord.as_ref().is_some_and(|b| {
                    b.next_expected() == Some(next_needed_poll)
                        && b.pending_contains(next_needed_poll)
                }),
                tip_in_feeder_poll,
            );
        // Hole repair must run even on the in-pipeline short-circuit (that path
        // `continue`s before the main gap-handler block).
        if let Some(ref bridge) = ready_bridge_for_coord {
            if bridge.next_expected().is_some_and(|n| n < next_needed_poll) {
                let _ = bridge.repair_missing_cursor_hole(next_needed_poll);
                if bridge.next_expected().is_some_and(|n| n < next_needed_poll) {
                    let _ = bridge.fast_forward_cursor_to(next_needed_poll);
                }
                let _ = bridge.try_flush();
            } else if bridge.pending_contains(next_needed_poll) {
                let _ = bridge.try_flush();
            }
        }
        // W26/W40: never long-park while tip is in reorder/bridge with empty feeder
        // (live: ~30% of WAN wall time with tip present and feeder=0).
        // Soft-resume LOCAL_AHEAD has the same handoff stall (live: REEMIT + feeder=0).
        let tip_handoff_urgent = {
            let tip_in_reorder = reorder_buffer.contains_key(&next_needed_poll);
            let bridge_waiting = ready_bridge_for_coord
                .as_ref()
                .and_then(|b| b.next_expected())
                == Some(next_needed_poll);
            let feeder_empty = IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed) == 0;
            (tip_in_reorder || bridge_waiting) && feeder_empty
        };
        // I3: gap already in-pipeline — drain block_rx non-blocking and tight-loop
        // back to Case D inject (lookahead chain). Do NOT park on timeout(0) which
        // busy-spins the coordinator.
        //
        // If tip is stranded in bridge pending behind a hole, do NOT take this path
        // (gap_in_pipeline is false above) — fall through to repair/Case C.
        if gap_in_pipeline {
            while let Ok((h, block, witnesses)) = block_rx.try_recv() {
                total_received += 1;
                let next_needed = validation_height_for_coord.load(Ordering::Relaxed) + 1;
                if dispatched.contains(&h) {
                    dispatched.remove(&h);
                }
                let _ = insert_reorder_gap_aware(
                    &mut reorder_buffer,
                    h,
                    block,
                    witnesses,
                    next_needed,
                    dynamic_buffer_limit,
                    admit_window,
                    bridge_pending_max,
                );
            }
            // Tip in reorder must fall through to W19 handoff / dispatch — do not
            // spin here while dispatched stays sticky (live: GAP_STREAM → reorder,
            // then continue forever with feeder=0).
            if !reorder_buffer.contains_key(&next_needed_poll) {
                tokio::task::yield_now().await;
                continue;
            }
        }
        // A2/A3: WAN catch-up (past bodies-on-disk tip) uses a tighter gap poll so
        // inject/requeue runs ~20 Hz instead of ~4 Hz (default 250ms).
        let wan_catchup = {
            let next_needed = validation_height_for_coord
                .load(Ordering::Relaxed)
                .saturating_add(1);
            next_needed > confirmed_body_height_for_coord && confirmed_body_height_for_coord > 0
        };
        let gap_poll_ms: u64 = std::env::var("BLVM_IBD_GAP_POLL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(if wan_catchup { 50 } else { 250 })
            .clamp(1, 5_000);
        // Synth bulk: never park on the 90s single-peer stall recv. When tip sits in
        // reorder with a non-empty feeder, gap_poll=false and tip_handoff_urgent=false
        // → old path waited coord_stall (90s) while Case C could not run; live cliff
        // showed ~10s INJECT cadence (channel refill / TIP_RESERVE) instead.
        let recv_wait = if gap_poll || tip_handoff_urgent || synthetic_wan::bulk_local_disk_stream()
        {
            Duration::from_millis(gap_poll_ms)
        } else {
            Duration::from_secs(coord_stall_effective_secs)
        };
        feeder_miss::GAP_POLL_TRUE.store(gap_poll, Ordering::Relaxed);
        feeder_miss::note_recv_wait_ms(recv_wait.as_millis() as u64);
        if synthetic_wan::bulk_local_disk_stream() {
            static CLIFF_RECV_AT: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let prev = CLIFF_RECV_AT.load(Ordering::Relaxed);
            if now_ms.saturating_sub(prev) >= 1000 {
                CLIFF_RECV_AT.store(now_ms, Ordering::Relaxed);
                warn!(
                    "[IBD_CLIFF_RECV] tip={} gap_poll={} handoff_urgent={} recv_wait_ms={} ch_len={} reorder_has_tip={} iter_ms={}",
                    next_needed_poll,
                    gap_poll,
                    tip_handoff_urgent,
                    recv_wait.as_millis(),
                    block_rx.len(),
                    reorder_buffer.contains_key(&next_needed_poll),
                    cliff_iter_t0.elapsed().as_millis(),
                );
            }
        }
        // W25c: admit validation tip from block_rx before bulk recv_many parks.
        if gap_poll {
            let next_needed = validation_height_for_coord.load(Ordering::Relaxed) + 1;
            let drained = drain_block_rx_tip_first(
                &mut block_rx,
                &mut reorder_buffer,
                &mut dispatched,
                next_needed,
                dynamic_buffer_limit,
                admit_window,
                bridge_pending_max,
                &mut total_received,
                BATCH_DRAIN_LIMIT,
            );
            // Synth bulk: do not skip recv_many after tip-first — ahead blocks
            // left in block_rx filled the channel (BACKLOG≈176) while tip-reserve
            // waited and LOCAL_GAP only recovered every ~10s.
            if drained > 0
                && reorder_buffer.contains_key(&next_needed)
                && !synthetic_wan::bulk_local_disk_stream()
            {
                continue;
            }
        }
        let recv_fut = block_rx.recv_many(&mut batch, BATCH_DRAIN_LIMIT);
        let recv_started = std::time::Instant::now();
        let n = match timeout(recv_wait, recv_fut).await {
            Ok(n) => {
                if synthetic_wan::bulk_local_disk_stream()
                    && recv_started.elapsed() >= Duration::from_millis(500)
                {
                    warn!(
                        "[IBD_CLIFF_RECV_DONE] ok n={} waited_ms={} recv_wait_ms={}",
                        n,
                        recv_started.elapsed().as_millis(),
                        recv_wait.as_millis(),
                    );
                }
                n
            }
            Err(_) if gap_poll || tip_handoff_urgent => {
                if synthetic_wan::bulk_local_disk_stream()
                    && recv_started.elapsed() >= Duration::from_millis(500)
                {
                    warn!(
                        "[IBD_CLIFF_RECV_DONE] gap_timeout waited_ms={} recv_wait_ms={}",
                        recv_started.elapsed().as_millis(),
                        recv_wait.as_millis(),
                    );
                }
                // Short gap/handoff poll timed out — loop back to proactive inject/flush.
                // Do NOT run full stall recovery (that would spam requeue every 250ms).
                while let Ok((h, block, witnesses)) = block_rx.try_recv() {
                    total_received += 1;
                    let next_needed = validation_height_for_coord.load(Ordering::Relaxed) + 1;
                    if dispatched.contains(&h) {
                        dispatched.remove(&h);
                    }
                    let _ = insert_reorder_gap_aware(
                        &mut reorder_buffer,
                        h,
                        block,
                        witnesses,
                        next_needed,
                        dynamic_buffer_limit,
                        admit_window,
                        bridge_pending_max,
                    );
                }
                continue;
            }
            Err(_) => {
                let next_needed = validation_height_for_coord.load(Ordering::Relaxed) + 1;
                let effective_end_now = effective_end_live_for_coord.load(Ordering::Relaxed);
                if next_needed > effective_end_now {
                    continue;
                }
                // Always drain block_rx on stall timeout — even pre-bootstrap.
                // Skipping drain while !bootstrap_complete dropped tip that was
                // already STREAM'd into a full channel (synth ~10s ignition stall).
                let mut stall_drained = 0usize;
                while let Ok((h, block, witnesses)) = block_rx.try_recv() {
                    total_received += 1;
                    if dispatched.contains(&h) {
                        dispatched.remove(&h);
                    }
                    if insert_reorder_gap_aware(
                        &mut reorder_buffer,
                        h,
                        block,
                        witnesses,
                        next_needed,
                        dynamic_buffer_limit,
                        admit_window,
                        bridge_pending_max,
                    ) {
                        stall_drained += 1;
                    }
                    if stall_drained >= BATCH_DRAIN_LIMIT {
                        break;
                    }
                }
                if stall_drained > 0 {
                    info!(
                        "[IBD_COORD_DRAIN] drained {} block(s) from block_rx during stall wait (next_needed={})",
                        stall_drained, next_needed
                    );
                    coord_buffer_full_since = None;
                    let val_h = next_needed.saturating_sub(1);
                    if try_inject_local_gap(&mut reorder_buffer, val_h, &dispatched, false) {
                        continue;
                    }
                    if reorder_buffer.contains_key(&next_needed) {
                        continue;
                    }
                }
                // next_prefetch_height is the coordinator's delivery cursor. Requeue the
                // chunk it is waiting on to unblock the download pipeline.
                // Additionally: if validation is stuck at a height below next_prefetch_height
                // and that block is NOT already in the reorder_buffer, its download chunk
                // partially failed. Requeue that chunk explicitly so the missing block is
                // re-downloaded. Without this fix the coordinator keeps requeuing chunks
                // ahead of the gap, leaving the missing block unreachable indefinitely.
                // With out-of-order dispatch, stall is always at next_needed (the height
                // validation is waiting for, i.e. bridge's gap). Requeue that chunk.
                let stall_height = if !dispatched.contains(&next_needed) {
                    next_needed // not yet dispatched → gap in downloads
                } else {
                    next_prefetch_height // dispatched but not delivered → bridge gap
                };
                let val_h = next_needed.saturating_sub(1);
                // Stall recovery: clear dispatched for the gap so local inject can
                // reload from disk even if a prior ReadyItem was lost (e.g. ready
                // channel disconnect / bridge send failure). Without this, inject
                // skips as already_dispatched and validation wedges forever
                // (observed: inject 662263 then IBD_STALL for minutes while download
                // repeatedly loads the same local body).
                dispatched.remove(&next_needed);
                if try_inject_local_gap(&mut reorder_buffer, val_h, &dispatched, false) {
                    coord_buffer_full_since = None;
                    continue;
                }
                coord_stall_count += 1;
                let bridge_pending = ready_bridge_for_coord
                    .as_ref()
                    .map(|b| b.pending_len())
                    .unwrap_or(0);
                warn!(
                    "Coordinator stall: no blocks for {}s, waiting for height {} (total_received={}, next_prefetch={}, stall_requeue={}, stall_count={}, reorder={}, dispatched_has_gap={}, bridge_pending={})",
                    coord_stall_log_secs,
                    next_needed,
                    total_received,
                    next_prefetch_height,
                    stall_height,
                    coord_stall_count,
                    reorder_buffer.len(),
                    dispatched.contains(&next_needed),
                    bridge_pending,
                );

                // OOM guard: exponentially throttle max_ahead as stall_count grows.
                //
                // When validation is stuck at height H, workers keep downloading blocks
                // at H+1000, H+2000, ... and feeding them into the prefetch pipeline.
                // With 11 peers × 128-block chunks each holding ~2 MB blocks in memory,
                // the prefetch queue and reorder buffer can accumulate 4–8 GB of block
                // bodies before the OOM killer fires.
                //
                // Throttle schedule (per stall = coord_stall_secs of no progress):
                //   stall 1  → requeue gap micro-chunks only (no throttle — first stall
                //              is often a slow peer, not runaway prefetch memory)
                //   stall 2  → max_ahead = base / 2
                //   stall 3  → max_ahead = base / 4
                //   stall 4+ → max_ahead = chunk_size (one batch per peer, minimum)
                //
                // This stops new block downloads far ahead of the stuck height and lets
                // the prefetch pipeline drain its buffered blocks naturally.  The cap is
                // restored to `max_ahead_blocks` once blocks flow again.
                {
                    let new_max = if coord_stall_count >= 4 {
                        // Hard floor: one chunk per peer so workers can still make progress
                        // on heights close to the stuck block.
                        chunk_size_for_ahead.max(16)
                    } else if coord_stall_count >= 2 {
                        max_ahead_blocks >> (coord_stall_count - 1)
                    } else {
                        max_ahead_blocks
                    };
                    let current = max_ahead_live.load(Ordering::Relaxed);
                    if new_max < current {
                        max_ahead_live.store(new_max, Ordering::Relaxed);
                        warn!(
                            "Coordinator stall #{}: throttling max_ahead {} → {} to bound prefetch memory",
                            coord_stall_count, current, new_max
                        );
                    }
                }

                // Every 10 stalls: emit a deeper diagnostic to verify validation_height
                // and prefetch channel health (helps distinguish true validation stall
                // from coordinator-reads-stale-height phantom stall).
                if coord_stall_count % 10 == 0 {
                    let vh_raw =
                        validation_height_for_coord.load(std::sync::atomic::Ordering::SeqCst);
                    warn!(
                        "[IBD_COORD_DIAG] stall_count={} vh_seqcst={} next_needed_check={} dispatched_has_needed={} reorder_buf_len={} next_prefetch={}",
                        coord_stall_count,
                        vh_raw,
                        vh_raw + 1,
                        dispatched.contains(&(vh_raw + 1)),
                        reorder_buffer.len(),
                        next_prefetch_height,
                    );
                }
                let _ = stall_tx_for_coord.send(stall_height);
                // Same span as the dispatch reissue. A stall that only
                // punches `next_needed` leaves the rest of the delivered
                // hole marked, and idle peers take the far side of it.
                {
                    let first_ahead = reorder_first_ahead(&reorder_buffer, next_needed);
                    assigner_for_coord.set_window_hole_until(first_ahead.unwrap_or(0));
                    let until = first_ahead.unwrap_or(next_needed.saturating_add(1));
                    assigner_for_coord.window_reissue_absent(next_needed, until);
                }
                if assigner_for_coord.wan_stall_micro_allowed(stall_height) {
                    assigner_for_coord.requeue_stall_gaps(stall_height, None);
                }
                if stall_height != next_needed
                    && assigner_for_coord.wan_stall_micro_allowed(next_needed)
                {
                    assigner_for_coord.requeue_stall_gaps(next_needed, None);
                }
                continue;
            }
        };
        if n == 0 {
            info!(
                "Coordinator: block_rx closed (total_received={})",
                total_received
            );
            // Channel closed — drain remaining reorder_buffer (any order), then exit.
            // Do not mark the feeder done here: blocks may still be in prefetch/bridge
            // pipeline. Premature guard.1 caused validation to exit before tail blocks
            // (e.g. height 955035) reached the feeder buffer.
            dispatch_heights_buf.clear();
            dispatch_heights_buf.extend(
                reorder_buffer
                    .keys()
                    .filter(|&&h| !dispatched.contains(&h))
                    .copied(),
            );
            for h in dispatch_heights_buf.drain(..) {
                let Some((block, witnesses)) = reorder_buffer.remove(&h) else {
                    continue;
                };
                dispatched.insert(h);
                if h == bootstrap_end {
                    assigner_for_coord.mark_bootstrap_complete();
                    info!(
                        "IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled",
                        h
                    );
                }
                // In engine mode skip key extraction: keys are unused by the engine
                // validation path. Only tx_ids are needed for SpendSession::append.
                prepare_coord_dispatch_bufs(
                    coord_engine_mode,
                    &block,
                    &mut coord_tx_ids_buf,
                    &mut coord_keys_buf,
                );
                let store = &ibd_store_v2_for_coord;
                let keys_owned = std::mem::take(&mut coord_keys_buf);
                let tx_ids_owned = std::mem::take(&mut coord_tx_ids_buf);
                let item = (
                    Arc::clone(store),
                    keys_owned,
                    tx_ids_owned,
                    h,
                    block,
                    witnesses,
                    coord_engine_mode,
                );
                if let Some((h, block, witnesses)) = dispatch_to_prefetch(item) {
                    dispatched.remove(&h);
                    reorder_buffer.insert(h, (block, witnesses));
                }
            }
            info!("Coordinator: done, sent {} blocks", total_received);
            break;
        }
        // Seq-1: When sequential, process batch directly — do NOT drain into reorder_buffer first.
        if sequential {
            // Seq-1: cap try_emit work per recv batch. Full-channel dumps (~1000
            // bodies) burned ~10s/loop (Case C once/iter → ~6 BPS). Absolute tip-only
            // tip-crawled from ignition — park when tip missing; otherwise emit a
            // small contiguous band and park the rest for later loops.
            let seq_batch_t0 = std::time::Instant::now();
            batch.sort_by_key(|(h, _, _)| *h);
            let tip_need = validation_height_for_coord
                .load(Ordering::Relaxed)
                .saturating_add(1);
            let tip_body_present = reorder_buffer.contains_key(&tip_need)
                || ready_bridge_for_coord
                    .as_ref()
                    .is_some_and(|b| b.pending_contains(tip_need))
                || {
                    let g = feeder_state_for_coord.0.lock();
                    g.0.get(tip_need).is_some()
                }
                || batch.iter().any(|(h, _, _)| *h == tip_need);
            // Match mid-loop sequential band (keep coordinator loops short).
            const SEQ_EMIT_BAND: u64 = 32;
            let batch_n = batch.len();
            let mut seq_parked = 0usize;
            let mut seq_emitted = 0usize;
            for (h, block, witnesses) in batch.drain(..) {
                total_received += 1;
                if total_received == 1 {
                    info!("Coordinator: first block received, height {}", h);
                }
                if total_received <= 3 || total_received % 500 == 0 {
                    debug!(
                        "[IBD] Coordinator: block {} (total_received={}) [sequential]",
                        h, total_received
                    );
                }
                if h == bootstrap_end {
                    assigner_for_coord.mark_bootstrap_complete();
                    info!(
                        "IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled",
                        h
                    );
                }
                if h < tip_need {
                    continue;
                }
                let park = if !tip_body_present {
                    h != tip_need
                } else {
                    h > tip_need.saturating_add(SEQ_EMIT_BAND)
                };
                if park {
                    let _ = insert_reorder_gap_aware(
                        &mut reorder_buffer,
                        h,
                        block,
                        witnesses,
                        tip_need,
                        dynamic_buffer_limit,
                        admit_window,
                        bridge_pending_max,
                    );
                    seq_parked += 1;
                    continue;
                }
                // Single-peer (sequential) path: still go through prefetch so the worker
                // pool warm-loads UTXOs in parallel with validation. Compute keys here
                // (same call the parallel path uses) so the prefetch worker has a key
                // list to MultiGet — sending an empty `keys` would force the validation
                // worker to re-derive them and fall through to a synchronous disk load.
                // In engine mode skip key extraction: keys are unused by the engine
                // validation path. Only tx_ids are needed for SpendSession::append.
                prepare_coord_dispatch_bufs(
                    coord_engine_mode,
                    &block,
                    &mut coord_tx_ids_buf,
                    &mut coord_keys_buf,
                );
                let store = &ibd_store_v2_for_coord;
                let keys_owned = std::mem::take(&mut coord_keys_buf);
                let tx_ids_owned = std::mem::take(&mut coord_tx_ids_buf);
                let item = (
                    Arc::clone(store),
                    keys_owned,
                    tx_ids_owned,
                    h,
                    block,
                    witnesses,
                    coord_engine_mode,
                );
                if let Some((h, block, witnesses)) = dispatch_to_prefetch(item) {
                    let _ = insert_reorder_gap_aware(
                        &mut reorder_buffer,
                        h,
                        block,
                        witnesses,
                        tip_need,
                        dynamic_buffer_limit,
                        admit_window,
                        bridge_pending_max,
                    );
                } else {
                    next_prefetch_height = h + 1;
                    seq_emitted += 1;
                }
            }
            if synthetic_wan::bulk_local_disk_stream()
                && seq_batch_t0.elapsed() >= Duration::from_millis(500)
            {
                warn!(
                    "[IBD_CLIFF_SEQ] tip={} tip_present={} batch_n={} parked={} emitted={} ms={} reorder={}",
                    tip_need,
                    tip_body_present,
                    batch_n,
                    seq_parked,
                    seq_emitted,
                    seq_batch_t0.elapsed().as_millis(),
                    reorder_buffer.len(),
                );
            }
        } else {
            // Parallel: drain batch into reorder_buffer, then dispatch ALL available
            // heights (out-of-order) to prefetch. The OrderedReadyBridge reorders them
            // before the feeder sees them, eliminating HOL blocking at the coordinator.
            for (height, block, witnesses) in batch.drain(..) {
                if total_received == 0 {
                    info!("Coordinator: first block received, height {}", height);
                }
                total_received += 1;
                // When blocks start flowing again after a stall, restore max_ahead_live
                // to the full base value so workers can resume lookahead.
                if coord_stall_count > 0 {
                    let current = max_ahead_live.load(Ordering::Relaxed);
                    if current < max_ahead_blocks {
                        max_ahead_live.store(max_ahead_blocks, Ordering::Relaxed);
                        coord_stall_count = 0;
                        info!(
                            "Coordinator: stall cleared at height {} — restoring max_ahead to {}",
                            height, max_ahead_blocks
                        );
                    }
                }
                if total_received <= 3 || total_received % 500 == 0 {
                    debug!(
                        "[IBD] Coordinator: block {} (total_received={}, reorder_len={})",
                        height,
                        total_received,
                        reorder_buffer.len() + 1
                    );
                }
                let next_needed = validation_height_for_coord.load(Ordering::Relaxed) + 1;
                if dispatched.contains(&height) {
                    // Gap recovery: re-queued chunk re-streams heights validation still needs.
                    dispatched.remove(&height);
                }
                let _ = insert_reorder_gap_aware(
                    &mut reorder_buffer,
                    height,
                    block,
                    witnesses,
                    next_needed,
                    dynamic_buffer_limit,
                    admit_window,
                    bridge_pending_max,
                );
            }
            // Dispatch all undispatched blocks from the buffer (any order).
            // Reuse scratch buffer to avoid per-loop Vec allocation.
            let cursor_now = ready_bridge_for_coord
                .as_ref()
                .and_then(|b| b.next_expected());
            if let Some(h) = cursor_now {
                let cursor_in_feeder = {
                    let g = feeder_state_for_coord.0.lock();
                    g.0.get(h).is_some()
                };
                let cursor_in_pending = ready_bridge_for_coord
                    .as_ref()
                    .is_some_and(|b| b.pending_contains(h));
                release_cursor_stuck_in_reorder(
                    &mut dispatched,
                    reorder_buffer.contains_key(&h),
                    cursor_now,
                    cursor_in_feeder,
                    cursor_in_pending,
                );
            }
            dispatch_heights_buf.clear();
            dispatch_heights_buf.extend(
                reorder_buffer
                    .keys()
                    .filter(|&&h| !dispatched.contains(&h))
                    .copied(),
            );
            let val_h_dispatch = validation_height_for_coord.load(Ordering::Relaxed);
            let next_needed_dispatch = val_h_dispatch.saturating_add(1);
            let gap_missing_dispatch = !reorder_buffer.contains_key(&next_needed_dispatch);
            let next_expected_missing_dispatch = ready_bridge_for_coord
                .as_ref()
                .is_some_and(|b| b.next_expected().is_some_and(|n| !b.pending_contains(n)));
            let wan_tip_crawl_dispatch = next_needed_dispatch > live_body_tip_for_coord;
            let bulk_catchup_dispatch = wan_bulk_catchup(
                assigner_for_coord
                    .header_tip()
                    .max(effective_end_live_for_coord.load(Ordering::Relaxed)),
                next_needed_dispatch,
            );
            let feeder_len_dispatch = IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
            let feeder_starved_dispatch =
                wan_feeder_prefetch_starved(wan_tip_crawl_dispatch, feeder_len_dispatch);
            if wan_tip_crawl_dispatch
                && feeder_starved_dispatch
                && (!gap_missing_dispatch
                    || reorder_has_feeder_prefetch_band(
                        &reorder_buffer,
                        next_needed_dispatch,
                        W34_FEEDER_PREFETCH_TARGET,
                    ))
            {
                let band_end =
                    next_needed_dispatch.saturating_add(W34_FEEDER_PREFETCH_TARGET as u64);
                dispatch_heights_buf.sort_by_key(|h| {
                    if *h >= next_needed_dispatch && *h < band_end {
                        *h - next_needed_dispatch
                    } else {
                        u64::MAX / 2 + *h
                    }
                });
            }
            for h in dispatch_heights_buf.drain(..) {
                if reorder_buffer.len() >= dynamic_buffer_limit {
                    break; // respect backpressure cap
                }
                if !bridge_cursor_dispatch_allowed(h, next_needed_dispatch, cursor_now)
                    && defer_bridge_ahead_dispatch(
                        h,
                        next_needed_dispatch,
                        gap_missing_dispatch,
                        next_expected_missing_dispatch,
                        admit_window,
                        wan_tip_crawl_dispatch,
                        feeder_starved_dispatch,
                        bulk_catchup_dispatch,
                    )
                {
                    continue;
                }
                if let Some(ref bridge) = ready_bridge_for_coord {
                    if !bridge.may_accept_height(h, bridge_pending_max) {
                        // Skip refused ahead heights; keep scanning for next_expected
                        // (always allowed). Breaking here wedged gap fills behind
                        // lower refused keys while bridge_pending was at cap.
                        continue;
                    }
                }
                let Some((block, witnesses)) = reorder_buffer.remove(&h) else {
                    continue;
                };
                dispatched.insert(h);
                if h >= next_prefetch_height {
                    next_prefetch_height = h + 1;
                }
                if h == bootstrap_end {
                    assigner_for_coord.mark_bootstrap_complete();
                    info!(
                        "IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled",
                        h
                    );
                }
                // In engine mode skip key extraction: keys are unused by the engine
                // validation path. Only tx_ids are needed for SpendSession::append.
                prepare_coord_dispatch_bufs(
                    coord_engine_mode,
                    &block,
                    &mut coord_tx_ids_buf,
                    &mut coord_keys_buf,
                );
                let store = &ibd_store_v2_for_coord;
                let keys_owned = std::mem::take(&mut coord_keys_buf);
                let tx_ids_owned = std::mem::take(&mut coord_tx_ids_buf);
                let item = (
                    Arc::clone(store),
                    keys_owned,
                    tx_ids_owned,
                    h,
                    block,
                    witnesses,
                    coord_engine_mode,
                );
                if let Some((h, block, witnesses)) = dispatch_to_prefetch(item) {
                    dispatched.remove(&h);
                    reorder_buffer.insert(h, (block, witnesses));
                    break;
                }
            }
        }
    }
}

impl ParallelIBD {
    /// Create a new parallel IBD coordinator
    pub fn new(config: ParallelIBDConfig) -> Self {
        Self {
            config,
            bip54_activation_from_version_bits: std::sync::atomic::AtomicU64::new(u64::MAX),
            peer_semaphores: Arc::new(DashMap::new()),
            peer_blocks_semaphores: Arc::new(DashMap::new()),
            peer_scorer: Arc::new(crate::network::peer_scoring::PeerScorer::new()),
        }
    }

    /// Get the peer scorer (for external access to stats)
    pub fn peer_scorer(&self) -> &Arc<crate::network::peer_scoring::PeerScorer> {
        &self.peer_scorer
    }

    /// Initialize peer semaphores. Uses DashMap so replacement workers can be added dynamically.
    pub fn initialize_peers(&mut self, peer_ids: &[String]) {
        let peer_sem: DashMap<String, Arc<Semaphore>> = peer_ids
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    Arc::new(Semaphore::new(self.config.max_concurrent_per_peer)),
                )
            })
            .collect();
        let blocks_sem: DashMap<String, Arc<Semaphore>> = peer_ids
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    Arc::new(Semaphore::new(self.config.max_blocks_in_transit_per_peer)),
                )
            })
            .collect();
        self.peer_semaphores = Arc::new(peer_sem);
        self.peer_blocks_semaphores = Arc::new(blocks_sem);
    }

    /// Download blocks in parallel from multiple peers
    ///
    /// Algorithm:
    /// 1. Download headers first (sequential, fast)
    /// 2. Split block range into chunks
    /// 3. Assign chunks to peers (round-robin)
    /// 4. Download chunks in parallel
    /// 5. Validate and store blocks sequentially (maintain order)
    ///
    /// Validation runs on a dedicated std::thread (not tokio) — no block_in_place on hot path.
    pub async fn sync_parallel(
        self: std::sync::Arc<Self>,
        start_height: u64,
        target_height: u64,
        peer_ids: &[String],
        blockstore: Arc<BlockStore>,
        storage: Option<&Arc<Storage>>,
        protocol: Arc<BitcoinProtocolEngine>,
        utxo_set: &mut UtxoSet,
        network: Option<Arc<NetworkManager>>,
        event_publisher: Option<Arc<crate::node::event_publisher::EventPublisher>>,
    ) -> Result<()> {
        if peer_ids.is_empty() && !synthetic_wan::allow_zero_real_peers() {
            return Err(anyhow::anyhow!("No peers available for parallel IBD"));
        }

        // IBD requires storage (IbdUtxoStore needs disk for UTXO persistence). Fail fast with clear error.
        let storage = match storage {
            Some(s) => s,
            None => {
                return Err(anyhow::anyhow!(
                    "IBD requires storage. Run with a data directory (e.g. --datadir) or ensure storage is initialized."
                ));
            }
        };

        #[cfg(not(feature = "production"))]
        return Err(anyhow::anyhow!(
            "IBD requires production build. Compile with --features production."
        ));

        info!(
            "Starting parallel IBD from height {} to {} using {} peers",
            start_height,
            target_height,
            peer_ids.len()
        );

        // Proactively seed full-history (archive) peers in the background so archive nodes are
        // reachable before we pass the local-block-store boundary and need network downloads.
        // Skip when BLVM_IBD_PEERS is pinned (LAN/Mode T archive tip-now) — DNS races the pin.
        if let Some(ref net) = network {
            if skip_ibd_archive_dns_seed() {
                info!("IBD: skipping archive DNS seed — BLVM_IBD_PEERS pinned");
            } else {
                let net_clone = Arc::clone(net);
                tokio::spawn(async move {
                    let _ = net_clone.discover_archive_peers_from_dns().await;
                });
            }
        }

        PARALLEL_IBD_SESSION_ACTIVE.store(true, Ordering::Release);

        // R-346: roster top-up. Connections were made once at startup (`Need 64 more peers`
        // → `Connected to 64`) and never replaced during IBD; evicted / gone / never-ready
        // peers thinned `ready` every run. Re-run `connect_peers_from_database` while the
        // session is live and the connected count is under target. Pinned peers: skip.
        if let Some(ref net) = network {
            if let (Some(target), false) = (ibd_roster_target(), skip_ibd_archive_dns_seed()) {
                let net_clone = Arc::clone(net);
                let session_id = PARALLEL_IBD_SESSION_ID.load(Ordering::Acquire) + 1;
                tokio::spawn(async move {
                    let every = std::time::Duration::from_secs(ibd_roster_topup_secs());
                    let mut wait = std::time::Duration::from_secs(10).min(every);
                    loop {
                        tokio::time::sleep(wait).await;
                        wait = every;
                        if !PARALLEL_IBD_SESSION_ACTIVE.load(Ordering::Acquire)
                            || IBD_SHUTDOWN_REQUESTED.load(Ordering::Relaxed)
                            || PARALLEL_IBD_SESSION_ID.load(Ordering::Acquire) != session_id
                        {
                            break;
                        }
                        let have = net_clone.peer_count();
                        if have >= target {
                            continue;
                        }
                        match net_clone.connect_peers_from_database(target).await {
                            Ok(n) => info!(
                                "[IBD_ROSTER_TOPUP] peers={} target={} connected={}",
                                have, target, n
                            ),
                            Err(e) => warn!(
                                "[IBD_ROSTER_TOPUP] peers={} target={} failed: {}",
                                have, target, e
                            ),
                        }
                    }
                });
            }
        }
        let _ibd_session_guard = ParallelIbdSessionGuard;
        let coord_session_id = PARALLEL_IBD_SESSION_ID.fetch_add(1, Ordering::AcqRel) + 1;

        // Enable write-optimised mode: skip per-commit fdatasync for all LMDB writes during IBD.
        // IbdNosyncGuard re-enables sync and flushes on drop (any exit path).
        if let Err(e) = storage.set_ibd_nosync(true) {
            warn!("Failed to enable IBD nosync mode (continuing with default sync): {e}");
        } else {
            info!("IBD nosync mode enabled (MDB_NOSYNC active; explicit flush at UTXO watermarks)");
        }
        let _nosync_guard = IbdNosyncGuard(storage.as_ref());

        let headers_start = std::time::Instant::now();

        // Download headers first (sequential, but fast); iterate until chain tip.
        if let Some(ref ep) = event_publisher {
            ep.publish_headers_sync_started(start_height).await;
        }
        info!("Downloading headers...");
        let network_for_headers = network.clone();
        let header_result = headers::download_headers(
            self.peer_scorer.clone(),
            start_height,
            target_height,
            peer_ids,
            &blockstore,
            network_for_headers,
            self.config.headers_timeout_secs,
            self.config.headers_max_failures,
            event_publisher.clone(),
        )
        .await
        .context("Failed to download headers")?;

        let actual_synced_height = header_result.tip_height;

        // Guard: if header sync claims to have completed at height 0 but the
        // target is > 1000, something went wrong (e.g. all peers failed).  Treat
        // this as a failure so the IBD loop retries rather than entering block-relay
        // mode with no headers (which causes OOM from unbounded block requests).
        if actual_synced_height == 0 && target_height > 1000 {
            return Err(anyhow::anyhow!(
                "Header sync returned height 0 against target {}; all peers failed",
                target_height
            ));
        }

        if let Some(ref ep) = event_publisher {
            let duration_secs = headers_start.elapsed().as_secs();
            ep.publish_headers_sync_completed(actual_synced_height, duration_secs)
                .await;
        }

        // Use the actual synced height (may be less than target_height if we reached chain tip)
        let mut effective_end_height = actual_synced_height.min(target_height);
        if let Some(cap) = ibd_end_height_cap() {
            let before = effective_end_height;
            effective_end_height = effective_end_height.min(cap);
            info!(
                "[IBD_END_HEIGHT] capping effective_end {} → {} (BLVM_IBD_END_HEIGHT={})",
                before, effective_end_height, cap
            );
        }
        // F-C4: live effective end — coordinator/validation read this; tip-follow extends it.
        let effective_end_live = Arc::new(AtomicU64::new(effective_end_height));
        if ibd_follow_tip_enabled() {
            info!(
                "[IBD_TIP_FOLLOW] enabled — effective_end will track peer tip (poll={}s)",
                ibd_follow_tip_poll_secs()
            );
        }
        info!(
            "Headers synced up to height {}, will download blocks for heights {} to {}",
            actual_synced_height, start_height, effective_end_height
        );

        // Peers have no blocks past our header tip (e.g. both at genesis). Nothing to fetch;
        // exit IBD so the node can run the main loop and pick up new blocks via relay/sync.
        if effective_end_height < start_height {
            info!(
                "Parallel IBD: no block range to download (end {} < start {}); treating as caught up to peer tip",
                effective_end_height, start_height
            );
            return Ok(());
        }

        // Memory auto-tuning runs after headers so `/proc/meminfo` reflects post-header RAM
        // (IDE/GPU spikes at process start no longer misclassify dedicated tmux IBD as Shared).
        let db_file_size_mb = storage.db_file_size_mb();
        if db_file_size_mb > 0 {
            info!(
                "IBD storage: file-backed DB size = {} MB (deducted from RSS budget to reserve room for anonymous pages)",
                db_file_size_mb
            );
        }
        let ibd_tuning = IbdTuningContext {
            storage_backend: storage.database_backend(),
            ibd_dedicated: self.config.dedicated,
            db_file_size_mb,
        };
        let mut mem_guard = MemoryGuard::new_for_ibd(ibd_tuning);

        // Drop extremely slow peers (>90s average latency); keep at least two peers when possible.
        const MAX_ACCEPTABLE_LATENCY_MS: f64 = 90_000.0; // 90 seconds
        let filtered_peers: Vec<String> = if peer_ids.len() > 2 {
            let mut scored_peers: Vec<(String, f64)> = peer_ids
                .iter()
                .map(|id| {
                    let latency = if let Ok(addr) = id.parse::<std::net::SocketAddr>() {
                        self.peer_scorer
                            .get_stats(&addr)
                            .map(|s| s.avg_block_latency_ms)
                            .unwrap_or(1000.0) // New peers get default latency
                    } else {
                        1000.0
                    };
                    (id.clone(), latency)
                })
                .collect();

            // Sort by latency (fastest first)
            scored_peers.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

            // Keep fast peers and at least 2 peers total
            let fast_peers: Vec<String> = scored_peers
                .iter()
                .filter(|(_, lat)| *lat < MAX_ACCEPTABLE_LATENCY_MS)
                .map(|(id, _)| id.clone())
                .collect();

            if fast_peers.len() >= 2 {
                info!(
                    "Filtered peers to {} fast peers (dropped {} slow peers with >90s latency)",
                    fast_peers.len(),
                    peer_ids.len() - fast_peers.len()
                );
                fast_peers
            } else {
                // Keep top 2 peers by latency even if all are slow
                info!("All peers are slow, keeping top 2 by latency");
                scored_peers.into_iter().take(2).map(|(id, _)| id).collect()
            }
        } else {
            peer_ids.to_vec()
        };

        // Sort peers: LAN first, then by latency (fastest first), then by score.
        // CRITICAL: Bootstrap chunk goes to first peer. With only WAN peers, latency order
        // ensures we pick the fastest one — avoids stall at block 99 waiting for block 100.
        let mut filtered_peers = filtered_peers;
        filtered_peers.sort_by(|a, b| {
            let a_addr = a.parse::<SocketAddr>().ok();
            let b_addr = b.parse::<SocketAddr>().ok();
            let a_lan = a_addr.map(|s| is_lan_peer(&s)).unwrap_or(false);
            let b_lan = b_addr.map(|s| is_lan_peer(&s)).unwrap_or(false);
            // 1. LAN first
            match (a_lan, b_lan) {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => {}
            }
            // 2. Same LAN status: fastest (lowest latency) first
            let a_lat = a_addr
                .and_then(|s| self.peer_scorer.get_stats(&s))
                .map(|s| s.avg_block_latency_ms)
                .unwrap_or(1000.0);
            let b_lat = b_addr
                .and_then(|s| self.peer_scorer.get_stats(&s))
                .map(|s| s.avg_block_latency_ms)
                .unwrap_or(1000.0);
            a_lat
                .partial_cmp(&b_lat)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    // 3. Tie-break: higher score
                    let a_score = a_addr
                        .map(|s| self.peer_scorer.get_score(&s))
                        .unwrap_or(1.0);
                    let b_score = b_addr
                        .map(|s| self.peer_scorer.get_score(&s))
                        .unwrap_or(1.0);
                    b_score
                        .partial_cmp(&a_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.cmp(b)) // 4. Stable: same addr string order when all equal
        });

        // preferred_peers: prefer these peers when connected; fall back if none match yet.
        filtered_peers = filter_ibd_download_peers(&self.config.preferred_peers, filtered_peers);

        // Zero-peer: synthetic WAN harness (multi fake peers) or local-disk replay.
        if filtered_peers.is_empty() && synthetic_wan::allow_zero_real_peers() {
            let body_tip = probe_confirmed_body_height(&blockstore).unwrap_or(0);
            let sparse_max = if body_tip == 0 {
                probe_highest_stored_body_height(&blockstore).unwrap_or(0)
            } else {
                body_tip
            };
            if sparse_max >= start_height {
                if synthetic_wan::enabled() && synthetic_wan::use_fake_download_peers() {
                    filtered_peers = synthetic_wan::peer_ids();
                    info!(
                        "IBD: synthetic WAN harness — {} fake peers, bodies through {} (start={}); \
                         wan_body_tip override={:?}, getdata_delay_ms={} replay={:?}",
                        filtered_peers.len(),
                        sparse_max,
                        start_height,
                        synthetic_wan::body_tip_override(),
                        synthetic_wan::getdata_delay_ms(),
                        synthetic_wan::replay_fixture()
                    );
                } else if synthetic_wan::enabled() {
                    // Bulk synth (delay=0, peer_count=1): local-disk stream + body_tip pin.
                    // Fake multi-peer path ahead-flooded reorder → SEND_WAIT ~9s → ~6–8 wall BPS.
                    filtered_peers = local_disk_peer_ids();
                    info!(
                        "IBD: synthetic WAN bulk — local-disk stream, bodies through {} (start={}); \
                         wan_body_tip override={:?} local_disk_peers={} (FORCE_PEERS=1 or PEER_COUNT>=2 for fake peers)",
                        sparse_max,
                        start_height,
                        synthetic_wan::body_tip_override(),
                        filtered_peers.len()
                    );
                } else {
                    filtered_peers = local_disk_peer_ids();
                    info!(
                        "IBD: zero-peer local-disk workers — {} peer(s) bodies through {} (start={}); \
                         streaming disk→channel (not LOCAL_GAP inject cadence)",
                        filtered_peers.len(),
                        sparse_max,
                        start_height
                    );
                }
                for p in &filtered_peers {
                    self.peer_semaphores.insert(
                        p.clone(),
                        Arc::new(Semaphore::new(self.config.max_concurrent_per_peer)),
                    );
                    self.peer_blocks_semaphores.insert(
                        p.clone(),
                        Arc::new(Semaphore::new(self.config.max_blocks_in_transit_per_peer)),
                    );
                }
            }
        }

        let ibd_mode: &str = &self.config.mode;
        let wan_multi_peer = ParallelIBDConfig::is_wan_only_multi_peer(&filtered_peers);
        filtered_peers = ParallelIBDConfig::collapse_wan_only_download_peers(filtered_peers);
        if wan_multi_peer {
            info!(
                "WAN multi-peer IBD: {} peers, 1 worker/peer, work-stealing, peer blacklisting (set BLVM_IBD_WAN_SINGLE_PEER=1 to force single-peer)",
                filtered_peers.len()
            );
        } else if filtered_peers.len() == 1 {
            if let Ok(addr) = filtered_peers[0].parse::<SocketAddr>() {
                if !is_lan_peer(&addr) {
                    info!(
                        "WAN-only IBD: single fastest peer {} for download (mode={})",
                        filtered_peers[0], ibd_mode
                    );
                }
            }
        }

        // mode=sequential: single-peer mode. Core-like earliest-first, no chunk-boundary stalls.
        if ibd_mode.eq_ignore_ascii_case("sequential") {
            if let Some(best) = filtered_peers.first().cloned() {
                filtered_peers = vec![best.clone()];
                info!(
                    "BLVM_IBD_MODE=sequential: single-peer mode ({}), Core-like block fetch",
                    best
                );
            }
        }

        // Split the height range into chunks and assign peers (weighted by speed).
        // BLVM_IBD_EARLIEST_FIRST=1: assign all chunks to fastest peer (Core-like, avoids chunk-boundary stalls)
        let scored_peers: Vec<(String, f64)> = filtered_peers
            .iter()
            .map(|p| {
                let score = if let Ok(addr) = p.parse::<SocketAddr>() {
                    self.peer_scorer.get_score(&addr)
                } else {
                    1.0
                };
                (p.clone(), score)
            })
            .collect();

        // P1: cap WAN download workers to top-N scored peers. With max_ahead=512 and
        // chunk_size=32 only ~16 slots exist — 90 workers just amplify gap timeout storms.
        if wan_multi_peer {
            let confirmed_probe = probe_confirmed_body_height(&blockstore).unwrap_or(0);
            let past_body_tip =
                confirmed_probe > 0 && start_height.saturating_sub(1) > confirmed_probe;
            let cap = if past_body_tip {
                ibd_wan_gap_active_peer_cap()
            } else {
                ibd_active_peer_cap()
            };
            if filtered_peers.len() > cap {
                let mut ranked = scored_peers.clone();
                ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                ranked.truncate(cap);
                filtered_peers = ranked.iter().map(|(p, _)| p.clone()).collect();
                info!(
                    "[IBD_P1] capped WAN download peers {} → {} (top scored; {}={})",
                    scored_peers.len(),
                    filtered_peers.len(),
                    if past_body_tip {
                        "BLVM_IBD_WAN_GAP_ACTIVE_PEERS"
                    } else {
                        "BLVM_IBD_ACTIVE_PEERS"
                    },
                    cap
                );
            }
        }
        let scored_peers: Vec<(String, f64)> = filtered_peers
            .iter()
            .map(|p| {
                let score = scored_peers
                    .iter()
                    .find(|(id, _)| id == p)
                    .map(|(_, s)| *s)
                    .unwrap_or(1.0);
                (p.clone(), score)
            })
            .collect();

        // WAN multi-peer: default 1 worker/peer (avoid thundering-herd).
        // Live 2026-07-15: sticky dual-pipe logged **0** fires — assigner allows
        // max_in_flight≥2 but a single worker blocks inside `download_chunk`, so the
        // second range never arms. Give **2** workers per peer when enabled so the
        // preferred sticky (or any peer that becomes sticky) can dual-pipe; non-sticky
        // still gated by max_in_flight=1 so the spare worker idles.
        // Kill switch: BLVM_IBD_STICKY_DUAL_WORKER=0.
        let wan_dual = wan_multi_peer && sticky_dual_worker_enabled();
        let sole_tip_pri = sole_tip_priority_enabled();
        let total_download_workers: usize = filtered_peers
            .iter()
            .map(|peer_id| {
                if wan_multi_peer {
                    if wan_dual { 2 } else { 1 }
                } else if sole_tip_pri {
                    // tc167 tip90≈54.4 with accidental spawn=clamp(2,6); spawn=1 (tc169)
                    // thinned tip90≈42. Keep tip-glue + max_in_flight=1 but allow the
                    // priority worker count so assign/re-arm is not single-threaded.
                    let priority = scored_peers
                        .iter()
                        .find(|(p, _)| p == peer_id)
                        .map(|(_, s)| *s)
                        .unwrap_or(1.0);
                    ((2.0 * priority) as usize).clamp(2, 6)
                } else {
                    let priority = scored_peers
                        .iter()
                        .find(|(p, _)| p == peer_id)
                        .map(|(_, s)| *s)
                        .unwrap_or(1.0);
                    ((2.0 * priority) as usize).clamp(2, 6)
                }
            })
            .sum::<usize>()
            .max(1);
        if sole_tip_pri {
            info!(
                "[IBD_SOLE_TIP_PRIORITY] peers={} total_workers={} — tip-glue + max_in_flight via TOP_PEER_IN_FLIGHT (multi-worker assign)",
                filtered_peers.len(),
                total_download_workers
            );
        }

        // Adaptive chunk sizing: on RAM-constrained hosts `max_ahead_blocks` may be too
        // small to give every peer a full default chunk simultaneously.  For example:
        //
        //   max_ahead=250, chunk_size=128, 14 peers → only 2 peers can have work at
        //   once (slots 0..1 fit in the 250-block window; peer 3+ is blocked).
        //
        // Fix: reduce chunk_size so at least `ceil(num_peers/2)` peers can have a chunk
        // in-flight within the max_ahead window.  Minimum is 16 (one GetData round-trip
        // still covers many blocks).  The user can always override with
        // BLVM_IBD_CHUNK_SIZE or BLVM_IBD_MAX_AHEAD.
        //
        // Only adapts when max_ahead_blocks comes from the MemoryGuard auto-tune (not a
        // user-supplied config value) to avoid overriding explicit operator choices.
        let effective_chunk_size: u64 = {
            let raw_max_ahead = self
                .config
                .max_ahead_blocks
                .unwrap_or(mem_guard.max_ahead_blocks);
            let n = filtered_peers.len().max(1) as u64;
            let needed_for_all_peers = n * self.config.chunk_size;
            if self.config.max_ahead_blocks.is_some() || needed_for_all_peers <= raw_max_ahead {
                // Explicit override or plenty of ahead-window: keep as configured.
                self.config.chunk_size
            } else {
                // Scale chunk_size down so at least half the peers can be active.
                // Using half (not all) leaves headroom for the validation pipeline to
                // consume blocks and free slots for the second half.
                let active_peers = n.div_ceil(2).max(1);
                let per_peer = raw_max_ahead / active_peers;
                // Round down to nearest multiple of 16 (clean GetData batch boundary).
                let snapped = (per_peer / 16) * 16;
                let adapted = snapped.max(16).min(self.config.chunk_size);
                if adapted < self.config.chunk_size {
                    info!(
                        "IBD: adaptive chunk_size {} → {} (max_ahead={}, {} peers, needed={})",
                        self.config.chunk_size, adapted, raw_max_ahead, n, needed_for_all_peers
                    );
                }
                adapted
            }
        };

        let chunks = if effective_chunk_size != self.config.chunk_size {
            let adapted_config = ParallelIBDConfig {
                chunk_size: effective_chunk_size,
                ..self.config.clone()
            };
            create_chunks_impl(
                &adapted_config,
                start_height,
                effective_end_height,
                &filtered_peers,
                Some(&scored_peers),
            )
        } else {
            self.create_chunks(
                start_height,
                effective_end_height,
                &filtered_peers,
                Some(&scored_peers),
            )
        };
        info!(
            "Created {} chunks for parallel download using {} peers (chunk_size={})",
            chunks.len(),
            filtered_peers.len(),
            effective_chunk_size
        );

        let block_sync_start = std::time::Instant::now();
        if let Some(ref ep) = event_publisher {
            ep.publish_block_sync_started(start_height, effective_end_height)
                .await;
        }

        // Streaming block download + validation pipeline
        //
        // Bounded channel from download workers → coordinator: **required** for RAM safety.
        // Unbounded `mpsc` let WAN workers flood full blocks while the coordinator was busy,
        // causing kernel OOM on 16GiB hosts (each queued item holds a full `Block`).

        // Disable Transparent Huge Pages for this process. THP promotes anonymous
        // pages to 2MB granularity, causing massive internal fragmentation with
        // millions of small UTXO allocations. On a system with THP=[always], this
        // saves ~300MB+ of wasted RSS. Zero performance cost.
        #[cfg(target_os = "linux")]
        {
            // PR_SET_THP_DISABLE = 41
            let ret = unsafe { libc::prctl(41, 1, 0, 0, 0) };
            if ret == 0 {
                info!("Disabled Transparent Huge Pages for this process");
            }
        }

        // Bounded download→coordinator queue + safety valve:
        // 1) Tokio bounded channel → backpressure when coordinator is slow (workers await send).
        // 2) RAM-tier ceiling → cap autosize so high worker×pipeline estimates do not create a
        //    huge queued-block arena on ≤16/32 GiB hosts.
        // 3) Optional BLVM_IBD_DOWNLOAD_QUEUE_MAX_BLOCKS → operator hard cap (min with computed).
        let download_block_queue_cap: usize = {
            let bl = mem_guard.buffer_limit(start_height);
            let pipeline = self
                .config
                .max_concurrent_per_peer
                .max(self.config.max_blocks_in_transit_per_peer);
            const PIPELINE_HORIZON_FOR_CAP: usize = 32;
            let h = pipeline.clamp(1, PIPELINE_HORIZON_FOR_CAP);
            let base = bl.saturating_mul(4);
            let parallel = total_download_workers.saturating_mul(h).saturating_mul(2);
            let floor = if mem_guard.system_total_ram_mb()
                <= memory::MemoryGuard::EXTENDED_SIXTEEN_CLASS_MB
            {
                128
            } else {
                256
            };
            let raw = base.max(parallel).clamp(floor, 8192);
            // Cap blocks buffered on the bounded tokio queue (≠ `MemoryGuard::max_ahead_blocks` /
            // `tier_max_download_ahead_blocks`, which throttle download prefetch). Two separate knobs.
            let ram_ceiling = match mem_guard.system_total_ram_mb() {
                // ≤~18 GiB physical — keep worst‑case queued full blocks bounded on tight hosts.
                m if m <= memory::MemoryGuard::EXTENDED_SIXTEEN_CLASS_MB => 1024,
                m if m <= 32 * 1024 => 4096,
                _ => 8192,
            };
            let env_cap = std::env::var("BLVM_IBD_DOWNLOAD_QUEUE_MAX_BLOCKS")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&n| n > 0);
            let capped = raw.min(ram_ceiling);
            match env_cap {
                Some(e) => capped.min(e).max(floor),
                None => capped.max(floor),
            }
        };
        let (block_tx, mut block_rx) =
            tokio::sync::mpsc::channel::<(u64, SharedBlock, SharedWitnesses)>(
                download_block_queue_cap,
            );
        info!(
            "IBD: download→coordinator channel capacity={} blocks (buffer_limit={}, workers={}, bounded + RAM/env valve)",
            download_block_queue_cap,
            mem_guard.buffer_limit(start_height),
            total_download_workers,
        );
        let (stall_tx, _) = broadcast::channel::<u64>(16);

        // Last block height whose UTXO effects are visible to the coordinator/prefetch path.
        // start_height is the *next* block to validate → parent is start_height - 1 (synced tip).
        let validation_height = Arc::new(AtomicU64::new(start_height.saturating_sub(1)));
        crate::node::parallel_ibd::wire_hash_gate::bind_validation_height(Arc::clone(
            &validation_height,
        ));
        let _ = crate::node::parallel_ibd::late_body::enabled();
        // Sequential chunk assigner: workers get ranges in height order; validation never starves.
        // LAN keeps create_chunks peer affinity; WAN work-stealing ignores it.
        let assigner = Arc::new(ChunkAssigner::from_block_chunks(
            &chunks,
            Arc::clone(&validation_height),
            start_height,
            wan_multi_peer,
        ));
        register_live_assigner(&assigner);
        assigner.set_ibd_end_height(effective_end_height);
        // Seed header tip immediately so bulk catch-up gating works before the first
        // peer-ready refresh (otherwise mid-chain is misclassified as near-tip crawl).
        if let Ok(Some(ht)) = blockstore.highest_stored_height() {
            assigner.set_header_tip(ht);
        } else if effective_end_height > 0 {
            assigner.set_header_tip(effective_end_height);
        }
        // W29: assume tip missing until the coordinator loop confirms otherwise — prevents
        // ahead-partition storms before the first set_tip_gap_missing update.
        assigner.set_tip_gap_missing(true);
        // P5/A4: score-prefer gap races + dual in-flight for top half of peers.
        assigner.set_peer_scores(&scored_peers);
        if filtered_peers.iter().any(|p| is_snapshot_sourced_peer(p)) {
            assigner.set_ibd_ready_peers(
                filtered_peers
                    .iter()
                    .filter(|p| is_snapshot_sourced_peer(p))
                    .cloned()
                    .collect(),
            );
        }
        info!(
            "IBD: {} chunk assignment — {} chunks (work_stealing={})",
            if wan_multi_peer {
                "work-stealing"
            } else {
                "sequential"
            },
            assigner.total_chunks(),
            wan_multi_peer
        );
        // Track which chunks workers are downloading (for debugging; workers push/retain)
        let workers_current_chunks: Arc<tokio::sync::Mutex<Vec<(String, u64, u64)>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));

        let effective_max_entries = mem_guard.utxo_max_entries;
        let utxo_flush_threshold = mem_guard.utxo_flush_threshold;
        // Max blocks download can race ahead of validation. Limits block_rx channel depth.
        let max_ahead_blocks: u64 = self
            .config
            .max_ahead_blocks
            .unwrap_or(mem_guard.max_ahead_blocks);
        // Synth bulk: start at LOCAL_AHEAD (not MemoryGuard 1k+) so workers cannot assign a
        // 1k ahead burst before the coordinator's first L1 clamp tick.
        let max_ahead_init = if synthetic_wan::bulk_local_disk_stream() {
            max_ahead_blocks.min(local_body_ahead_cap())
        } else {
            max_ahead_blocks
        };
        let max_ahead_live = Arc::new(AtomicU64::new(max_ahead_init));
        crate::node::parallel_ibd::late_body::bind_max_ahead(Arc::clone(&max_ahead_live));
        let coord_rss_budget_mb = mem_guard.rss_budget_mb;
        memory::publish_ibd_pressure(
            mem_guard.should_flush(Some((&max_ahead_live, max_ahead_blocks))),
        );
        #[cfg(target_os = "linux")]
        memory::refresh_stale_emergency_pressure(coord_rss_budget_mb);
        let engine_will_run = self.config.utxo_engine;
        let ibd_path_reason = match std::env::var("BLVM_IBD_ENGINE") {
            Ok(v)
                if v.trim() == "0"
                    || v.eq_ignore_ascii_case("false")
                    || v.eq_ignore_ascii_case("no") =>
            {
                "BLVM_IBD_ENGINE=0"
            }
            Ok(v)
                if v.trim() == "1"
                    || v.eq_ignore_ascii_case("true")
                    || v.eq_ignore_ascii_case("yes") =>
            {
                "BLVM_IBD_ENGINE=1"
            }
            Ok(_) => "BLVM_IBD_ENGINE set (unrecognized value — using config default)",
            Err(_) if !engine_will_run => "[ibd].utxo_engine=false",
            Err(_) => "default (engine on)",
        };
        info!(
            "IBD UTXO path: {} ({})",
            if engine_will_run { "engine" } else { "legacy" },
            ibd_path_reason
        );
        if !engine_will_run {
            warn!(
                "IBD legacy UTXO path is deprecated; prefer engine (default). \
                 Set BLVM_IBD_ENGINE unset or remove utxo_engine=false to migrate."
            );
        }
        // IBD v2 (IbdUtxoStore) is the only path. Storage is guaranteed Some (checked at start).
        let ibd_memory_only: bool =
            (self.config.memory_only && start_height <= 1) || engine_will_run;
        // Keeps the standalone LMDB environment alive for the duration of IBD (dropped after
        // `ibd_store_v2` is no longer used). The `Heed3Tree`'s Arc<Env> would keep the env
        // alive on its own, but holding the Box<dyn Database> here makes the lifetime explicit
        // and ensures a clean shutdown flush sequence.
        let _ibd_utxo_standalone_db: Option<Box<dyn crate::storage::database::Database>>;
        let ibd_store_v2: Arc<IbdUtxoStore> = {
            // Open the IBD UTXO tree from a SEPARATE, fresh LMDB environment when possible.
            //
            // The main storage LMDB holds 400+ GB of block data. After `ibd_utxos.clear()` is
            // called during autorepair, millions of freed pages are scattered throughout this
            // file. LMDB reuses freed pages for new allocations, which requires reading each
            // old page from a random file offset (page fault, ~89% cache-miss rate on a large
            // file). At h=385k this causes 40+ seconds per 200k-op write transaction.
            //
            // A separate fresh LMDB at `<data_dir>/ibd_utxo_store/` allocates pages
            // sequentially from the end of a small file. With no free-list bloat, the same
            // 200k-op batch completes in ~1 second — a 40x improvement.
            //
            // SELECTION RULE (preserves backward compatibility):
            //   1. If standalone LMDB exists AND is non-empty → use standalone (resumed session
            //      that already wrote to the standalone store).
            //   2. If standalone is empty/absent AND main storage's `ibd_utxos` is non-empty →
            //      use main storage (legacy partial IBD or previous session without standalone).
            //   3. If both are empty (fresh start or post-repair) → use standalone (future writes
            //      go to the small, unfragmented file).
            //   4. If standalone creation fails → fall back to main storage.
            let tree: std::sync::Arc<dyn crate::storage::database::Tree> = {
                let maybe_data_dir = storage.data_dir();
                if engine_will_run {
                    _ibd_utxo_standalone_db = None;
                    info!(
                        "IBD engine mode: skipping standalone {}/ LMDB (no 128GiB mmap reservation)",
                        IBD_UTXO_STORE_SUBDIR
                    );
                    storage
                        .open_tree("ibd_utxos")
                        .context("Failed to open IBD UTXO tree (engine legacy shell)")?
                } else if let Some(ref root) = maybe_data_dir {
                    let utxo_store_dir = root.join(IBD_UTXO_STORE_SUBDIR);
                    match crate::storage::database::create_ibd_utxo_standalone_db(&utxo_store_dir) {
                        Ok(standalone_db) => {
                            let standalone_tree = standalone_db.open_tree("ibd_utxos");
                            let standalone_non_empty = standalone_tree
                                .as_ref()
                                .ok()
                                .and_then(|t| t.is_empty().ok())
                                .map(|empty| !empty)
                                .unwrap_or(false);

                            // Case 0: standalone has data from a prior session BUT we are
                            // replaying from genesis (start_height == 0 ← watermark was not
                            // durably written on the previous exit, so ibd_resume_heights reset
                            // synced_tip to 0). Reusing that stale data causes UTXO_TOTAL_MISS:
                            // old DEL tombstones shadow UTXOs that the fresh replay needs to
                            // look up, and UTXOs added before the prior session's last
                            // checkpoint exist in LMDB but with the wrong generation context.
                            // Fix: drop the env, wipe the directory, and open a clean instance.
                            if standalone_non_empty && start_height == 0 {
                                warn!(
                                    "[IBD_UTXO_STORE] standalone LMDB is non-empty but \
                                     start_height=0 (watermark not durably written on prior exit). \
                                     Wiping stale standalone data to prevent UTXO_TOTAL_MISS \
                                     during local replay."
                                );
                                drop(standalone_tree);
                                drop(standalone_db);
                                if let Err(e) = std::fs::remove_dir_all(&utxo_store_dir) {
                                    warn!(
                                        "[IBD_UTXO_STORE] failed to remove stale standalone dir \
                                         {}: {} — continuing with potentially stale data",
                                        utxo_store_dir.display(),
                                        e
                                    );
                                }
                                // Re-open fresh (falls through to Case 3 below).
                                match crate::storage::database::create_ibd_utxo_standalone_db(
                                    &utxo_store_dir,
                                ) {
                                    Ok(fresh_db) => {
                                        info!(
                                            "IBD UTXO store: using SEPARATE LMDB at {:?} \
                                             (fresh after stale-wipe — avoids free-list bloat)",
                                            utxo_store_dir
                                        );
                                        let fresh_tree = fresh_db
                                            .open_tree("ibd_utxos")
                                            .context("open ibd_utxos after stale-wipe")?;
                                        _ibd_utxo_standalone_db = Some(fresh_db);
                                        std::sync::Arc::from(fresh_tree)
                                    }
                                    Err(e) => {
                                        warn!(
                                            "create_ibd_utxo_standalone_db failed after \
                                             stale-wipe ({e}); falling back to main storage"
                                        );
                                        _ibd_utxo_standalone_db = None;
                                        storage.open_tree("ibd_utxos").context(
                                            "Failed to open IBD UTXO tree (stale-wipe fallback)",
                                        )?
                                    }
                                }
                            } else if standalone_non_empty {
                                // Case 1: standalone already has data from a prior session.
                                info!(
                                    "IBD UTXO store: resuming SEPARATE LMDB at {:?} \
                                     (contains existing UTXO data from prior session)",
                                    utxo_store_dir
                                );
                                _ibd_utxo_standalone_db = Some(standalone_db);
                                std::sync::Arc::from(standalone_tree.unwrap())
                            } else {
                                // Case 2 or 3: standalone is empty.
                                // Check if main storage has UTXO data (legacy partial IBD).
                                let main_tree =
                                    storage.open_tree("ibd_utxos").context("open ibd_utxos")?;
                                let main_non_empty = !main_tree.is_empty().unwrap_or(true);
                                if main_non_empty {
                                    // Case 2: legacy data in main storage — keep using it.
                                    // Migrating mid-IBD would require copying millions of entries.
                                    info!(
                                        "IBD UTXO store: using MAIN storage ibd_utxos \
                                         (contains existing UTXO data; standalone store is empty). \
                                         Next fresh IBD will use the standalone store."
                                    );
                                    _ibd_utxo_standalone_db = None;
                                    main_tree
                                } else {
                                    // Case 3: both empty → fresh start → use standalone.
                                    info!(
                                        "IBD UTXO store: using SEPARATE LMDB at {:?} \
                                         (fresh IBD start — avoids free-list bloat in main \
                                         400GB block store LMDB)",
                                        utxo_store_dir
                                    );
                                    _ibd_utxo_standalone_db = Some(standalone_db);
                                    std::sync::Arc::from(standalone_tree.unwrap())
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                "create_ibd_utxo_standalone_db failed ({e}); \
                                 falling back to main storage ibd_utxos"
                            );
                            _ibd_utxo_standalone_db = None;
                            storage
                                .open_tree("ibd_utxos")
                                .context("Failed to open IBD UTXO tree (fallback)")?
                        }
                    }
                } else {
                    // In-memory or non-file-backed backend — use the main storage.
                    _ibd_utxo_standalone_db = None;
                    storage
                        .open_tree("ibd_utxos")
                        .context("Failed to open IBD UTXO tree")?
                }
            };
            info!(
                "IBD v2: IbdUtxoStore (DashMap, zero lock, max_cache={} entries)",
                effective_max_entries
            );
            let eviction: crate::storage::ibd_utxo_store::EvictionStrategy = self
                .config
                .eviction
                .parse()
                .unwrap_or(crate::storage::ibd_utxo_store::EvictionStrategy::Fifo);
            let utxo_disk_baseline = storage
                .chain()
                .get_utxo_watermark()
                .ok()
                .flatten()
                .unwrap_or_else(|| start_height.saturating_sub(1));
            let store = Arc::new(IbdUtxoStore::new_with_options(
                tree,
                utxo_flush_threshold,
                ibd_memory_only,
                effective_max_entries,
                eviction,
                utxo_disk_baseline,
                storage.utxo_value_codec(),
            ));
            if start_height <= 1 && !engine_will_run {
                store.bootstrap_genesis(&protocol.get_network_params().genesis_block);
            }
            if ibd_memory_only && !engine_will_run {
                info!("IBD_MEMORY_ONLY=1: prefetch uses cache only (no disk reads during IBD)");
            } else if engine_will_run {
                info!("IBD engine mode: IbdUtxoStore shell inactive (cache-only placeholder)");
            }
            store
        };

        // Age-tiered UTXO engine (Phase 2). Opened here (before coordinator) so `coord_engine_mode`
        // reflects the actual runtime path — including resume-after-SIGKILL when we re-seed from
        // the durable `ibd_utxos` checkpoint.
        let utxo_engine: Option<Arc<crate::storage::ibd_engine::UtxoDatabase>> = if !engine_will_run
        {
            None
        } else {
            let engine_path = crate::config::ibd::ibd_engine_path(storage.data_dir().as_deref());
            if let Some(parent) = engine_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create IBD engine directory {}", parent.display()))?;
            }
            let stored_export = storage.chain().get_engine_export_height().ok().flatten();
            let export_override = crate::config::ibd::export_height_override_from_env();
            let (resolved_export, ignored_override) =
                crate::config::ibd::resolve_engine_export_height(stored_export, export_override);
            if let Some(oh) = ignored_override {
                warn!(
                    "IBD engine: ignoring BLVM_IBD_EXPORT_HEIGHT_OVERRIDE={oh} \
                     (< stored export_h={}) — seeding from durable checkpoint height",
                    stored_export.unwrap_or(0)
                );
            }
            let checkpoint_height = resolved_export
                .map(|h| h as i32)
                .unwrap_or_else(|| start_height.saturating_sub(1) as i32);
            let resume_seed = start_height > 1 && checkpoint_height > 0;

            let mut segs_os_pre = engine_path.as_os_str().to_owned();
            segs_os_pre.push(".segs");
            let segs_path_pre = std::path::PathBuf::from(&segs_os_pre);
            let segs_exist_pre = segs_path_pre.is_dir();
            let sidecar_cl_pre =
                crate::storage::ibd_engine::read_contiguous_length_sidecar(&engine_path);
            let segment_max_pre =
                crate::storage::ibd_engine::engine_segment_max_height(&engine_path);

            // Wipe stale on-disk engine state on epoch mismatch or missing segments.
            // Do not wipe on resume solely because `.dirty` exists (SIGTERM leaves it set).
            let dirty_path = crate::storage::ibd_engine::engine_dirty_flag_path(&engine_path);
            let was_dirty_at_open = dirty_path.exists();
            {
                let mut epoch_os = engine_path.as_os_str().to_owned();
                epoch_os.push(".epoch");
                let epoch_path = std::path::PathBuf::from(&epoch_os);
                let stored_epoch: Option<u64> = std::fs::read_to_string(&epoch_path)
                    .ok()
                    .and_then(|s| s.trim().parse().ok());
                let epoch_matches = stored_epoch == Some(start_height);
                let segments_recoverable = segs_exist_pre && segment_max_pre >= checkpoint_height;
                let needs_wipe = if resume_seed {
                    if !engine_path.exists() || !segs_exist_pre {
                        true
                    } else if !epoch_matches {
                        // start_height may advance between restarts (engine sidecar ahead of export_h).
                        !segments_recoverable
                    } else {
                        false
                    }
                } else {
                    // Genesis / export_h=0: never reopen leftover engine state. Live 2026-07-14:
                    // clean epoch=1 + sidecar CL=49716 restored while start_h=1 → export labeled
                    // h=40000 with only 5581 UTXOs → infinite UTXO-miss restart loop.
                    engine_path.exists()
                        || segs_exist_pre
                        || sidecar_cl_pre.is_some()
                        || was_dirty_at_open
                        || !epoch_matches
                };
                if needs_wipe {
                    info!(
                        "IBD engine: wiping stale engine files at {} (start_height={}, resume_seed={}, stored_epoch={:?}, was_dirty={}, segments_recoverable={})",
                        engine_path.display(),
                        start_height,
                        resume_seed,
                        stored_epoch,
                        was_dirty_at_open,
                        segments_recoverable,
                    );
                    let _ = std::fs::remove_file(&engine_path);
                    crate::storage::ibd_engine::remove_contiguous_length_sidecar(&engine_path);
                    let _ = std::fs::remove_dir_all(&segs_path_pre);
                    let _ = std::fs::remove_file(&epoch_path);
                }
                let _ = std::fs::write(&dirty_path, b"1" as &[u8]);
                let _ = std::fs::write(&epoch_path, format!("{start_height}\n"));
            }

            // Dirty exit loses in-memory age tiers. Disk segments / segment_max only hold
            // spilled overflow — not a complete UTXO set — so always wipe + re-seed from the
            // durable checkpoint (export_h). Clean exits clear .dirty after flush and may
            // skip reseed when contiguous_length >= export_h.
            let mut db = if resume_seed && was_dirty_at_open {
                info!(
                    "IBD engine: dirty shutdown — wipe + re-seed from checkpoint \
                     (export_h={}; prior segment_max={}; sidecar={:?})",
                    checkpoint_height, segment_max_pre, sidecar_cl_pre,
                );
                let _ = std::fs::remove_file(&engine_path);
                crate::storage::ibd_engine::remove_contiguous_length_sidecar(&engine_path);
                let _ = std::fs::remove_dir_all(&segs_path_pre);
                crate::storage::ibd_engine::UtxoDatabase::open_skip_segments(
                    &engine_path,
                    mem_guard.engine_avail_mb(),
                )
                .with_context(|| {
                    format!(
                        "open IBD engine at {} for dirty re-seed (set BLVM_IBD_ENGINE=0 for legacy path)",
                        engine_path.display()
                    )
                })?
            } else {
                crate::storage::ibd_engine::UtxoDatabase::open(
                    &engine_path,
                    mem_guard.engine_avail_mb(),
                )
                .with_context(|| {
                    format!(
                        "open IBD engine at {} (set BLVM_IBD_ENGINE=0 for legacy path)",
                        engine_path.display()
                    )
                })?
            };

            if resume_seed {
                let ckpt_slot = storage.chain().get_engine_ckpt_slot().unwrap_or(0);
                let ckpt_tree_name = crate::storage::ibd_engine::ckpt_tree_for_slot(ckpt_slot);
                let expected_count = storage
                    .chain()
                    .get_engine_export_utxo_count()
                    .ok()
                    .flatten();
                let last_accepted = storage
                    .chain()
                    .get_engine_prev_accepted_utxo_count()
                    .ok()
                    .flatten()
                    .unwrap_or(0);

                let cl = db.contiguous_length();
                // Ages live in RAM — never skip re-seed when cl > export_h (segments alone are
                // incomplete). Live: skip at cl=698719 caused Invalid inputs → autorepair.
                let skip_reseed = !was_dirty_at_open && cl > 0 && cl == checkpoint_height;
                if skip_reseed {
                    info!(
                        "IBD engine: skip re-seed — clean shutdown, on-disk contiguous_length={cl} == export_h={checkpoint_height}"
                    );
                } else if was_dirty_at_open || cl > checkpoint_height {
                    if cl > checkpoint_height {
                        info!(
                            "IBD engine: contiguous_length={cl} > export_h={checkpoint_height} — \
                             re-seeding from checkpoint (ages are RAM-only across restarts)"
                        );
                    }
                    // Wipe stale segments then seed (same as clean cl-mismatch path below).
                    info!(
                        "IBD engine: wiping engine data before re-seed (contiguous_length={cl}, was_dirty={was_dirty_at_open})"
                    );
                    let _ = std::fs::remove_file(&engine_path);
                    crate::storage::ibd_engine::remove_contiguous_length_sidecar(&engine_path);
                    let mut segs_os = engine_path.as_os_str().to_owned();
                    segs_os.push(".segs");
                    let _ = std::fs::remove_dir_all(std::path::PathBuf::from(segs_os));
                    drop(db);
                    db = crate::storage::ibd_engine::UtxoDatabase::open_skip_segments(
                        &engine_path,
                        mem_guard.engine_avail_mb(),
                    )
                    .with_context(|| {
                        format!(
                            "re-open IBD engine at {} after wipe for re-seed",
                            engine_path.display()
                        )
                    })?;
                    let Some((seed_name, ckpt_tree)) =
                        storage.open_engine_resume_snapshot().with_context(|| {
                            format!("open engine resume snapshot (slot {ckpt_tree_name})")
                        })?
                    else {
                        return Err(anyhow::anyhow!(
                            "IBD engine: resume at height {start_height} requires non-empty \
                             checkpoint in {ckpt_tree_name} or a stored canonical UTXO snapshot \
                             (last export missing or incomplete — replay from last good export or genesis)"
                        ));
                    };
                    // Slot export count describes the ping-pong ckpt. Phase 3's
                    // canonical `ibd_utxos` rewrite does not update that count.
                    let count_for_seed = if seed_name == ckpt_tree_name {
                        expected_count
                    } else {
                        None
                    };

                    let n = crate::storage::ibd_engine::seed_from_ibd_utxos(
                        &db,
                        ckpt_tree.as_ref(),
                        checkpoint_height,
                        count_for_seed,
                        last_accepted,
                        storage.utxo_value_codec(),
                    )
                    .with_context(|| {
                        format!("seed engine from {seed_name} at checkpoint h={checkpoint_height}")
                    })?;
                    let validation_tip_at_open =
                        storage.chain().get_engine_validation_tip().ok().flatten();
                    let chain_tip_at_open = storage.chain().get_height().ok().flatten();
                    info!(
                        "IBD engine: resume from height {start_height} — re-seeded {n} UTXOs \
                         from {seed_name} (slot {ckpt_slot})"
                    );
                    info!(
                        "[IBD_ENGINE_REPLAY] seed complete: export_h={checkpoint_height} start_h={start_height} \
                         utxos={n} validation_tip={validation_tip_at_open:?} chain_tip={chain_tip_at_open:?}"
                    );
                    crate::storage::ibd_utxo_muhash::backfill_engine_export_muhash_if_missing(
                        storage.as_ref(),
                    )
                    .with_context(|| {
                        format!(
                            "backfill export MuHash snapshot for checkpoint h={checkpoint_height}"
                        )
                    })?;
                    crate::storage::ibd_utxo_muhash::reset_engine_resume_muhash_baseline(
                        storage.chain(),
                        checkpoint_height as u64,
                    )?;
                } else {
                    if cl > 0 && cl < checkpoint_height {
                        warn!(
                            "IBD engine: on-disk contiguous_length={cl} < export_h={checkpoint_height} \
                             — re-seeding from {ckpt_tree_name}"
                        );
                    }
                    info!(
                        "IBD engine: wiping engine data before re-seed (contiguous_length={cl}, was_dirty={was_dirty_at_open})"
                    );
                    let _ = std::fs::remove_file(&engine_path);
                    crate::storage::ibd_engine::remove_contiguous_length_sidecar(&engine_path);
                    let mut segs_os = engine_path.as_os_str().to_owned();
                    segs_os.push(".segs");
                    let _ = std::fs::remove_dir_all(std::path::PathBuf::from(segs_os));
                    drop(db);
                    db = crate::storage::ibd_engine::UtxoDatabase::open_skip_segments(
                        &engine_path,
                        mem_guard.engine_avail_mb(),
                    )
                    .with_context(|| {
                        format!(
                            "re-open IBD engine at {} after wipe for re-seed",
                            engine_path.display()
                        )
                    })?;
                    let Some((seed_name, ckpt_tree)) =
                        storage.open_engine_resume_snapshot().with_context(|| {
                            format!("open engine resume snapshot (slot {ckpt_tree_name})")
                        })?
                    else {
                        return Err(anyhow::anyhow!(
                            "IBD engine: resume at height {start_height} requires non-empty \
                             checkpoint in {ckpt_tree_name} or a stored canonical UTXO snapshot \
                             (last export missing or incomplete — replay from last good export or genesis)"
                        ));
                    };
                    // Slot export count describes the ping-pong ckpt. Phase 3's
                    // canonical `ibd_utxos` rewrite does not update that count.
                    let count_for_seed = if seed_name == ckpt_tree_name {
                        expected_count
                    } else {
                        None
                    };

                    let n = crate::storage::ibd_engine::seed_from_ibd_utxos(
                        &db,
                        ckpt_tree.as_ref(),
                        checkpoint_height,
                        count_for_seed,
                        last_accepted,
                        storage.utxo_value_codec(),
                    )
                    .with_context(|| {
                        format!("seed engine from {seed_name} at checkpoint h={checkpoint_height}")
                    })?;
                    let validation_tip_at_open =
                        storage.chain().get_engine_validation_tip().ok().flatten();
                    let chain_tip_at_open = storage.chain().get_height().ok().flatten();
                    info!(
                        "IBD engine: resume from height {start_height} — re-seeded {n} UTXOs \
                         from {seed_name} (slot {ckpt_slot})"
                    );
                    info!(
                        "[IBD_ENGINE_REPLAY] seed complete: export_h={checkpoint_height} start_h={start_height} \
                         utxos={n} validation_tip={validation_tip_at_open:?} chain_tip={chain_tip_at_open:?}"
                    );
                    crate::storage::ibd_utxo_muhash::backfill_engine_export_muhash_if_missing(
                        storage.as_ref(),
                    )
                    .with_context(|| {
                        format!(
                            "backfill export MuHash snapshot for checkpoint h={checkpoint_height}"
                        )
                    })?;
                    crate::storage::ibd_utxo_muhash::reset_engine_resume_muhash_baseline(
                        storage.chain(),
                        checkpoint_height as u64,
                    )?;
                }
            } else {
                info!(
                    "IBD engine enabled; flat-file table at {}",
                    engine_path.display()
                );
            }
            Some(Arc::new(db))
        };

        let engine_durability = crate::config::ibd::ibd_engine_durability_config(None);
        if utxo_engine.is_some() {
            info!(
                "IBD engine durability: ckpt_interval={:?} min={} max={} target_secs={} \
                 muhash_persist_every={}",
                engine_durability.checkpoint_interval,
                engine_durability.checkpoint_min_interval,
                engine_durability.checkpoint_max_interval,
                engine_durability.checkpoint_target_secs,
                engine_durability.muhash_persist_interval,
            );
        }

        // Ready-queue: ALWAYS created. Validation ONLY receives from ready_rx — fully isolated.
        // Legacy: prefetch workers load UTXOs; engine: coordinator feeds the bridge directly.
        let engine_direct_feed = utxo_engine.is_some();
        let max_prefetches_in_flight: usize = {
            let config_val = self.config.prefetch_queue_size;
            let guard_limit = mem_guard.prefetch_queue_size;
            match config_val {
                Some(v) if v <= guard_limit => v,
                Some(v) => {
                    info!(
                        "prefetch_queue_size={} exceeds MemoryGuard limit {}; capping",
                        v, guard_limit
                    );
                    guard_limit
                }
                None => guard_limit,
            }
        };
        let prefetch_workers: usize = if engine_direct_feed {
            0
        } else {
            self.config.prefetch_workers.unwrap_or_else(|| {
                let n = std::thread::available_parallelism()
                    .map(|p| p.get())
                    .unwrap_or(8);
                (n.saturating_mul(2)).clamp(4, 24)
            })
        };
        let gap_fill_workers: usize = if engine_direct_feed {
            0
        } else {
            prefetch_workers
        };
        let (
            prefetch_input_tx_v2,
            gap_fill_tx_v2,
            ready_bridge,
            ready_rx,
            mut prefetch_join_handles,
        ) = {
            let (out_tx, out_rx) =
                crossbeam_channel::bounded::<ReadyItem>(max_prefetches_in_flight);
            let bridge = Arc::new(prefetch::OrderedReadyBridge::new(out_tx));
            if engine_direct_feed {
                info!("IBD engine: direct coordinator→feeder path (no prefetch worker threads)");
                let (in_tx, _in_rx) = crossbeam_channel::bounded::<PrefetchWorkItemV2>(1);
                let (gap_tx_v2, _gap_rx) = crossbeam_channel::bounded::<PrefetchWorkItemV2>(1);
                (in_tx, gap_tx_v2, bridge, out_rx, Vec::new())
            } else {
                let store = Arc::clone(&ibd_store_v2);
                let (in_tx, in_rx) =
                    crossbeam_channel::bounded::<PrefetchWorkItemV2>(max_prefetches_in_flight);
                let (gap_tx_v2, gap_rx_v2) =
                    crossbeam_channel::bounded::<PrefetchWorkItemV2>(gap_fill_workers * 4);
                let mut prefetch_join_handles: Vec<std::thread::JoinHandle<()>> =
                    Vec::with_capacity(prefetch_workers + gap_fill_workers);
                for _ in 0..prefetch_workers {
                    let rx_clone = in_rx.clone();
                    let bridge_clone = Arc::clone(&bridge);
                    let store = Arc::clone(&store);
                    prefetch_join_handles.push(std::thread::spawn(move || {
                        prefetch::run_prefetch_worker(rx_clone, bridge_clone, store)
                    }));
                }
                for _ in 0..gap_fill_workers {
                    let rx_clone = gap_rx_v2.clone();
                    let bridge_clone = Arc::clone(&bridge);
                    let store = Arc::clone(&store);
                    prefetch_join_handles.push(std::thread::spawn(move || {
                        prefetch::run_prefetch_worker(rx_clone, bridge_clone, store)
                    }));
                }
                info!(
                    "IBD v2 prefetch: {} workers, queue={}; gap-fill overflow: {} workers",
                    prefetch_workers, max_prefetches_in_flight, gap_fill_workers
                );
                (in_tx, gap_tx_v2, bridge, out_rx, prefetch_join_handles)
            }
        };

        info!(
            "IBD: {} peers, {} total chunks (sequential assignment)",
            filtered_peers.len(),
            assigner.total_chunks()
        );

        let mut download_handles = Vec::new();
        let num_peers = filtered_peers.len();
        let ibd_protocol_version = protocol.get_protocol_version();
        let confirmed_body_height_at_start = probe_confirmed_body_height(&blockstore)
            .unwrap_or_else(|e| {
                warn!("IBD: probe_confirmed_body_height failed: {e:#}");
                0
            });
        assigner.set_confirmed_body_height_at_start(confirmed_body_height_at_start);
        let sparse_local_body_max = if confirmed_body_height_at_start == 0 {
            probe_highest_stored_body_height(&blockstore).unwrap_or(0)
        } else {
            confirmed_body_height_at_start
        };
        if confirmed_body_height_at_start == 0 {
            if let Ok(Some(header_max)) = blockstore.highest_stored_height() {
                if header_max > 0 {
                    let sparse_max = probe_highest_stored_body_height(&blockstore).unwrap_or(0);
                    if sparse_max > 0 {
                        info!(
                            "IBD: sparse block bodies on disk up to height {} (header_max={}) \
                             — per-height write skip active; contiguous replay from genesis unavailable",
                            sparse_max, header_max
                        );
                    } else {
                        info!(
                            "IBD: height_index has {header_max} entries but no block bodies found \
                             — starting fresh (no local replay)"
                        );
                    }
                }
            }
        } else if let Ok(Some(header_max)) = blockstore.highest_stored_height() {
            if confirmed_body_height_at_start < header_max {
                info!(
                    "IBD: block bodies confirmed up to height {} (header_max={})",
                    confirmed_body_height_at_start, header_max
                );
            }
        }

        for peer_id in &filtered_peers {
            let priority = scored_peers
                .iter()
                .find(|(p, _)| p == peer_id)
                .map(|(_, s)| *s)
                .unwrap_or(1.0);
            // WAN multi-peer: 1 worker/peer by default; 2 when sticky dual-worker enabled
            // (see sticky_dual_worker_enabled) so dual-pipe can arm.
            // Sole-tip priority (Mode T / dual loopback): must match total_download_workers
            // above — tc167 logged workers=1 then spawned clamp(2,6)=6 (channel sized for 1).
            // Synth bulk local-disk: single worker — dual workers raced the same tip span
            // (complete→clear→W28c reassign) and amplified the H6 DEDUP storm.
            let worker_count = if wan_multi_peer {
                if sticky_dual_worker_enabled() { 2 } else { 1 }
            } else if sole_tip_priority_enabled() {
                // Match total_download_workers (tc167 tip90≈54.4 > tc169 spawn=1).
                ((2.0 * priority) as usize).clamp(2, 6)
            } else if synthetic_wan::bulk_local_disk_stream() {
                1
            } else {
                ((2.0 * priority) as usize).clamp(2, 6)
            };

            info!(
                "IBD: {} worker(s) for peer {} (priority: {:.2})",
                worker_count, peer_id, priority
            );

            for _worker_idx in 0..worker_count {
                let peer_id = peer_id.clone();
                let semaphore = self
                    .peer_semaphores
                    .get(&peer_id)
                    .map(|r| Arc::clone(&*r))
                    .ok_or_else(|| {
                        anyhow::anyhow!("Peer {} not found in peer_semaphores", peer_id)
                    })?;
                let ctx = IbdWorkerCtx {
                    peer_id: peer_id.clone(),
                    config: self.config.clone(),
                    blockstore: Arc::clone(&blockstore),
                    network: network.clone(),
                    tx: block_tx.clone(),
                    peer_scorer: Arc::clone(&self.peer_scorer),
                    assigner: Arc::clone(&assigner),
                    workers_current_chunks: Arc::clone(&workers_current_chunks),
                    num_peers,
                    peer_blocks_semaphores: Arc::clone(&self.peer_blocks_semaphores),
                    max_ahead_live: Arc::clone(&max_ahead_live),
                    ibd_pv: ibd_protocol_version,
                    stall_rx: stall_tx.subscribe(),
                    semaphore,
                    validation_height: Arc::clone(&validation_height),
                    confirmed_body_height: confirmed_body_height_at_start,
                    ibd_start_height: start_height,
                    wan_multi_peer,
                };
                let handle = tokio::spawn(async move {
                    if let Err(e) = run_ibd_download_worker(ctx).await {
                        warn!("IBD worker exited with error: {e:#}");
                    }
                });

                download_handles.push((0, handle));
            }
        }

        // Keep one extra block_tx clone alive for the peer watcher so the channel
        // stays open until replacement workers also finish.
        let block_tx_for_watcher = block_tx.clone();
        // Drop the original sender so only workers + watcher hold the channel alive.
        drop(block_tx);

        // Peer watcher: every 15 s, check whether the network manager has connected
        // new full-history peers (to replace evicted pruned ones) and spawn IBD
        // workers for them.  Holds `block_tx_for_watcher` so the download→coordinator
        // channel stays open until all replacement workers also finish.
        {
            let network_for_watcher = network.clone();
            let assigner_for_watcher = Arc::clone(&assigner);
            let config_for_watcher = self.config.clone();
            let blockstore_for_watcher = Arc::clone(&blockstore);
            let peer_scorer_for_watcher = Arc::clone(&self.peer_scorer);
            let workers_current_for_watcher = Arc::clone(&workers_current_chunks);
            let peer_blocks_sems_for_watcher = Arc::clone(&self.peer_blocks_semaphores);
            let peer_sems_for_watcher = Arc::clone(&self.peer_semaphores);
            let max_ahead_for_watcher = Arc::clone(&max_ahead_live);
            let validation_height_for_watcher = Arc::clone(&validation_height);
            let stall_tx_for_watcher = stall_tx.clone();
            let ibd_pv_for_watcher = ibd_protocol_version;
            let confirmed_body_height_for_watcher = confirmed_body_height_at_start;
            let wan_multi_peer_for_watcher = wan_multi_peer;
            let max_concurrent_for_watcher = self.config.max_concurrent_per_peer;
            let max_blocks_transit_for_watcher = self.config.max_blocks_in_transit_per_peer;
            let initial_peers_set: HashSet<String> = filtered_peers.iter().cloned().collect();
            let num_initial_peers = num_peers;

            if network_for_watcher.is_some() {
                tokio::spawn(async move {
                    let mut known_peers: HashSet<String> = initial_peers_set;
                    loop {
                        tokio::time::sleep(Duration::from_secs(8)).await;
                        if assigner_for_watcher.is_done() {
                            info!("[IBD] Peer watcher exiting: all chunks complete");
                            break;
                        }
                        let Some(ref net) = network_for_watcher else {
                            break;
                        };
                        let current_peers = net.peer_addresses_for_ibd();
                        let new_peers: Vec<String> = {
                            let evicted = net.ibd_evicted_ips.read().unwrap();
                            current_peers
                                .iter()
                                .map(|a| a.to_string())
                                .filter(|s| !known_peers.contains(s))
                                .filter(|s| {
                                    s.parse::<std::net::SocketAddr>()
                                        .map(|sa| !evicted.contains(&sa.ip()))
                                        .unwrap_or(false)
                                })
                                .collect()
                        };
                        for peer_str in new_peers {
                            info!(
                                "[IBD] Peer watcher: new peer {} — spawning replacement worker",
                                peer_str
                            );
                            known_peers.insert(peer_str.clone());
                            // Tip-owner / OPEN_STALL / mid-clear gate on assigner.workers.
                            // Spawning without register left ready>0 but ready_active_ok=0/0
                            // (wan10k-c4 freeze @438479: CHEESE + preferred=None forever).
                            assigner_for_watcher.register_download_worker(&peer_str);
                            let chunk_sem = Arc::new(Semaphore::new(max_concurrent_for_watcher));
                            let blocks_sem =
                                Arc::new(Semaphore::new(max_blocks_transit_for_watcher));
                            peer_sems_for_watcher.insert(peer_str.clone(), Arc::clone(&chunk_sem));
                            peer_blocks_sems_for_watcher
                                .insert(peer_str.clone(), Arc::clone(&blocks_sem));

                            let num_total = known_peers.len().max(num_initial_peers).max(2);
                            let ctx = IbdWorkerCtx {
                                peer_id: peer_str.clone(),
                                config: config_for_watcher.clone(),
                                blockstore: Arc::clone(&blockstore_for_watcher),
                                network: network_for_watcher.clone(),
                                tx: block_tx_for_watcher.clone(),
                                peer_scorer: Arc::clone(&peer_scorer_for_watcher),
                                assigner: Arc::clone(&assigner_for_watcher),
                                workers_current_chunks: Arc::clone(&workers_current_for_watcher),
                                num_peers: num_total,
                                peer_blocks_semaphores: Arc::clone(&peer_blocks_sems_for_watcher),
                                max_ahead_live: Arc::clone(&max_ahead_for_watcher),
                                ibd_pv: ibd_pv_for_watcher,
                                stall_rx: stall_tx_for_watcher.subscribe(),
                                semaphore: chunk_sem,
                                validation_height: Arc::clone(&validation_height_for_watcher),
                                confirmed_body_height: confirmed_body_height_for_watcher,
                                ibd_start_height: 0, // bootstrap is long past for replacement workers
                                wan_multi_peer: wan_multi_peer_for_watcher,
                            };
                            tokio::spawn(async move {
                                if let Err(e) = run_ibd_download_worker(ctx).await {
                                    warn!("IBD replacement worker error: {e:#}");
                                }
                            });
                        }
                    }
                    info!("[IBD] Peer watcher: exiting, releasing block_tx hold");
                    // block_tx_for_watcher dropped here → channel closes when all workers done
                });
            } else {
                // No network, no peer replacement possible. Drop the extra sender
                // so the channel closes purely from worker lifetimes.
                drop(block_tx_for_watcher);
            }
        }

        // COORDINATOR: Drains block_rx, sends to prefetch. When prefetch and gap-fill are full,
        // pushes to the feeder with an empty UTXO map so the coordinator never blocks on workers
        // (keeps block_rx + buffer draining; validation supplements UTXOs on-thread when needed).
        // Mark bootstrap complete only when we've DRAINED the bootstrap chunk — not when the worker
        // returns. Otherwise parallel workers get chunks 128+ and send blocks before we receive 100,
        // causing interleaving and a stall at 99. Coordinator knows we have 0..=bootstrap_end when
        // we drain that block. Bootstrap is always ≥128 blocks so 99 and 100 are in the same chunk.
        let bootstrap_end = if !chunks.is_empty() {
            chunks[0].end_height
        } else {
            u64::MAX
        };
        let assigner_for_coord = Arc::clone(&assigner);
        let validation_height_for_coord = Arc::clone(&validation_height);
        let effective_end_live_for_coord = Arc::clone(&effective_end_live);
        let follow_tip_peer_ids_for_coord = peer_ids.to_vec();
        let headers_timeout_secs_for_coord = self.config.headers_timeout_secs;
        let headers_max_failures_for_coord = self.config.headers_max_failures;
        // WAN multi-peer: scale reorder buffer so fast peers can't fill it while a slow peer is
        // stalled. Minimum: num_peers × chunk_size × 2 (absorbs one full chunk per peer ahead).
        // Capped at 4000 to bound peak RSS (~4GB at 1MB/block on large-RAM machines).
        let total_ram_mb = mem_guard.system_total_ram_mb(); // for coordinator RAM-cap formula below
        let coord_buffer_limit = {
            let base = mem_guard.buffer_limit(start_height);
            let mut limit = if wan_multi_peer {
                // Lower bound: num_peers × chunk_size × 2 so each peer has 2 chunks of
                // lookahead without triggering a false stall.  Upper bound: RAM-aware so
                // accumulated block bodies (≈2 MB each at h>700k) don't cause OOM.
                //
                // OOM formula: the coordinator buffer, prefetch_queue, and in-flight blocks
                // each hold ~2 MB per slot.  Total ≈ (coord_buffer + prefetch_queue + ahead) × 2 MB.
                // Limit the coordinator buffer so the total never exceeds ~5% of total RAM.
                let wan_min = (num_peers * self.config.chunk_size as usize * 2).min(4000);
                let ram_cap_blocks = (total_ram_mb / 40) // 5% of RAM in MB ÷ 2 MB per block
                    .clamp(64, 2000) as usize;
                base.max(wan_min).min(ram_cap_blocks)
            } else {
                base
            };
            // Sparse local gap replay: keep coordinator/feeder prefetch headroom so validation
            // does not outrun download and fall back to per-loop disk inject (observed 200k+
            // inject logs, feeder=0, in_place=0 at ~90 BPS vs ~1600 BPS with buffer=400).
            if utxo_engine.is_some() && sparse_local_body_max > start_height {
                limit = limit.max(400);
            }
            // R-342: the dispatch loop breaks and the recv path yields while
            // `reorder_buffer.len() >= limit`. Admission is bounded separately by the bulk
            // admit window (`wan_bulk_admit_window`, R-342 default = `WAN_BULK_AHEAD`), so
            // the reorder can legitimately hold `window + 1` heights. With admit 4096 and
            // the RAM clamp at 2000 the reorder filled to ~4090, dispatch broke every
            // iteration, and the only path left was the one-block-per-poll LOCAL_GAP emit:
            // R-342 200–250k 40.2 BPS at 20 ms poll (R-341 418.7). The limit must sit above
            // anything admission allows; the admit window is the memory governor.
            if wan_multi_peer {
                limit = limit.max(wan_bulk_admit_window() as usize + 512);
            }
            limit
        };
        // Cap OrderedReadyBridge pending (out-of-order Arc<Block> backlog). Without this,
        // engine-mode dispatch dumps every ahead height into the bridge and pending grows to
        // 3–4k (~6–8 GB) while validation waits on a single gap. Default: ≤512 or reorder cap.
        // Override: BLVM_IBD_BRIDGE_PENDING_MAX (≥32).
        let bridge_pending_max: usize = std::env::var("BLVM_IBD_BRIDGE_PENDING_MAX")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n >= 32)
            .unwrap_or_else(|| coord_buffer_limit.clamp(128, 512));
        if wan_multi_peer {
            info!(
                "Coordinator: WAN multi-peer reorder buffer={} blocks",
                coord_buffer_limit
            );
        }
        let gap_fill_tx_v2_for_coord = gap_fill_tx_v2.clone();
        let prefetch_input_tx_v2_for_coord = prefetch_input_tx_v2.clone();
        let ibd_store_v2_for_coord = Arc::clone(&ibd_store_v2);
        let stall_tx_for_coord = stall_tx.clone();
        // When engine mode is active, prefetch workers skip prefetch_build_utxo_map and
        // build_spec_adds (both results are discarded by the engine validation path), and the
        // coordinator skips input key extraction (only tx_ids needed for SpendSession::append).
        let coord_engine_mode: bool = utxo_engine.is_some();
        if coord_engine_mode || wan_multi_peer {
            info!(
                "Coordinator: OrderedReadyBridge pending_max={} (reorder_limit={})",
                bridge_pending_max, coord_buffer_limit
            );
        }
        // Seq-1: single peer (`num_peers == 1`) — blocks arrive in order; skip reorder_buffer.
        let sequential = num_peers == 1;
        if sequential {
            info!("Coordinator: sequential mode (single peer) — passthrough, no reorder buffer");
        }
        feeder_miss::log_startup(
            num_peers,
            sequential,
            local_block::gap_inject_lookahead_pub(),
        );
        // The OrderedReadyBridge enforces strict-ascending delivery to the feeder. Initialize its
        // `next_expected` to start_height so prefetch worker completions are emitted starting there.
        // Prefetch workers complete out of order; without this seeding the first completion would
        // set the cursor (potentially skipping ahead of `start_height`).
        ready_bridge.coordinator_will_send_height(start_height);
        let ready_bridge_for_coord = if coord_engine_mode {
            Some(Arc::clone(&ready_bridge))
        } else {
            None
        };
        if !coord_engine_mode {
            // Bridge is held alive by prefetch worker threads.
            drop(ready_bridge);
        }
        // Create feeder_state here (before coordinator spawn) so the coordinator can hold a reference.
        // The feeder thread is spawned later but the Arc is shared; creating it early is safe.
        let feeder_state = new_feeder_state();
        // W56b: bridge must not advance `next_expected` via ready-channel hop.
        if let Some(ref bridge) = ready_bridge_for_coord {
            bridge.attach_feeder(Arc::clone(&feeder_state));
        }
        let feeder_state_for_coord = Arc::clone(&feeder_state);
        // Extract before `self` is moved into the coordinator async block.
        let download_timeout_secs_for_coord = self.config.download_timeout_secs;
        let chunk_size_for_ahead = self.config.chunk_size; // used by stall OOM throttle
        // Clone the Arc before moving into the coordinator; the validation loop also needs it.
        let max_ahead_live_for_validation = Arc::clone(&max_ahead_live);
        let blockstore_for_coord = Arc::clone(&blockstore);
        let confirmed_body_height_for_coord = confirmed_body_height_at_start;
        // L1: live on-disk body tip for local-ahead clamp. Never promote sparse-max /
        // binary-search cheese (live: confirmed=185817 hid holes at 70679/70713 and
        // disarmed GetData). Derive from the store's actual contiguous body range,
        // not height 1 (height-key slice / 500k seed have no genesis bodies).
        // Mutable: flush-path tip copies the write-path atomic as bodies hit disk.
        let contiguous_local_tip = contiguous_body_range_tip(&blockstore).unwrap_or(0);
        let mut live_body_tip_for_coord = synthetic_wan::effective_wan_body_tip(wan_live_body_tip(
            confirmed_body_height_at_start,
            contiguous_local_tip,
        ));
        assigner.set_wan_body_tip(live_body_tip_for_coord);
        if live_body_tip_for_coord > 0 {
            if synthetic_wan::body_tip_override().is_some() {
                info!(
                    "[IBD_SYNTH_WAN] pinned wan_body_tip={} (confirmed={}, sparse={}) — \
                     tip-crawl active while bodies load from snapshot",
                    live_body_tip_for_coord, confirmed_body_height_at_start, sparse_local_body_max
                );
            } else {
                info!(
                    "[IBD_LOCAL_AHEAD] live body tip={} (confirmed={}, sparse={}, contiguous={}) — max_ahead capped at {} while next_needed ≤ tip",
                    live_body_tip_for_coord,
                    confirmed_body_height_at_start,
                    sparse_local_body_max,
                    contiguous_local_tip,
                    local_body_ahead_cap()
                );
            }
        }
        let ibd_pv_for_coord = ibd_protocol_version;
        let coord_rss_budget_mb_for_coord = coord_rss_budget_mb;
        let coord_session_id_for_task = coord_session_id;
        let network_for_coord = network.clone();
        // A6d: live PeerScorer → ChunkAssigner scores (set once at start froze tip owner at
        // header-sync latency; sticky score=1.000 never upgraded → ~6 blk/s vs ~14).
        let peer_scorer_for_coord = Arc::clone(&self.peer_scorer);
        let coord_handle = tokio::spawn(run_ibd_coordinator(IbdCoordinatorCtx {
            assigner_for_coord,
            block_rx,
            blockstore_for_coord,
            bootstrap_end,
            bridge_pending_max,
            chunk_size_for_ahead,
            confirmed_body_height_for_coord,
            coord_buffer_limit,
            coord_engine_mode,
            coord_session_id_for_task,
            download_timeout_secs_for_coord,
            effective_end_live_for_coord,
            feeder_state_for_coord,
            follow_tip_peer_ids_for_coord,
            gap_fill_tx_v2_for_coord,
            headers_max_failures_for_coord,
            headers_timeout_secs_for_coord,
            ibd_pv_for_coord,
            ibd_store_v2_for_coord,
            live_body_tip_for_coord,
            max_ahead_blocks,
            max_ahead_live,
            network_for_coord,
            peer_scorer_for_coord,
            prefetch_input_tx_v2_for_coord,
            ready_bridge_for_coord,
            sequential,
            stall_tx_for_coord,
            start_height,
            validation_height_for_coord,
            wan_multi_peer,
        }));

        finish_parallel_ibd_session(IbdSessionTail {
            actual_synced_height,
            assigner,
            block_sync_start,
            blockstore,
            confirmed_body_height_at_start,
            coord_handle,
            download_handles,
            effective_end_height,
            effective_end_live,
            engine_durability,
            event_publisher,
            feeder_state,
            gap_fill_tx_v2,
            ibd_store_v2,
            max_ahead_blocks,
            max_ahead_live_for_validation,
            mem_guard,
            prefetch_input_tx_v2,
            prefetch_join_handles,
            protocol,
            ready_rx,
            stall_tx,
            self_ibd: self,
            sparse_local_body_max,
            start_height,
            storage,
            utxo_engine,
            utxo_set,
            validation_height,
        })
        .await
    }

    /// Create chunks for parallel download (delegates to chunk_assigner).
    pub fn create_chunks(
        &self,
        start_height: u64,
        end_height: u64,
        peer_ids: &[String],
        scored_peers: Option<&[(String, f64)]>,
    ) -> Vec<BlockChunk> {
        create_chunks_impl(
            &self.config,
            start_height,
            end_height,
            peer_ids,
            scored_peers,
        )
    }

    /// Returns pre-computed tx_ids so the caller avoids redundant double-SHA256.
    /// network_time: cached at loop init, refreshed every 1000 blocks (avoids per-block SystemTime syscall).
    /// bip30_index: O(1) duplicate-coinbase check; when Some, updated during apply_transaction.
    /// When BIP54 is active and height is at a period boundary (N % 2016 in {0, 2015}), boundary
    /// timestamps are read from blockstore so timewarp checks can run; otherwise None.
    #[inline]
    pub(crate) fn validate_block_only<'a>(
        &self,
        blockstore: &BlockStore,
        _protocol: &BitcoinProtocolEngine,
        utxo_set: &mut UtxoSet,
        bip30_index: Option<&mut Bip30Index>,
        block: &Block,
        block_arc: Option<Arc<Block>>,
        witnesses: &[Vec<Witness>],
        witnesses_arc: Option<&std::sync::Arc<Vec<Vec<Witness>>>>,
        height: u64,
        recent_headers: Option<&[Arc<BlockHeader>]>,
        network_time: u64,
        precomputed_tx_ids: Option<&'a [Hash]>,
        best_header_chainwork: Option<blvm_consensus::pow::U256>,
        ibd_utxo_lookup: Option<&dyn blvm_consensus::utxo_overlay::UtxoLookup>,
        ibd_block_outputs: Option<
            std::sync::Arc<
                rustc_hash::FxHashMap<
                    blvm_consensus::OutPoint,
                    std::sync::Arc<blvm_consensus::UTXO>,
                >,
            >,
        >,
    ) -> Result<(
        std::borrow::Cow<'a, [Hash]>,
        Option<blvm_protocol::block::UtxoDelta>,
        blvm_consensus::reorganization::BlockUndoLog,
    )> {
        // BIP54 activation from version bits when miners signal (no fixed height required).
        // Merge candidates with `min` so an earlier period’s lock-in is not overwritten by a
        // later window’s larger computed activation height (see `version_bits` module docs).
        let candidate = recent_headers.and_then(|hdr| {
            if hdr.len() >= blvm_protocol::version_bits::LOCK_IN_PERIOD as usize {
                blvm_protocol::version_bits::activation_height_from_headers(
                    hdr,
                    height,
                    network_time,
                    &blvm_protocol::version_bits::bip54_deployment_for_network(
                        &self.config.network,
                    ),
                )
            } else {
                None
            }
        });
        let bip54_activation_override = {
            use std::sync::atomic::Ordering;
            // Lock-free monotonic merge: only update when we have a candidate, since merging with
            // `None` is a no-op. `fetch_min` against `u64::MAX` (None sentinel) gives exact
            // `min(prev, cand)` semantics matching `merge_bip54_activation_candidate`.
            if let Some(c) = candidate {
                self.bip54_activation_from_version_bits
                    .fetch_min(c, Ordering::AcqRel);
            }
            let cur = self
                .bip54_activation_from_version_bits
                .load(Ordering::Acquire);
            if cur == u64::MAX { None } else { Some(cur) }
        };

        let bip54_active = blvm_protocol::bip_validation::is_bip54_active_at(
            height,
            self.config.network,
            bip54_activation_override,
        );
        let bip54_boundary = if bip54_active {
            let rem = height % 2016;
            if rem == 0 || rem == 2015 {
                let ts_n_minus_1 = blockstore
                    .get_hash_by_height(height.saturating_sub(1))
                    .ok()
                    .flatten()
                    .and_then(|h| blockstore.get_header(&h).ok().flatten())
                    .map(|h| h.timestamp);
                let ts_n_minus_2015 = if height >= 2015 {
                    blockstore
                        .get_hash_by_height(height - 2015)
                        .ok()
                        .flatten()
                        .and_then(|h| blockstore.get_header(&h).ok().flatten())
                        .map(|h| h.timestamp)
                } else {
                    None
                };
                match (ts_n_minus_1, ts_n_minus_2015) {
                    (Some(a), Some(b)) => Some(blvm_protocol::types::Bip54BoundaryTimestamps {
                        timestamp_n_minus_1: a,
                        timestamp_n_minus_2015: b,
                    }),
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        };

        let mut context = blvm_protocol::block::BlockValidationContext::from_connect_block_ibd_args(
            recent_headers,
            network_time,
            self.config.network,
            bip54_activation_override,
            bip54_boundary,
        );
        context.ibd_block_outputs = ibd_block_outputs;
        context.sequence_prev_mtp = Some(blockstore.sequence_prev_mtp_lookup());
        context.difficulty_ancestor = Some(blockstore.difficulty_ancestor_lookup());
        let owned_utxo = if ibd_utxo_lookup.is_some() {
            UtxoSet::default()
        } else {
            std::mem::take(utxo_set)
        };
        let (result, new_utxo_set, tx_ids, utxo_delta, undo_log) =
            blvm_consensus::block::connect_block_ibd_with_undo(
                block,
                witnesses,
                owned_utxo,
                height,
                &context,
                bip30_index,
                precomputed_tx_ids,
                block_arc,
                witnesses_arc,
                best_header_chainwork,
                ibd_utxo_lookup,
            )?;

        *utxo_set = new_utxo_set;
        match result {
            ValidationResult::Valid => Ok((tx_ids, utxo_delta, undo_log)),
            ValidationResult::Invalid(reason) => Err(anyhow::anyhow!(
                "Block validation failed at height {}: {}",
                height,
                reason
            )),
        }
    }

    /// When block validation fails, dump block, witnesses, and UTXO set to disk so a test case can be built.
    /// Directory: $BLVM_IBD_DUMP_DIR/height_{height}/. No-op when the env is unset (no /tmp default).
    /// Files: block.bin, witnesses.bin, utxo_set.bin, info.txt (height, error reason).
    pub(crate) fn dump_failed_block(
        height: u64,
        block: &Block,
        witnesses: &[Vec<Witness>],
        utxo_set: &UtxoSet,
        err: &anyhow::Error,
    ) {
        let Ok(base) = std::env::var("BLVM_IBD_DUMP_DIR") else {
            return;
        };
        if base.trim().is_empty() {
            return;
        }
        let dir = std::path::Path::new(&base).join(format!("height_{height}"));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            error!("Failed to create dump dir {}: {}", dir.display(), e);
            return;
        }
        let block_path = dir.join("block.bin");
        let witnesses_path = dir.join("witnesses.bin");
        let utxo_path = dir.join("utxo_set.bin");
        let info_path = dir.join("info.txt");

        if let Ok(f) = std::fs::File::create(&block_path) {
            if let Err(e) = bincode::serialize_into(std::io::BufWriter::new(f), block) {
                error!(
                    "Failed to serialize block to {}: {}",
                    block_path.display(),
                    e
                );
            }
        } else {
            error!("Failed to create {}", block_path.display());
        }
        if let Ok(f) = std::fs::File::create(&witnesses_path) {
            if let Err(e) = bincode::serialize_into(std::io::BufWriter::new(f), witnesses) {
                error!(
                    "Failed to serialize witnesses to {}: {}",
                    witnesses_path.display(),
                    e
                );
            }
        } else {
            error!("Failed to create {}", witnesses_path.display());
        }
        if let Ok(f) = std::fs::File::create(&utxo_path) {
            let serializable: std::collections::HashMap<_, _> =
                utxo_set.iter().map(|(k, v)| (*k, (**v).clone())).collect();
            if let Err(e) = bincode::serialize_into(std::io::BufWriter::new(f), &serializable) {
                error!(
                    "Failed to serialize utxo_set to {}: {}",
                    utxo_path.display(),
                    e
                );
            }
        } else {
            error!("Failed to create {}", utxo_path.display());
        }
        let info = format!(
            "height={}\nerror={}\ntxs={}\ninputs={}\nutxo_len={}\n",
            height,
            err,
            block.transactions.len(),
            block
                .transactions
                .iter()
                .map(|tx| tx.inputs.len())
                .sum::<usize>(),
            utxo_set.len(),
        );
        if let Err(e) = std::fs::write(&info_path, info) {
            error!("Failed to write {}: {}", info_path.display(), e);
        }
        info!(
            "IBD_FAILURE_DUMP: Block {} validation failed. Test data written to: {} (block.bin, witnesses.bin, utxo_set.bin, info.txt). Run: ./scripts/ibd_failure_to_repro_test.sh {}",
            height,
            dir.display(),
            height
        );
    }

    /// Dump successful block + witnesses + pre-state UTXO at IBD milestones for snapshot tests.
    /// Triggered when BLVM_IBD_SNAPSHOT_DIR is set; dumps at 50k, 90k, 125k, 133k, 145k, 175k, 181k, 190k, 200k.
    /// Same format as dump_failed_block; info.txt has error=ok, pre_state=1.
    pub(crate) fn dump_ibd_snapshot(
        height: u64,
        block: &Block,
        witnesses: &[Vec<Witness>],
        utxo_set: &UtxoSet,
        base_dir: &str,
    ) {
        let dir = std::path::Path::new(base_dir).join(format!("height_{height}"));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            error!(
                "IBD_SNAPSHOT: Failed to create dir {}: {}",
                dir.display(),
                e
            );
            return;
        }
        let block_path = dir.join("block.bin");
        let witnesses_path = dir.join("witnesses.bin");
        let utxo_path = dir.join("utxo_set.bin");
        let info_path = dir.join("info.txt");

        let mut success = true;
        if let Ok(f) = std::fs::File::create(&block_path) {
            if let Err(e) = bincode::serialize_into(std::io::BufWriter::new(f), block) {
                error!(
                    "IBD_SNAPSHOT: Failed to serialize block to {}: {}",
                    block_path.display(),
                    e
                );
                success = false;
            }
        } else {
            error!("IBD_SNAPSHOT: Failed to create {}", block_path.display());
            success = false;
        }
        if let Ok(f) = std::fs::File::create(&witnesses_path) {
            if let Err(e) = bincode::serialize_into(std::io::BufWriter::new(f), witnesses) {
                error!(
                    "IBD_SNAPSHOT: Failed to serialize witnesses to {}: {}",
                    witnesses_path.display(),
                    e
                );
                success = false;
            }
        } else {
            error!(
                "IBD_SNAPSHOT: Failed to create {}",
                witnesses_path.display()
            );
            success = false;
        }
        if let Ok(f) = std::fs::File::create(&utxo_path) {
            let serializable: std::collections::HashMap<_, _> =
                utxo_set.iter().map(|(k, v)| (*k, (**v).clone())).collect();
            if let Err(e) = bincode::serialize_into(std::io::BufWriter::new(f), &serializable) {
                error!(
                    "IBD_SNAPSHOT: Failed to serialize utxo_set to {}: {}",
                    utxo_path.display(),
                    e
                );
                success = false;
            }
        } else {
            error!("IBD_SNAPSHOT: Failed to create {}", utxo_path.display());
            success = false;
        }
        let n_txs = block.transactions.len();
        let n_inputs: usize = block.transactions.iter().map(|tx| tx.inputs.len()).sum();
        let info = format!(
            "height={}\nerror=ok\ntxs={}\ninputs={}\nutxo_len={}\npre_state=1\nrerun=BLVM_IBD_SNAPSHOT_DIR={} cargo test -p blvm-consensus --test block_ibd_snapshot_tests -- --ignored\n",
            height,
            n_txs,
            n_inputs,
            utxo_set.len(),
            base_dir
        );
        if let Err(e) = std::fs::write(&info_path, info) {
            error!(
                "IBD_SNAPSHOT: Failed to write {}: {}",
                info_path.display(),
                e
            );
            success = false;
        }
        if success {
            info!(
                "IBD_SNAPSHOT: Block {} dumped to: {} (block.bin, witnesses.bin, utxo_set.bin, info.txt)",
                height,
                dir.display()
            );
        } else {
            warn!(
                "IBD_SNAPSHOT: Block {} dump incomplete or failed (see errors above)",
                height
            );
        }
    }

    /// Flush pending blocks to storage using batch writes
    ///
    /// This commits multiple blocks in a single database transaction,
    /// which is much faster than individual writes.
    pub(crate) fn flush_pending_blocks(
        &self,
        blockstore: &BlockStore,
        _storage: Option<&Arc<Storage>>,
        pending: &mut Vec<(
            Arc<Block>,
            Arc<Vec<Vec<Witness>>>,
            u64,
            blvm_consensus::reorganization::BlockUndoLog,
        )>,
    ) -> Result<()> {
        self.flush_pending_blocks_with_opts(
            blockstore,
            _storage,
            pending,
            IbdBlockFlushOpts::default(),
        )
    }

    pub(crate) fn flush_pending_blocks_with_opts(
        &self,
        blockstore: &BlockStore,
        _storage: Option<&Arc<Storage>>,
        pending: &mut Vec<(
            Arc<Block>,
            Arc<Vec<Vec<Witness>>>,
            u64,
            blvm_consensus::reorganization::BlockUndoLog,
        )>,
        opts: IbdBlockFlushOpts,
    ) -> Result<()> {
        let to_flush = std::mem::take(pending);
        Self::do_flush_to_storage(blockstore, _storage, to_flush, opts)
    }

    /// Core flush logic. Takes ownership of pending. Used by sync flush and async spawn.
    /// Blocks are Arc<Block>; we try_unwrap to get owned Block for serialization (sync has completed).
    pub(crate) fn do_flush_to_storage(
        blockstore: &BlockStore,
        _storage: Option<&Arc<Storage>>,
        pending: Vec<(
            Arc<Block>,
            Arc<Vec<Vec<Witness>>>,
            u64,
            blvm_consensus::reorganization::BlockUndoLog,
        )>,
        opts: IbdBlockFlushOpts,
    ) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }

        let count = pending.len();
        let start = std::time::Instant::now();

        // Unwrap Arcs to get owned Block (sync has completed; refcount should be 1 when validation
        // holds the only Arc after dequeue). Witness Arc is cloned only when try_unwrap fails.
        let mut pending: Vec<(
            Block,
            Arc<Vec<Vec<Witness>>>,
            u64,
            blvm_consensus::reorganization::BlockUndoLog,
        )> = pending
            .into_iter()
            .map(|(arc_block, w, h, undo)| {
                let block = Arc::try_unwrap(arc_block).unwrap_or_else(|a| (*a).clone());
                (block, w, h, undo)
            })
            .collect();

        let flush_max_height = pending.iter().map(|(_, _, h, _)| *h).max().unwrap_or(0);

        // Sort by height once so each chunk is already in LSM-friendly order and we
        // can treat chunk indices as the flush_order directly (no secondary sort needed).
        pending.sort_by_key(|(_, _, h, _)| *h);

        pending = pending
            .into_iter()
            .map(|(block, witnesses, height, undo)| {
                let (block, witnesses) = crate::module::pipeline::try_filter_block_before_store(
                    height, block, witnesses,
                );
                (block, witnesses, height, undo)
            })
            .collect();

        // Returns true only if there is actual witness data (non-empty stack items).
        // An all-empty Vec<Vec<Witness>> (pre-SegWit blocks) does NOT count as having witnesses
        // and should not be stored, to avoid blocking re-download of SegWit blocks later.
        let block_has_witness_data = |w: &[Vec<Witness>]| {
            w.iter()
                .any(|tx_w| tx_w.iter().any(|stack| !stack.is_empty()))
        };

        let n = pending.len();
        // Serialise and flush in fixed-size chunks to bound peak RSS.
        // At h=700k each block averages ~1.5 MB serialised; flushing 300 blocks at once
        // previously allocated ~450 MB for `block_data` and then duplicated those bytes
        // inside the RocksDB WriteBatch (~900 MB transient spike). A chunk of 50 keeps
        // the spike at ~75 MB + ~75 MB = ~150 MB per iteration.
        const FLUSH_BLOCK_CHUNK_SIZE: usize = 50;

        #[cfg(feature = "rayon")]
        let parallel_serialize = opts.parallel_serialize;
        #[cfg(not(feature = "rayon"))]
        let parallel_serialize = false;

        if opts.log_progress && n > 0 {
            info!(
                "IBD block flush: storing {} blocks (heights {}-{})",
                n,
                pending.first().map(|(_, _, h, _)| *h).unwrap_or(0),
                flush_max_height,
            );
        }

        // Saved during the last chunk iteration for the update_tip call below.
        let mut tip_hash_for_update: Option<Hash> = None;

        for chunk_start in (0..n).step_by(FLUSH_BLOCK_CHUNK_SIZE) {
            let chunk_end = (chunk_start + FLUSH_BLOCK_CHUNK_SIZE).min(n);
            let is_last_chunk = chunk_end == n;
            let chunk = &pending[chunk_start..chunk_end];
            let chunk_t0 = std::time::Instant::now();
            if opts.log_progress {
                let lo = chunk.first().map(|(_, _, h, _)| *h).unwrap_or(0);
                let hi = chunk.last().map(|(_, _, h, _)| *h).unwrap_or(lo);
                info!(
                    "IBD block flush: chunk {}-{}/{} (heights {}-{})",
                    chunk_start + 1,
                    chunk_end,
                    n,
                    lo,
                    hi,
                );
            }

            // ── Serialise this chunk ──────────────────────────────────────────────
            // header_data uses Arc to avoid cloning Vec on cache hit.
            let (block_hashes, block_data, header_data): (
                Vec<Hash>,
                Vec<Vec<u8>>,
                Vec<Arc<Vec<u8>>>,
            ) = {
                let _ibd_header_cache_bypass =
                    crate::storage::serialization_cache::IbdHeaderSerializeCacheBypassGuard::enter(
                    );
                use crate::storage::serialization_cache::{
                    cache_serialized_header, get_cached_serialized_header,
                };
                if parallel_serialize {
                    #[cfg(feature = "rayon")]
                    {
                        use blvm_protocol::rayon::iter::IntoParallelRefIterator;
                        use blvm_protocol::rayon::prelude::*;
                        let block_hashes: Vec<Hash> = chunk
                            .par_iter()
                            .map(|(block, _, _, _)| blockstore.get_block_hash(block))
                            .collect();

                        let block_data: Vec<Vec<u8>> = chunk
                            .par_iter()
                            .map(|(block, _, _, _)| {
                                bincode::serialize(block)
                                    .map_err(|e| anyhow::anyhow!("Block serialization failed: {e}"))
                            })
                            .collect::<Result<Vec<_>>>()?;

                        let header_data: Vec<Arc<Vec<u8>>> = chunk
                            .par_iter()
                            .zip(block_hashes.par_iter())
                            .map(|((block, _, _, _), block_hash)| {
                                if let Some(cached) = get_cached_serialized_header(block_hash) {
                                    return Ok(cached);
                                }
                                let serialized =
                                    bincode::serialize(&block.header).map_err(|e| {
                                        anyhow::anyhow!("Header serialization failed: {e}")
                                    })?;
                                cache_serialized_header(*block_hash, serialized.clone());
                                Ok(Arc::new(serialized))
                            })
                            .collect::<Result<Vec<_>>>()?;

                        (block_hashes, block_data, header_data)
                    }
                    #[cfg(not(feature = "rayon"))]
                    {
                        unreachable!("parallel_serialize requires rayon feature")
                    }
                } else {
                    let block_hashes: Vec<Hash> = chunk
                        .iter()
                        .map(|(block, _, _, _)| blockstore.get_block_hash(block))
                        .collect();

                    let block_data: Vec<Vec<u8>> = chunk
                        .iter()
                        .map(|(block, _, _, _)| {
                            bincode::serialize(block)
                                .map_err(|e| anyhow::anyhow!("Block serialization failed: {e}"))
                        })
                        .collect::<Result<Vec<_>>>()?;

                    let header_data: Vec<Arc<Vec<u8>>> = chunk
                        .iter()
                        .zip(block_hashes.iter())
                        .map(|((block, _, _, _), block_hash)| {
                            if let Some(cached) = get_cached_serialized_header(block_hash) {
                                return Ok(cached);
                            }
                            let serialized = bincode::serialize(&block.header)
                                .map_err(|e| anyhow::anyhow!("Header serialization failed: {e}"))?;
                            cache_serialized_header(*block_hash, serialized.clone());
                            Ok(Arc::new(serialized))
                        })
                        .collect::<Result<Vec<_>>>()?;

                    (block_hashes, block_data, header_data)
                }
            };

            let witness_blobs: Vec<Option<Vec<u8>>> =
                if chunk.iter().any(|(_, w, _, _)| block_has_witness_data(w)) {
                    if parallel_serialize {
                        #[cfg(feature = "rayon")]
                        {
                            use blvm_protocol::rayon::iter::IntoParallelRefIterator;
                            use blvm_protocol::rayon::prelude::*;
                            let witness_data_vec: Vec<(usize, Vec<u8>)> = chunk
                                .par_iter()
                                .enumerate()
                                .filter_map(|(i, (_, witnesses, _, _))| {
                                    if block_has_witness_data(witnesses) {
                                        match bincode::serialize(witnesses.as_ref()) {
                                            Ok(data) => Some(Ok((i, data))),
                                            Err(e) => Some(Err(anyhow::anyhow!(
                                            "Failed to serialize witnesses at chunk index {i}: {e}"
                                        ))),
                                        }
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Result<Vec<_>>>()?;

                            let mut v = vec![None; chunk.len()];
                            for (i, data) in witness_data_vec {
                                v[i] = Some(data);
                            }
                            v
                        }
                        #[cfg(not(feature = "rayon"))]
                        {
                            unreachable!("parallel_serialize requires rayon feature")
                        }
                    } else {
                        let mut v = vec![None; chunk.len()];
                        for i in 0..chunk.len() {
                            let witnesses = &chunk[i].1;
                            if block_has_witness_data(witnesses) {
                                v[i] =
                                    Some(bincode::serialize(witnesses.as_ref()).map_err(|e| {
                                        anyhow::anyhow!("Failed to serialize witnesses: {}", e)
                                    })?);
                            }
                        }
                        v
                    }
                } else {
                    vec![None; chunk.len()]
                };

            for i in 0..chunk.len() {
                let (_, witnesses, height, _) = &chunk[i];
                if block_has_witness_data(witnesses) && witness_blobs[i].is_none() {
                    return Err(anyhow::anyhow!(
                        "IBD flush: witness data present in memory but not serialized for height {height}"
                    ));
                }
            }

            let metadata_blobs: Vec<Vec<u8>> = (0..chunk.len())
                .map(|i| {
                    let metadata = BlockMetadata {
                        n_tx: chunk[i].0.transactions.len() as u32,
                    };
                    bincode::serialize(&metadata)
                        .map_err(|e| anyhow::anyhow!("Block metadata serialization failed: {}", e))
                })
                .collect::<Result<Vec<_>>>()?;

            // flush_order is 0..chunk.len() — pending was sorted by height above, so each
            // chunk is already in ascending height order; no secondary sort needed.
            let flush_order: Vec<usize> = (0..chunk.len()).collect();
            let chunk_heights: Vec<u64> = chunk.iter().map(|(_, _, h, _)| *h).collect();

            // RECENT_HEADERS_TABLE is a sliding window of the last ~11 blocks. Only the
            // final chunk's entries matter for the end state; passing empty for earlier
            // chunks produces the same result without touching the table N-1 extra times.
            #[cfg(any(feature = "rocksdb", feature = "redb", feature = "tidesdb"))]
            let recent_entries: Vec<(u64, Vec<u8>)> = if is_last_chunk {
                flush_order
                    .iter()
                    .rev()
                    .take(11)
                    .map(|&idx| {
                        let h = chunk[idx].2;
                        let data = header_data[idx].as_slice().to_vec();
                        Ok((h, data))
                    })
                    .collect::<Result<Vec<_>>>()?
            } else {
                vec![]
            };

            // Save the tip block hash from the last block in the sorted last chunk.
            if is_last_chunk {
                tip_hash_for_update = block_hashes.last().copied();
            }

            // ── Write this chunk ──────────────────────────────────────────────────
            let mut storage_unified = false;
            #[cfg(feature = "rocksdb")]
            {
                if blockstore.try_ibd_flush_rocksdb_unified(
                    &flush_order,
                    &chunk_heights,
                    &block_hashes,
                    &block_data,
                    &header_data,
                    &witness_blobs,
                    &metadata_blobs,
                    &recent_entries,
                )? {
                    storage_unified = true;
                }
            }
            #[cfg(feature = "redb")]
            {
                if !storage_unified
                    && blockstore.try_ibd_flush_redb_unified(
                        &flush_order,
                        &chunk_heights,
                        &block_hashes,
                        &block_data,
                        &header_data,
                        &witness_blobs,
                        &metadata_blobs,
                        &recent_entries,
                    )?
                {
                    storage_unified = true;
                }
            }
            #[cfg(feature = "tidesdb")]
            {
                if !storage_unified
                    && blockstore.try_ibd_flush_tidesdb_unified(
                        &flush_order,
                        &chunk_heights,
                        &block_hashes,
                        &block_data,
                        &header_data,
                        &witness_blobs,
                        &metadata_blobs,
                        &recent_entries,
                    )?
                {
                    storage_unified = true;
                }
            }
            #[cfg(feature = "heed3")]
            {
                if !storage_unified
                    && blockstore.try_ibd_flush_heed3_unified(
                        &flush_order,
                        &chunk_heights,
                        &block_hashes,
                        &block_data,
                        &header_data,
                        &witness_blobs,
                        &metadata_blobs,
                        &recent_entries,
                    )?
                {
                    storage_unified = true;
                }
            }

            if !storage_unified {
                // Per-tree batches (Redb, Sled, TidesDB, or non-Rocks `Arc<dyn Database>`).
                {
                    let blocks_tree = blockstore.blocks_tree()?;
                    let mut batch = blocks_tree.batch()?;
                    for &i in &flush_order {
                        let height = chunk[i].2;
                        let key = block_height_row_key(height, &block_hashes[i]);
                        batch.put(&key, &block_data[i]);
                    }
                    batch.commit_no_wal()?;
                }
                {
                    let headers_tree = blockstore.headers_tree()?;
                    let mut batch = headers_tree.batch()?;
                    for &i in &flush_order {
                        let height = chunk[i].2;
                        let key = block_height_row_key(height, &block_hashes[i]);
                        batch.put(&key, header_data[i].as_slice());
                    }
                    batch.commit_no_wal()?;
                }
                {
                    let has_witnesses = witness_blobs.iter().any(|b| b.is_some());
                    if has_witnesses {
                        let witnesses_tree = blockstore.witnesses_tree()?;
                        let mut batch = witnesses_tree.batch()?;
                        for &i in &flush_order {
                            if let Some(ref data) = witness_blobs[i] {
                                let height = chunk[i].2;
                                let key = block_height_row_key(height, &block_hashes[i]);
                                batch.put(&key, data);
                            }
                        }
                        batch.commit_no_wal()?;
                    }
                }
                {
                    let height_tree = blockstore.height_tree()?;
                    let mut batch = height_tree.batch()?;
                    for &i in &flush_order {
                        let height = chunk[i].2;
                        let height_key = height.to_be_bytes();
                        batch.put(&height_key, &block_hashes[i]);
                    }
                    batch.commit_no_wal()?;
                }
                {
                    let ht_tree = blockstore.hash_to_height_tree()?;
                    let mut batch = ht_tree.batch()?;
                    for &i in &flush_order {
                        let height_bytes = chunk[i].2.to_be_bytes();
                        batch.put(&block_hashes[i], &height_bytes);
                    }
                    batch.commit_no_wal()?;
                }
                {
                    let meta_tree = blockstore.metadata_tree()?;
                    let mut batch = meta_tree.batch()?;
                    for &i in &flush_order {
                        let key = block_height_row_key(chunk[i].2, &block_hashes[i]);
                        batch.put(&key, &metadata_blobs[i]);
                    }
                    batch.commit_no_wal()?;
                }
                // Recent headers only needed for the last chunk (same rationale as above).
                if is_last_chunk {
                    let recent_batch: Vec<(u64, &BlockHeader)> = chunk
                        .iter()
                        .rev()
                        .take(11)
                        .map(|(block, _, height, _)| (*height, &block.header))
                        .collect();
                    blockstore.store_recent_headers_ibd_batch(&recent_batch)?;
                }
            }

            if let Some(storage) = _storage {
                blocks::index_ibd_flushed_blocks(storage, &block_hashes, chunk, &chunk_heights)?;
            }

            // Batch all undo logs for this chunk into a single write transaction.
            // The old per-block loop called Tree::insert once per block, each of which opened
            // its own LMDB write transaction and called fdatasync on commit — 50 fsyncs per
            // 50-block chunk. Over 8 chunks × 2 serial flush threads that is ~800 fsyncs per
            // IBD flush cycle, causing 30–90 s stalls on single-writer LMDB backends.
            #[cfg(feature = "production")]
            {
                let undo_entries: Vec<(
                    &blvm_protocol::types::Hash,
                    &blvm_consensus::reorganization::BlockUndoLog,
                )> = (0..block_hashes.len())
                    .map(|i| (&block_hashes[i], &chunk[i].3))
                    .collect();
                blockstore.store_undo_logs_batch(&undo_entries)?;
            }
            #[cfg(not(feature = "production"))]
            for i in 0..block_hashes.len() {
                blockstore.store_undo_log(&block_hashes[i], &chunk[i].3)?;
            }

            if opts.log_progress {
                info!(
                    "IBD block flush: chunk {}-{}/{} done in {:?}",
                    chunk_start + 1,
                    chunk_end,
                    n,
                    chunk_t0.elapsed(),
                );
            }

            // Durability only. Do not publish wan_body_tip here (flush-path /
            // arrival / feeder-take experiments excluded from this binary).

            // block_data, header_data, witness_blobs, metadata_blobs are dropped here,
            // releasing the serialised bytes before the next chunk is allocated.
        }

        #[cfg(feature = "profile")]
        {
            blvm_protocol::profile_log!(
                "[FLUSH_STORAGE_PERF] blocks={} max_height={} total_ms={}",
                count,
                flush_max_height,
                start.elapsed().as_millis()
            );
        }

        // Chain metadata: parallel IBD bypasses `run_loop`, so `update_tip` must run here
        // or `get_height()` / restarts see `chain_info` missing despite full block index.
        // pending is sorted by height; last entry is the tip.
        if let Some(storage) = _storage {
            if let Some(tip_hash) = tip_hash_for_update {
                if let Some((block, _, tip_height, _)) = pending.last() {
                    storage
                        .chain()
                        .update_tip(&tip_hash, &block.header, *tip_height)?;
                }
            }
            // Force durable commit under MDB_NOSYNC so chain_tip / height index survive
            // SIGTERM before the next watermark export can race ahead of block bodies.
            storage.flush()?;
        }

        let elapsed = start.elapsed();
        // Use debug! — this is disk write throughput for one batch, NOT IBD blocks/s.
        // Users often confuse 80k blocks/sec here with actual IBD rate (~100–5k BPS).
        debug!(
            "Batch stored {} blocks in {:?} ({:.0} blocks/sec)",
            count,
            elapsed,
            count as f64 / elapsed.as_secs_f64()
        );

        Ok(())
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
