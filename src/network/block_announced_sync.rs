//! Post-IBD block sync: act on peer `inv` announcements and refresh stale peer tips.
//!
//! After parallel IBD completes, new blocks are announced via `inv` (MSG_BLOCK). Previously the
//! node only recorded inventory and never sent `getdata`, so the main loop never received blocks.
//! Catch-up IBD also keyed off version `start_height`, which does not advance on an open connection
//! when the chain tip moves.

use crate::network::inventory::{MSG_BLOCK, MSG_WITNESS_BLOCK};
use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{
    GetDataMessage, GetHeadersMessage, InventoryVector, ProtocolMessage, ProtocolParser,
};
use crate::network::transport::TransportAddr;
use anyhow::Result;
use blvm_protocol::Hash;
use std::net::SocketAddr;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

/// Maximum block `getdata` items per `inv`. A larger block inventory is a historical range.
pub(crate) const MAX_BLOCK_REQUESTS_PER_INV: usize = 16;

/// Result of a `getheaders` tip probe.
pub(crate) struct HeadersTipProbe {
    pub peer_height: u64,
    /// Hash of the first header that connects to our tip, when one was stored.
    pub first_missing_hash: Option<Hash>,
}

impl NetworkManager {
    /// Request unknown blocks announced in a peer `inv` message (post-IBD tip follow).
    pub async fn process_announced_block_inventory(
        &self,
        data: &[u8],
        peer_addr: SocketAddr,
    ) -> Result<()> {
        let inv_msg = match ProtocolParser::parse_message(data) {
            Ok(ProtocolMessage::Inv(msg)) => msg,
            _ => return Ok(()),
        };

        let Some(storage) = self.storage().as_ref() else {
            return Ok(());
        };

        let local_height = storage.chain().get_height()?.unwrap_or(0);
        // IBD owns GetHeaders/GetData from genesis. A height-0 inv probe
        // registered on peers[0] and poisoned the header FIFO (R-48 640k sit;
        // R-49 100k sit after CATCH_UP timeout + mass disconnect).
        if local_height == 0
            || crate::node::parallel_ibd::PARALLEL_IBD_SESSION_ACTIVE
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(());
        }
        let blockstore = storage.blocks();
        let announced_block = inv_msg
            .inventory
            .iter()
            .any(|item| item.inv_type == MSG_BLOCK || item.inv_type == MSG_WITNESS_BLOCK);
        if announced_block {
            self.note_block_announcement(peer_addr).await;
        }

        let peer_key = peer_addr.to_string();

        let mut inventory: Vec<InventoryVector> = Vec::new();
        {
            let mut inv_mgr = self.inventory().lock().unwrap_or_else(|e| e.into_inner());

            for item in &inv_msg.inventory {
                if item.inv_type != MSG_BLOCK && item.inv_type != MSG_WITNESS_BLOCK {
                    continue;
                }
                if blockstore.has_block_body(&item.hash).unwrap_or(false) {
                    continue;
                }
                if inv_mgr.has_pending_request(&item.hash) {
                    continue;
                }
                inventory.push(InventoryVector {
                    inv_type: MSG_WITNESS_BLOCK,
                    hash: item.hash,
                });
                if inventory.len() >= MAX_BLOCK_REQUESTS_PER_INV {
                    break;
                }
            }
        }

        if inventory.is_empty() {
            return Ok(());
        }

        // Headers stored by the probe are fetched before the raw inv. That covers a
        // same-height sibling as well as a gap of more than one block.
        // Parallel IBD is not started.
        if let Ok((tip_hash, _)) = storage.chain().get_tip_hash_and_height() {
            if let Ok(Some(probed)) = self
                .refresh_peer_tip_from(peer_addr, local_height, tip_hash)
                .await
            {
                if let Some(hash) = probed.first_missing_hash {
                    info!(
                        "[BLOCK_RELAY] Local {} peer tip ≈ {} — getdata for first missing block",
                        local_height, probed.peer_height
                    );
                    let first = vec![InventoryVector {
                        inv_type: MSG_WITNESS_BLOCK,
                        hash,
                    }];
                    return self
                        .send_block_getdata(peer_addr, &peer_key, first, local_height)
                        .await;
                }
            }
        }

        self.send_block_getdata(peer_addr, &peer_key, inventory, local_height)
            .await
    }

    async fn send_block_getdata(
        &self,
        peer_addr: SocketAddr,
        peer_key: &str,
        inventory: Vec<InventoryVector>,
        local_height: u64,
    ) -> Result<()> {
        self.bump_peer_best_height(peer_addr, local_height.saturating_add(1))
            .await;

        let getdata = GetDataMessage {
            inventory: inventory.clone(),
        };
        let count = getdata.inventory.len();
        let wire = ProtocolParser::serialize_message(&ProtocolMessage::GetData(getdata))?;
        self.send_to_peer(peer_addr, wire).await?;
        {
            let mut inv_mgr = self.inventory().lock().unwrap_or_else(|e| e.into_inner());
            for item in &inventory {
                inv_mgr
                    .request_data(item.hash, item.inv_type, peer_key)
                    .map_err(|e| anyhow::anyhow!("inventory request_data: {e}"))?;
            }
        }
        info!(
            "[BLOCK_RELAY] Requested {} block(s) from {} (local height {})",
            count, peer_addr, local_height
        );
        Ok(())
    }

    /// When version `start_height` equals our tip, probe the best peer with `getheaders` to learn
    /// the live chain tip (handles multi-block gaps while connections stay open).
    pub(crate) async fn refresh_peer_tip_via_headers(
        &self,
        local_height: u64,
        tip_hash: Hash,
    ) -> Result<Option<HeadersTipProbe>> {
        let peers = self.get_connected_peer_addresses().await;
        if peers.is_empty() {
            return Ok(None);
        }
        self.refresh_peer_tip_from(peers[0], local_height, tip_hash)
            .await
    }

    /// Probe `peer_addr` with `getheaders` and store the linking prefix.
    pub(crate) async fn refresh_peer_tip_from(
        &self,
        peer_addr: SocketAddr,
        local_height: u64,
        tip_hash: Hash,
    ) -> Result<Option<HeadersTipProbe>> {
        let get_headers = GetHeadersMessage {
            version: 70015,
            block_locator_hashes: self.tip_follow_locator(local_height, tip_hash),
            hash_stop: [0; 32],
        };
        let wire = ProtocolParser::serialize_message(&ProtocolMessage::GetHeaders(get_headers))?;
        let headers_rx = self.register_headers_request(peer_addr);

        if let Err(e) = self.send_to_peer(peer_addr, wire).await {
            debug!(
                "[CATCH_UP] getheaders tip probe to {} failed: {}",
                peer_addr, e
            );
            return Ok(None);
        }

        match timeout(
            Duration::from_secs(self.headers_probe_timeout_secs()),
            headers_rx,
        )
        .await
        {
            Ok(Ok(headers)) if headers.is_empty() => {
                debug!(
                    "[CATCH_UP] getheaders from {}: empty (no shared block after the locator)",
                    peer_addr
                );
                Ok(Some(HeadersTipProbe {
                    peer_height: 0,
                    first_missing_hash: None,
                }))
            }
            Ok(Ok(headers)) => {
                let (stored, first_missing_hash, last_height) =
                    self.store_headers_linking_to_tip(tip_hash, local_height, &headers);
                let peer_tip = last_height.unwrap_or(local_height);
                if stored > 0 {
                    self.bump_peer_best_height(peer_addr, peer_tip).await;
                }
                info!(
                    "[CATCH_UP] getheaders from {}: peer tip ≈ {} ({} headers stored, {} received, local {})",
                    peer_addr,
                    peer_tip,
                    stored,
                    headers.len(),
                    local_height
                );
                Ok(Some(HeadersTipProbe {
                    peer_height: peer_tip,
                    first_missing_hash,
                }))
            }
            Ok(Err(_)) => {
                warn!(
                    "[CATCH_UP] getheaders response channel closed for {}",
                    peer_addr
                );
                Ok(None)
            }
            Err(_) => {
                warn!(
                    "[CATCH_UP] getheaders tip probe to {} timed out after {}s",
                    peer_addr,
                    self.headers_probe_timeout_secs()
                );
                Ok(None)
            }
        }
    }

    /// Locator hashes from the tip back to genesis, the same steps as header sync.
    fn tip_follow_locator(&self, local_height: u64, tip_hash: Hash) -> Vec<Hash> {
        use crate::node::parallel_ibd::ibd_header_locator_heights;
        use blvm_protocol::GENESIS_BLOCK_HASH_INTERNAL;

        let Some(storage) = self.storage().as_ref() else {
            return vec![tip_hash];
        };
        let blocks = storage.blocks();
        let mut hashes = Vec::new();
        for height in ibd_header_locator_heights(local_height) {
            if height == local_height {
                hashes.push(tip_hash);
                continue;
            }
            match blocks.get_hash_by_height(height) {
                Ok(Some(stored)) => hashes.push(stored),
                Ok(None) if height == 0 => hashes.push(GENESIS_BLOCK_HASH_INTERNAL),
                _ => {}
            }
        }
        if hashes.is_empty() {
            hashes.push(tip_hash);
        }
        hashes
    }

    /// Store the prefix of `headers` that connects to the tip or to a known ancestor.
    ///
    /// Returns the stored count, the first stored hash, and the height of the last
    /// stored header. A header that fails proof of work or breaks the chain ends the prefix.
    /// A header whose height already names a different block is stored by hash and does
    /// not replace the active height entry.
    fn store_headers_linking_to_tip(
        &self,
        tip_hash: Hash,
        local_height: u64,
        headers: &[blvm_protocol::BlockHeader],
    ) -> (usize, Option<Hash>, Option<u64>) {
        let Some(storage) = self.storage().as_ref() else {
            return (0, None, None);
        };
        let mut anchor: Option<(usize, Hash, u64)> = None;
        for (i, header) in headers.iter().enumerate() {
            if header.prev_block_hash == tip_hash {
                anchor = Some((i, tip_hash, local_height));
                break;
            }
            if let Ok(Some(parent_height)) =
                storage.blocks().get_height_by_hash(&header.prev_block_hash)
            {
                anchor = Some((i, header.prev_block_hash, parent_height));
                break;
            }
        }
        let Some((start, mut prev, mut height)) = anchor else {
            return (0, None, None);
        };
        let mut batch = Vec::new();
        for header in headers.iter().skip(start) {
            if header.version < 1
                || header.timestamp == 0
                || header.prev_block_hash != prev
                || !blvm_protocol::pow::check_proof_of_work(header).unwrap_or(false)
            {
                break;
            }
            height = height.saturating_add(1);
            let hash = blvm_consensus::block::block_header_hash(header);
            batch.push((hash, header.clone(), height));
            prev = hash;
        }
        let stored = batch.len();
        let first = batch.first().map(|(hash, _, _)| *hash);
        let last_height = batch.last().map(|(_, _, height)| *height);
        if stored == 0 {
            return (0, None, None);
        }
        let mut on_active = Vec::new();
        for (hash, header, height) in &batch {
            let occupied = storage.blocks().get_hash_by_height(*height).ok().flatten();
            if occupied.is_some_and(|existing| existing != *hash) {
                if let Err(e) = storage.blocks().store_header(hash, header) {
                    warn!("probe headers failed to store: {e}");
                    return (0, None, None);
                }
                if let Err(e) = storage.blocks().store_hash_height(*height, hash) {
                    warn!("probe headers failed to store: {e}");
                    return (0, None, None);
                }
            } else {
                on_active.push((*hash, header.clone(), *height));
            }
        }
        if !on_active.is_empty() {
            if let Err(e) = storage.blocks().store_headers_batch(&on_active) {
                warn!("probe headers failed to store: {e}");
                return (0, None, None);
            }
        }
        (stored, first, last_height)
    }

    pub(crate) async fn note_block_announcement(&self, peer_addr: SocketAddr) {
        let mut pm = self.peer_manager().await;
        let transport = pm
            .find_transport_addr_by_socket(peer_addr)
            .unwrap_or(TransportAddr::Tcp(peer_addr));
        if let Some(peer) = pm.get_peer_mut(&transport) {
            peer.record_block_announcement();
        }
    }

    /// Tell connected peers about a block that just became the tip.
    pub(crate) async fn announce_connected_block(
        &self,
        hash: Hash,
        header: blvm_protocol::BlockHeader,
    ) {
        if !self.relay_policies().enable_block_relay {
            return;
        }
        let peers = self.get_connected_peer_addresses().await;
        for peer_addr in peers {
            let prefer_headers = self
                .peer_states()
                .read()
                .await
                .get(&peer_addr)
                .map(|state| state.prefer_headers)
                .unwrap_or(false);
            let message = if prefer_headers {
                ProtocolMessage::Headers(crate::network::protocol::HeadersMessage {
                    headers: vec![header.clone()],
                })
            } else {
                ProtocolMessage::Inv(crate::network::protocol::InvMessage {
                    inventory: vec![InventoryVector {
                        inv_type: MSG_WITNESS_BLOCK,
                        hash,
                    }],
                })
            };
            if let Ok(wire) = ProtocolParser::serialize_message(&message) {
                let _ = self.send_to_peer(peer_addr, wire).await;
            }
        }
    }

    /// Tell connected peers about a transaction the mempool accepted.
    /// A peer with a fee filter receives the inv only when the rate meets that filter.
    pub(crate) async fn announce_accepted_transaction(
        &self,
        txid: Hash,
        fee_basis: Option<(u64, u64)>,
    ) {
        if !self.relay_policies().enable_tx_relay {
            return;
        }
        let peers = self.get_connected_peer_addresses().await;
        let mut eligible = Vec::new();
        for peer_addr in peers {
            let allowed = {
                let states = self.peer_states().read().await;
                match states.get(&peer_addr) {
                    Some(state) if !state.relay_tx => false,
                    Some(state) => match state.min_fee_rate {
                        Some(min) => match fee_basis {
                            Some((fee_sats, vsize)) => super::wire_dispatch::fee_rate_meets_filter(
                                fee_sats,
                                vsize,
                                min,
                            ),
                            None => false,
                        },
                        None => true,
                    },
                    None => true,
                }
            };
            if allowed {
                eligible.push(peer_addr);
            }
        }
        let peer_ids: Vec<String> = eligible.iter().map(|addr| addr.to_string()).collect();
        let stem_peer = {
            let mut relay = self
                .relay_manager()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !relay.should_relay_transaction(&txid) {
                return;
            }
            let stem = relay.relay_transaction_dandelion(txid, "local".to_string(), &peer_ids);
            relay.mark_transaction_relayed(txid);
            stem
        };
        let targets: Vec<SocketAddr> = match stem_peer {
            Some(peer_id) => eligible
                .into_iter()
                .filter(|addr| addr.to_string() == peer_id)
                .collect(),
            None => eligible,
        };
        for peer_addr in targets {
            let message = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
                inventory: vec![InventoryVector {
                    inv_type: crate::network::inventory::MSG_WITNESS_TX,
                    hash: txid,
                }],
            });
            if let Ok(wire) = ProtocolParser::serialize_message(&message) {
                let _ = self.send_to_peer(peer_addr, wire).await;
            }
        }
    }

    async fn bump_peer_best_height(&self, peer_addr: SocketAddr, height: u64) {
        let mut pm = self.peer_manager().await;
        let transport = pm
            .find_transport_addr_by_socket(peer_addr)
            .unwrap_or(TransportAddr::Tcp(peer_addr));
        if let Some(peer) = pm.get_peer_mut(&transport) {
            let current = peer
                .best_block_height()
                .unwrap_or(peer.start_height().max(0) as u64);
            if height > current {
                peer.set_best_block_height(height);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::transport::TransportAddr;
    use crate::node::mempool::MempoolManager;
    use crate::storage::Storage;
    use blvm_protocol::{BitcoinProtocolEngine, BlockHeader, ProtocolVersion};
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    async fn read_frame(sock: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut header = [0u8; 24];
        sock.read_exact(&mut header).await.unwrap();
        let len = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
        let mut frame = header.to_vec();
        let mut payload = vec![0u8; len];
        sock.read_exact(&mut payload).await.unwrap();
        frame.extend(payload);
        frame
    }

    fn connect_tip(storage: &Storage, tip_hash: &Hash, tip_header: &BlockHeader, height: u64) {
        if height > 0 {
            storage
                .chain()
                .cache_block_chainwork(&tip_header.prev_block_hash, tip_header, 0)
                .unwrap();
        }
        storage
            .chain()
            .update_tip(tip_hash, tip_header, height)
            .unwrap();
    }

    fn with_proof(mut header: BlockHeader) -> BlockHeader {
        for nonce in 0..200_000 {
            header.nonce = nonce;
            if blvm_protocol::pow::check_proof_of_work(&header).unwrap_or(false) {
                return header;
            }
        }
        panic!("header did not meet the regtest target");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inv_lock_two_headers_stores_and_requests_first_block() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let tip_hash = [9u8; 32];
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        };
        connect_tip(&storage, &tip_hash, &tip_header, 1);

        let header_one = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: tip_hash,
            merkle_root: [2u8; 32],
            timestamp: 1_700_000_001,
            bits: 0x207fffff,
            nonce: 0,
        });
        let hash_one = blvm_consensus::block::block_header_hash(&header_one);
        let header_two = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: hash_one,
            merkle_root: [3u8; 32],
            timestamp: 1_700_000_002,
            bits: 0x207fffff,
            nonce: 0,
        });
        let hash_two = blvm_consensus::block::block_header_hash(&header_two);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                Arc::clone(&storage),
                Arc::new(MempoolManager::new()),
            ),
        );

        let nm_read = Arc::clone(&nm);
        let announced = vec![header_one.clone(), header_two.clone()];
        let reader = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut getdata = None;
            for _ in 0..4 {
                let frame = read_frame(&mut sock).await;
                match ProtocolParser::parse_message(&frame).unwrap() {
                    ProtocolMessage::GetHeaders(_) => {
                        assert!(nm_read.complete_headers_request(peer_addr, announced.clone()));
                    }
                    ProtocolMessage::GetData(msg) => {
                        getdata = Some(msg);
                        break;
                    }
                    _ => {}
                }
            }
            getdata.expect("getdata")
        });

        nm.connect_to_peer(peer_addr).await.unwrap();
        let inv = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
            inventory: vec![
                InventoryVector {
                    inv_type: MSG_BLOCK,
                    hash: hash_one,
                },
                InventoryVector {
                    inv_type: MSG_BLOCK,
                    hash: hash_two,
                },
            ],
        });
        let inv_bytes = ProtocolParser::serialize_message(&inv).unwrap();
        timeout(
            Duration::from_secs(10),
            nm.process_announced_block_inventory(&inv_bytes, peer_addr),
        )
        .await
        .expect("headers probe")
        .unwrap();

        let getdata = timeout(Duration::from_secs(5), reader)
            .await
            .expect("reader")
            .unwrap();
        assert_eq!(getdata.inventory.len(), 1);
        assert_eq!(getdata.inventory[0].hash, hash_one);
        assert_eq!(getdata.inventory[0].inv_type, MSG_WITNESS_BLOCK);

        let stored_one = storage
            .blocks()
            .get_header_at_height(2)
            .unwrap()
            .expect("first header");
        let stored_two = storage
            .blocks()
            .get_header_at_height(3)
            .unwrap()
            .expect("second header");
        assert_eq!(stored_one.prev_block_hash, tip_hash);
        assert_eq!(stored_two.prev_block_hash, hash_one);
        assert!(
            !crate::node::parallel_ibd::PARALLEL_IBD_SESSION_ACTIVE
                .load(std::sync::atomic::Ordering::Acquire),
            "parallel IBD stays idle for this gap"
        );
    }

    async fn best_height(nm: &NetworkManager, peer: SocketAddr) -> Option<u64> {
        let pm = nm.peer_manager().await;
        let transport = pm
            .find_transport_addr_by_socket(peer)
            .unwrap_or(TransportAddr::Tcp(peer));
        pm.get_peer(&transport)
            .and_then(|peer| peer.best_block_height())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_height_uses_the_stored_header_count() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let tip_hash = [9u8; 32];
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        };
        connect_tip(&storage, &tip_hash, &tip_header, 1);
        let linking = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: tip_hash,
            merkle_root: [2u8; 32],
            timestamp: 1_700_000_001,
            bits: 0x207fffff,
            nonce: 0,
        });
        let unlinked = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [3u8; 32],
            timestamp: 1_700_000_002,
            bits: 0x207fffff,
            nonce: 4,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                Arc::clone(&storage),
                Arc::new(MempoolManager::new()),
            ),
        );
        let nm_read = Arc::clone(&nm);
        let reply = vec![linking, unlinked];
        let reader = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let frame = read_frame(&mut sock).await;
            match ProtocolParser::parse_message(&frame).unwrap() {
                ProtocolMessage::GetHeaders(_) => {
                    assert!(nm_read.complete_headers_request(peer_addr, reply));
                }
                other => panic!("expected getheaders, got {other:?}"),
            }
        });
        nm.connect_to_peer(peer_addr).await.unwrap();
        let probed = timeout(
            Duration::from_secs(5),
            nm.refresh_peer_tip_from(peer_addr, 1, tip_hash),
        )
        .await
        .expect("probe")
        .unwrap()
        .expect("headers");
        timeout(Duration::from_secs(5), reader)
            .await
            .expect("reader")
            .unwrap();
        assert_eq!(probed.peer_height, 2);
        assert_eq!(best_height(&nm, peer_addr).await, Some(2));
        assert!(storage.blocks().get_hash_by_height(3).unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unlinked_probe_leaves_peer_height_unchanged() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let tip_hash = [9u8; 32];
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        };
        connect_tip(&storage, &tip_hash, &tip_header, 1);
        let unlinked = BlockHeader {
            version: 1,
            prev_block_hash: [1u8; 32],
            merkle_root: [3u8; 32],
            timestamp: 1_700_000_002,
            bits: 0x207fffff,
            nonce: 4,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                storage,
                Arc::new(MempoolManager::new()),
            ),
        );
        let nm_read = Arc::clone(&nm);
        let reader = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let frame = read_frame(&mut sock).await;
            match ProtocolParser::parse_message(&frame).unwrap() {
                ProtocolMessage::GetHeaders(_) => {
                    assert!(nm_read.complete_headers_request(peer_addr, vec![unlinked]));
                }
                other => panic!("expected getheaders, got {other:?}"),
            }
        });
        nm.connect_to_peer(peer_addr).await.unwrap();
        let probed = timeout(
            Duration::from_secs(5),
            nm.refresh_peer_tip_from(peer_addr, 1, tip_hash),
        )
        .await
        .expect("probe")
        .unwrap()
        .expect("headers");
        timeout(Duration::from_secs(5), reader)
            .await
            .expect("reader")
            .unwrap();
        assert_eq!(probed.peer_height, 1);
        assert!(best_height(&nm, peer_addr).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_headers_reply_does_not_claim_the_local_height() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let tip_hash = [9u8; 32];
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        };
        connect_tip(&storage, &tip_hash, &tip_header, 1);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                storage,
                Arc::new(MempoolManager::new()),
            ),
        );
        let nm_read = Arc::clone(&nm);
        let reader = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let frame = read_frame(&mut sock).await;
            match ProtocolParser::parse_message(&frame).unwrap() {
                ProtocolMessage::GetHeaders(_) => {
                    assert!(nm_read.complete_headers_request(peer_addr, Vec::new()));
                }
                other => panic!("expected getheaders, got {other:?}"),
            }
        });
        nm.connect_to_peer(peer_addr).await.unwrap();
        let probed = timeout(
            Duration::from_secs(5),
            nm.refresh_peer_tip_from(peer_addr, 1, tip_hash),
        )
        .await
        .expect("probe")
        .unwrap()
        .expect("headers");
        timeout(Duration::from_secs(5), reader)
            .await
            .expect("reader")
            .unwrap();
        assert_eq!(probed.peer_height, 0);
        assert!(probed.first_missing_hash.is_none());
        assert!(best_height(&nm, peer_addr).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ancestor_header_is_stored_without_replacing_the_tip_height() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let parent = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [4u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        });
        let parent_hash = blvm_consensus::block::block_header_hash(&parent);
        storage
            .blocks()
            .store_header(&parent_hash, &parent)
            .unwrap();
        storage.blocks().store_height(0, &parent_hash).unwrap();
        let tip_hash = [9u8; 32];
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_001,
            bits: 0x207fffff,
            nonce: 1,
        };
        connect_tip(&storage, &tip_hash, &tip_header, 1);
        storage.blocks().store_height(1, &tip_hash).unwrap();
        let sibling = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [8u8; 32],
            timestamp: 1_700_000_002,
            bits: 0x207fffff,
            nonce: 0,
        });
        let sibling_hash = blvm_consensus::block::block_header_hash(&sibling);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                Arc::clone(&storage),
                Arc::new(MempoolManager::new()),
            ),
        );
        let nm_read = Arc::clone(&nm);
        let sibling_reply = sibling.clone();
        let reader = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut getdata = None;
            for _ in 0..4 {
                let frame = read_frame(&mut sock).await;
                match ProtocolParser::parse_message(&frame).unwrap() {
                    ProtocolMessage::GetHeaders(msg) => {
                        assert!(msg.block_locator_hashes.contains(&parent_hash));
                        assert!(msg.block_locator_hashes.contains(&tip_hash));
                        assert!(
                            nm_read
                                .complete_headers_request(peer_addr, vec![sibling_reply.clone()])
                        );
                    }
                    ProtocolMessage::GetData(msg) => {
                        getdata = Some(msg);
                        break;
                    }
                    _ => {}
                }
            }
            getdata.expect("getdata for the stored sibling")
        });
        nm.connect_to_peer(peer_addr).await.unwrap();
        let announced = [0xcdu8; 32];
        let inv = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
            inventory: vec![InventoryVector {
                inv_type: MSG_BLOCK,
                hash: announced,
            }],
        });
        let inv_bytes = ProtocolParser::serialize_message(&inv).unwrap();
        timeout(
            Duration::from_secs(5),
            nm.process_announced_block_inventory(&inv_bytes, peer_addr),
        )
        .await
        .expect("inv")
        .unwrap();
        let getdata = timeout(Duration::from_secs(5), reader)
            .await
            .expect("reader")
            .unwrap();
        assert_eq!(getdata.inventory.len(), 1);
        assert_eq!(getdata.inventory[0].hash, sibling_hash);
        assert_ne!(getdata.inventory[0].hash, announced);
        assert_eq!(
            storage.blocks().get_hash_by_height(1).unwrap().unwrap(),
            tip_hash
        );
        assert!(
            storage
                .blocks()
                .get_header(&sibling_hash)
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn announcer_is_probed_and_still_receives_getdata() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let tip_hash = [9u8; 32];
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        };
        connect_tip(&storage, &tip_hash, &tip_header, 1);
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                storage,
                Arc::new(MempoolManager::new()),
            ),
        );
        let listener_b = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_b = listener_b.local_addr().unwrap();
        let listener_a = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_a = listener_a.local_addr().unwrap();
        let announced = [0xabu8; 32];
        tokio::spawn(async move {
            let (mut sock, _) = listener_b.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let _ = timeout(Duration::from_secs(8), sock.read(&mut buf)).await;
        });
        let nm_read = Arc::clone(&nm);
        let reader_a = tokio::spawn(async move {
            let (mut sock, _) = listener_a.accept().await.unwrap();
            let mut saw_headers = false;
            let mut getdata = None;
            for _ in 0..6 {
                let frame = timeout(Duration::from_secs(5), read_frame(&mut sock))
                    .await
                    .expect("frame from A");
                match ProtocolParser::parse_message(&frame).unwrap() {
                    ProtocolMessage::GetHeaders(_) => {
                        saw_headers = true;
                        let unrelated = vec![
                            BlockHeader {
                                version: 1,
                                prev_block_hash: [1u8; 32],
                                merkle_root: [2u8; 32],
                                timestamp: 1_700_000_003,
                                bits: 0x207fffff,
                                nonce: 1,
                            },
                            BlockHeader {
                                version: 1,
                                prev_block_hash: [3u8; 32],
                                merkle_root: [4u8; 32],
                                timestamp: 1_700_000_004,
                                bits: 0x207fffff,
                                nonce: 2,
                            },
                        ];
                        assert!(nm_read.complete_headers_request(peer_a, unrelated));
                    }
                    ProtocolMessage::GetData(msg) => {
                        getdata = Some(msg);
                        break;
                    }
                    _ => {}
                }
            }
            assert!(saw_headers, "announcer received getheaders");
            getdata.expect("announcer received getdata")
        });
        nm.connect_to_peer(peer_b).await.unwrap();
        nm.connect_to_peer(peer_a).await.unwrap();
        let inv = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
            inventory: vec![InventoryVector {
                inv_type: MSG_BLOCK,
                hash: announced,
            }],
        });
        let inv_bytes = ProtocolParser::serialize_message(&inv).unwrap();
        timeout(
            Duration::from_secs(5),
            nm.process_announced_block_inventory(&inv_bytes, peer_a),
        )
        .await
        .expect("inv handled")
        .unwrap();
        let getdata = timeout(Duration::from_secs(5), reader_a)
            .await
            .expect("reader")
            .unwrap();
        assert!(getdata.inventory.iter().any(|item| item.hash == announced));
    }
}
