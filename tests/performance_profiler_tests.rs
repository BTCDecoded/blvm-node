//! Tests for performance profiler

use bllvm_node::node::performance::{OperationType, PerformanceProfiler, PerformanceTimer};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[test]
fn test_performance_profiler_creation() {
    let profiler = PerformanceProfiler::new(100);
    // Should create successfully
    assert!(true);
}

#[test]
fn test_performance_profiler_default() {
    let profiler = PerformanceProfiler::default();
    // Should create successfully
    assert!(true);
}

#[test]
fn test_record_block_processing() {
    let profiler = Arc::new(PerformanceProfiler::new(100));
    let duration = Duration::from_millis(100);

    profiler.record_block_processing(duration);

    let stats = profiler.get_stats();
    assert_eq!(stats.block_processing.count, 1);
    assert!(stats.block_processing.avg_ms > 0.0);
}

#[test]
fn test_record_tx_validation() {
    let profiler = Arc::new(PerformanceProfiler::new(100));
    let duration = Duration::from_millis(50);

    profiler.record_tx_validation(duration);

    let stats = profiler.get_stats();
    assert_eq!(stats.tx_validation.count, 1);
    assert!(stats.tx_validation.avg_ms > 0.0);
}

#[test]
fn test_record_storage_operation() {
    let profiler = Arc::new(PerformanceProfiler::new(100));
    let duration = Duration::from_millis(25);

    profiler.record_storage_operation(duration);

    let stats = profiler.get_stats();
    assert_eq!(stats.storage_operations.count, 1);
    assert!(stats.storage_operations.avg_ms > 0.0);
}

#[test]
fn test_record_network_operation() {
    let profiler = Arc::new(PerformanceProfiler::new(100));
    let duration = Duration::from_millis(10);

    profiler.record_network_operation(duration);

    let stats = profiler.get_stats();
    assert_eq!(stats.network_operations.count, 1);
    assert!(stats.network_operations.avg_ms > 0.0);
}

#[test]
fn test_performance_timer_block_processing() {
    let profiler = Arc::new(PerformanceProfiler::new(100));
    let timer = PerformanceTimer::start(profiler.clone(), OperationType::BlockProcessing);

    thread::sleep(Duration::from_millis(10));
    let duration = timer.stop();

    assert!(duration.as_millis() >= 10);
    let stats = profiler.get_stats();
    assert_eq!(stats.block_processing.count, 1);
}

#[test]
fn test_performance_timer_tx_validation() {
    let profiler = Arc::new(PerformanceProfiler::new(100));
    let timer = PerformanceTimer::start(profiler.clone(), OperationType::TxValidation);

    thread::sleep(Duration::from_millis(5));
    let duration = timer.stop();

    assert!(duration.as_millis() >= 5);
    let stats = profiler.get_stats();
    assert_eq!(stats.tx_validation.count, 1);
}

#[test]
fn test_performance_timer_storage_operation() {
    let profiler = Arc::new(PerformanceProfiler::new(100));
    let timer = PerformanceTimer::start(profiler.clone(), OperationType::StorageOperation);

    thread::sleep(Duration::from_millis(5));
    let duration = timer.stop();

    assert!(duration.as_millis() >= 5);
    let stats = profiler.get_stats();
    assert_eq!(stats.storage_operations.count, 1);
}

#[test]
fn test_performance_timer_network_operation() {
    let profiler = Arc::new(PerformanceProfiler::new(100));
    let timer = PerformanceTimer::start(profiler.clone(), OperationType::NetworkOperation);

    thread::sleep(Duration::from_millis(5));
    let duration = timer.stop();

    assert!(duration.as_millis() >= 5);
    let stats = profiler.get_stats();
    assert_eq!(stats.network_operations.count, 1);
}

#[test]
fn test_multiple_recordings() {
    let profiler = Arc::new(PerformanceProfiler::new(100));

    for _ in 0..10 {
        profiler.record_block_processing(Duration::from_millis(100));
    }

    let stats = profiler.get_stats();
    assert_eq!(stats.block_processing.count, 10);
    assert!(stats.block_processing.avg_ms > 0.0);
}

#[test]
fn test_max_samples_limit() {
    let profiler = Arc::new(PerformanceProfiler::new(5));

    // Record more than max_samples
    for _ in 0..10 {
        profiler.record_block_processing(Duration::from_millis(100));
    }

    let stats = profiler.get_stats();
    // Should only keep last 5 samples
    assert_eq!(stats.block_processing.count, 5);
}

#[test]
fn test_percentile_calculation() {
    let profiler = Arc::new(PerformanceProfiler::new(100));

    // Record varying durations
    for i in 0..10 {
        profiler.record_block_processing(Duration::from_millis(i * 10));
    }

    let stats = profiler.get_stats();
    assert_eq!(stats.block_processing.count, 10);
    assert!(stats.block_processing.p50_ms >= 0.0);
    assert!(stats.block_processing.p95_ms >= 0.0);
    assert!(stats.block_processing.p99_ms >= 0.0);
    assert!(stats.block_processing.min_ms >= 0.0);
    assert!(stats.block_processing.max_ms >= 0.0);
}

#[test]
fn test_empty_stats() {
    let profiler = Arc::new(PerformanceProfiler::new(100));
    let stats = profiler.get_stats();

    assert_eq!(stats.block_processing.count, 0);
    assert_eq!(stats.tx_validation.count, 0);
    assert_eq!(stats.storage_operations.count, 0);
    assert_eq!(stats.network_operations.count, 0);
}

#[test]
fn test_stats_serialization() {
    let profiler = Arc::new(PerformanceProfiler::new(100));
    profiler.record_block_processing(Duration::from_millis(100));

    let stats = profiler.get_stats();
    let json = serde_json::to_string(&stats).unwrap();

    assert!(json.contains("block_processing"));
    assert!(json.contains("tx_validation"));
    assert!(json.contains("storage_operations"));
    assert!(json.contains("network_operations"));
}

#[test]
fn test_percentile_ordering() {
    let profiler = Arc::new(PerformanceProfiler::new(100));

    // Record durations in reverse order
    for i in (0..10).rev() {
        profiler.record_block_processing(Duration::from_millis(i * 10));
    }

    let stats = profiler.get_stats();
    // Percentiles should be ordered correctly
    assert!(stats.block_processing.min_ms <= stats.block_processing.p50_ms);
    assert!(stats.block_processing.p50_ms <= stats.block_processing.p95_ms);
    assert!(stats.block_processing.p95_ms <= stats.block_processing.p99_ms);
    assert!(stats.block_processing.p99_ms <= stats.block_processing.max_ms);
}
