//! Node health check system
//!
//! Provides health status monitoring and alerting for node components.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Overall node health status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum HealthStatus {
    /// Node is healthy and operating normally
    Healthy,
    /// Node is degraded but functional
    Degraded,
    /// Node is unhealthy and may not function correctly
    Unhealthy,
    /// Node is down or not responding
    Down,
}

/// Component health check result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentHealth {
    /// Component name
    pub component: String,
    /// Health status
    pub status: HealthStatus,
    /// Optional message describing the status
    pub message: Option<String>,
    /// Timestamp of last check
    pub last_check: u64,
    /// Response time in milliseconds (if applicable)
    pub response_time_ms: Option<f64>,
}

/// Comprehensive node health report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    /// Overall health status
    pub overall_status: HealthStatus,
    /// Individual component health
    pub components: Vec<ComponentHealth>,
    /// Timestamp of report generation
    pub timestamp: u64,
    /// Uptime in seconds
    pub uptime_seconds: u64,
}

/// Health checker
pub struct HealthChecker {
    /// Start time for uptime calculation
    start_time: SystemTime,
}

impl HealthChecker {
    /// Create a new health checker
    pub fn new() -> Self {
        Self {
            start_time: SystemTime::now(),
        }
    }

    /// Perform comprehensive health check
    pub fn check_health(
        &self,
        network_healthy: bool,
        storage_healthy: bool,
        rpc_healthy: bool,
        network_metrics: Option<&crate::node::metrics::NetworkMetrics>,
        storage_metrics: Option<&crate::node::metrics::StorageMetrics>,
    ) -> HealthReport {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let uptime = SystemTime::now()
            .duration_since(self.start_time)
            .unwrap()
            .as_secs();

        let mut components = Vec::new();

        // Check network component
        let network_status = if network_healthy {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unhealthy
        };
        components.push(ComponentHealth {
            component: "network".to_string(),
            status: network_status.clone(),
            message: network_metrics.map(|m| {
                format!(
                    "Peers: {}, Connections: {}, Banned: {}",
                    m.peer_count, m.active_connections, m.banned_peers
                )
            }),
            last_check: timestamp,
            response_time_ms: None,
        });

        // Check storage component
        let storage_status = if storage_healthy {
            if let Some(metrics) = storage_metrics {
                if metrics.within_bounds {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Degraded
                }
            } else {
                HealthStatus::Healthy
            }
        } else {
            HealthStatus::Unhealthy
        };
        components.push(ComponentHealth {
            component: "storage".to_string(),
            status: storage_status.clone(),
            message: storage_metrics.map(|m| {
                format!(
                    "Blocks: {}, UTXOs: {}, Disk: {} bytes",
                    m.block_count, m.utxo_count, m.disk_size
                )
            }),
            last_check: timestamp,
            response_time_ms: None,
        });

        // Check RPC component
        let rpc_status = if rpc_healthy {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unhealthy
        };
        components.push(ComponentHealth {
            component: "rpc".to_string(),
            status: rpc_status.clone(),
            message: None,
            last_check: timestamp,
            response_time_ms: None,
        });

        // Determine overall status
        let overall_status = if components.iter().any(|c| c.status == HealthStatus::Down) {
            HealthStatus::Down
        } else if components
            .iter()
            .any(|c| c.status == HealthStatus::Unhealthy)
        {
            HealthStatus::Unhealthy
        } else if components
            .iter()
            .any(|c| c.status == HealthStatus::Degraded)
        {
            HealthStatus::Degraded
        } else {
            HealthStatus::Healthy
        };

        HealthReport {
            overall_status,
            components,
            timestamp,
            uptime_seconds: uptime,
        }
    }

    /// Quick health check (returns overall status only)
    pub fn quick_check(
        &self,
        network_healthy: bool,
        storage_healthy: bool,
        rpc_healthy: bool,
    ) -> HealthStatus {
        if !network_healthy || !storage_healthy || !rpc_healthy {
            HealthStatus::Unhealthy
        } else {
            HealthStatus::Healthy
        }
    }
}

impl Default for HealthChecker {
    fn default() -> Self {
        Self::new()
    }
}
