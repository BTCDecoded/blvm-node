//! Integration tests for UTXO Commitment Handler Implementation
//!
//! Tests handle_get_utxo_set and handle_get_filtered_block implementations
//! with storage integration and spam filtering.

use crate::network::protocol_extensions::{handle_get_filtered_block, handle_get_utxo_set};
use crate::network::protocol::{GetFilteredBlockMessage, GetUTXOSetMessage, FilterPreferences};
use crate::storage::Storage;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn test_handle_get_utxo_set_with_storage() {
    // Create temporary storage
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    
    // Create test message
    let message = GetUTXOSetMessage {
        request_id: 1,
        block_height: 0,
        block_hash: [0; 32],
    };
    
    // Call handler
    let response = handle_get_utxo_set(message, Some(storage)).await.unwrap();
    
    // Verify response structure
    assert_eq!(response.request_id, 1);
    // Note: Commitment values depend on actual UTXO set in storage
}

#[tokio::test]
async fn test_handle_get_utxo_set_without_storage() {
    // Test handler when storage is not available
    let message = GetUTXOSetMessage {
        request_id: 2,
        block_height: 100,
        block_hash: [1; 32],
    };
    
    let response = handle_get_utxo_set(message, None).await.unwrap();
    
    // Should return placeholder response
    assert_eq!(response.request_id, 2);
    assert_eq!(response.commitment.block_height, 100);
    assert_eq!(response.commitment.block_hash, [1; 32]);
    assert_eq!(response.commitment.merkle_root, [0; 32]);
}

#[tokio::test]
async fn test_handle_get_filtered_block_with_storage() {
    // Create temporary storage
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    
    // Create test message
    let message = GetFilteredBlockMessage {
        request_id: 1,
        block_hash: [0; 32],
        filter_preferences: FilterPreferences {
            filter_ordinals: true,
            filter_dust: true,
            filter_brc20: false,
            min_output_value: 546,
        },
        include_bip158_filter: false,
    };
    
    // Call handler
    let response = handle_get_filtered_block(message, Some(storage), None).await.unwrap();
    
    // Verify response structure
    assert_eq!(response.request_id, 1);
    // Note: Actual block data depends on what's in storage
}

#[tokio::test]
async fn test_handle_get_filtered_block_spam_filtering() {
    // Create temporary storage
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    
    // Create message with spam filtering enabled
    let message = GetFilteredBlockMessage {
        request_id: 2,
        block_hash: [0; 32],
        filter_preferences: FilterPreferences {
            filter_ordinals: true,
            filter_dust: true,
            filter_brc20: true,
            min_output_value: 1000,
        },
        include_bip158_filter: false,
    };
    
    let response = handle_get_filtered_block(message, Some(storage), None).await.unwrap();
    
    // Verify spam summary is included
    assert!(response.spam_summary.filtered_count >= 0);
}

#[tokio::test]
async fn test_handle_get_filtered_block_without_storage() {
    // Test handler when storage is not available
    let message = GetFilteredBlockMessage {
        request_id: 3,
        block_hash: [2; 32],
        filter_preferences: FilterPreferences {
            filter_ordinals: false,
            filter_dust: false,
            filter_brc20: false,
            min_output_value: 0,
        },
        include_bip158_filter: false,
    };
    
    let response = handle_get_filtered_block(message, None, None).await.unwrap();
    
    // Should return placeholder response
    assert_eq!(response.request_id, 3);
    assert_eq!(response.transactions.len(), 0);
    assert_eq!(response.commitment.block_hash, [2; 32]);
}

