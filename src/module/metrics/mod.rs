//! Metrics and telemetry system for modules
//!
//! Allows modules to report metrics that integrate with the node's metrics system.

pub mod manager;

pub use manager::MetricsManager;

