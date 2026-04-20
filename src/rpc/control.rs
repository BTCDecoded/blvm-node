//! Control and utility RPC methods
//!
//! Implements node control, monitoring, and utility methods:
//! - stop: Graceful node shutdown
//! - uptime: Node uptime tracking
//! - getmemoryinfo: Memory usage statistics
//! - getrpcinfo: RPC server information
//! - help: List available RPC methods
//! - logging: Control logging levels
//! - loadmodule, unloadmodule, reloadmodule: Hot reload module lifecycle

use crate::module::manager::ModuleManager;
use crate::module::registry::discovery::ModuleDiscovery;
use crate::rpc::cache::ThreadLocalTimedCache;
use crate::rpc::errors::{RpcError, RpcResult};
use crate::rpc::params::param_str;
use crate::utils::{CACHE_REFRESH_MEMORY, CACHE_REFRESH_UPTIME};
use serde_json::{json, Number, Value};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::debug;

thread_local! {
    static CACHED_UPTIME: ThreadLocalTimedCache<u64> = ThreadLocalTimedCache::new();
}

/// Memory stats from /proc/meminfo (Linux fallback when sysinfo not available)
#[cfg(all(not(feature = "sysinfo"), target_os = "linux"))]
struct ProcMemStats {
    total: u64,
    free: u64,
    available: u64,
    used: u64,
}

#[cfg(all(not(feature = "sysinfo"), target_os = "linux"))]
fn read_proc_meminfo() -> Result<ProcMemStats, std::io::Error> {
    let content = std::fs::read_to_string("/proc/meminfo")?;
    let mut mem_total_kb: Option<u64> = None;
    let mut mem_free_kb: Option<u64> = None;
    let mut mem_available_kb: Option<u64> = None;
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next().unwrap_or("");
        let val: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        match key {
            "MemTotal:" => mem_total_kb = Some(val),
            "MemFree:" => mem_free_kb = Some(val),
            "MemAvailable:" => mem_available_kb = Some(val),
            _ => {}
        }
    }
    let total = mem_total_kb.unwrap_or(0) * 1024;
    let free = mem_free_kb.unwrap_or(0) * 1024;
    let available = mem_available_kb.unwrap_or(free);
    let used = total.saturating_sub(available);
    Ok(ProcMemStats {
        total,
        free,
        available,
        used,
    })
}

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
    /// Module manager for load/unload/reload (optional)
    module_manager: Option<Arc<tokio::sync::Mutex<ModuleManager>>>,
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
            module_manager: None,
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
            module_manager: None,
        }
    }

    /// Add module manager for load/unload/reload RPC methods
    pub fn with_module_manager(
        mut self,
        module_manager: Arc<tokio::sync::Mutex<ModuleManager>>,
    ) -> Self {
        self.module_manager = Some(module_manager);
        self
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
                    "Failed to shutdown node: {e}"
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

        let start_time = self.start_time;
        let uptime = CACHED_UPTIME
            .with(|c| c.get_or_refresh(CACHE_REFRESH_UPTIME, || start_time.elapsed().as_secs()));
        Ok(Value::Number(Number::from(uptime)))
    }

    /// Get memory usage information
    ///
    /// Params: ["mode"] (optional, "stats" or "mallocinfo", default: "stats")
    pub async fn getmemoryinfo(&self, params: &Value) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getmemoryinfo");

        let mode = param_str(params, 0).unwrap_or("stats");

        match mode {
            "stats" => {
                // Get system memory information
                #[cfg(feature = "sysinfo")]
                {
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
                        if last_refresh.elapsed() >= CACHE_REFRESH_MEMORY {
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
                    // Fallback: parse /proc/meminfo on Linux when sysinfo not available
                    #[cfg(target_os = "linux")]
                    {
                        if let Ok(stats) = read_proc_meminfo() {
                            return Ok(json!({
                                "locked": {
                                    "used": stats.used,
                                    "free": stats.free,
                                    "total": stats.total,
                                    "available": stats.available,
                                    "locked": 0,
                                }
                            }));
                        }
                    }
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
                // Bitcoin Core returns glibc malloc_info() XML here; we do not implement that.
                // Empty string keeps the result type compatible with callers that expect a string.
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

        use std::sync::OnceLock;
        static RPC_INFO_VALUE: OnceLock<Value> = OnceLock::new();
        Ok(RPC_INFO_VALUE
            .get_or_init(|| {
                json!({
                    "active_commands": crate::rpc::methods::CORE_RPC_METHODS,
                    "logpath": ""
                })
            })
            .clone())
    }

    /// Load a module at runtime (hot load)
    ///
    /// Params: ["name"] (module name)
    pub async fn loadmodule(&self, params: &Value) -> RpcResult<Value> {
        let mgr = self
            .module_manager
            .as_ref()
            .ok_or_else(|| RpcError::internal_error("Module system not available".to_string()))?;
        let name = params.get(0).and_then(|v| v.as_str()).ok_or_else(|| {
            RpcError::invalid_params("loadmodule requires module name".to_string())
        })?;
        let mut manager = mgr.lock().await;
        let discovery = ModuleDiscovery::new(manager.modules_dir());
        let discovered = discovery
            .discover_module(name)
            .map_err(|e| RpcError::internal_error(e.to_string()))?;
        let config = crate::module::loader::ModuleLoader::load_module_config(
            name,
            &discovered.directory.join("config.toml"),
        )
        .map_err(|e| RpcError::internal_error(e.to_string()))?;
        manager
            .load_module(
                name,
                &discovered.binary_path,
                discovered.manifest.to_metadata(),
                config,
            )
            .await
            .map_err(|e| RpcError::internal_error(e.to_string()))?;
        Ok(json!("Module loaded"))
    }

    /// Unload a module at runtime (hot unload)
    ///
    /// Params: ["name"] (module name)
    pub async fn unloadmodule(&self, params: &Value) -> RpcResult<Value> {
        let mgr = self
            .module_manager
            .as_ref()
            .ok_or_else(|| RpcError::internal_error("Module system not available".to_string()))?;
        let name = params.get(0).and_then(|v| v.as_str()).ok_or_else(|| {
            RpcError::invalid_params("unloadmodule requires module name".to_string())
        })?;
        let mut manager = mgr.lock().await;
        manager
            .unload_module(name)
            .await
            .map_err(|e| RpcError::internal_error(e.to_string()))?;
        Ok(json!("Module unloaded"))
    }

    /// Reload a module at runtime (hot reload)
    ///
    /// Params: ["name"] (module name)
    pub async fn reloadmodule(&self, params: &Value) -> RpcResult<Value> {
        let mgr = self
            .module_manager
            .as_ref()
            .ok_or_else(|| RpcError::internal_error("Module system not available".to_string()))?;
        let name = params.get(0).and_then(|v| v.as_str()).ok_or_else(|| {
            RpcError::invalid_params("reloadmodule requires module name".to_string())
        })?;
        let mut manager = mgr.lock().await;
        let discovery = ModuleDiscovery::new(manager.modules_dir());
        let discovered = discovery
            .discover_module(name)
            .map_err(|e| RpcError::internal_error(e.to_string()))?;
        let config = crate::module::loader::ModuleLoader::load_module_config(
            name,
            &discovered.directory.join("config.toml"),
        )
        .map_err(|e| RpcError::internal_error(e.to_string()))?;
        manager
            .reload_module(
                name,
                &discovered.binary_path,
                discovered.manifest.to_metadata(),
                config,
            )
            .await
            .map_err(|e| RpcError::internal_error(e.to_string()))?;
        Ok(json!("Module reloaded"))
    }

    /// List loaded modules
    ///
    /// Params: [] (no parameters)
    pub async fn listmodules(&self, _params: &Value) -> RpcResult<Value> {
        let mgr = self
            .module_manager
            .as_ref()
            .ok_or_else(|| RpcError::internal_error("Module system not available".to_string()))?;
        let manager = mgr.lock().await;
        let modules = manager.list_modules().await;
        Ok(json!(modules))
    }

    /// Get CLI specs from all loaded modules
    ///
    /// Returns { "sync-policy": {...}, "hello": {...} } for blvm to build dynamic CLI.
    /// Params: [] (no parameters)
    pub async fn getmoduleclispecs(&self, _params: &Value) -> RpcResult<Value> {
        let mgr = self
            .module_manager
            .as_ref()
            .ok_or_else(|| RpcError::internal_error("Module system not available".to_string()))?;
        let manager = mgr.lock().await;
        let ipc_server = manager
            .ipc_server()
            .ok_or_else(|| RpcError::internal_error("IPC server not available".to_string()))?;
        let specs = ipc_server.lock().await.get_cli_specs().await;
        // Convert to JSON: use spec.name as key for blvm CLI building
        let mut result = serde_json::Map::new();
        for (_module_id, spec) in specs {
            if let Ok(v) = serde_json::to_value(&spec) {
                result.insert(spec.name.clone(), v);
            }
        }
        Ok(Value::Object(result))
    }

    #[cfg(not(unix))]
    pub async fn getmoduleclispecs(&self, _params: &Value) -> RpcResult<Value> {
        Ok(json!({}))
    }

    /// Run a module CLI subcommand
    ///
    /// Params: ["module_name", "subcommand", ...args]
    /// Returns: { "stdout": "...", "stderr": "...", "exit_code": 0 }
    pub async fn runmodulecli(&self, params: &Value) -> RpcResult<Value> {
        let mgr = self
            .module_manager
            .as_ref()
            .ok_or_else(|| RpcError::internal_error("Module system not available".to_string()))?;
        let manager = mgr.lock().await;
        let ipc_server = manager
            .ipc_server()
            .ok_or_else(|| RpcError::internal_error("IPC server not available".to_string()))?;
        let module_name = params.get(0).and_then(|p| p.as_str()).ok_or_else(|| {
            RpcError::invalid_params("runmodulecli requires module_name".to_string())
        })?;
        let subcommand = params.get(1).and_then(|p| p.as_str()).ok_or_else(|| {
            RpcError::invalid_params("runmodulecli requires subcommand".to_string())
        })?;
        let args: Vec<String> = params
            .as_array()
            .map(|a| {
                a.iter()
                    .skip(2)
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let payload = ipc_server
            .lock()
            .await
            .invoke_cli(module_name, subcommand, args)
            .await
            .map_err(|e| RpcError::internal_error(e.to_string()))?;
        match payload {
            crate::module::ipc::protocol::InvocationResultPayload::Cli {
                stdout,
                stderr,
                exit_code,
            } => Ok(json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": exit_code
            })),
            _ => Err(RpcError::internal_error(
                "Expected CLI result from module".to_string(),
            )),
        }
    }

    /// List available RPC methods
    ///
    /// Params: ["command"] (optional, specific command to get help for)
    pub async fn help(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: help");

        // If specific command requested, return detailed help
        if let Some(command) = param_str(params, 0) {
            let help_text = match command {
                "stop" => "Stop Bitcoin node.\n\nResult:\n\"Bitcoin node stopping\" (string)\n\nExamples:\n> bitcoin-cli stop",
                "uptime" => "Returns the total uptime of the server.\n\nResult:\nuptime (numeric) The number of seconds that the server has been running\n\nExamples:\n> bitcoin-cli uptime",
                "getmemoryinfo" => "Returns an object containing information about memory usage.\n\nArguments:\n1. mode (string, optional, default=\"stats\") determines what kind of information is returned.\n   - \"stats\" returns general statistics about memory usage in the daemon.\n   - \"mallocinfo\" is accepted for Bitcoin Core compatibility; in BLVM it returns an empty string (glibc mallocinfo XML is not implemented). Use \"stats\" for usable memory figures.\n\nResult (mode \"stats\"):\n{\n  \"locked\": {               (json object) Information about locked memory manager\n    \"used\": xxxxx,          (numeric) Number of bytes used\n    \"free\": xxxxx,          (numeric) Number of bytes available in current arenas\n    \"total\": xxxxx,         (numeric) Total number of bytes managed\n    \"locked\": xxxxx,        (numeric) Amount of bytes that succeeded locking. If this number is smaller than total, locking pages failed at some point and key data could be swapped to disk.\n    \"chunks_used\": xxxxx,   (numeric) Number allocated chunks\n    \"chunks_free\": xxxxx,   (numeric) Number unused chunks\n  }\n}\n\nExamples:\n> bitcoin-cli getmemoryinfo",
                "getrpcinfo" => "Returns details about the RPC server.\n\nResult:\n{\n  \"active_commands\" (array) All active commands\n  \"logpath\" (string) The complete file path to the debug log\n}\n\nExamples:\n> bitcoin-cli getrpcinfo",
                "help" => "List all commands, or get help for a specified command.\n\nArguments:\n1. \"command\"     (string, optional) The command to get help on\n\nResult:\n\"text\"     (string) The help text\n\nExamples:\n> bitcoin-cli help\n> bitcoin-cli help getblock",
                "logging" => "Gets and sets the logging configuration.\n\nArguments:\n1. \"include\" (array of strings, optional) A list of categories to add debug logging\n2. \"exclude\" (array of strings, optional) A list of categories to remove debug logging\n\nResult:\n{ (json object)\n  \"active\" (boolean) Whether debug logging is active\n}\n\nExamples:\n> bitcoin-cli logging [\"all\"]\n> bitcoin-cli logging [\"http\"] [\"net\"]",
                "loadmodule" => "Load a module at runtime (hot load).\n\nArguments:\n1. \"name\" (string, required) Module name\n\nResult:\n\"Module loaded\" (string)\n\nExamples:\n> bitcoin-cli loadmodule \"simple-module\"",
                "unloadmodule" => "Unload a module at runtime (hot unload).\n\nArguments:\n1. \"name\" (string, required) Module name\n\nResult:\n\"Module unloaded\" (string)\n\nExamples:\n> bitcoin-cli unloadmodule \"simple-module\"",
                "reloadmodule" => "Reload a module at runtime (hot reload). Picks up new binary/config.\n\nArguments:\n1. \"name\" (string, required) Module name\n\nResult:\n\"Module reloaded\" (string)\n\nExamples:\n> bitcoin-cli reloadmodule \"simple-module\"",
                "listmodules" => "List loaded modules.\n\nResult:\n[\"module1\", \"module2\", ...] (array of strings)\n\nExamples:\n> bitcoin-cli listmodules",
                "getmoduleclispecs" => "Get CLI specs from all loaded modules for dynamic CLI building.\n\nResult:\n{ \"sync-policy\": {...}, \"hello\": {...} } (object mapping CLI name to spec)\n\nExamples:\n> bitcoin-cli getmoduleclispecs",
                "runmodulecli" => "Run a module CLI subcommand.\n\nArguments:\n1. module_name (string, required) CLI name from getmoduleclispecs\n2. subcommand (string, required) Subcommand name\n3. ...args (strings, optional) Arguments for the subcommand\n\nResult:\n{ \"stdout\": \"...\", \"stderr\": \"...\", \"exit_code\": 0 }\n\nExamples:\n> bitcoin-cli runmodulecli \"sync-policy\" \"list\"",
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
                "loadtxoutset",
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
                "generatetoaddress",
                "submitblock",
                "estimatesmartfee",
                "stop",
                "uptime",
                "getmemoryinfo",
                "getrpcinfo",
                "help",
                "logging",
                "loadmodule",
                "unloadmodule",
                "reloadmodule",
                "listmodules",
                "getmoduleclispecs",
                "runmodulecli",
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
