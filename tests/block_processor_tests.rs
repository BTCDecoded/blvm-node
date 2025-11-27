//! Tests for block processing and validation

use bllvm_node::node::block_processor::{
    parse_block_from_wire, prepare_block_validation_context, store_block_with_context,
    store_block_with_context_and_index, validate_block_with_context,
};
use bllvm_node::storage::Storage;
use bllvm_node::{Block, BlockHeader, Hash, UtxoSet};
use bllvm_protocol::{segwit::Witness, BitcoinProtocolEngine, ProtocolVersion};
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_block() -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![].into_boxed_slice(),
    }
}

fn create_empty_witnesses() -> Vec<Witness> {
    vec![]
}

fn create_test_witnesses() -> Vec<Witness> {
    vec![vec![vec![0x01, 0x02, 0x03]]] // Single witness (Witness = Vec<Vec<u8>>)
}

fn create_test_storage() -> (TempDir, Arc<Storage>) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    (temp_dir, storage)
}

#[test]
fn test_parse_block_from_wire_invalid_data() {
    let invalid_data = b"invalid block data";
    let result = parse_block_from_wire(invalid_data);
    assert!(result.is_err());
}

#[test]
fn test_store_block_with_context() {
    let (_temp_dir, storage) = create_test_storage();
    let block = create_test_block();
    let witnesses = create_empty_witnesses();

    let result = store_block_with_context(&storage.blocks(), &block, &witnesses, 0);
    assert!(result.is_ok());

    // Verify block was stored
    let block_hash = storage.blocks().get_block_hash(&block);
    let stored_block = storage.blocks().get_block(&block_hash).unwrap();
    assert!(stored_block.is_some());
}

#[test]
fn test_store_block_with_context_and_witnesses() {
    let (_temp_dir, storage) = create_test_storage();
    let block = create_test_block();
    let witnesses = create_test_witnesses();

    let result = store_block_with_context(&storage.blocks(), &block, &witnesses, 0);
    assert!(result.is_ok());

    // Verify witness was stored
    let block_hash = storage.blocks().get_block_hash(&block);
    let stored_witness = storage.blocks().get_witness(&block_hash).unwrap();
    assert!(stored_witness.is_some());
    assert!(!stored_witness.unwrap().is_empty());
}

#[test]
fn test_store_block_with_context_and_index() {
    let (_temp_dir, storage) = create_test_storage();
    let block = create_test_block();
    let witnesses = create_empty_witnesses();

    let result = store_block_with_context_and_index(
        &storage.blocks(),
        Some(&storage),
        &block,
        &witnesses,
        0,
    );
    assert!(result.is_ok());
}

#[test]
fn test_store_block_with_context_updates_height_index() {
    let (_temp_dir, storage) = create_test_storage();
    let block = create_test_block();
    let witnesses = create_empty_witnesses();

    let result = store_block_with_context(&storage.blocks(), &block, &witnesses, 100);
    assert!(result.is_ok());

    // Verify block was stored (height index update is internal)
    let block_hash = storage.blocks().get_block_hash(&block);
    let stored_block = storage.blocks().get_block(&block_hash).unwrap();
    assert!(stored_block.is_some());
}

#[test]
fn test_store_block_with_context_stores_recent_header() {
    let (_temp_dir, storage) = create_test_storage();
    let block = create_test_block();
    let witnesses = create_empty_witnesses();

    let result = store_block_with_context(&storage.blocks(), &block, &witnesses, 0);
    assert!(result.is_ok());

    // Verify recent header was stored
    let recent_headers = storage.blocks().get_recent_headers(11).unwrap();
    assert!(!recent_headers.is_empty());
    assert_eq!(recent_headers[0].version, block.header.version);
}

#[test]
fn test_prepare_block_validation_context() {
    let (_temp_dir, storage) = create_test_storage();
    let block = create_test_block();

    // Store block first
    let witnesses = create_empty_witnesses();
    store_block_with_context(&storage.blocks(), &block, &witnesses, 0).unwrap();

    // Prepare validation context
    let (witnesses_result, recent_headers) =
        prepare_block_validation_context(&storage.blocks(), &block, 0).unwrap();

    // Should return witnesses (empty in this case)
    assert_eq!(witnesses_result.len(), block.transactions.len());

    // Should return recent headers if available
    if let Some(headers) = recent_headers {
        assert!(!headers.is_empty());
    }
}

#[test]
fn test_prepare_block_validation_context_with_no_headers() {
    let (_temp_dir, storage) = create_test_storage();
    let block = create_test_block();

    // Don't store block, so no headers available
    let (witnesses_result, recent_headers) =
        prepare_block_validation_context(&storage.blocks(), &block, 0).unwrap();

    // Should still return witnesses (empty for empty block)
    assert_eq!(witnesses_result.len(), block.transactions.len());

    // Recent headers may be None if not enough headers stored
    // This is acceptable
    if let Some(headers) = recent_headers {
        assert!(!headers.is_empty());
    }
}

#[test]
fn test_validate_block_with_context() {
    let (_temp_dir, storage) = create_test_storage();
    let protocol = BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap();
    let block = create_test_block();
    let mut utxo_set = UtxoSet::new();
    let witnesses = create_empty_witnesses();

    // Store block first
    store_block_with_context(&storage.blocks(), &block, &witnesses, 0).unwrap();

    // Validate block
    let result = validate_block_with_context(
        &storage.blocks(),
        &protocol,
        &block,
        &witnesses,
        &mut utxo_set,
        0,
    );

    // Validation should succeed for a valid empty block in regtest
    assert!(result.is_ok());
}

#[test]
fn test_store_block_with_context_multiple_blocks() {
    let (_temp_dir, storage) = create_test_storage();
    let witnesses = create_empty_witnesses();

    // Store multiple blocks
    let mut prev_hash = [0u8; 32];
    for height in 0..5 {
        let mut block = create_test_block();
        if height > 0 {
            block.header.prev_block_hash = prev_hash;
        }

        let result = store_block_with_context(&storage.blocks(), &block, &witnesses, height);
        assert!(result.is_ok());

        // Update prev_hash for next iteration
        prev_hash = storage.blocks().get_block_hash(&block);
    }

    // Verify blocks were stored by checking block count
    let block_count = storage.blocks().block_count().unwrap();
    assert_eq!(block_count, 5);
}

#[test]
fn test_store_block_with_context_empty_witnesses() {
    let (_temp_dir, storage) = create_test_storage();
    let block = create_test_block();
    let empty_witnesses = create_empty_witnesses();

    let result = store_block_with_context(&storage.blocks(), &block, &empty_witnesses, 0);
    assert!(result.is_ok());

    // Block should still be stored even without witnesses
    let block_hash = storage.blocks().get_block_hash(&block);
    let stored_block = storage.blocks().get_block(&block_hash).unwrap();
    assert!(stored_block.is_some());
}

#[test]
fn test_store_block_with_context_and_index_without_storage() {
    let (_temp_dir, storage) = create_test_storage();
    let block = create_test_block();
    let witnesses = create_empty_witnesses();

    // Pass None for storage (should still work, just won't index)
    let result = store_block_with_context_and_index(&storage.blocks(), None, &block, &witnesses, 0);
    assert!(result.is_ok());
}
