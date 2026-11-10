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
//! ```no_run
//! use blvm_node::utils::init_logging;
//!
//! init_logging(None); // Uses RUST_LOG or defaults to "info"
//! ```
//!
//! ## Module
//! ```no_run
//! use blvm_node::utils::init_module_logging;
//!
//! init_module_logging("my_module", None); // Module gets its own filter
//! ```

use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize logging for the main node
///
/// Uses standard Rust logging practices:
/// - Respects RUST_LOG environment variable (standard practice)
/// - Falls back to config filter if provided
/// - Defaults to "info" level
/// - Uses `EnvFilter::from_default_env()` for proper RUST_LOG handling
///
/// # Arguments
/// * `filter` - Optional log filter from config (e.g., "info", "debug", "blvm_node=debug,network=trace")
///   If None, uses RUST_LOG environment variable or defaults to "info"
///
/// # Example
/// ```no_run
/// use blvm_node::utils::init_logging;
///
/// // Use RUST_LOG (standard) or default to "info"
/// init_logging(None);
///
/// // Override with config filter (RUST_LOG still takes precedence)
/// init_logging(Some("debug"));
/// ```
pub fn init_logging(filter: Option<&str>) {
    let resolved = current_node_log_filter(filter, false);
    install_text_subscriber(&resolved);
}

/// Filter string for the node subscriber.
///
/// `RUST_LOG`, then `BLVM_LOG_LEVEL`, then the config filter, then `--verbose`
/// (`blvm=debug,blvm_node=debug`), then `blvm=info,blvm_node=info`.
pub fn node_log_filter(
    rust_log: Option<&str>,
    blvm_log_level: Option<&str>,
    config_filter: Option<&str>,
    verbose: bool,
) -> String {
    if let Some(value) = rust_log.map(str::trim).filter(|s| !s.is_empty()) {
        return value.to_string();
    }
    if let Some(value) = blvm_log_level.map(str::trim).filter(|s| !s.is_empty()) {
        return value.to_string();
    }
    if let Some(value) = config_filter.map(str::trim).filter(|s| !s.is_empty()) {
        return value.to_string();
    }
    if verbose {
        return "blvm=debug,blvm_node=debug".to_string();
    }
    "blvm=info,blvm_node=info".to_string()
}

fn current_node_log_filter(config_filter: Option<&str>, verbose: bool) -> String {
    let rust_log = std::env::var("RUST_LOG").ok();
    let blvm_log_level = std::env::var("BLVM_LOG_LEVEL").ok();
    node_log_filter(
        rust_log.as_deref(),
        blvm_log_level.as_deref(),
        config_filter,
        verbose,
    )
}

fn install_text_subscriber(filter: &str) {
    let env_filter = EnvFilter::new(filter);
    let _ = tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_target(true)
                .with_thread_ids(false)
                .with_ansi(std::env::var("NO_COLOR").is_err()),
        )
        .with(env_filter)
        .try_init();
}

/// Install the node subscriber once.
///
/// `verbose` is the `--verbose` flag: `blvm=debug,blvm_node=debug` when
/// `RUST_LOG`, `BLVM_LOG_LEVEL`, and `[logging].filter` are all unset.
pub fn init_node_logging(config: Option<&crate::config::LoggingConfig>, verbose: bool) {
    let config_filter = config.and_then(|c| c.filter.as_deref());
    let resolved = current_node_log_filter(config_filter, verbose);
    let want_json = config.map(|c| c.json_format).unwrap_or(false);
    if want_json {
        #[cfg(feature = "json-logging")]
        {
            init_json_logging(Some(&resolved));
            return;
        }
        #[cfg(not(feature = "json-logging"))]
        {
            install_text_subscriber(&resolved);
            tracing::warn!(
                "logging.json_format is set but the json-logging feature is off; using text logs"
            );
            return;
        }
    }
    install_text_subscriber(&resolved);
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
///   - RUST_LOG if set (standard, takes precedence)
///   - Otherwise: "{module_name}=info,blvm_node::module=debug"
///
/// # Example
/// ```no_run
/// use blvm_node::utils::init_module_logging;
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
    let default_filter = format!("{}={},blvm_node::module=debug", module_name, "info");

    // If RUST_LOG is not set, use config filter or default
    if std::env::var("RUST_LOG").is_err() {
        env_filter = filter
            .map(EnvFilter::new)
            .unwrap_or_else(|| EnvFilter::new(&default_filter));
    }
    // If RUST_LOG is set, it takes precedence (standard practice)

    // Module logging setup (consistent with node):
    // - Same formatter as node for consistency
    // - Include target (module path) for better debugging
    // - Module-specific filter for isolation
    // - Can still see node module communication logs
    let _ = tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_target(true) // Include module path - useful for module debugging
                .with_thread_ids(false) // Disable by default
                .with_ansi(std::env::var("NO_COLOR").is_err()), // Respect NO_COLOR standard
        )
        .with(env_filter)
        .try_init();
}

/// Initialize logging with JSON output (for production/monitoring)
///
/// Useful when logs need to be parsed by log aggregation systems.
///
/// # Arguments
/// * `filter` - Optional log filter. If None, uses RUST_LOG or defaults to "info"
///
/// # Example
/// ```no_run
/// use blvm_node::utils::init_json_logging;
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
    let _ = tracing_subscriber::registry()
        .with(fmt::layer().json().with_target(true))
        .with(env_filter)
        .try_init();
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
/// ```no_run
/// use blvm_node::utils::init_logging_from_config;
/// use blvm_node::config::NodeConfig;
///
/// let config = NodeConfig::default();
/// init_logging_from_config(config.logging.as_ref());
/// ```
pub fn init_logging_from_config(config: Option<&crate::config::LoggingConfig>) {
    init_node_logging(config, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logging_filter_uses_config_when_env_unset() {
        let filter = node_log_filter(None, None, Some("blvm=debug"), false);
        assert_eq!(filter, "blvm=debug");
    }

    #[test]
    fn logging_filter_prefers_rust_log_then_blvm_level() {
        assert_eq!(
            node_log_filter(Some("trace"), Some("warn"), Some("blvm=debug"), true),
            "trace"
        );
        assert_eq!(
            node_log_filter(None, Some("warn"), Some("blvm=debug"), true),
            "warn"
        );
    }

    #[test]
    fn logging_filter_verbose_when_nothing_else_set() {
        assert_eq!(
            node_log_filter(None, None, None, true),
            "blvm=debug,blvm_node=debug"
        );
        assert_eq!(
            node_log_filter(None, None, None, false),
            "blvm=info,blvm_node=info"
        );
    }

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

    /// One event, captured in a buffer. Does not call `init_json_logging`
    /// (`try_init` installs once per process and discards the line).
    #[cfg(feature = "json-logging")]
    #[test]
    fn json_format_emits_a_level_field() {
        use std::io::{self, Write};
        use std::sync::{Arc, Mutex};

        struct Buf(Arc<Mutex<Vec<u8>>>);

        impl Write for Buf {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                self.0.lock().expect("log buffer").extend_from_slice(data);
                Ok(data.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_target(true)
            .with_writer(move || Buf(writer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("json probe");
        });
        let bytes = captured.lock().expect("log buffer").clone();
        let text = String::from_utf8(bytes).expect("log line is utf-8");
        let line = text
            .lines()
            .find(|line| !line.is_empty())
            .expect("one log line");
        let value: serde_json::Value = serde_json::from_str(line).expect("json log line");
        assert!(
            value.get("level").is_some(),
            "json log missing level: {line}"
        );
    }
}
