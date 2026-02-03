#!/bin/bash
# IBD Testing Script
# Tests Initial Blockchain Download functionality

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
NETWORK="${NETWORK:-testnet}"
DATA_DIR="${DATA_DIR:-~/.local/share/blvm}"
RPC_PORT="${RPC_PORT:-8332}"
RPC_HOST="${RPC_HOST:-127.0.0.1}"
CLEAR_DATA="${CLEAR_DATA:-false}"

echo -e "${BLUE}=== IBD Testing Script ===${NC}"
echo ""

# Function to print section headers
section() {
    echo -e "\n${BLUE}--- $1 ---${NC}"
}

# Function to check if node is running
check_node_running() {
    curl -s -X POST http://${RPC_HOST}:${RPC_PORT} \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc": "2.0", "method": "getblockchaininfo", "params": [], "id": 1}' \
        > /dev/null 2>&1
}

# Function to get blockchain info
get_blockchain_info() {
    curl -s -X POST http://${RPC_HOST}:${RPC_PORT} \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc": "2.0", "method": "getblockchaininfo", "params": [], "id": 1}' \
        | jq -r '.result'
}

# Function to get peer info
get_peer_info() {
    curl -s -X POST http://${RPC_HOST}:${RPC_PORT} \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc": "2.0", "method": "getpeerinfo", "params": [], "id": 2}' \
        | jq -r '.result'
}

# Step 1: Check prerequisites
section "Checking Prerequisites"

if ! command -v jq &> /dev/null; then
    echo -e "${YELLOW}Warning: jq not found. Install it for better output formatting.${NC}"
    echo "  sudo apt-get install jq  # Ubuntu/Debian"
    echo "  brew install jq           # macOS"
fi

if ! command -v cargo &> /dev/null; then
    echo -e "${RED}Error: cargo not found. Please install Rust.${NC}"
    exit 1
fi

echo -e "${GREEN}✓ Prerequisites check passed${NC}"

# Step 2: Clear data if requested
if [ "$CLEAR_DATA" = "true" ]; then
    section "Clearing Blockchain Data"
    
    DATA_PATH=$(eval echo "$DATA_DIR")
    
    if [ -d "$DATA_PATH" ]; then
        echo "Clearing data directory: $DATA_PATH"
        read -p "This will delete all blockchain data. Continue? (y/N) " -n 1 -r
        echo
        if [[ $REPLY =~ ^[Yy]$ ]]; then
            rm -rf "$DATA_PATH/chainstate"
            rm -rf "$DATA_PATH/blocks"
            echo -e "${GREEN}✓ Data cleared${NC}"
        else
            echo "Skipping data clear"
        fi
    else
        echo -e "${YELLOW}Data directory not found: $DATA_PATH${NC}"
    fi
fi

# Step 3: Run unit tests
section "Running Unit Tests"

echo "Running parallel_ibd unit tests..."
if cargo test parallel_ibd --lib 2>&1 | tee /tmp/ibd_test_output.log; then
    echo -e "${GREEN}✓ Unit tests passed${NC}"
else
    echo -e "${RED}✗ Unit tests failed${NC}"
    exit 1
fi

# Step 4: Run integration tests
section "Running Integration Tests"

echo "Running parallel_ibd integration tests..."
if cargo test --test integration parallel_ibd_tests 2>&1 | tee -a /tmp/ibd_test_output.log; then
    echo -e "${GREEN}✓ Integration tests passed${NC}"
else
    echo -e "${RED}✗ Integration tests failed${NC}"
    exit 1
fi

# Step 5: Check if node is running
section "Checking Node Status"

if check_node_running; then
    echo -e "${GREEN}✓ Node is running${NC}"
    
    # Get current status
    BLOCKCHAIN_INFO=$(get_blockchain_info)
    BLOCKS=$(echo "$BLOCKCHAIN_INFO" | jq -r '.blocks // 0')
    HEADERS=$(echo "$BLOCKCHAIN_INFO" | jq -r '.headers // 0')
    IBD=$(echo "$BLOCKCHAIN_INFO" | jq -r '.initialblockdownload // false')
    
    echo "  Blocks: $BLOCKS"
    echo "  Headers: $HEADERS"
    echo "  IBD: $IBD"
    
    # Get peer count
    PEER_INFO=$(get_peer_info)
    PEER_COUNT=$(echo "$PEER_INFO" | jq 'length')
    echo "  Connected peers: $PEER_COUNT"
    
    if [ "$IBD" = "true" ]; then
        echo -e "${YELLOW}⚠ IBD is in progress${NC}"
        echo ""
        echo "Monitor progress with:"
        echo "  watch -n 1 'curl -s -X POST http://${RPC_HOST}:${RPC_PORT} \\"
        echo "    -H \"Content-Type: application/json\" \\"
        echo "    -d \"{\\\"jsonrpc\\\": \\\"2.0\\\", \\\"method\\\": \\\"getblockchaininfo\\\", \\\"params\\\": [], \\\"id\\\": 1}\" \\"
        echo "    | jq \\\".result | {blocks, headers, verificationprogress, initialblockdownload}\"'"
    else
        if [ "$BLOCKS" -eq 0 ]; then
            echo -e "${YELLOW}⚠ Node has no blocks. Start node to trigger IBD.${NC}"
        else
            echo -e "${GREEN}✓ Node is synced${NC}"
        fi
    fi
else
    echo -e "${YELLOW}⚠ Node is not running${NC}"
    echo ""
    echo "To start the node and test IBD:"
    echo "  cd blvm-node"
    echo "  cargo run -- --network $NETWORK"
    echo ""
    echo "Or with production features:"
    echo "  cargo run --features production -- --network $NETWORK"
fi

# Step 6: Summary
section "Test Summary"

echo "Unit tests: $(grep -c 'test result: ok' /tmp/ibd_test_output.log 2>/dev/null || echo 'N/A')"
echo "Integration tests: $(grep -c 'test result: ok' /tmp/ibd_test_output.log 2>/dev/null || echo 'N/A')"
echo ""
echo -e "${GREEN}Testing complete!${NC}"
echo ""
echo "Next steps:"
echo "  1. Review test output: /tmp/ibd_test_output.log"
echo "  2. Start node manually to test IBD: cargo run -- --network $NETWORK"
echo "  3. Monitor IBD progress via RPC"
echo ""
echo "For more information, see: docs/IBD_TESTING_GUIDE.md"




