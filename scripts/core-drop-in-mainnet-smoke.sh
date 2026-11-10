#!/usr/bin/env bash
# Manual mainnet Core drop-in smoke test (operator-run; not CI).
#
# Validates a synced Bitcoin Core mainnet datadir: layout, lock check, migrate+verify,
# and tip hash vs Core CLI when available.
#
# Usage:
#   BLVM_BIN=../blvm/target/release/blvm \
#   BITCOIN_CLI=bitcoin-cli \
#   ./scripts/core-drop-in-mainnet-smoke.sh /path/to/.bitcoin
#
# Or: BLVM_CORE_MAINNET_DATADIR=/path/to/.bitcoin ./scripts/core-drop-in-mainnet-smoke.sh
#
# Requirements:
#   - bitcoind STOPPED (no live lock on chainstate)
#   - Non-pruned datadir (or use storage.reuse_core_block_files in config)
#   - blvm built with rocksdb (default release)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_DIR="${1:-${BLVM_CORE_MAINNET_DATADIR:-}}"
BLVM="${BLVM_BIN:-blvm}"
BITCOIN_CLI="${BITCOIN_CLI:-bitcoin-cli}"
DEST="${BLVM_CORE_SMOKE_DEST:-}"

if [[ -z "$CORE_DIR" ]]; then
  echo "Usage: $0 <core-datadir>" >&2
  echo "  or set BLVM_CORE_MAINNET_DATADIR" >&2
  exit 1
fi

if [[ ! -d "$CORE_DIR/chainstate" || ! -d "$CORE_DIR/blocks" ]]; then
  echo "ERROR: not a Core layout (need chainstate/ and blocks/): $CORE_DIR" >&2
  exit 1
fi

if [[ -n "$DEST" ]]; then
  BLVM_DEST="$DEST"
else
  BLVM_DEST="$(mktemp -d)/blvm-mainnet-smoke"
fi

echo "=== Core drop-in mainnet smoke ==="
echo "  Core datadir: $CORE_DIR"
echo "  BLVM dest:    $BLVM_DEST"
echo "  blvm:         $BLVM"
echo

CORE_TIP="${BLVM_EXPECTED_TIP:-}"
if command -v "$BITCOIN_CLI" >/dev/null 2>&1; then
  if "$BITCOIN_CLI" -datadir="$CORE_DIR" getblockchaininfo >/dev/null 2>&1; then
    echo "ERROR: bitcoind appears to be running — stop it before migrating." >&2
    exit 1
  fi
  echo "Core node is not running (good)."
fi

echo "Running: $BLVM migrate core --source ... --destination ... --network mainnet --verify"
"$BLVM" migrate core \
  --source "$CORE_DIR" \
  --destination "$BLVM_DEST" \
  --network mainnet \
  --verify

MARKER="$BLVM_DEST/blvm_meta/migration.json"
if [[ ! -f "$MARKER" ]]; then
  echo "ERROR: migration marker missing at $MARKER" >&2
  exit 1
fi

echo
echo "Migration marker:"
cat "$MARKER"
echo

if [[ -n "$CORE_TIP" ]]; then
  BLVM_TIP="$(grep -o '"tip_hash": "[^"]*"' "$MARKER" | cut -d'"' -f4 || true)"
  if [[ -n "$BLVM_TIP" && "$BLVM_TIP" != "$CORE_TIP" ]]; then
    echo "ERROR: marker tip_hash ($BLVM_TIP) != BLVM_EXPECTED_TIP ($CORE_TIP)" >&2
    exit 1
  fi
  echo "Tip hash matches BLVM_EXPECTED_TIP."
fi

echo
echo "PASS: mainnet migrate+verify succeeded."
echo "Next: blvm start --datadir $CORE_DIR --network mainnet --no-auto-migrate"
echo "      (or point storage.data_dir at $BLVM_DEST if using a standalone BLVM store)"
if [[ -z "${BLVM_CORE_SMOKE_DEST:-}" ]]; then
  echo "Temp BLVM store left at: $BLVM_DEST (delete when done)"
fi
