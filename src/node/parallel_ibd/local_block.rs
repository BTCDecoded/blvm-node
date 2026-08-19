//! Local block load for IBD replay and coordinator gap fill.
//!
//! Separated from [`super::download`] so diagnostics and coordinator injection share one path.

use super::latch_env;
use super::types::{SharedBlock, SharedWitnesses};
use crate::storage::blockstore::BlockStore;
use anyhow::Result;
use blvm_protocol::features::FeatureRegistry;
use blvm_protocol::types::ARC_BLOCK_CREATED;
use blvm_protocol::{Block, Hash, ProtocolVersion, segwit::Witness};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, OnceLock};
use tracing::{debug, info, warn};

/// Process-latched [`FeatureRegistry`] — `for_protocol` rebuilds `Vec`+`String` feature
/// names on every call. Tip crawl hits persist/load/serve per body; cache by protocol.
pub fn cached_feature_registry(protocol_version: ProtocolVersion) -> &'static FeatureRegistry {
    static MAINNET: OnceLock<FeatureRegistry> = OnceLock::new();
    static TESTNET3: OnceLock<FeatureRegistry> = OnceLock::new();
    static REGTEST: OnceLock<FeatureRegistry> = OnceLock::new();
    static SIGNET: OnceLock<FeatureRegistry> = OnceLock::new();
    match protocol_version {
        ProtocolVersion::BitcoinV1 => {
            MAINNET.get_or_init(|| FeatureRegistry::for_protocol(ProtocolVersion::BitcoinV1))
        }
        ProtocolVersion::Testnet3 => {
            TESTNET3.get_or_init(|| FeatureRegistry::for_protocol(ProtocolVersion::Testnet3))
        }
        ProtocolVersion::Regtest => {
            REGTEST.get_or_init(|| FeatureRegistry::for_protocol(ProtocolVersion::Regtest))
        }
        ProtocolVersion::Signet => {
            SIGNET.get_or_init(|| FeatureRegistry::for_protocol(ProtocolVersion::Signet))
        }
    }
}

/// Why a height with a known header could not be loaded from the local blockstore for IBD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalBlockMiss {
    NotInStore,
    HashMismatch { computed: Hash },
    WitnessMissing,
    WitnessEmptyStale,
    HeightHashUnavailable,
}

impl fmt::Display for LocalBlockMiss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInStore => write!(f, "block body not in store"),
            Self::HashMismatch { computed } => {
                write!(
                    f,
                    "block hash mismatch (computed {})",
                    hex::encode(computed)
                )
            }
            Self::WitnessMissing => write!(f, "witness missing for segwit height"),
            Self::WitnessEmptyStale => write!(f, "witness blob all-empty (stale MSG_BLOCK fetch)"),
            Self::HeightHashUnavailable => write!(f, "height→hash index missing"),
        }
    }
}

/// Highest height with a stored block body (binary search over height index).
pub fn probe_confirmed_body_height(blockstore: &BlockStore) -> Result<u64> {
    let header_max = blockstore.highest_stored_height()?.unwrap_or(0);
    if header_max == 0 {
        return Ok(0);
    }
    let has_body = |h: u64| -> Result<bool> {
        if h == 0 {
            return Ok(false);
        }
        match blockstore.get_hash_by_height(h)? {
            Some(hash) => Ok(blockstore.get_block(&hash)?.is_some()),
            None => Ok(false),
        }
    };
    if !has_body(1)? {
        return Ok(0);
    }
    let mut lo = 1u64;
    let mut hi = header_max;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if has_body(mid)? {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    Ok(lo)
}

/// Peer body warehouse: advance live `wan_body_tip` as GAP_PERSIST extends contiguous on-disk
/// bodies past the start-of-run tip. Default **off** (`BLVM_IBD_BODY_WAREHOUSE=1` to enable).
///
/// Without this, `live_body_tip` is frozen at coordinator spawn — past-body stays tip-serial
/// GetData even after peers have persisted tip+1..N for LOCAL_GAP inject (A0 2026-08-02:
/// inject under WAN DNA hits dens-class wall ~192).
pub fn body_warehouse_enabled() -> bool {
    latch_env!(bool, {
        matches!(
            std::env::var("BLVM_IBD_BODY_WAREHOUSE").as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE")
        )
    })
}

/// Extend contiguous on-disk body tip from `from` forward (at most `max_steps` heights).
///
/// Used by body warehouse refresh: walk `from+1, from+2, …` while bodies exist. Does **not**
/// jump sparse holes (unlike [`probe_highest_stored_body_height`]).
pub fn extend_contiguous_body_tip(
    blockstore: &BlockStore,
    from: u64,
    max_steps: u64,
) -> Result<u64> {
    if from == 0 || max_steps == 0 {
        return Ok(from);
    }
    let mut tip = from;
    let limit = max_steps.min(1024);
    for _ in 0..limit {
        let next = tip.saturating_add(1);
        let Some(hash) = blockstore.get_hash_by_height(next)? else {
            break;
        };
        if !blockstore.has_block_body(&hash)? {
            break;
        }
        tip = next;
    }
    Ok(tip)
}

/// Highest height that has a stored block body. Unlike [`probe_confirmed_body_height`], does **not**
/// require a body at height 1 — sparse indexes (resume at h=260k with bodies only above the
/// checkpoint) still return the true max.
///
/// Scans backward from the height index tip: binary search fails when headers exist without bodies
/// below the highest stored body (non-monotonic `has_body`).
pub fn probe_highest_stored_body_height(blockstore: &BlockStore) -> Result<u64> {
    let header_max = blockstore.highest_stored_height()?.unwrap_or(0);
    if header_max == 0 {
        return Ok(0);
    }
    for h in (1..=header_max).rev() {
        let Some(hash) = blockstore.get_hash_by_height(h)? else {
            continue;
        };
        if blockstore.has_block_body(&hash)? {
            return Ok(h);
        }
    }
    Ok(0)
}

/// Skip heed3 block-store flush when the body is already on disk (cheap `contains_key` — no
/// deserialize). Used during sparse local gap replay when [`probe_confirmed_body_height`] is 0.
pub fn should_skip_block_store_write(
    blockstore: &BlockStore,
    height: u64,
    block_hash: &Hash,
    local_replay_max_height: u64,
) -> Result<bool> {
    if height > 0 && height <= local_replay_max_height {
        return Ok(true);
    }
    blockstore.has_block_body(block_hash)
}

/// True when at least one witness stack item is non-empty.
pub fn has_real_witnesses(w: &[Vec<Witness>]) -> bool {
    w.iter().any(|tx_w| tx_w.iter().any(|s| !s.is_empty()))
}

/// BIP141 witness commitment in coinbase (`OP_RETURN 0x24 aa21a9ed …`).
///
/// Blocks without this commitment may legitimately have empty witness stacks (no segwit
/// spends). W7 must not treat those as stripped `MSG_BLOCK` — live h=640022 is a 1-tx
/// coinbase-only block with no commitment; rejecting it pinned IBD forever.
pub fn coinbase_has_witness_commitment(block: &Block) -> bool {
    const MAGIC: [u8; 4] = [0xaa, 0x21, 0xa9, 0xed];
    let Some(coinbase) = block.transactions.first() else {
        return false;
    };
    coinbase.outputs.iter().any(|o| {
        let s = o.script_pubkey.as_slice();
        s.len() >= 38 && s[0] == 0x6a && s[1] == 0x24 && s[2..6] == MAGIC
    })
}

/// Empty witnesses are only unacceptable when segwit is active **and** the coinbase
/// commits to a witness merkle root (so a real `MSG_WITNESS_BLOCK` must carry data).
pub fn empty_witness_unacceptable(
    block: &Block,
    witnesses: &[Vec<Witness>],
    segwit_on: bool,
) -> bool {
    segwit_on && !has_real_witnesses(witnesses) && coinbase_has_witness_commitment(block)
}

/// Per-tx empty witness stacks for blocks that need no witness data.
pub fn empty_witness_stacks_for_block(block: &Block) -> Vec<Vec<Witness>> {
    vec![Vec::new(); block.transactions.len()]
}

/// Backfill witness blob when block body is already on disk but witness row is missing/stale.
///
/// Prefer `block_in_memory` when the caller already has the block (download / validation) to
/// avoid deserializing the body from LMDB on the hot path.
pub fn try_repair_missing_witness(
    blockstore: &BlockStore,
    height: u64,
    block_hash: Hash,
    witnesses: &[Vec<Witness>],
    protocol_version: ProtocolVersion,
    block_in_memory: Option<&Block>,
) -> Result<bool> {
    if !has_real_witnesses(witnesses) {
        return Ok(false);
    }
    if !blockstore.has_witness_blob(&block_hash)? {
        if block_in_memory.is_none() && blockstore.get_block(&block_hash)?.is_none() {
            return Ok(false);
        }
    } else if let Some(w) = blockstore.get_witness(&block_hash)? {
        if has_real_witnesses(&w) {
            return Ok(false);
        }
    }
    let header_ts = match block_in_memory {
        Some(b) => b.header.timestamp,
        None => {
            blockstore
                .get_block(&block_hash)?
                .ok_or_else(|| {
                    anyhow::anyhow!("block disappeared during witness repair at {height}")
                })?
                .header
                .timestamp
        }
    };
    let registry = cached_feature_registry(protocol_version);
    if !registry.is_feature_active("segwit", height, header_ts) {
        return Ok(false);
    }
    blockstore.store_witness_at_height(&block_hash, height, witnesses)?;
    debug!(
        "[IBD_WITNESS_REPAIR] stored missing witness for height {}",
        height
    );
    Ok(true)
}

/// How many heights ahead of `validation_height` to eagerly persist gap blocks to disk.
///
/// Workers download blocks in parallel: h=N+1, N+2, … arrive at roughly the same time.
/// Without lookahead, only h=val+1 is persisted; h=val+2..N evaporate and must be
/// re-fetched from WAN when validation reaches them (40-65s each in sparse zones).
/// With lookahead=10, blocks val+1..val+10 are all written to disk as they arrive, so
/// the coordinator inject loop can chain through them instantly — 1 WAN fetch covers
/// up to N consecutive heights.
fn gap_persist_lookahead() -> u64 {
    // W66: allow up to 256 so tip-owner pipe (128) can persist-then-trim without
    // silent drops outside the old clamp(1,64) window (live: 106M DOWNLOAD_RECEIVED_TRIM).
    latch_env!(u64, {
        std::env::var("BLVM_IBD_GAP_PERSIST_LOOKAHEAD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(128)
            .clamp(1, 256)
    })
}

/// Persist body + witness for gap blocks that are within `gap_persist_lookahead` heights
/// of `validation_height + 1`.
///
/// Download workers only repaired witnesses before; without the body on disk,
/// [`coordinator_inject_local_gap`] cannot recover after a stall even when the gap
/// block was received in memory.
///
/// The wider lookahead window (default: val+1..val+10) means that when workers
/// simultaneously deliver several consecutive gap heights, ALL of them are persisted.
/// The coordinator inject loop can then chain through them immediately without a
/// separate WAN round-trip (40-65s) for each height.
///
/// If the body is already on disk but the witness row is missing/stale, upgrades the
/// witness via [`try_repair_missing_witness`] so inject can succeed without a re-fetch.
///
/// **W1:** Never persist (or reinforce) a segwit-height body with empty/missing witnesses.
/// Live soak: body-on-disk + `WitnessMissing` broke LOCAL_GAP inject chains and forced
/// 25–30s WAN gap timeouts on the same heights (71% of gap timeouts ∩ witness-miss).
pub fn try_persist_gap_block_for_local_inject(
    blockstore: &BlockStore,
    validation_height: Option<&std::sync::Arc<std::sync::atomic::AtomicU64>>,
    height: u64,
    block_hash: Hash,
    block: &Block,
    witnesses: &[Vec<Witness>],
    protocol_version: ProtocolVersion,
) -> Result<bool> {
    try_persist_gap_block_for_local_inject_with_wire(
        blockstore,
        validation_height,
        height,
        block_hash,
        block,
        witnesses,
        protocol_version,
        None,
    )
}

/// GAP_PERSIST with optional original P2P payload (W5/N1).
pub fn try_persist_gap_block_for_local_inject_with_wire(
    blockstore: &BlockStore,
    validation_height: Option<&std::sync::Arc<std::sync::atomic::AtomicU64>>,
    height: u64,
    block_hash: Hash,
    block: &Block,
    witnesses: &[Vec<Witness>],
    protocol_version: ProtocolVersion,
    wire_payload: Option<&[u8]>,
) -> Result<bool> {
    let Some(vh) = validation_height else {
        return Ok(false);
    };
    let val_h = vh.load(std::sync::atomic::Ordering::Relaxed);
    let lookahead = gap_persist_lookahead();
    // Persist only heights in [val+1, val+lookahead].
    if height == 0 || height <= val_h || height > val_h + lookahead {
        return Ok(false);
    }
    let registry = cached_feature_registry(protocol_version);
    let segwit_on = registry.is_feature_active("segwit", height, block.header.timestamp);
    // W1: stripped MSG_BLOCK (empty witness) of a block that *commits* to witnesses must
    // not create a local hole. Blocks without a BIP141 commitment may legitimately have
    // empty stacks (live: h=640022 coinbase-only) — allow persist/inject for those.
    if empty_witness_unacceptable(block, witnesses, segwit_on) {
        warn!(
            "[IBD_GAP_PERSIST_SKIP] height {} (hash {}): empty witness with commitment — not persisting",
            height,
            hex::encode(block_hash)
        );
        return Ok(false);
    }
    if blockstore.has_block_body(&block_hash)? {
        // Body present (prior MSG_BLOCK / partial persist) — still need a real witness
        // for segwit heights or inject returns WitnessMissing and the chain breaks.
        return try_repair_missing_witness(
            blockstore,
            height,
            block_hash,
            witnesses,
            protocol_version,
            Some(block),
        );
    }
    if wire_bytes_store_enabled() {
        if let Some(payload) = wire_payload.filter(|p| !p.is_empty()) {
            blockstore.store_block_wire_bytes(block, height, payload)?;
            // Wire blob path does not store witnesses — write them when stacks are real
            // (download no longer pre-repairs before persist).
            if has_real_witnesses(witnesses) {
                blockstore.store_witness_at_height(&block_hash, height, witnesses)?;
            }
            debug!(
                "[IBD_GAP_PERSIST] stored wire-bytes gap block height {} ({} payload bytes)",
                height,
                payload.len()
            );
            return Ok(true);
        }
        // Env on but no original payload — keep legacy bincode (never re-encode as "wire").
        warn!(
            "[IBD_GAP_PERSIST] WIRE_BYTES_STORE=1 but no payload at height {} — falling back to bincode",
            height
        );
    }
    blockstore.store_block_with_witness(block, witnesses, height)?;
    debug!(
        "[IBD_GAP_PERSIST] stored gap block height {} for coordinator local inject",
        height
    );
    Ok(true)
}

/// True when the body is on disk but local IBD load fails for a witness reason
/// (`WitnessMissing` / `WitnessEmptyStale`). Used by download to extend micro-chunk
/// timeouts (W2) — body is local; only a witness-bearing fetch is needed.
pub fn is_local_witness_hole(
    blockstore: &BlockStore,
    height: u64,
    block_hash: Hash,
    protocol_version: ProtocolVersion,
) -> Result<bool> {
    if !blockstore.has_block_body(&block_hash)? {
        return Ok(false);
    }
    match try_load_local_ibd_block_with_reason(blockstore, height, block_hash, protocol_version)? {
        Ok(_) => Ok(false),
        Err(LocalBlockMiss::WitnessMissing | LocalBlockMiss::WitnessEmptyStale) => Ok(true),
        Err(_) => Ok(false),
    }
}

/// Opt-in W5/N1 wire-bytes GAP_PERSIST (`BLVM_IBD_WIRE_BYTES_STORE=1`).
pub fn wire_bytes_store_enabled() -> bool {
    latch_env!(bool, {
        matches!(
            std::env::var("BLVM_IBD_WIRE_BYTES_STORE").as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
        )
    })
}

/// Load block + witnesses from disk; returns structured miss reason on failure.
pub fn try_load_local_ibd_block_with_reason(
    blockstore: &BlockStore,
    height: u64,
    expected_hash: Hash,
    protocol_version: ProtocolVersion,
) -> Result<Result<(Block, Vec<Vec<Witness>>), LocalBlockMiss>> {
    // W5: one wire deser when body is a wire blob (avoids get_block + get_witness double parse).
    let Some((block, witnesses)) = blockstore.get_block_and_witnesses(&expected_hash)? else {
        return Ok(Err(LocalBlockMiss::NotInStore));
    };
    let computed = blockstore.get_block_hash(&block);
    if computed != expected_hash {
        return Ok(Err(LocalBlockMiss::HashMismatch { computed }));
    }
    let registry = cached_feature_registry(protocol_version);
    let segwit_on = registry.is_feature_active("segwit", height, block.header.timestamp);

    match (witnesses.is_empty(), has_real_witnesses(&witnesses)) {
        (_, true) => Ok(Ok((block, witnesses))),
        (false, false) if segwit_on && coinbase_has_witness_commitment(&block) => {
            Ok(Err(LocalBlockMiss::WitnessEmptyStale))
        }
        (false, false) => Ok(Ok((block, witnesses))),
        (true, _) if !segwit_on => Ok(Ok((block, Vec::new()))),
        (true, _) if coinbase_has_witness_commitment(&block) => {
            Ok(Err(LocalBlockMiss::WitnessMissing))
        }
        (true, _) => {
            let empty = empty_witness_stacks_for_block(&block);
            Ok(Ok((block, empty)))
        }
    }
}

/// Load block + witnesses from disk when complete for IBD replay (skips network).
pub fn try_load_local_ibd_block(
    blockstore: &BlockStore,
    height: u64,
    expected_hash: Hash,
    protocol_version: ProtocolVersion,
) -> Result<Option<(Block, Vec<Vec<Witness>>)>> {
    match try_load_local_ibd_block_with_reason(blockstore, height, expected_hash, protocol_version)?
    {
        Ok(pair) => Ok(Some(pair)),
        Err(_) => Ok(None),
    }
}

pub fn ibd_stall_abort_gap_fetch_on_confirmed_bodies() -> bool {
    matches!(
        std::env::var("BLVM_IBD_STALL_ABORT_GAP_FETCH")
            .ok()
            .as_deref(),
        Some("1") | Some("true") | Some("TRUE")
    )
}

/// Whether a coordinator stall broadcast should cancel an in-flight gap block download.
///
/// Default policy (no env override):
/// - **Critical/Emergency RSS:** abort stuck gap fetches so assigner can requeue micro-chunks.
/// - **WAN multi-peer (calm):** do **not** abort — let soft-retry + tip SLA rotate the owner.
///   Live A6h (2026-07-12): W25e tip-abort at coord_stall (~15–30s) killed deep tip pipes
///   mid soft-retry (25s×3), owner tenure p50≈5s, park wall ~37%, WAN ~4.5 blk/s vs
///   breakthrough ~14 with sticky tenure minutes. Tip SLA (90s) + soft-retry handle silent peers.
/// - **LAN / single-peer:** abort so assigner can requeue quickly (legacy behavior).
/// - **Local replay gap** (`gap_height <= confirmed_body_height`): do not abort unless
///   `BLVM_IBD_STALL_ABORT_GAP_FETCH=1`.
pub fn ibd_stall_aborts_inflight_gap_fetch(
    wan_multi_peer: bool,
    confirmed_body_height: u64,
    gap_height: u64,
) -> bool {
    match std::env::var("BLVM_IBD_STALL_ABORT_GAP_FETCH")
        .ok()
        .as_deref()
    {
        Some("1") | Some("true") | Some("TRUE") => true,
        Some("0") | Some("false") | Some("FALSE") => false,
        _ => {
            if super::memory::ibd_pressure_is_critical_or_worse() {
                return true;
            }
            if wan_multi_peer {
                // A6i: never abort calm WAN gap fetches (local replay or tip crawl).
                let _ = (confirmed_body_height, gap_height);
                false
            } else {
                !(confirmed_body_height > 0 && gap_height <= confirmed_body_height)
            }
        }
    }
}

pub fn ibd_local_gap_fill_enabled() -> bool {
    !matches!(
        std::env::var("BLVM_IBD_LOCAL_GAP_FILL").ok().as_deref(),
        Some("0") | Some("false") | Some("FALSE")
    )
}

/// Optional hard ceiling for local gap inject (`BLVM_IBD_LOCAL_GAP_FILL_MAX_HEIGHT`).
///
/// Default is uncapped (`u64::MAX`). Inject is already bounded to
/// `validation_height+1 .. +lookahead` by [`coordinator_inject_local_gap`]; capping at
/// start-time `confirmed_body_height` broke GAP_PERSIST recovery once validation passed
/// that watermark (live: inject stopped ≤648656 with confirmed=649265 while bodies were
/// persisted at 656k+ and validation crawled via 8s micro-chunk races).
pub fn ibd_local_gap_fill_max_height(_confirmed_body_height_at_start: u64) -> u64 {
    std::env::var("BLVM_IBD_LOCAL_GAP_FILL_MAX_HEIGHT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(u64::MAX)
}

fn height_eligible_for_local_gap_fill(height: u64, confirmed_body_height_at_start: u64) -> bool {
    if !ibd_local_gap_fill_enabled() || height == 0 {
        return false;
    }
    // Per-height try is cheap; caller already limits to val+1..lookahead.
    // Do not freeze eligibility at start-time confirmed bodies — GAP_PERSIST extends
    // the on-disk set during the run.
    height <= ibd_local_gap_fill_max_height(confirmed_body_height_at_start)
}

/// How many consecutive heights beyond `val+1` to pre-inject from disk in one coordinator loop.
///
/// After injecting val+1, the loop continues to val+2, val+3, … up to this limit, so a run
/// of on-disk gap heights are all queued in the reorder_buffer in a single pass rather than
/// waiting one full coordinator loop (potentially 5-10 s under load) per height.
pub fn gap_inject_lookahead_pub() -> u64 {
    gap_inject_lookahead()
}

fn gap_inject_lookahead() -> u64 {
    // L3: default 64 (was 10) so dense on-disk runs refill reorder in one coordinator pass
    // instead of ~one inject per gap-poll tick (live crawl ~4–8 inject/s).
    latch_env!(u64, {
        std::env::var("BLVM_IBD_GAP_INJECT_LOOKAHEAD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64)
            .clamp(1, 128)
    })
}

/// Try to load exactly `height` from disk and insert into the coordinator reorder buffer.
///
/// Returns `true` when a block was injected (dispatch can proceed on the next loop pass).
/// Use [`coordinator_inject_local_gap_chain`] from coordinator call sites — it wraps this
/// function and chains up to `gap_inject_lookahead()` consecutive on-disk heights.
///
/// `already_dispatched`: heights already handed to prefetch/bridge. Must not be re-loaded
/// from disk — under COMPACTER_GATE / RSS pauses the reorder buffer is empty while the
/// block is still alive in the OrderedReadyBridge/feeder; re-injecting allocated a fresh
/// `Arc<Block>` thousands of times per height (observed: 1753× at h≈539651 → OOM).
fn coordinator_inject_one(
    blockstore: &BlockStore,
    protocol_version: ProtocolVersion,
    height: u64,
    confirmed_body_height_at_start: u64,
    reorder_buffer: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    already_dispatched: &rustc_hash::FxHashSet<u64>,
    log_miss: &mut rustc_hash::FxHashSet<u64>,
    // When true, ignore `already_dispatched` and attempt a disk load (W24 tip recovery).
    reload_if_dispatched: bool,
) -> Result<bool> {
    if reorder_buffer.contains_key(&height) {
        return Ok(true); // already queued; caller can continue chaining
    }
    if already_dispatched.contains(&height) && !reload_if_dispatched {
        // Still in the pipeline (bridge/feeder/validation). Do not reload from disk.
        return Ok(true);
    }
    if !height_eligible_for_local_gap_fill(height, confirmed_body_height_at_start) {
        return Ok(false);
    }
    let Some(expected_hash) = blockstore.get_hash_by_height(height)? else {
        if log_miss.insert(height) {
            warn!(
                "[IBD_LOCAL_GAP] height {}: {}",
                height,
                LocalBlockMiss::HeightHashUnavailable
            );
        }
        return Ok(false);
    };
    match try_load_local_ibd_block_with_reason(blockstore, height, expected_hash, protocol_version)?
    {
        Ok((block, witnesses)) => {
            ARC_BLOCK_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            reorder_buffer.insert(height, (Arc::new(block), Arc::new(witnesses)));
            crate::node::parallel_ibd::tip_stage::mark_reorder(height);
            log_miss.remove(&height);
            debug!("[IBD_LOCAL_GAP] injected local block height {}", height);
            Ok(true)
        }
        Err(miss) => {
            if log_miss.insert(height) {
                warn!("[IBD_LOCAL_GAP] height {}: {miss}", height);
            }
            Ok(false)
        }
    }
}

/// Inject `validation_height + 1` from disk and chain up to `gap_inject_lookahead()` more
/// consecutive heights that are already available on disk.
///
/// **Why chaining matters**: after the wider GAP_PERSIST window persists val+1..val+N to disk,
/// this function injects all of them in one coordinator loop pass rather than one per loop
/// (each pass can take 5-10 s under load). A 10-height chain resolves a dense sparse cluster
/// 10× faster than the single-height path.
///
/// Heights that are already in the reorder_buffer are skipped (not re-fetched from disk).
/// The chain stops at the first height that is not on disk — that height needs WAN delivery.
///
/// Returns `true` if at least one block was injected **or** is already in-pipeline
/// (`already_dispatched` / reorder). Callers must **not** clear `dispatched` for these
/// heights — that was the LOCAL_GAP re-inject storm (reload → clear dispatched → re-dispatch).
///
/// `tip_in_pipeline`: caller confirms the validation tip is in bridge/feeder (or already
/// emitted). When false, a tip marked `already_dispatched` is **not** treated as success —
/// we try disk (or fail) so we do not chain H+1.. over a lost tip (W24 live: INJECT_CHAIN 24
/// while tip missing → covering collapse).
///
/// **W99 REVERTED (2026-07-18):** tip-only lookahead when `!tip_in_pipeline` correlated
/// with early tip freezes (@313k–321k) and did not recover W95 tip60. Call sites already
/// gate chain inject on bridge/feeder (`tip_in_pipeline`); restore full
/// [`gap_inject_lookahead`] for both paths (W24 still reloads tip when `!tip_in_pipeline`).
pub fn coordinator_inject_local_gap(
    blockstore: &BlockStore,
    protocol_version: ProtocolVersion,
    height: u64,
    confirmed_body_height_at_start: u64,
    validation_height: u64,
    reorder_buffer: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    already_dispatched: &rustc_hash::FxHashSet<u64>,
    log_miss: &mut rustc_hash::FxHashSet<u64>,
    tip_in_pipeline: bool,
) -> Result<bool> {
    if height != validation_height.saturating_add(1) {
        return Ok(false);
    }
    let lookahead = gap_inject_lookahead();
    let mut any_success = false;
    // Only count heights newly inserted into reorder. Already-dispatched / already-queued
    // heights also return Ok(true) from inject_one (so chaining can continue past them), but
    // counting those as "chained" made Case D log `[IBD_INJECT_CHAIN]` every coordinator
    // loop (~80k lines/s live 2026-07-15) while validation was stuck — drowning tip metrics
    // and burning CPU/disk on logging.
    let mut newly_injected = 0u64;
    let mut last_new_h = height;
    for i in 0..lookahead {
        let h = height + i;
        let had_in_reorder = reorder_buffer.contains_key(&h);
        // Tip only: reload when not confirmed in-pipeline. Lookahead heights keep skip-on-dispatched.
        let reload_if_dispatched = i == 0 && !tip_in_pipeline;
        match coordinator_inject_one(
            blockstore,
            protocol_version,
            h,
            confirmed_body_height_at_start,
            reorder_buffer,
            already_dispatched,
            log_miss,
            reload_if_dispatched,
        )? {
            true => {
                any_success = true;
                if !had_in_reorder && reorder_buffer.contains_key(&h) {
                    newly_injected += 1;
                    last_new_h = h;
                }
                // Continue chaining: next height might also be on disk.
            }
            false => break, // height not on disk; stop — WAN fetch must fill the gap.
        }
    }
    if newly_injected > 1 {
        info!(
            "[IBD_INJECT_CHAIN] from {} chained {} height(s) (lookahead={}, stopped_at={})",
            height,
            newly_injected,
            lookahead,
            last_new_h.saturating_add(1)
        );
    }
    Ok(any_success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::blockstore::BlockStore;
    use crate::storage::database::{create_database, default_backend};
    use blvm_protocol::{
        Block, BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput,
    };
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use tempfile::TempDir;

    fn temp_blockstore() -> BlockStore {
        let dir = TempDir::new().unwrap();
        let db: Arc<dyn crate::storage::database::Database> =
            Arc::from(create_database(dir.path(), default_backend(), None).unwrap());
        std::mem::forget(dir);
        BlockStore::new(db).unwrap()
    }

    #[test]
    fn local_gap_fill_allowed_without_contiguous_body_probe() {
        assert!(height_eligible_for_local_gap_fill(338_305, 0));
        assert!(!height_eligible_for_local_gap_fill(0, 0));
    }

    /// W5/N1: wire-bytes body coexists with bincode; inject loads via one wire deser.
    #[test]
    fn w5_wire_bytes_persist_and_inject_coexist_with_bincode() {
        use crate::storage::blockstore::{
            decode_wire_body_blob, encode_wire_body_blob, is_wire_body_blob,
        };
        use blvm_protocol::serialization::{
            deserialize_block_with_witnesses, serialize_block_with_witnesses,
        };

        let blockstore = temp_blockstore();
        let height = 500u64;
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        let witnesses: Vec<Vec<Witness>> = vec![vec![]];
        // Unit uses serialize_block_with_witnesses as a stand-in payload; production stores
        // the original P2P frame bytes (lossy re-encode is explicitly avoided on the hot path).
        let payload = serialize_block_with_witnesses(&block, &witnesses, true);
        let hash = blockstore.get_block_hash(&block);
        blockstore.store_height(height, &hash).unwrap();
        blockstore
            .store_block_wire_bytes(&block, height, &payload)
            .unwrap();

        let blob = blockstore
            .get_block_and_witnesses(&hash)
            .unwrap()
            .expect("wire body present");
        assert_eq!(blockstore.get_block_hash(&blob.0), hash);
        assert!(is_wire_body_blob(&encode_wire_body_blob(&payload)));
        assert_eq!(
            decode_wire_body_blob(&encode_wire_body_blob(&payload)).unwrap(),
            payload.as_slice()
        );

        let loaded =
            try_load_local_ibd_block(&blockstore, height, hash, ProtocolVersion::BitcoinV1)
                .unwrap()
                .expect("inject load");
        assert_eq!(blockstore.get_block_hash(&loaded.0), hash);

        // Legacy bincode at another height still works (dual-format).
        let h2 = 501u64;
        let hash2 = {
            let mut b2 = block.clone();
            b2.header.nonce = 99;
            let h = blockstore.get_block_hash(&b2);
            blockstore.store_height(h2, &h).unwrap();
            blockstore
                .store_block_with_witness(&b2, &witnesses, h2)
                .unwrap();
            h
        };
        let legacy = try_load_local_ibd_block(&blockstore, h2, hash2, ProtocolVersion::BitcoinV1)
            .unwrap()
            .expect("bincode inject");
        assert_eq!(blockstore.get_block_hash(&legacy.0), hash2);

        // Micro: bincode ser+de×2 vs memcpy+wire deser (same payload).
        let t0 = std::time::Instant::now();
        for _ in 0..200 {
            let body = bincode::serialize(&block).unwrap();
            let wit = bincode::serialize(&witnesses).unwrap();
            let _: Block = bincode::deserialize(&body).unwrap();
            let _: Vec<Vec<Witness>> = bincode::deserialize(&wit).unwrap();
        }
        let bincode_ns = t0.elapsed().as_nanos() / 200;
        let t1 = std::time::Instant::now();
        for _ in 0..200 {
            let tagged = encode_wire_body_blob(&payload);
            let p = decode_wire_body_blob(&tagged).unwrap();
            let _ = deserialize_block_with_witnesses(p).unwrap();
        }
        let wire_ns = t1.elapsed().as_nanos() / 200;
        eprintln!(
            "[W5 micro] bincode ser+de×2 ≈{bincode_ns} ns/op; wire memcpy+deser ≈{wire_ns} ns/op"
        );
        let _ = (bincode_ns, wire_ns);
    }

    #[test]
    fn sparse_body_probe_finds_max_without_height_one() {
        let blockstore = temp_blockstore();
        // Contiguous height index (headers synced through tip) but no body at h=1 — mirrors
        // resume-after-checkpoint where `highest_stored_height` must see the tip, not height 0.
        let height = 500u64;
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = blockstore.get_block_hash(&block);
        let placeholder = [0xAAu8; 32];
        for h in 0..height {
            blockstore.store_height(h, &placeholder).unwrap();
        }
        blockstore
            .store_block_with_witness(&block, &[], height)
            .unwrap();
        blockstore.store_height(height, &hash).unwrap();
        assert_eq!(probe_confirmed_body_height(&blockstore).unwrap(), 0);
        assert_eq!(
            probe_highest_stored_body_height(&blockstore).unwrap(),
            height
        );
        assert!(should_skip_block_store_write(&blockstore, height, &hash, 0).unwrap());
        assert!(!should_skip_block_store_write(&blockstore, height + 1, &[0xBBu8; 32], 0).unwrap());
    }

    #[test]
    fn contiguous_body_probe_unchanged_when_height_one_present() {
        let blockstore = temp_blockstore();
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        blockstore.store_block_with_witness(&block, &[], 1).unwrap();
        blockstore.store_height(0, &[0u8; 32]).unwrap();
        blockstore
            .store_height(1, &blockstore.get_block_hash(&block))
            .unwrap();
        assert_eq!(probe_confirmed_body_height(&blockstore).unwrap(), 1);
        assert_eq!(probe_highest_stored_body_height(&blockstore).unwrap(), 1);
    }

    #[test]
    fn cached_feature_registry_is_process_latched() {
        let a = cached_feature_registry(ProtocolVersion::BitcoinV1);
        let b = cached_feature_registry(ProtocolVersion::BitcoinV1);
        assert!(std::ptr::eq(a, b), "same protocol must reuse one registry");
        assert!(a.is_feature_active("segwit", 500_000, 1_600_000_000));
        let r = cached_feature_registry(ProtocolVersion::Regtest);
        assert!(
            !std::ptr::eq(a, r),
            "distinct protocols get distinct caches"
        );
    }

    #[test]
    fn stall_abort_policy() {
        use super::super::memory::{self, PressureLevel};
        let prev = std::env::var("BLVM_IBD_STALL_ABORT_GAP_FETCH").ok();
        unsafe { std::env::remove_var("BLVM_IBD_STALL_ABORT_GAP_FETCH") };
        memory::publish_ibd_pressure(PressureLevel::None);
        assert!(!ibd_stall_aborts_inflight_gap_fetch(true, 0, 265_553));
        // A6i: WAN tip crawl must NOT abort on coord stall (soft-retry + tip SLA rotate).
        assert!(!ibd_stall_aborts_inflight_gap_fetch(true, 640_000, 640_001));
        assert!(!ibd_stall_aborts_inflight_gap_fetch(true, 640_000, 640_000));
        assert!(ibd_stall_aborts_inflight_gap_fetch(false, 0, 265_553));
        memory::publish_ibd_pressure(PressureLevel::Critical);
        assert!(ibd_stall_aborts_inflight_gap_fetch(true, 0, 265_553));
        memory::publish_ibd_pressure(PressureLevel::None);
        unsafe { std::env::set_var("BLVM_IBD_STALL_ABORT_GAP_FETCH", "1") };
        assert!(ibd_stall_aborts_inflight_gap_fetch(true, 0, 265_553));
        if let Some(v) = prev {
            unsafe { std::env::set_var("BLVM_IBD_STALL_ABORT_GAP_FETCH", v) };
        } else {
            unsafe { std::env::remove_var("BLVM_IBD_STALL_ABORT_GAP_FETCH") };
        }
        memory::publish_ibd_pressure(PressureLevel::None);
    }

    #[test]
    fn local_block_miss_display() {
        let m = LocalBlockMiss::WitnessEmptyStale;
        assert!(m.to_string().contains("empty"));
    }

    #[test]
    fn extend_contiguous_body_tip_stops_at_hole() {
        let blockstore = temp_blockstore();
        for h in [100u64, 101, 102, 104] {
            let block = Block {
                header: BlockHeader {
                    version: 4,
                    timestamp: 1_600_000_000 + h,
                    ..Default::default()
                },
                transactions: vec![Transaction {
                    version: 1,
                    inputs: blvm_protocol::tx_inputs![],
                    outputs: blvm_protocol::tx_outputs![TransactionOutput {
                        value: 50_0000_0000,
                        script_pubkey: vec![0x51],
                    }],
                    lock_time: 0,
                }]
                .into(),
            };
            let hash = blockstore.get_block_hash(&block);
            blockstore.store_height(h, &hash).unwrap();
            blockstore.store_block_with_witness(&block, &[], h).unwrap();
        }
        assert_eq!(
            extend_contiguous_body_tip(&blockstore, 100, 256).unwrap(),
            102,
            "must stop before hole at 103"
        );
        assert_eq!(
            extend_contiguous_body_tip(&blockstore, 102, 256).unwrap(),
            102,
            "no further contiguous from hole edge"
        );
    }

    #[test]
    fn persist_gap_block_enables_local_inject() {
        let blockstore = temp_blockstore();
        let vh = Arc::new(AtomicU64::new(499));
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = blockstore.get_block_hash(&block);
        blockstore.store_height(500, &hash).unwrap();
        assert!(!blockstore.has_block_body(&hash).unwrap());

        let persisted = try_persist_gap_block_for_local_inject(
            &blockstore,
            Some(&vh),
            500,
            hash,
            &block,
            &[],
            ProtocolVersion::BitcoinV1,
        )
        .unwrap();
        assert!(persisted);
        assert!(blockstore.has_block_body(&hash).unwrap());

        let mut reorder_buffer = std::collections::BTreeMap::new();
        let already_dispatched = rustc_hash::FxHashSet::default();
        let mut log_miss = rustc_hash::FxHashSet::default();
        assert!(
            coordinator_inject_local_gap(
                &blockstore,
                ProtocolVersion::BitcoinV1,
                500,
                0,
                499,
                &mut reorder_buffer,
                &already_dispatched,
                &mut log_miss,
                false,
            )
            .unwrap()
        );
        assert!(reorder_buffer.contains_key(&500));
    }

    #[test]
    fn w1_persist_skips_empty_witness_when_commitment_present() {
        // Post-segwit + BIP141 commitment + empty witnesses = stripped MSG_BLOCK → skip.
        let blockstore = temp_blockstore();
        let vh = Arc::new(AtomicU64::new(500_000));
        let height = 500_001u64;
        let mut commitment_script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
        commitment_script.extend_from_slice(&[0u8; 32]);
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![TransactionInput {
                    prevout: OutPoint {
                        hash: [0u8; 32],
                        index: 0xffff_ffff,
                    },
                    script_sig: vec![0x01].into(),
                    sequence: 0xffff_ffff,
                }],
                outputs: blvm_protocol::tx_outputs![
                    TransactionOutput {
                        value: 50_0000_0000,
                        script_pubkey: vec![0x51],
                    },
                    TransactionOutput {
                        value: 0,
                        script_pubkey: commitment_script.into(),
                    }
                ],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = blockstore.get_block_hash(&block);
        blockstore.store_height(height, &hash).unwrap();
        let persisted = try_persist_gap_block_for_local_inject(
            &blockstore,
            Some(&vh),
            height,
            hash,
            &block,
            &[], // empty witness
            ProtocolVersion::BitcoinV1,
        )
        .unwrap();
        assert!(
            !persisted,
            "W1 must refuse empty-witness persist when commitment present"
        );
        assert!(
            !blockstore.has_block_body(&hash).unwrap(),
            "body must not be written"
        );
    }

    #[test]
    fn w1_persist_allows_empty_witness_without_commitment() {
        // Live h=640022: coinbase-only, no BIP141 commitment — empty stacks are valid.
        let blockstore = temp_blockstore();
        let vh = Arc::new(AtomicU64::new(500_000));
        let height = 500_001u64;
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = blockstore.get_block_hash(&block);
        blockstore.store_height(height, &hash).unwrap();
        let persisted = try_persist_gap_block_for_local_inject(
            &blockstore,
            Some(&vh),
            height,
            hash,
            &block,
            &[], // empty witness
            ProtocolVersion::BitcoinV1,
        )
        .unwrap();
        assert!(persisted, "empty witness without commitment must persist");
    }

    /// Wire GAP_PERSIST must store witness row when real stacks are provided (no download pre-repair).
    #[test]
    fn wire_persist_stores_witness_blob_for_commitment_block() {
        use blvm_protocol::serialization::serialize_block_with_witnesses;

        let prev_wire = std::env::var("BLVM_IBD_WIRE_BYTES_STORE").ok();
        unsafe { std::env::set_var("BLVM_IBD_WIRE_BYTES_STORE", "1") };

        let blockstore = temp_blockstore();
        let vh = Arc::new(AtomicU64::new(500_000));
        let height = 500_001u64;
        let mut commitment_script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
        commitment_script.extend_from_slice(&[0u8; 32]);
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![TransactionInput {
                    prevout: OutPoint {
                        hash: [0u8; 32],
                        index: 0xffff_ffff,
                    },
                    script_sig: vec![0x01].into(),
                    sequence: 0xffff_ffff,
                }],
                outputs: blvm_protocol::tx_outputs![
                    TransactionOutput {
                        value: 50_0000_0000,
                        script_pubkey: vec![0x51],
                    },
                    TransactionOutput {
                        value: 0,
                        script_pubkey: commitment_script.into(),
                    }
                ],
                lock_time: 0,
            }]
            .into(),
        };
        let witnesses: Vec<Vec<Witness>> = vec![vec![vec![vec![0x01u8, 0x02, 0x03]]]];
        let payload = serialize_block_with_witnesses(&block, &witnesses, true);
        let hash = blockstore.get_block_hash(&block);
        blockstore.store_height(height, &hash).unwrap();
        let persisted = try_persist_gap_block_for_local_inject_with_wire(
            &blockstore,
            Some(&vh),
            height,
            hash,
            &block,
            &witnesses,
            ProtocolVersion::BitcoinV1,
            Some(payload.as_slice()),
        )
        .unwrap();
        assert!(persisted, "wire persist with real witnesses must succeed");
        assert!(
            blockstore.has_witness_blob(&hash).unwrap(),
            "wire path must write witness row"
        );

        unsafe {
            match prev_wire {
                Some(v) => std::env::set_var("BLVM_IBD_WIRE_BYTES_STORE", v),
                None => std::env::remove_var("BLVM_IBD_WIRE_BYTES_STORE"),
            }
        }
    }

    #[test]
    fn local_load_synthesizes_empty_witness_without_commitment() {
        let blockstore = temp_blockstore();
        let height = 500_001u64;
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = blockstore.get_block_hash(&block);
        blockstore.store_height(height, &hash).unwrap();
        blockstore
            .store_block_with_witness(&block, &[], height)
            .unwrap();
        let loaded =
            try_load_local_ibd_block(&blockstore, height, hash, ProtocolVersion::BitcoinV1)
                .unwrap();
        assert!(
            loaded.is_some(),
            "body without commitment must load with empty witnesses"
        );
        assert!(
            !is_local_witness_hole(&blockstore, height, hash, ProtocolVersion::BitcoinV1).unwrap(),
            "no commitment → not a witness hole"
        );
    }

    #[test]
    fn is_local_witness_hole_detects_body_without_witness() {
        let blockstore = temp_blockstore();
        let height = 500_001u64;
        let mut commitment_script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
        commitment_script.extend_from_slice(&[0u8; 32]);
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![TransactionInput {
                    prevout: OutPoint {
                        hash: [0u8; 32],
                        index: 0xffff_ffff,
                    },
                    script_sig: vec![0x01].into(),
                    sequence: 0xffff_ffff,
                }],
                outputs: blvm_protocol::tx_outputs![
                    TransactionOutput {
                        value: 50_0000_0000,
                        script_pubkey: vec![0x51],
                    },
                    TransactionOutput {
                        value: 0,
                        script_pubkey: commitment_script.into(),
                    }
                ],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = blockstore.get_block_hash(&block);
        blockstore.store_height(height, &hash).unwrap();
        // Store body without witness (simulates prior MSG_BLOCK / partial persist).
        blockstore
            .store_block_with_witness(&block, &[], height)
            .unwrap();
        assert!(
            is_local_witness_hole(&blockstore, height, hash, ProtocolVersion::BitcoinV1).unwrap()
        );
    }

    #[test]
    fn inject_past_start_confirmed_body_cap_when_uncapped() {
        // Live stall: confirmed_bodies=649265 at start; GAP_PERSIST wrote past that; inject
        // refused heights > confirmed. Default max is uncapped so disk-complete gaps inject.
        // Use a pre-segwit height so empty witness is valid (post-segwit needs real stacks).
        let blockstore = temp_blockstore();
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_400_000_000,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = blockstore.get_block_hash(&block);
        blockstore.store_height(500, &hash).unwrap();
        blockstore
            .store_block_with_witness(&block, &[], 500)
            .unwrap();

        let mut reorder_buffer = std::collections::BTreeMap::new();
        let already_dispatched = rustc_hash::FxHashSet::default();
        let mut log_miss = rustc_hash::FxHashSet::default();
        assert!(
            coordinator_inject_local_gap(
                &blockstore,
                ProtocolVersion::BitcoinV1,
                500,
                400, // start-time confirmed — must not block inject at h=500
                499,
                &mut reorder_buffer,
                &already_dispatched,
                &mut log_miss,
                false,
            )
            .unwrap()
        );
        assert!(reorder_buffer.contains_key(&500));
    }

    #[test]
    fn inject_skips_already_dispatched_heights() {
        let blockstore = temp_blockstore();
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = blockstore.get_block_hash(&block);
        blockstore.store_height(500, &hash).unwrap();
        blockstore
            .store_block_with_witness(&block, &[], 500)
            .unwrap();

        let mut reorder_buffer = std::collections::BTreeMap::new();
        let mut already_dispatched = rustc_hash::FxHashSet::default();
        already_dispatched.insert(500);
        let mut log_miss = rustc_hash::FxHashSet::default();
        // Reports success (in-pipeline) but must NOT put a block in the reorder buffer.
        // Do not assert on ARC_BLOCK_CREATED — that global is shared across parallel tests.
        assert!(
            coordinator_inject_local_gap(
                &blockstore,
                ProtocolVersion::BitcoinV1,
                500,
                0,
                499,
                &mut reorder_buffer,
                &already_dispatched,
                &mut log_miss,
                true,
            )
            .unwrap()
        );
        assert!(
            reorder_buffer.is_empty(),
            "already-dispatched height must not be re-loaded into reorder_buffer"
        );
    }

    #[test]
    fn inject_chain_skips_counting_already_present_heights() {
        // Tip in reorder + ahead height already_dispatched must not look like a fresh chain
        // (regression: Case D logged IBD_INJECT_CHAIN every loop for already-present spans).
        let blockstore = temp_blockstore();
        let mk = |ts| Block {
            header: BlockHeader {
                version: 4,
                timestamp: ts,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        let b500 = mk(1_600_000_000);
        let h500 = blockstore.get_block_hash(&b500);
        blockstore.store_height(500, &h500).unwrap();
        blockstore
            .store_block_with_witness(&b500, &[], 500)
            .unwrap();
        let b501 = mk(1_600_000_001);
        let h501 = blockstore.get_block_hash(&b501);
        blockstore.store_height(501, &h501).unwrap();
        blockstore
            .store_block_with_witness(&b501, &[], 501)
            .unwrap();

        let mut reorder_buffer = std::collections::BTreeMap::new();
        reorder_buffer.insert(500, (Arc::new(b500), Arc::new(Vec::new())));
        let mut already_dispatched = rustc_hash::FxHashSet::default();
        already_dispatched.insert(500);
        already_dispatched.insert(501);
        let mut log_miss = rustc_hash::FxHashSet::default();
        assert!(
            coordinator_inject_local_gap(
                &blockstore,
                ProtocolVersion::BitcoinV1,
                500,
                0,
                499,
                &mut reorder_buffer,
                &already_dispatched,
                &mut log_miss,
                true,
            )
            .unwrap()
        );
        assert_eq!(
            reorder_buffer.len(),
            1,
            "already-present chain must not grow reorder_buffer"
        );
        assert!(reorder_buffer.contains_key(&500));
        assert!(!reorder_buffer.contains_key(&501));
    }

    #[test]
    fn inject_tip_not_in_pipeline_reloads_from_disk() {
        // W24: tip marked dispatched but not confirmed in bridge/feeder must reload
        // from disk instead of chaining over a missing tip.
        let blockstore = temp_blockstore();
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = blockstore.get_block_hash(&block);
        blockstore.store_height(500, &hash).unwrap();
        blockstore
            .store_block_with_witness(&block, &[], 500)
            .unwrap();

        let mut reorder_buffer = std::collections::BTreeMap::new();
        let mut already_dispatched = rustc_hash::FxHashSet::default();
        already_dispatched.insert(500);
        let mut log_miss = rustc_hash::FxHashSet::default();
        assert!(
            coordinator_inject_local_gap(
                &blockstore,
                ProtocolVersion::BitcoinV1,
                500,
                0,
                499,
                &mut reorder_buffer,
                &already_dispatched,
                &mut log_miss,
                false, // tip not confirmed in pipeline
            )
            .unwrap()
        );
        assert!(
            reorder_buffer.contains_key(&500),
            "unconfirmed dispatched tip must reload from disk"
        );
    }

    #[test]
    fn witness_repair_stores_row_key_when_body_exists() {
        let blockstore = temp_blockstore();
        let height = 500_000u64;
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = blockstore.get_block_hash(&block);
        // Height index is required: store_witness_at_height writes the row key
        // (height||hash); has_witness_blob / get_witness resolve via height_index.
        blockstore.store_height(height, &hash).unwrap();
        blockstore
            .store_block_with_witness(&block, &[], height)
            .unwrap();
        assert!(!blockstore.has_witness_blob(&hash).unwrap());

        let witnesses: Vec<Vec<Witness>> = vec![vec![vec![vec![0x51u8]]]];
        let repaired = try_repair_missing_witness(
            &blockstore,
            height,
            hash,
            witnesses.as_slice(),
            ProtocolVersion::BitcoinV1,
            Some(&block),
        )
        .unwrap();
        assert!(repaired);
        assert!(blockstore.has_witness_blob(&hash).unwrap());
        let loaded = blockstore.get_witness(&hash).unwrap().unwrap();
        assert!(has_real_witnesses(&loaded));
    }
}
