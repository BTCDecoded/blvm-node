//! Tests for BIP157 handler

use blvm_node::network::bip157_handler::{
    generate_cfilter_response, handle_getcfcheckpt, handle_getcfheaders, handle_getcfilters,
};
use blvm_node::network::filter_service::BlockFilterService;
use blvm_node::network::protocol::{
    GetCfcheckptMessage, GetCfheadersMessage, GetCfiltersMessage, ProtocolMessage,
};
use blvm_node::storage::Storage;
use blvm_protocol::{Block, BlockHeader, Transaction, TransactionOutput};
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
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Unsupported filter type")
    );
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

    let result = handle_getcfilters(&request, &filter_service, Some(&storage));
    let responses = result.expect("empty chain has no filters");
    assert!(responses.is_empty());
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
fn test_handle_getcfilters_uses_height_index_hash() {
    let filter_service = create_test_filter_service();
    let (_, storage) = create_test_storage();
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0x11; 32],
            timestamp: 1_231_006_505,
            bits: 0x0f00ffff,
            nonce: 7,
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into_boxed_slice(),
    };
    let block_hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage.blocks().store_height(0, &block_hash).unwrap();

    let request = GetCfiltersMessage {
        filter_type: 0,
        start_height: 0,
        stop_hash: block_hash,
    };
    let responses = handle_getcfilters(&request, &filter_service, Some(&storage)).unwrap();
    match responses.as_slice() {
        [ProtocolMessage::Cfilter(message)] => {
            assert_eq!(message.block_hash, block_hash);
            assert!(message.num_elements >= 1);
        }
        other => panic!("expected one cfilter, got {other:?}"),
    }
    assert!(filter_service.has_filter(&block_hash));

    let again = handle_getcfilters(&request, &filter_service, Some(&storage)).unwrap();
    match again.as_slice() {
        [ProtocolMessage::Cfilter(message)] => assert_eq!(message.block_hash, block_hash),
        other => panic!("expected the cached cfilter, got {other:?}"),
    }
}

#[test]
fn test_handle_getcfilters_reads_wire_output_scripts() {
    let filter_service = create_test_filter_service();
    let (_, storage) = create_test_storage();
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0x22; 32],
            timestamp: 1_231_006_505,
            bits: 0x0f00ffff,
            nonce: 8,
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 1,
                script_pubkey: vec![0x51, 0x52],
            }],
            lock_time: 0,
        }]
        .into_boxed_slice(),
    };
    let block_hash = storage.blocks().get_block_hash(&block);
    let payload = blvm_protocol::serialization::serialize_block(&block);
    storage
        .blocks()
        .store_block_wire_bytes(&block, 0, &payload)
        .unwrap();
    storage.blocks().store_height(0, &block_hash).unwrap();
    let expected = blvm_protocol::bip158::build_block_filter(block.transactions.as_ref(), &[])
        .unwrap();

    let request = GetCfiltersMessage {
        filter_type: 0,
        start_height: 0,
        stop_hash: block_hash,
    };
    let responses = handle_getcfilters(&request, &filter_service, Some(&storage)).unwrap();
    match responses.as_slice() {
        [ProtocolMessage::Cfilter(message)] => {
            assert_eq!(message.block_hash, block_hash);
            assert_eq!(message.num_elements, expected.num_elements);
            assert_eq!(
                message.filter_data.as_slice(),
                expected.filter_data.as_slice()
            );
        }
        other => panic!("expected one cfilter, got {other:?}"),
    }
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
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Unsupported filter type")
    );
}

#[test]
fn test_handle_getcfheaders_valid_filter_type() {
    let filter_service = create_test_filter_service();

    let request = GetCfheadersMessage {
        filter_type: 0, // Valid
        start_height: 0,
        stop_hash: [0u8; 32],
    };

    let result = handle_getcfheaders(&request, &filter_service);
    let message = result.expect_err("stop hash is not indexed").to_string();
    assert!(
        message.contains("Stop hash not found"),
        "unexpected error: {message}"
    );
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
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Unsupported filter type")
    );
}

#[test]
fn test_handle_getcfcheckpt_valid_filter_type() {
    let filter_service = create_test_filter_service();

    let request = GetCfcheckptMessage {
        filter_type: 0, // Valid
        stop_hash: [0u8; 32],
    };

    let result = handle_getcfcheckpt(&request, &filter_service);
    let message = result.expect_err("stop hash is not indexed").to_string();
    assert!(
        message.contains("Stop hash not found"),
        "unexpected error: {message}"
    );
}

#[test]
fn test_generate_cfilter_response_invalid_filter_type() {
    let filter_service = create_test_filter_service();
    let block_hash = [0u8; 32];

    let result = generate_cfilter_response(block_hash, 1, &filter_service);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Unsupported filter type")
    );
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
