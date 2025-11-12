# Workflow Caching Analysis for bllvm-node

## Current State

### Issues Identified

1. **No Shared Cache Setup**: Each job (test, clippy, fmt, docs, build) independently:
   - Checks out dependencies (bllvm-protocol, bllvm-consensus)
   - Sets up Rust toolchain
   - Configures cache
   - This wastes time and doesn't share cache keys

2. **GitHub Actions Cache Only**: Using `actions/cache@v3` which:
   - Has API rate limits
   - Slower than local filesystem caching
   - Limited to 10GB per cache entry
   - Can be slow for large dependency sets

3. **No Cross-Repo Build Artifact Caching**: 
   - bllvm-consensus and bllvm-protocol are checked out but their `target/` directories aren't cached across runs
   - Each run rebuilds dependencies from scratch

4. **No Cache Key Sharing**: Each job generates its own cache keys, preventing sharing

5. **Redundant Dependency Checkout**: Same dependencies checked out 5+ times per workflow run

## Opportunities from MyBitcoinFuture

### 1. Local Caching System (10-100x faster)

**Pattern**: Use `/tmp/runner-cache` with rsync for ultra-fast cache operations

**Benefits**:
- 10-100x faster than GitHub Actions cache
- No API rate limits
- Works offline once cached
- Preserves symlinks and permissions

**Implementation**:
```yaml
- name: Setup local cache system
  id: setup-cache
  run: |
    CACHE_ROOT="/tmp/runner-cache"
    DEPS_KEY=$(sha256sum Cargo.lock | cut -d' ' -f1)
    TOOLCHAIN=$(grep -E '^channel|rust-version' rust-toolchain.toml Cargo.toml 2>/dev/null | head -1 | sha256sum | cut -d' ' -f1 || echo "stable")
    CACHE_KEY="${DEPS_KEY}-${TOOLCHAIN}"
    
    CARGO_CACHE_DIR="$CACHE_ROOT/cargo/$CACHE_KEY"
    TARGET_CACHE_DIR="$CACHE_ROOT/target/$CACHE_KEY"
    CONSENSUS_TARGET_DIR="$CACHE_ROOT/bllvm-consensus-target/$CACHE_KEY"
    PROTOCOL_TARGET_DIR="$CACHE_ROOT/bllvm-protocol-target/$CACHE_KEY"
    
    echo "CARGO_CACHE_DIR=$CARGO_CACHE_DIR" >> $GITHUB_ENV
    echo "TARGET_CACHE_DIR=$TARGET_CACHE_DIR" >> $GITHUB_ENV
    echo "CONSENSUS_TARGET_DIR=$CONSENSUS_TARGET_DIR" >> $GITHUB_ENV
    echo "PROTOCOL_TARGET_DIR=$PROTOCOL_TARGET_DIR" >> $GITHUB_ENV
    echo "cache-key=$CACHE_KEY" >> $GITHUB_OUTPUT
    
    mkdir -p "$CARGO_CACHE_DIR"/{registry,git} "$TARGET_CACHE_DIR" "$CONSENSUS_TARGET_DIR" "$PROTOCOL_TARGET_DIR"
```

### 2. Shared Setup Job

**Pattern**: One setup job that all other jobs depend on

**Benefits**:
- Checkout dependencies once
- Generate cache keys once
- Share cache keys via job outputs
- Parallel execution after setup

**Structure**:
```yaml
jobs:
  setup:
    outputs:
      cache-key: ${{ steps.setup-cache.outputs.cache-key }}
      consensus-cache-key: ${{ steps.setup-cache.outputs.consensus-cache-key }}
      protocol-cache-key: ${{ steps.setup-cache.outputs.protocol-cache-key }}
  
  test:
    needs: setup
    # Uses cache-key from setup
  
  clippy:
    needs: setup
    # Uses cache-key from setup
```

### 3. Cross-Repo Build Artifact Caching

**Pattern**: Cache `target/` directories for bllvm-consensus and bllvm-protocol

**Benefits**:
- Don't rebuild dependencies on every run
- Faster incremental builds
- Shared across all bllvm repos

**Implementation**:
```yaml
- name: Restore bllvm-consensus build artifacts
  run: |
    if [ -d "$CONSENSUS_TARGET_DIR" ] && [ "$(ls -A $CONSENSUS_TARGET_DIR 2>/dev/null)" ]; then
      echo "🚀 Restoring bllvm-consensus target from cache..."
      rsync -a --delete "$CONSENSUS_TARGET_DIR/" ../bllvm-consensus/target/ || true
      echo "✅ bllvm-consensus target restored"
    fi

- name: Cache bllvm-consensus build artifacts
  if: always()
  run: |
    if [ -d "../bllvm-consensus/target" ]; then
      rsync -a --delete ../bllvm-consensus/target/ "$CONSENSUS_TARGET_DIR/" || true
      echo "✅ bllvm-consensus target cached"
    fi
```

### 4. Disk Space Management

**Pattern**: Emergency checks and proactive cleanup

**Benefits**:
- Prevents runner failures from disk exhaustion
- Automatic cleanup of old caches

**Implementation**:
```yaml
- name: Emergency disk space check
  run: |
    echo "🔍 DEBUG: Disk space before setup:"
    df -h
    if [ $(df / | tail -1 | awk '{print $5}' | sed 's/%//') -gt 80 ]; then
      echo "⚠️ Disk space >80%, cleaning old caches..."
      find /tmp/runner-cache -maxdepth 2 -type d -mtime +7 -exec rm -rf {} + 2>/dev/null || true
    fi
```

### 5. Cache Cleanup Management

**Pattern**: Automatic cleanup of old cache entries

**Benefits**:
- Prevents disk exhaustion
- Keeps most recent caches

**Implementation**:
```yaml
- name: Cache cleanup management
  if: always()
  run: |
    CACHE_ROOT="/tmp/runner-cache"
    # Keep last 5 Cargo caches
    find "$CACHE_ROOT/cargo" -maxdepth 1 -type d -mtime +1 2>/dev/null | head -n -5 | xargs rm -rf 2>/dev/null || true
    # Keep last 3 target caches (larger)
    find "$CACHE_ROOT/target" -maxdepth 1 -type d -mtime +1 2>/dev/null | head -n -3 | xargs rm -rf 2>/dev/null || true
```

## Proposed Workflow Structure

```yaml
jobs:
  setup:
    # Checkout all dependencies
    # Setup cache keys
    # Output cache keys for other jobs
  
  test:
    needs: setup
    # Restore caches using keys from setup
    # Run tests
  
  clippy:
    needs: setup
    # Restore caches using keys from setup
    # Run clippy
  
  fmt:
    needs: setup
    # Minimal setup (no cache needed)
    # Run fmt check
  
  docs:
    needs: setup
    # Restore caches using keys from setup
    # Build docs
  
  build:
    needs: setup
    # Restore caches using keys from setup
    # Restore dependency build artifacts
    # Build
  
  security:
    needs: setup
    # Minimal setup
    # Run security audit
```

## Expected Performance Improvements

### Current (estimated)
- **Dependency checkout**: ~30s × 6 jobs = 180s
- **Cache restore**: ~20s × 6 jobs = 120s
- **Dependency build**: ~5min × 6 jobs = 30min (if no cache)
- **Total overhead**: ~35min

### With Improvements (estimated)
- **Dependency checkout**: ~30s (once in setup)
- **Cache restore**: ~5s × 6 jobs = 30s (local cache is 10x faster)
- **Dependency build**: ~30s (cached artifacts)
- **Total overhead**: ~2min

**Estimated speedup**: ~17x faster for setup overhead

## Implementation Priority

1. **High Priority**:
   - Shared setup job
   - Local caching system
   - Cross-repo build artifact caching

2. **Medium Priority**:
   - Disk space management
   - Cache cleanup management

3. **Low Priority**:
   - Enhanced debug output
   - Cache size reporting

