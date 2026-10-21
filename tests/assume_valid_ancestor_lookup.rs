//! The node tells consensus which blocks are ancestors of the assume-valid block by
//! walking the stored header tree back from that block (Core: `GetAncestor`), not by
//! trusting whatever hash the height index holds.

use blvm_consensus::config::{ConsensusConfig, init_consensus_config};
use blvm_node::storage::Storage;
use blvm_protocol::{BlockHeader, Hash};
use tempfile::TempDir;

const AV_HEIGHT: u64 = 6;

fn header(prev: Hash, salt: u32) -> BlockHeader {
    BlockHeader {
        version: 4,
        prev_block_hash: prev,
        merkle_root: [salt as u8; 32],
        timestamp: 1_300_000_000,
        bits: 0x207fffff,
        nonce: salt as u64,
    }
}

/// Headers 0..=len-1 chained by prev hash; returns (hash, header, height).
fn chain(len: u64, salt: u32) -> Vec<(Hash, BlockHeader, u64)> {
    let mut out = Vec::new();
    let mut prev = [0u8; 32];
    for h in 0..len {
        let hd = header(prev, salt + h as u32);
        let hash = blvm_consensus::block::block_header_hash(&hd);
        out.push((hash, hd, h));
        prev = hash;
    }
    out
}

#[test]
fn ancestry_follows_the_assume_valid_block_not_the_height_index() {
    // Competing headers for heights 0..=3 are stored first, so the height index keeps them
    // (first seen wins) even after the real chain arrives.
    let real = chain(10, 1_000);
    let mut fork = chain(4, 5_000);
    fork[0] = real[0].clone(); // same genesis

    let mut cfg = ConsensusConfig::default();
    cfg.block_validation.assume_valid_height = AV_HEIGHT;
    cfg.block_validation.assume_valid_hash = Some(real[AV_HEIGHT as usize].0);
    init_consensus_config(cfg);

    let dir = TempDir::new().unwrap();
    let storage = Storage::new(dir.path()).unwrap();
    let blocks = storage.blocks();
    let lookup = blocks.assume_valid_ancestor_lookup();

    // Assume-valid header unknown yet: nothing is an ancestor, scripts get verified.
    blocks.store_headers_batch(&fork).unwrap();
    assert_eq!(lookup(2), None);

    blocks.store_headers_batch(&real).unwrap();
    assert_eq!(blocks.get_hash_by_height(2).unwrap(), Some(fork[2].0));
    for h in 0..=AV_HEIGHT {
        assert_eq!(lookup(h), Some(real[h as usize].0), "height {h}");
    }
    assert_ne!(lookup(2), Some(fork[2].0));
    // Above the assume-valid block there is no ancestor.
    assert_eq!(lookup(AV_HEIGHT + 1), None);
}
