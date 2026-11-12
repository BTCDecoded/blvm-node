//! Marker-driven IBD UTXO autorepair: after a validation/UTXO consistency failure we write
//! `ibd_utxo_repair_required`. On the **next** startup the default **soft** path rolls the
//! durable watermark back by one formal checkpoint interval and clears the marker so IBD
//! re-validates from the lowered height — rebuilding the in-memory UTXO view incrementally
//! on top of the persisted LMDB rows below that boundary. This recovers cold-resume
//! `UTXO_TOTAL_MISS` at `watermark+1` without a manual rollback or full wipe.
//!
//! **Default is non-destructive.** The previous default (full `ibd_utxos.clear()` +
//! `watermark = 0`) cost ≥40 k blocks of re-validation per crash on real workloads even
//! when the on-disk state was healthy below the watermark. The wipe-everything path is
//! preserved behind `BLVM_IBD_AGGRESSIVE_REPAIR=1` for cases where corruption persists
//! through replay (which would re-trigger the same error and re-set the marker).
//!
//! - `BLVM_IBD_SKIP_AUTOREPAIR=1`: do nothing (marker stays until deleted manually). Use
//!   when you want to inspect on-disk state without any auto-action.
//! - `BLVM_IBD_AGGRESSIVE_REPAIR=1`: pre-existing destructive wipe (`ibd_utxos.clear()` +
//!   `watermark = 0`). Use only if the soft repair loops because corruption is below the
//!   watermark.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

const MARKER_FILE: &str = "ibd_utxo_repair_required";

pub fn repair_marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MARKER_FILE)
}

pub fn set_ibd_utxo_repair_flag(data_dir: &Path) -> Result<()> {
    let path = repair_marker_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, b"1").context("write ibd_utxo_repair_required")?;
    warn!(
        "Wrote {} — next startup will roll back ibd_utxo_watermark one checkpoint interval \
         and replay unless BLVM_IBD_SKIP_AUTOREPAIR is set",
        path.display()
    );
    Ok(())
}

pub fn clear_ibd_utxo_repair_flag(data_dir: &Path) -> Result<()> {
    let path = repair_marker_path(data_dir);
    if path.exists() {
        std::fs::remove_file(&path).context("remove ibd_utxo_repair_required")?;
        info!("Removed IBD UTXO repair marker ({})", path.display());
    }
    Ok(())
}

pub fn ibd_utxo_repair_flag_present(data_dir: &Path) -> bool {
    repair_marker_path(data_dir).exists()
}

/// Remove the standalone `ibd_utxo_store/` LMDB and clear the legacy `ibd_utxos` tree in main
/// storage. Block bodies in `heed3/` are untouched — local replay rebuilds UTXO state.
#[cfg(feature = "production")]
pub fn wipe_standalone_ibd_utxo_store(
    storage: &crate::storage::Storage,
    data_dir: &Path,
) -> Result<()> {
    let utxo_store_dir = data_dir.join(crate::storage::database::IBD_UTXO_STORE_SUBDIR);
    if utxo_store_dir.exists() {
        std::fs::remove_dir_all(&utxo_store_dir).with_context(|| {
            format!(
                "remove standalone IBD UTXO store {}",
                utxo_store_dir.display()
            )
        })?;
        info!(
            "[ibd_autorepair] removed {} (standalone UTXO LMDB wiped)",
            utxo_store_dir.display()
        );
    }
    if let Ok(tree) = storage.open_tree("ibd_utxos") {
        tree.clear()?;
    }
    Ok(())
}

/// Best-effort classification: errors where clearing `ibd_utxos` and replaying from on-disk blocks may help.
///
/// Uses stable substrings from `blvm-consensus` connect paths and parallel IBD — not generic
/// "invalid block" text (consensus bugs and bad peers would otherwise trigger destructive repair).
/// Formal checkpoint interval used for soft-repair watermark rollback (matches MemoryGuard default).
pub fn ibd_defer_checkpoint_interval_for_repair() -> u64 {
    std::env::var("BLVM_IBD_DEFER_CHECKPOINT_INTERVAL")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| (20..=500_000).contains(&n))
        .unwrap_or(200)
}

/// Roll watermark back one full formal checkpoint interval.
///
/// Example: `680811` with interval `200` → `680600` (~211 blocks of incremental replay).
pub fn soft_repair_rollback_watermark(watermark: u64, interval: u64) -> u64 {
    if watermark == 0 || interval == 0 {
        return 0;
    }
    let floor = (watermark / interval) * interval;
    floor.saturating_sub(interval)
}

/// Marker on a validation error that is a double-spend of a block already connected.
///
/// `validation_error_suggests_utxo_repair` must not arm a snapshot rollback for this.
/// av2: block 157771 spent vout 1023, block 157772 spent it again, and the repair
/// marker would roll the watermark to 0.
pub const DOUBLE_SPEND_OF_CONNECTED_BLOCK: &str = "double-spend of a connected block";

pub fn validation_error_suggests_utxo_repair(err: &anyhow::Error) -> bool {
    let chain = format!("{err:#}");
    if chain.contains(DOUBLE_SPEND_OF_CONNECTED_BLOCK) {
        return false;
    }
    let s = err.to_string();
    s.contains("UTXO not found for input")
        || s.contains("Invalid transaction inputs")
        || s.contains("IBD UTXO mutex poisoned")
        || s.contains("UTXO flush panicked")
        || s.contains("Failed to open IBD UTXO tree")
}

/// `UTXO not found for input N (prevout HEX:VOUT tx_idx=M)` — HEX is internal byte order.
pub fn utxo_miss_prevout(err: &anyhow::Error) -> Option<(blvm_protocol::Hash, u32)> {
    let chain = format!("{err:#}");
    let marker = "prevout ";
    let start = chain.find(marker)? + marker.len();
    let rest = &chain[start..];
    let end = rest.find(" tx_idx=").unwrap_or(rest.len());
    let spec = rest[..end].trim();
    let (hex_s, vout_s) = spec.rsplit_once(':')?;
    if hex_s.len() != 64 {
        return None;
    }
    let mut hash = [0u8; 32];
    for i in 0..32 {
        hash[i] = u8::from_str_radix(&hex_s[i * 2..i * 2 + 2], 16).ok()?;
    }
    let vout = vout_s.parse().ok()?;
    Some((hash, vout))
}

/// True when `block` has an input that spends this outpoint.
pub fn block_spends_prevout(block: &blvm_protocol::Block, txid: &[u8; 32], vout: u32) -> bool {
    block.transactions.iter().any(|tx| {
        tx.inputs
            .iter()
            .any(|inp| inp.prevout.hash == *txid && inp.prevout.index == vout)
    })
}

/// Reject incomplete / truncated checkpoint snapshots.
///
/// dest-bc persisted 3.6M / 0 / 2.4M after fan-in no-op because `height/7` is too
/// weak (516773/7 ≈ 74k lets dest-ak 8.2M through). Repair and persist share this
/// gate: refuse `count==0`; refuse `count < last_accepted/2`; when there is no
/// predecessor, at h≥400k refuse `count < height*18` (8.2M@516k fails; a genesis
/// ~1M@221k must pass — a 100k height*18 floor would refuse the next soak).
pub fn checkpoint_utxo_count_plausible(height: u64, count: u64) -> bool {
    checkpoint_utxo_count_monotonic(height, count, 0)
}

/// Same as [`checkpoint_utxo_count_plausible`] plus the 50% drop vs last accepted.
pub fn checkpoint_utxo_count_monotonic(height: u64, count: u64, last_accepted: u64) -> bool {
    if height == 0 {
        return true;
    }
    if count == 0 {
        return false;
    }
    if height < 10_000 {
        return count > 0;
    }
    if last_accepted > 0 && count < last_accepted / 2 {
        return false;
    }
    // dest-ak 8.2M @516773 passed height/7 (~74k). Absolute floor only when
    // there is no last_accepted and height is past the 2015-era UTXO ramp —
    // dest-bc 1.15M@221k is real-enough genesis; 100k×18 would refuse it.
    if last_accepted == 0 && height >= 400_000 && count < height.saturating_mul(18) {
        return false;
    }
    if last_accepted == 0 && height < 100_000 {
        return count >= height / 7;
    }
    true
}

/// First persist (no predecessor): refuse fan-in no-op overlay and order-of-magnitude
/// drops vs what the tee actually merged.
pub fn checkpoint_first_persist_acceptable(
    count: u64,
    tee_merged_entries: u64,
    disk_segs_existed: bool,
) -> bool {
    if disk_segs_existed && tee_merged_entries == 0 {
        return false;
    }
    if tee_merged_entries > 0 && count.saturating_mul(10) < tee_merged_entries {
        return false;
    }
    true
}

/// Combined persist gate used by the export thread.
pub fn checkpoint_persist_acceptable(
    height: u64,
    count: u64,
    last_accepted: u64,
    tee_merged_entries: u64,
    disk_segs_existed: bool,
    is_first: bool,
) -> bool {
    if !checkpoint_utxo_count_monotonic(height, count, last_accepted) {
        return false;
    }
    if is_first
        && !checkpoint_first_persist_acceptable(count, tee_merged_entries, disk_segs_existed)
    {
        return false;
    }
    true
}

/// 3 consecutive refused persists stop the export path. Does **not** set
/// `ibd_utxo_repair_required` — that re-seeds (dest-ak poison).
pub const CKPT_REFUSE_HARD_STOP: u32 = 3;

pub fn checkpoint_refuse_hard_stop(streak: u32) -> bool {
    streak >= CKPT_REFUSE_HARD_STOP
}

/// If `ibd_utxo_watermark` is non-zero but the durable UTXO tree has no rows, persisted watermark
/// cannot reflect flushed state. Reset watermark to **0** so startup uses safe replay semantics.
///
/// In engine mode (`BLVM_IBD_ENGINE=1`) checkpoints live in ping-pong trees (`ibd_utxos_ckpt_*`),
/// not `ibd_utxos` — skip the empty-tree reset when a checkpoint snapshot is present.
#[cfg(feature = "production")]
pub(crate) fn reconcile_ibd_utxo_watermark_with_disk(
    storage: &crate::storage::Storage,
    watermark_val: u64,
) -> Result<u64> {
    if watermark_val == 0 {
        return Ok(0);
    }
    let engine_enabled = crate::config::ibd::ibd_engine_enabled(None);
    // Export height names a snapshot. The active ckpt is that snapshot when it
    // has rows. Phase 3 full watermark export clears both ckpt trees and stores
    // the snapshot in `ibd_utxos` with `ibd_utxo_canonical_tree` set — that key
    // must keep the watermark. Stray `ibd_utxos` rows with the key unset are
    // not a snapshot (testnet4: export_h=155499, ckpt_a empty).
    let mut export_ckpt_missing = false;
    if engine_enabled {
        if let Some(eh) = storage
            .chain()
            .get_engine_export_height()?
            .filter(|&h| h > 0)
        {
            let slot = storage.chain().get_engine_ckpt_slot()?;
            let ckpt_name = crate::storage::ibd_engine::ckpt_tree_for_slot(slot);
            let ckpt_has_rows = storage
                .open_tree(ckpt_name)
                .ok()
                .and_then(|tree| tree.is_empty().ok())
                == Some(false);
            let stored = storage.chain().get_stored_ibd_utxo_canonical_tree()?;
            let canonical_has_rows = match stored.as_deref() {
                Some(name) if name != ckpt_name => {
                    storage
                        .open_tree(name)
                        .ok()
                        .and_then(|tree| tree.is_empty().ok())
                        == Some(false)
                }
                _ => false,
            };
            if crate::storage::engine_resume_snapshot_name(
                ckpt_name,
                ckpt_has_rows,
                stored.as_deref(),
                canonical_has_rows,
            )
            .is_some()
            {
                if !ckpt_has_rows {
                    // Catch-up calls resume every 5s. Phase 3 leaves the ckpt
                    // empty for the life of the process, so log the keep once.
                    static LOGGED_CANONICAL_KEEP: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !LOGGED_CANONICAL_KEEP.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        info!(
                            "[ibd_autorepair] engine export height {eh} but {ckpt_name} empty — \
                             keeping watermark; canonical {} has the snapshot",
                            stored.unwrap_or_default()
                        );
                    } else {
                        debug!(
                            "[ibd_autorepair] engine export height {eh} but {ckpt_name} empty — \
                             keeping watermark; canonical {} has the snapshot",
                            stored.unwrap_or_default()
                        );
                    }
                }
                return Ok(watermark_val);
            }
            warn!(
                "[ibd_autorepair] engine export height {eh} but {ckpt_name} empty — \
                 resetting watermark to 0 (no stored canonical snapshot)"
            );
            export_ckpt_missing = true;
        }
        if !export_ckpt_missing {
            if let Some(root) = storage.data_dir() {
                if crate::storage::database::legacy_ibd_utxo_standalone_has_data(&root) {
                    return Ok(watermark_val);
                }
            }
            let tree = storage.open_tree("ibd_utxos")?;
            if !tree.is_empty()? {
                return Ok(watermark_val);
            }
            warn!(
                "[ibd_autorepair] engine mode: ibd_utxo_watermark={} but no engine ckpt, legacy \
                 standalone, or main ibd_utxos data — resetting durable engine metadata to 0",
                watermark_val
            );
        }
        storage.chain().force_reset_engine_checkpoint_metadata()?;
        storage.chain().force_set_ibd_utxo_watermark(0)?;
        // Also wipe leftover flat engine files. Leaving segments/sidecar with a high
        // contiguous_length while metadata says export_h=0 reopens CL ahead of validation
        // (live 2026-07-14: CL=49716 → poisoned export at 40000).
        if let Some(root) = storage.data_dir() {
            let engine_dir = root.join("ibd_engine");
            if engine_dir.exists() {
                match std::fs::remove_dir_all(&engine_dir) {
                    Ok(()) => info!(
                        "[ibd_autorepair] cleared {} after metadata reset to 0",
                        engine_dir.display()
                    ),
                    Err(e) => warn!(
                        "[ibd_autorepair] failed to clear {} after metadata reset: {e}",
                        engine_dir.display()
                    ),
                }
            }
        }
        return Ok(0);
    }
    // Legacy path: open standalone LMDB when present to validate watermark against durable rows.
    let standalone_non_empty: bool = if let Some(root) = storage.data_dir() {
        let utxo_store_dir = root.join(crate::storage::database::IBD_UTXO_STORE_SUBDIR);
        if utxo_store_dir.exists() {
            match crate::storage::database::create_ibd_utxo_standalone_db(&utxo_store_dir) {
                Ok(db) => match db.open_tree("ibd_utxos") {
                    Ok(t) => !t.is_empty().unwrap_or(true),
                    Err(_) => false,
                },
                Err(_) => false,
            }
        } else {
            false
        }
    } else {
        false
    };

    let tree = storage.open_tree("ibd_utxos")?;
    let main_empty = tree.is_empty()?;

    if standalone_non_empty || !main_empty {
        // At least one storage location has UTXO data consistent with the watermark.
        return Ok(watermark_val);
    }
    warn!(
        "[ibd_autorepair] ibd_utxo_watermark={} but ibd_utxos tree is empty in both \
         standalone store and main storage — resetting watermark to 0 \
         (watermark likely jumped ahead of durable UTXO flushes)",
        watermark_val
    );
    storage.chain().force_set_ibd_utxo_watermark(0)?;
    Ok(0)
}

#[cfg(feature = "production")]
pub fn apply_ibd_utxo_autorepair_if_needed(
    storage: &crate::storage::Storage,
    data_dir: &Path,
) -> Result<()> {
    if std::env::var("BLVM_IBD_SKIP_AUTOREPAIR").is_ok() {
        if ibd_utxo_repair_flag_present(data_dir) {
            warn!(
                "IBD UTXO repair marker present but BLVM_IBD_SKIP_AUTOREPAIR is set — leaving everything as-is"
            );
        }
        return Ok(());
    }
    if !ibd_utxo_repair_flag_present(data_dir) {
        return Ok(());
    }

    let aggressive = std::env::var("BLVM_IBD_AGGRESSIVE_REPAIR")
        .map(|v| v == "1")
        .unwrap_or(false);

    if aggressive {
        // Destructive path: wipe the UTXO state and reset to height 0. On-disk blocks are
        // kept, so only consensus validation needs to be re-run.
        let engine_mode = crate::config::ibd::ibd_engine_enabled(None);

        if engine_mode {
            // Engine mode: checkpoints live in ibd_utxos_ckpt_a/b, not ibd_utxos.
            // Clear both ping-pong trees, reset engine metadata, then reset watermark.
            info!(
                "IBD UTXO autorepair (aggressive, engine mode): clearing checkpoint trees \
                 (ibd_utxos_ckpt_a + ibd_utxos_ckpt_b) and resetting engine metadata \
                 (BLVM_IBD_AGGRESSIVE_REPAIR=1, marker was present)"
            );
            for tree_name in &["ibd_utxos_ckpt_a", "ibd_utxos_ckpt_b"] {
                let tree = storage.open_tree(tree_name)?;
                tree.clear()?;
            }
            storage.chain().force_reset_engine_checkpoint_metadata()?;
        } else {
            info!(
                "IBD UTXO autorepair (aggressive): clearing ibd_utxos and forcing \
                 ibd_utxo_watermark to 0 (BLVM_IBD_AGGRESSIVE_REPAIR=1, marker was present)"
            );
            let tree = storage.open_tree("ibd_utxos")?;
            tree.clear()?;
        }
        storage.chain().force_set_ibd_utxo_watermark(0)?;
        storage.flush()?;
        clear_ibd_utxo_repair_flag(data_dir)?;
        warn!(
            "IBD UTXO autorepair applied (aggressive{}); on-disk blocks kept; full re-validation will follow",
            if engine_mode { ", engine mode" } else { "" }
        );
        return Ok(());
    }

    // Default soft repair: roll back durable watermark one checkpoint interval so the next
    // IBD session re-validates from the lowered height and rebuilds the hot UTXO cache on
    // top of LMDB rows that are still valid below that boundary. Preserves ibd_utxos on disk
    // (no wipe) — bounded replay of ~interval blocks per repair cycle.
    //
    // Exception: watermark already 0 means a full local replay — stale standalone LMDB from
    // prior wm=0 sessions without a wipe causes fdatasync bloat and ~50 BPS crawl (see
    // docs/IBD_BPS_RESOLUTION_PLAN.md PR-STORE).
    let engine_mode = crate::config::ibd::ibd_engine_enabled(None);
    let watermark = storage
        .chain()
        .get_utxo_watermark()
        .unwrap_or(None)
        .unwrap_or(0);
    if watermark == 0 {
        if engine_mode {
            if let Some(root) = storage.data_dir() {
                let engine_dir = root.join("ibd_engine");
                let _ = std::fs::remove_dir_all(&engine_dir);
            }
            info!(
                "[ibd_autorepair] engine mode wm=0: cleared ibd_engine/ flat state; \
                 replay from h=1 (block store intact)"
            );
        } else {
            wipe_standalone_ibd_utxo_store(storage, data_dir)?;
            info!(
                "[ibd_autorepair] wiped ibd_utxo_store (dirty wm=0 replay); replay from h=1 \
                 (block store intact)"
            );
        }
    }
    let interval = if engine_mode {
        crate::config::ibd::ibd_engine_checkpoint_interval_for_repair()
    } else {
        ibd_defer_checkpoint_interval_for_repair()
    };
    let export_height = if engine_mode {
        storage
            .chain()
            .get_engine_export_height()
            .unwrap_or(None)
            .unwrap_or(0)
    } else {
        0
    };
    if engine_mode {
        let export_utxo_count = storage
            .chain()
            .get_engine_export_utxo_count()
            .unwrap_or(None)
            .unwrap_or(0);
        let prev_accepted = storage
            .chain()
            .get_engine_prev_accepted_utxo_count()
            .unwrap_or(None)
            .unwrap_or(0);
        let active_slot = storage.chain().get_engine_ckpt_slot().unwrap_or(0);
        let active_tree_name = crate::storage::ibd_engine::ckpt_tree_for_slot(active_slot);
        let active_tree_len = storage
            .open_tree(active_tree_name)
            .ok()
            .and_then(|t| t.len().ok())
            .unwrap_or(0) as u64;
        // Soft engine repair: 50% vs last accepted (not height/7). dest-ak 8.2M@516k
        // passed height/7 and re-seeded poison. Use prev_accepted, else other-slot len.
        let mut gate_last = prev_accepted;
        let other_slot = if active_slot == 0 { 1u8 } else { 0u8 };
        if gate_last == 0 {
            if let Ok(tree) =
                storage.open_tree(crate::storage::ibd_engine::ckpt_tree_for_slot(other_slot))
            {
                gate_last = tree.len().unwrap_or(0) as u64;
            }
        }
        let meta_poisoned = export_height > 0
            && !checkpoint_utxo_count_monotonic(export_height, export_utxo_count, gate_last);
        let tree_poisoned = export_height > 0
            && (!checkpoint_utxo_count_monotonic(export_height, active_tree_len, gate_last)
                || (export_utxo_count > 0 && active_tree_len != export_utxo_count));
        let active_poisoned = meta_poisoned || tree_poisoned;
        if active_poisoned {
            warn!(
                "IBD UTXO autorepair (soft, engine): active export_h={export_height} slot={active_slot} \
                 meta_count={export_utxo_count} tree_len={active_tree_len} — poisoned \
                 (meta={meta_poisoned} tree={tree_poisoned})"
            );
            // Prefer any other slot with a plausible tree. Live 2026-07-14: soft repair had
            // already switched to incomplete slot 0 (418818) while slot 1 (426718) still held
            // the full snapshot — "prev_height < export_height" alone would miss it.
            let mut best: Option<(u8, u64, u64)> = None; // (slot, height, tree_len)
            for slot in [0u8, 1u8] {
                if slot == active_slot {
                    continue;
                }
                let h = storage
                    .chain()
                    .get_engine_ckpt_slot_height(slot)
                    .unwrap_or(0);
                if h == 0 {
                    continue;
                }
                if let Ok(tree) =
                    storage.open_tree(crate::storage::ibd_engine::ckpt_tree_for_slot(slot))
                {
                    let len = tree.len().unwrap_or(0) as u64;
                    if len > 0 && checkpoint_utxo_count_monotonic(h, len, 0) {
                        if best.map(|(_, bh, _)| h > bh).unwrap_or(true) {
                            best = Some((slot, h, len));
                        }
                    } else {
                        warn!(
                            "IBD UTXO autorepair (soft, engine): slot {slot} h={h} \
                             tree_len={len} unusable"
                        );
                    }
                }
            }
            if let Some((good_slot, good_height, good_len)) = best {
                storage.chain().force_set_engine_ckpt_slot(good_slot)?;
                storage
                    .chain()
                    .force_set_engine_export_height(good_height)?;
                storage
                    .chain()
                    .force_set_engine_export_utxo_count(good_len)?;
                storage.chain().force_set_ibd_utxo_watermark(good_height)?;
                if let Some(root) = storage.data_dir() {
                    let engine_dir = root.join("ibd_engine");
                    let _ = std::fs::remove_dir_all(&engine_dir);
                }
                storage.flush()?;
                clear_ibd_utxo_repair_flag(data_dir)?;
                warn!(
                    "IBD UTXO autorepair (soft, engine): switched to slot {good_slot} \
                     export_h={good_height} tree_len={good_len} (was export_h={export_height} \
                     slot {active_slot} tree_len={active_tree_len})"
                );
                return Ok(());
            }
            // No prior good ping-pong slot. Wipe both ckpt trees and reset metadata so the
            // next session replays from genesis over on-disk bodies.
            for tree_name in &["ibd_utxos_ckpt_a", "ibd_utxos_ckpt_b"] {
                if let Ok(tree) = storage.open_tree(tree_name) {
                    if let Err(e) = tree.clear() {
                        warn!("[ibd_autorepair] genesis reset: failed to clear {tree_name}: {e}");
                        return Err(e)
                            .context(format!("IBD UTXO autorepair: failed to clear {tree_name}"));
                    }
                }
            }
            storage.chain().force_reset_engine_checkpoint_metadata()?;
            storage.chain().force_set_ibd_utxo_watermark(0)?;
            if let Some(root) = storage.data_dir() {
                let engine_dir = root.join("ibd_engine");
                if engine_dir.exists() {
                    match std::fs::remove_dir_all(&engine_dir) {
                        Ok(()) => info!(
                            "[ibd_autorepair] soft engine repair: removed {} (reset to genesis)",
                            engine_dir.display()
                        ),
                        Err(e) => warn!(
                            "[ibd_autorepair] soft engine repair: failed to remove {}: {e}",
                            engine_dir.display()
                        ),
                    }
                }
            }
            storage.flush()?;
            clear_ibd_utxo_repair_flag(data_dir)?;
            warn!(
                "IBD UTXO autorepair (soft, engine): no prior good ckpt slot for \
                 export_h={export_height} — wiped ping-pong trees + engine flat state; \
                 next session replays from h=1 (block store intact)"
            );
            return Ok(());
        }
        // Active snapshot looks healthy (meta + tree len). Soft miss: do NOT roll watermark
        // below export_h in engine mode. Live 2026-07-15: interval rollback 543250→542500
        // while seed stayed at export_h=543250 → start_h=542501 with UTXO state at 543250 →
        // immediate UTXO-miss loop → ABRT. Engine resume always seeds a complete snapshot at
        // export_h and must continue at export_h+1.
        info!(
            "IBD UTXO autorepair (soft, engine): active export_h={export_height} \
             tree_len={active_tree_len} looks healthy — align watermark to export_h \
             (no below-export rollback)"
        );
        if watermark != export_height {
            storage
                .chain()
                .force_set_ibd_utxo_watermark(export_height)?;
            storage.flush()?;
            info!(
                "IBD UTXO autorepair (soft, engine): ibd_utxo_watermark {watermark} -> {export_height} \
                 (match export; next start_h={})",
                export_height.saturating_add(1)
            );
        }
        if let Some(root) = storage.data_dir() {
            let engine_dir = root.join("ibd_engine");
            let _ = std::fs::remove_dir_all(&engine_dir);
        }
        clear_ibd_utxo_repair_flag(data_dir)?;
        warn!(
            "IBD UTXO autorepair (soft, engine) applied: watermark=export_h={export_height}; \
             engine flat state cleared for re-seed; next IBD starts at {}",
            export_height.saturating_add(1)
        );
        return Ok(());
    }
    let rollback_base = watermark;
    let rolled_back = soft_repair_rollback_watermark(rollback_base, interval);
    if rolled_back < rollback_base {
        storage.chain().force_set_ibd_utxo_watermark(rolled_back)?;
        storage.flush()?;
        info!(
            "IBD UTXO autorepair (soft): rolled ibd_utxo_watermark {rollback_base} -> {rolled_back} \
             (interval={interval}); preserving ibd_utxos — replay from height {} will rebuild \
             UTXO cache incrementally",
            rolled_back.saturating_add(1)
        );
    } else {
        info!("IBD UTXO autorepair (soft): watermark already 0; preserving ibd_utxos");
    }
    clear_ibd_utxo_repair_flag(data_dir)?;
    warn!(
        "IBD UTXO autorepair (soft) applied; if the next IBD attempt re-trips the same UTXO error \
         and sets the marker again, another interval rollback will run (or set \
         BLVM_IBD_AGGRESSIVE_REPAIR=1 for the destructive wipe path)"
    );
    Ok(())
}

#[cfg(all(test, feature = "production"))]
#[path = "ibd_autorepair_tests.rs"]
mod ibd_autorepair_tests;
