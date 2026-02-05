//! Unified Bandwidth Protection System
//!
//! Extends IBD protection to cover all high-bandwidth services:
//! - IBD (Initial Block Download)
//! - Filter Service (BIP157/158)
//! - Package Relay (BIP331)
//! - UTXO Set Serving
//! - Filtered Blocks
//! - Transaction Relay
//! - Module Serving
//!
//! Provides per-peer, per-IP, and per-subnet bandwidth limits for each service type.

use crate::network::ibd_protection::{IbdProtectionManager, IbdProtectionConfig};
use crate::utils::current_timestamp;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Service type for bandwidth protection
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceType {
    /// Initial Block Download (blocks/headers)
    Ibd,
    /// Compact Block Relay (BIP152)
    CompactBlocks,
    /// Filter Service (BIP157/158)
    Filters,
    /// UTXO Commitments
    UtxoCommitments,
    /// UTXO Set Serving
    UtxoSet,
    /// Filtered Blocks
    FilteredBlocks,
    /// Package Relay (BIP331)
    PackageRelay,
    /// Transaction Relay
    TransactionRelay,
    /// Module Serving
    ModuleServing,
}

/// Service-specific bandwidth limits
#[derive(Debug, Clone)]
pub struct ServiceLimits {
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
    /// Max requests per hour (optional)
    pub max_requests_per_hour: Option<u32>,
    /// Max CPU time per request in milliseconds (optional, for CPU-intensive services)
    pub cpu_time_limit_ms: Option<u64>,
}

impl Default for ServiceLimits {
    fn default() -> Self {
        Self {
            max_bandwidth_per_peer_per_day: 10 * 1024 * 1024 * 1024, // 10 GB/day
            max_bandwidth_per_peer_per_hour: 2 * 1024 * 1024 * 1024,  // 2 GB/hour
            max_bandwidth_per_ip_per_day: 20 * 1024 * 1024 * 1024,    // 20 GB/day
            max_bandwidth_per_ip_per_hour: 4 * 1024 * 1024 * 1024,     // 4 GB/hour
            max_bandwidth_per_subnet_per_day: 100 * 1024 * 1024 * 1024, // 100 GB/day
            max_bandwidth_per_subnet_per_hour: 20 * 1024 * 1024 * 1024,  // 20 GB/hour
            max_requests_per_hour: None,
            cpu_time_limit_ms: None,
        }
    }
}

/// Per-service bandwidth tracking
#[derive(Debug, Clone)]
struct ServiceBandwidthTracker {
    /// Daily bandwidth window
    daily_bytes: u64,
    /// Hourly bandwidth window
    hourly_bytes: u64,
    /// Window start timestamp
    window_start: u64,
    /// Request count in current hour
    request_count: u32,
    /// Last request timestamp
    last_request: Option<u64>,
}

impl ServiceBandwidthTracker {
    fn new() -> Self {
        Self {
            daily_bytes: 0,
            hourly_bytes: 0,
            window_start: current_timestamp(),
            request_count: 0,
            last_request: None,
        }
    }

    /// Check and reset windows if needed
    fn check_and_reset(&mut self) {
        let now = current_timestamp();
        let elapsed = now.saturating_sub(self.window_start);

        // Reset hourly window every hour
        if elapsed >= 3600 {
            self.hourly_bytes = 0;
            self.request_count = 0;
        }

        // Reset daily window every 24 hours
        if elapsed >= 86400 {
            self.daily_bytes = 0;
            self.hourly_bytes = 0;
            self.request_count = 0;
            self.window_start = now;
        }
    }

    /// Record bandwidth usage
    fn record_bandwidth(&mut self, bytes: u64) {
        self.check_and_reset();
        self.daily_bytes += bytes;
        self.hourly_bytes += bytes;
    }

    /// Record a request
    fn record_request(&mut self) {
        self.check_and_reset();
        self.request_count += 1;
        self.last_request = Some(current_timestamp());
    }

    /// Get current daily bytes
    fn get_daily_bytes(&mut self) -> u64 {
        self.check_and_reset();
        self.daily_bytes
    }

    /// Get current hourly bytes
    fn get_hourly_bytes(&mut self) -> u64 {
        self.check_and_reset();
        self.hourly_bytes
    }

    /// Get current request count
    fn get_request_count(&mut self) -> u32 {
        self.check_and_reset();
        self.request_count
    }
}

/// Unified bandwidth protection manager
pub struct BandwidthProtectionManager {
    /// IBD protection (reused for backward compatibility)
    ibd_protection: Arc<IbdProtectionManager>,
    /// Service-specific limits
    service_limits: HashMap<ServiceType, ServiceLimits>,
    /// Per-peer service bandwidth tracking
    peer_service_bandwidth: Arc<Mutex<HashMap<(SocketAddr, ServiceType), ServiceBandwidthTracker>>>,
    /// Per-IP service bandwidth tracking
    ip_service_bandwidth: Arc<Mutex<HashMap<(IpAddr, ServiceType), ServiceBandwidthTracker>>>,
    /// Per-subnet service bandwidth tracking (IPv4 /24)
    ipv4_subnet_service_bandwidth: Arc<Mutex<HashMap<([u8; 3], ServiceType), ServiceBandwidthTracker>>>,
    /// Per-subnet service bandwidth tracking (IPv6 /64)
    ipv6_subnet_service_bandwidth: Arc<Mutex<HashMap<([u8; 8], ServiceType), ServiceBandwidthTracker>>>,
}

impl BandwidthProtectionManager {
    /// Create a new bandwidth protection manager
    pub fn new(ibd_protection: Arc<IbdProtectionManager>) -> Self {
        let mut service_limits = HashMap::new();

        // Set default limits for each service
        service_limits.insert(ServiceType::Filters, ServiceLimits {
            max_bandwidth_per_peer_per_day: 5 * 1024 * 1024 * 1024,  // 5 GB/day
            max_bandwidth_per_peer_per_hour: 1 * 1024 * 1024 * 1024,  // 1 GB/hour
            max_bandwidth_per_ip_per_day: 10 * 1024 * 1024 * 1024,   // 10 GB/day
            max_bandwidth_per_ip_per_hour: 2 * 1024 * 1024 * 1024,   // 2 GB/hour
            max_bandwidth_per_subnet_per_day: 50 * 1024 * 1024 * 1024, // 50 GB/day
            max_bandwidth_per_subnet_per_hour: 10 * 1024 * 1024 * 1024, // 10 GB/hour
            max_requests_per_hour: Some(50),
            cpu_time_limit_ms: Some(100), // 100ms CPU time limit for filter generation
        });

        service_limits.insert(ServiceType::PackageRelay, ServiceLimits {
            max_bandwidth_per_peer_per_day: 10 * 1024 * 1024 * 1024,  // 10 GB/day
            max_bandwidth_per_peer_per_hour: 2 * 1024 * 1024 * 1024,  // 2 GB/hour
            max_bandwidth_per_ip_per_day: 20 * 1024 * 1024 * 1024,    // 20 GB/day
            max_bandwidth_per_ip_per_hour: 4 * 1024 * 1024 * 1024,    // 4 GB/hour
            max_bandwidth_per_subnet_per_day: 100 * 1024 * 1024 * 1024, // 100 GB/day
            max_bandwidth_per_subnet_per_hour: 20 * 1024 * 1024 * 1024, // 20 GB/hour
            max_requests_per_hour: Some(100),
            cpu_time_limit_ms: None,
        });

        service_limits.insert(ServiceType::UtxoSet, ServiceLimits {
            max_bandwidth_per_peer_per_day: 50 * 1024 * 1024 * 1024,  // 50 GB/day (full UTXO set is huge)
            max_bandwidth_per_peer_per_hour: 10 * 1024 * 1024 * 1024, // 10 GB/hour
            max_bandwidth_per_ip_per_day: 100 * 1024 * 1024 * 1024,  // 100 GB/day
            max_bandwidth_per_ip_per_hour: 20 * 1024 * 1024 * 1024,  // 20 GB/hour
            max_bandwidth_per_subnet_per_day: 500 * 1024 * 1024 * 1024, // 500 GB/day
            max_bandwidth_per_subnet_per_hour: 100 * 1024 * 1024 * 1024, // 100 GB/hour
            max_requests_per_hour: Some(1), // Very restrictive - full UTXO set is expensive
            cpu_time_limit_ms: None,
        });

        service_limits.insert(ServiceType::FilteredBlocks, ServiceLimits {
            max_bandwidth_per_peer_per_day: 20 * 1024 * 1024 * 1024,  // 20 GB/day
            max_bandwidth_per_peer_per_hour: 5 * 1024 * 1024 * 1024,  // 5 GB/hour
            max_bandwidth_per_ip_per_day: 40 * 1024 * 1024 * 1024,    // 40 GB/day
            max_bandwidth_per_ip_per_hour: 10 * 1024 * 1024 * 1024,   // 10 GB/hour
            max_bandwidth_per_subnet_per_day: 200 * 1024 * 1024 * 1024, // 200 GB/day
            max_bandwidth_per_subnet_per_hour: 50 * 1024 * 1024 * 1024, // 50 GB/hour
            max_requests_per_hour: Some(200),
            cpu_time_limit_ms: None,
        });

        service_limits.insert(ServiceType::ModuleServing, ServiceLimits {
            max_bandwidth_per_peer_per_day: 100 * 1024 * 1024 * 1024, // 100 GB/day (modules can be large)
            max_bandwidth_per_peer_per_hour: 20 * 1024 * 1024 * 1024, // 20 GB/hour
            max_bandwidth_per_ip_per_day: 200 * 1024 * 1024 * 1024,  // 200 GB/day
            max_bandwidth_per_ip_per_hour: 40 * 1024 * 1024 * 1024,   // 40 GB/hour
            max_bandwidth_per_subnet_per_day: 1000 * 1024 * 1024 * 1024, // 1000 GB/day
            max_bandwidth_per_subnet_per_hour: 200 * 1024 * 1024 * 1024, // 200 GB/hour
            max_requests_per_hour: Some(50),
            cpu_time_limit_ms: None,
        });

        service_limits.insert(ServiceType::TransactionRelay, ServiceLimits {
            max_bandwidth_per_peer_per_day: 50 * 1024 * 1024 * 1024,  // 50 GB/day
            max_bandwidth_per_peer_per_hour: 10 * 1024 * 1024 * 1024, // 10 GB/hour
            max_bandwidth_per_ip_per_day: 50 * 1024 * 1024 * 1024,   // 50 GB/day
            max_bandwidth_per_ip_per_hour: 10 * 1024 * 1024 * 1024,  // 10 GB/hour
            max_bandwidth_per_subnet_per_day: 200 * 1024 * 1024 * 1024, // 200 GB/day
            max_bandwidth_per_subnet_per_hour: 40 * 1024 * 1024 * 1024,  // 40 GB/hour
            max_requests_per_hour: None, // Transaction relay uses different rate limiting
            cpu_time_limit_ms: None,
        });

        Self {
            ibd_protection,
            service_limits,
            peer_service_bandwidth: Arc::new(Mutex::new(HashMap::new())),
            ip_service_bandwidth: Arc::new(Mutex::new(HashMap::new())),
            ipv4_subnet_service_bandwidth: Arc::new(Mutex::new(HashMap::new())),
            ipv6_subnet_service_bandwidth: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Check if a service request is allowed (bandwidth and rate limits)
    pub async fn check_service_request(
        &self,
        service_type: ServiceType,
        peer_addr: SocketAddr,
    ) -> Result<bool, String> {
        let limits = match self.service_limits.get(&service_type) {
            Some(l) => l,
            None => {
                warn!("No limits configured for service type {:?}, allowing request", service_type);
                return Ok(true); // No limits = allow
            }
        };

        let ip = peer_addr.ip();

        // Check per-peer limits
        {
            let mut peer_bw = self.peer_service_bandwidth.lock().await;
            let key = (peer_addr, service_type);
            let tracker = peer_bw.entry(key).or_insert_with(ServiceBandwidthTracker::new);

            // Check daily limit
            if tracker.get_daily_bytes() >= limits.max_bandwidth_per_peer_per_day {
                warn!(
                    "Peer {} exceeded daily bandwidth limit for {:?} ({} bytes)",
                    peer_addr, service_type, tracker.get_daily_bytes()
                );
                return Ok(false);
            }

            // Check hourly limit
            if tracker.get_hourly_bytes() >= limits.max_bandwidth_per_peer_per_hour {
                warn!(
                    "Peer {} exceeded hourly bandwidth limit for {:?} ({} bytes)",
                    peer_addr, service_type, tracker.get_hourly_bytes()
                );
                return Ok(false);
            }

            // Check rate limit
            if let Some(max_requests) = limits.max_requests_per_hour {
                if tracker.get_request_count() >= max_requests {
                    warn!(
                        "Peer {} exceeded rate limit for {:?} ({} requests/hour)",
                        peer_addr, service_type, tracker.get_request_count()
                    );
                    return Ok(false);
                }
            }
        }

        // Check per-IP limits
        {
            let mut ip_bw = self.ip_service_bandwidth.lock().await;
            let key = (ip, service_type);
            let tracker = ip_bw.entry(key).or_insert_with(ServiceBandwidthTracker::new);

            if tracker.get_daily_bytes() >= limits.max_bandwidth_per_ip_per_day {
                warn!(
                    "IP {} exceeded daily bandwidth limit for {:?} ({} bytes)",
                    ip, service_type, tracker.get_daily_bytes()
                );
                return Ok(false);
            }

            if tracker.get_hourly_bytes() >= limits.max_bandwidth_per_ip_per_hour {
                warn!(
                    "IP {} exceeded hourly bandwidth limit for {:?} ({} bytes)",
                    ip, service_type, tracker.get_hourly_bytes()
                );
                return Ok(false);
            }
        }

        // Check per-subnet limits
        match ip {
            IpAddr::V4(ipv4) => {
                let subnet = get_ipv4_subnet(ipv4);
                let mut subnet_bw = self.ipv4_subnet_service_bandwidth.lock().await;
                let key = (subnet, service_type);
                let tracker = subnet_bw.entry(key).or_insert_with(ServiceBandwidthTracker::new);

                if tracker.get_daily_bytes() >= limits.max_bandwidth_per_subnet_per_day {
                    warn!(
                        "Subnet {:?} exceeded daily bandwidth limit for {:?} ({} bytes)",
                        subnet, service_type, tracker.get_daily_bytes()
                    );
                    return Ok(false);
                }

                if tracker.get_hourly_bytes() >= limits.max_bandwidth_per_subnet_per_hour {
                    warn!(
                        "Subnet {:?} exceeded hourly bandwidth limit for {:?} ({} bytes)",
                        subnet, service_type, tracker.get_hourly_bytes()
                    );
                    return Ok(false);
                }
            }
            IpAddr::V6(ipv6) => {
                let subnet = get_ipv6_subnet(ipv6);
                let mut subnet_bw = self.ipv6_subnet_service_bandwidth.lock().await;
                let key = (subnet, service_type);
                let tracker = subnet_bw.entry(key).or_insert_with(ServiceBandwidthTracker::new);

                if tracker.get_daily_bytes() >= limits.max_bandwidth_per_subnet_per_day {
                    warn!(
                        "Subnet {:?} exceeded daily bandwidth limit for {:?} ({} bytes)",
                        subnet, service_type, tracker.get_daily_bytes()
                    );
                    return Ok(false);
                }

                if tracker.get_hourly_bytes() >= limits.max_bandwidth_per_subnet_per_hour {
                    warn!(
                        "Subnet {:?} exceeded hourly bandwidth limit for {:?} ({} bytes)",
                        subnet, service_type, tracker.get_hourly_bytes()
                    );
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    /// Record bandwidth usage for a service
    pub async fn record_service_bandwidth(
        &self,
        service_type: ServiceType,
        peer_addr: SocketAddr,
        bytes: u64,
    ) {
        let ip = peer_addr.ip();

        // Record per-peer bandwidth
        {
            let mut peer_bw = self.peer_service_bandwidth.lock().await;
            let key = (peer_addr, service_type);
            let tracker = peer_bw.entry(key).or_insert_with(ServiceBandwidthTracker::new);
            tracker.record_bandwidth(bytes);
        }

        // Record per-IP bandwidth
        {
            let mut ip_bw = self.ip_service_bandwidth.lock().await;
            let key = (ip, service_type);
            let tracker = ip_bw.entry(key).or_insert_with(ServiceBandwidthTracker::new);
            tracker.record_bandwidth(bytes);
        }

        // Record per-subnet bandwidth
        match ip {
            IpAddr::V4(ipv4) => {
                let subnet = get_ipv4_subnet(ipv4);
                let mut subnet_bw = self.ipv4_subnet_service_bandwidth.lock().await;
                let key = (subnet, service_type);
                let tracker = subnet_bw.entry(key).or_insert_with(ServiceBandwidthTracker::new);
                tracker.record_bandwidth(bytes);
            }
            IpAddr::V6(ipv6) => {
                let subnet = get_ipv6_subnet(ipv6);
                let mut subnet_bw = self.ipv6_subnet_service_bandwidth.lock().await;
                let key = (subnet, service_type);
                let tracker = subnet_bw.entry(key).or_insert_with(ServiceBandwidthTracker::new);
                tracker.record_bandwidth(bytes);
            }
        }
    }

    /// Record a service request (for rate limiting)
    pub async fn record_service_request(
        &self,
        service_type: ServiceType,
        peer_addr: SocketAddr,
    ) {
        let ip = peer_addr.ip();

        // Record per-peer request
        {
            let mut peer_bw = self.peer_service_bandwidth.lock().await;
            let key = (peer_addr, service_type);
            let tracker = peer_bw.entry(key).or_insert_with(ServiceBandwidthTracker::new);
            tracker.record_request();
        }

        // Record per-IP request
        {
            let mut ip_bw = self.ip_service_bandwidth.lock().await;
            let key = (ip, service_type);
            let tracker = ip_bw.entry(key).or_insert_with(ServiceBandwidthTracker::new);
            tracker.record_request();
        }
    }

    /// Check CPU time limit (for CPU-intensive services like filter generation)
    pub fn check_cpu_time_limit(
        &self,
        service_type: ServiceType,
        cpu_time_ms: u64,
    ) -> bool {
        let limits = match self.service_limits.get(&service_type) {
            Some(l) => l,
            None => return true, // No limit = allow
        };

        if let Some(max_cpu_ms) = limits.cpu_time_limit_ms {
            if cpu_time_ms > max_cpu_ms {
                warn!(
                    "CPU time limit exceeded for {:?}: {}ms > {}ms",
                    service_type, cpu_time_ms, max_cpu_ms
                );
                return false;
            }
        }

        true
    }

    /// Get IBD protection manager (for backward compatibility)
    pub fn ibd_protection(&self) -> &Arc<IbdProtectionManager> {
        &self.ibd_protection
    }

    /// Update service limits (for configuration updates)
    pub fn update_service_limits(&mut self, service_type: ServiceType, limits: ServiceLimits) {
        self.service_limits.insert(service_type, limits);
    }
}

/// Extract IPv4 subnet (/24) from IP address
fn get_ipv4_subnet(ip: std::net::Ipv4Addr) -> [u8; 3] {
    let octets = ip.octets();
    [octets[0], octets[1], octets[2]]
}

/// Extract IPv6 subnet (/64) from IP address
fn get_ipv6_subnet(ip: std::net::Ipv6Addr) -> [u8; 8] {
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




