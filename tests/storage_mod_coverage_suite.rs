//! Storage facade helpers (index, flush, trees, recovery).

use blvm_node::storage::Storage;
use blvm_protocol::block::calculate_tx_id;
use std::sync::Arc;
use tempfile::TempDir;

mod common;
use common::{setup_mining_chain_on, valid_transaction};

#[test]
fn test_storage_new_with_network_smoke() {
    use blvm_protocol::types::Network;

    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new_with_network(temp_dir.path(), Network::Regtest).unwrap();
    assert!(!storage.is_pruning_enabled());
}

#[test]
fn test_flush_and_disk_size_after_chain_seed() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    setup_mining_chain_on(&storage, 10).unwrap();
    storage.flush().unwrap();
    assert!(storage.disk_size().unwrap() > 0);
}

#[test]
fn test_index_block_and_transaction_count() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    setup_mining_chain_on(&storage, 4).unwrap();
    let hash = storage
        .blocks()
        .get_hash_by_height(1)
        .unwrap()
        .expect("block hash");
    let block = storage.blocks().get_block(&hash).unwrap().expect("block");
    storage.index_block(&block, &hash, 1).unwrap();
    assert!(storage.transaction_count().unwrap() >= block.transactions.len());
}

#[test]
fn test_index_block_via_txindex_directly() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let tx = valid_transaction();
    let tx_hash = calculate_tx_id(&tx);
    storage
        .transactions()
        .index_transaction(&tx, &[0x99; 32], 7, 0)
        .unwrap();
    assert!(storage.transactions().has_transaction(&tx_hash).unwrap());
}

#[test]
fn test_ibd_memory_pressure_tick_smoke() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    storage.ibd_memory_pressure_tick(2);
}

#[test]
fn test_utxos_arc_shares_same_store() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let outpoint = blvm_node::OutPoint {
        hash: [0xab; 32],
        index: 0,
    };
    let utxo = blvm_node::UTXO {
        value: 1_000,
        script_pubkey: vec![0x51].into(),
        height: 1,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&outpoint, &utxo).unwrap();
    assert!(storage.utxos_arc().get_utxo(&outpoint).unwrap().is_some());
}
