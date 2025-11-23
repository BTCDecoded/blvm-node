//! Permission model for module API access
//!
//! Implements whitelist-only access control for module API calls.

use std::collections::HashSet;
use tracing::{debug, warn};

use crate::module::ipc::protocol::RequestPayload;
use crate::module::traits::ModuleError;

/// Helper function to convert permission string to Permission enum
pub fn parse_permission_string(perm_str: &str) -> Option<Permission> {
    match perm_str {
        "read_blockchain" | "ReadBlockchain" => Some(Permission::ReadBlockchain),
        "read_utxo" | "ReadUTXO" => Some(Permission::ReadUTXO),
        "read_chain_state" | "ReadChainState" => Some(Permission::ReadChainState),
        "subscribe_events" | "SubscribeEvents" => Some(Permission::SubscribeEvents),
        "send_transactions" | "SendTransactions" => Some(Permission::SendTransactions),
        _ => None,
    }
}

/// Permission types that modules can request
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Read blockchain data (blocks, headers, transactions)
    ReadBlockchain,
    /// Query UTXO set (read-only)
    ReadUTXO,
    /// Subscribe to node events
    SubscribeEvents,
    /// Send transactions to mempool (future: may be restricted)
    SendTransactions,
    /// Query chain state (height, tip, etc.)
    ReadChainState,
}

/// Set of permissions for a module
#[derive(Debug, Clone, Default)]
pub struct PermissionSet {
    permissions: HashSet<Permission>,
}

impl PermissionSet {
    /// Create a new empty permission set
    pub fn new() -> Self {
        Self {
            permissions: HashSet::new(),
        }
    }

    /// Create a permission set from a vector
    pub fn from_vec(permissions: Vec<Permission>) -> Self {
        Self {
            permissions: permissions.into_iter().collect(),
        }
    }

    /// Add a permission
    pub fn add(&mut self, permission: Permission) {
        self.permissions.insert(permission);
    }

    /// Check if a permission is granted
    pub fn has(&self, permission: &Permission) -> bool {
        self.permissions.contains(permission)
    }

    /// Check if all required permissions are granted
    pub fn has_all(&self, required: &[Permission]) -> bool {
        required.iter().all(|p| self.permissions.contains(p))
    }

    /// Get all permissions as a vector
    pub fn to_vec(&self) -> Vec<Permission> {
        self.permissions.iter().cloned().collect()
    }
}

/// Permission checker for validating module API access
pub struct PermissionChecker {
    /// Default permissions granted to all modules
    default_permissions: PermissionSet,
    /// Module-specific permission overrides (module_id -> permissions)
    module_permissions: std::collections::HashMap<String, PermissionSet>,
    /// Cached mapping from RequestPayload type to required Permission (avoid repeated matching)
    #[allow(dead_code)]
    payload_to_permission_cache: std::collections::HashMap<std::any::TypeId, Permission>,
}

impl PermissionChecker {
    /// Create a new permission checker with default permissions
    pub fn new() -> Self {
        // Default permissions for modules (conservative - read-only by default)
        let mut default = PermissionSet::new();
        default.add(Permission::ReadBlockchain);
        default.add(Permission::ReadUTXO);
        default.add(Permission::ReadChainState);
        default.add(Permission::SubscribeEvents);

        Self {
            default_permissions: default,
            module_permissions: std::collections::HashMap::new(),
            payload_to_permission_cache: std::collections::HashMap::new(),
        }
    }

    /// Register module-specific permissions
    pub fn register_module_permissions(&mut self, module_id: String, permissions: PermissionSet) {
        debug!(
            "Registering permissions for module {}: {:?}",
            module_id,
            permissions.to_vec()
        );
        self.module_permissions.insert(module_id, permissions);
    }

    /// Check if a module has a specific permission
    #[inline]
    pub fn check_permission(&self, module_id: &str, permission: &Permission) -> bool {
        // Check module-specific permissions first
        if let Some(module_perms) = self.module_permissions.get(module_id) {
            if module_perms.has(permission) {
                return true;
            }
            // If module has custom permissions, only those apply (no defaults)
            return false;
        }

        // Fall back to default permissions
        self.default_permissions.has(permission)
    }

    /// Get effective permissions for a module
    pub fn get_permissions(&self, module_id: &str) -> PermissionSet {
        if let Some(module_perms) = self.module_permissions.get(module_id) {
            module_perms.clone()
        } else {
            self.default_permissions.clone()
        }
    }

    /// Check if a module can perform a specific API operation
    pub fn check_api_call(
        &self,
        module_id: &str,
        payload: &RequestPayload,
    ) -> Result<(), ModuleError> {
        let required_permission = match payload {
            RequestPayload::Handshake { .. } => {
                // Handshake doesn't require permissions - handled at connection level
                return Ok(());
            }
            RequestPayload::GetBlock { .. } => Permission::ReadBlockchain,
            RequestPayload::GetBlockHeader { .. } => Permission::ReadBlockchain,
            RequestPayload::GetTransaction { .. } => Permission::ReadBlockchain,
            RequestPayload::HasTransaction { .. } => Permission::ReadBlockchain,
            RequestPayload::GetChainTip => Permission::ReadChainState,
            RequestPayload::GetBlockHeight => Permission::ReadChainState,
            RequestPayload::GetUtxo { .. } => Permission::ReadUTXO,
            RequestPayload::SubscribeEvents { .. } => Permission::SubscribeEvents,
        };

        if !self.check_permission(module_id, &required_permission) {
            warn!(
                "Module {} denied access to {:?} (missing permission: {:?})",
                module_id, payload, required_permission
            );
            return Err(ModuleError::OperationError(format!(
                "Permission denied: module {module_id} does not have permission {required_permission:?}"
            )));
        }

        debug!("Module {} granted access to {:?}", module_id, payload);
        Ok(())
    }
}

impl Default for PermissionChecker {
    fn default() -> Self {
        Self::new()
    }
}
