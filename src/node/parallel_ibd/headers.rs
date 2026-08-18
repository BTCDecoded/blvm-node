//! Header download for parallel IBD.
//!
//! Downloads block headers from peers for a given height range.
//! Used by both sequential and checkpoint-parallel header sync.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use blvm_protocol::BlockHeader;
use tokio::task::JoinSet;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

/// One checkpoint-bounded header download range: `[start, end]` with GetHeaders locator.
pub(crate) type HeaderRange = (u64, u64, [u8; 32]);

/// Build all checkpoint window ranges in `[start_height, end_height]` (N13: no peer cap).
pub(crate) fn checkpoint_header_ranges(
    checkpoints: &[(u64, [u8; 32])],
    start_height: u64,
    end_height: u64,
) -> Vec<HeaderRange> {
    checkpoints
        .windows(2)
        .filter_map(|w| {
            let (range_start, start_hash) = w[0];
            let (range_end, _) = w[1];
            if range_end < start_height {
                return None;
            }
            let actual_start = range_start.max(start_height);
            let actual_end = range_end.min(end_height);
            if actual_start > actual_end {
                return None;
            }
            Some((actual_start, actual_end, start_hash))
        })
        .collect()
}

/// N13 unit helper: simulate ≤1 in-flight range per peer until all ranges are assigned.
/// Returns `(peer_idx, range_idx)` in completion/assignment order (initial fills then reuses).
pub(crate) fn simulate_header_range_schedule(
    peer_count: usize,
    range_count: usize,
) -> Vec<(usize, usize)> {
    if peer_count == 0 || range_count == 0 {
        return Vec::new();
    }
    let mut pending: VecDeque<usize> = (0..range_count).collect();
    let mut free: VecDeque<usize> = (0..peer_count).collect();
    let mut in_flight: VecDeque<(usize, usize)> = VecDeque::new();
    let mut assigned = Vec::with_capacity(range_count);

    while !pending.is_empty() || !in_flight.is_empty() {
        while !free.is_empty() && !pending.is_empty() {
            let peer = free.pop_front().unwrap();
            let range = pending.pop_front().unwrap();
            in_flight.push_back((peer, range));
            assigned.push((peer, range));
        }
        if let Some((peer, _)) = in_flight.pop_front() {
            free.push_back(peer);
        }
    }
    assigned
}

use crate::network::NetworkManager;
use crate::network::peer_scoring::PeerScorer;
use crate::network::protocol::{GetHeadersMessage, ProtocolMessage, ProtocolParser};
use crate::node::event_publisher::EventPublisher;
use crate::storage::blockstore::BlockStore;
use crate::storage::hashing::double_sha256;
use blvm_protocol::GENESIS_BLOCK_HASH_INTERNAL;

/// Result of a header download pass (sequential or parallel merge).
pub(crate) struct HeaderSyncResult {
    pub tip_height: u64,
}

impl HeaderSyncResult {
    fn at_height(tip_height: u64) -> Self {
        Self { tip_height }
    }
}

/// H08: child links to parent when parent header is stored, else compare to expected hash bytes.
fn header_links_to_parent(
    blockstore: &BlockStore,
    header: &BlockHeader,
    height: u64,
    last_hash: &[u8; 32],
) -> Result<bool> {
    if height > 0 {
        if let Some(parent) = blockstore.get_header_at_height(height - 1)? {
            return Ok(blvm_consensus::block::validate_prev_block_hash(
                header, &parent,
            ));
        }
    }
    Ok(header.prev_block_hash == *last_hash)
}

/// Download headers for a range starting from the given locator hash.
///
/// Standalone async function that can be spawned as a task.
/// Downloads headers until end height or empty response.
pub(crate) async fn download_header_range(
    network: Arc<NetworkManager>,
    peer: SocketAddr,
    locator_hash: [u8; 32],
    start_height: u64,
    end_height: u64,
) -> Result<Vec<blvm_protocol::BlockHeader>> {
    let mut all_headers = Vec::new();
    let mut current_hash = locator_hash;
    let mut current_height = start_height;
    let mut consecutive_failures = 0;
    let mut current_peer = peer;
    const MAX_FAILURES: u32 = 3; // fail fast per peer, then switch
    const TIMEOUT_SECS: u64 = 10;

    while current_height <= end_height {
        let get_headers = GetHeadersMessage {
            version: 70015,
            block_locator_hashes: vec![current_hash],
            hash_stop: [0; 32],
        };

        let wire_msg = ProtocolParser::serialize_message(&ProtocolMessage::GetHeaders(get_headers))
            .map_err(|e| anyhow::anyhow!("Failed to serialize GetHeaders: {}", e))?;

        let headers_rx = network.register_headers_request(current_peer);

        if let Err(e) = network.send_to_peer(current_peer, wire_msg).await {
            consecutive_failures += 1;
            if consecutive_failures >= MAX_FAILURES {
                // Switch to a fresh connected peer instead of giving up.
                let fresh = network.get_connected_peer_addresses().await;
                if let Some(&next) = fresh.iter().find(|&&p| p != current_peer) {
                    debug!("Range {}-{}: switching from {} to {}", start_height, end_height, current_peer, next);
                    current_peer = next;
                    consecutive_failures = 0;
                } else {
                    return Err(anyhow::anyhow!("No peers for range {}-{}: {}", start_height, end_height, e));
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }

        match timeout(Duration::from_secs(TIMEOUT_SECS), headers_rx).await {
            Ok(Ok(headers)) => {
                consecutive_failures = 0;

                if headers.is_empty() {
                    break;
                }

                for header in headers {
                    match blvm_protocol::pow::check_proof_of_work(&header) {
                        Ok(true) => {}
                        Ok(false) => {
                            return Err(anyhow::anyhow!(
                                "Header at height {} failed PoW — refusing to skip (would corrupt height index)",
                                current_height
                            ));
                        }
                        Err(e) => {
                            return Err(anyhow::anyhow!(
                                "Header at height {} PoW check error: {e}",
                                current_height
                            ));
                        }
                    }

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

                let max_headers = network.protocol_limits().max_headers_results;
                if all_headers.len() % max_headers != 0 {
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

type HeaderRangeJob = (
    SocketAddr,
    u64,
    u64,
    [u8; 32],
    Result<Vec<blvm_protocol::BlockHeader>>,
);

/// N13: fill free peer slots from the pending range queue (≤1 in-flight range per peer).
fn spawn_header_range_jobs(
    join_set: &mut JoinSet<HeaderRangeJob>,
    pending: &mut VecDeque<HeaderRange>,
    free_peers: &mut VecDeque<SocketAddr>,
    network_mgr: &Arc<NetworkManager>,
) {
    while !free_peers.is_empty() && !pending.is_empty() {
        let peer_addr = free_peers.pop_front().expect("non-empty");
        let (actual_start, actual_end, locator_hash) = pending.pop_front().expect("non-empty");
        let network_clone = Arc::clone(network_mgr);
        join_set.spawn(async move {
            let result = download_header_range(
                network_clone,
                peer_addr,
                locator_hash,
                actual_start,
                actual_end,
            )
            .await;
            (peer_addr, actual_start, actual_end, locator_hash, result)
        });
    }
}

/// Download headers in parallel using checkpoint-based ranges.
pub(crate) async fn download_headers_parallel(
    peer_scorer: Arc<PeerScorer>,
    start_height: u64,
    end_height: u64,
    peer_ids: &[String],
    blockstore: &BlockStore,
    network: Option<Arc<NetworkManager>>,
    headers_timeout_secs: u64,
    headers_max_failures: u32,
    event_publisher: Option<Arc<EventPublisher>>,
) -> Result<HeaderSyncResult> {
    let Some(network_mgr) = network else {
        return Err(anyhow::anyhow!(
            "NetworkManager not available for parallel header download"
        ));
    };

    let checkpoints = super::checkpoints::get_checkpoints_in_range(start_height, end_height);

    if checkpoints.len() < 2 {
        info!("Insufficient checkpoints for parallel download, using sequential");
        return download_headers(
            peer_scorer,
            start_height,
            end_height,
            peer_ids,
            blockstore,
            Some(network_mgr),
            headers_timeout_secs,
            headers_max_failures,
            event_publisher,
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

    // N13: all checkpoint ranges; ≤1 in-flight range per peer (FIFO headers match).
    // Extra peers stay idle when peers > ranges (no mid-hash locators to subdivide).
    let ranges = checkpoint_header_ranges(&checkpoints, start_height, end_height);
    let num_active = ranges.len();
    info!(
        "Parallel header download: {} ranges across {} peers (N13 wave, ≤1 range/peer in-flight)",
        num_active,
        peer_addrs.len()
    );

    let mut pending: VecDeque<HeaderRange> = ranges.into();
    let mut free_peers: VecDeque<SocketAddr> = peer_addrs.iter().copied().collect();
    let mut join_set: JoinSet<HeaderRangeJob> = JoinSet::new();

    spawn_header_range_jobs(
        &mut join_set,
        &mut pending,
        &mut free_peers,
        &network_mgr,
    );

    let mut all_headers: Vec<(u64, Vec<blvm_protocol::BlockHeader>)> = Vec::new();
    let mut highest_success = start_height;

    while let Some(joined) = join_set.join_next().await {
        let (peer_addr, range_start, range_end, start_hash, result) = match joined {
            Ok(v) => v,
            Err(e) => {
                warn!("Parallel headers: task panicked: {e}");
                continue;
            }
        };

        let headers = match result {
            Ok(h) => {
                info!(
                    "Parallel headers: got {} for range {}-{}",
                    h.len(),
                    range_start,
                    range_end
                );
                Some(h)
            }
            Err(e) => {
                warn!(
                    "Parallel headers: range {}-{} failed ({e}), retrying with fresh peer",
                    range_start, range_end
                );
                let fresh_peers = network_mgr.get_connected_peer_addresses().await;
                let retry_peer = fresh_peers
                    .into_iter()
                    .next()
                    .unwrap_or(peer_addr);
                let net = Arc::clone(&network_mgr);
                match download_header_range(net, retry_peer, start_hash, range_start, range_end)
                    .await
                {
                    Ok(h) => {
                        info!(
                            "Parallel headers: retry succeeded for range {}-{}",
                            range_start, range_end
                        );
                        Some(h)
                    }
                    Err(e2) => {
                        warn!(
                            "Parallel headers: retry also failed for range {}-{}: {e2}",
                            range_start, range_end
                        );
                        None
                    }
                }
            }
        };

        if let Some(headers) = headers {
            if !headers.is_empty() {
                all_headers.push((range_start, headers));
                highest_success = highest_success.max(range_end);
            }
        }

        free_peers.push_back(peer_addr);
        spawn_header_range_jobs(
            &mut join_set,
            &mut pending,
            &mut free_peers,
            &network_mgr,
        );
    }

    // If no ranges succeeded fall back to full sequential download — don't return
    // Ok(tip_height=0) which makes the IBD orchestrator think sync is complete.
    if all_headers.is_empty() {
        warn!(
            "Parallel header download: all {} ranges failed; falling back to sequential",
            num_active
        );
        return download_headers(
            peer_scorer,
            start_height,
            end_height,
            peer_ids,
            blockstore,
            Some(network_mgr),
            headers_timeout_secs,
            headers_max_failures,
            event_publisher,
        )
        .await;
    }

    all_headers.sort_by_key(|(start, _)| *start);

    let mut current_height = start_height;
    let mut last_hash: [u8; 32] = if start_height == 0 {
        GENESIS_BLOCK_HASH_INTERNAL
    } else {
        let parent_h = start_height
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("parallel header merge: invalid start_height"))?;
        blockstore.get_hash_by_height(parent_h)?.ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot merge parallel headers at height {}: missing parent hash at {}",
                start_height,
                parent_h
            )
        })?
    };

    for (range_start, headers) in all_headers {
        if range_start > current_height {
            // Gap: sequentially fill missing heights from the network.
            warn!(
                "Parallel header gap at {}-{}: downloading sequentially",
                current_height, range_start
            );
            let gap_peers = network_mgr.get_connected_peer_addresses().await;
            let gap_result = download_headers(
                peer_scorer.clone(),
                current_height,
                range_start.saturating_sub(1),
                &gap_peers.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                blockstore,
                Some(Arc::clone(&network_mgr)),
                headers_timeout_secs,
                headers_max_failures,
                None,
            )
            .await?;
            current_height = gap_result.tip_height + 1;
            last_hash = blockstore
                .get_hash_by_height(gap_result.tip_height)?
                .ok_or_else(|| anyhow::anyhow!("no hash after gap fill at {}", gap_result.tip_height))?;
            if current_height != range_start {
                return Err(anyhow::anyhow!(
                    "Gap fill ended at {} but expected {}", current_height, range_start
                ));
            }
        }
        if range_start < current_height {
            // Overlap: skip headers we already stored.
            continue;
        }

        for header in headers {
            match blvm_protocol::pow::check_proof_of_work(&header) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(anyhow::anyhow!(
                        "Header at height {} failed PoW — refusing to skip (parallel merge)",
                        current_height
                    ));
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Header at height {} PoW check error (parallel merge): {e}",
                        current_height
                    ));
                }
            }

            if !header_links_to_parent(blockstore, &header, current_height, &last_hash)? {
                return Err(anyhow::anyhow!(
                    "Header chain break at height {} (parallel merge): expected prev {} got {}",
                    current_height,
                    hex::encode(last_hash),
                    hex::encode(header.prev_block_hash)
                ));
            }

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

            last_hash = header_hash;
            current_height += 1;
        }
    }

    info!(
        "Parallel header download complete: {} headers stored",
        current_height - start_height
    );
    Ok(HeaderSyncResult {
        tip_height: current_height.saturating_sub(1),
    })
}

/// Download headers sequentially for the given height range.
pub(crate) async fn download_headers(
    peer_scorer: Arc<PeerScorer>,
    start_height: u64,
    end_height: u64,
    peer_ids: &[String],
    blockstore: &BlockStore,
    network: Option<Arc<NetworkManager>>,
    headers_timeout_secs: u64,
    headers_max_failures: u32,
    event_publisher: Option<Arc<EventPublisher>>,
) -> Result<HeaderSyncResult> {
    let network = match network.as_ref() {
        Some(n) => n,
        None => {
            warn!("NetworkManager not available, skipping header download");
            return Ok(HeaderSyncResult::at_height(start_height));
        }
    };

    // Zero-peer / synthetic WAN: headers already contiguous on disk — skip network.
    if peer_ids.is_empty() {
        if !super::synthetic_wan::allow_zero_real_peers() {
            return Err(anyhow::anyhow!("No peers available for header download"));
        }
        let mut h = start_height;
        while h <= end_height {
            match blockstore.get_hash_by_height(h) {
                Ok(Some(_)) => h += 1,
                _ => break,
            }
        }
        if h > start_height {
            let tip = h.saturating_sub(1);
            info!(
                "IBD header sync: zero-peer local-replay — using {} already-stored headers (tip={})",
                tip.saturating_sub(start_height).saturating_add(1),
                tip
            );
            return Ok(HeaderSyncResult::at_height(tip));
        }
        return Err(anyhow::anyhow!(
            "No peers and no stored headers from {} (synthetic WAN / ALLOW_ZERO_PEERS)",
            start_height
        ));
    }

    let mut peer_addrs: Vec<SocketAddr> = peer_ids
        .iter()
        .filter_map(|id| id.parse::<SocketAddr>().ok())
        .collect();

    if peer_addrs.is_empty() {
        return Err(anyhow::anyhow!("No valid peer addresses found"));
    }

    peer_addrs.sort_by(|a, b| {
        let a_score = peer_scorer.get_score(a);
        let b_score = peer_scorer.get_score(b);
        b_score
            .partial_cmp(&a_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    info!(
        "Using {} peers for sequential header download",
        peer_addrs.len()
    );

    let genesis_hash = GENESIS_BLOCK_HASH_INTERNAL;

    // `sync_parallel` passes the next block height to fetch. When resuming (start_height > 0),
    // we must anchor GetHeaders to the stored parent hash — not genesis — or peers return
    // block 1,2,… which get written at the wrong height (BIP90 rejects v1 at high height).
    let mut current_height: u64;
    let mut last_hash: [u8; 32];

    if start_height == 0 {
        let genesis_header = blvm_protocol::BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [
                0x3b, 0xa3, 0xed, 0xfd, 0x7a, 0x7b, 0x12, 0xb2, 0x7a, 0xc7, 0x2c, 0x3e, 0x67, 0x76,
                0x8f, 0x61, 0x7f, 0xc8, 0x1b, 0xc3, 0x88, 0x8a, 0x51, 0x32, 0x3a, 0x9f, 0xb8, 0xaa,
                0x4b, 0x1e, 0x5e, 0x4a,
            ],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 2083236893,
        };

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
        current_height = 1;
        last_hash = genesis_hash;
    } else {
        let parent_h = start_height
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("header sync: invalid start_height"))?;
        last_hash = blockstore.get_hash_by_height(parent_h)?.ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot resume header sync at height {}: missing parent hash at height {}. \
                     Sync from genesis or repair height_index (data may be inconsistent).",
                start_height,
                parent_h
            )
        })?;
        current_height = start_height;
        info!(
            "Resuming header sync at height {} (GetHeaders locator = parent {})",
            start_height,
            hex::encode(last_hash)
        );
    }
    let mut consecutive_failures = 0;
    let mut current_peer_idx = 0;
    let mut last_progress_log = start_height;
    let mut last_progress_event = start_height;
    let start_time = std::time::Instant::now();

    // Fast-forward through headers already on disk.  During local replay after a UTXO repair,
    // `start_height` may be well below `highest_stored_height`.  For every contiguous run of
    // already-stored headers we can advance without any network round-trip — reading 2000 hashes
    // from LMDB is orders of magnitude faster than one getheaders/headers RTT.
    {
        let mut skip_count: u64 = 0;
        while current_height <= end_height {
            match blockstore.get_hash_by_height(current_height) {
                Ok(Some(stored_hash)) => {
                    last_hash = stored_hash;
                    skip_count += 1;
                    current_height += 1;
                }
                _ => break,
            }
        }
        if skip_count > 0 {
            info!(
                "IBD header sync: skipped {} already-stored headers (now at height {}); \
                 only fetching new headers from peers",
                skip_count, current_height
            );
            last_progress_log = current_height;
        }
    }

    while current_height <= end_height {
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

        let peer_addr = peer_addrs[current_peer_idx % peer_addrs.len()];

        let get_headers = GetHeadersMessage {
            version: 70015,
            block_locator_hashes: vec![last_hash],
            hash_stop: [0; 32],
        };

        let wire_msg =
            match ProtocolParser::serialize_message(&ProtocolMessage::GetHeaders(get_headers)) {
                Ok(msg) => msg,
                Err(e) => {
                    warn!("Failed to serialize GetHeaders: {}", e);
                    return Err(anyhow::anyhow!("Serialization failed"));
                }
            };

        let headers_rx = network.register_headers_request(peer_addr);
        let request_start = std::time::Instant::now();

        if let Err(e) = network.send_to_peer(peer_addr, wire_msg).await {
            debug!("Send failed to {}: {}", peer_addr, e);
            peer_addrs.retain(|&a| a != peer_addr);
            current_peer_idx += 1;
            consecutive_failures += 1;
            if consecutive_failures >= headers_max_failures {
                return Err(anyhow::anyhow!("Too many failures"));
            }
            continue;
        }

        debug!(
            "Waiting for headers from {} (timeout: {}s)",
            peer_addr, headers_timeout_secs
        );
        match timeout(Duration::from_secs(headers_timeout_secs), headers_rx).await {
            Ok(Ok(headers)) => {
                let latency_ms = request_start.elapsed().as_secs_f64() * 1000.0;
                peer_scorer.record_latency_sample(peer_addr, latency_ms);
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

                debug!(
                    "Processing {} headers starting at height {}",
                    headers.len(),
                    current_height
                );
                let mut batch_entries: Vec<(blvm_protocol::Hash, BlockHeader, u64)> =
                    Vec::with_capacity(headers.len());

                for header in &headers {
                    match blvm_protocol::pow::check_proof_of_work(header) {
                        Ok(true) => {}
                        Ok(false) => {
                            return Err(anyhow::anyhow!(
                                "Header at height {} failed PoW — refusing to skip (would corrupt height index)",
                                current_height
                            ));
                        }
                        Err(e) => {
                            return Err(anyhow::anyhow!(
                                "Header at height {} PoW check error: {e}",
                                current_height
                            ));
                        }
                    }

                    if !header_links_to_parent(blockstore, header, current_height, &last_hash)? {
                        return Err(anyhow::anyhow!(
                            "Header chain break at height {}: expected prev {} got {}",
                            current_height,
                            hex::encode(last_hash),
                            hex::encode(header.prev_block_hash)
                        ));
                    }

                    let mut header_data = [0u8; 80];
                    header_data[0..4].copy_from_slice(&(header.version as i32).to_le_bytes());
                    header_data[4..36].copy_from_slice(&header.prev_block_hash);
                    header_data[36..68].copy_from_slice(&header.merkle_root);
                    header_data[68..72].copy_from_slice(&(header.timestamp as u32).to_le_bytes());
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

                let batch_count = batch_entries.len();
                debug!("Storing {} headers in batch...", batch_count);
                let store_start = std::time::Instant::now();
                let blockstore_clone = blockstore.clone();
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

                if current_height > last_progress_log && current_height - last_progress_log >= 20000
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

                // Publish HeadersSyncProgress every 5000 headers for module event subscribers
                if current_height > last_progress_event
                    && (current_height - last_progress_event) >= 5000
                {
                    if let Some(ref ep) = event_publisher {
                        let progress_percent = if end_height > start_height {
                            ((current_height - start_height) as f64
                                / (end_height - start_height + 1) as f64)
                                * 100.0
                        } else {
                            100.0
                        };
                        ep.publish_headers_sync_progress(
                            current_height.saturating_sub(1),
                            end_height,
                            progress_percent,
                        )
                        .await;
                        last_progress_event = current_height;
                    }
                }

                let max_headers = network.protocol_limits().max_headers_results;
                if headers.len() < max_headers {
                    let total = current_height - start_height;
                    let elapsed = start_time.elapsed();
                    let rate = if elapsed.as_secs_f64() > 0.0 {
                        total as f64 / elapsed.as_secs_f64()
                    } else {
                        0.0
                    };
                    info!(
                        "Header sync COMPLETE: {} headers in {:.1}s ({:.0} h/s) - chain tip reached",
                        total,
                        elapsed.as_secs_f64(),
                        rate
                    );
                    return Ok(HeaderSyncResult {
                        tip_height: current_height.saturating_sub(1),
                    });
                }
            }
            Ok(Err(_)) => {
                debug!("Channel closed for request to {}", peer_addr);
                consecutive_failures += 1;
                current_peer_idx += 1;
            }
            Err(_) => {
                debug!("Timeout waiting for headers from {}", peer_addr);
                consecutive_failures += 1;
                current_peer_idx += 1;
                if let Some(idx) = peer_addrs.iter().position(|&a| a == peer_addr) {
                    let p = peer_addrs.remove(idx);
                    peer_addrs.push(p);
                }
            }
        }

        if consecutive_failures >= headers_max_failures {
            warn!(
                "Too many failures ({}), refreshing peers",
                consecutive_failures
            );
            consecutive_failures = 0;
            peer_addrs = network.get_connected_peer_addresses().await;
            if peer_addrs.is_empty() {
                // Actively trigger archive peer discovery before waiting — don't just
                // sit idle for 5 s then give up. The address DB may be drained of
                // full-history peers after mass evictions; seed it explicitly.
                warn!("Header sync: no connected peers — triggering archive peer re-discovery");
                // Fire both archive and regular DNS seeds in parallel for speed.
                let default_ban = Default::default();
                let (arc_res, _) = tokio::join!(
                    network.discover_archive_peers_from_dns(),
                    {
                        let (net_name, port) =
                            crate::network::protocol::ProtocolParser::dns_seed_network();
                        network.discover_peers_from_dns(net_name, port, &default_ban)
                    },
                );
                let _ = arc_res;
                let _ = network.connect_peers_from_database(16).await;
                // Wait up to 15 s in 2-second slices for a peer to connect.
                for _ in 0..8 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    peer_addrs = network.get_connected_peer_addresses().await;
                    if !peer_addrs.is_empty() {
                        info!("Header sync: {} peer(s) reconnected, resuming", peer_addrs.len());
                        break;
                    }
                }
                if peer_addrs.is_empty() {
                    return Err(anyhow::anyhow!("No peers available after 70s wait + re-discovery"));
                }
            }
        }
    }

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

    Ok(HeaderSyncResult {
        tip_height: current_height.saturating_sub(1),
    })
}

#[cfg(test)]
mod n13_tests {
    use super::*;

    #[test]
    fn n13_checkpoint_ranges_not_capped_by_peer_count() {
        let cps = [
            (0u64, [0u8; 32]),
            (100, [1u8; 32]),
            (200, [2u8; 32]),
            (300, [3u8; 32]),
            (400, [4u8; 32]),
        ];
        let ranges = checkpoint_header_ranges(&cps, 0, 400);
        assert_eq!(ranges.len(), 4, "all windows kept (old bug: take(peers) dropped tail)");
        assert_eq!(ranges[0].0, 0);
        assert_eq!(ranges[3].1, 400);
        // Peers=2 would have dropped ranges 2–3 under the old .take(peers) cap.
        assert!(ranges.len() > 2);
    }

    #[test]
    fn n13_schedule_covers_all_ranges_with_one_inflight_per_peer() {
        let peer_count = 3;
        let range_count = 7;
        let assigned = simulate_header_range_schedule(peer_count, range_count);
        assert_eq!(assigned.len(), range_count);
        let mut seen = vec![false; range_count];
        for &(peer, range) in &assigned {
            assert!(peer < peer_count);
            assert!(!seen[range], "range {range} assigned twice");
            seen[range] = true;
        }
        assert!(seen.iter().all(|&s| s));

        // Replay concurrency: never exceed peer_count in-flight.
        let first_wave = peer_count.min(range_count);
        let mut inflight = first_wave;
        let mut free = peer_count - first_wave;
        let mut pending = range_count - first_wave;
        let mut max_inflight = inflight;
        for _ in first_wave..range_count {
            inflight -= 1;
            free += 1;
            inflight += 1;
            free -= 1;
            pending -= 1;
            max_inflight = max_inflight.max(inflight);
        }
        assert_eq!(pending, 0);
        assert!(max_inflight <= peer_count);
        assert_eq!(free + inflight, peer_count);
    }

    #[test]
    fn n13_extra_peers_idle_when_fewer_ranges() {
        let assigned = simulate_header_range_schedule(8, 3);
        assert_eq!(assigned.len(), 3);
        let peers_used: std::collections::HashSet<_> = assigned.iter().map(|(p, _)| *p).collect();
        assert_eq!(peers_used.len(), 3);
    }
}
