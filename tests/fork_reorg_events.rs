//! Regtest integration: reorg emits BlockDisconnected and ChainReorg module events.
use blvm_node::module::api::events::EventManager;
use blvm_node::module::ipc::protocol::{EventMessage, EventPayload, ModuleMessage};
use blvm_node::module::traits::EventType;
use blvm_node::node::event_publisher::EventPublisher;
use blvm_node::node::sync::SyncCoordinator;
use blvm_node::storage::Storage;
use blvm_protocol::ConsensusProof;
use blvm_protocol::mining::MiningResult;
use blvm_protocol::segwit::Witness;
use blvm_protocol::serialization::serialize_block_with_witnesses;
use blvm_protocol::{BitcoinProtocolEngine, Block, ProtocolVersion, UtxoSet};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

const MINE_ATTEMPTS: u64 = 2_000_000;
const LINEAR_BLOCK_COUNT: u64 = 12;
const FORK_DIVERGE_HEIGHT: u64 = 5;
const MAIN_TIP_HEIGHT: u64 = LINEAR_BLOCK_COUNT;
const FORK_TIP_HEIGHT: u64 = MAIN_TIP_HEIGHT + 1;
const EXPECTED_REORG_DEPTH: usize = (MAIN_TIP_HEIGHT - FORK_DIVERGE_HEIGHT + 1) as usize;

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

    storage.utxos().store_utxo_set(utxo)?;

    Ok((mined, wire))
}

fn connect_wire(
    storage: &Arc<Storage>,
    coord: &mut SyncCoordinator,
    utxo: &mut UtxoSet,
    protocol: &BitcoinProtocolEngine,
    wire: &[u8],
) -> anyhow::Result<Block> {
    let connect_height = storage.chain().get_height()?.unwrap_or(0) + 1;
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
    storage.utxos().store_utxo_set(utxo)?;
    Ok(block)
}

async fn recv_event(
    rx: &mut mpsc::Receiver<ModuleMessage>,
) -> anyhow::Result<(EventType, EventPayload)> {
    let msg = timeout(Duration::from_secs(2), rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for reorg event"))?
        .ok_or_else(|| anyhow::anyhow!("event channel closed"))?;
    match msg {
        ModuleMessage::Event(EventMessage {
            event_type,
            payload,
        }) => Ok((event_type, payload)),
        other => anyhow::bail!("unexpected module message: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_reorg_event_delivery_smoke() -> anyhow::Result<()> {
    let event_manager = Arc::new(EventManager::new());
    let (tx, mut rx) = mpsc::channel(8);
    event_manager
        .subscribe_module("smoke".to_string(), vec![EventType::ChainReorg], tx)
        .await?;

    let publisher = Arc::new(EventPublisher::new(Arc::clone(&event_manager)));
    let dir = TempDir::new()?;
    let storage = Storage::new(dir.path())?;
    let blockstore = storage.blocks();

    let old_tip = [1u8; 32];
    let new_tip = [2u8; 32];
    blvm_node::node::reorg_executor::publish_reorg_events(
        Some(&publisher),
        &storage,
        blockstore.as_ref(),
        &old_tip,
        &new_tip,
        &[],
    );

    tokio::time::sleep(Duration::from_millis(50)).await;
    let (event_type, payload) = recv_event(&mut rx).await?;
    assert_eq!(event_type, EventType::ChainReorg);
    match payload {
        EventPayload::ChainReorg {
            old_tip: o,
            new_tip: n,
        } => {
            assert_eq!(o, old_tip);
            assert_eq!(n, new_tip);
        }
        other => anyhow::bail!("expected ChainReorg, got {other:?}"),
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reorg_publishes_disconnect_and_chain_reorg_events() -> anyhow::Result<()> {
    let event_manager = Arc::new(EventManager::new());
    let (tx, mut rx) = mpsc::channel(32);
    event_manager
        .subscribe_module(
            "fork-reorg-test".to_string(),
            vec![EventType::BlockDisconnected, EventType::ChainReorg],
            tx,
        )
        .await?;

    let publisher = Arc::new(EventPublisher::new(Arc::clone(&event_manager)));

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
    let mut prefix_wires: Vec<Vec<u8>> = Vec::new();

    for _ in 0..LINEAR_BLOCK_COUNT {
        let (_, wire) = mine_and_connect(
            &storage_main,
            &mut main_coord,
            &mut main_utxo,
            &protocol,
            &consensus,
            0,
        )?;
        if storage_main.chain().get_height()?.unwrap_or(0) < FORK_DIVERGE_HEIGHT {
            prefix_wires.push(wire);
        }
    }

    let main_tip = storage_main.chain().get_tip_hash()?.unwrap();

    let mut fork_utxo = UtxoSet::default();
    let mut fork_coord = SyncCoordinator::new();
    for wire in prefix_wires.iter() {
        connect_wire(
            &storage_fork,
            &mut fork_coord,
            &mut fork_utxo,
            &protocol,
            wire,
        )?;
    }

    const FORK_NONCE_SALT: u64 = 1_000_000;
    for _ in FORK_DIVERGE_HEIGHT..=FORK_TIP_HEIGHT {
        mine_and_connect(
            &storage_fork,
            &mut fork_coord,
            &mut fork_utxo,
            &protocol,
            &consensus,
            FORK_NONCE_SALT,
        )?;
    }

    let fork_tip = storage_fork.chain().get_tip_hash()?.unwrap();
    assert_ne!(fork_tip, main_tip);

    let mut replay_utxo = main_utxo.clone();
    let mut replay_coord = SyncCoordinator::new();
    replay_coord.set_event_publisher(Some(Arc::clone(&publisher)));

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

    // Reorg events are published via spawned tasks from sync `process_block`.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut disconnect_heights = Vec::new();
    let (old_tip, new_tip) = loop {
        let (event_type, payload) = recv_event(&mut rx).await?;
        match (event_type, payload) {
            (EventType::BlockDisconnected, EventPayload::BlockDisconnected { height, .. }) => {
                disconnect_heights.push(height);
            }
            (EventType::ChainReorg, EventPayload::ChainReorg { old_tip, new_tip }) => {
                break (old_tip, new_tip);
            }
            (other, _) => anyhow::bail!("unexpected event {:?}", other),
        }
    };

    assert_eq!(
        disconnect_heights.len(),
        EXPECTED_REORG_DEPTH,
        "one BlockDisconnected per disconnected main-chain block"
    );
    assert!(
        disconnect_heights.windows(2).all(|w| w[0] > w[1]),
        "disconnect events must be tip-first: {disconnect_heights:?}"
    );
    assert_eq!(
        disconnect_heights[0], MAIN_TIP_HEIGHT,
        "first disconnect must be the former active tip height"
    );
    assert_eq!(
        *disconnect_heights.last().unwrap(),
        FORK_DIVERGE_HEIGHT,
        "last disconnect must be the first obsolete main-chain block after the fork point"
    );

    assert_eq!(old_tip, main_tip);
    assert_eq!(new_tip, fork_tip);

    Ok(())
}
