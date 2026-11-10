//! IbdUtxoStore retire/flush batch tests (legacy path, `BLVM_IBD_ENGINE=0`).
//! Complements `validation_loop_tests.rs` (orchestrator knobs).

use super::*;
use crate::storage::database::{BatchWriter, Tree};
use crate::storage::ibd_utxo_store::IbdUtxoStore;
use crate::storage::utxo_value_codec::ValueCodec;
use blvm_protocol::block::UtxoDelta;
use blvm_protocol::types::{OutPoint, UTXO};
use rustc_hash::FxHashMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

#[derive(Default)]
struct MemTree {
    inner: StdMutex<HashMap<Vec<u8>, Vec<u8>>>,
}

impl Tree for MemTree {
    fn insert(&self, key: &[u8], value: &[u8]) -> anyhow::Result<()> {
        self.inner
            .lock()
            .unwrap()
            .insert(key.to_vec(), value.to_vec());
        Ok(())
    }
    fn get(&self, key: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.inner.lock().unwrap().get(key).cloned())
    }
    fn get_many(&self, keys: &[&[u8]]) -> anyhow::Result<Vec<Option<Vec<u8>>>> {
        let g = self.inner.lock().unwrap();
        Ok(keys.iter().map(|k| g.get(*k).cloned()).collect())
    }
    fn remove(&self, key: &[u8]) -> anyhow::Result<()> {
        self.inner.lock().unwrap().remove(key);
        Ok(())
    }
    fn contains_key(&self, key: &[u8]) -> anyhow::Result<bool> {
        Ok(self.inner.lock().unwrap().contains_key(key))
    }
    fn clear(&self) -> anyhow::Result<()> {
        self.inner.lock().unwrap().clear();
        Ok(())
    }
    fn len(&self) -> anyhow::Result<usize> {
        Ok(self.inner.lock().unwrap().len())
    }
    fn iter(&self) -> Box<dyn Iterator<Item = anyhow::Result<(Vec<u8>, Vec<u8>)>> + '_> {
        let entries: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| Ok((k.clone(), v.clone())))
            .collect();
        Box::new(entries.into_iter())
    }
    fn batch(&self) -> anyhow::Result<Box<dyn BatchWriter + '_>> {
        Ok(Box::new(MemBatch {
            tree: self,
            ops: Vec::new(),
        }))
    }
}

struct MemBatch<'a> {
    tree: &'a MemTree,
    ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl<'a> BatchWriter for MemBatch<'a> {
    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.ops.push((key.to_vec(), Some(value.to_vec())));
    }
    fn delete(&mut self, key: &[u8]) {
        self.ops.push((key.to_vec(), None));
    }
    fn commit(self: Box<Self>) -> anyhow::Result<()> {
        let mut g = self.tree.inner.lock().unwrap();
        for (k, v) in self.ops {
            match v {
                Some(val) => {
                    g.insert(k, val);
                }
                None => {
                    g.remove(&k);
                }
            }
        }
        Ok(())
    }
    fn len(&self) -> usize {
        self.ops.len()
    }
}

fn synth_outpoint(seed: u64, idx: u32) -> OutPoint {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&seed.to_le_bytes());
    hash[8..12].copy_from_slice(&idx.to_le_bytes());
    OutPoint { hash, index: idx }
}

fn fill_del_backlog(store: &IbdUtxoStore, blocks: u64) {
    let mut alive: FxHashMap<OutPoint, UTXO> = FxHashMap::default();
    let mut del_scratch = Vec::new();
    let mut add_scratch = Vec::new();
    for h in 1..=blocks {
        let mut deletions = Vec::new();
        for op in alive.keys().take(50) {
            deletions.push(*op);
        }
        for op in &deletions {
            alive.remove(op);
        }
        let mut adds = FxHashMap::default();
        for i in 0..50 {
            let op = synth_outpoint(h, i);
            adds.insert(
                op,
                Arc::new(UTXO {
                    value: 1,
                    script_pubkey: (&[0u8; 25][..]).into(),
                    height: h,
                    is_coinbase: false,
                }),
            );
            alive.insert(
                op,
                UTXO {
                    value: 1,
                    script_pubkey: (&[0u8; 25][..]).into(),
                    height: h,
                    is_coinbase: false,
                },
            );
        }
        let delta = UtxoDelta {
            additions: adds,
            deletions: deletions
                .iter()
                .map(|op| {
                    let mut k = [0u8; 36];
                    k[..32].copy_from_slice(&op.hash);
                    k[32..36].copy_from_slice(&op.index.to_be_bytes());
                    k
                })
                .collect(),
        };
        store.apply_utxo_delta(&delta, h, &mut del_scratch, &mut add_scratch, false);
        while let Some(pkg) = store.maybe_take_flush_batch_adds_only() {
            drop(pkg);
        }
    }
}

#[serial_test::serial(ibd)]
#[test]
fn pick_flush_batch_no_force_on_del_backlog() {
    let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
    let store = IbdUtxoStore::new_with_options(
        Arc::clone(&disk),
        100,
        false,
        usize::MAX,
        crate::storage::ibd_utxo_store::EvictionStrategy::Fifo,
        0,
        ValueCodec::Bincode,
    );
    // Drain adds each block so DEL tombstones accumulate; leave the last block's ADDs
    // in add_shards so pick returns a small adds-only batch while pending stays del-heavy.
    fill_del_backlog(&store, 119);
    let mut del_scratch = Vec::new();
    let mut add_scratch = Vec::new();
    let mut adds = FxHashMap::default();
    for i in 0..50 {
        let op = synth_outpoint(120, i);
        adds.insert(
            op,
            Arc::new(UTXO {
                value: 1,
                script_pubkey: (&[0u8; 25][..]).into(),
                height: 120,
                is_coinbase: false,
            }),
        );
    }
    let delta = UtxoDelta {
        additions: adds,
        deletions: FxHashSet::default(),
    };
    store.apply_utxo_delta(&delta, 120, &mut del_scratch, &mut add_scratch, false);
    assert!(store.pending_len() > 1_000);

    let (pkg, force_durability) = ibd_retire_pick_flush_batch(&store);
    assert!(
        !force_durability,
        "del-heavy pick must not promote checkpoint; del_backlog runs at formal boundary only"
    );
    let pkg = pkg.expect("small adds batch should still flush async");
    assert!(
        pkg.ops.len() < IBD_MIN_ADDS_ONLY_BATCH,
        "E2 scenario: small adds drain with large del backlog"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn formal_checkpoint_sentinel_when_add_drain_empty() {
    use super::memory::MemoryGuard;

    let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
    let store = IbdUtxoStore::new_with_options(
        Arc::clone(&disk),
        100,
        false,
        usize::MAX,
        crate::storage::ibd_utxo_store::EvictionStrategy::Fifo,
        0,
        ValueCodec::Bincode,
    );
    fill_del_backlog(&store, 120);
    assert!(store.pending_len() > 1_000);
    assert!(store.take_flush_batch_adds_only().is_none());

    let mut mem = MemoryGuard::new();
    let max_ahead = Arc::new(AtomicU64::new(2048));
    let (_s, _e, pkg, force, _cap) =
        ibd_v2_retire_apply_utxo_delta(1_000, &store, &mut mem, &max_ahead, 2048, true, 1_000);
    assert!(force, "formal boundary must set force_durability");
    let pkg = pkg.expect("DEL-only pending at boundary must yield sentinel package");
    assert!(pkg.ops.is_empty());
    assert_eq!(pkg.max_block_height, 1_000);
}

#[serial_test::serial(ibd)]
#[test]
fn formal_checkpoint_runs_under_elevated_pressure() {
    use super::memory::MemoryGuard;

    let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
    let store = IbdUtxoStore::new_with_options(
        Arc::clone(&disk),
        100,
        false,
        usize::MAX,
        crate::storage::ibd_utxo_store::EvictionStrategy::Fifo,
        0,
        ValueCodec::Bincode,
    );
    fill_del_backlog(&store, 120);
    assert!(store.pending_len() > 1_000);

    let mut mem = MemoryGuard::new();
    mem.test_seed_pressure_level(PressureLevel::Elevated);
    let max_ahead = Arc::new(AtomicU64::new(2048));
    let (_s, _e, pkg, force, _cap) =
        ibd_v2_retire_apply_utxo_delta(681_000, &store, &mut mem, &max_ahead, 2048, true, 200);
    assert!(
        force,
        "formal boundary must checkpoint even when MemoryGuard is Elevated"
    );
    assert!(
        pkg.is_some(),
        "boundary at h=681000 must yield a flush package"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn between_checkpoints_under_elevated_stays_async() {
    use super::memory::MemoryGuard;

    let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
    let store = IbdUtxoStore::new_with_options(
        Arc::clone(&disk),
        100,
        false,
        usize::MAX,
        crate::storage::ibd_utxo_store::EvictionStrategy::Fifo,
        0,
        ValueCodec::Bincode,
    );
    fill_del_backlog(&store, 119);
    let mut del_scratch = Vec::new();
    let mut add_scratch = Vec::new();
    let mut adds = FxHashMap::default();
    for i in 0..50 {
        let op = synth_outpoint(120, i);
        adds.insert(
            op,
            Arc::new(UTXO {
                value: 1,
                script_pubkey: (&[0u8; 25][..]).into(),
                height: 120,
                is_coinbase: false,
            }),
        );
    }
    let delta = UtxoDelta {
        additions: adds,
        deletions: FxHashSet::default(),
    };
    store.apply_utxo_delta(&delta, 120, &mut del_scratch, &mut add_scratch, false);

    let mut mem = MemoryGuard::new();
    mem.test_seed_pressure_level(PressureLevel::Elevated);
    let max_ahead = Arc::new(AtomicU64::new(2048));
    let (_s, _e, _pkg, force, _cap) =
        ibd_v2_retire_apply_utxo_delta(681_001, &store, &mut mem, &max_ahead, 2048, true, 200);
    assert!(
        !force,
        "non-boundary retire under Elevated must stay adds-only async"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn retire_do_durability_channel_only_explicit_checkpoints() {
    assert!(!retire_do_durability(false, 16, 15, true));
    assert!(!retire_do_durability(false, 16, 15, false));
    assert!(retire_do_durability(false, 16, 16, false));
    assert!(retire_do_durability(true, 16, 3, true));
}

#[serial_test::serial(ibd)]
#[test]
fn ibd_durability_channel_cap_bounds() {
    assert_eq!(ibd_durability_channel_cap(400_000), 4);
    let segwit = ibd_durability_channel_cap(500_000);
    assert!((2..=16).contains(&segwit));
}

fn mk_durability_req(is_checkpoint: bool) -> DurabilityRequest {
    DurabilityRequest {
        pkg: PendingFlushPackage {
            ops: Arc::new(Vec::new()),
            max_block_height: 100,
            heights: Arc::new(FxHashSet::default()),
        },
        trigger_height: 100,
        is_checkpoint,
    }
}

#[serial_test::serial(ibd)]
#[test]
fn durability_batch_does_not_merge_mixed_checkpoint_flags() {
    let adds_only = mk_durability_req(false);
    let checkpoint = mk_durability_req(true);

    assert!(ibd_durability_may_merge_request(&[], &adds_only));
    assert!(ibd_durability_may_merge_request(
        &[mk_durability_req(false)],
        &adds_only
    ));
    assert!(!ibd_durability_may_merge_request(
        &[mk_durability_req(false)],
        &checkpoint
    ));
    assert!(!ibd_durability_may_merge_request(
        &[mk_durability_req(true)],
        &adds_only
    ));
    assert!(ibd_durability_may_merge_request(
        &[mk_durability_req(true)],
        &checkpoint
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn checkpoint_watermark_uses_formal_boundary_not_batch_max() {
    let batch = vec![DurabilityRequest {
        pkg: PendingFlushPackage {
            ops: Arc::new(Vec::new()),
            max_block_height: 435_005,
            heights: Arc::new(FxHashSet::default()),
        },
        trigger_height: 433_800,
        is_checkpoint: true,
    }];
    assert_eq!(
        ibd_checkpoint_watermark_for_batch(&batch, 435_005, 200),
        433_800
    );
}

#[serial_test::serial(ibd)]
#[test]
fn mtp_tip_fallback_only_near_tip() {
    // Gap resume: start 880001, tip 957850 — must not use tip window.
    assert!(!mtp_tip_window_fallback_ok(880_001, 957_850));
    assert!(!mtp_tip_window_fallback_ok(100, 200_000));
    // Near-tip catch-up: allow sliding-window fallback.
    assert!(mtp_tip_window_fallback_ok(957_820, 957_850));
    assert!(mtp_tip_window_fallback_ok(957_850, 957_850));
    assert!(!mtp_tip_window_fallback_ok(100, 0));
}

#[serial_test::serial(ibd)]
#[test]
fn formal_checkpoint_boundary_aligns_to_interval() {
    assert_eq!(ibd_formal_checkpoint_boundary(433_800, 200), Some(433_800));
    assert_eq!(ibd_formal_checkpoint_boundary(433_801, 200), None);
}

#[serial_test::serial(ibd)]
#[test]
fn del_backlog_collapse_default_min_height_350k() {
    let _lock = super::retire_flush_batch_tests_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT");
    }
    assert!(!ibd_del_backlog_use_collapse_path(349_000));
    assert!(ibd_del_backlog_use_collapse_path(350_000));
    assert!(ibd_del_backlog_use_collapse_path(431_000));
}

#[serial_test::serial(ibd)]
#[test]
fn del_backlog_collapse_gated_by_min_height_env() {
    let _lock = super::retire_flush_batch_tests_env_lock();
    unsafe {
        std::env::set_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT", "500000");
    }
    assert!(!ibd_del_backlog_use_collapse_path(300_000));
    assert!(ibd_del_backlog_use_collapse_path(500_000));
    unsafe {
        std::env::remove_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn del_backlog_collapse_forced_when_min_height_zero() {
    let _lock = super::retire_flush_batch_tests_env_lock();
    unsafe {
        std::env::set_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT", "0");
    }
    assert!(ibd_del_backlog_use_collapse_path(300_000));
    unsafe {
        std::env::remove_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn del_backlog_fast_path_returns_single_batch() {
    let _lock = super::retire_flush_batch_tests_env_lock();
    unsafe {
        std::env::set_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT", "500000");
    }

    let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
    let store = IbdUtxoStore::new_with_options(
        Arc::clone(&disk),
        100,
        false,
        usize::MAX,
        crate::storage::ibd_utxo_store::EvictionStrategy::Fifo,
        0,
        ValueCodec::Bincode,
    );
    fill_del_backlog(&store, 120);
    let store = Arc::new(store);
    let storage =
        Arc::new(crate::storage::Storage::new(tempfile::tempdir().unwrap().path()).unwrap());
    let mh = Arc::new(Mutex::new(blvm_muhash::MuHash3072::new()));

    let batch_n = ibd_flush_del_backlog_drain(&store, &mh, 120, false).unwrap();
    assert_eq!(batch_n, 1, "fast path must use single force-through batch");

    unsafe {
        std::env::remove_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT");
    }
}
