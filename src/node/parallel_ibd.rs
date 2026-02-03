//! Parallel Initial Block Download (IBD)
//!
//! Implements parallel block downloading from multiple peers during initial sync.
//! This significantly speeds up IBD by downloading blocks concurrently from different peers.

use crate::node::block_processor::{
    prepare_block_validation_context, store_block_with_context_and_index,
    validate_block_with_context,
};
use crate::network::NetworkManager;
use crate::network::protocol::{GetHeadersMessage, HeadersMessage, ProtocolMessage, ProtocolParser};
use crate::network::inventory::MSG_BLOCK;
use crate::storage::blockstore::BlockStore;
use crate::storage::Storage;
use anyhow::{Context, Result};
use blvm_protocol::{
    BitcoinProtocolEngine, Block, BlockHeader, Hash, UtxoSet, ValidationResult,
    segwit::Witness,
};
use blvm_consensus::serialization::varint::decode_varint;
use hex;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, warn};

/// Parallel IBD configuration
#[derive(Debug, Clone)]
pub struct ParallelIBDConfig {
    /// Number of parallel workers (default: CPU count)
    pub num_workers: usize,
    /// Chunk size in blocks (default: 1000)
    pub chunk_size: u64,
    /// Maximum concurrent downloads per peer (default: 3)
    pub max_concurrent_per_peer: usize,
    /// Checkpoint interval in blocks (default: 10,000)
    pub checkpoint_interval: u64,
    /// Timeout for block download in seconds (default: 30)
    pub download_timeout_secs: u64,
}

impl Default for ParallelIBDConfig {
    fn default() -> Self {
        Self {
            num_workers: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            chunk_size: 1000,
            max_concurrent_per_peer: 3,
            checkpoint_interval: 10_000,
            download_timeout_secs: 30,
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
            current_timeout_seconds: 2, // Start with 2 seconds (BLOCK_STALLING_TIMEOUT_DEFAULT)
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
        // Increase timeout: 2s * 2^disconnected_peers_count, max 64s
        self.current_timeout_seconds = (2u64.saturating_pow(self.disconnected_peers_count as u32) * 2)
            .min(64); // BLOCK_STALLING_TIMEOUT_MAX
    }
    
    /// Reset timeout to default
    fn reset_timeout(&mut self) {
        self.current_timeout_seconds = 2;
        self.disconnected_peers_count = 0;
    }
}

/// Parallel IBD coordinator
pub struct ParallelIBD {
    config: ParallelIBDConfig,
    /// Semaphore to limit concurrent downloads per peer
    peer_semaphores: Arc<HashMap<String, Arc<Semaphore>>>,
}

impl ParallelIBD {
    /// Create a new parallel IBD coordinator
    pub fn new(config: ParallelIBDConfig) -> Self {
        Self {
            config,
            peer_semaphores: Arc::new(HashMap::new()),
        }
    }

    /// Initialize peer semaphores
    pub fn initialize_peers(&mut self, peer_ids: &[String]) {
        let mut semaphores = HashMap::new();
        for peer_id in peer_ids {
            semaphores.insert(
                peer_id.clone(),
                Arc::new(Semaphore::new(self.config.max_concurrent_per_peer)),
            );
        }
        self.peer_semaphores = Arc::new(semaphores);
    }

    /// Download blocks in parallel from multiple peers
    ///
    /// Algorithm:
    /// 1. Download headers first (sequential, fast)
    /// 2. Split block range into chunks
    /// 3. Assign chunks to peers (round-robin)
    /// 4. Download chunks in parallel
    /// 5. Validate and store blocks sequentially (maintain order)
    pub async fn sync_parallel(
        &self,
        start_height: u64,
        target_height: u64,
        peer_ids: &[String],
        blockstore: Arc<BlockStore>,
        storage: Option<&Arc<Storage>>,
        protocol: &BitcoinProtocolEngine,
        utxo_set: &mut UtxoSet,
        network: Option<Arc<NetworkManager>>,
    ) -> Result<()> {
        if peer_ids.is_empty() {
            return Err(anyhow::anyhow!("No peers available for parallel IBD"));
        }

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
        let actual_synced_height = self.download_headers(start_height, target_height, peer_ids, &blockstore, network_for_headers)
            .await
            .context("Failed to download headers")?;

        // Use the actual synced height (may be less than target_height if we reached chain tip)
        let effective_end_height = actual_synced_height.min(target_height);
        info!("Headers synced up to height {}, will download blocks for heights {} to {}", 
            actual_synced_height, start_height, effective_end_height);

        // Step 2: Split into chunks and assign to peers
        // Only create chunks for headers we actually have
        let chunks = self.create_chunks(start_height, effective_end_height, peer_ids);
        info!("Created {} chunks for parallel download", chunks.len());

        // Step 3: Download chunks in parallel
        let mut downloaded_blocks: Vec<(u64, Block, Vec<Vec<Witness>>)> = Vec::new();
        let mut download_tasks = Vec::new();

        for chunk in chunks {
            let peer_id = chunk.peer_id.clone();
            let config = self.config.clone();
            let start = chunk.start_height;
            let end = chunk.end_height;
            let blockstore_clone = Arc::clone(&blockstore);
            let network_clone = network.clone();
            let semaphore = self
                .peer_semaphores
                .get(&peer_id)
                .ok_or_else(|| anyhow::anyhow!("Peer {} not found", peer_id))?
                .clone();

            // Spawn download task for this chunk
            let task = tokio::spawn(async move {
                let _permit = semaphore.acquire().await.unwrap();
                Self::download_chunk(start, end, &peer_id, network_clone, &blockstore_clone, &config).await
            });

            download_tasks.push((start, task));
        }

        // Wait for all downloads to complete
        for (chunk_start, task) in download_tasks {
            match task.await {
                Ok(Ok(blocks)) => {
                    for (height, block, witnesses) in blocks {
                        downloaded_blocks.push((height, block, witnesses));
                    }
                    info!(
                        "Downloaded chunk starting at height {} ({} blocks)",
                        chunk_start,
                        downloaded_blocks.len()
                    );
                }
                Ok(Err(e)) => {
                    warn!("Failed to download chunk starting at height {}: {}", chunk_start, e);
                    // Continue with other chunks
                }
                Err(e) => {
                    warn!("Chunk download task panicked at height {}: {}", chunk_start, e);
                }
            }
        }

        // Step 4: Sort blocks by height (maintain order)
        downloaded_blocks.sort_by_key(|(height, _, _)| *height);

        // Step 5: Validate and store blocks sequentially
        info!("Validating and storing {} blocks...", downloaded_blocks.len());
        let mut blocks_synced = 0;
        for (height, block, witnesses) in downloaded_blocks {
            if let Err(e) = self.validate_and_store_block(
                &blockstore,
                storage,
                protocol,
                utxo_set,
                &block,
                &witnesses,
                height,
            ) {
                error!("Failed to validate/store block at height {}: {}", height, e);
                return Err(e);
            }
            blocks_synced += 1;

            if height % 1000 == 0 {
                info!("Processed {} blocks (height: {})", height - start_height + 1, height);
            }
        }

        info!("Parallel IBD completed: {} blocks synced (heights {} to {})", 
            blocks_synced, start_height, effective_end_height);
        Ok(())
    }

    /// Download headers for the given height range
    /// OPTIMIZED: True parallel pipelining across multiple peers
    async fn download_headers(
        &self,
        start_height: u64,
        end_height: u64,
        peer_ids: &[String],
        blockstore: &BlockStore,
        network: Option<Arc<NetworkManager>>,
    ) -> Result<u64> {
        use crate::storage::hashing::double_sha256;
        use std::collections::VecDeque;
        
        info!(
            "Downloading headers {} to {} ({} headers) - PARALLEL PIPELINED",
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

        let mut peer_addrs: Vec<SocketAddr> = peer_ids.iter()
            .filter_map(|id| id.parse::<SocketAddr>().ok())
            .collect();
        
        if peer_addrs.is_empty() {
            return Err(anyhow::anyhow!("No valid peer addresses found"));
        }

        use crate::network::protocol::MAX_HEADERS_RESULTS;
        
        // Pipelining configuration
        const PIPELINE_DEPTH: usize = 4;  // Requests in flight per peer
        const MAX_PARALLEL_PEERS: usize = 8;
        const TIMEOUT_SECS: u64 = 30;
        
        let num_peers = peer_addrs.len().min(MAX_PARALLEL_PEERS);
        let total_pipeline = PIPELINE_DEPTH * num_peers;
        
        info!("Using {} peers with pipeline depth {} = {} parallel requests", 
            num_peers, PIPELINE_DEPTH, total_pipeline);

        // Genesis hash
        let genesis_hash: [u8; 32] = [
            0x6f, 0xe2, 0x8c, 0x0a, 0xb6, 0xf1, 0xb3, 0x72,
            0xc1, 0xa6, 0xa2, 0x46, 0xae, 0x63, 0xf7, 0x4f,
            0x93, 0x1e, 0x83, 0x65, 0xe1, 0x5a, 0x08, 0x9c,
            0x68, 0xd6, 0x19, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let mut next_request_height = start_height;
        let mut highest_stored = start_height.saturating_sub(1);
        let mut last_stored_hash = genesis_hash;
        let mut consecutive_failures = 0;
        let mut last_progress_log = start_height;
        let start_time = std::time::Instant::now();
        const MAX_FAILURES: u32 = 50;
        
        // Pending requests: (expected_start_height, peer_addr, receiver)
        let mut pending: VecDeque<(u64, SocketAddr, tokio::sync::oneshot::Receiver<Vec<blvm_protocol::BlockHeader>>)> = VecDeque::new();
        
        // Buffer for out-of-order headers (height -> (hash, header))
        let mut header_buffer: HashMap<u64, ([u8; 32], blvm_protocol::BlockHeader)> = HashMap::new();
        
        loop {
            // Fill the pipeline - send requests until we have enough in flight
            while pending.len() < total_pipeline && next_request_height < end_height {
                // Round-robin peer selection
                let peer_idx = (pending.len() / PIPELINE_DEPTH) % num_peers;
                let peer_addr = peer_addrs[peer_idx];
                
                // Build locator - use the hash we expect to be stored when this response arrives
                let locator_hashes = if next_request_height == 0 {
                    vec![genesis_hash]
                } else {
                    // Use last stored hash or try to get from blockstore
                    let locator_height = next_request_height.saturating_sub(1);
                    if let Ok(Some(hash)) = blockstore.get_hash_by_height(locator_height) {
                        vec![hash]
                    } else {
                        // Use the hash we're tracking
                        vec![last_stored_hash]
                    }
                };

                let get_headers = GetHeadersMessage {
                    version: 70015,
                    block_locator_hashes: locator_hashes,
                    hash_stop: [0; 32],
                };

                let wire_msg = match ProtocolParser::serialize_message(&ProtocolMessage::GetHeaders(get_headers)) {
                    Ok(msg) => msg,
                    Err(e) => {
                        warn!("Failed to serialize: {}", e);
                        break;
                    }
                };

                // Register BEFORE sending (queued per peer)
                let headers_rx = network.register_headers_request(peer_addr);
                
                if let Err(e) = network.send_to_peer(peer_addr, wire_msg).await {
                    debug!("Send failed to {}: {}", peer_addr, e);
                    // Remove failed peer
                    peer_addrs.retain(|&a| a != peer_addr);
                    if peer_addrs.is_empty() {
                        peer_addrs = network.get_connected_peer_addresses().await;
                        if peer_addrs.is_empty() {
                            return Err(anyhow::anyhow!("No peers available"));
                        }
                    }
                    continue;
                }
                
                pending.push_back((next_request_height, peer_addr, headers_rx));
                next_request_height += MAX_HEADERS_RESULTS as u64;
            }

            // Done if no pending requests
            if pending.is_empty() {
                break;
            }

            // Process oldest pending request
            let (expected_height, peer_addr, headers_rx) = pending.pop_front().unwrap();
            
            match timeout(Duration::from_secs(TIMEOUT_SECS), headers_rx).await {
                Ok(Ok(headers)) => {
                    consecutive_failures = 0;
                    
                    if headers.is_empty() {
                        info!("Empty response from {} - chain tip reached", peer_addr);
                        pending.clear();
                        break;
                    }
                    
                    // Process headers - calculate hashes and buffer them
                    let mut height = expected_height;
                    for header in &headers {
                        let mut header_data = Vec::with_capacity(80);
                        header_data.extend_from_slice(&header.version.to_le_bytes());
                        header_data.extend_from_slice(&header.prev_block_hash);
                        header_data.extend_from_slice(&header.merkle_root);
                        header_data.extend_from_slice(&header.timestamp.to_le_bytes());
                        header_data.extend_from_slice(&header.bits.to_le_bytes());
                        header_data.extend_from_slice(&header.nonce.to_le_bytes());
                        let header_hash = double_sha256(&header_data);
                        
                        header_buffer.insert(height, (header_hash, header.clone()));
                        height += 1;
                        
                        if height > end_height {
                            break;
                        }
                    }
                    
                    // Flush sequential headers from buffer to storage
                    while let Some((hash, header)) = header_buffer.remove(&(highest_stored + 1)) {
                        blockstore.store_header(&hash, &header)
                            .context("Failed to store header")?;
                        blockstore.store_height(highest_stored + 1, &hash)
                            .context("Failed to store height")?;
                        highest_stored += 1;
                        last_stored_hash = hash;
                    }
                    
                    // Progress logging every 20k headers
                    if highest_stored > last_progress_log && highest_stored - last_progress_log >= 20000 {
                        let elapsed = start_time.elapsed().as_secs_f64();
                        let synced = highest_stored - start_height;
                        let rate = if elapsed > 0.0 { synced as f64 / elapsed } else { 0.0 };
                        let remaining = end_height.saturating_sub(highest_stored);
                        let eta = if rate > 0.0 { remaining as f64 / rate } else { f64::INFINITY };
                        
                        info!(
                            "Header sync: {} / {} ({:.1}%) - {:.0} h/s - ETA: {:.0}s - {} in-flight, {} buffered",
                            highest_stored,
                            end_height,
                            (highest_stored as f64 / end_height as f64) * 100.0,
                            rate,
                            eta,
                            pending.len(),
                            header_buffer.len()
                        );
                        last_progress_log = highest_stored;
                    }
                    
                    // Chain tip detection
                    if headers.len() < MAX_HEADERS_RESULTS {
                        info!("Received {} headers (< {}) - chain tip", headers.len(), MAX_HEADERS_RESULTS);
                        // Drain remaining pending requests
                        while let Some((_, _, rx)) = pending.pop_front() {
                            let _ = timeout(Duration::from_millis(100), rx).await;
                        }
                        break;
                    }
                }
                Ok(Err(_)) => {
                    debug!("Channel closed for height {} from {}", expected_height, peer_addr);
                    consecutive_failures += 1;
                    // Re-queue this height
                    if next_request_height > expected_height {
                        next_request_height = expected_height;
                    }
                }
                Err(_) => {
                    debug!("Timeout for height {} from {}", expected_height, peer_addr);
                    consecutive_failures += 1;
                    // Rotate peer to end
                    if let Some(idx) = peer_addrs.iter().position(|&a| a == peer_addr) {
                        let p = peer_addrs.remove(idx);
                        peer_addrs.push(p);
                    }
                    // Re-queue this height  
                    if next_request_height > expected_height {
                        next_request_height = expected_height;
                    }
                }
            }
            
            if consecutive_failures >= MAX_FAILURES {
                warn!("Too many failures ({}), refreshing peers", consecutive_failures);
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
        
        // Flush any remaining buffered headers
        while let Some((hash, header)) = header_buffer.remove(&(highest_stored + 1)) {
            blockstore.store_header(&hash, &header)?;
            blockstore.store_height(highest_stored + 1, &hash)?;
            highest_stored += 1;
        }
        
        let total = highest_stored.saturating_sub(start_height) + 1;
        let elapsed = start_time.elapsed();
        let rate = if elapsed.as_secs_f64() > 0.0 { total as f64 / elapsed.as_secs_f64() } else { 0.0 };
        
        info!(
            "Header sync COMPLETE: {} headers in {:.1}s ({:.0} h/s)",
            total, elapsed.as_secs_f64(), rate
        );
        
        Ok(highest_stored)
    }

    /// Create chunks for parallel download
    fn create_chunks(&self, start_height: u64, end_height: u64, peer_ids: &[String]) -> Vec<BlockChunk> {
        let mut chunks = Vec::new();
        let mut current_height = start_height;
        let mut peer_index = 0;

        while current_height <= end_height {
            let chunk_end = (current_height + self.config.chunk_size - 1).min(end_height);
            let peer_id = peer_ids[peer_index % peer_ids.len()].clone();

            chunks.push(BlockChunk {
                start_height: current_height,
                end_height: chunk_end,
                peer_id,
            });

            current_height = chunk_end + 1;
            peer_index += 1;
        }

        chunks
    }

    /// Download a chunk of blocks from a peer
    async fn download_chunk(
        start_height: u64,
        end_height: u64,
        peer_id: &str,
        network: Option<Arc<NetworkManager>>,
        blockstore: &BlockStore,
        config: &ParallelIBDConfig,
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
                warn!("Block hash not found for height {} - header may not be stored yet", height);
                return Err(anyhow::anyhow!("Block hash not found for height {} - headers must be downloaded first", height));
            }
        }

        if block_hashes.is_empty() {
            return Err(anyhow::anyhow!("No block hashes found for heights {} to {}", start_height, end_height));
        }

        // Download blocks using GetData messages
        use crate::network::protocol::{GetDataMessage, InventoryItem};
        use crate::network::inventory::MSG_BLOCK;
        
        // Calculate timeout based on Bitcoin Core formula:
        // BLOCK_DOWNLOAD_TIMEOUT_BASE = 1 * block_interval (10 minutes)
        // BLOCK_DOWNLOAD_TIMEOUT_PER_PEER = 0.5 * block_interval (5 minutes per peer)
        // Timeout = base + per_peer * parallel_peers
        // For now, we use a simplified version: 10min + 5min * max_concurrent_per_peer
        // Note: This is per-chunk timeout, not per-block. Each block in chunk uses this timeout.
        let block_interval_seconds = 10 * 60; // 10 minutes (Bitcoin block interval)
        let timeout_base = block_interval_seconds; // 10 minutes
        let timeout_per_peer = block_interval_seconds / 2; // 5 minutes per peer
        // Get max_concurrent_per_peer from config
        let parallel_peers = config.max_concurrent_per_peer;
        let timeout_total: u64 = timeout_base + (timeout_per_peer * parallel_peers as u64);
        let timeout_duration = Duration::from_secs(timeout_total);
        
        debug!("Block download timeout: {} seconds (base: {}s, parallel_peers: {}, per-peer: {}s)", 
            timeout_total, timeout_base, parallel_peers, timeout_per_peer);
        
        for (height, block_hash) in block_hashes {
            // Check for stalling before requesting next block
            if progress.is_stalled() {
                warn!(
                    "Block download stalled for chunk {} to {} (no progress for {} seconds)",
                    start_height, end_height, progress.current_timeout_seconds
                );
                // Return error to trigger peer switch in calling code
                return Err(anyhow::anyhow!("Block download stalled: no progress for {} seconds", progress.current_timeout_seconds));
            }
            
            // Register pending Block request with actual block hash
            let block_rx = network.register_block_request(peer_addr, block_hash);
            
            // Create GetData message requesting this specific block
            let inventory = vec![InventoryItem {
                inv_type: MSG_BLOCK,
                hash: block_hash,
            }];
            
            let get_data = GetDataMessage { inventory };
            let wire_msg = ProtocolParser::serialize_message(&ProtocolMessage::GetData(get_data))?;
            
            // Send request
            network.send_to_peer(peer_addr, wire_msg).await
                .context(format!("Failed to send GetData message for block at height {}", height))?;

            info!("Sent GetData request for block at height {} (hash: {})", 
                height, hex::encode(block_hash));
            
            // Wait for Block response with timeout
            match timeout(timeout_duration, block_rx).await {
                Ok(Ok((block, block_witnesses))) => {
                    // Verify block hash matches
                    let received_hash = blockstore.get_block_hash(&block);
                    if received_hash != block_hash {
                        warn!("Received block hash mismatch for height {}: expected {}, got {}", 
                            height, hex::encode(block_hash), hex::encode(received_hash));
                        continue; // Skip this block
                    }
                    
                    // Record progress and reset timeout on success
                    progress.record_progress(received_hash);
                    progress.reset_timeout();
                    
                    info!("Received block at height {} from {}", height, peer_addr);
                    blocks.push((height, block, block_witnesses));
                }
                Ok(Err(_)) => {
                    warn!("Block request channel closed for height {}", height);
                    // Continue with next block
                }
                Err(_) => {
                    warn!("Block request timed out for height {} after {} seconds", height, timeout_duration.as_secs());
                    // Continue with next block - don't fail entire chunk
                }
            }
            
            // Small delay to avoid overwhelming the peer
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        
        info!("Downloaded {} blocks from peer {} (heights {} to {})", 
            blocks.len(), peer_id, start_height, end_height);
        
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
        let (stored_witnesses, recent_headers) =
            prepare_block_validation_context(blockstore, block, height)?;

        // Use witnesses from download or stored witnesses
        let witnesses_to_use = if !witnesses.is_empty() {
            witnesses
        } else {
            &stored_witnesses
        };

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
            Err(anyhow::anyhow!("Block validation failed at height {}", height))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parallel_ibd_config_default() {
        let config = ParallelIBDConfig::default();
        assert!(config.num_workers > 0);
        assert_eq!(config.chunk_size, 1000);
        assert_eq!(config.max_concurrent_per_peer, 3);
    }

    #[test]
    fn test_create_chunks() {
        let config = ParallelIBDConfig {
            chunk_size: 100,
            ..Default::default()
        };
        let ibd = ParallelIBD::new(config);
        let peer_ids = vec!["peer1".to_string(), "peer2".to_string()];
        
        let chunks = ibd.create_chunks(0, 250, &peer_ids);
        
        assert_eq!(chunks.len(), 3); // 0-99, 100-199, 200-250
        assert_eq!(chunks[0].start_height, 0);
        assert_eq!(chunks[0].end_height, 99);
        assert_eq!(chunks[1].start_height, 100);
        assert_eq!(chunks[1].end_height, 199);
        assert_eq!(chunks[2].start_height, 200);
        assert_eq!(chunks[2].end_height, 250);
        
        // Check round-robin peer assignment
        assert_eq!(chunks[0].peer_id, "peer1");
        assert_eq!(chunks[1].peer_id, "peer2");
        assert_eq!(chunks[2].peer_id, "peer1");
    }
}

