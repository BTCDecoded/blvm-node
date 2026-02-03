# IBD Testing Guide

Complete guide for testing Initial Blockchain Download (IBD) functionality.

## Quick Start

### 1. Run Existing Unit Tests

```bash
cd blvm-node

# Run all IBD-related tests
cargo test parallel_ibd

# Run with verbose output
cargo test parallel_ibd -- --nocapture

# Run integration tests
cargo test --test integration parallel_ibd_tests
```

### 2. Test IBD Manually

#### Option A: Fresh Start (Regtest - Safe for Testing)

```bash
# Start node in regtest mode (creates isolated network)
cd blvm-node
cargo run -- --network regtest

# The node will detect IBD (height 0) and attempt parallel IBD if 2+ peers available
```

#### Option B: Testnet IBD

```bash
# Start node in testnet mode
cargo run -- --network testnet

# Node will connect to testnet and start IBD
# Monitor logs for parallel IBD messages
```

#### Option C: Force IBD by Clearing Data

```bash
# Find your data directory (default: ~/.local/share/blvm or configured path)
# WARNING: This deletes all blockchain data!

# Clear storage to force IBD
rm -rf ~/.local/share/blvm/chainstate
rm -rf ~/.local/share/blvm/blocks

# Start node - it will detect height 0 and trigger IBD
cargo run -- --network testnet
```

## Testing Scenarios

### Scenario 1: Parallel IBD with Multiple Peers

**Setup:**
1. Start node in testnet mode
2. Ensure node connects to at least 2 peers
3. Clear blockchain data to force IBD

**Expected Behavior:**
- Node detects IBD (height == 0)
- Node detects 2+ peers
- Parallel IBD starts automatically
- Logs show:
  ```
  [INFO] Detected IBD (height: 0), checking for parallel IBD support...
  [INFO] Attempting parallel IBD with N peers
  [INFO] Starting parallel IBD from height 0 to X using N peers
  [INFO] Downloading headers...
  [INFO] Created M chunks for parallel download
  [INFO] Downloading chunk from peer X: heights Y to Z
  ```

**Verification:**
```bash
# Check sync progress via RPC
curl -X POST http://localhost:8332 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc": "2.0", "method": "getblockchaininfo", "params": [], "id": 1}'

# Look for:
# - "initialblockdownload": true
# - "blocks" increasing
# - "verificationprogress" increasing
```

### Scenario 2: Sequential Fallback (Single Peer)

**Setup:**
1. Start node with only 1 peer connection
2. Clear blockchain data

**Expected Behavior:**
- Node detects IBD
- Node detects < 2 peers
- Falls back to sequential sync
- Logs show:
  ```
  [INFO] Not enough peers for parallel IBD (have 1, need 2), will use sequential sync
  ```

### Scenario 3: Parallel IBD Failure Recovery

**Setup:**
1. Start parallel IBD
2. Simulate peer disconnection (or let peer disconnect naturally)

**Expected Behavior:**
- Parallel IBD continues with remaining peers
- Failed chunks are logged but don't stop entire sync
- Logs show warnings for failed chunks:
  ```
  [WARN] Failed to download chunk starting at height X: ...
  ```

## Monitoring IBD Progress

### Via Logs

Watch for these log messages:

```
[INFO] Starting parallel IBD from height X to Y using N peers
[INFO] Downloading headers...
[INFO] Received M headers from peer
[INFO] Created K chunks for parallel download
[INFO] Downloaded chunk starting at height X (M blocks)
[INFO] Validating and storing M blocks...
[INFO] Processed X blocks (height: Y)
[INFO] Parallel IBD completed: M blocks synced
```

### Via RPC

```bash
# Get blockchain info
curl -X POST http://localhost:8332 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc": "2.0", "method": "getblockchaininfo", "params": [], "id": 1}'

# Get peer info
curl -X POST http://localhost:8332 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc": "2.0", "method": "getpeerinfo", "params": [], "id": 2}'

# Get sync status
curl -X POST http://localhost:8332 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc": "2.0", "method": "getblockchaininfo", "params": [], "id": 3}' \
  | jq '.result | {blocks, headers, verificationprogress, initialblockdownload}'
```

## Configuration for Testing

### Custom IBD Configuration

You can customize IBD behavior via code:

```rust
use blvm_node::node::parallel_ibd::{ParallelIBD, ParallelIBDConfig};

let config = ParallelIBDConfig {
    num_workers: 4,              // Number of parallel workers
    chunk_size: 500,             // Smaller chunks for testing
    max_concurrent_per_peer: 2,  // Lower concurrency for testing
    checkpoint_interval: 5000,   // More frequent checkpoints
    download_timeout_secs: 10,   // Shorter timeout for testing
};
```

### Enable Debug Logging

```bash
# Set RUST_LOG environment variable
RUST_LOG=debug cargo run -- --network testnet

# Or for more detail
RUST_LOG=trace cargo run -- --network testnet
```

## Integration Test Development

### Creating New IBD Tests

Add tests to `blvm-node/tests/integration/parallel_ibd_tests.rs`:

```rust
#[tokio::test]
async fn test_parallel_ibd_with_real_network() {
    // Setup: Create storage, network manager, etc.
    // Test: Trigger parallel IBD
    // Verify: Blocks downloaded and validated correctly
}
```

### Running Integration Tests

```bash
# Run all integration tests
cargo test --test integration

# Run specific test
cargo test --test integration test_parallel_ibd_chunk_creation
```

## Performance Testing

### Benchmark IBD Speed

1. **Clear blockchain data**
2. **Start node and time IBD**
3. **Compare parallel vs sequential**

```bash
# Time the IBD process
time cargo run -- --network testnet

# Monitor progress
watch -n 1 'curl -s -X POST http://localhost:8332 \
  -H "Content-Type: application/json" \
  -d "{\"jsonrpc\": \"2.0\", \"method\": \"getblockchaininfo\", \"params\": [], \"id\": 1}" \
  | jq ".result.blocks"'
```

### Expected Performance

- **Sequential IBD**: ~8-10 hours for 800K blocks (single peer)
- **Parallel IBD (4 peers)**: ~2-3 hours for 800K blocks
- **Speedup**: ~3-4x with multiple peers

## Troubleshooting

### IBD Not Starting

**Check:**
1. Node height is 0: `getblockchaininfo` should show `blocks: 0`
2. Peers connected: `getpeerinfo` should show 2+ peers
3. Logs for IBD detection messages

### Parallel IBD Not Used

**Possible reasons:**
- Less than 2 peers connected
- `production` feature not enabled
- Network manager not available

**Solution:**
```bash
# Build with production feature
cargo build --release --features production

# Or run with feature
cargo run --features production -- --network testnet
```

### Slow IBD

**Check:**
1. Network bandwidth
2. Peer connection quality
3. Storage I/O performance
4. CPU usage (validation is CPU-intensive)

**Optimize:**
- Increase `chunk_size` for faster downloads
- Increase `max_concurrent_per_peer` (if peers support it)
- Use faster storage backend (SSD recommended)

## Test Checklist

- [ ] Unit tests pass (`cargo test parallel_ibd`)
- [ ] Integration tests pass (`cargo test --test integration parallel_ibd_tests`)
- [ ] Parallel IBD starts with 2+ peers
- [ ] Sequential fallback works with 1 peer
- [ ] Blocks are validated correctly
- [ ] Progress is tracked accurately
- [ ] RPC shows correct IBD status
- [ ] Node completes IBD successfully
- [ ] Performance meets expectations (2-4x speedup with multiple peers)

## Next Steps

1. **Add more integration tests** for edge cases
2. **Benchmark performance** with different configurations
3. **Test error recovery** (peer failures, timeouts)
4. **Test with different network conditions** (slow peers, packet loss)




