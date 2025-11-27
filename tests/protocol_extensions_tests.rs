//! Protocol Extensions Tests
//!
//! Tests for UTXO commitments protocol extensions.

use bllvm_node::network::protocol::{
    FilterPreferences, GetFilteredBlockMessage, GetUTXOSetMessage,
};
use bllvm_node::network::protocol_extensions::{handle_get_filtered_block, handle_get_utxo_set};
use bllvm_node::storage::Storage;
use bllvm_node::{Block, BlockHeader, Transaction};
use std::sync::Arc;

fn create_test_storage() -> Arc<Storage> {
    // Create a minimal test storage with temp directory
    let temp_dir = std::env::temp_dir().join(format!("bllvm_test_{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let storage = Storage::new(&temp_dir).unwrap();
    Arc::new(storage)
}

#[tokio::test]
async fn test_handle_get_utxo_set_no_storage() {
    // Test GetUTXOSet with no storage available
    let message = GetUTXOSetMessage {
        height: 0,
        block_hash: [0; 32],
    };

    let result = handle_get_utxo_set(message, None).await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Storage not available"));
}

#[tokio::test]
async fn test_handle_get_utxo_set_with_storage() {
    // Test GetUTXOSet with storage available
    let storage = create_test_storage();
    let message = GetUTXOSetMessage {
        height: 0,
        block_hash: [0; 32],
    };

    let result = handle_get_utxo_set(message, Some(storage)).await;

    // Should succeed even with empty storage
    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response.commitment.block_height, 0);
}

#[tokio::test]
async fn test_handle_get_utxo_set_specific_height() {
    // Test GetUTXOSet with specific height
    let storage = create_test_storage();
    let message = GetUTXOSetMessage {
        height: 100,
        block_hash: [0x42; 32],
    };

    let result = handle_get_utxo_set(message, Some(storage)).await;

    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response.commitment.block_height, 100);
}

#[tokio::test]
async fn test_handle_get_filtered_block_no_storage() {
    // Test GetFilteredBlock with no storage
    let message = GetFilteredBlockMessage {
        request_id: 12345,
        block_hash: [0x42; 32],
        filter_preferences: FilterPreferences {
            filter_ordinals: false,
            filter_dust: false,
            filter_brc20: false,
            min_output_value: 0,
        },
        include_bip158_filter: false,
    };

    let result = handle_get_filtered_block(message, None, None).await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Storage not available"));
}

#[tokio::test]
async fn test_handle_get_filtered_block_block_not_found() {
    // Test GetFilteredBlock with block not in storage
    let storage = create_test_storage();
    let message = GetFilteredBlockMessage {
        request_id: 12345,
        block_hash: [0xFF; 32], // Non-existent block
        filter_preferences: FilterPreferences {
            filter_ordinals: false,
            filter_dust: false,
            filter_brc20: false,
            min_output_value: 0,
        },
        include_bip158_filter: false,
    };

    let result = handle_get_filtered_block(message, Some(storage), None).await;

    // Should return error for block not found
    assert!(result.is_err());
}

#[tokio::test]
async fn test_handle_get_filtered_block_with_preferences() {
    // Test GetFilteredBlock with different filter preferences
    let storage = create_test_storage();

    // Add a test block to storage
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: [0; 32],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: vec![].into(),
            outputs: vec![].into(),
            lock_time: 0,
        }]
        .into_boxed_slice(),
    };

    let block_hash = [0x42; 32];
    storage.blocks().store_block(&block).unwrap();

    let message = GetFilteredBlockMessage {
        request_id: 12345,
        block_hash,
        filter_preferences: FilterPreferences {
            filter_ordinals: true,
            filter_dust: true,
            filter_brc20: true,
            min_output_value: 1000,
        },
        include_bip158_filter: false,
    };

    let result = handle_get_filtered_block(message, Some(storage), None).await;

    // Should succeed with filter preferences
    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response.request_id, 12345);
}

#[tokio::test]
async fn test_handle_get_filtered_block_with_bip158() {
    // Test GetFilteredBlock with BIP158 filter requested
    let storage = create_test_storage();

    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: [0; 32],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: vec![].into(),
            outputs: vec![].into(),
            lock_time: 0,
        }]
        .into_boxed_slice(),
    };

    let block_hash = [0x42; 32];
    storage.blocks().store_block(&block).unwrap();

    let message = GetFilteredBlockMessage {
        request_id: 12345,
        block_hash,
        filter_preferences: FilterPreferences {
            filter_ordinals: false,
            filter_dust: false,
            filter_brc20: false,
            min_output_value: 0,
        },
        include_bip158_filter: true,
    };

    let result = handle_get_filtered_block(message, Some(storage), None).await;

    // Should succeed with BIP158 filter
    assert!(result.is_ok());
    let response = result.unwrap();
    // BIP158 filter may or may not be present depending on filter_service availability
    assert_eq!(response.request_id, 12345);
}
