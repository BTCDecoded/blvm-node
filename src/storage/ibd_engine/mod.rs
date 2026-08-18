//! IBD UTXO Engine — age-tiered in-memory index with disk overflow for Initial Block Download.
//!
//! # Architecture
//!
//! ```text
//! UtxoDatabase
//!   ├── UtxoTable   — flat append-only file + in-memory tail
//!   │                 stores {OutputHeader (16B) || script_bytes} per UTXO
//!   └── UtxoIndex   — 7-age UTXO index (ages[0]=newest, ages[6]=oldest)
//!         ├── MemoryAge[0..2]  — mutable (accepts appends from orchestrator)
//!         ├── MemoryAge[3..6]  — frozen (compacter-only appends)
//!         └── Compacter        — 7 shared threads, one crossbeam channel
//!               each thread: take N runs from one age → merge → push to next age
//! ```
//!
//! # Key sizes
//! - `OutputKey = [u8; 36]` (txid 32B + vout u32 BE 4B) — smaller than legacy [u8; 40]
//! - `OutputKV  = 52 bytes` per index entry (height + id as separate fields)
//! - Bloom filter: ~12 bits/entry, ~1% FPR (7 probes, 64-byte blocked layout)
//! - Directory: prefix-bucket index, ~85 entries/bucket (~4 KB binary search range)
//!
//! # Usage (Phase 2 wire-in)
//! ```rust,ignore
//! // Orchestrator thread (sequential):
//! let pin = db.append(&block, &tx_ids, height)?;
//!
//! // Worker thread (parallel):
//! let session = SpendSession::resolve(&db, &block, &tx_ids, height);
//! let utxo_set = session_to_utxo_set(&session);
//! let result = parallel_ibd.validate_block_only(..., &mut utxo_set, ...);
//! drop(pin); // release height from mutable window
//! ```
//!
//! # Phase 1 scope
//! Module built and tested in isolation. No wire-in to IBD pipeline during Phase 1.
//! Phase 2 adds `SpendSession` and updates `validation_loop.rs`.

pub mod database;
pub mod disk_index;
pub mod disk_segment;
pub mod export;
pub mod file_io;
pub mod import;
pub mod index;
pub mod memory_age;
pub mod memory_run;
pub mod meta;
pub mod spend_session;
pub mod table;
pub mod types;

pub use database::UtxoDatabase;
pub use export::{
    CKPT_TREE_A, CKPT_TREE_B, CheckpointExportTimings, IBD_UTXOS_TREE, Phase3Finish,
    ckpt_inactive_slot, ckpt_tree_for_slot, is_ibd_utxo_tree_name, phase3_path,
    run_checkpoint_export_replace, run_watermark_export, sync_tree_after_persist,
};
pub use import::{bootstrap_ckpt_from_legacy_standalone, seed_from_ibd_utxos};
pub use memory_run::{advance_gc_fence_to, set_gc_fence};
pub use meta::{
    clear_engine_dirty_flag, contiguous_length_sidecar, engine_dirty_flag_path,
    read_contiguous_length_sidecar, remove_contiguous_length_sidecar,
    write_contiguous_length_sidecar,
};
pub use index::take_last_query_split_ms;
pub use spend_session::{
    PartialSpendSession, SpendSession, hotpath_timer_sample, session_fill_utxo_set,
    session_to_utxo_set,
};
#[cfg(feature = "production")]
pub use spend_session::{
    SpendSessionLookup, session_lookup_matches_fill, spend_session_lookup_enabled,
};
pub use types::{
    IdCodec, OUTPUT_ID_DELETED, OutputDetail, OutputHeader, OutputId, OutputKV, OutputKey,
    outpoint_to_output_key, output_key_to_outpoint, to_output_key,
};

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Recent tip DiskIndex fan-out (updated from validation HOTPATH path).
static TIP_DISK_SEGS_HINT: AtomicU64 = AtomicU64::new(0);
static TIP_DISK_MS_HINT: AtomicU64 = AtomicU64::new(0);

/// P1: record last spend-session DiskIndex segs / disk_ms for adaptive pipeline depth.
pub fn note_tip_disk_hints(segs: u64, disk_ms: u64) {
    TIP_DISK_SEGS_HINT.store(segs, Ordering::Relaxed);
    TIP_DISK_MS_HINT.store(disk_ms, Ordering::Relaxed);
}

pub fn tip_disk_segs_hint() -> u64 {
    TIP_DISK_SEGS_HINT.load(Ordering::Relaxed)
}

pub fn tip_disk_ms_hint() -> u64 {
    TIP_DISK_MS_HINT.load(Ordering::Relaxed)
}

/// P1 lock wait accumulators (`BLVM_IBD_LOCK_TIMERS=1`).
static LOCK_SEG_WAIT_NS: AtomicU64 = AtomicU64::new(0);
static LOCK_AGE_WAIT_NS: AtomicU64 = AtomicU64::new(0);
static LOCK_TIMER_SAMPLES: AtomicU64 = AtomicU64::new(0);

fn lock_timers_enabled() -> bool {
    matches!(
        std::env::var("BLVM_IBD_LOCK_TIMERS")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

#[inline]
pub(crate) fn timed_segments_read<'a, T>(
    lock: &'a parking_lot::RwLock<T>,
) -> parking_lot::RwLockReadGuard<'a, T> {
    if !lock_timers_enabled() {
        return lock.read();
    }
    let t0 = Instant::now();
    let g = lock.read();
    LOCK_SEG_WAIT_NS.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
    maybe_log_lock_timers();
    g
}

#[inline]
pub(crate) fn timed_age_runs_read<'a, T>(
    lock: &'a parking_lot::RwLock<T>,
) -> parking_lot::RwLockReadGuard<'a, T> {
    if !lock_timers_enabled() {
        return lock.read();
    }
    let t0 = Instant::now();
    let g = lock.read();
    LOCK_AGE_WAIT_NS.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
    maybe_log_lock_timers();
    g
}

fn maybe_log_lock_timers() {
    let n = LOCK_TIMER_SAMPLES.fetch_add(1, Ordering::Relaxed) + 1;
    if n % 4096 != 0 {
        return;
    }
    let seg = LOCK_SEG_WAIT_NS.swap(0, Ordering::Relaxed);
    let age = LOCK_AGE_WAIT_NS.swap(0, Ordering::Relaxed);
    // Use default target so RUST_LOG=info (wan-bench) keeps these lines.
    tracing::info!(
        "[IBD_LOCK_TIMERS] samples={} seg_wait_ms={:.3} age_wait_ms={:.3}",
        n,
        seg as f64 / 1_000_000.0,
        age as f64 / 1_000_000.0
    );
}

/// Max block height durably present in on-disk engine segments (header scan only).
pub fn engine_segment_max_height(table_path: &std::path::Path) -> i32 {
    let mut p = table_path.as_os_str().to_owned();
    p.push(".segs");
    disk_index::DiskIndex::peek_segment_dir_max_height(&std::path::PathBuf::from(p))
}
