# Contributing to blvm-node

Thank you for your interest in contributing to blvm-node! This document contains **repo-specific guidelines only**. For comprehensive contributing guidelines, see the [BLVM Documentation](https://docs.thebitcoincommons.org/development/contributing.html).

## Quick Links

- **[Complete Contributing Guide](https://docs.thebitcoincommons.org/development/contributing.html)** - Full developer workflow
- **[PR Process](https://docs.thebitcoincommons.org/development/pr-process.html)** - Governance tiers and review process
- **[Testing Infrastructure](https://docs.thebitcoincommons.org/development/testing.html)** - Testing guides

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

### Review Criteria

Reviewers will check:

1. **Correctness** - Does the code work as intended?
2. **Node compatibility** - Does it maintain Bitcoin network compatibility?
3. **Test coverage** - Are all cases covered (>85%)?
4. **Performance** - No regressions?
5. **Documentation** - Is it clear and complete?
6. **Security** - Any potential vulnerabilities?

### Approval Process

- **At least 2 approvals** required for node-critical changes
- **Security team review** for network security changes
- **Performance review** for storage/network changes

## Getting Help

- **Documentation**: [docs.thebitcoincommons.org](https://docs.thebitcoincommons.org)
- **Issues**: Use GitHub issues for bugs and feature requests
- **Discussions**: Use GitHub discussions for questions
- **Security**: See [SECURITY.md](SECURITY.md)

Thank you for contributing to blvm-node!
