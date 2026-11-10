#!/usr/bin/env bash
# Quick HTTP checks: modules.json, each module.toml, release sha256sums + binary for current platform.
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
  readarray -t pair < <(python3 -c "
import json, sys
name = sys.argv[1]
d = json.loads(sys.stdin.read())
row = next(x for x in d if x['name'] == name)
ref = (row.get('manifest_ref') or 'main').strip()
mturl = (row.get('module_toml_url') or '').strip()
if not mturl:
    mturl = f"https://raw.githubusercontent.com/{row['repo']}/{ref}/module.toml"
print(row['repo'])
print(mturl)
" "$name" <<<"$json")
  repo="${pair[0]}"
  mturl="${pair[1]}"
  echo ""
  echo "== $name module.toml (first lines)"
  curl -fsSL "$mturl" | head -n 22
  readarray -t verart < <(curl -fsSL "$mturl" | python3 -c "
import tomllib, sys
m = tomllib.loads(sys.stdin.read())
name = m['name']
ver = m['version'].strip()
plat = sys.argv[1]
if plat == 'x86_64-windows':
    art = f'{name}-x86_64-windows.exe'
else:
    art = f'{name}-{plat}'
print(ver)
print(art)
" "$PLATFORM")
  version="${verart[0]}"
  artifact="${verart[1]}"
  tag="v${version}"
  sums_url="https://github.com/${repo}/releases/download/${tag}/sha256sums.txt"
  echo "== $sums_url"
  sums="$(curl -fsSL "$sums_url")"
  expected="$(echo "$sums" | python3 -c "
import sys
art = sys.argv[1]
want = None
for line in sys.stdin:
    line = line.strip()
    if not line or line.startswith('#'):
        continue
    parts = line.split()
    if len(parts) < 2:
        continue
    h, rest = parts[0], ' '.join(parts[1:]).lstrip('*')
    if len(h) != 64:
        continue
    base = rest.split('/')[-1]
    if base == art or rest == art or rest.endswith(art):
        want = h.lower()
        break
if not want:
    sys.exit('no hash for ' + art)
print(want)
" "$artifact")"
  binurl="https://github.com/${repo}/releases/download/${tag}/${artifact}"
  echo "== HEAD $name binary ($PLATFORM)"
  code=$(curl -sS -L -o /dev/null -w "%{http_code}" -I "$binurl")
  echo "HTTP $code $binurl"
  test "$code" = "200" || exit 1
  echo "expected sha256=$expected"
done

echo ""
echo "OK: registry + module.toml + GitHub release checksums + binary URLs for $PLATFORM"
