//! Block feeder thread for parallel IBD.
//!
//! Drains the ready queue into a shared buffer so validation can run while the buffer fills.
//! Feeder runs on a dedicated std::thread; crossbeam recv is blocking.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};

use blvm_consensus::block;
use crossbeam_channel::Receiver;

use super::types::{FeederBufferValue, ReadyItem, FEEDER_BLOCK_BYTES_ESTIMATE};

/// Shared state between feeder thread and validation thread.
/// (map, channel_closed, current_bytes)
pub(crate) type FeederState =
    Arc<(Mutex<(BTreeMap<u64, FeederBufferValue>, bool, usize)>, Condvar)>;

/// Create new feeder state. Caller passes the Arc to both feeder and validation.
pub(crate) fn new_feeder_state() -> FeederState {
    Arc::new((
        Mutex::new((BTreeMap::new(), false, 0)),
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
        while let Ok((h, b, w, u)) = ready_rx.recv() {
            let tx_ids = block::compute_block_tx_ids(&b);
            let mut guard = feeder_state.0.lock().unwrap();
            // Allow insert when this block is the lowest (needed by validation next).
            // Without this, out-of-order prefetch fills the buffer with later heights,
            // and the block validation needs can't get in → deadlock.
            while (guard.0.len() >= feeder_buffer_limit
                || guard.2 + FEEDER_BLOCK_BYTES_ESTIMATE > feeder_buffer_bytes_limit)
                && guard
                    .0
                    .first_key_value()
                    .is_some_and(|(&min_h, _)| h >= min_h)
            {
                guard = feeder_state.1.wait(guard).unwrap();
            }
            let buffer_was_empty = guard.0.is_empty();
            guard.0.insert(h, (Arc::new(b), w, u, tx_ids));
            guard.2 += FEEDER_BLOCK_BYTES_ESTIMATE;
            #[cfg(feature = "profile")]
            if buffer_was_empty {
                let ts_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                blvm_consensus::profile_log!(
                    "[IBD_FEEDER_DELIVER] height={} ts_ms={} (buffer was empty, unblocking validation)",
                    h, ts_ms
                );
            }
            feeder_state.1.notify_one();
        }
        let mut guard = feeder_state.0.lock().unwrap();
        guard.1 = true; // channel_closed
        feeder_state.1.notify_one();
    })
}
