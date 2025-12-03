//! Module-to-module request router
//!
//! Routes module-to-module API calls through the IPC server.

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::module::inter_module::registry::ModuleApiRegistry;
use crate::module::ipc::server::ModuleIpcServer;
use crate::module::manager::ModuleManager;
use crate::module::traits::ModuleError;

/// Router for module-to-module communication
pub struct ModuleRouter {
    /// API registry
    registry: Arc<ModuleApiRegistry>,
    /// IPC server reference (for forwarding requests to modules)
    ipc_server: Option<Arc<tokio::sync::Mutex<ModuleIpcServer>>>,
    /// Module manager (for dependency validation)
    module_manager: Option<Arc<tokio::sync::Mutex<ModuleManager>>>,
}

impl ModuleRouter {
    /// Create a new module router
    pub fn new(registry: Arc<ModuleApiRegistry>) -> Self {
        Self {
            registry,
            ipc_server: None,
            module_manager: None,
        }
    }

    /// Set IPC server reference
    pub fn with_ipc_server(mut self, ipc_server: Arc<tokio::sync::Mutex<ModuleIpcServer>>) -> Self {
        self.ipc_server = Some(ipc_server);
        self
    }

    /// Set module manager reference (for dependency validation)
    pub fn with_module_manager(
        mut self,
        module_manager: Arc<tokio::sync::Mutex<ModuleManager>>,
    ) -> Self {
        self.module_manager = Some(module_manager);
        self
    }

    /// Route a module-to-module API call
    ///
    /// # Arguments
    /// * `caller_module_id` - ID of the module making the request
    /// * `target_module_id` - ID of the target module (or None to use method routing)
    /// * `method` - API method name
    /// * `params` - Serialized parameters
    ///
    /// # Returns
    /// Serialized response data
    pub async fn route_call(
        &self,
        caller_module_id: &str,
        target_module_id: Option<&str>,
        method: &str,
        params: &[u8],
    ) -> Result<Vec<u8>, ModuleError> {
        // Determine target module
        let (target_id, actual_method) = if let Some(target_id) = target_module_id {
            // Direct module call
            (target_id.to_string(), method.to_string())
        } else {
            // Use method routing
            match self.registry.route_method(method).await {
                Some((mid, mname)) => (mid, mname),
                None => {
                    return Err(ModuleError::OperationError(format!(
                        "Method '{}' not found in any module",
                        method
                    )));
                }
            }
        };

        debug!(
            "Routing call: {} -> {}::{}",
            caller_module_id, target_id, actual_method
        );

        // Validate dependencies and health at runtime (if module manager is available)
        if let Some(module_manager) = &self.module_manager {
            let manager = module_manager.lock().await;
            // Extract module name from target_id (format: {module_name}_{uuid})
            let target_module_name = target_id.split('_').next().unwrap_or(&target_id);

            // Validate required dependencies
            if let Err(e) = manager
                .validate_module_dependencies(target_module_name)
                .await
            {
                return Err(ModuleError::OperationError(format!(
                    "Dependency validation failed for module '{}': {}",
                    target_id, e
                )));
            }

            // Check target module health (don't allow calls to unhealthy modules)
            use crate::module::process::monitor::ModuleHealth;
            if let Some(state) = manager.get_module_state(target_module_name).await {
                let health = match state {
                    crate::module::traits::ModuleState::Running => ModuleHealth::Healthy,
                    crate::module::traits::ModuleState::Initializing => ModuleHealth::Healthy,
                    crate::module::traits::ModuleState::Stopped => ModuleHealth::Unresponsive,
                    crate::module::traits::ModuleState::Error(err) => ModuleHealth::Crashed(err),
                    crate::module::traits::ModuleState::Stopping => ModuleHealth::Unresponsive,
                };

                match health {
                    ModuleHealth::Healthy => {
                        // OK to proceed
                    }
                    ModuleHealth::Unresponsive | ModuleHealth::Crashed(_) => {
                        return Err(ModuleError::OperationError(format!(
                            "Target module '{}' is not healthy (health: {:?})",
                            target_id, health
                        )));
                    }
                }
            }
        }

        // Get the API implementation
        let api = self.registry.get_api(&target_id).await.ok_or_else(|| {
            ModuleError::OperationError(format!(
                "Module '{}' not found or has no API registered",
                target_id
            ))
        })?;

        // Call the API
        api.handle_request(&actual_method, params, caller_module_id)
            .await
    }
}
