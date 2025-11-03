//! Request validation for module API calls
//! 
//! Validates that modules cannot request consensus-modifying operations.

use tracing::debug;
use crate::module::traits::ModuleError;
use crate::module::ipc::protocol::RequestPayload;

/// Result of request validation
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationResult {
    /// Request is valid and allowed
    Allowed,
    /// Request is invalid and denied
    Denied(String),
}

/// Request validator that ensures modules cannot modify consensus
pub struct RequestValidator;

impl RequestValidator {
    /// Create a new request validator
    pub fn new() -> Self {
        Self
    }
    
    /// Validate a module request to ensure it doesn't modify consensus
    /// 
    /// All current RequestPayload variants are read-only, so validation always passes.
    /// When write operations are added, they will be rejected here.
    #[inline]
    pub fn validate_request(&self, _module_id: &str, payload: &RequestPayload) -> Result<ValidationResult, ModuleError> {
        // Fast path: all current operations are read-only
        // Using match ensures exhaustiveness when new variants are added
        match payload {
            // Read-only operations - all allowed
            RequestPayload::GetBlock { .. } 
            | RequestPayload::GetBlockHeader { .. }
            | RequestPayload::GetTransaction { .. }
            | RequestPayload::HasTransaction { .. }
            | RequestPayload::GetChainTip
            | RequestPayload::GetBlockHeight
            | RequestPayload::GetUtxo { .. }
            | RequestPayload::SubscribeEvents { .. } => Ok(ValidationResult::Allowed),
        }
    }
    
    /// Validate that a module cannot modify consensus state
    /// 
    /// This is a safeguard - modules should never have write access,
    /// but we validate explicitly to be defensive.
    pub fn validate_no_consensus_modification(&self, module_id: &str, operation: &str) -> Result<(), ModuleError> {
        // In Phase 2+, we would reject any operations that could modify:
        // - UTXO set
        // - Block validation rules
        // - Chain state
        // - Mempool state (unless explicitly allowed)
        
        // For now, all operations are read-only, so this always passes
        debug!("Validated no consensus modification for module {} operation: {}", module_id, operation);
        Ok(())
    }
    
    /// Validate resource limits (rate limiting, etc.)
    /// 
    /// Phase 1: No limits enforced (placeholder)
    /// Phase 2+: Implement rate limiting per module
    #[inline]
    pub fn validate_resource_limits(&self, _module_id: &str, _operation: &str) -> Result<(), ModuleError> {
        // TODO: Implement rate limiting per module
        // - Track requests per module in a time-windowed structure
        // - Enforce limits (e.g., max 100 requests/second per module)
        // - Use efficient data structures (circular buffer, sliding window)
        // - Return error if limit exceeded
        
        // For now, no limits enforced (Phase 2+)
        Ok(())
    }
}

impl Default for RequestValidator {
    fn default() -> Self {
        Self::new()
    }
}
