//! Enhanced DoS Protection
//!
//! Provides connection rate limiting, message queue monitoring, resource usage tracking,
//! and automatic mitigation for DoS attacks.

use crate::utils::current_timestamp;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::warn;

/// Connection rate limiter (tracks connection attempts per time window)
pub struct ConnectionRateLimiter {
    /// Connection attempts per IP (timestamp -> count)
    connection_attempts: HashMap<IpAddr, Vec<u64>>,
    /// Maximum connections per IP per time window
    max_connections_per_window: usize,
    /// Time window in seconds
    window_seconds: u64,
}

impl ConnectionRateLimiter {
    /// Create a new connection rate limiter
    pub fn new(max_connections_per_window: usize, window_seconds: u64) -> Self {
        Self {
            connection_attempts: HashMap::new(),
            max_connections_per_window,
            window_seconds,
        }
    }

    /// Check if a connection attempt is allowed
    pub fn check_connection(&mut self, ip: IpAddr) -> bool {
        let now = current_timestamp();

        // Clean up old entries outside the time window
        let cutoff = now.saturating_sub(self.window_seconds);

        let attempts = self.connection_attempts.entry(ip).or_default();
        attempts.retain(|&timestamp| timestamp > cutoff);

        // Check if we're within the limit
        if attempts.len() >= self.max_connections_per_window {
            warn!(
                "Connection rate limit exceeded for IP {}: {} attempts in {} seconds",
                ip,
                attempts.len(),
                self.window_seconds
            );
            false
        } else {
            // Record this connection attempt
            attempts.push(now);
            true
        }
    }

    /// Clean up old entries (periodic maintenance)
    pub fn cleanup(&mut self) {
        let now = current_timestamp();
        let cutoff = now.saturating_sub(self.window_seconds);

        self.connection_attempts.retain(|_, attempts| {
            attempts.retain(|&timestamp| timestamp > cutoff);
            !attempts.is_empty()
        });
    }

    /// Get current connection attempt count for an IP
    pub fn get_attempt_count(&self, ip: IpAddr) -> usize {
        self.connection_attempts
            .get(&ip)
            .map(|v| v.len())
            .unwrap_or(0)
    }
}

/// Resource usage metrics
#[derive(Debug, Clone)]
pub struct ResourceMetrics {
    /// Current number of connections
    pub active_connections: usize,
    /// Current message queue size
    pub message_queue_size: usize,
    /// Total bytes received
    pub bytes_received: u64,
    /// Total bytes sent
    pub bytes_sent: u64,
    /// Timestamp of last update
    pub last_update: u64,
}

/// DoS protection metrics (cumulative counters)
#[derive(Debug, Clone)]
pub struct DosProtectionMetrics {
    /// Total connection rate violations
    pub connection_rate_violations: u64,
    /// Total auto-bans applied
    pub auto_bans_applied: u64,
    /// Total message queue overflows
    pub message_queue_overflows: u64,
    /// Total active connection limit hits
    pub active_connection_limit_hits: u64,
    /// Total resource exhaustion events
    pub resource_exhaustion_events: u64,
}

impl Default for ResourceMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceMetrics {
    pub fn new() -> Self {
        let now = current_timestamp();
        Self {
            active_connections: 0,
            message_queue_size: 0,
            bytes_received: 0,
            bytes_sent: 0,
            last_update: now,
        }
    }
}

/// DoS protection manager
pub struct DosProtectionManager {
    /// Connection rate limiter
    connection_rate_limiter: Arc<Mutex<ConnectionRateLimiter>>,
    /// Maximum message queue size
    max_message_queue_size: usize,
    /// Maximum active connections
    max_active_connections: usize,
    /// Resource metrics
    resource_metrics: Arc<Mutex<ResourceMetrics>>,
    /// Auto-ban threshold for connection rate violations
    auto_ban_connection_violations: usize,
    /// IPs that have exceeded connection rate limits
    connection_violations: Arc<Mutex<HashMap<IpAddr, usize>>>,
    /// DoS protection metrics (cumulative counters)
    metrics: Arc<Mutex<DosProtectionMetrics>>,
    /// Ban duration in seconds
    ban_duration_seconds: u64,
}

impl DosProtectionManager {
    /// Create a new DoS protection manager
    pub fn new(
        max_connections_per_window: usize,
        window_seconds: u64,
        max_message_queue_size: usize,
        max_active_connections: usize,
    ) -> Self {
        Self::with_ban_settings(
            max_connections_per_window,
            window_seconds,
            max_message_queue_size,
            max_active_connections,
            3,    // Default auto-ban threshold
            3600, // Default ban duration (1 hour)
        )
    }

    /// Create with custom ban settings
    pub fn with_ban_settings(
        max_connections_per_window: usize,
        window_seconds: u64,
        max_message_queue_size: usize,
        max_active_connections: usize,
        auto_ban_threshold: usize,
        ban_duration_seconds: u64,
    ) -> Self {
        Self {
            connection_rate_limiter: Arc::new(Mutex::new(ConnectionRateLimiter::new(
                max_connections_per_window,
                window_seconds,
            ))),
            max_message_queue_size,
            max_active_connections,
            resource_metrics: Arc::new(Mutex::new(ResourceMetrics::new())),
            auto_ban_connection_violations: auto_ban_threshold,
            connection_violations: Arc::new(Mutex::new(HashMap::new())),
            metrics: Arc::new(Mutex::new(DosProtectionMetrics {
                connection_rate_violations: 0,
                auto_bans_applied: 0,
                message_queue_overflows: 0,
                active_connection_limit_hits: 0,
                resource_exhaustion_events: 0,
            })),
            ban_duration_seconds,
        }
    }

    /// Create with default settings
    pub fn default() -> Self {
        Self::new(
            10,    // Max 10 connections per IP per window
            60,    // 60 second window
            10000, // Max 10k messages in queue
            200,   // Max 200 active connections
        )
    }

    /// Check if a connection attempt is allowed
    pub async fn check_connection(&self, ip: IpAddr) -> bool {
        let mut limiter = self.connection_rate_limiter.lock().await;
        let allowed = limiter.check_connection(ip);

        if !allowed {
            // Track violation
            let mut violations = self.connection_violations.lock().await;
            let count = violations.entry(ip).or_insert(0);
            *count += 1;

            // Update metrics
            let mut metrics = self.metrics.lock().await;
            metrics.connection_rate_violations += 1;

            if *count >= self.auto_ban_connection_violations {
                warn!(
                    "Auto-banning IP {} for repeated connection rate violations ({} violations)",
                    ip, *count
                );
                metrics.auto_bans_applied += 1;
                // Return false to reject, caller should ban
                return false;
            }
        } else {
            // Reset violation count on successful connection
            let mut violations = self.connection_violations.lock().await;
            violations.remove(&ip);
        }

        allowed
    }

    /// Check if message queue is within limits
    pub async fn check_message_queue_size(&self, current_size: usize) -> bool {
        if current_size > self.max_message_queue_size {
            warn!(
                "Message queue size exceeded: {} > {}",
                current_size, self.max_message_queue_size
            );
            let mut metrics = self.metrics.lock().await;
            metrics.message_queue_overflows += 1;
            false
        } else {
            true
        }
    }

    /// Check if we can accept more connections
    pub async fn check_active_connections(&self, current_count: usize) -> bool {
        if current_count >= self.max_active_connections {
            warn!(
                "Active connection limit exceeded: {} >= {}",
                current_count, self.max_active_connections
            );
            let mut metrics = self.metrics.lock().await;
            metrics.active_connection_limit_hits += 1;
            false
        } else {
            true
        }
    }

    /// Get ban duration in seconds
    pub fn ban_duration_seconds(&self) -> u64 {
        self.ban_duration_seconds
    }

    /// Get auto-ban threshold (number of violations before auto-ban)
    pub fn auto_ban_connection_violations(&self) -> usize {
        self.auto_ban_connection_violations
    }

    /// Get list of IPs that should be auto-banned (exceeded violation threshold)
    pub async fn get_ips_to_auto_ban(&self) -> Vec<IpAddr> {
        let violations = self.connection_violations.lock().await;
        violations
            .iter()
            .filter(|(_, &count)| count >= self.auto_ban_connection_violations)
            .map(|(ip, _)| *ip)
            .collect()
    }

    /// Update resource metrics
    pub async fn update_metrics(
        &self,
        active_connections: usize,
        message_queue_size: usize,
        bytes_received: u64,
        bytes_sent: u64,
    ) {
        let mut metrics = self.resource_metrics.lock().await;
        let now = current_timestamp();

        metrics.active_connections = active_connections;
        metrics.message_queue_size = message_queue_size;
        metrics.bytes_received = bytes_received;
        metrics.bytes_sent = bytes_sent;
        metrics.last_update = now;
    }

    /// Get current resource metrics
    pub async fn get_metrics(&self) -> ResourceMetrics {
        self.resource_metrics.lock().await.clone()
    }

    /// Check if we're under DoS attack (heuristic)
    pub async fn detect_dos_attack(&self) -> bool {
        let metrics = self.resource_metrics.lock().await;

        // Heuristic: If message queue is > 80% full and connections are > 80% of max
        let queue_threshold = (self.max_message_queue_size as f64 * 0.8) as usize;
        let conn_threshold = (self.max_active_connections as f64 * 0.8) as usize;

        metrics.message_queue_size > queue_threshold && metrics.active_connections > conn_threshold
    }

    /// Cleanup old connection rate limiter entries
    pub async fn cleanup(&self) {
        let mut limiter = self.connection_rate_limiter.lock().await;
        limiter.cleanup();
    }

    /// Get connection attempt count for an IP
    pub async fn get_connection_attempts(&self, ip: IpAddr) -> usize {
        let limiter = self.connection_rate_limiter.lock().await;
        limiter.get_attempt_count(ip)
    }

    /// Check if an IP should be auto-banned
    pub async fn should_auto_ban(&self, ip: IpAddr) -> bool {
        let violations = self.connection_violations.lock().await;
        violations.get(&ip).copied().unwrap_or(0) >= self.auto_ban_connection_violations
    }

    /// Get DoS protection metrics
    pub async fn get_dos_metrics(&self) -> DosProtectionMetrics {
        self.metrics.lock().await.clone()
    }

    /// Get configuration (limits and thresholds)
    pub async fn get_config(&self) -> DosProtectionConfig {
        let limiter = self.connection_rate_limiter.lock().await;
        DosProtectionConfig {
            max_connections_per_window: limiter.max_connections_per_window,
            window_seconds: limiter.window_seconds,
            max_message_queue_size: self.max_message_queue_size,
            max_active_connections: self.max_active_connections,
            auto_ban_connection_violations: self.auto_ban_connection_violations,
        }
    }
}

/// DoS protection configuration
#[derive(Debug, Clone)]
pub struct DosProtectionConfig {
    pub max_connections_per_window: usize,
    pub window_seconds: u64,
    pub max_message_queue_size: usize,
    pub max_active_connections: usize,
    pub auto_ban_connection_violations: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_connection_rate_limiting() {
        let dos = DosProtectionManager::new(5, 60, 1000, 100);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();

        // Should allow 5 connections
        for _ in 0..5 {
            assert!(dos.check_connection(ip).await);
        }

        // 6th should be rejected
        assert!(!dos.check_connection(ip).await);
    }

    #[tokio::test]
    async fn test_message_queue_limit() {
        let dos = DosProtectionManager::new(10, 60, 100, 50);

        assert!(dos.check_message_queue_size(50).await);
        assert!(dos.check_message_queue_size(100).await);
        assert!(!dos.check_message_queue_size(101).await);
    }

    #[tokio::test]
    async fn test_active_connection_limit() {
        let dos = DosProtectionManager::new(10, 60, 100, 50);

        assert!(dos.check_active_connections(49).await); // Below limit, should accept
        assert!(!dos.check_active_connections(50).await); // At limit, should reject
        assert!(!dos.check_active_connections(51).await); // Above limit, should reject
    }

    #[tokio::test]
    async fn test_auto_ban_after_violations() {
        let dos = DosProtectionManager::new(2, 60, 1000, 100);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();

        // Exceed limit 3 times (should trigger auto-ban)
        for _ in 0..3 {
            dos.check_connection(ip).await; // First 2 allowed
            dos.check_connection(ip).await; // 3rd rejected
            dos.check_connection(ip).await; // 4th rejected (violation)
        }

        assert!(dos.should_auto_ban(ip).await);
    }
}
