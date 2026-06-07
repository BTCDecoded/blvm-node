//! `MetricsManager` report, query, clear, and Prometheus formatting.

use blvm_node::module::metrics::manager::{Metric, MetricValue, MetricsManager};
use std::collections::HashMap;

#[tokio::test]
async fn metrics_manager_report_update_and_clear() {
    let mgr = MetricsManager::new();
    let labels = HashMap::from([("env".into(), "test".into())]);

    mgr.report_metric(
        "mod-a".into(),
        Metric {
            name: "requests".into(),
            value: MetricValue::Counter(1),
            labels: labels.clone(),
            timestamp: 1,
        },
    )
    .await;
    mgr.report_metric(
        "mod-a".into(),
        Metric {
            name: "requests".into(),
            value: MetricValue::Counter(2),
            labels: labels.clone(),
            timestamp: 2,
        },
    )
    .await;

    let mod_metrics = mgr.get_module_metrics("mod-a").await;
    assert_eq!(mod_metrics.len(), 1);
    assert!(matches!(mod_metrics[0].value, MetricValue::Counter(2)));

    mgr.report_metric(
        "mod-b".into(),
        Metric {
            name: "latency".into(),
            value: MetricValue::Gauge(0.5),
            labels: HashMap::new(),
            timestamp: 3,
        },
    )
    .await;

    let all = mgr.get_all_metrics().await;
    assert_eq!(all.len(), 2);

    mgr.clear_module_metrics("mod-a").await;
    assert!(mgr.get_module_metrics("mod-a").await.is_empty());
}

#[tokio::test]
async fn metrics_manager_format_prometheus() {
    let mgr = MetricsManager::new();
    mgr.report_metric(
        "mod-p".into(),
        Metric {
            name: "depth".into(),
            value: MetricValue::Histogram(vec![1.0, 2.0]),
            labels: HashMap::new(),
            timestamp: 0,
        },
    )
    .await;

    let text = mgr.format_prometheus().await;
    assert!(text.contains("blvm_module_depth"));
    assert!(text.contains("histogram"));
    assert!(text.contains("module_id=\"mod-p\""));
}
