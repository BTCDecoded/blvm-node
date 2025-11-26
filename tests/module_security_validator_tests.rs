//! Module Security Validator Tests
//!
//! Tests request validation to ensure modules cannot modify consensus.

use bllvm_node::module::ipc::protocol::RequestPayload;
use bllvm_node::module::security::validator::RequestValidator;

#[test]
fn test_validator_allows_read_operations() {
    let validator = RequestValidator::new();

    // All read operations should be allowed
    let payloads = vec![
        RequestPayload::GetBlock { hash: [0u8; 32] },
        RequestPayload::GetBlockHeader { hash: [0u8; 32] },
        RequestPayload::GetTransaction { hash: [0u8; 32] },
        RequestPayload::HasTransaction { hash: [0u8; 32] },
        RequestPayload::GetChainTip,
        RequestPayload::GetBlockHeight,
        RequestPayload::GetUtxo {
            outpoint: bllvm_protocol::OutPoint {
                hash: [0u8; 32],
                index: 0,
            },
        },
    ];

    for payload in payloads {
        let result = validator.validate_request("test-module", &payload);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            bllvm_node::module::security::validator::ValidationResult::Allowed
        );
    }
}

#[test]
fn test_validator_allows_handshake() {
    let validator = RequestValidator::new();

    let payload = RequestPayload::Handshake {
        module_id: "test-module".to_string(),
        module_name: "test-module".to_string(),
        version: "1.0.0".to_string(),
    };

    let result = validator.validate_request("test-module", &payload);
    assert!(result.is_ok());
    assert_eq!(
        result.unwrap(),
        bllvm_node::module::security::validator::ValidationResult::Allowed
    );
}

#[test]
fn test_validator_allows_subscribe_events() {
    use bllvm_node::module::traits::EventType;
    let validator = RequestValidator::new();

    let payload = RequestPayload::SubscribeEvents {
        event_types: vec![EventType::NewBlock, EventType::NewTransaction],
    };

    let result = validator.validate_request("test-module", &payload);
    assert!(result.is_ok());
    assert_eq!(
        result.unwrap(),
        bllvm_node::module::security::validator::ValidationResult::Allowed
    );
}

#[test]
fn test_validator_no_consensus_modification() {
    let validator = RequestValidator::new();

    // All current operations are read-only, so this should always pass
    // When write operations are added, they should be rejected here
    let result = validator.validate_no_consensus_modification("test-module", "GetChainTip");
    assert!(result.is_ok());
}

#[test]
fn test_validator_concurrent_requests() {
    use std::sync::Arc;
    let validator = Arc::new(RequestValidator::new());

    // Simulate concurrent validation requests
    let mut handles = vec![];
    for _ in 0..20 {
        let validator_clone = Arc::clone(&validator);

        handles.push(std::thread::spawn(move || {
            let payload = RequestPayload::GetChainTip;
            validator_clone.validate_request("test-module", &payload)
        }));
    }

    // All should succeed
    for handle in handles {
        let result = handle.join().unwrap();
        assert!(result.is_ok());
    }
}
