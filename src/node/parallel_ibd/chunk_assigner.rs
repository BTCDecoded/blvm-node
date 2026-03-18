//! Chunk assignment for parallel IBD.
//!
//! ChunkAssigner assigns height-ordered chunks to workers. ChunkGuard ensures
//! chunks are re-queued on drop if not disarmed. Extracted for Phase 2.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::types::ChunkWorkItem;
use super::ParallelIBDConfig;

/// Chunk of blocks to download, assigned to a specific peer.
#[derive(Debug, Clone)]
pub struct BlockChunk {
    pub start_height: u64,
    pub end_height: u64,
    pub peer_id: String,
}

/// Create chunks for parallel download.
///
/// When scored_peers is Some and BLVM_IBD_MODE=earliest: assign all chunks to fastest peer
/// (Core-like, avoids chunk-boundary stalls when slow peer holds next chunk).
/// Otherwise: round-robin (chunk i → peer i % num_peers).
pub fn create_chunks(
    config: &ParallelIBDConfig,
    start_height: u64,
    end_height: u64,
    peer_ids: &[String],
    scored_peers: Option<&[(String, f64)]>,
) -> Vec<BlockChunk> {
    let mut chunks = Vec::new();
    let mut current_height = start_height;
    let num_peers = peer_ids.len().max(1);
    let mut chunk_index: usize = 0;

    let use_fastest = (config.mode.eq_ignore_ascii_case("earliest") || config.earliest_first)
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
        tracing::info!("IBD: earliest-first — all chunks to fastest peer");
    } else {
        tracing::info!(
            "Round-robin chunk assignment: {} peers, chunk_size={}",
            num_peers,
            config.chunk_size
        );
    }

    while current_height <= end_height {
        let (chunk_sz, is_bootstrap) = if current_height == 0 && start_height == 0 {
            let sz = 128.min(end_height.saturating_add(1));
            (sz, true)
        } else {
            (config.chunk_size, false)
        };
        let chunk_end = (current_height + chunk_sz - 1).min(end_height);
        if is_bootstrap {
            tracing::info!(
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
pub(crate) struct ChunkAssigner {
    chunks: Vec<(u64, u64)>,
    /// Peer assigned to each chunk; same length as chunks. Worker gets chunk only if peer matches.
    chunk_peers: Vec<String>,
    next_index: AtomicUsize,
    retry_queue: Mutex<VecDeque<ChunkWorkItem>>,
    validation_height: Arc<std::sync::atomic::AtomicU64>,
    /// When true, only chunks with start==0 are assignable. Set when start_height==0; cleared when bootstrap chunk completes.
    bootstrap_complete: AtomicBool,
    start_height: u64,
    /// Per-peer serial: peer_id -> (start, end) of chunk in flight. At most one chunk per peer.
    in_flight_per_peer: Mutex<HashMap<String, (u64, u64)>>,
}

impl ChunkAssigner {
    pub(crate) fn new(
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
            next_index: AtomicUsize::new(0),
            retry_queue: Mutex::new(VecDeque::new()),
            validation_height,
            bootstrap_complete: AtomicBool::new(bootstrap_complete),
            start_height,
            in_flight_per_peer: Mutex::new(HashMap::new()),
        }
    }

    /// Mark bootstrap chunk (0..N) complete — enables parallel chunk assignment for start_height > 0.
    pub(crate) fn mark_bootstrap_complete(&self) {
        self.bootstrap_complete.store(true, Ordering::Relaxed);
    }

    /// Returns the next assignable chunk for this peer, or None if nothing ready.
    /// Per-peer serial: returns None if this peer already has a chunk in flight (eliminates chunk-boundary stalls).
    /// Round-robin: prioritizes critical chunk (containing next_needed) from retry, then earliest available.
    /// CRITICAL: Entire operation under one lock to prevent duplicate chunk assignment (race: two workers
    /// for same peer both getting chunk 116240-116255, both requesting same blocks, one starves).
    pub(crate) fn get_work(&self, peer_id: &str, max_ahead: u64) -> Option<(u64, u64)> {
        let bootstrap_done = self.bootstrap_complete.load(Ordering::Relaxed);
        let current_validation = self.validation_height.load(Ordering::Relaxed);
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
        let idx = self.next_index.load(Ordering::Relaxed);
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
        self.next_index.store(idx + 1, Ordering::Relaxed);
        guard.insert(peer_id.to_string(), (start, end));
        Some((start, end))
    }

    /// Called when a worker completes (or fails) a chunk. Clears in-flight so peer can get next chunk.
    pub(crate) fn on_chunk_complete(&self, peer_id: &str) {
        self.in_flight_per_peer.lock().unwrap().remove(peer_id);
    }

    pub(crate) fn requeue(&self, start: u64, end: u64, exclude_peer: Option<String>) {
        // Use exclude_peer to avoid immediate retry with same peer, but stall recovery can clear it
        self.retry_queue
            .lock()
            .unwrap()
            .push_back((start, end, exclude_peer));
    }

    pub(crate) fn is_done(&self) -> bool {
        let idx = self.next_index.load(Ordering::Relaxed);
        idx >= self.chunks.len() && self.retry_queue.lock().unwrap().is_empty()
    }

    pub(crate) fn total_chunks(&self) -> usize {
        self.chunks.len()
    }

    pub(crate) fn remaining_count(&self) -> usize {
        let idx = self.next_index.load(Ordering::Relaxed);
        let retry_len = self.retry_queue.lock().unwrap().len();
        self.chunks.len().saturating_sub(idx) + retry_len
    }
}

/// Re-queues chunk on drop if not disarmed. Prevents chunk loss on panic/task-cancel/any exit.
pub(crate) struct ChunkGuard {
    chunk: Option<ChunkWorkItem>,
    peer_id: Option<String>,
    assigner: Arc<ChunkAssigner>,
}

impl ChunkGuard {
    pub(crate) fn new(
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
    pub(crate) fn disarm(&mut self) {
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
