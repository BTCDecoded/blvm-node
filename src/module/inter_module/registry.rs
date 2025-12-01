//! Module API registry
//!
//! Tracks which modules expose which APIs and routes requests to them.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::module::inter_module::api::ModuleAPI;
use crate::module::traits::ModuleError;

/// Registry of module APIs
pub struct ModuleApiRegistry {
    /// Map of module_id -> API implementation
    apis: Arc<RwLock<HashMap<String, Arc<dyn ModuleAPI>>>>,
    /// Map of method_name -> (module_id, method_name)
    /// Used for routing requests to the correct module
    method_routing: Arc<RwLock<HashMap<String, (String, String)>>>,
}

impl ModuleApiRegistry {
    /// Create a new module API registry
    pub fn new() -> Self {
        Self {
            apis: Arc::new(RwLock::new(HashMap::new())),
            method_routing: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    /// Register a module API
    ///
    /// # Arguments
    /// * `module_id` - ID of the module exposing the API
    /// * `api` - The API implementation
    pub async fn register_api(
        &self,
        module_id: String,
        api: Arc<dyn ModuleAPI>,
    ) -> Result<(), ModuleError> {
        info!("Registering API for module: {}", module_id);
        
        let methods = api.list_methods();
        let api_version = api.api_version();
        
        debug!(
            "Module {} API v{} exposes {} methods: {:?}",
            module_id, api_version, methods.len(), methods
        );
        
        // Store the API
        {
            let mut apis = self.apis.write().await;
            apis.insert(module_id.clone(), api);
        }
        
        // Register method routing
        {
            let mut routing = self.method_routing.write().await;
            for method in methods {
                let full_method_name = format!("{}::{}", module_id, method);
                routing.insert(full_method_name.clone(), (module_id.clone(), method.clone()));
                
                // Also allow routing by just method name if unique
                // (will be overridden if another module uses same method name)
                if !routing.contains_key(&method) {
                    routing.insert(method, (module_id.clone(), full_method_name));
                }
            }
        }
        
        Ok(())
    }
    
    /// Unregister a module API
    pub async fn unregister_api(&self, module_id: &str) -> Result<(), ModuleError> {
        info!("Unregistering API for module: {}", module_id);
        
        // Remove from APIs
        {
            let mut apis = self.apis.write().await;
            apis.remove(module_id);
        }
        
        // Remove from routing
        {
            let mut routing = self.method_routing.write().await;
            routing.retain(|_, (mid, _)| mid != module_id);
        }
        
        Ok(())
    }
    
    /// Get API for a module
    pub async fn get_api(&self, module_id: &str) -> Option<Arc<dyn ModuleAPI>> {
        let apis = self.apis.read().await;
        apis.get(module_id).cloned()
    }
    
    /// Route a method call to the appropriate module
    ///
    /// # Arguments
    /// * `method_name` - Full method name (module_id::method) or just method name
    ///
    /// # Returns
    /// (module_id, actual_method_name) if found
    pub async fn route_method(&self, method_name: &str) -> Option<(String, String)> {
        let routing = self.method_routing.read().await;
        routing.get(method_name).cloned()
    }
    
    /// List all registered modules
    pub async fn list_modules(&self) -> Vec<String> {
        let apis = self.apis.read().await;
        apis.keys().cloned().collect()
    }
    
    /// Get methods exposed by a module
    pub async fn get_module_methods(&self, module_id: &str) -> Option<Vec<String>> {
        let apis = self.apis.read().await;
        apis.get(module_id).map(|api| api.list_methods())
    }
}

impl Default for ModuleApiRegistry {
    fn default() -> Self {
        Self::new()
    }
}


