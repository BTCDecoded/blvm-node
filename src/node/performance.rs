//! Performance monitoring and profiling infrastructure
//!
//! Provides performance tracking, profiling hooks, and performance metrics collection.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};

/// Performance profiler for tracking operation timings
pub struct PerformanceProfiler {
    /// Block processing times
    block_processing_times: Arc<Mutex<Vec<Duration>>>,
    /// Transaction validation times
    tx_validation_times: Arc<Mutex<Vec<Duration>>>,
    /// Storage operation times
    storage_operation_times: Arc<Mutex<Vec<Duration>>>,
    /// Network operation times
    network_operation_times: Arc<Mutex<Vec<Duration>>>,
    /// Maximum samples to keep per operation type
    max_samples: usize,
}

impl PerformanceProfiler {
    /// Create a new performance profiler
    pub fn new(max_samples: usize) -> Self {
        Self {
            block_processing_times: Arc::new(Mutex::new(Vec::new())),
            tx_validation_times: Arc::new(Mutex::new(Vec::new())),
            storage_operation_times: Arc::new(Mutex::new(Vec::new())),
            network_operation_times: Arc::new(Mutex::new(Vec::new())),
            max_samples,
        }
    }

    /// Record block processing time
    pub fn record_block_processing(&self, duration: Duration) {
        let mut times = self.block_processing_times.lock().unwrap();
        times.push(duration);
        if times.len() > self.max_samples {
            times.remove(0);
        }
    }

    /// Record transaction validation time
    pub fn record_tx_validation(&self, duration: Duration) {
        let mut times = self.tx_validation_times.lock().unwrap();
        times.push(duration);
        if times.len() > self.max_samples {
            times.remove(0);
        }
    }

    /// Record storage operation time
    pub fn record_storage_operation(&self, duration: Duration) {
        let mut times = self.storage_operation_times.lock().unwrap();
        times.push(duration);
        if times.len() > self.max_samples {
            times.remove(0);
        }
    }

    /// Record network operation time
    pub fn record_network_operation(&self, duration: Duration) {
        let mut times = self.network_operation_times.lock().unwrap();
        times.push(duration);
        if times.len() > self.max_samples {
            times.remove(0);
        }
    }

    /// Get performance statistics
    pub fn get_stats(&self) -> PerformanceStats {
        PerformanceStats {
            block_processing: self.calculate_stats(&self.block_processing_times.lock().unwrap()),
            tx_validation: self.calculate_stats(&self.tx_validation_times.lock().unwrap()),
            storage_operations: self.calculate_stats(&self.storage_operation_times.lock().unwrap()),
            network_operations: self.calculate_stats(&self.network_operation_times.lock().unwrap()),
        }
    }

    fn calculate_stats(&self, times: &[Duration]) -> OperationStats {
        if times.is_empty() {
            return OperationStats::default();
        }

        let total: Duration = times.iter().sum();
        let count = times.len();
        let avg = total / count as u32;
        
        let mut sorted = times.to_vec();
        sorted.sort();
        
        let p50 = sorted[count / 2];
        let p95 = sorted[(count * 95) / 100];
        let p99 = sorted[(count * 99) / 100];
        let min = sorted[0];
        let max = sorted[count - 1];

        OperationStats {
            count,
            avg_ms: avg.as_secs_f64() * 1000.0,
            p50_ms: p50.as_secs_f64() * 1000.0,
            p95_ms: p95.as_secs_f64() * 1000.0,
            p99_ms: p99.as_secs_f64() * 1000.0,
            min_ms: min.as_secs_f64() * 1000.0,
            max_ms: max.as_secs_f64() * 1000.0,
        }
    }
}

/// Performance statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceStats {
    pub block_processing: OperationStats,
    pub tx_validation: OperationStats,
    pub storage_operations: OperationStats,
    pub network_operations: OperationStats,
}

/// Statistics for a single operation type
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OperationStats {
    /// Number of samples
    pub count: usize,
    /// Average time (milliseconds)
    pub avg_ms: f64,
    /// 50th percentile (median) time (milliseconds)
    pub p50_ms: f64,
    /// 95th percentile time (milliseconds)
    pub p95_ms: f64,
    /// 99th percentile time (milliseconds)
    pub p99_ms: f64,
    /// Minimum time (milliseconds)
    pub min_ms: f64,
    /// Maximum time (milliseconds)
    pub max_ms: f64,
}

/// Performance timer for measuring operation duration
pub struct PerformanceTimer {
    start: Instant,
    profiler: Arc<PerformanceProfiler>,
    operation_type: OperationType,
}

/// Operation type for profiling
#[derive(Debug, Clone, Copy)]
pub enum OperationType {
    BlockProcessing,
    TxValidation,
    StorageOperation,
    NetworkOperation,
}

impl PerformanceTimer {
    /// Start a new performance timer
    pub fn start(profiler: Arc<PerformanceProfiler>, operation_type: OperationType) -> Self {
        Self {
            start: Instant::now(),
            profiler,
            operation_type,
        }
    }

    /// Stop the timer and record the duration
    pub fn stop(self) -> Duration {
        let duration = self.start.elapsed();
        
        match self.operation_type {
            OperationType::BlockProcessing => {
                self.profiler.record_block_processing(duration);
            }
            OperationType::TxValidation => {
                self.profiler.record_tx_validation(duration);
            }
            OperationType::StorageOperation => {
                self.profiler.record_storage_operation(duration);
            }
            OperationType::NetworkOperation => {
                self.profiler.record_network_operation(duration);
            }
        }
        
        duration
    }
}

impl Default for PerformanceProfiler {
    fn default() -> Self {
        Self::new(1000) // Keep last 1000 samples
    }
}

