//! Section 14.4. MempoolManager and the estimator, not the hooks alone.

use super::{BlockObservation, FeeEstimator, HookEvent};
use crate::config::FeeEstimatorConfig;
use crate::node::mempool::MempoolManager;
use blvm_protocol::block::calculate_tx_id;
use blvm_protocol::opcodes::OP_1;
use blvm_protocol::{
    Block, BlockHeader, Hash, OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO,
    UtxoSet,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn spend(prev_hash: Hash) -> Transaction {
    Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: prev_hash,
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

fn fund(txs: &[Transaction]) -> UtxoSet {
    let mut set = UtxoSet::default();
    for tx in txs {
        set.insert(
            tx.inputs[0].prevout,
            Arc::new(UTXO {
                value: 50_000,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
    }
    set
}

fn wired(txs: &[Transaction]) -> (MempoolManager, Arc<FeeEstimator>) {
    let mempool = MempoolManager::new();
    let estimator = Arc::new(FeeEstimator::new(FeeEstimatorConfig::default(), true));
    estimator.enable_hook_trace();
    mempool.set_fee_estimator(Some(Arc::clone(&estimator)));
    mempool.set_chain_tip(1, 0);
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(fund(txs))));
    (mempool, estimator)
}

fn observation(height: u64, txid: Hash) -> BlockObservation {
    BlockObservation {
        height,
        hash: [height as u8; 32],
        weight: 4_000,
        tx_weights: vec![(txid, 400)],
    }
}

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

#[test]
fn scripted_blocks_account_for_admission_confirmation_failure_and_reload() {
    let confirmed_txs: Vec<Transaction> = (1..4).map(|index| spend([index; 32])).collect();
    let failed = spend([9u8; 32]);
    let mut all = confirmed_txs.clone();
    all.push(failed.clone());
    let (mempool, estimator) = wired(&all);
    for tx in &all {
        assert!(mempool.add_transaction(tx.clone()).unwrap());
    }
    assert_eq!(estimator.hook_counts().admitted, 4);
    assert_eq!(estimator.hook_counts().reloaded, 0);

    let failed_id = calculate_tx_id(&failed);
    mempool.remove_for_connected_block_at(
        Some(observation(1, calculate_tx_id(&confirmed_txs[0]))),
        &[confirmed_txs[0].clone()],
    );
    assert!(mempool.remove_transaction(&failed_id));
    assert_eq!(estimator.hook_counts().confirmed, 1);
    assert_eq!(estimator.hook_counts().failed, 1);

    for (offset, tx) in confirmed_txs.iter().enumerate().skip(1) {
        mempool.remove_for_connected_block_at(
            Some(observation((offset as u64) + 1, calculate_tx_id(tx))),
            std::slice::from_ref(tx),
        );
    }
    let counts = estimator.hook_counts();
    assert_eq!(counts.admitted, 4);
    assert_eq!(counts.confirmed, 3);
    assert_eq!(counts.failed, 1);
    assert_eq!(counts.blocks, 3);
    assert_eq!(counts.reloaded, 0);

    let events = estimator.hook_events();
    for tx in &confirmed_txs {
        let id = calculate_tx_id(tx);
        let removed = events
            .iter()
            .position(|event| matches!(event, HookEvent::Removed { txid } if *txid == id))
            .expect("confirmed removal");
        let observed = events[..removed]
            .iter()
            .rev()
            .find_map(|event| match event {
                HookEvent::Block { height } => Some(*height),
                _ => None,
            })
            .expect("observation before removal");
        assert!(observed >= 1);
    }

    let reloaded_tx = spend([8u8; 32]);
    let reloaded_id = calculate_tx_id(&reloaded_tx);
    let source = MempoolManager::new();
    source.set_chain_tip(1, 0);
    source.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(fund(&[
        reloaded_tx.clone()
    ]))));
    assert!(source.add_transaction(reloaded_tx.clone()).unwrap());
    let dir = std::env::temp_dir().join(format!(
        "blvm-fee-14-4-{}-{}",
        std::process::id(),
        reloaded_id[0]
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("mempool.dat");
    source.save_to_disk(&path).unwrap();

    let loaded = MempoolManager::new();
    let reload_estimator = Arc::new(FeeEstimator::new(FeeEstimatorConfig::default(), true));
    reload_estimator.enable_hook_trace();
    loaded.set_fee_estimator(Some(Arc::clone(&reload_estimator)));
    loaded.set_chain_tip(1, 0);
    loaded.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(fund(&[reloaded_tx]))));
    loaded.load_from_disk(&path).unwrap();
    loaded.remove_for_connected_block_at(
        Some(BlockObservation {
            height: 1,
            hash: [1u8; 32],
            weight: 4_000,
            tx_weights: Vec::new(),
        }),
        &[],
    );
    assert!(loaded.remove_transaction(&reloaded_id));
    let reloaded_counts = reload_estimator.hook_counts();
    assert_eq!(reloaded_counts.reloaded, 1);
    assert_eq!(reloaded_counts.confirmed, 0);
    assert_eq!(reloaded_counts.failed, 0);
    assert!(matches!(
        reload_estimator.hook_events().first(),
        Some(HookEvent::Admitted {
            txid,
            from_reload: true,
            from_package: false,
        }) if *txid == reloaded_id
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn a_reorg_leaves_confirmation_history_unchanged() {
    let confirmed = spend([1u8; 32]);
    let stays = spend([2u8; 32]);
    let resurrected = spend([3u8; 32]);
    let (mempool, estimator) = wired(&[confirmed.clone(), stays.clone(), resurrected.clone()]);
    assert!(mempool.add_transaction(confirmed.clone()).unwrap());
    mempool.remove_for_connected_block_at(
        Some(observation(1, calculate_tx_id(&confirmed))),
        &[confirmed],
    );
    // Admitted at the height just recorded, so `blocksAgo` is still below
    // `CBlockPolicyEstimator::SHORT_SCALE` when the reorg removes it.
    mempool.set_chain_tip(2, 0);
    assert!(mempool.add_transaction(stays.clone()).unwrap());
    let before = estimator.hook_counts();
    assert_eq!(before.confirmed, 1);
    assert_eq!(before.failed, 0);

    let blocks_before = estimator.hook_events().len();
    mempool.apply_reorg(
        &[block_with(vec![resurrected.clone()])],
        &[],
        &[block_with(vec![stays.clone()])],
        &fund(&[resurrected.clone()]),
    );
    let after = estimator.hook_counts();
    assert_eq!(after.confirmed, before.confirmed);
    assert_eq!(after.failed, before.failed);
    assert_eq!(after.blocks, before.blocks);
    assert!(
        estimator.hook_events()[blocks_before..]
            .iter()
            .all(|event| !matches!(event, HookEvent::Block { .. }))
    );
    assert!(
        mempool
            .get_transaction(&calculate_tx_id(&resurrected))
            .is_some()
    );
    assert!(mempool.get_transaction(&calculate_tx_id(&stays)).is_none());
}

#[tokio::test]
async fn table_after_a_connected_block_is_not_older_than_that_block() {
    let tx = spend([1u8; 32]);
    let (mempool, estimator) = wired(&[tx.clone()]);
    assert!(mempool.add_transaction(tx.clone()).unwrap());
    let before = estimator.table().await.unwrap();
    mempool.remove_for_connected_block_at(Some(observation(1, calculate_tx_id(&tx))), &[tx]);
    let after = estimator.table().await.unwrap();
    assert!(after.block_seq > before.block_seq);
    assert_eq!(after.block_seq, estimator.hook_counts().blocks);
    assert_eq!(after.tip_height, 1);
}

#[tokio::test]
async fn concurrent_admissions_removals_and_tables_match_block_seq() {
    let mempool = Arc::new(MempoolManager::new());
    let estimator = Arc::new(FeeEstimator::new(FeeEstimatorConfig::default(), true));
    mempool.set_fee_estimator(Some(Arc::clone(&estimator)));
    mempool.set_chain_tip(1, 0);
    let mut coins = UtxoSet::default();
    for worker in 0..4u8 {
        for n in 0..64u8 {
            let mut hash = [0u8; 32];
            hash[0] = worker;
            hash[1] = n;
            coins.insert(
                OutPoint { hash, index: 0 },
                Arc::new(UTXO {
                    value: 50_000,
                    script_pubkey: vec![OP_1].into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
        }
    }
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(coins)));

    let stop = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();
    for worker in 0..4u8 {
        let mempool = Arc::clone(&mempool);
        let stop = Arc::clone(&stop);
        workers.push(std::thread::spawn(move || {
            let mut n = 0u8;
            while !stop.load(Ordering::Relaxed) {
                let mut hash = [0u8; 32];
                hash[0] = worker;
                hash[1] = n;
                let tx = spend(hash);
                let id = calculate_tx_id(&tx);
                let _ = mempool.add_transaction(tx);
                if n % 3 == 0 {
                    let _ = mempool.remove_transaction(&id);
                }
                n = n.wrapping_add(1) % 64;
            }
        }));
    }
    let block_pool = Arc::clone(&mempool);
    let block_stop = Arc::clone(&stop);
    let blocks = std::thread::spawn(move || {
        let mut height = 1u64;
        while !block_stop.load(Ordering::Relaxed) {
            block_pool.remove_for_connected_block_at(
                Some(BlockObservation {
                    height,
                    hash: [height as u8; 32],
                    weight: 1_000,
                    tx_weights: Vec::new(),
                }),
                &[],
            );
            height = height.saturating_add(1);
            std::thread::sleep(Duration::from_millis(2));
        }
    });

    let table_estimator = Arc::clone(&estimator);
    let table_stop = Arc::clone(&stop);
    let tables = tokio::spawn(async move {
        while !table_stop.load(Ordering::Relaxed) {
            let before = table_estimator.hook_counts().blocks;
            let table = table_estimator.table().await.unwrap();
            let after = table_estimator.hook_counts().blocks;
            assert!(table.block_seq >= before);
            assert!(table.block_seq <= after);
        }
    });

    let finished = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::time::sleep(Duration::from_millis(250)).await;
        stop.store(true, Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            for worker in workers {
                worker.join().expect("admission thread");
            }
            blocks.join().expect("block thread");
        })
        .await
        .expect("join");
        tables.await.expect("table task");
        let table = estimator.table().await.unwrap();
        assert_eq!(table.block_seq, estimator.hook_counts().blocks);
    })
    .await;
    assert!(finished.is_ok(), "hammer timed out");
}
