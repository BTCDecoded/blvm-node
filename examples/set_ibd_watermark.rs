//! Set IBD UTXO watermark (rollback after unclean shutdown).
//!
//! ```bash
//! cargo run --example set_ibd_watermark --features production,heed3 -- \
//!   /mnt/data/blvm-mainnet-ibd 601289
//! ```

use anyhow::Context;
use blvm_node::storage::Storage;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let data_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("usage: set_ibd_watermark DATA_DIR HEIGHT")?,
    );
    let height: u64 = std::env::args()
        .nth(2)
        .context("usage: set_ibd_watermark DATA_DIR HEIGHT")?
        .parse()
        .context("HEIGHT must be u64")?;
    let storage = Storage::new(&data_dir)?;
    let before_wm = storage.chain().get_utxo_watermark()?.unwrap_or(0);
    let before_export = storage.chain().get_engine_export_height()?;
    storage.chain().force_set_ibd_utxo_watermark(height)?;
    if height == 0 {
        storage.chain().force_reset_engine_checkpoint_metadata()?;
        for tree_name in &["ibd_utxos_ckpt_a", "ibd_utxos_ckpt_b"] {
            if let Ok(tree) = storage.open_tree(tree_name) {
                let _ = tree.clear();
            }
        }
        println!("reset engine checkpoint metadata + cleared ckpt trees");
    } else {
        // Keep export_h aligned with watermark for engine reseed recovery.
        storage.chain().force_set_engine_export_height(height)?;
    }
    storage.flush()?;
    let after_wm = storage.chain().get_utxo_watermark()?.unwrap_or(0);
    let after_export = storage.chain().get_engine_export_height()?;
    println!("ibd_utxo_watermark: {before_wm} -> {after_wm}");
    println!("engine_export_height: {before_export:?} -> {after_export:?}");
    Ok(())
}
