//! `MemoryAge`: one tier of the age-tiered UTXO index.
//!
//! Wraps a list of `MemoryRun`s with snapshot-read semantics: readers take an `Arc` clone under
//! a short write-lock, then iterate without holding any lock. Writes replace the `Arc<Vec<…>>`
//! atomically. Uses `parking_lot::RwLock` (already a dep) instead of `arc-swap` to avoid a new
//! crate dependency. Profiler can revisit if read-lock contention shows up.

use super::memory_run::{MemoryRun, QueryResult};
use super::types::{OutputId, OutputKV};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static APPEND_IN_PLACE: AtomicU64 = AtomicU64::new(0);
static APPEND_SLOW_PATH: AtomicU64 = AtomicU64::new(0);
/// Slow path caused by shared outer/tip Arc (true poison) — used for pipeline throttle.
static APPEND_SLOW_CONTENTION: AtomicU64 = AtomicU64::new(0);

/// Minimum appends in the rolling window before `memory_age_window_slow_pct` reports.
const APPEND_WINDOW_MIN_SAMPLES: u64 = 256;
/// Max age of the rolling window before baselines reset.
const APPEND_WINDOW_MAX_AGE: Duration = Duration::from_secs(10);
/// Min window age before reset once enough samples exist (avoids per-iteration churn).
const APPEND_WINDOW_MIN_AGE: Duration = Duration::from_secs(2);

struct AppendRateWindow {
    baseline_in_place: u64,
    baseline_slow: u64,
    baseline_contention: u64,
    window_start: Instant,
}

impl AppendRateWindow {
    fn new() -> Self {
        Self {
            baseline_in_place: 0,
            baseline_slow: 0,
            baseline_contention: 0,
            window_start: Instant::now(),
        }
    }
}

/// Why `try_append_in_place` could not mutate the tip under the write lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InPlaceMiss {
    /// Outer `Arc<Vec>` or tip `Arc<MemoryRun>` has other holders — clone path is forced.
    Contention,
    /// Tip full / frozen / empty — freeze-and-new (or first run); expected every few blocks.
    Structural,
}

fn append_rate_window() -> &'static parking_lot::Mutex<AppendRateWindow> {
    static W: OnceLock<parking_lot::Mutex<AppendRateWindow>> = OnceLock::new();
    W.get_or_init(|| parking_lot::Mutex::new(AppendRateWindow::new()))
}

/// Diagnostic counters for index append paths (logged in `[MEM_REPORT]`).
/// Returns `(in_place, slow_total, slow_contention)`.
pub fn memory_age_append_stats() -> (u64, u64, u64) {
    (
        APPEND_IN_PLACE.load(Ordering::Relaxed),
        APPEND_SLOW_PATH.load(Ordering::Relaxed),
        APPEND_SLOW_CONTENTION.load(Ordering::Relaxed),
    )
}

/// Contention slow-path percentage over a short rolling window (since last baseline reset).
/// Freeze-and-new is excluded — it is expected ~1/3 of appends at mid-IBD with 25k run caps.
pub fn memory_age_window_slow_pct() -> Option<u64> {
    let (in_place, slow, contention) = memory_age_append_stats();
    let mut window = append_rate_window().lock();
    let delta_in_place = in_place.saturating_sub(window.baseline_in_place);
    let delta_slow = slow.saturating_sub(window.baseline_slow);
    let delta_contention = contention.saturating_sub(window.baseline_contention);
    let total = delta_in_place + delta_slow;
    let elapsed = window.window_start.elapsed();

    let pct = if total >= APPEND_WINDOW_MIN_SAMPLES {
        Some(delta_contention.saturating_mul(100) / total)
    } else {
        None
    };

    let should_roll =
        elapsed >= APPEND_WINDOW_MAX_AGE || (pct.is_some() && elapsed >= APPEND_WINDOW_MIN_AGE);
    if should_roll {
        window.baseline_in_place = in_place;
        window.baseline_slow = slow;
        window.baseline_contention = contention;
        window.window_start = Instant::now();
    }

    pct
}

/// Contention percentage for pipeline throttling: max(recent window, cumulative lifetime).
///
/// Only Arc-sharing poison counts. Structural freeze-and-new (~30% at h≈500k) must not
/// shrink `pipeline_depth` — that was the BPS regression after the 15% throttle landed.
pub fn memory_age_throttle_slow_pct() -> u64 {
    let (in_place, slow, contention) = memory_age_append_stats();
    let total = in_place + slow;
    let cumulative = if total >= 512 {
        contention.saturating_mul(100) / total
    } else {
        0
    };
    match memory_age_window_slow_pct() {
        Some(window) => window.max(cumulative),
        None => cumulative,
    }
}

#[cfg(test)]
pub(crate) fn reset_append_diagnostics_for_test() {
    APPEND_IN_PLACE.store(0, Ordering::Relaxed);
    APPEND_SLOW_PATH.store(0, Ordering::Relaxed);
    APPEND_SLOW_CONTENTION.store(0, Ordering::Relaxed);
    *append_rate_window().lock() = AppendRateWindow::new();
}

#[cfg(test)]
pub(crate) fn bump_append_stats_for_test(in_place: u64, slow: u64) {
    bump_append_stats_detailed_for_test(in_place, slow, slow);
}

#[cfg(test)]
pub(crate) fn bump_append_stats_detailed_for_test(in_place: u64, slow: u64, contention: u64) {
    if in_place > 0 {
        APPEND_IN_PLACE.fetch_add(in_place, Ordering::Relaxed);
    }
    if slow > 0 {
        APPEND_SLOW_PATH.fetch_add(slow, Ordering::Relaxed);
    }
    if contention > 0 {
        APPEND_SLOW_CONTENTION.fetch_add(contention, Ordering::Relaxed);
    }
}

#[cfg(test)]
pub(crate) fn set_append_window_baseline_for_test(in_place: u64, slow: u64) {
    let mut window = append_rate_window().lock();
    window.baseline_in_place = in_place;
    window.baseline_slow = slow;
    window.baseline_contention = APPEND_SLOW_CONTENTION.load(Ordering::Relaxed);
    window.window_start = Instant::now();
}

/// RAII guard that pins a height in a `MemoryAge`, preventing the compacter from merging it away.
pub struct Pin {
    age_pins: Arc<parking_lot::Mutex<BTreeSet<i32>>>,
    height: i32,
}

impl Pin {
    fn new(age_pins: Arc<parking_lot::Mutex<BTreeSet<i32>>>, height: i32) -> Self {
        age_pins.lock().insert(height);
        Self { age_pins, height }
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        self.age_pins.lock().remove(&self.height);
    }
}

/// One tier of the 7-age UTXO index.
///
/// Snapshot pattern:
/// - Readers: `runs.read()` → `Arc::clone` → release → iterate snapshot.
/// - Appends: `runs.write()` → build new `Arc<Vec<…>>` → replace → release.
///
/// The write lock is held only during the Arc swap, not during run builds.
pub struct MemoryAge {
    pub(super) is_mutable: bool,
    /// Fan-in threshold: trigger merge when `runs.len() >= merge_fan_in`.
    pub(super) merge_fan_in: usize,
    /// When >0 (spill tier): merge as soon as `mergeable >= spill_early_take`, taking at
    /// most that many runs. Breaks multi‑GB park-under-fan_in=8 and 8-way mega disk spills.
    /// Synced from live eviction age via `UtxoIndex::sync_spill_early_takes`.
    /// `0` = disabled (legacy fan_in behavior). See `BLVM_IBD_SPILL_MERGE_TAKE`.
    pub(super) spill_early_take: AtomicUsize,
    /// Snapshot of `Arc<MemoryRun>` list. Readers clone the outer Arc, iterate without lock.
    pub(super) runs: parking_lot::RwLock<Arc<Vec<Arc<MemoryRun>>>>,
    /// Heights pinned by in-flight blocks. Compacter must not merge below `min(pins)`.
    pins: Arc<parking_lot::Mutex<BTreeSet<i32>>>,
    /// Highest height fully merged out of this age (into the next older age).
    pub(super) merged_to: AtomicI32,
    /// CAS guard: only one compacter thread may merge this age at a time.
    pub(super) is_merging: AtomicBool,
    /// Callback to notify the compacter that this age is ready for merge.
    enqueue: Option<Box<dyn Fn() + Send + Sync>>,
    /// Shared with `UtxoIndex` — bumped whenever this age's run list is replaced.
    index_epoch: Option<Arc<AtomicU64>>,
    /// Block entries held when the mutable tip `Arc<MemoryRun>` is shared with query workers.
    /// Merged into the tip at end of `append` (dispatch thread only).
    dispatch_staging: parking_lot::Mutex<Vec<OutputKV>>,
}

impl std::fmt::Debug for MemoryAge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryAge")
            .field("is_mutable", &self.is_mutable)
            .field("merge_fan_in", &self.merge_fan_in)
            .field("merged_to", &self.merged_to.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl MemoryAge {
    pub fn new(is_mutable: bool, merge_fan_in: usize) -> Self {
        Self::new_with_enqueue(is_mutable, merge_fan_in, None)
    }

    pub fn new_with_enqueue(
        is_mutable: bool,
        merge_fan_in: usize,
        enqueue: Option<Box<dyn Fn() + Send + Sync>>,
    ) -> Self {
        Self::new_with_hooks(is_mutable, merge_fan_in, enqueue, None)
    }

    pub(super) fn new_with_hooks(
        is_mutable: bool,
        merge_fan_in: usize,
        enqueue: Option<Box<dyn Fn() + Send + Sync>>,
        index_epoch: Option<Arc<AtomicU64>>,
    ) -> Self {
        Self {
            is_mutable,
            merge_fan_in,
            spill_early_take: AtomicUsize::new(0),
            runs: parking_lot::RwLock::new(Arc::new(Vec::new())),
            pins: Arc::new(parking_lot::Mutex::new(BTreeSet::new())),
            merged_to: AtomicI32::new(i32::MIN),
            is_merging: AtomicBool::new(false),
            enqueue,
            index_epoch,
            dispatch_staging: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// Mark/unmark this age as spill-tier early-merge (see `spill_early_take`).
    pub(super) fn set_spill_early_take(&self, take: usize) {
        self.spill_early_take.store(take, Ordering::Release);
    }

    fn bump_index_epoch(&self) {
        if let Some(e) = &self.index_epoch {
            e.fetch_add(1, Ordering::Release);
        }
    }

    /// Pin `height` to prevent the compacter from merging it away.
    /// Returns a RAII `Pin` guard that unpins on drop.
    pub fn pin_height(&self, height: i32) -> Pin {
        Pin::new(Arc::clone(&self.pins), height)
    }

    /// Snapshot the current run list (cheap: one Arc clone under a short read-lock).
    pub fn snapshot_runs(&self) -> Arc<Vec<Arc<MemoryRun>>> {
        Arc::clone(&*super::timed_age_runs_read(&self.runs))
    }

    /// Total approximate resident bytes across all runs in this age tier.
    pub fn mem_bytes(&self) -> usize {
        self.runs.read().iter().map(|r| r.mem_bytes()).sum()
    }

    /// Number of runs currently in this age.
    pub fn run_count(&self) -> usize {
        self.runs.read().len()
    }

    /// Maximum entries in a mutable run before it is auto-frozen.
    ///
    /// Each block appends N entries via `append_and_rebuild` which merges sorted batches
    /// into the mutable tip (no bloom/directory rebuild until freeze).
    ///
    /// Reduced from 50,000 to 25,000 (was briefly 10,000):
    ///   • 10k froze every ~1 block at h≈450k → 1-height runs → PIN_BLOCKED with pipeline pins
    ///   • 25k ≈ 2–4 blocks/run at late heights: fewer freezes, wider height spans for merge
    ///   • Per-block clone still bounded (~1.3 MB) vs 50k (~2.6 MB)
    ///
    /// At 400k heights with ~10k entries/block, the run fills in ~2–3 blocks. Compacter
    /// keeps up via its merge pipeline; wider runs make `mergeable_prefix` grow past pins.
    ///
    /// Override via `BLVM_IBD_MUTABLE_RUN_MAX_ENTRIES` (10_000..=50_000). Default 35_000 (PR-H7).
    fn mutable_run_max_entries() -> usize {
        static CACHED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *CACHED.get_or_init(|| {
            std::env::var("BLVM_IBD_MUTABLE_RUN_MAX_ENTRIES")
                .ok()
                .and_then(|s| s.parse().ok())
                .filter(|&n| (10_000..=50_000).contains(&n))
                .unwrap_or(35_000)
        })
    }

    /// Append a new batch of UTXO entries into this mutable age at `height`.
    ///
    /// **All expensive work (clone, sort, bloom/directory rebuild) happens OUTSIDE the write
    /// lock.** The write lock is held only for the final `Arc` pointer swap — a few
    /// nanoseconds — so concurrent readers (validation workers) are never blocked for more
    /// than a pointer swap.
    ///
    /// Invariant exploited: only the dispatch thread calls `append` on mutable ages; the
    /// compacter only ever removes frozen runs from the *front* of the Vec (oldest). The
    /// mutable tip is always the *last* element and is never touched by the compacter.
    /// Therefore snapshotting the tip before the lock and swapping it back under the lock
    /// is race-free.
    pub fn append(&self, entries: Vec<OutputKV>, height: i32) {
        debug_assert!(!entries.is_empty());
        debug_assert!(self.is_mutable, "use push_frozen_run for frozen ages");
        let _ = height;

        if self.try_append_in_place(&entries).is_ok() {
            self.try_merge_dispatch_staging();
            self.maybe_notify_compacter();
            return;
        }

        self.dispatch_staging.lock().extend(entries);
        self.bump_index_epoch();
        self.try_merge_dispatch_staging();
        self.maybe_notify_compacter();
    }

    fn notify_threshold(&self) -> usize {
        let early = self.spill_early_take.load(Ordering::Relaxed);
        if early > 0 {
            early.min(self.merge_fan_in)
        } else {
            self.merge_fan_in
        }
    }

    fn maybe_notify_compacter(&self) {
        if self.runs.read().len() >= self.notify_threshold() {
            if let Some(ref eq) = self.enqueue {
                eq();
            }
        }
    }

    /// Append when the mutable tip `Arc<MemoryRun>` is uniquely owned (no hidden clone).
    ///
    /// Critical: do **not** call `Arc::make_mut` on a shared outer `Arc<Vec<…>>`. That clones
    /// every run pointer (including the tip), bumps tip `strong_count`, and guarantees the
    /// unique-tip check fails — turning every concurrent `snapshot_runs()` holder into a
    /// slow-path storm. Bail early when the outer Arc is shared; callers fall through to
    /// staging / `append_slow_path`.
    ///
    /// Returns `Ok(())` on success, or `Err(InPlaceMiss)` describing why the slow path is needed.
    fn try_append_in_place(&self, entries: &[OutputKV]) -> Result<(), InPlaceMiss> {
        let mut w = self.runs.write();
        if Arc::strong_count(&*w) != 1 {
            return Err(InPlaceMiss::Contention);
        }
        let runs = Arc::get_mut(&mut *w).expect("strong_count==1");
        let Some(last) = runs.last_mut() else {
            return Err(InPlaceMiss::Structural);
        };
        if !last.is_mutable || last.len() + entries.len() > Self::mutable_run_max_entries() {
            return Err(InPlaceMiss::Structural);
        }
        if Arc::strong_count(last) != 1 {
            return Err(InPlaceMiss::Contention);
        }
        let run = Arc::get_mut(last).expect("strong_count==1");
        run.append_and_rebuild(entries);
        drop(w);
        self.bump_index_epoch();
        APPEND_IN_PLACE.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Copy all `OutputKV` entries under a short `runs.read()` — does **not** return
    /// `Arc<MemoryRun>` handles. Prefer this over `snapshot_runs()` for long scans
    /// (watermark export): holding tip Arcs across multi-second disk work blocks in-place append.
    pub(super) fn collect_entries_into(&self, out: &mut Vec<OutputKV>) {
        let guard = self.runs.read();
        for run in guard.iter() {
            out.extend_from_slice(&run.entries);
        }
    }

    /// Like `collect_entries_into` but only entries with `height <= max_height`.
    pub(super) fn collect_entries_at_or_below_into(
        &self,
        max_height: i32,
        out: &mut Vec<OutputKV>,
    ) {
        let guard = self.runs.read();
        for run in guard.iter() {
            for &entry in &run.entries {
                if entry.height <= max_height {
                    out.push(entry);
                }
            }
        }
    }

    /// Merge `dispatch_staging` into the mutable tip. Uses in-place append when unique, else slow-path clone.
    fn try_merge_dispatch_staging(&self) {
        let mut staging = self.dispatch_staging.lock();
        if staging.is_empty() {
            return;
        }
        match self.try_append_in_place(&staging) {
            Ok(()) => {
                staging.clear();
            }
            Err(miss) => {
                let batch = std::mem::take(&mut *staging);
                drop(staging);
                self.append_slow_path(batch, miss);
            }
        }
    }

    fn append_slow_path(&self, entries: Vec<OutputKV>, miss: InPlaceMiss) {
        APPEND_SLOW_PATH.fetch_add(1, Ordering::Relaxed);
        if miss == InPlaceMiss::Contention {
            APPEND_SLOW_CONTENTION.fetch_add(1, Ordering::Relaxed);
        }
        let tip_snapshot: Option<Arc<MemoryRun>> = {
            let r = self.runs.read();
            r.last().cloned()
        };

        // ── Phase 2: build new run state OUTSIDE any lock ────────────────────────
        // This is where the expensive work lives: clone (≤50k × 52B), sort, bloom, dir.
        // No lock is held during this phase.
        enum NewState {
            Replace(Arc<MemoryRun>),
            FreezeAndNew {
                frozen: Arc<MemoryRun>,
                new_tip: Arc<MemoryRun>,
            },
        }

        let new_state = match tip_snapshot {
            Some(ref last)
                if last.is_mutable
                    && last.len() + entries.len() <= Self::mutable_run_max_entries() =>
            {
                // Extend mutable run in place (bounded: ≤ mutable_run_max_entries).
                let mut run = (**last).clone();
                run.append_and_rebuild(&entries);
                NewState::Replace(Arc::new(run))
            }
            Some(ref last) if last.is_mutable => {
                // Mutable run is full: freeze it, start a fresh mutable run.
                let mut frozen = (**last).clone();
                frozen.freeze();
                let mut new_run = MemoryRun::new_mutable();
                new_run.append_and_rebuild(&entries);
                NewState::FreezeAndNew {
                    frozen: Arc::new(frozen),
                    new_tip: Arc::new(new_run),
                }
            }
            _ => {
                // Empty age or last run already frozen: create the first mutable run.
                let mut new_run = MemoryRun::new_mutable();
                new_run.append_and_rebuild(&entries);
                NewState::Replace(Arc::new(new_run))
            }
        };

        // ── Phase 3: swap under write lock (nanoseconds — only Arc clones + ptr write) ──
        {
            let mut w = self.runs.write();
            // Re-read the current vec. The compacter may have removed frozen runs from the
            // *front* between Phase 1 and now; their removals are reflected here. The mutable
            // tip (last element) is guaranteed unchanged since only we modify it.
            let current_len = w.len();
            let mut new_runs: Vec<Arc<MemoryRun>> = (**w).clone(); // cheap: Vec of Arc
            match new_state {
                NewState::Replace(new_tip) => {
                    if new_runs.last().map(|r| r.is_mutable).unwrap_or(false) {
                        *new_runs.last_mut().unwrap() = new_tip;
                    } else {
                        // Edge: last run was frozen by compacter (shouldn't happen, but safe).
                        new_runs.push(new_tip);
                    }
                }
                NewState::FreezeAndNew { frozen, new_tip } => {
                    if new_runs.last().map(|r| r.is_mutable).unwrap_or(false) {
                        *new_runs.last_mut().unwrap() = frozen;
                    } else {
                        new_runs.push(frozen);
                    }
                    new_runs.push(new_tip);
                }
            }
            let _ = current_len;
            *w = Arc::new(new_runs);
        }
        self.bump_index_epoch();
        self.maybe_notify_compacter();
    }

    /// Push an already-built `MemoryRun` into this (frozen) age.
    ///
    /// Used by the compacter to deliver a merged run without rebuilding it. The run must
    /// already be frozen (sorted, bloom, directory built). The write lock is held only for
    /// the Vec append — no expensive work is done under the lock.
    pub fn push_frozen_run(&self, run: Arc<MemoryRun>) {
        debug_assert!(!run.is_mutable, "push_frozen_run: run must be frozen");
        {
            let mut w = self.runs.write();
            if Arc::strong_count(&*w) == 1 {
                Arc::get_mut(&mut *w).expect("strong_count==1").push(run);
            } else {
                let mut new_runs = (**w).clone();
                new_runs.push(run);
                *w = Arc::new(new_runs);
            }
        }
        self.bump_index_epoch();
        if self.runs.read().len() >= self.notify_threshold() {
            if let Some(ref eq) = self.enqueue {
                eq();
            }
        }
    }

    /// Freeze the mutable tip run (if any). Called before pushing to next older age.
    pub fn freeze_tip(&self) {
        let mut lock = self.runs.write();
        let old = Arc::clone(&*lock);
        if let Some(last) = old.last() {
            if last.is_mutable {
                let mut new_runs = (*old).clone();
                let mut frozen = (**last).clone();
                frozen.freeze();
                *new_runs.last_mut().unwrap() = Arc::new(frozen);
                *lock = Arc::new(new_runs);
                drop(lock);
                self.bump_index_epoch();
            }
        }
    }

    /// Query this age for `key` within the `[since, before)` height window.
    ///
    /// Iterates runs newest-to-oldest (last-to-first in the Vec), stopping on first resolved hit.
    /// Uses a read guard (not `snapshot_runs`) so the outer `Arc<Vec>` stays unique for in-place append.
    pub fn lookup_key(&self, key: &[u8; 36], since: i32, before: i32) -> Option<OutputId> {
        let guard = self.runs.read();
        for run in guard.iter().rev() {
            if let Some(id) = run.lookup_key(key, since, before) {
                return Some(id);
            }
        }
        None
    }

    /// Batch query using a pre-snapshotted run list (avoids per-age `runs.read()` on hot path).
    pub fn batch_query_with_runs(
        &self,
        snapshot: &Arc<Vec<Arc<MemoryRun>>>,
        keys: &[[u8; 36]],
        ids: &mut [OutputId],
        since: i32,
        before: i32,
    ) -> QueryResult {
        let mut result = QueryResult::default();
        for run in snapshot.iter().rev() {
            if !ids.contains(&OutputId::MAX) {
                break;
            }
            run.batch_lookup(keys, ids, since, before);
        }
        for id in ids.iter() {
            if *id == OutputId::MAX {
                result.absent += 1;
            } else {
                result.resolved += 1;
            }
        }
        result
    }

    /// Batch query across all runs in this age.
    ///
    /// For each key in `keys`, fills `ids[i]` with the found `OutputId` (or leaves it as
    /// `OutputId::MAX` if not found here). Callers chain multiple ages newest-to-oldest.
    pub fn batch_query(
        &self,
        keys: &[[u8; 36]],
        ids: &mut [OutputId],
        since: i32,
        before: i32,
    ) -> QueryResult {
        let mut result = QueryResult::default();
        let runs_guard = super::timed_age_runs_read(&self.runs);
        for run in runs_guard.iter().rev() {
            if !ids.contains(&OutputId::MAX) {
                break;
            }
            run.batch_lookup(keys, ids, since, before);
        }
        drop(runs_guard);
        for id in ids.iter() {
            if *id == OutputId::MAX {
                result.absent += 1;
            } else {
                result.resolved += 1;
            }
        }
        result
    }

    /// Returns `true` if this age has enough runs to trigger a merge and no pins block it.
    pub fn merge_ready(&self) -> bool {
        self.merge_ready_inner(false)
    }

    /// Like `merge_ready` but logs a warning when pin-blocked (call at INFO cadence, not hot path).
    pub fn merge_ready_logged(&self) -> bool {
        self.merge_ready_inner(true)
    }

    /// Minimum runs to merge when draining under pin pressure (below full `merge_fan_in`).
    /// Still a real k-way merge; just a smaller batch so tip pins cannot stall the age forever.
    /// 1 is intentional: at h≈450k each frozen run is often a single height, so
    /// `mergeable_prefix` is frequently 0–1 while pipeline pins cover ~16–32 tip heights.
    const MIN_PARTIAL_MERGE: usize = 1;

    /// Lowest pinned height, or `None` if nothing is pinned.
    fn min_pin_height(&self) -> Option<i32> {
        self.pins.lock().iter().next().copied()
    }

    /// How many leading (oldest) runs are safe to merge given current pins.
    ///
    /// A run is mergeable when its entire `height_range` is strictly below `min(pin)`.
    /// In-flight Phase-1/Phase-2 pins cover tip heights until `complete()` drops them;
    /// without this prefix filter the oldest `fan_in` runs often overlap those pins and
    /// the whole age stalls (`PIN_BLOCKED` with 20–40 runs piled up) even though older
    /// frozen runs below the pin window are ready to drain.
    fn mergeable_prefix_len(runs: &[Arc<MemoryRun>], min_pin: Option<i32>) -> usize {
        match min_pin {
            None => runs.len(),
            Some(pin) => runs.iter().take_while(|r| r.height_range.1 < pin).count(),
        }
    }

    /// How many runs to take now: full fan-in when possible, else a partial drain when
    /// the age is backlogged and a safe prefix exists below `min(pin)`.
    ///
    /// When heavily backlogged (`≥ 2× fan_in`), take at most 2 runs even if a full fan-in
    /// is mergeable. Spill-tier (age-3→disk) 8-way merges of multi-GB runs take 20–30s;
    /// during that window younger ages keep pushing and the tier piles to 50+ runs / 12GB.
    /// Smaller batches finish faster, free RSS sooner, and let more compacter threads help.
    ///
    /// `spill_early`: when >0 (spill tier), merge as soon as `mergeable >= spill_early`,
    /// taking at most that many — prevents parking multi‑GB under fan_in before first spill.
    fn merge_take_count(
        run_count: usize,
        mergeable: usize,
        fan_in: usize,
        spill_early: usize,
    ) -> Option<usize> {
        if mergeable == 0 {
            return None;
        }
        // Spill-tier early drain (opt-in via spill_early_take / BLVM_IBD_SPILL_MERGE_TAKE).
        if spill_early > 0 && mergeable >= spill_early {
            return Some(mergeable.min(spill_early).min(fan_in));
        }
        // Heavy backlog: prefer small, frequent merges over one giant 8-way disk write.
        if run_count >= fan_in.saturating_mul(2) && mergeable >= Self::MIN_PARTIAL_MERGE {
            return Some(mergeable.min(2).min(fan_in));
        }
        if mergeable >= fan_in {
            return Some(fan_in);
        }
        // Backlogged (≥ fan_in) with at least one safe older run: drain the safe prefix
        // instead of waiting for tip pins to clear the whole window.
        // At h≈500k each frozen run is often 1 height; pipeline pins cover tip heights, so
        // the oldest fan_in window almost always overlaps pins and never drains otherwise.
        if run_count >= fan_in && mergeable >= Self::MIN_PARTIAL_MERGE {
            return Some(mergeable.min(fan_in));
        }
        None
    }

    fn merge_ready_inner(&self, log_blocked: bool) -> bool {
        let min_pin = self.min_pin_height();
        let guard = self.runs.read();
        let run_count = guard.len();
        let mergeable = Self::mergeable_prefix_len(&guard, min_pin);
        let spill_early = self.spill_early_take.load(Ordering::Relaxed);
        if Self::merge_take_count(run_count, mergeable, self.merge_fan_in, spill_early).is_some() {
            return true;
        }
        if log_blocked && run_count >= self.merge_fan_in {
            let take_n = self.merge_fan_in.min(run_count);
            let merge_min = guard[..take_n]
                .iter()
                .map(|r| r.height_range.0)
                .min()
                .unwrap_or(i32::MAX);
            let merge_max = guard[..take_n]
                .iter()
                .map(|r| r.height_range.1)
                .max()
                .unwrap_or(i32::MIN);
            tracing::warn!(
                "UTXO compacter PIN_BLOCKED: {run_count} runs waiting, \
                 mergeable_prefix={mergeable} (need {} or partial≥{} when backlogged), \
                 oldest_fan_in_range={merge_min}..={merge_max}, min_pin={min_pin:?}",
                self.merge_fan_in,
                Self::MIN_PARTIAL_MERGE,
            );
        }
        false
    }

    /// Take the oldest mergeable runs for merging. Returns them (or None if not ready).
    ///
    /// Marks `is_merging = true` via CAS. Caller must call `complete_merge` when done.
    /// Snapshot the oldest runs for merging, WITHOUT removing them from the age.
    ///
    /// The runs remain in `self.runs` and continue to be visible to concurrent queries
    /// throughout the merge. Only after `complete_merge` (which receives the same runs back)
    /// are they atomically replaced by the merged result.
    ///
    /// This prevents the UTXO-invisible window that occurred when runs were removed eagerly:
    /// during the compacter's merge pass, any UTXO in the removed runs would return
    /// `OutputId::MAX` from queries, causing "UTXO not found" errors.
    pub fn take_for_merge(&self) -> Option<Vec<Arc<MemoryRun>>> {
        if self
            .is_merging
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return None; // another thread is already merging
        }
        if !self.merge_ready() {
            self.is_merging.store(false, Ordering::Release);
            return None;
        }
        let min_pin = self.min_pin_height();
        // Clone only mergeable oldest run Arcs under a short read guard — do not
        // snapshot_runs() (that pins the outer Arc and blocks in-place append).
        let guard = self.runs.read();
        let run_count = guard.len();
        let mergeable = Self::mergeable_prefix_len(&guard, min_pin);
        let spill_early = self.spill_early_take.load(Ordering::Relaxed);
        let Some(take_n) =
            Self::merge_take_count(run_count, mergeable, self.merge_fan_in, spill_early)
        else {
            drop(guard);
            self.is_merging.store(false, Ordering::Release);
            return None;
        };
        let taken: Vec<Arc<MemoryRun>> = guard[..take_n].to_vec();
        Some(taken)
    }

    /// Called after merge is complete.
    ///
    /// Atomically removes the `taken` runs from `self.runs` (they were kept in place during
    /// the merge so queries remained valid) and updates the watermark.
    /// The caller has already pushed the merged result to the next older age.
    pub fn complete_merge(&self, merged_height: i32, taken: &[Arc<MemoryRun>]) {
        // Build a set of raw pointers for the runs to remove (pointer identity, not clone).
        let taken_ptrs: std::collections::HashSet<*const MemoryRun> =
            taken.iter().map(Arc::as_ptr).collect();

        let mut lock = self.runs.write();
        if Arc::strong_count(&*lock) == 1 {
            // Unique outer Arc: retain in place — avoid allocating a new Vec of Arc clones.
            let runs = Arc::get_mut(&mut *lock).expect("strong_count==1");
            runs.retain(|r| !taken_ptrs.contains(&Arc::as_ptr(r)));
        } else {
            let old = Arc::clone(&*lock);
            let new_runs: Vec<Arc<MemoryRun>> = old
                .iter()
                .filter(|r| !taken_ptrs.contains(&Arc::as_ptr(r)))
                .cloned()
                .collect();
            *lock = Arc::new(new_runs);
        }
        drop(lock);
        self.bump_index_epoch();

        self.merged_to.fetch_max(merged_height, Ordering::Relaxed);
        self.is_merging.store(false, Ordering::Release);
    }

    /// Remove all entries with `height >= since` from mutable runs (reorg recovery).
    pub fn erase_since(&self, since: i32) {
        debug_assert!(self.is_mutable, "erase_since on frozen age");
        let mut lock = self.runs.write();
        let old = Arc::clone(&*lock);
        let mut new_runs: Vec<Arc<MemoryRun>> = Vec::with_capacity(old.len());
        for run in old.iter() {
            if run.height_range.0 >= since {
                // Entire run is at or after `since` — drop it.
                continue;
            }
            if run.height_range.1 < since {
                // Entire run is before `since` — keep as-is.
                new_runs.push(Arc::clone(run));
            } else {
                // Partial overlap — erase in place (requires clone since runs are Arc).
                let mut r = (**run).clone();
                r.erase_since(since);
                if !r.is_empty() {
                    new_runs.push(Arc::new(r));
                }
            }
        }
        *lock = Arc::new(new_runs);
        drop(lock);
        self.bump_index_epoch();
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::OutputKV;
    use super::*;

    fn make_key(n: u8) -> [u8; 36] {
        let mut k = [0u8; 36];
        k[0] = n;
        k
    }

    #[test]
    fn test_age_append_and_lookup() {
        let age = MemoryAge::new(true, 8);
        let k = make_key(1);
        age.append(vec![OutputKV::new_add(k, 100, 42)], 100);
        assert_eq!(age.lookup_key(&k, 0, i32::MAX), Some(42));
        assert_eq!(age.lookup_key(&make_key(2), 0, i32::MAX), None);
    }

    #[test]
    fn test_pin_prevents_merge() {
        let age = MemoryAge::new(false, 2);
        // Add 2 frozen runs to trigger fan-in.
        age.push_frozen_run(Arc::new(MemoryRun::build(vec![OutputKV::new_add(
            make_key(1),
            10,
            1,
        )])));
        age.push_frozen_run(Arc::new(MemoryRun::build(vec![OutputKV::new_add(
            make_key(2),
            20,
            2,
        )])));
        // Pin height 10 — in the merge range.
        let _pin = age.pin_height(10);
        assert!(
            !age.merge_ready(),
            "merge_ready should be false while height is pinned"
        );
    }

    #[test]
    fn heavy_backlog_takes_small_batches() {
        // 16 runs, all mergeable, fan_in=8 → take 2 (not 8) so spill-tier drains faster.
        assert_eq!(MemoryAge::merge_take_count(16, 16, 8, 0), Some(2));
        assert_eq!(MemoryAge::merge_take_count(16, 1, 8, 0), Some(1));
        // Not yet 2× fan_in: full fan-in when mergeable.
        assert_eq!(MemoryAge::merge_take_count(8, 8, 8, 0), Some(8));
        assert_eq!(MemoryAge::merge_take_count(10, 8, 8, 0), Some(8));
    }

    #[test]
    fn spill_early_take_drains_before_full_fan_in() {
        // Spill tier with take=2: merge at 2 runs (not wait for 8).
        assert_eq!(MemoryAge::merge_take_count(2, 2, 8, 2), Some(2));
        assert_eq!(MemoryAge::merge_take_count(6, 6, 8, 2), Some(2));
        // Still capped at spill_early even when mergeable ≥ fan_in.
        assert_eq!(MemoryAge::merge_take_count(8, 8, 8, 2), Some(2));
        // Disabled (0): legacy full fan-in.
        assert_eq!(MemoryAge::merge_take_count(6, 6, 8, 0), None);
        assert_eq!(MemoryAge::merge_take_count(8, 8, 8, 0), Some(8));
    }

    #[test]
    fn spill_early_take_on_age_merges_at_two_runs() {
        let age = MemoryAge::new(false, 8);
        age.set_spill_early_take(2);
        age.push_frozen_run(Arc::new(MemoryRun::build(vec![OutputKV::new_add(
            make_key(1),
            10,
            1,
        )])));
        assert!(!age.merge_ready(), "1 run must not merge");
        age.push_frozen_run(Arc::new(MemoryRun::build(vec![OutputKV::new_add(
            make_key(2),
            20,
            2,
        )])));
        assert!(age.merge_ready(), "spill_early=2 must merge at 2 runs");
        let taken = age.take_for_merge().expect("take 2");
        assert_eq!(taken.len(), 2);
        age.complete_merge(20, &taken);
        assert_eq!(age.run_count(), 0);
    }

    #[test]
    fn tip_pin_does_not_block_older_run_merge() {
        // Pipeline pins tip heights; older frozen runs below min(pin) must still merge.
        let age = MemoryAge::new(false, 2);
        for h in [10i32, 20, 30, 40] {
            age.push_frozen_run(Arc::new(MemoryRun::build(vec![OutputKV::new_add(
                make_key(h as u8),
                h,
                h as u64,
            )])));
        }
        let _pin = age.pin_height(35); // overlaps runs at 30..40, but 10+20 are below
        assert!(
            age.merge_ready(),
            "runs with height_range.max < min(pin) must remain mergeable"
        );
        let taken = age.take_for_merge().expect("should take older runs");
        assert_eq!(taken.len(), 2);
        assert!(taken.iter().all(|r| r.height_range.1 < 35));
        age.complete_merge(20, &taken);
        assert_eq!(age.run_count(), 2); // 30 and 40 remain
    }

    #[test]
    fn partial_merge_drains_when_backlogged_under_tip_pins() {
        // fan_in=8, 16 one-height runs, pin in the middle of the oldest window:
        // only a few runs are below min(pin), but backlog must still drain.
        let age = MemoryAge::new(false, 8);
        for h in 1i32..=16 {
            age.push_frozen_run(Arc::new(MemoryRun::build(vec![OutputKV::new_add(
                make_key((h % 200) as u8),
                h,
                h as u64,
            )])));
        }
        let _pin = age.pin_height(5); // oldest fan_in (1..8) overlaps; mergeable prefix = 4
        assert!(
            age.merge_ready(),
            "backlogged age must partial-merge below pin"
        );
        let taken = age.take_for_merge().expect("partial take");
        // At ≥2×fan_in backlog, take ≤2 so spill→disk writes stay small/frequent.
        assert_eq!(taken.len(), 2);
        assert!(taken.iter().all(|r| r.height_range.1 < 5));
        age.complete_merge(2, &taken);
        assert_eq!(age.run_count(), 14);
    }

    #[test]
    fn single_run_partial_merge_when_prefix_is_one() {
        // fan_in=8, 9 one-height runs, pin at height 2 → only run 1 is mergeable.
        let age = MemoryAge::new(false, 8);
        for h in 1i32..=9 {
            age.push_frozen_run(Arc::new(MemoryRun::build(vec![OutputKV::new_add(
                make_key((h % 200) as u8),
                h,
                h as u64,
            )])));
        }
        let _pin = age.pin_height(2);
        assert!(
            age.merge_ready(),
            "mergeable_prefix=1 must drain when backlogged"
        );
        let taken = age.take_for_merge().expect("1-run partial");
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].height_range.1, 1);
        age.complete_merge(1, &taken);
        assert_eq!(age.run_count(), 8);
    }

    #[test]
    fn test_erase_since_mutable() {
        let age = MemoryAge::new(true, 8);
        let k1 = make_key(1);
        let k2 = make_key(2);
        age.append(vec![OutputKV::new_add(k1, 50, 1)], 50);
        age.append(vec![OutputKV::new_add(k2, 100, 2)], 100);
        age.erase_since(75);
        assert_eq!(age.lookup_key(&k1, 0, i32::MAX), Some(1));
        assert_eq!(age.lookup_key(&k2, 0, i32::MAX), None);
    }

    #[test]
    fn append_in_place_survives_outer_arc_snapshot() {
        // Holding snapshot_runs() must not poison in-place append via Arc::make_mut
        // cloning every tip pointer (strong_count>1 → permanent slow path).
        reset_append_diagnostics_for_test();
        let age = MemoryAge::new(true, 8);
        // Seed tip (empty age always uses slow path to create the first mutable run).
        age.append(vec![OutputKV::new_add(make_key(1), 1, 1)], 1);
        let (in0, slow0, cont0) = memory_age_append_stats();
        assert!(slow0 >= 1, "empty age creates tip via slow path");
        assert_eq!(cont0, 0, "first-run create is structural, not contention");

        // Second append with unique outer Arc → in-place.
        age.append(vec![OutputKV::new_add(make_key(2), 2, 2)], 2);
        let (in1, slow1, _) = memory_age_append_stats();
        assert!(
            in1 > in0,
            "unique tip must append in-place (in={in1} prev={in0} slow={slow1})"
        );

        let _held = age.snapshot_runs(); // outer Arc strong_count >= 2
        age.append(vec![OutputKV::new_add(make_key(3), 3, 3)], 3);
        let (in2, slow2, cont2) = memory_age_append_stats();
        // Outer shared → try_append_in_place bails without make_mut; slow path replaces tip.
        assert!(
            slow2 > slow1,
            "shared outer Arc must take slow path without poisoning tip (slow={slow2} prev={slow1})"
        );
        assert!(
            cont2 > cont0,
            "shared outer Arc must count as contention (cont={cont2} prev={cont0})"
        );
        drop(_held);
        // After drop, in-place must work again.
        age.append(vec![OutputKV::new_add(make_key(4), 4, 4)], 4);
        let (in3, _, _) = memory_age_append_stats();
        assert!(
            in3 > in2,
            "after releasing snapshot, append must hit in-place again (in={in3} prev={in2})"
        );
    }

    #[test]
    fn throttle_ignores_structural_freeze_slow_path() {
        reset_append_diagnostics_for_test();
        // Mid-IBD shape: ~2/3 in-place, ~1/3 freeze-and-new, ~0 contention.
        bump_append_stats_detailed_for_test(70_000, 30_000, 0);
        assert_eq!(
            memory_age_throttle_slow_pct(),
            0,
            "structural freeze must not throttle the pipeline"
        );
    }

    #[test]
    fn throttle_slow_pct_prefers_recent_window_over_cumulative() {
        reset_append_diagnostics_for_test();
        bump_append_stats_detailed_for_test(90_000, 140_600, 140_600);
        set_append_window_baseline_for_test(90_000, 140_600);
        bump_append_stats_detailed_for_test(
            0,
            APPEND_WINDOW_MIN_SAMPLES,
            APPEND_WINDOW_MIN_SAMPLES,
        );
        assert_eq!(memory_age_throttle_slow_pct(), 100);
    }
}
