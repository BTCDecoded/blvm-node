# RocksDB Implementation Notes

## Dependency Conflict - RESOLVED

**Issue**: RocksDB and the erlay feature (minisketch-rs) have a dependency conflict:
- `rocksdb` requires `clang-sys ^1` (via bindgen 0.65+)
- `minisketch-rs` requires `clang-sys ^0.28` (via bindgen 0.49)

**Status**: ✅ **RESOLVED** - Erlay/minisketch-rs dependency has been disabled

**Solution**: The `minisketch-rs` dependency and `erlay` feature have been commented out in Cargo.toml. This resolves the dependency conflict. RocksDB can now be used without issues.

**To re-enable erlay** (not recommended with rocksdb):
1. Uncomment `minisketch-rs` dependency in Cargo.toml
2. Uncomment `erlay` feature in Cargo.toml
3. Note: You cannot use both erlay and rocksdb simultaneously

**Note**: RocksDB itself requires libclang to build (for its FFI bindings). This is a system dependency, not a Cargo dependency conflict. Ensure libclang is installed on your system.

## Implementation Status

### ✅ Phase 1 & 2 Complete

**Phase 1: Core Functionality**
1. ✅ Added RocksDB dependency to Cargo.toml
2. ✅ Updated DatabaseBackend enum to include RocksDB
3. ✅ Created rocksdb_impl module with Database and Tree implementations
4. ✅ Updated create_database() to handle RocksDB backend
5. ✅ Updated fallback_backend() to include RocksDB
6. ✅ Added Bitcoin Core detection module (`bitcoin_core_detection.rs`)
7. ✅ Added Bitcoin Core format parser (`bitcoin_core_format.rs`)
8. ✅ Added Bitcoin Core storage integration (`bitcoin_core_storage.rs`)
9. ✅ Integrated Bitcoin Core detection with storage initialization
10. ✅ Resolved dependency conflict (erlay disabled)

**Phase 2: Block File Integration**
11. ✅ Added Bitcoin Core block file reader (`bitcoin_core_blocks.rs`)
12. ✅ Integrated block file reader with BlockStore (fallback reading)
13. ✅ Automatic block file detection and initialization
14. ✅ Lazy index building for block files

### ⏳ Phase 3 (Optional)
1. Migration tool (RocksDB to redb)
2. Performance optimizations
3. Testing with real Bitcoin Core data

## Usage

### Detecting Bitcoin Core Data

The storage system will automatically detect Bitcoin Core data directories when initializing:

```rust
use blvm_node::storage::Storage;

// Automatically detects Bitcoin Core data if present
let storage = Storage::new("./data")?;
```

### Manual Detection

```rust
use blvm_node::storage::bitcoin_core_detection::{BitcoinCoreDetection, BitcoinCoreNetwork};
use blvm_node::storage::bitcoin_core_storage::BitcoinCoreStorage;

// Detect Bitcoin Core mainnet data
if let Some(core_dir) = BitcoinCoreDetection::detect_data_dir(BitcoinCoreNetwork::Mainnet)? {
    // Open with RocksDB
    let db = BitcoinCoreStorage::open_bitcoin_core_database(&core_dir, BitcoinCoreNetwork::Mainnet)?;
}
```

## Known Limitations

1. **Dependency Conflict**: RocksDB and erlay cannot be used together (documented above) - RESOLVED by disabling erlay
2. **Format Parsing**: Bitcoin Core format parser is implemented but may need enhancement for edge cases
3. **Block Files**: Block file reading (`blk*.dat`) is implemented with lazy indexing - first access builds the index
4. **Migration**: Migration tool not yet implemented (optional Phase 3)

