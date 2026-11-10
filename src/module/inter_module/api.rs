//! Module API trait
//!
//! Defines the interface that modules can implement to expose APIs to other modules.

use crate::module::traits::ModuleError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Module API request payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleApiRequest {
    /// API method name
    pub method: String,
    /// Method parameters (JSON-serialized)
    pub params: Vec<u8>,
}

/// Module API response payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleApiResponse {
    /// Response data (JSON-serialized)
    pub data: Vec<u8>,
    /// Error message if request failed
    pub error: Option<String>,
}

/// Trait that modules can implement to expose APIs to other modules
///
/// Modules implement this trait to provide services to other modules.
/// The node routes module-to-module calls through this interface.
#[async_trait]
pub trait ModuleAPI: Send + Sync {
    /// Handle an API request from another module
    ///
    /// # Arguments
    /// * `method` - The API method name
    /// * `params` - Serialized parameters (typically JSON)
    /// * `caller_module_id` - ID of the module making the request
    ///
    /// # Returns
    /// Serialized response data or error
    async fn handle_request(
        &self,
        method: &str,
        params: &[u8],
        caller_module_id: &str,
    ) -> Result<Vec<u8>, ModuleError>;

    /// Get list of available API methods
    fn list_methods(&self) -> Vec<String>;

    /// Get API version
    fn api_version(&self) -> u32;
}
