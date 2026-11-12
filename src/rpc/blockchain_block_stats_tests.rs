use super::*;
use blvm_protocol::opcodes::OP_1;
use blvm_protocol::{
    Block, BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput,
};

fn tx_with_output(value: i64) -> Transaction {
    Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [1u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    }
}

#[tokio::test]
async fn block_stats_ignore_a_negative_output() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 0,
            bits: 0,
            nonce: 0,
        },
        transactions: vec![tx_with_output(-1), tx_with_output(50_000)].into_boxed_slice(),
    };
    let hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();

    let stats = BlockchainRpc::with_dependencies(storage)
        .get_block_stats(&json!([crate::storage::hashing::hash_to_rpc_hex(&hash)]))
        .await
        .unwrap();
    assert_eq!(stats["outs"], 2);
    assert_eq!(stats["total_out"].as_u64(), Some(50_000));
    assert_eq!(stats["totalfee"].as_f64(), Some(0.0));
}

#[tokio::test]
async fn utxo_increase_is_outputs_minus_spent_prevouts() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let coinbase = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [0u8; 32],
                index: 0xffffffff,
            },
            script_sig: Vec::new(),
            sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 50_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 2,
            bits: 0,
            nonce: 1,
        },
        transactions: vec![coinbase, tx_with_output(40_000)].into_boxed_slice(),
    };
    let hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    let stats = BlockchainRpc::with_dependencies(storage)
        .get_block_stats(&json!([crate::storage::hashing::hash_to_rpc_hex(&hash)]))
        .await
        .unwrap();
    assert_eq!(stats["outs"], 2);
    assert_eq!(stats["ins"], 2);
    assert_eq!(stats["utxo_increase"].as_i64(), Some(1));
}

#[tokio::test]
async fn header_without_a_height_is_not_height_zero() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let header = BlockHeader {
        version: 1,
        prev_block_hash: [9u8; 32],
        merkle_root: [3u8; 32],
        timestamp: 50,
        bits: 0x207fffff,
        nonce: 4,
    };
    let hash = [4u8; 32];
    storage.blocks().store_header(&hash, &header).unwrap();
    let rpc_hash = crate::storage::hashing::hash_to_rpc_hex(&hash);
    let err = BlockchainRpc::with_dependencies(storage)
        .get_block_header(&rpc_hash, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no height index"), "{err}");
}

#[tokio::test]
async fn a_height_read_error_is_not_a_missing_block() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    storage.blocks().insert_short_height_row(100).unwrap();
    let err = BlockchainRpc::with_dependencies(storage)
        .get_block_hash(100)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("storage read failed"), "{err}");
    assert!(!err.contains("Block not found"), "{err}");
}

#[tokio::test]
async fn segwit_totals_skip_the_legacy_transaction() {
    use blvm_protocol::serialization::transaction::serialize_transaction_with_witness;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let legacy = tx_with_output(50_000);
    let segwit = tx_with_output(40_000);
    let witness = vec![vec![vec![0x01u8]]];
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 3,
            bits: 0,
            nonce: 5,
        },
        transactions: vec![legacy, segwit.clone()].into_boxed_slice(),
    };
    let hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage
        .blocks()
        .store_witness(&hash, &[Vec::new(), witness.clone()])
        .unwrap();
    let stats = BlockchainRpc::with_dependencies(storage)
        .get_block_stats(&json!([crate::storage::hashing::hash_to_rpc_hex(&hash)]))
        .await
        .unwrap();
    let segwit_size = serialize_transaction_with_witness(&segwit, &witness)
        .unwrap()
        .len();
    assert_eq!(stats["swtxs"], 1);
    assert_eq!(stats["swtotal_size"].as_u64(), Some(segwit_size as u64));
    assert!(stats["swtotal_weight"].as_u64().unwrap() < stats["total_weight"].as_u64().unwrap());
}

#[tokio::test]
async fn getblock_verbosity_zero_is_hex_and_one_is_json() {
    use blvm_protocol::serialization::serialize_block_with_witnesses;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1,
            bits: 0,
            nonce: 2,
        },
        transactions: vec![tx_with_output(50_000)].into_boxed_slice(),
    };
    let hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    let rpc_hash = crate::storage::hashing::hash_to_rpc_hex(&hash);
    let rpc = BlockchainRpc::with_dependencies(storage);
    let hex_value = rpc.get_block_with_verbosity(&rpc_hash, 0).await.unwrap();
    let expected = hex::encode(serialize_block_with_witnesses(&block, &[], true));
    assert_eq!(hex_value.as_str(), Some(expected.as_str()));
    let object = rpc.get_block(&rpc_hash).await.unwrap();
    assert!(object.get("tx").is_some());
    assert!(object.as_str().is_none());
}

#[cfg(feature = "production")]
#[tokio::test]
async fn one_thousand_sat_fee_is_min_max_and_median() {
    use blvm_consensus::reorganization::{BlockUndoLog, UndoEntry};
    use blvm_protocol::UTXO;
    use std::sync::Arc;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let spent = OutPoint {
        hash: [7u8; 32],
        index: 0,
    };
    let spender = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: spent,
            script_sig: Vec::new(),
            sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 1_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_000,
            bits: 0,
            nonce: 0,
        },
        transactions: vec![tx_with_output(50_000), spender].into_boxed_slice(),
    };
    let hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    let mut undo = BlockUndoLog::new();
    undo.push(UndoEntry {
        outpoint: spent,
        previous_utxo: Some(Arc::new(UTXO {
            value: 2_000,
            script_pubkey: vec![OP_1].into(),
            height: 1,
            is_coinbase: false,
        })),
        new_utxo: None,
    });
    storage.blocks().store_undo_log(&hash, &undo).unwrap();

    let stats = BlockchainRpc::with_dependencies(storage)
        .get_block_stats(&json!([crate::storage::hashing::hash_to_rpc_hex(&hash)]))
        .await
        .unwrap();
    assert_eq!(stats["minfee"].as_f64(), Some(0.00001));
    assert_eq!(stats["maxfee"].as_f64(), Some(0.00001));
    assert_eq!(stats["medianfee"].as_f64(), Some(0.00001));
}

#[cfg(feature = "production")]
#[tokio::test]
async fn spent_script_is_in_the_filter_and_the_header_is_not_zero() {
    use blvm_consensus::reorganization::{BlockUndoLog, UndoEntry};
    use blvm_protocol::bip158::{build_block_filter, match_filter};

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let spent = OutPoint {
        hash: [9u8; 32],
        index: 0,
    };
    let script = vec![OP_1];
    let spender = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: spent,
            script_sig: Vec::new(),
            sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 1_000,
            script_pubkey: vec![0x51],
        }]
        .into(),
        lock_time: 0,
    };
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_000,
            bits: 0,
            nonce: 1,
        },
        transactions: vec![tx_with_output(50_000), spender].into_boxed_slice(),
    };
    let hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    let mut undo = BlockUndoLog::new();
    undo.push(UndoEntry {
        outpoint: spent,
        previous_utxo: Some(std::sync::Arc::new(blvm_protocol::UTXO {
            value: 2_000,
            script_pubkey: script.clone().into(),
            height: 1,
            is_coinbase: false,
        })),
        new_utxo: None,
    });
    storage.blocks().store_undo_log(&hash, &undo).unwrap();

    let result = BlockchainRpc::with_dependencies(Arc::clone(&storage))
        .get_block_filter(&json!([crate::storage::hashing::hash_to_rpc_hex(&hash)]))
        .await
        .unwrap();
    let built = build_block_filter(&hash, block.transactions.as_ref(), &[script.clone()]).unwrap();
    assert!(match_filter(&hash, &built, &script));
    assert_eq!(
        result["filter"].as_str().unwrap(),
        hex::encode(built.filter_data.as_slice())
    );
    assert_ne!(result["header"].as_str().unwrap(), &"0".repeat(64));
    let filter_hash = crate::storage::hashing::double_sha256(built.filter_data.as_slice());
    let mut chained = [0u8; 64];
    chained[..32].copy_from_slice(&filter_hash);
    let expected = hex::encode(crate::storage::hashing::double_sha256(&chained));
    assert_eq!(result["header"].as_str().unwrap(), expected);
    let peer = blvm_protocol::bip157::FilterHeader::new(&built, None);
    assert_eq!(
        result["header"].as_str().unwrap(),
        hex::encode(peer.header_hash())
    );

    let rpc = BlockchainRpc::with_dependencies(Arc::clone(&storage));
    let before = rpc.get_index_info(&json!([])).await.unwrap();
    assert_eq!(
        before["basic block filter index"]["synced"].as_bool(),
        Some(false)
    );
    storage.chain().initialize(&block.header).unwrap();
    storage.blocks().store_height(0, &hash).unwrap();
    let after = rpc.get_index_info(&json!([])).await.unwrap();
    assert_eq!(
        after["basic block filter index"]["synced"].as_bool(),
        Some(true)
    );
}

#[cfg(feature = "production")]
#[tokio::test]
async fn invalidate_tip_moves_to_the_parent_and_drops_the_coinbase() {
    use blvm_consensus::reorganization::{BlockUndoLog, UndoEntry};
    use blvm_protocol::UTXO;
    use blvm_protocol::block::calculate_tx_id;
    use std::sync::Arc as StdArc;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let parent = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1_600_000_000,
        bits: 0,
        nonce: 1,
    };
    storage.chain().initialize(&parent).unwrap();
    let parent_hash = storage.chain().get_tip_hash().unwrap().unwrap();
    storage
        .blocks()
        .store_header(&parent_hash, &parent)
        .unwrap();

    let coinbase = tx_with_output(50_000);
    let coinbase_id = calculate_tx_id(&coinbase);
    let child = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_001,
            bits: 0,
            nonce: 2,
        },
        transactions: vec![coinbase].into_boxed_slice(),
    };
    let child_hash = storage.blocks().get_block_hash(&child);
    storage.blocks().store_block(&child).unwrap();
    storage
        .blocks()
        .store_header(&child_hash, &child.header)
        .unwrap();
    storage.blocks().store_height(1, &child_hash).unwrap();
    let outpoint = OutPoint {
        hash: coinbase_id,
        index: 0,
    };
    let utxo = UTXO {
        value: 50_000,
        script_pubkey: vec![OP_1].into(),
        height: 1,
        is_coinbase: true,
    };
    storage.utxos().add_utxo(&outpoint, &utxo).unwrap();
    let mut undo = BlockUndoLog::new();
    undo.push(UndoEntry {
        outpoint,
        previous_utxo: None,
        new_utxo: Some(StdArc::new(utxo)),
    });
    storage.blocks().store_undo_log(&child_hash, &undo).unwrap();
    storage
        .chain()
        .update_tip(&child_hash, &child.header, 1)
        .unwrap();

    BlockchainRpc::with_dependencies(Arc::clone(&storage))
        .invalidate_block(&json!([crate::storage::hashing::hash_to_rpc_hex(
            &child_hash
        )]))
        .await
        .unwrap();

    assert_eq!(
        storage.chain().get_tip_hash().unwrap().unwrap(),
        parent_hash
    );
    assert!(storage.utxos().get_utxo(&outpoint).unwrap().is_none());
    let tips = BlockchainRpc::with_dependencies(storage)
        .get_chain_tips()
        .await
        .unwrap();
    let old_tip = tips
        .as_array()
        .unwrap()
        .iter()
        .find(|tip| {
            tip["hash"].as_str()
                == Some(crate::storage::hashing::hash_to_rpc_hex(&child_hash).as_str())
        })
        .unwrap();
    assert_eq!(old_tip["status"], "invalid");
}

#[tokio::test]
async fn side_tip_is_valid_fork() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let parent = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1_600_000_000,
        bits: 0,
        nonce: 1,
    };
    storage.chain().initialize(&parent).unwrap();
    let parent_hash = storage.chain().get_tip_hash().unwrap().unwrap();
    let main = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_001,
            bits: 0,
            nonce: 2,
        },
        transactions: vec![tx_with_output(50_000)].into_boxed_slice(),
    };
    let main_hash = storage.blocks().get_block_hash(&main);
    storage
        .chain()
        .update_tip(&main_hash, &main.header, 1)
        .unwrap();
    let fork = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_002,
            bits: 0,
            nonce: 3,
        },
        transactions: vec![tx_with_output(50_000)].into_boxed_slice(),
    };
    let fork_hash = storage.blocks().get_block_hash(&fork);
    storage
        .chain()
        .block_index()
        .insert(
            &fork_hash,
            1,
            &parent_hash,
            crate::storage::block_index::BlockIndexStatus::Valid,
        )
        .unwrap();

    let tips = BlockchainRpc::with_dependencies(storage)
        .get_chain_tips()
        .await
        .unwrap();
    let tips = tips.as_array().unwrap();
    let status_of = |hash: &blvm_protocol::Hash| {
        let hex = crate::storage::hashing::hash_to_rpc_hex(hash);
        tips.iter()
            .find(|tip| tip["hash"].as_str() == Some(hex.as_str()))
            .unwrap()["status"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(status_of(&main_hash), "active");
    assert_eq!(status_of(&fork_hash), "valid-fork");
}

#[tokio::test]
async fn verifychain_uses_the_stored_transactions() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let parent = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1_600_000_000,
        bits: 0,
        nonce: 1,
    };
    storage.chain().initialize(&parent).unwrap();
    let parent_hash = storage.chain().get_tip_hash().unwrap().unwrap();
    let tx = tx_with_output(50_000);
    let root = blvm_protocol::mining::calculate_merkle_root(std::slice::from_ref(&tx)).unwrap();
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: root,
            timestamp: 1_600_000_001,
            bits: 0,
            nonce: 2,
        },
        transactions: vec![tx].into_boxed_slice(),
    };
    let hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage.blocks().store_height(1, &hash).unwrap();
    storage.chain().update_tip(&hash, &block.header, 1).unwrap();
    let rpc = BlockchainRpc::with_dependencies(Arc::clone(&storage));
    assert_eq!(
        rpc.verify_chain(Some(2), Some(1)).await.unwrap(),
        serde_json::Value::Bool(true)
    );

    let mut wrong = block.clone();
    wrong.header.merkle_root = [9u8; 32];
    wrong.header.nonce = 3;
    let wrong_hash = storage.blocks().get_block_hash(&wrong);
    storage.blocks().store_block(&wrong).unwrap();
    storage.blocks().store_height(1, &wrong_hash).unwrap();
    storage
        .chain()
        .update_tip(&wrong_hash, &wrong.header, 1)
        .unwrap();
    let report = rpc.verify_chain(Some(2), Some(1)).await.unwrap();
    let errors = report["errors"].as_array().unwrap();
    assert!(errors.iter().any(|err| {
        err.as_str()
            .is_some_and(|s| s.contains("incorrect merkle root"))
    }));
}

#[tokio::test]
async fn missing_undo_log_does_not_move_the_tip() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let parent = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1_600_000_000,
        bits: 0,
        nonce: 1,
    };
    storage.chain().initialize(&parent).unwrap();
    let parent_hash = storage.chain().get_tip_hash().unwrap().unwrap();
    storage
        .blocks()
        .store_header(&parent_hash, &parent)
        .unwrap();
    let child = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_001,
            bits: 0,
            nonce: 2,
        },
        transactions: vec![tx_with_output(50_000)].into_boxed_slice(),
    };
    let child_hash = storage.blocks().get_block_hash(&child);
    storage.blocks().store_block(&child).unwrap();
    storage
        .blocks()
        .store_header(&child_hash, &child.header)
        .unwrap();
    storage.blocks().store_height(1, &child_hash).unwrap();
    storage
        .chain()
        .update_tip(&child_hash, &child.header, 1)
        .unwrap();

    let err = BlockchainRpc::with_dependencies(Arc::clone(&storage))
        .invalidate_block(&json!([crate::storage::hashing::hash_to_rpc_hex(
            &child_hash
        )]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("missing undo log"), "{err}");
    assert_eq!(storage.chain().get_tip_hash().unwrap().unwrap(), child_hash);
}

#[tokio::test]
async fn invalidate_below_the_tip_disconnects_the_descendant() {
    use blvm_consensus::reorganization::BlockUndoLog;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let genesis = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1_600_000_000,
        bits: 0,
        nonce: 1,
    };
    storage.chain().initialize(&genesis).unwrap();
    let genesis_hash = storage.chain().get_tip_hash().unwrap().unwrap();
    storage
        .blocks()
        .store_header(&genesis_hash, &genesis)
        .unwrap();

    let mut prev = genesis_hash;
    let mut child_hash = genesis_hash;
    for (height, nonce) in [(1u64, 2u64), (2, 3)] {
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: prev,
                merkle_root: [0u8; 32],
                timestamp: 1_600_000_000 + height,
                bits: 0,
                nonce,
            },
            transactions: vec![tx_with_output(50_000)].into_boxed_slice(),
        };
        let hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_block(&block).unwrap();
        storage.blocks().store_header(&hash, &block.header).unwrap();
        storage.blocks().store_height(height, &hash).unwrap();
        storage
            .blocks()
            .store_undo_log(&hash, &BlockUndoLog::new())
            .unwrap();
        storage
            .chain()
            .update_tip(&hash, &block.header, height)
            .unwrap();
        if height == 1 {
            child_hash = hash;
        }
        prev = hash;
    }

    BlockchainRpc::with_dependencies(Arc::clone(&storage))
        .invalidate_block(&json!([crate::storage::hashing::hash_to_rpc_hex(
            &child_hash
        )]))
        .await
        .unwrap();

    assert_eq!(
        storage.chain().get_tip_hash().unwrap().unwrap(),
        genesis_hash
    );
    assert_eq!(storage.chain().get_height().unwrap().unwrap(), 0);
}

fn put_snapshot_coin(storage: &Storage, prevout: &OutPoint, utxo: &blvm_protocol::UTXO) {
    let tree_name = crate::storage::ibd_engine::ckpt_tree_for_slot(0);
    let tree = storage.open_tree(tree_name).unwrap();
    let key = crate::storage::disk_utxo::outpoint_to_key(prevout);
    let bytes =
        crate::storage::utxo_value_codec::encode_utxo_with_codec(storage.utxo_value_codec(), utxo)
            .unwrap();
    tree.insert(&key, &bytes).unwrap();
}

#[tokio::test]
async fn loadtxoutset_replaces_a_checkpoint_with_the_loaded_coins() {
    use blvm_protocol::UTXO;
    use std::sync::Arc;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    storage
        .chain()
        .initialize(&BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_000,
            bits: 0,
            nonce: 1,
        })
        .unwrap();
    storage.chain().force_set_ibd_utxo_watermark(50).unwrap();
    storage.chain().force_set_engine_export_height(50).unwrap();

    let stale = OutPoint {
        hash: [4u8; 32],
        index: 0,
    };
    let loaded = OutPoint {
        hash: [8u8; 32],
        index: 0,
    };
    let stale_utxo = UTXO {
        value: 1,
        script_pubkey: vec![OP_1].into(),
        height: 1,
        is_coinbase: false,
    };
    let loaded_utxo = UTXO {
        value: 10_000,
        script_pubkey: vec![OP_1].into(),
        height: 3,
        is_coinbase: false,
    };
    put_snapshot_coin(&storage, &stale, &stale_utxo);
    put_snapshot_coin_in(&storage, "ibd_utxos_ckpt_b", &stale, &stale_utxo);

    let snap_dir = tempfile::TempDir::new().unwrap();
    let mut file_set = blvm_protocol::UtxoSet::default();
    file_set.insert(loaded, Arc::new(loaded_utxo));
    let snap_header = BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1_600_000_003,
        bits: 0,
        nonce: 3,
    };
    let block_hash = blvm_consensus::block::block_header_hash(&snap_header);
    storage
        .blocks()
        .store_header(&block_hash, &snap_header)
        .unwrap();
    storage.blocks().store_height(3, &block_hash).unwrap();
    AssumeUtxoManager::new(snap_dir.path())
        .create_snapshot(&file_set, block_hash, 3)
        .unwrap();
    let path = snap_dir.path().join("utxo_snapshot_3.dat");

    BlockchainRpc::with_dependencies(Arc::clone(&storage))
        .load_txout_set(&json!([path.to_str().unwrap()]))
        .await
        .unwrap();

    assert!(storage.open_engine_resume_snapshot().unwrap().is_none());
    assert!(storage.read_chain_prevout(&stale).unwrap().is_none());
    assert_eq!(
        storage.read_chain_prevout(&loaded).unwrap().unwrap().value,
        10_000
    );
    assert_eq!(storage.chain().get_height().unwrap(), Some(3));
    assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(3));
    assert_eq!(storage.chain().get_engine_export_height().unwrap(), Some(0));
}

fn put_snapshot_coin_in(
    storage: &Storage,
    tree: &str,
    prevout: &OutPoint,
    utxo: &blvm_protocol::UTXO,
) {
    let tree = storage.open_tree(tree).unwrap();
    let key = crate::storage::disk_utxo::outpoint_to_key(prevout);
    let bytes =
        crate::storage::utxo_value_codec::encode_utxo_with_codec(storage.utxo_value_codec(), utxo)
            .unwrap();
    tree.insert(&key, &bytes).unwrap();
}

#[tokio::test]
async fn invalidate_undoes_the_snapshot_and_does_not_rewrite_the_chainstate_table() {
    use blvm_consensus::reorganization::{BlockUndoLog, UndoEntry};
    use blvm_protocol::UTXO;
    use std::sync::Arc;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let parent = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1_600_000_000,
        bits: 0,
        nonce: 1,
    };
    storage.chain().initialize(&parent).unwrap();
    let parent_hash = storage.chain().get_tip_hash().unwrap().unwrap();
    storage
        .blocks()
        .store_header(&parent_hash, &parent)
        .unwrap();
    let created = OutPoint {
        hash: [4u8; 32],
        index: 0,
    };
    let spent = OutPoint {
        hash: [5u8; 32],
        index: 0,
    };
    let untouched = OutPoint {
        hash: [6u8; 32],
        index: 0,
    };
    let created_utxo = UTXO {
        value: 50_000,
        script_pubkey: vec![OP_1].into(),
        height: 1,
        is_coinbase: true,
    };
    let spent_utxo = UTXO {
        value: 25_000,
        script_pubkey: vec![OP_1].into(),
        height: 0,
        is_coinbase: false,
    };
    put_snapshot_coin(&storage, &created, &created_utxo);
    storage.utxos().add_utxo(&untouched, &spent_utxo).unwrap();

    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_001,
            bits: 0,
            nonce: 2,
        },
        transactions: vec![tx_with_output(50_000)].into_boxed_slice(),
    };
    let hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage.blocks().store_header(&hash, &block.header).unwrap();
    storage.blocks().store_height(1, &hash).unwrap();
    let mut undo = BlockUndoLog::new();
    undo.push(UndoEntry {
        outpoint: created,
        previous_utxo: None,
        new_utxo: Some(Arc::new(created_utxo)),
    });
    undo.push(UndoEntry {
        outpoint: spent,
        previous_utxo: Some(Arc::new(spent_utxo.clone())),
        new_utxo: None,
    });
    storage.blocks().store_undo_log(&hash, &undo).unwrap();
    storage.chain().update_tip(&hash, &block.header, 1).unwrap();
    storage.chain().force_set_ibd_utxo_watermark(1).unwrap();
    storage.chain().force_set_engine_export_height(1).unwrap();

    BlockchainRpc::with_dependencies(Arc::clone(&storage))
        .invalidate_block(&json!([crate::storage::hashing::hash_to_rpc_hex(&hash)]))
        .await
        .unwrap();

    assert_eq!(
        storage.chain().get_tip_hash().unwrap().unwrap(),
        parent_hash
    );
    assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(0));
    assert_eq!(storage.chain().get_engine_export_height().unwrap(), Some(0));
    assert!(storage.read_chain_prevout(&created).unwrap().is_none());
    let restored = storage.read_chain_prevout(&spent).unwrap().unwrap();
    assert_eq!(restored.value, 25_000);
    assert!(storage.utxos().get_utxo(&spent).unwrap().is_none());
    assert!(storage.utxos().get_utxo(&untouched).unwrap().is_some());
}

#[tokio::test]
async fn invalidate_without_a_snapshot_undoes_the_chainstate_table() {
    use blvm_consensus::reorganization::{BlockUndoLog, UndoEntry};
    use blvm_protocol::UTXO;
    use std::sync::Arc;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let parent = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1_600_000_000,
        bits: 0,
        nonce: 1,
    };
    storage.chain().initialize(&parent).unwrap();
    let parent_hash = storage.chain().get_tip_hash().unwrap().unwrap();
    storage
        .blocks()
        .store_header(&parent_hash, &parent)
        .unwrap();
    let created = OutPoint {
        hash: [4u8; 32],
        index: 0,
    };
    let created_utxo = UTXO {
        value: 50_000,
        script_pubkey: vec![OP_1].into(),
        height: 1,
        is_coinbase: true,
    };
    storage.utxos().add_utxo(&created, &created_utxo).unwrap();
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_001,
            bits: 0,
            nonce: 2,
        },
        transactions: vec![tx_with_output(50_000)].into_boxed_slice(),
    };
    let hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage.blocks().store_header(&hash, &block.header).unwrap();
    storage.blocks().store_height(1, &hash).unwrap();
    let mut undo = BlockUndoLog::new();
    undo.push(UndoEntry {
        outpoint: created,
        previous_utxo: None,
        new_utxo: Some(Arc::new(created_utxo)),
    });
    storage.blocks().store_undo_log(&hash, &undo).unwrap();
    storage.chain().update_tip(&hash, &block.header, 1).unwrap();

    BlockchainRpc::with_dependencies(Arc::clone(&storage))
        .invalidate_block(&json!([crate::storage::hashing::hash_to_rpc_hex(&hash)]))
        .await
        .unwrap();

    assert_eq!(
        storage.chain().get_tip_hash().unwrap().unwrap(),
        parent_hash
    );
    assert!(storage.utxos().get_utxo(&created).unwrap().is_none());
}

#[tokio::test]
async fn reconsider_clears_the_mark_and_leaves_the_tip() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let parent = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1_600_000_000,
        bits: 0,
        nonce: 1,
    };
    storage.chain().initialize(&parent).unwrap();
    let parent_hash = storage.chain().get_tip_hash().unwrap().unwrap();
    let child = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [0u8; 32],
            timestamp: 1_600_000_001,
            bits: 0,
            nonce: 2,
        },
        transactions: vec![tx_with_output(50_000)].into_boxed_slice(),
    };
    let child_hash = storage.blocks().get_block_hash(&child);
    storage
        .blocks()
        .store_header(&child_hash, &child.header)
        .unwrap();
    storage.chain().mark_invalid(&child_hash).unwrap();

    BlockchainRpc::with_dependencies(Arc::clone(&storage))
        .reconsider_block(&json!([crate::storage::hashing::hash_to_rpc_hex(
            &child_hash
        )]))
        .await
        .unwrap();

    assert!(!storage.chain().is_invalid(&child_hash).unwrap());
    assert_eq!(
        storage.chain().get_tip_hash().unwrap().unwrap(),
        parent_hash
    );
    assert_eq!(storage.chain().get_height().unwrap(), Some(0));
}

#[tokio::test]
async fn p2wpkh_index_off_errors_and_a_full_spend_reports_both_sides() {
    use blvm_protocol::block::calculate_tx_id;

    let script = {
        let mut s = vec![0x00, 0x14];
        s.extend_from_slice(&[0x11u8; 20]);
        s
    };
    let temp = tempfile::TempDir::new().unwrap();
    let off = Arc::new(Storage::new(temp.path()).unwrap());
    assert!(
        BlockchainRpc::with_dependencies(off)
            .get_address_info(&json!([hex::encode(&script)]))
            .await
            .is_err()
    );

    let temp_on = tempfile::TempDir::new().unwrap();
    let mut indexing = crate::config::IndexingConfig::default();
    indexing.enable_address_index = true;
    let on = Arc::new(
        Storage::with_backend_pruning_and_indexing(
            temp_on.path(),
            crate::storage::database::default_backend(),
            None,
            Some(indexing),
            None,
            None,
            None,
        )
        .unwrap(),
    );
    let tx = tx_with_output(50_000);
    let mut paying = tx;
    paying.outputs[0].script_pubkey = script.clone();
    paying.outputs[0].value = 50_000;
    let txid = calculate_tx_id(&paying);
    on.transactions()
        .index_transaction(&paying, &[2u8; 32], 1, 0)
        .unwrap();
    on.transactions()
        .note_spend(&script, 50_000, &txid)
        .unwrap();
    let info = BlockchainRpc::with_dependencies(on)
        .get_address_info(&json!([hex::encode(&script)]))
        .await
        .unwrap();
    assert_eq!(info["received"].as_f64(), Some(0.0005));
    assert_eq!(info["sent"].as_f64(), Some(0.0005));
    assert_eq!(info["script_type"], "witness_v0_keyhash");
}

#[tokio::test]
async fn a_utxo_read_error_is_not_reported_as_spent() {
    use blvm_protocol::block::calculate_tx_id;

    let script = {
        let mut s = vec![0x00, 0x14];
        s.extend_from_slice(&[0x11u8; 20]);
        s
    };
    let temp = tempfile::TempDir::new().unwrap();
    let mut indexing = crate::config::IndexingConfig::default();
    indexing.enable_address_index = true;
    let storage = Arc::new(
        Storage::with_backend_pruning_and_indexing(
            temp.path(),
            crate::storage::database::default_backend(),
            None,
            Some(indexing),
            None,
            None,
            None,
        )
        .unwrap(),
    );
    let mut paying = tx_with_output(50_000);
    paying.outputs[0].script_pubkey = script.clone();
    paying.outputs[0].value = 50_000;
    let txid = calculate_tx_id(&paying);
    storage
        .transactions()
        .index_transaction(&paying, &[2u8; 32], 1, 0)
        .unwrap();
    storage
        .utxos()
        .insert_undecodable_utxo(&blvm_protocol::OutPoint {
            hash: txid,
            index: 0,
        })
        .unwrap();
    let err = BlockchainRpc::with_dependencies(storage)
        .get_address_info(&json!([hex::encode(&script)]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("utxo read failed"), "{err}");
}

#[tokio::test]
async fn address_balance_counts_a_snapshot_coin_the_table_does_not_hold() {
    use blvm_protocol::block::calculate_tx_id;

    let script = vec![OP_1];
    let temp = tempfile::TempDir::new().unwrap();
    let mut indexing = crate::config::IndexingConfig::default();
    indexing.enable_address_index = true;
    let storage = Arc::new(
        Storage::with_backend_pruning_and_indexing(
            temp.path(),
            crate::storage::database::default_backend(),
            None,
            Some(indexing),
            None,
            None,
            None,
        )
        .unwrap(),
    );
    let mut paying = tx_with_output(50_000);
    paying.outputs[0].script_pubkey = script.clone();
    paying.outputs[0].value = 50_000;
    let txid = calculate_tx_id(&paying);
    storage
        .transactions()
        .index_transaction(&paying, &[2u8; 32], 1, 0)
        .unwrap();
    let prevout = blvm_protocol::OutPoint {
        hash: txid,
        index: 0,
    };
    let tree = storage.open_tree("ibd_utxos_ckpt_a").unwrap();
    let utxo = blvm_protocol::UTXO {
        value: 50_000,
        script_pubkey: script.clone().into(),
        height: 1,
        is_coinbase: false,
    };
    let key = crate::storage::disk_utxo::outpoint_to_key(&prevout);
    let bytes =
        crate::storage::utxo_value_codec::encode_utxo_with_codec(storage.utxo_value_codec(), &utxo)
            .unwrap();
    tree.insert(&key, &bytes).unwrap();

    let rpc = BlockchainRpc::with_dependencies(Arc::clone(&storage));
    let balance = rpc
        .getaddressbalance(&json!([hex::encode(&script)]))
        .await
        .unwrap();
    assert_eq!(balance["balance"], 50_000, "{balance}");
    assert_eq!(balance["sent"], 0, "{balance}");
    let info = rpc
        .get_address_info(&json!([hex::encode(&script)]))
        .await
        .unwrap();
    assert_eq!(info["balance"].as_f64(), Some(0.0005), "{info}");
    assert_eq!(info["utxo_count"], 1, "{info}");
}

#[tokio::test]
async fn signet_stays_unknown_and_segwit_follows_its_activation_height() {
    use crate::storage::chainstate::ChainParams;
    use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Signet).unwrap());
    let header = protocol.get_network_params().genesis_block.header.clone();
    let mut params = ChainParams::default();
    params.network = "signet".to_string();
    storage
        .chain()
        .initialize_with_params(&header, params)
        .unwrap();
    let genesis_hash = storage.chain().get_tip_hash().unwrap().unwrap();
    storage.blocks().store_height(0, &genesis_hash).unwrap();
    let rpc =
        BlockchainRpc::with_dependencies_and_protocol(Arc::clone(&storage), Arc::clone(&protocol));
    let info = rpc.get_blockchain_info().await.unwrap();
    assert_eq!(info["chain"], "signet");
    assert_eq!(info["initialblockdownload"], false);
    assert_eq!(info["verificationprogress"].as_f64(), Some(1.0));
    assert_eq!(info["softforks"]["segwit"]["active"], false);
    assert_eq!(info["softforks"]["segwit"]["height"], 1);

    let tip = storage.chain().get_tip_hash().unwrap().unwrap();
    let mut child = header.clone();
    child.prev_block_hash = tip;
    storage.chain().update_tip(&tip, &child, 1).unwrap();
    let activated = storage.chain().get_tip_hash().unwrap().unwrap();
    storage.blocks().store_height(1, &activated).unwrap();
    storage.blocks().store_height(2, &[0x22; 32]).unwrap();
    let at_activation = rpc.get_blockchain_info().await.unwrap();
    assert_eq!(at_activation["chain"], "signet");
    assert_eq!(at_activation["initialblockdownload"], true);
    assert_eq!(at_activation["softforks"]["segwit"]["active"], true);

    let bare = tempfile::TempDir::new().unwrap();
    let bare_storage = Arc::new(Storage::new(bare.path()).unwrap());
    let testnet4 = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Testnet4).unwrap());
    let named = BlockchainRpc::with_dependencies_and_protocol(bare_storage, testnet4)
        .get_blockchain_info()
        .await
        .unwrap();
    assert_eq!(named["chain"], "testnet4");
    assert_eq!(named["initialblockdownload"], true);
}

#[tokio::test]
async fn blockchain_info_progress_is_chain_over_stored_headers() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let header = blvm_protocol::BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1_231_006_505,
        bits: 0x1d00ffff,
        nonce: 0,
    };
    storage.chain().initialize(&header).unwrap();
    let mut prev = storage.chain().get_tip_hash().unwrap().unwrap();
    for height in 1..=3 {
        let mut child = header.clone();
        child.prev_block_hash = prev;
        child.nonce = height;
        storage.chain().update_tip(&prev, &child, height).unwrap();
        prev = storage.chain().get_tip_hash().unwrap().unwrap();
    }
    for height in 0..=5 {
        storage
            .blocks()
            .store_height(height, &[height as u8; 32])
            .unwrap();
    }
    let info = BlockchainRpc::with_dependencies(Arc::clone(&storage))
        .get_blockchain_info()
        .await
        .unwrap();
    assert_eq!(info["blocks"], 3);
    assert_eq!(info["headers"], 5);
    assert_eq!(info["initialblockdownload"], true);
    assert_eq!(info["verificationprogress"].as_f64(), Some(3.0 / 5.0));

    let state = BlockchainRpc::with_dependencies(storage)
        .get_blockchain_state()
        .await
        .unwrap();
    assert_eq!(state["initialblockdownload"], true);
    assert_eq!(state["verificationprogress"].as_f64(), Some(3.0 / 5.0));
}
