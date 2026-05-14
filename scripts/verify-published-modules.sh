#!/usr/bin/env bash
# Quick HTTP checks: modules.json, each module.toml, and current-platform download URL.
set -euo pipefail

REGISTRY_URL="${REGISTRY_URL:-https://raw.githubusercontent.com/BTCDecoded/blvm/main/registry/modules.json}"

case "$(uname -s).$(uname -m)" in
  Linux.x86_64) PLATFORM=x86_64-linux ;;
  Linux.aarch64|Linux.arm64) PLATFORM=aarch64-linux ;;
  Darwin.x86_64) PLATFORM=x86_64-apple ;;
  Darwin.arm64|Darwin.aarch64) PLATFORM=aarch64-apple ;;
  MINGW*|CYGWIN*|MSYS*) PLATFORM=x86_64-windows ;;
  *) echo "Unknown platform; fix uname mapping in script"; exit 1 ;;
esac

python3 -c 'import tomllib, sys; assert sys.version_info >= (3, 11), "Python 3.11+ required (tomllib)"'

echo "== Registry: $REGISTRY_URL"
json="$(curl -fsSL "$REGISTRY_URL")"
echo "$json" | head -c 500 && echo "..."

for name in blvm-miniscript blvm-zmq; do
  mturl=$(python3 -c "import json,sys; d=json.loads(sys.stdin.read()); print(next(x['module_toml_url'] for x in d if x['name']==sys.argv[1]))" "$name" <<<"$json")
  echo ""
  echo "== $name module.toml (first lines)"
  curl -fsSL "$mturl" | head -n 25
  readarray -t dl < <(curl -fsSL "$mturl" | python3 -c "import tomllib,sys; m=tomllib.loads(sys.stdin.read()); d=m['downloads']['$PLATFORM']; print(d['url']); print(d['sha256'])")
  binurl="${dl[0]}"
  sha="${dl[1]}"
  echo "== HEAD $name binary ($PLATFORM)"
  code=$(curl -sS -L -o /dev/null -w "%{http_code}" -I "$binurl")
  echo "HTTP $code $binurl"
  test "$code" = "200" || exit 1
  echo "expected sha256=$sha"
done

echo ""
echo "OK: registry + module.toml + binary URLs reachable for $PLATFORM"
