# Erlay (BIP330) Build Notes

## Overview

Erlay (BIP330) transaction relay optimization is an **entirely optional** feature. The implementation uses `minisketch-rs`, which provides Rust bindings to Pieter Wuille's minisketch library for efficient set reconciliation.

**Erlay requires minisketch-rs, which requires libclang/LLVM for bindgen compilation.**

## Build Requirements

To enable Erlay, you need:

1. **libclang/LLVM**: Required for `minisketch-rs` bindgen compilation
   - On Arch Linux: `pacman -S clang llvm`
   - On Ubuntu/Debian: `apt-get install libclang-dev llvm-dev`
   - On macOS: `brew install llvm`

2. **Enable the erlay feature**:
   ```bash
   cargo build --features erlay
   ```

## Feature Flags

- `erlay`: Enables Erlay protocol support (automatically includes minisketch-rs)
  - **Requires**: libclang/LLVM for minisketch-rs bindgen compilation
  - **Optional**: If libclang is not available, simply don't enable this feature

## Runtime Behavior

- **With `erlay`**: Full Erlay functionality enabled (requires libclang at build time)
- **Without `erlay`**: Erlay code is not compiled (feature-gated), no libclang needed

## CI Recommendations

For CI environments without libclang, **simply don't enable the erlay feature**:

```yaml
# Example GitHub Actions - Build without Erlay (no libclang needed)
- name: Build without Erlay
  run: cargo build --features compression,production

# If libclang is available, you can enable Erlay
- name: Build with Erlay (requires libclang)
  run: cargo build --features erlay,compression,production
```

## Testing

Erlay tests are feature-gated and will only run when the `erlay` feature is enabled:

```bash
# Run Erlay tests (requires libclang for minisketch-rs compilation)
cargo test --features erlay

# Run tests without Erlay (no libclang needed)
cargo test --features compression,production
```

## Summary

- **Erlay is optional**: If you don't have libclang, don't enable the `erlay` feature
- **Erlay requires minisketch-rs**: They are coupled - you can't have Erlay without minisketch-rs
- **minisketch-rs requires libclang**: For bindgen compilation
- **Simple rule**: No libclang = no erlay feature. Other optimizations (compression, parallel IBD) work fine without it.

