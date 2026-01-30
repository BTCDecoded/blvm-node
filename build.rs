//! Build script to check for conflicting features

fn main() {
    // Check if both rocksdb and erlay features are enabled
    // This would cause a clang-sys version conflict
    #[cfg(all(feature = "rocksdb", feature = "erlay"))]
    {
        panic!(
            "ERROR: rocksdb and erlay features cannot be enabled simultaneously.\n\
             They conflict due to clang-sys version requirements:\n\
             - rocksdb requires clang-sys ^1 (via bindgen 0.65+)\n\
             - erlay/minisketch-rs requires clang-sys ^0.28 (via bindgen 0.49)\n\
             \n\
             Solution: Enable only one of these features at a time.\n\
             Example: cargo build --features rocksdb --no-default-features\n\
             Or: cargo build --features erlay (without rocksdb)"
        );
    }

    // Note: Even with --no-default-features, Cargo may still try to resolve
    // optional dependencies like minisketch-rs, causing a conflict.
    // Users must ensure erlay feature is not enabled when using rocksdb.
    // The build will fail at dependency resolution if both are present.
}

