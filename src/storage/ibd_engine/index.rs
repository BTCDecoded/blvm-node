//! `UtxoIndex`: age-tiered UTXO index with disk overflow.
//!
//! Architecture:
//! - `K_AGES = 7` tiers allocated, but only ages `0..K_DISK_EVICTION_AGE` are used in memory.
//! - `K_MUTABLE_AGES = 3`: ages 0–2 accept appends and merged pushes from younger ages.
//! - `K_FAN_IN = 8`: each age merges once it holds ≥8 runs.
//! - `K_DISK_EVICTION_AGE = K_MUTABLE_AGES = 3`: merges from age 2 go to `DiskIndex` directly,
//!   bounding RSS to ≈ (8 + 64 + 512) × 50k ≈ 1.7 GB max (GC reduces this further).
//! - **Compacter**: 7 shared worker threads, one `crossbeam::channel<usize>` (age index).
//!   Any thread handles any age; the thread re-enqueues cascaded merges as needed.
//!
//! `contiguous_length` is updated per-append so the Table flusher can use it as a stable
//! watermark for `commit_before(h)` without an extra barrier.

use super::disk_index::DiskIndex;
use super::memory_age::{MemoryAge, Pin};
use super::memory_run::MemoryRun;
use super::types::{OutputId, OutputKV};
use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

thread_local! {
    /// Last `batch_query` split (ms). Read by SpendSession after `db.query`.
    static LAST_QUERY_AGES_MS: Cell<u64> = const { Cell::new(0) };
    static LAST_QUERY_DISK_MS: Cell<u64> = const { Cell::new(0) };
    /// F5a: DiskIndex I/O from most recent `batch_query` on this thread.
    static LAST_DISK_PREADS: Cell<u64> = const { Cell::new(0) };
    static LAST_DISK_PREAD_KB: Cell<u64> = const { Cell::new(0) };
    static LAST_DISK_MAX_PREAD_KB: Cell<u64> = const { Cell::new(0) };
    static LAST_DISK_CANDS: Cell<u64> = const { Cell::new(0) };
    static LAST_DISK_SEGS: Cell<u64> = const { Cell::new(0) };
}

/// `(ages_ms, disk_ms)` from the most recent `batch_query` on this thread.
pub fn take_last_query_split_ms() -> (u64, u64) {
    (
        LAST_QUERY_AGES_MS.with(Cell::get),
        LAST_QUERY_DISK_MS.with(Cell::get),
    )
}

/// `(preads, pread_kb, max_pread_kb, cands, segs)` from the most recent disk fallback.
pub fn take_last_disk_io_stats() -> (u64, u64, u64, u64, u64) {
    (
        LAST_DISK_PREADS.with(Cell::get),
        LAST_DISK_PREAD_KB.with(Cell::get),
        LAST_DISK_MAX_PREAD_KB.with(Cell::get),
        LAST_DISK_CANDS.with(Cell::get),
        LAST_DISK_SEGS.with(Cell::get),
    )
}

/// Bytes currently held by compacter threads: in-flight merge Vecs + disk eviction bloom filters.
/// These are transient allocations NOT reflected in `mem_usage_bytes()` but visible as
/// UNEXPLAINED_ANON in MEM_REPORT. Tracked globally to surface in diagnostics.
pub(crate) static COMPACTER_INFLIGHT_BYTES: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
extern crate tempfile;

/// Number of age tiers.
const K_AGES: usize = 7;
/// Oldest mutable age (ages 0..K_MUTABLE are mutable).
const K_MUTABLE_AGES: usize = 3;
/// Fan-in: trigger merge after this many runs in one age.
const K_FAN_IN: usize = 8;
/// Number of compacter worker threads.
/// Reduced from 7 to 5: each compacter thread has its own mimalloc local heap.
/// With 7 threads, the 7 heaps accumulate ~100-200 MB of free-page caches that
/// show up as UNEXPLAINED_ANON. However, 3 was too few: at h=400k+ (164 BPS),
/// age-2 accumulates 65+ unmerged runs (1675 MB total) when only 3 threads handle
/// all age levels simultaneously. This creates mega-merge spikes that temporarily
/// allocate 1.6+ GB and inflate mimalloc virtual to 45+ GB. 5 threads keeps age-2
/// ≤ 8-16 runs while saving 2 heaps worth of cache vs 7. The net UNEXPLAINED_ANON
/// with 5 threads (300 MB cache) is far less than 65-run mega-merges (1675 MB).
const K_COMPACTER_THREADS: usize = 5;

/// Choose the disk eviction age.
///
/// `avail_mb`: available RAM in MiB at the time the index is opened (from the
/// caller's [`MemoryGuard`] or `ram_tier::probe_avail_ram_mib()`).
///
/// The UTXO index entry count in memory is bounded by:
///   Σ_{i=1}^{age} K_FAN_IN^i × MUTABLE_RUN_MAX_ENTRIES entries
///
/// `MUTABLE_RUN_MAX_ENTRIES = 10_000` and `K_FAN_IN = 8`.
///
/// During a fan-in merge at the deepest age, K_FAN_IN source runs stay resident
/// (queryable) while the merged output is built — a transient **2× spike**.
///
/// | Eviction age | Max index entries | Index RSS  | Merge-spike peak |
/// |---|---|---|---|
/// | 3 (min)  |   5.84M entries |  ~303 MB  |  ~532 MB  |
/// | 4        |  46.8M entries  |   ~2.4 GB |   ~4.3 GB |
/// | 5        |   374M entries  |  ~19.4 GB |   ~34 GB  |
/// | 6        |    ~3B entries  | ~156 GB   |  ~272 GB  |
///
/// Threshold: age N is safe when `avail_mb > merge_peak_for_N + ~10 GB` (non-index blvm).
/// We use `avail_mb` (MemAvailable), not total RAM — this already accounts for OS overhead
/// and co-resident services. Using MemTotal over-estimates headroom on shared machines.
///
/// Spill-tier early merge batch size (`BLVM_IBD_SPILL_MERGE_TAKE`).
///
/// Default **0** (legacy fan_in-only). Opt-in take=2 was tried on synth 300→400 and
/// **regressed** wall BPS vs lever C (≈451/205 vs ≈473/237) despite smaller spills;
/// tip 400→500 also cliffed under take=2. Keep available for A/B, never default on.
fn spill_merge_take_from_env() -> usize {
    static CACHED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("BLVM_IBD_SPILL_MERGE_TAKE")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n <= K_FAN_IN)
            .unwrap_or(0)
    })
}

/// When set (`1`/`true`/`yes`), Elevated pressure (level 1) does **not** demote
/// eviction age. Critical still demotes unless `BLVM_IBD_CRITICAL_NO_DEMOTE` is set;
/// Emergency floors unless tip-crawl supply is healthy (C2 / KEEP C0).
///
/// Synth 300→400: Elevated demote 4→3 was measured as a phase-2 accelerator toward
/// mega disk spills; this gate lets us A/B residency without losing Critical safety.
/// Not OnceLock — must be flip-able in unit tests / adaptive A/B without process restart
/// for the value itself (binary still restarted per adaptive iter).
fn elevated_no_demote_from_env() -> bool {
    matches!(
        std::env::var("BLVM_IBD_ELEVATED_NO_DEMOTE")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// When set (`1`/`true`/`yes`), Critical pressure (level 2) does **not** demote
/// eviction age. Emergency (level 3) still demotes to the mutable floor.
///
/// F6 synth: tip **regressed** (Emergency still demoted; disk_ms worse). Leave off.
fn critical_no_demote_from_env() -> bool {
    matches!(
        std::env::var("BLVM_IBD_CRITICAL_NO_DEMOTE")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// When set (`1`/`true`/`yes`), Critical demotes **one step** (`boot-1`) instead of
/// jumping to the mutable floor. Emergency still jumps to the floor.
///
/// F7: age=5 + elev still Critical `5→3` and tip cratered. Soft demote makes age=5
/// Critical become `5→4` (keep one frozen age resident). No-op at boot=4 (4→3).
fn critical_soft_demote_from_env() -> bool {
    matches!(
        std::env::var("BLVM_IBD_CRITICAL_SOFT_DEMOTE")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// F11: when set, HotPin is **not** cleared on Critical (level 2) — only Emergency (3).
/// Default off (F10 behavior: clear on Critical+).
fn hot_pin_keep_on_critical_from_env() -> bool {
    matches!(
        std::env::var("BLVM_IBD_HOT_PIN_KEEP_ON_CRITICAL")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// HP-M1: defer HotPin clear under Emergency until sustained for this many ms
/// (default 0 = immediate clear, F11 behavior). Brief Emergency spikes (often <1s after
/// mega-pin install) otherwise drop a multi-GiB HotPin and tip `disk_ms` cliffs.
/// Critical still honors `HOT_PIN_KEEP_ON_CRITICAL`; this only gates level‑3 clears.
fn hot_pin_emergency_hold_ms_from_env() -> u64 {
    std::env::var("BLVM_IBD_HOT_PIN_EMERGENCY_HOLD_MS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// P0c: Hornet-like tip residency. Elev+Critical keep boot age; merges stay in RAM
/// (force-disk off; eviction treated as `K_AGES` until Emergency floors live age).
/// Requires `EVICTION_AGE≥5` in practice — age=4 still has no RAM home past age[3].
fn tip_resident_from_env() -> bool {
    matches!(
        std::env::var("BLVM_IBD_TIP_RESIDENT")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Hornet parity: oldest RAM age never merges to DiskIndex.
///
/// At boot `eviction_age=E`, merges that would spill (`age_idx+1 >= E`) fold the
/// merged run back into `ages[E-1]` instead of `push_run_no_compact`. Elev+Critical
/// keep boot age; Emergency still floors and allows disk spill. Prefer this over
/// age5+`TIP_RESIDENT` on Zeus@64G — F20 Emergency'd on the age5 merge spike.
fn oldest_accumulate_from_env() -> bool {
    matches!(
        std::env::var("BLVM_IBD_OLDEST_ACCUMULATE")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// F17: defer Critical→mutable-floor demote until Critical has been sustained for this
/// many milliseconds (default 0 = immediate demote, current behavior).
/// Brief pin-install RSS spikes often clear in <2s; holding boot age across them avoids
/// mega age[3] spills. Emergency still demotes immediately. Soft/no-demote envs win first.
fn critical_demote_hold_ms_from_env() -> u64 {
    std::env::var("BLVM_IBD_CRITICAL_DEMOTE_HOLD_MS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// W52: spill a merge product to disk when it would otherwise stay in a deeper RAM age.
///
/// Defaults: ≥1536 MiB source runs **or** ≥6_000_000 merged entries. Override with
/// `BLVM_IBD_FORCE_DISK_MERGE_MB` / `BLVM_IBD_FORCE_DISK_MERGE_ENTRIES` (0 disables that arm).
fn force_merge_spill_to_disk(source_bytes: u64, merged_len: usize) -> bool {
    let mb_thr: u64 = std::env::var("BLVM_IBD_FORCE_DISK_MERGE_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1536);
    let entry_thr: usize = std::env::var("BLVM_IBD_FORCE_DISK_MERGE_ENTRIES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6_000_000);
    (mb_thr > 0 && source_bytes >= mb_thr.saturating_mul(1024 * 1024))
        || (entry_thr > 0 && merged_len >= entry_thr)
}

/// `BLVM_IBD_ENGINE_EVICTION_AGE` overrides auto-detection.
fn choose_eviction_age(avail_mb: u64) -> usize {
    if let Ok(s) = std::env::var("BLVM_IBD_ENGINE_EVICTION_AGE") {
        if let Ok(n) = s.trim().parse::<usize>() {
            let clamped = n.clamp(K_MUTABLE_AGES, K_AGES);
            tracing::info!(
                "UTXO engine: eviction age = {} (from BLVM_IBD_ENGINE_EVICTION_AGE)",
                clamped
            );
            return clamped;
        }
    }

    // Use available RAM (MemAvailable from /proc/meminfo) as the headroom budget.
    // This already accounts for OS overhead, page cache, and co-resident processes
    // (vLLM, IDE, etc.) that have claimed physical pages.  Using MemTotal here
    // over-estimates the headroom on shared machines, causing OOM at age=4 or age=5.
    //
    // Headroom required per age (steady-state + merge-spike + non-index blvm ~10 GB):
    //   age=5: ~19.4 GB steady + ~34 GB spike + 10 GB other ≈ 63 GB available
    //   age=4:  ~2.4 GB steady +  ~4.3 GB spike + 10 GB other ≈ 17 GB available
    //   age=3:  ~0.3 GB steady +  ~0.5 GB spike + 10 GB other ≈ 11 GB (always safe)
    //
    // `avail_mb` is the MemAvailable at engine open, which is the right signal.
    // Fall back to MemAvailable from /proc/meminfo directly if not provided.
    // Read total physical RAM for the log line only — decisions use avail_mb.
    let total_mb = proc_mem_total_mb().unwrap_or(avail_mb);

    let age = if avail_mb >= 65 * 1024 {
        // ≥65 GiB available: age-5 merge peak (~34 GB) fits with room to spare.
        5
    } else if avail_mb >= 20 * 1024 {
        // ≥20 GiB available: age-4 merge peak (~4.3 GB) fits comfortably.
        4
    } else {
        // <20 GiB available: evict at age 3 (spike ~530 MB, OOM-safe).
        // Common on machines where GPU LLMs or other services consume most RAM.
        3
    };

    // Note: a no-swap cap here was previously applied but removed. The OOM at h=550k+
    // occurs regardless of age=3 or age=4 (same height, same anon RSS at death). The
    // root cause is not the in-memory index size but something else in the engine or
    // pipeline. OOM prevention is now handled by the RSS hard gate in the dispatch loop
    // (validation_loop.rs [RSS_HARD_GATE]) which pauses block ingestion when anon RSS
    // exceeds 92% of budget, letting the engine reclaim memory before continuing.

    tracing::info!(
        "UTXO engine: eviction age = {} (auto-detected: {:.1} GiB physical RAM, {:.1} GiB available)",
        age,
        total_mb as f64 / 1024.0,
        avail_mb as f64 / 1024.0,
    );
    age
}

/// Read `MemTotal` from `/proc/meminfo` and return it in MiB. Returns `None` on any I/O error.
fn proc_mem_total_mb() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in content.lines() {
        if line.starts_with("MemTotal:") {
            let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

/// Manages background merges across all age tiers.
///
/// One `crossbeam::channel` shared by all `K_COMPACTER_THREADS` worker threads.
/// Any thread picks up any age index posted to the channel and runs its merge.
struct Compacter {
    tx: crossbeam_channel::Sender<usize>,
    /// Live eviction tier (ages at or above this spill to disk). Updated under memory pressure.
    eviction_age_live: Arc<AtomicUsize>,
    _threads: Vec<std::thread::JoinHandle<()>>,
}

impl Compacter {
    fn start(
        ages: Arc<[MemoryAge; K_AGES]>,
        disk_index: Arc<DiskIndex>,
        boot_eviction_age: usize,
    ) -> Self {
        let eviction_age_live = Arc::new(AtomicUsize::new(boot_eviction_age));
        let (tx, rx) = crossbeam_channel::unbounded::<usize>();
        let mut threads = Vec::with_capacity(K_COMPACTER_THREADS);
        for _ in 0..K_COMPACTER_THREADS {
            let rx = rx.clone();
            let tx = tx.clone();
            let ages = Arc::clone(&ages);
            let disk_index = Arc::clone(&disk_index);
            let eviction_age_live = Arc::clone(&eviction_age_live);
            let handle = std::thread::Builder::new()
                .name("utxo-compacter".to_string())
                .spawn(move || {
                    while let Ok(age_idx) = rx.recv() {
                        if age_idx == usize::MAX {
                            break; // shutdown sentinel
                        }
                        let eviction_age = eviction_age_live.load(Ordering::Acquire);
                        run_merge_for_age(&ages, age_idx, &disk_index, &tx, eviction_age);
                    }
                })
                .expect("spawn compacter thread");
            threads.push(handle);
        }
        Self {
            tx,
            eviction_age_live,
            _threads: threads,
        }
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

fn run_merge_for_age(
    ages: &[MemoryAge; K_AGES],
    age_idx: usize,
    disk_index: &Arc<DiskIndex>,
    tx: &crossbeam_channel::Sender<usize>,
    eviction_age: usize,
) {
    let age = &ages[age_idx];
    let runs_before = age.run_count();
    // Snapshot the oldest runs for merge. They remain in the age and are still queryable
    // throughout this merge pass — preventing the "UTXO invisible during merge" race.
    let Some(runs) = age.take_for_merge() else {
        // Log when we have many runs but can't merge (pin-blocked or already-merging)
        if runs_before >= age.merge_fan_in * 2 {
            age.merge_ready_logged(); // logs the blocking pin if any
        }
        return;
    };
    let t_merge = std::time::Instant::now();
    // Account for source runs that will be duplicated in the merged output.
    // The merge allocates a Vec<OutputKV> of capacity = sum(source run lengths).
    let source_bytes: u64 = runs.iter().map(|r| r.mem_bytes() as u64).sum();
    COMPACTER_INFLIGHT_BYTES.fetch_add(source_bytes, Ordering::Relaxed);

    // K-way merge — all expensive work (sort, bloom, directory build) happens here,
    // outside any lock. `MemoryRun::merge` calls `build_presorted` (no redundant sort).
    let merged = MemoryRun::merge(&runs);

    COMPACTER_INFLIGHT_BYTES.fetch_sub(source_bytes, Ordering::Relaxed);
    let merge_ms = t_merge.elapsed().as_millis() as u64;
    let max_h = runs
        .iter()
        .map(|r| r.height_range().1)
        .max()
        .unwrap_or(i32::MIN);
    let merged_len = merged.len();
    // W52: live WAN soak 2026-07-16 h≈285k — age[2] merge kept ~13M entries in RAM
    // (eviction_age=4 → to_disk=false), jemalloc allocated 6→14 GiB in one minute, host
    // swap +17 GiB / Cursor OOM while cgroup oom_kill=0. Cap in-RAM merge products.
    // P0c TIP_RESIDENT: while live eviction is above the mutable floor, keep merges in
    // RAM (force-disk off; treat eviction as K_AGES). Emergency floors to K_MUTABLE_AGES
    // and restores normal spill / force-disk.
    // OLDEST_ACCUMULATE: Hornet DoMerge no-op on oldest — fold into ages[E-1], not DiskIndex.
    let emergency_floor = eviction_age <= K_MUTABLE_AGES;
    let tip_hold = tip_resident_from_env() && !emergency_floor;
    let oldest_acc = oldest_accumulate_from_env() && !emergency_floor;
    // TIP_RESIDENT forbids force-disk until Emergency. OLDEST_ACCUMULATE keeps Critical
    // from demoting but still allows W52 size-cap spills (F23 FORCE=0 → Emergency@47G).
    let force_disk = !tip_hold && force_merge_spill_to_disk(source_bytes, merged_len);
    let effective_eviction = if tip_hold && !oldest_acc {
        K_AGES
    } else {
        eviction_age
    };
    let would_leave_ram = (age_idx + 1) >= effective_eviction || force_disk;
    // Fold eviction-tier merges into ages[E-1]; force_disk / Emergency still hit DiskIndex.
    let fold_oldest = oldest_acc && would_leave_ram && !force_disk && !merged.is_empty();
    let to_disk = !merged.is_empty() && would_leave_ram && !fold_oldest;

    if !merged.is_empty() {
        let next_idx = age_idx + 1;
        if fold_oldest {
            let dest = effective_eviction.saturating_sub(1).min(K_AGES - 1);
            ages[dest].push_frozen_run(Arc::new(merged));
            if ages[dest].merge_ready() {
                let _ = tx.send(dest);
            }
        } else if next_idx < effective_eviction && !force_disk {
            // Push the already-built merged run to the next memory age BEFORE removing
            // source runs. `push_frozen_run` holds the write lock only for an Arc push.
            ages[next_idx].push_frozen_run(Arc::new(merged));

            // *** CRITICAL: cascade the merge. ***
            // After pushing to age N+1, age N+1 may now be merge_ready. Without re-enqueueing
            // it here, frozen ages accumulate runs indefinitely — Add+Delete pairs never cancel,
            // memory grows without bound, and disk eviction never fires.
            if ages[next_idx].merge_ready() {
                let _ = tx.send(next_idx);
            }
        } else {
            // Spill tier → disk (or W52 size cap). Write the segment ONLY while holding
            // is_merging; do NOT compact here (compact can take minutes on 8×30M-entry
            // segments and would freeze this age's drain — see COMPACTER_GATE stalls).
            if force_disk && (age_idx + 1) < eviction_age {
                tracing::warn!(
                    "UTXO compacter: age[{}] force-spill to disk (source_mb={} entries={}) — avoid merge RAM spike",
                    age_idx,
                    source_bytes / (1024 * 1024),
                    merged_len
                );
            }
            if let Err(e) = disk_index.push_run_no_compact(merged) {
                tracing::error!("UTXO engine: disk eviction failed — data may be lost: {e}");
            }
        }
    }

    // Atomically remove the source runs. Merged data is now in the next age or on disk.
    // MUST happen before disk compact so other threads can keep draining this age.
    let runs_before_complete = age.run_count();
    age.complete_merge(max_h, &runs);
    let runs_after = age.run_count();
    // After complete_merge, `runs` goes out of scope and frees source run memory.
    drop(runs);

    // INFO only for spill→disk (the multi-second path); lower ages are too chatty at ~1k BPS.
    if to_disk || merge_ms >= 500 {
        tracing::info!(
            "UTXO compacter: age[{}] merged {}→{} runs, out_entries={}, merge_ms={}, to_disk={}",
            age_idx,
            runs_before,
            runs_after,
            merged_len,
            merge_ms,
            to_disk,
        );
    } else {
        tracing::debug!(
            "UTXO compacter: age[{}] merged {}→{} runs, out_entries={}, merge_ms={}",
            age_idx,
            runs_before,
            runs_after,
            merged_len,
            merge_ms,
        );
    }

    // Disk segment compaction AFTER releasing is_merging, on a dedicated thread so
    // this merge worker can immediately drain more spill runs.
    if to_disk {
        disk_index.compact_oldest_async();
    }

    // If this age is still above the merge threshold, re-enqueue immediately rather
    // than waiting for the next push_frozen_run trigger. Without this, a backlogged age
    // (e.g. 72 runs) would only drain one merge-fan-in (8 runs) per incoming push (~64
    // blocks apart), causing unbounded memory accumulation.
    if age.merge_ready() {
        let _ = tx.send(age_idx);
    }
}

/// 7-age UTXO index with disk overflow.
///
/// The primary lookup structure for the IBD engine. Memory ages hold recent data;
/// when the deepest age overflows it evicts to `DiskIndex` (cold storage on disk).
/// This bounds memory to approximately `K_FAN_IN^K_AGES` mutable-run-cap worth of entries
/// rather than growing linearly with all blocks processed.
pub struct UtxoIndex {
    ages: Arc<[MemoryAge; K_AGES]>,
    compacter: Compacter,
    /// Cold storage for entries evicted from the deepest memory age.
    disk_index: Arc<DiskIndex>,
    /// Highest height for which all blocks up to and including it have been appended.
    contiguous_length: AtomicI32,
    /// Eviction age chosen at open (from `choose_eviction_age`).
    boot_eviction_age: usize,
    /// Unix ms when Critical first observed under `CRITICAL_DEMOTE_HOLD_MS` (0 = clear).
    critical_entered_ms: AtomicU64,
    /// Unix ms when Emergency first observed under `HOT_PIN_EMERGENCY_HOLD_MS` (0 = clear).
    emergency_pin_entered_ms: AtomicU64,
}

impl UtxoIndex {
    /// Open (or create) the index with a `seg_dir` for disk-evicted segment files.
    ///
    /// `avail_mb`: available system RAM in MiB (from `MemoryGuard` or
    /// `ram_tier::probe_avail_ram_mib()`).  Used by `choose_eviction_age` to decide
    /// how many index tiers remain in memory vs. are spilled to disk.
    pub fn open(seg_dir: &Path, avail_mb: u64) -> anyhow::Result<Self> {
        let (disk_index, restored_cl) = DiskIndex::new(seg_dir)?;
        Self::open_with_disk(Arc::new(disk_index), avail_mb, restored_cl, None)
    }

    pub(super) fn open_with_disk(
        disk_index: Arc<DiskIndex>,
        avail_mb: u64,
        restored_cl: i32,
        table_path: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let eviction_age = choose_eviction_age(avail_mb);
        let index_epoch = Arc::new(AtomicU64::new(0));
        let ages_raw: [MemoryAge; K_AGES] = std::array::from_fn(|i| {
            let is_mutable = i < K_MUTABLE_AGES;
            let enqueue = None; // set below via compacter
            MemoryAge::new_with_hooks(
                is_mutable,
                K_FAN_IN,
                enqueue,
                Some(Arc::clone(&index_epoch)),
            )
        });

        let ages = Arc::new(ages_raw);
        let compacter = Compacter::start(Arc::clone(&ages), Arc::clone(&disk_index), eviction_age);
        // Mark spill-tier ages for early take-2 (or env override) before any appends.
        {
            let take = spill_merge_take_from_env();
            if take > 0 {
                tracing::info!(
                    "UTXO engine: spill-tier early merge take={take} \
                     (BLVM_IBD_SPILL_MERGE_TAKE; 0 disables)"
                );
            }
            for i in 0..K_AGES {
                let is_spill = i + 1 >= eviction_age;
                ages[i].set_spill_early_take(if is_spill { take } else { 0 });
            }
        }
        let mut restored_cl = restored_cl;
        if let Some(tp) = table_path {
            if let Some(file_cl) = super::meta::read_contiguous_length_sidecar(tp) {
                if restored_cl < 0 && file_cl >= 0 {
                    restored_cl = file_cl;
                } else if file_cl > restored_cl {
                    tracing::warn!(
                        "UTXO engine: contiguous_length sidecar={file_cl} > segment max={restored_cl} \
                         — clamping to segment max (in-memory tail not persisted)"
                    );
                }
            }
        }
        let contiguous_length = AtomicI32::new(restored_cl);
        if restored_cl >= 0 {
            super::set_gc_fence(restored_cl);
            tracing::info!(
                "UTXO engine: restored contiguous_length={} (segments + sidecar)",
                restored_cl,
            );
        }

        Ok(Self {
            ages,
            compacter,
            disk_index,
            contiguous_length,
            boot_eviction_age: eviction_age,
            critical_entered_ms: AtomicU64::new(0),
            emergency_pin_entered_ms: AtomicU64::new(0),
        })
    }

    /// React to IBD memory-guard pressure (see `parallel_ibd::memory::MemoryGuard`).
    ///
    /// `level_u8`: `0`=None, `1`=Elevated, `2`=Critical, `3`=Emergency (matches `PressureLevel`).
    ///
    /// Lowers the in-memory eviction tier and floods the compacter so index tiers spill to
    /// disk before process RSS exceeds the guard's `rss_budget_mb`. Restores the boot tier
    /// when pressure returns to None.
    /// Approximate resident memory in bytes across all in-memory age tiers and disk-index blooms.
    ///
    /// Used by `MEM_REPORT` to account for engine memory that would otherwise appear as
    /// `UNEXPLAINED_ANON`. Does not include the UtxoTable flat file (pread path, not mmap'd).
    pub fn mem_bytes(&self) -> usize {
        let age_bytes: usize = self.ages.iter().map(|a| a.mem_bytes()).sum();
        let disk_bytes = self.disk_index.bloom_bytes_total();
        age_bytes + disk_bytes
    }

    /// Run count for a specific age tier. Lightweight (one read-lock + len()).
    /// Used by the dispatch loop for compacter backpressure without paying full age_detail cost.
    pub fn age_run_count(&self, age_idx: usize) -> usize {
        if age_idx < K_AGES {
            self.ages[age_idx].run_count()
        } else {
            0
        }
    }

    /// Whether age `age_idx` currently holds `is_merging` (COMPACTER_GATE diagnostics).
    pub fn age_is_merging(&self, age_idx: usize) -> bool {
        if age_idx < K_AGES {
            self.ages[age_idx].is_merging.load(Ordering::Relaxed)
        } else {
            false
        }
    }

    /// Whether a disk-segment compaction is in progress.
    pub fn disk_is_compacting(&self) -> bool {
        self.disk_index.is_compacting()
    }

    /// Whether a DiskIndex spill segment file write is in progress (`SPILL_IO_GATE`).
    pub fn spill_io_busy(&self) -> bool {
        self.disk_index.spill_io_busy()
    }

    /// On-disk segment count (COMPACTER_GATE diagnostics).
    pub fn disk_segment_count(&self) -> usize {
        self.disk_index.segment_count()
    }

    /// Per-age diagnostics: (run_count, mem_mb) for each age tier, plus disk (segment_count, bloom_mb).
    ///
    /// Used by MEM_REPORT to show per-tier breakdown so operators can see if the compacter
    /// is falling behind (runs accumulating in an age tier) or if a specific tier dominates.
    pub fn age_detail(&self) -> (Vec<(usize, u64)>, (usize, u64)) {
        let ages: Vec<(usize, u64)> = self
            .ages
            .iter()
            .map(|a| {
                // Read under guard — do not snapshot_runs(); MEM_REPORT must not pin tip Arcs.
                let guard = a.runs.read();
                let bytes: usize = guard.iter().map(|r| r.mem_bytes()).sum();
                (guard.len(), bytes as u64 / (1024 * 1024))
            })
            .collect();
        let disk_segs = self.disk_index.segment_count();
        let disk_bloom_mb = self.disk_index.bloom_bytes_total() as u64 / (1024 * 1024);
        (ages, (disk_segs, disk_bloom_mb))
    }

    pub fn memory_pressure_tick(&self, level_u8: u8) {
        let boot = self.boot_eviction_age;
        // Higher pressure → lower eviction age (more aggressive disk spill).
        // K_MUTABLE_AGES (3) is the floor — below it there are no frozen ages to evict.
        let tip_res = tip_resident_from_env();
        let oldest_acc = oldest_accumulate_from_env();
        let target = match level_u8 {
            // C2 / KEEP C0: Emergency + healthy tip-crawl must not floor ages.
            // @416287 view doubled (14099≈validate) after the first Emergency tick
            // spilled the index; gd was 26. Raw Emergency (depth 8 / flush) stays.
            // Unhealthy supply still floors — real starve + reclaim.
            3 if crate::node::parallel_ibd::tip_stage::tip_crawl_supply_healthy_now() => {
                self.critical_entered_ms.store(0, Ordering::Relaxed);
                boot
            }
            3 => {
                self.critical_entered_ms.store(0, Ordering::Relaxed);
                K_MUTABLE_AGES // Emergency + no healthy supply: spill
            }
            // Critical: default → mutable floor. `CRITICAL_NO_DEMOTE` / `TIP_RESIDENT`
            // / `OLDEST_ACCUMULATE` keep boot (F6 alone REVERT tip — residency envs also
            // forbid DiskIndex spill until Emergency).
            // Land E: tip-crawl + healthy supply (fast GD / fresh LOCAL_STREAM) holds boot
            // like Elevated — portable Chunk B without raising RSS budgets.
            // `CRITICAL_SOFT_DEMOTE` → boot-1 (for age≥5).
            // F17: `CRITICAL_DEMOTE_HOLD_MS` defers floor until Critical is sustained.
            2 if tip_res
                || oldest_acc
                || critical_no_demote_from_env()
                || crate::node::parallel_ibd::tip_stage::tip_crawl_supply_healthy_now() =>
            {
                self.critical_entered_ms.store(0, Ordering::Relaxed);
                boot
            }
            2 if critical_soft_demote_from_env() => {
                self.critical_entered_ms.store(0, Ordering::Relaxed);
                boot.saturating_sub(1).max(K_MUTABLE_AGES)
            }
            2 => {
                let hold = critical_demote_hold_ms_from_env();
                if hold == 0 {
                    self.critical_entered_ms.store(0, Ordering::Relaxed);
                    K_MUTABLE_AGES
                } else {
                    let now = unix_now_ms();
                    let since = self.critical_entered_ms.load(Ordering::Relaxed);
                    let since = if since == 0 {
                        self.critical_entered_ms.store(now, Ordering::Relaxed);
                        now
                    } else {
                        since
                    };
                    if now.saturating_sub(since) < hold {
                        boot // defer floor demote across brief Critical spikes
                    } else {
                        K_MUTABLE_AGES
                    }
                }
            }
            // W52: Elevated must actually tighten (was boot.min(5) → no-op at boot=4 while
            // age-3 held 5 GiB and anon spiked to 18 GiB before Critical).
            // Opt-out: BLVM_IBD_ELEVATED_NO_DEMOTE=1 keeps boot age under Elevated only.
            1 if tip_res || oldest_acc || elevated_no_demote_from_env() => {
                self.critical_entered_ms.store(0, Ordering::Relaxed);
                boot
            }
            1 => {
                self.critical_entered_ms.store(0, Ordering::Relaxed);
                boot.saturating_sub(1).max(K_MUTABLE_AGES)
            }
            _ => {
                self.critical_entered_ms.store(0, Ordering::Relaxed);
                boot // None: restore boot age
            }
        };
        let live = &self.compacter.eviction_age_live;
        let prev = live.load(Ordering::Relaxed);
        if target != prev {
            live.store(target, Ordering::Release);
            // Spill tier moves with live eviction age — retarget early-merge marks.
            let take = spill_merge_take_from_env();
            for i in 0..K_AGES {
                let is_spill = i + 1 >= target;
                self.ages[i].set_spill_early_take(if is_spill { take } else { 0 });
            }
            if target < prev {
                tracing::warn!(
                    "UTXO engine: memory pressure level {} — eviction age {} → {} (spilling index to disk)",
                    level_u8,
                    prev,
                    target
                );
            } else if level_u8 == 0 {
                tracing::info!(
                    "UTXO engine: memory pressure cleared — eviction age restored to {}",
                    target
                );
            }
        }
        if level_u8 >= 2 {
            // F10: default — drop HotPin under Critical+.
            // F11: BLVM_IBD_HOT_PIN_KEEP_ON_CRITICAL=1 → only Emergency (3) clears.
            // HP-M1: BLVM_IBD_HOT_PIN_EMERGENCY_HOLD_MS defers that Emergency clear.
            let drop_pin = if level_u8 >= 3 {
                let hold = hot_pin_emergency_hold_ms_from_env();
                if hold == 0 {
                    self.emergency_pin_entered_ms.store(0, Ordering::Relaxed);
                    true
                } else {
                    let now = unix_now_ms();
                    let since = self.emergency_pin_entered_ms.load(Ordering::Relaxed);
                    let since = if since == 0 {
                        self.emergency_pin_entered_ms.store(now, Ordering::Relaxed);
                        now
                    } else {
                        since
                    };
                    let sustained = now.saturating_sub(since) >= hold;
                    if !sustained {
                        tracing::info!(
                            "DiskSegment: hot-pin clear deferred (emergency hold {}ms, elapsed {}ms)",
                            hold,
                            now.saturating_sub(since)
                        );
                    }
                    sustained
                }
            } else {
                self.emergency_pin_entered_ms.store(0, Ordering::Relaxed);
                !hot_pin_keep_on_critical_from_env()
            };
            if drop_pin {
                // Dens late-view confirm: keep seed HotPin under Emergency clear.
                self.disk_index.clear_hot_pins_keep_seed();
            }
            for i in 0..K_AGES {
                if self.ages[i].merge_ready() {
                    self.compacter.enqueue(i);
                }
            }
        } else {
            self.emergency_pin_entered_ms.store(0, Ordering::Relaxed);
        }
        // mi_collect removed from pressure tick — see index.rs compacter comment.
    }

    /// In-memory-only index for tests (uses a temp directory, cleaned up on drop).
    #[cfg(test)]
    pub fn new_for_test() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let idx = Self::open(tmp.path(), 8 * 1024).expect("UtxoIndex::open"); // avail_mb hint (age chosen by proc_mem_total_mb)
        // The TempDir would be dropped here, deleting the directory. To prevent that while
        // keeping the test simple, we deliberately leak it — it's a test-only temp dir.
        std::mem::forget(tmp);
        idx
    }

    /// Append a block's UTXO ops (Add + Delete entries) into the mutable tip (age 0).
    ///
    /// Returns a `Pin` keeping `height` resident in the mutable window until dropped.
    /// C4 tried Hornet `ages_[kMutableAges-1]` pin — **REVERT** tip 370→380 **230.3**
    /// vs F14c **268.9** (`baseline-runs/c4-pin-age2`). Keep age-0 pin.
    pub fn append(&self, entries: Vec<OutputKV>, height: i32) -> Pin {
        let pin = self.ages[0].pin_height(height);
        self.ages[0].append(entries, height);
        self.contiguous_length.fetch_max(height, Ordering::Relaxed);

        // Enqueue merge-ready ages through the spill tier (eviction_age-1), not only
        // mutable 0..2. Age-3 (disk spill when eviction_age=4) previously relied solely on
        // cascade-from-a2; under load that left a3 with 50+ runs / 12GB while a2 stayed healthy.
        let spill_hi = self
            .compacter
            .eviction_age_live
            .load(Ordering::Relaxed)
            .saturating_sub(1)
            .min(K_AGES - 1);
        for i in 0..=spill_hi {
            if self.ages[i].merge_ready() {
                self.compacter.enqueue(i);
            }
        }
        pin
    }

    /// Live disk-eviction age (ages at or above this spill to disk).
    pub fn eviction_age_live(&self) -> usize {
        self.compacter.eviction_age_live.load(Ordering::Relaxed)
    }

    /// Query all ages for `key`. Returns `Some(id)` from the newest age that has it.
    ///
    /// Used by `UtxoDatabase::query` (sorted batch path). For single-key lookup during
    /// intra-block resolution.
    pub fn lookup_key(&self, key: &[u8; 36]) -> Option<OutputId> {
        for age in self.ages.iter() {
            if let Some(id) = age.lookup_key(key, 0, i32::MAX) {
                // Filter internal sentinel — callers expect Some(real_id) or None.
                if id == super::types::OUTPUT_ID_DELETED {
                    return None;
                }
                return Some(id);
            }
        }
        None
    }

    /// Batch query: fills `ids[i]` for each `keys[i]` across all ages and disk overflow.
    ///
    /// `ids` must be pre-filled with `OutputId::MAX` (sentinel for "not yet resolved").
    /// Ages are queried newest-to-oldest; the disk index is the cold fallback for
    /// any keys still unresolved (MAX) after all memory ages.
    ///
    /// `before` is an exclusive upper bound on `entry.height`. `SpendSession` passes
    /// `before = height` so that Deletes recorded for the current block are invisible.
    ///
    /// On return, `ids[i]` is either a real `OutputId` (found) or `OutputId::MAX` (not found).
    pub fn batch_query(&self, keys: &[[u8; 36]], ids: &mut [OutputId], before: i32) {
        debug_assert_eq!(keys.len(), ids.len());
        // Per-age query under `runs.read()` — never clone the outer `Arc<Vec<MemoryRun>>`
        // (snapshot_runs / query_cache). Extra Vec Arc clones force `Arc::make_mut` to copy
        // the whole run list and bump tip `strong_count`, blocking in-place appends.
        // Early-exit: short-circuit `any(MAX)` (pre-N22 / champ path). N22 remaining
        // counter REVERT on synth S10 dens floor.
        let t_ages = Instant::now();
        for i in 0..K_AGES {
            if !ids.contains(&OutputId::MAX) {
                break;
            }
            self.ages[i].batch_query(keys, ids, 0, before);
        }
        let ages_ms = t_ages.elapsed().as_millis() as u64;
        // Disk fallback — keys with OUTPUT_ID_DELETED are already resolved (spent in
        // memory) so disk_index skips them. disk_index.batch_query also normalizes
        // any remaining OUTPUT_ID_DELETED → OutputId::MAX before returning.
        let t_disk = Instant::now();
        if ids.contains(&OutputId::MAX) {
            self.disk_index.batch_query(keys, ids, before);
        }
        let disk_ms = t_disk.elapsed().as_millis() as u64;
        LAST_QUERY_AGES_MS.with(|c| c.set(ages_ms));
        LAST_QUERY_DISK_MS.with(|c| c.set(disk_ms));
        let (preads, pread_kb, max_kb, cands, segs) = super::disk_segment::take_disk_io_stats();
        LAST_DISK_PREADS.with(|c| c.set(preads));
        LAST_DISK_PREAD_KB.with(|c| c.set(pread_kb));
        LAST_DISK_MAX_PREAD_KB.with(|c| c.set(max_kb));
        LAST_DISK_CANDS.with(|c| c.set(cands));
        LAST_DISK_SEGS.with(|c| c.set(segs));
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

    /// Seed the index from a checkpoint import (resume after SIGKILL). Sets contiguous_length
    /// exactly to `checkpoint_height` without posting compacter work.
    ///
    /// Large seed batches (e.g. 250M entries = 14 GB at h=640k) are written **directly to a
    /// disk segment** rather than through age-0 → age-1 → age-2 → disk.  The cascade creates
    /// multiple full copies of the UTXO set in RAM (each tier merge materialises a new Vec),
    /// causing 70+ GB RSS and an OOM kill before the next checkpoint export can run.
    ///
    /// The direct-to-disk path keeps peak extra RSS at O(entries.len() × 56 B) — exactly one
    /// Vec, sorted in-place and streamed to disk, then freed.  Memory ages stay empty, so the
    /// next `iter_live_at_height` call's `mem_entries` collection stays tiny.
    pub fn seed_checkpoint(&self, mut entries: Vec<OutputKV>, checkpoint_height: i32) {
        if !entries.is_empty() {
            // Sort in-place (no extra allocation beyond the existing Vec).
            entries.sort_unstable();
            match self.disk_index.push_sorted_segment_owned(entries) {
                Ok(()) => {
                    // Entries consumed by write_owned (HotPin keeps body when eligible;
                    // otherwise dropped after disk write).
                }
                Err(e) => {
                    // Disk write failed — entries already moved; cannot age-0 fallback.
                    // Streaming seed path is the production resume; this batch path is rare.
                    tracing::error!(
                        "seed_checkpoint: disk write failed ({e:#}) — UTXO seed incomplete"
                    );
                }
            }
        }
        self.contiguous_length
            .store(checkpoint_height, Ordering::Release);
        // Initialise the GC fence to the checkpoint height. This prevents compaction from
        // cancelling Add+Delete pairs for UTXOs that were live at the checkpoint but spent
        // after it — those Add entries must survive until the next checkpoint's scan.
        // The fence is advanced when the next checkpoint export starts.
        super::set_gc_fence(checkpoint_height);
    }

    /// Allocate a disk-segment slot for the streaming seed writer thread.
    ///
    /// Returns `(seg_idx, seg_dir)` — the caller writes the segment file and then calls
    /// `finalize_seed` to register it and commit the checkpoint height.
    pub fn alloc_seed_seg(&self) -> (usize, std::path::PathBuf) {
        self.disk_index.alloc_seg()
    }

    /// Register the streaming-seed segment and commit `contiguous_length` / GC fence.
    pub fn finalize_seed(&self, seg: super::disk_segment::DiskSegment, checkpoint_height: i32) {
        self.disk_index.register_seg(seg);
        self.contiguous_length
            .store(checkpoint_height, Ordering::Release);
        super::set_gc_fence(checkpoint_height);
    }

    /// Remove all UTXO ops at `height >= since` from mutable ages. Reorg recovery.
    pub fn erase_since(&self, since: i32) {
        for i in 0..K_MUTABLE_AGES {
            self.ages[i].erase_since(since);
        }
        // Roll back contiguous_length.
        self.contiguous_length
            .fetch_min(since - 1, Ordering::Relaxed);
    }

    /// Iterate all non-cancelled Add entries across all ages and disk segments.
    ///
    /// Used only by the **small-scale** [`crate::storage::ibd_engine::watermark_export`]
    /// helper. Tip-scale Phase 3 must use [`crate::storage::ibd_engine::run_watermark_export`]
    /// / streaming checkpoint export — this materializes every disk-segment Add+Delete into
    /// one `Vec` and OOMs near tip (live 2026-07-13: ~10→59 GB anon in seconds).
    pub fn scan_all_live(&self) -> Vec<OutputKV> {
        // Atomically snapshot disk segments AND copy memory-age entries while holding the
        // DiskIndex read lock. This closes the race where a compacter thread evicts a
        // MemoryAge run to DiskIndex between the disk scan and the memory-age scan.
        //
        // Copy OutputKV bytes (not Arc<MemoryRun> snapshots): holding tip Arcs across the
        // multi-second disk scan blocks in-place append for the whole export.
        let (disk_segs, mem_entries_pre) = {
            let guard = self.disk_index.segments.read();
            let disk = Arc::clone(&*guard);
            let mut mem: Vec<OutputKV> = Vec::new();
            for a in self.ages.iter().rev() {
                a.collect_entries_into(&mut mem);
            }
            // HP-M3: include in-flight async spills (not yet on disk).
            for run in self.disk_index.pending_spills.read().iter() {
                mem.extend_from_slice(&run.entries);
            }
            (disk, mem)
            // read lock drops here — compaction is unblocked for the long scan below
        };

        if disk_segs.len() > 2 || mem_entries_pre.len() > 1_000_000 {
            tracing::warn!(
                "scan_all_live: materializing full index (segs={}, mem_entries={}) — \
                 tip-scale callers must use streaming checkpoint export (run_watermark_export)",
                disk_segs.len(),
                mem_entries_pre.len()
            );
        }

        // Collect ALL entries (adds AND deletes) into a flat Vec.
        // This avoids a HashMap whose per-entry overhead exceeds the entry itself
        // (~120 bytes/entry vs 56 bytes for OutputKV), saving 10–15 GB at 170M UTXOs.
        let mut all_entries: Vec<OutputKV> = mem_entries_pre;

        for seg in disk_segs.iter() {
            let entries = match seg.read_all_entries() {
                Ok(e) => e,
                Err(err) => {
                    tracing::warn!("scan_all_live: skipping segment {:?}: {err}", seg.path);
                    continue;
                }
            };
            all_entries.extend_from_slice(&entries);
        }

        // Sort by OutputKV natural order: key ASC, height DESC, Add before Delete.
        // After sorting, the FIRST entry for each key is the most recent operation.
        // A single linear pass then determines whether each key is live (Add) or spent (Delete).
        all_entries.sort_unstable();

        let mut result: Vec<OutputKV> = Vec::new();
        let mut i = 0;
        while i < all_entries.len() {
            // First entry for this key = most recent (highest height, or Add before Delete).
            let first = all_entries[i];
            if first.is_add() {
                result.push(first);
            }
            // Skip all remaining entries for this key.
            let key = first.key;
            i += 1;
            while i < all_entries.len() && all_entries[i].key == key {
                i += 1;
            }
        }

        result
    }

    /// Like `scan_all_live` but only considers entries with `height <= max_height`.
    ///
    /// This returns the UTXO set as it was at `max_height` even if the engine has already
    /// advanced beyond that height. Used by periodic mid-IBD watermark exports so we can
    /// snapshot an earlier height while validation continues running concurrently.
    ///
    /// Correctness: entries created after `max_height` are excluded; deletions after
    /// `max_height` are also excluded, so UTXOs spent after `max_height` still appear live —
    /// exactly the UTXO set at `max_height`.
    pub fn scan_live_at_height(&self, max_height: i32) -> Vec<OutputKV> {
        // Pre-scan compaction: merge all disk segments into one with GC applied.
        // The caller has already set CHECKPOINT_GC_FENCE = max_height, so all
        // Add+Delete pairs where Delete.height <= max_height are cancelled during
        // compaction.  After this call, disk segments contain only live UTXOs
        // (Add entries without a Delete at or below max_height), shrinking the
        // scan's working set from O(total_spends) to O(live_UTXOs) and preventing
        // the multi-GB allocation spike that caused OOMs at h > 200k.
        self.disk_index.compact_for_checkpoint_sync();

        // Atomically snapshot disk segments AND copy memory-age entries while holding the
        // DiskIndex read lock. Prevents the race where a compacter evicts a MemoryAge
        // run to DiskIndex between the disk scan and the memory-age scan, dropping
        // UTXOs from the checkpoint snapshot entirely.
        // Copy OutputKV (not Arc<MemoryRun>) so tip Arcs are not pinned across the scan.
        let (disk_segs, mut all_entries) = {
            let guard = self.disk_index.segments.read();
            let disk = Arc::clone(&*guard);
            let mut mem: Vec<OutputKV> = Vec::new();
            for a in self.ages.iter().rev() {
                a.collect_entries_at_or_below_into(max_height, &mut mem);
            }
            for run in self.disk_index.pending_spills.read().iter() {
                for e in &run.entries {
                    if e.height <= max_height {
                        mem.push(*e);
                    }
                }
            }
            (disk, mem)
            // read lock drops here — compaction is unblocked for the long scan below
        };

        // Collect all entries with height <= max_height into a flat Vec.
        // Use streaming reads (SegmentReader) instead of read_all_entries() to avoid
        // loading the full segment (potentially 11 GB for 200 M entries) into RAM.
        // After compact_for_checkpoint_sync, the segment contains only live UTXOs at
        // max_height (~20 M entries = ~1.1 GB) — well within budget.
        for seg in disk_segs.iter() {
            let mut reader = seg.stream();
            loop {
                match reader.advance() {
                    Ok(Some(entry)) => {
                        if entry.height <= max_height {
                            all_entries.push(entry);
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        tracing::warn!(
                            "scan_live_at_height: read error on segment {:?}: {err}",
                            seg.path
                        );
                        break;
                    }
                }
            }
        }

        // Sort by OutputKV natural order: key ASC, height DESC, Add before Delete.
        // After sorting, the FIRST entry for each key is the most recent operation
        // at or below max_height. If it's an Add the UTXO is live; Delete means spent.
        all_entries.sort_unstable();

        let mut result: Vec<OutputKV> = Vec::new();
        let mut i = 0;
        while i < all_entries.len() {
            let first = all_entries[i];
            if first.is_add() {
                result.push(first);
            }
            let key = first.key;
            i += 1;
            while i < all_entries.len() && all_entries[i].key == key {
                i += 1;
            }
        }

        result
    }

    /// Return a streaming iterator over live UTXOs as of `max_height`.
    ///
    /// Unlike `scan_live_at_height` (which builds a full `Vec<OutputKV>` — ~14 GB at 250M
    /// UTXOs), this streams disk entries one chunk at a time via `SegmentReader` and collects
    /// memory entries into a small sorted Vec (≤5.84M entries ≈ 327 MB at `eviction_age=3`).
    ///
    /// Peak memory beyond baseline RSS: ~330 MB (stream state) + ~150 MB (export chunk buffers).
    pub fn iter_live_at_height(
        &self,
        max_height: i32,
    ) -> anyhow::Result<(CheckpointStream, u64, u64)> {
        let t_compact = std::time::Instant::now();
        // Compact disk segments with GC applied at max_height.
        self.disk_index.compact_for_checkpoint_sync();
        let compact_ms = t_compact.elapsed().as_millis() as u64;

        let t_scan_prep = std::time::Instant::now();
        // Atomic snapshot under read lock — prevents the race where a compacter evicts a
        // MemoryAge run between the disk scan and the memory-age scan.
        // Copy OutputKV entries (not Arc<MemoryRun> snapshots) so the mutable tip is not
        // pinned for the lifetime of CheckpointStream.
        let (disk_segs, mut mem_entries) = {
            let guard = self.disk_index.segments.read();
            let disk = Arc::clone(&*guard);
            let mut mem: Vec<OutputKV> = Vec::new();
            for a in self.ages.iter().rev() {
                a.collect_entries_at_or_below_into(max_height, &mut mem);
            }
            (disk, mem)
        };

        // Memory entries already filtered to height <= max_height.
        // At eviction_age=3 this is ≤5.84M entries = ~327 MB.
        mem_entries.sort_unstable();

        // Open one streaming reader per disk segment. After compact_for_checkpoint_sync,
        // typically just one segment. Each reader buffers READER_CHUNK entries (~1 MB).
        let mut readers: Vec<super::disk_segment::SegmentReader> =
            disk_segs.iter().map(|seg| seg.stream()).collect();

        // Pre-fetch first entry from each reader.
        let mut heads: Vec<Option<OutputKV>> = Vec::with_capacity(readers.len());
        for reader in &mut readers {
            heads.push(reader.advance()?);
        }

        let scan_prep_ms = t_scan_prep.elapsed().as_millis() as u64;
        Ok((
            CheckpointStream {
                mem: mem_entries,
                mem_pos: 0,
                readers,
                heads,
                max_height,
                last_key: None,
            },
            compact_ms,
            scan_prep_ms,
        ))
    }

    /// Compact disk segments for checkpoint export, optionally piggybacking live `Add` rows
    /// from the final re-GC pass into `sink`.
    pub fn compact_for_checkpoint_sync_with_sink<F>(
        &self,
        checkpoint_height: i32,
        on_live: Option<F>,
    ) -> anyhow::Result<u64>
    where
        F: FnMut(OutputKV) -> anyhow::Result<()>,
    {
        let t_compact = std::time::Instant::now();
        self.disk_index
            .compact_for_checkpoint_sync_with_sink(checkpoint_height, on_live)?;
        Ok(t_compact.elapsed().as_millis() as u64)
    }

    /// Copy in-memory age entries with `height <= max_height` (no disk scan).
    pub fn collect_memory_entries_at_or_below(&self, max_height: i32) -> Vec<OutputKV> {
        let mut mem: Vec<OutputKV> = Vec::new();
        for a in self.ages.iter().rev() {
            a.collect_entries_at_or_below_into(max_height, &mut mem);
        }
        mem.sort_unstable();
        mem
    }
}

/// Streaming live-UTXO iterator for checkpoint exports.
///
/// Performs a k-way merge of (small) in-memory age entries with (large) on-disk
/// `DiskSegment` entries. Both sources are pre-sorted by `OutputKV::Ord` (key ASC,
/// height DESC, Add before Delete). Entries beyond `max_height` are skipped without
/// consuming the key slot. Each key is deduplicated: the first valid entry determines
/// whether the UTXO is live (Add → yield) or spent (Delete → skip).
///
/// Peak allocations:
///   - `mem`: ≤5.84M × 56 B ≈ 327 MB  (eviction_age=3)
///   - per disk reader: `READER_CHUNK` entries ≈ 1 MB
pub struct CheckpointStream {
    mem: Vec<OutputKV>,
    mem_pos: usize,
    readers: Vec<super::disk_segment::SegmentReader>,
    heads: Vec<Option<OutputKV>>,
    max_height: i32,
    last_key: Option<[u8; 36]>,
}

impl CheckpointStream {
    /// Advance to the next live UTXO (Add, height ≤ max_height, per-key deduplicated).
    /// Returns `Ok(None)` when all sources are exhausted.
    pub fn next_live(&mut self) -> anyhow::Result<Option<OutputKV>> {
        loop {
            let entry = match self.pick_min()? {
                Some(e) => e,
                None => return Ok(None),
            };

            // Skip entries beyond the checkpoint — do NOT mark key as seen.
            // A later entry for the same key at valid height is still the deciding one.
            // Example: Delete(h=700k) then Add(h=100) → UTXO is live at fence=640k.
            if entry.height > self.max_height {
                continue;
            }

            // Dedup: first entry per key (highest valid height) determines live/spent.
            if Some(entry.key) == self.last_key {
                continue;
            }
            self.last_key = Some(entry.key);

            if entry.is_add() {
                return Ok(Some(entry));
            }
            // Delete at or before max_height → UTXO is spent, do not yield.
        }
    }

    fn pick_min(&mut self) -> anyhow::Result<Option<OutputKV>> {
        let mut best: Option<OutputKV> = None;
        let mut best_is_disk = false;
        let mut best_disk_idx: usize = 0;

        // Check memory cursor.
        if let Some(&me) = self.mem.get(self.mem_pos) {
            best = Some(me);
        }

        // Check each disk head; pick the global minimum under OutputKV::Ord.
        for i in 0..self.heads.len() {
            if let Some(de) = self.heads[i] {
                let take = match best {
                    None => true,
                    Some(cur) => de < cur,
                };
                if take {
                    best = Some(de);
                    best_is_disk = true;
                    best_disk_idx = i;
                }
            }
        }

        match best {
            None => Ok(None),
            Some(e) => {
                if best_is_disk {
                    self.heads[best_disk_idx] = self.readers[best_disk_idx].advance()?;
                } else {
                    self.mem_pos += 1;
                }
                Ok(Some(e))
            }
        }
    }
}

impl Drop for UtxoIndex {
    fn drop(&mut self) {
        self.compacter.shutdown();
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

    #[serial_test::serial(ibd)]
    #[test]
    fn w52_force_merge_spill_thresholds() {
        // Defaults: 1536 MiB source or 6M entries.
        assert!(!force_merge_spill_to_disk(1024 * 1024 * 1024, 1_000_000));
        assert!(force_merge_spill_to_disk(1536 * 1024 * 1024, 1));
        assert!(force_merge_spill_to_disk(1, 6_000_000));
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn test_append_and_query() {
        let idx = UtxoIndex::new_for_test();
        let k = make_key(1);
        let _pin = idx.append(vec![OutputKV::new_add(k, 100, 42)], 100);
        assert_eq!(idx.lookup_key(&k), Some(42));
        assert_eq!(idx.lookup_key(&make_key(2)), None);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn test_batch_query() {
        let idx = UtxoIndex::new_for_test();
        let k1 = make_key(1);
        let k2 = make_key(2);
        let _p1 = idx.append(vec![OutputKV::new_add(k1, 100, 10)], 100);
        let _p2 = idx.append(vec![OutputKV::new_add(k2, 101, 20)], 101);
        let mut ids = [OutputId::MAX; 2];
        idx.batch_query(&[k1, k2], &mut ids, i32::MAX);
        assert_eq!(ids[0], 10);
        assert_eq!(ids[1], 20);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn test_contiguous_length() {
        let idx = UtxoIndex::new_for_test();
        assert_eq!(idx.contiguous_length(), -1);
        let k = make_key(1);
        let _pin = idx.append(vec![OutputKV::new_add(k, 50, 1)], 50);
        assert_eq!(idx.contiguous_length(), 50);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn test_erase_since() {
        let idx = UtxoIndex::new_for_test();
        let k1 = make_key(1);
        let k2 = make_key(2);
        let _p1 = idx.append(vec![OutputKV::new_add(k1, 50, 1)], 50);
        let _p2 = idx.append(vec![OutputKV::new_add(k2, 100, 2)], 100);
        idx.erase_since(75);
        assert_eq!(idx.lookup_key(&k1), Some(1));
        assert_eq!(idx.lookup_key(&k2), None);
    }

    /// Serialize demote-env tests — they mutate process env and race under rayon.
    fn demote_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn critical_pressure_lowers_eviction_age_to_mutable_floor() {
        use std::sync::atomic::Ordering;
        let _guard = demote_env_lock();
        // SAFETY: exclusive via demote_env_lock; env restored before return.
        unsafe {
            std::env::remove_var("BLVM_IBD_ELEVATED_NO_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_NO_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_SOFT_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_DEMOTE_HOLD_MS");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk_index, restored_cl) = DiskIndex::new(tmp.path()).expect("DiskIndex::new");
        let idx = UtxoIndex::open_with_disk(Arc::new(disk_index), 24 * 1024, restored_cl, None)
            .expect("open_with_disk");
        assert_eq!(idx.boot_eviction_age, 4);
        idx.memory_pressure_tick(2);
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            K_MUTABLE_AGES,
        );
        std::mem::forget(tmp);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn tip_crawl_supply_healthy_holds_boot_under_critical() {
        use std::sync::atomic::Ordering;
        let _guard = demote_env_lock();
        let _tip = crate::node::parallel_ibd::tip_stage::test_tip_atomics_lock();
        crate::node::parallel_ibd::tip_stage::test_reset_tip_stage();
        // SAFETY: exclusive via demote_env_lock; env restored before return.
        unsafe {
            std::env::remove_var("BLVM_IBD_ELEVATED_NO_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_NO_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_SOFT_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_DEMOTE_HOLD_MS");
        }
        crate::node::parallel_ibd::tip_stage::publish_wan_body_tip(100);
        crate::node::parallel_ibd::tip_stage::mark_needed(200);
        crate::node::parallel_ibd::tip_stage::test_seed_getdata_body_ewma(40, 32);
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk_index, restored_cl) = DiskIndex::new(tmp.path()).expect("DiskIndex::new");
        let idx = UtxoIndex::open_with_disk(Arc::new(disk_index), 24 * 1024, restored_cl, None)
            .expect("open_with_disk");
        let boot = idx.boot_eviction_age;
        idx.memory_pressure_tick(2); // Critical — Land E holds boot
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            boot,
            "tip-crawl healthy supply must not floor eviction age on Critical"
        );
        idx.memory_pressure_tick(3); // Emergency + healthy — C2 holds boot (KEEP C0)
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            boot,
            "KEEP C0: Emergency + healthy supply must not floor ages (view-double)"
        );
        crate::node::parallel_ibd::tip_stage::test_reset_getdata_body_ewma();
        crate::node::parallel_ibd::tip_stage::publish_wan_body_tip(100);
        crate::node::parallel_ibd::tip_stage::mark_needed(200);
        idx.memory_pressure_tick(3); // Emergency + unhealthy — still floors
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            K_MUTABLE_AGES,
            "Emergency + unhealthy supply must still floor"
        );
        crate::node::parallel_ibd::tip_stage::test_reset_tip_stage();
        crate::node::parallel_ibd::tip_stage::test_reset_getdata_body_ewma();
        std::mem::forget(tmp);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn elevated_no_demote_keeps_boot_age_under_elevated() {
        use std::sync::atomic::Ordering;
        let _guard = demote_env_lock();
        // SAFETY: exclusive via demote_env_lock; env restored before return.
        unsafe {
            std::env::set_var("BLVM_IBD_ELEVATED_NO_DEMOTE", "1");
            std::env::remove_var("BLVM_IBD_CRITICAL_NO_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_SOFT_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_DEMOTE_HOLD_MS");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk_index, restored_cl) = DiskIndex::new(tmp.path()).expect("DiskIndex::new");
        let idx = UtxoIndex::open_with_disk(Arc::new(disk_index), 24 * 1024, restored_cl, None)
            .expect("open_with_disk");
        let boot = idx.boot_eviction_age;
        assert!(boot >= 4);
        idx.memory_pressure_tick(1); // Elevated — must not demote
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            boot,
        );
        idx.memory_pressure_tick(2); // Critical — still demotes
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            K_MUTABLE_AGES,
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_ELEVATED_NO_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_NO_DEMOTE");
        }
        std::mem::forget(tmp);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn critical_no_demote_keeps_boot_age_under_critical() {
        use std::sync::atomic::Ordering;
        let _guard = demote_env_lock();
        // SAFETY: exclusive via demote_env_lock; env restored before return.
        unsafe {
            std::env::set_var("BLVM_IBD_ELEVATED_NO_DEMOTE", "1");
            std::env::set_var("BLVM_IBD_CRITICAL_NO_DEMOTE", "1");
            std::env::remove_var("BLVM_IBD_CRITICAL_SOFT_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_DEMOTE_HOLD_MS");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk_index, restored_cl) = DiskIndex::new(tmp.path()).expect("DiskIndex::new");
        let idx = UtxoIndex::open_with_disk(Arc::new(disk_index), 24 * 1024, restored_cl, None)
            .expect("open_with_disk");
        let boot = idx.boot_eviction_age;
        assert!(boot >= 4);
        idx.memory_pressure_tick(2); // Critical — must not demote
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            boot,
        );
        idx.memory_pressure_tick(3); // Emergency — still demotes
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            K_MUTABLE_AGES,
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_ELEVATED_NO_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_NO_DEMOTE");
        }
        std::mem::forget(tmp);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn critical_soft_demote_steps_one_from_boot() {
        use std::sync::atomic::Ordering;
        let _guard = demote_env_lock();
        // SAFETY: exclusive via demote_env_lock; env restored before return.
        unsafe {
            std::env::remove_var("BLVM_IBD_CRITICAL_NO_DEMOTE");
            std::env::set_var("BLVM_IBD_CRITICAL_SOFT_DEMOTE", "1");
            std::env::set_var("BLVM_IBD_ENGINE_EVICTION_AGE", "5");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk_index, restored_cl) = DiskIndex::new(tmp.path()).expect("DiskIndex::new");
        let idx = UtxoIndex::open_with_disk(Arc::new(disk_index), 24 * 1024, restored_cl, None)
            .expect("open_with_disk");
        assert_eq!(idx.boot_eviction_age, 5);
        idx.memory_pressure_tick(2); // Critical — soft → 4, not mutable floor
        assert_eq!(idx.compacter.eviction_age_live.load(Ordering::Relaxed), 4,);
        idx.memory_pressure_tick(3); // Emergency — floor
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            K_MUTABLE_AGES,
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_CRITICAL_SOFT_DEMOTE");
            std::env::remove_var("BLVM_IBD_ENGINE_EVICTION_AGE");
        }
        std::mem::forget(tmp);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn critical_demote_hold_defers_floor_then_demotes() {
        use std::sync::atomic::Ordering;
        let _guard = demote_env_lock();
        // SAFETY: exclusive via demote_env_lock; env restored before return.
        unsafe {
            std::env::remove_var("BLVM_IBD_CRITICAL_NO_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_SOFT_DEMOTE");
            std::env::set_var("BLVM_IBD_CRITICAL_DEMOTE_HOLD_MS", "200");
            std::env::set_var("BLVM_IBD_ENGINE_EVICTION_AGE", "4");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk_index, restored_cl) = DiskIndex::new(tmp.path()).expect("DiskIndex::new");
        let idx = UtxoIndex::open_with_disk(Arc::new(disk_index), 24 * 1024, restored_cl, None)
            .expect("open_with_disk");
        let boot = idx.boot_eviction_age;
        assert_eq!(boot, 4);
        idx.memory_pressure_tick(2); // first Critical — hold boot
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            boot,
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
        idx.memory_pressure_tick(2); // sustained — floor
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            K_MUTABLE_AGES,
        );
        idx.memory_pressure_tick(0); // clear streak
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            boot,
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_CRITICAL_DEMOTE_HOLD_MS");
            std::env::remove_var("BLVM_IBD_ENGINE_EVICTION_AGE");
        }
        std::mem::forget(tmp);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn hot_pin_emergency_hold_defers_clear() {
        use std::sync::atomic::Ordering;
        let _guard = demote_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_HOT_PIN", "1");
            std::env::set_var("BLVM_IBD_HOT_PIN_KEEP_ON_CRITICAL", "1");
            std::env::set_var("BLVM_IBD_HOT_PIN_EMERGENCY_HOLD_MS", "200");
            std::env::set_var("BLVM_IBD_ENGINE_EVICTION_AGE", "4");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk_index, restored_cl) = DiskIndex::new(tmp.path()).expect("DiskIndex::new");
        let idx = UtxoIndex::open_with_disk(Arc::new(disk_index), 24 * 1024, restored_cl, None)
            .expect("open_with_disk");
        idx.memory_pressure_tick(3); // first Emergency — defer clear
        assert!(
            idx.emergency_pin_entered_ms.load(Ordering::Relaxed) > 0,
            "emergency hold streak should start"
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
        idx.memory_pressure_tick(3); // sustained — clear allowed (streak may remain)
        idx.memory_pressure_tick(0); // pressure clear resets streak
        assert_eq!(idx.emergency_pin_entered_ms.load(Ordering::Relaxed), 0);
        unsafe {
            std::env::remove_var("BLVM_IBD_HOT_PIN");
            std::env::remove_var("BLVM_IBD_HOT_PIN_KEEP_ON_CRITICAL");
            std::env::remove_var("BLVM_IBD_HOT_PIN_EMERGENCY_HOLD_MS");
            std::env::remove_var("BLVM_IBD_ENGINE_EVICTION_AGE");
        }
        std::mem::forget(tmp);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn tip_resident_keeps_boot_under_critical_emergency_floors() {
        unsafe {
            std::env::set_var("BLVM_IBD_TIP_RESIDENT", "1");
            std::env::set_var("BLVM_IBD_ENGINE_EVICTION_AGE", "5");
            std::env::remove_var("BLVM_IBD_CRITICAL_NO_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_SOFT_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_DEMOTE_HOLD_MS");
            std::env::remove_var("BLVM_IBD_ELEVATED_NO_DEMOTE");
            std::env::remove_var("BLVM_IBD_OLDEST_ACCUMULATE");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk_index, restored_cl) = DiskIndex::new(tmp.path()).expect("DiskIndex::new");
        let idx = UtxoIndex::open_with_disk(Arc::new(disk_index), 24 * 1024, restored_cl, None)
            .expect("open_with_disk");
        assert_eq!(idx.boot_eviction_age, 5);
        idx.memory_pressure_tick(1); // Elevated — tip_res keeps boot
        assert_eq!(idx.compacter.eviction_age_live.load(Ordering::Relaxed), 5,);
        idx.memory_pressure_tick(2); // Critical — tip_res keeps boot
        assert_eq!(idx.compacter.eviction_age_live.load(Ordering::Relaxed), 5,);
        idx.memory_pressure_tick(3); // Emergency — floor
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            K_MUTABLE_AGES,
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_TIP_RESIDENT");
            std::env::remove_var("BLVM_IBD_ENGINE_EVICTION_AGE");
        }
        std::mem::forget(tmp);
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn oldest_accumulate_keeps_boot_under_critical_emergency_floors() {
        unsafe {
            std::env::set_var("BLVM_IBD_OLDEST_ACCUMULATE", "1");
            std::env::set_var("BLVM_IBD_ENGINE_EVICTION_AGE", "4");
            std::env::remove_var("BLVM_IBD_TIP_RESIDENT");
            std::env::remove_var("BLVM_IBD_CRITICAL_NO_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_SOFT_DEMOTE");
            std::env::remove_var("BLVM_IBD_CRITICAL_DEMOTE_HOLD_MS");
            std::env::remove_var("BLVM_IBD_ELEVATED_NO_DEMOTE");
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk_index, restored_cl) = DiskIndex::new(tmp.path()).expect("DiskIndex::new");
        let idx = UtxoIndex::open_with_disk(Arc::new(disk_index), 24 * 1024, restored_cl, None)
            .expect("open_with_disk");
        assert_eq!(idx.boot_eviction_age, 4);
        idx.memory_pressure_tick(2);
        assert_eq!(idx.compacter.eviction_age_live.load(Ordering::Relaxed), 4,);
        idx.memory_pressure_tick(3);
        assert_eq!(
            idx.compacter.eviction_age_live.load(Ordering::Relaxed),
            K_MUTABLE_AGES,
        );
        unsafe {
            std::env::remove_var("BLVM_IBD_OLDEST_ACCUMULATE");
            std::env::remove_var("BLVM_IBD_ENGINE_EVICTION_AGE");
        }
        std::mem::forget(tmp);
    }
}
