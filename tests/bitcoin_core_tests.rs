//! Bitcoin Core compatibility tests
//!
//! Tests for Bitcoin Core detection, format parsing, and block file reading.

#[cfg(feature = "rocksdb")]
mod bitcoin_core_tests {
    use blvm_node::storage::bitcoin_core_detection::{BitcoinCoreDetection, BitcoinCoreNetwork};
    use blvm_node::storage::bitcoin_core_format::{
        convert_key, get_key_prefix, parse_block_index, parse_coin,
    };
    use blvm_node::storage::bitcoin_core_blocks::BitcoinCoreBlockReader;
    use tempfile::TempDir;
    use std::fs::{File, create_dir_all};
    use std::io::Write;

    #[test]
    fn test_bitcoin_core_detection_paths() {
        // Test that detection doesn't crash on non-existent paths
        let result = BitcoinCoreDetection::detect_data_dir(BitcoinCoreNetwork::Mainnet);
        // Should return Ok(None) if not found, not an error
        assert!(result.is_ok());
    }

    #[test]
    fn test_bitcoin_core_network_detection() {
        let temp_dir = TempDir::new().unwrap();
        
        // Test mainnet detection
        let mainnet_path = temp_dir.path().join(".bitcoin");
        create_dir_all(&mainnet_path).unwrap();
        let detected = BitcoinCoreDetection::detect_network(&mainnet_path);
        assert_eq!(detected, Some(BitcoinCoreNetwork::Mainnet));

        // Test testnet detection
        let testnet_path = temp_dir.path().join("testnet3");
        create_dir_all(&testnet_path).unwrap();
        let detected = BitcoinCoreDetection::detect_network(&testnet_path);
        assert_eq!(detected, Some(BitcoinCoreNetwork::Testnet));
    }

    // Note: read_varint is private, so we test it indirectly through parse_coin
    // VarInt parsing is tested as part of coin parsing tests

    #[test]
    fn test_key_conversion() {
        // Test coin key conversion (prefix 'c')
        let coin_key = b"c\x01\x02\x03";
        let converted = convert_key(coin_key).unwrap();
        assert_eq!(converted, b"\x01\x02\x03");

        // Test block index key conversion (prefix 'b')
        let block_key = b"b\x04\x05\x06";
        let converted = convert_key(block_key).unwrap();
        assert_eq!(converted, b"\x04\x05\x06");
    }

    #[test]
    fn test_get_key_prefix() {
        let coin_key = b"c\x01\x02\x03";
        assert_eq!(get_key_prefix(coin_key), Some(b'c'));

        let block_key = b"b\x04\x05\x06";
        assert_eq!(get_key_prefix(block_key), Some(b'b'));

        let empty_key = b"";
        assert_eq!(get_key_prefix(empty_key), None);
    }

    #[test]
    fn test_parse_coin_simple() {
        // Create a simple coin format
        // Format: VarInt(code) + script + amount(8) + height(4) + coinbase(1)
        let mut data = Vec::new();
        
        // VarInt code (0 = uncompressed, script length follows)
        data.push(0x00);
        // Script length VarInt (5 bytes)
        data.push(0x05);
        // Script (5 bytes)
        data.extend_from_slice(b"script");
        // Amount (8 bytes, little-endian)
        data.extend_from_slice(&1000000u64.to_le_bytes());
        // Height (4 bytes, little-endian)
        data.extend_from_slice(&100u32.to_le_bytes());
        // Coinbase flag (1 byte)
        data.push(0x01);

        let coin = parse_coin(&data).unwrap();
        assert_eq!(coin.amount, 1000000);
        assert_eq!(coin.height, 100);
        assert_eq!(coin.is_coinbase, true);
        assert_eq!(coin.script, b"script");
    }

    #[test]
    fn test_parse_block_index() {
        // Create a minimal block index (80+ bytes)
        let mut data = vec![0u8; 80];
        
        // Height (4 bytes)
        data[0..4].copy_from_slice(&100u32.to_le_bytes());
        // Status (4 bytes)
        data[4..8].copy_from_slice(&1u32.to_le_bytes());
        // n_tx (4 bytes)
        data[8..12].copy_from_slice(&10u32.to_le_bytes());
        // n_file (4 bytes)
        data[12..16].copy_from_slice(&0u32.to_le_bytes());
        // n_data_pos (4 bytes)
        data[16..20].copy_from_slice(&0u32.to_le_bytes());
        // n_undo_pos (4 bytes)
        data[20..24].copy_from_slice(&0u32.to_le_bytes());
        // n_version (4 bytes)
        data[24..28].copy_from_slice(&1u32.to_le_bytes());
        // hash_prev (32 bytes)
        data[28..60].copy_from_slice(&[0u8; 32]);
        // hash_merkle_root (32 bytes)
        data[60..92].copy_from_slice(&[1u8; 32]);
        // n_time (4 bytes)
        data[92..96].copy_from_slice(&1234567890u32.to_le_bytes());
        // n_bits (4 bytes)
        data[96..100].copy_from_slice(&0x1d00ffffu32.to_le_bytes());
        // n_nonce (4 bytes)
        data[100..104].copy_from_slice(&12345u32.to_le_bytes());

        let block_index = parse_block_index(&data).unwrap();
        assert_eq!(block_index.height, 100);
        assert_eq!(block_index.n_tx, 10);
        assert_eq!(block_index.n_time, 1234567890);
        assert_eq!(block_index.n_bits, 0x1d00ffff);
        assert_eq!(block_index.n_nonce, 12345);
    }

    #[test]
    fn test_block_file_reader_with_cache() {
        let temp_dir = TempDir::new().unwrap();
        let blocks_dir = temp_dir.path().join("blocks");
        create_dir_all(&blocks_dir).unwrap();

        // Create a test block file
        let file_path = blocks_dir.join("blk00000.dat");
        let mut file = File::create(&file_path).unwrap();

        // Write magic bytes
        file.write_all(&[0xF9, 0xBE, 0xB4, 0xD9]).unwrap();
        // Write block size (80 bytes for header)
        file.write_all(&80u32.to_le_bytes()).unwrap();
        // Write minimal block header (80 bytes)
        file.write_all(&vec![0u8; 80]).unwrap();

        // Test reader creation with cache
        let cache_dir = temp_dir.path().join("cache");
        let reader = BitcoinCoreBlockReader::new_with_cache(
            &blocks_dir,
            BitcoinCoreNetwork::Mainnet,
            Some(&cache_dir),
        );
        assert!(reader.is_ok());

        let reader = reader.unwrap();
        
        // First access should build index
        let count = reader.block_count().unwrap();
        assert!(count > 0);

        // Second access should load from cache (faster)
        let count2 = reader.block_count().unwrap();
        assert_eq!(count, count2);

        // Verify cache file exists
        let cache_file = cache_dir.join("block_index_mainnet.bin");
        assert!(cache_file.exists());
    }

    #[test]
    fn test_block_file_reader_index_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let blocks_dir = temp_dir.path().join("blocks");
        create_dir_all(&blocks_dir).unwrap();

        // Create test block file
        let file_path = blocks_dir.join("blk00000.dat");
        let mut file = File::create(&file_path).unwrap();
        file.write_all(&[0xF9, 0xBE, 0xB4, 0xD9]).unwrap();
        file.write_all(&80u32.to_le_bytes()).unwrap();
        file.write_all(&vec![0u8; 80]).unwrap();

        let cache_dir = temp_dir.path().join("cache");

        // First reader - builds index
        let reader1 = BitcoinCoreBlockReader::new_with_cache(
            &blocks_dir,
            BitcoinCoreNetwork::Mainnet,
            Some(&cache_dir),
        ).unwrap();
        let count1 = reader1.block_count().unwrap();

        // Second reader - should load from cache
        let reader2 = BitcoinCoreBlockReader::new_with_cache(
            &blocks_dir,
            BitcoinCoreNetwork::Mainnet,
            Some(&cache_dir),
        ).unwrap();
        let count2 = reader2.block_count().unwrap();

        assert_eq!(count1, count2);
    }
}

#[cfg(not(feature = "rocksdb"))]
mod bitcoin_core_tests {
    #[test]
    fn test_bitcoin_core_not_available() {
        // Tests that Bitcoin Core features are not available when rocksdb feature is disabled
        use blvm_node::storage::bitcoin_core_storage::BitcoinCoreStorage;
        use blvm_node::storage::bitcoin_core_detection::BitcoinCoreNetwork;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let result = BitcoinCoreStorage::open_bitcoin_core_database(
            temp_dir.path(),
            BitcoinCoreNetwork::Mainnet,
        );
        assert!(result.is_err());
    }
}

