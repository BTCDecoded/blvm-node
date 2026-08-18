//! Backfill `ibd_engine_export_muhash` from the active engine checkpoint tree (offline).
//!
//! Run while blvm is stopped, before an engine resume restart:
//!
//! ```bash
//! cargo run --example backfill_engine_export_muhash --release --features production,heed3 -- \
//!   /mnt/data/blvm-mainnet-ibd
//! ```

use anyhow::Context;
use blvm_node::storage::Storage;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let data_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("usage: backfill_engine_export_muhash DATA_DIR")?,
    );
    let storage = Storage::new(&data_dir)?;
    let export_h = storage
        .chain()
        .get_engine_export_height()?
        .unwrap_or(0);
    if export_h == 0 {
        println!("no engine export height — nothing to backfill");
        return Ok(());
    }
    if storage.chain().get_engine_export_muhash()?.is_some() {
        println!("export MuHash snapshot already present at h={export_h}");
        return Ok(());
    }
    let backfilled =
        blvm_node::storage::ibd_utxo_muhash::backfill_engine_export_muhash_if_missing(&storage)?;
    if backfilled {
        println!("backfilled export MuHash snapshot at h={export_h}");
    } else {
        println!("no backfill performed (export_h={export_h})");
    }
    Ok(())
}
