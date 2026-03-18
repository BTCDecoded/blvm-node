//! Module manager for orchestrating all modules
//!
//! Handles module lifecycle, runtime loading/unloading/reloading, and coordination.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::module::api::events::EventManager;
use crate::module::api::hub::ModuleApiHub;
#[cfg(unix)]
use crate::module::ipc::server::ModuleIpcServer;
#[cfg(all(unix, feature = "wasm-modules"))]
use crate::module::ipc::server::WasmInvoker;
use crate::module::loader::ModuleLoader;
use crate::module::process::{
    monitor::ModuleProcessMonitor,
    spawner::{ModuleProcess, ModuleProcessSpawner},
};
use crate::module::registry::{ModuleDependencies, ModuleDiscovery};
use crate::module::security::permissions::PermissionSet;
use crate::module::traits::{ModuleContext, ModuleError, ModuleMetadata, ModuleState};
#[cfg(feature = "wasm-modules")]
use crate::module::wasm::{WasmModuleInstance, WasmModuleLoader};
use crate::utils::MODULE_RELOAD_CLEANUP_DELAY;
use uuid::Uuid;

/// Module manager coordinates all loaded modules
pub struct ModuleManager {
    /// Process spawner
    spawner: ModuleProcessSpawner,
    /// Active modules (name -> process)
    modules: Arc<Mutex<HashMap<String, ManagedModule>>>,
    /// IPC server handle
    ipc_server_handle: Option<JoinHandle<Result<(), ModuleError>>>,
    /// IPC server reference (for getmoduleclispecs, etc.)
    #[cfg(unix)]
    ipc_server: Option<Arc<tokio::sync::Mutex<ModuleIpcServer>>>,
    /// Crash notification receiver (mutable so it can be moved to handler)
    crash_rx: Option<mpsc::UnboundedReceiver<(String, ModuleError)>>,
    /// Crash notification sender
    crash_tx: mpsc::UnboundedSender<(String, ModuleError)>,
    /// Base directory for module binaries
    modules_dir: PathBuf,
    /// Node config overrides per module ([modules.<name>] from config.toml). Merged over module config.
    module_config_overrides: std::sync::RwLock<HashMap<String, HashMap<String, String>>>,
    /// Default database backend for modules (inherited from node when not set per-module).
    /// Values: "redb", "rocksdb", "sled", "tidesdb".
    default_database_backend: std::sync::RwLock<Option<String>>,
    /// Event manager for module event subscriptions
    event_manager: Arc<EventManager>,
    /// API hub for request routing
    api_hub: Option<Arc<tokio::sync::Mutex<crate::module::api::hub::ModuleApiHub>>>,
    /// Module registry for fetching modules via P2P
    module_registry: Option<Arc<crate::module::registry::client::ModuleRegistry>>,
    /// WASM loader (injected by binary; e.g. from blvm-sdk)
    #[cfg(feature = "wasm-modules")]
    wasm_loader: Option<Arc<dyn WasmModuleLoader>>,
    /// Node IPC socket path (where node listens; modules connect here)
    node_socket_path: Option<PathBuf>,
}

/// Managed module instance
struct ManagedModule {
    /// Module metadata
    metadata: ModuleMetadata,
    /// Module process (native subprocess; None for WASM modules)
    process: Option<Arc<tokio::sync::Mutex<ModuleProcess>>>,
    /// Module state
    state: ModuleState,
    /// Monitoring handle (native only)
    monitor_handle: Option<JoinHandle<()>>,
    /// Process ID for tracking (native only)
    process_id: Option<u32>,
    /// WASM instance (in-process; None for native modules)
    #[cfg(feature = "wasm-modules")]
    wasm_instance: Option<Arc<tokio::sync::Mutex<Arc<dyn WasmModuleInstance>>>>,
}

/// In-process WASM invoker for the IPC server.
#[cfg(all(unix, feature = "wasm-modules"))]
struct ManagerWasmInvoker {
    modules: Arc<Mutex<HashMap<String, ManagedModule>>>,
}

#[cfg(all(unix, feature = "wasm-modules"))]
#[async_trait::async_trait]
impl WasmInvoker for ManagerWasmInvoker {
    async fn invoke_cli(
        &self,
        module_name: &str,
        subcommand: &str,
        args: Vec<String>,
    ) -> Result<
        crate::module::ipc::protocol::InvocationResultPayload,
        ModuleError,
    > {
        let wasm_guard = {
            let modules = self.modules.lock().await;
            let managed = modules
                .get(module_name)
                .ok_or_else(|| {
                    ModuleError::OperationError(format!(
                        "No WASM module '{}' is loaded",
                        module_name
                    ))
                })?;
            #[cfg(feature = "wasm-modules")]
            match &managed.wasm_instance {
                Some(inst) => inst.clone(),
                None => {
                    return Err(ModuleError::OperationError(format!(
                        "Module '{}' is not a WASM module",
                        module_name
                    )));
                }
            }
        };
        let instance_guard = wasm_guard.lock().await;
        let result = tokio::task::block_in_place(|| {
            (**instance_guard).invoke_cli(subcommand, args)
        });
        result
    }

    async fn invoke_rpc(
        &self,
        module_name: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ModuleError> {
        let wasm_guard = {
            let modules = self.modules.lock().await;
            let managed = modules
                .get(module_name)
                .ok_or_else(|| {
                    ModuleError::OperationError(format!(
                        "No WASM module '{}' is loaded",
                        module_name
                    ))
                })?;
            #[cfg(feature = "wasm-modules")]
            match &managed.wasm_instance {
                Some(inst) => inst.clone(),
                None => {
                    return Err(ModuleError::OperationError(format!(
                        "Module '{}' is not a WASM module",
                        module_name
                    )));
                }
            }
        };
        let instance_guard = wasm_guard.lock().await;
        let result = tokio::task::block_in_place(|| {
            (**instance_guard).invoke_rpc(method, params)
        });
        result
    }
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
            #[cfg(unix)]
            ipc_server: None,
            crash_rx: Some(crash_rx),
            crash_tx,
            modules_dir: modules_dir.as_ref().to_path_buf(),
            module_config_overrides: std::sync::RwLock::new(HashMap::new()),
            default_database_backend: std::sync::RwLock::new(None),
            event_manager: Arc::new(EventManager::new()),
            api_hub: None,
            module_registry: None,
            #[cfg(feature = "wasm-modules")]
            wasm_loader: None,
            node_socket_path: None,
        }
    }

    /// Set the WASM module loader (injected by binary; e.g. from blvm-sdk).
    #[cfg(feature = "wasm-modules")]
    pub fn set_wasm_loader(&mut self, loader: Arc<dyn WasmModuleLoader>) {
        self.wasm_loader = Some(loader);
    }

    /// Set node config overrides for modules ([modules.<name>] from config.toml).
    /// Merged over module's config.toml when loading. Node values take precedence.
    pub fn set_module_config_overrides(
        &self,
        overrides: HashMap<String, HashMap<String, String>>,
    ) {
        if let Ok(mut guard) = self.module_config_overrides.write() {
            *guard = overrides;
        }
    }

    /// Set default database backend for modules (inherited from node when not set per-module).
    /// Values: "redb", "rocksdb", "sled", "tidesdb". Modules use same format as node by default.
    pub fn set_default_database_backend(&self, backend: String) {
        if let Ok(mut guard) = self.default_database_backend.write() {
            *guard = Some(backend);
        }
    }

    /// Merge base config with node overrides for a module.
    fn merge_module_config(
        &self,
        module_name: &str,
        base: HashMap<String, String>,
    ) -> HashMap<String, String> {
        let mut merged = base;
        if let Ok(guard) = self.module_config_overrides.read() {
            if let Some(overrides) = guard.get(module_name) {
                for (k, v) in overrides {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
        // Inherit node's database backend when module doesn't specify one
        if !merged.contains_key("database_backend") {
            if let Ok(guard) = self.default_database_backend.read() {
                if let Some(ref backend) = *guard {
                    merged.insert("database_backend".to_string(), backend.clone());
                }
            }
        }
        merged
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

    /// Get modules directory path (for discovery)
    pub fn modules_dir(&self) -> &Path {
        &self.modules_dir
    }

    /// Get IPC server reference (for getmoduleclispecs; Unix only)
    #[cfg(unix)]
    pub fn ipc_server(&self) -> Option<Arc<tokio::sync::Mutex<ModuleIpcServer>>> {
        self.ipc_server.clone()
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

        // Start IPC server in background task (Unix: domain sockets, Windows: named pipes)
        let mut ipc_server = ModuleIpcServer::new(&socket_path)
            .with_event_manager(Arc::clone(&self.event_manager))
            .with_api_hub(Arc::clone(&api_hub));
        #[cfg(all(unix, feature = "wasm-modules"))]
        if self.wasm_loader.is_some() {
            let invoker = Arc::new(ManagerWasmInvoker {
                modules: Arc::clone(&self.modules),
            });
            ipc_server = ipc_server.with_wasm_invoker(invoker);
        }
        let ipc_server = Arc::new(tokio::sync::Mutex::new(ipc_server));
        self.ipc_server = Some(Arc::clone(&ipc_server));
        self.node_socket_path = Some(socket_path.as_ref().to_path_buf());
        let node_api_clone = Arc::clone(&node_api);
        let server_handle =
            tokio::spawn(async move { ipc_server.lock().await.start(node_api_clone).await });
        self.ipc_server_handle = Some(server_handle);

        // Start crash handler
        let modules = Arc::clone(&self.modules);
        let event_manager = Arc::clone(&self.event_manager);
        let api_hub = self.api_hub.clone();
        #[cfg(unix)]
        let ipc_server = self.ipc_server.clone();
        let mut crash_rx = self.crash_rx.take().expect("Crash receiver already taken");
        let modules_dir = self.modules_dir.clone();
        tokio::spawn(async move {
            while let Some((module_name, error)) = crash_rx.recv().await {
                warn!("Module {} crashed: {}", module_name, error);

                // Get dependent modules before removing
                let dependents: Vec<String> = {
                    let modules = modules.lock().await;
                    modules
                        .iter()
                        .filter_map(|(name, m)| {
                            if m.metadata.dependencies.contains_key(&module_name) {
                                Some(name.clone())
                            } else {
                                None
                            }
                        })
                        .collect()
                };

                // Remove crashed module
                {
                    let mut modules = modules.lock().await;
                    if let Some(mut managed) = modules.remove(&module_name) {
                        // Stop monitoring
                        if let Some(handle) = managed.monitor_handle.take() {
                            handle.abort();
                        }
                        // Update state to Error
                        managed.state = ModuleState::Error(error.to_string());
                    }
                }

                // Clean up: loaded_modules, CLI spec, API hub (same as explicit unload)
                event_manager.remove_loaded_module(&module_name).await;
                #[cfg(unix)]
                if let Some(ref ipc) = ipc_server {
                    ipc.lock()
                        .await
                        .unregister_cli_spec_by_name(&module_name)
                        .await;
                }
                if let Some(ref hub) = api_hub {
                    let mut h = hub.lock().await;
                    h.unregister_module(&module_name).await;
                }

                // Publish ModuleStateChanged and ModuleHealthChanged (Running -> Error)
                use crate::module::ipc::protocol::EventPayload;
                use crate::module::traits::{EventType, ModuleError};
                let payload = EventPayload::ModuleStateChanged {
                    module_name: module_name.clone(),
                    old_state: "Running".to_string(),
                    new_state: "Error".to_string(),
                };
                if let Err(e) = event_manager
                    .publish_event(EventType::ModuleStateChanged, payload)
                    .await
                {
                    warn!("Failed to publish ModuleStateChanged for crashed module: {}", e);
                }
                let payload = EventPayload::ModuleHealthChanged {
                    module_name: module_name.clone(),
                    old_health: "healthy".to_string(),
                    new_health: "unhealthy".to_string(),
                };
                if let Err(e) = event_manager
                    .publish_event(EventType::ModuleHealthChanged, payload)
                    .await
                {
                    warn!("Failed to publish ModuleHealthChanged for crashed module: {}", e);
                }

                // Publish ModuleCrashed event (specific to abnormal exit)
                let error_msg = match &error {
                    ModuleError::ModuleCrashed(msg) => msg.clone(),
                    _ => error.to_string(),
                };
                let payload = EventPayload::ModuleCrashed {
                    module_name: module_name.clone(),
                    error: error_msg,
                };
                if let Err(e) = event_manager
                    .publish_event(EventType::ModuleCrashed, payload)
                    .await
                {
                    warn!(
                        "Failed to publish ModuleCrashed event for {}: {}",
                        module_name, e
                    );
                }

                // Publish ModuleUnloaded event (crashed modules are effectively unloaded)
                let payload = EventPayload::ModuleUnloaded {
                    module_name: module_name.clone(),
                    version: String::new(), // Version unknown for crashed modules
                };
                if let Err(e) = event_manager
                    .publish_event(EventType::ModuleUnloaded, payload)
                    .await
                {
                    warn!(
                        "Failed to publish ModuleUnloaded event for crashed module: {}",
                        e
                    );
                }

                // Unload dependent modules with hard dependencies
                // Note: We can't use self.unload_module() here since we're in a spawned task
                // Instead, we'll just remove them and let the system handle it
                if !dependents.is_empty() {
                    warn!(
                        "Unloading {} dependent module(s) due to crashed dependency '{}'",
                        dependents.len(),
                        module_name
                    );
                    let removed: Vec<String> = {
                        let mut modules = modules.lock().await;
                        dependents
                            .into_iter()
                            .filter_map(|dependent| {
                                if let Some(mut managed) = modules.remove(&dependent) {
                                    if let Some(handle) = managed.monitor_handle.take() {
                                        handle.abort();
                                    }
                                    managed.state = ModuleState::Error(format!(
                                        "Dependency '{module_name}' crashed"
                                    ));
                                    warn!(
                                        "Dependent module '{}' unloaded due to crashed dependency",
                                        dependent
                                    );
                                    Some(dependent)
                                } else {
                                    None
                                }
                            })
                            .collect()
                    };
                    for dependent in removed {
                        event_manager.remove_loaded_module(&dependent).await;
                        #[cfg(unix)]
                        if let Some(ref ipc) = ipc_server {
                            ipc.lock()
                                .await
                                .unregister_cli_spec_by_name(&dependent)
                                .await;
                        }
                        if let Some(ref hub) = api_hub {
                            let mut h = hub.lock().await;
                            h.unregister_module(&dependent).await;
                        }
                    }
                }
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

        // Validate dependencies BEFORE spawning process (hard dependency enforcement)
        for (dep_name, dep_version_req) in &metadata.dependencies {
            // Check if dependency is loaded
            let dep_module = modules.get(dep_name).ok_or_else(|| {
                ModuleError::DependencyMissing(format!(
                    "Required dependency '{dep_name}' not loaded (required by '{module_name}')"
                ))
            })?;

            // Validate version constraint (basic semver checking)
            if !Self::check_version_constraint(&dep_module.metadata.version, dep_version_req) {
                return Err(ModuleError::DependencyMissing(format!(
                    "Dependency '{}' version '{}' does not satisfy requirement '{}' (required by '{}')",
                    dep_name, dep_module.metadata.version, dep_version_req, module_name
                )));
            }

            // Check if dependency is in a valid state (Running or Initialized)
            if dep_module.state != ModuleState::Running
                && dep_module.state != ModuleState::Initializing
            {
                return Err(ModuleError::OperationError(format!(
                    "Dependency '{}' is not in a valid state (state: {:?}, required by '{}')",
                    dep_name, dep_module.state, module_name
                )));
            }
        }

        // Create module context
        let module_id = format!("{module_name}_{}", uuid::Uuid::new_v4());
        let socket_path = self
            .node_socket_path
            .as_ref()
            .cloned()
            .unwrap_or_else(|| self.spawner.socket_dir.join(format!("{module_name}.sock")));
        let data_dir = self.spawner.data_dir.join(module_name);

        // Ensure module data directory exists
        std::fs::create_dir_all(&data_dir).map_err(|e| {
            ModuleError::InitializationError(format!("Failed to create module data directory: {e}"))
        })?;

        let mut merged_config = self.merge_module_config(module_name, config);
        merged_config.insert("version".to_string(), metadata.version.clone());
        let config_for_wasm = merged_config.clone();
        let context = ModuleContext::new(
            module_id,
            socket_path.to_string_lossy().to_string(),
            data_dir.to_string_lossy().to_string(),
            merged_config,
        );

        use std::sync::Arc;

        let is_wasm = binary_path
            .extension()
            .map(|e| e == "wasm")
            .unwrap_or(false);

        // WASM path: load in-process via injected loader
        if is_wasm {
            #[cfg(feature = "wasm-modules")]
            {
                let loader = self
                    .wasm_loader
                    .as_ref()
                    .ok_or_else(|| {
                        ModuleError::InitializationError(
                            "WASM loader not configured. Pass WasmModuleLoader when creating the node.".to_string(),
                        )
                    })?;
                let data_dir = data_dir.to_path_buf();
                let instance = loader
                    .load(binary_path, &data_dir, config_for_wasm)
                    .map_err(|e| {
                        ModuleError::InitializationError(format!(
                            "Failed to load WASM module: {e}"
                        ))
                    })?;
                let cli_spec = instance.cli_spec().unwrap_or_else(|| {
                    crate::module::ipc::protocol::CliSpec {
                        version: 1,
                        name: module_name.to_string(),
                        about: None,
                        subcommands: vec![],
                    }
                });
                let wasm_instance =
                    Some(Arc::new(tokio::sync::Mutex::new(instance)));

                if let Some(ref api_hub) = self.api_hub {
                    let permissions = Self::parse_permissions_from_metadata(&metadata);
                    let mut hub_guard = api_hub.lock().await;
                    hub_guard.register_module_permissions(module_name.to_string(), permissions);
                }

                let module_version = metadata.version.clone();
                let managed = ManagedModule {
                    metadata,
                    process: None,
                    state: ModuleState::Running,
                    monitor_handle: None,
                    process_id: None,
                    wasm_instance,
                };
                modules.insert(module_name.to_string(), managed);
                drop(modules);
                if let Some(ref ipc) = self.ipc_server {
                    ipc.lock()
                        .await
                        .register_cli_spec(module_name.to_string(), cli_spec)
                        .await;
                }
                let loaded_modules = self.event_manager.loaded_modules();
                let mut loaded = loaded_modules.lock().await;
                let timestamp = crate::utils::current_timestamp();
                loaded.insert(module_name.to_string(), (module_version, timestamp));
                info!("Module {} loaded successfully (WASM)", module_name);
                return Ok(());
            }
            #[cfg(not(feature = "wasm-modules"))]
            {
                return Err(ModuleError::InitializationError(
                    "WASM modules require --features wasm-modules".to_string(),
                ));
            }
        }

        // Native path: spawn subprocess
        let process = self
            .spawner
            .spawn(module_name, binary_path, context)
            .await?;
        let process_id = process.id();
        let shared_process = Arc::new(tokio::sync::Mutex::new(process));
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

        if let Some(ref api_hub) = self.api_hub {
            let permissions = Self::parse_permissions_from_metadata(&metadata);
            let mut hub_guard = api_hub.lock().await;
            hub_guard.register_module_permissions(module_name.to_string(), permissions);
        }

        let module_version = metadata.version.clone();
        let managed = ManagedModule {
            metadata,
            process: Some(shared_process),
            state: ModuleState::Running,
            monitor_handle: Some(monitor_handle),
            process_id,
            #[cfg(feature = "wasm-modules")]
            wasm_instance: None,
        };

        modules.insert(module_name.to_string(), managed);

        info!("Module {} loaded successfully", module_name);

        // Publish ModuleInstalled for registry/marketplace sync
        {
            use crate::module::ipc::protocol::EventPayload;
            use crate::module::traits::EventType;
            let payload = EventPayload::ModuleInstalled {
                module_name: module_name.to_string(),
                version: module_version.clone(),
            };
            if let Err(e) = self
                .event_manager
                .publish_event(EventType::ModuleInstalled, payload)
                .await
            {
                warn!("Failed to publish ModuleInstalled: {}", e);
            }
        }

        // Record module as loaded (for sending to newly subscribing modules)
        // ModuleLoaded event will be published AFTER the module subscribes (in subscribe_module)
        // This ensures consistency: ModuleLoaded only happens after startup is complete
        {
            let loaded_modules = self.event_manager.loaded_modules();
            let mut loaded = loaded_modules.lock().await;
            let timestamp = crate::utils::current_timestamp();
            loaded.insert(module_name.to_string(), (module_version.clone(), timestamp));
        }

        // Note: ModuleLoaded event is NOT published here
        // It will be published when the module subscribes to events (after startup is complete)
        // This ensures:
        // 1. ModuleLoaded only happens after module is fully ready (subscribed)
        // 2. Hotloaded modules receive ModuleLoaded for all already-loaded modules via subscribe_module()
        // 3. Consistent event ordering: subscription -> ModuleLoaded events

        Ok(())
    }

    /// Unload a module (stop and remove)
    pub async fn unload_module(&mut self, module_name: &str) -> Result<(), ModuleError> {
        info!("Unloading module: {}", module_name);

        // Get list of dependent modules (before we drop the lock)
        let dependents = self.get_dependent_modules(module_name).await;

        let mut modules = self.modules.lock().await;
        let module_id = modules
            .get(module_name)
            .map(|m| format!("{}_{}", module_name, uuid::Uuid::new_v4())) // We don't store module_id, so generate one
            .unwrap_or_else(|| module_name.to_string());

        if let Some(mut managed) = modules.remove(module_name) {
            // Publish ModuleStateChanged and ModuleHealthChanged before teardown
            let old_state = format!("{:?}", managed.state);
            let old_health = match &managed.state {
                ModuleState::Running | ModuleState::Initializing => "healthy",
                ModuleState::Stopping => "degraded",
                ModuleState::Stopped | ModuleState::Error(_) => "unhealthy",
            };
            drop(modules);
            {
                use crate::module::ipc::protocol::EventPayload;
                use crate::module::traits::EventType;
                let payload = EventPayload::ModuleStateChanged {
                    module_name: module_name.to_string(),
                    old_state: old_state.clone(),
                    new_state: "Stopping".to_string(),
                };
                if let Err(e) = self
                    .event_manager
                    .publish_event(EventType::ModuleStateChanged, payload)
                    .await
                {
                    warn!("Failed to publish ModuleStateChanged: {}", e);
                }
                let payload = EventPayload::ModuleHealthChanged {
                    module_name: module_name.to_string(),
                    old_health: old_health.to_string(),
                    new_health: "degraded".to_string(),
                };
                if let Err(e) = self
                    .event_manager
                    .publish_event(EventType::ModuleHealthChanged, payload)
                    .await
                {
                    warn!("Failed to publish ModuleHealthChanged: {}", e);
                }
            }

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

            // Unregister CLI spec (WASM modules register on load; native remove on disconnect, but we clean up here too)
            #[cfg(unix)]
            if let Some(ref ipc) = self.ipc_server {
                ipc.lock()
                    .await
                    .unregister_cli_spec_by_name(module_name)
                    .await;
            }

            // Remove from loaded_modules (event manager)
            self.event_manager.remove_loaded_module(module_name).await;

            // Unregister permissions and rate limiters (API hub)
            if let Some(ref api_hub) = self.api_hub {
                let mut hub = api_hub.lock().await;
                hub.unregister_module(module_name).await;
            }

            // Publish ModuleUnloaded event for dependent modules to react
            let version = managed.metadata.version.clone();
            {
                use crate::module::ipc::protocol::EventPayload;
                use crate::module::traits::EventType;
                let payload = EventPayload::ModuleUnloaded {
                    module_name: module_name.to_string(),
                    version: version.clone(),
                };
                if let Err(e) = self
                    .event_manager
                    .publish_event(EventType::ModuleUnloaded, payload)
                    .await
                {
                    warn!("Failed to publish ModuleUnloaded event: {}", e);
                }
                let payload = EventPayload::ModuleRemoved {
                    module_name: module_name.to_string(),
                    version,
                };
                if let Err(e) = self
                    .event_manager
                    .publish_event(EventType::ModuleRemoved, payload)
                    .await
                {
                    warn!("Failed to publish ModuleRemoved: {}", e);
                }
            }

            // Automatically unload dependent modules (if they have hard dependencies)
            for dependent in dependents {
                // Check if it's a hard dependency (required) or soft (optional)
                let is_required = {
                    let modules = self.modules.lock().await;
                    modules
                        .get(&dependent)
                        .map(|m| m.metadata.dependencies.contains_key(module_name))
                        .unwrap_or(false)
                };

                if is_required {
                    warn!(
                        "Unloading dependent module '{}' due to required dependency '{}' unloading",
                        dependent, module_name
                    );
                    if let Err(e) = Box::pin(self.unload_module(&dependent)).await {
                        error!("Failed to unload dependent module '{}': {}", dependent, e);
                    }
                } else {
                    debug!(
                        "Dependent module '{}' has optional dependency on '{}', leaving it running",
                        dependent, module_name
                    );
                }
            }

            Ok(())
        } else {
            Err(ModuleError::ModuleNotFound(module_name.to_string()))
        }
    }

    /// Reload a module (unload and load again)
    ///
    /// This gracefully handles dependent modules:
    /// 1. Tracks which dependent modules were unloaded (due to hard dependencies)
    /// 2. Reloads the module
    /// 3. Automatically reloads dependent modules that were unloaded
    /// 4. Handles version changes and validates dependency constraints
    /// 5. Publishes ModuleReloaded event
    ///
    /// # Errors
    /// - Returns error if the module cannot be reloaded
    /// - Dependent modules that fail to reload are logged but don't fail the operation
    pub async fn reload_module(
        &mut self,
        module_name: &str,
        binary_path: &Path,
        metadata: ModuleMetadata,
        config: HashMap<String, String>,
    ) -> Result<(), ModuleError> {
        info!("Reloading module: {}", module_name);

        // Get old version and dependent modules BEFORE unloading
        let (old_version, dependents_to_reload) = {
            let modules = self.modules.lock().await;
            let old_version = modules
                .get(module_name)
                .map(|m| m.metadata.version.clone())
                .unwrap_or_else(|| "unknown".to_string());

            // Get list of dependent modules that have hard dependencies
            // Use discovery to get proper binary paths and metadata
            let discovery =
                crate::module::registry::discovery::ModuleDiscovery::new(&self.modules_dir);
            let dependents: Vec<String> = modules
                .iter()
                .filter_map(|(name, m)| {
                    // Check if this module has a hard dependency on the module being reloaded
                    if m.metadata.dependencies.contains_key(module_name) {
                        Some(name.clone())
                    } else {
                        None
                    }
                })
                .collect();

            (old_version, dependents)
        };

        // Unload the module (this will also unload dependents with hard dependencies)
        let unload_result = self.unload_module(module_name).await;
        if let Err(e) = unload_result {
            warn!("Error unloading module {} for reload: {}", module_name, e);
            // Continue anyway - might be partially unloaded
        }

        // Small delay to ensure cleanup
        tokio::time::sleep(MODULE_RELOAD_CLEANUP_DELAY).await;

        // Reload the module
        let reload_result = self
            .load_module(module_name, binary_path, metadata.clone(), config.clone())
            .await;

        // Publish ModuleReloaded and ModuleUpdated events
        use crate::module::ipc::protocol::EventPayload;
        use crate::module::traits::EventType;
        let new_version = metadata.version.clone();
        let payload = EventPayload::ModuleReloaded {
            module_name: module_name.to_string(),
            old_version: old_version.clone(),
            new_version: new_version.clone(),
        };
        if let Err(e) = self
            .event_manager
            .publish_event(EventType::ModuleReloaded, payload)
            .await
        {
            warn!("Failed to publish ModuleReloaded event: {}", e);
        }
        if reload_result.is_ok() {
            let payload = EventPayload::ModuleUpdated {
                module_name: module_name.to_string(),
                old_version: old_version.clone(),
                new_version: new_version.clone(),
            };
            if let Err(e) = self
                .event_manager
                .publish_event(EventType::ModuleUpdated, payload)
                .await
            {
                warn!("Failed to publish ModuleUpdated event: {}", e);
            }
        }

        // If reload succeeded, reload dependent modules using discovery
        if reload_result.is_ok() && !dependents_to_reload.is_empty() {
            info!(
                "Reloading {} dependent module(s) after {} reload",
                dependents_to_reload.len(),
                module_name
            );

            let discovery =
                crate::module::registry::discovery::ModuleDiscovery::new(&self.modules_dir);

            for dependent_name in dependents_to_reload {
                // Discover the dependent module to get proper binary path and metadata
                let discovered = match discovery.discover_module(&dependent_name) {
                    Ok(d) => d,
                    Err(e) => {
                        error!(
                            "Cannot reload dependent module '{}': discovery failed: {}",
                            dependent_name, e
                        );
                        continue; // Skip this dependent
                    }
                };

                // Validate that the new version still satisfies the dependent's requirements
                let version_req = discovered.manifest.dependencies.get(module_name);
                if let Some(req) = version_req {
                    if !Self::check_version_constraint(&metadata.version, req) {
                        error!(
                            "Cannot reload dependent module '{}': new version '{}' of '{}' does not satisfy requirement '{}'",
                            dependent_name, metadata.version, module_name, req
                        );
                        continue; // Skip this dependent
                    }
                }

                // Check if binary path exists
                if !discovered.binary_path.exists() {
                    warn!(
                        "Cannot reload dependent module '{}': binary not found at {}",
                        dependent_name,
                        discovered.binary_path.display()
                    );
                    continue; // Skip this dependent
                }

                // Load config if it exists
                let config_path = discovered.directory.join("config.toml");
                let dependent_config = if config_path.exists() {
                    // Try to load config from TOML file
                    match std::fs::read_to_string(&config_path) {
                        Ok(contents) => {
                            // Parse TOML config
                            match toml::from_str::<toml::Value>(&contents) {
                                Ok(toml_value) => {
                                    // Convert TOML value to HashMap<String, String>
                                    // This handles nested structures by flattening keys
                                    fn flatten_toml_value(
                                        value: &toml::Value,
                                        prefix: &str,
                                        result: &mut HashMap<String, String>,
                                    ) {
                                        match value {
                                            toml::Value::String(s) => {
                                                result.insert(
                                                    if prefix.is_empty() {
                                                        "value".to_string()
                                                    } else {
                                                        prefix.to_string()
                                                    },
                                                    s.clone(),
                                                );
                                            }
                                            toml::Value::Integer(i) => {
                                                result.insert(
                                                    if prefix.is_empty() {
                                                        "value".to_string()
                                                    } else {
                                                        prefix.to_string()
                                                    },
                                                    i.to_string(),
                                                );
                                            }
                                            toml::Value::Float(f) => {
                                                result.insert(
                                                    if prefix.is_empty() {
                                                        "value".to_string()
                                                    } else {
                                                        prefix.to_string()
                                                    },
                                                    f.to_string(),
                                                );
                                            }
                                            toml::Value::Boolean(b) => {
                                                result.insert(
                                                    if prefix.is_empty() {
                                                        "value".to_string()
                                                    } else {
                                                        prefix.to_string()
                                                    },
                                                    b.to_string(),
                                                );
                                            }
                                            toml::Value::Table(table) => {
                                                for (key, val) in table {
                                                    let new_prefix = if prefix.is_empty() {
                                                        key.clone()
                                                    } else {
                                                        format!("{prefix}.{key}")
                                                    };
                                                    flatten_toml_value(val, &new_prefix, result);
                                                }
                                            }
                                            toml::Value::Array(arr) => {
                                                for (idx, val) in arr.iter().enumerate() {
                                                    let new_prefix = format!("{prefix}[{idx}]");
                                                    flatten_toml_value(val, &new_prefix, result);
                                                }
                                            }
                                            toml::Value::Datetime(dt) => {
                                                result.insert(
                                                    if prefix.is_empty() {
                                                        "value".to_string()
                                                    } else {
                                                        prefix.to_string()
                                                    },
                                                    dt.to_string(),
                                                );
                                            }
                                        }
                                    }
                                    let mut config_map = HashMap::new();
                                    flatten_toml_value(&toml_value, "", &mut config_map);
                                    config_map
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to parse TOML config from {}: {}",
                                        config_path.display(),
                                        e
                                    );
                                    HashMap::new()
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Failed to read TOML config from {}: {}",
                                config_path.display(),
                                e
                            );
                            HashMap::new()
                        }
                    }
                } else {
                    HashMap::new()
                };

                // Reload the dependent module
                info!("Reloading dependent module: {}", dependent_name);
                let dependent_metadata = discovered.manifest.to_metadata();
                match self
                    .load_module(
                        &dependent_name,
                        &discovered.binary_path,
                        dependent_metadata,
                        dependent_config,
                    )
                    .await
                {
                    Ok(()) => {
                        info!("Successfully reloaded dependent module: {}", dependent_name);
                    }
                    Err(e) => {
                        error!(
                            "Failed to reload dependent module '{}': {}",
                            dependent_name, e
                        );
                        // Continue with other dependents - don't fail the whole operation
                    }
                }
            }
        }

        reload_result
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

    /// Get module metadata
    pub async fn get_module_metadata(&self, module_name: &str) -> Option<ModuleMetadata> {
        let modules = self.modules.lock().await;
        modules.get(module_name).map(|m| m.metadata.clone())
    }

    /// Get all module information for discovery
    pub async fn get_all_module_info(&self) -> Vec<(String, ModuleMetadata, ModuleState)> {
        let modules = self.modules.lock().await;
        modules
            .iter()
            .map(|(name, m)| (name.clone(), m.metadata.clone(), m.state.clone()))
            .collect()
    }

    /// Validate that all required dependencies for a module are available and running
    /// Only validates required (hard) dependencies - optional dependencies are checked at runtime
    pub async fn validate_module_dependencies(&self, module_name: &str) -> Result<(), ModuleError> {
        let modules = self.modules.lock().await;
        let module = modules.get(module_name).ok_or_else(|| {
            ModuleError::OperationError(format!("Module {module_name} not found"))
        })?;

        // Check each required (hard) dependency
        for (dep_name, dep_version_req) in &module.metadata.dependencies {
            // Check if dependency is loaded
            let dep_module = modules.get(dep_name).ok_or_else(|| {
                ModuleError::OperationError(format!(
                    "Required dependency '{dep_name}' not loaded (required by '{module_name}')"
                ))
            })?;

            // Validate version constraint
            if !Self::check_version_constraint(&dep_module.metadata.version, dep_version_req) {
                return Err(ModuleError::DependencyMissing(format!(
                    "Dependency '{}' version '{}' does not satisfy requirement '{}' (required by '{}')",
                    dep_name, dep_module.metadata.version, dep_version_req, module_name
                )));
            }

            // Check if dependency is in a valid state (Running or Initialized)
            if dep_module.state != ModuleState::Running
                && dep_module.state != ModuleState::Initializing
            {
                return Err(ModuleError::OperationError(format!(
                    "Required dependency '{}' is not in a valid state (state: {:?}, required by '{}')",
                    dep_name, dep_module.state, module_name
                )));
            }
        }

        // Optional dependencies are not validated here - they're checked at runtime when needed

        Ok(())
    }

    /// Validate optional dependencies (soft dependencies)
    /// Returns list of missing optional dependencies (non-fatal)
    pub async fn validate_optional_dependencies(&self, module_name: &str) -> Vec<String> {
        let modules = self.modules.lock().await;
        let module = match modules.get(module_name) {
            Some(m) => m,
            None => return Vec::new(),
        };

        let mut missing = Vec::new();
        for dep_name in module.metadata.optional_dependencies.keys() {
            if !modules.contains_key(dep_name) {
                missing.push(dep_name.clone());
            }
        }

        missing
    }

    /// Get list of modules that depend on a given module (required or optional)
    pub async fn get_dependent_modules(&self, module_name: &str) -> Vec<String> {
        let modules = self.modules.lock().await;
        let mut dependents = Vec::new();

        for (name, module) in modules.iter() {
            // Check both required and optional dependencies
            if module.metadata.dependencies.contains_key(module_name)
                || module
                    .metadata
                    .optional_dependencies
                    .contains_key(module_name)
            {
                dependents.push(name.clone());
            }
        }

        dependents
    }

    /// Check if a module can be safely unloaded (no dependents)
    pub async fn can_unload_module(&self, module_name: &str) -> Result<bool, ModuleError> {
        let dependents = self.get_dependent_modules(module_name).await;
        Ok(dependents.is_empty())
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

        // Publish ModuleDiscovered for each module (for registry/marketplace sync)
        {
            use crate::module::ipc::protocol::EventPayload;
            use crate::module::traits::EventType;
            for module in &discovered_modules {
                let payload = EventPayload::ModuleDiscovered {
                module_name: module.manifest.name.clone(),
                version: module.manifest.version.clone(),
                source: "filesystem".to_string(),
            };
                if let Err(e) = self
                    .event_manager
                    .publish_event(EventType::ModuleDiscovered, payload)
                    .await
                {
                    warn!("Failed to publish ModuleDiscovered for {}: {}", module.manifest.name, e);
                }
            }
        }

        // Load module configurations (merge module config.toml with node [modules.<name>] overrides)
        let mut module_configs = HashMap::new();
        for module in &discovered_modules {
            let config_path = module.directory.join("config.toml");
            let base = ModuleLoader::load_module_config(&module.manifest.name, config_path)
                .unwrap_or_default();
            let merged = self.merge_module_config(&module.manifest.name, base);
            module_configs.insert(module.manifest.name.clone(), merged);
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
            ModuleError::OperationError(crate::module::traits::module_error_msg::MODULE_REGISTRY_NOT_AVAILABLE.to_string())
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
        fs::create_dir_all(&module_dir)
            .map_err(|e| ModuleError::op_err("Failed to create module directory", e))?;

        // Write manifest
        let manifest_path = module_dir.join("module.toml");
        let manifest_toml = toml::to_string_pretty(&entry.manifest)
            .map_err(|e| ModuleError::op_err("Failed to serialize manifest", e))?;
        fs::write(&manifest_path, manifest_toml)
            .map_err(|e| ModuleError::op_err("Failed to write manifest", e))?;

        // Write binary
        let binary_path = module_dir.join(&entry.name);
        if let Some(binary_data) = entry.binary {
            let mut file = fs::File::create(&binary_path)
                .map_err(|e| ModuleError::op_err("Failed to create binary file", e))?;
            file.write_all(&binary_data)
                .map_err(|e| ModuleError::op_err("Failed to write binary", e))?;
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
                .map_err(|e| ModuleError::op_err("Failed to get file metadata", e))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&binary_path, perms)
                .map_err(|e| ModuleError::op_err("Failed to set executable permissions", e))?;
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

    /// Check if a version satisfies a version requirement
    ///
    /// Supports basic semver constraints:
    /// - Exact: "1.0.0"
    /// - Greater than or equal: ">=1.0.0"
    /// - Less than or equal: "<=1.0.0"
    /// - Range: ">=1.0.0,<2.0.0"
    /// - Wildcard: "1.x" or "1.0.x"
    fn check_version_constraint(version: &str, requirement: &str) -> bool {
        // Simple implementation - for production, consider using semver crate
        // For now, handle common cases

        // Exact match
        if version == requirement {
            return true;
        }

        // Remove whitespace
        let req = requirement.trim();

        // Handle >= constraint
        if let Some(required_version) = req.strip_prefix(">=") {
            return Self::compare_versions(version, required_version.trim()) >= 0;
        }

        // Handle <= constraint
        if let Some(required_version) = req.strip_prefix("<=") {
            return Self::compare_versions(version, required_version.trim()) <= 0;
        }

        // Handle > constraint
        if let Some(required_version) = req.strip_prefix(">") {
            return Self::compare_versions(version, required_version.trim()) > 0;
        }

        // Handle < constraint
        if let Some(required_version) = req.strip_prefix("<") {
            return Self::compare_versions(version, required_version.trim()) < 0;
        }

        // Handle range (comma-separated)
        if req.contains(',') {
            let parts: Vec<&str> = req.split(',').collect();
            return parts
                .iter()
                .all(|part| Self::check_version_constraint(version, part.trim()));
        }

        // Handle wildcard (x or *)
        if req.contains('x') || req.contains('*') {
            // Simple prefix match for now
            let version_parts: Vec<&str> = version.split('.').collect();
            let req_parts: Vec<&str> = req.split('.').collect();

            if version_parts.len() != req_parts.len() {
                return false;
            }

            for (v_part, r_part) in version_parts.iter().zip(req_parts.iter()) {
                if *r_part != "x" && *r_part != "*" && v_part != r_part {
                    return false;
                }
            }
            return true;
        }

        // Default: exact match (already checked above, so false)
        false
    }

    #[cfg(test)]
    /// Expose merge_module_config for testing database_backend inheritance
    pub(crate) fn test_merge_module_config(
        &self,
        module_name: &str,
        base: HashMap<String, String>,
    ) -> HashMap<String, String> {
        self.merge_module_config(module_name, base)
    }

    /// Compare two version strings
    /// Returns: -1 if v1 < v2, 0 if v1 == v2, 1 if v1 > v2
    fn compare_versions(v1: &str, v2: &str) -> i32 {
        let parts1: Vec<u32> = v1.split('.').filter_map(|s| s.parse().ok()).collect();
        let parts2: Vec<u32> = v2.split('.').filter_map(|s| s.parse().ok()).collect();

        let max_len = parts1.len().max(parts2.len());

        for i in 0..max_len {
            let p1 = parts1.get(i).copied().unwrap_or(0);
            let p2 = parts2.get(i).copied().unwrap_or(0);

            if p1 < p2 {
                return -1;
            } else if p1 > p2 {
                return 1;
            }
        }

        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::traits::ModuleMetadata;
    use std::collections::HashMap;

    #[test]
    fn test_database_backend_inheritance() {
        let temp = std::env::temp_dir().join("blvm_module_test");
        std::fs::create_dir_all(&temp).unwrap();
        let manager = ModuleManager::new(&temp, &temp, &temp);
        manager.set_default_database_backend("rocksdb".to_string());

        let merged = manager.test_merge_module_config("my-module", HashMap::new());
        assert_eq!(
            merged.get("database_backend"),
            Some(&"rocksdb".to_string()),
            "module should inherit node's database_backend when not set"
        );
    }

    #[test]
    fn test_database_backend_no_override_when_module_sets() {
        let temp = std::env::temp_dir().join("blvm_module_test2");
        std::fs::create_dir_all(&temp).unwrap();
        let manager = ModuleManager::new(&temp, &temp, &temp);
        manager.set_default_database_backend("rocksdb".to_string());

        let mut base = HashMap::new();
        base.insert("database_backend".to_string(), "redb".to_string());
        let merged = manager.test_merge_module_config("my-module", base);
        assert_eq!(
            merged.get("database_backend"),
            Some(&"redb".to_string()),
            "module's database_backend should not be overridden by node default"
        );
    }

    /// When wasm-modules feature is disabled, loading a .wasm file must return a clear error.
    #[cfg(not(feature = "wasm-modules"))]
    #[tokio::test]
    async fn test_wasm_load_requires_feature_when_disabled() {
        let temp = tempfile::tempdir().expect("tempdir");
        let temp_path = temp.path();
        let wasm_path = temp_path.join("fake.wasm");
        std::fs::write(&wasm_path, b"").unwrap();

        let mut manager = ModuleManager::new(temp_path, temp_path, temp_path);
        let metadata = ModuleMetadata {
            name: "fake".to_string(),
            version: "0.1.0".to_string(),
            description: String::new(),
            author: String::new(),
            capabilities: vec![],
            dependencies: HashMap::new(),
            optional_dependencies: HashMap::new(),
            entry_point: "fake".to_string(),
        };

        let err = manager
            .load_module("fake", &wasm_path, metadata, HashMap::new())
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("wasm-modules"),
            "expected error to mention wasm-modules, got: {}",
            msg
        );
    }
}
