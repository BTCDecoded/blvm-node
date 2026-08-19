//! Probe whether an outpoint exists in engine ping-pong checkpoint trees.
//!
//! ```bash
//! cargo run --example probe_ckpt_utxo --features production,heed3 -- \
//!   ~/.local/share/blvm-mainnet \
//!   988c6291515bd1156feee3119d4ba2371d978a9b21f3ecd9abe193ad1a33a508:1
//! ```

use anyhow::{Context, bail};
use blvm_node::storage::Storage;
use blvm_node::storage::disk_utxo::outpoint_to_key;
use blvm_protocol::types::OutPoint;
use std::path::PathBuf;

fn parse_outpoint(s: &str) -> anyhow::Result<OutPoint> {
    let (txid_hex, vout_s) = s.split_once(':').context("expected txid:vout")?;
    let bytes = hex::decode(txid_hex).context("txid hex")?;
    if bytes.len() != 32 {
        bail!("txid must be 32 bytes, got {}", bytes.len());
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&bytes);
    let index: u32 = vout_s.parse().context("vout")?;
    Ok(OutPoint { hash, index })
}

fn main() -> anyhow::Result<()> {
    let data_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("usage: probe_ckpt_utxo DATA_DIR TXID:VOUT")?,
    );
    let op = parse_outpoint(
        &std::env::args()
            .nth(2)
            .context("usage: probe_ckpt_utxo DATA_DIR TXID:VOUT")?,
    )?;
    let key = outpoint_to_key(&op);
    let storage = Storage::new(&data_dir)?;
    let export_h = storage.chain().get_engine_export_height()?;
    let slot = storage.chain().get_engine_ckpt_slot()?;
    let wm = storage.chain().get_utxo_watermark()?;
    let count = storage.chain().get_engine_export_utxo_count()?;
    let tip = storage.chain().get_height()?;
    println!(
        "chain_tip={tip:?} wm={wm:?} export_h={export_h:?} active_slot={slot} utxo_count={count:?}"
    );
    for name in ["ibd_utxos_ckpt_a", "ibd_utxos_ckpt_b", "ibd_utxos"] {
        let tree = storage.open_tree(name)?;
        let empty = tree.is_empty().unwrap_or(true);
        let present = tree.contains_key(&key).unwrap_or(false);
        let len = tree.len().unwrap_or(0);
        println!("{name}: empty={empty} len≈{len} contains_prevout={present}");
    }
    Ok(())
}
