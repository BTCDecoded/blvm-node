//! Module Communication Hub
//!
//! Central API hub handling all module requests with routing,
//! permissions, and auditing.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

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
    /// Per-module rate limiters (token bucket algorithm, same as RPC)
    rate_limiters: Arc<Mutex<HashMap<String, ModuleRateLimiter>>>,
    /// Default rate limit burst (requests)
    default_rate_limit_burst: u32,
    /// Default rate limit rate (requests per second)
    default_rate_limit_rate: u32,
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

/// Token bucket rate limiter for module requests
/// Reuses the same algorithm as RpcRateLimiter for consistency
struct ModuleRateLimiter {
    /// Current number of tokens available
    tokens: u32,
    /// Maximum burst size (initial token count)
    burst_limit: u32,
    /// Tokens per second refill rate
    rate: u32,
    /// Last refill timestamp (Unix seconds)
    last_refill: u64,
}

impl ModuleRateLimiter {
    /// Create a new rate limiter
    fn new(burst_limit: u32, rate: u32) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("SystemTime should always be after UNIX_EPOCH")
            .as_secs();
        Self {
            tokens: burst_limit,
            burst_limit,
            rate,
            last_refill: now,
        }
    }

    /// Check if a request is allowed and consume a token
    fn check_and_consume(&mut self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("SystemTime should always be after UNIX_EPOCH")
            .as_secs();

        // Refill tokens based on elapsed time
        let elapsed = now.saturating_sub(self.last_refill);
        if elapsed > 0 {
            let tokens_to_add = (elapsed as u32).saturating_mul(self.rate);
            self.tokens = self
                .tokens
                .saturating_add(tokens_to_add)
                .min(self.burst_limit);
            self.last_refill = now;
        }

        // Check if we have tokens available
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }
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
            rate_limiters: Arc::new(Mutex::new(HashMap::new())),
            default_rate_limit_burst: 1000, // Higher than RPC (modules may make more calls)
            default_rate_limit_rate: 100,   // 100 req/sec per module
        }
    }

    /// Create a new rate limiter (reuses same algorithm as RpcRateLimiter)
    fn create_rate_limiter(&self) -> ModuleRateLimiter {
        ModuleRateLimiter::new(self.default_rate_limit_burst, self.default_rate_limit_rate)
    }

    /// Check if a module request is within rate limits
    /// Returns true if allowed, false if rate limited
    async fn check_module_rate_limit(&self, module_id: &str) -> bool {
        let mut limiters = self.rate_limiters.lock().await;
        
        // Get or create rate limiter for this module
        let limiter = limiters.entry(module_id.to_string()).or_insert_with(|| {
            self.create_rate_limiter()
        });

        limiter.check_and_consume()
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
            // Mempool API
            RequestPayload::GetMempoolTransactions => {
                let txs = self.node_api.get_mempool_transactions().await?;
                ResponsePayload::MempoolTransactions(txs)
            }
            RequestPayload::GetMempoolTransaction { tx_hash } => {
                let tx = self.node_api.get_mempool_transaction(tx_hash).await?;
                ResponsePayload::MempoolTransaction(tx)
            }
            RequestPayload::GetMempoolSize => {
                let size = self.node_api.get_mempool_size().await?;
                ResponsePayload::MempoolSize(size)
            }
            // Network API
            RequestPayload::GetNetworkStats => {
                let stats = self.node_api.get_network_stats().await?;
                ResponsePayload::NetworkStats(stats)
            }
            RequestPayload::GetNetworkPeers => {
                let peers = self.node_api.get_network_peers().await?;
                ResponsePayload::NetworkPeers(peers)
            }
            // Chain API
            RequestPayload::GetChainInfo => {
                let info = self.node_api.get_chain_info().await?;
                ResponsePayload::ChainInfo(info)
            }
            RequestPayload::GetBlockByHeight { height } => {
                let block = self.node_api.get_block_by_height(*height).await?;
                ResponsePayload::BlockByHeight(block)
            }
            // Lightning API
            RequestPayload::GetLightningNodeUrl => {
                let url = self.node_api.get_lightning_node_url().await?;
                ResponsePayload::LightningNodeUrl(url)
            }
            RequestPayload::GetLightningInfo => {
                let info = self.node_api.get_lightning_info().await?;
                ResponsePayload::LightningInfo(info)
            }
            // Payment API
            RequestPayload::GetPaymentState { payment_id } => {
                let state = self.node_api.get_payment_state(payment_id).await?;
                ResponsePayload::PaymentState(state)
            }
            // Additional Mempool API
            RequestPayload::CheckTransactionInMempool { tx_hash } => {
                let exists = self.node_api.check_transaction_in_mempool(tx_hash).await?;
                ResponsePayload::CheckTransactionInMempool(exists)
            }
            RequestPayload::GetFeeEstimate { target_blocks } => {
                let fee_rate = self.node_api.get_fee_estimate(*target_blocks).await?;
                ResponsePayload::FeeEstimate(fee_rate)
            }
            // Filesystem API
            RequestPayload::ReadFile { path } => {
                // Set module_id in thread-local for this call
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(module_id.to_string());
                let result = self.node_api.read_file(path.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                let data = result?;
                ResponsePayload::FileData(data)
            }
            RequestPayload::WriteFile { path, data } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(module_id.to_string());
                let result = self.node_api.write_file(path.clone(), data.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::DeleteFile { path } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(module_id.to_string());
                let result = self.node_api.delete_file(path.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::ListDirectory { path } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(module_id.to_string());
                let result = self.node_api.list_directory(path.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                let entries = result?;
                ResponsePayload::DirectoryListing(entries)
            }
            RequestPayload::CreateDirectory { path } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(module_id.to_string());
                let result = self.node_api.create_directory(path.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::GetFileMetadata { path } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(module_id.to_string());
                let result = self.node_api.get_file_metadata(path.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                let metadata = result?;
                ResponsePayload::FileMetadata(metadata)
            }
            // Storage API
            RequestPayload::StorageOpenTree { name } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(module_id.to_string());
                let result = self.node_api.storage_open_tree(name.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                let tree_id = result?;
                ResponsePayload::StorageTreeId(tree_id)
            }
            RequestPayload::StorageInsert { tree_id, key, value } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(module_id.to_string());
                let result = self.node_api.storage_insert(tree_id.clone(), key.clone(), value.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::StorageGet { tree_id, key } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(module_id.to_string());
                let result = self.node_api.storage_get(tree_id.clone(), key.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                let value = result?;
                ResponsePayload::StorageValue(value)
            }
            RequestPayload::StorageRemove { tree_id, key } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(module_id.to_string());
                let result = self.node_api.storage_remove(tree_id.clone(), key.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::StorageContainsKey { tree_id, key } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(module_id.to_string());
                let result = self.node_api.storage_contains_key(tree_id.clone(), key.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                let exists = result?;
                ResponsePayload::Bool(exists)
            }
            RequestPayload::StorageIter { tree_id } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(module_id.to_string());
                let result = self.node_api.storage_iter(tree_id.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                let pairs = result?;
                ResponsePayload::StorageKeyValuePairs(pairs)
            }
            RequestPayload::StorageTransaction { tree_id, operations } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(module_id.to_string());
                let result = self.node_api.storage_transaction(tree_id.clone(), operations.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::Bool(true)
            }
            // Module Discovery API
            RequestPayload::DiscoverModules => {
                let modules = self.node_api.discover_modules().await?;
                ResponsePayload::ModuleList(modules)
            }
            RequestPayload::GetModuleInfo { module_id: target_module_id } => {
                let info = self.node_api.get_module_info(target_module_id).await?;
                ResponsePayload::ModuleInfo(info)
            }
            RequestPayload::IsModuleAvailable { module_id: target_module_id } => {
                let available = self.node_api.is_module_available(target_module_id).await?;
                ResponsePayload::ModuleAvailable(available)
            }
            // Module Event Publishing
            RequestPayload::PublishEvent { event_type, payload } => {
                self.node_api.publish_event(*event_type, payload.clone()).await?;
                ResponsePayload::EventPublished
            }
            // Module-to-Module Communication
            RequestPayload::CallModule { target_module_id, method, params } => {
                let response = self.node_api.call_module(
                    target_module_id.as_deref(),
                    &method,
                    params.clone(),
                ).await?;
                ResponsePayload::ModuleApiResponse(response)
            }
            RequestPayload::RegisterModuleApi { .. } => {
                // Module API registration is handled differently - modules need to provide
                // the API implementation, which can't be serialized over IPC.
                // This will be handled via a different mechanism (module-side registration).
                // For now, return an error indicating this needs to be done differently.
                return Err(crate::module::traits::ModuleError::OperationError(
                    "Module API registration must be done via register_module_api() method, not IPC".to_string(),
                ));
            }
            RequestPayload::UnregisterModuleApi => {
                self.node_api.unregister_module_api().await?;
                ResponsePayload::ModuleApiUnregistered
            }
            // Module Health & Monitoring
            RequestPayload::GetModuleHealth { module_id } => {
                let health = self.node_api.get_module_health(&module_id).await?;
                ResponsePayload::ModuleHealth(health)
            }
            RequestPayload::GetAllModuleHealth => {
                let health = self.node_api.get_all_module_health().await?;
                ResponsePayload::AllModuleHealth(health)
            }
            RequestPayload::ReportModuleHealth { health } => {
                self.node_api.report_module_health(*health).await?;
                ResponsePayload::HealthReported
            }
            // Network Integration
            RequestPayload::SendMeshPacketToPeer { peer_addr, packet_data } => {
                self.node_api.send_mesh_packet_to_peer(peer_addr.clone(), packet_data.clone()).await?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::SendStratumV2MessageToPeer { peer_addr, message_data } => {
                self.node_api.send_stratum_v2_message_to_peer(peer_addr.clone(), message_data.clone()).await?;
                ResponsePayload::Bool(true)
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
            RequestPayload::GetMempoolTransactions => "get_mempool_transactions",
            RequestPayload::GetMempoolTransaction { .. } => "get_mempool_transaction",
            RequestPayload::GetMempoolSize => "get_mempool_size",
            RequestPayload::GetNetworkStats => "get_network_stats",
            RequestPayload::GetNetworkPeers => "get_network_peers",
            RequestPayload::GetChainInfo => "get_chain_info",
            RequestPayload::GetBlockByHeight { .. } => "get_block_by_height",
            RequestPayload::GetLightningNodeUrl => "get_lightning_node_url",
            RequestPayload::GetLightningInfo => "get_lightning_info",
            RequestPayload::GetPaymentState { .. } => "get_payment_state",
            RequestPayload::CheckTransactionInMempool { .. } => "check_transaction_in_mempool",
            RequestPayload::GetFeeEstimate { .. } => "get_fee_estimate",
            // Filesystem API
            RequestPayload::ReadFile { .. } => "read_file",
            RequestPayload::WriteFile { .. } => "write_file",
            RequestPayload::DeleteFile { .. } => "delete_file",
            RequestPayload::ListDirectory { .. } => "list_directory",
            RequestPayload::CreateDirectory { .. } => "create_directory",
            RequestPayload::GetFileMetadata { .. } => "get_file_metadata",
            // Storage API
            RequestPayload::StorageOpenTree { .. } => "storage_open_tree",
            RequestPayload::StorageInsert { .. } => "storage_insert",
            RequestPayload::StorageGet { .. } => "storage_get",
            RequestPayload::StorageRemove { .. } => "storage_remove",
            RequestPayload::StorageContainsKey { .. } => "storage_contains_key",
            RequestPayload::StorageIter { .. } => "storage_iter",
            RequestPayload::StorageTransaction { .. } => "storage_transaction",
            // Module RPC Endpoint Registration
            RequestPayload::RegisterRpcEndpoint { .. } => "register_rpc_endpoint",
            RequestPayload::UnregisterRpcEndpoint { .. } => "unregister_rpc_endpoint",
            // Timers and Scheduled Tasks
            RequestPayload::RegisterTimer { .. } => "register_timer",
            RequestPayload::CancelTimer { .. } => "cancel_timer",
            RequestPayload::ScheduleTask { .. } => "schedule_task",
            // Metrics and Telemetry
            RequestPayload::ReportMetric { .. } => "report_metric",
            RequestPayload::GetModuleMetrics { .. } => "get_module_metrics",
            RequestPayload::GetAllMetrics => "get_all_metrics",
            // Module Discovery API
            RequestPayload::DiscoverModules => "discover_modules",
            RequestPayload::GetModuleInfo { .. } => "get_module_info",
            RequestPayload::IsModuleAvailable { .. } => "is_module_available",
            // Module Event Publishing
            RequestPayload::PublishEvent { .. } => "publish_event",
            // Module-to-Module Communication
            RequestPayload::CallModule { .. } => "call_module",
            RequestPayload::RegisterModuleApi { .. } => "register_module_api",
            RequestPayload::UnregisterModuleApi => "unregister_module_api",
            // Module Health & Monitoring
            RequestPayload::GetModuleHealth { .. } => "get_module_health",
            RequestPayload::GetAllModuleHealth => "get_all_module_health",
            RequestPayload::ReportModuleHealth { .. } => "report_module_health",
            // Network Integration
            RequestPayload::SendMeshPacketToPeer { .. } => "send_mesh_packet_to_peer",
            RequestPayload::SendStratumV2MessageToPeer { .. } => "send_stratum_v2_message_to_peer",
        }
    }

    /// Log an audit entry
    fn log_audit(&mut self, module_id: String, api_call: String, success: bool) {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let entry = AuditEntry {
            module_id: module_id.clone(),
            api_call: api_call.clone(),
            timestamp,
            success,
        };

        // Store in-memory log (for programmatic access)
        self.audit_log.push_back(entry.clone());

        // Limit log size (keep last N entries)
        while self.audit_log.len() > self.max_audit_entries {
            self.audit_log.pop_front();
        }

        // Log to structured logging for monitoring
        use tracing::{info, warn};
        if success {
            info!(
                target: "bllvm_node::module::audit",
                module_id = %module_id,
                api_call = %api_call,
                timestamp = timestamp,
                "Module API call"
            );
        } else {
            warn!(
                target: "bllvm_node::module::audit",
                module_id = %module_id,
                api_call = %api_call,
                timestamp = timestamp,
                "Module API call failed"
            );
        }
    }

    /// Get audit log (for debugging/monitoring)
    pub fn get_audit_log(&self, limit: usize) -> Vec<AuditEntry> {
        let start = self.audit_log.len().saturating_sub(limit);
        self.audit_log.range(start..).cloned().collect()
    }
}
