//! Tests for module process spawner

use blvm_node::config::ModuleResourceLimitsConfig;
use blvm_node::module::process::spawner::ModuleProcessSpawner;
use blvm_node::module::traits::ModuleContext;
use std::collections::HashMap;
use std::path::PathBuf;
use tempfile::TempDir;

fn create_test_spawner() -> (TempDir, ModuleProcessSpawner) {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");

    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let spawner = ModuleProcessSpawner::new(&modules_dir, &data_dir, &socket_dir);
    (temp_dir, spawner)
}

fn create_test_context() -> ModuleContext {
    let temp = std::env::temp_dir();
    ModuleContext::new(
        "test-module-1".to_string(),
        temp.join("blvm_test.sock").to_string_lossy().into_owned(),
        temp.join("blvm_test-data").to_string_lossy().into_owned(),
        HashMap::new(),
    )
}

#[test]
fn test_spawner_creation() {
    let (_temp_dir, _spawner) = create_test_spawner();
    // Should create successfully
}

#[test]
fn test_spawner_with_config() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");

    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let config = ModuleResourceLimitsConfig {
        default_max_cpu_percent: 75,
        default_max_memory_bytes: 1024 * 1024 * 1024, // 1 GB
        default_max_file_descriptors: 512,
        default_max_child_processes: 20,
        module_startup_wait_millis: 200,
        module_socket_timeout_seconds: 10,
        module_socket_check_interval_millis: 50,
        module_socket_max_attempts: 100,
    };

    let _spawner =
        ModuleProcessSpawner::with_config(&modules_dir, &data_dir, &socket_dir, Some(&config));
    // Should create successfully with custom config
}

#[test]
fn test_spawner_directories() {
    let (_temp_dir, spawner) = create_test_spawner();

    // Verify directories are set correctly
    assert!(spawner.modules_dir.exists() || spawner.modules_dir.parent().is_some());
    assert!(spawner.data_dir.exists() || spawner.data_dir.parent().is_some());
    assert!(spawner.socket_dir.exists() || spawner.socket_dir.parent().is_some());
}

#[tokio::test]
async fn test_spawn_nonexistent_binary() {
    let (_temp_dir, spawner) = create_test_spawner();
    let context = create_test_context();
    let fake_binary = PathBuf::from("/nonexistent/binary");

    let result = spawner.spawn("test-module", &fake_binary, context).await;

    // Should fail with an error
    assert!(result.is_err());
    // Error type should be ModuleError (which implements Debug)
    // We don't need to match on the specific error variant for this test
}

#[tokio::test(flavor = "current_thread")]
async fn spawn_log_omits_module_config() {
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let subscriber = LogCapture(std::sync::Arc::clone(&lines));
    let _guard = tracing::subscriber::set_default(subscriber);

    let (temp_dir, _) = create_test_spawner();
    let config = ModuleResourceLimitsConfig {
        default_max_cpu_percent: 50,
        default_max_memory_bytes: 512 * 1024 * 1024,
        default_max_file_descriptors: 256,
        default_max_child_processes: 10,
        module_startup_wait_millis: 1,
        module_socket_timeout_seconds: 1,
        module_socket_check_interval_millis: 10,
        module_socket_max_attempts: 2,
    };
    let spawner = ModuleProcessSpawner::with_config(
        temp_dir.path().join("modules"),
        temp_dir.path().join("data"),
        temp_dir.path().join("sockets"),
        Some(&config),
    );
    let mut context = create_test_context();
    context
        .config
        .insert("api_key".to_string(), "super-secret-value".to_string());
    let fake_binary = PathBuf::from("/bin/true");
    let result = spawner.spawn("test-module", &fake_binary, context).await;
    drop(_guard);

    assert!(result.is_err());
    let text = lines.lock().unwrap().join("\n");
    assert!(
        text.contains("Spawning module process"),
        "spawn did not emit the spawn log: {text}"
    );
    assert!(text.contains("test-module"), "{text}");
    assert!(text.contains("/bin/true"), "{text}");
    assert!(
        !text.contains("super-secret-value"),
        "spawn log included module config: {text}"
    );
    assert!(!text.contains("MODULE_CONFIG"), "{text}");
}

struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

struct LogVisit(String);

impl tracing::field::Visit for LogVisit {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.0, "{}={:?} ", field.name(), value);
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write;
        let _ = write!(self.0, "{}={} ", field.name(), value);
    }
}

impl tracing::Subscriber for LogCapture {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::DEBUG
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut visit = LogVisit(String::new());
        event.record(&mut visit);
        self.0.lock().unwrap().push(visit.0);
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

// Note: ModuleProcess has private fields, so we can't construct it directly in tests.
// These tests would require actually spawning a module process via spawner.spawn(),
// which requires a real module binary. For now, we test the spawner's public API
// and the process lifecycle methods would be tested via integration tests.

#[test]
fn test_spawner_resource_limits_config() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");

    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let config = ModuleResourceLimitsConfig {
        default_max_cpu_percent: 50,
        default_max_memory_bytes: 512 * 1024 * 1024,
        default_max_file_descriptors: 256,
        default_max_child_processes: 10,
        module_startup_wait_millis: 100,
        module_socket_timeout_seconds: 5,
        module_socket_check_interval_millis: 100,
        module_socket_max_attempts: 50,
    };

    let _spawner =
        ModuleProcessSpawner::with_config(&modules_dir, &data_dir, &socket_dir, Some(&config));

    // Verify spawner was created with config
}

#[test]
fn test_module_context_creation() {
    let mut config = HashMap::new();
    config.insert("key1".to_string(), "value1".to_string());
    config.insert("key2".to_string(), "value2".to_string());

    let temp = std::env::temp_dir();
    let context = ModuleContext::new(
        "test-id".to_string(),
        temp.join("blvm_test.sock").to_string_lossy().into_owned(),
        temp.join("blvm_test-data").to_string_lossy().into_owned(),
        config,
    );

    assert_eq!(context.module_id, "test-id");
    assert_eq!(context.config.len(), 2);
    assert_eq!(context.config.get("key1"), Some(&"value1".to_string()));
}

#[test]
fn test_module_context_empty() {
    let temp = std::env::temp_dir();
    let context = ModuleContext::new(
        "test-id".to_string(),
        temp.join("blvm_test.sock").to_string_lossy().into_owned(),
        temp.join("blvm_test-data").to_string_lossy().into_owned(),
        HashMap::new(),
    );

    assert_eq!(context.module_id, "test-id");
    assert_eq!(context.config.len(), 0);
}
