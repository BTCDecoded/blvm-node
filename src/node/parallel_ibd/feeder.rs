//! Block feeder thread for parallel IBD.
//!
//! Drains the ready queue into a shared buffer so validation can run while the buffer fills.
//! Feeder runs on a dedicated std::thread; crossbeam recv is blocking.
//!
//! The buffer can be **sharded** by height (`BLVM_IBD_FEEDER_SHARDS`, default 1) so the feeder
//! thread spends less time in one global map; validation still consumes strict height order.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::{Condvar, Mutex};

use crossbeam_channel::Receiver;

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

    /// Minimum height currently buffered (any shard). Used for backpressure when the buffer is full.
    pub(crate) fn min_buffered_height(&self) -> Option<u64> {
        self.shards
            .iter()
            .filter_map(|m| m.keys().next().copied())
            .min()
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
pub(crate) fn run_feeder_thread(
    ready_rx: Receiver<ReadyItem>,
    feeder_state: FeederState,
    feeder_buffer_limit: usize,
    feeder_buffer_bytes_limit: usize,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // `b: SharedBlock = Arc<Block>` and `w: SharedWitnesses = Arc<Vec<Vec<Witness>>>` —
        // block bytes were allocated once at download; all pipeline stages share this Arc.
        while let Ok((h, b, w, keys, u, tx_ids, spec_adds)) = ready_rx.recv() {
            let est_bytes = estimate_block_bytes(b.as_ref(), w.as_ref());
            let mut guard = feeder_state.0.lock();
            while (guard.0.len() >= feeder_buffer_limit
                || guard.2 + est_bytes > feeder_buffer_bytes_limit)
                && guard
                    .0
                    .min_buffered_height()
                    .is_some_and(|min_h| h >= min_h)
            {
                feeder_state.1.wait(&mut guard);
            }
            let buffer_was_empty = guard.0.is_empty();
            guard
                .0
                .insert(h, (b, w, keys, u, tx_ids, spec_adds, est_bytes));
            guard.2 += est_bytes;
            #[cfg(feature = "profile")]
            if buffer_was_empty {
                let ts_ms = crate::utils::time::current_timestamp_millis();
                blvm_protocol::profile_log!(
                    "[IBD_FEEDER_DELIVER] height={} ts_ms={} (buffer was empty, unblocking validation)",
                    h,
                    ts_ms
                );
            }
            feeder_state.1.notify_one();
        }
        let mut guard = feeder_state.0.lock();
        guard.1 = true; // channel_closed
        feeder_state.1.notify_one();
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
            rustc_hash::FxHashMap::default(),
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
