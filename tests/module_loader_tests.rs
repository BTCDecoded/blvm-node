//! Tests for module loader

use bllvm_node::module::loader::ModuleLoader;
use tempfile::TempDir;

// Note: ModuleManifest and DiscoveredModule construction is complex
// These tests focus on the config loading functionality which is testable in isolation

#[test]
fn test_module_loader_load_module_config_nonexistent_file() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("nonexistent.toml");

    let result = ModuleLoader::load_module_config("test-module", &config_path);
    assert!(result.is_ok());

    let config = result.unwrap();
    assert!(config.is_empty());
}

#[test]
fn test_module_loader_load_module_config_toml() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("config.toml");

    let toml_content = r#"
key1 = "value1"
key2 = 42
key3 = true
"#;
    std::fs::write(&config_path, toml_content).unwrap();

    let result = ModuleLoader::load_module_config("test-module", &config_path);
    assert!(result.is_ok());

    let config = result.unwrap();
    assert_eq!(config.get("key1"), Some(&"value1".to_string()));
    assert_eq!(config.get("key2"), Some(&"42".to_string()));
    assert_eq!(config.get("key3"), Some(&"true".to_string()));
}

#[test]
fn test_module_loader_load_module_config_key_value_format() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("config.conf");

    let kv_content = r#"
# Comment line
key1=value1
key2 = value2
key3=value with spaces
"#;
    std::fs::write(&config_path, kv_content).unwrap();

    let result = ModuleLoader::load_module_config("test-module", &config_path);
    assert!(result.is_ok());

    let config = result.unwrap();
    assert_eq!(config.get("key1"), Some(&"value1".to_string()));
    assert_eq!(config.get("key2"), Some(&"value2".to_string()));
    assert_eq!(config.get("key3"), Some(&"value with spaces".to_string()));
}

#[test]
fn test_module_loader_load_module_config_empty_file() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("empty.toml");

    std::fs::write(&config_path, "").unwrap();

    let result = ModuleLoader::load_module_config("test-module", &config_path);
    assert!(result.is_ok());

    let config = result.unwrap();
    assert!(config.is_empty());
}

#[test]
fn test_module_loader_load_module_config_toml_with_nested_table() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("nested.toml");

    let toml_content = r#"
[section]
key1 = "value1"
key2 = 42
"#;
    std::fs::write(&config_path, toml_content).unwrap();

    let result = ModuleLoader::load_module_config("test-module", &config_path);
    assert!(result.is_ok());

    let config = result.unwrap();
    // Nested tables are converted to dot-notation
    assert!(config.contains_key("section.key1") || config.contains_key("section"));
}

#[test]
fn test_module_loader_load_module_config_toml_with_array() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("array.toml");

    let toml_content = r#"
items = ["item1", "item2", "item3"]
"#;
    std::fs::write(&config_path, toml_content).unwrap();

    let result = ModuleLoader::load_module_config("test-module", &config_path);
    assert!(result.is_ok());

    let config = result.unwrap();
    let items = config.get("items");
    assert!(items.is_some());
    // Arrays are joined with commas
    assert!(items.unwrap().contains("item1"));
}

#[test]
fn test_module_loader_load_module_config_key_value_with_comments() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("comments.conf");

    let kv_content = r#"
# This is a comment
key1=value1
# Another comment
key2=value2
"#;
    std::fs::write(&config_path, kv_content).unwrap();

    let result = ModuleLoader::load_module_config("test-module", &config_path);
    assert!(result.is_ok());

    let config = result.unwrap();
    assert_eq!(config.get("key1"), Some(&"value1".to_string()));
    assert_eq!(config.get("key2"), Some(&"value2".to_string()));
    // Comments should be ignored
    assert!(!config.contains_key("#"));
}

#[test]
fn test_module_loader_load_module_config_key_value_empty_lines() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("empty_lines.conf");

    let kv_content = r#"
key1=value1

key2=value2

"#;
    std::fs::write(&config_path, kv_content).unwrap();

    let result = ModuleLoader::load_module_config("test-module", &config_path);
    assert!(result.is_ok());

    let config = result.unwrap();
    assert_eq!(config.get("key1"), Some(&"value1".to_string()));
    assert_eq!(config.get("key2"), Some(&"value2".to_string()));
}
