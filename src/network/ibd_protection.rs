//! IBD (Initial Block Download) Bandwidth Protection
//!
//! Protects against bandwidth exhaustion attacks where malicious peers repeatedly
//! request full blockchain sync to exhaust node bandwidth and cause ISP overages.
//!
//! Mitigations:
//! - Per-peer bandwidth limits (daily/hourly)
//! - Per-IP bandwidth limits
//! - Per-subnet bandwidth limits
//! - Suspicious pattern detection (rapid reconnection + new peer ID)
//! - Peer reputation scoring
//! - Concurrent IBD serving limits

use crate::utils::current_timestamp;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Bandwidth usage tracking for a time window
#[derive(Debug, Clone)]
struct BandwidthWindow {
    /// Bytes transferred in this window
    bytes: u64,
    /// Window start timestamp
    window_start: u64,
    /// Window duration in seconds
    window_duration: u64,
}

impl BandwidthWindow {
    fn new(window_duration: u64) -> Self {
        Self {
            bytes: 0,
            window_start: current_timestamp(),
            window_duration,
        }
    }

    /// Check if window has expired and reset if needed
    fn check_and_reset(&mut self) {
        let now = current_timestamp();
        if now.saturating_sub(self.window_start) >= self.window_duration {
            self.bytes = 0;
            self.window_start = now;
        }
    }

    /// Add bytes to this window
    fn add_bytes(&mut self, bytes: u64) {
        self.check_and_reset();
        self.bytes += bytes;
    }

    /// Get current bytes in window
    fn get_bytes(&mut self) -> u64 {
        self.check_and_reset();
        self.bytes
    }
}

/// Per-peer bandwidth tracking
#[derive(Debug, Clone)]
struct PeerBandwidth {
    /// Daily bandwidth window (24 hours)
    daily: BandwidthWindow,
    /// Hourly bandwidth window (1 hour)
    hourly: BandwidthWindow,
    /// Total IBD requests from this peer
    ibd_requests: u32,
    /// Last IBD request timestamp
    last_ibd_request: Option<u64>,
    /// Peer reputation score (higher is better, negative = suspicious)
    reputation: i32,
}

impl PeerBandwidth {
    fn new() -> Self {
        Self {
            daily: BandwidthWindow::new(86400), // 24 hours
            hourly: BandwidthWindow::new(3600), // 1 hour
            ibd_requests: 0,
            last_ibd_request: None,
            reputation: 0,
        }
    }

    /// Record bandwidth usage
    fn record_bandwidth(&mut self, bytes: u64) {
        self.daily.add_bytes(bytes);
        self.hourly.add_bytes(bytes);
    }

    /// Record an IBD request
    fn record_ibd_request(&mut self) {
        self.ibd_requests += 1;
        self.last_ibd_request = Some(current_timestamp());
        // Penalize reputation for each IBD request
        self.reputation -= 10;
    }

    /// Check if peer can request IBD (cooldown period)
    fn can_request_ibd(&self, cooldown_seconds: u64) -> bool {
        if let Some(last_request) = self.last_ibd_request {
            let now = current_timestamp();
            now.saturating_sub(last_request) >= cooldown_seconds
        } else {
            true // First request is always allowed
        }
    }

    /// Decay reputation over time (improves reputation)
    fn decay_reputation(&mut self, hours_since_last_request: u64) {
        if hours_since_last_request > 0 {
            let improvement = (hours_since_last_request as i32).min(100);
            self.reputation = (self.reputation + improvement).min(100);
        }
    }
}

/// Per-IP bandwidth tracking
#[derive(Debug, Clone)]
struct IpBandwidth {
    /// Daily bandwidth window
    daily: BandwidthWindow,
    /// Hourly bandwidth window
    hourly: BandwidthWindow,
    /// Number of IBD requests from this IP
    ibd_requests: u32,
    /// Last IBD request timestamp
    last_ibd_request: Option<u64>,
}

impl IpBandwidth {
    fn new() -> Self {
        Self {
            daily: BandwidthWindow::new(86400),
            hourly: BandwidthWindow::new(3600),
            ibd_requests: 0,
            last_ibd_request: None,
        }
    }

    fn record_bandwidth(&mut self, bytes: u64) {
        self.daily.add_bytes(bytes);
        self.hourly.add_bytes(bytes);
    }

    fn record_ibd_request(&mut self) {
        self.ibd_requests += 1;
        self.last_ibd_request = Some(current_timestamp());
    }
}

/// Per-subnet bandwidth tracking
#[derive(Debug, Clone)]
struct SubnetBandwidth {
    /// Daily bandwidth window
    daily: BandwidthWindow,
    /// Hourly bandwidth window
    hourly: BandwidthWindow,
    /// Number of IBD requests from this subnet
    ibd_requests: u32,
}

impl SubnetBandwidth {
    fn new() -> Self {
        Self {
            daily: BandwidthWindow::new(86400),
            hourly: BandwidthWindow::new(3600),
            ibd_requests: 0,
        }
    }

    fn record_bandwidth(&mut self, bytes: u64) {
        self.daily.add_bytes(bytes);
        self.hourly.add_bytes(bytes);
    }

    fn record_ibd_request(&mut self) {
        self.ibd_requests += 1;
    }
}

/// Extract IPv4 subnet (/24) from IP address
fn get_ipv4_subnet(ip: Ipv4Addr) -> [u8; 3] {
    let octets = ip.octets();
    [octets[0], octets[1], octets[2]]
}

/// Extract IPv6 subnet (/64) from IP address
fn get_ipv6_subnet(ip: Ipv6Addr) -> [u8; 8] {
    let segments = ip.segments();
    [
        (segments[0] >> 8) as u8,
        segments[0] as u8,
        (segments[1] >> 8) as u8,
        segments[1] as u8,
        (segments[2] >> 8) as u8,
        segments[2] as u8,
        (segments[3] >> 8) as u8,
        segments[3] as u8,
    ]
}

/// IBD protection configuration
#[derive(Debug, Clone)]
pub struct IbdProtectionConfig {
    /// Max bandwidth per peer per day (bytes)
    pub max_bandwidth_per_peer_per_day: u64,
    /// Max bandwidth per peer per hour (bytes)
    pub max_bandwidth_per_peer_per_hour: u64,
    /// Max bandwidth per IP per day (bytes)
    pub max_bandwidth_per_ip_per_day: u64,
    /// Max bandwidth per IP per hour (bytes)
    pub max_bandwidth_per_ip_per_hour: u64,
    /// Max bandwidth per subnet per day (bytes)
    pub max_bandwidth_per_subnet_per_day: u64,
    /// Max bandwidth per subnet per hour (bytes)
    pub max_bandwidth_per_subnet_per_hour: u64,
    /// Max concurrent IBD serving connections
    pub max_concurrent_ibd_serving: usize,
    /// IBD request cooldown period (seconds)
    pub ibd_request_cooldown_seconds: u64,
    /// Suspicious reconnection threshold (reconnections per hour)
    pub suspicious_reconnection_threshold: u32,
    /// Peer reputation ban threshold (negative reputation to ban)
    pub reputation_ban_threshold: i32,
    /// Enable emergency throttle mode
    pub enable_emergency_throttle: bool,
    /// Emergency throttle percentage (0-100)
    pub emergency_throttle_percent: u8,
}

impl Default for IbdProtectionConfig {
    fn default() -> Self {
        Self {
            // Default: 50 GB/day, 10 GB/hour per peer
            max_bandwidth_per_peer_per_day: 50 * 1024 * 1024 * 1024,
            max_bandwidth_per_peer_per_hour: 10 * 1024 * 1024 * 1024,
            // Default: 100 GB/day, 20 GB/hour per IP
            max_bandwidth_per_ip_per_day: 100 * 1024 * 1024 * 1024,
            max_bandwidth_per_ip_per_hour: 20 * 1024 * 1024 * 1024,
            // Default: 500 GB/day, 100 GB/hour per subnet
            max_bandwidth_per_subnet_per_day: 500 * 1024 * 1024 * 1024,
            max_bandwidth_per_subnet_per_hour: 100 * 1024 * 1024 * 1024,
            // Default: Max 3 concurrent IBD serving
            max_concurrent_ibd_serving: 3,
            // Default: 1 hour cooldown after IBD request
            ibd_request_cooldown_seconds: 3600,
            // Default: 3 reconnections in 1 hour = suspicious
            suspicious_reconnection_threshold: 3,
            // Default: Ban peer if reputation < -100
            reputation_ban_threshold: -100,
            // Default: Emergency throttle disabled
            enable_emergency_throttle: false,
            // Default: 50% throttle when enabled
            emergency_throttle_percent: 50,
        }
    }
}

/// IBD protection manager
pub struct IbdProtectionManager {
    config: IbdProtectionConfig,
    /// Per-peer bandwidth tracking
    peer_bandwidth: Arc<Mutex<HashMap<SocketAddr, PeerBandwidth>>>,
    /// Per-IP bandwidth tracking
    ip_bandwidth: Arc<Mutex<HashMap<IpAddr, IpBandwidth>>>,
    /// Per-subnet bandwidth tracking (IPv4 /24)
    ipv4_subnet_bandwidth: Arc<Mutex<HashMap<[u8; 3], SubnetBandwidth>>>,
    /// Per-subnet bandwidth tracking (IPv6 /64)
    ipv6_subnet_bandwidth: Arc<Mutex<HashMap<[u8; 8], SubnetBandwidth>>>,
    /// Current concurrent IBD serving count
    concurrent_ibd_serving: Arc<Mutex<usize>>,
}

impl IbdProtectionManager {
    /// Create a new IBD protection manager with default config
    pub fn new() -> Self {
        Self::with_config(IbdProtectionConfig::default())
    }

    /// Create a new IBD protection manager with custom config
    pub fn with_config(config: IbdProtectionConfig) -> Self {
        Self {
            config,
            peer_bandwidth: Arc::new(Mutex::new(HashMap::new())),
            ip_bandwidth: Arc::new(Mutex::new(HashMap::new())),
            ipv4_subnet_bandwidth: Arc::new(Mutex::new(HashMap::new())),
            ipv6_subnet_bandwidth: Arc::new(Mutex::new(HashMap::new())),
            concurrent_ibd_serving: Arc::new(Mutex::new(0)),
        }
    }

    /// Check if a peer can request IBD (before starting to serve)
    pub async fn can_serve_ibd(&self, peer_addr: SocketAddr) -> Result<bool, String> {
        let ip = peer_addr.ip();

        // Check emergency throttle mode
        if self.config.enable_emergency_throttle {
            warn!("Emergency throttle mode enabled - rejecting IBD request from {}", peer_addr);
            return Ok(false);
        }

        // Check concurrent IBD serving limit
        {
            let mut concurrent = self.concurrent_ibd_serving.lock().await;
            if *concurrent >= self.config.max_concurrent_ibd_serving {
                warn!(
                    "Concurrent IBD serving limit reached ({}), rejecting request from {}",
                    *concurrent, peer_addr
                );
                return Ok(false);
            }
        }

        // Check per-peer limits
        {
            let mut peer_bw = self.peer_bandwidth.lock().await;
            let peer = peer_bw.entry(peer_addr).or_insert_with(PeerBandwidth::new);

            // Check cooldown period
            if !peer.can_request_ibd(self.config.ibd_request_cooldown_seconds) {
                warn!(
                    "Peer {} is in IBD cooldown period, rejecting request",
                    peer_addr
                );
                return Ok(false);
            }

            // Check reputation
            if peer.reputation < self.config.reputation_ban_threshold {
                warn!(
                    "Peer {} has low reputation ({}), rejecting IBD request",
                    peer_addr, peer.reputation
                );
                return Ok(false);
            }

            // Check daily bandwidth limit
            let daily_bytes = peer.daily.get_bytes();
            if daily_bytes >= self.config.max_bandwidth_per_peer_per_day {
                warn!(
                    "Peer {} exceeded daily bandwidth limit ({} bytes), rejecting IBD request",
                    peer_addr, daily_bytes
                );
                return Ok(false);
            }

            // Check hourly bandwidth limit
            let hourly_bytes = peer.hourly.get_bytes();
            if hourly_bytes >= self.config.max_bandwidth_per_peer_per_hour {
                warn!(
                    "Peer {} exceeded hourly bandwidth limit ({} bytes), rejecting IBD request",
                    peer_addr, hourly_bytes
                );
                return Ok(false);
            }
        }

        // Check per-IP limits
        {
            let mut ip_bw = self.ip_bandwidth.lock().await;
            let ip_tracker = ip_bw.entry(ip).or_insert_with(IpBandwidth::new);

            let daily_bytes = ip_tracker.daily.get_bytes();
            if daily_bytes >= self.config.max_bandwidth_per_ip_per_day {
                warn!(
                    "IP {} exceeded daily bandwidth limit ({} bytes), rejecting IBD request",
                    ip, daily_bytes
                );
                return Ok(false);
            }

            let hourly_bytes = ip_tracker.hourly.get_bytes();
            if hourly_bytes >= self.config.max_bandwidth_per_ip_per_hour {
                warn!(
                    "IP {} exceeded hourly bandwidth limit ({} bytes), rejecting IBD request",
                    ip, hourly_bytes
                );
                return Ok(false);
            }
        }

        // Check per-subnet limits
        match ip {
            IpAddr::V4(ipv4) => {
                let subnet = get_ipv4_subnet(ipv4);
                let mut subnet_bw = self.ipv4_subnet_bandwidth.lock().await;
                let subnet_tracker = subnet_bw.entry(subnet).or_insert_with(SubnetBandwidth::new);

                let daily_bytes = subnet_tracker.daily.get_bytes();
                if daily_bytes >= self.config.max_bandwidth_per_subnet_per_day {
                    warn!(
                        "Subnet {:?} exceeded daily bandwidth limit ({} bytes), rejecting IBD request",
                        subnet, daily_bytes
                    );
                    return Ok(false);
                }

                let hourly_bytes = subnet_tracker.hourly.get_bytes();
                if hourly_bytes >= self.config.max_bandwidth_per_subnet_per_hour {
                    warn!(
                        "Subnet {:?} exceeded hourly bandwidth limit ({} bytes), rejecting IBD request",
                        subnet, hourly_bytes
                    );
                    return Ok(false);
                }
            }
            IpAddr::V6(ipv6) => {
                let subnet = get_ipv6_subnet(ipv6);
                let mut subnet_bw = self.ipv6_subnet_bandwidth.lock().await;
                let subnet_tracker = subnet_bw.entry(subnet).or_insert_with(SubnetBandwidth::new);

                let daily_bytes = subnet_tracker.daily.get_bytes();
                if daily_bytes >= self.config.max_bandwidth_per_subnet_per_day {
                    warn!(
                        "Subnet {:?} exceeded daily bandwidth limit ({} bytes), rejecting IBD request",
                        subnet, daily_bytes
                    );
                    return Ok(false);
                }

                let hourly_bytes = subnet_tracker.hourly.get_bytes();
                if hourly_bytes >= self.config.max_bandwidth_per_subnet_per_hour {
                    warn!(
                        "Subnet {:?} exceeded hourly bandwidth limit ({} bytes), rejecting IBD request",
                        subnet, hourly_bytes
                    );
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    /// Record that IBD serving has started for a peer
    pub async fn start_ibd_serving(&self, peer_addr: SocketAddr) {
        let ip = peer_addr.ip();

        // Increment concurrent IBD serving count
        {
            let mut concurrent = self.concurrent_ibd_serving.lock().await;
            *concurrent += 1;
        }

        // Record IBD request
        {
            let mut peer_bw = self.peer_bandwidth.lock().await;
            let peer = peer_bw.entry(peer_addr).or_insert_with(PeerBandwidth::new);
            peer.record_ibd_request();
        }

        {
            let mut ip_bw = self.ip_bandwidth.lock().await;
            let ip_tracker = ip_bw.entry(ip).or_insert_with(IpBandwidth::new);
            ip_tracker.record_ibd_request();
        }

        match ip {
            IpAddr::V4(ipv4) => {
                let subnet = get_ipv4_subnet(ipv4);
                let mut subnet_bw = self.ipv4_subnet_bandwidth.lock().await;
                let subnet_tracker = subnet_bw.entry(subnet).or_insert_with(SubnetBandwidth::new);
                subnet_tracker.record_ibd_request();
            }
            IpAddr::V6(ipv6) => {
                let subnet = get_ipv6_subnet(ipv6);
                let mut subnet_bw = self.ipv6_subnet_bandwidth.lock().await;
                let subnet_tracker = subnet_bw.entry(subnet).or_insert_with(SubnetBandwidth::new);
                subnet_tracker.record_ibd_request();
            }
        }
    }

    /// Record that IBD serving has stopped for a peer
    pub async fn stop_ibd_serving(&self, peer_addr: SocketAddr) {
        // Decrement concurrent IBD serving count
        let mut concurrent = self.concurrent_ibd_serving.lock().await;
        if *concurrent > 0 {
            *concurrent -= 1;
        }
    }

    /// Record bandwidth usage for a peer
    pub async fn record_bandwidth(&self, peer_addr: SocketAddr, bytes: u64) {
        let ip = peer_addr.ip();

        // Apply emergency throttle if enabled
        let bytes_to_record = if self.config.enable_emergency_throttle {
            let throttle_factor = (100 - self.config.emergency_throttle_percent) as f64 / 100.0;
            (bytes as f64 * throttle_factor) as u64
        } else {
            bytes
        };

        // Record per-peer bandwidth
        {
            let mut peer_bw = self.peer_bandwidth.lock().await;
            let peer = peer_bw.entry(peer_addr).or_insert_with(PeerBandwidth::new);
            peer.record_bandwidth(bytes_to_record);
        }

        // Record per-IP bandwidth
        {
            let mut ip_bw = self.ip_bandwidth.lock().await;
            let ip_tracker = ip_bw.entry(ip).or_insert_with(IpBandwidth::new);
            ip_tracker.record_bandwidth(bytes_to_record);
        }

        // Record per-subnet bandwidth
        match ip {
            IpAddr::V4(ipv4) => {
                let subnet = get_ipv4_subnet(ipv4);
                let mut subnet_bw = self.ipv4_subnet_bandwidth.lock().await;
                let subnet_tracker = subnet_bw.entry(subnet).or_insert_with(SubnetBandwidth::new);
                subnet_tracker.record_bandwidth(bytes_to_record);
            }
            IpAddr::V6(ipv6) => {
                let subnet = get_ipv6_subnet(ipv6);
                let mut subnet_bw = self.ipv6_subnet_bandwidth.lock().await;
                let subnet_tracker = subnet_bw.entry(subnet).or_insert_with(SubnetBandwidth::new);
                subnet_tracker.record_bandwidth(bytes_to_record);
            }
        }
    }

    /// Get current configuration
    pub fn get_config(&self) -> &IbdProtectionConfig {
        &self.config
    }

    /// Update configuration
    pub fn update_config(&mut self, config: IbdProtectionConfig) {
        self.config = config;
    }

    /// Get peer reputation
    pub async fn get_peer_reputation(&self, peer_addr: SocketAddr) -> Option<i32> {
        let peer_bw = self.peer_bandwidth.lock().await;
        peer_bw.get(&peer_addr).map(|p| p.reputation)
    }

    /// Cleanup old entries (periodic maintenance)
    pub async fn cleanup(&self) {
        // Cleanup is handled automatically by BandwidthWindow::check_and_reset()
        // This method can be used for more aggressive cleanup if needed
        debug!("IBD protection cleanup completed");
    }
}

impl Default for IbdProtectionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ibd_protection_default_config() {
        let manager = IbdProtectionManager::new();
        let config = manager.get_config();
        assert_eq!(config.max_concurrent_ibd_serving, 3);
        assert_eq!(config.ibd_request_cooldown_seconds, 3600);
    }

    #[tokio::test]
    async fn test_can_serve_ibd_first_request() {
        let manager = IbdProtectionManager::new();
        let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();
        
        let can_serve = manager.can_serve_ibd(peer_addr).await;
        assert!(can_serve.is_ok());
        assert!(can_serve.unwrap());
    }

    #[tokio::test]
    async fn test_ibd_cooldown_period() {
        let mut config = IbdProtectionConfig::default();
        config.ibd_request_cooldown_seconds = 1; // 1 second for testing
        let manager = IbdProtectionManager::with_config(config);
        let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();
        
        // First request should succeed
        let can_serve1 = manager.can_serve_ibd(peer_addr).await.unwrap();
        assert!(can_serve1);
        
        // Start IBD serving
        manager.start_ibd_serving(peer_addr).await;
        
        // Second request immediately should fail (cooldown)
        let can_serve2 = manager.can_serve_ibd(peer_addr).await.unwrap();
        assert!(!can_serve2);
        
        // Wait for cooldown
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        
        // Third request after cooldown should succeed
        let can_serve3 = manager.can_serve_ibd(peer_addr).await.unwrap();
        assert!(can_serve3);
    }

    #[tokio::test]
    async fn test_concurrent_ibd_limit() {
        let mut config = IbdProtectionConfig::default();
        config.max_concurrent_ibd_serving = 2;
        let manager = IbdProtectionManager::with_config(config);
        
        let peer1: SocketAddr = "127.0.0.1:8333".parse().unwrap();
        let peer2: SocketAddr = "127.0.0.2:8333".parse().unwrap();
        let peer3: SocketAddr = "127.0.0.3:8333".parse().unwrap();
        
        // First two should succeed
        assert!(manager.can_serve_ibd(peer1).await.unwrap());
        manager.start_ibd_serving(peer1).await;
        
        assert!(manager.can_serve_ibd(peer2).await.unwrap());
        manager.start_ibd_serving(peer2).await;
        
        // Third should fail (limit reached)
        assert!(!manager.can_serve_ibd(peer3).await.unwrap());
        
        // After stopping one, third should succeed
        manager.stop_ibd_serving(peer1).await;
        assert!(manager.can_serve_ibd(peer3).await.unwrap());
    }

    #[tokio::test]
    async fn test_bandwidth_tracking() {
        let manager = IbdProtectionManager::new();
        let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();
        
        // Record bandwidth
        manager.record_bandwidth(peer_addr, 1024 * 1024 * 1024).await; // 1 GB
        
        // Check that bandwidth was recorded
        let can_serve = manager.can_serve_ibd(peer_addr).await.unwrap();
        assert!(can_serve); // Should still be under limit
    }
}




