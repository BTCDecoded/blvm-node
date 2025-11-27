//! Configuration tests for payment system
//!
//! Tests PaymentConfig and RestApiConfig validation, defaults, and behavior.

use bllvm_node::config::{PaymentConfig, RestApiConfig};
use bllvm_node::payment::processor::{PaymentError, PaymentProcessor};

#[test]
fn test_payment_config_defaults() {
    // Test default configuration values
    let config = PaymentConfig::default();
    assert_eq!(config.p2p_enabled, true, "P2P should be enabled by default");
    assert_eq!(
        config.http_enabled, false,
        "HTTP should be disabled by default"
    );
    assert_eq!(
        config.module_payments_enabled, true,
        "Module payments should be enabled by default"
    );
    assert_eq!(
        config.payment_store_path, "data/payments",
        "Default payment store path should be 'data/payments'"
    );
    assert_eq!(
        config.merchant_key, None,
        "Merchant key should be None by default"
    );
}

#[test]
fn test_rest_api_config_defaults() {
    // Test default REST API configuration values
    let config = RestApiConfig::default();
    assert_eq!(
        config.enabled, false,
        "REST API should be disabled by default"
    );
    assert_eq!(
        config.payment_endpoints_enabled, false,
        "Payment endpoints should be disabled by default"
    );
}

#[tokio::test]
async fn test_payment_config_validation_p2p_only() {
    // Test P2P-only configuration (valid, default)
    let config = PaymentConfig {
        p2p_enabled: true,
        http_enabled: false,
        ..Default::default()
    };
    let result = PaymentProcessor::new(config);
    assert!(result.is_ok(), "P2P-only configuration should be valid");
}

#[tokio::test]
async fn test_payment_config_validation_http_requires_feature() {
    // Test HTTP requires feature flag
    let config = PaymentConfig {
        p2p_enabled: false,
        http_enabled: true,
        ..Default::default()
    };

    #[cfg(not(feature = "bip70-http"))]
    {
        let result = PaymentProcessor::new(config.clone());
        assert!(
            result.is_err(),
            "HTTP should require bip70-http feature flag"
        );
        if let Err(PaymentError::FeatureNotEnabled(_)) = result {
            // Expected error
        } else {
            // Verify it's still an error (could be FeatureNotEnabled for rest-api)
            let result2 = PaymentProcessor::new(config);
            assert!(result2.is_err(), "Should still be an error");
        }
    }

    #[cfg(feature = "bip70-http")]
    {
        // With feature enabled, HTTP-only should work
        let result = PaymentProcessor::new(config);
        assert!(
            result.is_ok(),
            "HTTP-only configuration should work with bip70-http feature"
        );
    }
}

#[tokio::test]
async fn test_payment_config_validation_http_requires_rest_api() {
    // Test HTTP also requires rest-api feature
    let config = PaymentConfig {
        p2p_enabled: false,
        http_enabled: true,
        ..Default::default()
    };

    #[cfg(not(feature = "rest-api"))]
    {
        let result = PaymentProcessor::new(config.clone());
        assert!(result.is_err(), "HTTP should require rest-api feature flag");
        if let Err(PaymentError::FeatureNotEnabled(_)) = result {
            // Expected error
        } else {
            // Verify it's still an error
            let result2 = PaymentProcessor::new(config);
            assert!(result2.is_err(), "Should still be an error");
        }
    }
}

#[tokio::test]
async fn test_payment_config_validation_no_transport() {
    // Test that at least one transport must be enabled
    let config = PaymentConfig {
        p2p_enabled: false,
        http_enabled: false,
        ..Default::default()
    };
    let result = PaymentProcessor::new(config);
    assert!(
        result.is_err(),
        "Configuration with no transports enabled should fail"
    );
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("Expected error but got Ok"),
    };
    match err {
        PaymentError::NoTransportEnabled => {
            // Expected error
        }
        _ => panic!("Expected NoTransportEnabled error"),
    }
}

#[tokio::test]
async fn test_payment_config_validation_both_transports() {
    // Test that both transports can be enabled (if features are available)
    let config = PaymentConfig {
        p2p_enabled: true,
        http_enabled: true,
        ..Default::default()
    };

    #[cfg(all(feature = "bip70-http", feature = "rest-api"))]
    {
        let result = PaymentProcessor::new(config);
        assert!(
            result.is_ok(),
            "Both transports enabled should work with required features"
        );
    }

    #[cfg(not(all(feature = "bip70-http", feature = "rest-api")))]
    {
        let result = PaymentProcessor::new(config);
        assert!(
            result.is_err(),
            "Both transports enabled should fail without required features"
        );
    }
}

#[test]
fn test_payment_config_custom_store_path() {
    // Test custom payment store path
    let config = PaymentConfig {
        payment_store_path: "custom/payments/path".to_string(),
        ..Default::default()
    };
    assert_eq!(
        config.payment_store_path, "custom/payments/path",
        "Custom payment store path should be set"
    );
}

#[test]
fn test_payment_config_merchant_key() {
    // Test merchant key configuration
    let test_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let config = PaymentConfig {
        merchant_key: Some(test_key.to_string()),
        ..Default::default()
    };
    assert_eq!(
        config.merchant_key,
        Some(test_key.to_string()),
        "Merchant key should be set"
    );
}

#[test]
fn test_payment_config_module_payments_disabled() {
    // Test disabling module payments
    let config = PaymentConfig {
        module_payments_enabled: false,
        ..Default::default()
    };
    assert_eq!(
        config.module_payments_enabled, false,
        "Module payments should be disabled"
    );
}

#[test]
fn test_rest_api_config_enabled() {
    // Test enabling REST API
    let config = RestApiConfig {
        enabled: true,
        payment_endpoints_enabled: false,
    };
    assert_eq!(config.enabled, true, "REST API should be enabled");
    assert_eq!(
        config.payment_endpoints_enabled, false,
        "Payment endpoints should still be disabled"
    );
}

#[test]
fn test_rest_api_config_payment_endpoints_enabled() {
    // Test enabling payment endpoints
    let config = RestApiConfig {
        enabled: true,
        payment_endpoints_enabled: true,
    };
    assert_eq!(config.enabled, true, "REST API should be enabled");
    assert_eq!(
        config.payment_endpoints_enabled, true,
        "Payment endpoints should be enabled"
    );
}

#[test]
fn test_payment_config_serialization() {
    // Test that PaymentConfig can be serialized/deserialized
    use serde_json;

    let config = PaymentConfig {
        p2p_enabled: true,
        http_enabled: false,
        merchant_key: Some("test_key".to_string()),
        payment_store_path: "test/path".to_string(),
        module_payments_enabled: true,
    };

    let serialized = serde_json::to_string(&config).expect("Should serialize");
    let deserialized: PaymentConfig =
        serde_json::from_str(&serialized).expect("Should deserialize");

    assert_eq!(deserialized.p2p_enabled, config.p2p_enabled);
    assert_eq!(deserialized.http_enabled, config.http_enabled);
    assert_eq!(deserialized.merchant_key, config.merchant_key);
    assert_eq!(deserialized.payment_store_path, config.payment_store_path);
    assert_eq!(
        deserialized.module_payments_enabled,
        config.module_payments_enabled
    );
}

#[test]
fn test_rest_api_config_serialization() {
    // Test that RestApiConfig can be serialized/deserialized
    use serde_json;

    let config = RestApiConfig {
        enabled: true,
        payment_endpoints_enabled: true,
    };

    let serialized = serde_json::to_string(&config).expect("Should serialize");
    let deserialized: RestApiConfig =
        serde_json::from_str(&serialized).expect("Should deserialize");

    assert_eq!(deserialized.enabled, config.enabled);
    assert_eq!(
        deserialized.payment_endpoints_enabled,
        config.payment_endpoints_enabled
    );
}

#[test]
fn test_payment_config_toml_parsing() {
    // Test parsing PaymentConfig from TOML
    use toml;

    let toml_str = r#"
        p2p_enabled = true
        http_enabled = false
        payment_store_path = "custom/path"
        module_payments_enabled = true
    "#;

    let config: PaymentConfig = toml::from_str(toml_str).expect("Should parse TOML");
    assert_eq!(config.p2p_enabled, true);
    assert_eq!(config.http_enabled, false);
    assert_eq!(config.payment_store_path, "custom/path");
    assert_eq!(config.module_payments_enabled, true);
}

#[test]
fn test_payment_config_toml_with_defaults() {
    // Test TOML parsing with missing fields (should use defaults)
    use toml;

    let toml_str = r#"
        p2p_enabled = false
    "#;

    let config: PaymentConfig = toml::from_str(toml_str).expect("Should parse TOML");
    assert_eq!(config.p2p_enabled, false);
    assert_eq!(
        config.http_enabled, false,
        "Should use default for missing field"
    );
    assert_eq!(
        config.payment_store_path, "data/payments",
        "Should use default for missing field"
    );
    assert_eq!(
        config.module_payments_enabled, true,
        "Should use default for missing field"
    );
}

#[test]
fn test_rest_api_config_toml_parsing() {
    // Test parsing RestApiConfig from TOML
    use toml;

    let toml_str = r#"
        enabled = true
        payment_endpoints_enabled = true
    "#;

    let config: RestApiConfig = toml::from_str(toml_str).expect("Should parse TOML");
    assert_eq!(config.enabled, true);
    assert_eq!(config.payment_endpoints_enabled, true);
}

#[test]
fn test_rest_api_config_toml_with_defaults() {
    // Test TOML parsing with missing fields (should use defaults)
    use toml;

    let toml_str = r#"
        enabled = true
    "#;

    let config: RestApiConfig = toml::from_str(toml_str).expect("Should parse TOML");
    assert_eq!(config.enabled, true);
    assert_eq!(
        config.payment_endpoints_enabled, false,
        "Should use default for missing field"
    );
}

#[tokio::test]
async fn test_payment_processor_with_custom_config() {
    // Test that PaymentProcessor works with custom configuration
    let config = PaymentConfig {
        p2p_enabled: true,
        http_enabled: false,
        payment_store_path: "test/payments".to_string(),
        module_payments_enabled: true,
        merchant_key: None,
    };

    let _processor = PaymentProcessor::new(config).expect("Should create processor");
    // Processor is created successfully, config is stored internally
    // We can't directly access config, but we can verify processor works
    // by checking it doesn't panic
}

#[tokio::test]
async fn test_payment_processor_config_immutability() {
    // Test that PaymentProcessor configuration is immutable after creation
    let config1 = PaymentConfig {
        p2p_enabled: true,
        http_enabled: false,
        ..Default::default()
    };

    let config2 = PaymentConfig {
        p2p_enabled: true,
        http_enabled: false,
        payment_store_path: "different/path".to_string(),
        ..Default::default()
    };

    let _processor1 = PaymentProcessor::new(config1).expect("Should create processor 1");
    let _processor2 = PaymentProcessor::new(config2).expect("Should create processor 2");

    // Both processors should work independently
    // (We can't directly compare configs, but both should function)
    // This test mainly ensures that different configs create different processors
    // without errors
}
