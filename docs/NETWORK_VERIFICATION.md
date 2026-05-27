# Network Protocol Formal Verification

## Overview

This document describes formal verification of Bitcoin P2P protocol message parsing, serialization, and processing. Verification uses **blvm-spec-lock** (Z3-based) for consensus code and Dandelion (Section 10.6).

## Verification Status

### blvm-spec-lock (Orange Paper)

- **Dandelion (10.6)** – ✅ Verified in `src/network/dandelion.rs` via `#[spec_locked("10.6")]`
- **Protocol parsing (10.1)** – ✅ Covered: parse_message, calculate_checksum (protocol-verification feature, enabled by default)

### Planned spec-lock coverage

1. **Message Header Parsing** – Magic, command, payload length, checksum extraction
2. **Checksum Validation** – Invalid checksums rejected
3. **Size Limit Enforcement** – Oversized messages rejected
4. **Round-Trip Properties** – `parse(serialize(msg)) == msg` for version, verack, ping, pong, tx, block, headers, inv, getdata, getheaders

## Running Verification

### blvm-spec-lock (Dandelion-focused run)

With **blvm-spec** as a sibling directory (`../blvm-spec`, same layout as CI’s **`setup-blvm-spec`**), you can narrow verification to §**10.6** (optional; omit **`--section`** to verify all **`#[spec_locked]`** sites including the merged **`F_*`** registry when **`--spec-path`** is set):

```bash
cd blvm-node
export SPEC_LOCK_STRICT=1
cargo spec-lock verify --crate-path . \
  --spec-path ../blvm-spec/PROTOCOL.md ../blvm-spec/ARCHITECTURE.md \
  --section 10.6 \
  --timeout 120
```

### CI

Formal verification runs via **blvm-spec-lock** in **`.github/workflows/ci.yml`** (**verify** job): installs **`cargo-spec-lock`** from crates.io, runs **`verify`** on the published **blvm-consensus** sources and again on **blvm-node** (merged **`F_*`** gate + Rust rows; **`formula_registry`** in **`spec_lock_*_verify.json`**). Report shape and **`jq`**: **`blvm-spec-lock`** **`docs/VERIFY_JSON.md`**.

## References

- [blvm-spec-lock coverage](https://github.com/BTCDecoded/blvm-spec-lock/blob/main/SPEC_LOCK_COVERAGE.md)
- [Orange paper §10.6](https://github.com/BTCDecoded/blvm-spec/blob/main/THE_ORANGE_PAPER.md) – Dandelion++ k-Anonymity
