//! Tests for timeout utilities

use blvm_node::utils::timeout::{
    with_custom_timeout, with_network_timeout, with_rpc_timeout, with_storage_timeout,
    DEFAULT_NETWORK_TIMEOUT, DEFAULT_RPC_TIMEOUT, DEFAULT_STORAGE_TIMEOUT,
};
use std::time::Duration;
use tokio::time::sleep;

#[test]
fn test_default_timeout_constants() {
    assert_eq!(DEFAULT_NETWORK_TIMEOUT, Duration::from_secs(30));
    assert_eq!(DEFAULT_STORAGE_TIMEOUT, Duration::from_secs(10));
    assert_eq!(DEFAULT_RPC_TIMEOUT, Duration::from_secs(60));
}

#[tokio::test]
async fn test_with_custom_timeout_success() {
    let result = with_custom_timeout(async { 42 }, Duration::from_secs(1)).await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 42);
}

#[tokio::test]
async fn test_with_custom_timeout_timeout() {
    let result = with_custom_timeout(
        async {
            sleep(Duration::from_secs(2)).await;
            42
        },
        Duration::from_millis(100),
    )
    .await;

    assert!(result.is_err()); // Should timeout
}

#[tokio::test]
async fn test_with_network_timeout_success() {
    let result = with_network_timeout(async { 42 }).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 42);
}

#[tokio::test]
async fn test_with_storage_timeout_success() {
    let result = with_storage_timeout(async { 42 }).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 42);
}

#[tokio::test]
async fn test_with_rpc_timeout_success() {
    let result = with_rpc_timeout(async { 42 }).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 42);
}

#[tokio::test]
async fn test_with_network_timeout_timeout() {
    let result = with_network_timeout(async {
        sleep(Duration::from_secs(31)).await; // Longer than DEFAULT_NETWORK_TIMEOUT
        42
    })
    .await;

    assert!(result.is_err()); // Should timeout
}

#[tokio::test]
async fn test_with_storage_timeout_timeout() {
    let result = with_storage_timeout(async {
        sleep(Duration::from_secs(11)).await; // Longer than DEFAULT_STORAGE_TIMEOUT
        42
    })
    .await;

    assert!(result.is_err()); // Should timeout
}

#[tokio::test]
async fn test_timeout_config_helpers() {
    use blvm_node::config::RequestTimeoutConfig;
    use blvm_node::utils::timeout::{
        checkpoint_timeout_from_config, handshake_timeout_from_config,
        headers_verify_timeout_from_config, lan_security_timeouts_from_config,
        network_timeout_from_config, protocol_verify_timeout_from_config, rpc_timeout_from_config,
        storage_timeout_from_config,
    };

    let cfg = RequestTimeoutConfig {
        storage_timeout_seconds: 11,
        network_timeout_seconds: 31,
        rpc_timeout_seconds: 61,
        handshake_timeout_secs: 12,
        checkpoint_request_timeout_secs: 6,
        protocol_verify_timeout_secs: 7,
        headers_verify_timeout_secs: 13,
        ..Default::default()
    };

    assert_eq!(
        storage_timeout_from_config(Some(&cfg)),
        Duration::from_secs(11)
    );
    assert_eq!(
        network_timeout_from_config(Some(&cfg)),
        Duration::from_secs(31)
    );
    assert_eq!(rpc_timeout_from_config(Some(&cfg)), Duration::from_secs(61));
    assert_eq!(
        handshake_timeout_from_config(Some(&cfg)),
        Duration::from_secs(12)
    );
    assert_eq!(
        checkpoint_timeout_from_config(Some(&cfg)),
        Duration::from_secs(6)
    );
    assert_eq!(
        protocol_verify_timeout_from_config(Some(&cfg)),
        Duration::from_secs(7)
    );
    assert_eq!(
        headers_verify_timeout_from_config(Some(&cfg)),
        Duration::from_secs(13)
    );
    assert_eq!(
        lan_security_timeouts_from_config(Some(&cfg)),
        (Duration::from_secs(7), Duration::from_secs(13))
    );
}

#[tokio::test]
async fn test_with_rpc_timeout_timeout() {
    let result = with_rpc_timeout(async {
        sleep(Duration::from_secs(61)).await;
        42
    })
    .await;

    assert!(result.is_err());
}
