#!/usr/bin/env bash
# Local smoke: first-boot bootstrap of official blvm-miniscript + blvm-zmq from the
# public registry (same paths production uses).
#
# Prerequisite: run scripts/verify-published-modules.sh (URLs + hashes on disk OK).
#
# Usage:
#   BLVM_BIN=/path/to/blvm ./scripts/smoke-bootstrap-official-modules.sh
#
# The node must be built with the `governance` feature (default for the `blvm` crate)
# so HTTP bootstrap is compiled in. Config may omit [modules]; defaults pull
# blvm-miniscript + blvm-zmq and DEFAULT_MODULE_REGISTRY_INDEX_URL.
#
# Release model: each official module’s GitHub Release publishes binaries +
# sha256sums.txt; module.toml on main carries semver only (see blvm-node modules README).
#
set -euo pipefail

: "${BLVM_BIN:?Set BLVM_BIN to a blvm binary (e.g. target/release/blvm)}"
test -x "$BLVM_BIN" || {
  echo "Not executable: $BLVM_BIN"
  exit 1
}

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/blvm-bootstrap-smoke.XXXXXX")"
cleanup() { rm -rf "$WORKDIR"; }
trap cleanup EXIT

mkdir -p "$WORKDIR/data" "$WORKDIR/modules"

cat >"$WORKDIR/blvm.toml" <<EOF
listen_addr = "127.0.0.1:0"
protocol_version = "Regtest"
transport_preference = "tcponly"

[storage]
data_dir = "$WORKDIR/data"
EOF

RPC_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"

set +e
# Default modules_dir is relative ("modules"); run from WORKDIR so bootstrap writes
# into $WORKDIR/modules, not the caller's cwd.
(
  cd "$WORKDIR"
  RUST_LOG=info timeout 45 "$BLVM_BIN" -n regtest --config "$WORKDIR/blvm.toml" \
    --rpc-addr "127.0.0.1:${RPC_PORT}" >"$WORKDIR/log.txt" 2>&1
)
ec=$?
set -e

grep -q "Bootstrap: installed 'blvm-miniscript'" "$WORKDIR/log.txt" || {
  echo "FAIL: miniscript bootstrap log missing"
  tail -80 "$WORKDIR/log.txt"
  exit 1
}
grep -q "Bootstrap: installed 'blvm-zmq'" "$WORKDIR/log.txt" || {
  echo "FAIL: zmq bootstrap log missing"
  tail -80 "$WORKDIR/log.txt"
  exit 1
}
test -f "$WORKDIR/modules/blvm-miniscript/blvm-miniscript" || {
  echo "FAIL: miniscript binary not on disk under modules/"
  exit 1
}
test -f "$WORKDIR/modules/blvm-zmq/blvm-zmq" || {
  echo "FAIL: zmq binary not on disk under modules/"
  exit 1
}

echo "OK: official bootstrap installed both modules under $WORKDIR/modules (node exit=$ec)."
echo "    Handshake/load lines (module IPC / exit codes are orthogonal to download path):"
grep -E "Bootstrap:|loaded successfully|crashed|Failed to auto-load" "$WORKDIR/log.txt" || true
