#!/usr/bin/env bash
# §1a layering plan: fail if blvm-node/src uses blvm_consensus:: unless allowlisted.
# Allowlist: direct-consensus-allowlist.txt — non-comment lines; first field is a path glob
# relative to blvm-node (e.g. src/foo.rs or src/experimental/**).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
SRC="src"
LIST="direct-consensus-allowlist.txt"

if [[ ! -d "$SRC" ]]; then
  echo "check-direct-consensus-imports: missing $SRC" >&2
  exit 2
fi

mapfile -t FILES < <(rg -l 'blvm_consensus::' "$SRC" --glob '*.rs' || true)
if [[ ${#FILES[@]} -eq 0 ]]; then
  exit 0
fi

allowed_for() {
  local rel="$1"
  local line path_pat
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    path_pat="${line%%[[:space:]]*}"
    path_pat="${path_pat//$'\t'/}"
    [[ -z "$path_pat" ]] && continue
    if [[ "$rel" == $path_pat ]]; then
      return 0
    fi
  done <"$LIST"
  return 1
}

BAD=0
for f in "${FILES[@]}"; do
  if allowed_for "$f"; then
    continue
  fi
  echo "check-direct-consensus-imports: unallowlisted direct consensus import in $f" >&2
  rg -n 'blvm_consensus::' "$f" >&2 || true
  BAD=1
done

if [[ "$BAD" -ne 0 ]]; then
  echo "check-direct-consensus-imports: fix imports (prefer blvm_protocol::) or add a glob line to $LIST" >&2
  exit 1
fi
