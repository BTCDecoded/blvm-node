//! Integration tests for mining functionality

use bllvm_node::rpc::mining::MiningRpc;
use bllvm_node::storage::Storage;
use bllvm_node::node::mempool::MempoolManager;
use std::sync::Arc;
use tempfile::TempDir;
use bllvm_protocol::types::{BlockHeader, Transaction, UtxoSet, OutPoint, UTXO};
use serde_json::json;
mod common;
use common::*;

#[tokio::test]
async fn test_getblocktemplate_full_flow() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain state with genesis
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    // Call getblocktemplate
    let params = json!([]);
    let result = mining.get_block_template(&params).await;
    
    assert!(result.is_ok());
    let template = result.unwrap();
    
    // Verify required fields
    assert!(template.get("version").is_some());
    assert!(template.get("height").is_some());
    assert!(template.get("previousblockhash").is_some());
    assert!(template.get("coinbasevalue").is_some());
    assert!(template.get("target").is_some());
    assert!(template.get("bits").is_some());
    assert!(template.get("curtime").is_some());
    assert!(template.get("rules").is_some());
    assert!(template.get("transactions").is_some());
    
    // Verify height is 0 (genesis)
    assert_eq!(template.get("height").unwrap().as_u64().unwrap(), 0);
    
    // Verify coinbase value is genesis subsidy
    assert_eq!(template.get("coinbasevalue").unwrap().as_u64().unwrap(), 5000000000);
}

#[tokio::test]
async fn test_getblocktemplate_with_utxo_set() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain state
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    // Add UTXOs to the set
    let outpoint1 = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let utxo1 = UTXO {
        value: 1000000000, // 10 BTC
        script_pubkey: p2pkh_script(random_hash20()),
        height: 0,
    };
    storage.utxos().add_utxo(&outpoint1, &utxo1).unwrap();
    
    let outpoint2 = OutPoint {
        hash: [2u8; 32],
        index: 0,
    };
    let utxo2 = UTXO {
        value: 500000000, // 5 BTC
        script_pubkey: p2pkh_script(random_hash20()),
        height: 0,
    };
    storage.utxos().add_utxo(&outpoint2, &utxo2).unwrap();
    
    // Get template
    let params = json!([]);
    let result = mining.get_block_template(&params).await;
    
    assert!(result.is_ok());
    let template = result.unwrap();
    
    // Verify template was created
    assert!(template.get("height").is_some());
    assert_eq!(template.get("height").unwrap().as_u64().unwrap(), 0);
}

#[tokio::test]
async fn test_getblocktemplate_json_rpc_format() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain state
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    let params = json!([]);
    let result = mining.get_block_template(&params).await.unwrap();
    
    // Verify BIP 22/23 required fields
    assert!(result.get("capabilities").is_some());
    assert!(result.get("version").is_some());
    assert!(result.get("rules").is_some());
    assert!(result.get("vbavailable").is_some());
    assert!(result.get("vbrequired").is_some());
    assert!(result.get("previousblockhash").is_some());
    assert!(result.get("transactions").is_some());
    assert!(result.get("coinbaseaux").is_some());
    assert!(result.get("coinbasevalue").is_some());
    assert!(result.get("longpollid").is_some());
    assert!(result.get("target").is_some());
    assert!(result.get("mintime").is_some());
    assert!(result.get("mutable").is_some());
    assert!(result.get("noncerange").is_some());
    assert!(result.get("sigoplimit").is_some());
    assert!(result.get("sizelimit").is_some());
    assert!(result.get("weightlimit").is_some());
    assert!(result.get("curtime").is_some());
    assert!(result.get("bits").is_some());
    assert!(result.get("height").is_some());
}

#[tokio::test]
async fn test_template_creation_with_real_data() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain state at height 100
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    // Simulate chain at height 100
    let tip_header = BlockHeader {
        version: 1,
        prev_block_hash: random_hash(),
        merkle_root: random_hash(),
        timestamp: 1231006505 + 100 * 600, // ~100 blocks later
        bits: 0x1d00ffff,
        nonce: 0,
    };
    storage.chain().update_tip(&random_hash(), &tip_header, 100).unwrap();
    
    // Add multiple UTXOs
    for i in 0..10 {
        let outpoint = OutPoint {
            hash: random_hash(),
            index: i,
        };
        let utxo = UTXO {
            value: 100000000 * (i + 1), // Varying amounts
            script_pubkey: p2pkh_script(random_hash20()),
            height: 100,
        };
        storage.utxos().add_utxo(&outpoint, &utxo).unwrap();
    }
    
    // Get template
    let params = json!([]);
    let result = mining.get_block_template(&params).await;
    
    assert!(result.is_ok());
    let template = result.unwrap();
    
    // Verify height is correct
    assert_eq!(template.get("height").unwrap().as_u64().unwrap(), 100);
    
    // Verify previous block hash matches tip
    let prev_hash = template.get("previousblockhash").unwrap().as_str().unwrap();
    let tip_hash_hex = hex::encode(tip_header.prev_block_hash);
    assert_eq!(prev_hash, tip_hash_hex);
}

#[tokio::test]
async fn test_template_transaction_serialization() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain state
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    let params = json!([]);
    let result = mining.get_block_template(&params).await.unwrap();
    
    // Verify transactions array exists
    let transactions = result.get("transactions").unwrap().as_array().unwrap();
    
    // Transactions should be an array (may be empty)
    assert!(transactions.is_array());
    
    // If there are transactions, verify they have required fields
    for tx in transactions {
        assert!(tx.get("data").is_some());
        assert!(tx.get("txid").is_some());
        assert!(tx.get("fee").is_some());
        assert!(tx.get("sigops").is_some());
        assert!(tx.get("weight").is_some());
    }
}

#[tokio::test]
async fn test_template_target_and_bits() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain state
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    let params = json!([]);
    let result = mining.get_block_template(&params).await.unwrap();
    
    // Verify target is 64 hex characters (32 bytes)
    let target = result.get("target").unwrap().as_str().unwrap();
    assert_eq!(target.len(), 64);
    
    // Verify bits is 8 hex characters (4 bytes)
    let bits = result.get("bits").unwrap().as_str().unwrap();
    assert_eq!(bits.len(), 8);
    
    // Verify bits matches genesis difficulty
    assert_eq!(bits, "1d00ffff");
}

#[tokio::test]
async fn test_template_rules_activation() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain state at different heights
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();
    
    // Test at height 0 (genesis)
    let mut tip_header = genesis_header.clone();
    storage.chain().update_tip(&random_hash(), &tip_header, 0).unwrap();
    let params = json!([]);
    let result = mining.get_block_template(&params).await.unwrap();
    let rules = result.get("rules").unwrap().as_array().unwrap();
    assert!(rules.len() >= 1); // At least CSV
    
    // Test at SegWit activation (height 481824)
    tip_header.timestamp = 1231006505 + 481824 * 600;
    storage.chain().update_tip(&random_hash(), &tip_header, 481824).unwrap();
    let result = mining.get_block_template(&params).await.unwrap();
    let rules = result.get("rules").unwrap().as_array().unwrap();
    let rule_strings: Vec<String> = rules.iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect();
    assert!(rule_strings.contains(&"segwit".to_string()));
}

#[tokio::test]
async fn test_getblocktemplate_error_handling() {
    let mining = MiningRpc::new(); // No dependencies
    
    let params = json!([]);
    let result = mining.get_block_template(&params).await;
    
    // Should return error when chain is not initialized
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(error.to_string().contains("Chain not initialized") || 
            error.to_string().contains("No chain tip"));
}

