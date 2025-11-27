//! Module Communication Hub
//!
//! Central API hub handling all module requests with routing,
//! permissions, and auditing.

use std::collections::VecDeque;
use std::sync::Arc;
use tracing::{debug, info};

use crate::module::ipc::protocol::{
    RequestMessage, RequestPayload, ResponseMessage, ResponsePayload,
};
use crate::module::security::{PermissionChecker, RequestValidator};
use crate::module::traits::{ModuleError, NodeAPI};

/// API request router that routes module requests to appropriate handlers
pub struct ModuleApiHub {
    /// Node API implementation
    node_api: Arc<dyn NodeAPI + Send + Sync>,
    /// Permission checker for validating module access
    permission_checker: PermissionChecker,
    /// Request validator for consensus protection
    request_validator: RequestValidator,
    /// Request audit log (for security tracking) - bounded to last 1000 entries
    #[allow(dead_code)]
    audit_log: VecDeque<AuditEntry>,
    /// Maximum audit log size
    #[allow(dead_code)]
    max_audit_entries: usize,
}

/// Audit entry for tracking module API usage
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields are used by external consumers of the API
pub struct AuditEntry {
    pub module_id: String,
    pub api_call: String,
    pub timestamp: u64,
    pub success: bool,
}

impl ModuleApiHub {
    /// Create a new API hub
    pub fn new<A: NodeAPI + Send + Sync + 'static>(node_api: Arc<A>) -> Self {
        Self {
            node_api,
            permission_checker: PermissionChecker::new(),
            request_validator: RequestValidator::new(),
            audit_log: VecDeque::new(),
            max_audit_entries: 1000,
        }
    }

    /// Register a module's permissions
    pub fn register_module_permissions(
        &mut self,
        module_id: String,
        permissions: crate::module::security::permissions::PermissionSet,
    ) {
        self.permission_checker
            .register_module_permissions(module_id, permissions);
    }

    /// Handle a request from a module
    pub async fn handle_request(
        &mut self,
        module_id: &str,
        request: RequestMessage,
    ) -> Result<ResponseMessage, ModuleError> {
        debug!(
            "API hub handling request from module {}: {:?}",
            module_id, request.payload
        );

        // Validate permissions
        self.permission_checker
            .check_api_call(module_id, &request.payload)?;

        // Validate that request doesn't modify consensus
        match self
            .request_validator
            .validate_request(module_id, &request.payload)?
        {
            crate::module::security::ValidationResult::Allowed => {}
            crate::module::security::ValidationResult::Denied(reason) => {
                return Err(ModuleError::OperationError(format!(
                    "Request denied: {reason}"
                )));
            }
        }

        // Handle handshake specially (no validation needed, just acknowledge)
        if let RequestPayload::Handshake {
            module_id: handshake_id,
            module_name,
            version,
        } = &request.payload
        {
            // Verify module_id matches
            if handshake_id != module_id {
                return Err(ModuleError::OperationError(format!(
                    "Handshake module_id mismatch: expected {module_id}, got {handshake_id}"
                )));
            }

            info!(
                "Handshake from module {} (name={}, version={})",
                module_id, module_name, version
            );
            return Ok(ResponseMessage::success(
                request.correlation_id,
                ResponsePayload::HandshakeAck {
                    node_version: env!("CARGO_PKG_VERSION").to_string(),
                },
            ));
        }

        // Get operation ID for resource limits and audit logging (avoid duplicate matching)
        let operation_id = Self::get_operation_id(&request.payload);
        self.request_validator
            .validate_resource_limits(module_id, operation_id)?;

        // Route request to appropriate handler
        let response = match &request.payload {
            RequestPayload::Handshake { .. } => {
                // Handshake is handled at connection level, not here
                return Err(crate::module::traits::ModuleError::IpcError(
                    "Handshake should be handled at connection level".to_string(),
                ));
            }
            RequestPayload::GetBlock { hash } => {
                let block = self.node_api.get_block(hash).await?;
                ResponsePayload::Block(block)
            }
            RequestPayload::GetBlockHeader { hash } => {
                let header = self.node_api.get_block_header(hash).await?;
                ResponsePayload::BlockHeader(header)
            }
            RequestPayload::GetTransaction { hash } => {
                let tx = self.node_api.get_transaction(hash).await?;
                ResponsePayload::Transaction(tx)
            }
            RequestPayload::HasTransaction { hash } => {
                let exists = self.node_api.has_transaction(hash).await?;
                ResponsePayload::Bool(exists)
            }
            RequestPayload::GetChainTip => {
                let tip = self.node_api.get_chain_tip().await?;
                ResponsePayload::Hash(tip)
            }
            RequestPayload::GetBlockHeight => {
                let height = self.node_api.get_block_height().await?;
                ResponsePayload::U64(height)
            }
            RequestPayload::GetUtxo { outpoint } => {
                let utxo = self.node_api.get_utxo(outpoint).await?;
                ResponsePayload::Utxo(utxo)
            }
            RequestPayload::SubscribeEvents { event_types: _ } => {
                // Event subscription is handled in IPC server
                // Return success acknowledgment (Empty response)
                ResponsePayload::SubscribeAck
            }
        };

        // Log audit entry (use operation ID from earlier)
        self.log_audit(module_id.to_string(), operation_id.to_string(), true);

        Ok(ResponseMessage::success(request.correlation_id, response))
    }

    /// Get operation identifier from request payload (for logging/rate limiting)
    #[inline]
    fn get_operation_id(payload: &RequestPayload) -> &'static str {
        match payload {
            RequestPayload::Handshake { .. } => "handshake",
            RequestPayload::GetBlock { .. } => "get_block",
            RequestPayload::GetBlockHeader { .. } => "get_block_header",
            RequestPayload::GetTransaction { .. } => "get_transaction",
            RequestPayload::HasTransaction { .. } => "has_transaction",
            RequestPayload::GetChainTip => "get_chain_tip",
            RequestPayload::GetBlockHeight => "get_block_height",
            RequestPayload::GetUtxo { .. } => "get_utxo",
            RequestPayload::SubscribeEvents { .. } => "subscribe_events",
        }
    }

    /// Log an audit entry
    fn log_audit(&mut self, module_id: String, api_call: String, success: bool) {
        // For now, keep a simple in-memory log
        // In production, this would be persisted
        let entry = AuditEntry {
            module_id,
            api_call,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            success,
        };

        self.audit_log.push_back(entry);

        // Limit log size (keep last N entries)
        while self.audit_log.len() > self.max_audit_entries {
            self.audit_log.pop_front();
        }
    }

    /// Get audit log (for debugging/monitoring)
    pub fn get_audit_log(&self, limit: usize) -> Vec<AuditEntry> {
        let start = self.audit_log.len().saturating_sub(limit);
        self.audit_log.range(start..).cloned().collect()
    }
}
