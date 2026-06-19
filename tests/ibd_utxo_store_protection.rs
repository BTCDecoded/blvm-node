//! Deterministic test for IbdUtxoStore height-granular protection invariant.
//!
//! Drives the full lifecycle (worker_cache_put_protected → apply_utxo_delta →
//! take_flush_batch_force → flush_prepared_package → release_protected_heights)
//! and verifies that no UTXO that should still exist becomes unfindable via
//! supplement_utxo_map_with_buf.
//!
//! Run: `cargo test -p blvm-node --features production --test ibd_utxo_store_protection -- --nocapture`

#![cfg(feature = "production")]

use anyhow::Result;
use blvm_node::storage::database::{BatchWriter, Tree};
use blvm_node::storage::disk_utxo::{OutPointKey, outpoint_to_key};
use blvm_node::storage::ibd_utxo_store::{EvictionStrategy, IbdUtxoStore, PreparedFlushPackage};
use blvm_node::storage::utxo_value_codec::ValueCodec;
use blvm_protocol::block::UtxoDelta;
use blvm_protocol::types::{OutPoint, UTXO, UtxoSet};
use blvm_protocol::utxo_overlay::UtxoDeletionKey;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

// ─── In-memory Tree backed by HashMap, with realistic batch semantics ──────
#[derive(Default)]
struct MemTree {
    inner: StdMutex<std::collections::HashMap<Vec<u8>, Vec<u8>>>,
}

impl Tree for MemTree {
    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .insert(key.to_vec(), value.to_vec());
        Ok(())
    }
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.inner.lock().unwrap().get(key).cloned())
    }
    fn get_many(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>> {
        let g = self.inner.lock().unwrap();
        Ok(keys.iter().map(|k| g.get(*k).cloned()).collect())
    }
    fn remove(&self, key: &[u8]) -> Result<()> {
        self.inner.lock().unwrap().remove(key);
        Ok(())
    }
    fn contains_key(&self, key: &[u8]) -> Result<bool> {
        Ok(self.inner.lock().unwrap().contains_key(key))
    }
    fn clear(&self) -> Result<()> {
        self.inner.lock().unwrap().clear();
        Ok(())
    }
    fn len(&self) -> Result<usize> {
        Ok(self.inner.lock().unwrap().len())
    }
    fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
        let entries: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| Ok((k.clone(), v.clone())))
            .collect();
        Box::new(entries.into_iter())
    }
    fn batch(&self) -> Result<Box<dyn BatchWriter + '_>> {
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
    fn commit(self: Box<Self>) -> Result<()> {
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

// ─── Helpers for synthetic UTXOs ───────────────────────────────────────────

fn synth_outpoint(seed: u64, idx: u32) -> OutPoint {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&seed.to_le_bytes());
    hash[8..12].copy_from_slice(&idx.to_le_bytes());
    OutPoint { hash, index: idx }
}

fn synth_utxo(value: i64, height: u64) -> UTXO {
    UTXO {
        value,
        script_pubkey: (&[0u8; 25][..]).into(),
        height,
        is_coinbase: false,
    }
}

// 36-byte deletion key: 32-byte txid || 4-byte big-endian vout (matches utxo_overlay).
fn outpoint_to_deletion_key(op: &OutPoint) -> UtxoDeletionKey {
    let mut k = [0u8; 36];
    k[..32].copy_from_slice(&op.hash);
    k[32..36].copy_from_slice(&op.index.to_be_bytes());
    k
}

fn build_delta(additions: Vec<(OutPoint, UTXO)>, deletions: Vec<OutPoint>) -> UtxoDelta {
    let mut adds: FxHashMap<OutPoint, Arc<UTXO>> = FxHashMap::default();
    for (op, u) in additions {
        adds.insert(op, Arc::new(u));
    }
    let mut dels: FxHashSet<UtxoDeletionKey> = FxHashSet::default();
    for op in deletions {
        dels.insert(outpoint_to_deletion_key(&op));
    }
    UtxoDelta {
        additions: adds,
        deletions: dels,
    }
}

// ─── The actual reproduction test ──────────────────────────────────────────

/// Drive the height-granular protection lifecycle for many synthetic blocks,
/// asserting that every UTXO created and not yet spent remains findable through
/// the public lookup path (cache → in_flight → disk via supplement).
#[test]
fn height_granular_protection_no_lost_utxos() {
    let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
    // Use a real cache cap so eviction kicks in (reproduces the IBD pressure path).
    // 200 entries forces churn well below the 5k UTXOs we'll create over the run.
    let store = Arc::new(IbdUtxoStore::new_with_options(
        Arc::clone(&disk),
        /*flush_threshold=*/ 256,
        /*memory_only=*/ false,
        /*max_entries=*/ 200,
        EvictionStrategy::Lifo,
        /*utxo_disk_commit_through=*/ 0,
        ValueCodec::Bincode,
    ));

    // Track every UTXO ever created and the height at which it was spent (None = unspent).
    // After each block we assert: every unspent UTXO is findable via supplement.
    let mut alive: FxHashMap<OutPoint, (u64, UTXO)> = FxHashMap::default();
    let mut del_scratch: Vec<OutPointKey> = Vec::new();
    let mut add_scratch: Vec<(OutPointKey, Arc<UTXO>)> = Vec::new();

    // Default 100 blocks (CI-friendly). Override: `BLVM_TEST_IBD_UTXO_BLOCKS=500`.
    // Each block creates 10 outputs and spends up to 5 prior outputs.
    // Periodically (every 7 blocks) we flush. Mirrors retire-thread behavior.
    let n_blocks: u64 = std::env::var("BLVM_TEST_IBD_UTXO_BLOCKS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(100);
    const OUTS_PER_BLOCK: usize = 10;
    const SPENDS_PER_BLOCK: usize = 5;

    let mut rng_state: u64 = 0x12345678_9abcdef0;
    let mut next_rng = || {
        rng_state = rng_state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        rng_state
    };

    for h in 1..=n_blocks {
        // 1) Pick deletions from the alive set: simulate this block spending some prior outputs.
        let alive_keys: Vec<OutPoint> = alive.keys().copied().collect();
        let mut deletions: Vec<OutPoint> = Vec::new();
        if !alive_keys.is_empty() {
            for _ in 0..SPENDS_PER_BLOCK.min(alive_keys.len()) {
                let idx = (next_rng() as usize) % alive_keys.len();
                let op = alive_keys[idx];
                if !deletions.contains(&op) {
                    deletions.push(op);
                }
            }
        }

        // 2) Build additions: 10 brand-new outputs at this height.
        let mut additions: Vec<(OutPoint, UTXO)> = Vec::new();
        for i in 0..OUTS_PER_BLOCK {
            let op = synth_outpoint(h, i as u32);
            let u = synth_utxo((h * 1000 + i as u64) as i64, h);
            additions.push((op, u));
        }

        // 3) Update tracking state BEFORE pushing to store, so post-block invariants are clear.
        for op in &deletions {
            alive.remove(op);
        }
        for (op, u) in &additions {
            alive.insert(*op, (h, u.clone()));
        }

        // 4) Mirror the worker hot path:
        //    - worker_cache_put_protected populates cache + protects the height.
        //    - apply_utxo_delta(_, true) removes deletions from cache + pushes ops to pending.
        let delta = build_delta(additions.clone(), deletions.clone());
        store.worker_cache_put_protected(&delta.additions, h);
        store.apply_utxo_delta(&delta, h, &mut del_scratch, &mut add_scratch, true);

        // 5) Periodic flush, mirroring the retire thread's force-flush path.
        if h % 7 == 0 {
            if let Some(pkg) = store.take_flush_batch_force() {
                let heights = Arc::clone(&pkg.heights);
                let prepared = pkg
                    .prepare_for_disk(ValueCodec::Bincode)
                    .expect("prepare_for_disk");
                store
                    .flush_prepared_package(&prepared, None)
                    .expect("flush_prepared_package");
                store.release_protected_heights(&heights);
            }
        }

        // 6) Invariant: every unspent UTXO must be findable via supplement.
        // This drives the SAME path validation workers use.
        let keys: Vec<OutPointKey> = alive.keys().map(outpoint_to_key).collect();
        let mut buf: Vec<OutPointKey> = Vec::new();
        let mut map: UtxoSet = UtxoSet::default();
        store.supplement_utxo_map_with_buf(&mut map, &keys, &mut buf);

        let mut missing: Vec<OutPoint> = Vec::new();
        for op in alive.keys() {
            if !map.contains_key(op) {
                missing.push(*op);
            }
        }
        if !missing.is_empty() {
            eprintln!(
                "FAILURE at h={}: {} of {} unspent UTXOs are unreachable via supplement",
                h,
                missing.len(),
                alive.len()
            );
            for (i, op) in missing.iter().take(5).enumerate() {
                let (created_h, _) = &alive[op];
                let in_cache = store.cache_get(&outpoint_to_key(op)).is_some();
                eprintln!(
                    "  miss[{}]: created_at={} in_cache={} key={:02x?}",
                    i,
                    created_h,
                    in_cache,
                    &outpoint_to_key(op)[..8]
                );
            }
            panic!(
                "Lost UTXOs at h={} (cache.len={}, protected_len={}, pending_len={})",
                h,
                store.len(),
                store.protected_len(),
                store.pending_len()
            );
        }
    }

    // Final flush + verify everything is durable on disk.
    while let Some(pkg) = store.take_flush_batch_force() {
        let heights = Arc::clone(&pkg.heights);
        let prepared = pkg
            .prepare_for_disk(ValueCodec::Bincode)
            .expect("prepare_for_disk");
        store
            .flush_prepared_package(&prepared, None)
            .expect("flush_prepared_package");
        store.release_protected_heights(&heights);
    }

    let keys: Vec<OutPointKey> = alive.keys().map(outpoint_to_key).collect();
    let mut buf: Vec<OutPointKey> = Vec::new();
    let mut map: UtxoSet = UtxoSet::default();
    store.supplement_utxo_map_with_buf(&mut map, &keys, &mut buf);
    let post_flush_missing = alive.keys().filter(|op| !map.contains_key(*op)).count();
    assert_eq!(
        post_flush_missing, 0,
        "post-final-flush: {post_flush_missing} unspent UTXOs unreachable"
    );
}

/// Net delete in a flush batch (no durable `ibd_utxos` row): MuHash must not error; removes
/// match only what was persisted (regression for create+spend folded to delete before SST insert).
#[test]
fn flush_muhash_net_delete_without_disk_row_succeeds() {
    let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
    let store = IbdUtxoStore::new(Arc::clone(&disk), 1024);
    let op = synth_outpoint(42, 7);
    let key = outpoint_to_key(&op);

    assert!(disk.get(key.as_slice()).expect("get").is_none());

    let pkg = PreparedFlushPackage {
        rows: Arc::new(vec![(key, None)]),
        slab: Arc::new(Vec::new()),
        max_block_height: 1,
    };

    let mut mh = blvm_muhash::MuHash3072::new();
    let expect = mh.clone().finalize();

    store
        .flush_prepared_package(&pkg, Some(&mut mh))
        .expect("flush_prepared_package with MuHash must accept delete-without-disk-row");

    assert_eq!(
        mh.clone().finalize(),
        expect,
        "MuHash running state unchanged when no durable coin to remove"
    );
    assert!(
        disk.get(key.as_slice()).expect("get").is_none(),
        "delete against empty tree is a no-op"
    );
}

/// Regression: parallel workers may stamp pending ops for height H+k before H; flushes must not
/// drain future heights when capped to the retire height (`*_flush_batch_through`).
#[test]
fn flush_height_cap_keeps_higher_block_pending() {
    let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
    let store = Arc::new(IbdUtxoStore::new_with_options(
        Arc::clone(&disk),
        1,
        false,
        usize::MAX,
        EvictionStrategy::Fifo,
        0,
        ValueCodec::Bincode,
    ));
    let mut del_scratch = Vec::new();
    let mut add_scratch = Vec::new();

    let op10 = synth_outpoint(9001, 0);
    let op20 = synth_outpoint(9002, 0);
    let d10 = build_delta(vec![(op10, synth_utxo(1000, 10))], vec![]);
    let d20 = build_delta(vec![(op20, synth_utxo(2000, 20))], vec![]);

    store.worker_cache_put_protected(&d10.additions, 10);
    store.apply_utxo_delta(&d10, 10, &mut del_scratch, &mut add_scratch, true);

    store.worker_cache_put_protected(&d20.additions, 20);
    store.apply_utxo_delta(&d20, 20, &mut del_scratch, &mut add_scratch, true);

    assert_eq!(store.pending_len(), 2);

    let pkg = store
        .maybe_take_flush_batch_through(10)
        .expect("batch through h=10");
    assert_eq!(pkg.max_block_height, 10);
    assert_eq!(pkg.ops.len(), 1);

    assert_eq!(
        store.pending_len(),
        1,
        "height-20 ops must remain until flush cap catches up"
    );

    let pkg2 = store
        .maybe_take_flush_batch_through(20)
        .expect("batch through h=20");
    assert_eq!(pkg2.max_block_height, 20);
    assert_eq!(pkg2.ops.len(), 1);
    assert_eq!(store.pending_len(), 0);
}
