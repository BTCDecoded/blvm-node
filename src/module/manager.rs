//! Module manager for orchestrating all modules
//!
//! Handles module lifecycle, runtime loading/unloading/reloading, and coordination.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::module::api::events::EventManager;
use crate::module::api::hub::ModuleApiHub;
#[cfg(unix)]
use crate::module::ipc::server::ModuleIpcServer;
use crate::module::loader::ModuleLoader;
use crate::module::process::{
    monitor::ModuleProcessMonitor,
    spawner::{ModuleProcess, ModuleProcessSpawner},
};
use crate::module::registry::{ModuleDependencies, ModuleDiscovery};
use crate::module::security::permissions::PermissionSet;
use crate::module::traits::{ModuleContext, ModuleError, ModuleMetadata, ModuleState};

/// Module manager coordinates all loaded modules
pub struct ModuleManager {
    /// Process spawner
    spawner: ModuleProcessSpawner,
    /// Active modules (name -> process)
    modules: Arc<Mutex<HashMap<String, ManagedModule>>>,
    /// IPC server handle
    ipc_server_handle: Option<JoinHandle<Result<(), ModuleError>>>,
    /// Crash notification receiver (mutable so it can be moved to handler)
    crash_rx: Option<mpsc::UnboundedReceiver<(String, ModuleError)>>,
    /// Crash notification sender
    crash_tx: mpsc::UnboundedSender<(String, ModuleError)>,
    /// Base directory for module binaries
    modules_dir: PathBuf,
    /// Event manager for module event subscriptions
    event_manager: Arc<EventManager>,
    /// API hub for request routing
    api_hub: Option<Arc<tokio::sync::Mutex<crate::module::api::hub::ModuleApiHub>>>,
    /// Module registry for fetching modules via P2P
    module_registry: Option<Arc<crate::module::registry::client::ModuleRegistry>>,
}

/// Managed module instance
struct ManagedModule {
    /// Module metadata
    metadata: ModuleMetadata,
    /// Module process (shared with monitor via Arc<Mutex<>>)
    process: Option<Arc<tokio::sync::Mutex<ModuleProcess>>>,
    /// Module state
    state: ModuleState,
    /// Monitoring handle
    monitor_handle: Option<JoinHandle<()>>,
    /// Process ID for tracking
    process_id: Option<u32>,
}

impl ModuleManager {
    /// Create a new module manager
    pub fn new<P: AsRef<Path>>(modules_dir: P, data_dir: P, socket_dir: P) -> Self {
        Self::with_config(modules_dir, data_dir, socket_dir, None)
    }

    /// Create a new module manager with resource limits configuration
    pub fn with_config<P: AsRef<Path>>(
        modules_dir: P,
        data_dir: P,
        socket_dir: P,
        resource_limits_config: Option<&crate::config::ModuleResourceLimitsConfig>,
    ) -> Self {
        let (crash_tx, crash_rx) = mpsc::unbounded_channel();

        Self {
            spawner: ModuleProcessSpawner::with_config(
                &modules_dir,
                &data_dir,
                &socket_dir,
                resource_limits_config,
            ),
            modules: Arc::new(Mutex::new(HashMap::new())),
            ipc_server_handle: None,
            crash_rx: Some(crash_rx),
            crash_tx,
            modules_dir: modules_dir.as_ref().to_path_buf(),
            event_manager: Arc::new(EventManager::new()),
            api_hub: None,
            module_registry: None,
        }
    }

    /// Set module registry for fetching modules via P2P
    pub fn with_module_registry(
        mut self,
        module_registry: Arc<crate::module::registry::client::ModuleRegistry>,
    ) -> Self {
        self.module_registry = Some(module_registry);
        self
    }

    /// Set module registry for fetching modules via P2P (mutable reference version)
    pub fn set_module_registry(
        &mut self,
        module_registry: Arc<crate::module::registry::client::ModuleRegistry>,
    ) {
        self.module_registry = Some(module_registry);
    }

    /// Start the module manager
    pub async fn start<
        P: AsRef<Path>,
        A: crate::module::traits::NodeAPI + Send + Sync + 'static,
    >(
        &mut self,
        socket_path: P,
        node_api: Arc<A>,
    ) -> Result<(), ModuleError> {
        info!("Starting module manager");

        // Create API hub
        let api_hub = Arc::new(tokio::sync::Mutex::new(ModuleApiHub::new(Arc::clone(
            &node_api,
        ))));
        self.api_hub = Some(Arc::clone(&api_hub));

        // Start IPC server in background task (Unix only)
        #[cfg(unix)]
        {
            let mut ipc_server = ModuleIpcServer::new(&socket_path)
                .with_event_manager(Arc::clone(&self.event_manager))
                .with_api_hub(Arc::clone(&api_hub));
            let node_api_clone = Arc::clone(&node_api);
            let server_handle = tokio::spawn(async move { ipc_server.start(node_api_clone).await });
            self.ipc_server_handle = Some(server_handle);
        }
        #[cfg(not(unix))]
        {
            warn!("IPC server not available on Windows - module communication disabled");
        }

        // Start crash handler
        let modules = Arc::clone(&self.modules);
        let mut crash_rx = self.crash_rx.take().expect("Crash receiver already taken");
        tokio::spawn(async move {
            while let Some((module_name, error)) = crash_rx.recv().await {
                warn!("Module {} crashed: {}", module_name, error);
                // Remove crashed module
                let mut modules = modules.lock().await;
                modules.remove(&module_name);
            }
        });

        info!("Module manager started");
        Ok(())
    }

    /// Load a module at runtime
    pub async fn load_module(
        &mut self,
        module_name: &str,
        binary_path: &Path,
        metadata: ModuleMetadata,
        config: HashMap<String, String>,
    ) -> Result<(), ModuleError> {
        info!("Loading module: {}", module_name);

        let mut modules = self.modules.lock().await;

        // Check if module already loaded
        if modules.contains_key(module_name) {
            return Err(ModuleError::OperationError(format!(
                "Module {module_name} is already loaded"
            )));
        }

        // Create module context
        let module_id = format!("{module_name}_{}", uuid::Uuid::new_v4());
        let socket_path = self.spawner.socket_dir.join(format!("{module_name}.sock"));
        let data_dir = self.spawner.data_dir.join(module_name);

        let context = ModuleContext::new(
            module_id,
            socket_path.to_string_lossy().to_string(),
            data_dir.to_string_lossy().to_string(),
            config,
        );

        // Spawn module process
        let process = self
            .spawner
            .spawn(module_name, binary_path, context)
            .await?;
        let process_id = process.id();

        // Share process between manager and monitor using Arc<Mutex<>>
        // This allows both to access the process for different purposes
        use std::sync::Arc;
        let shared_process = Arc::new(tokio::sync::Mutex::new(process));

        // Create monitor with shared process
        let monitor = ModuleProcessMonitor::new(self.crash_tx.clone());
        let module_name_clone = module_name.to_string();
        let shared_process_for_monitor = Arc::clone(&shared_process);
        let monitor_handle = tokio::spawn(async move {
            if let Err(e) = monitor
                .monitor_module_shared(module_name_clone.clone(), shared_process_for_monitor)
                .await
            {
                error!("Module {} monitor error: {}", module_name_clone, e);
            }
        });

        // Register module permissions in API hub
        if let Some(ref api_hub) = self.api_hub {
            let permissions = Self::parse_permissions_from_metadata(&metadata);
            let mut hub_guard = api_hub.lock().await;
            hub_guard.register_module_permissions(module_name.to_string(), permissions);
        }

        // Store module with shared process
        let managed = ManagedModule {
            metadata,
            process: Some(shared_process),
            state: ModuleState::Running,
            monitor_handle: Some(monitor_handle),
            process_id,
        };

        modules.insert(module_name.to_string(), managed);

        info!("Module {} loaded successfully", module_name);
        Ok(())
    }

    /// Unload a module (stop and remove)
    pub async fn unload_module(&mut self, module_name: &str) -> Result<(), ModuleError> {
        info!("Unloading module: {}", module_name);

        let mut modules = self.modules.lock().await;

        if let Some(mut managed) = modules.remove(module_name) {
            // Stop monitoring
            if let Some(handle) = managed.monitor_handle.take() {
                handle.abort();
            }

            // Kill process if we have a reference
            if let Some(shared_process) = managed.process.take() {
                let mut process_guard = shared_process.lock().await;
                process_guard.kill().await?;
            } else if let Some(pid) = managed.process_id {
                // Kill by PID if we don't have process reference
                use tokio::process::Command;
                #[cfg(unix)]
                {
                    let _ = Command::new("kill")
                        .arg("-9")
                        .arg(pid.to_string())
                        .output()
                        .await;
                }
            }

            info!("Module {} unloaded", module_name);
            Ok(())
        } else {
            Err(ModuleError::ModuleNotFound(module_name.to_string()))
        }
    }

    /// Reload a module (unload and load again)
    pub async fn reload_module(
        &mut self,
        module_name: &str,
        binary_path: &Path,
        metadata: ModuleMetadata,
        config: HashMap<String, String>,
    ) -> Result<(), ModuleError> {
        info!("Reloading module: {}", module_name);

        // Unload first
        let _ = self.unload_module(module_name).await;

        // Small delay to ensure cleanup
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Load again
        self.load_module(module_name, binary_path, metadata, config)
            .await
    }

    /// Get list of loaded modules
    pub async fn list_modules(&self) -> Vec<String> {
        let modules = self.modules.lock().await;
        modules.keys().cloned().collect()
    }

    /// Get module state
    pub async fn get_module_state(&self, module_name: &str) -> Option<ModuleState> {
        let modules = self.modules.lock().await;
        modules.get(module_name).map(|m| m.state.clone())
    }

    /// Auto-discover and load all modules
    pub async fn auto_load_modules(&mut self) -> Result<(), ModuleError> {
        info!("Auto-discovering and loading modules");

        let discovery = ModuleDiscovery::new(&self.spawner.modules_dir);
        let mut discovered_modules = discovery.discover_modules()?;

        // If registry is available and we have missing dependencies, try fetching from registry
        if let Some(ref registry) = self.module_registry {
            // Check for missing dependencies (this would be determined by dependency resolution)
            // For now, we'll just try to fetch any modules that are requested but not found
            // In a full implementation, we'd check dependencies first
        }

        if discovered_modules.is_empty() {
            info!("No modules discovered");
            return Ok(());
        }

        // Try to resolve dependencies - if missing and we have a registry, fetch them
        let resolution = match ModuleDependencies::resolve(&discovered_modules) {
            Ok(res) => res,
            Err(ModuleError::DependencyMissing(msg)) => {
                // Try fetching missing dependencies from registry
                if let Some(ref registry) = self.module_registry {
                    // Parse missing dependencies from error message
                    // Format: "Missing dependencies: [\"dep1\", \"dep2\"]"
                    let missing: Vec<String> = if let Some(start) = msg.find('[') {
                        let deps_str = &msg[start + 1..msg.len() - 1];
                        deps_str
                            .split(',')
                            .map(|s| s.trim().trim_matches('"').to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    } else {
                        Vec::new()
                    };

                    // Fetch each missing dependency
                    for dep_name in &missing {
                        info!(
                            "Attempting to fetch missing dependency {} from registry",
                            dep_name
                        );
                        if let Ok(entry) = registry.fetch_module(dep_name).await {
                            // Install fetched module
                            if let Ok(installed) = self.install_module_from_registry(entry).await {
                                discovered_modules.push(installed);
                                info!(
                                    "Successfully fetched and installed module {} from registry",
                                    dep_name
                                );
                            }
                        }
                    }

                    // Re-resolve dependencies after fetching
                    ModuleDependencies::resolve(&discovered_modules)?
                } else {
                    return Err(ModuleError::DependencyMissing(msg));
                }
            }
            Err(e) => return Err(e),
        };

        // Load module configurations
        let mut module_configs = HashMap::new();
        for module in &discovered_modules {
            let config_path = module.directory.join("config.toml");
            let config = ModuleLoader::load_module_config(&module.manifest.name, config_path)
                .unwrap_or_default();
            module_configs.insert(module.manifest.name.clone(), config);
        }

        // Load modules in dependency order
        ModuleLoader::load_modules_in_order(
            self,
            &discovered_modules,
            &resolution.load_order,
            &module_configs,
        )
        .await?;

        info!("Auto-loaded {} modules", discovered_modules.len());
        Ok(())
    }

    /// Fetch and install a module from the registry
    pub async fn fetch_module_from_registry(
        &mut self,
        module_name: &str,
    ) -> Result<(), ModuleError> {
        let registry = self.module_registry.as_ref().ok_or_else(|| {
            ModuleError::OperationError("Module registry not available".to_string())
        })?;

        info!("Fetching module {} from registry", module_name);
        let entry = registry.fetch_module(module_name).await?;

        // Install the module
        self.install_module_from_registry(entry).await?;

        Ok(())
    }

    /// Install a module entry from the registry to the modules directory
    async fn install_module_from_registry(
        &self,
        entry: crate::module::registry::client::ModuleEntry,
    ) -> Result<crate::module::registry::discovery::DiscoveredModule, ModuleError> {
        use std::fs;
        use std::io::Write;

        // Create module directory
        let module_dir = self.modules_dir.join(&entry.name);
        fs::create_dir_all(&module_dir).map_err(|e| {
            ModuleError::OperationError(format!("Failed to create module directory: {e}"))
        })?;

        // Write manifest
        let manifest_path = module_dir.join("module.toml");
        let manifest_toml = toml::to_string_pretty(&entry.manifest).map_err(|e| {
            ModuleError::OperationError(format!("Failed to serialize manifest: {e}"))
        })?;
        fs::write(&manifest_path, manifest_toml)
            .map_err(|e| ModuleError::OperationError(format!("Failed to write manifest: {e}")))?;

        // Write binary
        let binary_path = module_dir.join(&entry.name);
        if let Some(binary_data) = entry.binary {
            let mut file = fs::File::create(&binary_path).map_err(|e| {
                ModuleError::OperationError(format!("Failed to create binary file: {e}"))
            })?;
            file.write_all(&binary_data)
                .map_err(|e| ModuleError::OperationError(format!("Failed to write binary: {e}")))?;
        } else {
            // Binary not included, fetch separately if needed
            warn!(
                "Module {} binary not included in registry entry",
                entry.name
            );
        }

        // Make binary executable (Unix)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&binary_path)
                .map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get file metadata: {e}"))
                })?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&binary_path, perms).map_err(|e| {
                ModuleError::OperationError(format!("Failed to set executable permissions: {e}"))
            })?;
        }

        // Create DiscoveredModule
        let discovery = ModuleDiscovery::new(&self.modules_dir);
        discovery.discover_module(&entry.name)
    }

    /// Get event manager for publishing events
    pub fn event_manager(&self) -> &Arc<EventManager> {
        &self.event_manager
    }

    /// Parse permissions from module metadata
    fn parse_permissions_from_metadata(metadata: &ModuleMetadata) -> PermissionSet {
        use crate::module::security::permissions::PermissionSet;

        let mut permissions = PermissionSet::new();

        // Parse permissions from metadata.capabilities (Vec<String>)
        // Note: In module.toml, these are declared as "capabilities" but represent permissions
        use crate::module::security::permissions::parse_permission_string;

        for perm_str in &metadata.capabilities {
            if let Some(permission) = parse_permission_string(perm_str) {
                permissions.add(permission);
            } else {
                warn!("Unknown permission string: {}", perm_str);
            }
        }

        permissions
    }

    /// Stop all modules and shutdown manager
    pub async fn shutdown(&mut self) -> Result<(), ModuleError> {
        info!("Shutting down module manager");

        // Unload all modules
        let module_names: Vec<String> = {
            let modules = self.modules.lock().await;
            modules.keys().cloned().collect()
        };

        for module_name in module_names {
            if let Err(e) = self.unload_module(&module_name).await {
                warn!("Error unloading module {}: {}", module_name, e);
            }
        }

        // Stop IPC server (Unix only)
        #[cfg(unix)]
        if let Some(handle) = self.ipc_server_handle.take() {
            handle.abort();
        }

        info!("Module manager shut down");
        Ok(())
    }
}
