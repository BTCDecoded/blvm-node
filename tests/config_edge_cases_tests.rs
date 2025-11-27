//! Configuration edge case tests
//!
//! Tests for complex configuration scenarios, invalid configurations,
//! and edge cases in configuration parsing and validation.

use bllvm_node::config::*;
use std::collections::HashMap;

#[test]
fn test_config_with_all_transport_preferences() {
    let mut preferences = vec![TransportPreferenceConfig::TcpOnly];

    #[cfg(feature = "quinn")]
    preferences.push(TransportPreferenceConfig::QuinnOnly);

    #[cfg(feature = "iroh")]
    {
        preferences.push(TransportPreferenceConfig::IrohOnly);
        preferences.push(TransportPreferenceConfig::Hybrid);
    }

    #[cfg(all(feature = "quinn", feature = "iroh"))]
    preferences.push(TransportPreferenceConfig::All);

    for preference in preferences {
        let mut config = NodeConfig::default();
        config.transport_preference = preference;

        // Should serialize/deserialize
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: NodeConfig = serde_json::from_str(&json).unwrap();

        // TransportPreferenceConfig doesn't implement PartialEq, so compare via conversion
        let deserialized_pref: bllvm_node::network::transport::TransportPreference =
            deserialized.transport_preference.into();
        let expected_pref: bllvm_node::network::transport::TransportPreference = preference.into();
        assert_eq!(deserialized_pref, expected_pref);
    }
}

#[test]
fn test_config_with_extreme_values() {
    let mut config = NodeConfig::default();

    // Set extreme but valid values
    let mut network_timing = NetworkTimingConfig::default();
    network_timing.target_peer_count = 1000; // Very high
    network_timing.peer_connection_delay_seconds = 3600; // 1 hour
    config.network_timing = Some(network_timing);

    // StorageConfig doesn't have max_disk_size_gb, test with other fields
    let storage = StorageConfig::default();
    config.storage = Some(storage);

    // Should serialize/deserialize
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: NodeConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(
        deserialized
            .network_timing
            .as_ref()
            .unwrap()
            .target_peer_count,
        1000
    );
    assert!(deserialized.storage.is_some());
}

#[test]
fn test_config_with_minimal_values() {
    let mut config = NodeConfig::default();

    // Set minimal values
    let mut network_timing = NetworkTimingConfig::default();
    network_timing.target_peer_count = 1;
    network_timing.peer_connection_delay_seconds = 0;
    config.network_timing = Some(network_timing);

    let storage = StorageConfig::default();
    config.storage = Some(storage);

    // Should serialize/deserialize
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: NodeConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(
        deserialized
            .network_timing
            .as_ref()
            .unwrap()
            .target_peer_count,
        1
    );
    assert_eq!(
        deserialized
            .network_timing
            .as_ref()
            .unwrap()
            .peer_connection_delay_seconds,
        0
    );
}

#[test]
fn test_config_with_all_pruning_modes() {
    // Test all pruning modes
    let test_cases = vec![
        (PruningMode::Disabled, None),
        (
            PruningMode::Normal {
                keep_from_height: 0,
                min_recent_blocks: 100,
            },
            Some(100),
        ),
        (
            PruningMode::Normal {
                keep_from_height: 0,
                min_recent_blocks: 1000,
            },
            Some(1000),
        ),
    ];

    for (mode, expected_min) in test_cases {
        let pruning_config = PruningConfig {
            mode: match mode {
                PruningMode::Disabled => PruningMode::Disabled,
                PruningMode::Normal {
                    keep_from_height,
                    min_recent_blocks,
                } => PruningMode::Normal {
                    keep_from_height,
                    min_recent_blocks,
                },
                _ => continue,
            },
            prune_on_startup: true,
            ..Default::default()
        };

        // Should serialize/deserialize
        let json = serde_json::to_string(&pruning_config).unwrap();
        let deserialized: PruningConfig = serde_json::from_str(&json).unwrap();

        // Compare by matching variants since PruningMode doesn't implement PartialEq
        match (deserialized.mode, expected_min) {
            (PruningMode::Disabled, None) => {}
            (
                PruningMode::Normal {
                    min_recent_blocks: d_min,
                    ..
                },
                Some(e_min),
            ) => {
                assert_eq!(d_min, e_min);
            }
            _ => panic!("Modes don't match"),
        }
    }
}

#[test]
fn test_config_with_complex_indexing_config() {
    let mut indexing_config = IndexingConfig::default();
    indexing_config.enable_address_index = true;
    indexing_config.enable_value_index = true;
    indexing_config.strategy = IndexingStrategy::Lazy;

    // Should serialize/deserialize
    let json = serde_json::to_string(&indexing_config).unwrap();
    let deserialized: IndexingConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.enable_address_index, true);
    assert_eq!(deserialized.enable_value_index, true);
    // IndexingStrategy doesn't implement PartialEq, compare by matching
    match (deserialized.strategy, IndexingStrategy::Lazy) {
        (IndexingStrategy::Lazy, IndexingStrategy::Lazy) => {}
        _ => panic!("Strategy doesn't match"),
    }
}

#[test]
fn test_config_with_module_overrides() {
    let mut config = NodeConfig::default();

    // Add module-specific config overrides
    let mut module_config = HashMap::new();
    module_config.insert("key1".to_string(), "value1".to_string());
    module_config.insert("key2".to_string(), "value2".to_string());

    if let Some(ref mut modules) = config.modules {
        modules
            .module_configs
            .insert("test-module".to_string(), module_config);
    }

    // Should serialize/deserialize
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: NodeConfig = serde_json::from_str(&json).unwrap();

    assert!(deserialized
        .modules
        .as_ref()
        .unwrap()
        .module_configs
        .contains_key("test-module"));
    let module_config = deserialized
        .modules
        .as_ref()
        .unwrap()
        .module_configs
        .get("test-module")
        .unwrap();
    assert_eq!(module_config.get("key1"), Some(&"value1".to_string()));
}

#[test]
fn test_config_with_empty_strings() {
    let mut config = NodeConfig::default();

    // Set empty strings (should use defaults)
    if let Some(ref mut modules) = config.modules {
        modules.modules_dir = String::new();
        modules.data_dir = String::new();
    }

    // Should serialize/deserialize (empty strings are valid)
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: NodeConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.modules.as_ref().unwrap().modules_dir, "");
    assert_eq!(deserialized.modules.as_ref().unwrap().data_dir, "");
}

#[test]
fn test_config_with_all_indexing_strategies() {
    let strategies = vec![IndexingStrategy::Eager, IndexingStrategy::Lazy];

    for strategy in &strategies {
        let mut storage = StorageConfig::default();
        if let Some(ref mut indexing) = storage.indexing {
            indexing.strategy = match strategy {
                IndexingStrategy::Eager => IndexingStrategy::Eager,
                IndexingStrategy::Lazy => IndexingStrategy::Lazy,
            };
        }

        let mut config = NodeConfig::default();
        config.storage = Some(storage);

        // Should serialize/deserialize
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: NodeConfig = serde_json::from_str(&json).unwrap();

        // Compare strategies by matching
        if let Some(ref storage) = deserialized.storage {
            if let Some(ref indexing) = storage.indexing {
                match (indexing.strategy, strategy) {
                    (IndexingStrategy::Eager, IndexingStrategy::Eager) => {}
                    (IndexingStrategy::Lazy, IndexingStrategy::Lazy) => {}
                    _ => panic!("Strategy doesn't match"),
                }
            }
        }
    }
}

#[test]
fn test_config_roundtrip_complex_scenario() {
    // Create a complex configuration
    let mut config = NodeConfig::default();

    // Network settings
    config.transport_preference = TransportPreferenceConfig::TcpOnly;

    let mut network_timing = NetworkTimingConfig::default();
    network_timing.target_peer_count = 50;
    config.network_timing = Some(network_timing);

    // Storage settings
    let mut storage = StorageConfig::default();
    let mut pruning = PruningConfig::default();
    pruning.mode = PruningMode::Normal {
        keep_from_height: 0,
        min_recent_blocks: 5000,
    };
    storage.pruning = Some(pruning);
    if let Some(ref mut indexing) = storage.indexing {
        indexing.enable_address_index = true;
        indexing.strategy = IndexingStrategy::Eager;
    }
    config.storage = Some(storage);

    // Module settings
    if let Some(ref mut modules) = config.modules {
        modules.enabled_modules = vec!["module1".to_string(), "module2".to_string()];
    }

    // Roundtrip through JSON
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: NodeConfig = serde_json::from_str(&json).unwrap();

    // Verify all settings
    let deserialized_pref: bllvm_node::network::transport::TransportPreference =
        deserialized.transport_preference.into();
    let expected: bllvm_node::network::transport::TransportPreference =
        TransportPreferenceConfig::TcpOnly.into();
    assert_eq!(deserialized_pref, expected);
    assert_eq!(
        deserialized
            .network_timing
            .as_ref()
            .unwrap()
            .target_peer_count,
        50
    );
    if let Some(ref storage) = deserialized.storage {
        if let Some(ref pruning) = storage.pruning {
            match &pruning.mode {
                PruningMode::Normal {
                    min_recent_blocks, ..
                } => assert_eq!(*min_recent_blocks, 5000),
                _ => panic!("Expected Normal pruning mode"),
            }
        }
        if let Some(ref indexing) = storage.indexing {
            assert_eq!(indexing.enable_address_index, true);
            match indexing.strategy {
                IndexingStrategy::Eager => {}
                _ => panic!("Expected Eager strategy"),
            }
        }
    }
    assert_eq!(
        deserialized.modules.as_ref().unwrap().enabled_modules.len(),
        2
    );
}

#[test]
fn test_config_with_malformed_json_handling() {
    // Test that malformed JSON is handled gracefully
    let malformed_json = r#"{
        "transport_preference": "invalid_value"
    }"#;

    let result: Result<NodeConfig, _> = serde_json::from_str(malformed_json);
    // Should fail gracefully with a parse error
    assert!(result.is_err());
}

#[test]
fn test_config_defaults_after_partial_deserialization() {
    // Partial config should use defaults for missing fields
    let partial_json = r#"{
        "transport_preference": "tcponly"
    }"#;

    let config: NodeConfig = serde_json::from_str(partial_json).unwrap();

    // Missing fields should use defaults
    let pref: bllvm_node::network::transport::TransportPreference =
        config.transport_preference.into();
    assert!(pref.allows_tcp());
    // Other fields should have defaults (modules may be None, which is fine)
    if let Some(ref modules) = config.modules {
        assert_eq!(modules.enabled, true); // Default
    }
}
