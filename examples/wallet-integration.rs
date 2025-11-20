//! Example: Basic wallet integration with bllvm-node
//!
//! This example shows how to use bllvm-node RPC API for wallet operations.
//! This demonstrates the RPC calls wallets need - actual wallets handle key management separately.
//!
//! This example shows the RPC request format. To test with a running node:
//!   1. Start bllvm-node: bllvm-node --network testnet
//!   2. Run this example: cargo run --example wallet-integration
//!
//! Or use curl:
//!   curl -X POST http://127.0.0.1:18332 \\
//!     -H "Content-Type: application/json" \\
//!     -d '{"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1}'

use serde_json::json;

fn main() -> anyhow::Result<()> {
    println!("bllvm-node Wallet Integration RPC Examples");
    println!("============================================");
    println!();
    println!("This example shows the RPC calls wallets need to integrate with bllvm-node.");
    println!("All methods are Bitcoin Core-compatible.");
    println!();

    // RPC endpoint (adjust for your setup)
    let rpc_url = "http://127.0.0.1:18332"; // Testnet
                                            // let rpc_url = "http://127.0.0.1:8332"; // Mainnet

    println!("RPC Endpoint: {}", rpc_url);
    println!();
    println!("Example RPC Requests:");
    println!();

    // Example 1: Get blockchain info
    println!("1. getblockchaininfo - Get chain state");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getblockchaininfo",
        "params": [],
        "id": 1
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Check chain state, block height, best block hash");
    println!();

    // Example 2: Get UTXO (for checking balance)
    println!("2. gettxout - Get UTXO information");
    let txid = "0000000000000000000000000000000000000000000000000000000000000000";
    let vout = 0;
    let request = json!({
        "jsonrpc": "2.0",
        "method": "gettxout",
        "params": [txid, vout, true],
        "id": 2
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Check if UTXO exists, get value and confirmations");
    println!("   Note: Replace txid/vout with actual values");
    println!();

    // Example 3: Get raw transaction
    println!("3. getrawtransaction - Get transaction data");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getrawtransaction",
        "params": [txid, true],
        "id": 3
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get transaction details for history");
    println!();

    // Example 4: Send raw transaction
    println!("4. sendrawtransaction - Broadcast transaction");
    let raw_tx =
        "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff...";
    let request = json!({
        "jsonrpc": "2.0",
        "method": "sendrawtransaction",
        "params": [raw_tx],
        "id": 4
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Broadcast signed transaction to network");
    println!("   Note: Wallet creates and signs transaction, node broadcasts");
    println!();

    // Example 5: Estimate fee
    println!("5. estimatesmartfee - Get fee estimation");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "estimatesmartfee",
        "params": [6],
        "id": 5
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get recommended fee rate for transaction");
    println!();

    println!("Integration Checklist:");
    println!("  ✅ getblockchaininfo - Chain state");
    println!("  ✅ gettxout - UTXO queries (balance checking)");
    println!("  ✅ getrawtransaction - Transaction data");
    println!("  ✅ sendrawtransaction - Transaction broadcasting");
    println!("  ✅ estimatesmartfee - Fee estimation");
    println!();
    println!("To test with a running node:");
    println!("  1. Start node: bllvm-node --network testnet");
    println!("  2. Use curl or your HTTP client to send these requests");
    println!("  3. Or use the bllvm-sdk for Rust integration");
    println!();
    println!("Your wallet can now integrate with bllvm-node!");

    Ok(())
}
