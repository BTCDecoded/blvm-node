//! Security and isolation tests
//!
//! Tests for sandboxing, permissions, and security boundaries.

use reference_node::tests::module::test_utils::*;
use reference_node::module::sandbox::{FileSystemSandbox, NetworkSandbox};

#[test]
fn test_filesystem_sandbox() {
    let fixture = ModuleTestFixture::new().unwrap();
    
    let sandbox = FileSystemSandbox::new(&fixture.data_dir);
    
    // Allowed path (within data directory)
    let allowed_path = fixture.data_dir.join("module1").join("data.txt");
    assert!(sandbox.is_within_sandbox(&allowed_path));
    
    // Disallowed path (outside data directory)
    let disallowed_path = fixture.modules_dir.join("../../etc/passwd");
    assert!(!sandbox.is_within_sandbox(&disallowed_path));
}

#[test]
fn test_network_sandbox() {
    let sandbox = NetworkSandbox::new();
    
    // Network access should be denied by default
    assert!(!sandbox.is_network_allowed());
    
    // Network operation should be rejected
    let result = sandbox.validate_network_operation("connect");
    assert!(result.is_err());
}

#[test]
fn test_manifest_validation() {
    use reference_node::module::validation::ManifestValidator;
    use reference_node::module::registry::manifest::ModuleManifest;
    
    let validator = ManifestValidator::new();
    
    // Create test manifests
    let valid_manifest = ModuleManifest {
        name: "my-module".to_string(),
        version: "1.0.0".to_string(),
        description: Some("Test module".to_string()),
        author: Some("Test".to_string()),
        capabilities: vec![],
        dependencies: std::collections::HashMap::new(),
        entry_point: "my-module".to_string(),
        config_schema: std::collections::HashMap::new(),
    };
    
    let invalid_manifest = ModuleManifest {
        name: "".to_string(),  // Invalid: empty name
        version: "1.0.0".to_string(),
        description: Some("Test module".to_string()),
        author: Some("Test".to_string()),
        capabilities: vec![],
        dependencies: std::collections::HashMap::new(),
        entry_point: "my-module".to_string(),
        config_schema: std::collections::HashMap::new(),
    };
    
    // Validate manifests
    use reference_node::module::validation::ValidationResult;
    let valid_result = validator.validate(&valid_manifest);
    assert!(matches!(valid_result, ValidationResult::Valid));
    
    let invalid_result = validator.validate(&invalid_manifest);
    assert!(matches!(invalid_result, ValidationResult::Invalid(_)));
}

