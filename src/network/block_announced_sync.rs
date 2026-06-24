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

/// Maximum block `getdata` items per `inv` (matches Core-style pipelining limits).
const MAX_BLOCK_REQUESTS_PER_INV: usize = 16;
const HEADERS_TIP_PROBE_TIMEOUT_SECS: u64 = 30;

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
        let blockstore = storage.blocks();
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

        // Multi-block gap: do not fetch tip blocks out of order (parent not in index).
        if let Ok((tip_hash, _)) = storage.chain().get_tip_hash_and_height() {
            if let Ok(Some(probed)) = self.refresh_peer_tip_via_headers(local_height, tip_hash).await
            {
                if probed > local_height.saturating_add(1) {
                    info!(
                        "[BLOCK_RELAY] Local {} peer tip ≈ {} — skipping getdata (catch-up fills gap)",
                        local_height, probed
                    );
                    return Ok(());
                }
            }
        }

        {
            let mut inv_mgr = self.inventory().lock().unwrap_or_else(|e| e.into_inner());
            for item in &inventory {
                inv_mgr
                    .request_data(item.hash, item.inv_type, &peer_key)
                    .map_err(|e| anyhow::anyhow!("inventory request_data: {e}"))?;
            }
        }

        self.bump_peer_best_height(peer_addr, local_height.saturating_add(1))
            .await;

        let getdata = GetDataMessage { inventory };
        let count = getdata.inventory.len();
        let wire =
            ProtocolParser::serialize_message(&ProtocolMessage::GetData(getdata))?;
        self.send_to_peer(peer_addr, wire).await?;
        info!(
            "[BLOCK_RELAY] Requested {} block(s) from {} (local height {})",
            count, peer_addr, local_height
        );
        Ok(())
    }

    /// When version `start_height` equals our tip, probe the best peer with `getheaders` to learn
    /// the live chain tip (handles multi-block gaps while connections stay open).
    pub async fn refresh_peer_tip_via_headers(
        &self,
        local_height: u64,
        tip_hash: Hash,
    ) -> Result<Option<u64>> {
        let peers = self.get_connected_peer_addresses().await;
        if peers.is_empty() {
            return Ok(None);
        }

        let peer_addr = peers[0];
        let get_headers = GetHeadersMessage {
            version: 70015,
            block_locator_hashes: vec![tip_hash],
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
            Duration::from_secs(HEADERS_TIP_PROBE_TIMEOUT_SECS),
            headers_rx,
        )
        .await
        {
            Ok(Ok(headers)) if headers.is_empty() => {
                debug!(
                    "[CATCH_UP] getheaders from {}: empty (peer at our tip)",
                    peer_addr
                );
                Ok(Some(local_height))
            }
            Ok(Ok(headers)) => {
                let peer_tip = local_height.saturating_add(headers.len() as u64);
                self.bump_peer_best_height(peer_addr, peer_tip).await;
                info!(
                    "[CATCH_UP] getheaders from {}: peer tip ≈ {} ({} headers ahead of {})",
                    peer_addr,
                    peer_tip,
                    headers.len(),
                    local_height
                );
                Ok(Some(peer_tip))
            }
            Ok(Err(_)) => {
                warn!("[CATCH_UP] getheaders response channel closed for {}", peer_addr);
                Ok(None)
            }
            Err(_) => {
                warn!(
                    "[CATCH_UP] getheaders tip probe to {} timed out after {}s",
                    peer_addr, HEADERS_TIP_PROBE_TIMEOUT_SECS
                );
                Ok(None)
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
