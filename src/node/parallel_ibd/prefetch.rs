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
use super::types::{SharedBlock, SharedWitnesses};
#[cfg(feature = "production")]
use blvm_protocol::types::UTXO;
#[cfg(feature = "production")]
use blvm_protocol::{Block, Hash, UtxoSet};
#[cfg(feature = "production")]
use crossbeam_channel::{Receiver, Sender};
#[cfg(feature = "production")]
use rustc_hash::FxHashMap;

#[cfg(feature = "production")]
use crate::storage::disk_utxo::{OutPointKey, load_keys_from_disk};
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
        if let Ok((loaded, keys_scanned)) =
            load_keys_from_disk(store.disk_clone(), to_load, store.value_codec())
        {
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
            // Check in_flight for any keys the disk lookup missed. This handles the race
            // where a flush is mid-commit (ADD not yet durable) when the disk lookup runs —
            // the same race that causes IBD_MISSING_UTXO if supplement also misses them.
            // Supplement has its own pre+post in_flight scan so this is defence-in-depth.
            if store.max_entries_is_bounded() {
                store.supplement_in_flight_for_keys(&keys_scanned, &mut full_map);
            }
        }
    }
    PREFETCH_TOTAL_BLOCKS.fetch_add(1, Ordering::Relaxed);
    full_map
}

/// Build the speculative-additions `UtxoSet` for a block: every output the block creates, ready
/// to plug View(h+k) holes for blocks that arrive at the validation worker before this block's
/// own validation has retired. Equivalent to `D(h).additions` ∪ intra-block-spent outputs (which
/// later blocks never reference, so the over-approximation is harmless).
///
/// Runs the same compute on the prefetch worker pool (`cpus * 2` threads), which is otherwise
/// idle while disk MultiGet RTTs complete. Moving this off the validation dispatcher removes
/// ~O(outputs) HashMap inserts + `Arc::new(UTXO)` allocations from the single-threaded hot path
/// (~3-15 ms/block at h>300k where blocks have 2-4k outputs).
/// Accepts `SharedBlock` (Arc<Block>) so callers pass the same Arc rather than a raw reference,
/// matching the Arc-first pipeline convention.
#[cfg(feature = "production")]
pub(crate) fn build_spec_adds(block: &Block, tx_ids: &[Hash], height: u64) -> UtxoSet {
    let mut map = UtxoSet::default();
    for (tx_idx, (tx, txid)) in block.transactions.iter().zip(tx_ids.iter()).enumerate() {
        let is_coinbase = tx_idx == 0;
        for (out_idx, output) in tx.outputs.iter().enumerate() {
            let op = blvm_protocol::OutPoint {
                hash: *txid,
                index: out_idx as u32,
            };
            let utxo = UTXO {
                value: output.value,
                script_pubkey: output.script_pubkey.as_slice().into(),
                height,
                is_coinbase,
            };
            map.insert(op, Arc::new(utxo));
        }
    }
    map
}

/// Run a single prefetch worker. Receives work items, builds UTXO map, sends to ready queue
/// **via `OrderedReadyBridge`** so heights reach the feeder in strict ascending order even when
/// parallel workers complete out of order. Without the bridge the feeder can land N+1 before N
/// and the validation cursor stalls (min_buffered_height > next_validation_height).
///
/// Logs [PREFETCH_PERF] aggregate stats every 5000 blocks to track disk latency evolution.
#[cfg(feature = "production")]
pub(crate) fn run_prefetch_worker(
    rx: Receiver<PrefetchWorkItemV2>,
    bridge: Arc<OrderedReadyBridge>,
    store: Arc<IbdUtxoStore>,
) {
    let _ = store; // store handed to closures via the work item; kept on signature for future reuse
    let mut local_blocks: u64 = 0;
    // `block: SharedBlock` and `witnesses: SharedWitnesses` are Arc — no deep copy.
    while let Ok((s, keys, tx_ids, h, block, witnesses, engine_mode)) = rx.recv() {
        // In engine mode the age-tiered UtxoDatabase resolves all UTXOs on the worker thread
        // via PartialSpendSession::complete(). The prefetch map and spec_adds are consumed only
        // by the legacy IbdUtxoStore path — skip both to avoid ~440 wasted DashMap lookups +
        // RocksDB MultiGet + ~2000 Arc<UTXO> allocs per block that are immediately discarded.
        let (full_map, spec_adds) = if engine_mode {
            (
                rustc_hash::FxHashMap::default(),
                Arc::new(blvm_protocol::UtxoSet::default()),
            )
        } else {
            let full_map = prefetch_build_utxo_map(&s, &keys);
            // Build spec_adds on this worker thread (was on the dispatcher; see `build_spec_adds`).
            let spec_adds = Arc::new(build_spec_adds(&block, &tx_ids, h));
            (full_map, spec_adds)
        };
        let item: ReadyItem = (h, block, witnesses, keys, full_map, tx_ids, spec_adds);
        bridge.worker_complete(h, item);
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
                h,
                total_blocks,
                total_reads,
                total_ms,
                avg_ms_per_read,
                reads_per_block
            );
        }
    }
}

#[cfg(all(test, feature = "production"))]
mod tests {
    use super::*;
    use blvm_protocol::{Block, BlockHeader, Hash, UtxoSet};
    use crossbeam_channel::unbounded;

    fn dummy_ready(height: u64) -> ReadyItem {
        let block = Arc::new(Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [height as u8; 32],
                timestamp: 1,
                bits: 0x0f00ffff,
                nonce: 0,
            },
            transactions: vec![].into(),
        });
        (
            height,
            block,
            Arc::new(Vec::new()),
            Vec::new(),
            rustc_hash::FxHashMap::default(),
            Vec::<Hash>::new(),
            Arc::new(UtxoSet::default()),
        )
    }

    #[test]
    fn ordered_ready_bridge_emits_in_height_order() {
        let (tx, rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(10);
        bridge.worker_complete(11, dummy_ready(11));
        assert!(rx.try_recv().is_err());
        bridge.worker_complete(10, dummy_ready(10));
        assert_eq!(rx.recv().unwrap().0, 10);
        assert_eq!(rx.recv().unwrap().0, 11);
        bridge.worker_complete(12, dummy_ready(12));
        assert_eq!(rx.recv().unwrap().0, 12);
    }
}
