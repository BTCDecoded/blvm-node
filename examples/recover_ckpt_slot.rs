//! Roll engine resume to a prior ping-pong checkpoint slot (poisoned export recovery).
//!
//! ```bash
//! cargo run --example recover_ckpt_slot --features production,heed3,ibd-dev -- \
//!   ~/.local/share/blvm-mainnet 1 640000
//! ```

use anyhow::Context;
use blvm_node::storage::Storage;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let data_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("usage: recover_ckpt_slot DATA_DIR SLOT HEIGHT")?,
    );
    let slot: u8 = std::env::args()
        .nth(2)
        .context("usage: recover_ckpt_slot DATA_DIR SLOT HEIGHT")?
        .parse()
        .context("SLOT must be 0 or 1")?;
    let height: u64 = std::env::args()
        .nth(3)
        .context("usage: recover_ckpt_slot DATA_DIR SLOT HEIGHT")?
        .parse()
        .context("HEIGHT must be u64")?;
    let storage = Storage::new(&data_dir)?;
    let tree_name = blvm_node::storage::ibd_engine::ckpt_tree_for_slot(slot);
    let tree = storage.open_tree(tree_name)?;
    if tree.is_empty().unwrap_or(true) {
        anyhow::bail!("{tree_name} is empty — refusing rollback");
    }
    let before_export = storage.chain().get_engine_export_height()?;
    let before_slot = storage.chain().get_engine_ckpt_slot()?;
    let before_wm = storage.chain().get_utxo_watermark()?;
    storage.chain().force_set_engine_ckpt_slot(slot)?;
    storage.chain().set_engine_ckpt_slot_height(slot, height)?;
    storage.chain().force_set_engine_export_height(height)?;
    storage.chain().force_set_ibd_utxo_watermark(height)?;
    let n = tree.len().unwrap_or(0) as u64;
    if n > 0 {
        storage.chain().force_set_engine_export_utxo_count(n)?;
    }
    let _ = storage.chain().force_set_engine_validation_tip(height);
    // Record the abandoned tip slot height when known.
    if let Some(bad_h) = before_export {
        let bad_slot = before_slot;
        storage
            .chain()
            .set_engine_ckpt_slot_height(bad_slot, bad_h)?;
    }
    if let Some(root) = storage.data_dir() {
        let engine_dir = root.join("ibd_engine");
        let _ = std::fs::remove_dir_all(engine_dir);
    }
    let marker = data_dir.join("ibd_utxo_repair_required");
    let _ = std::fs::remove_file(&marker);
    storage.flush()?;
    println!(
        "recovered: slot {before_slot}->{slot} export {before_export:?}->{height} wm {before_wm:?}->{height}"
    );
    println!(
        "cleared ibd_engine/ and repair marker; next start re-seeds from {tree_name} @ {height}"
    );
    Ok(())
}
