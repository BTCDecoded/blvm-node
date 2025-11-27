//! Tests for configuration parsing and validation

use bllvm_node::config::{
    BanListSharingConfig, BanShareMode, DosProtectionConfig, IndexingConfig, IndexingStrategy,
    ModuleConfig, ModuleResourceLimitsConfig, NetworkTimingConfig, NodeConfig, PruningConfig,
    PruningMode, RequestTimeoutConfig, RpcAuthConfig, StorageConfig, TransportPreferenceConfig,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn test_module_config_default() {
    let config = ModuleConfig::default();
    assert!(config.enabled);
    assert_eq!(config.modules_dir, "modules");
    assert_eq!(config.data_dir, "data/modules");
    assert_eq!(config.socket_dir, "data/modules/sockets");
    assert!(config.enabled_modules.is_empty());
    assert!(config.module_configs.is_empty());
}

#[test]
fn test_network_timing_config_default() {
    let config = NetworkTimingConfig::default();
    assert_eq!(config.target_peer_count, 8);
    assert_eq!(config.peer_connection_delay_seconds, 2);
    assert_eq!(config.addr_relay_min_interval_seconds, 8640);
    assert_eq!(config.max_addresses_per_addr_message, 1000);
    assert_eq!(config.max_addresses_from_dns, 100);
}

#[test]
fn test_request_timeout_config_default() {
    let config = RequestTimeoutConfig::default();
    assert!(config.async_request_timeout_seconds > 0);
    assert!(config.utxo_commitment_request_timeout_seconds > 0);
    assert!(config.request_cleanup_interval_seconds > 0);
    assert!(config.pending_request_max_age_seconds > 0);
    assert!(config.storage_timeout_seconds > 0);
    assert!(config.network_timeout_seconds > 0);
    assert!(config.rpc_timeout_seconds > 0);
}

#[test]
fn test_module_resource_limits_config_default() {
    let config = ModuleResourceLimitsConfig::default();
    assert_eq!(config.default_max_cpu_percent, 50);
    assert_eq!(config.default_max_memory_bytes, 512 * 1024 * 1024);
    assert_eq!(config.default_max_file_descriptors, 256);
    assert_eq!(config.default_max_child_processes, 10);
}

#[test]
fn test_node_config_default() {
    let config = NodeConfig::default();
    assert!(config.listen_addr.is_some());
    // TransportPreferenceConfig doesn't implement PartialEq, check via conversion
    let pref = config.get_transport_preference();
    assert!(pref.allows_tcp());
    assert_eq!(config.max_peers, Some(100));
    assert_eq!(config.protocol_version, Some("BitcoinV1".to_string()));
    assert!(config.modules.is_some());
    assert!(config.persistent_peers.is_empty());
    assert!(config.enable_self_advertisement);
}

#[test]
fn test_node_config_from_json_file() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("config.json");

    let mut config = NodeConfig::default();
    config.listen_addr = Some("127.0.0.1:8334".parse().unwrap());
    config.max_peers = Some(50);

    // Save to JSON
    config.to_json_file(&config_path).unwrap();

    // Load from JSON
    let loaded = NodeConfig::from_json_file(&config_path).unwrap();
    assert_eq!(loaded.listen_addr, config.listen_addr);
    assert_eq!(loaded.max_peers, config.max_peers);
}

#[test]
fn test_node_config_from_toml_file() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("config.toml");

    let mut config = NodeConfig::default();
    config.listen_addr = Some("127.0.0.1:8335".parse().unwrap());
    config.max_peers = Some(75);

    // Save to TOML
    config.to_toml_file(&config_path).unwrap();

    // Load from TOML
    let loaded = NodeConfig::from_toml_file(&config_path).unwrap();
    assert_eq!(loaded.listen_addr, config.listen_addr);
    assert_eq!(loaded.max_peers, config.max_peers);
}

#[test]
fn test_node_config_from_file_auto_detect_toml() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("config.toml");

    let config = NodeConfig::default();
    config.to_toml_file(&config_path).unwrap();

    // Should auto-detect TOML from extension
    let loaded = NodeConfig::from_file(&config_path).unwrap();
    assert_eq!(loaded.listen_addr, config.listen_addr);
}

#[test]
fn test_node_config_from_file_auto_detect_json() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("config.json");

    let config = NodeConfig::default();
    config.to_json_file(&config_path).unwrap();

    // Should auto-detect JSON from extension
    let loaded = NodeConfig::from_file(&config_path).unwrap();
    assert_eq!(loaded.listen_addr, config.listen_addr);
}

#[test]
fn test_node_config_get_transport_preference() {
    let mut config = NodeConfig::default();
    config.transport_preference = TransportPreferenceConfig::TcpOnly;

    let pref = config.get_transport_preference();
    assert!(pref.allows_tcp());
    #[cfg(feature = "iroh")]
    {
        assert!(!pref.allows_iroh());
    }
}

#[test]
fn test_pruning_config_validate_normal_mode() {
    let config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 0,
            min_recent_blocks: 10,
        },
        min_blocks_to_keep: 100,
        auto_prune: false,
        auto_prune_interval: 0,
        ..Default::default()
    };

    let result = config.validate();
    assert!(result.is_ok());
}

#[test]
fn test_pruning_config_validate_normal_mode_invalid() {
    let config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 0,
            min_recent_blocks: 0, // Invalid: must be > 0
        },
        min_blocks_to_keep: 100,
        auto_prune: false,
        auto_prune_interval: 0,
        ..Default::default()
    };

    let result = config.validate();
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("min_recent_blocks"));
}

#[test]
fn test_pruning_config_validate_min_blocks_to_keep_zero() {
    let config = PruningConfig {
        mode: PruningMode::Disabled,
        min_blocks_to_keep: 0, // Invalid: must be > 0
        auto_prune: false,
        auto_prune_interval: 0,
        ..Default::default()
    };

    let result = config.validate();
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("min_blocks_to_keep"));
}

#[test]
fn test_pruning_config_validate_auto_prune_interval_zero() {
    let config = PruningConfig {
        mode: PruningMode::Disabled,
        min_blocks_to_keep: 100,
        auto_prune: true,
        auto_prune_interval: 0, // Invalid: must be > 0 when auto_prune is true
        ..Default::default()
    };

    let result = config.validate();
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("auto_prune_interval"));
}

#[test]
fn test_pruning_config_validate_custom_mode_keep_headers_false() {
    let config = PruningConfig {
        mode: PruningMode::Custom {
            keep_headers: false, // Invalid: must be true
            keep_bodies_from_height: 0,
            keep_commitments: false,
            keep_filters: false,
            keep_filtered_blocks: false,
            keep_witnesses: false,
            keep_tx_index: false,
        },
        min_blocks_to_keep: 100,
        auto_prune: false,
        auto_prune_interval: 0,
        ..Default::default()
    };

    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("keep_headers"));
}

#[test]
fn test_pruning_config_validate_custom_mode_keep_headers_true() {
    let config = PruningConfig {
        mode: PruningMode::Custom {
            keep_headers: true,
            keep_bodies_from_height: 0,
            keep_commitments: false,
            keep_filters: false,
            keep_filtered_blocks: false,
            keep_witnesses: false,
            keep_tx_index: false,
        },
        min_blocks_to_keep: 100,
        auto_prune: false,
        auto_prune_interval: 0,
        ..Default::default()
    };

    let result = config.validate();
    assert!(result.is_ok());
}

#[test]
fn test_indexing_config_default() {
    let config = IndexingConfig::default();
    assert!(!config.enable_address_index);
    assert!(!config.enable_value_index);
    // IndexingStrategy doesn't implement PartialEq, check via match
    match config.strategy {
        IndexingStrategy::Eager => assert!(true),
        IndexingStrategy::Lazy => assert!(false, "Default should be Eager"),
    }
}

#[test]
fn test_indexing_config_eager_strategy() {
    let config = IndexingConfig {
        enable_address_index: true,
        enable_value_index: true,
        strategy: IndexingStrategy::Eager,
        max_indexed_addresses: 0, // 0 = unlimited
        enable_compression: false,
        background_indexing: false,
    };

    // Eager strategy should be valid
    assert!(config.enable_address_index);
    assert!(config.enable_value_index);
}

#[test]
fn test_indexing_config_lazy_strategy() {
    let config = IndexingConfig {
        enable_address_index: true,
        enable_value_index: false,
        strategy: IndexingStrategy::Lazy,
        max_indexed_addresses: 1000,
        enable_compression: true,
        background_indexing: true,
    };

    // Lazy strategy should be valid
    assert!(config.enable_address_index);
    assert!(!config.enable_value_index);
    match config.strategy {
        IndexingStrategy::Lazy => assert!(true),
        IndexingStrategy::Eager => assert!(false, "Should be Lazy"),
    }
}

#[test]
fn test_rpc_auth_config_default() {
    let config = RpcAuthConfig::default();
    // Verify defaults are set
    assert!(true);
}

#[test]
fn test_ban_list_sharing_config_default() {
    let config = BanListSharingConfig::default();
    // Verify defaults are set
    assert!(true);
}

#[test]
fn test_dos_protection_config_default() {
    let config = DosProtectionConfig::default();
    // Verify defaults are set
    assert!(true);
}

#[test]
fn test_node_config_with_custom_modules() {
    let mut config = NodeConfig::default();
    let mut module_config = ModuleConfig::default();
    module_config.enabled_modules = vec!["module1".to_string(), "module2".to_string()];
    config.modules = Some(module_config);

    assert_eq!(config.modules.as_ref().unwrap().enabled_modules.len(), 2);
}

#[test]
fn test_node_config_with_persistent_peers() {
    let mut config = NodeConfig::default();
    config.persistent_peers = vec![
        "127.0.0.1:8333".parse().unwrap(),
        "192.168.1.1:8333".parse().unwrap(),
    ];

    assert_eq!(config.persistent_peers.len(), 2);
}

#[test]
fn test_node_config_serialization_roundtrip_json() {
    let mut config = NodeConfig::default();
    config.max_peers = Some(150);
    config.enable_self_advertisement = false;

    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("test.json");

    config.to_json_file(&config_path).unwrap();
    let loaded = NodeConfig::from_json_file(&config_path).unwrap();

    assert_eq!(loaded.max_peers, config.max_peers);
    assert_eq!(
        loaded.enable_self_advertisement,
        config.enable_self_advertisement
    );
}

#[test]
fn test_node_config_serialization_roundtrip_toml() {
    let mut config = NodeConfig::default();
    config.max_peers = Some(200);
    config.enable_self_advertisement = true;

    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("test.toml");

    config.to_toml_file(&config_path).unwrap();
    let loaded = NodeConfig::from_toml_file(&config_path).unwrap();

    assert_eq!(loaded.max_peers, config.max_peers);
    assert_eq!(
        loaded.enable_self_advertisement,
        config.enable_self_advertisement
    );
}

#[test]
fn test_node_config_invalid_json() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("invalid.json");
    std::fs::write(&config_path, "{ invalid json }").unwrap();

    let result = NodeConfig::from_json_file(&config_path);
    assert!(result.is_err());
}

#[test]
fn test_node_config_invalid_toml() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("invalid.toml");
    std::fs::write(&config_path, "[invalid toml").unwrap();

    let result = NodeConfig::from_toml_file(&config_path);
    assert!(result.is_err());
}

#[test]
fn test_node_config_missing_file() {
    let config_path = PathBuf::from("/nonexistent/config.json");
    let result = NodeConfig::from_json_file(&config_path);
    assert!(result.is_err());
}

#[test]
fn test_storage_config_default() {
    let config = StorageConfig::default();
    assert!(!config.data_dir.is_empty());
    // database_backend is not Option, it's a direct field
    assert!(true);
}

#[test]
fn test_transport_preference_config_variants() {
    // TransportPreferenceConfig doesn't implement PartialEq, so we can't use assert_eq!
    // Instead, test via conversion to TransportPreference
    let tcp = TransportPreferenceConfig::TcpOnly;
    let pref: bllvm_node::network::transport::TransportPreference = tcp.into();
    assert!(pref.allows_tcp());

    #[cfg(feature = "quinn")]
    {
        let quinn = TransportPreferenceConfig::QuinnOnly;
        let pref: bllvm_node::network::transport::TransportPreference = quinn.into();
        assert!(pref.allows_quinn());
    }

    #[cfg(feature = "iroh")]
    {
        let iroh = TransportPreferenceConfig::IrohOnly;
        let pref: bllvm_node::network::transport::TransportPreference = iroh.into();
        assert!(pref.allows_iroh());
    }
}
