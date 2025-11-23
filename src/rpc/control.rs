//! Control and utility RPC methods
//!
//! Implements node control, monitoring, and utility methods:
//! - stop: Graceful node shutdown
//! - uptime: Node uptime tracking
//! - getmemoryinfo: Memory usage statistics
//! - getrpcinfo: RPC server information
//! - help: List available RPC methods
//! - logging: Control logging levels

use crate::rpc::errors::{RpcError, RpcResult};
use serde_json::{json, Number, Value};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::debug;

/// Control RPC methods
pub struct ControlRpc {
    /// Node start time for uptime calculation
    start_time: Instant,
    /// Shutdown channel for graceful shutdown
    shutdown_tx: Option<mpsc::UnboundedSender<()>>,
    /// Node shutdown callback (optional)
    node_shutdown: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
    /// Cached memory info (refreshed periodically, not every call)
    #[cfg(feature = "sysinfo")]
    cached_memory_info: Option<(Instant, Value)>,
}

impl ControlRpc {
    /// Create a new control RPC handler
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            shutdown_tx: None,
            node_shutdown: None,
            #[cfg(feature = "sysinfo")]
            cached_memory_info: None,
        }
    }

    /// Create with shutdown capability
    pub fn with_shutdown(
        shutdown_tx: mpsc::UnboundedSender<()>,
        node_shutdown: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
    ) -> Self {
        Self {
            start_time: Instant::now(),
            shutdown_tx: Some(shutdown_tx),
            node_shutdown,
            #[cfg(feature = "sysinfo")]
            cached_memory_info: None,
        }
    }

    /// Stop the node gracefully
    ///
    /// Params: [] (no parameters)
    pub async fn stop(&self, _params: &Value) -> RpcResult<Value> {
        debug!("RPC: stop");

        // Trigger node shutdown if callback provided
        if let Some(ref shutdown_fn) = self.node_shutdown {
            if let Err(e) = shutdown_fn() {
                return Err(RpcError::internal_error(format!(
                    "Failed to shutdown node: {}",
                    e
                )));
            }
        }

        // Send shutdown signal to RPC server
        if let Some(ref tx) = self.shutdown_tx {
            let _ = tx.send(());
        }

        // Return success immediately (shutdown happens asynchronously)
        Ok(json!("Bitcoin node stopping"))
    }

    /// Get node uptime
    ///
    /// Params: [] (no parameters)
    pub async fn uptime(&self, _params: &Value) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: uptime");

        use std::time::Duration;

        // Avoids repeated elapsed() calls and reduces JSON serialization overhead
        thread_local! {
            static CACHED_UPTIME: std::cell::Cell<(u64, Instant)> =
                std::cell::Cell::new((0, Instant::now()));
        }

        let start_time = self.start_time;
        let cached = CACHED_UPTIME.with(|c| c.get());

        // Update cache if >100ms old
        if cached.1.elapsed() >= Duration::from_millis(100) {
            let uptime = start_time.elapsed().as_secs();
            CACHED_UPTIME.with(|c| c.set((uptime, Instant::now())));

            Ok(Value::Number(Number::from(uptime)))
        } else {
            Ok(Value::Number(Number::from(cached.0)))
        }
    }

    /// Get memory usage information
    ///
    /// Params: ["mode"] (optional, "stats" or "mallocinfo", default: "stats")
    pub async fn getmemoryinfo(&self, params: &Value) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getmemoryinfo");

        let mode = params.get(0).and_then(|p| p.as_str()).unwrap_or("stats");

        match mode {
            "stats" => {
                // Get system memory information
                #[cfg(feature = "sysinfo")]
                {
                    use std::time::Duration;
                    use sysinfo::System;

                    // Use thread_local for better performance (no mutex contention)
                    thread_local! {
                        static CACHED_SYSTEM: std::cell::RefCell<(System, Instant, Value)> = {
                            let mut system = System::new();
                            system.refresh_memory();
                            let total_memory = system.total_memory();
                            let used_memory = system.used_memory();
                            let free_memory = system.free_memory();
                            let available_memory = system.available_memory();
                            let value = json!({
                                "locked": {
                                    "used": used_memory,
                                    "free": free_memory,
                                    "total": total_memory,
                                    "available": available_memory,
                                    "locked": 0,
                                }
                            });
                            std::cell::RefCell::new((system, Instant::now(), value))
                        };
                    }

                    CACHED_SYSTEM.with(|cache| {
                        let mut cache = cache.borrow_mut();
                        let tuple_ref = &mut *cache;
                        let system: &mut System = &mut tuple_ref.0;
                        let last_refresh: &mut Instant = &mut tuple_ref.1;
                        let cached_value: &mut Value = &mut tuple_ref.2;

                        // Memory stats don't need millisecond accuracy, 5s is fine
                        if last_refresh.elapsed() >= Duration::from_secs(5) {
                            system.refresh_memory();
                            let total_memory = system.total_memory();
                            let used_memory = system.used_memory();
                            let free_memory = system.free_memory();
                            let available_memory = system.available_memory();
                            let value = json!({
                                "locked": {
                                    "used": used_memory,
                                    "free": free_memory,
                                    "total": total_memory,
                                    "available": available_memory,
                                    "locked": 0,
                                }
                            });
                            *last_refresh = Instant::now();
                            *cached_value = value.clone();
                            Ok(value.clone())
                        } else {
                            Ok(cached_value.clone())
                        }
                    })
                }

                #[cfg(not(feature = "sysinfo"))]
                {
                    // Graceful degradation: return placeholder if sysinfo not available
                    // This allows the RPC method to work even without sysinfo feature
                    tracing::debug!("getmemoryinfo called but sysinfo feature not enabled, returning placeholder");
                    Ok(json!({
                        "locked": {
                            "used": 0,
                            "free": 0,
                            "total": 0,
                            "available": 0,
                            "locked": 0,
                        },
                        "note": "Memory statistics unavailable (sysinfo feature not enabled)"
                    }))
                }
            }
            "mallocinfo" => {
                // Mallocinfo format (XML) - not implemented
                // Bitcoin Core returns XML, but we'll return empty string for now
                Ok(json!(""))
            }
            _ => Err(RpcError::invalid_params(format!(
                "Invalid mode: {mode}. Must be 'stats' or 'mallocinfo'"
            ))),
        }
    }

    /// Get RPC server information
    ///
    /// Params: [] (no parameters)
    pub async fn getrpcinfo(&self, _params: &Value) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getrpcinfo");

        // Zero allocation on hot path
        const ACTIVE_COMMANDS: &[&str] = &[
            "getblockchaininfo",
            "getblock",
            "getblockhash",
            "getblockheader",
            "getbestblockhash",
            "getblockcount",
            "getdifficulty",
            "gettxoutsetinfo",
            "verifychain",
            "getrawtransaction",
            "sendrawtransaction",
            "testmempoolaccept",
            "decoderawtransaction",
            "gettxout",
            "gettxoutproof",
            "verifytxoutproof",
            "getmempoolinfo",
            "getrawmempool",
            "savemempool",
            "getnetworkinfo",
            "getpeerinfo",
            "getconnectioncount",
            "ping",
            "addnode",
            "disconnectnode",
            "getnettotals",
            "clearbanned",
            "setban",
            "listbanned",
            "getmininginfo",
            "getblocktemplate",
            "submitblock",
            "estimatesmartfee",
            "stop",
            "uptime",
            "getmemoryinfo",
            "getrpcinfo",
            "help",
            "logging",
        ];

        use std::sync::OnceLock;
        static RPC_INFO_VALUE: OnceLock<Value> = OnceLock::new();
        Ok(RPC_INFO_VALUE
            .get_or_init(|| {
                json!({
                    "active_commands": ACTIVE_COMMANDS,
                    "logpath": ""
                })
            })
            .clone())
    }

    /// List available RPC methods
    ///
    /// Params: ["command"] (optional, specific command to get help for)
    pub async fn help(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: help");

        // If specific command requested, return detailed help
        if let Some(command) = params.get(0).and_then(|p| p.as_str()) {
            let help_text = match command {
                "stop" => "Stop Bitcoin node.\n\nResult:\n\"Bitcoin node stopping\" (string)\n\nExamples:\n> bitcoin-cli stop",
                "uptime" => "Returns the total uptime of the server.\n\nResult:\nuptime (numeric) The number of seconds that the server has been running\n\nExamples:\n> bitcoin-cli uptime",
                "getmemoryinfo" => "Returns an object containing information about memory usage.\n\nArguments:\n1. mode (string, optional, default=\"stats\") determines what kind of information is returned.\n   - \"stats\" returns general statistics about memory usage in the daemon.\n   - \"mallocinfo\" returns an XML string describing low-level heap state (only available if compiled with glibc 2.10+).\n\nResult (mode \"stats\"):\n{\n  \"locked\": {               (json object) Information about locked memory manager\n    \"used\": xxxxx,          (numeric) Number of bytes used\n    \"free\": xxxxx,          (numeric) Number of bytes available in current arenas\n    \"total\": xxxxx,         (numeric) Total number of bytes managed\n    \"locked\": xxxxx,        (numeric) Amount of bytes that succeeded locking. If this number is smaller than total, locking pages failed at some point and key data could be swapped to disk.\n    \"chunks_used\": xxxxx,   (numeric) Number allocated chunks\n    \"chunks_free\": xxxxx,   (numeric) Number unused chunks\n  }\n}\n\nExamples:\n> bitcoin-cli getmemoryinfo",
                "getrpcinfo" => "Returns details about the RPC server.\n\nResult:\n{\n  \"active_commands\" (array) All active commands\n  \"logpath\" (string) The complete file path to the debug log\n}\n\nExamples:\n> bitcoin-cli getrpcinfo",
                "help" => "List all commands, or get help for a specified command.\n\nArguments:\n1. \"command\"     (string, optional) The command to get help on\n\nResult:\n\"text\"     (string) The help text\n\nExamples:\n> bitcoin-cli help\n> bitcoin-cli help getblock",
                "logging" => "Gets and sets the logging configuration.\n\nArguments:\n1. \"include\" (array of strings, optional) A list of categories to add debug logging\n2. \"exclude\" (array of strings, optional) A list of categories to remove debug logging\n\nResult:\n{ (json object)\n  \"active\" (boolean) Whether debug logging is active\n}\n\nExamples:\n> bitcoin-cli logging [\"all\"]\n> bitcoin-cli logging [\"http\"] [\"net\"]",
                _ => return Err(RpcError::invalid_params(format!("Unknown command: {command}"))),
            };
            Ok(json!(help_text.to_string()))
        } else {
            // No command specified, return list of all commands
            let commands = vec![
                "getblockchaininfo",
                "getblock",
                "getblockhash",
                "getblockheader",
                "getbestblockhash",
                "getblockcount",
                "getdifficulty",
                "gettxoutsetinfo",
                "verifychain",
                "getrawtransaction",
                "sendrawtransaction",
                "testmempoolaccept",
                "decoderawtransaction",
                "gettxout",
                "gettxoutproof",
                "verifytxoutproof",
                "getmempoolinfo",
                "getrawmempool",
                "savemempool",
                "getnetworkinfo",
                "getpeerinfo",
                "getconnectioncount",
                "ping",
                "addnode",
                "disconnectnode",
                "getnettotals",
                "clearbanned",
                "setban",
                "listbanned",
                "getmininginfo",
                "getblocktemplate",
                "submitblock",
                "estimatesmartfee",
                "stop",
                "uptime",
                "getmemoryinfo",
                "getrpcinfo",
                "help",
                "logging",
            ];

            Ok(json!(commands.join("\n")))
        }
    }

    /// Control logging levels
    ///
    /// Params: ["include"], ["exclude"] (optional arrays of log categories)
    pub async fn logging(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: logging");

        // Get include/exclude categories
        let _include = params
            .get(0)
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let exclude = params
            .get(1)
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Basic logging control implementation
        // Note: Full dynamic filter updates would require access to the global subscriber
        // which is complex. This implementation provides basic status and documents
        // the current filter state. For full control, the node would need to:
        // 1. Store a reference to the EnvFilter layer
        // 2. Provide methods to update the filter dynamically
        // 3. Rebuild the subscriber with the new filter

        // Check current filter state from environment
        use crate::utils::env_or_default;
        let current_filter = env_or_default("RUST_LOG", "info");

        // Determine if debug logging is active based on filter
        let active = current_filter.contains("debug")
            || current_filter.contains("trace")
            || !exclude.contains(&"all".to_string());

        Ok(json!({
            "active": active,
            "current_filter": current_filter,
            "note": "Full dynamic filter updates require subscriber access. Use RUST_LOG environment variable for full control."
        }))
    }

    /// Get node health status
    ///
    /// Returns comprehensive health report for all node components
    pub async fn gethealth(&self, _params: &Value) -> RpcResult<Value> {
        debug!("RPC: gethealth");

        // This would need access to Node instance to get full health report
        // For now, return basic health status
        Ok(json!({
            "status": "healthy",
            "message": "Node is operational",
            "note": "Full health check requires node instance access"
        }))
    }

    /// Get node metrics
    ///
    /// Returns comprehensive metrics for monitoring
    pub async fn getmetrics(&self, _params: &Value) -> RpcResult<Value> {
        debug!("RPC: getmetrics");

        // This would need access to MetricsCollector to get full metrics
        // For now, return basic metrics
        let uptime = self.start_time.elapsed().as_secs();
        Ok(json!({
            "uptime_seconds": uptime,
            "note": "Full metrics require MetricsCollector integration"
        }))
    }
}

impl Default for ControlRpc {
    fn default() -> Self {
        Self::new()
    }
}
