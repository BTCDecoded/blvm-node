//! Block chunk download for parallel IBD.
//!
//! Downloads blocks from a peer using pipelined GetData requests.
//! Core-style: max 16 blocks in flight per peer across all workers.

use super::types::{SharedBlock, SharedWitnesses};
use crate::network::NetworkManager;
use crate::network::inventory::{MSG_BLOCK, MSG_WITNESS_BLOCK};
use crate::network::protocol::{GetDataMessage, InventoryVector, ProtocolMessage, ProtocolParser};
use crate::storage::blockstore::BlockStore;
use anyhow::{Context, Result};
use blvm_protocol::features::FeatureRegistry;
use blvm_protocol::{Block, Hash, ProtocolVersion, segwit::Witness};
use futures::stream::{FuturesUnordered, StreamExt};
use hex;
use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::sync::broadcast;
use tokio::time::{Duration, timeout};
use tracing::{info, warn};

use super::ParallelIBDConfig;

/// Load block + witnesses from disk when complete for IBD replay (skips network).
fn try_load_local_ibd_block(
    blockstore: &BlockStore,
    height: u64,
    expected_hash: Hash,
    protocol_version: ProtocolVersion,
) -> Result<Option<(Block, Vec<Vec<Witness>>)>> {
    let Some(block) = blockstore.get_block(&expected_hash)? else {
        return Ok(None);
    };
    if blockstore.get_block_hash(&block) != expected_hash {
        return Ok(None);
    }
    let registry = FeatureRegistry::for_protocol(protocol_version);
    let ts = block.header.timestamp;
    let segwit_on = registry.is_feature_active("segwit", height, ts);
    // Returns true only when there is at least one non-empty witness stack item.
    // An all-empty structure comes from pre-MSG_WITNESS_BLOCK downloads and must be treated
    // the same as None so the block is re-fetched with full witness data.
    let has_real_witnesses =
        |w: &[Vec<Witness>]| w.iter().any(|tx_w| tx_w.iter().any(|s| !s.is_empty()));

    let witnesses = match blockstore.get_witness(&expected_hash)? {
        Some(w) if has_real_witnesses(&w) => w,
        // Stale all-empty blob stored by a prior MSG_BLOCK download: treat as missing.
        Some(_) if segwit_on => return Ok(None),
        Some(w) => w, // pre-segwit block: empty witnesses are correct
        None if !segwit_on => Vec::new(),
        None => return Ok(None),
    };
    Ok(Some((block, witnesses)))
}

/// Result of [`download_chunk`]: when streaming to the coordinator, `blocks` is empty and
/// `streamed_block_count` holds the number sent; otherwise `blocks` contains the full chunk.
/// Blocks and witnesses are Arc-wrapped so they can be passed cheaply through further pipeline stages.
pub(crate) struct DownloadChunkResult {
    pub blocks: Vec<(u64, SharedBlock, SharedWitnesses)>,
    pub streamed_block_count: usize,
}

impl DownloadChunkResult {
    #[inline]
    pub fn block_count(&self) -> usize {
        if self.blocks.is_empty() {
            self.streamed_block_count
        } else {
            self.blocks.len()
        }
    }
}

/// Tracks block download progress for stalling detection
struct BlockDownloadProgress {
    last_block_hash: Option<Hash>,
    last_progress_time: std::time::Instant,
    current_timeout_seconds: u64,
    disconnected_peers_count: usize,
}

impl BlockDownloadProgress {
    fn new() -> Self {
        Self {
            last_block_hash: None,
            last_progress_time: std::time::Instant::now(),
            current_timeout_seconds: 120,
            disconnected_peers_count: 0,
        }
    }

    fn record_progress(&mut self, block_hash: Hash) {
        self.last_block_hash = Some(block_hash);
        self.last_progress_time = std::time::Instant::now();
    }

    fn reset_timeout(&mut self) {
        self.current_timeout_seconds = 120;
        self.disconnected_peers_count = 0;
    }
}

/// `send_to_peer` can fail transiently (`Peer not found`, disconnected, send channel closed).
/// Failing the entire chunk on first error deadlocks IBD when `BLVM_IBD_PEERS` is a single LAN node:
/// the assigner requeues to the same peer, validation stalls, and `/common` profile runs hit timeout.
async fn send_block_getdata_with_retry(
    network: Arc<NetworkManager>,
    peer_addr: SocketAddr,
    wire_msg: Vec<u8>,
    height: u64,
) -> Result<()> {
    const MAX_ATTEMPTS: u32 = 30;
    const BASE_MS: u64 = 100;
    const MAX_WAIT_MS: u64 = 5_000;
    let mut attempt: u32 = 0;
    let mut reconnect_spawned = false;
    loop {
        match network.send_to_peer(peer_addr, wire_msg.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let msg = e.to_string();
                let is_gone = msg.contains("not found") || msg.contains("disconnected");
                if !reconnect_spawned && is_gone {
                    reconnect_spawned = true;
                    NetworkManager::spawn_outbound_reconnect_attempt(
                        Arc::clone(&network),
                        peer_addr,
                    );
                }
                attempt += 1;
                let wait_ms = BASE_MS
                    .saturating_mul(1u64 << (attempt - 1).min(6))
                    .min(MAX_WAIT_MS);
                if attempt >= MAX_ATTEMPTS {
                    return Err(e).with_context(|| {
                        format!(
                            "Failed to send GetData for block at height {height} after {MAX_ATTEMPTS} attempts"
                        )
                    });
                }
                if attempt <= 3 || attempt % 5 == 0 {
                    warn!(
                        "GetData send failed for height {} (attempt {}/{}): {} — retrying in {}ms",
                        height, attempt, MAX_ATTEMPTS, e, wait_ms
                    );
                }
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            }
        }
    }
}

/// Register a block download and send GetData. Fails fast when the peer is gone (avoids orphan
/// pending requests that time out and blacklist the whole chunk).
async fn register_and_request_block(
    network: Arc<NetworkManager>,
    peer_addr: SocketAddr,
    peer_id: &str,
    block_hash: Hash,
    height: u64,
) -> Result<tokio::sync::oneshot::Receiver<(Block, Vec<Vec<Witness>>)>> {
    if !network.is_peer_connected(peer_addr).await {
        return Err(anyhow::anyhow!(
            "Peer {peer_id} not connected — cannot request block at height {height}"
        ));
    }
    let block_rx = network.register_block_request(peer_addr, block_hash);
    let inventory = vec![InventoryVector {
        inv_type: MSG_WITNESS_BLOCK,
        hash: block_hash,
    }];
    let wire_msg =
        ProtocolParser::serialize_message(&ProtocolMessage::GetData(GetDataMessage { inventory }))?;
    if let Err(e) =
        send_block_getdata_with_retry(Arc::clone(&network), peer_addr, wire_msg, height).await
    {
        network.cancel_block_request(peer_addr, block_hash);
        return Err(e);
    }
    Ok(block_rx)
}

type PendingBlockResult = (
    u64,
    [u8; 32],
    std::time::Instant,
    Result<
        Result<(Block, Vec<Vec<Witness>>), tokio::sync::oneshot::error::RecvError>,
        tokio::time::error::Elapsed,
    >,
    Option<tokio::sync::OwnedSemaphorePermit>,
);

type PendingBlockFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = PendingBlockResult> + Send>>;

/// Enqueue one block download (local replay or network GetData). Skips if already in flight.
async fn enqueue_chunk_block(
    height: u64,
    block_hash: [u8; 32],
    network: &Arc<NetworkManager>,
    peer_addr: SocketAddr,
    peer_id: &str,
    blockstore: &BlockStore,
    protocol_version: ProtocolVersion,
    timeout_duration: Duration,
    blocks_sem: &Option<Arc<Semaphore>>,
    in_flight: &mut FuturesUnordered<PendingBlockFuture>,
    in_flight_heights: &mut HashSet<u64>,
    first_block_logged: &mut bool,
    start_height: u64,
    end_height: u64,
) -> Result<()> {
    if in_flight_heights.contains(&height) {
        return Ok(());
    }
    let permit = match blocks_sem {
        Some(sem) => Some(
            sem.clone()
                .acquire_owned()
                .await
                .map_err(|_| anyhow::anyhow!("blocks semaphore closed"))?,
        ),
        None => None,
    };
    if let Some((block, block_witnesses)) =
        try_load_local_ibd_block(blockstore, height, block_hash, protocol_version)?
    {
        if !*first_block_logged {
            info!(
                "[IBD] {} chunk {}-{}: local block height {} (hash {})",
                peer_id,
                start_height,
                end_height,
                height,
                hex::encode(block_hash)
            );
            *first_block_logged = true;
        }
        let request_start = std::time::Instant::now();
        in_flight_heights.insert(height);
        in_flight.push(Box::pin(async move {
            let r = Ok(Ok((block, block_witnesses)));
            (height, block_hash, request_start, r, permit)
        }));
    } else {
        if !*first_block_logged {
            info!(
                "[IBD] {} chunk {}-{}: registered block height {} (hash {})",
                peer_id,
                start_height,
                end_height,
                height,
                hex::encode(block_hash)
            );
            *first_block_logged = true;
        }
        let block_rx =
            register_and_request_block(Arc::clone(network), peer_addr, peer_id, block_hash, height)
                .await?;
        let request_start = std::time::Instant::now();
        in_flight_heights.insert(height);
        in_flight.push(Box::pin(async move {
            let r = timeout(timeout_duration, block_rx).await;
            (height, block_hash, request_start, r, permit)
        }));
    }
    Ok(())
}

/// Download a chunk of blocks from a peer.
///
/// When block_tx is Some, streams each block immediately so validation doesn't wait for full chunk.
/// blocks_sem: Core-style limit — max 16 blocks in flight per peer across all workers.
/// stall_rx: When coordinator stalls, it broadcasts the needed height; worker aborts if our chunk contains it.
pub(crate) async fn download_chunk(
    start_height: u64,
    end_height: u64,
    peer_id: &str,
    network: Option<Arc<NetworkManager>>,
    blockstore: &BlockStore,
    config: &ParallelIBDConfig,
    peer_scorer: Arc<crate::network::peer_scoring::PeerScorer>,
    block_tx: Option<tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>>,
    blocks_sem: Option<Arc<Semaphore>>,
    mut stall_rx: Option<&mut broadcast::Receiver<u64>>,
    protocol_version: ProtocolVersion,
) -> Result<DownloadChunkResult> {
    let streaming = block_tx.is_some();
    let mut blocks = Vec::new();
    let mut streamed_block_count: usize = 0;
    let mut progress = BlockDownloadProgress::new();
    // Used to detect genuinely stuck partial chunks: abort if stall signal arrives
    // and we have been active for >PARTIAL_STALL_ABORT_SECS without delivering the needed block.
    let chunk_start_time = std::time::Instant::now();

    // Drain stale stall broadcasts accumulated while this worker was finishing its previous chunk.
    // Workers hold stall_rx across the entire worker-task lifetime (one subscription, many chunks).
    // A broadcast sent during the previous chunk's work sits unread in the channel. Without
    // draining, the very first select! poll in the "no first block yet" branch fires the stale
    // signal immediately — "no first block yet" → abort → re-queue → same broadcast fires again.
    // Draining here gives this chunk a clean slate; only broadcasts sent AFTER we start are relevant.
    if let Some(ref mut rx) = stall_rx {
        loop {
            match rx.try_recv() {
                Ok(_) => continue,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break, // Empty or Closed
            }
        }
    }

    info!(
        "Downloading chunk from peer {}: heights {} to {}",
        peer_id, start_height, end_height
    );

    let network = match network {
        Some(n) => n,
        None => {
            warn!("NetworkManager not available, skipping block download");
            return Ok(DownloadChunkResult {
                blocks,
                streamed_block_count: 0,
            });
        }
    };

    let peer_addr = peer_id
        .parse::<SocketAddr>()
        .map_err(|_| anyhow::anyhow!("Invalid peer address: {}", peer_id))?;

    if !network.is_peer_connected(peer_addr).await {
        return Err(anyhow::anyhow!(
            "Peer {peer_id} not connected at chunk {}-{} start — chunk needs retry",
            start_height,
            end_height
        ));
    }

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

    let base_timeout_secs = config.download_timeout_secs;
    let timeout_duration = Duration::from_secs(base_timeout_secs);
    let first_block_wait = timeout_duration;

    let pipeline_depth: usize = blocks_sem
        .as_ref()
        .map(|_| config.max_blocks_in_transit_per_peer)
        .unwrap_or(config.max_concurrent_per_peer);

    let mut in_flight: FuturesUnordered<PendingBlockFuture> = FuturesUnordered::new();
    let block_hash_by_height: BTreeMap<u64, [u8; 32]> = block_hashes.into_iter().collect();
    let mut in_flight_heights: HashSet<u64> = HashSet::new();
    // Arc-wrap immediately so downstream pipeline stages never deep-copy block bytes.
    let mut received: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let mut next_to_send = start_height;

    let mut first_block_logged = false;

    async fn fill_pipeline(
        next_to_send: u64,
        end_height: u64,
        pipeline_depth: usize,
        received: &BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
        in_flight: &mut FuturesUnordered<PendingBlockFuture>,
        in_flight_heights: &mut HashSet<u64>,
        block_hash_by_height: &BTreeMap<u64, [u8; 32]>,
        network: &Arc<NetworkManager>,
        peer_addr: SocketAddr,
        peer_id: &str,
        blockstore: &BlockStore,
        protocol_version: ProtocolVersion,
        timeout_duration: Duration,
        blocks_sem: &Option<Arc<Semaphore>>,
        first_block_logged: &mut bool,
        start_height: u64,
    ) -> Result<()> {
        while in_flight.len() < pipeline_depth {
            let Some(height) = (next_to_send..=end_height)
                .find(|h| !received.contains_key(h) && !in_flight_heights.contains(h))
            else {
                break;
            };
            let block_hash = *block_hash_by_height
                .get(&height)
                .ok_or_else(|| anyhow::anyhow!("Block hash missing for height {height}"))?;
            enqueue_chunk_block(
                height,
                block_hash,
                network,
                peer_addr,
                peer_id,
                blockstore,
                protocol_version,
                timeout_duration,
                blocks_sem,
                in_flight,
                in_flight_heights,
                first_block_logged,
                start_height,
                end_height,
            )
            .await?;
        }
        Ok(())
    }

    fill_pipeline(
        next_to_send,
        end_height,
        pipeline_depth,
        &received,
        &mut in_flight,
        &mut in_flight_heights,
        &block_hash_by_height,
        &network,
        peer_addr,
        peer_id,
        blockstore,
        protocol_version,
        timeout_duration,
        &blocks_sem,
        &mut first_block_logged,
        start_height,
    )
    .await?;

    // Hard deadline: abort only when the gap height (next_to_send) makes no progress.
    // Out-of-order receives ahead of the gap must not extend the deadline indefinitely.
    const CHUNK_DEADLINE_SECS: u64 = 30;
    let mut last_gap_at = chunk_start_time;
    let mut deadline_poll = tokio::time::interval(Duration::from_secs(1));
    deadline_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the immediate first tick so the first poll is ~1s out.
    deadline_poll.tick().await;

    loop {
        let next_result = if progress.last_block_hash.is_none() {
            if let Some(ref mut rx) = stall_rx {
                // biased: poll in_flight first so a locally-cached block (resolved immediately)
                // is never preempted by a stall signal that arrived in the broadcast channel
                // before this select runs. Without biased, tokio may pick stall_rx non-deterministically
                // and abort a chunk we're already holding the first block for.
                tokio::select! {
                    biased;
                    r = in_flight.next() => r,
                    stall_res = rx.recv() => {
                        if let Ok(stall_h) = stall_res {
                            if stall_h >= start_height && stall_h <= end_height {
                                // Ignore premature coordinator stalls while the first block is
                                // still in flight (common at height 1 before peer responds).
                                if chunk_start_time.elapsed() < first_block_wait {
                                    continue;
                                }
                                warn!("Coordinator stall at {}: aborting chunk {}-{} (no first block yet)", stall_h, start_height, end_height);
                                return Err(anyhow::anyhow!(
                                    "Coordinator stall: aborting chunk {}-{} for retry",
                                    start_height, end_height
                                ));
                            }
                        }
                        continue;
                    }
                    _ = tokio::time::sleep(first_block_wait) => {
                        warn!("Chunk {} to {}: no first block in {}s, failing for retry", start_height, end_height, base_timeout_secs);
                        return Err(anyhow::anyhow!(
                            "Block download stalled (no first block in {base_timeout_secs}s)"
                        ));
                    }
                }
            } else {
                tokio::select! {
                    r = in_flight.next() => r,
                    _ = tokio::time::sleep(first_block_wait) => {
                        warn!("Chunk {} to {}: no first block in {}s, failing for retry", start_height, end_height, base_timeout_secs);
                        return Err(anyhow::anyhow!(
                            "Block download stalled (no first block in {base_timeout_secs}s)"
                        ));
                    }
                }
            }
        } else if let Some(ref mut rx) = stall_rx {
            // We have started receiving blocks. Race in_flight, stall signal, and hard deadline.
            tokio::select! {
                r = in_flight.next() => r,
                _ = rx.recv() => {
                    // Stall signal received — just continue; the hard deadline handles abort timing.
                    continue;
                }
                _ = deadline_poll.tick() => {
                    if last_gap_at.elapsed() < Duration::from_secs(CHUNK_DEADLINE_SECS) {
                        continue;
                    }
                    warn!(
                        "Chunk {}-{}: hard {}s gap deadline expired (next_to_send={}, in_flight={}, received={}) — aborting for retry",
                        start_height, end_height, CHUNK_DEADLINE_SECS,
                        next_to_send, in_flight.len(), received.len()
                    );
                    peer_scorer.record_failure(peer_addr);
                    return Err(anyhow::anyhow!(
                        "Chunk hard deadline {}-{}: stuck at height {} after {}s",
                        start_height, end_height, next_to_send, CHUNK_DEADLINE_SECS
                    ));
                }
            }
        } else {
            tokio::select! {
                r = in_flight.next() => r,
                _ = deadline_poll.tick() => {
                    if last_gap_at.elapsed() < Duration::from_secs(CHUNK_DEADLINE_SECS) {
                        continue;
                    }
                    warn!(
                        "Chunk {}-{}: hard {}s gap deadline expired (no stall_rx, next_to_send={}) — aborting for retry",
                        start_height, end_height, CHUNK_DEADLINE_SECS, next_to_send
                    );
                    peer_scorer.record_failure(peer_addr);
                    return Err(anyhow::anyhow!(
                        "Chunk hard deadline {}-{}: no stall_rx, stuck at height {} after {}s",
                        start_height, end_height, next_to_send, CHUNK_DEADLINE_SECS
                    ));
                }
            }
        };

        let Some((height, block_hash, request_start, block_result, _permit)) = next_result else {
            break;
        };
        in_flight_heights.remove(&height);
        match block_result {
            Ok(Ok((block, block_witnesses))) => {
                let received_hash = blockstore.get_block_hash(&block);
                if received_hash != block_hash {
                    // BUGFIX: Previously we continued without inserting; next_to_send stayed at this
                    // height while the pipeline requested later heights, so we could return Ok with a
                    // gap and mark the chunk complete — permanent reorder-buffer stall (e.g. min 547).
                    warn!(
                        "Block hash mismatch for height {} (expected {}, got {}) — failing chunk for retry",
                        height,
                        hex::encode(block_hash),
                        hex::encode(received_hash)
                    );
                    peer_scorer.record_failure(peer_addr);
                    return Err(anyhow::anyhow!(
                        "Block hash mismatch at height {} - chunk needs retry",
                        height
                    ));
                }
                progress.record_progress(received_hash);
                progress.reset_timeout();
                let latency_ms = request_start.elapsed().as_secs_f64() * 1000.0;
                let block_size = block.header.version.to_le_bytes().len() as u64 + 80;
                peer_scorer.record_block(peer_addr, block_size, latency_ms);
                received.insert(height, (Arc::new(block), Arc::new(block_witnesses)));
                if !first_block_logged {
                    info!(
                        "[IBD] {} chunk {}-{}: first block received (h={}, {}ms)",
                        peer_id, start_height, end_height, height, latency_ms as u64
                    );
                    first_block_logged = true;
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
                if height == next_to_send {
                    warn!(
                        "Block timeout for gap height {} after {}s",
                        height, base_timeout_secs
                    );
                    peer_scorer.record_failure(peer_addr);
                    return Err(anyhow::anyhow!(
                        "Block timeout for gap height {} after {}s - chunk needs retry",
                        height,
                        base_timeout_secs
                    ));
                }
                warn!(
                    "Block timeout for height {} ahead of gap {} after {}s — re-requesting",
                    height, next_to_send, base_timeout_secs
                );
                enqueue_chunk_block(
                    height,
                    block_hash,
                    &network,
                    peer_addr,
                    peer_id,
                    blockstore,
                    protocol_version,
                    timeout_duration,
                    &blocks_sem,
                    &mut in_flight,
                    &mut in_flight_heights,
                    &mut first_block_logged,
                    start_height,
                    end_height,
                )
                .await?;
                fill_pipeline(
                    next_to_send,
                    end_height,
                    pipeline_depth,
                    &received,
                    &mut in_flight,
                    &mut in_flight_heights,
                    &block_hash_by_height,
                    &network,
                    peer_addr,
                    peer_id,
                    blockstore,
                    protocol_version,
                    timeout_duration,
                    &blocks_sem,
                    &mut first_block_logged,
                    start_height,
                )
                .await?;
                continue;
            }
        }

        while let Some((block, block_witnesses)) = received.remove(&next_to_send) {
            if let Some(ref tx) = block_tx {
                let t0 = std::time::Instant::now();
                let send_r = tx.send((next_to_send, block, block_witnesses)).await;
                let wait_ms = t0.elapsed().as_millis() as u64;
                if wait_ms >= 10 {
                    info!(
                        "[IBD_BLOCK_TX_SEND_WAIT] height={} wait_ms={} (download→coordinator channel)",
                        next_to_send, wait_ms
                    );
                }
                if send_r.is_err() {
                    return Err(anyhow::anyhow!(
                        "block_tx closed during stream - chunk needs retry"
                    ));
                }
                streamed_block_count += 1;
            } else {
                blocks.push((next_to_send, block, block_witnesses));
            }
            next_to_send += 1;
            last_gap_at = std::time::Instant::now();
        }

        fill_pipeline(
            next_to_send,
            end_height,
            pipeline_depth,
            &received,
            &mut in_flight,
            &mut in_flight_heights,
            &block_hash_by_height,
            &network,
            peer_addr,
            peer_id,
            blockstore,
            protocol_version,
            timeout_duration,
            &blocks_sem,
            &mut first_block_logged,
            start_height,
        )
        .await?;
    }

    while let Some((block, block_witnesses)) = received.remove(&next_to_send) {
        if let Some(ref tx) = block_tx {
            let t0 = std::time::Instant::now();
            let send_r = tx.send((next_to_send, block, block_witnesses)).await;
            let wait_ms = t0.elapsed().as_millis() as u64;
            if wait_ms >= 10 {
                info!(
                    "[IBD_BLOCK_TX_SEND_WAIT] height={} wait_ms={} (download→coordinator channel)",
                    next_to_send, wait_ms
                );
            }
            if send_r.is_err() {
                return Err(anyhow::anyhow!(
                    "block_tx closed during stream - chunk needs retry"
                ));
            }
            streamed_block_count += 1;
        } else {
            blocks.push((next_to_send, block, block_witnesses));
        }
        next_to_send += 1;
    }

    // BUGFIX: If in_flight drained but we could not stream start_height..=end_height in order
    // (e.g. hash mismatch previously skipped without Err), we must not report success — that
    // left permanent gaps in the coordinator reorder buffer.
    if next_to_send != end_height + 1 {
        return Err(anyhow::anyhow!(
            "Incomplete chunk {}-{}: stuck before height {} ({} heights still buffered) — chunk needs retry",
            start_height,
            end_height,
            next_to_send,
            received.len()
        ));
    }

    Ok(DownloadChunkResult {
        blocks,
        streamed_block_count: if streaming { streamed_block_count } else { 0 },
    })
}
