//! Example: Module management RPC methods
//!
//! This example demonstrates the module lifecycle JSON-RPC methods available
//! in blvm-node. Modules are separate processes that extend node functionality
//! and communicate via Unix domain sockets.
//!
//! This example shows the RPC request format. To test with a running node:
//!   1. Start blvm-node: blvm-node --network regtest
//!   2. Run this example: cargo run --example rpc-modules
//!
//! Or use curl:
//!   curl -X POST http://127.0.0.1:18443 \
//!     -H "Content-Type: application/json" \
//!     -d '{"jsonrpc":"2.0","method":"listmodules","params":[],"id":1}'

use serde_json::json;

fn main() -> anyhow::Result<()> {
    println!("blvm-node Module Management RPC Examples");
    println!("==========================================");
    println!();
    println!("These methods manage the blvm-node module system.");
    println!("Modules are sandboxed processes declared in module.toml manifests.");
    println!();

    let rpc_url = "http://127.0.0.1:18443"; // Regtest
                                            // let rpc_url = "http://127.0.0.1:18332"; // Testnet

    println!("RPC Endpoint: {rpc_url}");
    println!();
    println!("Example RPC Requests:");
    println!();

    // Example 1: List loaded modules
    println!("1. listmodules - List all currently loaded modules");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "listmodules",
        "params": [],
        "id": 1
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Inspect which modules are active and their status");
    println!("   Returns: Array of module names currently loaded");
    println!();

    // Example 2: Load a module
    println!("2. loadmodule - Hot-load a module at runtime");
    let module_name = "simple-module";
    let request = json!({
        "jsonrpc": "2.0",
        "method": "loadmodule",
        "params": [module_name],
        "id": 2
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Start a new module process without restarting the node");
    println!("   Note: Module must be registered and its binary must be available");
    println!("   Note: See examples/simple-module/ for a minimal module implementation");
    println!();

    // Example 3: Unload a module
    println!("3. unloadmodule - Hot-unload a module at runtime");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "unloadmodule",
        "params": [module_name],
        "id": 3
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Stop and remove a module process without restarting the node");
    println!("   Note: In-flight module requests will be terminated");
    println!();

    // Example 4: Reload a module
    println!("4. reloadmodule - Hot-reload a module (unload + load)");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "reloadmodule",
        "params": [module_name],
        "id": 4
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Pick up a new module binary or updated config without restarting the node");
    println!("   Note: Equivalent to unloadmodule followed by loadmodule");
    println!();

    // Example 5: Get module CLI specs
    println!("5. getmoduleclispecs - Get CLI specs from all loaded modules");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getmoduleclispecs",
        "params": [],
        "id": 5
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Discover available CLI subcommands exposed by loaded modules");
    println!("   Returns: Object mapping CLI name to subcommand spec");
    println!("   Example response:");
    println!("     {{");
    println!("       \"sync-policy\": {{\"subcommands\": [\"list\", \"set\", \"reset\"]}},");
    println!("       \"hello\":        {{\"subcommands\": [\"greet\"]}}");
    println!("     }}");
    println!();

    // Example 6: Run a module CLI subcommand
    println!("6. runmodulecli - Run a CLI subcommand on a loaded module");
    let cli_name = "sync-policy";
    let subcommand = "list";
    let request = json!({
        "jsonrpc": "2.0",
        "method": "runmodulecli",
        "params": [cli_name, subcommand],  // [module_cli_name, subcommand, ...args]
        "id": 6
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Invoke a module's CLI subcommand and capture its output");
    println!("   Returns: {{stdout, stderr, exit_code}}");
    println!();

    println!("   With arguments:");
    let request_with_args = json!({
        "jsonrpc": "2.0",
        "method": "runmodulecli",
        "params": ["sync-policy", "set", "--mode", "conservative"],
        "id": 7
    });
    println!(
        "   Request: {}",
        serde_json::to_string_pretty(&request_with_args)?
    );
    println!("   Note: Additional params after subcommand are passed as CLI args");
    println!();

    println!("Method Summary:");
    println!("  listmodules       - List all currently loaded module names");
    println!("  loadmodule        - Hot-load a module process at runtime");
    println!("  unloadmodule      - Hot-unload a module process at runtime");
    println!("  reloadmodule      - Hot-reload a module (picks up new binary/config)");
    println!("  getmoduleclispecs - Get CLI subcommand specs from all loaded modules");
    println!("  runmodulecli      - Run a module CLI subcommand, returns stdout/stderr/exit_code");
    println!();
    println!("Module lifecycle:");
    println!("  Discovery → Verification → Loading → Execution → Monitoring");
    println!();
    println!("See examples/simple-module/ for a minimal module implementation.");
    println!();
    println!("To test with a running node:");
    println!("  1. Start node: blvm-node --network regtest");
    println!("  2. Send requests with curl or any HTTP client");

    Ok(())
}
