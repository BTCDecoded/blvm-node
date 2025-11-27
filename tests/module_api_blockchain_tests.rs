//! Comprehensive tests for blockchain API module

use bllvm_node::module::api::blockchain::BlockchainApi;
use bllvm_node::storage::Storage;
use bllvm_protocol::{tx_inputs, tx_outputs};
use bllvm_protocol::{
    Block, BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO,
};
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_storage() -> (TempDir, Arc<Storage>) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    (temp_dir, storage)
}

fn create_test_block(height: u64) -> Block {
    let mut prev_hash = [0u8; 32];
    prev_hash[0] = (height % 256) as u8;

    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: prev_hash,
            merkle_root: [0u8; 32],
            timestamp: 1231006505 + height,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![].into_boxed_slice(),
    }
}

#[tokio::test]
async fn test_blockchain_api_creation() {
    let (_temp_dir, storage) = create_test_storage();
    let _api = BlockchainApi::new(storage);
    // Should create successfully
    assert!(true);
}

#[tokio::test]
async fn test_get_block_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Store a block
    let block = create_test_block(0);
    let block_hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage.blocks().store_height(0, &block_hash).unwrap();

    // Retrieve it
    let result = api.get_block(&block_hash).await;
    assert!(result.is_ok());
    let retrieved = result.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().header.version, block.header.version);
}

#[tokio::test]
async fn test_get_block_header_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Store a block
    let block = create_test_block(0);
    let block_hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();

    // Retrieve header
    let result = api.get_block_header(&block_hash).await;
    assert!(result.is_ok());
    let retrieved = result.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().version, block.header.version);
}

#[tokio::test]
async fn test_get_block_by_height_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Store a block at height 0
    let block = create_test_block(0);
    let block_hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage.blocks().store_height(0, &block_hash).unwrap();

    // Retrieve by height
    let result = api.get_block_by_height(0).await;
    assert!(result.is_ok());
    let retrieved = result.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().header.version, block.header.version);
}

#[tokio::test]
async fn test_get_hash_by_height_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Store a block at height 0
    let block = create_test_block(0);
    let block_hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage.blocks().store_height(0, &block_hash).unwrap();

    // Retrieve hash by height
    let result = api.get_hash_by_height(0).await;
    assert!(result.is_ok());
    let retrieved = result.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap(), block_hash);
}

#[tokio::test]
async fn test_get_height_by_hash_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Store a block at height 0
    let block = create_test_block(0);
    let block_hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage.blocks().store_height(0, &block_hash).unwrap();

    // Retrieve height by hash
    let result = api.get_height_by_hash(&block_hash).await;
    assert!(result.is_ok());
    let retrieved = result.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap(), 0);
}

#[tokio::test]
async fn test_get_blocks_by_height_range_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Store multiple blocks
    for height in 0..5 {
        let block = create_test_block(height);
        let block_hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_block(&block).unwrap();
        storage.blocks().store_height(height, &block_hash).unwrap();
    }

    // Retrieve range
    let result = api.get_blocks_by_height_range(0, 4).await;
    assert!(result.is_ok());
    let blocks = result.unwrap();
    assert_eq!(blocks.len(), 5);
}

#[tokio::test]
async fn test_get_block_metadata_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Store a block
    let block = create_test_block(0);
    let block_hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();

    // Retrieve metadata
    let result = api.get_block_metadata(&block_hash).await;
    assert!(result.is_ok());
    let retrieved = result.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().n_tx, 0); // Empty transactions
}

#[tokio::test]
async fn test_has_block_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Store a block
    let block = create_test_block(0);
    let block_hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();

    // Check existence
    let result = api.has_block(&block_hash).await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    // Check non-existent block
    let fake_hash = [0xFFu8; 32];
    let result = api.has_block(&fake_hash).await;
    assert!(result.is_ok());
    assert!(!result.unwrap());
}

#[tokio::test]
async fn test_block_count_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Initially empty
    let result = api.block_count().await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 0);

    // Store some blocks
    for height in 0..3 {
        let block = create_test_block(height);
        storage.blocks().store_block(&block).unwrap();
    }

    // Check count
    let result = api.block_count().await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 3);
}

#[tokio::test]
async fn test_get_recent_headers_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Store some blocks with heights
    for height in 0..5 {
        let block = create_test_block(height);
        let block_hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_block(&block).unwrap();
        storage.blocks().store_height(height, &block_hash).unwrap();
        storage
            .blocks()
            .store_recent_header(height, &block.header)
            .unwrap();
    }

    // Retrieve recent headers
    let result = api.get_recent_headers(3).await;
    assert!(result.is_ok());
    let headers = result.unwrap();
    assert!(headers.len() <= 3);
}

#[tokio::test]
async fn test_get_transaction_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Create and store a transaction
    let tx = Transaction {
        version: 1,
        inputs: tx_inputs![TransactionInput {
            prevout: OutPoint {
                hash: [0u8; 32],
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xFFFFFFFF,
        }],
        outputs: tx_outputs![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x51], // OP_1
        }],
        lock_time: 0,
    };

    let tx_hash = bllvm_protocol::block::calculate_tx_id(&tx);
    let block_hash = [0u8; 32];
    storage
        .transactions()
        .index_transaction(&tx, &block_hash, 0, 0)
        .unwrap();

    // Retrieve transaction
    let result = api.get_transaction(&tx_hash).await;
    assert!(result.is_ok());
    let retrieved = result.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().version, tx.version);
}

#[tokio::test]
async fn test_has_transaction_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Create and store a transaction
    let tx = Transaction {
        version: 1,
        inputs: tx_inputs![],
        outputs: tx_outputs![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x51],
        }],
        lock_time: 0,
    };

    let tx_hash = bllvm_protocol::block::calculate_tx_id(&tx);
    let block_hash = [0u8; 32];
    storage
        .transactions()
        .index_transaction(&tx, &block_hash, 0, 0)
        .unwrap();

    // Check existence
    let result = api.has_transaction(&tx_hash).await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    // Check non-existent transaction
    let fake_hash = [0xFFu8; 32];
    let result = api.has_transaction(&fake_hash).await;
    assert!(result.is_ok());
    assert!(!result.unwrap());
}

#[tokio::test]
async fn test_get_utxo_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Create and store a UTXO
    let outpoint = OutPoint {
        hash: [0x42u8; 32],
        index: 0,
    };

    let utxo = UTXO {
        value: 1000000,
        script_pubkey: vec![0x51], // OP_1
        height: 0,
        is_coinbase: false,
    };

    storage.utxos().add_utxo(&outpoint, &utxo).unwrap();

    // Retrieve UTXO
    let result = api.get_utxo(&outpoint).await;
    assert!(result.is_ok());
    let retrieved = result.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().value, utxo.value);
}

#[tokio::test]
async fn test_has_utxo_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Create and store a UTXO
    let outpoint = OutPoint {
        hash: [0x42u8; 32],
        index: 0,
    };

    let utxo = UTXO {
        value: 1000000,
        script_pubkey: vec![0x51],
        height: 0,
        is_coinbase: false,
    };

    storage.utxos().add_utxo(&outpoint, &utxo).unwrap();

    // Check existence
    let result = api.has_utxo(&outpoint).await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    // Check non-existent UTXO
    let fake_outpoint = OutPoint {
        hash: [0xFFu8; 32],
        index: 999,
    };
    let result = api.has_utxo(&fake_outpoint).await;
    assert!(result.is_ok());
    assert!(!result.unwrap());
}

#[tokio::test]
async fn test_get_chain_info_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Initialize chain with genesis
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 0,
    };

    storage.chain().initialize(&genesis_header).unwrap();

    // Retrieve chain info
    let result = api.get_chain_info().await;
    assert!(result.is_ok());
    let chain_info = result.unwrap();
    assert!(chain_info.is_some());
    assert_eq!(chain_info.unwrap().height, 0);
}

#[tokio::test]
async fn test_get_chain_tip_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Initialize chain with genesis
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 0,
    };

    storage.chain().initialize(&genesis_header).unwrap();

    // Retrieve chain tip
    let result = api.get_chain_tip().await;
    assert!(result.is_ok());
    let tip = result.unwrap();
    // Tip should be genesis hash - calculate it using storage method
    let genesis_block = Block {
        header: genesis_header.clone(),
        transactions: vec![].into_boxed_slice(),
    };
    let genesis_hash = storage.blocks().get_block_hash(&genesis_block);
    assert_eq!(tip, genesis_hash);
}

#[tokio::test]
async fn test_get_block_height_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Initialize chain with genesis
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 0,
    };

    storage.chain().initialize(&genesis_header).unwrap();

    // Retrieve block height
    let result = api.get_block_height().await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 0);
}

#[tokio::test]
async fn test_get_chain_params_with_data() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(Arc::clone(&storage));

    // Initialize chain with genesis
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 0,
    };

    storage.chain().initialize(&genesis_header).unwrap();

    // Retrieve chain params
    let result = api.get_chain_params().await;
    assert!(result.is_ok());
    let params = result.unwrap();
    assert!(params.is_some());
    assert_eq!(params.unwrap().network, "mainnet");
}

#[tokio::test]
async fn test_concurrent_block_queries() {
    let (_temp_dir, storage) = create_test_storage();
    let api = Arc::new(BlockchainApi::new(Arc::clone(&storage)));

    // Store a block
    let block = create_test_block(0);
    let block_hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage.blocks().store_height(0, &block_hash).unwrap();

    // Make concurrent queries
    let mut handles = vec![];
    for _ in 0..10 {
        let api_clone = Arc::clone(&api);
        let hash = block_hash;
        handles.push(tokio::spawn(
            async move { api_clone.get_block(&hash).await },
        ));
    }

    // Wait for all
    let results: Vec<_> = futures::future::join_all(handles).await;

    // All should succeed
    for result in results {
        assert!(result.is_ok());
        let block_result = result.unwrap();
        assert!(block_result.is_ok());
        assert!(block_result.unwrap().is_some());
    }
}

#[tokio::test]
async fn test_error_handling_invalid_height_range() {
    let (_temp_dir, storage) = create_test_storage();
    let api = BlockchainApi::new(storage);

    // Try to get blocks with invalid range (end < start)
    let result = api.get_blocks_by_height_range(10, 5).await;
    assert!(result.is_ok());
    // Should return empty vector, not error
    assert!(result.unwrap().is_empty());
}
