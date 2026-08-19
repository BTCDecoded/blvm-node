//! Strip stored block bodies above a height (headers kept).
//!
//! Used by TRUE-WAN soaks: 400k CAS ships bodies_tip≈437k (LOCAL_GAP), but engine
//! export is @400287. After remat, strip bodies > export so peer WAN starts immediately
//! and tip-hole LOCAL_GAP still works for newly downloaded heights.
//!
//! Clears through `max(confirmed_body, highest_stored_body)` — orphan bodies above
//! confirmed otherwise seed `IBD_LOCAL_AHEAD` at ignition.
//!
//! ```bash
//! cargo run --example strip_bodies_above --features production,heed3,ibd-dev --release -- \
//!   $DATADIR/blvm-wan-bodies-400k 400287
//! ```

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
            .context("usage: strip_bodies_above DATA_DIR KEEP_BODY_MAX_HEIGHT")?,
    );
    let keep_max: u64 = args
        .next()
        .context("missing KEEP_BODY_MAX_HEIGHT")?
        .parse()
        .context("KEEP_BODY_MAX_HEIGHT")?;

    let storage = Storage::new(&data_dir)?;
    let blockstore = storage.blocks();
    let confirmed = probe_confirmed_body_height(&blockstore)?;
    // Orphan bodies above confirmed seed IBD_LOCAL_AHEAD at ignition (tc121 live tip
    // 405k while confirmed=400287 after a confirmed-only strip).
    let highest = probe_highest_stored_body_height(&blockstore)?;
    let before = confirmed.max(highest);
    let header_max = blockstore.highest_stored_height()?.unwrap_or(0);
    println!(
        "before confirmed_body={confirmed} highest_body={highest} strip_to={before} header_max={header_max} keep_max={keep_max}"
    );

    if before <= keep_max {
        println!("noop: body tip already ≤ keep_max");
        return Ok(());
    }

    let start = keep_max.saturating_add(1);
    let removed = blockstore.remove_blocks_by_height_range(start, before)?;
    // Drop witness blobs for the same range so tip reload cannot resurrect stale empty stacks.
    for h in start..=before {
        if let Some(hash) = blockstore.get_hash_by_height(h)? {
            let _ = blockstore.remove_witness(&hash);
        }
    }
    let after_c = probe_confirmed_body_height(&blockstore)?;
    let after_h = probe_highest_stored_body_height(&blockstore)?;
    println!("removed_bodies≈{removed} after confirmed_body={after_c} highest_body={after_h}");
    if after_c.max(after_h) > keep_max {
        anyhow::bail!(
            "strip failed: confirmed={after_c} highest={after_h} still > keep_max={keep_max}"
        );
    }
    Ok(())
}
