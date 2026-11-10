//! Network access control for modules
//!
//! Ensures modules cannot bind to network ports or make unauthorized network connections.
//! Modules can only communicate with the node via IPC.

use tracing::{debug, warn};

use crate::module::traits::ModuleError;

/// Network sandbox that restricts module network access
pub struct NetworkSandbox {
    /// Whether network access is allowed (default: false - only IPC allowed)
    allow_network: bool,
    /// Allowed network endpoints (if network access is enabled)
    #[allow(dead_code)]
    allowed_endpoints: Vec<String>,
}

impl NetworkSandbox {
    /// Create a new network sandbox (no network access by default)
    pub fn new() -> Self {
        Self {
            allow_network: false,
            allowed_endpoints: Vec::new(),
        }
    }

    /// Create a network sandbox with limited network access
    pub fn with_allowed_endpoints(endpoints: Vec<String>) -> Self {
        Self {
            allow_network: !endpoints.is_empty(),
            allowed_endpoints: endpoints,
        }
    }

    /// Check if network access is allowed
    pub fn is_network_allowed(&self) -> bool {
        self.allow_network
    }

    /// Validate that a network operation is allowed
    pub fn validate_network_operation(&self, operation: &str) -> Result<(), ModuleError> {
        if !self.allow_network {
            warn!(
                "Module attempted network operation: {} (network access denied)",
                operation
            );
            return Err(ModuleError::OperationError(format!(
                "Network access denied: modules cannot make network connections. Operation: {operation}"
            )));
        }

        debug!("Network operation allowed: {}", operation);
        Ok(())
    }

    /// Validate that a network endpoint is allowed
    pub fn validate_endpoint(&self, endpoint: &str) -> Result<(), ModuleError> {
        if !self.allow_network {
            return self.validate_network_operation("connect");
        }

        // Check if endpoint is in allowed list
        if !self.allowed_endpoints.is_empty() {
            let allowed = self
                .allowed_endpoints
                .iter()
                .any(|e| endpoint.starts_with(e));

            if !allowed {
                warn!(
                    "Module attempted to connect to unauthorized endpoint: {}",
                    endpoint
                );
                return Err(ModuleError::OperationError(format!(
                    "Endpoint not allowed: {endpoint} (not in allowed list)"
                )));
            }
        }

        debug!("Network endpoint validated: {}", endpoint);
        Ok(())
    }
}

impl Default for NetworkSandbox {
    fn default() -> Self {
        Self::new()
    }
}
