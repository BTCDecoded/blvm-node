#!/usr/bin/env bash
# Generate a real Bitcoin Core regtest datadir fixture (101 blocks) for BLVM Core drop-in tests.
#
# Usage:
#   ./scripts/gen-core-regtest-fixture.sh [OUTPUT_DIR]
#
# Default output: tests/fixtures/core-regtest-101/
# Requires: bitcoind and bitcoin-cli on PATH (or BITCOIND / BITCOIN_CLI env).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$ROOT/tests/fixtures/core-regtest-101}"
BITCOIND="${BITCOIND:-bitcoind}"
BITCOIN_CLI="${BITCOIN_CLI:-bitcoin-cli}"
WORKDIR="$(mktemp -d)"
DATADIR="$WORKDIR/regtest-data"
META="$OUT/fixture.json"

cleanup() {
  "$BITCOIN_CLI" -regtest -datadir="$DATADIR" stop >/dev/null 2>&1 || true
  sleep 1
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

command -v "$BITCOIND" >/dev/null || { echo "bitcoind not found"; exit 1; }
command -v "$BITCOIN_CLI" >/dev/null || { echo "bitcoin-cli not found"; exit 1; }

mkdir -p "$DATADIR" "$OUT"

echo "Starting regtest bitcoind in $DATADIR ..."
"$BITCOIND" -regtest -datadir="$DATADIR" -fallbackfee=0.0002 -daemon
"$BITCOIN_CLI" -regtest -datadir="$DATADIR" -rpcwait getblockchaininfo >/dev/null

echo "Creating wallet and mining 101 blocks ..."
"$BITCOIN_CLI" -regtest -datadir="$DATADIR" -rpcwait createwallet fixture 2>/dev/null \
  || "$BITCOIN_CLI" -regtest -datadir="$DATADIR" loadwallet fixture 2>/dev/null \
  || true
ADDR="$("$BITCOIN_CLI" -regtest -datadir="$DATADIR" getnewaddress "" "legacy" 2>/dev/null \
  || "$BITCOIN_CLI" -regtest -datadir="$DATADIR" getnewaddress)"
echo "Mining to $ADDR ..."
"$BITCOIN_CLI" -regtest -datadir="$DATADIR" generatetoaddress 101 "$ADDR" >/dev/null

HEIGHT="$("$BITCOIN_CLI" -regtest -datadir="$DATADIR" getblockcount)"
TIP="$("$BITCOIN_CLI" -regtest -datadir="$DATADIR" getbestblockhash)"
GENESIS="$("$BITCOIN_CLI" -regtest -datadir="$DATADIR" getblockhash 0)"
TIP_COINBASE_TX="$("$BITCOIN_CLI" -regtest -datadir="$DATADIR" getblock "$TIP" true | sed -n 's/.*"tx": \[\s*"\([^"]*\)".*/\1/p' | head -1)"
if [[ -z "$TIP_COINBASE_TX" ]]; then
  TIP_COINBASE_TX="$("$BITCOIN_CLI" -regtest -datadir="$DATADIR" getblock "$TIP" 1 | grep -oP '"txid": "\K[^"]+' | head -1)"
fi
UTXOS="$("$BITCOIN_CLI" -regtest -datadir="$DATADIR" gettxoutsetinfo | grep -o '"txouts": [0-9]*' | grep -o '[0-9]*' || echo 0)"

echo "Stopping bitcoind ..."
"$BITCOIN_CLI" -regtest -datadir="$DATADIR" stop
sleep 2

# Core may use datadir directly or datadir/regtest depending on version.
SRC="$DATADIR"
if [[ -d "$DATADIR/regtest/chainstate" ]]; then
  SRC="$DATADIR/regtest"
fi

if [[ ! -d "$SRC/chainstate" || ! -d "$SRC/blocks" ]]; then
  echo "Expected chainstate/ and blocks/ under $SRC" >&2
  exit 1
fi

rm -rf "$OUT/chainstate" "$OUT/blocks"
cp -a "$SRC/chainstate" "$SRC/blocks" "$OUT/"

cat >"$META" <<EOF
{
  "network": "regtest",
  "blocks_mined": 101,
  "height": $HEIGHT,
  "tip_hash": "$TIP",
  "genesis_hash": "$GENESIS",
  "tip_coinbase_txid": "$TIP_COINBASE_TX",
  "utxo_count": $UTXOS,
  "generated_by": "gen-core-regtest-fixture.sh"
}
EOF

echo "Fixture written to $OUT"
echo "  height=$HEIGHT tip=$TIP utxos=$UTXOS"
echo "Run: cargo test -p blvm-node --features rocksdb core_drop_in"
