//! Inventory management
//! 
//! Handles inventory tracking, peer inventory synchronization, and data requests.

use anyhow::Result;
use protocol_engine::Hash;
use std::collections::{HashMap, HashSet};
use tracing::{debug, info, warn};

use super::protocol::{InventoryItem, GetDataMessage};

/// Inventory types
pub const MSG_TX: u32 = 1;
pub const MSG_BLOCK: u32 = 2;
pub const MSG_FILTERED_BLOCK: u32 = 3;
pub const MSG_CMPCT_BLOCK: u32 = 4;

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
    fn default() -> Self { Self::new() }
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
    pub fn add_inventory(&mut self, peer: &str, inventory: &[InventoryItem]) -> Result<()> {
        let peer_inv = self.peer_inventories.entry(peer.to_string()).or_default();
        
        for item in inventory {
            peer_inv.insert(item.hash);
            self.known_inventory.insert(item.hash);
            
            debug!("Added inventory item {:?} from peer {}", item, peer);
        }
        
        Ok(())
    }
    
    /// Check if we have an inventory item
    pub fn has_inventory(&self, hash: &Hash) -> bool {
        self.known_inventory.contains(hash)
    }
    
    /// Request data for an inventory item
    pub fn request_data(&mut self, hash: Hash, inv_type: u32, peer: &str) -> Result<GetDataMessage> {
        let request = InventoryRequest {
            inv_type,
            hash,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            peer: peer.to_string(),
        };
        
        self.pending_requests.insert(hash, request.clone());
        
        let inventory = vec![InventoryItem {
            inv_type,
            hash,
        }];
        
        Ok(GetDataMessage { inventory })
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        let old_requests: Vec<Hash> = self.pending_requests
            .iter()
            .filter(|(_, request)| {
                let age = now - request.timestamp;
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
