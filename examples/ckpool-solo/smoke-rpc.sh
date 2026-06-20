#!/usr/bin/env bash
# ckpool-compatible RPC smoke test (node must be running).
set -euo pipefail

RPC_URL="${RPC_URL:-http://127.0.0.1:18443}"
RPC_USER="${RPC_USER:-ckpool}"
RPC_PASS="${RPC_PASS:-change-me}"

rpc() {
  local method="$1"
  local params="${2:-[]}"
  curl -sf -u "${RPC_USER}:${RPC_PASS}" -X POST "$RPC_URL" \
    -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"${method}\",\"params\":${params},\"id\":1}"
}

echo "== getblockchaininfo =="
rpc getblockchaininfo | jq -e '.result.chain' >/dev/null

echo "== getbestblockhash =="
tip="$(rpc getbestblockhash | jq -r '.result')"
test "${#tip}" -eq 64

echo "== getblockcount =="
height="$(rpc getblockcount | jq -r '.result')"

echo "== getblockhash =="
hash_at_height="$(rpc getblockhash "[${height}]" | jq -r '.result')"
test "$tip" = "$hash_at_height"

echo "== getblocktemplate (ckpool params) =="
gbt="$(rpc getblocktemplate '[{"capabilities":["coinbasetxn","workid","coinbase/append"],"rules":["segwit"]}]')"
prev="$(echo "$gbt" | jq -r '.result.previousblockhash')"
test "$prev" = "$tip"
test "$(echo "$gbt" | jq -r '.result.target | length')" -eq 64
test "$(echo "$gbt" | jq -r '.result.coinbasevalue')" -gt 0

echo "== validateaddress =="
rpc validateaddress '["bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"]' \
  | jq -e '.result.isvalid == true' >/dev/null

echo "OK: ckpool RPC contract satisfied against ${RPC_URL}"
