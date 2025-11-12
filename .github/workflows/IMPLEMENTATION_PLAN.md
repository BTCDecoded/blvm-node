# Build Chain Implementation Plan

## Overview

This document outlines the implementation of the enhanced build chain system across all BTCDecoded repositories, following MyBitcoinFuture patterns.

## Build Chain Flow

```
bllvm-spec (changes)
    ↓
bllvm-consensus (build + trigger)
    ↓
bllvm-protocol (build + trigger)
    ↓
bllvm-sdk (build + trigger)
    ↓
bllvm-node (final build)
```

## Repository Workflow Requirements

### 1. bllvm-spec
- **No build needed** (documentation only)
- **Triggers**: On push to main, trigger bllvm-consensus

### 2. bllvm-consensus
- **CI**: Test, clippy, fmt, docs, security
- **Build**: Library build (no binaries)
- **Triggers**: On successful build, trigger bllvm-protocol via repository_dispatch

### 3. bllvm-protocol
- **CI**: Test, clippy, fmt, docs, security
- **Build**: Library build (no binaries)
- **Triggers**: On successful build, trigger bllvm-sdk via repository_dispatch

### 4. bllvm-sdk
- **CI**: Test, clippy, fmt, docs, security
- **Build**: Library + binaries
- **Triggers**: On successful build, trigger bllvm-node via repository_dispatch

### 5. bllvm-node
- **CI**: Test, clippy, fmt, docs, security
- **Build**: Binary build (final artifact)
- **No triggers** (end of chain)

## Workflow Event Handling

### Push to main/develop
- Run CI checks only (test, clippy, fmt, docs, security)
- If successful and repo has downstream dependencies, trigger downstream via repository_dispatch

### Pull Request
- Run CI checks only
- No downstream triggers

### Release (published)
- Run full build chain
- Generate artifacts
- Trigger downstream if applicable

### repository_dispatch
- Handle upstream changes
- Run CI + build
- Trigger downstream if applicable

## Implementation Steps

1. ✅ **bllvm-node**: Enhanced CI with caching (DONE)
2. ⏳ **bllvm-consensus**: Apply enhanced CI + build chain trigger
3. ⏳ **bllvm-protocol**: Apply enhanced CI + build chain trigger
4. ⏳ **bllvm-sdk**: Apply enhanced CI + build chain trigger
5. ⏳ **bllvm-spec**: Add trigger workflow (no build needed)

## Enhanced Caching Strategy

All repos should use:
- Local caching (`/tmp/runner-cache` with rsync)
- Shared setup job
- Cross-repo build artifact caching
- Cache key based on Cargo.lock hash + toolchain version

## Validation

After implementation, validate:
- [ ] Push to bllvm-spec triggers bllvm-consensus
- [ ] Push to bllvm-consensus triggers bllvm-protocol
- [ ] Push to bllvm-protocol triggers bllvm-sdk
- [ ] Push to bllvm-sdk triggers bllvm-node
- [ ] Release workflow triggers full chain
- [ ] Caching works across repos
- [ ] Build artifacts are properly cached

