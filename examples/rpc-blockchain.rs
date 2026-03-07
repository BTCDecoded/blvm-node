//! Example: Blockchain RPC methods
//!
//! This example demonstrates the blockchain-related JSON-RPC methods available
//! in bllvm-node. These methods let you query chain state, blocks, and headers.
//!
//! This example shows the RPC request format. To test with a running node:
//!   1. Start bllvm-node: bllvm-node --network testnet
//!   2. Run this example: cargo run --example rpc-blockchain
//!
//! Or use curl:
//!   curl -X POST http://127.0.0.1:18332 \
//!     -H "Content-Type: application/json" \
//!     -d '{"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1}'

use serde_json::json;

fn main() -> anyhow::Result<()> {
    println!("bllvm-node Blockchain RPC Examples");
    println!("====================================");
    println!();
    println!("These methods query blockchain state, blocks, and headers.");
    println!("All methods are Bitcoin Core-compatible.");
    println!();

    let rpc_url = "http://127.0.0.1:18332"; // Testnet
                                            // let rpc_url = "http://127.0.0.1:8332"; // Mainnet

    println!("RPC Endpoint: {rpc_url}");
    println!();
    println!("Example RPC Requests:");
    println!();

    // Example 1: Get blockchain info
    println!("1. getblockchaininfo - Get overall chain state");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getblockchaininfo",
        "params": [],
        "id": 1
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Check chain, block height, best block hash, IBD status");
    println!();

    // Example 2: Get block count
    println!("2. getblockcount - Get current block height");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getblockcount",
        "params": [],
        "id": 2
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Quick check of chain height without full chain info");
    println!();

    // Example 3: Get best block hash
    println!("3. getbestblockhash - Get the hash of the best (tip) block");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getbestblockhash",
        "params": [],
        "id": 3
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get the current chain tip hash for syncing");
    println!();

    // Example 4: Get block hash by height
    println!("4. getblockhash - Get block hash at a given height");
    let height = 800_000u64;
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getblockhash",
        "params": [height],
        "id": 4
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Translate a block height to its hash");
    println!("   Note: Replace height with the block number you want");
    println!();

    // Example 5: Get block header
    println!("5. getblockheader - Get a block header by hash");
    let block_hash = "000000000000000000024bead8df69990852c202db0e0097c1a12ea637d7e96d";
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getblockheader",
        "params": [block_hash, true],  // true = verbose (JSON), false = hex
        "id": 5
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get header metadata (time, difficulty, nonce) without full block data");
    println!("   Note: Replace block_hash with actual hash; set verbose=false for raw hex");
    println!();

    // Example 6: Get full block
    println!("6. getblock - Get a full block by hash");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getblock",
        "params": [block_hash, 2],  // verbosity: 0=hex, 1=json, 2=json+tx details
        "id": 6
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Retrieve full block data including all transactions");
    println!("   Note: verbosity=2 includes decoded transaction inputs and outputs");
    println!();

    // Example 7: Get current difficulty
    println!("7. getdifficulty - Get the proof-of-work difficulty target");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getdifficulty",
        "params": [],
        "id": 7
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get current mining difficulty as a multiple of minimum difficulty");
    println!();

    // Example 8: Get UTXO set info
    println!("8. gettxoutsetinfo - Get statistics about the UTXO set");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "gettxoutsetinfo",
        "params": [],
        "id": 8
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get total UTXO count, total supply, and database stats");
    println!();

    // Example 9: Verify chain
    println!("9. verifychain - Verify blockchain database integrity");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "verifychain",
        "params": [3, 6],  // checklevel (0-4), numblocks (0=all)
        "id": 9
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Verify block and UTXO set consistency (checklevel 0-4, higher=more thorough)");
    println!("   Note: checklevel 3 checks all transactions; numblocks=0 checks entire chain");
    println!();

    // Example 10: Get chain tips
    println!("10. getchaintips - List all known chain tips including orphaned blocks");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getchaintips",
        "params": [],
        "id": 10
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Inspect main chain tip and any orphaned forks (status: active/valid/invalid)");
    println!();

    // Example 11: Get chain tx stats
    println!("11. getchaintxstats - Transaction throughput statistics for a block window");
    let nblocks = 2016u64; // one difficulty period
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getchaintxstats",
        "params": [nblocks],  // optional: [nblocks, blockhash]
        "id": 11
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Get tx rate and total tx count over the last N blocks (default: one difficulty period)");
    println!("   Note: Omit params for default window; pass blockhash as second param to anchor the window");
    println!();

    // Example 12: Get block stats
    println!("12. getblockstats - Compute statistics for a specific block");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getblockstats",
        "params": [800_000u64, ["txs", "total_size", "totalfee"]],
        // params: [hash_or_height, stats (optional array to filter fields)]
        "id": 12
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Per-block stats (fees, sizes, tx count); pass height or hash");
    println!("   Note: Omit stats array for all fields; specify subset for efficient partial queries");
    println!();

    // Example 13: Prune blockchain
    println!("13. pruneblockchain - Prune stored block data up to a given height or timestamp");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "pruneblockchain",
        "params": [800_000u64],  // height (or Unix timestamp if > 1,000,000,000)
        "id": 13
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Free disk space by pruning old block data (node must be started with prune=N)");
    println!("   Note: Pass a height to prune up to that point; pass a timestamp to prune by time");
    println!();

    // Example 14: Get prune info
    println!("14. getpruneinfo - Get pruning status and available range");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "getpruneinfo",
        "params": [],
        "id": 14
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Check if pruning is enabled and the earliest block still available");
    println!();

    // Example 15: Invalidate block
    println!("15. invalidateblock - Mark a block as permanently invalid");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "invalidateblock",
        "params": [block_hash],
        "id": 15
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Force the node to ignore a block and its descendants (useful for testing)");
    println!("   Note: Counterpart to reconsiderblock; requires node restart or reconsiderblock to undo");
    println!();

    // Example 16: Reconsider block
    println!("16. reconsiderblock - Undo the effect of a previous invalidateblock call");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "reconsiderblock",
        "params": [block_hash],
        "id": 16
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Re-enable a block previously marked invalid with invalidateblock");
    println!();

    // Example 17: Wait for new block
    println!("17. waitfornewblock - Long-poll until a new block arrives");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "waitfornewblock",
        "params": [5000],  // timeout in milliseconds (0 = no timeout)
        "id": 17
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Block until the next block is received; returns {hash, height}");
    println!("   Note: Timeout in milliseconds; use 0 for infinite wait");
    println!();

    // Example 18: Wait for block
    println!("18. waitforblock - Long-poll until a specific block hash is seen");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "waitforblock",
        "params": [block_hash, 5000],  // [blockhash, timeout_ms]
        "id": 18
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Block until the node sees the given block hash; returns {hash, height}");
    println!();

    // Example 19: Wait for block height
    println!("19. waitforblockheight - Long-poll until chain reaches a given height");
    let request = json!({
        "jsonrpc": "2.0",
        "method": "waitforblockheight",
        "params": [800_001u64, 5000],  // [height, timeout_ms]
        "id": 19
    });
    println!("   Request: {}", serde_json::to_string_pretty(&request)?);
    println!("   Use: Block until the chain reaches or exceeds the given height; returns {hash, height}");
    println!("   Note: All wait* methods support long-polling for real-time chain monitoring");
    println!();

    println!("Method Summary:");
    println!("  getblockchaininfo  - Full chain state (height, hash, IBD, difficulty)");
    println!("  getblockcount      - Current block height (integer)");
    println!("  getbestblockhash   - Tip block hash");
    println!("  getblockhash       - Hash at a given height");
    println!("  getblockheader     - Block header by hash (JSON or hex)");
    println!("  getblock           - Full block by hash (hex, JSON, or JSON+tx details)");
    println!("  getdifficulty      - Current proof-of-work difficulty");
    println!("  gettxoutsetinfo    - UTXO set size and total supply");
    println!("  verifychain        - Verify blockchain database integrity");
    println!("  getchaintips       - All known chain tips (main + orphans)");
    println!("  getchaintxstats    - Transaction throughput over a block window");
    println!("  getblockstats      - Per-block statistics (fees, sizes, tx count)");
    println!("  pruneblockchain    - Prune block data up to a height or timestamp");
    println!("  getpruneinfo       - Pruning status and earliest available block");
    println!("  invalidateblock    - Mark a block permanently invalid");
    println!("  reconsiderblock    - Undo a previous invalidateblock");
    println!("  waitfornewblock    - Long-poll for the next block");
    println!("  waitforblock       - Long-poll for a specific block hash");
    println!("  waitforblockheight - Long-poll until chain reaches a height");
    println!();
    println!("To test with a running node:");
    println!("  1. Start node: bllvm-node --network testnet");
    println!("  2. Send requests with curl or any HTTP client");

    Ok(())
}
