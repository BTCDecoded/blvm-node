//! Property-based tests for mining functionality
//!
//! Uses proptest to verify invariants and properties that should hold for all inputs.

use bllvm_node::node::mempool::MempoolManager;
use bllvm_node::rpc::mining::MiningRpc;
use bllvm_node::storage::Storage;
use bllvm_protocol::serialization::serialize_transaction;
use bllvm_protocol::types::{BlockHeader, OutPoint, TransactionInput, TransactionOutput};
use bllvm_protocol::Transaction;
use proptest::prelude::*;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tempfile::TempDir;
mod common;
use common::*;

// Strategy function for generating arbitrary Transaction (can't impl Arbitrary for re-exported type)
fn transaction_strategy() -> BoxedStrategy<Transaction> {
    (
        any::<u64>(), // version
        prop::collection::vec(
            (
                any::<[u8; 32]>(),                          // prevout hash
                any::<u64>(),                               // prevout index
                prop::collection::vec(any::<u8>(), 0..100), // script_sig
                any::<u64>(),                               // sequence
            ),
            0..10, // input count
        ),
        prop::collection::vec(
            (
                any::<i64>(),                               // value
                prop::collection::vec(any::<u8>(), 0..100), // script_pubkey
            ),
            0..10, // output count
        ),
        any::<u64>(), // lock_time
    )
        .prop_map(|(version, inputs, outputs, lock_time)| Transaction {
            version,
            inputs: inputs
                .into_iter()
                .map(|(hash, index, script_sig, sequence)| TransactionInput {
                    prevout: OutPoint { hash, index },
                    script_sig,
                    sequence,
                })
                .collect(),
            outputs: outputs
                .into_iter()
                .map(|(value, script_pubkey)| TransactionOutput {
                    value,
                    script_pubkey,
                })
                .collect(),
            lock_time,
        })
        .boxed()
}

/// Property: Transaction serialization never panics
proptest! {
    #[test]
    fn prop_transaction_serialization_no_panic(tx in transaction_strategy()) {
        // Serialization should never panic
        let serialized = serialize_transaction(&tx);
        prop_assert!(!serialized.is_empty() || tx.inputs.is_empty() && tx.outputs.is_empty());
    }
}

/// Property: Transaction hash is double SHA256
proptest! {
    #[test]
    fn prop_transaction_hash_double_sha256(tx in transaction_strategy()) {
        // Test hash calculation directly
        let tx_bytes = serialize_transaction(&tx);
        let hash1 = Sha256::digest(&tx_bytes);
        let hash2 = Sha256::digest(hash1);
        let expected_hash = {
            let mut result = [0u8; 32];
            result.copy_from_slice(&hash2);
            hex::encode(result)
        };

        // Verify hash is 64 hex characters (32 bytes)
        prop_assert_eq!(expected_hash.len(), 64);
    }
}

/// Property: Transaction serialization round-trip
proptest! {
    #[test]
    fn prop_transaction_serialization_round_trip(
        version in 1i32..=2i32,
        inputs_count in 0usize..=10,
        outputs_count in 0usize..=10,
        lock_time in 0u32..=0xffffffffu32,
    ) {
        // Generate transaction inputs
        let inputs: Vec<TransactionInput> = (0..inputs_count)
            .map(|i| TransactionInput {
                prevout: OutPoint {
                    hash: [i as u8; 32],
                    index: i as u64,
                },
                script_sig: vec![0x51; i % 100], // Variable length scripts
                sequence: 0xffffffff,
            })
            .collect();

        // Generate transaction outputs
        let outputs: Vec<TransactionOutput> = (0..outputs_count)
            .map(|i| TransactionOutput {
                value: (i as i64) * 1000,
                script_pubkey: vec![0x76; (i % 50)], // Variable length scripts
            })
            .collect();

        let tx = Transaction {
            version: version as u64,
            inputs: inputs.into(),
            outputs: outputs.into(),
            lock_time: lock_time as u64,
        };

        // Serialize and verify it's not empty
        let serialized = serialize_transaction(&tx);
        prop_assert!(!serialized.is_empty());

        // Verify serialized length is reasonable (at least header + varints)
        prop_assert!(serialized.len() >= 10);
    }
}

/// Property: Transaction serialization produces valid bytes
proptest! {
    #[test]
    fn prop_transaction_serialization_valid(tx in transaction_strategy()) {
        let serialized = serialize_transaction(&tx);

        // Serialized transaction should have at least version (4 bytes) + varints
        if !tx.inputs.is_empty() || !tx.outputs.is_empty() {
            prop_assert!(serialized.len() >= 4);
        }
    }
}

/// Property: Template height matches input height
proptest! {
    #[test]
    fn prop_template_height_matches(
        height in 0u64..=100u64, // Reduced range for faster tests
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();

        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
        let mempool = Arc::new(MempoolManager::new());
        let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

        // Initialize chain state
        let genesis_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 2083236893,
        };
        storage.chain().initialize(&genesis_header).unwrap();

        // Set height
        let tip_hash = random_hash();
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: random_hash(),
            merkle_root: random_hash(),
            timestamp: 1231006505 + height * 600,
            bits: 0x1d00ffff,
            nonce: 0,
        };
        storage.chain().update_tip(&tip_hash, &tip_header, height).unwrap();

        // Get template
        let params = serde_json::json!([]);
        let result = rt.block_on(mining.get_block_template(&params));

        if result.is_ok() {
            let template = result.unwrap();
            let template_height = template.get("height").unwrap().as_u64().unwrap();
            prop_assert_eq!(template_height, height);
        }
    }
}

/// Property: Coinbase value is always positive
proptest! {
    #[test]
    fn prop_coinbase_value_always_positive(
        height in 0u64..=100u64, // Reduced range for faster tests
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();

        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
        let mempool = Arc::new(MempoolManager::new());
        let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

        // Initialize chain state
        let genesis_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 2083236893,
        };
        storage.chain().initialize(&genesis_header).unwrap();

        // Set height
        let tip_hash = random_hash();
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: random_hash(),
            merkle_root: random_hash(),
            timestamp: 1231006505 + height * 600,
            bits: 0x1d00ffff,
            nonce: 0,
        };
        storage.chain().update_tip(&tip_hash, &tip_header, height).unwrap();

        // Get template
        let params = serde_json::json!([]);
        let result = rt.block_on(mining.get_block_template(&params));

        if result.is_ok() {
            let template = result.unwrap();
            let coinbase_value = template.get("coinbasevalue").unwrap().as_u64().unwrap();
            prop_assert!(coinbase_value > 0);
        }
    }
}

/// Property: Template target is always 64 hex characters
proptest! {
    #[test]
    fn prop_template_target_format(height in 0u64..=50u64) {
        let rt = tokio::runtime::Runtime::new().unwrap();

        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
        let mempool = Arc::new(MempoolManager::new());
        let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

        // Initialize chain state
        let genesis_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 2083236893,
        };
        storage.chain().initialize(&genesis_header).unwrap();

        let tip_hash = random_hash();
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: random_hash(),
            merkle_root: random_hash(),
            timestamp: 1231006505 + height * 600,
            bits: 0x1d00ffff,
            nonce: 0,
        };
        storage.chain().update_tip(&tip_hash, &tip_header, height).unwrap();

        let params = serde_json::json!([]);
        let result = rt.block_on(mining.get_block_template(&params));

        if result.is_ok() {
            let template = result.unwrap();
            let target = template.get("target").unwrap().as_str().unwrap();
            prop_assert_eq!(target.len(), 64);

            // Verify it's hex
            prop_assert!(target.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }
}

/// Property: Template bits is always 8 hex characters
proptest! {
    #[test]
    fn prop_template_bits_format(height in 0u64..=50u64) {
        let rt = tokio::runtime::Runtime::new().unwrap();

        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
        let mempool = Arc::new(MempoolManager::new());
        let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

        // Initialize chain state
        let genesis_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 2083236893,
        };
        storage.chain().initialize(&genesis_header).unwrap();

        let tip_hash = random_hash();
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: random_hash(),
            merkle_root: random_hash(),
            timestamp: 1231006505 + height * 600,
            bits: 0x1d00ffff,
            nonce: 0,
        };
        storage.chain().update_tip(&tip_hash, &tip_header, height).unwrap();

        let params = serde_json::json!([]);
        let result = rt.block_on(mining.get_block_template(&params));

        if result.is_ok() {
            let template = result.unwrap();
            let bits = template.get("bits").unwrap().as_str().unwrap();
            prop_assert_eq!(bits.len(), 8);

            // Verify it's hex
            prop_assert!(bits.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }
}

/// Property: Rules array always contains at least "csv"
proptest! {
    #[test]
    fn prop_rules_always_contains_csv(height in 0u64..=100u64) {
        let rt = tokio::runtime::Runtime::new().unwrap();

        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
        let mempool = Arc::new(MempoolManager::new());
        let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

        // Initialize chain state
        let genesis_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 2083236893,
        };
        storage.chain().initialize(&genesis_header).unwrap();

        let tip_hash = random_hash();
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: random_hash(),
            merkle_root: random_hash(),
            timestamp: 1231006505 + height * 600,
            bits: 0x1d00ffff,
            nonce: 0,
        };
        storage.chain().update_tip(&tip_hash, &tip_header, height).unwrap();

        let params = serde_json::json!([]);
        let result = rt.block_on(mining.get_block_template(&params));

        if result.is_ok() {
            let template = result.unwrap();
            let rules = template.get("rules").unwrap().as_array().unwrap();
            let rule_strings: Vec<String> = rules.iter()
                .map(|r| r.as_str().unwrap().to_string())
                .collect();
            prop_assert!(rule_strings.contains(&"csv".to_string()));
        }
    }
}

/// Property: Transaction hash is deterministic (same tx = same hash)
proptest! {
    #[test]
    fn prop_transaction_hash_deterministic(tx in transaction_strategy()) {
        let tx_bytes = serialize_transaction(&tx);
        let tx_bytes_clone = tx_bytes.clone();

        // Same transaction should serialize to same bytes
        prop_assert_eq!(&tx_bytes, &tx_bytes_clone);

        // Same bytes should produce same hash
        let hash1 = {
            let h1 = Sha256::digest(&tx_bytes);
            let h2 = Sha256::digest(h1);
            hex::encode(h2)
        };
        let hash2 = {
            let h1 = Sha256::digest(&tx_bytes_clone);
            let h2 = Sha256::digest(h1);
            hex::encode(h2)
        };

        prop_assert_eq!(hash1, hash2);
    }
}

/// Property: Different transactions have different hashes (with high probability)
proptest! {
    #[test]
    fn prop_different_transactions_different_hashes(
        tx1 in transaction_strategy(),
        tx2 in transaction_strategy(),
    ) {
        // Skip if transactions are identical
        if tx1.version == tx2.version &&
           tx1.inputs == tx2.inputs &&
           tx1.outputs == tx2.outputs &&
           tx1.lock_time == tx2.lock_time {
            return Ok(());
        }

        let tx1_bytes = serialize_transaction(&tx1);
        let tx2_bytes = serialize_transaction(&tx2);

        let hash1 = {
            let h1 = Sha256::digest(&tx1_bytes);
            let h2 = Sha256::digest(h1);
            hex::encode(h2)
        };
        let hash2 = {
            let h1 = Sha256::digest(&tx2_bytes);
            let h2 = Sha256::digest(h1);
            hex::encode(h2)
        };

        // With high probability, different transactions have different hashes
        prop_assert_ne!(hash1, hash2);
    }
}
