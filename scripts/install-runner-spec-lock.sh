#!/usr/bin/env bash
# Install blvm-spec-lock for GitHub Actions self-hosted runners on this host (Zeus).
# Run as root or via sudo; targets the github-runner service user.
set -euo pipefail

SPEC_LOCK_SRC="${SPEC_LOCK_SRC:-/mnt/data/bitcoin/blvm/blvm-spec-lock}"
RUNNER_USER="${RUNNER_USER:-github-runner}"

if [[ ! -f "${SPEC_LOCK_SRC}/Cargo.toml" ]]; then
  echo "❌ blvm-spec-lock not found at ${SPEC_LOCK_SRC}" >&2
  exit 1
fi

for cmd in cargo pkg-config; do
  command -v "${cmd}" >/dev/null 2>&1 || {
    echo "❌ ${cmd} missing (install Rust + z3 dev headers for ${RUNNER_USER})" >&2
    exit 1
  }
done
pkg-config --exists z3 || {
  echo "❌ z3 not found (pacman -S z3 or equivalent)" >&2
  exit 1
}

echo "Installing blvm-spec-lock from ${SPEC_LOCK_SRC} for user ${RUNNER_USER}..."
TARGET_DIR="${TARGET_DIR:-/var/lib/${RUNNER_USER}/.cache/blvm-spec-lock-build}"
sudo -u "${RUNNER_USER}" bash -lc "
  set -euo pipefail
  mkdir -p '${TARGET_DIR}'
  export CARGO_TARGET_DIR='${TARGET_DIR}'
  cargo install --path '${SPEC_LOCK_SRC}' --locked --features z3 --force
  export PATH=\"\${HOME}/.cargo/bin:\${PATH}\"
  echo \"✅ \$(cargo-spec-lock --version) at \$(command -v cargo-spec-lock)\"
"
