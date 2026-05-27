# blvm-node Documentation

Documentation for the blvm-node implementation.

## Core Documentation

- **[ARCHITECTURE.md](ARCHITECTURE.md)** - Node architecture and design decisions
- **[MODULE_SYSTEM.md](MODULE_SYSTEM.md)** - Module system architecture and usage
- **[NETWORK_VERIFICATION.md](NETWORK_VERIFICATION.md)** - Network verification and testing
- **[QUIC_RPC.md](QUIC_RPC.md)** - QUIC-based RPC implementation
- **[QUINN_INTEGRATION.md](QUINN_INTEGRATION.md)** - Quinn transport integration

## Spec-lock / Orange Paper CI

Authoritative **`cargo spec-lock verify`** (merged **`F_*`** registry + **`#[spec_locked]`** rows; **`formula_registry`** in **`verify`** JSON with **`--spec-path`**) is defined in **`.github/workflows/ci.yml`**. Structured report: **`blvm-spec-lock`** **`docs/VERIFY_JSON.md`**.

## Transport Documentation

- **[transport/](transport/)** - Transport abstraction documentation

