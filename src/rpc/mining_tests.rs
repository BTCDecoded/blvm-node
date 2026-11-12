use super::MiningRpc;
use crate::node::mempool::MempoolManager;
use crate::storage::Storage;
use blvm_protocol::opcodes::OP_1;
use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO, UtxoSet};
use std::sync::Arc;

fn spend(prevout: OutPoint, output_value: i64) -> Transaction {
    Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout,
            script_sig: Vec::new(),
            sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: output_value,
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

#[test]
fn mining_fee_counts_a_mempool_parent_and_ignores_an_out_of_range_value() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());

    let funding = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let negative = OutPoint {
        hash: [9u8; 32],
        index: 0,
    };
    let mut utxo_set = UtxoSet::default();
    utxo_set.insert(
        funding,
        Arc::new(UTXO {
            value: 50_000,
            script_pubkey: vec![OP_1].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    utxo_set.insert(
        negative,
        Arc::new(UTXO {
            value: -1,
            script_pubkey: vec![OP_1].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    for (outpoint, utxo) in &utxo_set {
        storage.utxos().add_utxo(outpoint, utxo).unwrap();
    }
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));

    let parent = spend(funding, 40_000);
    assert!(mempool.add_transaction(parent.clone()).unwrap());
    let child = spend(
        OutPoint {
            hash: blvm_protocol::block::calculate_tx_id(&parent),
            index: 0,
        },
        25_000,
    );

    let mining = MiningRpc::with_dependencies(storage, mempool);
    assert_eq!(mining.calculate_transaction_fee(&child), 15_000);
    assert_eq!(mining.calculate_transaction_fee(&spend(negative, 1_000)), 0);
}

#[test]
fn witness_sigop_cost_is_the_template_sigops() {
    use blvm_protocol::opcodes::OP_CHECKSIG;
    use blvm_protocol::segwit::Witness;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let prevout = OutPoint {
        hash: [4u8; 32],
        index: 0,
    };
    let mut script_pubkey = vec![0x00, 0x20];
    script_pubkey.extend_from_slice(&[0u8; 32]);
    let utxo = UTXO {
        value: 50_000,
        script_pubkey: script_pubkey.into(),
        height: 1,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&prevout, &utxo).unwrap();
    let tx = spend(prevout, 40_000);
    let witness: Witness = vec![vec![OP_CHECKSIG]];
    let mining = MiningRpc::with_dependencies(storage, mempool);
    assert_eq!(blvm_protocol::sigop::get_legacy_sigop_count(&tx), 0);
    assert_eq!(
        mining.count_sigops(&tx, Some(std::slice::from_ref(&witness))),
        1
    );
}

#[test]
fn mining_fee_does_not_wrap_a_negative_output() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());

    let funding = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 50_000,
        script_pubkey: vec![OP_1].into(),
        height: 0,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&funding, &utxo).unwrap();
    let mut utxo_set = UtxoSet::default();
    utxo_set.insert(funding, Arc::new(utxo));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));

    let mut wrapped = spend(funding, 1_000);
    wrapped.outputs.push(TransactionOutput {
        value: -1,
        script_pubkey: vec![OP_1],
    });

    let mining = MiningRpc::with_dependencies(storage, mempool);
    assert_eq!(mining.calculate_transaction_fee(&wrapped), 0);
    assert_eq!(
        mining.calculate_transaction_fee(&spend(funding, 40_000)),
        10_000
    );
}

#[tokio::test]
async fn submitblock_without_storage_returns_an_error() {
    let mining = MiningRpc::new();
    assert!(
        mining
            .submit_block(&serde_json::json!(["00"]))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn listfeeestimators_and_a_named_estimator_are_one_call_views() {
    let (_dir, _storage, mempool, mining) = regtest_mining();
    let listed = mining.list_fee_estimators().unwrap();
    let rows = listed.as_array().unwrap();
    assert_eq!(rows[0]["name"], "native");
    assert_eq!(rows[0]["active"], true);

    let verbose = mining
        .estimate_smart_fee(&serde_json::json!([1, "conservative", "none", 1]))
        .await
        .unwrap();
    assert_eq!(verbose["requested_estimator"], "auto");
    assert_eq!(verbose["active_estimator"], "native");
    assert_eq!(verbose["degraded"], false);

    let unknown = mining
        .estimate_smart_fee(&serde_json::json!([1, "conservative", "no-such"]))
        .await
        .unwrap_err();
    assert!(
        unknown.message.contains("registered estimator"),
        "{}",
        unknown.message
    );

    let estimator = mempool.fee_estimator().unwrap();
    let id = "fees_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    estimator.register_estimators(
        id,
        vec![crate::node::fee_estimator::FeeEstimatorSpec {
            name: "blend".to_string(),
            description: "blend".to_string(),
            modes: vec![crate::node::fee_estimator::FeeMode::Conservative],
            default_merge: crate::node::fee_estimator::ExternalMode::Replace,
            auto_priority: None,
        }],
    );
    estimator
        .submit_fee_table(
            id,
            crate::node::fee_estimator::ExternalFeeTable {
                estimator: "blend".to_string(),
                tip_hash: [0u8; 32],
                upstream_height: 0,
                mode: None,
                quotes: vec![(1, 80_000)],
            },
            std::time::Instant::now(),
        )
        .unwrap();
    let named = mining
        .estimate_smart_fee(&serde_json::json!([1, "conservative", "blend", 1]))
        .await
        .unwrap();
    assert_eq!(named["source"], "external");
    assert_eq!(named["active_estimator"], "native");
    assert!((named["feerate"].as_f64().unwrap() - 0.0008).abs() < 1e-12);
    let again = mining.list_fee_estimators().unwrap();
    assert!(
        again
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "blend" && row["fresh"] == true && row["active"] == false)
    );
}

fn regtest_mining() -> (
    tempfile::TempDir,
    Arc<Storage>,
    Arc<MempoolManager>,
    MiningRpc,
) {
    use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let genesis = protocol.get_network_params().genesis_block.header.clone();
    let dir = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(dir.path()).unwrap());
    storage.chain().initialize(&genesis).unwrap();
    let mempool = Arc::new(MempoolManager::new());
    let mut fees = crate::config::FeeEstimatorConfig::default();
    fees.health_required = Some(false);
    fees.projection_full_threshold = 0.0;
    let estimator = Arc::new(crate::node::fee_estimator::FeeEstimator::new(fees, true));
    estimator.mark_table_stale_for_test();
    mempool.set_fee_estimator(Some(estimator));
    let mining = MiningRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&mempool))
        .with_protocol_engine(protocol);
    (dir, storage, mempool, mining)
}

fn one_block_params() -> serde_json::Value {
    serde_json::json!([
        1u64,
        "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4",
        2_000_000u64
    ])
}

#[tokio::test]
async fn empty_pool_is_coinbase_only_and_the_hash_matches_get_block() {
    use crate::rpc::blockchain::BlockchainRpc;
    use crate::storage::hashing::hash_to_rpc_hex;
    use blvm_protocol::block::calculate_tx_id;

    let (_dir, storage, _mempool, mining) = regtest_mining();
    let mined = mining
        .generate_to_address(&one_block_params())
        .await
        .unwrap();
    let block_hash = mined[0].as_str().unwrap().to_string();
    let tip = storage.chain().get_tip_hash().unwrap().unwrap();
    let block = storage.blocks().get_block(&tip).unwrap().unwrap();
    assert_eq!(block.transactions.len(), 1);
    assert_eq!(block_hash, hash_to_rpc_hex(&tip));

    let chain = BlockchainRpc::with_dependencies(Arc::clone(&storage));
    let best = chain.get_best_block_hash().await.unwrap();
    assert_eq!(best.as_str(), Some(block_hash.as_str()));
    let shown = chain.get_block(&block_hash).await.unwrap();
    assert_eq!(
        shown["merkleroot"].as_str(),
        Some(hash_to_rpc_hex(&block.header.merkle_root).as_str())
    );
    assert_eq!(
        shown["tx"][0].as_str(),
        Some(hash_to_rpc_hex(&calculate_tx_id(&block.transactions[0])).as_str())
    );
}

#[tokio::test]
async fn fee_paying_pool_transaction_is_mined() {
    let (_dir, storage, mempool, mining) = regtest_mining();
    let funding = OutPoint {
        hash: [3u8; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 50_000,
        script_pubkey: vec![OP_1].into(),
        height: 0,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&funding, &utxo).unwrap();
    let mut set = UtxoSet::default();
    set.insert(funding, Arc::new(utxo));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
    let tx = spend(funding, 40_000);
    assert!(mempool.add_transaction(tx.clone()).unwrap());

    mining
        .generate_to_address(&one_block_params())
        .await
        .unwrap();
    let tip = storage.chain().get_tip_hash().unwrap().unwrap();
    let block = storage.blocks().get_block(&tip).unwrap().unwrap();
    assert!(
        block.transactions.len() > 1,
        "pool transaction must follow the coinbase"
    );
    assert_eq!(block.transactions[1].outputs[0].value, 40_000);
}

#[tokio::test]
async fn pruned_body_keeps_the_template_at_the_next_height() {
    let (_dir, storage, _mempool, mining) = regtest_mining();
    mining
        .generate_to_address(&one_block_params())
        .await
        .unwrap();
    let tip = storage.chain().get_tip_hash().unwrap().unwrap();
    let height = storage.chain().get_height().unwrap().unwrap();
    storage.blocks().remove_block_body(&tip).unwrap();
    let template = mining
        .get_block_template(&serde_json::json!([]))
        .await
        .unwrap();
    assert_eq!(template["height"].as_u64(), Some(height + 1));
}

#[tokio::test]
async fn submitblock_returns_null_only_after_the_block_connects() {
    use crate::module::api::events::EventManager;
    use crate::module::traits::EventType;
    use crate::node::event_publisher::EventPublisher;
    use blvm_protocol::serialization::serialize_block_with_witnesses;
    use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};

    let (_dir, storage, _mempool, mining) = regtest_mining();
    mining
        .generate_to_address(&one_block_params())
        .await
        .unwrap();
    let tip = storage.chain().get_tip_hash().unwrap().unwrap();
    let block = storage.blocks().get_block(&tip).unwrap().unwrap();
    let witnesses = storage
        .blocks()
        .get_witness(&tip)
        .unwrap()
        .unwrap_or_else(|| vec![vec![]; block.transactions.len()]);
    let bytes = serialize_block_with_witnesses(&block, &witnesses, true);

    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let genesis = protocol.get_network_params().genesis_block.header.clone();
    let dir = tempfile::TempDir::new().unwrap();
    let fresh = Arc::new(Storage::new(dir.path()).unwrap());
    fresh.chain().initialize(&genesis).unwrap();
    let genesis_hash = fresh.chain().get_tip_hash().unwrap().unwrap();
    fresh
        .blocks()
        .store_header(&genesis_hash, &genesis)
        .unwrap();
    fresh.blocks().store_height(0, &genesis_hash).unwrap();
    fresh.blocks().store_recent_header(0, &genesis).unwrap();
    let events = Arc::new(EventManager::new());
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    events
        .subscribe_module("lock".into(), vec![EventType::BlockMined], tx)
        .await
        .unwrap();
    let publisher = Arc::new(EventPublisher::new(Arc::clone(&events)));
    let submit = MiningRpc::with_dependencies(Arc::clone(&fresh), Arc::new(MempoolManager::new()))
        .with_protocol_engine(protocol)
        .with_event_publisher(Some(publisher));
    let result = submit
        .submit_block(&serde_json::json!([hex::encode(&bytes)]))
        .await
        .unwrap();
    assert!(result.is_null());
    assert_eq!(fresh.chain().get_height().unwrap().unwrap(), 1);
    assert!(rx.try_recv().is_ok());

    let rejected = submit
        .submit_block(&serde_json::json!([hex::encode(&bytes)]))
        .await;
    assert!(rejected.is_err());
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn stored_parent_is_handed_to_connect_when_it_is_not_the_tip() {
    use blvm_protocol::serialization::serialize_block_with_witnesses;
    use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};

    let (_dir, storage, _mempool, mining) = regtest_mining();
    mining
        .generate_to_address(&one_block_params())
        .await
        .unwrap();
    let tip = storage.chain().get_tip_hash().unwrap().unwrap();
    let block = storage.blocks().get_block(&tip).unwrap().unwrap();
    let parent = block.header.prev_block_hash;
    let genesis = BitcoinProtocolEngine::new(ProtocolVersion::Regtest)
        .unwrap()
        .get_network_params()
        .genesis_block
        .header
        .clone();
    storage.blocks().store_header(&parent, &genesis).unwrap();
    let mut sibling = block;
    // Regtest's easiest target still rejects about half of the nonces.
    // One past the mined nonce is not a header this node can store.
    let mut met_target = false;
    for _ in 0..200_000 {
        sibling.header.nonce = sibling.header.nonce.wrapping_add(1);
        if blvm_protocol::pow::check_proof_of_work(&sibling.header).unwrap_or(false) {
            met_target = true;
            break;
        }
    }
    assert!(met_target, "sibling did not meet the regtest target");
    let bytes =
        serialize_block_with_witnesses(&sibling, &vec![vec![]; sibling.transactions.len()], true);
    let result = mining
        .submit_block(&serde_json::json!([hex::encode(&bytes)]))
        .await;
    match &result {
        Ok(value) => assert!(value.is_null()),
        Err(err) => assert!(!err.to_string().contains("current chain tip"), "{err}"),
    }
    assert!(result.is_ok());
}

#[tokio::test]
async fn one_sat_per_virtual_byte_quotes_before_the_mode_multiplier() {
    use blvm_protocol::segwit::transaction_weight_from_stacks;

    let (_dir, storage, mempool, mining) = regtest_mining();
    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    let funding = OutPoint {
        hash: [4u8; 32],
        index: 0,
    };
    let mut tx = spend(funding, 50_000);
    tx.inputs[0].script_sig = vec![OP_1];
    let vsize = transaction_weight_from_stacks(&tx, None)
        .unwrap()
        .div_ceil(4)
        .max(1) as i64;
    tx.outputs[0].value = 50_000;
    let utxo = UTXO {
        value: 50_000 + vsize,
        script_pubkey: vec![OP_1].into(),
        height: 0,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&funding, &utxo).unwrap();
    let mut set = UtxoSet::default();
    set.insert(funding, Arc::new(utxo));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
    mempool.set_policy_config(Some(policy));
    assert!(mempool.add_transaction(tx).unwrap());

    let quote = mining
        .estimate_smart_fee(&serde_json::json!([1u64, "unset"]))
        .await
        .unwrap();
    assert!((quote["feerate"].as_f64().unwrap() - 0.00001).abs() < 1e-12);
    assert_eq!(quote["blocks"].as_u64(), Some(1));
    let conservative = mining
        .estimate_smart_fee(&serde_json::json!([1u64, "CONSERVATIVE"]))
        .await
        .unwrap();
    assert!((conservative["feerate"].as_f64().unwrap() - 0.00001).abs() < 1e-12);
}

#[tokio::test]
async fn estimate_smart_fee_prices_a_snapshot_coin_the_table_does_not_hold() {
    use blvm_protocol::segwit::transaction_weight_from_stacks;

    let (_dir, storage, mempool, mining) = regtest_mining();
    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    let funding = OutPoint {
        hash: [5u8; 32],
        index: 0,
    };
    let mut tx = spend(funding, 50_000);
    tx.inputs[0].script_sig = vec![OP_1];
    let vsize = transaction_weight_from_stacks(&tx, None)
        .unwrap()
        .div_ceil(4)
        .max(1) as i64;
    tx.outputs[0].value = 50_000;
    let utxo = UTXO {
        value: 50_000 + vsize,
        script_pubkey: vec![OP_1].into(),
        height: 0,
        is_coinbase: false,
    };
    let tree = storage.open_tree("ibd_utxos_ckpt_a").unwrap();
    let key = crate::storage::disk_utxo::outpoint_to_key(&funding);
    let bytes =
        crate::storage::utxo_value_codec::encode_utxo_with_codec(storage.utxo_value_codec(), &utxo)
            .unwrap();
    tree.insert(&key, &bytes).unwrap();
    mempool.set_storage(Arc::clone(&storage));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(UtxoSet::default())));
    mempool.set_policy_config(Some(policy));
    assert!(mempool.add_transaction(tx).unwrap());

    let quote = mining
        .estimate_smart_fee(&serde_json::json!([1u64, "Unset"]))
        .await
        .unwrap();
    assert!(
        (quote["feerate"].as_f64().unwrap() - 0.00001).abs() < 1e-12,
        "{quote}"
    );
}

#[tokio::test]
async fn economical_estimate_stays_at_the_relay_floor() {
    use blvm_protocol::segwit::transaction_weight_from_stacks;

    let (_dir, storage, mempool, mining) = regtest_mining();
    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    // The transaction pays 1 sat/vB, which is this relay floor.
    policy.min_relay_fee_rate = 1_000;
    let funding = OutPoint {
        hash: [4u8; 32],
        index: 0,
    };
    let mut tx = spend(funding, 50_000);
    tx.inputs[0].script_sig = vec![OP_1];
    let vsize = transaction_weight_from_stacks(&tx, None)
        .unwrap()
        .div_ceil(4)
        .max(1) as i64;
    tx.outputs[0].value = 50_000;
    let utxo = UTXO {
        value: 50_000 + vsize,
        script_pubkey: vec![OP_1].into(),
        height: 0,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&funding, &utxo).unwrap();
    let mut set = UtxoSet::default();
    set.insert(funding, Arc::new(utxo));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
    mempool.set_policy_config(Some(policy));
    assert!(mempool.add_transaction(tx).unwrap());

    let quote = mining
        .estimate_smart_fee(&serde_json::json!([1u64, "economical"]))
        .await
        .unwrap();
    assert!((quote["feerate"].as_f64().unwrap() - 0.00001).abs() < 1e-12);
}

#[tokio::test]
async fn estimate_uses_the_spent_prevout_when_another_row_is_bad() {
    let (_dir, storage, mempool, mining) = regtest_mining();
    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    mempool.set_policy_config(Some(policy));
    let funding = OutPoint {
        hash: [8u8; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 50_000,
        script_pubkey: vec![OP_1].into(),
        height: 1,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&funding, &utxo).unwrap();
    storage
        .utxos()
        .insert_undecodable_utxo(&OutPoint {
            hash: [9u8; 32],
            index: 1,
        })
        .unwrap();
    let mut set = UtxoSet::default();
    set.insert(funding, Arc::new(utxo));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
    let tx = spend(funding, 40_000);
    assert!(mempool.add_transaction(tx).unwrap());
    let quote = mining
        .estimate_smart_fee(&serde_json::json!([1u64, "unset"]))
        .await
        .unwrap();
    assert!(quote["feerate"].as_f64().unwrap() > 0.0, "{quote}");
}

#[tokio::test]
async fn getblocktemplate_reads_the_utxo_set_once() {
    let (_dir, _storage, _mempool, mining) = regtest_mining();
    crate::storage::utxostore::reset_get_all_utxos_calls();
    mining
        .get_block_template(&serde_json::json!([]))
        .await
        .unwrap();
    assert_eq!(crate::storage::utxostore::get_all_utxos_calls(), 1);
}

#[tokio::test]
async fn generatetoaddress_loads_the_utxo_table_once() {
    let (_dir, _storage, _mempool, mining) = regtest_mining();
    crate::storage::utxostore::reset_get_all_utxos_calls();
    mining
        .generate_to_address(&serde_json::json!([
            2u64,
            "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4",
            2_000_000u64
        ]))
        .await
        .unwrap();
    assert_eq!(crate::storage::utxostore::get_all_utxos_calls(), 1);
}

#[tokio::test]
async fn empty_pool_is_not_a_fee_estimate_or_a_mined_block() {
    let (_dir, _storage, _mempool, mining) = regtest_mining();
    let quote = mining
        .estimate_smart_fee(&serde_json::json!([6u64, "conservative"]))
        .await
        .unwrap();
    assert!(quote.get("feerate").is_none());
    assert!(!quote["errors"].as_array().unwrap().is_empty());
    assert_eq!(quote["blocks"].as_u64(), Some(6));
    assert!(
        mining
            .estimate_smart_fee(&serde_json::json!([0u64]))
            .await
            .is_err()
    );

    let info = mining.get_mining_info().await.unwrap();
    assert_eq!(info["pooledtx"].as_u64(), Some(0));
    assert_eq!(info["currentblocktx"].as_u64(), Some(0));
    assert_eq!(info["currentblocksize"].as_u64(), Some(0));
    assert_eq!(info["currentblockweight"].as_u64(), Some(0));
}

#[tokio::test]
async fn the_next_block_quotes_the_higher_rate_and_a_longer_target_keeps_it() {
    use blvm_protocol::segwit::transaction_weight_from_stacks;

    let (_dir, storage, mempool, mining) = regtest_mining();
    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    mempool.set_policy_config(Some(policy));
    let coins = Arc::new(tokio::sync::Mutex::new(UtxoSet::default()));
    mempool.set_utxo_set_arc(Arc::clone(&coins));

    for (byte, sats_per_vb) in [([5u8; 32], 1i64), ([6u8; 32], 10)] {
        let funding = OutPoint {
            hash: byte,
            index: 0,
        };
        let mut tx = spend(funding, 50_000);
        tx.inputs[0].script_sig = vec![OP_1];
        let vsize = transaction_weight_from_stacks(&tx, None)
            .unwrap()
            .div_ceil(4)
            .max(1) as i64;
        tx.outputs[0].value = 50_000;
        let utxo = UTXO {
            value: 50_000 + vsize * sats_per_vb,
            script_pubkey: vec![OP_1].into(),
            height: 0,
            is_coinbase: false,
        };
        storage.utxos().add_utxo(&funding, &utxo).unwrap();
        coins.lock().await.insert(funding, Arc::new(utxo));
        assert!(mempool.add_transaction(tx).unwrap());
    }

    let soon = mining
        .estimate_smart_fee(&serde_json::json!([1u64, "unset"]))
        .await
        .unwrap();
    let later = mining
        .estimate_smart_fee(&serde_json::json!([1008u64, "unset"]))
        .await
        .unwrap();
    assert!(
        (soon["feerate"].as_f64().unwrap() - 0.00010).abs() < 1e-12,
        "{soon}"
    );
    assert!(
        (later["feerate"].as_f64().unwrap() - soon["feerate"].as_f64().unwrap()).abs() < 1e-12,
        "{later}"
    );
    assert_eq!(later["blocks"].as_u64(), Some(1008));
}

#[tokio::test]
async fn estimator_and_verbosity_select_the_column_and_the_diagnostics() {
    use crate::node::fee_estimator::{BlockObservation, TxAdmission};
    use blvm_protocol::segwit::transaction_weight_from_stacks;

    let (_dir, storage, mempool, mining) = regtest_mining();
    let estimator = mempool.fee_estimator().unwrap();
    let mut tx_weights = Vec::new();
    for index in 0..20u8 {
        let mut txid = [8u8; 32];
        txid[0] = index;
        estimator.on_tx_admitted(TxAdmission {
            txid,
            fee_sats: 500,
            vsize: 100,
            admit_height: 0,
            parents: Vec::new(),
            from_reload: false,
            from_package: false,
        });
        tx_weights.push((txid, 400));
    }
    estimator.on_block_connected(&BlockObservation {
        height: 1,
        hash: [1u8; 32],
        weight: 8_000,
        tx_weights,
    });
    for height in 2..=5 {
        estimator.on_block_connected(&BlockObservation {
            height,
            hash: [height as u8; 32],
            weight: 8_000,
            tx_weights: Vec::new(),
        });
    }

    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    mempool.set_policy_config(Some(policy));
    let funding = OutPoint {
        hash: [6u8; 32],
        index: 0,
    };
    let mut tx = spend(funding, 50_000);
    tx.inputs[0].script_sig = vec![OP_1];
    let vsize = transaction_weight_from_stacks(&tx, None)
        .unwrap()
        .div_ceil(4)
        .max(1) as i64;
    tx.outputs[0].value = 50_000 - 10 * vsize;
    let utxo = UTXO {
        value: 50_000,
        script_pubkey: vec![OP_1].into(),
        height: 0,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&funding, &utxo).unwrap();
    let mut set = UtxoSet::default();
    set.insert(funding, Arc::new(utxo));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
    assert!(mempool.add_transaction(tx).unwrap());

    let plain = mining
        .estimate_smart_fee(&serde_json::json!([1u64, "unset"]))
        .await
        .unwrap();
    assert!(plain.get("source").is_none());
    assert!((plain["feerate"].as_f64().unwrap() - 0.00005).abs() < 1e-12);

    let combined = mining
        .estimate_smart_fee(&serde_json::json!([1u64, "unset", "none", 1]))
        .await
        .unwrap();
    assert_eq!(combined["source"].as_str(), Some("min"));
    assert_eq!(combined["mempool_healthy"].as_bool(), Some(true));
    assert_eq!(combined["blocks"].as_u64(), Some(1));
    assert!(combined.get("tip_height").is_none());

    let history = mining
        .estimate_smart_fee(&serde_json::json!([1u64, "CONSERVATIVE", "History", 1]))
        .await
        .unwrap();
    assert_eq!(history["source"].as_str(), Some("history"));
    assert!((history["feerate"].as_f64().unwrap() - 0.00005).abs() < 1e-12);
    assert_eq!(history["blocks"].as_u64(), Some(2));

    let mempool_view = mining
        .estimate_smart_fee(&serde_json::json!([1u64, "economical", "MEMPOOL", 2]))
        .await
        .unwrap();
    assert_eq!(mempool_view["source"].as_str(), Some("mempool"));
    assert!((mempool_view["feerate"].as_f64().unwrap() - 0.00010).abs() < 1e-12);
    assert_eq!(mempool_view["tip_height"].as_u64(), Some(5));
    assert_eq!(
        mempool_view["tip_hash"].as_str(),
        Some("0505050505050505050505050505050505050505050505050505050505050505")
    );
    assert!(mempool_view["table_age_secs"].as_f64().unwrap() < 30.0);
    assert_eq!(mempool_view["health"]["window"].as_u64(), Some(6));
    assert!((mempool_view["health"]["ratio"].as_f64().unwrap() - 0.2).abs() < 1e-9);
    assert!((mempool_view["health"]["required_ratio"].as_f64().unwrap() - 0.75).abs() < 1e-12);
}

#[tokio::test]
async fn a_missing_quote_names_the_reason_and_a_bad_parameter_is_rejected() {
    let (_dir, _storage, mempool, mining) = regtest_mining();
    let empty = mining
        .estimate_smart_fee(&serde_json::json!([6u64, "conservative", "none", 1]))
        .await
        .unwrap();
    assert!(empty.get("feerate").is_none());
    assert_eq!(empty["blocks"].as_u64(), Some(6));
    assert_eq!(empty["reason"].as_str(), Some("empty_mempool"));
    assert_eq!(
        empty["errors"][0].as_str(),
        Some("Insufficient data or no feerate found")
    );

    let mut fees = crate::config::FeeEstimatorConfig::default();
    fees.enabled = false;
    fees.health_required = Some(false);
    mempool
        .fee_estimator()
        .unwrap()
        .configure(fees, true)
        .unwrap();
    let disabled = mining
        .estimate_smart_fee(&serde_json::json!([6u64, "conservative", "none", 2]))
        .await
        .unwrap();
    assert_eq!(disabled["reason"].as_str(), Some("disabled"));
    assert!(disabled.get("health").is_none());

    let bare = MiningRpc::new();
    let missing = bare
        .estimate_smart_fee(&serde_json::json!([4u64]))
        .await
        .unwrap();
    assert!(missing.get("reason").is_none());
    assert_eq!(missing["blocks"].as_u64(), Some(4));
    let named = bare
        .estimate_smart_fee(&serde_json::json!([4u64, "unset", "none", 1]))
        .await
        .unwrap();
    assert_eq!(named["reason"].as_str(), Some("disabled"));

    assert!(
        mining
            .estimate_smart_fee(&serde_json::json!([1u64, "unset", "median"]))
            .await
            .is_err()
    );
    assert!(
        mining
            .estimate_smart_fee(&serde_json::json!([1u64, "unset", "none", 3]))
            .await
            .is_err()
    );
    assert!(
        mining
            .estimate_smart_fee(&serde_json::json!([1u64, "unset", "none", "loud"]))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn one_pool_transaction_sets_the_template_size() {
    let (_dir, _storage, mempool, mining) = regtest_mining();
    let funding = OutPoint {
        hash: [7u8; 32],
        index: 0,
    };
    let mut tx = spend(funding, 40_000);
    tx.inputs[0].script_sig = vec![OP_1];
    assert!(mempool.add_transaction(tx.clone()).unwrap());

    let info = mining.get_mining_info().await.unwrap();
    let coinbase = MiningRpc::template_coinbase(1);
    let expected = blvm_protocol::serialization::serialize_transaction(&tx).len()
        + blvm_protocol::serialization::serialize_transaction(&coinbase).len();
    assert_eq!(info["pooledtx"].as_u64(), Some(1));
    assert_eq!(info["currentblocktx"].as_u64(), Some(2));
    assert_eq!(info["currentblocksize"].as_u64(), Some(expected as u64));
    assert_eq!(
        info["currentblockweight"].as_u64(),
        Some((expected as u64).saturating_mul(4))
    );
}

#[test]
fn a_template_sigop_count_sees_a_mempool_parent() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::{OP_0, PUSH_32_BYTES};
    use sha2::{Digest, Sha256};

    let spend = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let mut script = vec![0x00, 0x63];
    for _ in 0..199 {
        script.push(0x60);
        script.push(0xae);
    }
    script.push(0x68);
    script.push(0x51);
    let mut output_script = vec![OP_0, PUSH_32_BYTES];
    output_script.extend_from_slice(&Sha256::digest(&script));
    let witness: blvm_protocol::segwit::Witness = vec![script];

    let child_of = |prevout: OutPoint| Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout,
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 50_000,
            script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
        }]
        .into(),
        lock_time: 0,
    };

    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let confirmed_out = OutPoint {
        hash: [3u8; 32],
        index: 0,
    };
    storage
        .utxos()
        .add_utxo(
            &confirmed_out,
            &UTXO {
                value: 100_000,
                script_pubkey: output_script.clone().into(),
                height: 1,
                is_coinbase: false,
            },
        )
        .unwrap();
    let confirmed_mining =
        MiningRpc::with_dependencies(Arc::clone(&storage), Arc::new(MempoolManager::new()));
    let confirmed = confirmed_mining.count_sigops(
        &child_of(confirmed_out),
        Some(std::slice::from_ref(&witness)),
    );

    let funding = OutPoint {
        hash: [4u8; 32],
        index: 0,
    };
    let mempool = Arc::new(MempoolManager::new());
    let mut utxo_set = UtxoSet::default();
    utxo_set.insert(
        funding,
        Arc::new(UTXO {
            value: 200_000,
            script_pubkey: spend.into(),
            height: 1,
            is_coinbase: false,
        }),
    );
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));
    let parent = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: funding,
            script_sig: vec![0x01, 0x51],
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 100_000,
            script_pubkey: output_script,
        }]
        .into(),
        lock_time: 0,
    };
    let parent_id = calculate_tx_id(&parent);
    assert!(mempool.add_transaction(parent).unwrap());
    let child = child_of(OutPoint {
        hash: parent_id,
        index: 0,
    });
    let mining = MiningRpc::with_dependencies(storage, mempool);
    let reported = mining.count_sigops(&child, Some(std::slice::from_ref(&witness)));
    eprintln!("confirmed reported={confirmed}; mempool parent reported={reported}");
    assert_eq!(confirmed, 3184);
    assert_eq!(reported, 3184);
}

#[tokio::test]
async fn a_mining_info_weight_counts_the_witness() {
    use blvm_protocol::segwit::transaction_weight_from_stacks;

    let tx_of = || {
        let funding = OutPoint {
            hash: [7u8; 32],
            index: 0,
        };
        let mut tx = spend(funding, 40_000);
        tx.inputs[0].script_sig = vec![OP_1];
        tx.outputs[0].script_pubkey = vec![0x6a, 0x03, 0x00, 0x00, 0x00];
        tx
    };
    let coinbase = MiningRpc::template_coinbase(1);
    let coinbase_weight = transaction_weight_from_stacks(&coinbase, None).unwrap();

    let (_plain_dir, _plain_storage, plain_pool, plain_mining) = regtest_mining();
    let plain_tx = tx_of();
    assert!(plain_pool.add_transaction(plain_tx.clone()).unwrap());
    let plain_info = plain_mining.get_mining_info().await.unwrap();
    let plain_reported = plain_info["currentblockweight"].as_u64().unwrap();
    let plain_real = coinbase_weight + transaction_weight_from_stacks(&plain_tx, None).unwrap();

    let (_wit_dir, _wit_storage, wit_pool, wit_mining) = regtest_mining();
    let wit_tx = tx_of();
    let witnesses = vec![vec![vec![0u8; 400]]];
    assert!(
        wit_pool
            .add_transaction_with_witness(wit_tx.clone(), Some(witnesses.clone()))
            .unwrap()
    );
    let wit_info = wit_mining.get_mining_info().await.unwrap();
    let wit_reported = wit_info["currentblockweight"].as_u64().unwrap();
    let wit_real = coinbase_weight
        + transaction_weight_from_stacks(&wit_tx, Some(witnesses.as_slice())).unwrap();
    eprintln!(
        "plain reported={plain_reported} real={plain_real}; witness reported={wit_reported} real={wit_real}"
    );
    assert_eq!(plain_reported, plain_real);
    assert_eq!(wit_reported, wit_real);
}

#[tokio::test]
async fn a_gbt_coinbase_carries_the_reserved_witness() {
    use blvm_protocol::segwit::validate_witness_commitment;
    use blvm_protocol::serialization::transaction::deserialize_transaction_with_witness;

    let (_dir, _storage, _mempool, mining) = regtest_mining();
    let template = mining
        .get_block_template(&serde_json::json!([{
            "capabilities": ["coinbasetxn"]
        }]))
        .await
        .unwrap();
    let hex_data = template["coinbasetxn"]["data"].as_str().unwrap();
    let bytes = hex::decode(hex_data).unwrap();
    let (tx, witnesses, _) = deserialize_transaction_with_witness(&bytes).unwrap();
    let root = [0u8; 32];
    let published_ok = validate_witness_commitment(&tx, &root, &witnesses).unwrap();
    let empty_ok = validate_witness_commitment(&tx, &root, &[]).unwrap();
    eprintln!("published_ok={published_ok}; empty_ok={empty_ok}");
    assert!(published_ok);
    assert!(!empty_ok);
}

#[tokio::test]
async fn a_gbt_coinbase_script_starts_with_the_height() {
    use blvm_protocol::ValidationResult;
    use blvm_protocol::bip_validation::encode_bip34_coinbase_script;
    use blvm_protocol::serialization::transaction::deserialize_transaction_with_witness;
    use blvm_protocol::transaction::check_transaction;

    let (_dir, _storage, _mempool, mining) = regtest_mining();
    let template = mining
        .get_block_template(&serde_json::json!([{
            "capabilities": ["coinbasetxn"]
        }]))
        .await
        .unwrap();
    let height = template["height"].as_u64().unwrap();
    let bytes = hex::decode(template["coinbasetxn"]["data"].as_str().unwrap()).unwrap();
    let (mut tx, _, _) = deserialize_transaction_with_witness(&bytes).unwrap();
    assert_eq!(
        tx.inputs[0].script_sig,
        encode_bip34_coinbase_script(height)
    );
    let published_ok = matches!(check_transaction(&tx).unwrap(), ValidationResult::Valid);
    tx.inputs[0].script_sig.clear();
    let cleared_ok = matches!(check_transaction(&tx).unwrap(), ValidationResult::Valid);
    eprintln!("published_ok={published_ok}; cleared_ok={cleared_ok}");
    assert!(published_ok);
    assert!(!cleared_ok);
}

#[tokio::test]
async fn a_child_sorted_ahead_of_its_parent_is_still_in_the_template() {
    use crate::storage::hashing::hash_to_rpc_hex;
    use blvm_protocol::block::calculate_tx_id;

    let redeem = vec![0x51u8];
    let hash = crate::storage::hashing::hash160(&redeem);
    let mut spend_script = vec![0xa9, 0x14];
    spend_script.extend(hash);
    spend_script.push(0x87);
    let origin = OutPoint {
        hash: [4u8; 32],
        index: 0,
    };
    let run = |child_value: i64| {
        let spend_script = spend_script.clone();
        async move {
            let (_dir, storage, mempool, mining) = regtest_mining();
            storage
                .utxos()
                .add_utxo(
                    &origin,
                    &UTXO {
                        value: 200_000,
                        script_pubkey: spend_script.clone().into(),
                        height: 0,
                        is_coinbase: false,
                    },
                )
                .unwrap();
            mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
            mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
                let mut set = UtxoSet::default();
                set.insert(
                    origin,
                    Arc::new(UTXO {
                        value: 200_000,
                        script_pubkey: spend_script.clone().into(),
                        height: 0,
                        is_coinbase: false,
                    }),
                );
                set
            })));
            let parent = Transaction {
                version: 1,
                inputs: vec![TransactionInput {
                    prevout: origin,
                    script_sig: vec![0x01, 0x51],
                    sequence: 0xffffffff,
                }]
                .into(),
                outputs: vec![TransactionOutput {
                    value: 190_000,
                    script_pubkey: spend_script.clone(),
                }]
                .into(),
                lock_time: 0,
            };
            let parent_id = calculate_tx_id(&parent);
            let child = Transaction {
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
                    value: child_value,
                    script_pubkey: vec![0x6a, 0x03, 0x00, 0x00, 0x00],
                }]
                .into(),
                lock_time: 0,
            };
            assert!(mempool.add_transaction(parent).unwrap());
            assert!(mempool.add_transaction(child.clone()).unwrap());
            let template = mining
                .get_block_template(&serde_json::json!([]))
                .await
                .unwrap();
            let ids: Vec<String> = template["transactions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tx| tx["txid"].as_str().unwrap().to_string())
                .collect();
            let child_id = hash_to_rpc_hex(&calculate_tx_id(&child));
            (ids.len(), ids.iter().any(|id| id == &child_id))
        }
    };
    let (low_fee_n, low_fee_child) = run(188_000).await;
    let (high_fee_n, high_fee_child) = run(100_000).await;
    eprintln!(
        "child fee 2000 n={low_fee_n} child={low_fee_child}; child fee 90000 n={high_fee_n} child={high_fee_child}"
    );
    assert!(low_fee_child);
    assert!(high_fee_child);
    assert_eq!(low_fee_n, 2);
    assert_eq!(high_fee_n, 2);
}

#[tokio::test]
async fn a_template_publishes_the_witness_commitment() {
    use crate::storage::hashing::hash_to_rpc_hex;
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::segwit::{
        compute_witness_merkle_root_from_nested, witness_commitment_script,
    };
    use blvm_protocol::serialization::transaction::deserialize_transaction_with_witness;
    use blvm_protocol::{Block, BlockHeader};
    use sha2::{Digest, Sha256};

    let script = vec![0x51u8];
    let program: [u8; 32] = Sha256::digest(&script).into();
    let mut spend_script = vec![0x00, 0x20];
    spend_script.extend(program);
    let origin = OutPoint {
        hash: [8u8; 32],
        index: 0,
    };
    let (_dir, storage, mempool, mining) = regtest_mining();
    storage
        .utxos()
        .add_utxo(
            &origin,
            &UTXO {
                value: 100_000,
                script_pubkey: spend_script.clone().into(),
                height: 0,
                is_coinbase: false,
            },
        )
        .unwrap();
    mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new({
        let mut set = UtxoSet::default();
        set.insert(
            origin,
            Arc::new(UTXO {
                value: 100_000,
                script_pubkey: spend_script.into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        set
    })));
    let tx = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: origin,
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
    let stored_witness = vec![vec![script]];
    assert!(
        mempool
            .add_transaction_with_witness(tx.clone(), Some(stored_witness.clone()))
            .unwrap()
    );
    let template = mining
        .get_block_template(&serde_json::json!([]))
        .await
        .unwrap();
    let published = template["default_witness_commitment"]
        .as_str()
        .expect("default witness commitment");
    let with_coinbase = mining
        .get_block_template(&serde_json::json!([{
            "capabilities": ["coinbasetxn"]
        }]))
        .await
        .unwrap();
    let (coinbase, _, _) = deserialize_transaction_with_witness(
        &hex::decode(with_coinbase["coinbasetxn"]["data"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let txid = hash_to_rpc_hex(&calculate_tx_id(&tx));
    let entry = with_coinbase["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["txid"].as_str() == Some(txid.as_str()))
        .expect("spend is in the template");
    let (published_tx, _, _) = deserialize_transaction_with_witness(
        &hex::decode(entry["data"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 0,
        bits: 0,
        nonce: 0,
    };
    let matches = |witness: &[blvm_protocol::segwit::Witness]| {
        let block = Block {
            header: header.clone(),
            transactions: vec![coinbase.clone(), published_tx.clone()].into_boxed_slice(),
        };
        let nested = vec![vec![vec![vec![0u8; 32]]], witness.to_vec()];
        let root = compute_witness_merkle_root_from_nested(&block, &nested, None).unwrap();
        hex::encode(witness_commitment_script(&root, &[0u8; 32])) == published
    };
    let stored_ok = matches(&stored_witness);
    let empty_ok = matches(&[vec![]]);
    eprintln!("stored_ok={stored_ok}; empty_ok={empty_ok}");
    assert!(stored_ok);
    assert!(!empty_ok);
}

#[tokio::test]
async fn a_supplied_coinbase_script_still_starts_with_the_height() {
    use blvm_protocol::bip_validation::check_bip34_network;
    use blvm_protocol::serialization::transaction::deserialize_transaction_with_witness;
    use blvm_protocol::{Block, BlockHeader};

    let (_dir, _storage, _mempool, mining) = regtest_mining();
    let template = mining
        .get_block_template(&serde_json::json!([{
            "capabilities": ["coinbasetxn"],
            "coinbasetxn": { "data": "00ff" }
        }]))
        .await
        .unwrap();
    let height = template["height"].as_u64().unwrap();
    let (mut coinbase, _, _) = deserialize_transaction_with_witness(
        &hex::decode(template["coinbasetxn"]["data"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 0,
        bits: 0,
        nonce: 0,
    };
    let bip34 = |tx: &Transaction| {
        let block = Block {
            header: header.clone(),
            transactions: vec![tx.clone()].into_boxed_slice(),
        };
        check_bip34_network(&block, height, blvm_consensus::Network::Regtest).unwrap()
    };
    let supplied_ok = bip34(&coinbase);
    coinbase.inputs[0].script_sig = vec![0x00, 0xff];
    let restored_ok = bip34(&coinbase);
    eprintln!("supplied_ok={supplied_ok}; restored_ok={restored_ok}");
    assert!(supplied_ok);
    assert!(!restored_ok);
}

#[tokio::test]
async fn a_one_byte_height_prefix_is_a_valid_coinbase_script() {
    use blvm_protocol::ValidationResult;
    use blvm_protocol::serialization::transaction::deserialize_transaction_with_witness;
    use blvm_protocol::transaction::check_transaction;

    let (_dir, _storage, _mempool, mining) = regtest_mining();
    let template = mining
        .get_block_template(&serde_json::json!([{
            "capabilities": ["coinbasetxn"],
            "coinbasetxn": { "data": "51" }
        }]))
        .await
        .unwrap();
    let (mut coinbase, _, _) = deserialize_transaction_with_witness(
        &hex::decode(template["coinbasetxn"]["data"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let published_ok = matches!(
        check_transaction(&coinbase).unwrap(),
        ValidationResult::Valid
    );
    coinbase.inputs[0].script_sig = vec![0x51];
    let restored_ok = matches!(
        check_transaction(&coinbase).unwrap(),
        ValidationResult::Valid
    );
    eprintln!("published_ok={published_ok}; restored_ok={restored_ok}");
    assert!(published_ok);
    assert!(!restored_ok);
}
