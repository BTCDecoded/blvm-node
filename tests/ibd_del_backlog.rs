//! DEL-only backlog drain (Phase A+B) — integration tests.
//!
//! Run: `cargo test -p blvm-node --features production --test ibd_del_backlog`

#![cfg(feature = "production")]

use anyhow::Result;
use blvm_node::storage::database::{BatchWriter, Tree};
use blvm_node::storage::ibd_utxo_store::{EvictionStrategy, IbdUtxoStore};
use blvm_node::storage::utxo_value_codec::ValueCodec;
use blvm_protocol::block::UtxoDelta;
use blvm_protocol::types::{OutPoint, UTXO};
use blvm_protocol::utxo_overlay::UtxoDeletionKey;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

#[derive(Default)]
struct MemTree {
    inner: StdMutex<std::collections::HashMap<Vec<u8>, Vec<u8>>>,
}

impl Tree for MemTree {
    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.inner.lock().unwrap().insert(key.to_vec(), value.to_vec());
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

impl BatchWriter for MemBatch<'_> {
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

fn make_store() -> Arc<IbdUtxoStore> {
    let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
    Arc::new(IbdUtxoStore::new_with_options(
        disk,
        100,
        false,
        usize::MAX,
        EvictionStrategy::Fifo,
        0,
        ValueCodec::Bincode,
    ))
}

/// After adds-only async flushes, del-only capped drain must return tombstones only.
#[test]
fn del_only_drain_returns_tombstones_only() {
    let store = make_store();
    let mut del_scratch = Vec::new();
    let mut add_scratch = Vec::new();
    let mut alive: FxHashMap<OutPoint, UTXO> = FxHashMap::default();

    for h in 1..=80u64 {
        let mut deletions = Vec::new();
        for op in alive.keys().take(20) {
            deletions.push(*op);
        }
        for op in &deletions {
            alive.remove(op);
        }
        let mut additions = Vec::new();
        for i in 0..20 {
            let op = synth_outpoint(h, i);
            let u = synth_utxo(50_000, h);
            additions.push((op, u.clone()));
            alive.insert(op, u);
        }
        let delta = build_delta(additions, deletions);
        store.apply_utxo_delta(&delta, h, &mut del_scratch, &mut add_scratch, false);
        if let Some(pkg) = store.maybe_take_flush_batch_adds_only() {
            let prepared = pkg.prepare_for_disk(ValueCodec::Bincode).unwrap();
            store.flush_prepared_package_adds_only(&prepared).unwrap();
        }
    }

    assert!(store.pending_len() > 500, "expected del-heavy backlog");

    let del_pkg = store
        .take_flush_batch_dels_only_through_capped(80, 10_000)
        .expect("del-only batch");
    assert!(
        del_pkg.ops.iter().all(|(_, v)| v.is_none()),
        "del-only package must contain tombstones only"
    );
    assert!(del_pkg.ops.len() > 100, "expected substantial del batch");
}

/// A1 path: height-filtered adds drain must not pull ops above watermark.
#[test]
fn adds_only_through_capped_respects_watermark() {
    let store = make_store();
    let mut del_scratch = Vec::new();
    let mut add_scratch = Vec::new();

    for h in 1..=10u64 {
        let op = synth_outpoint(h, 0);
        let u = synth_utxo(1_000, h);
        let delta = build_delta(vec![(op, u)], vec![]);
        store.apply_utxo_delta(&delta, h, &mut del_scratch, &mut add_scratch, false);
    }

    let pkg = store
        .take_flush_batch_adds_only_through_capped(5, 100)
        .expect("batch");
    assert!(
        pkg.ops.len() <= 5,
        "at most one add per height through wm=5"
    );
    assert!(
        store.has_pending_adds_at_or_below(5) == false,
        "all adds at h<=5 should be drained"
    );
    assert!(
        store.has_pending_adds_at_or_below(10),
        "adds above watermark must remain"
    );
}
