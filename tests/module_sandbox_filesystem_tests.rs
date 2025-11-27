//! Tests for filesystem sandbox

use bllvm_node::module::sandbox::FileSystemSandbox;
use bllvm_node::module::traits::ModuleError;
use std::path::PathBuf;
use tempfile::TempDir;

fn create_test_sandbox() -> (TempDir, FileSystemSandbox) {
    let temp_dir = TempDir::new().unwrap();
    let data_dir = temp_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let sandbox = FileSystemSandbox::new(&data_dir);
    (temp_dir, sandbox)
}

#[test]
fn test_filesystem_sandbox_creation() {
    let (_temp_dir, _sandbox) = create_test_sandbox();
    // Should create successfully
    assert!(true);
}

#[test]
fn test_validate_path_within_sandbox() {
    let (temp_dir, sandbox) = create_test_sandbox();
    let data_dir = temp_dir.path().join("data");
    let test_file = data_dir.join("test.txt");

    // Create a test file
    std::fs::write(&test_file, "test").unwrap();

    // Validate path should succeed
    let result = sandbox.validate_path(&test_file);
    assert!(result.is_ok());

    let validated_path = result.unwrap();
    assert!(validated_path.exists());
}

#[test]
fn test_validate_path_outside_sandbox() {
    let (temp_dir, sandbox) = create_test_sandbox();
    let outside_file = temp_dir.path().join("outside.txt");

    // Create a file outside the sandbox
    std::fs::write(&outside_file, "test").unwrap();

    // Validate path should fail
    let result = sandbox.validate_path(&outside_file);
    assert!(result.is_err());

    match result.unwrap_err() {
        ModuleError::OperationError(msg) => {
            assert!(msg.contains("outside") || msg.contains("denied"));
        }
        _ => panic!("Expected OperationError"),
    }
}

#[test]
fn test_validate_path_relative() {
    let (temp_dir, sandbox) = create_test_sandbox();
    let data_dir = temp_dir.path().join("data");
    let test_file = data_dir.join("test.txt");

    // Create a test file
    std::fs::write(&test_file, "test").unwrap();

    // Change to data directory and use relative path
    let original_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(&data_dir).unwrap();

    // Validate relative path
    let result = sandbox.validate_path("test.txt");
    assert!(result.is_ok());

    // Restore original directory
    std::env::set_current_dir(original_dir).unwrap();
}

#[test]
fn test_validate_path_nonexistent() {
    let (temp_dir, sandbox) = create_test_sandbox();
    let data_dir = temp_dir.path().join("data");
    let nonexistent = data_dir.join("nonexistent.txt");

    // Validate nonexistent path should fail (canonicalize will fail)
    let result = sandbox.validate_path(&nonexistent);
    assert!(result.is_err());
}

#[test]
fn test_is_within_sandbox() {
    let (temp_dir, sandbox) = create_test_sandbox();
    let data_dir = temp_dir.path().join("data");
    let test_file = data_dir.join("test.txt");

    // Create a test file
    std::fs::write(&test_file, "test").unwrap();

    // Should be within sandbox
    assert!(sandbox.is_within_sandbox(&test_file));

    // Path outside should not be within sandbox
    let outside_file = temp_dir.path().join("outside.txt");
    assert!(!sandbox.is_within_sandbox(&outside_file));
}

#[test]
fn test_is_within_sandbox_relative() {
    let (temp_dir, sandbox) = create_test_sandbox();
    let data_dir = temp_dir.path().join("data");

    // Relative path within data directory
    let relative_path = data_dir.join("subdir").join("file.txt");
    std::fs::create_dir_all(relative_path.parent().unwrap()).unwrap();
    std::fs::write(&relative_path, "test").unwrap();

    // Should be within sandbox
    assert!(sandbox.is_within_sandbox(&relative_path));
}

#[test]
fn test_allowed_path() {
    let (temp_dir, sandbox) = create_test_sandbox();
    let data_dir = temp_dir.path().join("data");

    // Should return the allowed path
    assert_eq!(sandbox.allowed_path(), data_dir);
}

#[test]
fn test_validate_path_subdirectory() {
    let (temp_dir, sandbox) = create_test_sandbox();
    let data_dir = temp_dir.path().join("data");
    let subdir = data_dir.join("subdir");
    let test_file = subdir.join("test.txt");

    // Create subdirectory and file
    std::fs::create_dir_all(&subdir).unwrap();
    std::fs::write(&test_file, "test").unwrap();

    // Should validate successfully
    let result = sandbox.validate_path(&test_file);
    assert!(result.is_ok());
}

#[test]
fn test_validate_path_parent_directory_traversal() {
    let (temp_dir, sandbox) = create_test_sandbox();
    let data_dir = temp_dir.path().join("data");
    let subdir = data_dir.join("subdir");

    // Create subdirectory
    std::fs::create_dir_all(&subdir).unwrap();

    // Try to access parent directory using ../
    let traversal_path = subdir.join("..").join("outside.txt");
    let outside_file = temp_dir.path().join("outside.txt");
    std::fs::write(&outside_file, "test").unwrap();

    // Should fail (canonicalize should resolve to outside sandbox)
    let result = sandbox.validate_path(&traversal_path);
    assert!(result.is_err());
}
