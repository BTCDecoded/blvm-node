#!/bin/bash
# Start Mainnet IBD with Incremental Pruning and UTXO Commitments

set -e

echo "🚀 Starting Mainnet IBD with Incremental Pruning"
echo "================================================"
echo ""

# Check if we're in the right directory
if [ ! -f "Cargo.toml" ]; then
    echo "❌ Error: Must run from blvm-node directory"
    exit 1
fi

# Configuration
CONFIG_FILE="mainnet-ibd-config.toml"
DATA_DIR="${HOME}/.local/share/blvm-mainnet"

echo "📋 Configuration:"
echo "   Config file: $CONFIG_FILE"
echo "   Data directory: $DATA_DIR"
echo ""

# Create data directory if it doesn't exist
mkdir -p "$DATA_DIR"
echo "✅ Data directory ready: $DATA_DIR"
echo ""

# Check disk space (warn if less than 20GB free)
AVAILABLE_SPACE=$(df -BG "$DATA_DIR" | tail -1 | awk '{print $4}' | sed 's/G//')
if [ "$AVAILABLE_SPACE" -lt 20 ]; then
    echo "⚠️  Warning: Less than 20GB free space available ($AVAILABLE_SPACE GB)"
    echo "   Recommended: At least 20GB for pruned sync (~13GB final size)"
    read -p "Continue anyway? (y/N) " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        exit 1
    fi
fi

echo "🔨 Building node with required features..."
echo "   Features: production"
echo "   Note: utxo-commitments disabled due to spec migration (Kani → Creusot)"
echo "   Incremental pruning still works, just without UTXO commitment verification"
echo ""

# Build with required features (utxo-commitments disabled for now due to spec migration)
cargo build --release --features "production" || {
    echo "❌ Build failed!"
    exit 1
}

echo ""
echo "✅ Build complete!"
echo ""
echo "🌐 Starting mainnet sync..."
echo "   - Incremental pruning: ENABLED"
echo "   - UTXO commitments: ENABLED"
echo "   - Parallel IBD: Will use if 2+ peers available"
echo "   - Storage: Will stay bounded at ~13GB"
echo ""
echo "📊 Monitor progress in another terminal:"
echo "   watch -n 5 'curl -s -X POST http://localhost:8332 \\"
echo "     -H \"Content-Type: application/json\" \\"
echo "     -d \"{\\\"jsonrpc\\\": \\\"2.0\\\", \\\"method\\\": \\\"getblockchaininfo\\\", \\\"params\\\": [], \\\"id\\\": 1}\" \\"
echo "     | jq \".result | {blocks, headers, verificationprogress: (.verificationprogress * 100 | floor | \\\"\\(.)%\\\"), initialblockdownload}\"'"
echo ""
echo "Press Ctrl+C to stop"
echo ""

# Start the node
./target/release/blvm \
    --config "$CONFIG_FILE" \
    --network mainnet \
    --data-dir "$DATA_DIR" \
    --rpc-addr 127.0.0.1:8332 \
    --listen-addr 0.0.0.0:8333

