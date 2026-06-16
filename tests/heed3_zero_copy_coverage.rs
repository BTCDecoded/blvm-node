//! Integration coverage for heed3 zero-copy IBD hot paths:
//! `IbdUtxoStore` disk supplement (→ `load_keys_from_disk`) and flush/reload roundtrip.
//!
//! Requires `heed3` + `production`. Run:
//! `cargo test --test heed3_zero_copy_coverage --no-default-features --features heed3,production,redb`

#![cfg(all(feature = "heed3", feature = "production"))]

use blvm_node::storage::database::{create_database, Database, DatabaseBackend, Tree};
use blvm_node::storage::disk_utxo::{outpoint_to_key, OutPointKey};
use blvm_node::storage::ibd_utxo_store::{EvictionStrategy, IbdUtxoStore};
use blvm_node::storage::utxo_value_codec::{encode_utxo_with_codec, ValueCodec};
use blvm_node::storage::Storage;
use blvm_node::{OutPoint, UTXO};
use blvm_protocol::block::UtxoDelta;
use blvm_protocol::types::UtxoSet;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use tempfile::TempDir;

fn seed_heed3_disk_tree(count: usize) -> (TempDir, Arc<dyn Tree>, Vec<OutPointKey>, Vec<OutPoint>) {
    let temp_dir = TempDir::new().unwrap();
    let db: Arc<dyn Database> =
        Arc::from(create_database(temp_dir.path(), DatabaseBackend::Heed3, None).unwrap());
    let tree: Arc<dyn Tree> = Arc::from(db.open_tree("ibd_utxos").unwrap());

    let mut keys = Vec::with_capacity(count);
    let mut outpoints = Vec::with_capacity(count);
    for i in 0..count {
        let op = OutPoint {
            hash: [i as u8; 32],
            index: i as u32,
        };
        let key = outpoint_to_key(&op);
        let utxo = UTXO {
            value: (i as i64 + 1) * 1000,
            script_pubkey: vec![0x76, i as u8].into(),
            height: i as u64,
            is_coinbase: i % 50 == 0,
        };
        tree.insert(
            &key,
            &encode_utxo_with_codec(ValueCodec::Rkyv, &utxo).unwrap(),
        )
        .unwrap();
        keys.push(key);
        outpoints.push(op);
    }
    (temp_dir, tree, keys, outpoints)
}

#[test]
fn ibd_utxo_store_supplement_loads_from_heed3_disk_zero_copy() {
    const N: usize = 40;
    let (_dir, disk, keys, outpoints) = seed_heed3_disk_tree(N);

    let store = IbdUtxoStore::new_with_options(
        Arc::clone(&disk),
        256,
        false,
        4, // tiny cache → force disk loads
        EvictionStrategy::Lifo,
        0,
        ValueCodec::Rkyv,
    );

    let mut map = UtxoSet::default();
    let mut miss_buf = Vec::new();
    store.supplement_utxo_map_with_buf(&mut map, &keys, &mut miss_buf);

    assert_eq!(map.len(), N, "every seeded UTXO must load from heed3 disk");
    for (i, op) in outpoints.iter().enumerate() {
        let got = map.get(op).expect("missing outpoint").as_ref();
        assert_eq!(got.value, ((i as i64 + 1) * 1000));
        assert_eq!(got.height, i as u64);
    }

    let (disk_loads, cache_hits, _, _) = store.stats();
    assert!(
        disk_loads >= (N as u64).saturating_sub(4),
        "expected disk loads for cache misses, got disk_loads={disk_loads} cache_hits={cache_hits}"
    );
}

#[test]
fn ibd_utxo_store_flush_and_reload_heed3_rkyv() {
    let (_dir, disk, _keys, _) = seed_heed3_disk_tree(0);

    let store = Arc::new(IbdUtxoStore::new_with_options(
        Arc::clone(&disk),
        8,
        false,
        16,
        EvictionStrategy::Lifo,
        0,
        ValueCodec::Rkyv,
    ));

    let op = OutPoint {
        hash: [0xcd; 32],
        index: 99,
    };
    let utxo = UTXO {
        value: 777_000,
        script_pubkey: vec![0x51].into(),
        height: 42,
        is_coinbase: false,
    };

    let mut adds: FxHashMap<OutPoint, Arc<UTXO>> = FxHashMap::default();
    adds.insert(op, Arc::new(utxo.clone()));
    let delta = UtxoDelta {
        additions: adds,
        deletions: FxHashSet::default(),
    };

    let mut del_scratch = Vec::new();
    let mut add_scratch = Vec::new();
    store.worker_cache_put_protected(&delta.additions, 42);
    store.apply_utxo_delta(&delta, 42, &mut del_scratch, &mut add_scratch, true);

    let pkg = store
        .take_flush_batch_force()
        .expect("flush package after delta");
    let heights = Arc::clone(&pkg.heights);
    let prepared = pkg
        .prepare_for_disk(ValueCodec::Rkyv)
        .expect("prepare_for_disk");
    store
        .flush_prepared_package(&prepared, None)
        .expect("flush to heed3 disk");
    store.release_protected_heights(&heights);

    // Fresh store handle → empty cache; reload must read rkyv rows from heed3 disk.
    let store2 = IbdUtxoStore::new_with_options(
        disk,
        256,
        false,
        16,
        EvictionStrategy::Lifo,
        0,
        ValueCodec::Rkyv,
    );

    let key = outpoint_to_key(&op);
    let mut map = UtxoSet::default();
    let mut miss_buf = Vec::new();
    store2.supplement_utxo_map_with_buf(&mut map, std::slice::from_ref(&key), &mut miss_buf);

    let got = map.get(&op).expect("UTXO missing after flush").as_ref();
    assert_eq!(*got, utxo);
}

#[test]
fn utxostore_get_utxo_heed3_zero_copy_path() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::with_backend(temp_dir.path(), DatabaseBackend::Heed3).unwrap();

    let op = OutPoint {
        hash: [0x42; 32],
        index: 3,
    };
    let utxo = UTXO {
        value: 250_000,
        script_pubkey: vec![0x76, 0xa9].into(),
        height: 99,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&op, &utxo).unwrap();

    let got = storage.utxos().get_utxo(&op).unwrap().expect("UTXO");
    assert_eq!(got, utxo);

    // Reopen to ensure rkyv rows read through get_utxo zero-copy path after persist.
    drop(storage);
    let storage2 = Storage::with_backend(temp_dir.path(), DatabaseBackend::Heed3).unwrap();
    let got2 = storage2
        .utxos()
        .get_utxo(&op)
        .unwrap()
        .expect("UTXO after reopen");
    assert_eq!(got2, utxo);
}

fn outpoint_to_deletion_key(op: &OutPoint) -> [u8; 36] {
    let mut k = [0u8; 36];
    k[..32].copy_from_slice(&op.hash);
    k[32..36].copy_from_slice(&op.index.to_be_bytes());
    k
}

/// Delete flush with per-op MuHash reads rkyv rows from heed3 disk (`decode_utxo_bytes` path).
#[test]
fn ibd_flush_delete_with_muhash_decodes_heed3_disk() {
    use blvm_muhash::MuHash3072;

    // Must be set before the first `ibd_per_op_muhash_enabled()` call in this process.
    std::env::set_var("BLVM_IBD_ENABLE_PER_OP_MUHASH", "1");

    let (_dir, disk, _keys, outpoints) = seed_heed3_disk_tree(1);
    let op = outpoints[0];

    let store = Arc::new(IbdUtxoStore::new_with_options(
        Arc::clone(&disk),
        8,
        false,
        16,
        EvictionStrategy::Lifo,
        0,
        ValueCodec::Rkyv,
    ));

    let mut dels = FxHashSet::default();
    dels.insert(outpoint_to_deletion_key(&op));
    let delta = UtxoDelta {
        additions: FxHashMap::default(),
        deletions: dels,
    };

    let mut del_scratch = Vec::new();
    let mut add_scratch = Vec::new();
    store.apply_utxo_delta(&delta, 1, &mut del_scratch, &mut add_scratch, true);

    let pkg = store
        .take_flush_batch_force()
        .expect("delete flush package");
    let heights = Arc::clone(&pkg.heights);
    let prepared = pkg.prepare_for_disk(ValueCodec::Rkyv).expect("prepare");
    let mut muhash = MuHash3072::new();
    store
        .flush_prepared_package(&prepared, Some(&mut muhash))
        .expect("flush delete with MuHash");
    store.release_protected_heights(&heights);

    // Row removed from heed3 disk.
    let key = outpoint_to_key(&op);
    assert!(
        disk.get(&key).unwrap().is_none(),
        "delete flush must remove UTXO from heed3 ibd_utxos tree"
    );
}
