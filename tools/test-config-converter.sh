#!/bin/bash
# Test script for Bitcoin Core config converter
#
# Creates a test bitcoin.conf and validates the conversion

set -euo pipefail

TEST_DIR=$(mktemp -d)
trap "rm -rf $TEST_DIR" EXIT

echo "Testing Bitcoin Core config converter..."
echo

# Create test bitcoin.conf
cat > "$TEST_DIR/bitcoin.conf" <<'EOF'
# Test Bitcoin Core configuration
testnet=1
rpcport=18332
rpcbind=127.0.0.1
rpcuser=testuser
rpcpassword=testpass
maxconnections=16
addnode=1.2.3.4
addnode=5.6.7.8
connect=9.10.11.12
server=1
rpcworkqueue=32
rpcthreads=4
EOF

echo "Created test bitcoin.conf:"
cat "$TEST_DIR/bitcoin.conf"
echo

# Convert using shell script
echo "Converting with shell script..."
./tools/convert-bitcoin-core-config.sh "$TEST_DIR/bitcoin.conf" "$TEST_DIR/config-shell.toml"

echo
echo "Generated config.toml (shell):"
cat "$TEST_DIR/config-shell.toml"
echo

# Validate TOML structure
echo "Validating TOML structure..."
if grep -q "\[network\]" "$TEST_DIR/config-shell.toml" && \
   grep -q "protocol_version" "$TEST_DIR/config-shell.toml" && \
   grep -q "\[rpc_auth\]" "$TEST_DIR/config-shell.toml"; then
    echo "✓ TOML structure is valid"
else
    echo "✗ TOML structure is invalid"
    exit 1
fi

# Check for expected values
echo "Checking expected values..."
if grep -q "testnet3" "$TEST_DIR/config-shell.toml"; then
    echo "✓ Network correctly set to testnet3"
else
    echo "✗ Network not set correctly"
    exit 1
fi

if grep -q "18332" "$TEST_DIR/config-shell.toml"; then
    echo "✓ RPC port correctly set to 18332"
else
    echo "✗ RPC port not set correctly"
    exit 1
fi

if grep -q "persistent_peers" "$TEST_DIR/config-shell.toml"; then
    echo "✓ Persistent peers section found"
else
    echo "✗ Persistent peers section missing"
    exit 1
fi

echo
echo "✓ All tests passed!"

