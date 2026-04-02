//! UTXO prefetch workers for parallel IBD.
//!
//! Workers load UTXOs for upcoming blocks while validation runs, hiding disk latency.

use std::sync::atomic::{AtomicU64, Ordering};

/// Aggregate multi_get disk latency across all prefetch workers (milliseconds).
static PREFETCH_TOTAL_DISK_MS: AtomicU64 = AtomicU64::new(0);
/// Total number of individual UTXO keys fetched from disk by prefetch workers.
static PREFETCH_TOTAL_DISK_READS: AtomicU64 = AtomicU64::new(0);
/// Total blocks processed by all prefetch workers (for avg disk-reads/block).
static PREFETCH_TOTAL_BLOCKS: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "production")]
use std::collections::BTreeMap;
#[cfg(feature = "production")]
use std::sync::{Arc, Mutex};

#[cfg(feature = "production")]
use blvm_consensus::types::UTXO;
#[cfg(feature = "production")]
use crossbeam_channel::{Receiver, Sender};
#[cfg(feature = "production")]
use rustc_hash::FxHashMap;

#[cfg(feature = "production")]
use crate::storage::disk_utxo::{load_keys_from_disk, OutPointKey};
#[cfg(feature = "production")]
use crate::storage::ibd_utxo_store::IbdUtxoStore;

use super::types::{PrefetchWorkItemV2, ReadyItem};

/// Reorders prefetch completions so the feeder always receives blocks in ascending height.
/// Parallel workers finish UTXO loads out of order; without this, `ready_tx` can deliver N+k
/// before N and validation stalls (feeder min_height > next_validation_height).
#[cfg(feature = "production")]
pub(crate) struct OrderedReadyBridge {
    inner: Mutex<OrderedReadyInner>,
    out: Sender<ReadyItem>,
}

#[cfg(feature = "production")]
struct OrderedReadyInner {
    /// Next height we may emit to `out` (set on first `coordinator_will_send_height`).
    next_expected: Option<u64>,
    pending: BTreeMap<u64, ReadyItem>,
}

#[cfg(feature = "production")]
impl OrderedReadyBridge {
    pub(crate) fn new(out: Sender<ReadyItem>) -> Self {
        Self {
            inner: Mutex::new(OrderedReadyInner {
                next_expected: None,
                pending: BTreeMap::new(),
            }),
            out,
        }
    }

    /// Call before sending height `h` to prefetch or gap-fill workers (same order as drain).
    pub(crate) fn coordinator_will_send_height(&self, h: u64) {
        let mut g = self
            .inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned");
        if g.next_expected.is_none() {
            g.next_expected = Some(h);
        }
    }

    /// Worker finished prefetch, or coordinator used direct-to-feeder fallback (same as a completion).
    pub(crate) fn worker_complete(&self, h: u64, item: ReadyItem) {
        let mut g = self
            .inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned");
        g.pending.insert(h, item);
        Self::flush_unlocked(&self.out, &mut g);
    }

    fn flush_unlocked(out: &Sender<ReadyItem>, g: &mut OrderedReadyInner) {
        let Some(mut n) = g.next_expected else {
            return;
        };
        while let Some(item) = g.pending.remove(&n) {
            let _ = out.send(item);
            n += 1;
        }
        g.next_expected = Some(n);
    }
}

/// Single-pass: cache lookup + disk load + map build.
/// Used by prefetch workers to build UTXO map for a block.
/// Updates global counters for [PREFETCH_PERF] logging in the worker loop.
#[cfg(feature = "production")]
pub(crate) fn prefetch_build_utxo_map(
    store: &IbdUtxoStore,
    keys: &[OutPointKey],
) -> FxHashMap<OutPointKey, Arc<UTXO>> {
    let mut full_map = FxHashMap::with_capacity_and_hasher(keys.len(), Default::default());
    let mut to_load: Vec<OutPointKey> = Vec::new();
    for key in keys {
        if let Some(ref r) = store.cache_get(key) {
            full_map.insert(*key, Arc::clone(&r.utxo));
            continue;
        }
        to_load.push(*key);
    }
    if !to_load.is_empty() && !store.memory_only() {
        let miss_count = to_load.len() as u64;
        let t_disk = std::time::Instant::now();
        if let Ok((loaded, _)) = load_keys_from_disk(store.disk_clone(), to_load) {
            let disk_ms = t_disk.elapsed().as_millis() as u64;
            PREFETCH_TOTAL_DISK_MS.fetch_add(disk_ms, Ordering::Relaxed);
            PREFETCH_TOTAL_DISK_READS.fetch_add(miss_count, Ordering::Relaxed);
            let skip_recache = store.skip_recache_disk_hits();
            if skip_recache {
                for (key, utxo) in loaded {
                    let arc = Arc::new(utxo);
                    full_map.insert(key, arc);
                }
            } else {
                let mut pairs: Vec<(OutPointKey, Arc<UTXO>)> = Vec::with_capacity(loaded.len());
                for (key, utxo) in loaded {
                    let arc = Arc::new(utxo);
                    full_map.insert(key, Arc::clone(&arc));
                    pairs.push((key, arc));
                }
                if !pairs.is_empty() {
                    store.cache_insert_and_track_batch(&pairs);
                }
            }
        }
    }
    PREFETCH_TOTAL_BLOCKS.fetch_add(1, Ordering::Relaxed);
    full_map
}

/// Run a single prefetch worker. Receives work items, builds UTXO map, sends to ready queue.
/// Logs [PREFETCH_PERF] aggregate stats every 5000 blocks to track disk latency evolution.
#[cfg(feature = "production")]
pub(crate) fn run_prefetch_worker(
    rx: Receiver<PrefetchWorkItemV2>,
    tx: Sender<ReadyItem>,
    store: Arc<IbdUtxoStore>,
) {
    let mut local_blocks: u64 = 0;
    while let Ok((s, keys, tx_ids, h, block, witnesses)) = rx.recv() {
        let full_map = prefetch_build_utxo_map(&s, &keys);
        let _ = tx.send((h, block, witnesses, keys, full_map, tx_ids));
        local_blocks += 1;
        // Log aggregate stats every 5000 blocks processed by this worker.
        if local_blocks % 5_000 == 0 {
            let total_blocks = PREFETCH_TOTAL_BLOCKS.load(Ordering::Relaxed);
            let total_reads = PREFETCH_TOTAL_DISK_READS.load(Ordering::Relaxed);
            let total_ms = PREFETCH_TOTAL_DISK_MS.load(Ordering::Relaxed);
            let avg_ms_per_read = if total_reads > 0 {
                total_ms as f64 / total_reads as f64
            } else {
                0.0
            };
            let reads_per_block = if total_blocks > 0 {
                total_reads as f64 / total_blocks as f64
            } else {
                0.0
            };
            tracing::info!(
                "[PREFETCH_PERF] h={} total_blocks={} disk_reads={} disk_ms={} avg_ms_per_read={:.3} reads_per_block={:.1}",
                h, total_blocks, total_reads, total_ms, avg_ms_per_read, reads_per_block
            );
        }
    }
}
