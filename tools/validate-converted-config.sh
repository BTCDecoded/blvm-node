#!/bin/bash
# Validate converted config.toml file
#
# Usage: ./validate-converted-config.sh <config.toml>

set -euo pipefail

CONFIG_FILE="${1:-}"

if [ -z "$CONFIG_FILE" ]; then
    echo "Usage: $0 <config.toml>"
    echo ""
    echo "Validates a converted bllvm-node config.toml file"
    exit 1
fi

if [ ! -f "$CONFIG_FILE" ]; then
    echo "Error: Config file '$CONFIG_FILE' not found"
    exit 1
fi

echo "Validating config: $CONFIG_FILE"
echo

ERRORS=0

# Check for required sections
check_section() {
    local section=$1
    if grep -q "^\[$section\]" "$CONFIG_FILE"; then
        echo "✓ Section [$section] found"
    else
        echo "✗ Section [$section] missing"
        ERRORS=$((ERRORS + 1))
    fi
}

# Check for required keys
check_key() {
    local key=$1
    if grep -q "^$key" "$CONFIG_FILE"; then
        echo "✓ Key $key found"
    else
        echo "⚠ Key $key not found (may be optional)"
    fi
}

echo "Checking required sections..."
check_section "network"
check_section "transport_preference"
check_section "network_timing"

echo
echo "Checking optional sections..."
if grep -q "^\[rpc_auth\]" "$CONFIG_FILE"; then
    echo "✓ RPC auth configured"
    check_key "port"
    check_key "bind"
else
    echo "⚠ RPC auth not configured (may be intentional)"
fi

if grep -q "^persistent_peers" "$CONFIG_FILE"; then
    echo "✓ Persistent peers configured"
else
    echo "⚠ Persistent peers not configured"
fi

echo
echo "Checking TOML syntax..."
if command -v toml-validator &> /dev/null; then
    if toml-validator "$CONFIG_FILE" 2>/dev/null; then
        echo "✓ TOML syntax is valid"
    else
        echo "✗ TOML syntax errors found"
        ERRORS=$((ERRORS + 1))
    fi
else
    echo "⚠ toml-validator not installed, skipping syntax check"
    echo "  Install with: cargo install toml-validator"
fi

echo
if [ $ERRORS -eq 0 ]; then
    echo "✓ Config validation passed!"
    exit 0
else
    echo "✗ Config validation failed with $ERRORS error(s)"
    exit 1
fi

