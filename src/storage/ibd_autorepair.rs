//! Marker-driven IBD UTXO autorepair: after a validation/UTXO consistency failure we write
//! `ibd_utxo_repair_required`. On the **next** startup we clear the marker and let the
//! normal `chain_tip > watermark → replay` codepath restore consistency by re-validating
//! the gap. The persisted `ibd_utxos` rows up to `watermark` are kept, so replay is bounded
//! to roughly `flush_threshold` blocks — not the full chain.
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
use tracing::{info, warn};

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
        "Wrote {} — next startup will clear IBD UTXO disk state unless BLVM_IBD_SKIP_AUTOREPAIR is set",
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

/// Best-effort classification: errors where clearing `ibd_utxos` and replaying from on-disk blocks may help.
///
/// Uses stable substrings from `blvm-consensus` connect paths and parallel IBD — not generic
/// "invalid block" text (consensus bugs and bad peers would otherwise trigger destructive repair).
pub fn validation_error_suggests_utxo_repair(err: &anyhow::Error) -> bool {
    let s = err.to_string();
    s.contains("UTXO not found for input")
        || s.contains("IBD UTXO mutex poisoned")
        || s.contains("UTXO flush panicked")
        || s.contains("Failed to open IBD UTXO tree")
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
    let engine_enabled = std::env::var("BLVM_IBD_ENGINE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if engine_enabled {
        if let Some(_eh) = storage
            .chain()
            .get_engine_export_height()?
            .filter(|&h| h > 0)
        {
            let slot = storage.chain().get_engine_ckpt_slot()?;
            let ckpt_name = crate::storage::ibd_engine::ckpt_tree_for_slot(slot);
            if let Ok(tree) = storage.open_tree(ckpt_name) {
                if !tree.is_empty()? {
                    return Ok(watermark_val);
                }
            }
            warn!(
                "[ibd_autorepair] engine export height set but {} empty — falling through to ibd_utxos check",
                ckpt_name
            );
        }
    }
    let tree = storage.open_tree("ibd_utxos")?;
    if tree.is_empty()? {
        warn!(
            "[ibd_autorepair] ibd_utxo_watermark={} but ibd_utxos tree is empty — resetting watermark to 0 \
             (watermark likely jumped ahead of durable UTXO flushes)",
            watermark_val
        );
        storage.chain().force_set_ibd_utxo_watermark(0)?;
        Ok(0)
    } else {
        Ok(watermark_val)
    }
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
        let engine_mode = std::env::var("BLVM_IBD_ENGINE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

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

    // Default soft repair: preserve persisted ibd_utxos rows up to `watermark`. Replay
    // (`chain_tip > watermark` → re-validate the gap) handles consistency. This bounds
    // re-validation to ≈`flush_threshold` blocks instead of the full chain.
    let watermark = storage
        .chain()
        .get_utxo_watermark()
        .unwrap_or(None)
        .unwrap_or(0);
    info!(
        "IBD UTXO autorepair (soft): clearing repair marker; preserving ibd_utxos & watermark={} \
         — replay (chain_tip > watermark) will reconcile any gap",
        watermark
    );
    clear_ibd_utxo_repair_flag(data_dir)?;
    warn!(
        "IBD UTXO autorepair (soft) applied; if the next IBD attempt re-trips the same UTXO error \
         and sets the marker again, set BLVM_IBD_AGGRESSIVE_REPAIR=1 for the destructive wipe path"
    );
    Ok(())
}

#[cfg(all(test, feature = "production"))]
mod ibd_autorepair_tests {
    use super::*;
    use crate::storage::Storage;
    use tempfile::TempDir;

    #[test]
    fn apply_autorepair_soft_preserves_state_clears_marker() {
        // Default (no BLVM_IBD_AGGRESSIVE_REPAIR): marker is consumed but ibd_utxos and
        // watermark are preserved. Replay (`chain_tip > watermark`) handles reconciliation.
        let _aggressive_guard = AggressiveRepairEnvGuard::cleared();
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let storage = Storage::new(data_dir).unwrap();

        storage.chain().set_utxo_watermark(999).unwrap();
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
            Some(999),
            "soft repair must preserve watermark — replay handles tip>watermark gap"
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

    /// Lock around `BLVM_IBD_AGGRESSIVE_REPAIR` so the soft/aggressive tests don't race.
    /// Cargo runs tests in parallel by default; an env-set in one test would otherwise leak
    /// into the other.
    struct AggressiveRepairEnvGuard;
    impl AggressiveRepairEnvGuard {
        fn set() -> std::sync::MutexGuard<'static, ()> {
            let g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("BLVM_IBD_AGGRESSIVE_REPAIR", "1");
            g
        }
        fn cleared() -> std::sync::MutexGuard<'static, ()> {
            let g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
            g
        }
    }
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
}
