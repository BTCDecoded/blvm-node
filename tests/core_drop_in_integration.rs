//! Core drop-in integration: migrate a real Core regtest fixture → open BLVM storage.
//!
//! Skips when no fixture is present. Generate with `scripts/gen-core-regtest-fixture.sh`.

#[cfg(feature = "rocksdb")]
mod core_drop_in {
    use blvm_node::storage::bitcoin_core_migrate::{
        migrate_core_data, migration_checkpoint_path, read_migration_marker, MigrateCoreArgs,
        MigrationPhase,
    };
    use blvm_node::storage::bitcoin_core_storage::BitcoinCoreStorage;
    use blvm_node::storage::bitcoin_detection::{BitcoinCoreDetection, CoreDataNetwork};
    use blvm_node::storage::database::DatabaseBackend;
    use blvm_node::storage::Storage;
    use serde::Deserialize;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tempfile::TempDir;

    #[derive(Debug, Deserialize)]
    struct FixtureMeta {
        height: u64,
        tip_hash: String,
        #[serde(default)]
        genesis_hash: Option<String>,
        #[serde(default)]
        tip_coinbase_txid: Option<String>,
        #[serde(default)]
        utxo_count: u64,
    }

    fn fixture_dir() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("BLVM_CORE_REGTEST_FIXTURE") {
            let path = PathBuf::from(p);
            if BitcoinCoreDetection::is_core_layout_at(&path) {
                return Some(path);
            }
        }
        let default =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/core-regtest-101");
        if BitcoinCoreDetection::is_core_layout_at(&default) {
            return Some(default);
        }
        None
    }

    fn read_fixture_meta(fixture: &Path) -> Option<FixtureMeta> {
        let path = fixture.join("fixture.json");
        let data = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    fn read_core_best_block(source: &Path) -> Option<[u8; 32]> {
        blvm_node::storage::bitcoin_core_migrate::read_core_best_block_hash(source).ok()?
    }

    fn rpc_hash_to_internal(hex_str: &str) -> Option<[u8; 32]> {
        let mut hash: [u8; 32] = hex::decode(hex_str).ok()?.try_into().ok()?;
        hash.reverse();
        Some(hash)
    }

    fn copy_dir_recursive(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let ty = entry.file_type().unwrap();
            let target = dst.join(entry.file_name());
            if ty.is_dir() {
                copy_dir_recursive(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    /// Isolated Core datadir copy so parallel tests do not contend on LevelDB LOCK.
    fn isolated_fixture_copy(fixture: &Path) -> TempDir {
        let dir = TempDir::new().unwrap();
        copy_dir_recursive(fixture, dir.path());
        dir
    }

    #[test]
    fn core_drop_in_migrate_verify_and_open() {
        let Some(fixture) = fixture_dir() else {
            eprintln!(
                "SKIP core_drop_in: no fixture (run blvm-node/scripts/gen-core-regtest-fixture.sh)"
            );
            return;
        };

        let meta = read_fixture_meta(&fixture);
        let expected_height = meta.as_ref().map(|m| m.height).unwrap_or(100);
        let expected_tip = meta
            .as_ref()
            .and_then(|m| rpc_hash_to_internal(&m.tip_hash))
            .or_else(|| read_core_best_block(&fixture));

        let core_copy = isolated_fixture_copy(&fixture);
        let source = core_copy.path().to_path_buf();
        BitcoinCoreStorage::ensure_not_locked(&source).expect("fixture copy must not be locked");

        let dest_root = TempDir::new().unwrap();
        let dest = dest_root.path().join("blvm");

        migrate_core_data(MigrateCoreArgs {
            source: source.clone(),
            destination: dest.clone(),
            network: CoreDataNetwork::Regtest,
            verify: true,
            verbose: false,
            dest_backend: Some(DatabaseBackend::RocksDB),
            stop_after: None,
            reuse_core_block_files: false,
        })
        .expect("migration should succeed on real Core fixture");

        let marker = read_migration_marker(&dest)
            .expect("read marker")
            .expect("migration marker must exist");
        assert_eq!(marker.network, "regtest");
        assert!(
            marker.height >= expected_height,
            "marker height {}",
            marker.height
        );
        if let Some(ref meta) = meta {
            assert_eq!(
                marker.tip_hash, meta.tip_hash,
                "marker tip_hash must match Core RPC hex"
            );
        }

        let storage =
            Storage::with_backend(&dest, DatabaseBackend::RocksDB).expect("open migrated store");
        let height = storage
            .chain()
            .get_height()
            .expect("get_height")
            .unwrap_or(0);
        assert!(
            height >= expected_height,
            "chain height {height} expected >={expected_height}"
        );

        if let Some(tip) = storage.chain().get_tip_hash().expect("tip hash") {
            if let Some(expected) = expected_tip {
                assert_eq!(tip, expected, "tip hash must match Core best block");
            }
            if let Some(ref meta) = meta {
                assert_eq!(
                    blvm_node::storage::hashing::hash_to_rpc_hex(&tip),
                    meta.tip_hash,
                    "marker tip_hash must match Core RPC display order"
                );
            }
        }
        drop(storage);
    }

    /// Same as `core_drop_in_migrate_verify_and_open` but destination is heed3 (default `auto` backend).
    #[cfg(feature = "heed3")]
    #[test]
    fn core_drop_in_migrate_to_heed3() {
        let Some(fixture) = fixture_dir() else {
            eprintln!(
                "SKIP core_drop_in_heed3: no fixture (run blvm-node/scripts/gen-core-regtest-fixture.sh)"
            );
            return;
        };

        let meta = read_fixture_meta(&fixture);
        let expected_height = meta.as_ref().map(|m| m.height).unwrap_or(100);
        let expected_tip = meta
            .as_ref()
            .and_then(|m| rpc_hash_to_internal(&m.tip_hash))
            .or_else(|| read_core_best_block(&fixture));

        let core_copy = isolated_fixture_copy(&fixture);
        let source = core_copy.path().to_path_buf();
        BitcoinCoreStorage::ensure_not_locked(&source).expect("fixture copy must not be locked");

        let dest_root = TempDir::new().unwrap();
        let dest = dest_root.path().join("blvm");

        migrate_core_data(MigrateCoreArgs {
            source: source.clone(),
            destination: dest.clone(),
            network: CoreDataNetwork::Regtest,
            verify: true,
            verbose: false,
            dest_backend: Some(DatabaseBackend::Heed3),
            stop_after: None,
            reuse_core_block_files: false,
        })
        .expect("Core → heed3 migration should succeed on real Core fixture");

        assert!(
            dest.join("heed3").is_dir(),
            "migration with Heed3 backend must create blvm/heed3/"
        );
        assert!(
            !dest.join("rocksdb").exists(),
            "heed3 migration must not create rocksdb/ subtree"
        );

        let marker = read_migration_marker(&dest)
            .expect("read marker")
            .expect("migration marker must exist");
        assert_eq!(marker.network, "regtest");
        assert!(marker.height >= expected_height);

        let storage = Storage::with_backend(&dest, DatabaseBackend::Heed3)
            .expect("open migrated heed3 store");
        assert_eq!(
            storage.utxo_value_codec(),
            blvm_node::storage::utxo_value_codec::ValueCodec::Rkyv
        );

        let height = storage
            .chain()
            .get_height()
            .expect("get_height")
            .unwrap_or(0);
        assert!(
            height >= expected_height,
            "chain height {height} expected >={expected_height}"
        );

        if let Some(tip) = storage.chain().get_tip_hash().expect("tip hash") {
            if let Some(expected) = expected_tip {
                assert_eq!(tip, expected, "tip hash must match Core best block");
            }
        }
    }

    #[test]
    fn core_drop_in_resumes_from_checkpoint() {
        let Some(fixture) = fixture_dir() else {
            eprintln!("SKIP core_drop_in_resume: no fixture");
            return;
        };

        let core_copy = isolated_fixture_copy(&fixture);
        let source = core_copy.path().to_path_buf();
        BitcoinCoreStorage::ensure_not_locked(&source).expect("fixture copy must not be locked");

        let dest_root = TempDir::new().unwrap();
        let dest = dest_root.path().join("blvm");
        let args = MigrateCoreArgs {
            source: source.clone(),
            destination: dest.clone(),
            network: CoreDataNetwork::Regtest,
            verify: false,
            verbose: false,
            dest_backend: Some(DatabaseBackend::RocksDB),
            stop_after: Some(MigrationPhase::Chainstate),
            reuse_core_block_files: false,
        };

        migrate_core_data(args.clone()).expect("partial migration should succeed");

        assert!(
            migration_checkpoint_path(&dest).is_file(),
            "checkpoint must exist after interrupted migration"
        );
        assert!(
            read_migration_marker(&dest).expect("read marker").is_none(),
            "migration marker must not exist until complete"
        );

        migrate_core_data(MigrateCoreArgs {
            verify: true,
            stop_after: None,
            ..args
        })
        .expect("resume migration should complete");

        assert!(
            !migration_checkpoint_path(&dest).is_file(),
            "checkpoint must be cleared after success"
        );
        let marker = read_migration_marker(&dest)
            .expect("read marker")
            .expect("migration marker must exist after resume");
        assert_eq!(marker.network, "regtest");
        assert!(marker.height >= 100);
    }

    #[test]
    fn core_drop_in_reuse_core_block_files() {
        let Some(fixture) = fixture_dir() else {
            eprintln!("SKIP core_drop_in_reuse: no fixture");
            return;
        };
        let meta = read_fixture_meta(&fixture).expect("fixture.json");

        let core_copy = isolated_fixture_copy(&fixture);
        let source = core_copy.path().to_path_buf();
        BitcoinCoreStorage::ensure_not_locked(&source).expect("fixture copy must not be locked");

        let dest = source.join("blvm");
        migrate_core_data(MigrateCoreArgs {
            source: source.clone(),
            destination: dest.clone(),
            network: CoreDataNetwork::Regtest,
            verify: true,
            verbose: false,
            dest_backend: Some(DatabaseBackend::RocksDB),
            stop_after: None,
            reuse_core_block_files: true,
        })
        .expect("reuse migration should succeed");

        let marker = read_migration_marker(&dest)
            .expect("read marker")
            .expect("marker must exist");
        assert_eq!(marker.reuse_core_blocks, Some(true));
        assert_eq!(
            marker.tip_hash, meta.tip_hash,
            "migration marker tip_hash must be RPC display order"
        );

        {
            let db: Arc<dyn blvm_node::storage::database::Database> = Arc::from(
                blvm_node::storage::database::create_database(
                    &dest,
                    DatabaseBackend::RocksDB,
                    None,
                )
                .expect("open dest db"),
            );
            let blocks_tree = db.open_tree("blocks").expect("blocks tree");
            assert_eq!(
                blocks_tree.len().expect("blocks len"),
                0,
                "reuse mode must not copy block bodies into blocks tree"
            );
            let tip_hash = blvm_node::storage::chainstate::ChainState::new(Arc::clone(&db))
                .expect("chain")
                .get_tip_hash()
                .expect("tip")
                .expect("tip hash");
            let native_blocks = blvm_node::storage::blockstore::BlockStore::new(Arc::clone(&db))
                .expect("blockstore");
            assert!(
                native_blocks
                    .get_block(&tip_hash)
                    .expect("lookup")
                    .is_none(),
                "native blockstore must not contain copied bodies"
            );
        }

        let storage = Storage::open_for_node(
            &source,
            blvm_protocol::types::Network::Regtest,
            Some(&blvm_node::config::StorageConfig {
                reuse_core_block_files: true,
                database_backend: blvm_node::config::DatabaseBackendConfig::Rocksdb,
                ..Default::default()
            }),
        )
        .expect("open with core block reader");

        let height = storage.chain().get_height().expect("height").unwrap_or(0);
        assert!(height >= 100, "expected migrated height, got {height}");

        let tip_hash = storage
            .chain()
            .get_tip_hash()
            .expect("tip")
            .expect("tip hash");
        let block = storage
            .blocks()
            .get_block(&tip_hash)
            .expect("get_block")
            .expect("tip block readable via Core blk files");
        assert!(!block.transactions.is_empty());

        if let Some(ref coinbase_txid) = meta.tip_coinbase_txid {
            let tx_hash = rpc_hash_to_internal(coinbase_txid).expect("coinbase txid");
            let indexed = storage
                .transactions()
                .get_transaction(&tx_hash)
                .expect("txindex lookup")
                .expect("coinbase must be indexed when reusing Core blocks/");
            assert!(
                !indexed.inputs.is_empty() || !indexed.outputs.is_empty(),
                "indexed coinbase must have vin or vout"
            );
        }
    }

    /// Reuse Core block files with heed3 destination (default `auto` backend path).
    #[cfg(feature = "heed3")]
    #[test]
    fn core_drop_in_reuse_core_block_files_heed3() {
        let Some(fixture) = fixture_dir() else {
            eprintln!("SKIP core_drop_in_reuse_heed3: no fixture");
            return;
        };
        let meta = read_fixture_meta(&fixture).expect("fixture.json");

        let core_copy = isolated_fixture_copy(&fixture);
        let source = core_copy.path().to_path_buf();
        BitcoinCoreStorage::ensure_not_locked(&source).expect("fixture copy must not be locked");

        let dest = source.join("blvm");
        migrate_core_data(MigrateCoreArgs {
            source: source.clone(),
            destination: dest.clone(),
            network: CoreDataNetwork::Regtest,
            verify: true,
            verbose: false,
            dest_backend: Some(DatabaseBackend::Heed3),
            stop_after: None,
            reuse_core_block_files: true,
        })
        .expect("heed3 reuse migration should succeed");

        assert!(dest.join("heed3").is_dir());
        let marker = read_migration_marker(&dest)
            .expect("read marker")
            .expect("marker must exist");
        assert_eq!(marker.reuse_core_blocks, Some(true));

        let storage = Storage::open_for_node(
            &source,
            blvm_protocol::types::Network::Regtest,
            Some(&blvm_node::config::StorageConfig {
                reuse_core_block_files: true,
                database_backend: blvm_node::config::DatabaseBackendConfig::Heed3,
                ..Default::default()
            }),
        )
        .expect("open heed3 store with core block reader");

        let height = storage.chain().get_height().expect("height").unwrap_or(0);
        assert!(height >= 100, "expected migrated height, got {height}");

        let tip_hash = storage
            .chain()
            .get_tip_hash()
            .expect("tip")
            .expect("tip hash");
        let block = storage
            .blocks()
            .get_block(&tip_hash)
            .expect("get_block")
            .expect("tip block readable via Core blk files");
        assert!(!block.transactions.is_empty());
        assert_eq!(marker.tip_hash, meta.tip_hash);
    }

    #[test]
    fn core_drop_in_reuse_resumes_txindex_phase() {
        let Some(fixture) = fixture_dir() else {
            eprintln!("SKIP core_drop_in_reuse_resume: no fixture");
            return;
        };
        let meta = read_fixture_meta(&fixture).expect("fixture.json");

        let core_copy = isolated_fixture_copy(&fixture);
        let source = core_copy.path().to_path_buf();
        BitcoinCoreStorage::ensure_not_locked(&source).expect("fixture copy must not be locked");

        let dest = source.join("blvm");
        let args = MigrateCoreArgs {
            source: source.clone(),
            destination: dest.clone(),
            network: CoreDataNetwork::Regtest,
            verify: false,
            verbose: false,
            dest_backend: Some(DatabaseBackend::RocksDB),
            stop_after: Some(MigrationPhase::BlockIndexes),
            reuse_core_block_files: true,
        };

        migrate_core_data(args.clone()).expect("partial reuse migration");
        assert!(
            migration_checkpoint_path(&dest).is_file(),
            "checkpoint must exist before block indexing"
        );

        migrate_core_data(MigrateCoreArgs {
            verify: true,
            stop_after: None,
            ..args
        })
        .expect("resume reuse migration");

        let marker = read_migration_marker(&dest)
            .expect("read marker")
            .expect("marker");
        assert_eq!(marker.reuse_core_blocks, Some(true));
        assert_eq!(marker.tip_hash, meta.tip_hash);

        let storage = Storage::with_backend(&dest, DatabaseBackend::RocksDB).expect("open");
        if let Some(ref coinbase_txid) = meta.tip_coinbase_txid {
            let tx_hash = rpc_hash_to_internal(coinbase_txid).expect("coinbase txid");
            assert!(
                storage
                    .transactions()
                    .get_transaction(&tx_hash)
                    .expect("lookup")
                    .is_some(),
                "resumed reuse migration must finish txindex"
            );
        }
    }

    #[test]
    fn core_drop_in_open_for_node_from_fixture_parent() {
        let Some(fixture) = fixture_dir() else {
            eprintln!("SKIP core_drop_in_open: no fixture");
            return;
        };

        let parent = TempDir::new().unwrap();
        let core_copy = parent.path().join("core");
        copy_dir_recursive(&fixture, &core_copy);

        let storage =
            Storage::open_for_node(&core_copy, blvm_protocol::types::Network::Regtest, None)
                .expect("open_for_node should auto-migrate Core layout");

        let height = storage.chain().get_height().expect("height").unwrap_or(0);
        assert!(
            height >= 100,
            "auto-migrate should reach regtest tip, got {height}"
        );

        let blvm_dir = core_copy.join("blvm");
        assert!(
            blvm_dir.exists(),
            "BLVM store should live under blvm/ subdir"
        );
        #[cfg(feature = "heed3")]
        assert!(
            blvm_dir.join("heed3").is_dir(),
            "auto-migrate with heed3 feature should create blvm/heed3/"
        );

        drop(storage);

        // Second start must reuse existing blvm/ without error.
        let storage2 =
            Storage::open_for_node(&core_copy, blvm_protocol::types::Network::Regtest, None)
                .expect("second open_for_node");
        let height2 = storage2.chain().get_height().expect("height").unwrap_or(0);
        assert_eq!(height2, height);
    }

    /// Migrate fixture → open storage + RPC handlers (DI-P1-05 smoke helper).
    fn migrated_fixture_rpc_setup() -> Option<(
        TempDir,
        TempDir,
        Arc<Storage>,
        blvm_node::rpc::blockchain::BlockchainRpc,
        blvm_node::rpc::rawtx::RawTxRpc,
        FixtureMeta,
    )> {
        use blvm_node::node::mempool::MempoolManager;
        use blvm_node::rpc::blockchain::BlockchainRpc;
        use blvm_node::rpc::rawtx::RawTxRpc;
        use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
        use std::sync::Arc;

        let fixture = fixture_dir()?;
        let meta = read_fixture_meta(&fixture)?;
        let core_copy = isolated_fixture_copy(&fixture);
        let source = core_copy.path().to_path_buf();
        BitcoinCoreStorage::ensure_not_locked(&source).ok()?;

        let dest_root = TempDir::new().ok()?;
        let dest = dest_root.path().join("blvm");
        migrate_core_data(MigrateCoreArgs {
            source,
            destination: dest.clone(),
            network: CoreDataNetwork::Regtest,
            verify: true,
            verbose: false,
            dest_backend: Some(DatabaseBackend::RocksDB),
            stop_after: None,
            reuse_core_block_files: false,
        })
        .ok()?;

        let storage = Arc::new(
            Storage::with_backend(&dest, DatabaseBackend::RocksDB).expect("open migrated store"),
        );
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).ok()?);
        let blockchain = BlockchainRpc::with_dependencies_and_protocol(storage.clone(), protocol);
        let rawtx = RawTxRpc::with_dependencies(
            storage.clone(),
            Arc::new(MempoolManager::new()),
            None,
            None,
        );
        Some((core_copy, dest_root, storage, blockchain, rawtx, meta))
    }

    /// DI-P1-05: post-migrate RPC spot checks (`getblockhash`, `getblock`, `getrawtransaction`).
    #[tokio::test]
    async fn core_drop_in_post_migrate_rpc_smoke() {
        let Some((_core, _dest, storage, blockchain, rawtx, meta)) = migrated_fixture_rpc_setup()
        else {
            eprintln!("SKIP core_drop_in_rpc: no fixture");
            return;
        };

        let tip_height = meta.height;
        let genesis_hash = blockchain
            .get_block_hash(0)
            .await
            .expect("getblockhash 0")
            .as_str()
            .expect("hash string")
            .to_string();
        let tip_hash = blockchain
            .get_block_hash(tip_height)
            .await
            .expect("getblockhash tip")
            .as_str()
            .expect("hash string")
            .to_string();

        assert_eq!(tip_hash, meta.tip_hash, "tip hash must match Core fixture");
        if let Some(ref expected_genesis) = meta.genesis_hash {
            assert_eq!(genesis_hash, *expected_genesis);
        }

        let block = blockchain.get_block(&tip_hash).await.expect("getblock tip");
        assert_eq!(
            block.get("hash").and_then(|v| v.as_str()),
            Some(tip_hash.as_str())
        );
        let txids = block
            .get("tx")
            .and_then(|v| v.as_array())
            .expect("getblock must list txids");
        assert!(!txids.is_empty(), "tip block must include coinbase tx");

        let coinbase_txid = txids[0].as_str().expect("txid").to_string();
        if let Some(ref expected) = meta.tip_coinbase_txid {
            assert_eq!(coinbase_txid, *expected, "coinbase txid vs Core fixture");
        }

        let verbose_tx = rawtx
            .getrawtransaction(&json!([coinbase_txid, true]))
            .await
            .expect("getrawtransaction coinbase after migrate txindex");
        assert_eq!(
            verbose_tx.get("txid").and_then(|v| v.as_str()),
            Some(coinbase_txid.as_str())
        );
        assert!(verbose_tx.get("vin").and_then(|v| v.as_array()).is_some());
        assert!(verbose_tx.get("hex").and_then(|v| v.as_str()).is_some());
    }
}

#[cfg(not(feature = "rocksdb"))]
mod core_drop_in {
    #[test]
    fn core_drop_in_requires_rocksdb() {
        eprintln!("SKIP core_drop_in: rocksdb feature disabled");
    }
}
