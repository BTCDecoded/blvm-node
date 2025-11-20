#!/bin/bash
# Convert Bitcoin Core bitcoin.conf to bllvm-node config.toml
#
# Usage: ./convert-bitcoin-core-config.sh <bitcoin.conf> [output.toml]
#
# This script converts Bitcoin Core configuration to bllvm-node format.
# Data directories are NOT converted (as requested).

set -euo pipefail

INPUT_FILE="${1:-}"
OUTPUT_FILE="${2:-config.toml}"

if [ -z "$INPUT_FILE" ]; then
    echo "Usage: $0 <bitcoin.conf> [output.toml]"
    echo ""
    echo "Converts Bitcoin Core bitcoin.conf to bllvm-node config.toml"
    echo ""
    echo "Examples:"
    echo "  $0 ~/.bitcoin/bitcoin.conf"
    echo "  $0 /etc/bitcoin/bitcoin.conf /etc/bllvm-node/config.toml"
    exit 1
fi

if [ ! -f "$INPUT_FILE" ]; then
    echo "Error: Input file '$INPUT_FILE' not found"
    exit 1
fi

echo "# bllvm-node configuration converted from Bitcoin Core"
echo "# Source: $INPUT_FILE"
echo "# Generated: $(date -u +"%Y-%m-%d %H:%M:%S UTC")"
echo ""
echo "# NOTE: Data directories are NOT converted - configure separately"
echo ""

# Network settings
NETWORK=""
RPC_PORT=""
RPC_BIND=""
RPC_ALLOWIP=""
RPC_USER=""
RPC_PASSWORD=""
RPC_AUTH=""
MAX_CONNECTIONS=""
LISTEN=""
BIND=""
EXTERNALIP=""
ONLYNET=""
PROXY=""
SEEDNODE=""
ADDNODE=""
CONNECT=""
DISCOVER=""

# Mining settings
SERVER=""
RPC_SERVER=""
RPC_WORKQUEUE=""
RPC_THREADS=""

# Other settings
DAEMON=""
PRINTTOCONSOLE=""
LOG_TIMESTAMPS=""
LOGIPS=""
LOGTIMEMICROS=""
SHOWDEBUG=""
DEBUG=""
LOGLEVEL=""

# Parse bitcoin.conf
while IFS= read -r line || [ -n "$line" ]; do
    # Skip comments and empty lines
    line="${line%%#*}"  # Remove comments
    line="${line// /}"   # Remove spaces
    [ -z "$line" ] && continue
    
    # Parse key=value
    if [[ "$line" =~ ^([^=]+)=(.*)$ ]]; then
        key="${BASH_REMATCH[1]}"
        value="${BASH_REMATCH[2]}"
        
        case "$key" in
            # Network
            testnet|regtest)
                NETWORK="$key"
                ;;
            mainnet)
                NETWORK="mainnet"
                ;;
            
            # RPC
            rpcport)
                RPC_PORT="$value"
                ;;
            rpcbind)
                RPC_BIND="$value"
                ;;
            rpcallowip)
                RPC_ALLOWIP="$value"
                ;;
            rpcuser)
                RPC_USER="$value"
                ;;
            rpcpassword)
                RPC_PASSWORD="$value"
                ;;
            rpcauth)
                RPC_AUTH="$value"
                ;;
            
            # Network connections
            maxconnections)
                MAX_CONNECTIONS="$value"
                ;;
            listen)
                LISTEN="$value"
                ;;
            bind)
                BIND="$value"
                ;;
            externalip)
                EXTERNALIP="$value"
                ;;
            onlynet)
                ONLYNET="$value"
                ;;
            proxy)
                PROXY="$value"
                ;;
            seednode)
                SEEDNODE="$value"
                ;;
            addnode)
                ADDNODE="$value"
                ;;
            connect)
                CONNECT="$value"
                ;;
            discover)
                DISCOVER="$value"
                ;;
            
            # Server
            server)
                SERVER="$value"
                ;;
            rpcworkqueue)
                RPC_WORKQUEUE="$value"
                ;;
            rpcthreads)
                RPC_THREADS="$value"
                ;;
            
            # Logging
            daemon)
                DAEMON="$value"
                ;;
            printtoconsole)
                PRINTTOCONSOLE="$value"
                ;;
            logtimestamps)
                LOG_TIMESTAMPS="$value"
                ;;
            logips)
                LOGIPS="$value"
                ;;
            logtimemicros)
                LOGTIMEMICROS="$value"
                ;;
            showdebug)
                SHOWDEBUG="$value"
                ;;
            debug)
                DEBUG="$value"
                ;;
            loglevel)
                LOGLEVEL="$value"
                ;;
        esac
    fi
done < "$INPUT_FILE"

# Generate TOML config
cat > "$OUTPUT_FILE" <<EOF
# bllvm-node configuration
# Converted from Bitcoin Core: $INPUT_FILE

# Network configuration
[network]
EOF

# Network
if [ -n "$NETWORK" ]; then
    case "$NETWORK" in
        testnet)
            echo 'protocol_version = "testnet3"' >> "$OUTPUT_FILE"
            ;;
        regtest)
            echo 'protocol_version = "regtest"' >> "$OUTPUT_FILE"
            ;;
        mainnet)
            echo 'protocol_version = "bitcoin-v1"' >> "$OUTPUT_FILE"
            ;;
    esac
fi

# Listen address
if [ -n "$BIND" ] || [ -n "$LISTEN" ]; then
    ADDR="${BIND:-${LISTEN}}"
    if [ -n "$ADDR" ]; then
        PORT="8333"
        if [ "$NETWORK" = "testnet" ]; then
            PORT="18333"
        elif [ "$NETWORK" = "regtest" ]; then
            PORT="18444"
        fi
        echo "listen_addr = \"$ADDR:$PORT\"" >> "$OUTPUT_FILE"
    fi
fi

# Max peers
if [ -n "$MAX_CONNECTIONS" ]; then
    echo "max_peers = $MAX_CONNECTIONS" >> "$OUTPUT_FILE"
fi

# Persistent peers
if [ -n "$ADDNODE" ] || [ -n "$CONNECT" ]; then
    echo "" >> "$OUTPUT_FILE"
    echo "# Persistent peers (from addnode/connect)" >> "$OUTPUT_FILE"
    persistent_peers=()
    if [ -n "$ADDNODE" ]; then
        persistent_peers+=("$ADDNODE")
    fi
    if [ -n "$CONNECT" ]; then
        persistent_peers+=("$CONNECT")
    fi
    if [ ${#persistent_peers[@]} -gt 0 ]; then
        echo "persistent_peers = [" >> "$OUTPUT_FILE"
        for peer in "${persistent_peers[@]}"; do
            # Add port if not present
            if [[ ! "$peer" =~ :[0-9]+$ ]]; then
                if [ "$NETWORK" = "testnet" ]; then
                    peer="$peer:18333"
                elif [ "$NETWORK" = "regtest" ]; then
                    peer="$peer:18444"
                else
                    peer="$peer:8333"
                fi
            fi
            echo "  \"$peer\"," >> "$OUTPUT_FILE"
        done
        echo "]" >> "$OUTPUT_FILE"
    fi
fi

# RPC configuration
if [ -n "$RPC_PORT" ] || [ -n "$RPC_USER" ] || [ -n "$RPC_PASSWORD" ] || [ -n "$RPC_AUTH" ]; then
    echo "" >> "$OUTPUT_FILE"
    echo "[rpc_auth]" >> "$OUTPUT_FILE"
    
    if [ -n "$RPC_PORT" ]; then
        echo "port = $RPC_PORT" >> "$OUTPUT_FILE"
    fi
    
    if [ -n "$RPC_BIND" ]; then
        echo "bind = \"$RPC_BIND\"" >> "$OUTPUT_FILE"
    fi
    
    if [ -n "$RPC_USER" ] && [ -n "$RPC_PASSWORD" ]; then
        echo "# Basic auth (user/password)" >> "$OUTPUT_FILE"
        echo "username = \"$RPC_USER\"" >> "$OUTPUT_FILE"
        echo "password = \"$RPC_PASSWORD\"" >> "$OUTPUT_FILE"
    elif [ -n "$RPC_AUTH" ]; then
        echo "# RPC auth (rpcauth format)" >> "$OUTPUT_FILE"
        echo "# Note: rpcauth format needs manual conversion" >> "$OUTPUT_FILE"
        echo "# Original: rpcauth=$RPC_AUTH" >> "$OUTPUT_FILE"
    fi
    
    if [ -n "$RPC_ALLOWIP" ]; then
        echo "allowed_ips = [\"$RPC_ALLOWIP\"]" >> "$OUTPUT_FILE"
    fi
fi

# Transport preference
echo "" >> "$OUTPUT_FILE"
echo "[transport_preference]" >> "$OUTPUT_FILE"
echo "prefer_tcp = true" >> "$OUTPUT_FILE"
echo "prefer_quinn = false" >> "$OUTPUT_FILE"
echo "prefer_iroh = false" >> "$OUTPUT_FILE"

# Network timing
echo "" >> "$OUTPUT_FILE"
echo "[network_timing]" >> "$OUTPUT_FILE"
if [ -n "$MAX_CONNECTIONS" ]; then
    echo "target_peer_count = $MAX_CONNECTIONS" >> "$OUTPUT_FILE"
else
    echo "target_peer_count = 8" >> "$OUTPUT_FILE"
fi

echo "" >> "$OUTPUT_FILE"
echo "# Additional notes:" >> "$OUTPUT_FILE"
echo "# - Data directories are NOT converted (configure separately)" >> "$OUTPUT_FILE"
echo "# - Some Bitcoin Core options may not have direct equivalents" >> "$OUTPUT_FILE"
echo "# - Review and adjust settings as needed" >> "$OUTPUT_FILE"

echo ""
echo "✓ Configuration converted successfully!"
echo "  Input:  $INPUT_FILE"
echo "  Output: $OUTPUT_FILE"
echo ""
echo "⚠️  IMPORTANT:"
echo "  - Data directories are NOT converted"
echo "  - Review the generated config and adjust as needed"
echo "  - Some options may need manual configuration"

