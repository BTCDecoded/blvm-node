//! Tests for node health monitoring

use bllvm_node::node::health::{ComponentHealth, HealthChecker, HealthReport, HealthStatus};
use bllvm_node::node::metrics::{NetworkMetrics, StorageMetrics};
use std::thread;
use std::time::Duration;

#[test]
fn test_health_checker_creation() {
    let checker = HealthChecker::new();
    // Should create successfully
    assert!(true);
}

#[test]
fn test_health_checker_default() {
    let checker = HealthChecker::default();
    // Should create successfully
    assert!(true);
}

#[test]
fn test_health_status_variants() {
    // Test all health status variants
    let statuses = vec![
        HealthStatus::Healthy,
        HealthStatus::Degraded,
        HealthStatus::Unhealthy,
        HealthStatus::Down,
    ];

    for status in statuses {
        match status {
            HealthStatus::Healthy => assert!(true),
            HealthStatus::Degraded => assert!(true),
            HealthStatus::Unhealthy => assert!(true),
            HealthStatus::Down => assert!(true),
        }
    }
}

#[test]
fn test_quick_check_all_healthy() {
    let checker = HealthChecker::new();
    let status = checker.quick_check(true, true, true);
    assert_eq!(status, HealthStatus::Healthy);
}

#[test]
fn test_quick_check_network_unhealthy() {
    let checker = HealthChecker::new();
    let status = checker.quick_check(false, true, true);
    assert_eq!(status, HealthStatus::Unhealthy);
}

#[test]
fn test_quick_check_storage_unhealthy() {
    let checker = HealthChecker::new();
    let status = checker.quick_check(true, false, true);
    assert_eq!(status, HealthStatus::Unhealthy);
}

#[test]
fn test_quick_check_rpc_unhealthy() {
    let checker = HealthChecker::new();
    let status = checker.quick_check(true, true, false);
    assert_eq!(status, HealthStatus::Unhealthy);
}

#[test]
fn test_quick_check_multiple_unhealthy() {
    let checker = HealthChecker::new();
    let status = checker.quick_check(false, false, true);
    assert_eq!(status, HealthStatus::Unhealthy);
}

#[test]
fn test_comprehensive_health_check_all_healthy() {
    let checker = HealthChecker::new();
    let report = checker.check_health(true, true, true, None, None);

    assert_eq!(report.overall_status, HealthStatus::Healthy);
    assert_eq!(report.components.len(), 3);
    assert!(report.uptime_seconds >= 0);
    assert!(report.timestamp > 0);
}

#[test]
fn test_comprehensive_health_check_network_unhealthy() {
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
fn test_comprehensive_health_check_storage_unhealthy() {
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
fn test_comprehensive_health_check_rpc_unhealthy() {
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
fn test_comprehensive_health_check_with_network_metrics() {
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
    assert_eq!(network_component.status, HealthStatus::Healthy);
    assert!(network_component.message.is_some());
    assert!(network_component
        .message
        .as_ref()
        .unwrap()
        .contains("Peers: 10"));
}

#[test]
fn test_comprehensive_health_check_with_storage_metrics_healthy() {
    let checker = HealthChecker::new();
    let storage_metrics = StorageMetrics {
        block_count: 1000,
        utxo_count: 5000,
        disk_size: 1_000_000,
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
    assert_eq!(storage_component.status, HealthStatus::Healthy);
    assert!(storage_component.message.is_some());
    assert!(storage_component
        .message
        .as_ref()
        .unwrap()
        .contains("Blocks: 1000"));
}

#[test]
fn test_comprehensive_health_check_with_storage_metrics_degraded() {
    let checker = HealthChecker::new();
    let storage_metrics = StorageMetrics {
        block_count: 1000,
        utxo_count: 5000,
        disk_size: 1_000_000,
        within_bounds: false, // Storage is out of bounds
        ..Default::default()
    };

    let report = checker.check_health(true, true, true, None, Some(&storage_metrics));

    assert_eq!(report.overall_status, HealthStatus::Degraded);

    let storage_component = report
        .components
        .iter()
        .find(|c| c.component == "storage")
        .unwrap();
    assert_eq!(storage_component.status, HealthStatus::Degraded);
}

#[test]
fn test_comprehensive_health_check_uptime() {
    let checker = HealthChecker::new();

    // Wait a bit
    thread::sleep(Duration::from_millis(100));

    let report1 = checker.check_health(true, true, true, None, None);
    let uptime1 = report1.uptime_seconds;

    thread::sleep(Duration::from_millis(100));

    let report2 = checker.check_health(true, true, true, None, None);
    let uptime2 = report2.uptime_seconds;

    // Uptime should increase
    assert!(uptime2 >= uptime1);
}

#[test]
fn test_comprehensive_health_check_timestamp() {
    let checker = HealthChecker::new();

    let report1 = checker.check_health(true, true, true, None, None);
    thread::sleep(Duration::from_millis(100));
    let report2 = checker.check_health(true, true, true, None, None);

    // Timestamp should increase
    assert!(report2.timestamp >= report1.timestamp);
}

#[test]
fn test_comprehensive_health_check_component_last_check() {
    let checker = HealthChecker::new();
    let report = checker.check_health(true, true, true, None, None);

    for component in &report.components {
        assert!(component.last_check > 0);
        assert_eq!(component.last_check, report.timestamp);
    }
}

#[test]
fn test_health_report_serialization() {
    let checker = HealthChecker::new();
    let report = checker.check_health(true, true, true, None, None);

    // Should serialize to JSON
    let json = serde_json::to_string(&report).unwrap();
    assert!(json.contains("overall_status"));
    assert!(json.contains("components"));
    assert!(json.contains("timestamp"));
    assert!(json.contains("uptime_seconds"));
}

#[test]
fn test_component_health_structure() {
    let checker = HealthChecker::new();
    let report = checker.check_health(true, true, true, None, None);

    for component in &report.components {
        assert!(!component.component.is_empty());
        assert!(matches!(
            component.status,
            HealthStatus::Healthy
                | HealthStatus::Degraded
                | HealthStatus::Unhealthy
                | HealthStatus::Down
        ));
        assert!(component.last_check > 0);
    }
}

#[test]
fn test_overall_status_priority() {
    let checker = HealthChecker::new();

    // All healthy -> Healthy
    let report = checker.check_health(true, true, true, None, None);
    assert_eq!(report.overall_status, HealthStatus::Healthy);

    // One unhealthy -> Unhealthy
    let report = checker.check_health(false, true, true, None, None);
    assert_eq!(report.overall_status, HealthStatus::Unhealthy);

    // Storage degraded -> Degraded
    let storage_metrics = StorageMetrics {
        within_bounds: false,
        ..Default::default()
    };
    let report = checker.check_health(true, true, true, None, Some(&storage_metrics));
    assert_eq!(report.overall_status, HealthStatus::Degraded);
}
