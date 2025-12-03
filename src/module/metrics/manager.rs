//! Metrics manager for module metrics

use std::collections::HashMap;
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use tracing::debug;

/// Metric value types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetricValue {
    Counter(u64),
    Gauge(f64),
    Histogram(Vec<f64>),
}

/// Metric reported by a module
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    pub value: MetricValue,
    pub labels: HashMap<String, String>,
    pub timestamp: u64,
}

/// Metrics manager for modules
pub struct MetricsManager {
    /// Module metrics (module_id -> metrics)
    module_metrics: Arc<tokio::sync::RwLock<HashMap<String, Vec<Metric>>>>,
}

impl MetricsManager {
    /// Create a new metrics manager
    pub fn new() -> Self {
        Self {
            module_metrics: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }
    
    /// Report a metric from a module
    pub async fn report_metric(&self, module_id: String, metric: Metric) {
        let mut metrics = self.module_metrics.write().await;
        let module_metrics = metrics.entry(module_id.clone()).or_insert_with(Vec::new);
        
        // Update or add metric (by name and labels)
        let metric_name = metric.name.clone();
        let metric_key = format!("{}_{:?}", metric.name, metric.labels);
        if let Some(existing) = module_metrics.iter_mut().find(|m| {
            format!("{}_{:?}", m.name, m.labels) == metric_key
        }) {
            *existing = metric;
        } else {
            module_metrics.push(metric);
        }
        
        debug!("Module {} reported metric: {}", module_id, metric_name);
    }
    
    /// Get all metrics for a module
    pub async fn get_module_metrics(&self, module_id: &str) -> Vec<Metric> {
        let metrics = self.module_metrics.read().await;
        metrics.get(module_id).cloned().unwrap_or_default()
    }
    
    /// Get all metrics from all modules
    pub async fn get_all_metrics(&self) -> HashMap<String, Vec<Metric>> {
        let metrics = self.module_metrics.read().await;
        metrics.clone()
    }
    
    /// Clear metrics for a module (on module shutdown)
    pub async fn clear_module_metrics(&self, module_id: &str) {
        let mut metrics = self.module_metrics.write().await;
        metrics.remove(module_id);
        debug!("Cleared metrics for module {}", module_id);
    }
    
    /// Format metrics as Prometheus text format
    pub async fn format_prometheus(&self) -> String {
        let metrics = self.module_metrics.read().await;
        let mut output = String::new();
        
        for (module_id, module_metrics) in metrics.iter() {
            for metric in module_metrics {
                let metric_name = format!("blvm_module_{}", metric.name.replace('-', "_"));
                let labels_str = if metric.labels.is_empty() {
                    format!("module_id=\"{}\"", module_id)
                } else {
                    let mut label_parts = vec![format!("module_id=\"{}\"", module_id)];
                    for (key, value) in &metric.labels {
                        label_parts.push(format!("{}=\"{}\"", key.replace('-', "_"), value));
                    }
                    label_parts.join(",")
                };
                
                match &metric.value {
                    MetricValue::Counter(value) => {
                        output.push_str(&format!(
                            "# HELP {} Module metric counter\n",
                            metric_name
                        ));
                        output.push_str(&format!(
                            "# TYPE {} counter\n",
                            metric_name
                        ));
                        output.push_str(&format!(
                            "{}{{{}}} {}\n",
                            metric_name, labels_str, value
                        ));
                    }
                    MetricValue::Gauge(value) => {
                        output.push_str(&format!(
                            "# HELP {} Module metric gauge\n",
                            metric_name
                        ));
                        output.push_str(&format!(
                            "# TYPE {} gauge\n",
                            metric_name
                        ));
                        output.push_str(&format!(
                            "{}{{{}}} {}\n",
                            metric_name, labels_str, value
                        ));
                    }
                    MetricValue::Histogram(buckets) => {
                        output.push_str(&format!(
                            "# HELP {} Module metric histogram\n",
                            metric_name
                        ));
                        output.push_str(&format!(
                            "# TYPE {} histogram\n",
                            metric_name
                        ));
                        for (i, bucket_value) in buckets.iter().enumerate() {
                            output.push_str(&format!(
                                "{}{{{},le=\"{}\"}} {}\n",
                                metric_name, labels_str, i, bucket_value
                            ));
                        }
                    }
                }
            }
        }
        
        output
    }
}

impl Default for MetricsManager {
    fn default() -> Self {
        Self::new()
    }
}

