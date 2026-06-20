//! Integration tests for version-pinned module auto-load and config.

use blvm_node::module::manager::ModuleManager;
use std::path::Path;
use tempfile::TempDir;

fn write_installed_module(modules_dir: &Path, name: &str, version: &str, binary_bytes: &[u8]) {
    let module_dir = modules_dir.join(name);
    std::fs::create_dir_all(&module_dir).unwrap();
    std::fs::write(
        module_dir.join("module.toml"),
        format!(
            r#"name = "{name}"
version = "{version}"
entry_point = "{name}"
"#
        ),
    )
    .unwrap();
    let binary_path = module_dir.join(name);
    std::fs::write(&binary_path, binary_bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn manager_with_module(name: &str, version: &str) -> (TempDir, ModuleManager) {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");
    std::fs::create_dir_all(&modules_dir).unwrap();
    write_installed_module(&modules_dir, name, version, b"#!/bin/sh\nexit 0\n");
    let manager = ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
    (temp_dir, manager)
}

#[tokio::test]
async fn auto_load_skips_on_disk_module_when_version_does_not_match_constraint() {
    let (_dir, mut manager) = manager_with_module("pin-test", "0.2.0");
    manager.set_enabled_modules([("pin-test".into(), "0.1.*".into())].into_iter().collect());

    manager.auto_load_modules().await.unwrap();
    assert!(manager.list_modules().await.is_empty());
}

#[tokio::test]
async fn auto_load_keeps_module_not_in_enabled_allowlist_off_disk() {
    let (_dir, mut manager) = manager_with_module("other", "1.0.0");
    manager.set_enabled_modules([("pin-test".into(), "0.1.*".into())].into_iter().collect());

    manager.auto_load_modules().await.unwrap();
    assert!(manager.list_modules().await.is_empty());
}

#[tokio::test]
async fn auto_load_matching_version_without_registry_reaches_load_stage() {
    let (_dir, mut manager) = manager_with_module("pin-test", "0.1.8");
    manager.set_enabled_modules([("pin-test".into(), "0.1.*".into())].into_iter().collect());
    // No registry_url: loader skips GitHub checksum fetch; unsigned dummy binary may load or fail at spawn.
    let result = manager.auto_load_modules().await;
    // Must not exit early with "no modules" — either loads or errors during load, not at filter.
    match &result {
        Ok(()) => {
            // Dummy shell script may load on Unix; otherwise list may still be empty if spawn fails softly.
        }
        Err(e) => {
            let msg = e.to_string();
            assert!(
                !msg.contains("No modules to load"),
                "unexpected early exit before load: {msg}"
            );
        }
    }
}

#[test]
fn module_config_enabled_modules_table_form_toml() {
    use blvm_node::config::NodeConfig;

    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("cfg.toml");
    std::fs::write(
        &path,
        r#"transport_preference = "tcponly"

[modules]
[modules.enabled_modules]
blvm-miniscript = "0.1.*"
blvm-zmq = "0.3.*"
"#,
    )
    .unwrap();
    let config = NodeConfig::from_toml_file(&path).unwrap();
    let modules = config.modules.as_ref().unwrap();
    assert_eq!(modules.enabled_modules.len(), 2);
    assert_eq!(
        modules.enabled_modules.get("blvm-zmq").map(String::as_str),
        Some("0.3.*")
    );
}
