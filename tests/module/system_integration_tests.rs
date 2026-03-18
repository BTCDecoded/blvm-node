//! Module system integration tests
//!
//! Tests migration run/rollback, IPC handshake, module manager, and full runtime module load.
//! Runtime module tests build and spawn the hello-module example (no compile-time dep on modules).

use blvm_node::module::api::NodeApiImpl;
use blvm_node::module::ipc::protocol::{CliSpec, RequestPayload};
use blvm_node::module::registry::discovery::ModuleDiscovery;
use blvm_node::module::traits::ModuleError;
use blvm_node::storage::Storage;
use crate::test_utils::*;
use blvm_sdk::module::{
    open_module_db, run_migrations, run_migrations_down, run_migrations_with_down,
    MigrationContext, MigrationDown, MigrationUp,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::time::{sleep, Duration};

// --- Migration tests ---

fn up_v1(_ctx: &MigrationContext) -> anyhow::Result<()> {
    Ok(())
}
fn down_v1(_ctx: &MigrationContext) -> anyhow::Result<()> {
    Ok(())
}
fn up_v2(_ctx: &MigrationContext) -> anyhow::Result<()> {
    Ok(())
}
fn down_v2(_ctx: &MigrationContext) -> anyhow::Result<()> {
    Ok(())
}

#[test]
fn test_run_migrations_up() {
    let temp = TempDir::new().unwrap();
    let db = open_module_db(temp.path()).unwrap();
    let migrations: Vec<(u32, MigrationUp)> = vec![(1, up_v1), (2, up_v2)];
    run_migrations(&db, &migrations).unwrap();

    let tree = db.open_tree("schema").unwrap();
    let v: u32 = String::from_utf8(tree.get(b"schema_version").unwrap().unwrap())
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(v, 2);
}

#[test]
fn test_run_migrations_down() {
    let temp = TempDir::new().unwrap();
    let db = open_module_db(temp.path()).unwrap();
    let migrations: &[(u32, MigrationUp, Option<MigrationDown>)] = &[
        (1, up_v1, Some(down_v1)),
        (2, up_v2, Some(down_v2)),
    ];
    run_migrations_with_down(&db, migrations).unwrap();
    run_migrations_down(&db, migrations, 1).unwrap();

    let tree = db.open_tree("schema").unwrap();
    let v: u32 = String::from_utf8(tree.get(b"schema_version").unwrap().unwrap())
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(v, 1);
}

#[test]
fn test_run_migrations_down_to_zero() {
    let temp = TempDir::new().unwrap();
    let db = open_module_db(temp.path()).unwrap();
    let migrations: &[(u32, MigrationUp, Option<MigrationDown>)] = &[
        (1, up_v1, Some(down_v1)),
        (2, up_v2, Some(down_v2)),
    ];
    run_migrations_with_down(&db, migrations).unwrap();
    run_migrations_down(&db, migrations, 0).unwrap();

    let tree = db.open_tree("schema").unwrap();
    let v: u32 = String::from_utf8(tree.get(b"schema_version").unwrap().unwrap())
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(v, 0);
}

// --- Config env overrides (tested in blvm-sdk; modules are runtime-loaded, never deps) ---

// --- IPC handshake (Unix only) ---

#[cfg(unix)]
#[tokio::test]
async fn test_ipc_handshake() {
    use blvm_node::module::api::NodeApiImpl;
    use blvm_node::module::ipc::client::ModuleIpcClient;
    use blvm_node::module::ipc::server::ModuleIpcServer;
    use blvm_node::module::ipc::protocol::{MessageType, RequestMessage};

    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("test.sock");
    std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();

    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let node_api = Arc::new(NodeApiImpl::new(storage));

    let mut server = ModuleIpcServer::new(&socket_path);
    let server_handle = tokio::spawn(async move {
        server.start(node_api).await
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut client = ModuleIpcClient::connect(&socket_path).await.unwrap();
    let req = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::Handshake,
        payload: RequestPayload::Handshake {
            module_id: "test-module".to_string(),
            module_name: "Test".to_string(),
            version: "1.0.0".to_string(),
        },
    };
    let resp = client.request(req).await.unwrap();
    assert!(resp.success);
    assert!(resp.error.is_none());

    server_handle.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn test_module_integration_connect() {
    use blvm_node::module::api::NodeApiImpl;
    use blvm_node::module::ipc::server::ModuleIpcServer;
    use blvm_node::module::integration::ModuleIntegration;

    let fixture = ModuleTestFixture::new().unwrap();
    let mut server = ModuleIpcServer::new(&fixture.socket_dir.join("test.sock"));
    let node_api = Arc::clone(&fixture.node_api);
    let server_handle = tokio::spawn(async move {
        server.start(node_api).await
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let integration = ModuleIntegration::connect(
        fixture.socket_dir.join("test.sock"),
        "test-module".to_string(),
        "Test".to_string(),
        "1.0.0".to_string(),
        Some(CliSpec {
            version: 1,
            name: "test".to_string(),
            about: None,
            subcommands: vec![],
        }),
    )
    .await
    .unwrap();

    assert_eq!(integration.module_id(), "test-module");

    server_handle.abort();
}

// --- Module manager ---

#[tokio::test]
async fn test_module_manager_config_overrides() {
    let fixture = ModuleTestFixture::new().unwrap();
    let mut overrides = HashMap::new();
    let mut mod_config = HashMap::new();
    mod_config.insert("registries".to_string(), "https://override.com".to_string());
    overrides.insert("selective-sync".to_string(), mod_config);
    fixture.module_manager.set_module_config_overrides(overrides);
    let ctx = fixture.create_test_context("selective-sync_abc");
    assert_eq!(ctx.module_id, "selective-sync_abc");
}

// --- Runtime module load (Unix only: spawns real module process) ---

/// Build hello-module example and return path to binary, or None if build fails.
fn build_hello_module_binary() -> Option<std::path::PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let workspace_root = Path::new(&manifest_dir).ancestors().nth(1)?;
    let target = workspace_root.join("target").join("debug").join("examples");
    let binary = target.join("hello-module");

    if binary.exists() {
        return Some(binary);
    }

    let status = std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "--example", "hello-module", "--package", "blvm-sdk"])
        .current_dir(workspace_root)
        .status()
        .ok()?;

    if status.success() && binary.exists() {
        Some(binary)
    } else {
        None
    }
}

/// Set up a module directory with hello-module: module.toml + optional config.toml + binary.
fn setup_hello_module_dir(
    temp_dir: &TempDir,
    binary_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    setup_hello_module_dir_with_config(temp_dir, binary_path, None)
}

fn setup_hello_module_dir_with_config(
    temp_dir: &TempDir,
    binary_path: &Path,
    config_toml: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let modules_dir = temp_dir.path().join("modules").join("hello");
    std::fs::create_dir_all(&modules_dir)?;

    let manifest = r#"
name = "hello"
version = "0.1.0"
entry_point = "hello-module"
description = "Test hello module"
author = "Test"
capabilities = ["read_blockchain"]
"#;
    std::fs::write(modules_dir.join("module.toml"), manifest)?;

    if let Some(config) = config_toml {
        std::fs::write(modules_dir.join("config.toml"), config)?;
    }

    let dest_binary = modules_dir.join("hello-module");
    std::fs::copy(binary_path, &dest_binary)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest_binary)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest_binary, perms)?;
    }

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn test_runtime_module_load_and_cli() {
    let binary = match build_hello_module_binary() {
        Some(b) => b,
        None => {
            eprintln!("Skipping: build hello-module with: cargo build -p blvm-sdk --example hello-module");
            return;
        }
    };

    let temp = TempDir::new().unwrap();
    setup_hello_module_dir(&temp, &binary).unwrap();

    let modules_dir = temp.path().join("modules");
    let data_dir = temp.path().join("data");
    let socket_dir = temp.path().join("sockets");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let node_api = Arc::new(NodeApiImpl::new(storage));

    let mut manager = blvm_node::module::manager::ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
    let socket_path = socket_dir.join("node.sock");
    manager.start(&socket_path, Arc::clone(&node_api)).await.unwrap();

    manager.auto_load_modules().await.unwrap();

    sleep(Duration::from_millis(500)).await;

    let ipc = manager.ipc_server().expect("IPC server should be available");
    let server = ipc.lock().await;

    let specs = server.get_cli_specs().await;
    let has_hello = specs.values().any(|s| s.name == "hello");
    assert!(has_hello, "hello module should register CLI; got: {:?}", specs);

    let result = server
        .invoke_cli("hello", "greet", vec!["Alice".to_string()])
        .await;

    let payload = result.expect("invoke_cli should succeed");
    match &payload {
        blvm_node::module::ipc::protocol::InvocationResultPayload::Cli { stdout, .. } => {
            assert!(stdout.contains("Alice"), "expected greeting for Alice, got: {}", stdout);
        }
        _ => panic!("expected CLI payload"),
    }

    let rpc_result = server
        .invoke_rpc("hello", "hello_greet", serde_json::json!({ "name": "Bob" }))
        .await
        .expect("invoke_rpc should succeed");
    let msg = rpc_result
        .get("message")
        .and_then(|v| v.as_str())
        .expect("expected message field");
    assert!(msg.contains("Bob"), "expected greeting for Bob, got: {}", msg);

    manager.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn test_runtime_module_discovery_and_load_order() {
    let binary = match build_hello_module_binary() {
        Some(b) => b,
        None => return,
    };

    let temp = TempDir::new().unwrap();
    setup_hello_module_dir(&temp, &binary).unwrap();
    let modules_dir = temp.path().join("modules");

    let discovery = ModuleDiscovery::new(&modules_dir);
    let discovered = discovery.discover_modules().unwrap();
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].manifest.name, "hello");
    assert!(discovered[0].binary_path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn test_runtime_module_unload_and_reload() {
    let binary = match build_hello_module_binary() {
        Some(b) => b,
        None => return,
    };

    let temp = TempDir::new().unwrap();
    setup_hello_module_dir(&temp, &binary).unwrap();
    let modules_dir = temp.path().join("modules");
    let data_dir = temp.path().join("data");
    let socket_dir = temp.path().join("sockets");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let node_api = Arc::new(NodeApiImpl::new(storage));

    let mut manager =
        blvm_node::module::manager::ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
    let socket_path = socket_dir.join("node.sock");
    manager.start(&socket_path, Arc::clone(&node_api)).await.unwrap();

    manager.auto_load_modules().await.unwrap();
    sleep(Duration::from_millis(500)).await;

    let ipc = manager.ipc_server().unwrap();
    {
        let server = ipc.lock().await;
        let result = server.invoke_cli("hello", "greet", vec!["First".to_string()]).await;
        assert!(result.is_ok());
    }

    manager.unload_module("hello").await.unwrap();
    sleep(Duration::from_millis(200)).await;

    {
        let ipc = manager.ipc_server().unwrap();
        let server = ipc.lock().await;
        let specs = server.get_cli_specs().await;
        assert!(!specs.values().any(|s| s.name == "hello"), "hello should be unloaded");
    }

    let discovery = ModuleDiscovery::new(&modules_dir);
    let discovered = discovery.discover_modules().unwrap();
    let hello_mod = discovered.iter().find(|m| m.manifest.name == "hello").unwrap();
    let config = std::collections::HashMap::new();
    manager
        .load_module(
            "hello",
            &hello_mod.binary_path,
            hello_mod.manifest.to_metadata(),
            config,
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(500)).await;

    {
        let server = ipc.lock().await;
        let result = server.invoke_cli("hello", "greet", vec!["Reloaded".to_string()]).await;
        let payload = result.expect("invoke after reload");
        match payload {
            blvm_node::module::ipc::protocol::InvocationResultPayload::Cli { stdout, .. } => {
                assert!(stdout.contains("Reloaded"));
            }
            _ => panic!("expected CLI payload"),
        }
    }

    manager.shutdown().await.unwrap();
}

/// Build demo-module example and return path to binary, or None if build fails.
fn build_demo_module_binary() -> Option<std::path::PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let workspace_root = Path::new(&manifest_dir).ancestors().nth(1)?;
    let target = workspace_root.join("target").join("debug").join("examples");
    let binary = target.join("demo-module");

    if binary.exists() {
        return Some(binary);
    }

    let status = std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "--example", "demo-module", "--package", "blvm-sdk"])
        .current_dir(workspace_root)
        .status()
        .ok()?;

    if status.success() && binary.exists() {
        Some(binary)
    } else {
        None
    }
}

/// Set up a module directory with demo-module: module.toml + binary.
fn setup_demo_module_dir(
    temp_dir: &TempDir,
    binary_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let modules_dir = temp_dir.path().join("modules").join("demo");
    std::fs::create_dir_all(&modules_dir)?;

    let manifest = r#"
name = "demo"
version = "0.1.0"
entry_point = "demo-module"
description = "Demo module with CLI, RPC, events"
author = "Test"
capabilities = ["read_blockchain"]
"#;
    std::fs::write(modules_dir.join("module.toml"), manifest)?;

    let dest_binary = modules_dir.join("demo-module");
    std::fs::copy(binary_path, &dest_binary)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest_binary)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest_binary, perms)?;
    }

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn test_runtime_demo_module_cli_and_rpc() {
    let binary = match build_demo_module_binary() {
        Some(b) => b,
        None => {
            eprintln!("Skipping: build demo-module with: cargo build -p blvm-sdk --example demo-module");
            return;
        }
    };

    let temp = TempDir::new().unwrap();
    setup_demo_module_dir(&temp, &binary).unwrap();

    let modules_dir = temp.path().join("modules");
    let data_dir = temp.path().join("data");
    let socket_dir = temp.path().join("sockets");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let node_api = Arc::new(NodeApiImpl::new(storage));

    let mut manager =
        blvm_node::module::manager::ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
    let socket_path = socket_dir.join("node.sock");
    manager.start(&socket_path, Arc::clone(&node_api)).await.unwrap();

    manager.auto_load_modules().await.unwrap();

    sleep(Duration::from_millis(500)).await;

    let ipc = manager.ipc_server().expect("IPC server should be available");
    let server = ipc.lock().await;

    let specs = server.get_cli_specs().await;
    let has_demo = specs.values().any(|s| s.name == "demo");
    assert!(has_demo, "demo module should register CLI; got: {:?}", specs);

    // CLI: set key=value
    let result = server
        .invoke_cli("demo", "set", vec!["--key".to_string(), "k1".to_string(), "--value".to_string(), "v1".to_string()])
        .await;
    let payload = result.expect("invoke_cli set should succeed");
    match &payload {
        blvm_node::module::ipc::protocol::InvocationResultPayload::Cli { stdout, .. } => {
            assert!(stdout.contains("Set k1=v1"), "expected set output, got: {}", stdout);
        }
        _ => panic!("expected CLI payload"),
    }

    // CLI: get key
    let result = server.invoke_cli("demo", "get", vec!["--key".to_string(), "k1".to_string()]).await;
    let payload = result.expect("invoke_cli get should succeed");
    match &payload {
        blvm_node::module::ipc::protocol::InvocationResultPayload::Cli { stdout, .. } => {
            assert!(stdout.contains("k1=v1"), "expected get output, got: {}", stdout);
        }
        _ => panic!("expected CLI payload"),
    }

    // CLI: list
    let result = server.invoke_cli("demo", "list", vec![]).await;
    let payload = result.expect("invoke_cli list should succeed");
    match &payload {
        blvm_node::module::ipc::protocol::InvocationResultPayload::Cli { stdout, .. } => {
            assert!(stdout.contains("k1=v1"), "expected list output, got: {}", stdout);
        }
        _ => panic!("expected CLI payload"),
    }

    // RPC: demo_set
    let rpc_result = server
        .invoke_rpc("demo", "demo_set", serde_json::json!({ "key": "rpc_k", "value": "rpc_v" }))
        .await
        .expect("invoke_rpc demo_set should succeed");
    assert!(rpc_result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));

    // RPC: demo_get
    let rpc_result = server
        .invoke_rpc("demo", "demo_get", serde_json::json!({ "key": "rpc_k" }))
        .await
        .expect("invoke_rpc demo_get should succeed");
    let value = rpc_result.get("value").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(value, "rpc_v", "expected rpc_v, got: {:?}", rpc_result);

    manager.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn test_config_merge_file_and_overrides() {
    let binary = match build_hello_module_binary() {
        Some(b) => b,
        None => return,
    };

    let temp = TempDir::new().unwrap();
    setup_hello_module_dir_with_config(
        &temp,
        &binary,
        Some(r#"
greeting = "from_file"
"#),
    )
    .unwrap();

    let modules_dir = temp.path().join("modules");
    let data_dir = temp.path().join("data");
    let socket_dir = temp.path().join("sockets");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let mut overrides = HashMap::new();
    let mut mod_config = HashMap::new();
    mod_config.insert("greeting".to_string(), "from_override".to_string());
    overrides.insert("hello".to_string(), mod_config);

    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let node_api = Arc::new(NodeApiImpl::new(storage));

    let mut manager =
        blvm_node::module::manager::ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
    manager.set_module_config_overrides(overrides);

    let socket_path = socket_dir.join("node.sock");
    manager.start(&socket_path, Arc::clone(&node_api)).await.unwrap();

    manager.auto_load_modules().await.unwrap();
    sleep(Duration::from_millis(500)).await;

    let ipc = manager.ipc_server().unwrap();
    let server = ipc.lock().await;
    let result = server.invoke_cli("hello", "greet", vec![]).await;
    assert!(result.is_ok(), "module with merged config should respond");

    manager.shutdown().await.unwrap();
}
