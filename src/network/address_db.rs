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
use iroh::PublicKey;

/// Get current Unix timestamp in seconds
///
/// Helper function to avoid code duplication of time calculation.
fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| {
            tracing::warn!("System time error, using fallback timestamp");
            std::time::Duration::from_secs(0)
        })
        .as_secs()
}

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
        let now = current_timestamp();
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
        self.last_seen = current_timestamp();
        self.seen_count += 1;
    }

    /// Check if address is fresh (seen within expiration window)
    pub fn is_fresh(&self, expiration_seconds: u64) -> bool {
        let now = current_timestamp();
        now.saturating_sub(self.last_seen) < expiration_seconds
    }
}

/// Address database for peer discovery
pub struct AddressDatabase {
    /// Map from SocketAddr to AddressEntry (for TCP/Quinn)
    addresses: HashMap<SocketAddr, AddressEntry>,
    /// Map from Iroh PublicKey to AddressEntry (for Iroh peers)
    #[cfg(feature = "iroh")]
    iroh_addresses: HashMap<PublicKey, AddressEntry>,
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
                // Use total_count() to respect max_addresses across both maps
                if self.total_count() >= self.max_addresses {
                    self.evict_oldest_unified();
                }
                self.addresses
                    .insert(socket_addr, AddressEntry::new(addr, services));
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
        // Collect entries with their addresses, avoiding repeated conversions
        let mut fresh: Vec<_> = self
            .addresses
            .iter()
            .filter(|(_, entry)| entry.is_fresh(self.expiration_seconds))
            .map(|(_, entry)| (entry.last_seen, entry.addr.clone()))
            .collect();

        // Sort by last_seen in descending order (most recent first)
        // Use sort_by with reverse to avoid double sort
        fresh.sort_by(|a, b| b.0.cmp(&a.0));

        // Extract addresses and take requested count
        fresh
            .into_iter()
            .map(|(_, addr)| addr)
            .take(count)
            .collect()
    }

    /// Get all fresh addresses
    pub fn get_all_fresh_addresses(&self) -> Vec<NetworkAddress> {
        self.get_fresh_addresses(self.max_addresses)
    }

    /// Remove expired addresses
    pub fn remove_expired(&mut self) -> usize {
        let before = self.addresses.len();
        self.addresses
            .retain(|_, entry| entry.is_fresh(self.expiration_seconds));
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
            let now = current_timestamp();
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
                // Check for localhost, unspecified, unique local, link-local, and multicast
                // Note: is_unicast_link_local() is unstable, so we check manually
                // Link-local addresses are in fe80::/10 range (fe80:: to febf::)
                ipv6.is_loopback()
                    || ipv6.is_unspecified()
                    || (ipv6.octets()[0] == 0xfe && (ipv6.octets()[1] & 0xc0) == 0x80) // Link-local (fe80::/10)
                    || ipv6.octets()[0] == 0xfc || ipv6.octets()[0] == 0xfd // Unique local (fc00::/7)
                    || ipv6.octets()[0] == 0xff // Multicast (ff00::/8)
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

    /// Add an Iroh PublicKey to the database
    #[cfg(feature = "iroh")]
    pub fn add_iroh_address(&mut self, public_key: PublicKey, services: u64) {
        match self.iroh_addresses.get_mut(&public_key) {
            Some(entry) => {
                entry.update_seen();
                entry.services |= services;
            }
            None => {
                if self.total_count() >= self.max_addresses {
                    self.evict_oldest_unified();
                }
                // Create a placeholder NetworkAddress for Iroh (not used, just for consistency)
                let placeholder_addr = NetworkAddress {
                    services,
                    ip: [0; 16],
                    port: 0,
                };
                self.iroh_addresses
                    .insert(public_key, AddressEntry::new(placeholder_addr, services));
            }
        }
    }

    /// Get fresh Iroh PublicKeys
    #[cfg(feature = "iroh")]
    pub fn get_fresh_iroh_addresses(&self, count: usize) -> Vec<PublicKey> {
        // Collect entries with their node IDs, avoiding repeated map lookups
        let mut fresh: Vec<_> = self
            .iroh_addresses
            .iter()
            .filter(|(_, entry)| entry.is_fresh(self.expiration_seconds))
            .map(|(public_key, entry)| (entry.last_seen, *public_key))
            .collect();

        // Sort by last_seen in descending order (most recent first)
        fresh.sort_by(|a, b| b.0.cmp(&a.0));

        // Extract node IDs and take requested count
        fresh
            .into_iter()
            .map(|(_, public_key)| public_key)
            .take(count)
            .collect()
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

    /// Evict oldest address across both maps (unified eviction)
    ///
    /// This ensures we respect max_addresses as a total limit across both
    /// SocketAddr and Iroh address maps, not per-map limits.
    fn evict_oldest_unified(&mut self) {
        // Find oldest across both maps
        let mut oldest_socket: Option<(SocketAddr, u64)> = None;

        // Find oldest SocketAddr entry
        if let Some((addr, entry)) = self
            .addresses
            .iter()
            .min_by_key(|(_, entry)| entry.last_seen)
        {
            oldest_socket = Some((*addr, entry.last_seen));
        }

        #[cfg(feature = "iroh")]
        {
            // Find oldest Iroh entry
            let mut oldest_iroh: Option<(PublicKey, u64)> = None;
            if let Some((public_key, entry)) = self
                .iroh_addresses
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen)
            {
                oldest_iroh = Some((*public_key, entry.last_seen));
            }

            // Evict the oldest entry across both maps
            match (oldest_socket, oldest_iroh) {
                (Some((socket_addr, socket_time)), Some((iroh_id, iroh_time))) => {
                    // Both maps have entries - evict the oldest
                    if socket_time <= iroh_time {
                        self.addresses.remove(&socket_addr);
                    } else {
                        self.iroh_addresses.remove(&iroh_id);
                    }
                }
                (Some((socket_addr, _)), None) => {
                    // Only SocketAddr map has entries
                    self.addresses.remove(&socket_addr);
                }
                (None, Some((iroh_id, _))) => {
                    // Only Iroh map has entries
                    self.iroh_addresses.remove(&iroh_id);
                }
                (None, None) => {
                    // Both maps empty - nothing to evict
                }
            }
        }

        #[cfg(not(feature = "iroh"))]
        {
            // Only SocketAddr map exists
            if let Some((socket_addr, _)) = oldest_socket {
                self.addresses.remove(&socket_addr);
            }
        }
    }

    /// Evict oldest address (SocketAddr only)
    ///
    /// Deprecated: Use evict_oldest_unified() instead for proper cross-map eviction.
    #[deprecated(note = "Use evict_oldest_unified() instead")]
    fn evict_oldest(&mut self) {
        self.evict_oldest_unified();
    }

    /// Evict oldest Iroh address
    ///
    /// Deprecated: Use evict_oldest_unified() instead for proper cross-map eviction.
    #[cfg(feature = "iroh")]
    #[deprecated(note = "Use evict_oldest_unified() instead")]
    fn evict_oldest_iroh(&mut self) {
        self.evict_oldest_unified();
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
                addr.ip[12],
                addr.ip[13],
                addr.ip[14],
                addr.ip[15],
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
                segments[0],
                segments[1],
                segments[2],
                segments[3],
                segments[4],
                segments[5],
                segments[6],
                segments[7],
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
                // IPv4-mapped IPv6 format
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
    fn test_is_local_ipv6() {
        let db = AddressDatabase::new(100);

        // IPv6 localhost
        let ipv6_localhost = create_test_address("::1", 8333);
        assert!(db.is_local(&ipv6_localhost));

        // IPv6 unspecified
        let ipv6_unspecified = create_test_address("::", 8333);
        assert!(db.is_local(&ipv6_unspecified));

        // IPv6 unique local (fc00::/7)
        let ipv6_unique_local = create_test_address("fc00::1", 8333);
        assert!(db.is_local(&ipv6_unique_local));

        // IPv6 link-local (fe80::/10)
        let ipv6_link_local = create_test_address("fe80::1", 8333);
        assert!(db.is_local(&ipv6_link_local));

        // IPv6 multicast (ff00::/8)
        let ipv6_multicast = create_test_address("ff02::1", 8333);
        assert!(db.is_local(&ipv6_multicast));

        // IPv6 public address (should not be local)
        let ipv6_public = create_test_address("2001:4860:4860::8888", 8333);
        assert!(!db.is_local(&ipv6_public));
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
            .as_secs()
            + 3600;
        ban_list.insert(socket, future_time);
        assert!(db.is_banned(&addr, &ban_list));

        // Banned (expired)
        ban_list.clear();
        let past_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3600;
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
        use getrandom::getrandom;
        use iroh::SecretKey;
        let mut db = AddressDatabase::new(100);

        // Generate a valid Ed25519 key for testing using getrandom to avoid RNG version conflicts
        let mut bytes = [0u8; 32];
        getrandom(&mut bytes).unwrap();
        let secret_key = SecretKey::from_bytes(&bytes);
        let public_key = secret_key.public();

        db.add_iroh_address(public_key, 1);
        assert_eq!(db.total_count(), 1);

        // Add same address again (should update, not duplicate)
        db.add_iroh_address(public_key, 2);
        assert_eq!(db.total_count(), 1);
    }

    #[cfg(feature = "iroh")]
    #[test]
    fn test_get_fresh_iroh_addresses() {
        use getrandom::getrandom;
        use iroh::SecretKey;
        let mut db = AddressDatabase::new(100);

        // Generate valid Ed25519 keys for testing using getrandom to avoid RNG version conflicts
        let mut bytes1 = [0u8; 32];
        let mut bytes2 = [0u8; 32];
        getrandom(&mut bytes1).unwrap();
        getrandom(&mut bytes2).unwrap();
        let secret_key1 = SecretKey::from_bytes(&bytes1);
        let secret_key2 = SecretKey::from_bytes(&bytes2);
        let public_key1 = secret_key1.public();
        let public_key2 = secret_key2.public();

        db.add_iroh_address(public_key1, 1);
        db.add_iroh_address(public_key2, 1);

        let fresh = db.get_fresh_iroh_addresses(10);
        assert_eq!(fresh.len(), 2);
    }

    #[cfg(feature = "iroh")]
    #[test]
    fn test_total_count_includes_iroh() {
        use iroh::PublicKey;
        let mut db = AddressDatabase::new(100);

        // Add SocketAddr address
        let addr = create_test_address("192.168.1.1", 8333);
        db.add_address(addr, 1);
        assert_eq!(db.total_count(), 1);

        // Add Iroh address
        let key_bytes = [0u8; 32];
        let public_key = PublicKey::from_bytes(&key_bytes).unwrap();
        db.add_iroh_address(public_key, 1);
        assert_eq!(db.total_count(), 2);
    }
}
