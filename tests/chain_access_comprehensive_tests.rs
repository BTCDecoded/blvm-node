//! Chain Access Comprehensive Tests
//!
//! Tests for chain state access patterns and NodeChainAccess implementation.

use blvm_node::network::chain_access::NodeChainAccess;
use blvm_node::node::mempool::MempoolManager;
use blvm_node::storage::Storage;
use blvm_protocol::Hash;
use blvm_protocol::network::ChainStateAccess;
use std::sync::Arc;

fn create_test_storage() -> Arc<Storage> {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    Arc::new(Storage::new(data_dir).unwrap())
}

#[test]
fn test_chain_access_creation() {
    // Create chain access with all components
    let storage = create_test_storage();

    let blockstore = storage.blocks();
    let txindex = storage.transactions();
    let mempool = Arc::new(MempoolManager::new());

    let _chain_access = NodeChainAccess::new(blockstore, txindex, mempool);

    // Should create successfully
}

#[test]
fn test_chain_access_has_object() {
    let storage = create_test_storage();

    let blockstore = storage.blocks();
    let txindex = storage.transactions();
    let mempool = Arc::new(MempoolManager::new());

    let chain_access = NodeChainAccess::new(blockstore, txindex, mempool);

    // Test with non-existent hash (trait method)
    let test_hash: Hash = [0u8; 32];
    assert!(!ChainStateAccess::has_object(&chain_access, &test_hash));
}

#[test]
fn test_chain_access_get_object() {
    let storage = create_test_storage();

    let blockstore = storage.blocks();
    let txindex = storage.transactions();
    let mempool = Arc::new(MempoolManager::new());

    let chain_access = NodeChainAccess::new(blockstore, txindex, mempool);

    // Test with non-existent hash (trait method)
    let test_hash: Hash = [0u8; 32];
    assert!(ChainStateAccess::get_object(&chain_access, &test_hash).is_none());
}

#[test]
fn test_chain_access_get_headers_for_locator() {
    let storage = create_test_storage();

    let blockstore = storage.blocks();
    let txindex = storage.transactions();
    let mempool = Arc::new(MempoolManager::new());

    let chain_access = NodeChainAccess::new(blockstore, txindex, mempool);

    // Test with empty locator (trait method)
    let locator: Vec<Hash> = vec![];
    let stop: Hash = [0u8; 32];
    let headers = ChainStateAccess::get_headers_for_locator(&chain_access, &locator, &stop);

    assert!(headers.is_empty());
}

#[test]
fn test_chain_access_get_mempool_transactions() {
    let storage = create_test_storage();

    let blockstore = storage.blocks();
    let txindex = storage.transactions();
    let mempool = Arc::new(MempoolManager::new());

    let chain_access = NodeChainAccess::new(blockstore, txindex, mempool);

    // Get mempool transactions (should be empty initially, trait method)
    let txs = ChainStateAccess::get_mempool_transactions(&chain_access);
    assert!(txs.is_empty());
}

#[test]
fn test_chain_access_block_locator_algorithm() {
    let storage = create_test_storage();

    let blockstore = storage.blocks();
    let txindex = storage.transactions();
    let mempool = Arc::new(MempoolManager::new());

    let chain_access = NodeChainAccess::new(blockstore, txindex, mempool);

    // Test block locator with stop hash (trait method)
    let locator = vec![[1u8; 32], [2u8; 32], [3u8; 32]];
    let stop: Hash = [2u8; 32];

    let headers = ChainStateAccess::get_headers_for_locator(&chain_access, &locator, &stop);
    // Should stop at stop hash (not include it)
    assert!(headers.len() <= 1);
}

#[test]
fn test_chain_access_get_block_witnesses() {
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::{Block, BlockHeader, Transaction, TransactionInput, TransactionOutput};

    let storage = create_test_storage();
    let blockstore = storage.blocks();
    let txindex = storage.transactions();
    let mempool = Arc::new(MempoolManager::new());
    let chain_access = NodeChainAccess::new(blockstore.clone(), txindex, mempool);

    let block = Block {
        header: BlockHeader {
            version: 4,
            prev_block_hash: [0; 32],
            merkle_root: [1; 32],
            timestamp: 1_231_006_505,
            bits: 0x0300ffff,
            nonce: 0,
        },
        transactions: vec![Transaction {
            version: 2,
            inputs: vec![TransactionInput {
                prevout: blvm_protocol::OutPoint {
                    hash: [0; 32],
                    index: 0xffffffff,
                },
                script_sig: vec![0x01, 0x00],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 5_000_000_000,
                script_pubkey: vec![0x51],
            }]
            .into(),
            lock_time: 0,
        }]
        .into(),
    };
    let block_hash = blockstore.get_block_hash(&block);
    blockstore.store_block(&block).unwrap();
    let witnesses: Vec<Vec<Witness>> = vec![vec![vec![vec![0xde, 0xad]]]];
    blockstore.store_witness(&block_hash, &witnesses).unwrap();

    let loaded = ChainStateAccess::get_block_witnesses(&chain_access, &block_hash);
    assert_eq!(loaded.as_ref(), Some(&witnesses));
}
