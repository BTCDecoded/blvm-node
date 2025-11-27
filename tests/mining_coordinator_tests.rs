//! Tests for mining coordinator

use bllvm_node::node::mempool::MempoolManager;
use bllvm_node::node::miner::{MiningCoordinator, MiningEngine, TransactionSelector};
use bllvm_protocol::tx_inputs;
use bllvm_protocol::tx_outputs;
use bllvm_protocol::{Block, BlockHeader, Transaction, UtxoSet};
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_transaction() -> Transaction {
    use bllvm_protocol::TransactionOutput;
    Transaction {
        version: 1,
        inputs: tx_inputs![],
        outputs: bllvm_protocol::tx_outputs![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x76, 0xa9, 0x14, 0x88, 0xac],
        }],
        lock_time: 0,
    }
}

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

// Note: TransactionSelector.select_transactions() requires a real MempoolManager
// These tests are covered in integration tests or through MiningCoordinator tests

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
async fn test_mining_coordinator_generate_block_template() {
    let mempool = create_test_mempool();
    let mut coordinator = MiningCoordinator::new(mempool, None);

    // Should generate a template even without storage
    let result = coordinator.generate_block_template().await;
    assert!(result.is_ok());

    let template = result.unwrap();
    assert_eq!(template.transactions.len(), 1); // Coinbase only
    assert_eq!(template.header.prev_block_hash, [0u8; 32]); // Default genesis hash
}

#[tokio::test]
async fn test_mining_coordinator_generate_block_template_with_storage() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(bllvm_node::storage::Storage::new(temp_dir.path()).unwrap());
    let mempool = create_test_mempool();
    let mut coordinator = MiningCoordinator::new(mempool, Some(storage));

    let result = coordinator.generate_block_template().await;
    assert!(result.is_ok());

    let template = result.unwrap();
    // Should have at least coinbase
    assert!(template.transactions.len() >= 1);
}
