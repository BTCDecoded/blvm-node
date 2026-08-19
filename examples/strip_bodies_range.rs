//! Remove bodies in [start, end] only. Headers kept.
//! Hole-punch so confirmed tip stays at start-1 while islands above end remain.
use anyhow::Context;
use blvm_node::node::parallel_ibd::local_block::{
    probe_confirmed_body_height, probe_highest_stored_body_height,
};
use blvm_node::storage::Storage;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = PathBuf::from(
        args.next()
            .context("usage: strip_bodies_range DATA_DIR START_HEIGHT END_HEIGHT (needs --features ibd-dev)")?,
    );
    let start: u64 = args.next().context("missing START")?.parse()?;
    let end: u64 = args.next().context("missing END")?.parse()?;
    anyhow::ensure!(end >= start, "END < START");

    let storage = Storage::new(&data_dir)?;
    let blockstore = storage.blocks();
    let confirmed = probe_confirmed_body_height(&blockstore)?;
    let highest = probe_highest_stored_body_height(&blockstore)?;
    let removed = blockstore.remove_blocks_by_height_range(start, end)?;
    for h in start..=end {
        if let Some(hash) = blockstore.get_hash_by_height(h)? {
            let _ = blockstore.remove_witness(&hash);
        }
    }
    let after_c = probe_confirmed_body_height(&blockstore)?;
    let after_h = probe_highest_stored_body_height(&blockstore)?;
    println!(
        "removed_bodies≈{removed} after confirmed_body={after_c} highest_body={after_h} (was {confirmed}/{highest}) range={start}-{end}"
    );
    Ok(())
}
