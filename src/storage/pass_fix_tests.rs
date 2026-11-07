use super::blockstore::BlockStore;
use super::chainstate::{ChainState, utxo_money_total};
use super::database::{DatabaseBackend, create_database};
use super::hashing::double_sha256;
use super::pruning::PruningManager;
use super::txindex::TxIndex;
use super::utxostore::UtxoStore;
use super::Storage;
use crate::config::{PruningConfig, PruningMode};
use blvm_consensus::reorganization::BlockUndoLog;
use blvm_protocol::constants::MAX_MONEY;
use blvm_protocol::{
    Block, BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO,
};
use std::sync::Arc;

fn temp_db() -> (tempfile::TempDir, Arc<dyn super::database::Database>) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::from(create_database(dir.path(), DatabaseBackend::Heed3, None).unwrap());
    (dir, db)
}

fn header_hash(header: &BlockHeader) -> [u8; 32] {
    let mut data = [0u8; 80];
    data[0..4].copy_from_slice(&(header.version as i32).to_le_bytes());
    data[4..36].copy_from_slice(&header.prev_block_hash);
    data[36..68].copy_from_slice(&header.merkle_root);
    data[68..72].copy_from_slice(&(header.timestamp as u32).to_le_bytes());
    data[72..76].copy_from_slice(&(header.bits as u32).to_le_bytes());
    data[76..80].copy_from_slice(&(header.nonce as u32).to_le_bytes());
    double_sha256(&data)
}

fn sample_block(nonce: u64) -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: [2; 32],
            timestamp: 1_231_006_505,
            bits: 0x1d00_ffff,
            nonce,
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![TransactionInput {
                prevout: OutPoint {
                    hash: [3; 32],
                    index: 0,
                },
                sequence: 0xffff_ffff,
                script_sig: vec![1, 2, 3],
            }],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into_boxed_slice(),
    }
}

fn store_body(store: &BlockStore, block: &Block, height: u64) {
    let hash = header_hash(&block.header);
    store.store_block(block).unwrap();
    store.store_height(height, &hash).unwrap();
    store.store_undo_log(&hash, &BlockUndoLog::new()).unwrap();
}

#[test]
fn money_total_rejects_a_value_outside_the_range() {
    assert_eq!(
        utxo_money_total([MAX_MONEY]).unwrap(),
        MAX_MONEY as u128
    );
    assert!(utxo_money_total([MAX_MONEY + 1]).is_err());
    assert!(utxo_money_total([-1]).is_err());
}

#[test]
fn legacy_36_byte_utxo_key_is_readable() {
    let (_dir, db) = temp_db();
    let store = UtxoStore::new(Arc::clone(&db)).unwrap();
    let outpoint = OutPoint {
        hash: [9; 32],
        index: 7,
    };
    let utxo = UTXO {
        value: 50,
        script_pubkey: vec![0x51].into(),
        height: 3,
        is_coinbase: false,
    };
    let bytes =
        crate::storage::utxo_value_codec::encode_utxo_with_codec(store.value_codec(), &utxo)
            .unwrap();
    let mut key = [0u8; 36];
    key[..32].copy_from_slice(&outpoint.hash);
    key[32..].copy_from_slice(&outpoint.index.to_be_bytes());
    db.open_tree("utxos").unwrap().insert(&key, &bytes).unwrap();
    let loaded = store.get_utxo(&outpoint).unwrap().expect("legacy key");
    assert_eq!(loaded.value, 50);
    store.remove_utxo(&outpoint).unwrap();
    assert!(store.get_utxo(&outpoint).unwrap().is_none());
}

#[test]
fn wire_body_is_indexed_by_hash() {
    let (_dir, db) = temp_db();
    let store = BlockStore::new(Arc::clone(&db)).unwrap();
    let block = sample_block(7);
    let hash = header_hash(&block.header);
    let wire = blvm_protocol::serialization::serialize_transaction(&block.transactions[0]);
    store
        .store_block_wire_bytes(&block, 7, &wire)
        .unwrap();
    assert!(store.has_block_body(&hash).unwrap());
    assert_eq!(store.get_height_by_hash(&hash).unwrap(), Some(7));
    assert!(store.get_block(&hash).is_err());
}

#[test]
fn a_height_gap_stays_off_the_contiguous_tip() {
    let (_dir, db) = temp_db();
    let store = BlockStore::new(db).unwrap();
    let block = sample_block(2);
    let hash = header_hash(&block.header);
    store.store_height(2, &hash).unwrap();
    assert_eq!(store.highest_stored_height().unwrap(), None);
    assert_eq!(store.stored_height_gap().unwrap(), Some(0));
}

#[test]
fn indexed_size_matches_the_wire_length() {
    let (_dir, db) = temp_db();
    let index = TxIndex::new(db).unwrap();
    let block = sample_block(4);
    let tx = &block.transactions[0];
    let wire = blvm_protocol::serialization::serialize_transaction(tx).len() as u32;
    index
        .index_transaction(tx, &[5; 32], 1, 0)
        .unwrap();
    let txid = blvm_protocol::block::calculate_tx_id(tx);
    let meta = index.get_metadata(&txid).unwrap().unwrap();
    assert_eq!(meta.size, wire);
    assert_eq!(meta.weight, wire * 4);
}

#[test]
fn undo_batch_and_recent_header_batch_round_trip() {
    let (_dir, db) = temp_db();
    let store = BlockStore::new(db).unwrap();
    let hash = [6u8; 32];
    store
        .store_undo_logs_batch(&[(&hash, &BlockUndoLog::new())])
        .unwrap();
    assert!(store.get_undo_log(&hash).unwrap().is_some());
    let block = sample_block(12);
    let header = block.header.clone();
    store.store_height(0, &[1u8; 32]).unwrap();
    store
        .store_recent_headers_ibd_batch(&[(0, &header)])
        .unwrap();
    let recent = store.get_recent_headers(1).unwrap();
    assert_eq!(recent[0].nonce, header.nonce);
}

#[test]
fn nosync_switch_returns() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::with_backend(dir.path(), DatabaseBackend::Heed3).unwrap();
    storage.set_ibd_nosync(true).unwrap();
    let tree = storage.open_tree("chain_info").unwrap();
    tree.insert(b"nosync", b"1").unwrap();
    storage.set_ibd_nosync(false).unwrap();
    tree.insert(b"sync", b"1").unwrap();
    assert_eq!(tree.get(b"sync").unwrap().unwrap(), b"1");
}

#[test]
fn custom_prune_drops_the_tx_index_when_asked() {
    let (_dir, db) = temp_db();
    let blockstore = Arc::new(BlockStore::new(Arc::clone(&db)).unwrap());
    let index = Arc::new(TxIndex::new(Arc::clone(&db)).unwrap());
    let block = sample_block(1);
    store_body(&blockstore, &block, 0);
    let tx = &block.transactions[0];
    let txid = blvm_protocol::block::calculate_tx_id(tx);
    let hash = header_hash(&block.header);
    index.index_transaction(tx, &hash, 0, 0).unwrap();
    let config = PruningConfig {
        mode: PruningMode::Custom {
            keep_headers: true,
            keep_bodies_from_height: 1,
            keep_commitments: false,
            keep_filters: false,
            keep_filtered_blocks: false,
            keep_witnesses: true,
            keep_tx_index: false,
        },
        ..PruningConfig::default()
    };
    let manager = PruningManager::new(config, Arc::clone(&blockstore)).with_tx_index(index.clone());
    manager.prune_to_height(2, 300, false).unwrap();
    assert!(index.get_metadata(&txid).unwrap().is_none());
    assert!(blockstore.get_block(&hash).unwrap().is_none());
}

#[test]
fn normal_prune_stops_at_the_keep_window() {
    let (_dir, db) = temp_db();
    let blockstore = Arc::new(BlockStore::new(db).unwrap());
    let early = sample_block(1);
    let kept = sample_block(8);
    store_body(&blockstore, &early, 0);
    store_body(&blockstore, &kept, 8);
    let config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 8,
            min_recent_blocks: 1000,
        },
        ..PruningConfig::default()
    };
    let manager = PruningManager::new(config, Arc::clone(&blockstore));
    manager.prune_to_height(10, 300, false).unwrap();
    assert!(
        blockstore
            .get_block(&header_hash(&early.header))
            .unwrap()
            .is_none()
    );
    assert!(
        blockstore
            .get_block(&header_hash(&kept.header))
            .unwrap()
            .is_some()
    );
}

#[test]
fn aggressive_prune_runs() {
    let (_dir, db) = temp_db();
    let blockstore = Arc::new(BlockStore::new(db).unwrap());
    let config = PruningConfig {
        mode: PruningMode::Aggressive {
            keep_from_height: 0,
            keep_commitments: false,
            keep_filtered_blocks: false,
            min_blocks: 0,
        },
        ..PruningConfig::default()
    };
    let manager = PruningManager::new(config, blockstore);
    assert!(manager.prune_to_height(10, 300, false).is_ok());
}

#[test]
fn snapshot_clear_removes_every_resume_tree() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::with_backend(dir.path(), DatabaseBackend::Heed3).unwrap();
    for name in ["ibd_utxos", "ibd_utxos_ckpt_a", "ibd_utxos_ckpt_b"] {
        storage
            .open_tree(name)
            .unwrap()
            .insert(b"row", b"1")
            .unwrap();
    }
    storage.clear_ibd_snapshot_trees();
    for name in ["ibd_utxos", "ibd_utxos_ckpt_a", "ibd_utxos_ckpt_b"] {
        assert!(
            storage.open_tree(name).unwrap().is_empty().unwrap(),
            "{name}"
        );
    }
    assert!(storage.open_engine_resume_snapshot().unwrap().is_none());
}

#[test]
fn tip_update_stores_the_header_with_the_index() {
    let (_dir, db) = temp_db();
    let chain = ChainState::new(db).unwrap();
    let block = sample_block(9);
    let hash = header_hash(&block.header);
    chain.update_tip(&hash, &block.header, 0).unwrap();
    let info = chain.load_chain_info().unwrap().unwrap();
    assert_eq!(info.tip_hash, hash);
    assert_eq!(info.tip_header.nonce, 9);
    assert!(chain.block_index().get(&hash).unwrap().is_some());
    chain.mark_invalid(&hash).unwrap();
    assert!(chain.is_invalid(&hash).unwrap());
    chain.reconsider(&hash).unwrap();
    assert!(!chain.is_invalid(&hash).unwrap());
    assert_eq!(
        chain.block_index().get(&hash).unwrap().unwrap().status,
        super::block_index::BlockIndexStatus::Valid
    );
}

fn fdatasync_total(mode: &str) -> i32 {
    let so = std::env::var("COUNT_SYNC_SO")
        .unwrap_or_else(|_| "/home/josh/tmp/count-sync.so".to_string());
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("storage::pass_fix_tests::update_tip_syncs_once")
        .arg("--exact")
        .arg("--test-threads=1")
        .env("F2_SYNC_MODE", mode)
        .env("LD_PRELOAD", &so)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = format!("{stdout}{stderr}");
    assert!(
        output.status.success(),
        "sync child {mode} failed\n{text}"
    );
    assert!(
        text.contains("update_tip_syncs_once"),
        "sync child {mode} did not run the test\n{text}"
    );
    let line = text
        .lines()
        .rev()
        .find(|line| line.contains("SYNC_TOTAL fdatasync="))
        .unwrap_or_else(|| panic!("no sync total\n{stderr}"));
    line.split("fdatasync=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn update_tip_syncs_once() {
    if let Ok(mode) = std::env::var("F2_SYNC_MODE") {
        let (_dir, db) = temp_db();
        let chain = ChainState::new(db).unwrap();
        if mode == "tip" {
            let block = sample_block(9);
            let hash = header_hash(&block.header);
            chain.update_tip(&hash, &block.header, 0).unwrap();
        }
        return;
    }
    let open = fdatasync_total("open");
    let both = fdatasync_total("tip");
    assert_eq!(both - open, 1, "open={open} tip={both}");
}
