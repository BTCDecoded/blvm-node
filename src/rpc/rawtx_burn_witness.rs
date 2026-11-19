use super::{RawTxRpc, UNSPENDABLE_EXCEEDS_MAX_BURN};
use crate::node::mempool::MempoolManager;
use crate::storage::Storage;
use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO};
use serde_json::json;
use std::sync::Arc;

fn rpc_with(mempool: Arc<MempoolManager>) -> (tempfile::TempDir, RawTxRpc) {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let rpc = RawTxRpc::with_dependencies(storage, mempool, None, None);
    (temp, rpc)
}

fn one_input(prev: [u8; 32], script_sig: Vec<u8>, value: i64, script: Vec<u8>) -> Transaction {
    Transaction {
        version: 2,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: prev,
                index: 0,
            },
            script_sig,
            sequence: 0xffff_ffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value,
            script_pubkey: script,
        }]
        .into(),
        lock_time: 0,
    }
}

fn push_only_script_sig() -> Vec<u8> {
    let mut script = vec![0x3c];
    script.extend(std::iter::repeat(0x01).take(60));
    script
}

fn p2pkh() -> Vec<u8> {
    let mut script = vec![0x76, 0xa9, 0x14];
    script.extend([0u8; 20]);
    script.extend([0x88, 0xac]);
    script
}

fn hex_of(tx: &Transaction) -> String {
    RawTxRpc::serialize_transaction_with_witness(tx, None)
}

#[tokio::test]
async fn omitted_burn_cap_rejects_one_sat_op_return() {
    let mempool = Arc::new(MempoolManager::new());
    let (_temp, rpc) = rpc_with(Arc::clone(&mempool));
    let tx = one_input([1u8; 32], push_only_script_sig(), 1, vec![0x6a]);
    let err = rpc
        .sendrawtransaction(&json!([hex_of(&tx)]))
        .await
        .unwrap_err();
    assert_eq!(err.code.code(), -25);
    assert_eq!(err.message, UNSPENDABLE_EXCEEDS_MAX_BURN);
    assert_eq!(mempool.size(), 0);
}

#[tokio::test]
async fn one_sat_burn_cap_allows_one_sat_op_return() {
    let mempool = Arc::new(MempoolManager::new());
    let (_temp, rpc) = rpc_with(mempool);
    let tx = one_input([2u8; 32], push_only_script_sig(), 1, vec![0x6a]);
    let err = rpc
        .sendrawtransaction(&json!([hex_of(&tx), null, 0.00000001]))
        .await
        .unwrap_err();
    assert_ne!(err.message, UNSPENDABLE_EXCEEDS_MAX_BURN);
    assert_ne!(err.code.code(), -3);
}

#[tokio::test]
async fn opcode_above_nop10_is_a_burn() {
    use blvm_protocol::opcodes::OP_NOP10;

    let mempool = Arc::new(MempoolManager::new());
    let (_temp, rpc) = rpc_with(mempool);
    let above = one_input([9u8; 32], push_only_script_sig(), 1, vec![OP_NOP10 + 1]);
    let err = rpc
        .sendrawtransaction(&json!([hex_of(&above)]))
        .await
        .unwrap_err();
    assert_eq!(err.code.code(), -25);
    assert_eq!(err.message, UNSPENDABLE_EXCEEDS_MAX_BURN);

    let nop10 = one_input([10u8; 32], push_only_script_sig(), 1, vec![OP_NOP10]);
    let err = rpc
        .sendrawtransaction(&json!([hex_of(&nop10)]))
        .await
        .unwrap_err();
    assert_ne!(err.message, UNSPENDABLE_EXCEEDS_MAX_BURN);
}

#[tokio::test]
async fn one_sat_p2pkh_is_not_a_burn() {
    let mempool = Arc::new(MempoolManager::new());
    let (_temp, rpc) = rpc_with(mempool);
    let tx = one_input([3u8; 32], push_only_script_sig(), 1, p2pkh());
    let err = rpc
        .sendrawtransaction(&json!([hex_of(&tx)]))
        .await
        .unwrap_err();
    assert_ne!(err.message, UNSPENDABLE_EXCEEDS_MAX_BURN);
}

#[tokio::test]
async fn a_non_amount_burn_cap_is_type_error() {
    let mempool = Arc::new(MempoolManager::new());
    let (_temp, rpc) = rpc_with(mempool);
    let tx = one_input([4u8; 32], push_only_script_sig(), 100_000, p2pkh());
    let err = rpc
        .sendrawtransaction(&json!([hex_of(&tx), 0.10, true]))
        .await
        .unwrap_err();
    assert_eq!(err.code.code(), -3);
    assert_eq!(err.message, "Invalid amount");
}

#[tokio::test]
async fn allowhighfees_at_index_three_skips_the_fee_cap() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let prev = OutPoint {
        hash: [5u8; 32],
        index: 0,
    };
    storage
        .utxos()
        .add_utxo(
            &prev,
            &UTXO {
                value: 2_000_000,
                script_pubkey: p2pkh().into(),
                height: 1,
                is_coinbase: false,
            },
        )
        .unwrap();
    let mempool = Arc::new(MempoolManager::new());
    let rpc = RawTxRpc::with_dependencies(storage, mempool, None, None);
    let tx = one_input(prev.hash, push_only_script_sig(), 1_000, p2pkh());
    let hex_tx = hex_of(&tx);
    let capped = rpc
        .sendrawtransaction(&json!([hex_tx.clone(), 0.00000001]))
        .await
        .unwrap_err();
    assert!(capped.message.contains("BTC/kvB"), "{}", capped.message);
    let skipped = rpc
        .sendrawtransaction(&json!([hex_tx, 0.00000001, 0, true]))
        .await;
    match skipped {
        Ok(_) => {}
        Err(err) => assert!(
            !err.message.contains("BTC/kvB"),
            "index 3 did not skip the fee cap: {}",
            err.message
        ),
    }
}

#[tokio::test]
async fn package_burn_admits_nothing() {
    let mempool = Arc::new(MempoolManager::new());
    let (_temp, rpc) = rpc_with(Arc::clone(&mempool));
    let first = one_input([6u8; 32], push_only_script_sig(), 100_000, p2pkh());
    let second = one_input([7u8; 32], push_only_script_sig(), 1, vec![0x6a]);
    assert_eq!(
        mempool
            .add_transaction_with_witness(first.clone(), None)
            .unwrap(),
        true,
        "the first member has to be admissible on its own"
    );
    mempool.remove_transaction(&blvm_protocol::block::calculate_tx_id(&first));
    assert_eq!(mempool.size(), 0);
    let err = rpc
        .submitpackage(&json!([[hex_of(&first), hex_of(&second)]]))
        .await
        .unwrap_err();
    assert_eq!(err.code.code(), -25);
    assert_eq!(err.message, UNSPENDABLE_EXCEEDS_MAX_BURN);
    assert_eq!(mempool.size(), 0);
}

#[tokio::test]
async fn duplicate_txid_returns_the_stored_id_and_package_splits_witnesses() {
    let mempool = Arc::new(MempoolManager::new());
    let (_temp, rpc) = rpc_with(Arc::clone(&mempool));
    let tx = one_input([8u8; 32], push_only_script_sig(), 100_000, p2pkh());
    let witness = vec![vec![vec![0x01u8]]];
    assert!(
        mempool
            .add_transaction_with_witness(tx.clone(), Some(witness.clone()))
            .unwrap(),
        "a witnessed member has to be admissible on its own"
    );
    let bare = hex_of(&tx);
    let witnessed = RawTxRpc::serialize_transaction_with_witness(&tx, Some(&witness));
    let txid_hex =
        crate::storage::hashing::hash_to_rpc_hex(&blvm_protocol::block::calculate_tx_id(&tx));
    let stored_wtxid = crate::storage::hashing::hash_to_rpc_hex(
        &crate::network::txhash::calculate_wtxid(&tx, Some(witness.as_slice())),
    );
    assert_ne!(stored_wtxid, txid_hex);

    let sent = rpc
        .sendrawtransaction(&json!([bare.clone()]))
        .await
        .unwrap();
    assert_eq!(sent, json!(txid_hex));
    assert_eq!(mempool.size(), 1);

    let same = rpc.submitpackage(&json!([[witnessed]])).await.unwrap();
    assert_eq!(same[0]["txid"], json!(txid_hex));
    assert_eq!(same[0]["error"], json!("txn-already-in-mempool"));
    assert!(same[0].get("other-wtxid").is_none());

    let other = rpc.submitpackage(&json!([[bare]])).await.unwrap();
    assert_eq!(other[0]["txid"], json!(txid_hex));
    assert!(other[0].get("error").is_none(), "{other}");
    assert_eq!(other[0]["other-wtxid"], json!(stored_wtxid));
    assert_eq!(mempool.size(), 1);
}
