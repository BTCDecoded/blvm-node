//! R-348: batched, off-worker GAP_PERSIST.
//!
//! R-347 put a gauge on the inline persist (`[IBD_PARSE_LANE] persist_ms`) and it read
//! **1.07 / 1.26** of wall from 300k: every download worker was doing bincode + one LMDB
//! write txn per body under the env-wide writer lock, on a tokio runtime thread, 60 workers
//! deep. Block wire sat on ~65 MB/s in R-346 and R-347 regardless of busy (42 → 64).
//!
//! The lane moves the write to one std thread that drains a channel and stores up to
//! `BLVM_IBD_PERSIST_BATCH` bodies (default 32) or `BLVM_IBD_PERSIST_BATCH_MB` (default 16)
//! per LMDB txn. Workers only clone two `Arc`s and the wire payload handle. Disk is a
//! fallback for the coordinator (local inject / hole), so the few ms of lag are covered by
//! the in-memory `block_rx` path; `[IBD_FEEDER_MISS] store_absent` may tick up slightly.
//!
//! Off switch: `BLVM_IBD_PERSIST_LANE=0` → inline (R-347 behaviour).

use super::latch_env;
use super::local_block::{GapPersistGate, gap_persist_gate, gap_persist_write_one};
use super::types::{SharedBlock, SharedWitnesses};
use crate::storage::blockstore::BlockStore;
use blvm_protocol::{Hash, ProtocolVersion};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tracing::{info, warn};

pub(crate) struct PersistJob {
    pub height: u64,
    pub hash: Hash,
    pub block: SharedBlock,
    pub witnesses: SharedWitnesses,
    pub wire: Option<Arc<Vec<u8>>>,
    /// Wire size (batch byte cap accounting; the payload itself is only carried when
    /// `BLVM_IBD_WIRE_BYTES_STORE=1`).
    pub bytes: u64,
}

struct Lane {
    tx: SyncSender<PersistJob>,
}

static LANE: OnceLock<Option<Lane>> = OnceLock::new();
static QUEUED: AtomicUsize = AtomicUsize::new(0);
static STAT_BATCHES: AtomicU64 = AtomicU64::new(0);
static STAT_BLOCKS: AtomicU64 = AtomicU64::new(0);
static STAT_MS: AtomicU64 = AtomicU64::new(0);
static STAT_MAX_MS: AtomicU64 = AtomicU64::new(0);
static STAT_MAX_DEPTH: AtomicUsize = AtomicUsize::new(0);
static STAT_FALLBACK: AtomicU64 = AtomicU64::new(0);

pub(crate) fn enabled() -> bool {
    latch_env!(bool, {
        !matches!(
            std::env::var("BLVM_IBD_PERSIST_LANE").as_deref(),
            Ok("0") | Ok("false") | Ok("FALSE") | Ok("no") | Ok("NO")
        )
    })
}

fn batch_blocks() -> usize {
    std::env::var("BLVM_IBD_PERSIST_BATCH")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(32)
        .clamp(1, 512)
}

fn batch_bytes() -> usize {
    std::env::var("BLVM_IBD_PERSIST_BATCH_MB")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(16)
        .clamp(1, 512)
        * 1_000_000
}

/// Channel capacity (jobs). Full → the worker persists inline (never blocks the socket).
fn queue_cap() -> usize {
    std::env::var("BLVM_IBD_PERSIST_QUEUE")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(4096)
        .clamp(64, 65_536)
}

/// Idempotent. Starts the writer thread on first call when the lane is enabled.
pub(crate) fn start(
    blockstore: Arc<BlockStore>,
    validation_height: Option<Arc<AtomicU64>>,
    protocol_version: ProtocolVersion,
) {
    LANE.get_or_init(|| {
        if !enabled() {
            info!("[IBD_PERSIST_LANE] off (BLVM_IBD_PERSIST_LANE=0) — inline persist");
            return None;
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<PersistJob>(queue_cap());
        let bb = batch_blocks();
        let bbytes = batch_bytes();
        match std::thread::Builder::new()
            .name("ibd-persist-lane".into())
            .spawn(move || {
                run(
                    rx,
                    blockstore,
                    validation_height,
                    protocol_version,
                    bb,
                    bbytes,
                )
            }) {
            Ok(_) => {
                info!(
                    "[IBD_PERSIST_LANE] on batch={} batch_mb={} queue={}",
                    bb,
                    bbytes / 1_000_000,
                    queue_cap()
                );
                Some(Lane { tx })
            }
            Err(e) => {
                warn!("[IBD_PERSIST_LANE] thread spawn failed ({e}) — inline persist");
                None
            }
        }
    });
}

/// Hand a body to the lane. `false` → caller must persist inline (lane off / full / gone).
pub(crate) fn submit(job: PersistJob) -> bool {
    let Some(Some(lane)) = LANE.get() else {
        return false;
    };
    match lane.tx.try_send(job) {
        Ok(()) => {
            let d = QUEUED.fetch_add(1, Ordering::Relaxed) + 1;
            STAT_MAX_DEPTH.fetch_max(d, Ordering::Relaxed);
            true
        }
        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
            STAT_FALLBACK.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

/// Snapshot for the `[IBD_PERSIST_LANE]` gauge: (batches, blocks, ms, max_ms, max_depth, fallback, depth_now).
pub(crate) fn stats() -> (u64, u64, u64, u64, usize, u64, usize) {
    (
        STAT_BATCHES.load(Ordering::Relaxed),
        STAT_BLOCKS.load(Ordering::Relaxed),
        STAT_MS.load(Ordering::Relaxed),
        STAT_MAX_MS.load(Ordering::Relaxed),
        STAT_MAX_DEPTH.swap(0, Ordering::Relaxed),
        STAT_FALLBACK.load(Ordering::Relaxed),
        QUEUED.load(Ordering::Relaxed),
    )
}

fn run(
    rx: Receiver<PersistJob>,
    blockstore: Arc<BlockStore>,
    validation_height: Option<Arc<AtomicU64>>,
    protocol_version: ProtocolVersion,
    batch_blocks: usize,
    batch_bytes: usize,
) {
    let mut jobs: Vec<PersistJob> = Vec::with_capacity(batch_blocks);
    loop {
        // Block for the first job, then drain what is already queued up to the caps.
        let Ok(first) = rx.recv() else {
            info!("[IBD_PERSIST_LANE] channel closed — exiting");
            return;
        };
        jobs.push(first);
        let mut bytes = jobs[0].bytes as usize;
        while jobs.len() < batch_blocks && bytes < batch_bytes {
            match rx.try_recv() {
                Ok(j) => {
                    bytes += j.bytes as usize;
                    jobs.push(j);
                }
                Err(_) => break,
            }
        }
        QUEUED.fetch_sub(jobs.len(), Ordering::Relaxed);
        let t0 = Instant::now();
        persist_batch(
            &jobs,
            &blockstore,
            validation_height.as_ref(),
            protocol_version,
        );
        let ms = t0.elapsed().as_millis() as u64;
        STAT_BATCHES.fetch_add(1, Ordering::Relaxed);
        STAT_BLOCKS.fetch_add(jobs.len() as u64, Ordering::Relaxed);
        STAT_MS.fetch_add(ms, Ordering::Relaxed);
        STAT_MAX_MS.fetch_max(ms, Ordering::Relaxed);
        super::ms_breakdown::note_gap_persist_batch(jobs.len() as u64, ms);
        jobs.clear();
    }
}

fn persist_batch(
    jobs: &[PersistJob],
    blockstore: &BlockStore,
    validation_height: Option<&Arc<AtomicU64>>,
    protocol_version: ProtocolVersion,
) {
    let mut to_write: Vec<&PersistJob> = Vec::with_capacity(jobs.len());
    for j in jobs {
        match gap_persist_gate(
            blockstore,
            validation_height,
            j.height,
            j.hash,
            j.block.as_ref(),
            j.witnesses.as_ref(),
            protocol_version,
        ) {
            Ok(GapPersistGate::Write) => to_write.push(j),
            Ok(_) => {}
            Err(e) => warn!("[IBD_GAP_PERSIST] height {}: gate failed: {e}", j.height),
        }
    }
    if to_write.is_empty() {
        return;
    }
    if super::local_block::wire_bytes_store_enabled() {
        // Wire-blob path is per-block (its own txn each); keep the tested code.
        for j in &to_write {
            if let Err(e) = gap_persist_write_one(
                blockstore,
                j.height,
                j.hash,
                j.block.as_ref(),
                j.witnesses.as_ref(),
                j.wire.as_deref().map(|v| v.as_slice()),
            ) {
                warn!("[IBD_GAP_PERSIST] height {}: persist failed: {e}", j.height);
            }
        }
        return;
    }
    let items: Vec<(
        &blvm_protocol::Block,
        &[Vec<blvm_protocol::segwit::Witness>],
        u64,
    )> = to_write
        .iter()
        .map(|j| (j.block.as_ref(), j.witnesses.as_ref().as_slice(), j.height))
        .collect();
    if let Err(e) = blockstore.store_blocks_with_witness_batch(&items) {
        warn!(
            "[IBD_GAP_PERSIST] batch of {} ({}..{}) failed: {e} — retrying one by one",
            items.len(),
            items.first().map(|i| i.2).unwrap_or(0),
            items.last().map(|i| i.2).unwrap_or(0)
        );
        for j in &to_write {
            if let Err(e) = gap_persist_write_one(
                blockstore,
                j.height,
                j.hash,
                j.block.as_ref(),
                j.witnesses.as_ref(),
                None,
            ) {
                warn!("[IBD_GAP_PERSIST] height {}: persist failed: {e}", j.height);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r348_batch_caps_clamp() {
        assert!((1..=512).contains(&batch_blocks()));
        assert!(batch_bytes() >= 1_000_000);
        assert!(queue_cap() >= 64);
    }
}
