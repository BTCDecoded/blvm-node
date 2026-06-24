//! `getchaintips` lists competing tips from the block index.

use blvm_node::node::sync::SyncCoordinator;
use blvm_node::rpc::blockchain::BlockchainRpc;
use blvm_node::storage::Storage;
use blvm_protocol::ConsensusProof;
use blvm_protocol::mining::MiningResult;
use blvm_protocol::segwit::Witness;
use blvm_protocol::serialization::serialize_block_with_witnesses;
use blvm_protocol::types::Network;
use blvm_protocol::{BitcoinProtocolEngine, Block, ProtocolVersion, UtxoSet};
use std::sync::Arc;
use tempfile::TempDir;

const MINE_ATTEMPTS: u64 = 2_000_000;
const MAIN_BLOCKS: u64 = 8;
const FORK_DIVERGE_HEIGHT: u64 = 5;

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
    nonce_salt: u64,
) -> anyhow::Result<(Block, Vec<u8>)> {
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

    let mut coinbase_script = regtest_coinbase_script_sig(connect_height);
    if nonce_salt != 0 {
        coinbase_script.push((nonce_salt & 0xff) as u8);
        coinbase_script.push(((nonce_salt >> 8) & 0xff) as u8);
    }

    let block_time = prev_header.timestamp.saturating_add(600);
    let mut block = consensus.create_new_block_with_time(
        utxo,
        &[],
        connect_height,
        &prev_header,
        &prev_headers,
        &coinbase_script,
        &vec![0x51u8],
        block_time,
        Network::Regtest,
        None,
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

    Ok((mined, wire))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn getchaintips_lists_main_and_orphan_fork() -> anyhow::Result<()> {
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest)?);
    let genesis_header = protocol.get_network_params().genesis_block.header.clone();
    let consensus = ConsensusProof::new();

    let dir = TempDir::new()?;
    let storage = Arc::new(Storage::new(dir.path())?);
    storage.chain().initialize(&genesis_header)?;
    seed_genesis_block(&storage, protocol.as_ref())?;

    let mut utxo = UtxoSet::default();
    let mut coord = SyncCoordinator::new();

    for _ in 0..MAIN_BLOCKS {
        mine_and_connect(
            &storage,
            &mut coord,
            &mut utxo,
            protocol.as_ref(),
            &consensus,
            0,
        )?;
    }

    let main_tip = storage.chain().get_tip_hash()?.unwrap();

    // Side-chain block at diverge height (indexed via process_block, tip unchanged).
    let fork_dir = TempDir::new()?;
    let fork_storage = Arc::new(Storage::new(fork_dir.path())?);
    fork_storage.chain().initialize(&genesis_header)?;
    seed_genesis_block(&fork_storage, protocol.as_ref())?;
    let mut fork_utxo = UtxoSet::default();
    let mut fork_coord = SyncCoordinator::new();
    for _ in 0..(FORK_DIVERGE_HEIGHT - 1) {
        mine_and_connect(
            &fork_storage,
            &mut fork_coord,
            &mut fork_utxo,
            protocol.as_ref(),
            &consensus,
            0,
        )?;
    }
    const FORK_NONCE_SALT: u64 = 1_000_000;
    mine_and_connect(
        &fork_storage,
        &mut fork_coord,
        &mut fork_utxo,
        protocol.as_ref(),
        &consensus,
        FORK_NONCE_SALT,
    )?;

    let fork_hash = fork_storage
        .blocks()
        .get_hash_by_height(FORK_DIVERGE_HEIGHT)?
        .unwrap();
    let main_hash_at_diverge = storage
        .blocks()
        .get_hash_by_height(FORK_DIVERGE_HEIGHT)?
        .unwrap();
    assert_ne!(
        fork_hash, main_hash_at_diverge,
        "fork must diverge by block hash at height {FORK_DIVERGE_HEIGHT}"
    );
    let fork_block = fork_storage.blocks().get_block(&fork_hash)?.unwrap();
    let fork_wire = serialize_block_with_witnesses(&fork_block, &witnesses_for(&fork_block), true);

    let mut replay_utxo = utxo.clone();
    let mut replay_coord = SyncCoordinator::new();
    assert!(replay_coord.process_block(
        storage.blocks().as_ref(),
        protocol.as_ref(),
        Some(&storage),
        &fork_wire,
        FORK_DIVERGE_HEIGHT,
        &mut replay_utxo,
        None,
        None,
    )?);

    assert_eq!(storage.chain().get_tip_hash()?.unwrap(), main_tip);

    let index_tips = storage.chain().block_index().chain_tips()?;
    assert_eq!(
        index_tips.len(),
        2,
        "main tip and orphan fork tip must both appear in block index"
    );

    let rpc = BlockchainRpc::with_dependencies_and_protocol(Arc::clone(&storage), protocol);
    let tips = rpc.get_chain_tips().await?;
    let arr = tips.as_array().expect("getchaintips array");
    assert_eq!(arr.len(), 2, "RPC must expose both tips");

    let statuses: Vec<&str> = arr
        .iter()
        .map(|t| t.get("status").and_then(|s| s.as_str()).unwrap())
        .collect();
    assert!(statuses.contains(&"active"));
    assert!(statuses.contains(&"valid"));

    let fork_rpc_hash = blvm_node::storage::hashing::hash_to_rpc_hex(&fork_hash);
    let fork_entry = arr
        .iter()
        .find(|t| {
            t.get("hash")
                .and_then(|s| s.as_str())
                .is_some_and(|h| h == fork_rpc_hash)
        })
        .expect("fork tip in getchaintips");
    assert_eq!(
        fork_entry.get("height").and_then(|v| v.as_u64()),
        Some(FORK_DIVERGE_HEIGHT)
    );
    assert!(
        fork_entry
            .get("branchlen")
            .and_then(|v| v.as_u64())
            .unwrap()
            > 0
    );

    Ok(())
}
