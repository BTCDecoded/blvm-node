//! Quick heed3 smoke test: `cargo run --example verify_heed3 --features heed3`

fn main() -> anyhow::Result<()> {
    use blvm_node::storage::database::{create_database, Database, DatabaseBackend};
    use std::sync::Arc;

    let temp = tempfile::tempdir()?;
    let db: Arc<dyn Database> =
        Arc::from(create_database(temp.path(), DatabaseBackend::Heed3, None)?);
    let tree = db.open_tree("blocks")?;
    tree.insert(b"smoke", b"ok")?;
    assert_eq!(tree.get(b"smoke")?, Some(b"ok".to_vec()));
    println!("heed3 OK at {:?}", temp.path());
    Ok(())
}
