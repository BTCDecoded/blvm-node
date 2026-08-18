//! Copy the active IBD UTXO checkpoint tree (+ export metadata) from one data dir to another.
//!
//! Use when a bodies snapshot's ckpt is poisoned/wrong-height but a sibling snapshot has a
//! good mid-IBD export (e.g. hotpath-400k → hotpath-500k for synth 400→500 resume).
//!
//! Both dirs must be stopped (no blvm holding the LMDB lock).
//!
//! ```bash
//! cargo run --example copy_ibd_ckpt --release --features production,heed3 -- \
//!   $DATADIR/blvm-hotpath-400k $DATADIR/blvm-hotpath-500k
//! ```

use anyhow::{bail, Context};
use blvm_node::storage::ibd_engine::{ckpt_tree_for_slot, CKPT_TREE_A, CKPT_TREE_B};
use blvm_node::storage::Storage;
use std::path::PathBuf;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let src_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("usage: copy_ibd_ckpt SRC_DATA_DIR DST_DATA_DIR")?,
    );
    let dst_dir = PathBuf::from(
        std::env::args()
            .nth(2)
            .context("usage: copy_ibd_ckpt SRC_DATA_DIR DST_DATA_DIR")?,
    );
    if src_dir == dst_dir {
        bail!("SRC and DST must differ");
    }

    println!("opening SRC {src_dir:?}");
    let src = Storage::new(&src_dir)?;
    let export_h = src
        .chain()
        .get_engine_export_height()?
        .context("SRC has no ibd_engine_export_height")?;
    let slot = src.chain().get_engine_ckpt_slot()?;
    let slot_h = src.chain().get_engine_ckpt_slot_height(slot)?;
    let utxo_count = src.chain().get_engine_export_utxo_count()?;
    let muhash = src.chain().get_engine_export_muhash()?;
    let wall_secs = src.chain().get_engine_export_wall_secs()?;
    let src_tree_name = ckpt_tree_for_slot(slot);
    let src_tree = src.open_tree(src_tree_name)?;
    let src_len = src_tree.len()?;
    println!(
        "SRC export_h={export_h} slot={slot} slot_h={slot_h} \
         utxo_count={utxo_count:?} tree={src_tree_name} len={src_len}"
    );
    if src_len == 0 {
        bail!("SRC active ckpt tree is empty");
    }
    if export_h == 0 {
        bail!("SRC export_h is 0");
    }

    println!("opening DST {dst_dir:?}");
    let dst = Storage::new(&dst_dir)?;
    let before_h = dst.chain().get_engine_export_height()?;
    let before_count = dst.chain().get_engine_export_utxo_count()?;
    let before_slot = dst.chain().get_engine_ckpt_slot()?;
    println!(
        "DST before: export_h={before_h:?} slot={before_slot} utxo_count={before_count:?}"
    );

    // Clear both ping-pong slots so freelist can absorb the rewrite; write into slot 0.
    for name in [CKPT_TREE_A, CKPT_TREE_B] {
        if let Ok(tree) = dst.open_tree(name) {
            let n = tree.len().unwrap_or(0);
            println!("clearing DST {name} (len={n}) …");
            tree.clear()?;
        }
    }
    dst.flush()?;

    let dst_tree = dst.open_tree(CKPT_TREE_A)?;
    let t0 = Instant::now();
    let mut copied = 0usize;
    // Match export chunk size (~500k keys / ~60 MB) — one LMDB write txn per chunk.
    const CHUNK: usize = 500_000;
    let mut buf: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(CHUNK);
    let flush_chunk = |buf: &mut Vec<(Vec<u8>, Vec<u8>)>,
                       copied: &mut usize,
                       dst_tree: &dyn blvm_node::storage::database::Tree|
     -> anyhow::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        dst_tree.bulk_load_sorted_kv(buf)?;
        *copied += buf.len();
        buf.clear();
        Ok(())
    };
    for item in src_tree.iter() {
        let (k, v) = item?;
        buf.push((k, v));
        if buf.len() >= CHUNK {
            flush_chunk(&mut buf, &mut copied, dst_tree.as_ref())?;
            let elapsed = t0.elapsed().as_secs_f64();
            let rate = copied as f64 / elapsed.max(0.001);
            println!(
                "  copied {copied}/{src_len} ({:.1}%) {:.0} rows/s",
                100.0 * copied as f64 / src_len as f64,
                rate
            );
        }
    }
    flush_chunk(&mut buf, &mut copied, dst_tree.as_ref())?;
    if copied != src_len {
        bail!("copied {copied} != SRC len {src_len}");
    }
    println!(
        "copied {copied} UTXOs into DST {CKPT_TREE_A} in {:.1}s",
        t0.elapsed().as_secs_f64()
    );

    dst.chain().force_set_engine_export_height(export_h)?;
    dst.chain().force_set_engine_ckpt_slot(0)?;
    dst.chain().set_engine_ckpt_slot_height(0, export_h)?;
    dst.chain().set_engine_ckpt_slot_height(1, 0)?;
    if let Some(c) = utxo_count {
        dst.chain().force_set_engine_export_utxo_count(c)?;
    } else {
        dst.chain()
            .force_set_engine_export_utxo_count(copied as u64)?;
    }
    if let Some(mh) = muhash {
        dst.chain().persist_engine_export_muhash_snapshot(&mh)?;
    }
    if let Some(secs) = wall_secs {
        let info = dst.open_tree("chain_info")?;
        info.insert(b"ibd_engine_export_wall_secs", &secs.to_be_bytes())?;
    }
    // Keep watermark ≤ export so resume doesn't invent a gap past the ckpt.
    dst.chain().force_set_ibd_utxo_watermark(export_h)?;
    dst.flush()?;

    let after_len = dst.open_tree(CKPT_TREE_A)?.len()?;
    let after_h = dst.chain().get_engine_export_height()?;
    let after_count = dst.chain().get_engine_export_utxo_count()?;
    println!(
        "DST after: export_h={after_h:?} slot=0 tree_len={after_len} utxo_count={after_count:?}"
    );
    if after_len != copied {
        bail!("DST tree len {after_len} != copied {copied}");
    }
    println!("OK: good ckpt transplanted SRC → DST");
    Ok(())
}
