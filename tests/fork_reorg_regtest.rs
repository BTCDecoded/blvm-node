//! Regtest integration: linear sync control and higher-work fork gap test.

use blvm_node::node::sync::SyncCoordinator;
use blvm_node::storage::Storage;
use blvm_protocol::ConsensusProof;
use blvm_protocol::mining::MiningResult;
use blvm_protocol::segwit::Witness;
use blvm_protocol::serialization::serialize_block_with_witnesses;
use blvm_protocol::{BitcoinProtocolEngine, Block, ProtocolVersion, UtxoSet};
use std::sync::Arc;
use tempfile::TempDir;

const MINE_ATTEMPTS: u64 = 2_000_000;
/// Shorter than `regtest_mine_sync_600` — enough blocks for a mid-chain fork.
const LINEAR_BLOCK_COUNT: u64 = 12;
/// Shared prefix heights `0..=3`; first divergent connect height is `4`.
const FORK_DIVERGE_HEIGHT: u64 = 4;
/// Main chain tip height after `LINEAR_BLOCK_COUNT` blocks (`0..=11`).
const MAIN_TIP_HEIGHT: u64 = LINEAR_BLOCK_COUNT - 1;
/// Fork extends one block past main after the diverge point (`0..=12`).
const FORK_TIP_HEIGHT: u64 = MAIN_TIP_HEIGHT + 1;

fn regtest_coinbase_script_sig(height: u64) -> Vec<u8> {
    if height == 0 {
        return vec![0x00, 0xff];
    }
    let mut n = height;
    let mut height_bytes = Vec::new();
    while n > 0 {
        height_bytes.push((n & 0xff) as u8);
        n >>= 8;
    }
    if height_bytes.last().is_some_and(|&b| b & 0x80 != 0) {
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

fn witnesses_for(block: &Block) -> Vec<Vec<Witness>> {
    block
        .transactions
        .iter()
        .map(|tx| tx.inputs.iter().map(|_| Witness::default()).collect())
        .collect()
}

fn mine_and_connect(
    storage: &Arc<Storage>,
    coord: &mut SyncCoordinator,
    utxo: &mut UtxoSet,
    protocol: &BitcoinProtocolEngine,
    consensus: &ConsensusProof,
    connect_height: u64,
    nonce_salt: u64,
) -> anyhow::Result<(Block, Vec<u8>)> {
    let prev_header = storage
        .chain()
        .get_tip_header()?
        .ok_or_else(|| anyhow::anyhow!("missing tip header at height {}", connect_height))?;

    let mut prev_headers = storage
        .blocks()
        .get_recent_headers(2016)
        .unwrap_or_default();
    if prev_headers.len() < 2 {
        prev_headers = vec![prev_header.clone(), prev_header.clone()];
    }

    let mut coinbase_script = regtest_coinbase_script_sig(connect_height);
    if nonce_salt != 0 {
        coinbase_script.extend_from_slice(&nonce_salt.to_le_bytes());
    }

    let coinbase_address = vec![0x51u8];

    let mut block = consensus.create_new_block(
        utxo,
        &[],
        connect_height,
        &prev_header,
        &prev_headers,
        &coinbase_script,
        &coinbase_address,
    )?;
    block.header.version = 4;

    let (mined, result) = consensus.mine_block(block, MINE_ATTEMPTS)?;
    assert!(
        matches!(result, MiningResult::Success),
        "PoW failed at connect_height {connect_height} (nonce_salt={nonce_salt})"
    );

    let witnesses = witnesses_for(&mined);
    let wire = serialize_block_with_witnesses(&mined, &witnesses, true);

    let accepted = coord.process_block(
        storage.blocks().as_ref(),
        protocol,
        Some(storage),
        &wire,
        connect_height,
        utxo,
        None,
        None,
    )?;
    assert!(
        accepted,
        "process_block rejected height {connect_height} (nonce_salt={nonce_salt})"
    );

    let block_hash = storage.blocks().as_ref().get_block_hash(&mined);
    storage
        .chain()
        .update_tip(&block_hash, &mined.header, connect_height)?;
    storage.utxos().store_utxo_set(utxo)?;

    Ok((mined, wire))
}

fn connect_wire(
    storage: &Arc<Storage>,
    coord: &mut SyncCoordinator,
    utxo: &mut UtxoSet,
    protocol: &BitcoinProtocolEngine,
    wire: &[u8],
    connect_height: u64,
) -> anyhow::Result<Block> {
    let accepted = coord.process_block(
        storage.blocks().as_ref(),
        protocol,
        Some(storage),
        wire,
        connect_height,
        utxo,
        None,
        None,
    )?;
    assert!(accepted, "replay rejected at height {connect_height}");
    let (block, _) = blvm_protocol::serialization::block::deserialize_block_with_witnesses(wire)?;
    let block_hash = storage.blocks().as_ref().get_block_hash(&block);
    storage
        .chain()
        .update_tip(&block_hash, &block.header, connect_height)?;
    storage.utxos().store_utxo_set(utxo)?;
    Ok(block)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn linear_chain_control() -> anyhow::Result<()> {
    let protocol = BitcoinProtocolEngine::new(ProtocolVersion::Regtest)?;
    let genesis_header = protocol.get_network_params().genesis_block.header.clone();
    let consensus = ConsensusProof::new();

    let dir = TempDir::new()?;
    let storage = Arc::new(Storage::new(dir.path())?);
    storage.chain().initialize(&genesis_header)?;

    let mut utxo = UtxoSet::default();
    let mut coord = SyncCoordinator::new();

    for connect_height in 0..LINEAR_BLOCK_COUNT {
        mine_and_connect(
            &storage,
            &mut coord,
            &mut utxo,
            &protocol,
            &consensus,
            connect_height,
            0,
        )?;
    }

    assert_eq!(
        storage.chain().get_height()?.unwrap(),
        MAIN_TIP_HEIGHT,
        "tip height after {LINEAR_BLOCK_COUNT} linear connects"
    );
    let tip = storage.chain().get_tip_hash()?.unwrap();
    assert_eq!(
        storage.blocks().get_hash_by_height(MAIN_TIP_HEIGHT)?,
        Some(tip),
        "height index must match chain tip hash"
    );

    Ok(())
}

/// Higher-work fork: after importing the fork branch, the active tip follows the heavier chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn higher_work_fork_from_mid_chain() -> anyhow::Result<()> {
    let protocol = BitcoinProtocolEngine::new(ProtocolVersion::Regtest)?;
    let genesis_header = protocol.get_network_params().genesis_block.header.clone();
    let consensus = ConsensusProof::new();

    let main_dir = TempDir::new()?;
    let fork_dir = TempDir::new()?;
    let storage_main = Arc::new(Storage::new(main_dir.path())?);
    let storage_fork = Arc::new(Storage::new(fork_dir.path())?);
    storage_main.chain().initialize(&genesis_header)?;
    storage_fork.chain().initialize(&genesis_header)?;

    let mut main_utxo = UtxoSet::default();
    let mut main_coord = SyncCoordinator::new();
    let mut prefix_wires: Vec<Vec<u8>> = Vec::new();

    for connect_height in 0..LINEAR_BLOCK_COUNT {
        let (_, wire) = mine_and_connect(
            &storage_main,
            &mut main_coord,
            &mut main_utxo,
            &protocol,
            &consensus,
            connect_height,
            0,
        )?;
        if connect_height < FORK_DIVERGE_HEIGHT {
            prefix_wires.push(wire);
        }
    }

    let main_tip = storage_main.chain().get_tip_hash()?.unwrap();
    let main_work = storage_main.chain().get_chainwork(&main_tip)?.unwrap_or(0);

    let mut fork_utxo = UtxoSet::default();
    let mut fork_coord = SyncCoordinator::new();
    for (h, wire) in prefix_wires.iter().enumerate() {
        connect_wire(
            &storage_fork,
            &mut fork_coord,
            &mut fork_utxo,
            &protocol,
            wire,
            h as u64,
        )?;
    }

    const FORK_NONCE_SALT: u64 = 1_000_000;
    for connect_height in FORK_DIVERGE_HEIGHT..=FORK_TIP_HEIGHT {
        mine_and_connect(
            &storage_fork,
            &mut fork_coord,
            &mut fork_utxo,
            &protocol,
            &consensus,
            connect_height,
            FORK_NONCE_SALT,
        )?;
    }

    let fork_tip = storage_fork.chain().get_tip_hash()?.unwrap();
    let fork_work = storage_fork.chain().get_chainwork(&fork_tip)?.unwrap_or(0);

    assert_ne!(fork_tip, main_tip, "fork must diverge by tip hash");
    assert!(
        fork_work > main_work,
        "fork must carry more cumulative chainwork (fork={fork_work}, main={main_work})"
    );

    // Import fork branch blocks onto the main node (side chain until the fork wins on work).
    let mut replay_utxo = main_utxo.clone();
    let mut replay_coord = SyncCoordinator::new();
    for connect_height in FORK_DIVERGE_HEIGHT..=FORK_TIP_HEIGHT {
        let fork_hash = storage_fork
            .blocks()
            .get_hash_by_height(connect_height)?
            .unwrap();
        let fork_block = storage_fork.blocks().get_block(&fork_hash)?.unwrap();
        let wire = serialize_block_with_witnesses(&fork_block, &witnesses_for(&fork_block), true);
        let accepted = replay_coord.process_block(
            storage_main.blocks().as_ref(),
            &protocol,
            Some(&storage_main),
            &wire,
            connect_height,
            &mut replay_utxo,
            None,
            None,
        )?;
        assert!(
            accepted,
            "fork block at height {connect_height} must be accepted"
        );
    }

    assert_eq!(
        storage_main.chain().get_tip_hash()?.unwrap(),
        fork_tip,
        "active tip must follow the heavier fork after reorg"
    );

    Ok(())
}
