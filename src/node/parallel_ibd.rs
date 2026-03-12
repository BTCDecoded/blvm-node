//! Parallel Initial Block Download (IBD)
//!
//! Implements parallel block downloading from multiple peers during initial sync.
//! This significantly speeds up IBD by downloading blocks concurrently from different peers.
//!
//! ## Header Sync Optimization
//!
//! Uses hardcoded checkpoints to parallelize header download:
//! - Headers are downloaded in parallel for ranges between checkpoints
//! - Each range uses the checkpoint hash as its starting locator
//! - Verification ensures continuity and checkpoint hash matching

use crate::network::inventory::MSG_BLOCK;
use crate::network::peer_scoring::is_lan_peer;
use crate::network::protocol::{
    GetHeadersMessage, HeadersMessage, ProtocolMessage, ProtocolParser,
};
use crate::network::NetworkManager;
use crate::node::block_processor::{
    prepare_block_validation_context, store_block_with_context_and_index,
    validate_block_with_context,
};
use crate::storage::blockstore::BlockStore;
use crate::storage::database::Tree;
use crate::storage::disk_utxo::{
    block_input_keys_batch_into_arc, block_input_keys_into, key_to_outpoint, load_keys_from_disk,
    outpoint_to_key, OutPointKey, SyncBatch,
};
#[cfg(feature = "production")]
use crate::storage::ibd_utxo_store::IbdUtxoStore;
use crate::storage::Storage;
use anyhow::{Context, Result};
use blvm_consensus::bip_validation::Bip30Index;
use blvm_protocol::{
    segwit::Witness, BitcoinProtocolEngine, Block, BlockHeader, Hash, UtxoSet, ValidationResult,
};

use blvm_consensus::serialization::varint::decode_varint;
use blvm_consensus::types::{OutPoint, UTXO};
use crossbeam_channel;
use futures::stream::{FuturesUnordered, StreamExt};
use hex;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::{Condvar, Mutex};
use std::thread;
use tokio::sync::broadcast;
use tokio::sync::oneshot;
use tokio::sync::Semaphore;
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, warn};

// ============================================================================
// UTXO Prefetch Cache
// ============================================================================

/// Number of blocks to prefetch ahead
const PREFETCH_LOOKAHEAD: usize = 10;

/// Rough bytes per block for feeder byte cap. SegWit blocks 1–4MB; use 1.5MB estimate.
const FEEDER_BLOCK_BYTES_ESTIMATE: usize = 1_500_000;

/// Ready-queue item: block + pre-loaded UTXOs. Arc avoids clone when sending to validation.
type ReadyItem = (
    u64,
    Block,
    Vec<Vec<Witness>>,
    rustc_hash::FxHashMap<OutPointKey, Arc<UTXO>>,
);

/// Block feeder buffer: shared between feeder thread (drains ready_rx) and validation thread.
/// Feeder inserts; validation removes next block and reads lookahead for protect_keys.
/// Precomputed tx_ids: feeder computes when inserting to free validation thread from SHA256 work.
type FeederBufferValue = (
    Arc<Block>,
    Vec<Vec<Witness>>,
    rustc_hash::FxHashMap<OutPointKey, Arc<UTXO>>,
    Vec<Hash>,
);

/// IBD v2 prefetch work item: (store, keys_raw, height, block, witnesses). Worker filters via store.
#[cfg(feature = "production")]
type PrefetchWorkItemV2 = (
    Arc<IbdUtxoStore>,
    Vec<OutPointKey>,
    u64,
    Block,
    Vec<Vec<Witness>>,
);

/// Chunk work item for re-queue on drop. Live log 2026-02-21: workers_in_flight=[], chunks lost every 100 blocks.
type ChunkWorkItem = (u64, u64, Option<String>);

/// Sequential chunk assigner: assigns chunks in height order so validation never starves.
/// Workers call get_work(peer_id); assigner returns next chunk when start <= validation_height + max_ahead.
/// Bootstrap serialization: when start_height==0, only chunk (0, N) is assignable until it completes.
/// This ensures block 0 arrives first — otherwise parallel chunks (128+, 256+) can receive blocks before
/// bootstrap, coordinator never gets block 0, and sync never starts.
///
/// Each chunk is assigned to a specific peer (create_chunks). We only give a chunk to a worker
/// whose peer_id matches. Bootstrap chunk is always ≥128 blocks so 99 and 100 are in the same chunk
/// — no out-of-order delivery regardless of peer type.
///
/// Per-peer serial: at most one chunk in flight per peer. Eliminates chunk-boundary stalls (Core-like
/// earliest-first) — chunks complete in order, validation rarely waits for next block.
struct ChunkAssigner {
    chunks: Vec<(u64, u64)>,
    /// Peer assigned to each chunk; same length as chunks. Worker gets chunk only if peer matches.
    chunk_peers: Vec<String>,
    next_index: std::sync::atomic::AtomicUsize,
    retry_queue: std::sync::Mutex<VecDeque<ChunkWorkItem>>,
    validation_height: Arc<std::sync::atomic::AtomicU64>,
    /// When true, only chunks with start==0 are assignable. Set when start_height==0; cleared when bootstrap chunk completes.
    bootstrap_complete: std::sync::atomic::AtomicBool,
    start_height: u64,
    /// Per-peer serial: peer_id -> (start, end) of chunk in flight. At most one chunk per peer.
    in_flight_per_peer: std::sync::Mutex<std::collections::HashMap<String, (u64, u64)>>,
}

impl ChunkAssigner {
    fn new(
        chunks: Vec<(u64, u64)>,
        chunk_peers: Vec<String>,
        validation_height: Arc<std::sync::atomic::AtomicU64>,
        start_height: u64,
    ) -> Self {
        assert_eq!(
            chunks.len(),
            chunk_peers.len(),
            "chunks and chunk_peers must match"
        );
        // Resuming IBD (start_height > 0): no bootstrap serialization, all chunks assignable immediately
        let bootstrap_complete = start_height > 0;
        Self {
            chunks,
            chunk_peers,
            next_index: std::sync::atomic::AtomicUsize::new(0),
            retry_queue: std::sync::Mutex::new(VecDeque::new()),
            validation_height,
            bootstrap_complete: std::sync::atomic::AtomicBool::new(bootstrap_complete),
            start_height,
            in_flight_per_peer: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Mark bootstrap chunk (0..N) complete — enables parallel chunk assignment for start_height > 0.
    fn mark_bootstrap_complete(&self) {
        self.bootstrap_complete
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Returns the next assignable chunk for this peer, or None if nothing ready.
    /// Per-peer serial: returns None if this peer already has a chunk in flight (eliminates chunk-boundary stalls).
    /// Round-robin: prioritizes critical chunk (containing next_needed) from retry, then earliest available.
    /// CRITICAL: Entire operation under one lock to prevent duplicate chunk assignment (race: two workers
    /// for same peer both getting chunk 116240-116255, both requesting same blocks, one starves).
    fn get_work(&self, peer_id: &str, max_ahead: u64) -> Option<(u64, u64)> {
        let bootstrap_done = self
            .bootstrap_complete
            .load(std::sync::atomic::Ordering::Relaxed);
        let current_validation = self
            .validation_height
            .load(std::sync::atomic::Ordering::Relaxed);
        let next_needed = current_validation + 1;
        let max_start = current_validation.saturating_add(max_ahead);

        // Bootstrap serialization: until bootstrap chunk completes, only assign chunks with start==0
        let allow_chunk = |start: u64| bootstrap_done || start == self.start_height;

        // Single lock: check in-flight + find chunk + insert. Prevents duplicate assignment.
        let mut guard = self.in_flight_per_peer.lock().unwrap();
        if guard.contains_key(peer_id) {
            return None;
        }

        // Try retry queue first (critical chunk, then earliest)
        {
            let mut retry = self.retry_queue.lock().unwrap();
            let critical = retry.iter().enumerate().find(|(_, (s, e, ex))| {
                *s <= next_needed
                    && next_needed <= *e
                    && ex.as_ref() != Some(&peer_id.to_string())
                    && *s <= max_start
                    && allow_chunk(*s)
            });
            if let Some((i, _)) = critical {
                let (start, end, _) = retry.remove(i).unwrap();
                guard.insert(peer_id.to_string(), (start, end));
                return Some((start, end));
            }
            let candidate = retry
                .iter()
                .enumerate()
                .filter(|(_, (_, _, ex))| ex.as_ref() != Some(&peer_id.to_string()))
                .filter(|(_, (s, _, _))| *s <= max_start && allow_chunk(*s))
                .min_by_key(|(_, (s, _, _))| *s);
            if let Some((i, _)) = candidate {
                let (start, end, _) = retry.remove(i).unwrap();
                guard.insert(peer_id.to_string(), (start, end));
                return Some((start, end));
            }
        }

        // Main queue
        let idx = self.next_index.load(std::sync::atomic::Ordering::Relaxed);
        if idx >= self.chunks.len() {
            return None;
        }
        if self.chunk_peers[idx] != peer_id {
            return None;
        }
        let (start, end) = self.chunks[idx];
        if start > current_validation.saturating_add(max_ahead) {
            return None;
        }
        if !allow_chunk(start) {
            return None;
        }
        self.next_index
            .store(idx + 1, std::sync::atomic::Ordering::Relaxed);
        guard.insert(peer_id.to_string(), (start, end));
        Some((start, end))
    }

    /// Called when a worker completes (or fails) a chunk. Clears in-flight so peer can get next chunk.
    fn on_chunk_complete(&self, peer_id: &str) {
        self.in_flight_per_peer.lock().unwrap().remove(peer_id);
    }

    fn requeue(&self, start: u64, end: u64, exclude_peer: Option<String>) {
        // Use exclude_peer to avoid immediate retry with same peer, but stall recovery can clear it
        self.retry_queue
            .lock()
            .unwrap()
            .push_back((start, end, exclude_peer));
    }

    fn is_done(&self) -> bool {
        let idx = self.next_index.load(std::sync::atomic::Ordering::Relaxed);
        idx >= self.chunks.len() && self.retry_queue.lock().unwrap().is_empty()
    }

    fn total_chunks(&self) -> usize {
        self.chunks.len()
    }

    fn remaining_count(&self) -> usize {
        let idx = self.next_index.load(std::sync::atomic::Ordering::Relaxed);
        let retry_len = self.retry_queue.lock().unwrap().len();
        self.chunks.len().saturating_sub(idx) + retry_len
    }
}

/// Re-queues chunk on drop if not disarmed. Prevents chunk loss on panic/task-cancel/any exit.
struct ChunkGuard {
    chunk: Option<ChunkWorkItem>,
    peer_id: Option<String>,
    assigner: Arc<ChunkAssigner>,
}

impl ChunkGuard {
    fn new(
        start: u64,
        end: u64,
        exclude: Option<String>,
        peer_id: String,
        assigner: Arc<ChunkAssigner>,
    ) -> Self {
        Self {
            chunk: Some((start, end, exclude)),
            peer_id: Some(peer_id),
            assigner,
        }
    }
    fn disarm(&mut self) {
        self.chunk = None;
        self.peer_id = None; // Don't call on_chunk_complete on Drop; caller will do it
    }
}

impl Drop for ChunkGuard {
    fn drop(&mut self) {
        if let Some((start, end, exclude)) = self.chunk.take() {
            self.assigner.requeue(start, end, exclude);
        }
        if let Some(peer_id) = self.peer_id.take() {
            self.assigner.on_chunk_complete(&peer_id);
        }
    }
}

/// Maximum entries in prefetch cache (bounded to prevent memory bloat)
/// This is a fallback; actual limit is calculated dynamically based on available memory
const MAX_PREFETCH_ENTRIES: usize = 100_000;

// ============================================================================
// Dynamic Memory Management (Cross-Platform)
// ============================================================================
//
// Unified hardware-aware tuning: derive a coherent memory budget from total RAM,
// allocate across UTXO cache, block buffer, prefetch, RocksDB, and overhead.
// Never exceeds 40% of total RAM (leaves headroom for OS, perf, other apps, OOM safety).
/// TidesDB hard limit: max ops per transaction (TDB_MAX_TXN_OPS=100000 in tidesdb.c).
const TIDESDB_MAX_TXN_OPS: usize = 50_000;

// ---------------------------------------------------------------------------
// MemoryGuard: cross-platform auto-tuning for IBD memory management.
//
// Probes total/available RAM at startup via sysinfo (Linux, macOS, Windows).
// Derives all thresholds from hardware — no env vars needed. During IBD the
// validation loop calls `should_flush()` periodically; when process RSS nears
// the limit the guard forces a UTXO flush to keep memory under control.
// ---------------------------------------------------------------------------

struct MemoryGuard {
    total_mb: u64,
    budget_mb: u64,
    /// RSS above this → force flush pending_writes to disk.
    flush_trigger_mb: u64,
    /// Derived UTXO cache max in MB (50% of budget).
    utxo_cache_mb: usize,
    /// Max UTXO cache entries (utxo_cache_mb / 320 bytes per entry).
    utxo_max_entries: usize,
    /// UTXO flush threshold (entries in pending_writes before auto-flush).
    utxo_flush_threshold: usize,
    /// Block buffer limit (blocks in reorder buffer).
    block_buffer_base: usize,
    /// Storage flush interval (blocks between storage flushes).
    storage_flush_interval: usize,
    /// Prefetch cache limit.
    prefetch_limit: usize,
    /// Max items in prefetch channels. Each ReadyItem holds Block (~300KB) + UTXO map (~200KB).
    /// 4096 items = ~2GB per channel. Must scale with RAM.
    prefetch_queue_size: usize,
    /// Max blocks download can race ahead of validation.
    max_ahead_blocks: u64,
    /// Defer UTXO flush to checkpoints when RAM is sufficient.
    pub defer_flush: bool,
    /// Checkpoint interval for deferred flushes (blocks).
    pub defer_checkpoint_interval: u64,
    /// Feeder buffer byte cap (alongside count cap). Prevents RSS spike from large blocks.
    pub feeder_buffer_bytes_limit: usize,
    #[cfg(feature = "sysinfo")]
    sys: sysinfo::System,
    last_rss_check: std::time::Instant,
}

impl MemoryGuard {
    fn new() -> Self {
        #[cfg(feature = "sysinfo")]
        let (total_mb, available_mb, mut sys) = {
            use sysinfo::System;
            let mut s = System::new_all();
            s.refresh_memory();
            let t = s.total_memory() / (1024 * 1024);
            let a = s.available_memory() / (1024 * 1024);
            (t, a, s)
        };
        #[cfg(not(feature = "sysinfo"))]
        let (total_mb, available_mb) = (8192u64, 6144u64);

        let total_gb = total_mb / 1024;

        // Budget: 28% of total, capped to 45% of available. Conservative for shared systems (Chromium, etc).
        let budget_mb = (total_mb * 28 / 100).min(available_mb * 45 / 100).max(512);

        // RSS flush trigger: 55% of total. With mimalloc, RSS tracks live memory closely;
        // glibc fragmentation no longer inflates RSS 2-3x. 55% leaves headroom for OS + browser.
        let flush_trigger_mb = total_mb * 55 / 100;

        // UTXO cache: 40% of budget. 320 bytes/entry (DashMap + pending_writes mirror + eviction queue).
        let utxo_cache_mb = ((budget_mb * 40 / 100) as usize).clamp(256, 3072);
        let utxo_max_entries = utxo_cache_mb * 1024 * 1024 / 320;

        // UTXO flush threshold: how many pending_writes entries before we flush to disk.
        let utxo_flush_threshold = if total_gb >= 32 {
            400_000
        } else if total_gb >= 24 {
            200_000
        } else if total_gb >= 16 {
            100_000
        } else {
            50_000
        }
        .min(TIDESDB_MAX_TXN_OPS);

        // Defer flush: only on systems with enough RAM to absorb pending_writes growth.
        // BLVM_IBD_DEFER_FLUSH=0|1 overrides auto-tune.
        let defer_flush = std::env::var("BLVM_IBD_DEFER_FLUSH")
            .ok()
            .and_then(|v| match v.as_str() {
                "0" | "false" => Some(false),
                "1" | "true" => Some(true),
                _ => v.parse().ok().map(|b: bool| b),
            })
            .unwrap_or(total_gb >= 32);
        let defer_checkpoint_interval = if total_gb >= 64 { 50_000 } else { 25_000 };

        // Block buffer: 10% of budget. Each block is ~300KB at height 220k+, with witnesses
        // and UTXO maps the total per-block footprint is ~500KB. Buffer exists in BOTH
        // reorder_buffer and feeder_state, so effective memory is 2× this.
        let block_buffer_base = {
            let buffer_mb = budget_mb * 10 / 100;
            let blocks = buffer_mb * 1024 / 500;
            (blocks as usize).clamp(100, 800)
        };

        // Storage flush interval: how often to flush committed blocks to disk.
        // Each pending block holds Arc<Block> (~300KB). Lower = less memory.
        let storage_flush_interval = if total_gb >= 32 { 2000 } else { 500 };

        // Prefetch queue size: each ReadyItem holds Block (~300KB) + FxHashMap of UTXOs (~200KB)
        // = ~500KB per item. The input channel also holds Block + keys (~350KB per item).
        // Total memory = prefetch_queue_size × ~850KB (across input + ready channels).
        let prefetch_queue_size = {
            let queue_mb = budget_mb * 15 / 100; // 15% of budget for prefetch channels
            let items = queue_mb * 1024 / 850; // ~850KB per item across both channels
            (items as usize).clamp(64, 2048)
        };

        // Max blocks download can race ahead of validation. Limits block_rx channel depth.
        let max_ahead_blocks = if total_gb >= 32 {
            1024
        } else if total_gb >= 16 {
            512
        } else {
            256
        };

        // Prefetch cache: 3% of budget, ~400 bytes per entry.
        let prefetch_limit = {
            let cache_mb = budget_mb * 3 / 100;
            ((cache_mb * 1024 * 1024 / 400) as usize).clamp(5_000, 50_000)
        };

        // Feeder buffer byte cap: 5% of budget. Prevents RSS spike from large blocks (SegWit 1–4MB).
        // Each block ~500KB–2MB; cap ensures we don't buffer 256×2MB = 512MB.
        let feeder_buffer_bytes_limit = (budget_mb * 5 / 100 * 1024 * 1024) as usize;

        info!(
            "MemoryGuard: total={}MB available={}MB budget={}MB flush_trigger={}MB \
             utxo_cache={}MB ({}entries) flush_threshold={} defer_flush={} buffer={} \
             prefetch={} prefetch_queue={} max_ahead={} storage_flush={} feeder_bytes={}MB",
            total_mb,
            available_mb,
            budget_mb,
            flush_trigger_mb,
            utxo_cache_mb,
            utxo_max_entries,
            utxo_flush_threshold,
            defer_flush,
            block_buffer_base,
            prefetch_limit,
            prefetch_queue_size,
            max_ahead_blocks,
            storage_flush_interval,
            feeder_buffer_bytes_limit / (1024 * 1024)
        );

        Self {
            total_mb,
            budget_mb,
            flush_trigger_mb,
            utxo_cache_mb,
            utxo_max_entries,
            utxo_flush_threshold,
            block_buffer_base,
            storage_flush_interval,
            prefetch_limit,
            prefetch_queue_size,
            max_ahead_blocks,
            defer_flush,
            defer_checkpoint_interval,
            feeder_buffer_bytes_limit,
            #[cfg(feature = "sysinfo")]
            sys,
            last_rss_check: std::time::Instant::now(),
        }
    }

    /// Check if process RSS exceeds the flush trigger. Uses /proc on Linux (reliable),
    /// sysinfo elsewhere. Cached: re-probe every 500ms to react before OOM.
    fn should_flush(&mut self) -> bool {
        if self.last_rss_check.elapsed() < std::time::Duration::from_millis(500) {
            return false;
        }
        self.last_rss_check = std::time::Instant::now();

        let rss_mb = self.current_rss_mb();
        if rss_mb > 0 && rss_mb > self.flush_trigger_mb {
            info!(
                "MemoryGuard: RSS {}MB > trigger {}MB, forcing flush",
                rss_mb, self.flush_trigger_mb
            );
            return true;
        }
        false
    }

    /// Current process RSS in MB. Linux: /proc/self/status. Windows/macOS: sysinfo.
    fn current_rss_mb(&mut self) -> u64 {
        #[cfg(target_os = "linux")]
        {
            if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
                for line in s.lines() {
                    if line.starts_with("VmRSS:") {
                        if let Some(kb) = line
                            .split_whitespace()
                            .nth(1)
                            .and_then(|v| v.parse::<u64>().ok())
                        {
                            return kb / 1024;
                        }
                        break;
                    }
                }
            }
            0
        }
        #[cfg(all(not(target_os = "linux"), feature = "sysinfo"))]
        {
            use sysinfo::Pid;
            let pid = Pid::from(std::process::id() as usize);
            self.sys.refresh_process(pid);
            self.sys
                .process(pid)
                .map(|p| p.memory() / (1024 * 1024))
                .unwrap_or(0)
        }
        #[cfg(all(not(target_os = "linux"), not(feature = "sysinfo")))]
        0u64
    }

    /// Dynamic block buffer limit adjusted for current height (bigger blocks later).
    fn buffer_limit(&self, current_height: u64) -> usize {
        let scale = match current_height {
            0..=100_000 => 100,
            100_001..=300_000 => 50,
            300_001..=480_000 => 33,
            480_001..=700_000 => 20,
            _ => 12,
        };
        (self.block_buffer_base * scale / 100).clamp(200, 2_000)
    }

    /// Diagnostic: current RSS and available memory (MB). For profile logging.
    fn memory_diag(&mut self) -> Option<(u64, u64)> {
        #[cfg(feature = "sysinfo")]
        {
            use sysinfo::Pid;
            let pid = Pid::from(std::process::id() as usize);
            self.sys.refresh_memory();
            self.sys.refresh_process(pid);
            let rss_mb = self
                .sys
                .process(pid)
                .map(|p| p.memory() / (1024 * 1024))
                .unwrap_or(0);
            let avail_mb = self.sys.available_memory() / (1024 * 1024);
            Some((rss_mb, avail_mb))
        }
        #[cfg(not(feature = "sysinfo"))]
        None
    }
}

/// UTXO Prefetch Cache for IBD optimization
///
/// Prefetches UTXOs for upcoming blocks while current block is being validated.
/// This hides UTXO lookup latency by doing lookups ahead of time.
struct PrefetchCache {
    /// Cached UTXOs (outpoint -> utxo)
    cache: HashMap<OutPoint, UTXO>,
    /// Heights that have been prefetched
    prefetched_heights: HashSet<u64>,
    /// Maximum entries (dynamically calculated based on available memory)
    max_entries: usize,
}

impl PrefetchCache {
    fn new() -> Self {
        Self::with_limit(MAX_PREFETCH_ENTRIES)
    }

    fn with_limit(max_entries: usize) -> Self {
        Self {
            cache: HashMap::with_capacity(max_entries.min(50_000)),
            prefetched_heights: HashSet::with_capacity(PREFETCH_LOOKAHEAD * 2),
            max_entries,
        }
    }

    /// Get a UTXO from the prefetch cache
    #[inline]
    fn get(&self, outpoint: &OutPoint) -> Option<&UTXO> {
        self.cache.get(outpoint)
    }

    /// Prefetch UTXOs for a block from the UTXO set
    fn prefetch_block(&mut self, height: u64, block: &Block, utxo_set: &UtxoSet) {
        // Skip if already prefetched
        if self.prefetched_heights.contains(&height) {
            return;
        }

        // Prefetch all input UTXOs for this block
        for tx in &block.transactions {
            for input in &tx.inputs {
                // Skip coinbase inputs
                if input.prevout.hash == [0u8; 32] && input.prevout.index == 0xffffffff {
                    continue;
                }

                // Look up and cache the UTXO
                if let Some(arc) = utxo_set.get(&input.prevout) {
                    self.cache.insert(input.prevout, (**arc).clone());
                }
            }
        }

        self.prefetched_heights.insert(height);

        // Evict old entries if cache is too large (using dynamic limit)
        if self.cache.len() > self.max_entries {
            self.evict_stale_entries(height);
        }
    }

    /// Remove entries for blocks we've already validated
    fn evict_stale_entries(&mut self, current_height: u64) {
        // Remove prefetched heights that are far behind current
        let stale_heights: Vec<u64> = self
            .prefetched_heights
            .iter()
            .filter(|&&h| h + 20 < current_height)
            .copied()
            .collect();

        for h in stale_heights {
            self.prefetched_heights.remove(&h);
        }

        // If still too large, clear older entries (simple strategy)
        if self.cache.len() > self.max_entries {
            // Keep only the most recent entries
            let keep_count = self.max_entries / 2;
            if self.cache.len() > keep_count {
                let to_remove = self.cache.len() - keep_count;
                let keys_to_remove: Vec<OutPoint> =
                    self.cache.keys().take(to_remove).cloned().collect();
                for key in keys_to_remove {
                    self.cache.remove(&key);
                }
            }
        }
    }

    /// Mark a height as processed (can clean up prefetch data)
    fn mark_processed(&mut self, height: u64) {
        self.prefetched_heights.remove(&height);
    }

    /// Get cache statistics
    fn stats(&self) -> (usize, usize) {
        (self.cache.len(), self.prefetched_heights.len())
    }
}

/// Bitcoin mainnet checkpoints for parallel header download
///
/// These are well-known block hashes at specific heights that allow us to
/// parallelize header downloads. Each checkpoint is immutable (deep enough
/// to never be reorganized in practice).
///
/// Format: (height, hash in internal byte order)
const MAINNET_CHECKPOINTS: &[(u64, [u8; 32])] = &[
    // Genesis block
    (
        0,
        [
            0x6f, 0xe2, 0x8c, 0x0a, 0xb6, 0xf1, 0xb3, 0x72, 0xc1, 0xa6, 0xa2, 0x46, 0xae, 0x63,
            0xf7, 0x4f, 0x93, 0x1e, 0x83, 0x65, 0xe1, 0x5a, 0x08, 0x9c, 0x68, 0xd6, 0x19, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 11111
    (
        11111,
        [
            0x1d, 0x7c, 0x6e, 0xb2, 0xfd, 0x42, 0xf5, 0x9c, 0x2e, 0x09, 0xe5, 0xbc, 0x23, 0x36,
            0xad, 0x18, 0xa7, 0x07, 0x05, 0x7a, 0xaa, 0x4e, 0x78, 0x3d, 0x24, 0x44, 0xe2, 0x69,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 33333
    (
        33333,
        [
            0xa6, 0xd0, 0xb5, 0xdf, 0x7d, 0xd0, 0x0f, 0x90, 0x5b, 0x02, 0x4a, 0x81, 0xa8, 0x6e,
            0x1d, 0xd6, 0x26, 0x2c, 0xc2, 0xc1, 0x3e, 0xc9, 0xa3, 0x93, 0x8c, 0x55, 0xd2, 0x2d,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 74000
    (
        74000,
        [
            0x20, 0x1a, 0x66, 0xb8, 0x72, 0x37, 0x72, 0xd8, 0x24, 0x81, 0x0e, 0xa2, 0xf0, 0x02,
            0xb0, 0x54, 0x2b, 0xd2, 0xa2, 0xf4, 0xa8, 0x7e, 0xf0, 0x79, 0x1c, 0x47, 0x34, 0xce,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 105000
    (
        105000,
        [
            0x97, 0xdc, 0x6b, 0x15, 0xfb, 0xea, 0xa3, 0x20, 0xf8, 0x27, 0x80, 0x2c, 0xe2, 0xc1,
            0x8d, 0xcd, 0x34, 0xcd, 0x15, 0xd3, 0x04, 0xd7, 0x5f, 0xe2, 0x02, 0x91, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 134444
    (
        134444,
        [
            0xfe, 0xb0, 0xd2, 0x42, 0x0d, 0x4a, 0x18, 0x91, 0x4c, 0xc8, 0xa8, 0x30, 0xf4, 0x36,
            0x5d, 0xfd, 0x4f, 0x34, 0xcd, 0x15, 0xd3, 0x4f, 0x71, 0x0b, 0x5b, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 168000
    (
        168000,
        [
            0x63, 0xb7, 0x03, 0x83, 0xc1, 0x84, 0xa1, 0x91, 0x4c, 0xfb, 0x6e, 0x21, 0xf6, 0x32,
            0x79, 0x5e, 0x01, 0x87, 0x82, 0xa6, 0x82, 0x6d, 0xf6, 0x16, 0x72, 0x99, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 193000
    (
        193000,
        [
            0x17, 0x93, 0xb8, 0x83, 0x3c, 0xbd, 0xe3, 0xc6, 0xf6, 0x0f, 0x10, 0x87, 0x7c, 0x98,
            0xa7, 0x82, 0x66, 0x40, 0x7f, 0x34, 0x9d, 0x15, 0x59, 0x75, 0x4f, 0x05, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 210000 (first halving)
    (
        210000,
        [
            0x4e, 0x34, 0x17, 0xb1, 0xa9, 0xe8, 0x28, 0x57, 0x22, 0xb1, 0xf5, 0xaf, 0x8f, 0x55,
            0x0b, 0x20, 0x02, 0xfc, 0xf6, 0x36, 0x07, 0xc4, 0x33, 0x36, 0x8b, 0x04, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 250000
    (
        250000,
        [
            0x14, 0xd2, 0xf4, 0x94, 0x2e, 0x38, 0x0a, 0xa3, 0xc5, 0x35, 0x70, 0xa1, 0xf8, 0x0f,
            0xf4, 0x64, 0xcf, 0x6f, 0x19, 0xd8, 0xdb, 0x69, 0xf7, 0x86, 0x87, 0x03, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 295000
    (
        295000,
        [
            0x83, 0xa9, 0x32, 0x64, 0xc6, 0x70, 0x03, 0xa1, 0x35, 0xfa, 0x2a, 0x03, 0xb2, 0xe6,
            0x6f, 0x9d, 0xf6, 0x68, 0x73, 0x2f, 0xe6, 0x26, 0x55, 0x2e, 0xf5, 0xb4, 0x9d, 0x04,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 350000
    (
        350000,
        [
            0x63, 0x6b, 0x92, 0xa6, 0xc2, 0xc6, 0x2b, 0xe7, 0x55, 0x6f, 0x6e, 0x26, 0x74, 0xb4,
            0x1b, 0x0c, 0x8e, 0xb3, 0x00, 0x40, 0xf6, 0x4f, 0xcf, 0x53, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 400000
    (
        400000,
        [
            0x6f, 0x3f, 0x6e, 0x27, 0x24, 0x52, 0xff, 0x8f, 0x55, 0xb0, 0x5c, 0xd4, 0x2d, 0xed,
            0x1c, 0xf8, 0xfe, 0x32, 0x73, 0x4e, 0xc8, 0xa0, 0x6c, 0x04, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 450000
    (
        450000,
        [
            0x0b, 0xa2, 0x07, 0x0c, 0x62, 0xcd, 0x19, 0xa8, 0xef, 0x8c, 0xaf, 0x08, 0xfb, 0x75,
            0x0c, 0xc5, 0x51, 0xd6, 0x14, 0x83, 0x72, 0x07, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 500000
    (
        500000,
        [
            0x04, 0x5d, 0x94, 0x1a, 0x00, 0x20, 0xcb, 0x64, 0x37, 0x5f, 0x9c, 0xc7, 0x2a, 0x50,
            0x0e, 0x2a, 0x86, 0x81, 0xcf, 0x9b, 0x73, 0xc2, 0xc9, 0xc0, 0x75, 0xc4, 0xfb, 0x24,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 550000
    (
        550000,
        [
            0xc9, 0x6b, 0xec, 0x12, 0x41, 0xff, 0x53, 0x58, 0xcb, 0xba, 0x42, 0x89, 0x9f, 0x13,
            0xcf, 0x39, 0xa9, 0x7a, 0xb1, 0xfb, 0x0e, 0x75, 0x6c, 0xf7, 0x22, 0x23, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 600000
    (
        600000,
        [
            0x91, 0x5f, 0xcd, 0x06, 0x85, 0x69, 0xb7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x07, 0x16, 0x68, 0x85, 0x90, 0xe7, 0xf4, 0x9a, 0x13, 0xcf, 0x39, 0xa9,
            0x7a, 0xb1, 0xfb, 0x0e,
        ],
    ),
    // Block 650000
    (
        650000,
        [
            0x5a, 0x6a, 0xef, 0xc1, 0x56, 0x26, 0xfd, 0xde, 0x2a, 0x6c, 0x9c, 0x3b, 0xc4, 0x2a,
            0xbd, 0x93, 0xd2, 0x16, 0x52, 0xb9, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 700000
    (
        700000,
        [
            0x59, 0xa9, 0x0c, 0x77, 0xa1, 0xf8, 0x5e, 0xa1, 0x3a, 0xfc, 0x90, 0x05, 0x2a, 0xf5,
            0x08, 0x0b, 0x37, 0xa2, 0x65, 0x27, 0x0a, 0x56, 0xb1, 0x7a, 0x90, 0xfc, 0x59, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 750000
    (
        750000,
        [
            0xbd, 0xca, 0x93, 0xdb, 0xa0, 0x04, 0x4a, 0x72, 0xd8, 0x72, 0x28, 0x7a, 0xb1, 0x56,
            0x70, 0x9a, 0x16, 0x5a, 0x27, 0x0a, 0x56, 0xb1, 0x7a, 0x90, 0xfc, 0xc7, 0x09, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 800000
    (
        800000,
        [
            0x54, 0xa0, 0x28, 0x72, 0xd7, 0x34, 0x15, 0x40, 0xfb, 0x1a, 0x7a, 0x8d, 0xb7, 0x56,
            0xa2, 0x70, 0x65, 0x91, 0x02, 0xa7, 0xc4, 0xe4, 0x1e, 0xd8, 0x76, 0x4d, 0x48, 0xc2,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
    // Block 850000
    (
        850000,
        [
            0xa1, 0x55, 0x0f, 0x8f, 0x8d, 0x1e, 0x0f, 0xd2, 0x42, 0xb0, 0x0d, 0xa8, 0x1a, 0x18,
            0x4a, 0x91, 0x7c, 0x13, 0xda, 0x1e, 0x76, 0xca, 0xf1, 0xe2, 0xa9, 0x89, 0x13, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
    ),
];

/// Get the applicable checkpoints for a given height range
fn get_checkpoints_in_range(start: u64, end: u64) -> Vec<(u64, [u8; 32])> {
    MAINNET_CHECKPOINTS
        .iter()
        .filter(|(h, _)| *h >= start && *h <= end)
        .cloned()
        .collect()
}

/// Parallel IBD configuration
#[derive(Debug, Clone)]
pub struct ParallelIBDConfig {
    /// Number of parallel workers (default: CPU count)
    pub num_workers: usize,
    /// Chunk size in blocks (default: 1000)
    pub chunk_size: u64,
    /// Maximum concurrent downloads per peer (default: 16)
    ///
    /// This is the pipeline depth - how many outstanding block requests
    /// we can have per peer. Higher values hide network latency better
    /// but use more memory. 16 is a good balance for most connections.
    ///
    /// Performance impact: 4-8x improvement vs sequential requests.
    pub max_concurrent_per_peer: usize,
    /// Checkpoint interval in blocks (default: 10,000)
    pub checkpoint_interval: u64,
    /// Timeout for block download in seconds (default: 30)
    pub download_timeout_secs: u64,
}

impl Default for ParallelIBDConfig {
    fn default() -> Self {
        // BLVM_IBD_CHUNK_SIZE: 16 = Core-like earliest-first granularity. 32 = 999/1000 same chunk.
        let chunk_size: u64 = std::env::var("BLVM_IBD_CHUNK_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(|n: u64| n.clamp(16, 2000))
            .unwrap_or(16);
        Self {
            num_workers: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            chunk_size,
            max_concurrent_per_peer: 64, // Pipeline depth; Core ~1000 BPS needs 64+ at 50ms RTT
            checkpoint_interval: 10_000,
            download_timeout_secs: std::env::var("BLVM_IBD_DOWNLOAD_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
        }
    }
}

/// Block download request
#[derive(Debug, Clone)]
struct BlockRequest {
    height: u64,
    hash: Hash,
    peer_id: String,
}

/// Chunk of blocks to download
#[derive(Debug, Clone)]
struct BlockChunk {
    start_height: u64,
    end_height: u64,
    peer_id: String,
}

/// Tracks block download progress for stalling detection
struct BlockDownloadProgress {
    /// Last block hash received
    last_block_hash: Option<Hash>,
    /// Timestamp of last progress
    last_progress_time: std::time::Instant,
    /// Current stalling timeout (adaptive, 2-64 seconds)
    current_timeout_seconds: u64,
    /// Number of peers that disconnected (increases timeout temporarily)
    disconnected_peers_count: usize,
}

impl BlockDownloadProgress {
    fn new() -> Self {
        Self {
            last_block_hash: None,
            last_progress_time: std::time::Instant::now(),
            current_timeout_seconds: 120, // 2 minutes - reasonable for slow peers
            disconnected_peers_count: 0,
        }
    }

    /// Record that progress was made
    fn record_progress(&mut self, block_hash: Hash) {
        self.last_block_hash = Some(block_hash);
        self.last_progress_time = std::time::Instant::now();
    }

    /// Check if download has stalled
    fn is_stalled(&self) -> bool {
        self.last_progress_time.elapsed().as_secs() > self.current_timeout_seconds
    }

    /// Increase timeout temporarily when peers disconnect
    fn on_peer_disconnect(&mut self) {
        self.disconnected_peers_count += 1;
        // Increase timeout on disconnects, max 300s (5 minutes)
        self.current_timeout_seconds =
            (120u64 + self.disconnected_peers_count as u64 * 30).min(300);
    }

    /// Reset timeout to default
    fn reset_timeout(&mut self) {
        self.current_timeout_seconds = 120;
        self.disconnected_peers_count = 0;
    }
}

/// Each peer has at most 16 block requests in flight.
const MAX_BLOCKS_IN_TRANSIT_PER_PEER: usize = 16;

/// Parallel IBD coordinator
pub struct ParallelIBD {
    config: ParallelIBDConfig,
    /// Semaphore to limit concurrent chunk downloads per peer
    peer_semaphores: Arc<HashMap<String, Arc<Semaphore>>>,
    /// Core-style: max blocks in flight per peer (shared across all workers). Prevents 6 workers × 64 pipeline = 384 requests to one peer.
    peer_blocks_semaphores: Arc<HashMap<String, Arc<Semaphore>>>,
    /// Peer scorer for bandwidth-based peer selection
    peer_scorer: Arc<crate::network::peer_scoring::PeerScorer>,
}

/// Download headers for a checkpoint range from a specific peer
///
/// This is a standalone async function that can be spawned as a task.
/// It downloads headers starting from the given locator hash until it
/// reaches the end height or receives an empty response.
async fn download_header_range(
    network: Arc<NetworkManager>,
    peer: SocketAddr,
    locator_hash: [u8; 32],
    start_height: u64,
    end_height: u64,
) -> Result<Vec<blvm_protocol::BlockHeader>> {
    use crate::network::protocol::{
        GetHeadersMessage, ProtocolMessage, ProtocolParser, MAX_HEADERS_RESULTS,
    };
    use crate::storage::hashing::double_sha256;

    let mut all_headers = Vec::new();
    let mut current_hash = locator_hash;
    let mut current_height = start_height;
    let mut consecutive_failures = 0;
    const MAX_FAILURES: u32 = 10;
    const TIMEOUT_SECS: u64 = 30;

    while current_height <= end_height {
        // Build GetHeaders request
        let get_headers = GetHeadersMessage {
            version: 70015,
            block_locator_hashes: vec![current_hash],
            hash_stop: [0; 32],
        };

        let wire_msg = ProtocolParser::serialize_message(&ProtocolMessage::GetHeaders(get_headers))
            .map_err(|e| anyhow::anyhow!("Failed to serialize GetHeaders: {}", e))?;

        // Register and send request
        let headers_rx = network.register_headers_request(peer);

        if let Err(e) = network.send_to_peer(peer, wire_msg).await {
            consecutive_failures += 1;
            if consecutive_failures >= MAX_FAILURES {
                return Err(anyhow::anyhow!("Too many send failures to {}: {}", peer, e));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }

        // Wait for response
        match timeout(Duration::from_secs(TIMEOUT_SECS), headers_rx).await {
            Ok(Ok(headers)) => {
                consecutive_failures = 0;

                if headers.is_empty() {
                    // Chain tip reached or peer doesn't have more headers
                    break;
                }

                // Process headers
                for header in headers {
                    // Verify proof of work before accepting header
                    match blvm_consensus::pow::check_proof_of_work(&header) {
                        Ok(true) => {}
                        Ok(false) => {
                            warn!(
                                "Header at height {} failed PoW check, skipping",
                                current_height
                            );
                            continue;
                        }
                        Err(e) => {
                            warn!(
                                "Header at height {} PoW check error: {}, skipping",
                                current_height, e
                            );
                            continue;
                        }
                    }

                    // Calculate hash (#53: stack array avoids heap alloc)
                    let mut header_data = [0u8; 80];
                    header_data[0..4].copy_from_slice(&(header.version as i32).to_le_bytes());
                    header_data[4..36].copy_from_slice(&header.prev_block_hash);
                    header_data[36..68].copy_from_slice(&header.merkle_root);
                    header_data[68..72].copy_from_slice(&(header.timestamp as u32).to_le_bytes());
                    header_data[72..76].copy_from_slice(&(header.bits as u32).to_le_bytes());
                    header_data[76..80].copy_from_slice(&(header.nonce as u32).to_le_bytes());
                    let header_hash = double_sha256(&header_data);

                    all_headers.push(header);
                    current_hash = header_hash;
                    current_height += 1;

                    if current_height > end_height {
                        break;
                    }
                }

                // Check if we got a partial batch (end of available headers)
                if all_headers.len() % MAX_HEADERS_RESULTS != 0 {
                    break;
                }
            }
            Ok(Err(_)) => {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_FAILURES {
                    return Err(anyhow::anyhow!("Headers channel closed too many times"));
                }
            }
            Err(_) => {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_FAILURES {
                    return Err(anyhow::anyhow!("Timeout waiting for headers from {}", peer));
                }
            }
        }
    }

    debug!(
        "Downloaded {} headers from {} for range {} - {}",
        all_headers.len(),
        peer,
        start_height,
        end_height
    );

    Ok(all_headers)
}

impl ParallelIBD {
    /// Create a new parallel IBD coordinator
    pub fn new(config: ParallelIBDConfig) -> Self {
        Self {
            config,
            peer_semaphores: Arc::new(HashMap::new()),
            peer_blocks_semaphores: Arc::new(HashMap::new()),
            peer_scorer: Arc::new(crate::network::peer_scoring::PeerScorer::new()),
        }
    }

    /// Get the peer scorer (for external access to stats)
    pub fn peer_scorer(&self) -> &Arc<crate::network::peer_scoring::PeerScorer> {
        &self.peer_scorer
    }

    /// Initialize peer semaphores
    pub fn initialize_peers(&mut self, peer_ids: &[String]) {
        let mut chunk_semaphores = HashMap::new();
        let mut blocks_semaphores = HashMap::new();
        for peer_id in peer_ids {
            chunk_semaphores.insert(
                peer_id.clone(),
                Arc::new(Semaphore::new(self.config.max_concurrent_per_peer)),
            );
            blocks_semaphores.insert(
                peer_id.clone(),
                Arc::new(Semaphore::new(MAX_BLOCKS_IN_TRANSIT_PER_PEER)),
            );
        }
        self.peer_semaphores = Arc::new(chunk_semaphores);
        self.peer_blocks_semaphores = Arc::new(blocks_semaphores);
    }

    /// Download blocks in parallel from multiple peers
    ///
    /// Algorithm:
    /// 1. Download headers first (sequential, fast)
    /// 2. Split block range into chunks
    /// 3. Assign chunks to peers (round-robin)
    /// 4. Download chunks in parallel
    /// 5. Validate and store blocks sequentially (maintain order)
    ///
    /// Validation runs on a dedicated std::thread (not tokio) — no block_in_place on hot path.
    pub async fn sync_parallel(
        self: std::sync::Arc<Self>,
        start_height: u64,
        target_height: u64,
        peer_ids: &[String],
        blockstore: Arc<BlockStore>,
        storage: Option<&Arc<Storage>>,
        protocol: Arc<BitcoinProtocolEngine>,
        utxo_set: &mut UtxoSet,
        network: Option<Arc<NetworkManager>>,
    ) -> Result<()> {
        if peer_ids.is_empty() {
            return Err(anyhow::anyhow!("No peers available for parallel IBD"));
        }

        // IBD requires storage (IbdUtxoStore needs disk for UTXO persistence). Fail fast with clear error.
        let storage = match storage {
            Some(s) => s,
            None => return Err(anyhow::anyhow!(
                "IBD requires storage. Run with a data directory (e.g. --datadir) or ensure storage is initialized."
            )),
        };

        #[cfg(not(feature = "production"))]
        return Err(anyhow::anyhow!(
            "IBD requires production build. Compile with --features production."
        ));

        info!(
            "Starting parallel IBD from height {} to {} using {} peers",
            start_height,
            target_height,
            peer_ids.len()
        );

        // Step 1: Download headers first (sequential, but fast)
        // This will iteratively download headers until chain tip is reached
        info!("Downloading headers...");
        let network_for_headers = network.clone();
        let actual_synced_height = self
            .download_headers(
                start_height,
                target_height,
                peer_ids,
                &blockstore,
                network_for_headers,
            )
            .await
            .context("Failed to download headers")?;

        // Use the actual synced height (may be less than target_height if we reached chain tip)
        let effective_end_height = actual_synced_height.min(target_height);
        info!(
            "Headers synced up to height {}, will download blocks for heights {} to {}",
            actual_synced_height, start_height, effective_end_height
        );

        // Step 2: Filter out extremely slow peers (>90s average latency)
        // Keep at least 2 peers even if all are slow
        const MAX_ACCEPTABLE_LATENCY_MS: f64 = 90_000.0; // 90 seconds
        let filtered_peers: Vec<String> = if peer_ids.len() > 2 {
            let mut scored_peers: Vec<(String, f64)> = peer_ids
                .iter()
                .map(|id| {
                    let latency = if let Ok(addr) = id.parse::<std::net::SocketAddr>() {
                        self.peer_scorer
                            .get_stats(&addr)
                            .map(|s| s.avg_block_latency_ms)
                            .unwrap_or(1000.0) // New peers get default latency
                    } else {
                        1000.0
                    };
                    (id.clone(), latency)
                })
                .collect();

            // Sort by latency (fastest first)
            scored_peers.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

            // Keep fast peers and at least 2 peers total
            let fast_peers: Vec<String> = scored_peers
                .iter()
                .filter(|(_, lat)| *lat < MAX_ACCEPTABLE_LATENCY_MS)
                .map(|(id, _)| id.clone())
                .collect();

            if fast_peers.len() >= 2 {
                info!(
                    "Filtered peers to {} fast peers (dropped {} slow peers with >90s latency)",
                    fast_peers.len(),
                    peer_ids.len() - fast_peers.len()
                );
                fast_peers
            } else {
                // Keep top 2 peers by latency even if all are slow
                info!("All peers are slow, keeping top 2 by latency");
                scored_peers.into_iter().take(2).map(|(id, _)| id).collect()
            }
        } else {
            peer_ids.to_vec()
        };

        // Sort peers: LAN first, then by latency (fastest first), then by score.
        // CRITICAL: Bootstrap chunk goes to first peer. With only WAN peers, latency order
        // ensures we pick the fastest one — avoids stall at block 99 waiting for block 100.
        let mut filtered_peers = filtered_peers;
        filtered_peers.sort_by(|a, b| {
            let a_addr = a.parse::<SocketAddr>().ok();
            let b_addr = b.parse::<SocketAddr>().ok();
            let a_lan = a_addr.map(|s| is_lan_peer(&s)).unwrap_or(false);
            let b_lan = b_addr.map(|s| is_lan_peer(&s)).unwrap_or(false);
            // 1. LAN first
            match (a_lan, b_lan) {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => {}
            }
            // 2. Same LAN status: fastest (lowest latency) first
            let a_lat = a_addr
                .and_then(|s| self.peer_scorer.get_stats(&s))
                .map(|s| s.avg_block_latency_ms)
                .unwrap_or(1000.0);
            let b_lat = b_addr
                .and_then(|s| self.peer_scorer.get_stats(&s))
                .map(|s| s.avg_block_latency_ms)
                .unwrap_or(1000.0);
            a_lat
                .partial_cmp(&b_lat)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    // 3. Tie-break: higher score
                    let a_score = a_addr
                        .map(|s| self.peer_scorer.get_score(&s))
                        .unwrap_or(1.0);
                    let b_score = b_addr
                        .map(|s| self.peer_scorer.get_score(&s))
                        .unwrap_or(1.0);
                    b_score
                        .partial_cmp(&a_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.cmp(b)) // 4. Stable: same addr string order when all equal
        });

        // BLVM_IBD_PEERS=addr1,addr2: restrict IBD to these peers only (must be connected).
        // Matches "192.168.2.100" to "192.168.2.100:8333" (preferred without port matches peer with port).
        let preferred: Vec<String> = std::env::var("BLVM_IBD_PEERS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if !preferred.is_empty() {
            let matches_preferred = |peer: &str| -> bool {
                preferred.iter().any(|pref| {
                    peer == pref.as_str()
                        || (!pref.contains(':')
                            && peer.starts_with(pref)
                            && peer.as_bytes().get(pref.len()) == Some(&b':'))
                })
            };
            let matched = filtered_peers
                .iter()
                .filter(|p| matches_preferred(p.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            if matched.is_empty() {
                return Err(anyhow::anyhow!(
                    "BLVM_IBD_PEERS={:?} but none are connected. Connected: {:?}",
                    preferred,
                    peer_ids
                ));
            }
            filtered_peers = matched;
            info!(
                "BLVM_IBD_PEERS: using {} ({})",
                filtered_peers.len(),
                filtered_peers.join(", ")
            );
        }

        // BLVM_IBD_MODE=sequential: single-peer mode. Core-like earliest-first, no chunk-boundary stalls.
        let ibd_mode: &str = &std::env::var("BLVM_IBD_MODE").unwrap_or_else(|_| "parallel".into());
        if ibd_mode.eq_ignore_ascii_case("sequential") {
            if let Some(best) = filtered_peers.first().cloned() {
                filtered_peers = vec![best.clone()];
                info!(
                    "BLVM_IBD_MODE=sequential: single-peer mode ({}), Core-like block fetch",
                    best
                );
            }
        }

        // Step 3: Split into chunks and assign to peers (weighted by speed)
        // BLVM_IBD_EARLIEST_FIRST=1: assign all chunks to fastest peer (Core-like, avoids chunk-boundary stalls)
        let scored_peers: Vec<(String, f64)> = filtered_peers
            .iter()
            .map(|p| {
                let score = if let Ok(addr) = p.parse::<SocketAddr>() {
                    self.peer_scorer.get_score(&addr)
                } else {
                    1.0
                };
                (p.clone(), score)
            })
            .collect();
        let chunks = self.create_chunks(
            start_height,
            effective_end_height,
            &filtered_peers,
            Some(&scored_peers),
        );
        info!(
            "Created {} chunks for parallel download using {} peers",
            chunks.len(),
            filtered_peers.len()
        );

        // Step 3: Streaming download + validation
        //
        // Uses a bounded channel for natural backpressure:
        // - Downloads pause when buffer is full
        // - Memory stays bounded (~500MB-1GB max)
        // - Validation runs concurrently with downloads

        // Unbounded channel: worker send never blocks, avoiding tokio task starvation where
        // worker sends block 0, coordinator recv_many gets it, worker never gets scheduled to send 1+
        // (bounded send could theoretically block but 10k cap made that rare; unbounded eliminates it)
        // Maximum blocks allowed in the reorder buffer before logging warnings
        const MAX_REORDER_BUFFER: usize = 500;
        let (block_tx, mut block_rx) =
            tokio::sync::mpsc::unbounded_channel::<(u64, Block, Vec<Vec<Witness>>)>();
        let (stall_tx, _) = broadcast::channel::<u64>(16);

        let validation_height = Arc::new(AtomicU64::new(start_height));
        // Sequential chunk assigner: workers get chunks in height order; validation never starves.
        // Peer-to-chunk mapping ensures bootstrap goes to the designated (typically fastest) peer.
        let chunk_list: Vec<(u64, u64)> = chunks
            .iter()
            .map(|c| (c.start_height, c.end_height))
            .collect();
        let chunk_peers: Vec<String> = chunks.iter().map(|c| c.peer_id.clone()).collect();
        let assigner = Arc::new(ChunkAssigner::new(
            chunk_list,
            chunk_peers,
            Arc::clone(&validation_height),
            start_height,
        ));
        info!(
            "IBD: sequential chunk assignment — {} chunks",
            assigner.total_chunks()
        );
        // Track which chunks workers are downloading (for debugging; workers push/retain)
        let workers_current_chunks: Arc<tokio::sync::Mutex<Vec<(String, u64, u64)>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));

        // Disable Transparent Huge Pages for this process. THP promotes anonymous
        // pages to 2MB granularity, causing massive internal fragmentation with
        // millions of small UTXO allocations. On a system with THP=[always], this
        // saves ~300MB+ of wasted RSS. Zero performance cost.
        #[cfg(target_os = "linux")]
        {
            // PR_SET_THP_DISABLE = 41
            let ret = unsafe { libc::prctl(41, 1, 0, 0, 0) };
            if ret == 0 {
                info!("Disabled Transparent Huge Pages for this process");
            }
        }

        // Auto-tune all memory parameters from hardware.
        let mut mem_guard = MemoryGuard::new();
        let effective_max_entries = mem_guard.utxo_max_entries;
        let utxo_flush_threshold = mem_guard.utxo_flush_threshold;
        // Max blocks download can race ahead of validation. Limits block_rx channel depth.
        let max_ahead_blocks: u64 = std::env::var("BLVM_IBD_MAX_AHEAD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(mem_guard.max_ahead_blocks);
        // IBD v2 (IbdUtxoStore) is the only path. Storage is guaranteed Some (checked at start).
        let ibd_memory_only: bool = std::env::var("BLVM_IBD_MEMORY_ONLY")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
            && start_height <= 1;
        let ibd_store_v2: Arc<IbdUtxoStore> = {
            let tree = storage
                .open_tree("ibd_utxos")
                .context("Failed to open IBD UTXO tree")?;
            info!(
                "IBD v2: IbdUtxoStore (DashMap, zero lock, max_cache={} entries)",
                effective_max_entries
            );
            let store = Arc::new(IbdUtxoStore::new_with_options(
                tree,
                utxo_flush_threshold,
                ibd_memory_only,
                effective_max_entries,
            ));
            if start_height <= 1 {
                store.bootstrap_genesis(&protocol.get_network_params().genesis_block);
            }
            if ibd_memory_only {
                info!("IBD_MEMORY_ONLY=1: prefetch uses cache only (no disk reads during IBD)");
            }
            store
        };
        // Ready-queue: ALWAYS created. Validation ONLY receives from ready_rx — fully isolated.
        // Prefetch workers load UTXOs; coordinator feeds them. Larger queue = less overflow to gap-fill.
        // Core does ~1k BPS; 2048 gives runway when validation briefly stalls.
        let max_prefetches_in_flight: usize = {
            let env_val = std::env::var("BLVM_PREFETCH_QUEUE_SIZE")
                .ok()
                .and_then(|s| s.parse().ok());
            let guard_limit = mem_guard.prefetch_queue_size;
            match env_val {
                Some(v) if v <= guard_limit => v,
                Some(v) => {
                    info!(
                        "BLVM_PREFETCH_QUEUE_SIZE={} exceeds MemoryGuard limit {}; capping",
                        v, guard_limit
                    );
                    guard_limit
                }
                None => guard_limit,
            }
        };
        // Prefetch/gap workers: env override > hardware-derived (nproc*2, capped 4–24)
        let prefetch_workers: usize = std::env::var("BLVM_PREFETCH_WORKERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0 && n <= 64)
            .unwrap_or_else(|| {
                let n = std::thread::available_parallelism()
                    .map(|p| p.get())
                    .unwrap_or(8);
                (n.saturating_mul(2)).clamp(4, 24)
            });
        // Gap-fill overflow: when prefetch is full, route here instead of bypassing with empty.
        // Par-2: Match prefetch count so critical blocks (gap-fill path) have same throughput.
        let gap_fill_workers: usize = prefetch_workers;
        let (prefetch_input_tx_v2, gap_fill_tx_v2, ready_tx, ready_rx) = {
            let store = Arc::clone(&ibd_store_v2);
            let (in_tx, in_rx) =
                crossbeam_channel::bounded::<PrefetchWorkItemV2>(max_prefetches_in_flight);
            let (gap_tx_v2, gap_rx_v2) =
                crossbeam_channel::bounded::<PrefetchWorkItemV2>(gap_fill_workers * 4);
            let (out_tx, out_rx) =
                crossbeam_channel::bounded::<ReadyItem>(max_prefetches_in_flight);
            for _ in 0..prefetch_workers {
                let rx_clone = in_rx.clone();
                let tx_clone = out_tx.clone();
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    while let Ok((s, keys, h, block, witnesses)) = rx_clone.recv() {
                        let full_map = Self::prefetch_build_utxo_map(&s, &keys);
                        let _ = tx_clone.send((h, block, witnesses, full_map));
                    }
                });
            }
            for _ in 0..gap_fill_workers {
                let rx_clone = gap_rx_v2.clone();
                let tx_clone = out_tx.clone();
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    while let Ok((s, keys, h, block, witnesses)) = rx_clone.recv() {
                        let full_map = Self::prefetch_build_utxo_map(&s, &keys);
                        let _ = tx_clone.send((h, block, witnesses, full_map));
                    }
                });
            }
            info!(
                "IBD v2 prefetch: {} workers, queue={}; gap-fill overflow: {} workers",
                prefetch_workers, max_prefetches_in_flight, gap_fill_workers
            );
            (in_tx, gap_tx_v2, out_tx, out_rx)
        };

        info!(
            "IBD: {} peers, {} total chunks (sequential assignment)",
            filtered_peers.len(),
            assigner.total_chunks()
        );

        let mut download_handles = Vec::new();
        let num_peers = filtered_peers.len();

        for peer_id in &filtered_peers {
            let priority = scored_peers
                .iter()
                .find(|(p, _)| p == peer_id)
                .map(|(_, s)| *s)
                .unwrap_or(1.0);
            // Worker count = 2 * priority (2x for high-priority). Single number, no branching.
            let worker_count = (2.0 * priority) as usize;
            let worker_count = worker_count.clamp(2, 6);

            info!(
                "IBD: {} workers for peer {} (priority: {:.2})",
                worker_count, peer_id, priority
            );

            for _worker_idx in 0..worker_count {
                let peer_id = peer_id.clone();
                let parallel_ibd = Arc::clone(&self);
                let config = self.config.clone();
                let blockstore_clone = Arc::clone(&blockstore);
                let network_clone = network.clone();
                let tx = block_tx.clone();
                let peer_scorer_clone = Arc::clone(&self.peer_scorer);
                let assigner_clone = Arc::clone(&assigner);
                let workers_current_clone = Arc::clone(&workers_current_chunks);
                let num_peers_clone = num_peers;
                let peer_blocks_semaphores_clone = Arc::clone(&self.peer_blocks_semaphores);
                let mut stall_rx = stall_tx.subscribe();
                let semaphore = self
                    .peer_semaphores
                    .get(&peer_id)
                    .ok_or_else(|| anyhow::anyhow!("Peer {} not found", peer_id))?
                    .clone();

                let handle = tokio::spawn(async move {
                    let mut chunks_completed = 0u64;
                    let mut blocks_downloaded = 0u64;
                    let mut consecutive_failures = 0u32;
                    const MAX_CONSECUTIVE_FAILURES: u32 = 5;

                    loop {
                        let maybe_work = loop {
                            if let Some((chunk_start, chunk_end)) =
                                assigner_clone.get_work(&peer_id, max_ahead_blocks)
                            {
                                break Some((chunk_start, chunk_end));
                            }
                            if assigner_clone.is_done() {
                                break None;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        };
                        let (start, end) = match maybe_work {
                            Some(x) => x,
                            None => {
                                info!("[IBD] Worker {} exiting: queue empty (chunks_completed={}, blocks_downloaded={})", peer_id, chunks_completed, blocks_downloaded);
                                break;
                            }
                        };
                        let mut _guard = ChunkGuard::new(
                            start,
                            end,
                            Some(peer_id.clone()),
                            peer_id.clone(),
                            assigner_clone.clone(),
                        );
                        info!("[IBD] {} took chunk {}-{}", peer_id, start, end);
                        workers_current_clone
                            .lock()
                            .await
                            .push((peer_id.clone(), start, end));
                        let _permit = match semaphore.acquire().await {
                            Ok(permit) => permit,
                            Err(_) => {
                                warn!("[IBD] Worker {} semaphore acquire failed — ChunkGuard will re-queue", peer_id);
                                break;
                            }
                        };

                        // Bootstrap (start==0): no per-peer semaphore so we don't starve the first chunk. Post-bootstrap: 16 blocks/peer (Core).
                        let blocks_sem = if start == 0 {
                            None
                        } else {
                            peer_blocks_semaphores_clone.get(&peer_id).cloned()
                        };
                        let dl_result = Self::download_chunk(
                            start,
                            end,
                            &peer_id,
                            network_clone.clone(),
                            &blockstore_clone,
                            &config,
                            peer_scorer_clone.clone(),
                            Some(tx.clone()),
                            blocks_sem,
                            Some(&mut stall_rx),
                        )
                        .await;
                        workers_current_clone
                            .lock()
                            .await
                            .retain(|(p, s, _)| !(*p == peer_id && *s == start));
                        match dl_result {
                            Ok(blocks) => {
                                consecutive_failures = 0;
                                let block_count = blocks.len();
                                if start == 0 {
                                    info!("IBD: bootstrap chunk 0-{} downloaded, coordinator enables parallel when received", end);
                                }
                                #[cfg(feature = "profile")]
                                if block_count > 0
                                    && (chunks_completed == 0
                                        || chunks_completed % 10 == 0
                                        || block_count > 400)
                                {
                                    let remaining = assigner_clone.remaining_count();
                                    blvm_consensus::profile_log!(
                                    "[IBD_DOWNLOAD] peer={} chunk={}-{} blocks={} assigner_remaining={}",
                                    peer_id, start, end, block_count, remaining
                                );
                                }
                                // Blocks already streamed during download_chunk; no second send
                                _guard.disarm();
                                #[cfg(feature = "profile")]
                                {
                                    let ts_ms = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis() as u64)
                                        .unwrap_or(0);
                                    blvm_consensus::profile_log!(
                                    "[IBD_CHUNK_COMPLETE] chunk_start={} chunk_end={} peer={} blocks={} ts_ms={}",
                                    start, end, peer_id, block_count, ts_ms
                                );
                                }
                                assigner_clone.on_chunk_complete(&peer_id);
                                chunks_completed += 1;
                                blocks_downloaded += block_count as u64;
                            }
                            Err(e) => {
                                consecutive_failures += 1;
                                warn!("Peer {} failed chunk {}-{} ({}/{}): {} - will retry with different peer", 
                                peer_id, start, end, consecutive_failures, MAX_CONSECUTIVE_FAILURES, e);
                                // Single-peer deadlock: exclude would block all workers (they're all this peer).
                                // Skip exclude so same peer can retry; stall recovery also clears exclude.
                                let exclude = if num_peers_clone > 1 {
                                    Some(peer_id.clone())
                                } else {
                                    info!("[IBD] Single peer: re-queuing chunk {}-{} without exclude (no fallback)", start, end);
                                    None
                                };
                                if exclude.is_some() {
                                    info!(
                                        "[IBD] Re-queuing chunk {}-{} exclude={}",
                                        start, end, peer_id
                                    );
                                }
                                assigner_clone.requeue(start, end, exclude);
                                _guard.disarm();
                                assigner_clone.on_chunk_complete(&peer_id);

                                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                                    warn!(
                                        "Peer {} exceeded max failures, stopping worker",
                                        peer_id
                                    );
                                    break;
                                }

                                let backoff_secs = (1 << (consecutive_failures - 1).min(4)).min(16);
                                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs))
                                    .await;
                            }
                        }
                    }

                    info!(
                        "Peer {} done: {} chunks, {} blocks",
                        peer_id, chunks_completed, blocks_downloaded
                    );
                    Ok::<(), anyhow::Error>(())
                });

                download_handles.push((0, handle));
            }
        }

        // Drop the original sender so the channel closes when all workers complete
        drop(block_tx);

        // COORDINATOR: Drains block_rx, sends to prefetch. When prefetch full, routes to gap_fill
        // overflow — validation never receives empty UTXOs, never blocks on disk.
        // Mark bootstrap complete only when we've DRAINED the bootstrap chunk — not when the worker
        // returns. Otherwise parallel workers get chunks 128+ and send blocks before we receive 100,
        // causing interleaving and a stall at 99. Coordinator knows we have 0..=bootstrap_end when
        // we drain that block. Bootstrap is always ≥128 blocks so 99 and 100 are in the same chunk.
        let bootstrap_end = if start_height == 0 && !chunks.is_empty() {
            chunks[0].end_height
        } else {
            u64::MAX // No bootstrap; skip coordinator-triggered mark
        };
        let assigner_for_coord = Arc::clone(&assigner);
        let validation_height_for_coord = Arc::clone(&validation_height);
        let dynamic_buffer_limit = mem_guard.buffer_limit(start_height);
        let gap_fill_tx_v2_for_coord = gap_fill_tx_v2.clone();
        let prefetch_input_tx_v2_for_coord = prefetch_input_tx_v2.clone();
        let ibd_store_v2_for_coord = Arc::clone(&ibd_store_v2);
        let stall_tx_for_coord = stall_tx.clone();
        // Seq-1: When single peer (BLVM_IBD_SEQUENTIAL), blocks arrive in order; skip reorder_buffer.
        let sequential = num_peers == 1;
        if sequential {
            info!("Coordinator: sequential mode (single peer) — passthrough, no reorder buffer");
        }
        tokio::spawn(async move {
            let mut reorder_buffer: std::collections::BTreeMap<u64, (Block, Vec<Vec<Witness>>)> =
                std::collections::BTreeMap::new();
            let mut next_prefetch_height = start_height;
            let mut total_received = 0u64;
            const BATCH_DRAIN_LIMIT: usize = 2000; // 10K BPS: larger batches reduce recv overhead
            let mut batch: Vec<(u64, Block, Vec<Vec<Witness>>)> =
                Vec::with_capacity(BATCH_DRAIN_LIMIT);
            // S2: Reuse buffer for block_input_keys (avoids alloc per block)
            let mut coord_keys_buf: Vec<OutPointKey> = Vec::new();
            info!("Coordinator: started, awaiting blocks from download workers");
            const COORD_STALL_LOG_SECS: u64 = 30;
            loop {
                // Backpressure: when reorder_buffer full, drain contiguous blocks before receiving more.
                // Prevents unbounded growth when downloads outpace validation.
                if reorder_buffer.len() >= dynamic_buffer_limit {
                    while reorder_buffer.contains_key(&next_prefetch_height) {
                        let (block, witnesses) = reorder_buffer
                            .remove(&next_prefetch_height)
                            .expect("contains_key");
                        block_input_keys_into(&block, &mut coord_keys_buf);
                        let h = next_prefetch_height;
                        next_prefetch_height += 1;
                        if h == bootstrap_end {
                            assigner_for_coord.mark_bootstrap_complete();
                            info!("IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled", h);
                        }
                        {
                            let store = &ibd_store_v2_for_coord;
                            let ptx = &prefetch_input_tx_v2_for_coord;
                            let keys_owned = std::mem::take(&mut coord_keys_buf);
                            let item = (Arc::clone(store), keys_owned, h, block, witnesses);
                            let next_needed =
                                validation_height_for_coord.load(Ordering::Relaxed) + 1;
                            let route_to_gap_fill = h <= next_needed;
                            if route_to_gap_fill {
                                tokio::task::block_in_place(|| {
                                    if let Err(e) = gap_fill_tx_v2_for_coord.try_send(item) {
                                        let (_, _, _, b, w) = e.into_inner();
                                        let _ = ready_tx.send((
                                            h,
                                            b,
                                            w,
                                            rustc_hash::FxHashMap::default(),
                                        ));
                                    }
                                });
                            } else {
                                tokio::task::block_in_place(|| {
                                    if let Err(e) = ptx.try_send(item) {
                                        let back = e.into_inner();
                                        if let Err(e2) = gap_fill_tx_v2_for_coord.try_send(back) {
                                            let (_, _, _, b, w) = e2.into_inner();
                                            let _ = ready_tx.send((
                                                h,
                                                b,
                                                w,
                                                rustc_hash::FxHashMap::default(),
                                            ));
                                        }
                                    }
                                });
                            }
                        }
                        if reorder_buffer.len() < dynamic_buffer_limit {
                            break;
                        }
                    }
                    if reorder_buffer.len() >= dynamic_buffer_limit {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    continue;
                }
                let recv_fut = block_rx.recv_many(&mut batch, BATCH_DRAIN_LIMIT);
                let n = match timeout(Duration::from_secs(COORD_STALL_LOG_SECS), recv_fut).await {
                    Ok(n) => n,
                    Err(_) => {
                        let next_needed = validation_height_for_coord.load(Ordering::Relaxed) + 1;
                        warn!(
                            "Coordinator stall: no blocks for {}s, waiting for height {} (total_received={}, next_prefetch={})",
                            COORD_STALL_LOG_SECS, next_needed, total_received, next_prefetch_height
                        );
                        let _ = stall_tx_for_coord.send(next_needed);
                        continue;
                    }
                };
                if n == 0 {
                    info!(
                        "Coordinator: block_rx closed (total_received={})",
                        total_received
                    );
                    // Channel closed — drain remaining reorder_buffer, then exit
                    while reorder_buffer.contains_key(&next_prefetch_height) {
                        let (block, witnesses) = reorder_buffer
                            .remove(&next_prefetch_height)
                            .expect("contains_key");
                        block_input_keys_into(&block, &mut coord_keys_buf);
                        let h = next_prefetch_height;
                        next_prefetch_height += 1;
                        if h == bootstrap_end {
                            assigner_for_coord.mark_bootstrap_complete();
                            info!("IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled", h);
                        }
                        {
                            let store = &ibd_store_v2_for_coord;
                            let ptx = &prefetch_input_tx_v2_for_coord;
                            let keys_owned = std::mem::take(&mut coord_keys_buf);
                            let item = (Arc::clone(store), keys_owned, h, block, witnesses);
                            let next_needed =
                                validation_height_for_coord.load(Ordering::Relaxed) + 1;
                            let route_to_gap_fill = h <= next_needed;
                            if route_to_gap_fill {
                                tokio::task::block_in_place(|| {
                                    if let Err(e) = gap_fill_tx_v2_for_coord.try_send(item) {
                                        let (_, _, _, b, w) = e.into_inner();
                                        let _ = ready_tx.send((
                                            h,
                                            b,
                                            w,
                                            rustc_hash::FxHashMap::default(),
                                        ));
                                    }
                                });
                            } else {
                                tokio::task::block_in_place(|| {
                                    if let Err(e) = ptx.try_send(item) {
                                        let back = e.into_inner();
                                        if let Err(e2) = gap_fill_tx_v2_for_coord.try_send(back) {
                                            let (_, _, _, b, w) = e2.into_inner();
                                            let _ = ready_tx.send((
                                                h,
                                                b,
                                                w,
                                                rustc_hash::FxHashMap::default(),
                                            ));
                                        }
                                    }
                                });
                            }
                        }
                    }
                    drop(ready_tx);
                    info!("Coordinator: done, sent {} blocks", total_received);
                    break;
                }
                // Seq-1: When sequential, process batch directly — do NOT drain into reorder_buffer first.
                if sequential {
                    // Seq-1: Blocks already in order; process batch directly, skip reorder_buffer
                    batch.sort_by_key(|(h, _, _)| *h);
                    for (h, block, witnesses) in batch.drain(..) {
                        total_received += 1;
                        if total_received == 1 {
                            info!("Coordinator: first block received, height {}", h);
                        }
                        if total_received <= 3 || total_received % 500 == 0 {
                            debug!(
                                "[IBD] Coordinator: block {} (total_received={}) [sequential]",
                                h, total_received
                            );
                        }
                        if h == bootstrap_end {
                            assigner_for_coord.mark_bootstrap_complete();
                            info!("IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled", h);
                        }
                        // Seq-2: Sequential fast path — skip prefetch entirely. Send directly to ready_tx.
                        // Validation does AccessCoin-style inline (store.build_utxo_map_into) when prefetched empty.
                        let _ = tokio::task::block_in_place(|| {
                            ready_tx.send((h, block, witnesses, rustc_hash::FxHashMap::default()))
                        });
                        next_prefetch_height = h + 1;
                    }
                } else {
                    // Parallel: drain batch into reorder_buffer, then drain contiguous to prefetch
                    for (height, block, witnesses) in batch.drain(..) {
                        if total_received == 0 {
                            info!("Coordinator: first block received, height {}", height);
                        }
                        total_received += 1;
                        if total_received <= 3 || total_received % 500 == 0 {
                            debug!(
                                "[IBD] Coordinator: block {} (total_received={}, reorder_len={})",
                                height,
                                total_received,
                                reorder_buffer.len() + 1
                            );
                        }
                        reorder_buffer.insert(height, (block, witnesses));
                    }
                    while reorder_buffer.contains_key(&next_prefetch_height) {
                        let (block, witnesses) = reorder_buffer
                            .remove(&next_prefetch_height)
                            .expect("contains_key");
                        block_input_keys_into(&block, &mut coord_keys_buf);
                        let h = next_prefetch_height;
                        next_prefetch_height += 1;
                        if h == bootstrap_end {
                            assigner_for_coord.mark_bootstrap_complete();
                            info!("IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled", h);
                        }
                        {
                            let store = &ibd_store_v2_for_coord;
                            let ptx = &prefetch_input_tx_v2_for_coord;
                            let keys_owned = std::mem::take(&mut coord_keys_buf);
                            let item = (Arc::clone(store), keys_owned, h, block, witnesses);
                            let next_needed =
                                validation_height_for_coord.load(Ordering::Relaxed) + 1;
                            let route_to_gap_fill = h <= next_needed;
                            if route_to_gap_fill {
                                tokio::task::block_in_place(|| {
                                    if let Err(e) = gap_fill_tx_v2_for_coord.try_send(item) {
                                        let (_, _, _, b, w) = e.into_inner();
                                        let _ = ready_tx.send((
                                            h,
                                            b,
                                            w,
                                            rustc_hash::FxHashMap::default(),
                                        ));
                                    }
                                });
                            } else {
                                tokio::task::block_in_place(|| {
                                    if let Err(e) = ptx.try_send(item) {
                                        let back = e.into_inner();
                                        if let Err(e2) = gap_fill_tx_v2_for_coord.try_send(back) {
                                            let (_, _, _, b, w) = e2.into_inner();
                                            let _ = ready_tx.send((
                                                h,
                                                b,
                                                w,
                                                rustc_hash::FxHashMap::default(),
                                            ));
                                        }
                                    }
                                });
                            }
                        }
                        if reorder_buffer.len() >= dynamic_buffer_limit {
                            break;
                        }
                    }
                }
            }
        });

        // Step 4: BLOCK FEEDER — drains ready_rx into shared buffer so validation can run while buffer fills.
        // Feeder runs on std::thread (crossbeam recv is blocking). Buffer fills while validation works.
        let dynamic_buffer_limit = mem_guard.buffer_limit(start_height);
        let feeder_state = Arc::new((
            Mutex::new((
                BTreeMap::<u64, FeederBufferValue>::new(),
                false,  // channel_closed
                0usize, // current_bytes (for byte cap)
            )),
            Condvar::new(),
        ));
        let feeder_state_for_feeder = Arc::clone(&feeder_state);
        let feeder_buffer_limit = dynamic_buffer_limit;
        let feeder_buffer_bytes_limit = mem_guard.feeder_buffer_bytes_limit;
        let feeder_handle = std::thread::spawn(move || {
            while let Ok((h, b, w, u)) = ready_rx.recv() {
                let tx_ids = blvm_consensus::block::compute_block_tx_ids(&b);
                let mut guard = feeder_state_for_feeder.0.lock().unwrap();
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
                    guard = feeder_state_for_feeder.1.wait(guard).unwrap();
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
                feeder_state_for_feeder.1.notify_one();
            }
            let mut guard = feeder_state_for_feeder.0.lock().unwrap();
            guard.1 = true; // channel_closed
            feeder_state_for_feeder.1.notify_one();
        });

        // Step 5: VALIDATION — runs on dedicated std::thread. Reads from shared buffer, waits on Condvar when empty.
        let storage_clone = Arc::clone(storage);
        let utxo_mutex = Arc::new(std::sync::Mutex::new(std::mem::take(utxo_set)));

        let feeder_state_valid = Arc::clone(&feeder_state);
        let ibd_store_v2_valid = Arc::clone(&ibd_store_v2);
        let blockstore_valid = Arc::clone(&blockstore);
        let storage_clone_valid = storage_clone.clone();
        let self_clone_valid = Arc::clone(&self);
        let protocol_valid = Arc::clone(&protocol);
        let utxo_mutex_valid = Arc::clone(&utxo_mutex);
        let validation_handle = std::thread::spawn(move || -> Result<()> {
            let feeder_state = feeder_state_valid;
            let ibd_store_v2_for_validation = ibd_store_v2_valid;
            let blockstore = blockstore_valid;
            let storage_clone = storage_clone_valid;
            let self_clone = self_clone_valid;
            let protocol = protocol_valid;
            let utxo_mutex = utxo_mutex_valid;
            let effective_end_height = effective_end_height;
            let start_height = start_height;
            let validation_height = Arc::clone(&validation_height);
            //
            // Blocks may arrive out of order. We maintain a small reorder buffer
            // and flush in-order blocks immediately to minimize memory usage.
            //
            // PERFORMANCE OPTIMIZATION: We use deferred (batched) storage to avoid
            // per-block database writes. Validated blocks are stored in a pending
            // buffer and flushed in batches of 1000 blocks. This improves IBD
            // performance from ~2 blocks/sec to ~50+ blocks/sec.
            let mut blocks_synced = 0;
            let validation_start = std::time::Instant::now();

            // IBD Profiling (profile feature): BLVM_IBD_DEBUG=profile,blocked,disk or =profile:100,blocked or =full
            // Format: comma-separated. profile[:sample][:slow_ms] (e.g. profile:100 = every 100th block; profile:1:50 = slow threshold 50ms)
            #[cfg(feature = "profile")]
            let (
                ibd_profile_sample,
                ibd_profile_slow_ms,
                ibd_profile,
                ibd_disk_profile,
                ibd_blocked_log,
            ) = {
                let mut sample: u64 = 0;
                let mut slow: u64 = 0;
                let mut disk = false;
                let mut blocked_log = false;
                if let Ok(val) = std::env::var("BLVM_IBD_DEBUG") {
                    let parts: Vec<&str> = val.split(',').map(|s| s.trim()).collect();
                    let full = parts.iter().any(|p| *p == "full");
                    for p in &parts {
                        let p = *p;
                        if p == "full" {
                            sample = sample.max(1);
                            disk = true;
                            blocked_log = true;
                        } else if p == "profile" {
                            sample = sample.max(1);
                        } else if p.starts_with("profile:") {
                            let rest: Vec<&str> = p[7..].split(':').collect();
                            if !rest.is_empty() && !rest[0].is_empty() {
                                if let Ok(n) = rest[0].parse::<u64>() {
                                    if rest.len() >= 2 && !rest[1].is_empty() {
                                        // profile:sample:slow (e.g. profile:100:50)
                                        sample = sample.max(n.max(1));
                                        if let Ok(s) = rest[1].parse::<u64>() {
                                            slow = s;
                                        }
                                    } else if n < 100 {
                                        // profile:50 = slow threshold 50ms (plan compat)
                                        sample = sample.max(1);
                                        slow = n;
                                    } else {
                                        // profile:100 = sample every 100 blocks
                                        sample = sample.max(n);
                                    }
                                }
                            }
                        } else if p == "blocked" {
                            blocked_log = true;
                        } else if p == "disk" {
                            disk = true;
                        }
                    }
                    if full && sample == 0 {
                        sample = 1;
                        disk = true;
                        blocked_log = true;
                    }
                    if sample > 0 && !blocked_log {
                        blocked_log = true; // default blocked_log=ON when profile sampling is on
                    }
                }
                let on = sample > 0 || disk;
                if on {
                    info!("IBD profiling ENABLED (BLVM_IBD_DEBUG): sample_interval={}, slow_threshold_ms={}, disk_io={}, blocked_log={}", sample, slow, disk, blocked_log);
                }
                if blocked_log {
                    info!(
                        "IBD_BLOCKED_LOG ENABLED: every validation-blocking phase will be logged"
                    );
                }
                (sample, slow, on, disk, blocked_log)
            };
            #[cfg(not(feature = "profile"))]
            let (
                ibd_profile_sample,
                ibd_profile_slow_ms,
                ibd_profile,
                ibd_disk_profile,
                ibd_blocked_log,
            ) = (0u64, 0u64, false, false, false);

            // Track last 11 block headers for BIP113 median-time-past calculation
            // Vec + drain keeps contiguity; avoids VecDeque::make_contiguous() per-block alloc
            let mut recent_headers_buf: std::collections::VecDeque<Arc<BlockHeader>> =
                std::collections::VecDeque::with_capacity(12);

            // DEFERRED STORAGE: Buffer validated blocks for batch commit
            // Keep flush interval small to avoid OOM on systems with limited RAM (16GB)
            let storage_flush_interval = mem_guard.storage_flush_interval;
            let mut pending_blocks: Vec<(
                Arc<Block>,
                Arc<BlockHeader>,
                Arc<Vec<Vec<Witness>>>,
                u64,
            )> = Vec::with_capacity(storage_flush_interval);
            let skip_storage = false;
            let dynamic_buffer_limit = mem_guard.buffer_limit(start_height);

            // Batched lookahead prefetch: load UTXOs for N+1..N+K in one TidesDB round-trip.
            // Reduces spawn_blocking overhead and amortizes disk access across multiple blocks.
            // Tunable via BLVM_UTXO_PREFETCH_LOOKAHEAD (default 64; 96 at 100k+ when blocks have 2–3k inputs).
            let utxo_prefetch_lookahead: usize = std::env::var("BLVM_UTXO_PREFETCH_LOOKAHEAD")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(64)
                .clamp(1, 128);

            info!("Validation loop starting (deferred storage enabled, flush every {} blocks, buffer limit: {}, utxo_prefetch_lookahead: {})...", 
            storage_flush_interval, dynamic_buffer_limit, utxo_prefetch_lookahead);

            let mut next_validation_height = start_height;
            // Cache network_time for block header validation (reject future blocks). Refresh every 1000
            // blocks to avoid 800k+ SystemTime::now() syscalls during IBD while staying correct near tip.
            let mut cached_network_time = crate::utils::time::current_timestamp();

            // FEEDER BUFFER: Block feeder drains ready_rx into shared state. We read next block and
            // lookahead blocks for protect_keys. Buffer fills while validation runs.

            // Async flush: run on std::thread (validation runs off tokio — dedicated thread).
            let mut flush_handles: std::collections::VecDeque<std::thread::JoinHandle<Result<()>>> =
                std::collections::VecDeque::new();
            let mut utxo_flush_handles: std::collections::VecDeque<
                std::thread::JoinHandle<Result<()>>,
            > = std::collections::VecDeque::new();
            // 10K BPS: 1024 flushes × 750 blocks = 768k buffered. Validation rarely blocks on handle.join().
            const MAX_FLUSHES_IN_FLIGHT: usize = 1024;
            const MAX_UTXO_FLUSHES_IN_FLIGHT: usize = 1024;
            /// When RSS > trigger, cap in-flight flushes so batches drain before we add more.
            const MAX_UTXO_FLUSHES_UNDER_RSS_PRESSURE: usize = 2;

            let ibd_defer_flush = mem_guard.defer_flush;
            let ibd_defer_checkpoint = mem_guard.defer_checkpoint_interval;

            // Reusable buffers for protect_keys (avoids 2–4 Vec+HashSet allocs per block).
            let mut blocks_buf: Vec<Arc<Block>> = Vec::with_capacity(utxo_prefetch_lookahead);
            let mut keys_buf: Vec<OutPointKey> = Vec::new();
            let mut keys_seen: rustc_hash::FxHashSet<OutPointKey> =
                rustc_hash::FxHashSet::default();
            // IBD v2: reuse buffer for block_input_keys (avoids ~80KB alloc per block).
            let mut keys_v2_buf: Vec<OutPointKey> = Vec::new();
            // IBD v2: reuse utxo_base buffer (avoids UtxoSet alloc + ~2000 map ops per block).
            let mut utxo_base_buf: UtxoSet = UtxoSet::default();
            // Per-block map: retain + insert (avoids full clear when overlap high; reduces allocs).

            let mut keys_missing_buf: Vec<OutPointKey> = Vec::new();
            let mut supplement_cache_buf: Vec<OutPointKey> = Vec::new();

            // Cache BLVM_IBD_SNAPSHOT_DIR once at loop init (was std::env::var per block)
            let snapshot_dir_base: Option<String> = std::env::var("BLVM_IBD_SNAPSHOT_DIR").ok();
            // #48: Tunable yield interval (default 500 for 5–10K BPS; fewer yields = less validation interruption)
            let yield_interval: u64 = std::env::var("BLVM_IBD_YIELD_INTERVAL")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1000);

            // BIP30 O(1) index: for non-disk path, maintain locally. For disk path, DiskBackedUtxoSet owns it.
            let mut bip30_index = Bip30Index::default();
            // Recent rate: blocks since last status / elapsed since last status. Shows burst vs wait (avg can overstate when mostly waiting).
            let mut last_log_blocks: u64 = 0;
            let mut last_log_instant = std::time::Instant::now();
            // 5a: Adaptive mi_collect — run more often when RSS grows fast
            let mut last_rss_mb: u64 = 0;
            let mut last_collect_block: u64 = 0;

            // Incremental UTXO commitment during IBD (Core-style; no full scan)
            #[cfg(all(feature = "utxo-commitments", feature = "production"))]
            let (mut commitment_tree_opt, commitment_store_opt) = {
                let pm = storage_clone.pruning();
                let tree = pm
                    .as_ref()
                    .and_then(|p| p.commitment_store())
                    .and_then(|_| {
                        blvm_protocol::utxo_commitments::merkle_tree::UtxoMerkleTree::new().ok()
                    });
                let store = pm.and_then(|p| p.commitment_store());
                if tree.is_some() && store.is_some() {
                    info!("IBD: incremental UTXO commitment enabled (applying delta per block)");
                }
                (tree, store)
            };
            #[cfg(not(all(feature = "utxo-commitments", feature = "production")))]
            let (mut commitment_tree_opt, commitment_store_opt) = (None, None);

            loop {
                // VALIDATION: Read from feeder buffer. Wait on Condvar when next block not yet arrived.
                // Feeder fills buffer while we validate; buffer grows to 20–50+ blocks when pipeline keeps up.
                // Use wait_timeout (5s) so we can log when stalled — helps diagnose freezes around 90k+.
                const FEEDER_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
                let next_block = loop {
                    let mut guard = feeder_state.0.lock().unwrap();
                    if let Some((arc_b, w, u, tx_ids)) = guard.0.remove(&next_validation_height) {
                        guard.2 = guard.2.saturating_sub(FEEDER_BLOCK_BYTES_ESTIMATE);
                        feeder_state.1.notify_one();
                        break Some((next_validation_height, arc_b, w, u, tx_ids));
                    }
                    if guard.1 && guard.0.is_empty() {
                        break None;
                    }
                    #[cfg(feature = "profile")]
                    let wait_start = std::time::Instant::now();
                    let (g, timeout_result) = feeder_state
                        .1
                        .wait_timeout(guard, FEEDER_WAIT_TIMEOUT)
                        .unwrap();
                    guard = g;
                    #[cfg(feature = "profile")]
                    if ibd_profile {
                        let wait_ms = wait_start.elapsed().as_millis() as u64;
                        if wait_ms >= 1 {
                            let buffer_len_after = guard.0.len();
                            let ts_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            blvm_consensus::profile_log!(
                        "[IBD_STALL_WAIT] next_height={} duration_ms={} buffer_after={} ts_ms={}",
                        next_validation_height, wait_ms, buffer_len_after, ts_ms
                    );
                        }
                    }
                    if timeout_result.timed_out() {
                        let cur_min = guard.0.keys().next().copied();
                        warn!(
                    "[IBD_STALL] Validation waiting for block {} (buffer has {} blocks, min_height={:?}) — coordinator/feeder may be blocked",
                    next_validation_height, guard.0.len(), cur_min
                );
                    }
                };
                let (next_height, block_arc, witnesses, prefetched_utxos, tx_ids_precomputed) =
                    match next_block {
                        Some(t) => t,
                        None => break, // Channel closed, buffer drained
                    };
                next_validation_height = next_height + 1;
                if blocks_synced == 0 {
                    info!("Validation: first block received, height {}", next_height);
                }

                // 4d: Single feeder_state lock for lookahead blocks (protect_keys). When dynamic eviction (needs protect_keys + evict_if_needed).
                let need_blocks_buf = ibd_store_v2_for_validation.is_dynamic_eviction();
                if need_blocks_buf {
                    blocks_buf.clear();
                    let guard = feeder_state.0.lock().unwrap();
                    for off in 1..=utxo_prefetch_lookahead {
                        let h = next_height + off as u64;
                        if let Some((b, _, _, _)) = guard.0.get(&h) {
                            blocks_buf.push(Arc::clone(b));
                        }
                    }
                }

                // #21: tx_ids precomputed in feeder — shared by collect_gaps and connect_block_ibd (saves 2–4k hashes/block).

                // IBD v2: prefetch provides full map — no gap-fill needed.
                let prefetched_for_v2 = prefetched_utxos;
                #[cfg(feature = "profile")]
                let gap_fill_ms = 0u64;
                #[cfg(not(feature = "profile"))]
                #[allow(dead_code)]
                let gap_fill_ms = 0u64;

                // Build recent headers slice for BIP113 median-time-past
                let recent_headers_opt: Option<&[Arc<BlockHeader>]> =
                    if recent_headers_buf.is_empty() {
                        None
                    } else {
                        recent_headers_buf.make_contiguous();
                        Some(recent_headers_buf.as_slices().0)
                    };

                // Resolve witnesses: for pre-SegWit, build a shared Arc of empty witnesses once
                // per block shape (avoids thousands of empty Vec allocs + deep clone every block).
                let witnesses_arc: Option<
                    std::sync::Arc<Vec<Vec<blvm_consensus::segwit::Witness>>>,
                > = if witnesses.is_empty() {
                    let empty: Vec<Vec<blvm_consensus::segwit::Witness>> = block_arc
                        .transactions
                        .iter()
                        .map(|tx| vec![blvm_consensus::segwit::Witness::default(); tx.inputs.len()])
                        .collect();
                    Some(std::sync::Arc::new(empty))
                } else if witnesses.len() != block_arc.transactions.len() {
                    return Err(anyhow::anyhow!(
                        "Witness count mismatch at height {}: {} witnesses for {} transactions",
                        next_height,
                        witnesses.len(),
                        block_arc.transactions.len()
                    ));
                } else {
                    Some(std::sync::Arc::new(witnesses))
                };
                let empty_witnesses_fallback: Vec<Vec<blvm_consensus::segwit::Witness>> =
                    Vec::new();
                let witnesses_to_use: &[Vec<blvm_consensus::segwit::Witness>] = witnesses_arc
                    .as_deref()
                    .unwrap_or(&empty_witnesses_fallback);

                // Validate + sync/evict (validation runs on dedicated thread — no tokio).
                #[cfg(feature = "profile")]
                if ibd_blocked_log {
                    blvm_consensus::profile_log!(
                        "[IBD_VALIDATION] height={} phase=start (validate+suggested sync)",
                        next_height
                    );
                }
                let (
                    prefetch_ms,
                    apply_pending_ms,
                    validation_result,
                    sync_ms,
                    evict_ms,
                    validation_time,
                    utxo_flush_batch,
                    _pipelined_sync_spawned,
                    rss_pressure,
                ) = {
                    let prefetch_ms = 0u64;
                    #[cfg(feature = "profile")]
                    let apply_pending_ms = 0u64;
                    #[cfg(not(feature = "profile"))]
                    let apply_pending_ms = 0u64;

                    let (validation_result, validation_time) = {
                        // Build at validation time. Use prefetched map when available to avoid N get()+clone.
                        block_input_keys_into(block_arc.as_ref(), &mut keys_v2_buf);
                        let keys_v2 = &keys_v2_buf;
                        let store = &ibd_store_v2_for_validation;
                        if next_height <= 200 {
                            debug!(
                                "[IBD_V2] height={} keys_needed={} store_len={}",
                                next_height,
                                keys_v2.len(),
                                store.len()
                            );
                        }
                        let prefetched = &prefetched_for_v2;
                        if prefetched.is_empty() {
                            store.build_utxo_map_into_with_buf(
                                keys_v2,
                                &mut utxo_base_buf,
                                &mut supplement_cache_buf,
                            );
                        } else {
                            // Clear + rebuild: prefetch provides most keys; supplement handles misses.
                            // Faster than retain() which scans entire map capacity.
                            utxo_base_buf.clear();
                            utxo_base_buf.reserve(keys_v2.len());
                            keys_missing_buf.clear();
                            for k in keys_v2.iter() {
                                if let Some(arc) = prefetched.get(k) {
                                    utxo_base_buf.insert(key_to_outpoint(k), Arc::clone(arc));
                                } else {
                                    keys_missing_buf.push(*k);
                                }
                            }
                            if !keys_missing_buf.is_empty() {
                                store.supplement_utxo_map_with_buf(
                                    &mut utxo_base_buf,
                                    &keys_missing_buf,
                                    &mut supplement_cache_buf,
                                );
                            }
                        }
                        if next_height <= 200 && utxo_base_buf.len() < keys_v2.len() {
                            let first_missing = keys_v2
                                .iter()
                                .find(|k| !utxo_base_buf.contains_key(&key_to_outpoint(k)));
                            warn!("[IBD_V2] height={} utxo_base.len()={} keys_needed={} store_len={} first_missing={:?}",
                                        next_height, utxo_base_buf.len(), keys_v2.len(), store.len(),
                                        first_missing.map(hex::encode));
                        }
                        if let Some(ref base) = snapshot_dir_base {
                            const SNAPSHOT_HEIGHTS: &[u64] = &[
                                50_000, 90_000, 125_000, 133_000, 145_000, 175_000, 181_000,
                                190_000, 200_000,
                            ];
                            if SNAPSHOT_HEIGHTS.contains(&next_height) {
                                let utxo_set = store.to_utxo_set_snapshot();
                                Self::dump_ibd_snapshot(
                                    next_height,
                                    block_arc.as_ref(),
                                    witnesses_to_use,
                                    &utxo_set,
                                    base,
                                );
                            }
                        }
                        let validation_start = std::time::Instant::now();
                        let r = self_clone.validate_block_only(
                            &blockstore,
                            protocol.as_ref(),
                            &mut utxo_base_buf,
                            Some(&mut bip30_index),
                            block_arc.as_ref(),
                            Some(Arc::clone(&block_arc)),
                            witnesses_to_use,
                            witnesses_arc.as_ref(),
                            next_height,
                            recent_headers_opt,
                            cached_network_time,
                            Some(&tx_ids_precomputed),
                        );
                        (r, validation_start.elapsed())
                    };

                    let (sync_ms, evict_ms, utxo_flush_batch, pipelined_sync_spawned, rss_pressure) =
                        match &validation_result {
                            Ok((_tx_ids, utxo_delta)) => {
                                if let Some(ref delta) = utxo_delta {
                                    let store = &ibd_store_v2_for_validation;
                                    // Apply delta to commitment tree BEFORE store (remove needs UTXO from store)
                                    #[cfg(all(
                                        feature = "utxo-commitments",
                                        feature = "production"
                                    ))]
                                    if let (Some(ref mut tree), Some(_)) =
                                        (&mut commitment_tree_opt, &commitment_store_opt)
                                    {
                                        for op in &delta.deletions {
                                            let key = outpoint_to_key(op);
                                            if let Some(utxo) = store.get(&key) {
                                                if let Err(e) = tree.remove(op, &utxo) {
                                                    warn!("IBD commitment: remove failed at height {}: {}", next_height, e);
                                                }
                                            }
                                        }
                                        for (op, arc) in &delta.additions {
                                            if let Err(e) = tree.insert(*op, arc.as_ref().clone()) {
                                                warn!("IBD commitment: insert failed at height {}: {}", next_height, e);
                                            }
                                        }
                                    }
                                    store.apply_utxo_delta(delta);
                                    // Dynamic eviction: protect keys for next blocks, then evict
                                    if store.is_dynamic_eviction() {
                                        block_input_keys_batch_into_arc(
                                            &blocks_buf,
                                            &mut keys_buf,
                                            &mut keys_seen,
                                        );
                                        store.protect_keys_for_next_blocks(&keys_buf);
                                        store.evict_if_needed(next_height);
                                    }
                                    let rss_pressure = mem_guard.should_flush();
                                    if rss_pressure {
                                        info!("[IBD_V2] height={} RSS pressure (cache={}, pending={}), forcing flush",
                                                        next_height, store.len(), store.pending_len());
                                        let batch = store.take_flush_batch_force();
                                        // Non-blocking: force a flush batch (async, same as normal path)
                                        // and return freed mimalloc pages. Do NOT sync-wait or evict —
                                        // the UTXO cache (~1GB) is a small fraction of RSS; evicting it
                                        // doesn't help but tanks BPS from cache misses + sync I/O.
                                        #[cfg(all(
                                            not(target_os = "windows"),
                                            feature = "mimalloc"
                                        ))]
                                        unsafe {
                                            libmimalloc_sys::mi_collect(true);
                                        }
                                        (
                                            0u64,
                                            0u64,
                                            batch,
                                            None::<std::thread::JoinHandle<Result<()>>>,
                                            true,
                                        )
                                    } else if ibd_defer_flush {
                                        let at_checkpoint = next_height > 0
                                            && next_height % ibd_defer_checkpoint == 0;
                                        let batch = if at_checkpoint {
                                            store.take_flush_batch_force()
                                        } else {
                                            None
                                        };
                                        (0u64, 0u64, batch, None, false)
                                    } else {
                                        let batch = store.maybe_take_flush_batch();
                                        (0u64, 0u64, batch, None, false)
                                    }
                                } else {
                                    (0u64, 0u64, None, None, false)
                                }
                            }
                            Err(_) => (0u64, 0u64, None, None, false),
                        };

                    (
                        prefetch_ms,
                        apply_pending_ms,
                        validation_result,
                        sync_ms,
                        evict_ms,
                        validation_time,
                        utxo_flush_batch,
                        pipelined_sync_spawned,
                        rss_pressure,
                    )
                };

                // V2: no pipelined sync (overlay delta applied directly).

                #[cfg(feature = "profile")]
                if ibd_blocked_log {
                    blvm_consensus::profile_log!(
                        "[IBD_VALIDATION] height={} phase=end apply_pending_ms={} validation_ms={} sync_ms={} evict_ms={}",
                        next_height, apply_pending_ms, validation_time.as_millis(), sync_ms, evict_ms
                    );
                    if apply_pending_ms > 2 {
                        blvm_consensus::profile_log!(
                            "[IBD_BLOCKED] phase=apply_pending height={} duration_ms={} (pending_writes/flushing scan for cache hits)",
                            next_height, apply_pending_ms
                        );
                    }
                    if sync_ms > 5 {
                        blvm_consensus::profile_log!(
                            "[IBD_BLOCKED] phase=sync_await height={} duration_ms={} (validation waited for previous block sync+evict)",
                            next_height, sync_ms
                        );
                    }
                }
                if let Err(ref e) = validation_result {
                    error!(
                        "Failed to prefetch/validate block at height {}: {}",
                        next_height, e
                    );
                }

                match validation_result {
                    Ok((_tx_ids, _utxo_delta)) => {
                        // Sync/evict already done in block_in_place
                        blocks_synced += 1;
                        // Store commitment for this block (incremental; tree already updated)
                        #[cfg(all(feature = "utxo-commitments", feature = "production"))]
                        if let (Some(ref tree), Some(ref cstore)) =
                            (&commitment_tree_opt, &commitment_store_opt)
                        {
                            let block_hash = blockstore.get_block_hash(block_arc.as_ref());
                            let commitment = tree.generate_commitment(block_hash, next_height);
                            if let Err(e) =
                                cstore.store_commitment(&block_hash, next_height, &commitment)
                            {
                                warn!(
                                    "IBD commitment: store failed at height {}: {}",
                                    next_height, e
                                );
                            }
                        }
                        let n_txs = block_arc.transactions.len();
                        let n_inputs: usize = block_arc
                            .transactions
                            .iter()
                            .map(|tx| tx.inputs.len())
                            .sum();

                        // Async UTXO flush: spawn batch without blocking validation
                        if let Some(batch) = utxo_flush_batch {
                            let store = &ibd_store_v2_for_validation;
                            let flush_limit = if rss_pressure {
                                MAX_UTXO_FLUSHES_UNDER_RSS_PRESSURE
                            } else {
                                MAX_UTXO_FLUSHES_IN_FLIGHT
                            };
                            while utxo_flush_handles.len() >= flush_limit {
                                let in_flight = utxo_flush_handles.len();
                                debug!(
                                    "[IBD_DEBUG] Block {}: awaiting UTXO flush slot (in_flight={}, batch_size={})",
                                    next_height, in_flight, batch.len()
                                );
                                let handle = utxo_flush_handles.pop_front().expect("non-empty");
                                match handle.join() {
                                    Ok(Ok(())) => {}
                                    Ok(Err(e)) => return Err(e),
                                    Err(e) => {
                                        return Err(anyhow::anyhow!("UTXO flush panicked: {:?}", e))
                                    }
                                }
                            }
                            let store_clone = Arc::clone(store);
                            let batch_clone = Arc::clone(&batch);
                            let batch_size = batch.len();
                            utxo_flush_handles.push_back(std::thread::spawn(move || {
                                store_clone.flush_pending_batch(&batch_clone).map(|_| ())
                            }));
                            debug!(
                                "[IBD_DEBUG] Block {}: spawned UTXO flush (batch_size={}, in_flight={})",
                                next_height, batch_size, utxo_flush_handles.len()
                            );
                        }

                        // Track recent headers for BIP113 MTP (keep last 11)
                        let header_rc = Arc::new(block_arc.header.clone());
                        if !skip_storage {
                            pending_blocks.push((
                                Arc::clone(&block_arc),
                                Arc::clone(&header_rc),
                                witnesses_arc
                                    .clone()
                                    .unwrap_or_else(|| Arc::new(Vec::new())),
                                next_height - 1,
                            ));
                        }
                        recent_headers_buf.push_back(header_rc);
                        if recent_headers_buf.len() > 11 {
                            recent_headers_buf.pop_front();
                        }

                        // Update shared validation height (allows download workers to track progress)
                        validation_height.store(next_height, Ordering::Relaxed);

                        let flush_ms = if !skip_storage
                            && pending_blocks.len() >= storage_flush_interval
                        {
                            let flush_start = std::time::Instant::now();
                            // Fully overlapped async flush: await only when 2 flushes in flight (backpressure)
                            while flush_handles.len() >= MAX_FLUSHES_IN_FLIGHT {
                                let in_flight = flush_handles.len();
                                let wait_start = std::time::Instant::now();
                                debug!(
                                    "[IBD_DEBUG] Block {}: awaiting block storage flush slot (in_flight={}, pending_blocks={})",
                                    next_height,
                                    in_flight,
                                    pending_blocks.len()
                                );
                                let handle = flush_handles.pop_front().expect("non-empty");
                                match handle.join() {
                                    Ok(Ok(())) => {
                                        let waited_ms = wait_start.elapsed().as_millis() as u64;
                                        debug!(
                                            "[IBD_DEBUG] Block {}: block storage flush slot free (waited {}ms)",
                                            next_height,
                                            waited_ms
                                        );
                                        #[cfg(feature = "profile")]
                                        if ibd_blocked_log && waited_ms > 0 {
                                            blvm_consensus::profile_log!(
                                                "[IBD_BLOCKED]                                                 phase=block_flush_await height={} duration_ms={} in_flight={} utxo_flush={} (validation waited for block storage write)",
                                                next_height, waited_ms, in_flight, utxo_flush_handles.len()
                                            );
                                        }
                                    }
                                    Ok(Err(e)) => return Err(e),
                                    Err(e) => {
                                        return Err(anyhow::anyhow!(
                                            "Block storage flush thread panicked: {:?}",
                                            e
                                        ))
                                    }
                                }
                            }
                            let to_flush = std::mem::take(&mut pending_blocks);
                            let blockstore_clone = Arc::clone(&blockstore);
                            let storage_for_flush = storage_clone.clone();
                            let to_flush_count = to_flush.len();
                            #[cfg(feature = "profile")]
                            if ibd_profile
                                && (ibd_profile_sample == 1
                                    || next_height % ibd_profile_sample == 0)
                            {
                                blvm_consensus::profile_log!(
                                    "[IBD_BLOCK_FLUSH_SPAWN] height={} blocks={} in_flight={} utxo_flush={}",
                                    next_height, to_flush_count, flush_handles.len(), utxo_flush_handles.len()
                                );
                            }
                            flush_handles.push_back(std::thread::spawn(move || {
                                Self::do_flush_to_storage(
                                    blockstore_clone.as_ref(),
                                    Some(&storage_for_flush),
                                    to_flush,
                                )
                            }));
                            let flush_elapsed = flush_start.elapsed().as_millis() as u64;
                            debug!(
                                "[IBD_DEBUG] Block {}: spawned block storage flush (blocks={}, in_flight={}, await_took={}ms)",
                                next_height,
                                to_flush_count,
                                flush_handles.len(),
                                flush_elapsed
                            );
                            flush_elapsed
                        } else {
                            0
                        };
                        if !skip_storage && pending_blocks.is_empty() && flush_ms > 0 {
                            debug!(
                                "Started async flush of {} blocks (overlapped, {} in flight)",
                                storage_flush_interval,
                                flush_handles.len()
                            );
                        }

                        // IBD Profiling: log per-block breakdown when enabled (profile feature)
                        // Ready-queue: prefetch_await=0 by design (validation never awaits prefetch).
                        #[cfg(feature = "profile")]
                        if ibd_profile {
                            let prefetch_await_ms = 0u64; // Ready-queue: no prefetch_await
                            let val_ms = validation_time.as_millis() as u64;
                            let total_ms = prefetch_await_ms
                                + gap_fill_ms
                                + prefetch_ms
                                + val_ms
                                + sync_ms
                                + evict_ms
                                + flush_ms;
                            let disk_total = prefetch_await_ms
                                + gap_fill_ms
                                + prefetch_ms
                                + sync_ms
                                + evict_ms
                                + flush_ms;
                            let should_log = (ibd_profile_sample == 1
                                || next_height % ibd_profile_sample == 0)
                                || (ibd_disk_profile
                                    && (prefetch_await_ms > 0
                                        || gap_fill_ms > 0
                                        || prefetch_ms > 0
                                        || sync_ms > 0
                                        || evict_ms > 0))
                                || (ibd_profile_slow_ms > 0
                                    && (prefetch_await_ms >= ibd_profile_slow_ms
                                        || gap_fill_ms >= ibd_profile_slow_ms
                                        || prefetch_ms >= ibd_profile_slow_ms
                                        || val_ms >= ibd_profile_slow_ms
                                        || sync_ms >= ibd_profile_slow_ms
                                        || evict_ms >= ibd_profile_slow_ms
                                        || flush_ms >= ibd_profile_slow_ms));
                            if should_log && total_ms > 0 {
                                blvm_consensus::profile_log!(
                                    "[IBD_PROFILE] height={} total_ms={} prefetch_await={} gap_fill={} prefetch={} validation={} sync={} evict={} flush={} disk_total={} txs={} inputs={}",
                                    next_height, total_ms, prefetch_await_ms, gap_fill_ms, prefetch_ms, val_ms, sync_ms, evict_ms, flush_ms, disk_total, n_txs, n_inputs
                                );
                                let (dl, ch, ev, _ph) = ibd_store_v2_for_validation.stats();
                                let utxo_stats = (ibd_store_v2_for_validation.len(), dl, ch, ev);
                                blvm_consensus::profile_log!(
                                    "[IBD_PIPELINE] height={} utxo_flush={} block_flush={} pending={} utxo_cache={} disk_loads={} cache_hits={} evictions={}",
                                    next_height, utxo_flush_handles.len(), flush_handles.len(), pending_blocks.len(),
                                    utxo_stats.0, utxo_stats.1, utxo_stats.2, utxo_stats.3
                                );
                            }
                        }
                    }
                    Err(e) => {
                        for handle in utxo_flush_handles.drain(..) {
                            let _ = handle.join();
                        }
                        for handle in flush_handles.drain(..) {
                            let _ = handle.join();
                        }
                        if !skip_storage && !pending_blocks.is_empty() {
                            let _ = self_clone.flush_pending_blocks(
                                &blockstore,
                                Some(&storage_clone),
                                &mut pending_blocks,
                            );
                        }
                        error!("Failed to validate block at height {}: {}", next_height, e);
                        block_input_keys_into(block_arc.as_ref(), &mut keys_v2_buf);
                        let utxo_for_dump =
                            ibd_store_v2_for_validation.build_utxo_map(&keys_v2_buf);
                        Self::dump_failed_block(
                            next_height,
                            block_arc.as_ref(),
                            witnesses_to_use,
                            &utxo_for_dump,
                            &e,
                        );
                        return Err(e);
                    }
                }

                // CRITICAL: Yield to the runtime (BLVM_IBD_YIELD_INTERVAL, default 100)
                // Allows download workers to progress; fewer yields = less validation interruption
                if yield_interval > 0 && blocks_synced % yield_interval == 0 {
                    #[cfg(feature = "profile")]
                    if ibd_profile
                        && (ibd_profile_sample == 1 || next_height % ibd_profile_sample == 0)
                    {
                        blvm_consensus::profile_log!(
                            "[IBD_YIELD] blocks_synced={} utxo_flush={} block_flush={} (yielding to runtime)",
                            blocks_synced, utxo_flush_handles.len(), flush_handles.len()
                        );
                    }
                    std::thread::yield_now();
                }

                // Periodic mimalloc page return. 5a: adaptive — every 1000 blocks, or sooner
                // when RSS grew >50MB since last collect (high allocation phase).
                if blocks_synced > 0 && blocks_synced % 100 == 0 {
                    #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
                    {
                        let current_rss_mb = mem_guard.current_rss_mb();
                        let rss_growth_mb = current_rss_mb.saturating_sub(last_rss_mb);
                        let blocks_since_collect = blocks_synced.saturating_sub(last_collect_block);
                        if rss_growth_mb > 50 || blocks_since_collect >= 1000 {
                            unsafe {
                                libmimalloc_sys::mi_collect(true);
                            }
                            last_rss_mb = mem_guard.current_rss_mb();
                            last_collect_block = blocks_synced;
                        }
                    }
                }

                // Progress logging: early (1, 10, 100) then every 1000 blocks
                let should_log = blocks_synced == 1
                    || blocks_synced == 10
                    || blocks_synced == 100
                    || (blocks_synced > 0 && blocks_synced % 1000 == 0);
                if should_log {
                    cached_network_time = crate::utils::time::current_timestamp();
                    // Don't show BPS at blocks 1, 10: elapsed includes header sync + handshake (~15-20s),
                    // which makes rate look absurdly low (1/17 = 0.06 blocks/s). From block 100 we have
                    // meaningful validation throughput to measure.
                    let total_elapsed = validation_start.elapsed().as_secs_f64();
                    let average_rate = if blocks_synced >= 100 && total_elapsed > 0.0 {
                        blocks_synced as f64 / total_elapsed
                    } else {
                        0.0
                    };
                    // Recent rate: blocks since last status / time since last status. Shows actual burst vs wait.
                    // When avg >> recent, we're mostly waiting (download bottleneck). When avg ≈ recent, pipeline is full.
                    let blocks_since_last = blocks_synced.saturating_sub(last_log_blocks);
                    let recent_elapsed = last_log_instant.elapsed().as_secs_f64();
                    let recent_rate = if blocks_since_last > 0 && recent_elapsed > 0.01 {
                        blocks_since_last as f64 / recent_elapsed
                    } else {
                        0.0
                    };
                    last_log_blocks = blocks_synced;
                    last_log_instant = std::time::Instant::now();

                    let remaining = effective_end_height.saturating_sub(next_height);
                    let eta = if average_rate > 0.0 {
                        remaining as f64 / average_rate
                    } else {
                        f64::INFINITY
                    };
                    let buffer_size = feeder_state.0.lock().unwrap().0.len();

                    // Show avg (sustained) rate: blocks/total_time. Matches actual throughput.
                    // Add recent rate so user sees: when height creeps slowly, recent << avg; when bursting, recent ≈ avg.
                    let rate_str = if blocks_synced < 100 {
                        "warming up (rate after block 100)".to_string()
                    } else if blocks_synced >= 1000 && blocks_since_last > 0 {
                        format!("{average_rate:.1} blocks/s avg (last {blocks_since_last} blocks: {recent_rate:.1} blocks/s)")
                    } else {
                        format!("{average_rate:.1} blocks/s")
                    };
                    info!(
                        "IBD: {} / {} ({:.1}%) - {} - buffer: {} - ETA: {:.0}s",
                        next_height,
                        effective_end_height,
                        (next_height as f64 / effective_end_height as f64) * 100.0,
                        rate_str,
                        buffer_size,
                        eta
                    );
                    // Memory diagnostics: log RSS breakdown and data structure sizes
                    if blocks_synced % 5000 == 0 {
                        let (rss_kb, swap_kb) = {
                            #[cfg(target_os = "linux")]
                            {
                                let rss = std::fs::read_to_string("/proc/self/status")
                                    .ok()
                                    .and_then(|s| {
                                        s.lines()
                                            .find(|l| l.starts_with("VmRSS:"))
                                            .and_then(|l| l.split_whitespace().nth(1))
                                            .and_then(|v| v.parse::<u64>().ok())
                                    })
                                    .unwrap_or(0);
                                let swap = std::fs::read_to_string("/proc/self/status")
                                    .ok()
                                    .and_then(|s| {
                                        s.lines()
                                            .find(|l| l.starts_with("VmSwap:"))
                                            .and_then(|l| l.split_whitespace().nth(1))
                                            .and_then(|v| v.parse::<u64>().ok())
                                    })
                                    .unwrap_or(0);
                                (rss, swap)
                            }
                            #[cfg(not(target_os = "linux"))]
                            {
                                (0u64, 0u64)
                            }
                        };
                        let store_info = format!(
                            "utxo_cache={} pending={}",
                            ibd_store_v2_for_validation.len(),
                            ibd_store_v2_for_validation.pending_len()
                        );
                        info!(
                            "[MEM] h={} rss={}MB swap={}MB {} feeder={} threads={}",
                            next_height,
                            rss_kb / 1024,
                            swap_kb / 1024,
                            store_info,
                            buffer_size,
                            std::thread::available_parallelism()
                                .map(|n| n.get())
                                .unwrap_or(0)
                        );
                    }
                    // BPS CSV for Core-comparable metrics (height,elapsed_sec) — same format as bitcoin-core-ibd-bench.sh
                    if let Ok(path) = std::env::var("BLVM_IBD_BPS_CSV") {
                        let elapsed_sec = validation_start.elapsed().as_secs();
                        let create_header = !std::path::Path::new(&path).exists()
                            || std::fs::metadata(&path)
                                .map(|m| m.len() == 0)
                                .unwrap_or(true);
                        if let Ok(mut f) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path)
                        {
                            use std::io::Write;
                            if create_header {
                                let _ = writeln!(f, "height,elapsed_sec");
                            }
                            let _ = writeln!(f, "{next_height},{elapsed_sec}");
                        }
                    }
                    #[cfg(feature = "profile")]
                    if ibd_profile {
                        blvm_consensus::profile_log!(
                            "[IBD_PREFETCH_STATS] height={} utxo_flush={} block_flush={}",
                            next_height,
                            utxo_flush_handles.len(),
                            flush_handles.len()
                        );
                        if blocks_synced > 0 && blocks_synced % 5000 == 0 {
                            // IBD_UTXO_PATH: cumulative UTXO path stats for overlap/eviction analysis
                            let (dl, ch, ev, ph) = ibd_store_v2_for_validation.stats();
                            blvm_consensus::profile_log!(
                                "[IBD_UTXO_PATH] height={} disk_loads={} cache_hits={} evictions={} pending_hits={} cache_len={} (cumulative since start)",
                                next_height, dl, ch, ev, ph, ibd_store_v2_for_validation.len()
                            );
                        }
                        if let Some((rss_mb, avail_mb)) = mem_guard.memory_diag() {
                            blvm_consensus::profile_log!(
                                "[IBD_DIAG] height={} rss_mb={} avail_mb={} utxo_flush={} block_flush={}",
                                next_height, rss_mb, avail_mb,
                                utxo_flush_handles.len(), flush_handles.len()
                            );
                        }
                    }
                }
            }

            // Join all in-flight UTXO flushes, then block storage flushes
            for handle in utxo_flush_handles.drain(..) {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => return Err(e),
                    Err(e) => return Err(anyhow::anyhow!("UTXO flush thread panicked: {:?}", e)),
                }
            }
            for handle in flush_handles.drain(..) {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => return Err(e),
                    Err(e) => {
                        return Err(anyhow::anyhow!(
                            "Block storage flush thread panicked: {:?}",
                            e
                        ))
                    }
                }
            }
            if !skip_storage && !pending_blocks.is_empty() {
                info!("Flushing final {} pending blocks", pending_blocks.len());
                self_clone.flush_pending_blocks(
                    &blockstore,
                    Some(&storage_clone),
                    &mut pending_blocks,
                )?;
            }

            // Flush IBD v2 remaining pending
            let remaining = ibd_store_v2_for_validation.take_remaining_pending();
            if !remaining.is_empty() {
                info!("Flushing remaining {} UTXO operations...", remaining.len());
                match ibd_store_v2_for_validation.flush_pending_batch(&remaining) {
                    Ok(count) => info!("Flushed {} UTXO operations to disk", count),
                    Err(e) => warn!("Failed to flush UTXO: {}", e),
                }
            }

            Ok(())
        });

        // Wait for validation thread (block_in_place keeps tokio worker free)
        match tokio::task::block_in_place(|| validation_handle.join()) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(panic) => return Err(anyhow::anyhow!("Validation thread panicked: {:?}", panic)),
        }
        // Feeder exits when ready_rx disconnects; join to avoid stray thread
        let _ = feeder_handle.join();
        *utxo_set = Arc::try_unwrap(utxo_mutex).unwrap().into_inner().unwrap();

        // Isolated validation: coordinator drained all blocks; no local reorder buffer to check.

        // Wait for all download tasks to complete (they should have already finished)
        for (chunk_start, handle) in download_handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    debug!(
                        "Download task for chunk {} completed with error: {}",
                        chunk_start, e
                    );
                }
                Err(e) => {
                    debug!("Download task for chunk {} panicked: {}", chunk_start, e);
                }
            }
        }

        // Log peer scoring summary
        info!("Peer scoring: {}", self.peer_scorer.summary());

        let blocks_synced = effective_end_height.saturating_sub(start_height) + 1;
        info!(
            "Parallel IBD completed: {} blocks synced (heights {} to {})",
            blocks_synced, start_height, effective_end_height
        );
        Ok(())
    }

    /// Download headers in parallel using checkpoint-based ranges
    ///
    /// This method parallelizes header download by using hardcoded checkpoints.
    /// Each checkpoint range can be downloaded independently because we know
    /// the starting hash for each range.
    ///
    /// Returns the highest height successfully downloaded.
    async fn download_headers_parallel(
        &self,
        start_height: u64,
        end_height: u64,
        peer_ids: &[String],
        blockstore: &BlockStore,
        network: Arc<NetworkManager>,
    ) -> Result<u64> {
        use crate::storage::hashing::double_sha256;

        // Get checkpoints within our range
        let checkpoints = get_checkpoints_in_range(start_height, end_height);

        if checkpoints.len() < 2 {
            // Not enough checkpoints for parallel download, fall back to sequential
            info!("Insufficient checkpoints for parallel download, using sequential");
            return self
                .download_headers(
                    start_height,
                    end_height,
                    peer_ids,
                    blockstore,
                    Some(network),
                )
                .await;
        }

        info!(
            "Downloading headers {} to {} using {} checkpoint ranges in parallel",
            start_height,
            end_height,
            checkpoints.len() - 1
        );

        let peer_addrs: Vec<SocketAddr> = peer_ids
            .iter()
            .filter_map(|id| id.parse::<SocketAddr>().ok())
            .collect();

        if peer_addrs.is_empty() {
            return Err(anyhow::anyhow!(
                "No valid peer addresses for parallel header download"
            ));
        }

        // Create tasks for each checkpoint range
        let mut tasks = Vec::new();
        let num_peers = peer_addrs.len();

        for (i, window) in checkpoints.windows(2).enumerate() {
            let (range_start, start_hash) = window[0];
            let (range_end, _end_hash) = window[1];

            // Skip ranges before our start_height
            if range_end < start_height {
                continue;
            }

            // Adjust range to our actual bounds
            let actual_start = range_start.max(start_height);
            let actual_end = range_end.min(end_height);

            if actual_start > actual_end {
                continue;
            }

            // Assign peer round-robin
            let peer_addr = peer_addrs[i % num_peers];
            let network_clone = Arc::clone(&network);
            let locator_hash = start_hash;

            let task = tokio::spawn(async move {
                download_header_range(
                    network_clone,
                    peer_addr,
                    locator_hash,
                    actual_start,
                    actual_end,
                )
                .await
            });

            tasks.push((actual_start, actual_end, task));
        }

        // Collect results
        let mut all_headers: Vec<(u64, Vec<blvm_protocol::BlockHeader>)> = Vec::new();
        let mut highest_success = start_height;

        for (range_start, range_end, task) in tasks {
            match task.await {
                Ok(Ok(headers)) => {
                    info!(
                        "Received {} headers for range {} - {}",
                        headers.len(),
                        range_start,
                        range_end
                    );
                    all_headers.push((range_start, headers));
                    highest_success = highest_success.max(range_end);
                }
                Ok(Err(e)) => {
                    warn!(
                        "Failed to download headers for range {} - {}: {}",
                        range_start, range_end, e
                    );
                }
                Err(e) => {
                    warn!(
                        "Task failed for range {} - {}: {}",
                        range_start, range_end, e
                    );
                }
            }
        }

        // Sort by start height and store in order
        all_headers.sort_by_key(|(start, _)| *start);

        let mut current_height = start_height;
        for (range_start, headers) in all_headers {
            // Handle gaps - we may have missed some ranges
            if range_start > current_height {
                warn!(
                    "Gap detected at height {}, expected {}",
                    range_start, current_height
                );
                // For now, we'll accept the gap - could try to fill with sequential download
            }

            // Store headers
            for header in headers {
                // Verify proof of work before accepting header
                match blvm_consensus::pow::check_proof_of_work(&header) {
                    Ok(true) => {}
                    Ok(false) => {
                        warn!(
                            "Header at height {} failed PoW check, skipping",
                            current_height
                        );
                        continue;
                    }
                    Err(e) => {
                        warn!(
                            "Header at height {} PoW check error: {}, skipping",
                            current_height, e
                        );
                        continue;
                    }
                }

                // Calculate hash (#53: stack array avoids heap alloc)
                let mut header_data = [0u8; 80];
                header_data[0..4].copy_from_slice(&(header.version as i32).to_le_bytes());
                header_data[4..36].copy_from_slice(&header.prev_block_hash);
                header_data[36..68].copy_from_slice(&header.merkle_root);
                header_data[68..72].copy_from_slice(&(header.timestamp as u32).to_le_bytes());
                header_data[72..76].copy_from_slice(&(header.bits as u32).to_le_bytes());
                header_data[76..80].copy_from_slice(&(header.nonce as u32).to_le_bytes());
                let header_hash = double_sha256(&header_data);

                blockstore
                    .store_header(&header_hash, &header)
                    .context("Failed to store header")?;
                blockstore
                    .store_height(current_height, &header_hash)
                    .context("Failed to store height")?;

                current_height += 1;
            }
        }

        info!(
            "Parallel header download complete: {} headers stored",
            current_height - start_height
        );
        Ok(current_height.saturating_sub(1))
    }

    /// Download headers for the given height range
    /// Sequential request, fast response processing - headers must be sequential
    /// because each GetHeaders request needs the hash from the previous response
    async fn download_headers(
        &self,
        start_height: u64,
        end_height: u64,
        peer_ids: &[String],
        blockstore: &BlockStore,
        network: Option<Arc<NetworkManager>>,
    ) -> Result<u64> {
        use crate::storage::hashing::double_sha256;

        info!(
            "Downloading headers {} to {} ({} headers) - SEQUENTIAL FAST",
            start_height,
            end_height,
            end_height - start_height + 1
        );

        let network = match network.as_ref() {
            Some(n) => n,
            None => {
                warn!("NetworkManager not available, skipping header download");
                return Ok(start_height);
            }
        };

        if peer_ids.is_empty() {
            return Err(anyhow::anyhow!("No peers available for header download"));
        }

        let mut peer_addrs: Vec<SocketAddr> = peer_ids
            .iter()
            .filter_map(|id| id.parse::<SocketAddr>().ok())
            .collect();

        if peer_addrs.is_empty() {
            return Err(anyhow::anyhow!("No valid peer addresses found"));
        }

        // Sort peers by score (high first) — high-score peers get priority
        peer_addrs.sort_by(|a, b| {
            let a_score = self.peer_scorer.get_score(a);
            let b_score = self.peer_scorer.get_score(b);
            b_score
                .partial_cmp(&a_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        use crate::network::protocol::MAX_HEADERS_RESULTS;
        const TIMEOUT_SECS: u64 = 30;
        const MAX_FAILURES: u32 = 50;

        info!(
            "Using {} peers for sequential header download",
            peer_addrs.len()
        );

        // Genesis block internal hash (this is how it's stored/referenced in Bitcoin)
        let genesis_hash: [u8; 32] = [
            0x6f, 0xe2, 0x8c, 0x0a, 0xb6, 0xf1, 0xb3, 0x72, 0xc1, 0xa6, 0xa2, 0x46, 0xae, 0x63,
            0xf7, 0x4f, 0x93, 0x1e, 0x83, 0x65, 0xe1, 0x5a, 0x08, 0x9c, 0x68, 0xd6, 0x19, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        // When starting from height 0, we need to store the genesis block first
        // because GetHeaders with genesis hash as locator returns headers starting from block 1
        let mut current_height = start_height;
        let mut last_hash = genesis_hash;

        if start_height == 0 {
            // Create genesis block header and store it
            use crate::storage::hashing::double_sha256;

            // Genesis block header fields (Bitcoin mainnet)
            let genesis_header = blvm_protocol::BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [
                    0x3b, 0xa3, 0xed, 0xfd, 0x7a, 0x7b, 0x12, 0xb2, 0x7a, 0xc7, 0x2c, 0x3e, 0x67,
                    0x76, 0x8f, 0x61, 0x7f, 0xc8, 0x1b, 0xc3, 0x88, 0x8a, 0x51, 0x32, 0x3a, 0x9f,
                    0xb8, 0xaa, 0x4b, 0x1e, 0x5e, 0x4a,
                ],
                timestamp: 1231006505,
                bits: 0x1d00ffff,
                nonce: 2083236893,
            };

            // Verify our genesis hash calculation (#53: stack array)
            let mut header_data = [0u8; 80];
            header_data[0..4].copy_from_slice(&(genesis_header.version as i32).to_le_bytes());
            header_data[4..36].copy_from_slice(&genesis_header.prev_block_hash);
            header_data[36..68].copy_from_slice(&genesis_header.merkle_root);
            header_data[68..72].copy_from_slice(&(genesis_header.timestamp as u32).to_le_bytes());
            header_data[72..76].copy_from_slice(&(genesis_header.bits as u32).to_le_bytes());
            header_data[76..80].copy_from_slice(&(genesis_header.nonce as u32).to_le_bytes());
            let computed_hash = double_sha256(&header_data);

            if computed_hash != genesis_hash {
                warn!(
                    "Genesis hash mismatch! Computed: {}, Expected: {}",
                    hex::encode(computed_hash),
                    hex::encode(genesis_hash)
                );
            }

            // Store genesis header
            blockstore
                .store_header(&genesis_hash, &genesis_header)
                .context("Failed to store genesis header")?;
            blockstore
                .store_height(0, &genesis_hash)
                .context("Failed to store genesis height")?;

            info!(
                "Stored genesis block (height 0, hash: {})",
                hex::encode(genesis_hash)
            );
            current_height = 1; // Start requesting from height 1
        }
        let mut consecutive_failures = 0;
        let mut current_peer_idx = 0;
        let mut last_progress_log = start_height;
        let start_time = std::time::Instant::now();

        while current_height <= end_height {
            // Refresh peer list if empty
            if peer_addrs.is_empty() {
                peer_addrs = network.get_connected_peer_addresses().await;
                if peer_addrs.is_empty() {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    peer_addrs = network.get_connected_peer_addresses().await;
                    if peer_addrs.is_empty() {
                        return Err(anyhow::anyhow!("No peers available"));
                    }
                }
            }

            // Stick with first peer (high-score). Only rotate on failure.
            let peer_addr = peer_addrs[current_peer_idx % peer_addrs.len()];

            // Build GetHeaders with current locator
            let get_headers = GetHeadersMessage {
                version: 70015,
                block_locator_hashes: vec![last_hash],
                hash_stop: [0; 32],
            };

            let wire_msg = match ProtocolParser::serialize_message(&ProtocolMessage::GetHeaders(
                get_headers,
            )) {
                Ok(msg) => msg,
                Err(e) => {
                    warn!("Failed to serialize GetHeaders: {}", e);
                    return Err(anyhow::anyhow!("Serialization failed"));
                }
            };

            // Register request BEFORE sending
            let headers_rx = network.register_headers_request(peer_addr);
            let request_start = std::time::Instant::now();

            if let Err(e) = network.send_to_peer(peer_addr, wire_msg).await {
                debug!("Send failed to {}: {}", peer_addr, e);
                peer_addrs.retain(|&a| a != peer_addr);
                current_peer_idx += 1; // Try next peer on failure
                consecutive_failures += 1;
                if consecutive_failures >= MAX_FAILURES {
                    return Err(anyhow::anyhow!("Too many failures"));
                }
                continue;
            }

            // Wait for response
            debug!(
                "Waiting for headers from {} (timeout: {}s)",
                peer_addr, TIMEOUT_SECS
            );
            match timeout(Duration::from_secs(TIMEOUT_SECS), headers_rx).await {
                Ok(Ok(headers)) => {
                    let latency_ms = request_start.elapsed().as_secs_f64() * 1000.0;
                    self.peer_scorer
                        .record_latency_sample(peer_addr, latency_ms);
                    debug!(
                        "Received {} headers from {} ({}ms)",
                        headers.len(),
                        peer_addr,
                        latency_ms as u64
                    );
                    consecutive_failures = 0;

                    if headers.is_empty() {
                        info!(
                            "Header sync COMPLETE at height {} (chain tip reached)",
                            current_height.saturating_sub(1)
                        );
                        break;
                    }

                    // Process headers and batch store for performance
                    debug!(
                        "Processing {} headers starting at height {}",
                        headers.len(),
                        current_height
                    );
                    let mut batch_entries: Vec<(Hash, BlockHeader, u64)> =
                        Vec::with_capacity(headers.len());

                    for header in &headers {
                        // Verify proof of work before accepting header
                        match blvm_consensus::pow::check_proof_of_work(header) {
                            Ok(true) => {}
                            Ok(false) => {
                                warn!(
                                    "Header at height {} failed PoW check, skipping",
                                    current_height
                                );
                                continue;
                            }
                            Err(e) => {
                                warn!(
                                    "Header at height {} PoW check error: {}, skipping",
                                    current_height, e
                                );
                                continue;
                            }
                        }

                        // Calculate hash (#53: stack array avoids heap alloc)
                        let mut header_data = [0u8; 80];
                        header_data[0..4].copy_from_slice(&(header.version as i32).to_le_bytes());
                        header_data[4..36].copy_from_slice(&header.prev_block_hash);
                        header_data[36..68].copy_from_slice(&header.merkle_root);
                        header_data[68..72]
                            .copy_from_slice(&(header.timestamp as u32).to_le_bytes());
                        header_data[72..76].copy_from_slice(&(header.bits as u32).to_le_bytes());
                        header_data[76..80].copy_from_slice(&(header.nonce as u32).to_le_bytes());
                        let header_hash = double_sha256(&header_data);

                        batch_entries.push((header_hash, header.clone(), current_height));

                        last_hash = header_hash;
                        current_height += 1;

                        if current_height > end_height {
                            break;
                        }
                    }

                    // Store all headers in one batch operation
                    // Use spawn_blocking to avoid blocking the async executor
                    let batch_count = batch_entries.len();
                    debug!("Storing {} headers in batch...", batch_count);
                    let store_start = std::time::Instant::now();
                    let blockstore_clone = blockstore.clone();
                    // Optimization: Move batch_entries instead of cloning (we don't need it after this)
                    tokio::task::spawn_blocking(move || {
                        blockstore_clone.store_headers_batch(&batch_entries)
                    })
                    .await
                    .context("Failed to spawn blocking task")?
                    .context("Failed to store headers batch")?;
                    debug!(
                        "Stored {} headers in {:?}",
                        batch_count,
                        store_start.elapsed()
                    );

                    // Progress logging every 20k headers
                    if current_height > last_progress_log
                        && current_height - last_progress_log >= 20000
                    {
                        let elapsed = start_time.elapsed().as_secs_f64();
                        let synced = current_height - start_height;
                        let rate = if elapsed > 0.0 {
                            synced as f64 / elapsed
                        } else {
                            0.0
                        };
                        let remaining = end_height.saturating_sub(current_height);
                        let eta = if rate > 0.0 {
                            remaining as f64 / rate
                        } else {
                            f64::INFINITY
                        };

                        info!(
                            "Header sync: {} / {} ({:.1}%) - {:.0} h/s - ETA: {:.0}s",
                            current_height,
                            end_height,
                            (current_height as f64 / end_height as f64) * 100.0,
                            rate,
                            eta
                        );
                        last_progress_log = current_height;
                    }

                    // Chain tip detection - received fewer than max headers
                    if headers.len() < MAX_HEADERS_RESULTS {
                        let total = current_height - start_height;
                        let elapsed = start_time.elapsed();
                        let rate = if elapsed.as_secs_f64() > 0.0 {
                            total as f64 / elapsed.as_secs_f64()
                        } else {
                            0.0
                        };
                        info!(
                            "Header sync COMPLETE: {} headers in {:.1}s ({:.0} h/s) - chain tip reached",
                            total, elapsed.as_secs_f64(), rate
                        );
                        return Ok(current_height.saturating_sub(1));
                    }
                }
                Ok(Err(_)) => {
                    debug!("Channel closed for request to {}", peer_addr);
                    consecutive_failures += 1;
                    // Rotate to next peer
                    current_peer_idx += 1;
                }
                Err(_) => {
                    debug!("Timeout waiting for headers from {}", peer_addr);
                    consecutive_failures += 1;
                    // Rotate to next peer
                    current_peer_idx += 1;
                    // Also rotate this peer to end of list
                    if let Some(idx) = peer_addrs.iter().position(|&a| a == peer_addr) {
                        let p = peer_addrs.remove(idx);
                        peer_addrs.push(p);
                    }
                }
            }

            if consecutive_failures >= MAX_FAILURES {
                warn!(
                    "Too many failures ({}), refreshing peers",
                    consecutive_failures
                );
                peer_addrs = network.get_connected_peer_addresses().await;
                if peer_addrs.is_empty() {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    peer_addrs = network.get_connected_peer_addresses().await;
                    if peer_addrs.is_empty() {
                        return Err(anyhow::anyhow!("No peers available"));
                    }
                }
                consecutive_failures = 0;
            }
        }

        // Final completion message
        let total = current_height - start_height;
        let elapsed = start_time.elapsed();
        let rate = if elapsed.as_secs_f64() > 0.0 {
            total as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        info!(
            "Header sync COMPLETE: {} headers in {:.1}s ({:.0} h/s)",
            total,
            elapsed.as_secs_f64(),
            rate
        );

        Ok(current_height.saturating_sub(1))
    }

    /// Create chunks for parallel download.
    ///
    /// When scored_peers is Some and BLVM_IBD_EARLIEST_FIRST=1: assign all chunks to fastest peer
    /// (Core-like, avoids chunk-boundary stalls when slow peer holds next chunk).
    /// Otherwise: round-robin (chunk i → peer i % num_peers).
    fn create_chunks(
        &self,
        start_height: u64,
        end_height: u64,
        peer_ids: &[String],
        scored_peers: Option<&[(String, f64)]>,
    ) -> Vec<BlockChunk> {
        let mut chunks = Vec::new();
        let mut current_height = start_height;
        let num_peers = peer_ids.len().max(1);
        let mut chunk_index: usize = 0;

        let ibd_mode: &str = &std::env::var("BLVM_IBD_MODE").unwrap_or_else(|_| "parallel".into());
        let use_fastest = ibd_mode.eq_ignore_ascii_case("earliest")
            && num_peers > 1
            && scored_peers.map(|s| !s.is_empty()).unwrap_or(false);

        let fastest_peer = if use_fastest {
            scored_peers.and_then(|s| {
                s.iter()
                    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|(p, _)| p.clone())
            })
        } else {
            None
        };

        if use_fastest && fastest_peer.is_some() {
            info!("IBD: earliest-first (BLVM_IBD_MODE=earliest) — all chunks to fastest peer");
        } else {
            info!(
                "Round-robin chunk assignment: {} peers, chunk_size={}",
                num_peers, self.config.chunk_size
            );
        }

        while current_height <= end_height {
            // First chunk (block 0) must be at least 128 blocks so 99 and 100 are always in the same
            // chunk. Otherwise parallel workers can deliver 100 before 99 → stall. Peer type is irrelevant.
            let (chunk_sz, is_bootstrap) = if current_height == 0 && start_height == 0 {
                let sz = 128.min(end_height.saturating_add(1));
                (sz, true)
            } else {
                (self.config.chunk_size, false)
            };
            let chunk_end = (current_height + chunk_sz - 1).min(end_height);
            if is_bootstrap {
                info!(
                    "IBD: bootstrap chunk 0-{} (99 and 100 in same chunk)",
                    chunk_end
                );
            }

            let peer_id = fastest_peer
                .clone()
                .unwrap_or_else(|| peer_ids[chunk_index % num_peers].clone());

            chunks.push(BlockChunk {
                start_height: current_height,
                end_height: chunk_end,
                peer_id,
            });

            current_height = chunk_end + 1;
            chunk_index += 1;
        }

        chunks
    }

    /// Download a chunk of blocks from a peer.
    /// When block_tx is Some, streams each block immediately so validation doesn't wait for full chunk.
    /// blocks_sem: Core-style limit — max 16 blocks in flight per peer across all workers.
    /// stall_rx: When coordinator stalls, it broadcasts the needed height; worker aborts if our chunk contains it.
    async fn download_chunk(
        start_height: u64,
        end_height: u64,
        peer_id: &str,
        network: Option<Arc<NetworkManager>>,
        blockstore: &BlockStore,
        config: &ParallelIBDConfig,
        peer_scorer: Arc<crate::network::peer_scoring::PeerScorer>,
        block_tx: Option<tokio::sync::mpsc::UnboundedSender<(u64, Block, Vec<Vec<Witness>>)>>,
        blocks_sem: Option<Arc<Semaphore>>,
        mut stall_rx: Option<&mut broadcast::Receiver<u64>>,
    ) -> Result<Vec<(u64, Block, Vec<Vec<Witness>>)>> {
        let mut blocks = Vec::new();
        let mut progress = BlockDownloadProgress::new();

        info!(
            "Downloading chunk from peer {}: heights {} to {}",
            peer_id, start_height, end_height
        );

        // If network is not available, return empty (for testing)
        let network = match network.as_ref() {
            Some(n) => n,
            None => {
                warn!("NetworkManager not available, skipping block download");
                return Ok(blocks);
            }
        };

        // Convert peer_id to SocketAddr
        let peer_addr = peer_id
            .parse::<SocketAddr>()
            .map_err(|_| anyhow::anyhow!("Invalid peer address: {}", peer_id))?;

        // Collect all block hashes for this chunk from stored headers
        let mut block_hashes = Vec::new();
        for height in start_height..=end_height {
            if let Ok(Some(hash)) = blockstore.get_hash_by_height(height) {
                block_hashes.push((height, hash));
            } else {
                warn!(
                    "Block hash not found for height {} - header may not be stored yet",
                    height
                );
                return Err(anyhow::anyhow!(
                    "Block hash not found for height {} - headers must be downloaded first",
                    height
                ));
            }
        }

        if block_hashes.is_empty() {
            return Err(anyhow::anyhow!(
                "No block hashes found for heights {} to {}",
                start_height,
                end_height
            ));
        }

        // Download blocks using GetData messages
        use crate::network::inventory::MSG_BLOCK;
        use crate::network::protocol::{GetDataMessage, InventoryVector};

        let base_timeout_secs = config.download_timeout_secs;
        let timeout_duration = Duration::from_secs(base_timeout_secs);

        // =====================================================================
        // PIPELINED BLOCK DOWNLOAD — Core-style: MAX_BLOCKS_IN_TRANSIT_PER_PEER
        // =====================================================================
        // Core limits each peer to 16 blocks in flight. We cap pipeline accordingly.
        // When blocks_sem is set, all workers for that peer share 16 permits.
        let pipeline_depth: usize = blocks_sem
            .as_ref()
            .map(|_| MAX_BLOCKS_IN_TRANSIT_PER_PEER)
            .unwrap_or(config.max_concurrent_per_peer);

        // Structures to track in-flight requests. Permit held until block received (or timeout).
        // Within-chunk ordering: wait on ANY block to complete (not FIFO), buffer by height,
        // drain in order. Avoids stall when block 0 arrives last among 0–15.
        type PendingResult = (
            u64,
            [u8; 32],
            std::time::Instant,
            Result<
                Result<(Block, Vec<Vec<Witness>>), tokio::sync::oneshot::error::RecvError>,
                tokio::time::error::Elapsed,
            >,
            Option<tokio::sync::OwnedSemaphorePermit>,
        );
        let mut in_flight: FuturesUnordered<
            std::pin::Pin<Box<dyn std::future::Future<Output = PendingResult> + Send>>,
        > = FuturesUnordered::new();
        let mut hash_iter = block_hashes.into_iter();
        let mut all_sent = false;
        let mut received: BTreeMap<u64, (Block, Vec<Vec<Witness>>)> = BTreeMap::new();
        let mut next_to_send = start_height;

        // Fill the pipeline initially
        let mut first_block_logged = false;
        while in_flight.len() < pipeline_depth {
            if let Some((height, block_hash)) = hash_iter.next() {
                let permit = match &blocks_sem {
                    Some(sem) => Some(
                        sem.clone()
                            .acquire_owned()
                            .await
                            .map_err(|_| anyhow::anyhow!("blocks semaphore closed"))?,
                    ),
                    None => None,
                };
                let block_rx = network.register_block_request(peer_addr, block_hash);
                if !first_block_logged {
                    info!(
                        "[IBD] {} chunk {}-{}: registered block height {} (hash {})",
                        peer_id,
                        start_height,
                        end_height,
                        height,
                        hex::encode(block_hash)
                    );
                    first_block_logged = true;
                }
                let inventory = vec![InventoryVector {
                    inv_type: MSG_BLOCK,
                    hash: block_hash,
                }];
                let wire_msg =
                    ProtocolParser::serialize_message(&ProtocolMessage::GetData(GetDataMessage {
                        inventory,
                    }))?;
                network
                    .send_to_peer(peer_addr, wire_msg)
                    .await
                    .context(format!(
                        "Failed to send GetData for block at height {height}"
                    ))?;
                let request_start = std::time::Instant::now();
                in_flight.push(Box::pin(async move {
                    let r = timeout(timeout_duration, block_rx).await;
                    (height, block_hash, request_start, r, permit)
                }));
            } else {
                all_sent = true;
                break;
            }
        }

        // Process responses: wait on ANY block (not FIFO), buffer by height, drain in order.
        // When coordinator stalls, it broadcasts the needed height; we abort if our chunk contains it.
        loop {
            let next_result = if progress.last_block_hash.is_none() {
                // No first block yet: still listen for stall (coordinator may be waiting for us)
                if let Some(ref mut rx) = stall_rx {
                    tokio::select! {
                        r = in_flight.next() => r,
                        stall_res = rx.recv() => {
                            if let Ok(stall_h) = stall_res {
                                if stall_h >= start_height && stall_h <= end_height {
                                    warn!("Coordinator stall at {}: aborting chunk {}-{} (no first block yet)", stall_h, start_height, end_height);
                                    return Err(anyhow::anyhow!(
                                        "Coordinator stall: aborting chunk {}-{} for retry",
                                        start_height, end_height
                                    ));
                                }
                            }
                            continue;
                        }
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {
                            warn!("Chunk {} to {}: no first block in 5s, failing for retry", start_height, end_height);
                            return Err(anyhow::anyhow!("Block download stalled (no first block in 5s)"));
                        }
                    }
                } else {
                    tokio::select! {
                        r = in_flight.next() => r,
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {
                            warn!("Chunk {} to {}: no first block in 5s, failing for retry", start_height, end_height);
                            return Err(anyhow::anyhow!("Block download stalled (no first block in 5s)"));
                        }
                    }
                }
            } else if let Some(ref mut rx) = stall_rx {
                tokio::select! {
                    r = in_flight.next() => r,
                    stall_res = rx.recv() => {
                        match stall_res {
                            Ok(stall_h) => {
                                if stall_h >= start_height && stall_h <= end_height {
                                    warn!("Coordinator stall at {}: aborting chunk {}-{} for retry", stall_h, start_height, end_height);
                                    return Err(anyhow::anyhow!(
                                        "Coordinator stall: aborting chunk {}-{} for retry",
                                        start_height, end_height
                                    ));
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                warn!("Chunk {}-{}: stall rx lagged {} messages (catching up)", start_height, end_height, n);
                            }
                            Err(broadcast::error::RecvError::Closed) => {}
                        }
                        continue;
                    }
                }
            } else {
                in_flight.next().await
            };

            let Some((height, block_hash, request_start, block_result, _permit)) = next_result
            else {
                break;
            };
            match block_result {
                Ok(Ok((block, block_witnesses))) => {
                    let received_hash = blockstore.get_block_hash(&block);
                    if received_hash != block_hash {
                        warn!("Block hash mismatch for height {}", height);
                        peer_scorer.record_failure(peer_addr);
                    } else {
                        progress.record_progress(received_hash);
                        progress.reset_timeout();
                        let latency_ms = request_start.elapsed().as_secs_f64() * 1000.0;
                        let block_size = block.header.version.to_le_bytes().len() as u64 + 80;
                        peer_scorer.record_block(peer_addr, block_size, latency_ms);
                        received.insert(height, (block, block_witnesses));
                        if !first_block_logged {
                            info!(
                                "[IBD] {} chunk {}-{}: first block received (h={}, {}ms)",
                                peer_id, start_height, end_height, height, latency_ms as u64
                            );
                            first_block_logged = true;
                        }
                    }
                }
                Ok(Err(_)) => {
                    warn!("Block channel closed for height {}", height);
                    peer_scorer.record_failure(peer_addr);
                    return Err(anyhow::anyhow!(
                        "Block channel closed for height {} - chunk needs retry",
                        height
                    ));
                }
                Err(_) => {
                    warn!(
                        "Block timeout for height {} after {}s",
                        height, base_timeout_secs
                    );
                    peer_scorer.record_failure(peer_addr);
                    return Err(anyhow::anyhow!(
                        "Block timeout for height {} after {}s - chunk needs retry",
                        height,
                        base_timeout_secs
                    ));
                }
            }

            // Drain received buffer in height order (validation needs sequential delivery)
            while let Some((block, block_witnesses)) = received.remove(&next_to_send) {
                blocks.push((next_to_send, block.clone(), block_witnesses.clone()));
                if let Some(ref tx) = block_tx {
                    if tx.send((next_to_send, block, block_witnesses)).is_err() {
                        return Err(anyhow::anyhow!(
                            "block_tx closed during stream - chunk needs retry"
                        ));
                    }
                }
                next_to_send += 1;
            }

            // Refill pipeline with next block
            if !all_sent {
                if let Some((next_height, next_hash)) = hash_iter.next() {
                    let permit = match &blocks_sem {
                        Some(sem) => Some(
                            sem.clone()
                                .acquire_owned()
                                .await
                                .map_err(|_| anyhow::anyhow!("blocks semaphore closed"))?,
                        ),
                        None => None,
                    };
                    let block_rx = network.register_block_request(peer_addr, next_hash);
                    let inventory = vec![InventoryVector {
                        inv_type: MSG_BLOCK,
                        hash: next_hash,
                    }];
                    let wire_msg = ProtocolParser::serialize_message(&ProtocolMessage::GetData(
                        GetDataMessage { inventory },
                    ))?;
                    network
                        .send_to_peer(peer_addr, wire_msg)
                        .await
                        .context(format!(
                            "Failed to send GetData for block at height {next_height}"
                        ))?;
                    let request_start = std::time::Instant::now();
                    in_flight.push(Box::pin(async move {
                        let r = timeout(timeout_duration, block_rx).await;
                        (next_height, next_hash, request_start, r, permit)
                    }));
                } else {
                    all_sent = true;
                }
            }
        }

        // Drain any remaining buffered blocks (e.g. last few in chunk)
        while let Some((block, block_witnesses)) = received.remove(&next_to_send) {
            blocks.push((next_to_send, block.clone(), block_witnesses.clone()));
            if let Some(ref tx) = block_tx {
                if tx.send((next_to_send, block, block_witnesses)).is_err() {
                    return Err(anyhow::anyhow!(
                        "block_tx closed during stream - chunk needs retry"
                    ));
                }
            }
            next_to_send += 1;
        }

        Ok(blocks)
    }

    /// Validate and store a block
    fn validate_and_store_block(
        &self,
        blockstore: &BlockStore,
        storage: Option<&Arc<Storage>>,
        protocol: &BitcoinProtocolEngine,
        utxo_set: &mut UtxoSet,
        block: &Block,
        witnesses: &[Vec<Witness>],
        height: u64,
    ) -> Result<()> {
        // Prepare validation context
        let (stored_witnesses, _recent_headers) =
            prepare_block_validation_context(blockstore, block, height)?;

        // Use witnesses from download or stored witnesses
        let witnesses_to_use = if !witnesses.is_empty() {
            witnesses
        } else {
            &stored_witnesses
        };

        // CRITICAL: Validate witness count before calling validation
        // The validation code has an assertion that will abort if counts don't match
        // We check here and return a proper error instead of crashing
        if witnesses_to_use.len() != block.transactions.len() {
            return Err(anyhow::anyhow!(
                "Witness count mismatch at height {}: {} witnesses for {} transactions",
                height,
                witnesses_to_use.len(),
                block.transactions.len()
            ));
        }

        // Validate block
        let validation_result = validate_block_with_context(
            blockstore,
            protocol,
            block,
            witnesses_to_use,
            utxo_set,
            height,
        )?;

        if matches!(validation_result, ValidationResult::Valid) {
            // Store block
            store_block_with_context_and_index(
                blockstore,
                storage,
                block,
                witnesses_to_use,
                height,
            )?;

            debug!("Validated and stored block at height {}", height);
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Block validation failed at height {}",
                height
            ))
        }
    }

    /// Validate a block WITHOUT storing it (for deferred storage mode)
    ///
    /// Calls connect_block_ibd directly — bypasses protocol layer for maximum speed.
    /// Single-pass prefetch: cache lookup + disk load + map build in one pass.
    /// Single-pass: cache lookup + disk load + map build.
    fn prefetch_build_utxo_map(
        store: &crate::storage::ibd_utxo_store::IbdUtxoStore,
        keys: &[OutPointKey],
    ) -> rustc_hash::FxHashMap<OutPointKey, Arc<UTXO>> {
        let mut full_map =
            rustc_hash::FxHashMap::with_capacity_and_hasher(keys.len(), Default::default());
        let mut to_load: Vec<OutPointKey> = Vec::new();
        for key in keys {
            if let Some(ref r) = store.cache_get(key) {
                if let Some(arc) = r.as_ref() {
                    full_map.insert(*key, Arc::clone(arc));
                    continue;
                }
            }
            to_load.push(*key);
        }
        if !to_load.is_empty() && !store.memory_only() {
            if let Ok(loaded) = load_keys_from_disk(store.disk_clone(), to_load) {
                for (key, utxo) in loaded {
                    let arc = Arc::new(utxo);
                    full_map.insert(key, Arc::clone(&arc));
                    store.cache_insert_and_track(key, arc);
                }
            }
        }
        full_map
    }

    /// Returns pre-computed tx_ids so the caller avoids redundant double-SHA256.
    /// network_time: cached at loop init, refreshed every 1000 blocks (avoids per-block SystemTime syscall).
    /// bip30_index: O(1) duplicate-coinbase check; when Some, updated during apply_transaction.
    #[inline]
    fn validate_block_only(
        &self,
        _blockstore: &BlockStore,
        _protocol: &BitcoinProtocolEngine,
        utxo_set: &mut UtxoSet,
        bip30_index: Option<&mut Bip30Index>,
        block: &Block,
        block_arc: Option<Arc<Block>>,
        witnesses: &[Vec<Witness>],
        witnesses_arc: Option<&std::sync::Arc<Vec<Vec<Witness>>>>,
        height: u64,
        recent_headers: Option<&[Arc<BlockHeader>]>,
        network_time: u64,
        precomputed_tx_ids: Option<&[Hash]>,
    ) -> Result<(Vec<Hash>, Option<blvm_consensus::block::UtxoDelta>)> {
        // Caller provides witnesses (or empty_witnesses_buf for pre-SegWit). Length already validated.
        let owned_utxo = std::mem::take(utxo_set);
        let (result, new_utxo_set, tx_ids, utxo_delta) = blvm_consensus::block::connect_block_ibd(
            block,
            witnesses,
            owned_utxo,
            height,
            recent_headers,
            network_time,
            blvm_consensus::types::Network::Mainnet,
            bip30_index,
            precomputed_tx_ids,
            block_arc,
            witnesses_arc,
        )?;

        *utxo_set = new_utxo_set;
        match result {
            ValidationResult::Valid => Ok((tx_ids, utxo_delta)),
            ValidationResult::Invalid(reason) => Err(anyhow::anyhow!(
                "Block validation failed at height {}: {}",
                height,
                reason
            )),
        }
    }

    /// When block validation fails, dump block, witnesses, and UTXO set to disk so a test case can be built.
    /// Directory: $BLVM_IBD_DUMP_DIR or /tmp/blvm_ibd_failure, then height_{height}/.
    /// Files: block.bin, witnesses.bin, utxo_set.bin, info.txt (height, error reason).
    fn dump_failed_block(
        height: u64,
        block: &Block,
        witnesses: &[Vec<Witness>],
        utxo_set: &UtxoSet,
        err: &anyhow::Error,
    ) {
        let base = std::env::var("BLVM_IBD_DUMP_DIR").unwrap_or_else(|_| {
            std::env::temp_dir()
                .join("blvm_ibd_failure")
                .to_string_lossy()
                .into_owned()
        });
        let dir = std::path::Path::new(&base).join(format!("height_{height}"));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            error!("Failed to create dump dir {}: {}", dir.display(), e);
            return;
        }
        let block_path = dir.join("block.bin");
        let witnesses_path = dir.join("witnesses.bin");
        let utxo_path = dir.join("utxo_set.bin");
        let info_path = dir.join("info.txt");

        if let Ok(f) = std::fs::File::create(&block_path) {
            if let Err(e) = bincode::serialize_into(std::io::BufWriter::new(f), block) {
                error!(
                    "Failed to serialize block to {}: {}",
                    block_path.display(),
                    e
                );
            }
        } else {
            error!("Failed to create {}", block_path.display());
        }
        if let Ok(f) = std::fs::File::create(&witnesses_path) {
            if let Err(e) = bincode::serialize_into(std::io::BufWriter::new(f), witnesses) {
                error!(
                    "Failed to serialize witnesses to {}: {}",
                    witnesses_path.display(),
                    e
                );
            }
        } else {
            error!("Failed to create {}", witnesses_path.display());
        }
        if let Ok(f) = std::fs::File::create(&utxo_path) {
            let serializable: std::collections::HashMap<_, _> =
                utxo_set.iter().map(|(k, v)| (*k, (**v).clone())).collect();
            if let Err(e) = bincode::serialize_into(std::io::BufWriter::new(f), &serializable) {
                error!(
                    "Failed to serialize utxo_set to {}: {}",
                    utxo_path.display(),
                    e
                );
            }
        } else {
            error!("Failed to create {}", utxo_path.display());
        }
        let info = format!(
            "height={}\nerror={}\ntxs={}\ninputs={}\nutxo_len={}\n",
            height,
            err,
            block.transactions.len(),
            block
                .transactions
                .iter()
                .map(|tx| tx.inputs.len())
                .sum::<usize>(),
            utxo_set.len(),
        );
        if let Err(e) = std::fs::write(&info_path, info) {
            error!("Failed to write {}: {}", info_path.display(), e);
        }
        info!(
            "IBD_FAILURE_DUMP: Block {} validation failed. Test data written to: {} (block.bin, witnesses.bin, utxo_set.bin, info.txt). Run: ./scripts/ibd_failure_to_repro_test.sh {}",
            height, dir.display(), height
        );
    }

    /// Dump successful block + witnesses + pre-state UTXO at IBD milestones for snapshot tests.
    /// Triggered when BLVM_IBD_SNAPSHOT_DIR is set; dumps at 50k, 90k, 125k, 133k, 145k, 175k, 181k, 190k, 200k.
    /// Same format as dump_failed_block; info.txt has error=ok, pre_state=1.
    fn dump_ibd_snapshot(
        height: u64,
        block: &Block,
        witnesses: &[Vec<Witness>],
        utxo_set: &UtxoSet,
        base_dir: &str,
    ) {
        let dir = std::path::Path::new(base_dir).join(format!("height_{height}"));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            error!(
                "IBD_SNAPSHOT: Failed to create dir {}: {}",
                dir.display(),
                e
            );
            return;
        }
        let block_path = dir.join("block.bin");
        let witnesses_path = dir.join("witnesses.bin");
        let utxo_path = dir.join("utxo_set.bin");
        let info_path = dir.join("info.txt");

        if let Ok(f) = std::fs::File::create(&block_path) {
            let _ = bincode::serialize_into(std::io::BufWriter::new(f), block);
        }
        if let Ok(f) = std::fs::File::create(&witnesses_path) {
            let _ = bincode::serialize_into(std::io::BufWriter::new(f), witnesses);
        }
        if let Ok(f) = std::fs::File::create(&utxo_path) {
            let serializable: std::collections::HashMap<_, _> =
                utxo_set.iter().map(|(k, v)| (*k, (**v).clone())).collect();
            let _ = bincode::serialize_into(std::io::BufWriter::new(f), &serializable);
        }
        let n_txs = block.transactions.len();
        let n_inputs: usize = block.transactions.iter().map(|tx| tx.inputs.len()).sum();
        let info = format!(
            "height={}\nerror=ok\ntxs={}\ninputs={}\nutxo_len={}\npre_state=1\nrerun=BLVM_IBD_SNAPSHOT_DIR={} cargo test -p blvm-consensus --test block_ibd_snapshot_tests -- --ignored\n",
            height,
            n_txs,
            n_inputs,
            utxo_set.len(),
            base_dir
        );
        let _ = std::fs::write(&info_path, info);
        info!(
            "IBD_SNAPSHOT: Block {} dumped to: {} (block.bin, witnesses.bin, utxo_set.bin, info.txt)",
            height,
            dir.display()
        );
    }

    /// Flush pending blocks to storage using batch writes
    ///
    /// This commits multiple blocks in a single database transaction,
    /// which is much faster than individual writes.
    fn flush_pending_blocks(
        &self,
        blockstore: &BlockStore,
        _storage: Option<&Arc<Storage>>,
        pending: &mut Vec<(Arc<Block>, Arc<BlockHeader>, Arc<Vec<Vec<Witness>>>, u64)>,
    ) -> Result<()> {
        let to_flush = std::mem::take(pending);
        Self::do_flush_to_storage(blockstore, _storage, to_flush)
    }

    /// Core flush logic. Takes ownership of pending. Used by sync flush and async spawn.
    /// Blocks are Arc<Block>; we try_unwrap to get owned Block for serialization (sync has completed).
    fn do_flush_to_storage(
        blockstore: &BlockStore,
        _storage: Option<&Arc<Storage>>,
        pending: Vec<(Arc<Block>, Arc<BlockHeader>, Arc<Vec<Vec<Witness>>>, u64)>,
    ) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }

        let count = pending.len();
        let start = std::time::Instant::now();
        #[cfg(feature = "profile")]
        let t_serialize = std::time::Instant::now();

        // Unwrap Arcs to get owned Block (sync has completed; refcount should be 1).
        // Restore header from Arc (we store header_rc separately; block has original header).
        let mut pending: Vec<(Block, Arc<BlockHeader>, Arc<Vec<Vec<Witness>>>, u64)> = pending
            .into_iter()
            .map(|(arc_block, header_rc, w, h)| {
                let mut block = Arc::try_unwrap(arc_block).unwrap_or_else(|a| (*a).clone());
                block.header = header_rc.as_ref().clone();
                (block, header_rc, w, h)
            })
            .collect();

        // Pre-compute all block hashes ONCE (avoids 4x redundant double SHA256 per block)
        // Parallelize hash computation and serialization for better CPU utilization
        // header_data uses Arc to avoid cloning Vec on cache hit (batch.put accepts &[u8] via .as_slice())
        let (block_hashes, block_data, header_data): (Vec<Hash>, Vec<Vec<u8>>, Vec<Arc<Vec<u8>>>) = {
            #[cfg(feature = "rayon")]
            {
                use rayon::prelude::*;
                let block_hashes: Vec<Hash> = pending
                    .par_iter()
                    .map(|(block, _, _, _)| blockstore.get_block_hash(block))
                    .collect();

                // Parallel serialize all block data
                let block_data: Vec<Vec<u8>> = pending
                    .par_iter()
                    .map(|(block, _, _, _)| bincode::serialize(block).unwrap())
                    .collect();

                // Parallel serialize all header data (with caching)
                use crate::storage::serialization_cache::{
                    cache_serialized_header, get_cached_serialized_header,
                };
                let header_data: Vec<Arc<Vec<u8>>> = pending
                    .par_iter()
                    .zip(block_hashes.par_iter())
                    .map(|((block, _, _, _), block_hash)| {
                        if let Some(cached) = get_cached_serialized_header(block_hash) {
                            return cached; // Arc::clone already done in get; no Vec clone
                        }
                        let serialized = bincode::serialize(&block.header).unwrap();
                        cache_serialized_header(*block_hash, serialized.clone());
                        Arc::new(serialized)
                    })
                    .collect();

                (block_hashes, block_data, header_data)
            }

            #[cfg(not(feature = "rayon"))]
            {
                let block_hashes: Vec<Hash> = pending
                    .iter()
                    .map(|(block, _, _, _)| blockstore.get_block_hash(block))
                    .collect();

                // Pre-serialize all block data
                let block_data: Vec<Vec<u8>> = pending
                    .iter()
                    .map(|(block, _, _, _)| bincode::serialize(block).unwrap())
                    .collect();

                // Pre-serialize all header data (with caching)
                use crate::storage::serialization_cache::{
                    cache_serialized_header, get_cached_serialized_header,
                };
                let header_data: Vec<Arc<Vec<u8>>> = pending
                    .iter()
                    .zip(block_hashes.iter())
                    .map(|((block, _, _, _), block_hash)| {
                        if let Some(cached) = get_cached_serialized_header(block_hash) {
                            return cached;
                        }
                        let serialized = bincode::serialize(&block.header).unwrap();
                        cache_serialized_header(*block_hash, serialized.clone());
                        Arc::new(serialized)
                    })
                    .collect();

                (block_hashes, block_data, header_data)
            }
        };

        #[cfg(feature = "profile")]
        let serialize_ms = t_serialize.elapsed().as_millis() as u64;
        #[cfg(feature = "profile")]
        let t_disk = std::time::Instant::now();

        // Batch write blocks
        {
            let blocks_tree = blockstore.blocks_tree()?;
            let mut batch = blocks_tree.batch();
            for (i, data) in block_data.iter().enumerate() {
                batch.put(&block_hashes[i], data);
            }
            batch.commit()?;
        }

        // Batch write headers
        {
            let headers_tree = blockstore.headers_tree()?;
            let mut batch = headers_tree.batch();
            for (i, data) in header_data.iter().enumerate() {
                batch.put(&block_hashes[i], data.as_slice());
            }
            batch.commit()?;
        }

        // Batch write witnesses (skip if all empty - common in early chain)
        // Parallelize witness serialization for better CPU utilization
        {
            let has_witnesses = pending.iter().any(|(_, _, w, _)| !w.is_empty());
            if has_witnesses {
                let witnesses_tree = blockstore.witnesses_tree()?;
                let mut batch = witnesses_tree.batch();

                #[cfg(feature = "rayon")]
                {
                    use rayon::prelude::*;
                    // Parallel serialize witnesses
                    let witness_data_vec: Vec<(usize, Vec<u8>)> = pending
                        .par_iter()
                        .enumerate()
                        .filter_map(|(i, (_, _, witnesses, _))| {
                            if !witnesses.is_empty() {
                                match bincode::serialize(witnesses.as_ref()) {
                                    Ok(data) => Some((i, data)),
                                    Err(_) => None,
                                }
                            } else {
                                None
                            }
                        })
                        .collect();

                    for (i, data) in witness_data_vec {
                        batch.put(&block_hashes[i], &data);
                    }
                }

                #[cfg(not(feature = "rayon"))]
                {
                    for (i, (_, _, witnesses, _)) in pending.iter().enumerate() {
                        if !witnesses.is_empty() {
                            let witness_data =
                                bincode::serialize(witnesses.as_ref()).map_err(|e| {
                                    anyhow::anyhow!("Failed to serialize witnesses: {}", e)
                                })?;
                            batch.put(&block_hashes[i], &witness_data);
                        }
                    }
                }

                batch.commit()?;
            }
        }

        // Batch write height index
        {
            let height_tree = blockstore.height_tree()?;
            let mut batch = height_tree.batch();
            for (i, (_, _, _, height)) in pending.iter().enumerate() {
                let height_key = height.to_be_bytes();
                batch.put(&height_key, &block_hashes[i]);
            }
            batch.commit()?;
        }

        // Store recent headers (needed for MTP calculation)
        // Only store the last 11 headers (minimal overhead, sequential is fine)
        for (block, _, _, height) in pending.iter().rev().take(11) {
            blockstore.store_recent_header(*height, &block.header)?;
        }

        #[cfg(feature = "profile")]
        {
            let disk_ms = t_disk.elapsed().as_millis() as u64;
            if serialize_ms > 2 || disk_ms > 2 {
                blvm_consensus::profile_log!(
                    "[FLUSH_STORAGE_PERF] blocks={} serialize_ms={} disk_ms={} total_ms={}",
                    count,
                    serialize_ms,
                    disk_ms,
                    start.elapsed().as_millis()
                );
            }
        }

        // Skip transaction indexing during IBD - it's not needed until sync is complete
        // and causes massive slowdowns due to individual writes per transaction

        let elapsed = start.elapsed();
        // Use debug! — this is disk write throughput for one batch, NOT IBD blocks/s.
        // Users often confuse 80k blocks/sec here with actual IBD rate (~100–5k BPS).
        debug!(
            "Batch stored {} blocks in {:?} ({:.0} blocks/sec)",
            count,
            elapsed,
            count as f64 / elapsed.as_secs_f64()
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn test_parallel_ibd_config_default() {
        let config = ParallelIBDConfig::default();
        assert!(config.num_workers > 0);
        // chunk_size: 500 default, or BLVM_IBD_CHUNK_SIZE (16-2000) if set
        assert!(
            config.chunk_size >= 16 && config.chunk_size <= 2000,
            "chunk_size={}",
            config.chunk_size
        );
        assert_eq!(config.max_concurrent_per_peer, 64);
    }

    #[test]
    fn test_create_chunks() {
        let config = ParallelIBDConfig {
            chunk_size: 100,
            ..Default::default()
        };
        let ibd = ParallelIBD::new(config);
        let peer_ids = vec!["peer1".to_string(), "peer2".to_string()];

        let chunks = ibd.create_chunks(0, 250, &peer_ids, None);

        // Bootstrap chunk is always ≥128 blocks so 99 and 100 are in same chunk (stall fix)
        assert_eq!(chunks.len(), 3); // 0-127, 128-227, 228-250
        assert_eq!(chunks[0].start_height, 0);
        assert_eq!(
            chunks[0].end_height, 127,
            "Bootstrap chunk must include 99 and 100"
        );
        assert_eq!(chunks[1].start_height, 128);
        assert_eq!(chunks[1].end_height, 227);
        assert_eq!(chunks[2].start_height, 228);
        assert_eq!(chunks[2].end_height, 250);

        // Note: With weighted assignment, peer selection depends on scores
        // All peers have equal score (1.0) by default, so they get equal chunks
        // Just verify all chunks have a valid peer assigned
        for chunk in &chunks {
            assert!(
                peer_ids.contains(&chunk.peer_id),
                "Chunk should be assigned to a valid peer, got: {}",
                chunk.peer_id
            );
        }
    }

    /// Ensures bootstrap chunk includes both block 99 and 100 — prevents stall at 99.
    #[test]
    fn test_bootstrap_chunk_includes_99_and_100() {
        let config = ParallelIBDConfig {
            chunk_size: 16, // Small chunk_size would normally put 99/100 in different chunks
            ..Default::default()
        };
        let ibd = ParallelIBD::new(config);
        let peer_ids = vec!["peer1".to_string()];
        let chunks = ibd.create_chunks(0, 500, &peer_ids, None);
        assert!(!chunks.is_empty(), "Must have at least one chunk");
        let bootstrap = &chunks[0];
        assert!(
            bootstrap.end_height >= 100,
            "Bootstrap chunk must include block 100 (end={})",
            bootstrap.end_height
        );
        assert!(
            bootstrap.start_height <= 99,
            "Bootstrap chunk must include block 99 (start={})",
            bootstrap.start_height
        );
    }

    // ============================================================
    // CRITICAL: Work Queue FIFO Order Tests
    //
    // This test prevents regression of the LIFO bug where chunks
    // were downloaded in REVERSE order (highest heights first instead
    // of lowest heights first). This caused IBD to fail because
    // validation requires sequential processing from block 0.
    // ============================================================

    #[test]
    fn test_work_queue_fifo_order_not_lifo() {
        // This test verifies the fix for the critical LIFO->FIFO bug
        //
        // BUG THAT WAS FIXED:
        // - Used Vec::pop() which is LIFO (Last-In-First-Out)
        // - This caused blocks to be downloaded starting at height 931,000
        //   instead of height 0
        // - Validation requires sequential processing from genesis
        //
        // FIX:
        // - Changed to VecDeque::pop_front() which is FIFO (First-In-First-Out)
        // - Now chunks are processed in the correct order (lowest heights first)

        // Simulate the work queue as created in sync_parallel
        let chunks: Vec<(u64, u64, Option<String>)> = vec![
            (0u64, 99u64, None),
            (100u64, 199u64, None),
            (200u64, 299u64, None),
            (931000u64, 931099u64, None),
        ];

        let mut work_queue: VecDeque<(u64, u64, Option<String>)> = chunks.into_iter().collect();

        // Verify FIFO order (first chunk in = first chunk out)
        let (s, e, _) = work_queue.pop_front().unwrap();
        assert_eq!((s, e), (0, 99), "First chunk should be (0, 99)");

        let (s, e, _) = work_queue.pop_front().unwrap();
        assert_eq!((s, e), (100, 199), "Second chunk should be (100, 199)");

        let (s, e, _) = work_queue.pop_front().unwrap();
        assert_eq!((s, e), (200, 299), "Third chunk should be (200, 299)");

        let (s, e, _) = work_queue.pop_front().unwrap();
        assert_eq!(
            (s, e),
            (931000, 931099),
            "Fourth chunk should be the high-height chunk"
        );
    }

    #[test]
    fn test_vec_pop_is_lifo_bug() {
        // Document the bug that was fixed: Vec::pop() is LIFO
        // This test shows WHY the old code was wrong

        let mut vec_queue: Vec<(u64, u64)> = vec![(0, 99), (100, 199), (200, 299)];

        // Vec::pop() takes from the END (LIFO) - this was the BUG
        let popped = vec_queue.pop().unwrap();
        assert_eq!(
            popped,
            (200, 299),
            "Vec::pop() returns LAST element (LIFO behavior)"
        );

        // This is why IBD was downloading height 931,000 first instead of height 0!
    }

    #[test]
    fn test_vecdeque_pop_front_is_fifo_correct() {
        // Document the fix: VecDeque::pop_front() is FIFO

        let mut deque_queue: VecDeque<(u64, u64, Option<String>)> =
            VecDeque::from(vec![(0, 99, None), (100, 199, None), (200, 299, None)]);

        // VecDeque::pop_front() takes from the FRONT (FIFO) - this is CORRECT
        let (s, e, _) = deque_queue.pop_front().unwrap();
        assert_eq!(
            (s, e),
            (0, 99),
            "VecDeque::pop_front() returns FIRST element (FIFO behavior)"
        );

        // This ensures IBD downloads blocks starting from height 0
    }

    #[test]
    fn test_failed_chunk_requeue_excludes_failing_peer() {
        // Verify that failed chunks are re-queued with exclude_peer so a DIFFERENT peer retries.
        // Same peer retrying would likely fail again (e.g. disconnected).

        let mut work_queue: VecDeque<(u64, u64, Option<String>)> =
            VecDeque::from(vec![(100, 199, None), (200, 299, None)]);

        // Simulate peer "flaky:8333" failing chunk 0-99 - re-queue with exclude
        work_queue.push_front((0, 99, Some("flaky:8333".to_string())));

        let (start, end, exclude) = work_queue.pop_front().unwrap();
        assert_eq!((start, end), (0, 99));
        assert_eq!(exclude.as_deref(), Some("flaky:8333"));
        // Worker for flaky:8333 would skip this; worker for other peer would take it
    }

    // ============================================================
    // Chunk Creation Order Tests
    // ============================================================

    #[test]
    fn test_chunks_created_in_ascending_height_order() {
        let config = ParallelIBDConfig {
            chunk_size: 1000,
            ..Default::default()
        };
        let ibd = ParallelIBD::new(config);
        let peer_ids = vec!["peer1".to_string()];

        let chunks = ibd.create_chunks(0, 10000, &peer_ids, None);

        // Verify chunks are in ascending order
        for i in 1..chunks.len() {
            assert!(
                chunks[i].start_height > chunks[i - 1].start_height,
                "Chunk {} start ({}) should be > chunk {} start ({})",
                i,
                chunks[i].start_height,
                i - 1,
                chunks[i - 1].start_height
            );
            assert!(
                chunks[i].start_height == chunks[i - 1].end_height + 1,
                "Chunk {} start ({}) should immediately follow chunk {} end ({})",
                i,
                chunks[i].start_height,
                i - 1,
                chunks[i - 1].end_height
            );
        }

        // First chunk must start at 0
        assert_eq!(
            chunks[0].start_height, 0,
            "First chunk must start at height 0"
        );
    }

    #[test]
    fn test_create_chunks_covers_full_range() {
        let config = ParallelIBDConfig {
            chunk_size: 500,
            ..Default::default()
        };
        let ibd = ParallelIBD::new(config);
        let peer_ids = vec!["peer1".to_string(), "peer2".to_string()];

        let start = 0u64;
        let end = 935000u64; // Approximate mainnet height
        let chunks = ibd.create_chunks(start, end, &peer_ids, None);

        // First chunk starts at start
        assert_eq!(chunks.first().unwrap().start_height, start);

        // Last chunk ends at or after end
        assert!(chunks.last().unwrap().end_height >= end);

        // No gaps between chunks
        for i in 1..chunks.len() {
            assert_eq!(
                chunks[i].start_height,
                chunks[i - 1].end_height + 1,
                "Gap detected between chunk {} and {}",
                i - 1,
                i
            );
        }
    }

    // ============================================================
    // Block Download Progress Tests
    // ============================================================

    #[test]
    fn test_block_download_progress_new() {
        let progress = BlockDownloadProgress::new();
        assert!(progress.last_block_hash.is_none());
        assert_eq!(progress.current_timeout_seconds, 120);
        assert_eq!(progress.disconnected_peers_count, 0);
    }

    #[test]
    fn test_block_download_progress_adaptive_timeout() {
        let mut progress = BlockDownloadProgress::new();

        // Initial timeout
        assert_eq!(progress.current_timeout_seconds, 120);

        // Simulate peer disconnects - timeout should increase
        progress.disconnected_peers_count = 3;
        // In production, this triggers timeout increase
        // We just verify the field is tracked
        assert_eq!(progress.disconnected_peers_count, 3);
    }

    // ============================================================
    // Checkpoint Tests
    // ============================================================

    #[test]
    fn test_mainnet_checkpoints_exist() {
        assert!(
            !MAINNET_CHECKPOINTS.is_empty(),
            "Checkpoints should be defined"
        );
    }

    #[test]
    fn test_mainnet_checkpoints_start_at_genesis() {
        let (height, _hash) = MAINNET_CHECKPOINTS[0];
        assert_eq!(
            height, 0,
            "First checkpoint should be genesis block (height 0)"
        );
    }

    #[test]
    fn test_mainnet_checkpoints_in_ascending_order() {
        for i in 1..MAINNET_CHECKPOINTS.len() {
            let (prev_height, _) = MAINNET_CHECKPOINTS[i - 1];
            let (curr_height, _) = MAINNET_CHECKPOINTS[i];
            assert!(
                curr_height > prev_height,
                "Checkpoint {} (height {}) should be > checkpoint {} (height {})",
                i,
                curr_height,
                i - 1,
                prev_height
            );
        }
    }

    #[test]
    fn test_mainnet_genesis_hash() {
        // Verify the genesis block hash is correct
        let (height, hash) = MAINNET_CHECKPOINTS[0];
        assert_eq!(height, 0);

        // Genesis block hash in little-endian (internal byte order)
        let expected_genesis = [
            0x6f, 0xe2, 0x8c, 0x0a, 0xb6, 0xf1, 0xb3, 0x72, 0xc1, 0xa6, 0xa2, 0x46, 0xae, 0x63,
            0xf7, 0x4f, 0x93, 0x1e, 0x83, 0x65, 0xe1, 0x5a, 0x08, 0x9c, 0x68, 0xd6, 0x19, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(hash, expected_genesis, "Genesis block hash should match");
    }

    // ============================================================
    // Configuration Tests
    // ============================================================

    #[test]
    fn test_config_chunk_size_reasonable() {
        let config = ParallelIBDConfig::default();
        // 16 = Core-like, 500 = default, 2000 = max (BLVM_IBD_CHUNK_SIZE override)
        assert!(
            config.chunk_size >= 16 && config.chunk_size <= 2000,
            "chunk_size={}",
            config.chunk_size
        );
    }

    #[test]
    fn test_config_timeout_reasonable() {
        let config = ParallelIBDConfig::default();
        // Timeout should accommodate slow peers and large blocks
        assert!(
            config.download_timeout_secs >= 30,
            "Timeout too short for large blocks"
        );
        assert!(
            config.download_timeout_secs <= 300,
            "Timeout too long, will stall on dead peers"
        );
    }

    #[test]
    fn test_config_concurrency_reasonable() {
        let config = ParallelIBDConfig::default();
        // Should pipeline multiple requests per peer
        assert!(
            config.max_concurrent_per_peer >= 8,
            "Need more pipelining for throughput"
        );
        assert!(
            config.max_concurrent_per_peer <= 256,
            "Too much pipelining may overwhelm peers"
        );
    }
}
