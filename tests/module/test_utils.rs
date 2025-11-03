//! Test utilities for module system testing
//!
//! Provides helpers for creating test modules, mock modules, and test infrastructure.

use std::path::{Path, PathBuf};
use std::collections::HashMap;
use tempfile::TempDir;
use tokio::time::{sleep, Duration};
use tracing::{debug, info};

use reference_node::module::traits::ModuleContext;
use reference_node::module::manager::ModuleManager;
use reference_node::module::api::NodeApiImpl;
use reference_node::module::registry::manifest::ModuleManifest;
use reference_node::storage::Storage;
use std::sync::Arc;

/// Test fixture for module system tests
pub struct ModuleTestFixture {
    /// Temporary directory for test data
    pub temp_dir: TempDir,
    /// Module manager instance
    pub module_manager: ModuleManager,
    /// Storage instance for node API
    pub storage: Arc<Storage>,
    /// Node API implementation
    pub node_api: Arc<NodeApiImpl>,
    /// Modules directory path
    pub modules_dir: PathBuf,
    /// Data directory path
    pub data_dir: PathBuf,
    /// Socket directory path
    pub socket_dir: PathBuf,
}

impl ModuleTestFixture {
    /// Create a new test fixture with isolated directories
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        
        let modules_dir = temp_dir.path().join("modules");
        let data_dir = temp_dir.path().join("data");
        let socket_dir = temp_dir.path().join("sockets");
        
        std::fs::create_dir_all(&modules_dir)?;
        std::fs::create_dir_all(&data_dir)?;
        std::fs::create_dir_all(&socket_dir)?;
        
        // Create storage for node API
        let storage = Arc::new(Storage::new(data_dir.as_ref())?);
        let node_api = Arc::new(NodeApiImpl::new(storage.clone()));
        
        // Create module manager
        let module_manager = ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
        
        Ok(Self {
            temp_dir,
            module_manager,
            storage,
            node_api,
            modules_dir,
            data_dir,
            socket_dir,
        })
    }
    
    /// Create a test module manifest
    pub fn create_test_manifest<P: AsRef<Path>>(
        &self,
        module_dir: P,
        name: &str,
        version: &str,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let module_dir = module_dir.as_ref();
        std::fs::create_dir_all(module_dir)?;
        
        let manifest_path = module_dir.join("module.toml");
        let manifest = ModuleManifest {
            name: name.to_string(),
            version: version.to_string(),
            description: Some(format!("Test module: {}", name)),
            author: Some("Test".to_string()),
            capabilities: vec!["read_blockchain".to_string()],
            dependencies: HashMap::new(),
            entry_point: name.to_string(),
            config_schema: HashMap::new(),
        };
        
        let toml_content = toml::to_string_pretty(&manifest)?;
        std::fs::write(&manifest_path, toml_content)?;
        
        Ok(manifest_path)
    }
    
    /// Create a test module context
    pub fn create_test_context(&self, module_id: &str) -> ModuleContext {
        let socket_path = self.socket_dir.join(format!("{}.sock", module_id));
        
        ModuleContext::new(
            module_id.to_string(),
            socket_path.to_string_lossy().to_string(),
            self.data_dir.join(module_id).to_string_lossy().to_string(),
            HashMap::new(),
        )
    }
}

impl Default for ModuleTestFixture {
    fn default() -> Self {
        Self::new().expect("Failed to create test fixture")
    }
}

/// Mock module for testing
///
/// This is a simple module implementation that can be used in tests
/// to verify module system functionality without needing a real module binary.
pub struct MockModule {
    pub name: String,
    pub state: String,
    pub events_received: usize,
}

impl MockModule {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: "stopped".to_string(),
            events_received: 0,
        }
    }
    
    pub fn start(&mut self) {
        self.state = "running".to_string();
        info!("Mock module {} started", self.name);
    }
    
    pub fn stop(&mut self) {
        self.state = "stopped".to_string();
        info!("Mock module {} stopped", self.name);
    }
    
    pub fn receive_event(&mut self) {
        self.events_received += 1;
        debug!("Mock module {} received event #{}", self.name, self.events_received);
    }
}

/// Wait for a condition with timeout
pub async fn wait_for_condition<F>(
    mut condition: F,
    timeout_secs: u64,
    interval_ms: u64,
) -> Result<(), String>
where
    F: FnMut() -> bool,
{
    let timeout = Duration::from_secs(timeout_secs);
    let interval = Duration::from_millis(interval_ms);
    let start = std::time::Instant::now();
    
    while start.elapsed() < timeout {
        if condition() {
            return Ok(());
        }
        sleep(interval).await;
    }
    
    Err(format!("Condition not met within {} seconds", timeout_secs))
}

/// Wait for file to exist
pub async fn wait_for_file<P: AsRef<Path>>(path: P, timeout_secs: u64) -> Result<(), String> {
    wait_for_condition(
        || path.as_ref().exists(),
        timeout_secs,
        100,
    ).await
}

