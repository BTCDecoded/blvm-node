//! Logging Utilities Tests
//!
//! Tests for logging initialization functions.

use blvm_node::config::LoggingConfig;
#[cfg(feature = "json-logging")]
use blvm_node::utils::logging::init_json_logging;
use blvm_node::utils::logging::{init_logging, init_logging_from_config, init_module_logging};

#[test]
fn test_init_logging_default() {
    // Test logging initialization with default settings
    // Note: This test verifies the function doesn't panic
    // Actual logging behavior is hard to test without capturing output

    // Clear RUST_LOG to test default behavior
    std::env::remove_var("RUST_LOG");

    // Should not panic
    init_logging(None);
}

#[test]
fn test_init_logging_with_filter() {
    // Test logging initialization with custom filter
    std::env::remove_var("RUST_LOG");

    // Should not panic
    init_logging(Some("debug"));
}

#[test]
fn test_init_logging_with_rust_log() {
    // Test that RUST_LOG takes precedence
    std::env::set_var("RUST_LOG", "trace");

    // Should not panic
    init_logging(Some("debug")); // Config filter should be ignored

    std::env::remove_var("RUST_LOG");
}

#[test]
fn test_init_module_logging_default() {
    // Test module logging initialization with default settings
    std::env::remove_var("RUST_LOG");

    // Should not panic
    init_module_logging("test_module", None);
}

#[test]
fn test_init_module_logging_with_filter() {
    // Test module logging with custom filter
    std::env::remove_var("RUST_LOG");

    // Should not panic
    init_module_logging("test_module", Some("debug"));
}

#[test]
fn test_init_module_logging_with_rust_log() {
    // Test that RUST_LOG takes precedence for modules too
    std::env::set_var("RUST_LOG", "info");

    // Should not panic
    init_module_logging("test_module", Some("debug"));

    std::env::remove_var("RUST_LOG");
}

// Note: init_json_logging is feature-gated behind "json-logging"
// These tests are skipped if the feature is not enabled
#[cfg(feature = "json-logging")]
mod json_logging_tests {
    use super::*;

    #[test]
    fn test_init_json_logging() {
        // Test JSON logging initialization
        std::env::remove_var("RUST_LOG");

        // Should not panic
        init_json_logging(None);
    }

    #[test]
    fn test_init_json_logging_with_filter() {
        // Test JSON logging with custom filter
        std::env::remove_var("RUST_LOG");

        // Should not panic
        init_json_logging(Some("debug"));
    }
}

#[test]
fn test_init_logging_from_config_none() {
    // Test logging initialization with None config
    std::env::remove_var("RUST_LOG");

    // Should not panic
    init_logging_from_config(None);
}

#[test]
fn test_init_logging_from_config() {
    // Test logging initialization with config
    std::env::remove_var("RUST_LOG");

    let config = LoggingConfig {
        filter: Some("debug".to_string()),
        json_format: false,
    };

    // Should not panic
    init_logging_from_config(Some(&config));
}

#[test]
fn test_logging_respects_no_color() {
    // Test that logging respects NO_COLOR environment variable
    std::env::set_var("NO_COLOR", "1");
    std::env::remove_var("RUST_LOG");

    // Should not panic
    init_logging(None);

    std::env::remove_var("NO_COLOR");
}
