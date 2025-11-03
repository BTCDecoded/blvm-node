//! Module API tests
//!
//! Tests for module API access, permissions, and request validation.

use reference_node::tests::module::test_utils::*;
use reference_node::module::security::permissions::{Permission, PermissionSet};
use reference_node::module::api::hub::ModuleApiHub;
use reference_node::module::ipc::protocol::{RequestMessage, RequestPayload};
use reference_node::Hash;

#[tokio::test]
async fn test_permission_checker() {
    use reference_node::module::security::permissions::PermissionChecker;
    
    let mut checker = PermissionChecker::new();
    
    // Test default permissions
    assert!(checker.check_permission("test-module", &Permission::ReadBlockchain));
    assert!(checker.check_permission("test-module", &Permission::ReadChainState));
    
    // Register custom permissions
    let mut custom_perms = PermissionSet::new();
    custom_perms.add(Permission::SendTransactions);
    checker.register_module_permissions("test-module".to_string(), custom_perms);
    
    // Custom permissions should override defaults
    assert!(checker.check_permission("test-module", &Permission::SendTransactions));
}

#[tokio::test]
async fn test_request_validator() {
    use reference_node::module::security::validator::RequestValidator;
    use reference_node::module::ipc::protocol::RequestPayload;
    
    let validator = RequestValidator::new();
    
    // All current operations are read-only, so validation should pass
    let payload = RequestPayload::GetBlock {
        hash: Hash::default(),
    };
    
    use reference_node::module::security::validator::ValidationResult;
    let result = validator.validate_request("test-module", &payload).unwrap();
    assert!(matches!(result, ValidationResult::Allowed));
}

#[tokio::test]
async fn test_api_hub_creation() {
    let fixture = ModuleTestFixture::new().unwrap();
    
    let hub = ModuleApiHub::new(fixture.node_api);
    
    // Hub should be created successfully
    assert!(true); // Placeholder - hub creation doesn't have visible state yet
}

