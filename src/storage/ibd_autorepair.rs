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

pub fn validation_error_suggests_utxo_repair(err: &anyhow::Error) -> bool {
    let s = err.to_string();
    s.contains("UTXO not found for input")
        || s.contains("Invalid transaction inputs")
        || s.contains("IBD UTXO mutex poisoned")
        || s.contains("UTXO flush panicked")
        || s.contains("Failed to open IBD UTXO tree")
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
mod ibd_autorepair_tests {
    use super::*;
    use crate::storage::Storage;
    use tempfile::TempDir;

    #[test]
    fn apply_autorepair_soft_rolls_back_watermark_clears_marker() {
        // Default (no BLVM_IBD_AGGRESSIVE_REPAIR): marker is consumed, ibd_utxos preserved,
        // watermark rolled back one checkpoint interval for incremental replay.
        let _aggressive_guard = AggressiveRepairEnvGuard::cleared();
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let storage = Storage::new(data_dir).unwrap();

        storage
            .chain()
            .force_set_ibd_utxo_watermark(680_811)
            .unwrap();
        let tree = storage.open_tree("ibd_utxos").unwrap();
        tree.insert(b"tkey", b"tval").unwrap();
        storage.flush().unwrap();

        set_ibd_utxo_repair_flag(data_dir).unwrap();
        assert!(ibd_utxo_repair_flag_present(data_dir));

        apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

        assert!(
            !ibd_utxo_repair_flag_present(data_dir),
            "marker must be removed so later restarts do not loop"
        );
        assert_eq!(
            storage.chain().get_utxo_watermark().unwrap(),
            Some(680_600),
            "soft repair must roll back one 200-block interval"
        );
        assert!(
            !tree.is_empty().unwrap(),
            "soft repair must NOT wipe ibd_utxos — destructive wipe is opt-in via env"
        );
    }

    #[test]
    fn apply_autorepair_aggressive_wipes_state_on_env_flag() {
        let _aggressive_guard = AggressiveRepairEnvGuard::set();
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let storage = Storage::new(data_dir).unwrap();

        storage.chain().set_utxo_watermark(999).unwrap();
        let tree = storage.open_tree("ibd_utxos").unwrap();
        tree.insert(b"tkey", b"tval").unwrap();
        storage.flush().unwrap();

        set_ibd_utxo_repair_flag(data_dir).unwrap();
        apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

        assert!(!ibd_utxo_repair_flag_present(data_dir));
        assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(0));
        assert!(tree.is_empty().unwrap());
    }

    /// Lock around autorepair-related env vars so parallel tests do not leak.
    struct AutorepairEnvGuard;
    impl AutorepairEnvGuard {
        fn cleared() -> crate::ibd_test_lock::Guard {
            let g = crate::ibd_test_lock::guard();
            unsafe {
                std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
                std::env::remove_var("BLVM_IBD_DEFER_CHECKPOINT_INTERVAL");
                std::env::set_var("BLVM_IBD_ENGINE", "0");
            }
            g
        }
    }

    /// Lock around `BLVM_IBD_AGGRESSIVE_REPAIR` so the soft/aggressive tests don't race.
    /// Cargo runs tests in parallel by default; an env-set in one test would otherwise leak
    /// into the other.
    struct AggressiveRepairEnvGuard;
    impl AggressiveRepairEnvGuard {
        fn set() -> crate::ibd_test_lock::Guard {
            let g = AutorepairEnvGuard::cleared();
            unsafe {
                std::env::set_var("BLVM_IBD_AGGRESSIVE_REPAIR", "1");
                std::env::set_var("BLVM_IBD_ENGINE", "0");
            }
            g
        }
        fn cleared() -> crate::ibd_test_lock::Guard {
            AutorepairEnvGuard::cleared()
        }
    }

    #[test]
    fn apply_autorepair_no_op_when_marker_missing_preserves_watermark() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let storage = Storage::new(data_dir).unwrap();
        storage.chain().set_utxo_watermark(42).unwrap();

        apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

        assert!(!ibd_utxo_repair_flag_present(data_dir));
        assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(42));
    }

    #[test]
    fn validation_error_suggests_utxo_repair_matching_substrings() {
        assert!(validation_error_suggests_utxo_repair(&anyhow::anyhow!(
            "connect: UTXO not found for input x"
        )));
        assert!(validation_error_suggests_utxo_repair(&anyhow::anyhow!(
            "IBD UTXO mutex poisoned"
        )));
        assert!(!validation_error_suggests_utxo_repair(&anyhow::anyhow!(
            "bad peer disconnect"
        )));
    }

    #[test]
    fn apply_autorepair_soft_wipes_standalone_store_when_watermark_zero() {
        let _aggressive_guard = AggressiveRepairEnvGuard::cleared();
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let storage = Storage::new(data_dir).unwrap();
        storage.chain().force_set_ibd_utxo_watermark(0).unwrap();

        let utxo_store_dir = data_dir.join(crate::storage::database::IBD_UTXO_STORE_SUBDIR);
        std::fs::create_dir_all(&utxo_store_dir).unwrap();
        std::fs::write(utxo_store_dir.join("marker"), b"stale").unwrap();

        set_ibd_utxo_repair_flag(data_dir).unwrap();
        apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

        assert!(
            !utxo_store_dir.exists(),
            "wm=0 soft repair must wipe standalone ibd_utxo_store"
        );
        assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(0));
    }

    #[test]
    fn apply_autorepair_soft_engine_rolls_back_to_prev_slot_when_available() {
        let _guard = crate::ibd_test_lock::guard();
        unsafe {
            std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
            std::env::remove_var("BLVM_IBD_DEFER_CHECKPOINT_INTERVAL");
            std::env::set_var("BLVM_IBD_ENGINE", "1");
        }
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let storage = Storage::new(data_dir).unwrap();

        // Active slot 0 at 48702 is poisoned (meta count 5581); prev slot 1 at 40000 has a
        // plausible tree (enough rows for height/7).
        storage.chain().force_set_engine_ckpt_slot(0).unwrap();
        storage
            .chain()
            .set_engine_ckpt_slot_height(0, 48_702)
            .unwrap();
        storage
            .chain()
            .set_engine_ckpt_slot_height(1, 40_000)
            .unwrap();
        storage
            .chain()
            .force_set_ibd_utxo_watermark(48_702)
            .unwrap();
        storage
            .chain()
            .force_set_engine_export_height(48_702)
            .unwrap();
        storage
            .chain()
            .force_set_engine_export_utxo_count(5_581)
            .unwrap();
        let ckpt_a = storage.open_tree("ibd_utxos_ckpt_a").unwrap();
        ckpt_a.insert(b"poison", b"x").unwrap();
        let ckpt_b = storage.open_tree("ibd_utxos_ckpt_b").unwrap();
        // Plausible floor at h=40000 is height/7 ≈ 5714 — write enough keys.
        for i in 0..8_000u64 {
            let k = i.to_be_bytes();
            ckpt_b.insert(&k, b"y").unwrap();
        }
        storage.flush().unwrap();

        set_ibd_utxo_repair_flag(data_dir).unwrap();
        apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

        assert!(!ibd_utxo_repair_flag_present(data_dir));
        assert_eq!(storage.chain().get_engine_ckpt_slot().unwrap(), 1);
        assert_eq!(
            storage.chain().get_engine_export_height().unwrap(),
            Some(40_000)
        );
        assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(40_000));
        assert_eq!(
            storage.chain().get_engine_export_utxo_count().unwrap(),
            Some(8_000),
            "rollback must set export_utxo_count from prev tree len"
        );
        assert!(
            !ckpt_b.is_empty().unwrap(),
            "prev-slot rollback must preserve the good tree"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_ENGINE");
        }
    }

    #[test]
    fn apply_autorepair_soft_engine_keeps_healthy_active_slot() {
        // Live 2026-07-14 bug: soft repair preferred prev slot even when active looked fine,
        // abandoning a good 426k snapshot for an incomplete 418k tree.
        let _guard = crate::ibd_test_lock::guard();
        unsafe {
            std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
            std::env::remove_var("BLVM_IBD_DEFER_CHECKPOINT_INTERVAL");
            std::env::set_var("BLVM_IBD_ENGINE", "1");
        }
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let storage = Storage::new(data_dir).unwrap();

        storage.chain().force_set_engine_ckpt_slot(1).unwrap();
        storage
            .chain()
            .set_engine_ckpt_slot_height(1, 42_000)
            .unwrap();
        storage
            .chain()
            .set_engine_ckpt_slot_height(0, 40_000)
            .unwrap();
        storage
            .chain()
            .force_set_ibd_utxo_watermark(42_000)
            .unwrap();
        storage
            .chain()
            .force_set_engine_export_height(42_000)
            .unwrap();
        // Plausible count matching tree len.
        let n = 10_000u64;
        storage
            .chain()
            .force_set_engine_export_utxo_count(n)
            .unwrap();
        let ckpt_b = storage.open_tree("ibd_utxos_ckpt_b").unwrap();
        for i in 0..n {
            let k = i.to_be_bytes();
            ckpt_b.insert(&k, b"y").unwrap();
        }
        let ckpt_a = storage.open_tree("ibd_utxos_ckpt_a").unwrap();
        ckpt_a.insert(b"old", b"x").unwrap();
        storage.flush().unwrap();

        set_ibd_utxo_repair_flag(data_dir).unwrap();
        apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

        assert!(!ibd_utxo_repair_flag_present(data_dir));
        assert_eq!(
            storage.chain().get_engine_ckpt_slot().unwrap(),
            1,
            "healthy active must not roll to prev slot"
        );
        assert_eq!(
            storage.chain().get_engine_export_height().unwrap(),
            Some(42_000)
        );
        let wm = storage.chain().get_utxo_watermark().unwrap().unwrap_or(0);
        assert_eq!(
            wm, 42_000,
            "healthy active must keep watermark=export_h (no below-export rollback), got {wm}"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_ENGINE");
        }
    }

    #[test]
    fn apply_autorepair_soft_engine_raises_watermark_up_to_export() {
        // Live 2026-07-15: wm already below export (prior interval rollback) must clamp UP.
        let _guard = crate::ibd_test_lock::guard();
        unsafe {
            std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
            std::env::set_var("BLVM_IBD_ENGINE", "1");
        }
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let storage = Storage::new(data_dir).unwrap();

        storage.chain().force_set_engine_ckpt_slot(1).unwrap();
        storage
            .chain()
            .set_engine_ckpt_slot_height(1, 42_000)
            .unwrap();
        storage
            .chain()
            .force_set_ibd_utxo_watermark(41_250)
            .unwrap();
        storage
            .chain()
            .force_set_engine_export_height(42_000)
            .unwrap();
        let n = 10_000u64; // plausible for h=42k (need ≥ height/7)
        storage
            .chain()
            .force_set_engine_export_utxo_count(n)
            .unwrap();
        let ckpt_b = storage.open_tree("ibd_utxos_ckpt_b").unwrap();
        for i in 0..n {
            let k = i.to_be_bytes();
            ckpt_b.insert(&k, b"y").unwrap();
        }
        storage.flush().unwrap();

        set_ibd_utxo_repair_flag(data_dir).unwrap();
        apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

        assert!(!ibd_utxo_repair_flag_present(data_dir));
        assert_eq!(
            storage.chain().get_utxo_watermark().unwrap(),
            Some(42_000),
            "must raise watermark to export_h"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_ENGINE");
        }
    }

    #[test]
    fn apply_autorepair_soft_engine_wipes_when_utxo_count_implausible() {
        let _guard = crate::ibd_test_lock::guard();
        unsafe {
            std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
            std::env::set_var("BLVM_IBD_ENGINE", "1");
        }
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let storage = Storage::new(data_dir).unwrap();

        storage.chain().force_set_engine_ckpt_slot(0).unwrap();
        storage
            .chain()
            .set_engine_ckpt_slot_height(0, 48_702)
            .unwrap();
        storage
            .chain()
            .set_engine_ckpt_slot_height(1, 40_000)
            .unwrap();
        storage
            .chain()
            .force_set_ibd_utxo_watermark(48_702)
            .unwrap();
        storage
            .chain()
            .force_set_engine_export_height(48_702)
            .unwrap();
        // Record the live poison count (5581 @ 48k) so soft repair refuses both slots.
        storage
            .chain()
            .force_set_engine_export_utxo_count(5_581)
            .unwrap();
        let ckpt_a = storage.open_tree("ibd_utxos_ckpt_a").unwrap();
        ckpt_a.insert(b"poison", b"x").unwrap();
        let ckpt_b = storage.open_tree("ibd_utxos_ckpt_b").unwrap();
        ckpt_b.insert(b"also_poison", b"y").unwrap();
        storage.flush().unwrap();

        set_ibd_utxo_repair_flag(data_dir).unwrap();
        apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

        assert!(!ibd_utxo_repair_flag_present(data_dir));
        assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(0));
        assert_eq!(storage.chain().get_engine_export_height().unwrap(), Some(0));
        assert!(ckpt_a.is_empty().unwrap());
        assert!(ckpt_b.is_empty().unwrap());
        unsafe {
            std::env::remove_var("BLVM_IBD_ENGINE");
        }
    }

    #[test]
    fn checkpoint_utxo_count_plausible_rejects_live_poison() {
        assert!(!checkpoint_utxo_count_plausible(40_000, 5_581));
        assert!(!checkpoint_utxo_count_plausible(48_702, 5_581));
        assert!(checkpoint_utxo_count_plausible(40_000, 80_000));
        assert!(
            !checkpoint_utxo_count_plausible(40_000, 0),
            "count==0 is refuse, not unset-OK"
        );
        // dest-ak 8.2M@516k passed height/7 (~74k); must be poison.
        assert!(!checkpoint_utxo_count_plausible(516_773, 8_236_260));
        assert!(!checkpoint_utxo_count_monotonic(
            516_773, 8_236_260, 16_800_000
        ));
        assert!(checkpoint_utxo_count_plausible(340_000, 16_800_000));
        // Overlay 3.6M@340k with no predecessor is tee/50%, not height*18.
        // A 100k×18 floor would refuse genesis ~1M@221k and abort the post-fix soak.
        assert!(checkpoint_utxo_count_plausible(221_814, 1_154_316));
        assert!(checkpoint_utxo_count_plausible(340_000, 3_600_000));
        assert!(!checkpoint_utxo_count_monotonic(
            340_000, 3_600_000, 16_800_000
        ));
        assert!(!checkpoint_utxo_count_monotonic(
            200_000, 4_000_000, 16_800_000
        ));
        assert!(checkpoint_first_persist_acceptable(
            16_800_000, 16_800_000, true
        ));
        assert!(!checkpoint_first_persist_acceptable(3_600_000, 0, true));
        assert!(!checkpoint_first_persist_acceptable(
            3_600_000,
            200_000_000,
            true
        ));
        // dest-bc overlay persists vs last good 16.8M — 50% gate, not height/7.
        assert!(!checkpoint_persist_acceptable(
            452_846, 0, 16_800_000, 0, true, false
        ));
        assert!(!checkpoint_persist_acceptable(
            340_000, 3_600_000, 16_800_000, 0, true, false
        ));
        assert!(checkpoint_persist_acceptable(
            340_000, 16_800_000, 16_800_000, 16_800_000, true, false
        ));
        assert!(
            checkpoint_persist_acceptable(221_814, 1_154_316, 0, 1_154_316, true, true),
            "genesis first persist ~1M@221k must not trip height*18"
        );
        assert!(checkpoint_persist_acceptable(
            265_774, 2_822_399, 1_154_316, 2_822_399, true, false
        ));
        assert!(!checkpoint_refuse_hard_stop(2));
        assert!(checkpoint_refuse_hard_stop(3));
        assert!(checkpoint_refuse_hard_stop(4));
    }

    #[test]
    fn apply_autorepair_dest_ak_8m_at_516k_must_not_look_healthy() {
        // dest-ak resumed 8236260 @516773 (passes height/7) then MISSING_UTXO.
        // Trees are gone on disk; this fixture reconstructs the metadata+stub tree.
        // Must not take "looks healthy — align watermark and re-seed".
        let _guard = crate::ibd_test_lock::guard();
        unsafe {
            std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
            std::env::remove_var("BLVM_IBD_DEFER_CHECKPOINT_INTERVAL");
            std::env::set_var("BLVM_IBD_ENGINE", "1");
        }
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let storage = Storage::new(data_dir).unwrap();

        storage.chain().force_set_engine_ckpt_slot(0).unwrap();
        storage
            .chain()
            .set_engine_ckpt_slot_height(0, 516_773)
            .unwrap();
        storage
            .chain()
            .force_set_ibd_utxo_watermark(516_773)
            .unwrap();
        storage
            .chain()
            .force_set_engine_export_height(516_773)
            .unwrap();
        storage
            .chain()
            .force_set_engine_export_utxo_count(8_236_260)
            .unwrap();
        storage
            .chain()
            .force_set_engine_prev_accepted_utxo_count(16_800_000)
            .unwrap();
        let ckpt_a = storage.open_tree("ibd_utxos_ckpt_a").unwrap();
        ckpt_a.insert(b"poison-stub", b"x").unwrap();
        storage.flush().unwrap();

        set_ibd_utxo_repair_flag(data_dir).unwrap();
        apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

        assert!(!ibd_utxo_repair_flag_present(data_dir));
        let export_h = storage
            .chain()
            .get_engine_export_height()
            .unwrap()
            .unwrap_or(0);
        let wm = storage.chain().get_utxo_watermark().unwrap().unwrap_or(0);
        assert_eq!(
            export_h, 0,
            "8.2M@516k must not re-seed; genesis reset, got export_h={export_h}"
        );
        assert_eq!(wm, 0, "must not align watermark to poison 516773, got {wm}");
        unsafe {
            std::env::remove_var("BLVM_IBD_ENGINE");
        }
    }

    #[test]
    fn apply_autorepair_soft_engine_wipes_poisoned_ckpt_when_no_prev_slot() {
        let _guard = crate::ibd_test_lock::guard();
        unsafe {
            std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
            std::env::remove_var("BLVM_IBD_DEFER_CHECKPOINT_INTERVAL");
            std::env::set_var("BLVM_IBD_ENGINE", "1");
        }
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let storage = Storage::new(data_dir).unwrap();

        storage
            .chain()
            .force_set_ibd_utxo_watermark(48_702)
            .unwrap();
        storage
            .chain()
            .force_set_engine_export_height(48_702)
            .unwrap();
        storage
            .chain()
            .persist_engine_validation_tip(5_800)
            .unwrap();
        let ckpt = storage.open_tree("ibd_utxos_ckpt_a").unwrap();
        ckpt.insert(b"ckpt", b"poison").unwrap();
        storage.flush().unwrap();

        set_ibd_utxo_repair_flag(data_dir).unwrap();
        apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

        assert!(!ibd_utxo_repair_flag_present(data_dir));
        assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(0));
        assert_eq!(storage.chain().get_engine_export_height().unwrap(), Some(0));
        assert!(
            ckpt.is_empty().unwrap(),
            "soft engine repair with no prior slot must wipe poisoned ckpt trees"
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_ENGINE");
        }
    }

    #[test]
    fn soft_repair_rollback_watermark_one_interval() {
        assert_eq!(soft_repair_rollback_watermark(680_811, 200), 680_600);
        assert_eq!(soft_repair_rollback_watermark(680_600, 200), 680_400);
        assert_eq!(soft_repair_rollback_watermark(200, 200), 0);
        assert_eq!(soft_repair_rollback_watermark(0, 200), 0);
        // On-boundary watermark rolls to the previous boundary.
        assert_eq!(soft_repair_rollback_watermark(680_800, 200), 680_600);
    }

    #[test]
    fn reconcile_resets_watermark_when_ibd_utxos_empty() {
        let dir = TempDir::new().unwrap();
        let storage = Storage::new(dir.path()).unwrap();
        storage
            .chain()
            .force_set_ibd_utxo_watermark(418_000)
            .unwrap();
        assert_eq!(
            reconcile_ibd_utxo_watermark_with_disk(&storage, 418_000).unwrap(),
            0
        );
        assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(0));
    }

    #[test]
    fn reconcile_keeps_watermark_when_ibd_utxos_nonempty() {
        let dir = TempDir::new().unwrap();
        let storage = Storage::new(dir.path()).unwrap();
        storage.chain().force_set_ibd_utxo_watermark(100).unwrap();
        storage
            .open_tree("ibd_utxos")
            .unwrap()
            .insert(b"k", b"v")
            .unwrap();
        assert_eq!(
            reconcile_ibd_utxo_watermark_with_disk(&storage, 100).unwrap(),
            100
        );
    }

    #[test]
    fn reconcile_resets_when_export_label_has_empty_ckpt_and_ibd_utxos_rows() {
        let _guard = crate::ibd_test_lock::guard();
        unsafe {
            std::env::set_var("BLVM_IBD_ENGINE", "1");
        }
        let dir = TempDir::new().unwrap();
        let storage = Storage::new(dir.path()).unwrap();
        storage.chain().force_set_engine_ckpt_slot(0).unwrap();
        storage
            .chain()
            .force_set_engine_export_height(155_499)
            .unwrap();
        storage
            .chain()
            .force_set_ibd_utxo_watermark(155_499)
            .unwrap();
        storage
            .open_tree("ibd_utxos")
            .unwrap()
            .insert(b"k", b"v")
            .unwrap();
        assert!(storage
            .open_tree("ibd_utxos_ckpt_a")
            .unwrap()
            .is_empty()
            .unwrap());

        assert_eq!(
            reconcile_ibd_utxo_watermark_with_disk(&storage, 155_499).unwrap(),
            0
        );
        assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(0));
        assert_eq!(storage.chain().get_engine_export_height().unwrap(), Some(0));
        unsafe {
            std::env::remove_var("BLVM_IBD_ENGINE");
        }
    }

    #[test]
    fn reconcile_keeps_watermark_when_phase3_canonical_ibd_utxos_is_set() {
        let _guard = crate::ibd_test_lock::guard();
        unsafe {
            std::env::set_var("BLVM_IBD_ENGINE", "1");
        }
        let dir = TempDir::new().unwrap();
        let storage = Storage::new(dir.path()).unwrap();
        storage.chain().force_set_engine_ckpt_slot(0).unwrap();
        storage
            .chain()
            .force_set_engine_export_height(155_512)
            .unwrap();
        storage
            .chain()
            .force_set_ibd_utxo_watermark(155_512)
            .unwrap();
        storage
            .chain()
            .set_ibd_utxo_canonical_tree("ibd_utxos")
            .unwrap();
        storage
            .open_tree("ibd_utxos")
            .unwrap()
            .insert(b"k", b"v")
            .unwrap();
        assert!(storage
            .open_tree("ibd_utxos_ckpt_a")
            .unwrap()
            .is_empty()
            .unwrap());

        assert_eq!(
            reconcile_ibd_utxo_watermark_with_disk(&storage, 155_512).unwrap(),
            155_512
        );
        assert_eq!(
            storage.chain().get_engine_export_height().unwrap(),
            Some(155_512)
        );
        let snap = storage.open_engine_resume_snapshot().unwrap();
        let (name, tree) = snap.expect("canonical snapshot");
        assert_eq!(name, "ibd_utxos");
        assert!(!tree.is_empty().unwrap());
        unsafe {
            std::env::remove_var("BLVM_IBD_ENGINE");
        }
    }

    #[test]
    fn resume_snapshot_name_ignores_default_ibd_utxos() {
        assert_eq!(
            crate::storage::engine_resume_snapshot_name("ibd_utxos_ckpt_a", true, None, false),
            Some("ibd_utxos_ckpt_a")
        );
        assert_eq!(
            crate::storage::engine_resume_snapshot_name("ibd_utxos_ckpt_a", false, None, true),
            None
        );
        assert_eq!(
            crate::storage::engine_resume_snapshot_name(
                "ibd_utxos_ckpt_a",
                false,
                Some("ibd_utxos"),
                true
            ),
            Some("ibd_utxos")
        );
        assert_eq!(
            crate::storage::engine_resume_snapshot_name(
                "ibd_utxos_ckpt_a",
                false,
                Some("ibd_utxos_ckpt_a"),
                true
            ),
            None
        );
        assert_eq!(
            crate::storage::engine_resume_snapshot_name(
                "ibd_utxos_ckpt_a",
                false,
                Some("ibd_utxos"),
                false
            ),
            None
        );
    }
}
