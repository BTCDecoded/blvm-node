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
        "read_mempool" | "ReadMempool" => Some(Permission::ReadMempool),
        "read_network" | "ReadNetwork" => Some(Permission::ReadNetwork),
        "network_access" | "NetworkAccess" => Some(Permission::NetworkAccess),
        "read_lightning" | "ReadLightning" => Some(Permission::ReadLightning),
        "read_payment" | "ReadPayment" => Some(Permission::ReadPayment),
        "read_storage" | "ReadStorage" => Some(Permission::ReadStorage),
        "write_storage" | "WriteStorage" => Some(Permission::WriteStorage),
        "manage_storage" | "ManageStorage" => Some(Permission::ManageStorage),
        "read_filesystem" | "ReadFilesystem" => Some(Permission::ReadFilesystem),
        "write_filesystem" | "WriteFilesystem" => Some(Permission::WriteFilesystem),
        "manage_filesystem" | "ManageFilesystem" => Some(Permission::ManageFilesystem),
        "register_rpc_endpoint" | "RegisterRpcEndpoint" => Some(Permission::RegisterRpcEndpoint),
        "manage_timers" | "ManageTimers" => Some(Permission::ManageTimers),
        "report_metrics" | "ReportMetrics" => Some(Permission::ReportMetrics),
        "read_metrics" | "ReadMetrics" => Some(Permission::ReadMetrics),
        "discover_modules" | "DiscoverModules" => Some(Permission::DiscoverModules),
        "publish_events" | "PublishEvents" => Some(Permission::PublishEvents),
        "call_module" | "CallModule" => Some(Permission::CallModule),
        "register_module_api" | "RegisterModuleApi" => Some(Permission::RegisterModuleApi),
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
    /// Read mempool data (transactions, size, fee estimates)
    ReadMempool,
    /// Read network data (peers, stats)
    ReadNetwork,
    /// Send network packets (mesh packets, etc.)
    NetworkAccess,
    /// Read Lightning network data
    ReadLightning,
    /// Read payment data
    ReadPayment,
    // Storage permissions
    /// Read from module storage
    ReadStorage,
    /// Write to module storage
    WriteStorage,
    /// Manage storage (create/delete trees, manage quotas)
    ManageStorage,
    // Filesystem permissions
    /// Read files from module data directory
    ReadFilesystem,
    /// Write files to module data directory
    WriteFilesystem,
    /// Manage filesystem (create/delete directories, manage quotas)
    ManageFilesystem,
    // RPC permissions
    /// Register RPC endpoints
    RegisterRpcEndpoint,
    // Timer permissions
    /// Manage timers and scheduled tasks
    ManageTimers,
    // Metrics permissions
    /// Report metrics
    ReportMetrics,
    /// Read metrics
    ReadMetrics,
    // Module discovery permissions
    /// Discover other modules
    DiscoverModules,
    // Event publishing permissions
    /// Publish events to other modules
    PublishEvents,
    // Module-to-module communication permissions
    /// Call other modules' APIs
    CallModule,
    /// Register module API for other modules to call
    RegisterModuleApi,
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
            // Mempool API
            RequestPayload::GetMempoolTransactions => Permission::ReadMempool,
            RequestPayload::GetMempoolTransaction { .. } => Permission::ReadMempool,
            RequestPayload::GetMempoolSize => Permission::ReadMempool,
            // Network API
            RequestPayload::GetNetworkStats => Permission::ReadNetwork,
            RequestPayload::GetNetworkPeers => Permission::ReadNetwork,
            // Chain API
            RequestPayload::GetChainInfo => Permission::ReadChainState,
            RequestPayload::GetBlockByHeight { .. } => Permission::ReadBlockchain,
            // Lightning API
            RequestPayload::GetLightningNodeUrl => Permission::ReadLightning,
            RequestPayload::GetLightningInfo => Permission::ReadLightning,
            // Payment API
            RequestPayload::GetPaymentState { .. } => Permission::ReadPayment,
            // Additional Mempool API
            RequestPayload::CheckTransactionInMempool { .. } => Permission::ReadMempool,
            RequestPayload::GetFeeEstimate { .. } => Permission::ReadMempool,
            // Filesystem API
            RequestPayload::ReadFile { .. } => Permission::ReadFilesystem,
            RequestPayload::WriteFile { .. } => Permission::WriteFilesystem,
            RequestPayload::DeleteFile { .. } => Permission::ManageFilesystem,
            RequestPayload::ListDirectory { .. } => Permission::ReadFilesystem,
            RequestPayload::CreateDirectory { .. } => Permission::ManageFilesystem,
            RequestPayload::GetFileMetadata { .. } => Permission::ReadFilesystem,
            // Storage API
            RequestPayload::StorageOpenTree { .. } => Permission::ManageStorage,
            RequestPayload::StorageInsert { .. } => Permission::WriteStorage,
            RequestPayload::StorageGet { .. } => Permission::ReadStorage,
            RequestPayload::StorageRemove { .. } => Permission::WriteStorage,
            RequestPayload::StorageContainsKey { .. } => Permission::ReadStorage,
            RequestPayload::StorageIter { .. } => Permission::ReadStorage,
            RequestPayload::StorageTransaction { .. } => Permission::WriteStorage,
            // Module RPC Endpoint Registration
            RequestPayload::RegisterRpcEndpoint { .. } => Permission::RegisterRpcEndpoint,
            RequestPayload::UnregisterRpcEndpoint { .. } => Permission::RegisterRpcEndpoint,
            // Timers and Scheduled Tasks
            RequestPayload::RegisterTimer { .. } => Permission::ManageTimers,
            RequestPayload::CancelTimer { .. } => Permission::ManageTimers,
            RequestPayload::ScheduleTask { .. } => Permission::ManageTimers,
            // Metrics and Telemetry
            RequestPayload::ReportMetric { .. } => Permission::ReportMetrics,
            RequestPayload::GetModuleMetrics { .. } => Permission::ReadMetrics,
            RequestPayload::GetAllMetrics => Permission::ReadMetrics,
            // Module Health & Monitoring
            RequestPayload::GetModuleHealth { .. } => Permission::ReadMetrics,
            RequestPayload::GetAllModuleHealth => Permission::ReadMetrics,
            RequestPayload::ReportModuleHealth { .. } => Permission::ReportMetrics,
            // Network Integration
            RequestPayload::SendMeshPacketToPeer { .. } => Permission::NetworkAccess,
            RequestPayload::SendStratumV2MessageToPeer { .. } => Permission::NetworkAccess,
            // Module Discovery API
            RequestPayload::DiscoverModules => Permission::DiscoverModules,
            RequestPayload::GetModuleInfo { .. } => Permission::DiscoverModules,
            RequestPayload::IsModuleAvailable { .. } => Permission::DiscoverModules,
            // Module Event Publishing
            RequestPayload::PublishEvent { .. } => Permission::PublishEvents,
            // Module-to-Module Communication
            RequestPayload::CallModule { .. } => Permission::CallModule,
            RequestPayload::RegisterModuleApi { .. } => Permission::RegisterModuleApi,
            RequestPayload::UnregisterModuleApi => Permission::RegisterModuleApi,
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
