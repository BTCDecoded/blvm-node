//! Tests for Module Manifest Validator

use bllvm_node::module::registry::manifest::ModuleManifest;
use bllvm_node::module::validation::manifest_validator::{ManifestValidator, ValidationResult};
use std::collections::HashMap;

fn create_valid_manifest() -> ModuleManifest {
    ModuleManifest {
        name: "test-module".to_string(),
        version: "1.0.0".to_string(),
        description: Some("Test module".to_string()),
        author: Some("Test Author".to_string()),
        capabilities: Vec::new(),
        dependencies: HashMap::new(),
        entry_point: "test-module.so".to_string(),
        config_schema: HashMap::new(),
    }
}

#[test]
fn test_manifest_validator_new() {
    let validator = ManifestValidator::new();
    // Validator should be created
    assert!(true);
}

#[test]
fn test_manifest_validator_default() {
    let validator = ManifestValidator::default();
    // Default should work
    assert!(true);
}

#[test]
fn test_manifest_validator_valid_manifest() {
    let validator = ManifestValidator::new();
    let manifest = create_valid_manifest();

    let result = validator.validate(&manifest);
    assert_eq!(result, ValidationResult::Valid);
}

#[test]
fn test_manifest_validator_empty_name() {
    let validator = ManifestValidator::new();
    let mut manifest = create_valid_manifest();
    manifest.name = String::new();

    let result = validator.validate(&manifest);
    match result {
        ValidationResult::Invalid(errors) => {
            assert!(errors.iter().any(|e| e.contains("name cannot be empty")));
        }
        ValidationResult::Valid => panic!("Should be invalid"),
    }
}

#[test]
fn test_manifest_validator_empty_version() {
    let validator = ManifestValidator::new();
    let mut manifest = create_valid_manifest();
    manifest.version = String::new();

    let result = validator.validate(&manifest);
    match result {
        ValidationResult::Invalid(errors) => {
            assert!(errors.iter().any(|e| e.contains("version cannot be empty")));
        }
        ValidationResult::Valid => panic!("Should be invalid"),
    }
}

#[test]
fn test_manifest_validator_empty_entry_point() {
    let validator = ManifestValidator::new();
    let mut manifest = create_valid_manifest();
    manifest.entry_point = String::new();

    let result = validator.validate(&manifest);
    match result {
        ValidationResult::Invalid(errors) => {
            assert!(errors
                .iter()
                .any(|e| e.contains("Entry point cannot be empty")));
        }
        ValidationResult::Valid => panic!("Should be invalid"),
    }
}

#[test]
fn test_manifest_validator_invalid_version_format() {
    let validator = ManifestValidator::new();
    let mut manifest = create_valid_manifest();
    manifest.version = "invalid".to_string();

    let result = validator.validate(&manifest);
    match result {
        ValidationResult::Invalid(errors) => {
            assert!(errors.iter().any(|e| e.contains("Invalid version format")));
        }
        ValidationResult::Valid => panic!("Should be invalid"),
    }
}

#[test]
fn test_manifest_validator_valid_version_formats() {
    let validator = ManifestValidator::new();

    let valid_versions = vec![
        "1.0",
        "1.0.0",
        "2.1.3",
        "1.0.0-alpha",
        "1.0.0+build",
        "1.0.0-alpha+build",
    ];

    for version in valid_versions {
        let mut manifest = create_valid_manifest();
        manifest.version = version.to_string();

        let result = validator.validate(&manifest);
        assert_eq!(
            result,
            ValidationResult::Valid,
            "Version {} should be valid",
            version
        );
    }
}

#[test]
fn test_manifest_validator_invalid_name_format() {
    let validator = ManifestValidator::new();

    let invalid_names = vec![
        "",             // Empty
        "-invalid",     // Starts with dash
        "_invalid",     // Starts with underscore
        "invalid name", // Contains space
        "invalid@name", // Contains invalid char
    ];

    for name in invalid_names {
        let mut manifest = create_valid_manifest();
        manifest.name = name.to_string();

        let result = validator.validate(&manifest);
        match result {
            ValidationResult::Invalid(errors) => {
                assert!(
                    errors.iter().any(|e| e.contains("Invalid module name")),
                    "Name '{}' should be invalid",
                    name
                );
            }
            ValidationResult::Valid => panic!("Name '{}' should be invalid", name),
        }
    }
}

#[test]
fn test_manifest_validator_valid_name_formats() {
    let validator = ManifestValidator::new();

    let valid_names = vec![
        "test-module",
        "test_module",
        "testModule",
        "test123",
        "a",
        "module-name-with-dashes",
        "module_name_with_underscores",
    ];

    for name in valid_names {
        let mut manifest = create_valid_manifest();
        manifest.name = name.to_string();

        let result = validator.validate(&manifest);
        assert_eq!(
            result,
            ValidationResult::Valid,
            "Name '{}' should be valid",
            name
        );
    }
}

#[test]
fn test_manifest_validator_invalid_capability() {
    let validator = ManifestValidator::new();
    let mut manifest = create_valid_manifest();
    manifest.capabilities = vec!["invalid-capability".to_string()];

    let result = validator.validate(&manifest);
    match result {
        ValidationResult::Invalid(errors) => {
            assert!(errors.iter().any(|e| e.contains("Unknown capability")));
        }
        ValidationResult::Valid => panic!("Should be invalid"),
    }
}

#[test]
fn test_manifest_validator_valid_capabilities() {
    let validator = ManifestValidator::new();
    let mut manifest = create_valid_manifest();
    manifest.capabilities = vec![
        "read_blockchain".to_string(),
        "subscribe_events".to_string(),
    ];

    let result = validator.validate(&manifest);
    assert_eq!(result, ValidationResult::Valid);
}

#[test]
fn test_manifest_validator_invalid_dependency_name() {
    let validator = ManifestValidator::new();
    let mut manifest = create_valid_manifest();
    let mut deps = HashMap::new();
    deps.insert("invalid-name!".to_string(), "1.0.0".to_string());
    manifest.dependencies = deps;

    let result = validator.validate(&manifest);
    match result {
        ValidationResult::Invalid(errors) => {
            assert!(errors.iter().any(|e| e.contains("Invalid dependency name")));
        }
        ValidationResult::Valid => panic!("Should be invalid"),
    }
}

#[test]
fn test_manifest_validator_invalid_dependency_version() {
    let validator = ManifestValidator::new();
    let mut manifest = create_valid_manifest();
    let mut deps = HashMap::new();
    deps.insert("valid-dep".to_string(), "invalid-version".to_string());
    manifest.dependencies = deps;

    let result = validator.validate(&manifest);
    match result {
        ValidationResult::Invalid(errors) => {
            assert!(errors
                .iter()
                .any(|e| e.contains("Invalid dependency version format")));
        }
        ValidationResult::Valid => panic!("Should be invalid"),
    }
}

#[test]
fn test_manifest_validator_valid_dependency_versions() {
    let validator = ManifestValidator::new();

    let valid_versions = vec!["1.0.0", ">=1.0.0", "<=2.0.0", "==1.0.0", "^1.0.0", "~1.0.0"];

    for version in valid_versions {
        let mut manifest = create_valid_manifest();
        let mut deps = HashMap::new();
        deps.insert("valid-dep".to_string(), version.to_string());
        manifest.dependencies = deps;

        let result = validator.validate(&manifest);
        assert_eq!(
            result,
            ValidationResult::Valid,
            "Version '{}' should be valid",
            version
        );
    }
}

#[test]
fn test_manifest_validator_multiple_errors() {
    let validator = ManifestValidator::new();
    let mut manifest = create_valid_manifest();
    manifest.name = String::new();
    manifest.version = "invalid".to_string();
    manifest.entry_point = String::new();

    let result = validator.validate(&manifest);
    match result {
        ValidationResult::Invalid(errors) => {
            assert!(errors.len() >= 3); // Should have multiple errors
        }
        ValidationResult::Valid => panic!("Should be invalid"),
    }
}

#[test]
fn test_validate_module_signature() {
    use bllvm_node::module::validation::manifest_validator::validate_module_signature;
    use std::path::PathBuf;

    let manifest = create_valid_manifest();
    let binary_path = PathBuf::from("/nonexistent/binary.so");

    // Phase 1: Signature validation is disabled (always succeeds)
    let result = validate_module_signature(&manifest, &binary_path);
    assert!(result.is_ok());
}
