//! Mempool admission, policy, persistence, and reorg tests.

use super::{
    MempoolManager, decode_persisted_tx, effective_ranked_fee, encode_persisted_tx,
    replacement_clears_fee_rate,
};
use crate::config::MempoolPolicyConfig;
use blvm_protocol::serialization::transaction::serialize_transaction;

#[test]
fn priority_delta_does_not_overflow_the_ranked_fee() {
    assert_eq!(effective_ranked_fee(1_000, 0), 1_000);
    assert_eq!(effective_ranked_fee(1_000, i64::MAX), i64::MAX as u64);
    assert_eq!(effective_ranked_fee(1_000, -50), 950);
}

#[test]
fn legacy_mempool_count_is_rejected_before_reserve() {
    let pool = MempoolManager::new();
    let err = pool
        .load_legacy_bytes(&u32::MAX.to_le_bytes())
        .expect_err("a count larger than the file must fail");
    assert!(
        err.to_string().contains("truncated"),
        "unexpected error: {err}"
    );
    assert_eq!(pool.size(), 0);
}

#[test]
fn replacement_fee_rate_does_not_use_binary64() {
    let n = (1u128 << 53) + 1;
    assert!(!replacement_clears_fee_rate(n, n, 1.0).unwrap());
    assert!(replacement_clears_fee_rate(n + 1, n, 1.0).unwrap());
    let aggressive = 562949953421319u128;
    let floor = 591097451092384u128;
    assert!(!replacement_clears_fee_rate(floor, aggressive, 1.05).unwrap());
    assert!(replacement_clears_fee_rate(floor + 1, aggressive, 1.05).unwrap());
    assert!(!replacement_clears_fee_rate(1, 1, f64::INFINITY).unwrap());
}

#[test]
fn persisted_tx_keeps_a_witness_stack() {
    let tx = legacy_empty_script();
    let bare = encode_persisted_tx(&tx, None).unwrap();
    let (_, bare_wits) = decode_persisted_tx(&bare).unwrap();
    assert!(bare_wits.is_none());

    let stacks: Vec<blvm_protocol::Witness> = vec![vec![vec![0x30, 0x01]]];
    let with_witness = encode_persisted_tx(&tx, Some(&stacks)).unwrap();
    assert_eq!(with_witness.len(), bare.len() + 6);
    let (_, wits) = decode_persisted_tx(&with_witness).unwrap();
    assert_eq!(wits.unwrap(), stacks);
}

#[test]
fn admission_stops_at_the_transaction_cap() {
    let mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_mempool_txs = 1;
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    mempool.set_policy_config(Some(policy));
    let mut first = legacy_empty_script();
    first.inputs[0].prevout.hash = [1u8; 32];
    let mut second = legacy_empty_script();
    second.inputs[0].prevout.hash = [2u8; 32];
    assert!(mempool.add_transaction(first).unwrap());
    assert!(!mempool.add_transaction(second).unwrap());
    assert_eq!(mempool.size(), 1);
}

#[test]
fn duplicate_mempool_input_is_refused() {
    let mempool = MempoolManager::new();
    let mut tx = legacy_empty_script();
    tx.inputs.push(tx.inputs[0].clone());
    assert!(!mempool.add_transaction(tx).unwrap());
    assert_eq!(mempool.size(), 0);
}

#[test]
fn policy_cap_is_file_size_and_unset_policy_is_300_mib() {
    let unset = MempoolManager::new();
    assert_eq!(unset.max_mempool_bytes(), 300 * 1_048_576);

    let set = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_mempool_mb = 50;
    set.set_policy_config(Some(policy));
    assert_eq!(set.max_mempool_bytes(), 50 * 1_048_576);
}

use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};

fn legacy_empty_script() -> Transaction {
    Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [1u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 1000,
            script_pubkey: {
                let mut script = vec![0x00, 0x14];
                script.extend_from_slice(&[0u8; 20]);
                script
            },
        }]
        .into(),
        lock_time: 0,
    }
}

fn funded_at(script_pubkey: Vec<u8>, value: i64, height: u64) -> blvm_protocol::UtxoSet {
    use std::sync::Arc;
    let tx = legacy_empty_script();
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    utxo_set.insert(
        tx.inputs[0].prevout,
        Arc::new(blvm_protocol::UTXO {
            value,
            script_pubkey: script_pubkey.into(),
            height,
            is_coinbase: false,
        }),
    );
    utxo_set
}

#[test]
fn default_policy_rejects_a_nonstandard_output_and_a_heavy_transaction() {
    use std::sync::Arc;

    let mempool = MempoolManager::new();
    let mut policy = crate::config::MempoolPolicyConfig::default();
    assert!(policy.require_standard);
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    mempool.set_policy_config(Some(policy));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded(
        vec![blvm_protocol::opcodes::OP_1],
        2_000_000,
    ))));

    let mut nonstandard = legacy_empty_script();
    nonstandard.outputs[0].script_pubkey = vec![0x51];
    assert!(!mempool.add_transaction(nonstandard).unwrap());

    let mut heavy = legacy_empty_script();
    heavy.inputs[0].script_sig = vec![0u8; 100_001];
    assert!(!mempool.add_transaction(heavy).unwrap());
}

fn funded(script_pubkey: Vec<u8>, value: i64) -> blvm_protocol::UtxoSet {
    funded_at(script_pubkey, value, 0)
}

#[test]
fn mempool_file_is_replaced_or_loaded_as_a_whole() {
    let mut first = legacy_empty_script();
    first.inputs[0].prevout.hash = [1u8; 32];
    let mut second = legacy_empty_script();
    second.inputs[0].prevout.hash = [2u8; 32];

    let source = MempoolManager::new();
    assert!(source.add_transaction(first).unwrap());
    assert!(source.add_transaction(second).unwrap());
    assert_eq!(source.size(), 2);

    let dir = std::env::temp_dir().join(format!("blvm-f50-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mempool.dat");
    source.save_to_disk(&path).unwrap();
    let intact = std::fs::read(&path).unwrap();
    assert!(intact.len() > 1);

    let mut torn = intact.clone();
    torn.pop();
    std::fs::write(&path, &torn).unwrap();
    let torn_pool = MempoolManager::new();
    let torn_load = torn_pool.load_from_disk(&path);
    assert!(
        torn_load.is_err(),
        "a short mempool file must return an error, got {torn_load:?}"
    );
    assert_eq!(
        torn_pool.size(),
        0,
        "a short mempool file must not keep a prefix"
    );

    std::fs::write(&path, &intact).unwrap();
    let refused = MempoolManager::new();
    refused.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(
        blvm_protocol::UtxoSet::default(),
    )));
    let refused_load = refused.load_from_disk(&path);
    assert!(
        refused_load.is_err(),
        "a file the wired UTXO set refuses must return an error, got {refused_load:?}"
    );
    assert_eq!(refused.size(), 0);

    let mut perms = std::fs::metadata(&dir).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&dir, perms).unwrap();
    let save = MempoolManager::new().save_to_disk(&path);
    let after = std::fs::read(&path).unwrap_or_default();
    let mut perms = std::fs::metadata(&dir).unwrap().permissions();
    perms.set_readonly(false);
    std::fs::set_permissions(&dir, perms).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        save.is_err(),
        "a save that cannot replace the file must fail, bytes={}",
        after.len()
    );
    assert_eq!(after, intact, "a failed save must leave the previous dump");
}

#[test]
fn child_before_parent_dump_is_admitted() {
    use blvm_protocol::block::calculate_tx_id;
    let mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    mempool.set_policy_config(Some(policy));

    let script = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    let mut parent = legacy_empty_script();
    parent.inputs[0].prevout.hash = [9u8; 32];
    parent.outputs[0].value = 50_000;
    parent.outputs[0].script_pubkey = script.clone();
    let parent_hash = calculate_tx_id(&parent);
    let mut child = legacy_empty_script();
    child.inputs[0].prevout.hash = parent_hash;
    child.outputs[0].value = 40_000;
    child.outputs[0].script_pubkey = script.clone();

    let mut utxo = blvm_protocol::UtxoSet::default();
    utxo.insert(
        parent.inputs[0].prevout,
        std::sync::Arc::new(blvm_protocol::UTXO {
            value: 60_000,
            script_pubkey: vec![blvm_protocol::opcodes::OP_1].into(),
            height: 1,
            is_coinbase: false,
        }),
    );
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(utxo)));

    let dump = crate::node::core_mempool_dat::CoreMempoolDat {
        txs: vec![
            crate::node::core_mempool_dat::CoreMempoolTx {
                tx: child,
                witnesses: None,
                time: 1,
                fee_delta: 0,
            },
            crate::node::core_mempool_dat::CoreMempoolTx {
                tx: parent,
                witnesses: None,
                time: 1,
                fee_delta: 0,
            },
        ],
        ..Default::default()
    };
    mempool.admit_core_dump(dump).unwrap();
    assert_eq!(mempool.size(), 2);
}

/// P2SH of `OP_1`, spent by a direct push of that redeem. Standard under default policy.
fn p2sh_of_op_1() -> (Vec<u8>, Vec<u8>) {
    let mut prevout = vec![0xa9, 0x14];
    prevout.extend_from_slice(&[
        0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71, 0xc7, 0x7e,
        0xba, 0x30, 0xcd, 0x5a, 0x4b,
    ]);
    prevout.push(0x87);
    (prevout, vec![0x01, 0x51])
}

fn funded_coinbase(height: u64, is_coinbase: bool) -> blvm_protocol::UtxoSet {
    use std::sync::Arc;
    let tx = legacy_empty_script();
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    utxo_set.insert(
        tx.inputs[0].prevout,
        Arc::new(blvm_protocol::UTXO {
            value: 2_000,
            script_pubkey: vec![blvm_protocol::opcodes::OP_1].into(),
            height,
            is_coinbase,
        }),
    );
    utxo_set
}

#[test]
fn an_outpoint_maps_to_its_spender() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    use std::sync::Arc;

    let mempool = MempoolManager::new();
    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    mempool.set_policy_config(Some(policy));
    let first = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let second = OutPoint {
        hash: [2u8; 32],
        index: 0,
    };
    let mut set = blvm_protocol::UtxoSet::default();
    for prevout in [first, second] {
        set.insert(
            prevout,
            Arc::new(blvm_protocol::UTXO {
                value: 50_000,
                script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
                height: 1,
                is_coinbase: false,
            }),
        );
    }
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
    let spend = |prevout: OutPoint| Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout,
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 40_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61],
        }]
        .into(),
        lock_time: 0,
    };
    let left = spend(first);
    let right = spend(second);
    assert!(mempool.add_transaction(left.clone()).unwrap());
    assert!(mempool.add_transaction(right.clone()).unwrap());
    assert_eq!(mempool.spender_of(&first), Some(calculate_tx_id(&left)));
    assert_eq!(mempool.spender_of(&second), Some(calculate_tx_id(&right)));
}

#[tokio::test]
async fn busy_utxo_lock_still_installs_the_reorg_snapshot() {
    use blvm_protocol::opcodes::OP_1;
    use std::sync::Arc;
    let mempool = MempoolManager::new();
    let old = funded(vec![OP_1], 50_000);
    let arc = Arc::new(tokio::sync::Mutex::new(old));
    mempool.set_utxo_set_arc(Arc::clone(&arc));
    let _held = arc.lock().await;
    let fresh = OutPoint {
        hash: [9u8; 32],
        index: 0,
    };
    let mut next = blvm_protocol::UtxoSet::default();
    next.insert(
        fresh,
        Arc::new(blvm_protocol::UTXO {
            value: 1,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 1,
            is_coinbase: false,
        }),
    );
    mempool.apply_reorg(&[], &[], &[], &next);
    let slot = mempool
        .utxo_set_arc
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let current = slot.as_ref().expect("snapshot stays wired");
    assert!(!Arc::ptr_eq(current, &arc));
    let guard = current.try_lock().expect("installed snapshot is free");
    assert!(guard.get(&fresh).is_some());
}

#[tokio::test]
async fn package_admission_refuses_a_busy_utxo_lock() {
    use std::sync::Arc;

    let mempool = MempoolManager::new();
    let shared = Arc::new(tokio::sync::Mutex::new(blvm_protocol::UtxoSet::default()));
    mempool.set_utxo_set_arc(Arc::clone(&shared));
    let _held = shared.lock().await;
    assert!(!mempool.add_package(vec![legacy_empty_script()]).unwrap());
    assert_eq!(mempool.size(), 0);
}

#[test]
fn mempool_rejects_consensus_invalid_transactions() {
    use blvm_protocol::opcodes::{OP_0, OP_1};

    let mempool = MempoolManager::new();
    let mut negative = legacy_empty_script();
    negative.outputs[0].value = -1;
    assert!(!mempool.add_transaction(negative).unwrap());

    let mempool = MempoolManager::new();
    let mut coinbase = legacy_empty_script();
    coinbase.inputs[0].prevout.hash = [0u8; 32];
    coinbase.inputs[0].prevout.index = 0xffff_ffff;
    coinbase.inputs[0]
        .script_sig
        .extend_from_slice(&[OP_1, OP_1]);
    assert!(!mempool.add_transaction(coinbase).unwrap());

    let mempool = MempoolManager::new();
    let tx = legacy_empty_script();
    assert!(
        !mempool
            .add_transaction_with_witness(tx, Some(Vec::new()))
            .unwrap()
    );

    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_0],
        2_000,
    ))));
    assert!(!mempool.add_transaction(legacy_empty_script()).unwrap());

    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_1],
        2_000,
    ))));
    assert!(mempool.add_transaction(legacy_empty_script()).unwrap());
}

#[test]
fn an_empty_chain_set_admits_a_snapshot_coin_and_stays_empty() {
    use std::sync::Arc;

    use blvm_protocol::opcodes::OP_1;

    let mempool = MempoolManager::new();
    let shared = Arc::new(tokio::sync::Mutex::new(blvm_protocol::UtxoSet::default()));
    mempool.set_utxo_set_arc(Arc::clone(&shared));
    let tx = legacy_empty_script();
    let prevout = tx.inputs[0].prevout;
    let coin = funded(vec![OP_1], 2_000)
        .get(&prevout)
        .unwrap()
        .as_ref()
        .clone();
    mempool.set_snapshot_prevout_lookup(Arc::new(move |outpoint| {
        if *outpoint == prevout {
            Some(coin.clone())
        } else {
            None
        }
    }));

    assert!(mempool.add_transaction(tx).unwrap());
    let guard = shared.try_lock().expect("shared set");
    assert!(guard.is_empty());
}

#[test]
fn a_missing_snapshot_coin_is_still_rejected() {
    use std::sync::Arc;

    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(
        blvm_protocol::UtxoSet::default(),
    )));
    mempool.set_snapshot_prevout_lookup(Arc::new(|_| None));
    assert!(!mempool.add_transaction(legacy_empty_script()).unwrap());
}

#[test]
fn a_chainstate_coin_is_admitted_when_no_snapshot_exists_and_the_shared_set_stays_empty() {
    use std::sync::Arc;

    use blvm_protocol::opcodes::OP_1;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(crate::storage::Storage::new(temp.path()).unwrap());
    let tx = legacy_empty_script();
    let prevout = tx.inputs[0].prevout;
    let coin = funded(vec![OP_1], 2_000)
        .get(&prevout)
        .unwrap()
        .as_ref()
        .clone();
    storage.utxos().add_utxo(&prevout, &coin).unwrap();

    let mempool = MempoolManager::new();
    let shared = Arc::new(tokio::sync::Mutex::new(blvm_protocol::UtxoSet::default()));
    mempool.set_utxo_set_arc(Arc::clone(&shared));
    mempool.set_storage(Arc::clone(&storage));

    assert!(mempool.add_transaction(tx).unwrap());
    assert!(shared.try_lock().expect("shared set").is_empty());
}

#[test]
fn a_snapshot_miss_does_not_read_the_chainstate_table() {
    use std::sync::Arc;

    use blvm_protocol::opcodes::OP_1;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(crate::storage::Storage::new(temp.path()).unwrap());
    let tx = legacy_empty_script();
    let prevout = tx.inputs[0].prevout;
    let coin = funded(vec![OP_1], 2_000)
        .get(&prevout)
        .unwrap()
        .as_ref()
        .clone();
    storage.utxos().add_utxo(&prevout, &coin).unwrap();

    let other = OutPoint {
        hash: [9u8; 32],
        index: 0,
    };
    let tree_name = crate::storage::ibd_engine::ckpt_tree_for_slot(0);
    let tree = storage.open_tree(tree_name).unwrap();
    let key = crate::storage::disk_utxo::outpoint_to_key(&other);
    let bytes =
        crate::storage::utxo_value_codec::encode_utxo_with_codec(storage.utxo_value_codec(), &coin)
            .unwrap();
    tree.insert(&key, &bytes).unwrap();
    assert!(storage.open_engine_resume_snapshot().unwrap().is_some());

    let mempool = MempoolManager::new();
    let shared = Arc::new(tokio::sync::Mutex::new(blvm_protocol::UtxoSet::default()));
    mempool.set_utxo_set_arc(Arc::clone(&shared));
    mempool.set_storage(Arc::clone(&storage));

    assert!(!mempool.add_transaction(tx).unwrap());
    assert!(shared.try_lock().expect("shared set").is_empty());
}

#[test]
fn mempool_cap_counts_witness_bytes_and_rejects_at_insert() {
    use std::sync::Arc;

    use crate::config::MempoolPolicyConfig;
    use blvm_protocol::opcodes::{OP_0, OP_1, PUSH_32_BYTES};
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    let witness_script = vec![OP_1];
    let script_hash = Sha256::digest(&witness_script);
    let mut script_pubkey = vec![OP_0, PUSH_32_BYTES];
    script_pubkey.extend_from_slice(&script_hash);
    let witness: Vec<Witness> = vec![vec![witness_script.clone()]];

    let mut tx = legacy_empty_script();
    tx.outputs[0].value = 50_000;
    tx.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];

    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded(
        script_pubkey.clone(),
        100_000,
    ))));
    let mut policy = MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.max_mempool_mb = 1;
    mempool.set_policy_config(Some(policy));
    assert!(
        mempool
            .add_transaction_with_witness(tx.clone(), Some(witness))
            .unwrap()
    );
    let base = serialize_transaction(&tx).len() as u64;
    assert!(mempool.mempool_size_bytes() >= base + witness_script.len() as u64);

    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded(
        script_pubkey,
        100_000,
    ))));
    let mut policy = MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.max_mempool_mb = 1;
    mempool.set_policy_config(Some(policy));
    let big: Vec<Witness> = vec![vec![witness_script, vec![7u8; 1_100_000]]];
    assert!(!mempool.add_transaction_with_witness(tx, Some(big)).unwrap());
    assert_eq!(mempool.size(), 0);
}

#[test]
fn admission_height_is_the_tip() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    let mempool = MempoolManager::new();
    mempool.set_chain_tip(800_001, 0);
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_1],
        50_000,
    ))));
    let tx = legacy_empty_script();
    assert!(mempool.add_transaction(tx.clone()).unwrap());
    assert_eq!(mempool.admission_height(&calculate_tx_id(&tx)), 800_000);
}

#[test]
fn estimator_sees_admission_removal_and_one_block_in_order() {
    use crate::node::fee_estimator::{FeeEstimator, HookEvent};
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    use std::sync::Arc;

    let mempool = MempoolManager::new();
    let estimator = Arc::new(FeeEstimator::new(
        crate::config::FeeEstimatorConfig::default(),
        true,
    ));
    estimator.enable_hook_trace();
    mempool.set_fee_estimator(Some(Arc::clone(&estimator)));
    mempool.set_chain_tip(2, 0);
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_1],
        50_000,
    ))));

    let confirmed = legacy_empty_script();
    let confirmed_id = calculate_tx_id(&confirmed);
    assert!(mempool.add_transaction(confirmed.clone()).unwrap());

    mempool.remove_for_connected_block_at(
        Some(crate::node::fee_estimator::BlockObservation {
            height: 1,
            hash: [1u8; 32],
            weight: 1_000,
            tx_weights: vec![(confirmed_id, 400)],
        }),
        &[confirmed],
    );
    mempool.remove_for_connected_block_at(
        Some(crate::node::fee_estimator::BlockObservation {
            height: 1,
            hash: [1u8; 32],
            weight: 1_000,
            tx_weights: Vec::new(),
        }),
        &[],
    );

    let mut evicted = legacy_empty_script();
    evicted.inputs[0].prevout.hash = [4u8; 32];
    let evicted_id = calculate_tx_id(&evicted);
    let mut coins = funded(vec![OP_1], 50_000);
    coins.insert(
        evicted.inputs[0].prevout,
        Arc::new(blvm_protocol::UTXO {
            value: 50_000,
            script_pubkey: vec![OP_1].into(),
            height: 1,
            is_coinbase: false,
        }),
    );
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(coins)));
    assert!(mempool.add_transaction(evicted).unwrap());
    assert!(mempool.remove_transaction(&evicted_id));
    mempool.clear();

    let events = estimator.hook_events();
    let block_at = events
        .iter()
        .position(|event| matches!(event, HookEvent::Block { height: 1 }))
        .expect("one block observation");
    let confirmed_removed = events
        .iter()
        .position(|event| matches!(event, HookEvent::Removed { txid } if *txid == confirmed_id))
        .expect("confirmed transaction removed");
    assert!(block_at < confirmed_removed);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, HookEvent::Block { .. }))
            .count(),
        1
    );
    let evicted_admitted = events
        .iter()
        .rposition(|event| {
            matches!(
                event,
                HookEvent::Admitted {
                    txid,
                    from_reload: false,
                    from_package: false,
                } if *txid == evicted_id
            )
        })
        .expect("evicted admission");
    let evicted_removed = events
        .iter()
        .rposition(|event| matches!(event, HookEvent::Removed { txid } if *txid == evicted_id))
        .expect("evicted removal");
    assert!(evicted_admitted < evicted_removed);
    assert!(
        events[evicted_admitted + 1..evicted_removed]
            .iter()
            .all(|event| !matches!(event, HookEvent::Block { .. }))
    );
    assert!(matches!(events.last(), Some(HookEvent::Cleared)));
    assert_eq!(estimator.hook_counts().blocks, 1);
    assert_eq!(estimator.hook_counts().ignored_blocks, 1);
}

#[test]
fn reload_and_package_admissions_are_flagged() {
    use crate::node::fee_estimator::{FeeEstimator, HookEvent};
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    use std::sync::Arc;

    let mut tx = legacy_empty_script();
    tx.inputs[0].prevout.hash = [8u8; 32];
    let txid = calculate_tx_id(&tx);
    let coins = {
        let mut set = blvm_protocol::UtxoSet::default();
        set.insert(
            tx.inputs[0].prevout,
            Arc::new(blvm_protocol::UTXO {
                value: 50_000,
                script_pubkey: vec![OP_1].into(),
                height: 1,
                is_coinbase: false,
            }),
        );
        set
    };

    let source = MempoolManager::new();
    source.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(coins.clone())));
    assert!(source.add_transaction(tx.clone()).unwrap());
    let dir = std::env::temp_dir().join(format!(
        "blvm-fee-estimator-{}-{}",
        std::process::id(),
        txid[0]
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("mempool.dat");
    source.save_to_disk(&path).unwrap();

    let loaded = MempoolManager::new();
    let estimator = Arc::new(FeeEstimator::new(
        crate::config::FeeEstimatorConfig::default(),
        true,
    ));
    estimator.enable_hook_trace();
    loaded.set_fee_estimator(Some(Arc::clone(&estimator)));
    loaded.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(coins.clone())));
    loaded.load_from_disk(&path).unwrap();
    assert!(matches!(
        estimator.hook_events().last(),
        Some(HookEvent::Admitted {
            txid: id,
            from_reload: true,
            from_package: false,
        }) if *id == txid
    ));

    let packaged = MempoolManager::new();
    let package_estimator = Arc::new(FeeEstimator::new(
        crate::config::FeeEstimatorConfig::default(),
        true,
    ));
    package_estimator.enable_hook_trace();
    packaged.set_fee_estimator(Some(package_estimator.clone()));
    packaged.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(coins)));
    assert!(packaged.add_package(vec![tx]).unwrap());
    assert!(matches!(
        package_estimator.hook_events().last(),
        Some(HookEvent::Admitted {
            txid: id,
            from_reload: false,
            from_package: true,
        }) if *id == txid
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn witness_count_mismatch_is_not_admitted() {
    use blvm_protocol::block::calculate_tx_id;
    use std::sync::Arc;

    let mempool = MempoolManager::new();
    let first = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let second = OutPoint {
        hash: [2u8; 32],
        index: 0,
    };
    let mut set = blvm_protocol::UtxoSet::default();
    for prevout in [first, second] {
        set.insert(
            prevout,
            Arc::new(blvm_protocol::UTXO {
                value: 50_000,
                script_pubkey: vec![blvm_protocol::opcodes::OP_1].into(),
                height: 1,
                is_coinbase: false,
            }),
        );
    }
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
    let tx = Transaction {
        version: 1,
        inputs: vec![
            TransactionInput {
                prevout: first,
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            },
            TransactionInput {
                prevout: second,
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            },
        ]
        .into(),
        outputs: vec![TransactionOutput {
            value: 40_000,
            script_pubkey: vec![blvm_protocol::opcodes::OP_1].into(),
        }]
        .into(),
        lock_time: 0,
    };
    assert!(
        !mempool
            .add_transaction_with_witness(tx.clone(), Some(vec![vec![]]))
            .unwrap()
    );
    assert!(mempool.get_transaction(&calculate_tx_id(&tx)).is_none());
}

#[test]
fn pooled_fee_basis_is_the_txid_and_the_cached_rate() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;

    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_1],
        50_000,
    ))));
    let tx = legacy_empty_script();
    assert!(mempool.add_transaction(tx.clone()).unwrap());
    let txid = calculate_tx_id(&tx);
    let rows = mempool.pooled_fee_basis();
    assert_eq!(rows, vec![(txid, mempool.cached_fee_basis(&txid))]);
}

#[test]
fn missing_prevout_is_not_ranked_at_zero() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    use std::sync::Arc;

    let first = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let second = OutPoint {
        hash: [2u8; 32],
        index: 0,
    };
    let mut set = blvm_protocol::UtxoSet::default();
    for prevout in [first, second] {
        set.insert(
            prevout,
            Arc::new(blvm_protocol::UTXO {
                value: 40_000,
                script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
                height: 1,
                is_coinbase: false,
            }),
        );
    }
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set.clone())));
    let tx = Transaction {
        version: 1,
        inputs: vec![
            TransactionInput {
                prevout: first,
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            },
            TransactionInput {
                prevout: second,
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            },
        ]
        .into(),
        outputs: vec![TransactionOutput {
            value: 49_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
        }]
        .into(),
        lock_time: 0,
    };
    assert!(mempool.add_transaction(tx.clone()).unwrap());
    let txid = calculate_tx_id(&tx);
    set.remove(&second);
    let ranked = mempool.get_prioritized_transactions(10, &set);
    assert!(
        ranked
            .iter()
            .all(|ranked_tx| calculate_tx_id(ranked_tx) != txid)
    );
    assert!(mempool.cached_fee_basis(&txid).is_none());
}

#[test]
fn a_value_outside_a_ten_entry_sample_rebuilds_the_fee() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    use std::sync::Arc;

    let mut set = blvm_protocol::UtxoSet::default();
    for i in 0..11u8 {
        set.insert(
            OutPoint {
                hash: [i; 32],
                index: 0,
            },
            Arc::new(blvm_protocol::UTXO {
                value: 50_000,
                script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
                height: 1,
                is_coinbase: false,
            }),
        );
    }
    let base = MempoolManager::calculate_utxo_set_hash(&set);
    for i in 0..11u8 {
        let mut changed = set.clone();
        let op = OutPoint {
            hash: [i; 32],
            index: 0,
        };
        changed.insert(
            op,
            Arc::new(blvm_protocol::UTXO {
                value: 50_001,
                script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
                height: 1,
                is_coinbase: false,
            }),
        );
        assert_ne!(MempoolManager::calculate_utxo_set_hash(&changed), base);
    }

    let spend = OutPoint {
        hash: [10; 32],
        index: 0,
    };
    let tx = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: spend,
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 40_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
        }]
        .into(),
        lock_time: 0,
    };
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set.clone())));
    assert!(mempool.add_transaction(tx.clone()).unwrap());
    let txid = calculate_tx_id(&tx);
    mempool.get_prioritized_transactions(1, &set);
    let before = mempool.cached_fee_basis(&txid).unwrap().0;
    set.insert(
        spend,
        Arc::new(blvm_protocol::UTXO {
            value: 80_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 1,
            is_coinbase: false,
        }),
    );
    mempool.get_prioritized_transactions(1, &set);
    let after = mempool.cached_fee_basis(&txid).unwrap().0;
    assert!(after > before, "before {before} after {after}");
}

fn child_of(parent: OutPoint, value: i64) -> Transaction {
    Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: parent,
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
        }]
        .into(),
        lock_time: 0,
    }
}

#[test]
fn twenty_five_children_are_admitted_and_the_twenty_sixth_is_not() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    use std::sync::Arc;

    let funding = OutPoint {
        hash: [9u8; 32],
        index: 0,
    };
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    utxo_set.insert(
        funding,
        Arc::new(blvm_protocol::UTXO {
            value: 2_000_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));

    let parent = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: funding,
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: (0..26)
            .map(|_| TransactionOutput {
                value: 3_000,
                script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            })
            .collect::<Vec<_>>()
            .into(),
        lock_time: 0,
    };
    let parent_hash = calculate_tx_id(&parent);
    assert!(mempool.add_transaction(parent).unwrap());

    for index in 0..24 {
        let child = child_of(
            OutPoint {
                hash: parent_hash,
                index,
            },
            1_000,
        );
        assert!(
            mempool.add_transaction(child).unwrap(),
            "child {index} should be admitted"
        );
    }
    let rejected = child_of(
        OutPoint {
            hash: parent_hash,
            index: 24,
        },
        1_000,
    );
    assert!(!mempool.add_transaction(rejected).unwrap());
    assert_eq!(mempool.size(), 25);
}

#[test]
fn parent_vsize_counts_toward_descendant_size() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    use std::sync::Arc;

    let funding = OutPoint {
        hash: [4u8; 32],
        index: 0,
    };
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    utxo_set.insert(
        funding,
        Arc::new(blvm_protocol::UTXO {
            value: 2_000_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set.clone())));

    let parent = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: funding,
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 50_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
        }]
        .into(),
        lock_time: 0,
    };
    let parent_hash = calculate_tx_id(&parent);
    assert!(mempool.add_transaction(parent).unwrap());
    let parent_vsize = mempool
        .pool_lock()
        .adjusted_vsize
        .get(&parent_hash)
        .copied()
        .unwrap();
    let child = child_of(
        OutPoint {
            hash: parent_hash,
            index: 0,
        },
        1_000,
    );
    let (child_vsize, _) = mempool.admit_vsize(&child, None, &utxo_set);
    let mut policy = crate::config::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.max_descendant_size = parent_vsize + child_vsize - 1;
    mempool.set_policy_config(Some(policy));
    assert!(!mempool.add_transaction(child).unwrap());
    assert_eq!(mempool.size(), 1);
}

#[test]
fn child_is_rejected_when_parent_already_has_twenty_five_descendants() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    use std::sync::Arc;

    let funding = OutPoint {
        hash: [8u8; 32],
        index: 0,
    };
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    utxo_set.insert(
        funding,
        Arc::new(blvm_protocol::UTXO {
            value: 2_000_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));

    let parent = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: funding,
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: (0..25)
            .map(|_| TransactionOutput {
                value: 4_000,
                script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            })
            .collect::<Vec<_>>()
            .into(),
        lock_time: 0,
    };
    let parent_hash = calculate_tx_id(&parent);
    assert!(mempool.add_transaction(parent).unwrap());

    let child_with_room = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: parent_hash,
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 3_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
        }]
        .into(),
        lock_time: 0,
    };
    let child_hash = calculate_tx_id(&child_with_room);
    assert!(mempool.add_transaction(child_with_room).unwrap());
    let grandchild = child_of(
        OutPoint {
            hash: child_hash,
            index: 0,
        },
        1_000,
    );
    assert!(mempool.add_transaction(grandchild).unwrap());

    for index in 1..23 {
        let child = child_of(
            OutPoint {
                hash: parent_hash,
                index,
            },
            1_000,
        );
        assert!(mempool.add_transaction(child).unwrap(), "child {index}");
    }
    // Parent plus 24 descendants is the package limit of 25.
    let rejected = child_of(
        OutPoint {
            hash: parent_hash,
            index: 23,
        },
        1_000,
    );
    assert!(!mempool.add_transaction(rejected).unwrap());
}

#[test]
fn mempool_rejects_a_prevout_value_outside_money_range() {
    use blvm_protocol::constants::MAX_MONEY;
    use blvm_protocol::opcodes::OP_1;

    let tx = legacy_empty_script();

    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_1],
        -1,
    ))));
    assert!(!mempool.add_transaction(tx.clone()).unwrap());

    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_1],
        MAX_MONEY + 1,
    ))));
    assert!(!mempool.add_transaction(tx.clone()).unwrap());

    let mut at_cap = tx;
    at_cap.outputs[0].value = MAX_MONEY - 1_000;
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_1],
        MAX_MONEY,
    ))));
    assert!(mempool.add_transaction(at_cap).unwrap());
}

#[test]
fn dust_floor_and_minimal_push_reject_at_admission() {
    const OP_0: u8 = 0x00;
    const OP_1: u8 = 0x51;
    const OP_DROP: u8 = 0x75;
    const OP_RETURN: u8 = 0x6a;

    let mut policy = crate::config::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_relay_fee_rate = 0;
    policy.min_tx_fee = 0;
    policy.reject_spam_in_mempool = false;

    let mut program = vec![OP_0, 20];
    program.extend_from_slice(&[0u8; 20]);

    let spend_to = |value: i64, script: Vec<u8>, script_sig: Vec<u8>| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = value;
        tx.outputs[0].script_pubkey = script;
        tx
    };
    let admit = |tx: Transaction, prevout_script: Vec<u8>| {
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(policy.clone()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout_script,
            100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };

    assert!(!admit(
        spend_to(293, program.clone(), Vec::new()),
        vec![OP_1]
    ));
    assert!(admit(spend_to(294, program, Vec::new()), vec![OP_1]));
    assert!(admit(
        spend_to(1, vec![OP_RETURN, 0x03, 0x00, 0x00, 0x00], Vec::new()),
        vec![OP_1]
    ));

    let drop_true = vec![OP_DROP, OP_1];
    assert!(!admit(
        spend_to(
            1_000,
            vec![0x51, 0x61, 0x61, 0x61, 0x61],
            vec![0x4c, 0x01, 0x51]
        ),
        drop_true.clone()
    ));
    assert!(admit(
        spend_to(1_000, vec![0x51, 0x61, 0x61, 0x61, 0x61], vec![0x01, 0x51]),
        drop_true
    ));
}

#[test]
fn op_return_of_84_bytes_is_standard() {
    let carrier = |payload: usize| {
        let mut s = vec![0x6a, 0x4c, payload as u8];
        s.extend(std::iter::repeat(0u8).take(payload));
        s
    };
    let admit = |script: Vec<u8>| {
        let (prevout, script_sig) = p2sh_of_op_1();
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = script;
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let eighty_three = admit(carrier(80));
    let eighty_four = admit(carrier(81));
    let mut not_carrier = carrier(81);
    not_carrier[0] = 0x00;
    let not_return = admit(not_carrier);
    eprintln!(
        "83-byte op_return admitted={eighty_three}; 84-byte op_return admitted={eighty_four}; 84-byte non-return admitted={not_return}"
    );
    assert!(eighty_three);
    assert!(eighty_four);
    assert!(!not_return);
}

#[test]
fn op_return_with_a_non_push_is_not_standard() {
    let admit = |script: Vec<u8>| {
        let (prevout, script_sig) = p2sh_of_op_1();
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = script;
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let push = admit(vec![0x6a, 0x03, 0x00, 0x00, 0x00]);
    let nop = admit(vec![0x6a, 0x61, 0x00, 0x00, 0x00]);
    eprintln!("op_return push admitted={push}; op_return nop admitted={nop}");
    assert!(push);
    assert!(!nop);
}

#[test]
fn p2sh_redeem_with_sixteen_sigops_is_not_standard() {
    use blvm_protocol::opcodes::{OP_0, OP_1, OP_CHECKSIG, OP_ENDIF, OP_EQUAL, OP_HASH160, OP_IF};
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    fn hash160(data: &[u8]) -> [u8; 20] {
        Ripemd160::digest(Sha256::digest(data)).into()
    }

    let admit = |sigops: usize| {
        let mut redeem = vec![OP_0, OP_IF];
        redeem.extend(std::iter::repeat(OP_CHECKSIG).take(sigops));
        redeem.extend_from_slice(&[OP_ENDIF, OP_1]);
        let hash = hash160(&redeem);
        let mut prev = vec![OP_HASH160, 0x14];
        prev.extend_from_slice(&hash);
        prev.push(OP_EQUAL);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = {
            let mut script_sig = Vec::with_capacity(redeem.len() + 1);
            script_sig.push(redeem.len() as u8);
            script_sig.extend_from_slice(&redeem);
            script_sig
        };
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prev, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let fifteen = admit(15);
    let sixteen = admit(16);
    eprintln!("15 sigops admitted={fifteen}; 16 sigops admitted={sixteen}");
    assert!(fifteen);
    assert!(!sixteen);
}

#[test]
fn undefined_witness_program_spend_is_not_standard() {
    use blvm_protocol::opcodes::{OP_0, OP_1, OP_2};
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    let program: [u8; 32] = Sha256::digest([OP_1]).into();
    let script = |version: u8| {
        let mut out = vec![version, 0x20];
        out.extend_from_slice(&program);
        out
    };
    let admit = |prevout: Vec<u8>| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![vec![vec![OP_1]]];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let version_zero = admit(script(OP_0));
    let version_two = admit(script(OP_2));
    eprintln!("v0 p2wsh admitted={version_zero}; v2 witness program admitted={version_two}");
    assert!(version_zero);
    assert!(!version_two);
}

#[test]
fn p2wsh_data_item_of_81_bytes_is_not_standard() {
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    const OP_0: u8 = 0x00;
    const OP_1: u8 = 0x51;
    const OP_DROP: u8 = 0x75;
    let script = vec![OP_DROP, OP_1];
    let program: [u8; 32] = Sha256::digest(&script).into();
    let mut prevout = vec![OP_0, 0x20];
    prevout.extend_from_slice(&program);
    let admit = |item_len: usize| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![vec![vec![0u8; item_len], script.clone()]];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let eighty = admit(80);
    let eighty_one = admit(81);
    eprintln!("80-byte p2wsh item admitted={eighty}; 81-byte p2wsh item admitted={eighty_one}");
    assert!(eighty);
    assert!(!eighty_one);
}

#[test]
fn wrapped_p2wsh_data_item_of_81_bytes_is_not_standard() {
    use blvm_protocol::segwit::Witness;
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let script = vec![0x75, 0x51];
    let program: [u8; 32] = Sha256::digest(&script).into();
    let mut redeem = vec![0x00, 0x20];
    redeem.extend_from_slice(&program);
    let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
    let mut prevout = vec![0xa9, 0x14];
    prevout.extend_from_slice(&hash);
    prevout.push(0x87);
    let mut script_sig = vec![redeem.len() as u8];
    script_sig.extend_from_slice(&redeem);
    let admit = |item_len: usize| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig.clone();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![vec![vec![0u8; item_len], script.clone()]];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let eighty = admit(80);
    let eighty_one = admit(81);
    eprintln!("wrapped 80-byte item admitted={eighty}; wrapped 81-byte item admitted={eighty_one}");
    assert!(eighty);
    assert!(!eighty_one);
}

#[test]
fn p2wsh_script_of_3601_bytes_is_not_standard() {
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    const OP_0: u8 = 0x00;
    const OP_1: u8 = 0x51;
    const OP_DROP: u8 = 0x75;
    let script_of = |script_len: usize| {
        let mut script = Vec::with_capacity(script_len);
        let mut left = script_len - 1;
        while left > 524 {
            script.push(0x4d);
            script.extend_from_slice(&520u16.to_le_bytes());
            script.extend(std::iter::repeat(0u8).take(520));
            script.push(OP_DROP);
            left -= 524;
        }
        let payload = left - 4;
        script.push(0x4d);
        script.extend_from_slice(&(payload as u16).to_le_bytes());
        script.extend(std::iter::repeat(0u8).take(payload));
        script.push(OP_DROP);
        script.push(OP_1);
        script
    };
    let admit = |script_len: usize| {
        let script = script_of(script_len);
        let program: [u8; 32] = Sha256::digest(&script).into();
        let mut prevout = vec![OP_0, 0x20];
        prevout.extend_from_slice(&program);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![vec![script]];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let at_limit = admit(3_600);
    let over = admit(3_601);
    eprintln!("3600-byte p2wsh script admitted={at_limit}; 3601-byte p2wsh script admitted={over}");
    assert!(at_limit);
    assert!(!over);
}

#[test]
fn wrapped_p2wsh_script_of_3601_bytes_is_not_standard() {
    use blvm_protocol::segwit::Witness;
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    const OP_1: u8 = 0x51;
    const OP_DROP: u8 = 0x75;
    let script_of = |script_len: usize| {
        let mut script = Vec::with_capacity(script_len);
        let mut left = script_len - 1;
        while left > 524 {
            script.push(0x4d);
            script.extend_from_slice(&520u16.to_le_bytes());
            script.extend(std::iter::repeat(0u8).take(520));
            script.push(OP_DROP);
            left -= 524;
        }
        let payload = left - 4;
        script.push(0x4d);
        script.extend_from_slice(&(payload as u16).to_le_bytes());
        script.extend(std::iter::repeat(0u8).take(payload));
        script.push(OP_DROP);
        script.push(OP_1);
        script
    };
    let admit = |script_len: usize| {
        let script = script_of(script_len);
        let program: [u8; 32] = Sha256::digest(&script).into();
        let mut redeem = vec![0x00, 0x20];
        redeem.extend_from_slice(&program);
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![vec![script]];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let at_limit = admit(3_600);
    let over = admit(3_601);
    eprintln!(
        "wrapped 3600-byte script admitted={at_limit}; wrapped 3601-byte script admitted={over}"
    );
    assert!(at_limit);
    assert!(!over);
}

#[test]
fn p2wsh_stack_of_101_items_is_not_standard() {
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    const OP_0: u8 = 0x00;
    const OP_1: u8 = 0x51;
    const OP_DROP: u8 = 0x75;
    let admit = |items: usize| {
        let mut script = vec![OP_DROP; items];
        script.push(OP_1);
        let program: [u8; 32] = Sha256::digest(&script).into();
        let mut prevout = vec![OP_0, 0x20];
        prevout.extend_from_slice(&program);
        let mut stack = vec![Vec::new(); items];
        stack.push(script);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![stack];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let hundred = admit(100);
    let hundred_one = admit(101);
    eprintln!("100 p2wsh items admitted={hundred}; 101 p2wsh items admitted={hundred_one}");
    assert!(hundred);
    assert!(!hundred_one);
}

#[test]
fn wrapped_p2wsh_stack_of_101_items_is_not_standard() {
    use blvm_protocol::segwit::Witness;
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |items: usize| {
        let mut script = vec![0x75; items];
        script.push(0x51);
        let program: [u8; 32] = Sha256::digest(&script).into();
        let mut redeem = vec![0x00, 0x20];
        redeem.extend_from_slice(&program);
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut stack = vec![Vec::new(); items];
        stack.push(script);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![stack];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let hundred = admit(100);
    let hundred_one = admit(101);
    eprintln!("wrapped 100 items admitted={hundred}; wrapped 101 items admitted={hundred_one}");
    assert!(hundred);
    assert!(!hundred_one);
}

#[test]
fn nonminimal_script_number_in_add_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x00, 0x93]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x00, 0x93]);
    eprintln!("minimal add admitted={minimal}; padded add admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_sub_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x00, 0x94]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x00, 0x94]);
    eprintln!("minimal sub admitted={minimal}; padded sub admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_booland_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x51, 0x9a]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x51, 0x9a]);
    eprintln!("minimal booland admitted={minimal}; padded booland admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_boolor_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x51, 0x9b]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x51, 0x9b]);
    eprintln!("minimal boolor admitted={minimal}; padded boolor admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_numequal_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x51, 0x9c]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x51, 0x9c]);
    eprintln!("minimal numequal admitted={minimal}; padded numequal admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_numequalverify_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x51, 0x9d, 0x51]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x51, 0x9d, 0x51]);
    eprintln!("minimal numequalverify admitted={minimal}; padded numequalverify admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_numnotequal_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x00, 0x9e]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x00, 0x9e]);
    eprintln!("minimal numnotequal admitted={minimal}; padded numnotequal admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_lessthan_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x00, 0x51, 0x9f]);
    let padded = admit(vec![0x00, 0x02, 0x01, 0x00, 0x9f]);
    eprintln!("minimal lessthan admitted={minimal}; padded lessthan admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_greaterthan_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x00, 0xa0]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x00, 0xa0]);
    eprintln!("minimal greaterthan admitted={minimal}; padded greaterthan admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_lessthanorequal_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x00, 0x51, 0xa1]);
    let padded = admit(vec![0x00, 0x02, 0x01, 0x00, 0xa1]);
    eprintln!(
        "minimal lessthanorequal admitted={minimal}; padded lessthanorequal admitted={padded}"
    );
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_greaterthanorequal_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x00, 0xa2]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x00, 0xa2]);
    eprintln!(
        "minimal greaterthanorequal admitted={minimal}; padded greaterthanorequal admitted={padded}"
    );
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_min_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x51, 0xa3]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x51, 0xa3]);
    eprintln!("minimal min admitted={minimal}; padded min admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_max_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x51, 0xa4]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x51, 0xa4]);
    eprintln!("minimal max admitted={minimal}; padded max admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_within_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x00, 0x52, 0xa5]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x00, 0x52, 0xa5]);
    eprintln!("minimal within admitted={minimal}; padded within admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_1sub_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x52, 0x8c]);
    let padded = admit(vec![0x02, 0x02, 0x00, 0x8c]);
    eprintln!("minimal 1sub admitted={minimal}; padded 1sub admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_negate_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x8f]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x8f]);
    eprintln!("minimal negate admitted={minimal}; padded negate admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_abs_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x90]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x90]);
    eprintln!("minimal abs admitted={minimal}; padded abs admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_not_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x00, 0x91]);
    let padded = admit(vec![0x02, 0x00, 0x00, 0x91]);
    eprintln!("minimal not admitted={minimal}; padded not admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_0notequal_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x92]);
    let padded = admit(vec![0x02, 0x01, 0x00, 0x92]);
    eprintln!("minimal 0notequal admitted={minimal}; padded 0notequal admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_pick_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x00, 0x79, 0x75]);
    let padded = admit(vec![0x51, 0x02, 0x00, 0x00, 0x79, 0x75]);
    eprintln!("minimal pick admitted={minimal}; padded pick admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_roll_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let minimal = admit(vec![0x51, 0x00, 0x7a]);
    let padded = admit(vec![0x51, 0x02, 0x00, 0x00, 0x7a]);
    eprintln!("minimal roll admitted={minimal}; padded roll admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_checkmultisig_is_not_standard() {
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![0xa9, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(0x87);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let pubkey = {
        let mut key = vec![0x02];
        key.extend(std::iter::repeat(0u8).take(32));
        key
    };
    let with_pubkey = |head: &[u8]| {
        let mut redeem = head.to_vec();
        redeem.push(0x21);
        redeem.extend_from_slice(&pubkey);
        redeem.extend_from_slice(&[0x51, 0xae]);
        redeem
    };
    let minimal = admit(with_pubkey(&[0x00, 0x00]));
    let padded = admit(with_pubkey(&[0x00, 0x02, 0x00, 0x00]));
    eprintln!("minimal checkmultisig admitted={minimal}; padded checkmultisig admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_multisig_key_count_is_not_standard() {
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    let pubkey = {
        let mut key = vec![0x02];
        key.extend(std::iter::repeat(0u8).take(32));
        key
    };
    let script_of = |key_count: &[u8]| {
        let mut script = vec![0x00, 0x00, 0x21];
        script.extend_from_slice(&pubkey);
        script.extend_from_slice(key_count);
        script.push(0xae);
        script
    };
    let admit = |script: Vec<u8>| {
        let program: [u8; 32] = Sha256::digest(&script).into();
        let mut prevout = vec![0x00, 0x20];
        prevout.extend_from_slice(&program);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![vec![script]];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let minimal = admit(script_of(&[0x51]));
    let padded = admit(script_of(&[0x02, 0x01, 0x00]));
    eprintln!("minimal key count admitted={minimal}; padded key count admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn nonminimal_script_number_in_checksigadd_is_not_standard() {
    use blvm_consensus::secp256k1_backend::taproot_output_key_with_parity;
    use blvm_consensus::taproot::{TAPROOT_LEAF_VERSION_TAPSCRIPT, compute_script_merkle_root};
    use blvm_protocol::segwit::Witness;

    let internal = [
        0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87, 0x0b,
        0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8,
        0x17, 0x98,
    ];
    let mut tapscript = vec![0x20];
    tapscript.extend_from_slice(&internal);
    tapscript.push(0xba);
    let admit = |number: Vec<u8>| {
        let root =
            compute_script_merkle_root(&tapscript, &[], TAPROOT_LEAF_VERSION_TAPSCRIPT).unwrap();
        let (output_key, parity) = taproot_output_key_with_parity(&internal, &root).unwrap();
        let mut prevout = vec![0x51, 0x20];
        prevout.extend_from_slice(&output_key);
        let mut control = vec![TAPROOT_LEAF_VERSION_TAPSCRIPT | parity];
        control.extend_from_slice(&internal);
        let stack = vec![Vec::new(), number, tapscript.clone(), control];
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![stack];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let minimal = admit(vec![0x01]);
    let padded = admit(vec![0x01, 0x00]);
    eprintln!("minimal checksigadd admitted={minimal}; padded checksigadd admitted={padded}");
    assert!(minimal);
    assert!(!padded);
}

#[test]
fn empty_anchor_is_standard_and_a_witness_is_not() {
    use blvm_protocol::segwit::Witness;

    let prevout = vec![0x51, 0x02, 0x4e, 0x73];
    let admit = |witness: Option<Vec<Witness>>| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            100_000,
        ))));
        mempool.add_transaction_with_witness(tx, witness).unwrap()
    };
    let empty = admit(None);
    let stuffed = admit(Some(vec![vec![vec![0x00]]]));
    eprintln!("empty anchor admitted={empty}; stuffed anchor admitted={stuffed}");
    assert!(empty);
    assert!(!stuffed);
}

#[test]
fn fee_of_999_sats_is_standard() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let admit = |output_value: i64| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig.clone();
        tx.outputs[0].value = output_value;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            1_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let at_thousand = admit(0);
    let at_999 = admit(1);
    eprintln!("fee 1000 admitted={at_thousand}; fee 999 admitted={at_999}");
    assert!(at_thousand);
    assert!(at_999);
}

#[test]
fn relay_fee_of_seven_sats_is_standard() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let mut sample = legacy_empty_script();
    sample.inputs[0].script_sig = script_sig.clone();
    sample.outputs[0].value = 0;
    sample.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
    let vsize = blvm_protocol::wire::serialize_tx(&sample).unwrap().len() as i64;
    let core_fee = vsize * 100 / 1000 + i64::from(vsize * 100 % 1000 != 0);
    let admit = |fee: i64| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig.clone();
        tx.outputs[0].value = 10_000 - fee;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            10_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let at_one = admit(vsize);
    let at_core = admit(core_fee);
    let under = admit(core_fee - 1);
    eprintln!(
        "vsize={vsize}; fee {vsize} admitted={at_one}; fee {core_fee} admitted={at_core}; fee {} admitted={under}",
        core_fee - 1
    );
    assert_eq!(vsize, 67);
    assert_eq!(core_fee, 7);
    assert!(at_one);
    assert!(at_core);
    assert!(!under);
}

#[test]
fn a_second_replacement_is_not_on_a_timer() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let spend = |fee: i64| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig.clone();
        tx.inputs[0].sequence = 0xfffffffd;
        tx.outputs[0].value = 10_000 - fee;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        tx
    };
    let mempool = MempoolManager::new();
    mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
        prevout, 10_000,
    ))));
    let original = mempool.add_transaction(spend(1_000)).unwrap();
    let first = mempool.add_transaction(spend(1_100)).unwrap();
    let second = mempool.add_transaction(spend(1_200)).unwrap();
    eprintln!(
        "original admitted={original}; first replacement admitted={first}; second replacement admitted={second}"
    );
    assert!(original);
    assert!(first);
    assert!(second);
}

#[test]
fn a_final_sequence_can_be_replaced() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let run = |original_sequence: u64| {
        let spend = |fee: i64, sequence: u64| {
            let mut tx = legacy_empty_script();
            tx.inputs[0].script_sig = script_sig.clone();
            tx.inputs[0].sequence = sequence;
            tx.outputs[0].value = 10_000 - fee;
            tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
            tx
        };
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            10_000,
        ))));
        let original = mempool
            .add_transaction(spend(1_000, original_sequence))
            .unwrap();
        let replacement = mempool.add_transaction(spend(1_100, 0xfffffffd)).unwrap();
        (original, replacement)
    };
    let signaling = run(0xfffffffd);
    let final_sequence = run(0xffffffff);
    eprintln!(
        "signaling original={} replacement={}; final original={} replacement={}",
        signaling.0, signaling.1, final_sequence.0, final_sequence.1
    );
    assert!(signaling.0 && signaling.1);
    assert!(final_sequence.0 && final_sequence.1);
}

#[test]
fn replacement_pays_for_every_conflict() {
    use std::sync::Arc;

    let (prevout, script_sig) = p2sh_of_op_1();
    let coin = |value: i64| {
        Arc::new(blvm_protocol::UTXO {
            value,
            script_pubkey: prevout.clone().into(),
            height: 0,
            is_coinbase: false,
        })
    };
    let mut utxos = blvm_protocol::UtxoSet::default();
    for hash in [[1u8; 32], [2u8; 32], [3u8; 32]] {
        utxos.insert(OutPoint { hash, index: 0 }, coin(50_000));
    }
    let spend_one = |hash: [u8; 32]| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].prevout.hash = hash;
        tx.inputs[0].script_sig = script_sig.clone();
        tx.outputs[0].value = 45_000;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        tx
    };
    let mut replacement = legacy_empty_script();
    replacement.inputs[0].prevout.hash = [1u8; 32];
    replacement.inputs[0].script_sig = script_sig.clone();
    replacement.inputs.push(TransactionInput {
        prevout: OutPoint {
            hash: [3u8; 32],
            index: 0,
        },
        script_sig: script_sig.clone(),
        sequence: 0xffffffff,
    });
    replacement.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
    let meter = MempoolManager::new();
    meter.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
    let old_vsize = meter.admit_vsize(&spend_one([1u8; 32]), None, &utxos).0;
    let new_vsize = meter.admit_vsize(&replacement, None, &utxos).0;
    let one_fee = 5_000u128;
    let feerate_floor = one_fee * new_vsize as u128 / old_vsize as u128 + 1;
    let bandwidth_floor = one_fee + (100u128 * new_vsize as u128).div_ceil(1000);
    let fee = feerate_floor.max(bandwidth_floor) as i64;
    let run = |second: [u8; 32]| {
        let mut tx = replacement.clone();
        tx.inputs[1].prevout.hash = second;
        tx.outputs[0].value = 100_000 - fee;
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(utxos.clone())));
        let first = mempool.add_transaction(spend_one([1u8; 32])).unwrap();
        let second_tx = mempool.add_transaction(spend_one([2u8; 32])).unwrap();
        let replaced = mempool.add_transaction(tx).unwrap();
        (first, second_tx, replaced)
    };
    let one = run([3u8; 32]);
    let both = run([2u8; 32]);
    eprintln!(
        "vsize old={old_vsize} new={new_vsize} fee={fee}; one conflict admitted={}; two conflicts admitted={}",
        one.2, both.2
    );
    assert!(fee < 10_000);
    assert!(one.0 && one.1 && one.2);
    assert!(both.0 && both.1 && !both.2);
}

#[test]
fn replacement_may_spend_an_unrelated_mempool_output() {
    use std::sync::Arc;

    let (prevout, script_sig) = p2sh_of_op_1();
    let admit = |also_spend_other: bool| {
        let mut coin_y = legacy_empty_script();
        coin_y.inputs[0].prevout.hash = [2u8; 32];
        let mut utxo = funded(prevout.clone(), 20_000);
        utxo.insert(
            coin_y.inputs[0].prevout,
            Arc::new(blvm_protocol::UTXO {
                value: 20_000,
                script_pubkey: prevout.clone().into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let mut conflict = legacy_empty_script();
        conflict.inputs[0].script_sig = script_sig.clone();
        conflict.outputs[0].value = 10_000;
        conflict.outputs[0].script_pubkey = prevout.clone();
        let mut other = coin_y;
        other.inputs[0].script_sig = script_sig.clone();
        other.outputs[0].value = 15_000;
        other.outputs[0].script_pubkey = prevout.clone();
        let other_id = blvm_protocol::block::calculate_tx_id(&other);
        let mut replacement = legacy_empty_script();
        replacement.inputs[0].script_sig = script_sig.clone();
        replacement.outputs[0].value = 1_000;
        replacement.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        if also_spend_other {
            replacement.inputs.push(TransactionInput {
                prevout: OutPoint {
                    hash: other_id,
                    index: 0,
                },
                script_sig: script_sig.clone(),
                sequence: 0xffffffff,
            });
        }
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo)));
        assert!(mempool.add_transaction(conflict).unwrap());
        assert!(mempool.add_transaction(other).unwrap());
        let admitted = mempool.add_transaction(replacement).unwrap();
        (admitted, mempool.get_transaction(&other_id).is_some())
    };
    let plain = admit(false);
    let with_parent = admit(true);
    eprintln!(
        "plain replacement admitted={}; spends unrelated output admitted={} parent_remains={}",
        plain.0, with_parent.0, with_parent.1
    );
    assert!(plain.0 && plain.1);
    assert!(with_parent.0 && with_parent.1);
}

#[test]
fn version_two_sequence_zero_is_final() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let spend = |sequence: u64| {
        let mut tx = legacy_empty_script();
        tx.version = 2;
        tx.inputs[0].script_sig = script_sig.clone();
        tx.inputs[0].sequence = sequence;
        tx.outputs[0].value = 9_000;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        tx
    };
    let admit = |sequence: u64| {
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            10_000,
        ))));
        mempool.add_transaction(spend(sequence)).unwrap()
    };
    let disabled = admit(0xffffffff);
    let zero = admit(0);
    let one = admit(1);
    eprintln!(
        "sequence disabled admitted={disabled}; sequence 0 admitted={zero}; sequence 1 admitted={one}"
    );
    assert!(disabled && zero);
    assert!(!one);
}

#[test]
fn a_zero_time_lock_is_final() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let spend = |sequence: u64| {
        let mut tx = legacy_empty_script();
        tx.version = 2;
        tx.inputs[0].script_sig = script_sig.clone();
        tx.inputs[0].sequence = sequence;
        tx.outputs[0].value = 9_000;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        tx
    };
    let admit = |sequence: u64| {
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_chain_tip(500_000, 1_600_000_000);
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            10_000,
        ))));
        mempool.add_transaction(spend(sequence)).unwrap()
    };
    let blocks = admit(0);
    let time = admit(0x0040_0000);
    let one_unit = admit(0x0040_0001);
    eprintln!(
        "block lock 0 admitted={blocks}; time lock 0 admitted={time}; time lock 1 admitted={one_unit}"
    );
    assert!(blocks && time);
    assert!(!one_unit);
}

#[test]
fn a_future_locktime_needs_a_tip() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let spend = |lock_time: u64, sequence: u64| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig.clone();
        tx.inputs[0].sequence = sequence;
        tx.lock_time = lock_time;
        tx.outputs[0].value = 9_000;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        tx
    };
    let admit = |lock_time: u64, sequence: u64| {
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            10_000,
        ))));
        mempool.add_transaction(spend(lock_time, sequence)).unwrap()
    };
    let open = admit(0, 0xfffffffe);
    let future = admit(200, 0xfffffffe);
    let ignored = admit(200, 0xffffffff);
    eprintln!(
        "locktime 0 admitted={open}; locktime 200 admitted={future}; final sequence admitted={ignored}"
    );
    assert!(open && ignored);
    assert!(!future);
}

#[test]
fn a_parent_output_above_its_input_is_not_carried() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let admit = |parent_value: i64| {
        let mut parent = legacy_empty_script();
        parent.inputs[0].script_sig = script_sig.clone();
        parent.outputs[0].value = parent_value;
        parent.outputs[0].script_pubkey = prevout.clone();
        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: blvm_protocol::block::calculate_tx_id(&parent),
            index: 0,
        };
        child.inputs[0].script_sig = script_sig.clone();
        child.outputs[0].value = 0;
        child.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let parent_id = blvm_protocol::block::calculate_tx_id(&parent);
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            10_000,
        ))));
        mempool.add_package(vec![parent, child]).unwrap();
        mempool.get_transaction(&parent_id).is_some()
    };
    let covered = admit(10_000);
    let over = admit(10_001);
    eprintln!("output 10000 parent={covered}; output 10001 parent={over}");
    assert!(covered);
    assert!(!over);
}

#[test]
fn child_fee_carries_a_zero_fee_parent() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let pair = |parent_fee: i64| {
        let mut parent = legacy_empty_script();
        parent.inputs[0].script_sig = script_sig.clone();
        parent.outputs[0].value = 10_000 - parent_fee;
        parent.outputs[0].script_pubkey = prevout.clone();
        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: blvm_protocol::block::calculate_tx_id(&parent),
            index: 0,
        };
        child.inputs[0].script_sig = script_sig.clone();
        child.outputs[0].value = 0;
        child.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        (parent, child)
    };
    let admit = |parent_fee: i64| {
        let (parent, child) = pair(parent_fee);
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            10_000,
        ))));
        let parent_id = blvm_protocol::block::calculate_tx_id(&parent);
        let child_id = blvm_protocol::block::calculate_tx_id(&child);
        mempool.add_package(vec![parent, child]).unwrap();
        (
            mempool.get_transaction(&parent_id).is_some(),
            mempool.get_transaction(&child_id).is_some(),
        )
    };
    let (pays, pays_child) = admit(1_000);
    let (zero, zero_child) = admit(0);
    let lone = {
        let (parent, _) = pair(0);
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 10_000,
        ))));
        mempool.add_transaction(parent).unwrap()
    };
    eprintln!(
        "parent fee 1000 admitted={pays}; its child admitted={pays_child}; parent fee 0 admitted={zero}; its child admitted={zero_child}; alone admitted={lone}"
    );
    assert!(pays);
    assert!(pays_child);
    assert!(zero);
    assert!(zero_child);
    assert!(!lone);
}

#[test]
fn zero_fee_parent_may_have_one_dust_output() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let admit = |value: i64, change: i64| {
        let mut parent = legacy_empty_script();
        parent.inputs[0].script_sig = script_sig.clone();
        parent.outputs[0].value = value;
        parent.outputs[0].script_pubkey = prevout.clone();
        parent.outputs.push(TransactionOutput {
            value: change,
            script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
        });
        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: blvm_protocol::block::calculate_tx_id(&parent),
            index: 0,
        };
        child.inputs[0].script_sig = script_sig.clone();
        child.outputs[0].value = 0;
        child.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            10_000,
        ))));
        mempool.add_package(vec![parent, child]).unwrap()
    };
    let at_floor = admit(540, 9_460);
    let under = admit(539, 9_461);
    let paid = admit(539, 9_460);
    eprintln!(
        "output 540 admitted={at_floor}; output 539 admitted={under}; paid dust admitted={paid}"
    );
    assert!(at_floor);
    assert!(under);
    assert!(!paid);
}

#[test]
fn anchor_output_dust_floor_is_240() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let admit = |value: i64| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig.clone();
        tx.outputs[0].value = value;
        tx.outputs[0].script_pubkey = vec![0x51, 0x02, 0x4e, 0x73];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            10_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let at_floor = admit(240);
    let under = admit(239);
    eprintln!("anchor 240 admitted={at_floor}; anchor 239 admitted={under}");
    assert!(at_floor);
    assert!(!under);
}

#[test]
fn a_zero_fee_parent_may_not_have_two_dust_outputs() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let op_return = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
    let admit = |second_is_spendable: bool| {
        let mut parent = legacy_empty_script();
        parent.inputs[0].script_sig = script_sig.clone();
        parent.outputs[0].value = 539;
        parent.outputs[0].script_pubkey = prevout.clone();
        parent.outputs.push(TransactionOutput {
            value: 539,
            script_pubkey: if second_is_spendable {
                prevout.clone()
            } else {
                op_return.clone()
            },
        });
        parent.outputs.push(TransactionOutput {
            value: 8_922,
            script_pubkey: op_return.clone(),
        });
        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: blvm_protocol::block::calculate_tx_id(&parent),
            index: 0,
        };
        child.inputs[0].script_sig = script_sig.clone();
        child.outputs[0].value = 0;
        child.outputs[0].script_pubkey = op_return.clone();
        let parent_id = blvm_protocol::block::calculate_tx_id(&parent);
        let child_id = blvm_protocol::block::calculate_tx_id(&child);
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            10_000,
        ))));
        let _ = mempool.add_package(vec![parent, child]);
        (
            mempool.get_transaction(&parent_id).is_some(),
            mempool.get_transaction(&child_id).is_some(),
        )
    };
    let (one_parent, one_child) = admit(false);
    let (two_parent, two_child) = admit(true);
    eprintln!(
        "one dust parent={one_parent} child={one_child}; two dust parent={two_parent} child={two_child}"
    );
    assert!(one_parent && one_child);
    assert!(!two_parent && !two_child);
}

#[test]
fn a_rejected_child_does_not_leave_its_parent() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let keeps_parent = |parent_fee: i64, child_sig: Vec<u8>| {
        let mut parent = legacy_empty_script();
        parent.inputs[0].script_sig = script_sig.clone();
        parent.outputs[0].value = 10_000 - parent_fee;
        parent.outputs[0].script_pubkey = prevout.clone();
        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: blvm_protocol::block::calculate_tx_id(&parent),
            index: 0,
        };
        child.inputs[0].script_sig = child_sig;
        child.outputs[0].value = 0;
        child.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let parent_id = blvm_protocol::block::calculate_tx_id(&parent);
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            10_000,
        ))));
        let _ = mempool.add_package(vec![parent, child]);
        mempool.get_transaction(&parent_id).is_some()
    };
    let valid = keeps_parent(0, script_sig.clone());
    let rejected = keeps_parent(0, vec![0x01, 0x00]);
    let paying = keeps_parent(1_000, script_sig.clone());
    let paying_rejected = keeps_parent(1_000, vec![0x01, 0x00]);
    eprintln!(
        "valid child keeps parent={valid}; rejected child keeps parent={rejected}; paying parent with valid child={paying}; paying parent with rejected child={paying_rejected}"
    );
    assert!(valid);
    assert!(!rejected);
    assert!(paying);
    assert!(paying_rejected);
}

#[test]
fn a_rejected_sibling_does_not_leave_the_paying_child() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let keeps = |sibling_sig: Vec<u8>| {
        let mut parent = legacy_empty_script();
        parent.inputs[0].script_sig = script_sig.clone();
        parent.outputs[0].value = 10_000;
        parent.outputs[0].script_pubkey = prevout.clone();
        parent.outputs.push(TransactionOutput {
            value: 10_000,
            script_pubkey: prevout.clone(),
        });
        let parent_id = blvm_protocol::block::calculate_tx_id(&parent);
        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: parent_id,
            index: 0,
        };
        child.inputs[0].script_sig = script_sig.clone();
        child.outputs[0].value = 0;
        child.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let child_id = blvm_protocol::block::calculate_tx_id(&child);
        let mut sibling = legacy_empty_script();
        sibling.inputs[0].prevout = OutPoint {
            hash: parent_id,
            index: 1,
        };
        sibling.inputs[0].script_sig = sibling_sig;
        sibling.outputs[0].value = 0;
        sibling.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            20_000,
        ))));
        let _ = mempool.add_package(vec![parent, child, sibling]);
        (
            mempool.get_transaction(&parent_id).is_some(),
            mempool.get_transaction(&child_id).is_some(),
        )
    };
    let valid = keeps(script_sig.clone());
    let rejected = keeps(vec![0x01, 0x00]);
    eprintln!(
        "valid sibling parent={} child={}; rejected sibling parent={} child={}",
        valid.0, valid.1, rejected.0, rejected.1
    );
    assert!(valid.0 && valid.1);
    assert!(!rejected.0 && !rejected.1);
}

#[test]
fn a_grandchild_does_not_carry_a_zero_fee_grandparent() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let keeps_grandparent = |grand_out: i64| {
        let mut grand = legacy_empty_script();
        grand.inputs[0].script_sig = script_sig.clone();
        grand.outputs[0].value = grand_out;
        grand.outputs[0].script_pubkey = prevout.clone();
        let grand_id = blvm_protocol::block::calculate_tx_id(&grand);
        let mut parent = legacy_empty_script();
        parent.inputs[0].prevout = OutPoint {
            hash: grand_id,
            index: 0,
        };
        parent.inputs[0].script_sig = script_sig.clone();
        parent.outputs[0].value = 5_000;
        parent.outputs[0].script_pubkey = prevout.clone();
        let parent_id = blvm_protocol::block::calculate_tx_id(&parent);
        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: parent_id,
            index: 0,
        };
        child.inputs[0].script_sig = script_sig.clone();
        child.outputs[0].value = 0;
        child.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            10_000,
        ))));
        let _ = mempool.add_package(vec![grand, parent, child]);
        mempool.get_transaction(&grand_id).is_some()
    };
    let paying = keeps_grandparent(9_000);
    let carried = keeps_grandparent(10_000);
    eprintln!("paying grandparent={paying}; zero-fee grandparent={carried}");
    assert!(paying);
    assert!(!carried);
}

#[test]
fn two_parents_are_priced_once() {
    use std::sync::Arc;

    let (prevout, script_sig) = p2sh_of_op_1();
    let mut left = legacy_empty_script();
    left.inputs[0].script_sig = script_sig.clone();
    left.outputs[0].value = 50_000;
    left.outputs[0].script_pubkey = prevout.clone();
    let mut right = legacy_empty_script();
    right.inputs[0].prevout.hash = [2u8; 32];
    right.inputs[0].script_sig = script_sig.clone();
    right.outputs[0].value = 50_000;
    right.outputs[0].script_pubkey = prevout.clone();
    let left_id = blvm_protocol::block::calculate_tx_id(&left);
    let right_id = blvm_protocol::block::calculate_tx_id(&right);
    let mut child = legacy_empty_script();
    child.inputs[0].prevout = OutPoint {
        hash: left_id,
        index: 0,
    };
    child.inputs[0].script_sig = script_sig.clone();
    child.inputs.push(TransactionInput {
        prevout: OutPoint {
            hash: right_id,
            index: 0,
        },
        script_sig: script_sig,
        sequence: 0xffffffff,
    });
    child.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
    let mut measure = blvm_protocol::UtxoSet::default();
    let coin = |value: i64| {
        Arc::new(blvm_protocol::UTXO {
            value,
            script_pubkey: prevout.clone().into(),
            height: 0,
            is_coinbase: false,
        })
    };
    measure.insert(left.inputs[0].prevout, coin(50_000));
    measure.insert(right.inputs[0].prevout, coin(50_000));
    measure.insert(
        OutPoint {
            hash: left_id,
            index: 0,
        },
        coin(50_000),
    );
    measure.insert(
        OutPoint {
            hash: right_id,
            index: 0,
        },
        coin(50_000),
    );
    let meter = MempoolManager::new();
    meter.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
    let left_vsize = meter.admit_vsize(&left, None, &measure).0;
    let right_vsize = meter.admit_vsize(&right, None, &measure).0;
    let child_vsize = meter.admit_vsize(&child, None, &measure).0;
    let package_vsize = left_vsize + right_vsize + child_vsize;
    let package_fee = (100u128 * package_vsize as u128).div_ceil(1000) as i64;
    let keeps = |fee: i64| {
        let mut child = child.clone();
        child.outputs[0].value = 100_000 - fee;
        let mut funded = blvm_protocol::UtxoSet::default();
        funded.insert(left.inputs[0].prevout, coin(50_000));
        funded.insert(right.inputs[0].prevout, coin(50_000));
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded)));
        let _ = mempool.add_package(vec![left.clone(), right.clone(), child]);
        (
            mempool.get_transaction(&left_id).is_some(),
            mempool.get_transaction(&right_id).is_some(),
        )
    };
    let covered = keeps(package_fee);
    let short = keeps(package_fee - 1);
    eprintln!(
        "vsize package={package_vsize}; fee {package_fee} parents={} {}; fee {} parents={} {}",
        covered.0,
        covered.1,
        package_fee - 1,
        short.0,
        short.1
    );
    assert!(covered.0 && covered.1);
    assert!(!short.0 && !short.1);
}

#[test]
fn a_parent_may_carry_a_child_under_the_relay_floor() {
    let (prevout, script_sig) = p2sh_of_op_1();
    let keeps_child = |child_fee: i64| {
        let mut parent = legacy_empty_script();
        parent.inputs[0].script_sig = script_sig.clone();
        parent.outputs[0].value = 10_000;
        parent.outputs[0].script_pubkey = prevout.clone();
        let parent_id = blvm_protocol::block::calculate_tx_id(&parent);
        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: parent_id,
            index: 0,
        };
        child.inputs[0].script_sig = script_sig.clone();
        child.outputs[0].value = 10_000 - child_fee;
        child.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let child_id = blvm_protocol::block::calculate_tx_id(&child);
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            50_000,
        ))));
        let _ = mempool.add_package(vec![parent, child]);
        mempool.get_transaction(&child_id).is_some()
    };
    let paying = keeps_child(100);
    let carried = keeps_child(0);
    eprintln!("child fee 100 admitted={paying}; child fee 0 admitted={carried}");
    assert!(paying);
    assert!(carried);
}

#[test]
fn nonwitness_size_below_65_is_not_relayed() {
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    const OP_0: u8 = 0x00;
    const OP_1: u8 = 0x51;
    let script = vec![OP_1];
    let program: [u8; 32] = Sha256::digest(&script).into();
    let mut prevout = vec![OP_0, 0x20];
    prevout.extend_from_slice(&program);
    let admit = |payload: usize| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        let mut out = vec![0x6a, payload as u8];
        out.extend(std::iter::repeat(0u8).take(payload));
        tx.outputs[0].script_pubkey = out;
        let size = blvm_protocol::wire::serialize_tx(&tx).unwrap().len();
        let witness: Vec<Witness> = vec![vec![script.clone()]];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            100_000,
        ))));
        let admitted = mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap();
        (size, admitted)
    };
    let (at_limit_size, at_limit) = admit(3);
    let (under_size, under) = admit(2);
    eprintln!(
        "nonwitness {at_limit_size} admitted={at_limit}; nonwitness {under_size} admitted={under}"
    );
    assert_eq!(at_limit_size, 65);
    assert_eq!(under_size, 64);
    assert!(at_limit);
    assert!(!under);
}

#[test]
fn taproot_annex_is_not_standard() {
    use blvm_consensus::secp256k1_backend::taproot_output_key_with_parity;
    use blvm_consensus::taproot::{TAPROOT_LEAF_VERSION_TAPSCRIPT, compute_script_merkle_root};
    use blvm_protocol::segwit::Witness;

    let tapscript = vec![0x51u8];
    let internal = [
        0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87, 0x0b,
        0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8,
        0x17, 0x98,
    ];
    let root = compute_script_merkle_root(&tapscript, &[], TAPROOT_LEAF_VERSION_TAPSCRIPT).unwrap();
    let (output_key, parity) = taproot_output_key_with_parity(&internal, &root).unwrap();
    let mut prevout = vec![0x51, 0x20];
    prevout.extend_from_slice(&output_key);
    let mut control = vec![TAPROOT_LEAF_VERSION_TAPSCRIPT | parity];
    control.extend_from_slice(&internal);
    let admit = |annex: bool| {
        let mut stack = vec![tapscript.clone(), control.clone()];
        if annex {
            stack.push(vec![0x50]);
        }
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![stack];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let plain = admit(false);
    let with_annex = admit(true);
    eprintln!("taproot script path admitted={plain}; annex admitted={with_annex}");
    assert!(plain);
    assert!(!with_annex);
}

#[test]
fn tapscript_item_of_81_bytes_is_not_standard() {
    use blvm_consensus::secp256k1_backend::taproot_output_key_with_parity;
    use blvm_consensus::taproot::{TAPROOT_LEAF_VERSION_TAPSCRIPT, compute_script_merkle_root};
    use blvm_protocol::segwit::Witness;

    let tapscript = vec![0x75u8, 0x51];
    let internal = [
        0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87, 0x0b,
        0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8,
        0x17, 0x98,
    ];
    let root = compute_script_merkle_root(&tapscript, &[], TAPROOT_LEAF_VERSION_TAPSCRIPT).unwrap();
    let (output_key, parity) = taproot_output_key_with_parity(&internal, &root).unwrap();
    let mut prevout = vec![0x51, 0x20];
    prevout.extend_from_slice(&output_key);
    let mut control = vec![TAPROOT_LEAF_VERSION_TAPSCRIPT | parity];
    control.extend_from_slice(&internal);
    let admit = |item_len: usize| {
        let stack = vec![vec![0u8; item_len], tapscript.clone(), control.clone()];
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![stack];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let eighty = admit(80);
    let eighty_one = admit(81);
    eprintln!(
        "80-byte tapscript item admitted={eighty}; 81-byte tapscript item admitted={eighty_one}"
    );
    assert!(eighty);
    assert!(!eighty_one);
}

#[test]
fn non_empty_failed_signature_is_not_standard() {
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    const OP_0: u8 = 0x00;
    let script = vec![0xac, 0x75, 0x51];
    let program: [u8; 32] = Sha256::digest(&script).into();
    let mut prevout = vec![OP_0, 0x20];
    prevout.extend_from_slice(&program);
    let pubkey = {
        let mut key = vec![0x02];
        key.extend_from_slice(&[
            0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
            0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b,
            0x16, 0xf8, 0x17, 0x98,
        ]);
        key
    };
    let admit = |sig: Vec<u8>| {
        let stack = vec![sig, pubkey.clone(), script.clone()];
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![stack];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let empty = admit(Vec::new());
    let bad = admit(vec![0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01, 0x01]);
    eprintln!("empty signature admitted={empty}; non-empty failure admitted={bad}");
    assert!(empty);
    assert!(!bad);
}

#[test]
fn high_s_signature_is_not_standard() {
    use blvm_protocol::segwit::Witness;
    use blvm_secp256k1::ecdsa::{ecdsa_sign_compact_rfc6979, ge_to_compressed, pubkey_from_secret};
    use blvm_secp256k1::scalar::Scalar;
    use sha2::{Digest, Sha256};

    fn der_int(value: &[u8]) -> Vec<u8> {
        let mut start = 0;
        while start + 1 < value.len() && value[start] == 0 {
            start += 1;
        }
        let mut body = value[start..].to_vec();
        if body.first().is_some_and(|byte| byte & 0x80 != 0) {
            body.insert(0, 0x00);
        }
        let mut out = vec![0x02, body.len() as u8];
        out.extend(body);
        out
    }
    fn der_sig(compact: &[u8; 64]) -> Vec<u8> {
        let mut body = der_int(&compact[..32]);
        body.extend(der_int(&compact[32..]));
        let mut out = vec![0x30, body.len() as u8];
        out.extend(body);
        out.push(0x01);
        out
    }
    fn negate_scalar(value: &[u8]) -> [u8; 32] {
        const ORDER: [u8; 32] = [
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFE, 0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C,
            0xD0, 0x36, 0x41, 0x41,
        ];
        let mut out = [0u8; 32];
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let diff = ORDER[i] as i16 - value[i] as i16 - borrow;
            if diff < 0 {
                out[i] = (diff + 256) as u8;
                borrow = 1;
            } else {
                out[i] = diff as u8;
                borrow = 0;
            }
        }
        out
    }

    let script = vec![0xac];
    let program: [u8; 32] = Sha256::digest(&script).into();
    let mut prevout_script = vec![0x00, 0x20];
    prevout_script.extend_from_slice(&program);
    let mut tx = legacy_empty_script();
    tx.inputs[0].script_sig = Vec::new();
    tx.outputs[0].value = 0;
    tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
    let sighash = blvm_consensus::transaction_hash::calculate_bip143_sighash(
        &tx, 0, &script, 100_000, 0x01, None,
    )
    .unwrap();
    let seckey = [0x22u8; 32];
    let low = ecdsa_sign_compact_rfc6979(&sighash, &seckey).unwrap();
    let mut high = low;
    high[32..].copy_from_slice(&negate_scalar(&low[32..]));
    let mut secret = Scalar::zero();
    assert!(!secret.set_b32(&seckey));
    let pubkey = ge_to_compressed(&pubkey_from_secret(&secret)).to_vec();
    let admit = |sig: Vec<u8>| {
        let stack = vec![sig, pubkey.clone(), script.clone()];
        let witness: Vec<Witness> = vec![stack];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout_script.clone(),
            100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx.clone(), Some(witness))
            .unwrap()
    };
    let low_admitted = admit(der_sig(&low));
    let high_admitted = admit(der_sig(&high));
    eprintln!("low s admitted={low_admitted}; high s admitted={high_admitted}");
    assert!(low_admitted);
    assert!(!high_admitted);
}

#[test]
fn uncompressed_witness_pubkey_is_not_standard() {
    use blvm_protocol::segwit::Witness;
    use blvm_secp256k1::ecdsa::{
        ecdsa_sign_compact_rfc6979, ge_to_compressed, ge_to_uncompressed, pubkey_from_secret,
    };
    use blvm_secp256k1::scalar::Scalar;
    use sha2::{Digest, Sha256};

    fn der_int(value: &[u8]) -> Vec<u8> {
        let mut start = 0;
        while start + 1 < value.len() && value[start] == 0 {
            start += 1;
        }
        let mut body = value[start..].to_vec();
        if body.first().is_some_and(|byte| byte & 0x80 != 0) {
            body.insert(0, 0x00);
        }
        let mut out = vec![0x02, body.len() as u8];
        out.extend(body);
        out
    }
    fn der_sig(compact: &[u8; 64]) -> Vec<u8> {
        let mut body = der_int(&compact[..32]);
        body.extend(der_int(&compact[32..]));
        let mut out = vec![0x30, body.len() as u8];
        out.extend(body);
        out.push(0x01);
        out
    }

    let script = vec![0xac];
    let program: [u8; 32] = Sha256::digest(&script).into();
    let mut prevout_script = vec![0x00, 0x20];
    prevout_script.extend_from_slice(&program);
    let mut tx = legacy_empty_script();
    tx.inputs[0].script_sig = Vec::new();
    tx.outputs[0].value = 0;
    tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
    let sighash = blvm_consensus::transaction_hash::calculate_bip143_sighash(
        &tx, 0, &script, 100_000, 0x01, None,
    )
    .unwrap();
    let seckey = [0x22u8; 32];
    let compact = ecdsa_sign_compact_rfc6979(&sighash, &seckey).unwrap();
    let sig = der_sig(&compact);
    let mut secret = Scalar::zero();
    assert!(!secret.set_b32(&seckey));
    let point = pubkey_from_secret(&secret);
    let compressed = ge_to_compressed(&point).to_vec();
    let uncompressed = ge_to_uncompressed(&point).to_vec();
    let admit = |pubkey: Vec<u8>| {
        let stack = vec![sig.clone(), pubkey, script.clone()];
        let witness: Vec<Witness> = vec![stack];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout_script.clone(),
            100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx.clone(), Some(witness))
            .unwrap()
    };
    let compressed_admitted = admit(compressed);
    let uncompressed_admitted = admit(uncompressed);
    eprintln!(
        "compressed pubkey admitted={compressed_admitted}; uncompressed pubkey admitted={uncompressed_admitted}"
    );
    assert!(compressed_admitted);
    assert!(!uncompressed_admitted);
}

#[test]
fn tapscript_op_success_is_not_standard() {
    use blvm_consensus::secp256k1_backend::taproot_output_key_with_parity;
    use blvm_consensus::taproot::{TAPROOT_LEAF_VERSION_TAPSCRIPT, compute_script_merkle_root};
    use blvm_protocol::segwit::Witness;

    let internal = [
        0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87, 0x0b,
        0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8,
        0x17, 0x98,
    ];
    let admit = |tapscript: Vec<u8>| {
        let root =
            compute_script_merkle_root(&tapscript, &[], TAPROOT_LEAF_VERSION_TAPSCRIPT).unwrap();
        let (output_key, parity) = taproot_output_key_with_parity(&internal, &root).unwrap();
        let mut prevout = vec![0x51, 0x20];
        prevout.extend_from_slice(&output_key);
        let mut control = vec![TAPROOT_LEAF_VERSION_TAPSCRIPT | parity];
        control.extend_from_slice(&internal);
        let stack = vec![tapscript, control];
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![stack];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let plain = admit(vec![0x51]);
    let success = admit(vec![0xbb]);
    eprintln!("op_1 tapscript admitted={plain}; op_success admitted={success}");
    assert!(plain);
    assert!(!success);
}

#[test]
fn unknown_taproot_leaf_is_not_standard() {
    use blvm_consensus::secp256k1_backend::taproot_output_key_with_parity;
    use blvm_consensus::taproot::compute_script_merkle_root;
    use blvm_protocol::segwit::Witness;

    let script = vec![0x51u8];
    let internal = [
        0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87, 0x0b,
        0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8,
        0x17, 0x98,
    ];
    let admit = |leaf_version: u8| {
        let root = compute_script_merkle_root(&script, &[], leaf_version).unwrap();
        let (output_key, parity) = taproot_output_key_with_parity(&internal, &root).unwrap();
        let mut prevout = vec![0x51, 0x20];
        prevout.extend_from_slice(&output_key);
        let mut control = vec![leaf_version | parity];
        control.extend_from_slice(&internal);
        let stack = vec![script.clone(), control];
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![stack];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let tapscript = admit(0xc0);
    let unknown = admit(0xc2);
    eprintln!("tapscript leaf admitted={tapscript}; unknown leaf admitted={unknown}");
    assert!(tapscript);
    assert!(!unknown);
}

#[test]
fn witness_inscription_is_rejected_when_spam_filter_is_on() {
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    const OP_0: u8 = 0x00;
    const OP_1: u8 = 0x51;
    const OP_IF: u8 = 0x63;
    const OP_ENDIF: u8 = 0x68;

    let admit = |script: Vec<u8>| {
        let program: [u8; 32] = Sha256::digest(&script).into();
        let mut prevout = vec![OP_0, 0x20];
        prevout.extend_from_slice(&program);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 1_000;
        tx.outputs[0].script_pubkey = {
            let mut pay = vec![OP_0, 0x14];
            pay.extend_from_slice(&[0x11; 20]);
            pay
        };
        let witness: Vec<Witness> = vec![vec![script]];
        let mut policy = crate::config::MempoolPolicyConfig::default();
        policy.reject_spam_in_mempool = true;
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(policy));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let plain = admit(vec![OP_1]);
    let envelope = admit(vec![OP_0, OP_IF, OP_ENDIF, OP_1]);
    eprintln!("plain p2wsh admitted={plain}; inscription envelope admitted={envelope}");
    assert!(plain);
    assert!(!envelope);
}

#[test]
fn upgradable_nop_is_not_standard() {
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    const OP_0: u8 = 0x00;
    const OP_1: u8 = 0x51;
    const OP_NOP: u8 = 0x61;
    const OP_NOP1: u8 = 0xb0;
    let admit = |opcode: u8| {
        let script = vec![opcode, OP_1];
        let program: [u8; 32] = Sha256::digest(&script).into();
        let mut prevout = vec![OP_0, 0x20];
        prevout.extend_from_slice(&program);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![vec![script]];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let nop = admit(OP_NOP);
    let nop1 = admit(OP_NOP1);
    eprintln!("op_nop admitted={nop}; op_nop1 admitted={nop1}");
    assert!(nop);
    assert!(!nop1);
}

#[test]
fn nonminimal_witness_if_is_not_standard() {
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    const OP_0: u8 = 0x00;
    const OP_1: u8 = 0x51;
    const OP_IF: u8 = 0x63;
    const OP_ELSE: u8 = 0x67;
    const OP_ENDIF: u8 = 0x68;
    let script = vec![OP_IF, OP_1, OP_ELSE, OP_0, OP_ENDIF];
    let program: [u8; 32] = Sha256::digest(&script).into();
    let mut prevout = vec![OP_0, 0x20];
    prevout.extend_from_slice(&program);
    let admit = |condition: u8| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![vec![vec![condition], script.clone()]];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let minimal = admit(0x01);
    let nonminimal = admit(0x02);
    eprintln!("minimal if admitted={minimal}; nonminimal if admitted={nonminimal}");
    assert!(minimal);
    assert!(!nonminimal);
}

#[test]
fn p2sh_extra_stack_item_is_not_standard() {
    use blvm_protocol::opcodes::{OP_EQUAL, OP_HASH160};
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    fn hash160(data: &[u8]) -> [u8; 20] {
        Ripemd160::digest(Sha256::digest(data)).into()
    }

    let redeem = vec![0x51];
    let hash = hash160(&redeem);
    let mut prevout = vec![OP_HASH160, 0x14];
    prevout.extend_from_slice(&hash);
    prevout.push(OP_EQUAL);
    let admit = |script_sig: Vec<u8>| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let clean = admit(vec![0x01, 0x51]);
    let extra = admit(vec![0x00, 0x01, 0x51]);
    eprintln!("clean p2sh admitted={clean}; extra stack item admitted={extra}");
    assert!(clean);
    assert!(!extra);
}

#[test]
fn nonstandard_prevout_spend_is_not_standard() {
    use blvm_protocol::opcodes::{OP_EQUAL, OP_HASH160};
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let redeem = vec![0x51];
    let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
    let mut p2sh = vec![OP_HASH160, 0x14];
    p2sh.extend_from_slice(&hash);
    p2sh.push(OP_EQUAL);
    let script_sig = vec![0x01, 0x51];
    let admit = |prevout: Vec<u8>| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig.clone();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let standard = admit(p2sh);
    let bare = admit(vec![0x75, 0x51]);
    eprintln!("p2sh op_1 admitted={standard}; bare drop op_1 admitted={bare}");
    assert!(standard);
    assert!(!bare);
}

#[test]
fn signature_inside_script_code_is_not_standard() {
    use std::sync::Arc;

    use blvm_secp256k1::ecdsa::{ecdsa_sign_compact_rfc6979, ge_to_compressed, pubkey_from_secret};
    use blvm_secp256k1::scalar::Scalar;
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    fn der_int(value: &[u8]) -> Vec<u8> {
        let mut start = 0;
        while start + 1 < value.len() && value[start] == 0 {
            start += 1;
        }
        let mut body = value[start..].to_vec();
        if body.first().is_some_and(|byte| byte & 0x80 != 0) {
            body.insert(0, 0x00);
        }
        let mut out = vec![0x02, body.len() as u8];
        out.extend(body);
        out
    }
    fn der_sig(compact: &[u8; 64]) -> Vec<u8> {
        let mut body = der_int(&compact[..32]);
        body.extend(der_int(&compact[32..]));
        let mut out = vec![0x30, body.len() as u8];
        out.extend(body);
        out.push(0x03);
        out
    }
    fn push(data: &[u8]) -> Vec<u8> {
        let mut out = if data.len() < 76 {
            vec![data.len() as u8]
        } else {
            vec![0x4c, data.len() as u8]
        };
        out.extend_from_slice(data);
        out
    }
    fn p2sh(redeem: &[u8]) -> Vec<u8> {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(redeem)).into();
        let mut script = vec![0xa9, 0x14];
        script.extend_from_slice(&hash);
        script.push(0x87);
        script
    }

    let mut digest = [0u8; 32];
    digest[0] = 1;
    let seckey = [0x22u8; 32];
    let compact = ecdsa_sign_compact_rfc6979(&digest, &seckey).unwrap();
    let sig = der_sig(&compact);
    let mut secret = Scalar::zero();
    assert!(!secret.set_b32(&seckey));
    let pubkey = ge_to_compressed(&pubkey_from_secret(&secret)).to_vec();
    let admit = |redeem: Vec<u8>| {
        let mut script_sig = push(&sig);
        script_sig.extend(push(&pubkey));
        script_sig.extend(push(&redeem));
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = vec![0x01, 0x51];
        tx.inputs.push(TransactionInput {
            prevout: OutPoint {
                hash: [2u8; 32],
                index: 0,
            },
            script_sig,
            sequence: 0xffff_ffff,
        });
        tx.outputs[0].value = 50_000;
        tx.outputs[0].script_pubkey = {
            let mut script = vec![0x00, 0x14];
            script.extend_from_slice(&[0x22; 20]);
            script
        };
        let mut utxo = funded(p2sh(&[0x51]), 100_000);
        utxo.insert(
            tx.inputs[1].prevout,
            Arc::new(blvm_protocol::UTXO {
                value: 100_000,
                script_pubkey: p2sh(&redeem).into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(utxo)));
        mempool.add_transaction(tx).unwrap()
    };
    let clean = admit(vec![0xac]);
    let mut embedded_redeem = push(&sig);
    embedded_redeem.extend_from_slice(&[0x75, 0xac]);
    let embedded = admit(embedded_redeem);
    eprintln!("clean checksig admitted={clean}; signature in script admitted={embedded}");
    assert!(clean);
    assert!(!embedded);
}

#[test]
fn signature_inside_multisig_script_code_is_not_standard() {
    use std::sync::Arc;

    use blvm_secp256k1::ecdsa::{ecdsa_sign_compact_rfc6979, ge_to_compressed, pubkey_from_secret};
    use blvm_secp256k1::scalar::Scalar;
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    fn der_int(value: &[u8]) -> Vec<u8> {
        let mut start = 0;
        while start + 1 < value.len() && value[start] == 0 {
            start += 1;
        }
        let mut body = value[start..].to_vec();
        if body.first().is_some_and(|byte| byte & 0x80 != 0) {
            body.insert(0, 0x00);
        }
        let mut out = vec![0x02, body.len() as u8];
        out.extend(body);
        out
    }
    fn der_sig(compact: &[u8; 64]) -> Vec<u8> {
        let mut body = der_int(&compact[..32]);
        body.extend(der_int(&compact[32..]));
        let mut out = vec![0x30, body.len() as u8];
        out.extend(body);
        out.push(0x03);
        out
    }
    fn push(data: &[u8]) -> Vec<u8> {
        let mut out = if data.len() < 76 {
            vec![data.len() as u8]
        } else {
            vec![0x4c, data.len() as u8]
        };
        out.extend_from_slice(data);
        out
    }
    fn p2sh(redeem: &[u8]) -> Vec<u8> {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(redeem)).into();
        let mut script = vec![0xa9, 0x14];
        script.extend_from_slice(&hash);
        script.push(0x87);
        script
    }

    let mut digest = [0u8; 32];
    digest[0] = 1;
    let seckey = [0x22u8; 32];
    let compact = ecdsa_sign_compact_rfc6979(&digest, &seckey).unwrap();
    let sig = der_sig(&compact);
    let mut secret = Scalar::zero();
    assert!(!secret.set_b32(&seckey));
    let pubkey = ge_to_compressed(&pubkey_from_secret(&secret)).to_vec();
    let mut multisig = vec![0x51, 0x21];
    multisig.extend_from_slice(&pubkey);
    multisig.extend_from_slice(&[0x51, 0xae]);
    let admit = |redeem: Vec<u8>| {
        let mut script_sig = vec![0x00];
        script_sig.extend(push(&sig));
        script_sig.extend(push(&redeem));
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = vec![0x01, 0x51];
        tx.inputs.push(TransactionInput {
            prevout: OutPoint {
                hash: [2u8; 32],
                index: 0,
            },
            script_sig,
            sequence: 0xffff_ffff,
        });
        tx.outputs[0].value = 50_000;
        tx.outputs[0].script_pubkey = {
            let mut script = vec![0x00, 0x14];
            script.extend_from_slice(&[0x22; 20]);
            script
        };
        let mut utxo = funded(p2sh(&[0x51]), 100_000);
        utxo.insert(
            tx.inputs[1].prevout,
            Arc::new(blvm_protocol::UTXO {
                value: 100_000,
                script_pubkey: p2sh(&redeem).into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(utxo)));
        mempool.add_transaction(tx).unwrap()
    };
    let clean = admit(multisig.clone());
    let mut embedded_redeem = push(&sig);
    embedded_redeem.push(0x75);
    embedded_redeem.extend_from_slice(&multisig);
    let embedded = admit(embedded_redeem);
    eprintln!("clean multisig admitted={clean}; signature in multisig script admitted={embedded}");
    assert!(clean);
    assert!(!embedded);
}

#[test]
fn future_witness_program_is_admitted_when_the_payload_is_true() {
    const OP_1: u8 = 0x51;
    const OP_2: u8 = 0x52;

    let mut policy = crate::config::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_relay_fee_rate = 0;
    policy.min_tx_fee = 0;
    policy.reject_spam_in_mempool = false;

    let admit = |prevout_script: Vec<u8>| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 50_000;
        tx.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(policy.clone()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout_script,
            100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };

    assert!(admit(vec![OP_2, 0x02, 0x00, 0x01]));
    assert!(!admit(vec![OP_2, 0x02, 0x00, 0x00]));
}

#[test]
fn version_two_witness_output_is_a_standard_payment() {
    let admit = |script: Vec<u8>| {
        let (prevout, script_sig) = p2sh_of_op_1();
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 50_000;
        tx.outputs[0].script_pubkey = script;
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let mut taproot = vec![0x51, 0x20];
    taproot.extend_from_slice(&[0x11; 32]);
    assert!(admit(taproot));
    let mut program = vec![0x52, 0x20];
    program.extend_from_slice(&[0x11; 32]);
    assert!(admit(program));
    assert!(!admit(vec![0x00, 0x02, 0x00, 0x01]));
}

#[test]
fn non_push_script_sig_is_not_standard() {
    use blvm_protocol::opcodes::{OP_1, OP_EQUAL, OP_HASH160, OP_NOP};
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let redeem = vec![OP_1];
    let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
    let mut p2sh = vec![OP_HASH160, 0x14];
    p2sh.extend_from_slice(&hash);
    p2sh.push(OP_EQUAL);
    let admit = |script_sig: Vec<u8>, prevout: Vec<u8>, standard: bool| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 50_000;
        tx.outputs[0].script_pubkey = {
            let mut script = vec![0x00, 0x14];
            script.extend_from_slice(&[0x22; 20]);
            script
        };
        let mut policy = crate::config::MempoolPolicyConfig::default();
        policy.require_standard = standard;
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(policy));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    assert!(admit(vec![0x01, OP_1], p2sh, true));
    assert!(admit(vec![OP_NOP], vec![OP_1], false));
    assert!(!admit(vec![OP_NOP], vec![OP_1], true));
}

#[test]
fn legacy_codeseparator_is_not_standard() {
    use blvm_protocol::opcodes::{OP_EQUAL, OP_HASH160};
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let admit = |redeem: Vec<u8>| {
        let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
        let mut prevout = vec![OP_HASH160, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(OP_EQUAL);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 50_000;
        tx.outputs[0].script_pubkey = {
            let mut script = vec![0x00, 0x14];
            script.extend_from_slice(&[0x22; 20]);
            script
        };
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let plain = admit(vec![0x51]);
    let codesep = admit(vec![0xab, 0x51]);
    eprintln!("legacy op_1 admitted={plain}; legacy codeseparator admitted={codesep}");
    assert!(plain);
    assert!(!codesep);
}

#[test]
fn wrapped_undefined_witness_spend_is_not_standard() {
    use blvm_protocol::opcodes::{OP_EQUAL, OP_HASH160};
    use blvm_protocol::segwit::Witness;
    use ripemd::{Digest as _, Ripemd160};
    use sha2::{Digest, Sha256};

    fn hash160(data: &[u8]) -> [u8; 20] {
        Ripemd160::digest(Sha256::digest(data)).into()
    }

    let program: [u8; 32] = Sha256::digest([0x51u8]).into();
    let admit = |version: u8| {
        let mut redeem = vec![version, 0x20];
        redeem.extend_from_slice(&program);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let hash = hash160(&redeem);
        let mut prevout = vec![OP_HASH160, 0x14];
        prevout.extend_from_slice(&hash);
        prevout.push(OP_EQUAL);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let witness: Vec<Witness> = vec![vec![vec![0x51]]];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let wrapped_v0 = admit(0x00);
    let wrapped_v2 = admit(0x52);
    eprintln!("wrapped v0 admitted={wrapped_v0}; wrapped v2 admitted={wrapped_v2}");
    assert!(wrapped_v0);
    assert!(!wrapped_v2);
}

#[test]
fn unknown_tapscript_pubkey_is_not_standard() {
    use blvm_consensus::secp256k1_backend::taproot_output_key_with_parity;
    use blvm_consensus::taproot::{
        TAPROOT_LEAF_VERSION_TAPSCRIPT, compute_script_merkle_root,
        compute_tapscript_signature_hash,
    };
    use blvm_protocol::segwit::Witness;
    use blvm_secp256k1::schnorr::{Keypair, schnorr_sign};

    let seckey = [0x22u8; 32];
    let keypair = Keypair::from_seckey(&seckey).unwrap();
    let compressed_x = keypair.xonly_pubkey();
    let mut unknown = compressed_x.to_vec();
    unknown.push(0x02);
    let internal = [
        0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87, 0x0b,
        0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16, 0xf8,
        0x17, 0x98,
    ];
    let script_for = |pubkey: &[u8]| {
        let mut script = vec![pubkey.len() as u8];
        script.extend_from_slice(pubkey);
        script.push(0xac);
        script
    };
    let known_script = script_for(&compressed_x);
    let root =
        compute_script_merkle_root(&known_script, &[], TAPROOT_LEAF_VERSION_TAPSCRIPT).unwrap();
    let (output_key, _parity) = taproot_output_key_with_parity(&internal, &root).unwrap();
    let mut known_prevout = vec![0x51, 0x20];
    known_prevout.extend_from_slice(&output_key);
    let mut tx = legacy_empty_script();
    tx.inputs[0].script_sig = Vec::new();
    tx.outputs[0].value = 0;
    tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
    let sighash = compute_tapscript_signature_hash(
        &tx,
        0,
        &[100_000],
        &[known_prevout.as_slice()],
        &known_script,
        TAPROOT_LEAF_VERSION_TAPSCRIPT,
        0xffff_ffff,
        0x00,
        None,
    )
    .unwrap();
    let sig = schnorr_sign(&seckey, &sighash, &[0u8; 32]).unwrap();
    let admit = |pubkey: &[u8]| {
        let tapscript = script_for(pubkey);
        let root =
            compute_script_merkle_root(&tapscript, &[], TAPROOT_LEAF_VERSION_TAPSCRIPT).unwrap();
        let (output_key, parity) = taproot_output_key_with_parity(&internal, &root).unwrap();
        let mut prevout = vec![0x51, 0x20];
        prevout.extend_from_slice(&output_key);
        let mut control = vec![TAPROOT_LEAF_VERSION_TAPSCRIPT | parity];
        control.extend_from_slice(&internal);
        let stack = vec![sig.to_vec(), tapscript, control];
        let witness: Vec<Witness> = vec![stack];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx.clone(), Some(witness))
            .unwrap()
    };
    let known = admit(&compressed_x);
    let unknown_admitted = admit(&unknown);
    eprintln!("32-byte pubkey admitted={known}; 33-byte pubkey admitted={unknown_admitted}");
    assert!(known);
    assert!(!unknown_admitted);
}

#[test]
fn empty_signature_uncompressed_pubkey_is_not_standard() {
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    let script = vec![0xac, 0x75, 0x51];
    let program: [u8; 32] = Sha256::digest(&script).into();
    let mut prevout = vec![0x00, 0x20];
    prevout.extend_from_slice(&program);
    let compressed = {
        let mut key = vec![0x02];
        key.extend_from_slice(&[0x11; 32]);
        key
    };
    let uncompressed = {
        let mut key = vec![0x04];
        key.extend_from_slice(&[0x11; 64]);
        key
    };
    let admit = |pubkey: Vec<u8>| {
        let stack = vec![Vec::new(), pubkey, script.clone()];
        let witness: Vec<Witness> = vec![stack];
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = Vec::new();
        tx.outputs[0].value = 0;
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            100_000,
        ))));
        mempool
            .add_transaction_with_witness(tx, Some(witness))
            .unwrap()
    };
    let compressed_admitted = admit(compressed);
    let uncompressed_admitted = admit(uncompressed);
    eprintln!(
        "empty sig compressed admitted={compressed_admitted}; empty sig uncompressed admitted={uncompressed_admitted}"
    );
    assert!(compressed_admitted);
    assert!(!uncompressed_admitted);
}

#[test]
fn hybrid_pubkey_is_not_standard() {
    use blvm_protocol::opcodes::{OP_EQUAL, OP_HASH160};
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    let redeem = vec![0xac, 0x75, 0x51];
    let hash: [u8; 20] = Ripemd160::digest(Sha256::digest(&redeem)).into();
    let mut prevout = vec![OP_HASH160, 0x14];
    prevout.extend_from_slice(&hash);
    prevout.push(OP_EQUAL);
    let admit = |header: u8| {
        let mut pubkey = vec![header];
        pubkey.extend_from_slice(&[0x11; 64]);
        let mut script_sig = vec![0x00, pubkey.len() as u8];
        script_sig.extend_from_slice(&pubkey);
        script_sig.push(redeem.len() as u8);
        script_sig.extend_from_slice(&redeem);
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 50_000;
        tx.outputs[0].script_pubkey = {
            let mut script = vec![0x00, 0x14];
            script.extend_from_slice(&[0x22; 20]);
            script
        };
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let uncompressed = admit(0x04);
    let hybrid = admit(0x06);
    eprintln!("uncompressed pubkey admitted={uncompressed}; hybrid pubkey admitted={hybrid}");
    assert!(uncompressed);
    assert!(!hybrid);
}

#[test]
fn an_empty_snapshot_counts_a_mempool_parents_sigops() {
    use blvm_protocol::opcodes::{OP_0, OP_1, OP_CHECKSIG, OP_ENDIF, OP_EQUAL, OP_HASH160, OP_IF};
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;
    use std::sync::Arc;

    fn hash160(data: &[u8]) -> [u8; 20] {
        Ripemd160::digest(Sha256::digest(data)).into()
    }

    let admit = |input_count: usize, empty_snapshot: bool| {
        let mut redeem = vec![OP_0, OP_IF];
        redeem.extend(std::iter::repeat(OP_CHECKSIG).take(15));
        redeem.extend_from_slice(&[OP_ENDIF, OP_1]);
        let hash = hash160(&redeem);
        let mut spend = vec![OP_HASH160, 0x14];
        spend.extend_from_slice(&hash);
        spend.push(OP_EQUAL);
        let mut script_sig = vec![redeem.len() as u8];
        script_sig.extend_from_slice(&redeem);
        let (fund_script, fund_sig) = p2sh_of_op_1();
        let fund = OutPoint {
            hash: [1u8; 32],
            index: 0,
        };
        let mut coins = blvm_protocol::UtxoSet::default();
        coins.insert(
            fund,
            Arc::new(blvm_protocol::UTXO {
                value: (input_count as i64) * 1_000 + 50_000,
                script_pubkey: fund_script.into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let parent = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: fund,
                script_sig: fund_sig,
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![
                TransactionOutput {
                    value: 1_000,
                    script_pubkey: spend.clone(),
                };
                input_count
            ]
            .into(),
            lock_time: 0,
        };
        let parent_id = blvm_protocol::block::calculate_tx_id(&parent);
        let child = Transaction {
            version: 1,
            inputs: (0..input_count)
                .map(|index| TransactionInput {
                    prevout: OutPoint {
                        hash: parent_id,
                        index: index as u32,
                    },
                    script_sig: script_sig.clone(),
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
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(coins)));
        assert!(mempool.add_transaction(parent).unwrap());
        if empty_snapshot {
            mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(
                blvm_protocol::UtxoSet::default(),
            )));
        }
        mempool.add_transaction(child).unwrap()
    };
    let small = admit(1, true);
    let over_empty = admit(267, true);
    let over_seen = admit(267, false);
    eprintln!(
        "1 input on empty snapshot admitted={small}; 267 inputs on empty snapshot admitted={over_empty}; 267 inputs on the funding set admitted={over_seen}"
    );
    assert!(small);
    assert!(!over_empty);
    assert!(!over_seen);
}

#[test]
fn a_replacement_counts_clusters_not_transactions() {
    use std::sync::Arc;

    let chains = |clusters: usize, depth: usize| {
        let (script, script_sig) = p2sh_of_op_1();
        let mut utxo = blvm_protocol::UtxoSet::default();
        for cluster in 0..clusters {
            let mut coin = [0u8; 32];
            coin[0] = cluster as u8;
            coin[1] = (cluster >> 8) as u8;
            coin[2] = 0xab;
            utxo.insert(
                OutPoint {
                    hash: coin,
                    index: 0,
                },
                Arc::new(blvm_protocol::UTXO {
                    value: 300_000,
                    script_pubkey: script.clone().into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
        }
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo)));
        for cluster in 0..clusters {
            let mut coin = [0u8; 32];
            coin[0] = cluster as u8;
            coin[1] = (cluster >> 8) as u8;
            coin[2] = 0xab;
            let mut prev = OutPoint {
                hash: coin,
                index: 0,
            };
            let mut value = 300_000i64;
            for _ in 0..depth {
                value -= 1_000;
                let tx = Transaction {
                    version: 1,
                    inputs: vec![TransactionInput {
                        prevout: prev,
                        script_sig: script_sig.clone(),
                        sequence: 0xffffffff,
                    }]
                    .into(),
                    outputs: vec![TransactionOutput {
                        value,
                        script_pubkey: script.clone(),
                    }]
                    .into(),
                    lock_time: 0,
                };
                prev = OutPoint {
                    hash: blvm_protocol::block::calculate_tx_id(&tx),
                    index: 0,
                };
                assert!(mempool.add_transaction(tx).unwrap());
            }
        }
        let replacement = Transaction {
            version: 1,
            inputs: (0..clusters)
                .map(|cluster| {
                    let mut coin = [0u8; 32];
                    coin[0] = cluster as u8;
                    coin[1] = (cluster >> 8) as u8;
                    coin[2] = 0xab;
                    TransactionInput {
                        prevout: OutPoint {
                            hash: coin,
                            index: 0,
                        },
                        script_sig: script_sig.clone(),
                        sequence: 0xffffffff,
                    }
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
        let pooled = mempool.size();
        let admitted = mempool.add_transaction(replacement).unwrap();
        (pooled, admitted)
    };
    let four = chains(4, 25);
    let five = chains(5, 25);
    let at_limit = chains(100, 1);
    let over = chains(101, 1);
    eprintln!(
        "4 clusters of 25 pooled={} admitted={}; 5 clusters of 25 pooled={} admitted={}; 100 clusters admitted={}; 101 clusters admitted={}",
        four.0, four.1, five.0, five.1, at_limit.1, over.1
    );
    assert!(four.1);
    assert!(five.1);
    assert!(at_limit.1);
    assert!(!over.1);
}

#[test]
fn sigop_cost_over_16000_is_not_standard() {
    use std::sync::Arc;

    fn sigop_script(checksigs: usize) -> Vec<u8> {
        let mut script = vec![0x00, 0x63];
        script.extend(std::iter::repeat(0xac).take(checksigs));
        script.extend_from_slice(&[0x68, 0x51]);
        script
    }
    fn hash160(data: &[u8]) -> [u8; 20] {
        use ripemd::Digest as _;
        use sha2::Digest;
        ripemd::Ripemd160::digest(sha2::Sha256::digest(data)).into()
    }

    let admit = |tail: usize| {
        // Each redeem stays at or under the 15-sigop P2SH cap.
        // 266 * 15 + tail is 4,000 or 4,001 signature operations.
        // Cost is that count times 4: 16,000 is admitted, 16,004 is not.
        let mut counts = vec![15usize; 266];
        counts.push(tail);
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        let mut inputs = Vec::new();
        for (i, count) in counts.iter().enumerate() {
            let redeem = sigop_script(*count);
            let hash = hash160(&redeem);
            let mut prevout_script = vec![0xa9, 0x14];
            prevout_script.extend_from_slice(&hash);
            prevout_script.push(0x87);
            let mut script_sig = vec![redeem.len() as u8];
            script_sig.extend_from_slice(&redeem);
            let mut hash_id = [0u8; 32];
            hash_id[0] = i as u8;
            hash_id[1] = (i >> 8) as u8;
            hash_id[2] = 0x5a;
            let prevout = OutPoint {
                hash: hash_id,
                index: 0,
            };
            utxo_set.insert(
                prevout,
                Arc::new(blvm_protocol::UTXO {
                    value: 1_000,
                    script_pubkey: prevout_script.into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
            inputs.push(TransactionInput {
                prevout,
                script_sig,
                sequence: 0xffff_ffff,
            });
        }
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
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(utxo_set)));
        mempool.add_transaction(tx).unwrap()
    };
    let at_limit = admit(10);
    let over = admit(11);
    eprintln!("sigop cost 16000 admitted={at_limit}; sigop cost 16004 admitted={over}");
    assert!(at_limit);
    assert!(!over);
}

#[test]
fn script_sig_over_1650_bytes_is_not_standard() {
    use blvm_protocol::opcodes::OP_PUSHDATA2;

    fn pushdata2(len: usize) -> Vec<u8> {
        let mut out = vec![OP_PUSHDATA2, (len & 0xff) as u8, ((len >> 8) & 0xff) as u8];
        out.extend(std::iter::repeat(0x11).take(len));
        out
    }
    fn script_sig(total: usize) -> Vec<u8> {
        let redeem = vec![0x75, 0x75, 0x75, 0x75, 0x51];
        let mut script = Vec::new();
        for _ in 0..3 {
            script.extend(pushdata2(520));
        }
        let data_len = total - script.len() - 1 - 1 - redeem.len();
        script.push(data_len as u8);
        script.extend(std::iter::repeat(0x11).take(data_len));
        script.push(redeem.len() as u8);
        script.extend_from_slice(&redeem);
        assert_eq!(script.len(), total);
        script
    }

    let redeem = vec![0x75, 0x75, 0x75, 0x75, 0x51];
    let hash: [u8; 20] = {
        use ripemd::Digest as _;
        use sha2::Digest;
        ripemd::Ripemd160::digest(sha2::Sha256::digest(&redeem)).into()
    };
    let mut prevout = vec![0xa9, 0x14];
    prevout.extend_from_slice(&hash);
    prevout.push(0x87);
    let admit = |script_sig: Vec<u8>, standard: bool| {
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 50_000;
        tx.outputs[0].script_pubkey = {
            let mut script = vec![0x00, 0x14];
            script.extend_from_slice(&[0x22; 20]);
            script
        };
        let mut policy = crate::config::MempoolPolicyConfig::default();
        policy.require_standard = standard;
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(policy));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout.clone(),
            100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    assert!(admit(script_sig(1_650), true));
    assert!(admit(script_sig(1_651), false));
    assert!(!admit(script_sig(1_651), true));
}

#[test]
fn bare_one_of_one_is_a_standard_output() {
    use blvm_protocol::opcodes::{OP_1, OP_4, OP_CHECKMULTISIG, PUSH_33_BYTES};

    let key = [0x02u8; 33];
    let admit = |script: Vec<u8>| {
        let (prevout, script_sig) = p2sh_of_op_1();
        let mut tx = legacy_empty_script();
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 50_000;
        tx.outputs[0].script_pubkey = script;
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    let mut p2pk = vec![PUSH_33_BYTES];
    p2pk.extend_from_slice(&key);
    p2pk.push(0xac);
    assert!(admit(p2pk));
    let mut bare = vec![OP_1, PUSH_33_BYTES];
    bare.extend_from_slice(&key);
    bare.extend_from_slice(&[OP_1, OP_CHECKMULTISIG]);
    assert!(admit(bare));
    let mut too_wide = vec![OP_1];
    for _ in 0..4 {
        too_wide.push(PUSH_33_BYTES);
        too_wide.extend_from_slice(&key);
    }
    too_wide.extend_from_slice(&[OP_4, OP_CHECKMULTISIG]);
    assert!(!admit(too_wide));
}

#[test]
fn version_zero_is_not_a_standard_transaction() {
    let admit = |version: u64, standard: bool| {
        let (prevout, script_sig) = p2sh_of_op_1();
        let mut tx = legacy_empty_script();
        tx.version = version;
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 50_000;
        tx.outputs[0].script_pubkey = {
            let mut script = vec![0x00, 0x14];
            script.extend_from_slice(&[0x22; 20]);
            script
        };
        let mut policy = crate::config::MempoolPolicyConfig::default();
        policy.require_standard = standard;
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(policy));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            prevout, 100_000,
        ))));
        mempool.add_transaction(tx).unwrap()
    };
    assert!(admit(1, true));
    assert!(admit(2, true));
    assert!(admit(0, false));
    assert!(!admit(0, true));
}

#[test]
fn preview_rejects_a_nonstandard_version() {
    let run = |version: u64| {
        let (prevout, script_sig) = p2sh_of_op_1();
        let mut tx = legacy_empty_script();
        tx.version = version;
        tx.inputs[0].script_sig = script_sig;
        tx.outputs[0].value = 50_000;
        tx.outputs[0].script_pubkey = {
            let mut script = vec![0x00, 0x14];
            script.extend_from_slice(&[0x22; 20]);
            script
        };
        let utxo = funded(prevout, 100_000);
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(utxo.clone())));
        let preview = mempool.preview_accept(&tx, None, &utxo).is_ok();
        let admitted = mempool.add_transaction(tx).unwrap();
        (preview, admitted)
    };
    let standard = run(2);
    let next = run(3);
    eprintln!(
        "version 2 preview={} admitted={}; version 3 preview={} admitted={}",
        standard.0, standard.1, next.0, next.1
    );
    assert!(standard.0 && standard.1);
    assert!(!next.0 && !next.1);
}

#[test]
fn preview_rejects_a_scriptsig_that_is_not_push_only() {
    use blvm_secp256k1::ecdsa::{ecdsa_sign_compact_rfc6979, ge_to_compressed, pubkey_from_secret};
    use blvm_secp256k1::scalar::Scalar;

    fn der_int(value: &[u8]) -> Vec<u8> {
        let mut start = 0;
        while start + 1 < value.len() && value[start] == 0 {
            start += 1;
        }
        let mut body = value[start..].to_vec();
        if body.first().is_some_and(|byte| byte & 0x80 != 0) {
            body.insert(0, 0x00);
        }
        let mut out = vec![0x02, body.len() as u8];
        out.extend(body);
        out
    }
    fn der_sig(compact: &[u8; 64]) -> Vec<u8> {
        let mut body = der_int(&compact[..32]);
        body.extend(der_int(&compact[32..]));
        let mut out = vec![0x30, body.len() as u8];
        out.extend(body);
        out.push(0x01);
        out
    }

    let seckey = [0x22u8; 32];
    let mut secret = Scalar::zero();
    assert!(!secret.set_b32(&seckey));
    let pubkey = ge_to_compressed(&pubkey_from_secret(&secret)).to_vec();
    let mut script_pubkey = vec![0x21];
    script_pubkey.extend_from_slice(&pubkey);
    script_pubkey.push(0xac);

    let mut tx = legacy_empty_script();
    tx.inputs[0].script_sig = Vec::new();
    tx.outputs[0].value = 0;
    tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
    let sighash = blvm_consensus::transaction_hash::calculate_transaction_sighash_single_input(
        &tx,
        0,
        &script_pubkey,
        100_000,
        blvm_consensus::transaction_hash::SighashType::ALL,
        None,
    )
    .unwrap();
    let sig = der_sig(&ecdsa_sign_compact_rfc6979(&sighash, &seckey).unwrap());
    let mut push_only = vec![sig.len() as u8];
    push_only.extend_from_slice(&sig);
    let mut with_nop = push_only.clone();
    with_nop.push(0x61);

    let judge = |script_sig: Vec<u8>| {
        let mut spend = tx.clone();
        spend.inputs[0].script_sig = script_sig;
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        let utxo = funded(script_pubkey.clone(), 100_000);
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(utxo.clone())));
        let preview = mempool.preview_accept(&spend, None, &utxo).is_ok();
        let admitted = mempool.add_transaction(spend).unwrap();
        (preview, admitted)
    };
    let (push_preview, push_admitted) = judge(push_only);
    let (nop_preview, nop_admitted) = judge(with_nop);
    eprintln!(
        "push-only preview={push_preview} admitted={push_admitted}; nop preview={nop_preview} admitted={nop_admitted}"
    );
    assert!(push_preview && push_admitted);
    assert!(!nop_preview && !nop_admitted);
}

#[test]
fn mempool_rejects_a_prevout_sum_above_max_money() {
    use std::sync::Arc;

    use blvm_protocol::UTXO;
    use blvm_protocol::constants::MAX_MONEY;
    use blvm_protocol::opcodes::OP_1;

    let mut policy = crate::config::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_relay_fee_rate = 0;
    policy.min_tx_fee = 0;

    let mut over = legacy_empty_script();
    over.inputs.push(TransactionInput {
        prevout: OutPoint {
            hash: [2u8; 32],
            index: 0,
        },
        script_sig: Vec::new(),
        sequence: 0xffffffff,
    });
    over.outputs[0].value = 1_000;
    let mut utxo_set = funded(vec![OP_1], MAX_MONEY);
    utxo_set.insert(
        over.inputs[1].prevout,
        Arc::new(UTXO {
            value: MAX_MONEY,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let mempool = MempoolManager::new();
    mempool.set_policy_config(Some(policy.clone()));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));
    assert!(!mempool.add_transaction(over).unwrap());

    let mut at_cap = legacy_empty_script();
    at_cap.inputs.push(TransactionInput {
        prevout: OutPoint {
            hash: [3u8; 32],
            index: 0,
        },
        script_sig: Vec::new(),
        sequence: 0xffffffff,
    });
    at_cap.outputs[0].value = 1_000;
    let mut utxo_set = funded(vec![OP_1], MAX_MONEY / 2);
    utxo_set.insert(
        at_cap.inputs[1].prevout,
        Arc::new(UTXO {
            value: MAX_MONEY - (MAX_MONEY / 2),
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let mempool = MempoolManager::new();
    mempool.set_policy_config(Some(policy.clone()));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));
    assert!(mempool.add_transaction(at_cap).unwrap());

    let mempool = MempoolManager::new();
    mempool.set_policy_config(Some(policy));
    let mut utxo_set = funded(vec![OP_1], 60_000);
    utxo_set.insert(
        OutPoint {
            hash: [4u8; 32],
            index: 0,
        },
        Arc::new(UTXO {
            value: MAX_MONEY,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));
    let mut parent = legacy_empty_script();
    parent.outputs[0].value = 50_000;
    parent.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(parent.clone()).unwrap());

    let mut child = legacy_empty_script();
    child.inputs[0].prevout = OutPoint {
        hash: blvm_protocol::block::calculate_tx_id(&parent),
        index: 0,
    };
    child.inputs.push(TransactionInput {
        prevout: OutPoint {
            hash: [4u8; 32],
            index: 0,
        },
        script_sig: Vec::new(),
        sequence: 0xffffffff,
    });
    child.outputs[0].value = 1_000;
    assert!(!mempool.add_transaction(child).unwrap());
}

#[test]
fn selector_includes_a_child_that_spends_a_mempool_parent() {
    use std::sync::Arc;

    use crate::node::miner::{MempoolProvider, TransactionSelector};
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;

    let utxo_set = funded(vec![OP_1], 200_000);
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set.clone())));

    let mut parent = legacy_empty_script();
    parent.outputs[0].value = 190_000;
    parent.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(parent.clone()).unwrap());

    let mut child = legacy_empty_script();
    child.inputs[0].prevout = OutPoint {
        hash: calculate_tx_id(&parent),
        index: 0,
    };
    child.outputs[0].value = 100_000;
    assert!(mempool.add_transaction(child.clone()).unwrap());

    let rates = mempool.fee_rates_sat_vb(&utxo_set);
    assert!(rates.len() >= 2);
    assert!(rates.iter().all(|rate| *rate >= 1));

    let ordered = mempool.get_prioritized_transactions(2, &utxo_set);
    assert_eq!(calculate_tx_id(&ordered[0]), calculate_tx_id(&child));

    let parent_id = calculate_tx_id(&parent);
    let child_id = calculate_tx_id(&child);
    let selected = TransactionSelector::new().select_transactions(&mempool, &utxo_set);
    let selected_ids: Vec<_> = selected.iter().map(calculate_tx_id).collect();
    let parent_at = selected_ids.iter().position(|id| *id == parent_id).unwrap();
    let child_at = selected_ids.iter().position(|id| *id == child_id).unwrap();
    assert!(parent_at < child_at);
}

#[test]
fn selector_includes_a_parent_paid_for_by_its_child() {
    use std::sync::Arc;

    use crate::config::MempoolPolicyConfig;
    use crate::node::miner::{MempoolProvider, TransactionSelector};
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;

    let mut policy = MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;

    let utxo_set = funded(vec![OP_1], 60_000);
    let mempool = MempoolManager::new();
    mempool.set_policy_config(Some(policy));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set.clone())));

    let mut parent = legacy_empty_script();
    parent.outputs[0].value = 59_999;
    parent.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(parent.clone()).unwrap());

    let selector = TransactionSelector::with_params(1_000_000, 4_000_000, 5_000);
    let alone = selector.select_transactions(&mempool, &utxo_set);
    assert!(alone.is_empty());

    let mut child = legacy_empty_script();
    child.inputs[0].prevout = OutPoint {
        hash: calculate_tx_id(&parent),
        index: 0,
    };
    child.outputs[0].value = 10_000;
    child.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(child.clone()).unwrap());

    let parent_id = calculate_tx_id(&parent);
    let child_id = calculate_tx_id(&child);
    let selected = selector.select_transactions(&mempool, &utxo_set);
    let selected_ids: Vec<_> = selected.iter().map(calculate_tx_id).collect();
    let parent_at = selected_ids.iter().position(|id| *id == parent_id).unwrap();
    let child_at = selected_ids.iter().position(|id| *id == child_id).unwrap();
    assert!(parent_at < child_at);
}

#[test]
fn selector_includes_two_parents_paid_for_by_their_child() {
    use std::sync::Arc;

    use crate::config::MempoolPolicyConfig;
    use crate::node::miner::{MempoolProvider, TransactionSelector};
    use blvm_protocol::TransactionInput;
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::constants::SEQUENCE_FINAL;
    use blvm_protocol::opcodes::OP_1;

    let mut policy = MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;

    let mut utxo_set = funded(vec![OP_1], 30_000);
    utxo_set.insert(
        OutPoint {
            hash: [2u8; 32],
            index: 0,
        },
        Arc::new(blvm_protocol::UTXO {
            value: 30_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let mempool = MempoolManager::new();
    mempool.set_policy_config(Some(policy));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set.clone())));

    let mut parent_a = legacy_empty_script();
    parent_a.outputs[0].value = 29_999;
    parent_a.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(parent_a.clone()).unwrap());

    let mut parent_b = legacy_empty_script();
    parent_b.inputs[0].prevout.hash = [2u8; 32];
    parent_b.outputs[0].value = 29_999;
    parent_b.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(parent_b.clone()).unwrap());

    let selector = TransactionSelector::with_params(1_000_000, 4_000_000, 5_000);
    assert!(selector.select_transactions(&mempool, &utxo_set).is_empty());

    let parent_a_id = calculate_tx_id(&parent_a);
    let parent_b_id = calculate_tx_id(&parent_b);
    let mut child = legacy_empty_script();
    child.inputs[0].prevout = OutPoint {
        hash: parent_a_id,
        index: 0,
    };
    child.inputs.push(TransactionInput {
        prevout: OutPoint {
            hash: parent_b_id,
            index: 0,
        },
        script_sig: Vec::new(),
        sequence: SEQUENCE_FINAL as u64,
    });
    child.outputs[0].value = 10_000;
    child.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(child.clone()).unwrap());

    let child_id = calculate_tx_id(&child);
    let selected = selector.select_transactions(&mempool, &utxo_set);
    let selected_ids: Vec<_> = selected.iter().map(calculate_tx_id).collect();
    let parent_a_at = selected_ids
        .iter()
        .position(|id| *id == parent_a_id)
        .unwrap();
    let parent_b_at = selected_ids
        .iter()
        .position(|id| *id == parent_b_id)
        .unwrap();
    let child_at = selected_ids.iter().position(|id| *id == child_id).unwrap();
    assert!(parent_a_at < child_at);
    assert!(parent_b_at < child_at);
    assert_eq!(selected.len(), 3);
}

#[test]
fn selector_includes_a_grandparent_paid_for_by_its_grandchild() {
    use std::sync::Arc;

    use crate::config::MempoolPolicyConfig;
    use crate::node::miner::{MempoolProvider, TransactionSelector};
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;

    let mut policy = MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;

    let utxo_set = funded(vec![OP_1], 40_000);
    let mempool = MempoolManager::new();
    mempool.set_policy_config(Some(policy));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set.clone())));

    let mut grandparent = legacy_empty_script();
    grandparent.outputs[0].value = 39_999;
    grandparent.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(grandparent.clone()).unwrap());

    let mut middle = legacy_empty_script();
    middle.inputs[0].prevout = OutPoint {
        hash: calculate_tx_id(&grandparent),
        index: 0,
    };
    middle.outputs[0].value = 39_998;
    middle.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(middle.clone()).unwrap());

    let selector = TransactionSelector::with_params(1_000_000, 4_000_000, 5_000);
    assert!(selector.select_transactions(&mempool, &utxo_set).is_empty());

    let mut grandchild = legacy_empty_script();
    grandchild.inputs[0].prevout = OutPoint {
        hash: calculate_tx_id(&middle),
        index: 0,
    };
    grandchild.outputs[0].value = 10_000;
    grandchild.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(grandchild.clone()).unwrap());

    let grandparent_id = calculate_tx_id(&grandparent);
    let middle_id = calculate_tx_id(&middle);
    let grandchild_id = calculate_tx_id(&grandchild);
    let selected = selector.select_transactions(&mempool, &utxo_set);
    let selected_ids: Vec<_> = selected.iter().map(calculate_tx_id).collect();
    let grandparent_at = selected_ids
        .iter()
        .position(|id| *id == grandparent_id)
        .unwrap();
    let middle_at = selected_ids.iter().position(|id| *id == middle_id).unwrap();
    let grandchild_at = selected_ids
        .iter()
        .position(|id| *id == grandchild_id)
        .unwrap();
    assert!(grandparent_at < middle_at);
    assert!(middle_at < grandchild_at);
    assert_eq!(selected.len(), 3);
}

#[test]
fn ranking_recomputes_a_tx_admitted_while_the_utxo_lock_is_held() {
    use std::sync::Arc;

    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;

    let mut utxo_set = funded(vec![OP_1], 50_000);
    utxo_set.insert(
        OutPoint {
            hash: [2u8; 32],
            index: 0,
        },
        Arc::new(blvm_protocol::UTXO {
            value: 80_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let utxo_arc = Arc::new(tokio::sync::Mutex::new(utxo_set.clone()));
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::clone(&utxo_arc));

    let mut low_fee = legacy_empty_script();
    low_fee.outputs[0].value = 40_000;
    assert!(mempool.add_transaction(low_fee).unwrap());
    assert_eq!(mempool.get_prioritized_transactions(10, &utxo_set).len(), 1);

    let mut high_fee = legacy_empty_script();
    high_fee.inputs[0].prevout = OutPoint {
        hash: [2u8; 32],
        index: 0,
    };
    high_fee.outputs[0].value = 10_000;
    let _held = utxo_arc.blocking_lock();
    assert!(!mempool.add_transaction(high_fee.clone()).unwrap());
    drop(_held);
    assert!(mempool.add_transaction(high_fee.clone()).unwrap());

    let ranked = mempool.get_prioritized_transactions(1, &utxo_set);
    assert_eq!(ranked.len(), 1);
    assert_eq!(calculate_tx_id(&ranked[0]), calculate_tx_id(&high_fee));
}

#[test]
fn mempool_rejects_admission_while_the_utxo_lock_is_held() {
    use std::sync::Arc;

    use blvm_protocol::opcodes::{OP_0, OP_1};

    let mut utxo_set = funded(vec![OP_1], 50_000);
    utxo_set.insert(
        OutPoint {
            hash: [2u8; 32],
            index: 0,
        },
        Arc::new(blvm_protocol::UTXO {
            value: 50_000,
            script_pubkey: vec![OP_0].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let utxo_arc = Arc::new(tokio::sync::Mutex::new(utxo_set));
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::clone(&utxo_arc));

    let valid = legacy_empty_script();
    let mut unspendable = legacy_empty_script();
    unspendable.inputs[0].prevout = OutPoint {
        hash: [2u8; 32],
        index: 0,
    };
    let _held = utxo_arc.blocking_lock();
    assert!(!mempool.add_transaction(valid.clone()).unwrap());
    assert!(!mempool.add_transaction(unspendable.clone()).unwrap());
    drop(_held);
    assert_eq!(mempool.size(), 0);

    assert!(!mempool.add_transaction(unspendable).unwrap());
    assert!(mempool.add_transaction(valid).unwrap());
    assert_eq!(mempool.size(), 1);
}

#[test]
fn package_is_refused_while_the_utxo_lock_is_held() {
    use std::sync::Arc;

    let utxo_arc = Arc::new(tokio::sync::Mutex::new(blvm_protocol::UtxoSet::default()));
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::clone(&utxo_arc));
    let first = legacy_empty_script();
    let mut second = legacy_empty_script();
    second.inputs[0].prevout.hash = [2u8; 32];
    let _held = utxo_arc.blocking_lock();
    let flags = mempool
        .add_package_with_witnesses(vec![(first, None), (second, None)])
        .unwrap();
    assert_eq!(flags, vec![false, false]);
    drop(_held);
}

#[test]
fn mempool_rejects_a_spend_when_the_wired_utxo_set_is_empty() {
    use std::sync::Arc;

    use blvm_protocol::opcodes::OP_1;

    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(
        blvm_protocol::UtxoSet::default(),
    )));
    let tx = legacy_empty_script();
    assert!(!mempool.add_transaction(tx.clone()).unwrap());
    assert_eq!(mempool.size(), 0);

    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_1],
        50_000,
    ))));
    assert!(mempool.add_transaction(tx).unwrap());
}

#[test]
fn connected_block_removes_confirmed_and_conflicting_spends() {
    use std::sync::Arc;

    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;

    let mut utxo_set = funded(vec![OP_1], 100_000);
    utxo_set.insert(
        OutPoint {
            hash: [3u8; 32],
            index: 0,
        },
        Arc::new(blvm_protocol::UTXO {
            value: 50_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));

    let mut confirmed = legacy_empty_script();
    confirmed.outputs[0].value = 80_000;
    confirmed.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(confirmed.clone()).unwrap());

    let mut child = legacy_empty_script();
    child.inputs[0].prevout = OutPoint {
        hash: calculate_tx_id(&confirmed),
        index: 0,
    };
    child.outputs[0].value = 60_000;
    assert!(mempool.add_transaction(child.clone()).unwrap());

    let mut unrelated = legacy_empty_script();
    unrelated.inputs[0].prevout = OutPoint {
        hash: [3u8; 32],
        index: 0,
    };
    unrelated.outputs[0].value = 40_000;
    assert!(mempool.add_transaction(unrelated.clone()).unwrap());

    mempool.remove_for_connected_block(&[confirmed.clone()]);
    assert!(
        mempool
            .get_transaction(&calculate_tx_id(&confirmed))
            .is_none()
    );
    assert!(mempool.get_transaction(&calculate_tx_id(&child)).is_some());
    assert!(
        mempool
            .get_transaction(&calculate_tx_id(&unrelated))
            .is_some()
    );

    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_1],
        100_000,
    ))));
    let mut conflict = legacy_empty_script();
    conflict.outputs[0].value = 80_000;
    conflict.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(conflict.clone()).unwrap());
    let mut conflict_child = legacy_empty_script();
    conflict_child.inputs[0].prevout = OutPoint {
        hash: calculate_tx_id(&conflict),
        index: 0,
    };
    conflict_child.outputs[0].value = 60_000;
    assert!(mempool.add_transaction(conflict_child.clone()).unwrap());

    let mut in_block = conflict.clone();
    in_block.outputs[0].value = 70_000;
    mempool.remove_for_connected_block(&[in_block]);
    assert!(
        mempool
            .get_transaction(&calculate_tx_id(&conflict))
            .is_none()
    );
    assert!(
        mempool
            .get_transaction(&calculate_tx_id(&conflict_child))
            .is_none()
    );
    assert_eq!(mempool.size(), 0);
}

#[test]
fn reorg_restores_disconnected_spends_and_drops_conflicts() {
    use std::sync::Arc;

    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{Block, BlockHeader};

    fn block_with(txs: Vec<Transaction>) -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 0,
                bits: 0,
                nonce: 0,
            },
            transactions: txs.into_boxed_slice(),
        }
    }

    let mut before = funded(vec![OP_1], 100_000);
    before.insert(
        OutPoint {
            hash: [2u8; 32],
            index: 0,
        },
        Arc::new(blvm_protocol::UTXO {
            value: 100_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(before)));

    let mut conflict = legacy_empty_script();
    conflict.inputs[0].prevout.hash = [2u8; 32];
    conflict.outputs[0].value = 80_000;
    conflict.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(conflict.clone()).unwrap());
    let mut conflict_child = legacy_empty_script();
    conflict_child.inputs[0].prevout = OutPoint {
        hash: calculate_tx_id(&conflict),
        index: 0,
    };
    conflict_child.outputs[0].value = 60_000;
    assert!(mempool.add_transaction(conflict_child.clone()).unwrap());

    let mut coinbase = legacy_empty_script();
    coinbase.inputs[0].prevout.hash = [0u8; 32];
    coinbase.inputs[0].prevout.index = 0xffff_ffff;
    coinbase.inputs[0].script_sig = vec![OP_1, OP_1];
    coinbase.outputs[0].value = 50_000;

    let mut resurrected = legacy_empty_script();
    resurrected.outputs[0].value = 80_000;
    resurrected.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    let mut resurrected_child = legacy_empty_script();
    resurrected_child.inputs[0].prevout = OutPoint {
        hash: calculate_tx_id(&resurrected),
        index: 0,
    };
    resurrected_child.outputs[0].value = 60_000;

    let mut connected_spend = legacy_empty_script();
    connected_spend.inputs[0].prevout.hash = [2u8; 32];
    connected_spend.outputs[0].value = 70_000;

    let after = funded(vec![OP_1], 100_000);
    mempool.apply_reorg(
        &[block_with(vec![
            coinbase,
            resurrected.clone(),
            resurrected_child.clone(),
        ])],
        &[],
        &[block_with(vec![connected_spend])],
        &after,
    );

    assert!(
        mempool
            .get_transaction(&calculate_tx_id(&conflict))
            .is_none()
    );
    assert!(
        mempool
            .get_transaction(&calculate_tx_id(&conflict_child))
            .is_none()
    );
    assert!(
        mempool
            .get_transaction(&calculate_tx_id(&resurrected))
            .is_some()
    );
    assert!(
        mempool
            .get_transaction(&calculate_tx_id(&resurrected_child))
            .is_some()
    );
    assert_eq!(mempool.size(), 2);
}

#[test]
fn reorg_restores_a_segwit_spend_with_its_witness() {
    use std::sync::Arc;

    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::{OP_0, OP_1, PUSH_32_BYTES};
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::{Block, BlockHeader};
    use sha2::{Digest, Sha256};

    fn block_with(txs: Vec<Transaction>) -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 0,
                bits: 0,
                nonce: 0,
            },
            transactions: txs.into_boxed_slice(),
        }
    }

    let witness_script = vec![OP_1];
    let script_hash = Sha256::digest(&witness_script);
    let mut script_pubkey = vec![OP_0, PUSH_32_BYTES];
    script_pubkey.extend_from_slice(&script_hash);

    let utxo = funded(script_pubkey, 100_000);
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo.clone())));

    let mut spend = legacy_empty_script();
    spend.outputs[0].value = 80_000;
    spend.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    let witness: Vec<Witness> = vec![vec![witness_script]];

    let mut coinbase = legacy_empty_script();
    coinbase.inputs[0].prevout.hash = [0u8; 32];
    coinbase.inputs[0].prevout.index = 0xffff_ffff;
    coinbase.inputs[0].script_sig = vec![OP_1, OP_1];
    coinbase.outputs[0].value = 50_000;

    mempool.apply_reorg(
        &[block_with(vec![coinbase, spend.clone()])],
        &[vec![Vec::new(), witness.clone()]],
        &[],
        &utxo,
    );

    let txid = calculate_tx_id(&spend);
    assert!(mempool.get_transaction(&txid).is_some());
    assert_eq!(mempool.get_transaction_witnesses(&txid), Some(witness));
    assert_eq!(mempool.size(), 1);
}

#[test]
fn mempool_rejects_a_witness_program_spend_without_a_witness() {
    use std::sync::Arc;

    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::{OP_0, OP_1, PUSH_32_BYTES};
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    let witness_script = vec![OP_1];
    let script_hash = Sha256::digest(&witness_script);
    let mut script_pubkey = vec![OP_0, PUSH_32_BYTES];
    script_pubkey.extend_from_slice(&script_hash);

    let mut utxo = funded(script_pubkey, 100_000);
    utxo.insert(
        OutPoint {
            hash: [2u8; 32],
            index: 0,
        },
        Arc::new(blvm_protocol::UTXO {
            value: 50_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo)));

    let mut legacy = legacy_empty_script();
    legacy.inputs[0].prevout.hash = [2u8; 32];
    legacy.outputs[0].value = 40_000;
    legacy.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(legacy).unwrap());

    let mut spend = legacy_empty_script();
    spend.outputs[0].value = 80_000;
    spend.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(!mempool.add_transaction(spend.clone()).unwrap());

    let witness: Vec<Witness> = vec![vec![witness_script]];
    assert!(
        mempool
            .add_transaction_with_witness(spend.clone(), Some(witness.clone()))
            .unwrap()
    );
    assert_eq!(
        mempool.get_transaction_witnesses(&calculate_tx_id(&spend)),
        Some(witness)
    );
    assert_eq!(mempool.size(), 2);
}

#[test]
fn mempool_rejects_a_p2sh_witness_redeem_without_a_witness() {
    use std::sync::Arc;

    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::{OP_0, OP_1, OP_EQUAL, OP_HASH160, PUSH_20_BYTES, PUSH_32_BYTES};
    use blvm_protocol::segwit::Witness;
    use ripemd::{Digest, Ripemd160};
    use sha2::Sha256;

    fn hash160(data: &[u8]) -> [u8; 20] {
        let sha = Sha256::digest(data);
        Ripemd160::digest(sha).into()
    }

    fn p2sh_script(redeem: &[u8]) -> Vec<u8> {
        let hash = hash160(redeem);
        let mut script = vec![OP_HASH160, PUSH_20_BYTES];
        script.extend_from_slice(&hash);
        script.push(OP_EQUAL);
        script
    }

    fn push_data(data: &[u8]) -> Vec<u8> {
        let mut script = Vec::with_capacity(data.len() + 1);
        script.push(data.len() as u8);
        script.extend_from_slice(data);
        script
    }

    let witness_script = vec![OP_1];
    let program = Sha256::digest(&witness_script);
    let mut redeem = vec![OP_0, PUSH_32_BYTES];
    redeem.extend_from_slice(&program);

    let plain_redeem = vec![OP_1];
    let mut utxo = funded(p2sh_script(&redeem), 100_000);
    utxo.insert(
        OutPoint {
            hash: [2u8; 32],
            index: 0,
        },
        Arc::new(blvm_protocol::UTXO {
            value: 50_000,
            script_pubkey: p2sh_script(&plain_redeem).into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo)));

    let mut plain = legacy_empty_script();
    plain.inputs[0].prevout.hash = [2u8; 32];
    plain.inputs[0].script_sig = push_data(&plain_redeem);
    plain.outputs[0].value = 40_000;
    plain.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(plain).unwrap());

    let mut spend = legacy_empty_script();
    spend.inputs[0].script_sig = push_data(&redeem);
    spend.outputs[0].value = 80_000;
    spend.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(!mempool.add_transaction(spend.clone()).unwrap());

    let witness: Vec<Witness> = vec![vec![witness_script]];
    assert!(
        mempool
            .add_transaction_with_witness(spend.clone(), Some(witness.clone()))
            .unwrap()
    );
    assert_eq!(
        mempool.get_transaction_witnesses(&calculate_tx_id(&spend)),
        Some(witness)
    );
    assert_eq!(mempool.size(), 2);
}

#[test]
fn mempool_rejects_a_witness_spend_of_a_mempool_parent_without_a_witness() {
    use std::sync::Arc;

    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::{OP_0, OP_1, PUSH_32_BYTES};
    use blvm_protocol::segwit::Witness;
    use sha2::{Digest, Sha256};

    let witness_script = vec![OP_1];
    let program = Sha256::digest(&witness_script);
    let mut parent_script = vec![OP_0, PUSH_32_BYTES];
    parent_script.extend_from_slice(&program);

    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_1],
        100_000,
    ))));

    let mut parent = legacy_empty_script();
    parent.outputs[0].value = 80_000;
    parent.outputs[0].script_pubkey = parent_script;
    assert!(mempool.add_transaction(parent.clone()).unwrap());

    let mut child = legacy_empty_script();
    child.inputs[0].prevout = OutPoint {
        hash: calculate_tx_id(&parent),
        index: 0,
    };
    child.outputs[0].value = 60_000;
    child.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(!mempool.add_transaction(child.clone()).unwrap());

    let witness: Vec<Witness> = vec![vec![witness_script]];
    assert!(
        mempool
            .add_transaction_with_witness(child.clone(), Some(witness.clone()))
            .unwrap()
    );
    assert_eq!(
        mempool.get_transaction_witnesses(&calculate_tx_id(&child)),
        Some(witness)
    );
    assert_eq!(mempool.size(), 2);
}

#[test]
fn mempool_rejects_an_unknown_prevout() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;

    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_1],
        5_000,
    ))));

    let mut mixed = legacy_empty_script();
    mixed.inputs.push(TransactionInput {
        prevout: OutPoint {
            hash: [9u8; 32],
            index: 0,
        },
        script_sig: Vec::new(),
        sequence: 0xffffffff,
    });
    assert!(!mempool.add_transaction(mixed).unwrap());

    let mut parent = legacy_empty_script();
    parent.outputs[0].value = 3_000;
    parent.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(parent.clone()).unwrap());

    let mut child = legacy_empty_script();
    child.inputs[0].prevout = OutPoint {
        hash: calculate_tx_id(&parent),
        index: 0,
    };
    child.outputs[0].value = 1_000;
    assert!(mempool.add_transaction(child).unwrap());
}

#[test]
fn mempool_rejects_an_immature_coinbase_spend() {
    use blvm_protocol::constants::COINBASE_MATURITY;

    let created = 1_000u64;
    let immature = created + COINBASE_MATURITY - 1;
    let mature = created + COINBASE_MATURITY;
    let tx = legacy_empty_script();

    let mempool = MempoolManager::new();
    mempool.set_chain_tip(immature, 0);
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(
        funded_coinbase(created, true),
    )));
    assert!(!mempool.add_transaction(tx.clone()).unwrap());

    let mempool = MempoolManager::new();
    mempool.set_chain_tip(mature, 0);
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(
        funded_coinbase(created, true),
    )));
    assert!(mempool.add_transaction(tx.clone()).unwrap());

    let mempool = MempoolManager::new();
    mempool.set_chain_tip(immature, 0);
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(
        funded_coinbase(created, false),
    )));
    assert!(mempool.add_transaction(tx).unwrap());
}

#[test]
fn an_immature_coinbase_needs_a_tip() {
    let tx = legacy_empty_script();
    let admit = |is_coinbase: bool| {
        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(
            funded_coinbase(0, is_coinbase),
        )));
        mempool.add_transaction(tx.clone()).unwrap()
    };
    let ordinary = admit(false);
    let coinbase = admit(true);
    eprintln!("ordinary coin admitted={ordinary}; coinbase coin admitted={coinbase}");
    assert!(ordinary);
    assert!(!coinbase);
}

#[test]
fn mempool_rejects_a_relative_locktime() {
    use blvm_protocol::opcodes::OP_1;

    let next_height = 500_000;
    let mut young = legacy_empty_script();
    young.version = 2;
    young.inputs[0].sequence = 10;
    let mempool = MempoolManager::new();
    mempool.set_chain_tip(next_height, 1_700_000_000);
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded_at(
        vec![OP_1],
        2_000,
        499_995,
    ))));
    assert!(!mempool.add_transaction(young.clone()).unwrap());

    let mempool = MempoolManager::new();
    mempool.set_chain_tip(next_height, 1_700_000_000);
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded_at(
        vec![OP_1],
        2_000,
        499_000,
    ))));
    assert!(mempool.add_transaction(young.clone()).unwrap());

    young.version = 1;
    let mempool = MempoolManager::new();
    mempool.set_chain_tip(next_height, 1_700_000_000);
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded_at(
        vec![OP_1],
        2_000,
        499_995,
    ))));
    assert!(mempool.add_transaction(young).unwrap());

    let mut time_locked = legacy_empty_script();
    time_locked.version = 2;
    time_locked.inputs[0].sequence = (1 << 22) | 1;
    let mempool = MempoolManager::new();
    mempool.set_chain_tip(next_height, 1_700_000_000);
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded_at(
        vec![OP_1],
        2_000,
        499_000,
    ))));
    assert!(!mempool.add_transaction(time_locked).unwrap());
}

#[test]
fn mempool_rejects_a_non_final_locktime() {
    use blvm_protocol::constants::{LOCKTIME_THRESHOLD, SEQUENCE_FINAL};

    let not_final = (SEQUENCE_FINAL as u64).saturating_sub(1);
    let mut locked = legacy_empty_script();
    locked.lock_time = 200;
    locked.inputs[0].sequence = not_final;

    let mempool = MempoolManager::new();
    mempool.set_chain_tip(100, 0);
    assert!(!mempool.add_transaction(locked.clone()).unwrap());

    let mempool = MempoolManager::new();
    mempool.set_chain_tip(100, 0);
    locked.inputs[0].sequence = SEQUENCE_FINAL as u64;
    assert!(mempool.add_transaction(locked).unwrap());

    let mempool = MempoolManager::new();
    mempool.set_chain_tip(100, 0);
    let mut height_final = legacy_empty_script();
    height_final.lock_time = 50;
    height_final.inputs[0].sequence = not_final;
    assert!(mempool.add_transaction(height_final).unwrap());

    let mempool = MempoolManager::new();
    mempool.set_chain_tip(100, 0);
    let mut time_locked = legacy_empty_script();
    time_locked.lock_time = LOCKTIME_THRESHOLD as u64;
    time_locked.inputs[0].sequence = not_final;
    assert!(!mempool.add_transaction(time_locked.clone()).unwrap());

    let mempool = MempoolManager::new();
    mempool.set_chain_tip(100, (LOCKTIME_THRESHOLD as u64).saturating_add(1));
    assert!(mempool.add_transaction(time_locked).unwrap());
}

#[test]
fn empty_witness_stacks_use_stripped_weight() {
    let mempool = MempoolManager::new();
    let tx = legacy_empty_script();
    let stripped = (serialize_transaction(&tx).len() as u64).saturating_mul(4);
    assert_eq!(
        mempool.transaction_weight(&tx, Some(&[Vec::new()])),
        stripped
    );
    assert!(mempool.estimate_transaction_weight(&tx) > stripped);
}

#[test]
fn fee_index_ignores_a_negative_output() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::constants::SEQUENCE_FINAL;
    use blvm_protocol::opcodes::OP_1;
    use std::sync::Arc;

    let mempool = MempoolManager::new();
    let funding = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let negative_prevout = OutPoint {
        hash: [2u8; 32],
        index: 0,
    };
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    for prevout in [funding, negative_prevout] {
        utxo_set.insert(
            prevout,
            Arc::new(blvm_protocol::UTXO {
                value: 50_000,
                script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
    }

    let positive = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: funding,
            script_sig: Vec::new(),
            sequence: SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 40_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61],
        }]
        .into(),
        lock_time: 0,
    };
    assert!(mempool.add_transaction(positive.clone()).unwrap());

    let negative = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: negative_prevout,
            script_sig: Vec::new(),
            sequence: SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![
            TransactionOutput {
                value: 1_000,
                script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61],
            },
            TransactionOutput {
                value: -1,
                script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61],
            },
        ]
        .into(),
        lock_time: 0,
    };
    let negative_id = calculate_tx_id(&negative);
    mempool
        .pool_lock()
        .transactions
        .insert(negative_id, negative);

    mempool.fee_rates_sat_vb(&utxo_set);
    let rates = mempool.fee_cache.read().unwrap_or_else(|e| e.into_inner());
    assert_eq!(rates.get(&negative_id).copied(), Some(0));
    assert!(rates.get(&calculate_tx_id(&positive)).copied().unwrap() > 0);
}

fn open_policy() -> crate::config::MempoolPolicyConfig {
    let mut policy = crate::config::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_relay_fee_rate = 0;
    policy.min_tx_fee = 0;
    policy.reject_spam_in_mempool = false;
    policy.require_standard = false;
    policy
}

fn bulky(index: u32, output_value: i64) -> Transaction {
    use blvm_protocol::opcodes::OP_1;
    let mut prev = [0u8; 32];
    prev[..4].copy_from_slice(&index.to_le_bytes());
    let mut tx = legacy_empty_script();
    tx.inputs[0].prevout.hash = prev;
    tx.outputs[0].value = output_value;
    tx.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    tx.outputs[0].script_pubkey.extend(vec![0u8; 7_000]);
    tx
}

#[tokio::test]
async fn mempool_byte_cap_removes_lowest_fee_transactions() {
    use blvm_protocol::opcodes::OP_1;
    let mut policy = open_policy();
    policy.max_mempool_mb = 1;
    let mempool = MempoolManager::new();
    mempool.set_policy_config(Some(policy));
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    let cheap_value = 999_999i64;
    let rich_value = 950_000i64;
    for index in 0..200u32 {
        let value = if index < 100 { cheap_value } else { rich_value };
        let tx = bulky(index, value);
        utxo_set.insert(
            tx.inputs[0].prevout,
            std::sync::Arc::new(blvm_protocol::UTXO {
                value: 1_000_000,
                script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
                height: 1,
                is_coinbase: false,
            }),
        );
    }
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(utxo_set)));
    for index in 0..200u32 {
        let value = if index < 100 { cheap_value } else { rich_value };
        assert!(
            mempool.add_transaction(bulky(index, value)).unwrap(),
            "tx {index} admitted"
        );
    }
    assert!(mempool.mempool_size_bytes() > 1_048_576);
    mempool.process_once().await.unwrap();
    let bytes = mempool.mempool_size_bytes();
    assert!(bytes <= 1_048_576, "pool is {bytes} bytes");
    let remaining = mempool.get_transactions();
    assert!(remaining.len() < 200);
    let cheap = remaining
        .iter()
        .filter(|tx| tx.outputs[0].value == cheap_value)
        .count();
    let rich = remaining
        .iter()
        .filter(|tx| tx.outputs[0].value == rich_value)
        .count();
    assert_eq!(cheap + rich, remaining.len());
    assert!(cheap < 100);
    if cheap > 0 {
        assert_eq!(rich, 100);
    }
}

#[tokio::test]
async fn mempool_evicts_parent_together_with_child() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    let mut policy = open_policy();
    // Insert stops at the transaction cap. Admit the package, then tighten
    // the cap so eviction has to remove the parent and its child together.
    policy.max_mempool_txs = 2;
    let mempool = MempoolManager::new();
    mempool.set_policy_config(Some(policy.clone()));
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_1],
        200_000,
    ))));
    let mut parent = legacy_empty_script();
    parent.outputs[0].value = 190_000;
    parent.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(parent.clone()).unwrap());
    let mut child = legacy_empty_script();
    child.inputs[0].prevout = OutPoint {
        hash: calculate_tx_id(&parent),
        index: 0,
    };
    child.outputs[0].value = 100_000;
    child.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(child.clone()).unwrap());
    policy.max_mempool_txs = 1;
    mempool.set_policy_config(Some(policy));
    mempool.process_once().await.unwrap();
    assert!(mempool.get_transaction(&calculate_tx_id(&parent)).is_none());
    assert!(mempool.get_transaction(&calculate_tx_id(&child)).is_none());
}

#[tokio::test]
async fn mempool_expiry_removes_a_backdated_transaction() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    let mut policy = open_policy();
    policy.mempool_expiry_hours = 1;
    let mempool = MempoolManager::new();
    mempool.set_policy_config(Some(policy));
    let mut utxo_set = funded(vec![OP_1], 50_000);
    let mut young = legacy_empty_script();
    young.inputs[0].prevout.hash = [2u8; 32];
    utxo_set.insert(
        young.inputs[0].prevout,
        std::sync::Arc::new(blvm_protocol::UTXO {
            value: 50_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 1,
            is_coinbase: false,
        }),
    );
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(utxo_set)));
    let old = legacy_empty_script();
    assert!(mempool.add_transaction(old.clone()).unwrap());
    assert!(mempool.add_transaction(young.clone()).unwrap());
    mempool.set_entry_time(&calculate_tx_id(&old), 0);
    mempool.process_once().await.unwrap();
    assert!(mempool.get_transaction(&calculate_tx_id(&old)).is_none());
    assert!(mempool.get_transaction(&calculate_tx_id(&young)).is_some());
}

#[tokio::test]
async fn expiry_removes_descendants_and_keeps_a_young_parent() {
    use std::sync::Arc;

    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;

    let mut policy = open_policy();
    policy.mempool_expiry_hours = 1;
    let mempool = MempoolManager::new();
    mempool.set_policy_config(Some(policy));

    let mut utxo_set = funded(vec![OP_1], 200_000);
    let kept_prevout = OutPoint {
        hash: [2u8; 32],
        index: 0,
    };
    utxo_set.insert(
        kept_prevout,
        Arc::new(blvm_protocol::UTXO {
            value: 50_000,
            script_pubkey: vec![0x51, 0x61, 0x61, 0x61, 0x61].into(),
            height: 1,
            is_coinbase: false,
        }),
    );
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));

    let mut parent = legacy_empty_script();
    parent.outputs[0].value = 190_000;
    parent.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(parent.clone()).unwrap());
    let parent_id = calculate_tx_id(&parent);
    let mut child = legacy_empty_script();
    child.inputs[0].prevout = OutPoint {
        hash: parent_id,
        index: 0,
    };
    child.outputs[0].value = 180_000;
    child.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(child.clone()).unwrap());
    let child_id = calculate_tx_id(&child);
    let mut grandchild = legacy_empty_script();
    grandchild.inputs[0].prevout = OutPoint {
        hash: child_id,
        index: 0,
    };
    grandchild.outputs[0].value = 100_000;
    grandchild.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(grandchild.clone()).unwrap());
    let grandchild_id = calculate_tx_id(&grandchild);

    let mut kept_parent = legacy_empty_script();
    kept_parent.inputs[0].prevout = kept_prevout;
    kept_parent.outputs[0].value = 40_000;
    kept_parent.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(kept_parent.clone()).unwrap());
    let kept_parent_id = calculate_tx_id(&kept_parent);
    let mut expired_child = legacy_empty_script();
    expired_child.inputs[0].prevout = OutPoint {
        hash: kept_parent_id,
        index: 0,
    };
    expired_child.outputs[0].value = 20_000;
    expired_child.outputs[0].script_pubkey = vec![0x51, 0x61, 0x61, 0x61, 0x61];
    assert!(mempool.add_transaction(expired_child.clone()).unwrap());
    let expired_child_id = calculate_tx_id(&expired_child);

    mempool.set_entry_time(&parent_id, 0);
    mempool.set_entry_time(&expired_child_id, 0);
    mempool.process_once().await.unwrap();

    assert!(mempool.get_transaction(&parent_id).is_none());
    assert!(mempool.get_transaction(&child_id).is_none());
    assert!(mempool.get_transaction(&grandchild_id).is_none());
    assert!(mempool.get_transaction(&kept_parent_id).is_some());
    assert!(mempool.get_transaction(&expired_child_id).is_none());
    assert_eq!(mempool.size(), 1);
}

#[tokio::test]
async fn mempool_expiry_overflow_keeps_a_young_transaction() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;
    let mut policy = open_policy();
    policy.mempool_expiry_hours = u64::MAX / 3600 + 1;
    let mempool = MempoolManager::new();
    mempool.set_policy_config(Some(policy));
    mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
        vec![OP_1],
        50_000,
    ))));
    let young = legacy_empty_script();
    assert!(mempool.add_transaction(young.clone()).unwrap());
    mempool.process_once().await.unwrap();
    assert!(mempool.get_transaction(&calculate_tx_id(&young)).is_some());
}
