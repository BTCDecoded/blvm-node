#!/bin/bash
set -e
FUZZ_DIR="$(cd "$(dirname "$0")" && pwd)"
CORPUS_DIR="${1:-$FUZZ_DIR/corpus}"
mkdir -p "$CORPUS_DIR"
for t in \
  compact_block_reconstruction transport_aware_negotiation protocol_message_parsing \
  mining_transaction_serialization mining_block_template \
  node_mempool_policy node_rpc_surface node_miniscript node_p2p_allowed_frames node_payment; do
  mkdir -p "$CORPUS_DIR/$t"
done
echo "Corpus dirs under $CORPUS_DIR"
