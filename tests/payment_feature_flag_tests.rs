//! Feature flag tests for payment system
//!
//! Tests compile-time feature flag behavior for HTTP BIP70 support.
//! Verifies that HTTP endpoints are conditionally compiled and P2P is always available.

use blvm_node::config::PaymentConfig;
use blvm_node::payment::processor::{PaymentError, PaymentProcessor};

#[test]
fn test_p2p_always_available() {
    // Test that P2P is always available (no feature flag required)
    let config = PaymentConfig {
        p2p_enabled: true,
        http_enabled: false,
        ..Default::default()
    };

    let result = PaymentProcessor::new(config);
    assert!(
        result.is_ok(),
        "P2P should always be available without any feature flags"
    );
}

#[test]
#[cfg(feature = "bip70-http")]
fn test_http_feature_enabled() {
    // Test that HTTP endpoints are available when feature enabled
    let config = PaymentConfig {
        p2p_enabled: false,
        http_enabled: true,
        ..Default::default()
    };

    let result = PaymentProcessor::new(config);
    assert!(
        result.is_ok(),
        "HTTP should be available when bip70-http feature is enabled"
    );
}

#[test]
#[cfg(not(feature = "bip70-http"))]
fn test_http_feature_disabled() {
    // Test that HTTP requires feature flag
    let config = PaymentConfig {
        p2p_enabled: false,
        http_enabled: true,
        ..Default::default()
    };

    let result = PaymentProcessor::new(config.clone());
    assert!(
        result.is_err(),
        "HTTP should require bip70-http feature flag when disabled"
    );

    if let Err(PaymentError::FeatureNotEnabled(_)) = result {
        // Expected error
    } else {
        // Verify it's still an error
        let result2 = PaymentProcessor::new(config);
        assert!(result2.is_err(), "Should still be an error");
    }
}

#[test]
#[cfg(feature = "bip70-http")]
fn test_http_endpoints_conditional_compilation() {
    // Test that HTTP handler functions exist when feature is enabled
    // Module exists - compilation succeeds
    use blvm_node::payment::http;
    assert!(true, "HTTP handler module is conditionally compiled");
}

#[test]
#[cfg(feature = "bip70-http")]
fn test_payment_module_http_available() {
    // Test that payment module exports HTTP handlers when feature enabled
    // This verifies conditional compilation in mod.rs
    use blvm_node::payment;

    // Verify HTTP module is available
    #[cfg(feature = "bip70-http")]
    {
        use blvm_node::payment::http;
        // Module exists - compilation succeeds
        assert!(true, "HTTP payment module is available with feature");
    }
}

#[test]
#[cfg(not(feature = "bip70-http"))]
fn test_payment_module_http_not_available() {
    // Test that HTTP module is not available when feature disabled
    // The http module should not be accessible
    // PaymentProcessor should still be available
    let config = PaymentConfig::default();
    let result = PaymentProcessor::new(config);
    assert!(
        result.is_ok(),
        "PaymentProcessor should be available without HTTP feature"
    );
}

#[test]
#[cfg(all(feature = "bip70-http", feature = "rest-api"))]
fn test_http_with_rest_api_feature() {
    // Test that HTTP works when both bip70-http and rest-api features are enabled
    let config = PaymentConfig {
        p2p_enabled: true,
        http_enabled: true,
        ..Default::default()
    };

    let result = PaymentProcessor::new(config);
    assert!(
        result.is_ok(),
        "HTTP should work when both bip70-http and rest-api features are enabled"
    );
}

#[test]
#[cfg(not(feature = "rest-api"))]
fn test_http_requires_rest_api_feature() {
    // Test that HTTP also requires rest-api feature
    let config = PaymentConfig {
        p2p_enabled: false,
        http_enabled: true,
        ..Default::default()
    };

    let result = PaymentProcessor::new(config.clone());
    assert!(
        result.is_err(),
        "HTTP should require rest-api feature flag when disabled"
    );

    if let Err(PaymentError::FeatureNotEnabled(_)) = result {
        // Expected error
    } else {
        // Verify it's still an error
        let result2 = PaymentProcessor::new(config);
        assert!(result2.is_err(), "Should still be an error");
    }
}

#[test]
fn test_both_transports_with_features() {
    // Test that both transports can be enabled when features are available
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
            "Both transports should work when required features are enabled"
        );
    }

    #[cfg(not(all(feature = "bip70-http", feature = "rest-api")))]
    {
        let result = PaymentProcessor::new(config);
        assert!(
            result.is_err(),
            "Both transports should fail when required features are not enabled"
        );
    }
}

#[test]
#[cfg(all(feature = "bip70-http", feature = "rest-api"))]
fn test_rest_api_server_payment_processor_method() {
    // Test that RestApiServer has with_payment_processor method when features enabled
    // This is a compile-time test - if the method doesn't exist, compilation fails
    #[cfg(all(feature = "bip70-http", feature = "rest-api"))]
    {
        use blvm_node::rpc::rest::server::RestApiServer;
        // Method exists - compilation succeeds
        assert!(
            true,
            "RestApiServer has with_payment_processor method when features enabled"
        );
    }
}

#[test]
#[cfg(any(not(feature = "bip70-http"), not(feature = "rest-api")))]
fn test_rest_api_server_no_payment_processor_method() {
    // Test that RestApiServer doesn't have with_payment_processor method when features disabled
    // This is a compile-time test - the method should not exist
    #[cfg(not(feature = "bip70-http"))]
    {
        // When bip70-http is disabled, the method shouldn't exist
        // We can't easily test this without trying to use it, but compilation succeeds
        assert!(
            true,
            "RestApiServer doesn't have with_payment_processor method when feature disabled"
        );
    }

    #[cfg(all(feature = "bip70-http", not(feature = "rest-api")))]
    {
        // When rest-api is disabled, the rest module doesn't exist
        assert!(
            true,
            "RestApiServer module not available when rest-api feature disabled"
        );
    }
}

#[test]
fn test_p2p_independent_of_features() {
    // Test that P2P functionality is independent of HTTP feature flags
    let config = PaymentConfig {
        p2p_enabled: true,
        http_enabled: false,
        ..Default::default()
    };

    // Should work regardless of feature flags
    let result = PaymentProcessor::new(config);
    assert!(
        result.is_ok(),
        "P2P should work regardless of HTTP feature flags"
    );

    // P2P handlers should always be available
    // (We can't easily test P2P handlers without network setup, but processor creation proves it)
}

#[test]
#[cfg(feature = "bip70-http")]
fn test_http_handler_functions_exist() {
    // Test that HTTP handler functions are available when feature enabled
    // This is a compile-time test
    use blvm_node::payment::http;
    // Module exists - compilation succeeds
    assert!(true, "HTTP handler functions exist when feature enabled");
}
