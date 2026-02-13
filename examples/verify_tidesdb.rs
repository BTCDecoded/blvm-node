//! Verify TidesDB backend works - run with:
//!   PKG_CONFIG_PATH=/path/to/lib/pkgconfig LD_LIBRARY_PATH=/path/to/lib cargo run --example verify_tidesdb
//!
//! Requires tidesdb feature and TidesDB C library installed.

#[cfg(feature = "tidesdb")]
fn main() -> anyhow::Result<()> {
    use blvm_node::storage::database::{create_database, Database, DatabaseBackend};
    use std::sync::Arc;

    type Db = Arc<dyn Database>;

    let temp = std::env::temp_dir().join(format!("blvm-tidesdb-verify-{}", std::process::id()));
    std::fs::create_dir_all(&temp)?;
    println!("Creating TidesDB at {:?}...", temp);
    let db: Db = Arc::from(create_database(&temp, DatabaseBackend::TidesDB, None)?);
    println!("  OK");

    println!("Opening tree, insert/get/remove...");
    let tree = db.open_tree("verify")?;
    tree.insert(b"key", b"value")?;
    assert_eq!(tree.get(b"key")?, Some(b"value".to_vec()));
    tree.remove(b"key")?;
    assert_eq!(tree.get(b"key")?, None);
    println!("  OK");

    println!("TidesDB backend verification PASSED");
    let _ = std::fs::remove_dir_all(&temp);
    Ok(())
}

#[cfg(not(feature = "tidesdb"))]
fn main() {
    eprintln!("Build with --features tidesdb");
    std::process::exit(1);
}
