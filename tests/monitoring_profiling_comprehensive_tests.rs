//! Comprehensive monitoring and profiling tests
//!
//! Tests for health checks, performance profiling, and metrics collection
//! covering various scenarios and edge cases.

use bllvm_node::node::health::{HealthChecker, HealthStatus};
use bllvm_node::node::metrics::{MetricsCollector, NetworkMetrics, StorageMetrics};
use bllvm_node::node::performance::{OperationType, PerformanceProfiler};
use std::time::Duration;

#[test]
fn test_health_checker_creation() {
    let checker = HealthChecker::new();
    let report = checker.check_health(true, true, true, None, None);

    assert_eq!(report.overall_status, HealthStatus::Healthy);
    assert!(report.uptime_seconds >= 0);
    assert!(!report.components.is_empty());
}

#[test]
fn test_health_checker_all_healthy() {
    let checker = HealthChecker::new();
    let report = checker.check_health(true, true, true, None, None);

    assert_eq!(report.overall_status, HealthStatus::Healthy);
    assert_eq!(report.components.len(), 3); // network, storage, rpc

    for component in &report.components {
        assert_eq!(component.status, HealthStatus::Healthy);
    }
}

#[test]
fn test_health_checker_network_unhealthy() {
    let checker = HealthChecker::new();
    let report = checker.check_health(false, true, true, None, None);

    assert_eq!(report.overall_status, HealthStatus::Unhealthy);

    let network_component = report
        .components
        .iter()
        .find(|c| c.component == "network")
        .unwrap();
    assert_eq!(network_component.status, HealthStatus::Unhealthy);
}

#[test]
fn test_health_checker_storage_unhealthy() {
    let checker = HealthChecker::new();
    let report = checker.check_health(true, false, true, None, None);

    assert_eq!(report.overall_status, HealthStatus::Unhealthy);

    let storage_component = report
        .components
        .iter()
        .find(|c| c.component == "storage")
        .unwrap();
    assert_eq!(storage_component.status, HealthStatus::Unhealthy);
}

#[test]
fn test_health_checker_rpc_unhealthy() {
    let checker = HealthChecker::new();
    let report = checker.check_health(true, true, false, None, None);

    assert_eq!(report.overall_status, HealthStatus::Unhealthy);

    let rpc_component = report
        .components
        .iter()
        .find(|c| c.component == "rpc")
        .unwrap();
    assert_eq!(rpc_component.status, HealthStatus::Unhealthy);
}

#[test]
fn test_health_checker_with_network_metrics() {
    let checker = HealthChecker::new();
    let network_metrics = NetworkMetrics {
        peer_count: 10,
        active_connections: 8,
        banned_peers: 2,
        ..Default::default()
    };

    let report = checker.check_health(true, true, true, Some(&network_metrics), None);

    assert_eq!(report.overall_status, HealthStatus::Healthy);

    let network_component = report
        .components
        .iter()
        .find(|c| c.component == "network")
        .unwrap();
    assert!(network_component.message.is_some());
    assert!(network_component
        .message
        .as_ref()
        .unwrap()
        .contains("Peers: 10"));
}

#[test]
fn test_health_checker_with_storage_metrics() {
    let checker = HealthChecker::new();
    let storage_metrics = StorageMetrics {
        block_count: 1000,
        utxo_count: 50000,
        within_bounds: true,
        ..Default::default()
    };

    let report = checker.check_health(true, true, true, None, Some(&storage_metrics));

    assert_eq!(report.overall_status, HealthStatus::Healthy);

    let storage_component = report
        .components
        .iter()
        .find(|c| c.component == "storage")
        .unwrap();
    assert!(storage_component.message.is_some());
    assert!(storage_component
        .message
        .as_ref()
        .unwrap()
        .contains("Blocks: 1000"));
}

#[test]
fn test_health_checker_uptime_tracking() {
    let checker = HealthChecker::new();

    // First check
    let report1 = checker.check_health(true, true, true, None, None);
    let uptime1 = report1.uptime_seconds;

    // Wait a bit (simulated)
    std::thread::sleep(Duration::from_millis(100));

    // Second check
    let report2 = checker.check_health(true, true, true, None, None);
    let uptime2 = report2.uptime_seconds;

    // Uptime should increase
    assert!(uptime2 >= uptime1);
}

#[test]
fn test_performance_profiler_creation() {
    let profiler = PerformanceProfiler::new(1000);
    let stats = profiler.get_stats();

    // Initially empty stats
    assert_eq!(stats.block_processing.count, 0);
    assert_eq!(stats.tx_validation.count, 0);
}

#[test]
fn test_performance_profiler_block_processing() {
    let profiler = PerformanceProfiler::new(1000);

    // Record some block processing times
    profiler.record_block_processing(Duration::from_millis(100));
    profiler.record_block_processing(Duration::from_millis(200));
    profiler.record_block_processing(Duration::from_millis(150));

    let stats = profiler.get_stats();
    assert_eq!(stats.block_processing.count, 3);
    assert!(stats.block_processing.avg_ms > 0.0);
}

#[test]
fn test_performance_profiler_tx_validation() {
    let profiler = PerformanceProfiler::new(1000);

    // Record some transaction validation times
    profiler.record_tx_validation(Duration::from_micros(500));
    profiler.record_tx_validation(Duration::from_micros(600));
    profiler.record_tx_validation(Duration::from_micros(550));

    let stats = profiler.get_stats();
    assert_eq!(stats.tx_validation.count, 3);
    assert!(stats.tx_validation.avg_ms > 0.0);
}

#[test]
fn test_performance_profiler_storage_operations() {
    let profiler = PerformanceProfiler::new(1000);

    // Record some storage operation times
    profiler.record_storage_operation(Duration::from_millis(50));
    profiler.record_storage_operation(Duration::from_millis(60));

    let stats = profiler.get_stats();
    assert_eq!(stats.storage_operations.count, 2);
}

#[test]
fn test_performance_profiler_network_operations() {
    let profiler = PerformanceProfiler::new(1000);

    // Record some network operation times
    profiler.record_network_operation(Duration::from_millis(10));
    profiler.record_network_operation(Duration::from_millis(20));
    profiler.record_network_operation(Duration::from_millis(15));

    let stats = profiler.get_stats();
    assert_eq!(stats.network_operations.count, 3);
}

#[test]
fn test_performance_profiler_max_samples() {
    let profiler = PerformanceProfiler::new(5);

    // Record more than max_samples
    for i in 0..10 {
        profiler.record_block_processing(Duration::from_millis(i as u64));
    }

    let stats = profiler.get_stats();
    // Should only keep max_samples (5)
    assert_eq!(stats.block_processing.count, 5);
}

#[test]
fn test_performance_profiler_percentiles() {
    let profiler = PerformanceProfiler::new(100);

    // Record times from 1ms to 100ms
    for i in 1..=100 {
        profiler.record_block_processing(Duration::from_millis(i));
    }

    let stats = profiler.get_stats();
    assert_eq!(stats.block_processing.count, 100);
    assert!(stats.block_processing.p50_ms > 0.0);
    assert!(stats.block_processing.p95_ms > 0.0);
    assert!(stats.block_processing.p99_ms > 0.0);
    assert_eq!(stats.block_processing.min_ms, 1.0);
    assert_eq!(stats.block_processing.max_ms, 100.0);
}

#[test]
fn test_metrics_collector_creation() {
    let collector = MetricsCollector::new();
    let metrics = collector.collect();

    assert!(metrics.timestamp > 0);
    assert_eq!(metrics.network.peer_count, 0);
    assert_eq!(metrics.storage.block_count, 0);
}

#[test]
fn test_metrics_collector_update_network() {
    let collector = MetricsCollector::new();

    collector.update_network(|m| {
        m.peer_count = 10;
        m.active_connections = 8;
        m.bytes_sent = 1000000;
        m.bytes_received = 2000000;
    });

    let metrics = collector.collect();
    assert_eq!(metrics.network.peer_count, 10);
    assert_eq!(metrics.network.active_connections, 8);
    assert_eq!(metrics.network.bytes_sent, 1000000);
    assert_eq!(metrics.network.bytes_received, 2000000);
}

#[test]
fn test_metrics_collector_update_storage() {
    let collector = MetricsCollector::new();

    collector.update_storage(|m| {
        m.block_count = 1000;
        m.utxo_count = 50000;
        m.transaction_count = 100000;
        m.disk_size = 5000000000; // 5GB
    });

    let metrics = collector.collect();
    assert_eq!(metrics.storage.block_count, 1000);
    assert_eq!(metrics.storage.utxo_count, 50000);
    assert_eq!(metrics.storage.transaction_count, 100000);
    assert_eq!(metrics.storage.disk_size, 5000000000);
}

#[test]
fn test_metrics_collector_update_rpc() {
    let collector = MetricsCollector::new();

    collector.update_rpc(|m| {
        m.requests_total = 1000;
        m.requests_success = 950;
        m.requests_failed = 50;
    });

    let metrics = collector.collect();
    assert_eq!(metrics.rpc.requests_total, 1000);
    assert_eq!(metrics.rpc.requests_success, 950);
    assert_eq!(metrics.rpc.requests_failed, 50);
}

#[test]
fn test_metrics_collector_timestamp() {
    let collector = MetricsCollector::new();

    let metrics1 = collector.collect();
    std::thread::sleep(Duration::from_millis(10));
    let metrics2 = collector.collect();

    // Timestamp should update
    assert!(metrics2.timestamp >= metrics1.timestamp);
}

#[test]
fn test_health_checker_degraded_scenario() {
    let checker = HealthChecker::new();

    // One component unhealthy should make overall unhealthy
    let report = checker.check_health(false, true, true, None, None);
    assert_eq!(report.overall_status, HealthStatus::Unhealthy);
}

#[test]
fn test_performance_profiler_empty_stats() {
    let profiler = PerformanceProfiler::new(1000);
    let stats = profiler.get_stats();

    // Empty stats should have count 0
    assert_eq!(stats.block_processing.count, 0);
    assert_eq!(stats.tx_validation.count, 0);
    assert_eq!(stats.storage_operations.count, 0);
    assert_eq!(stats.network_operations.count, 0);
}

#[test]
fn test_metrics_collector_concurrent_updates() {
    use std::sync::Arc;
    use std::thread;

    let collector = Arc::new(MetricsCollector::new());
    let mut handles = vec![];

    // Spawn multiple threads updating metrics
    for i in 0..10 {
        let collector_clone = Arc::clone(&collector);
        let handle = thread::spawn(move || {
            collector_clone.update_network(|m| {
                m.peer_count = i;
            });
        });
        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().unwrap();
    }

    // Should have some value (last write wins)
    let metrics = collector.collect();
    assert!(metrics.network.peer_count < 10);
}
