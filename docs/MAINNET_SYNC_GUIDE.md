# Mainnet Sync Guide

Complete guide for syncing Bitcoin mainnet blockchain.

## ⚠️ Important Warnings

**Before starting mainnet sync:**

1. **This is REAL Bitcoin mainnet** - Real network, real blocks, real resources
2. **Resource Requirements:**
   - **Full archival sync**: ~600GB disk space
   - **Pruned sync**: ~13-15GB disk space (recommended)
   - **Bandwidth**: Significant (500GB+ for full sync, ~10GB for pruned)
   - **Time**: Days to weeks depending on connection and hardware
   - **RAM**: 8GB+ recommended (16GB+ for better performance)

3. **Security**: Mainnet requires proper security hardening (see SECURITY.md)

## Quick Start

### Option 1: Pruned Sync (Recommended - 13GB)

**Best for most users - keeps only recent blocks:**

```bash
cd blvm-node

# Start mainnet sync with pruning enabled
cargo run --release --features production -- --network mainnet \
  --data-dir ~/.local/share/blvm-mainnet \
  --rpc-addr 127.0.0.1:8332 \
  --listen-addr 0.0.0.0:8333
```

**What gets kept:**
- All block headers (~40MB)
- UTXO set (~13GB)
- Recent 144 blocks (~144MB)
- UTXO commitments (~1MB)
- **Total: ~13.2GB**

### Option 2: Full Archival Sync (~600GB)

**For users who need full blockchain history:**

```bash
cd blvm-node

# Start mainnet sync (archival mode - keeps all blocks)
cargo run --release --features production -- --network mainnet \
  --data-dir ~/.local/share/blvm-mainnet \
  --rpc-addr 127.0.0.1:8332 \
  --listen-addr 0.0.0.0:8333
```

**What gets kept:**
- Full blockchain: ~600GB
- UTXO set: ~13GB
- **Total: ~613GB**

## Configuration

### Create Configuration File

Create `~/.config/blvm/mainnet.toml`:

```toml
[network]
network = "mainnet"
listen_address = "0.0.0.0:8333"

[storage]
# For pruned sync (recommended)
mode = "pruned"
prune_mode = "aggressive"  # or "normal"
keep_from_height = 0
min_blocks_to_keep = 144

# For archival sync
# mode = "archival"

data_dir = "~/.local/share/blvm-mainnet"

[rpc]
enabled = true
listen_address = "127.0.0.1:8332"
# Set strong RPC credentials for mainnet!
# rpc_user = "your_username"
# rpc_password = "strong_password_here"

[logging]
level = "info"  # Use "debug" for more detail during sync

[features]
# Enable production features for better performance
production = true
```

### Start with Config File

```bash
cargo run --release --features production -- \
  --config ~/.config/blvm/mainnet.toml \
  --network mainnet
```

## Monitoring Sync Progress

### Via RPC (Recommended)

**Get blockchain info:**
```bash
curl -X POST http://localhost:8332 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc": "2.0", "method": "getblockchaininfo", "params": [], "id": 1}' \
  | jq '.result | {blocks, headers, verificationprogress, initialblockdownload}'
```

**Watch progress in real-time:**
```bash
watch -n 5 'curl -s -X POST http://localhost:8332 \
  -H "Content-Type: application/json" \
  -d "{\"jsonrpc\": \"2.0\", \"method\": \"getblockchaininfo\", \"params\": [], \"id\": 1}" \
  | jq ".result | {blocks, headers, verificationprogress: (.verificationprogress * 100 | floor | \"\(.)%\"), initialblockdownload, chainwork}"'
```

**Get peer information:**
```bash
curl -X POST http://localhost:8332 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc": "2.0", "method": "getpeerinfo", "params": [], "id": 2}' \
  | jq '.result | length'  # Number of connected peers
```

### Via Logs

Watch for these log messages:

**Parallel IBD (if 2+ peers):**
```
[INFO] Detected IBD (height: 0), checking for parallel IBD support...
[INFO] Attempting parallel IBD with N peers
[INFO] Starting parallel IBD from height 0 to X using N peers
[INFO] Downloading headers...
[INFO] Created M chunks for parallel download
[INFO] Processed X blocks (height: Y)
```

**Sequential sync (if < 2 peers):**
```
[INFO] Not enough peers for parallel IBD (have 1, need 2), will use sequential sync
```

**Progress updates:**
```
[INFO] Processed 1000 blocks (height: 1000)
[INFO] Processed 10000 blocks (height: 10000)
```

### Enable Verbose Logging

```bash
# Set RUST_LOG for more detail
RUST_LOG=info cargo run --release --features production -- --network mainnet

# Or for debug-level detail
RUST_LOG=debug cargo run --release --features production -- --network mainnet
```

## Expected Sync Time

### With Parallel IBD (2-4 peers)

- **Pruned sync**: ~2-4 hours (downloads ~10GB)
- **Full sync**: ~2-3 days (downloads ~500GB)

### With Sequential Sync (1 peer)

- **Pruned sync**: ~8-12 hours
- **Full sync**: ~1-2 weeks

**Factors affecting speed:**
- Number of connected peers (more = faster with parallel IBD)
- Network bandwidth
- Peer connection quality
- CPU speed (validation is CPU-intensive)
- Storage I/O performance (SSD recommended)

## Resource Usage During Sync

### Disk Space

**Monitor disk usage:**
```bash
# Watch data directory size
watch -n 10 'du -sh ~/.local/share/blvm-mainnet'
```

**Expected growth:**
- **Pruned**: Starts at ~0GB, grows to ~13GB
- **Archival**: Starts at ~0GB, grows to ~600GB

### Network Bandwidth

**Monitor network usage:**
```bash
# Install if needed: sudo apt-get install iftop
sudo iftop -i eth0  # Replace with your network interface
```

**Expected bandwidth:**
- **Pruned**: ~10GB total download
- **Archival**: ~500GB total download
- **Peak**: 10-50 Mbps during active sync

### CPU Usage

Validation is CPU-intensive. Expect:
- **Peak**: 50-100% CPU usage during validation
- **Average**: 20-50% CPU usage
- **Multi-core**: Parallel IBD uses multiple cores

### RAM Usage

- **Minimum**: 4GB
- **Recommended**: 8GB+
- **Optimal**: 16GB+

## Troubleshooting

### Sync Stalled

**Check:**
1. Are peers connected? (`getpeerinfo`)
2. Are blocks increasing? (`getblockchaininfo`)
3. Check logs for errors

**Solutions:**
```bash
# Restart node (it will resume from last synced block)
# The node automatically resumes from where it left off

# Check for network issues
ping 8.8.8.8

# Check firewall (port 8333 must be open for incoming connections)
sudo ufw status
```

### Slow Sync

**Optimize:**
1. **Enable parallel IBD**: Ensure 2+ peers connected
2. **Use SSD**: Much faster than HDD
3. **Increase bandwidth**: Better connection = faster sync
4. **Check CPU**: Faster CPU = faster validation

**Configuration tweaks:**
```toml
[storage]
# Increase cache size for better performance
cache_size_mb = 1024  # 1GB cache
```

### Out of Disk Space

**If running out of space:**
1. **Switch to pruned mode** (if not already)
2. **Clear old data** (if resyncing)
3. **Free up space** on disk

**Switch to pruned mode:**
```toml
[storage]
mode = "pruned"
prune_mode = "aggressive"
keep_from_height = 0
min_blocks_to_keep = 144
```

### Connection Issues

**If having trouble connecting to peers:**
1. **Check firewall**: Port 8333 must be open
2. **Check DNS**: DNS seeds should resolve
3. **Check network**: Internet connection must be working

**Test connectivity:**
```bash
# Test DNS resolution
nslookup seed.bitcoin.sipa.be

# Test port connectivity
telnet seed.bitcoin.sipa.be 8333
```

## Running in Background

### Using screen

```bash
# Start screen session
screen -S blvm-mainnet

# Start node
cargo run --release --features production -- --network mainnet

# Detach: Press Ctrl+A, then D
# Reattach: screen -r blvm-mainnet
```

### Using tmux

```bash
# Start tmux session
tmux new -s blvm-mainnet

# Start node
cargo run --release --features production -- --network mainnet

# Detach: Press Ctrl+B, then D
# Reattach: tmux attach -t blvm-mainnet
```

### Using systemd (Production)

Create `/etc/systemd/system/blvm-mainnet.service`:

```ini
[Unit]
Description=BLVM Bitcoin Mainnet Node
After=network.target

[Service]
Type=simple
User=bitcoin
WorkingDirectory=/opt/blvm-node
ExecStart=/opt/blvm-node/target/release/blvm --network mainnet --config /etc/blvm/mainnet.toml
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
```

**Enable and start:**
```bash
sudo systemctl enable blvm-mainnet
sudo systemctl start blvm-mainnet
sudo systemctl status blvm-mainnet
```

## Security Considerations

### RPC Security

**For mainnet, secure your RPC:**

```toml
[rpc]
enabled = true
listen_address = "127.0.0.1:8332"  # Only localhost
rpc_user = "strong_username"
rpc_password = "very_strong_password_here"
# Consider adding IP whitelist if exposing RPC
```

### Firewall

**Allow P2P port (8333) but restrict RPC:**
```bash
# Allow P2P (incoming connections)
sudo ufw allow 8333/tcp

# Restrict RPC to localhost only (already done via config)
# If exposing RPC, use firewall rules:
# sudo ufw allow from 192.168.1.0/24 to any port 8332
```

### Data Directory Permissions

```bash
# Secure data directory
chmod 700 ~/.local/share/blvm-mainnet
```

## Completion

### When Sync is Complete

You'll see:
- `initialblockdownload: false` in `getblockchaininfo`
- `verificationprogress: 1.0` (100%)
- Blocks count matches headers count
- Logs show: `[INFO] Node is synced`

### Post-Sync

**Verify sync:**
```bash
curl -X POST http://localhost:8332 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc": "2.0", "method": "getblockchaininfo", "params": [], "id": 1}' \
  | jq '.result | {blocks, headers, verificationprogress, initialblockdownload}'
```

**Expected output:**
```json
{
  "blocks": 850000,  // Current block height
  "headers": 850000,
  "verificationprogress": 1.0,
  "initialblockdownload": false
}
```

## Next Steps

After sync completes:
1. **Keep node running** to stay synced
2. **Monitor logs** for any issues
3. **Set up monitoring** (optional)
4. **Configure backups** (for archival nodes)
5. **Review security** settings

## Additional Resources

- [IBD Testing Guide](IBD_TESTING_GUIDE.md) - Testing IBD functionality
- [Configuration Guide](CONFIGURATION_GUIDE.md) - Complete configuration options
- [Security Guide](../SECURITY.md) - Security best practices
- [RPC Reference](RPC_REFERENCE.md) - RPC API documentation




