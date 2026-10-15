//! Bitcoin Core compatibility tests
//!
//! Tests for Bitcoin Core detection, format parsing, and block file reading.

#[cfg(feature = "rocksdb")]
mod bitcoin_core_tests {
    use blvm_node::config::StorageConfig;
    use blvm_node::storage::bitcoin_core_blocks::BitcoinCoreBlockReader;
    use blvm_node::storage::bitcoin_core_format::{
        convert_key, get_key_prefix, parse_block_index, parse_coin,
    };
    use blvm_node::storage::bitcoin_core_storage::BitcoinCoreStorage;
    use blvm_node::storage::bitcoin_detection::{BitcoinCoreDetection, CoreDataNetwork};
    use std::fs::{File, create_dir_all};
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_bitcoin_core_detection_paths() {
        // Test that detection doesn't crash on non-existent paths
        let result = BitcoinCoreDetection::detect_data_dir(CoreDataNetwork::Mainnet);
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
        assert_eq!(detected, Some(CoreDataNetwork::Mainnet));

        // Test testnet detection
        let testnet_path = temp_dir.path().join("testnet3");
        create_dir_all(&testnet_path).unwrap();
        let detected = BitcoinCoreDetection::detect_network(&testnet_path);
        assert_eq!(detected, Some(CoreDataNetwork::Testnet));
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
        // Script length VarInt (6 bytes)
        data.push(0x06);
        // Script (6 bytes)
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
        assert!(coin.is_coinbase);
        assert_eq!(coin.script, b"script");
    }

    #[test]
    fn test_parse_block_index() {
        // CBlockIndex layout used by parse_block_index needs 104+ bytes
        let mut data = vec![0u8; 104];

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
        file.write_all(&[0u8; 80]).unwrap();

        // Test reader creation with cache
        let cache_dir = temp_dir.path().join("cache");
        let reader = BitcoinCoreBlockReader::new_with_cache(
            &blocks_dir,
            CoreDataNetwork::Mainnet,
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
        file.write_all(&[0u8; 80]).unwrap();

        let cache_dir = temp_dir.path().join("cache");

        // First reader - builds index
        let reader1 = BitcoinCoreBlockReader::new_with_cache(
            &blocks_dir,
            CoreDataNetwork::Mainnet,
            Some(&cache_dir),
        )
        .unwrap();
        let count1 = reader1.block_count().unwrap();

        // Second reader - should load from cache
        let reader2 = BitcoinCoreBlockReader::new_with_cache(
            &blocks_dir,
            CoreDataNetwork::Mainnet,
            Some(&cache_dir),
        )
        .unwrap();
        let count2 = reader2.block_count().unwrap();

        assert_eq!(count1, count2);
    }

    #[test]
    fn test_ensure_not_locked_rejects_live_pid_file() {
        let temp_dir = TempDir::new().unwrap();
        let pid = std::process::id();
        std::fs::write(temp_dir.path().join("bitcoind.pid"), pid.to_string()).unwrap();
        assert!(BitcoinCoreStorage::ensure_not_locked(temp_dir.path()).is_err());
    }

    #[test]
    fn test_ensure_not_locked_ignores_stale_pid_and_lock_files() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("bitcoind.pid"), b"99999999").unwrap();
        std::fs::create_dir_all(temp_dir.path().join("chainstate")).unwrap();
        std::fs::write(temp_dir.path().join("chainstate/LOCK"), b"").unwrap();
        assert!(BitcoinCoreStorage::ensure_not_locked(temp_dir.path()).is_ok());
    }

    #[test]
    fn test_storage_config_auto_migrate_env_override() {
        let cfg = StorageConfig {
            auto_migrate_core: true,
            ..Default::default()
        };
        unsafe {
            std::env::set_var("BLVM_NO_AUTO_MIGRATE_CORE", "1");
        }
        assert!(!cfg.auto_migrate_core_effective());
        unsafe {
            std::env::remove_var("BLVM_NO_AUTO_MIGRATE_CORE");
        }

        unsafe {
            std::env::set_var("BLVM_AUTO_MIGRATE_CORE", "0");
        }
        assert!(!cfg.auto_migrate_core_effective());
        unsafe {
            std::env::remove_var("BLVM_AUTO_MIGRATE_CORE");
        }
    }

    #[test]
    fn test_storage_config_reuse_core_block_files_default() {
        let cfg = StorageConfig::default();
        assert!(cfg.reuse_core_block_files);
        assert!(cfg.reuse_core_block_files_effective());
    }

    #[test]
    fn test_storage_config_reuse_core_block_files_env() {
        let cfg = StorageConfig {
            reuse_core_block_files: false,
            ..Default::default()
        };
        unsafe {
            std::env::set_var("BLVM_REUSE_CORE_BLOCK_FILES", "1");
        }
        assert!(cfg.reuse_core_block_files_effective());
        unsafe {
            std::env::remove_var("BLVM_REUSE_CORE_BLOCK_FILES");
        }
    }

    #[test]
    fn test_is_core_layout_requires_chainstate_and_blocks() {
        let temp = TempDir::new().unwrap();
        assert!(!BitcoinCoreDetection::is_core_layout_at(temp.path()));
        create_dir_all(temp.path().join("blocks")).unwrap();
        assert!(!BitcoinCoreDetection::is_core_layout_at(temp.path()));
    }

    fn create_core_layout(dir: &std::path::Path) {
        let chainstate = dir.join("chainstate");
        create_dir_all(&chainstate).unwrap();
        std::fs::write(chainstate.join("CURRENT"), "MANIFEST-000001\n").unwrap();
        std::fs::write(chainstate.join("MANIFEST-000001"), b"").unwrap();
        create_dir_all(dir.join("blocks")).unwrap();
    }

    #[test]
    fn test_detect_network_custom_signet_returns_none() {
        let temp = TempDir::new().unwrap();
        let custom_signet = temp.path().join("signet_abc123def456");
        create_dir_all(&custom_signet).unwrap();

        let detected = BitcoinCoreDetection::detect_network(&custom_signet);
        assert_eq!(
            detected, None,
            "Custom signet should not silently fall back to mainnet"
        );
    }

    #[test]
    fn test_detect_network_strict_custom_signet_errors() {
        let temp = TempDir::new().unwrap();
        let custom_signet = temp.path().join("signet_0f9188f13cb7b2c71f2a335e3a4fc328");
        create_dir_all(&custom_signet).unwrap();

        let result = BitcoinCoreDetection::detect_network_strict(&custom_signet);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Custom signet directory detected"),
            "Error should mention custom signet: {err_msg}"
        );
    }

    #[test]
    fn test_is_bitcoin_core_dir_respects_network_argument() {
        let temp = TempDir::new().unwrap();
        let base = temp.path();
        let testnet4_dir = base.join("testnet4");
        create_core_layout(&testnet4_dir);

        assert!(
            BitcoinCoreDetection::is_bitcoin_core_dir(base, CoreDataNetwork::Testnet4),
            "Should detect testnet4 layout in testnet4/ subdir"
        );
        assert!(
            !BitcoinCoreDetection::is_bitcoin_core_dir(base, CoreDataNetwork::Mainnet),
            "Should NOT detect mainnet when only testnet4/ exists"
        );
        assert!(
            !BitcoinCoreDetection::is_bitcoin_core_dir(base, CoreDataNetwork::Testnet),
            "Should NOT detect testnet3 when only testnet4/ exists"
        );
    }

    #[test]
    fn test_is_bitcoin_core_dir_mainnet_at_root() {
        let temp = TempDir::new().unwrap();
        let base = temp.path();
        create_core_layout(base);

        assert!(
            BitcoinCoreDetection::is_bitcoin_core_dir(base, CoreDataNetwork::Mainnet),
            "Should detect mainnet layout at root"
        );
        assert!(
            !BitcoinCoreDetection::is_bitcoin_core_dir(base, CoreDataNetwork::Testnet4),
            "Should NOT detect testnet4 when data is at root"
        );
    }

    #[test]
    fn test_testnet4_datadir_integration() {
        let temp = TempDir::new().unwrap();
        let base = temp.path().join(".bitcoin");
        create_dir_all(&base).unwrap();

        let testnet4_dir = base.join("testnet4");
        create_core_layout(&testnet4_dir);

        assert_eq!(
            BitcoinCoreDetection::detect_network(&testnet4_dir),
            Some(CoreDataNetwork::Testnet4),
            "detect_network should identify testnet4 subdirectory"
        );

        assert!(
            BitcoinCoreDetection::is_bitcoin_core_dir(&base, CoreDataNetwork::Testnet4),
            "is_bitcoin_core_dir should find testnet4 data under base"
        );

        assert!(
            BitcoinCoreDetection::is_core_layout_at(&testnet4_dir),
            "is_core_layout_at should validate testnet4 directory structure"
        );

        assert!(
            BitcoinCoreDetection::verify_database(&testnet4_dir).is_ok(),
            "verify_database should pass for valid testnet4 layout"
        );
    }

    #[test]
    fn test_all_networks_directory_layout() {
        let temp = TempDir::new().unwrap();
        let base = temp.path();

        create_core_layout(base);
        create_core_layout(&base.join("testnet3"));
        create_core_layout(&base.join("testnet4"));
        create_core_layout(&base.join("signet"));
        create_core_layout(&base.join("regtest"));

        assert!(BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Mainnet
        ));
        assert!(BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Testnet
        ));
        assert!(BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Testnet4
        ));
        assert!(BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Signet
        ));
        assert!(BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Regtest
        ));
    }

    // The reader-level custom signet test lives in src/storage/core_reuse_tests.rs:
    // `Storage::open_core_block_reader_for_store` is pub(crate).

    #[test]
    fn test_standard_signet_core_dir_detection_works() {
        // Verify that standard signet directories are detected correctly
        // (as opposed to custom signet_<hash> which returns None)
        let temp = TempDir::new().unwrap();

        // Create a standard signet directory
        let signet_dir = temp.path().join("signet");
        create_dir_all(&signet_dir).unwrap();

        let detected = BitcoinCoreDetection::detect_network(&signet_dir);
        assert_eq!(
            detected,
            Some(CoreDataNetwork::Signet),
            "Standard signet directory should be detected as Signet"
        );

        // Contrast with custom signet
        let custom_signet = temp.path().join("signet_abc123");
        create_dir_all(&custom_signet).unwrap();

        let detected_custom = BitcoinCoreDetection::detect_network(&custom_signet);
        assert_eq!(
            detected_custom, None,
            "Custom signet_<hash> directory should return None, not Mainnet"
        );
    }
}

#[cfg(not(feature = "rocksdb"))]
mod bitcoin_core_tests {
    #[test]
    fn test_bitcoin_core_not_available() {
        // Tests that Bitcoin Core features are not available when rocksdb feature is disabled
        use blvm_node::storage::bitcoin_core_storage::BitcoinCoreStorage;
        use blvm_node::storage::bitcoin_detection::CoreDataNetwork;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let result = BitcoinCoreStorage::open_bitcoin_core_database(
            temp_dir.path(),
            CoreDataNetwork::Mainnet,
        );
        assert!(result.is_err());
    }
}
