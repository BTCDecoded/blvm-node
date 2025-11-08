//! Tests for Storage Arc refactoring

use bllvm_node::storage::Storage;
use bllvm_node::network::chain_access::NodeChainAccess;
use bllvm_node::node::mempool::MempoolManager;
use std::sync::Arc;
use tempfile::TempDir;

#[test]
fn test_storage_returns_arcs() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    
    // Verify blocks() returns Arc
    let blocks_arc = storage.blocks();
    let blocks_arc2 = storage.blocks();
    
    // Both should be valid Arcs (can clone)
    assert!(Arc::ptr_eq(&blocks_arc, &blocks_arc2) || blocks_arc.block_count().is_ok());
    
    // Verify transactions() returns Arc
    let tx_arc = storage.transactions();
    let tx_arc2 = storage.transactions();
    
    assert!(Arc::ptr_eq(&tx_arc, &tx_arc2) || tx_arc.has_transaction(&[0u8; 32]).is_ok());
}

#[test]
fn test_node_chain_access_creation() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let mempool = Arc::new(MempoolManager::new());
    
    // Should be able to create NodeChainAccess with Arcs from Storage
    let chain_access = NodeChainAccess::new(
        storage.blocks(),
        storage.transactions(),
        mempool,
    );
    
    // Verify it works
    let hash = [0u8; 32];
    let has_object = chain_access.has_object(&hash);
    assert!(!has_object); // Empty storage, so should be false
}

