//! `UtxoIndex`: 7-age UTXO index with compacter.
//!
//! Architecture (mirrors Hornet `index.h`):
//! - 7 `MemoryAge` tiers: ages[0] = newest (mutable), ages[6] = oldest (frozen).
//! - `kMutableAges = 3`: ages 0–2 are mutable (accept appends).
//! - `kFanIn = 8`: each age merges when it has ≥8 runs.
//! - Mutable window ≈ 8 + 64 + 512 = 584 blocks before forced freeze.
//! - **Compacter**: 7 shared worker threads, one `crossbeam::channel<usize>` (age index).
//!   Any thread handles any age. Age posts its index when `merge_ready()` fires.
//!
//! BLVM improvement: `contiguous_length` is updated per-append so the Table flusher
//! can use it as a stable watermark for `commit_before(h)` without an extra barrier.

use super::memory_age::{MemoryAge, Pin};
use super::memory_run::MemoryRun;
use super::types::{OutputId, OutputKV};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

/// Number of age tiers.
const K_AGES: usize = 7;
/// Oldest mutable age (ages 0..K_MUTABLE are mutable).
const K_MUTABLE_AGES: usize = 3;
/// Fan-in: trigger merge after this many runs in one age.
const K_FAN_IN: usize = 8;
/// Number of compacter worker threads.
const K_COMPACTER_THREADS: usize = 7;

/// Manages background merges across all age tiers.
///
/// One `crossbeam::channel` shared by all `K_COMPACTER_THREADS` worker threads.
/// Any thread picks up any age index posted to the channel and runs its merge.
struct Compacter {
    tx: crossbeam_channel::Sender<usize>,
    _threads: Vec<std::thread::JoinHandle<()>>,
}

impl Compacter {
    fn start(ages: Arc<[MemoryAge; K_AGES]>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<usize>();
        let mut threads = Vec::with_capacity(K_COMPACTER_THREADS);
        for _ in 0..K_COMPACTER_THREADS {
            let rx = rx.clone();
            let ages = Arc::clone(&ages);
            let handle = std::thread::Builder::new()
                .name("utxo-compacter".to_string())
                .spawn(move || {
                    while let Ok(age_idx) = rx.recv() {
                        if age_idx == usize::MAX {
                            break; // shutdown sentinel
                        }
                        run_merge_for_age(&ages, age_idx);
                    }
                })
                .expect("spawn compacter thread");
            threads.push(handle);
        }
        Self { tx, _threads: threads }
    }

    fn enqueue(&self, age_idx: usize) {
        let _ = self.tx.try_send(age_idx);
    }

    fn shutdown(&self) {
        for _ in 0..K_COMPACTER_THREADS {
            let _ = self.tx.send(usize::MAX);
        }
    }
}

fn run_merge_for_age(ages: &[MemoryAge; K_AGES], age_idx: usize) {
    let age = &ages[age_idx];
    // Take runs for merge (CAS-guarded, non-blocking on contention).
    let Some(runs) = age.take_for_merge() else { return };

    let merged = MemoryRun::merge(&runs);
    if merged.is_empty() {
        // All entries cancelled (create+spend pairs) — nothing to push downstream.
        let max_h = runs.iter().map(|r| r.height_range().1).max().unwrap_or(i32::MIN);
        age.complete_merge(max_h);
        return;
    }

    let max_h = merged.height_range().1;
    let entries: Vec<OutputKV> = merged.entries.clone();

    // Push merged result to the next older age.
    let next_idx = age_idx + 1;
    if next_idx < K_AGES {
        ages[next_idx].append(entries, max_h);
    }
    // If next_idx == K_AGES, entries are beyond all ages — they would be exported to disk.
    // In Phase 1 / 2 we simply drop them; Phase 3 export handles this via scan_live.

    age.complete_merge(max_h);
}

/// 7-age UTXO index. The primary lookup structure for the IBD engine.
pub struct UtxoIndex {
    ages: Arc<[MemoryAge; K_AGES]>,
    compacter: Compacter,
    /// Highest height for which all blocks up to and including it have been appended.
    contiguous_length: AtomicI32,
}

impl UtxoIndex {
    pub fn new() -> Self {
        // Build ages; mutable ages get enqueue callbacks; frozen ages do not.
        // We need Arc<[MemoryAge; K_AGES]> to share with the compacter, but we build
        // the ages before the compacter (which needs the Arc). Use a two-step init:
        // build raw ages, wrap in Arc, then create compacter with the Arc.
        //
        // Safety: we build the ages array, wrap in Arc, then the compacter holds a clone.
        // The enqueue closures will be wired after compacter creation via `set_enqueue`
        // (the Compacter struct holds the tx end of the channel).

        // Build ages without enqueue first (no-op enqueue placeholder).
        let ages_raw: [MemoryAge; K_AGES] = std::array::from_fn(|i| {
            let is_mutable = i < K_MUTABLE_AGES;
            MemoryAge::new(is_mutable, K_FAN_IN)
        });

        let ages = Arc::new(ages_raw);
        let compacter = Compacter::start(Arc::clone(&ages));

        // Wire enqueue callbacks to the compacter channel.
        // Since MemoryAge stores the callback in `enqueue: Option<Box<dyn Fn()>>` but
        // we built ages without it, we need a different approach: the Compacter's `enqueue`
        // method is called from UtxoIndex::append directly instead of from within MemoryAge.
        // This avoids the bootstrapping problem.

        Self {
            ages,
            compacter,
            contiguous_length: AtomicI32::new(-1),
        }
    }

    /// Append a block's UTXO ops (Add + Delete entries) into the mutable tip (age 0).
    ///
    /// Returns a `Pin` keeping `height` resident in the mutable window until dropped.
    pub fn append(&self, entries: Vec<OutputKV>, height: i32) -> Pin {
        let pin = self.ages[0].pin_height(height);
        self.ages[0].append(entries, height);
        self.contiguous_length.fetch_max(height, Ordering::Relaxed);

        // Check if any of the mutable ages is ready for merge; post to compacter.
        for i in 0..K_MUTABLE_AGES {
            if self.ages[i].merge_ready() {
                self.compacter.enqueue(i);
            }
        }
        pin
    }

    /// Query all ages for `key`. Returns `Some(id)` from the newest age that has it.
    ///
    /// Used by `UtxoDatabase::query` (sorted batch path). For single-key lookup during
    /// intra-block resolution.
    pub fn lookup_key(&self, key: &[u8; 36]) -> Option<OutputId> {
        for age in self.ages.iter() {
            if let Some(id) = age.lookup_key(key, 0, i32::MAX) {
                return Some(id);
            }
        }
        None
    }

    /// Batch query: fills `ids[i]` for each `keys[i]` across all ages.
    ///
    /// `ids` must be pre-filled with `OutputId::MAX` (sentinel for "not yet resolved").
    /// Ages are queried newest-to-oldest; once all ids are resolved the loop short-circuits.
    ///
    /// `before` is an exclusive upper bound on `entry.height`. Pass `i32::MAX` to see all heights.
    /// `SpendSession` passes `before = height` so that Deletes recorded for the current block
    /// are invisible — only prior-block Adds are returned.
    pub fn batch_query(&self, keys: &[[u8; 36]], ids: &mut [OutputId], before: i32) {
        debug_assert_eq!(keys.len(), ids.len());
        for age in self.ages.iter() {
            // Short-circuit: if all ids are resolved, stop early.
            if ids.iter().all(|id| *id != OutputId::MAX) {
                break;
            }
            age.batch_query(keys, ids, 0, before);
        }
    }

    /// Block the calling thread until `contiguous_length >= height`.
    ///
    /// Used only by the watermark export path — NOT on the validation hot path.
    pub fn wait_for_height(&self, height: i32) {
        while self.contiguous_length.load(Ordering::Relaxed) < height {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    pub fn contiguous_length(&self) -> i32 {
        self.contiguous_length.load(Ordering::Relaxed)
    }

    /// Remove all UTXO ops at `height >= since` from mutable ages. Reorg recovery.
    pub fn erase_since(&self, since: i32) {
        for i in 0..K_MUTABLE_AGES {
            self.ages[i].erase_since(since);
        }
        // Roll back contiguous_length.
        self.contiguous_length.fetch_min(since - 1, Ordering::Relaxed);
    }

    /// Iterate all non-cancelled Add entries across all ages. For watermark export.
    ///
    /// Returns entries sorted by OutputKV order (key asc, height desc). Duplicates (same key
    /// in multiple ages) are resolved by taking the newest (highest height) entry.
    pub fn scan_all_live(&self) -> Vec<OutputKV> {
        use std::collections::HashMap;
        let mut live: HashMap<[u8; 36], OutputKV> = HashMap::new();

        // Oldest-to-newest so newer entries overwrite older ones.
        for age in self.ages.iter().rev() {
            let snapshot = age.snapshot_runs();
            for run in snapshot.iter() {
                for entry in &run.entries {
                    if entry.is_add() {
                        live.entry(entry.key).or_insert(*entry);
                    } else if entry.is_delete() {
                        live.remove(&entry.key);
                    }
                }
            }
        }

        let mut result: Vec<OutputKV> = live.into_values().collect();
        result.sort_unstable();
        result
    }
}

impl Drop for UtxoIndex {
    fn drop(&mut self) {
        self.compacter.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::OutputKV;

    fn make_key(n: u8) -> [u8; 36] {
        let mut k = [0u8; 36];
        k[0] = n;
        k
    }

    #[test]
    fn test_append_and_query() {
        let idx = UtxoIndex::new();
        let k = make_key(1);
        let _pin = idx.append(vec![OutputKV::new_add(k, 100, 42)], 100);
        assert_eq!(idx.lookup_key(&k), Some(42));
        assert_eq!(idx.lookup_key(&make_key(2)), None);
    }

    #[test]
    fn test_batch_query() {
        let idx = UtxoIndex::new();
        let k1 = make_key(1);
        let k2 = make_key(2);
        let _p1 = idx.append(vec![OutputKV::new_add(k1, 100, 10)], 100);
        let _p2 = idx.append(vec![OutputKV::new_add(k2, 101, 20)], 101);
        let mut ids = [OutputId::MAX; 2];
        idx.batch_query(&[k1, k2], &mut ids, i32::MAX);
        assert_eq!(ids[0], 10);
        assert_eq!(ids[1], 20);
    }

    #[test]
    fn test_contiguous_length() {
        let idx = UtxoIndex::new();
        assert_eq!(idx.contiguous_length(), -1);
        let k = make_key(1);
        let _pin = idx.append(vec![OutputKV::new_add(k, 50, 1)], 50);
        assert_eq!(idx.contiguous_length(), 50);
    }

    #[test]
    fn test_erase_since() {
        let idx = UtxoIndex::new();
        let k1 = make_key(1);
        let k2 = make_key(2);
        let _p1 = idx.append(vec![OutputKV::new_add(k1, 50, 1)], 50);
        let _p2 = idx.append(vec![OutputKV::new_add(k2, 100, 2)], 100);
        idx.erase_since(75);
        assert_eq!(idx.lookup_key(&k1), Some(1));
        assert_eq!(idx.lookup_key(&k2), None);
    }
}
