//! ModuleManager configuration, discovery, and dependency helpers.

use blvm_node::module::manager::ModuleManager;
use blvm_node::module::traits::ModuleError;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tempfile::TempDir;

fn manager_dirs() -> (TempDir, ModuleManager) {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");
    std::fs::create_dir_all(&modules_dir).unwrap();
    let manager = ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
    (temp_dir, manager)
}

#[tokio::test]
async fn module_manager_registry_and_filter_setters() {
    let (_dir, mut manager) = manager_dirs();

    manager.set_registry_url("https://example.test/modules.json".into());
    assert_eq!(
        manager.registry_url(),
        Some("https://example.test/modules.json")
    );

    manager.set_enabled_modules(vec!["alpha".into()]);
    manager.set_disabled_modules(vec!["beta".into()]);
    manager.set_default_database_backend("rocksdb".into());
    manager.set_module_config_overrides(HashMap::from([(
        "alpha".into(),
        HashMap::from([("key".into(), "value".into())]),
    )]));
    manager
        .set_node_p2p_listen_for_modules(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444));

    assert!(manager.modules_dir().ends_with("modules"));
}

#[tokio::test]
async fn module_manager_discovery_and_info_empty() {
    let (_dir, mut manager) = manager_dirs();

    manager.auto_load_modules().await.unwrap();
    assert!(manager.list_modules().await.is_empty());
    assert!(manager.get_all_module_info().await.is_empty());
}

#[tokio::test]
async fn module_manager_dependency_helpers_without_modules() {
    let (_dir, manager) = manager_dirs();

    let dep_err = manager
        .validate_module_dependencies("missing-module")
        .await
        .unwrap_err();
    assert!(matches!(dep_err, ModuleError::OperationError(_)));

    assert!(manager
        .validate_optional_dependencies("missing-module")
        .await
        .is_empty());
    assert!(manager
        .get_dependent_modules("missing-module")
        .await
        .is_empty());
    assert!(manager.can_unload_module("missing-module").await.unwrap());
}

#[tokio::test]
async fn module_manager_api_hub_and_ipc_initially_none() {
    let (_dir, manager) = manager_dirs();
    assert!(manager.ipc_server().is_none());
    assert!(manager.api_hub_arc().is_none());
    let _events = manager.event_manager();
}

#[tokio::test]
async fn module_manager_get_module_metadata_nonexistent() {
    let (_dir, manager) = manager_dirs();
    assert!(manager.get_module_metadata("nope").await.is_none());
}

#[tokio::test]
async fn module_manager_shutdown_empty() {
    let (_dir, mut manager) = manager_dirs();
    manager.shutdown().await.unwrap();
}
