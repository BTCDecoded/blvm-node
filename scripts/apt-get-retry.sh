#!/usr/bin/env bash
# Retry apt-get when another concurrent job holds /var/lib/apt/lists/lock or dpkg locks
# (common on busy self-hosted runners). DPkg::Lock::Timeout only covers part of contention.
set -uo pipefail

MAX_ATTEMPTS="${CI_APT_RETRY_MAX:-30}"
DELAY_SEC="${CI_APT_RETRY_DELAY:-10}"
APT_OPTS=(-o "DPkg::Lock::Timeout=120")

attempt=1
while true; do
  if sudo apt-get "${APT_OPTS[@]}" "$@"; then
    exit 0
  fi
  if [[ "${attempt}" -ge "${MAX_ATTEMPTS}" ]]; then
    echo "apt-get failed after ${MAX_ATTEMPTS} attempts: $*" >&2
    exit 100
  fi
  echo "apt-get contention or failure (attempt ${attempt}/${MAX_ATTEMPTS}), sleeping ${DELAY_SEC}s..." >&2
  sleep "${DELAY_SEC}"
  attempt=$((attempt + 1))
done
