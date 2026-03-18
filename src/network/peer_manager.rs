//! Peer management: PeerManager, PeerRateLimiter, PeerByteRateLimiter.
//!
//! Extracted from network/mod.rs for Phase 1 of complexity remediation.

/// Maximum number of outbound peers to protect from eviction/disconnect.
/// Core-style: keep best N peers when rotating outbound connections.
pub const MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT: usize = 4;

use crate::utils::current_timestamp;
use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;

use super::peer;
use super::transport::TransportAddr;

/// Peer manager for tracking connected peers
///
/// Uses TransportAddr as key to support all transport types (TCP, Quinn, Iroh).
/// This allows proper peer identification for Iroh (NodeId) while maintaining
/// compatibility with TCP/Quinn (SocketAddr).
pub struct PeerManager {
    peers: HashMap<TransportAddr, peer::Peer>,
    max_peers: usize,
}

impl PeerManager {
    pub fn new(max_peers: usize) -> Self {
        Self {
            peers: HashMap::new(),
            max_peers,
        }
    }

    pub fn add_peer(&mut self, addr: TransportAddr, peer: peer::Peer) -> Result<()> {
        if self.peers.len() >= self.max_peers {
            return Err(anyhow::anyhow!("Maximum peer limit reached"));
        }
        self.peers.insert(addr, peer);
        Ok(())
    }

    pub fn remove_peer(&mut self, addr: &TransportAddr) -> Option<peer::Peer> {
        self.peers.remove(addr)
    }

    pub fn get_peer(&self, addr: &TransportAddr) -> Option<&peer::Peer> {
        self.peers.get(addr)
    }

    pub fn get_peer_mut(&mut self, addr: &TransportAddr) -> Option<&mut peer::Peer> {
        self.peers.get_mut(addr)
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    pub fn peer_addresses(&self) -> Vec<TransportAddr> {
        self.peers.keys().cloned().collect()
    }

    /// Get peer addresses as SocketAddr (for backward compatibility)
    /// Only returns SocketAddr for TCP/Quinn peers, skips Iroh peers
    pub fn peer_socket_addresses(&self) -> Vec<SocketAddr> {
        self.peers
            .keys()
            .flat_map(|addr| {
                match addr {
                    TransportAddr::Tcp(sock) => Some(*sock),
                    #[cfg(feature = "quinn")]
                    TransportAddr::Quinn(sock) => Some(*sock),
                    #[cfg(feature = "iroh")]
                    TransportAddr::Iroh(_) => None, // Iroh peers don't have SocketAddr
                }
            })
            .collect()
    }

    /// Get governance-enabled peers (peers with NODE_GOVERNANCE service flag)
    #[cfg(feature = "governance")]
    pub fn get_governance_peers(&self) -> Vec<(TransportAddr, SocketAddr)> {
        use crate::network::protocol::NODE_GOVERNANCE;
        self.peers
            .iter()
            .filter_map(|(transport_addr, peer)| {
                if peer.has_service(NODE_GOVERNANCE) {
                    match transport_addr {
                        TransportAddr::Tcp(sock) => Some((transport_addr.clone(), *sock)),
                        #[cfg(feature = "quinn")]
                        TransportAddr::Quinn(sock) => Some((transport_addr.clone(), *sock)),
                        #[cfg(feature = "iroh")]
                        TransportAddr::Iroh(_) => None, // Iroh peers don't have SocketAddr for now
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn can_accept_peer(&self) -> bool {
        self.peers.len() < self.max_peers
    }

    pub fn max_peers(&self) -> usize {
        self.max_peers
    }

    /// Internal access for iteration (used by NetworkManager)
    pub(crate) fn peers(&self) -> &HashMap<TransportAddr, peer::Peer> {
        &self.peers
    }

    /// Internal access for mutable iteration (used by NetworkManager)
    pub(crate) fn peers_mut(&mut self) -> &mut HashMap<TransportAddr, peer::Peer> {
        &mut self.peers
    }

    /// Select best peers based on quality score
    ///
    /// Returns peers sorted by quality score (highest first)
    pub fn select_best_peers(&self, count: usize) -> Vec<TransportAddr> {
        let mut peers: Vec<_> = self
            .peers
            .iter()
            .map(|(addr, peer)| (addr.clone(), peer.quality_score()))
            .collect();

        // Sort by quality score (descending)
        peers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Return top N addresses
        peers
            .into_iter()
            .take(count)
            .map(|(addr, _)| addr)
            .collect()
    }

    /// Select reliable peers only
    ///
    /// Returns peers that meet reliability criteria
    pub fn select_reliable_peers(&self) -> Vec<TransportAddr> {
        self.peers
            .iter()
            .filter(|(_, peer)| peer.quality_score() > 0.5) // Use quality_score as reliability indicator
            .map(|(addr, _)| addr.clone())
            .collect()
    }

    /// Get peer quality statistics
    pub fn get_quality_stats(&self) -> (usize, usize, f64) {
        let total = self.peers.len();
        let reliable = self
            .peers
            .values()
            .filter(|peer| peer.quality_score() > 0.5) // Use quality_score as reliability indicator
            .count();
        let avg_quality = if total > 0 {
            self.peers
                .values()
                .map(|peer| peer.quality_score())
                .sum::<f64>()
                / total as f64
        } else {
            0.0
        };

        (total, reliable, avg_quality)
    }

    /// Find peer by SocketAddr (tries TCP and Quinn variants)
    /// Returns the TransportAddr if found
    pub fn find_transport_addr_by_socket(&self, addr: SocketAddr) -> Option<TransportAddr> {
        // Try TCP first
        let tcp_addr = TransportAddr::Tcp(addr);
        if self.peers.contains_key(&tcp_addr) {
            return Some(tcp_addr);
        }

        // Try Quinn
        #[cfg(feature = "quinn")]
        {
            let quinn_addr = TransportAddr::Quinn(addr);
            if self.peers.contains_key(&quinn_addr) {
                return Some(quinn_addr);
            }
        }

        None
    }
}

/// Token bucket rate limiter for peer message rate limiting
pub struct PeerRateLimiter {
    /// Current number of tokens available
    tokens: u32,
    /// Maximum burst size (initial token count)
    burst_limit: u32,
    /// Tokens per second refill rate
    rate: u32,
    /// Last refill timestamp (Unix seconds)
    last_refill: u64,
}

impl PeerRateLimiter {
    /// Create a new rate limiter
    pub fn new(burst_limit: u32, rate: u32) -> Self {
        let now = current_timestamp();
        Self {
            tokens: burst_limit,
            burst_limit,
            rate,
            last_refill: now,
        }
    }

    /// Check if a message can be consumed and consume a token
    pub fn check_and_consume(&mut self) -> bool {
        self.refill();
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }

    /// Refill tokens based on elapsed time
    fn refill(&mut self) {
        let now = current_timestamp();

        if now > self.last_refill {
            let elapsed = now - self.last_refill;
            let tokens_to_add = (elapsed as u32) * self.rate;
            self.tokens = self
                .tokens
                .saturating_add(tokens_to_add)
                .min(self.burst_limit);
            self.last_refill = now;
        }
    }
}

/// Byte rate limiter for peer transaction byte rate limiting
pub struct PeerByteRateLimiter {
    /// Current number of bytes available
    bytes: u64,
    /// Maximum burst size (initial byte count)
    burst_limit: u64,
    /// Bytes per second refill rate
    rate: u64,
    /// Last refill timestamp (Unix seconds)
    last_refill: u64,
}

impl PeerByteRateLimiter {
    /// Create a new byte rate limiter
    pub fn new(burst_limit: u64, rate: u64) -> Self {
        let now = current_timestamp();
        Self {
            bytes: burst_limit,
            burst_limit,
            rate,
            last_refill: now,
        }
    }

    /// Check if bytes can be consumed and consume them
    pub fn check_and_consume(&mut self, bytes: u64) -> bool {
        self.refill();
        if self.bytes >= bytes {
            self.bytes -= bytes;
            true
        } else {
            false
        }
    }

    /// Refill bytes based on elapsed time
    fn refill(&mut self) {
        let now = current_timestamp();

        if now > self.last_refill {
            let elapsed = now - self.last_refill;
            let bytes_to_add = elapsed * self.rate;
            self.bytes = self
                .bytes
                .saturating_add(bytes_to_add)
                .min(self.burst_limit);
            self.last_refill = now;
        }
    }
}
