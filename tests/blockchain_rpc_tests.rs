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
            script_sig: vec![0x04, 0xff, 0xff, 0x00, 0x1d],
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
            script_sig: vec![0x01, 0x01],
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
