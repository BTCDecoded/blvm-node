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

### blvm-spec-lock (Dandelion)

```bash
cd blvm-node
cargo spec-lock verify --crate-path . --section 10.6
```

### CI

Formal verification runs via blvm-spec-lock in CI. See `.github/workflows/ci.yml` for spec-lock verification steps.

## References

- [blvm-spec-lock Coverage](../../blvm-spec-lock/SPEC_LOCK_COVERAGE.md)
- [Orange Paper Section 10.6](../../blvm-spec/THE_ORANGE_PAPER.md) – Dandelion++ k-Anonymity
