//! Storage on the redb backend.
//!
//! Redb only opens tables it has a static definition for. Any tree the storage layer opens
//! that is missing from `redb_impl::get_table_def` makes `Storage::with_backend(.., Redb)` fail
//! (`ChainState::new` opens `block_index` via `BlockIndex::new`).

#[cfg(feature = "redb")]
mod redb_storage_tests {
    use blvm_node::storage::Storage;
    use blvm_node::storage::database::{DatabaseBackend, KNOWN_TREE_NAMES, create_database};
    use blvm_protocol::{Block, BlockHeader};
    use tempfile::TempDir;

    fn test_block() -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1234567890,
                bits: 0x1d00ffff,
                nonce: 0,
            },
            transactions: vec![].into_boxed_slice(),
        }
    }

    /// `Storage::with_backend(.., Redb)` opens, stores a block, and reopens the same datadir.
    #[test]
    fn storage_opens_and_reopens_on_redb() {
        let temp_dir = TempDir::new().unwrap();
        {
            let storage = Storage::with_backend(temp_dir.path(), DatabaseBackend::Redb)
                .expect("redb storage must open");
            storage.blocks().store_block(&test_block()).unwrap();
            assert_eq!(storage.blocks().block_count().unwrap(), 1);
            storage.flush().unwrap();
        }
        // Second open takes the existing-database path in `RedbDatabase::new`.
        let storage = Storage::with_backend(temp_dir.path(), DatabaseBackend::Redb)
            .expect("redb storage must reopen");
        assert_eq!(storage.blocks().block_count().unwrap(), 1);
    }

    /// Every production tree name opens on redb, except the documented gaps below.
    #[test]
    fn redb_opens_every_known_tree() {
        // Not registered on redb yet:
        // - IBD engine ping-pong checkpoints. Registering them turns on engine checkpoints for
        //   redb nodes, which is an IBD behaviour change; tracked separately.
        // - Names that only `tests/rocksdb_tests.rs` uses as column families.
        const NOT_ON_REDB: &[&str] = &["ibd_utxos_ckpt_a", "ibd_utxos_ckpt_b"];
        fn rocksdb_test_only(name: &str) -> bool {
            matches!(name, "test_tree" | "tree1" | "tree2") || name.starts_with("dynamic_tree_")
        }

        let temp_dir = TempDir::new().unwrap();
        let db = create_database(temp_dir.path(), DatabaseBackend::Redb, None).unwrap();
        let missing: Vec<&str> = KNOWN_TREE_NAMES
            .iter()
            .copied()
            .filter(|n| !NOT_ON_REDB.contains(n) && !rocksdb_test_only(n))
            .filter(|n| db.open_tree(n).is_err())
            .collect();
        assert!(missing.is_empty(), "redb has no table for: {missing:?}");
    }
}
