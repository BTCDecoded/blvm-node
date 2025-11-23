//! Logging utilities for consistent logging across node and modules
//!
//! Provides simple, non-overengineered logging initialization that:
//! - Supports centralized node logging
//! - Supports module-specific logging with different filters
//! - Respects RUST_LOG environment variable
//! - Allows configuration via config file
//!
//! # Usage
//!
//! ## Main Node
//! ```rust
//! use bllvm_node::utils::init_logging;
//!
//! init_logging(None); // Uses RUST_LOG or defaults to "info"
//! ```
//!
//! ## Module
//! ```rust
//! use bllvm_node::utils::init_module_logging;
//!
//! init_module_logging("my_module", None); // Module gets its own filter
//! ```

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Initialize logging for the main node
///
/// Uses standard Rust logging practices:
/// - Respects RUST_LOG environment variable (standard practice)
/// - Falls back to config filter if provided
/// - Defaults to "info" level
/// - Uses `EnvFilter::from_default_env()` for proper RUST_LOG handling
///
/// # Arguments
/// * `filter` - Optional log filter from config (e.g., "info", "debug", "bllvm_node=debug,network=trace")
///              If None, uses RUST_LOG environment variable or defaults to "info"
///
/// # Example
/// ```rust
/// use bllvm_node::utils::init_logging;
///
/// // Use RUST_LOG (standard) or default to "info"
/// init_logging(None);
///
/// // Override with config filter (RUST_LOG still takes precedence)
/// init_logging(Some("debug"));
/// ```
pub fn init_logging(filter: Option<&str>) {
    // Standard practice: Use EnvFilter::from_default_env() which properly handles RUST_LOG
    // This respects the standard RUST_LOG environment variable format
    let mut env_filter = EnvFilter::from_default_env();

    // If config provides a filter and RUST_LOG is not set, use config filter
    // RUST_LOG always takes precedence (standard practice)
    if filter.is_some() && std::env::var("RUST_LOG").is_err() {
        if let Some(f) = filter {
            env_filter = EnvFilter::new(f);
        }
    }

    // If neither RUST_LOG nor config filter is set, default to "info"
    if std::env::var("RUST_LOG").is_err() && filter.is_none() {
        env_filter = EnvFilter::new("info");
    }

    // Standard setup following Rust logging best practices:
    // - Human-readable format (default)
    // - Output to stderr (standard for logs)
    // - Include target (module path) for better debugging
    // - Thread IDs disabled by default (can be noisy)
    // - ANSI colors enabled (can be disabled via NO_COLOR env var)
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_target(true) // Include module path - useful for debugging
                .with_thread_ids(false) // Disable by default (can be noisy)
                .with_ansi(!std::env::var("NO_COLOR").is_ok()), // Respect NO_COLOR standard
        )
        .with(env_filter)
        .init();
}

/// Initialize logging for a module
///
/// Modules get their own logging context with sensible defaults:
/// - Module name is included in filter
/// - Node module communication logs are visible
/// - Can be overridden with RUST_LOG (standard practice)
///
/// Uses standard Rust logging practices with module-specific defaults.
///
/// # Arguments
/// * `module_name` - Name of the module (e.g., "lightning", "lnbits")
/// * `filter` - Optional log filter from config. If None, uses:
///              - RUST_LOG if set (standard, takes precedence)
///              - Otherwise: "{module_name}=info,bllvm_node::module=debug"
///
/// # Example
/// ```rust
/// use bllvm_node::utils::init_module_logging;
///
/// // Default module logging (respects RUST_LOG)
/// init_module_logging("lightning", None);
///
/// // Custom filter from config (RUST_LOG still takes precedence)
/// init_module_logging("lightning", Some("lightning=debug"));
/// ```
pub fn init_module_logging(module_name: &str, filter: Option<&str>) {
    // Standard practice: Use EnvFilter::from_default_env() for RUST_LOG
    let mut env_filter = EnvFilter::from_default_env();

    // Default filter for modules: module at info, node module communication at debug
    let default_filter = format!("{}={},bllvm_node::module=debug", module_name, "info");

    // If RUST_LOG is not set, use config filter or default
    if std::env::var("RUST_LOG").is_err() {
        env_filter = filter
            .map(|f| EnvFilter::new(f))
            .unwrap_or_else(|| EnvFilter::new(&default_filter));
    }
    // If RUST_LOG is set, it takes precedence (standard practice)

    // Module logging setup (consistent with node):
    // - Same formatter as node for consistency
    // - Include target (module path) for better debugging
    // - Module-specific filter for isolation
    // - Can still see node module communication logs
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_target(true) // Include module path - useful for module debugging
                .with_thread_ids(false) // Disable by default
                .with_ansi(!std::env::var("NO_COLOR").is_ok()), // Respect NO_COLOR standard
        )
        .with(env_filter)
        .init();
}

/// Initialize logging with JSON output (for production/monitoring)
///
/// Useful when logs need to be parsed by log aggregation systems.
///
/// # Arguments
/// * `filter` - Optional log filter. If None, uses RUST_LOG or defaults to "info"
///
/// # Example
/// ```rust
/// use bllvm_node::utils::init_json_logging;
///
/// init_json_logging(None);
/// ```
/// Initialize logging with JSON output (for production/monitoring)
///
/// Useful when logs need to be parsed by log aggregation systems.
/// Follows standard structured logging practices.
///
/// # Arguments
/// * `filter` - Optional log filter from config. If None, uses RUST_LOG or defaults to "info"
///
/// # Example
/// ```rust
/// use bllvm_node::utils::init_json_logging;
///
/// init_json_logging(None);
/// ```
#[cfg(feature = "json-logging")]
pub fn init_json_logging(filter: Option<&str>) {
    // Standard practice: Use EnvFilter::from_default_env() for RUST_LOG
    let mut env_filter = EnvFilter::from_default_env();

    // If config provides a filter and RUST_LOG is not set, use config filter
    if filter.is_some() && std::env::var("RUST_LOG").is_err() {
        if let Some(f) = filter {
            env_filter = EnvFilter::new(f);
        }
    }

    // If neither RUST_LOG nor config filter is set, default to "info"
    if std::env::var("RUST_LOG").is_err() && filter.is_none() {
        env_filter = EnvFilter::new("info");
    }

    // JSON logging for structured logging (production/monitoring):
    // - JSON format for log aggregation systems
    // - Include target, spans, and span lists for full context
    // - Standard structured logging practice
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_target(true) // Include module path in JSON
                .with_current_span(true) // Include current span context
                .with_span_list(true), // Include span list for full context
        )
        .with(env_filter)
        .init();
}

/// Initialize logging from NodeConfig
///
/// Integrates logging configuration from config file with standard RUST_LOG handling.
/// RUST_LOG always takes precedence (standard practice).
///
/// # Arguments
/// * `config` - Optional LoggingConfig from NodeConfig
///
/// # Example
/// ```rust
/// use bllvm_node::utils::init_logging_from_config;
/// use bllvm_node::config::NodeConfig;
///
/// let config = NodeConfig::default();
/// init_logging_from_config(config.logging.as_ref());
/// ```
pub fn init_logging_from_config(config: Option<&crate::config::LoggingConfig>) {
    let filter = config.and_then(|c| c.filter.as_deref());

    if config.map(|c| c.json_format).unwrap_or(false) {
        #[cfg(feature = "json-logging")]
        {
            init_json_logging(filter);
        }
        #[cfg(not(feature = "json-logging"))]
        {
            // Fall back to regular logging if json-logging feature not enabled
            init_logging(filter);
        }
    } else {
        init_logging(filter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_logging_initialization() {
        // Test that logging functions exist and have correct signatures
        // We can't actually call init_logging() because tracing_subscriber
        // can only be initialized once per process, and other tests may have
        // already initialized it. Instead, we verify the functions compile
        // and can be referenced.

        // Verify function signatures exist
        let _init_logging_fn: fn(Option<&str>) = init_logging;
        let _init_module_logging_fn: fn(&str, Option<&str>) = init_module_logging;
        let _init_logging_from_config_fn: fn(Option<&crate::config::LoggingConfig>) =
            init_logging_from_config;

        #[cfg(feature = "json-logging")]
        {
            let _init_json_logging_fn: fn(Option<&str>) = init_json_logging;
        }

        // If we can reference the functions, they compile correctly
        // This test will fail if function signatures change
    }
}
