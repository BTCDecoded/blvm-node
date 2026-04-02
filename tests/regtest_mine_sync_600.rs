//! Regtest: mine 600 blocks on one in-memory node, then replay the same wire bytes on a
//! second node (sync). Exercises consensus mining + `SyncCoordinator::process_block` end-to-end.

use blvm_consensus::mining::MiningResult;
use blvm_consensus::ConsensusProof;
use blvm_node::node::sync::SyncCoordinator;
use blvm_node::storage::Storage;
use blvm_protocol::segwit::Witness;
use blvm_protocol::serialization::serialize_block_with_witnesses;
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion, UtxoSet};
use std::sync::Arc;
use tempfile::TempDir;

const REGTEST_BLOCKS: u64 = 600;
const MINE_ATTEMPTS: u64 = 2_000_000;

/// BIP34 height in coinbase scriptSig (regtest has BIP34 active from height 0).
fn regtest_coinbase_script_sig(height: u64) -> Vec<u8> {
    if height == 0 {
        // OP_0 (BIP34 height 0) + padding: consensus requires coinbase scriptSig length 2..=100.
        return vec![0x00, 0xff];
    }
    let mut n = height;
    let mut height_bytes = Vec::new();
    while n > 0 {
        height_bytes.push((n & 0xff) as u8);
        n >>= 8;
    }
    if height_bytes.last().map_or(false, |&b| b & 0x80 != 0) {
        height_bytes.push(0x00);
    }
    let mut script_sig = Vec::with_capacity(1 + height_bytes.len());
    script_sig.push(height_bytes.len() as u8);
    script_sig.extend_from_slice(&height_bytes);
    if script_sig.len() < 2 {
        script_sig.push(0x00);
    }
    script_sig
}

fn bootstrap_miner_storage(
    storage: &Arc<Storage>,
    genesis_header: &blvm_protocol::BlockHeader,
) -> anyhow::Result<()> {
    storage.chain().initialize(genesis_header)?;
    Ok(())
}

fn bootstrap_follower_storage(
    storage: &Arc<Storage>,
    genesis_header: &blvm_protocol::BlockHeader,
) -> anyhow::Result<()> {
    storage.chain().initialize(genesis_header)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn regtest_mine_600_blocks_then_sync_follower() -> anyhow::Result<()> {
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest)?);
    let genesis = protocol.get_network_params().genesis_block.clone();
    let genesis_header = genesis.header.clone();

    let consensus = ConsensusProof::new();

    let miner_dir = TempDir::new()?;
    let follower_dir = TempDir::new()?;
    let storage_m = Arc::new(Storage::new(miner_dir.path())?);
    let storage_f = Arc::new(Storage::new(follower_dir.path())?);

    bootstrap_miner_storage(&storage_m, &genesis_header)?;
    bootstrap_follower_storage(&storage_f, &genesis_header)?;

    let mut miner_utxo = UtxoSet::default();
    let mut follower_utxo = UtxoSet::default();
    let mut coord_m = SyncCoordinator::new();
    let mut coord_f = SyncCoordinator::new();

    let mut wire_blocks: Vec<Vec<u8>> = Vec::with_capacity(REGTEST_BLOCKS as usize);

    for connect_height in 0..REGTEST_BLOCKS {
        let stored: u64 = storage_m.blocks().block_count()? as u64;
        assert_eq!(
            stored, connect_height,
            "miner blockstore height index vs loop"
        );

        let prev_header = storage_m
            .chain()
            .get_tip_header()?
            .ok_or_else(|| anyhow::anyhow!("missing tip header"))?;

        let mut prev_headers = storage_m
            .blocks()
            .get_recent_headers(2016)
            .unwrap_or_default();
        if prev_headers.len() < 2 {
            prev_headers = vec![prev_header.clone(), prev_header.clone()];
        }

        let coinbase_script = regtest_coinbase_script_sig(connect_height);
        let coinbase_address = vec![0x51u8];

        // Use `create_new_block` (not `create_block_template`): template path calls
        // `mining::expand_target` into u128, which cannot represent regtest nBits (0x207fffff).
        let mut block = consensus.create_new_block(
            &miner_utxo,
            &[],
            connect_height,
            &prev_header,
            &prev_headers,
            &coinbase_script,
            &coinbase_address,
        )?;
        // BIP90 on regtest: BIP34/BIP66/BIP65 active from height 0 → need version >= 4.
        block.header.version = 4;

        let (mined, result) = consensus.mine_block(block, MINE_ATTEMPTS)?;
        assert!(
            matches!(result, MiningResult::Success),
            "PoW failed at connect_height {connect_height} (regtest target should be easy)"
        );

        let witnesses: Vec<Vec<Witness>> = mined
            .transactions
            .iter()
            .map(|tx| tx.inputs.iter().map(|_| Witness::default()).collect())
            .collect();
        let wire = serialize_block_with_witnesses(&mined, &witnesses, true);

        let accepted = coord_m.process_block(
            storage_m.blocks().as_ref(),
            protocol.as_ref(),
            Some(&storage_m),
            &wire,
            connect_height,
            &mut miner_utxo,
            None,
            None,
        )?;
        assert!(accepted, "miner rejected block at {connect_height}");

        let block_hash = storage_m.blocks().as_ref().get_block_hash(&mined);
        storage_m
            .chain()
            .update_tip(&block_hash, &mined.header, connect_height)?;
        storage_m.utxos().store_utxo_set(&miner_utxo)?;

        wire_blocks.push(wire);
    }

    assert_eq!(storage_m.chain().get_height()?.unwrap(), REGTEST_BLOCKS - 1);
    assert_eq!(wire_blocks.len() as u64, REGTEST_BLOCKS);

    let miner_tip = storage_m.chain().get_tip_hash()?.unwrap();

    for connect_height in 0..REGTEST_BLOCKS {
        let wire = &wire_blocks[connect_height as usize];
        let accepted = coord_f.process_block(
            storage_f.blocks().as_ref(),
            protocol.as_ref(),
            Some(&storage_f),
            wire,
            connect_height,
            &mut follower_utxo,
            None,
            None,
        )?;
        assert!(accepted, "follower rejected block at {connect_height}");
        let (block, _) =
            blvm_protocol::serialization::block::deserialize_block_with_witnesses(wire)?;
        let block_hash = storage_f.blocks().as_ref().get_block_hash(&block);
        storage_f
            .chain()
            .update_tip(&block_hash, &block.header, connect_height)?;
        storage_f.utxos().store_utxo_set(&follower_utxo)?;
    }

    assert_eq!(storage_f.chain().get_height()?.unwrap(), REGTEST_BLOCKS - 1);
    let follower_tip = storage_f.chain().get_tip_hash()?.unwrap();
    assert_eq!(
        follower_tip, miner_tip,
        "follower tip hash must match miner after full replay"
    );

    Ok(())
}
