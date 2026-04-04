//! Example: Node control and utility RPC methods
//!
//! This example demonstrates the control/utility JSON-RPC methods available
//! in blvm-node. These methods let you manage the node lifecycle, inspect
//! memory and RPC state, and control logging.
//!
//! This example shows the RPC request format. To test with a running node:
//!   1. Start blvm-node: blvm-node --network testnet
//!   2. Run this example: cargo run --example rpc-control
//!
//! Or use curl:
//!   curl -X POST http://127.0.0.1:18332 \
//!     -H "Content-Type: application/json" \
//!     -d '{"jsonrpc":"2.0","method":"uptime","params":[],"id":1}'

use serde_json::json;

fn main() -> anyhow::Result<()> {
    println!("blvm-node Control & Utility RPC Examples");
    println!("============================================");
    println!();
    println!("These methods manage the node lifecycle and expose operational telemetry.");
    println!("All methods are Bitcoin Core-compatible (stop, uptime, getmemoryinfo, getrpcinfo, help, logging).");
    println!("gethealth and getmetrics are blvm-node extensions.");
    println!();

    let rpc_url = "http://127.0.0.1:18332"; // Testnet
                                            // let rpc_url = "http://127.0.0.1:8332"; // Mainnet

    println!("RPC Endpoint: {rpc_url}");
    println!();
    println!("Example RPC Requests:");
    println!();

    // Example 1: Stop
    println!("1. stop - Gracefully shut down the node");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "stop",
        "params": [],
        "id": 1
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Initiate graceful node shutdown; flushes mempool and closes storage");
    println!("   Note: The connection will drop after the response — handle disconnects in your client");
    println!();

    // Example 2: Uptime
    println!("2. uptime - Seconds the node has been running");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "uptime",
        "params": [],
        "id": 2
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Check how long the node has been running; useful for monitoring/alerting");
    println!("   Note: Returns an integer (seconds); compare against expected uptime in dashboards");
    println!();

    // Example 3: Get memory info
    println!("3. getmemoryinfo - Memory usage statistics");
    let mode = "stats";
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getmemoryinfo",
        "params": [mode],
        "id": 3
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Inspect locked memory usage (used, free, total, available) for the node process");
    println!("   Note: mode='stats' returns JSON; mode='mallocinfo' returns XML heap info");
    println!();

    // Example 4: Get RPC info
    println!("4. getrpcinfo - Active RPC commands and log path");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getrpcinfo",
        "params": [],
        "id": 4
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: List all registered RPC methods and the path to the debug log file");
    println!("   Note: Returns {{active_commands: [...], logpath: \"\"}}");
    println!();

    // Example 5: Help
    println!("5. help - List commands or get detailed help on one");
    let command = "uptime";
    let request = json!({
        "jsonrpc": "2.0",
        "method": "help",
        "params": [command],
        "id": 5
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get the help text for a specific command; omit params to list all commands");
    println!("   Note: Pass any method name as the param; empty params returns full command list");
    println!();

    // Example 6: Logging
    println!("6. logging - Inspect or adjust active log categories");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "logging",
        "params": [["net", "mempool"], []],  // [include_categories, exclude_categories]
        "id": 6
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Enable verbose logging for specific subsystems while the node is running");
    println!("   Note: First array = categories to include; second = categories to exclude");
    println!("   Note: Common categories: net, mempool, rpc, bench, tor, zmq, walletdb");
    println!();

    // Example 7: Get health
    println!("7. gethealth - Node health status (blvm-node extension)");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "gethealth",
        "params": [],
        "id": 7
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get a health summary across all node components; ideal for liveness probes");
    println!("   Note: Returns {{status: \"healthy\"|\"degraded\"|\"unhealthy\", message, components}}");
    println!();

    // Example 8: Get metrics
    println!("8. getmetrics - Operational metrics (blvm-node extension)");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getmetrics",
        "params": [],
        "id": 8
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Retrieve uptime and performance counters for monitoring dashboards");
    println!("   Note: Returns {{uptime_seconds, ...}}; extended metrics require MetricsCollector integration");
    println!();

    println!("Method Summary:");
    println!("  stop          - Gracefully shut down the node");
    println!("  uptime        - Seconds since the node started");
    println!("  getmemoryinfo - Memory usage statistics (stats or mallocinfo)");
    println!("  getrpcinfo    - Active RPC commands and log path");
    println!("  help          - List all commands or get help on a specific command");
    println!("  logging       - Inspect or adjust active log categories");
    println!("  gethealth     - Node health status (blvm-node extension)");
    println!("  getmetrics    - Operational metrics (blvm-node extension)");
    println!();
    println!("To test with a running node:");
    println!("  1. Start node: blvm-node --network testnet");
    println!("  2. Send requests with curl or any HTTP client");

    Ok(())
}
