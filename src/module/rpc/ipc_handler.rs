//! IPC-based RPC handler for module endpoints
//!
//! Forwards RPC requests to modules via IPC and returns responses.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use crate::module::rpc::handler::ModuleRpcHandler;
use crate::rpc::errors::RpcError;

/// IPC-based RPC handler that forwards requests to modules
pub struct IpcRpcHandler {
    /// Module ID
    module_id: String,
    /// Method name
    method: String,
    /// Channel to send requests to module
    request_tx: mpsc::UnboundedSender<(u64, Value, mpsc::UnboundedSender<Result<Value, RpcError>>)>,
    /// Next correlation ID
    next_correlation_id: Arc<tokio::sync::Mutex<u64>>,
}

impl IpcRpcHandler {
    /// Create a new IPC-based RPC handler
    pub fn new(
        module_id: String,
        method: String,
        request_tx: mpsc::UnboundedSender<(u64, Value, mpsc::UnboundedSender<Result<Value, RpcError>>)>,
    ) -> Self {
        Self {
            module_id,
            method,
            request_tx,
            next_correlation_id: Arc::new(tokio::sync::Mutex::new(1)),
        }
    }
}

#[async_trait]
impl ModuleRpcHandler for IpcRpcHandler {
    async fn handle(&self, params: Value) -> Result<Value, RpcError> {
        // Get next correlation ID
        let correlation_id = {
            let mut id = self.next_correlation_id.lock().await;
            let current = *id;
            *id = current.wrapping_add(1);
            current
        };
        
        // Create response channel
        let (response_tx, mut response_rx) = mpsc::unbounded_channel();
        
        // Send request to module
        self.request_tx.send((correlation_id, params, response_tx))
            .map_err(|_| RpcError::internal_error("Failed to send RPC request to module".to_string()))?;
        
        // Wait for response with timeout
        tokio::select! {
            result = response_rx.recv() => {
                match result {
                    Some(Ok(value)) => Ok(value),
                    Some(Err(e)) => Err(e),
                    None => Err(RpcError::internal_error("Module disconnected".to_string())),
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(30)) => {
                Err(RpcError::internal_error("RPC request timeout".to_string()))
            }
        }
    }
}

