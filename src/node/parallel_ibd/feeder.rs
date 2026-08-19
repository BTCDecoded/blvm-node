//! Block feeder thread for parallel IBD.
//!
//! Drains the ready queue into a shared buffer so validation can run while the buffer fills.
//! Feeder runs on a dedicated std::thread; crossbeam recv is blocking.
//!
//! The buffer can be **sharded** by height (`BLVM_IBD_FEEDER_SHARDS`, default 1) so the feeder
//! thread spends less time in one global map; validation still consumes strict height order.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crossbeam_channel::{Receiver, RecvTimeoutError};

// Static buffer limits passed at startup; no dynamic recalculation needed.
use super::types::{
    FeederBufferValue, ReadyItem, SharedBlock, SharedWitnesses, estimate_block_bytes,
};

/// Height-partitioned pending blocks. With one shard this matches a single `BTreeMap`.
pub(crate) struct FeederBuffer {
    shards: Vec<BTreeMap<u64, FeederBufferValue>>,
}

impl FeederBuffer {
    pub(crate) fn new(shard_count: usize) -> Self {
        let n = shard_count.max(1);
        Self {
            shards: (0..n).map(|_| BTreeMap::new()).collect(),
        }
    }

    #[inline]
    fn shard_idx(&self, height: u64) -> usize {
        (height as usize) % self.shards.len()
    }

    pub(crate) fn insert(
        &mut self,
        height: u64,
        value: FeederBufferValue,
    ) -> Option<FeederBufferValue> {
        let i = self.shard_idx(height);
        self.shards[i].insert(height, value)
    }

    pub(crate) fn remove(&mut self, height: u64) -> Option<FeederBufferValue> {
        let i = self.shard_idx(height);
        self.shards[i].remove(&height)
    }

    pub(crate) fn get(&self, height: u64) -> Option<&FeederBufferValue> {
        let i = self.shard_idx(height);
        self.shards[i].get(&height)
    }

    pub(crate) fn len(&self) -> usize {
        self.shards.iter().map(|m| m.len()).sum()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sum `estimate_block_bytes` across all shards for accurate MEM_REPORT accounting.
    /// The fixed 400 KB/block estimate underestimates post-SegWit blocks by 4–10×.
    pub(crate) fn total_bytes_estimate(&self) -> usize {
        use crate::node::parallel_ibd::types::estimate_block_bytes;
        self.shards
            .iter()
            .flat_map(|m| m.values())
            .map(|(blk, wit, ..)| estimate_block_bytes(blk, wit))
            .sum()
    }

    /// Minimum height currently buffered (any shard). Used for backpressure when the buffer is full.
    pub(crate) fn min_buffered_height(&self) -> Option<u64> {
        self.shards
            .iter()
            .filter_map(|m| m.keys().next().copied())
            .min()
    }

    /// Drop heights strictly below `floor` (already validated). Returns (count, bytes_freed).
    ///
    /// W26d: try_emit direct-feeds the tip while flush pushes ahead onto the ready channel;
    /// validation can race ahead of the channel, then late channel deliveries refill the feeder
    /// with stale heights and backpressure-block the real tip (live: min=640009 need=642564).
    pub(crate) fn prune_below(&mut self, floor: u64) -> (usize, usize) {
        // N14: split_off keeps h >= floor without allocating a stale-key Vec under the Mutex.
        let mut n = 0usize;
        let mut bytes = 0usize;
        for shard in &mut self.shards {
            let keep = shard.split_off(&floor);
            let stale = std::mem::replace(shard, keep);
            n = n.saturating_add(stale.len());
            for v in stale.into_values() {
                bytes = bytes.saturating_add(v.6);
            }
        }
        (n, bytes)
    }
}

fn feeder_shard_count() -> usize {
    std::env::var("BLVM_IBD_FEEDER_SHARDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .clamp(1, 64)
}

/// Shared state between feeder thread and validation thread.
/// `(buffer, channel_closed, total_estimated_bytes)`
pub(crate) type FeederState = Arc<(Mutex<(FeederBuffer, bool, usize)>, Condvar)>;

/// Create new feeder state. Caller passes the Arc to both feeder and validation.
pub(crate) fn new_feeder_state() -> FeederState {
    let shards = feeder_shard_count();
    Arc::new((
        Mutex::new((FeederBuffer::new(shards), false, 0)),
        Condvar::new(),
    ))
}

/// Run the feeder thread. Drains ready_rx into feeder_state.
/// Returns the JoinHandle so the caller can join when IBD completes.
///
/// `shutdown`: set by the IBD teardown path (`ibd_pipeline_shutdown`) when validation has exited
/// (normally on error). Without it the feeder can wedge teardown forever: when validation stops
/// consuming, the feeder fills its buffer and parks on the backpressure condvar, and its blocking
/// `ready_rx.recv()` never returns because the detached coordinator task still holds prefetch-channel
/// clones that keep the workers — and thus the bridge → `ready` senders — alive. `feeder_handle.join()`
/// would then block indefinitely, swallowing the real validation error (observed as the node hanging
/// with the coordinator spamming "Coordinator stall" while no progress is made). Polling the flag (via
/// `recv_timeout`) and bailing out of the backpressure wait lets the feeder exit promptly so the error
/// surfaces.
pub(crate) fn run_feeder_thread(
    ready_rx: Receiver<ReadyItem>,
    feeder_state: FeederState,
    feeder_buffer_limit: usize,
    feeder_buffer_bytes_limit: usize,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // `b: SharedBlock = Arc<Block>` and `w: SharedWitnesses = Arc<Vec<Vec<Witness>>>` —
        // block bytes were allocated once at download; all pipeline stages share this Arc.
        loop {
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            // Bounded wait so the feeder periodically re-checks `shutdown` even when no items
            // arrive (channel still open because the coordinator holds sender clones).
            let (h, b, w, keys, u, tx_ids, spec_adds) =
                match ready_rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(item) => item,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
            let est_bytes = estimate_block_bytes(b.as_ref(), w.as_ref());
            let mut guard = feeder_state.0.lock();
            while (guard.0.len() >= feeder_buffer_limit
                || guard.2 + est_bytes > feeder_buffer_bytes_limit)
                && guard
                    .0
                    .min_buffered_height()
                    .is_some_and(|min_h| h >= min_h)
                && !shutdown.load(Ordering::Acquire)
            {
                // Time-bounded so a teardown `notify` is never missed even if it races the wait.
                feeder_state
                    .1
                    .wait_for(&mut guard, Duration::from_millis(250));
            }
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            let buffer_was_empty = guard.0.is_empty();
            guard
                .0
                .insert(h, (b, w, keys, u, tx_ids, spec_adds, est_bytes));
            guard.2 += est_bytes;
            // W26: keep TIP_CRAWL / gap_in_pipeline metrics honest — channel-path inserts
            // previously left IBD_FEEDER_BUFFER_BLOCKS stale at 0 while the buffer held tip.
            crate::node::parallel_ibd::IBD_FEEDER_BUFFER_BLOCKS
                .store(guard.0.len(), std::sync::atomic::Ordering::Relaxed);
            crate::node::parallel_ibd::tip_stage::mark_feeder(h);
            #[cfg(feature = "profile")]
            if buffer_was_empty {
                let ts_ms = crate::utils::time::current_timestamp_millis();
                blvm_protocol::profile_log!(
                    "[IBD_FEEDER_DELIVER] height={} ts_ms={} (buffer was empty, unblocking validation)",
                    h,
                    ts_ms
                );
            }
            // N13: tip-aware notify — ahead-only inserts skip wake (flush storms).
            // Synth bisect always-notify did not recover S10 champ (still ~189); keep N13.
            let wait_tip = crate::node::parallel_ibd::IBD_FEEDER_WAIT_TIP
                .load(std::sync::atomic::Ordering::Relaxed);
            if buffer_was_empty || h == wait_tip || wait_tip == 0 {
                feeder_state.1.notify_one();
            }
        }
        let mut guard = feeder_state.0.lock();
        guard.1 = true; // channel_closed
        feeder_state.1.notify_all();
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use blvm_protocol::{Block, BlockHeader};

    fn dummy_feeder_value(height: u64) -> FeederBufferValue {
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
            block,
            Arc::new(Vec::new()),
            Vec::new(),
            Arc::new(rustc_hash::FxHashMap::default()),
            Vec::new(),
            Arc::new(blvm_consensus::types::UtxoSet::default()),
            100,
        )
    }

    #[test]
    fn feeder_buffer_routes_by_height_modulo_shards() {
        let buf = FeederBuffer::new(3);
        assert_eq!(buf.shard_idx(0), 0);
        assert_eq!(buf.shard_idx(1), 1);
        assert_eq!(buf.shard_idx(3), 0);
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn feeder_buffer_insert_remove_and_min_height() {
        let mut buf = FeederBuffer::new(2);
        buf.insert(5, dummy_feeder_value(5));
        buf.insert(2, dummy_feeder_value(2));
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.min_buffered_height(), Some(2));
        assert!(buf.get(5).is_some());
        buf.remove(2);
        assert_eq!(buf.min_buffered_height(), Some(5));
        assert!(!buf.is_empty());
        buf.remove(5);
        assert!(buf.is_empty());
    }

    #[test]
    fn feeder_buffer_prune_below_drops_stale_keeps_tip_and_ahead() {
        let mut buf = FeederBuffer::new(2);
        for h in [10, 11, 12, 13] {
            buf.insert(h, dummy_feeder_value(h));
        }
        let (n, bytes) = buf.prune_below(12);
        assert_eq!(n, 2);
        assert_eq!(bytes, 200);
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.min_buffered_height(), Some(12));
        assert!(buf.get(11).is_none());
        assert!(buf.get(12).is_some());
        assert!(buf.get(13).is_some());
    }

    #[test]
    fn feeder_shard_count_defaults_and_clamps() {
        unsafe {
            std::env::remove_var("BLVM_IBD_FEEDER_SHARDS");
        }
        assert_eq!(feeder_shard_count(), 1);
        unsafe {
            std::env::set_var("BLVM_IBD_FEEDER_SHARDS", "999");
        }
        assert_eq!(feeder_shard_count(), 64);
        unsafe {
            std::env::remove_var("BLVM_IBD_FEEDER_SHARDS");
        }
    }
}
