//! Storage integration tests with heed3 / LMDB backend.

#[cfg(feature = "heed3")]
mod heed3_integration_tests {
    use blvm_node::storage::database::{DatabaseBackend, create_database};
    use blvm_node::storage::utxo_value_codec::ValueCodec;
    use blvm_node::storage::*;
    use blvm_protocol::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn test_storage_with_heed3_backend() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::with_backend(temp_dir.path(), DatabaseBackend::Heed3).unwrap();
        assert_eq!(storage.utxo_value_codec(), ValueCodec::Rkyv);
        let _blocks = storage.blocks();
        let _utxos = storage.utxos();
        let _chain = storage.chain();
    }

    #[test]
    fn test_utxostore_with_heed3_rkyv() {
        let temp_dir = TempDir::new().unwrap();
        let db = Arc::from(create_database(temp_dir.path(), DatabaseBackend::Heed3, None).unwrap());
        let utxostore = utxostore::UtxoStore::new(db).unwrap();

        let outpoint = OutPoint {
            hash: [2u8; 32],
            index: 1,
        };
        let utxo = UTXO {
            value: 1000,
            script_pubkey: vec![0x51].into(),
            height: 10,
            is_coinbase: false,
        };

        utxostore.add_utxo(&outpoint, &utxo).unwrap();
        let got = utxostore.get_utxo(&outpoint).unwrap().unwrap();
        assert_eq!(got, utxo);
    }

    #[test]
    fn test_blockstore_with_heed3() {
        let temp_dir = TempDir::new().unwrap();
        let db = Arc::from(create_database(temp_dir.path(), DatabaseBackend::Heed3, None).unwrap());
        let blockstore = blockstore::BlockStore::new(db).unwrap();

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

        blockstore.store_block(&block).unwrap();
        assert_eq!(blockstore.block_count().unwrap(), 1);
    }
}
