//! Example: Network RPC methods
//!
//! This example demonstrates the network-related JSON-RPC methods available
//! in blvm-node. These methods let you inspect peers, manage connections,
//! and configure bans.
//!
//! This example shows the RPC request format. To test with a running node:
//!   1. Start blvm-node: blvm-node --network testnet
//!   2. Run this example: cargo run --example rpc-network
//!
//! Or use curl:
//!   curl -X POST http://127.0.0.1:18332 \
//!     -H "Content-Type: application/json" \
//!     -d '{"jsonrpc":"2.0","method":"getnetworkinfo","params":[],"id":1}'

use serde_json::json;

fn main() -> anyhow::Result<()> {
    println!("blvm-node Network RPC Examples");
    println!("=================================");
    println!();
    println!("These methods inspect and manage P2P network connections.");
    println!("All methods are Bitcoin Core-compatible.");
    println!();

    let rpc_url = "http://127.0.0.1:18332"; // Testnet
                                            // let rpc_url = "http://127.0.0.1:8332"; // Mainnet

    println!("RPC Endpoint: {rpc_url}");
    println!();
    println!("Example RPC Requests:");
    println!();

    // Example 1: Get network info
    println!("1. getnetworkinfo - Get node network information");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getnetworkinfo",
        "params": [],
        "id": 1
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Check protocol version, connection count, relay fee, network status");
    println!();

    // Example 2: Get connection count
    println!("2. getconnectioncount - Get the number of active peer connections");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getconnectioncount",
        "params": [],
        "id": 2
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Quick check of peer count (returns integer)");
    println!();

    // Example 3: Get peer info
    println!("3. getpeerinfo - Get info about each connected peer");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getpeerinfo",
        "params": [],
        "id": 3
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Inspect peer addresses, versions, ping times, sync state");
    println!();

    // Example 4: Add a node
    println!("4. addnode - Add, remove, or connect to a specific peer");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "addnode",
        "params": ["192.0.2.1:8333", "add"],  // command: "add" | "remove" | "onetry"
        "id": 4
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Manually manage peer connections");
    println!("   Note: Commands are 'add' (persistent), 'remove', or 'onetry' (single attempt)");
    println!();

    // Example 5: Set ban
    println!("5. setban - Ban or unban a peer by IP/subnet");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "setban",
        "params": ["192.0.2.1", "add", 86400, false],
        // params: [ip/subnet, "add"|"remove", bantime_seconds, absolute_time]
        "id": 5
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Block a misbehaving or malicious peer");
    println!("   Note: bantime=86400 bans for 24h; absolute=false means relative to now");
    println!();

    // Example 6: List banned peers
    println!("6. listbanned - List all banned IPs/subnets");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "listbanned",
        "params": [],
        "id": 6
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Review active bans and their expiry times");
    println!();

    // Example 7: Ping peers
    println!("7. ping - Send ping message to all connected peers");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "ping",
        "params": [],
        "id": 7
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Trigger ping messages; observe pingtime/minping in getpeerinfo results");
    println!(
        "   Note: Returns null immediately — ping happens asynchronously in the network thread"
    );
    println!();

    // Example 8: Disconnect a node
    println!("8. disconnectnode - Disconnect a specific peer by address");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "disconnectnode",
        "params": ["192.0.2.1:8333"],
        "id": 8
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Drop a specific peer connection without banning");
    println!("   Note: The node may reconnect unless the peer is also banned");
    println!();

    // Example 9: Get network totals
    println!("9. getnettotals - Get total bytes sent and received");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getnettotals",
        "params": [],
        "id": 9
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Monitor cumulative network traffic (totalbytesrecv, totalbytessent)");
    println!();

    // Example 10: Clear banned peers
    println!("10. clearbanned - Remove all IP/subnet bans");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "clearbanned",
        "params": [],
        "id": 10
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Wipe all active bans at once (individual removal: setban with 'remove')");
    println!();

    // Example 11: Get added node info
    println!("11. getaddednodeinfo - Get info about a node added with addnode");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getaddednodeinfo",
        "params": ["192.0.2.1:8333"],  // node address; optional dns flag as second param
        "id": 11
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Check whether a manually added node is connected and its resolved addresses");
    println!("   Note: Pass true as second param to include DNS-resolved addresses");
    println!();

    // Example 12: Get node addresses
    println!("12. getnodeaddresses - Get a sample of known peer addresses from the address book");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getnodeaddresses",
        "params": [5],  // count (optional, default: 1, max: 100)
        "id": 12
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Bootstrap a new node or discover peers without DNS seeds");
    println!("   Note: Returns {{time, services, address, port, network}} per entry");
    println!();

    // Example 13: Set network active
    println!("13. setnetworkactive - Enable or disable all P2P network activity");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "setnetworkactive",
        "params": [false],  // false = disable all network connections
        "id": 13
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Temporarily isolate the node for debugging or offline testing");
    println!("   Note: Pass true to re-enable; node will reconnect to peers automatically");
    println!();

    println!("Method Summary:");
    println!("  getnetworkinfo     - Protocol version, connections, relay fee, networks");
    println!("  getconnectioncount - Active peer count (integer)");
    println!("  getpeerinfo        - Per-peer details (address, version, ping, sync)");
    println!("  addnode            - Manually add/remove/connect a peer");
    println!("  setban             - Ban or unban an IP or subnet");
    println!("  listbanned         - Show all active bans");
    println!("  ping               - Send ping to all peers (async)");
    println!("  disconnectnode     - Drop a specific peer connection");
    println!("  getnettotals       - Cumulative bytes sent/received");
    println!("  clearbanned        - Wipe all active bans");
    println!("  getaddednodeinfo   - Status of manually added nodes");
    println!("  getnodeaddresses   - Sample addresses from the address book");
    println!("  setnetworkactive   - Enable or disable P2P networking");
    println!();
    println!("To test with a running node:");
    println!("  1. Start node: blvm-node --network testnet");
    println!("  2. Send requests with curl or any HTTP client");

    Ok(())
}
