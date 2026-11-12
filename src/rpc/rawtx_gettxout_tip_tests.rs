use super::RawTxRpc;
use crate::node::mempool::MempoolManager;
use crate::storage::Storage;
use blvm_protocol::{OutPoint, UTXO};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn failed_tip_read_is_not_a_zero_bestblock() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let hash = [7u8; 32];
    storage
        .utxos()
        .add_utxo(
            &OutPoint { hash, index: 0 },
            &UTXO {
                value: 50_000,
                script_pubkey: vec![0x51].into(),
                height: 100,
                is_coinbase: false,
            },
        )
        .unwrap();
    let rpc = RawTxRpc::with_dependencies(
        Arc::clone(&storage),
        Arc::new(MempoolManager::new()),
        None,
        None,
    );
    let txid = crate::storage::hashing::hash_to_rpc_hex(&hash);
    let err = rpc
        .gettxout(&json!([txid, 0, false]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("tip read failed"), "{err}");
    assert!(!err.contains(&"0".repeat(64)), "{err}");
}

fn snapshot_coin(storage: &Storage, prevout: &OutPoint, utxo: &UTXO) {
    let tree_name = crate::storage::ibd_engine::ckpt_tree_for_slot(0);
    let tree = storage.open_tree(tree_name).unwrap();
    let key = crate::storage::disk_utxo::outpoint_to_key(prevout);
    let bytes =
        crate::storage::utxo_value_codec::encode_utxo_with_codec(storage.utxo_value_codec(), utxo)
            .unwrap();
    tree.insert(&key, &bytes).unwrap();
    assert!(storage.utxos().get_utxo(prevout).unwrap().is_none());
}

#[tokio::test]
async fn gettxout_reads_a_snapshot_coin_the_chainstate_table_does_not_hold() {
    use blvm_protocol::BlockHeader;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let prevout = OutPoint {
        hash: [4u8; 32],
        index: 0,
    };
    snapshot_coin(
        &storage,
        &prevout,
        &UTXO {
            value: 50_000,
            script_pubkey: vec![0x51].into(),
            height: 1,
            is_coinbase: true,
        },
    );
    storage
        .chain()
        .update_tip(
            &[9u8; 32],
            &BlockHeader {
                bits: 0x207f_ffff,
                ..BlockHeader::default()
            },
            0,
        )
        .unwrap();
    let rpc = RawTxRpc::with_dependencies(
        Arc::clone(&storage),
        Arc::new(MempoolManager::new()),
        None,
        None,
    );
    let txid = crate::storage::hashing::hash_to_rpc_hex(&prevout.hash);
    let value = rpc.gettxout(&json!([txid, 0, false])).await.unwrap();
    assert_eq!(value["value"], json!(0.0005), "{value}");
    assert_eq!(value["coinbase"], json!(true), "{value}");
}

#[tokio::test]
async fn sendrawtransaction_and_testmempoolaccept_see_a_snapshot_coin() {
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let prevout = OutPoint {
        hash: [6u8; 32],
        index: 0,
    };
    snapshot_coin(
        &storage,
        &prevout,
        &UTXO {
            value: 2_000_000,
            script_pubkey: vec![OP_1].into(),
            height: 1,
            is_coinbase: false,
        },
    );
    let tx = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout,
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
    let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
    let rpc = RawTxRpc::with_dependencies(
        Arc::clone(&storage),
        Arc::new(MempoolManager::new()),
        None,
        None,
    );
    let err = rpc
        .sendrawtransaction(&json!([hex_tx.clone()]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("BTC/kvB"), "{err}");
    assert!(!err.contains("not found"), "{err}");

    let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
    assert_eq!(accepted[0]["allowed"], json!(true), "{accepted}");
}

#[tokio::test]
async fn testmempoolaccept_rejects_a_script_that_does_not_pass() {
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let prevout = OutPoint {
        hash: [7u8; 32],
        index: 0,
    };
    storage
        .utxos()
        .add_utxo(
            &prevout,
            &UTXO {
                value: 50_000,
                script_pubkey: vec![0x00].into(),
                height: 1,
                is_coinbase: false,
            },
        )
        .unwrap();
    let tx = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout,
            script_sig: Vec::new(),
            sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 10_000,
            script_pubkey: {
                let mut script = vec![0x00, 0x14];
                script.extend_from_slice(&[0u8; 20]);
                script
            },
        }]
        .into(),
        lock_time: 0,
    };
    let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
    let rpc = RawTxRpc::with_dependencies(
        Arc::clone(&storage),
        Arc::new(MempoolManager::new()),
        None,
        None,
    );
    let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
    assert_eq!(accepted[0]["allowed"], json!(false), "{accepted}");
    assert_eq!(
        accepted[0]["reject-reason"],
        json!("failed consensus checks"),
        "{accepted}"
    );
}

#[tokio::test]
async fn a_future_locktime_is_not_acceptable() {
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let script_pubkey = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, lock_time) in [0u64, 200].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 10_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 10_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: vec![0x01, 0x51],
                sequence: 0xfffffffe,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 9_000,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx).unwrap();
    }
    eprintln!(
        "locktime 0 reported={} admitted={}; locktime 200 reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_nonzero_sequence_needs_a_tip() {
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let script_pubkey = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, sequence) in [0u64, 1].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 10_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 10_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 2,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: vec![0x01, 0x51],
                sequence,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 9_000,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx).unwrap();
    }
    eprintln!(
        "sequence 0 reported={} admitted={}; sequence 1 reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn an_immature_coinbase_is_not_acceptable() {
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let script_pubkey = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let tx = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout,
            script_sig: vec![0x01, 0x51],
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 9_000,
            script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
        }]
        .into(),
        lock_time: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, is_coinbase) in [false, true].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 10_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 10_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase,
                }),
            );
            set
        })));
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx.clone()).unwrap();
    }
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn an_output_above_its_input_is_not_acceptable() {
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let script_pubkey = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, output_value) in [9_000i64, 10_001].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 10_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 10_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: output_value,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx).unwrap();
    }
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_fee_under_the_relay_floor_is_not_acceptable() {
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let script_pubkey = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    // This shape is 67 virtual bytes. The 100 sat/kvB floor rounds up to 7 sat.
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, fee) in [7i64, 6].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 10_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 10_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 10_000 - fee,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx).unwrap();
    }
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_nonstandard_output_is_not_acceptable() {
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let script_pubkey = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, output_script) in [vec![0x6a, 0x03, 0x00, 0x00, 0x00], vec![0x51]]
        .into_iter()
        .enumerate()
    {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 10_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 10_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 9_000,
                script_pubkey: output_script,
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx).unwrap();
    }
    eprintln!(
        "op_return reported={} admitted={}; op_1 reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_tiny_transaction_is_not_acceptable() {
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let script_pubkey = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    let mut sizes = [0usize; 2];
    for (index, output_script) in [vec![0x6a, 0x03, 0x00, 0x00, 0x00], vec![0x6a]]
        .into_iter()
        .enumerate()
    {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 10_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 10_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: output_script,
            }]
            .into(),
            lock_time: 0,
        };
        sizes[index] = blvm_protocol::wire::serialize_tx(&tx).unwrap().len();
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx).unwrap();
    }
    eprintln!(
        "script 5 bytes size={} reported={} admitted={}; script 1 byte size={} reported={} admitted={}",
        sizes[0], reported[0], admitted[0], sizes[1], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn an_overweight_transaction_is_not_acceptable() {
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let script_pubkey = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut heavy = vec![0x6a, 0x4e, 0, 0, 0, 0];
    heavy.extend(std::iter::repeat(0u8).take(99_994));
    let data_len = (heavy.len() - 6) as u32;
    heavy[2..6].copy_from_slice(&data_len.to_le_bytes());
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    let mut weights = [0u64; 2];
    for (index, output_script) in [vec![0x6a, 0x03, 0x00, 0x00, 0x00], heavy]
        .into_iter()
        .enumerate()
    {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 20_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 20_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: output_script,
            }]
            .into(),
            lock_time: 0,
        };
        let size = blvm_protocol::wire::serialize_tx(&tx).unwrap().len() as u64;
        weights[index] = size.saturating_mul(4);
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx).unwrap();
    }
    eprintln!(
        "weight {} reported={} admitted={}; weight {} reported={} admitted={}",
        weights[0], reported[0], admitted[0], weights[1], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_nonstandard_spent_script_is_not_acceptable() {
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let p2sh = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, script_pubkey) in [p2sh, vec![0x75, 0x51]].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 10_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 10_000,
                    script_pubkey: script_pubkey.into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 9_000,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx).unwrap();
    }
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_long_scriptsig_is_not_acceptable() {
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    fn pushdata2(len: usize) -> Vec<u8> {
        let mut bytes = vec![0x4d, (len & 0xff) as u8, (len >> 8) as u8];
        bytes.extend(std::iter::repeat(0u8).take(len));
        bytes
    }
    let redeem = vec![0x75, 0x75, 0x75, 0x75, 0x51];
    let mut redeem_push = vec![redeem.len() as u8];
    redeem_push.extend(redeem.iter().copied());
    let hash = crate::storage::hashing::hash160(&redeem);
    let mut script_pubkey = vec![0xa9, 0x14];
    script_pubkey.extend(hash);
    script_pubkey.push(0x87);
    let mut at_limit = pushdata2(520);
    at_limit.extend(pushdata2(520));
    at_limit.extend(pushdata2(520));
    at_limit.push(74);
    at_limit.extend(std::iter::repeat(0u8).take(74));
    at_limit.extend(redeem_push.iter().copied());
    let mut over = pushdata2(520);
    over.extend(pushdata2(520));
    over.extend(pushdata2(520));
    over.push(75);
    over.extend(std::iter::repeat(0u8).take(75));
    over.extend(redeem_push);
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    let mut lengths = [0usize; 2];
    for (index, script_sig) in [at_limit, over].into_iter().enumerate() {
        lengths[index] = script_sig.len();
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 20_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 20_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig,
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx).unwrap();
    }
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_prevout_sum_above_max_money_is_not_acceptable() {
    use blvm_protocol::constants::MAX_MONEY;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let script_pubkey = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let first = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let second = OutPoint {
        hash: [2u8; 32],
        index: 0,
    };
    let half = MAX_MONEY / 2;
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, second_value) in [half, half + 1].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        for (prevout, value) in [(first, half), (second, second_value)] {
            snapshot_coin(
                &storage,
                &prevout,
                &UTXO {
                    value,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                },
            );
        }
        let mempool = Arc::new(MempoolManager::new());
        let mut policy = crate::config::MempoolPolicyConfig::default();
        policy.min_relay_fee_rate = 0;
        mempool.set_policy_config(Some(policy));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            for (prevout, value) in [(first, half), (second, second_value)] {
                set.insert(
                    prevout,
                    Arc::new(UTXO {
                        value,
                        script_pubkey: script_pubkey.clone().into(),
                        height: 0,
                        is_coinbase: false,
                    }),
                );
            }
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![
                TransactionInput {
                    prevout: first,
                    script_sig: vec![0x01, 0x51],
                    sequence: 0xffffffff,
                },
                TransactionInput {
                    prevout: second,
                    script_sig: vec![0x01, 0x51],
                    sequence: 0xffffffff,
                },
            ]
            .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx).unwrap();
    }
    eprintln!(
        "sum at cap reported={} admitted={}; sum over reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_p2sh_redeem_with_sixteen_sigops_is_not_acceptable() {
    use blvm_protocol::opcodes::{OP_0, OP_1, OP_CHECKSIG, OP_ENDIF, OP_IF};
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, sigops) in [15usize, 16].into_iter().enumerate() {
        let mut redeem = vec![OP_0, OP_IF];
        redeem.extend(std::iter::repeat(OP_CHECKSIG).take(sigops));
        redeem.extend_from_slice(&[OP_ENDIF, OP_1]);
        let hash = crate::storage::hashing::hash160(&redeem);
        let mut script_pubkey = vec![0xa9, 0x14];
        script_pubkey.extend(hash);
        script_pubkey.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend(redeem);
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 100_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 100_000,
                    script_pubkey: script_pubkey.into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig,
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx).unwrap();
    }
    eprintln!(
        "15 sigops reported={} admitted={}; 16 sigops reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_p2sh_sigop_cost_of_16000_is_acceptable() {
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    fn sigop_script(checksigs: usize) -> Vec<u8> {
        let mut script = vec![0x00, 0x63];
        script.extend(std::iter::repeat(0xac).take(checksigs));
        script.extend_from_slice(&[0x68, 0x51]);
        script
    }
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, tail) in [10usize, 11].into_iter().enumerate() {
        // 266 * 15 + tail is 4,000 or 4,001 signature operations.
        // Cost is that count times 4: 16,000 is accepted, 16,004 is not.
        let mut counts = vec![15usize; 266];
        counts.push(tail);
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let mut set = blvm_protocol::UtxoSet::default();
        let mut inputs = Vec::new();
        for (i, count) in counts.iter().enumerate() {
            let redeem = sigop_script(*count);
            let hash = crate::storage::hashing::hash160(&redeem);
            let mut script_pubkey = vec![0xa9, 0x14];
            script_pubkey.extend(hash);
            script_pubkey.push(0x87);
            let mut script_sig = vec![redeem.len() as u8];
            script_sig.extend(redeem);
            let mut coin = [0u8; 32];
            coin[0] = i as u8;
            coin[1] = (i >> 8) as u8;
            coin[2] = 0x5a;
            let prevout = OutPoint {
                hash: coin,
                index: 0,
            };
            let utxo = UTXO {
                value: 1_000,
                script_pubkey: script_pubkey.into(),
                height: 0,
                is_coinbase: false,
            };
            snapshot_coin(&storage, &prevout, &utxo);
            set.insert(prevout, Arc::new(utxo));
            inputs.push(TransactionInput {
                prevout,
                script_sig,
                sequence: 0xffffffff,
            });
        }
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
        let tx = Transaction {
            version: 1,
            inputs: inputs.into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx).unwrap();
    }
    eprintln!(
        "sigop cost 16000 reported={} admitted={}; sigop cost 16004 reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_p2wsh_item_of_81_bytes_is_not_acceptable() {
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};
    use sha2::{Digest, Sha256};

    let script = vec![0x75, 0x51];
    let program: [u8; 32] = Sha256::digest(&script).into();
    let mut script_pubkey = vec![0x00, 0x20];
    script_pubkey.extend_from_slice(&program);
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, item_len) in [80usize, 81].into_iter().enumerate() {
        let witness: Vec<Witness> = vec![vec![vec![0u8; item_len], script.clone()]];
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 100_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 100_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, Some(&witness));
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap();
    }
    eprintln!(
        "80-byte item reported={} admitted={}; 81-byte item reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_p2wsh_script_of_3601_bytes_is_not_acceptable() {
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};
    use sha2::{Digest, Sha256};

    let script_of = |script_len: usize| {
        let mut script = Vec::with_capacity(script_len);
        let mut left = script_len - 1;
        while left > 524 {
            script.push(0x4d);
            script.extend_from_slice(&520u16.to_le_bytes());
            script.extend(std::iter::repeat(0u8).take(520));
            script.push(0x75);
            left -= 524;
        }
        let payload = left - 4;
        script.push(0x4d);
        script.extend_from_slice(&(payload as u16).to_le_bytes());
        script.extend(std::iter::repeat(0u8).take(payload));
        script.push(0x75);
        script.push(0x51);
        script
    };
    let prevout_point = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, script_len) in [3_600usize, 3_601].into_iter().enumerate() {
        let script = script_of(script_len);
        let program: [u8; 32] = Sha256::digest(&script).into();
        let mut script_pubkey = vec![0x00, 0x20];
        script_pubkey.extend_from_slice(&program);
        let witness: Vec<Witness> = vec![vec![script]];
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout_point,
            &UTXO {
                value: 100_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout_point,
                Arc::new(UTXO {
                    value: 100_000,
                    script_pubkey: script_pubkey.into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: prevout_point,
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, Some(&witness));
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap();
    }
    eprintln!(
        "3600-byte script reported={} admitted={}; 3601-byte script reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_pay_to_anchor_witness_is_not_acceptable() {
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let script_pubkey = vec![0x51, 0x02, 0x4e, 0x73];
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, stack) in [Vec::new(), vec![vec![0x00]]].into_iter().enumerate() {
        let witness: Vec<Witness> = vec![stack];
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 100_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 100_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, Some(&witness));
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap();
    }
    eprintln!(
        "empty anchor reported={} admitted={}; witness byte reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_p2wsh_stack_of_101_items_is_not_acceptable() {
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};
    use sha2::{Digest, Sha256};

    let prevout_point = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, items) in [100usize, 101].into_iter().enumerate() {
        let mut script = vec![0x75; items];
        script.push(0x51);
        let program: [u8; 32] = Sha256::digest(&script).into();
        let mut script_pubkey = vec![0x00, 0x20];
        script_pubkey.extend_from_slice(&program);
        let mut stack = vec![Vec::new(); items];
        stack.push(script);
        let witness: Vec<Witness> = vec![stack];
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout_point,
            &UTXO {
                value: 100_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout_point,
                Arc::new(UTXO {
                    value: 100_000,
                    script_pubkey: script_pubkey.into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: prevout_point,
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, Some(&witness));
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap();
    }
    eprintln!(
        "100 items reported={} admitted={}; 101 items reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_taproot_annex_is_not_acceptable() {
    use blvm_consensus::secp256k1_backend::taproot_output_key_with_parity;
    use blvm_consensus::taproot::{TAPROOT_LEAF_VERSION_TAPSCRIPT, compute_script_merkle_root};
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let tapscript = vec![0x51u8];
    let internal = [
        0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87, 0x0b,
        0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8,
        0x17, 0x98,
    ];
    let root = compute_script_merkle_root(&tapscript, &[], TAPROOT_LEAF_VERSION_TAPSCRIPT).unwrap();
    let (output_key, parity) = taproot_output_key_with_parity(&internal, &root).unwrap();
    let mut script_pubkey = vec![0x51, 0x20];
    script_pubkey.extend_from_slice(&output_key);
    let mut control = vec![TAPROOT_LEAF_VERSION_TAPSCRIPT | parity];
    control.extend_from_slice(&internal);
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, annex) in [false, true].into_iter().enumerate() {
        let mut stack = vec![tapscript.clone(), control.clone()];
        if annex {
            stack.push(vec![0x50]);
        }
        let witness: Vec<Witness> = vec![stack];
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 100_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 100_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, Some(&witness));
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap();
    }
    eprintln!(
        "script path reported={} admitted={}; annex reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_tapscript_item_of_81_bytes_is_not_acceptable() {
    use blvm_consensus::secp256k1_backend::taproot_output_key_with_parity;
    use blvm_consensus::taproot::{TAPROOT_LEAF_VERSION_TAPSCRIPT, compute_script_merkle_root};
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let tapscript = vec![0x75u8, 0x51];
    let internal = [
        0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87, 0x0b,
        0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8,
        0x17, 0x98,
    ];
    let root = compute_script_merkle_root(&tapscript, &[], TAPROOT_LEAF_VERSION_TAPSCRIPT).unwrap();
    let (output_key, parity) = taproot_output_key_with_parity(&internal, &root).unwrap();
    let mut script_pubkey = vec![0x51, 0x20];
    script_pubkey.extend_from_slice(&output_key);
    let mut control = vec![TAPROOT_LEAF_VERSION_TAPSCRIPT | parity];
    control.extend_from_slice(&internal);
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, item_len) in [80usize, 81].into_iter().enumerate() {
        let witness: Vec<Witness> = vec![vec![
            vec![0u8; item_len],
            tapscript.clone(),
            control.clone(),
        ]];
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 100_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 100_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, Some(&witness));
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap();
    }
    eprintln!(
        "80-byte tapscript item reported={} admitted={}; 81-byte tapscript item reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_sigop_cost_over_16000_is_not_acceptable() {
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};
    use sha2::{Digest, Sha256};

    fn witness_script(multisigs: usize) -> Vec<u8> {
        let mut script = vec![0x00, 0x63];
        for _ in 0..multisigs {
            script.extend_from_slice(&[0x60, 0xae]);
        }
        script.extend_from_slice(&[0x68, 0x51]);
        script
    }
    let prevouts: Vec<OutPoint> = (0..6)
        .map(|i| OutPoint {
            hash: [i + 1; 32],
            index: 0,
        })
        .collect();
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, tail) in [10usize, 11].into_iter().enumerate() {
        let mut counts = vec![198usize; 5];
        counts.push(tail);
        let scripts: Vec<Vec<u8>> = counts.iter().copied().map(witness_script).collect();
        let programs: Vec<Vec<u8>> = scripts
            .iter()
            .map(|script| {
                let program: [u8; 32] = Sha256::digest(script).into();
                let mut script_pubkey = vec![0x00, 0x20];
                script_pubkey.extend_from_slice(&program);
                script_pubkey
            })
            .collect();
        let witness: Vec<Witness> = scripts.iter().map(|script| vec![script.clone()]).collect();
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let mut set = blvm_protocol::UtxoSet::default();
        for (prevout, script_pubkey) in prevouts.iter().zip(programs.iter()) {
            let utxo = UTXO {
                value: 20_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            };
            snapshot_coin(&storage, prevout, &utxo);
            set.insert(*prevout, Arc::new(utxo));
        }
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
        let tx = Transaction {
            version: 1,
            inputs: prevouts
                .iter()
                .copied()
                .map(|prevout| TransactionInput {
                    prevout,
                    script_sig: Vec::new(),
                    sequence: 0xffffffff,
                })
                .collect::<Vec<_>>()
                .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, Some(&witness));
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap();
    }
    eprintln!(
        "16000 sigop cost reported={} admitted={}; 16016 sigop cost reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_fee_under_the_sigop_adjusted_floor_is_not_acceptable() {
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};
    use sha2::{Digest, Sha256};

    let mut script = vec![0x00, 0x63];
    for _ in 0..100 {
        script.extend_from_slice(&[0x60, 0xae]);
    }
    script.extend_from_slice(&[0x68, 0x51]);
    let program: [u8; 32] = Sha256::digest(&script).into();
    let mut script_pubkey = vec![0x00, 0x20];
    script_pubkey.extend_from_slice(&program);
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let witness: Vec<Witness> = vec![vec![script]];
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, output_value) in [19_200i64, 19_201].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 20_000,
                script_pubkey: script_pubkey.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 20_000,
                    script_pubkey: script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: output_value,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, Some(&witness));
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool
            .add_transaction_with_witness(tx, Some(witness.clone()))
            .unwrap();
    }
    eprintln!(
        "fee 800 reported={} admitted={}; fee 799 reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_witness_weight_over_400000_is_not_acceptable() {
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};
    use sha2::{Digest, Sha256};

    let script_of = |script_len: usize| {
        let mut script = Vec::with_capacity(script_len);
        let mut left = script_len - 1;
        while left > 524 {
            script.push(0x4d);
            script.extend_from_slice(&520u16.to_le_bytes());
            script.extend(std::iter::repeat(0u8).take(520));
            script.push(0x75);
            left -= 524;
        }
        let payload = left - 4;
        script.push(0x4d);
        script.extend_from_slice(&(payload as u16).to_le_bytes());
        script.extend(std::iter::repeat(0u8).take(payload));
        script.push(0x75);
        script.push(0x51);
        script
    };
    let full = script_of(3_600);
    let full_program: [u8; 32] = Sha256::digest(&full).into();
    let mut full_script = vec![0x00, 0x20];
    full_script.extend_from_slice(&full_program);
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, short_len) in [326usize, 327].into_iter().enumerate() {
        let short = script_of(short_len);
        let short_program: [u8; 32] = Sha256::digest(&short).into();
        let mut short_script = vec![0x00, 0x20];
        short_script.extend_from_slice(&short_program);
        let prevouts: Vec<OutPoint> = (0..107)
            .map(|i| OutPoint {
                hash: [i + 1; 32],
                index: 0,
            })
            .collect();
        let mut witness: Vec<Witness> = Vec::with_capacity(107);
        witness.push(vec![short]);
        for _ in 1..107 {
            witness.push(vec![full.clone()]);
        }
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let mut set = blvm_protocol::UtxoSet::default();
        for (input_index, prevout) in prevouts.iter().enumerate() {
            let script_pubkey = if input_index == 0 {
                short_script.clone()
            } else {
                full_script.clone()
            };
            let utxo = UTXO {
                value: 1_000,
                script_pubkey: script_pubkey.into(),
                height: 0,
                is_coinbase: false,
            };
            snapshot_coin(&storage, prevout, &utxo);
            set.insert(*prevout, Arc::new(utxo));
        }
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
        let tx = Transaction {
            version: 1,
            inputs: prevouts
                .iter()
                .copied()
                .map(|prevout| TransactionInput {
                    prevout,
                    script_sig: Vec::new(),
                    sequence: 0xffffffff,
                })
                .collect::<Vec<_>>()
                .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, Some(&witness));
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap();
    }
    eprintln!(
        "weight 400000 reported={} admitted={}; weight 400001 reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_chain_of_26_is_not_acceptable() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let spend = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let origin = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut prev = origin;
    let mut value = 100_000i64;
    let mut txs = Vec::with_capacity(26);
    for _ in 0..26 {
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: prev,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: value - 1_000,
                script_pubkey: spend.clone(),
            }]
            .into(),
            lock_time: 0,
        };
        prev = OutPoint {
            hash: calculate_tx_id(&tx),
            index: 0,
        };
        value -= 1_000;
        txs.push(tx);
    }
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    snapshot_coin(
        &storage,
        &origin,
        &UTXO {
            value: 100_000,
            script_pubkey: spend.into(),
            height: 0,
            is_coinbase: false,
        },
    );
    let mempool = Arc::new(MempoolManager::new());
    mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
        let mut set = blvm_protocol::UtxoSet::default();
        set.insert(
            origin,
            Arc::new(UTXO {
                value: 100_000,
                script_pubkey: vec![
                    0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56,
                    0x99, 0x71, 0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
                ]
                .into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        set
    })));
    for tx in &txs[..24] {
        assert!(mempool.add_transaction(tx.clone()).unwrap());
    }
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
    for (index, tx) in txs[24..].iter().enumerate() {
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx.clone()).unwrap();
    }
    eprintln!(
        "count 25 reported={} admitted={}; count 26 reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_fee_under_min_tx_fee_is_not_acceptable() {
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let spend = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let prevout = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, output_value) in [9_000i64, 9_001].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &prevout,
            &UTXO {
                value: 10_000,
                script_pubkey: spend.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mut policy = crate::config::MempoolPolicyConfig::default();
        policy.min_tx_fee = 1_000;
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(policy));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                prevout,
                Arc::new(UTXO {
                    value: 10_000,
                    script_pubkey: spend.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: output_value,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex_tx = RawTxRpc::serialize_transaction_with_witness(&tx, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex_tx]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        admitted[index] = mempool.add_transaction(tx).unwrap();
    }
    eprintln!(
        "fee 1000 reported={} admitted={}; fee 999 reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_package_child_prevout_is_visible() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let spend = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let origin = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let spend_of = |prevout: OutPoint| Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout,
            script_sig: vec![0x01, 0x51],
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 0,
            script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
        }]
        .into(),
        lock_time: 0,
    };
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    snapshot_coin(
        &storage,
        &origin,
        &UTXO {
            value: 10_000,
            script_pubkey: spend.clone().into(),
            height: 0,
            is_coinbase: false,
        },
    );
    let chain = spend_of(origin);
    let chain_pool = Arc::new(MempoolManager::new());
    chain_pool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
    chain_pool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
        let mut set = blvm_protocol::UtxoSet::default();
        set.insert(
            origin,
            Arc::new(UTXO {
                value: 10_000,
                script_pubkey: spend.clone().into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        set
    })));
    let chain_rpc =
        RawTxRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&chain_pool), None, None);
    let chain_hex = RawTxRpc::serialize_transaction_with_witness(&chain, None);
    let chain_accepted = chain_rpc
        .testmempoolaccept(&json!([[chain_hex]]))
        .await
        .unwrap();
    let chain_reported = chain_accepted[0]["allowed"] == json!(true);
    let chain_admitted = chain_pool.add_transaction(chain).unwrap();

    let parent = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: origin,
            script_sig: vec![0x01, 0x51],
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 10_000,
            script_pubkey: spend.clone(),
        }]
        .into(),
        lock_time: 0,
    };
    let child = spend_of(OutPoint {
        hash: calculate_tx_id(&parent),
        index: 0,
    });
    let package_pool = Arc::new(MempoolManager::new());
    package_pool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
    package_pool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
        let mut set = blvm_protocol::UtxoSet::default();
        set.insert(
            origin,
            Arc::new(UTXO {
                value: 10_000,
                script_pubkey: spend.into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        set
    })));
    let package_rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&package_pool), None, None);
    let parent_hex = RawTxRpc::serialize_transaction_with_witness(&parent, None);
    let child_hex = RawTxRpc::serialize_transaction_with_witness(&child, None);
    let accepted = package_rpc
        .testmempoolaccept(&json!([[parent_hex, child_hex]]))
        .await
        .unwrap();
    let child_reported = accepted[1]["allowed"] == json!(true);
    let child_id = calculate_tx_id(&child);
    package_pool.add_package(vec![parent, child]).unwrap();
    let child_admitted = package_pool.get_transaction(&child_id).is_some();
    eprintln!(
        "chain spend reported={chain_reported} admitted={chain_admitted}; package child reported={child_reported} admitted={child_admitted}"
    );
    assert!(chain_reported && chain_admitted);
    assert!(child_reported && child_admitted);
}

#[tokio::test]
async fn a_zero_fee_dust_parent_is_acceptable_in_a_package() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let spend = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let origin = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, amount) in [540i64, 539].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &origin,
            &UTXO {
                value: amount,
                script_pubkey: spend.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let parent = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: origin,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: amount,
                script_pubkey: spend.clone(),
            }]
            .into(),
            lock_time: 0,
        };
        let child = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: calculate_tx_id(&parent),
                    index: 0,
                },
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                origin,
                Arc::new(UTXO {
                    value: amount,
                    script_pubkey: spend.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let parent_hex = RawTxRpc::serialize_transaction_with_witness(&parent, None);
        let child_hex = RawTxRpc::serialize_transaction_with_witness(&child, None);
        let accepted = rpc
            .testmempoolaccept(&json!([[parent_hex, child_hex]]))
            .await
            .unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        let parent_id = calculate_tx_id(&parent);
        mempool.add_package(vec![parent, child]).unwrap();
        admitted[index] = mempool.get_transaction(&parent_id).is_some();
    }
    eprintln!(
        "value 540 reported={} admitted={}; value 539 reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(reported[1] && admitted[1]);
}

#[tokio::test]
async fn a_package_of_26_is_not_acceptable() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let spend = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let origin = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut prev = origin;
    let mut value = 100_000i64;
    let mut txs = Vec::with_capacity(26);
    for _ in 0..26 {
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: prev,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: value - 1_000,
                script_pubkey: spend.clone(),
            }]
            .into(),
            lock_time: 0,
        };
        prev = OutPoint {
            hash: calculate_tx_id(&tx),
            index: 0,
        };
        value -= 1_000;
        txs.push(tx);
    }
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, len) in [25usize, 26].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &origin,
            &UTXO {
                value: 100_000,
                script_pubkey: spend.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                origin,
                Arc::new(UTXO {
                    value: 100_000,
                    script_pubkey: spend.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let package = txs[..len].to_vec();
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hexes: Vec<String> = package
            .iter()
            .map(|tx| RawTxRpc::serialize_transaction_with_witness(tx, None))
            .collect();
        let accepted = rpc.testmempoolaccept(&json!([hexes])).await.unwrap();
        let last = accepted.as_array().unwrap().len() - 1;
        reported[index] = accepted[last]["allowed"] == json!(true);
        let last_id = calculate_tx_id(package.last().unwrap());
        mempool.add_package(package).unwrap();
        admitted[index] = mempool.get_transaction(&last_id).is_some();
    }
    eprintln!(
        "package 25 reported={} admitted={}; package 26 reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_package_child_relative_lock_uses_the_parent_height() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let spend = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let origin = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, sequence) in [0u64, 1].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &origin,
            &UTXO {
                value: 10_000,
                script_pubkey: spend.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let parent = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: origin,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 9_000,
                script_pubkey: spend.clone(),
            }]
            .into(),
            lock_time: 0,
        };
        let child = Transaction {
            version: 2,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: calculate_tx_id(&parent),
                    index: 0,
                },
                script_sig: vec![0x01, 0x51],
                sequence,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 0,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_chain_tip(500_000, 1_700_000_000);
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                origin,
                Arc::new(UTXO {
                    value: 10_000,
                    script_pubkey: spend.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let parent_hex = RawTxRpc::serialize_transaction_with_witness(&parent, None);
        let child_hex = RawTxRpc::serialize_transaction_with_witness(&child, None);
        let accepted = rpc
            .testmempoolaccept(&json!([[parent_hex, child_hex]]))
            .await
            .unwrap();
        reported[index] = accepted[1]["allowed"] == json!(true);
        let child_id = calculate_tx_id(&child);
        mempool.add_package(vec![parent, child]).unwrap();
        admitted[index] = mempool.get_transaction(&child_id).is_some();
    }
    eprintln!(
        "sequence 0 reported={} admitted={}; sequence 1 reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_replacement_under_the_relay_bump_is_not_acceptable() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let spend = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let origin = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, output_value) in [17_000i64, 17_999].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &origin,
            &UTXO {
                value: 20_000,
                script_pubkey: spend.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let original = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: origin,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 18_000,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let replacement = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: origin,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: output_value,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                origin,
                Arc::new(UTXO {
                    value: 20_000,
                    script_pubkey: spend.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        assert!(mempool.add_transaction(original).unwrap());
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex = RawTxRpc::serialize_transaction_with_witness(&replacement, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        let replacement_id = calculate_tx_id(&replacement);
        mempool.add_transaction(replacement).unwrap();
        admitted[index] = mempool.get_transaction(&replacement_id).is_some();
    }
    eprintln!(
        "fee 3000 reported={} admitted={}; fee 2001 reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(!reported[1] && !admitted[1]);
}

#[tokio::test]
async fn a_replacement_at_the_descendant_limit_is_acceptable() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    let spend = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let origin = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut reported = [false; 2];
    let mut admitted = [false; 2];
    for (index, child_count) in [23usize, 24].into_iter().enumerate() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        snapshot_coin(
            &storage,
            &origin,
            &UTXO {
                value: 1_000_000,
                script_pubkey: spend.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        );
        let parent = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: origin,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![
                TransactionOutput {
                    value: 10_000,
                    script_pubkey: spend.clone(),
                };
                24
            ]
            .into(),
            lock_time: 0,
        };
        let parent_id = calculate_tx_id(&parent);
        let child = |output_index: u32| Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: parent_id,
                    index: output_index,
                },
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 9_000,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let replacement = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: parent_id,
                    index: 0,
                },
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 8_000,
                script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
            }]
            .into(),
            lock_time: 0,
        };
        let mempool = Arc::new(MempoolManager::new());
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
            let mut set = blvm_protocol::UtxoSet::default();
            set.insert(
                origin,
                Arc::new(UTXO {
                    value: 1_000_000,
                    script_pubkey: spend.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            set
        })));
        assert!(mempool.add_transaction(parent).unwrap());
        for output_index in 0..child_count {
            assert!(mempool.add_transaction(child(output_index as u32)).unwrap());
        }
        let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
        let hex = RawTxRpc::serialize_transaction_with_witness(&replacement, None);
        let accepted = rpc.testmempoolaccept(&json!([[hex]])).await.unwrap();
        reported[index] = accepted[0]["allowed"] == json!(true);
        let replacement_id = calculate_tx_id(&replacement);
        mempool.add_transaction(replacement).unwrap();
        admitted[index] = mempool.get_transaction(&replacement_id).is_some();
    }
    eprintln!(
        "children 23 reported={} admitted={}; children 24 reported={} admitted={}",
        reported[0], admitted[0], reported[1], admitted[1]
    );
    assert!(reported[0] && admitted[0]);
    assert!(reported[1] && admitted[1]);
}
