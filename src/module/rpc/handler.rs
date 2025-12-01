//! RPC handler trait for module endpoints
//!
//! Allows modules to register custom RPC endpoints that integrate with the node's RPC server.

use async_trait::async_trait;
use serde_json::Value;
use crate::rpc::errors::RpcError;

/// Async RPC handler trait for module endpoints
#[async_trait]
pub trait ModuleRpcHandler: Send + Sync {
    /// Handle an RPC request
    /// 
    /// # Arguments
    /// * `params` - JSON-RPC parameters (array or object)
    /// 
    /// # Returns
    /// * `Ok(Value)` - JSON-RPC result
    /// * `Err(RpcError)` - JSON-RPC error
    async fn handle(&self, params: Value) -> Result<Value, RpcError>;
}

