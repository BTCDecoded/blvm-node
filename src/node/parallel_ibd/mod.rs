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
#[cfg(feature = "production")]
mod ibd_staging;
mod memory;
mod prefetch;
#[cfg(feature = "production")]
mod retire_dispatcher;
mod types;
#[cfg(feature = "production")]
mod validation_loop;

#[cfg(feature = "production")]
pub(crate) use validation_loop::IbdRetireWork;

use chunk_assigner::{ChunkAssigner, ChunkGuard, create_chunks as create_chunks_impl};

pub use chunk_assigner::BlockChunk;
use download::download_chunk;
use feeder::{new_feeder_state, run_feeder_thread};
use memory::{MemoryGuard, TIDESDB_MAX_TXN_OPS};
#[cfg(feature = "production")]
use types::PrefetchWorkItemV2;
use types::{
    ChunkWorkItem, FeederBufferValue, ReadyItem, SharedBlock, SharedWitnesses, estimate_block_bytes,
};

use crate::network::NetworkManager;
use crate::network::peer_scoring::is_lan_peer;
use crate::network::protocol::{
    GetHeadersMessage, HeadersMessage, ProtocolMessage, ProtocolParser,
};
use crate::node::block_processor::validate_block_with_context;
use crate::storage::Storage;
use crate::storage::blockstore::{BlockMetadata, BlockStore, block_height_row_key};
use crate::storage::database::Tree;
use crate::storage::disk_utxo::{
    OutPointKey, SyncBatch, block_input_keys_and_tx_ids_filtered, block_input_keys_batch_into_arc,
    compute_tx_ids_only, key_to_outpoint, outpoint_to_key,
};
#[cfg(feature = "production")]
use crate::storage::ibd_utxo_store::IbdUtxoStore;
use crate::utils::{IBD_YIELD_SLEEP, MESSAGE_PROCESSOR_POLL_SLEEP};
use anyhow::{Context, Result};
use blvm_protocol::bip_validation::Bip30Index;
use blvm_protocol::{
    BitcoinProtocolEngine, Block, BlockHeader, Hash, UtxoSet, ValidationResult, segwit::Witness,
};

use blvm_protocol::serialization::varint::decode_varint;
use blvm_protocol::types::{OutPoint, UTXO};
use crossbeam_channel;
/// Set to `true` by the process-level signal handler when SIGTERM/SIGINT is received.
///
/// The IBD validation loop polls this on its feeder condvar timeout (every 5 s) and, when
/// set, marks the feeder as done so the pipeline drains cleanly and the watermark checkpoint
/// is flushed before returning.  The main binary waits for `node.start()` to complete rather
/// than dropping it, so the join of the validation thread and the final `persist_ibd_utxo_flush_checkpoint`
/// call both execute before the process exits.
pub static IBD_SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// True while `ParallelIBD::sync_parallel` is running (startup IBD or run-loop catch-up).
pub static PARALLEL_IBD_SESSION_ACTIVE: AtomicBool = AtomicBool::new(false);

struct ParallelIbdSessionGuard;

impl Drop for ParallelIbdSessionGuard {
    fn drop(&mut self) {
        PARALLEL_IBD_SESSION_ACTIVE.store(false, Ordering::Release);
    }
}
use futures::stream::{FuturesUnordered, StreamExt};
use hex;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use tokio::sync::Semaphore;
use tokio::sync::broadcast;
use tokio::sync::oneshot;
use tokio::time::{Duration, timeout};
use tracing::{debug, error, info, warn};

/// Parallel IBD configuration
#[derive(Debug, Clone)]
pub struct ParallelIBDConfig {
    /// Network (mainnet/testnet/regtest) for consensus rule selection
    pub network: blvm_protocol::types::Network,
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
    /// True when `BLVM_IBD_PEERS` is set to a non-empty value (empty string = unset).
    fn ibd_peers_env_explicit() -> bool {
        std::env::var("BLVM_IBD_PEERS")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }

    /// Build config from optional IbdConfig. ENV overrides config file.
    pub fn from_config(ibd_config: Option<&crate::config::IbdConfig>) -> Self {
        let chunk_size = std::env::var("BLVM_IBD_CHUNK_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(|n: u64| n.clamp(16, 2000))
            .or_else(|| ibd_config.map(|c| c.chunk_size))
            .unwrap_or(128);
        let download_timeout_secs = std::env::var("BLVM_IBD_DOWNLOAD_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| ibd_config.map(|c| c.download_timeout_secs))
            .unwrap_or(30);
        let preferred_peers = std::env::var("BLVM_IBD_PEERS")
            .ok()
            .filter(|s| !s.trim().is_empty())
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
            .filter(|s| !s.trim().is_empty())
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
            .unwrap_or(128);
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
            network: blvm_protocol::types::Network::Mainnet,
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

    /// Build config for an IBD session: file/ENV defaults, then LAN auto-prefer.
    /// Does not change `[ibd].mode` — parallel stays parallel unless set in config/ENV.
    pub fn resolve_for_session(
        ibd_config: Option<&crate::config::IbdConfig>,
        _synced_chain_height: u64,
        peer_addresses: &[String],
    ) -> Self {
        let mut config = Self::from_config(ibd_config);

        if config.preferred_peers.is_empty() && !Self::ibd_peers_env_explicit() {
            let lan_peers: Vec<String> = peer_addresses
                .iter()
                .filter(|p| {
                    p.parse::<SocketAddr>()
                        .ok()
                        .is_some_and(|a| is_lan_peer(&a))
                })
                .cloned()
                .collect();
            if !lan_peers.is_empty() {
                info!(
                    "IBD: auto-preferring {} LAN peer(s) for download (set BLVM_IBD_PEERS to override): {}",
                    lan_peers.len(),
                    lan_peers.join(", ")
                );
                config.preferred_peers = lan_peers;
            }
        }

        config
    }

    /// Minimum connected peers before starting block download.
    pub fn min_peers_for_ibd(&self) -> usize {
        if !self.preferred_peers.is_empty() {
            1
        } else {
            2
        }
    }

    /// WAN-only multi-peer mode: true when multiple WAN peers are available.
    /// Default ON. Set BLVM_IBD_WAN_SINGLE_PEER=1 to force single-peer.
    fn is_wan_only_multi_peer(peers: &[String]) -> bool {
        let all_wan = peers.len() > 1
            && peers.iter().all(|p| {
                p.parse::<SocketAddr>()
                    .ok()
                    .is_none_or(|a| !is_lan_peer(&a))
            });
        all_wan
            && !std::env::var("BLVM_IBD_WAN_SINGLE_PEER")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
    }

    /// WAN-only: collapse to single fastest peer only if BLVM_IBD_WAN_SINGLE_PEER=1.
    fn collapse_wan_only_download_peers(mut peers: Vec<String>) -> Vec<String> {
        if Self::is_wan_only_multi_peer(&peers) {
            return peers; // multi-peer: no collapse
        }
        // Single-peer forced or only one peer
        if peers.len() > 1
            && peers.iter().all(|p| {
                p.parse::<SocketAddr>()
                    .ok()
                    .is_none_or(|a| !is_lan_peer(&a))
            })
        {
            peers.truncate(1);
        }
        peers
    }
}

/// When `preferred` is non-empty, restrict to matching connected peers.
/// If none match yet, warn and fall back to all connected peers so IBD can proceed.
pub(crate) fn filter_ibd_download_peers(
    preferred: &[String],
    connected: Vec<String>,
) -> Vec<String> {
    if preferred.is_empty() {
        return connected;
    }
    let matches_preferred = |peer: &str| -> bool {
        preferred.iter().any(|pref| {
            peer == pref.as_str()
                || (!pref.contains(':')
                    && peer.starts_with(pref)
                    && peer.as_bytes().get(pref.len()) == Some(&b':'))
        })
    };
    let matched: Vec<String> = connected
        .iter()
        .filter(|p| matches_preferred(p.as_str()))
        .cloned()
        .collect();
    if matched.is_empty() {
        warn!(
            "IBD preferred_peers={preferred:?} but none are connected yet (connected: {connected:?}); \
             continuing with all connected peers"
        );
        connected
    } else {
        info!(
            "IBD preferred_peers: using {} ({})",
            matched.len(),
            matched.join(", ")
        );
        matched
    }
}

/// Block download request
#[derive(Debug, Clone)]
struct BlockRequest {
    height: u64,
    hash: Hash,
    peer_id: String,
}

/// Options for [`ParallelIBD::do_flush_to_storage`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct IbdBlockFlushOpts {
    /// Use rayon for bincode serialization. Disabled on the validation-thread shutdown path
    /// to avoid pool contention / nested-par_iter deadlocks while P2P is still active.
    pub parallel_serialize: bool,
    /// Emit `info!` per 50-block chunk (terminal flush diagnostics).
    pub log_progress: bool,
}

impl Default for IbdBlockFlushOpts {
    fn default() -> Self {
        Self {
            parallel_serialize: true,
            log_progress: false,
        }
    }
}

impl IbdBlockFlushOpts {
    pub(crate) fn shutdown_sync() -> Self {
        Self {
            parallel_serialize: false,
            log_progress: true,
        }
    }
}

/// Parallel IBD coordinator
pub struct ParallelIBD {
    config: ParallelIBDConfig,
    /// Earliest BIP54 activation height from version-bits lock-in along the validated chain (mainnet).
    /// Lock-free: `u64::MAX` sentinel = `None`. The merge semantics (`min` of present values) are
    /// expressible as a lock-free `fetch_min`, eliminating the per-block parking_lot::Mutex
    /// contention that previously serialized 8 validation workers through this code path.
    bip54_activation_from_version_bits: std::sync::atomic::AtomicU64,
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
            bip54_activation_from_version_bits: std::sync::atomic::AtomicU64::new(u64::MAX),
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
            None => {
                return Err(anyhow::anyhow!(
                    "IBD requires storage. Run with a data directory (e.g. --datadir) or ensure storage is initialized."
                ));
            }
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

        PARALLEL_IBD_SESSION_ACTIVE.store(true, Ordering::Release);
        let _ibd_session_guard = ParallelIbdSessionGuard;

        let headers_start = std::time::Instant::now();

        // Download headers first (sequential, but fast); iterate until chain tip.
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

        // Drop extremely slow peers (>90s average latency); keep at least two peers when possible.
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

        // preferred_peers: prefer these peers when connected; fall back if none match yet.
        filtered_peers = filter_ibd_download_peers(&self.config.preferred_peers, filtered_peers);

        let ibd_mode: &str = &self.config.mode;
        let wan_multi_peer = ParallelIBDConfig::is_wan_only_multi_peer(&filtered_peers);
        filtered_peers = ParallelIBDConfig::collapse_wan_only_download_peers(filtered_peers);
        if wan_multi_peer {
            info!(
                "WAN multi-peer IBD: {} peers, 1 worker/peer, work-stealing, peer blacklisting (set BLVM_IBD_WAN_SINGLE_PEER=1 to force single-peer)",
                filtered_peers.len()
            );
        } else if filtered_peers.len() == 1 {
            if let Ok(addr) = filtered_peers[0].parse::<SocketAddr>() {
                if !is_lan_peer(&addr) {
                    info!(
                        "WAN-only IBD: single fastest peer {} for download (mode={})",
                        filtered_peers[0], ibd_mode
                    );
                }
            }
        }

        // mode=sequential: single-peer mode. Core-like earliest-first, no chunk-boundary stalls.
        if ibd_mode.eq_ignore_ascii_case("sequential") {
            if let Some(best) = filtered_peers.first().cloned() {
                filtered_peers = vec![best.clone()];
                info!(
                    "BLVM_IBD_MODE=sequential: single-peer mode ({}), Core-like block fetch",
                    best
                );
            }
        }

        // Split the height range into chunks and assign peers (weighted by speed).
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

        // WAN multi-peer: 1 worker per peer (prevents thundering-herd on WAN peers that throttle).
        // LAN / single-peer: 2–6 workers per peer weighted by priority score.
        let total_download_workers: usize = filtered_peers
            .iter()
            .map(|peer_id| {
                if wan_multi_peer {
                    1
                } else {
                    let priority = scored_peers
                        .iter()
                        .find(|(p, _)| p == peer_id)
                        .map(|(_, s)| *s)
                        .unwrap_or(1.0);
                    ((2.0 * priority) as usize).clamp(2, 6)
                }
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

        // Streaming block download + validation pipeline
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
        // Bounded download→coordinator queue + safety valve:
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
            let h = pipeline.clamp(1, PIPELINE_HORIZON_FOR_CAP);
            let base = bl.saturating_mul(4);
            let parallel = total_download_workers.saturating_mul(h).saturating_mul(2);
            let floor = if mem_guard.system_total_ram_mb()
                <= memory::MemoryGuard::EXTENDED_SIXTEEN_CLASS_MB
            {
                128
            } else {
                256
            };
            let raw = base.max(parallel).clamp(floor, 8192);
            // Cap blocks buffered on the bounded tokio queue (≠ `MemoryGuard::max_ahead_blocks` /
            // `tier_max_download_ahead_blocks`, which throttle download prefetch). Two separate knobs.
            let ram_ceiling = match mem_guard.system_total_ram_mb() {
                // ≤~18 GiB physical — keep worst‑case queued full blocks bounded on tight hosts.
                m if m <= memory::MemoryGuard::EXTENDED_SIXTEEN_CLASS_MB => 1024,
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
            tokio::sync::mpsc::channel::<(u64, SharedBlock, SharedWitnesses)>(
                download_block_queue_cap,
            );
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
            wan_multi_peer,
        ));
        info!(
            "IBD: {} chunk assignment — {} chunks (work_stealing={})",
            if wan_multi_peer {
                "work-stealing"
            } else {
                "sequential"
            },
            assigner.total_chunks(),
            wan_multi_peer
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
            let eviction: crate::storage::ibd_utxo_store::EvictionStrategy = self
                .config
                .eviction
                .parse()
                .unwrap_or(crate::storage::ibd_utxo_store::EvictionStrategy::Fifo);
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
                storage.utxo_value_codec(),
            ));
            if start_height <= 1 {
                store.bootstrap_genesis(&protocol.get_network_params().genesis_block);
            }
            if ibd_memory_only {
                info!("IBD_MEMORY_ONLY=1: prefetch uses cache only (no disk reads during IBD)");
            }
            store
        };

        // Age-tiered UTXO engine (Phase 2). Opened here (before coordinator) so `coord_engine_mode`
        // reflects the actual runtime path — including resume-after-SIGKILL when we re-seed from
        // the durable `ibd_utxos` checkpoint.
        let utxo_engine: Option<Arc<crate::storage::ibd_engine::UtxoDatabase>> = {
            let engine_enabled = std::env::var("BLVM_IBD_ENGINE")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            if !engine_enabled {
                None
            } else {
                let engine_path = std::env::var("BLVM_IBD_ENGINE_PATH")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| {
                        std::env::temp_dir()
                            .join(format!("blvm_ibd_engine_{}.bin", std::process::id()))
                    });
                let checkpoint_height = std::env::var("BLVM_IBD_EXPORT_HEIGHT_OVERRIDE")
                    .ok()
                    .and_then(|v| v.parse::<i32>().ok())
                    .or_else(|| {
                        storage
                            .chain()
                            .get_engine_export_height()
                            .ok()
                            .flatten()
                            .map(|h| h as i32)
                    })
                    .unwrap_or_else(|| start_height.saturating_sub(1) as i32);
                let resume_seed = start_height > 1 && checkpoint_height > 0;

                // Wipe stale on-disk engine state whenever resuming or epoch/dirty mismatch.
                {
                    let mut epoch_os = engine_path.as_os_str().to_owned();
                    epoch_os.push(".epoch");
                    let epoch_path = std::path::PathBuf::from(&epoch_os);
                    let mut dirty_os = engine_path.as_os_str().to_owned();
                    dirty_os.push(".dirty");
                    let dirty_path = std::path::PathBuf::from(&dirty_os);
                    let was_dirty = dirty_path.exists();
                    let stored_epoch: Option<u64> = std::fs::read_to_string(&epoch_path)
                        .ok()
                        .and_then(|s| s.trim().parse().ok());
                    let needs_wipe = resume_seed
                        || (engine_path.exists()
                            && ((stored_epoch != Some(start_height)) || was_dirty));
                    if needs_wipe {
                        info!(
                            "IBD engine: wiping stale engine files at {} (start_height={}, resume_seed={}, stored_epoch={:?}, was_dirty={})",
                            engine_path.display(),
                            start_height,
                            resume_seed,
                            stored_epoch,
                            was_dirty,
                        );
                        let _ = std::fs::remove_file(&engine_path);
                        let mut segs_os = engine_path.as_os_str().to_owned();
                        segs_os.push(".segs");
                        let _ = std::fs::remove_dir_all(std::path::PathBuf::from(segs_os));
                        let _ = std::fs::remove_file(&epoch_path);
                    }
                    let _ = std::fs::write(&dirty_path, b"1" as &[u8]);
                    let _ = std::fs::write(&epoch_path, format!("{start_height}\n"));
                }

                match crate::storage::ibd_engine::UtxoDatabase::open(
                    &engine_path,
                    mem_guard.engine_avail_mb(),
                ) {
                    Ok(db) => {
                        if resume_seed {
                            let ckpt_slot = storage.chain().get_engine_ckpt_slot().unwrap_or(0);
                            let ckpt_tree_name =
                                crate::storage::ibd_engine::ckpt_tree_for_slot(ckpt_slot);
                            let expected_count = storage
                                .chain()
                                .get_engine_export_utxo_count()
                                .ok()
                                .flatten();
                            let seed_ok = match storage.open_tree(ckpt_tree_name) {
                                Ok(tree) if tree.is_empty().unwrap_or(true) => {
                                    warn!(
                                        "IBD engine: resume at height {} but {} empty — legacy path",
                                        start_height, ckpt_tree_name
                                    );
                                    false
                                }
                                Ok(tree) => {
                                    match crate::storage::ibd_engine::seed_from_ibd_utxos(
                                        &db,
                                        tree.as_ref(),
                                        checkpoint_height,
                                        expected_count,
                                        storage.utxo_value_codec(),
                                    ) {
                                        Ok(n) => {
                                            info!(
                                                "IBD engine: resume from height {} — re-seeded {} UTXOs from {} (slot {})",
                                                start_height, n, ckpt_tree_name, ckpt_slot
                                            );
                                            true
                                        }
                                        Err(e) => {
                                            warn!(
                                                "IBD engine: seed from {} failed at checkpoint {}: {e:#} — falling back to legacy path",
                                                ckpt_tree_name, checkpoint_height
                                            );
                                            false
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        "IBD engine: could not open {} for seed: {e:#}",
                                        ckpt_tree_name
                                    );
                                    false
                                }
                            };
                            if !seed_ok { None } else { Some(Arc::new(db)) }
                        } else {
                            info!(
                                "IBD engine (Phase 2) enabled; flat-file table at {}",
                                engine_path.display()
                            );
                            Some(Arc::new(db))
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Failed to open IBD engine at {:?}: {e:#}. Falling back to legacy path.",
                            engine_path
                        );
                        None
                    }
                }
            }
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
        let prefetch_workers: usize = if utxo_engine.is_some() {
            // Engine mode: workers do near-zero work (no UTXO loads, just passthrough).
            // 1 worker + 1 gap-fill is enough to drain the channel without blocking the
            // coordinator. Spawning N×2 idle threads adds unnecessary bridge-mutex contention.
            self.config.prefetch_workers.unwrap_or(1).min(2)
        } else {
            self.config.prefetch_workers.unwrap_or_else(|| {
                let n = std::thread::available_parallelism()
                    .map(|p| p.get())
                    .unwrap_or(8);
                (n.saturating_mul(2)).clamp(4, 24)
            })
        };
        let gap_fill_workers: usize = if utxo_engine.is_some() {
            1
        } else {
            prefetch_workers
        };
        let (prefetch_input_tx_v2, gap_fill_tx_v2, ready_bridge, ready_rx) = {
            let store = Arc::clone(&ibd_store_v2);
            let (in_tx, in_rx) =
                crossbeam_channel::bounded::<PrefetchWorkItemV2>(max_prefetches_in_flight);
            let (gap_tx_v2, gap_rx_v2) =
                crossbeam_channel::bounded::<PrefetchWorkItemV2>(gap_fill_workers * 4);
            let (out_tx, out_rx) =
                crossbeam_channel::bounded::<ReadyItem>(max_prefetches_in_flight);
            // OrderedReadyBridge wraps `out_tx`. Parallel prefetch workers complete out of order;
            // the bridge buffers completions and only releases heights in strict ascending order
            // so the feeder/validation cursor never stalls on a future height.
            let bridge = Arc::new(prefetch::OrderedReadyBridge::new(out_tx));
            for _ in 0..prefetch_workers {
                let rx_clone = in_rx.clone();
                let bridge_clone = Arc::clone(&bridge);
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    prefetch::run_prefetch_worker(rx_clone, bridge_clone, store)
                });
            }
            for _ in 0..gap_fill_workers {
                let rx_clone = gap_rx_v2.clone();
                let bridge_clone = Arc::clone(&bridge);
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    prefetch::run_prefetch_worker(rx_clone, bridge_clone, store)
                });
            }
            info!(
                "IBD v2 prefetch: {} workers, queue={}; gap-fill overflow: {} workers",
                prefetch_workers, max_prefetches_in_flight, gap_fill_workers
            );
            (in_tx, gap_tx_v2, bridge, out_rx)
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
            // WAN multi-peer: 1 worker per peer so we don't hammer peers with concurrent requests.
            // LAN / single-peer: 2–6 workers weighted by priority for pipeline depth.
            let worker_count = if wan_multi_peer {
                1
            } else {
                ((2.0 * priority) as usize).clamp(2, 6)
            };

            info!(
                "IBD: {} worker(s) for peer {} (priority: {:.2})",
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

                let validation_height_clone = Arc::clone(&validation_height);
                let handle = tokio::spawn(async move {
                    let mut chunks_completed = 0u64;
                    let mut blocks_downloaded = 0u64;
                    let mut consecutive_failures = 0u32;
                    const MAX_CONSECUTIVE_FAILURES: u32 = 10;

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
                                info!(
                                    "[IBD] Worker {} exiting: queue empty (chunks_completed={}, blocks_downloaded={})",
                                    peer_id, chunks_completed, blocks_downloaded
                                );
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
                                warn!(
                                    "[IBD] Worker {} semaphore acquire failed — ChunkGuard will re-queue",
                                    peer_id
                                );
                                break;
                            }
                        };

                        // Bootstrap (start==0): no per-peer semaphore so we don't starve the first chunk. Post-bootstrap: 16 blocks/peer (Core).
                        let blocks_sem = if start == 0 {
                            None
                        } else {
                            peer_blocks_semaphores_clone.get(&peer_id).cloned()
                        };
                        // Hard outer deadline: scale with remaining blocks so near-tip multi-block
                        // chunks are not aborted after a single per-block timeout window.
                        let validated_tip =
                            validation_height_clone.load(std::sync::atomic::Ordering::Relaxed);
                        let resume_from =
                            download::resume_download_height(start, end, validated_tip)
                                .unwrap_or(start);
                        let outer_secs = download::chunk_outer_deadline_secs(
                            start,
                            end,
                            resume_from,
                            config.download_timeout_secs,
                        );
                        let dl_result = match tokio::time::timeout(
                            std::time::Duration::from_secs(outer_secs),
                            download_chunk(
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
                                Some(Arc::clone(&validation_height_clone)),
                            ),
                        )
                        .await
                        {
                            Ok(r) => r,
                            Err(_elapsed) => {
                                warn!(
                                    "[IBD] chunk {}-{} outer deadline ({}s) expired — aborting for retry",
                                    start, end, outer_secs
                                );
                                peer_scorer_clone.record_failure(
                                    peer_id
                                        .parse::<std::net::SocketAddr>()
                                        .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap()),
                                );
                                Err(anyhow::anyhow!(
                                    "Chunk {}-{}: outer deadline {}s",
                                    start,
                                    end,
                                    outer_secs
                                ))
                            }
                        };
                        workers_current_clone
                            .lock()
                            .await
                            .retain(|(p, s, _)| !(*p == peer_id && *s == start));
                        match dl_result {
                            Ok(chunk) => {
                                consecutive_failures = 0;
                                let block_count = chunk.block_count();
                                if start == 0 {
                                    info!(
                                        "IBD: bootstrap chunk 0-{} downloaded, coordinator enables parallel when received",
                                        end
                                    );
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
                                        peer_id,
                                        start,
                                        end,
                                        block_count,
                                        remaining
                                    );
                                }
                                // Blocks already streamed during download_chunk; no second send
                                _guard.disarm();
                                #[cfg(feature = "profile")]
                                {
                                    let ts_ms = crate::utils::time::current_timestamp_millis();
                                    blvm_protocol::profile_log!(
                                        "[IBD_CHUNK_COMPLETE] chunk_start={} chunk_end={} peer={} blocks={} ts_ms={}",
                                        start,
                                        end,
                                        peer_id,
                                        block_count,
                                        ts_ms
                                    );
                                }
                                assigner_clone.on_chunk_complete(&peer_id);
                                if start == start_height {
                                    assigner_clone.mark_bootstrap_complete();
                                }
                                chunks_completed += 1;
                                blocks_downloaded += block_count as u64;
                            }
                            Err(e) => {
                                consecutive_failures += 1;
                                warn!(
                                    "Peer {} failed chunk {}-{} ({}/{}): {} - will retry with different peer",
                                    peer_id,
                                    start,
                                    end,
                                    consecutive_failures,
                                    MAX_CONSECUTIVE_FAILURES,
                                    e
                                );
                                let err_str = e.to_string();
                                if err_str.contains("gap height")
                                    || err_str.contains("stuck at height")
                                    || err_str.contains("Coordinator stall at gap")
                                {
                                    let gap_h = assigner_clone.next_needed_height();
                                    assigner_clone.requeue_stall_gaps(gap_h);
                                }
                                // Exclude the failing peer only when another worker can take the
                                // retry. With a single chunk, other workers exit after taking no
                                // work — exclude would deadlock the only live worker.
                                let exclude = if num_peers_clone > 1
                                    && assigner_clone.total_chunks() > 1
                                {
                                    Some(peer_id.clone())
                                } else {
                                    if num_peers_clone == 1 {
                                        info!(
                                            "[IBD] Single peer: re-queuing chunk {}-{} without exclude (no fallback)",
                                            start, end
                                        );
                                    }
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
                                    // Blacklist the peer so it gets no more chunks for 300s.
                                    // Chunks it had in-flight are already requeued with exclude=peer.
                                    // Do NOT exit the worker: when all peers fail in parallel every
                                    // worker would exit, block_tx closes, and validation hangs forever
                                    // on the feeder while retry chunks remain queued.
                                    let blacklist_secs = 300u64;
                                    assigner_clone.blacklist_peer(
                                        &peer_id,
                                        std::time::Duration::from_secs(blacklist_secs),
                                    );
                                    warn!(
                                        "Peer {} exceeded max failures — blacklisted for {}s (worker stays alive)",
                                        peer_id, blacklist_secs
                                    );
                                    consecutive_failures = 0;
                                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                    continue;
                                }

                                if num_peers_clone == 1
                                    && consecutive_failures >= MAX_CONSECUTIVE_FAILURES
                                {
                                    // Single peer: wait for reconnect instead of killing the worker.
                                    warn!(
                                        "Peer {} at {} consecutive failures (single-peer mode) — waiting for reconnect",
                                        peer_id, consecutive_failures
                                    );
                                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                                    consecutive_failures = 0;
                                    continue;
                                }

                                let backoff_secs =
                                    (1u64 << (consecutive_failures - 1).min(3)).min(8);
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
        let bootstrap_end = if !chunks.is_empty() {
            chunks[0].end_height
        } else {
            u64::MAX
        };
        let assigner_for_coord = Arc::clone(&assigner);
        let validation_height_for_coord = Arc::clone(&validation_height);
        let effective_end_for_coord = effective_end_height;
        // WAN multi-peer: scale reorder buffer so fast peers can't fill it while a slow peer is
        // stalled. Minimum: num_peers × chunk_size × 2 (absorbs one full chunk per peer ahead).
        // Capped at 4000 to bound peak RSS (~4GB at 1MB/block on large-RAM machines).
        let coord_buffer_limit = {
            let base = mem_guard.buffer_limit(start_height);
            if wan_multi_peer {
                let wan_min = (num_peers * self.config.chunk_size as usize * 2).min(4000);
                base.max(wan_min)
            } else {
                base
            }
        };
        if wan_multi_peer {
            info!(
                "Coordinator: WAN multi-peer reorder buffer={} blocks",
                coord_buffer_limit
            );
        }
        let gap_fill_tx_v2_for_coord = gap_fill_tx_v2.clone();
        let prefetch_input_tx_v2_for_coord = prefetch_input_tx_v2.clone();
        let ibd_store_v2_for_coord = Arc::clone(&ibd_store_v2);
        let stall_tx_for_coord = stall_tx.clone();
        // When engine mode is active, prefetch workers skip prefetch_build_utxo_map and
        // build_spec_adds (both results are discarded by the engine validation path), and the
        // coordinator skips input key extraction (only tx_ids needed for SpendSession::append).
        let coord_engine_mode: bool = utxo_engine.is_some();
        // Seq-1: When single peer (BLVM_IBD_SEQUENTIAL), blocks arrive in order; skip reorder_buffer.
        let sequential = num_peers == 1;
        if sequential {
            info!("Coordinator: sequential mode (single peer) — passthrough, no reorder buffer");
        }
        // The OrderedReadyBridge enforces strict-ascending delivery to the feeder. Initialize its
        // `next_expected` to start_height so prefetch worker completions are emitted starting there.
        // Prefetch workers complete out of order; without this seeding the first completion would
        // set the cursor (potentially skipping ahead of `start_height`).
        ready_bridge.coordinator_will_send_height(start_height);
        // Bridge is now held alive by every worker thread (Arc::clone'd in the spawn loop above).
        // The coordinator only needed it for the seeding call; subsequent dispatching goes through
        // `prefetch_input_tx_v2` and the workers route to the bridge themselves.
        drop(ready_bridge);
        // Create feeder_state here (before coordinator spawn) so the coordinator can hold a reference.
        // The feeder thread is spawned later but the Arc is shared; creating it early is safe.
        let feeder_state = new_feeder_state();
        let feeder_state_for_coord = Arc::clone(&feeder_state);
        tokio::spawn(async move {
            let mut reorder_buffer: std::collections::BTreeMap<
                u64,
                (SharedBlock, SharedWitnesses),
            > = std::collections::BTreeMap::new();
            // `next_prefetch_height`: tracks the watermark for stale-duplicate detection and
            // stall logging. With out-of-order dispatch (below) this is the *max* height + 1
            // of all blocks we have dispatched so far, used only for stale-dup filtering.
            let mut next_prefetch_height = start_height;
            // Heights already dispatched to prefetch workers. Prevents re-dispatching stale
            // duplicates from re-queued chunks. Pruned periodically against validation_height.
            let mut dispatched: rustc_hash::FxHashSet<u64> = rustc_hash::FxHashSet::default();
            let mut total_received = 0u64;
            const BATCH_DRAIN_LIMIT: usize = 2000; // 10K BPS: larger batches reduce recv overhead
            let mut batch: Vec<(u64, SharedBlock, SharedWitnesses)> =
                Vec::with_capacity(BATCH_DRAIN_LIMIT);
            // S2: Reuse buffer for block_input_keys (avoids alloc per block)
            let mut coord_keys_buf: Vec<OutPointKey> = Vec::new();
            let mut coord_tx_ids_buf: Vec<Hash> = Vec::new();
            // Scratch buffer for out-of-order dispatch: heights to dispatch this iteration.
            // Reused across loop iterations to avoid a per-loop Vec<u64> allocation.
            let mut dispatch_heights_buf: Vec<u64> = Vec::new();
            // Dispatch a block to prefetch workers. The prefetch pool warm-loads input UTXOs
            // (cache miss → RocksDB MultiGet) on N background threads before the validation
            // worker ever sees the block — so the validation worker only has to do CPU work
            // (script/sig/state checks) and never blocks on disk IO. Order is preserved by
            // the OrderedReadyBridge wrapping the workers' output channel.
            //
            // Channel strategy: try the primary prefetch queue first; on Full, overflow to the
            // gap-fill pool (small bounded queue with the same worker pool semantics). Both
            // full → block on prefetch (natural backpressure to the coordinator). All sends
            // are wrapped in `block_in_place` because crossbeam's `send` is sync-blocking and
            // would otherwise block the tokio runtime worker.
            let dispatch_to_prefetch = |item: (
                Arc<IbdUtxoStore>,
                Vec<OutPointKey>,
                Vec<Hash>,
                u64,
                SharedBlock,
                SharedWitnesses,
                bool,
            )| {
                tokio::task::block_in_place(|| {
                    let item = match prefetch_input_tx_v2_for_coord.try_send(item) {
                        Ok(()) => return,
                        Err(crossbeam_channel::TrySendError::Full(it)) => it,
                        Err(crossbeam_channel::TrySendError::Disconnected(_)) => return,
                    };
                    let item = match gap_fill_tx_v2_for_coord.try_send(item) {
                        Ok(()) => return,
                        Err(crossbeam_channel::TrySendError::Full(it)) => it,
                        Err(crossbeam_channel::TrySendError::Disconnected(_)) => return,
                    };
                    let _ = prefetch_input_tx_v2_for_coord.send(item);
                });
            };
            info!("Coordinator: started, awaiting blocks from download workers");
            // Stall timeout: 90s for LAN (Bitcoin Core needs 40-80s for dense Satoshi Dice era
            // chunks at h=285k-310k). 5s for WAN multi-peer: fail fast and requeue gap
            // micro-chunks so another peer can race the missing block (override via
            // BLVM_IBD_COORD_STALL_SECS).
            let coord_stall_secs: u64 = if wan_multi_peer { 5 } else { 90 };
            let coord_stall_secs = std::env::var("BLVM_IBD_COORD_STALL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(coord_stall_secs);
            let coord_stall_log_secs = coord_stall_secs;
            let mut coord_buffer_full_since: Option<std::time::Instant> = None;
            #[cfg(target_os = "linux")]
            let mut coord_emergency_log = std::time::Instant::now();
            loop {
                if validation_height_for_coord.load(Ordering::Relaxed) >= effective_end_for_coord {
                    info!(
                        "Coordinator: validation reached effective end height {} — stopping block dispatch",
                        effective_end_for_coord
                    );
                    break;
                }
                let dynamic_buffer_limit = coord_buffer_limit;
                let coord_stall_effective_secs = if assigner_for_coord.is_bootstrap_complete() {
                    coord_stall_log_secs
                } else {
                    // Bootstrap is single-worker; short stall + requeue only churns micro-chunks.
                    coord_stall_log_secs.saturating_mul(6).max(30)
                };
                // Under Emergency memory pressure, do not drain block_rx — WAN workers block on send().
                //
                // Eviction is handled by the retire thread (which calls `evict_aggressive_for_rss`
                // under PressureLevel::Emergency). Calling it from this tokio task blocks the
                // async runtime worker for ~1 s per scan and allocates ~250 MB transient per call
                // on a 6 M-entry cache. Trust retire to drain the cache; the coordinator only needs to back
                // off admission and let retire catch up.
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
                // Periodically prune the dispatched set to keep it bounded.
                // Heights below the current validation cursor have been retired; duplicates at
                // those heights can no longer reach the prefetch pipeline.
                {
                    let val_h = validation_height_for_coord.load(Ordering::Relaxed);
                    if val_h > start_height + 1024 {
                        dispatched.retain(|&h| h >= val_h.saturating_sub(512));
                    }
                }

                // Out-of-order dispatch: drain ALL blocks in the reorder_buffer to prefetch
                // workers, regardless of whether they are contiguous from next_prefetch_height.
                // The OrderedReadyBridge enforces ascending delivery to the feeder, so it is safe
                // to dispatch future heights first — they will be buffered in the bridge's pending
                // map until any gaps are filled (i.e. when a retried chunk finally arrives).
                // This eliminates HOL (head-of-line) blocking entirely: one slow/failed peer can
                // no longer stall all heights above its chunk.
                //
                // Backpressure: if buffer is full AND we've dispatched everything in it already,
                // receive more blocks before looping. If buffer has undispatched entries, drain them.
                let undispatched_in_buffer = reorder_buffer.keys().any(|h| !dispatched.contains(h));
                if reorder_buffer.len() >= dynamic_buffer_limit && !undispatched_in_buffer {
                    // Buffer full, nothing new to dispatch. Try to receive more blocks.
                    let mut gap_drained = 0usize;
                    while let Ok((h, block, witnesses)) = block_rx.try_recv() {
                        total_received += 1;
                        let next_needed = validation_height_for_coord.load(Ordering::Relaxed) + 1;
                        if h < next_needed {
                            continue; // validation already passed this height
                        }
                        if dispatched.contains(&h) {
                            // Gap recovery: re-queued chunk re-streams heights validation still needs.
                            dispatched.remove(&h);
                        }
                        reorder_buffer.insert(h, (block, witnesses));
                        gap_drained += 1;
                        if gap_drained >= 200 {
                            break;
                        }
                    }
                    if !reorder_buffer.keys().any(|h| !dispatched.contains(h)) {
                        // Still nothing undispatched. Check for stall.
                        let min_undispatched_needed = {
                            let val_h = validation_height_for_coord.load(Ordering::Relaxed);
                            // The bridge's pending holds dispatched-but-not-yet-released blocks.
                            // If validation is stalled, the gap height is around val_h+1.
                            val_h + 1
                        };
                        let now = std::time::Instant::now();
                        let stall_start = *coord_buffer_full_since.get_or_insert(now);
                        let stuck_secs = now.duration_since(stall_start).as_secs();
                        if stuck_secs >= coord_stall_effective_secs {
                            if assigner_for_coord.is_bootstrap_complete() {
                                warn!(
                                    "Coordinator stall: buffer full ({}) but height {} not in buffer for {}s — requeuing",
                                    reorder_buffer.len(),
                                    min_undispatched_needed,
                                    stuck_secs
                                );
                                let _ = stall_tx_for_coord.send(min_undispatched_needed);
                                assigner_for_coord.requeue_stall_gaps(min_undispatched_needed);
                            }
                            coord_buffer_full_since = None;
                        }
                        tokio::time::sleep(IBD_YIELD_SLEEP).await;
                        continue;
                    }
                    coord_buffer_full_since = None;
                }
                // Dispatch all undispatched heights from the reorder_buffer.
                // Reuse scratch buffer to avoid a per-loop Vec allocation.
                {
                    dispatch_heights_buf.clear();
                    dispatch_heights_buf.extend(
                        reorder_buffer
                            .keys()
                            .filter(|&&h| !dispatched.contains(&h))
                            .copied(),
                    );
                    for h in dispatch_heights_buf.drain(..) {
                        let Some((block, witnesses)) = reorder_buffer.remove(&h) else {
                            continue;
                        };
                        dispatched.insert(h);
                        if h >= next_prefetch_height {
                            next_prefetch_height = h + 1;
                        }
                        if h == bootstrap_end {
                            assigner_for_coord.mark_bootstrap_complete();
                            info!(
                                "IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled",
                                h
                            );
                        }
                        // In engine mode skip key extraction: keys are unused by the engine
                        // validation path. Only tx_ids are needed for SpendSession::append.
                        if coord_engine_mode {
                            compute_tx_ids_only(&block, &mut coord_tx_ids_buf);
                        } else {
                            block_input_keys_and_tx_ids_filtered(
                                &block,
                                &mut coord_tx_ids_buf,
                                &mut coord_keys_buf,
                            );
                        }
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
                            coord_engine_mode,
                        );
                        dispatch_to_prefetch(item);
                    }
                }
                if reorder_buffer.len() >= dynamic_buffer_limit {
                    tokio::time::sleep(IBD_YIELD_SLEEP).await;
                    continue;
                }
                let recv_fut = block_rx.recv_many(&mut batch, BATCH_DRAIN_LIMIT);
                let n = match timeout(Duration::from_secs(coord_stall_effective_secs), recv_fut)
                    .await
                {
                    Ok(n) => n,
                    Err(_) => {
                        let next_needed = validation_height_for_coord.load(Ordering::Relaxed) + 1;
                        if next_needed > effective_end_for_coord {
                            continue;
                        }
                        if !assigner_for_coord.is_bootstrap_complete() {
                            continue;
                        }
                        // next_prefetch_height is the coordinator's delivery cursor. Requeue the
                        // chunk it is waiting on to unblock the download pipeline.
                        // Additionally: if validation is stuck at a height below next_prefetch_height
                        // and that block is NOT already in the reorder_buffer, its download chunk
                        // partially failed. Requeue that chunk explicitly so the missing block is
                        // re-downloaded. Without this fix the coordinator keeps requeuing chunks
                        // ahead of the gap, leaving the missing block unreachable indefinitely.
                        // With out-of-order dispatch, stall is always at next_needed (the height
                        // validation is waiting for, i.e. bridge's gap). Requeue that chunk.
                        let stall_height = if !dispatched.contains(&next_needed) {
                            next_needed // not yet dispatched → gap in downloads
                        } else {
                            next_prefetch_height // dispatched but not delivered → bridge gap
                        };
                        warn!(
                            "Coordinator stall: no blocks for {}s, waiting for height {} (total_received={}, next_prefetch={}, stall_requeue={})",
                            coord_stall_log_secs,
                            next_needed,
                            total_received,
                            next_prefetch_height,
                            stall_height
                        );
                        let _ = stall_tx_for_coord.send(stall_height);
                        assigner_for_coord.requeue_stall_gaps(stall_height);
                        if stall_height != next_needed {
                            assigner_for_coord.requeue_stall_gaps(next_needed);
                        }
                        continue;
                    }
                };
                if n == 0 {
                    info!(
                        "Coordinator: block_rx closed (total_received={})",
                        total_received
                    );
                    // Channel closed — drain remaining reorder_buffer (any order), then exit.
                    // Do not mark the feeder done here: blocks may still be in prefetch/bridge
                    // pipeline. Premature guard.1 caused validation to exit before tail blocks
                    // (e.g. height 955035) reached the feeder buffer.
                    dispatch_heights_buf.clear();
                    dispatch_heights_buf.extend(
                        reorder_buffer
                            .keys()
                            .filter(|&&h| !dispatched.contains(&h))
                            .copied(),
                    );
                    for h in dispatch_heights_buf.drain(..) {
                        let Some((block, witnesses)) = reorder_buffer.remove(&h) else {
                            continue;
                        };
                        dispatched.insert(h);
                        if h == bootstrap_end {
                            assigner_for_coord.mark_bootstrap_complete();
                            info!(
                                "IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled",
                                h
                            );
                        }
                        // In engine mode skip key extraction: keys are unused by the engine
                        // validation path. Only tx_ids are needed for SpendSession::append.
                        if coord_engine_mode {
                            compute_tx_ids_only(&block, &mut coord_tx_ids_buf);
                        } else {
                            block_input_keys_and_tx_ids_filtered(
                                &block,
                                &mut coord_tx_ids_buf,
                                &mut coord_keys_buf,
                            );
                        }
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
                            coord_engine_mode,
                        );
                        dispatch_to_prefetch(item);
                    }
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
                            info!(
                                "IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled",
                                h
                            );
                        }
                        // Single-peer (sequential) path: still go through prefetch so the worker
                        // pool warm-loads UTXOs in parallel with validation. Compute keys here
                        // (same call the parallel path uses) so the prefetch worker has a key
                        // list to MultiGet — sending an empty `keys` would force the validation
                        // worker to re-derive them and fall through to a synchronous disk load.
                        // In engine mode skip key extraction: keys are unused by the engine
                        // validation path. Only tx_ids are needed for SpendSession::append.
                        if coord_engine_mode {
                            compute_tx_ids_only(&block, &mut coord_tx_ids_buf);
                        } else {
                            block_input_keys_and_tx_ids_filtered(
                                &block,
                                &mut coord_tx_ids_buf,
                                &mut coord_keys_buf,
                            );
                        }
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
                            coord_engine_mode,
                        );
                        dispatch_to_prefetch(item);
                        next_prefetch_height = h + 1;
                    }
                } else {
                    // Parallel: drain batch into reorder_buffer, then dispatch ALL available
                    // heights (out-of-order) to prefetch. The OrderedReadyBridge reorders them
                    // before the feeder sees them, eliminating HOL blocking at the coordinator.
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
                        let next_needed = validation_height_for_coord.load(Ordering::Relaxed) + 1;
                        if height < next_needed {
                            continue; // validation already passed this height
                        }
                        if dispatched.contains(&height) {
                            // Gap recovery: re-queued chunk re-streams heights validation still needs.
                            dispatched.remove(&height);
                        }
                        reorder_buffer.insert(height, (block, witnesses));
                    }
                    // Dispatch all undispatched blocks from the buffer (any order).
                    // Reuse scratch buffer to avoid per-loop Vec allocation.
                    dispatch_heights_buf.clear();
                    dispatch_heights_buf.extend(
                        reorder_buffer
                            .keys()
                            .filter(|&&h| !dispatched.contains(&h))
                            .copied(),
                    );
                    for h in dispatch_heights_buf.drain(..) {
                        if reorder_buffer.len() >= dynamic_buffer_limit {
                            break; // respect backpressure cap
                        }
                        let Some((block, witnesses)) = reorder_buffer.remove(&h) else {
                            continue;
                        };
                        dispatched.insert(h);
                        if h >= next_prefetch_height {
                            next_prefetch_height = h + 1;
                        }
                        if h == bootstrap_end {
                            assigner_for_coord.mark_bootstrap_complete();
                            info!(
                                "IBD: bootstrap chunk 0-{} received by coordinator, parallel download enabled",
                                h
                            );
                        }
                        // In engine mode skip key extraction: keys are unused by the engine
                        // validation path. Only tx_ids are needed for SpendSession::append.
                        if coord_engine_mode {
                            compute_tx_ids_only(&block, &mut coord_tx_ids_buf);
                        } else {
                            block_input_keys_and_tx_ids_filtered(
                                &block,
                                &mut coord_tx_ids_buf,
                                &mut coord_keys_buf,
                            );
                        }
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
                            coord_engine_mode,
                        );
                        dispatch_to_prefetch(item);
                    }
                }
            }
        });

        // Block feeder: drains ready_rx into shared buffer so validation can run while buffer fills.
        // Feeder runs on std::thread (crossbeam recv is blocking). Buffer fills while validation works.
        // feeder_state was created earlier (before coordinator spawn) so the coordinator could reference it.
        let feeder_buffer_limit = mem_guard.buffer_limit(start_height);
        let feeder_buffer_bytes_limit = mem_guard.feeder_buffer_bytes_limit;
        let feeder_handle = run_feeder_thread(
            ready_rx,
            Arc::clone(&feeder_state),
            feeder_buffer_limit,
            feeder_buffer_bytes_limit,
        );

        // Validation worker thread: reads shared buffer, waits on Condvar when empty.
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

        // Keep a clone of the engine Arc for Phase 3 watermark export after IBD completes.
        // The engine is also moved into ValidationParams so the dispatch thread can call
        // SpendSession::resolve. Both arcs point to the same UtxoDatabase.
        let utxo_engine_for_export = utxo_engine.clone();

        // Periodic mid-IBD checkpoint export (engine mode only). Every BLVM_IBD_CHECKPOINT_INTERVAL
        // blocks (default: adaptive based on UTXO count) the background thread writes an exact live
        // UTXO snapshot to the inactive ping-pong tree (`ibd_utxos_ckpt_a` / `ibd_utxos_ckpt_b`),
        // then atomically flips the active slot in chain_info. On SIGKILL the node resumes from the
        // last completed snapshot.
        //
        // Adaptive interval: at low heights the UTXO set is small and exports are fast; at 400k+
        // with 80M+ UTXOs each export takes 100–300s (before SST ingestion) or 10–30s (after SST).
        // We target ~60s per export by scaling the interval inversely with UTXO count, clamped to
        // [2_000, 20_000]. With SST ingestion, this is effectively constant-time regardless.
        // Override via BLVM_IBD_CHECKPOINT_INTERVAL env var.
        let periodic_checkpoint_handle: Option<std::thread::JoinHandle<()>> = if let Some(
            ref engine_arc,
        ) =
            utxo_engine_for_export
        {
            let engine_clone = Arc::clone(engine_arc);
            let storage_ckpt = Arc::clone(storage);
            let fixed_interval: Option<i32> = std::env::var("BLVM_IBD_CHECKPOINT_INTERVAL")
                .ok()
                .and_then(|v| v.parse().ok());
            let end_h = effective_end_height as i32;
            let handle = std::thread::Builder::new()
                    .name("ibd-checkpoint-export".to_string())
                    .spawn(move || {
                        // Lower this thread's I/O scheduling class to "best-effort, priority 7"
                        // (lowest within BE class) so validation workers' io_uring reads are
                        // preferred over the export's sequential flat-file reads under contention.
                        #[cfg(target_os = "linux")]
                        {
                            // IOPRIO_WHO_PROCESS=1, IOPRIO_CLASS_BE=2, prio=7 (lowest)
                            // IOPRIO_PRIO_VALUE(class, prio) = (class << 13) | prio
                            let ioprio: libc::c_long = (2 << 13) | 7;
                            unsafe { libc::syscall(libc::SYS_ioprio_set, 1i32, 0i32, ioprio) };
                        }
                        let mut last_exported: i32 = 0;
                        let mut last_utxo_count: u64 = 0;
                        loop {
                            std::thread::sleep(std::time::Duration::from_secs(5));
                            let cl = engine_clone.contiguous_length();
                            if cl <= 0 {
                                continue;
                            }
                            // Adaptive interval: scale with UTXO count to keep export time bounded.
                            // Use the last known UTXO count to avoid scanning the index every poll.
                            let interval: i32 = fixed_interval.unwrap_or_else(|| {
                                // Under memory pressure, export more often so GC fence advances
                                // and cross-height Add+Delete pairs can cancel (lowers RSS).
                                if crate::node::parallel_ibd::memory::ibd_pressure_level_snapshot()
                                    >= crate::node::parallel_ibd::memory::PressureLevel::Critical
                                {
                                    return 2_000;
                                }
                                // At 25M UTXOs (height ~300k) → 10_000 blocks
                                // At 80M UTXOs (height ~400k) → ~3_125 blocks
                                // Clamped to [2_000, 10_000]
                                const BASE_UTXOS: u64 = 25_000_000;
                                const BASE_INTERVAL: i32 = 10_000;
                                if last_utxo_count == 0 {
                                    BASE_INTERVAL
                                } else {
                                    let scaled = (BASE_INTERVAL as u64).saturating_mul(BASE_UTXOS)
                                        / last_utxo_count.max(1);
                                    (scaled as i32).clamp(2_000, 10_000)
                                }
                            });
                            let ckpt = (cl / interval) * interval;
                            if ckpt <= last_exported || ckpt <= 0 {
                                continue;
                            }
                            // Only export if the engine has fully processed the checkpoint height.
                            if cl < ckpt {
                                continue;
                            }
                            let active_slot =
                                storage_ckpt.chain().get_engine_ckpt_slot().unwrap_or(0);
                            let write_slot =
                                crate::storage::ibd_engine::ckpt_inactive_slot(active_slot);
                            let ckpt_tree_name =
                                crate::storage::ibd_engine::ckpt_tree_for_slot(write_slot);
                            let tree = match storage_ckpt.open_tree(ckpt_tree_name) {
                                Ok(t) => t,
                                Err(e) => {
                                    warn!(
                                        "IBD checkpoint: failed to open {}: {e:#}",
                                        ckpt_tree_name
                                    );
                                    continue;
                                }
                            };
                            // Set the GC fence *before* starting the scan so that
                            // concurrent MemoryRun compactions do not cancel Add+Delete
                            // pairs where the Delete is above this checkpoint height.
                            // Without the fence, UTXOs created before `ckpt` but spent
                            // after `ckpt` can be GC'd out of the engine while
                            // `scan_live_at_height(ckpt)` is running, producing an
                            // incomplete checkpoint that fails on resume.
                            crate::storage::ibd_engine::set_gc_fence(ckpt);
                            info!(
                                "IBD engine: starting periodic checkpoint export at height {} \
                                 (engine_height={}, tree={}) [GC fence={}]",
                                ckpt, cl, ckpt_tree_name, ckpt
                            );
                            match crate::storage::ibd_engine::run_checkpoint_export_replace(
                                &engine_clone,
                                &tree,
                                ckpt,
                                storage_ckpt.utxo_value_codec(),
                            ) {
                                Ok((muhash, count)) => {
                                    let muhash_bytes = muhash.serialize_running_state();
                                    match storage_ckpt.chain().persist_engine_checkpoint_complete(
                                        ckpt as u64,
                                        write_slot,
                                        count as u64,
                                        &muhash_bytes,
                                    ) {
                                        Ok(()) => {
                                            info!(
                                                "IBD engine: checkpoint persisted at height {} \
                                                 (slot {}, {} UTXOs)",
                                                ckpt, write_slot, count
                                            );
                                            last_exported = ckpt;
                                            last_utxo_count = count as u64;
                                            // Immediately advance the GC fence to the NEXT
                                            // expected checkpoint height. This allows compaction
                                            // to cancel Add+Delete pairs that are no longer
                                            // needed, while protecting only the pairs that cross
                                            // the upcoming checkpoint boundary.
                                            // This bounds non-GC'd accumulation to one interval
                                            // (~840 MB) rather than letting it grow without bound
                                            // while validation races ahead of the exporter.
                                            let next_interval: i32 =
                                                fixed_interval.unwrap_or_else(|| {
                                                    if crate::node::parallel_ibd::memory::ibd_pressure_level_snapshot()
                                                        >= crate::node::parallel_ibd::memory::PressureLevel::Critical
                                                    {
                                                        return 2_000;
                                                    }
                                                    const BASE_UTXOS: u64 = 25_000_000;
                                                    const BASE_INTERVAL: i32 = 10_000;
                                                    if last_utxo_count == 0 {
                                                        BASE_INTERVAL
                                                    } else {
                                                        let scaled = (BASE_INTERVAL as u64)
                                                            .saturating_mul(BASE_UTXOS)
                                                            / last_utxo_count.max(1);
                                                        (scaled as i32).clamp(2_000, 10_000)
                                                    }
                                                });
                                            crate::storage::ibd_engine::set_gc_fence(
                                                ckpt + next_interval,
                                            );
                                        }
                                        Err(e) => {
                                            warn!(
                                                "IBD engine: failed to persist checkpoint \
                                                 metadata at {ckpt}: {e:#}"
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        "IBD engine: checkpoint export failed at height {ckpt}: \
                                         {e:#}"
                                    );
                                    // On failure the fence stays at ckpt so the retry scan is
                                    // protected. The retry will re-advance the fence when it
                                    // starts the next export attempt.
                                }
                            }
                            if ckpt >= end_h {
                                break;
                            }
                        }
                    })
                    .expect("spawn ibd-checkpoint-export");
            Some(handle)
        } else {
            None
        };

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
            stall_tx: stall_tx.clone(),
            utxo_engine,
            checkpoint_tx: None,
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
        let mut ibd_pipeline_shutdown = || {
            drop(prefetch_input_tx_v2);
            drop(gap_fill_tx_v2);
            let _ = feeder_handle.join();
        };
        match tokio::task::block_in_place(|| validation_handle.join()) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                ibd_pipeline_shutdown();
                return Err(e);
            }
            Err(panic) => {
                ibd_pipeline_shutdown();
                return Err(anyhow::anyhow!("Validation thread panicked: {:?}", panic));
            }
        }
        let validated_through = validation_height.load(Ordering::Relaxed);
        if validated_through < effective_end_height {
            ibd_pipeline_shutdown();
            return Err(anyhow::anyhow!(
                "Parallel IBD ended early: validated through height {} but need {}",
                validated_through,
                effective_end_height
            ));
        }
        if let Some(h) = progress_handle {
            let _ = h.await;
        }
        ibd_pipeline_shutdown();
        // Periodic checkpoint export thread: join (it detects engine completion via end_h check
        // and exits, or the engine Arc drop will cause its next poll to see cl == 0).
        if let Some(h) = periodic_checkpoint_handle {
            let _ = h.join();
        }
        *utxo_set = match Arc::try_unwrap(utxo_mutex) {
            Ok(mutex) => mutex
                .into_inner()
                .map_err(|e| anyhow::anyhow!("IBD UTXO mutex poisoned: {e:?}"))?,
            Err(arc) => arc
                .lock()
                .map_err(|e| anyhow::anyhow!("IBD UTXO mutex poisoned: {e:?}"))?
                .clone(),
        };

        // Phase 3: watermark export — scan all live UTXOs from the engine and write to
        // the production ibd_utxos tree. Computes MuHash from the same scan (no per-op reads).
        if let Some(ref db) = utxo_engine_for_export {
            info!("IBD engine: running Phase 3 watermark export...");
            match storage.open_tree("ibd_utxos") {
                Ok(tree) => {
                    match crate::storage::ibd_engine::run_watermark_export(
                        db,
                        &tree,
                        effective_end_height as i32,
                        storage.utxo_value_codec(),
                    ) {
                        Ok(muhash) => {
                            let muhash_bytes = muhash.serialize_running_state();
                            if let Err(e) = storage.chain().persist_ibd_utxo_flush_checkpoint(
                                effective_end_height,
                                &muhash_bytes,
                            ) {
                                warn!("IBD engine: failed to persist watermark checkpoint: {e:#}");
                            } else {
                                info!(
                                    "IBD engine: watermark checkpoint written at height {}",
                                    effective_end_height
                                );
                            }
                        }
                        Err(e) => warn!("IBD engine: watermark export failed: {e:#}"),
                    }
                }
                Err(e) => warn!("IBD engine: failed to open ibd_utxos tree for export: {e:#}"),
            }

            // IBD completed cleanly — remove the dirty flag so the next restart knows
            // this was a clean shutdown and won't force-wipe the engine unnecessarily.
            {
                let engine_path_clean = std::env::var("BLVM_IBD_ENGINE_PATH")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| {
                        std::env::temp_dir()
                            .join(format!("blvm_ibd_engine_{}.bin", std::process::id()))
                    });
                let mut dirty_os = engine_path_clean.as_os_str().to_owned();
                dirty_os.push(".dirty");
                let _ = std::fs::remove_file(std::path::PathBuf::from(&dirty_os));
            }
        }

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
        best_header_chainwork: Option<blvm_consensus::pow::U256>,
    ) -> Result<(
        std::borrow::Cow<'a, [Hash]>,
        Option<blvm_protocol::block::UtxoDelta>,
        blvm_consensus::reorganization::BlockUndoLog,
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
                    &blvm_protocol::version_bits::bip54_deployment_for_network(
                        &self.config.network,
                    ),
                )
            } else {
                None
            }
        });
        let bip54_activation_override = {
            use std::sync::atomic::Ordering;
            // Lock-free monotonic merge: only update when we have a candidate, since merging with
            // `None` is a no-op. `fetch_min` against `u64::MAX` (None sentinel) gives exact
            // `min(prev, cand)` semantics matching `merge_bip54_activation_candidate`.
            if let Some(c) = candidate {
                self.bip54_activation_from_version_bits
                    .fetch_min(c, Ordering::AcqRel);
            }
            let cur = self
                .bip54_activation_from_version_bits
                .load(Ordering::Acquire);
            if cur == u64::MAX { None } else { Some(cur) }
        };

        let bip54_active = blvm_protocol::bip_validation::is_bip54_active_at(
            height,
            self.config.network,
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
            self.config.network,
            bip54_activation_override,
            bip54_boundary,
        );
        let owned_utxo = std::mem::take(utxo_set);
        let (result, new_utxo_set, tx_ids, utxo_delta, undo_log) =
            blvm_consensus::block::connect_block_ibd_with_undo(
                block,
                witnesses,
                owned_utxo,
                height,
                &context,
                bip30_index,
                precomputed_tx_ids,
                block_arc,
                witnesses_arc,
                best_header_chainwork,
            )?;

        *utxo_set = new_utxo_set;
        match result {
            ValidationResult::Valid => Ok((tx_ids, utxo_delta, undo_log)),
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
            height,
            dir.display(),
            height
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
        pending: &mut Vec<(
            Arc<Block>,
            Arc<Vec<Vec<Witness>>>,
            u64,
            blvm_consensus::reorganization::BlockUndoLog,
        )>,
    ) -> Result<()> {
        self.flush_pending_blocks_with_opts(
            blockstore,
            _storage,
            pending,
            IbdBlockFlushOpts::default(),
        )
    }

    pub(crate) fn flush_pending_blocks_with_opts(
        &self,
        blockstore: &BlockStore,
        _storage: Option<&Arc<Storage>>,
        pending: &mut Vec<(
            Arc<Block>,
            Arc<Vec<Vec<Witness>>>,
            u64,
            blvm_consensus::reorganization::BlockUndoLog,
        )>,
        opts: IbdBlockFlushOpts,
    ) -> Result<()> {
        let to_flush = std::mem::take(pending);
        Self::do_flush_to_storage(blockstore, _storage, to_flush, opts)
    }

    /// Core flush logic. Takes ownership of pending. Used by sync flush and async spawn.
    /// Blocks are Arc<Block>; we try_unwrap to get owned Block for serialization (sync has completed).
    pub(crate) fn do_flush_to_storage(
        blockstore: &BlockStore,
        _storage: Option<&Arc<Storage>>,
        pending: Vec<(
            Arc<Block>,
            Arc<Vec<Vec<Witness>>>,
            u64,
            blvm_consensus::reorganization::BlockUndoLog,
        )>,
        opts: IbdBlockFlushOpts,
    ) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }

        let count = pending.len();
        let start = std::time::Instant::now();

        // Unwrap Arcs to get owned Block (sync has completed; refcount should be 1 when validation
        // holds the only Arc after dequeue). Witness Arc is cloned only when try_unwrap fails.
        let mut pending: Vec<(
            Block,
            Arc<Vec<Vec<Witness>>>,
            u64,
            blvm_consensus::reorganization::BlockUndoLog,
        )> = pending
            .into_iter()
            .map(|(arc_block, w, h, undo)| {
                let block = Arc::try_unwrap(arc_block).unwrap_or_else(|a| (*a).clone());
                (block, w, h, undo)
            })
            .collect();

        let flush_max_height = pending.iter().map(|(_, _, h, _)| *h).max().unwrap_or(0);

        // Sort by height once so each chunk is already in LSM-friendly order and we
        // can treat chunk indices as the flush_order directly (no secondary sort needed).
        pending.sort_by_key(|(_, _, h, _)| *h);

        pending = pending
            .into_iter()
            .map(|(block, witnesses, height, undo)| {
                let (block, witnesses) = crate::module::pipeline::try_filter_block_before_store(
                    height, block, witnesses,
                );
                (block, witnesses, height, undo)
            })
            .collect();

        // Returns true only if there is actual witness data (non-empty stack items).
        // An all-empty Vec<Vec<Witness>> (pre-SegWit blocks) does NOT count as having witnesses
        // and should not be stored, to avoid blocking re-download of SegWit blocks later.
        let block_has_witness_data = |w: &[Vec<Witness>]| {
            w.iter()
                .any(|tx_w| tx_w.iter().any(|stack| !stack.is_empty()))
        };

        let n = pending.len();
        // Serialise and flush in fixed-size chunks to bound peak RSS.
        // At h=700k each block averages ~1.5 MB serialised; flushing 300 blocks at once
        // previously allocated ~450 MB for `block_data` and then duplicated those bytes
        // inside the RocksDB WriteBatch (~900 MB transient spike). A chunk of 50 keeps
        // the spike at ~75 MB + ~75 MB = ~150 MB per iteration.
        const FLUSH_BLOCK_CHUNK_SIZE: usize = 50;

        #[cfg(feature = "rayon")]
        let parallel_serialize = opts.parallel_serialize;
        #[cfg(not(feature = "rayon"))]
        let parallel_serialize = false;

        if opts.log_progress && n > 0 {
            info!(
                "IBD block flush: storing {} blocks (heights {}-{})",
                n,
                pending.first().map(|(_, _, h, _)| *h).unwrap_or(0),
                flush_max_height,
            );
        }

        // Saved during the last chunk iteration for the update_tip call below.
        let mut tip_hash_for_update: Option<Hash> = None;

        for chunk_start in (0..n).step_by(FLUSH_BLOCK_CHUNK_SIZE) {
            let chunk_end = (chunk_start + FLUSH_BLOCK_CHUNK_SIZE).min(n);
            let is_last_chunk = chunk_end == n;
            let chunk = &pending[chunk_start..chunk_end];
            let chunk_t0 = std::time::Instant::now();
            if opts.log_progress {
                let lo = chunk.first().map(|(_, _, h, _)| *h).unwrap_or(0);
                let hi = chunk.last().map(|(_, _, h, _)| *h).unwrap_or(lo);
                info!(
                    "IBD block flush: chunk {}-{}/{} (heights {}-{})",
                    chunk_start + 1,
                    chunk_end,
                    n,
                    lo,
                    hi,
                );
            }

            // ── Serialise this chunk ──────────────────────────────────────────────
            // header_data uses Arc to avoid cloning Vec on cache hit.
            let (block_hashes, block_data, header_data): (
                Vec<Hash>,
                Vec<Vec<u8>>,
                Vec<Arc<Vec<u8>>>,
            ) = {
                let _ibd_header_cache_bypass =
                    crate::storage::serialization_cache::IbdHeaderSerializeCacheBypassGuard::enter(
                    );
                use crate::storage::serialization_cache::{
                    cache_serialized_header, get_cached_serialized_header,
                };
                if parallel_serialize {
                    #[cfg(feature = "rayon")]
                    {
                        use blvm_protocol::rayon::iter::IntoParallelRefIterator;
                        use blvm_protocol::rayon::prelude::*;
                        let block_hashes: Vec<Hash> = chunk
                            .par_iter()
                            .map(|(block, _, _, _)| blockstore.get_block_hash(block))
                            .collect();

                        let block_data: Vec<Vec<u8>> = chunk
                            .par_iter()
                            .map(|(block, _, _, _)| {
                                bincode::serialize(block)
                                    .map_err(|e| anyhow::anyhow!("Block serialization failed: {e}"))
                            })
                            .collect::<Result<Vec<_>>>()?;

                        let header_data: Vec<Arc<Vec<u8>>> = chunk
                            .par_iter()
                            .zip(block_hashes.par_iter())
                            .map(|((block, _, _, _), block_hash)| {
                                if let Some(cached) = get_cached_serialized_header(block_hash) {
                                    return Ok(cached);
                                }
                                let serialized =
                                    bincode::serialize(&block.header).map_err(|e| {
                                        anyhow::anyhow!("Header serialization failed: {e}")
                                    })?;
                                cache_serialized_header(*block_hash, serialized.clone());
                                Ok(Arc::new(serialized))
                            })
                            .collect::<Result<Vec<_>>>()?;

                        (block_hashes, block_data, header_data)
                    }
                    #[cfg(not(feature = "rayon"))]
                    {
                        unreachable!("parallel_serialize requires rayon feature")
                    }
                } else {
                    let block_hashes: Vec<Hash> = chunk
                        .iter()
                        .map(|(block, _, _, _)| blockstore.get_block_hash(block))
                        .collect();

                    let block_data: Vec<Vec<u8>> = chunk
                        .iter()
                        .map(|(block, _, _, _)| {
                            bincode::serialize(block)
                                .map_err(|e| anyhow::anyhow!("Block serialization failed: {e}"))
                        })
                        .collect::<Result<Vec<_>>>()?;

                    let header_data: Vec<Arc<Vec<u8>>> = chunk
                        .iter()
                        .zip(block_hashes.iter())
                        .map(|((block, _, _, _), block_hash)| {
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

            let witness_blobs: Vec<Option<Vec<u8>>> =
                if chunk.iter().any(|(_, w, _, _)| block_has_witness_data(w)) {
                    if parallel_serialize {
                        #[cfg(feature = "rayon")]
                        {
                            use blvm_protocol::rayon::iter::IntoParallelRefIterator;
                            use blvm_protocol::rayon::prelude::*;
                            let witness_data_vec: Vec<(usize, Vec<u8>)> = chunk
                                .par_iter()
                                .enumerate()
                                .filter_map(|(i, (_, witnesses, _, _))| {
                                    if block_has_witness_data(witnesses) {
                                        match bincode::serialize(witnesses.as_ref()) {
                                            Ok(data) => Some((i, data)),
                                            Err(_) => None,
                                        }
                                    } else {
                                        None
                                    }
                                })
                                .collect();

                            let mut v = vec![None; chunk.len()];
                            for (i, data) in witness_data_vec {
                                v[i] = Some(data);
                            }
                            v
                        }
                        #[cfg(not(feature = "rayon"))]
                        {
                            unreachable!("parallel_serialize requires rayon feature")
                        }
                    } else {
                        let mut v = vec![None; chunk.len()];
                        for i in 0..chunk.len() {
                            let witnesses = &chunk[i].1;
                            if block_has_witness_data(witnesses) {
                                v[i] =
                                    Some(bincode::serialize(witnesses.as_ref()).map_err(|e| {
                                        anyhow::anyhow!("Failed to serialize witnesses: {}", e)
                                    })?);
                            }
                        }
                        v
                    }
                } else {
                    vec![None; chunk.len()]
                };

            let metadata_blobs: Vec<Vec<u8>> = (0..chunk.len())
                .map(|i| {
                    let metadata = BlockMetadata {
                        n_tx: chunk[i].0.transactions.len() as u32,
                    };
                    bincode::serialize(&metadata)
                        .map_err(|e| anyhow::anyhow!("Block metadata serialization failed: {}", e))
                })
                .collect::<Result<Vec<_>>>()?;

            // flush_order is 0..chunk.len() — pending was sorted by height above, so each
            // chunk is already in ascending height order; no secondary sort needed.
            let flush_order: Vec<usize> = (0..chunk.len()).collect();
            let chunk_heights: Vec<u64> = chunk.iter().map(|(_, _, h, _)| *h).collect();

            // RECENT_HEADERS_TABLE is a sliding window of the last ~11 blocks. Only the
            // final chunk's entries matter for the end state; passing empty for earlier
            // chunks produces the same result without touching the table N-1 extra times.
            #[cfg(any(feature = "rocksdb", feature = "redb", feature = "tidesdb"))]
            let recent_entries: Vec<(u64, Vec<u8>)> = if is_last_chunk {
                flush_order
                    .iter()
                    .rev()
                    .take(11)
                    .map(|&idx| {
                        let h = chunk[idx].2;
                        let data = header_data[idx].as_slice().to_vec();
                        Ok((h, data))
                    })
                    .collect::<Result<Vec<_>>>()?
            } else {
                vec![]
            };

            // Save the tip block hash from the last block in the sorted last chunk.
            if is_last_chunk {
                tip_hash_for_update = block_hashes.last().copied();
            }

            // ── Write this chunk ──────────────────────────────────────────────────
            let mut storage_unified = false;
            #[cfg(feature = "rocksdb")]
            {
                if blockstore.try_ibd_flush_rocksdb_unified(
                    &flush_order,
                    &chunk_heights,
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
                        &chunk_heights,
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
                        &chunk_heights,
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
            #[cfg(feature = "heed3")]
            {
                if !storage_unified
                    && blockstore.try_ibd_flush_heed3_unified(
                        &flush_order,
                        &chunk_heights,
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
                {
                    let blocks_tree = blockstore.blocks_tree()?;
                    let mut batch = blocks_tree.batch()?;
                    for &i in &flush_order {
                        let height = chunk[i].2;
                        let key = block_height_row_key(height, &block_hashes[i]);
                        batch.put(&key, &block_data[i]);
                    }
                    batch.commit_no_wal()?;
                }
                {
                    let headers_tree = blockstore.headers_tree()?;
                    let mut batch = headers_tree.batch()?;
                    for &i in &flush_order {
                        let height = chunk[i].2;
                        let key = block_height_row_key(height, &block_hashes[i]);
                        batch.put(&key, header_data[i].as_slice());
                    }
                    batch.commit_no_wal()?;
                }
                {
                    let has_witnesses = witness_blobs.iter().any(|b| b.is_some());
                    if has_witnesses {
                        let witnesses_tree = blockstore.witnesses_tree()?;
                        let mut batch = witnesses_tree.batch()?;
                        for &i in &flush_order {
                            if let Some(ref data) = witness_blobs[i] {
                                let height = chunk[i].2;
                                let key = block_height_row_key(height, &block_hashes[i]);
                                batch.put(&key, data);
                            }
                        }
                        batch.commit_no_wal()?;
                    }
                }
                {
                    let height_tree = blockstore.height_tree()?;
                    let mut batch = height_tree.batch()?;
                    for &i in &flush_order {
                        let height = chunk[i].2;
                        let height_key = height.to_be_bytes();
                        batch.put(&height_key, &block_hashes[i]);
                    }
                    batch.commit_no_wal()?;
                }
                {
                    let ht_tree = blockstore.hash_to_height_tree()?;
                    let mut batch = ht_tree.batch()?;
                    for &i in &flush_order {
                        let height_bytes = chunk[i].2.to_be_bytes();
                        batch.put(&block_hashes[i], &height_bytes);
                    }
                    batch.commit_no_wal()?;
                }
                {
                    let meta_tree = blockstore.metadata_tree()?;
                    let mut batch = meta_tree.batch()?;
                    for &i in &flush_order {
                        let key = block_height_row_key(chunk[i].2, &block_hashes[i]);
                        batch.put(&key, &metadata_blobs[i]);
                    }
                    batch.commit_no_wal()?;
                }
                // Recent headers only needed for the last chunk (same rationale as above).
                if is_last_chunk {
                    let recent_batch: Vec<(u64, &BlockHeader)> = chunk
                        .iter()
                        .rev()
                        .take(11)
                        .map(|(block, _, height, _)| (*height, &block.header))
                        .collect();
                    blockstore.store_recent_headers_ibd_batch(&recent_batch)?;
                }
            }

            if let Some(storage) = _storage {
                blocks::index_ibd_flushed_blocks(storage, &block_hashes, chunk, &chunk_heights)?;
            }

            for i in 0..block_hashes.len() {
                blockstore.store_undo_log(&block_hashes[i], &chunk[i].3)?;
            }

            if opts.log_progress {
                info!(
                    "IBD block flush: chunk {}-{}/{} done in {:?}",
                    chunk_start + 1,
                    chunk_end,
                    n,
                    chunk_t0.elapsed(),
                );
            }

            // block_data, header_data, witness_blobs, metadata_blobs are dropped here,
            // releasing the serialised bytes before the next chunk is allocated.
        }

        #[cfg(feature = "profile")]
        {
            blvm_protocol::profile_log!(
                "[FLUSH_STORAGE_PERF] blocks={} max_height={} total_ms={}",
                count,
                flush_max_height,
                start.elapsed().as_millis()
            );
        }

        // Chain metadata: parallel IBD bypasses `run_loop`, so `update_tip` must run here
        // or `get_height()` / restarts see `chain_info` missing despite full block index.
        // pending is sorted by height; last entry is the tip.
        if let Some(storage) = _storage {
            if let Some(tip_hash) = tip_hash_for_update {
                if let Some((block, _, tip_height, _)) = pending.last() {
                    storage
                        .chain()
                        .update_tip(&tip_hash, &block.header, *tip_height)?;
                }
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

    /// Isolate tests from shell `BLVM_IBD_*` (e.g. left over from manual IBD runs).
    fn with_ibd_env_cleared<F: FnOnce()>(f: F) {
        let peers = std::env::var("BLVM_IBD_PEERS").ok();
        let mode = std::env::var("BLVM_IBD_MODE").ok();
        let wan_single = std::env::var("BLVM_IBD_WAN_SINGLE_PEER").ok();
        unsafe {
            std::env::remove_var("BLVM_IBD_PEERS");
            std::env::remove_var("BLVM_IBD_MODE");
            std::env::remove_var("BLVM_IBD_WAN_SINGLE_PEER");
        }
        f();
        unsafe {
            if let Some(v) = peers {
                std::env::set_var("BLVM_IBD_PEERS", v);
            } else {
                std::env::remove_var("BLVM_IBD_PEERS");
            }
            if let Some(v) = mode {
                std::env::set_var("BLVM_IBD_MODE", v);
            } else {
                std::env::remove_var("BLVM_IBD_MODE");
            }
            if let Some(v) = wan_single {
                std::env::set_var("BLVM_IBD_WAN_SINGLE_PEER", v);
            } else {
                std::env::remove_var("BLVM_IBD_WAN_SINGLE_PEER");
            }
        }
    }

    #[test]
    fn ibd_block_flush_opts_default_enables_parallel_serialize() {
        let opts = IbdBlockFlushOpts::default();
        assert!(opts.parallel_serialize);
        assert!(!opts.log_progress);
    }

    #[test]
    fn ibd_block_flush_opts_shutdown_sync_is_serial_with_progress() {
        let opts = IbdBlockFlushOpts::shutdown_sync();
        assert!(!opts.parallel_serialize);
        assert!(opts.log_progress);
    }

    #[test]
    fn test_parallel_ibd_config_default() {
        let config = ParallelIBDConfig::default();
        assert!(config.num_workers > 0);
        // chunk_size: 128 default, or BLVM_IBD_CHUNK_SIZE (16-2000) if set
        assert!(
            config.chunk_size >= 16 && config.chunk_size <= 2000,
            "chunk_size={}",
            config.chunk_size
        );
        assert_eq!(config.max_concurrent_per_peer, 64);
    }

    #[test]
    fn empty_blvm_ibd_peers_env_allows_auto_lan() {
        with_ibd_env_cleared(|| {
            unsafe {
                std::env::set_var("BLVM_IBD_PEERS", "");
            }
            let peers = vec!["192.168.2.100:8333".to_string(), "8.8.8.8:8333".to_string()];
            let config = ParallelIBDConfig::resolve_for_session(None, 0, &peers);
            assert_eq!(config.preferred_peers, vec!["192.168.2.100:8333"]);
        });
    }

    #[test]
    fn wan_multi_peer_keeps_all_peers_by_default() {
        let peers = vec!["8.8.8.8:8333".to_string(), "1.1.1.1:8333".to_string()];
        let out = ParallelIBDConfig::collapse_wan_only_download_peers(peers);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn collapse_keeps_multi_peer_when_lan_present() {
        let peers = vec!["192.168.1.1:8333".to_string(), "8.8.8.8:8333".to_string()];
        let out = ParallelIBDConfig::collapse_wan_only_download_peers(peers);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn resolve_wan_only_keeps_parallel_mode() {
        with_ibd_env_cleared(|| {
            let peers = vec!["8.8.8.8:8333".to_string(), "1.1.1.1:8333".to_string()];
            let config = ParallelIBDConfig::resolve_for_session(None, 100_000, &peers);
            assert_eq!(config.mode, "parallel");
            assert!(config.preferred_peers.is_empty());
            assert_eq!(config.min_peers_for_ibd(), 2);
        });
    }

    #[test]
    fn resolve_auto_prefers_lan_peers() {
        with_ibd_env_cleared(|| {
            let peers = vec!["192.168.2.100:8333".to_string(), "8.8.8.8:8333".to_string()];
            let config = ParallelIBDConfig::resolve_for_session(None, 100_000, &peers);
            assert_eq!(config.preferred_peers, vec!["192.168.2.100:8333"]);
            assert_eq!(config.min_peers_for_ibd(), 1);
        });
    }

    #[test]
    fn filter_ibd_download_peers_falls_back_when_none_connected() {
        let preferred = vec!["192.168.1.10:8333".to_string()];
        let connected = vec!["8.8.8.8:8333".to_string(), "1.1.1.1:8333".to_string()];
        let out = super::filter_ibd_download_peers(&preferred, connected.clone());
        assert_eq!(out, connected);
    }

    #[test]
    fn filter_ibd_download_peers_matches_host_without_port() {
        let preferred = vec!["192.168.1.10".to_string()];
        let connected = vec!["192.168.1.10:8333".to_string(), "8.8.8.8:8333".to_string()];
        let out = super::filter_ibd_download_peers(&preferred, connected);
        assert_eq!(out, vec!["192.168.1.10:8333".to_string()]);
    }

    #[test]
    fn resolve_fresh_chain_keeps_parallel_mode() {
        with_ibd_env_cleared(|| {
            let peers = vec!["192.168.1.1:8333".to_string()];
            let config = ParallelIBDConfig::resolve_for_session(None, 0, &peers);
            assert_eq!(config.mode, "parallel");
        });
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

    // Regression: chunk queue must drain in height order (FIFO). Vec::pop would yield highest
    // heights first and break sequential validation.

    #[test]
    fn test_work_queue_fifo_order_not_lifo() {
        // Queue uses VecDeque::pop_front — lowest-height chunk leaves first.

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
        // Vec::pop takes from the end — wrong order if used as a download work queue.

        let mut vec_queue: Vec<(u64, u64)> = vec![(0, 99), (100, 199), (200, 299)];

        let popped = vec_queue.pop().unwrap();
        assert_eq!(
            popped,
            (200, 299),
            "Vec::pop() returns LAST element (LIFO behavior)"
        );
    }

    #[test]
    fn test_vecdeque_pop_front_is_fifo_correct() {
        let mut deque_queue: VecDeque<(u64, u64, Option<String>)> =
            VecDeque::from(vec![(0, 99, None), (100, 199, None), (200, 299, None)]);

        let (s, e, _) = deque_queue.pop_front().unwrap();
        assert_eq!(
            (s, e),
            (0, 99),
            "VecDeque::pop_front() returns FIRST element (FIFO behavior)"
        );
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
        assert_ne!(
            checkpoints::MAINNET_CHECKPOINTS.len(),
            0,
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
        // 16 = Core-like minimum, 128 = default, 2000 = max (BLVM_IBD_CHUNK_SIZE override)
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
