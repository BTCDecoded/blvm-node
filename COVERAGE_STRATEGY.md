# Test Coverage Strategy for bllvm-node

## Current Situation

- **Coverage**: 23% (needs significant improvement)
- **Test Count**: ~354 test functions across 63 test files
- **Current Runtime**: 35 minutes for tests + coverage
- **Problem**: As we add more tests to improve coverage, runtime will increase significantly
- **Tool**: Currently using `cargo tarpaulin` with `--jobs 2` to prevent OOM

## Goals

1. **Improve coverage** from 23% to 80%+ over time
2. **Keep CI fast** - tests should run quickly on every PR
3. **Accurate coverage reporting** - but not blocking every run
4. **Scalable** - solution must work as test count grows

## Proposed Strategy

### 1. Separate Test Execution from Coverage Collection

**Current**: Tests + coverage run together (35 min)
**Proposed**: 
- **Fast path**: Run tests only (no coverage) on every PR/push (~5-10 min)
- **Coverage path**: Run coverage separately, less frequently

**Benefits**:
- PRs merge faster
- Coverage doesn't block development
- Can optimize coverage runs independently

### 2. Coverage Workflow Options

#### Option A: Scheduled Coverage (Recommended)
- Run coverage **weekly** (Sunday 00:00 UTC)
- Run coverage on **main branch merges** only
- Run coverage **on-demand** via workflow_dispatch

**Pros**: 
- Doesn't slow down PRs
- Regular coverage tracking
- Can catch coverage regressions

**Cons**: 
- Coverage data may be slightly stale

#### Option B: Coverage on Main Branch Only
- Run coverage only when code is merged to `main`
- Skip coverage on PRs and feature branches

**Pros**: 
- Always up-to-date coverage for main
- Fast PR feedback

**Cons**: 
- No coverage feedback during PR review

#### Option C: Incremental Coverage
- Only measure coverage for **changed files**
- Use `cargo-llvm-cov` with `--diff` option
- Run on every PR but only for changed code

**Pros**: 
- Fast (only changed files)
- Immediate feedback on PRs
- Encourages test coverage for new code

**Cons**: 
- Doesn't track overall coverage trends
- Requires more setup

### 3. Tool Comparison

#### cargo-tarpaulin (Current)
- **Pros**: Simple, widely used, good Rust integration
- **Cons**: Slow, high memory usage, requires `--jobs 2` to prevent OOM
- **Performance**: ~35 min for full coverage

#### cargo-llvm-cov (Recommended Alternative)
- **Pros**: 
  - Faster (uses LLVM source-based coverage)
  - Lower memory usage
  - Better incremental coverage support
  - Official Rust recommendation
  - Can do diff-based coverage
- **Cons**: 
  - Requires nightly Rust or specific LLVM setup
  - Slightly more complex setup
- **Performance**: ~15-20 min estimated (50% faster)

#### grcov
- **Pros**: 
  - Very fast
  - Good for large codebases
  - Used by Mozilla/Firefox
- **Cons**: 
  - More setup complexity
  - Less Rust-native integration

### 4. Recommended Implementation

**Phase 1: Immediate (Keep Current, Add Fast Path)**
1. Split `test` job into:
   - `test` job: Run tests only (no coverage) - fast feedback
   - `coverage` job: Run coverage separately (can be skipped)
2. Make coverage job conditional:
   - Only on `main` branch merges
   - Weekly schedule
   - Manual trigger via workflow_dispatch

**Phase 2: Tool Migration (After Testing)**
1. Test `cargo-llvm-cov` locally
2. Compare performance vs tarpaulin
3. If faster, migrate to `cargo-llvm-cov`
4. Keep tarpaulin as fallback option

**Phase 3: Incremental Coverage (Future)**
1. Implement diff-based coverage for PRs
2. Only measure changed files in PRs
3. Full coverage still runs on main/weekly

## Implementation Plan

### Step 1: Split Test and Coverage Jobs

```yaml
# Fast test job - runs on every PR
test:
  name: Test
  steps:
    - name: Run tests
      run: cargo test --all-features

# Separate coverage job - runs less frequently  
coverage:
  name: Coverage
  if: |
    github.ref == 'refs/heads/main' ||
    github.event_name == 'schedule' ||
    github.event_name == 'workflow_dispatch'
  steps:
    - name: Run coverage
      run: cargo tarpaulin --lib --bins --tests --all-features --out xml --output-dir coverage --skip-clean --timeout 120 --jobs 2
```

### Step 2: Test cargo-llvm-cov Locally

```bash
# Install
cargo install cargo-llvm-cov

# Run coverage
cargo llvm-cov --all-features --lib --bins --tests --lcov --output-path lcov.info

# Compare performance
time cargo tarpaulin --lib --bins --tests --all-features --jobs 2
time cargo llvm-cov --all-features --lib --bins --tests
```

### Step 3: Coverage Thresholds

Add coverage thresholds to prevent regressions:
- Current: 23%
- Target: 80% (long-term)
- Minimum: Don't allow coverage to decrease

### Step 4: Coverage Reporting

- Upload to Codecov (already configured)
- Add coverage badge to README
- Track coverage trends over time
- Set up coverage alerts for regressions

## Performance Estimates

| Scenario | Current (tarpaulin) | With llvm-cov | Improvement |
|----------|---------------------|---------------|-------------|
| Full coverage | 35 min | ~15-20 min | 40-50% faster |
| Tests only | N/A | ~5-10 min | Immediate feedback |
| Incremental (changed files) | N/A | ~2-5 min | 85-90% faster |

## Migration Checklist

- [ ] Split test and coverage jobs in CI
- [ ] Test `cargo-llvm-cov` locally
- [ ] Compare performance metrics
- [ ] Update CI to use faster tool if beneficial
- [ ] Set up scheduled coverage runs
- [ ] Add coverage thresholds
- [ ] Update documentation
- [ ] Monitor coverage trends

## Questions to Answer

1. **How often do we need coverage data?**
   - Weekly? Daily? On every merge?

2. **Do we need coverage feedback on PRs?**
   - Yes → Use incremental/diff coverage
   - No → Coverage only on main branch

3. **What's acceptable test runtime?**
   - Current: 35 min (with coverage)
   - Target: <10 min (tests only), <20 min (coverage)

4. **Coverage threshold policy?**
   - Hard requirement? Soft guideline?
   - Block merges if coverage decreases?

## Next Steps

1. **Immediate**: Split test and coverage jobs (this week)
2. **Short-term**: Test cargo-llvm-cov (next week)
3. **Medium-term**: Implement incremental coverage (next month)
4. **Long-term**: Reach 80% coverage target (ongoing)

