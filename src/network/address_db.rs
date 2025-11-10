//! Address Database for Peer Discovery
//!
//! Manages a database of known peer addresses with freshness tracking,
//! expiration, and filtering capabilities.
//!
//! Supports both SocketAddr-based addresses (TCP/Quinn) and Iroh NodeIds.

use crate::network::protocol::NetworkAddress;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(feature = "iroh")]
use iroh_net::NodeId;

/// Address entry with metadata
#[derive(Debug, Clone)]
pub struct AddressEntry {
    /// Network address
    pub addr: NetworkAddress,
    /// Unix timestamp when address was first seen
    pub first_seen: u64,
    /// Unix timestamp when address was last seen
    pub last_seen: u64,
    /// Service flags from version message
    pub services: u64,
    /// Number of times we've seen this address
    pub seen_count: u32,
}

impl AddressEntry {
    /// Create a new address entry
    pub fn new(addr: NetworkAddress, services: u64) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self {
            addr,
            first_seen: now,
            last_seen: now,
            services,
            seen_count: 1,
        }
    }

    /// Update last seen timestamp
    pub fn update_seen(&mut self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.last_seen = now;
        self.seen_count += 1;
    }

    /// Check if address is fresh (seen within expiration window)
    pub fn is_fresh(&self, expiration_seconds: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        now.saturating_sub(self.last_seen) < expiration_seconds
    }
}

/// Address database for peer discovery
pub struct AddressDatabase {
    /// Map from SocketAddr to AddressEntry (for TCP/Quinn)
    addresses: HashMap<SocketAddr, AddressEntry>,
    /// Map from Iroh NodeId to AddressEntry (for Iroh peers)
    #[cfg(feature = "iroh")]
    iroh_addresses: HashMap<NodeId, AddressEntry>,
    /// Maximum number of addresses to store (total across both maps)
    max_addresses: usize,
    /// Address expiration time in seconds (default: 24 hours)
    expiration_seconds: u64,
}

impl AddressDatabase {
    /// Create a new address database
    pub fn new(max_addresses: usize) -> Self {
        Self {
            addresses: HashMap::new(),
            #[cfg(feature = "iroh")]
            iroh_addresses: HashMap::new(),
            max_addresses,
            expiration_seconds: 24 * 60 * 60, // 24 hours default
        }
    }

    /// Create with custom expiration
    pub fn with_expiration(max_addresses: usize, expiration_seconds: u64) -> Self {
        Self {
            addresses: HashMap::new(),
            #[cfg(feature = "iroh")]
            iroh_addresses: HashMap::new(),
            max_addresses,
            expiration_seconds,
        }
    }

    /// Add or update an address
    pub fn add_address(&mut self, addr: NetworkAddress, services: u64) {
        // Convert NetworkAddress to SocketAddr for key
        let socket_addr = self.network_addr_to_socket(&addr);
        
        match self.addresses.get_mut(&socket_addr) {
            Some(entry) => {
                // Update existing entry
                entry.update_seen();
                entry.services |= services; // Merge service flags
            }
            None => {
                // Add new entry (evict old if needed)
                if self.addresses.len() >= self.max_addresses {
                    self.evict_oldest();
                }
                self.addresses.insert(socket_addr, AddressEntry::new(addr, services));
            }
        }
    }

    /// Add multiple addresses
    pub fn add_addresses(&mut self, addresses: Vec<NetworkAddress>, services: u64) {
        for addr in addresses {
            self.add_address(addr, services);
        }
    }

    /// Get fresh addresses (not expired)
    pub fn get_fresh_addresses(&self, count: usize) -> Vec<NetworkAddress> {
        let mut fresh: Vec<_> = self
            .addresses
            .values()
            .filter(|entry| entry.is_fresh(self.expiration_seconds))
            .map(|entry| entry.addr.clone())
            .collect();
        
        // Sort by last_seen (most recent first)
        fresh.sort_by_key(|addr| {
            let socket = self.network_addr_to_socket(addr);
            self.addresses
                .get(&socket)
                .map(|e| e.last_seen)
                .unwrap_or(0)
        });
        fresh.reverse();
        
        fresh.into_iter().take(count).collect()
    }

    /// Get all fresh addresses
    pub fn get_all_fresh_addresses(&self) -> Vec<NetworkAddress> {
        self.get_fresh_addresses(self.max_addresses)
    }

    /// Remove expired addresses
    pub fn remove_expired(&mut self) -> usize {
        let before = self.addresses.len();
        self.addresses.retain(|_, entry| entry.is_fresh(self.expiration_seconds));
        before - self.addresses.len()
    }

    /// Remove an address
    pub fn remove_address(&mut self, addr: &NetworkAddress) {
        let socket_addr = self.network_addr_to_socket(addr);
        self.addresses.remove(&socket_addr);
    }

    /// Check if address is banned
    pub fn is_banned(&self, addr: &NetworkAddress, ban_list: &HashMap<SocketAddr, u64>) -> bool {
        let socket_addr = self.network_addr_to_socket(addr);
        if let Some(unban_timestamp) = ban_list.get(&socket_addr) {
            use std::time::{SystemTime, UNIX_EPOCH};
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            // Check if ban has expired
            if *unban_timestamp == u64::MAX || now < *unban_timestamp {
                return true; // Still banned
            }
        }
        false
    }

    /// Check if address is local/private
    pub fn is_local(&self, addr: &NetworkAddress) -> bool {
        let socket = self.network_addr_to_socket(addr);
        match socket.ip() {
            IpAddr::V4(ipv4) => {
                // Check for localhost, private ranges
                ipv4.is_loopback()
                    || ipv4.is_private()
                    || ipv4.is_link_local()
                    || ipv4.is_broadcast()
            }
            IpAddr::V6(ipv6) => {
                ipv6.is_loopback() || ipv6.is_unspecified()
            }
        }
    }

    /// Filter addresses (exclude local, banned, already connected)
    pub fn filter_addresses(
        &self,
        addresses: Vec<NetworkAddress>,
        ban_list: &HashMap<SocketAddr, u64>,
        connected_peers: &[SocketAddr],
    ) -> Vec<NetworkAddress> {
        addresses
            .into_iter()
            .filter(|addr| {
                let socket = self.network_addr_to_socket(addr);
                !self.is_local(addr)
                    && !self.is_banned(addr, ban_list)
                    && !connected_peers.contains(&socket)
            })
            .collect()
    }

    /// Add an Iroh NodeId to the database
    #[cfg(feature = "iroh")]
    pub fn add_iroh_address(&mut self, node_id: NodeId, services: u64) {
        match self.iroh_addresses.get_mut(&node_id) {
            Some(entry) => {
                entry.update_seen();
                entry.services |= services;
            }
            None => {
                if self.total_count() >= self.max_addresses {
                    self.evict_oldest_iroh();
                }
                // Create a placeholder NetworkAddress for Iroh (not used, just for consistency)
                let placeholder_addr = NetworkAddress {
                    services,
                    ip: [0; 16],
                    port: 0,
                };
                self.iroh_addresses.insert(node_id, AddressEntry::new(placeholder_addr, services));
            }
        }
    }

    /// Get fresh Iroh NodeIds
    #[cfg(feature = "iroh")]
    pub fn get_fresh_iroh_addresses(&self, count: usize) -> Vec<NodeId> {
        let mut fresh: Vec<_> = self
            .iroh_addresses
            .iter()
            .filter(|(_, entry)| entry.is_fresh(self.expiration_seconds))
            .map(|(node_id, _)| *node_id)
            .collect();
        
        // Sort by last_seen (most recent first)
        fresh.sort_by_key(|node_id| {
            self.iroh_addresses
                .get(node_id)
                .map(|e| e.last_seen)
                .unwrap_or(0)
        });
        fresh.reverse();
        
        fresh.into_iter().take(count).collect()
    }

    /// Get total address count (SocketAddr + Iroh)
    pub fn total_count(&self) -> usize {
        let socket_count = self.addresses.len();
        #[cfg(feature = "iroh")]
        {
            socket_count + self.iroh_addresses.len()
        }
        #[cfg(not(feature = "iroh"))]
        {
            socket_count
        }
    }

    /// Get address count (SocketAddr only, for backward compatibility)
    pub fn len(&self) -> usize {
        self.addresses.len()
    }

    /// Check if database is empty
    pub fn is_empty(&self) -> bool {
        #[cfg(feature = "iroh")]
        {
            self.addresses.is_empty() && self.iroh_addresses.is_empty()
        }
        #[cfg(not(feature = "iroh"))]
        {
            self.addresses.is_empty()
        }
    }

    /// Evict oldest address (SocketAddr)
    fn evict_oldest(&mut self) {
        // Find the oldest address first, then remove it
        let oldest_addr = self
            .addresses
            .iter()
            .min_by_key(|(_, entry)| entry.last_seen)
            .map(|(addr, _)| addr.clone());
        if let Some(addr) = oldest_addr {
            self.addresses.remove(&addr);
        }
    }

    /// Evict oldest Iroh address
    #[cfg(feature = "iroh")]
    fn evict_oldest_iroh(&mut self) {
        if let Some((oldest_node_id, _)) = self
            .iroh_addresses
            .iter()
            .min_by_key(|(_, entry)| entry.last_seen)
        {
            self.iroh_addresses.remove(oldest_node_id);
        }
    }

    /// Convert NetworkAddress to SocketAddr
    /// 
    /// Note: This is public for use in NetworkManager when connecting to peers.
    pub fn network_addr_to_socket(&self, addr: &NetworkAddress) -> SocketAddr {
        // Convert IPv6 address bytes to SocketAddr
        // NetworkAddress uses 16-byte IPv6 format
        let ip = if addr.ip[0..12] == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff] {
            // IPv4-mapped IPv6 address
            IpAddr::V4(std::net::Ipv4Addr::new(
                addr.ip[12], addr.ip[13], addr.ip[14], addr.ip[15],
            ))
        } else {
            // Native IPv6
            let segments = [
                u16::from_be_bytes([addr.ip[0], addr.ip[1]]),
                u16::from_be_bytes([addr.ip[2], addr.ip[3]]),
                u16::from_be_bytes([addr.ip[4], addr.ip[5]]),
                u16::from_be_bytes([addr.ip[6], addr.ip[7]]),
                u16::from_be_bytes([addr.ip[8], addr.ip[9]]),
                u16::from_be_bytes([addr.ip[10], addr.ip[11]]),
                u16::from_be_bytes([addr.ip[12], addr.ip[13]]),
                u16::from_be_bytes([addr.ip[14], addr.ip[15]]),
            ];
            IpAddr::V6(std::net::Ipv6Addr::new(
                segments[0], segments[1], segments[2], segments[3],
                segments[4], segments[5], segments[6], segments[7],
            ))
        };
        SocketAddr::new(ip, addr.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn create_test_address(ip: &str, port: u16) -> NetworkAddress {
        let socket = SocketAddr::new(ip.parse().unwrap(), port);
        let ip_bytes = match socket.ip() {
            IpAddr::V4(ipv4) => {
                let mut bytes = [0u8; 16];
                bytes[10] = 0xff;
                bytes[11] = 0xff;
                bytes[12..16].copy_from_slice(&ipv4.octets());
                bytes
            }
            IpAddr::V6(ipv6) => ipv6.octets(),
        };
        NetworkAddress {
            services: 0,
            ip: ip_bytes,
            port,
        }
    }

    #[test]
    fn test_address_database_creation() {
        let db = AddressDatabase::new(100);
        assert_eq!(db.len(), 0);
        assert!(db.is_empty());
    }

    #[test]
    fn test_add_address() {
        let mut db = AddressDatabase::new(100);
        let addr = create_test_address("192.168.1.1", 8333);
        db.add_address(addr.clone(), 1);
        assert_eq!(db.len(), 1);
        assert!(!db.is_empty());
    }

    #[test]
    fn test_add_duplicate_address() {
        let mut db = AddressDatabase::new(100);
        let addr = create_test_address("192.168.1.1", 8333);
        db.add_address(addr.clone(), 1);
        db.add_address(addr.clone(), 2);
        assert_eq!(db.len(), 1); // Should still be 1
    }

    #[test]
    fn test_get_fresh_addresses() {
        let mut db = AddressDatabase::new(100);
        let addr1 = create_test_address("192.168.1.1", 8333);
        let addr2 = create_test_address("192.168.1.2", 8333);
        db.add_address(addr1.clone(), 1);
        db.add_address(addr2.clone(), 1);
        
        let fresh = db.get_fresh_addresses(10);
        assert_eq!(fresh.len(), 2);
    }

    #[test]
    fn test_address_expiration() {
        let mut db = AddressDatabase::with_expiration(100, 1); // 1 second expiration
        let addr = create_test_address("192.168.1.1", 8333);
        db.add_address(addr.clone(), 1);
        assert_eq!(db.len(), 1);
        
        // Wait for expiration
        std::thread::sleep(std::time::Duration::from_secs(2));
        
        let fresh = db.get_fresh_addresses(10);
        assert_eq!(fresh.len(), 0); // Should be expired
    }

    #[test]
    fn test_remove_expired() {
        let mut db = AddressDatabase::with_expiration(100, 1); // 1 second expiration
        let addr1 = create_test_address("192.168.1.1", 8333);
        let addr2 = create_test_address("192.168.1.2", 8333);
        db.add_address(addr1.clone(), 1);
        db.add_address(addr2.clone(), 1);
        assert_eq!(db.len(), 2);
        
        // Wait for expiration
        std::thread::sleep(std::time::Duration::from_secs(2));
        
        let removed = db.remove_expired();
        assert_eq!(removed, 2);
        assert_eq!(db.len(), 0);
    }

    #[test]
    fn test_is_local() {
        let db = AddressDatabase::new(100);
        let localhost = create_test_address("127.0.0.1", 8333);
        let private = create_test_address("192.168.1.1", 8333);
        let public = create_test_address("8.8.8.8", 8333);
        
        assert!(db.is_local(&localhost));
        assert!(db.is_local(&private));
        assert!(!db.is_local(&public));
    }

    #[test]
    fn test_is_banned() {
        let db = AddressDatabase::new(100);
        let addr = create_test_address("192.168.1.1", 8333);
        let socket = SocketAddr::new("192.168.1.1".parse().unwrap(), 8333);
        let mut ban_list = HashMap::new();
        
        // Not banned
        assert!(!db.is_banned(&addr, &ban_list));
        
        // Banned (permanent)
        ban_list.insert(socket, u64::MAX);
        assert!(db.is_banned(&addr, &ban_list));
        
        // Banned (temporary, not expired)
        ban_list.clear();
        let future_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() + 3600;
        ban_list.insert(socket, future_time);
        assert!(db.is_banned(&addr, &ban_list));
        
        // Banned (expired)
        ban_list.clear();
        let past_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() - 3600;
        ban_list.insert(socket, past_time);
        assert!(!db.is_banned(&addr, &ban_list));
    }

    #[test]
    fn test_filter_addresses() {
        let db = AddressDatabase::new(100);
        let local = create_test_address("127.0.0.1", 8333);
        let banned = create_test_address("192.168.1.1", 8333);
        let public = create_test_address("8.8.8.8", 8333);
        
        let socket_banned = SocketAddr::new("192.168.1.1".parse().unwrap(), 8333);
        let socket_connected = SocketAddr::new("8.8.8.8".parse().unwrap(), 8333);
        let mut ban_list = HashMap::new();
        ban_list.insert(socket_banned, u64::MAX);
        let connected_peers = vec![socket_connected];
        
        let addresses = vec![local, banned, public];
        let filtered = db.filter_addresses(addresses, &ban_list, &connected_peers);
        
        // Should filter out local, banned, and connected
        assert_eq!(filtered.len(), 0);
    }

    #[test]
    fn test_eviction_when_full() {
        let mut db = AddressDatabase::new(2); // Small capacity
        let addr1 = create_test_address("192.168.1.1", 8333);
        let addr2 = create_test_address("192.168.1.2", 8333);
        let addr3 = create_test_address("192.168.1.3", 8333);
        
        db.add_address(addr1.clone(), 1);
        db.add_address(addr2.clone(), 1);
        assert_eq!(db.len(), 2);
        
        // Adding third should evict oldest
        db.add_address(addr3.clone(), 1);
        assert_eq!(db.len(), 2); // Should still be 2
    }

    #[cfg(feature = "iroh")]
    #[test]
    fn test_add_iroh_address() {
        use iroh_net::NodeId;
        let mut db = AddressDatabase::new(100);
        
        // Create a test NodeId (32 bytes)
        let node_id_bytes = [0u8; 32];
        let node_id = NodeId::from_bytes(&node_id_bytes).unwrap();
        
        db.add_iroh_address(node_id, 1);
        assert_eq!(db.total_count(), 1);
        
        // Add same address again (should update, not duplicate)
        db.add_iroh_address(node_id, 2);
        assert_eq!(db.total_count(), 1);
    }

    #[cfg(feature = "iroh")]
    #[test]
    fn test_get_fresh_iroh_addresses() {
        use iroh_net::NodeId;
        let mut db = AddressDatabase::new(100);
        
        let node_id1_bytes = [1u8; 32];
        let node_id2_bytes = [2u8; 32];
        let node_id1 = NodeId::from_bytes(&node_id1_bytes).unwrap();
        let node_id2 = NodeId::from_bytes(&node_id2_bytes).unwrap();
        
        db.add_iroh_address(node_id1, 1);
        db.add_iroh_address(node_id2, 1);
        
        let fresh = db.get_fresh_iroh_addresses(10);
        assert_eq!(fresh.len(), 2);
    }

    #[cfg(feature = "iroh")]
    #[test]
    fn test_total_count_includes_iroh() {
        use iroh_net::NodeId;
        let mut db = AddressDatabase::new(100);
        
        // Add SocketAddr address
        let addr = create_test_address("192.168.1.1", 8333);
        db.add_address(addr, 1);
        assert_eq!(db.total_count(), 1);
        
        // Add Iroh address
        let node_id_bytes = [0u8; 32];
        let node_id = NodeId::from_bytes(&node_id_bytes).unwrap();
        db.add_iroh_address(node_id, 1);
        assert_eq!(db.total_count(), 2);
    }
}
