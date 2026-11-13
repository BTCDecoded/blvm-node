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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tracing::{debug, info, warn};

/// Contiguous on-disk body tip advanced on the flush write path.
/// Coordinator copies this into `live_body_tip`. Discovery walks are not
/// tip authorities (closed `body_tip` cascade).
static FLUSH_PATH_BODY_TIP: AtomicU64 = AtomicU64::new(0);

/// Incremental-prune keep boundary. `0` = archive (GAP_PERSIST unchanged).
/// Heights below this are not written and are not deleted on the validation thread.
static IBD_PRUNE_HORIZON: AtomicU64 = AtomicU64::new(0);
static GAP_PERSIST_PRUNE_SKIP_LOGGED: AtomicBool = AtomicBool::new(false);

/// Process-latched [`FeatureRegistry`] — `for_protocol` rebuilds `Vec`+`String` feature
/// names on every call. Tip crawl hits persist/load/serve per body; cache by protocol.
pub fn cached_feature_registry(protocol_version: ProtocolVersion) -> &'static FeatureRegistry {
    static MAINNET: OnceLock<FeatureRegistry> = OnceLock::new();
    static TESTNET3: OnceLock<FeatureRegistry> = OnceLock::new();
    static REGTEST: OnceLock<FeatureRegistry> = OnceLock::new();
    static SIGNET: OnceLock<FeatureRegistry> = OnceLock::new();
    static TESTNET4: OnceLock<FeatureRegistry> = OnceLock::new();
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
        ProtocolVersion::Testnet4 => {
            TESTNET4.get_or_init(|| FeatureRegistry::for_protocol(ProtocolVersion::Testnet4))
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
            Some(hash) => blockstore.has_block_body(&hash),
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

/// Peer body warehouse: advance live `wan_body_tip` as bodies are persisted, not only
/// at spawn discovery. Default **on**. Opt out with `BLVM_IBD_BODY_WAREHOUSE=0`.
///
/// Genesis spawn has contiguous=0, so `wan_live_body_tip` stays 0 until this refresh
/// walks the store. Fixture height-keys already have a range at spawn (step 0).
/// Dens-192 bake stays burned — this is crawl disengage, not inject.
pub fn body_warehouse_enabled() -> bool {
    latch_env!(bool, {
        !matches!(
            std::env::var("BLVM_IBD_BODY_WAREHOUSE").as_deref(),
            Ok("0") | Ok("false") | Ok("FALSE")
        )
    })
}

/// Contiguous tip published by [`note_flush_path_bodies`].
pub fn flush_path_body_tip() -> u64 {
    FLUSH_PATH_BODY_TIP.load(Ordering::Relaxed)
}

fn flush_path_height_has_body(blockstore: &BlockStore, height: u64) -> Result<bool> {
    let Some(hash) = blockstore.get_hash_by_height(height)? else {
        return Ok(false);
    };
    blockstore.has_block_body(&hash)
}

fn bump_flush_path_if_next(height: u64) {
    let mut tip = FLUSH_PATH_BODY_TIP.load(Ordering::Relaxed);
    if height <= tip || height != tip.saturating_add(1) {
        return;
    }
    loop {
        match FLUSH_PATH_BODY_TIP.compare_exchange_weak(
            tip,
            height,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(actual) => {
                tip = actual;
                if height <= tip || height != tip.saturating_add(1) {
                    return;
                }
            }
        }
    }
}

fn catch_up_flush_path_on_disk(blockstore: &BlockStore) {
    let mut tip = FLUSH_PATH_BODY_TIP.load(Ordering::Relaxed);
    loop {
        let next = tip.saturating_add(1);
        match flush_path_height_has_body(blockstore, next) {
            Ok(true) => {}
            _ => break,
        }
        match FLUSH_PATH_BODY_TIP.compare_exchange(tip, next, Ordering::Release, Ordering::Relaxed)
        {
            Ok(_) => tip = next,
            Err(actual) => {
                if actual > tip {
                    tip = actual;
                } else {
                    break;
                }
            }
        }
    }
}

fn publish_flush_path_tip() {
    let tip = FLUSH_PATH_BODY_TIP.load(Ordering::Relaxed);
    if tip > 0 {
        super::tip_stage::publish_wan_body_tip(tip);
    }
}

/// Sequential bump only. **Tests.** Not a live publisher (arrival / feeder-take /
/// flush-commit experiments excluded).
pub fn note_available_body(height: u64) {
    if !body_warehouse_enabled() || height == 0 {
        return;
    }
    bump_flush_path_if_next(height);
    publish_flush_path_tip();
}

/// Disk-insert authority for `wan_body_tip`. Call after a body is in the
/// store (`store_block_with_witness` / wire-bytes / already-persisted skip).
/// Flush chunk **commit** is durability only and must not be the publisher.
///
/// Sequential heights bump without a disk walk. Out-of-order writes do not
/// jump holes. After the batch, catch-up fills successors already stored.
pub fn note_flush_path_bodies(blockstore: &BlockStore, heights: &[u64]) {
    if !body_warehouse_enabled() {
        return;
    }
    for &height in heights {
        if height == 0 {
            continue;
        }
        bump_flush_path_if_next(height);
    }
    catch_up_flush_path_on_disk(blockstore);
    publish_flush_path_tip();
}

#[cfg(test)]
pub(crate) fn reset_flush_path_body_tip_for_test() {
    FLUSH_PATH_BODY_TIP.store(0, Ordering::Relaxed);
}

/// Extend contiguous on-disk body tip from `from` forward (at most `max_steps` heights).
///
/// Used by spawn-range / unit tests. **Not** a live tip authority — the
/// coordinator must not walk this (closed `body_tip` cascade).
/// Does **not** jump sparse holes (unlike [`probe_highest_stored_body_height`]).
pub fn extend_contiguous_body_tip(
    blockstore: &BlockStore,
    from: u64,
    max_steps: u64,
) -> Result<u64> {
    if from == 0 || max_steps == 0 {
        return Ok(from);
    }
    let mut tip = from;
    // Apply-tracking backup: one tick may need to cover a 300+ BPS apply window.
    let limit = max_steps.min(4096);
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

/// Contiguous on-disk body tip starting at `from` (inclusive). Stops at the first
/// missing body — does **not** jump leftover cheese the way
/// [`probe_confirmed_body_height`] / [`probe_highest_stored_body_height`] do.
///
/// After incremental prune + partial re-download, `has_body` is non-monotonic.
/// Binary search lands on the highest leftover (live: 185817) and hides holes
/// (70679 / 70713). New-user GetData must see the first hole.
/// Lowest height whose height-index row has a stored body.
///
/// Does **not** assume genesis. A height-key slice (first key 180001) or a dest
/// seeded at 500k returns that range start instead of 0. Empty store → 0.
pub fn first_stored_body_height(blockstore: &BlockStore) -> Result<u64> {
    let tree = blockstore.height_tree()?;
    for item in tree.iter() {
        let (key, _) = item?;
        if key.len() != 8 {
            continue;
        }
        let h = u64::from_be_bytes(key[..].try_into().unwrap_or([0u8; 8]));
        if h == 0 {
            continue;
        }
        let Some(hash) = blockstore.get_hash_by_height(h)? else {
            continue;
        };
        if blockstore.has_block_body(&hash)? {
            return Ok(h);
        }
    }
    Ok(0)
}

/// Contiguous on-disk body tip of the store's **actual** body range.
///
/// Finds the first stored body (any start height), then walks until the first
/// hole. Does not assume contiguity from height 1 and does not use sparse-max.
pub fn contiguous_body_range_tip(blockstore: &BlockStore) -> Result<u64> {
    let start = first_stored_body_height(blockstore)?;
    if start == 0 {
        return Ok(0);
    }
    contiguous_body_tip_from(blockstore, start)
}

pub fn contiguous_body_tip_from(blockstore: &BlockStore, from: u64) -> Result<u64> {
    if from == 0 {
        return Ok(0);
    }
    let Some(hash) = blockstore.get_hash_by_height(from)? else {
        return Ok(from.saturating_sub(1));
    };
    if !blockstore.has_block_body(&hash)? {
        return Ok(from.saturating_sub(1));
    }
    let mut tip = from;
    loop {
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

/// Lowest height that must still persist body+undo on a pruned dest.
///
/// `0` = archive / no window: never skip for prune. Otherwise skip flush when
/// `height < horizon` (i.e. `height + window < header_tip`). Persist
/// `[header_tip − window, header_tip]`. GAP_PERSIST uses the same horizon:
/// a body the keep window will not retain is not written.
pub fn pruned_midchain_skip_horizon(header_tip: u64, prune_window: u64) -> u64 {
    if prune_window == 0 || header_tip <= prune_window {
        return 0;
    }
    header_tip.saturating_sub(prune_window)
}

/// Sliding keep window for incremental prune on the parallel IBD path.
///
/// `0` leaves archive persist unchanged: pruning disabled, or
/// `incremental_prune_during_ibd` off. Otherwise the window is
/// `max(prune_window_size, min_blocks_to_keep)`.
pub fn incremental_ibd_prune_window(
    incremental_during_ibd: bool,
    pruning_enabled: bool,
    prune_window_size: u64,
    min_blocks_to_keep: u64,
) -> u64 {
    if !incremental_during_ibd || !pruning_enabled {
        return 0;
    }
    let window = prune_window_size.max(min_blocks_to_keep);
    if window == 0 { 0 } else { window }
}

/// Header-tip horizon for the validation flush. `0` means do not skip for prune.
pub fn ibd_prune_horizon(header_tip: u64, end_height: u64, window: u64) -> u64 {
    if window == 0 {
        return 0;
    }
    pruned_midchain_skip_horizon(header_tip.max(end_height), window)
}

/// Height whose GAP_PERSIST body just left the keep window. `0` means do not delete.
pub fn ibd_prune_gc_height(validated: u64, window: u64, min_height: u64) -> u64 {
    if window == 0 || validated < min_height {
        return 0;
    }
    validated.saturating_sub(window)
}

/// Skip heed3 block-store flush when the body is already on disk (cheap `contains_key` — no
/// deserialize), or when a pruned dest will delete it (`height < prune_horizon`).
///
/// Used during sparse local gap replay when [`probe_confirmed_body_height`] is 0.
/// `prune_horizon == 0` keeps archive / warehouse / synth persist unchanged.
pub fn should_skip_block_store_write(
    blockstore: &BlockStore,
    height: u64,
    block_hash: &Hash,
    local_replay_max_height: u64,
    prune_horizon: u64,
) -> Result<bool> {
    if height > 0 && height <= local_replay_max_height {
        return Ok(true);
    }
    if prune_horizon > 0 && height < prune_horizon {
        return Ok(true);
    }
    blockstore.has_block_body(block_hash)
}

/// Publish the keep boundary used by validation flush and GAP_PERSIST.
/// `0` leaves archive persist unchanged.
pub fn publish_ibd_prune_horizon(horizon: u64) {
    IBD_PRUNE_HORIZON.store(horizon, Ordering::Relaxed);
}

/// Horizon last published by the validation loop. `0` before publish, and on archive dests.
pub fn published_prune_horizon() -> u64 {
    IBD_PRUNE_HORIZON.load(Ordering::Relaxed)
}

/// Same bound as the validation flush: `horizon == 0` keeps archive persist,
/// and `height == horizon` is still inside the keep window.
pub(crate) fn height_below_prune_horizon(height: u64, horizon: u64) -> bool {
    horizon > 0 && height < horizon
}

pub fn gc_pruned_window_gap_persist(blockstore: &BlockStore, gc_height: u64) -> Result<bool> {
    if gc_height == 0 {
        return Ok(false);
    }
    let Some(hash) = blockstore.get_hash_by_height(gc_height)? else {
        return Ok(false);
    };
    if !blockstore.has_block_body(&hash)? {
        return Ok(false);
    }
    blockstore.remove_block_body(&hash)?;
    let _ = blockstore.remove_witness(&hash);
    Ok(true)
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
    } else if let Some(w) = blockstore.get_witness(&block_hash)?
        && has_real_witnesses(&w)
    {
        return Ok(false);
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
            .clamp(1, 512)
    })
}

/// Whether GAP_PERSIST should write `height`.
///
/// On-disk predecessor (late desert fill after apply passed). Any height
/// strictly ahead of apply (farm / LEAD tiles). Height 0 never. The lookahead
/// env is not a refuse gate. Does not read `FLUSH_PATH_TIP`.
fn should_persist_gap_height(blockstore: &BlockStore, val_h: u64, height: u64) -> bool {
    if height == 0 {
        return false;
    }
    if height > val_h {
        return true;
    }
    // Late sequential hole fill after apply passed. Use on-disk predecessor,
    // not FLUSH_PATH_TIP (that atomic is leftover warehouse — R-196).
    if height == 1 {
        return true;
    }
    flush_path_height_has_body(blockstore, height.saturating_sub(1)).unwrap_or(false)
}

/// Persist body + witness for gap blocks that can extend the on-disk prefix.
///
/// Writes `flush_path_body_tip()+1` even after apply has passed, and any
/// height ahead of `validation_height`. Lookahead is not a refuse gate.
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
    match gap_persist_gate(
        blockstore,
        validation_height,
        height,
        block_hash,
        block,
        witnesses,
        protocol_version,
    )? {
        GapPersistGate::Skip => Ok(false),
        GapPersistGate::OnDisk(repaired) => Ok(repaired),
        GapPersistGate::Write => gap_persist_write_one(
            blockstore,
            height,
            block_hash,
            block,
            witnesses,
            wire_payload,
        ),
    }
}

/// R-348: outcome of the persist gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GapPersistGate {
    /// Outside the apply window, or W1 empty-witness-with-commitment: do not persist.
    Skip,
    /// Body already on disk; carries the witness-repair result (the old return value).
    OnDisk(bool),
    /// Body must be written (bincode or wire blob).
    Write,
}

/// R-348: the gate half of [`try_persist_gap_block_for_local_inject_with_wire`] — apply
/// window, W1 empty-witness check, and the body-already-on-disk repair path. Shared by the
/// inline path and the batched persist lane.
pub(crate) fn gap_persist_gate(
    blockstore: &BlockStore,
    validation_height: Option<&std::sync::Arc<std::sync::atomic::AtomicU64>>,
    height: u64,
    block_hash: Hash,
    block: &Block,
    witnesses: &[Vec<Witness>],
    protocol_version: ProtocolVersion,
) -> Result<GapPersistGate> {
    let Some(vh) = validation_height else {
        return Ok(GapPersistGate::Skip);
    };
    let val_h = vh.load(std::sync::atomic::Ordering::Relaxed);
    // Below the keep window the validation flush will not store this body, and
    // deleting it later takes the LMDB writer on the validation thread. Skip
    // before any blockstore call.
    let horizon = published_prune_horizon();
    if height_below_prune_horizon(height, horizon) {
        if !GAP_PERSIST_PRUNE_SKIP_LOGGED.swap(true, Ordering::Relaxed)
            || height.is_multiple_of(10_000)
        {
            info!("[IBD_GAP_PERSIST_PRUNE_SKIP] h={height} horizon={horizon} val={val_h}");
        }
        return Ok(GapPersistGate::Skip);
    }
    let lookahead = gap_persist_lookahead();
    // Persist-ahead (Phase 1): do not refuse on the apply window.
    // Step 0/1 already widened the clamp and still glued FLUSH_PATH_TIP to apply
    // (holes; inject chained 2). Two refuses made the prefix:
    //   * height > val+lookahead — farm / LEAD tiles never hit disk, so
    //     catch_up cannot walk when the desert fills.
    //   * height <= val — late tip+1 (apply outran GetData) never lands, so
    //     the contiguous store tip stays at stall−1.
    // GC is still behind val−144.
    //
    // Do **not** call `note_flush_path_bodies` here. Coordinator copies that
    // atomic into `live_body_tip` / leftover. R-196: persist published tip 32
    // at apply 1 → `LOCAL_AHEAD` + leftover inject + C1j abort; stall 11011.
    // Persist writes disk for LOCAL_GAP. It is not the leftover warehouse.
    if !should_persist_gap_height(blockstore, val_h, height) {
        return Ok(GapPersistGate::Skip);
    }
    if height > val_h.saturating_add(lookahead) {
        debug!(
            "[IBD_GAP_PERSIST] height {} outside lookahead {} (val={}) — persist-ahead",
            height, lookahead, val_h
        );
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
        return Ok(GapPersistGate::Skip);
    }
    if blockstore.has_block_body(&block_hash)? {
        // Body present (prior MSG_BLOCK / partial persist) — still need a real witness
        // for segwit heights or inject returns WitnessMissing and the chain breaks.
        let repaired = try_repair_missing_witness(
            blockstore,
            height,
            block_hash,
            witnesses,
            protocol_version,
            Some(block),
        )?;
        return Ok(GapPersistGate::OnDisk(repaired));
    }
    Ok(GapPersistGate::Write)
}

/// R-348: the write half — wire blob when `BLVM_IBD_WIRE_BYTES_STORE=1` and the payload is
/// here, else bincode via `store_block_with_witness` (one LMDB txn).
pub(crate) fn gap_persist_write_one(
    blockstore: &BlockStore,
    height: u64,
    block_hash: Hash,
    block: &Block,
    witnesses: &[Vec<Witness>],
    wire_payload: Option<&[u8]>,
) -> Result<bool> {
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

/// Apply takes H from the body store when the feeder is empty.
///
/// Default **off**. R-194b dump **192** vs R-193 **2278** with default on.
/// Opt in with `BLVM_IBD_STORE_APPLY=1`. One height only — not inject-2048.
pub fn store_apply_enabled() -> bool {
    latch_env!(bool, {
        matches!(
            std::env::var("BLVM_IBD_STORE_APPLY").as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE") | Ok("on") | Ok("yes")
        )
    })
}

static STORE_APPLY_N: AtomicU64 = AtomicU64::new(0);

pub fn store_apply_count() -> u64 {
    STORE_APPLY_N.load(Ordering::Relaxed)
}

pub fn note_store_apply(height: u64) {
    let n = STORE_APPLY_N
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    if n <= 3 || n.is_multiple_of(256) {
        info!("[IBD_STORE_APPLY] h={} n={}", height, n);
    }
}

/// Load block + witnesses from disk when complete for IBD replay (skips network).
pub fn leftover_stall_try_load(
    blockstore: &BlockStore,
    height: u64,
    protocol_version: ProtocolVersion,
) -> Result<Option<(SharedBlock, SharedWitnesses)>> {
    let Some(expected_hash) = blockstore.get_hash_by_height(height)? else {
        return Ok(None);
    };
    // Header-without-body must miss on contains_key (live 70736 heed3 get hang).
    if !blockstore.has_block_body(&expected_hash)? {
        return Ok(None);
    }
    Ok(
        try_load_local_ibd_block(blockstore, height, expected_hash, protocol_version)?
            .map(|(block, witnesses)| (Arc::new(block), Arc::new(witnesses))),
    )
}

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
                static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let prev = LAST.load(std::sync::atomic::Ordering::Relaxed);
                if now.saturating_sub(prev) >= 15 {
                    LAST.store(now, std::sync::atomic::Ordering::Relaxed);
                    super::memory::log_pressure_behavior(
                        "stall_abort_gap_fetch",
                        "abort",
                        "reason=pressure_critical_or_worse",
                    );
                }
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
    // Match persist default 128. R-197/R-198 INJECT_CHAIN max 64 was this cap, not
    // prefix length. Clamp stays 256 (do not widen; step 0/1 already spent 2048).
    latch_env!(u64, {
        std::env::var("BLVM_IBD_GAP_INJECT_LOOKAHEAD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(128)
            .clamp(1, 256)
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
        log_miss.remove(&height);
        return Ok(true); // already queued; caller can continue chaining
    }
    if already_dispatched.contains(&height) && !reload_if_dispatched {
        // Still in the pipeline (bridge/feeder/validation). Do not reload from disk.
        log_miss.remove(&height);
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
    log_miss.retain(|h| *h >= height);
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
    super::feeder_miss::note_inject(newly_injected, height, tip_in_pipeline);
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
#[path = "local_block_tests.rs"]
mod tests;
