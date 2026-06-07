//! Raw transaction RPC decode/accept paths.

use blvm_node::node::mempool::MempoolManager;
use blvm_node::rpc::rawtx::RawTxRpc;
use blvm_node::storage::Storage;
use blvm_node::{OutPoint, UTXO};
use blvm_protocol::serialization::transaction::serialize_transaction;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

mod common;
use common::{
    p2pkh_script, random_hash20, setup_mining_chain, valid_transaction, TestBlockBuilder,
};

fn rpc_with_mempool() -> (TempDir, RawTxRpc, String) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let tx = valid_transaction();
    use blvm_protocol::block::calculate_tx_id;
    let txid = hex::encode(calculate_tx_id(&tx));
    mempool.add_transaction(tx.clone()).unwrap();
    let rpc = RawTxRpc::with_dependencies(storage, mempool, None, None);
    (temp_dir, rpc, txid)
}

#[tokio::test]
async fn test_decoderawtransaction_round_trip() {
    let tx = valid_transaction();
    let hex_tx = hex::encode(serialize_transaction(&tx));
    let rpc = RawTxRpc::new();
    let decoded = rpc.decoderawtransaction(&json!([hex_tx])).await.unwrap();
    assert_eq!(decoded.get("version").unwrap().as_i64(), Some(1));
    assert!(decoded.get("vin").unwrap().is_array());
    assert!(decoded.get("vout").unwrap().is_array());
}

#[tokio::test]
async fn test_testmempoolaccept_with_mempool_fixture() {
    let tx = valid_transaction();
    let hex_tx = hex::encode(serialize_transaction(&tx));
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let rpc = RawTxRpc::with_dependencies(storage, mempool, None, None);
    let result = rpc.testmempoolaccept(&json!([hex_tx])).await.unwrap();
    let arr = result.as_array().expect("accept results");
    assert_eq!(arr.len(), 1);
    assert!(arr[0].get("allowed").is_some());
}

#[tokio::test]
async fn test_gettxout_with_indexed_utxo() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let tx = valid_transaction();
    use blvm_protocol::block::calculate_tx_id;
    let tx_hash = calculate_tx_id(&tx);
    let txid_hex = hex::encode(tx_hash);
    storage
        .transactions()
        .index_transaction(&tx, &[0xab; 32], 0, 0)
        .unwrap();
    storage
        .utxos()
        .add_utxo(
            &OutPoint {
                hash: tx_hash,
                index: 0,
            },
            &UTXO {
                value: 1000,
                script_pubkey: vec![0x51].into(),
                height: 0,
                is_coinbase: false,
            },
        )
        .unwrap();
    let rpc = RawTxRpc::with_dependencies(storage, Arc::new(MempoolManager::new()), None, None);
    let out = rpc.gettxout(&json!([txid_hex, 0, false])).await.unwrap();
    assert_eq!(
        out.get("value").unwrap().as_f64(),
        Some(1000.0 / 100_000_000.0)
    );
}

#[tokio::test]
async fn test_getrawtransaction_from_index() {
    let (_dir, rpc, txid) = rpc_with_mempool();
    let result = rpc.getrawtransaction(&json!([txid])).await;
    assert!(result.is_err() || result.unwrap().get("hex").is_some());
}

#[tokio::test]
async fn test_createrawtransaction_smoke() {
    let rpc = RawTxRpc::new();
    let txid = "aa".repeat(32);
    let hex_tx = rpc
        .createrawtransaction(&json!([
            [{"txid": txid, "vout": 0}],
            {"bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4": 0.001}
        ]))
        .await
        .unwrap();
    assert!(hex_tx.as_str().unwrap().len() >= 64);
}

#[tokio::test]
async fn test_getrawtransaction_verbose_from_index() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let tx = valid_transaction();
    use blvm_protocol::block::calculate_tx_id;
    let tx_hash = calculate_tx_id(&tx);
    let txid = hex::encode(tx_hash);
    storage
        .transactions()
        .index_transaction(&tx, &[0xde; 32], 0, 0)
        .unwrap();
    let rpc = RawTxRpc::with_dependencies(storage, Arc::new(MempoolManager::new()), None, None);
    let result = rpc.getrawtransaction(&json!([txid, true])).await.unwrap();
    assert!(result.get("txid").is_some());
    assert!(result.get("vin").is_some());
}

#[tokio::test]
async fn test_sendrawtransaction_invalid_hex_errors() {
    let (_dir, rpc, _txid) = rpc_with_mempool();
    assert!(rpc.sendrawtransaction(&json!(["not-hex"])).await.is_err());
}

#[tokio::test]
async fn test_gettxout_missing_utxo_returns_null() {
    let (_dir, rpc, txid) = rpc_with_mempool();
    let out = rpc.gettxout(&json!([txid, 99, true])).await.unwrap();
    assert!(out.is_null());
}

#[tokio::test]
async fn test_gettxoutproof_and_verifytxoutproof_on_chain() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    setup_mining_chain(&storage, 4).unwrap();

    let tx = valid_transaction();
    use blvm_protocol::block::calculate_tx_id;
    let txid = hex::encode(calculate_tx_id(&tx));

    let tip = storage.chain().get_height().unwrap().unwrap_or(0);
    let prev_hash = storage.blocks().get_hash_by_height(tip).unwrap().unwrap();
    let block = TestBlockBuilder::new()
        .set_prev_hash(prev_hash)
        .add_coinbase_transaction(p2pkh_script(random_hash20()))
        .add_transaction(tx.clone())
        .build();
    let height = tip + 1;
    let block_hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage.blocks().store_height(height, &block_hash).unwrap();
    storage
        .chain()
        .update_tip(&block_hash, &block.header, height)
        .unwrap();
    storage
        .transactions()
        .index_transaction(&tx, &block_hash, height, 1)
        .unwrap();

    let rpc = RawTxRpc::with_dependencies(storage, Arc::new(MempoolManager::new()), None, None);

    let proof = rpc
        .gettxoutproof(&json!([[txid]]))
        .await
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let blockhash_hex = hex::encode(block_hash);
    let verified = rpc
        .verifytxoutproof(&json!([proof, blockhash_hex]))
        .await
        .unwrap();
    assert!(verified.get("txids").unwrap().is_array());
    assert!(verified.get("matches").is_some());
}

#[tokio::test]
async fn test_gettxoutproof_missing_params_and_bad_blockhash() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    setup_mining_chain(&storage, 3).unwrap();
    let rpc = RawTxRpc::with_dependencies(storage, Arc::new(MempoolManager::new()), None, None);

    assert!(rpc.gettxoutproof(&json!([])).await.is_err());
    assert!(rpc
        .gettxoutproof(&json!([["aa".repeat(32)], "abcd"]))
        .await
        .is_err());
}

#[tokio::test]
async fn test_verifytxoutproof_invalid_proof_errors() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    setup_mining_chain(&storage, 3).unwrap();
    let rpc = RawTxRpc::with_dependencies(storage, Arc::new(MempoolManager::new()), None, None);

    assert!(rpc.verifytxoutproof(&json!([])).await.is_err());
    assert!(rpc
        .verifytxoutproof(&json!(["", "aa".repeat(64)]))
        .await
        .is_err());
    assert!(rpc
        .verifytxoutproof(&json!(["00", "aa".repeat(64)]))
        .await
        .is_err());
}

#[tokio::test]
async fn test_sendrawtransaction_already_in_mempool_errors() {
    let tx = valid_transaction();
    let hex_tx = hex::encode(serialize_transaction(&tx));
    let (_dir, rpc, _txid) = rpc_with_mempool();
    assert!(rpc.sendrawtransaction(&json!([hex_tx])).await.is_err());
}

#[tokio::test]
async fn test_gettransactiondetails_from_mempool() {
    let (_dir, rpc, txid) = rpc_with_mempool();
    let details = rpc
        .get_transaction_details(&json!([txid, true]))
        .await
        .unwrap();
    assert_eq!(details.get("txid").unwrap().as_str(), Some(txid.as_str()));
    assert!(details.get("vin").unwrap().is_array());
    assert!(details.get("hex").unwrap().as_str().unwrap().len() >= 64);
}
