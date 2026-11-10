//! BIP157 handler helpers (`handle_getcfilters`, headers, checkpoints).

use blvm_node::BlockHeader;
use blvm_node::Hash;
use blvm_node::network::bip157_handler::{
    generate_cfilter_response, handle_getcfcheckpt, handle_getcfheaders, handle_getcfilters,
};
use blvm_node::network::filter_service::BlockFilterService;
use blvm_node::network::protocol::{
    GetCfcheckptMessage, GetCfheadersMessage, GetCfiltersMessage, ProtocolMessage,
};
use blvm_node::storage::Storage;
use std::sync::Arc;
use tempfile::TempDir;

mod common;
use common::setup_mining_chain;

const CHAIN_LEN: u64 = 6;

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

fn chain_storage() -> (TempDir, Arc<Storage>) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    setup_mining_chain(&storage, CHAIN_LEN).unwrap();
    (temp_dir, storage)
}

fn seeded_chain() -> (TempDir, Arc<Storage>, BlockFilterService) {
    let (temp_dir, storage) = chain_storage();

    let filter_service = BlockFilterService::new();
    for height in 0..CHAIN_LEN {
        let hash = storage
            .blocks()
            .get_hash_by_height(height)
            .unwrap()
            .expect("block hash");
        let block = storage.blocks().get_block(&hash).unwrap().expect("block");
        filter_service
            .generate_and_cache_filter(&block, &[], height as u32)
            .unwrap();
    }

    (temp_dir, storage, filter_service)
}

#[test]
fn test_handle_getcfilters_returns_cfilter_messages() {
    let (_dir, storage) = chain_storage();
    let filter_service = BlockFilterService::new();
    let stop_header = storage
        .chain()
        .get_tip_header()
        .unwrap()
        .expect("tip header");
    let stop_hash = wire_block_hash(&stop_header);

    let request = GetCfiltersMessage {
        filter_type: 0,
        start_height: 0,
        stop_hash,
    };
    let responses = handle_getcfilters(&request, &filter_service, Some(&storage)).unwrap();
    assert!(!responses.is_empty());
    assert!(matches!(responses[0], ProtocolMessage::Cfilter(_)));
}

#[test]
fn test_handle_getcfilters_unsupported_type_errors() {
    let (_dir, storage, filter_service) = seeded_chain();
    let request = GetCfiltersMessage {
        filter_type: 1,
        start_height: 0,
        stop_hash: [0u8; 32],
    };
    assert!(handle_getcfilters(&request, &filter_service, Some(&storage)).is_err());
}

#[test]
fn test_handle_getcfheaders_returns_header_chain() {
    let (_dir, _storage, filter_service) = seeded_chain();
    let cached = filter_service.get_cached_filter_hashes();
    let stop_hash = *cached.last().expect("cached hash");

    let request = GetCfheadersMessage {
        filter_type: 0,
        start_height: 0,
        stop_hash,
    };
    let response = handle_getcfheaders(&request, &filter_service).unwrap();
    match response {
        ProtocolMessage::Cfheaders(msg) => {
            assert_eq!(msg.filter_type, 0);
            assert!(!msg.filter_headers.is_empty());
        }
        other => panic!("expected Cfheaders, got {other:?}"),
    }
}

#[test]
fn test_handle_getcfcheckpt_returns_checkpoints() {
    let (_dir, _storage, filter_service) = seeded_chain();
    let stop_hash = *filter_service
        .get_cached_filter_hashes()
        .last()
        .expect("stop hash");

    let request = GetCfcheckptMessage {
        filter_type: 0,
        stop_hash,
    };
    let response = handle_getcfcheckpt(&request, &filter_service).unwrap();
    match response {
        ProtocolMessage::Cfcheckpt(msg) => {
            assert_eq!(msg.stop_hash, stop_hash);
            assert!(!msg.filter_header_hashes.is_empty());
        }
        other => panic!("expected Cfcheckpt, got {other:?}"),
    }
}

#[test]
fn test_generate_cfilter_response_from_cache() {
    let (_dir, _storage, filter_service) = seeded_chain();
    let block_hash = filter_service.get_cached_filter_hashes()[0];
    let response = generate_cfilter_response(block_hash, 0, &filter_service).unwrap();
    match response {
        ProtocolMessage::Cfilter(msg) => {
            assert_eq!(msg.block_hash, block_hash);
            assert_eq!(msg.filter_type, 0);
        }
        other => panic!("expected Cfilter, got {other:?}"),
    }
}

#[test]
fn test_generate_cfilter_response_missing_filter_errors() {
    let filter_service = BlockFilterService::new();
    assert!(generate_cfilter_response([0xab; 32], 0, &filter_service).is_err());
}
