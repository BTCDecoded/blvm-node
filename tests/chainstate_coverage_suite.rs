//! ChainState metadata, caches, and recovery paths.

use blvm_node::BlockHeader;
use blvm_node::storage::chainstate::{ChainInfo, ChainParams, UTXOStats};
use blvm_protocol::test_utils::create_test_header;
use std::sync::Arc;

mod common;
use common::{DIFFICULTY_INTERVAL, setup_mining_chain_on};

fn seeded_storage(blocks: u64) -> (tempfile::TempDir, Arc<blvm_node::storage::Storage>) {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(blvm_node::storage::Storage::new(temp_dir.path()).unwrap());
    setup_mining_chain_on(&storage, blocks).unwrap();
    (temp_dir, storage)
}

use blvm_consensus::pow::U256;

#[test]
fn test_store_and_load_work_and_chainwork() {
    let temp_db = common::TempDb::new().unwrap();
    let chain = &temp_db.chain_state;
    let header = create_test_header(1_700_000_000, [0x11; 32]);
    chain.initialize(&header).unwrap();
    let hash = [0x22u8; 32];
    chain.store_work(&hash, U256::from_u128(42)).unwrap();
    chain.store_chainwork(&hash, U256::from_u128(999)).unwrap();
    assert_eq!(chain.get_work(&hash).unwrap(), Some(U256::from_u128(42)));
    assert_eq!(
        chain.get_chainwork(&hash).unwrap(),
        Some(U256::from_u128(999))
    );
    assert!(chain.calculate_total_work().unwrap() >= 42);
}

#[test]
fn test_cache_block_chainwork_matches_consensus_work() {
    let temp_db = common::TempDb::new().unwrap();
    let chain = &temp_db.chain_state;
    let mut header = create_test_header(1_700_000_000, [0x11; 32]);
    header.bits = 0x0300ffff;
    chain.initialize(&header).unwrap();
    let hash = [0x44u8; 32];
    chain.cache_block_chainwork(&hash, &header, 0).unwrap();
    let expected = blvm_consensus::reorganization::work_for_bits(header.bits).unwrap();
    assert_eq!(chain.get_work(&hash).unwrap(), Some(expected));
    assert_eq!(chain.get_chainwork(&hash).unwrap(), Some(expected));
}

#[test]
fn test_utxo_stats_and_hashrate_caches() {
    let (_dir, storage) = seeded_storage(12);
    let chain = storage.chain();
    let tip = chain.get_tip_hash().unwrap().expect("tip");
    let height = chain.get_height().unwrap().unwrap();

    let stats = UTXOStats {
        height,
        txouts: 3,
        total_amount: 150_000,
        muhash: [0x33; 32],
        transactions: 5,
    };
    chain.store_utxo_stats(&tip, &stats).unwrap();
    let loaded = chain.get_utxo_stats(&tip).unwrap().expect("stats");
    assert_eq!(loaded.txouts, 3);
    assert_eq!(
        chain.get_latest_utxo_stats().unwrap().unwrap().height,
        height
    );

    chain.store_network_hashrate(height, 1.5e12).unwrap();
    assert!(chain.get_network_hashrate().unwrap().unwrap() > 0.0);
}

#[test]
fn test_update_utxo_stats_cache_from_protocol_utxo_set() {
    use blvm_protocol::UtxoSet;
    use std::sync::Arc;

    let (_dir, storage) = seeded_storage(8);
    let chain = storage.chain();
    let tip = chain.get_tip_hash().unwrap().expect("tip");
    let height = chain.get_height().unwrap().unwrap();

    let mut utxo_set = UtxoSet::default();
    utxo_set.insert(
        blvm_protocol::OutPoint {
            hash: [0x44; 32],
            index: 0,
        },
        Arc::new(blvm_protocol::UTXO {
            value: 50_000,
            script_pubkey: vec![0x51].into(),
            height: 1,
            is_coinbase: false,
        }),
    );

    chain
        .update_utxo_stats_cache(&tip, height, &utxo_set, 10)
        .unwrap();
    let cached = chain.get_utxo_stats(&tip).unwrap().expect("cached stats");
    assert_eq!(cached.txouts, 1);
    assert_eq!(cached.transactions, 10);
}

#[test]
fn test_calculate_and_cache_network_hashrate_with_chain() {
    let (_dir, storage) = seeded_storage(DIFFICULTY_INTERVAL);
    let chain = storage.chain();
    let height = chain.get_height().unwrap().unwrap();
    chain
        .calculate_and_cache_network_hashrate(height, storage.blocks().as_ref())
        .unwrap();
    assert!(chain.get_network_hashrate().unwrap().is_some());
}

#[test]
fn test_invalid_block_mark_and_unmark() {
    let temp_db = common::TempDb::new().unwrap();
    let chain = &temp_db.chain_state;
    let hash = [0x55u8; 32];
    assert!(!chain.is_invalid(&hash).unwrap());
    chain.mark_invalid(&hash).unwrap();
    assert!(chain.is_invalid(&hash).unwrap());
    assert_eq!(chain.get_invalid_blocks().unwrap(), vec![hash]);
    chain.unmark_invalid(&hash).unwrap();
    assert!(!chain.is_invalid(&hash).unwrap());
}

#[test]
fn test_utxo_and_engine_watermarks() {
    let temp_db = common::TempDb::new().unwrap();
    let chain = &temp_db.chain_state;
    let header = create_test_header(1_700_000_001, [0x66; 32]);
    chain.initialize(&header).unwrap();

    chain.set_utxo_watermark(100).unwrap();
    assert_eq!(chain.get_utxo_watermark().unwrap(), Some(100));
    chain.force_set_ibd_utxo_watermark(200).unwrap();
    assert_eq!(chain.get_utxo_watermark().unwrap(), Some(200));

    chain.persist_engine_export_height(512).unwrap();
    assert_eq!(chain.get_engine_export_height().unwrap(), Some(512));
    chain
        .persist_engine_checkpoint_complete(
            512,
            1,
            10_000,
            &[0u8; blvm_muhash::MUHASH_RUNNING_STATE_BYTES],
        )
        .unwrap();
    assert_eq!(chain.get_engine_export_utxo_count().unwrap(), Some(10_000));
    chain.force_reset_engine_checkpoint_metadata().unwrap();
    assert_eq!(chain.get_engine_export_height().unwrap(), Some(0));
}

#[test]
fn test_recover_chain_tip_from_blockstore_without_chain_info() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = blvm_node::storage::Storage::new(temp_dir.path()).unwrap();
    setup_mining_chain_on(&storage, 20).unwrap();
    storage.chain().reset().unwrap();
    assert!(storage.chain().load_chain_info().unwrap().is_none());
    storage.recover_chain_tip_from_blockstore().unwrap();
    let info = storage
        .chain()
        .load_chain_info()
        .unwrap()
        .expect("recovered");
    assert_eq!(info.height, 19);
}

#[test]
fn test_store_chain_info_roundtrip() {
    let temp_db = common::TempDb::new().unwrap();
    let chain = &temp_db.chain_state;
    let header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0x77; 32],
        timestamp: 1_700_000_002,
        bits: 0x0f00ffff,
        nonce: 1,
    };
    chain.initialize(&header).unwrap();
    let tip = [0x88u8; 32];
    let info = ChainInfo {
        tip_hash: tip,
        tip_header: header.clone(),
        height: 5,
        total_work: 123,
        chain_params: ChainParams::default(),
    };
    chain.store_chain_info(&info).unwrap();
    let loaded = chain.load_chain_info().unwrap().expect("chain info");
    assert_eq!(loaded.height, 5);
    assert_eq!(loaded.tip_hash, tip);
}
