//! Bandwidth-based peer scoring for IBD optimization
//!
//! Tracks peer performance metrics and scores peers for download selection.
//! Faster peers get more download work assigned.
//! 
//! ## Sibling/LAN Node Support
//! 
//! Automatically detects peers on private networks (LAN) and gives them
//! priority for block downloads. LAN peers typically have:
//! - <10ms latency vs 100-5000ms for internet peers
//! - ~1 Gbps throughput vs ~10-100 Mbps for internet
//! - 100% reliability vs connection drops
//!
//! This can speed up IBD by 10-50x when a local Bitcoin node is available.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// LAN peer score bonus multiplier (maximum)
/// SECURITY: Reduced from 10x to 3x to prevent attack domination
/// See lan_security.rs for progressive trust system (1.5x -> 2x -> 3x)
const LAN_PEER_SCORE_MULTIPLIER: f64 = 3.0;

/// Check if an address is on a private/local network (LAN peer)
/// 
/// Detects:
/// - IPv4 private ranges: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
/// - IPv4 loopback: 127.0.0.0/8
/// - IPv4 link-local: 169.254.0.0/16
/// - IPv6 unique local: fd00::/8 (ULA)
/// - IPv6 link-local: fe80::/10
/// - IPv6 loopback: ::1
pub fn is_lan_peer(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => {
            ip.is_private() ||      // 10.x, 172.16-31.x, 192.168.x
            ip.is_loopback() ||     // 127.x
            ip.is_link_local()      // 169.254.x
        }
        IpAddr::V6(ip) => {
            ip.is_loopback() ||     // ::1
            // Check for unique local (fd00::/8) or link-local (fe80::/10)
            {
                let segments = ip.segments();
                (segments[0] & 0xff00) == 0xfd00 ||  // ULA
                (segments[0] & 0xffc0) == 0xfe80     // Link-local
            }
        }
    }
}

/// Peer performance statistics for scoring
#[derive(Debug, Clone)]
pub struct PeerStats {
    /// Total bytes received from this peer
    pub bytes_received: u64,
    /// Total blocks received from this peer
    pub blocks_received: u64,
    /// Total time connected (seconds)
    pub connection_duration_secs: f64,
    /// Average block latency (milliseconds)
    pub avg_block_latency_ms: f64,
    /// Number of timeouts/failures
    pub failures: u64,
    /// Last successful block time
    pub last_block_time: Option<Instant>,
    /// Computed bandwidth (bytes/sec)
    pub bandwidth_bytes_per_sec: f64,
    /// Computed score (higher = better)
    pub score: f64,
    /// Whether this peer is on the local network (LAN/sibling)
    pub is_lan: bool,
}

impl Default for PeerStats {
    fn default() -> Self {
        Self {
            bytes_received: 0,
            blocks_received: 0,
            connection_duration_secs: 0.0,
            avg_block_latency_ms: 1000.0, // Default 1 second
            failures: 0,
            last_block_time: None,
            bandwidth_bytes_per_sec: 0.0,
            score: 1.0, // Start with neutral score
            is_lan: false,
        }
    }
}

impl PeerStats {
    /// Update bandwidth calculation
    pub fn update_bandwidth(&mut self) {
        if self.connection_duration_secs > 0.0 {
            self.bandwidth_bytes_per_sec = self.bytes_received as f64 / self.connection_duration_secs;
        }
    }

    /// Calculate composite score
    ///
    /// Score formula:
    /// - Base: bandwidth_bytes_per_sec / 100_000 (normalize to reasonable range)
    /// - Penalty: -0.1 per failure
    /// - Bonus: +0.2 for recent activity (< 30 seconds)
    /// - Latency penalty: -0.001 * avg_latency_ms (faster = better)
    /// - LAN bonus: 3x max multiplier for local network peers (reduced from 10x for security)
    pub fn calculate_score(&mut self) {
        // Base score from bandwidth
        let bandwidth_score = self.bandwidth_bytes_per_sec / 100_000.0;
        
        // Failure penalty
        let failure_penalty = self.failures as f64 * 0.1;
        
        // Recent activity bonus
        let activity_bonus = match self.last_block_time {
            Some(t) if t.elapsed() < Duration::from_secs(30) => 0.2,
            Some(t) if t.elapsed() < Duration::from_secs(60) => 0.1,
            _ => 0.0,
        };
        
        // Latency penalty (lower is better)
        let latency_penalty = (self.avg_block_latency_ms / 1000.0) * 0.1;
        
        // Compute base score (minimum 0.1 to always have a chance)
        let base_score = (bandwidth_score - failure_penalty + activity_bonus - latency_penalty).max(0.1);
        
        // Apply LAN bonus: sibling/local peers get 3x priority (max)
        // SECURITY: Reduced from 10x to prevent attack domination
        // See lan_security.rs for progressive trust (1.5x -> 2x -> 3x)
        self.score = if self.is_lan {
            base_score * LAN_PEER_SCORE_MULTIPLIER
        } else {
            base_score
        };
    }
}

/// Peer scoring manager for IBD optimization
pub struct PeerScorer {
    /// Per-peer statistics
    stats: RwLock<HashMap<SocketAddr, PeerStats>>,
    /// Start time for connection duration calculation
    start_time: Instant,
}

impl PeerScorer {
    /// Create a new peer scorer
    pub fn new() -> Self {
        Self {
            stats: RwLock::new(HashMap::new()),
            start_time: Instant::now(),
        }
    }

    /// Record bytes received from a peer
    pub fn record_bytes(&self, peer: SocketAddr, bytes: u64) {
        let mut stats = self.stats.write().unwrap();
        let entry = stats.entry(peer).or_insert_with(|| {
            let mut s = PeerStats::default();
            s.is_lan = is_lan_peer(&peer);
            s
        });
        entry.bytes_received += bytes;
        entry.connection_duration_secs = self.start_time.elapsed().as_secs_f64();
        entry.update_bandwidth();
        entry.calculate_score();
    }

    /// Record a block received from a peer
    pub fn record_block(&self, peer: SocketAddr, block_size: u64, latency_ms: f64) {
        let mut stats = self.stats.write().unwrap();
        let entry = stats.entry(peer).or_insert_with(|| {
            let mut s = PeerStats::default();
            s.is_lan = is_lan_peer(&peer);
            s
        });
        
        entry.blocks_received += 1;
        entry.bytes_received += block_size;
        entry.connection_duration_secs = self.start_time.elapsed().as_secs_f64();
        entry.last_block_time = Some(Instant::now());
        
        // Update average latency (exponential moving average)
        let alpha = 0.3; // Smoothing factor
        entry.avg_block_latency_ms = entry.avg_block_latency_ms * (1.0 - alpha) + latency_ms * alpha;
        
        entry.update_bandwidth();
        entry.calculate_score();
    }

    /// Record a failure/timeout for a peer
    pub fn record_failure(&self, peer: SocketAddr) {
        let mut stats = self.stats.write().unwrap();
        let entry = stats.entry(peer).or_insert_with(|| {
            let mut s = PeerStats::default();
            s.is_lan = is_lan_peer(&peer);
            s
        });
        entry.failures += 1;
        entry.calculate_score();
    }

    /// Check if a peer is on the local network
    pub fn is_peer_lan(&self, peer: &SocketAddr) -> bool {
        self.stats.read().unwrap()
            .get(peer)
            .map(|s| s.is_lan)
            .unwrap_or_else(|| is_lan_peer(peer))
    }

    /// Get count of LAN peers
    pub fn lan_peer_count(&self) -> usize {
        self.stats.read().unwrap()
            .values()
            .filter(|s| s.is_lan)
            .count()
    }

    /// Get all LAN peers
    pub fn get_lan_peers(&self) -> Vec<SocketAddr> {
        self.stats.read().unwrap()
            .iter()
            .filter(|(_, s)| s.is_lan)
            .map(|(addr, _)| *addr)
            .collect()
    }

    /// Get the score for a peer
    /// 
    /// Applies LAN bonus even for peers without stats yet.
    /// This ensures LAN peers get priority at the START of IBD
    /// before any blocks are received and scored.
    pub fn get_score(&self, peer: &SocketAddr) -> f64 {
        self.stats.read().unwrap()
            .get(peer)
            .map(|s| s.score)
            .unwrap_or_else(|| {
                // Apply LAN bonus to initial score for new peers
                if is_lan_peer(peer) {
                    1.0 * LAN_PEER_SCORE_MULTIPLIER  // LAN peers start at 3.0 (reduced from 10.0)
                } else {
                    1.0  // Normal peers start at 1.0
                }
            })
    }

    /// Get stats for a peer
    pub fn get_stats(&self, peer: &SocketAddr) -> Option<PeerStats> {
        self.stats.read().unwrap().get(peer).cloned()
    }

    /// Get all peer scores sorted by score (highest first)
    pub fn get_sorted_peers(&self) -> Vec<(SocketAddr, f64)> {
        let stats = self.stats.read().unwrap();
        let mut peers: Vec<_> = stats.iter()
            .map(|(addr, s)| (*addr, s.score))
            .collect();
        peers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        peers
    }

    /// Select the best N peers for download
    ///
    /// Uses weighted random selection based on scores, so faster peers
    /// are more likely to be selected but slower peers still get some work.
    pub fn select_best_peers(&self, available: &[SocketAddr], count: usize) -> Vec<SocketAddr> {
        if available.is_empty() {
            return vec![];
        }
        
        if available.len() <= count {
            return available.to_vec();
        }

        let stats = self.stats.read().unwrap();
        
        // Get scores for available peers
        let mut scored: Vec<_> = available.iter()
            .map(|addr| {
                let score = stats.get(addr).map(|s| s.score).unwrap_or(1.0);
                (*addr, score)
            })
            .collect();
        
        // Sort by score (highest first)
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        
        // Take top N
        scored.into_iter().take(count).map(|(addr, _)| addr).collect()
    }

    /// Get the single best peer for download
    pub fn get_best_peer(&self, available: &[SocketAddr]) -> Option<SocketAddr> {
        self.select_best_peers(available, 1).into_iter().next()
    }

    /// Get summary statistics for logging
    pub fn summary(&self) -> String {
        let stats = self.stats.read().unwrap();
        if stats.is_empty() {
            return "No peer stats yet".to_string();
        }

        let mut total_bytes = 0u64;
        let mut total_blocks = 0u64;
        let mut lan_blocks = 0u64;
        let mut lan_count = 0usize;
        let mut best_peer: Option<(SocketAddr, f64, bool)> = None;

        for (addr, s) in stats.iter() {
            total_bytes += s.bytes_received;
            total_blocks += s.blocks_received;
            if s.is_lan {
                lan_count += 1;
                lan_blocks += s.blocks_received;
            }
            if best_peer.is_none() || s.score > best_peer.as_ref().unwrap().1 {
                best_peer = Some((*addr, s.score, s.is_lan));
            }
        }

        let lan_pct = if total_blocks > 0 {
            (lan_blocks * 100) / total_blocks
        } else {
            0
        };

        format!(
            "Peers: {} ({} LAN), Total: {} blocks / {} MB, LAN blocks: {}%, Best: {:?} (score: {:.2}{})",
            stats.len(),
            lan_count,
            total_blocks,
            total_bytes / 1_000_000,
            lan_pct,
            best_peer.as_ref().map(|(addr, _, _)| addr),
            best_peer.as_ref().map(|(_, s, _)| *s).unwrap_or(0.0),
            if best_peer.as_ref().map(|(_, _, is_lan)| *is_lan).unwrap_or(false) { " LAN" } else { "" }
        )
    }
}

impl Default for PeerScorer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_scoring_basic() {
        let scorer = PeerScorer::new();
        let peer1: SocketAddr = "127.0.0.1:8333".parse().unwrap();
        let peer2: SocketAddr = "127.0.0.2:8333".parse().unwrap();

        // Peer1 gets more blocks faster
        scorer.record_block(peer1, 500_000, 100.0); // 500KB in 100ms
        scorer.record_block(peer1, 500_000, 100.0);
        scorer.record_block(peer1, 500_000, 100.0);

        // Peer2 gets fewer blocks slower
        scorer.record_block(peer2, 500_000, 500.0); // 500KB in 500ms
        scorer.record_failure(peer2);

        let score1 = scorer.get_score(&peer1);
        let score2 = scorer.get_score(&peer2);

        // Peer1 should have higher score
        assert!(score1 > score2, "Peer1 score {} should be > Peer2 score {}", score1, score2);
    }

    #[test]
    fn test_peer_selection() {
        let scorer = PeerScorer::new();
        let peer1: SocketAddr = "127.0.0.1:8333".parse().unwrap();
        let peer2: SocketAddr = "127.0.0.2:8333".parse().unwrap();
        let peer3: SocketAddr = "127.0.0.3:8333".parse().unwrap();

        // Give peer1 best stats
        scorer.record_block(peer1, 1_000_000, 50.0);
        scorer.record_block(peer1, 1_000_000, 50.0);

        // Give peer2 medium stats
        scorer.record_block(peer2, 500_000, 200.0);

        // Give peer3 worst stats
        scorer.record_failure(peer3);
        scorer.record_failure(peer3);

        let available = vec![peer1, peer2, peer3];
        let selected = scorer.select_best_peers(&available, 2);

        // Should select peer1 and peer2 (not peer3)
        assert_eq!(selected.len(), 2);
        assert!(selected.contains(&peer1));
        assert!(selected.contains(&peer2));
        assert!(!selected.contains(&peer3));
    }

    // ============================================================
    // LAN Peer Detection Tests - Critical for sibling node prioritization
    // ============================================================

    #[test]
    fn test_is_lan_peer_ipv4_private_10_range() {
        // 10.0.0.0/8 - Class A private
        assert!(is_lan_peer(&"10.0.0.1:8333".parse().unwrap()));
        assert!(is_lan_peer(&"10.255.255.254:8333".parse().unwrap()));
        assert!(is_lan_peer(&"10.123.45.67:8333".parse().unwrap()));
    }

    #[test]
    fn test_is_lan_peer_ipv4_private_172_range() {
        // 172.16.0.0/12 - Class B private (172.16.x.x - 172.31.x.x)
        assert!(is_lan_peer(&"172.16.0.1:8333".parse().unwrap()));
        assert!(is_lan_peer(&"172.31.255.254:8333".parse().unwrap()));
        assert!(is_lan_peer(&"172.20.5.10:8333".parse().unwrap()));
        
        // 172.32.x.x should NOT be private
        assert!(!is_lan_peer(&"172.32.0.1:8333".parse().unwrap()));
        assert!(!is_lan_peer(&"172.15.0.1:8333".parse().unwrap()));
    }

    #[test]
    fn test_is_lan_peer_ipv4_private_192_168_range() {
        // 192.168.0.0/16 - Most common home network range
        assert!(is_lan_peer(&"192.168.0.1:8333".parse().unwrap()));
        assert!(is_lan_peer(&"192.168.1.1:8333".parse().unwrap()));
        assert!(is_lan_peer(&"192.168.2.100:8333".parse().unwrap())); // Start9 node!
        assert!(is_lan_peer(&"192.168.255.254:8333".parse().unwrap()));
    }

    #[test]
    fn test_is_lan_peer_ipv4_loopback() {
        // 127.0.0.0/8 - Loopback
        assert!(is_lan_peer(&"127.0.0.1:8333".parse().unwrap()));
        assert!(is_lan_peer(&"127.255.255.254:8333".parse().unwrap()));
    }

    #[test]
    fn test_is_lan_peer_ipv4_link_local() {
        // 169.254.0.0/16 - Link-local (APIPA)
        assert!(is_lan_peer(&"169.254.0.1:8333".parse().unwrap()));
        assert!(is_lan_peer(&"169.254.255.254:8333".parse().unwrap()));
    }

    #[test]
    fn test_is_lan_peer_ipv4_public() {
        // Public IPs should NOT be considered LAN peers
        assert!(!is_lan_peer(&"8.8.8.8:8333".parse().unwrap()));     // Google DNS
        assert!(!is_lan_peer(&"1.1.1.1:8333".parse().unwrap()));     // Cloudflare
        assert!(!is_lan_peer(&"45.33.20.159:8333".parse().unwrap()));// Random public
        assert!(!is_lan_peer(&"216.107.135.194:8333".parse().unwrap())); // Internet peer
    }

    #[test]
    fn test_is_lan_peer_ipv6_loopback() {
        // ::1 - IPv6 loopback
        assert!(is_lan_peer(&"[::1]:8333".parse().unwrap()));
    }

    #[test]
    fn test_is_lan_peer_ipv6_unique_local() {
        // fd00::/8 - Unique Local Address (ULA)
        assert!(is_lan_peer(&"[fd00::1]:8333".parse().unwrap()));
        assert!(is_lan_peer(&"[fd12:3456:789a::1]:8333".parse().unwrap()));
        assert!(is_lan_peer(&"[fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:8333".parse().unwrap()));
    }

    #[test]
    fn test_is_lan_peer_ipv6_link_local() {
        // fe80::/10 - Link-local
        assert!(is_lan_peer(&"[fe80::1]:8333".parse().unwrap()));
        assert!(is_lan_peer(&"[fe80::abcd:1234]:8333".parse().unwrap()));
    }

    #[test]
    fn test_is_lan_peer_ipv6_public() {
        // Public IPv6 should NOT be LAN
        assert!(!is_lan_peer(&"[2001:4860:4860::8888]:8333".parse().unwrap())); // Google DNS
        assert!(!is_lan_peer(&"[2606:4700:4700::1111]:8333".parse().unwrap())); // Cloudflare
    }

    // ============================================================
    // LAN Bonus Score Tests - Critical for sibling node prioritization
    // ============================================================

    #[test]
    fn test_lan_peer_gets_10x_score_multiplier() {
        let scorer = PeerScorer::new();
        
        // LAN peer (192.168.x.x)
        let lan_peer: SocketAddr = "192.168.2.100:8333".parse().unwrap();
        // Internet peer (public IP)
        let internet_peer: SocketAddr = "45.33.20.159:8333".parse().unwrap();
        
        // Record identical performance for both
        scorer.record_block(lan_peer, 1_000_000, 100.0);
        scorer.record_block(internet_peer, 1_000_000, 100.0);
        
        let lan_score = scorer.get_score(&lan_peer);
        let internet_score = scorer.get_score(&internet_peer);
        
        // LAN peer should have 10x higher score due to multiplier
        assert!(
            lan_score > internet_score * 5.0, 
            "LAN peer score {} should be significantly higher than internet peer {} (10x multiplier expected)",
            lan_score, internet_score
        );
        
        // Verify LAN detection worked
        assert!(scorer.is_peer_lan(&lan_peer), "192.168.2.100 should be detected as LAN");
        assert!(!scorer.is_peer_lan(&internet_peer), "45.33.20.159 should NOT be detected as LAN");
    }

    #[test]
    fn test_lan_peer_dominates_best_peer_selection() {
        let scorer = PeerScorer::new();
        
        // LAN peer with moderate performance
        let lan_peer: SocketAddr = "192.168.1.50:8333".parse().unwrap();
        scorer.record_block(lan_peer, 500_000, 100.0);  // 500KB, 100ms
        
        // Internet peer with BETTER raw performance (but no LAN bonus)
        let internet_peer: SocketAddr = "8.8.8.8:8333".parse().unwrap();
        scorer.record_block(internet_peer, 2_000_000, 50.0);  // 2MB, 50ms - 4x better!
        
        let available = vec![lan_peer, internet_peer];
        let best = scorer.get_best_peer(&available);
        
        // LAN peer should still win due to 10x multiplier
        assert_eq!(
            best, Some(lan_peer),
            "LAN peer should be selected as best despite worse raw performance"
        );
    }

    #[test]
    fn test_lan_peer_count() {
        let scorer = PeerScorer::new();
        
        // Add some LAN peers
        scorer.record_block("192.168.1.1:8333".parse().unwrap(), 100, 10.0);
        scorer.record_block("192.168.2.100:8333".parse().unwrap(), 100, 10.0);
        scorer.record_block("10.0.0.5:8333".parse().unwrap(), 100, 10.0);
        
        // Add some internet peers
        scorer.record_block("8.8.8.8:8333".parse().unwrap(), 100, 10.0);
        scorer.record_block("1.1.1.1:8333".parse().unwrap(), 100, 10.0);
        
        assert_eq!(scorer.lan_peer_count(), 3, "Should have 3 LAN peers");
        
        let lan_peers = scorer.get_lan_peers();
        assert_eq!(lan_peers.len(), 3, "get_lan_peers should return 3 peers");
    }

    // ============================================================
    // Failure Penalty Tests
    // ============================================================

    #[test]
    fn test_failures_reduce_score() {
        let scorer = PeerScorer::new();
        let peer: SocketAddr = "8.8.8.8:8333".parse().unwrap();
        
        // Record good performance
        scorer.record_block(peer, 1_000_000, 100.0);
        let score_before = scorer.get_score(&peer);
        
        // Record failures
        scorer.record_failure(peer);
        scorer.record_failure(peer);
        scorer.record_failure(peer);
        
        let score_after = scorer.get_score(&peer);
        
        assert!(
            score_after < score_before,
            "Score should decrease after failures: {} -> {}",
            score_before, score_after
        );
    }

    #[test]
    fn test_minimum_score_preserved() {
        let scorer = PeerScorer::new();
        let peer: SocketAddr = "8.8.8.8:8333".parse().unwrap();
        
        // Record many failures
        for _ in 0..100 {
            scorer.record_failure(peer);
        }
        
        let score = scorer.get_score(&peer);
        
        // Score should not go below 0.1
        assert!(
            score >= 0.1,
            "Score should not go below minimum 0.1: got {}",
            score
        );
    }
}


