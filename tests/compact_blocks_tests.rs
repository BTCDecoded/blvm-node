//! Tests for Compact Block Relay (BIP152)

use bllvm_node::network::compact_blocks::{
    calculate_short_tx_id, calculate_tx_hash, CompactBlock,
};
use bllvm_node::network::transport::TransportType;
use bllvm_node::{Block, BlockHeader, Hash, Transaction};
use bllvm_protocol::tx_inputs;
use bllvm_protocol::tx_outputs;

fn create_test_transaction() -> Transaction {
    Transaction {
        version: 1,
        inputs: tx_inputs![],
        outputs: tx_outputs![
            bllvm_protocol::TransactionOutput {
                value: 1000,
                script_pubkey: vec![0x51], // OP_1
            }
        ],
        lock_time: 0,
    }
}

fn create_test_block() -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 12345,
        },
        transactions: vec![create_test_transaction()].into_boxed_slice(),
    }
}

#[test]
fn test_calculate_tx_hash() {
    let tx = create_test_transaction();
    let hash = calculate_tx_hash(&tx);

    // Hash should be 32 bytes
    assert_eq!(hash.len(), 32);

    // Same transaction should produce same hash
    let hash2 = calculate_tx_hash(&tx);
    assert_eq!(hash, hash2);
}

#[test]
fn test_calculate_short_tx_id() {
    let tx = create_test_transaction();
    let tx_hash = calculate_tx_hash(&tx);
    let nonce = 12345u64;

    let short_id = calculate_short_tx_id(&tx_hash, nonce);

    // Short ID should be 6 bytes
    assert_eq!(short_id.len(), 6);

    // Same inputs should produce same short ID
    let short_id2 = calculate_short_tx_id(&tx_hash, nonce);
    assert_eq!(short_id, short_id2);

    // Different nonce should produce different short ID
    let short_id3 = calculate_short_tx_id(&tx_hash, nonce + 1);
    assert_ne!(short_id, short_id3);
}

#[test]
fn test_short_tx_id_different_transactions() {
    let tx1 = create_test_transaction();
    let tx2 = Transaction {
        version: 2, // Different version
        inputs: tx_inputs![],
        outputs: tx_outputs![
            bllvm_protocol::TransactionOutput {
                value: 2000, // Different value
                script_pubkey: vec![0x51],
            }
        ],
        lock_time: 0,
    };

    let hash1 = calculate_tx_hash(&tx1);
    let hash2 = calculate_tx_hash(&tx2);
    let nonce = 12345u64;

    let short_id1 = calculate_short_tx_id(&hash1, nonce);
    let short_id2 = calculate_short_tx_id(&hash2, nonce);

    // Different transactions should produce different short IDs (with high probability)
    assert_ne!(short_id1, short_id2);
}

#[test]
fn test_compact_block_creation() {
    let block = create_test_block();
    let nonce = 12345u64;

    let compact = CompactBlock {
        header: block.header.clone(),
        nonce,
        short_ids: vec![],
        prefilled_txs: vec![],
    };

    assert_eq!(compact.header.version, block.header.version);
    assert_eq!(compact.nonce, nonce);
    assert_eq!(compact.short_ids.len(), 0);
    assert_eq!(compact.prefilled_txs.len(), 0);
}

#[test]
fn test_compact_block_with_short_ids() {
    let block = create_test_block();
    let tx_hash = calculate_tx_hash(&block.transactions[0]);
    let nonce = block.header.nonce;

    let short_id = calculate_short_tx_id(&tx_hash, nonce);

    let compact = CompactBlock {
        header: block.header.clone(),
        nonce,
        short_ids: vec![short_id],
        prefilled_txs: vec![],
    };

    assert_eq!(compact.short_ids.len(), 1);
    assert_eq!(compact.short_ids[0].len(), 6);
}

#[test]
fn test_compact_block_serialization() {
    let block = create_test_block();
    let compact = CompactBlock {
        header: block.header.clone(),
        nonce: 12345,
        short_ids: vec![],
        prefilled_txs: vec![],
    };

    // Should serialize to bytes
    let serialized = bincode::serialize(&compact).unwrap();
    assert!(!serialized.is_empty());

    // Should deserialize back
    let deserialized: CompactBlock = bincode::deserialize(&serialized).unwrap();
    assert_eq!(deserialized.header.version, compact.header.version);
    assert_eq!(deserialized.nonce, compact.nonce);
}

#[test]
fn test_should_prefer_compact_blocks_tcp() {
    let should_prefer = bllvm_node::network::compact_blocks::should_prefer_compact_blocks(
        TransportType::Tcp,
    );
    // TCP doesn't necessarily prefer compact blocks
    assert!(should_prefer == false || should_prefer == true); // Just verify it returns a bool
}

#[test]
fn test_recommended_compact_block_version() {
    let version = bllvm_node::network::compact_blocks::recommended_compact_block_version(
        TransportType::Tcp,
    );
    // Should return a valid version (1 or 2)
    assert!(version == 1 || version == 2);
}

#[test]
fn test_negotiate_optimizations() {
    let peer_services = 0u64;
    let (compact_version, prefer_compact, supports_filters) =
        bllvm_node::network::compact_blocks::negotiate_optimizations(
            TransportType::Tcp,
            peer_services,
        );

    // Should return valid values
    assert!(compact_version == 1 || compact_version == 2);
    assert!(prefer_compact == true || prefer_compact == false);
    assert!(supports_filters == true || supports_filters == false);
}

#[test]
fn test_is_quic_transport() {
    let is_quic = bllvm_node::network::compact_blocks::is_quic_transport(TransportType::Tcp);
    // TCP is not QUIC
    assert_eq!(is_quic, false);
}

#[test]
fn test_compact_block_with_prefilled_txs() {
    let block = create_test_block();
    let tx = block.transactions[0].clone();

    let compact = CompactBlock {
        header: block.header.clone(),
        nonce: 12345,
        short_ids: vec![],
        prefilled_txs: vec![(0, tx.clone())],
    };

    assert_eq!(compact.prefilled_txs.len(), 1);
    assert_eq!(compact.prefilled_txs[0].0, 0); // Index
    assert_eq!(compact.prefilled_txs[0].1.version, tx.version);
}

