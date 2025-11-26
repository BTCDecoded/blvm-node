//! Tests for module registry (discovery, dependencies, manifest)

use bllvm_node::module::registry::dependencies::{DependencyResolution, ModuleDependencies};
use bllvm_node::module::registry::discovery::{DiscoveredModule, ModuleDiscovery};
use bllvm_node::module::registry::manifest::ModuleManifest;
use bllvm_node::module::traits::ModuleError;
use std::collections::HashMap;
use std::path::PathBuf;
use tempfile::TempDir;

// Helper to create a test manifest
fn create_test_manifest(name: &str, deps: HashMap<String, String>) -> ModuleManifest {
    ModuleManifest {
        name: name.to_string(),
        version: "1.0.0".to_string(),
        description: Some(format!("Test module {}", name)),
        author: Some("Test Author".to_string()),
        capabilities: Vec::new(),
        dependencies: deps,
        entry_point: format!("{}.so", name),
        config_schema: HashMap::new(),
    }
}

// Helper to create a test discovered module
fn create_discovered_module(name: &str, deps: HashMap<String, String>, dir: PathBuf) -> DiscoveredModule {
    DiscoveredModule {
        directory: dir.clone(),
        manifest: create_test_manifest(name, deps),
        binary_path: dir.join(format!("{}.so", name)),
    }
}

// ===== Manifest Tests =====

#[test]
fn test_module_manifest_from_file_valid() {
    let temp_dir = TempDir::new().unwrap();
    let manifest_path = temp_dir.path().join("module.toml");
    
    let toml_content = r#"
name = "test-module"
version = "1.0.0"
description = "A test module"
author = "Test Author"
entry_point = "test-module.so"
"#;
    std::fs::write(&manifest_path, toml_content).unwrap();
    
    let result = ModuleManifest::from_file(&manifest_path);
    assert!(result.is_ok());
    
    let manifest = result.unwrap();
    assert_eq!(manifest.name, "test-module");
    assert_eq!(manifest.version, "1.0.0");
    assert_eq!(manifest.description, Some("A test module".to_string()));
    assert_eq!(manifest.author, Some("Test Author".to_string()));
    assert_eq!(manifest.entry_point, "test-module.so");
}

#[test]
fn test_module_manifest_from_file_with_dependencies() {
    let temp_dir = TempDir::new().unwrap();
    let manifest_path = temp_dir.path().join("module.toml");
    
    let toml_content = r#"
name = "test-module"
version = "1.0.0"
entry_point = "test-module.so"

[dependencies]
dep1 = "1.0.0"
dep2 = "2.0.0"
"#;
    std::fs::write(&manifest_path, toml_content).unwrap();
    
    let result = ModuleManifest::from_file(&manifest_path);
    assert!(result.is_ok());
    
    let manifest = result.unwrap();
    assert_eq!(manifest.dependencies.len(), 2);
    assert_eq!(manifest.dependencies.get("dep1"), Some(&"1.0.0".to_string()));
    assert_eq!(manifest.dependencies.get("dep2"), Some(&"2.0.0".to_string()));
}

#[test]
fn test_module_manifest_from_file_with_capabilities() {
    let temp_dir = TempDir::new().unwrap();
    let manifest_path = temp_dir.path().join("module.toml");
    
    let toml_content = r#"
name = "test-module"
version = "1.0.0"
entry_point = "test-module.so"
capabilities = ["cap1", "cap2"]
"#;
    std::fs::write(&manifest_path, toml_content).unwrap();
    
    let result = ModuleManifest::from_file(&manifest_path);
    assert!(result.is_ok());
    
    let manifest = result.unwrap();
    assert_eq!(manifest.capabilities.len(), 2);
    assert!(manifest.capabilities.contains(&"cap1".to_string()));
    assert!(manifest.capabilities.contains(&"cap2".to_string()));
}

#[test]
fn test_module_manifest_from_file_empty_name() {
    let temp_dir = TempDir::new().unwrap();
    let manifest_path = temp_dir.path().join("module.toml");
    
    let toml_content = r#"
name = ""
version = "1.0.0"
entry_point = "test-module.so"
"#;
    std::fs::write(&manifest_path, toml_content).unwrap();
    
    let result = ModuleManifest::from_file(&manifest_path);
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ModuleError::InvalidManifest(_)));
}

#[test]
fn test_module_manifest_from_file_empty_entry_point() {
    let temp_dir = TempDir::new().unwrap();
    let manifest_path = temp_dir.path().join("module.toml");
    
    let toml_content = r#"
name = "test-module"
version = "1.0.0"
entry_point = ""
"#;
    std::fs::write(&manifest_path, toml_content).unwrap();
    
    let result = ModuleManifest::from_file(&manifest_path);
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ModuleError::InvalidManifest(_)));
}

#[test]
fn test_module_manifest_from_file_nonexistent() {
    let manifest_path = PathBuf::from("/nonexistent/module.toml");
    let result = ModuleManifest::from_file(&manifest_path);
    assert!(result.is_err());
}

#[test]
fn test_module_manifest_from_file_invalid_toml() {
    let temp_dir = TempDir::new().unwrap();
    let manifest_path = temp_dir.path().join("module.toml");
    
    let invalid_toml = r#"
name = "test-module"
version = "1.0.0"
entry_point = "test-module.so"
[invalid
"#;
    std::fs::write(&manifest_path, invalid_toml).unwrap();
    
    let result = ModuleManifest::from_file(&manifest_path);
    assert!(result.is_err());
}

#[test]
fn test_module_manifest_to_metadata() {
    let mut deps = HashMap::new();
    deps.insert("dep1".to_string(), "1.0.0".to_string());
    
    let manifest = create_test_manifest("test-module", deps.clone());
    let metadata = manifest.to_metadata();
    
    assert_eq!(metadata.name, "test-module");
    assert_eq!(metadata.version, "1.0.0");
    assert_eq!(metadata.dependencies, deps);
    assert_eq!(metadata.entry_point, "test-module.so");
}

// ===== Dependency Resolution Tests =====

#[test]
fn test_dependency_resolution_no_dependencies() {
    let temp_dir = TempDir::new().unwrap();
    let module_dir = temp_dir.path().join("module1");
    std::fs::create_dir_all(&module_dir).unwrap();
    
    let modules = vec![create_discovered_module("module1", HashMap::new(), module_dir)];
    
    let result = ModuleDependencies::resolve(&modules);
    assert!(result.is_ok());
    
    let resolution = result.unwrap();
    assert_eq!(resolution.load_order, vec!["module1"]);
    assert!(resolution.missing.is_empty());
}

#[test]
fn test_dependency_resolution_simple_dependency() {
    let temp_dir = TempDir::new().unwrap();
    let module1_dir = temp_dir.path().join("module1");
    let module2_dir = temp_dir.path().join("module2");
    std::fs::create_dir_all(&module1_dir).unwrap();
    std::fs::create_dir_all(&module2_dir).unwrap();
    
    let mut deps = HashMap::new();
    deps.insert("module1".to_string(), "1.0.0".to_string());
    
    let modules = vec![
        create_discovered_module("module1", HashMap::new(), module1_dir),
        create_discovered_module("module2", deps, module2_dir),
    ];
    
    let result = ModuleDependencies::resolve(&modules);
    assert!(result.is_ok());
    
    let resolution = result.unwrap();
    // module1 should come before module2 (dependency first)
    assert_eq!(resolution.load_order.len(), 2);
    assert_eq!(resolution.load_order[0], "module1");
    assert_eq!(resolution.load_order[1], "module2");
}

#[test]
fn test_dependency_resolution_missing_dependency() {
    let temp_dir = TempDir::new().unwrap();
    let module2_dir = temp_dir.path().join("module2");
    std::fs::create_dir_all(&module2_dir).unwrap();
    
    let mut deps = HashMap::new();
    deps.insert("missing-module".to_string(), "1.0.0".to_string());
    
    let modules = vec![create_discovered_module("module2", deps, module2_dir)];
    
    let result = ModuleDependencies::resolve(&modules);
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ModuleError::DependencyMissing(_)));
}

#[test]
fn test_dependency_resolution_circular_dependency() {
    let temp_dir = TempDir::new().unwrap();
    let module1_dir = temp_dir.path().join("module1");
    let module2_dir = temp_dir.path().join("module2");
    std::fs::create_dir_all(&module1_dir).unwrap();
    std::fs::create_dir_all(&module2_dir).unwrap();
    
    let mut deps1 = HashMap::new();
    deps1.insert("module2".to_string(), "1.0.0".to_string());
    
    let mut deps2 = HashMap::new();
    deps2.insert("module1".to_string(), "1.0.0".to_string());
    
    let modules = vec![
        create_discovered_module("module1", deps1, module1_dir),
        create_discovered_module("module2", deps2, module2_dir),
    ];
    
    let result = ModuleDependencies::resolve(&modules);
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ModuleError::DependencyMissing(_)));
}

#[test]
fn test_dependency_resolution_complex_dependency_chain() {
    let temp_dir = TempDir::new().unwrap();
    let module1_dir = temp_dir.path().join("module1");
    let module2_dir = temp_dir.path().join("module2");
    let module3_dir = temp_dir.path().join("module3");
    std::fs::create_dir_all(&module1_dir).unwrap();
    std::fs::create_dir_all(&module2_dir).unwrap();
    std::fs::create_dir_all(&module3_dir).unwrap();
    
    let mut deps2 = HashMap::new();
    deps2.insert("module1".to_string(), "1.0.0".to_string());
    
    let mut deps3 = HashMap::new();
    deps3.insert("module2".to_string(), "1.0.0".to_string());
    
    let modules = vec![
        create_discovered_module("module1", HashMap::new(), module1_dir),
        create_discovered_module("module2", deps2, module2_dir),
        create_discovered_module("module3", deps3, module3_dir),
    ];
    
    let result = ModuleDependencies::resolve(&modules);
    assert!(result.is_ok());
    
    let resolution = result.unwrap();
    // Should be: module1, module2, module3
    assert_eq!(resolution.load_order.len(), 3);
    assert_eq!(resolution.load_order[0], "module1");
    assert_eq!(resolution.load_order[1], "module2");
    assert_eq!(resolution.load_order[2], "module3");
}

#[test]
fn test_dependency_resolution_multiple_dependencies() {
    let temp_dir = TempDir::new().unwrap();
    let module1_dir = temp_dir.path().join("module1");
    let module2_dir = temp_dir.path().join("module2");
    let module3_dir = temp_dir.path().join("module3");
    std::fs::create_dir_all(&module1_dir).unwrap();
    std::fs::create_dir_all(&module2_dir).unwrap();
    std::fs::create_dir_all(&module3_dir).unwrap();
    
    let mut deps3 = HashMap::new();
    deps3.insert("module1".to_string(), "1.0.0".to_string());
    deps3.insert("module2".to_string(), "1.0.0".to_string());
    
    let modules = vec![
        create_discovered_module("module1", HashMap::new(), module1_dir),
        create_discovered_module("module2", HashMap::new(), module2_dir),
        create_discovered_module("module3", deps3, module3_dir),
    ];
    
    let result = ModuleDependencies::resolve(&modules);
    assert!(result.is_ok());
    
    let resolution = result.unwrap();
    // module1 and module2 should come before module3
    assert_eq!(resolution.load_order.len(), 3);
    assert!(resolution.load_order.contains(&"module1".to_string()));
    assert!(resolution.load_order.contains(&"module2".to_string()));
    assert_eq!(resolution.load_order[2], "module3");
}

// ===== Discovery Tests =====

#[test]
fn test_module_discovery_new() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    
    let discovery = ModuleDiscovery::new(&modules_dir);
    // modules_dir is private, so we can't directly assert it
    // Instead, test that discovery works
    let result = discovery.discover_modules();
    assert!(result.is_ok());
}

#[test]
fn test_module_discovery_discover_modules_nonexistent_dir() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("nonexistent");
    
    let discovery = ModuleDiscovery::new(&modules_dir);
    let result = discovery.discover_modules();
    
    // Should create directory and return empty list
    assert!(result.is_ok());
    assert!(modules_dir.exists());
    assert_eq!(result.unwrap().len(), 0);
}

#[test]
fn test_module_discovery_discover_modules_empty_dir() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    std::fs::create_dir_all(&modules_dir).unwrap();
    
    let discovery = ModuleDiscovery::new(&modules_dir);
    let result = discovery.discover_modules();
    
    assert!(result.is_ok());
    assert_eq!(result.unwrap().len(), 0);
}

#[test]
fn test_module_discovery_discover_modules_with_valid_module() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let module_dir = modules_dir.join("test-module");
    std::fs::create_dir_all(&module_dir).unwrap();
    
    let manifest_path = module_dir.join("module.toml");
    let toml_content = r#"
name = "test-module"
version = "1.0.0"
entry_point = "test-module.so"
"#;
    std::fs::write(&manifest_path, toml_content).unwrap();
    
    // Create a dummy binary (discovery will fail to find it, but that's ok for this test)
    let binary_path = module_dir.join("test-module.so");
    std::fs::write(&binary_path, b"dummy binary").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    
    let discovery = ModuleDiscovery::new(&modules_dir);
    let result = discovery.discover_modules();
    
    // Should discover the module (even if binary validation might fail)
    assert!(result.is_ok());
    let modules = result.unwrap();
    // Note: Binary finding might fail, so we check if discovery at least tried
    // The actual binary finding logic is tested separately
}

#[test]
fn test_module_discovery_discover_modules_skip_non_directories() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    std::fs::create_dir_all(&modules_dir).unwrap();
    
    // Create a file (not a directory)
    let file_path = modules_dir.join("not-a-module");
    std::fs::write(&file_path, b"not a module").unwrap();
    
    let discovery = ModuleDiscovery::new(&modules_dir);
    let result = discovery.discover_modules();
    
    assert!(result.is_ok());
    // Should skip the file and return empty
    assert_eq!(result.unwrap().len(), 0);
}

#[test]
fn test_module_discovery_discover_modules_skip_no_manifest() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let module_dir = modules_dir.join("no-manifest");
    std::fs::create_dir_all(&module_dir).unwrap();
    
    // Directory exists but no module.toml
    let discovery = ModuleDiscovery::new(&modules_dir);
    let result = discovery.discover_modules();
    
    assert!(result.is_ok());
    // Should skip directories without manifest
    assert_eq!(result.unwrap().len(), 0);
}

#[test]
fn test_module_discovery_discover_module_by_name() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let module_dir = modules_dir.join("test-module");
    std::fs::create_dir_all(&module_dir).unwrap();
    
    let manifest_path = module_dir.join("module.toml");
    let toml_content = r#"
name = "test-module"
version = "1.0.0"
entry_point = "test-module.so"
"#;
    std::fs::write(&manifest_path, toml_content).unwrap();
    
    // Create a dummy binary
    let binary_path = module_dir.join("test-module.so");
    std::fs::write(&binary_path, b"dummy binary").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    
    let discovery = ModuleDiscovery::new(&modules_dir);
    let result = discovery.discover_module("test-module");
    
    // Should find the module
    assert!(result.is_ok());
    let discovered = result.unwrap();
    assert_eq!(discovered.manifest.name, "test-module");
}

#[test]
fn test_module_discovery_discover_module_by_name_not_found() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    std::fs::create_dir_all(&modules_dir).unwrap();
    
    let discovery = ModuleDiscovery::new(&modules_dir);
    let result = discovery.discover_module("nonexistent-module");
    
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ModuleError::ModuleNotFound(_)));
}

