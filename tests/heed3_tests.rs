//! heed3 backend tests
//!
//! Requires liblmdb. Build with `--features heed3`.

#[cfg(feature = "heed3")]
mod heed3_tests {
    use blvm_node::storage::database::{Database, DatabaseBackend, create_database};
    use blvm_node::storage::rkyv_codec::{ValueCodec, access_utxo, encode_utxo};
    use blvm_protocol::types::UTXO;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn test_heed3_creation() {
        let temp_dir = TempDir::new().unwrap();
        let db = create_database(temp_dir.path(), DatabaseBackend::Heed3, None);
        assert!(db.is_ok());
    }

    #[test]
    fn test_heed3_tree_operations() {
        let temp_dir = TempDir::new().unwrap();
        let db: Arc<dyn Database> =
            Arc::from(create_database(temp_dir.path(), DatabaseBackend::Heed3, None).unwrap());

        let tree = db.open_tree("test_tree").unwrap();

        tree.insert(b"key1", b"value1").unwrap();
        tree.insert(b"key2", b"value2").unwrap();

        assert_eq!(tree.get(b"key1").unwrap(), Some(b"value1".to_vec()));
        assert_eq!(tree.len().unwrap(), 2);

        tree.remove(b"key2").unwrap();
        assert_eq!(tree.len().unwrap(), 1);

        tree.clear().unwrap();
        assert_eq!(tree.len().unwrap(), 0);
    }

    #[test]
    fn test_heed3_rkyv_utxo_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let db: Arc<dyn Database> =
            Arc::from(create_database(temp_dir.path(), DatabaseBackend::Heed3, None).unwrap());
        let tree = db.open_tree("ibd_utxos").unwrap();

        let utxo = UTXO {
            value: 100_000,
            script_pubkey: vec![0x51].into(),
            height: 42,
            is_coinbase: false,
        };
        let bytes = encode_utxo(ValueCodec::Rkyv, &utxo).unwrap();
        tree.insert(b"utxo-key", &bytes).unwrap();

        let stored = tree.get(b"utxo-key").unwrap().unwrap();
        let archived = access_utxo(&stored).unwrap();
        assert_eq!(archived.value, 100_000);
        assert_eq!(archived.height, 42);
    }

    #[test]
    fn test_heed3_concurrent_reads() {
        use std::thread;

        let temp_dir = TempDir::new().unwrap();
        let db: Arc<dyn Database> =
            Arc::from(create_database(temp_dir.path(), DatabaseBackend::Heed3, None).unwrap());
        let tree = db.open_tree("utxos").unwrap();
        for i in 0u8..20 {
            tree.insert(&[i], &[i]).unwrap();
        }
        let tree = Arc::new(tree);

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let t = Arc::clone(&tree);
                thread::spawn(move || {
                    for i in 0u8..20 {
                        assert_eq!(t.get(&[i]).unwrap(), Some(vec![i]));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_heed3_streaming_iter() {
        let temp_dir = TempDir::new().unwrap();
        let db: Arc<dyn Database> =
            Arc::from(create_database(temp_dir.path(), DatabaseBackend::Heed3, None).unwrap());
        let tree = db.open_tree("utxos").unwrap();

        const N: u32 = 5_000;
        for i in 0..N {
            let key = i.to_be_bytes();
            let val = (i as u64).to_be_bytes();
            tree.insert(&key, &val).unwrap();
        }
        assert_eq!(tree.len().unwrap(), N as usize);

        let mut count = 0usize;
        for item in tree.iter() {
            let (_k, v) = item.unwrap();
            assert_eq!(v.len(), 8);
            count += 1;
        }
        assert_eq!(count, N as usize);
    }

    /// ZC-7: verify that get_many_heed3 returns the same values as get_many_no_cache and
    /// that access_utxo succeeds on each returned slice (validates the zero-copy fast path).
    #[test]
    fn test_heed3_zero_copy_batch_load() {
        use blvm_node::storage::database::Heed3Tree;
        use blvm_node::storage::rkyv_codec::{access_utxo, utxo_from_archived};
        use blvm_protocol::types::UTXO;

        const N: usize = 500;

        let temp_dir = TempDir::new().unwrap();
        let db: Arc<dyn Database> =
            Arc::from(create_database(temp_dir.path(), DatabaseBackend::Heed3, None).unwrap());
        let tree = db.open_tree("ibd_utxos").unwrap();

        // Write N rkyv-encoded UTXOs.
        let mut written: Vec<([u8; 8], UTXO)> = Vec::with_capacity(N);
        for i in 0..N as u64 {
            let key = i.to_be_bytes();
            let utxo = UTXO {
                value: i as i64 * 1000,
                script_pubkey: vec![0x76, (i & 0xff) as u8].into(),
                height: i,
                is_coinbase: i % 100 == 0,
            };
            let bytes = encode_utxo(ValueCodec::Rkyv, &utxo).unwrap();
            tree.insert(&key, &bytes).unwrap();
            written.push((key, utxo));
        }

        // Downcast to Heed3Tree and open a single read txn for the batch.
        let heed3_tree: &Heed3Tree = tree.as_heed3_tree().expect("expected heed3 backend");
        let key_refs: Vec<&[u8]> = written.iter().map(|(k, _)| k.as_slice()).collect();
        let rtxn = heed3_tree.env().read_txn().unwrap();
        let slices = heed3_tree.get_many_heed3(&key_refs, &rtxn).unwrap();

        assert_eq!(slices.len(), N);
        for (i, (opt, (_, expected))) in slices.iter().zip(written.iter()).enumerate() {
            let bytes = opt.unwrap_or_else(|| panic!("key {i} missing"));
            let archived = access_utxo(bytes).unwrap_or_else(|_| panic!("key {i} invalid"));
            let got = utxo_from_archived(archived);
            assert_eq!(got.value, expected.value, "key {i}");
            assert_eq!(got.height, expected.height, "key {i}");
            assert_eq!(got.is_coinbase, expected.is_coinbase, "key {i}");
            assert_eq!(
                got.script_pubkey.as_ref(),
                expected.script_pubkey.as_ref(),
                "key {i}"
            );
        }
        // rtxn drops here — slices are no longer accessed (owned UTXO values already built).
        drop(rtxn);
    }

    /// Verify scan_heed3 streams all entries as mmap slices — spot-checks that the closure
    /// receives the correct key and value bytes without going through Tree::iter's Vec batches.
    #[test]
    fn test_heed3_scan_heed3() {
        use blvm_node::storage::database::Heed3Tree;
        use blvm_node::storage::rkyv_codec::{access_utxo, encode_utxo, utxo_from_archived};
        use blvm_protocol::types::UTXO;

        const N: usize = 200;
        let temp_dir = TempDir::new().unwrap();
        let db: Arc<dyn Database> =
            Arc::from(create_database(temp_dir.path(), DatabaseBackend::Heed3, None).unwrap());
        let tree = db.open_tree("ibd_utxos").unwrap();

        for i in 0..N as u64 {
            let key = i.to_be_bytes();
            let utxo = UTXO {
                value: (i * 5000) as i64,
                script_pubkey: vec![0xac, (i & 0xff) as u8].into(),
                height: i * 10,
                is_coinbase: false,
            };
            tree.insert(&key, &encode_utxo(ValueCodec::Rkyv, &utxo).unwrap())
                .unwrap();
        }

        let heed3_tree: &Heed3Tree = tree.as_heed3_tree().expect("expected heed3 backend");
        let mut seen = 0usize;
        heed3_tree
            .scan_heed3(|k, v| {
                assert_eq!(k.len(), 8);
                let i = u64::from_be_bytes(k.try_into().unwrap());
                let archived = access_utxo(v).unwrap();
                let got = utxo_from_archived(archived);
                assert_eq!(got.value, (i * 5000) as i64);
                assert_eq!(got.height, i * 10);
                seen += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, N);
    }

    #[test]
    fn test_heed3_module_dynamic_trees() {
        use blvm_node::storage::database::try_create_module_kv_database_with_preference;

        let temp_dir = TempDir::new().unwrap();
        let db = try_create_module_kv_database_with_preference(
            temp_dir.path(),
            Some(DatabaseBackend::Heed3),
        )
        .unwrap();

        let schema = db.open_tree("schema").unwrap();
        schema.insert(b"v", b"1").unwrap();
        let custom = db.open_tree("module_custom_cache").unwrap();
        custom.insert(b"k", b"v").unwrap();
        assert_eq!(custom.get(b"k").unwrap(), Some(b"v".to_vec()));
    }
}
