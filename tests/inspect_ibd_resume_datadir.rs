//! Manual inspection of on-disk IBD resume inputs (parallel IBD watermark vs chain tip).
//!
//! Opens the node's RocksDB read-write — **stop `blvm` first** or the open may fail or contend.
//!
//! ```text
//! BLVM_INSPECT_DATADIR="$HOME/.local/share/blvm-mainnet" \
//!   cargo test -p blvm-node --test inspect_ibd_resume_datadir inspect_ibd_resume_datadir -- --ignored --nocapture
//! ```

#![cfg(all(feature = "production", feature = "rocksdb"))]

use blvm_node::storage::Storage;
use blvm_node::storage::ibd_autorepair::{ibd_utxo_repair_flag_present, repair_marker_path};
use std::path::PathBuf;

#[test]
#[ignore = "Touches live RocksDB; stop blvm. Requires BLVM_INSPECT_DATADIR."]
fn inspect_ibd_resume_datadir() -> anyhow::Result<()> {
    let dir: PathBuf = std::env::var("BLVM_INSPECT_DATADIR")
        .expect(
            "Set BLVM_INSPECT_DATADIR to your node data directory (same as blvm Data directory)",
        )
        .into();

    println!("BLVM_INSPECT_DATADIR = {}", dir.display());
    let marker = repair_marker_path(&dir);
    println!(
        "ibd_utxo_repair_required present: {} ({})",
        ibd_utxo_repair_flag_present(&dir),
        marker.display()
    );

    let storage = Storage::new(&dir)?;
    let chain_tip = storage.chain().get_height()?.unwrap_or(0);
    let wm = storage.chain().get_utxo_watermark()?;
    let mh = storage.chain().get_ibd_utxo_muhash_running()?;

    let ibd_tree = storage.open_tree("ibd_utxos")?;
    let ibd_empty = ibd_tree.is_empty()?;

    let effective = chain_tip.min(wm.unwrap_or(0));

    println!("chain_tip (chain_info.height): {chain_tip}");
    println!(
        "ibd_utxo_watermark: {}",
        wm.map(|w| w.to_string())
            .unwrap_or_else(|| "<key missing>".to_string())
    );
    println!(
        "ibd_utxo_muhash_running: {}",
        if mh.is_some() { "present" } else { "<missing>" }
    );
    println!("ibd_utxos tree empty: {ibd_empty}");
    println!("effective_tip (min(chain_tip, watermark)): {effective}");
    println!(
        "startup ibd_first_block_height would be: {}",
        effective.saturating_add(1)
    );

    if wm.is_none() && chain_tip > 0 {
        println!(
            "NOTE: missing watermark → node treats watermark as 0 (full UTXO replay from block 1)."
        );
    }
    if wm == Some(0) && chain_tip > 0 {
        println!(
            "NOTE: watermark explicitly 0 with blocks indexed → typically force_set_ibd_utxo_watermark(0) or never flushed."
        );
    }
    if ibd_empty && wm.unwrap_or(0) > 0 {
        println!(
            "NOTE: reconcile_ibd_utxo_watermark_with_disk would reset watermark to 0 on next production startup."
        );
    }

    Ok(())
}
