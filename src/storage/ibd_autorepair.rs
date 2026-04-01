//! Marker-driven IBD UTXO autorepair: after a validation/UTXO consistency failure, the next
//! startup clears `ibd_utxos` and resets `ibd_utxo_watermark` so parallel IBD can replay from
//! blocks already on disk. Set `BLVM_IBD_SKIP_AUTOREPAIR=1` to disable.

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
        info!("Removed IBD UTXO autorepair marker after successful parallel IBD");
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

#[cfg(feature = "production")]
pub fn apply_ibd_utxo_autorepair_if_needed(
    storage: &crate::storage::Storage,
    data_dir: &Path,
) -> Result<()> {
    if std::env::var("BLVM_IBD_SKIP_AUTOREPAIR").is_ok() {
        if ibd_utxo_repair_flag_present(data_dir) {
            warn!(
                "IBD UTXO repair marker present but BLVM_IBD_SKIP_AUTOREPAIR is set — not clearing ibd_utxos"
            );
        }
        return Ok(());
    }
    if !ibd_utxo_repair_flag_present(data_dir) {
        return Ok(());
    }
    info!(
        "IBD UTXO autorepair: clearing ibd_utxos and forcing ibd_utxo_watermark to 0 (marker was present)"
    );
    let tree = storage.open_tree("ibd_utxos")?;
    tree.clear()?;
    storage.chain().force_set_ibd_utxo_watermark(0)?;
    storage.flush()?;
    warn!(
        "IBD UTXO autorepair applied; on-disk blocks are kept. Marker stays until parallel IBD completes successfully."
    );
    Ok(())
}
