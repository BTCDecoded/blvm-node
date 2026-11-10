//! Diagnose local IBD block load for a single height or scan a range (resume gap debugging).
//!
//! ```bash
//! cargo run --example inspect_local_block --features production,heed3 -- \
//!   /mnt/data/blvm-mainnet-ibd 601620
//!
//! cargo run --example inspect_local_block --features production,heed3 -- \
//!   /mnt/data/blvm-mainnet-ibd --scan 601610 601630
//! ```

use anyhow::Context;
use blvm_node::node::parallel_ibd::local_block::{
    LocalBlockMiss, has_real_witnesses, probe_confirmed_body_height,
    try_load_local_ibd_block_with_reason,
};
use blvm_node::storage::Storage;
use blvm_node::storage::blockstore::block_height_row_key;
use blvm_protocol::ProtocolVersion;
use std::path::PathBuf;

fn inspect_one(
    blockstore: &blvm_node::storage::blockstore::BlockStore,
    height: u64,
    protocol_version: ProtocolVersion,
) -> anyhow::Result<()> {
    let hash = match blockstore.get_hash_by_height(height)? {
        Some(h) => h,
        None => {
            println!("height={height} hash=MISSING (no height index)");
            return Ok(());
        }
    };
    let row_key = block_height_row_key(height, &hash);
    let body_row = blockstore.blocks_tree()?.get(row_key.as_slice())?.is_some();
    let witness_row = blockstore.has_witness_blob(&hash)?;

    println!(
        "height={height} hash={} body_row={} witness_row_or_legacy={}",
        hex::encode(hash),
        body_row,
        witness_row
    );

    match try_load_local_ibd_block_with_reason(blockstore, height, hash, protocol_version)? {
        Ok((block, witnesses)) => {
            let witness_items: usize = witnesses
                .iter()
                .map(|tx| tx.iter().filter(|s| !s.is_empty()).count())
                .sum();
            println!(
                "load=OK txs={} witness_stack_items={} has_real_witnesses={}",
                block.transactions.len(),
                witness_items,
                has_real_witnesses(&witnesses)
            );
        }
        Err(miss) => {
            println!("load=MISS reason={miss}");
            if miss == LocalBlockMiss::WitnessMissing {
                println!(
                    "hint: body on disk but witness row missing — WAN re-fetch + IBD_WITNESS_REPAIR"
                );
            } else if miss == LocalBlockMiss::WitnessEmptyStale {
                println!("hint: re-fetch with MSG_WITNESS_BLOCK or repair witness blob");
            }
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let data_dir = PathBuf::from(
        args.first()
            .context("usage: inspect_local_block DATA_DIR HEIGHT | --scan START END")?,
    );

    let storage = Storage::new(&data_dir)?;
    let blockstore = storage.blocks();
    let confirmed = probe_confirmed_body_height(blockstore.as_ref())?;
    println!("confirmed_body_height={confirmed}");

    let protocol_version = ProtocolVersion::BitcoinV1;

    if args.get(1).map(String::as_str) == Some("--scan") {
        let start: u64 = args
            .get(2)
            .context("--scan requires START END")?
            .parse()
            .context("START must be u64")?;
        let end: u64 = args
            .get(3)
            .context("--scan requires START END")?
            .parse()
            .context("END must be u64")?;
        let mut misses = 0u64;
        for h in start..=end {
            let hash = match blockstore.get_hash_by_height(h)? {
                Some(h) => h,
                None => continue,
            };
            match try_load_local_ibd_block_with_reason(
                blockstore.as_ref(),
                h,
                hash,
                protocol_version,
            )? {
                Ok(_) => {}
                Err(miss) => {
                    misses += 1;
                    println!("SCAN_MISS height={h} reason={miss}");
                }
            }
        }
        println!("SCAN_DONE range={start}-{end} misses={misses}");
        return Ok(());
    }

    let height: u64 = args
        .get(1)
        .context("usage: inspect_local_block DATA_DIR HEIGHT | --scan START END")?
        .parse()
        .context("HEIGHT must be u64")?;

    inspect_one(blockstore.as_ref(), height, protocol_version)
}
