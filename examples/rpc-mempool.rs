//! Example: Mempool RPC methods
//!
//! This example demonstrates the mempool-related JSON-RPC methods available
//! in blvm-node. These methods let you inspect unconfirmed transactions,
//! check mempool statistics, and test transaction acceptance.
//!
//! This example shows the RPC request format. To test with a running node:
//!   1. Start blvm-node: blvm-node --network testnet
//!   2. Run this example: cargo run --example rpc-mempool
//!
//! Or use curl:
//!   curl -X POST http://127.0.0.1:18332 \
//!     -H "Content-Type: application/json" \
//!     -d '{"jsonrpc":"2.0","method":"getmempoolinfo","params":[],"id":1}'

use serde_json::json;

fn main() -> anyhow::Result<()> {
    println!("blvm-node Mempool RPC Examples");
    println!("=================================");
    println!();
    println!("These methods inspect the mempool and test transaction acceptance.");
    println!("All methods are Bitcoin Core-compatible.");
    println!();

    let rpc_url = "http://127.0.0.1:18332"; // Testnet
                                            // let rpc_url = "http://127.0.0.1:8332"; // Mainnet

    println!("RPC Endpoint: {rpc_url}");
    println!();
    println!("Example RPC Requests:");
    println!();

    let txid = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
    let raw_tx =
        "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff...";

    // Example 1: Get mempool info
    println!("1. getmempoolinfo - Get overall mempool stats");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getmempoolinfo",
        "params": [],
        "id": 1
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Overall mempool stats (size, bytes, min fee rate)");
    println!();

    // Example 2: Get raw mempool
    println!("2. getrawmempool - List all unconfirmed transactions");
    let verbose = false;
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getrawmempool",
        "params": [verbose],
        "id": 2
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: List all unconfirmed transactions");
    println!("   Note: verbose=false returns txid array; verbose=true returns full entry map");
    println!();

    // Example 3: Get mempool entry
    println!("3. getmempoolentry - Inspect a single tx's mempool entry");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getmempoolentry",
        "params": [txid],
        "id": 3
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Inspect a single tx's mempool entry (fee, size, ancestors, time)");
    println!();

    // Example 4: Test mempool accept
    println!("4. testmempoolaccept - Dry-run acceptance check");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "testmempoolaccept",
        "params": [[raw_tx]],  // array of hex strings — supports batch validation
        "id": 4
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Dry-run acceptance check — does NOT broadcast the transaction");
    println!("   Note: params take an array of hex strings to support batch validation");
    println!();

    // Example 5: Get mempool ancestors
    println!("5. getmempoolancestors - Find all unconfirmed ancestors of a transaction");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getmempoolancestors",
        "params": [txid, verbose],
        "id": 5
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Find all unconfirmed ancestors of a transaction (CPFP chains)");
    println!("   Note: verbose=false returns txid array; verbose=true returns entry details");
    println!();

    // Example 6: Save mempool
    println!("6. savemempool - Flush in-memory mempool to disk");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "savemempool",
        "params": [],
        "id": 6
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Persist unconfirmed transactions so they survive a node restart");
    println!(
        "   Note: Mempool is saved automatically on clean shutdown; call explicitly for safety"
    );
    println!();

    // Example 7: Get mempool descendants
    println!("7. getmempooldescendants - Find all unconfirmed descendants of a transaction");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getmempooldescendants",
        "params": [txid, verbose],
        "id": 7
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!(
        "   Use: Find all unconfirmed transactions that depend on this one (reverse of ancestors)"
    );
    println!("   Note: verbose=false returns txid array; verbose=true returns full entry details");
    println!();

    println!("Method Summary:");
    println!("  getmempoolinfo       - Overall mempool stats (size, bytes, min fee)");
    println!("  getrawmempool        - List all unconfirmed txids (or full entries)");
    println!("  getmempoolentry      - Details for one mempool transaction");
    println!("  testmempoolaccept    - Dry-run: would this tx be accepted?");
    println!("  getmempoolancestors  - Unconfirmed ancestor chain for a transaction");
    println!("  savemempool          - Flush in-memory mempool to disk");
    println!("  getmempooldescendants - All unconfirmed descendants of a transaction");
    println!();
    println!("To test with a running node:");
    println!("  1. Start node: blvm-node --network testnet");
    println!("  2. Send requests with curl or any HTTP client");

    Ok(())
}
