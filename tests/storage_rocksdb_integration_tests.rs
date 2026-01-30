//! Storage integration tests with RocksDB backend
//!
//! Tests that verify RocksDB backend works correctly with all storage components.

#[cfg(feature = "rocksdb")]
mod rocksdb_integration_tests {
    use blvm_node::storage::*;
    use blvm_node::storage::database::{create_database, DatabaseBackend};
    use blvm_protocol::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn test_storage_with_rocksdb_backend() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::with_backend(
            temp_dir.path(),
            DatabaseBackend::RocksDB,
        ).unwrap();

        // Test that storage components are accessible
        let _blocks = storage.blocks();
        let _utxos = storage.utxos();
        let _chain = storage.chain();
        let _transactions = storage.transactions();
    }

    #[test]
    fn test_blockstore_with_rocksdb() {
        let temp_dir = TempDir::new().unwrap();
        let db = Arc::from(
            create_database(temp_dir.path(), DatabaseBackend::RocksDB).unwrap()
        );
        let blockstore = blockstore::BlockStore::new(db).unwrap();

        // Create a test block
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1234567890,
                bits: 0x1d00ffff,
                nonce: 0,
            },
            transactions: vec![].into_boxed_slice(),
        };

        // Store the block
        blockstore.store_block(&block).unwrap();

        // Verify block count
        assert_eq!(blockstore.block_count().unwrap(), 1);

        // Retrieve the block
        let block_hash = blockstore.get_block_hash(&block);
        let retrieved = blockstore.get_block(&block_hash).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().header.version, block.header.version);
    }

    #[test]
    fn test_utxostore_with_rocksdb() {
        let temp_dir = TempDir::new().unwrap();
        let db = Arc::from(
            create_database(temp_dir.path(), DatabaseBackend::RocksDB).unwrap()
        );
        let utxostore = utxostore::UtxoStore::new(db).unwrap();

        let outpoint = OutPoint {
            hash: [1u8; 32],
            index: 0,
        };

        let utxo = UTXO {
            value: 5000000000,
            script_pubkey: vec![0x76, 0xa9, 0x14],
            height: 0,
            is_coinbase: false,
        };

        // Add UTXO
        utxostore.add_utxo(&outpoint, &utxo).unwrap();

        // Verify UTXO exists
        assert!(utxostore.has_utxo(&outpoint).unwrap());

        // Get UTXO
        let retrieved_utxo = utxostore.get_utxo(&outpoint).unwrap().unwrap();
        assert_eq!(retrieved_utxo.value, utxo.value);
    }

    #[test]
    fn test_chainstate_with_rocksdb() {
        let temp_dir = TempDir::new().unwrap();
        let db = Arc::from(
            create_database(temp_dir.path(), DatabaseBackend::RocksDB).unwrap()
        );
        let chainstate = chainstate::ChainState::new(db).unwrap();

        let genesis_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 2083236893,
        };

        // Initialize chain state
        chainstate.initialize(&genesis_header).unwrap();

        // Verify chain info
        let chain_info = chainstate.load_chain_info().unwrap();
        assert!(chain_info.is_some());
    }

    #[test]
    fn test_txindex_with_rocksdb() {
        let temp_dir = TempDir::new().unwrap();
        let db = Arc::from(
            create_database(temp_dir.path(), DatabaseBackend::RocksDB).unwrap()
        );
        let txindex = txindex::TxIndex::new(db).unwrap();

        // Create a test transaction
        let tx = Transaction {
            version: 1,
            inputs: vec![].into(),
            outputs: vec![TransactionOutput {
                value: 1000,
                script_pubkey: vec![0x76, 0xa9, 0x14],
            }].into(),
            lock_time: 0,
        };

        let block_hash = [2u8; 32];
        let tx_hash = blvm_protocol::block::calculate_tx_id(&tx);

        // Index transaction
        txindex.index_transaction(&tx, &block_hash, 100, 0).unwrap();

        // Verify transaction can be retrieved
        let retrieved = txindex.get_transaction(&tx_hash).unwrap();
        assert!(retrieved.is_some());
    }

    #[test]
    fn test_storage_backend_interchangeability() {
        // Test that storage works the same with different backends
        let temp_dir1 = TempDir::new().unwrap();
        let temp_dir2 = TempDir::new().unwrap();

        // Test with redb
        let storage1 = Storage::with_backend(
            temp_dir1.path(),
            DatabaseBackend::Redb,
        ).unwrap();

        // Test with RocksDB
        let storage2 = Storage::with_backend(
            temp_dir2.path(),
            DatabaseBackend::RocksDB,
        ).unwrap();

        // Both should work the same way
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1234567890,
                bits: 0x1d00ffff,
                nonce: 0,
            },
            transactions: vec![].into_boxed_slice(),
        };

        storage1.blocks().store_block(&block).unwrap();
        storage2.blocks().store_block(&block).unwrap();

        assert_eq!(storage1.blocks().block_count().unwrap(), 1);
        assert_eq!(storage2.blocks().block_count().unwrap(), 1);
    }
}

#[cfg(not(feature = "rocksdb"))]
mod rocksdb_integration_tests {
    #[test]
    fn test_rocksdb_not_available() {
        use blvm_node::storage::Storage;
        use blvm_node::storage::database::DatabaseBackend;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let result = Storage::with_backend(
            temp_dir.path(),
            DatabaseBackend::RocksDB,
        );
        assert!(result.is_err());
    }
}

