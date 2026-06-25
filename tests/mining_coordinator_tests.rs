//! Tests for mining coordinator

use blvm_node::node::mempool::MempoolManager;
use blvm_node::node::miner::{
    MempoolProvider, MiningCoordinator, MiningEngine, TransactionSelector,
};
use blvm_protocol::opcodes::{OP_0, OP_CHECKSIG};
use blvm_protocol::segwit::Witness;
use blvm_protocol::{
    BitcoinProtocolEngine, ProtocolVersion, Transaction, TransactionInput, TransactionOutput,
};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

#[test]
fn test_transaction_selector_new() {
    let selector = TransactionSelector::new();
    assert_eq!(selector.max_block_size(), 1_000_000);
    assert_eq!(selector.max_block_weight(), 4_000_000);
    assert_eq!(selector.min_fee_rate(), 1);
}

#[test]
fn test_transaction_selector_with_params() {
    let selector = TransactionSelector::with_params(2_000_000, 8_000_000, 10);
    assert_eq!(selector.max_block_size(), 2_000_000);
    assert_eq!(selector.max_block_weight(), 8_000_000);
    assert_eq!(selector.min_fee_rate(), 10);
}

// select_transactions() integration: real MempoolManager + UTXO fee path.

#[test]
fn test_transaction_selector_selects_mempool_tx() {
    let mempool = create_test_mempool();
    let selector = TransactionSelector::new();

    let funding_hash = [0x11u8; 32];
    let utxo_set = {
        use blvm_protocol::{OutPoint, UTXO};
        let mut set = blvm_protocol::UtxoSet::default();
        set.insert(
            OutPoint {
                hash: funding_hash,
                index: 0,
            },
            Arc::new(UTXO {
                value: 100_000,
                script_pubkey: vec![OP_0, OP_CHECKSIG].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        set
    };

    let tx = Transaction {
        version: 2,
        inputs: vec![TransactionInput {
            prevout: blvm_protocol::OutPoint {
                hash: funding_hash,
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xfffffffe,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 90_000,
            script_pubkey: vec![OP_0, OP_CHECKSIG],
        }]
        .into(),
        lock_time: 0,
    };

    mempool
        .add_transaction(tx.clone())
        .expect("add_transaction ok");

    let selected = selector.select_transactions(&*mempool as &dyn MempoolProvider, &utxo_set);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].version, tx.version);
}

#[test]
fn test_mining_engine_new() {
    let engine = MiningEngine::new();
    assert!(!engine.is_mining_enabled());
    assert_eq!(engine.get_threads(), 1);
}

#[test]
fn test_mining_engine_with_threads() {
    let engine = MiningEngine::with_threads(4);
    assert_eq!(engine.get_threads(), 4);
}

#[test]
fn test_mining_engine_enable_disable() {
    let mut engine = MiningEngine::new();
    assert!(!engine.is_mining_enabled());

    engine.enable_mining();
    assert!(engine.is_mining_enabled());

    engine.disable_mining();
    assert!(!engine.is_mining_enabled());
}

#[test]
fn test_mining_engine_set_threads() {
    let mut engine = MiningEngine::new();
    assert_eq!(engine.get_threads(), 1);

    engine.set_threads(8);
    assert_eq!(engine.get_threads(), 8);
}

#[test]
fn test_mining_engine_get_stats() {
    let engine = MiningEngine::new();
    let stats = engine.get_stats();

    // Stats should be initialized
    assert_eq!(stats.blocks_mined, 0);
    assert_eq!(stats.total_hashrate, 0.0);
}

#[test]
fn test_mining_engine_clear_template() {
    let mut engine = MiningEngine::new();

    // Template should be None initially
    assert!(engine.get_block_template().is_none());

    // Clear should not panic
    engine.clear_template();
    assert!(engine.get_block_template().is_none());
}

#[test]
fn test_mining_engine_update_hashrate() {
    let mut engine = MiningEngine::new();

    engine.update_hashrate(100.0);
    let stats = engine.get_stats();
    assert_eq!(stats.total_hashrate, 100.0);
}

#[test]
fn test_mining_engine_update_average_block_time() {
    let mut engine = MiningEngine::new();

    engine.update_average_block_time(10.5);
    let stats = engine.get_stats();
    assert_eq!(stats.average_block_time, 10.5);
}

fn create_test_mempool() -> Arc<MempoolManager> {
    Arc::new(MempoolManager::new())
}

#[test]
fn test_mining_coordinator_new() {
    let mempool = create_test_mempool();
    let coordinator = MiningCoordinator::new(mempool, None);

    assert!(!coordinator.is_mining_enabled());
}

#[test]
fn test_mining_coordinator_enable_disable() {
    let mempool = create_test_mempool();
    let mut coordinator = MiningCoordinator::new(mempool, None);

    assert!(!coordinator.is_mining_enabled());

    coordinator.enable_mining();
    assert!(coordinator.is_mining_enabled());

    coordinator.disable_mining();
    assert!(!coordinator.is_mining_enabled());
}

#[tokio::test]
async fn test_mining_coordinator_generate_block_template_with_storage() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(blvm_node::storage::Storage::new(temp_dir.path()).unwrap());
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let genesis = protocol.get_network_params().genesis_block.header.clone();
    storage.chain().initialize(&genesis).unwrap();

    let mempool = create_test_mempool();
    let mut coordinator = MiningCoordinator::new(mempool, Some(storage));
    coordinator.set_protocol_engine(protocol);

    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let template = coordinator
        .generate_block_template()
        .await
        .expect("template generation");

    assert!(!template.transactions.is_empty());
    assert!(
        template.header.timestamp >= before.saturating_sub(5),
        "template must use live timestamp, got {}",
        template.header.timestamp
    );
    assert_ne!(template.header.timestamp, 1_231_006_505);
}

#[tokio::test]
async fn test_mining_coordinator_mempool_witness_storage() {
    let mempool = create_test_mempool();
    let funding_hash = [0x22u8; 32];
    let tx = Transaction {
        version: 2,
        inputs: vec![TransactionInput {
            prevout: blvm_protocol::OutPoint {
                hash: funding_hash,
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xfffffffe,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 50_000,
            script_pubkey: vec![OP_0, OP_CHECKSIG],
        }]
        .into(),
        lock_time: 0,
    };
    use blvm_protocol::block::calculate_tx_id;
    let txid = calculate_tx_id(&tx);
    let witness_stack: Witness = vec![vec![0x01, 0x02]];
    let witnesses = vec![witness_stack];

    mempool
        .add_transaction_with_witness(tx, Some(witnesses.clone()))
        .expect("add with witness");

    let stored = mempool.get_transaction_witnesses(&txid);
    assert_eq!(stored, Some(witnesses));
}

#[tokio::test]
async fn test_mining_coordinator_template_selects_mempool_tx() {
    use blvm_protocol::opcodes::{OP_0, OP_CHECKSIG};
    use blvm_protocol::{OutPoint, UTXO};

    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(blvm_node::storage::Storage::new(temp_dir.path()).unwrap());
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let genesis = protocol.get_network_params().genesis_block.header.clone();
    storage.chain().initialize(&genesis).unwrap();

    let funding_hash = [0x33u8; 32];
    storage
        .utxos()
        .add_utxo(
            &OutPoint {
                hash: funding_hash,
                index: 0,
            },
            &UTXO {
                value: 100_000,
                script_pubkey: vec![OP_0, OP_CHECKSIG].into(),
                height: 0,
                is_coinbase: false,
            },
        )
        .unwrap();

    let mempool = create_test_mempool();
    let tx = Transaction {
        version: 2,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: funding_hash,
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xfffffffe,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 90_000,
            script_pubkey: vec![OP_0, OP_CHECKSIG],
        }]
        .into(),
        lock_time: 0,
    };
    mempool.add_transaction(tx).expect("mempool add");

    let mut coordinator = MiningCoordinator::new(Arc::clone(&mempool), Some(storage));
    coordinator.set_protocol_engine(protocol);

    let template = coordinator
        .generate_block_template()
        .await
        .expect("template with mempool tx");

    assert_eq!(
        template.transactions.len(),
        2,
        "coinbase + one mempool transaction"
    );
}

#[tokio::test]
async fn test_mining_coordinator_generate_block_template() {
    let mempool = create_test_mempool();
    let mut coordinator = MiningCoordinator::new(mempool, None);

    let template = coordinator
        .generate_block_template()
        .await
        .expect("template without storage");

    assert_eq!(template.transactions.len(), 1);
    assert_eq!(template.header.prev_block_hash, [0u8; 32]);
}
