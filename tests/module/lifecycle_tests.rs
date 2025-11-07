//! Module lifecycle tests
//!
//! Tests for module loading, unloading, reloading, and crash recovery.

use bllvm_node::tests::module::test_utils::*;

#[tokio::test]
async fn test_module_manager_creation() {
    let fixture = ModuleTestFixture::new().unwrap();
    
    // Verify fixture components
    assert!(fixture.modules_dir.exists());
    assert!(fixture.data_dir.exists());
    assert!(fixture.socket_dir.exists());
}

#[tokio::test]
async fn test_module_discovery() {
    let fixture = ModuleTestFixture::new().unwrap();
    
    // Create a test module manifest
    let module_dir = fixture.modules_dir.join("test-module");
    fixture.create_test_manifest(&module_dir, "test-module", "0.1.0").unwrap();
    
    // Test module discovery  
    use bllvm_node::module::registry::discovery::ModuleDiscovery;
    let discovery = ModuleDiscovery::new(&fixture.modules_dir);
    let modules = discovery.discover_modules().unwrap();
    
    assert_eq!(modules.len(), 1);
    assert_eq!(modules[0].manifest.name, "test-module");
}

#[tokio::test]
async fn test_module_context_creation() {
    let fixture = ModuleTestFixture::new().unwrap();
    
    let context = fixture.create_test_context("test-module-1");
    
    assert_eq!(context.module_id, "test-module-1");
    assert!(context.ipc_socket_path.contains("test-module-1"));
    assert!(context.data_dir.contains("test-module-1"));
}

#[tokio::test]
async fn test_module_manifest_validation() {
    let fixture = ModuleTestFixture::new().unwrap();
    
    // Create valid manifest
    let module_dir = fixture.modules_dir.join("valid-module");
    let manifest_path = fixture.create_test_manifest(&module_dir, "valid-module", "1.0.0").unwrap();
    
    let manifest = ModuleManifest::from_file(&manifest_path).unwrap();
    assert_eq!(manifest.name, "valid-module");
    assert_eq!(manifest.version, "1.0.0");
    
    // Validate manifest
    use bllvm_node::module::validation::{ManifestValidator, ValidationResult};
    let validator = ManifestValidator::new();
    let result = validator.validate(&manifest);
    
    assert!(matches!(result, ValidationResult::Valid));
}

#[tokio::test]
async fn test_mock_module() {
    let mut mock = MockModule::new("test-mock");
    
    assert_eq!(mock.state, "stopped");
    assert_eq!(mock.events_received, 0);
    
    mock.start();
    assert_eq!(mock.state, "running");
    
    mock.receive_event();
    assert_eq!(mock.events_received, 1);
    
    mock.stop();
    assert_eq!(mock.state, "stopped");
}

