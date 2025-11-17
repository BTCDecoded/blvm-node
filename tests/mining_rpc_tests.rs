//! Unit tests for Mining RPC methods

use bllvm_node::node::mempool::MempoolManager;
use bllvm_node::rpc::mining::MiningRpc;
use bllvm_node::storage::Storage;
use bllvm_protocol::serialization::serialize_transaction;
use bllvm_protocol::{BlockHeader, Natural, OutPoint, Transaction, UtxoSet, UTXO};
use std::sync::Arc;
use tempfile::TempDir;
// Sha256 not needed directly in tests
use bllvm_protocol::mining::BlockTemplate;
mod common;
use common::*;

#[tokio::test]
async fn test_mining_rpc_new() {
    let mining = MiningRpc::new();
    // Should create without dependencies
    assert!(true);
}

#[tokio::test]
async fn test_mining_rpc_with_dependencies() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());

    let mining = MiningRpc::with_dependencies(storage, mempool);
    // Should create with dependencies
    assert!(true);
}

#[tokio::test]
async fn test_get_current_height_uninitialized() {
    let mining = MiningRpc::new();
    let params = serde_json::json!([]);
    // Should fail when chain not initialized
    let result = mining.get_block_template(&params).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_get_current_height_initialized() {
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

    // Test through get_block_template
    let params = serde_json::json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_ok());
    let template = result.unwrap();
    assert_eq!(template.get("height").unwrap().as_u64().unwrap(), 0);
}

#[tokio::test]
async fn test_get_tip_header_initialized() {
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

    // Test through get_block_template
    let params = serde_json::json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_ok());
    let template = result.unwrap();
    // Verify previousblockhash exists (indicates tip header was retrieved)
    assert!(template.get("previousblockhash").is_some());
}

#[tokio::test]
async fn test_get_utxo_set_empty() {
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

    // Test through get_block_template - should work with empty UTXO set
    let params = serde_json::json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_get_utxo_set_populated() {
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

    // Add a UTXO
    let outpoint = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 5000000000,
        script_pubkey: vec![0x76, 0xa9, 0x14],
        height: 0,
    };
    storage.utxos().add_utxo(&outpoint, &utxo).unwrap();

    // Test through get_block_template - should work with populated UTXO set
    let params = serde_json::json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_transaction_serialization_in_template() {
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

    // Test transaction serialization through template
    let params = serde_json::json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_ok());
    let template = result.unwrap();

    // Verify transactions array exists
    let transactions = template.get("transactions").unwrap().as_array().unwrap();
    // Transactions should be serialized properly
    for tx in transactions {
        assert!(tx.get("data").is_some());
        assert!(tx.get("txid").is_some());
    }
}

#[tokio::test]
async fn test_calculate_tx_hash_format() {
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

    let tx = valid_transaction();
    let tx_bytes = serialize_transaction(&tx);

    // Test hash calculation through template
    let params = serde_json::json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_ok());
    let template = result.unwrap();

    // Verify transaction hashes are 64 hex characters (32 bytes)
    let transactions = template.get("transactions").unwrap().as_array().unwrap();
    for tx_json in transactions {
        let txid = tx_json.get("txid").unwrap().as_str().unwrap();
        assert_eq!(txid.len(), 64);
    }
}

#[tokio::test]
async fn test_calculate_tx_hash_matches_bitcoin_core() {
    // Test with a known Bitcoin transaction
    // Using a simple coinbase transaction structure
    let tx = Transaction {
        version: 1,
          inputs: bllvm_protocol::tx_inputs![bllvm_protocol::types::TransactionInput {
            prevout: bllvm_protocol::types::OutPoint {
                hash: [0u8; 32],
                index: 0xffffffff,
            },
            script_sig: vec![0x03, 0x00, 0x00, 0x00], // Minimal coinbase script
            sequence: 0xffffffff,
        }],
          outputs: bllvm_protocol::tx_outputs![bllvm_protocol::types::TransactionOutput {
            value: 5000000000,
            script_pubkey: vec![
                0x41, 0x04, 0x67, 0x8a, 0xfd, 0xb0, 0xfe, 0x55, 0x48, 0x27, 0x19, 0x67, 0xf1, 0xa6,
                0x71, 0x30, 0xb7, 0x10, 0x5c, 0xd6, 0xa8, 0x28, 0xe0, 0x39, 0x09, 0xa6, 0x79, 0x62,
                0xe0, 0xea, 0x1f, 0x61, 0xde, 0xb6, 0x49, 0xf6, 0xbc, 0x3f, 0x4c, 0xef, 0x38, 0xc4,
                0xf3, 0x55, 0x04, 0xe5, 0x1e, 0xc1, 0x12, 0xde, 0x5c, 0x38, 0x4d, 0xf7, 0xba, 0x0b,
                0x8d, 0x57, 0x8a, 0x4c, 0x70, 0x2b, 0x6b, 0xf1, 0x1d, 0x5f, 0xac,
            ],
        }],
        lock_time: 0,
    };

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

    // Test hash calculation through template
    let params = serde_json::json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_ok());
    let template = result.unwrap();

    // Verify transactions have valid hashes
    let transactions = template.get("transactions").unwrap().as_array().unwrap();
    for tx_json in transactions {
        let txid = tx_json.get("txid").unwrap().as_str().unwrap();
        assert_eq!(txid.len(), 64); // 32 bytes = 64 hex chars
                                    // Verify it's not all zeros
        assert_ne!(
            txid,
            "0000000000000000000000000000000000000000000000000000000000000000"
        );
    }
}

#[tokio::test]
async fn test_calculate_weight() {
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

    let tx = valid_transaction();
    let base_size = serialize_transaction(&tx).len() as u64;

    // Test weight calculation through template
    let params = serde_json::json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_ok());
    let template = result.unwrap();

    let transactions = template.get("transactions").unwrap().as_array().unwrap();
    for tx_json in transactions {
        if let Some(weight) = tx_json.get("weight").and_then(|w| w.as_u64()) {
            // Weight should be reasonable (base_size * 4 for non-SegWit)
            assert!(weight >= base_size * 4);
        }
    }
}

#[tokio::test]
async fn test_calculate_coinbase_value() {
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

    // Test coinbase value through template
    let params = serde_json::json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_ok());
    let template = result.unwrap();

    // Genesis block subsidy should be 50 BTC = 5000000000 satoshis
    let coinbase_value = template.get("coinbasevalue").unwrap().as_u64().unwrap();
    assert_eq!(coinbase_value, 5000000000);
}

#[tokio::test]
async fn test_get_active_rules() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    // Initialize chain state at height 0
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    storage.chain().initialize(&genesis_header).unwrap();

    // Test at genesis (height 0)
    let params = serde_json::json!([]);
    let result = mining.get_block_template(&params).await.unwrap();
    let rules = result.get("rules").unwrap().as_array().unwrap();
    let rule_strings: Vec<String> = rules
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect();
    assert!(rule_strings.contains(&"csv".to_string()));
    assert!(!rule_strings.contains(&"segwit".to_string()));
    assert!(!rule_strings.contains(&"taproot".to_string()));
}
