use super::{RawTxRpc, address_string_to_script_pubkey, script_pubkey_desc, script_pubkey_type};
use blvm_protocol::opcodes::{OP_PUSHDATA1, OP_RETURN};
use serde_json::json;

#[test]
fn witness_program_push_includes_its_length() {
    use blvm_protocol::address::{BitcoinAddress, Network};
    use blvm_protocol::opcodes::OP_1;

    let p2wpkh =
        address_string_to_script_pubkey("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4").unwrap();
    assert_eq!(
        p2wpkh,
        hex::decode("0014751e76e8199196d454941c45d1b3a323f1433bd6").unwrap()
    );

    let program = hex::decode("751e76e8199196d454941c45d1b3a323f1433bd6").unwrap();
    let regtest = BitcoinAddress::new(Network::Regtest, 0, program.clone())
        .unwrap()
        .encode()
        .unwrap();
    assert_eq!(address_string_to_script_pubkey(&regtest).unwrap(), p2wpkh);

    let p2wsh_program = vec![0x11u8; 32];
    let p2wsh = BitcoinAddress::new(Network::Regtest, 0, p2wsh_program.clone())
        .unwrap()
        .encode()
        .unwrap();
    let mut expected = vec![0x00, 0x20];
    expected.extend_from_slice(&p2wsh_program);
    let script = address_string_to_script_pubkey(&p2wsh).unwrap();
    assert_eq!(script, expected);
    assert_eq!(script_pubkey_type(&script), "witness_v0_scripthash");

    let p2tr_program = vec![0x22u8; 32];
    let p2tr = BitcoinAddress::new(Network::Regtest, 1, p2tr_program.clone())
        .unwrap()
        .encode()
        .unwrap();
    let mut expected = vec![OP_1, 0x20];
    expected.extend_from_slice(&p2tr_program);
    let script = address_string_to_script_pubkey(&p2tr).unwrap();
    assert_eq!(script, expected);
    assert_eq!(script_pubkey_type(&script), "witness_v1_taproot");
}

#[test]
fn three_satoshi_dust_and_eighty_byte_op_return() {
    assert_eq!(RawTxRpc::btc_amount_to_sats(0.00000003).unwrap(), 3);
    assert!(RawTxRpc::btc_amount_to_sats(-1.0).is_err());
    let payload = vec![0xab; 80];
    let script = RawTxRpc::op_return_script(&payload).unwrap();
    assert_eq!(script[0], OP_RETURN);
    assert_eq!(script[1], OP_PUSHDATA1);
    assert_eq!(script[2], 80);
    assert_eq!(&script[3..], payload.as_slice());
    assert_eq!(script_pubkey_type(&script), "nulldata");
    assert!(script_pubkey_desc(&script).starts_with("raw("));
    let mut key = vec![33, 0x02];
    key.extend(std::iter::repeat(0x11).take(32));
    key.push(blvm_protocol::opcodes::OP_CHECKSIG);
    assert_eq!(script_pubkey_type(&key), "pubkey");
    assert!(script_pubkey_desc(&key).starts_with("pk("));
    let bare = vec![
        0x51,
        33,
        0x02,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x11,
        0x51,
        blvm_protocol::opcodes::OP_CHECKMULTISIG,
    ];
    assert_eq!(script_pubkey_type(&bare), "multisig");
    assert!(script_pubkey_desc(&bare).contains("#"));
    assert_eq!(
        script_pubkey_type(&[
            blvm_protocol::opcodes::OP_RETURN,
            blvm_protocol::opcodes::OP_CHECKSIG
        ]),
        "nonstandard"
    );
    assert_eq!(
        crate::rpc::errors::RpcErrorCode::TxMissingInputs.code(),
        -25
    );
    assert_eq!(
        crate::rpc::errors::RpcErrorCode::TxAlreadyInMempool.code(),
        -26
    );
    assert_eq!(
        crate::rpc::errors::RpcErrorCode::TxAlreadyInChain.code(),
        -27
    );
}

#[tokio::test]
async fn locktime_above_u32_max_is_rejected() {
    let rpc = RawTxRpc::new();
    let err = rpc
        .createrawtransaction(&json!([
            [{"txid": "11".repeat(32), "vout": 0}],
            {"bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4": 0.001},
            4294967296u64
        ]))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("locktime"), "{text}");
}

#[tokio::test]
async fn output_above_max_money_is_rejected() {
    let rpc = RawTxRpc::new();
    let err = rpc
        .createrawtransaction(&json!([
            [{"txid": "11".repeat(32), "vout": 0}],
            {"bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4": 21000001.0}
        ]))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("too large"), "{text}");
}

#[tokio::test]
async fn both_writers_emit_three_sats_and_an_eighty_byte_op_return() {
    use blvm_protocol::opcodes::OP_PUSHDATA1;
    let rpc = RawTxRpc::new();
    let txid = "11".repeat(32);
    let payload = "ab".repeat(80);
    let address = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";
    let object = rpc
        .createrawtransaction(&json!([
            [{"txid": txid, "vout": 0}],
            {address: 0.00000003, "data": payload}
        ]))
        .await
        .unwrap();
    let array = rpc
        .createrawtransaction(&json!([
            [{"txid": txid, "vout": 0}],
            [{"address": address, "amount": 0.00000003}, {"data": payload}]
        ]))
        .await
        .unwrap();
    for hex_tx in [object.as_str().unwrap(), array.as_str().unwrap()] {
        let (tx, _) =
            RawTxRpc::deserialize_transaction_with_witness(&hex::decode(hex_tx).unwrap()).unwrap();
        let pay = tx.outputs.iter().find(|output| output.value == 3).unwrap();
        assert_eq!(
            pay.script_pubkey.as_slice(),
            hex::decode("0014751e76e8199196d454941c45d1b3a323f1433bd6")
                .unwrap()
                .as_slice()
        );
        assert_eq!(
            script_pubkey_type(pay.script_pubkey.as_slice()),
            "witness_v0_keyhash"
        );
        let data = tx
            .outputs
            .iter()
            .find(|output| output.script_pubkey.first() == Some(&OP_RETURN))
            .unwrap();
        assert_eq!(data.script_pubkey[1], OP_PUSHDATA1);
        assert_eq!(data.script_pubkey[2], 80);
        assert_eq!(script_pubkey_type(data.script_pubkey.as_ref()), "nulldata");
    }
    assert!(
        rpc.createrawtransaction(&json!([
            [{"txid": txid, "vout": 0}],
            {address: -1.0}
        ]))
        .await
        .is_err()
    );
}

#[tokio::test]
async fn omitted_maxfeerate_rejects_a_rate_above_one_tenth_btc_per_kvb() {
    use crate::node::mempool::MempoolManager;
    use crate::storage::Storage;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{
        OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO, UtxoSet,
    };
    use std::sync::Arc;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    mempool.set_policy_config(Some(policy));
    let funding = OutPoint {
        hash: [6u8; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 2_000_000,
        script_pubkey: vec![OP_1].into(),
        height: 1,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&funding, &utxo).unwrap();
    let mut set = UtxoSet::default();
    set.insert(funding, Arc::new(utxo));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
    let tx = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: funding,
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
    let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
    let err = rpc.sendrawtransaction(&json!([hex_tx])).await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("BTC/kvB"), "{text}");
    assert!(
        mempool
            .get_transaction(&blvm_protocol::block::calculate_tx_id(&tx))
            .is_none()
    );
}

#[tokio::test]
async fn sendrawtransaction_looks_up_prevouts_not_the_utxo_set() {
    use crate::node::mempool::MempoolManager;
    use crate::storage::Storage;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO};
    use std::sync::Arc;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    mempool.set_policy_config(Some(policy));
    let funding = OutPoint {
        hash: [6u8; 32],
        index: 0,
    };
    storage
        .utxos()
        .add_utxo(
            &funding,
            &UTXO {
                value: 2_000_000,
                script_pubkey: vec![OP_1].into(),
                height: 1,
                is_coinbase: false,
            },
        )
        .unwrap();
    storage
        .utxos()
        .insert_undecodable_utxo(&OutPoint {
            hash: [9u8; 32],
            index: 0,
        })
        .unwrap();
    let tx = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: funding,
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
    let err = RawTxRpc::with_dependencies(storage, mempool, None, None)
        .sendrawtransaction(&json!([hex_tx]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("BTC/kvB"), "{err}");
    assert!(!err.contains("Failed to get UTXO set"), "{err}");
}

#[tokio::test]
async fn pool_parent_is_admitted_and_a_missing_output_is_rejected() {
    use crate::node::mempool::MempoolManager;
    use crate::storage::Storage;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{
        OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO, UtxoSet,
    };
    use std::sync::Arc;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    mempool.set_policy_config(Some(policy));
    let funding = OutPoint {
        hash: [5u8; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 50_000,
        script_pubkey: vec![OP_1].into(),
        height: 1,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&funding, &utxo).unwrap();
    let mut set = UtxoSet::default();
    set.insert(funding, Arc::new(utxo));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
    let parent = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: funding,
            script_sig: Vec::new(),
            sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 40_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    assert!(mempool.add_transaction(parent.clone()).unwrap());
    let child = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: blvm_protocol::block::calculate_tx_id(&parent),
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 30_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    let child_hex = RawTxRpc::serialize_transaction_with_witness(&child, None);
    let rpc = RawTxRpc::with_dependencies(storage, Arc::clone(&mempool), None, None);
    let admitted = rpc.sendrawtransaction(&json!([child_hex])).await.unwrap();
    assert_eq!(admitted.as_str().unwrap().len(), 64);
    assert!(
        mempool
            .get_transaction(&blvm_protocol::block::calculate_tx_id(&child))
            .is_some()
    );

    let missing = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [8u8; 32],
                index: 0,
            },
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
    let missing_hex = RawTxRpc::serialize_transaction_with_witness(&missing, None);
    assert!(rpc.sendrawtransaction(&json!([missing_hex])).await.is_err());
}

#[tokio::test]
async fn output_at_height_1001_reports_confirmations_and_a_spent_pool_output_is_null() {
    use crate::node::mempool::MempoolManager;
    use crate::storage::Storage;
    use crate::storage::hashing::hash_to_rpc_hex;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{
        BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO, UtxoSet,
    };
    use std::sync::Arc;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [1u8; 32],
        timestamp: 1_600_000_000,
        bits: 0x207fffff,
        nonce: 1,
    };
    storage.chain().initialize(&header).unwrap();
    storage
        .chain()
        .cache_block_chainwork(&header.prev_block_hash, &header, 0)
        .unwrap();
    let tip = storage.chain().get_tip_hash().unwrap().unwrap();
    storage.chain().update_tip(&tip, &header, 2000).unwrap();
    let created = OutPoint {
        hash: [6u8; 32],
        index: 0,
    };
    storage
        .utxos()
        .add_utxo(
            &created,
            &UTXO {
                value: 50_000,
                script_pubkey: vec![OP_1].into(),
                height: 1001,
                is_coinbase: true,
            },
        )
        .unwrap();
    let mempool = Arc::new(MempoolManager::new());
    let rpc = RawTxRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&mempool), None, None);
    let shown = rpc
        .gettxout(&json!([hash_to_rpc_hex(&created.hash), 0, false]))
        .await
        .unwrap();
    assert_eq!(shown["confirmations"].as_i64(), Some(2000 - 1001 + 1));
    assert_eq!(shown["coinbase"].as_bool(), Some(true));
    assert_eq!(shown["scriptPubKey"]["type"], "nonstandard");

    let parent = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [2u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 20_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    let parent_id = blvm_protocol::block::calculate_tx_id(&parent);
    let spent = OutPoint {
        hash: parent_id,
        index: 0,
    };
    let child = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: spent,
            script_sig: Vec::new(),
            sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 10_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    mempool.add_transaction(parent).unwrap();
    mempool.add_transaction(child).unwrap();
    let hidden = rpc
        .gettxout(&json!([hash_to_rpc_hex(&parent_id), 0, true]))
        .await
        .unwrap();
    assert!(hidden.is_null());
}

#[tokio::test]
async fn unknown_txid_errors_and_pool_fee_is_the_base_fee() {
    use crate::node::mempool::MempoolManager;
    use crate::rpc::mempool::MempoolRpc;
    use crate::storage::Storage;
    use crate::storage::hashing::hash_to_rpc_hex;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{
        OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO, UtxoSet,
    };
    use std::sync::Arc;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let rpc = RawTxRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&mempool), None, None);
    assert!(
        rpc.getrawtransaction(&json!([hash_to_rpc_hex(&[9u8; 32])]))
            .await
            .is_err()
    );

    let funding = OutPoint {
        hash: [7u8; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 50_000,
        script_pubkey: vec![OP_1].into(),
        height: 1,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&funding, &utxo).unwrap();
    let mut set = UtxoSet::default();
    set.insert(funding, Arc::new(utxo.clone()));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
    let tx = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: funding,
            script_sig: Vec::new(),
            sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 40_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    assert!(mempool.add_transaction(tx.clone()).unwrap());
    let listing = MempoolRpc::with_dependencies(mempool.clone(), storage)
        .getrawmempool(&json!([true]))
        .await
        .unwrap();
    let entry = listing.as_object().unwrap().values().next().unwrap();
    let mut priced = UtxoSet::default();
    priced.insert(funding, Arc::new(utxo));
    let fee = mempool.calculate_transaction_fee(&tx, &priced) as f64 / 100_000_000.0;
    assert_eq!(entry["fees"]["base"].as_f64(), Some(fee));
}

#[tokio::test]
async fn decoderaw_puts_one_op_1_witness_on_vin() {
    use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};
    let tx = Transaction {
        version: 2,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [1u8; 32],
                index: 0,
            },
            script_sig: Vec::new().into(),
            sequence: 0xfffffffe,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 1,
            script_pubkey: RawTxRpc::op_return_script(&[0xab; 80]).unwrap().into(),
        }]
        .into(),
        lock_time: 0,
    };
    let witnesses = vec![vec![vec![blvm_protocol::opcodes::OP_1]]];
    let hex = RawTxRpc::serialize_transaction_with_witness(&tx, Some(&witnesses));
    let decoded = RawTxRpc::new()
        .decoderawtransaction(&json!([hex]))
        .await
        .unwrap();
    assert_eq!(decoded["vin"][0]["txinwitness"][0], "51");
    assert_eq!(decoded["vout"][0]["scriptPubKey"]["type"], "nulldata");
}

fn base58check(payload: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let hash1 = Sha256::digest(payload);
    let hash2 = Sha256::digest(hash1);
    let mut data = payload.to_vec();
    data.extend_from_slice(&hash2[..4]);
    let mut zeros = 0usize;
    while zeros < data.len() && data[zeros] == 0 {
        zeros += 1;
    }
    let mut digits = vec![0u8];
    for byte in &data[zeros..] {
        let mut carry = *byte as u32;
        for digit in digits.iter_mut() {
            carry += (*digit as u32) * 256;
            *digit = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut out = "1".repeat(zeros);
    for digit in digits.iter().rev() {
        out.push(ALPHABET[*digit as usize] as char);
    }
    out
}

#[test]
fn testnet_p2pkh_and_regtest_p2sh_become_scripts() {
    use blvm_protocol::opcodes::{OP_DUP, OP_EQUAL, OP_HASH160};
    let mut p2pkh = vec![0x6f];
    p2pkh.extend_from_slice(&[0x11; 20]);
    let script = address_string_to_script_pubkey(&base58check(&p2pkh)).unwrap();
    assert_eq!(script[0], OP_DUP);
    assert_eq!(&script[3..23], &[0x11; 20]);

    let mut p2sh = vec![0xc4];
    p2sh.extend_from_slice(&[0x22; 20]);
    let script = address_string_to_script_pubkey(&base58check(&p2sh)).unwrap();
    assert_eq!(script[0], OP_HASH160);
    assert_eq!(*script.last().unwrap(), OP_EQUAL);
    assert_eq!(&script[2..22], &[0x22; 20]);
}
