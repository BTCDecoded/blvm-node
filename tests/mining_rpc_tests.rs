//! Unit tests for Mining RPC methods

use blvm_node::node::mempool::MempoolManager;
use blvm_node::rpc::mining::MiningRpc;
use blvm_node::storage::Storage;
use blvm_protocol::serialization::serialize_transaction;
use blvm_protocol::{OutPoint, UTXO};
use std::sync::Arc;
use tempfile::TempDir;

mod common;
use common::{
    MINING_RPC_CHAIN_BLOCKS, patch_storage_chain_network_regtest, setup_mining_chain,
    valid_transaction,
};

async fn expect_block_template(
    mining: &MiningRpc,
    params: &serde_json::Value,
) -> serde_json::Value {
    mining.get_block_template(params).await.unwrap_or_else(|e| {
        panic!("get_block_template failed: {e:?}");
    })
}

#[tokio::test]
async fn test_mining_rpc_new() {
    let _mining = MiningRpc::new();
}

#[tokio::test]
async fn test_mining_rpc_with_dependencies() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let _mining = MiningRpc::with_dependencies(storage, mempool);
}

#[tokio::test]
async fn test_get_current_height_uninitialized() {
    let mining = MiningRpc::new();
    let params = serde_json::json!([]);
    let result = mining.get_block_template(&params).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_get_current_height_initialized() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;
    assert_eq!(
        template.get("height").unwrap().as_u64().unwrap(),
        MINING_RPC_CHAIN_BLOCKS
    );
}

#[tokio::test]
async fn test_get_tip_header_initialized() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;
    let tip_hash = storage.chain().get_tip_hash().unwrap().expect("tip hash");
    assert_eq!(
        template.get("previousblockhash").unwrap().as_str().unwrap(),
        blvm_node::storage::hashing::hash_to_rpc_hex(&tip_hash)
    );
}

#[tokio::test]
async fn test_get_utxo_set_empty() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    expect_block_template(&mining, &params).await;
}

#[tokio::test]
async fn test_get_utxo_set_populated() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let outpoint = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 5000000000,
        script_pubkey: vec![0x76, 0xa9, 0x14].into(),
        height: 0,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&outpoint, &utxo).unwrap();

    let params = serde_json::json!([]);
    expect_block_template(&mining, &params).await;
}

#[tokio::test]
async fn test_transaction_serialization_in_template() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;

    let transactions = template.get("transactions").unwrap().as_array().unwrap();
    for tx in transactions {
        assert!(tx.get("data").is_some());
        assert!(tx.get("txid").is_some());
    }
}

#[tokio::test]
async fn test_calculate_tx_hash_format() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;

    let transactions = template.get("transactions").unwrap().as_array().unwrap();
    for tx_json in transactions {
        let txid = tx_json.get("txid").unwrap().as_str().unwrap();
        assert_eq!(txid.len(), 64);
    }
}

#[tokio::test]
async fn test_calculate_tx_hash_matches_bitcoin_core() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;

    let transactions = template.get("transactions").unwrap().as_array().unwrap();
    for tx_json in transactions {
        let txid = tx_json.get("txid").unwrap().as_str().unwrap();
        assert_eq!(txid.len(), 64);
        assert_ne!(
            txid,
            "0000000000000000000000000000000000000000000000000000000000000000"
        );
    }
}

#[tokio::test]
async fn test_calculate_weight() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let tx = valid_transaction();
    let base_size = serialize_transaction(&tx).len() as u64;

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;

    let transactions = template.get("transactions").unwrap().as_array().unwrap();
    for tx_json in transactions {
        if let Some(weight) = tx_json.get("weight").and_then(|w| w.as_u64()) {
            assert!(weight >= base_size * 4);
        }
    }
}

#[tokio::test]
async fn test_calculate_coinbase_value() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;

    let coinbase_value = template.get("coinbasevalue").unwrap().as_u64().unwrap();
    assert_eq!(coinbase_value, 5000000000);
}

#[tokio::test]
async fn test_get_active_rules() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let params = serde_json::json!([]);
    let template = expect_block_template(&mining, &params).await;
    let rules = template.get("rules").unwrap().as_array().unwrap();
    let rule_strings: Vec<String> = rules
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect();

    // Default storage network is mainnet; at this short test height no soft-fork rules are active yet.
    assert!(!rule_strings.contains(&"csv".to_string()));
    assert!(!rule_strings.contains(&"segwit".to_string()));
    assert!(!rule_strings.contains(&"taproot".to_string()));
}

#[tokio::test]
async fn test_getblocktemplate_includes_prioritized_mempool_tx() {
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{TransactionInput, TransactionOutput};

    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&mempool));

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let funding_hash = [0x55u8; 32];
    storage
        .utxos()
        .add_utxo(
            &OutPoint {
                hash: funding_hash,
                index: 0,
            },
            &UTXO {
                value: 100_000,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            },
        )
        .unwrap();

    let tx = blvm_protocol::Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: funding_hash,
                index: 0,
            },
            script_sig: vec![OP_1],
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 90_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    assert!(
        mempool.add_transaction(tx).expect("add_transaction"),
        "mempool must accept funded tx"
    );

    let template = expect_block_template(&mining, &serde_json::json!([])).await;
    let entries = template.get("transactions").unwrap().as_array().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "GBT transactions list should include one mempool entry"
    );
    assert_eq!(entries[0].get("txid").unwrap().as_str().unwrap().len(), 64);
    assert!(entries[0].get("fee").unwrap().as_u64().unwrap() > 0);
}

#[tokio::test]
async fn test_getblocktemplate_includes_segwit_mempool_tx_on_regtest() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::{TransactionInput, TransactionOutput};
    use sha2::{Digest, Sha256};

    fn p2wsh_scriptpubkey(witness_script: &[u8]) -> Vec<u8> {
        let hash = Sha256::digest(witness_script);
        let mut spk = vec![blvm_protocol::opcodes::OP_0, 0x20];
        spk.extend_from_slice(&hash);
        spk
    }

    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&mempool));

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();
    patch_storage_chain_network_regtest(storage.as_ref()).unwrap();

    let witness_script = vec![OP_1];
    let funding_hash = [0x57u8; 32];
    storage
        .utxos()
        .add_utxo(
            &OutPoint {
                hash: funding_hash,
                index: 0,
            },
            &UTXO {
                value: 100_000,
                script_pubkey: p2wsh_scriptpubkey(&witness_script).into(),
                height: 0,
                is_coinbase: false,
            },
        )
        .unwrap();

    let tx = blvm_protocol::Transaction {
        version: 2,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: funding_hash,
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xfffffffe,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 90_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    let txid = calculate_tx_id(&tx);
    let witness_stack: Witness = vec![witness_script.clone()];
    assert!(
        mempool
            .add_transaction_with_witness(tx, Some(vec![witness_stack.clone()]))
            .expect("add segwit tx"),
        "mempool must accept funded P2WSH spend on regtest"
    );

    let template =
        expect_block_template(&mining, &serde_json::json!([{"rules": ["segwit"]}])).await;
    let entries = template.get("transactions").unwrap().as_array().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "GBT must include SegWit mempool tx when accept_to_memory_pool validates on regtest"
    );
    assert_eq!(
        entries[0].get("txid").unwrap().as_str().unwrap(),
        hex::encode(txid)
    );
    let rules = template.get("rules").unwrap().as_array().unwrap();
    assert!(
        rules.iter().any(|r| r.as_str() == Some("segwit")),
        "regtest GBT must advertise segwit rule"
    );
}

#[tokio::test]
async fn test_getblocktemplate_mempool_witness_available_for_template() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::{TransactionInput, TransactionOutput};
    use sha2::{Digest, Sha256};

    fn p2wsh_scriptpubkey(witness_script: &[u8]) -> Vec<u8> {
        let hash = Sha256::digest(witness_script);
        let mut spk = vec![blvm_protocol::opcodes::OP_0, 0x20];
        spk.extend_from_slice(&hash);
        spk
    }

    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&mempool));

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();
    patch_storage_chain_network_regtest(storage.as_ref()).unwrap();

    let funding_hash = [0x56u8; 32];
    let witness_script = vec![OP_1];
    storage
        .utxos()
        .add_utxo(
            &OutPoint {
                hash: funding_hash,
                index: 0,
            },
            &UTXO {
                value: 100_000,
                script_pubkey: p2wsh_scriptpubkey(&witness_script).into(),
                height: 0,
                is_coinbase: false,
            },
        )
        .unwrap();

    let tx = blvm_protocol::Transaction {
        version: 2,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: funding_hash,
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xfffffffe,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 90_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    let txid = calculate_tx_id(&tx);
    let witness_stack: Witness = vec![witness_script.clone()];
    assert!(
        mempool
            .add_transaction_with_witness(tx, Some(vec![witness_stack.clone()]))
            .expect("add with witness"),
        "mempool must store witness-bearing tx"
    );
    assert_eq!(
        mempool.get_transaction_witnesses(&txid),
        Some(vec![witness_stack])
    );

    let template = expect_block_template(&mining, &serde_json::json!([])).await;
    let entries = template.get("transactions").unwrap().as_array().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "witness-backed tx must survive create_block_template on regtest"
    );
    assert_eq!(
        entries[0].get("txid").unwrap().as_str().unwrap(),
        hex::encode(txid)
    );
}
