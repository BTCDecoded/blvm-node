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
                Self::download_chunk(start, end, &peer_id, network_clone, &blockstore_clone).await
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
    /// Iteratively requests headers until chain tip is reached (fewer than 2000 headers returned)
    async fn download_headers(
        &self,
        start_height: u64,
        end_height: u64,
        peer_ids: &[String],
        blockstore: &BlockStore,
        network: Option<Arc<NetworkManager>>,
    ) -> Result<u64> {
        info!(
            "Downloading headers from height {} to {} (up to {} headers)",
            start_height,
            end_height,
            end_height - start_height + 1
        );

        // If network is not available, skip header download (for testing)
        let network = match network.as_ref() {
            Some(n) => n,
            None => {
                warn!("NetworkManager not available, skipping header download");
                return Ok(start_height);
            }
        };

        // Select best peer for header download (prefer peer with highest start_height)
        if peer_ids.is_empty() {
            return Err(anyhow::anyhow!("No peers available for header download"));
        }

        // Find peer with highest start_height (best synced peer)
        // Query network manager for each peer's start_height
        let mut best_peer_addr: Option<SocketAddr> = None;
        let mut best_start_height: i32 = -1;
        
        let pm = network.peer_manager().await;
        for peer_id_str in peer_ids {
            if let Ok(peer_addr) = peer_id_str.parse::<SocketAddr>() {
                // Find peer by SocketAddr (try TCP transport address)
                use crate::network::transport::TransportAddr;
                let transport_addr = TransportAddr::Tcp(peer_addr);
                let peer_start_height = if let Some(peer) = pm.get_peer(&transport_addr) {
                    peer.start_height()
                } else {
                    0 // Default if peer not found
                };
                
                if peer_start_height > best_start_height {
                    best_start_height = peer_start_height;
                    best_peer_addr = Some(peer_addr);
                }
            }
        }
        drop(pm); // Release lock before async operations
        
        let mut current_peer_index = 0;
        let peer_addrs: Vec<SocketAddr> = peer_ids.iter()
            .filter_map(|id| id.parse::<SocketAddr>().ok())
            .collect();
        
        if peer_addrs.is_empty() {
            return Err(anyhow::anyhow!("No valid peer addresses found"));
        }
        
        // Use best peer if found, otherwise use first peer
        let mut peer_addr = best_peer_addr.unwrap_or(peer_addrs[0]);

        let mut current_height = start_height;
        let mut consecutive_failures = 0;
        const MAX_CONSECUTIVE_FAILURES: u32 = 3;
        const BITCOIN_MAX_HEADERS_PER_RESPONSE: usize = 2000;

        // Iteratively download headers until we reach the chain tip
        loop {
            // Check if we've reached the target height
            if current_height >= end_height {
                info!("Reached target height {}, header sync complete", end_height);
                break;
            }

            // Build block locator hashes
            // Use the highest known header as locator
            let mut locator_hashes = Vec::new();
            
            if current_height > 0 {
                // Get the hash at current_height - 1 (the last header we have)
                let locator_height = current_height.saturating_sub(1);
                if let Ok(Some(locator_hash)) = blockstore.get_hash_by_height(locator_height) {
                    locator_hashes.push(locator_hash);
                } else {
                    // Fallback: try to find any header we have
                    let mut found = false;
                    for h in (0..current_height).rev().take(10) {
                        if let Ok(Some(hash)) = blockstore.get_hash_by_height(h) {
                            locator_hashes.push(hash);
                            found = true;
                            break;
                        }
                    }
                    if !found && current_height == start_height && start_height == 0 {
                        // Genesis case
                        let genesis_hash: [u8; 32] = [
                            0x6f, 0xe2, 0x8c, 0x0a, 0xb6, 0xf1, 0xb3, 0x72,
                            0xc1, 0xa6, 0xa2, 0x46, 0xae, 0x63, 0xf7, 0x4f,
                            0x93, 0x1e, 0x83, 0x65, 0xe1, 0x5a, 0x08, 0x9c,
                            0x68, 0xd6, 0x19, 0x00, 0x00, 0x00, 0x00, 0x00,
                        ];
                        locator_hashes.push(genesis_hash);
                        info!("Using genesis block hash as locator for initial header sync");
                    }
                }
            } else {
                // For start_height = 0, include genesis block hash as locator
                let genesis_hash: [u8; 32] = [
                    0x6f, 0xe2, 0x8c, 0x0a, 0xb6, 0xf1, 0xb3, 0x72,
                    0xc1, 0xa6, 0xa2, 0x46, 0xae, 0x63, 0xf7, 0x4f,
                    0x93, 0x1e, 0x83, 0x65, 0xe1, 0x5a, 0x08, 0x9c,
                    0x68, 0xd6, 0x19, 0x00, 0x00, 0x00, 0x00, 0x00,
                ];
                locator_hashes.push(genesis_hash);
                if current_height == start_height {
                    info!("Using genesis block hash as locator for initial header sync");
                }
            }

            if locator_hashes.is_empty() {
                return Err(anyhow::anyhow!("Failed to build locator hashes for height {}", current_height));
            }

            // Create GetHeaders message
            let get_headers = GetHeadersMessage {
                version: 70015, // Protocol version
                block_locator_hashes: locator_hashes,
                hash_stop: [0; 32], // Request all headers (0 means no stop)
            };

            // Register pending Headers request
            let headers_rx = network.register_headers_request(peer_addr);
            
            // Serialize and send message
            let wire_msg = ProtocolParser::serialize_message(&ProtocolMessage::GetHeaders(get_headers))?;
            
            // Send request
            if let Err(e) = network.send_to_peer(peer_addr, wire_msg).await {
                warn!("Failed to send GetHeaders message to {}: {}", peer_addr, e);
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    return Err(anyhow::anyhow!("Too many consecutive failures sending GetHeaders"));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }

            debug!("Sent GetHeaders request for headers starting at height {}, waiting for response...", current_height);
            
            // Wait for Headers response with timeout
            let timeout_duration = Duration::from_secs(self.config.download_timeout_secs);
            match timeout(timeout_duration, headers_rx).await {
                Ok(Ok(headers)) => {
                    consecutive_failures = 0; // Reset failure counter on success
                    
                    if headers.is_empty() {
                        info!("Received empty headers response - reached chain tip at height {}", current_height);
                        break;
                    }
                    
                    info!("Received {} headers from {} (starting at height {})", headers.len(), peer_addr, current_height);
                    
                    // Store headers in BlockStore
                    let mut stored_count = 0;
                    for header in headers {
                        // Calculate block hash from header
                        let header_hash = blockstore.get_block_hash(&Block {
                            header: header.clone(),
                            transactions: vec![].into_boxed_slice(),
                        });
                        blockstore.store_header(&header_hash, &header)
                            .context("Failed to store header")?;
                        blockstore.store_height(current_height, &header_hash)
                            .context("Failed to store height")?;
                        current_height += 1;
                        stored_count += 1;
                        
                        // Safety check: don't exceed end_height
                        if current_height > end_height {
                            info!("Reached target height {}, stopping header sync", end_height);
                            break;
                        }
                    }
                    
                    info!("Stored {} headers (heights {} to {})", 
                        stored_count, current_height - stored_count, current_height - 1);
                    
                    // If we got fewer than the max, we've reached the chain tip
                    if stored_count < BITCOIN_MAX_HEADERS_PER_RESPONSE as u64 {
                        info!("Received {} headers (less than max {}), reached chain tip at height {}", 
                            stored_count, BITCOIN_MAX_HEADERS_PER_RESPONSE, current_height - 1);
                        break;
                    }
                    
                    // Progress update every 10k headers
                    if current_height % 10000 == 0 {
                        info!("Header sync progress: {} headers synced (target: {})", 
                            current_height - start_height, end_height - start_height);
                    }
                }
                Ok(Err(_)) => {
                    warn!("Headers request channel closed for peer {}", peer_addr);
                    consecutive_failures += 1;
                    // Try next peer
                    current_peer_index = (current_peer_index + 1) % peer_addrs.len();
                    peer_addr = peer_addrs[current_peer_index];
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES * peer_addrs.len() as u32 {
                        return Err(anyhow::anyhow!("Headers request channel closed after {} attempts across all peers", consecutive_failures));
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                Err(_) => {
                    warn!("Headers request timed out after {} seconds for peer {}, trying next peer", 
                        timeout_duration.as_secs(), peer_addr);
                    consecutive_failures += 1;
                    // Try next peer
                    current_peer_index = (current_peer_index + 1) % peer_addrs.len();
                    peer_addr = peer_addrs[current_peer_index];
                    info!("Switching to peer {} for header download", peer_addr);
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES * peer_addrs.len() as u32 {
                        return Err(anyhow::anyhow!("Headers request timed out after {} attempts across all peers", consecutive_failures));
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        }
        
        let total_synced = current_height - start_height;
        info!("Header sync complete: synced {} headers (heights {} to {})", 
            total_synced, start_height, current_height - 1);
        
        Ok(current_height - 1) // Return the highest height we synced
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
    ) -> Result<Vec<(u64, Block, Vec<Vec<Witness>>)>> {
        let mut blocks = Vec::new();

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
        let timeout_duration = Duration::from_secs(30); // 30 second timeout per block
        
        for (height, block_hash) in block_hashes {
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

