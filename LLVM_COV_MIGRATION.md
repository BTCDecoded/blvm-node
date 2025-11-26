# Migration from cargo-tarpaulin to cargo-llvm-cov

## Complexity Assessment: **LOW to MEDIUM** ⚠️

The migration is relatively straightforward but requires a few changes.

## Requirements

### 1. LLVM Tools Component
**Required**: `llvm-tools-preview` component must be installed

```bash
rustup component add llvm-tools-preview
```

**Note**: This is a one-time setup per environment (local dev, CI runners)

### 2. Installation
```bash
cargo install cargo-llvm-cov
```

## Command Mapping

### Current (tarpaulin)
```bash
cargo tarpaulin \
  --lib --bins --tests \
  --all-features \
  --out xml \
  --output-dir coverage \
  --skip-clean \
  --timeout 120 \
  --jobs 2
```

### New (llvm-cov)
```bash
cargo llvm-cov \
  --lib --bins --tests \
  --all-features \
  --lcov \
  --output-path coverage/lcov.info \
  --workspace
```

**Key Differences**:
- `--out xml` → `--lcov` (different format)
- `--output-dir` → `--output-path` (file path, not directory)
- `--skip-clean` → Not needed (llvm-cov handles this better)
- `--timeout` → Not needed (different timeout mechanism)
- `--jobs` → Not needed (llvm-cov handles parallelism better)
- Need to add `--workspace` for workspace support

## Codecov Compatibility

**Good news**: Codecov supports both formats!

- **Tarpaulin**: `cobertura.xml` (XML format)
- **llvm-cov**: `lcov.info` (LCOV format)

Codecov accepts both, so no changes needed to upload step.

## Files to Modify

### 1. CI Workflow (`.github/workflows/coverage.yml`)
**Lines to change**: ~80-100 (coverage step)

**Before**:
```yaml
- name: Install tarpaulin
  run: cargo install cargo-tarpaulin --version 0.27.0

- name: Run tests with coverage
  run: |
    cargo tarpaulin --lib --bins --tests --all-features \
      --out xml --output-dir coverage \
      --skip-clean --timeout 120 --jobs 2
```

**After**:
```yaml
- name: Install LLVM tools
  run: rustup component add llvm-tools-preview

- name: Install cargo-llvm-cov
  run: cargo install cargo-llvm-cov

- name: Run tests with coverage
  run: |
    cargo llvm-cov --lib --bins --tests --all-features \
      --lcov --output-path coverage/lcov.info \
      --workspace
```

### 2. Codecov Upload Step
**Change**: Update file path

**Before**:
```yaml
file: coverage/cobertura.xml
```

**After**:
```yaml
file: coverage/lcov.info
```

### 3. CONTRIBUTING.md
**Line 99**: Update example command

**Before**:
```bash
cargo tarpaulin --out Html --jobs 2
```

**After**:
```bash
cargo llvm-cov --html --output-dir coverage/html
```

### 4. Cargo.toml (Optional)
**Line 177**: Remove or update dev-dependency

**Before**:
```toml
cargo-tarpaulin = "0.27"
```

**After**: (Remove - not needed as dev-dependency, installed via cargo install)

## Potential Issues

### 1. LLVM Tools Availability
- **CI**: Need to ensure `llvm-tools-preview` is available on runners
- **Local**: One-time setup per developer machine
- **Solution**: Add component installation step in CI

### 2. Output Format Differences
- **Tarpaulin**: XML (cobertura format)
- **llvm-cov**: LCOV format (also widely supported)
- **Impact**: Codecov supports both, so minimal impact

### 3. Feature Flag Differences
- Some tarpaulin flags don't exist in llvm-cov
- Most have equivalents or aren't needed
- **Impact**: Low - flags are mostly compatible

### 4. Workspace Support
- Need `--workspace` flag for workspace projects
- **Impact**: Just add the flag

## Testing the Migration

### Step 1: Test Locally
```bash
# Install
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov

# Test
cargo llvm-cov --lib --bins --tests --all-features --lcov --output-path test-lcov.info

# Compare with tarpaulin
cargo tarpaulin --lib --bins --tests --all-features --out xml --output-dir test-coverage
```

### Step 2: Compare Results
- Check coverage percentages match (should be similar)
- Verify output files are generated correctly
- Test Codecov upload with new format

### Step 3: Update CI
- Modify coverage workflow
- Test on a branch/PR
- Verify Codecov receives data correctly

## Rollback Plan

If issues arise:
1. Revert workflow changes
2. Keep tarpaulin as fallback
3. Both tools can coexist (just different commands)

## Estimated Effort

- **Setup**: 15 minutes (install tools, test locally)
- **Code changes**: 30 minutes (update workflows/docs)
- **Testing**: 1 hour (verify in CI, check Codecov)
- **Total**: ~2 hours

## Benefits After Migration

1. **Faster**: 40-50% faster than tarpaulin
2. **Lower memory**: Better memory usage (no need for --jobs 2)
3. **More accurate**: LLVM source-based coverage
4. **Better incremental**: Built-in diff coverage support
5. **Official**: Recommended by Rust project

## Recommendation

**Start with Phase 1 (split jobs) first**, then migrate to llvm-cov in Phase 2. This way:
- You get immediate speed improvements (fast test runs)
- You can test llvm-cov migration separately
- Lower risk - can rollback coverage tool without affecting test speed

## Migration Checklist

- [ ] Install llvm-tools-preview locally
- [ ] Install cargo-llvm-cov locally
- [ ] Test llvm-cov command locally
- [ ] Compare coverage results with tarpaulin
- [ ] Update `.github/workflows/coverage.yml`
- [ ] Update Codecov upload step
- [ ] Update `CONTRIBUTING.md`
- [ ] Remove `cargo-tarpaulin` from `Cargo.toml` (optional)
- [ ] Test in CI on a branch
- [ ] Verify Codecov receives data
- [ ] Monitor for any issues

