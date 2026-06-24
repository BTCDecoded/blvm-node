//! Equal-work sibling at height H keeps first-connected tip; child on the lagging fork wins.

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
/// Shared prefix ends at height 7; siblings connect at height 8.
const PREFIX_HEIGHT: u64 = 7;
const SIBLING_HEIGHT: u64 = 8;
const CHILD_HEIGHT: u64 = 9;
const FORK_SALT: u64 = 42_000;

fn regtest_coinbase_script_sig(height: u64, fork_salt: u64) -> Vec<u8> {
    let mut script = if height == 0 {
        vec![0x00, 0xff]
    } else {
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
    };
    if fork_salt != 0 {
        script.extend_from_slice(&fork_salt.to_le_bytes());
    }
    script
}

fn witnesses_for(block: &Block) -> Vec<Vec<Witness>> {
    block
        .transactions
        .iter()
        .map(|tx| tx.inputs.iter().map(|_| Witness::default()).collect())
        .collect()
}

fn seed_genesis_block(storage: &Storage, protocol: &BitcoinProtocolEngine) -> anyhow::Result<()> {
    let genesis = protocol.get_network_params().genesis_block.clone();
    let hash = storage.blocks().get_block_hash(&genesis);
    storage.blocks().store_block(&genesis)?;
    storage.blocks().store_height(0, &hash)?;
    storage.blocks().store_recent_header(0, &genesis.header)?;
    let witnesses: Vec<Vec<Witness>> = genesis
        .transactions
        .iter()
        .map(|tx| tx.inputs.iter().map(|_| Witness::default()).collect())
        .collect();
    storage.blocks().store_witness(&hash, &witnesses)?;
    Ok(())
}

fn mine_and_connect(
    storage: &Arc<Storage>,
    coord: &mut SyncCoordinator,
    utxo: &mut UtxoSet,
    protocol: &BitcoinProtocolEngine,
    consensus: &ConsensusProof,
    fork_salt: u64,
) -> anyhow::Result<Block> {
    let connect_height = storage.chain().get_height()?.unwrap_or(0) + 1;
    let prev_header = storage
        .chain()
        .get_tip_header()?
        .ok_or_else(|| anyhow::anyhow!("missing tip header"))?;

    let mut prev_headers = storage
        .blocks()
        .get_recent_headers(2016)
        .unwrap_or_default();
    if prev_headers.len() < 2 {
        prev_headers = vec![prev_header.clone(), prev_header.clone()];
    }

    let coinbase_script = regtest_coinbase_script_sig(connect_height, fork_salt);
    let mut block = consensus.create_new_block(
        utxo,
        &[],
        connect_height,
        &prev_header,
        &prev_headers,
        &coinbase_script,
        &vec![0x51u8],
    )?;
    block.header.version = 4;

    let (mined, result) = consensus.mine_block(block, MINE_ATTEMPTS)?;
    assert!(matches!(result, MiningResult::Success));

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
    assert!(accepted, "process_block rejected height {connect_height}");

    storage.utxos().store_utxo_set(utxo)?;
    Ok(mined)
}

fn import_side_chain_block(
    storage: &Arc<Storage>,
    coord: &mut SyncCoordinator,
    utxo: &mut UtxoSet,
    protocol: &BitcoinProtocolEngine,
    block: &Block,
) -> anyhow::Result<()> {
    let connect_height = storage.chain().get_height()?.unwrap_or(0) + 1;
    let wire = serialize_block_with_witnesses(block, &witnesses_for(block), true);
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
    assert!(accepted, "side-chain block at height {connect_height}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equal_work_sibling_keeps_first_tip_child_on_lagging_fork_wins() -> anyhow::Result<()> {
    let protocol = BitcoinProtocolEngine::new(ProtocolVersion::Regtest)?;
    let genesis_header = protocol.get_network_params().genesis_block.header.clone();
    let consensus = ConsensusProof::new();

    let main_dir = TempDir::new()?;
    let fork_dir = TempDir::new()?;
    let storage_main = Arc::new(Storage::new(main_dir.path())?);
    let storage_fork = Arc::new(Storage::new(fork_dir.path())?);
    storage_main.chain().initialize(&genesis_header)?;
    storage_fork.chain().initialize(&genesis_header)?;
    seed_genesis_block(&storage_main, &protocol)?;
    seed_genesis_block(&storage_fork, &protocol)?;

    let mut main_utxo = UtxoSet::default();
    let mut main_coord = SyncCoordinator::new();
    for _ in 0..PREFIX_HEIGHT {
        mine_and_connect(
            &storage_main,
            &mut main_coord,
            &mut main_utxo,
            &protocol,
            &consensus,
            0,
        )?;
    }

    let block_a = mine_and_connect(
        &storage_main,
        &mut main_coord,
        &mut main_utxo,
        &protocol,
        &consensus,
        0,
    )?;
    let hash_a = storage_main.blocks().get_block_hash(&block_a);
    let work_a = storage_main
        .chain()
        .get_chainwork(&hash_a)?
        .unwrap_or_else(blvm_consensus::pow::U256::zero);

    let mut fork_utxo = UtxoSet::default();
    let mut fork_coord = SyncCoordinator::new();
    for _ in 0..PREFIX_HEIGHT {
        mine_and_connect(
            &storage_fork,
            &mut fork_coord,
            &mut fork_utxo,
            &protocol,
            &consensus,
            0,
        )?;
    }

    let block_b = mine_and_connect(
        &storage_fork,
        &mut fork_coord,
        &mut fork_utxo,
        &protocol,
        &consensus,
        FORK_SALT,
    )?;
    let hash_b = storage_fork.blocks().get_block_hash(&block_b);
    let work_b = storage_fork
        .chain()
        .get_chainwork(&hash_b)?
        .unwrap_or_else(blvm_consensus::pow::U256::zero);
    assert_eq!(
        work_a, work_b,
        "siblings must carry equal cumulative chainwork"
    );
    assert_ne!(hash_a, hash_b);

    let block_c = mine_and_connect(
        &storage_fork,
        &mut fork_coord,
        &mut fork_utxo,
        &protocol,
        &consensus,
        FORK_SALT,
    )?;
    let hash_c = storage_fork.blocks().get_block_hash(&block_c);

    let mut replay_utxo = main_utxo.clone();
    let mut replay_coord = SyncCoordinator::new();

    import_side_chain_block(
        &storage_main,
        &mut replay_coord,
        &mut replay_utxo,
        &protocol,
        &block_b,
    )?;

    assert_eq!(
        storage_main.chain().get_tip_hash()?.unwrap(),
        hash_a,
        "equal-work sibling must not displace the first-connected tip"
    );
    let seq_a = storage_main
        .chain()
        .block_index()
        .get(&hash_a)?
        .unwrap()
        .sequence_id;
    let seq_b = storage_main
        .chain()
        .block_index()
        .get(&hash_b)?
        .unwrap()
        .sequence_id;
    assert!(seq_a < seq_b);

    import_side_chain_block(
        &storage_main,
        &mut replay_coord,
        &mut replay_utxo,
        &protocol,
        &block_c,
    )?;

    assert_eq!(
        storage_main.chain().get_tip_hash()?.unwrap(),
        hash_c,
        "heavier child on the lagging fork must become the active tip"
    );

    Ok(())
}
