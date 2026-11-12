//! Test-line QA: a module whose required dependency is not loaded is rejected.

use std::collections::HashMap;

use blvm_node::module::manager::ModuleManager;
use blvm_node::module::traits::ModuleMetadata;

#[tokio::test]
async fn load_module_rejects_a_missing_dependency() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let mut manager = ModuleManager::new(
        dir.path().join("modules"),
        dir.path().join("data"),
        dir.path().join("sockets"),
    );
    let mut dependencies = HashMap::new();
    dependencies.insert("other".to_string(), "1.0.0".to_string());
    let metadata = ModuleMetadata {
        name: "demo".to_string(),
        version: "1.0.0".to_string(),
        description: "demo".to_string(),
        author: "qa".to_string(),
        capabilities: Vec::new(),
        rpc_overrides: Vec::new(),
        dependencies,
        optional_dependencies: HashMap::new(),
        entry_point: "demo".to_string(),
    };

    match manager
        .load_module("demo", dir.path(), metadata, HashMap::new())
        .await
    {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Required dependency 'other' not loaded"),
                "load_module returned {message}"
            );
        }
        Ok(()) => panic!("module loaded without its required dependency"),
    }
}
