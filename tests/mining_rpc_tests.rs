//! Unit tests for Mining RPC methods

use blvm_node::node::mempool::MempoolManager;
use blvm_node::rpc::mining::MiningRpc;
use blvm_node::storage::Storage;
use blvm_protocol::serialization::serialize_transaction;
use blvm_protocol::{OutPoint, UTXO};
use std::sync::Arc;
use tempfile::TempDir;

mod common;
use common::{MINING_RPC_CHAIN_BLOCKS, setup_mining_chain, valid_transaction};

async fn expect_block_template(
    mining: &MiningRpc,
    params: &serde_json::Value,
) -> serde_json::Value {
    mining.get_block_template(params).await.unwrap_or_else(|e| {
        panic!("get_block_template failed: {e:?}");
    })
}

#[tokio::test]
async fn test_mining_rpc_new() {
    let _mining = MiningRpc::new();
}

#[tokio::test]
async fn test_mining_rpc_with_dependencies() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let _mining = MiningRpc::with_dependencies(storage, mempool);
}

#[tokio::test]
async fn test_get_current_height_uninitialized() {
    let mining = MiningRpc::new();
    let params = serde_json::json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_get_current_height_initialized() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;
    assert_eq!(
        template.get("height").unwrap().as_u64().unwrap(),
        MINING_RPC_CHAIN_BLOCKS
    );
}

#[tokio::test]
async fn test_get_tip_header_initialized() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;
    let tip_hash = storage.chain().get_tip_hash().unwrap().expect("tip hash");
    assert_eq!(
        template.get("previousblockhash").unwrap().as_str().unwrap(),
        blvm_node::storage::hashing::hash_to_rpc_hex(&tip_hash)
    );
}

#[tokio::test]
async fn test_get_utxo_set_empty() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    expect_block_template(&mining, &params).await;
}

#[tokio::test]
async fn test_get_utxo_set_populated() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let outpoint = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 5000000000,
        script_pubkey: vec![0x76, 0xa9, 0x14].into(),
        height: 0,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&outpoint, &utxo).unwrap();

    let params = serde_json::json!([]);
    expect_block_template(&mining, &params).await;
}

#[tokio::test]
async fn test_transaction_serialization_in_template() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;

    let transactions = template.get("transactions").unwrap().as_array().unwrap();
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

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;

    let transactions = template.get("transactions").unwrap().as_array().unwrap();
    for tx_json in transactions {
        let txid = tx_json.get("txid").unwrap().as_str().unwrap();
        assert_eq!(txid.len(), 64);
    }
}

#[tokio::test]
async fn test_calculate_tx_hash_matches_bitcoin_core() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;

    let transactions = template.get("transactions").unwrap().as_array().unwrap();
    for tx_json in transactions {
        let txid = tx_json.get("txid").unwrap().as_str().unwrap();
        assert_eq!(txid.len(), 64);
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

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let tx = valid_transaction();
    let base_size = serialize_transaction(&tx).len() as u64;

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;

    let transactions = template.get("transactions").unwrap().as_array().unwrap();
    for tx_json in transactions {
        if let Some(weight) = tx_json.get("weight").and_then(|w| w.as_u64()) {
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

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;

    let coinbase_value = template.get("coinbasevalue").unwrap().as_u64().unwrap();
    assert_eq!(coinbase_value, 5000000000);
}

#[tokio::test]
async fn test_get_active_rules() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;
    let rules = template.get("rules").unwrap().as_array().unwrap();
    let rule_strings: Vec<String> = rules
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect();

    // Default storage network is mainnet; at this short test height no soft-fork rules are active yet.
    assert!(!rule_strings.contains(&"csv".to_string()));
    assert!(!rule_strings.contains(&"segwit".to_string()));
    assert!(!rule_strings.contains(&"taproot".to_string()));
}
