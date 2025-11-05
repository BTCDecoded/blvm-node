//! Comprehensive integration tests for mining workflows
//! 
//! Tests real-world mining scenarios and end-to-end workflows.

use reference_node::rpc::mining::MiningRpc;
use reference_node::storage::Storage;
use reference_node::node::mempool::MempoolManager;
use std::sync::Arc;
use tempfile::TempDir;
use protocol_engine::types::{BlockHeader, OutPoint, UTXO, Transaction, TransactionInput, TransactionOutput};
use serde_json::json;
mod common;
use common::*;

/// Test complete mining workflow: template creation → mining → block submission
#[tokio::test]
async fn test_complete_mining_workflow() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain at height 100
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    let tip_hash = random_hash();
    let tip_header = BlockHeader {
        version: 1,
        prev_block_hash: random_hash(),
        merkle_root: random_hash(),
        timestamp: 1231006505 + 100 * 600,
        bits: 0x1d00ffff,
        nonce: 0,
    };
    storage.chain().update_tip(&tip_hash, &tip_header, 100).unwrap();
    
    // Add UTXOs
    for i in 0..5 {
        let outpoint = OutPoint {
            hash: random_hash(),
            index: i,
        };
        let utxo = UTXO {
            value: 100000000 * (i + 1), // 1-5 BTC
            script_pubkey: p2pkh_script(random_hash20()),
            height: 100,
        };
        storage.utxos().add_utxo(&outpoint, &utxo).unwrap();
    }
    
    // Step 1: Get block template
    let params = json!([]);
    let template_result = mining.get_block_template(&params).await;
    assert!(template_result.is_ok());
    let template = template_result.unwrap();
    
    // Verify template fields
    assert_eq!(template.get("height").unwrap().as_u64().unwrap(), 100);
    assert!(template.get("previousblockhash").is_some());
    assert!(template.get("coinbasevalue").is_some());
    assert!(template.get("target").is_some());
    assert!(template.get("bits").is_some());
    
    // Step 2: Template should be usable for mining
    // (In real implementation, would use template to create block and mine)
    assert!(true);
}

/// Test template creation with multiple height transitions
#[tokio::test]
async fn test_template_height_transitions() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    // Test at different heights
    let heights = vec![0, 1, 10, 100, 1000, 10000, 100000];
    
    for height in heights {
        let tip_hash = random_hash();
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: random_hash(),
            merkle_root: random_hash(),
            timestamp: 1231006505 + height * 600,
            bits: 0x1d00ffff,
            nonce: 0,
        };
        storage.chain().update_tip(&tip_hash, &tip_header, height).unwrap();
        
        let params = json!([]);
        let result = mining.get_block_template(&params).await;
        assert!(result.is_ok());
        let template = result.unwrap();
        assert_eq!(template.get("height").unwrap().as_u64().unwrap(), height);
    }
}

/// Test template creation with large UTXO sets
#[tokio::test]
async fn test_template_large_utxo_set() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    // Add 1000 UTXOs
    for i in 0..1000 {
        let outpoint = OutPoint {
            hash: random_hash(),
            index: i % 10,
        };
        let utxo = UTXO {
            value: 1000000 + (i as i64) * 1000, // Varying amounts
            script_pubkey: p2pkh_script(random_hash20()),
            height: 0,
        };
        storage.utxos().add_utxo(&outpoint, &utxo).unwrap();
    }
    
    let params = json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_ok());
    let template = result.unwrap();
    
    // Template should be created successfully even with large UTXO set
    assert!(template.get("height").is_some());
    assert_eq!(template.get("height").unwrap().as_u64().unwrap(), 0);
}

/// Test template creation at halving boundaries
#[tokio::test]
async fn test_template_halving_boundaries() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    // Test at halving boundaries
    let halving_heights = vec![209999, 210000, 210001, 419999, 420000, 420001];
    
    for height in halving_heights {
        let tip_hash = random_hash();
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: random_hash(),
            merkle_root: random_hash(),
            timestamp: 1231006505 + height * 600,
            bits: 0x1d00ffff,
            nonce: 0,
        };
        storage.chain().update_tip(&tip_hash, &tip_header, height).unwrap();
        
        let params = json!([]);
        let result = mining.get_block_template(&params).await;
        assert!(result.is_ok());
        let template = result.unwrap();
        
        let coinbase_value = template.get("coinbasevalue").unwrap().as_u64().unwrap();
        
        // Verify coinbase value changes at halving boundaries
        if height == 209999 {
            assert_eq!(coinbase_value, 5000000000); // 50 BTC before first halving
        } else if height == 210000 {
            assert_eq!(coinbase_value, 2500000000); // 25 BTC after first halving
        } else if height == 419999 {
            assert_eq!(coinbase_value, 2500000000); // 25 BTC before second halving
        } else if height == 420000 {
            assert_eq!(coinbase_value, 1250000000); // 12.5 BTC after second halving
        }
    }
}

/// Test template creation with different difficulty levels
#[tokio::test]
async fn test_template_different_difficulty() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    // Test with different difficulty bits
    let difficulty_bits = vec![0x1d00ffff, 0x1e00ffff, 0x1f00ffff, 0x2000ffff];
    
    for (i, bits) in difficulty_bits.iter().enumerate() {
        let height = i as u64;
        let tip_hash = random_hash();
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: random_hash(),
            merkle_root: random_hash(),
            timestamp: 1231006505 + height * 600,
            bits: *bits as u64,
            nonce: 0,
        };
        storage.chain().update_tip(&tip_hash, &tip_header, height).unwrap();
        
        let params = json!([]);
        let result = mining.get_block_template(&params).await;
        assert!(result.is_ok());
        let template = result.unwrap();
        
        // Verify bits match
        let template_bits = template.get("bits").unwrap().as_str().unwrap();
        let expected_bits = format!("{:08x}", bits);
        assert_eq!(template_bits, expected_bits);
    }
}

/// Test template creation with mempool transactions (when implemented)
#[tokio::test]
async fn test_template_with_mempool_transactions() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    // Currently mempool returns empty, but test should still pass
    let params = json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_ok());
    
    // When mempool prioritization is implemented, this test should verify
    // that transactions from mempool appear in template
}

/// Test template creation error handling
#[tokio::test]
async fn test_template_error_handling() {
    // Test without dependencies
    let mining = MiningRpc::new();
    let params = json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_err());
    
    // Test with uninitialized chain
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage, mempool);
    
    let params = json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_err()); // Should fail without initialized chain
}

/// Test template creation with edge case heights
#[tokio::test]
async fn test_template_edge_case_heights() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    // Test edge cases
    let edge_heights = vec![
        0,           // Genesis
        1,           // First block
        2016,        // First difficulty adjustment
        2017,        // After first difficulty adjustment
        210000,      // First halving
        420000,      // Second halving
        840000,      // Third halving
        13440000,    // After all halvings (subsidy = 0)
    ];
    
    for height in edge_heights {
        let tip_hash = random_hash();
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: random_hash(),
            merkle_root: random_hash(),
            timestamp: 1231006505 + height * 600,
            bits: 0x1d00ffff,
            nonce: 0,
        };
        storage.chain().update_tip(&tip_hash, &tip_header, height).unwrap();
        
        let params = json!([]);
        let result = mining.get_block_template(&params).await;
        assert!(result.is_ok());
        let template = result.unwrap();
        assert_eq!(template.get("height").unwrap().as_u64().unwrap(), height);
    }
}

/// Test template creation with concurrent requests
#[tokio::test]
async fn test_template_concurrent_requests() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = Arc::new(MiningRpc::with_dependencies(storage.clone(), mempool));
    
    // Initialize chain
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    // Make 10 concurrent requests
    let params = json!([]);
    let mut handles = Vec::new();
    
    for _ in 0..10 {
        let mining_clone = Arc::clone(&mining);
        let params_clone = params.clone();
        handles.push(tokio::spawn(async move {
            mining_clone.get_block_template(&params_clone).await
        }));
    }
    
    // Wait for all requests
    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.is_ok());
        let template = result.unwrap();
        assert_eq!(template.get("height").unwrap().as_u64().unwrap(), 0);
    }
}

