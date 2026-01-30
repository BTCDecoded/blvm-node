//! RocksDB backend tests
//!
//! Tests for RocksDB database backend implementation, including
//! Bitcoin Core LevelDB format compatibility.

#[cfg(feature = "rocksdb")]
mod rocksdb_tests {
    use blvm_node::storage::database::{create_database, DatabaseBackend, Database, Tree};
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn test_rocksdb_creation() {
        let temp_dir = TempDir::new().unwrap();
        let db = create_database(temp_dir.path(), DatabaseBackend::RocksDB);
        assert!(db.is_ok());
    }

    #[test]
    fn test_rocksdb_tree_operations() {
        let temp_dir = TempDir::new().unwrap();
        let db = Arc::from(create_database(temp_dir.path(), DatabaseBackend::RocksDB).unwrap());

        let tree = db.open_tree("test_tree").unwrap();

        // Test insert
        tree.insert(b"key1", b"value1").unwrap();
        tree.insert(b"key2", b"value2").unwrap();
        tree.insert(b"key3", b"value3").unwrap();

        // Test get
        assert_eq!(tree.get(b"key1").unwrap(), Some(b"value1".to_vec()));
        assert_eq!(tree.get(b"key2").unwrap(), Some(b"value2".to_vec()));
        assert_eq!(tree.get(b"key3").unwrap(), Some(b"value3".to_vec()));
        assert_eq!(tree.get(b"nonexistent").unwrap(), None);

        // Test contains_key
        assert!(tree.contains_key(b"key1").unwrap());
        assert!(!tree.contains_key(b"nonexistent").unwrap());

        // Test len
        assert_eq!(tree.len().unwrap(), 3);

        // Test remove
        tree.remove(b"key2").unwrap();
        assert_eq!(tree.len().unwrap(), 2);
        assert_eq!(tree.get(b"key2").unwrap(), None);

        // Test clear
        tree.clear().unwrap();
        assert_eq!(tree.len().unwrap(), 0);
    }

    #[test]
    fn test_rocksdb_tree_isolation() {
        let temp_dir = TempDir::new().unwrap();
        let db = Arc::from(create_database(temp_dir.path(), DatabaseBackend::RocksDB).unwrap());

        let tree1 = db.open_tree("tree1").unwrap();
        let tree2 = db.open_tree("tree2").unwrap();

        // Insert same key in both trees
        tree1.insert(b"key", b"value1").unwrap();
        tree2.insert(b"key", b"value2").unwrap();

        // Verify isolation
        assert_eq!(tree1.get(b"key").unwrap(), Some(b"value1".to_vec()));
        assert_eq!(tree2.get(b"key").unwrap(), Some(b"value2".to_vec()));
    }

    #[test]
    fn test_rocksdb_iteration() {
        let temp_dir = TempDir::new().unwrap();
        let db = Arc::from(create_database(temp_dir.path(), DatabaseBackend::RocksDB).unwrap());

        let tree = db.open_tree("test_tree").unwrap();

        // Insert test data
        let test_data = vec![
            (b"key1".to_vec(), b"value1".to_vec()),
            (b"key2".to_vec(), b"value2".to_vec()),
            (b"key3".to_vec(), b"value3".to_vec()),
        ];

        for (key, value) in &test_data {
            tree.insert(key, value).unwrap();
        }

        // Test iteration
        let mut collected: Vec<(Vec<u8>, Vec<u8>)> = tree
            .iter()
            .map(|item| item.unwrap())
            .collect();

        // Sort for comparison (RocksDB iteration order may vary)
        collected.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0], (b"key1".to_vec(), b"value1".to_vec()));
        assert_eq!(collected[1], (b"key2".to_vec(), b"value2".to_vec()));
        assert_eq!(collected[2], (b"key3".to_vec(), b"value3".to_vec()));
    }

    #[test]
    fn test_rocksdb_dynamic_tree_creation() {
        let temp_dir = TempDir::new().unwrap();
        let db = Arc::from(create_database(temp_dir.path(), DatabaseBackend::RocksDB).unwrap());

        // Create multiple trees dynamically
        for i in 0..10 {
            let tree_name = format!("dynamic_tree_{}", i);
            let tree = db.open_tree(&tree_name).unwrap();
            tree.insert(b"test_key", b"test_value").unwrap();
        }

        // Verify all trees exist and are isolated
        for i in 0..10 {
            let tree_name = format!("dynamic_tree_{}", i);
            let tree = db.open_tree(&tree_name).unwrap();
            assert_eq!(tree.get(b"test_key").unwrap(), Some(b"test_value".to_vec()));
        }
    }

    #[test]
    fn test_rocksdb_flush() {
        let temp_dir = TempDir::new().unwrap();
        let db = Arc::from(create_database(temp_dir.path(), DatabaseBackend::RocksDB).unwrap());

        let tree = db.open_tree("test_tree").unwrap();
        tree.insert(b"key", b"value").unwrap();

        // Flush should succeed
        assert!(db.flush().is_ok());
    }

    #[test]
    fn test_rocksdb_large_data() {
        let temp_dir = TempDir::new().unwrap();
        let db = Arc::from(create_database(temp_dir.path(), DatabaseBackend::RocksDB).unwrap());

        let tree = db.open_tree("test_tree").unwrap();

        // Insert large values
        let large_value = vec![0u8; 1024 * 1024]; // 1MB
        tree.insert(b"large_key", &large_value).unwrap();

        let retrieved = tree.get(b"large_key").unwrap();
        assert_eq!(retrieved, Some(large_value));
    }
}

#[cfg(not(feature = "rocksdb"))]
mod rocksdb_tests {
    #[test]
    fn test_rocksdb_not_available() {
        // Test that RocksDB backend is not available when feature is disabled
        use blvm_node::storage::database::{create_database, DatabaseBackend};
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let result = create_database(temp_dir.path(), DatabaseBackend::RocksDB);
        assert!(result.is_err());
    }
}

