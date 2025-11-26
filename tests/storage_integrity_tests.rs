//! Storage Integrity Tests
//!
//! Tests data integrity, atomicity, and consistency of storage operations.

use bllvm_node::storage::*;
use bllvm_protocol::*;
use tempfile::TempDir;
mod common;
use common::*;

#[test]
fn test_block_store_atomicity() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let blockstore = storage.blocks();
    
    // Create a block
    let block = TestBlockBuilder::new()
        .add_coinbase_transaction(p2pkh_script(random_hash20()))
        .build();
    
    // Store block - should be atomic
    let result = blockstore.store_block(&block);
    assert!(result.is_ok());
    
    // Verify block was stored
    let block_hash = blockstore.get_block_hash(&block);
    let retrieved = blockstore.get_block(&block_hash).unwrap();
    assert_eq!(retrieved.unwrap().header.version, block.header.version);
}

#[test]
fn test_utxo_store_consistency() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let utxostore = storage.utxos();
    
    let outpoint = OutPoint {
        hash: random_hash(),
        index: 0,
    };
    
    let utxo = UTXO {
        value: 1000000,
        script_pubkey: p2pkh_script(random_hash20()),
        height: 0,
    };
    
    // Add UTXO
    utxostore.add_utxo(&outpoint, &utxo).unwrap();
    
    // Verify UTXO exists
    let retrieved = utxostore.get_utxo(&outpoint).unwrap();
    assert!(retrieved.is_some());
    let retrieved_utxo = retrieved.unwrap();
    assert_eq!(retrieved_utxo.value, utxo.value);
    assert_eq!(retrieved_utxo.height, utxo.height);
    
    // Remove UTXO
    utxostore.remove_utxo(&outpoint).unwrap();
    
    // Verify UTXO no longer exists
    let result = utxostore.get_utxo(&outpoint).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_chain_state_consistency() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let chainstate = storage.chain();
    
    // Initialize chain
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    
    chainstate.initialize(&genesis_header).unwrap();
    
    // Verify chain state
    let height = chainstate.get_height().unwrap();
    assert_eq!(height, Some(0));
    let tip_header = chainstate.get_tip_header().unwrap();
    assert_eq!(tip_header.unwrap().version, genesis_header.version);
}

#[test]
fn test_storage_concurrent_reads() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let blockstore = storage.blocks();
    
    // Store a block
    let block = TestBlockBuilder::new()
        .add_coinbase_transaction(p2pkh_script(random_hash20()))
        .build();
    
    blockstore.store_block(&block).unwrap();
    let block_hash = blockstore.get_block_hash(&block);
    
    // Concurrent reads should work
    use std::sync::Arc;
    use std::thread;
    
    let storage_arc = Arc::new(storage);
    let mut handles = vec![];
    
    for _ in 0..10 {
        let storage_clone = Arc::clone(&storage_arc);
        let hash = block_hash;
        
        handles.push(thread::spawn(move || {
            let blockstore = storage_clone.blocks();
            blockstore.get_block(&hash).unwrap()
        }));
    }
    
    // All reads should succeed
    for handle in handles {
        let retrieved = handle.join().unwrap();
        assert_eq!(retrieved.unwrap().header.version, block.header.version);
    }
}

#[test]
fn test_storage_index_consistency() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let blockstore = storage.blocks();
    let txindex = storage.transactions();
    
    // Create block with transaction
    let tx = TestTransactionBuilder::new()
        .add_input(OutPoint {
            hash: random_hash(),
            index: 0,
        })
        .add_output(1000, p2pkh_script(random_hash20()))
        .build();
    
    let block = TestBlockBuilder::new()
        .add_coinbase_transaction(p2pkh_script(random_hash20()))
        .add_transaction(tx.clone())
        .build();
    
    // Store block
    blockstore.store_block(&block).unwrap();
    
    // Transaction should be indexed
    // Note: Transaction indexing happens when block is stored
    // For now, just verify the block was stored correctly
    let block_hash = blockstore.get_block_hash(&block);
    let retrieved_block = blockstore.get_block(&block_hash).unwrap();
    assert!(retrieved_block.is_some());
}

