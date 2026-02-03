# Mainnet IBD Quick Start

## ✅ Configuration Verified

Your mainnet IBD is configured with:
- ✅ **Incremental pruning during IBD**: ENABLED
- ✅ **UTXO commitments**: ENABLED (via feature flag)
- ✅ **Parallel IBD**: Will use automatically if 2+ peers available
- ✅ **Storage bounded**: Will stay at ~13GB instead of 600GB

## 🚀 Start IBD

### Option 1: Use the Script (Recommended)

```bash
cd blvm-node
./start-mainnet-ibd.sh
```

This script will:
1. Check prerequisites
2. Build with `production` and `utxo-commitments` features
3. Start mainnet sync with incremental pruning

### Option 2: Manual Start

```bash
cd blvm-node

# Build with required features
cargo build --release --features "production,utxo-commitments"

# Start IBD
./target/release/blvm \
    --config mainnet-ibd-config.toml \
    --network mainnet \
    --data-dir ~/.local/share/blvm-mainnet \
    --rpc-addr 127.0.0.1:8332 \
    --listen-addr 0.0.0.0:8333
```

## 📊 Monitor Progress

**In another terminal:**

```bash
# Watch sync progress
watch -n 5 'curl -s -X POST http://localhost:8332 \
  -H "Content-Type: application/json" \
  -d "{\"jsonrpc\": \"2.0\", \"method\": \"getblockchaininfo\", \"params\": [], \"id\": 1}" \
  | jq ".result | {blocks, headers, verificationprogress: (.verificationprogress * 100 | floor | \"\(.)%\"), initialblockdownload}"'
```

## 🔍 What to Expect

### Initial Phase (First 288 blocks)
- Downloads headers first (~40MB)
- Downloads and validates blocks
- **No pruning yet** (waiting for 288 block threshold)

### After 288 Blocks
- **Incremental pruning starts**
- Old blocks are pruned (keeping only last 144)
- Storage stays bounded at ~13GB
- UTXO commitments generated for verification

### Log Messages to Watch For

```
[INFO] Detected IBD (height: 0), checking for parallel IBD support...
[INFO] Attempting parallel IBD with N peers
[INFO] Starting parallel IBD from height 0 to X using N peers
[INFO] Downloading headers...
[INFO] Processed 1000 blocks (height: 1000)
[INFO] Incremental pruning during IBD: X blocks pruned, Y bytes freed
```

## 📈 Expected Timeline

- **With parallel IBD (2-4 peers)**: 2-4 hours
- **With sequential sync (1 peer)**: 8-12 hours
- **Storage**: Will stabilize at ~13GB after 288 blocks

## ✅ Verification

After sync completes, verify:

```bash
curl -X POST http://localhost:8332 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc": "2.0", "method": "getblockchaininfo", "params": [], "id": 1}' \
  | jq '.result | {blocks, headers, verificationprogress, initialblockdownload}'
```

**Expected:**
- `initialblockdownload: false`
- `verificationprogress: 1.0` (100%)
- `blocks` matches `headers`

## 🛑 Stop IBD

Press `Ctrl+C` to stop. The node will resume from the last synced block on next start.

## 📝 Configuration

Configuration file: `mainnet-ibd-config.toml`

Key settings:
- `incremental_prune_during_ibd = true` ✅
- `prune_window_size = 144` (keep last 144 blocks)
- `min_blocks_for_incremental_prune = 288` (start after 288 blocks)
- `keep_commitments = true` (UTXO commitments enabled)

## 🔧 Troubleshooting

### Build fails with "utxo-commitments" feature

Make sure you're building with the feature:
```bash
cargo build --release --features "production,utxo-commitments"
```

### Pruning not happening

Check logs for:
- `incremental_prune_during_ibd` enabled
- UTXO commitments feature available
- At least 288 blocks synced

### Slow sync

- Check peer count: `getpeerinfo`
- Ensure parallel IBD is used (2+ peers)
- Check network bandwidth
- Use SSD for storage (much faster than HDD)




