//! Header download for parallel IBD.
//!
//! Downloads block headers from peers for a given height range.
//! Used by both sequential and checkpoint-parallel header sync.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use blvm_protocol::BlockHeader;
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

use crate::network::peer_scoring::PeerScorer;
use crate::network::protocol::{GetHeadersMessage, ProtocolMessage, ProtocolParser};
use crate::network::NetworkManager;
use crate::node::event_publisher::EventPublisher;
use crate::storage::blockstore::BlockStore;
use crate::storage::hashing::double_sha256;

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
    const MAX_FAILURES: u32 = 10;
    const TIMEOUT_SECS: u64 = 30;

    while current_height <= end_height {
        let get_headers = GetHeadersMessage {
            version: 70015,
            block_locator_hashes: vec![current_hash],
            hash_stop: [0; 32],
        };

        let wire_msg = ProtocolParser::serialize_message(&ProtocolMessage::GetHeaders(get_headers))
            .map_err(|e| anyhow::anyhow!("Failed to serialize GetHeaders: {}", e))?;

        let headers_rx = network.register_headers_request(peer);

        if let Err(e) = network.send_to_peer(peer, wire_msg).await {
            consecutive_failures += 1;
            if consecutive_failures >= MAX_FAILURES {
                return Err(anyhow::anyhow!("Too many send failures to {}: {}", peer, e));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }

        match timeout(Duration::from_secs(TIMEOUT_SECS), headers_rx).await {
            Ok(Ok(headers)) => {
                consecutive_failures = 0;

                if headers.is_empty() {
                    break;
                }

                for header in headers {
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

/// Download headers in parallel using checkpoint-based ranges.
pub(crate) async fn download_headers_parallel(
    peer_scorer: Arc<PeerScorer>,
    start_height: u64,
    end_height: u64,
    peer_ids: &[String],
    blockstore: &BlockStore,
    network: Arc<NetworkManager>,
) -> Result<u64> {
    let checkpoints = super::checkpoints::get_checkpoints_in_range(start_height, end_height);

    if checkpoints.len() < 2 {
        info!("Insufficient checkpoints for parallel download, using sequential");
        return download_headers(
            peer_scorer,
            start_height,
            end_height,
            peer_ids,
            blockstore,
            Some(network),
            30,  // headers_timeout_secs (default when parallel path delegates)
            10,  // headers_max_failures
            None, // event_publisher (parallel path fallback has no publisher)
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

    let mut tasks = Vec::new();
    let num_peers = peer_addrs.len();

    for (i, window) in checkpoints.windows(2).enumerate() {
        let (range_start, start_hash) = window[0];
        let (range_end, _end_hash) = window[1];

        if range_end < start_height {
            continue;
        }

        let actual_start = range_start.max(start_height);
        let actual_end = range_end.min(end_height);

        if actual_start > actual_end {
            continue;
        }

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

    all_headers.sort_by_key(|(start, _)| *start);

    let mut current_height = start_height;
    for (range_start, headers) in all_headers {
        if range_start > current_height {
            warn!(
                "Gap detected at height {}, expected {}",
                range_start, current_height
            );
        }

        for header in headers {
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
) -> Result<u64> {
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

    let genesis_hash: [u8; 32] = [
        0x6f, 0xe2, 0x8c, 0x0a, 0xb6, 0xf1, 0xb3, 0x72, 0xc1, 0xa6, 0xa2, 0x46, 0xae, 0x63,
        0xf7, 0x4f, 0x93, 0x1e, 0x83, 0x65, 0xe1, 0x5a, 0x08, 0x9c, 0x68, 0xd6, 0x19, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    let mut current_height = start_height;
    let mut last_hash = genesis_hash;

    if start_height == 0 {
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
    }
    let mut consecutive_failures = 0;
    let mut current_peer_idx = 0;
    let mut last_progress_log = start_height;
    let mut last_progress_event = start_height;
    let start_time = std::time::Instant::now();

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

        let wire_msg = match ProtocolParser::serialize_message(&ProtocolMessage::GetHeaders(
            get_headers,
        )) {
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
                        total, elapsed.as_secs_f64(), rate
                    );
                    return Ok(current_height.saturating_sub(1));
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
