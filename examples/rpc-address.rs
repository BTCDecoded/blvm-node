//! Example: Address and index RPC methods
//!
//! This example demonstrates address validation and chain index RPC methods
//! available in bllvm-node. These methods let you validate addresses, query
//! compact block filters, and inspect chain index state.
//!
//! This example shows the RPC request format. To test with a running node:
//!   1. Start bllvm-node: bllvm-node --network testnet
//!   2. Run this example: cargo run --example rpc-address
//!
//! Or use curl:
//!   curl -X POST http://127.0.0.1:18332 \
//!     -H "Content-Type: application/json" \
//!     -d '{"jsonrpc":"2.0","method":"validateaddress","params":["tb1q..."],"id":1}'

use serde_json::json;

fn main() -> anyhow::Result<()> {
    println!("bllvm-node Address & Index RPC Examples");
    println!("==========================================");
    println!();
    println!("These methods validate addresses and query chain index state.");
    println!("All methods are Bitcoin Core-compatible.");
    println!();

    let rpc_url = "http://127.0.0.1:18332"; // Testnet
                                            // let rpc_url = "http://127.0.0.1:8332"; // Mainnet

    println!("RPC Endpoint: {rpc_url}");
    println!();
    println!("Example RPC Requests:");
    println!();

    let block_hash = "000000000000000000024bead8df69990852c202db0e0097c1a12ea637d7e96d";
    let address = "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx"; // testnet bech32

    // Example 1: Get block filter
    println!("1. getblockfilter - Get the BIP158 compact block filter for a block");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getblockfilter",
        "params": [block_hash, "basic"],  // [blockhash, filtertype (default: "basic")]
        "id": 1
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get the compact block filter for lightweight wallet scanning");
    println!("   Note: filtertype 'basic' is the standard BIP158 filter; requires compact block filter index");
    println!();

    // Example 2: Get index info
    println!("2. getindexinfo - Query the status of chain indexes");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getindexinfo",
        "params": [],  // optional: ["txindex"] to query a specific index
        "id": 2
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Check which indexes are built and how far they have synced");
    println!("   Note: Common indexes: txindex, coinstatsindex, blockfilterindex");
    println!();

    // Example 3: Get blockchain state (bllvm-node extended)
    println!("3. getblockchainstate - Extended chain state summary (bllvm-node extended)");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getblockchainstate",
        "params": [],
        "id": 3
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get a combined view of chain height, sync progress, and UTXO set size");
    println!();

    // Example 4: Validate address
    println!("4. validateaddress - Check whether an address is valid");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "validateaddress",
        "params": [address],
        "id": 4
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Validate address format and network (mainnet/testnet) before sending funds");
    println!("   Note: Returns {{isvalid, address, scriptPubKey, isscript, iswitness, witness_version}}");
    println!();

    // Example 5: Get address info
    println!("5. getaddressinfo - Get detailed information about an address");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getaddressinfo",
        "params": [address],
        "id": 5
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Inspect address type, script, witness program, and encoding");
    println!("   Note: Extends validateaddress with script type and witness program details");
    println!();

    println!("Method Summary:");
    println!("  getblockfilter    - BIP158 compact block filter for a block hash");
    println!("  getindexinfo      - Status and sync progress of chain indexes");
    println!("  getblockchainstate - Extended chain state summary");
    println!("  validateaddress   - Validate address format and network");
    println!("  getaddressinfo    - Detailed address type and script information");
    println!();
    println!("Typical workflow for wallet scanning:");
    println!("  1. getindexinfo            → confirm blockfilterindex is synced");
    println!("  2. getblockfilter <hash>   → download filter for each block");
    println!("  3. Match filter locally    → download full block only on match");
    println!();
    println!("To test with a running node:");
    println!("  1. Start node: bllvm-node --network testnet");
    println!("  2. Send requests with curl or any HTTP client");

    Ok(())
}
