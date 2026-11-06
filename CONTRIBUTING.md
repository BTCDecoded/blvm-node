# Contributing to blvm-node

Thank you for your interest in contributing to blvm-node! This document contains **repo-specific guidelines only**. For comprehensive contributing guidelines, see the [BLVM Documentation](https://docs.btcdecoded.org/blvm/development/contributing.html).

## Quick Links

- **[Complete Contributing Guide](https://docs.btcdecoded.org/blvm/development/contributing.html)** - Full developer workflow
- **[PR Process](https://docs.btcdecoded.org/blvm/development/pr-process.html)** - Governance tiers and review process
- **[Testing Infrastructure](https://docs.btcdecoded.org/blvm/development/testing.html)** - Testing guides

## Code of Conduct

This project follows the [Rust Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct). By participating, you agree to uphold this code.

## Repository-Specific Guidelines

### Node Implementation

**IMPORTANT:** This code implements a Bitcoin node. Changes must:

1. **Maintain compatibility** with Bitcoin network
2. **Not break consensus** validation (use blvm-consensus for consensus changes)
3. **Handle network protocols** correctly
4. **Preserve data integrity**

### Additional Requirements

- **Consensus Integrity**: Never modify consensus rules (use blvm-consensus for that)
- **Production Readiness**: Consider production deployment implications
- **Performance**: Maintain reasonable performance characteristics
- **Test Coverage**: >85% for node-critical code

### Development Setup

**crates.io / CI (no monorepo path patches):** Like other `blvm-*` crates, `blvm-muhash` is declared as `>=0.1, <1`. The crates.io release you depend on must include the MuHash APIs this tree uses (`serialize_running_state` / `deserialize_running_state`, `insert_mut` / `remove_mut` on the IBD flush hot path). Publish [`blvm-muhash`](https://crates.io/crates/blvm-muhash) before or with any `blvm-node` release that needs newer APIs.

Root **`Cargo.lock`** is **gitignored** and not tracked. **CI** runs **`cargo … --locked`** using a lockfile present on the **runner workspace** (generated or cached there). For **local** **`--locked`** builds, run **`cargo generate-lockfile`** or a normal **`cargo build`** / **`cargo test`** first so **`Cargo.lock`** exists beside **`Cargo.toml`**. Dependency resolution in CI follows **`Cargo.toml`** after stripping **`[patch.crates-io]`**, consistent with crates.io builds.

```bash
git clone https://github.com/BTCDecoded/blvm-node.git
cd blvm-node
cargo build
cargo test
```

### Running Tests

```bash
# Run all tests
cargo test

# Run with coverage
cargo tarpaulin --out Html --jobs 2

# Run specific test categories
cargo test --test integration_tests
cargo test --test storage_tests
```

### CI parity (default features)

This repo’s **CI** runs **`cargo test`** with **default** `[features].default` from `Cargo.toml` — not `--all-features`. For the same compile/link surface locally: **`cargo check -p blvm-node`**, then **`cargo test -p blvm-node --no-run`** (or **`cargo test`** to execute). If you touch optional features (`iroh`, `rocksdb`, etc.), add **`--features ...`** or **`--test <name>`** for those paths. **`cargo test --all-features`** is a separate, heavier matrix and is not what the main CI job runs.

### Review Criteria

Reviewers will check:

1. **Correctness** - Does the code work as intended?
2. **Node compatibility** - Does it maintain Bitcoin network compatibility?
3. **Test coverage** - Are all cases covered (>85%)?
4. **Performance** - No regressions?
5. **Documentation** - Is it clear and complete?
6. **Security** - Any potential vulnerabilities?

### Dependency bumps (`cargo audit`)

On **`iroh`**, **`quinn`**, **`hickory`**, **`time`**, or other security-sensitive upgrades:

1. Follow **`docs/AUDIT_SUPPRESSIONS.md`** — temporarily clear **`.cargo/audit.toml`** ignores, run **`cargo audit`**, and shrink ignores only when the graph supports it.
2. Attach raw audit output (or **`cargo tree -i`** snippets) to the PR when advisories remain **or** when another transitive edge still pulls a flagged version after fixing the primary dependency path.

## Getting Help

- **Documentation**: [docs.btcdecoded.org/blvm](https://docs.btcdecoded.org/blvm)
- **Issues**: Use GitHub issues for bugs and feature requests
- **Discussions**: Use GitHub discussions for questions
- **Security**: See [SECURITY.md](SECURITY.md)

Thank you for contributing to blvm-node!
