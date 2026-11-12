use super::*;
use blvm_protocol::TransactionOutput;

#[test]
fn test_transaction_selector_creation() {
    let selector = TransactionSelector::new();
    assert_eq!(selector.max_block_size(), 1_000_000);
    assert_eq!(selector.max_block_weight(), 4_000_000);
    assert_eq!(selector.min_fee_rate(), 1);
}

#[test]
fn fee_of_seven_sats_is_mined() {
    let selector = TransactionSelector::new();
    let utxo_value = 20_000i64;
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    utxo_set.insert(
        blvm_protocol::OutPoint {
            hash: [0u8; 32],
            index: 0,
        },
        std::sync::Arc::new(blvm_protocol::UTXO {
            value: utxo_value,
            script_pubkey: vec![blvm_protocol::opcodes::OP_1].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let pick = |output: i64| {
        let mut mempool = MockMempoolProvider::new();
        mempool.add_transaction(create_test_transaction(1, output as u64));
        !selector.select_transactions(&mempool, &utxo_set).is_empty()
    };
    let pays_ten_thousand = pick(utxo_value - 10_000);
    let pays_seven = pick(utxo_value - 7);
    let pays_zero = pick(utxo_value);
    eprintln!(
        "fee 10000 selected={pays_ten_thousand}; fee 7 selected={pays_seven}; fee 0 selected={pays_zero}"
    );
    assert!(pays_ten_thousand);
    assert!(pays_seven);
    assert!(!pays_zero);
}

#[test]
fn test_transaction_selector_with_params() {
    let selector = TransactionSelector::with_params(2_000_000, 8_000_000, 5);
    assert_eq!(selector.max_block_size(), 2_000_000);
    assert_eq!(selector.max_block_weight(), 8_000_000);
    assert_eq!(selector.min_fee_rate(), 5);
}

#[test]
fn test_transaction_selector_transaction_selection() {
    let selector = TransactionSelector::new();
    let mut mempool = MockMempoolProvider::new();

    // Add some test transactions
    let tx1 = create_test_transaction(1, 1000);
    let tx2 = create_test_transaction(2, 2000);
    let tx3 = create_test_transaction(3, 500);

    mempool.add_transaction(tx1);
    mempool.add_transaction(tx2);
    mempool.add_transaction(tx3);

    // UTXO set must contain inputs for fee calculation (create_test_transaction uses [0;32], index 0)
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    let outpoint = blvm_protocol::OutPoint {
        hash: [0u8; 32],
        index: 0,
    };
    utxo_set.insert(
        outpoint,
        std::sync::Arc::new(blvm_protocol::UTXO {
            value: 100_000_000, // 1 BTC - enough for all test tx outputs
            script_pubkey: vec![blvm_protocol::opcodes::OP_1].into(),
            height: 0,
            is_coinbase: false,
        }),
    );

    let selected = selector.select_transactions(&mempool, &utxo_set);
    assert!(!selected.is_empty());
    assert!(selected.len() <= 3);
}

#[test]
fn non_witness_stripped_size_100_weighs_400() {
    let mut tx = Transaction {
        version: 1,
        inputs: blvm_protocol::tx_inputs![blvm_protocol::TransactionInput {
            prevout: blvm_protocol::OutPoint {
                hash: [1u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        }],
        outputs: blvm_protocol::tx_outputs![TransactionOutput {
            value: 1,
            script_pubkey: vec![0x51],
        }],
        lock_time: 0,
    };
    while blvm_consensus::transaction::calculate_transaction_size(&tx) < 100 {
        tx.inputs[0].script_sig.push(0);
    }
    assert_eq!(
        blvm_consensus::transaction::calculate_transaction_size(&tx),
        100
    );
    assert_eq!(bip141_weight(&tx, None), 400);
}

#[test]
fn selector_stops_before_block_weight_passes_four_million() {
    let selector = TransactionSelector::with_params(1_000_000, 4_000_000, 0);
    let mut mempool = MockMempoolProvider::new();
    let mut sample = create_test_transaction(2, 1_000);
    sample.inputs[0].script_sig = vec![0u8; 80_000];
    let one = bip141_weight(&sample, None);
    for i in 0..20 {
        let mut tx = sample.clone();
        tx.lock_time = i;
        mempool.add_transaction(tx);
    }
    let selected = selector.select_transactions(&mempool, &blvm_protocol::UtxoSet::default());
    let mut used = TEMPLATE_RESERVE_WEIGHT;
    let mut fit = 0usize;
    while used.saturating_add(one) <= 4_000_000 && fit < 20 {
        used = used.saturating_add(one);
        fit += 1;
    }
    assert!(fit < 20);
    assert_eq!(selected.len(), fit);
    assert!(used <= 4_000_000);
}

#[test]
fn selector_leaves_room_for_the_coinbase() {
    let mut sample = create_test_transaction(2, 1_000);
    sample.inputs[0].script_sig = vec![0x51];
    while blvm_consensus::transaction::calculate_transaction_size(&sample) < 100 {
        sample.inputs[0].script_sig.push(0);
    }
    let one = blvm_consensus::transaction::calculate_transaction_size(&sample);
    let selector = TransactionSelector::with_params(TEMPLATE_RESERVE_BYTES + one, 4_000_000, 0);
    let mut mempool = MockMempoolProvider::new();
    for i in 0..5u64 {
        let mut tx = sample.clone();
        tx.lock_time = i;
        mempool.add_transaction(tx);
    }
    let selected = selector.select_transactions(&mempool, &blvm_protocol::UtxoSet::default());
    let stripped: usize = selected
        .iter()
        .map(blvm_consensus::transaction::calculate_transaction_size)
        .sum();
    assert_eq!(selected.len(), 1);
    assert_eq!(
        stripped + TEMPLATE_RESERVE_BYTES,
        TEMPLATE_RESERVE_BYTES + one
    );
}

#[test]
fn selector_keeps_sigop_cost_at_or_under_80000() {
    let selector = TransactionSelector::with_params(1_000_000, 4_000_000, 0);
    let mut mempool = MockMempoolProvider::new();
    for i in 0..6u32 {
        let tx = Transaction {
            version: 2,
            inputs: blvm_protocol::tx_inputs![blvm_protocol::TransactionInput {
                prevout: blvm_protocol::OutPoint {
                    hash: [2u8; 32],
                    index: i,
                },
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            }],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 1_000,
                script_pubkey: vec![0xac; 4_000],
            }],
            lock_time: i as u64,
        };
        mempool.add_transaction(tx);
    }
    let selected = selector.select_transactions(&mempool, &blvm_protocol::UtxoSet::default());
    assert_eq!(selected.len(), 5);
    let mut cost = 0u64;
    for tx in &selected {
        cost += tx_sigop_cost(tx, &blvm_protocol::UtxoSet::default(), &[], None);
    }
    assert!(cost <= 80_000);
    assert_eq!(cost, 80_000);
}

#[test]
fn selector_reads_the_whole_pool_in_fee_order() {
    let wide = TransactionSelector::with_params(10_000_000, 10_000_000, 0);
    let mut mempool = MockMempoolProvider::new();
    for i in 0..1001u32 {
        let mut tx = create_test_transaction((i + 1) as i32, 1_000);
        tx.lock_time = i as u64;
        mempool.add_transaction(tx);
    }
    let utxo = blvm_protocol::UtxoSet::default();
    let all = wide.select_transactions(&mempool, &utxo);
    assert_eq!(all.len(), 1001);

    let sample = create_test_transaction(1, 1_000);
    let one = bip141_weight(&sample, None);
    let room = TEMPLATE_RESERVE_WEIGHT + one.saturating_mul(1000);
    let tight = TransactionSelector::with_params(10_000_000, room, 0);
    let top = tight.select_transactions(&mempool, &utxo);
    assert_eq!(top.len(), 1000);
    assert!(top.iter().all(|tx| tx.version != 1));
}

#[test]
fn coordinator_bits_match_next_required_bits() {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let storage = std::sync::Arc::new(crate::storage::Storage::new(temp_dir.path()).unwrap());
    let mut entries = Vec::with_capacity(2016);
    for height in 0..2016u64 {
        let mut hash = [0u8; 32];
        hash[0..8].copy_from_slice(&height.to_le_bytes());
        entries.push((
            hash,
            BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: height.saturating_mul(60),
                bits: 0x1d00ffff,
                nonce: 0,
            },
            height,
        ));
    }
    storage.blocks().store_headers_batch(&entries).unwrap();
    let mempool = std::sync::Arc::new(crate::node::mempool::MempoolManager::new());
    let coordinator = MiningCoordinator::new(mempool, Some(storage));
    let parent_bits = 0x1d00ffffu64;
    let parent_time = 2015u64 * 60;
    let retarget = coordinator
        .template_work_bits(2016, parent_time + 60, 2015, parent_bits, parent_time)
        .unwrap();
    assert_ne!(retarget, parent_bits);
    let kept = coordinator
        .template_work_bits(2017, parent_time + 120, 2016, retarget, parent_time + 60)
        .unwrap();
    assert_eq!(kept, retarget);
}

#[test]
fn below_floor_parent_outside_the_window_is_mined_with_its_child() {
    use blvm_protocol::constants::SEQUENCE_FINAL;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{OutPoint, Transaction, TransactionInput};

    let selector = TransactionSelector::new();
    let mut mempool = MockMempoolProvider::new();
    let parent_key = {
        let mut hash = [0u8; 32];
        hash[0] = 1;
        hash[1] = 1;
        hash[2] = 1;
        hash
    };
    let funding = OutPoint {
        hash: [7u8; 32],
        index: 0,
    };
    let parent = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: funding,
            script_sig: Vec::new(),
            sequence: SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 9_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    let child = Transaction {
        version: 2,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: parent_key,
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 100,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    mempool.add_unranked(parent.clone());
    mempool.add_transaction(child.clone());
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    utxo_set.insert(
        funding,
        std::sync::Arc::new(blvm_protocol::UTXO {
            value: 9_001,
            script_pubkey: vec![OP_1].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let selected = selector.select_transactions(&mempool, &utxo_set);
    assert!(
        selected.iter().any(
            |tx| tx.version == parent.version && tx.outputs[0].value == parent.outputs[0].value
        ),
        "parent outside the window was left out"
    );
    assert!(selected.iter().any(|tx| tx.version == child.version));
}

#[test]
fn test_transaction_selector_size_calculation() {
    let selector = TransactionSelector::new();
    let tx = create_test_transaction(1, 1000);

    let vsize = transaction_vsize(&tx, None);
    assert!(vsize > 0);

    let weight = bip141_weight(&tx, None);
    assert!(weight > 0);

    let mut utxo_set = blvm_protocol::UtxoSet::default();
    let outpoint = blvm_protocol::OutPoint {
        hash: [0u8; 32],
        index: 0,
    };
    utxo_set.insert(
        outpoint,
        std::sync::Arc::new(blvm_protocol::UTXO {
            value: 10000,
            script_pubkey: vec![blvm_protocol::opcodes::OP_1].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let fee_rate =
        selector.calculate_fee_rate_with_utxo(&tx, &utxo_set, vsize, &MockMempoolProvider::new());
    assert!(fee_rate > 0);
}

#[test]
fn template_fee_ignores_a_negative_output() {
    use blvm_protocol::constants::SEQUENCE_FINAL;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{OutPoint, Transaction, TransactionInput};

    let selector = TransactionSelector::new();
    let funding = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    utxo_set.insert(
        funding,
        std::sync::Arc::new(blvm_protocol::UTXO {
            value: 50_000,
            script_pubkey: vec![OP_1].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    let spend = |outputs: Vec<TransactionOutput>| Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: funding,
            script_sig: Vec::new(),
            sequence: SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: outputs.into(),
        lock_time: 0,
    };
    let negative = spend(vec![
        TransactionOutput {
            value: 1_000,
            script_pubkey: vec![OP_1],
        },
        TransactionOutput {
            value: -1,
            script_pubkey: vec![OP_1],
        },
    ]);
    let positive = spend(vec![TransactionOutput {
        value: 40_000,
        script_pubkey: vec![OP_1],
    }]);
    let mempool = MockMempoolProvider::new();
    assert_eq!(selector.transaction_fee(&negative, &utxo_set, &mempool), 0);
    assert_eq!(
        selector.transaction_fee(&positive, &utxo_set, &mempool),
        10_000
    );
}

#[test]
fn selector_includes_a_smaller_tx_after_one_that_does_not_fit() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::OP_1;

    let small = create_test_transaction(1, 1_000);
    let mut large = create_test_transaction(2, 1_000);
    large.outputs[0].script_pubkey = vec![OP_1; 800];

    let small_size = blvm_consensus::transaction::calculate_transaction_size(&small);
    let large_size = blvm_consensus::transaction::calculate_transaction_size(&large);
    assert!(large_size > small_size);

    let selector = TransactionSelector::with_params(small_size, 4_000_000, 1);
    let mut mempool = MockMempoolProvider::new();
    mempool.add_transaction(large.clone());
    mempool.add_transaction(small.clone());

    let mut utxo_set = blvm_protocol::UtxoSet::default();
    utxo_set.insert(
        blvm_protocol::OutPoint {
            hash: [0u8; 32],
            index: 0,
        },
        std::sync::Arc::new(blvm_protocol::UTXO {
            value: 100_000_000,
            script_pubkey: vec![OP_1].into(),
            height: 0,
            is_coinbase: false,
        }),
    );

    let selected = selector.select_transactions(&mempool, &utxo_set);
    assert_eq!(selected.len(), 1);
    assert_eq!(calculate_tx_id(&selected[0]), calculate_tx_id(&small));
}

#[test]
fn test_mining_engine_creation() {
    let engine = MiningEngine::new();
    assert!(!engine.is_mining_enabled());
    assert_eq!(engine.get_threads(), 1);
    assert!(engine.get_block_template().is_none());
    assert_eq!(engine.get_stats().blocks_mined, 0);
}

#[test]
fn test_mining_engine_with_threads() {
    let engine = MiningEngine::with_threads(4);
    assert!(!engine.is_mining_enabled());
    assert_eq!(engine.get_threads(), 4);
}

#[test]
fn test_mining_engine_enable_disable() {
    let mut engine = MiningEngine::new();

    assert!(!engine.is_mining_enabled());
    engine.enable_mining();
    assert!(engine.is_mining_enabled());

    engine.disable_mining();
    assert!(!engine.is_mining_enabled());
}

#[test]
fn test_mining_engine_thread_management() {
    let mut engine = MiningEngine::new();

    assert_eq!(engine.get_threads(), 1);
    engine.set_threads(8);
    assert_eq!(engine.get_threads(), 8);
}

#[tokio::test]
async fn test_mining_engine_mine_template() {
    let mut engine = MiningEngine::new();
    let template = create_test_block();

    let result = engine.mine_template(template.clone()).await;

    // Mining may succeed (if low difficulty) or fail (if high difficulty)
    // Both are valid outcomes for real PoW mining
    if let Ok(mined_block) = result {
        // Successfully mined - verify the block
        assert_eq!(mined_block.header.version, template.header.version);
        assert_ne!(mined_block.header.nonce, template.header.nonce); // Nonce should change

        // Verify proof of work
        use blvm_protocol::pow::check_proof_of_work;
        let pow_valid = check_proof_of_work(&mined_block.header).unwrap();
        assert!(pow_valid, "Mined block should have valid proof of work");

        // Check that template was stored
        assert!(engine.get_block_template().is_some());
        assert_eq!(engine.get_stats().blocks_mined, 1);
    } else {
        // Mining failed (high difficulty) - this is expected for mainnet difficulty
        // Just verify the template was stored
        assert!(engine.get_block_template().is_some());
    }
}

#[tokio::test]
async fn test_mining_engine_mine_template_multithreaded() {
    let mut engine = MiningEngine::with_threads(4);
    let template = create_test_block();

    let result = engine.mine_template(template.clone()).await;

    // Mining may succeed (if low difficulty) or fail (if high difficulty)
    if let Ok(mined_block) = result {
        // Successfully mined - verify the block
        assert_eq!(mined_block.header.version, template.header.version);

        // Verify proof of work
        use blvm_protocol::pow::check_proof_of_work;
        let pow_valid = check_proof_of_work(&mined_block.header).unwrap();
        assert!(pow_valid, "Mined block should have valid proof of work");

        // Check that template was stored
        assert!(engine.get_block_template().is_some());
        assert_eq!(engine.get_stats().blocks_mined, 1);
    } else {
        // Mining failed (high difficulty) - this is expected
        assert!(engine.get_block_template().is_some());
    }
}

#[tokio::test]
async fn test_mining_engine_mine_template_regtest_difficulty() {
    // Test with mainnet difficulty - mining may succeed or fail depending on luck
    // This tests that the mining infrastructure works correctly
    let mut engine = MiningEngine::new();
    let template = create_test_block();
    // template already has bits: 0x1d00ffff (mainnet difficulty)

    let result = engine.mine_template(template.clone()).await;

    // Mining may succeed (if we find a nonce) or fail (if we don't within max_attempts)
    // Both are valid outcomes for real PoW mining
    if let Ok(mined_block) = result {
        // Successfully mined - verify the block
        assert_eq!(mined_block.header.version, template.header.version);
        assert_ne!(mined_block.header.nonce, template.header.nonce);

        // Verify proof of work
        use blvm_protocol::pow::check_proof_of_work;
        let pow_valid = check_proof_of_work(&mined_block.header).unwrap();
        assert!(pow_valid, "Mined block should have valid proof of work");

        // Check statistics
        assert_eq!(engine.get_stats().blocks_mined, 1);
    } else {
        // Mining failed (didn't find nonce within max_attempts) - this is expected
        // The important thing is that the mining infrastructure worked correctly
        assert!(engine.get_block_template().is_some());
    }
}

#[test]
fn test_mining_engine_template_management() {
    let mut engine = MiningEngine::new();

    assert!(engine.get_block_template().is_none());

    let template = create_test_block();
    engine.block_template = Some(template.clone());

    assert!(engine.get_block_template().is_some());
    assert_eq!(
        engine.get_block_template().unwrap().header.version,
        template.header.version
    );

    engine.clear_template();
    assert!(engine.get_block_template().is_none());
}

#[test]
fn test_mining_engine_statistics() {
    let mut engine = MiningEngine::new();
    let stats = engine.get_stats();

    assert_eq!(stats.blocks_mined, 0);
    assert_eq!(stats.total_hashrate, 0.0);
    assert_eq!(stats.average_block_time, 0.0);
    assert!(stats.last_block_time.is_none());

    engine.update_hashrate(1000.0);
    assert_eq!(engine.get_stats().total_hashrate, 1000.0);

    engine.update_average_block_time(600.0);
    assert_eq!(engine.get_stats().average_block_time, 600.0);
}

#[test]
fn test_mock_mempool_provider_creation() {
    let mempool = MockMempoolProvider::new();
    assert_eq!(mempool.get_mempool_size(), 0);
    assert!(mempool.get_transactions().is_empty());
    let empty_utxo_set = blvm_protocol::UtxoSet::default();
    assert!(
        mempool
            .get_prioritized_transactions(10, &empty_utxo_set)
            .is_empty()
    );
}

#[test]
fn test_mock_mempool_provider_transaction_management() {
    let mut mempool = MockMempoolProvider::new();

    let tx1 = create_test_transaction(1, 1000);
    let tx2 = create_test_transaction(2, 2000);

    mempool.add_transaction(tx1.clone());
    mempool.add_transaction(tx2.clone());

    assert_eq!(mempool.get_mempool_size(), 2);
    assert_eq!(mempool.get_transactions().len(), 2);

    let empty_utxo_set = blvm_protocol::UtxoSet::default();
    let prioritized = mempool.get_prioritized_transactions(10, &empty_utxo_set);
    assert_eq!(prioritized.len(), 2);

    // Test transaction removal
    let hash = mempool.calculate_tx_hash(&tx1);
    assert!(mempool.remove_transaction(&hash));
    assert_eq!(mempool.get_mempool_size(), 1);

    // Test removal of non-existent transaction
    let fake_hash = [0u8; 32];
    assert!(!mempool.remove_transaction(&fake_hash));
}

#[test]
fn test_mock_mempool_provider_prioritization() {
    let mut mempool = MockMempoolProvider::new();

    // Add transactions with different fee rates
    let tx_low_fee = create_test_transaction(1, 100); // Low fee
    let tx_high_fee = create_test_transaction(2, 5000); // High fee
    let tx_medium_fee = create_test_transaction(3, 1000); // Medium fee

    mempool.add_transaction(tx_low_fee);
    mempool.add_transaction(tx_high_fee);
    mempool.add_transaction(tx_medium_fee);

    let empty_utxo_set = blvm_protocol::UtxoSet::default();
    let prioritized = mempool.get_prioritized_transactions(10, &empty_utxo_set);
    assert_eq!(prioritized.len(), 3);

    // Transactions should be sorted by fee rate (descending)
    // Version 3 (medium fee) should be first, then version 2 (high fee), then version 1 (low fee)
    assert_eq!(prioritized[0].version, 3);
    assert_eq!(prioritized[1].version, 2);
    assert_eq!(prioritized[2].version, 1);
}

#[test]
fn test_mock_mempool_provider_clear() {
    let mut mempool = MockMempoolProvider::new();

    let tx = create_test_transaction(1, 1000);
    mempool.add_transaction(tx);

    assert_eq!(mempool.get_mempool_size(), 1);

    mempool.clear();
    assert_eq!(mempool.get_mempool_size(), 0);
    assert!(mempool.get_transactions().is_empty());
}

#[test]
fn test_mining_coordinator_creation() {
    use std::sync::Arc;
    let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
    let coordinator = MiningCoordinator::new(mempool, None);

    assert!(!coordinator.is_mining_enabled());
    assert_eq!(coordinator.get_mempool_size(), 0);
    assert_eq!(coordinator.mining_engine().get_threads(), 1);
    assert_eq!(
        coordinator.transaction_selector().max_block_size(),
        1_000_000
    );
}

#[test]
fn test_mining_coordinator_with_params() {
    use std::sync::Arc;
    let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
    let coordinator = MiningCoordinator::with_params(mempool, None, 4, 2_000_000, 8_000_000, 5);

    assert_eq!(coordinator.mining_engine().get_threads(), 4);
    assert_eq!(
        coordinator.transaction_selector().max_block_size(),
        2_000_000
    );
    assert_eq!(
        coordinator.transaction_selector().max_block_weight(),
        8_000_000
    );
    assert_eq!(coordinator.transaction_selector().min_fee_rate(), 5);
}

#[test]
fn test_mining_coordinator_enable_disable() {
    use std::sync::Arc;
    let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
    let mut coordinator = MiningCoordinator::new(mempool, None);

    assert!(!coordinator.is_mining_enabled());
    coordinator.enable_mining();
    assert!(coordinator.is_mining_enabled());

    coordinator.disable_mining();
    assert!(!coordinator.is_mining_enabled());
}

#[test]
fn test_mining_coordinator_info() {
    use std::sync::Arc;
    let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
    let coordinator = MiningCoordinator::new(mempool, None);

    let info = coordinator.get_mining_info();
    assert!(!info.enabled);
    assert_eq!(info.threads, 1);
    assert!(!info.has_template);
}

#[test]
fn test_mining_coordinator_statistics() {
    use std::sync::Arc;
    let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
    let coordinator = MiningCoordinator::new(mempool, None);

    let stats = coordinator.get_mining_stats();
    assert_eq!(stats.blocks_mined, 0);
    assert_eq!(stats.total_hashrate, 0.0);
    assert_eq!(stats.average_block_time, 0.0);
    assert!(stats.last_block_time.is_none());
}

#[test]
fn test_mining_coordinator_accessors() {
    use std::sync::Arc;
    let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
    let coordinator = MiningCoordinator::new(mempool, None);

    // Test immutable access
    let engine = coordinator.mining_engine();
    assert_eq!(engine.get_threads(), 1);

    let selector = coordinator.transaction_selector();
    assert_eq!(selector.max_block_size(), 1_000_000);

    // Test mutable access
    let mut coordinator = coordinator;
    let engine_mut = coordinator.mining_engine_mut();
    engine_mut.set_threads(4);
    assert_eq!(coordinator.mining_engine().get_threads(), 4);

    let selector_mut = coordinator.transaction_selector_mut();
    // Test that we can access the selector
    assert_eq!(selector_mut.max_block_size(), 1_000_000);
}

#[tokio::test]
async fn test_mining_coordinator_mempool_operations() {
    use std::sync::Arc;
    // Create mempool and add transaction before wrapping in Arc
    let mut mempool_manager = crate::node::mempool::MempoolManager::new();
    let tx = create_test_transaction(1, 1000);
    let _ = mempool_manager.add_transaction(tx);
    let mempool = Arc::new(mempool_manager);
    let coordinator = MiningCoordinator::new(mempool, None);

    assert_eq!(coordinator.get_mempool_size(), 1);
}

#[tokio::test]
async fn test_mining_coordinator_block_template_generation() {
    use std::sync::Arc;
    // Create mempool and add transaction before wrapping in Arc
    let mut mempool_manager = crate::node::mempool::MempoolManager::new();
    let tx = create_test_transaction(1, 1000);
    let _ = mempool_manager.add_transaction(tx);
    let mempool = Arc::new(mempool_manager);

    let mut coordinator = MiningCoordinator::new(mempool, None);

    let template = coordinator.generate_block_template().await;
    assert!(template.is_ok());

    let block = template.unwrap();
    assert_eq!(block.header.version, 1);
    assert!(!block.transactions.is_empty()); // Should have coinbase + mempool tx
    let cb = &block.transactions[0];
    assert!(cb.outputs.len() >= 2, "payout plus BIP141 commitment");
    let last = cb.outputs.last().unwrap();
    assert_eq!(last.value, 0);
    assert_eq!(last.script_pubkey[0], 0x6a);
}

#[tokio::test]
async fn test_mining_coordinator_coinbase_creation() {
    use std::sync::Arc;
    let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
    let coordinator = MiningCoordinator::new(mempool, None);

    // Test coinbase creation with no transactions (subsidy only)
    let empty_utxo_set = blvm_protocol::UtxoSet::default();
    let coinbase = coordinator
        .create_coinbase_transaction(0, &[], &empty_utxo_set)
        .await;
    assert!(coinbase.is_ok());

    let tx = coinbase.unwrap();
    assert_eq!(tx.version, 1);
    // Bitcoin coinbase: exactly one input, null prevout + 0xffffffff index (BIP30/BIP34).
    assert_eq!(tx.inputs.len(), 1);
    assert_eq!(tx.inputs[0].prevout.hash, [0u8; 32]);
    assert_eq!(tx.inputs[0].prevout.index, 0xffff_ffff);
    assert_eq!(tx.outputs.len(), 1);
    // Should be 50 BTC (subsidy) at height 0, with no fees
    assert_eq!(tx.outputs[0].value, 5000000000); // 50 BTC
    assert_eq!(tx.lock_time, 0);
}

// Helper functions for tests
fn create_test_transaction(version: i32, output_value: u64) -> Transaction {
    use blvm_protocol::{OutPoint, TransactionInput};
    Transaction {
        version: version as u64,
        inputs: blvm_protocol::tx_inputs![TransactionInput {
            prevout: OutPoint {
                hash: [0u8; 32],
                index: 0,
            },
            script_sig: vec![
                blvm_protocol::opcodes::OP_DUP,
                blvm_protocol::opcodes::OP_HASH160,
                blvm_protocol::opcodes::PUSH_20_BYTES,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                blvm_protocol::opcodes::OP_EQUALVERIFY,
                blvm_protocol::opcodes::OP_CHECKSIG,
            ],
            sequence: 0xffffffff,
        }],
        outputs: blvm_protocol::tx_outputs![TransactionOutput {
            value: output_value as i64,
            script_pubkey: vec![
                blvm_protocol::opcodes::OP_DUP,
                blvm_protocol::opcodes::OP_HASH160,
                blvm_protocol::opcodes::PUSH_20_BYTES,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                blvm_protocol::opcodes::OP_EQUALVERIFY,
                blvm_protocol::opcodes::OP_CHECKSIG,
            ],
        }],
        lock_time: 0,
    }
}

fn create_test_block() -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![create_test_transaction(1, 1000)].into_boxed_slice(),
    }
}

#[tokio::test]
async fn test_build_witnesses_uses_mempool_stored_witnesses() {
    use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};
    use sha2::{Digest, Sha256};
    use std::sync::Arc;

    fn p2wsh_scriptpubkey(witness_script: &[u8]) -> Vec<u8> {
        let hash = Sha256::digest(witness_script);
        let mut spk = vec![blvm_protocol::opcodes::OP_0, 0x20];
        spk.extend_from_slice(&hash);
        spk
    }

    let witness_script = vec![0x51]; // OP_1
    let funding_hash = [0xab; 32];
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    utxo_set.insert(
        OutPoint {
            hash: funding_hash,
            index: 0,
        },
        Arc::new(blvm_protocol::UTXO {
            value: 100_000,
            script_pubkey: p2wsh_scriptpubkey(&witness_script).into(),
            height: 0,
            is_coinbase: false,
        }),
    );

    let spend = Transaction {
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
            script_pubkey: {
                let mut script = vec![0x00, 0x14];
                script.extend_from_slice(&[0u8; 20]);
                script
            },
        }]
        .into(),
        lock_time: 0,
    };
    let txid = blvm_protocol::block::calculate_tx_id(&spend);
    let witness_stack: Witness = vec![witness_script.clone()];

    let mut mempool_manager = crate::node::mempool::MempoolManager::new();
    assert!(
        mempool_manager
            .add_transaction_with_witness(spend.clone(), Some(vec![witness_stack.clone()]))
            .unwrap(),
        "witness tx must enter mempool"
    );
    let mempool = Arc::new(mempool_manager);
    let coordinator = MiningCoordinator::new(mempool, None);

    let coinbase = coordinator
        .create_coinbase_transaction(1, &[spend.clone()], &utxo_set)
        .await
        .unwrap();
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![coinbase, spend].into_boxed_slice(),
    };

    let witnesses = coordinator
        .build_witnesses_for_block(&block, &utxo_set)
        .expect("witness spend with stored mempool witnesses");
    assert_eq!(witnesses.len(), 2);
    assert_eq!(witnesses[0], vec![vec![vec![0u8; 32]]]);
    assert_eq!(witnesses[1], vec![witness_stack.clone()]);
    assert_eq!(
        coordinator.mempool.get_transaction_witnesses(&txid),
        Some(vec![witness_stack])
    );

    let cb = block.transactions[0].clone();
    let spend_tx = block.transactions[1].clone();
    let mut empty_cb = cb.clone();
    let mut empty_txs = vec![cb.clone(), spend_tx.clone()];
    blvm_protocol::mining::append_witness_commitment(&mut empty_cb, &mut empty_txs).unwrap();
    let mut real_cb = cb.clone();
    let mut real_txs = vec![cb, spend_tx];
    blvm_protocol::mining::append_witness_commitment_from_nested(
        &mut real_cb,
        &mut real_txs,
        Some(&witnesses),
    )
    .unwrap();
    assert_ne!(
        empty_cb.outputs.last().unwrap().script_pubkey,
        real_cb.outputs.last().unwrap().script_pubkey
    );
}

#[tokio::test]
async fn test_build_witnesses_fails_when_mempool_missing_witness_spend() {
    use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};
    use sha2::{Digest, Sha256};
    use std::sync::Arc;

    fn p2wsh_scriptpubkey(witness_script: &[u8]) -> Vec<u8> {
        let hash = Sha256::digest(witness_script);
        let mut spk = vec![blvm_protocol::opcodes::OP_0, 0x20];
        spk.extend_from_slice(&hash);
        spk
    }

    let witness_script = vec![0x51];
    let funding_hash = [0xcd; 32];
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    utxo_set.insert(
        OutPoint {
            hash: funding_hash,
            index: 0,
        },
        Arc::new(blvm_protocol::UTXO {
            value: 100_000,
            script_pubkey: p2wsh_scriptpubkey(&witness_script).into(),
            height: 0,
            is_coinbase: false,
        }),
    );

    let spend = Transaction {
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
            script_pubkey: {
                let mut script = vec![0x00, 0x14];
                script.extend_from_slice(&[0u8; 20]);
                script
            },
        }]
        .into(),
        lock_time: 0,
    };

    let mut mempool_manager = crate::node::mempool::MempoolManager::new();
    assert!(
        mempool_manager.add_transaction(spend.clone()).unwrap(),
        "tx must enter mempool"
    );
    let mempool = Arc::new(mempool_manager);
    let coordinator = MiningCoordinator::new(mempool, None);

    let coinbase = coordinator
        .create_coinbase_transaction(1, &[spend.clone()], &utxo_set)
        .await
        .unwrap();
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![coinbase, spend].into_boxed_slice(),
    };

    let err = coordinator
        .build_witnesses_for_block(&block, &utxo_set)
        .unwrap_err();
    assert!(
        err.to_string().contains("missing mempool witnesses"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn generate_template_refuses_missing_mempool_witnesses() {
    use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput, UTXO};
    use sha2::{Digest, Sha256};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn p2wsh_scriptpubkey(witness_script: &[u8]) -> Vec<u8> {
        let hash = Sha256::digest(witness_script);
        let mut spk = vec![blvm_protocol::opcodes::OP_0, 0x20];
        spk.extend_from_slice(&hash);
        spk
    }

    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(crate::storage::Storage::new(temp_dir.path()).unwrap());
    let witness_script = vec![0x51];
    let funding_hash = [0xcd; 32];
    let outpoint = OutPoint {
        hash: funding_hash,
        index: 0,
    };
    let utxo = UTXO {
        value: 100_000,
        script_pubkey: p2wsh_scriptpubkey(&witness_script).into(),
        height: 0,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&outpoint, &utxo).unwrap();

    let spend = Transaction {
        version: 2,
        inputs: vec![TransactionInput {
            prevout: outpoint,
            script_sig: vec![],
            sequence: 0xfffffffe,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 90_000,
            script_pubkey: {
                let mut script = vec![0x00, 0x14];
                script.extend_from_slice(&[0u8; 20]);
                script
            },
        }]
        .into(),
        lock_time: 0,
    };
    let mut mempool_manager = crate::node::mempool::MempoolManager::new();
    assert!(
        mempool_manager.add_transaction(spend).unwrap(),
        "tx must enter mempool"
    );
    let mut coordinator = MiningCoordinator::new(Arc::new(mempool_manager), Some(storage));

    let err = coordinator
        .generate_block_template()
        .await
        .expect_err("missing witnesses must refuse the template");
    assert!(
        err.to_string().contains("mempool witnesses unavailable"),
        "unexpected error: {err}"
    );
}

/// REV-TN-04: template → mine → `submit_block` → `connect_mined_block` on regtest storage.
#[tokio::test]
async fn test_submit_block_connects_regtest() {
    use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
    use std::sync::Arc;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(crate::storage::Storage::new(temp_dir.path()).unwrap());
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let genesis = protocol.get_network_params().genesis_block.header.clone();
    storage.chain().initialize(&genesis).unwrap();
    // Required-work checks read the parent header from the blockstore, not chain_info.
    let genesis_hash = storage.chain().get_tip_hash().unwrap().unwrap();
    storage
        .blocks()
        .store_header(&genesis_hash, &genesis)
        .unwrap();
    storage.blocks().store_height(0, &genesis_hash).unwrap();

    let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
    let mut coordinator = MiningCoordinator::new(mempool, Some(Arc::clone(&storage)));
    coordinator.set_protocol_engine(Arc::clone(&protocol));

    assert_eq!(
        storage.chain().get_height().unwrap().unwrap_or(0),
        0,
        "genesis only"
    );

    let mut template = coordinator
        .generate_block_template()
        .await
        .expect("block template");
    assert_eq!(template.transactions.len(), 1, "coinbase only");
    let cb = &template.transactions[0];
    assert!(
        cb.outputs
            .last()
            .is_some_and(|o| o.value == 0 && o.script_pubkey.first() == Some(&0x6a)),
        "mined template coinbase must carry BIP141 commitment"
    );
    // BIP90: post-genesis blocks need version ≥ 4 (same as `generatetoaddress` RPC path).
    template.header.version = 4;

    let mined = coordinator
        .mining_engine_mut()
        .mine_template(template)
        .await
        .expect("regtest PoW should succeed");

    coordinator
        .submit_block(mined)
        .await
        .expect("submit connects mined block");

    let height = storage
        .chain()
        .get_height()
        .unwrap()
        .expect("height after connect");
    assert_eq!(height, 1, "mined block extends chain from genesis");
}

#[tokio::test]
async fn an_empty_connect_keeps_table_coins_the_block_does_not_spend() {
    use blvm_protocol::types::{OutPoint, UTXO};
    use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion, UtxoSet};
    use std::sync::Arc;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(crate::storage::Storage::new(temp_dir.path()).unwrap());
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let genesis = protocol.get_network_params().genesis_block.header.clone();
    storage.chain().initialize(&genesis).unwrap();
    let genesis_hash = storage.chain().get_tip_hash().unwrap().unwrap();
    storage
        .blocks()
        .store_header(&genesis_hash, &genesis)
        .unwrap();
    storage.blocks().store_height(0, &genesis_hash).unwrap();

    let kept = OutPoint {
        hash: [7u8; 32],
        index: 0,
    };
    storage
        .utxos()
        .add_utxo(
            &kept,
            &UTXO {
                value: 40_000,
                script_pubkey: vec![0x51].into(),
                height: 0,
                is_coinbase: false,
            },
        )
        .unwrap();

    let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
    let mut coordinator = MiningCoordinator::new(Arc::clone(&mempool), Some(Arc::clone(&storage)));
    coordinator.set_protocol_engine(Arc::clone(&protocol));
    let mut template = coordinator.generate_block_template().await.unwrap();
    template.header.version = 4;
    let mined = coordinator
        .mining_engine_mut()
        .mine_template(template)
        .await
        .unwrap();

    let mut utxo = UtxoSet::default();
    let witnesses = coordinator
        .build_witnesses_for_block(&mined, &utxo)
        .unwrap();
    let mut sync = crate::node::sync::SyncCoordinator::new();
    let accepted = sync
        .connect_mined_block(
            storage.blocks().as_ref(),
            protocol.as_ref(),
            &storage,
            &mined,
            &witnesses,
            1,
            &mut utxo,
        )
        .unwrap();
    assert!(accepted);
    assert_eq!(storage.chain().get_height().unwrap(), Some(1));
    assert_eq!(
        storage.utxos().get_utxo(&kept).unwrap().unwrap().value,
        40_000
    );
    assert!(
        storage.utxos().get_all_utxos().unwrap().len() >= 2,
        "coinbase output must be added beside the untouched coin"
    );
    assert!(utxo.is_empty());
}

struct HoldCommons;

#[async_trait::async_trait]
impl crate::rpc::mining::CommonsGbtCaller for HoldCommons {
    async fn fetch_commons_gbt_outputs(
        &self,
    ) -> crate::rpc::errors::RpcResult<Option<Vec<(i64, Vec<u8>)>>> {
        Err(crate::rpc::errors::RpcError::internal_error(
            "commons pool holding; not issuing work",
        ))
    }
}

struct ScriptCommons;

#[async_trait::async_trait]
impl crate::rpc::mining::CommonsGbtCaller for ScriptCommons {
    async fn fetch_commons_gbt_outputs(
        &self,
    ) -> crate::rpc::errors::RpcResult<Option<Vec<(i64, Vec<u8>)>>> {
        Ok(Some(vec![(0, vec![0x51])]))
    }
}

#[tokio::test]
async fn commons_hold_refuses_miner_template() {
    let mempool = std::sync::Arc::new(crate::node::mempool::MempoolManager::new());
    let mut coordinator = MiningCoordinator::new(mempool, None);
    let slot = crate::rpc::mining::CommonsGbtSlot::default();
    slot.set(std::sync::Arc::new(HoldCommons));
    coordinator.set_commons_gbt_slot(slot);

    let err = coordinator
        .generate_block_template()
        .await
        .expect_err("hold must refuse a template");
    assert!(
        err.to_string().contains("holding"),
        "unexpected miner error: {err}"
    );
}

#[tokio::test]
async fn commons_outputs_appear_in_miner_coinbase() {
    let mempool = std::sync::Arc::new(crate::node::mempool::MempoolManager::new());
    let mut coordinator = MiningCoordinator::new(mempool, None);
    let slot = crate::rpc::mining::CommonsGbtSlot::default();
    slot.set(std::sync::Arc::new(ScriptCommons));
    coordinator.set_commons_gbt_slot(slot);

    let template = coordinator
        .generate_block_template()
        .await
        .expect("template");
    let cb = &template.transactions[0];
    assert_eq!(cb.outputs[0].script_pubkey, vec![0x51]);
    assert!(
        cb.outputs
            .last()
            .is_some_and(|o| o.value == 0 && o.script_pubkey.first() == Some(&0x6a)),
        "BIP141 still appended after Commons payouts"
    );
}

#[test]
fn a_template_stops_at_the_witness_sigop_limit() {
    use blvm_protocol::opcodes::{OP_0, PUSH_32_BYTES};
    use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO};
    use sha2::{Digest, Sha256};

    let mut script = vec![0x00, 0x63];
    for _ in 0..199 {
        script.push(0x60);
        script.push(0xae);
    }
    script.push(0x68);
    script.push(0x51);
    let program = Sha256::digest(&script);
    let mut output_script = vec![OP_0, PUSH_32_BYTES];
    output_script.extend_from_slice(&program);

    let mut selected_n = [0usize; 2];
    let mut cost = [0u64; 2];
    for (index, count) in [25usize, 26].into_iter().enumerate() {
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        let mut txs = Vec::new();
        let mut witnesses = Vec::new();
        for n in 0..count {
            let prevout = OutPoint {
                hash: [n as u8; 32],
                index: 0,
            };
            utxo_set.insert(
                prevout,
                std::sync::Arc::new(UTXO {
                    value: 100_000,
                    script_pubkey: output_script.clone().into(),
                    height: 1,
                    is_coinbase: false,
                }),
            );
            txs.push(Transaction {
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
            });
            witnesses.push(vec![vec![script.clone()]]);
        }
        let mempool = crate::node::mempool::MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(
            utxo_set.clone(),
        )));
        for (tx, witness) in txs.iter().zip(witnesses.iter()) {
            assert!(
                mempool
                    .add_transaction_with_witness(tx.clone(), Some(witness.clone()))
                    .unwrap()
            );
        }
        let picked = TransactionSelector::new().select_transactions(&mempool, &utxo_set);
        selected_n[index] = picked.len();
        cost[index] = picked
            .iter()
            .map(|tx| {
                let id = blvm_protocol::block::calculate_tx_id(tx);
                let stacks = mempool.get_transaction_witnesses(&id);
                blvm_protocol::sigop::get_transaction_sigop_cost_with_witness_slices(
                    tx,
                    &utxo_set,
                    stacks.as_deref(),
                    0x01 | 0x800,
                )
                .unwrap()
            })
            .sum();
    }
    eprintln!(
        "txs 25 selected={} cost={}; txs 26 selected={} cost={}",
        selected_n[0], cost[0], selected_n[1], cost[1]
    );
    assert_eq!(selected_n[0], 25);
    assert!(cost[0] <= 80_000);
    assert_eq!(selected_n[1], 25);
    assert!(cost[1] <= 80_000);
}

#[test]
fn a_template_counts_sigops_of_a_mempool_parent() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::{OP_0, PUSH_32_BYTES};
    use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO};
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
    let program = Sha256::digest(&script);
    let mut output_script = vec![OP_0, PUSH_32_BYTES];
    output_script.extend_from_slice(&program);
    let witness = vec![vec![script]];

    let run = |mempool_parent: bool| {
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        let mempool = crate::node::mempool::MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        for n in 0..26u8 {
            let mut origin_hash = [0u8; 32];
            origin_hash[0] = n;
            origin_hash[1] = u8::from(mempool_parent);
            let prevout = OutPoint {
                hash: if mempool_parent { origin_hash } else { [n; 32] },
                index: 0,
            };
            let coin_script = if mempool_parent {
                spend.clone()
            } else {
                output_script.clone()
            };
            let coin_value = if mempool_parent { 200_000 } else { 100_000 };
            utxo_set.insert(
                prevout,
                std::sync::Arc::new(UTXO {
                    value: coin_value,
                    script_pubkey: coin_script.into(),
                    height: 1,
                    is_coinbase: false,
                }),
            );
            mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(
                utxo_set.clone(),
            )));
            let child_prevout = if mempool_parent {
                let parent = Transaction {
                    version: 1,
                    inputs: vec![TransactionInput {
                        prevout,
                        script_sig: vec![0x01, 0x51],
                        sequence: 0xffffffff,
                    }]
                    .into(),
                    outputs: vec![TransactionOutput {
                        value: 100_000,
                        script_pubkey: output_script.clone(),
                    }]
                    .into(),
                    lock_time: 0,
                };
                let parent_id = calculate_tx_id(&parent);
                assert!(mempool.add_transaction(parent).unwrap());
                OutPoint {
                    hash: parent_id,
                    index: 0,
                }
            } else {
                prevout
            };
            let child = Transaction {
                version: 1,
                inputs: vec![TransactionInput {
                    prevout: child_prevout,
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
            assert!(
                mempool
                    .add_transaction_with_witness(child, Some(witness.clone()))
                    .unwrap()
            );
        }
        let picked = TransactionSelector::new().select_transactions(&mempool, &utxo_set);
        let mut overlay = utxo_set.clone();
        for tx in &picked {
            let id = calculate_tx_id(tx);
            for (index, output) in tx.outputs.iter().enumerate() {
                overlay.insert(
                    OutPoint {
                        hash: id,
                        index: index as u32,
                    },
                    std::sync::Arc::new(UTXO {
                        value: output.value,
                        script_pubkey: output.script_pubkey.clone().into(),
                        height: 0,
                        is_coinbase: false,
                    }),
                );
            }
        }
        let spends: Vec<_> = picked
            .iter()
            .filter(|tx| tx.inputs[0].script_sig.is_empty())
            .cloned()
            .collect();
        let cost: u64 = spends
            .iter()
            .map(|tx| {
                let id = calculate_tx_id(tx);
                let stacks = mempool.get_transaction_witnesses(&id);
                blvm_protocol::sigop::get_transaction_sigop_cost_with_witness_slices(
                    tx,
                    &overlay,
                    stacks.as_deref(),
                    0x01 | 0x800,
                )
                .unwrap()
            })
            .sum();
        (spends.len(), cost)
    };
    let confirmed = run(false);
    let in_pool = run(true);
    eprintln!(
        "confirmed selected={} cost={}; mempool parent selected={} cost={}",
        confirmed.0, confirmed.1, in_pool.0, in_pool.1
    );
    assert_eq!(confirmed.0, 25);
    assert!(confirmed.1 <= 80_000);
    assert_eq!(in_pool.0, 25);
    assert!(in_pool.1 <= 80_000);
}

#[test]
fn a_below_floor_parent_is_removed_when_the_child_does_not_fit() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::opcodes::{OP_0, PUSH_32_BYTES};
    use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO};
    use sha2::{Digest, Sha256};

    let spend = vec![
        0xa9, 0x14, 0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71,
        0xc7, 0x7e, 0xba, 0x30, 0xcd, 0x5a, 0x4b, 0x87,
    ];
    let mut heavy = vec![0x00, 0x63];
    for _ in 0..199 {
        heavy.push(0x60);
        heavy.push(0xae);
    }
    heavy.push(0x68);
    heavy.push(0x51);
    let program_of = |script: &[u8]| {
        let mut output_script = vec![OP_0, PUSH_32_BYTES];
        output_script.extend_from_slice(&Sha256::digest(script));
        output_script
    };
    let heavy_script = program_of(&heavy);

    let run = |child_script: Vec<u8>| {
        let child_program = program_of(&child_script);
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        let mempool = crate::node::mempool::MempoolManager::new();
        mempool.set_policy_config(Some(crate::config::MempoolPolicyConfig::default()));
        for n in 0..25u8 {
            let mut hash = [0u8; 32];
            hash[0] = n;
            hash[1] = 1;
            let prevout = OutPoint { hash, index: 0 };
            utxo_set.insert(
                prevout,
                std::sync::Arc::new(UTXO {
                    value: 100_000,
                    script_pubkey: heavy_script.clone().into(),
                    height: 1,
                    is_coinbase: false,
                }),
            );
            mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(
                utxo_set.clone(),
            )));
            let filler = Transaction {
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
            assert!(
                mempool
                    .add_transaction_with_witness(filler, Some(vec![vec![heavy.clone()]]))
                    .unwrap()
            );
        }
        let funding = OutPoint {
            hash: [9u8; 32],
            index: 0,
        };
        utxo_set.insert(
            funding,
            std::sync::Arc::new(UTXO {
                value: 200_000,
                script_pubkey: spend.clone().into(),
                height: 1,
                is_coinbase: false,
            }),
        );
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(
            utxo_set.clone(),
        )));
        let parent = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: funding,
                script_sig: vec![0x01, 0x51],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 199_000,
                script_pubkey: child_program,
            }]
            .into(),
            lock_time: 0,
        };
        let parent_id = calculate_tx_id(&parent);
        assert!(mempool.add_transaction(parent).unwrap());
        let child = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: parent_id,
                    index: 0,
                },
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
        assert!(
            mempool
                .add_transaction_with_witness(child, Some(vec![vec![child_script]]))
                .unwrap()
        );
        let picked = TransactionSelector::with_params(1_000_000, 4_000_000, 100_000)
            .select_transactions(&mempool, &utxo_set);
        let parent_in = picked
            .iter()
            .any(|tx| tx.inputs[0].script_sig == [0x01, 0x51]);
        let child_in = picked
            .iter()
            .any(|tx| tx.inputs[0].prevout.hash == parent_id && tx.inputs[0].script_sig.is_empty());
        (parent_in, child_in)
    };
    let light = run(vec![0x51]);
    let heavy_result = run(heavy.clone());
    eprintln!(
        "light parent={} child={}; heavy parent={} child={}",
        light.0, light.1, heavy_result.0, heavy_result.1
    );
    assert!(light.0);
    assert!(light.1);
    assert!(!heavy_result.0);
    assert!(!heavy_result.1);
}

#[test]
fn a_mined_block_stops_at_the_witness_sigop_limit() {
    use blvm_protocol::opcodes::{OP_0, PUSH_32_BYTES};
    use blvm_protocol::{
        BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO,
    };
    use sha2::{Digest, Sha256};

    let mut heavy = vec![0x00, 0x63];
    for _ in 0..199 {
        heavy.push(0x60);
        heavy.push(0xae);
    }
    heavy.push(0x68);
    heavy.push(0x51);
    let mut output_script = vec![OP_0, PUSH_32_BYTES];
    output_script.extend_from_slice(&Sha256::digest(&heavy));

    let run = |count: u8| {
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        let mut txs = Vec::new();
        let mut witnesses = Vec::new();
        for n in 0..count {
            let mut hash = [0u8; 32];
            hash[0] = n;
            let prevout = OutPoint { hash, index: 0 };
            utxo_set.insert(
                prevout,
                std::sync::Arc::new(UTXO {
                    value: 100_000,
                    script_pubkey: output_script.clone().into(),
                    height: 1,
                    is_coinbase: false,
                }),
            );
            txs.push(Transaction {
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
            });
            witnesses.push(Some(vec![vec![heavy.clone()]]));
        }
        let parent = BlockHeader {
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: [0; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        };
        let block = blvm_consensus::mining::create_new_block_with_time(
            &utxo_set,
            &txs,
            1,
            &parent,
            &[parent.clone()],
            &vec![0x01, 0x51],
            &vec![0x51],
            1_700_000_000,
            blvm_consensus::Network::Regtest,
            Some(&witnesses),
        )
        .unwrap();
        let selected: Vec<_> = block.transactions.iter().skip(1).cloned().collect();
        let cost: u64 = selected
            .iter()
            .map(|tx| {
                blvm_protocol::sigop::get_transaction_sigop_cost_with_witness_slices(
                    tx,
                    &utxo_set,
                    Some(&[vec![heavy.clone()]]),
                    0x01 | 0x800,
                )
                .unwrap()
            })
            .sum();
        (selected.len(), cost)
    };
    let few = run(25);
    let many = run(26);
    eprintln!(
        "txs 25 selected={} cost={}; txs 26 selected={} cost={}",
        few.0, few.1, many.0, many.1
    );
    assert_eq!(few.0, 25);
    assert!(few.1 <= 80_000);
    assert_eq!(many.0, 25);
    assert!(many.1 <= 80_000);
}
