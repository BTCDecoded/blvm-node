//! Blockchain RPC methods with seeded chain storage.

use blvm_node::rpc::blockchain::BlockchainRpc;
use blvm_node::storage::Storage;
use blvm_protocol::BitcoinProtocolEngine;
use blvm_protocol::ProtocolVersion;
use std::sync::Arc;
use tempfile::TempDir;

mod common;
use common::{DIFFICULTY_INTERVAL, setup_mining_chain};

/// Block hash as stored in `BlockStore` (80-byte wire header), not bincode.
fn wire_block_hash(header: &blvm_node::BlockHeader) -> blvm_node::Hash {
    let mut header_data = [0u8; 80];
    header_data[0..4].copy_from_slice(&(header.version as i32).to_le_bytes());
    header_data[4..36].copy_from_slice(&header.prev_block_hash);
    header_data[36..68].copy_from_slice(&header.merkle_root);
    header_data[68..72].copy_from_slice(&(header.timestamp as u32).to_le_bytes());
    header_data[72..76].copy_from_slice(&(header.bits as u32).to_le_bytes());
    header_data[76..80].copy_from_slice(&(header.nonce as u32).to_le_bytes());
    blvm_node::storage::hashing::double_sha256(&header_data)
}

fn rpc_with_chain() -> (TempDir, Arc<Storage>, BlockchainRpc) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    setup_mining_chain(&storage, DIFFICULTY_INTERVAL).unwrap();
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let rpc = BlockchainRpc::with_dependencies_and_protocol(storage.clone(), protocol);
    (temp_dir, storage, rpc)
}

#[tokio::test]
async fn test_get_block_count_and_best_hash_with_chain() {
    let (_dir, storage, rpc) = rpc_with_chain();
    let count = rpc.get_block_count().await.unwrap();
    assert_eq!(count.as_u64().unwrap(), DIFFICULTY_INTERVAL - 1);

    let best = rpc
        .get_best_block_hash()
        .await
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let tip = storage.chain().get_tip_hash().unwrap().expect("tip");
    assert_eq!(best, blvm_node::storage::hashing::hash_to_rpc_hex(&tip));
}

#[tokio::test]
async fn test_get_block_hash_by_height() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    let hash = rpc.get_block_hash(0).await.unwrap();
    assert!(hash.as_str().unwrap().len() >= 64);
    let hash_tip = rpc.get_block_hash(DIFFICULTY_INTERVAL - 1).await.unwrap();
    assert!(hash_tip.as_str().unwrap().len() >= 64);
}

#[tokio::test]
async fn test_get_difficulty_and_chain_tips() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    let diff = rpc.get_difficulty().await.unwrap();
    assert!(diff.as_f64().unwrap_or(0.0) > 0.0);

    let tips = rpc.get_chain_tips().await.unwrap();
    assert!(tips.is_array());
    assert!(!tips.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_get_block_header_verbose_and_non_verbose() {
    let (_dir, storage, rpc) = rpc_with_chain();
    let tip_header = storage
        .chain()
        .get_tip_header()
        .unwrap()
        .expect("tip header");
    let tip = wire_block_hash(&tip_header);
    let hex = blvm_node::storage::hashing::hash_to_rpc_hex(&tip);

    let verbose = rpc.get_block_header(&hex, true).await.unwrap();
    assert!(verbose.get("height").is_some());
    assert!(verbose.get("hash").is_some());

    let raw = rpc.get_block_header(&hex, false).await.unwrap();
    assert!(raw.as_str().is_some());
}

#[tokio::test]
async fn test_get_blockchain_info_fields() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    let info = rpc.get_blockchain_info().await.unwrap();
    assert_eq!(info.get("chain").unwrap().as_str().unwrap(), "main");
    assert_eq!(
        info.get("blocks").unwrap().as_u64().unwrap(),
        DIFFICULTY_INTERVAL - 1
    );
    assert!(info.get("bestblockhash").is_some());
    assert!(info.get("difficulty").is_some());
}

#[tokio::test]
async fn test_validate_address_regtest() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    let params = serde_json::json!(["bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080"]);
    let result = rpc.validate_address(&params).await.unwrap();
    assert_eq!(result.get("isvalid").unwrap().as_bool(), Some(true));
}

#[tokio::test]
async fn test_get_block_by_wire_hash() {
    let (_dir, storage, rpc) = rpc_with_chain();
    let tip_header = storage
        .chain()
        .get_tip_header()
        .unwrap()
        .expect("tip header");
    let tip = wire_block_hash(&tip_header);
    let hex = blvm_node::storage::hashing::hash_to_rpc_hex(&tip);
    let block = rpc.get_block(&hex).await.unwrap();
    assert!(block.get("hash").is_some() || block.get("hex").is_some());
}

#[tokio::test]
async fn test_get_chain_tx_stats_with_chain() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    let stats = rpc
        .get_chain_tx_stats(&serde_json::json!([10]))
        .await
        .unwrap();
    assert!(stats.get("window_block_count").is_some());
    assert!(stats.get("txcount").is_some());
}

#[tokio::test]
async fn test_verify_chain_smoke() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    let ok = rpc.verify_chain(Some(1), Some(32)).await.unwrap();
    assert!(ok.is_boolean() || ok.get("valid").is_some());
}

#[tokio::test]
async fn test_get_prune_info_smoke() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    let info = rpc.get_prune_info(&serde_json::json!([])).await.unwrap();
    assert!(info.get("pruning_enabled").is_some());
}

#[tokio::test]
async fn test_get_txoutset_info_with_chain() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    let info = rpc.get_txoutset_info().await.unwrap();
    assert!(info.get("height").is_some());
    assert!(info.get("txouts").is_some());
}

#[tokio::test]
async fn test_get_blockchain_state_with_chain() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    let state = rpc.get_blockchain_state().await.unwrap();
    assert!(state.get("bestblockhash").is_some());
    assert!(state.get("difficulty").is_some());
    assert_eq!(state.get("chain").unwrap().as_str().unwrap(), "regtest");
}

#[tokio::test]
async fn test_get_index_info_with_chain() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    let info = rpc.get_index_info(&serde_json::json!([])).await.unwrap();
    assert!(info.get("txindex").is_some());
}

fn seed_wire_block(storage: &Storage) -> blvm_node::Hash {
    let block = blvm_node::Block {
        header: blvm_protocol::BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1_231_006_505,
            bits: 0x0f00ffff,
            nonce: 1,
        },
        transactions: vec![].into_boxed_slice(),
    };
    let hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage.blocks().store_height(0, &hash).unwrap();
    hash
}

#[tokio::test]
async fn test_get_block_stats_by_height() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    seed_wire_block(&storage);
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let rpc = BlockchainRpc::with_dependencies_and_protocol(storage, protocol);
    let stats = rpc.get_block_stats(&serde_json::json!([0])).await.unwrap();
    assert!(stats.get("height").is_some());
    assert!(stats.get("txs").is_some());
}

fn rpc_with_address_index() -> (TempDir, BlockchainRpc, String) {
    use blvm_node::config::IndexingConfig;
    use blvm_node::storage::database::default_backend;

    let temp_dir = TempDir::new().unwrap();
    let mut indexing = IndexingConfig::default();
    indexing.enable_address_index = true;
    let storage = Arc::new(
        Storage::with_backend_pruning_and_indexing(
            temp_dir.path(),
            default_backend(),
            None,
            Some(indexing),
            None,
            None,
        )
        .unwrap(),
    );
    let tx = common::valid_transaction();
    let script_hex = hex::encode(&tx.outputs[0].script_pubkey);
    storage
        .transactions()
        .index_transaction(&tx, &[0xde; 32], 1, 0)
        .unwrap();
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let rpc = BlockchainRpc::with_dependencies_and_protocol(storage, protocol);
    (temp_dir, rpc, script_hex)
}

#[tokio::test]
async fn test_getaddresstxids_with_indexed_script() {
    let (_dir, rpc, script_hex) = rpc_with_address_index();
    let ids = rpc
        .getaddresstxids(&serde_json::json!([script_hex]))
        .await
        .unwrap();
    let arr = ids.as_array().unwrap();
    assert_eq!(arr.len(), 1);
}

#[tokio::test]
async fn test_getaddressbalance_with_indexed_script() {
    let (_dir, rpc, script_hex) = rpc_with_address_index();
    let balance = rpc
        .getaddressbalance(&serde_json::json!([script_hex]))
        .await
        .unwrap();
    assert!(balance.get("balance").unwrap().as_i64().unwrap() > 0);
}

#[tokio::test]
async fn test_get_block_filter_for_wire_block() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let hash = seed_wire_block(&storage);
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let rpc = BlockchainRpc::with_dependencies_and_protocol(storage, protocol);
    let hash_hex = blvm_node::storage::hashing::hash_to_rpc_hex(&hash);
    let filter = rpc
        .get_block_filter(&serde_json::json!([hash_hex, 0]))
        .await
        .unwrap();
    assert!(filter.get("filter").is_some());
}

#[tokio::test]
async fn test_invalidate_and_reconsider_block_wire_hash() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let hash = seed_wire_block(&storage);
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let rpc = BlockchainRpc::with_dependencies_and_protocol(storage, protocol);
    let hash_hex = blvm_node::storage::hashing::hash_to_rpc_hex(&hash);
    rpc.invalidate_block(&serde_json::json!([hash_hex.clone()]))
        .await
        .unwrap();
    rpc.reconsider_block(&serde_json::json!([hash_hex]))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_get_address_info_with_hex_script() {
    let (_dir, rpc, script_hex) = rpc_with_address_index();
    let info = rpc
        .get_address_info(&serde_json::json!([script_hex]))
        .await
        .unwrap();
    assert_eq!(info.get("tx_count").unwrap().as_u64(), Some(1));
}

fn rpc_with_pruning_chain() -> (TempDir, Arc<Storage>, BlockchainRpc) {
    use blvm_node::config::PruningConfig;
    use blvm_node::storage::database::default_backend;

    let temp_dir = TempDir::new().unwrap();
    let pruning = Some(PruningConfig {
        mode: blvm_node::config::PruningMode::Normal {
            keep_from_height: 0,
            min_recent_blocks: 50,
        },
        auto_prune: true,
        auto_prune_interval: 100,
        min_blocks_to_keep: 50,
        prune_on_startup: false,
        incremental_prune_during_ibd: false,
        prune_window_size: 50,
        min_blocks_for_incremental_prune: 288,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        bip158_filters: None,
    });
    let storage = Arc::new(
        Storage::with_backend_pruning_and_indexing(
            temp_dir.path(),
            default_backend(),
            pruning,
            None,
            None,
            None,
        )
        .unwrap(),
    );
    setup_mining_chain(&storage, 120).unwrap();
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let rpc = BlockchainRpc::with_dependencies_and_protocol(storage.clone(), protocol);
    (temp_dir, storage, rpc)
}

#[tokio::test]
async fn test_prune_blockchain_when_enabled() {
    let (_dir, _storage, rpc) = rpc_with_pruning_chain();
    let result = rpc
        .prune_blockchain(&serde_json::json!([20]))
        .await
        .unwrap();
    assert_eq!(result.get("pruned_height").unwrap().as_u64(), Some(20));
}

#[tokio::test]
async fn test_prune_blockchain_without_pruning_errors() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    assert!(
        rpc.prune_blockchain(&serde_json::json!([10]))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn test_load_txoutset_missing_path_errors() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    assert!(rpc.load_txout_set(&serde_json::json!([])).await.is_err());
}

#[tokio::test]
async fn test_get_raw_transaction_stub_shape() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    let txid = "0000000000000000000000000000000000000000000000000000000000000001";
    let tx = rpc.get_raw_transaction(txid).await.unwrap();
    assert_eq!(tx.get("txid").unwrap().as_str().unwrap(), txid);
    assert!(tx.get("vin").unwrap().is_array());
}

#[tokio::test]
async fn test_verify_chain_default_params() {
    let (_dir, _storage, rpc) = rpc_with_chain();
    let ok = rpc.verify_chain(None, None).await.unwrap();
    assert!(ok.is_boolean() || ok.get("valid").is_some());
}

#[tokio::test]
async fn test_get_block_header_at_genesis_height() {
    let (_dir, storage, rpc) = rpc_with_chain();
    let genesis_hash = storage
        .blocks()
        .get_hash_by_height(0)
        .unwrap()
        .expect("genesis");
    let hex = blvm_node::storage::hashing::hash_to_rpc_hex(&genesis_hash);
    let header = rpc.get_block_header(&hex, true).await.unwrap();
    assert_eq!(header.get("height").unwrap().as_u64(), Some(0));
}
