//! Retire dispatcher: 1..N retire threads sharded by `height % N`.
//!
//! **Why this exists.** The original retire path was a single thread consuming an
//! `Arc<Mutex<BTreeMap<u64, Arc<UtxoDelta>>>>` plus an `mpsc::channel<IbdRetireWork>`.
//! Per-block bookkeeping (eviction, dynamic-protect, flush decisions) plus the periodic
//! flush trigger could not scale past one core, even though the underlying store, the
//! pending log, and the muhash accumulator all support concurrent updates.
//!
//! **Design.**
//! - `N` retire threads, each with its own `mpsc::channel<IbdRetireWork>`.
//! - Producer dispatches via [`RetireDispatcher::send`], which routes to shard
//!   `height % N`.
//! - Each shard tracks its own `local_last_retired` atomic; the public
//!   [`RetireDispatcher::global_last_retired`] is the **min** across all shards (the
//!   highest height that has been retired by *every* shard, the contiguously-retired
//!   floor). Logic that needs a contiguous floor reads `global_last_retired`.
//! - Flush packages are still spawned per-shard via the existing
//!   `push_utxo_flush_from_retire`. Workers populate the pending log independently of
//!   retire, so `take_flush_batch_through(local_h)` is safe to call concurrently from
//!   multiple shards: the only shared mutable state in the flush path is the `mh_acc`
//!   mutex (already serialized) and the `utxo_flush_handles` queue (already
//!   mutex-guarded).
//!
//! **Defaults.** [`configured_retire_shards`] returns `1` unless
//! `BLVM_IBD_RETIRE_SHARDS=N` is set with `N>=2`, which is byte-for-byte the original
//! single-threaded behavior. `>=2` opts into the sharded path; values larger than
//! `available_parallelism()/2` are clamped because each shard contends on `mem_mtx`
//! during the apply call. Practical sweet-spot is `2..=4` on most hosts.
//!
//! **Correctness invariants.**
//! 1. Workers push pending ops for height `h` *before* the dispatcher sends
//!    `IbdRetireWork{height=h}`. Therefore any shard calling
//!    `take_flush_batch_through(local_h)` only sees ops that workers already finished
//!    pushing — `local_h` may exceed `global_last_retired` without losing data.
//! 2. The chain UTXO watermark advances based on the per-flush-package
//!    `max_block_height`, not on `global_last_retired`. This means a fast shard's flush
//!    can advance the watermark past a slower shard's cursor; that is *correct* because
//!    the worker-side production is what the watermark actually depends on.
//! 3. `staged.remove(&h)` runs on the shard that owns `h` (per the modulo). No two
//!    shards ever touch the same staged entry.
//!
//! **Shutdown.** Dropping the dispatcher drops all senders, which causes each shard's
//! `mpsc::recv_timeout` to return `Disconnected` on the next tick and the thread to
//! exit cleanly. [`RetireDispatcher::shutdown_and_join`] waits for every shard.

use anyhow::Result;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use super::IbdRetireWork;

/// Public entry point used by `validation_loop` in place of the bare `mpsc::Sender`.
pub(crate) struct RetireDispatcher {
    shards: Vec<DispatcherShard>,
    /// Highest height retired by **every** shard (= min of `local_last_retired`). This is
    /// the contiguously-retired floor; existing call-sites that expect a monotonically
    /// non-decreasing "we are at least here" cursor read this atomic.
    global_last_retired: Arc<AtomicU64>,
}

struct DispatcherShard {
    tx: Option<mpsc::Sender<IbdRetireWork>>,
    handle: Option<JoinHandle<()>>,
}

impl RetireDispatcher {
    /// Create N senders + N background threads. Caller provides one `spawn_thread` per
    /// shard, given `(shard_index, work_rx, local_last_retired, publisher)`. Each
    /// thread is expected to drive its retire loop using the supplied
    /// `local_last_retired` as its progress atomic; on every advance the loop should
    /// also call `publisher.publish(&local, h)` to refresh `global_last_retired`.
    /// (See `validation_loop::run_ibd_retire_loop_*` for the concrete callers.)
    ///
    /// `start_height_minus_one` is the seed value for both `local_last_retired` and
    /// `global_last_retired` — same convention as the existing `last_retired` atomic.
    pub fn spawn<F>(num_shards: usize, start_height_minus_one: u64, mut spawn_thread: F) -> Self
    where
        F: FnMut(
            usize,
            mpsc::Receiver<IbdRetireWork>,
            Arc<AtomicU64>,
            Arc<GlobalProgressPublisher>,
        ) -> JoinHandle<()>,
    {
        let n = num_shards.max(1);
        let global_last_retired = Arc::new(AtomicU64::new(start_height_minus_one));
        let local_cursors: Vec<Arc<AtomicU64>> = (0..n)
            .map(|_| Arc::new(AtomicU64::new(start_height_minus_one)))
            .collect();
        let publisher = Arc::new(GlobalProgressPublisher {
            locals: local_cursors.clone(),
            global: Arc::clone(&global_last_retired),
            recompute_lock: Mutex::new(()),
        });

        let mut shards = Vec::with_capacity(n);
        for (i, cursor) in local_cursors.iter().enumerate().take(n) {
            let (tx, rx) = mpsc::channel::<IbdRetireWork>();
            let handle = spawn_thread(i, rx, Arc::clone(cursor), Arc::clone(&publisher));
            shards.push(DispatcherShard {
                tx: Some(tx),
                handle: Some(handle),
            });
        }

        Self {
            shards,
            global_last_retired,
        }
    }

    /// Number of retire shards (always >= 1).
    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }

    /// Route `work` to its owning shard (height modulo num_shards). On a dropped or
    /// crashed shard, returns `SendError` exactly like the original `mpsc::Sender::send`.
    pub fn send(
        &self,
        work: IbdRetireWork,
    ) -> std::result::Result<(), mpsc::SendError<IbdRetireWork>> {
        let i = (work.height as usize) % self.shards.len();
        match &self.shards[i].tx {
            Some(tx) => tx.send(work),
            // Shard already shut down — surface like a closed channel so callers go
            // through the same recovery path as a crashed retire thread.
            None => Err(mpsc::SendError(work)),
        }
    }

    /// Shared atomic that converges to `min(local_last_retired)` — the
    /// contiguously-retired floor, suitable for `staged` drain logic and similar.
    pub fn global_last_retired(&self) -> &Arc<AtomicU64> {
        &self.global_last_retired
    }

    /// Drop all senders, then join every retire thread. Per-shard error mutexes (passed
    /// in by the caller via `spawn_thread`) must be inspected separately — this method
    /// only guarantees that no retire thread is still running on return.
    pub fn shutdown_and_join(&mut self) -> Result<()> {
        for s in self.shards.iter_mut() {
            s.tx.take();
        }
        for s in self.shards.iter_mut() {
            if let Some(h) = s.handle.take() {
                let _ = h.join();
            }
        }
        Ok(())
    }
}

impl Drop for RetireDispatcher {
    fn drop(&mut self) {
        // Best-effort: ensure no retire thread outlives us silently. Errors from join()
        // are swallowed because Drop has nowhere to surface them; explicit shutdown via
        // `shutdown_and_join` is the path production code uses.
        for s in self.shards.iter_mut() {
            s.tx.take();
        }
        for s in self.shards.iter_mut() {
            if let Some(h) = s.handle.take() {
                let _ = h.join();
            }
        }
    }
}

/// Publishes per-shard progress and recomputes the global min. Called from every retire
/// thread on each height advance — must be cheap and lock-friendly.
pub(crate) struct GlobalProgressPublisher {
    locals: Vec<Arc<AtomicU64>>,
    global: Arc<AtomicU64>,
    /// Serializes the min-reduction so two shards racing on `publish` don't compute and
    /// store stale mins out of order. Held for `O(N)` atomic loads — fine for `N <= 8`.
    recompute_lock: Mutex<()>,
}

impl GlobalProgressPublisher {
    /// Update `local` to `h`, then recompute `global = min(locals)` and publish it.
    /// Safe to call from any retire thread; the min is monotonic across calls because
    /// each `local` is only ever written from its owning thread and only ever advances.
    pub fn publish(&self, local: &AtomicU64, h: u64) {
        local.store(h, Ordering::Release);
        let _g = self.recompute_lock.lock();
        let mut m = u64::MAX;
        for l in &self.locals {
            let v = l.load(Ordering::Acquire);
            if v < m {
                m = v;
            }
        }
        if m != u64::MAX {
            self.global.store(m, Ordering::Release);
        }
    }
}

/// Read `BLVM_IBD_RETIRE_SHARDS`. Defaults to 1 (= original single-threaded retire);
/// values are clamped to `[1, available_parallelism / 2]` so every shard has at least
/// one validation worker behind it. `0` and unparseable values map to 1.
pub(crate) fn configured_retire_shards() -> usize {
    let raw: usize = std::env::var("BLVM_IBD_RETIRE_SHARDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    if raw <= 1 {
        return 1;
    }
    let cap = std::thread::available_parallelism()
        .map(|p| (p.get() / 2).max(1))
        .unwrap_or(1);
    raw.min(cap).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `publisher.publish` must always store `min(locals)` to global. Specifically,
    /// when a fast shard advances first, the global must NOT track its progress until
    /// every slower shard has caught up.
    #[test]
    fn publisher_global_tracks_min_not_max() {
        let local0 = Arc::new(AtomicU64::new(0));
        let local1 = Arc::new(AtomicU64::new(0));
        let global = Arc::new(AtomicU64::new(0));
        let publisher = GlobalProgressPublisher {
            locals: vec![Arc::clone(&local0), Arc::clone(&local1)],
            global: Arc::clone(&global),
            recompute_lock: Mutex::new(()),
        };

        publisher.publish(&local0, 100);
        // shard 1 still at 0 → global must remain 0
        assert_eq!(global.load(Ordering::Acquire), 0);
        assert_eq!(local0.load(Ordering::Acquire), 100);

        publisher.publish(&local1, 50);
        // global is now min(100, 50) = 50
        assert_eq!(global.load(Ordering::Acquire), 50);

        publisher.publish(&local1, 200);
        // global is now min(100, 200) = 100 (shard 0 is the floor)
        assert_eq!(global.load(Ordering::Acquire), 100);

        publisher.publish(&local0, 300);
        // global is now min(300, 200) = 200 (shard 1 became the floor)
        assert_eq!(global.load(Ordering::Acquire), 200);
    }

    /// N=1: publisher reduces over a single local; `global == local` at all times.
    #[test]
    fn publisher_n1_global_equals_local() {
        let local = Arc::new(AtomicU64::new(0));
        let global = Arc::new(AtomicU64::new(0));
        let publisher = GlobalProgressPublisher {
            locals: vec![Arc::clone(&local)],
            global: Arc::clone(&global),
            recompute_lock: Mutex::new(()),
        };
        for h in [1u64, 5, 17, 100, 1_000_000].iter().copied() {
            publisher.publish(&local, h);
            assert_eq!(global.load(Ordering::Acquire), h);
            assert_eq!(local.load(Ordering::Acquire), h);
        }
    }

    /// `configured_retire_shards()` must default to 1 with no env var, and must clamp
    /// to `available_parallelism / 2` for sane values.
    #[test]
    fn configured_retire_shards_defaults_and_clamps() {
        // Each test that mutates BLVM_IBD_RETIRE_SHARDS must serialize on this lock so
        // the env var doesn't leak into other tests running in parallel. The env reads
        // happen inside this lock too, since cargo runs tests in parallel by default.
        let _guard = ENV_LOCK.lock();

        unsafe {
            std::env::remove_var("BLVM_IBD_RETIRE_SHARDS");
        }
        assert_eq!(configured_retire_shards(), 1, "default must be 1");

        unsafe {
            std::env::set_var("BLVM_IBD_RETIRE_SHARDS", "0");
        }
        assert_eq!(configured_retire_shards(), 1, "0 must clamp to 1");

        unsafe {
            std::env::set_var("BLVM_IBD_RETIRE_SHARDS", "1");
        }
        assert_eq!(configured_retire_shards(), 1);

        unsafe {
            std::env::set_var("BLVM_IBD_RETIRE_SHARDS", "garbage");
        }
        assert_eq!(configured_retire_shards(), 1, "unparseable must clamp to 1");

        // For values >=2, exact clamp depends on host's available_parallelism, but the
        // result must always be in [1, available_parallelism / 2] and >= 1.
        unsafe {
            std::env::set_var("BLVM_IBD_RETIRE_SHARDS", "999");
        }
        let n = configured_retire_shards();
        assert!(n >= 1);
        let cap = std::thread::available_parallelism()
            .map(|p| (p.get() / 2).max(1))
            .unwrap_or(1);
        assert_eq!(n, cap, "999 must clamp to available_parallelism / 2");

        unsafe {
            std::env::remove_var("BLVM_IBD_RETIRE_SHARDS");
        }
    }

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Work at height `h` must land on shard `h % num_shards`.
    #[test]
    fn dispatcher_routes_work_by_height_modulo_shards() {
        use super::super::IbdRetireWork;
        use crate::Block;
        use std::sync::Mutex;

        let dummy_block = Arc::new(Block {
            header: blvm_protocol::BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1,
                bits: 0x0f00ffff,
                nonce: 0,
            },
            transactions: vec![].into_boxed_slice(),
        });

        let routes: Arc<Mutex<Vec<(usize, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let routes_clone = Arc::clone(&routes);
        let mut dispatcher = RetireDispatcher::spawn(3, 0, move |shard, rx, _local, _pub| {
            let routes = Arc::clone(&routes_clone);
            std::thread::spawn(move || {
                while let Ok(work) = rx.recv() {
                    routes.lock().unwrap().push((shard, work.height));
                }
            })
        });

        for height in 0..9u64 {
            let work = IbdRetireWork {
                height,
                blocks_buf: vec![Arc::clone(&dummy_block)],
                block: Arc::clone(&dummy_block),
            };
            dispatcher.send(work).unwrap();
        }
        dispatcher.shutdown_and_join().unwrap();

        let mut seen = routes.lock().unwrap().clone();
        seen.sort_by_key(|(shard, h)| (*h, *shard));
        assert_eq!(seen.len(), 9);
        for (shard, height) in seen {
            assert_eq!(shard, height as usize % 3);
        }
    }
}
