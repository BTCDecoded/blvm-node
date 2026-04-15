//! UTXO commitment handler tests (wired via `tests/utxo_commitments_integration.rs`).
//!
//! Uses **Redb** for temp storage — `Storage::new()` defaults to a backend that may use RocksDB;
//! RocksDB background threads have caused SIGSEGV in short‑lived `cargo test` processes.

use blvm_node::network::protocol::{FilterPreferences, GetFilteredBlockMessage, GetUTXOSetMessage};
use blvm_node::network::protocol_extensions::{handle_get_filtered_block, handle_get_utxo_set};
use blvm_node::storage::database::DatabaseBackend;
use blvm_node::storage::Storage;
#[cfg(feature = "utxo-commitments")]
use blvm_protocol::utxo_commitments::merkle_tree::UtxoMerkleTree;
#[cfg(feature = "utxo-commitments")]
use blvm_protocol::{OutPoint, UTXO};
use std::sync::Arc;

fn create_test_storage() -> Arc<Storage> {
    let temp_dir = std::env::temp_dir().join(format!(
        "blvm_uc_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    #[cfg(feature = "redb")]
    {
        let storage = Storage::with_backend(&temp_dir, DatabaseBackend::Redb).unwrap();
        return Arc::new(storage);
    }
    #[cfg(not(feature = "redb"))]
    {
        let storage = Storage::new(&temp_dir).unwrap();
        Arc::new(storage)
    }
}

#[tokio::test]
async fn test_handle_get_utxo_set_without_storage() {
    let message = GetUTXOSetMessage {
        height: 100,
        block_hash: [1; 32],
    };

    let result = handle_get_utxo_set(message, None).await;
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Storage not available"));
}

#[tokio::test]
async fn test_handle_get_utxo_set_with_storage_empty() {
    let storage = create_test_storage();
    let message = GetUTXOSetMessage {
        height: 0,
        block_hash: [0; 32],
    };

    let result = handle_get_utxo_set(message, Some(storage)).await;
    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response.commitment.block_height, 0);
}

/// Independent `UtxoMerkleTree::from_utxo_set` must match `handle_get_utxo_set` output.
#[cfg(feature = "utxo-commitments")]
#[tokio::test]
async fn test_handle_get_utxo_set_matches_merkle_oracle() {
    let storage = create_test_storage();

    let outpoint = OutPoint {
        hash: [0xC0; 32],
        index: 7,
    };
    let utxo = UTXO {
        value: 12_345,
        script_pubkey: vec![0x76, 0xa9, 0x14].into(),
        height: 1,
        is_coinbase: false,
    };
    storage
        .utxos()
        .add_utxo(&outpoint, &utxo)
        .expect("seed utxo");

    let block_height = 42u64;
    let block_hash = [0xAB; 32];
    let message = GetUTXOSetMessage {
        height: block_height,
        block_hash,
    };

    let response = handle_get_utxo_set(message, Some(Arc::clone(&storage)))
        .await
        .expect("handler");

    let utxo_set = storage.utxos().get_all_utxos().expect("utxo set");
    let oracle_tree = UtxoMerkleTree::from_utxo_set(&utxo_set).expect("oracle tree");
    let expected = oracle_tree.generate_commitment(block_hash, block_height);

    assert_eq!(response.commitment.merkle_root, expected.merkle_root);
    assert_eq!(response.commitment.total_supply, expected.total_supply);
    assert_eq!(response.commitment.utxo_count, expected.utxo_count);
    assert_eq!(response.commitment.block_height, expected.block_height);
    assert_eq!(response.commitment.block_hash, expected.block_hash);
}

#[tokio::test]
async fn test_handle_get_filtered_block_without_storage() {
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

    let result = handle_get_filtered_block(message, None, None).await;
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Storage not available"));
}

#[tokio::test]
async fn test_handle_get_filtered_block_missing_block_returns_err() {
    let storage = create_test_storage();
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

    let result = handle_get_filtered_block(message, Some(storage), None).await;
    assert!(result.is_err());
}
