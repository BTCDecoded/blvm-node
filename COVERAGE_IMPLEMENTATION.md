# Coverage Implementation Plan

## Summary

Split test execution from coverage collection to:
- **Fast PR feedback**: Tests run in ~5-10 minutes (no coverage)
- **Coverage tracking**: Coverage runs separately, less frequently (35 min, but doesn't block PRs)

## Changes Required

### 1. Modify `ci.yml` - Remove Coverage from Test Job

**Current**: Test job runs tests + coverage together (35 min)
**New**: Test job runs tests only (~5-10 min)

**File**: `.github/workflows/ci.yml`
**Job**: `test` (lines 182-221)

**Change**: Remove coverage steps, keep only test execution:

```yaml
- name: Run tests
  run: |
    cargo test --all-features --verbose
```

### 2. Create Separate Coverage Workflow

**New file**: `.github/workflows/coverage.yml` (already created)

**Architecture**: 
- **Separate workflow** (not called from ci.yml)
- GitHub Actions workflows are independent - this is the cleanest approach
- No need to "call" from ci.yml - workflows run independently

**Triggers**:
- **Manual trigger** via `workflow_dispatch` (default: enabled) - run on-demand
- Main branch merges (automatic)
- Weekly schedule (Sunday 00:00 UTC)

**Benefits**:
- Coverage doesn't slow down PRs (completely separate)
- Still tracks coverage regularly
- Can run on-demand when needed
- Only runs coverage (no tests) - faster and focused

### 3. Migration Path

#### Phase 1: Split Jobs (Immediate)
1. Remove coverage from `test` job in `ci.yml`
2. Add new `coverage.yml` workflow
3. Test both workflows

#### Phase 2: Optimize (Next Week)
1. Test `cargo-llvm-cov` locally
2. Compare performance vs tarpaulin
3. If faster, migrate coverage workflow to llvm-cov

#### Phase 3: Incremental Coverage (Future)
1. Add diff-based coverage for PRs
2. Only measure changed files
3. Full coverage still on main/weekly

## Performance Comparison

| Scenario | Current | After Split | Improvement |
|----------|---------|-------------|-------------|
| PR Test Run | 35 min | ~5-10 min | **70-85% faster** |
| Coverage Run | 35 min | 35 min | Same (but doesn't block PRs) |
| Developer Feedback | Slow | Fast | ✅ Immediate |

## File Changes

### Files to Modify
1. `.github/workflows/ci.yml` - Remove coverage from test job
2. `.github/workflows/coverage.yml` - New file (already created)

### Files Created
1. `docs/COVERAGE_STRATEGY.md` - Strategy document
2. `docs/COVERAGE_IMPLEMENTATION.md` - This file

## Testing Plan

1. **Test locally**: Run `cargo test --all-features` to verify speed
2. **Test coverage workflow**: Manually trigger coverage workflow
3. **Monitor**: Check that PRs are faster, coverage still works

## Rollback Plan

If issues arise:
1. Revert `ci.yml` changes (add coverage back to test job)
2. Keep `coverage.yml` as additional workflow
3. Both can run in parallel if needed

## Next Steps

1. Review this plan
2. Modify `ci.yml` to remove coverage from test job
3. Test the changes
4. Monitor performance improvements

