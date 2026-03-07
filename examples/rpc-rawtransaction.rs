//! Example: Raw Transaction RPC methods
//!
//! This example demonstrates the raw transaction JSON-RPC methods available
//! in bllvm-node. These methods let you fetch, decode, create, and broadcast
//! raw Bitcoin transactions, as well as verify inclusion via tx output proofs.
//!
//! This example shows the RPC request format. To test with a running node:
//!   1. Start bllvm-node: bllvm-node --network testnet
//!   2. Run this example: cargo run --example rpc-rawtransaction
//!
//! Or use curl:
//!   curl -X POST http://127.0.0.1:18332 \
//!     -H "Content-Type: application/json" \
//!     -d '{"jsonrpc":"2.0","method":"getrawtransaction","params":["<txid>",true],"id":1}'

use serde_json::json;

fn main() -> anyhow::Result<()> {
    println!("bllvm-node Raw Transaction RPC Examples");
    println!("=========================================");
    println!();
    println!("These methods work with raw Bitcoin transactions.");
    println!("All methods are Bitcoin Core-compatible.");
    println!();

    let rpc_url = "http://127.0.0.1:18332"; // Testnet
                                            // let rpc_url = "http://127.0.0.1:8332"; // Mainnet

    println!("RPC Endpoint: {rpc_url}");
    println!();
    println!("Example RPC Requests:");
    println!();

    let txid = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
    let block_hash = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";

    // Example 1: Get raw transaction
    println!("1. getrawtransaction - Fetch a transaction by txid");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getrawtransaction",
        "params": [txid, true],  // true = verbose (decoded JSON), false = raw hex
        "id": 1
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Retrieve transaction data for inspection or rebroadcast");
    println!("   Note: verbose=false returns raw hex; verbose=true returns decoded JSON");
    println!();

    // Example 2: Decode a raw transaction
    println!("2. decoderawtransaction - Decode a raw hex transaction without broadcasting");
    let raw_tx_hex = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff4d04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73ffffffff0100f2052a01000000434104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac00000000";
    let request = json!({
        "jsonrpc": "2.0",
        "method": "decoderawtransaction",
        "params": [raw_tx_hex],
        "id": 2
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Inspect inputs, outputs, and scripts before broadcasting");
    println!("   Note: Does not broadcast — safe to use for validation");
    println!();

    // Example 3: Create a raw transaction
    println!("3. createrawtransaction - Build an unsigned raw transaction");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "createrawtransaction",
        "params": [
            // inputs: array of UTXOs to spend
            [{"txid": txid, "vout": 0}],
            // outputs: map of address -> BTC amount
            {"tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx": 0.001}
        ],
        "id": 3
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Construct the unsigned transaction skeleton (wallet signs separately)");
    println!("   Note: Returns hex; sign with your wallet before calling sendrawtransaction");
    println!();

    // Example 4: Send (broadcast) a raw transaction
    println!("4. sendrawtransaction - Broadcast a signed raw transaction");
    let signed_tx_hex = "01000000..."; // Replace with your signed transaction hex
    let request = json!({
        "jsonrpc": "2.0",
        "method": "sendrawtransaction",
        "params": [signed_tx_hex],
        // Optional: [hex, maxfeerate] where maxfeerate is BTC/kB limit
        "id": 4
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Submit a signed transaction to be relayed to the network");
    println!("   Note: Returns txid on success; fails if tx is invalid or fee too low");
    println!();

    // Example 5: Get tx output proof
    println!("5. gettxoutproof - Get a merkle proof that a tx is in a block");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "gettxoutproof",
        "params": [[txid], block_hash],
        // params: [array of txids, optional block_hash]
        "id": 5
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Generate an SPV proof of transaction inclusion for lightweight clients");
    println!("   Note: Returns hex-encoded merkle block; verify with verifytxoutproof");
    println!();

    // Example 6: Get UTXO (transaction output)
    println!("6. gettxout - Get details about an unspent transaction output");
    let vout = 0u32;
    let request = json!({
        "jsonrpc": "2.0",
        "method": "gettxout",
        "params": [txid, vout, true],  // [txid, vout, include_mempool]
        "id": 6
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Check if a UTXO exists and get its value, script, and confirmations");
    println!("   Note: include_mempool=true also checks unconfirmed outputs; returns null if spent");
    println!();

    // Example 7: Verify tx output proof
    println!("7. verifytxoutproof - Verify an SPV merkle proof and extract the proven txid(s)");
    let merkle_block_hex = "01000000..."; // hex-encoded merkle block from gettxoutproof
    let request = json!({
        "jsonrpc": "2.0",
        "method": "verifytxoutproof",
        "params": [merkle_block_hex],
        "id": 7
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Verify the proof returned by gettxoutproof; returns array of proven txids");
    println!("   Note: Pair with gettxoutproof to implement SPV verification for lightweight clients");
    println!();

    // Example 8: Get transaction details (enhanced)
    println!("8. gettransactiondetails - Get enriched transaction details with input values");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "gettransactiondetails",
        "params": [txid],
        "id": 8
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get decoded transaction with spent input values resolved (fee calculation)");
    println!("   Note: Extends getrawtransaction by fetching previous outputs to compute fees");
    println!();

    println!("Method Summary:");
    println!("  getrawtransaction    - Fetch tx by txid (hex or decoded JSON)");
    println!("  decoderawtransaction - Decode raw hex without broadcasting");
    println!("  createrawtransaction - Build an unsigned transaction");
    println!("  sendrawtransaction   - Broadcast a signed transaction");
    println!("  gettxoutproof        - Get SPV merkle proof of tx inclusion");
    println!("  gettxout             - Check if a UTXO exists and get its value");
    println!("  verifytxoutproof     - Verify an SPV proof and extract txids");
    println!("  gettransactiondetails - Enriched tx details with resolved input values");
    println!();
    println!("Typical workflow:");
    println!("  1. createrawtransaction  → unsigned hex");
    println!("  2. Sign with your wallet → signed hex");
    println!("  3. sendrawtransaction    → txid");
    println!();
    println!("To test with a running node:");
    println!("  1. Start node: bllvm-node --network testnet");
    println!("  2. Send requests with curl or any HTTP client");

    Ok(())
}
