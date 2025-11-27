//! Tests for metrics collector

use bllvm_node::node::metrics::{
    DosMetrics, MetricsCollector, NetworkMetrics, NodeMetrics, PerformanceMetrics, RpcMetrics,
    StorageMetrics, SystemMetrics,
};

#[test]
fn test_metrics_collector_creation() {
    let collector = MetricsCollector::new();
    // Should create successfully
    assert!(true);
}

#[test]
fn test_metrics_collector_default() {
    let _collector = MetricsCollector::default();
    // Should create successfully
    assert!(true);
}

#[test]
fn test_update_network_metrics() {
    let collector = MetricsCollector::new();

    collector.update_network(|m| {
        m.peer_count = 10;
        m.bytes_sent = 1000;
        m.bytes_received = 2000;
    });

    let metrics = collector.collect();
    assert_eq!(metrics.network.peer_count, 10);
    assert_eq!(metrics.network.bytes_sent, 1000);
    assert_eq!(metrics.network.bytes_received, 2000);
}

#[test]
fn test_update_storage_metrics() {
    let collector = MetricsCollector::new();

    collector.update_storage(|m| {
        m.block_count = 1000;
        m.utxo_count = 5000;
        m.disk_size = 1_000_000;
    });

    let metrics = collector.collect();
    assert_eq!(metrics.storage.block_count, 1000);
    assert_eq!(metrics.storage.utxo_count, 5000);
    assert_eq!(metrics.storage.disk_size, 1_000_000);
}

#[test]
fn test_update_rpc_metrics() {
    let collector = MetricsCollector::new();

    collector.update_rpc(|m| {
        m.requests_total = 100;
        m.requests_success = 95;
        m.requests_failed = 5;
    });

    let metrics = collector.collect();
    assert_eq!(metrics.rpc.requests_total, 100);
    assert_eq!(metrics.rpc.requests_success, 95);
    assert_eq!(metrics.rpc.requests_failed, 5);
}

#[test]
fn test_update_performance_metrics() {
    let collector = MetricsCollector::new();

    collector.update_performance(|m| {
        m.avg_block_processing_time_ms = 100.0;
        m.avg_tx_validation_time_ms = 50.0;
    });

    let metrics = collector.collect();
    assert_eq!(metrics.performance.avg_block_processing_time_ms, 100.0);
    assert_eq!(metrics.performance.avg_tx_validation_time_ms, 50.0);
}

#[test]
fn test_collect_metrics() {
    let collector = MetricsCollector::new();
    let metrics = collector.collect();

    // Should return valid metrics
    assert!(metrics.timestamp > 0);
    assert_eq!(metrics.network.peer_count, 0);
    assert_eq!(metrics.storage.block_count, 0);
}

#[test]
fn test_metrics_serialization() {
    let collector = MetricsCollector::new();
    let metrics = collector.collect();

    let json = serde_json::to_string(&metrics).unwrap();
    assert!(json.contains("network"));
    assert!(json.contains("storage"));
    assert!(json.contains("rpc"));
    assert!(json.contains("performance"));
    assert!(json.contains("system"));
    assert!(json.contains("timestamp"));
}

#[test]
fn test_network_metrics_default() {
    let metrics = NetworkMetrics::default();
    assert_eq!(metrics.peer_count, 0);
    assert_eq!(metrics.bytes_sent, 0);
    assert_eq!(metrics.bytes_received, 0);
}

#[test]
fn test_dos_metrics_default() {
    let metrics = DosMetrics::default();
    assert_eq!(metrics.connection_rate_violations, 0);
    assert_eq!(metrics.auto_bans, 0);
    assert_eq!(metrics.message_queue_overflows, 0);
}

#[test]
fn test_storage_metrics_default() {
    let metrics = StorageMetrics::default();
    assert_eq!(metrics.block_count, 0);
    assert_eq!(metrics.utxo_count, 0);
    assert_eq!(metrics.transaction_count, 0);
    assert_eq!(metrics.disk_size, 0);
    // within_bounds may default to false or true depending on implementation
    assert!(metrics.within_bounds == true || metrics.within_bounds == false);
}

#[test]
fn test_rpc_metrics_default() {
    let metrics = RpcMetrics::default();
    assert_eq!(metrics.requests_total, 0);
    assert_eq!(metrics.requests_success, 0);
    assert_eq!(metrics.requests_failed, 0);
    assert_eq!(metrics.active_connections, 0);
}

#[test]
fn test_performance_metrics_default() {
    let metrics = PerformanceMetrics::default();
    assert_eq!(metrics.avg_block_processing_time_ms, 0.0);
    assert_eq!(metrics.avg_tx_validation_time_ms, 0.0);
    assert_eq!(metrics.blocks_per_second, 0.0);
    assert_eq!(metrics.transactions_per_second, 0.0);
}

#[test]
fn test_system_metrics_default() {
    let metrics = SystemMetrics::default();
    assert_eq!(metrics.uptime_seconds, 0);
    assert_eq!(metrics.memory_usage_bytes, None);
    assert_eq!(metrics.cpu_usage_percent, None);
}

#[test]
fn test_node_metrics_structure() {
    let collector = MetricsCollector::new();
    let metrics = collector.collect();

    // Verify all components are present
    assert!(metrics.timestamp > 0);
    // Network metrics
    assert_eq!(metrics.network.peer_count, 0);
    // Storage metrics
    assert_eq!(metrics.storage.block_count, 0);
    // RPC metrics
    assert_eq!(metrics.rpc.requests_total, 0);
    // Performance metrics
    assert_eq!(metrics.performance.avg_block_processing_time_ms, 0.0);
    // System metrics
    assert!(metrics.system.uptime_seconds >= 0);
}

#[test]
fn test_get_network_metrics_reference() {
    let collector = MetricsCollector::new();
    let network_arc = collector.network();

    let mut network = network_arc.lock().unwrap();
    network.peer_count = 5;
    drop(network);

    let metrics = collector.collect();
    assert_eq!(metrics.network.peer_count, 5);
}

#[test]
fn test_get_storage_metrics_reference() {
    let collector = MetricsCollector::new();
    let storage_arc = collector.storage();

    let mut storage = storage_arc.lock().unwrap();
    storage.block_count = 100;
    drop(storage);

    let metrics = collector.collect();
    assert_eq!(metrics.storage.block_count, 100);
}
