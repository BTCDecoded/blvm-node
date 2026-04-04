//! Example: Payment, vault, and pool RPC methods
//!
//! This example demonstrates the payment-related JSON-RPC methods available
//! in blvm-node. These methods cover BIP70 payment requests, CTV covenant
//! vaults, and payment pools — all blvm-node extensions beyond Bitcoin Core.
//!
//! This example shows the RPC request format. To test with a running node:
//!   1. Start blvm-node with BIP70: blvm-node --network testnet --features bip70-http,ctv
//!   2. Run this example: cargo run --example rpc-payment
//!
//! Or use curl:
//!   curl -X POST http://127.0.0.1:18332 \
//!     -H "Content-Type: application/json" \
//!     -d '{"jsonrpc":"2.0","method":"listpayments","params":[],"id":1}'
//!
//! Note: Vault and pool methods require the `ctv` feature flag.

use serde_json::json;

fn main() -> anyhow::Result<()> {
    println!("blvm-node Payment RPC Examples");
    println!("==================================");
    println!();
    println!("These methods handle BIP70 payments, CTV vaults, and payment pools.");
    println!("These are blvm-node extensions — not part of the Bitcoin Core API.");
    println!();

    let rpc_url = "http://127.0.0.1:18332"; // Testnet
                                            // let rpc_url = "http://127.0.0.1:8332"; // Mainnet

    println!("RPC Endpoint: {rpc_url}");
    println!();

    // ── Payment Request Methods ─────────────────────────────────────────────

    println!("=== Payment Request Methods ===");
    println!();

    // Example 1: Create payment request
    println!("1. createpaymentrequest - Create a BIP70 payment request");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "createpaymentrequest",
        "params": [
            // outputs: [{amount (satoshis), script_pubkey (hex)}]
            [{"amount": 100_000, "script_pubkey": "76a914...88ac"}],
            // merchant_data: optional hex string
            "deadbeef",
            // create_covenant: create CTV proof immediately (requires ctv feature)
            false
        ],
        "id": 1
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Generate a BIP70 payment request; returns {{payment_id, covenant_proof?}}");
    println!();

    // Example 2: Create covenant proof
    println!("2. createcovenantproof - Create a CTV covenant proof for an existing payment");
    let payment_id = "pay_abc123";
    let request = json!({
        "jsonrpc": "2.0",
        "method": "createcovenantproof",
        "params": [payment_id],
        "id": 2
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Create a CTV commitment proof for a payment request (requires ctv feature)");
    println!();

    // Example 3: Get payment state
    println!("3. getpaymentstate - Query the current state of a payment");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getpaymentstate",
        "params": [payment_id],
        "id": 3
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Poll payment lifecycle: request_created → in_mempool → settled → failed");
    println!();

    // Example 4: List payments
    println!("4. listpayments - List all tracked payment requests");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "listpayments",
        "params": [],
        "id": 4
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get a summary of all payment IDs and their current states");
    println!();

    // ── Vault Methods ───────────────────────────────────────────────────────

    println!("=== Vault Methods (requires ctv feature) ===");
    println!();

    // Example 5: Create vault
    println!("5. createvault - Create a CTV-enforced time-locked vault");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "createvault",
        "params": {
            "vault_id": "vault_001",
            "deposit_amount": 1_000_000,  // satoshis
            "withdrawal_script": "76a914...88ac",  // hex script pubkey
            "config": {}  // optional VaultConfig
        },
        "id": 5
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Create a vault that enforces a withdrawal delay via CTV covenants");
    println!();

    // Example 6: Get vault state
    println!("6. getvaultstate - Query the state of an existing vault");
    let vault_id = "vault_001";
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getvaultstate",
        "params": {"vault_id": vault_id},
        "id": 6
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Inspect vault deposit amount, withdrawal script, and status");
    println!();

    // Example 7: Unvault (begin withdrawal)
    println!("7. unvault - Initiate the first step of a vault withdrawal");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "unvault",
        "params": {
            "vault_id": vault_id,
            "unvault_script": "76a914...88ac"  // hex script pubkey for unvault output
        },
        "id": 7
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Move vault into the unvault state; starts the time-lock countdown");
    println!();

    // Example 8: Withdraw from vault
    println!("8. withdrawfromvault - Complete a vault withdrawal after the time-lock expires");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "withdrawfromvault",
        "params": {
            "vault_id": vault_id,
            "withdrawal_script": "76a914...88ac",  // final destination script
            "current_block_height": 800_100u64
        },
        "id": 8
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Complete withdrawal once the CTV time-lock has elapsed");
    println!();

    // ── Pool Methods ────────────────────────────────────────────────────────

    println!("=== Pool Methods (requires ctv feature) ===");
    println!();

    // Example 9: Create pool
    println!("9. createpool - Create a multi-party payment pool");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "createpool",
        "params": {
            "pool_id": "pool_001",
            "initial_participants": [
                // [participant_id, contribution_sats, script_pubkey_hex]
                ["alice", 500_000, "76a914aaa...88ac"],
                ["bob",   500_000, "76a914bbb...88ac"]
            ],
            "config": {}  // optional PoolConfig
        },
        "id": 9
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Create a CTV-backed pool for multi-party payment channels");
    println!();

    // Example 10: Get pool state
    println!("10. getpoolstate - Query the state of a payment pool");
    let pool_id = "pool_001";
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getpoolstate",
        "params": {"pool_id": pool_id},
        "id": 10
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Inspect pool participants, contributions, and current state");
    println!();

    // Example 11: Join pool
    println!("11. joinpool - Add a new participant to an existing pool");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "joinpool",
        "params": {
            "pool_id": pool_id,
            "participant_id": "carol",
            "contribution": 250_000,
            "script_pubkey": "76a914ccc...88ac"
        },
        "id": 11
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Add a participant to an open pool before distribution");
    println!();

    // Example 12: Distribute pool
    println!("12. distributepool - Distribute pool funds to participants");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "distributepool",
        "params": {
            "pool_id": pool_id,
            "distribution": [
                // [participant_id, amount_sats]
                ["alice", 600_000],
                ["bob",   400_000]
            ]
        },
        "id": 12
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Finalize pool payout; returns covenant_proof for the distribution tx");
    println!();

    println!("Method Summary:");
    println!("  createpaymentrequest - Create a BIP70 payment request");
    println!("  createcovenantproof  - Create a CTV proof for a payment request");
    println!("  getpaymentstate      - Query payment lifecycle state");
    println!("  listpayments         - List all tracked payments");
    println!("  createvault          - Create a CTV time-locked vault");
    println!("  getvaultstate        - Inspect vault state");
    println!("  unvault              - Begin vault withdrawal (starts time-lock)");
    println!("  withdrawfromvault    - Complete vault withdrawal after time-lock");
    println!("  createpool           - Create a multi-party payment pool");
    println!("  getpoolstate         - Inspect pool participants and state");
    println!("  joinpool             - Add a participant to a pool");
    println!("  distributepool       - Finalize pool payout with CTV proof");
    println!();
    println!("Note: Vault and pool methods require `ctv` feature: cargo build --features ctv");
    println!();
    println!("To test with a running node:");
    println!("  1. Start node: blvm-node --network testnet --features bip70-http");
    println!("  2. Send requests with curl or any HTTP client");

    Ok(())
}
