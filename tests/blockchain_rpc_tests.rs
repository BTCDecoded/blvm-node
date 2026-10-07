//! Tests for BlockchainRpc methods

use blvm_node::rpc::blockchain::BlockchainRpc;
use blvm_node::storage::Storage;
use blvm_protocol::BitcoinProtocolEngine;
use blvm_protocol::ProtocolVersion;
use serde_json::Value;
use std::sync::Arc;
use tempfile::TempDir;

fn chain_from_state(v: &Value) -> String {
    v["chain"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn test_get_chain_name_with_protocol() {
    // `getblockchainstate` includes `chain` from the same logic as internal get_chain_name.
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());

    let mainnet_protocol =
        Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::BitcoinV1).unwrap());
    let rpc_mainnet =
        BlockchainRpc::with_dependencies_and_protocol(storage.clone(), mainnet_protocol);
    let info = rpc_mainnet.get_blockchain_state().await.unwrap();
    assert_eq!(chain_from_state(&info), "mainnet");

    let testnet_protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Testnet3).unwrap());
    let rpc_testnet =
        BlockchainRpc::with_dependencies_and_protocol(storage.clone(), testnet_protocol);
    let info = rpc_testnet.get_blockchain_state().await.unwrap();
    assert_eq!(chain_from_state(&info), "testnet");

    let regtest_protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let rpc_regtest =
        BlockchainRpc::with_dependencies_and_protocol(storage.clone(), regtest_protocol);
    let info = rpc_regtest.get_blockchain_state().await.unwrap();
    assert_eq!(chain_from_state(&info), "regtest");
}

#[tokio::test]
async fn test_get_chain_name_without_protocol() {
    let rpc = BlockchainRpc::new();
    let info = rpc.get_blockchain_state().await.unwrap();
    assert_eq!(chain_from_state(&info), "regtest");

    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let rpc_with_storage = BlockchainRpc::with_dependencies(storage);
    let info = rpc_with_storage.get_blockchain_state().await.unwrap();
    assert_eq!(chain_from_state(&info), "regtest");
}

/// Test that verifychain returns true on a chain with non-coinbase spends.
/// This is the key regression test for GitHub issue #10.
///
/// Prior to the fix, verifychain would attempt full UTXO validation at all check levels,
/// which would fail on blocks with spends because those blocks were already applied to
/// the tip UTXO set.
#[tokio::test]
async fn test_verify_chain_with_spends_returns_true() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());

    // Create genesis block with a coinbase transaction that has a spendable output
    let coinbase_script: blvm_node::ByteString = vec![0x51].into(); // OP_1
    let coinbase_output_value = 50_000_000_00i64; // 50 BTC in satoshis

    let genesis_coinbase = blvm_protocol::Transaction {
        version: 1,
        inputs: blvm_protocol::tx_inputs![blvm_protocol::TransactionInput {
            prevout: blvm_node::OutPoint {
                hash: [0u8; 32],
                index: 0xffffffff,
            },
            script_sig: vec![0x00, 0xff], // BIP34 height 0: OP_0 + padding
            sequence: 0xffffffff,
        }],
        outputs: blvm_protocol::tx_outputs![blvm_protocol::TransactionOutput {
            value: coinbase_output_value,
            script_pubkey: coinbase_script.clone().into(),
        }],
        lock_time: 0,
    };

    let genesis_header = blvm_protocol::BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: blvm_protocol::mining::calculate_merkle_root(&[genesis_coinbase.clone()])
            .unwrap(),
        timestamp: 1_231_006_505,
        bits: 0x0f00ffff,
        nonce: 1,
    };

    let genesis_block = blvm_node::Block {
        header: genesis_header.clone(),
        transactions: vec![genesis_coinbase.clone()].into_boxed_slice(),
    };

    let genesis_hash = storage.blocks().get_block_hash(&genesis_block);
    storage.blocks().store_block(&genesis_block).unwrap();
    storage.blocks().store_height(0, &genesis_hash).unwrap();
    storage.chain().initialize(&genesis_header).unwrap();

    // Add the coinbase UTXO to the UTXO set
    let coinbase_txid = blvm_protocol::block::calculate_tx_id(&genesis_coinbase);
    let coinbase_outpoint = blvm_node::OutPoint {
        hash: coinbase_txid,
        index: 0,
    };
    storage
        .utxos()
        .add_utxo(
            &coinbase_outpoint,
            &blvm_node::UTXO {
                value: coinbase_output_value,
                script_pubkey: coinbase_script.clone().into(),
                height: 0,
                is_coinbase: true,
            },
        )
        .unwrap();

    // Create block 1 with a coinbase and a transaction spending the genesis coinbase
    let block1_coinbase = blvm_protocol::Transaction {
        version: 1,
        inputs: blvm_protocol::tx_inputs![blvm_protocol::TransactionInput {
            prevout: blvm_node::OutPoint {
                hash: [0u8; 32],
                index: 0xffffffff,
            },
            script_sig: vec![0x51, 0xff], // OP_1, BIP34 height 1
            sequence: 0xffffffff,
        }],
        outputs: blvm_protocol::tx_outputs![blvm_protocol::TransactionOutput {
            value: coinbase_output_value,
            script_pubkey: coinbase_script.clone().into(),
        }],
        lock_time: 0,
    };

    // Transaction spending the genesis coinbase output
    let spend_tx = blvm_protocol::Transaction {
        version: 1,
        inputs: blvm_protocol::tx_inputs![blvm_protocol::TransactionInput {
            prevout: coinbase_outpoint,
            script_sig: vec![0x51],
            sequence: 0xffffffff,
        }],
        outputs: blvm_protocol::tx_outputs![blvm_protocol::TransactionOutput {
            value: coinbase_output_value - 1000,
            script_pubkey: coinbase_script.clone().into(),
        }],
        lock_time: 0,
    };

    let block1_txs = vec![block1_coinbase.clone(), spend_tx.clone()];
    let block1_merkle = blvm_protocol::mining::calculate_merkle_root(&block1_txs).unwrap();

    let block1_header = blvm_protocol::BlockHeader {
        version: 1,
        prev_block_hash: genesis_hash,
        merkle_root: block1_merkle,
        timestamp: 1_231_006_605,
        bits: 0x0f00ffff,
        nonce: 2,
    };

    let block1 = blvm_node::Block {
        header: block1_header.clone(),
        transactions: block1_txs.into_boxed_slice(),
    };

    let block1_hash = storage.blocks().get_block_hash(&block1);
    storage.blocks().store_block(&block1).unwrap();
    storage.blocks().store_height(1, &block1_hash).unwrap();
    storage
        .chain()
        .update_tip(&block1_hash, &block1_header, 1)
        .unwrap();

    // Update UTXO set: remove spent coinbase, add new outputs
    storage.utxos().remove_utxo(&coinbase_outpoint).unwrap();

    // Add block1 coinbase output
    let block1_coinbase_txid = blvm_protocol::block::calculate_tx_id(&block1_coinbase);
    storage
        .utxos()
        .add_utxo(
            &blvm_node::OutPoint {
                hash: block1_coinbase_txid,
                index: 0,
            },
            &blvm_node::UTXO {
                value: coinbase_output_value,
                script_pubkey: coinbase_script.clone().into(),
                height: 1,
                is_coinbase: true,
            },
        )
        .unwrap();

    // Add spend_tx output
    let spend_txid = blvm_protocol::block::calculate_tx_id(&spend_tx);
    storage
        .utxos()
        .add_utxo(
            &blvm_node::OutPoint {
                hash: spend_txid,
                index: 0,
            },
            &blvm_node::UTXO {
                value: coinbase_output_value - 1000,
                script_pubkey: coinbase_script.into(),
                height: 1,
                is_coinbase: false,
            },
        )
        .unwrap();

    // Test verifychain at different levels
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let rpc = BlockchainRpc::with_dependencies_and_protocol(storage, protocol);

    // Level 1: basic checks only
    let result_level1 = rpc.verify_chain(Some(1), Some(10)).await.unwrap();
    assert!(
        result_level1.as_bool() == Some(true),
        "verifychain level 1 should return true for chain with spends, got: {result_level1}"
    );

    // Level 2: merkle root check
    let result_level2 = rpc.verify_chain(Some(2), Some(10)).await.unwrap();
    assert!(
        result_level2.as_bool() == Some(true),
        "verifychain level 2 should return true for chain with spends, got: {result_level2}"
    );

    // Level 3: header linkage check (default level)
    let result_level3 = rpc.verify_chain(Some(3), Some(10)).await.unwrap();
    assert!(
        result_level3.as_bool() == Some(true),
        "verifychain level 3 should return true for chain with spends, got: {result_level3}"
    );

    // Default (no level specified, should use level 3)
    let result_default = rpc.verify_chain(None, None).await.unwrap();
    assert!(
        result_default.as_bool() == Some(true),
        "verifychain default should return true for chain with spends, got: {result_default}"
    );
}

/// Test that verifychain level 4 works correctly with undo logs.
/// Level 4 requires undo logs to rewind the UTXO set before replaying blocks.
/// This test uses coinbase-only blocks to verify undo log rewind mechanics
/// without hitting coinbase maturity constraints (which require 100 blocks).
#[tokio::test]
#[cfg(feature = "production")]
async fn test_verify_chain_level4_with_undo_logs() {
    use blvm_consensus::reorganization::{BlockUndoLog, UndoEntry};
    use std::sync::Arc as StdArc;

    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());

    // Create genesis block with a coinbase transaction
    let coinbase_script: blvm_node::ByteString = vec![0x51].into(); // OP_1
    let coinbase_output_value = 50_000_000_00i64;

    let genesis_coinbase = blvm_protocol::Transaction {
        version: 1,
        inputs: blvm_protocol::tx_inputs![blvm_protocol::TransactionInput {
            prevout: blvm_node::OutPoint {
                hash: [0u8; 32],
                index: 0xffffffff,
            },
            script_sig: vec![0x00, 0xff], // BIP34 height 0: OP_0 + padding
            sequence: 0xffffffff,
        }],
        outputs: blvm_protocol::tx_outputs![blvm_protocol::TransactionOutput {
            value: coinbase_output_value,
            script_pubkey: coinbase_script.clone().into(),
        }],
        lock_time: 0,
    };

    let genesis_header = blvm_protocol::BlockHeader {
        version: 0x20000000, // BIP9 version bits (required for regtest consensus)
        prev_block_hash: [0u8; 32],
        merkle_root: blvm_protocol::mining::calculate_merkle_root(&[genesis_coinbase.clone()])
            .unwrap(),
        timestamp: 1_231_006_505,
        bits: 0x0f00ffff,
        nonce: 1,
    };

    let genesis_block = blvm_node::Block {
        header: genesis_header.clone(),
        transactions: vec![genesis_coinbase.clone()].into_boxed_slice(),
    };

    let genesis_hash = storage.blocks().get_block_hash(&genesis_block);
    storage.blocks().store_block(&genesis_block).unwrap();
    storage.blocks().store_height(0, &genesis_hash).unwrap();
    storage.chain().initialize(&genesis_header).unwrap();

    // Store genesis header for MTP
    storage
        .blocks()
        .store_recent_header(0, &genesis_header)
        .unwrap();

    // Store empty witnesses for genesis block (required for regtest where segwit is active)
    let genesis_witnesses: Vec<Vec<blvm_protocol::segwit::Witness>> = genesis_block
        .transactions
        .iter()
        .map(|tx| tx.inputs.iter().map(|_| Vec::new()).collect())
        .collect();
    storage
        .blocks()
        .store_witness(&genesis_hash, &genesis_witnesses)
        .unwrap();

    // Add the coinbase UTXO to the UTXO set
    let coinbase_txid = blvm_protocol::block::calculate_tx_id(&genesis_coinbase);
    let coinbase_outpoint = blvm_node::OutPoint {
        hash: coinbase_txid,
        index: 0,
    };
    let genesis_coinbase_utxo = blvm_node::UTXO {
        value: coinbase_output_value,
        script_pubkey: coinbase_script.clone().into(),
        height: 0,
        is_coinbase: true,
    };
    storage
        .utxos()
        .add_utxo(&coinbase_outpoint, &genesis_coinbase_utxo)
        .unwrap();

    // Create undo log for genesis block (coinbase output created)
    let genesis_undo = BlockUndoLog {
        entries: vec![UndoEntry {
            outpoint: coinbase_outpoint,
            previous_utxo: None, // No previous UTXO (this is a creation)
            new_utxo: Some(StdArc::new(genesis_coinbase_utxo.clone())),
        }],
    };
    storage
        .blocks()
        .store_undo_log(&genesis_hash, &genesis_undo)
        .unwrap();

    // Create block 1 with just a coinbase (no spends due to coinbase maturity)
    let block1_coinbase = blvm_protocol::Transaction {
        version: 1,
        inputs: blvm_protocol::tx_inputs![blvm_protocol::TransactionInput {
            prevout: blvm_node::OutPoint {
                hash: [0u8; 32],
                index: 0xffffffff,
            },
            script_sig: vec![0x51, 0xff], // OP_1, BIP34 height 1
            sequence: 0xffffffff,
        }],
        outputs: blvm_protocol::tx_outputs![blvm_protocol::TransactionOutput {
            value: coinbase_output_value,
            script_pubkey: coinbase_script.clone().into(),
        }],
        lock_time: 0,
    };

    let block1_txs = vec![block1_coinbase.clone()];
    let block1_merkle = blvm_protocol::mining::calculate_merkle_root(&block1_txs).unwrap();

    let block1_header = blvm_protocol::BlockHeader {
        version: 0x20000000, // BIP9 version bits (required for regtest consensus)
        prev_block_hash: genesis_hash,
        merkle_root: block1_merkle,
        timestamp: 1_231_006_605,
        bits: 0x0f00ffff,
        nonce: 2,
    };

    let block1 = blvm_node::Block {
        header: block1_header.clone(),
        transactions: block1_txs.into_boxed_slice(),
    };

    let block1_hash = storage.blocks().get_block_hash(&block1);
    storage.blocks().store_block(&block1).unwrap();
    storage.blocks().store_height(1, &block1_hash).unwrap();
    storage
        .blocks()
        .store_recent_header(1, &block1_header)
        .unwrap();
    storage
        .chain()
        .update_tip(&block1_hash, &block1_header, 1)
        .unwrap();

    // Store empty witnesses for block 1 (required for regtest where segwit is active)
    let block1_witnesses: Vec<Vec<blvm_protocol::segwit::Witness>> = block1
        .transactions
        .iter()
        .map(|tx| tx.inputs.iter().map(|_| Vec::new()).collect())
        .collect();
    storage
        .blocks()
        .store_witness(&block1_hash, &block1_witnesses)
        .unwrap();

    // Add block1 coinbase output to UTXO set
    let block1_coinbase_txid = blvm_protocol::block::calculate_tx_id(&block1_coinbase);
    let block1_coinbase_outpoint = blvm_node::OutPoint {
        hash: block1_coinbase_txid,
        index: 0,
    };
    let block1_coinbase_utxo = blvm_node::UTXO {
        value: coinbase_output_value,
        script_pubkey: coinbase_script.into(),
        height: 1,
        is_coinbase: true,
    };
    storage
        .utxos()
        .add_utxo(&block1_coinbase_outpoint, &block1_coinbase_utxo)
        .unwrap();

    // Create undo log for block 1 (only coinbase output created)
    let block1_undo = BlockUndoLog {
        entries: vec![UndoEntry {
            outpoint: block1_coinbase_outpoint,
            previous_utxo: None,
            new_utxo: Some(StdArc::new(block1_coinbase_utxo)),
        }],
    };
    storage
        .blocks()
        .store_undo_log(&block1_hash, &block1_undo)
        .unwrap();

    // Test verifychain at level 4
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let rpc = BlockchainRpc::with_dependencies_and_protocol(storage, protocol);

    // Level 4: Full UTXO validation with undo log rewind
    // This tests that:
    // 1. Undo logs are correctly loaded and applied (rewind)
    // 2. Blocks are correctly replayed (validation + UTXO creation)
    let result_level4 = rpc.verify_chain(Some(4), Some(10)).await.unwrap();
    assert!(
        result_level4.as_bool() == Some(true),
        "verifychain level 4 should return true for valid chain with undo logs, got: {result_level4}"
    );
}
