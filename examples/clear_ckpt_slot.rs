//! Clear a single IBD UTXO ping-pong ckpt tree (does not touch the other slot).
//!
//! Use to wipe a poisoned/incomplete slot before `fill_empty_ckpt_slot`.
//!
//! ```bash
//! cargo run --example clear_ckpt_slot --profile release-fast --features production,heed3,ibd-dev -- \
//!   $DATADIR/blvm-hotpath-400k 1
//! ```

use anyhow::{Context, bail};
use blvm_node::storage::Storage;
use blvm_node::storage::ibd_engine::ckpt_tree_for_slot;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let data_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("usage: clear_ckpt_slot DATA_DIR SLOT")?,
    );
    let slot: u8 = std::env::args()
        .nth(2)
        .context("usage: clear_ckpt_slot DATA_DIR SLOT")?
        .parse()
        .context("SLOT must be 0 or 1")?;
    if slot > 1 {
        bail!("SLOT must be 0 or 1");
    }

    let storage = Storage::new(&data_dir)?;
    let name = ckpt_tree_for_slot(slot);
    let other = ckpt_tree_for_slot(1 - slot);
    let other_len = storage.open_tree(other)?.len().unwrap_or(0);
    let tree = storage.open_tree(name)?;
    let before = tree.len().unwrap_or(0);
    println!("clearing {name} len={before}; keeping {other} len={other_len}");
    if other_len == 0 {
        bail!("{other} is empty — refusing to clear last ckpt");
    }
    tree.clear()?;
    storage.flush()?;
    let after = storage.open_tree(name)?.len().unwrap_or(0);
    let keep = storage.open_tree(other)?.len().unwrap_or(0);
    println!("OK: {name} len={after}; {other} still len={keep}");
    if keep != other_len {
        bail!("OTHER SLOT CHANGED {other_len} → {keep}");
    }
    if after != 0 {
        bail!("{name} still non-empty after clear");
    }
    Ok(())
}
