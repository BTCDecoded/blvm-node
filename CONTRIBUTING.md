# Contributing to reference-node

Thank you for your interest in contributing to reference-node! This document contains repo-specific guidelines. See the [BTCDecoded Contribution Guide](https://github.com/BTCDecoded/.github/blob/main/CONTRIBUTING.md) for general guidelines.

## Code of Conduct

This project follows the [Rust Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct). By participating, you agree to uphold this code.

## How to Contribute

### Reporting Issues

Before creating an issue, please:

1. **Search existing issues** to avoid duplicates
2. **Check the documentation** to ensure it's not a usage question
3. **Verify the issue** with the latest version

For security issues, see [SECURITY.md](SECURITY.md).

### Submitting Pull Requests

1. **Fork the repository**
2. **Create a feature branch** from `main`
3. **Make your changes** following our guidelines
4. **Add tests** for new functionality
5. **Update documentation** as needed
6. **Submit a pull request**

## Development Guidelines

### Code Style

We use `rustfmt` and `clippy` for code formatting and linting:

```bash
cargo fmt
cargo clippy -- -D warnings
```

### Testing Requirements

**All code must be thoroughly tested:**

- **Unit tests** for all new functions
- **Integration tests** for node components
- **Network tests** for P2P functionality
- **Storage tests** for data persistence
- **RPC tests** for API endpoints

**Test coverage must be >85%** for node-critical code.

### Node Implementation

**IMPORTANT:** This code implements a Bitcoin node. Changes must:

1. **Maintain compatibility** with Bitcoin network
2. **Not break consensus** validation
3. **Handle network protocols** correctly
4. **Preserve data integrity**

### Documentation

- **All public APIs** must be documented
- **Node components** need clear descriptions
- **Network protocols** should be well-documented
- **Examples** should be provided for key features

### Performance

- **No performance regressions** without justification
- **Benchmark node operations** performance
- **Consider memory usage** for blockchain data
- **Profile network operations**

## Development Setup

### Prerequisites

- Rust 1.70 or later
- Git
- A text editor or IDE

### Building

```bash
git clone https://github.com/BTCDecoded/reference-node
cd reference-node
cargo build
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

### Running Benchmarks

```bash
cargo bench
```

## Commit Message Format

Use conventional commits:

```
type(scope): description

[optional body]

[optional footer]
```

**Types:**
- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation changes
- `test`: Test additions/changes
- `refactor`: Code refactoring
- `perf`: Performance improvements
- `ci`: CI/CD changes

**Examples:**
```
feat(network): add peer discovery implementation
fix(storage): correct UTXO set persistence
docs(readme): update node configuration examples
test(rpc): add blockchain RPC method tests
```

## Review Process

### Pull Request Requirements

- [ ] **All tests pass**
- [ ] **Code is formatted** (`cargo fmt`)
- [ ] **No clippy warnings** (`cargo clippy`)
- [ ] **Documentation is updated**
- [ ] **Commit messages are clear**
- [ ] **Changes are minimal and focused**

### Review Criteria

Reviewers will check:

1. **Correctness** - Does the code work as intended?
2. **Node compatibility** - Does it maintain Bitcoin network compatibility?
3. **Test coverage** - Are all cases covered?
4. **Performance** - No regressions?
5. **Documentation** - Is it clear and complete?
6. **Security** - Any potential vulnerabilities?

### Approval Process

- **At least 2 approvals** required for node-critical changes
- **Security team review** for network security changes
- **Performance review** for storage/network changes
- **Documentation review** for API changes

## Release Process

### Versioning

We use [Semantic Versioning](https://semver.org/):

- **MAJOR**: Breaking changes to node APIs
- **MINOR**: New features, backward compatible
- **PATCH**: Bug fixes, backward compatible

### Release Checklist

- [ ] **All tests pass**
- [ ] **Documentation is updated**
- [ ] **CHANGELOG.md is updated**
- [ ] **Version is bumped**
- [ ] **Security audit completed**
- [ ] **Performance benchmarks pass**

## Getting Help

- **Documentation**: Check the README and inline docs
- **Issues**: Search existing issues or create new ones
- **Discussions**: Use GitHub Discussions for questions
- **Security**: See [SECURITY.md](SECURITY.md)

## Recognition

Contributors will be:

- **Listed in CONTRIBUTORS.md**
- **Mentioned in release notes** for significant contributions
- **Invited to the team** for consistent contributors

## Questions?

If you have questions about contributing, please:

1. **Check this document** first
2. **Search existing issues** for similar questions
3. **Create a new issue** with the "question" label
4. **Join our discussions** for general questions

Thank you for contributing to Bitcoin node implementation! 🚀
