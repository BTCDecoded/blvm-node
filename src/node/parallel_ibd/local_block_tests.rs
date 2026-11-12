use super::*;
use crate::storage::blockstore::BlockStore;
use crate::storage::database::{create_database, default_backend};
use blvm_protocol::{
    Block, BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput,
};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tempfile::TempDir;

fn temp_blockstore() -> BlockStore {
    let dir = TempDir::new().unwrap();
    let db: Arc<dyn crate::storage::database::Database> =
        Arc::from(create_database(dir.path(), default_backend(), None).unwrap());
    std::mem::forget(dir);
    BlockStore::new(db).unwrap()
}

#[test]
fn note_store_apply_counts() {
    let before = store_apply_count();
    note_store_apply(7);
    assert!(store_apply_count() > before);
}

#[test]
fn leftover_stall_try_load_hits_stored_body() {
    let blockstore = temp_blockstore();
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(1, &hash).unwrap();
    blockstore.store_block_with_witness(&block, &[], 1).unwrap();
    let loaded = leftover_stall_try_load(&blockstore, 1, ProtocolVersion::BitcoinV1)
        .unwrap()
        .expect("body on disk");
    assert_eq!(blockstore.get_block_hash(loaded.0.as_ref()), hash);
    assert!(
        leftover_stall_try_load(&blockstore, 2, ProtocolVersion::BitcoinV1)
            .unwrap()
            .is_none()
    );
}

#[test]
fn local_gap_fill_allowed_without_contiguous_body_probe() {
    assert!(height_eligible_for_local_gap_fill(338_305, 0));
    assert!(!height_eligible_for_local_gap_fill(0, 0));
}

#[test]
fn incremental_ibd_prune_window_is_zero_unless_enabled() {
    assert_eq!(incremental_ibd_prune_window(false, true, 144, 144), 0);
    assert_eq!(incremental_ibd_prune_window(true, false, 144, 144), 0);
    assert_eq!(incremental_ibd_prune_window(true, true, 0, 0), 0);
    assert_eq!(
        incremental_ibd_prune_window(true, true, 144, 50_000),
        50_000
    );
    assert_eq!(incremental_ibd_prune_window(true, true, 144, 144), 144);
}

#[test]
fn ibd_prune_horizon_and_gc_follow_the_keep_window() {
    assert_eq!(ibd_prune_horizon(961_637, 250_000, 144), 961_493);
    assert_eq!(ibd_prune_horizon(100, 250_000, 144), 249_856);
    assert_eq!(ibd_prune_horizon(961_637, 250_000, 0), 0);
    assert_eq!(ibd_prune_gc_height(287, 144, 288), 0);
    assert_eq!(ibd_prune_gc_height(288, 144, 288), 144);
    assert_eq!(ibd_prune_gc_height(50_000, 50_000, 288), 0);
    assert_eq!(ibd_prune_gc_height(50_144, 144, 288), 50_000);
}

#[test]
fn pruned_midchain_horizon_skips_below_window() {
    assert_eq!(pruned_midchain_skip_horizon(961_637, 144), 961_493);
    assert_eq!(pruned_midchain_skip_horizon(223_000, 144), 222_856);
    assert_eq!(pruned_midchain_skip_horizon(100, 144), 0);
    assert_eq!(pruned_midchain_skip_horizon(0, 144), 0);
    assert_eq!(pruned_midchain_skip_horizon(961_637, 0), 0);
}

#[test]
fn gap_persist_prune_skip_matches_flush_horizon() {
    assert!(height_below_prune_horizon(223_000, 961_493));
    assert!(!height_below_prune_horizon(961_493, 961_493));
    assert!(!height_below_prune_horizon(961_494, 961_493));
    assert!(!height_below_prune_horizon(223_000, 0));
}

fn should_skip_pruned_midchain_without_body_on_disk() {
    let blockstore = temp_blockstore();
    let missing = [0xCCu8; 32];
    // Leftover dest: header tip 961k, window 144, validation at 223k — never persist.
    assert!(should_skip_block_store_write(&blockstore, 223_000, &missing, 0, 961_493).unwrap());
    // Inclusive window start (tip − 144) must still flush.
    assert!(!should_skip_block_store_write(&blockstore, 961_493, &missing, 0, 961_493).unwrap());
    // Archive / warehouse: horizon 0 never skips on prune.
    assert!(!should_skip_block_store_write(&blockstore, 223_000, &missing, 0, 0).unwrap());
}

#[test]
fn gc_pruned_window_gap_persist_drops_body_keeps_header() {
    let blockstore = temp_blockstore();
    let height = 200u64;
    let block = Block {
        header: BlockHeader {
            version: 1,
            timestamp: 1_234_567,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(height, &hash).unwrap();
    blockstore
        .store_block_with_witness(&block, &[vec![]], height)
        .unwrap();
    assert!(blockstore.has_block_body(&hash).unwrap());
    assert!(gc_pruned_window_gap_persist(&blockstore, height).unwrap());
    assert!(!blockstore.has_block_body(&hash).unwrap());
    assert_eq!(blockstore.get_hash_by_height(height).unwrap(), Some(hash));
    assert!(!gc_pruned_window_gap_persist(&blockstore, height).unwrap());
    assert!(!gc_pruned_window_gap_persist(&blockstore, 0).unwrap());
}

/// W5/N1: wire-bytes body coexists with bincode; inject loads via one wire deser.
#[test]
fn w5_wire_bytes_persist_and_inject_coexist_with_bincode() {
    use crate::storage::blockstore::{
        decode_wire_body_blob, encode_wire_body_blob, is_wire_body_blob,
    };
    use blvm_protocol::serialization::{
        deserialize_block_with_witnesses, serialize_block_with_witnesses,
    };

    let blockstore = temp_blockstore();
    let height = 500u64;
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let witnesses: Vec<Vec<Witness>> = vec![vec![]];
    // Unit uses serialize_block_with_witnesses as a stand-in payload; production stores
    // the original P2P frame bytes (lossy re-encode is explicitly avoided on the hot path).
    let payload = serialize_block_with_witnesses(&block, &witnesses, true);
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(height, &hash).unwrap();
    blockstore
        .store_block_wire_bytes(&block, height, &payload)
        .unwrap();

    let blob = blockstore
        .get_block_and_witnesses(&hash)
        .unwrap()
        .expect("wire body present");
    assert_eq!(blockstore.get_block_hash(&blob.0), hash);
    assert!(is_wire_body_blob(&encode_wire_body_blob(&payload)));
    assert_eq!(
        decode_wire_body_blob(&encode_wire_body_blob(&payload)).unwrap(),
        payload.as_slice()
    );

    let loaded = try_load_local_ibd_block(&blockstore, height, hash, ProtocolVersion::BitcoinV1)
        .unwrap()
        .expect("inject load");
    assert_eq!(blockstore.get_block_hash(&loaded.0), hash);

    // Legacy bincode at another height still works (dual-format).
    let h2 = 501u64;
    let hash2 = {
        let mut b2 = block.clone();
        b2.header.nonce = 99;
        let h = blockstore.get_block_hash(&b2);
        blockstore.store_height(h2, &h).unwrap();
        blockstore
            .store_block_with_witness(&b2, &witnesses, h2)
            .unwrap();
        h
    };
    let legacy = try_load_local_ibd_block(&blockstore, h2, hash2, ProtocolVersion::BitcoinV1)
        .unwrap()
        .expect("bincode inject");
    assert_eq!(blockstore.get_block_hash(&legacy.0), hash2);

    // Micro: bincode ser+de×2 vs memcpy+wire deser (same payload).
    let t0 = std::time::Instant::now();
    for _ in 0..200 {
        let body = bincode::serialize(&block).unwrap();
        let wit = bincode::serialize(&witnesses).unwrap();
        let _: Block = bincode::deserialize(&body).unwrap();
        let _: Vec<Vec<Witness>> = bincode::deserialize(&wit).unwrap();
    }
    let bincode_ns = t0.elapsed().as_nanos() / 200;
    let t1 = std::time::Instant::now();
    for _ in 0..200 {
        let tagged = encode_wire_body_blob(&payload);
        let p = decode_wire_body_blob(&tagged).unwrap();
        let _ = deserialize_block_with_witnesses(p).unwrap();
    }
    let wire_ns = t1.elapsed().as_nanos() / 200;
    eprintln!(
        "[W5 micro] bincode ser+de×2 ≈{bincode_ns} ns/op; wire memcpy+deser ≈{wire_ns} ns/op"
    );
    let _ = (bincode_ns, wire_ns);
}

#[test]
fn sparse_body_probe_finds_max_without_height_one() {
    let blockstore = temp_blockstore();
    // Contiguous height index (headers synced through tip) but no body at h=1 — mirrors
    // resume-after-checkpoint where `highest_stored_height` must see the tip, not height 0.
    let height = 500u64;
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    let placeholder = [0xAAu8; 32];
    for h in 0..height {
        blockstore.store_height(h, &placeholder).unwrap();
    }
    blockstore
        .store_block_with_witness(&block, &[], height)
        .unwrap();
    blockstore.store_height(height, &hash).unwrap();
    assert_eq!(probe_confirmed_body_height(&blockstore).unwrap(), 0);
    assert_eq!(
        probe_highest_stored_body_height(&blockstore).unwrap(),
        height
    );
    assert!(should_skip_block_store_write(&blockstore, height, &hash, 0, 0).unwrap());
    assert!(!should_skip_block_store_write(&blockstore, height + 1, &[0xBBu8; 32], 0, 0).unwrap());
}

#[test]
fn contiguous_body_probe_unchanged_when_height_one_present() {
    let blockstore = temp_blockstore();
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    blockstore.store_block_with_witness(&block, &[], 1).unwrap();
    blockstore.store_height(0, &[0u8; 32]).unwrap();
    blockstore
        .store_height(1, &blockstore.get_block_hash(&block))
        .unwrap();
    assert_eq!(probe_confirmed_body_height(&blockstore).unwrap(), 1);
    assert_eq!(probe_highest_stored_body_height(&blockstore).unwrap(), 1);
}

#[test]
fn cached_feature_registry_is_process_latched() {
    let a = cached_feature_registry(ProtocolVersion::BitcoinV1);
    let b = cached_feature_registry(ProtocolVersion::BitcoinV1);
    assert!(std::ptr::eq(a, b), "same protocol must reuse one registry");
    assert!(a.is_feature_active("segwit", 500_000, 1_600_000_000));
    let r = cached_feature_registry(ProtocolVersion::Regtest);
    assert!(
        !std::ptr::eq(a, r),
        "distinct protocols get distinct caches"
    );
}

#[test]
fn stall_abort_policy() {
    use super::super::memory::{self, PressureLevel};
    let prev = std::env::var("BLVM_IBD_STALL_ABORT_GAP_FETCH").ok();
    unsafe { std::env::remove_var("BLVM_IBD_STALL_ABORT_GAP_FETCH") };
    memory::publish_ibd_pressure(PressureLevel::None);
    assert!(!ibd_stall_aborts_inflight_gap_fetch(true, 0, 265_553));
    // A6i: WAN tip crawl must NOT abort on coord stall (soft-retry + tip SLA rotate).
    assert!(!ibd_stall_aborts_inflight_gap_fetch(true, 640_000, 640_001));
    assert!(!ibd_stall_aborts_inflight_gap_fetch(true, 640_000, 640_000));
    assert!(ibd_stall_aborts_inflight_gap_fetch(false, 0, 265_553));
    memory::publish_ibd_pressure(PressureLevel::Critical);
    assert!(ibd_stall_aborts_inflight_gap_fetch(true, 0, 265_553));
    memory::publish_ibd_pressure(PressureLevel::None);
    unsafe { std::env::set_var("BLVM_IBD_STALL_ABORT_GAP_FETCH", "1") };
    assert!(ibd_stall_aborts_inflight_gap_fetch(true, 0, 265_553));
    if let Some(v) = prev {
        unsafe { std::env::set_var("BLVM_IBD_STALL_ABORT_GAP_FETCH", v) };
    } else {
        unsafe { std::env::remove_var("BLVM_IBD_STALL_ABORT_GAP_FETCH") };
    }
    memory::publish_ibd_pressure(PressureLevel::None);
}

#[test]
fn local_block_miss_display() {
    let m = LocalBlockMiss::WitnessEmptyStale;
    assert!(m.to_string().contains("empty"));
}

#[test]
fn contiguous_body_tip_from_stops_at_first_hole() {
    let blockstore = temp_blockstore();
    for h in [1u64, 2, 3, 5] {
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000 + h,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = blockstore.get_block_hash(&block);
        blockstore.store_height(h, &hash).unwrap();
        blockstore.store_block_with_witness(&block, &[], h).unwrap();
    }
    assert_eq!(
        contiguous_body_tip_from(&blockstore, 1).unwrap(),
        3,
        "must stop before leftover cheese at 5"
    );
    assert_eq!(
        contiguous_body_tip_from(&blockstore, 5).unwrap(),
        5,
        "walk from a later island stays on that island"
    );
}

#[test]
fn contiguous_body_range_tip_does_not_require_genesis() {
    let blockstore = temp_blockstore();
    for h in [180_001u64, 180_002, 180_003, 180_005] {
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000 + h,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = blockstore.get_block_hash(&block);
        blockstore.store_height(h, &hash).unwrap();
        blockstore.store_block_with_witness(&block, &[], h).unwrap();
    }
    assert_eq!(first_stored_body_height(&blockstore).unwrap(), 180_001);
    assert_eq!(
        contiguous_body_range_tip(&blockstore).unwrap(),
        180_003,
        "range tip is the first island, not 0 and not leftover cheese"
    );
    assert_eq!(
        contiguous_body_tip_from(&blockstore, 1).unwrap(),
        0,
        "walk-from-1 still misses a slice that does not start at genesis"
    );
}

#[test]
fn extend_contiguous_body_tip_stops_at_hole() {
    let blockstore = temp_blockstore();
    for h in [100u64, 101, 102, 104] {
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000 + h,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = blockstore.get_block_hash(&block);
        blockstore.store_height(h, &hash).unwrap();
        blockstore.store_block_with_witness(&block, &[], h).unwrap();
    }
    assert_eq!(
        extend_contiguous_body_tip(&blockstore, 100, 256).unwrap(),
        102,
        "must stop before hole at 103"
    );
    assert_eq!(
        extend_contiguous_body_tip(&blockstore, 102, 256).unwrap(),
        102,
        "no further contiguous from hole edge"
    );
}

fn store_body_at(blockstore: &BlockStore, h: u64) {
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000 + h,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(h, &hash).unwrap();
    blockstore.store_block_with_witness(&block, &[], h).unwrap();
}

#[test]
fn available_body_bumps_without_disk_or_flush_commit() {
    reset_flush_path_body_tip_for_test();
    note_available_body(1);
    assert_eq!(flush_path_body_tip(), 1, "feeder-take / insert of 1 bumps");
    note_available_body(2);
    assert_eq!(flush_path_body_tip(), 2, "sequential availability bumps");
    note_available_body(4);
    assert_eq!(flush_path_body_tip(), 2, "hole at 3 must not jump");
    note_available_body(3);
    assert_eq!(
        flush_path_body_tip(),
        3,
        "fill 3; 4 is not auto-caught without disk"
    );
    reset_flush_path_body_tip_for_test();
}

#[test]
fn flush_path_tip_advances_on_write_not_sparse_jump() {
    reset_flush_path_body_tip_for_test();
    let blockstore = temp_blockstore();
    for h in [3u64, 5] {
        store_body_at(&blockstore, h);
        note_flush_path_bodies(&blockstore, &[h]);
    }
    assert_eq!(
        flush_path_body_tip(),
        0,
        "out-of-order persist must not jump a hole at 1"
    );
    store_body_at(&blockstore, 1);
    note_flush_path_bodies(&blockstore, &[1]);
    assert_eq!(flush_path_body_tip(), 1, "tip+1 persist bumps once");
    store_body_at(&blockstore, 2);
    note_flush_path_bodies(&blockstore, &[2]);
    assert_eq!(
        flush_path_body_tip(),
        3,
        "tip+1 persist catch-up walks already-on-disk 3"
    );
    store_body_at(&blockstore, 4);
    note_flush_path_bodies(&blockstore, &[4]);
    assert_eq!(flush_path_body_tip(), 5, "catch-up reaches 5 after 4 lands");
    reset_flush_path_body_tip_for_test();
}

#[test]
fn flush_path_tip_batch_1_to_50_holds() {
    reset_flush_path_body_tip_for_test();
    let blockstore = temp_blockstore();
    let heights: Vec<u64> = (1..=50).collect();
    for &h in &heights {
        store_body_at(&blockstore, h);
    }
    note_flush_path_bodies(&blockstore, &heights);
    assert_eq!(
        flush_path_body_tip(),
        50,
        "sorted flush batch 1..=50 must publish tip=50"
    );
    reset_flush_path_body_tip_for_test();
}

#[test]
fn persist_gap_block_enables_local_inject() {
    let blockstore = temp_blockstore();
    let vh = Arc::new(AtomicU64::new(499));
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(500, &hash).unwrap();
    assert!(!blockstore.has_block_body(&hash).unwrap());

    let persisted = try_persist_gap_block_for_local_inject(
        &blockstore,
        Some(&vh),
        500,
        hash,
        &block,
        &[],
        ProtocolVersion::BitcoinV1,
    )
    .unwrap();
    assert!(persisted);
    assert!(blockstore.has_block_body(&hash).unwrap());

    let mut reorder_buffer = std::collections::BTreeMap::new();
    let already_dispatched = rustc_hash::FxHashSet::default();
    let mut log_miss = rustc_hash::FxHashSet::default();
    assert!(
        coordinator_inject_local_gap(
            &blockstore,
            ProtocolVersion::BitcoinV1,
            500,
            0,
            499,
            &mut reorder_buffer,
            &already_dispatched,
            &mut log_miss,
            false,
        )
        .unwrap()
    );
    assert!(reorder_buffer.contains_key(&500));
}

fn persist_toy_at(blockstore: &BlockStore, vh: &Arc<AtomicU64>, h: u64) -> bool {
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000 + h,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(h, &hash).unwrap();
    try_persist_gap_block_for_local_inject(
        blockstore,
        Some(vh),
        h,
        hash,
        &block,
        &[],
        ProtocolVersion::BitcoinV1,
    )
    .unwrap()
}

fn toy_block_at(h: u64) -> Block {
    Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000 + h,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000 + h as i64,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    }
}

/// R-348: the persist-lane batch write lands every body in one txn and each reads
/// back through the same path the coordinator's local inject uses; the gate reports
/// `OnDisk` afterwards (no second write).
#[test]
fn r348_batch_store_round_trips_and_gate_sees_on_disk() {
    let bs = temp_blockstore();
    let blocks: Vec<Block> = (100u64..132).map(toy_block_at).collect();
    for (i, b) in blocks.iter().enumerate() {
        bs.store_height(100 + i as u64, &bs.get_block_hash(b))
            .unwrap();
    }
    let empty: Vec<Vec<blvm_protocol::segwit::Witness>> = Vec::new();
    let items: Vec<(&Block, &[Vec<blvm_protocol::segwit::Witness>], u64)> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b, empty.as_slice(), 100 + i as u64))
        .collect();
    bs.store_blocks_with_witness_batch(&items).unwrap();
    let vh = Arc::new(AtomicU64::new(99));
    for (i, b) in blocks.iter().enumerate() {
        let h = 100 + i as u64;
        let hash = bs.get_block_hash(b);
        assert!(bs.has_block_body(&hash).unwrap(), "body {h} on disk");
        let (got, _w) =
            try_load_local_ibd_block_with_reason(&bs, h, hash, ProtocolVersion::BitcoinV1)
                .unwrap()
                .unwrap_or_else(|m| panic!("load {h}: {m:?}"));
        assert_eq!(got.header.timestamp, b.header.timestamp);
        let gate =
            gap_persist_gate(&bs, Some(&vh), h, hash, b, &[], ProtocolVersion::BitcoinV1).unwrap();
        assert!(
            matches!(gate, GapPersistGate::OnDisk(_)),
            "gate after batch: {gate:?}"
        );
    }
    // A fresh height still asks to be written.
    let nb = toy_block_at(200);
    let nh = bs.get_block_hash(&nb);
    bs.store_height(200, &nh).unwrap();
    assert_eq!(
        gap_persist_gate(
            &bs,
            Some(&vh),
            200,
            nh,
            &nb,
            &[],
            ProtocolVersion::BitcoinV1
        )
        .unwrap(),
        GapPersistGate::Write
    );
}

fn persist_wrote_body(blockstore: &BlockStore, h: u64) -> bool {
    blockstore
        .get_hash_by_height(h)
        .ok()
        .flatten()
        .and_then(|hash| blockstore.has_block_body(&hash).ok())
        .unwrap_or(false)
}

#[test]
fn r196_persist_ahead_prefix() {
    // Persist writes disk. It must not publish FLUSH_PATH_TIP (R-196 leftover).
    reset_flush_path_body_tip_for_test();
    let ahead = temp_blockstore();
    let vh0 = Arc::new(AtomicU64::new(0));
    for h in 1u64..=5 {
        assert!(
            persist_toy_at(&ahead, &vh0, h),
            "persist {h} while val=0 must write"
        );
        assert!(persist_wrote_body(&ahead, h), "body {h} on disk");
    }
    assert_eq!(
        flush_path_body_tip(),
        0,
        "GAP_PERSIST must not publish leftover warehouse tip"
    );

    reset_flush_path_body_tip_for_test();
    let late = temp_blockstore();
    let vh600 = Arc::new(AtomicU64::new(600));
    assert!(
        persist_toy_at(&late, &vh600, 1),
        "late tip+1 must persist after apply passed"
    );
    assert!(persist_wrote_body(&late, 1));
    assert_eq!(flush_path_body_tip(), 0);
    assert!(
        persist_toy_at(&late, &vh600, 2),
        "sequential glue continues after apply passed"
    );
    assert!(persist_wrote_body(&late, 2));
    assert_eq!(flush_path_body_tip(), 0);

    reset_flush_path_body_tip_for_test();
    let farm = temp_blockstore();
    let vh10 = Arc::new(AtomicU64::new(10));
    assert!(
        persist_toy_at(&farm, &vh10, 2000),
        "farm tile outside lookahead must persist"
    );
    assert!(persist_wrote_body(&farm, 2000));
    assert_eq!(flush_path_body_tip(), 0);
    reset_flush_path_body_tip_for_test();
}

#[test]
fn w1_persist_skips_empty_witness_when_commitment_present() {
    // Post-segwit + BIP141 commitment + empty witnesses = stripped MSG_BLOCK → skip.
    let blockstore = temp_blockstore();
    let vh = Arc::new(AtomicU64::new(500_000));
    let height = 500_001u64;
    let mut commitment_script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
    commitment_script.extend_from_slice(&[0u8; 32]);
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![TransactionInput {
                prevout: OutPoint {
                    hash: [0u8; 32],
                    index: 0xffff_ffff,
                },
                script_sig: vec![0x01].into(),
                sequence: 0xffff_ffff,
            }],
            outputs: blvm_protocol::tx_outputs![
                TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                },
                TransactionOutput {
                    value: 0,
                    script_pubkey: commitment_script.into(),
                }
            ],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(height, &hash).unwrap();
    let persisted = try_persist_gap_block_for_local_inject(
        &blockstore,
        Some(&vh),
        height,
        hash,
        &block,
        &[], // empty witness
        ProtocolVersion::BitcoinV1,
    )
    .unwrap();
    assert!(
        !persisted,
        "W1 must refuse empty-witness persist when commitment present"
    );
    assert!(
        !blockstore.has_block_body(&hash).unwrap(),
        "body must not be written"
    );
}

#[test]
fn w1_persist_allows_empty_witness_without_commitment() {
    // Live h=640022: coinbase-only, no BIP141 commitment — empty stacks are valid.
    let blockstore = temp_blockstore();
    let vh = Arc::new(AtomicU64::new(500_000));
    let height = 500_001u64;
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(height, &hash).unwrap();
    let persisted = try_persist_gap_block_for_local_inject(
        &blockstore,
        Some(&vh),
        height,
        hash,
        &block,
        &[], // empty witness
        ProtocolVersion::BitcoinV1,
    )
    .unwrap();
    assert!(persisted, "empty witness without commitment must persist");
}

/// Wire GAP_PERSIST must store witness row when real stacks are provided (no download pre-repair).
#[test]
fn wire_persist_stores_witness_blob_for_commitment_block() {
    use blvm_protocol::serialization::serialize_block_with_witnesses;

    let prev_wire = std::env::var("BLVM_IBD_WIRE_BYTES_STORE").ok();
    unsafe { std::env::set_var("BLVM_IBD_WIRE_BYTES_STORE", "1") };

    let blockstore = temp_blockstore();
    let vh = Arc::new(AtomicU64::new(500_000));
    let height = 500_001u64;
    let mut commitment_script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
    commitment_script.extend_from_slice(&[0u8; 32]);
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![TransactionInput {
                prevout: OutPoint {
                    hash: [0u8; 32],
                    index: 0xffff_ffff,
                },
                script_sig: vec![0x01].into(),
                sequence: 0xffff_ffff,
            }],
            outputs: blvm_protocol::tx_outputs![
                TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                },
                TransactionOutput {
                    value: 0,
                    script_pubkey: commitment_script.into(),
                }
            ],
            lock_time: 0,
        }]
        .into(),
    };
    let witnesses: Vec<Vec<Witness>> = vec![vec![vec![vec![0x01u8, 0x02, 0x03]]]];
    let payload = serialize_block_with_witnesses(&block, &witnesses, true);
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(height, &hash).unwrap();
    let persisted = try_persist_gap_block_for_local_inject_with_wire(
        &blockstore,
        Some(&vh),
        height,
        hash,
        &block,
        &witnesses,
        ProtocolVersion::BitcoinV1,
        Some(payload.as_slice()),
    )
    .unwrap();
    assert!(persisted, "wire persist with real witnesses must succeed");
    assert!(
        blockstore.has_witness_blob(&hash).unwrap(),
        "wire path must write witness row"
    );

    unsafe {
        match prev_wire {
            Some(v) => std::env::set_var("BLVM_IBD_WIRE_BYTES_STORE", v),
            None => std::env::remove_var("BLVM_IBD_WIRE_BYTES_STORE"),
        }
    }
}

#[test]
fn local_load_synthesizes_empty_witness_without_commitment() {
    let blockstore = temp_blockstore();
    let height = 500_001u64;
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(height, &hash).unwrap();
    blockstore
        .store_block_with_witness(&block, &[], height)
        .unwrap();
    let loaded =
        try_load_local_ibd_block(&blockstore, height, hash, ProtocolVersion::BitcoinV1).unwrap();
    assert!(
        loaded.is_some(),
        "body without commitment must load with empty witnesses"
    );
    assert!(
        !is_local_witness_hole(&blockstore, height, hash, ProtocolVersion::BitcoinV1).unwrap(),
        "no commitment → not a witness hole"
    );
}

#[test]
fn is_local_witness_hole_detects_body_without_witness() {
    let blockstore = temp_blockstore();
    let height = 500_001u64;
    let mut commitment_script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
    commitment_script.extend_from_slice(&[0u8; 32]);
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![TransactionInput {
                prevout: OutPoint {
                    hash: [0u8; 32],
                    index: 0xffff_ffff,
                },
                script_sig: vec![0x01].into(),
                sequence: 0xffff_ffff,
            }],
            outputs: blvm_protocol::tx_outputs![
                TransactionOutput {
                    value: 50_0000_0000,
                    script_pubkey: vec![0x51],
                },
                TransactionOutput {
                    value: 0,
                    script_pubkey: commitment_script.into(),
                }
            ],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(height, &hash).unwrap();
    // Store body without witness (simulates prior MSG_BLOCK / partial persist).
    blockstore
        .store_block_with_witness(&block, &[], height)
        .unwrap();
    assert!(is_local_witness_hole(&blockstore, height, hash, ProtocolVersion::BitcoinV1).unwrap());
}

#[test]
fn inject_past_start_confirmed_body_cap_when_uncapped() {
    // Live stall: confirmed_bodies=649265 at start; GAP_PERSIST wrote past that; inject
    // refused heights > confirmed. Default max is uncapped so disk-complete gaps inject.
    // Use a pre-segwit height so empty witness is valid (post-segwit needs real stacks).
    let blockstore = temp_blockstore();
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_400_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(500, &hash).unwrap();
    blockstore
        .store_block_with_witness(&block, &[], 500)
        .unwrap();

    let mut reorder_buffer = std::collections::BTreeMap::new();
    let already_dispatched = rustc_hash::FxHashSet::default();
    let mut log_miss = rustc_hash::FxHashSet::default();
    assert!(
        coordinator_inject_local_gap(
            &blockstore,
            ProtocolVersion::BitcoinV1,
            500,
            400, // start-time confirmed — must not block inject at h=500
            499,
            &mut reorder_buffer,
            &already_dispatched,
            &mut log_miss,
            false,
        )
        .unwrap()
    );
    assert!(reorder_buffer.contains_key(&500));
}

#[test]
fn inject_skips_already_dispatched_heights() {
    let blockstore = temp_blockstore();
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(500, &hash).unwrap();
    blockstore
        .store_block_with_witness(&block, &[], 500)
        .unwrap();

    let mut reorder_buffer = std::collections::BTreeMap::new();
    let mut already_dispatched = rustc_hash::FxHashSet::default();
    already_dispatched.insert(500);
    let mut log_miss = rustc_hash::FxHashSet::default();
    // Reports success (in-pipeline) but must NOT put a block in the reorder buffer.
    // Do not assert on ARC_BLOCK_CREATED — that global is shared across parallel tests.
    assert!(
        coordinator_inject_local_gap(
            &blockstore,
            ProtocolVersion::BitcoinV1,
            500,
            0,
            499,
            &mut reorder_buffer,
            &already_dispatched,
            &mut log_miss,
            true,
        )
        .unwrap()
    );
    assert!(
        reorder_buffer.is_empty(),
        "already-dispatched height must not be re-loaded into reorder_buffer"
    );
}

#[test]
fn inject_chain_skips_counting_already_present_heights() {
    // Tip in reorder + ahead height already_dispatched must not look like a fresh chain
    // (regression: Case D logged IBD_INJECT_CHAIN every loop for already-present spans).
    let blockstore = temp_blockstore();
    let mk = |ts| Block {
        header: BlockHeader {
            version: 4,
            timestamp: ts,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let b500 = mk(1_600_000_000);
    let h500 = blockstore.get_block_hash(&b500);
    blockstore.store_height(500, &h500).unwrap();
    blockstore
        .store_block_with_witness(&b500, &[], 500)
        .unwrap();
    let b501 = mk(1_600_000_001);
    let h501 = blockstore.get_block_hash(&b501);
    blockstore.store_height(501, &h501).unwrap();
    blockstore
        .store_block_with_witness(&b501, &[], 501)
        .unwrap();

    let mut reorder_buffer = std::collections::BTreeMap::new();
    reorder_buffer.insert(500, (Arc::new(b500), Arc::new(Vec::new())));
    let mut already_dispatched = rustc_hash::FxHashSet::default();
    already_dispatched.insert(500);
    already_dispatched.insert(501);
    let mut log_miss = rustc_hash::FxHashSet::default();
    assert!(
        coordinator_inject_local_gap(
            &blockstore,
            ProtocolVersion::BitcoinV1,
            500,
            0,
            499,
            &mut reorder_buffer,
            &already_dispatched,
            &mut log_miss,
            true,
        )
        .unwrap()
    );
    assert_eq!(
        reorder_buffer.len(),
        1,
        "already-present chain must not grow reorder_buffer"
    );
    assert!(reorder_buffer.contains_key(&500));
    assert!(!reorder_buffer.contains_key(&501));
}

#[test]
fn inject_tip_not_in_pipeline_reloads_from_disk() {
    // W24: tip marked dispatched but not confirmed in bridge/feeder must reload
    // from disk instead of chaining over a missing tip.
    let blockstore = temp_blockstore();
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    blockstore.store_height(500, &hash).unwrap();
    blockstore
        .store_block_with_witness(&block, &[], 500)
        .unwrap();

    let mut reorder_buffer = std::collections::BTreeMap::new();
    let mut already_dispatched = rustc_hash::FxHashSet::default();
    already_dispatched.insert(500);
    let mut log_miss = rustc_hash::FxHashSet::default();
    assert!(
        coordinator_inject_local_gap(
            &blockstore,
            ProtocolVersion::BitcoinV1,
            500,
            0,
            499,
            &mut reorder_buffer,
            &already_dispatched,
            &mut log_miss,
            false, // tip not confirmed in pipeline
        )
        .unwrap()
    );
    assert!(
        reorder_buffer.contains_key(&500),
        "unconfirmed dispatched tip must reload from disk"
    );
}

#[test]
fn filled_gap_miss_leaves_the_log_and_the_cursor_drops_old_keys() {
    let blockstore = temp_blockstore();
    let mut reorder_buffer = std::collections::BTreeMap::new();
    let already_dispatched = rustc_hash::FxHashSet::default();
    let mut log_miss = rustc_hash::FxHashSet::default();
    log_miss.insert(100);
    assert!(
        !coordinator_inject_local_gap(
            &blockstore,
            ProtocolVersion::BitcoinV1,
            500,
            0,
            499,
            &mut reorder_buffer,
            &already_dispatched,
            &mut log_miss,
            true,
        )
        .unwrap()
    );
    assert!(
        log_miss.contains(&500),
        "a live miss inside the lookahead stays"
    );
    assert!(!log_miss.contains(&100), "a key below the cursor leaves");

    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    reorder_buffer.insert(500, (Arc::new(block), Arc::new(Vec::new())));
    log_miss.insert(500);
    assert!(
        coordinator_inject_local_gap(
            &blockstore,
            ProtocolVersion::BitcoinV1,
            500,
            0,
            499,
            &mut reorder_buffer,
            &already_dispatched,
            &mut log_miss,
            true,
        )
        .unwrap()
    );
    assert!(
        !log_miss.contains(&500),
        "a queued height leaves the miss log"
    );

    reorder_buffer.clear();
    let mut dispatched = rustc_hash::FxHashSet::default();
    dispatched.insert(500);
    log_miss.insert(500);
    assert!(
        coordinator_inject_local_gap(
            &blockstore,
            ProtocolVersion::BitcoinV1,
            500,
            0,
            499,
            &mut reorder_buffer,
            &dispatched,
            &mut log_miss,
            true,
        )
        .unwrap()
    );
    assert!(
        !log_miss.contains(&500),
        "a dispatched height leaves the miss log"
    );
    assert!(reorder_buffer.is_empty());
}

#[test]
fn witness_repair_stores_row_key_when_body_exists() {
    let blockstore = temp_blockstore();
    let height = 500_000u64;
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let hash = blockstore.get_block_hash(&block);
    // Height index is required: store_witness_at_height writes the row key
    // (height||hash); has_witness_blob / get_witness resolve via height_index.
    blockstore.store_height(height, &hash).unwrap();
    blockstore
        .store_block_with_witness(&block, &[], height)
        .unwrap();
    assert!(!blockstore.has_witness_blob(&hash).unwrap());

    let witnesses: Vec<Vec<Witness>> = vec![vec![vec![vec![0x51u8]]]];
    let repaired = try_repair_missing_witness(
        &blockstore,
        height,
        hash,
        witnesses.as_slice(),
        ProtocolVersion::BitcoinV1,
        Some(&block),
    )
    .unwrap();
    assert!(repaired);
    assert!(blockstore.has_witness_blob(&hash).unwrap());
    let loaded = blockstore.get_witness(&hash).unwrap().unwrap();
    assert!(has_real_witnesses(&loaded));
}
