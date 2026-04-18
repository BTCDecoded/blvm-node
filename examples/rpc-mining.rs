//! Example: Mining RPC methods
//!
//! This example demonstrates the mining-related JSON-RPC methods available
//! in blvm-node. These methods support block template generation, block
//! submission, fee estimation, and transaction prioritization.
//!
//! This example shows the RPC request format. To test with a running node:
//!   1. Start blvm-node: blvm-node --network testnet
//!   2. Run this example: cargo run --example rpc-mining
//!
//! Or use curl:
//!   curl -X POST http://127.0.0.1:18332 \
//!     -H "Content-Type: application/json" \
//!     -d '{"jsonrpc":"2.0","method":"getmininginfo","params":[],"id":1}'

use serde_json::json;

fn main() -> anyhow::Result<()> {
    println!("blvm-node Mining RPC Examples");
    println!("================================");
    println!();
    println!("These methods support mining operations and fee estimation.");
    println!("All methods are Bitcoin Core-compatible.");
    println!();

    let rpc_url = "http://127.0.0.1:18332"; // Testnet
                                            // let rpc_url = "http://127.0.0.1:8332"; // Mainnet

    println!("RPC Endpoint: {rpc_url}");
    println!();
    println!("Example RPC Requests:");
    println!();

    // Example 1: Get mining info
    println!("1. getmininginfo - Get current mining state");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getmininginfo",
        "params": [],
        "id": 1
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Check block height, current difficulty, network hashrate");
    println!();

    // Example 2: Get block template (GBT)
    println!("2. getblocktemplate - Get a block template for mining (BIP22/BIP23)");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getblocktemplate",
        "params": [{"rules": ["segwit"]}],
        "id": 2
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get transactions, target, and metadata for constructing a block");
    println!("   Note: Pass {{\"rules\":[\"segwit\"]}} to signal SegWit support");
    println!();

    // Example 3: Generate blocks to address (regtest only)
    println!(
        "3. generatetoaddress - Mine blocks immediately and send coinbase to address (regtest)"
    );
    let nblocks = 1u64;
    let address = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080"; // regtest bech32 address
    let request = json!({
        "jsonrpc": "2.0",
        "method": "generatetoaddress",
        "params": [nblocks, address],  // [nblocks, address, maxtries (optional)]
        "id": 3
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Instantly mine N blocks in regtest mode; coinbase pays to address");
    println!("   Note: Only works with a regtest node; use for local testing and wallet funding");
    println!();

    // Example 4: Submit a mined block
    println!("4. submitblock - Submit a solved block to the network");
    let raw_block_hex = "00000020..."; // Replace with actual solved block hex
    let request = json!({
        "jsonrpc": "2.0",
        "method": "submitblock",
        "params": [raw_block_hex],
        "id": 4
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Broadcast a block your miner solved");
    println!("   Note: Replace hex with your complete serialized block");
    println!();

    // Example 5: Estimate smart fee
    println!("5. estimatesmartfee - Estimate fee rate for confirmation within N blocks");
    let target_blocks = 6u64;
    let request = json!({
        "jsonrpc": "2.0",
        "method": "estimatesmartfee",
        "params": [target_blocks, "ECONOMICAL"],  // mode: "ECONOMICAL" | "CONSERVATIVE"
        "id": 5
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get recommended fee rate (BTC/kB) for N-block confirmation");
    println!("   Note: ECONOMICAL = lower fee; CONSERVATIVE = higher confidence");
    println!();

    // Example 6: Prioritise transaction
    println!("6. prioritisetransaction - Adjust a tx's priority in the mempool");
    let txid = "0000000000000000000000000000000000000000000000000000000000000000";
    let fee_delta_satoshis = 10_000i64; // Add 10,000 satoshis to effective fee
    let request = json!({
        "jsonrpc": "2.0",
        "method": "prioritisetransaction",
        "params": [txid, null, fee_delta_satoshis],
        // params: [txid, dummy (ignored), fee_delta_in_satoshis]
        "id": 6
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Boost or penalize a tx's effective fee for block inclusion");
    println!("   Note: fee_delta is in satoshis; positive = higher priority");
    println!();

    println!("Method Summary:");
    println!("  getmininginfo         - Block height, difficulty, network hashrate");
    println!("  getblocktemplate      - Block template for GBT-compatible miners (BIP22/23)");
    println!(
        "  generatetoaddress     - Mine N blocks instantly on regtest, pay coinbase to address"
    );
    println!("  submitblock           - Submit a solved block hex to the network");
    println!("  estimatesmartfee      - Fee rate estimate for N-block target");
    println!("  prioritisetransaction - Modify effective fee of a mempool transaction");
    println!();
    println!("To test with a running node:");
    println!("  1. Start node: blvm-node --network testnet");
    println!("  2. Send requests with curl or any HTTP client");

    Ok(())
}
