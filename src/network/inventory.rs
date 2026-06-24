//! Inventory management
//!
//! Handles inventory tracking, peer inventory synchronization, and data requests.

use anyhow::Result;
use blvm_protocol::Hash;
use std::collections::{HashMap, HashSet};
use tracing::{debug, info, warn};

use super::protocol::{GetDataMessage, InventoryVector};

/// Inventory types
pub const MSG_TX: u32 = 1;
pub const MSG_BLOCK: u32 = 2;
pub const MSG_FILTERED_BLOCK: u32 = 3;
pub const MSG_CMPCT_BLOCK: u32 = 4;
/// Request block with full SegWit witness data (BIP144). Use instead of MSG_BLOCK for heights
/// at or after SegWit activation (481824 mainnet) to ensure witness data is included in the
/// peer's response. Peers that don't support SegWit respond with MSG_BLOCK format (harmless).
pub const MSG_WITNESS_BLOCK: u32 = 0x40000002;

/// Maximum entries in the global `known_inventory` set.
/// Matches Bitcoin Core's `INVENTORY_BROADCAST_MAX` order-of-magnitude (5x headroom).
const MAX_KNOWN_INVENTORY: usize = 250_000;
/// Maximum inventory entries stored per peer.
const MAX_PER_PEER_INVENTORY: usize = 50_000;

/// Inventory manager
pub struct InventoryManager {
    /// Known inventory items
    known_inventory: HashSet<Hash>,
    /// Pending requests
    pending_requests: HashMap<Hash, InventoryRequest>,
    /// Peer inventories
    peer_inventories: HashMap<String, HashSet<Hash>>,
}

impl Default for InventoryManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Inventory request
#[derive(Debug, Clone)]
pub struct InventoryRequest {
    pub inv_type: u32,
    pub hash: Hash,
    pub timestamp: u64,
    pub peer: String,
}

impl InventoryManager {
    /// Create a new inventory manager
    pub fn new() -> Self {
        Self {
            known_inventory: HashSet::new(),
            pending_requests: HashMap::new(),
            peer_inventories: HashMap::new(),
        }
    }

    /// Add inventory items from a peer
    pub fn add_inventory(&mut self, peer: &str, inventory: &[InventoryVector]) -> Result<()> {
        let peer_inv = self.peer_inventories.entry(peer.to_string()).or_default();

        for item in inventory {
            // Per-peer cap: silently stop accepting once the peer's set is full.
            if peer_inv.len() >= MAX_PER_PEER_INVENTORY {
                warn!(
                    "Peer {} inventory cap ({}) reached, ignoring remaining items",
                    peer, MAX_PER_PEER_INVENTORY
                );
                break;
            }
            peer_inv.insert(item.hash);

            // Global cap: if we're full, don't grow the shared set further.
            if self.known_inventory.len() < MAX_KNOWN_INVENTORY {
                self.known_inventory.insert(item.hash);
            }

            debug!("Added inventory item {:?} from peer {}", item, peer);
        }

        Ok(())
    }

    /// Check if we have an inventory item
    pub fn has_inventory(&self, hash: &Hash) -> bool {
        self.known_inventory.contains(hash)
    }

    /// Request data for an inventory item
    pub fn request_data(
        &mut self,
        hash: Hash,
        inv_type: u32,
        peer: &str,
    ) -> Result<GetDataMessage> {
        let request = InventoryRequest {
            inv_type,
            hash,
            timestamp: crate::utils::current_timestamp(),
            peer: peer.to_string(),
        };

        self.pending_requests.insert(hash, request.clone());

        let inventory = vec![InventoryVector { inv_type, hash }];

        Ok(GetDataMessage { inventory })
    }

    /// Whether a download for `hash` is already in flight.
    pub fn has_pending_request(&self, hash: &Hash) -> bool {
        self.pending_requests.contains_key(hash)
    }

    /// Mark request as fulfilled
    pub fn mark_fulfilled(&mut self, hash: &Hash) {
        self.pending_requests.remove(hash);
        debug!("Marked request for {} as fulfilled", hex::encode(hash));
    }

    /// Get pending requests
    pub fn get_pending_requests(&self) -> Vec<&InventoryRequest> {
        self.pending_requests.values().collect()
    }

    /// Clean up old pending requests
    pub fn cleanup_old_requests(&mut self, max_age_seconds: u64) {
        let now = crate::utils::current_timestamp();

        let old_requests: Vec<Hash> = self
            .pending_requests
            .iter()
            .filter(|(_, request)| {
                let age = now.saturating_sub(request.timestamp);
                age >= max_age_seconds
            })
            .map(|(hash, _)| *hash)
            .collect();

        for hash in old_requests {
            self.pending_requests.remove(&hash);
            warn!("Removed old pending request for {}", hex::encode(hash));
        }
    }

    /// Get inventory for a peer
    pub fn get_peer_inventory(&self, peer: &str) -> Option<&HashSet<Hash>> {
        self.peer_inventories.get(peer)
    }

    /// Remove peer inventory
    pub fn remove_peer(&mut self, peer: &str) {
        self.peer_inventories.remove(peer);
        info!("Removed inventory for peer {}", peer);
    }

    /// Get total inventory count
    pub fn inventory_count(&self) -> usize {
        self.known_inventory.len()
    }

    /// Get pending request count
    pub fn pending_request_count(&self) -> usize {
        self.pending_requests.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_pending_request_tracks_in_flight_getdata() {
        let mut mgr = InventoryManager::new();
        let hash = [0xab; 32];
        assert!(!mgr.has_pending_request(&hash));
        mgr.request_data(hash, MSG_BLOCK, "1.2.3.4:8333").unwrap();
        assert!(mgr.has_pending_request(&hash));
        mgr.mark_fulfilled(&hash);
        assert!(!mgr.has_pending_request(&hash));
    }
}
