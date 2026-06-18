//! Blockstore height index vs per-hash block bodies.
//!
//! Verifies `store_height` last-write-wins on `height_index` while both block bodies
//! remain loadable by hash (`block_height_row_key`).

use blvm_node::node::block_processor::store_block_with_context;
use blvm_node::storage::Storage;
use blvm_node::{Block, BlockHeader, Transaction};
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use std::sync::Arc;
use tempfile::TempDir;

fn coinbase_block(prev_hash: [u8; 32], nonce: u64) -> Block {
    Block {
        header: BlockHeader {
            version: 4,
            prev_block_hash: prev_hash,
            merkle_root: [0xab; 32],
            timestamp: 1_600_000_000,
            bits: 0x207fffff,
            nonce,
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![],
            lock_time: 0,
        }]
        .into_boxed_slice(),
    }
}

#[test]
fn height_index_last_write_wins_both_blocks_retrievable_by_hash() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let storage = Arc::new(Storage::new(temp_dir.path())?);
    let protocol = BitcoinProtocolEngine::new(ProtocolVersion::Regtest)?;
    let genesis = protocol.get_network_params().genesis_block.clone();
    storage.chain().initialize(&genesis.header)?;

    let blockstore = storage.blocks();
    let witnesses: Vec<Vec<blvm_protocol::segwit::Witness>> = vec![];

    store_block_with_context(&blockstore, &genesis, &witnesses, 0)?;
    let genesis_hash = blockstore.get_block_hash(&genesis);

    let block_a = coinbase_block(genesis_hash, 1);
    let block_b = coinbase_block(genesis_hash, 2);
    let hash_a = blockstore.get_block_hash(&block_a);
    let hash_b = blockstore.get_block_hash(&block_b);
    assert_ne!(hash_a, hash_b, "competing blocks must differ by hash");

    store_block_with_context(&blockstore, &block_a, &witnesses, 1)?;
    store_block_with_context(&blockstore, &block_b, &witnesses, 1)?;

    assert!(
        blockstore.get_block(&hash_a)?.is_some(),
        "first block at height must remain retrievable by hash"
    );
    assert!(
        blockstore.get_block(&hash_b)?.is_some(),
        "second block at height must remain retrievable by hash"
    );
    assert_eq!(
        blockstore.get_hash_by_height(1)?,
        Some(hash_b),
        "height_index must reflect the last store_height call (last-write-wins)"
    );

    Ok(())
}
