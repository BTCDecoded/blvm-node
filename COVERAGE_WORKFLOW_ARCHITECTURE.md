# Coverage Workflow Architecture

## Overview

**Separate workflow is the BEST approach** - workflows in GitHub Actions are independent and cannot directly "call" each other. A separate workflow is cleaner and more maintainable.

## Architecture

```
┌─────────────────┐         ┌──────────────────┐
│   ci.yml        │         │  coverage.yml    │
│                 │         │                  │
│  - Test job     │         │  - Coverage job  │
│    (fast, ~5min)│         │    (slow, ~35min)│
│                 │         │                  │
│  - Clippy       │         │  - Only coverage │
│  - Build        │         │  - No tests      │
│  - Verify       │         │                  │
└─────────────────┘         └──────────────────┘
      │                              │
      │                              │
      └──────────┬───────────────────┘
                 │
          Independent workflows
          (not called from each other)
```

## Coverage Workflow Triggers

### 1. Manual Trigger (workflow_dispatch)
- **Default**: Enabled (checkbox checked by default)
- **Usage**: Go to Actions → Coverage → Run workflow
- **When to use**: 
  - Before a release
  - After major changes
  - When you need coverage data immediately

### 2. Automatic on Main Branch
- **Trigger**: Push to `main` branch
- **When**: After code is merged to main
- **Purpose**: Track coverage trends automatically

### 3. Weekly Schedule
- **Trigger**: Every Sunday at 00:00 UTC
- **Purpose**: Regular coverage tracking
- **Frequency**: Once per week

## Coverage is Skipped by Default in CI

**In `ci.yml`**: Coverage is removed from the test job
- Tests run fast (~5-10 min)
- No coverage collection
- Immediate PR feedback

**In `coverage.yml`**: Coverage runs separately
- Only runs coverage (no tests)
- Doesn't block PRs
- Can be triggered on-demand

## Why Separate Workflow?

### ✅ Advantages
1. **Independent**: No coupling between test and coverage
2. **Flexible**: Can run coverage without running tests
3. **Fast CI**: Tests don't wait for coverage
4. **On-demand**: Can trigger coverage when needed
5. **Scheduled**: Can run on schedule without affecting CI

### ❌ Alternative (Not Recommended)
- **Calling from ci.yml**: GitHub Actions doesn't support directly calling workflows
- **Workflow chaining**: Would require `workflow_run` triggers, more complex
- **Conditional steps**: Would still slow down CI if enabled

## How to Use

### Run Coverage Manually
1. Go to GitHub Actions tab
2. Select "Coverage" workflow
3. Click "Run workflow"
4. Check "Run coverage report" (default: checked)
5. Click "Run workflow" button

### Coverage Runs Automatically
- When code is merged to `main`
- Every Sunday at midnight UTC

### Check Coverage Results
- Coverage report uploaded to Codecov
- View in Codecov dashboard
- Coverage percentage shown in workflow summary

## Workflow Comparison

| Aspect | ci.yml (test job) | coverage.yml |
|--------|-------------------|--------------|
| **Purpose** | Fast feedback | Coverage tracking |
| **Runs** | Every PR/push | Main branch, weekly, manual |
| **Speed** | ~5-10 min | ~35 min |
| **Blocks PRs** | Yes (must pass) | No (separate) |
| **Includes** | Tests only | Coverage only |
| **Trigger** | Automatic | Manual + scheduled |

## Implementation Status

✅ **Coverage workflow created**: `.github/workflows/coverage.yml`
- Has `workflow_dispatch` with default enabled
- Runs on main branch pushes
- Scheduled weekly
- Only runs coverage (no tests)

⏳ **CI workflow update needed**: `.github/workflows/ci.yml`
- Remove coverage from test job
- Keep tests only (fast feedback)

## Next Steps

1. ✅ Coverage workflow created
2. ⏳ Remove coverage from `ci.yml` test job
3. ⏳ Test both workflows
4. ⏳ Monitor performance improvements

