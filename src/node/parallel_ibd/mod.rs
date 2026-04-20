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

mod blocks;
mod checkpoints;
mod chunk_assigner;
mod download;
mod feeder;
mod headers;
mod memory;
mod prefetch;
mod types;
#[cfg(feature = "production")]
mod validation_loop;

use chunk_assigner::{create_chunks as create_chunks_impl, ChunkAssigner, ChunkGuard};

pub use chunk_assigner::BlockChunk;
use download::download_chunk;
use feeder::{new_feeder_state, run_feeder_thread};
use memory::{MemoryGuard, TIDESDB_MAX_TXN_OPS};
#[cfg(feature = "production")]
use types::PrefetchWorkItemV2;
use types::{ChunkWorkItem, FeederBufferValue, ReadyItem};

use crate::network::inventory::MSG_BLOCK;
use crate::network::peer_scoring::is_lan_peer;
use crate::network::protocol::{
    GetHeadersMessage, HeadersMessage, ProtocolMessage, ProtocolParser,
};
use crate::network::NetworkManager;
use crate::node::block_processor::validate_block_with_context;
use crate::storage::blockstore::{block_height_row_key, BlockMetadata, BlockStore};
use crate::storage::database::Tree;
use crate::storage::disk_utxo::{
    block_input_keys_and_tx_ids_filtered, block_input_keys_batch_into_arc, key_to_outpoint,
    outpoint_to_key, OutPointKey, SyncBatch,
};
#[cfg(feature = "production")]
use crate::storage::ibd_utxo_store::IbdUtxoStore;
use crate::storage::Storage;
use crate::utils::{IBD_YIELD_SLEEP, MESSAGE_PROCESSOR_POLL_SLEEP};
use anyhow::{Context, Result};
use blvm_protocol::bip_validation::Bip30Index;
use blvm_protocol::{
    segwit::Witness, BitcoinProtocolEngine, Block, BlockHeader, Hash, UtxoSet, ValidationResult,
};

use blvm_protocol::serialization::varint::decode_varint;
use blvm_protocol::types::{OutPoint, UTXO};
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

/// Parallel IBD configuration
#[derive(Debug, Clone)]
pub struct ParallelIBDConfig {
    /// Number of parallel workers (default: CPU count)
    pub num_workers: usize,
    /// Chunk size in blocks (default: 16)
    pub chunk_size: u64,
    /// Maximum concurrent downloads per peer (default: 64)
    pub max_concurrent_per_peer: usize,
    /// Checkpoint interval in blocks (default: 10,000)
    pub checkpoint_interval: u64,
    /// Timeout for block download in seconds (default: 30)
    pub download_timeout_secs: u64,
    /// Preferred peer addresses (ENV > config > empty)
    pub preferred_peers: Vec<String>,
    /// Mode: parallel, sequential, earliest (default: parallel)
    pub mode: String,
    /// Max blocks download can race ahead (None = auto from RAM)
    pub max_ahead_blocks: Option<u64>,
    /// Skip disk reads during IBD from genesis (default: false)
    pub memory_only: bool,
    /// Failure dump directory (None = platform temp)
    pub dump_dir: Option<String>,
    /// Snapshot directory for debug dumps (None = unset)
    pub snapshot_dir: Option<String>,
    /// Tokio yield interval (default: 1000)
    pub yield_interval: u64,
    /// Eviction: dynamic, fifo, lifo (default: fifo)
    pub eviction: String,
    /// Assign all chunks to fastest peer (default: false)
    pub earliest_first: bool,
    /// Prefetch workers (None = auto from nproc)
    pub prefetch_workers: Option<usize>,
    /// Prefetch queue size (None = auto)
    pub prefetch_queue_size: Option<usize>,
    /// UTXO prefetch lookahead (default: 64)
    pub utxo_prefetch_lookahead: u64,
    /// Max blocks in transit per peer (default: 16)
    pub max_blocks_in_transit_per_peer: usize,
    /// Headers download timeout (seconds, default: 30)
    pub headers_timeout_secs: u64,
    /// Headers max failures before peer switch (default: 10)
    pub headers_max_failures: u32,
}

impl Default for ParallelIBDConfig {
    fn default() -> Self {
        Self::from_config(None)
    }
}

impl ParallelIBDConfig {
    /// Build config from optional IbdConfig. ENV overrides config file.
    pub fn from_config(ibd_config: Option<&crate::config::IbdConfig>) -> Self {
        let chunk_size = std::env::var("BLVM_IBD_CHUNK_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(|n: u64| n.clamp(16, 2000))
            .or_else(|| ibd_config.map(|c| c.chunk_size))
            .unwrap_or(16);
        let download_timeout_secs = std::env::var("BLVM_IBD_DOWNLOAD_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.map(|c| c.download_timeout_secs))
            .unwrap_or(30);
        let preferred_peers = std::env::var("BLVM_IBD_PEERS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect()
            })
            .or_else(|| {
                ibd_config
                    .filter(|c| !c.preferred_peers.is_empty())
                    .map(|c| c.preferred_peers.clone())
            })
            .unwrap_or_default();
        let mode = std::env::var("BLVM_IBD_MODE")
            .ok()
            .or_else(|| ibd_config.map(|c| c.mode.clone()))
            .unwrap_or_else(|| "parallel".to_string());
        let max_ahead_blocks = std::env::var("BLVM_IBD_MAX_AHEAD")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.and_then(|c| c.max_ahead_blocks));
        let memory_only = std::env::var("BLVM_IBD_MEMORY_ONLY")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or_else(|_| ibd_config.map(|c| c.memory_only).unwrap_or(false));
        let dump_dir = std::env::var("BLVM_IBD_DUMP_DIR")
            .ok()
            .or_else(|| ibd_config.and_then(|c| c.dump_dir.clone()));
        let snapshot_dir = std::env::var("BLVM_IBD_SNAPSHOT_DIR")
            .ok()
            .or_else(|| ibd_config.and_then(|c| c.snapshot_dir.clone()));
        let yield_interval = std::env::var("BLVM_IBD_YIELD_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.map(|c| c.yield_interval))
            .unwrap_or(1000);
        let eviction = std::env::var("BLVM_IBD_EVICTION")
            .ok()
            .or_else(|| ibd_config.map(|c| c.eviction.clone()))
            .unwrap_or_else(|| "fifo".to_string());
        let earliest_first = std::env::var("BLVM_IBD_EARLIEST_FIRST")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or_else(|_| ibd_config.map(|c| c.earliest_first).unwrap_or(false));
        let prefetch_workers = std::env::var("BLVM_PREFETCH_WORKERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n: &usize| n > 0 && n <= 64)
            .or_else(|| ibd_config.and_then(|c| c.prefetch_workers));
        let prefetch_queue_size = std::env::var("BLVM_PREFETCH_QUEUE_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.and_then(|c| c.prefetch_queue_size));
        let utxo_prefetch_lookahead = std::env::var("BLVM_UTXO_PREFETCH_LOOKAHEAD")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| ibd_config.map(|c| c.utxo_prefetch_lookahead))
            .unwrap_or(128)
            .clamp(1, 128);
        let max_blocks_in_transit = std::env::var("BLVM_IBD_MAX_BLOCKS_IN_TRANSIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.map(|c| c.max_blocks_in_transit_per_peer))
            .unwrap_or(16);
        let headers_timeout = std::env::var("BLVM_IBD_HEADERS_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.map(|c| c.headers_timeout_secs))
            .unwrap_or(30);
        let headers_max_failures = std::env::var("BLVM_IBD_HEADERS_MAX_FAILURES")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.map(|c| c.headers_max_failures))
            .unwrap_or(10);
        Self {
            num_workers: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            chunk_size,
            max_concurrent_per_peer: 64,
            checkpoint_interval: 10_000,
            download_timeout_secs,
            preferred_peers,
            mode,
            max_ahead_blocks,
            memory_only,
            dump_dir,
            snapshot_dir,
            yield_interval,
            eviction,
            earliest_first,
            prefetch_workers,
            prefetch_queue_size,
            utxo_prefetch_lookahead,
            max_blocks_in_transit_per_peer: max_blocks_in_transit,
            headers_timeout_secs: headers_timeout,
            headers_max_failures,
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

/// Parallel IBD coordinator
pub struct ParallelIBD {
    config: ParallelIBDConfig,
    /// Earliest BIP54 activation height from version-bits lock-in along the validated chain (mainnet).
    bip54_activation_from_version_bits: parking_lot::Mutex<Option<u64>>,
    /// Semaphore to limit concurrent chunk downloads per peer
    peer_semaphores: Arc<HashMap<String, Arc<Semaphore>>>,
    /// Core-style: max blocks in flight per peer (shared across all workers). Prevents 6 workers × 64 pipeline = 384 requests to one peer.
    peer_blocks_semaphores: Arc<HashMap<String, Arc<Semaphore>>>,
    /// Peer scorer for bandwidth-based peer selection
    peer_scorer: Arc<crate::network::peer_scoring::PeerScorer>,
}

impl ParallelIBD {
    /// Create a new parallel IBD coordinator
    pub fn new(config: ParallelIBDConfig) -> Self {
        Self {
            config,
            bip54_activation_from_version_bits: parking_lot::Mutex::new(None),
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
                Arc::new(Semaphore::new(self.config.max_blocks_in_transit_per_peer)),
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
        event_publisher: Option<Arc<crate::node::event_publisher::EventPublisher>>,
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

        let headers_start = std::time::Instant::now();

        // Step 1: Download headers first (sequential, but fast)
        // This will iteratively download headers until chain tip is reached
        if let Some(ref ep) = event_publisher {
            ep.publish_headers_sync_started(start_height).await;
        }
        info!("Downloading headers...");
        let network_for_headers = network.clone();
        let actual_synced_height = headers::download_headers(
            self.peer_scorer.clone(),
            start_height,
            target_height,
            peer_ids,
            &blockstore,
            network_for_headers,
            self.config.headers_timeout_secs,
            self.config.headers_max_failures,
            event_publisher.clone(),
        )
        .await
        .context("Failed to download headers")?;

        if let Some(ref ep) = event_publisher {
            let duration_secs = headers_start.elapsed().as_secs();
            ep.publish_headers_sync_completed(actual_synced_height, duration_secs)
                .await;
        }

        // Use the actual synced height (may be less than target_height if we reached chain tip)
        let effective_end_height = actual_synced_height.min(target_height);
        info!(
            "Headers synced up to height {}, will download blocks for heights {} to {}",
            actual_synced_height, start_height, effective_end_height
        );

        // Peers have no blocks past our header tip (e.g. both at genesis). Nothing to fetch;
        // exit IBD so the node can run the main loop and pick up new blocks via relay/sync.
        if effective_end_height < start_height {
            info!(
                "Parallel IBD: no block range to download (end {} < start {}); treating as caught up to peer tip",
                effective_end_height, start_height
            );
            return Ok(());
        }

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

        // preferred_peers: restrict IBD to these peers only (must be connected).
        // Matches "192.168.2.100" to "192.168.2.100:8333" (preferred without port matches peer with port).
        let preferred: &[String] = &self.config.preferred_peers;
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
                    "preferred_peers={:?} but none are connected. Connected: {:?}",
                    preferred,
                    peer_ids
                ));
            }
            filtered_peers = matched;
            info!(
                "IBD preferred_peers: using {} ({})",
                filtered_peers.len(),
                filtered_peers.join(", ")
            );
        }

        // mode=sequential: single-peer mode. Core-like earliest-first, no chunk-boundary stalls.
        let ibd_mode: &str = &self.config.mode;
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

        // Matches per-peer worker_count below: (2×priority).clamp(2, 6) per peer.
        let total_download_workers: usize = filtered_peers
            .iter()
            .map(|peer_id| {
                let priority = scored_peers
                    .iter()
                    .find(|(p, _)| p == peer_id)
                    .map(|(_, s)| *s)
                    .unwrap_or(1.0);
                ((2.0 * priority) as usize).clamp(2, 6)
            })
            .sum::<usize>()
            .max(1);

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

        let block_sync_start = std::time::Instant::now();
        if let Some(ref ep) = event_publisher {
            ep.publish_block_sync_started(start_height, effective_end_height)
                .await;
        }

        // Step 3: Streaming download + validation
        //
        // Bounded channel from download workers → coordinator: **required** for RAM safety.
        // Unbounded `mpsc` let WAN workers flood full blocks while the coordinator was busy,
        // causing kernel OOM on 16GiB hosts (each queued item holds a full `Block`).

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

        // Auto-tune memory **before** allocating download queue (capacity uses buffer_limit).
        let mut mem_guard = MemoryGuard::new();
        // Bounded download→coordinator queue + safety valve (see docs/IBD_AGENT_HANDOFF §2.4):
        // 1) Tokio bounded channel → backpressure when coordinator is slow (workers await send).
        // 2) RAM-tier ceiling → cap autosize so high worker×pipeline estimates do not create a
        //    huge queued-block arena on ≤16/32 GiB hosts.
        // 3) Optional BLVM_IBD_DOWNLOAD_QUEUE_MAX_BLOCKS → operator hard cap (min with computed).
        let download_block_queue_cap: usize = {
            let bl = mem_guard.buffer_limit(start_height);
            let pipeline = self
                .config
                .max_concurrent_per_peer
                .max(self.config.max_blocks_in_transit_per_peer);
            const PIPELINE_HORIZON_FOR_CAP: usize = 32;
            let h = pipeline.min(PIPELINE_HORIZON_FOR_CAP).max(1);
            let base = bl.saturating_mul(4);
            let parallel = total_download_workers.saturating_mul(h).saturating_mul(2);
            let floor = if mem_guard.system_total_ram_mb() <= 16 * 1024 {
                128
            } else {
                256
            };
            let raw = base.max(parallel).clamp(floor, 8192);
            let ram_ceiling = match mem_guard.system_total_ram_mb() {
                m if m <= 16 * 1024 => 2048,
                m if m <= 32 * 1024 => 4096,
                _ => 8192,
            };
            let env_cap = std::env::var("BLVM_IBD_DOWNLOAD_QUEUE_MAX_BLOCKS")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&n| n > 0);
            let capped = raw.min(ram_ceiling);
            match env_cap {
                Some(e) => capped.min(e).max(floor),
                None => capped.max(floor),
            }
        };
        let (block_tx, mut block_rx) =
            tokio::sync::mpsc::channel::<(u64, Block, Vec<Vec<Witness>>)>(download_block_queue_cap);
        info!(
            "IBD: download→coordinator channel capacity={} blocks (buffer_limit={}, workers={}, bounded + RAM/env valve)",
            download_block_queue_cap,
            mem_guard.buffer_limit(start_height),
            total_download_workers,
        );
        let (stall_tx, _) = broadcast::channel::<u64>(16);

        // Last block height whose UTXO effects are visible to the coordinator/prefetch path.
        // start_height is the *next* block to validate → parent is start_height - 1 (synced tip).
        let validation_height = Arc::new(AtomicU64::new(start_height.saturating_sub(1)));
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

        let effective_max_entries = mem_guard.utxo_max_entries;
        let utxo_flush_threshold = mem_guard.utxo_flush_threshold;
        // Max blocks download can race ahead of validation. Limits block_rx channel depth.
        let max_ahead_blocks: u64 = self
            .config
            .max_ahead_blocks
            .unwrap_or(mem_guard.max_ahead_blocks);
        let max_ahead_live = Arc::new(AtomicU64::new(max_ahead_blocks));
        // IBD v2 (IbdUtxoStore) is the only path. Storage is guaranteed Some (checked at start).
        let ibd_memory_only: bool = self.config.memory_only && start_height <= 1;
        let ibd_store_v2: Arc<IbdUtxoStore> = {
            let tree = storage
                .open_tree("ibd_utxos")
                .context("Failed to open IBD UTXO tree")?;
            info!(
                "IBD v2: IbdUtxoStore (DashMap, zero lock, max_cache={} entries)",
                effective_max_entries
            );
            let eviction =
                crate::storage::ibd_utxo_store::EvictionStrategy::from_str(&self.config.eviction);
            let utxo_disk_baseline = storage
                .chain()
                .get_utxo_watermark()
                .ok()
                .flatten()
                .unwrap_or_else(|| start_height.saturating_sub(1));
            let store = Arc::new(IbdUtxoStore::new_with_options(
                tree,
                utxo_flush_threshold,
                ibd_memory_only,
                effective_max_entries,
                eviction,
                utxo_disk_baseline,
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
            let config_val = self.config.prefetch_queue_size;
            let guard_limit = mem_guard.prefetch_queue_size;
            match config_val {
                Some(v) if v <= guard_limit => v,
                Some(v) => {
                    info!(
                        "prefetch_queue_size={} exceeds MemoryGuard limit {}; capping",
                        v, guard_limit
                    );
                    guard_limit
                }
                None => guard_limit,
            }
        };
        let prefetch_workers: usize = self.config.prefetch_workers.unwrap_or_else(|| {
            let n = std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(8);
            (n.saturating_mul(2)).clamp(4, 24)
        });
        let gap_fill_workers: usize = prefetch_workers;
        let (prefetch_input_tx_v2, gap_fill_tx_v2, ready_tx_for_coord, ready_rx) = {
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
                    prefetch::run_prefetch_worker(rx_clone, tx_clone, store)
                });
            }
            for _ in 0..gap_fill_workers {
                let rx_clone = gap_rx_v2.clone();
                let tx_clone = out_tx.clone();
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    prefetch::run_prefetch_worker(rx_clone, tx_clone, store)
                });
            }
            info!(
                "IBD v2 prefetch: {} workers, queue={}; gap-fill overflow: {} workers (direct to feeder)",
                prefetch_workers,
                max_prefetches_in_flight,
                gap_fill_workers
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
        let ibd_protocol_version = protocol.get_protocol_version();

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
                let max_ahead_live_clone = Arc::clone(&max_ahead_live);
                let ibd_pv = ibd_protocol_version;
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
                            if let Some((chunk_start, chunk_end)) = assigner_clone.get_work(
                                &peer_id,
                                max_ahead_live_clone.load(std::sync::atomic::Ordering::Relaxed),
                            ) {
                                break Some((chunk_start, chunk_end));
                            }
                            if assigner_clone.is_done() {
                                break None;
                            }
                            tokio::time::sleep(MESSAGE_PROCESSOR_POLL_SLEEP).await;
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
                        let dl_result = download_chunk(
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
                            ibd_pv,
                        )
                        .await;
                        workers_current_clone
                            .lock()
                            .await
                            .retain(|(p, s, _)| !(*p == peer_id && *s == start));
                        match dl_result {
                            Ok(chunk) => {
                                consecutive_failures = 0;
                                let block_count = chunk.block_count();
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
                                    blvm_protocol::profile_log!(
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
                                    blvm_protocol::profile_log!(
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

                                if num_peers_clone > 1
                                    && consecutive_failures >= MAX_CONSECUTIVE_FAILURES
                                {
                                    warn!(
                                        "Peer {} exceeded max failures, stopping worker",
                                        peer_id
                                    );
                                    break;
                                }

                                if num_peers_clone == 1
                                    && consecutive_failures >= MAX_CONSECUTIVE_FAILURES
                                {
                                    // Single peer: wait for reconnect instead of killing the worker.
                                    // The reconnect logic in peer_connections / background_tasks will
                                    // re-establish the TCP session; we just need to stay alive.
                                    warn!(
                                        "Peer {} at {} consecutive failures (single-peer mode) — waiting for reconnect",
                                        peer_id, consecutive_failures
                                    );
                                    let wait_secs = 10u64;
                                    tokio::time::sleep(std::time::Duration::from_secs(wait_secs))
                                        .await;
                                    // After waiting, try to get work again — peer may be back.
                                    // Reset failure count so we get fresh retries.
                                    consecutive_failures = 0;
                                    continue;
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

        // COORDINATOR: Drains block_rx, sends to prefetch. When prefetch and gap-fill are full,
        // pushes to the feeder with an empty UTXO map so the coordinator never blocks on workers
        // (keeps block_rx + buffer draining; validation supplements UTXOs on-thread when needed).
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
        let coord_buffer_limit = mem_guard.buffer_limit(start_height);
        let gap_fill_tx_v2_for_coord = gap_fill_tx_v2.clone();
        let prefetch_input_tx_v2_for_coord = prefetch_input_tx_v2.clone();
        let ibd_store_v2_for_coord = Arc::clone(&ibd_store_v2);
        let stall_tx_for_coord = stall_tx.clone();
        // Seq-1: When single peer (BLVM_IBD_SEQUENTIAL), blocks arrive in order; skip reorder_buffer.
        let sequential = num_peers == 1;
        if sequential {
            info!("Coordinator: sequential mode (single peer) — passthrough, no reorder buffer");
        }
        let ready_tx_coord = ready_tx_for_coord;
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
            let mut coord_tx_ids_buf: Vec<Hash> = Vec::new();
            info!("Coordinator: started, awaiting blocks from download workers");
            const COORD_STALL_LOG_SECS: u64 = 30;
            let mut coord_buffer_full_since: Option<std::time::Instant> = None;
            #[cfg(target_os = "linux")]
            let mut coord_emergency_log = std::time::Instant::now();
            loop {
                let dynamic_buffer_limit = coord_buffer_limit;
                // §9 L3: do not drain block_rx while validation reports Emergency — WAN workers block on send().
                #[cfg(target_os = "linux")]
                {
                    while memory::ibd_pressure_is_emergency() {
                        if coord_emergency_log.elapsed() > Duration::from_secs(5) {
                            warn!(
                                "Coordinator: EMERGENCY admission pause — not draining block_rx until memory recovers"
                            );
                            coord_emergency_log = std::time::Instant::now();
                        }
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
                // Backpressure: when reorder_buffer full, drain contiguous blocks before receiving more.
                // Prevents unbounded growth when downloads outpace validation.
                if reorder_buffer.len() >= dynamic_buffer_limit {
                    while reorder_buffer.contains_key(&next_prefetch_height) {
                        let (block, witnesses) = reorder_buffer
                            .remove(&next_prefetch_height)
                            .expect("contains_key");
                        block_input_keys_and_tx_ids_filtered(
                            &block,
                            &mut coord_tx_ids_buf,
                            &mut coord_keys_buf,
                        );
                        let h = next_prefetch_height;
                        next_prefetch_height += 1;
                        if h == bootstrap_end {
                            assigner_for_coord.mark_bootstrap_complete();
                            info!("IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled", h);
                        }
                        {
                            let store = &ibd_store_v2_for_coord;
                            let keys_owned = std::mem::take(&mut coord_keys_buf);
                            let tx_ids_owned = std::mem::take(&mut coord_tx_ids_buf);
                            let item = (
                                Arc::clone(store),
                                keys_owned,
                                tx_ids_owned,
                                h,
                                block,
                                witnesses,
                            );
                            let next_needed =
                                validation_height_for_coord.load(Ordering::Relaxed) + 1;
                            let route_to_gap_fill = h <= next_needed;
                            let ptx = &prefetch_input_tx_v2_for_coord;
                            tokio::task::block_in_place(|| {
                                if route_to_gap_fill {
                                    if let Err(e) = gap_fill_tx_v2_for_coord.try_send(item) {
                                        let (_, _, tx_ids, hb, b, w) = e.into_inner();
                                        let _ = ready_tx_coord.send((
                                            hb,
                                            b,
                                            w,
                                            Vec::new(),
                                            rustc_hash::FxHashMap::default(),
                                            tx_ids,
                                        ));
                                    }
                                } else if let Err(e) = ptx.try_send(item) {
                                    let back = e.into_inner();
                                    if let Err(e2) = gap_fill_tx_v2_for_coord.try_send(back) {
                                        let (_, _, tx_ids, hb, b, w) = e2.into_inner();
                                        let _ = ready_tx_coord.send((
                                            hb,
                                            b,
                                            w,
                                            Vec::new(),
                                            rustc_hash::FxHashMap::default(),
                                            tx_ids,
                                        ));
                                    }
                                }
                            });
                        }
                        if reorder_buffer.len() < dynamic_buffer_limit {
                            break;
                        }
                    }
                    if reorder_buffer.len() >= dynamic_buffer_limit
                        && !reorder_buffer.contains_key(&next_prefetch_height)
                    {
                        // Gap: next_prefetch_height is missing. Drain block_rx
                        // to see if it arrived after the buffer filled.
                        let mut found_missing = false;
                        let mut gap_drained = 0usize;
                        while let Ok((h, block, witnesses)) = block_rx.try_recv() {
                            total_received += 1;
                            if h == next_prefetch_height {
                                found_missing = true;
                            }
                            reorder_buffer.insert(h, (block, witnesses));
                            gap_drained += 1;
                            if found_missing || gap_drained >= 200 {
                                break;
                            }
                        }
                        if !found_missing {
                            // Still missing after draining channel. Track how long.
                            let now = std::time::Instant::now();
                            let stall_start = *coord_buffer_full_since.get_or_insert(now);
                            let stuck_secs = now.duration_since(stall_start).as_secs();
                            if stuck_secs >= COORD_STALL_LOG_SECS {
                                warn!(
                                    "Coordinator stall: buffer full ({}) but height {} missing for {}s (drained {} from rx), signalling retry",
                                    reorder_buffer.len(), next_prefetch_height, stuck_secs, gap_drained
                                );
                                let _ = stall_tx_for_coord.send(next_prefetch_height);
                                assigner_for_coord
                                    .requeue_chunk_containing_height(next_prefetch_height);
                                coord_buffer_full_since = None;
                            }
                            tokio::time::sleep(IBD_YIELD_SLEEP).await;
                        } else {
                            coord_buffer_full_since = None;
                            // Found the missing block — loop will drain contiguous blocks next iteration
                        }
                    } else if reorder_buffer.len() >= dynamic_buffer_limit {
                        coord_buffer_full_since = None;
                        tokio::time::sleep(IBD_YIELD_SLEEP).await;
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
                        assigner_for_coord.requeue_chunk_containing_height(next_needed);
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
                        block_input_keys_and_tx_ids_filtered(
                            &block,
                            &mut coord_tx_ids_buf,
                            &mut coord_keys_buf,
                        );
                        let h = next_prefetch_height;
                        next_prefetch_height += 1;
                        if h == bootstrap_end {
                            assigner_for_coord.mark_bootstrap_complete();
                            info!("IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled", h);
                        }
                        {
                            let store = &ibd_store_v2_for_coord;
                            let keys_owned = std::mem::take(&mut coord_keys_buf);
                            let tx_ids_owned = std::mem::take(&mut coord_tx_ids_buf);
                            let item = (
                                Arc::clone(store),
                                keys_owned,
                                tx_ids_owned,
                                h,
                                block,
                                witnesses,
                            );
                            let next_needed =
                                validation_height_for_coord.load(Ordering::Relaxed) + 1;
                            let route_to_gap_fill = h <= next_needed;
                            let ptx = &prefetch_input_tx_v2_for_coord;
                            tokio::task::block_in_place(|| {
                                if route_to_gap_fill {
                                    if let Err(e) = gap_fill_tx_v2_for_coord.try_send(item) {
                                        let (_, _, tx_ids, hb, b, w) = e.into_inner();
                                        let _ = ready_tx_coord.send((
                                            hb,
                                            b,
                                            w,
                                            Vec::new(),
                                            rustc_hash::FxHashMap::default(),
                                            tx_ids,
                                        ));
                                    }
                                } else if let Err(e) = ptx.try_send(item) {
                                    let back = e.into_inner();
                                    if let Err(e2) = gap_fill_tx_v2_for_coord.try_send(back) {
                                        let (_, _, tx_ids, hb, b, w) = e2.into_inner();
                                        let _ = ready_tx_coord.send((
                                            hb,
                                            b,
                                            w,
                                            Vec::new(),
                                            rustc_hash::FxHashMap::default(),
                                            tx_ids,
                                        ));
                                    }
                                }
                            });
                        }
                    }
                    drop(ready_tx_coord);
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
                        blvm_protocol::block::compute_block_tx_ids_into(
                            &block,
                            &mut coord_tx_ids_buf,
                        );
                        // Single-peer path: no prefetch overlap; validation builds the UTXO base map
                        // on-thread (same as a prefetch miss, but steady state at low BPS).
                        let _ = tokio::task::block_in_place(|| {
                            ready_tx_coord.send((
                                h,
                                block,
                                witnesses,
                                Vec::new(),
                                rustc_hash::FxHashMap::default(),
                                std::mem::take(&mut coord_tx_ids_buf),
                            ))
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
                        block_input_keys_and_tx_ids_filtered(
                            &block,
                            &mut coord_tx_ids_buf,
                            &mut coord_keys_buf,
                        );
                        let h = next_prefetch_height;
                        next_prefetch_height += 1;
                        if h == bootstrap_end {
                            assigner_for_coord.mark_bootstrap_complete();
                            info!("IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled", h);
                        }
                        {
                            let store = &ibd_store_v2_for_coord;
                            let keys_owned = std::mem::take(&mut coord_keys_buf);
                            let tx_ids_owned = std::mem::take(&mut coord_tx_ids_buf);
                            let item = (
                                Arc::clone(store),
                                keys_owned,
                                tx_ids_owned,
                                h,
                                block,
                                witnesses,
                            );
                            let next_needed =
                                validation_height_for_coord.load(Ordering::Relaxed) + 1;
                            let route_to_gap_fill = h <= next_needed;
                            let ptx = &prefetch_input_tx_v2_for_coord;
                            tokio::task::block_in_place(|| {
                                if route_to_gap_fill {
                                    if let Err(e) = gap_fill_tx_v2_for_coord.try_send(item) {
                                        let (_, _, tx_ids, hb, b, w) = e.into_inner();
                                        let _ = ready_tx_coord.send((
                                            hb,
                                            b,
                                            w,
                                            Vec::new(),
                                            rustc_hash::FxHashMap::default(),
                                            tx_ids,
                                        ));
                                    }
                                } else if let Err(e) = ptx.try_send(item) {
                                    let back = e.into_inner();
                                    if let Err(e2) = gap_fill_tx_v2_for_coord.try_send(back) {
                                        let (_, _, tx_ids, hb, b, w) = e2.into_inner();
                                        let _ = ready_tx_coord.send((
                                            hb,
                                            b,
                                            w,
                                            Vec::new(),
                                            rustc_hash::FxHashMap::default(),
                                            tx_ids,
                                        ));
                                    }
                                }
                            });
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
        let feeder_state = new_feeder_state();
        let feeder_buffer_limit = mem_guard.buffer_limit(start_height);
        let feeder_buffer_bytes_limit = mem_guard.feeder_buffer_bytes_limit;
        let feeder_handle = run_feeder_thread(
            ready_rx,
            Arc::clone(&feeder_state),
            feeder_buffer_limit,
            feeder_buffer_bytes_limit,
        );

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
        let utxo_nominal_max_entries = mem_guard.utxo_max_entries;
        let utxo_pf = self.config.utxo_prefetch_lookahead.clamp(1, 128) as usize;
        let params = validation_loop::ValidationParams {
            feeder_state: feeder_state_valid,
            ibd_store: ibd_store_v2_valid,
            blockstore: blockstore_valid,
            storage: storage_clone_valid,
            parallel_ibd: self_clone_valid,
            protocol: protocol_valid,
            utxo_mutex: utxo_mutex_valid,
            effective_end_height,
            start_height,
            validation_height: Arc::clone(&validation_height),
            mem_guard,
            max_ahead_live: Arc::clone(&max_ahead_live),
            nominal_max_ahead: max_ahead_blocks,
            utxo_nominal_max_entries,
            utxo_prefetch_lookahead: utxo_pf,
        };
        let validation_handle =
            std::thread::spawn(move || validation_loop::run_validation_loop(params));

        // Spawn BlockSyncProgress publisher — polls validation_height every 2s for module event subscribers
        let progress_handle = if let Some(ref ep) = event_publisher {
            let ep = Arc::clone(ep);
            let vh = Arc::clone(&validation_height);
            let start = start_height;
            let end = effective_end_height;
            let sync_start = block_sync_start;
            Some(tokio::spawn(async move {
                let mut last_height = start;
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let current = vh.load(Ordering::Relaxed);
                    if current > last_height {
                        let elapsed = sync_start.elapsed().as_secs_f64();
                        let progress_percent = if end > start && elapsed > 0.0 {
                            ((current - start) as f64 / (end - start + 1) as f64) * 100.0
                        } else {
                            0.0
                        };
                        let blocks_per_second = if elapsed > 0.0 {
                            (current - start) as f64 / elapsed
                        } else {
                            0.0
                        };
                        ep.publish_block_sync_progress(
                            current,
                            end,
                            progress_percent,
                            blocks_per_second,
                        )
                        .await;
                        last_height = current;
                    }
                    if current >= end {
                        break;
                    }
                }
            }))
        } else {
            None
        };

        // Wait for validation thread (block_in_place keeps tokio worker free)
        match tokio::task::block_in_place(|| validation_handle.join()) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(panic) => return Err(anyhow::anyhow!("Validation thread panicked: {:?}", panic)),
        }
        if let Some(h) = progress_handle {
            let _ = h.await;
        }
        // Feeder exits when ready_rx disconnects; join to avoid stray thread
        let _ = feeder_handle.join();
        *utxo_set = match Arc::try_unwrap(utxo_mutex) {
            Ok(mutex) => mutex
                .into_inner()
                .map_err(|e| anyhow::anyhow!("IBD UTXO mutex poisoned: {e:?}"))?,
            Err(arc) => arc
                .lock()
                .map_err(|e| anyhow::anyhow!("IBD UTXO mutex poisoned: {e:?}"))?
                .clone(),
        };

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
        if let Some(ref ep) = event_publisher {
            let duration_secs = block_sync_start.elapsed().as_secs();
            ep.publish_block_sync_completed(effective_end_height, duration_secs)
                .await;
        }
        Ok(())
    }

    /// Create chunks for parallel download (delegates to chunk_assigner).
    pub fn create_chunks(
        &self,
        start_height: u64,
        end_height: u64,
        peer_ids: &[String],
        scored_peers: Option<&[(String, f64)]>,
    ) -> Vec<BlockChunk> {
        create_chunks_impl(
            &self.config,
            start_height,
            end_height,
            peer_ids,
            scored_peers,
        )
    }

    /// Returns pre-computed tx_ids so the caller avoids redundant double-SHA256.
    /// network_time: cached at loop init, refreshed every 1000 blocks (avoids per-block SystemTime syscall).
    /// bip30_index: O(1) duplicate-coinbase check; when Some, updated during apply_transaction.
    /// When BIP54 is active and height is at a period boundary (N % 2016 in {0, 2015}), boundary
    /// timestamps are read from blockstore so timewarp checks can run; otherwise None.
    #[inline]
    pub(crate) fn validate_block_only<'a>(
        &self,
        blockstore: &BlockStore,
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
        precomputed_tx_ids: Option<&'a [Hash]>,
    ) -> Result<(
        std::borrow::Cow<'a, [Hash]>,
        Option<blvm_protocol::block::UtxoDelta>,
    )> {
        // BIP54 activation from version bits when miners signal (no fixed height required).
        // Merge candidates with `min` so an earlier period’s lock-in is not overwritten by a
        // later window’s larger computed activation height (see `version_bits` module docs).
        let candidate = recent_headers.and_then(|hdr| {
            if hdr.len() >= blvm_protocol::version_bits::LOCK_IN_PERIOD as usize {
                blvm_protocol::version_bits::activation_height_from_headers(
                    hdr,
                    height,
                    network_time,
                    &blvm_protocol::version_bits::bip54_deployment_mainnet(),
                )
            } else {
                None
            }
        });
        let bip54_activation_override = {
            let mut g = self.bip54_activation_from_version_bits.lock();
            *g = blvm_protocol::version_bits::merge_bip54_activation_candidate(*g, candidate);
            *g
        };

        let bip54_active = blvm_protocol::bip_validation::is_bip54_active_at(
            height,
            blvm_protocol::types::Network::Mainnet,
            bip54_activation_override,
        );
        let bip54_boundary = if bip54_active {
            let rem = height % 2016;
            if rem == 0 || rem == 2015 {
                let ts_n_minus_1 = blockstore
                    .get_hash_by_height(height.saturating_sub(1))
                    .ok()
                    .flatten()
                    .and_then(|h| blockstore.get_header(&h).ok().flatten())
                    .map(|h| h.timestamp);
                let ts_n_minus_2015 = if height >= 2015 {
                    blockstore
                        .get_hash_by_height(height - 2015)
                        .ok()
                        .flatten()
                        .and_then(|h| blockstore.get_header(&h).ok().flatten())
                        .map(|h| h.timestamp)
                } else {
                    None
                };
                match (ts_n_minus_1, ts_n_minus_2015) {
                    (Some(a), Some(b)) => Some(blvm_protocol::types::Bip54BoundaryTimestamps {
                        timestamp_n_minus_1: a,
                        timestamp_n_minus_2015: b,
                    }),
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        };

        let context = blvm_protocol::block::BlockValidationContext::from_connect_block_ibd_args(
            recent_headers,
            network_time,
            blvm_protocol::types::Network::Mainnet,
            bip54_activation_override,
            bip54_boundary,
        );
        let owned_utxo = std::mem::take(utxo_set);
        let (result, new_utxo_set, tx_ids, utxo_delta) = blvm_protocol::block::connect_block_ibd(
            block,
            witnesses,
            owned_utxo,
            height,
            &context,
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
    pub(crate) fn dump_failed_block(
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
    pub(crate) fn dump_ibd_snapshot(
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

        let mut success = true;
        if let Ok(f) = std::fs::File::create(&block_path) {
            if let Err(e) = bincode::serialize_into(std::io::BufWriter::new(f), block) {
                error!(
                    "IBD_SNAPSHOT: Failed to serialize block to {}: {}",
                    block_path.display(),
                    e
                );
                success = false;
            }
        } else {
            error!("IBD_SNAPSHOT: Failed to create {}", block_path.display());
            success = false;
        }
        if let Ok(f) = std::fs::File::create(&witnesses_path) {
            if let Err(e) = bincode::serialize_into(std::io::BufWriter::new(f), witnesses) {
                error!(
                    "IBD_SNAPSHOT: Failed to serialize witnesses to {}: {}",
                    witnesses_path.display(),
                    e
                );
                success = false;
            }
        } else {
            error!(
                "IBD_SNAPSHOT: Failed to create {}",
                witnesses_path.display()
            );
            success = false;
        }
        if let Ok(f) = std::fs::File::create(&utxo_path) {
            let serializable: std::collections::HashMap<_, _> =
                utxo_set.iter().map(|(k, v)| (*k, (**v).clone())).collect();
            if let Err(e) = bincode::serialize_into(std::io::BufWriter::new(f), &serializable) {
                error!(
                    "IBD_SNAPSHOT: Failed to serialize utxo_set to {}: {}",
                    utxo_path.display(),
                    e
                );
                success = false;
            }
        } else {
            error!("IBD_SNAPSHOT: Failed to create {}", utxo_path.display());
            success = false;
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
        if let Err(e) = std::fs::write(&info_path, info) {
            error!(
                "IBD_SNAPSHOT: Failed to write {}: {}",
                info_path.display(),
                e
            );
            success = false;
        }
        if success {
            info!(
                "IBD_SNAPSHOT: Block {} dumped to: {} (block.bin, witnesses.bin, utxo_set.bin, info.txt)",
                height,
                dir.display()
            );
        } else {
            warn!(
                "IBD_SNAPSHOT: Block {} dump incomplete or failed (see errors above)",
                height
            );
        }
    }

    /// Flush pending blocks to storage using batch writes
    ///
    /// This commits multiple blocks in a single database transaction,
    /// which is much faster than individual writes.
    pub(crate) fn flush_pending_blocks(
        &self,
        blockstore: &BlockStore,
        _storage: Option<&Arc<Storage>>,
        pending: &mut Vec<(Arc<Block>, Arc<Vec<Vec<Witness>>>, u64)>,
    ) -> Result<()> {
        let to_flush = std::mem::take(pending);
        Self::do_flush_to_storage(blockstore, _storage, to_flush)
    }

    /// Core flush logic. Takes ownership of pending. Used by sync flush and async spawn.
    /// Blocks are Arc<Block>; we try_unwrap to get owned Block for serialization (sync has completed).
    pub(crate) fn do_flush_to_storage(
        blockstore: &BlockStore,
        _storage: Option<&Arc<Storage>>,
        pending: Vec<(Arc<Block>, Arc<Vec<Vec<Witness>>>, u64)>,
    ) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }

        let count = pending.len();
        let start = std::time::Instant::now();
        #[cfg(feature = "profile")]
        let t_serialize = std::time::Instant::now();

        // Unwrap Arcs to get owned Block (sync has completed; refcount should be 1 when validation
        // holds the only Arc after dequeue). Witness Arc is cloned only when try_unwrap fails.
        let mut pending: Vec<(Block, Arc<Vec<Vec<Witness>>>, u64)> = pending
            .into_iter()
            .map(|(arc_block, w, h)| {
                let block = Arc::try_unwrap(arc_block).unwrap_or_else(|a| (*a).clone());
                (block, w, h)
            })
            .collect();

        let flush_max_height = pending.iter().map(|(_, _, h)| *h).max().unwrap_or(0);

        // Pre-compute all block hashes ONCE (avoids 4x redundant double SHA256 per block)
        // Parallelize hash computation and serialization for better CPU utilization
        // header_data uses Arc to avoid cloning Vec on cache hit (batch.put accepts &[u8] via .as_slice())
        let (block_hashes, block_data, header_data): (Vec<Hash>, Vec<Vec<u8>>, Vec<Arc<Vec<u8>>>) = {
            let _ibd_header_cache_bypass =
                crate::storage::serialization_cache::IbdHeaderSerializeCacheBypassGuard::enter();
            #[cfg(feature = "rayon")]
            {
                use blvm_protocol::rayon::iter::IntoParallelRefIterator;
                use blvm_protocol::rayon::prelude::*;
                let block_hashes: Vec<Hash> = pending
                    .par_iter()
                    .map(|(block, _, _)| blockstore.get_block_hash(block))
                    .collect();

                // Parallel serialize all block data
                let block_data: Vec<Vec<u8>> = pending
                    .par_iter()
                    .map(|(block, _, _)| {
                        bincode::serialize(block)
                            .map_err(|e| anyhow::anyhow!("Block serialization failed: {e}"))
                    })
                    .collect::<Result<Vec<_>>>()?;

                // Parallel serialize all header data (with caching)
                use crate::storage::serialization_cache::{
                    cache_serialized_header, get_cached_serialized_header,
                };
                let header_data: Vec<Arc<Vec<u8>>> = pending
                    .par_iter()
                    .zip(block_hashes.par_iter())
                    .map(|((block, _, _), block_hash)| {
                        if let Some(cached) = get_cached_serialized_header(block_hash) {
                            return Ok(cached); // Arc::clone already done in get; no Vec clone
                        }
                        let serialized = bincode::serialize(&block.header)
                            .map_err(|e| anyhow::anyhow!("Header serialization failed: {e}"))?;
                        cache_serialized_header(*block_hash, serialized.clone());
                        Ok(Arc::new(serialized))
                    })
                    .collect::<Result<Vec<_>>>()?;

                (block_hashes, block_data, header_data)
            }

            #[cfg(not(feature = "rayon"))]
            {
                let block_hashes: Vec<Hash> = pending
                    .iter()
                    .map(|(block, _, _)| blockstore.get_block_hash(block))
                    .collect();

                // Pre-serialize all block data
                let block_data: Vec<Vec<u8>> = pending
                    .iter()
                    .map(|(block, _, _)| {
                        bincode::serialize(block)
                            .map_err(|e| anyhow::anyhow!("Block serialization failed: {e}"))
                    })
                    .collect::<Result<Vec<_>>>()?;

                // Pre-serialize all header data (with caching)
                use crate::storage::serialization_cache::{
                    cache_serialized_header, get_cached_serialized_header,
                };
                let header_data: Vec<Arc<Vec<u8>>> = pending
                    .iter()
                    .zip(block_hashes.iter())
                    .map(|((block, _, _), block_hash)| {
                        if let Some(cached) = get_cached_serialized_header(block_hash) {
                            return Ok(cached);
                        }
                        let serialized = bincode::serialize(&block.header)
                            .map_err(|e| anyhow::anyhow!("Header serialization failed: {e}"))?;
                        cache_serialized_header(*block_hash, serialized.clone());
                        Ok(Arc::new(serialized))
                    })
                    .collect::<Result<Vec<_>>>()?;

                (block_hashes, block_data, header_data)
            }
        };

        #[cfg(feature = "profile")]
        let serialize_ms = t_serialize.elapsed().as_millis() as u64;
        #[cfg(feature = "profile")]
        let t_disk = std::time::Instant::now();

        // Sort flush order by height so LSM writes are monotonic (matches `block_height_row_key`).
        let mut flush_order: Vec<usize> = (0..pending.len()).collect();
        flush_order.sort_by_key(|&i| pending[i].2);

        // Pre-serialize witness payloads once (shared by RocksDB unified flush and legacy per-CF batches).
        let witness_blobs: Vec<Option<Vec<u8>>> = if pending.iter().any(|(_, w, _)| !w.is_empty()) {
            #[cfg(feature = "rayon")]
            {
                use blvm_protocol::rayon::iter::IntoParallelRefIterator;
                use blvm_protocol::rayon::prelude::*;
                let witness_data_vec: Vec<(usize, Vec<u8>)> = pending
                    .par_iter()
                    .enumerate()
                    .filter_map(|(i, (_, witnesses, _))| {
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

                let mut v = vec![None; pending.len()];
                for (i, data) in witness_data_vec {
                    v[i] = Some(data);
                }
                v
            }

            #[cfg(not(feature = "rayon"))]
            {
                let mut v = vec![None; pending.len()];
                for i in 0..pending.len() {
                    let witnesses = &pending[i].1;
                    if !witnesses.is_empty() {
                        v[i] = Some(bincode::serialize(witnesses.as_ref()).map_err(|e| {
                            anyhow::anyhow!("Failed to serialize witnesses: {}", e)
                        })?);
                    }
                }
                v
            }
        } else {
            vec![None; pending.len()]
        };

        let metadata_blobs: Vec<Vec<u8>> = (0..pending.len())
            .map(|i| {
                let metadata = BlockMetadata {
                    n_tx: pending[i].0.transactions.len() as u32,
                };
                bincode::serialize(&metadata)
                    .map_err(|e| anyhow::anyhow!("Block metadata serialization failed: {}", e))
            })
            .collect::<Result<Vec<_>>>()?;

        // Reuse `header_data` from the parallel serialize pass (avoid double bincode of last 11).
        #[cfg(any(feature = "rocksdb", feature = "redb", feature = "tidesdb"))]
        let recent_entries: Vec<(u64, Vec<u8>)> = flush_order
            .iter()
            .rev()
            .take(11)
            .map(|&idx| {
                let h = pending[idx].2;
                let data = header_data[idx].as_slice().to_vec();
                Ok((h, data))
            })
            .collect::<Result<Vec<_>>>()?;

        let heights: Vec<u64> = pending.iter().map(|(_, _, h)| *h).collect();

        let mut storage_unified = false;
        #[cfg(feature = "rocksdb")]
        {
            if blockstore.try_ibd_flush_rocksdb_unified(
                &flush_order,
                &heights,
                &block_hashes,
                &block_data,
                &header_data,
                &witness_blobs,
                &metadata_blobs,
                &recent_entries,
            )? {
                storage_unified = true;
            }
        }
        #[cfg(feature = "redb")]
        {
            if !storage_unified
                && blockstore.try_ibd_flush_redb_unified(
                    &flush_order,
                    &heights,
                    &block_hashes,
                    &block_data,
                    &header_data,
                    &witness_blobs,
                    &metadata_blobs,
                    &recent_entries,
                )?
            {
                storage_unified = true;
            }
        }
        #[cfg(feature = "tidesdb")]
        {
            if !storage_unified
                && blockstore.try_ibd_flush_tidesdb_unified(
                    &flush_order,
                    &heights,
                    &block_hashes,
                    &block_data,
                    &header_data,
                    &witness_blobs,
                    &metadata_blobs,
                    &recent_entries,
                )?
            {
                storage_unified = true;
            }
        }

        if !storage_unified {
            // Per-tree batches (Redb, Sled, TidesDB, or non-Rocks `Arc<dyn Database>`).
            // Batch write blocks (no WAL — safe for IBD, re-downloads on crash)
            {
                let blocks_tree = blockstore.blocks_tree()?;
                let mut batch = blocks_tree.batch()?;
                for &i in &flush_order {
                    let height = pending[i].2;
                    let key = block_height_row_key(height, &block_hashes[i]);
                    batch.put(&key, &block_data[i]);
                }
                batch.commit_no_wal()?;
            }

            // Batch write headers
            {
                let headers_tree = blockstore.headers_tree()?;
                let mut batch = headers_tree.batch()?;
                for &i in &flush_order {
                    let height = pending[i].2;
                    let key = block_height_row_key(height, &block_hashes[i]);
                    batch.put(&key, header_data[i].as_slice());
                }
                batch.commit_no_wal()?;
            }

            // Batch write witnesses (skip if all empty — common in early chain)
            {
                let has_witnesses = pending.iter().any(|(_, w, _)| !w.is_empty());
                if has_witnesses {
                    let witnesses_tree = blockstore.witnesses_tree()?;
                    let mut batch = witnesses_tree.batch()?;
                    for &i in &flush_order {
                        if let Some(ref data) = witness_blobs[i] {
                            let height = pending[i].2;
                            let key = block_height_row_key(height, &block_hashes[i]);
                            batch.put(&key, data);
                        }
                    }
                    batch.commit_no_wal()?;
                }
            }

            // Batch write height index
            {
                let height_tree = blockstore.height_tree()?;
                let mut batch = height_tree.batch()?;
                for &i in &flush_order {
                    let height = pending[i].2;
                    let height_key = height.to_be_bytes();
                    batch.put(&height_key, &block_hashes[i]);
                }
                batch.commit_no_wal()?;
            }

            // Reverse index (hash → height) — required for RPC lookups and chain recovery
            {
                let ht_tree = blockstore.hash_to_height_tree()?;
                let mut batch = ht_tree.batch()?;
                for &i in &flush_order {
                    let height_bytes = pending[i].2.to_be_bytes();
                    batch.put(&block_hashes[i], &height_bytes);
                }
                batch.commit_no_wal()?;
            }

            // Block metadata (same row keys as bodies — keeps RPC n_tx consistent with `store_block_with_witness`)
            {
                let meta_tree = blockstore.metadata_tree()?;
                let mut batch = meta_tree.batch()?;
                for &i in &flush_order {
                    let key = block_height_row_key(pending[i].2, &block_hashes[i]);
                    batch.put(&key, &metadata_blobs[i]);
                }
                batch.commit_no_wal()?;
            }

            // Store recent headers (needed for MTP calculation) — single batch vs N small writes.
            let recent_batch: Vec<(u64, &BlockHeader)> = pending
                .iter()
                .rev()
                .take(11)
                .map(|(block, _, height)| (*height, &block.header))
                .collect();
            blockstore.store_recent_headers_ibd_batch(&recent_batch)?;
        }

        #[cfg(feature = "profile")]
        {
            let disk_ms = t_disk.elapsed().as_millis() as u64;
            blvm_protocol::profile_log!(
                "[FLUSH_STORAGE_PERF] blocks={} max_height={} serialize_ms={} disk_ms={} total_ms={}",
                count,
                flush_max_height,
                serialize_ms,
                disk_ms,
                start.elapsed().as_millis()
            );
        }

        // Skip transaction indexing during IBD - it's not needed until sync is complete
        // and causes massive slowdowns due to individual writes per transaction

        // Chain metadata: parallel IBD bypasses `run_loop`, so `update_tip` must run here
        // or `get_height()` / restarts see `chain_info` missing despite full block index.
        if let Some(storage) = _storage {
            if let Some((idx, _)) = pending.iter().enumerate().max_by_key(|(_, (_, _, h))| *h) {
                let block = &pending[idx].0;
                let tip_height = pending[idx].2;
                let tip_hash = block_hashes[idx];
                storage
                    .chain()
                    .update_tip(&tip_hash, &block.header, tip_height)?;
            }
        }

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
    // Checkpoint Tests
    // ============================================================

    #[test]
    fn test_mainnet_checkpoints_exist() {
        assert!(
            !checkpoints::MAINNET_CHECKPOINTS.is_empty(),
            "Checkpoints should be defined"
        );
    }

    #[test]
    fn test_mainnet_checkpoints_start_at_genesis() {
        let (height, _hash) = checkpoints::MAINNET_CHECKPOINTS[0];
        assert_eq!(
            height, 0,
            "First checkpoint should be genesis block (height 0)"
        );
    }

    #[test]
    fn test_mainnet_checkpoints_in_ascending_order() {
        for i in 1..checkpoints::MAINNET_CHECKPOINTS.len() {
            let (prev_height, _) = checkpoints::MAINNET_CHECKPOINTS[i - 1];
            let (curr_height, _) = checkpoints::MAINNET_CHECKPOINTS[i];
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
        let (height, hash) = checkpoints::MAINNET_CHECKPOINTS[0];
        assert_eq!(height, 0);

        assert_eq!(
            hash,
            blvm_protocol::GENESIS_BLOCK_HASH_INTERNAL,
            "Genesis block hash should match"
        );
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
