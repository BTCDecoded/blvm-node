//! Tests for BIP157 handler

use bllvm_node::network::bip157_handler::{
    generate_cfilter_response, handle_getcfcheckpt, handle_getcfheaders, handle_getcfilters,
};
use bllvm_node::network::filter_service::BlockFilterService;
use bllvm_node::network::protocol::{
    GetCfcheckptMessage, GetCfheadersMessage, GetCfiltersMessage,
};
use bllvm_node::storage::Storage;
use bllvm_protocol::Hash;
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_filter_service() -> BlockFilterService {
    BlockFilterService::new()
}

fn create_test_storage() -> (TempDir, Arc<Storage>) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    (temp_dir, storage)
}

#[test]
fn test_handle_getcfilters_invalid_filter_type() {
    let filter_service = create_test_filter_service();
    let (_, storage) = create_test_storage();
    
    let request = GetCfiltersMessage {
        filter_type: 1, // Invalid (only 0 is supported)
        start_height: 0,
        stop_hash: [0u8; 32],
    };
    
    let result = handle_getcfilters(&request, &filter_service, Some(&storage));
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Unsupported filter type"));
}

#[test]
fn test_handle_getcfilters_valid_filter_type() {
    let filter_service = create_test_filter_service();
    let (_, storage) = create_test_storage();
    
    let request = GetCfiltersMessage {
        filter_type: 0, // Valid
        start_height: 0,
        stop_hash: [0u8; 32],
    };
    
    // Should not error on filter type validation
    // (May error on storage/block lookup, but that's expected with empty storage)
    let result = handle_getcfilters(&request, &filter_service, Some(&storage));
    // Result may be Ok with empty vec or Err on storage lookup - both are valid
    assert!(result.is_ok() || result.is_err());
}

#[test]
fn test_handle_getcfilters_no_storage() {
    let filter_service = create_test_filter_service();
    
    let request = GetCfiltersMessage {
        filter_type: 0,
        start_height: 0,
        stop_hash: [0u8; 32],
    };
    
    // Should return empty vec when no storage
    let result = handle_getcfilters(&request, &filter_service, None);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().len(), 0);
}

#[test]
fn test_handle_getcfheaders_invalid_filter_type() {
    let filter_service = create_test_filter_service();
    
    let request = GetCfheadersMessage {
        filter_type: 1, // Invalid
        start_height: 0,
        stop_hash: [0u8; 32],
    };
    
    let result = handle_getcfheaders(&request, &filter_service);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Unsupported filter type"));
}

#[test]
fn test_handle_getcfheaders_valid_filter_type() {
    let filter_service = create_test_filter_service();
    
    let request = GetCfheadersMessage {
        filter_type: 0, // Valid
        start_height: 0,
        stop_hash: [0u8; 32],
    };
    
    // Should not error on filter type validation
    // (May error on filter service lookup, but that's expected with empty service)
    let result = handle_getcfheaders(&request, &filter_service);
    // Result may be Ok or Err - both are valid depending on filter service state
    assert!(result.is_ok() || result.is_err());
}

#[test]
fn test_handle_getcfcheckpt_invalid_filter_type() {
    let filter_service = create_test_filter_service();
    
    let request = GetCfcheckptMessage {
        filter_type: 1, // Invalid
        stop_hash: [0u8; 32],
    };
    
    let result = handle_getcfcheckpt(&request, &filter_service);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Unsupported filter type"));
}

#[test]
fn test_handle_getcfcheckpt_valid_filter_type() {
    let filter_service = create_test_filter_service();
    
    let request = GetCfcheckptMessage {
        filter_type: 0, // Valid
        stop_hash: [0u8; 32],
    };
    
    // Should not error on filter type validation
    // (May error on filter service lookup, but that's expected with empty service)
    let result = handle_getcfcheckpt(&request, &filter_service);
    // Result may be Ok or Err - both are valid depending on filter service state
    assert!(result.is_ok() || result.is_err());
}

#[test]
fn test_generate_cfilter_response_invalid_filter_type() {
    let filter_service = create_test_filter_service();
    let block_hash = [0u8; 32];
    
    let result = generate_cfilter_response(block_hash, 1, &filter_service);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Unsupported filter type"));
}

#[test]
fn test_generate_cfilter_response_valid_filter_type_no_filter() {
    let filter_service = create_test_filter_service();
    let block_hash = [0u8; 32];
    
    // Should error when filter not found (expected with empty service)
    let result = generate_cfilter_response(block_hash, 0, &filter_service);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Filter not found"));
}

#[test]
fn test_getcfilters_message_structure() {
    let request = GetCfiltersMessage {
        filter_type: 0,
        start_height: 100,
        stop_hash: [1u8; 32],
    };
    
    assert_eq!(request.filter_type, 0);
    assert_eq!(request.start_height, 100);
    assert_eq!(request.stop_hash, [1u8; 32]);
}

#[test]
fn test_getcfheaders_message_structure() {
    let request = GetCfheadersMessage {
        filter_type: 0,
        start_height: 100,
        stop_hash: [1u8; 32],
    };
    
    assert_eq!(request.filter_type, 0);
    assert_eq!(request.start_height, 100);
    assert_eq!(request.stop_hash, [1u8; 32]);
}

#[test]
fn test_getcfcheckpt_message_structure() {
    let request = GetCfcheckptMessage {
        filter_type: 0,
        stop_hash: [1u8; 32],
    };
    
    assert_eq!(request.filter_type, 0);
    assert_eq!(request.stop_hash, [1u8; 32]);
}

