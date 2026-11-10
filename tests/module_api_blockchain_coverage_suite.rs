//! `BlockchainApi` read-only module queries against seeded storage.

use blvm_node::module::api::blockchain::BlockchainApi;
use blvm_node::storage::Storage;
use blvm_node::{BlockHeader, Hash, OutPoint, UTXO};
use blvm_protocol::block::calculate_tx_id;
use std::sync::Arc;
use tempfile::TempDir;

mod common;
use common::{DIFFICULTY_INTERVAL, setup_mining_chain, valid_transaction};

const SHORT_CHAIN: u64 = 12;

fn wire_block_hash(header: &BlockHeader) -> Hash {
    use blvm_node::storage::hashing::double_sha256;
    let mut header_data = [0u8; 80];
    header_data[0..4].copy_from_slice(&(header.version as i32).to_le_bytes());
    header_data[4..36].copy_from_slice(&header.prev_block_hash);
    header_data[36..68].copy_from_slice(&header.merkle_root);
    header_data[68..72].copy_from_slice(&(header.timestamp as u32).to_le_bytes());
    header_data[72..76].copy_from_slice(&(header.bits as u32).to_le_bytes());
    header_data[76..80].copy_from_slice(&(header.nonce as u32).to_le_bytes());
    double_sha256(&header_data)
}

fn api_with_chain(blocks: u64) -> (TempDir, Arc<Storage>, BlockchainApi) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    setup_mining_chain(&storage, blocks).unwrap();
    let api = BlockchainApi::new(storage.clone());
    (temp_dir, storage, api)
}

#[tokio::test]
async fn test_blockchain_api_block_and_chain_queries() {
    let (_dir, storage, api) = api_with_chain(SHORT_CHAIN);

    let height = api.get_block_height().await.unwrap();
    assert_eq!(height, SHORT_CHAIN - 1);

    let tip = api.get_chain_tip().await.unwrap();
    let stored_tip = storage.chain().get_tip_hash().unwrap().expect("tip");
    assert_eq!(tip, stored_tip);

    let genesis_hash = api.get_hash_by_height(0).await.unwrap().expect("genesis");
    let block = api
        .get_block(&genesis_hash)
        .await
        .unwrap()
        .expect("genesis block");
    let header = api
        .get_block_header(&genesis_hash)
        .await
        .unwrap()
        .expect("header");
    assert_eq!(header.version, block.header.version);

    assert!(api.has_block(&genesis_hash).await.unwrap());
    assert_eq!(api.block_count().await.unwrap(), SHORT_CHAIN as usize);

    let by_height = api
        .get_block_by_height(1)
        .await
        .unwrap()
        .expect("height 1 block");
    assert_eq!(by_height.header.version, 1);

    let resolved_height = api
        .get_height_by_hash(&genesis_hash)
        .await
        .unwrap()
        .expect("genesis height");
    assert_eq!(resolved_height, 0);

    let range = api.get_blocks_by_height_range(0, 3).await.unwrap();
    assert_eq!(range.len(), 4);

    let metadata = api
        .get_block_metadata(&genesis_hash)
        .await
        .unwrap()
        .expect("metadata");
    assert!(metadata.n_tx >= 1);

    let headers = api.get_recent_headers(3).await.unwrap();
    assert!(!headers.is_empty());

    assert!(api.get_chain_info().await.unwrap().is_some());
    assert!(api.get_chain_params().await.unwrap().is_some());
}

#[tokio::test]
async fn test_blockchain_api_transaction_and_utxo_queries() {
    let (_dir, storage, api) = api_with_chain(SHORT_CHAIN);

    let tx = valid_transaction();
    let tx_hash = calculate_tx_id(&tx);
    let tip_header = storage
        .chain()
        .get_tip_header()
        .unwrap()
        .expect("tip header");
    let tip_hash = wire_block_hash(&tip_header);
    storage
        .transactions()
        .index_transaction(&tx, &tip_hash, SHORT_CHAIN - 1, 0)
        .unwrap();

    let fetched = api.get_transaction(&tx_hash).await.unwrap();
    assert!(fetched.is_some());
    assert!(api.has_transaction(&tx_hash).await.unwrap());

    let outpoint = OutPoint {
        hash: [0x42; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 50_000,
        script_pubkey: vec![0x51].into(),
        height: 1,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&outpoint, &utxo).unwrap();

    assert!(api.get_utxo(&outpoint).await.unwrap().is_some());
    assert!(api.has_utxo(&outpoint).await.unwrap());
}

#[tokio::test]
async fn test_blockchain_api_long_chain_tip_matches_mining_fixture() {
    let (_dir, storage, api) = api_with_chain(DIFFICULTY_INTERVAL);
    assert_eq!(
        api.get_block_height().await.unwrap(),
        DIFFICULTY_INTERVAL - 1
    );
    assert_eq!(
        api.get_chain_tip().await.unwrap(),
        storage.chain().get_tip_hash().unwrap().unwrap()
    );
}

#[tokio::test]
async fn test_blockchain_api_uninitialized_chain_errors() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let api = BlockchainApi::new(storage);

    assert!(api.get_chain_tip().await.is_err());
    assert!(api.get_block_height().await.is_err());
    assert!(api.get_chain_info().await.unwrap().is_none());
}

#[tokio::test]
async fn test_blockchain_api_missing_transaction_and_utxo() {
    let (_dir, _storage, api) = api_with_chain(SHORT_CHAIN);
    let missing = [0xdeu8; 32];
    assert!(api.get_transaction(&missing).await.unwrap().is_none());
    assert!(!api.has_transaction(&missing).await.unwrap());

    let outpoint = OutPoint {
        hash: [0xef; 32],
        index: 99,
    };
    assert!(api.get_utxo(&outpoint).await.unwrap().is_none());
    assert!(!api.has_utxo(&outpoint).await.unwrap());
}
