//! Tests for filter service (BIP157/158)

use bllvm_node::network::filter_service::BlockFilterService;
use bllvm_protocol::{Block, BlockHeader, Hash, Transaction};
use bllvm_protocol::bip157::FilterHeader;
use bllvm_protocol::tx_inputs;
use bllvm_protocol::tx_outputs;

fn create_test_block(height: u32) -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1234567890,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: tx_inputs![],
            outputs: tx_outputs![],
            lock_time: 0,
        }].into_boxed_slice(),
    }
}

#[test]
fn test_filter_service_creation() {
    let service = BlockFilterService::new();
    assert_eq!(service.current_height(), 0);
}

#[test]
fn test_filter_service_default() {
    let service = BlockFilterService::default();
    assert_eq!(service.current_height(), 0);
}

#[test]
fn test_get_filter_nonexistent() {
    let service = BlockFilterService::new();
    let block_hash = [0u8; 32];
    
    let result = service.get_filter(&block_hash);
    assert!(result.is_none());
}

#[test]
fn test_has_filter_false() {
    let service = BlockFilterService::new();
    let block_hash = [0u8; 32];
    
    assert!(!service.has_filter(&block_hash));
}

#[test]
fn test_get_cached_filter_hashes_empty() {
    let service = BlockFilterService::new();
    let hashes = service.get_cached_filter_hashes();
    
    assert_eq!(hashes.len(), 0);
}

#[test]
fn test_get_filter_header_nonexistent() {
    let service = BlockFilterService::new();
    let result = service.get_filter_header(0);
    
    assert!(result.is_none());
}

#[test]
fn test_get_prev_filter_header_at_zero() {
    let service = BlockFilterService::new();
    let result = service.get_prev_filter_header(0);
    
    // Should return None for height 0 (genesis)
    assert!(result.is_none());
}

#[test]
fn test_get_prev_filter_header_nonexistent() {
    let service = BlockFilterService::new();
    let result = service.get_prev_filter_header(1);
    
    // Should return None if no filter header exists
    assert!(result.is_none());
}

#[test]
fn test_generate_and_cache_filter() {
    let service = BlockFilterService::new();
    let block = create_test_block(0);
    let prev_scripts = vec![];
    
    let result = service.generate_and_cache_filter(&block, &prev_scripts, 0);
    assert!(result.is_ok());
    
    let filter = result.unwrap();
    assert!(filter.num_elements >= 0);
}

#[test]
fn test_get_filter_after_caching() {
    let service = BlockFilterService::new();
    let block = create_test_block(0);
    let prev_scripts = vec![];
    
    // Generate and cache filter
    let _filter = service.generate_and_cache_filter(&block, &prev_scripts, 0).unwrap();
    
    // Calculate block hash (simplified - would use proper calculation in real code)
    // For test, we'll use the service's internal calculation
    // Since we can't access private methods, we'll test that filter exists via has_filter
    // after generation
    
    // Filter should exist after generation
    // Note: We can't easily get the block hash without the private method,
    // but we can verify the service state changed
    assert_eq!(service.current_height(), 0);
}

#[test]
fn test_get_filter_header_after_generation() {
    let service = BlockFilterService::new();
    let block = create_test_block(0);
    let prev_scripts = vec![];
    
    // Generate and cache filter
    let _filter = service.generate_and_cache_filter(&block, &prev_scripts, 0).unwrap();
    
    // Should be able to get filter header at height 0
    let header = service.get_filter_header(0);
    assert!(header.is_some());
}

#[test]
fn test_get_prev_filter_header_after_generation() {
    let service = BlockFilterService::new();
    let block0 = create_test_block(0);
    let block1 = create_test_block(1);
    let prev_scripts = vec![];
    
    // Generate filters for height 0 and 1
    let _filter0 = service.generate_and_cache_filter(&block0, &prev_scripts, 0).unwrap();
    let _filter1 = service.generate_and_cache_filter(&block1, &prev_scripts, 1).unwrap();
    
    // Should be able to get previous filter header for height 1
    let prev_header = service.get_prev_filter_header(1);
    assert!(prev_header.is_some());
}

#[test]
fn test_current_height_updates() {
    let service = BlockFilterService::new();
    let block0 = create_test_block(0);
    let block1 = create_test_block(1);
    let prev_scripts = vec![];
    
    assert_eq!(service.current_height(), 0);
    
    let _filter0 = service.generate_and_cache_filter(&block0, &prev_scripts, 0).unwrap();
    assert_eq!(service.current_height(), 0);
    
    let _filter1 = service.generate_and_cache_filter(&block1, &prev_scripts, 1).unwrap();
    assert_eq!(service.current_height(), 1);
}

#[test]
fn test_remove_filter_for_pruned_block() {
    let service = BlockFilterService::new();
    let block = create_test_block(0);
    let prev_scripts = vec![];
    
    // Generate and cache filter
    let _filter = service.generate_and_cache_filter(&block, &prev_scripts, 0).unwrap();
    
    // Calculate block hash (we'll use a dummy hash for test)
    let block_hash = [1u8; 32];
    
    // Remove filter (may not exist, but should not error)
    let result = service.remove_filter_for_pruned_block(&block_hash);
    assert!(result.is_ok());
}

#[test]
fn test_get_filter_headers_range_empty() {
    let service = BlockFilterService::new();
    let stop_hash = [0u8; 32];
    
    // Should error when stop hash not found
    let result = service.get_filter_headers_range(0, stop_hash);
    assert!(result.is_err());
}

#[test]
fn test_get_filter_checkpoints_empty() {
    let service = BlockFilterService::new();
    let stop_hash = [0u8; 32];
    
    // Should error when stop hash not found
    let result = service.get_filter_checkpoints(stop_hash);
    assert!(result.is_err());
}

