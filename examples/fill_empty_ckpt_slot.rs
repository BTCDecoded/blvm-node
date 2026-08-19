//! Copy SRC active IBD UTXO ckpt into an **empty** DST ping-pong slot.
//! Does **not** clear the other DST slot (unlike `copy_ibd_ckpt`).
//!
//! Use to restore 400287 into hotpath-400k `ckpt_a` while keeping 300287 in `ckpt_b`.
//!
//! ```bash
//! cargo run --example fill_empty_ckpt_slot --profile release-fast --features production,heed3,ibd-dev -- \
//!   $DATADIR/blvm-adapt-bodies-400-500 $DATADIR/blvm-hotpath-400k 0
//! ```

use anyhow::{Context, bail};
use blvm_node::storage::Storage;
use blvm_node::storage::ibd_engine::ckpt_tree_for_slot;
use std::path::PathBuf;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let src_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("usage: fill_empty_ckpt_slot SRC_DIR DST_DIR DST_SLOT")?,
    );
    let dst_dir = PathBuf::from(
        std::env::args()
            .nth(2)
            .context("usage: fill_empty_ckpt_slot SRC_DIR DST_DIR DST_SLOT")?,
    );
    let dst_slot: u8 = std::env::args()
        .nth(3)
        .context("usage: fill_empty_ckpt_slot SRC_DIR DST_DIR DST_SLOT")?
        .parse()
        .context("DST_SLOT must be 0 or 1")?;
    if dst_slot > 1 {
        bail!("DST_SLOT must be 0 or 1");
    }
    if src_dir == dst_dir {
        bail!("SRC and DST must differ");
    }

    let src = Storage::new(&src_dir)?;
    let export_h = src
        .chain()
        .get_engine_export_height()?
        .context("SRC has no export_h")?;
    let src_slot = src.chain().get_engine_ckpt_slot()?;
    let utxo_count = src.chain().get_engine_export_utxo_count()?;
    let muhash = src.chain().get_engine_export_muhash()?;
    let src_tree_name = ckpt_tree_for_slot(src_slot);
    let src_tree = src.open_tree(src_tree_name)?;
    let src_len = src_tree.len()?;
    println!("SRC export_h={export_h} slot={src_slot} tree={src_tree_name} len={src_len}");
    if src_len == 0 {
        bail!("SRC active ckpt empty");
    }

    let dst = Storage::new(&dst_dir)?;
    let dst_tree_name = ckpt_tree_for_slot(dst_slot);
    let other_slot = 1u8 - dst_slot;
    let other_name = ckpt_tree_for_slot(other_slot);
    let other_len = dst.open_tree(other_name)?.len().unwrap_or(0);
    let dst_tree = dst.open_tree(dst_tree_name)?;
    let before_len = dst_tree.len()?;
    println!("DST before: {dst_tree_name} len={before_len}; keeping {other_name} len={other_len}");
    if before_len != 0 {
        bail!("{dst_tree_name} is not empty (len={before_len}) — refusing overwrite");
    }
    if other_len == 0 {
        bail!("{other_name} is also empty — use copy_ibd_ckpt instead");
    }

    let t0 = Instant::now();
    let mut copied = 0usize;
    const CHUNK: usize = 500_000;
    let mut buf: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(CHUNK);
    let flush = |buf: &mut Vec<(Vec<u8>, Vec<u8>)>,
                 copied: &mut usize,
                 tree: &dyn blvm_node::storage::database::Tree|
     -> anyhow::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        tree.bulk_load_sorted_kv(buf)?;
        *copied += buf.len();
        buf.clear();
        Ok(())
    };
    for item in src_tree.iter() {
        let (k, v) = item?;
        buf.push((k, v));
        if buf.len() >= CHUNK {
            flush(&mut buf, &mut copied, dst_tree.as_ref())?;
            println!(
                "  copied {copied}/{src_len} ({:.1}%)",
                100.0 * copied as f64 / src_len as f64
            );
        }
    }
    flush(&mut buf, &mut copied, dst_tree.as_ref())?;
    if copied != src_len {
        bail!("copied {copied} != src {src_len}");
    }

    // Point active export at the restored slot; do not touch other slot height/tree.
    dst.chain().force_set_engine_ckpt_slot(dst_slot)?;
    dst.chain()
        .set_engine_ckpt_slot_height(dst_slot, export_h)?;
    dst.chain().force_set_engine_export_height(export_h)?;
    dst.chain()
        .force_set_engine_export_utxo_count(utxo_count.unwrap_or(copied as u64))?;
    if let Some(mh) = muhash {
        dst.chain().persist_engine_export_muhash_snapshot(&mh)?;
    }
    dst.chain().force_set_ibd_utxo_watermark(export_h)?;
    let _ = dst.chain().force_set_engine_validation_tip(export_h);
    dst.flush()?;

    let after = dst.open_tree(dst_tree_name)?.len()?;
    let keep = dst.open_tree(other_name)?.len()?;
    println!(
        "OK: filled {dst_tree_name} len={after} export_h={export_h}; {other_name} still len={keep} ({:.1}s)",
        t0.elapsed().as_secs_f64()
    );
    if keep != other_len {
        bail!("OTHER SLOT CHANGED {other_len} → {keep} — abort state inconsistent");
    }
    Ok(())
}
