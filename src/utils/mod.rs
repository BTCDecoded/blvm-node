//! Utility modules for fault tolerance and resilience

pub mod arc;
pub mod async_helpers;
pub mod circuit_breaker;
pub mod env;
pub mod error;
pub mod lock;
pub mod logging;
pub mod option;
pub mod retry;
pub mod signal;
pub mod time;
pub mod timeout;
pub mod validation;

// Re-export commonly used items
pub use arc::{arc_clone_many, arc_clone_pair};
pub use async_helpers::{collect_results, delay_before, ignore_error, with_timeout_opt};
pub use circuit_breaker::{CircuitBreaker, CircuitState};
pub use env::{env_bool, env_int, env_opt, env_or_default, env_or_else};
pub use error::{
    err_option_to_result, log_error, log_error_async, result_to_option, with_default,
    with_default_async, with_fallback, with_fallback_async,
};
pub use lock::{try_with_lock_timeout, with_lock, with_read_lock, with_write_lock};
#[cfg(feature = "json-logging")]
pub use logging::init_json_logging;
pub use logging::{init_logging, init_logging_from_config, init_module_logging};
pub use option::{map_or_default, option_to_result, or_else, unwrap_or_default_with};
pub use retry::{retry_async_with_backoff, retry_with_backoff, RetryConfig};
pub use signal::{create_shutdown_receiver, wait_for_shutdown_signal};
pub use time::{current_timestamp, current_timestamp_duration};
pub use timeout::{
    with_custom_timeout, with_network_timeout, with_rpc_timeout, with_storage_timeout,
    with_timeout, DEFAULT_NETWORK_TIMEOUT, DEFAULT_RPC_TIMEOUT, DEFAULT_STORAGE_TIMEOUT,
};
pub use validation::{ensure, ensure_fmt, ensure_not_empty, ensure_range, ensure_some};
