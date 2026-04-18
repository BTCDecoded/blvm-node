//! Example: Batch, congestion, and script analysis RPC methods
//!
//! This example demonstrates transaction batching, congestion control, and
//! script analysis RPC methods available in blvm-node.
//!
//! This example shows the RPC request format. To test with a running node:
//!   1. Start blvm-node: blvm-node --network testnet
//!   2. Run this example: cargo run --example rpc-batch
//!
//! Or use curl:
//!   curl -X POST http://127.0.0.1:18332 \
//!     -H "Content-Type: application/json" \
//!     -d '{"jsonrpc":"2.0","method":"getcongestion","params":[],"id":1}'
//!
//! Note: createbatch, addtobatch, broadcastbatch require the `ctv` feature.
//! Note: getdescriptorinfo, analyzepsbt require the `miniscript` feature.

use serde_json::json;

fn main() -> anyhow::Result<()> {
    println!("blvm-node Batch & Script Analysis RPC Examples");
    println!("==================================================");
    println!();
    println!("These methods handle transaction batching, congestion monitoring,");
    println!("and script/descriptor/PSBT analysis.");
    println!();

    let rpc_url = "http://127.0.0.1:18332"; // Testnet
                                            // let rpc_url = "http://127.0.0.1:8332"; // Mainnet

    println!("RPC Endpoint: {rpc_url}");
    println!();

    // ── Congestion Methods ──────────────────────────────────────────────────

    println!("=== Congestion Monitoring ===");
    println!();

    // Example 1: Get congestion
    println!("1. getcongestion - Get current mempool congestion metrics");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getcongestion",
        "params": [],
        "id": 1
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Monitor mempool fee rates and congestion level to decide batch timing");
    println!("   Returns: {{mempool_size, avg_fee_rate, median_fee_rate, estimated_blocks}}");
    println!();

    // Example 2: Get congestion metrics (alias)
    println!("2. getcongestionmetrics - Alias for getcongestion with identical output");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getcongestionmetrics",
        "params": [],
        "id": 2
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Same as getcongestion; provided for API symmetry with batch methods");
    println!();

    // ── Batch Methods ───────────────────────────────────────────────────────

    println!("=== Transaction Batching (requires ctv feature) ===");
    println!();

    // Example 3: Create batch
    println!("3. createbatch - Create a new transaction batch");
    let batch_id = "batch_001";
    let request = json!({
        "jsonrpc": "2.0",
        "method": "createbatch",
        "params": {
            "batch_id": batch_id,
            "target_fee_rate": 5  // sat/vbyte (optional)
        },
        "id": 3
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Initialize a batch to collect multiple payment outputs into one transaction");
    println!(
        "   Note: target_fee_rate controls when broadcastbatch fires; omit for default policy"
    );
    println!();

    // Example 4: Add to batch
    println!("4. addtobatch - Add a transaction to an existing batch");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "addtobatch",
        "params": {
            "batch_id": batch_id,
            "tx_id": "pay_xyz789",
            "outputs": [
                {"amount": 100_000, "script_pubkey": "76a914aaa...88ac"},
                {"amount": 200_000, "script_pubkey": "76a914bbb...88ac"}
            ],
            "priority": "normal",  // "low" | "normal" | "high" | "urgent"
            "deadline": null       // optional Unix timestamp
        },
        "id": 4
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Accumulate payment outputs; batching reduces per-payment fee overhead");
    println!(
        "   Note: priority affects ordering within the batch; deadline triggers early broadcast"
    );
    println!();

    // Example 5: Broadcast batch
    println!("5. broadcastbatch - Broadcast a batch when congestion conditions are optimal");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "broadcastbatch",
        "params": {
            "batch_id": batch_id
        },
        "id": 5
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Trigger batch broadcast; returns covenant_proof for the aggregated tx");
    println!("   Note: The congestion manager may delay broadcast until fee rate meets target");
    println!();

    // ── Script Analysis Methods ─────────────────────────────────────────────

    println!("=== Script Analysis (requires miniscript feature) ===");
    println!();

    // Example 6: Get descriptor info
    println!("6. getdescriptorinfo - Analyze an output descriptor");
    let descriptor = "wpkh(02f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9)";
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getdescriptorinfo",
        "params": [descriptor],
        "id": 6
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Parse and validate a descriptor; get checksum, script type, and is_range");
    println!("   Note: Descriptors encode spending conditions (wpkh, wsh, tr, multi, etc.)");
    println!();

    // Example 7: Analyze PSBT
    println!("7. analyzepsbt - Analyze a Partially Signed Bitcoin Transaction");
    let psbt_base64 = "cHNidP8BAHUCAAAAASaBcTce3/KF6Tet7qSze3gADAVmy7OtZGQXE8pn/...";
    let request = json!({
        "jsonrpc": "2.0",
        "method": "analyzepsbt",
        "params": [psbt_base64],
        "id": 7
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Inspect PSBT roles, missing signatures, and estimated fee before finalizing");
    println!("   Note: Returns per-input analysis showing which signers have signed");
    println!();

    println!("Method Summary:");
    println!("  getcongestion       - Mempool fee rate and congestion metrics");
    println!("  getcongestionmetrics - Alias for getcongestion");
    println!("  createbatch         - Initialize a transaction batch (ctv feature)");
    println!("  addtobatch          - Add outputs to a batch (ctv feature)");
    println!("  broadcastbatch      - Broadcast batch when conditions are met (ctv feature)");
    println!(
        "  getdescriptorinfo   - Parse and validate an output descriptor (miniscript feature)"
    );
    println!("  analyzepsbt         - Inspect PSBT signing status and fee (miniscript feature)");
    println!();
    println!("Typical batching workflow:");
    println!("  1. getcongestion           → check current fee environment");
    println!("  2. createbatch             → open a new batch");
    println!("  3. addtobatch (N times)    → accumulate payment outputs");
    println!("  4. broadcastbatch          → submit when fee rate is acceptable");
    println!();
    println!("To test with a running node:");
    println!("  1. Start node: blvm-node --network testnet");
    println!("  2. Send requests with curl or any HTTP client");

    Ok(())
}
