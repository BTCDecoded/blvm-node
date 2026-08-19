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
use std::sync::LazyLock;

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

use super::types::{PrefetchWorkItemV2, PrefetchedUtxoMap, ReadyItem};

#[cfg(feature = "production")]
static ENGINE_EMPTY_PREFETCH_ARC: LazyLock<PrefetchedUtxoMap> =
    LazyLock::new(|| Arc::new(FxHashMap::default()));

#[cfg(feature = "production")]
static ENGINE_EMPTY_SPEC_ADDS: LazyLock<Arc<UtxoSet>> =
    LazyLock::new(|| Arc::new(UtxoSet::default()));

/// Shared empty prefetch map for engine-mode `ReadyItem` field 5 (no per-block alloc).
#[cfg(feature = "production")]
pub(crate) fn engine_empty_prefetch_arc() -> PrefetchedUtxoMap {
    Arc::clone(&ENGINE_EMPTY_PREFETCH_ARC)
}

/// Shared empty spec-adds set for engine-mode `ReadyItem` field 6.
#[cfg(feature = "production")]
pub(crate) fn engine_empty_spec_adds() -> Arc<UtxoSet> {
    Arc::clone(&ENGINE_EMPTY_SPEC_ADDS)
}

/// Reorders prefetch completions so the feeder always receives blocks in ascending height.
/// Parallel workers finish UTXO loads out of order; without this, `ready_tx` can deliver N+k
/// before N and validation stalls (feeder min_height > next_validation_height).
#[cfg(feature = "production")]
pub(crate) struct OrderedReadyBridge {
    inner: Mutex<OrderedReadyInner>,
    out: Sender<ReadyItem>,
    /// W56b: when set, in-order emits go straight into the feeder buffer. Advancing
    /// `next_expected` via the ready channel while items were still on `ready_rx` caused
    /// false "lost ready item" rewinds (live W56: still ~6.7 blk/s, REWIND ~0.8/tip).
    feeder: Mutex<Option<super::feeder::FeederState>>,
}

#[cfg(feature = "production")]
struct OrderedReadyInner {
    /// Next height we may emit to `out` (set on first `coordinator_will_send_height`).
    next_expected: Option<u64>,
    pending: BTreeMap<u64, ReadyItem>,
}

#[cfg(feature = "production")]
fn sync_bridge_pending_count(g: &OrderedReadyInner) {
    super::memory::BRIDGE_PENDING_COUNT.store(g.pending.len() as u64, Ordering::Relaxed);
    let next = g.next_expected.unwrap_or(u64::MAX);
    super::memory::BRIDGE_NEXT_EXPECTED.store(next, Ordering::Relaxed);
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
            feeder: Mutex::new(None),
        }
    }

    /// Attach feeder so `flush` / `worker_complete` advance the cursor only after durable insert.
    pub(crate) fn attach_feeder(&self, feeder: super::feeder::FeederState) {
        *self
            .feeder
            .lock()
            .expect("OrderedReadyBridge feeder mutex poisoned") = Some(feeder);
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
    ///
    /// Sends are performed **while holding the bridge mutex**. This is mandatory: it is the only
    /// thing that guarantees the `out` channel receives heights in strict ascending order. If the
    /// lock were released before sending (so two workers could race their `out.send()` calls), a
    /// worker that drained a *higher* contiguous range could push its blocks into the bounded
    /// `ready` channel ahead of the worker carrying the lower block that validation is waiting on.
    /// The feeder's backpressure only lets a block bypass a full buffer when it is below
    /// `min_buffered_height` (feeder.rs); out-of-order delivery therefore traps the needed low
    /// block behind higher ones and wedges validation permanently (observed as the h=575388 IBD
    /// stall: validation_height frozen, reorder_buf_len=0, next_prefetch racing ahead).
    ///
    /// Holding the lock across the (possibly blocking) `out.send()` is safe and cannot deadlock:
    /// because delivery is strictly ascending, the block the feeder consumes next is always the
    /// lowest unconsumed height, which is exactly the one validation needs — the feeder's
    /// `h < min_buffered_height` escape lets it accept that block even when its buffer is full, so
    /// the feeder always drains the channel and unblocks the send.
    /// `validation_height` is the last applied height (`next_needed - 1`). Pass `0` in tests
    /// when the bridge cursor is already primed via [`Self::coordinator_will_send_height`].
    pub(crate) fn worker_complete(&self, h: u64, item: ReadyItem, validation_height: u64) {
        let mut g = self
            .inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned");
        // W22/W40: drop duplicate/late completions only when clearly stale. If this height is
        // still validation's tip (or cursor is uninitialized), rewind/init instead of silent
        // drop — live soft-resume: GAP_STREAM → worker_complete drop → feeder=0 stall.
        // `validation_height == 0` → test/legacy path (cursor already primed).
        let next_needed = if validation_height > 0 {
            validation_height.saturating_add(1)
        } else {
            0
        };
        if g.next_expected.is_none() {
            g.next_expected = Some(if next_needed > 0 { next_needed } else { h });
        }
        if g.next_expected.is_some_and(|n| h < n) {
            let still_tip = next_needed > 0 && h == next_needed;
            if still_tip && !super::tip_stage::tip_taken_by_validation(h) {
                tracing::warn!(
                    "[IBD_BRIDGE_TIP_REWIND_COMPLETE] h={} next_expected={:?} next_needed={} — keeping tip",
                    h,
                    g.next_expected,
                    next_needed
                );
                g.next_expected = Some(h);
            } else {
                sync_bridge_pending_count(&g);
                return;
            }
        }
        // Fast path: in-order completion with no backlog — skip BTreeMap insert.
        if g.pending.is_empty() {
            if let Some(n) = g.next_expected {
                if h == n {
                    if let Some(feeder) = self.feeder_attached() {
                        // W56b: durable feeder insert before cursor advance.
                        Self::insert_ready_into_feeder(&feeder, item);
                        g.next_expected = Some(n + 1);
                        sync_bridge_pending_count(&g);
                        return;
                    }
                    match self.out.try_send(item) {
                        Ok(()) => {
                            g.next_expected = Some(n + 1);
                            sync_bridge_pending_count(&g);
                            return;
                        }
                        Err(crossbeam_channel::TrySendError::Full(item))
                        | Err(crossbeam_channel::TrySendError::Disconnected(item)) => {
                            // Park in pending; a later try_flush / repair will deliver.
                            g.pending.insert(h, item);
                            sync_bridge_pending_count(&g);
                            return;
                        }
                    }
                }
            }
        }
        g.pending.insert(h, item);
        self.flush_pending_unlocked(&mut g);
        sync_bridge_pending_count(&g);
    }

    fn feeder_attached(&self) -> Option<super::feeder::FeederState> {
        self.feeder
            .lock()
            .expect("OrderedReadyBridge feeder mutex poisoned")
            .clone()
    }

    /// Drain contiguous pending — prefer feeder (W56b), else ready channel.
    fn flush_pending_unlocked(&self, g: &mut OrderedReadyInner) {
        if let Some(feeder) = self.feeder_attached() {
            Self::flush_contiguous_to_feeder(&feeder, g);
        } else {
            Self::flush_unlocked(&self.out, g);
        }
    }

    /// W22: if validation still needs `h` but the cursor advanced past it while `h` sits in
    /// pending (duplicate completion race), rewind and flush so the stranded tip can drain.
    ///
    /// If the cursor is ahead and tip is *not* in pending, rewind only — caller must requeue
    /// download (tip was emitted to the ready channel but never reached validation/feeder).
    pub(crate) fn recover_stranded_tip(&self, next_needed: u64) -> bool {
        let mut g = self
            .inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned");
        let Some(n) = g.next_expected else {
            return false;
        };
        if n <= next_needed {
            return false;
        }
        if g.pending.contains_key(&next_needed) {
            tracing::warn!(
                "[IBD_BRIDGE_REWIND] next_expected={} → {} (stranded tip in pending, pending={})",
                n,
                next_needed,
                g.pending.len()
            );
            g.next_expected = Some(next_needed);
            self.flush_pending_unlocked(&mut g);
            sync_bridge_pending_count(&g);
            return true;
        }
        false
    }

    /// W22: rewind cursor when tip was emitted (`next_expected > next_needed`) but never
    /// arrived at validation (feeder empty, not in pending). Allows a fresh handoff/re-download.
    pub(crate) fn rewind_cursor_to(&self, next_needed: u64) -> bool {
        let mut g = self
            .inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned");
        let Some(n) = g.next_expected else {
            return false;
        };
        if n <= next_needed {
            return false;
        }
        tracing::warn!(
            "[IBD_BRIDGE_REWIND] next_expected={} → {} (tip missing from pending+feeder — lost ready item)",
            n,
            next_needed
        );
        g.next_expected = Some(next_needed);
        // Drop pending below new cursor floor — they can never flush usefully.
        let stale: Vec<u64> = g
            .pending
            .keys()
            .filter(|&&h| h < next_needed)
            .copied()
            .collect();
        for h in stale {
            g.pending.remove(&h);
        }
        sync_bridge_pending_count(&g);
        true
    }

    /// W26b: validation already past `next_expected` (cursor behind). Advance cursor and drop
    /// obsolete pending below the floor so tip handoff can emit `next_needed`.
    ///
    /// Live W26 stall: aggressive rewind set next_expected 640066→640001 while validation was
    /// already at 640065; tip 640066 sat in pending forever (flush waits for 640001).
    pub(crate) fn fast_forward_cursor_to(&self, next_needed: u64) -> bool {
        let mut g = self
            .inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned");
        let Some(n) = g.next_expected else {
            g.next_expected = Some(next_needed);
            sync_bridge_pending_count(&g);
            return true;
        };
        if n >= next_needed {
            return false;
        }
        tracing::warn!(
            "[IBD_BRIDGE_FF] next_expected={} → {} (validation ahead of bridge)",
            n,
            next_needed
        );
        g.next_expected = Some(next_needed);
        let stale: Vec<u64> = g
            .pending
            .keys()
            .filter(|&&h| h < next_needed)
            .copied()
            .collect();
        for h in stale {
            g.pending.remove(&h);
        }
        self.flush_pending_unlocked(&mut g);
        sync_bridge_pending_count(&g);
        true
    }

    /// W23/W26/W39: emit in-order height directly into the feeder buffer, bypassing the bounded
    /// ready channel. Live WAN: tip was `GAP_STREAM`ed 164× in 42s while `bridge_next`
    /// ran ahead of `next_needed` with `feeder=0` — ReadyItems sat/lost on the channel hop.
    ///
    /// W26: direct-emit whenever `h == next_expected`, **even if pending is non-empty**.
    /// The old `pending.is_empty()` gate forced tip through `worker_complete` → ready channel
    /// whenever any ahead block was buffered — live soak: tip in reorder + `feeder=0` +
    /// `bridge_next==need` ~30% of WAN wall time while handoff logged `flushed=0`.
    ///
    /// W39b: when cursor ran ahead of validation, tip re-downloads were dropped as
    /// `h < next_expected` (live soft-resume: 4.8k RESEND of tip while feeder held ahead only).
    /// If `h == validation_height + 1`, rewind and emit instead of discarding.
    ///
    /// Returns `None` if handled (emitted, or duplicate below cursor dropped). Returns
    /// `Some(item)` if the caller must use [`Self::worker_complete`] (out-of-order / backlog).
    pub(crate) fn try_emit_in_order_to_feeder(
        &self,
        h: u64,
        item: ReadyItem,
        feeder: &super::feeder::FeederState,
        validation_height: u64,
    ) -> Option<ReadyItem> {
        let mut g = self
            .inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned");
        let next_needed = validation_height.saturating_add(1);
        // W40: uninitialized cursor — bind to tip so first emit is not parked forever in
        // pending (flush_unlocked no-ops when next_expected is None).
        if g.next_expected.is_none() {
            g.next_expected = Some(next_needed);
        }
        if g.next_expected.is_some_and(|n| h < n) {
            if h == next_needed {
                // Already in feeder, or taken into validation while vh atomic lags retire:
                // do not re-insert (live: REEMIT storm at ~200Hz with feeder/validation busy).
                let in_feeder = feeder.0.lock().0.get(h).is_some();
                if in_feeder || super::tip_stage::tip_taken_by_validation(h) {
                    sync_bridge_pending_count(&g);
                    return None;
                }
                tracing::warn!(
                    "[IBD_BRIDGE_TIP_REEMIT] h={} next_expected={:?} — cursor ahead of validation, re-emitting tip",
                    h,
                    g.next_expected
                );
                g.next_expected = Some(h);
            } else {
                sync_bridge_pending_count(&g);
                return None;
            }
        }
        if g.next_expected != Some(h) {
            return Some(item);
        }
        debug_assert_eq!(item.0, h);
        Self::insert_ready_into_feeder(feeder, item);
        g.next_expected = Some(h + 1);
        // W56: drain contiguous pending *into the feeder*, not the ready channel.
        // Channel flush advanced `next_expected` while items were still in-flight on
        // ready_rx — live W55 true-WAN ~466k @ ~6.5 blk/s: BRIDGE_REWIND/TIP_REWIND
        // storms ("lost ready item"), REEMIT, feeder0≈67%, bmin>>tip. Cursor must not
        // outrun durable feeder presence.
        Self::flush_contiguous_to_feeder(feeder, &mut g);
        sync_bridge_pending_count(&g);
        None
    }

    fn insert_ready_into_feeder(feeder: &super::feeder::FeederState, item: ReadyItem) {
        let (hh, b, w, keys, u, tx_ids, spec_adds) = item;
        let est = super::types::estimate_block_bytes(b.as_ref(), w.as_ref());
        let wait_tip = super::IBD_FEEDER_WAIT_TIP.load(Ordering::Relaxed);
        let should_wake = {
            let mut fg = feeder.0.lock();
            // In-order tip always bypasses feeder backpressure (same rule as feeder thread
            // when `h < min_buffered_height` or buffer empty).
            let was_empty = fg.0.is_empty();
            fg.0.insert(hh, (b, w, keys, u, tx_ids, spec_adds, est));
            fg.2 = fg.2.saturating_add(est);
            super::IBD_FEEDER_BUFFER_BLOCKS.store(fg.0.len(), Ordering::Relaxed);
            super::tip_stage::mark_feeder(hh);
            // N13: wake only for tip / empty→nonempty (not every ahead insert).
            was_empty || hh == wait_tip || wait_tip == 0
        };
        if should_wake {
            feeder.1.notify_one();
        }
    }

    /// Emit contiguous `pending` heights starting at `next_expected` directly into the feeder.
    ///
    /// Cap burst size (default 64): unbounded flush after tip emit dumped 1500+ blocks into
    /// the feeder (live W56b soft-resume) and spiked anon while validation was still at tip.
    /// N13: batch under one lock and at most one tip Condvar notify per burst.
    fn flush_contiguous_to_feeder(feeder: &super::feeder::FeederState, g: &mut OrderedReadyInner) {
        let Some(mut n) = g.next_expected else {
            return;
        };
        let max_burst: u64 = super::latch_env!(u64, {
            std::env::var("BLVM_IBD_FEEDER_FLUSH_BURST")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(64)
                .clamp(8, 256)
        });
        let wait_tip = super::IBD_FEEDER_WAIT_TIP.load(Ordering::Relaxed);
        // N16: estimate outside the feeder lock (same as `insert_ready_into_feeder`).
        // Holding feeder.0 across full witness walks stalled tip take under burst flush.
        let mut prepared: Vec<(ReadyItem, usize)> = Vec::with_capacity(max_burst as usize);
        let mut n_scan = n;
        while (prepared.len() as u64) < max_burst {
            let Some(item) = g.pending.remove(&n_scan) else {
                break;
            };
            debug_assert_eq!(item.0, n_scan);
            let est = super::types::estimate_block_bytes(item.1.as_ref(), item.2.as_ref());
            prepared.push((item, est));
            n_scan += 1;
        }
        let flushed = prepared.len() as u64;
        let mut should_wake = false;
        if flushed > 0 {
            let mut fg = feeder.0.lock();
            let was_empty = fg.0.is_empty();
            for (item, est) in prepared {
                let (hh, b, w, keys, u, tx_ids, spec_adds) = item;
                fg.0.insert(hh, (b, w, keys, u, tx_ids, spec_adds, est));
                fg.2 = fg.2.saturating_add(est);
                super::tip_stage::mark_feeder(hh);
                if hh == wait_tip {
                    should_wake = true;
                }
            }
            super::IBD_FEEDER_BUFFER_BLOCKS.store(fg.0.len(), Ordering::Relaxed);
            if was_empty || should_wake || wait_tip == 0 {
                should_wake = true;
            }
        }
        g.next_expected = Some(n_scan);
        sync_bridge_pending_count(g);
        if flushed > 0 && should_wake {
            feeder.1.notify_one();
        }
    }

    /// Heights buffered out-of-order waiting for a gap (each holds a full `ReadyItem` / `Arc<Block>`).
    pub(crate) fn pending_len(&self) -> usize {
        self.inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned")
            .pending
            .len()
    }

    /// Whether `h` is buffered in the bridge pending map (dispatched, not yet released to feeder).
    pub(crate) fn pending_contains(&self, h: u64) -> bool {
        self.inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned")
            .pending
            .contains_key(&h)
    }

    /// Next height the bridge will emit to the feeder (None until first `coordinator_will_send_height`).
    pub(crate) fn next_expected(&self) -> Option<u64> {
        self.inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned")
            .next_expected
    }

    /// W17 diagnostics: `(next_expected, pending_len, min_pending, max_pending, holes_in_span)`.
    ///
    /// `holes_in_span` counts missing heights in `[next_expected, min(next_expected+512, max_pending)]`
    /// — the sparse-hole signature of the ~1 BPS WAN tip crawl (flush stops at first hole).
    pub(crate) fn pending_diag(&self) -> (Option<u64>, usize, Option<u64>, Option<u64>, usize) {
        let g = self
            .inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned");
        let next = g.next_expected;
        let len = g.pending.len();
        let min_h = g.pending.keys().next().copied();
        let max_h = g.pending.keys().next_back().copied();
        let holes = match (next, max_h) {
            (Some(n), Some(max)) => {
                let end = max.min(n.saturating_add(512));
                let mut holes = 0usize;
                let mut h = n;
                while h <= end {
                    if !g.pending.contains_key(&h) {
                        holes += 1;
                    }
                    h += 1;
                }
                holes
            }
            _ => 0,
        };
        (next, len, min_h, max_h, holes)
    }

    /// Re-run ascending flush. Needed when `next_expected` is already in `pending` but no further
    /// `worker_complete` arrives to trigger flush (gap-fill via LOCAL_GAP / reorder dispatch can
    /// leave the bridge idle with the gap block already buffered).
    pub(crate) fn try_flush(&self) -> usize {
        let mut g = self
            .inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned");
        let before = g.pending.len();
        self.flush_pending_unlocked(&mut g);
        before.saturating_sub(g.pending.len())
    }

    /// Whether the coordinator may dispatch height `h` into the bridge under `pending_max`.
    ///
    /// Always allows `h == next_expected` so a gap fill can drain the pending map even when
    /// already at the cap. Refuses other heights when `pending.len() >= pending_max` so
    /// out-of-order ahead blocks stay in the coordinator reorder_buffer instead of holding
    /// multi-GB of `Arc<Block>` in the bridge (observed: bridge_pending=3–4k ≈ 6–8 GB).
    pub(crate) fn may_accept_height(&self, h: u64, pending_max: usize) -> bool {
        let g = self
            .inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned");
        if g.next_expected == Some(h) {
            return true;
        }
        g.pending.len() < pending_max
    }

    /// GAP-8: evict far-ahead pending heights when the gap cannot drain.
    ///
    /// Triggers when either:
    /// - `gap_missing` (next_needed absent from reorder) **and** pending at `pending_max`, or
    /// - `next_expected` is set but absent from `pending` (live: bridge_next=H, feeder=0,
    ///   pending=64–450 ahead — never reached 512 so old cap gate never fired).
    ///
    /// **B1 under-cap:** when `next_expected` is missing from pending, evict once
    /// `pending.len() >= max(64, pending_max/4)` so crawl recovery does not wait for a full
    /// 512-slot bridge.
    ///
    /// **B1b:** when `next_expected` is missing but `max_ahead`≈`window`, all pending heights
    /// sit inside the far ceiling (`next+window`) and nothing was evicted (live WAN crawl:
    /// bridge=512 pinned, gap missing, 0 evictions). Use a tight keep band
    /// `next+min(64, window/4)` and batch-peel highest pending until there is headroom.
    ///
    /// **WAN tip crawl (2026-07-14 fix):** keep band is **`next+128`** (env
    /// `BLVM_IBD_WAN_BRIDGE_TIGHT_KEEP`), not `next+16`. Live wan-bench at ~386k with
    /// `tight_keep=16` + under-min bypass + in-window peel drove `pending→0` every tip
    /// tick (`IBD_BRIDGE_EVICT` ~7k, ~4 blk/s). In-window peel is **off** unless
    /// `BLVM_IBD_WAN_B1B_PEEL=1`. Under `min_pending`, only heights beyond the **far**
    /// ceiling (`next+window`) may be evicted.
    ///
    /// Never evicts `next_expected`. Drops `ReadyItem`s — heights may be re-downloaded.
    pub(crate) fn evict_far_ahead_pending(
        &self,
        next_needed: u64,
        window: u64,
        gap_missing: bool,
        pending_max: usize,
    ) -> usize {
        self.evict_far_ahead_pending_ex(next_needed, window, gap_missing, pending_max, false)
    }

    /// Same as [`Self::evict_far_ahead_pending`] with explicit WAN tip-crawl peel.
    pub(crate) fn evict_far_ahead_pending_ex(
        &self,
        next_needed: u64,
        window: u64,
        gap_missing: bool,
        pending_max: usize,
        wan_tip_crawl: bool,
    ) -> usize {
        let mut g = self
            .inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned");
        let next_expected_missing = g.next_expected.is_some_and(|n| !g.pending.contains_key(&n));

        // W18: always purge pending heights below the delivery floor — they can never flush
        // (flush starts at next_expected). Live W17 soak: bridge_min=640001 while tip≈6859xx
        // on 73/73 TIP_CRAWL samples — ~100 dead ReadyItems pinning the bridge.
        let floor = g
            .next_expected
            .map(|n| n.min(next_needed))
            .unwrap_or(next_needed);
        const STALE_EVICT_BATCH: usize = 64;
        let stale: Vec<u64> = g
            .pending
            .keys()
            .filter(|&&h| h < floor)
            .copied()
            .take(STALE_EVICT_BATCH)
            .collect();
        let mut evicted = stale.len();
        for h in &stale {
            g.pending.remove(h);
        }
        if !stale.is_empty() {
            sync_bridge_pending_count(&g);
            let total = super::memory::BRIDGE_EVICT_BLOCKS
                .fetch_add(evicted as u64, Ordering::Relaxed)
                + evicted as u64;
            tracing::warn!(
                "[IBD_BRIDGE_STALE_PURGE] purged {} pending below floor={} (next_needed={}, pending={}, total_evicted={})",
                stale.len(),
                floor,
                next_needed,
                g.pending.len(),
                total
            );
        }

        if !gap_missing && !next_expected_missing {
            return evicted;
        }
        // B1: under-cap threshold when next_expected is absent; full cap otherwise.
        // W19: WAN tip crawl with next_expected missing must still evict above the tight
        // ceiling even when pending ≪ 128 (live: bridge=13–59, tip in reorder, holes=25–47,
        // peel never fired → ahead sat forever while tip starved).
        let min_pending = if next_expected_missing {
            if wan_tip_crawl {
                // W34c: peel sooner on WAN tip gap — don't wait for 64+ pending slots.
                32usize.min(pending_max.max(1))
            } else {
                pending_max
                    .saturating_div(4)
                    .max(64)
                    .min(pending_max.max(1))
            }
        } else {
            pending_max
        };
        let tight_keep = if wan_tip_crawl {
            std::env::var("BLVM_IBD_WAN_BRIDGE_TIGHT_KEEP")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(128)
                .clamp(32, 512)
        } else {
            window.saturating_div(4).clamp(16, 64)
        };
        let far_ceiling = next_needed.saturating_add(window);
        let ceiling = if next_expected_missing && window >= tight_keep {
            next_needed.saturating_add(tight_keep)
        } else {
            far_ceiling
        };
        const B1B_EVICT_BATCH_MAX: usize = 32;
        // Collect above-ceiling keys first.
        let mut to_evict: Vec<u64> = g
            .pending
            .keys()
            .filter(|&&h| h > ceiling && g.next_expected != Some(h))
            .copied()
            .collect();
        let under_min = g.pending.len() < min_pending;
        if under_min {
            // Live 2026-07-14: wan tip crawl used to bypass under_min and wipe the tip
            // pipe (pending→0 every tick). Only allow under-min eviction beyond the
            // *far* admit window — never the tight tip-pipe band.
            if wan_tip_crawl && next_expected_missing && !to_evict.is_empty() {
                to_evict.retain(|&h| h > far_ceiling);
                if to_evict.is_empty() {
                    return evicted;
                }
            } else {
                return evicted;
            }
        }
        if next_expected_missing && window >= tight_keep && to_evict.len() > B1B_EVICT_BATCH_MAX {
            to_evict.sort_unstable();
            to_evict = to_evict
                .into_iter()
                .rev()
                .take(B1B_EVICT_BATCH_MAX)
                .collect();
        }
        // L2: inside-window peel opt-in for local (`BLVM_IBD_B1B_PEEL`).
        // WAN peel is separately opt-in (`BLVM_IBD_WAN_B1B_PEEL`) — default ON peel
        // destroyed the tip pipe under next_expected_missing (2026-07-14 wan-bench).
        let b1b_peel = if wan_tip_crawl {
            matches!(
                std::env::var("BLVM_IBD_WAN_B1B_PEEL").ok().as_deref(),
                Some("1") | Some("true") | Some("TRUE")
            )
        } else {
            matches!(
                std::env::var("BLVM_IBD_B1B_PEEL").ok().as_deref(),
                Some("1") | Some("true") | Some("TRUE")
            )
        };
        if b1b_peel
            && next_expected_missing
            && window >= tight_keep
            && to_evict.is_empty()
            && g.pending.len() >= min_pending
        {
            let target = min_pending
                .saturating_div(2)
                .max(tight_keep as usize)
                .min(g.pending.len().saturating_sub(1));
            let peel = g
                .pending
                .len()
                .saturating_sub(target)
                .min(B1B_EVICT_BATCH_MAX);
            let mut keys: Vec<u64> = g
                .pending
                .keys()
                .filter(|&&h| g.next_expected != Some(h))
                .copied()
                .collect();
            keys.sort_unstable();
            to_evict = keys.into_iter().rev().take(peel).collect();
        }
        let far_evicted = to_evict.len();
        for h in to_evict {
            g.pending.remove(&h);
        }
        evicted += far_evicted;
        if far_evicted > 0 {
            sync_bridge_pending_count(&g);
            let total = super::memory::BRIDGE_EVICT_BLOCKS
                .fetch_add(far_evicted as u64, Ordering::Relaxed)
                + far_evicted as u64;
            if total == far_evicted as u64 || total % 64 == 0 || wan_tip_crawl {
                tracing::warn!(
                    "[IBD_BRIDGE_EVICT] evicted {} bridge pending block(s) (next_needed={}, pending={}, ceiling={}, gap_missing={}, next_expected_missing={}, min_pending={}, tight_keep={}, wan_tip_crawl={}, total_evicted={})",
                    far_evicted,
                    next_needed,
                    g.pending.len(),
                    ceiling,
                    gap_missing,
                    next_expected_missing,
                    min_pending,
                    tight_keep,
                    wan_tip_crawl,
                    total
                );
            }
        } else if wan_tip_crawl && g.pending.len() >= min_pending && next_expected_missing {
            static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let prev = LAST.load(std::sync::atomic::Ordering::Relaxed);
            if now.saturating_sub(prev) >= 5 {
                LAST.store(now, std::sync::atomic::Ordering::Relaxed);
                let min_h = g.pending.keys().next().copied();
                let max_h = g.pending.keys().next_back().copied();
                tracing::warn!(
                    "[IBD_BRIDGE_EVICT_SKIP] next_needed={} pending={} ceiling={} min={:?} max={:?} gap_missing={} next_expected_missing={} (no keys above ceiling; peel empty)",
                    next_needed,
                    g.pending.len(),
                    ceiling,
                    min_h,
                    max_h,
                    gap_missing,
                    next_expected_missing
                );
            }
        }
        evicted
    }

    /// Send all consecutive ready items starting from `next_expected`, in ascending height order,
    /// and advance the cursor. Must be called with the inner lock held so channel delivery order
    /// stays strictly ascending across concurrent workers.
    ///
    /// Uses `try_send` (not blocking `send`): a full ready channel must not park the coordinator
    /// while holding the bridge mutex — that prevents `fast_forward_cursor_to` from repairing a
    /// hole at `next_expected` while the tip sits in `pending` (live: bridge_next=698199 missing,
    /// tip 698202 in pending=56, feeder=0, validation frozen 40+ min).
    fn flush_unlocked(out: &Sender<ReadyItem>, g: &mut OrderedReadyInner) {
        let Some(mut n) = g.next_expected else {
            return;
        };
        while let Some(item) = g.pending.remove(&n) {
            match out.try_send(item) {
                Ok(()) => {
                    n += 1;
                }
                Err(crossbeam_channel::TrySendError::Full(item)) => {
                    g.pending.insert(n, item);
                    break;
                }
                Err(crossbeam_channel::TrySendError::Disconnected(item)) => {
                    g.pending.insert(n, item);
                    break;
                }
            }
        }
        g.next_expected = Some(n);
        sync_bridge_pending_count(g);
    }

    /// When validation has advanced past a missing `next_expected` hole **and** the tip (or a
    /// later height) sits stranded in `pending`, jump the cursor to `next_needed` and flush.
    /// Returns true if the cursor moved.
    ///
    /// Live 698202 stall: `bridge_next=698199` absent from pending while tip `698202` sat in
    /// pending; without this, flush is a no-op forever and Case C skips inject (`in_bridge_pending`).
    ///
    /// Do **not** FF when the hole is empty and tip is also absent — that races local inject
    /// (every height briefly "missing") and fights `[IBD_BRIDGE_REWIND]`.
    pub(crate) fn repair_missing_cursor_hole(&self, next_needed: u64) -> bool {
        let mut g = self
            .inner
            .lock()
            .expect("OrderedReadyBridge mutex poisoned");
        let Some(n) = g.next_expected else {
            g.next_expected = Some(next_needed);
            sync_bridge_pending_count(&g);
            return true;
        };
        if n >= next_needed {
            // Cursor already at tip — still flush if tip is parked in pending (Full try_send).
            if n == next_needed && g.pending.contains_key(&n) {
                let before = g.pending.len();
                self.flush_pending_unlocked(&mut g);
                sync_bridge_pending_count(&g);
                return g.pending.len() < before;
            }
            return false;
        }
        let hole = !g.pending.contains_key(&n);
        if !hole {
            return false;
        }
        // Tip height itself must be stranded behind the hole (live 698202).
        // "Any height ≥ next_needed" is too loose: ahead prefetch pending fires every
        // local-replay height and fights BRIDGE_REWIND.
        if !g.pending.contains_key(&next_needed) {
            return false;
        }
        tracing::warn!(
            "[IBD_BRIDGE_HOLE] next_expected={} missing from pending (len={}) — fast-forward to tip {}",
            n,
            g.pending.len(),
            next_needed
        );
        g.next_expected = Some(next_needed);
        let stale: Vec<u64> = g
            .pending
            .keys()
            .filter(|&&h| h < next_needed)
            .copied()
            .collect();
        for h in stale {
            g.pending.remove(&h);
        }
        self.flush_pending_unlocked(&mut g);
        sync_bridge_pending_count(&g);
        true
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
            (engine_empty_prefetch_arc(), engine_empty_spec_adds())
        } else {
            let full_map = Arc::new(prefetch_build_utxo_map(&s, &keys));
            // Build spec_adds on this worker thread (was on the dispatcher; see `build_spec_adds`).
            let spec_adds = Arc::new(build_spec_adds(&block, &tx_ids, h));
            (full_map, spec_adds)
        };
        let item: ReadyItem = (h, block, witnesses, keys, full_map, tx_ids, spec_adds);
        let vh = super::tip_stage::tracked_tip_height().saturating_sub(1);
        bridge.worker_complete(h, item, vh);
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
            Arc::new(FxHashMap::default()),
            Vec::<Hash>::new(),
            Arc::new(UtxoSet::default()),
        )
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn ordered_ready_bridge_emits_in_height_order() {
        let (tx, rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(10);
        bridge.worker_complete(11, dummy_ready(11), 0);
        assert!(rx.try_recv().is_err());
        bridge.worker_complete(10, dummy_ready(10), 0);
        assert_eq!(rx.recv().unwrap().0, 10);
        assert_eq!(rx.recv().unwrap().0, 11);
        bridge.worker_complete(12, dummy_ready(12), 0);
        assert_eq!(rx.recv().unwrap().0, 12);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn worker_complete_drops_duplicate_below_cursor() {
        let (tx, rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(10);
        bridge.worker_complete(10, dummy_ready(10), 0);
        assert_eq!(rx.recv().unwrap().0, 10);
        assert_eq!(bridge.next_expected(), Some(11));
        // Late duplicate of 10 must not strand in pending or rewind cursor.
        bridge.worker_complete(10, dummy_ready(10), 0);
        assert!(rx.try_recv().is_err());
        assert_eq!(bridge.next_expected(), Some(11));
        assert!(!bridge.pending_contains(10));
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn rewind_cursor_allows_reemit_of_lost_tip() {
        let (tx, rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(10);
        bridge.worker_complete(10, dummy_ready(10), 0);
        let _ = rx.recv();
        bridge.worker_complete(11, dummy_ready(11), 0);
        let _ = rx.recv();
        assert_eq!(bridge.next_expected(), Some(12));
        assert!(bridge.rewind_cursor_to(11));
        assert_eq!(bridge.next_expected(), Some(11));
        bridge.worker_complete(11, dummy_ready(11), 0);
        assert_eq!(rx.recv().unwrap().0, 11);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn ordered_ready_bridge_may_accept_respects_pending_cap() {
        let (tx, _rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(10);
        // Fill pending with ahead heights (gap at 10).
        bridge.worker_complete(11, dummy_ready(11), 0);
        bridge.worker_complete(12, dummy_ready(12), 0);
        assert_eq!(bridge.pending_len(), 2);
        assert!(
            !bridge.may_accept_height(13, 2),
            "at cap: refuse ahead height"
        );
        assert!(
            bridge.may_accept_height(10, 2),
            "at cap: still accept gap height to drain"
        );
        assert!(bridge.may_accept_height(13, 3), "under cap: accept ahead");
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn ordered_ready_bridge_try_flush_emits_when_gap_already_pending() {
        let (tx, rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(10);
        // Gap height buffered without a subsequent worker_complete to trigger flush.
        {
            let mut g = bridge.inner.lock().unwrap();
            g.pending.insert(10, dummy_ready(10));
            g.pending.insert(11, dummy_ready(11));
            sync_bridge_pending_count(&g);
        }
        assert!(bridge.pending_contains(10));
        assert_eq!(bridge.try_flush(), 2);
        assert_eq!(rx.recv().unwrap().0, 10);
        assert_eq!(rx.recv().unwrap().0, 11);
        assert_eq!(bridge.pending_len(), 0);
        assert_eq!(bridge.next_expected(), Some(12));
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn try_emit_direct_to_feeder_even_when_pending_has_ahead() {
        // W26: tip must not take the ready-channel hop just because ahead blocks are pending.
        let (tx, rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(10);
        {
            let mut g = bridge.inner.lock().unwrap();
            g.pending.insert(11, dummy_ready(11));
            g.pending.insert(12, dummy_ready(12));
            sync_bridge_pending_count(&g);
        }
        let feeder = super::super::feeder::new_feeder_state();
        let leftover = bridge.try_emit_in_order_to_feeder(10, dummy_ready(10), &feeder, 9);
        assert!(leftover.is_none(), "tip must be direct-emitted");
        assert!(
            feeder.0.lock().0.get(10).is_some(),
            "tip must land in feeder buffer"
        );
        // W56: contiguous ahead drained directly into feeder (not ready channel).
        assert!(
            feeder.0.lock().0.get(11).is_some() && feeder.0.lock().0.get(12).is_some(),
            "contiguous pending must land in feeder with tip"
        );
        assert!(
            rx.try_recv().is_err(),
            "must not advance cursor via ready-channel hop"
        );
        assert_eq!(bridge.pending_len(), 0);
        assert_eq!(bridge.next_expected(), Some(13));
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn try_emit_skips_reemit_when_tip_already_taken() {
        // W40: vh atomic lags retire; tip_stage advances on feeder take — no REEMIT storm.
        let (tx, _rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(10);
        let _ = bridge.try_emit_in_order_to_feeder(
            10,
            dummy_ready(10),
            &super::super::feeder::new_feeder_state(),
            9,
        );
        assert_eq!(bridge.next_expected(), Some(11));
        // W59: take is latched under feeder lock *before* finish_validated / HEIGHT roll.
        super::super::tip_stage::mark_needed(10);
        super::super::tip_stage::mark_getdata(10);
        super::super::tip_stage::mark_body(10);
        super::super::tip_stage::mark_feeder(10);
        super::super::tip_stage::mark_taken_from_feeder(10);
        assert!(
            super::super::tip_stage::tip_taken_by_validation(10),
            "taken latch must suppress REEMIT/REWIND before finish_validated"
        );
        super::super::tip_stage::finish_validated(10);
        assert!(super::super::tip_stage::tip_taken_by_validation(10));
        let feeder = super::super::feeder::new_feeder_state();
        let leftover = bridge.try_emit_in_order_to_feeder(10, dummy_ready(10), &feeder, 9);
        assert!(leftover.is_none());
        assert!(
            feeder.0.lock().0.get(10).is_none(),
            "must not re-insert tip already taken by validation"
        );
        assert_eq!(bridge.next_expected(), Some(11));
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn fast_forward_cursor_when_validation_ahead() {
        // W26b: bridge behind validation — advance cursor and drop obsolete pending.
        let (tx, rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(10);
        {
            let mut g = bridge.inner.lock().unwrap();
            g.pending.insert(11, dummy_ready(11));
            g.pending.insert(66, dummy_ready(66));
            sync_bridge_pending_count(&g);
        }
        assert!(bridge.fast_forward_cursor_to(66));
        assert!(!bridge.pending_contains(11), "obsolete pending dropped");
        // flush emits 66 if it was pending → cursor becomes 67
        assert_eq!(bridge.next_expected(), Some(67));
        assert_eq!(bridge.pending_len(), 0);
        let _ = rx.try_recv(); // 66 may already have been flushed
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn repair_missing_cursor_hole_flushes_stranded_tip() {
        // Live 698202: bridge_next=698199 missing, tip 698202 in pending.
        let (tx, rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(199);
        {
            let mut g = bridge.inner.lock().unwrap();
            g.pending.insert(200, dummy_ready(200));
            g.pending.insert(202, dummy_ready(202));
            sync_bridge_pending_count(&g);
        }
        assert!(!bridge.pending_contains(199));
        assert!(bridge.repair_missing_cursor_hole(202));
        assert_eq!(rx.recv().unwrap().0, 202);
        assert_eq!(bridge.next_expected(), Some(203));
        assert!(!bridge.pending_contains(200), "below-tip pending dropped");
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn repair_missing_cursor_hole_skips_when_tip_not_pending() {
        // Local-replay race: hole at cursor but tip not yet in pending — do not FF.
        let (tx, _rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(100);
        {
            let mut g = bridge.inner.lock().unwrap();
            g.pending.insert(101, dummy_ready(101)); // only below tip
            g.pending.insert(200, dummy_ready(200)); // ahead prefetch — must not trigger FF
            sync_bridge_pending_count(&g);
        }
        assert!(!bridge.repair_missing_cursor_hole(105));
        assert_eq!(bridge.next_expected(), Some(100));
        assert!(bridge.pending_contains(101));
        assert!(bridge.pending_contains(200));
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn flush_try_send_full_channel_leaves_pending_and_returns() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(10);
        // Emit 10 into the single channel slot.
        bridge.worker_complete(10, dummy_ready(10), 0);
        // Channel full — next in-order complete must not block; leave in pending.
        bridge.worker_complete(11, dummy_ready(11), 0);
        assert!(
            bridge.pending_contains(11),
            "full channel must park tip in pending, not block coordinator"
        );
        assert_eq!(bridge.next_expected(), Some(11));
        // Drain channel then flush.
        assert_eq!(rx.try_recv().unwrap().0, 10);
        assert!(bridge.try_flush() >= 1);
        assert_eq!(rx.try_recv().unwrap().0, 11);
        assert_eq!(bridge.next_expected(), Some(12));
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn ordered_ready_bridge_evicts_far_ahead_when_gap_missing() {
        let (tx, _rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(10);
        // Fill pending at cap with far-ahead heights (gap at 10 missing).
        for h in 11..=13 {
            bridge.worker_complete(h, dummy_ready(h), 0);
        }
        assert_eq!(bridge.pending_len(), 3);
        assert_eq!(
            bridge.evict_far_ahead_pending(10, 1, true, 3),
            2,
            "evict h=12,13 above ceiling=11"
        );
        assert_eq!(bridge.pending_len(), 1);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn ordered_ready_bridge_evicts_when_next_expected_missing_from_pending() {
        let (tx, _rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(10);
        // IBD_STALL pattern: bridge_next=10, pending full of ahead, gap not in pending.
        // gap_missing=false (near-ahead may be in reorder) but next_expected absent from pending.
        for h in 11..=13 {
            bridge.worker_complete(h, dummy_ready(h), 0);
        }
        assert_eq!(bridge.pending_len(), 3);
        assert!(!bridge.pending_contains(10));
        assert_eq!(
            bridge.evict_far_ahead_pending(10, 1, false, 3),
            2,
            "evict far-ahead when next_expected missing from pending even if gap_missing=false"
        );
        assert_eq!(bridge.pending_len(), 1);
        // After putting next_expected into pending, no further eviction with gap_missing=false
        // when under cap.
        bridge.worker_complete(10, dummy_ready(10), 0);
        // pending may have flushed 10+11; refill ahead to cap without gap_missing path.
        let (tx2, _rx2) = unbounded();
        let bridge2 = OrderedReadyBridge::new(tx2);
        bridge2.coordinator_will_send_height(20);
        bridge2.worker_complete(20, dummy_ready(20), 0); // in-order: flushes, pending empty
        bridge2.worker_complete(21, dummy_ready(21), 0);
        bridge2.worker_complete(22, dummy_ready(22), 0);
        // next_expected=23 missing; pending=2 < min_pending=max(64,3/4).min(3)=3 → no eviction
        assert_eq!(bridge2.evict_far_ahead_pending(23, 1, false, 3), 0);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn ordered_ready_bridge_b1_evicts_under_cap_when_next_expected_missing() {
        // Live soak: bridge_pending=64–450, never hit 512 — old gate never fired.
        // B1: next_expected missing → min_pending = max(64, pending_max/4).
        let (tx, _rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(100);
        // Fill 64 ahead heights (under pending_max=256).
        for h in 101..=164 {
            bridge.worker_complete(h, dummy_ready(h), 0);
        }
        assert_eq!(bridge.pending_len(), 64);
        assert!(!bridge.pending_contains(100));
        let evicted = bridge.evict_far_ahead_pending(100, 1, false, 256);
        assert!(
            evicted >= 1,
            "B1 must evict far-ahead under cap when next_expected missing (evicted={evicted})"
        );
        assert!(
            bridge.pending_len() < 64,
            "pending should shrink after under-cap eviction"
        );
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn ordered_ready_bridge_b1b_peel_disabled_by_default() {
        // L2: inside-window peel is opt-in. Far-ceiling (tight keep) still runs for heights
        // above next+tight_keep; this fixture keeps all pending ≤ ceiling so only peel
        // would have fired — and peel must not.
        let (tx, _rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(100);
        // window=256 → tight_keep=64 → ceiling=164. Fill 101..=164 (all ≤ ceiling).
        for h in 101..=164 {
            bridge.worker_complete(h, dummy_ready(h), 0);
        }
        assert_eq!(bridge.pending_len(), 64);
        assert!(!bridge.pending_contains(100));
        let evicted = bridge.evict_far_ahead_pending(100, 256, false, 256);
        assert_eq!(
            evicted, 0,
            "default must not peel inside tight keep band (evicted={evicted})"
        );
        assert_eq!(bridge.pending_len(), 64);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn ordered_ready_bridge_no_evict_when_under_cap() {
        let (tx, _rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(10);
        bridge.worker_complete(11, dummy_ready(11), 0);
        assert_eq!(
            bridge.evict_far_ahead_pending(10, 1, true, 3),
            0,
            "under min_pending: no eviction"
        );
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn ordered_ready_bridge_w17_wan_default_preserves_tip_pipe() {
        let _guard = crate::ibd_test_lock::guard();
        // SAFETY: test-only; peel opt-in must not leak from parallel env tests.
        unsafe { std::env::remove_var("BLVM_IBD_WAN_B1B_PEEL") };
        // 2026-07-14: default WAN peel/tight_keep=16 wiped pending→0 every tip tick.
        // Near-ahead inside next+128 must be kept while tip is missing.
        let (tx, _rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(100);
        for h in 101..=164 {
            bridge.worker_complete(h, dummy_ready(h), 0);
        }
        assert_eq!(bridge.pending_len(), 64);
        let evicted = bridge.evict_far_ahead_pending_ex(100, 256, true, 256, true);
        assert_eq!(
            evicted, 0,
            "default WAN must not peel/wipe tip-pipe band (evicted={evicted})"
        );
        assert_eq!(bridge.pending_len(), 64);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn ordered_ready_bridge_w17_wan_peel_opt_in() {
        // Opt-in peel still available for experiments.
        // SAFETY: test-only env mutation; serial within this module.
        unsafe { std::env::set_var("BLVM_IBD_WAN_B1B_PEEL", "1") };
        let (tx, _rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(100);
        for h in 101..=164 {
            bridge.worker_complete(h, dummy_ready(h), 0);
        }
        let evicted = bridge.evict_far_ahead_pending_ex(100, 256, true, 256, true);
        unsafe { std::env::remove_var("BLVM_IBD_WAN_B1B_PEEL") };
        assert!(
            evicted > 0,
            "WAN peel with BLVM_IBD_WAN_B1B_PEEL=1 must evict (evicted={evicted})"
        );
        assert!(bridge.pending_len() < 64);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn ordered_ready_bridge_w18_purges_stale_below_floor() {
        // Live W17: bridge_min=640001 while tip=6859xx — dead pending never flushed.
        let (tx, _rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(200);
        // Insert stale heights below next_expected via pending map.
        {
            let mut g = bridge.inner.lock().unwrap();
            g.pending.insert(50, dummy_ready(50));
            g.pending.insert(51, dummy_ready(51));
            g.pending.insert(201, dummy_ready(201));
            sync_bridge_pending_count(&g);
        }
        assert_eq!(bridge.pending_len(), 3);
        // Tip healthy — still purge stale below floor.
        let evicted = bridge.evict_far_ahead_pending_ex(200, 256, false, 512, true);
        assert!(
            evicted >= 2,
            "must purge h=50,51 below floor=200 (evicted={evicted})"
        );
        assert!(!bridge.pending_contains(50));
        assert!(!bridge.pending_contains(51));
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn ordered_ready_bridge_w19_under_min_only_evicts_beyond_far_ceiling() {
        // 2026-07-14: under-min + tight_keep=16 deleted tip-pipe (h=120..139 with tip=100).
        // Under min_pending, only heights beyond far_ceiling (next+window) may go.
        let (tx, _rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(100);
        // Inside tip-pipe band (next+128) — must keep.
        for h in 120..=139 {
            bridge.worker_complete(h, dummy_ready(h), 0);
        }
        assert_eq!(bridge.pending_len(), 20);
        let kept = bridge.evict_far_ahead_pending_ex(100, 256, false, 512, true);
        assert_eq!(
            kept, 0,
            "must not wipe tip-pipe under min_pending (evicted={kept})"
        );
        assert_eq!(bridge.pending_len(), 20);

        // Beyond far_ceiling=356 — may evict even under min.
        let (tx2, _rx2) = unbounded();
        let bridge2 = OrderedReadyBridge::new(tx2);
        bridge2.coordinator_will_send_height(100);
        for h in 400..=419 {
            bridge2.worker_complete(h, dummy_ready(h), 0);
        }
        let evicted = bridge2.evict_far_ahead_pending_ex(100, 256, false, 512, true);
        assert!(
            evicted > 0,
            "under-min may still drop heights beyond far_ceiling (evicted={evicted})"
        );
        assert!(bridge2.pending_len() < 20);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn ordered_ready_bridge_wan_no_pending_zero_thrash() {
        // Live signature: one ahead ReadyItem arrives, eviction leaves pending=0.
        let (tx, _rx) = unbounded();
        let bridge = OrderedReadyBridge::new(tx);
        bridge.coordinator_will_send_height(392138);
        bridge.worker_complete(392140, dummy_ready(392140), 0);
        assert_eq!(bridge.pending_len(), 1);
        let evicted = bridge.evict_far_ahead_pending_ex(392138, 1024, true, 512, true);
        assert_eq!(evicted, 0, "must not discard lone tip-pipe ReadyItem");
        assert_eq!(bridge.pending_len(), 1);
    }
}
