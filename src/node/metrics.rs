//! Node metrics collection and reporting
//!
//! Provides comprehensive metrics for monitoring node health, performance, and behavior.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};

/// Comprehensive node metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeMetrics {
    /// Network metrics
    pub network: NetworkMetrics,
    /// Storage metrics
    pub storage: StorageMetrics,
    /// RPC metrics
    pub rpc: RpcMetrics,
    /// Performance metrics
    pub performance: PerformanceMetrics,
    /// System metrics
    pub system: SystemMetrics,
    /// Timestamp when metrics were collected
    pub timestamp: u64,
}

/// Network layer metrics
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkMetrics {
    /// Total peers connected
    pub peer_count: usize,
    /// Total bytes sent
    pub bytes_sent: u64,
    /// Total bytes received
    pub bytes_received: u64,
    /// Messages sent
    pub messages_sent: u64,
    /// Messages received
    pub messages_received: u64,
    /// Active connections
    pub active_connections: usize,
    /// Banned peers count
    pub banned_peers: usize,
    /// Connection attempts (successful)
    pub connection_attempts: u64,
    /// Connection failures
    pub connection_failures: u64,
    /// DoS protection metrics
    pub dos_protection: DosMetrics,
}

/// DoS protection metrics
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DosMetrics {
    /// Connection rate violations
    pub connection_rate_violations: u64,
    /// Auto-bans triggered
    pub auto_bans: u64,
    /// Message queue overflows
    pub message_queue_overflows: u64,
    /// Active connection limit hits
    pub active_connection_limit_hits: u64,
    /// Resource exhaustion events
    pub resource_exhaustion_events: u64,
}

/// Storage layer metrics
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StorageMetrics {
    /// Total blocks stored
    pub block_count: usize,
    /// Total UTXOs
    pub utxo_count: usize,
    /// Total transactions indexed
    pub transaction_count: usize,
    /// Estimated disk size (bytes)
    pub disk_size: u64,
    /// Storage bounds status (true = within bounds)
    pub within_bounds: bool,
    /// Pruning statistics (if pruning enabled)
    pub pruning: Option<PruningMetrics>,
}

/// Pruning metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruningMetrics {
    /// Blocks pruned
    pub blocks_pruned: u64,
    /// Blocks kept
    pub blocks_kept: u64,
    /// Storage freed (bytes)
    pub storage_freed: u64,
    /// Last prune height
    pub last_prune_height: Option<u64>,
}

/// RPC layer metrics
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RpcMetrics {
    /// Total RPC requests
    pub requests_total: u64,
    /// Successful requests
    pub requests_success: u64,
    /// Failed requests
    pub requests_failed: u64,
    /// Requests per second (current)
    pub requests_per_second: f64,
    /// Average response time (ms)
    pub avg_response_time_ms: f64,
    /// Active connections
    pub active_connections: usize,
}

/// Performance metrics
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PerformanceMetrics {
    /// Block processing time (ms, average)
    pub avg_block_processing_time_ms: f64,
    /// Transaction validation time (ms, average)
    pub avg_tx_validation_time_ms: f64,
    /// Blocks processed per second
    pub blocks_per_second: f64,
    /// Transactions processed per second
    pub transactions_per_second: f64,
}

/// System metrics
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemMetrics {
    /// Uptime (seconds)
    pub uptime_seconds: u64,
    /// Memory usage (bytes, if available)
    pub memory_usage_bytes: Option<u64>,
    /// CPU usage percentage (if available)
    pub cpu_usage_percent: Option<f64>,
}

/// Metrics collector
pub struct MetricsCollector {
    /// Network metrics
    network: Arc<Mutex<NetworkMetrics>>,
    /// Storage metrics
    storage: Arc<Mutex<StorageMetrics>>,
    /// RPC metrics
    rpc: Arc<Mutex<RpcMetrics>>,
    /// Performance metrics
    performance: Arc<Mutex<PerformanceMetrics>>,
    /// System metrics
    system: Arc<Mutex<SystemMetrics>>,
    /// Start time for uptime calculation
    start_time: SystemTime,
}

impl MetricsCollector {
    /// Create a new metrics collector
    pub fn new() -> Self {
        Self {
            network: Arc::new(Mutex::new(NetworkMetrics::default())),
            storage: Arc::new(Mutex::new(StorageMetrics::default())),
            rpc: Arc::new(Mutex::new(RpcMetrics::default())),
            performance: Arc::new(Mutex::new(PerformanceMetrics::default())),
            system: Arc::new(Mutex::new(SystemMetrics::default())),
            start_time: SystemTime::now(),
        }
    }

    /// Collect all metrics into a single structure
    pub fn collect(&self) -> NodeMetrics {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let uptime = SystemTime::now()
            .duration_since(self.start_time)
            .unwrap()
            .as_secs();

        let mut system = self.system.lock().unwrap();
        system.uptime_seconds = uptime;

        NodeMetrics {
            network: self.network.lock().unwrap().clone(),
            storage: self.storage.lock().unwrap().clone(),
            rpc: self.rpc.lock().unwrap().clone(),
            performance: self.performance.lock().unwrap().clone(),
            system: system.clone(),
            timestamp,
        }
    }

    /// Update network metrics
    pub fn update_network<F>(&self, f: F)
    where
        F: FnOnce(&mut NetworkMetrics),
    {
        let mut metrics = self.network.lock().unwrap();
        f(&mut metrics);
    }

    /// Update storage metrics
    pub fn update_storage<F>(&self, f: F)
    where
        F: FnOnce(&mut StorageMetrics),
    {
        let mut metrics = self.storage.lock().unwrap();
        f(&mut metrics);
    }

    /// Update RPC metrics
    pub fn update_rpc<F>(&self, f: F)
    where
        F: FnOnce(&mut RpcMetrics),
    {
        let mut metrics = self.rpc.lock().unwrap();
        f(&mut metrics);
    }

    /// Update performance metrics
    pub fn update_performance<F>(&self, f: F)
    where
        F: FnOnce(&mut PerformanceMetrics),
    {
        let mut metrics = self.performance.lock().unwrap();
        f(&mut metrics);
    }

    /// Get network metrics reference
    pub fn network(&self) -> Arc<Mutex<NetworkMetrics>> {
        Arc::clone(&self.network)
    }

    /// Get storage metrics reference
    pub fn storage(&self) -> Arc<Mutex<StorageMetrics>> {
        Arc::clone(&self.storage)
    }

    /// Get RPC metrics reference
    pub fn rpc(&self) -> Arc<Mutex<RpcMetrics>> {
        Arc::clone(&self.rpc)
    }

    /// Get performance metrics reference
    pub fn performance(&self) -> Arc<Mutex<PerformanceMetrics>> {
        Arc::clone(&self.performance)
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

